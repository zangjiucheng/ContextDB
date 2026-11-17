//! Reading and writing the root chain with the git CLI (ADR 0018 §8.1).
//! Every call runs with `core.hooksPath=/dev/null` (P8), so ContextDB's own
//! git use never re-enters its hooks, and without a terminal prompt.

use crate::commit::ObjectFormat;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitError {
    /// Objects the repository does not have (a partial clone): fetch by id.
    Missing(Vec<String>),
    Failed(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// An object read by id: `(type, bytes)`, `None` if the repository lacks it.
pub type Object = Option<(String, Vec<u8>)>;

fn failed(m: impl Into<String>) -> GitError {
    GitError::Failed(m.into())
}

/// One entry's objects: the raw commit and its flattened tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryObjects {
    pub commit: Vec<u8>,
    pub tree: Vec<(String, Vec<u8>)>,
}

/// A file mode other than a regular file shows up in the flattened tree
/// under this prefix, which `accept` rejects as an unexpected tree entry.
pub const ODD_MODE_PREFIX: &str = "!mode ";

#[derive(Clone, Debug)]
pub struct Repo {
    dir: PathBuf,
    format: ObjectFormat,
}

impl Repo {
    pub fn open(dir: &Path) -> Result<Self> {
        let mut r = Self {
            dir: dir.to_owned(),
            format: ObjectFormat::Sha1,
        };
        let f = r.run(&["rev-parse", "--show-object-format"], &[])?;
        r.format = match f.trim() {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            other => return Err(failed(format!("object format {other}"))),
        };
        Ok(r)
    }

    pub fn format(&self) -> ObjectFormat {
        self.format
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs git; `Ok((success, stdout, stderr))`.
    pub fn raw(&self, args: &[&str], input: &[u8]) -> Result<(bool, Vec<u8>, Vec<u8>)> {
        let mut child = Command::new("git")
            .arg("-c")
            .arg("core.hooksPath=/dev/null")
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            // Never fetch from a promisor remote behind the caller's back: a
            // partial clone reports missing objects instead (§8.1).
            .env("GIT_NO_LAZY_FETCH", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| failed(format!("git: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped");
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut out = Vec::new();
        let mut err = Vec::new();
        child
            .stdout
            .take()
            .expect("piped")
            .read_to_end(&mut out)
            .map_err(|e| failed(e.to_string()))?;
        child
            .stderr
            .take()
            .expect("piped")
            .read_to_end(&mut err)
            .map_err(|e| failed(e.to_string()))?;
        let status = child.wait().map_err(|e| failed(e.to_string()))?;
        let _ = writer.join();
        Ok((status.success(), out, err))
    }

    pub fn run(&self, args: &[&str], input: &[u8]) -> Result<String> {
        match self.raw(args, input)? {
            (true, out, _) => String::from_utf8(out).map_err(|e| failed(e.to_string())),
            (false, _, err) => Err(failed(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&err).trim()
            ))),
        }
    }

    /// The commit a ref names, `None` if the ref does not exist.
    pub fn resolve(&self, reference: &str) -> Result<Option<String>> {
        match self.raw(
            &[
                "rev-parse",
                "-q",
                "--verify",
                &format!("{reference}^{{commit}}"),
            ],
            &[],
        )? {
            (true, out, _) => Ok(Some(String::from_utf8_lossy(&out).trim().to_owned())),
            (false, _, _) => Ok(None),
        }
    }

    /// The first-parent path from `tip` back to its root, root first.
    pub fn first_parent_path(&self, tip: &str) -> Result<Vec<String>> {
        match self.raw(&["rev-list", "--first-parent", "--reverse", tip], &[])? {
            (true, out, _) => Ok(String::from_utf8_lossy(&out)
                .split_whitespace()
                .map(str::to_owned)
                .collect()),
            (false, _, err) => {
                let e = String::from_utf8_lossy(&err);
                if e.contains("bad object")
                    || e.contains("missing")
                    || e.contains("unknown revision")
                {
                    Err(GitError::Missing(vec![tip.to_owned()]))
                } else {
                    Err(failed(format!("rev-list: {}", e.trim())))
                }
            }
        }
    }

    /// Whether `a` is an ancestor of (or equal to) `b`.
    pub fn is_ancestor(&self, a: &str, b: &str) -> Result<bool> {
        Ok(self.raw(&["merge-base", "--is-ancestor", a, b], &[])?.0)
    }

    /// Objects by id through `git cat-file --batch`; `None` for a missing one.
    pub fn read_objects(&self, oids: &[String]) -> Result<Vec<Object>> {
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let (ok, out, err) = self.raw(&["cat-file", "--batch"], input.as_bytes())?;
        if !ok {
            return Err(failed(format!(
                "cat-file: {}",
                String::from_utf8_lossy(&err).trim()
            )));
        }
        let mut res = Vec::with_capacity(oids.len());
        let mut rest = &out[..];
        for _ in oids {
            let nl = rest
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| failed("cat-file: truncated"))?;
            let header = std::str::from_utf8(&rest[..nl])
                .map_err(|_| failed("cat-file header"))?
                .to_owned();
            rest = &rest[nl + 1..];
            let parts: Vec<&str> = header.split(' ').collect();
            if parts.len() == 2 && parts[1] == "missing" {
                res.push(None);
                continue;
            }
            let [_, kind, size] = parts[..] else {
                return Err(failed(format!("cat-file header {header}")));
            };
            let size: usize = size.parse().map_err(|_| failed("cat-file size"))?;
            if rest.len() < size + 1 {
                return Err(failed("cat-file: truncated body"));
            }
            res.push(Some((kind.to_owned(), rest[..size].to_vec())));
            rest = &rest[size + 1..];
        }
        Ok(res)
    }

    /// An entry's commit and flattened tree. Missing objects (a partial
    /// clone without the blobs) are reported all at once, by id.
    pub fn read_entry(&self, oid: &str) -> Result<EntryObjects> {
        let commit = match self.read_objects(&[oid.to_owned()])?.pop().flatten() {
            Some((k, b)) if k == "commit" => b,
            Some((k, _)) => return Err(failed(format!("{oid} is a {k}"))),
            None => return Err(GitError::Missing(vec![oid.to_owned()])),
        };
        let tree = std::str::from_utf8(&commit)
            .ok()
            .and_then(|t| t.strip_prefix("tree "))
            .and_then(|t| t.split('\n').next())
            .ok_or_else(|| failed("commit without a tree"))?
            .to_owned();
        let (ok, out, err) = self.raw(&["ls-tree", "-r", "-z", "--full-tree", &tree], &[])?;
        if !ok {
            let e = String::from_utf8_lossy(&err);
            return Err(if e.contains("not a tree object") || e.contains("bad") {
                GitError::Missing(vec![tree])
            } else {
                failed(format!("ls-tree: {}", e.trim()))
            });
        }
        let mut listed: Vec<(String, String, String)> = Vec::new();
        for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let rec = std::str::from_utf8(rec).map_err(|_| failed("ls-tree: not utf-8"))?;
            let (meta, path) = rec
                .split_once('\t')
                .ok_or_else(|| failed("ls-tree record"))?;
            let m: Vec<&str> = meta.split(' ').collect();
            let [mode, kind, id] = m[..] else {
                return Err(failed("ls-tree record"));
            };
            listed.push((format!("{mode} {kind}"), id.to_owned(), path.to_owned()));
        }
        let ids: Vec<String> = listed
            .iter()
            .filter(|l| l.0 == "100644 blob")
            .map(|l| l.1.clone())
            .collect();
        let objects = self.read_objects(&ids)?;
        let missing: Vec<String> = ids
            .iter()
            .zip(&objects)
            .filter(|(_, o)| o.is_none())
            .map(|(i, _)| i.clone())
            .collect();
        if !missing.is_empty() {
            return Err(GitError::Missing(missing));
        }
        let mut objects = objects.into_iter();
        let mut files = Vec::with_capacity(listed.len());
        for (mode, _, path) in listed {
            if mode == "100644 blob" {
                files.push((path, objects.next().flatten().expect("present").1));
            } else {
                files.push((format!("{ODD_MODE_PREFIX}{mode} {path}"), Vec::new()));
            }
        }
        Ok(EntryObjects {
            commit,
            tree: files,
        })
    }

    /// [`Self::read_entry`] for many entries in a few `cat-file --batch`
    /// runs (commits, then each tree level, then the blobs), whatever their
    /// number: import's cost is no longer a git process per entry. Each
    /// entry's result is what `read_entry` gives for it, missing objects
    /// included.
    pub fn read_entries(&self, oids: &[String]) -> Result<Vec<Result<EntryObjects>>> {
        let id_len = match self.format {
            ObjectFormat::Sha1 => 20,
            ObjectFormat::Sha256 => 32,
        };
        let commits = self.read_objects(oids)?;
        // Per entry: its commit bytes, then its tree id.
        let mut out: Vec<Option<Result<EntryObjects>>> = Vec::with_capacity(oids.len());
        let mut roots: Vec<Option<(Vec<u8>, String)>> = Vec::with_capacity(oids.len());
        for (oid, c) in oids.iter().zip(commits) {
            match c {
                Some((k, b)) if k == "commit" => {
                    let tree = std::str::from_utf8(&b)
                        .ok()
                        .and_then(|t| t.strip_prefix("tree "))
                        .and_then(|t| t.split('\n').next())
                        .map(str::to_owned);
                    match tree {
                        Some(t) => {
                            roots.push(Some((b, t)));
                            out.push(None);
                        }
                        None => {
                            roots.push(None);
                            out.push(Some(Err(failed("commit without a tree"))));
                        }
                    }
                }
                Some((k, _)) => {
                    roots.push(None);
                    out.push(Some(Err(failed(format!("{oid} is a {k}")))));
                }
                None => {
                    roots.push(None);
                    out.push(Some(Err(GitError::Missing(vec![oid.clone()]))));
                }
            }
        }
        // Every tree reachable from the roots, level by level.
        let mut trees: std::collections::BTreeMap<String, Option<Vec<u8>>> = Default::default();
        let mut level: Vec<String> = roots.iter().flatten().map(|(_, t)| t.clone()).collect();
        level.sort();
        level.dedup();
        while !level.is_empty() {
            let got = self.read_objects(&level)?;
            let mut next = Vec::new();
            for (id, o) in level.iter().zip(got) {
                let body = match o {
                    Some((k, b)) if k == "tree" => Some(b),
                    _ => None,
                };
                if let Some(b) = &body {
                    for (mode, _, sub) in parse_tree(b, id_len)? {
                        if mode == "40000" && !trees.contains_key(&sub) {
                            next.push(sub);
                        }
                    }
                }
                trees.insert(id.clone(), body);
            }
            next.sort();
            next.dedup();
            level = next;
        }
        // Flatten each root in `ls-tree -r` order.
        type Listed = Vec<(String, String, String)>;
        fn flatten(
            trees: &std::collections::BTreeMap<String, Option<Vec<u8>>>,
            id: &str,
            prefix: &str,
            id_len: usize,
            into: &mut Listed,
        ) -> std::result::Result<(), GitError> {
            let Some(Some(body)) = trees.get(id) else {
                return Err(GitError::Missing(vec![id.to_owned()]));
            };
            for (mode, name, sub) in parse_tree(body, id_len)? {
                let path = format!("{prefix}{name}");
                match mode.as_str() {
                    "40000" => flatten(trees, &sub, &format!("{path}/"), id_len, into)?,
                    "160000" => into.push((format!("{mode} commit"), sub, path)),
                    m => into.push((
                        format!(
                            "{} blob",
                            if m.len() < 6 {
                                format!("{m:0>6}")
                            } else {
                                m.to_owned()
                            }
                        ),
                        sub,
                        path,
                    )),
                }
            }
            Ok(())
        }
        let mut listings: Vec<Option<Listed>> = Vec::with_capacity(oids.len());
        let mut blob_ids: Vec<String> = Vec::new();
        for (i, r) in roots.iter().enumerate() {
            let Some((_, tree)) = r else {
                listings.push(None);
                continue;
            };
            let mut l = Vec::new();
            match flatten(&trees, tree, "", id_len, &mut l) {
                Ok(()) => {
                    blob_ids.extend(
                        l.iter()
                            .filter(|x| x.0 == "100644 blob")
                            .map(|x| x.1.clone()),
                    );
                    listings.push(Some(l));
                }
                Err(e) => {
                    out[i] = Some(Err(e));
                    listings.push(None);
                }
            }
        }
        blob_ids.sort();
        blob_ids.dedup();
        let blobs: std::collections::BTreeMap<String, Option<Vec<u8>>> = blob_ids
            .iter()
            .cloned()
            .zip(
                self.read_objects(&blob_ids)?
                    .into_iter()
                    .map(|o| o.map(|(_, b)| b)),
            )
            .collect();
        let mut res = Vec::with_capacity(oids.len());
        for ((o, root), listed) in out.into_iter().zip(roots).zip(listings) {
            if let Some(r) = o {
                res.push(r);
                continue;
            }
            let (Some((commit, _)), Some(listed)) = (root, listed) else {
                res.push(Err(failed("unreachable")));
                continue;
            };
            let missing: Vec<String> = listed
                .iter()
                .filter(|x| x.0 == "100644 blob" && blobs.get(&x.1).is_none_or(Option::is_none))
                .map(|x| x.1.clone())
                .collect();
            if !missing.is_empty() {
                res.push(Err(GitError::Missing(missing)));
                continue;
            }
            let tree = listed
                .into_iter()
                .map(|(mode, id, path)| {
                    if mode == "100644 blob" {
                        (path, blobs[&id].clone().expect("present"))
                    } else {
                        (format!("{ODD_MODE_PREFIX}{mode} {path}"), Vec::new())
                    }
                })
                .collect();
            res.push(Ok(EntryObjects { commit, tree }));
        }
        Ok(res)
    }

    /// Writes regular files as a tree (through a temporary index) and
    /// returns its id.
    pub fn write_tree(&self, files: &[(String, Vec<u8>)]) -> Result<String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let index = std::env::temp_dir().join(format!("ctx-team-index-{}-{n}", std::process::id()));
        let mut info = String::new();
        for (path, bytes) in files {
            let oid = self.run(&["hash-object", "-w", "--stdin"], bytes)?;
            info.push_str(&format!("100644 {}\t{path}\n", oid.trim()));
        }
        let with_index = |args: &[&str], input: &[u8]| -> Result<String> {
            let mut child = Command::new("git")
                .arg("-c")
                .arg("core.hooksPath=/dev/null")
                .args(args)
                .current_dir(&self.dir)
                .env("GIT_INDEX_FILE", &index)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| failed(e.to_string()))?;
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(input)
                .map_err(|e| failed(e.to_string()))?;
            let out = child
                .wait_with_output()
                .map_err(|e| failed(e.to_string()))?;
            if !out.status.success() {
                return Err(failed(
                    String::from_utf8_lossy(&out.stderr).trim().to_owned(),
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        };
        let r = with_index(&["update-index", "--add", "--index-info"], info.as_bytes())
            .and_then(|_| with_index(&["write-tree"], &[]));
        let _ = std::fs::remove_file(&index);
        r
    }

    /// Writes a raw commit object and returns its id.
    pub fn write_commit(&self, raw: &[u8]) -> Result<String> {
        Ok(self
            .run(&["hash-object", "-t", "commit", "-w", "--stdin"], raw)?
            .trim()
            .to_owned())
    }

    /// Moves `name` to `new` if it is at `old` (`None`: must not exist).
    pub fn update_ref(&self, name: &str, new: &str, old: Option<&str>) -> Result<()> {
        let zero = "0".repeat(self.format.hex_len());
        self.run(&["update-ref", name, new, old.unwrap_or(&zero)], &[])?;
        Ok(())
    }
}

/// A raw tree object's entries: `(mode, name, object id hex)`.
fn parse_tree(body: &[u8], id_len: usize) -> Result<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let sp = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| failed("tree: mode"))?;
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| failed("tree: name"))?;
        if nul < sp || rest.len() < nul + 1 + id_len {
            return Err(failed("tree: truncated"));
        }
        let mode = std::str::from_utf8(&rest[..sp])
            .map_err(|_| failed("tree: mode"))?
            .to_owned();
        let name = std::str::from_utf8(&rest[sp + 1..nul])
            .map_err(|_| failed("tree: name"))?
            .to_owned();
        let id: String = rest[nul + 1..nul + 1 + id_len]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        out.push((mode, name, id));
        rest = &rest[nul + 1 + id_len..];
    }
    Ok(out)
}
