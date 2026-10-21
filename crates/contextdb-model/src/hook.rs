//! Hook and outbox vocabulary shared by the protocol, the session log and
//! `ctx-hook` (ADR 0010 §3–§5). These records are session facts: they never
//! feed kernel state (SPEC §3.5 #7, ADR 0010 A8).
use crate::{ContentHash, Decoder, Wire, WireError};

/// What kind of tool an action is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolClass {
    Edit,
    Write,
    Notebook,
    Patch,
    Shell,
    Other(String),
}
/// A tool call as the hooks see it. Paths are repository-relative, or
/// absolute for paths outside the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub tool: ToolClass,
    pub paths: Vec<String>,
    pub command: Option<String>,
}
/// A gate decision (ADR 0010 §6.1); `Deny` only comes from `enforce`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Allow,
    Warn,
    Deny,
}
/// Gating mode for one action (ADR 0010 §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GateMode {
    Off,
    Warn,
    Enforce,
}
/// Declared-vs-done drift of one agent (ADR 0010 §5.4). `undeclared` is an
/// upper bound: edits by people or by agents without hooks also land there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriftReport {
    pub undeclared: Vec<String>,
    pub unrealised: Vec<String>,
    /// `git` did not answer in time: `undeclared` is empty but unknown.
    pub done_unknown: bool,
    pub shell_actions: u32,
    pub writes: u32,
    pub writes_with_deps: u32,
}
/// One outbox fact (ADR 0010 §5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact {
    /// PostToolUse: the action ran (`ok`) or failed.
    Action { action: Action, ok: bool },
    /// Stop / SubagentStop / PreCompact / SessionEnd: the paths git reports
    /// as changed since the session started (None: unknown).
    Drift { done: Option<Vec<String>> },
    /// SessionStart: paths already dirty, with their content hashes.
    Baseline { dirty: Vec<(String, ContentHash)> },
    /// SessionEnd.
    StreamEnd,
    /// PreToolUse while the daemon was unavailable: what the hook decided
    /// (`on_unavailable`, ADR 0010 §6.3).
    Gate {
        action: Action,
        mode: GateMode,
        decision: Decision,
        harness_version: String,
    },
}
/// A fact with its stream position and the agent that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub seq: u64,
    pub agent: Option<String>,
    pub fact: Fact,
}

impl Wire for ToolClass {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Edit => 0u8.encode(o),
            Self::Write => 1u8.encode(o),
            Self::Notebook => 2u8.encode(o),
            Self::Patch => 3u8.encode(o),
            Self::Shell => 4u8.encode(o),
            Self::Other(name) => {
                5u8.encode(o);
                name.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Edit,
            1 => Self::Write,
            2 => Self::Notebook,
            3 => Self::Patch,
            4 => Self::Shell,
            5 => Self::Other(Wire::decode(i)?),
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for Action {
    fn encode(&self, o: &mut Vec<u8>) {
        self.tool.encode(o);
        self.paths.encode(o);
        self.command.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            tool: Wire::decode(i)?,
            paths: Wire::decode(i)?,
            command: Wire::decode(i)?,
        })
    }
}
impl Wire for Decision {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Allow,
            1 => Self::Warn,
            2 => Self::Deny,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for GateMode {
    fn encode(&self, o: &mut Vec<u8>) {
        (*self as u8).encode(o)
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Off,
            1 => Self::Warn,
            2 => Self::Enforce,
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for DriftReport {
    fn encode(&self, o: &mut Vec<u8>) {
        self.undeclared.encode(o);
        self.unrealised.encode(o);
        self.done_unknown.encode(o);
        self.shell_actions.encode(o);
        self.writes.encode(o);
        self.writes_with_deps.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            undeclared: Wire::decode(i)?,
            unrealised: Wire::decode(i)?,
            done_unknown: Wire::decode(i)?,
            shell_actions: Wire::decode(i)?,
            writes: Wire::decode(i)?,
            writes_with_deps: Wire::decode(i)?,
        })
    }
}
impl Wire for Fact {
    fn encode(&self, o: &mut Vec<u8>) {
        match self {
            Self::Action { action, ok } => {
                0u8.encode(o);
                action.encode(o);
                ok.encode(o);
            }
            Self::Drift { done } => {
                1u8.encode(o);
                done.encode(o);
            }
            Self::Baseline { dirty } => {
                2u8.encode(o);
                dirty.encode(o);
            }
            Self::StreamEnd => 3u8.encode(o),
            Self::Gate {
                action,
                mode,
                decision,
                harness_version,
            } => {
                4u8.encode(o);
                action.encode(o);
                mode.encode(o);
                decision.encode(o);
                harness_version.encode(o);
            }
        }
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(match u8::decode(i)? {
            0 => Self::Action {
                action: Wire::decode(i)?,
                ok: Wire::decode(i)?,
            },
            1 => Self::Drift {
                done: Wire::decode(i)?,
            },
            2 => Self::Baseline {
                dirty: Wire::decode(i)?,
            },
            3 => Self::StreamEnd,
            4 => Self::Gate {
                action: Wire::decode(i)?,
                mode: Wire::decode(i)?,
                decision: Wire::decode(i)?,
                harness_version: Wire::decode(i)?,
            },
            _ => return Err(WireError::InvalidEncoding),
        })
    }
}
impl Wire for Packet {
    fn encode(&self, o: &mut Vec<u8>) {
        self.seq.encode(o);
        self.agent.encode(o);
        self.fact.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            seq: Wire::decode(i)?,
            agent: Wire::decode(i)?,
            fact: Wire::decode(i)?,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::DecodeLimits;
    #[test]
    fn hook_records_round_trip() {
        let action = Action {
            tool: ToolClass::Other("Monitor".into()),
            paths: vec!["src/lib.rs".into(), "/etc/hosts".into()],
            command: Some("make".into()),
        };
        for fact in [
            Fact::Action {
                action: action.clone(),
                ok: true,
            },
            Fact::Drift { done: None },
            Fact::Drift {
                done: Some(vec!["a".into()]),
            },
            Fact::Baseline {
                dirty: vec![("b".into(), [7; 32])],
            },
            Fact::StreamEnd,
            Fact::Gate {
                action,
                mode: GateMode::Enforce,
                decision: Decision::Deny,
                harness_version: "2.1.306".into(),
            },
        ] {
            let p = Packet {
                seq: 9,
                agent: Some("a1".into()),
                fact,
            };
            let bytes = p.canonical_bytes();
            assert_eq!(
                Packet::from_bytes_with(&bytes, DecodeLimits::IPC).unwrap(),
                p
            );
        }
        let d = DriftReport {
            undeclared: vec!["x".into()],
            shell_actions: 2,
            ..DriftReport::default()
        };
        assert_eq!(DriftReport::from_bytes(&d.canonical_bytes()).unwrap(), d);
    }
}
