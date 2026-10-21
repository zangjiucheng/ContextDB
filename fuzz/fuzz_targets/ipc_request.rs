//! Daemon request decoding (ADR 0008 §5): arbitrary bytes never panic, and a
//! decoded request re-encodes to bytes that decode to the same request.
#![no_main]
use contextdb_daemon::proto::decode_request;
use contextdb_model::Wire;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(request) = decode_request(data) {
        let again = decode_request(&request.canonical_bytes()).expect("re-encoded request decodes");
        assert_eq!(again, request);
    }
});
