//! Durable operation log, blobs and recovery (SPEC §5 P2, ADR 0006).
//!
//! op_log is an own append-only segment log and the only source of truth.
//! All durable I/O goes through [`Durability`]; [`SimDisk`] drives crash tests.
pub mod doctor;
pub mod durability;
mod keys;
pub mod log;
pub mod migrate;
pub mod migrate5;
mod pack;
pub mod projection;
pub mod purge;
pub mod recall;
pub mod session;
#[doc(hidden)]
pub mod snapshot;
mod store;
pub mod vectors;
pub use durability::{Durability, SimDisk, StdDisk, TailFate};
pub use projection::{PROJECTIONS_FILE, ProjectionFile};
pub use store::{
    ChangePage, LogReader, OpenOptions, OpenReport, Payload, Prepare, Store, StoreError,
    TruncatedTail, TxnStatus, Unavailable, Write,
};

/// Test support (G4, T104): the bytes one payload leaves in a key-log
/// segment (header and key record) and an encrypted pack segment (header
/// and record), as a process of this version writes them.
#[doc(hidden)]
pub fn encrypted_payload_files(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key = keys::fresh().expect("getrandom");
    let mut k = keys::header(keys::Kind::Append, contextdb_model::Epoch(0));
    let (hash, rec) = pack::sealed(bytes, &key).expect("seal");
    k.extend(keys::record(&hash, &key));
    let mut p = pack::SEGMENT_HEADER.to_vec();
    p.extend(rec);
    (k, p)
}
/// Test support (T104): a plaintext pack record, as stores before P9
/// wrote them.
#[doc(hidden)]
pub fn plaintext_pack_record(bytes: &[u8]) -> Vec<u8> {
    let mut r = b"CXBP".to_vec();
    r.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    r.extend_from_slice(blake3::hash(bytes).as_bytes());
    r.extend_from_slice(bytes);
    r
}
/// Fuzz support (ADR 0017 §11 G4): runs the pack (`CXBP`/`CXBE`) and key-log
/// (`CXKH`/`CXKY`) parsers over arbitrary bytes, with keys taken from the
/// key records found, and checks what they accept.
#[doc(hidden)]
pub fn fuzz_record_parsers(data: &[u8]) {
    let _ = keys::parse_header(data);
    let found = keys::parse(data);
    let map: std::collections::HashMap<_, _> = found.iter().copied().collect();
    for (hash, offset, len, nonce) in pack::parse_all(data, &|h| map.get(h).copied()) {
        let body = &data[offset as usize..offset as usize + len as usize];
        match nonce {
            // A plaintext record is valid only when its bytes hash to it.
            None => assert_eq!(*blake3::hash(body).as_bytes(), hash),
            // An encrypted one authenticates under its key and decrypts to
            // bytes with its hash.
            Some(n) => {
                let pt = pack::open_sealed(&map[&hash], &hash, &n, body).expect("accepted");
                assert_eq!(*blake3::hash(&pt).as_bytes(), hash);
            }
        }
    }
}
