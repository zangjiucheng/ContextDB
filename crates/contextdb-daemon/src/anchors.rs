//! P5a trusted routing, bounded registry and local live evidence.
use crate::{
    actor::Snapshot,
    service::{Identity, Service},
};
use contextdb_anchors::{
    Repo,
    capture::{LIVE_TIMEOUT, MAX_ANCHORS},
};
use contextdb_core::{Db, IssueRequest, LogLink};
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
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        if id.channel == Channel::Agent {
            return Response::Error {
                code: "ChannelRestricted".into(),
                detail: "observer/admin only".into(),
            };
        }
        let inputs = snap.db.inputs();
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
                .collect();
            anchors.push(AnchorEntry {
                logical,
                key: l.key.clone(),
                head,
                content,
                base: inputs.base[head.index()],
                breaks,
                descriptor,
            });
        }
        Response::AnchorState {
            watermark: inputs.watermarks.get(scope).cloned(),
            anchors,
        }
    }
    /// Config is operator-controlled and outside kernel truth; absent/bad data
    /// yields no live evidence and thus Unknown, including after restart.
    pub(crate) fn anchor_live(&self, db: &Db) -> BTreeMap<String, Vec<u8>> {
        self.anchor_evidence(db)
            .into_iter()
            .map(|(scope, (position, _))| (scope, position))
            .collect()
    }
    fn anchor_evidence(&self, db: &Db) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
        let deadline = Instant::now() + LIVE_TIMEOUT;
        let Some(path) = self.anchor_routes.lock().expect("routes").clone() else {
            return BTreeMap::new();
        };
        let outbox = contextdb_proto::outbox::Outbox::new(path.parent().expect("route parent"));
        let Ok(cutoff) = outbox.observation_cutoff_until(deadline) else {
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
        let inputs = db.inputs();
        let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for l in &inputs.logicals {
            if l.kind != RecordKind::CodeAnchor {
                continue;
            }
            let Some(path) = l.key.strip_prefix(&format!("{}/", l.scope_lane)) else {
                return BTreeMap::new();
            };
            groups
                .entry(l.scope_lane.clone())
                .or_default()
                .insert(path.to_owned());
        }
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
                        Payload::Available(b) => contextdb_anchors::Descriptor::from_bytes(&b)
                            .ok()
                            .is_some_and(|d| {
                                d.key() == logical.key
                                    && d.scope() == logical.scope_lane
                                    && d.content() == content
                            }),
                        _ => false,
                    };
                    if !valid {
                        invalid.insert(logical.scope_lane.clone());
                    }
                }
                invalid
            });
            let mut live = BTreeMap::new();
            for (scope, paths) in &groups {
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
                if let Ok(capture) = repo.capture(paths, deadline) {
                    live.insert(scope.clone(), (capture.position, capture.head));
                }
            }
            (invalid.join(), live)
        });
        let Ok(invalid) = invalid else {
            return BTreeMap::new();
        };
        live.retain(|scope, _| !invalid.contains(scope));
        // Only the pre-capture cutoff was covered. An append during sampling
        // must not be silently swept into that proof at the end of the scan.
        if Instant::now() > deadline
            || outbox.observation_cutoff_until(deadline).ok().as_ref() != Some(&cutoff)
        {
            live.clear();
        }
        live
    }
    /// The routed worktree, reused while its root and Git directory keep the
    /// identity they were opened with (saves a Git process per live sample).
    /// `root` is the route canonicalized for this sample, the same root the
    /// runtime-path check used; the cache is keyed by it.
    fn routed_repo(&self, root: &std::path::Path, deadline: Instant) -> Option<Repo> {
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
        let db = &snap.db;
        let inputs = db.inputs();
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
            .filter(|(_, l)| {
                l.kind == RecordKind::CodeAnchor
                    && l.scope_lane == scope
                    && paths.iter().any(|p| l.key == format!("{scope}/{p}"))
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
                logical: LogicalId(l as u32),
                rev: *r,
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
