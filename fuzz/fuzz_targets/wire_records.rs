//! Wire records the hooks read from disk (ADR 0010 §5, §6.2): `GateConfig`
//! (`gating.wire`), outbox records and packets, stream state. Any bytes
//! decode or error, and a decoded value re-encodes to itself; the outbox
//! scan never panics and only returns packets that round-trip.
#![no_main]
use contextdb_model::{Packet, Wire};
use contextdb_proto::{gate::GateConfig, outbox};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(g) = GateConfig::from_bytes(data) {
        assert_eq!(GateConfig::from_bytes(&g.canonical_bytes()).unwrap(), g);
    }
    if let Ok(p) = Packet::from_bytes(data) {
        assert_eq!(Packet::from_bytes(&p.canonical_bytes()).unwrap(), p);
    }
    if let Ok(s) = outbox::StreamState::from_bytes(data) {
        assert_eq!(outbox::StreamState::from_bytes(&s.canonical_bytes()).unwrap(), s);
    }
    for p in outbox::scan(data) {
        assert_eq!(Packet::from_bytes(&p.canonical_bytes()).unwrap(), p);
    }
});
