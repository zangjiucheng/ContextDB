//! ADR 0008 §4: the session log keeps acknowledged groups across crashes and
//! recovers the in-flight group entirely or not at all (shared tail algorithm).
use contextdb_model::*;
use contextdb_store::{
    session::{Layer, SessionEvent, SessionLog},
    *,
};
use std::path::Path;

const DIR: &str = "/db";
#[test]
fn p5a_legacy_tail_rolls_before_new_event_and_old_reader_refuses_without_repair() {
    use contextdb_store::{
        log::{self, GroupPos, Record},
        session::{RootProof, SessionRecord},
    };
    let disk = SimDisk::new();
    let dir = Path::new(DIR).join("sessions");
    disk.create_dir_all(&dir).unwrap();
    let path = dir.join(log::segment_name(Epoch(1)));
    let legacy = SessionRecord {
        event: read(7),
        prev: log::GENESIS,
        group: GroupPos {
            seq: 1,
            idx: 0,
            count: 1,
        },
    };
    let (frame, _) = log::frame_with(&log::SESSION_LOG, &legacy.encode());
    let mut bytes = log::segment_header_with(&log::SESSION_LOG, 1, log::GENESIS, 1);
    bytes.extend(frame);
    disk.write_new(&path, &bytes).unwrap();
    disk.sync(&path).unwrap();
    disk.sync_dir(&dir).unwrap();
    let (mut opened, events) = open(&disk, usize::MAX);
    assert_eq!(events, vec![read(7)]);
    let cert = Certificate {
        cert_id: [0; 32],
        session_id: [7; 16],
        principal: PrincipalId(0),
        branch: MAIN,
        issued_epoch: Epoch(0),
        log_checksum_at_issue: [0; 32],
        code_commit: String::new(),
        watermarks: Vec::new(),
        entries: Vec::new(),
        vector: Vec::new(),
        strict_no_aba: false,
    };
    let event = SessionEvent::RootProof {
        agent: None,
        proof: Box::new(RootProof {
            cert,
            paths: vec!["code:repo/a.rs".into()],
        }),
    };
    opened.append_group(std::slice::from_ref(&event)).unwrap();
    drop(opened);
    assert_eq!(
        disk.read(&path).unwrap(),
        bytes,
        "legacy segment is immutable even below size target"
    );
    let segments = log::segments(&disk, &dir).unwrap();
    assert_eq!(segments.len(), 2);
    assert_eq!(
        log::parse_header_with(&log::SESSION_LOG, &disk.read(&segments[1]).unwrap())
            .unwrap()
            .format,
        2
    );
    let old = log::scan_with::<SessionRecord>(&log::SESSION_LOG, &disk, &dir, 1).unwrap();
    assert_eq!(old.newer_format, Some(2));
    assert!(old.torn_tail.is_none());
    assert!(old.empty_tail_segment.is_none());
    let before = disk.read(&segments[1]).unwrap();
    let (_, events) = open(&disk, usize::MAX);
    assert_eq!(events, vec![read(7), event]);
    assert_eq!(disk.read(&segments[1]).unwrap(), before);
}
fn read(n: u32) -> SessionEvent {
    SessionEvent::Read {
        session_id: [7; 16],
        agent: None,
        logical: LogicalId(n),
        rev: RevisionId(n),
        layer: Layer::L2,
        epoch: Epoch(u64::from(n)),
        required: n.is_multiple_of(2),
    }
}
fn open(disk: &SimDisk, target: usize) -> (SessionLog<SimDisk>, Vec<SessionEvent>) {
    let (log, events, _) = SessionLog::open(disk.clone(), Path::new(DIR), false, target).unwrap();
    (log, events)
}
#[test]
fn session_log_round_trip_with_rollover() {
    let disk = SimDisk::new();
    let (mut log, events) = open(&disk, 300);
    assert!(events.is_empty());
    let mut all = Vec::new();
    for g in 0..20u32 {
        let group: Vec<_> = (0..1 + g % 4).map(|k| read(g * 10 + k)).collect();
        log.append_group(&group).unwrap();
        all.extend(group);
    }
    drop(log);
    let (_, events) = open(&disk, 300);
    assert_eq!(events, all);
    assert!(disk.list(&Path::new(DIR).join("sessions")).unwrap().len() > 2);
}
fn runs() -> u64 {
    std::env::var("CONTEXTDB_G4_RUNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
}
#[test]
fn g4_session_log_groups_are_all_or_nothing() {
    let fates = [
        TailFate::Torn,
        TailFate::ZeroFill,
        TailFate::Garbage,
        TailFate::OutOfOrder,
    ];
    for seed in 0..runs() {
        let disk = SimDisk::new();
        let target = if seed.is_multiple_of(2) { 400 } else { 1 << 20 };
        let (mut log, _) = open(&disk, target);
        let mut acked = Vec::new();
        for g in 0..(seed % 6) as u32 {
            let group: Vec<_> = (0..1 + (seed as u32 + g) % 5)
                .map(|k| read(g * 10 + k))
                .collect();
            log.append_group(&group).unwrap();
            acked.extend(group);
        }
        let inflight: Vec<_> = (0..2 + seed as u32 % 4).map(|k| read(900 + k)).collect();
        let step = ["append", "sync", "none"][seed as usize % 3];
        if step != "none" {
            disk.fail_next(step);
        }
        let ok = log.append_group(&inflight).is_ok();
        assert_eq!(ok, step == "none");
        drop(log);
        let crashed = disk.crash(fates[seed as usize % 4], seed);
        let (_, events) = open(&crashed, target);
        let mut with = acked.clone();
        with.extend(inflight.clone());
        if ok {
            assert_eq!(events, with, "seed {seed}: acked group lost");
        } else {
            assert!(
                events == acked || events == with,
                "seed {seed}: partial group"
            );
            if step == "append" {
                assert_eq!(events, acked, "seed {seed}: unwritten group recovered");
            }
        }
    }
}
/// Review finding 3: a drain larger than a u16 group splits into several
/// synced groups instead of failing (and losing every event in it).
#[test]
fn oversized_drain_is_split_into_synced_groups() {
    let disk = SimDisk::new();
    let (mut log, _) = open(&disk, 64 << 20);
    let all: Vec<_> = (0..70_000u32).map(read).collect();
    log.append_group(&all).unwrap();
    drop(log);
    let (_, events) = open(&disk.crash(TailFate::ZeroFill, 0), 64 << 20);
    assert_eq!(events, all);
}
/// Closure check N2: groups a killed process left only in the page cache
/// are synced at open, so nothing replayed from them can vanish later.
#[test]
fn open_syncs_groups_a_killed_process_left_unsynced() {
    let disk = SimDisk::new();
    let (mut log, _) = open(&disk, 1 << 20);
    log.append_group(&[read(1)]).unwrap();
    disk.fail_next("sync");
    assert!(log.append_group(&[read(2)]).is_err());
    drop(log);
    let disk = disk.restart();
    let (_, events) = open(&disk, 1 << 20);
    assert_eq!(events, vec![read(1), read(2)]);
    let (_, events) = open(&disk.crash(TailFate::ZeroFill, 0), 1 << 20);
    assert_eq!(events, vec![read(1), read(2)]);
}
/// N2 closure confirmation: a rollover killed after the segment's file sync
/// but before its directory sync leaves a valid header-only segment that
/// later groups append to; open makes its name durable.
#[test]
fn open_syncs_the_name_of_a_segment_whose_rollover_was_killed() {
    let disk = SimDisk::new();
    let (mut log, _) = open(&disk, 1 << 20);
    disk.fail_next("sync_dir");
    assert!(log.append_group(&[read(1)]).is_err());
    drop(log);
    let disk = disk.restart();
    let (mut log, _) = open(&disk, 1 << 20);
    log.append_group(&[read(2)]).unwrap();
    drop(log);
    let (_, events) = open(&disk.crash(TailFate::ZeroFill, 0), 1 << 20);
    assert_eq!(events, vec![read(2)]);
}
/// ADR 0010 §4.3 (review M1): P3 session-log events (tags 1, 3, 5) still
/// decode under P4, as the root agent; P4 writes them under tags 15–17.
#[test]
fn p3_events_decode_as_the_root_agent() {
    let mut read = vec![1u8];
    [7u8; 16].encode(&mut read);
    LogicalId(3).encode(&mut read);
    RevisionId(4).encode(&mut read);
    Layer::L2.encode(&mut read);
    Epoch(5).encode(&mut read);
    true.encode(&mut read);
    let e = SessionEvent::from_bytes(&read).unwrap();
    assert_eq!(
        e,
        SessionEvent::Read {
            session_id: [7; 16],
            agent: None,
            logical: LogicalId(3),
            rev: RevisionId(4),
            layer: Layer::L2,
            epoch: Epoch(5),
            required: true,
        }
    );
    assert_eq!(e.canonical_bytes()[0], 15);
    let mut write = vec![3u8];
    [1u8; 16].encode(&mut write);
    [2u8; 16].encode(&mut write);
    Epoch(6).encode(&mut write);
    "why".to_string().encode(&mut write);
    assert!(matches!(
        SessionEvent::from_bytes(&write).unwrap(),
        SessionEvent::Write {
            agent: None,
            with_deps: false,
            ..
        }
    ));
    let mut cert = vec![5u8];
    [1u8; 16].encode(&mut cert);
    [9u8; 32].encode(&mut cert);
    assert!(matches!(
        SessionEvent::from_bytes(&cert).unwrap(),
        SessionEvent::SessionCert { agent: None, cert_id, .. } if cert_id == [9; 32]
    ));
    // Every P4 event round-trips.
    let p4 = [
        SessionEvent::SessionKey {
            session_id: [1; 16],
            harness: "codex".into(),
            harness_session: "t".into(),
        },
        SessionEvent::Fact {
            session_id: [1; 16],
            stream: [2; 16],
            packet: Packet {
                seq: 3,
                agent: Some("a".into()),
                fact: Fact::StreamEnd,
            },
            epoch: Epoch(1),
        },
        SessionEvent::Gate {
            session_id: [1; 16],
            agent: None,
            action: Action {
                tool: ToolClass::Edit,
                paths: vec!["x".into()],
                command: None,
            },
            mode: GateMode::Warn,
            verdict: "Valid".into(),
            decision: Decision::Allow,
            harness_version: "2.1.306".into(),
            epoch: Epoch(2),
            at: 1_790_000_000,
        },
        SessionEvent::Drift {
            session_id: [1; 16],
            agent: Some("a".into()),
            report: DriftReport::default(),
            epoch: Epoch(2),
        },
        SessionEvent::Reported {
            session_id: [1; 16],
            agent: None,
            items: vec![(LogicalId(1), (Some(RevisionId(2)), Summary::Certified))],
        },
        SessionEvent::StreamMark {
            stream: [2; 16],
            applied_through: 4,
            skipped: Some((2, 3)),
        },
        SessionEvent::Unclaimed { epoch: Epoch(9) },
        SessionEvent::Write {
            session_id: [1; 16],
            agent: Some("a".into()),
            client_txn_id: [3; 16],
            epoch: Epoch(4),
            reason: "record".into(),
            with_deps: true,
        },
        SessionEvent::SessionCert {
            session_id: [1; 16],
            agent: Some("a".into()),
            cert_id: [4; 32],
        },
    ];
    for e in p4 {
        assert_eq!(SessionEvent::from_bytes(&e.canonical_bytes()).unwrap(), e);
    }
}

#[test]
fn legacy_header_only_rollover_is_recovered_before_format_upgrade() {
    use contextdb_store::log::{self, GroupPos, Record};
    use contextdb_store::session::SessionRecord;
    for has_frame in [false, true] {
        let disk = SimDisk::new();
        let dir = Path::new(DIR).join("sessions");
        disk.create_dir_all(&dir).unwrap();
        let mut prev = log::GENESIS;
        let mut next = 1;
        if has_frame {
            let row = SessionRecord {
                event: read(1),
                prev,
                group: GroupPos {
                    seq: 1,
                    idx: 0,
                    count: 1,
                },
            };
            let (frame, sum) = log::frame_with(&log::SESSION_LOG, &row.encode());
            prev = sum;
            next = 2;
            let mut bytes = log::segment_header_with(&log::SESSION_LOG, 1, log::GENESIS, 1);
            bytes.extend(frame);
            disk.write_new(&dir.join(log::segment_name(Epoch(1))), &bytes)
                .unwrap();
        }
        let empty = dir.join(log::segment_name(Epoch(next)));
        disk.write_new(
            &empty,
            &log::segment_header_with(&log::SESSION_LOG, next, prev, 1),
        )
        .unwrap();
        let (mut log, events) = open(&disk, 1);
        assert_eq!(events.len(), usize::from(has_frame));
        log.append_group(&[read(2)]).unwrap();
        drop(log);
        let (_, events) = open(&disk.crash(TailFate::ZeroFill, 0), 1);
        assert_eq!(
            events,
            if has_frame {
                vec![read(1), read(2)]
            } else {
                vec![read(2)]
            }
        );
    }
}
