//! Symbol extraction and normalisation (ADR 0012 §4, P5b G4): any bytes in
//! any language, no panic and no stack overflow; the result is deterministic
//! and every symbol name round-trips through its key encoding.
#![no_main]
use contextdb_anchors::{
    Language,
    symbol::{decode_symbol, encode_symbol},
};
use libfuzzer_sys::fuzz_target;
use std::time::{Duration, Instant};

const LANGUAGES: [Language; 6] = [
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Python,
    Language::Go,
];

fuzz_target!(|data: &[u8]| {
    let Some((&pick, src)) = data.split_first() else {
        return;
    };
    let language = LANGUAGES[usize::from(pick) % LANGUAGES.len()];
    let parse =
        || contextdb_symbols::parse(language, src, Instant::now() + Duration::from_secs(10));
    let Ok(file) = parse() else {
        return;
    };
    if let Ok(again) = parse() {
        assert_eq!(file, again, "deterministic");
    }
    for symbol in file.symbols.keys() {
        let key = encode_symbol(symbol).expect("extracted names are encodable");
        assert_eq!(&decode_symbol(&key).unwrap(), symbol);
    }
});
