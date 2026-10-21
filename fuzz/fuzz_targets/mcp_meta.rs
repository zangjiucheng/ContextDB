//! `tools/call` `_meta` binding (ADR 0010 §2): any JSON maps to a binding or
//! none, never a panic; a binding never carries an empty id.
#![no_main]
use contextdb_daemon::proto::Binding;
use contextdb_mcp::meta_binding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    match meta_binding(&v) {
        Some(Binding::Call { id }) => assert!(!id.is_empty()),
        Some(Binding::Key {
            harness_session,
            agent,
            ..
        }) => {
            assert!(!harness_session.is_empty());
            assert!(agent.is_none_or(|a| !a.is_empty() && a != harness_session));
        }
        Some(Binding::Session { .. }) => unreachable!("never from _meta"),
        None => {}
    }
});
