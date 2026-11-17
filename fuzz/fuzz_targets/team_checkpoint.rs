//! P10 G4: the checkpoint (`CXTK` index, state chunks, blobs). The input is
//! split into named files; decoding never panics, whatever the names,
//! sizes and contents.
#![no_main]
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;

fuzz_target!(|data: &[u8]| {
    let mut files = BTreeMap::new();
    let names = ["checkpoint/index", "checkpoint/state/000000", "checkpoint/state/000001"];
    let mut rest = data;
    for name in names {
        let n = rest.first().map_or(0, |b| *b as usize * 64).min(rest.len().saturating_sub(1));
        if rest.is_empty() {
            break;
        }
        files.insert(name.to_owned(), rest[1..1 + n].to_vec());
        rest = &rest[1 + n..];
    }
    if !rest.is_empty() {
        let h = blake3::hash(rest);
        let hex: String = h.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        files.insert(format!("blobs/{hex}"), rest.to_vec());
    }
    let _ = contextdb_team::checkpoint::decode(&files);
});
