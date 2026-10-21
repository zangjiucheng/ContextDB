//! What `ctx-hook <harness> <event>` does (ADR 0010 §3–§6, §9): one hook
//! event in, at most one JSON object out. The daemon is reached with a hard
//! deadline; when it is unavailable the hook fails open or closed per the
//! gating configuration, and facts that must not be lost go to the outbox.
use crate::{
    adapter::{self, EventName, Harness, HookEvent, Kind},
    outbox::{self, Delivered, Outbox, Reply, Stream},
    paths,
    render::{self, Output},
};
use contextdb_model::*;
use contextdb_proto::{
    Client,
    gate::{self, GateConfig},
    lease,
    proto::{Binding, Envelope, Request, Response, UpdateItem},
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// ContextDB's session-scoped MCP tools, which the PreToolUse hook claims
/// (ADR 0010 §2.3).
pub const CLAIMED_TOOLS: [&str; 5] = [
    "ctx_recall",
    "ctx_read",
    "ctx_record",
    "ctx_supersede",
    "ctx_check",
];
/// An Empty warning is rendered at most this often per stream and agent.
const EMPTY_EVERY: u64 = 600;
const GIT_LIMIT: Duration = Duration::from_millis(300);

/// Runs `git` in `dir`, bounded by `limit`; None on error or timeout.
pub fn git(dir: &Path, args: &[&str], limit: Duration) -> Option<Vec<u8>> {
    git_with_input(dir, args, None, limit)
}
fn git_with_input(
    dir: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    limit: Duration,
) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        use std::io::Write;
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        out
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
fn line(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_owned()
}
/// The main worktree root of the repository containing `cwd` (ADR 0010
/// §3.2): worktrees share it; inside a submodule, the superproject's.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
            "--show-superproject-working-tree",
            "--show-toplevel",
        ],
        GIT_LIMIT,
    )?;
    let text = String::from_utf8_lossy(&out).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    // With no superproject the middle line is absent.
    let (common, superproject, top) = match lines.as_slice() {
        [c, s, t] => (*c, Some(*s), *t),
        [c, t] => (*c, None, *t),
        _ => return None,
    };
    if let Some(sp) = superproject.filter(|s| !s.is_empty()) {
        return repo_root(Path::new(sp));
    }
    let common = Path::new(common);
    match (common.file_name(), common.parent()) {
        (Some(n), Some(parent)) if n == ".git" => Some(parent.to_path_buf()),
        _ => Some(PathBuf::from(top)),
    }
}
/// [`repo_root`], cached per working directory (ADR 0010 §3.2): `git
/// rev-parse` costs ~15 ms, more than the rest of a PreToolUse hook. A cached
/// root is used only while it still holds `.git` and still contains `cwd`.
pub fn repo_root_cached(cwd: &Path) -> Option<PathBuf> {
    let uid = std::env::var("USER").unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("ctx-hook-{uid}"));
    let key = blake3::hash(cwd.as_os_str().as_encoded_bytes()).to_hex();
    let file = dir.join(&key[..32]);
    if let Ok(text) = std::fs::read_to_string(&file) {
        let root = PathBuf::from(text.trim_end());
        let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        if root.join(".git").exists() && (canon.starts_with(&root) || cwd.starts_with(&root)) {
            return Some(root);
        }
    }
    let root = repo_root(cwd)?;
    if std::fs::create_dir_all(&dir).is_ok() {
        let tmp = file.with_extension(format!("{}.tmp", std::process::id()));
        if std::fs::write(&tmp, root.to_string_lossy().as_bytes()).is_ok() {
            let _ = std::fs::rename(&tmp, &file);
        }
    }
    Some(root)
}
/// The data directory for a repository (ADR 0010 §6.2): `CTX_DIR`, else the
/// path in `<root>/.contextdb/dir.wire`, else `<root>/.contextdb/db`.
pub fn data_dir(root: Option<&Path>) -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CTX_DIR") {
        return Some(PathBuf::from(d));
    }
    let root = root?;
    let dot = root.join(".contextdb");
    if let Ok(bytes) = std::fs::read(dot.join("dir.wire"))
        && let Ok(p) = String::from_bytes(&bytes)
    {
        let p = PathBuf::from(p);
        return Some(if p.is_absolute() { p } else { root.join(p) });
    }
    Some(dot.join("db"))
}
/// The compiled gating configuration (defaults when absent).
pub fn gate_config(data: &Path) -> GateConfig {
    std::fs::read(data.join("gating.wire"))
        .ok()
        .and_then(|b| GateConfig::from_bytes(&b).ok())
        .unwrap_or_default()
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
fn summary(s: &Summary) -> String {
    match s {
        Summary::Certified => "CERTIFIED".into(),
        Summary::Conflicted => "CONFLICTED".into(),
        Summary::Unavailable => "UNAVAILABLE".into(),
        Summary::Stale(r) => format!("STALE({r:?})"),
        Summary::Pending(r) => format!("PENDING({r:?})"),
    }
}
fn update_line(u: &UpdateItem) -> String {
    match u.current {
        Some(c) if c != u.cert_rev => format!(
            "{}: read r{}, now r{} {}",
            u.key,
            u.cert_rev.0,
            c.0,
            summary(&u.summary)
        ),
        _ => format!("{}: r{} {}", u.key, u.cert_rev.0, summary(&u.summary)),
    }
}
struct Daemon {
    client: Client,
}
impl Daemon {
    fn connect(data: &Path, timeout: Duration) -> Result<Self, String> {
        let l = lease::read_lease(&data.join("ctxd.lease")).ok_or("no ctxd lease")?;
        let sock = l
            .sockets
            .iter()
            .find(|s| s.ends_with("agent.sock"))
            .ok_or("lease has no agent socket")?;
        let client = Client::connect_timeout(sock, timeout).map_err(|e| e.to_string())?;
        Ok(Self { client })
    }
    fn call(&mut self, r: Request) -> Result<Envelope, String> {
        let env = self.client.call(&r).map_err(|e| format!("{e:?}"))?;
        match &env.response {
            Response::Error { code, detail } => Err(format!("{code}: {detail}")),
            _ => Ok(env),
        }
    }
}
/// One hook invocation's context.
pub struct Hook {
    pub harness: Harness,
    pub event: EventName,
    pub data: PathBuf,
    pub root: PathBuf,
    pub cfg: GateConfig,
    pub outbox: Outbox,
    pub stream: Stream,
    pub ev: HookEvent,
}
/// Runs one hook event; returns stdout (None: print nothing).
pub fn run(harness: Harness, event: EventName, stdin: &str) -> Option<String> {
    let ev = match adapter::parse(harness, event, stdin) {
        Ok(ev) => ev,
        Err(adapter::Unparsed(why)) => {
            // Unparsed input follows on_unavailable and is always reported
            // (ADR 0010 §3.1, review M11).
            let cwd = std::env::current_dir().ok()?;
            let data = data_dir(repo_root_cached(&cwd).as_deref())?;
            if !data.exists() {
                return None;
            }
            let cfg = gate_config(&data);
            let note = format!(
                "ContextDB could not read the {} input ({why})",
                event.wire()
            );
            return render::render(event, &unavailable_output(event, &cfg, note));
        }
    };
    let cwd = PathBuf::from(&ev.cwd);
    let root = repo_root_cached(&cwd).unwrap_or_else(|| cwd.clone());
    // Not a ContextDB repository: nothing, in every mode (§6.3).
    let data = data_dir(Some(&root))?;
    if !data.exists() {
        return None;
    }
    let hook = Hook {
        harness,
        event,
        cfg: gate_config(&data),
        outbox: Outbox::new(&data),
        stream: outbox::stream_id(harness.name(), &ev.session),
        root,
        data,
        ev,
    };
    let out = hook.dispatch();
    render::render(event, &out)
}
fn unavailable_output(event: EventName, cfg: &GateConfig, note: String) -> Output {
    if event == EventName::PreToolUse && cfg.fail_closed {
        Output {
            decision: Some(Decision::Deny),
            reason: format!("{note}; gating is fail-closed"),
            ..Output::default()
        }
    } else {
        Output {
            system: format!("{note}: action not checked"),
            ..Output::default()
        }
    }
}
impl Hook {
    fn timeout(&self) -> Duration {
        Duration::from_millis(u64::from(self.cfg.timeout_ms.max(1)))
    }
    fn agent(&self) -> Option<String> {
        self.ev.agent_id.clone()
    }
    fn harness_version(&self) -> String {
        match self.harness {
            Harness::Claude => std::env::var("CLAUDE_CODE_EXECPATH")
                .ok()
                .and_then(|p| {
                    Path::new(&p)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
            Harness::Codex => std::fs::read_to_string(self.data.join("codex.version"))
                .map(|s| s.trim().to_owned())
                .unwrap_or_default(),
        }
    }
    /// Repository-relative paths. git reports canonical paths, the harness
    /// may not (`/tmp` → `/private/tmp` on macOS), so a path that does not
    /// fall under the root lexically is retried through its canonical parent.
    fn normalize(&self, a: &mut Action) {
        let cwd = Path::new(&self.ev.cwd);
        a.paths = a
            .paths
            .iter()
            .map(|p| {
                let n = paths::normalize(&self.root, cwd, p);
                if !Path::new(&n).is_absolute() {
                    return n;
                }
                let abs = Path::new(&n);
                match (
                    abs.parent().and_then(|d| d.canonicalize().ok()),
                    abs.file_name(),
                ) {
                    (Some(dir), Some(name)) => {
                        paths::normalize(&self.root, cwd, &dir.join(name).to_string_lossy())
                    }
                    _ => n,
                }
            })
            .collect();
    }
    /// The daemon and this harness session's ctxd session.
    fn attach(&self) -> Result<(Daemon, [u8; 16]), String> {
        let mut d = Daemon::connect(&self.data, self.timeout())?;
        let env = d.call(Request::SessionAttach {
            harness: self.harness.name().into(),
            harness_session: self.ev.session.clone(),
            repo: self.root.to_string_lossy().into_owned(),
            head_sha: String::new(),
        })?;
        match env.response {
            Response::Attached { session, .. } => Ok((d, session)),
            other => Err(format!("unexpected {other:?}")),
        }
    }
    fn binding(&self, session: [u8; 16]) -> Binding {
        Binding::Session {
            session,
            agent: self.agent(),
        }
    }
    fn dispatch(&self) -> Output {
        match self.ev.kind.clone() {
            Kind::SessionStart { .. } => self.session_start(),
            Kind::UserPrompt => self.user_prompt(),
            Kind::PreTool { mut action } => {
                self.normalize(&mut action);
                self.pre_tool(action)
            }
            Kind::McpCall { tool } => self.claim(&tool),
            Kind::PostTool { mut action } => {
                self.normalize(&mut action);
                self.append(Fact::Action { action, ok: true });
                self.deliver(Duration::from_millis(50), false);
                Output::default()
            }
            Kind::OtherTool { .. } | Kind::SubagentStart => Output::default(),
            Kind::SubagentStop | Kind::Stop | Kind::PreCompact => {
                self.drift(Duration::from_millis(500))
            }
            Kind::SessionEnd { .. } => self.session_end(),
        }
    }
    fn append(&self, fact: Fact) {
        // An outbox that cannot be written is reported by the next delivery
        // (dropped count); the hook itself never fails the harness.
        let _ = self.outbox.append(&self.stream, self.agent(), fact);
    }
    /// Delivers this stream, then (`all`) every other dirty stream, within
    /// `budget`. Returns the drift reports the daemon computed.
    fn deliver(&self, budget: Duration, all: bool) -> Vec<DriftReport> {
        let start = Instant::now();
        let Ok(mut d) = Daemon::connect(&self.data, self.timeout()) else {
            return Vec::new();
        };
        let mut streams = vec![self.stream];
        if all {
            streams.extend(
                self.outbox
                    .dirty()
                    .into_iter()
                    .filter(|s| *s != self.stream),
            );
        }
        let mut drift = Vec::new();
        for s in streams {
            let left = budget.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            let st = self.outbox.state(&s);
            let (harness, key) = if s == self.stream {
                (self.harness.name().to_owned(), self.ev.session.clone())
            } else if st.harness.is_empty() {
                continue; // a stream whose state was never written
            } else {
                (st.harness.clone(), st.harness_session.clone())
            };
            let result = self.outbox.deliver(&s, left, |packets, skip| {
                let env = d.call(Request::Facts {
                    harness: harness.clone(),
                    harness_session: key.clone(),
                    stream: s,
                    packets: packets.to_vec(),
                    skip,
                })?;
                match env.response {
                    Response::Facts {
                        applied_through,
                        drift: reports,
                    } => {
                        drift.extend(reports);
                        Ok(Reply::Applied(applied_through))
                    }
                    Response::Gap { expected } => Ok(Reply::Gap(expected)),
                    other => Err(format!("unexpected {other:?}")),
                }
            });
            if !matches!(result, Ok(Delivered::Done { .. } | Delivered::Busy)) {
                break;
            }
        }
        drift
    }
    fn remember_stream(
        &self,
        head_sha: Option<String>,
        baseline: Option<Vec<(String, ContentHash)>>,
    ) {
        let _ = self.outbox.update_state(&self.stream, |st| {
            st.harness = self.harness.name().into();
            st.harness_session = self.ev.session.clone();
            if let Some(h) = head_sha {
                st.head_sha = h;
            }
            if let Some(b) = baseline {
                st.baseline = b;
            }
        });
    }
    fn session_start(&self) -> Output {
        let head = git(&self.root, &["rev-parse", "HEAD"], GIT_LIMIT).map(line);
        let baseline = self.baseline();
        self.remember_stream(head.clone(), Some(baseline.clone()));
        self.append(Fact::Baseline { dirty: baseline });
        let mut out = Output::default();
        match self.attach() {
            Ok((mut d, session)) => {
                if let Ok(Envelope {
                    epoch,
                    response:
                        Response::Briefing {
                            stale,
                            pending,
                            conflicted,
                            items,
                        },
                }) = d.call(Request::Briefing {
                    binding: self.binding(session),
                    budget_chars: 1_500,
                }) {
                    let mut text = format!(
                        "<contextdb_briefing epoch={}>\nHeads: {stale} STALE, {pending} PENDING, {conflicted} CONFLICTED.\n",
                        epoch.0
                    );
                    if !items.is_empty() {
                        text.push_str("Recently changed:\n");
                        for i in &items {
                            text.push_str(&format!(
                                "- {} ({:?}) r{} {}\n",
                                i.key,
                                i.kind,
                                i.rev.0,
                                summary(&i.summary)
                            ));
                        }
                    }
                    text.push_str("Read what you rely on with ctx_recall/ctx_read (certified) before acting; edits are checked against those reads.\n</contextdb_briefing>");
                    out.context = text;
                }
                if self.gating_file_changed() {
                    out.system =
                        "ContextDB: .contextdb/gating.toml changed; run `ctx gating apply`".into();
                }
            }
            Err(e) => out.system = format!("ContextDB unavailable ({e}): no briefing"),
        }
        self.deliver(Duration::from_millis(500), true);
        out
    }
    fn gating_file_changed(&self) -> bool {
        match std::fs::read(self.root.join(".contextdb/gating.toml")) {
            Ok(src) => *blake3::hash(&src).as_bytes() != self.cfg.source_hash,
            Err(_) => false,
        }
    }
    fn user_prompt(&self) -> Output {
        let Ok((mut d, session)) = self.attach() else {
            return Output::default(); // fail-open (SPEC §6.5)
        };
        let agent = self.agent();
        let ack = self
            .outbox
            .state(&self.stream)
            .tokens
            .iter()
            .find(|(a, _)| *a == agent)
            .map(|(_, t)| *t);
        let Ok(env) = d.call(Request::Updates {
            binding: self.binding(session),
            ack,
            max_items: 30,
        }) else {
            return Output::default();
        };
        let Response::Updates { token, items, more } = env.response else {
            return Output::default();
        };
        let _ = self.outbox.update_state(&self.stream, |st| {
            st.tokens.retain(|(a, _)| *a != agent);
            if !items.is_empty() {
                st.tokens.push((agent.clone(), token));
            }
        });
        if items.is_empty() {
            return Output::default();
        }
        let mut text = format!("<context_update epoch={}>\n", env.epoch.0);
        for u in &items {
            text.push_str(&update_line(u));
            text.push('\n');
        }
        if more {
            text.push_str("(more changes: run ctx_check)\n");
        }
        text.push_str("</context_update>");
        Output {
            context: text,
            ..Output::default()
        }
    }
    fn claim(&self, tool: &str) -> Output {
        let short = tool.rsplit("__").next().unwrap_or("");
        let Some(call) = self.ev.tool_use_id.clone() else {
            return Output::default();
        };
        if self.harness != Harness::Claude || !CLAIMED_TOOLS.contains(&short) {
            return Output::default();
        }
        if let Ok((mut d, session)) = self.attach() {
            // Wait for the reply: the claim must exist before the tool runs.
            let _ = d.call(Request::Claim {
                session,
                agent: self.agent(),
                call,
            });
        }
        Output::default()
    }
    fn pre_tool(&self, action: Action) -> Output {
        let mode = gate::resolve_mode(&self.cfg, &action);
        if mode == GateMode::Off {
            return Output::default();
        }
        let checked = self.attach().and_then(|(mut d, session)| {
            let env = d.call(Request::Check {
                binding: self.binding(session),
                cert_id: None,
                live_pos: Vec::new(),
                action: Some(action.clone()),
                mode: Some(mode),
                harness_version: self.harness_version(),
                strict: false,
            })?;
            match env.response {
                Response::Check {
                    validation, gate, ..
                } => Ok((d, session, validation, gate.unwrap_or(Decision::Warn))),
                other => Err(format!("unexpected {other:?}")),
            }
        });
        let (mut d, session, validation, decision) = match checked {
            Ok(v) => v,
            Err(e) => {
                let out = unavailable_output(
                    EventName::PreToolUse,
                    &self.cfg,
                    format!("ContextDB unavailable ({e})"),
                );
                self.append(Fact::Gate {
                    action,
                    mode,
                    decision: out.decision.unwrap_or(Decision::Allow),
                    harness_version: self.harness_version(),
                });
                return out;
            }
        };
        if decision == Decision::Allow {
            return Output::default();
        }
        if validation == Validation::Empty {
            // The decision is logged every time; the warning is rate-limited.
            let agent = self.agent();
            let last = self
                .outbox
                .state(&self.stream)
                .empty_warned
                .iter()
                .find(|(a, _)| *a == agent)
                .map_or(0, |(_, t)| *t);
            if now().saturating_sub(last) < EMPTY_EVERY {
                return Output::default();
            }
            let _ = self.outbox.update_state(&self.stream, |st| {
                st.empty_warned.retain(|(a, _)| *a != agent);
                st.empty_warned.push((agent.clone(), now()));
            });
            let reason = "ContextDB: this session has no certified reads, so nothing backs this action (Empty). Read the decisions it relies on with ctx_recall/ctx_read (certified).".to_owned();
            return Output {
                decision: Some(decision),
                reason: reason.clone(),
                context: reason,
                system: "ContextDB: no certified reads back this edit".into(),
            };
        }
        // Name the changed reads (peek; UserPromptSubmit acknowledges).
        let changed: Vec<String> = match d.call(Request::Updates {
            binding: self.binding(session),
            ack: None,
            max_items: 8,
        }) {
            Ok(Envelope {
                response: Response::Updates { items, .. },
                ..
            }) => items.iter().map(update_line).collect(),
            _ => Vec::new(),
        };
        let verdict = gate::verdict_code(&validation);
        let mut reason = format!("ContextDB {verdict}");
        if !changed.is_empty() {
            reason.push_str(": ");
            reason.push_str(&changed.join("; "));
        }
        reason.push_str(
            ". Re-read them (ctx_read/ctx_recall, certified) or supersede before editing.",
        );
        let system = match decision {
            Decision::Deny => format!("ContextDB denied {:?} ({verdict})", action.tool),
            _ => format!("ContextDB warning: {verdict}"),
        };
        Output {
            decision: Some(decision),
            context: reason.clone(),
            reason,
            system,
        }
    }
    /// Paths dirty now, with content hashes (the SessionStart baseline).
    fn baseline(&self) -> Vec<(String, ContentHash)> {
        let paths = self.dirty_paths(None).unwrap_or_default();
        self.hashes(&paths)
    }
    fn hashes(&self, paths: &[String]) -> Vec<(String, ContentHash)> {
        let paths: Vec<&String> = paths.iter().take(2_000).collect();
        if paths.is_empty() {
            return Vec::new();
        }
        let input: String = paths.iter().map(|p| format!("{p}\n")).collect();
        let Some(out) = git_with_input(
            &self.root,
            &["hash-object", "--stdin-paths"],
            Some(input.as_bytes()),
            GIT_LIMIT,
        ) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&out).into_owned();
        paths
            .into_iter()
            .zip(text.lines())
            .map(|(p, h)| (p.clone(), *blake3::hash(h.as_bytes()).as_bytes()))
            .collect()
    }
    /// Changed and untracked paths, relative to the root: against `since`
    /// (a commit) when given, else against HEAD.
    fn dirty_paths(&self, since: Option<&str>) -> Option<Vec<String>> {
        let mut out = std::collections::BTreeSet::new();
        let diff = match since {
            Some(c) => git(&self.root, &["diff", "--name-only", "-z", c], GIT_LIMIT)?,
            None => git(
                &self.root,
                &["diff", "--name-only", "-z", "HEAD"],
                GIT_LIMIT,
            )?,
        };
        let untracked = git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            GIT_LIMIT,
        )?;
        // The data directory may live inside the repository: its own files
        // are never the agent's work.
        let data = self
            .data
            .canonicalize()
            .ok()
            .and_then(|d| d.strip_prefix(&self.root).ok().map(Path::to_path_buf));
        for chunk in diff.split(|b| *b == 0).chain(untracked.split(|b| *b == 0)) {
            let path = String::from_utf8_lossy(chunk).into_owned();
            if path.is_empty()
                || data
                    .as_ref()
                    .is_some_and(|d| Path::new(&path).starts_with(d))
            {
                continue;
            }
            out.insert(path);
        }
        Some(out.into_iter().collect())
    }
    fn drift(&self, budget: Duration) -> Output {
        let st = self.outbox.state(&self.stream);
        let since = (!st.head_sha.is_empty()).then_some(st.head_sha.as_str());
        let done = self.dirty_paths(since).map(|paths| {
            // Paths dirty at SessionStart and unchanged since are not ours.
            let base: std::collections::BTreeMap<&String, &ContentHash> =
                st.baseline.iter().map(|(p, h)| (p, h)).collect();
            let touched: Vec<String> = paths
                .iter()
                .filter(|p| base.contains_key(p))
                .cloned()
                .collect();
            let now: std::collections::BTreeMap<String, ContentHash> =
                self.hashes(&touched).into_iter().collect();
            paths
                .into_iter()
                .filter(|p| match base.get(p) {
                    Some(h) => now.get(p) != Some(*h),
                    None => true,
                })
                .collect()
        });
        self.append(Fact::Drift { done });
        let reports = self.deliver(budget, true);
        let Some(r) = reports.last() else {
            return Output::default();
        };
        if r.undeclared.is_empty() && r.unrealised.is_empty() {
            return Output::default();
        }
        let list = |v: &[String]| {
            let mut s = v.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
            if v.len() > 8 {
                s.push_str(&format!(" (+{} more)", v.len() - 8));
            }
            s
        };
        let mut msg = String::from("ContextDB drift:");
        if !r.undeclared.is_empty() {
            msg.push_str(&format!(
                " changed without a recorded edit: {};",
                list(&r.undeclared)
            ));
        }
        if !r.unrealised.is_empty() {
            msg.push_str(&format!(
                " edited but no longer changed: {};",
                list(&r.unrealised)
            ));
        }
        Output {
            system: msg,
            ..Output::default()
        }
    }
    fn session_end(&self) -> Output {
        self.append(Fact::StreamEnd);
        self.deliver(Duration::from_millis(900), false);
        if let Ok((mut d, session)) = self.attach() {
            let _ = d.call(Request::SessionEnd { session });
        }
        Output::default()
    }
}
