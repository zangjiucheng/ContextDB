//! Lexical contradiction candidates and the recall indexer (ADR 0015 §5).
//!
//! One background thread, never the writer, follows the published epochs:
//!
//! 1. **Index.** It brings the recall projection up to date. Rows are per
//!    `(branch, logical)`: `main`'s head everywhere, and another branch's
//!    head only where it differs from `main`'s. A logical whose head moved
//!    gets all its rows rewritten. A row with a branch op, an unreadable
//!    row, or a projection ahead of the log rebuilds the whole index.
//! 2. **Candidates.** For each head of kind Decision, Plan or Fact that an
//!    epoch since the candidate watermark moved, and that is still the head
//!    on its branch, it takes the body's highest-idf terms (at most 12) and
//!    runs an OR query over the rows visible on that branch, in the same
//!    scope lane. It drops the head's own logical, its direct premises and
//!    dependents, other kinds, rows that are not the branch's head, and
//!    scores under `min_score`; it keeps at most `per_head` pairs.
//!    The first run (or one after the log went backwards) backfills from
//!    every head.
//! 3. **Sweep.** Pairs a side of which is no longer the head on their
//!    branch (or whose branch is archived) are marked `gone`.
//!
//! `<data>/candidates.sqlite` holds `candidates(id, branch, a_rev, b_rev,
//! score, epoch, state)`, unique per branch and unordered pair. Like every
//! projection it is rebuildable and never a source of truth: listing
//! re-checks both sides against the snapshot, so a pair the sweep has not
//! reached yet is never shown with a side invisible on the branch (T84).
//! Candidates only list pairs; nothing derives a Conflict from them (D3).
use crate::actor::{ActorHandle, Snapshot};
use crate::index_health::IndexHealth;
use contextdb_core::BranchView;
use contextdb_model::*;
use contextdb_store::recall::{RecallIndex, Row};
use contextdb_store::{Durability, Payload};
use contextdb_validate::config::CandidateLimits;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const FILE: &str = "candidates.sqlite";
/// The kinds that take part in contradiction candidates.
pub const KINDS: [RecordKind; 3] = [RecordKind::Decision, RecordKind::Plan, RecordKind::Fact];
/// Most query terms per head.
pub const TERMS: usize = 12;
/// Most body bytes indexed per head.
const BODY_MAX: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandState {
    /// Not acted on: listed by `ctx_conflicts`.
    New = 0,
    /// A Conflict was opened from it.
    Opened = 1,
    /// A side is no longer the head on the branch.
    Gone = 2,
}
impl CandState {
    fn from_i64(n: i64) -> Self {
        match n {
            0 => Self::New,
            1 => Self::Opened,
            _ => Self::Gone,
        }
    }
}
/// Where a pair stands with the validators that judge pairs (milestone
/// 6b, ADR 0015 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judge {
    /// Not judged yet (or given back for a retry).
    Pending = 0,
    /// A worker holds it under a lease.
    Running = 1,
    /// Judged: `none`, or `conflict` with a PendingReview opened.
    Done = 2,
    /// Out of attempts.
    Failed = 3,
    /// No longer worth judging (a side moved, or the pair was acted on).
    Skipped = 4,
}
impl Judge {
    fn from_i64(n: i64) -> Self {
        match n {
            0 => Self::Pending,
            1 => Self::Running,
            2 => Self::Done,
            3 => Self::Failed,
            _ => Self::Skipped,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: u64,
    pub branch: BranchId,
    /// The head whose arrival produced the pair.
    pub a: RevisionId,
    pub b: RevisionId,
    pub score: f64,
    pub epoch: Epoch,
    pub state: CandState,
    pub judge: Judge,
    /// The judging validator's label and name, once judged.
    pub label: Option<String>,
    pub validator: Option<String>,
    /// The PendingReview a `conflict` judgement opened.
    pub review: Option<ArtifactId>,
    pub attempts: u32,
    pub error: Option<String>,
    /// The worker's claim token while `Running`.
    pub token: u64,
    /// Which sources found the pair: [`LEXICAL`], [`EMBEDDING`] or both
    /// (ADR 0016 §7.3).
    pub source: u8,
    /// The embedding cosine, when the embedding source found it.
    pub cosine: Option<f64>,
}
/// `Candidate::source` bits.
pub const LEXICAL: u8 = 1;
pub const EMBEDDING: u8 = 2;
/// Counters for `ctx validators status` and the G5/G6 reports.
#[derive(Debug, Default)]
pub struct CandStats {
    /// Heads queried.
    pub heads: AtomicU64,
    /// Pairs inserted.
    pub pairs: AtomicU64,
    /// Pairs marked `gone`.
    pub gone: AtomicU64,
    /// Total and worst time per head, in microseconds.
    pub micros: AtomicU64,
    pub micros_max: AtomicU64,
}
pub struct Candidates {
    conn: Mutex<Connection>,
    limits: Mutex<CandidateLimits>,
    pub stats: CandStats,
}
type Sql<T> = rusqlite::Result<T>;
fn row_to(r: &rusqlite::Row<'_>) -> Sql<Candidate> {
    Ok(Candidate {
        id: r.get::<_, i64>(0)? as u64,
        branch: BranchId(r.get(1)?),
        a: RevisionId(r.get(2)?),
        b: RevisionId(r.get(3)?),
        score: r.get(4)?,
        epoch: Epoch(r.get::<_, i64>(5)? as u64),
        state: CandState::from_i64(r.get(6)?),
        judge: Judge::from_i64(r.get(7)?),
        label: r.get(8)?,
        validator: r.get(9)?,
        review: r.get::<_, Option<u32>>(10)?.map(ArtifactId),
        attempts: r.get(11)?,
        error: r.get(12)?,
        token: r.get::<_, i64>(13)? as u64,
        source: r.get::<_, i64>(14)? as u8,
        cosine: r.get(15)?,
    })
}
const COLUMNS: &str = "id, branch, a_rev, b_rev, score, epoch, state, judge, label, validator, review, attempts, error, judge_token, source, cosine";
/// Judging columns, added to a table from milestone 6 on open.
const JUDGE_COLUMNS: &[(&str, &str)] = &[
    ("judge", "INTEGER NOT NULL DEFAULT 0"),
    ("judge_token", "INTEGER NOT NULL DEFAULT 0"),
    ("lease_until", "INTEGER NOT NULL DEFAULT 0"),
    ("attempts", "INTEGER NOT NULL DEFAULT 0"),
    ("next_at", "INTEGER NOT NULL DEFAULT 0"),
    ("label", "TEXT"),
    ("validator", "TEXT"),
    ("review", "INTEGER"),
    ("error", "TEXT"),
    ("meta", "BLOB"),
    // P8 milestone 5b: which sources found the pair.
    ("source", "INTEGER NOT NULL DEFAULT 1"),
    ("cosine", "REAL"),
];
impl Candidates {
    /// Opens (or creates) the table; memory when `path` is `None`.
    pub fn open(path: Option<&Path>) -> Result<Self, String> {
        let conn = match path {
            Some(p) => Connection::open(p),
            None => Connection::open_in_memory(),
        }
        .map_err(|e| format!("{FILE}: {e}"))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS candidates(
                 id INTEGER PRIMARY KEY,
                 branch INTEGER NOT NULL,
                 a_rev INTEGER NOT NULL,
                 b_rev INTEGER NOT NULL,
                 a_logical INTEGER NOT NULL,
                 b_logical INTEGER NOT NULL,
                 score REAL NOT NULL,
                 epoch INTEGER NOT NULL,
                 state INTEGER NOT NULL);
             CREATE UNIQUE INDEX IF NOT EXISTS candidates_pair
                 ON candidates(branch, min(a_rev, b_rev), max(a_rev, b_rev));
             CREATE INDEX IF NOT EXISTS candidates_a ON candidates(branch, a_logical) WHERE state = 0;
             CREATE INDEX IF NOT EXISTS candidates_b ON candidates(branch, b_logical) WHERE state = 0;
             CREATE TABLE IF NOT EXISTS candidates_watermark(
                 id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS candidates_embed_watermark(
                 id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);",
        )
        .map_err(|e| format!("{FILE}: {e}"))?;
        for (name, ty) in JUDGE_COLUMNS {
            if conn
                .prepare(&format!("SELECT {name} FROM candidates LIMIT 0"))
                .is_err()
            {
                conn.execute_batch(&format!("ALTER TABLE candidates ADD COLUMN {name} {ty}"))
                    .map_err(|e| format!("{FILE}: {e}"))?;
            }
        }
        // This process holds the data directory's lock: a pair left
        // Running was held by a worker that is gone.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS candidates_judge ON candidates(id) WHERE state = 0 AND judge = 0;
             UPDATE candidates SET judge = 0 WHERE judge = 1;",
        )
        .map_err(|e| format!("{FILE}: {e}"))?;
        Ok(Self {
            conn: Mutex::new(conn),
            limits: Mutex::new(CandidateLimits::default()),
            stats: CandStats::default(),
        })
    }
    pub fn limits(&self) -> CandidateLimits {
        *self.limits.lock().expect("limits")
    }
    pub fn set_limits(&self, l: CandidateLimits) {
        *self.limits.lock().expect("limits") = l;
    }
    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("candidates")
    }
    /// The last epoch whose heads were queried; `None` before the first run.
    pub fn watermark(&self) -> Sql<Option<Epoch>> {
        Ok(self
            .db()
            .query_row(
                "SELECT epoch FROM candidates_watermark WHERE id = 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|e| Epoch(e as u64)))
    }
    fn set_watermark(&self, e: Epoch) -> Sql<()> {
        self.db().execute(
            "INSERT INTO candidates_watermark(id, epoch) VALUES (0, ?1)
             ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch",
            params![e.0 as i64],
        )?;
        Ok(())
    }
    fn clear(&self) -> Sql<()> {
        self.db()
            .execute_batch(
            "DELETE FROM candidates; DELETE FROM candidates_watermark; DELETE FROM candidates_embed_watermark;",
        )
    }
    /// Inserts a lexical pair unless the branch already has it (in either
    /// order); a pair the embedding source found becomes `both`.
    fn insert(
        &self,
        branch: BranchId,
        a: (LogicalId, RevisionId),
        b: (LogicalId, RevisionId),
        score: f64,
        epoch: Epoch,
    ) -> Sql<bool> {
        let db = self.db();
        let n = db.execute(
            "INSERT OR IGNORE INTO candidates(branch, a_rev, b_rev, a_logical, b_logical, score, epoch, state, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 1)",
            params![branch.0, a.1.0, b.1.0, a.0.0, b.0.0, score, epoch.0 as i64],
        )?;
        if n == 0 {
            db.execute(
                "UPDATE candidates SET source = source | 1, score = max(score, ?4)
                 WHERE branch = ?1 AND min(a_rev, b_rev) = min(?2, ?3) AND max(a_rev, b_rev) = max(?2, ?3)",
                params![branch.0, a.1.0, b.1.0, score],
            )?;
        }
        Ok(n == 1)
    }
    /// Inserts an embedding pair (ADR 0016 §7.3) unless the branch already
    /// has it; a pair the lexical source found becomes `both`.
    pub(crate) fn insert_embedding(
        &self,
        branch: BranchId,
        a: (LogicalId, RevisionId),
        b: (LogicalId, RevisionId),
        cosine: f64,
        epoch: Epoch,
    ) -> Sql<bool> {
        let db = self.db();
        let n = db.execute(
            "INSERT OR IGNORE INTO candidates(branch, a_rev, b_rev, a_logical, b_logical, score, epoch, state, source, cosine)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, 0, 2, ?7)",
            params![branch.0, a.1.0, b.1.0, a.0.0, b.0.0, epoch.0 as i64, cosine],
        )?;
        if n == 0 {
            db.execute(
                "UPDATE candidates SET source = source | 2, cosine = ?4
                 WHERE branch = ?1 AND min(a_rev, b_rev) = min(?2, ?3) AND max(a_rev, b_rev) = max(?2, ?3)",
                params![branch.0, a.1.0, b.1.0, cosine],
            )?;
        }
        Ok(n == 1)
    }
    /// The last epoch the embedding source queried; `None` before its first
    /// run or after the table was cleared.
    pub fn embed_watermark(&self) -> Sql<Option<Epoch>> {
        Ok(self
            .db()
            .query_row(
                "SELECT epoch FROM candidates_embed_watermark WHERE id = 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|e| Epoch(e as u64)))
    }
    pub(crate) fn set_embed_watermark(&self, e: Epoch) -> Sql<()> {
        self.db().execute(
            "INSERT INTO candidates_embed_watermark(id, epoch) VALUES (0, ?1)
             ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch",
            params![e.0 as i64],
        )?;
        Ok(())
    }
    pub fn get(&self, id: u64) -> Sql<Option<Candidate>> {
        self.db()
            .query_row(
                &format!("SELECT {COLUMNS} FROM candidates WHERE id = ?1"),
                params![id as i64],
                row_to,
            )
            .optional()
    }
    /// Pairs not yet acted on, on one branch (or every branch), oldest
    /// first.
    pub fn listed(&self, branch: Option<BranchId>) -> Sql<Vec<Candidate>> {
        let db = self.db();
        let mut stmt = db.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM candidates
             WHERE state = 0 AND (?1 IS NULL OR branch = ?1) ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![branch.map(|b| b.0)], row_to)?;
        rows.collect()
    }
    /// All pairs, any state (tests and reports).
    pub fn all(&self) -> Sql<Vec<Candidate>> {
        let db = self.db();
        let mut stmt =
            db.prepare_cached(&format!("SELECT {COLUMNS} FROM candidates ORDER BY id"))?;
        let rows = stmt.query_map([], row_to)?;
        rows.collect()
    }
    pub fn set_state(&self, id: u64, state: CandState) -> Sql<()> {
        self.db().execute(
            "UPDATE candidates SET state = ?2 WHERE id = ?1",
            params![id as i64, state as i64],
        )?;
        Ok(())
    }
    // Judging (milestone 6b).

    /// Pairs not acted on and not judged, whose retry time has come, oldest
    /// first.
    pub fn judge_queue(&self, now_ms: i64, limit: usize) -> Sql<Vec<Candidate>> {
        let db = self.db();
        let mut stmt = db.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM candidates
             WHERE state = 0 AND judge = 0 AND next_at <= ?1 ORDER BY id LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![now_ms, limit as i64], row_to)?;
        rows.collect()
    }
    /// Takes a pending pair under a lease; false when another worker has it.
    pub fn judge_claim(&self, id: u64, token: u64, lease_until_ms: i64) -> Sql<bool> {
        Ok(self.db().execute(
            "UPDATE candidates SET judge = 1, judge_token = ?2, lease_until = ?3
             WHERE id = ?1 AND judge = 0",
            params![id as i64, token as i64, lease_until_ms],
        )? == 1)
    }
    /// Running pairs whose lease lapsed count an attempt and go back to
    /// pending (or `Failed` after `max_attempts`).
    pub fn judge_expire(&self, now_ms: i64, max_attempts: u32) -> Sql<usize> {
        self.db().execute(
            "UPDATE candidates SET
                 attempts = attempts + 1,
                 judge = CASE WHEN attempts + 1 >= ?2 THEN 3 ELSE 0 END,
                 error = 'lease expired'
             WHERE judge = 1 AND lease_until < ?1",
            params![now_ms, max_attempts],
        )
    }
    /// Ends a claim held by `token`; false when the token no longer holds it.
    #[allow(clippy::too_many_arguments)]
    pub fn judge_finish(
        &self,
        id: u64,
        token: u64,
        judge: Judge,
        label: Option<&str>,
        validator: Option<&str>,
        review: Option<ArtifactId>,
        meta: Option<&[u8]>,
    ) -> Sql<bool> {
        Ok(self.db().execute(
            "UPDATE candidates SET judge = ?3, label = ?4, validator = ?5, review = ?6,
                 meta = ?7, error = NULL
             WHERE id = ?1 AND judge = 1 AND judge_token = ?2",
            params![
                id as i64,
                token as i64,
                judge as i64,
                label,
                validator,
                review.map(|a| a.0),
                meta
            ],
        )? == 1)
    }
    /// A failed attempt: retried at `next_at_ms`, or `Failed` after
    /// `max_attempts`.
    pub fn judge_fail(
        &self,
        id: u64,
        token: u64,
        error: &str,
        max_attempts: u32,
        next_at_ms: i64,
    ) -> Sql<bool> {
        Ok(self.db().execute(
            "UPDATE candidates SET
                 attempts = attempts + 1,
                 judge = CASE WHEN attempts + 1 >= ?4 THEN 3 ELSE 0 END,
                 error = ?3, next_at = ?5
             WHERE id = ?1 AND judge = 1 AND judge_token = ?2",
            params![id as i64, token as i64, error, max_attempts, next_at_ms],
        )? == 1)
    }
    /// Gives a claim back without counting an attempt (the LLM call cap).
    pub fn judge_release(&self, id: u64, token: u64, next_at_ms: i64) -> Sql<bool> {
        Ok(self.db().execute(
            "UPDATE candidates SET judge = 0, next_at = ?3
             WHERE id = ?1 AND judge = 1 AND judge_token = ?2",
            params![id as i64, token as i64, next_at_ms],
        )? == 1)
    }
    /// Puts a pending pair no validator takes back to `next_at_ms`, in one
    /// statement and without a claim, so no lease can lapse and count an
    /// attempt in between (P7 m4).
    pub fn judge_postpone(&self, id: u64, next_at_ms: i64) -> Sql<bool> {
        Ok(self.db().execute(
            "UPDATE candidates SET next_at = ?2 WHERE id = ?1 AND judge = 0",
            params![id as i64, next_at_ms],
        )? == 1)
    }
    /// Makes every postponed pending pair due now: after a `SetAccess`
    /// (a lane's egress may have turned on) or a validator reload (ADR 0017
    /// §7). Retry back-off of failed pairs is cut short too.
    pub fn judge_wake(&self) -> Sql<usize> {
        self.db().execute(
            "UPDATE candidates SET next_at = 0 WHERE state = 0 AND judge = 0 AND next_at > 0",
            [],
        )
    }
    /// Marks a pending pair `Skipped` (no claim needed).
    pub fn judge_skip(&self, id: u64) -> Sql<()> {
        self.db().execute(
            "UPDATE candidates SET judge = 4 WHERE id = ?1 AND judge = 0",
            params![id as i64],
        )?;
        Ok(())
    }
    /// Counts by judge state (`ctx validators status`).
    pub fn judge_counts(&self) -> Sql<[u64; 5]> {
        let db = self.db();
        let mut stmt =
            db.prepare_cached("SELECT judge, count(*) FROM candidates GROUP BY judge")?;
        let mut out = [0u64; 5];
        for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (j, n) = r?;
            out[Judge::from_i64(j) as usize] += n as u64;
        }
        Ok(out)
    }
    /// Open pairs on `branch` with a side on logical `l`.
    pub(crate) fn touching(&self, branch: BranchId, l: LogicalId) -> Sql<Vec<Candidate>> {
        let db = self.db();
        // Two arms, so each uses its partial index (with `OR` SQLite scans
        // the table: 28 ms per head at 4·10⁴ pairs, G5).
        let mut stmt = db.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM candidates WHERE state = 0 AND branch = ?1 AND a_logical = ?2
             UNION
             SELECT {COLUMNS} FROM candidates WHERE state = 0 AND branch = ?1 AND b_logical = ?2"
        ))?;
        let rows = stmt.query_map(params![branch.0, l.0], row_to)?;
        rows.collect()
    }
}

/// Whether both sides of a pair are still the head on its branch.
pub fn current(snap_view: Option<&BranchView<'_>>, c: &Candidate) -> bool {
    let Some(view) = snap_view else { return false };
    !view.info().archived
        && [c.a, c.b].iter().all(|r| {
            view.revision_logical(*r)
                .is_some_and(|l| view.head(l) == Some(*r))
        })
}

/// What the recall projection stores for one revision.
#[derive(Clone)]
pub(crate) struct Text {
    pub(crate) key: String,
    pub(crate) kind: RecordKind,
    pub(crate) scope: String,
    /// Lossy UTF-8, cut at `BODY_MAX`.
    pub(crate) body: String,
    /// Rendered from the whole payload (ADR 0016 §4.1).
    pub(crate) abstract_: levels::Rendered,
    pub(crate) outline: levels::Rendered,
}
/// The text of revision `r`; an unavailable payload indexes an empty body.
pub(crate) fn text<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    l: LogicalId,
    r: RevisionId,
) -> Option<Text> {
    let (key, kind, scope) = view.logical_info(l)?;
    let content = view.revision_content(r).ok()?;
    let bytes = match snap.log.read_blob(&content) {
        Payload::Available(b) => b,
        Payload::Unavailable(_) => Vec::new(),
    };
    let mut body = String::from_utf8_lossy(&bytes).into_owned();
    if body.len() > BODY_MAX {
        let mut end = BODY_MAX;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
    }
    Some(Text {
        key,
        kind,
        scope,
        body,
        abstract_: levels::abstract_of(&bytes),
        outline: levels::outline_of(&bytes),
    })
}
/// The recall rows of logical `l`: `main`'s head, and each other live
/// branch's head where it differs from `main`'s.
fn rows_of<D: Durability>(snap: &Snapshot<D>, l: LogicalId) -> Vec<Row> {
    let main_head = snap.kernel.branch(MAIN).and_then(|v| v.head(l));
    let mut texts: BTreeMap<RevisionId, Option<Text>> = BTreeMap::new();
    let mut out = Vec::new();
    for info in snap.kernel.branches() {
        if info.archived {
            continue;
        }
        let Some(view) = snap.kernel.branch(info.id) else {
            continue;
        };
        let Some(r) = view.head(l) else { continue };
        if info.id != MAIN && main_head == Some(r) {
            continue;
        }
        let t = texts
            .entry(r)
            .or_insert_with(|| text(snap, &view, l, r))
            .clone();
        if let Some(t) = t {
            out.push(Row {
                branch: info.id,
                rev: r,
                key: t.key,
                kind: t.kind,
                scope: t.scope,
                body: t.body,
                abstract_: t.abstract_,
                outline: t.outline,
            });
        }
    }
    out
}
fn has_branch_op(batch: &Batch) -> bool {
    (batch.ops.iter()).any(|op| {
        matches!(
            op,
            Op::CreateBranch { .. } | Op::Merge { .. } | Op::ArchiveBranch { .. }
        )
    })
}
/// What epochs `from..=to` changed: `None` when a full pass is needed (a
/// branch op or an unreadable row), else the `(branch, logical)` heads that
/// moved.
fn moved<D: Durability>(
    snap: &Snapshot<D>,
    from: u64,
    to: u64,
) -> Option<BTreeSet<(BranchId, LogicalId)>> {
    let mut out = BTreeSet::new();
    for e in from..=to {
        if e == 0 {
            continue;
        }
        let row = snap.log.read_row(Epoch(e)).ok()?;
        let batch = row.batch().ok()?;
        if has_branch_op(&batch) {
            return None;
        }
        let result = row.result().ok()?;
        out.extend((result.net_delta.head_moves.iter()).map(|(l, _, _)| (batch.branch, *l)));
    }
    Some(out)
}
/// The `(branch, logical)` heads epochs `from..=to` moved, for candidates
/// (a merge's moves included). The flag asks for a sweep of every pair: a
/// branch was archived, or a row could not be read.
pub(crate) fn moved_heads<D: Durability>(
    snap: &Snapshot<D>,
    from: u64,
    to: u64,
) -> (BTreeSet<(BranchId, LogicalId)>, bool) {
    let mut out = BTreeSet::new();
    let mut sweep_all = false;
    for e in from..=to {
        if e == 0 {
            continue;
        }
        let Some((batch, result)) = (snap.log.read_row(Epoch(e)).ok())
            .and_then(|row| Some((row.batch().ok()?, row.result().ok()?)))
        else {
            sweep_all = true;
            continue;
        };
        sweep_all |= (batch.ops.iter()).any(|op| matches!(op, Op::ArchiveBranch { .. }));
        out.extend((result.net_delta.head_moves.iter()).map(|(l, _, _)| (batch.branch, *l)));
    }
    (out, sweep_all)
}
/// Every live `(branch, logical)` head: `main`'s, and other branches' where
/// they differ from `main`'s.
pub(crate) fn all_heads<D: Durability>(snap: &Snapshot<D>) -> BTreeSet<(BranchId, LogicalId)> {
    let main = snap.kernel.branch(MAIN);
    let mut out = BTreeSet::new();
    for info in snap.kernel.branches() {
        if info.archived {
            continue;
        }
        let Some(view) = snap.kernel.branch(info.id) else {
            continue;
        };
        for l in view.logicals() {
            let h = view.head(l);
            if info.id == MAIN || main.as_ref().and_then(|m| m.head(l)) != h {
                out.insert((info.id, l));
            }
        }
    }
    out
}

/// Logicals per transaction of a full rebuild: recall queries interleave
/// between chunks, and progress is reported after each (ADR 0016 §2.2).
const REBUILD_CHUNK: usize = 256;

/// Brings the recall index up to date with `snap` (step 1), reporting to
/// `health`. Returns whether anything was done. A full pass (first run, a
/// branch op, an unreadable row, a watermark ahead of the log, or `ctx
/// reindex`) clears the index and rebuilds it in chunks; the watermark is
/// set only by the last one. A failed write is reported and retried on the
/// next pass.
pub fn index<D: Durability>(
    snap: &Snapshot<D>,
    recall: &Mutex<RecallIndex>,
    health: &IndexHealth,
) -> bool {
    index_with(snap, recall, health, true)
}
/// [`index`] when `full` is false only applies an incremental update and
/// does nothing if a full rebuild is due. A candidate pass refreshes the
/// index this way (P8 M7 review m3): an incremental update only rewrites
/// heads that moved after the pass's snapshot, which the next pass queries
/// again, while a rebuild clears rows the pass still needs, and a failure
/// part way would leave them missing under a watermark the pass then sets.
fn index_with<D: Durability>(
    snap: &Snapshot<D>,
    recall: &Mutex<RecallIndex>,
    health: &IndexHealth,
    full: bool,
) -> bool {
    if !full && health.reindex_requested() {
        return false;
    }
    let forced = health.take_reindex();
    let indexed = recall
        .lock()
        .expect("recall")
        .watermark()
        .unwrap_or(Epoch(0));
    if indexed == snap.epoch && !forced {
        return false;
    }
    let incremental = if forced || indexed > snap.epoch || indexed.0 == 0 {
        None
    } else {
        moved(snap, indexed.0 + 1, snap.epoch.0)
    };
    if incremental.is_none() && !full {
        return false;
    }
    let fail = |e: rusqlite::Error| {
        health.failed(e.to_string());
        false
    };
    match incremental {
        Some(m) => {
            let touched: BTreeSet<LogicalId> = m.into_iter().map(|(_, l)| l).collect();
            let entries: Vec<(LogicalId, Vec<Row>)> =
                touched.into_iter().map(|l| (l, rows_of(snap, l))).collect();
            if let Err(e) = recall.lock().expect("recall").update(snap.epoch, &entries) {
                return fail(e);
            }
        }
        None => {
            let mut all = BTreeSet::new();
            for b in snap.kernel.branches() {
                if let Some(v) = snap.kernel.branch(b.id) {
                    all.extend(v.logicals());
                }
            }
            let all: Vec<LogicalId> = all.into_iter().collect();
            let total = all.len() as u64;
            health.rebuilding(0, total);
            if let Err(e) = recall.lock().expect("recall").clear() {
                return fail(e);
            }
            let mut done = 0u64;
            for chunk in all.chunks(REBUILD_CHUNK) {
                let entries: Vec<(LogicalId, Vec<Row>)> =
                    chunk.iter().map(|l| (*l, rows_of(snap, *l))).collect();
                if let Err(e) = recall.lock().expect("recall").update_partial(&entries) {
                    return fail(e);
                }
                done += chunk.len() as u64;
                health.rebuilding(done, total);
                crate::p8_crash_point("reindex-chunk");
            }
            if let Err(e) = recall.lock().expect("recall").update(snap.epoch, &[]) {
                return fail(e);
            }
        }
    }
    health.applied(snap.epoch);
    true
}

/// Queries one head and inserts its pairs (step 2); returns the number
/// inserted.
fn pairs_for<D: Durability>(
    snap: &Snapshot<D>,
    recall: &Mutex<RecallIndex>,
    table: &Candidates,
    limits: CandidateLimits,
    branch: BranchId,
    l: LogicalId,
) -> usize {
    let Some(view) = snap.kernel.branch(branch) else {
        return 0;
    };
    if view.info().archived || limits.per_head == 0 {
        return 0;
    }
    let Some(rev) = view.head(l) else { return 0 };
    let Some(Text {
        key,
        kind,
        scope,
        body,
        ..
    }) = text(snap, &view, l, rev)
    else {
        return 0;
    };
    if !KINDS.contains(&kind) {
        return 0;
    }
    let started = std::time::Instant::now();
    let hits = {
        let idx = recall.lock().expect("recall");
        let Ok(terms) = idx.top_terms(&format!("{key} {body}"), TERMS) else {
            return 0;
        };
        idx.any_of(&terms, &scope, branch, limits.per_head * 4 + 16)
            .unwrap_or_default()
    };
    let mut skip: BTreeSet<LogicalId> = view.neighbour_logicals(l).into_iter().collect();
    skip.insert(l);
    let mut inserted = 0;
    let mut kept = 0;
    for h in hits {
        if kept == limits.per_head {
            break;
        }
        if h.score < limits.min_score
            || skip.contains(&h.logical)
            || view.head(h.logical) != Some(h.rev)
            || !(view.logical_info(h.logical)).is_some_and(|(_, k, _)| KINDS.contains(&k))
        {
            continue;
        }
        skip.insert(h.logical);
        kept += 1;
        if table
            .insert(branch, (l, rev), (h.logical, h.rev), h.score, snap.epoch)
            .unwrap_or(false)
        {
            inserted += 1;
        }
    }
    let us = started.elapsed().as_micros() as u64;
    let s = &table.stats;
    s.heads.fetch_add(1, Ordering::Relaxed);
    s.pairs.fetch_add(inserted as u64, Ordering::Relaxed);
    s.micros.fetch_add(us, Ordering::Relaxed);
    s.micros_max.fetch_max(us, Ordering::Relaxed);
    inserted
}
pub(crate) fn sweep_rows<D: Durability>(
    snap: &Snapshot<D>,
    table: &Candidates,
    rows: Vec<Candidate>,
) {
    for c in rows {
        if !current(snap.kernel.branch(c.branch).as_ref(), &c) {
            let _ = table.set_state(c.id, CandState::Gone);
            table.stats.gone.fetch_add(1, Ordering::Relaxed);
        }
    }
}
/// Steps 2 and 3 for the epochs since the candidate watermark; the index
/// must already be at `snap.epoch`. Returns the pairs inserted. When `stop`
/// is raised part way, the watermark stays and the pass is redone (pairs
/// are unique, so nothing is inserted twice).
pub fn generate<D: Durability>(
    snap: &Snapshot<D>,
    recall: &Mutex<RecallIndex>,
    table: &Candidates,
    stop: &AtomicBool,
) -> usize {
    generate_with(snap, recall, table, stop, &mut || {})
}
/// How often a long candidate pass lets the text index catch up.
const REFRESH: std::time::Duration = std::time::Duration::from_millis(50);
/// [`generate`], calling `refresh` at most every 50 ms between heads, so a
/// long pass (a burst of commits, a backfill) never holds the text index
/// back (P8 G5). Rows a refresh rewrites to newer heads drop out of this
/// pass's queries (their revision is not the head in `snap`); the newer
/// heads are queried by the next pass.
pub fn generate_with<D: Durability>(
    snap: &Snapshot<D>,
    recall: &Mutex<RecallIndex>,
    table: &Candidates,
    stop: &AtomicBool,
    refresh: &mut dyn FnMut(),
) -> usize {
    let Ok(from) = table.watermark() else {
        return 0;
    };
    let (heads, full) = match from {
        Some(e) if e == snap.epoch => return 0,
        Some(e) if e < snap.epoch => moved_heads(snap, e.0 + 1, snap.epoch.0),
        // First run, or the log went backwards: start over from every head.
        _ => {
            let _ = table.clear();
            (all_heads(snap), true)
        }
    };
    let limits = table.limits();
    let mut inserted = 0;
    let mut last = std::time::Instant::now();
    for (b, l) in &heads {
        if stop.load(Ordering::Acquire) {
            return inserted;
        }
        if last.elapsed() >= REFRESH {
            refresh();
            last = std::time::Instant::now();
        }
        inserted += pairs_for(snap, recall, table, limits, *b, *l);
    }
    // Pairs that lost a side.
    if full {
        sweep_rows(snap, table, table.listed(None).unwrap_or_default());
    } else {
        for (b, l) in &heads {
            sweep_rows(snap, table, table.touching(*b, *l).unwrap_or_default());
        }
    }
    let _ = table.set_watermark(snap.epoch);
    inserted
}

/// The indexer thread: index, then candidates, for each published epoch.
pub(crate) fn indexer<D: Durability>(
    actor: ActorHandle<D>,
    recall: Arc<Mutex<RecallIndex>>,
    candidates: Arc<Candidates>,
    health: Arc<IndexHealth>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let snap = actor.snapshot();
        let indexed = index(&snap, &recall, &health);
        let before = candidates.watermark().ok().flatten();
        // Candidates need the index at this snapshot (it may have failed).
        let ready = recall.lock().expect("recall").watermark().ok() == Some(snap.epoch);
        if ready && before != Some(snap.epoch) {
            generate_with(&snap, &recall, &candidates, &stop, &mut || {
                index_with(&actor.snapshot(), &recall, &health, false);
            });
        }
        // Idle, or the table cannot advance: wait for the next epoch.
        if !indexed && candidates.watermark().ok().flatten() == before {
            // Never hold a snapshot while idle: it would keep the actor from
            // reclaiming that replica (forcing a clone per group, ADR 0008 §3.3).
            drop(snap);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// A milestone-6 table gains the judging columns; its pairs are pending.
    /// A pair left Running by a dead worker is pending again on open.
    #[test]
    fn a_milestone_6_table_gains_the_judging_columns() {
        let dir = std::env::temp_dir().join(format!("cand-m6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE);
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE candidates(id INTEGER PRIMARY KEY, branch INTEGER NOT NULL,
                     a_rev INTEGER NOT NULL, b_rev INTEGER NOT NULL, a_logical INTEGER NOT NULL,
                     b_logical INTEGER NOT NULL, score REAL NOT NULL, epoch INTEGER NOT NULL,
                     state INTEGER NOT NULL);
                 INSERT INTO candidates VALUES (1, 0, 5, 3, 2, 1, 2.5, 7, 0);",
            )
            .unwrap();
        }
        let t = Candidates::open(Some(&path)).unwrap();
        let c = t.get(1).unwrap().unwrap();
        assert_eq!((c.judge, c.attempts, c.label), (Judge::Pending, 0, None));
        assert!(t.judge_claim(1, 42, i64::MAX).unwrap());
        assert!(!t.judge_claim(1, 43, i64::MAX).unwrap(), "held");
        drop(t);
        let t = Candidates::open(Some(&path)).unwrap();
        assert_eq!(t.get(1).unwrap().unwrap().judge, Judge::Pending);
        assert_eq!(t.judge_queue(0, 10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
