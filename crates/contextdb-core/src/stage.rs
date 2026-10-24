//! Structural staging overlay (ADR 0007): the published database plus appended
//! and overridden inputs. Validation reads and writes only the overlay, so a
//! rejected batch is discarded without touching published state.
use crate::{
    Indexes, InputView,
    arena::{LogicalArena, RevisionArena, RuleArena},
    pk::PkSource,
};
use contextdb_model::*;
use std::collections::BTreeMap;

pub(crate) type RuleKey = (RevisionId, Vec<(RevisionId, u8)>, Vec<RevisionId>);
pub(crate) fn rule_key(c: RevisionId, p: &[Incidence], refs: &[RevisionId]) -> RuleKey {
    (
        c,
        p.iter().map(|p| (p.premise, p.kind as u8)).collect(),
        refs.to_vec(),
    )
}
pub(crate) struct Stage<'a> {
    pub tables: &'a Inputs,
    pub logicals: &'a LogicalArena,
    pub revisions: &'a RevisionArena,
    pub rules: &'a RuleArena,
    pub index: &'a Indexes,
    pub new_logicals: Vec<Logical>,
    pub new_heads: Vec<RevisionId>,
    pub new_revisions: Vec<Revision>,
    pub new_base: Vec<bool>,
    pub new_rules: Vec<Rule>,
    pub new_artifacts: Vec<Artifact>,
    /// Overrides of published entries only.
    pub heads: BTreeMap<LogicalId, RevisionId>,
    pub base: BTreeMap<RevisionId, bool>,
    pub enabled: BTreeMap<RuleId, bool>,
    pub artifact_open: BTreeMap<ArtifactId, bool>,
    pub metadata: BTreeMap<LogicalId, Metadata>,
    pub validations: BTreeMap<(RuleId, HeadVector), ValidationRecord>,
    pub watermarks: BTreeMap<String, Vec<u8>>,
    pub policy: Option<PolicyDoc>,
    new_keys: BTreeMap<String, LogicalId>,
    new_revision_keys: BTreeMap<(LogicalId, ContentHash), RevisionId>,
    new_rule_keys: BTreeMap<RuleKey, RuleId>,
    pub incidences: u64,
    /// `ExpectHeads` entries (SPEC §6.1 contract 12), checked at batch end
    /// against the published (pre-batch) heads.
    pub read_set: BTreeMap<LogicalId, RevisionId>,
    /// Keys taken by logicals invisible here (the global key namespace,
    /// SD19); None for a single-branch database.
    pub foreign_key: Option<&'a dyn Fn(&str) -> bool>,
}
impl<'a> Stage<'a> {
    pub fn new(
        tables: &'a Inputs,
        logicals: &'a LogicalArena,
        revisions: &'a RevisionArena,
        rules: &'a RuleArena,
        index: &'a Indexes,
    ) -> Self {
        Self {
            tables,
            logicals,
            revisions,
            rules,
            index,
            new_logicals: Vec::new(),
            new_heads: Vec::new(),
            new_revisions: Vec::new(),
            new_base: Vec::new(),
            new_rules: Vec::new(),
            new_artifacts: Vec::new(),
            heads: BTreeMap::new(),
            base: BTreeMap::new(),
            enabled: BTreeMap::new(),
            artifact_open: BTreeMap::new(),
            metadata: BTreeMap::new(),
            validations: BTreeMap::new(),
            watermarks: BTreeMap::new(),
            policy: None,
            new_keys: BTreeMap::new(),
            new_revision_keys: BTreeMap::new(),
            new_rule_keys: BTreeMap::new(),
            incidences: index.inc_rule.len() as u64,
            read_set: BTreeMap::new(),
            foreign_key: None,
        }
    }
    /// Whether another branch's logical holds `key` (SD19's one exception).
    pub fn key_taken_elsewhere(&self, key: &str) -> bool {
        self.foreign_key.is_some_and(|taken| taken(key))
    }
    /// The head before this batch; logicals created in the batch have none.
    pub fn published_head(&self, l: LogicalId) -> Option<RevisionId> {
        self.tables.heads.get(l.index()).copied()
    }
    pub fn published_logicals(&self) -> usize {
        self.logicals.len()
    }
    pub fn published_revisions(&self) -> usize {
        self.revisions.len()
    }
    pub fn published_rules(&self) -> usize {
        self.rules.len()
    }
    pub fn artifact_count(&self) -> usize {
        self.tables.artifacts.len() + self.new_artifacts.len()
    }
    pub fn find_key(&self, key: &str) -> Option<LogicalId> {
        self.new_keys
            .get(key)
            .copied()
            .or_else(|| self.logicals.find_key(key))
    }
    /// Revision with this (logical, content) identity, staged or published.
    pub fn find_revision(&self, l: LogicalId, content: &ContentHash) -> Option<RevisionId> {
        if let Some(r) = self.new_revision_keys.get(&(l, *content)) {
            return Some(*r);
        }
        // Rows past the published ones are empty, so staged logicals find none.
        self.index
            .revisions_by_logical
            .row(l.index())
            .iter()
            .copied()
            .find(|r| self.revisions.content(*r) == *content)
    }
    /// Existing rule with this canonical key, staged or published.
    pub fn find_rule(&self, key: &RuleKey) -> Option<RuleId> {
        if let Some(j) = self.new_rule_keys.get(key) {
            return Some(*j);
        }
        let (c, premises, refs) = key;
        if c.index() >= self.published_revisions() {
            return None;
        }
        self.index
            .rules_by_concl
            .row(c.index())
            .iter()
            .copied()
            .find(|j| {
                self.rules.references(*j) == refs.as_slice()
                    && self
                        .rules
                        .premises(*j)
                        .map(|p| (p.premise, p.kind as u8))
                        .eq(premises.iter().copied())
            })
    }
    pub fn artifact(&self, a: ArtifactId) -> (ArtifactType, RevisionId, bool) {
        let n = a.index();
        let published = self.tables.artifacts.len();
        if n < published {
            let x = &self.tables.artifacts[n];
            (
                x.ty,
                x.target,
                *self.artifact_open.get(&a).unwrap_or(&x.open),
            )
        } else {
            let x = &self.new_artifacts[n - published];
            (x.ty, x.target, x.open)
        }
    }
    pub fn metadata_of(&self, l: LogicalId) -> Metadata {
        // Staged logicals have no published metadata: the lookup yields the default.
        self.metadata
            .get(&l)
            .cloned()
            .unwrap_or_else(|| self.logicals.metadata(l))
    }
    pub fn watermark(&self, scope: &str) -> Option<Vec<u8>> {
        self.watermarks
            .get(scope)
            .or_else(|| self.tables.watermarks.get(scope))
            .cloned()
    }
    pub fn policy(&self) -> &PolicyDoc {
        self.policy.as_ref().unwrap_or(&self.tables.policy)
    }
    pub fn push_logical(&mut self, l: Logical, head: RevisionId) -> LogicalId {
        let id = LogicalId((self.published_logicals() + self.new_logicals.len()) as u32);
        self.new_keys.insert(l.key.clone(), id);
        self.new_logicals.push(l);
        self.new_heads.push(head);
        id
    }
    pub fn push_revision(&mut self, r: Revision, base: bool) -> RevisionId {
        let id = RevisionId((self.published_revisions() + self.new_revisions.len()) as u32);
        self.new_revision_keys.insert((r.logical, r.content), id);
        self.new_revisions.push(r);
        self.new_base.push(base);
        id
    }
    pub fn push_rule(&mut self, j: Rule) -> RuleId {
        let id = RuleId((self.published_rules() + self.new_rules.len()) as u32);
        self.new_rule_keys
            .insert(rule_key(j.conclusion, &j.premises, &j.references), id);
        self.new_rules.push(j);
        id
    }
    pub fn set_head(&mut self, l: LogicalId, r: RevisionId) {
        match l.index().checked_sub(self.published_logicals()) {
            Some(n) => self.new_heads[n] = r,
            None => {
                self.heads.insert(l, r);
            }
        }
    }
    pub fn set_base(&mut self, r: RevisionId, b: bool) {
        match r.index().checked_sub(self.published_revisions()) {
            Some(n) => self.new_base[n] = b,
            None => {
                self.base.insert(r, b);
            }
        }
    }
    pub fn set_enabled(&mut self, j: RuleId, e: bool) {
        match j.index().checked_sub(self.published_rules()) {
            Some(n) => self.new_rules[n].enabled = e,
            None => {
                self.enabled.insert(j, e);
            }
        }
    }
    pub fn set_artifact_open(&mut self, a: ArtifactId, open: bool) {
        match a.index().checked_sub(self.tables.artifacts.len()) {
            Some(n) => self.new_artifacts[n].open = open,
            None => {
                self.artifact_open.insert(a, open);
            }
        }
    }
    /// Revisions the batch made base (published overrides or new): the only A5
    /// BaseNotAllowed candidates (ADR 0007 §5). Overrides that were already
    /// base are included too; they passed A5 before and pass it again.
    pub fn base_candidates(&self) -> impl Iterator<Item = RevisionId> + '_ {
        self.base
            .iter()
            .filter(|(_, b)| **b)
            .map(|(r, _)| *r)
            .chain(
                (0..self.new_revisions.len())
                    .filter(|n| self.new_base[*n])
                    .map(|n| RevisionId((self.published_revisions() + n) as u32)),
            )
    }
    /// Whether an enabled rule concludes the staged revision `r`. Rules
    /// concluding a new revision are themselves new, so only staged rules count.
    pub fn staged_rule_enabled(&self, r: RevisionId) -> bool {
        debug_assert!(r.index() >= self.published_revisions());
        self.new_rules
            .iter()
            .any(|j| j.conclusion == r && j.enabled)
    }
}
impl InputView for Stage<'_> {
    fn revision_count(&self) -> usize {
        self.published_revisions() + self.new_revisions.len()
    }
    fn logical_count(&self) -> usize {
        self.published_logicals() + self.new_logicals.len()
    }
    fn rule_count(&self) -> usize {
        self.published_rules() + self.new_rules.len()
    }
    fn logical_kind(&self, id: LogicalId) -> RecordKind {
        match id.index().checked_sub(self.published_logicals()) {
            Some(n) => self.new_logicals[n].kind,
            None => self.logicals.kind(id),
        }
    }
    fn revision_logical(&self, id: RevisionId) -> LogicalId {
        match id.index().checked_sub(self.published_revisions()) {
            Some(n) => self.new_revisions[n].logical,
            None => self.revisions.logical(id),
        }
    }
    fn head(&self, id: LogicalId) -> RevisionId {
        match id.index().checked_sub(self.published_logicals()) {
            Some(n) => self.new_heads[n],
            None => *self
                .heads
                .get(&id)
                .unwrap_or(&self.tables.heads[id.index()]),
        }
    }
    fn base(&self, id: RevisionId) -> bool {
        match id.index().checked_sub(self.published_revisions()) {
            Some(n) => self.new_base[n],
            None => *self.base.get(&id).unwrap_or(&self.tables.base[id.index()]),
        }
    }
    fn enabled(&self, id: RuleId) -> bool {
        match id.index().checked_sub(self.published_rules()) {
            Some(n) => self.new_rules[n].enabled,
            None => *self
                .enabled
                .get(&id)
                .unwrap_or(&self.rules.enabled[id.index()]),
        }
    }
    fn conclusion(&self, id: RuleId) -> RevisionId {
        match id.index().checked_sub(self.published_rules()) {
            Some(n) => self.new_rules[n].conclusion,
            None => self.rules.conclusion(id),
        }
    }
    fn premises(&self, id: RuleId) -> impl Iterator<Item = Incidence> {
        let staged: Option<&[Incidence]> = id
            .index()
            .checked_sub(self.published_rules())
            .map(|n| self.new_rules[n].premises.as_slice());
        let published = staged.is_none().then(|| self.rules.premises(id));
        staged
            .into_iter()
            .flatten()
            .copied()
            .chain(published.into_iter().flatten())
    }
    fn premise_at(&self, id: RuleId, k: usize) -> Incidence {
        match id.index().checked_sub(self.published_rules()) {
            Some(n) => self.new_rules[n].premises[k],
            None => self.rules.premise_at(id, k),
        }
    }
    fn references(&self, id: RuleId) -> &[RevisionId] {
        match id.index().checked_sub(self.published_rules()) {
            Some(n) => &self.new_rules[n].references,
            None => self.rules.references(id),
        }
    }
    fn validation(&self, id: RuleId, vector: &HeadVector) -> Option<&ValidationRecord> {
        let key = (id, vector.clone());
        self.validations
            .get(&key)
            .or_else(|| self.tables.validations.get(&key))
    }
}
impl PkSource for Stage<'_> {
    fn revisions_of(&self, l: LogicalId) -> Vec<RevisionId> {
        // Rows past the published ones are empty.
        let mut out: Vec<RevisionId> = self
            .index
            .revisions_by_logical
            .row(l.index())
            .iter()
            .copied()
            .collect();
        out.extend(
            self.new_revisions
                .iter()
                .enumerate()
                .filter(|(_, r)| r.logical == l)
                .map(|(n, _)| RevisionId((self.published_revisions() + n) as u32)),
        );
        out
    }
    /// Full staged inputs (error-path diagnostics and tests only; O(N)).
    fn export(&self) -> Inputs {
        let mut i = self.tables.clone();
        (i.logicals, i.metadata) = self.logicals.export();
        i.revisions = self.revisions.export();
        i.rules = self.rules.export();
        i.logicals.extend(self.new_logicals.iter().cloned());
        i.metadata.extend(std::iter::repeat_n(
            Metadata::default(),
            self.new_logicals.len(),
        ));
        i.heads.extend(self.new_heads.iter().copied());
        i.revisions.extend(self.new_revisions.iter().cloned());
        i.base.extend(self.new_base.iter().copied());
        i.rules.extend(self.new_rules.iter().cloned());
        i.artifacts.extend(self.new_artifacts.iter().cloned());
        for (l, h) in &self.heads {
            i.heads[l.index()] = *h;
        }
        for (r, b) in &self.base {
            i.base[r.index()] = *b;
        }
        for (j, e) in &self.enabled {
            i.rules[j.index()].enabled = *e;
        }
        for (a, open) in &self.artifact_open {
            i.artifacts[a.index()].open = *open;
        }
        for (l, m) in &self.metadata {
            i.metadata[l.index()] = m.clone();
        }
        for (k, v) in &self.validations {
            i.validations.insert(k.clone(), v.clone());
        }
        for (k, v) in &self.watermarks {
            i.watermarks.insert(k.clone(), v.clone());
        }
        if let Some(p) = &self.policy {
            i.policy = p.clone();
        }
        i
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;
    fn create(key: &str, content: u8) -> Op {
        Op::CreateLogical {
            key: key.into(),
            kind: RecordKind::Fact,
            scope_lane: "repo".into(),
            content: [content; 32],
            adopt_as_base: true,
        }
    }
    fn revision(logical: LogicalId, content: u8) -> Revision {
        Revision {
            logical,
            content: [content; 32],
            author: PrincipalId::default(),
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(9),
        }
    }
    fn rule(conclusion: RevisionId, references: Vec<RevisionId>, enabled: bool) -> Rule {
        Rule {
            conclusion,
            premises: vec![Incidence {
                premise: RevisionId(0),
                kind: EdgeKind::Pin,
            }],
            references,
            enabled,
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(9),
        }
    }
    /// a: r0 (base), r2; b: r1 (base); c: r3 (base). Rules: 0 concludes r1
    /// (Pin r0, Ref r3), 1 concludes r3 (Cur r1, Sem r2). One open artifact.
    fn published() -> Db {
        let mut db = Db::default();
        db.commit(&Batch::new(vec![create("a", 0), create("b", 1)]))
            .unwrap();
        db.commit(&Batch::new(vec![
            Op::AddRevision {
                logical: LogicalRef::Existing(LogicalId(0)),
                content: [2; 32],
                adopt_as_base: false,
            },
            create("c", 3),
            Op::SetMetadata {
                logical: LogicalRef::Existing(LogicalId(0)),
                tags: ["t".to_owned()].into(),
                dirs: Default::default(),
            },
            Op::OpenArtifact {
                ty: ArtifactType::PendingReview,
                target: RevisionId(0).into(),
                payload_hash: [0; 32],
            },
        ]))
        .unwrap();
        db.commit(&Batch::new(vec![
            Op::AddRule {
                conclusion: RevisionId(1).into(),
                incidences: vec![
                    (RevisionId(0).into(), EdgeKind::Pin),
                    (RevisionId(3).into(), EdgeKind::Ref),
                ],
                enabled: true,
            },
            Op::AddRule {
                conclusion: RevisionId(3).into(),
                incidences: vec![
                    (RevisionId(1).into(), EdgeKind::Cur),
                    (RevisionId(2).into(), EdgeKind::Sem),
                ],
                enabled: true,
            },
        ]))
        .unwrap();
        db
    }
    fn stage(db: &Db) -> Stage<'_> {
        Stage::new(
            &db.tables,
            &db.logicals,
            &db.revisions,
            &db.rules,
            &db.indexes,
        )
    }
    #[test]
    fn incremental_indexes_equal_a_rebuild() {
        let db = published();
        assert_eq!(db.indexes.inc_rule.len(), 3, "Ref is not an incidence");
        assert_eq!(Indexes::build(&db.inputs()), db.indexes);
    }
    #[test]
    fn overlay_reads_published_entries_then_staged_ones() {
        let mut db = published();
        let seen = vec![(LogicalId(0), RevisionId(0))];
        let record = |verdict| ValidationRecord {
            verdict,
            validator: "v".into(),
        };
        db.tables
            .validations
            .insert((RuleId(1), seen.clone()), record(Verdict::Verified));
        db.tables
            .validations
            .insert((RuleId(0), seen.clone()), record(Verdict::Verified));
        let mut s = stage(&db);
        let a = LogicalId(0);
        // Revisions and (logical, content) identity.
        assert_eq!(s.revisions_of(a), vec![RevisionId(0), RevisionId(2)]);
        assert_eq!(s.find_revision(a, &[2; 32]), Some(RevisionId(2)));
        assert_eq!(s.find_revision(a, &[5; 32]), None);
        let r4 = s.push_revision(revision(a, 5), true);
        assert_eq!(r4, RevisionId(4));
        assert_eq!(s.find_revision(a, &[5; 32]), Some(r4));
        assert_eq!(s.revisions_of(a), vec![RevisionId(0), RevisionId(2), r4]);
        let d = s.push_logical(
            Logical {
                key: "d".into(),
                kind: RecordKind::Fact,
                scope_lane: "repo".into(),
                owner: PrincipalId::default(),
                origin_branch: BranchId::default(),
                origin_epoch: Epoch(9),
            },
            RevisionId(5),
        );
        let r5 = s.push_revision(revision(d, 0), false);
        assert_eq!(s.revisions_of(d), vec![r5]);
        assert_eq!(s.find_revision(d, &[0; 32]), Some(r5));
        assert_eq!(s.find_revision(d, &[2; 32]), None);
        // References and validations: staged overrides published.
        assert_eq!(InputView::references(&s, RuleId(0)), &[RevisionId(3)]);
        let j = s.push_rule(rule(r4, vec![RevisionId(1)], true));
        assert_eq!(InputView::references(&s, j), &[RevisionId(1)]);
        s.validations
            .insert((RuleId(1), seen.clone()), record(Verdict::Rejected));
        let verdict = |s: &Stage, j| s.validation(j, &seen).map(|r| r.verdict);
        assert_eq!(verdict(&s, RuleId(1)), Some(Verdict::Rejected));
        assert_eq!(verdict(&s, RuleId(0)), Some(Verdict::Verified));
        assert_eq!(verdict(&s, j), None);
        // Metadata: published, staged override, default for staged logicals.
        assert!(s.metadata_of(a).tags.contains("t"));
        assert_eq!(s.metadata_of(LogicalId(1)), Metadata::default());
        s.metadata.insert(LogicalId(1), db.logicals.metadata(a));
        assert!(s.metadata_of(LogicalId(1)).tags.contains("t"));
        assert_eq!(s.metadata_of(d), Metadata::default());
        // Artifacts: published, then staged after them.
        assert_eq!(
            s.artifact(ArtifactId(0)),
            (ArtifactType::PendingReview, RevisionId(0), true)
        );
        s.new_artifacts.push(Artifact {
            ty: ArtifactType::Conflict,
            target: RevisionId(1),
            payload_hash: [1; 32],
            open: true,
            author: PrincipalId::default(),
            origin_branch: BranchId::default(),
            origin_epoch: Epoch(9),
        });
        assert_eq!(
            s.artifact(ArtifactId(1)),
            (ArtifactType::Conflict, RevisionId(1), true)
        );
        s.set_artifact_open(ArtifactId(0), false);
        assert!(!s.artifact(ArtifactId(0)).2);
    }
    #[test]
    fn a5_candidates_and_staged_rule_support() {
        let db = published();
        let mut s = stage(&db);
        s.set_base(RevisionId(2), true);
        s.set_base(RevisionId(1), false);
        let r4 = s.push_revision(revision(LogicalId(0), 5), true);
        let r5 = s.push_revision(revision(LogicalId(0), 6), false);
        let candidates: Vec<_> = s.base_candidates().collect();
        assert_eq!(candidates, vec![RevisionId(2), r4]);
        assert!(!s.staged_rule_enabled(r5));
        // An enabled rule for another revision does not support r5.
        s.push_rule(rule(r4, Vec::new(), true));
        assert!(!s.staged_rule_enabled(r5));
        let j = s.push_rule(rule(r5, Vec::new(), false));
        assert!(!s.staged_rule_enabled(r5));
        s.set_enabled(j, true);
        assert!(s.staged_rule_enabled(r5));
    }
}
