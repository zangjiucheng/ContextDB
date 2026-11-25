//! P11 milestone 4 unit U3 (ADR 0019 §5): payload GC through ctxd, on a
//! real data directory.
//!
//! T67, daemon part:
//! - the dry run reports and changes nothing;
//! - `--apply` collects an old superseded revision, but keeps one a
//!   session certificate pins (K4) and one inside the epochs a held
//!   snapshot covers (K5);
//! - `ctx_read` of the collected revision answers `unavailable:
//!   "collected"`; `ctx_what_if` and `ctx_commit` of a `SetHead` to it give
//!   `MissingBlob`; `ctx_supersede` back to its text succeeds, after which
//!   the `SetHead` does too;
//! - `ctx reindex` after GC gives the projection rows the purge left;
//! - `ctx gc --compact` with ctxd stopped, then a restart: every read is
//!   unchanged and the kernel is the one before (PROP10);
//! - `keep_epochs` below 100 is refused, by the request and by `ctxd.toml`.
use contextdb_daemon::{
    proto::*,
    server::{Daemon, Options, StartError},
    service::Identity,
};
use contextdb_model::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId(0),
};
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

fn temp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ctx-p11-gc-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn h(b: &[u8]) -> ContentHash {
    *blake3::hash(b).as_bytes()
}
fn start(dir: &Path) -> Daemon {
    Daemon::start(
        dir,
        &Options {
            full_sync: false,
            team_lag_check: None,
            ..Options::default()
        },
    )
    .unwrap()
}
fn call(d: &Daemon, id: Identity, req: Request) -> Response {
    d.service.handle(id, req).response
}
fn code(r: &Response) -> &str {
    match r {
        Response::Error { code, .. } => code,
        _ => "ok",
    }
}
fn session(d: &Daemon) -> SessionId {
    match call(
        d,
        AGENT,
        Request::SessionStart {
            harness: "test".into(),
            repo: "r".into(),
            head_sha: "h".into(),
            parent: None,
            branch: None,
            git_branch: None,
        },
    ) {
        Response::Session { session } => session,
        other => panic!("{other:?}"),
    }
}
fn committed(r: Response) -> CommitResult {
    match r {
        Response::Committed { result, .. } => result,
        other => panic!("{other:?}"),
    }
}
/// A new record; its first revision.
fn create(d: &Daemon, s: SessionId, key: &str, body: &str, base: bool) -> RevisionId {
    committed(call(
        d,
        AGENT,
        Request::Record {
            binding: Binding::root(s),
            kind: RecordKind::Fact,
            body: body.as_bytes().to_vec(),
            target: WriteTarget::Create {
                key: key.into(),
                scope_lane: "repo".into(),
            },
            deps: Vec::new(),
            adopt_as_base: base,
            client_txn_id: None,
        },
    ))
    .allocations
    .new_revisions[0]
}
fn logical(d: &Daemon, key: &str) -> (LogicalId, RevisionId) {
    let snap = d.service.actor.snapshot();
    let v = snap.view(MAIN);
    let l = v.logical_by_key(key).unwrap();
    (l, v.head(l).unwrap())
}
fn supersede(d: &Daemon, s: SessionId, key: &str, body: &str) -> Response {
    let (l, head) = logical(d, key);
    call(
        d,
        AGENT,
        Request::Supersede {
            binding: Binding::root(s),
            logical: l,
            body: body.as_bytes().to_vec(),
            expected_head: head,
            adopt_as_base: false,
            carry_rules: false,
            reason: "test".into(),
            client_txn_id: None,
        },
    )
}
/// `(payload, unavailable)` of `key@rev`.
fn read(d: &Daemon, s: SessionId, key: &str, rev: RevisionId) -> (Option<Vec<u8>>, Option<String>) {
    match call(
        d,
        AGENT,
        Request::Read {
            binding: Binding::root(s),
            target: Target::KeyRev {
                key: key.into(),
                rev,
            },
            mode: Mode::Advisory,
            level: None,
        },
    ) {
        Response::Read {
            payload,
            unavailable,
            ..
        } => (payload, unavailable),
        other => panic!("{other:?}"),
    }
}
fn gc(d: &Daemon, keep_epochs: Option<u64>, apply: bool) -> Response {
    call(d, ADMIN, Request::Gc { keep_epochs, apply })
}
fn report(r: Response) -> GcSummary {
    match r {
        Response::Gc { report } => report,
        other => panic!("{other:?}"),
    }
}
fn status(d: &Daemon) -> (Epoch, IndexReport) {
    let r = d.service.handle(ADMIN, Request::Status);
    match r.response {
        Response::Status { index, .. } => (r.epoch, index),
        other => panic!("{other:?}"),
    }
}
/// Waits until recall and vectors are Ready at the head, with no reindex
/// still requested.
fn indexed(d: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let v = d.service.vectors.get().expect("an embedder");
    loop {
        let (e, i) = status(d);
        let ready =
            |s: &IndexState| matches!(s, IndexState::Ready { watermark } if *watermark == e);
        let requested = d.service.index_health.reindex_requested() || v.health.reindex_requested();
        if ready(&i.fts) && ready(&i.vectors) && !requested {
            return;
        }
        assert!(Instant::now() < deadline, "{i:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// The contents `vectors.sqlite` holds a vector for.
fn vector_rows(dir: &Path) -> BTreeSet<Vec<u8>> {
    let c = rusqlite::Connection::open(dir.join("vectors.sqlite")).unwrap();
    let mut stmt = c.prepare("SELECT content FROM vectors").unwrap();
    stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}
fn candidate_rows(d: &Daemon) -> Vec<(u64, RevisionId, RevisionId)> {
    (d.service.candidates.listed(None).unwrap().into_iter())
        .map(|c| (c.id, c.a, c.b))
        .collect()
}
fn set_head(l: LogicalId, expected: RevisionId, new: RevisionId) -> Vec<Op> {
    vec![Op::SetHead {
        logical: l.into(),
        expected: expected.into(),
        new: new.into(),
    }]
}
fn digests(d: &Daemon) -> Vec<ContentHash> {
    let snap = d.service.actor.snapshot();
    (snap.kernel.branches())
        .map(|b| snap.kernel.branch(b.id).unwrap().digest())
        .collect()
}

#[test]
fn t67_gc_through_ctxd() {
    let dir = temp("t67");
    std::fs::write(
        dir.join("embeddings.toml"),
        "[embedder]\ntype = \"mock\"\ndims = 32\n",
    )
    .unwrap();
    let d = start(&dir);
    let s = session(&d);
    // `old` and `gone`: v1 superseded early, nothing else names it.
    let old1 = create(&d, s, "old", "old body one", false);
    let gone1 = create(&d, s, "gone", "gone body one", false);
    indexed(&d); // v1 bodies have vectors before they are superseded
    committed(supersede(&d, s, "old", "old body two"));
    committed(supersede(&d, s, "gone", "gone body two"));
    // `pinned`: a session certificate names v1 (K4), then it moves on and
    // loses its base bit, so only the certificate keeps it.
    let pinned1 = create(&d, s, "pinned", "pinned body one", true);
    let reader = session(&d);
    let r = call(
        &d,
        AGENT,
        Request::Read {
            binding: Binding::root(reader),
            target: Target::Key("pinned".into()),
            mode: Mode::Certified,
            level: None,
        },
    );
    assert!(matches!(r, Response::Read { .. }), "{r:?}");
    let r = call(
        &d,
        AGENT,
        Request::Check {
            binding: Binding::root(reader),
            cert_id: None,
            live_pos: Vec::new(),
            action: None,
            mode: None,
            harness_version: String::new(),
            strict: false,
        },
    );
    let Response::Check {
        cert: Some(cert), ..
    } = r
    else {
        panic!("{r:?}")
    };
    assert!(cert.vector.iter().any(|v| v.rev == pinned1), "{cert:?}");
    committed(supersede(&d, s, "pinned", "pinned body two"));
    let mut retract = Batch::new(vec![Op::RetractBase {
        rev: pinned1.into(),
    }]);
    retract.channel = Channel::Admin;
    committed(call(
        &d,
        ADMIN,
        Request::Raw {
            blobs: Vec::new(),
            batch: retract,
        },
    ));
    // `held`: a reader holds a snapshot, then v1 moves on after it (K5).
    let held1 = create(&d, s, "held", "held body one", false);
    let held = d.service.actor.snapshot();
    committed(supersede(&d, s, "held", "held body two"));
    for n in 0..110 {
        create(&d, s, &format!("filler/{n}"), &format!("filler {n}"), false);
    }
    indexed(&d);
    let reads = [
        ("old", old1),
        ("gone", gone1),
        ("pinned", pinned1),
        ("held", held1),
    ];
    let before: Vec<_> = reads.iter().map(|(k, r)| read(&d, s, k, *r)).collect();
    assert!(before.iter().all(|(p, u)| p.is_some() && u.is_none()));

    // The dry run: what would go, and why the rest stays.
    let dry = report(gc(&d, Some(100), false));
    assert!(!dry.applied);
    assert_eq!(dry.e0, status(&d).0);
    assert_eq!((dry.cutoff, dry.cause), (held.epoch, GcCause::HeldSnapshot));
    assert_eq!(dry.held, held.epoch);
    assert_eq!(dry.payloads, 2, "old and gone v1: {dry:?}");
    assert!(
        dry.bytes > 0 && dry.kept_k4 >= 1 && dry.kept_k1 >= 114,
        "{dry:?}"
    );
    assert_eq!(dry.generation, 0);
    // It changed nothing: the same report again, and every body reads.
    assert_eq!(report(gc(&d, Some(100), false)), dry);
    let after_dry: Vec<_> = reads.iter().map(|(k, r)| read(&d, s, k, *r)).collect();
    assert_eq!(after_dry, before);

    // Below the floor.
    assert_eq!(code(&gc(&d, Some(99), true)), "InvalidRequest");
    assert_eq!(code(&gc(&d, Some(99), false)), "InvalidRequest");

    // Apply.
    let applied = report(gc(&d, Some(100), true));
    assert!(applied.applied && applied.purged, "{applied:?}");
    assert_eq!((applied.payloads, applied.bytes), (2, dry.bytes));
    assert_eq!(applied.generation, 1);
    let collected = [h(b"old body one"), h(b"gone body one")];
    assert_eq!(
        read(&d, s, "old", old1),
        (None, Some("collected".to_owned()))
    );
    assert_eq!(
        read(&d, s, "gone", gone1),
        (None, Some("collected".to_owned()))
    );
    assert_eq!(read(&d, s, "pinned", pinned1), before[2], "K4");
    assert_eq!(read(&d, s, "held", held1), before[3], "K5");
    // Released, the snapshot no longer bounds the cutoff: `held` v1 is
    // collectable now, and `pinned` v1 still is not.
    drop(held);
    let next = report(gc(&d, Some(100), false));
    assert_eq!(
        (next.cause, next.payloads),
        (GcCause::KeepEpochs, 1),
        "{next:?}"
    );

    // The purge left no vector for a collected content, and `ctx reindex`
    // gives the same rows.
    indexed(&d);
    let vectors = vector_rows(&dir);
    let candidates = candidate_rows(&d);
    for c in &collected {
        assert!(!vectors.contains(c.as_slice()));
    }
    assert!(vectors.contains(h(b"old body two").as_slice()));
    let r = call(
        &d,
        ADMIN,
        Request::Reindex {
            target: ReindexTarget::All,
        },
    );
    assert!(matches!(r, Response::Reindexed { .. }), "{r:?}");
    indexed(&d);
    assert_eq!(vector_rows(&dir), vectors);
    assert_eq!(candidate_rows(&d), candidates);

    // Making the collected revision the head again needs its bytes.
    let (l, head) = logical(&d, "old");
    let what_if = call(
        &d,
        AGENT,
        Request::WhatIf {
            binding: Binding::root(s),
            ops: set_head(l, head, old1),
            blobs: Vec::new(),
        },
    );
    assert_eq!(code(&what_if), "MissingBlob", "{what_if:?}");
    let commit = |d: &Daemon, ops: Vec<Op>| {
        call(
            d,
            AGENT,
            Request::Commit {
                binding: Binding::root(s),
                ops,
                blobs: Vec::new(),
                expect_cert_valid: false,
                client_txn_id: None,
            },
        )
    };
    let r = commit(&d, set_head(l, head, old1));
    assert_eq!(code(&r), "MissingBlob", "{r:?}");
    // `ctx_supersede` sends the bytes: the old text is a head again, and
    // the old revision reads again too (same content).
    committed(supersede(&d, s, "old", "old body one"));
    assert_eq!(read(&d, s, "old", old1), before[0]);
    let (l, head) = logical(&d, "old");
    committed(commit(&d, set_head(l, head, old1)));

    // `--compact` with ctxd stopped, then a restart.
    let reads_before: Vec<_> = reads.iter().map(|(k, r)| read(&d, s, k, *r)).collect();
    assert_eq!(reads_before[1], (None, Some("collected".to_owned())));
    let (epoch, kernel) = (status(&d).0, digests(&d));
    d.stop();
    drop(d);
    let r = contextdb_store::gc::compact(contextdb_store::StdDisk::default(), &dir).unwrap();
    assert!(
        r.records_dropped >= 2 && r.bytes_reclaimed > 0,
        "old and gone v1: {r:?}"
    );
    let d = start(&dir);
    assert_eq!((status(&d).0, digests(&d)), (epoch, kernel), "PROP10");
    let s = session(&d);
    let reads_after: Vec<_> = reads.iter().map(|(k, r)| read(&d, s, k, *r)).collect();
    assert_eq!(reads_after, reads_before);
    d.stop();
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}

/// O3: `ctxd` refuses to start with `[gc] keep_epochs` below 100, and the
/// request falls back to the configured value.
#[test]
fn keep_epochs_below_the_floor_is_refused_at_start() {
    let dir = temp("floor");
    std::fs::write(dir.join("ctxd.toml"), "[gc]\nkeep_epochs = 99\n").unwrap();
    match Daemon::start(&dir, &Options::default()) {
        Err(StartError::Config(e)) => assert!(e.contains("gc.keep_epochs"), "{e}"),
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("started with keep_epochs 99"),
    }
    std::fs::write(dir.join("ctxd.toml"), "[gc]\nkeep_epochs = 250\n").unwrap();
    let d = start(&dir);
    let r = report(gc(&d, None, false));
    assert_eq!((r.keep_epochs, r.payloads), (250, 0));
    // Only the admin socket runs GC.
    let r = call(
        &d,
        AGENT,
        Request::Gc {
            keep_epochs: None,
            apply: true,
        },
    );
    assert_eq!(code(&r), "ChannelRestricted");
    d.stop();
    drop(d);
    let _ = std::fs::remove_dir_all(&dir);
}
