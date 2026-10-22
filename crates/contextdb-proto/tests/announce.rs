//! ADR 0011 §9: the O(1) announce record that brackets a live sample.
use contextdb_model::{Action, Fact, ToolClass};
use contextdb_proto::outbox::Outbox;
use std::path::PathBuf;

fn root(name: &str) -> PathBuf {
    let r = std::env::temp_dir().join(format!("p5a-announce-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&r);
    std::fs::create_dir_all(&r).unwrap();
    r
}
fn edit() -> Fact {
    Fact::Action {
        action: Action {
            tool: ToolClass::Edit,
            paths: vec!["a.rs".into()],
            command: None,
        },
        ok: true,
    }
}
/// What the daemon checks around a sample: trusted, and the same record.
fn fresh(ob: &Outbox, before: &[u8]) -> bool {
    ob.announce_trusted() && ob.announce_record().ok().as_deref() == Some(before)
}

#[test]
fn edits_are_announced_other_facts_are_not_and_streams_do_not_matter() {
    let r = root("basic");
    let ob = Outbox::new(&r);
    // A missing record is never zero; ctxd start creates it and, with no
    // streams yet, marks the data dir as having only announcing appenders.
    assert!(ob.announce_record().is_err());
    ob.init_announce().unwrap();
    assert!(ob.announce_trusted());
    let before = ob.announce_record().unwrap();
    ob.append(&[1; 16], None, Fact::StreamEnd).unwrap();
    assert!(fresh(&ob, &before), "a non-edit fact is not announced");
    ob.append(&[1; 16], None, edit()).unwrap();
    assert!(
        !fresh(&ob, &before),
        "an edit during the sample is announced"
    );
    // 2,000 streams: the record is one small file, independent of streams.
    let before = ob.announce_record().unwrap();
    for k in 0..2000u32 {
        let mut s = [0u8; 16];
        s[..4].copy_from_slice(&k.to_le_bytes());
        ob.append(&s, None, Fact::StreamEnd).unwrap();
    }
    let t = std::time::Instant::now();
    for _ in 0..100 {
        assert!(fresh(&ob, &before));
    }
    assert!(
        t.elapsed() < std::time::Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn missing_corrupt_or_recreated_records_are_never_fresh() {
    let r = root("corrupt");
    let ob = Outbox::new(&r);
    ob.init_announce().unwrap();
    let path = r.join("outbox/announce");
    let good = std::fs::read(&path).unwrap();
    // Delete between the two reads: missing, so not fresh.
    std::fs::remove_file(&path).unwrap();
    assert!(!fresh(&ob, &good));
    // Short, trailing, flipped checksum, wrong version.
    let mut cases = vec![
        good[..good.len() - 1].to_vec(),
        [good.clone(), vec![0]].concat(),
    ];
    let mut flipped = good.clone();
    *flipped.last_mut().unwrap() ^= 1;
    cases.push(flipped);
    let mut version = good.clone();
    version[4] = 2;
    cases.push(version);
    for bad in cases {
        std::fs::write(&path, &bad).unwrap();
        assert!(ob.announce_record().is_err());
        assert!(!fresh(&ob, &good));
    }
    // The next edit heals a corrupt record with a new epoch: never equal to
    // a record seen before the corruption (delete/recreate ABA).
    ob.append(&[2; 16], None, edit()).unwrap();
    let healed = ob.announce_record().unwrap();
    assert_ne!(healed[8..24], good[8..24], "new epoch");
    assert!(!fresh(&ob, &good));
    // ctxd start also heals and removes stale tmp files left by killed hooks.
    std::fs::write(&path, b"junk").unwrap();
    std::fs::write(r.join("outbox/announce.999.tmp"), b"x").unwrap();
    ob.init_announce().unwrap();
    assert!(ob.announce_record().is_ok());
    assert!(!r.join("outbox/announce.999.tmp").exists());
    let _ = std::fs::remove_dir_all(r);
}

#[test]
fn marker_sentinel_and_repair() {
    let r = root("marker");
    let ob = Outbox::new(&r);
    // A data dir that already has streams (possibly from earlier hooks) is
    // not marked at start: untrusted until `ctx hooks install` marks it.
    ob.append(&[3; 16], None, Fact::StreamEnd).unwrap();
    ob.init_announce().unwrap();
    assert!(!ob.announce_trusted());
    ob.mark_appenders().unwrap();
    assert!(ob.announce_trusted());
    let before = ob.announce_record().unwrap();
    // A failed announce leaves the sentinel: untrusted until repaired, which
    // also starts a new epoch.
    std::fs::File::create(r.join("outbox/announce.failed")).unwrap();
    assert!(!fresh(&ob, &before));
    ob.repair_announce().unwrap();
    assert!(ob.announce_trusted());
    assert_ne!(ob.announce_record().unwrap(), before);
    let _ = std::fs::remove_dir_all(r);
}

/// A failed announce (record cannot be replaced) unlinks the record, creates
/// the sentinel, and still appends the fact (P4 at-least-once).
#[test]
fn failed_announce_unlinks_and_still_appends() {
    let r = root("failed");
    let ob = Outbox::new(&r);
    ob.init_announce().unwrap();
    let before = ob.announce_record().unwrap();
    // A directory where the hook's tmp file must go makes the write fail.
    let pid_tmp = r.join(format!("outbox/announce.{}.tmp", std::process::id()));
    std::fs::create_dir(&pid_tmp).unwrap();
    let seq = ob.append(&[4; 16], None, edit()).unwrap();
    assert!(seq.is_some(), "the fact is still appended");
    assert!(ob.announce_record().is_err(), "record unlinked");
    assert!(r.join("outbox/announce.failed").exists());
    assert!(!fresh(&ob, &before));
    std::fs::remove_dir(&pid_tmp).unwrap();
    ob.repair_announce().unwrap();
    assert!(ob.announce_trusted());
    let _ = std::fs::remove_dir_all(r);
}

/// An edit dropped at LOG_CAP was still announced (ADR 0011 §9.2).
#[test]
fn a_dropped_edit_is_announced() {
    let r = root("dropped");
    let ob = Outbox::new(&r);
    ob.init_announce().unwrap();
    let stream = [5; 16];
    ob.append(&stream, None, Fact::StreamEnd).unwrap();
    let log = r.join(format!(
        "outbox/{}.log",
        contextdb_proto::outbox::hex(&stream)
    ));
    let f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
    f.set_len(contextdb_proto::outbox::LOG_CAP).unwrap();
    let before = ob.announce_record().unwrap();
    assert_eq!(ob.append(&stream, None, edit()).unwrap(), None, "dropped");
    assert!(!fresh(&ob, &before));
    let _ = std::fs::remove_dir_all(r);
}

/// G4 (ADR 0011 §9): a hook dies inside an announce. Before the rename the
/// record is unchanged and no fact exists (nothing to miss; the tmp file is
/// cleaned at ctxd start). After the announce the record changed but no fact
/// was appended: one spurious Unknown, then fresh again; seqs never reused.
/// Crash points exist only in debug builds.
#[cfg(debug_assertions)]
#[test]
fn g4_hook_killed_inside_an_announce() {
    if let Ok(dir) = std::env::var("CTX_ANNOUNCE_CHILD") {
        let ob = Outbox::new(std::path::Path::new(&dir));
        let _ = ob.append(&[6; 16], None, edit());
        return;
    }
    for (point, record_changed) in [
        ("announce:before-rename", false),
        ("append:after-announce", true),
    ] {
        let r = root(&point.replace(':', "-"));
        let ob = Outbox::new(&r);
        ob.init_announce().unwrap();
        assert_eq!(ob.append(&[6; 16], None, edit()).unwrap(), Some(1));
        let before = ob.announce_record().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "g4_hook_killed_inside_an_announce",
                "--nocapture",
            ])
            .env("CTX_ANNOUNCE_CHILD", &r)
            .env("CTX_OUTBOX_CRASH", point)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "{point}: the child must die");
        assert_eq!(!fresh(&ob, &before), record_changed, "{point}");
        assert_eq!(
            ob.pending(&[6; 16]).unwrap().1.len(),
            1,
            "{point}: no fact appended"
        );
        // ctxd restart cleans tmp files; the next sample is fresh again and
        // the next edit gets the next seq.
        ob.init_announce().unwrap();
        let again = ob.announce_record().unwrap();
        assert!(fresh(&ob, &again), "{point}");
        assert_eq!(
            ob.append(&[6; 16], None, edit()).unwrap(),
            Some(2),
            "{point}"
        );
        let leftovers = std::fs::read_dir(r.join("outbox"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "{point}");
        let _ = std::fs::remove_dir_all(r);
    }
}

/// Property (ADR 0011 §9 verification): with concurrent appenders and
/// samplers, a sample that an edit append started after the first read and
/// finished before the second read never sees equal records.
#[test]
fn no_sample_overlapping_an_edit_append_sees_equal_records() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
    let r = root("concurrent");
    let ob = Outbox::new(&r);
    ob.init_announce().unwrap();
    let started = AtomicU64::new(0);
    let completed = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let (mut overlapping, mut quiet) = (0, 0);
    std::thread::scope(|s| {
        for w in 0..4u8 {
            let (ob, started, completed, stop) = (&ob, &started, &completed, &stop);
            s.spawn(move || {
                let mut k = 0u64;
                while !stop.load(SeqCst) {
                    started.fetch_add(1, SeqCst);
                    ob.append(&[10 + w; 16], None, edit()).unwrap();
                    completed.fetch_add(1, SeqCst);
                    k += 1;
                    std::thread::sleep(std::time::Duration::from_micros(50 + (k % 7) * 40));
                }
            });
        }
        // Windows from none to several fsync-bound appends long.
        for i in 0..1500u64 {
            let s1 = started.load(SeqCst);
            let Ok(r1) = ob.announce_record() else {
                continue;
            };
            match i % 3 {
                0 => {}
                1 => std::thread::sleep(std::time::Duration::from_micros(i % 300)),
                _ => std::thread::sleep(std::time::Duration::from_millis(1 + i % 6)),
            }
            let c2 = completed.load(SeqCst);
            let Ok(r2) = ob.announce_record() else {
                continue;
            };
            if c2 > s1 {
                overlapping += 1;
                assert_ne!(r1, r2, "sample {i} overlapped a completed edit append");
            } else if r1 == r2 {
                quiet += 1;
            }
        }
        stop.store(true, SeqCst);
    });
    // Both cases must actually occur, or the property says nothing.
    assert!(
        overlapping > 100 && quiet > 0,
        "overlapping={overlapping} quiet={quiet}"
    );
    let _ = std::fs::remove_dir_all(r);
}

/// The documented residual of §9.2: when neither the record can be replaced
/// nor unlinked nor the sentinel created (here: a read-only outbox
/// directory), the edit is still appended and is not announced.
#[test]
fn both_fail_residual_appends_unannounced() {
    use std::os::unix::fs::PermissionsExt;
    let r = root("residual");
    let ob = Outbox::new(&r);
    ob.init_announce().unwrap();
    let stream = [7; 16];
    ob.append(&stream, None, Fact::StreamEnd).unwrap();
    let before = ob.announce_record().unwrap();
    let dir = r.join("outbox");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root ignores the mode; the residual only exists for other users.
    if std::fs::write(dir.join("probe"), b"").is_ok() {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let appended = ob.append(&stream, None, edit());
    let still_fresh = fresh(&ob, &before);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(appended.unwrap(), Some(2), "the edit is still appended");
    assert!(
        still_fresh,
        "documented residual: not announced, no sentinel"
    );
    let _ = std::fs::remove_dir_all(r);
}
