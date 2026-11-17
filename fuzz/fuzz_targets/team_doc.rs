//! P10 G4: the team document, binary (`decode`) and TOML (`from_toml`).
//! Never a panic; a decoded document re-encodes to the same bytes, and a
//! TOML document that parses round-trips through the binary form.
#![no_main]
use contextdb_team::doc::TeamDoc;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(d) = TeamDoc::decode(data) {
        assert_eq!(d.encode(), data, "the team document encoding is canonical");
    }
    if let Ok(d) = TeamDoc::from_toml(&String::from_utf8_lossy(data)) {
        let back = TeamDoc::decode(&d.encode()).expect("an encoded document decodes");
        assert_eq!(back, d);
    }
});
