//! Routing a write (ADR 0018 §6.1, SPEC §6.1 P10 check order): a batch
//! whose write targets are all records of one team is a team proposal,
//! translated here to portable ops (§4.1); one that writes team and local
//! records, or two teams, is `MixedTeamBatch`.
//!
//! The kernel decides first. A batch it commits (or would commit) is local.
//! The kernel refuses a team write only with `ReservedNamespace` or
//! `ReplicaReadOnly`, after references resolve and after P9's `Unauthorized`
//! checks (SPEC §6.1 order), and names it `UnknownRef` when a `team/` key
//! has no mirror yet (a record only queued, or none). Only then is the batch
//! translated, so P9's protection of hidden names (`logical_first`) is the
//! kernel's own; [`translate`] adds no new way to learn about a name the
//! principal may not read. A team op that is not portable (a `Sem` premise,
//! local `dir` grants on a mirror, an artifact on a mirror, a validation)
//! keeps the kernel's refusal.
//!
//! Expected values come from the team as this edge sees it — the replica
//! with the author's readable queued proposals applied ([`Overlays`]) — not
//! from the mirrors: a mirror's base shows the team *status* (§5.1), and
//! mirrors carry no rules, so a local batch names a team rule only when it
//! adds the rule itself.

use crate::ops::{Edge, Hash, RuleSpec, TeamOp, tags_hash};
use crate::replica::Replica;
use contextdb_core::{BranchView, Kernel};
use contextdb_model::*;
use std::collections::BTreeMap;

/// Per team, the replica with the author's readable queued proposals
/// applied (the scratch overlay of §6.2). A team without one is not joined.
pub trait Overlays {
    fn overlay(&self, team: TeamId) -> Option<&Replica>;
}
impl Overlays for BTreeMap<TeamId, Replica> {
    fn overlay(&self, team: TeamId) -> Option<&Replica> {
        self.get(&team)
    }
}

/// What a write is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// A team proposal: portable ops for `team`'s queue.
    Team { team: TeamId, ops: Vec<TeamOp> },
    /// Not a team proposal: the kernel's answer stands.
    Local,
}

/// `team/<32 hex>/<path>` → `(team, path)`.
pub fn team_key(key: &str) -> Option<(TeamId, &str)> {
    let rest = key.strip_prefix("team/")?;
    let (hex, path) = rest.split_once('/')?;
    Some((parse_team(hex)?, path))
}

/// `team:<32 hex>/<lane>` → `(team, lane)`.
pub fn team_lane(lane: &str) -> Option<(TeamId, &str)> {
    let rest = lane.strip_prefix("team:")?;
    let (hex, l) = rest.split_once('/')?;
    Some((parse_team(hex)?, l))
}

fn parse_team(hex: &str) -> Option<TeamId> {
    if hex.len() != 32 || !hex.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut id = [0u8; 16];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(id)
}

/// Whether the kernel's refusal of a batch may be a team write.
pub fn may_be_team(b: &Batch, kernel: &TxError) -> bool {
    match kernel {
        TxError::ReservedNamespace | TxError::ReplicaReadOnly => true,
        TxError::UnknownRef => b.ops.iter().any(names_team_key),
        _ => false,
    }
}

fn names_team_key(op: &Op) -> bool {
    let key = |l: &LogicalRef| matches!(l, LogicalRef::Key(k) if k.starts_with("team/"));
    match op {
        Op::AddRevision { logical, .. }
        | Op::SetHead { logical, .. }
        | Op::SetMetadata { logical, .. } => key(logical),
        Op::ExpectHeads { entries, .. } => entries.iter().any(|(l, _)| key(l)),
        _ => false,
    }
}

/// One op's class.
enum Class {
    Team(TeamId, Vec<TeamOp>),
    Local,
}

struct Tr<'a> {
    v: BranchView<'a>,
    /// Keys of the logicals this batch creates, in order (`LogicalRef::New`).
    new_logicals: Vec<String>,
    /// `(key, content)` of the revisions it creates (`RevRef::New`).
    new_revs: Vec<(String, Hash)>,
    /// The rules it adds (`RuleRef::New`) with their team and flag; `None`
    /// for a local rule.
    new_rules: Vec<Option<(TeamId, RuleSpec, bool)>>,
    /// Base bits this batch already set, for `SetBase.expected`.
    base_set: Vec<((String, Hash), bool)>,
    overlays: &'a dyn Overlays,
    kernel: TxError,
}

impl Tr<'_> {
    fn key(&self, l: &LogicalRef) -> Result<String, TxError> {
        match l {
            LogicalRef::Existing(id) => (self.v.logical_info(*id))
                .map(|(k, _, _)| k)
                .ok_or(TxError::UnknownRef),
            LogicalRef::Key(k) => Ok(k.clone()),
            LogicalRef::New(n) => {
                (self.new_logicals.get(*n as usize).cloned()).ok_or(TxError::UnknownRef)
            }
        }
    }
    fn rev(&self, r: &RevRef) -> Result<(String, Hash), TxError> {
        match r {
            RevRef::Existing(id) => {
                let l = self.v.revision_logical(*id).ok_or(TxError::UnknownRef)?;
                Ok((
                    self.key(&LogicalRef::Existing(l))?,
                    self.v.revision_content(*id)?,
                ))
            }
            RevRef::New(n) => (self.new_revs.get(*n as usize).cloned()).ok_or(TxError::UnknownRef),
        }
    }
    /// The team's overlay, or the kernel's refusal for a team not joined.
    fn overlay(&self, t: TeamId) -> Result<&Replica, TxError> {
        self.overlays.overlay(t).ok_or_else(|| self.kernel.clone())
    }
    /// The team base bit of `(path, content)` as the proposer sees it.
    fn base(&self, t: TeamId, key: &str, path: &str, content: &Hash) -> Result<bool, TxError> {
        if let Some((_, b)) =
            (self.base_set.iter().rev()).find(|((k, c), _)| k == key && c == content)
        {
            return Ok(*b);
        }
        // A revision created here without adopt_as_base, or none (accept
        // then names it).
        Ok(self.overlay(t)?.base_bit(path, content).unwrap_or(false))
    }
    /// The team rule a rule reference names, with its team and current
    /// flag; `None` for a local rule. Mirrors carry no rules, so an
    /// existing rule is local.
    fn rule_spec(&self, rule: &RuleRef) -> Result<Option<(TeamId, RuleSpec, bool)>, TxError> {
        match rule {
            RuleRef::New(n) => {
                (self.new_rules.get(*n as usize).cloned()).ok_or(TxError::UnknownRef)
            }
            RuleRef::Existing(_) => Ok(None),
        }
    }
    /// A record a team proposal relies on: a path of the same team.
    fn team_path(&self, team: TeamId, key: &str) -> Result<String, TxError> {
        match team_key(key) {
            Some((t, p)) if t == team => Ok(p.to_owned()),
            _ => Err(TxError::TeamDepOutside),
        }
    }
    fn class(&mut self, op: &Op) -> Result<Class, TxError> {
        let team = |key: &str| team_key(key).map(|(t, p)| (t, p.to_owned()));
        Ok(match op {
            Op::CreateLogical {
                key,
                kind,
                scope_lane,
                content,
                adopt_as_base,
                origin: _,
            } => {
                self.new_logicals.push(key.clone());
                self.new_revs.push((key.clone(), *content));
                match (team(key), team_lane(scope_lane)) {
                    (Some((t, path)), Some((lt, lane))) if t == lt => {
                        if *adopt_as_base {
                            self.base_set.push(((key.clone(), *content), true));
                        }
                        Class::Team(
                            t,
                            vec![TeamOp::New {
                                path,
                                lane: lane.to_owned(),
                                kind: *kind,
                                content: *content,
                                adopt_as_base: *adopt_as_base,
                            }],
                        )
                    }
                    (None, None) => Class::Local,
                    // A team key in a local or other team's lane, or the
                    // reverse: a direct write to a reserved name.
                    _ => return Err(TxError::ReservedNamespace),
                }
            }
            Op::AddRevision {
                logical,
                content,
                adopt_as_base,
            } => {
                let key = self.key(logical)?;
                self.new_revs.push((key.clone(), *content));
                match team(&key) {
                    Some((t, path)) => {
                        if *adopt_as_base {
                            self.base_set.push(((key.clone(), *content), true));
                        }
                        Class::Team(
                            t,
                            vec![TeamOp::Revise {
                                path,
                                content: *content,
                                adopt_as_base: *adopt_as_base,
                            }],
                        )
                    }
                    None => Class::Local,
                }
            }
            Op::SetHead {
                logical,
                expected,
                new,
            } => {
                let key = self.key(logical)?;
                match team(&key) {
                    Some((t, path)) => {
                        let (ek, eh) = self.rev(expected)?;
                        let (nk, nh) = self.rev(new)?;
                        if ek != key || nk != key {
                            return Err(TxError::UnknownRef);
                        }
                        Class::Team(
                            t,
                            vec![TeamOp::SetHead {
                                path,
                                expected: eh,
                                new: nh,
                            }],
                        )
                    }
                    None => Class::Local,
                }
            }
            Op::AssertBase { rev } | Op::RetractBase { rev } => {
                let base = matches!(op, Op::AssertBase { .. });
                let (key, content) = self.rev(rev)?;
                match team(&key) {
                    Some((t, path)) => {
                        let expected = self.base(t, &key, &path, &content)?;
                        self.base_set.push(((key, content), base));
                        Class::Team(
                            t,
                            vec![TeamOp::SetBase {
                                path,
                                content,
                                expected,
                                base,
                            }],
                        )
                    }
                    None => Class::Local,
                }
            }
            Op::AddRule {
                conclusion,
                incidences,
                enabled,
            } => {
                let (ckey, ch) = self.rev(conclusion)?;
                let Some((t, cpath)) = team(&ckey) else {
                    // A local rule; it may rely on mirrors (§5.3).
                    self.new_rules.push(None);
                    return Ok(Class::Local);
                };
                let mut inc = Vec::with_capacity(incidences.len());
                for (r, kind) in incidences {
                    let (k, h) = self.rev(r)?;
                    let edge = match kind {
                        EdgeKind::Pin => Edge::Pin(h),
                        EdgeKind::Cur => Edge::Cur(h),
                        EdgeKind::Ref => Edge::Ref(h),
                        // O6: team rules have no Sem.
                        EdgeKind::Sem => return Err(self.kernel.clone()),
                    };
                    inc.push((self.team_path(t, &k)?, edge));
                }
                inc.sort();
                inc.dedup();
                let rule = RuleSpec {
                    conclusion: (cpath, ch),
                    incidences: inc,
                };
                self.new_rules.push(Some((t, rule.clone(), *enabled)));
                Class::Team(
                    t,
                    vec![TeamOp::AddRule {
                        rule,
                        enabled: *enabled,
                    }],
                )
            }
            Op::SetRuleEnabled { rule, enabled } => match self.rule_spec(rule)? {
                Some((t, spec, expected)) => {
                    if let RuleRef::New(n) = rule {
                        self.new_rules[*n as usize] = Some((t, spec.clone(), *enabled));
                    }
                    Class::Team(
                        t,
                        vec![TeamOp::SetRuleEnabled {
                            rule: spec,
                            expected,
                            enabled: *enabled,
                        }],
                    )
                }
                None => Class::Local,
            },
            // §4.1: `Rebind` is disable plus add.
            Op::Rebind { rule, from, to } => match self.rule_spec(rule)? {
                Some((t, spec, expected)) => {
                    let (fk, fh) = self.rev(from)?;
                    let (tk, th) = self.rev(to)?;
                    let (fp, tp) = (self.team_path(t, &fk)?, self.team_path(t, &tk)?);
                    let mut inc: Vec<(String, Edge)> = (spec.incidences.iter())
                        .map(|(p, e)| {
                            if *p == fp && e.content() == fh {
                                (
                                    tp.clone(),
                                    match e {
                                        Edge::Pin(_) => Edge::Pin(th),
                                        Edge::Cur(_) => Edge::Cur(th),
                                        Edge::Ref(_) => Edge::Ref(th),
                                    },
                                )
                            } else {
                                (p.clone(), *e)
                            }
                        })
                        .collect();
                    inc.sort();
                    inc.dedup();
                    let new = RuleSpec {
                        conclusion: spec.conclusion.clone(),
                        incidences: inc,
                    };
                    self.new_rules.push(Some((t, new.clone(), true)));
                    Class::Team(
                        t,
                        vec![
                            TeamOp::SetRuleEnabled {
                                rule: spec,
                                expected,
                                enabled: false,
                            },
                            TeamOp::AddRule {
                                rule: new,
                                enabled: true,
                            },
                        ],
                    )
                }
                None => {
                    self.new_rules.push(None);
                    Class::Local
                }
            },
            Op::SetMetadata {
                logical,
                tags,
                dirs,
            } => {
                let key = self.key(logical)?;
                match team(&key) {
                    // Local `dir` grants never travel (§5.1, Q9).
                    Some(_) if !dirs.is_empty() => return Err(self.kernel.clone()),
                    Some((t, path)) => {
                        let seen = self.overlay(t)?.record_tags(&path).unwrap_or_default();
                        Class::Team(
                            t,
                            vec![TeamOp::SetTags {
                                path,
                                expected: tags_hash(&seen),
                                tags: tags.clone(),
                            }],
                        )
                    }
                    None => Class::Local,
                }
            }
            // The read set is not a write target: it follows the writes
            // ([`translate`]).
            Op::ExpectHeads { .. } => Class::Local,
            _ => Class::Local,
        })
    }
}

/// Decides a write the kernel refused with `kernel` (see the module
/// documentation).
pub fn translate(
    k: &Kernel,
    b: &Batch,
    kernel: TxError,
    overlays: &dyn Overlays,
) -> Result<Route, TxError> {
    if !may_be_team(b, &kernel) {
        return Ok(Route::Local);
    }
    let v = k.branch(b.branch).ok_or_else(|| kernel.clone())?;
    let mut tr = Tr {
        v,
        new_logicals: Vec::new(),
        new_revs: Vec::new(),
        new_rules: Vec::new(),
        base_set: Vec::new(),
        overlays,
        kernel: kernel.clone(),
    };
    let mut team: Option<TeamId> = None;
    let mut ops = Vec::new();
    let mut local = false;
    for op in &b.ops {
        match tr.class(op)? {
            Class::Team(t, mut o) => {
                if team.is_some_and(|x| x != t) {
                    return Err(TxError::MixedTeamBatch);
                }
                team = Some(t);
                ops.append(&mut o);
            }
            Class::Local if !matches!(op, Op::ExpectHeads { .. }) => local = true,
            Class::Local => {}
        }
    }
    let reads: Vec<(String, RevisionId)> = b
        .ops
        .iter()
        .filter_map(|op| match op {
            Op::ExpectHeads { entries, .. } => Some(entries),
            _ => None,
        })
        .flatten()
        .map(|(l, r)| Ok((tr.key(l)?, *r)))
        .collect::<Result<_, TxError>>()?;
    let Some(team) = team else {
        // No team write. A local batch relying on a record only queued
        // (O3) — in the overlay, which holds only proposals the author may
        // read, but not mirrored; otherwise the kernel's answer stands.
        let queued = |key: &str| {
            team_key(key).is_some_and(|(t, p)| {
                tr.v.logical_by_key(key).is_none()
                    && overlays.overlay(t).is_some_and(|o| o.has_record(p))
            })
        };
        if reads.iter().any(|(key, _)| queued(key)) {
            return Err(TxError::Unpublished);
        }
        return Err(kernel);
    };
    if local {
        return Err(TxError::MixedTeamBatch);
    }
    // The read set in team terms (M2): `(path, content seen)`, first.
    let mut expect = Vec::with_capacity(reads.len());
    for (key, r) in reads {
        let path = tr.team_path(team, &key)?;
        let content = tr.v.revision_content(r)?;
        expect.push(TeamOp::Expect { path, content });
    }
    expect.append(&mut ops);
    Ok(Route::Team { team, ops: expect })
}
