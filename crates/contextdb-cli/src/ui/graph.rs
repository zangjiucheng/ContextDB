//! The context graph, read straight from the op_log (the only source of
//! truth, SPEC §3.5 #7): every frame `log::scan` verifies is replayed into a
//! private multi-branch `Kernel` (as the store replays, ADR 0013), so
//! branches, logicals, revisions, rules, edge kinds, Conflict artifacts and
//! derived states are exact. Nothing is written and the store lock is not
//! taken, so this runs next to a live `ctxd`; a group still being written is
//! simply not part of the verified prefix yet.
use contextdb_core::{BranchView, Kernel, Proof};
use contextdb_model::*;
use contextdb_store::{StdDisk, log};
use contextdb_team::{replica::Replica, store::TeamStore};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// One replayed view of a data directory.
pub struct Snapshot {
    pub kernel: Kernel,
    pub epoch: Epoch,
    /// Per epoch: who committed what, on which branch (for the timeline).
    pub rows: Vec<Commit>,
    /// Each joined team's replica (by lowercase hex id), read from its own
    /// log: mirrors carry no rules, the root's rules live there (ADR 0018).
    pub teams: BTreeMap<String, Result<Replica, String>>,
}
pub struct Commit {
    pub epoch: Epoch,
    pub branch: BranchId,
    pub principal: PrincipalId,
    pub channel: Channel,
    pub ops: Vec<&'static str>,
    /// Branch ops name their branch (create, archive, merge source).
    pub names: Vec<String>,
    pub head_moves: Vec<(LogicalId, Option<RevisionId>, RevisionId)>,
    pub status_diff: Vec<RevisionId>,
}
/// Size and modification time of every op_log segment.
pub type Fingerprint = Vec<(PathBuf, u64, Option<SystemTime>)>;
/// The snapshot is rebuilt only when the fingerprint changes.
pub fn fingerprint(dir: &Path) -> Fingerprint {
    let mut out: Vec<_> = std::fs::read_dir(dir.join("log"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((e.path(), m.len(), m.modified().ok()))
        })
        .collect();
    // A team's log too: its root rules can change without a local commit.
    out.extend(team_dirs(dir).into_iter().filter_map(|(_, d)| {
        let m = std::fs::metadata(d.join("log")).ok()?;
        Some((d.join("log"), m.len(), m.modified().ok()))
    }));
    out.sort();
    out
}
/// `(hex id, directory)` of every joined team (`<data>/teams/<hex>/anchor`).
fn team_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<_> = std::fs::read_dir(dir.join(contextdb_daemon::team::TEAMS_DIR))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.join("anchor").is_file())
        .filter_map(|d| Some((d.file_name()?.to_str()?.to_owned(), d)))
        .collect();
    out.sort();
    out
}
pub fn load(dir: &Path) -> Result<Snapshot, String> {
    let disk = StdDisk::default();
    let scan = log::scan(&disk, &dir.join("log")).map_err(|e| format!("scan: {e}"))?;
    if let Some(f) = scan.newer_format.or(scan.older_format) {
        return Err(format!(
            "op_log format {f} is not the one this viewer reads (run the matching ctx, or ctx migrate)"
        ));
    }
    if let Some((epoch, fault)) = scan.broken {
        return Err(format!(
            "op_log chain broken at epoch {}: {fault:?}",
            epoch.0
        ));
    }
    let mut kernel = Kernel::default();
    let mut rows = Vec::new();
    for f in scan.frames {
        let row = f.row;
        let batch = row
            .batch()
            .map_err(|e| format!("epoch {}: {e}", row.epoch.0))?;
        kernel
            .commit(&batch)
            .map_err(|e| format!("replay diverged at epoch {}: {e:?}", row.epoch.0))?;
        let result = row.result().ok();
        rows.push(Commit {
            epoch: row.epoch,
            branch: batch.branch,
            principal: row.principal,
            channel: row.channel,
            ops: batch.ops.iter().map(Op::name).collect(),
            names: (batch.ops.iter())
                .filter_map(|op| match op {
                    Op::CreateBranch { name } | Op::ArchiveBranch { name } => Some(name.clone()),
                    Op::Merge { source, .. } => Some(
                        (kernel.branches())
                            .find(|b| b.id == *source)
                            .map_or_else(|| format!("branch {}", source.0), |b| b.name.clone()),
                    ),
                    _ => None,
                })
                .collect(),
            head_moves: result
                .as_ref()
                .map(|r| r.net_delta.head_moves.clone())
                .unwrap_or_default(),
            status_diff: result.map(|r| r.status_diff).unwrap_or_default(),
        });
    }
    let epoch = rows.last().map_or(Epoch(0), |c| c.epoch);
    let teams = (team_dirs(dir).into_iter())
        .map(|(id, d)| {
            let r = TeamStore::read_replica(&d).map_err(|e| format!("{e:?}"));
            (id, r)
        })
        .collect();
    Ok(Snapshot {
        kernel,
        epoch,
        rows,
        teams,
    })
}
/// The summary as a short label and a class for colouring.
pub fn summary(s: &Summary) -> (String, &'static str) {
    match s {
        Summary::Certified => ("CERTIFIED".into(), "certified"),
        Summary::Conflicted => ("CONFLICTED".into(), "conflicted"),
        Summary::Unavailable => ("UNAVAILABLE".into(), "unavailable"),
        Summary::Stale(r) => (format!("STALE({})", stale(r)), "stale"),
        Summary::Pending(r) => (format!("PENDING({r:?})"), "pending"),
    }
}
fn stale(r: &StaleReason) -> String {
    match r {
        StaleReason::SupersededPinnedPremise(p) => format!("SupersededPinnedPremise r{}", p.0),
        other => format!("{other:?}"),
    }
}
fn kind(k: EdgeKind) -> &'static str {
    match k {
        EdgeKind::Pin => "pin",
        EdgeKind::Cur => "cur",
        EdgeKind::Sem => "sem",
        EdgeKind::Ref => "ref",
    }
}
/// A code anchor's key without the repository id: `path` or `path#symbol`.
/// For display only; the descriptor (ADR 0012 §7) is authoritative.
fn anchor_label(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("code:")?;
    Some(rest.split_once('/').map_or(rest, |(_, p)| p))
}
fn proof(p: &Proof) -> Value {
    let e = &p.explanation;
    json!({
        "rev": e.revision.0,
        "base": e.base,
        "witness": match e.witness {
            Some(Witness::Base) => json!("base"),
            Some(Witness::Rule(j)) => json!(j.0),
            Some(Witness::Redacted) => json!("redacted"),
            None => Value::Null,
        },
        "active_rules": e.active_rules.iter().map(|j| j.0).collect::<Vec<_>>(),
        "references": e.references.iter().map(|(j, r)| json!({ "rule": j.0, "rev": r.0 })).collect::<Vec<_>>(),
        "premises": p.premises.iter().map(proof).collect::<Vec<_>>(),
        "truncated": p.truncated,
    })
}
fn why_not(w: &WhyNot) -> Value {
    json!({
        "rev": w.revision.0,
        "rules": w.rules.iter().map(|f| json!({
            "rule": f.rule.0,
            "disabled": f.disabled,
            "gate": format!("{:?}", f.gate),
            "incidences": f.incidences.iter().map(|i| match i {
                IncidenceFailure::Unsupported(p) => json!({ "type": "unsupported", "premise": p.0 }),
                IncidenceFailure::NotCurrent { premise, head } =>
                    json!({ "type": "not_current", "premise": premise.0, "head": head.0 }),
                IncidenceFailure::SupersededPinned { premise, head } =>
                    json!({ "type": "superseded_pinned", "premise": premise.0, "head": head.0 }),
                IncidenceFailure::SemanticHeadUnsupported(h) =>
                    json!({ "type": "semantic_head_unsupported", "head": h.0 }),
            }).collect::<Vec<_>>(),
            "upstream": f.upstream.iter().map(why_not).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}
impl Snapshot {
    /// The root's rules over the team records this branch mirrors (see
    /// [`team_rules`]), with ids that never collide with local rule ids.
    fn team_rules(&self, v: &BranchView<'_>) -> Vec<Value> {
        let mut out = Vec::new();
        for (id, replica) in &self.teams {
            if let Ok(replica) = replica {
                out.extend(team_rules(v, id, |path| replica.why(path)));
            }
        }
        for (n, rule) in out.iter_mut().enumerate() {
            rule["id"] = json!(format!("root {n}"));
        }
        out
    }
    pub fn view(&self, branch: BranchId) -> Result<BranchView<'_>, String> {
        self.kernel
            .branch(branch)
            .ok_or_else(|| format!("no branch {}", branch.0))
    }
    /// Every branch, `main` first.
    pub fn branches(&self) -> Value {
        let mut out: Vec<_> = self.kernel.branches().collect();
        out.sort_by_key(|b| b.id);
        json!(
            out.iter()
                .map(|b| json!({
                    "id": b.id.0, "name": b.name, "parent": b.parent.map(|p| p.0),
                    "fork_epoch": b.fork_epoch.0, "archived": b.archived,
                }))
                .collect::<Vec<_>>()
        )
    }
    /// The revisions visible on a branch, with global ids.
    fn revisions<'a>(v: &'a BranchView<'a>) -> impl Iterator<Item = RevisionId> + 'a {
        (0..v.revision_count() as u32).map(move |n| v.global_revision(RevisionId(n)))
    }
    /// One branch's logicals, revisions with derived state, rules with
    /// premises, references and edge kinds, and Conflict artifacts.
    pub fn graph(&self, branch: BranchId) -> Result<Value, String> {
        let v = self.view(branch)?;
        // Observation as of the last committed watermark of each anchor
        // scope: a replay has no live position, and lag is ctxd's to report.
        let inputs = v.db().inputs();
        let mut committed: BTreeMap<String, Vec<u8>> = v
            .logicals()
            .filter_map(|l| {
                let (_, k, lane) = v.logical_info(l)?;
                let w = v.watermark(&lane)?;
                (k == RecordKind::CodeAnchor).then(|| (lane, w.to_vec()))
            })
            .collect();
        // Team mirrors likewise, as of the branch's last catch-up: without a
        // `team:<id>` position every mirror read as PENDING(ReplicationLag)
        // (cert.rs `lags`), although ctxd reads them CERTIFIED.
        committed.extend(
            (inputs.watermarks.iter())
                .filter(|(scope, _)| scope.starts_with("team:"))
                .map(|(scope, w)| (scope.clone(), w.clone())),
        );
        let logicals: Vec<Value> = v
            .logicals()
            .filter_map(|l| {
                let (key, k, lane) = v.logical_info(l)?;
                let anchor = (k == RecordKind::CodeAnchor)
                    .then(|| anchor_label(&key).map(str::to_owned))
                    .flatten();
                Some(json!({
                    "id": l.0, "key": key, "kind": format!("{k:?}"), "lane": lane,
                    "head": v.head(l).map(|r| r.0), "anchor": anchor,
                    "team": mirror_path(&key).map(|(team, _)| team),
                }))
            })
            .collect();
        // A mirror revision's local twin: the revision of the local record
        // whose key is the mirror's team path, with the same content (the
        // store published its own record and imported it back). The page
        // can show the pair once; nothing here changes the data.
        let index = content_index(&v);
        let twin = |r: RevisionId| -> Option<u32> {
            let (key, ..) = v.logical_info(v.revision_logical(r)?)?;
            let (_, path) = mirror_path(&key)?;
            let local = v
                .logical_by_key(path)
                .filter(|_| mirror_path(path).is_none())?;
            index
                .get(&(local, v.revision_content(r).ok()?))
                .map(|t| t.0)
        };
        let mut rules = Vec::new();
        let revisions: Vec<Value> = Self::revisions(&v)
            .filter_map(|r| {
                let l = v.revision_logical(r)?;
                let node = v.node(r).ok()?;
                let (label, class) = summary(&v.summary_at(r, &committed).ok()?);
                for (rule, enabled, premises, references) in v.rules_concluding(r) {
                    rules.push(json!({
                        "id": rule.0, "conclusion": r.0, "enabled": enabled,
                        "premises": premises.iter().map(|p| json!({
                            "rev": p.premise.0, "kind": kind(p.kind),
                        })).collect::<Vec<_>>(),
                        "references": references.iter().map(|x| x.0).collect::<Vec<_>>(),
                    }));
                }
                let premises = (v.logical_info(l).map(|(_, k, _)| k) == Some(RecordKind::Summary))
                    .then(|| premise_count(&v, r))
                    .flatten()
                    .map(|(moved, total)| json!({ "moved": moved, "total": total }));
                Some(json!({
                    "id": r.0, "logical": l.0,
                    "head": v.head(l) == Some(r),
                    "premises": premises,
                    "base": v.is_base(r).unwrap_or(false),
                    "summary": label, "class": class,
                    "l0": node.l0, "l1": node.l1, "l2": node.l2,
                    "support": node.support,
                    "branch_only": node.dimensions.currency == Currency::BranchOnly,
                    "twin": twin(r),
                }))
            })
            .collect();
        rules.extend(self.team_rules(&v));
        let conflicts: Vec<Value> = (inputs.artifacts.iter().enumerate())
            .filter(|(_, a)| a.ty == ArtifactType::Conflict)
            .map(|(n, a)| {
                json!({
                    "id": v.global_artifact(ArtifactId(n as u32)).0,
                    "target": v.global_revision(a.target).0,
                    "open": a.open,
                    "origin_branch": a.origin_branch.0,
                    "origin_epoch": a.origin_epoch.0,
                })
            })
            .collect();
        Ok(json!({
            "epoch": self.epoch.0, "branch": branch.0, "branches": self.branches(),
            "logicals": logicals, "revisions": revisions, "rules": rules, "conflicts": conflicts,
        }))
    }
    /// WHY / WHY-NOT / IMPACT for one revision on a branch.
    pub fn explain(&self, branch: BranchId, r: RevisionId) -> Result<Value, String> {
        let v = self.view(branch)?;
        let l = v
            .revision_logical(r)
            .ok_or("no such revision on this branch")?;
        let impact = v.impact_reachable(&[r]).map_err(|e| format!("{e:?}"))?;
        let why = v.why_depth(r, 3).map_err(|e| format!("{e:?}"))?;
        let not = v.why_not(r, 2).map_err(|e| format!("{e:?}"))?;
        let history: Vec<Value> = Self::revisions(&v)
            .filter(|x| v.revision_logical(*x) == Some(l))
            .map(|x| json!(x.0))
            .collect();
        let moves: Vec<Value> = self
            .rows
            .iter()
            .filter(|c| c.branch == branch)
            .flat_map(|c| {
                c.head_moves
                    .iter()
                    .filter(|(ml, _, _)| *ml == l)
                    .map(|(_, from, to)| json!({ "epoch": c.epoch.0, "from": from.map(|f| f.0), "to": to.0 }))
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(json!({
            "rev": r.0,
            "logical": l.0,
            "content": v.revision_content(r).map(|h| hex(&h)).unwrap_or_default(),
            "why": proof(&why),
            "why_not": why_not(&not),
            "impact": {
                "supported": impact.supported.iter().map(|x| x.0).collect::<Vec<_>>(),
                "reference_only": impact.reference_only.iter().map(|x| x.0).collect::<Vec<_>>(),
            },
            "revisions": history,
            "head_moves": moves,
        }))
    }
    /// The commit history, newest first, with each commit's branch.
    pub fn timeline(&self, limit: usize) -> Value {
        let names: BTreeMap<BranchId, String> = self
            .kernel
            .branches()
            .map(|b| (b.id, b.name.clone()))
            .collect();
        let items: Vec<Value> = self
            .rows
            .iter()
            .rev()
            .take(limit)
            .map(|c| {
                json!({
                    "epoch": c.epoch.0,
                    "branch": c.branch.0,
                    "branch_name": names.get(&c.branch).cloned().unwrap_or_else(|| format!("branch {}", c.branch.0)),
                    "principal": c.principal.0,
                    "channel": format!("{:?}", c.channel),
                    "ops": c.ops,
                    "names": c.names,
                    "head_moves": c.head_moves.iter().map(|(l, from, to)| json!({
                        "logical": l.0, "from": from.map(|f| f.0), "to": to.0,
                    })).collect::<Vec<_>>(),
                    "status_diff": c.status_diff.iter().map(|r| r.0).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({ "epoch": self.epoch.0, "commits": items })
    }
}
/// `(team hex id, path)` of a mirror's key, `team/<id>/<path>`.
fn mirror_path(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix("team/")?.split_once('/')
}
/// Each revision visible on the branch by `(logical, content)`: a revision
/// is identified by them (SPEC §2.1).
fn content_index(v: &BranchView<'_>) -> BTreeMap<(LogicalId, ContentHash), RevisionId> {
    let mut out = BTreeMap::new();
    for r in Snapshot::revisions(v) {
        if let (Some(l), Ok(c)) = (v.revision_logical(r), v.revision_content(r)) {
            out.insert((l, c), r);
        }
    }
    out
}
/// The rules `why` gives for team `id`'s records (the root's, kept in the
/// replica: mirrors carry none, ADR 0018 §5.1), between the revisions `v`
/// mirrors. Each incidence goes to the mirror revision holding the root's
/// content; one the branch has not caught up to is left out, and a rule
/// left with no incidence is dropped.
fn team_rules(
    v: &BranchView<'_>,
    id: &str,
    why: impl Fn(&str) -> Option<contextdb_team::replica::Why>,
) -> Vec<Value> {
    let by_content = content_index(v);
    let rev = |path: &str, c: &ContentHash| {
        let l = v.logical_by_key(&format!("team/{id}/{path}"))?;
        by_content.get(&(l, *c)).copied()
    };
    let prefix = format!("team/{id}/");
    let mut out = Vec::new();
    for l in v.logicals() {
        let Some((key, ..)) = v.logical_info(l) else {
            continue;
        };
        let Some(path) = key.strip_prefix(&prefix) else {
            continue;
        };
        let Some(why) = why(path) else { continue };
        let Some(conclusion) = rev(path, &why.head) else {
            continue;
        };
        for (enabled, incidences) in why.rules {
            let (mut premises, mut references) = (Vec::new(), Vec::new());
            for (p, k, c) in &incidences {
                let Some(r) = rev(p, c) else { continue };
                if *k == EdgeKind::Ref {
                    references.push(json!(r.0));
                } else {
                    premises.push(json!({ "rev": r.0, "kind": kind(*k) }));
                }
            }
            if premises.is_empty() && references.is_empty() {
                continue;
            }
            out.push(json!({
                "team": true, "conclusion": conclusion.0, "enabled": enabled,
                "premises": premises, "references": references,
            }));
        }
    }
    out
}
/// A Summary revision's pending count (ADR 0016 §6), as `ctxd` reports it:
/// of its witness rule's Sem premises (the first enabled rule concluding
/// `r` with one, in rule-id order), how many differ from the nearest
/// Verified vector, or from the asserted vector when none is Verified.
fn premise_count(v: &BranchView<'_>, r: RevisionId) -> Option<(usize, usize)> {
    let mut rules = v.rules_concluding(r);
    rules.sort_by_key(|(j, ..)| *j);
    let (j, ..) = rules.into_iter().find(|(_, enabled, premises, _)| {
        *enabled && premises.iter().any(|p| p.kind == EdgeKind::Sem)
    })?;
    let sem = v.rule_semantic(j)?;
    let mut bases: Vec<HeadVector> = (v.rule_validations(j).into_iter())
        .filter(|(_, rec)| rec.verdict == Verdict::Verified)
        .map(|(vec, _)| vec)
        .collect();
    if bases.is_empty() {
        bases.push(sem.asserted.clone());
    }
    let moved = |base: &HeadVector| (sem.current.iter()).filter(|x| !base.contains(x)).count();
    Some((
        bases.iter().map(moved).min().unwrap_or(0),
        sem.current.len(),
    ))
}
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use contextdb_store::{OpenOptions, Store};
    /// A store written through the real commit path replays into the same
    /// graph: logicals, heads, rules with edge kinds, and derived states
    /// (a Cur dependent of a superseded premise is STALE).
    #[test]
    fn the_op_log_replays_into_the_graph() {
        let dir = std::env::temp_dir().join(format!("ctx-ui-graph-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) =
                Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
            let x = store.put_blob(b"x v1").unwrap();
            let y = store.put_blob(b"y").unwrap();
            let x2 = store.put_blob(b"x v2").unwrap();
            let create = |key: &str, content, base| Op::CreateLogical {
                origin: Default::default(),
                key: key.into(),
                kind: RecordKind::Decision,
                scope_lane: "repo".into(),
                content,
                adopt_as_base: base,
            };
            store
                .commit(&Batch::new(vec![create("x", x, true)]))
                .unwrap();
            store
                .commit(&Batch::new(vec![
                    create("y", y, false),
                    Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: vec![(RevisionId(0).into(), EdgeKind::Cur)],
                        enabled: true,
                    },
                ]))
                .unwrap();
            store
                .commit(&Batch::new(vec![
                    Op::AddRevision {
                        logical: LogicalId(0).into(),
                        content: x2,
                        adopt_as_base: true,
                    },
                    Op::SetHead {
                        logical: LogicalId(0).into(),
                        expected: RevisionId(0).into(),
                        new: RevRef::New(0),
                    },
                ]))
                .unwrap();
        }
        let snap = load(&dir).unwrap();
        assert_eq!(snap.epoch, Epoch(3));
        let g = snap.graph(MAIN).unwrap();
        assert_eq!(g["logicals"].as_array().unwrap().len(), 2);
        assert_eq!(g["logicals"][0]["head"], 2);
        let rule = &g["rules"][0];
        assert_eq!(
            (
                rule["conclusion"].clone(),
                rule["premises"][0]["rev"].clone()
            ),
            (json!(1), json!(0))
        );
        assert_eq!(rule["premises"][0]["kind"], "cur");
        let y = &g["revisions"][1];
        assert_eq!(y["class"], "stale", "{y}");
        assert_eq!(g["revisions"][2]["class"], "certified");
        // Details, history and the timeline.
        let d = snap.explain(MAIN, RevisionId(0)).unwrap();
        assert_eq!(d["revisions"], json!([0, 2]));
        assert_eq!(d["head_moves"].as_array().unwrap().len(), 2);
        assert_eq!(d["impact"]["supported"], json!([1]));
        let t = snap.timeline(10);
        assert_eq!(t["commits"][0]["epoch"], 3);
        assert_eq!(t["commits"][2]["ops"], json!(["CreateLogical"]));
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A team mirror reads as of its branch's last committed catch-up, as
    /// an anchor does: CERTIFIED, not PENDING(ReplicationLag) for want of a
    /// live position the replay cannot have. The root's rules, which the
    /// replica holds and mirrors do not, become edges between the mirror
    /// revisions holding the root's contents (v0.4.2).
    #[test]
    fn team_mirrors_read_as_of_the_catch_up_with_the_root_rules() {
        use contextdb_model::{Channel, Origin};
        use contextdb_team::replica::Why;
        let dir = std::env::temp_dir().join(format!("ctx-ui-team-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let team = [0xab; 16];
        let id = "ab".repeat(16);
        let mut contents = Vec::new();
        {
            let (mut store, _) =
                Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
            let mut ops = Vec::new();
            for path in ["a", "b", "c"] {
                let content = store.put_blob(path.as_bytes()).unwrap();
                contents.push(content);
                ops.push(Op::CreateLogical {
                    origin: Origin::Replica { team },
                    key: format!("team/{id}/{path}"),
                    kind: RecordKind::Decision,
                    scope_lane: format!("team:{id}/main"),
                    content,
                    adopt_as_base: true,
                });
            }
            ops.push(Op::ObserverWatermark {
                scope: format!("team:{id}"),
                expected: None,
                position: b"g0:1".to_vec(),
                gap: false,
            });
            let mut b = Batch::new(ops);
            b.channel = Channel::Replica;
            store.commit(&b).unwrap();
        }
        let snap = load(&dir).unwrap();
        let g = snap.graph(MAIN).unwrap();
        for r in g["revisions"].as_array().unwrap() {
            assert_eq!(r["class"], "certified", "{r}");
        }
        // No team directory: no replica, no root rules.
        assert!(snap.teams.is_empty());
        assert_eq!(g["rules"], json!([]));
        // The root: a from (b cur, c ref, an unknown record), a disabled
        // rule over the unknown record only, and b's head at a content this
        // branch has not caught up to.
        let (ca, cb, cc) = (contents[0], contents[1], contents[2]);
        let why = |path: &str| {
            let rules = match path {
                "a" => vec![
                    (
                        true,
                        vec![
                            ("b".to_owned(), EdgeKind::Cur, cb),
                            ("c".to_owned(), EdgeKind::Ref, cc),
                            ("gone".to_owned(), EdgeKind::Pin, [9; 32]),
                        ],
                    ),
                    (false, vec![("gone".to_owned(), EdgeKind::Pin, [9; 32])]),
                ],
                "b" => vec![(true, vec![("c".to_owned(), EdgeKind::Pin, cc)])],
                _ => return None,
            };
            let head = if path == "a" { ca } else { [7; 32] };
            Some(Why {
                head,
                status: Summary::Certified,
                base: true,
                rules,
            })
        };
        let v = snap.view(MAIN).unwrap();
        assert_eq!(
            team_rules(&v, &id, why),
            vec![json!({
                "team": true, "conclusion": 0, "enabled": true,
                "premises": [{ "rev": 1, "kind": "cur" }], "references": [2],
            })]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A mirror's revision names its local twin, the local record with the
    /// mirror's team path as key and the same content (a store that
    /// published its own record and imported it back), so the page can show
    /// the pair once. A mirror whose content differs has none.
    #[test]
    fn mirrors_name_their_local_twin() {
        use contextdb_model::{Channel, Origin};
        let dir = std::env::temp_dir().join(format!("ctx-ui-twin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let team = [0xab; 16];
        let id = "ab".repeat(16);
        {
            let (mut store, _) =
                Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
            let same = store.put_blob(b"same").unwrap();
            let local_b = store.put_blob(b"local b").unwrap();
            let team_b = store.put_blob(b"team b").unwrap();
            let local = |key: &str, content| Op::CreateLogical {
                origin: Origin::Local,
                key: key.into(),
                kind: RecordKind::Decision,
                scope_lane: "project".into(),
                content,
                adopt_as_base: true,
            };
            store
                .commit(&Batch::new(vec![local("p/a", same), local("p/b", local_b)]))
                .unwrap();
            let mirror = |path: &str, content| Op::CreateLogical {
                origin: Origin::Replica { team },
                key: format!("team/{id}/{path}"),
                kind: RecordKind::Decision,
                scope_lane: format!("team:{id}/main"),
                content,
                adopt_as_base: true,
            };
            let mut b = Batch::new(vec![mirror("p/a", same), mirror("p/b", team_b)]);
            b.channel = Channel::Replica;
            store.commit(&b).unwrap();
        }
        let g = load(&dir).unwrap().graph(MAIN).unwrap();
        let team_of: Vec<Value> = (g["logicals"].as_array().unwrap().iter())
            .map(|l| l["team"].clone())
            .collect();
        assert_eq!(
            team_of,
            vec![json!(null), json!(null), json!(id), json!(id)]
        );
        let twins: Vec<Value> = (g["revisions"].as_array().unwrap().iter())
            .map(|r| r["twin"].clone())
            .collect();
        assert_eq!(twins, vec![json!(null), json!(null), json!(0), json!(null)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A Summary head carries its changed-premise count (ADR 0016 §6);
    /// other kinds carry none.
    #[test]
    fn summaries_carry_their_changed_premise_count() {
        let dir = std::env::temp_dir().join(format!("ctx-ui-summary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) =
                Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
            let f1 = store.put_blob(b"fact v1").unwrap();
            let f2 = store.put_blob(b"fact v2").unwrap();
            let sum = store.put_blob(b"digest").unwrap();
            store
                .commit(&Batch::new(vec![
                    Op::CreateLogical {
                        origin: Default::default(),
                        key: "f".into(),
                        kind: RecordKind::Fact,
                        scope_lane: "repo".into(),
                        content: f1,
                        adopt_as_base: true,
                    },
                    Op::CreateLogical {
                        origin: Default::default(),
                        key: "s".into(),
                        kind: RecordKind::Summary,
                        scope_lane: "repo".into(),
                        content: sum,
                        adopt_as_base: false,
                    },
                    Op::AddRule {
                        conclusion: RevRef::New(1),
                        incidences: vec![(RevRef::New(0), EdgeKind::Sem)],
                        enabled: true,
                    },
                ]))
                .unwrap();
            store
                .commit(&Batch::new(vec![
                    Op::AddRevision {
                        logical: LogicalId(0).into(),
                        content: f2,
                        adopt_as_base: true,
                    },
                    Op::SetHead {
                        logical: LogicalId(0).into(),
                        expected: RevisionId(0).into(),
                        new: RevRef::New(0),
                    },
                ]))
                .unwrap();
        }
        let g = load(&dir).unwrap().graph(MAIN).unwrap();
        let rev = |id: u64| {
            g["revisions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(rev(1)["premises"], json!({ "moved": 1, "total": 1 }));
        assert_eq!(rev(1)["class"], "pending");
        assert_eq!(rev(2)["premises"], Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A log with branch ops replays (a single-branch `Db` refuses them):
    /// each branch has its own view, a conflicting merge shows its Conflict
    /// artifact, a reference-only rule keeps its references, and the
    /// timeline names branches.
    #[test]
    fn branches_conflicts_and_references_replay() {
        let dir = std::env::temp_dir().join(format!("ctx-ui-branches-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (mut store, _) =
                Store::open(StdDisk::default(), &dir, &OpenOptions::default()).unwrap();
            let mut blob = |b: &[u8]| store.put_blob(b).unwrap();
            let (a1, s1, a2, a3, r1) = (
                blob(b"a1"),
                blob(b"s1"),
                blob(b"a2"),
                blob(b"a3"),
                blob(b"r"),
            );
            let create = |key: &str, kind, content| Op::CreateLogical {
                origin: Default::default(),
                key: key.into(),
                kind,
                scope_lane: "repo".into(),
                content,
                adopt_as_base: true,
            };
            let on = |branch, ops| {
                let mut b = Batch::new(ops);
                b.branch = branch;
                b
            };
            let move_a = |content| {
                vec![
                    Op::AddRevision {
                        logical: LogicalId(0).into(),
                        content,
                        adopt_as_base: true,
                    },
                    Op::SetHead {
                        logical: LogicalId(0).into(),
                        expected: RevisionId(0).into(),
                        new: RevRef::New(0),
                    },
                ]
            };
            store
                .commit(&Batch::new(vec![
                    create("a", RecordKind::Decision, a1),
                    create("s", RecordKind::Summary, s1),
                ]))
                .unwrap();
            store
                .commit(&Batch::new(vec![Op::CreateBranch {
                    name: "feat".into(),
                }]))
                .unwrap();
            let feat = BranchId(1);
            store.commit(&on(feat, move_a(a2))).unwrap();
            store.commit(&on(MAIN, move_a(a3))).unwrap();
            store
                .commit(&on(
                    MAIN,
                    vec![Op::Merge {
                        source: feat,
                        require_certified: false,
                        certify: Vec::new(),
                    }],
                ))
                .unwrap();
            store
                .commit(&Batch::new(vec![
                    create("r", RecordKind::Decision, r1),
                    Op::AddRule {
                        conclusion: RevRef::New(0),
                        incidences: vec![(RevisionId(1).into(), EdgeKind::Ref)],
                        enabled: true,
                    },
                ]))
                .unwrap();
        }
        let snap = load(&dir).unwrap();
        let main = snap.graph(MAIN).unwrap();
        let names: Vec<_> = (main["branches"].as_array().unwrap().iter())
            .map(|b| b["name"].clone())
            .collect();
        assert_eq!(names, vec![json!("main"), json!("feat")]);
        let conflicts = main["conflicts"].as_array().unwrap();
        assert_eq!(conflicts.len(), 1, "{main}");
        assert_eq!(conflicts[0]["open"], true);
        let target = conflicts[0]["target"].as_u64().unwrap();
        let t = (main["revisions"].as_array().unwrap().iter())
            .find(|r| r["id"] == target)
            .unwrap();
        assert_eq!(t["class"], "conflicted", "{t}");
        let rule = (main["rules"].as_array().unwrap().iter())
            .find(|j| j["premises"].as_array().unwrap().is_empty())
            .expect("the reference-only rule");
        assert_eq!(rule["references"], json!([1]));
        let r = rule["conclusion"].as_u64().unwrap() as u32;
        let d = snap.explain(MAIN, RevisionId(r)).unwrap();
        assert_eq!(d["why"]["witness"], "base");
        assert_eq!(d["why"]["references"][0]["rev"], 1, "{d}");
        let s = snap.explain(MAIN, RevisionId(1)).unwrap();
        assert_eq!(s["impact"]["reference_only"], json!([r]));
        // feat sees its own head of a.
        let feat = snap.graph(BranchId(1)).unwrap();
        assert_eq!(feat["logicals"][0]["head"], 2, "{feat}");
        assert!(snap.graph(BranchId(9)).is_err());
        let t = snap.timeline(10);
        let merge = (t["commits"].as_array().unwrap().iter())
            .find(|c| c["ops"] == json!(["Merge"]))
            .unwrap();
        assert_eq!(
            (merge["branch_name"].clone(), merge["names"].clone()),
            (json!("main"), json!(["feat"]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
