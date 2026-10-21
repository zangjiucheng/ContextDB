//! Hook stdin adapters (ADR 0010 §3): any input for any harness and event
//! parses or is `Unparsed`, and whatever renders is one JSON object; never
//! a panic.
#![no_main]
use contextdb_hooks::{
    adapter::{self, EventName, Harness},
    render::{self, Output},
};
use contextdb_model::Decision;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    let harness = if sel & 1 == 0 { Harness::Claude } else { Harness::Codex };
    let event = EventName::ALL[(sel as usize >> 1) % EventName::ALL.len()];
    let text = String::from_utf8_lossy(rest);
    let _ = adapter::parse(harness, event, &text);
    let out = Output {
        decision: [None, Some(Decision::Allow), Some(Decision::Warn), Some(Decision::Deny)]
            [(sel as usize >> 5) % 4],
        reason: text.to_string(),
        context: text.to_string(),
        system: text.to_string(),
    };
    if let Some(json) = render::render(event, &out) {
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok_and(|v| v.is_object()));
    }
});
