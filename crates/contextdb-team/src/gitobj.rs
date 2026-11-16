//! Git object ids computed in process (blob, tree), in SHA-1 (with
//! collision detection) or SHA-256 repositories. `accept` checks a
//! `Compact`'s committed checkpoint tree without git (ADR 0018 §8.6).

use crate::commit::ObjectFormat;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collision;

pub fn object_id(format: ObjectFormat, kind: &str, body: &[u8]) -> Result<Vec<u8>, Collision> {
    let header = format!("{kind} {}\0", body.len());
    match format {
        ObjectFormat::Sha1 => {
            let mut h = sha1_checked::Sha1::new();
            h.update(header.as_bytes());
            h.update(body);
            let r = h.try_finalize();
            if r.has_collision() {
                return Err(Collision);
            }
            Ok(r.hash().to_vec())
        }
        ObjectFormat::Sha256 => {
            let mut h = Sha256::new();
            h.update(header.as_bytes());
            h.update(body);
            Ok(h.finalize().to_vec())
        }
    }
}

enum Node {
    Blob(Vec<u8>),
    Tree(BTreeMap<String, Node>),
}

/// The id of the tree holding `files` (`a/b/c` paths, regular files, mode
/// 100644). Paths must be distinct, non-empty and without empty segments.
pub fn tree_id(format: ObjectFormat, files: &[(String, Vec<u8>)]) -> Result<Vec<u8>, Collision> {
    let mut root = BTreeMap::new();
    for (path, bytes) in files {
        let mut dir = &mut root;
        let parts: Vec<&str> = path.split('/').collect();
        for p in &parts[..parts.len() - 1] {
            let node = dir
                .entry((*p).to_owned())
                .or_insert_with(|| Node::Tree(BTreeMap::new()));
            dir = match node {
                Node::Tree(t) => t,
                Node::Blob(_) => panic!("{path}: a file is also a directory"),
            };
        }
        dir.insert(parts[parts.len() - 1].to_owned(), Node::Blob(bytes.clone()));
    }
    write_tree(format, &root)
}

fn write_tree(format: ObjectFormat, t: &BTreeMap<String, Node>) -> Result<Vec<u8>, Collision> {
    // Git orders tree entries by name, a directory compared as `name/`.
    let mut entries: Vec<(Vec<u8>, &str, Vec<u8>)> = Vec::with_capacity(t.len());
    for (name, node) in t {
        let (mode, oid, sort) = match node {
            Node::Blob(b) => (
                "100644",
                object_id(format, "blob", b)?,
                name.as_bytes().to_vec(),
            ),
            Node::Tree(sub) => {
                let mut s = name.as_bytes().to_vec();
                s.push(b'/');
                ("40000", write_tree(format, sub)?, s)
            }
        };
        let mut e = format!("{mode} {name}\0").into_bytes();
        e.extend_from_slice(&oid);
        entries.push((sort, mode, e));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let body: Vec<u8> = entries.into_iter().flat_map(|(_, _, e)| e).collect();
    object_id(format, "tree", &body)
}
