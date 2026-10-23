//! P5b end to end (ADR 0012): symbol anchors through the real `ctx` binary,
//! observer and daemon. T42b, T42d, T45 (minimal), T46d, T46e, rename
//! suggestions, `ctx anchors symbols`/`status`.
use contextdb_daemon::server::{Client, Daemon, Options};
use contextdb_model::*;
use contextdb_proto::proto::*;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

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
fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}
fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("p5b-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p.canonicalize().unwrap()
}
const LIB: &str = "pub fn foo(x: u8) -> u8 {\n    x + 1\n}\n\npub fn bar() -> u8 {\n    2\n}\n";

struct Rig {
    root: PathBuf,
    data: PathBuf,
    daemon: Daemon,
}
impl Rig {
    fn ds(&self) -> &str {
        self.data.to_str().unwrap()
    }
    fn rs(&self) -> &str {
        self.root.to_str().unwrap()
    }
    fn write(&self, text: &str) {
        std::fs::write(self.root.join("src/lib.rs"), text).unwrap();
    }
    fn scan(&self) -> (i32, String) {
        ctx(&["anchors", "scan", self.ds(), self.rs()])
    }
    fn client(&self, channel: Channel) -> Client {
        Client::connect(self.daemon.socket(channel)).unwrap()
    }
    fn anchors(&self) -> Vec<AnchorEntry> {
        let scope = self.scope();
        match self
            .client(Channel::Observer)
            .call(&Request::AnchorState { scope })
            .unwrap()
            .response
        {
            Response::AnchorState { anchors, .. } => anchors,
            r => panic!("{r:?}"),
        }
    }
    fn scope(&self) -> String {
        let repo = contextdb_anchors::Repo::open(
            &self.root,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        repo.scope()
    }
    fn head_of(&self, suffix: &str) -> RevisionId {
        self.anchors()
            .into_iter()
            .find(|a| a.key.ends_with(suffix))
            .unwrap_or_else(|| panic!("{suffix} not anchored"))
            .head
    }
    fn read(&self, session: SessionId, key: &str) -> Response {
        self.client(Channel::Agent)
            .call(&Request::Read {
                binding: Binding::root(session),
                target: Target::Key(key.into()),
                mode: Mode::Certified,
            })
            .unwrap()
            .response
    }
    fn check(&self, session: SessionId, path: &str) -> Validation {
        match self
            .client(Channel::Agent)
            .call(&Request::Check {
                binding: Binding::root(session),
                cert_id: None,
                live_pos: vec![],
                action: Some(Action {
                    tool: ToolClass::Edit,
                    paths: vec![path.into()],
                    command: None,
                }),
                mode: None,
                harness_version: "test".into(),
                strict: false,
            })
            .unwrap()
            .response
        {
            Response::Check { validation, .. } => validation,
            r => panic!("{r:?}"),
        }
    }
    fn session(&self) -> SessionId {
        match self
            .client(Channel::Agent)
            .call(&Request::SessionStart {
                harness: "test".into(),
                repo: self.rs().into(),
                head_sha: String::new(),
                parent: None,
            })
            .unwrap()
            .response
        {
            Response::Session { session } => session,
            r => panic!("{r:?}"),
        }
    }
    /// A decision with a `Cur` dependency on `rev`.
    fn decision(&self, key: &str, rev: RevisionId) {
        let body = format!("decision {key}").into_bytes();
        let hash = *blake3::hash(&body).as_bytes();
        let r = self
            .client(Channel::Admin)
            .call(&Request::Raw {
                blobs: vec![body],
                batch: Batch::new(vec![
                    Op::CreateLogical {
                        key: key.into(),
                        kind: RecordKind::Decision,
                        scope_lane: "test".into(),
                        content: hash,
                        adopt_as_base: false,
                    },
                    Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: vec![(rev.into(), EdgeKind::Cur)],
                        enabled: true,
                    },
                ]),
            })
            .unwrap()
            .response;
        assert!(matches!(r, Response::Committed { .. }), "{r:?}");
    }
}
fn certified(r: &Response) -> bool {
    matches!(r, Response::Read { .. })
}

#[test]
fn symbol_anchors_end_to_end() {
    let root = temp("repo");
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(root.join("other.rs"), "fn o() {}\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    let data = root.join(".contextdb/db");
    std::fs::create_dir_all(&data).unwrap();
    let daemon = Daemon::start(&data, &Options::default()).unwrap();
    let rig = Rig { root, data, daemon };

    // Registration: symbols must exist; a miss lists the file's symbols.
    let (rc, out) = ctx(&["anchors", "add", rig.ds(), rig.rs(), "src/lib.rs#nope"]);
    assert_ne!(rc, 0);
    assert!(out.contains("foo") && out.contains("bar"), "{out}");
    let (rc, out) = ctx(&[
        "anchors",
        "add",
        rig.ds(),
        rig.rs(),
        "src/lib.rs#foo",
        "src/lib.rs#bar",
        "other.rs",
    ]);
    assert_eq!(rc, 0, "{out}");
    let (_, listing) = ctx(&["anchors", "symbols", rig.rs(), "src/lib.rs"]);
    assert!(listing.contains("src/lib.rs#foo\tcode:"), "{listing}");
    let (_, status) = ctx(&["anchors", "status", rig.ds(), rig.rs()]);
    assert!(status.contains("src/lib.rs#foo r"), "{status}");

    let foo = rig.head_of("#foo");
    rig.decision("uses-foo", foo);
    let session = rig.session();
    assert!(certified(&rig.read(session, "uses-foo")));

    // T42b: an edit outside foo lags until the scan, then foo is fresh with
    // no new revision.
    rig.write(&LIB.replace("    2\n", "    3\n"));
    assert!(
        !certified(&rig.read(session, "uses-foo")),
        "host changed: lag"
    );
    assert_eq!(rig.scan().0, 0);
    assert_eq!(rig.head_of("#foo"), foo);
    assert!(certified(&rig.read(session, "uses-foo")));

    // T45 (minimal): reformatting and comments are no-ops.
    rig.write(&LIB.replace("    2\n", "    3\n").replace(
        "pub fn foo(x: u8) -> u8 {\n    x + 1\n}",
        "/// documented\npub fn foo(\n    x: u8,\n) -> u8 {\n    // why\n    x + 1\n}",
    ));
    assert_eq!(rig.scan().0, 0);
    assert_eq!(rig.head_of("#foo"), foo);
    assert!(certified(&rig.read(session, "uses-foo")));

    // T46d: with foo fresh, an Edit to its host file selects the root.
    assert!(matches!(
        rig.check(session, "src/lib.rs"),
        Validation::Valid
    ));

    // A body change is a new revision: the Cur dependent goes STALE.
    rig.write(&LIB.replace("x + 1", "x + 2"));
    assert_eq!(rig.scan().0, 0);
    assert_ne!(rig.head_of("#foo"), foo);
    assert!(!certified(&rig.read(session, "uses-foo")));
    // T46d: an Edit to the host selects the stale root (no false allow);
    // an Edit to an unrelated anchored file does not.
    assert!(
        matches!(rig.check(session, "src/lib.rs"), Validation::Invalid { .. }),
        "host edit must select the symbol root"
    );
    assert!(!matches!(
        rig.check(session, "other.rs"),
        Validation::Invalid { .. }
    ));

    // T46e: a range touching the host selects the symbol anchors' roots.
    git(&rig.root, &["add", "."]);
    git(&rig.root, &["commit", "-qm", "foo"]);
    let (rc, out) = ctx(&[
        "check",
        rig.ds(),
        "--strict",
        "--range",
        "HEAD~1..HEAD",
        "--repo",
        rig.rs(),
    ]);
    assert_eq!(rc, 1, "{out}");
    assert!(out.contains("Invalid"), "{out}");

    // T42d: an edited host that does not parse is unavailable (nothing
    // committed); a committed one is Broken(parse-error) for the affected
    // symbol only, and the rest stays live.
    let before = rig.anchors();
    rig.write(
        &LIB.replace("x + 1", "x + 2")
            .replace("    2\n", "    2 +* \n"),
    );
    let (rc, out) = rig.scan();
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("being edited"), "{out}");
    assert_eq!(rig.anchors(), before, "nothing committed");
    git(&rig.root, &["add", "."]);
    git(&rig.root, &["commit", "-qm", "broken bar"]);
    let (rc, out) = rig.scan();
    assert_eq!(rc, 0, "{out}");
    let after = rig.anchors();
    let bar = after.iter().find(|a| a.key.ends_with("#bar")).unwrap();
    assert!(!bar.base && !bar.breaks.is_empty(), "bar Broken: {bar:?}");
    let foo_now = after.iter().find(|a| a.key.ends_with("#foo")).unwrap();
    assert!(
        foo_now.base && foo_now.breaks.is_empty(),
        "foo unaffected: {foo_now:?}"
    );

    // Renaming the broken symbol keeps the scope live: bar stays broken
    // (its anchor observes Missing), nothing else changes.
    rig.write(
        &LIB.replace("x + 1", "x + 2")
            .replace("pub fn bar", "pub fn baz"),
    );
    assert_eq!(rig.scan().0, 0);
    let renamed = rig.anchors();
    assert!(renamed.iter().any(|a| a.key.ends_with("#bar") && !a.base));
    assert_eq!(
        renamed
            .iter()
            .find(|a| a.key.ends_with("#foo"))
            .unwrap()
            .head,
        foo_now.head
    );
    drop(rig.daemon);
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// T42e: a committed symbol head made with another normaliser version makes
/// `scan` unavailable (nothing committed); only `migrate` crosses versions.
#[test]
fn normaliser_version_mismatch_needs_migrate() {
    use contextdb_anchors::{
        AnchorDescriptor, AnchorObservation,
        observer::{state_by_key, submit},
        prepare_anchors,
    };
    let root = temp("t42e");
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    let data = root.join(".contextdb/db");
    std::fs::create_dir_all(&data).unwrap();
    let daemon = Daemon::start(&data, &Options::default()).unwrap();
    let rig = Rig { root, data, daemon };
    let (rc, out) = ctx(&["anchors", "add", rig.ds(), rig.rs(), "src/lib.rs#foo"]);
    assert_eq!(rc, 0, "{out}");
    let current = rig.head_of("#foo");

    // Another binary's observer committed foo with an older normaliser.
    let scope = rig.scope();
    let mut client = rig.client(Channel::Observer);
    let (watermark, committed) = state_by_key(&mut client, &scope).unwrap();
    let (key, (descriptor, _)) = committed.iter().next().unwrap();
    let AnchorDescriptor::Symbol(mut old) = descriptor.clone() else {
        panic!("symbol descriptor expected")
    };
    old.normaliser = "rust:tree-sitter-rust=0.0.0;tree-sitter=0.0.0;walk=0".into();
    let observations = [(
        key.clone(),
        (
            "src/lib.rs#foo".to_owned(),
            AnchorObservation::Present(AnchorDescriptor::Symbol(old)),
        ),
    )]
    .into();
    let prior = committed
        .iter()
        .map(|(k, (_, s))| (k.clone(), s.clone()))
        .collect();
    let repo_id = descriptor.scope().trim_start_matches("code:").to_owned();
    let prepared = prepare_anchors(
        &repo_id,
        &observations,
        &prior,
        &Default::default(),
        watermark.clone(),
        watermark.clone().unwrap(),
    )
    .unwrap();
    submit(&mut client, prepared).unwrap().expect("committed");
    let old_head = rig.head_of("#foo");
    assert_ne!(old_head, current);

    // scan refuses and names migrate; nothing is committed.
    let before = rig.anchors();
    let (rc, out) = rig.scan();
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("ctx anchors migrate"), "{out}");
    assert_eq!(rig.anchors(), before);
    let (_, status) = ctx(&["anchors", "status", rig.ds(), rig.rs()]);
    assert!(status.contains("migrate"), "{status}");

    // migrate crosses versions; identical syntax restores the same revision.
    let (rc, out) = ctx(&["anchors", "migrate", rig.ds(), rig.rs()]);
    assert_eq!(rc, 0, "{out}");
    assert_eq!(rig.head_of("#foo"), current);
    assert_eq!(rig.scan().0, 0);
    drop(rig.daemon);
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// A repository with `src/lib.rs` committed and a daemon on `.contextdb/db`.
fn rig(name: &str, files: &[(&str, &str)]) -> Rig {
    let root = temp(name);
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), LIB).unwrap();
    for (path, text) in files {
        std::fs::write(root.join(path), text).unwrap();
    }
    git(&root, &["add", "."]);
    git(&root, &["commit", "-qm", "init"]);
    let data = root.join(".contextdb/db");
    std::fs::create_dir_all(&data).unwrap();
    let daemon = Daemon::start(&data, &Options::default()).unwrap();
    Rig { root, data, daemon }
}
impl Rig {
    /// The scope's committed watermark and anchors.
    fn state(&self) -> Response {
        let scope = self.scope();
        let mut r = self
            .client(Channel::Observer)
            .call(&Request::AnchorState { scope })
            .unwrap()
            .response;
        if let Response::AnchorState { anchors, .. } = &mut r {
            anchors.sort_by(|a, b| a.key.cmp(&b.key));
        }
        r
    }
}
/// Many functions: a host file that takes a while to parse.
fn big_rust(functions: usize, value: u32) -> String {
    let mut s = format!("pub fn f0() -> u32 {{ {value} }}\n");
    for i in 1..functions {
        s += &format!("pub fn f{i}() -> u32 {{ {i} }}\n");
    }
    s
}

/// G4: the observer killed at any point of a scan (capture, parse, walk,
/// submit) publishes all of the scan or nothing; the next scan converges.
#[test]
fn observer_killed_mid_scan_publishes_all_or_nothing() {
    let rig = rig("g4-kill", &[("big.rs", &big_rust(30_000, 0))]);
    let (rc, out) = ctx(&[
        "anchors",
        "add",
        rig.ds(),
        rig.rs(),
        "src/lib.rs#foo",
        "big.rs#f0",
        "big.rs#f1",
    ]);
    assert_eq!(rc, 0, "{out}");
    // Both hosts change: foo's and f0's bodies, so a scan commits.
    rig.write(&LIB.replace("x + 1", "x + 9"));
    std::fs::write(rig.root.join("big.rs"), big_rust(30_000, 7)).unwrap();
    let before = rig.state();

    // How long one observation takes here (listing parses, commits nothing).
    let started = std::time::Instant::now();
    let (rc, out) = ctx(&["anchors", "symbols", rig.rs(), "big.rs"]);
    assert_eq!(rc, 0, "{}", &out[out.len().saturating_sub(300)..]);
    let observe = started.elapsed();
    assert_eq!(rig.state(), before, "listing commits nothing");

    let mut seen = Vec::new();
    let mut killed = 0;
    for tenth in 0..=15u32 {
        let mut scan = Command::new(env!("CARGO_BIN_EXE_ctx"))
            .args(["anchors", "scan", rig.ds(), rig.rs()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(observe * tenth / 10);
        let finished = scan.try_wait().unwrap().is_some();
        let _ = scan.kill();
        scan.wait().unwrap();
        seen.push(rig.state());
        if finished {
            break;
        }
        killed += 1;
    }
    assert!(killed > 0, "no scan was interrupted");
    let (rc, out) = rig.scan();
    assert_eq!(rc, 0, "{out}");
    let after = rig.state();
    assert_ne!(after, before, "the scan has changes to publish");
    for (i, state) in seen.iter().enumerate() {
        assert!(
            *state == before || *state == after,
            "run {i}: partial state {state:?}"
        );
    }
    let published = seen.iter().filter(|s| **s == after).count();
    eprintln!(
        "observe {observe:?}: {killed} scans killed, {} before / {published} published",
        seen.len() - published
    );
    assert_eq!(rig.scan().0, 0);
    assert_eq!(rig.state(), after, "converged");
    drop(rig.daemon);
    let _ = std::fs::remove_dir_all(&rig.root);
}

/// G4: host files beyond the 32 MiB parse budget make the scan unavailable
/// before any parse (nothing committed); identical contents count once.
#[test]
fn parse_byte_budget_end_to_end() {
    let hosts = ["a.rs", "b.rs", "c.rs"];
    let small: Vec<(&str, &str)> = hosts.iter().map(|h| (*h, "pub fn f() {}\n")).collect();
    let rig = rig("g4-budget", &small);
    let mut args = vec!["anchors", "add", rig.ds(), rig.rs()];
    let keys: Vec<String> = hosts.iter().map(|h| format!("{h}#f")).collect();
    args.extend(keys.iter().map(String::as_str));
    let (rc, out) = ctx(&args);
    assert_eq!(rc, 0, "{out}");
    let before = rig.state();

    // 3 × 11 MiB of distinct contents: over budget.
    let pad = 11 << 20;
    for (i, h) in hosts.iter().enumerate() {
        let text = format!("pub fn f() {{}}\n{}\n", format!("// {i}\n").repeat(pad / 5));
        std::fs::write(rig.root.join(h), text).unwrap();
    }
    let started = std::time::Instant::now();
    let (rc, out) = rig.scan();
    assert_ne!(rc, 0, "{out}");
    assert!(out.contains("parse byte budget"), "{out}");
    assert_eq!(rig.state(), before, "nothing committed");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));

    // The same contents in all three: 11 MiB distinct, within budget (the
    // parse may still meet the deadline in an unoptimised build).
    let text = format!("pub fn f() {{}}\n{}\n", "// same\n".repeat(pad / 8));
    for h in hosts {
        std::fs::write(rig.root.join(h), &text).unwrap();
    }
    let (_, out) = rig.scan();
    assert!(!out.contains("parse byte budget"), "{out}");
    drop(rig.daemon);
    let _ = std::fs::remove_dir_all(&rig.root);
}
