//! P11 G6 A: the end-to-end scenarios through a real `ctxd` process and
//! its client protocol (the sockets the CLI uses). One output line per
//! step with its counts and errors; exit 1 when any error occurred.
//!
//! Usage:
//!   p11_g6 hub [n=1000000] [options]
//!   p11_g6 gc [n=2000] [options]
//! Options:
//!   --bin DIR             where `ctxd` and `ctx` are (default: next to this binary)
//!   --evidence DIR        checkpoints, verify output and ctxd logs (default: a new temp dir)
//!   --lazy-budget E       `[maintenance] lazy_budget` (default: ctxd's)
//!   --read-settle-ms MS   `[maintenance] read_settle_ms` (default: ctxd's)
//!   --chunk N             ops per load request (hub, default 50000)
//!   --rounds R            probe rounds per cascade at most (hub, default 20)
//!   --keep                keep the data directory
//!
//! `hub`: a hub of n revisions (bases 0, 1, 2; every other revision
//! concludes a rule on all three), loaded in chunks. Revision 0's base is
//! retracted, then restored; during each cascade STRICT/Certified reads,
//! certificate issue and validate and the hook gate's check run, every
//! answer is recorded, and none may be a false positive. After each
//! settles, `ctx g6 checkpoint` and `ctx g6 verify`. Then a next write; a
//! mirror export right after another deferred ack, which an export after
//! the settle must leave unchanged; a SIGKILL while a cascade is pending, a
//! restart, every acknowledged client_txn_id looked up, and a checkpoint
//! verified.
//!
//! `gc`: n records with distinct payloads, a quarter superseded, a
//! session-pinned revision, `[gc] keep_epochs = 100`; `ctx gc` dry run and
//! `--apply`, Collected reads, MissingBlob, `ctx gc --compact` with ctxd
//! stopped, restart, re-staging; checkpoints verified, digests unchanged
//! at the same epoch.
use contextdb_model::*;
use contextdb_proto::{
    Client,
    gate::verdict_code,
    lease,
    proto::{Binding, CheckpointReport, MirrorAction, Mode, Request, Response, SessionId, Target},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const USAGE: &str = "usage: p11_g6 <hub|gc> [n] [--bin DIR] [--evidence DIR] [--lazy-budget E] \
                     [--read-settle-ms MS] [--chunk N] [--rounds R] [--keep]";
const PAYLOAD: &[u8] = b"p11 g6 hub payload";
/// Client reads and writes wait at most this long.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// A start (an eager replay of the whole log) or a settle waits this long.
const START_TIMEOUT: Duration = Duration::from_secs(1800);

struct Args {
    item: String,
    n: usize,
    bin: PathBuf,
    evidence: Option<PathBuf>,
    lazy_budget: Option<u64>,
    read_settle_ms: Option<u64>,
    chunk: usize,
    rounds: usize,
    keep: bool,
}
fn args() -> Args {
    let mut a = std::env::args().skip(1);
    let item = a.next().filter(|i| i == "hub" || i == "gc").expect(USAGE);
    let mut out = Args {
        n: if item == "hub" { 1_000_000 } else { 2_000 },
        item,
        bin: std::env::current_exe()
            .expect("current_exe")
            .parent()
            .expect("bin dir")
            .to_path_buf(),
        evidence: None,
        lazy_budget: None,
        read_settle_ms: None,
        chunk: 50_000,
        rounds: 20,
        keep: false,
    };
    let num = |v: Option<String>| -> u64 { v.and_then(|v| v.parse().ok()).expect(USAGE) };
    while let Some(x) = a.next() {
        match x.as_str() {
            "--bin" => out.bin = PathBuf::from(a.next().expect(USAGE)),
            "--evidence" => out.evidence = Some(PathBuf::from(a.next().expect(USAGE))),
            "--lazy-budget" => out.lazy_budget = Some(num(a.next())),
            "--read-settle-ms" => out.read_settle_ms = Some(num(a.next())),
            "--chunk" => out.chunk = num(a.next()) as usize,
            "--rounds" => out.rounds = num(a.next()) as usize,
            "--keep" => out.keep = true,
            n => out.n = n.parse().expect(USAGE),
        }
    }
    assert!(out.n >= 16 && out.chunk > 0, "{USAGE}");
    out
}

/// One scenario run: the ctxd process, its clients and the error count.
struct Run {
    args: Args,
    data: PathBuf,
    evidence: PathBuf,
    errors: u64,
    /// Errors when the current step began.
    step_errors: u64,
    ctxd: Option<Child>,
    starts: usize,
    admin: Option<Client>,
    agent: Option<Client>,
    /// Every acknowledged write: (client_txn_id, epoch).
    acked: Vec<([u8; 16], Epoch)>,
    /// The session the scenario's writes commit through (`ctx_commit`
    /// carries a client_txn_id; a raw batch does not).
    writer: Option<SessionId>,
    txn_seq: u64,
}
impl Run {
    fn fail(&mut self, what: &str, detail: impl std::fmt::Display) {
        self.errors += 1;
        eprintln!("error: {what}: {detail}");
    }
    fn check(&mut self, what: &str, held: bool, detail: impl std::fmt::Display) {
        if !held {
            self.fail(what, detail);
        }
    }
    /// The value, or the run ends here: the error, the final line, exit 1.
    fn need<T>(&mut self, what: &str, r: Result<T, String>) -> T {
        r.unwrap_or_else(|e| {
            self.fail(what, e);
            self.finish()
        })
    }
    fn step(&mut self, name: &str, fields: impl std::fmt::Display) {
        println!(
            "p11_g6 item={} step={name} {fields} errors={}",
            self.args.item,
            self.errors - self.step_errors
        );
        self.step_errors = self.errors;
    }
    fn finish(&mut self) -> ! {
        if let Some(mut c) = self.ctxd.take() {
            let _ = self.admin_call(Request::Shutdown);
            let deadline = Instant::now() + Duration::from_secs(30);
            while c.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
        if !self.args.keep {
            let _ = std::fs::remove_dir_all(&self.data);
        }
        println!(
            "p11_g6 item={} n={} evidence={} data={} errors={}",
            self.args.item,
            self.args.n,
            self.evidence.display(),
            if self.args.keep {
                self.data.display().to_string()
            } else {
                "removed".into()
            },
            self.errors
        );
        std::process::exit(i32::from(self.errors > 0))
    }

    fn connect(&self, socket: &str) -> Result<Client, String> {
        let l = lease::read_lease(&self.data.join("ctxd.lease"))
            .filter(lease::Lease::is_live)
            .ok_or("ctxd is not running")?;
        let path = (l.sockets.iter().find(|s| s.ends_with(socket))).ok_or("no socket")?;
        Client::connect_timeout(path, CALL_TIMEOUT).map_err(|e| format!("connect: {e}"))
    }
    /// Starts ctxd on the data directory and waits until it answers.
    fn start(&mut self) -> Result<f64, String> {
        self.starts += 1;
        let log = self.evidence.join(format!("ctxd-{}.log", self.starts));
        let log = std::fs::File::create(&log).map_err(|e| e.to_string())?;
        let t = Instant::now();
        let child = Command::new(self.args.bin.join("ctxd"))
            .arg(&self.data)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map_err(|e| format!("spawn ctxd: {e}"))?;
        self.ctxd = Some(child);
        self.writer = None;
        loop {
            if let Some(s) = self.ctxd.as_mut().and_then(|c| c.try_wait().ok().flatten()) {
                return Err(format!("ctxd exited at start: {s}"));
            }
            if let (Ok(mut a), Ok(g)) = (self.connect("admin.sock"), self.connect("agent.sock"))
                && a.call(&Request::Status).is_ok()
            {
                self.admin = Some(a);
                self.agent = Some(g);
                return Ok(t.elapsed().as_secs_f64() * 1e3);
            }
            if t.elapsed() > START_TIMEOUT {
                return Err("ctxd did not answer".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    fn admin_call(&mut self, r: Request) -> Result<(Epoch, Response), String> {
        let c = self.admin.as_mut().ok_or("no admin client")?;
        c.call(&r)
            .map(|e| (e.epoch, e.response))
            .map_err(|e| format!("{e:?}"))
    }
    fn agent_call(&mut self, r: Request) -> Result<(Epoch, Response), String> {
        let c = self.agent.as_mut().ok_or("no agent client")?;
        c.call(&r)
            .map(|e| (e.epoch, e.response))
            .map_err(|e| format!("{e:?}"))
    }
    fn txn(&mut self) -> [u8; 16] {
        self.txn_seq += 1;
        let h = blake3::hash(format!("p11_g6 {} {}", std::process::id(), self.txn_seq).as_bytes());
        h.as_bytes()[..16].try_into().expect("16")
    }
    /// `ctx_commit` with a client_txn_id through the writer session; its
    /// ack and settling flag.
    fn raw(&mut self, ops: Vec<Op>, blobs: Vec<Vec<u8>>) -> Result<(CommitResult, bool), String> {
        let id = self.txn();
        let writer = match self.writer {
            Some(w) => w,
            None => {
                let w = self.session()?;
                self.writer = Some(w);
                w
            }
        };
        let commit = Request::Commit {
            binding: Binding::root(writer),
            ops,
            blobs,
            expect_cert_valid: false,
            client_txn_id: Some(id),
        };
        match self.agent_call(commit)?.1 {
            Response::Committed { result, settling } => {
                self.acked.push((id, result.epoch));
                Ok((result, settling))
            }
            other => Err(format!("{other:?}")),
        }
    }
    /// Asks for checkpoints until one comes (a full settle): the report.
    fn settled(&mut self) -> Result<Box<CheckpointReport>, String> {
        let t = Instant::now();
        loop {
            match self.admin_call(Request::Checkpoint { live_pos: None })?.1 {
                Response::Checkpoint { report } => return Ok(report),
                Response::Error { code, .. }
                    if code == "Settling" && t.elapsed() < START_TIMEOUT => {}
                other => return Err(format!("{other:?}")),
            }
        }
    }
    fn session(&mut self) -> Result<SessionId, String> {
        let start = Request::SessionStart {
            harness: "p11_g6".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        };
        match self.agent_call(start)?.1 {
            Response::Session { session } => Ok(session),
            other => Err(format!("{other:?}")),
        }
    }
    fn ctx(&self, args: &[&str]) -> Result<(i32, String), String> {
        let out = Command::new(self.args.bin.join("ctx"))
            .args(args)
            .output()
            .map_err(|e| format!("ctx: {e}"))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        Ok((out.status.code().unwrap_or(-1), text))
    }
    /// `ctx g6 checkpoint` then `ctx g6 verify`, their output kept as
    /// evidence. Returns the checkpoint file's JSON.
    fn checkpoint_and_verify(&mut self, name: &str) -> Value {
        let file = self.evidence.join(format!("{name}.json"));
        let (data, f) = (self.data.display().to_string(), file.display().to_string());
        let t = Instant::now();
        let r = self.ctx(&["g6", "checkpoint", &data, "--out", &f]);
        let (code, out) = self.need("ctx g6 checkpoint", r);
        let checkpoint_ms = t.elapsed().as_millis();
        if code != 0 {
            self.fail("ctx g6 checkpoint", &out);
            return Value::Null;
        }
        let t = Instant::now();
        let r = self.ctx(&["g6", "verify", &data, &f]);
        let (code, verify) = self.need("ctx g6 verify", r);
        let verify_ms = t.elapsed().as_millis();
        let _ = std::fs::write(self.evidence.join(format!("{name}.verify.txt")), &verify);
        let line = (verify.lines())
            .find(|l| l.starts_with("g6 verify "))
            .unwrap_or("g6 verify (no verdict line)")
            .to_owned();
        let payloads = (verify.lines())
            .find(|l| l.starts_with("payloads "))
            .unwrap_or("payloads ?")
            .to_owned();
        self.check("ctx g6 verify", code == 0, &verify);
        let cp: Value = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        self.step(
            &format!("checkpoint_{name}"),
            format_args!(
                "{} {payloads} verify_exit={code} checkpoint_ms={checkpoint_ms} verify_ms={verify_ms} file={f}",
                line.trim_start_matches("g6 verify ")
            ),
        );
        cp
    }
    /// The ctxd.toml settings this run asks for, if any.
    fn write_toml(&mut self, gc_keep: Option<u64>) {
        let mut t = String::new();
        let mut m = String::new();
        if let Some(e) = self.args.lazy_budget {
            m.push_str(&format!("lazy_budget = {e}\n"));
        }
        if let Some(ms) = self.args.read_settle_ms {
            m.push_str(&format!("read_settle_ms = {ms}\n"));
        }
        if !m.is_empty() {
            t.push_str(&format!("[maintenance]\n{m}"));
        }
        if let Some(k) = gc_keep {
            t.push_str(&format!("[gc]\nkeep_epochs = {k}\n"));
        }
        if !t.is_empty() {
            let r = std::fs::write(self.data.join("ctxd.toml"), t).map_err(|e| e.to_string());
            self.need("ctxd.toml", r);
        }
    }
    fn started(&mut self) {
        let r = self.start();
        let ms = self.need("start ctxd", r);
        let r = self.settled();
        let cp = self.need("first checkpoint", r);
        // The binary this run launched, hashed as a file: the running
        // process must report the same.
        let file = std::fs::read(self.args.bin.join("ctxd"))
            .map(|b| blake3::hash(&b).to_hex().to_string())
            .unwrap_or_else(|e| format!("unavailable: {e}"));
        self.check(
            "ctxd binary",
            cp.exe_blake3 == file,
            format_args!("running {} file {file}", cp.exe_blake3),
        );
        self.step(
            "start",
            format_args!(
                "pid={} ctxd_blake3={} ctxd_file_blake3={file} exe={} protocol={} log_format={} lazy_budget={} \
                 read_settle_ms={} keep_epochs={} epoch={} start_ms={ms:.0}",
                cp.pid,
                cp.exe_blake3,
                cp.exe,
                cp.protocol,
                cp.log_format,
                cp.lazy_budget.map_or("eager".into(), |b| b.to_string()),
                cp.read_settle_ms,
                cp.keep_epochs,
                cp.epoch.0
            ),
        );
    }
    /// Every acknowledged write is committed at its acked epoch.
    fn acked_present(&mut self) -> (usize, usize) {
        let mut present = 0;
        for (id, epoch) in self.acked.clone() {
            match self.admin_call(Request::TxnStatus { id }) {
                Ok((
                    _,
                    Response::TxnStatus {
                        committed: Some((e, _)),
                        ..
                    },
                )) if e == epoch => present += 1,
                other => self.fail("acked txn", format_args!("{epoch:?}: {other:?}")),
            }
        }
        (self.acked.len(), present)
    }
}

fn main() {
    let args = args();
    let root = std::env::temp_dir().join(format!("p11g6-{}-{}", args.item, std::process::id()));
    let evidence = args
        .evidence
        .clone()
        .unwrap_or_else(|| root.join("evidence"));
    let data = root.join("data");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::create_dir_all(&evidence).expect("evidence dir");
    let mut run = Run {
        args,
        data,
        evidence,
        errors: 0,
        step_errors: 0,
        ctxd: None,
        starts: 0,
        admin: None,
        agent: None,
        acked: Vec::new(),
        writer: None,
        txn_seq: 0,
    };
    match run.args.item.as_str() {
        "hub" => hub(&mut run),
        _ => gc(&mut run),
    }
    run.finish()
}

// ---------------------------------------------------------------- hub

/// What each probe answered in one cascade, and the false positives.
#[derive(Default)]
struct Tally {
    answers: BTreeMap<String, u64>,
    false_positives: u64,
    /// Answers that show the frontier: PENDING(StructuralUnknown),
    /// `Settling`, a `Pending` verdict.
    frontier: u64,
}
impl Tally {
    fn add(&mut self, probe: &str, answer: &str) {
        *self.answers.entry(format!("{probe}.{answer}")).or_default() += 1;
        if matches!(answer, "pending" | "settling" | "Pending") {
            self.frontier += 1;
        }
    }
    fn line(&self) -> String {
        let a: Vec<String> = self
            .answers
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!(
            "frontier_answers={} false_positives={} {}",
            self.frontier,
            self.false_positives,
            a.join(" ")
        )
    }
}
fn read_answer(r: &Response) -> &'static str {
    match r {
        Response::Read { .. } => "certified",
        Response::Error { code, detail } if code == "NotCertified" => {
            if detail.ends_with("Pending(StructuralUnknown)") {
                "pending"
            } else if detail.contains("Stale(") {
                "stale"
            } else {
                "not_certified"
            }
        }
        Response::Error { code, .. } if code == "Settling" => "settling",
        Response::Error { code, .. } if code == "Busy" => "busy",
        _ => "other",
    }
}
fn positive(v: &Validation) -> bool {
    matches!(v, Validation::Valid | Validation::ValidRecertified { .. })
}
fn check_request(s: SessionId, cert_id: Option<ContentHash>, gate: Option<GateMode>) -> Request {
    Request::Check {
        binding: Binding::root(s),
        cert_id,
        live_pos: Vec::new(),
        action: None,
        mode: gate,
        harness_version: String::new(),
        strict: false,
    }
}
fn read_request(s: SessionId, key: String) -> Request {
    Request::Read {
        binding: Binding::root(s),
        target: Target::Key(key),
        mode: Mode::Certified,
        level: None,
    }
}
/// The hub's state an answer must agree with: support withdrawn
/// (`retracted`) or restored.
#[derive(Clone, Copy, PartialEq)]
enum Truth {
    Retracted,
    Restored,
}
struct Probe {
    session: SessionId,
    cert_id: ContentHash,
    rng: u64,
}
impl Probe {
    fn next_key(&mut self, n: usize) -> String {
        // splitmix64 over the hub's conclusions 3..n
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        format!("shape/{}", 3 + (z ^ (z >> 31)) % (n as u64 - 3))
    }
}
/// One round during a cascade: a Certified read of a random hub record and
/// of an unrelated one, the certificate validated by id, and the hook gate's
/// check (issue and validate). A positive answer the truth denies is a
/// false positive; while restored, a definite negative is wrong too.
fn probe_round(run: &mut Run, p: &mut Probe, truth: Truth, t: &mut Tally) {
    let key = p.next_key(run.args.n);
    let s = p.session;
    let r = run.agent_call(read_request(s, key.clone()));
    let r = run.need("read hub", r).1;
    let a = read_answer(&r);
    t.add("read_hub", a);
    match (truth, a) {
        (Truth::Retracted, "certified") => {
            t.false_positives += 1;
            run.fail("false positive read", format_args!("{key}: {r:?}"));
        }
        (Truth::Restored, "stale" | "not_certified" | "other") => {
            run.fail("wrong negative read", format_args!("{key}: {r:?}"));
        }
        _ => {}
    }
    let r = run.agent_call(read_request(s, "g6/unrelated".into()));
    let r = run.need("read unrelated", r).1;
    let a = read_answer(&r);
    t.add("read_unrelated", a);
    run.check("unrelated read", a == "certified", format_args!("{r:?}"));
    for (probe, cert_id, gate) in [
        ("validate", Some(p.cert_id), None),
        ("gate", None, Some(GateMode::Enforce)),
    ] {
        let r = run.agent_call(check_request(s, cert_id, gate));
        match run.need(probe, r).1 {
            Response::Check {
                validation, gate, ..
            } => {
                t.add(probe, verdict_code(&validation));
                let pos = positive(&validation) || gate == Some(Decision::Allow);
                if truth == Truth::Retracted && pos {
                    t.false_positives += 1;
                    run.fail(
                        "false positive check",
                        format_args!("{probe}: {validation:?} {gate:?}"),
                    );
                }
                if gate.is_some() {
                    t.add("gate_decision", &format!("{gate:?}"));
                }
            }
            Response::Error { code, .. } => t.add(probe, &code),
            other => run.fail(probe, format_args!("{other:?}")),
        }
    }
}
fn export(run: &mut Run, repo: &Path) -> Result<Response, String> {
    let action = MirrorAction::Export {
        branch: None,
        project: repo.display().to_string(),
        force: false,
        adopt: false,
        readable_ok: false,
        rebuild: false,
    };
    run.admin_call(Request::Mirror { action }).map(|(_, r)| r)
}
fn unchanged(r: &Response) -> bool {
    matches!(r, Response::Mirror { report } if report.lines.iter().any(|l| l.contains("unchanged at")))
}
/// Revision 0's base asserted (`true`) or retracted.
fn toggle(on: bool) -> Vec<Op> {
    let rev = RevisionId(0).into();
    vec![match on {
        true => Op::AssertBase { rev },
        false => Op::RetractBase { rev },
    }]
}

fn hub(run: &mut Run) {
    let n = run.args.n;
    run.write_toml(None);
    run.started();

    // Load: n records over one payload, then n − 3 rules, in chunks.
    let content = *blake3::hash(PAYLOAD).as_bytes();
    let t = Instant::now();
    let (mut requests, mut settling_acks) = (0, 0);
    let chunk = run.args.chunk.min(u16::MAX as usize);
    for start in (0..n).step_by(chunk) {
        let end = (start + chunk).min(n);
        let ops = (start..end)
            .map(|k| Op::CreateLogical {
                origin: Default::default(),
                key: format!("shape/{k}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content,
                adopt_as_base: k < 3,
            })
            .collect();
        let r = run.raw(ops, vec![PAYLOAD.to_vec()]);
        let (result, settling) = run.need("load records", r);
        let first = result.allocations.new_revisions.first().copied();
        let r = (first == Some(RevisionId(start as u32)))
            .then_some(())
            .ok_or(format!("{first:?}"));
        run.need("revision ids", r);
        requests += 1;
        settling_acks += usize::from(settling);
    }
    for start in (3..n).step_by(chunk) {
        let end = (start + chunk).min(n);
        let ops = (start..end)
            .map(|c| Op::AddRule {
                conclusion: RevisionId(c as u32).into(),
                incidences: (0..3)
                    .map(|p| (RevisionId(p).into(), EdgeKind::Pin))
                    .collect(),
                enabled: true,
            })
            .collect();
        let r = run.raw(ops, Vec::new());
        let (_, settling) = run.need("load rules", r);
        requests += 1;
        settling_acks += usize::from(settling);
    }
    let load_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let r = run.settled();
    let cp = run.need("settle after load", r);
    run.step(
        "load",
        format_args!(
            "shape=hub n={n} bases=3 rules={} incidences={} chunk={chunk} requests={requests} \
             settling_acks={settling_acks} load_ms={load_ms} settle_ms={} epoch={}",
            n - 3,
            3 * (n - 3),
            t.elapsed().as_millis(),
            cp.epoch.0
        ),
    );

    // A certificate on a hub record, and an unrelated record.
    let body = b"g6 unrelated".to_vec();
    let r = run.raw(
        vec![Op::CreateLogical {
            origin: Default::default(),
            key: "g6/unrelated".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: *blake3::hash(&body).as_bytes(),
            adopt_as_base: true,
        }],
        vec![body],
    );
    run.need("unrelated record", r);
    let r = run.session();
    let session = run.need("session", r);
    let hub_key = format!("shape/{}", n - 1);
    for key in [hub_key.clone(), "g6/unrelated".into()] {
        let r = run.agent_call(read_request(session, key.clone()));
        let r = run.need("certified read", r).1;
        run.check(
            "certified read",
            read_answer(&r) == "certified",
            format_args!("{key}: {r:?}"),
        );
    }
    let r = run.agent_call(check_request(session, None, None));
    let cert_id = match run.need("issue", r).1 {
        Response::Check {
            validation: Validation::Valid,
            cert: Some(c),
            ..
        } => c.cert_id,
        other => run.need("issue", Err(format!("{other:?}"))),
    };
    run.step(
        "certificate",
        format_args!("cert_id={} reads=2 verdict=Valid", hex(&cert_id[..6])),
    );
    let mut probe = Probe {
        session,
        cert_id,
        rng: 0x6736_7036_9e37_79b9,
    };
    let repo = run.evidence.join("mirror-repo");
    let r = Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .map_err(|e| e.to_string())
        .and_then(|s| s.success().then_some(()).ok_or("git init".into()));
    run.need("git init", r);

    // Retract: a deferred ack, then probes while the frontier lasts.
    for (name, truth) in [("retract", Truth::Retracted), ("restore", Truth::Restored)] {
        let t = Instant::now();
        let r = run.raw(toggle(truth == Truth::Restored), Vec::new());
        let (result, settling) = run.need(name, r);
        let ack_ms = t.elapsed().as_secs_f64() * 1e3;
        let mut tally = Tally::default();
        let mut rounds = 0;
        for _ in 0..run.args.rounds {
            let before = tally.frontier;
            probe_round(run, &mut probe, truth, &mut tally);
            rounds += 1;
            if tally.frontier == before {
                break;
            }
        }
        let t = Instant::now();
        let r = run.settled();
        let cp = run.need("settle", r);
        let settle_ms = t.elapsed().as_millis();
        run.check(
            "frontier",
            settling,
            "the ack was not deferred: no frontier (try a smaller --lazy-budget)",
        );
        run.step(
            &format!("{name}_frontier"),
            format_args!(
                "epoch={} settling_ack={settling} ack_ms={ack_ms:.1} rounds={rounds} \
                 settle_ms={settle_ms} settled_epoch={} {}",
                result.epoch.0,
                cp.epoch.0,
                tally.line()
            ),
        );
        // Settled: the eager answers.
        let mut after = Tally::default();
        probe_round(run, &mut probe, truth, &mut after);
        let want = match truth {
            Truth::Retracted => ["read_hub.stale", "validate.Invalid"],
            Truth::Restored => ["read_hub.certified", "validate.Valid"],
        };
        for w in want {
            run.check(
                "settled answer",
                after.answers.contains_key(w),
                format_args!("want {w}: {}", after.line()),
            );
        }
        run.step(&format!("{name}_settled"), after.line());
        run.checkpoint_and_verify(&format!("{name}_settled"));
    }

    // A following write.
    let body = b"g6 next".to_vec();
    let t = Instant::now();
    let r = run.raw(
        vec![Op::CreateLogical {
            origin: Default::default(),
            key: "g6/next".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: *blake3::hash(&body).as_bytes(),
            adopt_as_base: true,
        }],
        vec![body],
    );
    let (result, settling) = run.need("next write", r);
    run.step(
        "next_write",
        format_args!(
            "epoch={} settling_ack={settling} ack_ms={:.1}",
            result.epoch.0,
            t.elapsed().as_secs_f64() * 1e3
        ),
    );

    // A mirror export right after a deferred ack: it pulls to the end (or
    // answers Settling) and renders a settled tree, which an export after
    // the settle leaves unchanged.
    let r = run.raw(toggle(false), Vec::new());
    let (result, settling) = run.need("retract for the mirror", r);
    let t = Instant::now();
    let r = export(run, &repo);
    let first = run.need("mirror export", r);
    let export_ms = t.elapsed().as_millis();
    let answer = match &first {
        Response::Mirror { .. } => "exported",
        Response::Error { code, .. } if code == "Settling" => "settling",
        _ => "other",
    };
    run.check(
        "mirror export",
        answer != "other",
        format_args!("{first:?}"),
    );
    let r = run.settled();
    run.need("settle", r);
    let r = export(run, &repo);
    let again = run.need("mirror export again", r);
    let same = unchanged(&again);
    run.check(
        "mirror rendered from a frontier",
        answer != "exported" || same,
        format_args!("{again:?}"),
    );
    let after = match &again {
        Response::Mirror { .. } if same => "unchanged",
        Response::Mirror { .. } => "changed",
        _ => "other",
    };
    run.check(
        "mirror export after settle",
        after != "other",
        format_args!("{again:?}"),
    );
    run.step(
        "mirror",
        format_args!(
            "epoch={} settling_ack={settling} during={answer} export_ms={export_ms} \
             after_settle={after}",
            result.epoch.0
        ),
    );

    // SIGKILL while a cascade is pending, then a restart.
    let r = run.raw(toggle(true), Vec::new());
    let (result, settling) = run.need("restore before kill", r);
    let mut child = run.ctxd.take().expect("ctxd");
    let r = child.kill().map_err(|e| e.to_string());
    run.need("SIGKILL", r);
    let status = child.wait().map(|s| format!("{s}")).unwrap_or_default();
    run.admin = None;
    run.agent = None;
    run.check("frontier at kill", settling, "the ack was not deferred");
    run.step(
        "kill",
        format_args!(
            "signal=SIGKILL epoch={} settling_ack={settling} status={status:?}",
            result.epoch.0
        ),
    );
    let r = run.start();
    let ms = run.need("restart ctxd", r);
    let (acked, present) = run.acked_present();
    run.step(
        "restart",
        format_args!("restart_ms={ms:.0} acked={acked} present={present}"),
    );
    run.checkpoint_and_verify("restart");
}

// ----------------------------------------------------------------- gc

fn body(i: usize, version: u8) -> Vec<u8> {
    format!("p11 g6 gc payload {i} v{version} {:<96}", "").into_bytes()
}
fn blob_bytes(dir: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                _ => e.metadata().map_or(0, |m| m.len()),
            })
            .sum()
    }
    walk(&dir.join("blobs"))
}
/// `(payload, unavailable)` of `key@rev`, read Advisory.
fn read_rev(
    run: &mut Run,
    s: SessionId,
    key: &str,
    rev: RevisionId,
) -> (Option<Vec<u8>>, Option<String>) {
    let req = Request::Read {
        binding: Binding::root(s),
        target: Target::KeyRev {
            key: key.into(),
            rev,
        },
        mode: Mode::Advisory,
        level: None,
    };
    match run.agent_call(req) {
        Ok((
            _,
            Response::Read {
                payload,
                unavailable,
                ..
            },
        )) => (payload, unavailable),
        other => {
            run.fail("read", format_args!("{key}@{}: {other:?}", rev.0));
            (None, None)
        }
    }
}
/// The checkpoint's epoch and digests, to compare two checkpoints.
fn digests(cp: &Value) -> (Value, Value) {
    let b: Vec<Value> = (cp["branches"].as_array().into_iter().flatten())
        .map(|b| {
            Value::Array(vec![
                b["kernel_digest"].clone(),
                b["derived_digest"].clone(),
            ])
        })
        .collect();
    (cp["epoch"].clone(), Value::Array(b))
}
/// The number before `word` in `text` ("12 payload(s)" → 12).
fn number_before(text: &str, word: &str) -> Option<u64> {
    let at = text.find(word)?;
    text[..at].split_whitespace().last()?.parse().ok()
}
/// The number after `word` in `text` ("collected 12 payload(s)" → 12).
fn number_after(text: &str, word: &str) -> Option<u64> {
    let at = text.find(word)? + word.len();
    let word = text[at..].split_whitespace().next()?;
    word.trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

fn gc(run: &mut Run) {
    let n = run.args.n;
    let superseded = n / 4;
    run.write_toml(Some(100));
    run.started();
    let r = run.session();
    let s = run.need("session", r);

    // n records with distinct payloads; a quarter superseded.
    let mut first = Vec::with_capacity(n);
    for start in (0..n).step_by(500) {
        let end = (start + 500).min(n);
        let blobs: Vec<Vec<u8>> = (start..end).map(|i| body(i, 1)).collect();
        let ops = (start..end)
            .zip(&blobs)
            .map(|(i, b)| Op::CreateLogical {
                origin: Default::default(),
                key: format!("gc/{i}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: *blake3::hash(b).as_bytes(),
                adopt_as_base: false,
            })
            .collect();
        let r = run.raw(ops, blobs);
        first.extend(run.need("create", r).0.allocations.new_revisions);
    }
    for start in (0..superseded).step_by(500) {
        let end = (start + 500).min(superseded);
        let blobs: Vec<Vec<u8>> = (start..end).map(|i| body(i, 2)).collect();
        let mut ops = Vec::new();
        for (j, (i, b)) in (start..end).zip(&blobs).enumerate() {
            ops.push(Op::AddRevision {
                logical: LogicalRef::Key(format!("gc/{i}")),
                content: *blake3::hash(b).as_bytes(),
                adopt_as_base: false,
            });
            ops.push(Op::SetHead {
                logical: LogicalRef::Key(format!("gc/{i}")),
                expected: first[i].into(),
                new: RevRef::New(j as u16),
            });
        }
        let r = run.raw(ops, blobs);
        run.need("supersede", r);
    }
    // `gc/pinned`: a session certificate names v1, then v1 is superseded and
    // loses its base bit, so only the certificate keeps it (K4).
    let (pinned_v1, pinned_v2) = (b"g6 pinned v1".to_vec(), b"g6 pinned v2".to_vec());
    let r = run.raw(
        vec![Op::CreateLogical {
            origin: Default::default(),
            key: "gc/pinned".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: *blake3::hash(&pinned_v1).as_bytes(),
            adopt_as_base: true,
        }],
        vec![pinned_v1.clone()],
    );
    let pinned1 = run.need("pinned", r).0.allocations.new_revisions[0];
    let r = run.agent_call(read_request(s, "gc/pinned".into()));
    let r = run.need("pinned read", r).1;
    run.check(
        "pinned read",
        read_answer(&r) == "certified",
        format_args!("{r:?}"),
    );
    let r = run.agent_call(check_request(s, None, None));
    let pinned_cert = matches!(run.need("pinned cert", r).1, Response::Check { cert: Some(ref c), .. } if c.vector.iter().any(|v| v.rev == pinned1));
    run.check(
        "pinned cert",
        pinned_cert,
        "the certificate names gc/pinned v1",
    );
    let r = run.raw(
        vec![
            Op::AddRevision {
                logical: LogicalRef::Key("gc/pinned".into()),
                content: *blake3::hash(&pinned_v2).as_bytes(),
                adopt_as_base: false,
            },
            Op::SetHead {
                logical: LogicalRef::Key("gc/pinned".into()),
                expected: pinned1.into(),
                new: RevRef::New(0),
            },
            Op::RetractBase {
                rev: pinned1.into(),
            },
        ],
        vec![pinned_v2],
    );
    run.need("pinned supersede", r);
    // 110 more epochs: the superseded v1s fall behind keep_epochs = 100.
    for i in 0..110 {
        let b = format!("g6 filler {i}").into_bytes();
        let r = run.raw(
            vec![Op::CreateLogical {
                origin: Default::default(),
                key: format!("filler/{i}"),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: *blake3::hash(&b).as_bytes(),
                adopt_as_base: false,
            }],
            vec![b],
        );
        run.need("filler", r);
    }
    let bytes0 = blob_bytes(&run.data);
    run.step(
        "load",
        format_args!(
            "shape=records n={n} superseded={superseded} pinned=1 filler=110 keep_epochs=100 \
             blob_bytes={bytes0}"
        ),
    );
    let before = run.checkpoint_and_verify("before_gc");

    // Dry run, then apply, through the CLI. A snapshot a reader still holds
    // (K5: the candidate generator walks an early one) can set the cutoff;
    // the dry run is repeated until keep_epochs does.
    let data = run.data.display().to_string();
    let t = Instant::now();
    let mut polls = 0;
    let (code, out) = loop {
        let r = run.ctx(&["gc", &data, "--keep-epochs", "100"]);
        let (code, out) = run.need("ctx gc", r);
        polls += 1;
        if code != 0 || out.contains("set by keep_epochs") || t.elapsed() > START_TIMEOUT {
            break (code, out);
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    let _ = std::fs::write(run.evidence.join("gc-dry-run.txt"), &out);
    let dry = number_before(&out, "payload(s) collectable");
    run.check("dry run", code == 0 && dry == Some(superseded as u64), &out);
    let cutoff = number_after(&out, "cutoff epoch");
    run.step(
        "gc_dry_run",
        format_args!(
            "exit={code} collectable={dry:?} cutoff={cutoff:?} want={superseded} polls={polls} \
             k5_wait_ms={}",
            t.elapsed().as_millis()
        ),
    );
    let r = run.ctx(&["gc", &data, "--apply", "--keep-epochs", "100"]);
    let (code, out) = run.need("ctx gc --apply", r);
    let _ = std::fs::write(run.evidence.join("gc-apply.txt"), &out);
    let collected = number_after(&out, "collected");
    run.check(
        "apply",
        code == 0 && collected == Some(superseded as u64),
        &out,
    );

    // Collected, pinned and kept reads; MissingBlob without the bytes.
    let (p, u) = read_rev(run, s, "gc/0", first[0]);
    let collected_read = p.is_none() && u.as_deref() == Some("collected");
    run.check("collected read", collected_read, format_args!("{u:?}"));
    let (p, _) = read_rev(run, s, "gc/pinned", pinned1);
    let pinned_read = p.as_deref() == Some(&b"g6 pinned v1"[..]);
    run.check("pinned readable", pinned_read, "pinned v1 after apply");
    let kept = n - 1;
    let (p, _) = read_rev(run, s, &format!("gc/{kept}"), first[kept]);
    let kept_read = p == Some(body(kept, 1));
    run.check("kept readable", kept_read, "an unsuperseded head");
    let head0 = match run.agent_call(Request::Read {
        binding: Binding::root(s),
        target: Target::Key("gc/0".into()),
        mode: Mode::Advisory,
        level: None,
    }) {
        Ok((_, Response::Read { item, .. })) => Some((item.logical, item.rev)),
        other => {
            run.fail("read gc/0 head", format_args!("{other:?}"));
            None
        }
    };
    let set_back = |(l, head): (LogicalId, RevisionId)| {
        vec![Op::SetHead {
            logical: l.into(),
            expected: head.into(),
            new: first[0].into(),
        }]
    };
    let commit = |ops: Vec<Op>, txn: [u8; 16]| Request::Commit {
        binding: Binding::root(s),
        ops,
        blobs: Vec::new(),
        expect_cert_valid: false,
        client_txn_id: Some(txn),
    };
    let missing = match head0 {
        Some(h) => {
            let txn = run.txn();
            let r = run.agent_call(commit(set_back(h), txn));
            matches!(r, Ok((_, Response::Error { ref code, .. })) if code == "MissingBlob")
        }
        None => false,
    };
    run.check(
        "MissingBlob",
        missing,
        "SetHead to a collected revision without its bytes",
    );
    let after = run.checkpoint_and_verify("after_apply");
    run.check(
        "digests after apply",
        digests(&after) == digests(&before),
        "epoch or digests changed",
    );
    run.step(
        "gc_apply",
        format_args!(
            "exit={code} collected={collected:?} collected_read={collected_read} pinned_read={pinned_read} kept_read={kept_read} \
             missing_blob={missing} same_epoch_digests={}",
            digests(&after) == digests(&before)
        ),
    );

    // Offline compaction with ctxd stopped, then a restart.
    let r = run.admin_call(Request::Shutdown);
    run.need("shutdown", r);
    if let Some(mut c) = run.ctxd.take() {
        let _ = c.wait();
    }
    run.admin = None;
    run.agent = None;
    let bytes1 = blob_bytes(&run.data);
    let r = run.ctx(&["gc", &data, "--compact"]);
    let (code, out) = run.need("ctx gc --compact", r);
    let _ = std::fs::write(run.evidence.join("gc-compact.txt"), &out);
    let dropped = number_after(&out, "segment(s):");
    let reclaimed = number_after(&out, "dropped,");
    run.check(
        "compact",
        code == 0 && dropped >= Some(superseded as u64),
        &out,
    );
    let bytes2 = blob_bytes(&run.data);
    let r = run.start();
    let ms = run.need("restart", r);
    run.step(
        "gc_compact",
        format_args!(
            "exit={code} records_dropped={dropped:?} bytes_reclaimed={reclaimed:?} \
             blob_bytes_before={bytes1} blob_bytes_after={bytes2} restart_ms={ms:.0}"
        ),
    );
    let compacted = run.checkpoint_and_verify("after_compact");
    run.check(
        "digests after compact",
        digests(&compacted) == digests(&before),
        "epoch or digests changed",
    );
    let r = run.session();
    let s = run.need("session", r);
    let (p, u) = read_rev(run, s, "gc/0", first[0]);
    let collected_restart = p.is_none() && u.as_deref() == Some("collected");
    run.check(
        "collected after restart",
        collected_restart,
        format_args!("{u:?}"),
    );
    let (p, _) = read_rev(run, s, "gc/pinned", pinned1);
    let pinned_ok = p.as_deref() == Some(&b"g6 pinned v1"[..]);
    run.check("pinned after restart", pinned_ok, "pinned v1");

    // Re-staging: supersede back to the collected text sends its bytes.
    let r = run.agent_call(Request::Read {
        binding: Binding::root(s),
        target: Target::Key("gc/0".into()),
        mode: Mode::Advisory,
        level: None,
    });
    let head = match run.need("read gc/0", r).1 {
        Response::Read { item, .. } => (item.logical, item.rev),
        other => run.need("read gc/0", Err(format!("{other:?}"))),
    };
    let txn = run.txn();
    let r = run.agent_call(Request::Supersede {
        binding: Binding::root(s),
        logical: head.0,
        body: body(0, 1),
        expected_head: head.1,
        adopt_as_base: false,
        carry_rules: false,
        reason: "p11_g6 re-stage".into(),
        client_txn_id: Some(txn),
    });
    let restaged = matches!(run.need("re-stage", r).1, Response::Committed { .. });
    let (p, u) = read_rev(run, s, "gc/0", first[0]);
    let readable = p == Some(body(0, 1));
    run.check("re-staged", restaged && readable, format_args!("{u:?}"));
    run.step(
        "restage",
        format_args!(
            "collected_after_restart={collected_restart} pinned_after_restart={pinned_ok} restaged={restaged} \
             readable_again={readable}"
        ),
    );
    run.checkpoint_and_verify("after_restage");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
