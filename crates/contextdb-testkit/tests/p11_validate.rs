//! P11 certificate validation across a maintenance frontier (ADR 0019 §3.6,
//! SPEC §6.4 "P11 lazy 校验"): T61's core goldens (§9 G2) and the companion
//! property for the validate contract (§9 G3). While a required head is
//! unsettled the verdict aggregates only what is confirmed, is never
//! positive, and equals eager once everything has settled.
use contextdb_core::{Budget, Db, Drain, IssueRequest, LogLink};
use contextdb_model::*;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};

const SCOPE: &str = "repo";
const SEEDS: u64 = 10_000;
const TINY: Budget = Budget { units: 1, chunk: 1 };

fn create(key: &str, content: u8, base: bool) -> Op {
    Op::CreateLogical {
        origin: Default::default(),
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: SCOPE.into(),
        content: [content; 32],
        adopt_as_base: base,
    }
}
fn pin(conclusion: RevRef, premise: RevRef) -> Op {
    Op::AddRule {
        conclusion,
        incidences: vec![(premise, EdgeKind::Pin)],
        enabled: true,
    }
}
fn retract(r: u32) -> Op {
    Op::RetractBase {
        rev: RevisionId(r).into(),
    }
}
fn conflict(target: u32) -> Op {
    Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: RevisionId(target).into(),
        payload_hash: [target as u8; 32],
    }
}
fn request(strict: bool) -> IssueRequest {
    IssueRequest {
        session_id: [1; 16],
        principal: PrincipalId(1),
        branch: MAIN,
        log_checksum: [0; 32],
        code_commit: "abc".into(),
        strict_no_aba: strict,
    }
}
fn item(db: &Db, r: u32) -> CertItem {
    CertItem {
        logical: db.revision_logical(RevisionId(r)).unwrap(),
        rev: RevisionId(r),
        required: true,
        via: Via::Head,
        since_epoch: db.epoch(),
    }
}
fn issue(db: &Db, strict: bool, revs: &[u32]) -> Certificate {
    let entries: Vec<_> = revs.iter().map(|r| item(db, *r)).collect();
    db.issue_certificate(&request(strict), &entries, &BTreeMap::new())
        .unwrap()
}
const LINKED: LogLink = LogLink {
    matches: true,
    current_checksum: [0; 32],
};
fn validate(db: &Db, cert: &Certificate) -> Validation {
    db.validate_certificate(cert, &BTreeMap::new(), LINKED)
        .unwrap()
}
fn commit_both(lazy: &mut Db, eager: &mut Db, ops: Vec<Op>) {
    let batch = Batch::new(ops);
    eager.commit(&batch).unwrap();
    lazy.commit_lazy(&batch, TINY).unwrap();
    assert!(lazy.has_frontier());
}
/// Builds the same history twice: one for lazy commits, one eager.
fn pair(ops: impl Fn() -> Vec<Vec<Op>>) -> (Db, Db) {
    let build = || {
        let mut db = Db::default();
        for batch in ops() {
            db.commit(&Batch::new(batch)).unwrap();
        }
        db
    };
    (build(), build())
}
/// Slices of one unit until `until` holds of the lazy database.
fn drain_until(db: &mut Db, until: impl Fn(&Db) -> bool) {
    while !until(db) {
        assert!(db.has_frontier(), "the frontier emptied first");
        db.drain(Drain::Units(1));
    }
}
fn stale(cert_rev: u32, logical: LogicalId) -> CertDiff {
    CertDiff {
        logical,
        cert_rev: Some(RevisionId(cert_rev)),
        current: Some(RevisionId(cert_rev)),
        summary: Some(Summary::Stale(StaleReason::Unsupported)),
    }
}

/// The owner's case: A(base) → B, a certificate requiring B, A's base
/// retracted. While B is unsettled the verdict is `Pending{rules: []}`;
/// after the drain it is eager's `Invalid`.
#[test]
fn a_retracted_support_reads_pending_until_the_cascade_reaches_the_entry() {
    let (mut lazy, mut eager) = pair(|| {
        vec![vec![
            create("a", 0, true),
            create("b", 1, false),
            pin(RevRef::New(1), RevRef::New(0)),
        ]]
    });
    let cert = issue(&lazy, false, &[1]);
    assert_eq!(validate(&lazy, &cert), Validation::Valid);
    commit_both(&mut lazy, &mut eager, vec![retract(0)]);
    let b = RevisionId(1);
    assert!(!lazy.settled(b));
    assert_eq!(
        validate(&lazy, &cert),
        Validation::Pending { rules: vec![] }
    );
    lazy.drain(Drain::Full).unwrap();
    let want = Validation::Invalid {
        reason: InvalidReason::Changed,
        diffs: vec![stale(1, LogicalId(1))],
    };
    assert_eq!(validate(&eager, &cert), want);
    assert_eq!(validate(&lazy, &cert), want);
}

/// The owner's multi-entry case: required B and C; B's support A retracted
/// and a Conflict opened on C in one batch. With B unsettled and C settled
/// the verdict is `Conflicted`; after the drain eager's `Invalid`.
#[test]
fn a_settled_conflict_outranks_an_unsettled_entry_until_it_settles() {
    let (mut lazy, mut eager) = pair(|| {
        vec![vec![
            create("c", 0, true),
            create("a", 1, true),
            create("b", 2, false),
            pin(RevRef::New(2), RevRef::New(1)),
        ]]
    });
    let (c, b) = (RevisionId(0), RevisionId(2));
    let cert = issue(&lazy, false, &[2, 0]);
    commit_both(&mut lazy, &mut eager, vec![retract(1), conflict(0)]);
    drain_until(&mut lazy, |db| db.settled(c));
    assert!(!lazy.settled(b), "B must still be pending");
    assert_eq!(
        validate(&lazy, &cert),
        Validation::Conflicted {
            entries: vec![LogicalId(0)]
        }
    );
    lazy.drain(Drain::Full).unwrap();
    let want = Validation::Invalid {
        reason: InvalidReason::Changed,
        diffs: vec![stale(2, LogicalId(2))],
    };
    assert_eq!(validate(&eager, &cert), want);
    assert_eq!(validate(&lazy, &cert), want);
}

/// A hub X(base) with Pin dependents d0..d7 (revisions 1..=8).
fn hub() -> Vec<Vec<Op>> {
    let mut ops = vec![create("x", 0, true)];
    for i in 0..8 {
        ops.push(create(&format!("d{i}"), i + 1, false));
        ops.push(pin(RevRef::New(i as u16 + 1), RevRef::New(0)));
    }
    vec![ops]
}

/// N1: a strict certificate whose base revision was retracted gives
/// `Invalid{StrictChange}` as soon as that revision settles, while the
/// cascade below it is still pending.
#[test]
fn a_strict_change_is_exact_while_other_entries_are_unsettled() {
    let (mut lazy, mut eager) = pair(hub);
    let cert = issue(&lazy, true, &[0, 8]);
    commit_both(&mut lazy, &mut eager, vec![retract(0)]);
    drain_until(&mut lazy, |db| db.settled(RevisionId(0)));
    assert!(!lazy.settled(RevisionId(8)));
    let Validation::Invalid {
        reason: InvalidReason::StrictChange,
        diffs,
    } = validate(&lazy, &cert)
    else {
        panic!("strict change while the cascade is pending");
    };
    assert_eq!(diffs[0].logical, LogicalId(0));
    lazy.drain(Drain::Full).unwrap();
    assert_eq!(validate(&lazy, &cert), validate(&eager, &cert));
}

/// Issuing on an unsettled required entry fails: it reads
/// `PENDING(StructuralUnknown)`.
#[test]
fn issuing_on_an_unsettled_entry_is_not_certified() {
    let (mut lazy, mut eager) = pair(hub);
    commit_both(&mut lazy, &mut eager, vec![retract(0)]);
    assert!(!lazy.settled(RevisionId(8)));
    let entries = [item(&lazy, 8)];
    assert_eq!(
        lazy.issue_certificate(&request(false), &entries, &BTreeMap::new()),
        Err(IssueFailure::NotCertified {
            logical: LogicalId(8),
            rev: RevisionId(8),
            summary: Summary::Pending(PendingReason::StructuralUnknown),
        })
    );
}

/// Certificates issued before a lazy commit, validated during and after
/// it: an entry the cascade has not reached is `Pending`, an untouched one
/// stays `Valid`, and after the drain both equal eager (the alternative
/// support recertifies).
#[test]
fn certificates_issued_before_a_frontier_validate_during_and_after_it() {
    let (mut lazy, mut eager) = pair(|| {
        let mut batches = hub();
        batches.push(vec![create("u", 9, true)]);
        batches
    });
    let (touched, untouched) = (issue(&lazy, false, &[8]), issue(&lazy, false, &[9]));
    commit_both(
        &mut lazy,
        &mut eager,
        vec![
            retract(0),
            create("y", 10, true),
            pin(RevisionId(8).into(), RevRef::New(0)),
        ],
    );
    assert!(!lazy.settled(RevisionId(8)) && lazy.settled(RevisionId(9)));
    assert_eq!(
        validate(&lazy, &touched),
        Validation::Pending { rules: vec![] }
    );
    assert_eq!(validate(&lazy, &untouched), Validation::Valid);
    lazy.drain(Drain::Full).unwrap();
    for cert in [&touched, &untouched] {
        assert_eq!(validate(&lazy, cert), validate(&eager, cert));
    }
    assert!(matches!(
        validate(&lazy, &touched),
        Validation::ValidRecertified { .. }
    ));
    assert_eq!(validate(&lazy, &untouched), Validation::Valid);
}

// ---- The companion property (ADR 0019 §9 G3) ----

/// SPEC §6.4 per-entry table, restated from the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Denied,
    Invalid,
    Conflicted,
    Unknown,
    Pending,
    Ok,
}
fn outcome(summary: &Summary, is_head: bool) -> Outcome {
    match summary {
        Summary::Unavailable => Outcome::Denied,
        _ if !is_head => Outcome::Invalid,
        Summary::Stale(_) => Outcome::Invalid,
        Summary::Conflicted => Outcome::Conflicted,
        Summary::Pending(PendingReason::ObservationLag | PendingReason::ReplicationLag) => {
            Outcome::Unknown
        }
        Summary::Pending(_) => Outcome::Pending,
        Summary::Certified => Outcome::Ok,
    }
}
/// Variant and reason: what rule 2 holds exact.
fn class(v: &Validation) -> (std::mem::Discriminant<Validation>, Option<InvalidReason>) {
    let reason = match v {
        Validation::Invalid { reason, .. } => Some(*reason),
        _ => None,
    };
    (std::mem::discriminant(v), reason)
}
/// Eager's verdict comes only from inputs (ADR 0019 §3.6 rule 2).
fn input_level(v: &Validation, required: &[(&CertItem, RevisionId)]) -> bool {
    match v {
        Validation::Denied | Validation::Conflicted { .. } => true,
        Validation::Invalid {
            reason: InvalidReason::Changed,
            ..
        } => required.iter().any(|(e, head)| e.rev != *head),
        Validation::Invalid { .. } => true,
        _ => false,
    }
}
/// Rule 3's expected verdict: the §6.4 priority over eager's per-entry
/// results for settled entries and the input-level result (or `Pending`)
/// for unsettled ones, with lag over the settled heads only.
fn aggregate(
    lazy: &Db,
    eager: &Db,
    required: &[(&CertItem, RevisionId)],
    live: &BTreeMap<String, Vec<u8>>,
) -> Validation {
    let inputs = eager.inputs();
    let derived = contextdb_model::DerivedView::derived(eager);
    let (mut invalid, mut conflicted, mut rules) = (Vec::new(), Vec::new(), BTreeSet::new());
    let mut settled_heads = Vec::new();
    let mut pending = false;
    for (e, head) in required {
        let (o, summary) = if lazy.settled(*head) {
            settled_heads.push(*head);
            let summary = eager.summary_at(*head, live).unwrap();
            (outcome(&summary, e.rev == *head), summary)
        } else {
            let node = eager.node(*head).unwrap();
            let o = if e.rev != *head {
                Outcome::Invalid
            } else if node.dimensions.conflicted {
                Outcome::Conflicted
            } else {
                Outcome::Pending
            };
            (o, Summary::Pending(PendingReason::StructuralUnknown))
        };
        match o {
            Outcome::Denied => return Validation::Denied,
            Outcome::Invalid => invalid.push(CertDiff {
                logical: e.logical,
                cert_rev: Some(e.rev),
                current: Some(*head),
                summary: Some(summary),
            }),
            Outcome::Conflicted => conflicted.push(e.logical),
            Outcome::Unknown | Outcome::Ok => {}
            Outcome::Pending => {
                pending = true;
                if lazy.settled(*head) {
                    rules.extend((inputs.rules.iter().enumerate()).filter_map(|(j, rule)| {
                        (rule.conclusion == *head && derived.sem[j] == SemState::Pending)
                            .then_some(RuleId(j as u32))
                    }));
                }
            }
        }
    }
    if !invalid.is_empty() {
        return Validation::Invalid {
            reason: InvalidReason::Changed,
            diffs: invalid,
        };
    }
    if !conflicted.is_empty() {
        return Validation::Conflicted {
            entries: conflicted,
        };
    }
    let lag: Vec<String> = (eager.observation_scopes(&settled_heads).unwrap())
        .into_iter()
        .filter(|s| live.get(s).is_none() || inputs.watermarks.get(s) != live.get(s))
        .collect();
    if !lag.is_empty() {
        return Validation::Unknown { lag };
    }
    assert!(pending, "an unsettled entry is at least Pending");
    Validation::Pending {
        rules: rules.into_iter().collect(),
    }
}

/// [rule 1, rule 2, rule 3, validations with D unsettled, certificates
/// issued, slices].
type Counts = [u64; 6];

/// Validates `cert` on both databases and checks the contract.
fn check(
    lazy: &Db,
    eager: &Db,
    cert: &Certificate,
    live: &BTreeMap<String, Vec<u8>>,
    link: LogLink,
    counts: &mut Counts,
) {
    let got = lazy.validate_certificate(cert, live, link).unwrap();
    let want = eager.validate_certificate(cert, live, link).unwrap();
    let required: Vec<(&CertItem, RevisionId)> = (cert.entries.iter())
        .filter(|e| e.required)
        .map(|e| (e, eager.head(e.logical).unwrap()))
        .collect();
    // D: the required heads (settled implies their closure is) and every
    // head the eager verdict's diffs report.
    let diffs = match &want {
        Validation::Invalid { diffs, .. } | Validation::ValidRecertified { diffs, .. } => {
            diffs.as_slice()
        }
        _ => &[],
    };
    let d_settled = (required.iter().map(|(_, head)| *head))
        .chain(diffs.iter().filter_map(|d| d.current))
        .all(|r| lazy.settled(r));
    if d_settled {
        assert_eq!(got, want, "rule 1: settled ⇒ equal");
        counts[0] += 1;
        return;
    }
    counts[3] += 1;
    assert!(
        !matches!(got, Validation::Valid | Validation::ValidRecertified { .. }),
        "positive while D is unsettled: {got:?}"
    );
    if input_level(&want, &required) {
        assert_eq!(class(&got), class(&want), "rule 2: {got:?} vs {want:?}");
        counts[1] += 1;
        return;
    }
    assert!(
        required.iter().any(|(_, head)| !lazy.settled(*head)),
        "only a strict diff reports a head outside the required heads"
    );
    assert_eq!(
        got,
        aggregate(lazy, eager, &required, live),
        "rule 3, eager {want:?}"
    );
    counts[2] += 1;
}

/// Observer batches: a CodeAnchor, new content and head moves, watermark
/// advances (as PROP11 does), so lag and watermark strict changes occur.
fn observer_batch(rng: &mut ChaCha8Rng, db: &Db) -> Batch {
    let inputs = db.inputs();
    let anchor = (inputs.logicals.iter()).position(|l| l.key == "code:repo/a.rs");
    let ops = match (anchor, rng.next_u32() % 3) {
        (None, _) => vec![Op::CreateLogical {
            origin: Default::default(),
            key: "code:repo/a.rs".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: SCOPE.into(),
            content: [200; 32],
            adopt_as_base: true,
        }],
        (Some(l), 0) => {
            let l = LogicalId(l as u32);
            vec![
                Op::AddRevision {
                    logical: l.into(),
                    content: [200 + (rng.next_u32() % 3) as u8; 32],
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: l.into(),
                    expected: inputs.heads[l.index()].into(),
                    new: RevRef::New(0),
                },
            ]
        }
        (Some(_), _) => vec![Op::ObserverWatermark {
            gap: false,
            scope: SCOPE.into(),
            expected: inputs.watermarks.get(SCOPE).cloned(),
            position: vec![(rng.next_u32() % 4) as u8],
        }],
    };
    let mut b = Batch::new(ops);
    b.channel = Channel::Observer;
    b
}
/// A rule from a head to the anchor (Cur or Sem), so anchors enter closures.
fn anchor_rule(rng: &mut ChaCha8Rng, db: &Db) -> Option<Batch> {
    let inputs = db.inputs();
    let a = (inputs.logicals.iter()).position(|l| l.key == "code:repo/a.rs")?;
    let targets: Vec<usize> = (inputs.logicals.iter().enumerate())
        .filter(|(_, l)| !matches!(l.kind, RecordKind::CodeAnchor | RecordKind::Evidence))
        .map(|(n, _)| n)
        .collect();
    let t = *targets.get(rng.next_u32() as usize % targets.len().max(1))?;
    let kind = if rng.next_u32().is_multiple_of(2) {
        EdgeKind::Cur
    } else {
        EdgeKind::Sem
    };
    Some(Batch::new(vec![Op::AddRule {
        conclusion: inputs.heads[t].into(),
        incidences: vec![(inputs.heads[a].into(), kind)],
        enabled: true,
    }]))
}
/// A P1 batch, with a Conflict opened or resolved now and then (R3).
fn agent_batch(rng: &mut ChaCha8Rng, db: &Db, mut b: Batch) -> Batch {
    let inputs = db.inputs();
    let n = inputs.revisions.len() as u32;
    if n > 0 && rng.next_u32().is_multiple_of(3) {
        b.ops.push(conflict(rng.next_u32() % n));
    }
    let open: Vec<_> = (inputs.artifacts.iter().enumerate())
        .filter(|(_, a)| a.open)
        .map(|(n, _)| ArtifactId(n as u32))
        .collect();
    if !open.is_empty() && rng.next_u32().is_multiple_of(3) {
        b.ops.push(Op::ResolveArtifact {
            id: open[rng.next_u32() as usize % open.len()].into(),
            note: None,
        });
    }
    b
}
fn live_pos(rng: &mut ChaCha8Rng, db: &Db) -> BTreeMap<String, Vec<u8>> {
    match rng.next_u32() % 3 {
        0 => BTreeMap::new(),
        1 => (db.inputs().watermarks.get(SCOPE))
            .map(|p| BTreeMap::from([(SCOPE.to_owned(), p.clone())]))
            .unwrap_or_default(),
        _ => BTreeMap::from([(SCOPE.to_owned(), vec![9])]),
    }
}
/// Up to three current heads, each required or not, strict or not.
fn issue_random(rng: &mut ChaCha8Rng, lazy: &Db, eager: &Db) -> Option<Certificate> {
    let inputs = lazy.inputs();
    if inputs.logicals.is_empty() {
        return None;
    }
    let now = lazy.epoch();
    let entries: Vec<CertItem> = (0..1 + rng.next_u32() % 3)
        .map(|_| {
            let l = LogicalId(rng.next_u32() % inputs.logicals.len() as u32);
            CertItem {
                logical: l,
                rev: inputs.heads[l.index()],
                required: !rng.next_u32().is_multiple_of(5),
                via: Via::Head,
                since_epoch: Epoch(rng.next_u64() % (now.0 + 1)),
            }
        })
        .collect();
    let request = request(rng.next_u32().is_multiple_of(2));
    let live = live_pos(rng, lazy);
    let got = lazy.issue_certificate(&request, &entries, &live);
    assert_eq!(got, eager.issue_certificate(&request, &entries, &live));
    got.ok()
}

const BUDGETS: [u64; 4] = [1, 2, 3, 8];
const CHUNKS: [usize; 3] = [1, 2, 4096];

#[test]
fn the_validate_contract_holds_across_frontiers() {
    let seeds = std::env::var("P11_VALIDATE_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(SEEDS);
    let mut counts: Counts = [0; 6];
    for seed in 0..seeds {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x0019_3600);
        let mut lazy = Db::default();
        let mut eager = Db::default();
        let mut certs: Vec<Certificate> = Vec::new();
        let mut batches = contextdb_testkit::generate_p1(seed).batches.into_iter();
        for step in 0..40 {
            if rng.next_u32().is_multiple_of(2)
                && let Some(cert) = issue_random(&mut rng, &lazy, &eager)
            {
                counts[4] += 1;
                certs.push(cert);
            }
            let batch = match rng.next_u32() % 5 {
                0 => observer_batch(&mut rng, &eager),
                1 => match anchor_rule(&mut rng, &eager) {
                    Some(b) => b,
                    None => continue,
                },
                _ => match batches.next() {
                    Some(b) => agent_batch(&mut rng, &eager, b),
                    None => break,
                },
            };
            let budget = Budget {
                units: BUDGETS[rng.next_u32() as usize % BUDGETS.len()],
                chunk: CHUNKS[rng.next_u32() as usize % CHUNKS.len()],
            };
            let want = eager.commit(&batch);
            let got = lazy.commit_lazy(&batch, budget);
            assert_eq!(got.is_ok(), want.is_ok(), "seed {seed} step {step}");
            loop {
                counts[5] += 1;
                for cert in &certs {
                    let live = live_pos(&mut rng, &eager);
                    let link = LogLink {
                        matches: !rng.next_u32().is_multiple_of(20),
                        current_checksum: [eager.epoch().0 as u8; 32],
                    };
                    let run = std::panic::AssertUnwindSafe(|| {
                        check(&lazy, &eager, cert, &live, link, &mut counts)
                    });
                    if let Err(failure) = std::panic::catch_unwind(run) {
                        eprintln!("P11 validate: seed {seed} step {step}");
                        std::panic::resume_unwind(failure);
                    }
                }
                if !lazy.has_frontier() {
                    break;
                }
                lazy.drain(Drain::Units(BUDGETS[rng.next_u32() as usize % 4]));
            }
            assert_eq!(lazy.digest(), eager.digest(), "seed {seed} step {step}");
        }
    }
    eprintln!("P11 validate [rule1, rule2, rule3, D unsettled, issued, slices]={counts:?}");
    for (n, name) in [(0, "rule 1"), (1, "rule 2"), (2, "rule 3")] {
        assert!(counts[n] > 0, "{name} never exercised: {counts:?}");
    }
}
