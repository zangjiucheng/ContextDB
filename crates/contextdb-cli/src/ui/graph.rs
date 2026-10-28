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
    Ok(Snapshot {
        kernel,
        epoch,
        rows,
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
        let committed: BTreeMap<String, Vec<u8>> = v
            .logicals()
            .filter_map(|l| {
                let (_, k, lane) = v.logical_info(l)?;
                let w = v.watermark(&lane)?;
                (k == RecordKind::CodeAnchor).then(|| (lane, w.to_vec()))
            })
            .collect();
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
                }))
            })
            .collect();
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
                Some(json!({
                    "id": r.0, "logical": l.0,
                    "head": v.head(l) == Some(r),
                    "base": v.is_base(r).unwrap_or(false),
                    "summary": label, "class": class,
                    "l0": node.l0, "l1": node.l1, "l2": node.l2,
                    "support": node.support,
                    "branch_only": node.dimensions.currency == Currency::BranchOnly,
                }))
            })
            .collect();
        let inputs = v.db().inputs();
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
