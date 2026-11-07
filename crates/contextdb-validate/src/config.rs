//! `<data>/validators.toml` (ADR 0015 §4.4): the worker limits and the
//! configured validators. Operational configuration only: it never enters
//! derived state. No validator is configured by default, so nothing is
//! called until the owner writes one.
//!
//! ```toml
//! [limits]                      # all optional; these are the defaults
//! workers = 2
//! lease_secs = 120
//! attempts = 3
//! llm_calls_per_hour = 200
//!
//! [candidates]                  # lexical contradiction candidates (§5)
//! min_score = 1.0               # bm25 floor; initial, calibrated in G6
//! per_head = 5                  # most pairs per new head
//!
//! [[validator]]
//! name = "claude"
//! type = "claude"               # claude | command
//! model = "claude-haiku-5-5"
//! program = "claude"            # optional, default: claude on PATH
//! token_file = "~/.config/contextdb/claude-token"   # optional
//! branches = ["main"]           # optional filters on the job
//! kinds = ["Decision", "Plan"]  # the conclusion's kind
//! scopes = ["repo"]             # the conclusion's scope lane
//! pairs = true                  # also judge contradiction candidates
//!                               # (filters match the newer side)
//!
//! [[validator]]
//! name = "lint"
//! type = "command"
//! command = ["/usr/local/bin/ctx-lint", "--json"]
//! trusted = true                # Deterministic; otherwise counted as Llm
//! local = true                  # keeps data on this machine (ADR 0017 §7)
//! ```
//!
//! A job goes to the first validator whose filters all match; a candidate
//! pair to the first with `pairs = true` whose filters match its newer side.
//! A job or pair touching a lane whose egress is off goes only to a
//! validator marked `local = true` (the owner's statement that the command
//! keeps data on the machine; `claude` validators cannot be marked).
use crate::{Validator, claude::ClaudeCli, command::CommandValidator};
use contextdb_model::RecordKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const FILE: &str = "validators.toml";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    pub workers: usize,
    pub lease_secs: u64,
    pub attempts: u32,
    pub llm_calls_per_hour: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            workers: 2,
            lease_secs: 120,
            attempts: 3,
            llm_calls_per_hour: 200,
        }
    }
}
/// Lexical contradiction candidates (ADR 0015 §5). They run whether or not
/// a validator is configured: they only list pairs and call nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CandidateLimits {
    /// Pairs scoring below this bm25 score are dropped (initial value,
    /// calibrated in G6).
    pub min_score: f64,
    /// Most pairs kept per new head.
    pub per_head: usize,
}
impl Default for CandidateLimits {
    fn default() -> Self {
        Self {
            min_score: 1.0,
            per_head: 5,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adapter {
    Claude {
        program: PathBuf,
        model: String,
        token_file: Option<PathBuf>,
        isolated_login: Option<PathBuf>,
    },
    Command {
        program: PathBuf,
        args: Vec<String>,
        trusted: bool,
    },
}
/// Which jobs a validator takes; an empty list matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub branches: Vec<String>,
    pub kinds: Vec<RecordKind>,
    pub scopes: Vec<String>,
}
impl Filter {
    pub fn matches(&self, branch: &str, kind: RecordKind, scope: &str) -> bool {
        (self.branches.is_empty() || self.branches.iter().any(|b| b == branch))
            && (self.kinds.is_empty() || self.kinds.contains(&kind))
            && (self.scopes.is_empty() || self.scopes.iter().any(|s| s == scope))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub adapter: Adapter,
    pub filter: Filter,
    /// Also judges contradiction candidates (ADR 0015 §5); off by default.
    pub pairs: bool,
    /// The owner's statement that the command keeps data on this machine:
    /// it may receive bodies of lanes whose egress is off (ADR 0017 §7).
    pub local: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub limits: Limits,
    pub candidates: CandidateLimits,
    pub validators: Vec<Entry>,
}
fn kind_of(s: &str) -> Option<RecordKind> {
    use RecordKind::*;
    [
        Fact, Assumption, Decision, Plan, Convention, Procedure, Artifact, Evidence, CodeAnchor,
        Summary,
    ]
    .into_iter()
    .find(|k| format!("{k:?}") == s)
}
/// `~/x` against `home`; other paths unchanged.
fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(path),
    }
}
impl Config {
    /// Reads `<dir>/validators.toml`; a missing file is the empty default.
    pub fn load(dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(dir.join(FILE)) {
            Ok(text) => Self::parse(
                &text,
                std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{FILE}: {e}")),
        }
    }
    pub fn parse(text: &str, home: Option<&Path>) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e| format!("{FILE}: {e}"))?;
        let mut config = Self::default();
        for key in table.keys() {
            if !["limits", "candidates", "validator"].contains(&key.as_str()) {
                return Err(format!("{FILE}: unknown section `{key}`"));
            }
        }
        if let Some(l) = table.get("limits") {
            let l = l.as_table().ok_or("limits must be a table")?;
            let int = |k: &str, min: i64, max: i64| -> Result<Option<i64>, String> {
                match l.get(k) {
                    None => Ok(None),
                    Some(v) => match v.as_integer() {
                        Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
                        _ => Err(format!("limits.{k} must be an integer in {min}..={max}")),
                    },
                }
            };
            for k in l.keys() {
                if !["workers", "lease_secs", "attempts", "llm_calls_per_hour"]
                    .contains(&k.as_str())
                {
                    return Err(format!("unknown limit `{k}`"));
                }
            }
            if let Some(n) = int("workers", 1, 64)? {
                config.limits.workers = n as usize;
            }
            if let Some(n) = int("lease_secs", 5, 3600)? {
                config.limits.lease_secs = n as u64;
            }
            if let Some(n) = int("attempts", 1, 100)? {
                config.limits.attempts = n as u32;
            }
            if let Some(n) = int("llm_calls_per_hour", 0, 100_000)? {
                config.limits.llm_calls_per_hour = n as u32;
            }
        }
        if let Some(c) = table.get("candidates") {
            let c = c.as_table().ok_or("candidates must be a table")?;
            for (k, v) in c {
                match k.as_str() {
                    "min_score" => {
                        let n = (v.as_float())
                            .or_else(|| v.as_integer().map(|i| i as f64))
                            .filter(|n| n.is_finite() && *n >= 0.0)
                            .ok_or("candidates.min_score must be a number >= 0")?;
                        config.candidates.min_score = n;
                    }
                    "per_head" => {
                        let n = (v.as_integer())
                            .filter(|n| (0..=50).contains(n))
                            .ok_or("candidates.per_head must be an integer in 0..=50")?;
                        config.candidates.per_head = n as usize;
                    }
                    _ => return Err(format!("unknown candidates setting `{k}`")),
                }
            }
        }
        let empty = Vec::new();
        let list = match table.get("validator") {
            None => &empty,
            Some(v) => v.as_array().ok_or("validator must be an array of tables")?,
        };
        for v in list {
            let t = v.as_table().ok_or("each validator must be a table")?;
            let s = |k: &str| t.get(k).and_then(|x| x.as_str());
            let strings = |k: &str| -> Result<Vec<String>, String> {
                match t.get(k) {
                    None => Ok(Vec::new()),
                    Some(x) => (x.as_array().ok_or(format!("{k} must be a list"))?.iter())
                        .map(|e| {
                            e.as_str()
                                .map(str::to_owned)
                                .ok_or(format!("{k}: strings only"))
                        })
                        .collect(),
                }
            };
            let name = s("name").ok_or("validator without a name")?.to_owned();
            if config.validators.iter().any(|e| e.name == name) {
                return Err(format!("validator `{name}` is listed twice"));
            }
            let allowed: &[&str] = match s("type") {
                Some("claude") => &[
                    "name",
                    "type",
                    "model",
                    "program",
                    "token_file",
                    "isolated_login",
                    "branches",
                    "kinds",
                    "scopes",
                    "pairs",
                ],
                Some("command") => &[
                    "name", "type", "command", "trusted", "local", "branches", "kinds", "scopes",
                    "pairs",
                ],
                _ => {
                    return Err(format!(
                        "validator `{name}`: type must be claude or command"
                    ));
                }
            };
            for k in t.keys() {
                if k == "local" && s("type") == Some("claude") {
                    return Err(format!(
                        "validator `{name}`: a claude validator sends its task to a remote model \
                         and cannot be marked local"
                    ));
                }
                if !allowed.contains(&k.as_str()) {
                    return Err(format!("validator `{name}`: unknown field `{k}`"));
                }
            }
            let adapter = if s("type") == Some("claude") {
                Adapter::Claude {
                    program: expand(s("program").unwrap_or("claude"), home),
                    model: s("model")
                        .ok_or(format!("validator `{name}`: model is required"))?
                        .to_owned(),
                    token_file: s("token_file").map(|p| expand(p, home)),
                    isolated_login: s("isolated_login").map(|p| expand(p, home)),
                }
            } else {
                let mut command = strings("command")?.into_iter();
                let program = command
                    .next()
                    .ok_or(format!("validator `{name}`: command is required"))?;
                let trusted = match t.get("trusted") {
                    None => false,
                    Some(b) => b
                        .as_bool()
                        .ok_or(format!("validator `{name}`: trusted is a bool"))?,
                };
                Adapter::Command {
                    program: expand(&program, home),
                    args: command.collect(),
                    trusted,
                }
            };
            let pairs = match t.get("pairs") {
                None => false,
                Some(b) => b
                    .as_bool()
                    .ok_or(format!("validator `{name}`: pairs is a bool"))?,
            };
            let local = match t.get("local") {
                None => false,
                Some(b) => b
                    .as_bool()
                    .ok_or(format!("validator `{name}`: local is a bool"))?,
            };
            let kinds = (strings("kinds")?.iter())
                .map(|k| kind_of(k).ok_or(format!("validator `{name}`: unknown kind `{k}`")))
                .collect::<Result<_, _>>()?;
            config.validators.push(Entry {
                name,
                adapter,
                filter: Filter {
                    branches: strings("branches")?,
                    kinds,
                    scopes: strings("scopes")?,
                },
                pairs,
                local,
            });
        }
        Ok(config)
    }
}
impl Entry {
    /// The adapter, with `version` for the metadata (the `claude` version
    /// string, or the configured program's).
    pub fn build(&self, version: &str) -> Arc<dyn Validator> {
        match &self.adapter {
            Adapter::Claude {
                program,
                model,
                token_file,
                isolated_login,
            } => Arc::new(ClaudeCli {
                name: self.name.clone(),
                program: program.clone(),
                model: model.clone(),
                version: version.into(),
                isolated_login: isolated_login.clone(),
                token_file: token_file.clone(),
            }),
            Adapter::Command {
                program,
                args,
                trusted,
            } => Arc::new(CommandValidator {
                name: self.name.clone(),
                version: version.into(),
                program: program.clone(),
                args: args.clone(),
                trusted: *trusted,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_no_validator_and_default_limits() {
        let c = Config::parse("", None).unwrap();
        assert!(c.validators.is_empty());
        assert_eq!(c.limits, Limits::default());
        assert_eq!(c.candidates, CandidateLimits::default());
    }

    #[test]
    fn parses_candidate_limits() {
        let c = Config::parse("[candidates]\nmin_score = 2\nper_head = 0", None).unwrap();
        assert_eq!(
            c.candidates,
            CandidateLimits {
                min_score: 2.0,
                per_head: 0
            }
        );
        for bad in [
            "[candidates]\nmin_score = -1.0",
            "[candidates]\nper_head = 51",
            "[candidates]\nlimit = 1",
            "candidates = 1",
        ] {
            assert!(Config::parse(bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn parses_both_adapters_and_filters() {
        let text = r#"
            [limits]
            workers = 4
            llm_calls_per_hour = 10
            [[validator]]
            name = "claude"
            type = "claude"
            model = "claude-haiku-5-5"
            token_file = "~/.config/contextdb/claude-token"
            kinds = ["Decision"]
            pairs = true
            [[validator]]
            name = "lint"
            type = "command"
            command = ["/bin/lint", "--json"]
            trusted = true
            branches = ["main"]
        "#;
        let c = Config::parse(text, Some(Path::new("/home/u"))).unwrap();
        assert_eq!(c.limits.workers, 4);
        assert_eq!(c.limits.lease_secs, 120);
        assert_eq!(
            c.validators[0].adapter,
            Adapter::Claude {
                program: "claude".into(),
                model: "claude-haiku-5-5".into(),
                token_file: Some("/home/u/.config/contextdb/claude-token".into()),
                isolated_login: None,
            }
        );
        assert!(
            c.validators[0]
                .filter
                .matches("x", RecordKind::Decision, "repo")
        );
        assert!(c.validators[0].pairs && !c.validators[1].pairs);
        assert!(
            !c.validators[0]
                .filter
                .matches("x", RecordKind::Plan, "repo")
        );
        assert_eq!(
            c.validators[1].adapter,
            Adapter::Command {
                program: "/bin/lint".into(),
                args: vec!["--json".into()],
                trusted: true
            }
        );
        assert!(
            !c.validators[1]
                .filter
                .matches("dev", RecordKind::Plan, "repo")
        );
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        for bad in [
            "[other]",
            "[limits]\nworkers = 0",
            "[limits]\nspeed = 1",
            "[[validator]]\nname = \"a\"\ntype = \"llm\"",
            "[[validator]]\nname = \"a\"\ntype = \"claude\"",
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = []",
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\nmodel = \"m\"",
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\nkinds = [\"Note\"]",
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\npairs = 1",
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\n[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"y\"]",
        ] {
            assert!(Config::parse(bad, None).is_err(), "{bad}");
        }
    }

    /// `local` (ADR 0017 §7): commands only, a bool, off by default.
    #[test]
    fn local_is_for_commands_only() {
        let c = Config::parse(
            "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\nlocal = true\n\
             [[validator]]\nname = \"b\"\ntype = \"command\"\ncommand = [\"y\"]",
            None,
        )
        .unwrap();
        assert!(c.validators[0].local && !c.validators[1].local);
        let e = Config::parse(
            "[[validator]]\nname = \"c\"\ntype = \"claude\"\nmodel = \"m\"\nlocal = true",
            None,
        )
        .unwrap_err();
        assert!(e.contains("cannot be marked local"), "{e}");
        assert!(
            Config::parse(
                "[[validator]]\nname = \"a\"\ntype = \"command\"\ncommand = [\"x\"]\nlocal = 1",
                None
            )
            .is_err()
        );
    }
}
