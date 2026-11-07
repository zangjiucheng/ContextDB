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
// - Across principals (P9, ADR 0017 §3.4) the sender names the recipient
//   (`to`); an unnamed handoff is accepted only by the sender's own
//   principal. Listing and accept show only the handoffs the caller may
//   accept. The accept copies only the reads the acceptor may read, counts
//   the rest (`dropped_unauthorized`), and the verdict is the acceptor's own
//   validation of what was copied: the sender's certificate is never
//   validated for, or shown to, another principal.
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
    /// The sending principal (its class) and the addressed one.
    from: PrincipalId,
    to: Option<PrincipalId>,
    epoch: Epoch,
    accepted: Option<(SessionId, Agent)>,
    /// Reads copied, skipped and dropped as unauthorised by the accept (0
    /// after a restart).
    copied: u32,
    skipped: u32,
    dropped: u32,
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
                principal,
                to,
            } => {
                self.map.entry(*id).or_insert_with(|| Handoff {
                    owner: (*session_id, agent.clone()),
                    branch: *branch,
                    cert: (**cert).clone(),
                    summary: summary.clone(),
                    to_agent: to_agent.clone(),
                    from: principal.unwrap_or(PrincipalId::STORE_OWNER),
                    to: *to,
                    epoch: *epoch,
                    accepted: None,
                    copied: 0,
                    skipped: 0,
                    dropped: 0,
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
impl Handoff {
    /// May a session of principal `p` (its class) accept this handoff?
    fn addressed_to(&self, p: PrincipalId) -> bool {
        self.to.map_or(self.from == p, |t| t == p)
    }
}
impl<D: Durability + Clone + 'static> Service<D> {
    /// May principal `p` read `logical` on `branch`, under the current
    /// access document (ADR 0017 §3.4, §4.4)?
    pub(crate) fn may_read(&self, p: PrincipalId, logical: LogicalId, branch: BranchId) -> bool {
        self.actor.snapshot().view(branch).authorizes(p, logical)
    }
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
        to: Option<String>,
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
        let (branch, from) = {
            let s = s.lock().expect("session");
            if s.ended_epoch.is_some() {
                return err("InvalidArgument", "the session has ended");
            }
            (s.branch, crate::principals::class(s.principal))
        };
        let to = match to {
            None => None,
            Some(name) => match self.principals().principal_of_name(&name) {
                Some(p) => Some(p),
                None => {
                    return err("InvalidArgument", format!("no principal named {name}"));
                }
            },
        };
        // Addressing one's own principal is the unnamed handoff.
        let to = to.filter(|t| *t != from);
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
                from,
                to,
                epoch: snap.epoch,
                accepted: None,
                copied: 0,
                skipped: 0,
                dropped: 0,
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
            principal: (from != PrincipalId::STORE_OWNER).then_some(from),
            to,
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
        let (branch, me_p) = {
            let s = s.lock().expect("session");
            if s.ended_epoch.is_some() {
                return err("InvalidArgument", "the session has ended");
            }
            (s.branch, crate::principals::class(s.principal))
        };
        let Some(id) = id else {
            // The open handoffs on this branch the caller may accept.
            let h = self.handoffs.lock().expect("handoffs");
            let items = (h.map.iter())
                .filter(|(_, x)| {
                    x.accepted.is_none()
                        && x.addressed_to(me_p)
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
        // A handoff not addressed to the caller's principal does not exist
        // for it (ADR 0017 §3.4).
        let Some(x) = h.map.get_mut(&id).filter(|x| x.addressed_to(me_p)) else {
            return err("UnknownRef", format!("no handoff {}", hex16(&id)));
        };
        let foreign = x.from != me_p;
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
                        if foreign && !self.may_read(me_p, e.logical, branch) {
                            x.dropped += 1;
                            continue;
                        }
                        if view.contains_key(&e.logical) {
                            x.skipped += 1;
                            continue;
                        }
                        let entry = LedgerEntry {
                            logical: e.logical,
                            rev: e.rev,
                            required: true,
                            epoch: e.since_epoch,
                            // A handoff copies the entry, not what was shown.
                            level: contextdb_model::levels::Level::Header,
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
        let mut cert = x.cert.clone();
        if foreign {
            // The acceptor's own validation of what it may read; the sender's
            // certificate itself is never validated for another principal.
            cert.entries
                .retain(|e| self.may_read(me_p, e.logical, branch));
        }
        let reply = (
            x.owner.clone(),
            x.summary.clone(),
            x.copied,
            x.skipped,
            x.dropped,
        );
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
        let ((owner, owner_agent), summary, copied, skipped, dropped_unauthorized) = reply;
        Response::HandoffAccepted {
            id,
            owner,
            owner_agent,
            summary,
            validation,
            copied,
            skipped,
            dropped_unauthorized,
        }
    }
}
