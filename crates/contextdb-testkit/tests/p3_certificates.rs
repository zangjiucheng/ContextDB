//! PROP11 (SPEC §8.2, ADR 0008 §6.5): certificate issuance and validation by
//! the engine equal the oracle's independent recomputation, compared in full
//! (entries, vector, watermarks, failure reasons, verdicts, diffs and the
//! recertified certificate), over random histories, random issue epochs,
//! random later batches, random `live_pos` and log linkage.
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_oracle::{CertParams, cert_issue_oracle, cert_validate_oracle};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::BTreeMap;

const SCOPE: &str = "repo";

fn zero_id(mut c: Certificate) -> Certificate {
    c.cert_id = [0; 32];
    c
}
fn normalize(v: Validation) -> Validation {
    match v {
        Validation::ValidRecertified { new_cert, diffs } => Validation::ValidRecertified {
            new_cert: Box::new(zero_id(*new_cert)),
            diffs,
        },
        other => other,
    }
}
fn kind(v: &Validation) -> usize {
    match v {
        Validation::Denied => 0,
        Validation::Invalid {
            reason: InvalidReason::Changed,
            ..
        } => 1,
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        } => 2,
        Validation::Invalid {
            reason: InvalidReason::LogMismatch,
            ..
        } => 3,
        Validation::Invalid {
            reason: InvalidReason::BranchMismatch,
            ..
        } => unreachable!("single-branch certificates"),
        Validation::Conflicted { .. } => 4,
        Validation::Ambiguous { .. } => 5,
        Validation::Unknown { .. } => 6,
        Validation::Pending { .. } => 7,
        Validation::Empty => 8,
        Validation::Valid => 9,
        Validation::ValidRecertified { .. } => 10,
    }
}
/// Observer batches: a CodeAnchor (create, then new content and head moves)
/// and watermark advances, so lag and watermark strict changes occur.
fn observer_batch(rng: &mut ChaCha8Rng, db: &Db) -> Batch {
    let inputs = db.inputs();
    let anchor = inputs
        .logicals
        .iter()
        .position(|l| l.key == "code:repo/a.rs");
    let ops = match (anchor, rng.next_u32() % 3) {
        (None, _) => vec![Op::CreateLogical {
            key: "code:repo/a.rs".into(),
            kind: RecordKind::CodeAnchor,
            scope_lane: SCOPE.into(),
            content: [200; 32],
            adopt_as_base: true,
        }],
        (Some(l), 0) => {
            let l = LogicalId(l as u32);
            let head = inputs.heads[l.index()];
            vec![
                Op::AddRevision {
                    logical: l.into(),
                    content: [200 + (rng.next_u32() % 3) as u8; 32],
                    adopt_as_base: true,
                },
                Op::SetHead {
                    logical: l.into(),
                    expected: head.into(),
                    new: RevRef::New(0),
                },
            ]
        }
        (Some(_), _) => vec![Op::ObserverWatermark {
            scope: SCOPE.into(),
            expected: inputs.watermarks.get(SCOPE).cloned(),
            position: vec![(rng.next_u32() % 4) as u8],
        }],
    };
    let mut b = Batch::new(ops);
    b.channel = Channel::Observer;
    b
}
/// Agent batch linking a head to the anchor (Cur or Sem), so anchors enter closures.
fn anchor_rule(rng: &mut ChaCha8Rng, db: &Db) -> Option<Batch> {
    let inputs = db.inputs();
    let a = inputs
        .logicals
        .iter()
        .position(|l| l.key == "code:repo/a.rs")?;
    let targets: Vec<usize> = inputs
        .logicals
        .iter()
        .enumerate()
        .filter(|(_, l)| !matches!(l.kind, RecordKind::CodeAnchor | RecordKind::Evidence))
        .map(|(n, _)| n)
        .collect();
    if targets.is_empty() {
        return None;
    }
    let t = targets[rng.next_u32() as usize % targets.len()];
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
fn live_pos(rng: &mut ChaCha8Rng, db: &Db) -> BTreeMap<String, Vec<u8>> {
    match rng.next_u32() % 3 {
        0 => BTreeMap::new(),
        1 => db
            .inputs()
            .watermarks
            .get(SCOPE)
            .map(|p| BTreeMap::from([(SCOPE.to_owned(), p.clone())]))
            .unwrap_or_default(),
        _ => BTreeMap::from([(SCOPE.to_owned(), vec![9])]),
    }
}
#[test]
fn prop11_certificates_equal_oracle_10000_seeds() {
    let seeds: u64 = std::env::var("CONTEXTDB_PROP11_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);
    let mut verdicts = [0u64; 11];
    let mut issue = [0u64; 4]; // ok, not-certified, ambiguous, empty
    for seed in 0..seeds {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x11);
        let trace = contextdb_testkit::generate_p1(seed);
        let mut db = Db::default();
        let mut history = vec![db.inputs()];
        let mut certs: Vec<Certificate> = Vec::new();
        let mut batches = trace.batches.into_iter();
        let mut steps = 0;
        loop {
            steps += 1;
            let batch = match rng.next_u32() % 5 {
                0 => observer_batch(&mut rng, &db),
                1 => match anchor_rule(&mut rng, &db) {
                    Some(b) => b,
                    None => continue,
                },
                _ => match batches.next() {
                    Some(b) => b,
                    None => break,
                },
            };
            if db.commit(&batch).is_ok() {
                history.push(db.inputs());
            }
            assert_eq!(history.len() as u64 - 1, db.epoch().0);
            if steps > 60 {
                break;
            }
            let inputs = db.inputs();
            // Issue a certificate for 1–3 random reads.
            if rng.next_u32().is_multiple_of(3) && !inputs.logicals.is_empty() {
                let now = db.epoch();
                let entries: Vec<CertItem> = (0..1 + rng.next_u32() % 3)
                    .map(|_| {
                        let l = LogicalId(rng.next_u32() % inputs.logicals.len() as u32);
                        let revs: Vec<_> = inputs
                            .revisions
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| r.logical == l)
                            .map(|(n, _)| RevisionId(n as u32))
                            .collect();
                        let rev = if rng.next_u32().is_multiple_of(4) {
                            revs[rng.next_u32() as usize % revs.len()]
                        } else {
                            inputs.heads[l.index()]
                        };
                        CertItem {
                            logical: l,
                            rev,
                            required: !rng.next_u32().is_multiple_of(5),
                            via: Via::Head,
                            since_epoch: Epoch(rng.next_u64() % (now.0 + 1)),
                        }
                    })
                    .collect();
                let request = IssueRequest {
                    session_id: [seed as u8; 16],
                    principal: PrincipalId(1),
                    branch: MAIN,
                    log_checksum: [now.0 as u8; 32],
                    code_commit: "abc".into(),
                    strict_no_aba: rng.next_u32().is_multiple_of(2),
                };
                let params = CertParams {
                    session_id: request.session_id,
                    principal: request.principal,
                    branch: request.branch,
                    log_checksum: request.log_checksum,
                    code_commit: request.code_commit.clone(),
                    strict_no_aba: request.strict_no_aba,
                };
                let live = live_pos(&mut rng, &db);
                let engine = db.issue_certificate(&request, &entries, &live);
                let oracle = cert_issue_oracle(&history, &params, &entries, &live);
                assert_eq!(
                    engine.clone().map(zero_id),
                    oracle,
                    "seed {seed} step {steps}: issuance"
                );
                issue[match &engine {
                    Ok(_) => 0,
                    Err(IssueFailure::NotCertified { .. }) => 1,
                    Err(IssueFailure::Ambiguous { .. }) => 2,
                    Err(IssueFailure::Empty) => 3,
                }] += 1;
                if let Ok(c) = engine {
                    assert_eq!(c.cert_id, *blake3::hash(&c.body_bytes()).as_bytes());
                    certs.push(c);
                }
            }
            // Validate every live certificate against the current state.
            for c in &certs {
                let live = live_pos(&mut rng, &db);
                let matches = !rng.next_u32().is_multiple_of(20);
                let current = [db.epoch().0 as u8; 32];
                let engine = db
                    .validate_certificate(
                        c,
                        &live,
                        LogLink {
                            matches,
                            current_checksum: current,
                        },
                    )
                    .expect("history from genesis");
                let oracle = cert_validate_oracle(&history, c, &live, matches, current);
                verdicts[kind(&engine)] += 1;
                if let Validation::ValidRecertified { new_cert, .. } = &engine {
                    assert_eq!(
                        new_cert.cert_id,
                        *blake3::hash(&new_cert.body_bytes()).as_bytes()
                    );
                }
                assert_eq!(
                    normalize(engine),
                    oracle,
                    "seed {seed} step {steps}: validation of cert issued at {:?}",
                    c.issued_epoch
                );
            }
        }
    }
    eprintln!(
        "PROP11 issue [ok,not-certified,ambiguous,empty]={issue:?} verdicts \
         [denied,invalid,strict,log,conflicted,ambiguous,unknown,pending,empty,valid,recertified]={verdicts:?}"
    );
    // Reachable outcomes before P7 (no conflicts) and P9 (no denial) must all occur.
    for (n, name) in [
        (1, "invalid"),
        (2, "strict"),
        (3, "log"),
        (6, "unknown"),
        (7, "pending"),
        (9, "valid"),
        (10, "recertified"),
    ] {
        assert!(
            verdicts[n] > 0,
            "PROP11 never produced {name}: {verdicts:?}"
        );
    }
    for (n, name) in [(0, "ok"), (1, "not certified"), (3, "empty")] {
        assert!(issue[n] > 0, "PROP11 issuance never {name}: {issue:?}");
    }
}
