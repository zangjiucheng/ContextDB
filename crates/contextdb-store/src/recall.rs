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
        key, kind, scope, body, logical UNINDEXED, branch UNINDEXED, rev UNINDEXED);
    CREATE VIRTUAL TABLE IF NOT EXISTS recall_vocab USING fts5vocab(recall, 'row');
    CREATE TABLE IF NOT EXISTS recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);";
impl RecallIndex {
    pub fn open(path: Option<&std::path::Path>) -> Sql<Self> {
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        // A projection from before P7 (no branch rows) is dropped and rebuilt.
        let old = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE name = 'recall'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
            && conn
                .prepare("SELECT branch, rev FROM recall LIMIT 0")
                .is_err();
        if old {
            conn.execute_batch(
                "DROP TABLE IF EXISTS recall_vocab; DROP TABLE recall; DROP TABLE IF EXISTS recall_watermark;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
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
        let tx = self.conn.transaction()?;
        for (l, rows) in entries {
            tx.prepare_cached("DELETE FROM recall WHERE logical = ?1")?
                .execute(params![l.0])?;
            for row in rows {
                tx.prepare_cached(
                    "INSERT INTO recall(key, kind, scope, body, logical, branch, rev)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?
                .execute(params![
                    row.key,
                    format!("{:?}", row.kind),
                    row.scope,
                    row.body,
                    l.0,
                    row.branch.0,
                    row.rev.0
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO recall_watermark(id, epoch) VALUES (0, ?1)
             ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch",
            params![epoch.0 as i64],
        )?;
        tx.commit()
    }
    pub fn clear(&mut self) -> Sql<()> {
        self.conn
            .execute_batch("DELETE FROM recall; DELETE FROM recall_watermark;")
    }
    fn hits(&self, q: &str, scope: &str, branch: BranchId, limit: usize) -> Sql<Vec<Hit>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT logical, rev, bm25(recall) FROM recall
             WHERE recall MATCH ?1 AND (?2 = '' OR scope = ?2) AND (branch = 0 OR branch = ?3)
             ORDER BY rank LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![q, scope, branch.0, limit as i64], |r| {
            Ok(Hit {
                logical: LogicalId(r.get(0)?),
                rev: RevisionId(r.get(1)?),
                score: -r.get::<_, f64>(2)?,
            })
        })?;
        rows.collect()
    }
    /// Best-ranked rows matching every term of `text` among `main`'s and
    /// `branch`'s rows, optionally within one scope lane. The caller keeps a
    /// hit only when its revision is the head on `branch`.
    pub fn search(&self, text: &str, scope: &str, branch: BranchId, limit: usize) -> Sql<Vec<Hit>> {
        match fts_query(text) {
            Some(q) => self.hits(&q, scope, branch, limit),
            None => Ok(Vec::new()),
        }
    }
    /// At most `n` terms of `text` with the highest idf among those at least
    /// two rows contain (a term only one row holds can pair with nothing).
    /// Ties go to the term that sorts first.
    pub fn top_terms(&self, text: &str, n: usize) -> Sql<Vec<String>> {
        let total: f64 = self
            .conn
            .query_row("SELECT count(*) FROM recall", [], |r| r.get::<_, i64>(0))?
            as f64;
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
        }
    }
    fn logicals(hits: Vec<Hit>) -> Vec<LogicalId> {
        hits.into_iter().map(|h| h.logical).collect()
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
}
