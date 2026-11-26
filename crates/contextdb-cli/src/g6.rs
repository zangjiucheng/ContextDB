//! P11 G6 B: `ctx g6 checkpoint` asks a running ctxd for a checkpoint and
//! saves it as canonical JSON; `ctx g6 verify` replays the checkpoint's
//! op_log prefix eagerly in an isolated copy and compares every field.
//!
//! The live data directory is only read: verify copies the op_log segments,
//! truncates the copy at the checkpoint's epoch, copies `blobs/`, `keys/`
//! and `gc/`, and opens the copy (never the live store, whose lock ctxd
//! holds) with an eager kernel.
use crate::p4::{connect, flag};
use contextdb_daemon::service::{checkpoint_branches, checkpoint_check};
use contextdb_model::*;
use contextdb_proto::{gate::verdict_code, proto::*};
use contextdb_store::{
    OpenOptions, Payload, ProjectionFile, StdDisk, Store, Unavailable,
    log::{self, FORMAT_VERSION},
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const USAGE: &str = "usage: ctx g6 checkpoint <dir> [--out FILE] [--live-pos SCOPE=HEX]... | \
                     ctx g6 verify <dir> <checkpoint.json> [--work DIR]";
/// The `format` of a checkpoint file.
const FORMAT: &str = "contextdb.g6.checkpoint/1";
/// How long `checkpoint` asks again while ctxd answers `Settling`.
const SETTLE_DEADLINE: Duration = Duration::from_secs(600);
/// Unavailable head payloads listed by `verify`, at most.
const LISTED: usize = 20;

pub fn run(args: &[String]) -> Result<u8, String> {
    let dir = |n: usize| args.get(n).map(Path::new).ok_or(USAGE);
    match args.first().map(String::as_str) {
        Some("checkpoint") => checkpoint(dir(1)?, &args[2.min(args.len())..]),
        Some("verify") => {
            let rest = &args[3.min(args.len())..];
            if let Some(a) = rest.iter().step_by(2).find(|a| *a != "--work") {
                return Err(format!("unknown argument {a}; {USAGE}"));
            }
            verify(dir(1)?, dir(2)?, flag(rest, "--work").map(PathBuf::from))
        }
        _ => Err(USAGE.into()),
    }
}

fn checkpoint(dir: &Path, args: &[String]) -> Result<u8, String> {
    let mut live_pos: Option<Vec<(String, Vec<u8>)>> = None;
    let mut out = None;
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--out" => out = Some(PathBuf::from(rest.next().ok_or(USAGE)?)),
            "--live-pos" => {
                let v = rest.next().ok_or(USAGE)?;
                let (scope, pos) =
                    (v.split_once('=')).ok_or(format!("--live-pos {v}: SCOPE=HEX"))?;
                let pos = unhex(pos).ok_or(format!("--live-pos {v}: not hex"))?;
                live_pos
                    .get_or_insert_with(Vec::new)
                    .push((scope.into(), pos));
            }
            other => return Err(format!("unknown argument {other}; {USAGE}")),
        }
    }
    if let Some(l) = live_pos.as_mut() {
        l.sort();
    }
    let deadline = Instant::now() + SETTLE_DEADLINE;
    let report = loop {
        let mut c = connect(dir, "admin.sock")?;
        let request = Request::Checkpoint {
            live_pos: live_pos.clone(),
        };
        let env = c.call(&request).map_err(|e| format!("{e:?}"))?;
        match env.response {
            Response::Checkpoint { report } => break report,
            Response::Error { code, detail } if code == "Settling" => {
                if Instant::now() > deadline {
                    return Err(format!(
                        "Settling: {detail} (gave up after {SETTLE_DEADLINE:?})"
                    ));
                }
                eprintln!("ctx g6: {detail}");
            }
            Response::Error { code, detail } => return Err(format!("{code}: {detail}")),
            other => return Err(format!("unexpected response {other:?}")),
        }
    };
    let now = utc(SystemTime::now());
    let toml = match std::fs::read_to_string(dir.join(contextdb_daemon::config::FILE)) {
        Ok(t) => Value::String(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => return Err(format!("ctxd.toml: {e}")),
    };
    let data_dir = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    // The file at the path ctxd started from, as it is now: it differs from
    // `ctxd.exe_blake3` when the installed binary was replaced since.
    let file_now = file_blake3(Path::new(&report.exe));
    let meta = json!({
        "taken_at_utc": now,
        "data_dir": data_dir.display().to_string(),
        "ctxd_toml_now": toml,
        "exe_file_blake3_now": file_now,
    });
    let value = render(&report, meta);
    let out = out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "g6-checkpoint-e{}-{}.json",
            report.epoch.0,
            now.replace([':', '-'], "")
        ))
    });
    let text = serde_json::to_string_pretty(&value).expect("json") + "\n";
    std::fs::write(&out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    let counts = &value["counts"];
    println!(
        "g6 checkpoint epoch={} branches={} certs={} non_empty={} history_unavailable={} \
         ctxd_pid={} ctxd_blake3={} exe_file_now={} out={}",
        report.epoch.0,
        report.branches.len(),
        counts["certs"],
        counts["non_empty"],
        counts["history_unavailable"],
        report.pid,
        report.exe_blake3,
        value["meta"]["exe_file_blake3_now"],
        out.display()
    );
    Ok(0)
}

/// The BLAKE3 of the file at `path` (`unavailable: <reason>`).
pub fn file_blake3(path: &Path) -> String {
    let mut h = blake3::Hasher::new();
    let r = std::fs::File::open(path).and_then(|mut f| std::io::copy(&mut f, &mut h));
    match r {
        Ok(_) => h.finalize().to_hex().to_string(),
        Err(e) => format!("unavailable: {e}"),
    }
}
/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
fn utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (H. Hinnant), days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Option<Vec<u8>> {
    (s.len().is_multiple_of(2) && s.is_ascii())
        .then(|| {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
                .collect()
        })
        .flatten()
}
fn hash32(s: &str) -> Option<ContentHash> {
    unhex(s)?.try_into().ok()
}
/// Objects with their keys in sorted order, whatever the map's order.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: BTreeMap<String, Value> =
                m.into_iter().map(|(k, v)| (k, canonical(v))).collect();
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}
fn verdict_name(c: &CertCheck) -> &'static str {
    match c {
        CertCheck::Verdict(v) => verdict_code(v),
        CertCheck::HistoryUnavailable { .. } => "HistoryUnavailable",
    }
}
/// The checkpoint file: `report` as JSON, its counts, and `meta` (what the
/// CLI added: time, directory, the ctxd.toml read then).
pub fn render(r: &CheckpointReport, meta: Value) -> Value {
    let branches: Vec<Value> = (r.branches.iter())
        .map(|b| {
            json!({
                "id": b.id.0,
                "name": b.name,
                "kernel_digest": hex(&b.kernel_digest),
                "derived_digest": hex(&b.derived_digest),
            })
        })
        .collect();
    let mut by_verdict: BTreeMap<&str, u64> = BTreeMap::new();
    let certs: Vec<Value> = (r.certs.iter())
        .map(|c| {
            *by_verdict.entry(verdict_name(&c.check)).or_default() += 1;
            let live: Map<String, Value> = (c.live_pos.iter())
                .map(|(s, p)| (s.clone(), Value::String(hex(p))))
                .collect();
            let (verdict_wire, since) = match &c.check {
                CertCheck::Verdict(v) => (Value::String(hex(&v.canonical_bytes())), Value::Null),
                CertCheck::HistoryUnavailable { since } => (Value::Null, json!(since.0)),
            };
            json!({
                "cert_id": hex(&c.cert.cert_id),
                "sources": c.sources,
                "principal": c.cert.principal.0,
                "branch": c.cert.branch.0,
                "issued_epoch": c.cert.issued_epoch.0,
                "strict_no_aba": c.cert.strict_no_aba,
                "entries": c.cert.entries.len(),
                "live_pos": live,
                "cert": hex(&c.cert.canonical_bytes()),
                "verdict": verdict_name(&c.check),
                "verdict_wire": verdict_wire,
                "history_unavailable_since": since,
            })
        })
        .collect();
    let checked: u64 = (by_verdict.iter())
        .filter(|(k, _)| **k != "HistoryUnavailable")
        .map(|(_, n)| n)
        .sum();
    canonical(json!({
        "format": FORMAT,
        "meta": meta,
        "epoch": r.epoch.0,
        "log_checksum": hex(&r.log_checksum),
        "protocol": r.protocol,
        "log_format": r.log_format,
        "ctxd": {
            "pid": r.pid,
            "exe": r.exe,
            "exe_blake3": r.exe_blake3,
            "lazy_budget": r.lazy_budget,
            "read_settle_ms": r.read_settle_ms,
            "keep_epochs": r.keep_epochs,
        },
        "live_pos_given": r.live_pos_given,
        "branches": branches,
        "certs": certs,
        "counts": {
            "certs": r.certs.len(),
            "non_empty": checked - by_verdict.get("Empty").copied().unwrap_or(0),
            "history_unavailable": by_verdict.get("HistoryUnavailable").copied().unwrap_or(0),
            "by_verdict": by_verdict,
        },
    }))
}
/// The report a checkpoint file holds (its `meta` is not part of it). The
/// readable copies of certificate fields are not read back: [`verify`]
/// compares the whole file with its own rendering instead.
pub fn parse(v: &Value) -> Result<CheckpointReport, String> {
    if v["format"] != FORMAT {
        return Err(format!("not a {FORMAT} file (format {})", v["format"]));
    }
    let u = |v: &Value, what: &str| v.as_u64().ok_or(format!("{what}: expected a number"));
    let s = |v: &Value, what: &str| {
        (v.as_str().map(str::to_owned)).ok_or(format!("{what}: expected a string"))
    };
    let h = |v: &Value, what: &str| {
        (v.as_str().and_then(hash32)).ok_or(format!("{what}: expected 32 hex bytes"))
    };
    let arr =
        |v: &Value, what: &str| (v.as_array().cloned()).ok_or(format!("{what}: expected an array"));
    let ctxd = &v["ctxd"];
    let mut branches = Vec::new();
    for (n, b) in arr(&v["branches"], "branches")?.iter().enumerate() {
        let what = format!("branches[{n}]");
        branches.push(CheckpointBranch {
            id: BranchId(u16::try_from(u(&b["id"], &what)?).map_err(|e| e.to_string())?),
            name: s(&b["name"], &what)?,
            kernel_digest: h(&b["kernel_digest"], &what)?,
            derived_digest: h(&b["derived_digest"], &what)?,
        });
    }
    let mut certs = Vec::new();
    for (n, c) in arr(&v["certs"], "certs")?.iter().enumerate() {
        let what = format!("certs[{n}]");
        let bytes = (c["cert"].as_str().and_then(unhex)).ok_or(format!("{what}.cert: not hex"))?;
        let cert = Certificate::from_bytes(&bytes).map_err(|e| format!("{what}.cert: {e}"))?;
        let mut live_pos = Vec::new();
        for (scope, pos) in (c["live_pos"].as_object()).ok_or(format!("{what}.live_pos"))? {
            let pos = (pos.as_str().and_then(unhex)).ok_or(format!("{what}.live_pos: not hex"))?;
            live_pos.push((scope.clone(), pos));
        }
        live_pos.sort();
        let check = match (&c["verdict_wire"], &c["history_unavailable_since"]) {
            (Value::String(w), Value::Null) => {
                let w = unhex(w).ok_or(format!("{what}.verdict_wire: not hex"))?;
                CertCheck::Verdict(
                    Validation::from_bytes(&w).map_err(|e| format!("{what}.verdict_wire: {e}"))?,
                )
            }
            (Value::Null, since) => CertCheck::HistoryUnavailable {
                since: Epoch(u(since, &what)?),
            },
            _ => return Err(format!("{what}: a verdict or history_unavailable_since")),
        };
        let sources = arr(&c["sources"], &what)?;
        certs.push(CheckpointCert {
            sources: (sources.iter())
                .map(|x| s(x, &what))
                .collect::<Result<_, _>>()?,
            cert,
            live_pos,
            check,
        });
    }
    Ok(CheckpointReport {
        epoch: Epoch(u(&v["epoch"], "epoch")?),
        log_checksum: h(&v["log_checksum"], "log_checksum")?,
        protocol: u16::try_from(u(&v["protocol"], "protocol")?).map_err(|e| e.to_string())?,
        log_format: u32::try_from(u(&v["log_format"], "log_format")?).map_err(|e| e.to_string())?,
        lazy_budget: match &ctxd["lazy_budget"] {
            Value::Null => None,
            b => Some(u(b, "ctxd.lazy_budget")?),
        },
        read_settle_ms: u(&ctxd["read_settle_ms"], "ctxd.read_settle_ms")?,
        keep_epochs: u(&ctxd["keep_epochs"], "ctxd.keep_epochs")?,
        pid: u32::try_from(u(&ctxd["pid"], "ctxd.pid")?).map_err(|e| e.to_string())?,
        exe: s(&ctxd["exe"], "ctxd.exe")?,
        exe_blake3: s(&ctxd["exe_blake3"], "ctxd.exe_blake3")?,
        live_pos_given: (v["live_pos_given"].as_bool()).ok_or("live_pos_given: expected a bool")?,
        branches,
        certs,
    })
}

/// Every leaf where `got` (the file) differs from `want` (the replay).
fn diff(path: &str, got: &Value, want: &Value, out: &mut Vec<String>) {
    let short = |v: &Value| {
        let t = v.to_string();
        match t.len() > 140 {
            true => format!("{}…", &t[..140]),
            false => t,
        }
    };
    match (got, want) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for k in keys {
                let p = format!("{path}.{k}");
                diff(
                    &p,
                    a.get(k).unwrap_or(&Value::Null),
                    b.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (n, (x, y)) in a.iter().zip(b).enumerate() {
                diff(&format!("{path}[{n}]"), x, y, out);
            }
        }
        _ if got == want => {}
        _ => out.push(format!(
            "{path}: checkpoint={} replay={}",
            short(got),
            short(want)
        )),
    }
}

/// The live store's op_log through `epoch`, copied into `work/log`: whole
/// segments, the last one cut after the frame of `epoch`. Returns that
/// frame's checksum.
fn copy_prefix(live: &Path, work: &Path, epoch: Epoch) -> Result<ContentHash, String> {
    let io = |what: &str, e: std::io::Error| format!("{what}: {e}");
    let (from, to) = (live.join("log"), work.join("log"));
    std::fs::create_dir_all(&to).map_err(|e| io("work/log", e))?;
    if epoch.0 == 0 {
        return Ok(log::GENESIS);
    }
    let disk = StdDisk::default();
    for seg in log::segments(&disk, &from).map_err(|e| io("log", e))? {
        let name = seg.file_name().expect("segment name");
        std::fs::copy(&seg, to.join(name)).map_err(|e| io(&seg.display().to_string(), e))?;
    }
    let scan = log::scan(&disk, &to).map_err(|e| io("scan", e))?;
    let Some(frame) = scan.frames.iter().find(|f| f.row.epoch == epoch) else {
        let tail = scan.frames.last().map_or(0, |f| f.row.epoch.0);
        let why = match (scan.broken, scan.frames.first()) {
            (Some((at, fault)), _) => format!("the log is broken at epoch {} ({fault:?})", at.0),
            (None, Some(f)) if f.row.epoch.0 != 1 => "the log does not start at epoch 1".into(),
            _ => format!("its verified tail is epoch {tail}"),
        };
        return Err(format!(
            "epoch {} is not in the live op_log: {why}",
            epoch.0
        ));
    };
    let g = frame.row.group;
    if g.idx + 1 != g.count {
        return Err(format!("epoch {} ends inside a commit group", epoch.0));
    }
    let file = std::fs::OpenOptions::new().write(true).open(&frame.segment);
    (file.and_then(|f| f.set_len(frame.end))).map_err(|e| io("truncate", e))?;
    for seg in log::segments(&disk, &to).map_err(|e| io("work/log", e))? {
        if seg > frame.segment {
            std::fs::remove_file(&seg).map_err(|e| io("work/log", e))?;
        }
    }
    Ok(frame.checksum)
}
/// Copies the regular files under `from` (if any) to `to`.
fn copy_tree(from: &Path, to: &Path) -> Result<u64, String> {
    let io = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(0);
    };
    std::fs::create_dir_all(to).map_err(|e| io(to, e))?;
    let mut bytes = 0;
    for e in entries {
        let e = e.map_err(|e| io(from, e))?;
        let t = e.file_type().map_err(|x| io(&e.path(), x))?;
        if t.is_dir() {
            bytes += copy_tree(&e.path(), &to.join(e.file_name()))?;
        } else if t.is_file() {
            bytes +=
                std::fs::copy(e.path(), to.join(e.file_name())).map_err(|x| io(&e.path(), x))?;
        }
    }
    Ok(bytes)
}
/// A fresh private directory for the copy: `--work` (absent or empty), or
/// a temporary one that is removed afterwards.
fn work_dir(live: &Path, work: Option<PathBuf>) -> Result<(PathBuf, bool), String> {
    use std::os::unix::fs::DirBuilderExt;
    let (dir, temporary) = match work {
        Some(w) => (w, false),
        None => {
            let nanos = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let name = format!("ctx-g6-verify-{}-{nanos}", std::process::id());
            (std::env::temp_dir().join(name), true)
        }
    };
    // Resolved before anything is created, so an overlapping `--work` never
    // writes inside the data directory.
    let err = |p: &Path, e: std::io::Error| format!("{}: {e}", p.display());
    let name = dir
        .file_name()
        .ok_or(format!("--work {}: a directory name", dir.display()))?;
    let parent = match dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let overlap = |d: &Path| match d.starts_with(live) || live.starts_with(d) {
        true => Err(format!(
            "--work {} overlaps the data directory",
            d.display()
        )),
        false => Ok(()),
    };
    let dir = parent
        .canonicalize()
        .map_err(|e| err(parent, e))?
        .join(name);
    overlap(&dir)?;
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(err(&dir, e)),
        _ => {}
    }
    // An existing `--work` may be a symlink.
    let dir = dir.canonicalize().map_err(|e| err(&dir, e))?;
    overlap(&dir)?;
    if std::fs::read_dir(&dir)
        .map_err(|e| e.to_string())?
        .next()
        .is_some()
    {
        return Err(format!("--work {} is not empty", dir.display()));
    }
    Ok((dir, temporary))
}

/// What one verify found.
#[derive(Default)]
struct Found {
    mismatches: Vec<String>,
    errors: Vec<String>,
}
fn verify(dir: &Path, file: &Path, work: Option<PathBuf>) -> Result<u8, String> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
    let cp = parse(&value)?;
    let live = dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let (work, temporary) = work_dir(&live, work)?;
    let mut found = Found::default();
    let replayed = replay(&live, &work, &cp, &mut found);
    if let Err(e) = &replayed {
        found.errors.push(e.clone());
    }
    if let Ok(r) = &replayed {
        let want = render(r, value["meta"].clone());
        diff("", &value, &want, &mut found.mismatches);
        for (c, w) in cp.certs.iter().zip(&r.certs) {
            if c.check != w.check {
                println!(
                    "mismatch cert {}: checkpoint={:?} replay={:?}",
                    hex(&c.cert.cert_id),
                    c.check,
                    w.check
                );
            }
        }
    }
    for m in &found.mismatches {
        println!("mismatch {m}");
    }
    for e in &found.errors {
        println!("error {e}");
    }
    let counts = &value["counts"];
    println!(
        "g6 verify epoch={} branches={} certs={} non_empty={} mismatches={} errors={}",
        cp.epoch.0,
        cp.branches.len(),
        counts["certs"],
        counts["non_empty"],
        found.mismatches.len(),
        found.errors.len()
    );
    if temporary {
        let _ = std::fs::remove_dir_all(&work);
    } else {
        println!("work copy kept in {}", work.display());
    }
    Ok(u8::from(
        !found.mismatches.is_empty() || !found.errors.is_empty(),
    ))
}
/// The checkpoint recomputed from an eager replay of the copied prefix;
/// an error when the copy cannot be made or replayed.
fn replay(
    live: &Path,
    work: &Path,
    cp: &CheckpointReport,
    found: &mut Found,
) -> Result<CheckpointReport, String> {
    let checksum =
        copy_prefix(live, work, cp.epoch).map_err(|e| format!("copy op_log prefix: {e}"))?;
    if checksum != cp.log_checksum {
        // The comparison below reports the field; nothing after it holds.
        println!(
            "prefix: the live op_log's checksum at epoch {} is {}, the checkpoint's {}",
            cp.epoch.0,
            hex(&checksum),
            hex(&cp.log_checksum)
        );
    }
    let mut bytes = 0;
    for d in ["blobs", "keys", "gc"] {
        bytes += copy_tree(&live.join(d), &work.join(d)).map_err(|e| format!("copy {d}: {e}"))?;
    }
    let options = OpenOptions {
        strict_tail: true,
        projections: ProjectionFile::Memory,
        create: false,
        lazy: None,
        ..OpenOptions::default()
    };
    let t = Instant::now();
    let (store, report) = Store::open(StdDisk { full_sync: false }, work, &options)
        .map_err(|e| format!("eager replay of the copy: {e:?}"))?;
    let k = store.kernel().map_err(|e| format!("kernel: {e:?}"))?;
    println!(
        "replay epoch={} frames_replayed={} snapshot={:?} blob_bytes_copied={bytes} ms={}",
        store.epoch().0,
        report.replayed,
        report.snapshot_epoch.map(|e| e.0),
        t.elapsed().as_millis()
    );
    if let Some((_, e)) = k.frontier() {
        return Err(format!("the eager replay left a frontier at epoch {}", e.0));
    }
    let mut r = cp.clone();
    r.epoch = store.epoch();
    r.log_checksum = store.checksum_at(r.epoch).unwrap_or_default();
    r.protocol = PROTOCOL_VERSION;
    r.log_format = FORMAT_VERSION;
    r.branches = checkpoint_branches(k);
    let at = |e: Epoch| store.checksum_at(e).ok();
    for c in &mut r.certs {
        let view = (k.branch(c.cert.branch)).unwrap_or_else(|| k.branch(MAIN).expect("main"));
        let live: BTreeMap<String, Vec<u8>> = c.live_pos.iter().cloned().collect();
        c.check = checkpoint_check(&view, r.epoch, &at, &c.cert, &live);
    }
    payloads(&store, k, found);
    Ok(r)
}
/// Every head on every branch must have its payload (shredded ones are
/// counted, not failed).
fn payloads(store: &Store<StdDisk>, k: &contextdb_core::Kernel, found: &mut Found) {
    let mut heads: BTreeMap<ContentHash, String> = BTreeMap::new();
    let mut count = 0u64;
    for info in k.branches() {
        let v = k.branch(info.id).expect("listed");
        for l in v.logicals() {
            if let (Some(h), Some((key, _, _))) = (v.head(l), v.logical_info(l))
                && let Ok(c) = v.revision_content(h)
            {
                count += 1;
                heads
                    .entry(c)
                    .or_insert_with(|| format!("{key}@{} on {}", h.0, info.name));
            }
        }
    }
    let (mut available, mut shredded, mut unavailable) = (0, 0, 0);
    for (content, head) in &heads {
        match store.get_blob(content) {
            Ok(Payload::Available(_)) => available += 1,
            Ok(Payload::Unavailable(Unavailable::Shredded)) => shredded += 1,
            other => {
                unavailable += 1;
                if unavailable <= LISTED {
                    let why = match other {
                        Ok(Payload::Unavailable(u)) => format!("{u:?}"),
                        other => format!("{other:?}"),
                    };
                    found
                        .errors
                        .push(format!("head payload {why}: {} ({head})", hex(content)));
                }
            }
        }
    }
    if unavailable > LISTED {
        found.errors.push(format!(
            "{} more unavailable head payloads",
            unavailable - LISTED
        ));
    }
    println!(
        "payloads heads={count} contents={} available={available} shredded={shredded} \
         unavailable={unavailable}",
        heads.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> CheckpointReport {
        let cert = |id: u8, branch: BranchId| Certificate {
            cert_id: [id; 32],
            session_id: [6; 16],
            principal: PrincipalId(3),
            branch,
            issued_epoch: Epoch(3),
            log_checksum_at_issue: [7; 32],
            code_commit: "c0ffee".into(),
            watermarks: vec![("repo".into(), vec![1, 2])],
            entries: Vec::new(),
            vector: Vec::new(),
            strict_no_aba: true,
        };
        CheckpointReport {
            epoch: Epoch(12),
            log_checksum: [4; 32],
            protocol: PROTOCOL_VERSION,
            log_format: FORMAT_VERSION,
            lazy_budget: None,
            read_settle_ms: 50,
            keep_epochs: 10_000,
            pid: 77,
            exe: "/bin/ctxd".into(),
            exe_blake3: "ab".repeat(32),
            live_pos_given: true,
            branches: vec![
                CheckpointBranch {
                    id: MAIN,
                    name: "main".into(),
                    kernel_digest: [1; 32],
                    derived_digest: [2; 32],
                },
                CheckpointBranch {
                    id: BranchId(3),
                    name: "feature".into(),
                    kernel_digest: [8; 32],
                    derived_digest: [9; 32],
                },
            ],
            certs: vec![
                CheckpointCert {
                    sources: vec!["root_proof".into(), "session".into()],
                    cert: cert(5, MAIN),
                    live_pos: vec![("repo".into(), vec![1, 2]), ("team:x".into(), vec![])],
                    check: CertCheck::Verdict(Validation::Pending {
                        rules: vec![RuleId(4)],
                    }),
                },
                CheckpointCert {
                    sources: vec!["handoff".into()],
                    cert: cert(6, BranchId(3)),
                    live_pos: Vec::new(),
                    check: CertCheck::HistoryUnavailable { since: Epoch(2) },
                },
                CheckpointCert {
                    sources: vec!["session_cert".into()],
                    cert: cert(7, MAIN),
                    live_pos: Vec::new(),
                    check: CertCheck::Verdict(Validation::Empty),
                },
            ],
        }
    }

    /// The file round-trips: parse(render(r)) is r, rendering again gives
    /// the same text, and the counts are the verdicts'.
    #[test]
    fn checkpoint_json_round_trip() {
        let r = report();
        let meta = json!({"taken_at_utc": "2026-11-26T00:00:00Z", "ctxd_toml_now": null});
        let v = render(&r, meta.clone());
        assert_eq!(parse(&v).unwrap(), r);
        let text = serde_json::to_string_pretty(&v).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&render(&parse(&back).unwrap(), meta)).unwrap(),
            text
        );
        assert_eq!(v["counts"]["certs"], 3);
        assert_eq!(
            v["counts"]["non_empty"], 1,
            "Pending; Empty and HistoryUnavailable are not"
        );
        assert_eq!(v["counts"]["history_unavailable"], 1);
        // Canonical: every object's keys are sorted.
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }
    /// `diff` reports a changed leaf, a missing key and an array of another
    /// length, each once, and nothing for equal values.
    #[test]
    fn diff_names_each_differing_field() {
        let a = json!({"x": 1, "b": [{"d": "aa"}, {"d": "bb"}], "c": [1]});
        let mut out = Vec::new();
        diff("", &a, &a, &mut out);
        assert!(out.is_empty());
        let b = json!({"x": 2, "b": [{"d": "aa"}, {"d": "cc"}], "c": [1, 2], "y": true});
        diff("", &a, &b, &mut out);
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(out[0].starts_with(".b[1].d:"), "{out:?}");
    }
    #[test]
    fn utc_formats_the_civil_date() {
        let t = |s: u64| utc(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(t(0), "1970-01-01T00:00:00Z");
        assert_eq!(t(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(t(1_795_746_896), "2026-11-27T02:34:56Z");
    }
}
