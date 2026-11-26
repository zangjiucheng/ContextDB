//! P11 G6 B: `ctx g6 checkpoint` against a running ctxd and `ctx g6
//! verify` of its file.
//!
//! - a checkpoint taken right after a deferred cascade settles first, and
//!   holds the session's certificates with their verdicts;
//! - verify replays the prefix in a copy and finds no mismatch, also after
//!   ctxd appended more epochs;
//! - a forged kernel digest and a forged certificate verdict each fail;
//! - the request is admin only, and `--work` may not overlap the data.
use contextdb_daemon::{
    proto::*,
    server::{Daemon, Options},
    service::Identity,
};
use contextdb_model::*;
use contextdb_testkit::{Shape, shape};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const N: usize = 300;
const AGENT: Identity = Identity {
    channel: Channel::Agent,
    principal: PrincipalId::STORE_OWNER,
};
const ADMIN: Identity = Identity {
    channel: Channel::Admin,
    principal: PrincipalId::STORE_OWNER,
};

fn ctx(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_ctx"))
        .args(args)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code(), text)
}
fn call(d: &Daemon, id: Identity, req: Request) -> Response {
    d.service.handle(id, req).response
}
fn admin(d: &Daemon, ops: Vec<Op>, blobs: Vec<Vec<u8>>) -> (Epoch, bool) {
    let mut batch = Batch::new(ops);
    batch.channel = Channel::Admin;
    match call(d, ADMIN, Request::Raw { blobs, batch }) {
        Response::Committed { result, settling } => (result.epoch, settling),
        other => panic!("{other:?}"),
    }
}
/// A hub of `N` revisions over one payload (bases 0, 1, 2).
fn hub(d: &Daemon) {
    let payload = b"payload".to_vec();
    let content = *blake3::hash(&payload).as_bytes();
    for mut b in shape(Shape::Hub, N, 42).batches {
        for op in &mut b.ops {
            if let Op::CreateLogical { content: c, .. } = op {
                *c = content;
            }
        }
        admin(d, b.ops, vec![payload.clone()]);
    }
}
fn toggle(on: bool) -> Vec<Op> {
    let rev = RevisionId(0).into();
    vec![match on {
        true => Op::AssertBase { rev },
        false => Op::RetractBase { rev },
    }]
}
/// A session that read `key` certified, and its session certificate.
fn certified(d: &Daemon, key: &str) {
    let start = Request::SessionStart {
        harness: "test".into(),
        repo: "r".into(),
        head_sha: "h".into(),
        parent: None,
        branch: None,
        git_branch: None,
    };
    let Response::Session { session } = call(d, AGENT, start) else {
        panic!()
    };
    let read = Request::Read {
        binding: Binding::root(session),
        target: Target::Key(key.into()),
        mode: Mode::Certified,
        level: None,
    };
    let r = call(d, AGENT, read);
    assert!(matches!(r, Response::Read { .. }), "{r:?}");
    let check = Request::Check {
        binding: Binding::root(session),
        cert_id: None,
        live_pos: Vec::new(),
        action: None,
        mode: None,
        harness_version: String::new(),
        strict: false,
    };
    let r = call(d, AGENT, check);
    assert!(
        matches!(
            &r,
            Response::Check {
                validation: Validation::Valid,
                cert: Some(_),
                ..
            }
        ),
        "{r:?}"
    );
}
fn read_json(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}
fn write_json(p: &Path, v: &Value) {
    std::fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn checkpoint_and_verify_through_ctxd() {
    let root = std::env::temp_dir().join(format!("ctx-p11-g6-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    // One unit per slice: the retract below is acknowledged settling.
    std::fs::write(dir.join("ctxd.toml"), "[maintenance]\nlazy_budget = 1\n").unwrap();
    let d = Daemon::start(
        &dir,
        &Options {
            full_sync: false,
            ..Options::default()
        },
    )
    .unwrap();
    let path = dir.to_str().unwrap();
    hub(&d);
    certified(&d, &format!("shape/{}", N - 1));
    let (_, settling) = admin(&d, toggle(false), Vec::new());
    assert!(settling, "the cascade exceeds the budget");

    // Admin only.
    let r = call(&d, AGENT, Request::Checkpoint { live_pos: None });
    assert!(
        matches!(&r, Response::Error { code, .. } if code == "ChannelRestricted"),
        "{r:?}"
    );

    let file = root.join("cp.json");
    let (code, out) = ctx(&["g6", "checkpoint", path, "--out", file.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{out}");
    let cp = read_json(&file);
    let epoch = cp["epoch"].as_u64().unwrap();
    assert!(
        out.contains(&format!("g6 checkpoint epoch={epoch} branches=1")),
        "{out}"
    );
    assert_eq!(cp["ctxd"]["lazy_budget"], 1);
    assert_eq!(cp["ctxd"]["pid"], std::process::id());
    assert_eq!(cp["ctxd"]["exe_blake3"].as_str().unwrap().len(), 64, "{cp}");
    assert_eq!(cp["meta"]["exe_file_blake3_now"], cp["ctxd"]["exe_blake3"]);
    assert_eq!(
        cp["meta"]["ctxd_toml_now"],
        "[maintenance]\nlazy_budget = 1\n"
    );
    // The session's certificate: its read now has no support.
    let certs = cp["certs"].as_array().unwrap();
    assert!(!certs.is_empty(), "{cp}");
    assert!(cp["counts"]["non_empty"].as_u64().unwrap() >= 1, "{cp}");
    assert!(certs.iter().all(|c| c["verdict"] != "Valid"), "{cp}");

    let verify = |file: &Path| ctx(&["g6", "verify", path, file.to_str().unwrap()]);
    let (code, out) = verify(&file);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains(&format!("g6 verify epoch={epoch} branches=1")),
        "{out}"
    );
    assert!(out.contains("mismatches=0 errors=0"), "{out}");
    assert!(out.contains("unavailable=0"), "{out}");

    // ctxd appends; the prefix is still the checkpoint's.
    admin(&d, toggle(true), Vec::new());
    let (code, out) = verify(&file);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("mismatches=0 errors=0"), "{out}");

    // A forged kernel digest.
    let mut forged = cp.clone();
    forged["branches"][0]["kernel_digest"] = Value::String(hex(&[0xaa; 32]));
    let bad = root.join("digest.json");
    write_json(&bad, &forged);
    let (code, out) = verify(&bad);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("mismatch .branches[0].kernel_digest"), "{out}");
    assert!(out.contains("mismatches=1 errors=0"), "{out}");

    // A forged certificate verdict: `Valid` where the replay says otherwise.
    let mut forged = cp.clone();
    forged["certs"][0]["verdict"] = Value::String("Valid".into());
    forged["certs"][0]["verdict_wire"] = Value::String(hex(&Validation::Valid.canonical_bytes()));
    let bad = root.join("verdict.json");
    write_json(&bad, &forged);
    let (code, out) = verify(&bad);
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("mismatch .certs[0].verdict"), "{out}");
    assert!(out.contains("mismatch cert "), "{out}");

    // The work copy never overlaps the data directory.
    let (code, out) = ctx(&[
        "g6",
        "verify",
        path,
        file.to_str().unwrap(),
        "--work",
        dir.join("w").to_str().unwrap(),
    ]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("overlaps the data directory"), "{out}");
    d.stop();
    drop(d);
    let _ = std::fs::remove_dir_all(&root);
}
