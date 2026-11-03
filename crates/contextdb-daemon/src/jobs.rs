//! The durable job table `<data>/jobs.sqlite` (ADR 0015 §4.4, SPEC §7
//! `jobs`): one row per key `(branch, rule, curvec)`.
//!
//! - **Claim (T53)** is one `UPDATE … WHERE state = Queued` transaction: of
//!   several claimers exactly one matches, and the row is durable (WAL,
//!   `synchronous = FULL`) before the validator is called.
//! - **The token** fences successive holders of a row; the kernel CAS and
//!   store idempotency (`client_txn_id`) fence the commit itself.
//! - **The generation** changes only on a manual retry, so every attempt of
//!   one generation commits under the same `client_txn_id`.
//!
//! Times are milliseconds since the Unix epoch, passed in by the caller.
//! The table is operational state, never derived state.
use contextdb_model::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

pub const FILE: &str = "jobs.sqlite";
pub type JobKey = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JobState {
    Queued = 0,
    Running = 1,
    Done = 2,
    Failed = 3,
    Superseded = 4,
}
impl JobState {
    pub const ALL: [JobState; 5] = [
        Self::Queued,
        Self::Running,
        Self::Done,
        Self::Failed,
        Self::Superseded,
    ];
    fn from_i64(n: i64) -> Self {
        match n {
            0 => Self::Queued,
            1 => Self::Running,
            2 => Self::Done,
            3 => Self::Failed,
            _ => Self::Superseded,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub key: JobKey,
    pub branch: BranchId,
    pub rule: RuleId,
    pub curvec: HeadVector,
    pub generation: u32,
    pub token: u64,
    pub state: JobState,
    /// Running: the lease's end; Queued: not claimable before this time.
    pub lease_until: i64,
    pub attempts: u32,
    pub last_error: Option<String>,
    /// The outcome of a trusted `BoundaryChange`/`NewRule`, which writes no
    /// record: the `ValidationMeta` blob bytes.
    pub outcome_meta: Option<Vec<u8>>,
}
impl Job {
    /// `H("job", claim_key, generation)`: one commit per job generation.
    pub fn client_txn_id(&self) -> [u8; 16] {
        txn_id(&self.key, self.generation)
    }
}
/// `claim_key = H(branch, rule, curvec)`.
pub fn claim_key(branch: BranchId, rule: RuleId, curvec: &HeadVector) -> JobKey {
    let mut h = blake3::Hasher::new();
    h.update(b"ctx-job-claim\0");
    h.update(&branch.0.to_le_bytes());
    h.update(&rule.0.to_le_bytes());
    h.update(&encode_vector(curvec));
    *h.finalize().as_bytes()
}
pub fn txn_id(key: &JobKey, generation: u32) -> [u8; 16] {
    let mut h = blake3::Hasher::new();
    h.update(b"job\0");
    h.update(key);
    h.update(&generation.to_le_bytes());
    let mut out = [0; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    out
}
fn encode_vector(v: &HeadVector) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 8);
    for (l, r) in v {
        out.extend(l.0.to_le_bytes());
        out.extend(r.0.to_le_bytes());
    }
    out
}
fn decode_vector(b: &[u8]) -> HeadVector {
    b.chunks_exact(8)
        .map(|c| {
            let n = |x: &[u8]| u32::from_le_bytes(x.try_into().expect("4 bytes"));
            (LogicalId(n(&c[..4])), RevisionId(n(&c[4..])))
        })
        .collect()
}
/// What discovery did with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovered {
    New,
    /// A `Superseded` row whose key is Pending again (ABA head moves).
    Requeued,
    Exists,
}
pub struct JobTable {
    conn: Mutex<Connection>,
}
fn sql(e: rusqlite::Error) -> String {
    format!("jobs.sqlite: {e}")
}
const COLUMNS: &str = "claim_key, branch, rule, curvec, generation, token, state, lease_until, \
                       attempts, last_error, outcome_meta";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let key: Vec<u8> = r.get(0)?;
    let curvec: Vec<u8> = r.get(3)?;
    Ok(Job {
        key: key.try_into().unwrap_or([0; 32]),
        branch: BranchId(r.get(1)?),
        rule: RuleId(r.get(2)?),
        curvec: decode_vector(&curvec),
        generation: r.get(4)?,
        token: r.get::<_, i64>(5)? as u64,
        state: JobState::from_i64(r.get(6)?),
        lease_until: r.get(7)?,
        attempts: r.get(8)?,
        last_error: r.get(9)?,
        outcome_meta: r.get(10)?,
    })
}
impl JobTable {
    /// Opens (creating) the table. Call [`JobTable::recover`] once at
    /// daemon start, while the data directory's lock is held.
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(sql)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS jobs(
                 claim_key BLOB PRIMARY KEY,
                 branch INTEGER NOT NULL,
                 rule INTEGER NOT NULL,
                 curvec BLOB NOT NULL,
                 generation INTEGER NOT NULL DEFAULT 0,
                 token INTEGER NOT NULL DEFAULT 0,
                 state INTEGER NOT NULL DEFAULT 0,
                 lease_until INTEGER NOT NULL DEFAULT 0,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT,
                 outcome_meta BLOB);
             CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state, lease_until);",
        )
        .map_err(sql)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("jobs")
    }
    /// Start-up recovery (§4.4 M6): the daemon holds the data directory's
    /// flock, so every `Running` row belongs to a dead daemon and goes back
    /// to `Queued` (token bumped). Returns how many.
    pub fn recover(&self) -> Result<usize, String> {
        self.conn()
            .execute(
                "UPDATE jobs SET state = 0, token = token + 1, lease_until = 0 WHERE state = 1",
                [],
            )
            .map_err(sql)
    }
    /// Discovery of a Pending key: a new row is `Queued`; a `Superseded`
    /// row is requeued; `Done`, `Failed`, `Queued` and `Running` rows are
    /// left alone (so a `BoundaryChange` outcome does not loop).
    pub fn discover(
        &self,
        branch: BranchId,
        rule: RuleId,
        curvec: &HeadVector,
    ) -> Result<Discovered, String> {
        let key = claim_key(branch, rule, curvec);
        let conn = self.conn();
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO jobs(claim_key, branch, rule, curvec) VALUES (?1, ?2, ?3, ?4)",
                params![&key[..], branch.0, rule.0, encode_vector(curvec)],
            )
            .map_err(sql)?;
        if inserted == 1 {
            return Ok(Discovered::New);
        }
        let requeued = conn
            .execute(
                "UPDATE jobs SET state = 0, token = token + 1, attempts = 0, lease_until = 0,
                     last_error = NULL WHERE claim_key = ?1 AND state = 4",
                params![&key[..]],
            )
            .map_err(sql)?;
        Ok(if requeued == 1 {
            Discovered::Requeued
        } else {
            Discovered::Exists
        })
    }
    /// Claims the oldest claimable `Queued` row that `pick` accepts (T53):
    /// marker-first, one transaction per row. Returns the claimed job with
    /// its new token and lease.
    pub fn claim(
        &self,
        now: i64,
        lease_ms: i64,
        mut pick: impl FnMut(&Job) -> bool,
    ) -> Result<Option<Job>, String> {
        let conn = self.conn();
        let candidates: Vec<Job> = {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {COLUMNS} FROM jobs WHERE state = 0 AND lease_until <= ?1
                     ORDER BY rowid LIMIT 256"
                ))
                .map_err(sql)?;
            stmt.query_map(params![now], row)
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?
        };
        for mut job in candidates {
            if !pick(&job) {
                continue;
            }
            let until = now + lease_ms;
            let n = conn
                .execute(
                    "UPDATE jobs SET state = 1, token = token + 1, lease_until = ?2
                     WHERE claim_key = ?1 AND state = 0",
                    params![&job.key[..], until],
                )
                .map_err(sql)?;
            if n == 1 {
                job.state = JobState::Running;
                job.token += 1;
                job.lease_until = until;
                return Ok(Some(job));
            }
        }
        Ok(None)
    }
    /// Ends a claim if `token` still holds the row. Returns whether it did.
    pub fn finish(
        &self,
        key: &JobKey,
        token: u64,
        state: JobState,
        error: Option<&str>,
        outcome_meta: Option<&[u8]>,
    ) -> Result<bool, String> {
        let n = self
            .conn()
            .execute(
                "UPDATE jobs SET state = ?3, lease_until = 0, last_error = ?4, outcome_meta = ?5
                 WHERE claim_key = ?1 AND token = ?2 AND state = 1",
                params![&key[..], token as i64, state as i64, error, outcome_meta],
            )
            .map_err(sql)?;
        Ok(n == 1)
    }
    /// Gives a claim back without counting an attempt (the LLM call cap):
    /// `Queued`, not claimable before `not_before`.
    pub fn release(&self, key: &JobKey, token: u64, not_before: i64) -> Result<bool, String> {
        let n = self
            .conn()
            .execute(
                "UPDATE jobs SET state = 0, lease_until = ?3
                 WHERE claim_key = ?1 AND token = ?2 AND state = 1",
                params![&key[..], token as i64, not_before],
            )
            .map_err(sql)?;
        Ok(n == 1)
    }
    /// A failed attempt: `Failed` after `max_attempts`, otherwise `Queued`
    /// again, not before `retry_at`. `None` if the token no longer holds.
    pub fn fail(
        &self,
        key: &JobKey,
        token: u64,
        error: &str,
        max_attempts: u32,
        retry_at: i64,
    ) -> Result<Option<JobState>, String> {
        let conn = self.conn();
        let n = conn
            .execute(
                "UPDATE jobs SET attempts = attempts + 1, last_error = ?3,
                     state = CASE WHEN attempts + 1 >= ?4 THEN 3 ELSE 0 END,
                     lease_until = CASE WHEN attempts + 1 >= ?4 THEN 0 ELSE ?5 END
                 WHERE claim_key = ?1 AND token = ?2 AND state = 1",
                params![&key[..], token as i64, error, max_attempts, retry_at],
            )
            .map_err(sql)?;
        if n == 0 {
            return Ok(None);
        }
        let state: i64 = conn
            .query_row(
                "SELECT state FROM jobs WHERE claim_key = ?1",
                params![&key[..]],
                |r| r.get(0),
            )
            .map_err(sql)?;
        Ok(Some(JobState::from_i64(state)))
    }
    /// Expiry: `Running` rows whose lease passed go back to `Queued` with
    /// the token bumped and an attempt counted (`Failed` after
    /// `max_attempts`). A late result can only commit through the same
    /// `client_txn_id`. Returns the expired keys.
    pub fn expire(&self, now: i64, max_attempts: u32) -> Result<Vec<JobKey>, String> {
        let conn = self.conn();
        let keys: Vec<JobKey> = {
            let mut stmt = conn
                .prepare_cached("SELECT claim_key FROM jobs WHERE state = 1 AND lease_until < ?1")
                .map_err(sql)?;
            stmt.query_map(params![now], |r| r.get::<_, Vec<u8>>(0))
                .map_err(sql)?
                .filter_map(|k| k.ok()?.try_into().ok())
                .collect()
        };
        for key in &keys {
            conn.execute(
                "UPDATE jobs SET token = token + 1, attempts = attempts + 1,
                     last_error = 'lease expired',
                     state = CASE WHEN attempts + 1 >= ?2 THEN 3 ELSE 0 END, lease_until = 0
                 WHERE claim_key = ?1 AND state = 1",
                params![&key[..], max_attempts],
            )
            .map_err(sql)?;
        }
        Ok(keys)
    }
    /// `ctx validators retry <key>`: requeues with the generation bumped,
    /// so the commit is not answered by an earlier attempt's result.
    /// A `Running` row is left to its holder.
    pub fn retry(&self, key: &JobKey) -> Result<bool, String> {
        let n = self
            .conn()
            .execute(
                "UPDATE jobs SET state = 0, generation = generation + 1, token = token + 1,
                     attempts = 0, lease_until = 0, last_error = NULL
                 WHERE claim_key = ?1 AND state != 1",
                params![&key[..]],
            )
            .map_err(sql)?;
        Ok(n == 1)
    }
    /// `ctx validators retry --failed`.
    pub fn retry_failed(&self) -> Result<usize, String> {
        self.conn()
            .execute(
                "UPDATE jobs SET state = 0, generation = generation + 1, token = token + 1,
                     attempts = 0, lease_until = 0, last_error = NULL WHERE state = 3",
                [],
            )
            .map_err(sql)
    }
    pub fn get(&self, key: &JobKey) -> Result<Option<Job>, String> {
        self.conn()
            .query_row(
                &format!("SELECT {COLUMNS} FROM jobs WHERE claim_key = ?1"),
                params![&key[..]],
                row,
            )
            .optional()
            .map_err(sql)
    }
    /// Row counts per state, in [`JobState::ALL`] order.
    pub fn counts(&self) -> Result<[u64; 5], String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached("SELECT state, count(*) FROM jobs GROUP BY state")
            .map_err(sql)?;
        let mut out = [0; 5];
        for r in stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .map_err(sql)?
        {
            let (s, n) = r.map_err(sql)?;
            out[JobState::from_i64(s) as usize] += n as u64;
        }
        Ok(out)
    }
    /// Rows in `state`, oldest first, at most `limit`.
    pub fn list(&self, state: JobState, limit: usize) -> Result<Vec<Job>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM jobs WHERE state = ?1 ORDER BY rowid LIMIT ?2"
            ))
            .map_err(sql)?;
        stmt.query_map(params![state as i64, limit as i64], row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }
    /// Every row (tests and the quiescence property).
    pub fn all(&self) -> Result<Vec<Job>, String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!("SELECT {COLUMNS} FROM jobs ORDER BY rowid"))
            .map_err(sql)?;
        stmt.query_map([], row)
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
    }
}
