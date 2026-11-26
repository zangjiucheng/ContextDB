// `ctx g6 checkpoint` (P11 G6 B). Included into `service.rs` for its
// session internals (the certificates ctxd holds).
//
// One published snapshot after a full settle answers everything: its
// epoch and op_log checksum, each branch's kernel and derived digest, and
// the verdict of every certificate ctxd holds, checked with one `live_pos`
// and the `LogLink` at that epoch. `ctx g6 verify` replays the same op_log
// prefix eagerly offline and must reach the same answers.

/// How long a checkpoint waits for the frontier to drain before it answers
/// `Settling` (the CLI asks again).
const CHECKPOINT_SETTLE: Duration = Duration::from_secs(10);

/// The executable this process runs, opened at start so that replacing the
/// installed file later does not change what is hashed.
pub(crate) struct ExeImage {
    path: String,
    file: Result<std::fs::File, String>,
    blake3: std::sync::OnceLock<String>,
}
impl ExeImage {
    /// The BLAKE3 of the opened file, read through the descriptor held
    /// since start; hashed once.
    fn blake3(&self) -> String {
        use std::os::unix::fs::FileExt;
        let hash = || -> Result<String, String> {
            let file = self.file.as_ref().map_err(Clone::clone)?;
            let mut h = blake3::Hasher::new();
            let (mut buf, mut at) = (vec![0u8; 1 << 20], 0u64);
            loop {
                let n = file.read_at(&mut buf, at).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                at += n as u64;
            }
            Ok(h.finalize().to_hex().to_string())
        };
        (self
            .blake3
            .get_or_init(|| hash().unwrap_or_else(|e| format!("unavailable: {e}"))))
        .clone()
    }
}

/// Every branch's kernel digest and derived digest (no frontier).
pub fn checkpoint_branches(k: &contextdb_core::Kernel) -> Vec<CheckpointBranch> {
    (k.branches())
        .filter_map(|info| {
            let v = k.branch(info.id)?;
            Some(CheckpointBranch {
                id: info.id,
                name: info.name.clone(),
                kernel_digest: v.digest(),
                derived_digest: v.db().derived_digest(),
            })
        })
        .collect()
}
/// `cert` validated on `view` at epoch `now` with `live`, its `LogLink`
/// from the op_log checksums `at` knows (none above `now`), as `ctx_check`
/// links it.
pub fn checkpoint_check(
    view: &BranchView<'_>,
    now: Epoch,
    at: &dyn Fn(Epoch) -> Option<ContentHash>,
    cert: &Certificate,
    live: &BTreeMap<String, Vec<u8>>,
) -> CertCheck {
    let link = LogLink {
        matches: cert.issued_epoch <= now
            && at(cert.issued_epoch) == Some(cert.log_checksum_at_issue),
        current_checksum: at(now).unwrap_or([0; 32]),
    };
    match view.validate_certificate(cert, live, link) {
        Ok(v) => CertCheck::Verdict(v),
        Err(h) => CertCheck::HistoryUnavailable { since: h.since },
    }
}

impl<D: Durability + Clone + 'static> Service<D> {
    /// Opens the running executable (`Daemon::start`): on Linux the image
    /// itself (`/proc/self/exe`), elsewhere the file `current_exe` names now.
    pub fn set_exe(&self) {
        let path = std::env::current_exe().map_err(|e| e.to_string());
        let target = if cfg!(target_os = "linux") {
            Ok(std::path::PathBuf::from("/proc/self/exe"))
        } else {
            path.clone()
        };
        let file = target.and_then(|p| std::fs::File::open(p).map_err(|e| e.to_string()));
        let _ = self.exe.set(ExeImage {
            path: path.map_or_else(|e| format!("unavailable: {e}"), |p| p.display().to_string()),
            file,
            blake3: std::sync::OnceLock::new(),
        });
    }
    /// `ctx g6 checkpoint`: settles fully (or answers `Settling`), then
    /// reports one snapshot.
    fn checkpoint(&self, live_pos: Option<Vec<(String, Vec<u8>)>>) -> Response {
        let snap = self.actor.snapshot();
        let (snap, ()) = self.pulled(snap, CHECKPOINT_SETTLE, |s| ((), need_all(s)));
        if let Some((_, e)) = snap.kernel.frontier() {
            return settling(e);
        }
        let checksum = match snap.log.checksum_at(snap.epoch) {
            Ok(c) => c,
            Err(e) => return gc_err(&e),
        };
        let branches = checkpoint_branches(&snap.kernel);
        let mut held: BTreeMap<ContentHash, (BTreeSet<&str>, Certificate)> = BTreeMap::new();
        for (source, c) in self.held_certificates() {
            held.entry(c.cert_id)
                .or_insert_with(|| (BTreeSet::new(), c))
                .0
                .insert(source);
        }
        let given: Option<BTreeMap<String, Vec<u8>>> = live_pos.map(|l| l.into_iter().collect());
        let certs = (held.into_values())
            .map(|(sources, cert)| {
                let view = snap.view(cert.branch);
                let live = (given.clone()).unwrap_or_else(|| view.db().watermarks().clone());
                let at = |e: Epoch| snap.log.checksum_at(e).ok();
                let check = checkpoint_check(&view, snap.epoch, &at, &cert, &live);
                CheckpointCert {
                    sources: sources.into_iter().map(str::to_owned).collect(),
                    cert,
                    live_pos: live.into_iter().collect(),
                    check,
                }
            })
            .collect();
        let exe = self.exe.get();
        Response::Checkpoint {
            report: Box::new(CheckpointReport {
                epoch: snap.epoch,
                log_checksum: checksum,
                protocol: PROTOCOL_VERSION,
                log_format: contextdb_store::log::FORMAT_VERSION,
                lazy_budget: self.actor.lazy.map(|b| b.units),
                read_settle_ms: self.actor.read_settle.as_millis() as u64,
                keep_epochs: self.gc_keep_epochs.load(Ordering::Relaxed),
                pid: std::process::id(),
                exe: exe.map_or_else(
                    || "unavailable: not started as ctxd".into(),
                    |e| e.path.clone(),
                ),
                exe_blake3: exe.map_or_else(
                    || "unavailable: not started as ctxd".into(),
                    ExeImage::blake3,
                ),
                live_pos_given: given.is_some(),
                branches,
                certs,
            }),
        }
    }
}
