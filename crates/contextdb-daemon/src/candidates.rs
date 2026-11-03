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
}
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
    })
}
const COLUMNS: &str = "id, branch, a_rev, b_rev, score, epoch, state";
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
                 id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);",
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
            .execute_batch("DELETE FROM candidates; DELETE FROM candidates_watermark;")
    }
    /// Inserts a pair unless the branch already has it (in either order).
    fn insert(
        &self,
        branch: BranchId,
        a: (LogicalId, RevisionId),
        b: (LogicalId, RevisionId),
        score: f64,
        epoch: Epoch,
    ) -> Sql<bool> {
        let n = self.db().execute(
            "INSERT OR IGNORE INTO candidates(branch, a_rev, b_rev, a_logical, b_logical, score, epoch, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![branch.0, a.1.0, b.1.0, a.0.0, b.0.0, score, epoch.0 as i64],
        )?;
        Ok(n == 1)
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
    /// Open pairs on `branch` with a side on logical `l`.
    fn touching(&self, branch: BranchId, l: LogicalId) -> Sql<Vec<Candidate>> {
        let db = self.db();
        let mut stmt = db.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM candidates
             WHERE state = 0 AND branch = ?1 AND (a_logical = ?2 OR b_logical = ?2)"
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

/// `(key, kind, scope, body)` of revision `r`; an unavailable payload
/// indexes an empty body.
fn text<D: Durability>(
    snap: &Snapshot<D>,
    view: &BranchView<'_>,
    l: LogicalId,
    r: RevisionId,
) -> Option<(String, RecordKind, String, String)> {
    let (key, kind, scope) = view.logical_info(l)?;
    let content = view.revision_content(r).ok()?;
    let body = match snap.log.read_blob(&content) {
        Payload::Available(b) => {
            let mut s = String::from_utf8_lossy(&b).into_owned();
            if s.len() > BODY_MAX {
                let mut end = BODY_MAX;
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
            }
            s
        }
        Payload::Unavailable(_) => String::new(),
    };
    Some((key, kind, scope, body))
}
/// The recall rows of logical `l`: `main`'s head, and each other live
/// branch's head where it differs from `main`'s.
fn rows_of<D: Durability>(snap: &Snapshot<D>, l: LogicalId) -> Vec<Row> {
    let main_head = snap.kernel.branch(MAIN).and_then(|v| v.head(l));
    let mut texts: BTreeMap<RevisionId, Option<(String, RecordKind, String, String)>> =
        BTreeMap::new();
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
        if let Some((key, kind, scope, body)) = t {
            out.push(Row {
                branch: info.id,
                rev: r,
                key,
                kind,
                scope,
                body,
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
fn moved_heads<D: Durability>(
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
fn all_heads<D: Durability>(snap: &Snapshot<D>) -> BTreeSet<(BranchId, LogicalId)> {
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

/// Brings the recall index up to date with `snap` (step 1). Returns
/// whether anything was done.
pub fn index<D: Durability>(snap: &Snapshot<D>, recall: &Mutex<RecallIndex>) -> bool {
    let indexed = recall
        .lock()
        .expect("recall")
        .watermark()
        .unwrap_or(Epoch(0));
    if indexed == snap.epoch {
        return false;
    }
    let incremental = if indexed > snap.epoch || indexed.0 == 0 {
        None
    } else {
        moved(snap, indexed.0 + 1, snap.epoch.0)
    };
    let touched: BTreeSet<LogicalId> = match incremental {
        Some(m) => m.into_iter().map(|(_, l)| l).collect(),
        None => {
            let _ = recall.lock().expect("recall").clear();
            let mut all = BTreeSet::new();
            for b in snap.kernel.branches() {
                if let Some(v) = snap.kernel.branch(b.id) {
                    all.extend(v.logicals());
                }
            }
            all
        }
    };
    let entries: Vec<(LogicalId, Vec<Row>)> =
        touched.into_iter().map(|l| (l, rows_of(snap, l))).collect();
    let _ = recall.lock().expect("recall").update(snap.epoch, &entries);
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
    let Some((key, kind, scope, body)) = text(snap, &view, l, rev) else {
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
fn sweep_rows<D: Durability>(snap: &Snapshot<D>, table: &Candidates, rows: Vec<Candidate>) {
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
    for (b, l) in &heads {
        if stop.load(Ordering::Acquire) {
            return inserted;
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
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        let snap = actor.snapshot();
        let indexed = index(&snap, &recall);
        let before = candidates.watermark().ok().flatten();
        if before != Some(snap.epoch) {
            generate(&snap, &recall, &candidates, &stop);
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
