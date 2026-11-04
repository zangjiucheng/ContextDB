//! Judging contradiction candidates (ADR 0015 §5, milestone 6b).
//!
//! A validator configured with `pairs = true` also takes candidate pairs,
//! when no rule job is waiting. Its filters match the pair's newer side
//! (`a`). A worker claims a pending pair under a lease in
//! `candidates.sqlite`, re-checks that both sides are still the heads on the
//! branch, and runs the validator on a pair task (only `conflict` or `none`
//! are accepted, §4.1's checks apply):
//!
//! - `none`: nothing is committed; the label and metadata stay in the row.
//! - `conflict`: a non-gating PendingReview on `a`, `suggestion: Reject`,
//!   naming both sides, committed on the admin channel by the worker's
//!   principal with `client_txn_id = H(branch, pair)`. A crash after the
//!   commit is found through `TxnStatus` and not judged again.
//!
//! Neither outcome opens a Conflict: an agent or the admin confirms a pair
//! with `ctx_conflicts open` (D3). An LLM batch never carries a
//! `RecordValidation` (T50's guard applies here too).
use super::*;
use crate::candidates::{CandState, Candidate, Candidates, Judge, current};

/// The client transaction id of a pair's review: one commit per
/// `(branch, unordered pair)`.
pub fn pair_txn(c: &Candidate) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(b"ctx-pair-review\0");
    h.update(&c.branch.0.to_le_bytes());
    h.update(&c.a.min(c.b).0.to_le_bytes());
    h.update(&c.a.max(c.b).0.to_le_bytes());
    h.finalize().as_bytes()[..16].try_into().expect("16 bytes")
}
/// How long a pair no configured validator takes waits before it is looked
/// at again (a reload may add one).
const UNTAKEN_RETRY_MS: i64 = 10 * 60 * 1000;

enum PairChoice {
    Run(usize),
    /// A side moved, the branch is gone, or the pair was acted on.
    Stale,
    /// No `pairs` validator matches it.
    Skip,
}
fn choose_pair<D: Durability>(loaded: &Loaded, snap: &Snapshot<D>, c: &Candidate) -> PairChoice {
    let Some(view) = snap.kernel.branch(c.branch) else {
        return PairChoice::Stale;
    };
    if c.state != CandState::New || !current(Some(&view), c) {
        return PairChoice::Stale;
    }
    let Some((_, kind, scope)) = (view.revision_logical(c.a)).and_then(|l| view.logical_info(l))
    else {
        return PairChoice::Stale;
    };
    let name = &view.info().name;
    match (loaded.validators.iter()).position(|v| v.pairs && v.filter.matches(name, kind, &scope)) {
        Some(n) => PairChoice::Run(n),
        None => PairChoice::Skip,
    }
}
fn token() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    ((now_ms() as u64) << 16) ^ SEQ.fetch_add(1, Ordering::Relaxed)
}
fn sql(e: rusqlite::Error) -> String {
    format!("{}: {e}", crate::candidates::FILE)
}
impl<D: Durability + Clone + 'static> Scheduler<D> {
    /// Expires lapsed pair leases, then claims and judges one pair. `None`
    /// when nothing was claimable (or no validator judges pairs).
    pub fn judge_once(&self) -> Result<Option<Ran>, String> {
        let loaded = self.loaded();
        let Some(table) = self.candidates.get() else {
            return Ok(None);
        };
        table
            .judge_expire(now_ms(), loaded.limits.attempts)
            .map_err(sql)?;
        if !loaded.validators.iter().any(|v| v.pairs) {
            return Ok(None);
        }
        let lease = Duration::from_secs(loaded.limits.lease_secs);
        let queue = table.judge_queue(now_ms(), 64).map_err(sql)?;
        let snap = self.actor.snapshot();
        for c in queue {
            match choose_pair(&loaded, &snap, &c) {
                PairChoice::Stale => table.judge_skip(c.id).map_err(sql)?,
                PairChoice::Skip => {
                    // Not ours now; look again later rather than block the
                    // queue's head.
                    let t = token();
                    if table.judge_claim(c.id, t, now_ms()).map_err(sql)? {
                        table
                            .judge_release(c.id, t, now_ms() + UNTAKEN_RETRY_MS)
                            .map_err(sql)?;
                    }
                }
                PairChoice::Run(n) => {
                    let t = token();
                    // The call ends with the lease, not a full lease after
                    // the setup (review finding: a call could outlive it).
                    let deadline = Instant::now() + lease;
                    let until = now_ms() + lease.as_millis() as i64;
                    if !table.judge_claim(c.id, t, until).map_err(sql)? {
                        continue;
                    }
                    drop(snap);
                    let v = loaded.validators[n].validator.clone();
                    let c = Candidate { token: t, ..c };
                    return self
                        .judge(&loaded, table, &c, v.as_ref(), deadline)
                        .map(Some);
                }
            }
        }
        Ok(None)
    }
    /// Gives the pair back without counting an attempt (a store error).
    fn pair_defer(&self, table: &Candidates, c: &Candidate, error: String) -> Result<Ran, String> {
        self.stats.store_errors.fetch_add(1, Ordering::Relaxed);
        let until = now_ms() + self.retry_base.as_millis() as i64;
        Ok(if table.judge_release(c.id, c.token, until).map_err(sql)? {
            Ran::Requeued(error)
        } else {
            Ran::Lost
        })
    }
    fn pair_fail(
        &self,
        loaded: &Loaded,
        table: &Candidates,
        c: &Candidate,
        error: String,
    ) -> Result<Ran, String> {
        self.stats.failures.fetch_add(1, Ordering::Relaxed);
        let delay = self.retry_base.as_millis() as i64 * (1i64 << c.attempts.min(10));
        let held = table
            .judge_fail(
                c.id,
                c.token,
                &error,
                loaded.limits.attempts,
                now_ms() + delay,
            )
            .map_err(sql)?;
        Ok(if held { Ran::Failed(error) } else { Ran::Lost })
    }
    #[allow(clippy::too_many_arguments)]
    fn pair_end(
        &self,
        table: &Candidates,
        c: &Candidate,
        judge: Judge,
        label: Option<&str>,
        validator: Option<&str>,
        review: Option<ArtifactId>,
        meta: Option<&[u8]>,
        ran: Ran,
    ) -> Result<Ran, String> {
        let held = table
            .judge_finish(c.id, c.token, judge, label, validator, review, meta)
            .map_err(sql)?;
        Ok(if held { ran } else { Ran::Lost })
    }
    fn judge(
        &self,
        loaded: &Loaded,
        table: &Candidates,
        c: &Candidate,
        v: &dyn Validator,
        deadline: Instant,
    ) -> Result<Ran, String> {
        let txn = pair_txn(c);
        // Recovery: the review may already be committed.
        match self
            .actor
            .request(|reply| Message::TxnStatus { id: txn, reply })
        {
            Ok(Ok(TxnStatus::Committed { .. })) => {
                return self.pair_end(
                    table,
                    c,
                    Judge::Done,
                    Some(Label::Conflict.name()),
                    None,
                    None,
                    None,
                    Ran::AlreadyCommitted,
                );
            }
            Ok(Ok(TxnStatus::NotCommitted)) => {}
            Ok(Err(e)) => return self.pair_defer(table, c, format!("txn status: {e:?}")),
            Err(e) => return self.pair_defer(table, c, format!("txn status: {e:?}")),
        }
        let snap = self.actor.snapshot();
        let view = snap.kernel.branch(c.branch);
        if !current(view.as_ref(), c) {
            return self.pair_end(
                table,
                c,
                Judge::Skipped,
                None,
                None,
                None,
                None,
                Ran::Superseded,
            );
        }
        let view = view.expect("current implies the branch");
        let item = |r: RevisionId| -> Result<Item, String> {
            let (key, kind, _) = (view.revision_logical(r))
                .and_then(|l| view.logical_info(l))
                .ok_or_else(|| format!("revision {} is not visible", r.0))?;
            Ok(Item {
                handle: format!("{key}@{}", r.0),
                key,
                kind,
                body: body(&snap, &view, r)?,
            })
        };
        let task = match (item(c.a), item(c.b)) {
            (Ok(a), Ok(b)) => Task::pair(txn, c.branch, a, b),
            (Err(e), _) | (_, Err(e)) => return self.pair_fail(loaded, table, c, e),
        };
        drop(snap);
        let kind = v.kind();
        // A task over the size cap fails before it takes an LLM permit
        // (review finding: the permit was spent with no call).
        if task.body_bytes() > contextdb_validate::MAX_TASK_BODY_BYTES {
            return self.pair_fail(
                loaded,
                table,
                c,
                contextdb_validate::ValidatorError::TooLarge.to_string(),
            );
        }
        if kind == ValidatorKind::Llm {
            if !self.llm_permit(loaded.limits.llm_calls_per_hour) {
                let held = table
                    .judge_release(c.id, c.token, now_ms() + 60_000)
                    .map_err(sql)?;
                return Ok(if held { Ran::Deferred } else { Ran::Lost });
            }
            self.stats.llm_calls.fetch_add(1, Ordering::Relaxed);
        }
        self.stats.calls.fetch_add(1, Ordering::Relaxed);
        self.stats.pair_calls.fetch_add(1, Ordering::Relaxed);
        let outcome = match contextdb_validate::run(v, &task, deadline) {
            Ok(o) => o,
            Err(e) => return self.pair_fail(loaded, table, c, e.to_string()),
        };
        if let Some((i, o)) = outcome.tokens {
            self.stats.tokens_in.fetch_add(i as u64, Ordering::Relaxed);
            self.stats.tokens_out.fetch_add(o as u64, Ordering::Relaxed);
        }
        let meta = ValidationMeta {
            v: 1,
            kind,
            validator: v.name().into(),
            version: v.version().into(),
            model: outcome.model.clone(),
            prompt_sha256: outcome.prompt_sha256,
            label: outcome.label,
            rationale: outcome.rationale.clone(),
            evidence: outcome.evidence.clone(),
            tokens_in: outcome.tokens.map(|t| t.0),
            tokens_out: outcome.tokens.map(|t| t.1),
            job: Some(txn),
            based_on: None,
            at: unix_secs(),
        };
        let (meta_hash, meta_bytes) = contextdb_proto::docs::blob(&meta)?;
        if outcome.label != Label::Conflict {
            self.stats.pair_none.fetch_add(1, Ordering::Relaxed);
            return self.pair_end(
                table,
                c,
                Judge::Done,
                Some(outcome.label.name()),
                Some(v.name()),
                None,
                Some(&meta_bytes),
                Ran::Kept(outcome.label),
            );
        }
        let review = ReviewDoc {
            v: 1,
            suggestion: Suggestion::Reject,
            branch: c.branch,
            rule: None,
            curvec: Vec::new(),
            sides: vec![c.a, c.b],
            meta: meta_hash,
        };
        let (review_hash, review_bytes) = contextdb_proto::docs::blob(&review)?;
        let mut batch = Batch::new(vec![Op::OpenArtifact {
            ty: ArtifactType::PendingReview,
            target: c.a.into(),
            payload_hash: review_hash,
        }]);
        batch.branch = c.branch;
        batch.channel = Channel::Admin;
        batch.principal = if kind == ValidatorKind::Llm {
            LLM_PRINCIPAL
        } else {
            TRUSTED_PRINCIPAL
        };
        batch.client_txn_id = Some(txn);
        if let Some(e) = refuse_llm_validation(&batch) {
            return self.pair_fail(loaded, table, c, e);
        }
        let label = Some(Label::Conflict.name());
        match self.actor.write(vec![meta_bytes, review_bytes], batch) {
            Ok(Ok(result)) => {
                self.stats.reviews.fetch_add(1, Ordering::Relaxed);
                self.stats.pair_reviews.fetch_add(1, Ordering::Relaxed);
                self.pair_end(
                    table,
                    c,
                    Judge::Done,
                    label,
                    Some(v.name()),
                    result.allocations.new_artifacts.first().copied(),
                    None,
                    Ran::Reviewed(Suggestion::Reject),
                )
            }
            Ok(Err(StoreError::Tx(TxError::TxnIdReuse { .. }))) => self.pair_end(
                table,
                c,
                Judge::Done,
                label,
                Some(v.name()),
                None,
                None,
                Ran::AlreadyCommitted,
            ),
            Ok(Err(StoreError::Tx(e))) => {
                self.pair_fail(loaded, table, c, format!("commit: {e:?}"))
            }
            // A store failure (full disk, poisoned store, stopped actor) uses
            // no attempt, as for rule jobs (review finding: pairs were left
            // `Failed` across a recovery).
            Ok(Err(e)) => self.pair_defer(table, c, format!("commit: {e:?}")),
            Err(e) => self.pair_defer(table, c, format!("commit: {e:?}")),
        }
    }
}
