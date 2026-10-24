//! Rebuildable SQLite projections (ADR 0006 §1, §8): diff_log, head_history,
//! base_history and a watermark naming the log checksum at its epoch. Never a
//! source of truth: reads use them only when the watermark covers the epoch.
//! Schema 2 (ADR 0013 §8) keys every history by branch and adds the branch
//! table, `branch_visibility` and `rule_enabled`; a schema mismatch clears
//! the tables for a rebuild from the log.
use contextdb_core::EpochDiff;
use contextdb_model::*;
use rusqlite::{Connection, OptionalExtension, params};

/// Where projection tables live: a file next to the log, or memory (rebuilt on open).
#[derive(Clone, Debug)]
pub enum ProjectionFile {
    Memory,
    Path(std::path::PathBuf),
}
pub struct Projections {
    conn: Connection,
    watermark: Epoch,
    /// A write failed after a durable commit; reads skip projections until reopen.
    stale: bool,
    /// G4 fault injection: the next `apply` fails before its transaction commits.
    fail_next: bool,
    bulk: Option<Bulk>,
}
/// Post-states and head/base movements of one committed epoch.
pub struct EpochRows<'a> {
    pub epoch: Epoch,
    /// The batch's branch; every row below belongs to it.
    pub branch: BranchId,
    /// A branch this epoch created (id, name, parent) or archived.
    pub created: Option<(BranchId, String)>,
    pub archived: bool,
    /// Objects that became visible on the branch: (kind 0–3, global id).
    pub visible: Vec<(u8, u32)>,
    /// Rules whose `enabled` this epoch set on the branch.
    pub enabled: Vec<(RuleId, bool)>,
    pub checksum: ContentHash,
    pub result: &'a CommitResult,
    pub base: &'a [(RevisionId, bool)],
    /// Watermark advances: (scope, new position).
    pub watermarks: Vec<(String, Vec<u8>)>,
    pub posts: Vec<(RevisionId, NodeState)>,
}
type Sql<T> = rusqlite::Result<T>;
/// An open bulk transaction: the watermark it started from and the checksum
/// of the last epoch written in it.
struct Bulk {
    start: Epoch,
    checksum: Option<ContentHash>,
}
/// One epoch's rows, through cached statements.
fn write_rows(conn: &Connection, rows: &EpochRows<'_>) -> Sql<()> {
    let e = rows.epoch.0 as i64;
    let b = rows.branch.0;
    let mut diff = conn.prepare_cached(
        "INSERT INTO diff_log(branch, epoch, revision, post) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (r, s) in &rows.posts {
        diff.execute(params![b, e, r.0, s.canonical_bytes()])?;
    }
    for (l, old, new) in &rows.result.net_delta.head_moves {
        if old.is_some() {
            conn.prepare_cached(
                "UPDATE head_history SET to_epoch = ?1
                 WHERE branch = ?2 AND logical_id = ?3 AND to_epoch IS NULL",
            )?
            .execute(params![e, b, l.0])?;
        }
        conn.prepare_cached(
            "INSERT INTO head_history(branch, logical_id, rev_id, from_epoch) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![b, l.0, new.0, e])?;
    }
    for (r, asserted) in rows.base {
        if *asserted {
            conn.prepare_cached(
                "INSERT INTO base_history(branch, rev_id, asserted_epoch) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![b, r.0, e])?;
        } else {
            conn.prepare_cached(
                "UPDATE base_history SET retracted_epoch = ?1
                 WHERE branch = ?2 AND rev_id = ?3 AND retracted_epoch IS NULL",
            )?
            .execute(params![e, b, r.0])?;
        }
    }
    for (scope, position) in &rows.watermarks {
        conn.prepare_cached(
            "INSERT INTO watermark_history(branch, scope, position, from_epoch) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![b, scope, position, e])?;
    }
    if let Some((id, name)) = &rows.created {
        conn.prepare_cached(
            "INSERT INTO branches(id, name, parent, fork_epoch) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![id.0, name, b, e])?;
    }
    if rows.archived {
        conn.prepare_cached("UPDATE branches SET archived_epoch = ?1 WHERE id = ?2")?
            .execute(params![e, b])?;
    }
    for (kind, id) in &rows.visible {
        conn.prepare_cached(
            "INSERT INTO branch_visibility(branch, kind, id, epoch) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![b, kind, id, e])?;
    }
    for (j, enabled) in &rows.enabled {
        conn.prepare_cached(
            "INSERT INTO rule_enabled(branch, rule_id, enabled, from_epoch) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![b, j.0, enabled, e])?;
    }
    Ok(())
}
fn write_watermark(conn: &Connection, epoch: Epoch, checksum: &ContentHash) -> Sql<()> {
    conn.prepare_cached(
        "INSERT INTO projection_watermark(id, epoch, log_checksum) VALUES (0, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch, log_checksum = excluded.log_checksum",
    )?
    .execute(params![epoch.0 as i64, checksum.as_slice()])?;
    Ok(())
}
/// Bump with every incompatible table change: a mismatch clears all tables.
const SCHEMA_VERSION: i64 = 2;
const TABLES: [&str; 9] = [
    "projection_watermark",
    "diff_log",
    "head_history",
    "base_history",
    "watermark_history",
    "branches",
    "branch_visibility",
    "rule_enabled",
    "projection_schema",
];
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projection_schema(
    id INTEGER PRIMARY KEY CHECK (id = 0), version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS projection_watermark(
    id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL, log_checksum BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS diff_log(
    branch INTEGER NOT NULL, epoch INTEGER NOT NULL, revision INTEGER NOT NULL, post BLOB NOT NULL,
    PRIMARY KEY (branch, revision, epoch));
CREATE INDEX IF NOT EXISTS diff_log_epoch ON diff_log(branch, epoch);
CREATE TABLE IF NOT EXISTS head_history(
    branch INTEGER NOT NULL, logical_id INTEGER NOT NULL, rev_id INTEGER NOT NULL,
    from_epoch INTEGER NOT NULL, to_epoch INTEGER);
CREATE INDEX IF NOT EXISTS head_history_logical ON head_history(branch, logical_id, from_epoch);
CREATE TABLE IF NOT EXISTS base_history(
    branch INTEGER NOT NULL, rev_id INTEGER NOT NULL,
    asserted_epoch INTEGER NOT NULL, retracted_epoch INTEGER);
CREATE INDEX IF NOT EXISTS base_history_rev ON base_history(branch, rev_id, asserted_epoch);
CREATE TABLE IF NOT EXISTS watermark_history(
    branch INTEGER NOT NULL, scope TEXT NOT NULL, position BLOB NOT NULL, from_epoch INTEGER NOT NULL,
    PRIMARY KEY (branch, scope, from_epoch));
CREATE TABLE IF NOT EXISTS branches(
    id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, parent INTEGER,
    fork_epoch INTEGER NOT NULL, archived_epoch INTEGER);
CREATE TABLE IF NOT EXISTS branch_visibility(
    branch INTEGER NOT NULL, kind INTEGER NOT NULL, id INTEGER NOT NULL, epoch INTEGER NOT NULL,
    PRIMARY KEY (branch, kind, id));
CREATE TABLE IF NOT EXISTS rule_enabled(
    branch INTEGER NOT NULL, rule_id INTEGER NOT NULL, enabled INTEGER NOT NULL,
    from_epoch INTEGER NOT NULL, PRIMARY KEY (branch, rule_id, from_epoch));
";
impl Projections {
    /// Opens the tables and returns the watermark if it names `checksums`
    /// (the verified log); otherwise the tables are cleared for a rebuild.
    pub fn open(file: &ProjectionFile, checksums: &[ContentHash]) -> Sql<Self> {
        let conn = match file {
            ProjectionFile::Memory => Connection::open_in_memory()?,
            ProjectionFile::Path(p) => Connection::open(p)?,
        };
        // Rebuildable: NORMAL suffices; a lost tail only lowers the watermark.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        // An older schema is dropped wholesale and rebuilt from the log.
        let version: Option<i64> = conn
            .query_row(
                "SELECT version FROM projection_schema WHERE id = 0",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None);
        if version != Some(SCHEMA_VERSION) {
            for t in TABLES {
                conn.execute_batch(&format!("DROP TABLE IF EXISTS {t};"))?;
            }
        }
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO projection_schema(id, version) VALUES (0, ?1)
             ON CONFLICT(id) DO UPDATE SET version = excluded.version",
            params![SCHEMA_VERSION],
        )?;
        let mark: Option<(i64, Vec<u8>)> = conn
            .query_row(
                "SELECT epoch, log_checksum FROM projection_watermark WHERE id = 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let valid = mark.as_ref().and_then(|(e, sum)| {
            let e = u64::try_from(*e).ok()?;
            let expected = match e {
                0 => crate::log::GENESIS,
                n => *checksums.get(usize::try_from(n - 1).ok()?)?,
            };
            (sum.as_slice() == expected).then_some(Epoch(e))
        });
        let mut p = Self {
            conn,
            watermark: valid.unwrap_or(Epoch(0)),
            stale: false,
            fail_next: false,
            bulk: None,
        };
        if valid.is_none() {
            p.clear()?;
        }
        Ok(p)
    }
    pub fn clear(&mut self) -> Sql<()> {
        self.conn.execute_batch(
            "DELETE FROM diff_log; DELETE FROM head_history; DELETE FROM base_history;
             DELETE FROM watermark_history; DELETE FROM branches; DELETE FROM branch_visibility;
             DELETE FROM rule_enabled; DELETE FROM projection_watermark;",
        )?;
        self.watermark = Epoch(0);
        Ok(())
    }
    pub fn watermark(&self) -> Epoch {
        self.watermark
    }
    /// True when projections answer reads at `e` (ADR 0006 §8, source 2).
    pub fn covers(&self, e: Epoch) -> bool {
        !self.stale && e <= self.watermark
    }
    /// Groups many `apply` calls into one SQLite transaction (roll-forward at
    /// open, catch-up). Epochs are written without per-epoch savepoints and the
    /// watermark row once, at `end_bulk`; a crash or `abort_bulk` before it
    /// leaves tables and watermark where they were.
    pub fn begin_bulk(&mut self) -> Sql<()> {
        self.conn.execute_batch("BEGIN")?;
        self.bulk = Some(Bulk {
            start: self.watermark,
            checksum: None,
        });
        Ok(())
    }
    pub fn end_bulk(&mut self) -> Sql<()> {
        if let Some(Bulk {
            checksum: Some(sum),
            ..
        }) = self.bulk.as_ref()
        {
            write_watermark(&self.conn, self.watermark, sum)?;
        }
        self.conn.execute_batch("COMMIT")?;
        self.bulk = None;
        Ok(())
    }
    /// Discards a bulk transaction and restores the watermark.
    pub fn abort_bulk(&mut self) {
        if let Some(b) = self.bulk.take() {
            let _ = self.conn.execute_batch("ROLLBACK");
            self.watermark = b.start;
        }
    }
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }
    pub fn fail_next_write(&mut self) {
        self.fail_next = true;
    }
    /// Writes one epoch and advances the watermark atomically (its own
    /// transaction, or part of the bulk one). Only the next contiguous epoch
    /// is accepted; after a gap the tables stay behind (still exact up to the
    /// watermark) until caught up.
    pub fn apply(&mut self, rows: &EpochRows<'_>) -> Sql<()> {
        if rows.epoch.0 != self.watermark.0 + 1 {
            return Ok(());
        }
        let fail = std::mem::take(&mut self.fail_next);
        if let Some(bulk) = self.bulk.as_mut() {
            write_rows(&self.conn, rows)?;
            if fail {
                return Err(rusqlite::Error::InvalidQuery);
            }
            bulk.checksum = Some(rows.checksum);
        } else {
            let tx = self.conn.transaction()?;
            write_rows(&tx, rows)?;
            write_watermark(&tx, rows.epoch, &rows.checksum)?;
            if fail {
                // Dropping the transaction rolls back the rows written above.
                return Err(rusqlite::Error::InvalidQuery);
            }
            tx.commit()?;
        }
        self.watermark = rows.epoch;
        Ok(())
    }
    fn decode(bytes: Vec<u8>) -> Sql<NodeState> {
        NodeState::from_bytes_with(&bytes, DecodeLimits::DURABLE)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
    /// Historical(e): last post-state of `r` on `branch` at or before `e`.
    pub fn state_at(&self, branch: BranchId, r: RevisionId, e: Epoch) -> Sql<Option<NodeState>> {
        self.conn
            .query_row(
                "SELECT post FROM diff_log WHERE branch = ?1 AND revision = ?2 AND epoch <= ?3
                 ORDER BY epoch DESC LIMIT 1",
                params![branch.0, r.0, e.0 as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(Self::decode)
            .transpose()
    }
    /// Did `scope`'s watermark advance in `(since, until]`? Strict-mode history
    /// beyond the in-memory change log (ADR 0008 §6.3).
    pub fn watermark_advanced(
        &self,
        branch: BranchId,
        scope: &str,
        since: Epoch,
        until: Epoch,
    ) -> Sql<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM watermark_history WHERE branch = ?1 AND scope = ?2
             AND from_epoch > ?3 AND from_epoch <= ?4)",
            params![branch.0, scope, since.0 as i64, until.0 as i64],
            |r| r.get(0),
        )
    }
    /// Snapshot(e): the logical's head as of `e`.
    pub fn head_at(&self, branch: BranchId, l: LogicalId, e: Epoch) -> Sql<Option<RevisionId>> {
        self.conn
            .query_row(
                "SELECT rev_id FROM head_history WHERE branch = ?1 AND logical_id = ?2
                 AND from_epoch <= ?3 AND (to_epoch IS NULL OR to_epoch > ?3)",
                params![branch.0, l.0, e.0 as i64],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map(|r| r.map(RevisionId))
    }
    /// DIFF(a, b) from diff_log through the shared endpoint comparison.
    pub fn diff(&self, branch: BranchId, a: Epoch, b: Epoch) -> Sql<EpochDiff> {
        let high = a.max(b).0 as i64;
        let mut stmt = self.conn.prepare_cached(
            "SELECT epoch, revision, post FROM diff_log WHERE branch = ?1 AND epoch <= ?2
             ORDER BY epoch, revision",
        )?;
        let rows = stmt
            .query_map(params![branch.0, high], |row| {
                Ok((
                    Epoch(row.get::<_, i64>(0)? as u64),
                    RevisionId(row.get(1)?),
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .map(|r| r.and_then(|(e, rev, b)| Ok((e, rev, Self::decode(b)?))))
            .collect::<Sql<Vec<_>>>()?;
        Ok(contextdb_core::diff_rows(
            a,
            b,
            rows.iter().map(|(e, r, s)| (*e, *r, s)),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn rows<'a>(
        epoch: u64,
        branch: u16,
        result: &'a CommitResult,
        post: NodeState,
    ) -> EpochRows<'a> {
        EpochRows {
            epoch: Epoch(epoch),
            branch: BranchId(branch),
            created: None,
            archived: false,
            visible: vec![(1, 0)],
            enabled: Vec::new(),
            checksum: [epoch as u8; 32],
            result,
            base: &[],
            watermarks: Vec::new(),
            posts: vec![(RevisionId(0), post)],
        }
    }
    fn result(epoch: u64) -> CommitResult {
        CommitResult {
            epoch: Epoch(epoch),
            allocations: Allocations::default(),
            net_delta: NetDelta::default(),
            l0_flips: Vec::new(),
            status_diff: Vec::new(),
        }
    }

    /// Histories are per branch; a schema-1 file (no branch columns) is
    /// dropped and rebuilt on open (ADR 0013 §8).
    #[test]
    fn branch_keyed_rows_and_schema_rebuild() {
        let dir = std::env::temp_dir().join(format!("ctxdb-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = ProjectionFile::Path(dir.join("p.sqlite"));
        {
            // A schema-1 file: diff_log without a branch column.
            let conn = Connection::open(dir.join("p.sqlite")).unwrap();
            conn.execute_batch(
                "CREATE TABLE diff_log(epoch INTEGER NOT NULL, revision INTEGER NOT NULL,
                 post BLOB NOT NULL, PRIMARY KEY (revision, epoch));",
            )
            .unwrap();
        }
        let checksums = [[1u8; 32], [2u8; 32]];
        let mut p = Projections::open(&file, &checksums).unwrap();
        assert_eq!(p.watermark(), Epoch(0));
        let mut db = contextdb_core::Db::default();
        db.commit(&Batch::new(vec![Op::CreateLogical {
            key: "a".into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [0; 32],
            adopt_as_base: true,
        }]))
        .unwrap();
        let on_main = db.node(RevisionId(0)).unwrap();
        let mut on_b = on_main.clone();
        on_b.support = 2;
        let (r1, r2) = (result(1), result(2));
        p.apply(&rows(1, 0, &r1, on_main.clone())).unwrap();
        p.apply(&rows(2, 1, &r2, on_b.clone())).unwrap();
        assert_eq!(
            p.state_at(MAIN, RevisionId(0), Epoch(2)).unwrap(),
            Some(on_main)
        );
        assert_eq!(
            p.state_at(BranchId(1), RevisionId(0), Epoch(2)).unwrap(),
            Some(on_b)
        );
        assert_eq!(
            p.state_at(BranchId(1), RevisionId(0), Epoch(1)).unwrap(),
            None
        );
        drop(p);
        // Same schema: the watermark survives.
        let p = Projections::open(&file, &checksums).unwrap();
        assert_eq!(p.watermark(), Epoch(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
