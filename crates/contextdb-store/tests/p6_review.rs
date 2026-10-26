//! Findings of the P6 independent review in the store: projections written
//! by a group commit equal a rebuild from the op_log (PROP10).
use contextdb_model::*;
use contextdb_store::*;
use rusqlite::Connection;
use std::path::Path;

fn dump(p: &Path) -> Vec<String> {
    let c = Connection::open(p).unwrap();
    let mut out = Vec::new();
    for q in [
        "SELECT 'diff', branch, epoch, revision, hex(post) FROM diff_log ORDER BY 2, 3, 4",
        "SELECT 'base', branch, rev_id, asserted_epoch, retracted_epoch FROM base_history ORDER BY 2, 3, 4",
        "SELECT 'rule', branch, rule_id, enabled, from_epoch FROM rule_enabled ORDER BY 2, 3, 5",
    ] {
        let mut s = c.prepare(q).unwrap();
        let rows = s
            .query_map([], |r| {
                let v = |i| r.get::<_, rusqlite::types::Value>(i);
                Ok(format!("{:?}", (v(0)?, v(1)?, v(2)?, v(3)?, v(4)?)))
            })
            .unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    out
}

/// Review (P6): a group whose batches flip a base and a rule back and forth
/// writes each epoch's rows as of that epoch, not as of the end of the
/// group; a historical read served by the projection after a restart
/// agrees with the live read.
#[test]
fn group_commit_projections_equal_a_rebuild() {
    let dir = std::env::temp_dir().join(format!("contextdb-p6-group-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let proj = dir.join("projections.sqlite");
    let opts = OpenOptions {
        projections: ProjectionFile::Path(proj.clone()),
        ..OpenOptions::default()
    };
    let live = {
        let (mut s, _) = Store::open(StdDisk::default(), &dir, &opts).unwrap();
        let (a, p) = (s.put_blob(b"a").unwrap(), s.put_blob(b"p").unwrap());
        s.commit(&Batch::new(vec![
            Op::CreateLogical {
                key: "p".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: p,
                adopt_as_base: true,
            },
            Op::CreateLogical {
                key: "a".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                content: a,
                adopt_as_base: false,
            },
            Op::AddRule {
                conclusion: RevRef::New(1),
                incidences: vec![(RevRef::New(0), EdgeKind::Sem)],
                enabled: true,
            },
        ]))
        .unwrap();
        let p0 = RevRef::Existing(RevisionId(0));
        let rule = |enabled| Op::SetRuleEnabled {
            rule: RuleRef::Existing(RuleId(0)),
            enabled,
        };
        let group = s.commit_group(&[
            Batch::new(vec![Op::RetractBase { rev: p0 }]),
            Batch::new(vec![Op::AssertBase { rev: p0 }]),
            Batch::new(vec![rule(false)]),
            Batch::new(vec![rule(true)]),
        ]);
        assert!(group.iter().all(Result::is_ok), "{group:?}");
        let live = s.read_historical(RevisionId(0), Epoch(2)).unwrap().0;
        s.snapshot().unwrap();
        live
    };
    {
        let (s, _) = Store::open(StdDisk::default(), &dir, &opts).unwrap();
        assert!(
            s.db().unwrap().initial_epoch() > Epoch(2),
            "the projection serves it"
        );
        assert_eq!(s.read_historical(RevisionId(0), Epoch(2)).unwrap().0, live);
    }
    let written = dump(&proj);
    let rebuild = OpenOptions {
        rebuild_projections: true,
        ..opts
    };
    let (mut s, _) = Store::open(StdDisk::default(), &dir, &rebuild).unwrap();
    s.catch_up_projections().unwrap();
    drop(s);
    assert_eq!(written, dump(&proj));
    let _ = std::fs::remove_dir_all(&dir);
}

fn fact(key: &str, content: u8, base: bool) -> Op {
    Op::CreateLogical {
        key: key.into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content: [content; 32],
        adopt_as_base: base,
    }
}
/// A format-2 log whose epoch 2 opens a Conflict: the P5 kernel had no
/// gate, so the migration changes that epoch's result.
fn conflict_log(disk: &SimDisk, dir: &Path) {
    let conflict = Op::OpenArtifact {
        ty: ArtifactType::Conflict,
        target: RevisionId(0).into(),
        payload_hash: [1; 32],
    };
    let batches: Vec<Batch> = [
        vec![fact("a", 0, true)],
        vec![conflict],
        vec![fact("b", 1, true)],
    ]
    .into_iter()
    .map(Batch::new)
    .collect();
    let pre_gate = |b: &Batch, r: CommitResult| match b.ops[0] {
        Op::OpenArtifact { .. } => CommitResult {
            status_diff: Vec::new(),
            ..r
        },
        _ => r,
    };
    migrate::write_format2_with(disk, dir, &batches, &pre_gate).unwrap();
}

/// Review (P6): a migration interrupted after staging still reports the
/// changed epochs when it resumes, and a rerun after a completed swap lists
/// them from the backup (T68).
#[test]
fn interrupted_migration_still_reports_changed_epochs() {
    let dir = Path::new("/db");
    // One rename succeeds and the second fails (resume); or the directory
    // sync after both renames fails (the swap is complete).
    for (op, n) in [("rename", 1), ("sync_dir", 2)] {
        let disk = SimDisk::new();
        conflict_log(&disk, dir);
        disk.fail_nth(op, n);
        assert!(migrate::migrate(&disk, dir).is_err(), "{op} {n}");
        let report = migrate::migrate(&disk, dir).unwrap();
        assert_eq!(report.changed, vec![Epoch(2)], "{op} {n}: {report:?}");
        let again = migrate::migrate(&disk, dir).unwrap();
        assert_eq!(again.changed, vec![Epoch(2)], "a rerun after completion");
    }
}

/// Review (P6): a format-2 store that came from format 0 keeps each row's
/// format-0 result (no `status_diff`) as `acked_result`; the 2 → 3 listing
/// compares against the result P5 derived, so only real changes appear.
#[test]
fn format0_descended_stores_list_only_real_changes() {
    let disk = SimDisk::new();
    let dir = Path::new("/db");
    let batches = [
        Batch::new(vec![fact("p", 1, true)]),
        Batch::new(vec![
            fact("a", 2, false),
            Op::AddRule {
                conclusion: RevRef::New(0),
                incidences: vec![(RevisionId(0).into(), EdgeKind::Sem)],
                enabled: true,
            },
        ]),
        Batch::new(vec![Op::RetractBase {
            rev: RevisionId(0).into(),
        }]),
    ];
    let log_dir = dir.join("log");
    disk.create_dir_all(&log_dir).unwrap();
    let seg = log_dir.join(log::segment_name(Epoch(1)));
    disk.write_new(
        &seg,
        &log::segment_header_with(&log::OP_LOG, 1, log::GENESIS, 2),
    )
    .unwrap();
    let mut db = contextdb_core::Db::default();
    let mut prev = log::GENESIS;
    for b in &batches {
        let result = db.commit(b).unwrap();
        let told = CommitResult {
            status_diff: Vec::new(),
            ..result.clone()
        };
        let batch_blob = b.canonical_bytes();
        let row = log::LogRow {
            epoch: result.epoch,
            branch: b.branch,
            principal: b.principal,
            channel: b.channel,
            client_txn_id: b.client_txn_id,
            batch_hash: *blake3::hash(&batch_blob).as_bytes(),
            batch_blob,
            result_blob: result.canonical_bytes(),
            acked_result: Some(told.canonical_bytes()),
            client_hash: None,
            prev_checksum: prev,
            group: log::GroupPos {
                seq: result.epoch.0,
                idx: 0,
                count: 1,
            },
        };
        let (bytes, sum) = log::frame_format(&row, 2);
        disk.append(&seg, &bytes).unwrap();
        prev = sum;
    }
    disk.sync(&seg).unwrap();
    disk.sync_dir(&log_dir).unwrap();
    let report = migrate::migrate(&disk, dir).unwrap();
    assert_eq!(report.changed, Vec::<Epoch>::new());
}
