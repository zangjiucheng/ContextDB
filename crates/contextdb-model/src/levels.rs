//! Read levels (ADR 0016 §4.1): `abstract` and `outline` are pure,
//! deterministic functions of a revision's payload bytes, with no model and
//! no dependency. A level is a rendering of an immutable revision, so
//! certification stays per revision (§4.2).
//!
//! - **abstract**: the first paragraph after a leading front-matter block
//!   and a leading heading line, cut at [`ABSTRACT_MAX`] bytes; if empty,
//!   the first [`ABSTRACT_MAX`] bytes of the body.
//! - **outline**: every Markdown heading line outside code fences, each
//!   followed by its section's first non-empty line (cut at
//!   [`OUTLINE_LINE_MAX`] bytes), up to [`OUTLINE_MAX`] bytes; a body
//!   without headings gives the abstract and the line count.
//!
//! Every cut ends in `…` and is reported, so a reader can tell that a
//! rendering may omit a qualifier further down.

/// Versioned with the projection's stored copies (T87).
pub const LEVELS_VERSION: u32 = 1;
pub const ABSTRACT_MAX: usize = 320;
pub const OUTLINE_LINE_MAX: usize = 160;
pub const OUTLINE_MAX: usize = 2048;
const ELLIPSIS: &str = "…";

/// How much of a revision a reader is shown (ADR 0016 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// No body text: key, status and the other header fields only.
    Header,
    Abstract,
    Outline,
    Full,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Header => "header",
            Level::Abstract => "abstract",
            Level::Outline => "outline",
            Level::Full => "full",
        }
    }
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s {
            "header" => Level::Header,
            "abstract" => Level::Abstract,
            "outline" => Level::Outline,
            "full" => Level::Full,
            _ => return None,
        })
    }
}

/// A rendering and whether anything was left out of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub text: String,
    pub cut: bool,
}

/// The body at `level`. `Full` is the body as stored (lossy UTF-8) and
/// never `cut`; `Header` is empty.
pub fn render(body: &[u8], level: Level) -> Rendered {
    match level {
        Level::Header => Rendered {
            text: String::new(),
            cut: false,
        },
        Level::Abstract => abstract_of(body),
        Level::Outline => outline_of(body),
        Level::Full => Rendered {
            text: String::from_utf8_lossy(body).into_owned(),
            cut: false,
        },
    }
}

/// Cuts `s` to at most `max` bytes on a char boundary.
fn cut_at(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn with_ellipsis(s: &str, max: usize) -> (String, bool) {
    let (head, cut) = cut_at(s, max);
    if cut {
        (format!("{}{ELLIPSIS}", head.trim_end()), true)
    } else {
        (head.to_string(), false)
    }
}

/// Lines after a leading `---` front-matter block (if it is closed).
fn skip_front_matter(lines: &[&str]) -> usize {
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return 0;
    }
    match lines.iter().skip(1).position(|l| l.trim_end() == "---") {
        Some(p) => p + 2,
        None => 0,
    }
}

fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    let hashes = t.bytes().take_while(|b| *b == b'#').count();
    (1..=6).contains(&hashes) && t[hashes..].starts_with([' ', '\t'])
        || (hashes > 0 && hashes <= 6 && t.len() == hashes)
}

fn is_fence(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

pub fn abstract_of(body: &[u8]) -> Rendered {
    let text = String::from_utf8_lossy(body);
    let lines: Vec<&str> = text.lines().collect();
    let mut i = skip_front_matter(&lines);
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i < lines.len() && is_heading(lines[i]) {
        i += 1;
    }
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    let mut para: Vec<&str> = Vec::new();
    while i < lines.len() && !lines[i].trim().is_empty() {
        para.push(lines[i].trim_end());
        i += 1;
    }
    let rest_has_text = lines[i.min(lines.len())..]
        .iter()
        .any(|l| !l.trim().is_empty());
    let joined = para.join("\n");
    if joined.trim().is_empty() {
        let (t, cut) = with_ellipsis(text.trim(), ABSTRACT_MAX);
        return Rendered { text: t, cut };
    }
    let (t, cut) = with_ellipsis(&joined, ABSTRACT_MAX);
    Rendered {
        text: t,
        cut: cut || rest_has_text,
    }
}

pub fn outline_of(body: &[u8]) -> Rendered {
    let text = String::from_utf8_lossy(body);
    let lines: Vec<&str> = text.lines().collect();
    let start = skip_front_matter(&lines);
    let mut out: Vec<String> = Vec::new();
    let mut fenced = false;
    let mut want_first = false;
    let mut cut = false;
    let mut headings = 0usize;
    for line in &lines[start..] {
        if is_fence(line) {
            fenced = !fenced;
            if want_first {
                // A section starting with code: its first line is the fence.
                out.push(line.trim_end().to_string());
                want_first = false;
            } else {
                cut = true;
            }
            continue;
        }
        if !fenced && is_heading(line) {
            out.push(line.trim().to_string());
            headings += 1;
            want_first = true;
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if want_first {
            let (t, c) = with_ellipsis(line.trim(), OUTLINE_LINE_MAX);
            cut |= c;
            out.push(t);
            want_first = false;
        } else {
            cut = true;
        }
    }
    if headings == 0 {
        let a = abstract_of(body);
        let n = lines.len();
        return Rendered {
            text: format!("{}\n({n} lines)", a.text),
            cut: a.cut || n > 1,
        };
    }
    let joined = out.join("\n");
    let (t, c) = with_ellipsis(&joined, OUTLINE_MAX);
    Rendered {
        text: t,
        cut: cut || c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abstract_skips_front_matter_and_heading() {
        let body = b"---\nkind: decision\n---\n# Title\n\nFirst line\nsecond line.\n\nMore text.\n";
        let a = abstract_of(body);
        assert_eq!(a.text, "First line\nsecond line.");
        assert!(a.cut, "more text follows");
        let one = abstract_of(b"Just this.");
        assert_eq!(
            one,
            Rendered {
                text: "Just this.".into(),
                cut: false
            }
        );
    }

    #[test]
    fn abstract_cuts_on_a_char_boundary() {
        let body = "é".repeat(400);
        let a = abstract_of(body.as_bytes());
        assert!(a.cut);
        assert!(a.text.ends_with(ELLIPSIS));
        assert!(a.text.len() <= ABSTRACT_MAX + ELLIPSIS.len());
    }

    #[test]
    fn empty_paragraph_falls_back_to_the_body() {
        let a = abstract_of(b"# Only a heading\n");
        assert_eq!(a.text, "# Only a heading");
        assert_eq!(abstract_of(b"").text, "");
    }

    #[test]
    fn outline_lists_headings_with_first_lines_outside_fences() {
        let body = b"# A\nintro a\nmore a\n```\n# not a heading\n```\n## B\n\nfirst b\n";
        let o = outline_of(body);
        assert_eq!(o.text, "# A\nintro a\n## B\nfirst b");
        assert!(o.cut);
    }

    #[test]
    fn outline_without_headings_is_the_abstract_and_line_count() {
        let o = outline_of(b"one\ntwo\n");
        assert_eq!(o.text, "one\ntwo\n(2 lines)");
    }

    #[test]
    fn levels_order_and_names_round_trip() {
        assert!(Level::Header < Level::Abstract && Level::Outline < Level::Full);
        for l in [Level::Header, Level::Abstract, Level::Outline, Level::Full] {
            assert_eq!(Level::parse(l.name()), Some(l));
        }
        assert_eq!(render(b"x", Level::Header).text, "");
        assert_eq!(render(b"x\n", Level::Full).text, "x\n");
    }
}
