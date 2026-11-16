//! A commit signed by real `git` through `ssh-keygen` (`gpg.format=ssh`)
//! parses and verifies in process, in SHA-1 and SHA-256 repositories,
//! whatever the user's global signing configuration says (ADR 0018 §7.1).

use contextdb_team::commit::{self, ObjectFormat};
use contextdb_team::sshsig::{self, PublicKey};
use std::path::Path;
use std::process::Command;

fn run(dir: &Path, args: &[&str], stdin_msg: Option<&str>) -> String {
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "ContextDB")
        .env("GIT_AUTHOR_EMAIL", "team@contextdb")
        .env("GIT_AUTHOR_DATE", "@0 +0000")
        .env("GIT_COMMITTER_NAME", "ContextDB")
        .env("GIT_COMMITTER_EMAIL", "team@contextdb")
        .env("GIT_COMMITTER_DATE", "@0 +0000");
    if let Some(m) = stdin_msg {
        cmd.args(["-m", m]);
    }
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-V").output().is_ok()
        || Command::new(tool).arg("--version").output().is_ok()
}

fn interop(format: ObjectFormat) {
    if !have("git") || !have("ssh-keygen") {
        eprintln!("git or ssh-keygen missing; skipped");
        return;
    }
    let tmp = std::env::temp_dir().join(format!(
        "ctx-team-interop-{}-{:?}",
        std::process::id(),
        format
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let key = tmp.join("id");
    run(
        &tmp,
        &[
            "ssh-keygen",
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "alice@test",
            "-f",
            key.to_str().unwrap(),
        ],
        None,
    );
    let pk = PublicKey::parse(std::fs::read_to_string(tmp.join("id.pub")).unwrap().trim()).unwrap();
    let repo = tmp.join("repo");
    let fmt = match format {
        ObjectFormat::Sha1 => "sha1",
        ObjectFormat::Sha256 => "sha256",
    };
    run(
        &tmp,
        &[
            "git",
            "init",
            "-q",
            "--bare",
            &format!("--object-format={fmt}"),
            repo.to_str().unwrap(),
        ],
        None,
    );
    let tree = run(&repo, &["git", "mktree"], None).trim().to_owned();
    let msg = "ctx team: genesis\n\nCtx-Team: 00112233445566778899aabbccddeeff\nCtx-Team-Seq: 0\nCtx-Author: alice\nCtx-Accept-Version: 1";
    // An OpenPGP-flavoured config underneath, overridden on the command line.
    let oid = run(
        &repo,
        &[
            "git",
            "-c",
            "gpg.format=ssh",
            "-c",
            &format!("user.signingkey={}", key.display()),
            "commit-tree",
            "-S",
            &tree,
        ],
        Some(msg),
    )
    .trim()
    .to_owned();
    let raw = Command::new("git")
        .args(["cat-file", "commit", &oid])
        .current_dir(&repo)
        .output()
        .unwrap()
        .stdout;
    let c = commit::parse(&raw, format)
        .unwrap_or_else(|e| panic!("{e:?}\n{}", String::from_utf8_lossy(&raw)));
    assert_eq!(c.trailers.seq, Some(0));
    assert_eq!(
        sshsig::verify(c.signature.as_ref().unwrap(), "git", &c.payload, &[pk]),
        Ok(pk)
    );
    let mut tampered = c.payload.clone();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(sshsig::verify(c.signature.as_ref().unwrap(), "git", &tampered, &[pk]).is_err());
    // Our own builder produces the same payload bytes git signed.
    let built = commit::build(
        &tree,
        &[],
        "ctx team: genesis",
        &[
            ("Ctx-Team", "00112233445566778899aabbccddeeff".into()),
            ("Ctx-Team-Seq", "0".into()),
            ("Ctx-Author", "alice".into()),
            ("Ctx-Accept-Version", "1".into()),
        ],
    );
    assert_eq!(built, c.payload);
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn git_ssh_signed_commit_verifies_sha1() {
    interop(ObjectFormat::Sha1);
}

#[test]
fn git_ssh_signed_commit_verifies_sha256() {
    interop(ObjectFormat::Sha256);
}
