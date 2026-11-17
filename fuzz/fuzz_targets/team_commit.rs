//! P10 G4: the entry commit and trailer parser, both object formats. Never
//! a panic, whatever the bytes.
#![no_main]
use contextdb_team::commit::{self, ObjectFormat};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for f in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
        let _ = commit::parse(data, f);
    }
});
