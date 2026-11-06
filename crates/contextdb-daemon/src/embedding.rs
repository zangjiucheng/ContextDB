//! Embeddings (ADR 0016 §7): the vector projection, the vector ranker's
//! lookups, and embedding contradiction candidates.
//!
//! One background thread, never the writer and never the recall indexer,
//! so a slow, hung or failing embedder can delay only vectors (T89):
//!
//! 1. **Fill.** It lists every live head whose content has no vector for
//!    the model, drops the snapshot, and embeds them in batches of
//!    `max_batch`. A failed batch sets the state `Failed` and is retried
//!    with back-off (1 s doubling to 60 s). The state is `Ready` at an
//!    epoch once every live head of that snapshot has a vector.
//! 2. **Candidates.** For each head of kind Decision, Plan or Fact that an
//!    epoch since the embedding watermark moved, and that is still the head
//!    on its branch, it compares its vector with every other such head
//!    visible on the branch in the same scope lane. It drops its own
//!    logical, its direct premises and dependents, and cosines under
//!    `min_cosine`, and keeps at most `per_head` pairs, as `source =
//!    embedding` (or `both` with a lexical pair). The rows touching those
//!    heads are swept, as the lexical pass does.
//!
//! Vectors are keyed by content, so branches share them; every scan covers
//! only heads visible on the caller's branch (SD19).
use crate::actor::{ActorHandle, Snapshot};
use crate::candidates::{self, Candidates, KINDS};
use crate::index_health::IndexHealth;
use contextdb_model::*;
use contextdb_proto::proto::IndexState;
use contextdb_store::Durability;
use contextdb_store::vectors::VectorStore;
use contextdb_validate::ValidatorError;
use contextdb_validate::embed::{self, Embedder, VectorLimits};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a query's vector is reused (ADR 0016 §7.2).
pub const QUERY_TTL: Duration = Duration::from_secs(600);
/// Most cached query vectors.
const QUERY_CACHE: usize = 256;
/// Longest a recall waits for its query vector.
pub const QUERY_DEADLINE: Duration = Duration::from_secs(2);
/// How long a failed query embedding is not retried, so a broken embedder
/// costs one deadline per window, not one per recall.
const QUERY_FAILURE_HOLD: Duration = Duration::from_secs(30);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// What the daemon runs the embedder with.
pub struct Settings {
    pub embedder: Arc<dyn Embedder>,
    pub max_batch: usize,
    pub timeout: Duration,
    pub limits: VectorLimits,
}
impl Settings {
    pub fn from_config(c: &embed::Config) -> Self {
        Self {
            embedder: Arc::from(c.embedder()),
            max_batch: c.max_batch,
            timeout: c.timeout,
            limits: c.limits,
        }
    }
}

/// Counters for reports (G5, G6).
#[derive(Debug, Default)]
pub struct VectorStats {
    /// Records embedded, and batches failed.
    pub embedded: AtomicU64,
    pub failures: AtomicU64,
    /// Embedding pairs inserted.
    pub pairs: AtomicU64,
    /// Vector lookups served from the cache, and from SQLite.
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
}

/// When a query was embedded, and its vector (`None`: the call failed).
type QueryEntry = (Instant, Option<Arc<[f32]>>);

#[derive(Default)]
struct Cache {
    tick: u64,
    map: HashMap<ContentHash, (u64, Arc<[f32]>)>,
}

pub struct Vectors {
    pub settings: Settings,
    model: String,
    dims: usize,
    store: Mutex<VectorStore>,
    cache: Mutex<Cache>,
    /// Most vectors the cache holds (`max_cache_mb`, E2).
    cache_cap: usize,
    queries: Mutex<HashMap<String, QueryEntry>>,
    pub health: IndexHealth,
    pub stats: VectorStats,
}
impl Vectors {
    pub fn open(settings: Settings, path: Option<&Path>) -> Result<Self, String> {
        let store = VectorStore::open(path)
            .or_else(|_| VectorStore::open(None))
            .map_err(|e| format!("{}: {e}", contextdb_store::vectors::FILE))?;
        let model = settings.embedder.model().to_owned();
        let dims = settings.embedder.dims();
        let cache_cap = (settings.limits.max_cache_mb << 20) / (4 * dims.max(1));
        Ok(Self {
            settings,
            model,
            dims,
            store: Mutex::new(store),
            cache: Mutex::new(Cache::default()),
            cache_cap,
            queries: Mutex::new(HashMap::new()),
            health: IndexHealth::new(Epoch(0)),
            stats: VectorStats::default(),
        })
    }
    pub fn model(&self) -> &str {
        &self.model
    }
    /// G4 fault injection: caps `vectors.sqlite` at its current size plus
    /// `extra` pages (`None` lifts the cap).
    #[doc(hidden)]
    pub fn limit_store(&self, extra: Option<u64>) {
        let s = self.store.lock().expect("vectors");
        let pages = extra.map(|e| s.page_count().unwrap_or(0) + e);
        let _ = s.limit_pages(pages);
    }
    /// Vectors stored for the model.
    pub fn stored(&self) -> u64 {
        self.store
            .lock()
            .expect("vectors")
            .count(&self.model)
            .unwrap_or(0)
    }
    /// The state relative to a snapshot at `head`.
    pub fn state(&self, head: Epoch) -> IndexState {
        self.health.state(head)
    }
    /// The unit vector of `content`, from the cache or else from SQLite
    /// (the uncached path, E2). A vector read from SQLite is cached while
    /// there is room, evicting the least recently touched.
    pub fn vector(&self, content: &ContentHash) -> Option<Arc<[f32]>> {
        {
            let mut c = self.cache.lock().expect("vector cache");
            c.tick += 1;
            let tick = c.tick;
            if let Some(e) = c.map.get_mut(content) {
                e.0 = tick;
                self.stats.cache_hits.fetch_add(1, Ordering::Relaxed);
                return Some(e.1.clone());
            }
        }
        self.stats.cache_misses.fetch_add(1, Ordering::Relaxed);
        let v: Arc<[f32]> = (self.store.lock().expect("vectors"))
            .get(&self.model, self.dims, content)
            .ok()
            .flatten()?
            .into();
        self.remember(*content, v.clone());
        Some(v)
    }
    fn remember(&self, content: ContentHash, v: Arc<[f32]>) {
        if self.cache_cap == 0 {
            return;
        }
        let mut c = self.cache.lock().expect("vector cache");
        if c.map.len() >= self.cache_cap && !c.map.contains_key(&content) {
            let oldest = (c.map.iter()).min_by_key(|(_, (t, _))| *t).map(|(k, _)| *k);
            if let Some(k) = oldest {
                c.map.remove(&k);
            }
        }
        c.tick += 1;
        let tick = c.tick;
        c.map.insert(content, (tick, v));
    }
    fn has(&self, content: &ContentHash) -> bool {
        self.cache
            .lock()
            .expect("vector cache")
            .map
            .contains_key(content)
            || (self.store.lock().expect("vectors"))
                .has(&self.model, self.dims, content)
                .unwrap_or(false)
    }
    /// The query's vector: one embedder call, cached by query text for
    /// [`QUERY_TTL`]. `None` when the embedder fails (the recall then runs
    /// without the vector ranker; a failure is held for 30 s).
    pub fn query_vector(&self, query: &str) -> Option<Arc<[f32]>> {
        let now = Instant::now();
        if let Some((at, v)) = self.queries.lock().expect("queries").get(query) {
            let ttl = if v.is_some() {
                QUERY_TTL
            } else {
                QUERY_FAILURE_HOLD
            };
            if now.duration_since(*at) < ttl {
                return v.clone();
            }
        }
        let deadline = now + self.settings.timeout.min(QUERY_DEADLINE);
        let v: Option<Arc<[f32]>> = (self.settings.embedder)
            .embed(&[query.to_owned()], deadline)
            .ok()
            .and_then(|mut v| v.pop())
            .filter(|v| v.len() == self.dims)
            .map(Into::into);
        let mut q = self.queries.lock().expect("queries");
        if q.len() >= QUERY_CACHE {
            q.retain(|_, (at, _)| now.duration_since(*at) < QUERY_TTL);
            if q.len() >= QUERY_CACHE {
                q.clear();
            }
        }
        q.insert(query.to_owned(), (now, v.clone()));
        v
    }
}

/// The texts of every live head whose content has no vector yet, one per
/// content hash.
fn missing<D: Durability>(snap: &Snapshot<D>, v: &Vectors) -> Vec<(ContentHash, String)> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (b, l) in candidates::all_heads(snap) {
        let Some(view) = snap.kernel.branch(b) else {
            continue;
        };
        let Some(r) = view.head(l) else { continue };
        let Ok(content) = view.revision_content(r) else {
            continue;
        };
        if !seen.insert(content) || v.has(&content) {
            continue;
        }
        if let Some(t) = candidates::text(snap, &view, l, r) {
            out.push((
                content,
                embed::record_text(&t.abstract_.text, &t.outline.text, &t.body),
            ));
        }
    }
    out
}

/// Runs one embedder call on its own thread, so a stop request is never
/// held up by a slow embedder (the call ends by itself at its deadline).
fn call(
    e: &Arc<dyn Embedder>,
    texts: Vec<String>,
    deadline: Instant,
    stop: &AtomicBool,
) -> Option<Result<Vec<Vec<f32>>, ValidatorError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let e = e.clone();
    std::thread::Builder::new()
        .name("ctxd-embed-call".into())
        .spawn(move || {
            let _ = tx.send(e.embed(&texts, deadline));
        })
        .ok()?;
    loop {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return Some(r),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return Some(Err(ValidatorError::Failed("embedder call lost".into()))),
        }
    }
}

/// Step 1: embeds what is missing. `Ok(true)` when something was embedded,
/// `Err` on a failed batch, `None` when stopped.
fn fill(
    v: &Vectors,
    todo: Vec<(ContentHash, String)>,
    stop: &AtomicBool,
) -> Option<Result<bool, String>> {
    let s = &v.settings;
    let mut any = false;
    for chunk in todo.chunks(s.max_batch.max(1)) {
        let texts: Vec<String> = chunk.iter().map(|(_, t)| t.clone()).collect();
        let deadline = Instant::now() + s.timeout;
        let out = match call(&s.embedder, texts, deadline, stop)? {
            Ok(out) if out.len() == chunk.len() && out.iter().all(|x| x.len() == v.dims) => out,
            Ok(_) => return Some(Err("embedder: wrong number or size of vectors".into())),
            Err(e) => return Some(Err(format!("embedder: {e}"))),
        };
        let rows: Vec<(ContentHash, Vec<f32>)> = chunk.iter().map(|(c, _)| *c).zip(out).collect();
        if let Err(e) = v.store.lock().expect("vectors").put(&v.model, &rows) {
            return Some(Err(format!("{}: {e}", contextdb_store::vectors::FILE)));
        }
        v.stats
            .embedded
            .fetch_add(rows.len() as u64, Ordering::Relaxed);
        for (c, x) in rows {
            v.remember(c, x.into());
        }
        any = true;
        crate::p8_crash_point("embed-batch");
    }
    Some(Ok(any))
}

/// The pairs of one head (step 2); returns the number inserted.
fn pairs_for<D: Durability>(
    snap: &Snapshot<D>,
    v: &Vectors,
    table: &Candidates,
    branch: BranchId,
    l: LogicalId,
) -> usize {
    let limits = v.settings.limits;
    let Some(view) = snap.kernel.branch(branch) else {
        return 0;
    };
    if view.info().archived || limits.per_head == 0 {
        return 0;
    }
    let Some(rev) = view.head(l) else { return 0 };
    let Some((_, kind, scope)) = view.logical_info(l) else {
        return 0;
    };
    if !KINDS.contains(&kind) {
        return 0;
    }
    let Some(mine) = view.revision_content(rev).ok().and_then(|c| v.vector(&c)) else {
        return 0;
    };
    let mut skip: BTreeSet<LogicalId> = view.neighbour_logicals(l).into_iter().collect();
    skip.insert(l);
    let mut near: Vec<(f32, LogicalId, RevisionId)> = Vec::new();
    for other in view.logicals() {
        if skip.contains(&other) {
            continue;
        }
        let Some(r) = view.head(other) else { continue };
        if !(view.logical_info(other)).is_some_and(|(_, k, s)| KINDS.contains(&k) && s == scope) {
            continue;
        }
        let Some(x) = view.revision_content(r).ok().and_then(|c| v.vector(&c)) else {
            continue;
        };
        let cos = embed::dot(&mine, &x);
        if cos >= limits.min_cosine {
            near.push((cos, other, r));
        }
    }
    near.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut inserted = 0;
    for (cos, other, r) in near.into_iter().take(limits.per_head) {
        if table
            .insert_embedding(branch, (l, rev), (other, r), cos as f64, snap.epoch)
            .unwrap_or(false)
        {
            inserted += 1;
        }
    }
    v.stats.pairs.fetch_add(inserted as u64, Ordering::Relaxed);
    inserted
}

/// Step 2 for the epochs since the embedding watermark; every live head of
/// `snap` must have its vector. Returns the pairs inserted, or `None` when
/// stopped part way (the watermark stays; pairs are unique).
pub fn generate<D: Durability>(
    snap: &Snapshot<D>,
    v: &Vectors,
    table: &Candidates,
    stop: &AtomicBool,
) -> Option<usize> {
    let from = table.embed_watermark().ok()?;
    let heads: BTreeSet<(BranchId, LogicalId)> = match from {
        Some(e) if e == snap.epoch => return Some(0),
        Some(e) if e < snap.epoch => candidates::moved_heads(snap, e.0 + 1, snap.epoch.0).0,
        _ => candidates::all_heads(snap),
    };
    let mut inserted = 0;
    for (b, l) in &heads {
        if stop.load(Ordering::Acquire) {
            return None;
        }
        inserted += pairs_for(snap, v, table, *b, *l);
    }
    for (b, l) in &heads {
        candidates::sweep_rows(snap, table, table.touching(*b, *l).unwrap_or_default());
    }
    let _ = table.set_embed_watermark(snap.epoch);
    Some(inserted)
}

/// Sleeps up to `d`, waking early on `stop`.
fn nap(d: Duration, stop: &AtomicBool) {
    let until = Instant::now() + d;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= until {
            return;
        }
        std::thread::sleep((until - now).min(Duration::from_millis(50)));
    }
}

/// The embedding thread.
pub(crate) fn run<D: Durability>(
    actor: ActorHandle<D>,
    v: Arc<Vectors>,
    table: Arc<Candidates>,
    stop: Arc<AtomicBool>,
) {
    let mut backoff = BACKOFF_MIN;
    let mut filled: Option<Epoch> = None;
    while !stop.load(Ordering::Acquire) {
        if v.health.take_reindex() {
            // `ctx reindex --vectors`: drop other models' vectors and the
            // cache, then re-embed only what is missing (ADR 0016 §2.3).
            let _ = (v.store.lock().expect("vectors")).prune_except(&v.model, v.dims);
            v.cache.lock().expect("vector cache").map.clear();
            filled = None;
        }
        let snap = actor.snapshot();
        let epoch = snap.epoch;
        if filled == Some(epoch) {
            drop(snap);
            nap(Duration::from_millis(20), &stop);
            continue;
        }
        let todo = missing(&snap, &v);
        if todo.is_empty() {
            v.health.applied(epoch);
            backoff = BACKOFF_MIN;
            if generate(&snap, &v, &table, &stop).is_some() {
                filled = Some(epoch);
            }
            continue;
        }
        if filled.is_none() {
            let total = todo.len() as u64;
            v.health.rebuilding(0, total);
        }
        // Never hold a snapshot across embedder calls (ADR 0008 §3.3).
        drop(snap);
        match fill(&v, todo, &stop) {
            None => return,
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                v.stats.failures.fetch_add(1, Ordering::Relaxed);
                v.health.failed(e);
                nap(backoff, &stop);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Fused-ranking input from the vector ranker (ADR 0016 §3.2): the heads of
/// `view` with a vector, by descending cosine to `q`, ties by logical id.
/// Heads without a vector yet are skipped (the projection lags; status
/// never comes from here).
pub fn ranked(
    v: &Vectors,
    view: &contextdb_core::BranchView<'_>,
    q: &[f32],
) -> Vec<(LogicalId, RevisionId)> {
    let mut scored: Vec<(f32, LogicalId, RevisionId)> = view
        .logicals()
        .filter_map(|l| {
            let r = view.head(l)?;
            let x = v.vector(&view.revision_content(r).ok()?)?;
            Some((embed::dot(q, &x), l, r))
        })
        .filter(|(c, ..)| *c > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, l, r)| (l, r)).collect()
}
