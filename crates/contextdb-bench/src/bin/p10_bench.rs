//! `p10_bench [entries=10000] [branches=10] [state_mib=100]` (ADR 0018 §13
//! G5). Each part prints one JSON line:
//!
//! - **import**: `entries` signed entries (one proposal, one 64-byte blob
//!   each) written as real git objects, then imported into a fresh replica
//!   from the tracking ref (budget ≤ 60 s for 10,000 on the remote host);
//! - **join**: the imported history caught up on `branches` open branches
//!   of a fresh kernel, one net batch each (budget ≤ 120 s; resident memory
//!   reported);
//! - **catch-up**: one more entry imported and caught up on every branch
//!   (budget ≤ 50 ms; batches reported);
//! - **compaction**: a state of `state_mib` MiB (1 MiB blobs), Σc, the
//!   checkpoint files and a checkpoint join, with peak resident memory
//!   (reported, no budget);
//! - **announce**: the `reference-transaction` announce in process — the
//!   fsynced append under the lock, then `TeamAnnounce` to a `Service`
//!   (budget ≤ 50 ms added to a fetch).
//!
//! Recall with 10⁴ mirrors and the publish and `git commit` hook overheads
//! are measured by `p10_g5.sh` with the real binaries.
use contextdb_core::Kernel;
use contextdb_model::*;
use contextdb_team::catchup;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::git::Repo;
use contextdb_team::ops::{TeamBatch, TeamOp};
use contextdb_team::replica::{EntryInput, Replica};
use contextdb_team::sshsig;
use contextdb_team::store::TeamStore;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

const TEAM: [u8; 16] = [0x10; 16];
const KEY: [u8; 32] = [1; 32];

fn arg(n: usize, d: u64) -> u64 {
    std::env::args()
        .nth(n)
        .and_then(|s| s.parse().ok())
        .unwrap_or(d)
}

fn rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

fn peak_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

fn doc(state_bytes: u64) -> TeamDoc {
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"a\"]\n\
         [members.a]\nkeys = [\"{}\"]\n[lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*members\"]\n\
         [limits]\nstate_bytes = {state_bytes}\n",
        hex(&TEAM),
        sshsig::public_key(&KEY).to_line()
    ))
    .unwrap()
}

/// `(raw commit, tree files, oid)` of a signed entry.
type Signed = (Vec<u8>, Vec<(String, Vec<u8>)>, String);

/// Signed entries, as an admin's edge writes them.
struct Chain {
    seq: u64,
    parent: Option<String>,
}

impl Chain {
    fn entry(&mut self, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>) -> Signed {
        let batch = TeamBatch {
            proposals: vec![ops],
        }
        .encode();
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&self.seq.to_be_bytes());
        let tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", "a".into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
            ("Ctx-Proposal", hex(&id)),
        ];
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        let t = hex(&contextdb_team::gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&t, &parents, "e", &tr);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(&KEY, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid =
            hex(&contextdb_team::gitobj::object_id(ObjectFormat::Sha1, "commit", &raw).unwrap());
        self.seq += 1;
        self.parent = Some(oid.clone());
        (raw, tree, oid)
    }
}

fn new_op(i: u64, c: [u8; 32]) -> TeamOp {
    TeamOp::New {
        path: format!("k/{i}"),
        lane: "main".into(),
        kind: RecordKind::Decision,
        content: c,
        adopt_as_base: true,
    }
}

fn main() {
    let entries = arg(1, 10_000);
    let branches = arg(2, 10);
    let state_mib = arg(3, 100);
    let tmp = std::env::temp_dir().join(format!("p10-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let repo_dir = tmp.join("repo");
    git(&tmp, &["init", "-q", repo_dir.to_str().unwrap()]);
    let repo = Repo::open(&repo_dir).unwrap();
    // Fixture (not timed): the chain as real git objects, one batch of
    // `git hash-object` per object kind would be faster; this is the
    // writer every edge uses.
    let t = Instant::now();
    let mut chain = Chain {
        seq: 0,
        parent: None,
    };
    let refname = format!("refs/contextdb/remotes/origin/team/{}/g0", hex(&TEAM));
    let write = |chain: &mut Chain, ops: Vec<TeamOp>, blobs: Vec<(String, Vec<u8>)>| {
        let (raw, tree, oid) = chain.entry(ops, blobs);
        let tid = repo.write_tree(&tree).unwrap();
        let _ = tid;
        let written = repo.write_commit(&raw).unwrap();
        assert_eq!(written, oid);
        oid
    };
    let genesis = write(
        &mut chain,
        vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc(1 << 30)),
        }],
        vec![],
    );
    for i in 1..entries {
        let body = format!("record {i:058}").into_bytes();
        let c = *blake3::hash(&body).as_bytes();
        write(
            &mut chain,
            vec![new_op(i, c)],
            vec![(format!("blobs/{}", hex(&c)), body)],
        );
    }
    let tip = chain.parent.clone().unwrap();
    repo.update_ref(&refname, &tip, None).unwrap();
    eprintln!(
        "fixture: {entries} entries in {:.1} s",
        t.elapsed().as_secs_f64()
    );

    // Import.
    let data = tmp.join("team");
    let t = Instant::now();
    let mut store = TeamStore::create(&data, TEAM, ObjectFormat::Sha1, "origin", &genesis).unwrap();
    let status = store.import(&repo).unwrap();
    let import_s = t.elapsed().as_secs_f64();
    let log_bytes = std::fs::metadata(data.join("log"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "{{\"part\":\"import\",\"entries\":{entries},\"seconds\":{import_s:.2},\"budget_s\":60,\"status\":\"{status:?}\",\"log_bytes\":{log_bytes},\"rss_kib\":{}}}",
        rss_kib().unwrap_or(0)
    );

    // Join: catch every open branch up from nothing.
    let mut k = Kernel::default();
    for b in 1..branches {
        let mut batch = Batch::new(vec![Op::CreateBranch {
            name: format!("b{b}"),
        }]);
        batch.channel = Channel::Admin;
        k.commit(&batch).unwrap();
    }
    let rss0 = rss_kib().unwrap_or(0);
    let t = Instant::now();
    let n = catchup::catch_up_all(&mut k, TEAM, store.history()).unwrap();
    let join_s = t.elapsed().as_secs_f64();
    println!(
        "{{\"part\":\"join\",\"entries\":{entries},\"branches\":{branches},\"batches\":{n},\"seconds\":{join_s:.2},\"budget_s\":120,\"rss_kib_before\":{rss0},\"rss_kib_after\":{}}}",
        rss_kib().unwrap_or(0)
    );

    // One more entry, imported and caught up on every branch.
    let body = b"one more".to_vec();
    let c = *blake3::hash(&body).as_bytes();
    let last = write(
        &mut chain,
        vec![new_op(entries, c)],
        vec![(format!("blobs/{}", hex(&c)), body)],
    );
    repo.update_ref(&refname, &last, Some(&tip)).unwrap();
    // The import's git calls, measured apart (reported).
    let t = Instant::now();
    let path = repo.first_parent_path(&last).unwrap();
    let walk_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let _ = repo.read_entries(std::slice::from_ref(&last)).unwrap();
    let read_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let _ = repo.resolve(&refname).unwrap();
    let resolve_ms = t.elapsed().as_secs_f64() * 1e3;
    eprintln!(
        "{{\"part\":\"import-one-parts\",\"walk_ms\":{walk_ms:.2},\"walk_len\":{},\"read_one_ms\":{read_ms:.2},\"resolve_ms\":{resolve_ms:.2}}}",
        path.len()
    );
    let t = Instant::now();
    store.import(&repo).unwrap();
    let import_ms = t.elapsed().as_secs_f64() * 1e3;
    let t = Instant::now();
    let n = catchup::catch_up_all(&mut k, TEAM, store.history()).unwrap();
    let catch_ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "{{\"part\":\"catch-up\",\"branches\":{branches},\"batches\":{n},\"catch_up_ms\":{catch_ms:.2},\"budget_ms\":50,\"import_one_ms\":{import_ms:.2}}}"
    );

    // Compaction at `state_mib` MiB.
    let mut r = Replica::new(TEAM, ObjectFormat::Sha1);
    let mut chain = Chain {
        seq: 0,
        parent: None,
    };
    let accept = |r: &mut Replica, chain: &mut Chain, ops, blobs| {
        let first = chain.parent.is_none();
        let seq = chain.seq;
        let (raw, tree, oid) = chain.entry(ops, blobs);
        let out = r.accept(&EntryInput {
            oid: &oid,
            seq,
            first_in_generation: first,
            commit: &raw,
            tree: &tree,
        });
        assert!(
            matches!(out, contextdb_team::replica::Outcome::Decided(ref d) if d.iter().all(Result::is_ok)),
            "{out:?}"
        );
    };
    accept(
        &mut r,
        &mut chain,
        vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc((state_mib + 16) << 20)),
        }],
        vec![],
    );
    for i in 0..state_mib {
        let mut body = vec![0u8; (1 << 20) - 64];
        body[..8].copy_from_slice(&i.to_le_bytes());
        let c = *blake3::hash(&body).as_bytes();
        accept(
            &mut r,
            &mut chain,
            vec![new_op(i, c)],
            vec![(format!("blobs/{}", hex(&c)), body)],
        );
    }
    let peak0 = peak_kib().unwrap_or(0);
    let t = Instant::now();
    let sc = r.sigma_c("a", [0xcc; 16], chain.seq).unwrap();
    let files = r.checkpoint_files(&sc, 1);
    let ckpt_s = t.elapsed().as_secs_f64();
    let total: usize = files.iter().map(|(_, b)| b.len()).sum();
    let peak1 = peak_kib().unwrap_or(0);
    let tree = files.clone();
    let tid = hex(&contextdb_team::gitobj::tree_id(ObjectFormat::Sha1, &tree).unwrap());
    let tr: Vec<(&str, String)> = vec![
        ("Ctx-Team", hex(&TEAM)),
        ("Ctx-Team-Seq", (chain.seq + 1).to_string()),
        ("Ctx-Author", "a".into()),
        ("Ctx-Accept-Version", "1".into()),
        ("Ctx-Compacts", "0".repeat(40)),
    ];
    let unsigned = commit::build(&tid, &[], "ctx team: checkpoint", &tr);
    let raw = commit::with_signature(
        &unsigned,
        &sshsig::sign(&KEY, "git", &unsigned),
        ObjectFormat::Sha1,
    );
    let t = Instant::now();
    let joined = Replica::join_checkpoint(TEAM, ObjectFormat::Sha1, &raw, &tree);
    let join_ckpt_s = t.elapsed().as_secs_f64();
    println!(
        "{{\"part\":\"compaction\",\"state_mib\":{state_mib},\"checkpoint_bytes\":{total},\"checkpoint_s\":{ckpt_s:.2},\"join_s\":{join_ckpt_s:.2},\"join_ok\":{},\"peak_kib_before\":{peak0},\"peak_kib_after_ckpt\":{peak1},\"peak_kib_end\":{}}}",
        joined.is_ok(),
        peak_kib().unwrap_or(0)
    );

    // The announce of `reference-transaction`, in process.
    let dir = tmp.join("ann");
    std::fs::create_dir_all(&dir).unwrap();
    let mut times = Vec::new();
    for i in 0..200u32 {
        let t = Instant::now();
        contextdb_daemon::team::append_announce(
            &dir,
            &contextdb_daemon::team::Announce {
                refname: refname.clone(),
                oid: format!("{i:040x}"),
                pid: std::process::id(),
            },
        )
        .unwrap();
        times.push(t.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    println!(
        "{{\"part\":\"announce-append\",\"n\":200,\"p50_ms\":{:.2},\"p99_ms\":{:.2},\"budget_ms\":50}}",
        times[100], times[198]
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
