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
