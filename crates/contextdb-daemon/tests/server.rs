//! P3 transport: T37 (second instance refused), channel binding by socket,
//! protocol version errors, shutdown cleanup.
use contextdb_daemon::{
    proto::*,
    server::{Client, Daemon, Options, StartError},
};
use contextdb_model::*;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ctxd-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}
#[test]
fn t37_second_daemon_on_the_same_directory_is_refused() {
    let dir = tmp("t37");
    let first = Daemon::start(&dir, &Options::default()).unwrap();
    assert!(matches!(
        Daemon::start(&dir, &Options::default()),
        Err(StartError::Locked)
    ));
    drop(first);
    // After the first daemon stops, a new one starts.
    let again = Daemon::start(&dir, &Options::default()).unwrap();
    drop(again);
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn channel_is_bound_by_socket_and_shutdown_cleans_up() {
    let dir = tmp("chan");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut agent = Client::connect(d.socket(Channel::Agent)).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let start = Request::SessionStart {
        harness: "t".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
    };
    let Response::Session { session } = agent.call(&start).unwrap().response else {
        panic!()
    };
    let rec = Request::Record {
        binding: Binding::root(session),
        kind: RecordKind::Fact,
        body: b"hello".to_vec(),
        target: WriteTarget::Create {
            key: "hello".into(),
            scope_lane: "repo".into(),
        },
        deps: Vec::new(),
        adopt_as_base: true,
        client_txn_id: None,
    };
    let env = agent.call(&rec).unwrap();
    assert!(matches!(env.response, Response::Committed { .. }));
    assert_eq!(env.epoch, Epoch(0), "computed on the pre-commit snapshot");
    let status = agent.call(&Request::Status).unwrap();
    assert_eq!(status.epoch, Epoch(1));
    // The agent socket cannot shut the daemon down; the admin socket can.
    assert!(matches!(
        agent.call(&Request::Shutdown).unwrap().response,
        Response::Error { ref code, .. } if code == "ChannelRestricted"
    ));
    assert!(matches!(
        admin.call(&Request::Shutdown).unwrap().response,
        Response::Done
    ));
    d.wait();
    let (sockets, lease) = (d.sockets.clone(), d.lease.clone());
    drop(d);
    assert!(sockets.iter().all(|s| !s.exists()) && !lease.exists());
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn wrong_protocol_version_gets_an_error_and_a_closed_connection() {
    use std::io::{Read, Write};
    let dir = tmp("ver");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut s = std::os::unix::net::UnixStream::connect(d.socket(Channel::Agent)).unwrap();
    let mut frame = Vec::new();
    write_request(&mut frame, &Request::Status).unwrap();
    frame[4] = 99;
    s.write_all(&frame).unwrap();
    let env = read_response(&mut s).unwrap();
    assert!(matches!(env.response, Response::Error { ref code, .. } if code == "ProtocolVersion"));
    let mut rest = Vec::new();
    assert_eq!(s.read_to_end(&mut rest).unwrap(), 0, "connection closed");
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
fn session(c: &mut Client) -> SessionId {
    let start = Request::SessionStart {
        harness: "t".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
    };
    match c.call(&start).unwrap().response {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn create(c: &mut Client, s: SessionId, key: &str, body: &str) {
    let r = c
        .call(&Request::Record {
            binding: Binding::root(s),
            kind: RecordKind::Fact,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: true,
            client_txn_id: None,
        })
        .unwrap();
    assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
}
#[test]
fn t12_concurrent_writers_expecting_the_same_head_one_wins() {
    let dir = tmp("t12");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut c = Client::connect(d.socket(Channel::Agent)).unwrap();
    let s = session(&mut c);
    create(&mut c, s, "db", "v1");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let writers: Vec<_> = (0..2)
        .map(|n| {
            let (path, barrier) = (d.socket(Channel::Agent).to_path_buf(), barrier.clone());
            std::thread::spawn(move || {
                let mut c = Client::connect(&path).unwrap();
                let s = session(&mut c);
                barrier.wait();
                c.call(&Request::Record {
                    binding: Binding::root(s),
                    kind: RecordKind::Fact,
                    body: format!("v2-{n}").into_bytes(),
                    target: WriteTarget::Revise {
                        logical: LogicalId(0),
                        expected_head: RevisionId(0),
                    },
                    deps: Vec::new(),
                    adopt_as_base: true,
                    client_txn_id: None,
                })
                .unwrap()
                .response
            })
        })
        .collect();
    let results: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
    let wins = results
        .iter()
        .filter(|r| matches!(r, Response::Committed { .. }))
        .count();
    let conflicts: Vec<_> = results
        .iter()
        .filter_map(|r| match r {
            Response::Error { code, detail } if code == "HeadConflict" => Some(detail.clone()),
            _ => None,
        })
        .collect();
    assert_eq!((wins, conflicts.len()), (1, 1), "{results:?}");
    assert!(
        conflicts[0].contains("actual"),
        "conflict names the actual head: {}",
        conflicts[0]
    );
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn t08_readers_never_see_coupled_heads_out_of_step() {
    let dir = tmp("t08");
    let d = Daemon::start(&dir, &Options::default()).unwrap();
    let mut admin = Client::connect(d.socket(Channel::Admin)).unwrap();
    let mut c = Client::connect(d.socket(Channel::Agent)).unwrap();
    let s = session(&mut c);
    create(&mut c, s, "pair-a", "0");
    create(&mut c, s, "pair-b", "0");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (path, stop) = (d.socket(Channel::Agent).to_path_buf(), stop.clone());
            std::thread::spawn(move || {
                let mut c = Client::connect(&path).unwrap();
                let s = session(&mut c);
                let mut checked = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let r = c
                        .call(&Request::Recall {
                            binding: Binding::root(s),
                            query: "pair-".into(),
                            scope: "repo".into(),
                            mode: Mode::Advisory,
                            limit: 2,
                        })
                        .unwrap();
                    if let Response::Recall { items, .. } = r.response {
                        assert_eq!(items.len(), 2);
                        // Both heads move in one batch: their revision counts agree.
                        let (a, b) = (items[0].rev.0, items[1].rev.0);
                        assert_eq!(b, a + 1, "mixed epoch: a@{a} b@{b} at {:?}", r.epoch);
                        checked += 1;
                    }
                }
                checked
            })
        })
        .collect();
    // The writer adds a revision to both logicals and moves both heads atomically.
    let (mut ha, mut hb) = (0u32, 1u32);
    for n in 0..60u32 {
        let next_a = 2 + 2 * n;
        let batch = Batch::new(vec![
            Op::AddRevision {
                logical: LogicalId(0).into(),
                content: *blake3::hash(format!("a{n}").as_bytes()).as_bytes(),
                adopt_as_base: true,
            },
            Op::AddRevision {
                logical: LogicalId(1).into(),
                content: *blake3::hash(format!("b{n}").as_bytes()).as_bytes(),
                adopt_as_base: true,
            },
            Op::SetHead {
                logical: LogicalId(0).into(),
                expected: RevisionId(ha).into(),
                new: RevRef::New(0),
            },
            Op::SetHead {
                logical: LogicalId(1).into(),
                expected: RevisionId(hb).into(),
                new: RevRef::New(1),
            },
        ]);
        let blobs = vec![format!("a{n}").into_bytes(), format!("b{n}").into_bytes()];
        let r = admin.call(&Request::Raw { blobs, batch }).unwrap();
        assert!(matches!(r.response, Response::Committed { .. }), "{r:?}");
        ha = next_a;
        hb = next_a + 1;
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let checked: usize = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(checked > 0);
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
