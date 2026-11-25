//! P11 milestone 5, Gate P11 G5 evidence (ADR 0019 §9 G5): one item per
//! process, one output line with its error and work counts.
//!
//! Usage: p11_g5 <item> [n=1000000] [hub|chain] [E=LAZY_BUDGET] [read_settle_ms]
//!
//! - `cascade`: a store with file projections and the default lazy budget,
//!   as ctxd opens it. Revision 0's base retracted: the ack, its op_log
//!   frame bytes and the `settle()` time; and the same toggle on the same
//!   store opened eager. Each sample reopens the store lazy, then eager.
//! - `seeding`: the ack of `Db::commit_lazy` with a one-unit budget, for a
//!   base toggle on the shape and for a SetHead on a record that 10⁵ rules
//!   cite through Sem.
//! - `next_ack`: through an in-process ctxd, a small write's ack right
//!   after a cascade's, and after the idle drain finished.
//! - `structural_unknown`: through ctxd with the default `read_settle_ms`,
//!   Certified reads of random revisions while a cascade is pending.
//! - `gc`: `gc_plan` and `gc_apply` times on n revisions, a quarter of
//!   them superseded before a 10,000-epoch keep window.
//! - `reopen`: `Store::open` after load and three cascades.
//!
//! The shape is hub or chain. `E` and `read_settle_ms` exist so a small
//! smoke run can defer its cascade and catch a read during it; G5 runs
//! leave both at their defaults. The line names the parameters its item
//! uses.
use contextdb_bench::p11::*;
use contextdb_core::{Budget, CommitStats, Counting, Db, Drain, LAZY_BUDGET};
use contextdb_daemon::{
    actor::ActorHandle,
    proto::*,
    server::{Daemon, Options},
    service::Identity,
};
use contextdb_model::*;
use contextdb_proto::docs::nested_hashes;
use contextdb_store::{gc, *};
use contextdb_testkit::{Shape, shape};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SAMPLES: usize = 3;
const PAYLOAD: &[u8] = b"payload";
/// The Sem fixture's citing rules (ADR 0019 §9 G5).
const SEM_RULES: usize = 100_000;
/// The default `keep_epochs` (O3).
const KEEP_EPOCHS: u64 = 10_000;
/// The share of the gc fixture's revisions that are superseded.
const SUPERSEDED: f64 = 0.25;
/// `structural_unknown`: cascades, and reads at most per cascade.
const ROUNDS: usize = 5;
const READS: usize = 200;
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId::STORE_OWNER,
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId::STORE_OWNER,
};

struct Run {
    kind: Shape,
    read_settle_ms: Option<u64>,
    n: usize,
    budget: Budget,
    dir: PathBuf,
    errors: Errors,
    line: String,
}

fn main() {
    let usage = "usage: p11_g5 <cascade|seeding|next_ack|structural_unknown|gc|reopen> [n=1000000] [hub|chain] [E] [read_settle_ms]";
    let args: Vec<String> = std::env::args().collect();
    let item = args.get(1).map(String::as_str).unwrap_or_default();
    let n = args.get(2).map_or(1_000_000, |s| s.parse().expect(usage));
    let name = match args.get(3).map(String::as_str) {
        None | Some("hub") => "hub",
        Some("chain") => "chain",
        Some(_) => panic!("{usage}"),
    };
    let e = args.get(4).map_or(LAZY_BUDGET, |s| s.parse().expect(usage));
    let read_settle_ms = args.get(5).map(|s| s.parse().expect(usage));
    let read_settle = read_settle_ms.map_or("default".into(), |ms: u64| ms.to_string());
    let parameters = match item {
        "gc" => String::new(),
        "seeding" => format!(" shape={name}"),
        "structural_unknown" => format!(" shape={name} E={e} read_settle_ms={read_settle}"),
        _ => format!(" shape={name} E={e}"),
    };
    let items = [
        "cascade",
        "seeding",
        "next_ack",
        "structural_unknown",
        "gc",
        "reopen",
    ];
    assert!(n >= 16 && items.contains(&item), "{usage}");
    let dir = std::env::temp_dir().join(format!("p11g5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut run = Run {
        kind: parse_shape(name).unwrap(),
        read_settle_ms,
        n,
        budget: Budget::new(e),
        dir: dir.clone(),
        errors: Errors::default(),
        line: format!("p11_g5 item={item} n={n}{parameters}"),
    };
    let fields = match item {
        "cascade" => cascade(&mut run),
        "seeding" => seeding(&mut run),
        "next_ack" => next_ack(&mut run),
        "structural_unknown" => structural_unknown(&mut run),
        "gc" => collect(&mut run),
        "reopen" => reopen(&mut run),
        _ => panic!("{usage}"),
    };
    let _ = std::fs::remove_dir_all(&dir);
    run.errors.finish(&format!("{} {fields}", run.line));
}

fn options(dir: &Path, lazy: Option<Budget>) -> OpenOptions {
    OpenOptions {
        projections: ProjectionFile::Path(dir.join(PROJECTIONS_FILE)),
        lazy,
        ..OpenOptions::default()
    }
}
impl Run {
    fn need<T, E: std::fmt::Debug>(&mut self, what: &str, r: Result<T, E>) -> T {
        self.errors.need(&self.line, what, r)
    }
    fn open(&mut self, lazy: Option<Budget>) -> (Store<StdDisk>, OpenReport) {
        let r = Store::open(StdDisk::default(), &self.dir, &options(&self.dir, lazy));
        self.need("open", r)
    }
    /// The shape in a fresh store, its contents pointed at one real blob.
    /// Returns the store and the L0 flips a retract of revision 0 causes.
    fn load(&mut self, lazy: Option<Budget>) -> (Store<StdDisk>, usize) {
        let (mut s, _) = self.open(lazy);
        let blob = s.put_blob(PAYLOAD);
        let blob = self.need("put_blob", blob);
        let mut generated = shape(self.kind, self.n, 42);
        for b in &mut generated.batches {
            for op in &mut b.ops {
                if let Op::CreateLogical { content, .. } = op {
                    *content = blob;
                }
            }
        }
        for b in &generated.batches {
            let r = s.commit(b);
            self.need("load", r);
        }
        let r = s.settle();
        self.need("settle", r);
        (s, expected_retract_flips(&generated))
    }
    /// The diff rows the projections hold for epoch `e`: written when the
    /// epoch settles.
    fn diff_rows(&mut self, s: &Store<StdDisk>, e: Epoch) -> usize {
        match s.changes(MAIN, Epoch(e.0 - 1), self.n + 16) {
            Ok(ChangePage::Rows { rows, .. }) => rows.iter().filter(|r| r.0 == e).count(),
            other => {
                (self.errors).fail("changes", format_args!("{other:?}"));
                0
            }
        }
    }
    fn check_count(&mut self, what: &str, got: usize, want: usize) {
        (self.errors).check(what, got == want, format_args!("{got}, want {want}"));
    }
    /// Revision 0's base asserted again and settled, untimed.
    fn restore(&mut self, s: &mut Store<StdDisk>, expected: usize) {
        if let Some(r) = self.errors.ok("restore", s.commit(&toggle(true)))
            && self.errors.ok("restore settle", s.settle()).is_some()
        {
            let rows = self.diff_rows(s, r.epoch);
            self.check_count("restore diff rows", rows, expected);
        }
    }
}
fn log_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir.join("log"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.metadata().ok())
        .map(|m| m.len())
        .sum()
}
fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NAN, f64::max)
}

fn cascade(run: &mut Run) -> String {
    let (s, expected) = run.load(None);
    drop(s);
    let (mut ack, mut settle, mut total, mut eager) = (vec![], vec![], vec![], vec![]);
    let (mut frame, mut deferred) = (0, 0);
    // Each sample reopens the store lazy, then eager, so both sides run on
    // a freshly opened store and alternate.
    for _ in 0..SAMPLES {
        let (mut s, _) = run.open(Some(run.budget));
        let before = log_bytes(&run.dir);
        let t = Instant::now();
        let r = s.commit(&toggle(false));
        let ack_ms = ms(t);
        frame = frame.max(log_bytes(&run.dir) - before);
        deferred += usize::from(s.frontier_epoch().is_some());
        let t = Instant::now();
        let settled = s.settle();
        let settle_ms = ms(t);
        ack.push(ack_ms);
        settle.push(settle_ms);
        total.push(ack_ms + settle_ms);
        if let Some(r) = run.errors.ok("lazy retract", r)
            && run.errors.ok("settle", settled).is_some()
        {
            let rows = run.diff_rows(&s, r.epoch);
            run.check_count("lazy diff rows", rows, expected);
        }
        run.restore(&mut s, expected);
        drop(s);
        let (mut s, _) = run.open(None);
        let t = Instant::now();
        let r = s.commit(&toggle(false));
        eager.push(ms(t));
        if let Some(r) = run.errors.ok("eager retract", r) {
            run.check_count("eager status_diff", r.status_diff.len(), expected);
            let rows = run.diff_rows(&s, r.epoch);
            run.check_count("eager diff rows", rows, expected);
        }
        run.restore(&mut s, expected);
    }
    let (ack_max, total_m, eager_m) = (max(&ack), median(&mut total), median(&mut eager));
    format!(
        "ack_ms={:.1} ack_max_ms={ack_max:.1} frame_max_bytes={frame} settle_ms={:.1} total_ms={total_m:.1} eager_ms={eager_m:.1} total_over_eager={:.3} ack_le_150ms={} frame_le_1KiB={} total_le_1.25_eager={} deferred={deferred}/{SAMPLES} status_diff={expected}",
        median(&mut ack),
        median(&mut settle),
        total_m / eager_m,
        ack_max <= 150.0,
        frame <= 1024,
        total_m <= 1.25 * eager_m,
    )
}

fn seeding(run: &mut Run) -> String {
    let one = Budget {
        units: 1,
        ..Budget::default()
    };
    // A base toggle on the shape.
    let generated = shape(run.kind, run.n, 42);
    let expected = expected_retract_flips(&generated);
    let mut db = Db::<Counting, CommitStats>::new();
    for b in &generated.batches {
        let r = db.commit(b);
        run.need("load", r);
    }
    drop(generated);
    let (mut base, mut base_units) = (vec![], 0);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let r = db.commit_lazy(&toggle(false), one);
        base.push(ms(t));
        let settled = db.drain(Drain::Full);
        base_units = db.stats().units;
        if let Some(r) = run.errors.ok("base retract", r) {
            let flips = settled.map_or(r.l0_flips.len(), |s| s.l0_flips.len());
            run.check_count("base retract l0 flips", flips, expected);
        }
        if let Some(r) = run.errors.ok("base restore", db.commit(&toggle(true))) {
            run.check_count("base restore l0 flips", r.l0_flips.len(), expected);
        }
    }
    drop(db);
    // Record x: revision 0 (base, head) and revision 1; SEM_RULES records
    // whose rule cites x's revision 0 through Sem. Moving x's head to 1
    // seeds every citing rule.
    let create = |key: String, base: bool| Op::CreateLogical {
        origin: Default::default(),
        key,
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [1; 32],
        adopt_as_base: base,
    };
    let x = || LogicalRef::Key("sem/x".into());
    let mut nodes = vec![
        create("sem/x".into(), true),
        Op::AddRevision {
            logical: x(),
            content: [2; 32],
            adopt_as_base: false,
        },
    ];
    nodes.extend((0..SEM_RULES).map(|i| create(format!("sem/{i}"), false)));
    let rules = (0..SEM_RULES)
        .map(|i| Op::AddRule {
            conclusion: RevisionId(2 + i as u32).into(),
            incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
            enabled: true,
        })
        .collect();
    let mut db = Db::<Counting, CommitStats>::new();
    for b in [Batch::new(nodes), Batch::new(rules)] {
        let r = db.commit(&b);
        run.need("sem load", r);
    }
    let head = |from: u32, to: u32| {
        Batch::new(vec![Op::SetHead {
            logical: x(),
            expected: RevisionId(from).into(),
            new: RevisionId(to).into(),
        }])
    };
    let mut reference = db.clone();
    let eager = run.need("sem eager reference", reference.commit(&head(0, 1)));
    drop(reference);
    let (mut sem, mut sem_units, mut sem_diff) = (vec![], 0, 0);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        let r = db.commit_lazy(&head(0, 1), one);
        sem.push(ms(t));
        let settled = db.drain(Drain::Full);
        sem_units = db.stats().units;
        if let Some(r) = run.errors.ok("sem sethead", r) {
            let diff = settled.map_or(r.status_diff, |s| s.status_diff);
            sem_diff = diff.len();
            (run.errors).check(
                "sem work",
                sem_diff >= SEM_RULES,
                format_args!("{sem_diff} status changes for {SEM_RULES} citing rules"),
            );
            (run.errors).check(
                "sem status_diff",
                diff == eager.status_diff,
                format_args!("{} nodes, eager {}", diff.len(), eager.status_diff.len()),
            );
        }
        run.errors.ok("sem restore", db.commit(&head(1, 0)));
    }
    format!(
        "base_seed_ms={:.2} base_units={base_units} base_l0_flips={expected} sem_seed_ms={:.2} sem_rules={SEM_RULES} sem_units={sem_units} sem_status_diff={sem_diff}",
        median(&mut base),
        median(&mut sem),
    )
}

/// The shape in a store, then ctxd started on it: `[maintenance]` at its
/// defaults, but for `E` when the run sets one.
fn daemon(run: &mut Run) -> Daemon {
    let (s, _) = run.load(None);
    drop(s);
    let mut toml = String::new();
    if run.budget.units != LAZY_BUDGET {
        toml.push_str(&format!("lazy_budget = {}\n", run.budget.units));
    }
    if let Some(ms) = run.read_settle_ms {
        toml.push_str(&format!("read_settle_ms = {ms}\n"));
    }
    if !toml.is_empty() {
        let toml = format!("[maintenance]\n{toml}");
        let r = std::fs::write(run.dir.join("ctxd.toml"), toml);
        run.need("ctxd.toml", r);
    }
    let d = Daemon::start(&run.dir, &Options::default());
    let d = run.need("ctxd start", d);
    settled(&d.service.actor, &mut run.errors);
    d
}
fn admin(d: &Daemon, batch: Batch, blobs: Vec<Vec<u8>>) -> Response {
    let batch = Batch {
        channel: Channel::Admin,
        ..batch
    };
    d.service
        .handle(ADMIN, Request::Raw { blobs, batch })
        .response
}
/// A write that settles nothing: a new base record.
fn small(d: &Daemon, i: usize) -> Response {
    let bytes = format!("probe {i}").into_bytes();
    let batch = Batch::new(vec![Op::CreateLogical {
        origin: Default::default(),
        key: format!("probe/{i}"),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: *blake3::hash(&bytes).as_bytes(),
        adopt_as_base: true,
    }]);
    admin(d, batch, vec![bytes])
}
/// Whether the write committed, and whether its maintenance is pending.
fn committed(errors: &mut Errors, what: &str, r: Response) -> Option<bool> {
    match r {
        Response::Committed { settling, .. } => Some(settling),
        other => {
            errors.fail(what, format_args!("{other:?}"));
            None
        }
    }
}
/// Waits until the published snapshot has no frontier (60 s at most).
fn settled(a: &ActorHandle<StdDisk>, errors: &mut Errors) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snap = a.snapshot();
        if !snap.db().has_frontier() {
            return;
        }
        let g = snap.generation;
        drop(snap);
        if Instant::now() > deadline {
            return errors.fail("settle wait", format_args!("frontier after 60 s"));
        }
        a.wait_generation(g, Duration::from_secs(1));
    }
}
fn actor_counts(run: &mut Run, d: &Daemon) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let stats = &d.service.actor.stats;
    let mismatches = stats.replay_mismatches.load(Relaxed);
    let what = "replay mismatches";
    (run.errors).check(what, mismatches == 0, format_args!("{mismatches}"));
    format!(
        "clone_fallbacks={} requeued={} replay_mismatches={mismatches}",
        stats.clone_fallbacks.load(Relaxed),
        stats.requeued.load(Relaxed),
    )
}

/// A write's ack time; whether it committed and left a frontier.
fn write(
    errors: &mut Errors,
    what: &str,
    r: &dyn Fn() -> Response,
    times: &mut Vec<f64>,
) -> Option<bool> {
    let t = Instant::now();
    let r = r();
    times.push(ms(t));
    committed(errors, what, r)
}
fn next_ack(run: &mut Run) -> String {
    let d = daemon(run);
    let (mut cascade, mut next, mut idle, mut next_idle) = (vec![], vec![], vec![], vec![]);
    let mut deferred = 0;
    let errors = &mut run.errors;
    for i in 0..SAMPLES {
        // Right after a cascade.
        let settling = write(
            errors,
            "cascade",
            &|| admin(&d, toggle(false), vec![]),
            &mut cascade,
        );
        deferred += usize::from(settling == Some(true));
        let settling = write(errors, "next", &|| small(&d, 2 * i), &mut next);
        errors.check(
            "next",
            settling == Some(false),
            format_args!("{settling:?}"),
        );
        settled(&d.service.actor, errors);
        // After the idle drain.
        let settling = write(
            errors,
            "cascade",
            &|| admin(&d, toggle(true), vec![]),
            &mut cascade,
        );
        deferred += usize::from(settling == Some(true));
        let t = Instant::now();
        settled(&d.service.actor, errors);
        idle.push(ms(t));
        let settling = write(
            errors,
            "next after idle",
            &|| small(&d, 2 * i + 1),
            &mut next_idle,
        );
        errors.check(
            "next",
            settling == Some(false),
            format_args!("{settling:?}"),
        );
    }
    let counts = actor_counts(run, &d);
    format!(
        "cascade_ack_ms={:.1} next_ack_ms={:.1} idle_drain_ms={:.1} next_ack_after_idle_ms={:.1} deferred={deferred}/{} {counts}",
        median(&mut cascade),
        median(&mut next),
        median(&mut idle),
        median(&mut next_idle),
        2 * SAMPLES,
    )
}

fn structural_unknown(run: &mut Run) -> String {
    let d = daemon(run);
    let a = &d.service.actor;
    let start = Request::SessionStart {
        harness: "p11_g5".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
        branch: None,
        git_branch: None,
    };
    let session = match d.service.handle(AGENT, start).response {
        Response::Session { session } => session,
        other => run.need("session", Err(other)),
    };
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let (mut reads, mut pending, mut certified, mut other, mut deferred) = (0, 0, 0, 0, 0);
    let mut times = vec![];
    for round in 0..ROUNDS {
        let r = admin(&d, toggle(round % 2 == 1), Vec::new());
        deferred += usize::from(committed(&mut run.errors, "cascade", r) == Some(true));
        for _ in 0..READS {
            if !a.snapshot().db().has_frontier() {
                break;
            }
            // splitmix64
            rng = rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = rng;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            let k = (z ^ (z >> 31)) % run.n as u64;
            let read = Request::Read {
                binding: Binding::root(session),
                target: Target::Key(format!("shape/{k}")),
                mode: Mode::Certified,
                level: None,
            };
            let t = Instant::now();
            let r = d.service.handle(AGENT, read).response;
            times.push(ms(t));
            reads += 1;
            match r {
                Response::Read { .. } => certified += 1,
                Response::Error { code, detail } if code == "NotCertified" => {
                    if detail.ends_with("Pending(StructuralUnknown)") {
                        pending += 1;
                    } else {
                        other += 1;
                    }
                }
                r => run.errors.fail("read", format_args!("{r:?}")),
            }
        }
        settled(a, &mut run.errors);
    }
    let counts = actor_counts(run, &d);
    format!(
        "structural_unknown_fraction={:.3} reads={reads} structural_unknown={pending} certified={certified} not_certified_other={other} read_median_ms={:.1} read_max_ms={:.1} deferred={deferred}/{ROUNDS} {counts}",
        pending as f64 / reads.max(1) as f64,
        median(&mut times),
        max(&times),
    )
}

/// 128-byte payloads, one per revision, so superseded ones are collectable.
fn payload(i: usize) -> Vec<u8> {
    format!("p11 gc payload {i:<112}").into_bytes()
}
/// Puts the payloads of `range` and returns their hashes.
fn stage(run: &mut Run, s: &mut Store<StdDisk>, range: std::ops::Range<usize>) -> Vec<ContentHash> {
    let blobs: Vec<Vec<u8>> = range.map(payload).collect();
    let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
    let r = s.put_blobs(&refs);
    run.need("put_blobs", r);
    blobs.iter().map(|b| *blake3::hash(b).as_bytes()).collect()
}
fn collect(run: &mut Run) -> String {
    let n = run.n;
    let superseded = (n as f64 * SUPERSEDED) as usize;
    let records = n - superseded;
    // Built without F_FULLFSYNC, then reopened as ctxd opens it.
    let setup = Store::open(
        StdDisk { full_sync: false },
        &run.dir,
        &options(&run.dir, None),
    );
    let (mut s, _) = run.need("open", setup);
    for chunk in (0..records).step_by(50_000) {
        let hashes = stage(run, &mut s, chunk..(chunk + 50_000).min(records));
        let ops = (hashes.iter().enumerate())
            .map(|(i, content)| Op::CreateLogical {
                origin: Default::default(),
                key: format!("gc/{}", chunk + i),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: *content,
                adopt_as_base: false,
            })
            .collect();
        let r = s.commit(&Batch::new(ops));
        run.need("create", r);
    }
    // Supersede records spread over the store, 1,000 per commit.
    let stride = records / superseded.max(1);
    for chunk in (0..superseded).step_by(1000) {
        let range = chunk..(chunk + 1000).min(superseded);
        let hashes = stage(run, &mut s, records + range.start..records + range.end);
        let mut ops = vec![];
        for (k, content) in hashes.into_iter().enumerate() {
            let old = (range.start + k) * stride;
            let logical = LogicalRef::Key(format!("gc/{old}"));
            ops.push(Op::AddRevision {
                logical: logical.clone(),
                content,
                adopt_as_base: false,
            });
            ops.push(Op::SetHead {
                logical,
                expected: RevisionId(old as u32).into(),
                new: RevRef::New(k as u16),
            });
        }
        let r = s.commit(&Batch::new(ops));
        run.need("supersede", r);
    }
    // The keep window: KEEP_EPOCHS small commits after the supersessions.
    let blob = s.put_blob(PAYLOAD);
    let blob = run.need("put_blob", blob);
    for k in 0..KEEP_EPOCHS {
        let r = s.commit(&Batch::new(vec![Op::CreateLogical {
            origin: Default::default(),
            key: format!("window/{k}"),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: blob,
            adopt_as_base: false,
        }]));
        run.need("window", r);
    }
    drop(s);
    let (mut s, _) = run.open(Some(Budget::default()));
    let e0 = s.epoch();
    let cutoff = Epoch(e0.0 - KEEP_EPOCHS);
    let t = Instant::now();
    let plan = match s.kernel() {
        Ok(k) => gc::gc_plan(k, &s.log_reader(), e0, cutoff, &nested_hashes),
        Err(e) => Err(e),
    };
    let plan_ms = ms(t);
    let plan = run.need("gc_plan", plan);
    run.check_count("candidates", plan.candidates.len(), superseded);
    let t = Instant::now();
    let report = s.gc_apply(&plan, &BTreeSet::new(), s.epoch(), &nested_hashes);
    let apply_ms = ms(t);
    let report = run.need("gc_apply", report);
    run.check_count("collected", report.collected.len(), superseded);
    run.check_count("rechecked_out", report.rechecked_out, 0);
    format!(
        "gc_plan_ms={plan_ms:.0} gc_apply_ms={apply_ms:.0} revisions={} superseded={superseded} keep_epochs={KEEP_EPOCHS} candidates={} collected={} collected_bytes={} kept_k1={} kept_k3={}",
        records + superseded,
        plan.candidates.len(),
        report.collected.len(),
        report.bytes,
        plan.kept_k1,
        plan.kept_k3,
    )
}

fn reopen(run: &mut Run) -> String {
    let (mut s, expected) = run.load(Some(run.budget));
    for _ in 0..SAMPLES {
        if let Some(r) = run.errors.ok("retract", s.commit(&toggle(false)))
            && run.errors.ok("settle", s.settle()).is_some()
        {
            let rows = run.diff_rows(&s, r.epoch);
            run.check_count("retract diff rows", rows, expected);
        }
        run.restore(&mut s, expected);
    }
    let epoch = s.epoch();
    drop(s);
    let t = Instant::now();
    let (s, report) = run.open(Some(run.budget));
    let reopen_ms = ms(t);
    (run.errors).check(
        "reopen epoch",
        s.epoch() == epoch,
        format_args!("{:?}, before {epoch:?}", s.epoch()),
    );
    format!(
        "reopen_ms={reopen_ms:.0} epoch={} replayed={} snapshot_epoch={:?}",
        epoch.0,
        report.replayed,
        report.snapshot_epoch.map(|e| e.0),
    )
}
