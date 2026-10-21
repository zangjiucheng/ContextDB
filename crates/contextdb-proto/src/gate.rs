//! The gate predicate (ADR 0010 §6.1), shared by the daemon (`Check`), the
//! hook, `ctx check --dry-run`, `ctx_check` and the report, so a dry run and
//! a real check can never disagree.
use contextdb_model::*;

/// Gating configuration compiled by `ctx gating apply` into
/// `<data dir>/gating.wire` (ADR 0010 §6.2). Hooks read only this form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateConfig {
    /// Mode for actions no rule matches.
    pub mode: GateMode,
    /// Mode for shell actions no rule matches (default `Warn`).
    pub shell_mode: GateMode,
    /// `true`: deny when the daemon is unavailable (fail-closed).
    pub fail_closed: bool,
    pub timeout_ms: u32,
    /// Ordered rules; the last match wins.
    pub rules: Vec<GateRule>,
    /// blake3 of the source `gating.toml`, to detect an unapplied edit.
    pub source_hash: ContentHash,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateRule {
    /// `None` matches every tool class.
    pub tool: Option<ToolClass>,
    /// Glob over repository-relative paths (`*`, `**`, `?`); `None` matches all.
    pub path: Option<String>,
    pub mode: GateMode,
}
impl Default for GateConfig {
    fn default() -> Self {
        Self {
            mode: GateMode::Warn,
            shell_mode: GateMode::Warn,
            fail_closed: false,
            timeout_ms: 150,
            rules: Vec::new(),
            source_hash: [0; 32],
        }
    }
}
impl Wire for GateRule {
    fn encode(&self, o: &mut Vec<u8>) {
        self.tool.encode(o);
        self.path.encode(o);
        self.mode.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            tool: Wire::decode(i)?,
            path: Wire::decode(i)?,
            mode: Wire::decode(i)?,
        })
    }
}
impl Wire for GateConfig {
    fn encode(&self, o: &mut Vec<u8>) {
        self.mode.encode(o);
        self.shell_mode.encode(o);
        self.fail_closed.encode(o);
        self.timeout_ms.encode(o);
        self.rules.encode(o);
        self.source_hash.encode(o);
    }
    fn decode(i: &mut Decoder<'_>) -> Result<Self, WireError> {
        Ok(Self {
            mode: Wire::decode(i)?,
            shell_mode: Wire::decode(i)?,
            fail_closed: Wire::decode(i)?,
            timeout_ms: Wire::decode(i)?,
            rules: Wire::decode(i)?,
            source_hash: Wire::decode(i)?,
        })
    }
}
fn same_class(a: &ToolClass, b: &ToolClass) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}
/// The mode for one action: per path the last matching rule wins (else the
/// default for its class), and across paths the most severe mode wins.
pub fn resolve_mode(cfg: &GateConfig, action: &Action) -> GateMode {
    let base = if action.tool == ToolClass::Shell {
        cfg.shell_mode
    } else {
        cfg.mode
    };
    let for_path = |path: Option<&str>| {
        cfg.rules
            .iter()
            .rev()
            .filter(|r| r.tool.as_ref().is_none_or(|t| same_class(t, &action.tool)))
            .find(|r| match (&r.path, path) {
                (None, _) => true,
                (Some(g), Some(p)) => glob(g, p),
                (Some(_), None) => false,
            })
            .map_or(base, |r| r.mode)
    };
    if action.paths.is_empty() {
        for_path(None)
    } else {
        action
            .paths
            .iter()
            .map(|p| for_path(Some(p)))
            .max()
            .unwrap_or(base)
    }
}
/// `*` matches within one path segment, `**` across segments, `?` one char.
pub fn glob(pattern: &str, path: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p {
            [] => s.is_empty(),
            // `**/` spans whole segments only: `**/x.rs` matches `x.rs` and
            // `a/x.rs`, never `abx.rs` (review minor 3).
            ['*', '*', '/', rest @ ..] => {
                (0..=s.len()).any(|k| (k == 0 || s[k - 1] == '/') && go(rest, &s[k..]))
            }
            ['*', '*', rest @ ..] => (0..=s.len()).any(|k| go(rest, &s[k..])),
            ['*', rest @ ..] => (0..=s.len())
                .take_while(|&k| k == 0 || s[k - 1] != '/')
                .any(|k| go(rest, &s[k..])),
            ['?', rest @ ..] => !s.is_empty() && s[0] != '/' && go(rest, &s[1..]),
            [c, rest @ ..] => s.first() == Some(c) && go(rest, &s[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    go(&p, &s)
}
/// Stable name of a verdict, as logged and reported.
pub fn verdict_code(v: &Validation) -> &'static str {
    match v {
        Validation::Valid => "Valid",
        Validation::ValidRecertified { .. } => "ValidRecertified",
        Validation::Empty => "Empty",
        Validation::Pending { .. } => "Pending",
        Validation::Unknown { .. } => "Unknown",
        Validation::Ambiguous { .. } => "Ambiguous",
        Validation::Conflicted { .. } => "Conflicted",
        Validation::Invalid {
            reason: InvalidReason::LogMismatch,
            ..
        } => "Invalid(LogMismatch)",
        Validation::Invalid {
            reason: InvalidReason::StrictChange,
            ..
        } => "Invalid(StrictChange)",
        Validation::Invalid { .. } => "Invalid",
        Validation::Denied => "Denied",
    }
}
/// Severity order of verdicts (ADR 0010 §6.1); `gate` is monotone in it.
pub fn severity(v: &Validation) -> u8 {
    match v {
        Validation::Valid | Validation::ValidRecertified { .. } => 0,
        Validation::Empty => 1,
        Validation::Pending { .. } => 2,
        Validation::Unknown { .. } => 3,
        Validation::Ambiguous { .. } => 4,
        Validation::Conflicted { .. } => 5,
        Validation::Invalid { .. } => 6,
        Validation::Denied => 7,
    }
}
/// The gate decision. `Empty` never allows (I6); only `enforce` denies.
pub fn gate(mode: GateMode, v: &Validation) -> Decision {
    match (mode, severity(v)) {
        (GateMode::Off, _) | (_, 0) => Decision::Allow,
        (GateMode::Enforce, s) if s >= 4 => Decision::Deny,
        _ => Decision::Warn,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn verdicts() -> Vec<Validation> {
        vec![
            Validation::Valid,
            Validation::Empty,
            Validation::Pending { rules: vec![] },
            Validation::Unknown { lag: vec![] },
            Validation::Ambiguous {
                logical: LogicalId(0),
            },
            Validation::Conflicted { entries: vec![] },
            Validation::Invalid {
                reason: InvalidReason::Changed,
                diffs: vec![],
            },
            Validation::Denied,
        ]
    }
    #[test]
    fn decision_table() {
        use Decision::*;
        let expect = [
            (
                GateMode::Warn,
                [Allow, Warn, Warn, Warn, Warn, Warn, Warn, Warn],
            ),
            (
                GateMode::Enforce,
                [Allow, Warn, Warn, Warn, Deny, Deny, Deny, Deny],
            ),
            (GateMode::Off, [Allow; 8]),
        ];
        for (mode, row) in expect {
            let got: Vec<Decision> = verdicts().iter().map(|v| gate(mode, v)).collect();
            assert_eq!(got, row, "{mode:?}");
        }
        // Empty never allows outside Off (I6).
        assert_ne!(gate(GateMode::Warn, &Validation::Empty), Allow);
        assert_ne!(gate(GateMode::Enforce, &Validation::Empty), Allow);
    }
    #[test]
    fn gate_is_monotone_in_severity() {
        for mode in [GateMode::Off, GateMode::Warn, GateMode::Enforce] {
            let vs = verdicts();
            for a in &vs {
                for b in &vs {
                    if severity(a) <= severity(b) {
                        assert!(gate(mode, a) <= gate(mode, b), "{mode:?} {a:?} {b:?}");
                    }
                }
            }
        }
    }
    #[test]
    fn globs() {
        assert!(glob("src/*.rs", "src/lib.rs"));
        assert!(!glob("src/*.rs", "src/a/lib.rs"));
        assert!(glob("src/**/*.rs", "src/a/b/lib.rs"));
        assert!(glob("src/**/*.rs", "src/lib.rs"));
        assert!(glob("**", "a/b"));
        assert!(glob("a?c", "abc") && !glob("a?c", "a/c"));
        assert!(!glob("docs/**", "src/x"));
        assert!(glob("**/x.rs", "x.rs") && glob("**/x.rs", "a/b/x.rs"));
        assert!(!glob("**/x.rs", "src/abx.rs"));
        assert!(glob("src/**/gen/*", "src/gen/a") && !glob("src/**/gen/*", "src/xgen/a"));
    }
    #[test]
    fn modes_resolve_per_path_and_take_the_most_severe() {
        let cfg = GateConfig {
            mode: GateMode::Warn,
            rules: vec![
                GateRule {
                    tool: None,
                    path: Some("src/**".into()),
                    mode: GateMode::Enforce,
                },
                GateRule {
                    tool: Some(ToolClass::Edit),
                    path: Some("src/gen/**".into()),
                    mode: GateMode::Off,
                },
            ],
            ..GateConfig::default()
        };
        let act = |tool, paths: &[&str]| Action {
            tool,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            command: None,
        };
        assert_eq!(
            resolve_mode(&cfg, &act(ToolClass::Edit, &["docs/a.md"])),
            GateMode::Warn
        );
        assert_eq!(
            resolve_mode(&cfg, &act(ToolClass::Edit, &["src/gen/x.rs"])),
            GateMode::Off
        );
        assert_eq!(
            resolve_mode(&cfg, &act(ToolClass::Write, &["src/gen/x.rs"])),
            GateMode::Enforce
        );
        assert_eq!(
            resolve_mode(&cfg, &act(ToolClass::Edit, &["src/gen/x.rs", "src/a.rs"])),
            GateMode::Enforce
        );
        assert_eq!(
            resolve_mode(&cfg, &act(ToolClass::Shell, &[])),
            GateMode::Warn
        );
        let cfg2 = GateConfig::default();
        assert_eq!(GateConfig::from_bytes(&cfg.canonical_bytes()).unwrap(), cfg);
        assert_eq!(
            resolve_mode(&cfg2, &act(ToolClass::Patch, &[])),
            GateMode::Warn
        );
    }
}
