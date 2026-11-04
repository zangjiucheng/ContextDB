//! T87 (ADR 0016 §4.1): `abstract` and `outline` are pure functions of the
//! payload bytes. Over random Markdown-ish and random byte bodies they are
//! deterministic, never panic, stay within their size bounds, and never cut
//! without saying so.
use contextdb_model::levels::*;

/// xorshift64*: a fixed-seed generator, so the cases are reproducible
/// without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const PIECES: &[&str] = &[
    "# ",
    "## ",
    "###### ",
    "####### ",
    "---",
    "```",
    "~~~",
    "\n",
    "\n\n",
    " ",
    "\t",
    "word",
    "é",
    "漢字",
    "🦀",
    "…",
    "%",
    ".",
    "-",
    "text with spaces",
    "a.b",
    "\r\n",
];

fn body(r: &mut Rng) -> Vec<u8> {
    if r.below(5) == 0 {
        // Raw bytes, often invalid UTF-8.
        return (0..r.below(900)).map(|_| r.next() as u8).collect();
    }
    let mut s = String::new();
    for _ in 0..r.below(400) {
        s.push_str(PIECES[r.below(PIECES.len() as u64) as usize]);
    }
    s.into_bytes()
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn t87_levels_are_pure_bounded_and_honest() {
    let seeds: u64 = std::env::var("CONTEXTDB_T87_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000);
    let (mut cut_a, mut cut_o) = (0, 0);
    for seed in 0..seeds {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let b = body(&mut r);
        let a = abstract_of(&b);
        let o = outline_of(&b);
        assert_eq!(
            a,
            abstract_of(&b),
            "seed {seed}: abstract not deterministic"
        );
        assert_eq!(o, outline_of(&b), "seed {seed}: outline not deterministic");
        assert!(
            a.text.len() <= ABSTRACT_MAX + "…".len(),
            "seed {seed}: abstract {} bytes",
            a.text.len()
        );
        assert!(
            o.text.len() <= OUTLINE_MAX + "…".len() + ABSTRACT_MAX + 32,
            "seed {seed}: outline {} bytes",
            o.text.len()
        );
        // A rendering shorter than the body's text either is the whole
        // text (modulo whitespace) or says it was cut.
        let full = String::from_utf8_lossy(&b);
        // (Compared with whitespace runs collapsed: line endings and
        // trailing blanks are normalised.)
        if !a.cut {
            assert!(
                squash(&full).contains(&squash(&a.text)),
                "seed {seed}: uncut abstract is not a substring"
            );
        }
        cut_a += a.cut as u32;
        cut_o += o.cut as u32;
        assert_eq!(render(&b, Level::Abstract), a);
        assert_eq!(render(&b, Level::Outline), o);
        assert!(!render(&b, Level::Full).cut);
    }
    // Coverage: both outcomes occur.
    assert!(
        cut_a > 0 && (cut_a as u64) < seeds,
        "abstract cut {cut_a}/{seeds}"
    );
    assert!(
        cut_o > 0 && (cut_o as u64) < seeds,
        "outline cut {cut_o}/{seeds}"
    );
}
