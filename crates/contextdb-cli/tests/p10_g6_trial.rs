//! P10 G6, the bare-remote trial (owner decision 2026-11-17: this trial
//! only; the ≥5-day real-remote part is skipped). Two clones of one bare
//! remote on this machine, two members with their own SSH keys (alice, an
//! admin; bob, a writer), two `ctxd` with the default `lag_check`, real
//! hooks. Scripted agents on both edges, every round:
//!
//! - propose team records, supersede existing ones (through the mirrors,
//!   as an agent writes), and sometimes retract a base — which bob may not
//!   (curators are the admins), so his are refused at enqueue;
//! - commit code and push their own branch: `pre-push` syncs and publishes;
//! - the other edge fetches; its hooks alone import.
//!
//! Checked every round, as G6 asks: equal team digests; every changed
//! team record invalidates the other edge's probe (a local `Cur`
//! dependent) within that fetch; no push blocked; no `ReplicationLag`
//! once the fetch's import is done. At the end, zero lost proposals (each
//! accepted, rejected, conflicted or dropped).
//!
//! Run: `CONTEXTDB_G6_ROUNDS=60 CONTEXTDB_G6_REPORT=<file> cargo test -p
//! contextdb-cli --test p10_g6_trial -- --ignored --nocapture`.

use contextdb_daemon::server::{Daemon, Options};
use contextdb_daemon::service::Identity;
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn ctx(args: &[&str]) -> (i32, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let o = Command::new("git")
        .current_dir(dir)
        .args(args)
        .envs([
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@t"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@t"),
        ])
        .output()
        .unwrap();
    (
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr),
    )
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let (s, out) = git(dir, args);
    assert!(s, "git {args:?}: {out}");
    out
}

const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId(1),
};

struct Rng(u64);
impl Rng {
    fn n(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n.max(1)
    }
}

/// One edge: its clone, data directory, daemon, session and probes.
struct Edge {
    name: &'static str,
    repo: PathBuf,
    data: PathBuf,
    d: Daemon,
    sid: SessionId,
    /// Team key → (the mirror head the probe depends on, probe key).
    probes: BTreeMap<String, (RevisionId, String)>,
    next: u32,
}

impl Edge {
    fn handle(&self, r: Request) -> Response {
        self.d.service.handle(AGENT, r).response
    }
    fn commit(&self, ops: Vec<Op>, blobs: Vec<Vec<u8>>) -> Response {
        self.handle(Request::Commit {
            binding: Binding::root(self.sid),
            ops,
            blobs,
            expect_cert_valid: false,
            client_txn_id: None,
        })
    }
    fn status(&self) -> String {
        ctx(&["team", "status", self.data.to_str().unwrap()]).1
    }
    fn field(&self, out: &str, f: &str) -> String {
        out.split(&format!("{f} "))
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("")
            .to_owned()
    }
    /// The team's mirrors on main: key → head revision.
    fn mirrors(&self, team: &str) -> BTreeMap<String, RevisionId> {
        let snap = self.d.service.actor.snapshot();
        let v = snap.view(MAIN);
        let prefix = format!("team/{team}/");
        v.logicals()
            .filter_map(|l| {
                let (k, ..) = v.logical_info(l)?;
                k.starts_with(&prefix).then(|| (k, v.head(l).unwrap()))
            })
            .collect()
    }
    fn summary(&self, key: &str) -> Option<Summary> {
        let l = self
            .d
            .service
            .actor
            .snapshot()
            .view(MAIN)
            .logical_by_key(key)?;
        match self.handle(Request::Read {
            binding: Binding::root(self.sid),
            target: Target::Logical(l),
            mode: Mode::Advisory,
            level: None,
        }) {
            Response::Read { item, .. } => Some(item.summary),
            other => panic!("{other:?}"),
        }
    }
    /// A probe for every mirror whose head it does not track yet.
    fn refresh_probes(&mut self, team: &str) {
        for (k, head) in self.mirrors(team) {
            if self.probes.get(&k).is_some_and(|(h, _)| *h == head) {
                continue;
            }
            self.next += 1;
            let probe = format!("probe:{}:{}", self.name, self.next);
            let body = probe.clone().into_bytes();
            let r = self.commit(
                vec![
                    Op::CreateLogical {
                        origin: Origin::Local,
                        key: probe.clone(),
                        kind: RecordKind::Decision,
                        scope_lane: "repo".into(),
                        content: *blake3::hash(&body).as_bytes(),
                        adopt_as_base: false,
                    },
                    Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: vec![(head.into(), EdgeKind::Cur)],
                        enabled: true,
                    },
                ],
                vec![body],
            );
            assert!(matches!(r, Response::Committed { .. }), "{r:?}");
            self.probes.insert(k, (head, probe));
        }
    }
}

#[derive(Default, Debug)]
struct Report {
    rounds: u64,
    proposals: u64,
    refused_at_enqueue: u64,
    pushes: u64,
    pushes_blocked: u64,
    entries: u64,
    digest_checks: u64,
    digest_mismatches: u64,
    probe_checks: u64,
    probe_misses: u64,
    lag_checks: u64,
    lag_persisting: u64,
    max_import_ms: u128,
    final_states: BTreeMap<String, u64>,
}

#[test]
#[ignore = "the G6 bare-remote trial; run explicitly"]
fn p10_g6_bare_remote_trial() {
    let rounds: u64 = std::env::var("CONTEXTDB_G6_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let tmp = std::env::temp_dir().join(format!("p10-g6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let remote = tmp.join("remote.git");
    ok(&tmp, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    let url = format!("file://{}", remote.display());
    let keygen = |name: &str| {
        let k = tmp.join(name);
        assert!(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f"])
                .arg(&k)
                .status()
                .unwrap()
                .success()
        );
        k
    };
    let (alice, bob) = (keygen("alice"), keygen("bob"));
    let mut edges = Vec::new();
    for name in ["a", "b"] {
        let repo = tmp.join(name);
        ok(&tmp, &["clone", "-q", &url, repo.to_str().unwrap()]);
        let data = tmp.join(format!("data-{name}"));
        let d = Daemon::start(&data, &Options::default()).unwrap();
        edges.push((repo, data, d));
    }
    // alice creates the team and adds bob; bob joins.
    let (ra, da_dir) = (&edges[0].0, &edges[0].1);
    let (code, out) = ctx(&[
        "team",
        "create",
        da_dir.to_str().unwrap(),
        "--repo",
        ra.to_str().unwrap(),
        "--key",
        alice.to_str().unwrap(),
        "--member",
        "alice",
    ]);
    assert_eq!(code, 0, "{out}");
    let genesis = out
        .split("genesis ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let team = std::fs::read_dir(da_dir.join("teams"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let pubkey = |k: &Path| {
        std::fs::read_to_string(k.with_extension("pub"))
            .unwrap()
            .trim()
            .to_owned()
    };
    let doc = format!(
        "version = 1\naccept_version = 1\nid = \"{team}\"\nadmins = [\"alice\"]\n\
         [members.alice]\nkeys = [\"{}\"]\n[members.bob]\nkeys = [\"{}\"]\n\
         [lanes.main]\nwriters = [\"*members\"]\ncurators = [\"*admins\"]\n",
        pubkey(&alice),
        pubkey(&bob)
    );
    std::fs::write(tmp.join("team.toml"), doc).unwrap();
    let f = tmp.join("team.toml");
    assert_eq!(
        ctx(&[
            "team",
            "access",
            da_dir.to_str().unwrap(),
            "--file",
            f.to_str().unwrap()
        ])
        .0,
        0
    );
    assert_eq!(ctx(&["team", "publish", da_dir.to_str().unwrap()]).0, 0);
    let (rb, db_dir) = (&edges[1].0, &edges[1].1);
    let (code, out) = ctx(&[
        "team",
        "join",
        db_dir.to_str().unwrap(),
        "--repo",
        rb.to_str().unwrap(),
        "--genesis",
        &genesis,
        "--key",
        bob.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{out}");
    let mut edges: Vec<Edge> = edges
        .into_iter()
        .zip(["a", "b"])
        .map(|((repo, data, d), name)| {
            let sid = match d
                .service
                .handle(
                    AGENT,
                    Request::SessionStart {
                        harness: "g6".into(),
                        repo: "r".into(),
                        head_sha: "h".into(),
                        parent: None,
                        branch: None,
                        git_branch: None,
                    },
                )
                .response
            {
                Response::Session { session } => session,
                other => panic!("{other:?}"),
            };
            Edge {
                name,
                repo,
                data,
                d,
                sid,
                probes: BTreeMap::new(),
                next: 0,
            }
        })
        .collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut rep = Report::default();
    let mut fresh = 0u32;
    for round in 0..rounds {
        rep.rounds += 1;
        // Agents on both edges.
        for e in edges.iter_mut() {
            e.refresh_probes(&team);
            for _ in 0..1 + rng.n(2) {
                let mirrors = e.mirrors(&team);
                fresh += 1;
                let body = format!("{} round {round} #{fresh}", e.name).into_bytes();
                let c = *blake3::hash(&body).as_bytes();
                let pick = (!mirrors.is_empty()).then(|| {
                    let i = rng.n(mirrors.len() as u64) as usize;
                    mirrors.iter().nth(i).map(|(k, h)| (k.clone(), *h)).unwrap()
                });
                let (ops, blobs) = match (rng.n(10), pick) {
                    (0..=4, _) | (_, None) => (
                        vec![Op::CreateLogical {
                            origin: Origin::Local,
                            key: format!("team/{team}/k/{fresh}"),
                            kind: RecordKind::Decision,
                            scope_lane: format!("team:{team}/main"),
                            content: c,
                            adopt_as_base: e.name == "a",
                        }],
                        vec![body],
                    ),
                    (5..=8, Some((k, h))) => (
                        vec![
                            Op::AddRevision {
                                logical: LogicalRef::Key(k.clone()),
                                content: c,
                                adopt_as_base: e.name == "a",
                            },
                            Op::SetHead {
                                logical: LogicalRef::Key(k),
                                expected: RevRef::Existing(h),
                                new: RevRef::New(0),
                            },
                        ],
                        vec![body],
                    ),
                    (_, Some((_, h))) => (
                        vec![Op::RetractBase {
                            rev: RevRef::Existing(h),
                        }],
                        vec![],
                    ),
                };
                match e.commit(ops, blobs) {
                    Response::Proposed { .. } => rep.proposals += 1,
                    Response::Error { .. } => rep.refused_at_enqueue += 1,
                    other => panic!("{other:?}"),
                }
            }
        }
        // Code pushes: pre-push syncs and publishes.
        let seq_before = edges[0].field(&edges[0].status(), "seq");
        for e in &edges {
            std::fs::write(e.repo.join(format!("{}.txt", e.name)), format!("{round}")).unwrap();
            ok(&e.repo, &["add", "."]);
            ok(
                &e.repo,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    &format!("r{round}"),
                ],
            );
            rep.pushes += 1;
            let (pushed, out) = git(
                &e.repo,
                &[
                    "push",
                    "-q",
                    "origin",
                    &format!("HEAD:refs/heads/edge-{}", e.name),
                ],
            );
            if !pushed {
                rep.pushes_blocked += 1;
                eprintln!("round {round}: push from {} blocked: {out}", e.name);
            }
        }
        // Fetches: the hooks import; reads must be current within it.
        let t = Instant::now();
        for e in &edges {
            ok(&e.repo, &["fetch", "-q", "origin"]);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let (sa, sb) = loop {
            let (sa, sb) = (edges[0].status(), edges[1].status());
            let tip = ok(
                &edges[0].repo,
                &[
                    "ls-remote",
                    "origin",
                    &format!("refs/contextdb/team/{team}/*"),
                ],
            );
            let caught = |s: &str, e: &Edge| {
                e.field(s, "seq") == edges[0].field(&sa, "seq") && !s.contains("Stalled")
            };
            if caught(&sb, &edges[1])
                && !tip.is_empty()
                && edges[0].field(&sa, "digest") == edges[1].field(&sb, "digest")
            {
                break (sa, sb);
            }
            if Instant::now() > deadline {
                break (sa, sb);
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        rep.max_import_ms = rep.max_import_ms.max(t.elapsed().as_millis());
        let seq_after = edges[0].field(&sa, "seq");
        if seq_after != seq_before {
            rep.entries +=
                seq_after.parse::<u64>().unwrap_or(0) - seq_before.parse::<u64>().unwrap_or(0);
        }
        rep.digest_checks += 1;
        if edges[0].field(&sa, "digest") != edges[1].field(&sb, "digest") {
            rep.digest_mismatches += 1;
            eprintln!("round {round}: digests differ\n{sa}\n{sb}");
        }
        // Probes: every record whose head moved since its probe was made
        // has a STALE probe on that edge now, within this fetch.
        for e in &edges {
            let mirrors = e.mirrors(&team);
            for (k, (head, probe)) in &e.probes {
                let Some(now) = mirrors.get(k) else { continue };
                let s = e.summary(probe).unwrap();
                if now != head {
                    rep.probe_checks += 1;
                    if !matches!(s, Summary::Stale(_)) {
                        rep.probe_misses += 1;
                        eprintln!(
                            "round {round}: {} probe of {k} is {s:?} after a head move",
                            e.name
                        );
                    }
                }
                rep.lag_checks += 1;
                if s == Summary::Pending(PendingReason::ReplicationLag) {
                    rep.lag_persisting += 1;
                    eprintln!("round {round}: {} still lags on {k}", e.name);
                }
            }
        }
    }
    // Zero lost proposals: every one terminal after a last publish.
    for e in &edges {
        ctx(&["team", "publish", e.data.to_str().unwrap()]);
    }
    for e in &edges {
        ok(&e.repo, &["fetch", "-q", "origin"]);
        ctx(&["team", "sync", e.data.to_str().unwrap()]);
        for l in e
            .status()
            .lines()
            .filter(|l| l.trim_start().starts_with("proposal"))
        {
            let state = l
                .split_whitespace()
                .nth(4)
                .unwrap_or("?")
                .split(['{', ' '])
                .next()
                .unwrap()
                .to_owned();
            *rep.final_states.entry(state).or_default() += 1;
        }
    }
    let lost: u64 = rep
        .final_states
        .iter()
        .filter(|(s, _)| !["Accepted", "Rejected", "Conflicted", "Dropped"].contains(&s.as_str()))
        .map(|(_, n)| n)
        .sum();
    let json = format!(
        "{{\"rounds\":{},\"proposals\":{},\"refused_at_enqueue\":{},\"entries\":{},\"pushes\":{},\"pushes_blocked\":{},\
         \"digest_checks\":{},\"digest_mismatches\":{},\"probe_checks\":{},\"probe_misses\":{},\
         \"lag_checks\":{},\"lag_persisting\":{},\"max_fetch_to_import_ms\":{},\"final_states\":{:?},\"lost\":{}}}",
        rep.rounds,
        rep.proposals,
        rep.refused_at_enqueue,
        rep.entries,
        rep.pushes,
        rep.pushes_blocked,
        rep.digest_checks,
        rep.digest_mismatches,
        rep.probe_checks,
        rep.probe_misses,
        rep.lag_checks,
        rep.lag_persisting,
        rep.max_import_ms,
        rep.final_states,
        lost
    );
    eprintln!("G6 trial: {json}");
    if let Ok(p) = std::env::var("CONTEXTDB_G6_REPORT") {
        std::fs::write(p, &json).unwrap();
    }
    assert_eq!(rep.pushes_blocked, 0);
    assert_eq!(rep.digest_mismatches, 0);
    assert_eq!(rep.probe_misses, 0);
    assert_eq!(rep.lag_persisting, 0);
    assert_eq!(lost, 0, "{:?}", rep.final_states);
    let _ = BTreeSet::<u8>::new();
    drop(edges);
    let _ = std::fs::remove_dir_all(&tmp);
}
