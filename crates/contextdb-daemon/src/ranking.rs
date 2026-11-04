//! Hybrid ranking (ADR 0016 §3.2). Rankers hand in **eligible** candidates
//! only, in their own order; this module fuses them. Eligibility (head,
//! scope, certification) is decided by the caller before any ranker cuts,
//! so fusion never sees a candidate that cannot be shown.
use contextdb_model::LogicalId;
use std::collections::BTreeMap;

/// Reciprocal rank fusion constant.
pub const RRF_K: f64 = 60.0;

/// How a key matched the query: exact, prefix, then substring.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyRank {
    Exact = 0,
    Prefix = 1,
    Substring = 2,
}

impl KeyRank {
    pub fn of(key: &str, query: &str) -> Option<KeyRank> {
        if key == query {
            Some(KeyRank::Exact)
        } else if key.starts_with(query) {
            Some(KeyRank::Prefix)
        } else if key.contains(query) {
            Some(KeyRank::Substring)
        } else {
            None
        }
    }
}

/// Fuses ranked lists with `score = Σ 1/(60 + rank)` (ranks from 1).
///
/// `key` is the key ranker's list. The exact key match, if any, comes
/// first overall. Ties break by key rank (no key match sorts last), then
/// by logical id, so the order is fully determined by the inputs (T85).
/// Each list must hold a logical at most once.
pub fn fuse(key: &[(KeyRank, LogicalId)], others: &[&[LogicalId]]) -> Vec<LogicalId> {
    let mut score: BTreeMap<LogicalId, (f64, Option<KeyRank>)> = BTreeMap::new();
    for (n, (rank, l)) in key.iter().enumerate() {
        let e = score.entry(*l).or_insert((0.0, None));
        e.0 += 1.0 / (RRF_K + (n + 1) as f64);
        e.1 = Some(*rank);
    }
    for list in others {
        for (n, l) in list.iter().enumerate() {
            score.entry(*l).or_insert((0.0, None)).0 += 1.0 / (RRF_K + (n + 1) as f64);
        }
    }
    let mut out: Vec<(LogicalId, f64, Option<KeyRank>)> =
        score.into_iter().map(|(l, (s, k))| (l, s, k)).collect();
    // Sums are accumulated in a fixed order (key list, then each list in
    // order), so equal inputs give bit-equal scores.
    out.sort_by(|a, b| {
        // No key match ranks after every key match.
        let k = |k: Option<KeyRank>| k.map_or(3, |k| k as u8);
        let exact = |r: Option<KeyRank>| r != Some(KeyRank::Exact);
        exact(a.2)
            .cmp(&exact(b.2))
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| k(a.2).cmp(&k(b.2)))
            .then_with(|| a.0.cmp(&b.0))
    });
    out.into_iter().map(|(l, ..)| l).collect()
}

/// Whether the path prefix `dir` covers `path` (whole segments only:
/// `src` covers `src/a.rs` and `src`, not `srcx/a.rs`). An empty prefix
/// covers everything.
pub fn covers(dir: &str, path: &str) -> bool {
    let d = dir.strip_prefix("./").unwrap_or(dir).trim_end_matches('/');
    let p = path.strip_prefix("./").unwrap_or(path);
    d.is_empty()
        || p == d
        || (p.len() > d.len() && p.starts_with(d) && p.as_bytes()[d.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;
    fn l(n: u32) -> LogicalId {
        LogicalId(n)
    }

    #[test]
    fn exact_key_first_then_fused_score_then_key_rank_then_id() {
        let key = [(KeyRank::Prefix, l(5)), (KeyRank::Exact, l(9))];
        let bm25 = [l(5), l(1), l(2)];
        let out = fuse(&key, &[&bm25]);
        // 9 is exact: first although 5 scores more.
        assert_eq!(out[0], l(9));
        assert_eq!(out[1], l(5));
        // 1 and 2 only in bm25, at ranks 2 and 3.
        assert_eq!(&out[2..], &[l(1), l(2)]);
    }

    #[test]
    fn ties_break_by_key_rank_then_logical() {
        // 3 is a key match at rank 1 of keys; 4 is at rank 1 of bm25: equal
        // scores, 3 wins on key rank.
        let out = fuse(&[(KeyRank::Substring, l(3))], &[&[l(4)]]);
        assert_eq!(out, vec![l(3), l(4)]);
        // No key matches: equal scores break by id.
        let out = fuse(&[], &[&[l(8)], &[l(2)]]);
        assert_eq!(out, vec![l(2), l(8)]);
    }

    #[test]
    fn fusion_is_deterministic_and_order_free_in_ties() {
        let key = [(KeyRank::Prefix, l(1)), (KeyRank::Substring, l(2))];
        let a = [l(2), l(3), l(4)];
        let b = [l(4), l(1)];
        assert_eq!(fuse(&key, &[&a, &b]), fuse(&key, &[&a, &b]));
        let out = fuse(&key, &[&a, &b]);
        let mut sorted = out.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), out.len(), "each logical once");
    }

    #[test]
    fn dir_covers_whole_segments() {
        assert!(covers("src", "src/a.rs"));
        assert!(covers("src/", "src/a.rs"));
        assert!(covers("./src", "src/x/y.rs"));
        assert!(covers("src/a.rs", "src/a.rs"));
        assert!(!covers("src", "srcx/a.rs"));
        assert!(!covers("src/a", "src/ab.rs"));
        assert!(covers("", "anything"));
    }
}
