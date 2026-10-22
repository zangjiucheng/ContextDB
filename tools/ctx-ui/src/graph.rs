//! The context graph, read straight from the op_log (the only source of
//! truth, SPEC §3.5 #7): every frame `log::scan` verifies is replayed into a
//! private `Db`, so logicals, revisions, rules, edge kinds and derived states
//! are exact. Nothing is written and the store lock is not taken, so this
//! runs next to a live `ctxd`; a group still being written is simply not part
//! of the verified prefix yet.
use contextdb_core::Db;
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
    pub db: Db,
    pub epoch: Epoch,
    /// Per epoch: who committed what (for the timeline).
    pub rows: Vec<Commit>,
}
pub struct Commit {
    pub epoch: Epoch,
    pub principal: PrincipalId,
    pub channel: Channel,
    pub ops: Vec<&'static str>,
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
            "op_log format {f} is not the one this viewer reads"
        ));
    }
    if let Some((epoch, fault)) = scan.broken {
        return Err(format!(
            "op_log chain broken at epoch {}: {fault:?}",
            epoch.0
        ));
    }
    let mut db = Db::default();
    let mut rows = Vec::new();
    for f in scan.frames {
        let row = f.row;
        let batch = row
            .batch()
            .map_err(|e| format!("epoch {}: {e}", row.epoch.0))?;
        db.commit(&batch)
            .map_err(|e| format!("replay diverged at epoch {}: {e:?}", row.epoch.0))?;
        let result = row.result().ok();
        rows.push(Commit {
            epoch: row.epoch,
            principal: row.principal,
            channel: row.channel,
            ops: batch.ops.iter().map(Op::name).collect(),
            head_moves: result
                .as_ref()
                .map(|r| r.net_delta.head_moves.clone())
                .unwrap_or_default(),
            status_diff: result.map(|r| r.status_diff).unwrap_or_default(),
        });
    }
    let epoch = rows.last().map_or(Epoch(0), |c| c.epoch);
    Ok(Snapshot { db, epoch, rows })
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
impl Snapshot {
    /// Logicals, every revision with its derived state, and every rule with
    /// its premises and edge kinds.
    pub fn graph(&self) -> Value {
        let db = &self.db;
        let none = BTreeMap::new();
        let logicals: Vec<Value> = (0..db.logical_count() as u32)
            .map(LogicalId)
            .filter_map(|l| {
                let (key, k, lane) = db.logical_info(l)?;
                Some(json!({
                    "id": l.0, "key": key, "kind": format!("{k:?}"), "lane": lane,
                    "head": db.head(l).map(|r| r.0),
                }))
            })
            .collect();
        let mut rules = Vec::new();
        let revisions: Vec<Value> = (0..db.revision_count() as u32)
            .map(RevisionId)
            .filter_map(|r| {
                let l = db.revision_logical(r)?;
                let node = db.node(r).ok()?;
                let (label, class) = summary(&db.summary_at(r, &none).ok()?);
                for (rule, enabled, premises, references) in db.rules_concluding(r) {
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
                    "head": db.head(l) == Some(r),
                    "base": db.is_base(r).unwrap_or(false),
                    "summary": label, "class": class,
                    "l0": node.l0, "l1": node.l1, "l2": node.l2,
                    "support": node.support,
                }))
            })
            .collect();
        json!({ "epoch": self.epoch.0, "logicals": logicals, "revisions": revisions, "rules": rules })
    }
    /// WHY / WHY-NOT / IMPACT for one revision, computed on the replay.
    pub fn explain(&self, r: RevisionId) -> Result<Value, String> {
        let db = &self.db;
        let l = db.revision_logical(r).ok_or("no such revision")?;
        let impact = db.impact_reachable(&[r]).map_err(|e| format!("{e:?}"))?;
        let why = db.why_depth(r, 3).map_err(|e| format!("{e:?}"))?;
        let why_not = db.why_not(r, 2).map_err(|e| format!("{e:?}"))?;
        let history: Vec<Value> = (0..db.revision_count() as u32)
            .map(RevisionId)
            .filter(|x| db.revision_logical(*x) == Some(l))
            .map(|x| json!(x.0))
            .collect();
        let moves: Vec<Value> = self
            .rows
            .iter()
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
            "content": db.revision_content(r).map(|h| hex(&h)).unwrap_or_default(),
            "why": format!("{why:#?}"),
            "why_not": format!("{why_not:#?}"),
            "impact": {
                "supported": impact.supported.iter().map(|x| x.0).collect::<Vec<_>>(),
                "reference_only": impact.reference_only.iter().map(|x| x.0).collect::<Vec<_>>(),
            },
            "revisions": history,
            "head_moves": moves,
        }))
    }
    /// The commit history, newest first.
    pub fn timeline(&self, limit: usize) -> Value {
        let items: Vec<Value> = self
            .rows
            .iter()
            .rev()
            .take(limit)
            .map(|c| {
                json!({
                    "epoch": c.epoch.0,
                    "principal": c.principal.0,
                    "channel": format!("{:?}", c.channel),
                    "ops": c.ops,
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
        let g = snap.graph();
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
        let d = snap.explain(RevisionId(0)).unwrap();
        assert_eq!(d["revisions"], json!([0, 2]));
        assert_eq!(d["head_moves"].as_array().unwrap().len(), 2);
        assert_eq!(d["impact"]["supported"], json!([1]));
        let t = snap.timeline(10);
        assert_eq!(t["commits"][0]["epoch"], 3);
        assert_eq!(t["commits"][2]["ops"], json!(["CreateLogical"]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
