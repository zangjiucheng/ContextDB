//! ADR 0012 §9 G3: two-oracle PROP12 over mixed file/symbol repositories.
//! Run at the gate count: cargo test --release -p contextdb-symbols --test
//! prop12_symbols -- --nocapture. The optional seed override is smoke-only.
#[path = "common/generated.rs"]
mod generated;
#[path = "common/history.rs"]
mod history;
#[path = "common/spec.rs"]
mod spec;

use contextdb_anchors::{
    AnchorDescriptor, AnchorState, Descriptor, ObjectFormat, Observation,
    capture::{blob_oid, position},
    symbol::{decode_symbol, encode_symbol},
    validate_path,
};
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_symbols::observer::{Added, Mode, prepare_scan, targets};
use generated::{LANGS, Meaning, STEPS, Step};
use history::{GitHistories, format_of};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
fn hash(bytes: &[u8]) -> ContentHash {
    *blake3::hash(bytes).as_bytes()
}
fn file_descriptor(repo: &str, path: &str, format: ObjectFormat, oid: &[u8]) -> Vec<u8> {
    let mut out = b"CTXANC01".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    spec::wire(&mut out, repo.as_bytes());
    spec::wire(&mut out, path.as_bytes());
    out.push(if format == ObjectFormat::Sha1 { 1 } else { 2 });
    spec::wire(&mut out, oid);
    out
}
fn key(repo: &str, path: &str, name: &str) -> String {
    // Independent reversible key encoding from ADR §3, not encode_symbol.
    let encoded: String = name
        .chars()
        .map(|c| match c {
            'A'..='Z' => format!("-{}", c.to_ascii_lowercase()),
            '_' => ".".into(),
            '$' => "-.".into(),
            _ => c.to_string(),
        })
        .collect();
    assert_eq!(encode_symbol(name).unwrap(), encoded);
    assert_eq!(decode_symbol(&encoded).unwrap(), name);
    format!("code:{repo}/{path}#{encoded}")
}

#[derive(Default)]
struct Expected {
    seen: Vec<ContentHash>,
    head: ContentHash,
    base: BTreeMap<ContentHash, bool>,
    live: bool,
    breaks: usize,
}
#[derive(Default)]
struct Sut {
    db: Db,
    history: Vec<Inputs>,
    descriptors: BTreeMap<ContentHash, AnchorDescriptor>,
}
impl Sut {
    fn commit(&mut self, ops: Vec<Op>) -> Result<(), TxError> {
        let mut batch = Batch::new(ops);
        batch.channel = Channel::Observer;
        self.db.commit(&batch)?;
        self.history.push(self.db.inputs());
        Ok(())
    }
    fn prior(&self) -> BTreeMap<String, (AnchorDescriptor, AnchorState)> {
        let i = self.db.inputs();
        i.logicals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind == RecordKind::CodeAnchor)
            .map(|(l, logical)| {
                let head = i.heads[l];
                let content = i.revisions[head.index()].content;
                let descriptor = self.descriptors[&content].clone();
                let oid = match &descriptor {
                    AnchorDescriptor::File(d) => d.oid.clone(),
                    AnchorDescriptor::Symbol(d) => d.masked.clone(),
                };
                (
                    logical.key.clone(),
                    (
                        descriptor,
                        AnchorState {
                            logical: LogicalId(l as u32),
                            head,
                            content,
                            oid,
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
                                .map(|(a, _)| ArtifactId(a as u32))
                                .collect(),
                        },
                    ),
                )
            })
            .collect()
    }
    fn accept(&mut self, prepared: contextdb_anchors::Prepared) {
        for blob in prepared.blobs {
            if let Ok(d) = AnchorDescriptor::from_bytes(&blob) {
                self.descriptors.insert(d.content(), d);
            }
        }
        if !prepared.ops.is_empty() {
            self.commit(prepared.ops).unwrap();
        }
    }
}

#[derive(Default)]
struct Coverage {
    transactions: usize,
    counts: BTreeMap<&'static str, usize>,
    languages: [usize; 6],
}
impl Coverage {
    fn count(&mut self, name: &'static str) {
        *self.counts.entry(name).or_default() += 1;
    }
    fn verify(&self, seeds: u64) {
        println!(
            "PROP12_SYMBOLS seeds={seeds} transactions={} counts={:?} languages={:?}",
            self.transactions, self.counts, self.languages
        );
        for (name, floor) in [
            ("delete", 5),
            ("same_restore", 5),
            ("old_head_aba", 5),
            ("cas_retry", 3),
            ("lag", 3),
            ("unavailable", 2),
            ("unsupported", 2),
            ("sha256", 1),
            ("format", 5),
            ("comment", 5),
            ("body", 5),
            ("outside", 5),
            ("rename", 2),
            ("group", 2),
            ("hash_path", 2),
            ("case_alias", 2),
            ("restore", 3),
            ("syntax", 2),
        ] {
            let count = self.counts.get(name).copied().unwrap_or(0);
            assert!(
                count * 100 >= self.transactions * floor,
                "coverage {name}: {count}/{} < {floor}%",
                self.transactions
            );
        }
        for count in self.languages {
            assert!(
                count * 100 >= self.transactions * 10,
                "language coverage below 10%"
            );
        }
    }
}

fn run(seed: u64, steps: &[Step], git: &GitHistories, cov: &mut Coverage) {
    let repo = format!("{seed:032x}");
    let scope = format!("code:{repo}");
    let format = format_of(seed);
    let mut sut = Sut {
        history: vec![Inputs::default()],
        ..Sut::default()
    };
    let mut expected: BTreeMap<String, Expected> = BTreeMap::new();
    let mut registered: BTreeSet<(String, String)> = BTreeSet::new();
    // Independent model equivalence, checked against the SPEC stream below:
    // meaning equality iff hash equality, including restores to earlier text.
    type Model = Vec<Meaning>;
    let mut meanings: BTreeMap<(usize, Model), Vec<u8>> = BTreeMap::new();
    let mut hashes: BTreeMap<(usize, Vec<u8>), Model> = BTreeMap::new();
    for (k, step) in steps.iter().enumerate() {
        cov.transactions += 1;
        cov.languages[step.target] += 1;
        if format == ObjectFormat::Sha256 {
            cov.count("sha256");
        }
        let (head, manifest) = &git.commits[&(seed, k)];
        let world: BTreeMap<_, _> = manifest
            .iter()
            .map(|(p, oid)| (p.clone(), git.blobs[oid].clone()))
            .collect();
        assert_eq!(
            world,
            step.world(),
            "Git byte readback seed {seed} step {k}"
        );
        let mut entries = BTreeMap::new();
        let mut files = BTreeSet::new();
        for (j, (_, path)) in LANGS.iter().enumerate() {
            if j.is_multiple_of(2) {
                files.insert((*path).to_owned());
            }
            entries.insert(
                (*path).to_owned(),
                match manifest.get(*path) {
                    Some(oid) => {
                        assert_eq!(*oid, blob_oid(format, &world[*path]).unwrap());
                        Observation::Present(Descriptor {
                            repo: repo.clone(),
                            path: (*path).to_owned(),
                            format,
                            oid: oid.clone(),
                        })
                    }
                    None => Observation::Missing,
                },
            );
        }
        // Oracle values, keyed by actual logical keys. No classifier calls.
        let mut fresh: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        for path in &files {
            fresh.insert(
                format!("{scope}/{path}"),
                manifest
                    .get(path)
                    .map(|oid| file_descriptor(&repo, path, format, oid)),
            );
        }
        let mut spec_files = Vec::new();
        for (j, (l, path)) in LANGS.iter().enumerate() {
            let model = step.hosts[j].meanings();
            let parsed = world.get(*path).map(|b| spec::parse(*l, b));
            if let Some(p) = &parsed {
                assert_eq!(
                    p.symbols.keys().collect::<Vec<_>>(),
                    model.keys().collect::<Vec<_>>(),
                    "seed {seed} step {k} {path}: extraction"
                );
                if step.hosts[j].bad {
                    let bad = &step.hosts[j].functions[0].name;
                    assert!(
                        p.has_error && p.symbols[bad].error,
                        "fatal ERROR/MISSING injection not inside {path}#{bad}"
                    );
                } else {
                    assert!(
                        !p.has_error,
                        "unexpected parse error seed {seed} step {k} {path}"
                    );
                }
                for (name, meaning) in &model {
                    registered.insert(((*path).to_owned(), name.clone()));
                    let s = &p.symbols[name];
                    if !s.error {
                        let key = (j, meaning.clone());
                        if let Some(previous) = meanings.insert(key, s.hash.clone()) {
                            assert_eq!(
                                previous, s.hash,
                                "meaning unchanged but hash moved: seed {seed} step {k} {path}#{name}"
                            );
                        }
                        if let Some(previous) = hashes.insert((j, s.hash.clone()), meaning.clone())
                        {
                            assert_eq!(
                                previous, *meaning,
                                "meaning changed but hash stayed: seed {seed} step {k} {path}#{name}"
                            );
                        }
                    }
                }
            }
            spec_files.push(parsed);
        }
        for (path, name) in &registered {
            let j = LANGS.iter().position(|(_, p)| *p == path).unwrap();
            let blob = spec_files[j]
                .as_ref()
                .and_then(|p| p.symbols.get(name))
                .filter(|s| !s.error)
                .map(|s| spec::descriptor(&repo, path, name, LANGS[j].0, s));
            fresh.insert(key(&repo, path, name), blob);
        }
        let added = Added {
            files: files.clone(),
            symbols: registered.clone(),
        };
        let targets = targets(&scope, &sut.prior(), &added, Mode::Scan).unwrap();
        let versions = LANGS
            .iter()
            .map(|(l, _)| (*l, contextdb_symbols::normaliser_version(*l)))
            .collect();
        let hosts_only = LANGS
            .iter()
            .map(|(_, p)| (*p).to_owned())
            .filter(|p| !files.contains(p))
            .collect();
        let live = position(&repo, head, &entries, &hosts_only, &versions);
        // Re-encode CTXPOS02 independently, including per-path deduplication.
        let mut pos = b"CTXPOS02".to_vec();
        pos.extend_from_slice(&1u32.to_le_bytes());
        spec::wire(&mut pos, repo.as_bytes());
        spec::wire(&mut pos, head);
        pos.extend_from_slice(&6u32.to_le_bytes());
        let mut normalisers: Vec<_> = LANGS.into_iter().map(|(l, _)| spec::version(l)).collect();
        normalisers.sort();
        for normaliser in normalisers {
            spec::wire(&mut pos, normaliser.as_bytes());
        }
        pos.extend_from_slice(&6u32.to_le_bytes());
        for (_, path) in LANGS {
            spec::wire(&mut pos, path.as_bytes());
            let (tag, value) = match manifest.get(path) {
                None => (2, Vec::new()),
                Some(oid) if files.contains(path) => (1, file_descriptor(&repo, path, format, oid)),
                Some(oid) => (4, oid.clone()),
            };
            pos.push(tag);
            spec::wire(&mut pos, &value);
        }
        assert_eq!(live, hash(&pos));

        if k == 1 {
            for bad in ["A.rs", "src/../x.rs", "src/a.rs#f"] {
                assert!(validate_path(&repo, bad).is_err());
            }
            cov.count("unsupported");
            cov.count("hash_path");
            let names = &step.hosts[0].functions;
            assert_ne!(
                key(&repo, LANGS[0].1, &names[0].name),
                key(&repo, LANGS[0].1, &names[1].name)
            );
            cov.count("case_alias");
        }
        if k > 0 {
            // The new Git head makes the previous certificate Unknown until
            // this observer's watermark is committed (including no-op symbols).
            let before = sut.db.inputs();
            let items: Vec<_> = sut
                .prior()
                .values()
                .filter(|(_, a)| a.base)
                .map(|(_, a)| CertItem {
                    logical: a.logical,
                    rev: a.head,
                    required: true,
                    via: Via::Head,
                    since_epoch: sut.db.epoch(),
                })
                .collect();
            let cert = sut
                .db
                .issue_certificate(
                    &IssueRequest {
                        session_id: [1; 16],
                        principal: PrincipalId(0),
                        branch: MAIN,
                        log_checksum: [0; 32],
                        code_commit: String::new(),
                        strict_no_aba: false,
                    },
                    &items,
                    &before.watermarks,
                )
                .unwrap();
            let live_map = BTreeMap::from([(scope.clone(), live.clone())]);
            let actual = sut
                .db
                .validate_certificate(
                    &cert,
                    &live_map,
                    LogLink {
                        matches: true,
                        current_checksum: [0; 32],
                    },
                )
                .unwrap();
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
            assert!(matches!(actual, Validation::Unknown { .. }));
            cov.count("lag");
        }
        let prepare = |sut: &Sut, edited: bool| {
            prepare_scan(
                &repo,
                &targets,
                head,
                &entries,
                &world,
                sut.prior().into_iter().map(|(k, (_, s))| (k, s)).collect(),
                sut.db.watermark(&scope).map(<[u8]>::to_vec),
                &mut |_, _| Ok(edited),
                deadline(),
            )
        };
        if step.event == "syntax" {
            let before = sut.db.inputs();
            assert!(prepare(&sut, true).unwrap_err().0.contains("being edited"));
            assert_eq!(before, sut.db.inputs());
            cov.count("unavailable");
        }
        let mut prepared = prepare(&sut, false).unwrap();
        if k == 5 {
            sut.commit(vec![Op::ObserverWatermark {
                scope: scope.clone(),
                expected: sut.db.watermark(&scope).map(<[u8]>::to_vec),
                position: vec![99],
            }])
            .unwrap();
            let before = sut.db.inputs();
            assert!(sut.commit(prepared.ops.clone()).is_err());
            assert_eq!(before, sut.db.inputs());
            // Real retry rebuilds from state and re-parses the capture.
            prepared = prepare(&sut, false).unwrap();
            cov.count("cas_retry");
        }
        sut.accept(prepared);
        assert_eq!(sut.db.watermark(&scope), Some(&hash(&pos)[..]));
        let mut deleted = false;
        let mut restored = false;
        let mut aba = false;
        for (key, blob) in &fresh {
            let e = expected.entry(key.clone()).or_default();
            if let Some(blob) = blob {
                let content = hash(blob);
                restored |= !e.seen.is_empty() && !e.live && e.head == content;
                aba |= !e.seen.is_empty() && e.head != content && e.seen.contains(&content);
                if !e.seen.contains(&content) {
                    e.seen.push(content);
                }
                e.head = content;
                e.base.insert(content, true);
                e.live = true;
            } else if e.live {
                deleted = true;
                e.live = false;
                e.breaks += 1;
                e.base.insert(e.head, false);
            }
        }
        if deleted && step.event == "delete" {
            cov.count("delete");
        }
        if restored {
            cov.count("same_restore");
            cov.count("restore");
        }
        if aba {
            cov.count("old_head_aba");
        }
        // A counted edit must actually change bytes in Git and its model.
        if [
            "format", "comment", "body", "outside", "rename", "group", "syntax",
        ]
        .contains(&step.event)
        {
            assert_ne!(
                world[LANGS[step.target].1],
                steps[k - 1].world()[LANGS[step.target].1]
            );
            let old = steps[k - 1].hosts[step.target].meanings();
            let new = step.hosts[step.target].meanings();
            if matches!(step.event, "format" | "comment" | "syntax") {
                assert_eq!(old, new);
            } else {
                assert_ne!(old, new);
            }
            if step.event == "outside" {
                let first = &step.hosts[step.target].functions[0].name;
                assert_eq!(old[first], new[first]);
            }
            cov.count(step.event);
        }
        let i = sut.db.inputs();
        assert_eq!(sut.db.derived(), &contextdb_oracle::jacobi(&i));
        assert_eq!(i.logicals.len(), expected.len());
        for (l, logical) in i.logicals.iter().enumerate() {
            let e = &expected[&logical.key];
            let revisions: Vec<_> = i
                .revisions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.logical.index() == l)
                .collect();
            assert_eq!(
                revisions.iter().map(|(_, r)| r.content).collect::<Vec<_>>(),
                e.seen,
                "seed {seed} step {k} {}: revisions",
                logical.key
            );
            assert_eq!(i.revisions[i.heads[l].index()].content, e.head);
            for (r, revision) in revisions {
                assert_eq!(i.base[r], e.base[&revision.content]);
            }
            let breaks: Vec<_> = i
                .artifacts
                .iter()
                .filter(|a| {
                    a.ty == ArtifactType::AnchorBroken
                        && i.revisions[a.target.index()].logical.index() == l
                })
                .collect();
            assert_eq!(breaks.len(), e.breaks);
            assert_eq!(
                breaks.iter().filter(|a| a.open).count(),
                usize::from(!e.live)
            );
        }
    }
}

#[test]
fn prop12_symbols_two_oracles_10000_fixed_seeds() {
    let seeds = std::env::var("CONTEXTDB_SYMBOL_SEEDS")
        .map(|s| s.parse().unwrap())
        .unwrap_or(10_000);
    let plans = (0..seeds).map(|s| (s, generated::plan(s))).collect();
    let git = GitHistories::new(&plans);
    let mut cov = Coverage::default();
    for (seed, steps) in &plans {
        run(*seed, steps, &git, &mut cov);
    }
    assert_eq!(cov.transactions, seeds as usize * STEPS);
    cov.verify(seeds);
}
