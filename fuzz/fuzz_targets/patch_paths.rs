//! The `apply_patch` path parser (ADR 0010 §3.3): any text, no panic, no
//! empty or duplicate paths.
#![no_main]
use contextdb_hooks::patch;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let paths = patch::paths(&text);
    let _ = patch::is_patch_command(&text);
    for (i, p) in paths.iter().enumerate() {
        assert!(!p.is_empty());
        assert!(!paths[..i].contains(p));
    }
});
