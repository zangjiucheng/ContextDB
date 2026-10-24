//! P5a trusted routing, bounded registry and local live evidence.
use crate::{
    actor::Snapshot,
    service::{Identity, Service, resolve_branch},
};
use contextdb_anchors::{AnchorDescriptor, Repo, capture::MAX_ANCHORS};
use contextdb_core::{BranchView, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_proto::proto::*;
use contextdb_store::{Durability, Payload};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Instant,
};
/// A routed worktree with the root/Git-dir identity it was opened at.
pub(crate) type OpenedRepo = (contextdb_anchors::capture::RepoIdentity, Repo);
/// One scope's observed paths (ADR 0012 §6).
#[derive(Default)]
struct Layout {
    files: BTreeSet<String>,
    hosts: BTreeSet<String>,
    versions: BTreeMap<contextdb_anchors::Language, String>,
}
fn err(detail: impl Into<String>) -> Response {
    Response::Error {
        code: "ObservationUnavailable".into(),
        detail: detail.into(),
    }
}
impl<D: Durability + Clone + 'static> Service<D> {
    pub(crate) fn anchor_state(
        &self,
        id: Identity,
        scope: &str,
        git_branch: Option<&str>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        if id.channel == Channel::Agent {
            return Response::Error {
                code: "ChannelRestricted".into(),
                detail: "observer/admin only".into(),
            };
        }
        // The observer worktree's branch (ADR 0013 §7, m3); ids are global.
        let branch = match resolve_branch(snap, None, git_branch) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let view = snap.view(branch);
        let inputs = view.db().inputs();
        let mut anchors = Vec::new();
        for (index, l) in inputs.logicals.iter().enumerate() {
            if l.kind != RecordKind::CodeAnchor || l.scope_lane != scope {
                continue;
            }
            if anchors.len() == MAX_ANCHORS {
                return err("scope anchor limit");
            }
            let logical = LogicalId(index as u32);
            let head = inputs.heads[index];
            let content = inputs.revisions[head.index()].content;
            let descriptor = match snap.log.read_blob(&content) {
                Payload::Available(b) => b,
                _ => return err("descriptor unavailable"),
            };
            let breaks = inputs
                .artifacts
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    a.open
                        && a.ty == ArtifactType::AnchorBroken
                        && inputs.revisions[a.target.index()].logical == logical
                })
                .map(|(i, _)| ArtifactId(i as u32))
                .collect::<Vec<_>>();
            anchors.push(AnchorEntry {
                logical: view.global_logical(logical),
                key: l.key.clone(),
                head: view.global_revision(head),
                content,
                base: inputs.base[head.index()],
                breaks: breaks
                    .into_iter()
                    .map(|a| view.global_artifact(a))
                    .collect(),
                descriptor,
            });
        }
        Response::AnchorState {
            branch: view.info().id,
            watermark: inputs.watermarks.get(scope).cloned(),
            anchors,
        }
    }
    /// Config is operator-controlled and outside kernel truth; absent/bad data
    /// yields no live evidence and thus Unknown, including after restart.
    pub(crate) fn anchor_live(&self, db: &BranchView<'_>) -> BTreeMap<String, Vec<u8>> {
        self.anchor_evidence(db)
            .into_iter()
            .map(|(scope, (position, _))| (scope, position))
            .collect()
    }
    fn anchor_evidence(&self, db: &BranchView<'_>) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
        let deadline = Instant::now() + *self.anchor_live_timeout.lock().expect("timeout");
        let Some(path) = self.anchor_routes.lock().expect("routes").clone() else {
            return BTreeMap::new();
        };
        let outbox = contextdb_proto::outbox::Outbox::new(path.parent().expect("route parent"));
        // ADR 0011 §9: this sample covers every edit announced before it, so
        // only an announce during sampling matters. The record is read before
        // the first and after the last capture; the sentinel and appender
        // marker are checked on both sides.
        if !outbox.announce_trusted() {
            return BTreeMap::new();
        }
        let Ok(announced) = outbox.announce_record() else {
            return BTreeMap::new();
        };
        let routes = (|| {
            use std::{io::Read, os::unix::fs::OpenOptionsExt};
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&path)
                .ok()?;
            if !file.metadata().ok()?.is_file() {
                return None;
            }
            let mut bytes = Vec::new();
            file.take((1 << 20) + 1).read_to_end(&mut bytes).ok()?;
            (bytes.len() <= 1 << 20 && Instant::now() <= deadline).then_some(bytes)
        })()
        .and_then(|b| Vec::<(String, String)>::from_bytes(&b).ok())
        .unwrap_or_default();
        let inputs = db.db().inputs();
        // Paths come from the head descriptors, never from splitting keys
        // (ADR 0012 §7): file anchors, symbol host files and the normaliser
        // version of each symbol language. A scope whose layout cannot be
        // read is left out (Unknown).
        let mut groups: BTreeMap<String, Layout> = BTreeMap::new();
        let mut unreadable = BTreeSet::new();
        for (index, l) in inputs.logicals.iter().enumerate() {
            if l.kind != RecordKind::CodeAnchor {
                continue;
            }
            let content = inputs.revisions[inputs.heads[index].index()].content;
            let layout = groups.entry(l.scope_lane.clone()).or_default();
            match self.anchor_descriptor(content) {
                Some(AnchorDescriptor::File(d)) => {
                    layout.files.insert(d.path);
                }
                Some(AnchorDescriptor::Symbol(d)) => {
                    layout.hosts.insert(d.path);
                    layout.versions.insert(d.language, d.normaliser);
                }
                None => {
                    unreadable.insert(l.scope_lane.clone());
                }
            }
        }
        groups.retain(|scope, _| !unreadable.contains(scope));
        let Some(data) = path.parent().and_then(|p| p.canonicalize().ok()) else {
            return BTreeMap::new();
        };
        // Descriptor checks run beside the capture; a scope with any invalid
        // head descriptor is dropped afterwards, exactly as if checked first.
        let (invalid, mut live) = std::thread::scope(|s| {
            let invalid = s.spawn(|| {
                let blobs = self.actor.snapshot();
                let mut invalid = BTreeSet::new();
                for (index, logical) in inputs
                    .logicals
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| l.kind == RecordKind::CodeAnchor)
                {
                    let content = inputs.revisions[inputs.heads[index].index()].content;
                    let valid = match blobs.log.read_blob(&content) {
                        Payload::Available(b) => {
                            AnchorDescriptor::from_bytes(&b).ok().is_some_and(|d| {
                                d.key() == logical.key
                                    && d.scope() == logical.scope_lane
                                    && d.content() == content
                            })
                        }
                        _ => false,
                    };
                    if !valid {
                        invalid.insert(logical.scope_lane.clone());
                    }
                }
                invalid
            });
            let mut live = BTreeMap::new();
            for (scope, layout) in &groups {
                let paths: BTreeSet<String> = layout.files.union(&layout.hosts).cloned().collect();
                let matching: Vec<_> = routes.iter().filter(|(s, _)| s == scope).collect();
                if matching.len() != 1 {
                    continue;
                }
                let Ok(root) = std::path::Path::new(&matching[0].1).canonicalize() else {
                    continue;
                };
                if paths
                    .iter()
                    .any(|path| runtime_path(&root.join(path), &data))
                {
                    continue;
                }
                let Some(repo) = self.routed_repo(&root, deadline) else {
                    continue;
                };
                if repo.scope() != *scope {
                    continue;
                }
                if let Ok(capture) = repo.capture(&paths, deadline) {
                    let hosts_only = layout.hosts.difference(&layout.files).cloned().collect();
                    let position = contextdb_anchors::capture::position(
                        &repo.id,
                        &capture.head,
                        &capture.entries,
                        &hosts_only,
                        &layout.versions,
                    );
                    live.insert(scope.clone(), (position, capture.head));
                }
            }
            (invalid.join(), live)
        });
        let Ok(invalid) = invalid else {
            return BTreeMap::new();
        };
        live.retain(|scope, _| !invalid.contains(scope));
        if Instant::now() > deadline
            || outbox.announce_record().ok().as_ref() != Some(&announced)
            || !outbox.announce_trusted()
        {
            live.clear();
        }
        live
    }
    /// A head descriptor by content hash. Descriptors are content-addressed,
    /// so a decoded one is cached; validity is still re-read every sample
    /// (anchor_evidence), so a damaged descriptor stays Unknown.
    pub(crate) fn anchor_descriptor(&self, content: ContentHash) -> Option<AnchorDescriptor> {
        if let Some(d) = self
            .anchor_descriptors
            .lock()
            .expect("descriptors")
            .get(&content)
        {
            return Some(d.clone());
        }
        let bytes = match self.actor.snapshot().log.read_blob(&content) {
            Payload::Available(b) => b,
            _ => return None,
        };
        let d = AnchorDescriptor::from_bytes(&bytes).ok()?;
        if d.content() != content {
            return None;
        }
        let mut cache = self.anchor_descriptors.lock().expect("descriptors");
        if cache.len() >= 4 * MAX_ANCHORS {
            cache.clear();
        }
        cache.insert(content, d.clone());
        Some(d)
    }
    /// The routed worktree, reused while its root and Git directory keep the
    /// identity they were opened with (saves a Git process per live sample).
    /// `root` is the route canonicalized for this sample, the same root the
    /// runtime-path check used; the cache is keyed by it.
    pub(crate) fn routed_repo(&self, root: &std::path::Path, deadline: Instant) -> Option<Repo> {
        self.cached_repo(root, deadline, true)
    }
    /// `remember`: only trusted routes enter the cache; other roots (a
    /// session's repository in path filtering) may use it but not fill it.
    pub(crate) fn cached_repo(
        &self,
        root: &std::path::Path,
        deadline: Instant,
        remember: bool,
    ) -> Option<Repo> {
        let key = root.to_str()?;
        let cached = self.anchor_repos.lock().expect("repos").get(key).cloned();
        if let Some((identity, repo)) = cached
            && repo.root == root
            && repo.identity().ok() == Some(identity)
        {
            return Some(repo);
        }
        let repo = Repo::open(root, deadline).ok()?;
        if repo.root != root {
            return None;
        }
        if !remember {
            return Some(repo);
        }
        let root = key;
        let identity = repo.identity().ok()?;
        let mut repos = self.anchor_repos.lock().expect("repos");
        // Bounded like the routes it mirrors.
        if repos.len() >= MAX_ANCHORS && !repos.contains_key(root) {
            repos.clear();
        }
        repos.insert(root.to_owned(), (identity, repo.clone()));
        Some(repo)
    }
    pub(crate) fn check_anchors(
        &self,
        id: Identity,
        scope: &str,
        paths: &[String],
        expected_head: &[u8],
        expected_position: &[u8],
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        if id.channel != Channel::Admin {
            return Response::Error {
                code: "ChannelRestricted".into(),
                detail: "range check requires admin".into(),
            };
        }
        // Protocol 4 has no branch for this admin check: `main`.
        let db = &snap.view(MAIN);
        let inputs = db.db().inputs();
        // Bind the pushed-tree comparison to this exact committed snapshot AND
        // the same bounded live capture. A watcher committing newer U after
        // the CLI verified T must never certify T using U's freshness proof.
        let evidence = self.anchor_evidence(db);
        if expected_position.len() != 32
            || !matches!(expected_head.len(), 20 | 32)
            || inputs.watermarks.get(scope).map(Vec::as_slice) != Some(expected_position)
            || evidence.get(scope).is_none_or(|(position, head)| {
                position != expected_position || head != expected_head
            })
        {
            return range_response(
                Validation::Unknown {
                    lag: vec![scope.into()],
                },
                None,
            );
        }
        let live = evidence
            .into_iter()
            .map(|(scope, (position, _))| (scope, position))
            .collect();
        let mut affected: BTreeSet<_> = inputs
            .logicals
            .iter()
            .enumerate()
            .filter(|(index, l)| {
                // A changed path selects its file anchor and every symbol
                // anchor hosted in it (ADR 0012 §7); paths come from
                // descriptors. An unreadable descriptor is selected
                // conservatively.
                l.kind == RecordKind::CodeAnchor
                    && l.scope_lane == scope
                    && self
                        .anchor_descriptor(inputs.revisions[inputs.heads[*index].index()].content)
                        .is_none_or(|d| paths.iter().any(|p| p == d.path()))
            })
            .flat_map(|(l, _)| {
                inputs
                    .revisions
                    .iter()
                    .enumerate()
                    .filter(move |(_, r)| r.logical.index() == l)
                    .map(|(r, _)| RevisionId(r as u32))
            })
            .collect();
        loop {
            let before = affected.len();
            for rule in &inputs.rules {
                if rule.premises.iter().any(|p| {
                    p.kind != EdgeKind::Ref
                        && (affected.contains(&p.premise)
                            || (p.kind == EdgeKind::Sem
                                && affected.iter().any(|r| {
                                    inputs.revisions[r.index()].logical
                                        == inputs.revisions[p.premise.index()].logical
                                })))
                }) {
                    affected.insert(rule.conclusion);
                }
            }
            if before == affected.len() {
                break;
            }
        }
        let entries: Vec<_> = inputs
            .heads
            .iter()
            .enumerate()
            .filter(|(_, r)| affected.contains(r))
            .map(|(l, r)| CertItem {
                logical: db.global_logical(LogicalId(l as u32)),
                rev: db.global_revision(*r),
                required: true,
                via: Via::Head,
                since_epoch: snap.epoch,
            })
            .collect();
        let checksum = match snap.log.checksum_at(snap.epoch) {
            Ok(c) => c,
            Err(e) => return err(format!("{e:?}")),
        };
        let request = IssueRequest {
            session_id: self.range_session_id(),
            principal: id.principal,
            branch: MAIN,
            log_checksum: checksum,
            code_commit: expected_head.iter().map(|b| format!("{b:02x}")).collect(),
            strict_no_aba: true,
        };
        // The kernel validator applies the complete fixed verdict precedence;
        // issue_certificate's first failing entry cannot aggregate failures.
        let probe = Certificate {
            cert_id: [0; 32],
            session_id: request.session_id,
            principal: id.principal,
            branch: MAIN,
            issued_epoch: snap.epoch,
            log_checksum_at_issue: checksum,
            code_commit: request.code_commit.clone(),
            watermarks: Vec::new(),
            entries: entries.clone(),
            vector: Vec::new(),
            strict_no_aba: false,
        };
        let validation = match db.validate_certificate(
            &probe,
            &live,
            LogLink {
                matches: true,
                current_checksum: checksum,
            },
        ) {
            Ok(v) => v,
            Err(e) => return err(format!("{e:?}")),
        };
        let (validation, cert) = if matches!(
            validation,
            Validation::Valid | Validation::ValidRecertified { .. }
        ) {
            match db.issue_certificate(&request, &entries, &live) {
                Ok(c) => {
                    if let Err(response) = self.audit_range(&c, scope) {
                        return response;
                    }
                    (Validation::Valid, Some(Box::new(c)))
                }
                Err(e) => return err(format!("{e:?}")),
            }
        } else {
            (validation, None)
        };
        range_response(validation, cert)
    }
}
/// Whether an anchor spelling lies in the daemon's data directory. Capture
/// opens every component with O_NOFOLLOW, so a path through a symlink is
/// Broken and never read; only the spelling itself can reach runtime files.
/// Components compare ASCII case-insensitively because the volume may be.
fn runtime_path(path: &std::path::Path, data: &std::path::Path) -> bool {
    let mut p = path.components();
    data.components().all(|d| {
        p.next().is_some_and(|c| {
            c.as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(d.as_os_str().as_encoded_bytes())
        })
    })
}
fn range_response(validation: Validation, cert: Option<Box<Certificate>>) -> Response {
    Response::Check {
        decision: if matches!(validation, Validation::Valid) {
            Decision::Allow
        } else {
            Decision::Deny
        },
        validation,
        cert,
        superseded: Vec::new(),
        gate: None,
        updates_pending: 0,
        named: Vec::new(),
    }
}
