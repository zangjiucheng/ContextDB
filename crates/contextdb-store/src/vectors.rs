//! The vector projection (ADR 0016 §2.1, §7.2): `(content_hash, model) →
//! f32[dims]`, keyed by content, not by logical, so branches and reverted
//! revisions share a vector. Rebuildable and never a source of truth: a
//! deleted file costs one embedding per live head.
//!
//! Vectors are stored unit-length, little-endian `f32`.
use contextdb_model::{ContentHash, Epoch};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub const FILE: &str = "vectors.sqlite";
type Sql<T> = rusqlite::Result<T>;

pub struct VectorStore {
    conn: Connection,
}
fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}
fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().expect("4")))
        .collect()
}
impl VectorStore {
    /// Opens (or creates) the projection; memory when `path` is `None`.
    pub fn open(path: Option<&Path>) -> Sql<Self> {
        let fresh = crate::purge::is_fresh(path);
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        if fresh {
            crate::purge::stamp(&conn)?;
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS vectors(
                 content BLOB NOT NULL,
                 model TEXT NOT NULL,
                 dims INTEGER NOT NULL,
                 vector BLOB NOT NULL,
                 PRIMARY KEY (model, content)) WITHOUT ROWID;",
        )?;
        crate::purge::prepare(&conn)?;
        Ok(Self { conn })
    }
    /// G4 fault injection: as `RecallIndex::limit_pages`.
    #[doc(hidden)]
    pub fn limit_pages(&self, pages: Option<u64>) -> Sql<()> {
        let n = pages.unwrap_or(4_294_967_294);
        self.conn
            .query_row(&format!("PRAGMA max_page_count = {n}"), [], |_| Ok(()))
    }
    #[doc(hidden)]
    pub fn page_count(&self) -> Sql<u64> {
        self.conn
            .query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
    }
    /// Whether `content` has a vector of `dims` for `model`.
    pub fn has(&self, model: &str, dims: usize, content: &ContentHash) -> Sql<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT 1 FROM vectors WHERE model = ?1 AND content = ?2 AND dims = ?3",
            )?
            .query_row(params![model, &content[..], dims as i64], |_| Ok(()))
            .optional()?
            .is_some())
    }
    pub fn get(&self, model: &str, dims: usize, content: &ContentHash) -> Sql<Option<Vec<f32>>> {
        self.conn
            .prepare_cached(
                "SELECT vector FROM vectors WHERE model = ?1 AND content = ?2 AND dims = ?3",
            )?
            .query_row(params![model, &content[..], dims as i64], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .optional()
            .map(|b| b.map(|b| from_blob(&b)))
    }
    /// Stores vectors in one transaction (a vector already there is
    /// replaced: same content and model give the same vector).
    pub fn put(&mut self, model: &str, rows: &[(ContentHash, Vec<f32>)]) -> Sql<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO vectors(content, model, dims, vector) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (content, v) in rows {
                stmt.execute(params![&content[..], model, v.len() as i64, to_blob(v)])?;
            }
        }
        tx.commit()
    }
    /// Removes the vectors of every other model or dimension (`ctx reindex
    /// --vectors` after a model change).
    pub fn prune_except(&self, model: &str, dims: usize) -> Sql<usize> {
        self.conn.execute(
            "DELETE FROM vectors WHERE model != ?1 OR dims != ?2",
            params![model, dims as i64],
        )
    }
    /// The last shred epoch purged from this projection (ADR 0017 §8.4).
    pub fn purged_through(&self) -> Sql<Epoch> {
        crate::purge::through(&self.conn)
    }
    /// Deletes every model's vector of each shredded content, records
    /// `through` and truncates the WAL.
    pub fn purge(&mut self, contents: &[ContentHash], through: Epoch) -> Sql<()> {
        let tx = self.conn.transaction()?;
        for c in contents {
            tx.execute("DELETE FROM vectors WHERE content = ?1", params![&c[..]])?;
        }
        crate::purge::set_through(&tx, through)?;
        tx.commit()?;
        crate::purge::compact(&self.conn)
    }
    pub fn count(&self, model: &str) -> Sql<u64> {
        self.conn
            .query_row(
                "SELECT count(*) FROM vectors WHERE model = ?1",
                params![model],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_round_trip_by_model_and_dims() {
        let mut s = VectorStore::open(None).unwrap();
        let (a, b) = ([1u8; 32], [2u8; 32]);
        s.put("m", &[(a, vec![0.6, 0.8]), (b, vec![1.0, 0.0])])
            .unwrap();
        s.put("other", &[(a, vec![0.0, 1.0, 0.0])]).unwrap();
        assert_eq!(s.get("m", 2, &a).unwrap(), Some(vec![0.6, 0.8]));
        assert!(s.has("m", 2, &b).unwrap());
        assert!(!s.has("m", 3, &b).unwrap(), "another dimension");
        assert_eq!(s.get("other", 3, &a).unwrap(), Some(vec![0.0, 1.0, 0.0]));
        assert_eq!(s.prune_except("m", 2).unwrap(), 1);
        assert_eq!(s.count("m").unwrap(), 2);
        assert_eq!(s.count("other").unwrap(), 0);
    }
}
