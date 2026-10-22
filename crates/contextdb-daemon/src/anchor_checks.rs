// Included in service.rs so session internals remain private to the service.
impl<D: Durability + Clone + 'static> Service<D> {
    pub(crate) fn range_session_id(&self) -> SessionId {
        random_id()
    }
    pub(crate) fn audit_range(&self, cert: &Certificate, scope: &str) -> Result<(), Response> {
        self.log(vec![
            SessionEvent::Start {
                session_id: cert.session_id,
                parent: None,
                harness: "anchors-range".into(),
                repo: scope.into(),
                branch: MAIN,
                head_sha: cert.code_commit.clone(),
                principal: cert.principal,
                channel: Channel::Admin,
                started_epoch: cert.issued_epoch,
                log_checksum: cert.log_checksum_at_issue,
            },
            SessionEvent::CertIssued {
                cert: Box::new(cert.clone()),
            },
            SessionEvent::End {
                session_id: cert.session_id,
                epoch: cert.issued_epoch,
            },
        ]);
        self.flush()
    }
    fn root_proof(&self, cert: Certificate, db: &Db) -> RootProof {
        let roots: Vec<_> = cert
            .entries
            .iter()
            .filter(|e| e.required)
            .map(|e| e.rev)
            .collect();
        let paths = db
            .observation_support(&roots)
            .expect("certified roots")
            .into_iter()
            .filter_map(|r| db.revision_logical(r).and_then(|l| db.logical_info(l)))
            .filter(|(_, kind, _)| *kind == RecordKind::CodeAnchor)
            .map(|(key, _, _)| key)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        RootProof { cert, paths }
    }
    /// None means use the existing full-session check. A filtered result is
    /// ephemeral and never replaces/acknowledges the full session certificate.
    fn proof_check(
        &self,
        bound: &Bound,
        cert_id: Option<ContentHash>,
        action: Option<&Action>,
        strict: bool,
        live: &BTreeMap<String, Vec<u8>>,
        snap: &Arc<Snapshot<D>>,
    ) -> Option<Response> {
        let (session_id, session, agent, _) = bound;
        let mut s = session.lock().expect("session");
        if s.log_broken {
            return Some(proof_response(
                Validation::Invalid {
                    reason: InvalidReason::LogMismatch,
                    diffs: Vec::new(),
                },
                None,
            ));
        }
        let mut proofs = BTreeMap::new();
        for (logical, (owner, _)) in s.view_with_agents(agent) {
            if let Some(proof) = s.agents.get(&owner).and_then(|a| a.proofs.get(&logical)) {
                proofs.insert(logical, proof.clone());
            }
        }
        let explicit = cert_id.and_then(|id| s.certs.iter().find(|c| c.cert_id == id));
        if cert_id.is_some() && explicit.is_none() {
            return Some(err("UnknownRef", "certificate not issued to this session"));
        }
        let entries: Vec<_> = match explicit {
            Some(c) => c.entries.iter().filter(|e| e.required).cloned().collect(),
            None => s
                .view(agent)
                .values()
                .map(|e| CertItem {
                    logical: e.logical,
                    rev: e.rev,
                    required: true,
                    via: Via::Head,
                    since_epoch: e.epoch,
                })
                .collect(),
        };
        if let Some(cert) = explicit {
            proofs.clear();
            for entry in &cert.entries {
                if let Some(proof) = s.proof_history.iter().rev().find(|p| {
                    p.cert.issued_epoch <= cert.issued_epoch
                        && p.cert.entries.first().is_some_and(|e| {
                            e.logical == entry.logical
                                && e.rev == entry.rev
                                && e.since_epoch == entry.since_epoch
                        })
                }) {
                    proofs.insert(entry.logical, proof.clone());
                }
            }
        }
        let strict = strict
            || explicit.is_some_and(|c| c.strict_no_aba)
            || s.agents
                .get(agent)
                .and_then(|a| a.session_cert.as_ref())
                .is_some_and(|c| c.strict_no_aba);
        // Establish unambiguous real filesystem identities before excluding a
        // root. Missing, aliased, linked or unsupported paths check everything.
        let targets = action.and_then(|a| self.filter_targets(&s.repo, a));
        let filtered = targets.is_some();
        let mut selected = Vec::new();
        for e in entries {
            let current = snap.db.head(e.logical).unwrap_or(e.rev);
            let support = self
                .root_proof(
                    Certificate {
                        cert_id: [0; 32],
                        session_id: [0; 16],
                        principal: s.principal,
                        branch: MAIN,
                        issued_epoch: snap.epoch,
                        log_checksum_at_issue: [0; 32],
                        code_commit: String::new(),
                        watermarks: Vec::new(),
                        entries: vec![CertItem {
                            rev: current,
                            ..e.clone()
                        }],
                        vector: Vec::new(),
                        strict_no_aba: false,
                    },
                    &snap.db,
                )
                .paths;
            let old = proofs
                .get(&e.logical)
                .filter(|p| p.cert.entries.first().is_some_and(|x| x.rev == e.rev));
            if old.is_none() && (filtered || strict) {
                return Some(proof_finish(
                    &mut s,
                    agent,
                    Validation::Unknown {
                        lag: vec!["MissingBaseline".into()],
                    },
                    &snap.db,
                    live,
                    !filtered && cert_id.is_none(),
                ));
            }
            let paths: BTreeSet<_> = support
                .iter()
                .chain(old.into_iter().flat_map(|p| p.paths.iter()))
                .cloned()
                .collect();
            let relevant = targets
                .as_ref()
                .is_none_or(|t| paths.is_empty() || paths.iter().any(|p| t.contains(p)));
            if !relevant {
                continue;
            }
            match old {
                Some(p) => selected.push(p.clone()),
                None if filtered || strict => {
                    return Some(proof_response(
                        Validation::Unknown {
                            lag: vec!["MissingBaseline".into()],
                        },
                        None,
                    ));
                }
                None => {} // Legacy non-strict full check uses current complete closure.
            }
        }
        if !filtered && !strict {
            return None;
        }
        if selected.is_empty() {
            return filtered.then(|| proof_response(Validation::Empty, None));
        }
        // Keep each root's historical strict window and original issue link.
        // Current verdicts and ambiguity are aggregated once by the kernel,
        // rather than comparing stale per-root vectors or dropping payloads.
        let mut strict_diffs = Vec::new();
        for p in &selected {
            let old = match self.checksum(p.cert.issued_epoch) {
                Ok(c) => c,
                Err(r) => return Some(r),
            };
            if old != p.cert.log_checksum_at_issue {
                return Some(proof_response(
                    Validation::Invalid {
                        reason: InvalidReason::LogMismatch,
                        diffs: Vec::new(),
                    },
                    None,
                ));
            }
            if strict {
                let mut c = p.cert.clone();
                c.strict_no_aba = true;
                match snap.db.validate_certificate(
                    &c,
                    live,
                    LogLink {
                        matches: true,
                        current_checksum: old,
                    },
                ) {
                    Ok(Validation::Invalid {
                        reason: InvalidReason::StrictChange,
                        diffs,
                    }) => strict_diffs.extend(diffs),
                    Ok(_) => {}
                    Err(h) => return Some(err("HistoryUnavailable", format!("{h:?}"))),
                }
            }
        }
        let checksum = match self.checksum(snap.epoch) {
            Ok(c) => c,
            Err(r) => return Some(r),
        };
        let mut aggregate = selected[0].cert.clone();
        aggregate.issued_epoch = snap.epoch;
        aggregate.log_checksum_at_issue = checksum;
        aggregate.strict_no_aba = false;
        aggregate.entries = selected
            .iter()
            .flat_map(|p| p.cert.entries.clone())
            .collect();
        // Roots may hold different revisions of one logical. Keep the earliest
        // read (as shared scopes keep the earliest window), so the result is
        // independent of root order and an older read still reports its diff.
        let mut vector: BTreeMap<LogicalId, VectorItem> = BTreeMap::new();
        for item in selected.iter().flat_map(|p| &p.cert.vector) {
            vector
                .entry(item.logical)
                .and_modify(|e| {
                    if (item.since_epoch, item.rev) < (e.since_epoch, e.rev) {
                        *e = item.clone();
                    }
                })
                .or_insert_with(|| item.clone());
        }
        aggregate.vector = vector.into_values().collect();
        aggregate.cert_id = *blake3::hash(&aggregate.body_bytes()).as_bytes();
        let mut validation = match snap.db.validate_certificate(
            &aggregate,
            live,
            LogLink {
                matches: true,
                current_checksum: checksum,
            },
        ) {
            Ok(v) => v,
            Err(h) => return Some(err("HistoryUnavailable", format!("{h:?}"))),
        };
        if !matches!(validation, Validation::Denied) && !strict_diffs.is_empty() {
            if let Validation::Invalid { diffs, .. } = &validation {
                strict_diffs.extend(diffs.clone());
            }
            strict_diffs.sort_by_key(|d| d.logical);
            strict_diffs.dedup_by_key(|d| d.logical);
            validation = Validation::Invalid {
                reason: InvalidReason::StrictChange,
                diffs: strict_diffs,
            };
        }
        let anchored = selected.iter().any(|p| !p.paths.is_empty());
        if !(filtered || strict && anchored)
            && matches!(
                validation,
                Validation::Valid | Validation::ValidRecertified { .. }
            )
        {
            return None;
        }
        if !filtered
            && matches!(
                validation,
                Validation::Valid | Validation::ValidRecertified { .. }
            )
        {
            // Full-session success retains P3's durable, sticky strict mode.
            // A filtered result never changes that certificate or read ledger.
            if let Some(id) = cert_id {
                let cert = s
                    .certs
                    .iter()
                    .find(|c| c.cert_id == id)
                    .expect("checked explicit")
                    .clone();
                return Some(proof_response(validation, Some(Box::new(cert))));
            }
            let request = match self.issue_request(*session_id, &s, snap.epoch, strict) {
                Ok(r) => r,
                Err(r) => return Some(r),
            };
            let fresh = match snap
                .db
                .issue_certificate(&request, &aggregate.entries, live)
            {
                Ok(c) => c,
                Err(e) => return Some(err("NotCertified", format!("{e:?}"))),
            };
            let prior = s.agents.get(agent).and_then(|a| a.session_cert.as_ref());
            let changed = prior.is_none_or(|c| {
                !c.strict_no_aba
                    || c.entries != fresh.entries
                    || c.vector != fresh.vector
                    || c.watermarks != fresh.watermarks
            });
            let cert = if changed {
                fresh
            } else {
                prior.expect("unchanged").clone()
            };
            let superseded = s
                .agent(agent)
                .superseded
                .drain(..)
                .map(|(logical, old, new)| CertDiff {
                    logical,
                    cert_rev: Some(old),
                    current: Some(new),
                    summary: snap.db.summary_at(new, live).ok(),
                })
                .collect();
            if changed {
                s.certs.push(cert.clone());
                s.agent(agent).session_cert = Some(cert.clone());
                self.log(vec![
                    SessionEvent::CertIssued {
                        cert: Box::new(cert.clone()),
                    },
                    SessionEvent::SessionCert {
                        session_id: cert.session_id,
                        agent: agent.clone(),
                        cert_id: cert.cert_id,
                    },
                ]);
            }
            drop(s);
            if changed && let Err(r) = self.flush() {
                return Some(r);
            }
            if let Validation::ValidRecertified { new_cert, .. } = &mut validation {
                **new_cert = cert.clone();
            }
            let mut response = proof_response(validation, Some(Box::new(cert)));
            if let Response::Check {
                superseded: out, ..
            } = &mut response
            {
                *out = superseded;
            }
            return Some(response);
        }
        Some(proof_finish(
            &mut s,
            agent,
            validation,
            &snap.db,
            live,
            !filtered && cert_id.is_none(),
        ))
    }
    fn filter_targets(&self, root: &str, action: &Action) -> Option<BTreeSet<String>> {
        use std::{
            os::unix::fs::MetadataExt,
            path::{Component, Path},
        };
        if !matches!(
            action.tool,
            ToolClass::Edit | ToolClass::Write | ToolClass::Notebook | ToolClass::Patch
        ) || action.paths.is_empty()
        {
            return None;
        }
        let root = Path::new(root).canonicalize().ok()?;
        // The same identity-checked handle cache as live sampling: no Git
        // process per check once the repository has been opened.
        let repo = self.cached_repo(
            &root,
            std::time::Instant::now() + *self.anchor_live_timeout.lock().expect("timeout"),
            false,
        )?;
        let mut keys = BTreeSet::new();
        for target in &action.paths {
            let given = Path::new(target);
            let joined = if given.is_absolute() {
                given.to_path_buf()
            } else {
                root.join(given)
            };
            if joined
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return None;
            }
            let rel = joined.strip_prefix(&root).ok()?;
            let path = rel.to_str()?;
            contextdb_anchors::validate_path(&repo.id, path).ok()?;
            let canonical = joined.canonicalize().ok()?;
            // Canonical spelling check catches case aliases. Parent symlinks and
            // hard links conservatively fall back to full proof validation.
            if canonical != joined {
                return None;
            }
            let mut cursor = root.clone();
            let parts = rel.components().count();
            for (index, part) in rel.components().enumerate() {
                cursor.push(part);
                if cursor.symlink_metadata().ok()?.file_type().is_symlink() {
                    return None;
                }
                // A nested repository, submodule or linked worktree is its own
                // scope, so a bound-repo key would miss its anchors: full check.
                // Only a definite absence of `.git` keeps the target.
                if index + 1 < parts {
                    match cursor.join(".git").symlink_metadata() {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        _ => return None,
                    }
                }
            }
            let m = joined.metadata().ok()?;
            if !m.is_file() || m.nlink() != 1 {
                return None;
            }
            keys.insert(format!("{}/{}", repo.scope(), path));
        }
        Some(keys)
    }
}
fn proof_response(validation: Validation, cert: Option<Box<Certificate>>) -> Response {
    Response::Check {
        decision: if matches!(
            validation,
            Validation::Valid | Validation::ValidRecertified { .. }
        ) {
            Decision::Allow
        } else {
            Decision::Warn
        },
        validation,
        cert,
        superseded: Vec::new(),
        gate: None,
        updates_pending: 0,
        named: Vec::new(),
    }
}

fn proof_finish(
    s: &mut Session,
    agent: &Agent,
    validation: Validation,
    db: &Db,
    live: &BTreeMap<String, Vec<u8>>,
    consume: bool,
) -> Response {
    let mut response = proof_response(validation, None);
    if consume && let Response::Check { superseded, .. } = &mut response {
        *superseded = s
            .agent(agent)
            .superseded
            .drain(..)
            .map(|(logical, old, new)| CertDiff {
                logical,
                cert_rev: Some(old),
                current: Some(new),
                summary: db.summary_at(new, live).ok(),
            })
            .collect();
    }
    response
}
