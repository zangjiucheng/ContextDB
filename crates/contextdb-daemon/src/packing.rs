//! Packing (ADR 0016 §5): fit fused, eligible items into a token budget,
//! greedily in order. Pure: the caller supplies each item's renderings.
//!
//! - **Header first.** An included item's header is charged before any of
//!   its text and is never cut.
//! - **Level fitting.** Each item gets the highest level, up to the one
//!   wanted, whose text fits what is left: `full`, `outline`, `abstract`,
//!   then the header alone.
//! - **Stop.** Once a header no longer fits, packing stops; the rest are
//!   `omitted` (no handle, no text). An item is never shown without its
//!   label, so the included items are always a prefix of the input order.
use crate::proto::Rendering;
use contextdb_model::levels::{Level, Rendered};

/// Named in every budgeted response, so callers can apply their own margin.
pub const ESTIMATE: &str = "bytes/4 v1";
/// Fixed per-item header cost, on top of the key's bytes/4.
pub const HEADER_TOKENS: u32 = 16;

/// `ceil(bytes / 4)`.
pub fn tokens(s: &str) -> u32 {
    u32::try_from(s.len().div_ceil(4)).unwrap_or(u32::MAX)
}

/// A candidate's renderings below `header`.
pub struct Texts {
    pub key: String,
    pub abstract_: Rendered,
    pub outline: Rendered,
    pub full: Rendered,
}

impl Texts {
    fn at(&self, level: Level) -> Rendering {
        let r = match level {
            Level::Header => {
                return Rendering {
                    level,
                    // Below `full` only the abstract may be loaded; it is
                    // empty only for an empty body.
                    cut: !self.abstract_.text.is_empty(),
                    text: String::new(),
                };
            }
            Level::Abstract => &self.abstract_,
            Level::Outline => &self.outline,
            Level::Full => &self.full,
        };
        Rendering {
            level,
            cut: r.cut,
            text: r.text.clone(),
        }
    }
}

/// The renderings of the included prefix and the number omitted. Without a
/// budget every item is rendered at `want`.
pub fn pack(items: &[Texts], want: Level, budget: Option<u32>) -> (Vec<Rendering>, usize) {
    let Some(budget) = budget else {
        return (items.iter().map(|t| t.at(want)).collect(), 0);
    };
    let mut left = budget;
    let mut out = Vec::new();
    for t in items {
        let header = HEADER_TOKENS.saturating_add(tokens(&t.key));
        if header > left {
            break;
        }
        left -= header;
        let level = [Level::Full, Level::Outline, Level::Abstract]
            .into_iter()
            .filter(|l| *l <= want)
            .find(|l| tokens(&t.at(*l).text) <= left)
            .unwrap_or(Level::Header);
        let r = t.at(level);
        left -= tokens(&r.text);
        out.push(r);
    }
    let omitted = items.len() - out.len();
    (out, omitted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use contextdb_model::levels;

    fn texts(key: &str, body: &str) -> Texts {
        Texts {
            key: key.into(),
            abstract_: levels::abstract_of(body.as_bytes()),
            outline: levels::outline_of(body.as_bytes()),
            full: levels::render(body.as_bytes(), Level::Full),
        }
    }

    #[test]
    fn tokens_are_bytes_over_four_rounded_up() {
        assert_eq!(tokens(""), 0);
        assert_eq!(tokens("abcd"), 1);
        assert_eq!(tokens("abcde"), 2);
    }

    #[test]
    fn no_budget_renders_every_item_at_the_wanted_level() {
        let items = [texts("a", "# A\n\none\n"), texts("b", "two")];
        let (r, omitted) = pack(&items, Level::Abstract, None);
        assert_eq!(omitted, 0);
        assert_eq!(r[0].text, "one");
        assert_eq!(r[1].level, Level::Abstract);
    }

    #[test]
    fn levels_step_down_then_headers_then_stop() {
        let long = format!("# T\n\n{}\n\n## S\nfirst\n", "word ".repeat(200));
        let items: Vec<Texts> = (0..5).map(|n| texts(&format!("k{n}"), &long)).collect();
        let full = tokens(&items[0].full.text);
        let header = HEADER_TOKENS + tokens("k0");
        // Room for one full item: the first is full, the rest step down.
        let (r, omitted) = pack(&items, Level::Full, Some(header + full + 3 * header));
        assert_eq!(r[0].level, Level::Full);
        assert!(r.len() == 4 && omitted == 1, "{} {omitted}", r.len());
        assert!(r[1..].iter().all(|x| x.level == Level::Header && x.cut));
        // The included items are a prefix, and the budget holds.
        for budget in 0..(full + 6 * header) {
            let (r, omitted) = pack(&items, Level::Full, Some(budget));
            assert_eq!(r.len() + omitted, items.len());
            let spent: u32 = r.iter().map(|x| header + tokens(&x.text)).sum();
            assert!(spent <= budget, "budget {budget}: spent {spent}");
            assert!(r.iter().all(|x| x.level <= Level::Full));
        }
        // Never above the wanted level.
        let (r, _) = pack(&items, Level::Abstract, Some(u32::MAX));
        assert!(r.iter().all(|x| x.level == Level::Abstract));
    }
}
