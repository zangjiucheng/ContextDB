//! P10 G4 (ADR 0018 §13): the `CXTB` batch codec. Any input decodes or is
//! refused, never a panic; a batch that decodes re-encodes to the same
//! bytes (one canonical encoding).
#![no_main]
use contextdb_team::ops::TeamBatch;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(b) = TeamBatch::decode(data) {
        assert_eq!(b.encode(), data, "CXTB is canonical");
    }
});
