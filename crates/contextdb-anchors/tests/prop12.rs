//! PROP12 (SPEC §8, ADR 0011 §7 G3): observer output == anchor state rebuilt
//! from Git history. 10,000 fixed seeds each generate a multi-file history
//! (edits, deletions, identical restores, old-head ABA, late registrations,
//! unsupported paths) with injected CAS races and capture failures.
//!
//! - Git (fast-import, then `cat-file`) independently records every manifest
//!   and commit id; nothing on the oracle side calls the classifier, and the
//!   descriptor and position bytes are re-encoded here from the ADR §2/§4 text.
//! - The system under test is the production classifier (`prepare`) and the
//!   kernel commit, with the observer's reload-and-recapture CAS retry.
//! - Coverage counts transitions that actually happened between successful
//!   scans (or injected faults that actually took effect), never branch labels.
use contextdb_anchors::{
    AnchorState, Descriptor, ObjectFormat, Observation,
    capture::{blob_oid, fingerprint, git},
    prepare, validate_path,
};
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const SEEDS: u64 = 10_000;
const STEPS: usize = 10;
/// Supported paths, nested and flat; each seed uses a prefix of 2–4.
const PATHS: [&str; 4] = ["a.rs", "src/b.rs", "src/c/d.rs", "e.txt"];
/// Registration attempts that the path domain must reject (ADR 0011 §2).
const UNSUPPORTED: [&str; 6] = [
    "A.rs",
    "src/../a.rs",
    ".git/config",
    "src//x.rs",
    "/abs.rs",
    "caf\u{e9}.rs",
];
/// Few contents per path, so identical restores and ABA are frequent.
fn bytes(path: &str, v: u8) -> Vec<u8> {
    format!("{path} v{v}\r\n\0raw").into_bytes()
}

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, permille: u64) -> bool {
        self.below(1000) < permille
    }
}

#[derive(Clone)]
struct Step {
    /// The worktree after this step (path -> content version).
    world: BTreeMap<&'static str, Option<u8>>,
    /// Paths registered by this scan (only present paths, only if available).
    register: Vec<&'static str>,
    unsupported: Option<&'static str>,
    unavailable: bool,
    race: bool,
}
fn plan(seed: u64) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let active = &PATHS[..2 + rng.below(3) as usize];
    let mut world: BTreeMap<&str, Option<u8>> = active
        .iter()
        .map(|p| (*p, rng.chance(800).then(|| rng.below(3) as u8)))
        .collect();
    if world.values().all(Option::is_none) {
        world.insert(active[0], Some(0));
    }
    let mut registered = BTreeSet::new();
    let mut steps = Vec::new();
    for step in 0..STEPS {
        if step > 0 {
            for p in active {
                let v = world.get_mut(p).unwrap();
                *v = match *v {
                    Some(_) if rng.chance(150) => None,
                    Some(_) if rng.chance(450) => Some(rng.below(3) as u8),
                    None if rng.chance(550) => Some(rng.below(3) as u8),
                    keep => keep,
                };
            }
        }
        let unavailable = step > 0 && rng.chance(80);
        let mut register = Vec::new();
        if !unavailable {
            for p in active {
                let wanted = step == 0 || rng.chance(120);
                if wanted && world[p].is_some() && registered.insert(*p) {
                    register.push(*p);
                }
            }
        }
        steps.push(Step {
            world: world.clone(),
            register,
            unsupported: rng
                .chance(60)
                .then(|| UNSUPPORTED[rng.below(UNSUPPORTED.len() as u64) as usize]),
            unavailable,
            race: !unavailable && step > 0 && rng.chance(70),
        });
    }
    steps
}
fn format_of(seed: u64) -> ObjectFormat {
    if seed.is_multiple_of(50) {
        ObjectFormat::Sha256
    } else {
        ObjectFormat::Sha1
    }
}

/// Raw tree objects: id -> (mode, name, oid) entries.
type Trees = BTreeMap<String, Vec<(String, String, Vec<u8>)>>;
/// Git's record of every (seed, step): commit id and path -> blob OID.
type Manifest = BTreeMap<String, Vec<u8>>;
struct GitHistories {
    roots: Vec<std::path::PathBuf>,
    commits: BTreeMap<(u64, usize), (Vec<u8>, Manifest)>,
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
impl GitHistories {
    fn new(plans: &BTreeMap<u64, Vec<Step>>) -> Self {
        let mut out = Self {
            roots: Vec::new(),
            commits: BTreeMap::new(),
        };
        for (format, oid_len) in [("sha1", 20), ("sha256", 32)] {
            let root = std::env::temp_dir()
                .join(format!("p5a-prop12-git-{}-{format}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let deadline = Instant::now() + Duration::from_secs(600);
            let init = format!("--object-format={format}");
            assert!(
                git(&root, &["init", "-q", &init], &[], 1 << 16, deadline)
                    .unwrap()
                    .0
            );
            let mut input = Vec::new();
            let mut mark = 0;
            for (seed, steps) in plans {
                if (format_of(*seed) == ObjectFormat::Sha256) != (format == "sha256") {
                    continue;
                }
                for (step, s) in steps.iter().enumerate() {
                    mark += 1;
                    let message = format!("seed {seed} step {step}\n");
                    input.extend_from_slice(
                        format!(
                            "commit refs/heads/main\ncommitter T <t@example.invalid> {mark} +0000\ndata {}\n{message}deleteall\n",
                            message.len()
                        )
                        .as_bytes(),
                    );
                    for (path, v) in &s.world {
                        if let Some(v) = v {
                            let body = bytes(path, *v);
                            input.extend_from_slice(
                                format!("M 100644 inline {path}\ndata {}\n", body.len()).as_bytes(),
                            );
                            input.extend_from_slice(&body);
                            input.push(b'\n');
                        }
                    }
                    input.push(b'\n');
                }
            }
            assert!(
                git(
                    &root,
                    &["fast-import", "--quiet"],
                    &input,
                    1 << 20,
                    deadline
                )
                .unwrap()
                .0
            );
            let (ok, objects) = git(
                &root,
                &["cat-file", "--batch-all-objects", "--batch"],
                &[],
                1 << 30,
                deadline,
            )
            .unwrap();
            assert!(ok);
            // Decode raw tree entries ("<mode> <name>\0<oid>") and commits.
            let mut trees: Trees = BTreeMap::new();
            let mut commits = Vec::new();
            let mut cursor = 0;
            while cursor < objects.len() {
                let end = cursor + objects[cursor..].iter().position(|b| *b == b'\n').unwrap();
                let header = std::str::from_utf8(&objects[cursor..end])
                    .unwrap()
                    .to_owned();
                let fields: Vec<_> = header.split(' ').collect();
                let n: usize = fields[2].parse().unwrap();
                let body = &objects[end + 1..end + 1 + n];
                cursor = end + 2 + n;
                match fields[1] {
                    "tree" => {
                        let mut entries = Vec::new();
                        let mut at = 0;
                        while at < body.len() {
                            let nul = at + body[at..].iter().position(|b| *b == 0).unwrap();
                            let (mode, name) = std::str::from_utf8(&body[at..nul])
                                .unwrap()
                                .split_once(' ')
                                .unwrap();
                            let oid = body[nul + 1..nul + 1 + oid_len].to_vec();
                            entries.push((mode.to_owned(), name.to_owned(), oid));
                            at = nul + 1 + oid_len;
                        }
                        trees.insert(fields[0].to_owned(), entries);
                    }
                    "commit" => {
                        let text = std::str::from_utf8(body).unwrap();
                        let tree = text.lines().next().unwrap()["tree ".len()..].to_owned();
                        let message: Vec<_> = text
                            .split_once("\n\n")
                            .unwrap()
                            .1
                            .split_whitespace()
                            .collect();
                        commits.push((
                            (message[1].parse().unwrap(), message[3].parse().unwrap()),
                            unhex(fields[0]),
                            tree,
                        ));
                    }
                    "blob" => {}
                    other => panic!("unexpected {other}"),
                }
            }
            fn walk(trees: &Trees, tree: &str, prefix: &str, out: &mut Manifest) {
                for (mode, name, oid) in &trees[tree] {
                    let path = format!("{prefix}{name}");
                    match mode.as_str() {
                        "40000" => walk(trees, &hex(oid), &format!("{path}/"), out),
                        "100644" => {
                            out.insert(path, oid.clone());
                        }
                        other => panic!("mode {other}"),
                    }
                }
            }
            for (key, commit, tree) in commits {
                let mut manifest = Manifest::new();
                walk(&trees, &tree, "", &mut manifest);
                out.commits.insert(key, (commit, manifest));
            }
            out.roots.push(root);
        }
        assert_eq!(out.commits.len(), plans.len() * STEPS);
        out
    }
}
impl Drop for GitHistories {
    fn drop(&mut self) {
        for root in &self.roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// ADR 0011 §2 descriptor and §4 position, re-encoded from the text.
fn wire(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}
fn descriptor_bytes(repo: &str, path: &str, format: ObjectFormat, oid: &[u8]) -> Vec<u8> {
    let mut out = b"CTXANC01".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    wire(&mut out, repo.as_bytes());
    wire(&mut out, path.as_bytes());
    out.push(match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    });
    wire(&mut out, oid);
    out
}
fn position(repo: &str, head: &[u8], entries: &[(String, u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = b"CTXPOS01".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    wire(&mut out, repo.as_bytes());
    wire(&mut out, head);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (path, tag, value) in entries {
        wire(&mut out, path.as_bytes());
        out.push(*tag);
        wire(&mut out, value);
    }
    blake3::hash(&out).as_bytes().to_vec()
}

/// What Git history alone implies for one registered path.
#[derive(Default)]
struct Expected {
    /// Distinct OIDs in order of first observation (one revision each).
    seen: Vec<Vec<u8>>,
    head: Vec<u8>,
    base: BTreeMap<Vec<u8>, bool>,
    present: bool,
    breaks_opened: usize,
}

/// System under test: kernel state plus the descriptors it committed.
struct Sut {
    db: Db,
    history: Vec<Inputs>,
    descriptors: BTreeMap<ContentHash, Descriptor>,
}
impl Sut {
    fn commit(&mut self, ops: Vec<Op>) -> Result<(), TxError> {
        let mut b = Batch::new(ops);
        b.channel = Channel::Observer;
        self.db.commit(&b).map(|_| ())?;
        self.history.push(self.db.inputs());
        Ok(())
    }
    /// The observer's view of committed anchors (what `AnchorState` serves).
    fn prior(&self, scope: &str) -> BTreeMap<String, AnchorState> {
        let i = self.db.inputs();
        let mut out = BTreeMap::new();
        for (l, logical) in i.logicals.iter().enumerate() {
            if logical.kind != RecordKind::CodeAnchor || logical.scope_lane != scope {
                continue;
            }
            let head = i.heads[l];
            let content = i.revisions[head.index()].content;
            let d = &self.descriptors[&content];
            out.insert(
                d.path.clone(),
                AnchorState {
                    logical: LogicalId(l as u32),
                    head,
                    content,
                    oid: d.oid.clone(),
                    base: i.base[head.index()],
                    breaks: i
                        .artifacts
                        .iter()
                        .enumerate()
                        .filter(|(_, a)| {
                            a.open
                                && a.ty == ArtifactType::AnchorBroken
                                && i.revisions[a.target.index()].logical.index() == l
                        })
                        .map(|(k, _)| ArtifactId(k as u32))
                        .collect(),
                },
            );
        }
        out
    }
}

#[derive(Default)]
struct Coverage {
    transactions: usize,
    delete: usize,
    same_restore: usize,
    old_head_aba: usize,
    cas_retry: usize,
    lag: usize,
    unavailable: usize,
    unsupported: usize,
    sha256: usize,
}

fn run_seed(seed: u64, steps: &[Step], git: &GitHistories, cov: &mut Coverage) {
    let format = format_of(seed);
    let repo = format!("{seed:032x}");
    let scope = format!("code:{repo}");
    let mut sut = Sut {
        db: Db::default(),
        history: vec![Inputs::default()],
        descriptors: BTreeMap::new(),
    };
    let mut registered: BTreeSet<&str> = BTreeSet::new();
    let mut expected: BTreeMap<&str, Expected> = BTreeMap::new();
    let mut last_scan: Option<usize> = None;
    let mut race = 0u8;
    for (k, step) in steps.iter().enumerate() {
        cov.transactions += 1;
        if format == ObjectFormat::Sha256 {
            cov.sha256 += 1;
        }
        let (commit, manifest) = &git.commits[&(seed, k)];
        // Git's blob OIDs must equal the observer's raw hash of the bytes.
        for (path, v) in &step.world {
            assert_eq!(
                manifest.get(*path),
                v.map(|v| blob_oid(format, &bytes(path, v)).unwrap())
                    .as_ref(),
                "seed {seed} step {k} {path}"
            );
        }
        if let Some(bad) = step.unsupported {
            let before = sut.db.inputs();
            assert!(validate_path(&repo, bad).is_err(), "{bad:?} accepted");
            let obs = BTreeMap::from([(bad.to_owned(), Observation::Missing)]);
            assert!(prepare(&repo, &obs, &sut.prior(&scope), None, vec![0]).is_err());
            assert_eq!(before, sut.db.inputs());
            cov.unsupported += 1;
        }
        // The observer's capture of every registered (and newly added) path.
        let paths: BTreeSet<&str> = registered
            .iter()
            .copied()
            .chain(step.register.iter().copied())
            .collect();
        let observations: BTreeMap<String, Observation> = paths
            .iter()
            .map(|p| {
                let obs = match step.world[p] {
                    Some(v) => Observation::Present(Descriptor {
                        repo: repo.clone(),
                        path: (*p).to_owned(),
                        format,
                        oid: blob_oid(format, &bytes(p, v)).unwrap(),
                    }),
                    None => Observation::Missing,
                };
                ((*p).to_owned(), obs)
            })
            .collect();
        let live = fingerprint(&repo, commit, &observations);
        if step.unavailable {
            // Capture failed: nothing is committed. Freshness against the live
            // worktree is Unknown whenever it moved past the last watermark.
            cov.unavailable += 1;
            if let Some(last) = last_scan {
                let moved = registered
                    .iter()
                    .any(|p| step.world[p] != steps[last].world[p])
                    || git.commits[&(seed, last)].0 != *commit;
                // Present anchors only: a deleted one is STALE(Unsupported)
                // and cannot be certified at all.
                let entries: Vec<_> = sut
                    .prior(&scope)
                    .values()
                    .filter(|a| a.base)
                    .map(|a| CertItem {
                        logical: a.logical,
                        rev: a.head,
                        required: true,
                        via: Via::Head,
                        since_epoch: sut.db.epoch(),
                    })
                    .collect();
                if entries.is_empty() {
                    continue;
                }
                let fresh = sut.db.inputs().watermarks;
                let request = IssueRequest {
                    session_id: [1; 16],
                    principal: PrincipalId(0),
                    branch: MAIN,
                    log_checksum: [0; 32],
                    code_commit: String::new(),
                    strict_no_aba: seed.is_multiple_of(2),
                };
                let cert = sut
                    .db
                    .issue_certificate(&request, &entries, &fresh)
                    .unwrap();
                let live_map = BTreeMap::from([(scope.clone(), live.clone())]);
                let link = LogLink {
                    matches: true,
                    current_checksum: [0; 32],
                };
                let actual = sut.db.validate_certificate(&cert, &live_map, link).unwrap();
                assert_eq!(
                    actual,
                    contextdb_oracle::cert_validate_oracle(
                        &sut.history,
                        &cert,
                        &live_map,
                        true,
                        [0; 32]
                    )
                );
                assert_eq!(
                    moved,
                    matches!(actual, Validation::Unknown { .. }),
                    "seed {seed} step {k}"
                );
                if moved {
                    cov.lag += 1;
                }
            }
            continue;
        }
        let mut prepared = prepare(
            &repo,
            &observations,
            &sut.prior(&scope),
            sut.db.watermark(&scope).map(<[u8]>::to_vec),
            live.clone(),
        )
        .unwrap();
        if step.race {
            // Another observer publishes first; the captured batch must fail
            // atomically, then the scan reloads state and recaptures.
            race += 1;
            sut.commit(vec![Op::ObserverWatermark {
                scope: scope.clone(),
                expected: sut.db.watermark(&scope).map(<[u8]>::to_vec),
                position: vec![race],
            }])
            .unwrap();
            let before = sut.db.inputs();
            assert!(
                sut.commit(prepared.ops.clone()).is_err(),
                "stale batch committed"
            );
            assert_eq!(before, sut.db.inputs(), "failed CAS is atomic");
            cov.cas_retry += 1;
            prepared = prepare(
                &repo,
                &observations,
                &sut.prior(&scope),
                sut.db.watermark(&scope).map(<[u8]>::to_vec),
                live.clone(),
            )
            .unwrap();
        }
        // Descriptors (break payloads are the other blobs).
        for blob in &prepared.blobs {
            if let Ok(d) = Descriptor::from_bytes(blob) {
                sut.descriptors.insert(d.content(), d);
            }
        }
        if !prepared.ops.is_empty() {
            sut.commit(prepared.ops).unwrap();
        }
        assert_eq!(
            sut.db.derived(),
            &contextdb_oracle::jacobi(sut.history.last().unwrap())
        );

        // Oracle: advance each registered path from Git's manifest alone.
        for p in &step.register {
            registered.insert(p);
            expected.insert(p, Expected::default());
        }
        for p in &registered {
            let e = expected.get_mut(p).unwrap();
            let fresh = step.register.contains(p);
            match manifest.get(*p) {
                Some(oid) => {
                    if !fresh && !e.present && *oid == e.head {
                        cov.same_restore += 1;
                    }
                    if !fresh && *oid != e.head && e.seen.contains(oid) {
                        cov.old_head_aba += 1;
                    }
                    if !e.seen.contains(oid) {
                        e.seen.push(oid.clone());
                    }
                    e.head = oid.clone();
                    e.base.insert(oid.clone(), true);
                    e.present = true;
                }
                None => {
                    assert!(!fresh, "registration of a missing path");
                    if e.present {
                        cov.delete += 1;
                        e.breaks_opened += 1;
                        e.base.insert(e.head.clone(), false);
                    }
                    e.present = false;
                }
            }
        }
        last_scan = Some(k);

        // Compare the committed state with the reconstruction.
        let i = sut.db.inputs();
        let entries: Vec<(String, u8, Vec<u8>)> = registered
            .iter()
            .map(|p| match manifest.get(*p) {
                Some(oid) => ((*p).to_owned(), 1, descriptor_bytes(&repo, p, format, oid)),
                None => ((*p).to_owned(), 2, Vec::new()),
            })
            .collect();
        assert_eq!(
            sut.db.watermark(&scope),
            Some(&position(&repo, commit, &entries)[..]),
            "seed {seed} step {k}: watermark"
        );
        let anchors: Vec<_> = i
            .logicals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind == RecordKind::CodeAnchor)
            .collect();
        assert_eq!(anchors.len(), registered.len(), "seed {seed} step {k}");
        for (l, logical) in anchors {
            let path = logical.key.strip_prefix(&format!("{scope}/")).unwrap();
            let e = &expected[path];
            let hash = |oid: &Vec<u8>| {
                *blake3::hash(&descriptor_bytes(&repo, path, format, oid)).as_bytes()
            };
            let revisions: Vec<RevisionId> = (0..i.revisions.len())
                .filter(|r| i.revisions[*r].logical.index() == l)
                .map(|r| RevisionId(r as u32))
                .collect();
            let contents: Vec<ContentHash> = revisions
                .iter()
                .map(|r| i.revisions[r.index()].content)
                .collect();
            let want: Vec<ContentHash> = e.seen.iter().map(hash).collect();
            assert_eq!(contents, want, "seed {seed} step {k} {path}: revisions");
            let head = i.heads[l];
            assert_eq!(
                i.revisions[head.index()].content,
                hash(&e.head),
                "seed {seed} step {k} {path}: head"
            );
            for r in &revisions {
                let oid = &e.seen[contents
                    .iter()
                    .position(|c| *c == i.revisions[r.index()].content)
                    .unwrap()];
                assert_eq!(
                    i.base[r.index()],
                    e.base[oid],
                    "seed {seed} step {k} {path}: base"
                );
            }
            let breaks: Vec<_> = i
                .artifacts
                .iter()
                .filter(|a| {
                    a.ty == ArtifactType::AnchorBroken
                        && i.revisions[a.target.index()].logical.index() == l
                })
                .collect();
            assert_eq!(
                breaks.len(),
                e.breaks_opened,
                "seed {seed} step {k} {path}: breaks"
            );
            assert_eq!(
                breaks.iter().filter(|a| a.open).count(),
                usize::from(!e.present),
                "seed {seed} step {k} {path}: open breaks"
            );
        }
    }
}

#[test]
fn prop12_10000_fixed_seeds_observer_matches_git_history_reconstruction() {
    let plans: BTreeMap<u64, Vec<Step>> = (0..SEEDS).map(|s| (s, plan(s))).collect();
    let git = GitHistories::new(&plans);
    let mut cov = Coverage::default();
    for (seed, steps) in &plans {
        run_seed(*seed, steps, &git, &mut cov);
    }
    let t = cov.transactions;
    println!(
        "PROP12 seeds={SEEDS} transactions={t} delete={} same_restore={} old_head_aba={} cas_retry={} lag={} unavailable={} unsupported={} sha256={}",
        cov.delete,
        cov.same_restore,
        cov.old_head_aba,
        cov.cas_retry,
        cov.lag,
        cov.unavailable,
        cov.unsupported,
        cov.sha256
    );
    // Floors fixed by ADR 0011 §7 before the run, as % of transactions.
    for (name, count, floor) in [
        ("delete", cov.delete, 5),
        ("same_restore", cov.same_restore, 5),
        ("old_head_aba", cov.old_head_aba, 5),
        ("cas_retry", cov.cas_retry, 3),
        ("lag", cov.lag, 3),
        ("unavailable", cov.unavailable, 2),
        ("unsupported", cov.unsupported, 2),
        ("sha256", cov.sha256, 1),
    ] {
        assert!(
            count * 100 >= t * floor,
            "coverage {name}: {count} / {t} < {floor}%"
        );
    }
}
