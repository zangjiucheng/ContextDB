//! P4 G3 (ADR 0010 §11) at 10,000 seeds (override with
//! CONTEXTDB_P4_VIEW_SEEDS), against the in-process service:
//! (d) `Updates` peek/ack per agent reports exactly the reads in the agent's
//!     view whose head moved since they were last acknowledged (dropped
//!     outputs are re-sent; acknowledged ones are not);
//! (e) each agent's `Check` verdict equals an oracle over the ADR 0010 §7
//!     view (own entry, else the root's, else the newest other read), so a
//!     child's newer read never changes the root's verdict for a logical the
//!     root read itself.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::*;
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
const AGENTS: [Option<&str>; 3] = [None, Some("c1"), Some("c2")];
fn seeds() -> u64 {
    std::env::var("CONTEXTDB_P4_VIEW_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}
fn service() -> Service<SimDisk> {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn ok(r: Response) -> Response {
    if let Response::Error { code, detail } = &r {
        panic!("{code}: {detail}");
    }
    r
}
/// The last peek per agent: token, then (logical, head shown).
type Peek = (u64, Vec<(u32, Option<u32>)>);
/// The oracle's model of one session.
#[derive(Default)]
struct Model {
    /// (agent, logical) → (rev read, read order).
    reads: BTreeMap<(Option<String>, u32), (u32, u64)>,
    /// (agent, logical) → head last acknowledged.
    reported: BTreeMap<(Option<String>, u32), Option<u32>>,
}
impl Model {
    fn view(&self, agent: &Option<String>) -> BTreeMap<u32, u32> {
        let mut v = BTreeMap::new();
        let mut newest: BTreeMap<u32, (u64, u32)> = BTreeMap::new();
        for ((a, l), (rev, order)) in &self.reads {
            if a == agent {
                v.insert(*l, *rev);
            } else if a.is_some() {
                let e = newest.entry(*l).or_insert((*order, *rev));
                if *order > e.0 {
                    *e = (*order, *rev);
                }
            }
        }
        for ((a, l), (rev, _)) in &self.reads {
            if a.is_none() {
                v.entry(*l).or_insert(*rev);
            }
        }
        for (l, (_, rev)) in newest {
            v.entry(l).or_insert(rev);
        }
        v
    }
}
#[test]
fn d_e_updates_and_agent_views_equal_the_oracle() {
    // Coverage: verdicts seen, non-empty updates, and checks where a child's
    // newer read of a logical the root also read was in play.
    let (mut verdicts, mut nonempty, mut shadowed) = (BTreeMap::<&str, u64>::new(), 0u64, 0u64);
    for seed in 0..seeds() {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xD0);
        let s = service();
        let attach = |key: &str| match ok(s
            .handle(
                AGENT,
                Request::SessionAttach {
                    harness: "p".into(),
                    harness_session: key.into(),
                    repo: String::new(),
                    head_sha: String::new(),
                },
            )
            .response)
        {
            Response::Attached { session, .. } => session,
            other => panic!("{other:?}"),
        };
        let (writer, session) = (attach("w"), attach("s"));
        let n = 2 + rng.next_u32() % 3;
        let mut heads: Vec<u32> = Vec::new();
        for k in 0..n {
            let r = ok(s
                .handle(
                    AGENT,
                    Request::Record {
                        binding: Binding::root(writer),
                        kind: RecordKind::Fact,
                        body: format!("k{k} v0").into_bytes(),
                        target: WriteTarget::Create {
                            key: format!("k{k}"),
                            scope_lane: "repo".into(),
                        },
                        deps: Vec::new(),
                        adopt_as_base: true,
                        client_txn_id: None,
                    },
                )
                .response);
            let Response::Committed { result } = r else {
                panic!()
            };
            heads.push(result.allocations.new_revisions[0].0);
        }
        let mut model = Model::default();
        let mut order = 0u64;
        let mut version = vec![0u32; n as usize];
        // Up to eight kept peeks per agent, as the daemon keeps them.
        let mut pending: BTreeMap<Option<String>, Vec<Peek>> = BTreeMap::new();
        let b = |agent: &Option<String>| Binding::Session {
            session,
            agent: agent.clone(),
        };
        for _ in 0..(3 + rng.next_u32() % 12) {
            let agent = AGENTS[rng.next_u32() as usize % 3].map(String::from);
            let l = rng.next_u32() % n;
            match rng.next_u32() % 4 {
                0 => {
                    // A certified read by `agent`.
                    ok(s.handle(
                        AGENT,
                        Request::Read {
                            binding: b(&agent),
                            target: Target::Key(format!("k{l}")),
                            mode: Mode::Certified,
                        },
                    )
                    .response);
                    order += 1;
                    let key = (agent.clone(), l);
                    if model.reads.get(&key).map(|r| r.0) != Some(heads[l as usize]) {
                        // A re-read restarts what this agent was told.
                        model.reported.remove(&key);
                    }
                    model.reads.insert(key, (heads[l as usize], order));
                }
                1 => {
                    // The writer moves the head.
                    version[l as usize] += 1;
                    let r = ok(s
                        .handle(
                            AGENT,
                            Request::Supersede {
                                binding: Binding::root(writer),
                                logical: LogicalId(l),
                                body: format!("k{l} v{}", version[l as usize]).into_bytes(),
                                expected_head: RevisionId(heads[l as usize]),
                                adopt_as_base: true,
                                carry_rules: false,
                                reason: String::new(),
                                client_txn_id: None,
                            },
                        )
                        .response);
                    let Response::Committed { result } = r else {
                        panic!()
                    };
                    heads[l as usize] = result.allocations.new_revisions[0].0;
                }
                2 => {
                    // Peek, acknowledging one kept batch half the time (not
                    // necessarily the latest: another peek may have come in
                    // between), else its output was dropped.
                    let kept = pending.entry(agent.clone()).or_default();
                    let pick = (rng.next_u32() % 2 == 0 && !kept.is_empty())
                        .then(|| rng.next_u32() as usize % kept.len());
                    let ack = pick.map(|k| {
                        let (t, items) = kept.remove(k);
                        for (l, cur) in items {
                            model.reported.insert((agent.clone(), l), cur);
                        }
                        t
                    });
                    let Response::Updates { token, items, .. } = ok(s
                        .handle(
                            AGENT,
                            Request::Updates {
                                binding: b(&agent),
                                ack,
                                max_items: 100,
                            },
                        )
                        .response)
                    else {
                        panic!()
                    };
                    let got: BTreeSet<u32> = items.iter().map(|i| i.logical.0).collect();
                    let want: BTreeSet<u32> = model
                        .view(&agent)
                        .into_iter()
                        .filter(|(l, rev)| {
                            let last = model
                                .reported
                                .get(&(agent.clone(), *l))
                                .copied()
                                .unwrap_or(Some(*rev));
                            last != Some(heads[*l as usize])
                        })
                        .map(|(l, _)| l)
                        .collect();
                    assert_eq!(got, want, "seed {seed}: updates for {agent:?}");
                    nonempty += u64::from(!got.is_empty());
                    let kept = pending.entry(agent.clone()).or_default();
                    kept.push((
                        token,
                        items
                            .iter()
                            .map(|i| (i.logical.0, i.current.map(|r| r.0)))
                            .collect(),
                    ));
                    if kept.len() > 8 {
                        kept.remove(0);
                    }
                }
                _ => {
                    let Response::Check { validation, .. } = ok(s
                        .handle(
                            AGENT,
                            Request::Check {
                                binding: b(&agent),
                                cert_id: None,
                                live_pos: Vec::new(),
                                action: None,
                                mode: None,
                                harness_version: String::new(),
                                strict: false,
                            },
                        )
                        .response)
                    else {
                        panic!()
                    };
                    let view = model.view(&agent);
                    let want = if view.is_empty() {
                        "Empty"
                    } else if view.iter().all(|(l, rev)| *rev == heads[*l as usize]) {
                        "Valid"
                    } else {
                        "Invalid"
                    };
                    let got = match validation {
                        Validation::Empty => "Empty",
                        Validation::Valid | Validation::ValidRecertified { .. } => "Valid",
                        Validation::Invalid { .. } => "Invalid",
                        other => panic!("seed {seed}: {other:?}"),
                    };
                    assert_eq!(got, want, "seed {seed}: check for {agent:?}, view {view:?}");
                    *verdicts.entry(got).or_default() += 1;
                    if agent.is_none()
                        && model.reads.iter().any(|((a, l), (r, _))| {
                            a.is_some()
                                && model
                                    .reads
                                    .get(&(None, *l))
                                    .is_some_and(|(root, _)| root != r)
                        })
                    {
                        shadowed += 1;
                    }
                }
            }
        }
    }
    println!(
        "verdicts {verdicts:?}, non-empty updates {nonempty}, root checks with a differing child read {shadowed}"
    );
    if seeds() >= 1_000 {
        for v in ["Empty", "Valid", "Invalid"] {
            assert!(
                verdicts.get(v).copied().unwrap_or(0) > seeds() / 50,
                "coverage: {v}"
            );
        }
        assert!(
            nonempty > seeds() / 20 && shadowed > seeds() / 100,
            "coverage"
        );
    }
}
