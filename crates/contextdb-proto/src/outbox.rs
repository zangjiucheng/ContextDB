//! Client-side outbox (ADR 0010 §5): at-least-once delivery of hook facts,
//! applied idempotently by the daemon per `(stream, seq)`.
//!
//! Per stream (one harness session) under `<data dir>/outbox/`:
//! - `<stream>.log`: self-verifying records `"CXOB" | len u32 LE | blake3 | body`
//!   (body = `Packet`); a scan resyncs at the next magic, so a torn tail
//!   loses only the record being written (§5.1).
//! - `<stream>.seq`: `next_seq`, guarded by the **append lock** (flock on it).
//!   An append recovers `next_seq` from the log too, so a hook killed between
//!   appending and updating `.seq` can never cause a reuse (review N1).
//! - `<stream>.ack`: `acked_through`, guarded by the **delivery lock**.
//! - `<stream>.state`: [`StreamState`], written under the append lock.
//!
//! Lock order is always append → delivery. Appends hold the append lock for
//! a few writes and fsyncs, never across IPC; delivery takes the delivery
//! lock without blocking, so no hook waits behind another's delivery.
use contextdb_model::*;
use nix::fcntl::{Flock, FlockArg};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const MAGIC: [u8; 4] = *b"CXOB";
const HEADER: usize = 4 + 4 + 32;
/// Most bytes kept per stream log while the daemon is unreachable (§5.2).
pub const LOG_CAP: u64 = 16 << 20;
/// Most packets per `Facts` request.
pub const BATCH: usize = 256;

pub type Stream = [u8; 16];

/// Stream id of a harness session (ADR 0010 §5.1, A5).
pub fn stream_id(harness: &str, harness_session: &str) -> Stream {
    let mut h = blake3::Hasher::new_derive_key("contextdb outbox stream v1");
    h.update(harness.as_bytes());
    h.update(&[0]);
    h.update(harness_session.as_bytes());
    h.finalize().as_bytes()[..16].try_into().expect("16 bytes")
}
pub fn hex(s: &Stream) -> String {
    s.iter().map(|b| format!("{b:02x}")).collect()
}
/// G4 fault injection (debug builds only): the process aborts at the named
/// step when `CTX_OUTBOX_CRASH` names it, as a SIGKILL would (no destructors;
/// the page cache survives). Compiled out of release builds.
#[inline]
fn crash_point(name: &str) {
    #[cfg(debug_assertions)]
    if std::env::var("CTX_OUTBOX_CRASH").is_ok_and(|v| v == name) {
        std::process::abort();
    }
    #[cfg(not(debug_assertions))]
    let _ = name;
}
/// One serialised record.
pub fn record(p: &Packet) -> Vec<u8> {
    let body = p.canonical_bytes();
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(blake3::hash(&body).as_bytes());
    out.extend_from_slice(&body);
    out
}
/// Every whole, valid record in file order; damage costs only the damaged
/// records (resync at the next magic).
pub fn scan(data: &[u8]) -> Vec<Packet> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(at) = data
        .get(pos..)
        .and_then(|d| d.windows(4).position(|w| w == MAGIC))
        .map(|p| p + pos)
    {
        let parsed = data.get(at + 4..at + HEADER).and_then(|h| {
            let len = u32::from_le_bytes(h[..4].try_into().expect("4")) as usize;
            let body = data.get(at + HEADER..(at + HEADER).checked_add(len)?)?;
            (blake3::hash(body).as_bytes() == &h[4..])
                .then(|| Packet::from_bytes(body).ok())
                .flatten()
                .map(|p| (p, len))
        });
        match parsed {
            Some((p, len)) => {
                out.push(p);
                pos = at + HEADER + len;
            }
            None => pos = at + 1,
        }
    }
    out
}
/// Per-stream client state (ADR 0010 §5.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamState {
    pub harness: String,
    pub harness_session: String,
    /// The ctxd session the hooks attached, if known.
    pub session: Option<[u8; 16]>,
    /// git HEAD at SessionStart: drift compares against it (§5.4).
    pub head_sha: String,
    /// Cached repository roots per `cwd` (§3.2).
    pub roots: Vec<(String, String)>,
    /// Unacknowledged `context_update` token per agent (§4.4).
    pub tokens: Vec<(Option<String>, u64)>,
    /// When an Empty warning was last rendered, per agent (unix seconds).
    pub empty_warned: Vec<(Option<String>, u64)>,
    /// Paths dirty at SessionStart with their content hashes (§5.4).
    pub baseline: Vec<(String, ContentHash)>,
    /// Facts dropped because the log reached [`LOG_CAP`].
    pub dropped: u64,
}
impl Wire for StreamState {
    fn encode(&self, o: &mut Vec<u8>) {
        self.harness.encode(o);
        self.harness_session.encode(o);
        self.session.encode(o);
        self.head_sha.encode(o);
        self.roots.encode(o);
        self.tokens.encode(o);
        self.empty_warned.encode(o);
        self.baseline.encode(o);
        self.dropped.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            harness: Wire::decode(i)?,
            harness_session: Wire::decode(i)?,
            session: Wire::decode(i)?,
            head_sha: Wire::decode(i)?,
            roots: Wire::decode(i)?,
            tokens: Wire::decode(i)?,
            empty_warned: Wire::decode(i)?,
            baseline: Wire::decode(i)?,
            dropped: Wire::decode(i)?,
        })
    }
}
/// What the daemon said to one `Facts` batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Applied(u64),
    Gap(u64),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Delivered {
    /// Another process is delivering this stream.
    Busy,
    /// Everything up to `acked` is durable at the daemon.
    Done { acked: u64, remaining: usize },
    /// The daemon could not be reached; nothing is lost.
    Failed { acked: u64, error: String },
}
pub struct Outbox {
    dir: PathBuf,
}
fn open_rw(p: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(p)
}
fn read_u64(f: &mut File) -> io::Result<u64> {
    let mut b = Vec::new();
    f.seek(SeekFrom::Start(0))?;
    f.read_to_end(&mut b)?;
    Ok(b.get(..8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8")))
        .unwrap_or(0))
}
fn write_u64(f: &mut File, v: u64) -> io::Result<()> {
    f.seek(SeekFrom::Start(0))?;
    f.write_all(&v.to_le_bytes())?;
    f.set_len(8)?;
    sync(f)
}
/// Plain fsync(2): a killed process does not lose the page cache, and power
/// loss is covered by the daemon's durability (§5.1).
fn sync(f: &File) -> io::Result<()> {
    nix::unistd::fsync(f).map_err(io::Error::from)
}
fn lock(f: File, nonblocking: bool) -> io::Result<Option<Flock<File>>> {
    let arg = if nonblocking {
        FlockArg::LockExclusiveNonblock
    } else {
        FlockArg::LockExclusive
    };
    match Flock::lock(f, arg) {
        Ok(l) => Ok(Some(l)),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) if nonblocking => Ok(None),
        Err((_, e)) => Err(io::Error::from(e)),
    }
}
impl Outbox {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("outbox"),
        }
    }
    fn path(&self, s: &Stream, ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", hex(s)))
    }
    fn ensure(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)
    }
    /// The stream's state (default when missing or unreadable).
    pub fn state(&self, s: &Stream) -> StreamState {
        std::fs::read(self.path(s, "state"))
            .ok()
            .and_then(|b| StreamState::from_bytes(&b).ok())
            .unwrap_or_default()
    }
    /// Updates the state under the append lock.
    pub fn update_state(&self, s: &Stream, f: impl FnOnce(&mut StreamState)) -> io::Result<()> {
        self.ensure()?;
        let _guard = lock(open_rw(&self.path(s, "seq"))?, false)?;
        let mut st = self.state(s);
        f(&mut st);
        let tmp = self.path(s, "state.tmp");
        let mut file = File::create(&tmp)?;
        file.write_all(&st.canonical_bytes())?;
        sync(&file)?;
        std::fs::rename(tmp, self.path(s, "state"))
    }
    /// Appends a fact and returns its seq, or None when the cap dropped it.
    pub fn append(&self, s: &Stream, agent: Option<String>, fact: Fact) -> io::Result<Option<u64>> {
        self.ensure()?;
        let mut guard = lock(open_rw(&self.path(s, "seq"))?, false)?.expect("blocking lock");
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(self.path(s, "log"))?;
        let mut bytes = Vec::new();
        File::open(self.path(s, "log"))?.read_to_end(&mut bytes)?;
        if bytes.len() as u64 >= LOG_CAP {
            drop(log);
            drop(guard);
            self.update_state(s, |st| st.dropped += 1)?;
            return Ok(None);
        }
        let last = scan(&bytes).iter().map(|p| p.seq).max().unwrap_or(0);
        // `.seq`, the log and the acknowledged mark each bound the next seq:
        // a hook killed before writing `.seq` whose record was then
        // delivered and truncated is still covered by `.ack` (G3 property b).
        let acked = match File::open(self.path(s, "ack")) {
            Ok(mut f) => read_u64(&mut f)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let seq = read_u64(&mut guard)?.max(last + 1).max(acked + 1);
        crash_point("append:before-write");
        log.write_all(&record(&Packet { seq, agent, fact }))?;
        crash_point("append:before-sync");
        sync(&log)?;
        crash_point("append:before-seq");
        write_u64(&mut guard, seq + 1)?;
        Ok(Some(seq))
    }
    /// Unacknowledged packets in seq order, and `acked_through`.
    pub fn pending(&self, s: &Stream) -> io::Result<(u64, Vec<Packet>)> {
        let acked = match File::open(self.path(s, "ack")) {
            Ok(mut f) => read_u64(&mut f)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let bytes = match std::fs::read(self.path(s, "log")) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut packets: Vec<Packet> = scan(&bytes).into_iter().filter(|p| p.seq > acked).collect();
        packets.sort_by_key(|p| p.seq);
        packets.dedup_by_key(|p| p.seq);
        Ok((acked, packets))
    }
    /// Streams with undelivered records, oldest log first.
    pub fn dirty(&self) -> Vec<Stream> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut found: Vec<(std::time::SystemTime, Stream)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let hexid = name.strip_suffix(".log")?;
                let meta = e.metadata().ok()?;
                (meta.len() > 0).then_some(())?;
                let mut s = [0u8; 16];
                for (k, b) in s.iter_mut().enumerate() {
                    *b = u8::from_str_radix(hexid.get(2 * k..2 * k + 2)?, 16).ok()?;
                }
                Some((meta.modified().ok()?, s))
            })
            .collect();
        found.sort();
        found.into_iter().map(|(_, s)| s).collect()
    }
    /// Delivers the stream's unacknowledged facts in seq order within
    /// `budget` (§5.2–5.3), resolving gaps by resending or skipping.
    pub fn deliver(
        &self,
        s: &Stream,
        budget: Duration,
        mut send: impl FnMut(&[Packet], Option<(u64, u64)>) -> Result<Reply, String>,
    ) -> io::Result<Delivered> {
        self.ensure()?;
        let start = Instant::now();
        let Some(mut ack) = lock(open_rw(&self.path(s, "ack"))?, true)? else {
            return Ok(Delivered::Busy);
        };
        let mut acked = read_u64(&mut ack)?;
        let (_, all) = self.pending(s)?;
        let mut queue: Vec<Packet> = all.into_iter().filter(|p| p.seq > acked).collect();
        // Seqs missing between the mark and the first record (a corrupt
        // middle record) cannot be resent.
        let mut skip = queue
            .first()
            .filter(|p| p.seq > acked + 1)
            .map(|p| (acked + 1, p.seq - 1));
        let mut gaps = 0;
        while !queue.is_empty() || skip.is_some() {
            if start.elapsed() > budget {
                break;
            }
            let n = queue.len().min(BATCH);
            let reply = send(&queue[..n], skip);
            crash_point("deliver:before-ack");
            match reply {
                Ok(Reply::Applied(through)) => {
                    if through <= acked && skip.is_none() {
                        break; // no progress: leave the rest for later
                    }
                    acked = acked.max(through);
                    write_u64(&mut ack, acked)?;
                    queue.retain(|p| p.seq > acked);
                    skip = None;
                }
                Ok(Reply::Gap(expected)) => {
                    gaps += 1;
                    if gaps > 3 {
                        return Ok(Delivered::Failed {
                            acked,
                            error: format!("unresolved gap at {expected}"),
                        });
                    }
                    // Resend from `expected` if the log still has it; else
                    // skip up to the first record we still hold.
                    let (_, all) = self.pending_from(s, expected)?;
                    match all.first() {
                        Some(p) if p.seq == expected => {
                            queue = all;
                            skip = None;
                        }
                        first => {
                            let to = first.map_or(acked.max(expected), |p| p.seq - 1);
                            skip = Some((expected, to.max(expected)));
                            queue = all;
                        }
                    }
                }
                Err(error) => return Ok(Delivered::Failed { acked, error }),
            }
        }
        let remaining = queue.len();
        drop(ack);
        if remaining == 0 {
            self.truncate_if_acked(s)?;
        }
        Ok(Delivered::Done { acked, remaining })
    }
    /// Valid records with `seq ≥ from`, in order (gap resolution).
    fn pending_from(&self, s: &Stream, from: u64) -> io::Result<(u64, Vec<Packet>)> {
        let bytes = std::fs::read(self.path(s, "log")).unwrap_or_default();
        let mut packets: Vec<Packet> = scan(&bytes).into_iter().filter(|p| p.seq >= from).collect();
        packets.sort_by_key(|p| p.seq);
        packets.dedup_by_key(|p| p.seq);
        Ok((from.saturating_sub(1), packets))
    }
    /// Empties a fully acknowledged log, under both locks in order.
    fn truncate_if_acked(&self, s: &Stream) -> io::Result<()> {
        let Some(mut append) = lock(open_rw(&self.path(s, "seq"))?, false)? else {
            return Ok(());
        };
        let Some(mut ack) = lock(open_rw(&self.path(s, "ack"))?, true)? else {
            return Ok(());
        };
        let acked = read_u64(&mut ack)?;
        let bytes = std::fs::read(self.path(s, "log")).unwrap_or_default();
        let records = scan(&bytes);
        if records.iter().all(|p| p.seq <= acked) {
            // The log is about to forget its seqs: `.seq` must not.
            let last = records.iter().map(|p| p.seq).max().unwrap_or(0);
            let next = read_u64(&mut append)?.max(last + 1).max(acked + 1);
            write_u64(&mut append, next)?;
            let log = open_rw(&self.path(s, "log"))?;
            log.set_len(0)?;
            sync(&log)?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ctx-outbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }
    fn fact(n: u8) -> Fact {
        Fact::Baseline {
            dirty: vec![(format!("f{n}"), [n; 32])],
        }
    }
    #[test]
    fn scan_skips_damage_and_resyncs() {
        let a = record(&Packet {
            seq: 1,
            agent: None,
            fact: fact(1),
        });
        let b = record(&Packet {
            seq: 2,
            agent: Some("c".into()),
            fact: Fact::StreamEnd,
        });
        let mut data = [b"junkCX".to_vec(), a.clone(), b.clone()].concat();
        assert_eq!(scan(&data).len(), 2);
        let n = data.len();
        data[n - 1] ^= 1; // corrupt b's body
        assert_eq!(scan(&data).iter().map(|p| p.seq).collect::<Vec<_>>(), [1]);
        let torn = [a.clone(), b[..b.len() - 3].to_vec()].concat();
        assert_eq!(scan(&torn).len(), 1);
    }
    #[test]
    fn seqs_survive_truncation_and_a_kill_before_the_seq_write() {
        let dir = tmp("seq");
        let ob = Outbox::new(&dir);
        let s = stream_id("claude", "x");
        assert_eq!(ob.append(&s, None, fact(1)).unwrap(), Some(1));
        assert_eq!(ob.append(&s, None, fact(2)).unwrap(), Some(2));
        // A hook killed after appending seq 3 but before writing `.seq`.
        let mut log = OpenOptions::new()
            .append(true)
            .open(ob.path(&s, "log"))
            .unwrap();
        log.write_all(&record(&Packet {
            seq: 3,
            agent: None,
            fact: fact(3),
        }))
        .unwrap();
        assert_eq!(
            ob.append(&s, None, fact(4)).unwrap(),
            Some(4),
            "never reuses 3"
        );
        // Deliver all, which truncates; seqs continue after truncation.
        let mut seen = Vec::new();
        let d = ob
            .deliver(&s, Duration::from_secs(5), |p, _| {
                seen.extend(p.iter().map(|p| p.seq));
                Ok(Reply::Applied(p.last().map_or(0, |p| p.seq)))
            })
            .unwrap();
        assert_eq!(
            d,
            Delivered::Done {
                acked: 4,
                remaining: 0
            }
        );
        assert_eq!(seen, [1, 2, 3, 4]);
        assert_eq!(std::fs::metadata(ob.path(&s, "log")).unwrap().len(), 0);
        assert_eq!(ob.append(&s, None, fact(5)).unwrap(), Some(5));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn gaps_resend_or_skip_and_failures_lose_nothing() {
        let dir = tmp("gap");
        let ob = Outbox::new(&dir);
        let s = stream_id("codex", "y");
        for n in 1..=3 {
            ob.append(&s, None, fact(n)).unwrap();
        }
        // The daemon is down: nothing is acknowledged or lost.
        let d = ob
            .deliver(&s, Duration::from_secs(5), |_, _| Err("down".into()))
            .unwrap();
        assert_eq!(
            d,
            Delivered::Failed {
                acked: 0,
                error: "down".into()
            }
        );
        assert_eq!(ob.pending(&s).unwrap().1.len(), 3);
        // The daemon lost its mark (e.g. doctor truncation) and wants 1 again
        // after we acknowledged 2 and truncated: it is skipped, then 3 sent.
        let d = ob
            .deliver(&s, Duration::from_secs(5), |_, _| Ok(Reply::Applied(2)))
            .unwrap();
        assert_eq!(
            d,
            Delivered::Done {
                acked: 2,
                remaining: 1
            }
        );
        let log = open_rw(&ob.path(&s, "log")).unwrap();
        let rest: Vec<u8> = scan(&std::fs::read(ob.path(&s, "log")).unwrap())
            .into_iter()
            .filter(|p| p.seq == 3)
            .flat_map(|p| record(&p))
            .collect();
        log.set_len(0).unwrap();
        drop(log);
        std::fs::write(ob.path(&s, "log"), rest).unwrap();
        let mut calls = Vec::new();
        let d = ob
            .deliver(&s, Duration::from_secs(5), |p, skip| {
                calls.push((p.iter().map(|p| p.seq).collect::<Vec<_>>(), skip));
                Ok(match (p.first().map(|p| p.seq), skip) {
                    (Some(3), None) if calls.len() == 1 => Reply::Gap(1),
                    (_, Some((1, 2))) => Reply::Applied(3),
                    _ => Reply::Gap(1),
                })
            })
            .unwrap();
        assert_eq!(
            d,
            Delivered::Done {
                acked: 3,
                remaining: 0
            }
        );
        assert_eq!(calls[1].1, Some((1, 2)));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn state_round_trips_and_dirty_lists_streams() {
        let dir = tmp("state");
        let ob = Outbox::new(&dir);
        let s = stream_id("claude", "z");
        ob.update_state(&s, |st| {
            st.harness = "claude".into();
            st.harness_session = "z".into();
            st.tokens.push((None, 7));
        })
        .unwrap();
        assert_eq!(ob.state(&s).tokens, [(None, 7)]);
        assert!(ob.dirty().is_empty());
        ob.append(&s, None, fact(1)).unwrap();
        assert_eq!(ob.dirty(), [s]);
        assert_ne!(stream_id("claude", "z"), stream_id("codex", "z"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
