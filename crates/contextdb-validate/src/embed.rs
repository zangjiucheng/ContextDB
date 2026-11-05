//! The embedder adapter (ADR 0016 §7.1): an external command that turns
//! texts into vectors, configured in `<data>/embeddings.toml`. Off by
//! default: without the file there is no embedder, and the vector
//! projection is `Disabled`.
//!
//! ```toml
//! [embedder]
//! command = ["/path/to/embed", "--model", "x"]  # JSON lines in, JSON lines out
//! model = "x"                                    # the cache key
//! dims = 1024
//! max_batch = 32                                 # optional, these are the defaults
//! timeout_secs = 30
//!
//! [vectors]                                      # optional
//! max_cache_mb = 256                             # in-memory vector cache (E2)
//! min_cosine = 0.82                              # embedding candidates; calibrated in G6
//! per_head = 5                                   # most embedding pairs per new head
//! ```
//!
//! `type = "mock"` in `[embedder]` (with `dims`, and no `command`) selects
//! the deterministic [`Mock`]: a hashed bag of words. It is for tests and
//! local trials only and never counts as a real embedder (G6).
//!
//! **Protocol.** Each input line is `{"id": n, "text": "..."}`; each output
//! line is `{"id": n, "vector": [f32; dims]}`. A wrong dimension, a
//! non-finite value, a missing, repeated or unknown id, or more than
//! [`MAX_OUTPUT_BYTES`] of output fails the whole batch as `Malformed`.
//!
//! **Process.** The child runs through [`crate::run_capped`]: its own process
//! group, a deadline, the pipe readers abandoned after a grace. It gets a
//! minimal environment (`HOME`, `PATH`, `LANG`) and an empty private (0700)
//! working directory, removed afterwards, because it receives record bodies
//! (SPEC §10).
use crate::{ValidatorError, claude::TempDir, run_capped};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

pub const FILE: &str = "embeddings.toml";
/// Most output one batch may produce.
pub const MAX_OUTPUT_BYTES: usize = 4 << 20;
/// Most text bytes sent per record.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;
/// Largest dimension accepted.
pub const MAX_DIMS: usize = 8192;

pub trait Embedder: Send + Sync {
    /// The model name: the vector cache key.
    fn model(&self) -> &str;
    fn dims(&self) -> usize;
    /// One vector per text, in order, within `deadline`.
    fn embed(&self, texts: &[String], deadline: Instant) -> Result<Vec<Vec<f32>>, ValidatorError>;
}

/// The text embedded for one record: abstract, outline and body, cut at
/// [`MAX_TEXT_BYTES`] on a character boundary.
pub fn record_text(abstract_: &str, outline: &str, body: &str) -> String {
    let mut s = String::with_capacity(MAX_TEXT_BYTES);
    for part in [abstract_, outline, body] {
        if part.is_empty() {
            continue;
        }
        if !s.is_empty() {
            s.push_str("\n\n");
        }
        s.push_str(part);
        if s.len() >= MAX_TEXT_BYTES {
            break;
        }
    }
    cut(&mut s, MAX_TEXT_BYTES);
    s
}
fn cut(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Scales `v` to unit length; a zero vector stays zero.
pub fn normalise(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 && n.is_finite() {
        v.iter_mut().for_each(|x| *x /= n);
    }
}
/// The dot product: the cosine of two unit vectors.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A deterministic embedder for tests: each lower-cased alphanumeric word
/// adds ±1 at a position chosen by its hash, and the sum is normalised.
pub struct Mock {
    pub model: String,
    pub dims: usize,
}
impl Mock {
    pub fn new(dims: usize) -> Self {
        Self {
            model: format!("mock-bow-{dims}"),
            dims,
        }
    }
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dims];
        for word in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
        {
            let h = blake3::hash(word.to_lowercase().as_bytes());
            let b = h.as_bytes();
            let i = u64::from_le_bytes(b[..8].try_into().expect("8")) as usize % self.dims;
            v[i] += if b[8] & 1 == 0 { 1.0 } else { -1.0 };
        }
        normalise(&mut v);
        v
    }
}
impl Embedder for Mock {
    fn model(&self) -> &str {
        &self.model
    }
    fn dims(&self) -> usize {
        self.dims
    }
    fn embed(&self, texts: &[String], _: Instant) -> Result<Vec<Vec<f32>>, ValidatorError> {
        Ok(texts.iter().map(|t| self.vector(t)).collect())
    }
}

/// The external-command embedder.
pub struct CommandEmbedder {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub model: String,
    pub dims: usize,
}
impl Embedder for CommandEmbedder {
    fn model(&self) -> &str {
        &self.model
    }
    fn dims(&self) -> usize {
        self.dims
    }
    fn embed(&self, texts: &[String], deadline: Instant) -> Result<Vec<Vec<f32>>, ValidatorError> {
        let mut input = String::new();
        for (id, text) in texts.iter().enumerate() {
            input.push_str(&serde_json::json!({ "id": id, "text": text }).to_string());
            input.push('\n');
        }
        let work = TempDir::new("embed")?;
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).current_dir(&work.0).env_clear();
        for k in ["HOME", "PATH", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        let (ok, stdout, stderr) =
            run_capped(&mut cmd, input.as_bytes(), deadline, MAX_OUTPUT_BYTES)?;
        if !ok {
            return Err(ValidatorError::Failed(crate::failure(&stdout, &stderr)));
        }
        parse_output(&stdout, texts.len(), self.dims)
    }
}

/// Parses and checks an embedder's output for a batch of `n` texts.
pub fn parse_output(stdout: &[u8], n: usize, dims: usize) -> Result<Vec<Vec<f32>>, ValidatorError> {
    let bad = |m: String| ValidatorError::Malformed(m);
    let text = std::str::from_utf8(stdout).map_err(|_| bad("output is not UTF-8".into()))?;
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value =
            serde_json::from_str(line).map_err(|e| bad(format!("json: {e}")))?;
        let obj = v
            .as_object()
            .ok_or_else(|| bad("a line is not an object".into()))?;
        if let Some(k) = obj.keys().find(|k| !matches!(k.as_str(), "id" | "vector")) {
            return Err(bad(format!("unexpected key {k}")));
        }
        let id = (obj.get("id").and_then(serde_json::Value::as_u64))
            .ok_or_else(|| bad("id".into()))? as usize;
        let slot = out
            .get_mut(id)
            .ok_or_else(|| bad(format!("unknown id {id}")))?;
        if slot.is_some() {
            return Err(bad(format!("id {id} repeated")));
        }
        let vector = (obj.get("vector").and_then(serde_json::Value::as_array))
            .ok_or_else(|| bad("vector".into()))?;
        if vector.len() != dims {
            return Err(bad(format!("id {id}: {} dims, not {dims}", vector.len())));
        }
        let mut vec = Vec::with_capacity(dims);
        for x in vector {
            let f = x.as_f64().map(|f| f as f32).filter(|f| f.is_finite());
            vec.push(f.ok_or_else(|| bad(format!("id {id}: a value is not a finite number")))?);
        }
        normalise(&mut vec);
        *slot = Some(vec);
    }
    out.into_iter()
        .enumerate()
        .map(|(id, v)| v.ok_or_else(|| bad(format!("id {id} missing"))))
        .collect()
}

/// `[vectors]`: search and candidate settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VectorLimits {
    pub max_cache_mb: usize,
    /// Embedding candidates below this cosine are dropped (initial value,
    /// calibrated in G6).
    pub min_cosine: f32,
    /// Most embedding pairs kept per new head.
    pub per_head: usize,
}
impl Default for VectorLimits {
    fn default() -> Self {
        Self {
            max_cache_mb: 256,
            min_cosine: 0.82,
            per_head: 5,
        }
    }
}
/// What `[embedder]` selects.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Command { program: PathBuf, args: Vec<String> },
    Mock,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub source: Source,
    pub model: String,
    pub dims: usize,
    pub max_batch: usize,
    pub timeout: Duration,
    pub limits: VectorLimits,
}
impl Config {
    /// Reads `<dir>/embeddings.toml`; `None` when the file does not exist.
    pub fn load(dir: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(dir.join(FILE)) {
            Ok(text) => Self::parse(
                &text,
                std::env::var_os("HOME").map(PathBuf::from).as_deref(),
            )
            .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{FILE}: {e}")),
        }
    }
    pub fn parse(text: &str, home: Option<&Path>) -> Result<Self, String> {
        let table: toml::Table = text.parse().map_err(|e| format!("{FILE}: {e}"))?;
        if let Some(k) = table
            .keys()
            .find(|k| !["embedder", "vectors"].contains(&k.as_str()))
        {
            return Err(format!("{FILE}: unknown section `{k}`"));
        }
        let e = (table.get("embedder"))
            .and_then(toml::Value::as_table)
            .ok_or(format!("{FILE}: an [embedder] table is required"))?;
        let known = [
            "type",
            "command",
            "model",
            "dims",
            "max_batch",
            "timeout_secs",
        ];
        if let Some(k) = e.keys().find(|k| !known.contains(&k.as_str())) {
            return Err(format!("{FILE}: unknown key embedder.{k}"));
        }
        let int = |t: &toml::Table, sec: &str, k: &str, min: i64, max: i64| match t.get(k) {
            None => Ok(None),
            Some(v) => match v.as_integer() {
                Some(n) if (min..=max).contains(&n) => Ok(Some(n as usize)),
                _ => Err(format!(
                    "{FILE}: {sec}.{k} must be an integer in {min}..={max}"
                )),
            },
        };
        let dims = int(e, "embedder", "dims", 1, MAX_DIMS as i64)?
            .ok_or(format!("{FILE}: embedder.dims is required"))?;
        let kind = e
            .get("type")
            .map(|v| v.as_str().unwrap_or("?"))
            .unwrap_or("command");
        let (source, model) = match kind {
            "mock" => {
                if e.contains_key("command") {
                    return Err(format!("{FILE}: a mock embedder takes no command"));
                }
                (Source::Mock, Mock::new(dims).model)
            }
            "command" => {
                let argv: Vec<String> = (e.get("command").and_then(toml::Value::as_array))
                    .ok_or(format!("{FILE}: embedder.command is required"))?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<_>>()
                    .filter(|a: &Vec<String>| !a.is_empty())
                    .ok_or(format!(
                        "{FILE}: embedder.command must be a non-empty list of strings"
                    ))?;
                let model = (e.get("model").and_then(toml::Value::as_str))
                    .filter(|m| !m.is_empty())
                    .ok_or(format!("{FILE}: embedder.model is required"))?
                    .to_owned();
                let program = match (argv[0].strip_prefix("~/"), home) {
                    (Some(rest), Some(h)) => h.join(rest),
                    _ => PathBuf::from(&argv[0]),
                };
                (
                    Source::Command {
                        program,
                        args: argv[1..].to_vec(),
                    },
                    model,
                )
            }
            other => return Err(format!("{FILE}: unknown embedder type `{other}`")),
        };
        let mut limits = VectorLimits::default();
        if let Some(v) = table.get("vectors") {
            let v = v
                .as_table()
                .ok_or(format!("{FILE}: vectors must be a table"))?;
            for k in v.keys() {
                if !["max_cache_mb", "min_cosine", "per_head"].contains(&k.as_str()) {
                    return Err(format!("{FILE}: unknown key vectors.{k}"));
                }
            }
            if let Some(n) = int(v, "vectors", "max_cache_mb", 0, 1 << 20)? {
                limits.max_cache_mb = n;
            }
            if let Some(n) = int(v, "vectors", "per_head", 0, 100)? {
                limits.per_head = n;
            }
            if let Some(x) = v.get("min_cosine") {
                limits.min_cosine = (x.as_float())
                    .or_else(|| x.as_integer().map(|i| i as f64))
                    .filter(|f| (-1.0..=1.0).contains(f))
                    .ok_or(format!(
                        "{FILE}: vectors.min_cosine must be a number in -1..=1"
                    ))? as f32;
            }
        }
        Ok(Self {
            source,
            model,
            dims,
            max_batch: int(e, "embedder", "max_batch", 1, 1024)?.unwrap_or(32),
            timeout: Duration::from_secs(
                int(e, "embedder", "timeout_secs", 1, 600)?.unwrap_or(30) as u64
            ),
            limits,
        })
    }
    /// The configured embedder.
    pub fn embedder(&self) -> Box<dyn Embedder> {
        match &self.source {
            Source::Mock => Box::new(Mock::new(self.dims)),
            Source::Command { program, args } => Box::new(CommandEmbedder {
                program: program.clone(),
                args: args.clone(),
                model: self.model.clone(),
                dims: self.dims,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perl(script: &str, dims: usize) -> CommandEmbedder {
        CommandEmbedder {
            program: "/usr/bin/perl".into(),
            args: vec!["-e".into(), script.into()],
            model: "t".into(),
            dims,
        }
    }
    fn texts(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("text {i}")).collect()
    }
    fn soon(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }
    /// Echoes `{"id": n, "vector": [1, 0, …]}` for each input line.
    const ECHO: &str =
        r#"while (<STDIN>) { /"id":(\d+)/; print qq({"id":$1,"vector":[1,0,0]}\n) }"#;

    #[test]
    fn the_mock_is_deterministic_normalised_and_word_based() {
        let m = Mock::new(64);
        let a = m.vector("Use the otter queue");
        assert_eq!(a, m.vector("use THE otter, queue!"));
        assert!((dot(&a, &a) - 1.0).abs() < 1e-5);
        let b = m.vector("otter queue");
        let c = m.vector("penguin ledger");
        assert!(dot(&a, &b) > dot(&a, &c));
        assert_eq!(m.vector(""), vec![0.0; 64]);
    }

    #[test]
    fn a_command_embedder_round_trips_and_normalises() {
        let e = perl(
            r#"while (<STDIN>) { /"id":(\d+)/; print qq({"id":$1,"vector":[3,4,0]}\n) }"#,
            3,
        );
        let out = e.embed(&texts(3), soon(20)).unwrap();
        assert_eq!(out.len(), 3);
        assert!((out[0][0] - 0.6).abs() < 1e-6 && (out[0][1] - 0.8).abs() < 1e-6);
        assert!(perl(ECHO, 3).embed(&texts(2), soon(20)).is_ok());
    }

    /// T89: every failure is an error for the batch, within the deadline.
    #[test]
    fn failures_fail_the_batch() {
        let malformed = |r: Result<Vec<Vec<f32>>, ValidatorError>| {
            assert!(matches!(r, Err(ValidatorError::Malformed(_))), "{r:?}")
        };
        // Wrong dimension.
        malformed(perl(ECHO, 4).embed(&texts(1), soon(20)));
        // A non-number, an overflowing number, a missing id, a repeated id.
        for out in [
            r#"{"id":0,"vector":[NaN,0,0]}"#,
            r#"{"id":0,"vector":[1e999,0,0]}"#,
            r#"{"id":0,"vector":[1,0,0]}"#,
            r#"{"id":0,"vector":[1,0,0]}\n{"id":0,"vector":[1,0,0]}"#,
            r#"{"id":5,"vector":[1,0,0]}"#,
            r#"{"id":0,"vector":[1,0,0],"x":1}"#,
        ] {
            malformed(parse_output(out.replace("\\n", "\n").as_bytes(), 2, 3));
        }
        // Oversize output.
        let t = Instant::now();
        malformed(perl("print 'x' x (8 << 20)", 3).embed(&texts(1), soon(20)));
        assert!(t.elapsed() < Duration::from_secs(10));
        // A hang hits the deadline.
        let t = Instant::now();
        let r = perl("sleep 30", 3).embed(&texts(1), Instant::now() + Duration::from_millis(300));
        assert!(matches!(r, Err(ValidatorError::Timeout)), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
        // A grandchild holding the pipes.
        let t = Instant::now();
        let r = perl(
            "use POSIX; if (fork() == 0) { POSIX::setsid(); sleep 20; exit 0 } exit 0",
            3,
        )
        .embed(&texts(1), soon(20));
        assert!(matches!(r, Err(ValidatorError::Failed(_))), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(5));
        // A nonzero exit.
        let r = perl("exit 3", 3).embed(&texts(1), soon(20));
        assert!(matches!(r, Err(ValidatorError::Failed(_))), "{r:?}");
    }

    /// T89: the child gets only HOME, PATH and LANG, and an empty private
    /// working directory.
    #[test]
    fn the_embedder_gets_a_minimal_environment() {
        let script = r#"
            use Cwd; my @extra = grep { !/^(HOME|PATH|LANG)$/ } keys %ENV;
            opendir(my $d, getcwd()) or die; my @f = grep { !/^\.\.?$/ } readdir($d);
            my $mode = (stat(getcwd()))[2] & 07777;
            my $ok = (!@extra && !@f && $mode == 0700) ? 1 : 0;
            while (<STDIN>) { /"id":(\d+)/; print qq({"id":$1,"vector":[$ok,1,0]}\n) }"#;
        let out = perl(script, 3).embed(&texts(1), soon(20)).unwrap();
        assert!(
            out[0][0] > 0.5,
            "extra environment, files or mode in the cwd"
        );
    }

    #[test]
    fn record_text_is_cut_on_a_char_boundary() {
        let body = "é".repeat(MAX_TEXT_BYTES);
        let t = record_text("abs", "", &body);
        assert!(t.len() <= MAX_TEXT_BYTES && t.starts_with("abs\n\né"));
        assert_eq!(record_text("", "", "x"), "x");
    }

    #[test]
    fn config_parses_and_rejects() {
        let c = Config::parse(
            "[embedder]\ncommand = [\"~/bin/embed\", \"--m\"]\nmodel = \"m1\"\ndims = 8\n[vectors]\nmin_cosine = 0.9\n",
            Some(Path::new("/h")),
        )
        .unwrap();
        assert_eq!(
            c.source,
            Source::Command {
                program: "/h/bin/embed".into(),
                args: vec!["--m".into()]
            }
        );
        assert_eq!((c.model.as_str(), c.dims, c.max_batch), ("m1", 8, 32));
        assert_eq!(c.timeout, Duration::from_secs(30));
        assert!((c.limits.min_cosine - 0.9).abs() < 1e-6);
        let m = Config::parse("[embedder]\ntype = \"mock\"\ndims = 16\n", None).unwrap();
        assert_eq!((m.source, m.model.as_str()), (Source::Mock, "mock-bow-16"));
        for bad in [
            "",
            "[embedder]\nmodel = \"m\"\ndims = 8\n",
            "[embedder]\ncommand = [\"x\"]\ndims = 8\n",
            "[embedder]\ncommand = [\"x\"]\nmodel = \"m\"\n",
            "[embedder]\ncommand = [\"x\"]\nmodel = \"m\"\ndims = 0\n",
            "[embedder]\ncommand = []\nmodel = \"m\"\ndims = 8\n",
            "[embedder]\ntype = \"mock\"\ndims = 8\ncommand = [\"x\"]\n",
            "[embedder]\ntype = \"mock\"\ndims = 8\nzzz = 1\n",
            "[embedder]\ntype = \"mock\"\ndims = 8\n[vectors]\nmin_cosine = 2\n",
            "[embedder]\ntype = \"mock\"\ndims = 8\n[other]\n",
        ] {
            assert!(Config::parse(bad, None).is_err(), "{bad}");
        }
    }
}
