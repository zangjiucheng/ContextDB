//! P9 milestone 6a (ADR 0017 §8.1, §8.2): encrypted packs and the key log.
//!
//! - Payloads are stored as `CXBE` records under one key per content hash;
//!   no file in the store holds the plaintext.
//! - An acknowledged payload always has a durable key: after a power cut at
//!   any point after the commit returned, the payload reads back.
//! - Without its key a record is unreadable and the payload absent, so a
//!   commit naming it is `MissingBlob`.
//! - Plaintext records written before P9 stay readable.
use contextdb_model::*;
use contextdb_store::{
    Durability, OpenOptions, Payload, SimDisk, Store, StoreError, TailFate, Unavailable,
};
use std::path::{Path, PathBuf};

const DIR: &str = "/db";
const SECRET: &[u8] = b"the zanzibar vendor holds the signing keys";

fn open(disk: &SimDisk) -> Result<Store<SimDisk>, StoreError> {
    Store::open(disk.clone(), Path::new(DIR), &OpenOptions::default()).map(|(s, _)| s)
}
fn create(content: ContentHash) -> Batch {
    Batch::new(vec![Op::CreateLogical {
        key: "a".into(),
        kind: RecordKind::Fact,
        scope_lane: "repo".into(),
        content,
        adopt_as_base: true,
    }])
}
fn files(disk: &SimDisk, dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for name in disk.list(dir).unwrap_or_default() {
        let path = dir.join(&name);
        match disk.read(&path) {
            Ok(_) => out.push(path),
            Err(_) => out.extend(files(disk, &path)),
        }
    }
    out
}
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Stored payloads are ciphertext: no file holds the plaintext, the key log
/// holds one key record, and the payload reads back before and after a
/// restart.
#[test]
fn payloads_are_encrypted_at_rest() {
    let disk = SimDisk::new();
    let mut store = open(&disk).unwrap();
    let h = store.put_blob(SECRET).unwrap();
    store.commit(&create(h)).unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(SECRET.into())
    );
    drop(store);
    let all = files(&disk, Path::new(DIR));
    assert!(all.iter().any(|p| p.starts_with("/db/keys")), "{all:?}");
    for p in &all {
        assert!(!contains(&disk.read(p).unwrap(), SECRET), "{p:?}");
    }
    let pack = disk
        .read(&Path::new(DIR).join("blobs/pack-00000001"))
        .unwrap();
    assert!(pack.starts_with(b"CXPH"), "an encrypted segment");
    assert!(contains(&pack, b"CXBE") && !contains(&pack, b"CXBP"));
    let store = open(&disk).unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(SECRET.into())
    );
}

/// An acknowledged payload always has a durable key (§8.2): payloads staged
/// with an ordered sync, then committed (the group's full sync), survive a
/// power cut under every tail fate.
#[test]
fn an_acknowledged_payload_always_has_a_durable_key() {
    for seed in 0..64u64 {
        for fate in [
            TailFate::Torn,
            TailFate::ZeroFill,
            TailFate::Garbage,
            TailFate::OutOfOrder,
        ] {
            let disk = SimDisk::new();
            let mut store = open(&disk).unwrap();
            let body = format!("payload {seed}");
            store.stage_blobs(&[body.as_bytes()]).unwrap();
            let h = *blake3::hash(body.as_bytes()).as_bytes();
            store.commit(&create(h)).unwrap();
            // Staged but never committed: may be lost, never half-present.
            let orphan = format!("orphan {seed}");
            store.stage_blobs(&[orphan.as_bytes()]).unwrap();
            drop(store);
            let after = disk.crash(fate, seed);
            let store = open(&after).unwrap();
            assert_eq!(store.epoch(), Epoch(1));
            assert_eq!(
                store.get_blob(&h).unwrap(),
                Payload::Available(body.into_bytes()),
                "seed {seed} {fate:?}"
            );
            let o = *blake3::hash(orphan.as_bytes()).as_bytes();
            assert!(
                matches!(
                    store.get_blob(&o).unwrap(),
                    Payload::Available(_) | Payload::Unavailable(Unavailable::Missing)
                ),
                "seed {seed} {fate:?}"
            );
        }
    }
}

/// Without its key, a record is unreadable and the payload is absent: a
/// read is `Missing` and a commit naming it `MissingBlob`. A record whose
/// header names another content hash does not authenticate either.
#[test]
fn a_record_without_its_key_is_absent() {
    let disk = SimDisk::new();
    let mut store = open(&disk).unwrap();
    let h = store.put_blob(SECRET).unwrap();
    drop(store);
    let keys = Path::new(DIR).join("keys");
    for name in disk.list(&keys).unwrap() {
        disk.remove(&keys.join(name)).unwrap();
    }
    disk.sync_dir(&keys).unwrap();
    let mut store = open(&disk).unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Unavailable(Unavailable::Missing)
    );
    assert_eq!(
        store.commit(&create(h)),
        Err(StoreError::Tx(TxError::MissingBlob { content: h }))
    );
    // Re-sending the bytes stores a fresh record under a fresh key.
    store.put_blob(SECRET).unwrap();
    store.commit(&create(h)).unwrap();
    assert_eq!(
        store.get_blob(&h).unwrap(),
        Payload::Available(SECRET.into())
    );

    // A record relabelled with another hash fails authentication (the
    // hash is the AEAD's associated data).
    let disk = SimDisk::new();
    let mut store = open(&disk).unwrap();
    let a = store.put_blob(b"alpha").unwrap();
    let b = store.put_blob(b"bravo").unwrap();
    drop(store);
    let pack = Path::new(DIR).join("blobs/pack-00000001");
    let mut bytes = disk.read(&pack).unwrap();
    let at = bytes.windows(32).position(|w| w == a).unwrap();
    bytes[at..at + 32].copy_from_slice(&b);
    disk.write_new(&pack, &bytes).unwrap();
    disk.sync(&pack).unwrap();
    let store = open(&disk).unwrap();
    assert_eq!(
        store.get_blob(&a).unwrap(),
        Payload::Unavailable(Unavailable::Missing)
    );
    assert_eq!(
        store.get_blob(&b).unwrap(),
        Payload::Available(b"bravo".into())
    );
}
