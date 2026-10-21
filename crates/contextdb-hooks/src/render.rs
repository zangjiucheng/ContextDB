//! Hook output (ADR 0010 §3.4): exactly the keys each pinned harness accepts
//! for each event. Codex rejects any unknown key, so nothing else is ever
//! emitted; exit code 2 is never used.
use crate::adapter::EventName;
use contextdb_model::Decision;
use serde_json::{Map, Value, json};

/// Most bytes of a PreToolUse reason (T38).
pub const REASON_BYTES: usize = 480;
/// Most chars of injected context (ADR 0010 §3.4).
pub const CONTEXT_CHARS: usize = 2_000;

/// What a hook wants to say.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Output {
    /// PreToolUse only. `Allow` and `None` render nothing for the decision.
    pub decision: Option<Decision>,
    /// Shown to the model (PreToolUse warn/deny reason or added context).
    pub reason: String,
    /// `additionalContext` for SessionStart, UserPromptSubmit, PreToolUse warn.
    pub context: String,
    /// `systemMessage`: a one-line notice to the user.
    pub system: String,
}
/// Cuts `s` to at most `max` bytes at a char boundary, marking the cut.
pub fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let ell = "…";
    let mut end = max.saturating_sub(ell.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ell}", &s[..end])
}
/// Cuts `s` to at most `max` chars, marking the cut.
pub fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
/// The stdout object for `event`, or None for no output.
pub fn render(event: EventName, out: &Output) -> Option<String> {
    let mut o = Map::new();
    let specific = |fields: Value| {
        let mut m = Map::new();
        m.insert("hookEventName".into(), json!(event.wire()));
        if let Value::Object(f) = fields {
            m.extend(f);
        }
        Value::Object(m)
    };
    match event {
        EventName::PreToolUse => match out.decision {
            Some(Decision::Deny) => {
                let reason = cut_bytes(
                    if out.reason.is_empty() {
                        "ContextDB denied this action"
                    } else {
                        &out.reason
                    },
                    REASON_BYTES,
                );
                o.insert(
                    "hookSpecificOutput".into(),
                    specific(json!({
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    })),
                );
            }
            Some(Decision::Warn) => {
                let context = if out.context.is_empty() {
                    &out.reason
                } else {
                    &out.context
                };
                o.insert(
                    "hookSpecificOutput".into(),
                    specific(json!({ "additionalContext": cut_chars(context, CONTEXT_CHARS) })),
                );
            }
            // Never `allow`: Claude would skip the permission prompt, Codex
            // rejects a bare allow.
            Some(Decision::Allow) | None => {}
        },
        EventName::SessionStart | EventName::UserPromptSubmit if !out.context.is_empty() => {
            o.insert(
                "hookSpecificOutput".into(),
                specific(json!({ "additionalContext": cut_chars(&out.context, CONTEXT_CHARS) })),
            );
        }
        _ => {}
    }
    // systemMessage is accepted by every event except SessionEnd (no output
    // schema in Codex) and PostToolUse/SubagentStart (nothing to say).
    let system_ok = !matches!(
        event,
        EventName::SessionEnd | EventName::PostToolUse | EventName::SubagentStart
    );
    if system_ok && !out.system.is_empty() {
        o.insert(
            "systemMessage".into(),
            json!(cut_chars(&out.system, CONTEXT_CHARS)),
        );
    }
    (!o.is_empty()).then(|| Value::Object(o).to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cuts_respect_boundaries() {
        let s = "é".repeat(400); // 800 bytes
        let c = cut_bytes(&s, REASON_BYTES);
        assert!(c.len() <= REASON_BYTES && c.ends_with('…'));
        assert_eq!(cut_bytes("short", 480), "short");
        assert_eq!(cut_chars(&"x".repeat(2_500), 2_000).chars().count(), 2_000);
    }
    #[test]
    fn allow_renders_nothing_and_deny_has_a_reason() {
        let allow = Output {
            decision: Some(Decision::Allow),
            ..Output::default()
        };
        assert_eq!(render(EventName::PreToolUse, &allow), None);
        let deny = Output {
            decision: Some(Decision::Deny),
            ..Output::default()
        };
        let v: Value =
            serde_json::from_str(&render(EventName::PreToolUse, &deny).unwrap()).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            !v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        let stop = Output {
            system: "drift".into(),
            context: "ignored".into(),
            ..Output::default()
        };
        assert_eq!(
            render(EventName::Stop, &stop).unwrap(),
            r#"{"systemMessage":"drift"}"#
        );
        assert_eq!(render(EventName::SessionEnd, &stop), None);
    }
}
