//! P8 G3, PROP14 (ADR 0016 §10): retrieval against the oracle.
//!
//! Each seed takes a `generate_p7` history (rules, validations, Conflicts,
//! PendingReviews, revisions), commits its first half on `main` and the
//! rest on a fork `dev`,
//! and gives every content hash a text body (an injective rewrite, so
//! revision dedup is unchanged). Random queries, modes, limits, kind
//! filters, budgets and levels then check:
//!
//! - every certified item is CERTIFIED, and every advisory label equals
//!   the reference summary: on `main` the oracle's (an `InputModel` fed the
//!   batches `main` committed, then Jacobi), on `dev` the kernel's (whose
//!   per-branch labels PROP13 checks against the branch oracle);
//! - items are current heads, each logical once, and exactly
//!   `min(limit, |eligible matches|)` of them, all eligible matches: a page
//!   is short only when the eligible set is exhausted;
//! - withheld entries are ineligible matches, at most `limit`;
//! - a budget keeps labels and order (a prefix of the unbudgeted page);
//! - the result, as a set with statuses, is the same after `ctx reindex`;
//! - metamorphic: lane `""` and `"repo"` agree (every record is in
//!   `repo`), and adding an unrelated record never removes a certified
//!   exact-key match.
use contextdb_daemon::{
    actor,
    proto::*,
    service::{Identity, Service},
};
use contextdb_model::levels::Level;
use contextdb_model::*;
use contextdb_oracle::{InputModel, jacobi, summarize};
use contextdb_store::{OpenOptions, SimDisk, Store, session::SessionLog};
use contextdb_testkit::generate_p7;
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};
/// Words no key, kind or lane contains.
const VOCAB: [&str; 12] = [
    "otter", "quartz", "maple", "ember", "falcon", "harbor", "lantern", "meadow", "nickel",
    "orchid", "pepper", "ridge",
];
type Svc = Service<SimDisk>;

fn seeds() -> u64 {
    std::env::var("CONTEXTDB_PROP14_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150)
}
/// The body a content hash stands for: three vocabulary words and a unique
/// token, so distinct hashes get distinct bodies.
fn body_of(h: &ContentHash) -> String {
    let w = |i: usize| VOCAB[h[i] as usize % VOCAB.len()];
    let tag: String = h[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("# note\n\n{} {} {} c{tag}\n", w(0), w(1), w(2))
}
fn rewrite(h: &ContentHash, blobs: &mut BTreeMap<ContentHash, Vec<u8>>) -> ContentHash {
    let body = body_of(h).into_bytes();
    let new = *blake3::hash(&body).as_bytes();
    blobs.insert(new, body);
    new
}
/// The batch with every content hash rewritten, and the blobs it needs.
fn rewritten(b: &Batch) -> (Batch, Vec<Vec<u8>>) {
    let mut blobs = BTreeMap::new();
    let mut out = b.clone();
    for op in &mut out.ops {
        match op {
            Op::CreateLogical { content, .. } | Op::AddRevision { content, .. } => {
                *content = rewrite(content, &mut blobs)
            }
            Op::RecordValidation { meta: Some(h), .. } => *h = rewrite(h, &mut blobs),
            Op::OpenArtifact { payload_hash, .. } => {
                *payload_hash = rewrite(payload_hash, &mut blobs)
            }
            Op::ResolveArtifact { note: Some(h), .. } => *h = rewrite(h, &mut blobs),
            _ => {}
        }
    }
    (out, blobs.into_values().collect())
}
fn service() -> Svc {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let (store, _) = Store::open(disk.clone(), dir, &OpenOptions::default()).unwrap();
    let (log, events, _) = SessionLog::open(disk.clone(), dir, false, 1 << 20).unwrap();
    Service::new(actor::spawn(store).unwrap(), log, events).unwrap()
}
fn attach(s: &Svc, key: &str, branch: Option<&str>) -> Binding {
    match (s.handle(
        AGENT,
        Request::SessionAttach {
            harness: "claude".into(),
            harness_session: key.into(),
            repo: "/repo".into(),
            head_sha: "h".into(),
            branch: branch.map(str::to_owned),
            git_branch: None,
        },
    ))
    .response
    {
        Response::Attached { session, .. } => Binding::root(session),
        other => panic!("{other:?}"),
    }
}
#[derive(Clone, Debug)]
struct Page {
    items: Vec<Item>,
    withheld: Vec<Item>,
}
#[allow(clippy::too_many_arguments)]
fn recall(
    s: &Svc,
    b: &Binding,
    query: &str,
    lane: &str,
    mode: Mode,
    limit: u32,
    opts: RecallOpts,
) -> Page {
    for _ in 0..200 {
        match (s.handle(
            AGENT,
            Request::Recall {
                binding: b.clone(),
                query: query.into(),
                scope: lane.into(),
                mode,
                limit,
                index_wait_ms: 2_000,
                opts: opts.clone(),
            },
        ))
        .response
        {
            Response::Recall {
                items,
                withheld,
                index,
                ..
            } => {
                if matches!(index.fts, IndexState::Ready { .. }) {
                    return Page { items, withheld };
                }
            }
            other => panic!("{other:?}"),
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the index never became ready")
}
/// What the reference says about one live head.
#[derive(Clone, Debug)]
struct Head {
    rev: RevisionId,
    kind: RecordKind,
    summary: Summary,
    tokens: BTreeSet<String>,
    key: String,
}
fn tokens(text: &str) -> BTreeSet<String> {
    contextdb_store::recall::terms(text).into_iter().collect()
}
/// The live heads of `branch` with their reference labels.
fn heads(s: &Svc, branch: BranchId, oracle: Option<&InputModel>) -> BTreeMap<String, Head> {
    let snap = s.actor.snapshot();
    let view = snap.kernel.branch(branch).unwrap();
    let mut by_key: BTreeMap<String, Summary> = BTreeMap::new();
    if let Some(o) = oracle {
        let i = o.inputs();
        let d = jacobi(i);
        for (n, lg) in i.logicals.iter().enumerate() {
            let r = i.heads[n];
            by_key.insert(lg.key.clone(), summarize(&d.nodes[r.0 as usize].dimensions));
        }
    }
    let mut out = BTreeMap::new();
    for l in view.logicals() {
        let (Some(r), Some((key, kind, lane))) = (view.head(l), view.logical_info(l)) else {
            continue;
        };
        let summary = match oracle {
            Some(_) => by_key[&key].clone(),
            None => view.status(r).unwrap(),
        };
        let content = view.revision_content(r).unwrap();
        let body = match snap.log.read_blob(&content) {
            contextdb_store::Payload::Available(b) => String::from_utf8(b).unwrap(),
            other => panic!("{other:?}"),
        };
        let toks = tokens(&format!("{key} {kind:?} {lane} {body}"));
        out.insert(
            key.clone(),
            Head {
                rev: r,
                kind,
                summary,
                tokens: toks,
                key,
            },
        );
    }
    out
}
/// The heads a query matches: a key substring, or a shared term.
fn matches<'a>(heads: &'a BTreeMap<String, Head>, query: &str) -> Vec<&'a Head> {
    let q = tokens(query);
    heads
        .values()
        .filter(|h| h.key.contains(query) || !q.is_disjoint(&h.tokens))
        .collect()
}
fn slabel(i: &Item) -> (String, RevisionId, String) {
    (i.key.clone(), i.rev, format!("{:?}", i.summary))
}
fn label(i: &Item) -> (String, RevisionId, Summary) {
    (i.key.clone(), i.rev, i.summary.clone())
}

#[derive(Default, Debug)]
struct Counts {
    seeds: u64,
    queries: u64,
    items: u64,
    withheld: u64,
    short_pages: u64,
    full_pages: u64,
    budgeted: u64,
    dev_queries: u64,
    labels: BTreeMap<String, u64>,
}

#[test]
fn prop14_retrieval_against_the_oracle() {
    let mut c = Counts::default();
    for seed in 0..seeds() {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0x5052_4f50_3134);
        let trace = generate_p7(seed);
        let s = service();
        let mut oracle = InputModel::default();
        let split = trace.batches.len() / 2;
        let mut dev: Option<BranchId> = None;
        for (n, batch) in trace.batches.iter().enumerate() {
            if n == split {
                let r = s.handle(
                    ADMIN,
                    Request::BranchCreate {
                        name: "dev".into(),
                        from: None,
                        client_txn_id: None,
                    },
                );
                assert!(!matches!(r.response, Response::Error { .. }), "{r:?}");
                dev = s.actor.snapshot().kernel.branch_by_name("dev");
            }
            let (mut b, blobs) = rewritten(batch);
            // After the fork every batch goes to `dev`: `main` allocating
            // again would renumber its ids against the oracle's.
            let on_dev = n >= split;
            if on_dev {
                b.branch = dev.unwrap();
            }
            let r = s.handle(
                ADMIN,
                Request::Raw {
                    blobs,
                    batch: b.clone(),
                },
            );
            if matches!(r.response, Response::Committed { .. }) && !on_dev {
                let mut ob = b.clone();
                ob.branch = MAIN;
                oracle.apply(&ob).unwrap_or_else(|e| {
                    panic!("seed {seed}: oracle refused a committed batch: {e:?}")
                });
            }
        }
        let dev = dev.unwrap();
        let main_heads = heads(&s, MAIN, Some(&oracle));
        let dev_heads = heads(&s, dev, None);
        // The oracle and the kernel agree on main (a sanity check of the
        // reference itself).
        for (k, h) in &main_heads {
            let snap = s.actor.snapshot();
            let v = snap.kernel.branch(MAIN).unwrap();
            assert_eq!(v.status(h.rev).unwrap(), h.summary, "seed {seed} {k}");
        }
        let bindings = [
            (attach(&s, "m", None), &main_heads, false),
            (attach(&s, "d", Some("dev")), &dev_heads, true),
        ];
        let keys: Vec<String> = main_heads.keys().chain(dev_heads.keys()).cloned().collect();
        for q in 0..12 {
            let (b, heads, is_dev) = &bindings[q % 2];
            let query: String = match rng.next_u32() % 6 {
                0 if !keys.is_empty() => keys[rng.next_u32() as usize % keys.len()].clone(),
                1 if !keys.is_empty() => {
                    let k = &keys[rng.next_u32() as usize % keys.len()];
                    k[..1.max(k.len() / 2)].to_owned()
                }
                2 => VOCAB[rng.next_u32() as usize % VOCAB.len()].to_owned(),
                3 => format!(
                    "{} {}",
                    VOCAB[rng.next_u32() as usize % VOCAB.len()],
                    VOCAB[rng.next_u32() as usize % VOCAB.len()]
                ),
                4 => "zzqq absent".to_owned(),
                _ => "note".to_owned(),
            };
            let mode = if rng.next_u32() % 2 == 0 {
                Mode::Certified
            } else {
                Mode::Advisory
            };
            let limit = 1 + rng.next_u32() % 8;
            let kinds: Vec<RecordKind> = if rng.next_u32() % 3 == 0 {
                vec![RecordKind::Fact, RecordKind::Decision]
            } else {
                Vec::new()
            };
            let base = RecallOpts {
                kinds: kinds.clone(),
                ..RecallOpts::default()
            };
            let page = recall(&s, b, &query, "", mode, limit, base.clone());
            let at = format!(
                "seed {seed} q{q} {query:?} {mode:?} limit {limit} kinds {kinds:?} dev {is_dev}"
            );
            // The reference eligible set.
            let matched: Vec<&Head> = matches(heads, &query)
                .into_iter()
                .filter(|h| kinds.is_empty() || kinds.contains(&h.kind))
                .collect();
            let eligible: BTreeSet<&str> = (matched.iter())
                .filter(|h| mode == Mode::Advisory || h.summary == Summary::Certified)
                .map(|h| h.key.as_str())
                .collect();
            let mut seen = BTreeSet::new();
            for i in &page.items {
                let h = heads
                    .get(&i.key)
                    .unwrap_or_else(|| panic!("{at}: {} not live", i.key));
                assert!(seen.insert(i.key.clone()), "{at}: {} twice", i.key);
                assert_eq!(i.rev, h.rev, "{at}: {} not the head", i.key);
                assert_eq!(i.summary, h.summary, "{at}: {} label", i.key);
                assert!(
                    eligible.contains(i.key.as_str()),
                    "{at}: {} not eligible",
                    i.key
                );
                if mode == Mode::Certified {
                    assert_eq!(i.summary, Summary::Certified, "{at}");
                }
                *c.labels
                    .entry(
                        format!("{:?}", i.summary)
                            .split('(')
                            .next()
                            .unwrap()
                            .to_owned(),
                    )
                    .or_default() += 1;
            }
            assert_eq!(
                page.items.len(),
                eligible.len().min(limit as usize),
                "{at}: short page with eligible matches left: got {:?}, eligible {eligible:?}",
                page.items.iter().map(|i| &i.key).collect::<Vec<_>>()
            );
            if page.items.len() < limit as usize {
                c.short_pages += 1;
            } else {
                c.full_pages += 1;
            }
            assert!(page.withheld.len() <= limit as usize, "{at}");
            for w in &page.withheld {
                let h = &heads[&w.key];
                assert_ne!(h.summary, Summary::Certified, "{at}: {} withheld", w.key);
                assert_eq!(w.rev, h.rev, "{at}");
                assert!(
                    matched.iter().any(|m| m.key == w.key),
                    "{at}: {} withheld, not matched",
                    w.key
                );
            }
            if mode == Mode::Advisory {
                assert!(page.withheld.is_empty(), "{at}");
            }
            // A budget and a level keep labels and order.
            if rng.next_u32() % 2 == 0 {
                let budget = rng.next_u32() % 400;
                let level = [Level::Header, Level::Abstract, Level::Outline, Level::Full]
                    [rng.next_u32() as usize % 4];
                let packed = recall(
                    &s,
                    b,
                    &query,
                    "",
                    mode,
                    limit,
                    RecallOpts {
                        budget_tokens: Some(budget),
                        level: Some(level),
                        ..base.clone()
                    },
                );
                assert!(packed.items.len() <= page.items.len(), "{at}");
                for (x, y) in packed.items.iter().zip(&page.items) {
                    assert_eq!(label(x), label(y), "{at}: budget {budget}");
                    assert!(x.body.as_ref().is_some_and(|r| r.level <= level), "{at}");
                }
                assert_eq!(
                    packed.withheld.iter().map(label).collect::<Vec<_>>(),
                    page.withheld.iter().map(label).collect::<Vec<_>>(),
                    "{at}"
                );
                c.budgeted += 1;
            }
            // Lane "" and "repo" agree.
            let laned = recall(&s, b, &query, "repo", mode, limit, base.clone());
            assert_eq!(
                laned.items.iter().map(label).collect::<Vec<_>>(),
                page.items.iter().map(label).collect::<Vec<_>>(),
                "{at}: lane"
            );
            c.queries += 1;
            c.items += page.items.len() as u64;
            c.withheld += page.withheld.len() as u64;
            c.dev_queries += u64::from(*is_dev);
        }
        // Rebuild: the same items with the same statuses.
        let probe: Vec<(Binding, String)> = bindings
            .iter()
            .flat_map(|(b, ..)| {
                ["note", "otter", "maple quartz"].map(|q| (b.clone(), q.to_owned()))
            })
            .collect();
        let before: Vec<BTreeSet<(String, RevisionId, String)>> = (probe.iter())
            .map(|(b, q)| {
                recall(&s, b, q, "", Mode::Advisory, 100, RecallOpts::default())
                    .items
                    .iter()
                    .map(slabel)
                    .collect()
            })
            .collect();
        let r = s.handle(
            ADMIN,
            Request::Reindex {
                target: ReindexTarget::Fts,
            },
        );
        assert!(matches!(r.response, Response::Reindexed { .. }));
        for ((b, q), want) in probe.iter().zip(&before) {
            let got: BTreeSet<_> = recall(&s, b, q, "", Mode::Advisory, 100, RecallOpts::default())
                .items
                .iter()
                .map(slabel)
                .collect();
            assert_eq!(&got, want, "seed {seed}: {q:?} after reindex");
        }
        // Adding an unrelated record never removes a certified exact-key
        // match.
        let main_b = &bindings[0].0;
        let exact: Vec<String> = (main_heads.values())
            .filter(|h| h.summary == Summary::Certified)
            .map(|h| h.key.clone())
            .take(3)
            .collect();
        let firsts: Vec<Option<String>> = (exact.iter())
            .map(|k| {
                recall(&s, main_b, k, "", Mode::Certified, 3, RecallOpts::default())
                    .items
                    .first()
                    .map(|i| i.key.clone())
            })
            .collect();
        let unrelated = b"unrelated filler".to_vec();
        let r = s.handle(
            ADMIN,
            Request::Raw {
                blobs: vec![unrelated.clone()],
                batch: Batch::new(vec![Op::CreateLogical {
                    key: "zz-unrelated".into(),
                    kind: RecordKind::Fact,
                    scope_lane: "repo".into(),
                    content: *blake3::hash(&unrelated).as_bytes(),
                    adopt_as_base: true,
                }]),
            },
        );
        assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
        for (k, first) in exact.iter().zip(firsts) {
            assert_eq!(
                first.as_deref(),
                Some(k.as_str()),
                "seed {seed}: exact key first"
            );
            let after = recall(&s, main_b, k, "", Mode::Certified, 3, RecallOpts::default());
            assert_eq!(
                after.items.first().map(|i| i.key.as_str()),
                Some(k.as_str()),
                "seed {seed}"
            );
        }
        s.handle(ADMIN, Request::Shutdown);
        c.seeds += 1;
    }
    eprintln!("PROP14: {c:?}");
    assert!(c.short_pages > 0 && c.full_pages > 0 && c.budgeted > 0 && c.withheld > 0);
}
