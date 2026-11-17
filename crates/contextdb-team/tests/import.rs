//! Import through real git (ADR 0018 §8.1–8.2, §8.6–8.7; T110, T114 and the
//! real-object half of T118): a bare remote, a publisher writing signed
//! entries as real git objects, edges that fetch the team refs through an
//! added refspec and import. Every object id is git's own.

use contextdb_model::RecordKind;
use contextdb_team::commit::{self, ObjectFormat, hex};
use contextdb_team::doc::TeamDoc;
use contextdb_team::git::Repo;
use contextdb_team::ops::{Hash, TeamBatch, TeamOp};
use contextdb_team::replica::{EntryInput, Outcome, Replica};
use contextdb_team::sshsig;
use contextdb_team::store::{Status, TeamStore};
use std::path::{Path, PathBuf};
use std::process::Command;

const TEAM: [u8; 16] = [0x7e; 16];
const ALICE: [u8; 32] = [1; 32];
const BOB: [u8; 32] = [2; 32];

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn doc() -> TeamDoc {
    TeamDoc::from_toml(&format!(
        "version = 1\naccept_version = 1\nid = \"{}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"alice\"]\n",
        hex(&TEAM),
        sshsig::public_key(&ALICE).to_line(),
        sshsig::public_key(&BOB).to_line(),
    ))
    .unwrap()
}

struct World {
    tmp: PathBuf,
    remote: PathBuf,
}

impl World {
    fn new(name: &str) -> Self {
        let tmp =
            std::env::temp_dir().join(format!("ctx-team-import-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let remote = tmp.join("remote.git");
        git(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
        git(&remote, &["config", "uploadpack.allowFilter", "true"]);
        git(
            &remote,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        Self { tmp, remote }
    }
    /// A clone that tracks the team refs (the join's added refspec).
    fn edge(&self, name: &str, args: &[&str]) -> PathBuf {
        let dir = self.tmp.join(name);
        let url = format!("file://{}", self.remote.display());
        let mut a = vec!["clone", "-q"];
        a.extend_from_slice(args);
        a.extend_from_slice(&[url.as_str(), dir.to_str().unwrap()]);
        git(&self.tmp, &a);
        git(
            &dir,
            &[
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/contextdb/team/*:refs/contextdb/remotes/origin/team/*",
            ],
        );
        dir
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Writes signed entries as real objects and pushes them.
struct Publisher {
    repo: Repo,
    dir: PathBuf,
    generation: u32,
    seq: u64,
    parent: Option<String>,
    next_id: u8,
    /// The publisher's own replica, fed what it publishes.
    replica: Replica,
}

impl Publisher {
    fn new(w: &World) -> Self {
        let dir = w.tmp.join("publisher");
        git(&w.tmp, &["init", "-q", dir.to_str().unwrap()]);
        git(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                &format!("file://{}", w.remote.display()),
            ],
        );
        let repo = Repo::open(&dir).unwrap();
        Self {
            repo,
            dir,
            generation: 0,
            seq: 0,
            parent: None,
            next_id: 1,
            replica: Replica::new(TEAM, ObjectFormat::Sha1),
        }
    }

    fn refname(&self) -> String {
        format!("refs/contextdb/team/{}/g{}", hex(&TEAM), self.generation)
    }

    fn push(&self) {
        let r = self.refname();
        git(&self.dir, &["push", "-q", "origin", &format!("{r}:{r}")]);
    }

    fn write(
        &mut self,
        trailers: Vec<(&str, String)>,
        tree: Vec<(String, Vec<u8>)>,
        key: &[u8; 32],
        subject: &str,
    ) -> String {
        let tree_oid = self.repo.write_tree(&tree).unwrap();
        let parents: Vec<String> = self.parent.iter().cloned().collect();
        let unsigned = commit::build(&tree_oid, &parents, subject, &trailers);
        let raw = commit::with_signature(
            &unsigned,
            &sshsig::sign(key, "git", &unsigned),
            ObjectFormat::Sha1,
        );
        let oid = self.repo.write_commit(&raw).unwrap();
        let old = self.parent.clone();
        self.repo
            .update_ref(&self.refname(), &oid, old.as_deref())
            .unwrap();
        let out = self.replica.accept(&EntryInput {
            oid: &oid,
            seq: self.seq,
            first_in_generation: self.parent.is_none(),
            commit: &raw,
            tree: &tree,
        });
        assert!(
            !matches!(out, Outcome::Stall(_) | Outcome::Ignored),
            "{out:?}"
        );
        self.parent = Some(oid.clone());
        self.seq += 1;
        oid
    }

    fn entry(
        &mut self,
        key: &[u8; 32],
        author: &str,
        proposals: Vec<Vec<TeamOp>>,
        blobs: Vec<(String, Vec<u8>)>,
    ) -> String {
        let batch = TeamBatch { proposals }.encode();
        let n = TeamBatch::decode(&batch).unwrap().proposals.len();
        let mut tr: Vec<(&str, String)> = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", author.into()),
            ("Ctx-Batch-Hash", TeamBatch::hash_hex(&batch)),
            ("Ctx-Accept-Version", "1".into()),
        ];
        for _ in 0..n {
            tr.push(("Ctx-Proposal", hex(&[self.next_id; 16])));
            self.next_id += 1;
        }
        let mut tree = vec![("batch".to_owned(), batch)];
        tree.extend(blobs);
        self.write(tr, tree, key, "ctx team: entry")
    }

    /// Step 1 and step 2 of §8.6, as an admin's ctxd does them.
    fn compact(&mut self, push_successor: bool) -> (String, String) {
        let id = [self.next_id; 16];
        let sc = self.replica.sigma_c("alice", id, self.seq).unwrap();
        let files = self.replica.checkpoint_files(&sc, self.generation + 1);
        let t = self.repo.write_tree(&files).unwrap();
        let tree = (0..t.len() / 2)
            .map(|i| u8::from_str_radix(&t[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let c = self.entry(
            &ALICE,
            "alice",
            vec![vec![TeamOp::Compact {
                generation: self.generation + 1,
                tree,
                digest: sc.digest(),
            }]],
            vec![],
        );
        self.push();
        assert!(self.replica.pending().is_some());
        // The successor, on the new ref.
        self.generation += 1;
        self.parent = None;
        let tr = vec![
            ("Ctx-Team", hex(&TEAM)),
            ("Ctx-Team-Seq", self.seq.to_string()),
            ("Ctx-Author", "alice".to_owned()),
            ("Ctx-Accept-Version", "1".to_owned()),
            ("Ctx-Compacts", c.clone()),
        ];
        let f = self.write(tr, files, &ALICE, "ctx team: checkpoint");
        assert_eq!(self.replica.pending(), None);
        if push_successor {
            self.push();
        }
        (c, f)
    }
}

fn blob(bytes: &[u8]) -> (Hash, (String, Vec<u8>)) {
    let h = *blake3::hash(bytes).as_bytes();
    (h, (format!("blobs/{}", hex(&h)), bytes.to_vec()))
}

fn new_op(path: &str, h: Hash) -> TeamOp {
    TeamOp::New {
        path: path.into(),
        lane: "main".into(),
        kind: RecordKind::Fact,
        content: h,
        adopt_as_base: false,
    }
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

#[test]
fn import_restart_and_torn_log() {
    if !have_git() {
        return;
    }
    let w = World::new("restart");
    let mut p = Publisher::new(&w);
    let genesis = p.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc()),
        }]],
        vec![],
    );
    let (a, ba) = blob(b"a");
    p.entry(&BOB, "bob", vec![vec![new_op("k/a", a)]], vec![ba.clone()]);
    // A rejected proposal (missing blob) and a valid one after it.
    let (b, bb) = blob(b"b");
    p.entry(&BOB, "bob", vec![vec![new_op("k/b", b)]], vec![]);
    p.entry(&BOB, "bob", vec![vec![new_op("k/b", b)]], vec![bb]);
    p.push();
    let edge = w.edge("edge", &[]);
    git(&edge, &["fetch", "-q", "origin"]);
    let repo = Repo::open(&edge).unwrap();
    let dir = w.tmp.join("data-edge");
    let mut s = TeamStore::create(&dir, TEAM, ObjectFormat::Sha1, "origin", &genesis).unwrap();
    assert_eq!(s.import(&repo).unwrap(), Status::Imported(4));
    assert_eq!(s.import(&repo).unwrap(), Status::UpToDate);
    assert_eq!(s.replica().digest(), p.replica.digest());
    // The catch-up history ends at the tip, one position per entry.
    let projection = contextdb_team::catchup::projection(s.replica());
    assert_eq!(s.history().tip(), s.tip());
    assert_eq!(s.history().entries.len(), 4);
    assert_eq!(s.history().current(), &projection);
    // Restart: the log replays to the same state and history.
    let entries = s.history().entries.clone();
    drop(s);
    let s = TeamStore::open(&dir).unwrap();
    assert_eq!(s.replica().digest(), p.replica.digest());
    assert_eq!(s.replica().seq(), Some(3));
    assert_eq!(s.history().entries, entries);
    drop(s);
    // A torn tail: garbage after the last frame is cut on open.
    let log = dir.join("log");
    let mut bytes = std::fs::read(&log).unwrap();
    bytes.extend_from_slice(b"CXTL\x00\x00\x10\x00partial");
    std::fs::write(&log, &bytes).unwrap();
    let s = TeamStore::open(&dir).unwrap();
    assert_eq!(s.replica().digest(), p.replica.digest());
    drop(s);
    // A torn last frame: its entry is imported again from git.
    let len = std::fs::metadata(&log).unwrap().len();
    let f = std::fs::OpenOptions::new().write(true).open(&log).unwrap();
    f.set_len(len - 10).unwrap();
    drop(f);
    let mut s = TeamStore::open(&dir).unwrap();
    assert_eq!(s.replica().seq(), Some(2));
    assert_eq!(s.import(&repo).unwrap(), Status::Imported(1));
    assert_eq!(s.replica().digest(), p.replica.digest());
    // `ctx team rebuild`: discarded and replayed from the genesis.
    drop(s);
    let (mut s, status) = TeamStore::rebuild(&dir, &repo).unwrap();
    assert_eq!(status, Status::Imported(4));
    assert_eq!(s.replica().digest(), p.replica.digest());
    assert_eq!(s.history().entries.len(), 4);
    assert!(!dir.join("log.rebuild").exists());
    // More entries later.
    let (c, bc) = blob(b"c");
    p.entry(&BOB, "bob", vec![vec![new_op("k/c", c)]], vec![bc]);
    p.push();
    git(&edge, &["fetch", "-q", "origin"]);
    assert_eq!(s.import(&repo).unwrap(), Status::Imported(1));
    assert_eq!(s.replica().digest(), p.replica.digest());
}

#[test]
fn tracking_ref_cases() {
    if !have_git() {
        return;
    }
    let w = World::new("cases");
    let mut p = Publisher::new(&w);
    let genesis = p.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc()),
        }]],
        vec![],
    );
    let (a, ba) = blob(b"a");
    let first = p.entry(&BOB, "bob", vec![vec![new_op("k/a", a)]], vec![ba]);
    p.push();
    let edge = w.edge("edge", &[]);
    git(&edge, &["fetch", "-q", "origin"]);
    let repo = Repo::open(&edge).unwrap();
    let mut s = TeamStore::create(
        &w.tmp.join("d1"),
        TEAM,
        ObjectFormat::Sha1,
        "origin",
        &genesis,
    )
    .unwrap();
    assert_eq!(s.import(&repo).unwrap(), Status::Imported(2));
    let tracking = s.tracking_ref(0);
    // Ancestor (a stale fetch): nothing.
    git(&edge, &["update-ref", &tracking, &genesis]);
    assert_eq!(s.import(&repo).unwrap(), Status::UpToDate);
    git(&edge, &["update-ref", &tracking, &first]);
    // Missing (pruned after the remote deleted it).
    git(&edge, &["update-ref", "-d", &tracking]);
    assert_eq!(s.import(&repo).unwrap(), Status::RootMissing);
    // Unrelated history: a rewritten remote.
    let other_dir = w.tmp.join("rewriter");
    git(&w.tmp, &["init", "-q", other_dir.to_str().unwrap()]);
    let rw = Repo::open(&other_dir).unwrap();
    let t = rw
        .write_tree(&[("batch".into(), b"junk".to_vec())])
        .unwrap();
    let unsigned = commit::build(&t, &[], "rewrite", &[("Ctx-Team", hex(&TEAM))]);
    let oid = rw.write_commit(&unsigned).unwrap();
    git(
        &edge,
        &[
            "fetch",
            "-q",
            other_dir.to_str().unwrap(),
            &format!("{oid}:{tracking}"),
        ],
    );
    assert!(matches!(s.import(&repo).unwrap(), Status::Diverged(_)));
    // A store joined with the wrong genesis never follows the chain.
    git(&edge, &["fetch", "-q", "origin"]);
    let mut wrong = TeamStore::create(
        &w.tmp.join("d2"),
        TEAM,
        ObjectFormat::Sha1,
        "origin",
        &first,
    )
    .unwrap();
    assert!(matches!(wrong.import(&repo).unwrap(), Status::Diverged(_)));
}

#[test]
fn compaction_with_real_objects() {
    if !have_git() {
        return;
    }
    let w = World::new("compact");
    let mut p = Publisher::new(&w);
    let genesis = p.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc()),
        }]],
        vec![],
    );
    let (a, ba) = blob(b"a");
    p.entry(&BOB, "bob", vec![vec![new_op("k/a", a)]], vec![ba]);
    p.push();
    // An online edge imports now; an offline edge has not fetched at all.
    let online = w.edge("online", &[]);
    git(&online, &["fetch", "-q", "origin"]);
    let orepo = Repo::open(&online).unwrap();
    let mut on = TeamStore::create(
        &w.tmp.join("d-online"),
        TEAM,
        ObjectFormat::Sha1,
        "origin",
        &genesis,
    )
    .unwrap();
    assert_eq!(on.import(&orepo).unwrap(), Status::Imported(2));
    // Compact pushed, successor not yet: CompactPending.
    let (compact, f) = p.compact(false);
    git(&online, &["fetch", "-q", "origin"]);
    assert_eq!(on.import(&orepo).unwrap(), Status::Imported(1));
    assert_eq!(on.import(&orepo).unwrap(), Status::CompactPending);
    // Junk appended to generation 0 after the Compact (a member's late push).
    let junk_dir = w.tmp.join("junk");
    git(
        &w.tmp,
        &[
            "clone",
            "-q",
            &format!("file://{}", w.remote.display()),
            junk_dir.to_str().unwrap(),
        ],
    );
    git(
        &junk_dir,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/contextdb/team/*:refs/contextdb/team/*",
        ],
    );
    let jr = Repo::open(&junk_dir).unwrap();
    let r0 = format!("refs/contextdb/team/{}/g0", hex(&TEAM));
    let t = jr
        .write_tree(&[("batch".into(), b"junk".to_vec())])
        .unwrap();
    let junk = jr
        .write_commit(&commit::build(
            &t,
            std::slice::from_ref(&compact),
            "junk",
            &[("Ctx-Team", hex(&TEAM))],
        ))
        .unwrap();
    jr.update_ref(&r0, &junk, Some(&compact)).unwrap();
    git(&junk_dir, &["push", "-q", "origin", &format!("{r0}:{r0}")]);
    // Now the successor lands.
    p.push();
    git(&online, &["fetch", "-q", "origin"]);
    assert_eq!(on.import(&orepo).unwrap(), Status::Imported(1));
    assert_eq!(on.replica().generation(), 1);
    assert_eq!(on.replica().digest(), p.replica.digest());
    // The offline edge sees everything at once and follows.
    let offline = w.edge("offline", &[]);
    git(&offline, &["fetch", "-q", "origin"]);
    let frepo = Repo::open(&offline).unwrap();
    let mut off = TeamStore::create(
        &w.tmp.join("d-offline"),
        TEAM,
        ObjectFormat::Sha1,
        "origin",
        &genesis,
    )
    .unwrap();
    assert_eq!(off.import(&frepo).unwrap(), Status::Imported(4));
    assert_eq!(off.replica().digest(), p.replica.digest());
    // A checkpoint joiner given F out of band.
    let mut joiner =
        TeamStore::create_from_checkpoint(&w.tmp.join("d-join"), TEAM, &frepo, "origin", &f)
            .unwrap();
    assert_eq!(joiner.replica().digest(), p.replica.digest());
    // Generation 1 continues for everyone, and restarts replay it.
    let (b, bb) = blob(b"b");
    p.entry(&BOB, "bob", vec![vec![new_op("k/b", b)]], vec![bb]);
    p.push();
    for dir in [&online, &offline] {
        git(dir, &["fetch", "-q", "origin"]);
    }
    assert_eq!(on.import(&orepo).unwrap(), Status::Imported(1));
    assert_eq!(off.import(&frepo).unwrap(), Status::Imported(1));
    assert_eq!(joiner.import(&frepo).unwrap(), Status::Imported(1));
    for d in [
        on.replica().digest(),
        off.replica().digest(),
        joiner.replica().digest(),
    ] {
        assert_eq!(d, p.replica.digest());
    }
    // A checkpoint joiner's history starts at its anchor and follows on.
    assert_eq!(joiner.history().tip(), joiner.tip());
    assert_eq!(
        joiner.history().current(),
        &contextdb_team::catchup::projection(joiner.replica())
    );
    let dir = joiner.dir().to_owned();
    let entries = joiner.history().entries.clone();
    drop(joiner);
    let reopened = TeamStore::open(&dir).unwrap();
    assert_eq!(reopened.replica().digest(), p.replica.digest());
    assert_eq!(reopened.history().entries, entries);
    let dir = off.dir().to_owned();
    drop(off);
    assert_eq!(
        TeamStore::open(&dir).unwrap().replica().digest(),
        p.replica.digest()
    );
    // The edge followed the Compact: its trust anchor is the verified
    // checkpoint entry, and a rebuild replays from there (Q13).
    let anchor = std::fs::read_to_string(dir.join("anchor")).unwrap();
    assert!(!anchor.contains(&format!("anchor {genesis}\n")), "{anchor}");
    let (s, _) = TeamStore::rebuild(&dir, &frepo).unwrap();
    assert_eq!(s.replica().digest(), p.replica.digest());
}

#[test]
fn partial_clone_reports_missing_blobs_until_fetched() {
    if !have_git() {
        return;
    }
    let w = World::new("partial");
    let mut p = Publisher::new(&w);
    let genesis = p.entry(
        &ALICE,
        "alice",
        vec![vec![TeamOp::SetTeamAccess {
            doc: Box::new(doc()),
        }]],
        vec![],
    );
    let (a, ba) = blob(b"partial clone payload");
    p.entry(&BOB, "bob", vec![vec![new_op("k/a", a)]], vec![ba]);
    p.push();
    let edge = w.edge("edge", &["--filter=blob:none"]);
    git(&edge, &["fetch", "-q", "origin"]);
    let repo = Repo::open(&edge).unwrap();
    let mut s = TeamStore::create(
        &w.tmp.join("d"),
        TEAM,
        ObjectFormat::Sha1,
        "origin",
        &genesis,
    )
    .unwrap();
    let Status::Incomplete(missing) = s.import(&repo).unwrap() else {
        panic!("expected Incomplete")
    };
    assert!(!missing.is_empty());
    // The hook's by-OID fetch, in the user's environment.
    let mut args = vec!["fetch", "-q", "origin"];
    args.extend(missing.iter().map(String::as_str));
    git(&edge, &args);
    loop {
        match s.import(&repo).unwrap() {
            Status::Incomplete(m) => {
                let mut args = vec!["fetch", "-q", "origin"];
                args.extend(m.iter().map(String::as_str));
                git(&edge, &args);
            }
            other => {
                assert!(matches!(other, Status::Imported(_)), "{other:?}");
                break;
            }
        }
    }
    assert_eq!(s.replica().digest(), p.replica.digest());
}
