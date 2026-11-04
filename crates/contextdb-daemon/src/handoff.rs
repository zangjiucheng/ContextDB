// Handoff (ADR 0015 §7; SPEC §3.5 #7, §6.3). Included into `service.rs`
// for its session internals.
//
// - `ctx_handoff_begin{summary, to_agent?}` issues the caller's session
//   certificate as `ctx_check` would, refuses unless it is Valid or
//   ValidRecertified, and writes a durable `HandoffBegin` carrying the
//   certificate before replying.
// - `ctx_handoff_accept{id}` claims the handoff once: the check and the
//   claim happen under the handoff table's lock, and the reply waits until
//   the `HandoffAccept` event (with the copied reads) is durable. A failed
//   or timed-out flush puts the daemon in `session_log_failed`, refusing
//   every handoff operation until restart; on replay the first durable
//   accept wins, so two accepts never both succeed (T52, T83). An acceptor
//   retrying its own claim gets it back.
// - The accept copies the certificate's required reads into the acceptor's
//   ledger at their original revisions, skipping logicals the acceptor
//   already reads; its next check is Invalid exactly when a handed-over read
//   changed (T79).
//
// A handoff is not a lock, and it does not expire.

/// Largest handoff summary; it lives in the event itself.
pub const HANDOFF_SUMMARY_MAX: usize = 8 * 1024;

/// One handoff, rebuilt from the session log on start.
struct Handoff {
    owner: (SessionId, Agent),
    branch: BranchId,
    cert: Certificate,
    summary: String,
    to_agent: Option<String>,
    epoch: Epoch,
    accepted: Option<(SessionId, Agent)>,
    /// Reads copied and skipped by the accept (0 after a restart).
    copied: u32,
    skipped: u32,
}
#[derive(Default)]
struct Handoffs {
    map: BTreeMap<[u8; 16], Handoff>,
    /// A handoff event could not be made durable: the daemon refuses
    /// handoffs until restart, when the durable log decides.
    session_log_failed: bool,
}
fn hex16(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
impl Handoffs {
    fn replay(&mut self, e: &SessionEvent) {
        match e {
            SessionEvent::HandoffBegin {
                id,
                session_id,
                agent,
                branch,
                cert,
                summary,
                to_agent,
                epoch,
            } => {
                self.map.entry(*id).or_insert_with(|| Handoff {
                    owner: (*session_id, agent.clone()),
                    branch: *branch,
                    cert: (**cert).clone(),
                    summary: summary.clone(),
                    to_agent: to_agent.clone(),
                    epoch: *epoch,
                    accepted: None,
                    copied: 0,
                    skipped: 0,
                });
            }
            SessionEvent::HandoffAccept {
                id,
                session_id,
                agent,
                ..
            } => {
                // The first durable accept decides.
                if let Some(h) = self.map.get_mut(id)
                    && h.accepted.is_none()
                {
                    h.accepted = Some((*session_id, agent.clone()));
                }
            }
            _ => {}
        }
    }
}
impl<D: Durability + Clone + 'static> Service<D> {
    fn handoff_unavailable(&self) -> Option<Response> {
        self.handoffs
            .lock()
            .expect("handoffs")
            .session_log_failed
            .then(|| {
                err(
                    "Unavailable",
                    "a handoff event could not be made durable; restart ctxd",
                )
            })
    }
    fn handoff_begin(
        &self,
        bound: Bound,
        summary: String,
        to_agent: Option<String>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        if summary.len() > HANDOFF_SUMMARY_MAX {
            return err(
                "InvalidArgument",
                format!("a handoff summary holds at most {HANDOFF_SUMMARY_MAX} bytes"),
            );
        }
        if let Some(r) = self.handoff_unavailable() {
            return r;
        }
        let (session, s, agent, _) = bound.clone();
        let branch = {
            let s = s.lock().expect("session");
            if s.ended_epoch.is_some() {
                return err("InvalidArgument", "the session has ended");
            }
            s.branch
        };
        let live = self.anchor_live(&snap.view(branch));
        // The owner's certified state, as `ctx_check` issues it.
        let cert = match self.check(bound, None, live, false, snap) {
            Response::Check {
                validation: Validation::Valid | Validation::ValidRecertified { .. },
                cert: Some(cert),
                ..
            } => *cert,
            Response::Check { validation, .. } => return Response::Refused { validation },
            other => return other,
        };
        let id = random_id();
        let cert_id = cert.cert_id;
        let mut h = self.handoffs.lock().expect("handoffs");
        h.map.insert(
            id,
            Handoff {
                owner: (session, agent.clone()),
                branch,
                cert: cert.clone(),
                summary: summary.clone(),
                to_agent: to_agent.clone(),
                epoch: snap.epoch,
                accepted: None,
                copied: 0,
                skipped: 0,
            },
        );
        self.log(vec![SessionEvent::HandoffBegin {
            id,
            session_id: session,
            agent,
            branch,
            cert: Box::new(cert),
            summary,
            to_agent,
            epoch: snap.epoch,
        }]);
        if let Err(r) = self.flush() {
            h.session_log_failed = true;
            return r;
        }
        Response::HandoffBegun { id, cert_id }
    }
    fn handoff_accept(
        &self,
        (session, s, agent, _): Bound,
        id: Option<[u8; 16]>,
        snap: &Arc<Snapshot<D>>,
    ) -> Response {
        if let Some(r) = self.handoff_unavailable() {
            return r;
        }
        let branch = {
            let s = s.lock().expect("session");
            if s.ended_epoch.is_some() {
                return err("InvalidArgument", "the session has ended");
            }
            s.branch
        };
        let Some(id) = id else {
            // The open handoffs on this branch the caller may accept.
            let h = self.handoffs.lock().expect("handoffs");
            let items = (h.map.iter())
                .filter(|(_, x)| {
                    x.accepted.is_none()
                        && x.branch == branch
                        && x.to_agent
                            .as_ref()
                            .is_none_or(|t| agent.as_ref() == Some(t))
                })
                .map(|(id, x)| HandoffInfo {
                    id: *id,
                    owner: x.owner.0,
                    owner_agent: x.owner.1.clone(),
                    to_agent: x.to_agent.clone(),
                    summary: x.summary.clone(),
                    epoch: x.epoch,
                    reads: x.cert.entries.iter().filter(|e| e.required).count() as u32,
                })
                .collect();
            return Response::Handoffs { items };
        };
        let me = (session, agent.clone());
        let mut h = self.handoffs.lock().expect("handoffs");
        let Some(x) = h.map.get_mut(&id) else {
            return err("UnknownRef", format!("no handoff {}", hex16(&id)));
        };
        match &x.accepted {
            Some(by) if *by != me => {
                return err(
                    "HandoffTaken",
                    format!(
                        "handoff {} was accepted by session {}",
                        hex16(&id),
                        hex16(&by.0)
                    ),
                );
            }
            // A retry of our own claim: already durable (or the daemon
            // would have refused above).
            Some(_) => {}
            None => {
                if let Some(t) = &x.to_agent
                    && agent.as_ref() != Some(t)
                {
                    return err(
                        "HandoffNotAddressed",
                        format!("handoff {} is addressed to agent {t}", hex16(&id)),
                    );
                }
                if x.branch != branch {
                    return err(
                        "BranchMismatch",
                        "the handoff was made on another branch than this session's",
                    );
                }
                x.accepted = Some(me);
                let mut events = vec![SessionEvent::HandoffAccept {
                    id,
                    session_id: session,
                    agent: agent.clone(),
                    epoch: snap.epoch,
                }];
                {
                    let mut s = s.lock().expect("session");
                    let view = s.view(&agent);
                    for e in x.cert.entries.iter().filter(|e| e.required) {
                        if view.contains_key(&e.logical) {
                            x.skipped += 1;
                            continue;
                        }
                        let entry = LedgerEntry {
                            logical: e.logical,
                            rev: e.rev,
                            required: true,
                            epoch: e.since_epoch,
                        };
                        events.push(read_event(session, &agent, &entry));
                        s.agent(&agent).record(entry);
                        x.copied += 1;
                    }
                }
                self.log(events);
                if let Err(r) = self.flush() {
                    h.session_log_failed = true;
                    return r;
                }
            }
        }
        let x = h.map.get(&id).expect("present");
        let cert = x.cert.clone();
        let reply = (x.owner.clone(), x.summary.clone(), x.copied, x.skipped);
        drop(h);
        // The handoff certificate's verdict now, as `ctx_check{cert_id}`.
        let db = &snap.view(branch);
        let live = self.anchor_live(db);
        let link = match (self.checksum(cert.issued_epoch), self.checksum(snap.epoch)) {
            (Ok(at), Ok(now)) => LogLink {
                matches: cert.issued_epoch <= snap.epoch && at == cert.log_checksum_at_issue,
                current_checksum: now,
            },
            (Err(r), _) | (_, Err(r)) => return r,
        };
        let validation = match db.validate_certificate(&cert, &live, link) {
            Ok(v) => v,
            Err(e) => return err("HistoryUnavailable", format!("{e:?}")),
        };
        let ((owner, owner_agent), summary, copied, skipped) = reply;
        Response::HandoffAccepted {
            id,
            owner,
            owner_agent,
            summary,
            validation,
            copied,
            skipped,
        }
    }
}
