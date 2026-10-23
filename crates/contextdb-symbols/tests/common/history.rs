//! Batched real Git histories: independent object identity and byte readback.
use super::generated::{STEPS, Step};
use contextdb_anchors::{ObjectFormat, capture::git};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
pub fn format_of(seed: u64) -> ObjectFormat {
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
pub struct GitHistories {
    roots: Vec<std::path::PathBuf>,
    pub blobs: BTreeMap<Vec<u8>, Vec<u8>>,
    pub commits: BTreeMap<(u64, usize), (Vec<u8>, Manifest)>,
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
    pub fn new(plans: &BTreeMap<u64, Vec<Step>>) -> Self {
        let mut out = Self {
            roots: Vec::new(),
            blobs: BTreeMap::new(),
            commits: BTreeMap::new(),
        };
        for (format, oid_len) in [("sha1", 20), ("sha256", 32)] {
            let root = std::env::temp_dir().join(format!(
                "p5b-prop12-symbols-git-{}-{format}",
                std::process::id()
            ));
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
                    for (path, body) in s.world() {
                        {
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
                    "blob" => {
                        out.blobs.insert(unhex(fields[0]), body.to_vec());
                    }
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
