//! `<data>/ctxd.toml` (ADR 0019 §3.7, §8, owner decisions O1–O3). Optional:
//! a missing file is the defaults; an invalid one refuses to start.
//!
//! ```toml
//! [maintenance]
//! lazy = true            # acknowledge a cascade within the budget (O1)
//! lazy_budget = 1000000  # E, work units per commit and per idle slice
//! read_settle_ms = 50    # how long a certified read waits for a pull (O2)
//!
//! [gc]
//! keep_epochs = 10000    # payload GC keeps what the last N epochs named (O3)
//! ```
use contextdb_core::{Budget, LAZY_BUDGET};
use std::{path::Path, time::Duration};

pub const FILE: &str = "ctxd.toml";
/// `[gc] keep_epochs` by default, and its floor (O3).
pub const KEEP_EPOCHS: u64 = 10_000;
pub const KEEP_EPOCHS_FLOOR: u64 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Maintenance {
    pub lazy: bool,
    pub lazy_budget: u64,
    pub read_settle: Duration,
}
impl Default for Maintenance {
    fn default() -> Self {
        Self {
            lazy: true,
            lazy_budget: LAZY_BUDGET,
            read_settle: crate::actor::READ_SETTLE,
        }
    }
}
impl Maintenance {
    /// The store's `OpenOptions::lazy`.
    pub fn budget(&self) -> Option<Budget> {
        self.lazy.then(|| Budget::new(self.lazy_budget))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gc {
    pub keep_epochs: u64,
}
impl Default for Gc {
    fn default() -> Self {
        Self {
            keep_epochs: KEEP_EPOCHS,
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub maintenance: Maintenance,
    pub gc: Gc,
}
impl Config {
    /// Reads `<dir>/ctxd.toml`; a missing file is the default.
    pub fn load(dir: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(dir.join(FILE)) {
            Ok(text) => Self::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{FILE}: {e}")),
        }
    }
    pub fn parse(text: &str) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e| format!("{FILE}: {e}"))?;
        let mut config = Self::default();
        let mut sections = Vec::new();
        for (key, v) in &table {
            let known: &[&str] = match key.as_str() {
                "maintenance" => &["lazy", "lazy_budget", "read_settle_ms"],
                "gc" => &["keep_epochs"],
                _ => return Err(format!("{FILE}: unknown section `{key}`")),
            };
            let t = (v.as_table()).ok_or(format!("{FILE}: {key} must be a table"))?;
            if let Some(k) = t.keys().find(|k| !known.contains(&k.as_str())) {
                return Err(format!("{FILE}: unknown {key} setting `{k}`"));
            }
            sections.push((key.as_str(), t));
        }
        for (section, t) in sections {
            let int = |k: &str, min: i64, max: i64| -> Result<Option<u64>, String> {
                match t.get(k) {
                    None => Ok(None),
                    Some(v) => match v.as_integer() {
                        Some(n) if (min..=max).contains(&n) => Ok(Some(n as u64)),
                        _ => Err(format!(
                            "{FILE}: {section}.{k} must be an integer in {min}..={max}"
                        )),
                    },
                }
            };
            if section == "gc" {
                if let Some(n) = int("keep_epochs", KEEP_EPOCHS_FLOOR as i64, i64::MAX)? {
                    config.gc.keep_epochs = n;
                }
                continue;
            }
            let c = &mut config.maintenance;
            if let Some(v) = t.get("lazy") {
                c.lazy = (v.as_bool())
                    .ok_or(format!("{FILE}: maintenance.lazy must be true or false"))?;
            }
            if let Some(n) = int("lazy_budget", 1, i64::MAX)? {
                c.lazy_budget = n;
            }
            if let Some(n) = int("read_settle_ms", 0, 60_000)? {
                c.read_settle = Duration::from_millis(n);
            }
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_every_setting() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(
            Config::default().maintenance.budget(),
            Some(Budget::new(LAZY_BUDGET))
        );
        let c =
            Config::parse("[maintenance]\nlazy = false\nlazy_budget = 500\nread_settle_ms = 10\n")
                .unwrap()
                .maintenance;
        assert_eq!(
            c,
            Maintenance {
                lazy: false,
                lazy_budget: 500,
                read_settle: Duration::from_millis(10),
            }
        );
        assert_eq!(c.budget(), None);
        assert_eq!(Config::default().gc.keep_epochs, KEEP_EPOCHS);
        let g = Config::parse("[gc]\nkeep_epochs = 100\n").unwrap();
        assert_eq!(g.gc.keep_epochs, KEEP_EPOCHS_FLOOR);
        assert_eq!(g.maintenance, Maintenance::default());
    }
    #[test]
    fn invalid_files_are_refused() {
        for (text, needle) in [
            ("[maintenance", "ctxd.toml"),
            ("[other]\n", "unknown section `other`"),
            ("maintenance = 1\n", "must be a table"),
            ("[maintenance]\nlazy = 1\n", "maintenance.lazy"),
            (
                "[maintenance]\nlazy_budget = 0\n",
                "maintenance.lazy_budget",
            ),
            (
                "[maintenance]\nread_settle_ms = -1\n",
                "maintenance.read_settle_ms",
            ),
            (
                "[maintenance]\nbudget = 5\n",
                "unknown maintenance setting `budget`",
            ),
            ("[gc]\nkeep_epochs = 99\n", "gc.keep_epochs"),
            ("[gc]\nkeep = 500\n", "unknown gc setting `keep`"),
            ("gc = 1\n", "gc must be a table"),
        ] {
            let e = Config::parse(text).unwrap_err();
            assert!(e.contains(needle), "{text:?}: {e}");
        }
    }
}
