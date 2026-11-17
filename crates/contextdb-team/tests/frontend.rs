//! T109 (ADR 0018 §4.1, §6.1; SPEC §6.1 P10 check order): routing a write.
//! The kernel decides first; a write it refuses as a team write is
//! translated to portable ops (keys, contents and expected values only),
//! which the replica's scratch `accept` then takes. Mixed writes, team rules
//! relying on local records, local reads of unpublished records, two teams
//! in one batch and reserved names are refused; a name hidden by P9 keeps
//! the kernel's `Unauthorized(Read)`.

use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_team::catchup::{self, History};
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::frontend::{self, Route};
use contextdb_team::gitobj;
use contextdb_team::ops::{Edge, Hash, RuleSpec, TeamBatch, TeamOp, tags_hash};
use contextdb_team::replica::Replica as Overlay;
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use std::collections::{BTreeMap, BTreeSet};

const TEAM: [u8; 16] = [0x09; 16];
const OTHER: [u8; 16] = [0x0a; 16];
const ALICE: [u8; 32] = [1; 32];

fn content(name: &str) -> (Hash, (String, Vec<u8>)) {
    let b = name.as_bytes().to_vec();
    let h = *blake3::hash(&b).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), b))
}

struct Team {
    replica: Replica,
    history: History,
    seq: u64,
    parent: Option<String>,
}

impl Team {
    fn new() -> Self {
        let mut t = Self {
            replica: Replica::new(TEAM, ObjectFormat::Sha1),
            history: History::default(),
            seq: 0,
            parent: None,
        };
        let doc = TeamDoc::from_toml(&format!(
            "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
             [members.alice]\nkeys = [\"{}\"]\n[lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n",
            hex(&TEAM),
            sshsig::public_key(&ALICE).to_line()
        ))
        .unwrap();
        t.entry(vec![TeamOp::SetTeamAccess { doc: Box::new(doc) }], vec![]);
        t
    }
    fn entry(&mut self, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>) {
        let batch = TeamBatch {
            proposals: vec![ops],
        }
        .encode();
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.seq.to_be_bytes());
        let tr = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", "alice".to_owned()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".to_owned()),
            ("Ctx-Proposal", hex(&id)),
        ];
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let tree_hex = hex(&gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&tree_hex, &parents, "e", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&ALICE, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid = hex(&gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
        let out = self.replica.accept(&EntryInput {
            oid: &oid,
            seq: self.seq,
            first_in_generation: self.parent.is_none(),
            commit: &raw,
            tree: &tree,
        });
        assert!(
            matches!(&out, Outcome::Decided(d) if d.iter().all(Result::is_ok)),
            "{out:?}"
        );
        self.history
            .record(&oid, catchup::projection(&self.replica));
        self.parent = Some(oid);
        self.seq += 1;
    }
    /// Whether the replica's scratch `accept` takes `ops` from alice.
    fn takes(&self, ops: &[TeamOp], blobs: &[(Hash, Vec<u8>)]) -> Result<(), String> {
        let mut r = self.replica.clone();
        let blobs: BTreeMap<Hash, Vec<u8>> = blobs.iter().cloned().collect();
        r.scratch_apply("alice", [0x77; 16], ops, &blobs)
            .map_err(|e| format!("{}: {}", e.code, e.detail))
    }
}

fn key(path: &str) -> String {
    format!("team/{}/{path}", hex(&TEAM))
}
fn lane() -> String {
    format!("team:{}/main", hex(&TEAM))
}

struct Edge0 {
    team: Team,
    k: Kernel,
    a1: Hash,
    b1: Hash,
}

impl Edge0 {
    fn new() -> Self {
        let mut team = Team::new();
        let (a1, ba) = content("a1");
        let (b1, bb) = content("b1");
        team.entry(
            vec![
                TeamOp::New {
                    path: "k/a".into(),
                    lane: "main".into(),
                    kind: RecordKind::Decision,
                    content: a1,
                    adopt_as_base: true,
                },
                TeamOp::New {
                    path: "k/b".into(),
                    lane: "main".into(),
                    kind: RecordKind::Decision,
                    content: b1,
                    adopt_as_base: false,
                },
            ],
            vec![ba, bb],
        );
        let mut k = Kernel::default();
        catchup::catch_up(&mut k, MAIN, TEAM, &team.history).unwrap();
        let mut e = Self { team, k, a1, b1 };
        e.commit_local(vec![local_create("dec:l", 1, true)]);
        e
    }
    fn commit_local(&mut self, ops: Vec<Op>) {
        let mut b = Batch::new(ops);
        b.channel = Channel::Agent;
        self.k.commit(&b).unwrap();
    }
    fn logical(&self, key: &str) -> LogicalId {
        self.k.branch(MAIN).unwrap().logical_by_key(key).unwrap()
    }
    fn head(&self, key: &str) -> RevisionId {
        let v = self.k.branch(MAIN).unwrap();
        v.head(self.logical(key)).unwrap()
    }
    /// The daemon's order: the kernel, then the router.
    fn route_as(
        &mut self,
        p: PrincipalId,
        ops: Vec<Op>,
        overlays: &BTreeMap<TeamId, Overlay>,
    ) -> Result<Route, TxError> {
        let mut b = Batch::new(ops);
        b.channel = Channel::Agent;
        b.principal = p;
        match self.k.what_if(&b) {
            Ok(_) => Ok(Route::Local),
            Err(e) => match frontend::translate(&self.k, &b, e.clone(), overlays)? {
                Route::Local => Err(e),
                team => Ok(team),
            },
        }
    }
    fn overlays(&self) -> BTreeMap<TeamId, Overlay> {
        [(TEAM, self.team.replica.clone())].into()
    }
    fn route(&mut self, ops: Vec<Op>) -> Result<Route, TxError> {
        let o = self.overlays();
        self.route_as(PrincipalId::STORE_OWNER, ops, &o)
    }
}

fn local_create(key: &str, n: u8, base: bool) -> Op {
    Op::CreateLogical {
        origin: Origin::Local,
        key: key.into(),
        kind: RecordKind::Decision,
        scope_lane: "repo".into(),
        content: [n; 32],
        adopt_as_base: base,
    }
}

fn team_ops(r: Result<Route, TxError>) -> Vec<TeamOp> {
    match r {
        Ok(Route::Team { team, ops }) => {
            assert_eq!(team, TEAM);
            ops
        }
        other => panic!("not a team proposal: {other:?}"),
    }
}

#[test]
fn t109_a_local_batch_stays_local_even_when_it_relies_on_mirrors() {
    let mut e = Edge0::new();
    let ma = e.head(&key("k/a"));
    let r = e.route(vec![
        local_create("dec:on-a", 2, false),
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![(ma.into(), EdgeKind::Pin), (ma.into(), EdgeKind::Sem)],
            enabled: true,
        },
        Op::ExpectHeads {
            entries: vec![(LogicalRef::Key(key("k/a")), ma)],
            source: ReadSource::Client,
        },
    ]);
    assert_eq!(r, Ok(Route::Local));
}

#[test]
fn t109_a_supersede_of_a_mirror_becomes_portable_ops_accept_takes() {
    let mut e = Edge0::new();
    let ma = e.head(&key("k/a"));
    let (a2, blob) = content("a2");
    let ops = team_ops(e.route(vec![
        Op::AddRevision {
            logical: LogicalRef::Existing(e.logical(&key("k/a"))),
            content: a2,
            adopt_as_base: true,
        },
        Op::SetHead {
            logical: LogicalRef::Key(key("k/a")),
            expected: RevRef::Existing(ma),
            new: RevRef::New(0),
        },
        Op::ExpectHeads {
            entries: vec![(LogicalRef::Key(key("k/b")), e.head(&key("k/b")))],
            source: ReadSource::Client,
        },
    ]));
    assert_eq!(
        ops,
        vec![
            TeamOp::Expect {
                path: "k/b".into(),
                content: e.b1
            },
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: e.a1,
                new: a2
            },
        ]
    );
    // Portable: the encoding round-trips, and names no local id.
    let enc = TeamBatch {
        proposals: vec![ops.clone()],
    }
    .encode();
    assert_eq!(
        TeamBatch::decode(&enc).unwrap().proposals,
        vec![ops.clone()]
    );
    e.team.takes(&ops, &[(a2, blob.1)]).unwrap();
}

#[test]
fn t109_new_team_records_rules_bases_tags_and_rebind_translate() {
    let mut e = Edge0::new();
    let (ma, mb) = (e.head(&key("k/a")), e.head(&key("k/b")));
    let (c, blob) = content("c1");
    let ops = team_ops(e.route(vec![
        Op::CreateLogical {
            origin: Origin::Local,
            key: key("k/c"),
            kind: RecordKind::Plan,
            scope_lane: lane(),
            content: c,
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![
                (mb.into(), EdgeKind::Cur),
                (ma.into(), EdgeKind::Pin),
                (ma.into(), EdgeKind::Ref),
            ],
            enabled: true,
        },
        Op::RetractBase {
            rev: RevRef::Existing(ma),
        },
        Op::AssertBase {
            rev: RevRef::Existing(mb),
        },
        Op::SetMetadata {
            logical: LogicalRef::Key(key("k/b")),
            tags: ["t1".to_owned()].into(),
            dirs: BTreeSet::new(),
        },
    ]));
    let rule = RuleSpec {
        conclusion: ("k/c".into(), c),
        incidences: vec![
            ("k/a".into(), Edge::Pin(e.a1)),
            ("k/a".into(), Edge::Ref(e.a1)),
            ("k/b".into(), Edge::Cur(e.b1)),
        ],
    };
    assert_eq!(
        ops,
        vec![
            TeamOp::New {
                path: "k/c".into(),
                lane: "main".into(),
                kind: RecordKind::Plan,
                content: c,
                adopt_as_base: false
            },
            TeamOp::AddRule {
                rule: rule.clone(),
                enabled: true
            },
            TeamOp::SetBase {
                path: "k/a".into(),
                content: e.a1,
                expected: true,
                base: false
            },
            TeamOp::SetBase {
                path: "k/b".into(),
                content: e.b1,
                expected: false,
                base: true
            },
            TeamOp::SetTags {
                path: "k/b".into(),
                expected: tags_hash(&BTreeSet::new()),
                tags: ["t1".to_owned()].into()
            },
        ]
    );
    e.team.takes(&ops, &[(c, blob.1.clone())]).unwrap();
    // A rule added and switched in one batch; an existing rule is local
    // (mirrors carry no rules, so a local rule never concludes a mirror).
    let (d, _) = content("d1");
    let ops = team_ops(e.route(vec![
        Op::CreateLogical {
            origin: Origin::Local,
            key: key("k/d"),
            kind: RecordKind::Decision,
            scope_lane: lane(),
            content: d,
            adopt_as_base: false,
        },
        Op::AddRule {
            conclusion: RevRef::New(0),
            incidences: vec![(ma.into(), EdgeKind::Pin)],
            enabled: true,
        },
        Op::SetRuleEnabled {
            rule: RuleRef::New(0),
            enabled: false,
        },
        Op::Rebind {
            rule: RuleRef::New(0),
            from: RevRef::Existing(ma),
            to: RevRef::Existing(mb),
        },
    ]));
    let pin = |h: Hash, p: &str| RuleSpec {
        conclusion: ("k/d".into(), d),
        incidences: vec![(p.into(), Edge::Pin(h))],
    };
    assert_eq!(
        &ops[1..],
        &[
            TeamOp::AddRule {
                rule: pin(e.a1, "k/a"),
                enabled: true
            },
            TeamOp::SetRuleEnabled {
                rule: pin(e.a1, "k/a"),
                expected: true,
                enabled: false
            },
            TeamOp::SetRuleEnabled {
                rule: pin(e.a1, "k/a"),
                expected: false,
                enabled: false
            },
            TeamOp::AddRule {
                rule: pin(e.b1, "k/b"),
                enabled: true
            },
        ]
    );
}

#[test]
fn t109_expected_base_bits_are_the_teams_not_the_mirrors() {
    let mut e = Edge0::new();
    // a2 becomes the head: a1 keeps its team base bit, but the mirror's
    // base is on the head only (it shows the status, §5.1).
    let (a2, blob) = content("a2");
    e.team.entry(
        vec![
            TeamOp::Revise {
                path: "k/a".into(),
                content: a2,
                adopt_as_base: true,
            },
            TeamOp::SetHead {
                path: "k/a".into(),
                expected: e.a1,
                new: a2,
            },
        ],
        vec![blob],
    );
    catchup::catch_up(&mut e.k, MAIN, TEAM, &e.team.history).unwrap();
    let v = e.k.branch(MAIN).unwrap();
    let ma1 = v.revisions_with_content(&e.a1)[0];
    assert!(!v.is_base(ma1).unwrap());
    let ops = team_ops(e.route(vec![Op::RetractBase {
        rev: RevRef::Existing(ma1),
    }]));
    assert_eq!(
        ops,
        vec![TeamOp::SetBase {
            path: "k/a".into(),
            content: e.a1,
            expected: true,
            base: false
        }]
    );
    e.team.takes(&ops, &[]).unwrap();
}

#[test]
fn t109_mixed_writes_two_teams_and_outside_dependencies_are_refused() {
    let mut e = Edge0::new();
    let ma = e.head(&key("k/a"));
    let local = e.head("dec:l");
    // Team and local write targets.
    assert_eq!(
        e.route(vec![
            local_create("dec:x", 3, false),
            Op::RetractBase {
                rev: RevRef::Existing(ma)
            },
        ]),
        Err(TxError::MixedTeamBatch)
    );
    // Two teams.
    let other = |path: &str| Op::CreateLogical {
        origin: Origin::Local,
        key: format!("team/{}/{path}", hex(&OTHER)),
        kind: RecordKind::Decision,
        scope_lane: format!("team:{}/main", hex(&OTHER)),
        content: [4; 32],
        adopt_as_base: false,
    };
    assert_eq!(
        e.route(vec![
            Op::RetractBase {
                rev: RevRef::Existing(ma)
            },
            other("x"),
        ]),
        Err(TxError::MixedTeamBatch)
    );
    // A team rule with a local premise; a team read of a local record.
    let (c, _) = content("c");
    let new_c = Op::CreateLogical {
        origin: Origin::Local,
        key: key("k/c"),
        kind: RecordKind::Decision,
        scope_lane: lane(),
        content: c,
        adopt_as_base: false,
    };
    assert_eq!(
        e.route(vec![
            new_c.clone(),
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(local.into(), EdgeKind::Pin)],
                enabled: true,
            },
        ]),
        Err(TxError::TeamDepOutside)
    );
    assert_eq!(
        e.route(vec![
            new_c.clone(),
            Op::ExpectHeads {
                entries: vec![(LogicalRef::Key("dec:l".into()), local)],
                source: ReadSource::Client,
            },
        ]),
        Err(TxError::TeamDepOutside)
    );
    // A team key in a local lane is a direct write to a reserved name.
    assert_eq!(
        e.route(vec![Op::CreateLogical {
            origin: Origin::Local,
            key: key("k/d"),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: c,
            adopt_as_base: false,
        }]),
        Err(TxError::ReservedNamespace)
    );
    // Not portable: a Sem premise in a team rule, local dirs on a mirror.
    assert_eq!(
        e.route(vec![
            new_c,
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(ma.into(), EdgeKind::Sem)],
                enabled: true,
            },
        ]),
        Err(TxError::ReservedNamespace)
    );
    assert_eq!(
        e.route(vec![Op::SetMetadata {
            logical: LogicalRef::Key(key("k/a")),
            tags: BTreeSet::new(),
            dirs: ["src".to_owned()].into(),
        }]),
        Err(TxError::ReplicaReadOnly)
    );
}

#[test]
fn t109_a_local_read_of_a_record_only_queued_is_unpublished() {
    let mut e = Edge0::new();
    let ops = || {
        vec![
            local_create("dec:y", 5, false),
            Op::ExpectHeads {
                entries: vec![(LogicalRef::Key(key("k/queued")), RevisionId(0))],
                source: ReadSource::Client,
            },
        ]
    };
    // The author's overlay holds a queued proposal creating k/queued.
    let (q, blob) = content("q");
    let mut overlay = e.team.replica.clone();
    overlay
        .scratch_apply(
            "alice",
            [0x55; 16],
            &[TeamOp::New {
                path: "k/queued".into(),
                lane: "main".into(),
                kind: RecordKind::Decision,
                content: q,
                adopt_as_base: false,
            }],
            &[(q, blob.1)].into(),
        )
        .unwrap();
    let with_queue: BTreeMap<TeamId, Overlay> = [(TEAM, overlay)].into();
    assert_eq!(
        e.route_as(PrincipalId::STORE_OWNER, ops(), &with_queue),
        Err(TxError::Unpublished)
    );
    // Not queued (or queued where the author cannot read): the kernel's
    // reply for a key that does not exist.
    let kernel = {
        let mut b = Batch::new(ops());
        b.channel = Channel::Agent;
        e.k.what_if(&b).unwrap_err()
    };
    assert_eq!(e.route(ops()), Err(kernel));
}

#[test]
fn t109_a_hidden_mirror_keeps_the_kernels_unauthorized_read() {
    let mut e = Edge0::new();
    let access = AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: [("alice".into(), 501), ("bob".into(), 502)].into(),
        groups: BTreeMap::new(),
        lanes: [(
            "repo".into(),
            LaneEntry {
                members: ["*members".to_owned()].into(),
                writers: ["*members".to_owned()].into(),
                curators: ["alice".to_owned()].into(),
                egress: true,
                grants: BTreeMap::new(),
                deny: BTreeSet::new(),
            },
        )]
        .into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
        team_lanes: None,
    };
    let mut b = Batch::new(vec![Op::SetAccess {
        doc: Box::new(access),
    }]);
    b.channel = Channel::Admin;
    e.k.commit(&b).unwrap();
    let bob = PrincipalId(16);
    let ma = e.head(&key("k/a"));
    // Team lanes fall to default_lane (owner only): bob reads no mirror.
    let o = e.overlays();
    let r = e.route_as(
        bob,
        vec![
            local_create("dec:bob", 6, false),
            Op::SetHead {
                logical: LogicalRef::Key(key("k/a")),
                expected: RevRef::Existing(ma),
                new: RevRef::Existing(ma),
            },
        ],
        &o,
    );
    assert!(
        matches!(
            r,
            Err(TxError::Unauthorized {
                need: AccessNeed::Read,
                ..
            })
        ),
        "{r:?}"
    );
}
