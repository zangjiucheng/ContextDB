//! T116 (ADR 0018 §5.3, §8.3, §13): certificates over mirrors.
//!
//! - `ReplicationLag` is decided at read time over the L2-witness closure:
//!   a mirror, or a local record supported through one, reads
//!   `PENDING(ReplicationLag)` while the branch's `team:` watermark differs
//!   from the fetched tip in `live_pos`; issuing is refused, validation is
//!   `Unknown{lag}`.
//! - `strict_no_aba`: a team entry about other records leaves a strict
//!   certificate Valid; a root ABA on the head (across two imports and
//!   within one), a base retract-then-assert on an unchanged head, and a
//!   catch-up across a history gap are `Invalid{StrictChange}`, never Valid.
//!
//! Every engine answer is compared with the oracle, which decides strictness
//! from consecutive committed states and the per-epoch gap scopes.

use contextdb_core::{IssueRequest, Kernel, LogLink};
use contextdb_model::*;
use contextdb_oracle::{CertParams, cert_issue_oracle, cert_validate_oracle_gaps};
use contextdb_team::catchup::{self, History, Writer};
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::gitobj;
use contextdb_team::ops::{Hash, TeamBatch, TeamOp};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use std::collections::{BTreeMap, BTreeSet};

const TEAM: [u8; 16] = [0x16; 16];
const ALICE: [u8; 32] = [1; 32];
const SUM: ContentHash = [0x5a; 32];

fn content(name: &str) -> (Hash, (String, Vec<u8>)) {
    let b = name.as_bytes().to_vec();
    let h = *blake3::hash(&b).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), b))
}

/// The team side: a replica fed signed entries, and its history.
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
    fn new_record(&mut self, path: &str, body: &str) -> Hash {
        let (c, blob) = content(body);
        self.entry(
            vec![TeamOp::New {
                path: path.into(),
                lane: "main".into(),
                kind: RecordKind::Decision,
                content: c,
                adopt_as_base: true,
            }],
            vec![blob],
        );
        c
    }
    fn revise(&mut self, path: &str, body: &str) -> Hash {
        let (c, blob) = content(body);
        self.entry(
            vec![TeamOp::Revise {
                path: path.into(),
                content: c,
                adopt_as_base: true,
            }],
            vec![blob],
        );
        c
    }
    /// A new revision that becomes the head (from `from`).
    fn supersede(&mut self, path: &str, body: &str, from: Hash) -> Hash {
        let c = self.revise(path, body);
        self.set_head(path, from, c);
        c
    }
    fn set_head(&mut self, path: &str, expected: Hash, new: Hash) {
        self.entry(
            vec![TeamOp::SetHead {
                path: path.into(),
                expected,
                new,
            }],
            vec![],
        );
    }
    fn set_base(&mut self, path: &str, c: Hash, base: bool) {
        self.entry(
            vec![TeamOp::SetBase {
                path: path.into(),
                content: c,
                expected: !base,
                base,
            }],
            vec![],
        );
    }
}

/// The kernel with `main`'s committed input states (index = epoch) and the
/// team scopes each epoch's batch caught up across a gap, for the oracle.
/// Only `main` is written, so its ids are the global ids.
struct Rec {
    k: Kernel,
    history: Vec<Inputs>,
    gaps: Vec<BTreeSet<String>>,
}

impl Writer for Rec {
    type Error = TxError;
    fn kernel(&self) -> &Kernel {
        &self.k
    }
    fn commit(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        assert_eq!(b.branch, MAIN);
        let r = self.k.commit(b)?;
        self.history
            .push(self.k.branch(MAIN).unwrap().db().inputs());
        self.gaps.push(
            (b.ops.iter())
                .filter_map(|op| match op {
                    Op::ObserverWatermark {
                        scope, gap: true, ..
                    } => Some(scope.clone()),
                    _ => None,
                })
                .collect(),
        );
        assert_eq!(self.history.len() as u64 - 1, r.epoch.0);
        Ok(r)
    }
}

impl Rec {
    fn new() -> Self {
        let k = Kernel::default();
        let genesis = k.branch(MAIN).unwrap().db().inputs();
        Self {
            k,
            history: vec![genesis],
            gaps: vec![BTreeSet::new()],
        }
    }
    fn mirror_head(&self, path: &str) -> RevisionId {
        let v = self.k.branch(MAIN).unwrap();
        let l = v
            .logical_by_key(&format!("team/{}/{path}", hex(&TEAM)))
            .unwrap();
        v.head(l).unwrap()
    }
    /// A local Decision supported by `premise` through a `Cur` rule.
    fn local_on(&mut self, key: &str, premise: RevisionId) -> RevisionId {
        let mut b = Batch::new(vec![
            Op::CreateLogical {
                origin: Origin::Local,
                key: key.into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content: *blake3::hash(key.as_bytes()).as_bytes(),
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(premise.into(), EdgeKind::Cur)],
                enabled: true,
            },
        ]);
        b.channel = Channel::Agent;
        self.commit(&b).unwrap();
        let v = self.k.branch(MAIN).unwrap();
        v.head(v.logical_by_key(key).unwrap()).unwrap()
    }
    fn local_alone(&mut self, key: &str) -> RevisionId {
        let mut b = Batch::new(vec![Op::CreateLogical {
            origin: Origin::Local,
            key: key.into(),
            kind: RecordKind::Decision,
            scope_lane: "repo".into(),
            content: *blake3::hash(key.as_bytes()).as_bytes(),
            adopt_as_base: true,
        }]);
        b.channel = Channel::Agent;
        self.commit(&b).unwrap();
        let v = self.k.branch(MAIN).unwrap();
        v.head(v.logical_by_key(key).unwrap()).unwrap()
    }
    fn catch_up(&mut self, h: &History) -> usize {
        catchup::catch_up(self, MAIN, TEAM, h).unwrap().len()
    }
    /// `live_pos` with the team's fetched tip equal to `main`'s watermark.
    fn fresh(&self) -> BTreeMap<String, Vec<u8>> {
        let s = catchup::scope(TEAM);
        let w = self
            .k
            .branch(MAIN)
            .unwrap()
            .db()
            .watermark(&s)
            .unwrap()
            .to_vec();
        [(s, w)].into()
    }
    fn summary(&self, r: RevisionId, live: &BTreeMap<String, Vec<u8>>) -> Summary {
        self.k.branch(MAIN).unwrap().summary_at(r, live).unwrap()
    }
    fn request(&self, strict: bool) -> (IssueRequest, CertParams) {
        let r = IssueRequest {
            session_id: [7; 16],
            principal: PrincipalId::STORE_OWNER,
            branch: MAIN,
            log_checksum: SUM,
            code_commit: "c".into(),
            strict_no_aba: strict,
        };
        let p = CertParams {
            session_id: r.session_id,
            principal: r.principal,
            branch: MAIN,
            log_checksum: SUM,
            code_commit: r.code_commit.clone(),
            strict_no_aba: strict,
        };
        (r, p)
    }
    fn entries(&self, revs: &[RevisionId]) -> Vec<CertItem> {
        let v = self.k.branch(MAIN).unwrap();
        revs.iter()
            .map(|r| CertItem {
                logical: v.revision_logical(*r).unwrap(),
                rev: *r,
                required: true,
                via: Via::Head,
                since_epoch: self.k.epoch(),
            })
            .collect()
    }
    /// Issues on the engine and the oracle; they must agree.
    fn issue(
        &self,
        revs: &[RevisionId],
        strict: bool,
        live: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Certificate, IssueFailure> {
        let (request, params) = self.request(strict);
        let entries = self.entries(revs);
        let engine = (self.k.branch(MAIN).unwrap()).issue_certificate(&request, &entries, live);
        let oracle = cert_issue_oracle(&self.history, &params, &entries, live);
        assert_eq!(
            engine.clone().map(|mut c| {
                c.cert_id = [0; 32];
                c
            }),
            oracle,
            "issuance"
        );
        engine
    }
    fn validate(&self, c: &Certificate, live: &BTreeMap<String, Vec<u8>>) -> Validation {
        let engine = (self.k.branch(MAIN).unwrap())
            .validate_certificate(
                c,
                live,
                LogLink {
                    matches: true,
                    current_checksum: SUM,
                },
            )
            .expect("history from genesis");
        let mut zero = c.clone();
        zero.cert_id = [0; 32];
        let oracle = cert_validate_oracle_gaps(&self.history, &self.gaps, &zero, live, true, SUM);
        let norm = |v: Validation| match v {
            Validation::ValidRecertified {
                mut new_cert,
                diffs,
            } => {
                new_cert.cert_id = [0; 32];
                Validation::ValidRecertified { new_cert, diffs }
            }
            v => v,
        };
        assert_eq!(norm(engine.clone()), norm(oracle), "validation");
        engine
    }
}

fn strict_change(v: &Validation) -> bool {
    matches!(
        v,
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        }
    )
}

/// Team records `k/a` and `k/b`, both certified at the root; `main` caught
/// up; a local record supported by mirror `k/a`.
fn setup() -> (Team, Rec, Hash, RevisionId) {
    let mut team = Team::new();
    let a1 = team.new_record("k/a", "a1");
    team.new_record("k/b", "b1");
    let mut rec = Rec::new();
    rec.catch_up(&team.history);
    let ma = rec.mirror_head("k/a");
    let local = rec.local_on("dec:on-a", ma);
    (team, rec, a1, local)
}

#[test]
fn t116_replication_lag_is_decided_at_read_time_over_the_closure() {
    let (mut team, mut rec, _, local) = setup();
    let ma = rec.mirror_head("k/a");
    let solo = rec.local_alone("dec:solo");
    let fresh = rec.fresh();
    let scope = catchup::scope(TEAM);
    let lag = ReplicationLagCases {
        none: BTreeMap::new(),
        other: [(scope.clone(), b"another-oid".to_vec())].into(),
    };
    assert_eq!(rec.summary(ma, &fresh), Summary::Certified);
    assert_eq!(rec.summary(local, &fresh), Summary::Certified);
    for live in [&lag.none, &lag.other] {
        // A direct read of a mirror, and a record supported through one.
        for r in [ma, local] {
            assert_eq!(
                rec.summary(r, live),
                Summary::Pending(PendingReason::ReplicationLag)
            );
        }
        // A record with no mirror in its closure is not affected.
        assert_eq!(rec.summary(solo, live), Summary::Certified);
        // Issuing under lag is refused, as the oracle refuses it.
        assert!(matches!(
            rec.issue(&[local], false, live),
            Err(IssueFailure::NotCertified {
                summary: Summary::Pending(PendingReason::ReplicationLag),
                ..
            })
        ));
        assert!(rec.issue(&[solo], false, live).is_ok());
    }
    // Issued fresh, validated after the tip moved on: Unknown{lag}.
    let cert = rec.issue(&[local, solo], false, &fresh).unwrap();
    assert_eq!(rec.validate(&cert, &fresh), Validation::Valid);
    team.revise("k/b", "b2");
    let mut moved = fresh.clone();
    moved.insert(
        scope.clone(),
        team.history.tip().unwrap().as_bytes().to_vec(),
    );
    assert_eq!(
        rec.validate(&cert, &moved),
        Validation::Unknown {
            lag: vec![scope.clone()]
        }
    );
    // Caught up, the read is current again.
    rec.catch_up(&team.history);
    assert_eq!(rec.validate(&cert, &rec.fresh()), Validation::Valid);
    // Precedence: a STALE read stays STALE under lag (SD13 order).
    let a1 = rec.k.branch(MAIN).unwrap().revision_content(ma).unwrap();
    team.supersede("k/a", "a2", a1);
    rec.catch_up(&team.history);
    let old = ma;
    assert!(matches!(
        rec.summary(old, &lag.none),
        Summary::Stale(StaleReason::HeadSuperseded)
    ));
}

struct ReplicationLagCases {
    none: BTreeMap<String, Vec<u8>>,
    other: BTreeMap<String, Vec<u8>>,
}

#[test]
fn t116_team_entries_about_other_records_leave_a_strict_certificate_valid() {
    let (mut team, mut rec, _, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    team.revise("k/b", "b2");
    team.revise("k/b", "b3");
    rec.catch_up(&team.history);
    assert_eq!(rec.validate(&strict, &rec.fresh()), Validation::Valid);
}

#[test]
fn t116_head_aba_across_two_imports_is_a_strict_change() {
    let (mut team, mut rec, a1, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    let plain = rec.issue(&[local], false, &rec.fresh()).unwrap();
    let before = rec.mirror_head("k/a");
    let a2 = team.supersede("k/a", "a2", a1);
    rec.catch_up(&team.history);
    assert_ne!(rec.mirror_head("k/a"), before);
    team.set_head("k/a", a2, a1);
    rec.catch_up(&team.history);
    assert_eq!(rec.mirror_head("k/a"), before);
    let live = rec.fresh();
    assert!(strict_change(&rec.validate(&strict, &live)));
    // Non-strict: back on the same head, compared with the oracle.
    rec.validate(&plain, &live);
}

#[test]
fn t116_head_aba_within_one_import_is_split_and_a_strict_change() {
    let (mut team, mut rec, a1, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    let a2 = team.supersede("k/a", "a2", a1);
    team.set_head("k/a", a2, a1);
    let batches = rec.catch_up(&team.history);
    assert!(batches >= 2, "the split rule cuts the return: {batches}");
    assert!(strict_change(&rec.validate(&strict, &rec.fresh())));
}

#[test]
fn t116_base_retract_then_assert_on_an_unchanged_head_is_a_strict_change() {
    for one_import in [true, false] {
        let (mut team, mut rec, a1, local) = setup();
        let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
        team.set_base("k/a", a1, false);
        if !one_import {
            rec.catch_up(&team.history);
        }
        team.set_base("k/a", a1, true);
        rec.catch_up(&team.history);
        let live = rec.fresh();
        assert_eq!(rec.summary(local, &live), Summary::Certified);
        assert!(
            strict_change(&rec.validate(&strict, &live)),
            "one import: {one_import}"
        );
    }
}

#[test]
fn t116_head_aba_mixed_with_a_base_flip() {
    let (mut team, mut rec, a1, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    let a2 = team.supersede("k/a", "a2", a1);
    team.set_base("k/a", a2, false);
    team.set_head("k/a", a2, a1);
    rec.catch_up(&team.history);
    assert!(strict_change(&rec.validate(&strict, &rec.fresh())));
}

#[test]
fn t116_an_edge_offline_for_many_entries_catches_up_in_one_import() {
    let (mut team, mut rec, a1, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    // Twenty entries; every fifth takes `k/a` away from a1 and back.
    for n in 0..20 {
        team.revise("k/b", &format!("b-{n}"));
        if n % 5 == 4 {
            let next = team.supersede("k/a", &format!("a-{n}"), a1);
            team.set_head("k/a", next, a1);
        }
    }
    rec.catch_up(&team.history);
    assert!(strict_change(&rec.validate(&strict, &rec.fresh())));
}

#[test]
fn t116_a_history_gap_is_a_strict_change_never_valid() {
    let (mut team, mut rec, _, local) = setup();
    let strict = rec.issue(&[local], true, &rec.fresh()).unwrap();
    let plain = rec.issue(&[local], false, &rec.fresh()).unwrap();
    // The replica was rebuilt from a later checkpoint: its log no longer
    // holds the branch's position. Nothing about `k/a` changed.
    team.revise("k/b", "b2");
    let mut rebuilt = History::default();
    rebuilt.record(team.history.tip().unwrap(), team.history.current().clone());
    assert_eq!(rec.catch_up(&rebuilt), 1);
    assert_eq!(
        rec.gaps.last().unwrap(),
        &BTreeSet::from([catchup::scope(TEAM)])
    );
    let live = rec.fresh();
    assert!(strict_change(&rec.validate(&strict, &live)));
    // A non-strict certificate is unaffected by the gap.
    assert_eq!(rec.validate(&plain, &live), Validation::Valid);
    // A strict certificate issued after the gap is Valid again.
    let after = rec.issue(&[local], true, &live).unwrap();
    team.revise("k/b", "b3");
    rec.catch_up(&team.history);
    assert_eq!(rec.validate(&after, &rec.fresh()), Validation::Valid);
}
