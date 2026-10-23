//! A `ctxd` whose `PATH` has no `ps` (a reduced service or hook environment)
//! still writes a lease that others can verify: `ps` is found by its standard
//! location. (With no `PATH` at all, exec falls back to `/bin:/usr/bin`; a
//! `PATH` without `ps` did not.)
use contextdb_proto::lease;
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn lease_is_verifiable_when_path_has_no_ps() {
    let dir = std::env::temp_dir().join(format!("ctxd-lease-env-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let empty = dir.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let mut ctxd = Command::new(env!("CARGO_BIN_EXE_ctxd"))
        .arg(&dir)
        .env_clear()
        .env("PATH", &empty)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let lease = loop {
        if let Some(l) = lease::read_lease(&dir.join("ctxd.lease")) {
            break l;
        }
        assert!(Instant::now() < deadline, "no lease within 10 s");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(lease.pid, ctxd.id());
    assert!(!lease.start_time.is_empty(), "start time recorded");
    assert!(lease.is_live(), "the lease verifies as live");
    ctxd.kill().unwrap();
    ctxd.wait().unwrap();
    assert!(!lease.is_live(), "a dead ctxd's lease is not live");
    let _ = std::fs::remove_dir_all(&dir);
}
