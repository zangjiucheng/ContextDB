//! Basic retrieval projection (ADR 0008 §9): an FTS5 index over each
//! logical's key, kind, scope lane and current payload text. Rebuildable and
//! never a source of truth: it only discovers candidates, whose status is
//! always evaluated on a snapshot afterwards (I6).
//!
//! Rows are per `(branch, logical)` (ADR 0015 §5, B2): `main` holds a row for
//! each of its logicals, and another branch only where its head differs from
//! `main`'s (or `main` cannot see the logical). Each row carries the revision
//! it indexes, so a reader on branch `β` keeps a row only when that revision
//! is `β`'s head: main rows that `β` shadows or cannot see drop out.
use contextdb_model::*;
use rusqlite::{Connection, OptionalExtension, params};

type Sql<T> = rusqlite::Result<T>;
/// Matches ranked together by bm25 (see `RecallIndex::hits`).
pub const RANK_WINDOW: usize = 2_048;
/// A row's FTS rowid: `(logical << 16) | branch`. Rows are per `(branch,
/// logical)`, so this is unique, and it depends only on the row, never on
/// the order rows were indexed in: bm25 windows (`RecallIndex::hits`) are
/// the same after any rebuild.
fn row_id(l: LogicalId, b: BranchId) -> i64 {
    (i64::from(l.0) << 16) | i64::from(b.0)
}
pub struct RecallIndex {
    conn: Connection,
}
/// One indexed row: a logical's head on one branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub branch: BranchId,
    pub rev: RevisionId,
    pub key: String,
    pub kind: RecordKind,
    pub scope: String,
    pub body: String,
    /// `levels::abstract_of` / `outline_of` of the **whole** payload (the
    /// body above may be cut), ADR 0016 §4.1.
    pub abstract_: levels::Rendered,
    pub outline: levels::Rendered,
}
/// A row's stored renderings (T87: equal to recomputing from the payload).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Levels {
    pub abstract_: levels::Rendered,
    pub outline: levels::Rendered,
}
/// A full-text hit: the logical, the revision its row indexes, and the bm25
/// score (higher is better).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    pub logical: LogicalId,
    pub rev: RevisionId,
    pub score: f64,
}
/// User text → an FTS5 query of quoted terms (implicit AND), so no input can
/// be a syntax error or reach FTS5 operators.
pub fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}
/// The distinct lowercase alphanumeric terms of `text` (as FTS5's default
/// `unicode61` tokenizer splits them).
pub fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_lowercase)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}
const SCHEMA: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS recall USING fts5(
        key, kind, scope, body, logical UNINDEXED, branch UNINDEXED, rev UNINDEXED,
        abstract UNINDEXED, abstract_cut UNINDEXED, outline UNINDEXED, outline_cut UNINDEXED);
    CREATE TABLE IF NOT EXISTS recall_levels(id INTEGER PRIMARY KEY CHECK (id = 0), version INTEGER NOT NULL);
    CREATE VIRTUAL TABLE IF NOT EXISTS recall_vocab USING fts5vocab(recall, 'row');
    -- Each FTS row's logical, indexed: `logical` is UNINDEXED in the FTS
    -- table, so deleting or counting through it would scan every row.
    CREATE TABLE IF NOT EXISTS recall_map(rid INTEGER PRIMARY KEY, logical INTEGER NOT NULL);
    CREATE INDEX IF NOT EXISTS recall_map_logical ON recall_map(logical);
    CREATE TABLE IF NOT EXISTS recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);";
/// Every stored abstract, by `(logical, revision)`, for a read-only viewer
/// (`ctx ui`). The file is opened read-only and never migrated: a missing
/// file, a projection from before P8, or one from another
/// `LEVELS_VERSION` gives an empty map, never a wrong rendering.
pub fn read_abstracts(
    path: &std::path::Path,
) -> Sql<std::collections::BTreeMap<(LogicalId, RevisionId), levels::Rendered>> {
    use rusqlite::OpenFlags;
    let mut out = std::collections::BTreeMap::new();
    if !path.exists() {
        return Ok(out);
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version: Option<i64> = conn
        .query_row("SELECT version FROM recall_levels WHERE id = 0", [], |r| {
            r.get(0)
        })
        .optional()
        .unwrap_or(None);
    if version != Some(levels::LEVELS_VERSION as i64) {
        return Ok(out);
    }
    let mut stmt = conn.prepare("SELECT logical, rev, abstract, abstract_cut FROM recall")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (LogicalId(r.get(0)?), RevisionId(r.get(1)?)),
            levels::Rendered {
                text: r.get(2)?,
                cut: r.get::<_, i64>(3)? != 0,
            },
        ))
    })?;
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}
impl RecallIndex {
    /// A read-only connection to an index another connection maintains
    /// (WAL: its queries never wait for the writer's transactions). Only
    /// the query methods may be used on it.
    pub fn open_reader(path: &std::path::Path) -> Sql<Self> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Self { conn })
    }
    pub fn open(path: Option<&std::path::Path>) -> Sql<Self> {
        let fresh = crate::purge::is_fresh(path);
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        if fresh {
            crate::purge::stamp(&conn)?;
        }
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        crate::purge::prepare(&conn)?;
        // A projection from before P7 (no branch rows) or before the row
        // map is dropped and rebuilt (the watermark goes with it).
        let exists = |name: &str| -> Sql<bool> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE name = ?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };
        // So is one from before P8's level columns, or one whose stored
        // renderings come from another `LEVELS_VERSION`.
        let version: Option<i64> = if exists("recall_levels")? {
            conn.query_row("SELECT version FROM recall_levels WHERE id = 0", [], |r| {
                r.get(0)
            })
            .optional()?
        } else {
            None
        };
        let old = exists("recall")?
            && (conn
                .prepare("SELECT branch, rev, abstract, outline_cut FROM recall LIMIT 0")
                .is_err()
                || !exists("recall_map")?
                || version != Some(levels::LEVELS_VERSION as i64)
                // Rowids from before P8 M7 followed indexing order.
                || conn
                    .query_row(
                        "SELECT 1 FROM recall_map WHERE (rid >> 16) != logical LIMIT 1",
                        [],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some());
        if old {
            conn.execute_batch(
                "DROP TABLE IF EXISTS recall_vocab; DROP TABLE recall; DROP TABLE IF EXISTS recall_watermark;
                 DROP TABLE IF EXISTS recall_map; DROP TABLE IF EXISTS recall_levels;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO recall_levels(id, version) VALUES (0, ?1)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version",
            params![levels::LEVELS_VERSION as i64],
        )?;
        Ok(Self { conn })
    }
    /// G4 fault injection: caps the file at `pages` pages, so writes past
    /// it fail as SQLite's "database or disk is full" (`None` lifts it).
    #[doc(hidden)]
    pub fn limit_pages(&self, pages: Option<u64>) -> Sql<()> {
        let n = pages.unwrap_or(4_294_967_294);
        self.conn
            .query_row(&format!("PRAGMA max_page_count = {n}"), [], |_| Ok(()))
    }
    /// Pages the file uses now.
    #[doc(hidden)]
    pub fn page_count(&self) -> Sql<u64> {
        self.conn
            .query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
    }
    /// The last shred epoch purged from this projection (ADR 0017 §8.4).
    pub fn purged_through(&self) -> Sql<Epoch> {
        crate::purge::through(&self.conn)
    }
    /// Re-indexes the rows of logicals that showed a shredded payload (with
    /// the payload now unavailable, their body, abstract and outline are
    /// empty), merges the FTS segments so the old ones are freed (and
    /// zeroed by `secure_delete`), records `through` and truncates the WAL.
    pub fn purge(&mut self, entries: &[(LogicalId, Vec<Row>)], through: Epoch) -> Sql<()> {
        self.write(None, entries)?;
        self.conn
            .execute("INSERT INTO recall(recall) VALUES ('optimize')", [])?;
        crate::purge::set_through(&self.conn, through)?;
        crate::purge::compact(&self.conn)
    }
    pub fn watermark(&self) -> Sql<Epoch> {
        Ok(Epoch(
            self.conn
                .query_row("SELECT epoch FROM recall_watermark WHERE id = 0", [], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()?
                .unwrap_or(0) as u64,
        ))
    }
    /// Replaces every row of each listed logical with the given rows and
    /// advances the watermark, in one transaction.
    pub fn update(&mut self, epoch: Epoch, entries: &[(LogicalId, Vec<Row>)]) -> Sql<()> {
        self.write(Some(epoch), entries)
    }
    /// As [`RecallIndex::update`], without moving the watermark: one chunk
    /// of a rebuild, which is complete only when the last chunk sets it.
    pub fn update_partial(&mut self, entries: &[(LogicalId, Vec<Row>)]) -> Sql<()> {
        self.write(None, entries)
    }
    fn write(&mut self, epoch: Option<Epoch>, entries: &[(LogicalId, Vec<Row>)]) -> Sql<()> {
        let tx = self.conn.transaction()?;
        for (l, rows) in entries {
            tx.prepare_cached(
                "DELETE FROM recall WHERE rowid IN (SELECT rid FROM recall_map WHERE logical = ?1)",
            )?
            .execute(params![l.0])?;
            tx.prepare_cached("DELETE FROM recall_map WHERE logical = ?1")?
                .execute(params![l.0])?;
            for row in rows {
                let rid = row_id(*l, row.branch);
                tx.prepare_cached(
                    "INSERT INTO recall(rowid, key, kind, scope, body, logical, branch, rev,
                         abstract, abstract_cut, outline, outline_cut)
                     VALUES (?12, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )?
                .execute(params![
                    row.key,
                    format!("{:?}", row.kind),
                    row.scope,
                    row.body,
                    l.0,
                    row.branch.0,
                    row.rev.0,
                    row.abstract_.text,
                    row.abstract_.cut,
                    row.outline.text,
                    row.outline.cut,
                    rid
                ])?;
                tx.prepare_cached("INSERT INTO recall_map(rid, logical) VALUES (?1, ?2)")?
                    .execute(params![rid, l.0])?;
            }
        }
        if let Some(epoch) = epoch {
            tx.execute(
                "INSERT INTO recall_watermark(id, epoch) VALUES (0, ?1)
                 ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch",
                params![epoch.0 as i64],
            )?;
        }
        tx.commit()
    }
    /// The stored renderings of revision `rev` of `logical`, if a row holds
    /// it (ADR 0016 §4.1).
    pub fn levels(&self, logical: LogicalId, rev: RevisionId) -> Sql<Option<Levels>> {
        self.conn
            .prepare_cached(
                "SELECT abstract, abstract_cut, outline, outline_cut FROM recall
                 WHERE rowid IN (SELECT rid FROM recall_map WHERE logical = ?1) AND rev = ?2
                 LIMIT 1",
            )?
            .query_row(params![logical.0, rev.0], |r| {
                Ok(Levels {
                    abstract_: levels::Rendered {
                        text: r.get(0)?,
                        cut: r.get(1)?,
                    },
                    outline: levels::Rendered {
                        text: r.get(2)?,
                        cut: r.get(3)?,
                    },
                })
            })
            .optional()
    }
    /// Number of indexed rows (rebuild progress, ADR 0016 §2.2).
    pub fn rows(&self) -> Sql<u64> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM recall_map", [], |r| {
                r.get::<_, i64>(0)
            })? as u64)
    }
    pub fn clear(&mut self) -> Sql<()> {
        self.conn.execute_batch(
            "DELETE FROM recall; DELETE FROM recall_map; DELETE FROM recall_watermark;",
        )
    }
    /// Runs `f` inside one read transaction, so every query in it sees the
    /// same index state (WAL: the writer is never blocked). Paging through
    /// [`Self::search_page`] or [`Self::any_of_page`] is consistent only
    /// inside one snapshot; outside one, a write between pages can shift
    /// rows past a page boundary.
    pub fn read_snapshot<T>(&self, f: impl FnOnce(&Self) -> T) -> T {
        let began = self.conn.execute_batch("BEGIN").is_ok();
        let out = f(self);
        if began {
            let _ = self.conn.execute_batch("COMMIT");
        }
        out
    }
    /// The matching rows, best bm25 first, from the `offset`-th on.
    ///
    /// bm25 scores every row a query matches, so a term most rows hold
    /// would cost time linear in the store (P8 G5). Matches are therefore
    /// ranked in windows of [`RANK_WINDOW`] rows, highest rowid (latest
    /// created logical) first: window 0 is those `RANK_WINDOW` matches in
    /// bm25 order, window 1 the next ones, and so on. A query with at most
    /// `RANK_WINDOW` matches has one window, the plain bm25 order. Paging
    /// walks every window, so every match is still reached; the pages agree
    /// only within one read snapshot ([`RecallIndex::read_snapshot`]).
    fn hits(
        &self,
        q: &str,
        scope: &str,
        branch: BranchId,
        limit: usize,
        offset: usize,
    ) -> Sql<Vec<Hit>> {
        // The window bounds and the ranked query must see one state.
        self.read_snapshot(|s| s.hits_in(q, scope, branch, limit, offset))
    }
    fn hits_in(
        &self,
        q: &str,
        scope: &str,
        branch: BranchId,
        limit: usize,
        offset: usize,
    ) -> Sql<Vec<Hit>> {
        let mut out = Vec::new();
        let (mut k, mut skip) = (offset / RANK_WINDOW, offset % RANK_WINDOW);
        let mut ranked = self.conn.prepare_cached(
            "SELECT logical, rev, bm25(recall) FROM recall
             WHERE recall MATCH ?1 AND (?2 = '' OR scope = ?2) AND (branch = 0 OR branch = ?3)
               AND rowid <= ?6 AND rowid >= ?7
             ORDER BY rank, rowid LIMIT ?4 OFFSET ?5",
        )?;
        while out.len() < limit {
            // Window k holds the matches from the (k·W)-th highest rowid to the
            // ((k+1)·W − 1)-th; no k·W-th match means no window k.
            let hi = match k {
                0 => i64::MAX,
                _ => match self.nth_highest(q, scope, branch, k * RANK_WINDOW)? {
                    Some(r) => r,
                    None => break,
                },
            };
            let lo = self.nth_highest(q, scope, branch, (k + 1) * RANK_WINDOW - 1)?;
            let rows = ranked.query_map(
                params![
                    q,
                    scope,
                    branch.0,
                    (limit - out.len()) as i64,
                    skip as i64,
                    hi,
                    lo.unwrap_or(i64::MIN)
                ],
                |r| {
                    Ok(Hit {
                        logical: LogicalId(r.get(0)?),
                        rev: RevisionId(r.get(1)?),
                        score: -r.get::<_, f64>(2)?,
                    })
                },
            )?;
            for h in rows {
                out.push(h?);
            }
            if lo.is_none() {
                break;
            }
            (k, skip) = (k + 1, 0);
        }
        Ok(out)
    }
    /// The `n`-th highest rowid among the matching rows (from 0), without
    /// scoring: FTS5 walks its doclists in rowid order.
    fn nth_highest(&self, q: &str, scope: &str, branch: BranchId, n: usize) -> Sql<Option<i64>> {
        self.conn
            .prepare_cached(
                "SELECT rowid FROM recall
                 WHERE recall MATCH ?1 AND (?2 = '' OR scope = ?2) AND (branch = 0 OR branch = ?3)
                 ORDER BY rowid DESC LIMIT 1 OFFSET ?4",
            )?
            .query_row(params![q, scope, branch.0, n as i64], |r| r.get(0))
            .optional()
    }
    /// Best-ranked rows matching every term of `text` among `main`'s and
    /// `branch`'s rows, optionally within one scope lane. The caller keeps a
    /// hit only when its revision is the head on `branch`.
    pub fn search(&self, text: &str, scope: &str, branch: BranchId, limit: usize) -> Sql<Vec<Hit>> {
        self.search_page(text, scope, branch, limit, 0)
    }
    /// [`Self::search`] from the `offset`-th hit on (ADR 0016 §3.2: rankers
    /// page until they hold enough eligible candidates).
    pub fn search_page(
        &self,
        text: &str,
        scope: &str,
        branch: BranchId,
        limit: usize,
        offset: usize,
    ) -> Sql<Vec<Hit>> {
        match fts_query(text) {
            Some(q) => self.hits(&q, scope, branch, limit, offset),
            None => Ok(Vec::new()),
        }
    }
    /// At most `n` terms of `text` with the highest idf among those at least
    /// two rows contain (a term only one row holds can pair with nothing).
    /// Ties go to the term that sorts first.
    pub fn top_terms(&self, text: &str, n: usize) -> Sql<Vec<String>> {
        let total: f64 = self
            .conn
            .query_row("SELECT count(*) FROM recall_map", [], |r| {
                r.get::<_, i64>(0)
            })? as f64;
        let mut stmt = self
            .conn
            .prepare_cached("SELECT doc FROM recall_vocab WHERE term = ?1")?;
        let mut scored = Vec::new();
        for t in terms(text) {
            let df: i64 = stmt
                .query_row(params![t], |r| r.get(0))
                .optional()?
                .unwrap_or(0);
            if df >= 2 {
                let df = df as f64;
                scored.push((((total - df + 0.5) / (df + 0.5)).ln(), t));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        Ok(scored.into_iter().take(n).map(|(_, t)| t).collect())
    }
    /// Rows matching any of `terms` in their key or body, best bm25 first,
    /// among `main`'s and `branch`'s rows within one scope lane.
    pub fn any_of(
        &self,
        terms: &[String],
        scope: &str,
        branch: BranchId,
        limit: usize,
    ) -> Sql<Vec<Hit>> {
        self.any_of_page(terms, scope, branch, limit, 0)
    }
    /// [`Self::any_of`] from the `offset`-th hit on.
    pub fn any_of_page(
        &self,
        terms: &[String],
        scope: &str,
        branch: BranchId,
        limit: usize,
        offset: usize,
    ) -> Sql<Vec<Hit>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let q: Vec<String> = (terms.iter())
            .map(|t| format!("\"{}\"", t.replace('"', "")))
            .collect();
        self.hits(
            &format!("{{key body}} : ({})", q.join(" OR ")),
            scope,
            branch,
            limit,
            offset,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn row(branch: u16, rev: u32, key: &str, kind: RecordKind, scope: &str, body: &str) -> Row {
        Row {
            branch: BranchId(branch),
            rev: RevisionId(rev),
            key: key.into(),
            kind,
            scope: scope.into(),
            body: body.into(),
            abstract_: levels::abstract_of(body.as_bytes()),
            outline: levels::outline_of(body.as_bytes()),
        }
    }
    fn logicals(hits: Vec<Hit>) -> Vec<LogicalId> {
        hits.into_iter().map(|h| h.logical).collect()
    }
    /// P8 G5: more matches than one bm25 window. Paging reaches every
    /// match exactly once, window 0 is the highest-rowid `RANK_WINDOW` rows in
    /// bm25 order, and scope filters count within the windows.
    #[test]
    fn ranking_windows_page_through_every_match() {
        let mut idx = RecallIndex::open(None).unwrap();
        let n = RANK_WINDOW * 2 + 300;
        let entries: Vec<(LogicalId, Vec<Row>)> = (0..n as u32)
            .map(|i| {
                // Every row holds "common"; every 7th repeats it (higher
                // bm25). Odd rows are in another lane.
                let body = if i % 7 == 0 {
                    "common common common x"
                } else {
                    "common y z w v"
                };
                let scope = if i % 2 == 0 { "repo" } else { "other" };
                (
                    LogicalId(i),
                    vec![row(
                        0,
                        i,
                        &format!("k{i}"),
                        RecordKind::Decision,
                        scope,
                        body,
                    )],
                )
            })
            .collect();
        idx.update(Epoch(1), &entries).unwrap();
        for (scope, total) in [("", n), ("repo", n.div_ceil(2))] {
            let mut seen = Vec::new();
            for page in [7usize, 100, 1000] {
                let mut got = Vec::new();
                let mut offset = 0;
                loop {
                    let hits = idx
                        .search_page("common", scope, BranchId(0), page, offset)
                        .unwrap();
                    let len = hits.len();
                    got.extend(logicals(hits));
                    if len < page {
                        break;
                    }
                    offset += page;
                }
                assert_eq!(got.len(), total, "{scope} {page}");
                let mut sorted = got.clone();
                sorted.sort();
                sorted.dedup();
                assert_eq!(sorted.len(), total, "no repeats");
                seen.push(got);
            }
            assert!(
                seen.windows(2).all(|w| w[0] == w[1]),
                "page size never changes the order"
            );
            // Window 0: the RANK_WINDOW matches with the highest logical
            // ids, repeated bodies first.
            let first = &seen[0][..RANK_WINDOW];
            let mut newest: Vec<u32> = seen[0].iter().map(|l| l.0).collect();
            newest.sort_unstable_by(|a, b| b.cmp(a));
            let mut w0: Vec<u32> = first.iter().map(|l| l.0).collect();
            w0.sort_unstable_by(|a, b| b.cmp(a));
            assert_eq!(w0, newest[..RANK_WINDOW]);
            let boosted = first.iter().take_while(|l| l.0 % 7 == 0).count();
            assert!(boosted > 0 && first[boosted..].iter().all(|l| l.0 % 7 != 0));
        }
        // Under one window the order is the plain bm25 order.
        let small = idx.search_page("x", "", BranchId(0), 10_000, 0).unwrap();
        assert!(small.windows(2).all(|w| w[0].score >= w[1].score));
    }
    fn common_rows(n: u32) -> Vec<(LogicalId, Vec<Row>)> {
        (0..n)
            .map(|i| {
                let body = if i % 7 == 0 {
                    "common common common x"
                } else {
                    "common y z w v"
                };
                (
                    LogicalId(i),
                    vec![row(
                        0,
                        i,
                        &format!("k{i}"),
                        RecordKind::Decision,
                        "repo",
                        body,
                    )],
                )
            })
            .collect()
    }
    fn all_pages(idx: &RecallIndex, page: usize) -> Vec<LogicalId> {
        idx.read_snapshot(|idx| {
            let mut got = Vec::new();
            let mut offset = 0;
            loop {
                let hits = idx
                    .search_page("common", "", BranchId(0), page, offset)
                    .unwrap();
                let len = hits.len();
                got.extend(logicals(hits));
                if len < page {
                    return got;
                }
                offset += page;
            }
        })
    }
    /// Review M2: windows depend only on the rows, not on the order they
    /// were indexed in, so a rebuild returns the same pages.
    #[test]
    fn ranking_windows_survive_a_rebuild_in_another_order() {
        let n = RANK_WINDOW as u32 * 2 + 100;
        let mut a = RecallIndex::open(None).unwrap();
        a.update(Epoch(1), &common_rows(n)).unwrap();
        // Indexed in reverse, then some rows rewritten (as head moves do).
        let mut b = RecallIndex::open(None).unwrap();
        let mut rows = common_rows(n);
        rows.reverse();
        b.update(Epoch(1), &rows).unwrap();
        let again: Vec<_> = common_rows(n).into_iter().step_by(5).collect();
        b.update(Epoch(2), &again).unwrap();
        assert_eq!(all_pages(&a, 100), all_pages(&b, 100));
    }
    /// Review M1: pages read inside one snapshot agree while another
    /// connection writes between them.
    #[test]
    fn pages_in_one_snapshot_ignore_a_concurrent_write() {
        let path = std::env::temp_dir().join(format!("ctx-recall-snap-{}.db", std::process::id()));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
        let mut w = RecallIndex::open(Some(&path)).unwrap();
        w.update(Epoch(1), &common_rows(300)).unwrap();
        let r = RecallIndex::open_reader(&path).unwrap();
        let before = all_pages(&r, 50);
        let during = r.read_snapshot(|r| {
            let first = logicals(r.search_page("common", "", BranchId(0), 50, 0).unwrap());
            // The writer drops the first page's rows from the matches.
            let gone: Vec<(LogicalId, Vec<Row>)> = (first.iter())
                .map(|l| {
                    (
                        *l,
                        vec![row(0, l.0, "k", RecordKind::Decision, "repo", "other")],
                    )
                })
                .collect();
            w.update(Epoch(2), &gone).unwrap();
            let mut all = first;
            let mut offset = 50;
            loop {
                let hits = r
                    .search_page("common", "", BranchId(0), 50, offset)
                    .unwrap();
                let len = hits.len();
                all.extend(logicals(hits));
                if len < 50 {
                    return all;
                }
                offset += 50;
            }
        });
        assert_eq!(during, before, "the snapshot saw no part of the write");
        assert_eq!(all_pages(&r, 50).len(), 250, "a new snapshot sees it");
        drop((w, r));
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
        }
    }
    /// A projection whose rowids followed indexing order (before P8 M7) is
    /// dropped and rebuilt (its watermark goes with it).
    #[test]
    fn a_projection_with_ordinal_rowids_is_rebuilt() {
        let path = std::env::temp_dir().join(format!("ctx-recall-rid-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut idx = RecallIndex::open(Some(&path)).unwrap();
            idx.update(Epoch(4), &common_rows(3)).unwrap();
            assert_eq!(idx.watermark().unwrap(), Epoch(4));
        }
        assert_eq!(
            RecallIndex::open(Some(&path)).unwrap().watermark().unwrap(),
            Epoch(4)
        );
        {
            let idx = RecallIndex::open(Some(&path)).unwrap();
            idx.conn
                .execute("UPDATE recall_map SET rid = 2 WHERE logical = 1", [])
                .unwrap();
        }
        let idx = RecallIndex::open(Some(&path)).unwrap();
        assert_eq!(
            (idx.watermark().unwrap(), idx.rows().unwrap()),
            (Epoch(0), 0)
        );
        drop(idx);
        let _ = std::fs::remove_file(&path);
    }
    /// `ctx ui` reads abstracts without writing: a missing file or one
    /// without current levels gives nothing.
    #[test]
    fn read_abstracts_is_read_only_and_versioned() {
        let path = std::env::temp_dir().join(format!("ctx-recall-ro-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert!(read_abstracts(&path).unwrap().is_empty());
        assert!(!path.exists(), "nothing created");
        {
            let mut idx = RecallIndex::open(Some(&path)).unwrap();
            idx.update(
                Epoch(1),
                &[(
                    LogicalId(3),
                    vec![row(
                        0,
                        7,
                        "k",
                        RecordKind::Decision,
                        "repo",
                        "# T\n\nfirst para\n\nmore",
                    )],
                )],
            )
            .unwrap();
        }
        let got = read_abstracts(&path).unwrap();
        let r = &got[&(LogicalId(3), RevisionId(7))];
        assert_eq!((r.text.as_str(), r.cut), ("first para", true));
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE recall_levels SET version = 999", [])
            .unwrap();
        drop(conn);
        assert!(read_abstracts(&path).unwrap().is_empty(), "other version");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
    #[test]
    fn indexes_text_replaces_rows_and_never_fails_on_odd_queries() {
        let mut idx = RecallIndex::open(None).unwrap();
        idx.update(
            Epoch(1),
            &[
                (
                    LogicalId(0),
                    vec![row(
                        0,
                        0,
                        "d1-db",
                        RecordKind::Decision,
                        "repo",
                        "Use MongoDB for documents",
                    )],
                ),
                (
                    LogicalId(1),
                    vec![row(
                        0,
                        1,
                        "plan",
                        RecordKind::Plan,
                        "ops",
                        "deploy the cluster",
                    )],
                ),
            ],
        )
        .unwrap();
        let s = |idx: &RecallIndex, q: &str, scope: &str| {
            logicals(idx.search(q, scope, MAIN, 10).unwrap())
        };
        assert_eq!(s(&idx, "mongodb", ""), vec![LogicalId(0)]);
        assert!(s(&idx, "cluster", "repo").is_empty());
        assert_eq!(s(&idx, "cluster", "ops"), vec![LogicalId(1)]);
        // A new head replaces the logical's row.
        idx.update(
            Epoch(2),
            &[(
                LogicalId(0),
                vec![row(
                    0,
                    2,
                    "d1-db",
                    RecordKind::Decision,
                    "repo",
                    "Use PostgreSQL",
                )],
            )],
        )
        .unwrap();
        assert!(s(&idx, "mongodb", "").is_empty());
        assert_eq!(s(&idx, "postgresql", ""), vec![LogicalId(0)]);
        assert_eq!(idx.watermark().unwrap(), Epoch(2));
        for odd in ["\"", "AND OR NOT", "a*", "(", "col:x", "", "  "] {
            assert!(idx.search(odd, "", MAIN, 5).is_ok(), "{odd:?}");
        }
    }
    #[test]
    fn branch_rows_are_read_with_main_and_carry_their_revision() {
        let mut idx = RecallIndex::open(None).unwrap();
        idx.update(
            Epoch(1),
            &[(
                LogicalId(0),
                vec![
                    row(0, 0, "db", RecordKind::Decision, "repo", "use postgres"),
                    row(1, 5, "db", RecordKind::Decision, "repo", "use sqlite"),
                    row(2, 6, "db", RecordKind::Decision, "repo", "use sqlite too"),
                ],
            )],
        )
        .unwrap();
        // Branch 1 sees main's row and its own; branch 2's is not read.
        let hits = idx.search("use", "", BranchId(1), 10).unwrap();
        let mut revs: Vec<u32> = hits.iter().map(|h| h.rev.0).collect();
        revs.sort();
        assert_eq!(revs, vec![0, 5]);
        assert!(idx.search("sqlite", "", MAIN, 10).unwrap().is_empty());
    }
    #[test]
    fn top_terms_prefer_rare_shared_terms_and_any_of_scores_them() {
        let mut idx = RecallIndex::open(None).unwrap();
        let mut entries = Vec::new();
        for i in 0..20u32 {
            entries.push((
                LogicalId(i),
                vec![row(
                    0,
                    i,
                    &format!("k{i}"),
                    RecordKind::Fact,
                    "repo",
                    "the service runs in the cluster",
                )],
            ));
        }
        entries.push((
            LogicalId(20),
            vec![row(
                0,
                20,
                "db",
                RecordKind::Decision,
                "repo",
                "the service stores sessions in redis",
            )],
        ));
        entries.push((
            LogicalId(21),
            vec![row(
                0,
                21,
                "db2",
                RecordKind::Decision,
                "repo",
                "sessions are never stored in redis",
            )],
        ));
        idx.update(Epoch(1), &entries).unwrap();
        let t = idx
            .top_terms("the service stores sessions in redis", 2)
            .unwrap();
        assert_eq!(t, vec!["redis".to_string(), "sessions".to_string()]);
        let hits = idx.any_of(&t, "repo", MAIN, 10).unwrap();
        assert_eq!(
            logicals(hits.clone()),
            vec![LogicalId(20), LogicalId(21)]
                .into_iter()
                .collect::<Vec<_>>()
        );
        assert!(hits.iter().all(|h| h.score > 0.0));
        assert!(idx.any_of(&t, "other", MAIN, 10).unwrap().is_empty());
        assert!(idx.any_of(&[], "repo", MAIN, 10).unwrap().is_empty());
    }
    #[test]
    fn a_pre_p7_projection_is_dropped_and_rebuilt() {
        let dir = std::env::temp_dir().join(format!("recall-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recall.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE VIRTUAL TABLE recall USING fts5(key, kind, scope, body, logical UNINDEXED);
                 CREATE TABLE recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);
                 INSERT INTO recall VALUES ('k', 'Fact', 'repo', 'old', 0);
                 INSERT INTO recall_watermark VALUES (0, 9);",
            )
            .unwrap();
        }
        let idx = RecallIndex::open(Some(&path)).unwrap();
        assert_eq!(idx.watermark().unwrap(), Epoch(0));
        assert!(idx.search("old", "", MAIN, 5).unwrap().is_empty());
        drop(idx);
        // Reopening a current projection keeps it.
        let mut idx = RecallIndex::open(Some(&path)).unwrap();
        idx.update(Epoch(3), &[]).unwrap();
        drop(idx);
        assert_eq!(
            RecallIndex::open(Some(&path)).unwrap().watermark().unwrap(),
            Epoch(3)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A P7 projection (no level columns) and one whose stored renderings
    /// come from another `LEVELS_VERSION` are rebuilt; a current one keeps
    /// its rows and returns its renderings (ADR 0016 §4.1).
    #[test]
    fn a_projection_without_current_levels_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!("recall-levels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recall.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE VIRTUAL TABLE recall USING fts5(
                     key, kind, scope, body, logical UNINDEXED, branch UNINDEXED, rev UNINDEXED);
                 CREATE TABLE recall_map(rid INTEGER PRIMARY KEY, logical INTEGER NOT NULL);
                 CREATE TABLE recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);
                 INSERT INTO recall_watermark VALUES (0, 9);",
            )
            .unwrap();
        }
        let mut idx = RecallIndex::open(Some(&path)).unwrap();
        assert_eq!(idx.watermark().unwrap(), Epoch(0), "P7 schema rebuilt");
        let body = "# T\n\nFirst.\n\n## A\nalpha\n";
        idx.update(
            Epoch(4),
            &[(
                LogicalId(1),
                vec![row(0, 7, "k", RecordKind::Fact, "repo", body)],
            )],
        )
        .unwrap();
        let got = idx.levels(LogicalId(1), RevisionId(7)).unwrap().unwrap();
        assert_eq!(got.abstract_, levels::abstract_of(body.as_bytes()));
        assert_eq!(got.outline, levels::outline_of(body.as_bytes()));
        assert_eq!(idx.levels(LogicalId(1), RevisionId(8)).unwrap(), None);
        assert_eq!(idx.rows().unwrap(), 1);
        drop(idx);
        // Same version: kept.
        assert_eq!(
            RecallIndex::open(Some(&path)).unwrap().watermark().unwrap(),
            Epoch(4)
        );
        // Another version: rebuilt.
        Connection::open(&path)
            .unwrap()
            .execute("UPDATE recall_levels SET version = 0", [])
            .unwrap();
        assert_eq!(
            RecallIndex::open(Some(&path)).unwrap().watermark().unwrap(),
            Epoch(0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A milestone-6 projection (branch rows, no row map) is rebuilt too;
    /// updates replace exactly the listed logical's rows through the map.
    #[test]
    fn an_index_without_the_row_map_is_rebuilt_and_updates_replace_rows() {
        let dir = std::env::temp_dir().join(format!("recall-map-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recall.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE VIRTUAL TABLE recall USING fts5(
                     key, kind, scope, body, logical UNINDEXED, branch UNINDEXED, rev UNINDEXED);
                 CREATE TABLE recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);
                 INSERT INTO recall VALUES ('k', 'Fact', 'repo', 'old', 0, 0, 0);
                 INSERT INTO recall_watermark VALUES (0, 9);",
            )
            .unwrap();
        }
        let mut idx = RecallIndex::open(Some(&path)).unwrap();
        assert_eq!(idx.watermark().unwrap(), Epoch(0));
        assert!(idx.search("old", "", MAIN, 5).unwrap().is_empty());
        let r = |branch, rev, body: &str| row(branch, rev, "k", RecordKind::Fact, "repo", body);
        idx.update(
            Epoch(1),
            &[
                (LogicalId(0), vec![r(0, 0, "alpha"), r(1, 1, "beta")]),
                (LogicalId(1), vec![r(0, 2, "alpha")]),
            ],
        )
        .unwrap();
        idx.update(Epoch(2), &[(LogicalId(0), vec![r(0, 3, "gamma")])])
            .unwrap();
        let logicals = |q: &str| -> Vec<u32> {
            let mut v: Vec<u32> = (idx.search(q, "", BranchId(1), 9).unwrap().iter())
                .map(|h| h.logical.0)
                .collect();
            v.sort();
            v
        };
        assert_eq!(logicals("alpha"), vec![1]);
        assert!(logicals("beta").is_empty());
        assert_eq!(logicals("gamma"), vec![0]);
        drop(idx);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
