//! ADR 0008 §4: the session log keeps acknowledged groups across crashes and
//! recovers the in-flight group entirely or not at all (shared tail algorithm).
use contextdb_model::*;
use contextdb_store::{
    session::{Layer, SessionEvent, SessionLog},
    *,
};
use std::path::Path;

const DIR: &str = "/db";
fn read(n: u32) -> SessionEvent {
    SessionEvent::Read {
        session_id: [7; 16],
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
