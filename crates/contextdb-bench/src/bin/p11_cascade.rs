//! P11 (ADR 0019) motivation: the cost of one commit whose status cascade
//! reaches every revision, eager `Counting`, in the core and through the
//! store (op_log frame + SQLite projections). Informational.
//! Usage: p11_cascade [n=1000000] [samples=5]
use contextdb_core::{CommitStats, Counting, Db};
use contextdb_model::*;
use contextdb_store::*;
use contextdb_testkit::{Shape, shape};
use std::{path::Path, time::Instant};

fn toggle(on: bool) -> Batch {
    let rev = RevisionId(0).into();
    Batch::new(vec![if on {
        Op::AssertBase { rev }
    } else {
        Op::RetractBase { rev }
    }])
}
fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}
fn core(kind: Shape, name: &str, n: usize, samples: usize) {
    let generated = shape(kind, n, 42);
    let mut db = Db::<Counting, CommitStats>::new();
    for b in &generated.batches {
        db.commit(b).unwrap();
    }
    for k in 0..samples {
        for on in [false, true] {
            let t = Instant::now();
            let r = db.commit(&toggle(on)).unwrap();
            let elapsed = ms(t);
            println!(
                "core shape={name} n={n} sample={k} base0={on} commit_ms={elapsed:.1} status_diff={} l0_flips={} incidences_visited={}",
                r.status_diff.len(),
                r.l0_flips.len(),
                db.stats().incidences_visited
            );
        }
    }
}
fn store(kind: Shape, name: &str, n: usize, samples: usize, dir: &Path, file: bool) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let options = if file {
        OpenOptions {
            projections: ProjectionFile::Path(dir.join(PROJECTIONS_FILE)),
            ..OpenOptions::default()
        }
    } else {
        OpenOptions::default()
    };
    let (mut s, _) = Store::open(StdDisk::default(), dir, &options).unwrap();
    let blob = s.put_blob(b"payload").unwrap();
    let mut generated = shape(kind, n, 42);
    // The shape's contents are synthetic hashes; point them at one real blob.
    for b in &mut generated.batches {
        for op in &mut b.ops {
            if let Op::CreateLogical { content, .. } = op {
                *content = blob;
            }
        }
    }
    let load = Instant::now();
    for b in &generated.batches {
        s.commit(b).unwrap();
    }
    println!(
        "store shape={name} n={n} file_projections={file} load_ms={:.0}",
        ms(load)
    );
    for k in 0..samples {
        for on in [false, true] {
            let before = log_bytes(dir);
            let t = Instant::now();
            let r = s.commit(&toggle(on)).unwrap();
            let elapsed = ms(t);
            println!(
                "store shape={name} n={n} file_projections={file} sample={k} base0={on} commit_ms={elapsed:.1} status_diff={} log_frame_bytes={}",
                r.status_diff.len(),
                log_bytes(dir) - before
            );
        }
    }
    drop(s);
    let t = Instant::now();
    let (s, report) = Store::open(StdDisk::default(), dir, &options).unwrap();
    println!(
        "store shape={name} n={n} file_projections={file} reopen_ms={:.0} replayed={}",
        ms(t),
        report.replayed
    );
    drop(s);
    let _ = std::fs::remove_dir_all(dir);
}
fn log_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir.join("log")).unwrap() {
        total += e.unwrap().metadata().unwrap().len();
    }
    total
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).map_or(1_000_000, |s| s.parse().unwrap());
    let samples: usize = args.get(2).map_or(5, |s| s.parse().unwrap());
    let root = std::env::temp_dir().join(format!("contextdb-p11-cascade-{}", std::process::id()));
    for (kind, name) in [(Shape::Hub, "hub"), (Shape::Chain, "chain")] {
        core(kind, name, n, samples);
        store(
            kind,
            name,
            n,
            samples,
            &root.join(format!("{name}-mem")),
            false,
        );
        store(
            kind,
            name,
            n,
            samples,
            &root.join(format!("{name}-file")),
            true,
        );
    }
}
