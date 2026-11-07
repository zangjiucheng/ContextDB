//! P9 milestone 2 (ADR 0017 §3.1, §4): the access document. `SetAccess`
//! through the kernel and the independent branch oracle, the validation
//! rules of §4.2, principal id assignment, and `Authorized` against the
//! oracle's decision table (the read half of PROP15 over responses is
//! milestone 3).
use contextdb_core::{
    Kernel,
    access::{AccessState, LogicalFacts, Step, authorized},
};
use contextdb_model::*;
use contextdb_oracle::{AccessModel, BranchOracle};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

fn seeds(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}
fn lane(members: &[&str], writers: &[&str], curators: &[&str]) -> LaneEntry {
    LaneEntry {
        members: set(members),
        writers: set(writers),
        curators: set(curators),
        egress: true,
        grants: BTreeMap::new(),
        deny: BTreeSet::new(),
    }
}
/// ADR 0017 §4.1's example.
fn example() -> AccessDoc {
    let mut billing = lane(&["@billing"], &["@billing"], &["bob"]);
    billing.egress = false;
    billing
        .grants
        .insert("carol".into(), set(&["src/billing/invoices"]));
    let mut repo = lane(&["*members"], &["*members"], &["alice"]);
    repo.deny.insert(("carol".into(), "secrets.".into()));
    AccessDoc {
        version: 1,
        owner: "alice".into(),
        principals: [
            ("alice".into(), 501),
            ("bob".into(), 502),
            ("carol".into(), 503),
        ]
        .into(),
        groups: [("billing".into(), set(&["bob", "carol"]))].into(),
        lanes: [("repo".into(), repo), ("billing-notes".into(), billing)].into(),
        code_lanes: LaneEntry::code_default(),
        personal_egress: false,
        default_lane: LaneEntry::owner_only(),
    }
}
fn admin(ops: Vec<Op>) -> Batch {
    let mut b = Batch::new(ops);
    b.channel = Channel::Admin;
    b
}
fn set_access(doc: AccessDoc) -> Op {
    Op::SetAccess { doc: Box::new(doc) }
}
fn code(e: &TxError) -> String {
    format!("{e:?}")
        .split(['{', '(', ' '])
        .next()
        .unwrap()
        .to_owned()
}
fn hash(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}

#[test]
fn set_access_op_round_trips_and_needs_eventlog_v3() {
    let b = admin(vec![set_access(example())]);
    let bytes = b.canonical_bytes();
    assert_eq!(Batch::from_bytes(&bytes).unwrap(), b);
    assert_eq!(EventLog::version_of(&b), 3);
    let log = EventLog {
        batches: vec![b.clone()],
    };
    assert_eq!(EventLog::from_jsonl(&log.to_jsonl()).unwrap(), log);
    // A v2 line cannot carry it.
    let v2 = log.to_jsonl().replacen("\"version\":3", "\"version\":2", 1);
    assert_eq!(EventLog::from_jsonl(&v2), Err(WireError::UnknownVersion));
}

#[test]
fn set_access_is_admin_only_sole_on_main_and_never_dry_run() {
    let mut k = Kernel::default();
    let mut o = BranchOracle::new(hash);
    let fork = admin(vec![Op::CreateBranch { name: "f".into() }]);
    k.commit(&fork).unwrap();
    o.apply(&fork, true).unwrap();
    let mut agent = admin(vec![set_access(example())]);
    agent.channel = Channel::Agent;
    let mut on_branch = admin(vec![set_access(example())]);
    on_branch.branch = BranchId(1);
    let with_other = admin(vec![
        set_access(example()),
        Op::SetPolicy {
            policy: PolicyDoc::default(),
        },
    ]);
    let cases = [
        (agent, true, "ChannelRestricted"),
        (on_branch, true, "InvalidAccess"),
        (with_other, true, "SoleOpRequired"),
        (admin(vec![set_access(example())]), false, "InvalidAccess"),
    ];
    for (b, publish, want) in cases {
        let got = if publish { k.commit(&b) } else { k.what_if(&b) };
        assert_eq!(code(&got.unwrap_err()), want, "{b:?}");
        assert_eq!(code(&o.apply(&b, publish).unwrap_err()), want);
        assert!(k.access().is_none() && o.access().is_none());
    }
    let ok = admin(vec![set_access(example())]);
    let r = k.commit(&ok).unwrap();
    o.apply(&ok, true).unwrap();
    assert_eq!(r.epoch, Epoch(2));
    assert_eq!(k.epoch(), o.epoch());
    let ids: BTreeMap<String, u32> = (k.access().unwrap().ids.iter())
        .map(|(n, p)| (n.clone(), p.0))
        .collect();
    assert_eq!(ids, o.access().unwrap().ids);
    assert_eq!(
        ids,
        [
            ("alice".into(), 0),
            ("bob".into(), 16),
            ("carol".into(), 17)
        ]
        .into()
    );
    // Global: every branch's later commit sees the same document.
    let snap: Kernel = Kernel::import(k.export()).unwrap();
    assert_eq!(snap.access(), k.access());
}

#[test]
fn access_document_validation_cases() {
    type Edit = fn(&mut AccessDoc);
    let cases: [(&str, Edit); 17] = [
        ("unknown version", |d| d.version = 2),
        ("missing owner entry", |d| d.owner = "zed".into()),
        ("uid mapped twice", |d| {
            d.principals.insert("dave".into(), 502);
        }),
        ("bad principal name", |d| {
            d.principals.insert("Dave".into(), 504);
        }),
        ("bad group name", |d| {
            d.groups.insert("Bad Group".into(), set(&["bob"]));
        }),
        ("group with unknown principal", |d| {
            d.groups.insert("g".into(), set(&["zed"]));
        }),
        ("nested group", |d| {
            d.groups.insert("g".into(), set(&["@billing"]));
        }),
        ("unknown role token", |d| {
            d.lanes
                .get_mut("repo")
                .unwrap()
                .members
                .insert("zed".into());
        }),
        ("*writers outside curators", |d| {
            d.lanes
                .get_mut("repo")
                .unwrap()
                .writers
                .insert("*writers".into());
        }),
        ("unknown group token", |d| {
            d.lanes
                .get_mut("repo")
                .unwrap()
                .members
                .insert("@nope".into());
        }),
        ("grant dir with ..", |d| {
            (d.lanes.get_mut("billing-notes").unwrap().grants)
                .insert("bob".into(), set(&["src/../etc"]));
        }),
        ("grant dir with *", |d| {
            (d.lanes.get_mut("billing-notes").unwrap().grants)
                .insert("bob".into(), set(&["src/*"]));
        }),
        ("empty deny prefix", |d| {
            (d.lanes.get_mut("repo").unwrap().deny).insert(("bob".into(), String::new()));
        }),
        ("listed personal lane", |d| {
            d.lanes
                .insert("personal:bob".into(), lane(&["bob"], &[], &[]));
        }),
        ("code lane written by others", |d| {
            d.lanes
                .insert("code:r1".into(), lane(&["*members"], &["bob"], &[]));
        }),
        ("code_lanes written by members", |d| {
            d.code_lanes.writers = set(&["*members"]);
        }),
        ("grant for a non-member", |d| {
            // alice is not in @billing.
            (d.lanes.get_mut("billing-notes").unwrap().grants)
                .insert("alice".into(), set(&["src"]));
        }),
    ];
    for (what, edit) in cases {
        let mut doc = example();
        edit(&mut doc);
        let b = admin(vec![set_access(doc)]);
        let e = Kernel::default().commit(&b).unwrap_err();
        assert_eq!(code(&e), "InvalidAccess", "{what}: {e:?}");
        let e = BranchOracle::new(hash).apply(&b, true).unwrap_err();
        assert_eq!(code(&e), "InvalidAccess", "oracle, {what}");
    }
    // The example, `*writers` curators and a group deny are fine.
    let mut ok = example();
    ok.lanes.get_mut("repo").unwrap().curators = set(&["*writers"]);
    (ok.lanes.get_mut("billing-notes").unwrap().deny).insert(("@billing".into(), "x.".into()));
    Kernel::default()
        .commit(&admin(vec![set_access(ok)]))
        .unwrap();
}

#[test]
fn principal_ids_are_assigned_once_and_never_reused() {
    let mut k = Kernel::default();
    let mut doc = example();
    k.commit(&admin(vec![set_access(doc.clone())])).unwrap();
    // carol leaves, dave joins: dave gets a fresh id, carol's is not reused.
    doc.principals.remove("carol");
    doc.groups.insert("billing".into(), set(&["bob"]));
    let b = doc.lanes.get_mut("billing-notes").unwrap();
    b.grants.clear();
    doc.lanes.get_mut("repo").unwrap().deny.clear();
    doc.principals.insert("dave".into(), 504);
    k.commit(&admin(vec![set_access(doc.clone())])).unwrap();
    let a = k.access().unwrap();
    assert_eq!(a.ids["dave"], PrincipalId(18));
    assert_eq!(a.ids["carol"], PrincipalId(17));
    assert_eq!(a.principal_of_name("carol"), None);
    assert_eq!(a.name_of(PrincipalId(17)), None);
    // carol comes back under her first id, even with a new uid.
    doc.principals.insert("carol".into(), 510);
    k.commit(&admin(vec![set_access(doc.clone())])).unwrap();
    assert_eq!(
        k.access().unwrap().principal_of_name("carol"),
        Some(PrincipalId(17))
    );
    // Renaming the owner: the new owner name maps to 0, and neither name can
    // switch sides afterwards.
    let mut renamed = doc.clone();
    let uid = renamed.principals.remove("alice").unwrap();
    renamed.principals.insert("alicia".into(), uid);
    renamed.owner = "alicia".into();
    let repo = renamed.lanes.get_mut("repo").unwrap();
    repo.curators = set(&["alicia"]);
    k.commit(&admin(vec![set_access(renamed.clone())])).unwrap();
    assert_eq!(k.access().unwrap().ids["alicia"], PrincipalId::STORE_OWNER);
    let mut back = renamed.clone();
    back.principals.insert("alice".into(), 520);
    let e = k.commit(&admin(vec![set_access(back)])).unwrap_err();
    assert_eq!(
        code(&e),
        "InvalidAccess",
        "a former owner name as another principal"
    );
    let mut promote = renamed.clone();
    promote.principals.remove("alicia");
    promote.principals.insert("bob".into(), uid);
    promote.principals.insert("erin".into(), 502);
    promote.owner = "bob".into();
    promote.lanes.get_mut("repo").unwrap().curators = set(&["bob"]);
    let e = k.commit(&admin(vec![set_access(promote)])).unwrap_err();
    assert_eq!(code(&e), "InvalidAccess", "a mapped name as the owner");
}

#[test]
fn authorized_follows_the_adr_steps() {
    let s = AccessState::apply(None, &example()).unwrap();
    let (bob, carol) = (PrincipalId(16), PrincipalId(17));
    let none = BTreeSet::new();
    let check = |p, key: &str, lane: &str, owner, dirs: &BTreeSet<String>| {
        let d = s.authorized(
            p,
            &LogicalFacts {
                key,
                lane,
                owner,
                dirs,
            },
        );
        (d.allowed, d.step)
    };
    let owner = PrincipalId::STORE_OWNER;
    assert_eq!(
        check(PrincipalId(1), "k", "personal:bob", bob, &none),
        (true, Step::StoreOwner)
    );
    assert_eq!(
        check(PrincipalId(0x5641_0003), "k", "x", bob, &none).1,
        Step::StoreOwner
    );
    assert_eq!(
        check(bob, "k", "default", bob, &none),
        (true, Step::LogicalOwner)
    );
    assert_eq!(
        check(bob, "k", "unlisted", owner, &none),
        (false, Step::Membership)
    );
    assert_eq!(
        check(bob, "k", "personal:carol", owner, &none),
        (false, Step::Membership)
    );
    assert_eq!(
        check(bob, "k", "personal:bob", owner, &none),
        (true, Step::Allowed)
    );
    assert_eq!(check(bob, "k", "repo", owner, &none), (true, Step::Allowed));
    assert_eq!(
        check(bob, "k", "code:r9", owner, &none),
        (true, Step::Allowed)
    );
    // carol is narrowed in billing-notes to src/billing/invoices.
    let inv = set(&["src/billing/invoices/2026"]);
    let other = set(&["src/billing/payroll"]);
    let prefix_trap = set(&["src/billing/invoicesX"]);
    assert_eq!(
        check(carol, "k", "billing-notes", owner, &inv),
        (true, Step::Allowed)
    );
    assert_eq!(
        check(carol, "k", "billing-notes", owner, &other),
        (false, Step::Role)
    );
    assert_eq!(
        check(carol, "k", "billing-notes", owner, &prefix_trap),
        (false, Step::Role)
    );
    assert_eq!(
        check(carol, "k", "billing-notes", owner, &none),
        (false, Step::Role)
    );
    assert_eq!(
        check(bob, "k", "billing-notes", owner, &none),
        (true, Step::Allowed)
    );
    // carol's deny in repo.
    assert_eq!(
        check(carol, "secrets.db", "repo", owner, &none),
        (false, Step::Acl)
    );
    assert_eq!(
        check(carol, "db.secrets", "repo", owner, &none),
        (true, Step::Allowed)
    );
    // A principal no longer listed is a member of nothing.
    assert_eq!(
        check(PrincipalId(40), "k", "repo", owner, &none),
        (false, Step::Membership)
    );
    // Without a document only the store owner (and a logical's owner).
    assert!(
        authorized(
            None,
            PrincipalId(1),
            &LogicalFacts {
                key: "k",
                lane: "repo",
                owner: bob,
                dirs: &none
            }
        )
        .allowed
    );
    assert!(
        !authorized(
            None,
            bob,
            &LogicalFacts {
                key: "k",
                lane: "repo",
                owner,
                dirs: &none
            }
        )
        .allowed
    );
}

/// Random documents (valid and invalid) and random logicals.
struct Gen(ChaCha8Rng);
impl Gen {
    fn pick(&mut self, n: usize) -> usize {
        self.0.next_u32() as usize % n.max(1)
    }
    fn coin(&mut self, percent: u32) -> bool {
        self.0.next_u32() % 100 < percent
    }
    fn some<'a>(&mut self, pool: &[&'a str], max: usize) -> Vec<&'a str> {
        (0..self.pick(max + 1))
            .map(|_| pool[self.pick(pool.len())])
            .collect()
    }
    const NAMES: [&'static str; 6] = ["alice", "bob", "carol", "dave", "erin", "frank"];
    const LANES: [&'static str; 6] = ["repo", "billing", "notes", "code:r1", "personal:bob", "x"];
    const DIRS: [&'static str; 5] = [
        "src",
        "src/billing",
        "src/billing/invoices",
        "docs",
        "src/a",
    ];
    fn doc(&mut self) -> AccessDoc {
        let n = 2 + self.pick(4);
        let mut principals = BTreeMap::new();
        for (i, name) in Self::NAMES.iter().take(n).enumerate() {
            let uid = if self.coin(3) { 501 } else { 501 + i as u32 };
            principals.insert((*name).to_owned(), uid);
        }
        let names: Vec<&str> = Self::NAMES[..n].to_vec();
        let owner = if self.coin(3) {
            "zed"
        } else {
            names[self.pick(2)]
        };
        let mut groups = BTreeMap::new();
        let mut tokens: Vec<&str> = names.clone();
        tokens.extend(["*members", "*owner"]);
        for g in ["g1", "g2"].iter().take(self.pick(3)) {
            let mut m: Vec<&str> = self.some(&names, 3);
            m.push(names[self.pick(n)]);
            if self.coin(3) {
                m.push("@g1");
            }
            groups.insert((*g).to_owned(), set(&m));
            tokens.push(if *g == "g1" { "@g1" } else { "@g2" });
        }
        if self.coin(3) {
            tokens.push("@nope");
        }
        let entry = |g: &mut Self, code: bool| {
            let mut members = g.some(&tokens, 3);
            if g.coin(70) {
                members.push("*members");
            }
            let mut e = LaneEntry {
                members: set(&members),
                writers: if code && !g.coin(10) {
                    set(&["*owner"])
                } else {
                    set(&g.some(&tokens, 2))
                },
                curators: set(&g.some(&tokens, 2)),
                egress: g.coin(50),
                grants: BTreeMap::new(),
                deny: BTreeSet::new(),
            };
            if g.coin(15) {
                e.curators.insert("*writers".into());
            }
            for _ in 0..g.pick(3) {
                let who = tokens[g.pick(tokens.len())];
                if !who.starts_with('*') {
                    let mut dirs = g.some(&Self::DIRS, 2);
                    if g.coin(3) {
                        dirs.push("src/*");
                    }
                    e.grants
                        .entry(who.to_owned())
                        .or_default()
                        .extend(set(&dirs));
                }
            }
            for _ in 0..g.pick(3) {
                let who = tokens[g.pick(tokens.len())];
                if !who.starts_with('*') {
                    let prefix = if g.coin(3) {
                        ""
                    } else {
                        ["s.", "db/", "billing"][g.pick(3)]
                    };
                    e.deny.insert((who.to_owned(), prefix.to_owned()));
                }
            }
            e
        };
        let mut lanes = BTreeMap::new();
        for l in Self::LANES {
            if self.coin(50) && (l != "personal:bob" || self.coin(10)) {
                let e = entry(self, l.starts_with("code:"));
                lanes.insert(l.to_owned(), e);
            }
        }
        let code_lanes = if self.coin(50) {
            LaneEntry::code_default()
        } else {
            entry(self, true)
        };
        let default_lane = if self.coin(50) {
            LaneEntry::owner_only()
        } else {
            entry(self, false)
        };
        AccessDoc {
            version: if self.coin(2) { 2 } else { 1 },
            owner: owner.to_owned(),
            principals,
            groups,
            lanes,
            code_lanes,
            personal_egress: self.coin(50),
            default_lane,
        }
    }
}

#[test]
fn prop15_authorized_and_ids_match_the_reference() {
    let n = seeds("CONTEXTDB_P9_ACCESS_SEEDS", 2_000);
    let (mut valid, mut decisions) = (0u64, [0u64; 7]);
    for seed in 0..n {
        let mut g = Gen(ChaCha8Rng::seed_from_u64(seed));
        let (mut engine, mut reference): (Option<AccessState>, Option<AccessModel>) = (None, None);
        for _ in 0..1 + g.pick(4) {
            let doc = g.doc();
            let e = AccessState::apply(engine.as_ref(), &doc);
            let r = AccessModel::apply(reference.as_ref(), &doc);
            assert_eq!(
                e.as_ref().map(|_| ()).map_err(code),
                r.as_ref().map(|_| ()).map_err(code),
                "seed {seed}: {doc:?}"
            );
            if let (Ok(e), Ok(r)) = (e, r) {
                let ids: BTreeMap<String, u32> =
                    e.ids.iter().map(|(n, p)| (n.clone(), p.0)).collect();
                assert_eq!(ids, r.ids, "seed {seed}");
                assert!(e.mapped().all(|(_, _, p)| !p.is_reserved()));
                engine = Some(e);
                reference = Some(r);
                valid += 1;
            }
        }
        let (Some(e), Some(r)) = (&engine, &reference) else {
            continue;
        };
        let mut principals: Vec<PrincipalId> = e.ids.values().copied().collect();
        principals.extend([PrincipalId(1), PrincipalId(0x5641_0001), PrincipalId(99)]);
        let lanes = [
            "repo",
            "billing",
            "notes",
            "code:r1",
            "code:r2",
            "personal:alice",
            "personal:bob",
            "personal:zed",
            "other",
        ];
        for _ in 0..40 {
            let p = principals[g.pick(principals.len())];
            let owner = principals[g.pick(principals.len())];
            let key = ["s.k", "db/x", "billing.y", "k"][g.pick(4)];
            let lane = lanes[g.pick(lanes.len())];
            let dirs = set(&g.some(
                &[
                    "src",
                    "src/billing",
                    "src/billing/invoices/x",
                    "docs/",
                    "srcx",
                ],
                2,
            ));
            let d = e.authorized(
                p,
                &LogicalFacts {
                    key,
                    lane,
                    owner,
                    dirs: &dirs,
                },
            );
            let (allowed, step) = r.authorized(p, key, lane, owner, &dirs);
            assert_eq!(
                (d.allowed, d.step.name()),
                (allowed, step),
                "seed {seed}: {p:?} {key} {lane} {owner:?} {dirs:?}"
            );
            decisions[d.step as usize] += 1;
        }
    }
    // Every step but Visibility is reached; most documents are valid.
    // Visibility cannot decide while personal entries are synthesised
    // (ADR 0017 §4.4 keeps it for a future document format): a principal
    // is never a member of another's personal lane.
    assert_eq!(decisions[Step::Visibility as usize], 0);
    let reached = decisions.iter().filter(|&&c| c > 0).count();
    assert_eq!(reached, decisions.len() - 1, "{decisions:?}");
    assert!(valid > n / 2, "{valid} valid documents of {n} seeds");
}

/// PROP15, write half (ADR 0017 §6.1, §6.2, §11 G3): random `generate_p7`
/// histories whose logicals spread over shared, personal (listed and
/// unlisted), code and unlisted lanes, committed by random principals
/// (the store owner, mapped ones, an unknown one), with `SetMetadata` dirs
/// and a second access document in the middle. The kernel and the
/// independent branch oracle accept and refuse exactly the same batches,
/// with the same error (op index, ref and need included), and end in the
/// same epoch. `CONTEXTDB_P9_WRITE_SEEDS` (default 1,000).
#[test]
fn prop15_write_refusals_match_the_reference() {
    let n = seeds("CONTEXTDB_P9_WRITE_SEEDS", 1_000);
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for seed in 0..n {
        let mut g = Gen(ChaCha8Rng::seed_from_u64(seed ^ 0x5039_0004));
        // Two documents, the second valid over the first (a revision of
        // the same store: same owner name).
        let first = loop {
            let doc = g.doc();
            if let Ok(s) = AccessState::apply(None, &doc) {
                break (doc, s);
            }
        };
        let second = loop {
            let doc = g.doc();
            if AccessState::apply(Some(&first.1), &doc).is_ok() {
                break doc;
            }
        };
        let docs = [first.0, second];
        let mut g = Gen(ChaCha8Rng::seed_from_u64(seed));
        let mut k = Kernel::default();
        // The single-branch reference (every batch here is on `main`) with
        // the oracle's own access model.
        let mut o = contextdb_oracle::InputModel::default();
        let mut m: Option<AccessModel> = None;
        let history = contextdb_testkit::generate_p7(seed).batches;
        let middle = history.len() / 2;
        let lanes = [
            "repo",
            "billing",
            "notes",
            "code:r1",
            "personal:alice",
            "personal:bob",
            "personal:carol",
            "personal:zed",
            "x",
        ];
        let mut both = |b: &Batch,
                        k: &mut Kernel,
                        o: &mut contextdb_oracle::InputModel,
                        m: &Option<AccessModel>| {
            let got = k.commit(b);
            let want = o.apply_authorised(b, &|_| false, m.as_ref());
            match (&got, &want) {
                (Ok(r), Ok(a)) => assert_eq!(r.allocations, a.allocations, "seed {seed}"),
                (Err(e), Err(w)) => assert_eq!(e, w, "seed {seed}: {b:?}"),
                _ => panic!("seed {seed}: kernel {got:?}, oracle {want:?}, batch {b:?}"),
            }
            let label = match (&got, b.principal.is_store_owner()) {
                (Ok(_), true) => "owner ok".to_owned(),
                (Ok(_), false) => "principal ok".to_owned(),
                (Err(TxError::Unauthorized { need, .. }), _) => format!("Unauthorized {need:?}"),
                (Err(e), _) => code(e),
            };
            *counts.entry(label).or_default() += 1;
        };
        for (i, mut b) in history.into_iter().enumerate() {
            if i == 0 || i == middle {
                let d = docs[usize::from(i != 0)].clone();
                k.commit(&admin(vec![set_access(d.clone())])).unwrap();
                m = Some(AccessModel::apply(m.as_ref(), &d).unwrap());
                // SetAccess takes an epoch in the kernel only.
                o = contextdb_oracle::InputModel::from_inputs(o.inputs().clone(), k.epoch());
            }
            for op in &mut b.ops {
                if let Op::CreateLogical { scope_lane, .. } = op {
                    *scope_lane = lanes[g.pick(lanes.len())].into();
                }
            }
            let ids: Vec<PrincipalId> = k.access().unwrap().ids.values().copied().collect();
            b.principal = match g.pick(4) {
                0 => PrincipalId(0),
                3 => PrincipalId(99),
                _ => ids[g.pick(ids.len())],
            };
            both(&b, &mut k, &mut o, &m);
            if g.coin(30) {
                let count = k.branch(MAIN).unwrap().logical_count();
                if count > 0 {
                    let mut x = Batch::new(vec![Op::SetMetadata {
                        logical: LogicalRef::Existing(LogicalId(g.pick(count) as u32)),
                        tags: BTreeSet::new(),
                        dirs: set(
                            &g.some(&["src", "src/billing", "src/billing/invoices/x", "docs"], 2)
                        ),
                    }]);
                    x.principal = ids[g.pick(ids.len())];
                    both(&x, &mut k, &mut o, &m);
                }
            }
        }
        assert_eq!(k.epoch(), o.epoch(), "seed {seed}");
    }
    eprintln!("PROP15 write half, {n} seeds: {counts:?}");
    for want in [
        "owner ok",
        "principal ok",
        "Unauthorized Read",
        "Unauthorized Write",
        "Unauthorized Curate",
        "InvalidLane",
    ] {
        assert!(
            counts.get(want).is_some_and(|c| *c > 0),
            "{want} never seen: {counts:?}"
        );
    }
}

/// PROP15, write half, merges (ADR 0017 §6.2 rules 6–7): a branch created
/// by a random principal, random writes on it and on `main` by random
/// principals (lanes, bases, metadata, rules), then merges either way by
/// random principals. The kernel and the independent branch oracle agree
/// on every outcome, `MergeUnauthorized{count}` and
/// `BranchOfAnotherPrincipal` included; a merge into the merger's own
/// branch is never refused for authorisation. `CONTEXTDB_P9_MERGE_SEEDS`
/// (default 1,000).
#[test]
fn prop15_merge_refusals_match_the_reference() {
    let n = seeds("CONTEXTDB_P9_MERGE_SEEDS", 1_000);
    let authz = |e: &TxError| {
        matches!(
            e,
            TxError::Unauthorized { .. }
                | TxError::MergeUnauthorized { .. }
                | TxError::BranchOfAnotherPrincipal
                | TxError::InvalidLane { .. }
        )
    };
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let lanes = [
        "repo",
        "billing",
        "notes",
        "personal:alice",
        "personal:bob",
        "x",
    ];
    for seed in 0..n {
        let mut g = Gen(ChaCha8Rng::seed_from_u64(seed ^ 0x5039_0005));
        let doc = loop {
            let d = g.doc();
            if AccessState::apply(None, &d).is_ok() {
                break d;
            }
        };
        let mut k = Kernel::default();
        let mut o = BranchOracle::new(hash);
        let run = |b: &Batch,
                   k: &mut Kernel,
                   o: &mut BranchOracle,
                   counts: &mut BTreeMap<String, u64>| {
            let got = k.commit(b);
            let want = o.apply(b, true);
            match (&got, &want) {
                (Ok(r), Ok((a, _))) => assert_eq!(r.epoch, a.epoch, "seed {seed}"),
                // Authorisation errors match exactly; others (whose ids or
                // cycle paths the two report in their own ways on branches)
                // by variant.
                (Err(e), Err(w)) if authz(e) || authz(w) => {
                    assert_eq!(e, w, "seed {seed}: {b:?}")
                }
                (Err(e), Err(w)) => assert_eq!(code(e), code(w), "seed {seed}: {b:?}"),
                _ => panic!("seed {seed}: kernel {got:?}, oracle {want:?}, batch {b:?}"),
            }
            let merge = b.ops.iter().any(|op| matches!(op, Op::Merge { .. }));
            let label = match &got {
                Ok(_) if merge => "merge ok".to_owned(),
                Ok(_) => "ok".to_owned(),
                Err(TxError::MergeUnauthorized { .. }) => "MergeUnauthorized".to_owned(),
                Err(TxError::Unauthorized { need, .. }) => format!("Unauthorized {need:?}"),
                Err(e) => code(e),
            };
            *counts.entry(label).or_default() += 1;
            got.is_ok()
        };
        run(&admin(vec![set_access(doc)]), &mut k, &mut o, &mut counts);
        let ids: Vec<PrincipalId> = k.access().unwrap().ids.values().copied().collect();
        let pick_principal = |g: &mut Gen| match g.pick(3) {
            0 => PrincipalId(0),
            _ => ids[g.pick(ids.len())],
        };
        // Some owner records on main first.
        let mut serial = 0u32;
        let create = |g: &mut Gen, serial: &mut u32| {
            *serial += 1;
            Op::CreateLogical {
                key: format!("k{serial}"),
                kind: RecordKind::Fact,
                scope_lane: lanes[g.pick(lanes.len())].into(),
                content: [*serial as u8; 32],
                adopt_as_base: g.coin(50),
            }
        };
        let seed_ops: Vec<Op> = (0..4).map(|_| create(&mut g, &mut serial)).collect();
        run(&Batch::new(seed_ops), &mut k, &mut o, &mut counts);
        let mut fork = admin(vec![Op::CreateBranch { name: "f".into() }]);
        fork.channel = Channel::Agent;
        fork.principal = pick_principal(&mut g);
        if !run(&fork, &mut k, &mut o, &mut counts) {
            continue;
        }
        let f = BranchId(1);
        for _ in 0..10 {
            let on = if g.coin(50) { MAIN } else { f };
            let p = pick_principal(&mut g);
            if g.coin(25) {
                // A merge, either way.
                let source = if on == MAIN { f } else { MAIN };
                let mut b = Batch::new(vec![Op::Merge {
                    source,
                    require_certified: false,
                    certify: Vec::new(),
                }]);
                b.branch = on;
                b.principal = p;
                let creator_is_p = {
                    let v = k.branch(on).unwrap();
                    v.info().creator == p && !p.is_store_owner()
                };
                let ok = run(&b, &mut k, &mut o, &mut counts);
                if creator_is_p && !ok {
                    // Never refused for authorisation into one's own branch.
                    let e = k.what_if(&b).err();
                    assert!(
                        !matches!(e, Some(TxError::MergeUnauthorized { .. })),
                        "seed {seed}: {e:?}"
                    );
                }
                continue;
            }
            let v = k.branch(on).unwrap();
            let logicals: Vec<LogicalId> = v.logicals().collect();
            let heads: Vec<RevisionId> = logicals.iter().filter_map(|l| v.head(*l)).collect();
            let mut ops = Vec::new();
            for _ in 0..1 + g.pick(2) {
                let op = match g.pick(6) {
                    0 => create(&mut g, &mut serial),
                    1 => {
                        serial += 1;
                        Op::AddRevision {
                            logical: LogicalRef::Existing(logicals[g.pick(logicals.len())]),
                            content: [serial as u8; 32],
                            adopt_as_base: g.coin(40),
                        }
                    }
                    2 => Op::AssertBase {
                        rev: heads[g.pick(heads.len())].into(),
                    },
                    3 => Op::RetractBase {
                        rev: heads[g.pick(heads.len())].into(),
                    },
                    4 => Op::SetMetadata {
                        logical: LogicalRef::Existing(logicals[g.pick(logicals.len())]),
                        tags: set(&g.some(&["t1", "t2"], 1)),
                        dirs: set(&g.some(&["src", "src/billing", "docs"], 1)),
                    },
                    _ => Op::AddRule {
                        conclusion: heads[g.pick(heads.len())].into(),
                        incidences: vec![(
                            heads[g.pick(heads.len())].into(),
                            [EdgeKind::Cur, EdgeKind::Ref][g.pick(2)],
                        )],
                        enabled: g.coin(80),
                    },
                };
                ops.push(op);
            }
            let mut b = Batch::new(ops);
            b.branch = on;
            b.principal = p;
            run(&b, &mut k, &mut o, &mut counts);
        }
    }
    eprintln!("PROP15 merges, {n} seeds: {counts:?}");
    for want in ["merge ok", "MergeUnauthorized", "BranchOfAnotherPrincipal"] {
        assert!(
            counts.get(want).is_some_and(|c| *c > 0),
            "{want} never seen: {counts:?}"
        );
    }
}
