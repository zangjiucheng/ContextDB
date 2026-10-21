//! P4 G3 (ADR 0010 §11): outbox properties at 10,000 seeds (override with
//! CONTEXTDB_P4_OUTBOX_SEEDS).
//! (a) A scan after truncation, garbage and corruption returns every intact
//!     record, in order, and nothing that was not a whole record.
//! (b) Any interleaving of appends (including hooks killed before the `.seq`
//!     write and torn appends), deliveries that fail, truncations and a
//!     daemon that loses its marks yields strictly increasing, never reused
//!     seqs; every fully appended fact is applied exactly once or reported
//!     skipped, against a model daemon with `ctxd`'s stream-mark rules.
use contextdb_model::*;
use contextdb_proto::outbox::{self, Delivered, Outbox, Reply, record, scan};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};
use std::{collections::BTreeMap, time::Duration};

fn seeds() -> u64 {
    std::env::var("CONTEXTDB_P4_OUTBOX_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}
fn fact(rng: &mut ChaCha8Rng) -> Fact {
    match rng.next_u32() % 3 {
        0 => Fact::StreamEnd,
        1 => Fact::Drift {
            done: Some(vec![format!("f{}", rng.next_u32() % 100)]),
        },
        _ => Fact::Baseline {
            dirty: vec![("x".into(), [rng.next_u32() as u8; 32])],
        },
    }
}
#[test]
fn a_scan_returns_exactly_the_intact_records() {
    for seed in 0..seeds() {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xA0);
        let n = 1 + rng.next_u32() as usize % 8;
        let packets: Vec<Packet> = (0..n)
            .map(|k| Packet {
                seq: k as u64 + 1,
                agent: (rng.next_u32() % 2 == 0).then(|| "a".into()),
                fact: fact(&mut rng),
            })
            .collect();
        let mut bytes = Vec::new();
        let mut intact = Vec::new();
        for p in &packets {
            if rng.next_u32() % 4 == 0 {
                // Garbage between records, sometimes a false magic.
                let len = rng.next_u32() as usize % 12;
                bytes.extend((0..len).map(|_| rng.next_u32() as u8));
                if rng.next_u32() % 2 == 0 {
                    bytes.extend_from_slice(b"CXOB\x05\x00\x00\x00");
                }
            }
            let mut r = record(p);
            if rng.next_u32() % 5 == 0 {
                let i = rng.next_u32() as usize % r.len();
                r[i] ^= 1 + (rng.next_u32() % 255) as u8; // damaged
            } else {
                intact.push(p.clone());
            }
            bytes.extend(r);
        }
        if rng.next_u32() % 3 == 0 {
            // Torn tail: a prefix of one more record.
            let extra = record(&Packet {
                seq: 99,
                agent: None,
                fact: Fact::StreamEnd,
            });
            let k = rng.next_u32() as usize % extra.len();
            bytes.extend_from_slice(&extra[..k]);
        }
        let got = scan(&bytes);
        // Every intact record, in order; nothing else.
        let mut it = got.iter();
        for want in &intact {
            assert!(it.any(|g| g == want), "seed {seed}: lost {want:?}");
        }
        for g in &got {
            assert!(packets.contains(g), "seed {seed}: invented {g:?}");
        }
    }
}
/// `ctxd`'s stream-mark rules (ADR 0010 §5.3) over an in-memory store.
#[derive(Default)]
struct Daemon {
    through: u64,
    applied: BTreeMap<u64, Packet>,
    skipped: Vec<(u64, u64)>,
}
impl Daemon {
    fn facts(&mut self, packets: &[Packet], skip: Option<(u64, u64)>) -> Reply {
        if let Some((from, to)) = skip {
            if from > self.through + 1 {
                return Reply::Gap(self.through + 1);
            }
            if to > self.through {
                self.skipped.push((self.through + 1, to));
                self.through = to;
            }
        }
        let mut any = false;
        for p in packets {
            if p.seq <= self.through {
                continue;
            }
            if p.seq != self.through + 1 {
                if !any && skip.is_none() {
                    return Reply::Gap(self.through + 1);
                }
                break;
            }
            assert!(
                self.applied.insert(p.seq, p.clone()).is_none(),
                "applied twice: {}",
                p.seq
            );
            self.through = p.seq;
            any = true;
        }
        Reply::Applied(self.through)
    }
    /// e.g. `ctx doctor --truncate-before`: marks and effects above `to` are gone.
    fn lose_marks(&mut self, to: u64) {
        self.through = self.through.min(to);
        self.applied.retain(|s, _| *s <= to);
        self.skipped.retain(|(f, _)| *f <= to);
    }
}
#[test]
fn b_seqs_are_never_reused_and_every_fact_is_applied_once_or_skipped() {
    let base = std::env::temp_dir().join(format!("ctx-outbox-props-{}", std::process::id()));
    for seed in 0..seeds() {
        let mut rng = ChaCha8Rng::seed_from_u64(seed ^ 0xB0);
        let dir = base.join(seed.to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let ob = Outbox::new(&dir);
        let s = outbox::stream_id("p", &seed.to_string());
        let log = dir.join("outbox").join(format!("{}.log", outbox::hex(&s)));
        let seq_file = log.with_extension("seq");
        let mut d = Daemon::default();
        let mut appended: Vec<u64> = Vec::new();
        for _ in 0..(2 + rng.next_u32() % 10) {
            match rng.next_u32() % 10 {
                0..=3 => {
                    let seq = ob.append(&s, None, fact(&mut rng)).unwrap().unwrap();
                    appended.push(seq);
                }
                4 => {
                    // Killed after the append, before `.seq` was written.
                    let before = std::fs::read(&seq_file).ok();
                    let seq = ob.append(&s, None, fact(&mut rng)).unwrap().unwrap();
                    appended.push(seq);
                    match before {
                        Some(b) => std::fs::write(&seq_file, b).unwrap(),
                        None => std::fs::remove_file(&seq_file).unwrap(),
                    }
                }
                5 => {
                    // Torn append: never fully written, never reported done.
                    use std::io::Write;
                    let r = record(&Packet {
                        seq: u64::MAX,
                        agent: None,
                        fact: Fact::StreamEnd,
                    });
                    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log)
                        .unwrap();
                    f.write_all(&r[..r.len() / 2]).unwrap();
                }
                6..=8 => {
                    let fail = rng.next_u32() % 3 == 0;
                    let _ = ob
                        .deliver(&s, Duration::from_secs(5), |p, skip| {
                            if fail {
                                Err("down".into())
                            } else {
                                Ok(d.facts(p, skip))
                            }
                        })
                        .unwrap();
                }
                _ => {
                    let to = d.through.saturating_sub(u64::from(rng.next_u32() % 3));
                    d.lose_marks(to);
                }
            }
        }
        // Seqs strictly increase and are never reused.
        assert!(
            appended.windows(2).all(|w| w[0] < w[1]),
            "seed {seed}: {appended:?}"
        );
        // A later hook of the stream appends and delivers with the daemon up.
        // That is when lost marks surface (a Gap resolved by skip): a loss the
        // stream never delivers after stays unreported (ADR 0010 §5.3).
        appended.push(ob.append(&s, None, Fact::StreamEnd).unwrap().unwrap());
        let r = ob
            .deliver(&s, Duration::from_secs(5), |p, skip| Ok(d.facts(p, skip)))
            .unwrap();
        assert!(
            matches!(r, Delivered::Done { remaining: 0, .. }),
            "seed {seed}: {r:?}"
        );
        for seq in &appended {
            let skipped = d.skipped.iter().any(|(f, t)| (f..=t).contains(&seq));
            assert!(
                d.applied.contains_key(seq) || skipped,
                "seed {seed}: fact {seq} neither applied nor skipped"
            );
        }
        // Nothing beyond what was appended was applied (the torn record never).
        assert!(
            d.applied.keys().all(|k| appended.contains(k)),
            "seed {seed}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// G4 (ADR 0010 §11): a hook process dies (abort, as SIGKILL) at every
/// outbox step. A fact whose record reached the file is applied exactly
/// once after recovery; one that never did is not; seqs are never reused.
/// The child is this test binary re-run with `CTX_OUTBOX_CHILD` set.
#[test]
fn g4_hook_killed_at_every_outbox_step() {
    if let Ok(dir) = std::env::var("CTX_OUTBOX_CHILD") {
        let ob = Outbox::new(std::path::Path::new(&dir));
        let s = outbox::stream_id("kill", "s");
        let _ = ob.append(&s, None, Fact::StreamEnd);
        let _ = ob.deliver(&s, Duration::from_secs(5), |_, _| Ok(Reply::Applied(2)));
        return;
    }
    let base = std::env::temp_dir().join(format!("ctx-outbox-kill-{}", std::process::id()));
    for (point, applied_after_kill) in [
        ("append:before-write", false),
        ("append:before-sync", true),
        ("append:before-seq", true),
        ("deliver:before-ack", true),
    ] {
        let dir = base.join(point.replace(':', "-"));
        let _ = std::fs::remove_dir_all(&dir);
        let ob = Outbox::new(&dir);
        let s = outbox::stream_id("kill", "s");
        // One fact already there (seq 1).
        assert_eq!(ob.append(&s, None, Fact::StreamEnd).unwrap(), Some(1));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "g4_hook_killed_at_every_outbox_step",
                "--nocapture",
            ])
            .env("CTX_OUTBOX_CHILD", &dir)
            .env("CTX_OUTBOX_CRASH", point)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "{point}: the child must die");
        // A later hook appends again and delivers to a daemon that, for the
        // deliver case, already applied what the dead hook sent.
        let next = ob.append(&s, None, Fact::StreamEnd).unwrap().unwrap();
        let mut d = Daemon::default();
        if point == "deliver:before-ack" {
            let (_, sent) = ob.pending(&s).unwrap();
            d.facts(&sent[..2], None);
        }
        let r = ob
            .deliver(&s, Duration::from_secs(5), |p, skip| Ok(d.facts(p, skip)))
            .unwrap();
        assert!(
            matches!(r, Delivered::Done { remaining: 0, .. }),
            "{point}: {r:?}"
        );
        let expect: Vec<u64> = if applied_after_kill {
            vec![1, 2, 3]
        } else {
            vec![1, 2]
        };
        assert_eq!(next, *expect.last().unwrap(), "{point}: seq never reused");
        assert_eq!(
            d.applied.keys().copied().collect::<Vec<_>>(),
            expect,
            "{point}"
        );
        assert!(d.skipped.is_empty(), "{point}");
    }
    let _ = std::fs::remove_dir_all(&base);
}
