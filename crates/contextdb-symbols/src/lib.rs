//! Symbol extraction and normalisation for symbol-level code anchors
//! (SPEC SD18, ADR 0012). Observer-side only: `ctxd` never links this crate,
//! so the daemon's live path never parses.
mod extract;
pub mod observer;
mod walk;

use contextdb_anchors::Language;
pub use extract::MAX_NESTING;
use std::{collections::BTreeMap, ops::ControlFlow, time::Instant};

/// Walk revision (ADR 0012 §4); part of every normaliser version.
const WALK: u32 = 2;
const RUNTIME: &str = "tree-sitter=0.27.1";

fn grammar(language: Language) -> (tree_sitter::Language, &'static str) {
    match language {
        Language::Rust => (tree_sitter_rust::LANGUAGE.into(), "tree-sitter-rust=0.24.2"),
        Language::TypeScript => (
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            "tree-sitter-typescript=0.23.2",
        ),
        Language::Tsx => (
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            "tree-sitter-typescript=0.23.2",
        ),
        Language::JavaScript => (
            tree_sitter_javascript::LANGUAGE.into(),
            "tree-sitter-javascript=0.25.0",
        ),
        Language::Python => (
            tree_sitter_python::LANGUAGE.into(),
            "tree-sitter-python=0.25.0",
        ),
        Language::Go => (tree_sitter_go::LANGUAGE.into(), "tree-sitter-go=0.25.0"),
    }
}
/// The normaliser version for a language, derived mechanically from the exact
/// grammar and runtime versions plus the walk revision (ADR 0012 §4).
pub fn normaliser_version(language: Language) -> String {
    format!(
        "{}:{};{RUNTIME};walk={WALK}",
        language.name(),
        grammar(language).1
    )
}
pub(crate) fn walk_directive(rules: &walk::Rules, text: &str) -> bool {
    rules.is_directive(text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    /// Normalised hash of all occurrences in source order.
    pub hash: [u8; 32],
    /// The same with each occurrence's declared name masked.
    pub masked: [u8; 32],
    /// An occurrence's span, or a node enclosing it, has a parse error.
    pub error: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedFile {
    pub language: Language,
    /// The file has a parse error anywhere.
    pub has_error: bool,
    /// Symbol path (real spelling, `::`-separated) → symbol.
    pub symbols: BTreeMap<String, Symbol>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable(pub String);
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unavailable {}

/// Parses with cancellation at `deadline` (unavailable, never a partial tree).
fn tree(
    language: Language,
    bytes: &[u8],
    deadline: Instant,
) -> Result<tree_sitter::Tree, Unavailable> {
    // A small input can parse before the progress callback first runs.
    if Instant::now() >= deadline {
        return Err(Unavailable("parse deadline".into()));
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&grammar(language).0)
        .map_err(|e| Unavailable(e.to_string()))?;
    let mut cancel = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut cancel);
    parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(options),
        )
        .ok_or_else(|| Unavailable("parse deadline".into()))
}

/// The symbol table of a parsed tree, with the facts and the clock the walk
/// goes on with; unavailable at the deadline.
fn table<'t>(
    language: Language,
    rules: &walk::Rules,
    bytes: &[u8],
    root: tree_sitter::Node<'t>,
    deadline: Instant,
) -> Result<(extract::Table<'t>, walk::Facts, walk::Clock), Unavailable> {
    let mut clock = walk::Clock::new(deadline);
    let facts = walk::Facts::of(rules, root, &mut clock)?;
    let mut extract = extract::Extract {
        language,
        rules,
        src: bytes,
        table: BTreeMap::new(),
        clock,
        depth: 0,
        failed: None,
    };
    extract.container(root, &[]);
    if let Some(e) = extract.failed {
        return Err(e);
    }
    Ok((extract.table, facts, extract.clock))
}

/// Parses `bytes` and returns every symbol (ADR 0012 §2, §4). Parsing,
/// extraction and the walk are cancelled at `deadline` (unavailable, never a
/// partial result).
pub fn parse(
    language: Language,
    bytes: &[u8],
    deadline: Instant,
) -> Result<ParsedFile, Unavailable> {
    let tree = tree(language, bytes, deadline)?;
    let root = tree.root_node();
    let rules = walk::rules(language);
    let (table, facts, mut clock) = table(language, &rules, bytes, root, deadline)?;
    let mut symbols = BTreeMap::new();
    for (path, occurrences) in table {
        let mut hash = blake3::Hasher::new();
        let mut masked = blake3::Hasher::new();
        let mut error = false;
        for occ in &occurrences {
            hash.update(b"O");
            masked.update(b"O");
            for node in &occ.nodes {
                error |= node.has_error() || node.is_missing() || facts.under_error(*node);
                // One pass feeds both streams; without a name node the
                // masked stream is the plain one.
                let name = occ.name.map_or(usize::MAX, |n| n.id());
                walk::Walk {
                    rules: &rules,
                    facts: &facts,
                    clock: &mut clock,
                    src: bytes,
                    hasher: &mut hash,
                    masked: Some((&mut masked, name)),
                    trace: None,
                    in_name: false,
                }
                .node(*node)?;
            }
        }
        symbols.insert(
            path,
            Symbol {
                hash: *hash.finalize().as_bytes(),
                masked: *masked.finalize().as_bytes(),
                error,
            },
        );
    }
    Ok(ParsedFile {
        language,
        has_error: root.has_error(),
        symbols,
    })
}

/// Byte ranges of the comments the walk drops anywhere in the file (directive
/// comments excluded), in source order: the T45 corpus edits them (ADR 0012
/// §4) to check that comment-only changes are no-ops.
pub fn dropped_comments(
    language: Language,
    bytes: &[u8],
    deadline: Instant,
) -> Result<Vec<std::ops::Range<usize>>, Unavailable> {
    let tree = tree(language, bytes, deadline)?;
    let rules = walk::rules(language);
    let mut clock = walk::Clock::new(deadline);
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    let mut cursor = tree.walk();
    while let Some(n) = stack.pop() {
        clock.tick()?;
        if rules.is_comment(n.kind()) {
            if !rules.is_directive(&String::from_utf8_lossy(&bytes[n.byte_range()])) {
                out.push(n.byte_range());
            }
            continue;
        }
        let children: Vec<_> = n.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    Ok(out)
}

/// The readable normalised stream of one symbol (all occurrences), or `None`
/// if the file has no such symbol: corpus triage only, never hashed.
pub fn trace(
    language: Language,
    bytes: &[u8],
    symbol: &str,
    deadline: Instant,
) -> Result<Option<Vec<String>>, Unavailable> {
    let tree = tree(language, bytes, deadline)?;
    let rules = walk::rules(language);
    let (mut table, facts, mut clock) = table(language, &rules, bytes, tree.root_node(), deadline)?;
    let Some(occurrences) = table.remove(symbol) else {
        return Ok(None);
    };
    let mut lines = Vec::new();
    let mut sink = blake3::Hasher::new();
    for occ in &occurrences {
        lines.push("O".to_owned());
        for node in &occ.nodes {
            walk::Walk {
                rules: &rules,
                facts: &facts,
                clock: &mut clock,
                src: bytes,
                hasher: &mut sink,
                masked: None,
                trace: Some(&mut lines),
                in_name: false,
            }
            .node(*node)?;
        }
    }
    Ok(Some(lines))
}
