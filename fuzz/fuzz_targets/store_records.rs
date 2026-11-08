//! P9 G4 (ADR 0017 §11): the `CXBE` / `CXBP` pack record and `CXKY` key
//! record parsers never panic on arbitrary bytes, and every record they
//! accept verifies (hash, or authentication and hash).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    contextdb_store::fuzz_record_parsers(data);
});
