//! Rebuildable SQLite projections (ADR 0006 §1, §8): diff_log, head_history,
//! base_history and a watermark naming the log checksum at its epoch. Never a
//! source of truth: reads use them only when the watermark covers the epoch.
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
}
/// Post-states and head/base movements of one committed epoch.
pub struct EpochRows<'a> {
    pub epoch: Epoch,
    pub checksum: ContentHash,
    pub result: &'a CommitResult,
    pub base: &'a [(RevisionId, bool)],
    pub posts: Vec<(RevisionId, NodeState)>,
}
type Sql<T> = rusqlite::Result<T>;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projection_watermark(
    id INTEGER PRIMARY KEY CHECK (id = 0), epoch INTEGER NOT NULL, log_checksum BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS diff_log(
    epoch INTEGER NOT NULL, revision INTEGER NOT NULL, post BLOB NOT NULL,
    PRIMARY KEY (revision, epoch));
CREATE INDEX IF NOT EXISTS diff_log_epoch ON diff_log(epoch);
CREATE TABLE IF NOT EXISTS head_history(
    branch INTEGER NOT NULL, logical_id INTEGER NOT NULL, rev_id INTEGER NOT NULL,
    from_epoch INTEGER NOT NULL, to_epoch INTEGER);
CREATE INDEX IF NOT EXISTS head_history_logical ON head_history(logical_id, from_epoch);
CREATE TABLE IF NOT EXISTS base_history(
    branch INTEGER NOT NULL, rev_id INTEGER NOT NULL,
    asserted_epoch INTEGER NOT NULL, retracted_epoch INTEGER);
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
        conn.execute_batch(SCHEMA)?;
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
        };
        if valid.is_none() {
            p.clear()?;
        }
        Ok(p)
    }
    fn clear(&mut self) -> Sql<()> {
        self.conn.execute_batch(
            "DELETE FROM diff_log; DELETE FROM head_history; DELETE FROM base_history;
             DELETE FROM projection_watermark;",
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
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }
    /// Writes one epoch and advances the watermark in a single transaction.
    /// Only the next contiguous epoch is accepted; after a gap the tables stay
    /// behind (still exact up to the watermark) until caught up.
    pub fn apply(&mut self, rows: &EpochRows<'_>) -> Sql<()> {
        if rows.epoch.0 != self.watermark.0 + 1 {
            return Ok(());
        }
        let e = rows.epoch.0 as i64;
        let tx = self.conn.transaction()?;
        {
            let mut diff = tx.prepare_cached(
                "INSERT INTO diff_log(epoch, revision, post) VALUES (?1, ?2, ?3)",
            )?;
            for (r, s) in &rows.posts {
                diff.execute(params![e, r.0, s.canonical_bytes()])?;
            }
            for (l, old, new) in &rows.result.net_delta.head_moves {
                if old.is_some() {
                    tx.execute(
                        "UPDATE head_history SET to_epoch = ?1 WHERE logical_id = ?2 AND to_epoch IS NULL",
                        params![e, l.0],
                    )?;
                }
                tx.execute(
                    "INSERT INTO head_history(branch, logical_id, rev_id, from_epoch) VALUES (?1, ?2, ?3, ?4)",
                    params![MAIN.0, l.0, new.0, e],
                )?;
            }
            for (r, asserted) in rows.base {
                if *asserted {
                    tx.execute(
                        "INSERT INTO base_history(branch, rev_id, asserted_epoch) VALUES (?1, ?2, ?3)",
                        params![MAIN.0, r.0, e],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE base_history SET retracted_epoch = ?1 WHERE rev_id = ?2 AND retracted_epoch IS NULL",
                        params![e, r.0],
                    )?;
                }
            }
            tx.execute(
                "INSERT INTO projection_watermark(id, epoch, log_checksum) VALUES (0, ?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch, log_checksum = excluded.log_checksum",
                params![e, rows.checksum.as_slice()],
            )?;
        }
        tx.commit()?;
        self.watermark = rows.epoch;
        Ok(())
    }
    fn decode(bytes: Vec<u8>) -> Sql<NodeState> {
        NodeState::from_bytes_with(&bytes, DecodeLimits::DURABLE)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
    }
    /// Historical(e): last post-state of `r` at or before `e`.
    pub fn state_at(&self, r: RevisionId, e: Epoch) -> Sql<Option<NodeState>> {
        self.conn
            .query_row(
                "SELECT post FROM diff_log WHERE revision = ?1 AND epoch <= ?2 ORDER BY epoch DESC LIMIT 1",
                params![r.0, e.0 as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(Self::decode)
            .transpose()
    }
    /// Snapshot(e): the logical's head as of `e`.
    pub fn head_at(&self, l: LogicalId, e: Epoch) -> Sql<Option<RevisionId>> {
        self.conn
            .query_row(
                "SELECT rev_id FROM head_history WHERE logical_id = ?1 AND from_epoch <= ?2
                 AND (to_epoch IS NULL OR to_epoch > ?2)",
                params![l.0, e.0 as i64],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map(|r| r.map(RevisionId))
    }
    /// DIFF(a, b) from diff_log through the shared endpoint comparison.
    pub fn diff(&self, a: Epoch, b: Epoch) -> Sql<EpochDiff> {
        let high = a.max(b).0 as i64;
        let mut stmt = self.conn.prepare_cached(
            "SELECT epoch, revision, post FROM diff_log WHERE epoch <= ?1 ORDER BY epoch, revision",
        )?;
        let rows = stmt
            .query_map(params![high], |row| {
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
