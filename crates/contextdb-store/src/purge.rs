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
