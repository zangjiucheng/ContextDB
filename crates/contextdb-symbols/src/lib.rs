//! Symbol extraction and normalisation for symbol-level code anchors
//! (SPEC SD18, ADR 0012). Observer-side only: `ctxd` never links this crate,
//! so the daemon's live path never parses.
mod extract;
mod walk;

use contextdb_anchors::Language;
use std::{collections::BTreeMap, ops::ControlFlow, time::Instant};

/// Walk revision (ADR 0012 §4); part of every normaliser version.
const WALK: u32 = 1;
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

/// Parses `bytes` and returns every symbol (ADR 0012 §2, §4). Parsing is
/// cancelled at `deadline` (unavailable, never a partial result).
pub fn parse(
    language: Language,
    bytes: &[u8],
    deadline: Instant,
) -> Result<ParsedFile, Unavailable> {
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
    let tree = parser
        .parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(options),
        )
        .ok_or_else(|| Unavailable("parse deadline".into()))?;
    let root = tree.root_node();
    let rules = walk::rules(language);
    let mut extract = extract::Extract {
        language,
        rules: &rules,
        src: bytes,
        table: BTreeMap::new(),
    };
    extract.container(root, &[]);
    let mut symbols = BTreeMap::new();
    for (path, occurrences) in extract.table {
        let mut hash = blake3::Hasher::new();
        let mut masked = blake3::Hasher::new();
        let mut error = false;
        for occ in &occurrences {
            hash.update(b"O");
            masked.update(b"O");
            for node in &occ.nodes {
                error |= node.has_error() || node.is_missing() || inside_error(*node);
                walk::Walk {
                    rules: &rules,
                    src: bytes,
                    hasher: &mut hash,
                    mask: None,
                }
                .node(*node);
                walk::Walk {
                    rules: &rules,
                    src: bytes,
                    hasher: &mut masked,
                    mask: occ.name.map(|n| n.id()),
                }
                .node(*node);
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
/// The node sits inside an `ERROR` node (error recovery wrapped it).
fn inside_error(node: tree_sitter::Node) -> bool {
    let mut n = node.parent();
    while let Some(p) = n {
        if p.is_error() {
            return true;
        }
        n = p.parent();
    }
    false
}
