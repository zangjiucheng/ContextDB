//! `WHAT_IF` and `IMPACT{Exact}` (SPEC SD19, ADR 0013 §5): a commit staged
//! and published under an undo journal, then rolled back. Nothing durable
//! changes: no epoch, no id, no history, no stats. The journal holds the
//! pre-values of what the commit touched, so the cost is O(affected).
use crate::{
    CommitContext, Db, Maintainer, StatsSink, fast, layers::LayerColumns, layers::LayerRows,
    pk::PkMark, view,
};
use contextdb_model::*;
use std::collections::BTreeMap;

/// What a journalled commit must restore beyond the maintainer's own
/// journal.
#[derive(Clone, Debug, Default)]
pub(crate) struct Journal {
    /// Logical, revision, rule and artifact counts.
    pub lens: [usize; 4],
    pub index_mark: (usize, usize),
    pub layer_lens: [usize; 3],
    pub layer_rows: Vec<LayerRows>,
    /// FullRecompute rebuilds every layer column: the whole pre-state.
    pub layers: Option<LayerColumns>,
    /// Set by a structural commit, which leaves the PK journal open.
    pub pk: Option<PkMark>,
    /// Set by `finish`: overwritten heads, base, enabled and validations,
    /// and the change-log record.
    pub undo: Option<fast::Undo>,
    pub record: Option<(Epoch, NetDelta, Vec<String>, usize)>,
    pub policy: Option<PolicyDoc>,
    pub watermarks: BTreeMap<String, Option<Vec<u8>>>,
    pub metadata: BTreeMap<LogicalId, Metadata>,
    pub artifact_open: BTreeMap<ArtifactId, bool>,
}

impl<M: Maintainer, S: StatsSink> Db<M, S> {
    /// Runs `b` as if committed and returns its result, then restores the
    /// database exactly (T14).
    pub fn what_if(&mut self, b: &Batch) -> Result<CommitResult, TxError> {
        self.what_if_in(b, &AllPresent, &CommitContext::default())
    }
    pub fn what_if_in(
        &mut self,
        b: &Batch,
        blobs: &dyn BlobPresence,
        ctx: &CommitContext<'_>,
    ) -> Result<CommitResult, TxError> {
        // A materialized snapshot is set aside rather than patched.
        let derived = std::mem::take(&mut self.derived);
        let (epoch, stats, history) = (self.epoch, self.stats.clone(), self.history.len());
        #[cfg(feature = "timing")]
        let timing = self.timing.clone();
        self.maintainer.begin_journal();
        self.journal = Some(Box::new(Journal {
            lens: [
                self.logicals.len(),
                self.revisions.len(),
                self.rules.len(),
                self.tables.artifacts.len(),
            ],
            index_mark: self.indexes.mark(),
            layer_lens: self.layers.lens(),
            layers: (!M::INCREMENTAL_LAYERS).then(|| self.layers.clone()),
            policy: Some(self.tables.policy.clone()),
            ..Journal::default()
        }));
        let result = self.commit_inner(b, blobs, ctx);
        let journal = *self.journal.take().expect("opened above");
        if result.is_ok() {
            self.roll_back(journal);
        }
        self.maintainer.rollback_journal();
        self.history.truncate(history);
        self.epoch = epoch;
        self.stats = stats;
        self.derived = derived;
        #[cfg(feature = "timing")]
        {
            self.timing = timing;
        }
        result
    }
    fn roll_back(&mut self, j: Journal) {
        let [logicals, revisions, rules, artifacts] = j.lens;
        let (epoch, delta, watermark_moves, old_revisions) = j.record.expect("finished");
        self.changes
            .unrecord(epoch, &delta, &watermark_moves, old_revisions);
        match j.layers {
            Some(layers) => self.layers = layers,
            None => self.layers.restore(j.layer_rows, j.layer_lens),
        }
        if let Some(mark) = j.pk {
            self.pk.rollback(mark);
        }
        // Index rows are popped while the arenas still hold the new rules.
        let view = view::StoredView {
            tables: &self.tables,
            logicals: &self.logicals,
            revisions: &self.revisions,
            rules: &self.rules,
        };
        self.indexes
            .truncate(&view, (logicals, revisions, rules), j.index_mark);
        let undo = j.undo.expect("finished");
        undo.restore(&mut self.tables, &mut self.rules);
        for (key, old) in undo.validations {
            match old {
                Some(record) => self.tables.validations.insert(key, record),
                None => self.tables.validations.remove(&key),
            };
        }
        self.tables.policy = j.policy.expect("saved");
        for (scope, old) in j.watermarks {
            match old {
                Some(position) => self.tables.watermarks.insert(scope, position),
                None => self.tables.watermarks.remove(&scope),
            };
        }
        for (l, m) in j.metadata {
            self.logicals.put_metadata(l, m);
        }
        for (a, open) in j.artifact_open {
            self.tables.artifacts[a.index()].open = open;
        }
        self.tables.heads.truncate(logicals);
        self.tables.base.truncate(revisions);
        self.tables.artifacts.truncate(artifacts);
        self.logicals.truncate(logicals);
        self.revisions.truncate(revisions);
        self.rules.truncate(rules);
    }
}

impl<M: Maintainer, S: StatsSink + PartialEq> Db<M, S> {
    /// The first component in which two databases differ, comparing every
    /// part a commit can touch (T14 and the WHAT_IF property; test aid).
    #[doc(hidden)]
    pub fn state_diff(&self, other: &Self) -> Option<&'static str> {
        let inputs = self.inputs();
        let keys_resolve = (inputs.logicals.iter().enumerate())
            .all(|(n, l)| other.logicals.find_key(&l.key) == Some(LogicalId(n as u32)));
        [
            ("inputs", inputs == other.inputs()),
            ("keys", keys_resolve),
            ("derived", self.derived() == other.derived()),
            (
                "counting",
                self.maintainer.state() == other.maintainer.state(),
            ),
            ("layers", self.layers == other.layers),
            ("indexes", self.indexes == other.indexes),
            ("pk", self.pk == other.pk),
            ("history", self.history == other.history),
            ("changes", self.changes == other.changes),
            (
                "epoch",
                (self.epoch, self.initial_epoch) == (other.epoch, other.initial_epoch),
            ),
            ("stats", self.stats == other.stats),
            ("journal", self.journal.is_none() && other.journal.is_none()),
        ]
        .into_iter()
        .find(|(_, same)| !same)
        .map(|(part, _)| part)
    }
}
