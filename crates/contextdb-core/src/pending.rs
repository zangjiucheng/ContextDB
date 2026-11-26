//! Pending lists of a maintenance pass (ADR 0019 §6, A3): per revision, the
//! incidences or rules to re-evaluate when it pops, in push order.
//!
//! Every list is a circular singly linked list in one node arena, so a push
//! or a take is O(1). A revision's tail slot holds 1 + the last node of its
//! list, whose `next` is the first node. A take detaches the list and hands
//! back a [`Cursor`] over it; a taken list's nodes are never written again,
//! so the cursor stays valid across frontier slices and clones.
//!
//! The slots live in pages of `PAGE` revisions allocated on a page's first
//! push, so a pass zeroes index for the revisions it touches, not for every
//! revision. [`Pending::reset`] keeps a bounded cache for the next pass: the
//! directory (one u32 per `PAGE` revisions), up to `KEEP_PAGES` pages of
//! capacity and up to `KEEP_NODES` nodes of capacity. A small pass then
//! allocates nothing, and a large one does not keep its high-water memory.
//! A lazy frontier is a pass that has not ended.
use contextdb_model::RevisionId;

const NONE: u32 = u32::MAX;
const PAGE: usize = 1 << 10;
/// Page capacity kept across passes: 8 × 4,100 B.
const KEEP_PAGES: usize = 8;
/// Node capacity kept across passes: 4,096 × 8 B for u32 ids.
const KEEP_NODES: usize = 1 << 12;

#[derive(Clone, Debug, Default)]
pub(crate) struct Pending<T> {
    /// Per run of `PAGE` revisions: 1 + its page in `pages`, or 0.
    dir: Vec<u32>,
    /// `(dir index, per revision: 1 + the last node of its list, or 0)`.
    pages: Vec<(u32, [u32; PAGE])>,
    /// `(item, next)`.
    nodes: Vec<(T, u32)>,
}
/// The rest of a taken list.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursor(u32);
impl Cursor {
    pub(crate) fn is_done(self) -> bool {
        self.0 == NONE
    }
}

impl<T: Copy> Pending<T> {
    fn slot(&self, r: usize) -> u32 {
        match self.dir.get(r / PAGE) {
            Some(&p) if p != 0 => self.pages[p as usize - 1].1[r % PAGE],
            _ => 0,
        }
    }
    pub(crate) fn push(&mut self, r: RevisionId, item: T) {
        let n = u32::try_from(self.nodes.len()).expect("pending nodes fit u32");
        let r = r.index();
        if r / PAGE >= self.dir.len() {
            self.dir.resize(r / PAGE + 1, 0);
        }
        let page = &mut self.dir[r / PAGE];
        if *page == 0 {
            self.pages.push(((r / PAGE) as u32, [0; PAGE]));
            *page = u32::try_from(self.pages.len()).expect("pending pages fit u32");
        }
        let next = match std::mem::replace(&mut self.pages[*page as usize - 1].1[r % PAGE], n + 1) {
            0 => n,
            last => std::mem::replace(&mut self.nodes[last as usize - 1].1, n),
        };
        self.nodes.push((item, next));
    }
    /// Detaches `r`'s list; later pushes for `r` start a new one.
    pub(crate) fn take(&mut self, r: RevisionId) -> Cursor {
        let r = r.index();
        match self.slot(r) {
            0 => Cursor(NONE),
            last => {
                let page = self.dir[r / PAGE] as usize - 1;
                self.pages[page].1[r % PAGE] = 0;
                Cursor(std::mem::replace(
                    &mut self.nodes[last as usize - 1].1,
                    NONE,
                ))
            }
        }
    }
    pub(crate) fn next(&self, at: &mut Cursor) -> Option<T> {
        let (item, next) = *self.nodes.get(at.0 as usize)?;
        at.0 = next;
        Some(item)
    }
    /// `r`'s list, without taking it.
    fn list(&self, r: usize) -> impl Iterator<Item = T> + '_ {
        let last = self.slot(r).wrapping_sub(1);
        let mut at = if last == NONE {
            NONE
        } else {
            self.nodes[last as usize].1
        };
        std::iter::from_fn(move || {
            let (item, next) = *self.nodes.get(at as usize)?;
            at = if at == last { NONE } else { next };
            Some(item)
        })
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.pages
            .iter()
            .all(|(_, tails)| tails.iter().all(|t| *t == 0))
    }
    /// Ends a pass: every list has been taken and no cursor is live. Empties
    /// the arena and its index, keeping the bounded cache.
    pub(crate) fn reset(&mut self) {
        crate::invariant_assert!(self.is_empty());
        for (d, _) in &self.pages {
            self.dir[*d as usize] = 0;
        }
        self.pages.clear();
        self.pages.shrink_to(KEEP_PAGES);
        self.nodes.clear();
        self.nodes.shrink_to(KEEP_NODES);
    }
}
/// Equal when every revision's list holds the same items in the same order.
impl<T: Copy + PartialEq> PartialEq for Pending<T> {
    fn eq(&self, other: &Self) -> bool {
        // A revision with a list on either side lies in a page of that side.
        self.pages.iter().chain(&other.pages).all(|(d, _)| {
            let first = *d as usize * PAGE;
            (first..first + PAGE).all(|r| self.list(r).eq(other.list(r)))
        })
    }
}
impl<T: Copy + Eq> Eq for Pending<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_keep_push_order_and_survive_interleaving() {
        let mut p = Pending::default();
        for (r, x) in [(3, 1), (1, 2), (3, 3), (1, 4), (3, 5)] {
            p.push(RevisionId(r), x);
        }
        let take = |p: &mut Pending<u32>, r| {
            let mut c = p.take(RevisionId(r));
            std::iter::from_fn(|| p.next(&mut c)).collect::<Vec<_>>()
        };
        let mut c = p.take(RevisionId(3));
        assert_eq!(p.next(&mut c), Some(1));
        // A push for a taken revision starts a new list; the cursor goes on.
        p.push(RevisionId(3), 6);
        // A clone mid-pass keeps the cursor valid.
        let mut clone = p.clone();
        let mut d = c;
        assert_eq!(
            std::iter::from_fn(|| p.next(&mut c)).collect::<Vec<_>>(),
            [3, 5]
        );
        assert_eq!(
            std::iter::from_fn(|| clone.next(&mut d)).collect::<Vec<_>>(),
            [3, 5]
        );
        let mut q = Pending::default();
        q.push(RevisionId(3), 6);
        q.push(RevisionId(1), 2);
        q.push(RevisionId(1), 4);
        assert_eq!(p, q);
        assert_eq!(take(&mut clone, 1), [2, 4]);
        assert!(take(&mut clone, 0).is_empty() && take(&mut clone, 9).is_empty());
        // A revision on a page no push touched has no list and gets no page.
        let pages = clone.pages.len();
        assert!(take(&mut clone, 5 * PAGE as u32).is_empty() && clone.pages.len() == pages);
        assert!(clone != q && !clone.is_empty());
        assert_eq!(take(&mut clone, 3), [6]);
        clone.reset();
        assert!(clone.is_empty() && clone == Pending::default());
        assert!(clone.pages.is_empty() && clone.nodes.is_empty());
    }
    #[test]
    fn pages_cover_only_touched_revisions() {
        let mut p = Pending::default();
        let far = RevisionId(3 * PAGE as u32 + 7);
        p.push(far, 1u32);
        p.push(RevisionId(1), 2);
        p.push(far, 3);
        assert_eq!((p.dir.len(), p.pages.len()), (4, 2));
        let mut c = p.take(far);
        assert_eq!(
            std::iter::from_fn(|| p.next(&mut c)).collect::<Vec<_>>(),
            [1, 3]
        );
        let mut c = p.take(RevisionId(1));
        assert_eq!(p.next(&mut c), Some(2));
        p.reset();
        assert!(p.pages.is_empty() && p.dir.iter().all(|d| *d == 0));
    }
    #[test]
    fn reset_keeps_a_bounded_cache() {
        let drain = |p: &mut Pending<u32>, revisions: &[u32]| {
            for &r in revisions {
                let mut c = p.take(RevisionId(r));
                while p.next(&mut c).is_some() {}
            }
            p.reset();
        };
        // Like a small commit near the tail: two pages, 32 nodes.
        let tail = 50 * PAGE as u32;
        let near_tail: Vec<u32> = (tail - 7..=tail).collect();
        let heap = |p: &Pending<u32>| {
            [
                (p.dir.as_ptr() as usize, p.dir.capacity()),
                (p.pages.as_ptr() as usize, p.pages.capacity()),
                (p.nodes.as_ptr() as usize, p.nodes.capacity()),
            ]
        };
        // Asserts that the pass, pushes included, reuses the heap it starts with.
        let small = |p: &mut Pending<u32>| {
            let before = heap(p);
            for x in 0..4 {
                for &r in &near_tail {
                    p.push(RevisionId(r), x);
                }
            }
            assert_eq!(p.nodes.len(), 32);
            assert_eq!(heap(p), before, "a small pass after a reset allocates");
            drain(p, &near_tail);
            assert_eq!(heap(p), before);
        };
        let mut p = Pending::default();
        for x in 0..4 {
            for &r in &near_tail {
                p.push(RevisionId(r), x);
            }
        }
        drain(&mut p, &near_tail);
        small(&mut p);
        // A large pass: 51 pages, 101,000 nodes.
        let spread: Vec<u32> = (0..=tail).step_by(PAGE / 2).collect();
        for x in 0..1000 {
            for &r in &spread {
                p.push(RevisionId(r), x);
            }
        }
        assert_eq!((p.pages.len(), p.nodes.len()), (51, 101_000));
        drain(&mut p, &spread);
        assert!(p.pages.capacity() <= KEEP_PAGES && p.nodes.capacity() <= KEEP_NODES);
        assert_eq!(p.dir.len(), 51);
        // Besides the directory (4 B per `PAGE` revisions), the cache is
        // at most 64 KiB.
        let cache = p.pages.capacity() * std::mem::size_of::<(u32, [u32; PAGE])>()
            + p.nodes.capacity() * std::mem::size_of::<(u32, u32)>();
        assert!(cache <= 65_568, "{cache}");
        assert_eq!(p, Pending::default());
        small(&mut p);
    }
}
