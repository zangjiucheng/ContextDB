//! Basic retrieval projection (ADR 0008 §9): an FTS5 index over each
//! logical's key, kind, scope lane and current payload text. Rebuildable and
//! never a source of truth: it only discovers candidates, whose status is
//! always evaluated on a snapshot afterwards (I6).
use contextdb_model::*;
use rusqlite::{Connection, OptionalExtension, params};

type Sql<T> = rusqlite::Result<T>;
pub struct RecallIndex {
    conn: Connection,
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
impl RecallIndex {
    pub fn open(path: Option<&std::path::Path>) -> Sql<Self> {
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE VIRTUAL TABLE IF NOT EXISTS recall USING fts5(key, kind, scope, body, logical UNINDEXED);
             CREATE TABLE IF NOT EXISTS recall_watermark(id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL);",
        )?;
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
    /// Replaces the indexed rows of `entries` and advances the watermark, in
    /// one transaction: `(logical, key, kind, scope, body text)`.
    pub fn update(
        &mut self,
        epoch: Epoch,
        entries: &[(LogicalId, String, RecordKind, String, String)],
    ) -> Sql<()> {
        let tx = self.conn.transaction()?;
        for (l, key, kind, scope, body) in entries {
            tx.prepare_cached("DELETE FROM recall WHERE logical = ?1")?
                .execute(params![l.0])?;
            tx.prepare_cached(
                "INSERT INTO recall(key, kind, scope, body, logical) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![key, format!("{kind:?}"), scope, body, l.0])?;
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
    /// Best-ranked logicals matching `text`, optionally within one scope lane.
    pub fn search(&self, text: &str, scope: &str, limit: usize) -> Sql<Vec<LogicalId>> {
        let Some(q) = fts_query(text) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT logical FROM recall WHERE recall MATCH ?1 AND (?2 = '' OR scope = ?2) ORDER BY rank LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![q, scope, limit as i64], |r| r.get::<_, u32>(0))?;
        rows.map(|r| r.map(LogicalId)).collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn indexes_text_replaces_rows_and_never_fails_on_odd_queries() {
        let mut idx = RecallIndex::open(None).unwrap();
        idx.update(
            Epoch(1),
            &[
                (
                    LogicalId(0),
                    "d1-db".into(),
                    RecordKind::Decision,
                    "repo".into(),
                    "Use MongoDB for documents".into(),
                ),
                (
                    LogicalId(1),
                    "plan".into(),
                    RecordKind::Plan,
                    "ops".into(),
                    "deploy the cluster".into(),
                ),
            ],
        )
        .unwrap();
        assert_eq!(idx.search("mongodb", "", 10).unwrap(), vec![LogicalId(0)]);
        assert_eq!(
            idx.search("cluster", "repo", 10).unwrap(),
            Vec::<LogicalId>::new()
        );
        assert_eq!(
            idx.search("cluster", "ops", 10).unwrap(),
            vec![LogicalId(1)]
        );
        // A new head replaces the logical's row.
        idx.update(
            Epoch(2),
            &[(
                LogicalId(0),
                "d1-db".into(),
                RecordKind::Decision,
                "repo".into(),
                "Use PostgreSQL".into(),
            )],
        )
        .unwrap();
        assert!(idx.search("mongodb", "", 10).unwrap().is_empty());
        assert_eq!(
            idx.search("postgresql", "", 10).unwrap(),
            vec![LogicalId(0)]
        );
        assert_eq!(idx.watermark().unwrap(), Epoch(2));
        for odd in ["\"", "AND OR NOT", "a*", "(", "col:x", "", "  "] {
            assert!(idx.search(odd, "", 5).is_ok(), "{odd:?}");
        }
    }
}
