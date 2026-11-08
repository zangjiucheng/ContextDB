//! Purging shredded payloads from the body-bearing projections (ADR 0017
//! §8.4): `recall.sqlite`, `vectors.sqlite`, `jobs.sqlite` and
//! `candidates.sqlite`.
//!
//! - Each opens with `PRAGMA secure_delete = ON`, so freed pages are
//!   overwritten with zeros.
//! - Each records the last shred epoch it purged (`purged_through`) in a
//!   one-row table `ctx_purge`; on open the daemon purges every shred above
//!   it again (derived from the op_log, idempotent).
//! - After a purge, [`compact`] checkpoints the WAL with `TRUNCATE`, so no
//!   frame still holding the old rows survives in the `-wal` file.
use contextdb_model::Epoch;
use rusqlite::{Connection, OptionalExtension, params};

/// Run first on every body-bearing projection connection.
pub const PRAGMAS: &str = "PRAGMA secure_delete = ON;";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ctx_purge(id INTEGER PRIMARY KEY CHECK (id = 0), through INTEGER NOT NULL);";

/// Body-bearing projections, in the data directory (ADR 0017 §8.5).
pub const FILES: [&str; 4] = [
    "recall.sqlite",
    "vectors.sqlite",
    "jobs.sqlite",
    "candidates.sqlite",
];
/// The format a projection file was created or rebuilt at (ADR 0017 §10
/// step 4): a file without `rebuilt_at_format = 5` may hold pre-P9
/// plaintext in its free pages.
pub const FORMAT: i64 = 5;
const FORMAT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ctx_format(id INTEGER PRIMARY KEY CHECK (id = 0), rebuilt_at_format INTEGER NOT NULL);";

/// Whether opening `path` creates the file: a projection created by this
/// binary is stamped (see [`stamp`]) as its first write.
pub fn is_fresh(path: Option<&std::path::Path>) -> bool {
    path.is_none_or(|p| std::fs::metadata(p).map_or(true, |m| m.len() == 0))
}
/// Records `rebuilt_at_format = 5`.
pub fn stamp(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(FORMAT_SCHEMA)?;
    conn.execute(
        "INSERT INTO ctx_format(id, rebuilt_at_format) VALUES (0, ?1)
         ON CONFLICT(id) DO UPDATE SET rebuilt_at_format = excluded.rebuilt_at_format",
        params![FORMAT],
    )?;
    Ok(())
}
/// Whether the projection at `path` needs no rebuild: absent, empty, or
/// stamped `rebuilt_at_format = 5`.
pub fn stamped(path: &std::path::Path) -> rusqlite::Result<bool> {
    if is_fresh(Some(path)) {
        return Ok(true);
    }
    // Not read-only: a WAL database opened read-only needs its `-shm`.
    let conn = Connection::open(path)?;
    let has: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'ctx_format'",
        [],
        |r| r.get(0),
    )?;
    if has == 0 {
        return Ok(false);
    }
    let v: Option<i64> = conn
        .query_row(
            "SELECT rebuilt_at_format FROM ctx_format WHERE id = 0",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v == Some(FORMAT))
}
/// Sets `secure_delete` and creates the `purged_through` table.
pub fn prepare(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(PRAGMAS)?;
    conn.execute_batch(SCHEMA)
}
/// The last shred epoch this projection purged (`Epoch(0)`: none).
pub fn through(conn: &Connection) -> rusqlite::Result<Epoch> {
    Ok(Epoch(
        conn.query_row("SELECT through FROM ctx_purge WHERE id = 0", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .unwrap_or(0) as u64,
    ))
}
/// Records `epoch` as purged.
pub fn set_through(conn: &Connection, epoch: Epoch) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO ctx_purge(id, through) VALUES (0, ?1)
         ON CONFLICT(id) DO UPDATE SET through = excluded.through",
        params![epoch.0 as i64],
    )?;
    Ok(())
}
/// Checkpoints the WAL into the database file and truncates it (and syncs
/// the file under `synchronous = NORMAL`): old frames are gone from disk.
pub fn compact(conn: &Connection) -> rusqlite::Result<()> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn through_round_trips() {
        let c = Connection::open_in_memory().unwrap();
        prepare(&c).unwrap();
        assert_eq!(through(&c).unwrap(), Epoch(0));
        set_through(&c, Epoch(7)).unwrap();
        set_through(&c, Epoch(9)).unwrap();
        assert_eq!(through(&c).unwrap(), Epoch(9));
        let on: i64 = c
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }
}
