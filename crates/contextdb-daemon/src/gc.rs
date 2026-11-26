// Payload GC through ctxd (ADR 0019 §5.1, §5.2, §5.7). Included into
// `service.rs` for its session internals (K4).
//
// - `ctx gc` plans on the current snapshot at e0 with
//   `cutoff = min(e0 − keep_epochs, K5)`, then computes K4 and reports.
// - `--apply` plans on the connection thread (writers are not blocked),
//   computes K4, and sends the plan to the actor, which applies it as one
//   exclusive step with the K5 bound taken then. A plan the actor finds
//   outdated is planned again once. The projections are purged after.

/// A store GC failure as a response.
fn gc_err(e: &StoreError) -> Response {
    let (code, detail) = store_code(e);
    err(&code, detail)
}

impl<D: Durability + Clone + 'static> Service<D> {
    /// `[gc] keep_epochs` of `ctxd.toml` (O3).
    pub fn set_gc_keep_epochs(&self, n: u64) {
        self.gc_keep_epochs.store(n, Ordering::Relaxed);
    }
    /// Every certificate ctxd keeps, with where: each session's
    /// certificates and RootProofs (live and recently ended sessions), each
    /// agent's proofs and session certificate, and the certificate of every
    /// pending handoff.
    fn held_certificates(&self) -> Vec<(&'static str, Certificate)> {
        let mut out = Vec::new();
        // One lock at a time: the maps, then each session.
        let live: Vec<Arc<Mutex<Session>>> = (self.sessions.lock().expect("sessions"))
            .values()
            .cloned()
            .collect();
        let ended: Vec<Arc<Mutex<Session>>> = (self.hooks.lock().expect("hooks").ended.iter())
            .map(|(_, s)| s.clone())
            .collect();
        let sessions = (live.iter().map(|s| (s, ["session", "root_proof", "session_cert"])))
            .chain(ended.iter().map(|s| {
                let names = ["ended_session", "ended_root_proof", "ended_session_cert"];
                (s, names)
            }));
        for (s, [certs, proofs, session_cert]) in sessions {
            let s = s.lock().expect("session");
            out.extend(s.certs.iter().map(|c| (certs, c.clone())));
            out.extend(s.proof_history.iter().map(|p| (proofs, p.cert.clone())));
            for a in s.agents.values() {
                out.extend(a.proofs.values().map(|p| (proofs, p.cert.clone())));
                out.extend(a.session_cert.iter().map(|c| (session_cert, c.clone())));
            }
        }
        (self.handoffs.lock().expect("handoffs").map.values())
            .filter(|h| h.accepted.is_none())
            .for_each(|h| out.push(("handoff", h.cert.clone())));
        out
    }
    /// K4 (ADR 0019 §5.1): the contents of every revision named in the
    /// `vector` or `entries` of a certificate ctxd keeps. Called after
    /// phase 1 took its snapshot: a certificate issued later came from a
    /// snapshot K5 bounded or one at an epoch ≥ e0, which phase 2's
    /// re-check covers.
    fn gc_pins(&self, snap: &Snapshot<D>) -> BTreeSet<ContentHash> {
        let mut named: Vec<(BranchId, RevisionId)> = Vec::new();
        for (_, c) in self.held_certificates() {
            named.extend(c.vector.iter().map(|v| (c.branch, v.rev)));
            named.extend(c.entries.iter().map(|e| (c.branch, e.rev)));
        }
        let k = &snap.kernel;
        named
            .into_iter()
            .filter_map(|(b, r)| {
                (k.branch(b).and_then(|v| v.revision_content(r).ok())).or_else(|| {
                    k.branches()
                        .find_map(|i| k.branch(i.id)?.revision_content(r).ok())
                })
            })
            .collect()
    }
    /// Phase 1 on the current snapshot, then K4: the plan, the K5 bound,
    /// what set the cutoff, and the pins.
    #[allow(clippy::type_complexity)]
    fn gc_phase1(
        &self,
        keep: u64,
    ) -> Result<(GcPlan, Epoch, GcCause, BTreeSet<ContentHash>), Response> {
        let snap = self.actor.snapshot();
        let e0 = snap.epoch;
        // Read after the snapshot is held, so the bound covers it.
        let held = self.actor.held();
        let by_keep = Epoch(e0.0.saturating_sub(keep));
        let (cutoff, cause) = match held < by_keep {
            true => (held, GcCause::HeldSnapshot),
            false => (by_keep, GcCause::KeepEpochs),
        };
        let nested = contextdb_proto::docs::nested_hashes;
        let plan = contextdb_store::gc::gc_plan(&snap.kernel, &snap.log, e0, cutoff, &nested)
            .map_err(|e| gc_err(&e))?;
        let pins = self.gc_pins(&snap);
        Ok((plan, held, cause, pins))
    }
    /// `ctx gc [--apply] [--keep-epochs N]` (ADR 0019 §5.2, §5.7).
    fn gc(&self, keep_epochs: Option<u64>, apply: bool) -> Response {
        use crate::config::KEEP_EPOCHS_FLOOR;
        let keep = keep_epochs.unwrap_or(self.gc_keep_epochs.load(Ordering::Relaxed));
        if keep < KEEP_EPOCHS_FLOOR {
            return err(
                "InvalidRequest",
                format!("keep_epochs must be at least {KEEP_EPOCHS_FLOOR}, got {keep}"),
            );
        }
        let mut replanned = false;
        loop {
            let (plan, held, cause, pinned) = match self.gc_phase1(keep) {
                Ok(p) => p,
                Err(r) => return r,
            };
            let mut report = GcSummary {
                applied: apply,
                keep_epochs: keep,
                e0: plan.e0,
                cutoff: plan.cutoff,
                cause,
                held,
                kept_k1: plan.kept_k1 as u64,
                kept_k2: plan.kept_k2 as u64,
                kept_k3: plan.kept_k3 as u64,
                kept_k4: pinned.len() as u64,
                now: plan.e0,
                ..GcSummary::default()
            };
            if !apply {
                let left = (plan.candidates.iter()).filter(|c| !pinned.contains(&c.content));
                for c in left {
                    report.payloads += 1;
                    report.bytes += c.bytes;
                }
                return Response::Gc { report };
            }
            let plan = Box::new(plan);
            match (self.actor).request(|reply| Message::Gc {
                plan,
                pinned,
                reply,
            }) {
                Ok(Ok(r)) => {
                    report.payloads = r.collected.len() as u64;
                    report.bytes = r.bytes;
                    report.rechecked_out = r.rechecked_out as u64;
                    report.generation = r.generation;
                    report.now = r.now;
                    report.purged = match self.gc_purge() {
                        Ok(()) => true,
                        Err(e) => {
                            eprintln!("ctxd: GC purge of projections (retried at start): {e}");
                            false
                        }
                    };
                    return Response::Gc { report };
                }
                Ok(Err(StoreError::PlanOutdated)) if !replanned => replanned = true,
                Ok(Err(e)) => return gc_err(&e),
                Err(e) => return submit_err(e),
            }
        }
    }
    /// Purges what payload GC collected from `vectors.sqlite`, the jobs and
    /// the candidates (ADR 0019 §5.2 step 4), idempotent: each projection
    /// purges the ledger entries above its own `collected_through` mark and
    /// then records the newest generation. A content staged again since is
    /// left alone. Recall indexes heads only, which GC never collects.
    pub fn gc_purge(&self) -> Result<(), String> {
        let ledger = (self.actor)
            .request(|reply| Message::GcLedger { reply })
            .map_err(|e| format!("ledger: {e:?}"))?;
        let Some(top) = ledger.iter().map(|e| e.gc_generation).max() else {
            return Ok(());
        };
        let snap = self.actor.snapshot();
        let above = |mark: u64| -> BTreeSet<ContentHash> {
            (ledger.iter())
                .filter(|e| e.gc_generation > mark && snap.log.is_collected(&e.content))
                .map(|e| e.content)
                .collect()
        };
        let sqlerr = |what: &str, e: &dyn std::fmt::Display| format!("{what}: {e}");
        // vectors.sqlite: also when no embedder runs now but the file is left.
        let data = self.data_dir.lock().expect("data dir").clone();
        if let Some(v) = self.vectors.get() {
            let mark = v.collected_through()?;
            if mark < top {
                v.collect(&above(mark).into_iter().collect::<Vec<_>>(), top)?;
            }
        } else if let Some(path) = (data.as_ref())
            .map(|d| d.join(contextdb_store::vectors::FILE))
            .filter(|p| p.exists())
        {
            let mut v = contextdb_store::vectors::VectorStore::open(Some(&path))
                .map_err(|e| sqlerr("vectors", &e))?;
            let mark = v.collected_through().map_err(|e| sqlerr("vectors", &e))?;
            if mark < top {
                let contents: Vec<ContentHash> = above(mark).into_iter().collect();
                v.collect(&contents, top)
                    .map_err(|e| sqlerr("vectors", &e))?;
            }
        }
        // jobs.sqlite: every job whose task holds a collected payload.
        if let Some(s) = self.validators.get() {
            let mark = s.jobs.collected_through()?;
            if mark < top {
                let contents = above(mark);
                let keys: Vec<_> = (s.jobs.all()?.into_iter())
                    .filter(|j| {
                        job_contents(&snap, j.branch, j.rule, &j.curvec)
                            .iter()
                            .any(|c| contents.contains(c))
                    })
                    .map(|j| j.key)
                    .collect();
                s.jobs.collect(&keys, top)?;
            }
        }
        // candidates.sqlite: every pair with a side carrying one.
        let mark = (self.candidates.collected_through()).map_err(|e| sqlerr("candidates", &e))?;
        if mark < top {
            let contents = above(mark);
            let mut revs = Vec::new();
            for info in snap.kernel.branches() {
                let Some(v) = snap.kernel.branch(info.id) else {
                    continue;
                };
                for c in &contents {
                    revs.extend(v.revisions_with_content(c).into_iter().map(|r| (info.id, r)));
                }
            }
            (self.candidates.collect(&revs, top)).map_err(|e| sqlerr("candidates", &e))?;
        }
        Ok(())
    }
    /// `ctx team status` (ADR 0019 §5.4): a mirror head whose content is
    /// collected is a problem (the replica no longer had its bytes when
    /// catch-up made it the head). Only non-head collected contents are
    /// harmless.
    fn collected_mirror_heads(&self, snap: &Snapshot<D>, team: Option<TeamId>) -> Vec<String> {
        let k = &snap.kernel;
        let mut out = Vec::new();
        for info in k.branches() {
            let Some(v) = k.branch(info.id) else {
                continue;
            };
            for l in v.logicals() {
                let Some((key, _, _)) = v.logical_info(l) else {
                    continue;
                };
                let Some(t) = k.replica_record(&key).and_then(|r| r.origin.team()) else {
                    continue;
                };
                let Some(head) = v.head(l).filter(|_| team.is_none_or(|x| x == t)) else {
                    continue;
                };
                if (v.revision_content(head)).is_ok_and(|c| snap.log.is_collected(&c)) {
                    out.push(format!(
                        "  problem: mirror head {key}@{} on {} reads Collected",
                        head.0, info.name
                    ));
                }
            }
        }
        out
    }
}
