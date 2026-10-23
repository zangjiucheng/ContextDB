//! Deliberately small SPEC-form oracle for PROP12's generated declaration
//! language. Shares pinned grammars, but no production extraction, walk,
//! trace, descriptor, position or classification code. Unsupported syntax is
//! outside this generator's domain (the broad forms have G2 goldens).
use contextdb_anchors::Language;
use std::collections::BTreeMap;
use tree_sitter::Node;

pub fn version(l: Language) -> String {
    let (name, grammar) = match l {
        Language::Rust => ("rust", "tree-sitter-rust=0.24.2"),
        Language::TypeScript => ("typescript", "tree-sitter-typescript=0.23.2"),
        Language::Tsx => ("tsx", "tree-sitter-typescript=0.23.2"),
        Language::JavaScript => ("javascript", "tree-sitter-javascript=0.25.0"),
        Language::Python => ("python", "tree-sitter-python=0.25.0"),
        Language::Go => ("go", "tree-sitter-go=0.25.0"),
    };
    format!("{name}:{grammar};tree-sitter=0.27.1;walk=2")
}

fn grammar(l: Language) -> tree_sitter::Language {
    match l {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    }
}
fn comment(n: Node) -> bool {
    matches!(n.kind(), "comment" | "line_comment" | "block_comment")
}
fn directive(l: Language, t: &str) -> bool {
    match l {
        Language::Rust => false,
        Language::Python => t.trim_start_matches('#').trim_start().starts_with("type:"),
        Language::Go => ["//go:", "//export ", "//line ", "// +build"]
            .iter()
            .any(|s| t.starts_with(s)),
        Language::JavaScript if t.starts_with("/**") => true,
        _ => ["@ts-ignore", "@ts-expect-error", "@ts-nocheck", "@ts-check"]
            .iter()
            .any(|s| t.contains(s)),
    }
}
fn part(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}
fn leaf(out: &mut Vec<u8>, kind: &str, text: &[u8]) {
    out.push(b'L');
    part(out, kind.as_bytes());
    part(out, text);
}
fn normalise(l: Language, n: Node, src: &[u8], mask: Option<usize>, out: &mut Vec<u8>) {
    if mask == Some(n.id()) {
        leaf(out, n.kind(), b"\0name");
        return;
    }
    // ADR §4 walk-2: the JS/TS grammar kind `string` becomes one content
    // leaf. This kind also names TS's zero-child string type keyword; its
    // surrounding predefined_type still distinguishes the type context.
    if matches!(
        l,
        Language::TypeScript | Language::Tsx | Language::JavaScript
    ) && n.kind() == "string"
    {
        let mut text = Vec::new();
        for child in n.named_children(&mut n.walk()) {
            let bytes = &src[child.byte_range()];
            if bytes == b"\\\"" {
                text.push(b'"');
            } else if bytes == b"\\'" {
                text.push(b'\'');
            } else {
                text.extend_from_slice(bytes);
            }
        }
        leaf(out, "string", &text);
        return;
    }
    let mut c = n.walk();
    let children: Vec<_> = n.children(&mut c).collect();
    let kept: Vec<_> = children
        .iter()
        .enumerate()
        .filter_map(|(i, child)| {
            if comment(*child) {
                return directive(l, std::str::from_utf8(&src[child.byte_range()]).unwrap())
                    .then_some(*child);
            }
            if child.kind() == ";" && l != Language::Rust && l != Language::Python {
                return None;
            }
            if child.kind() == ","
                && children
                    .get(i + 1)
                    .is_some_and(|n| matches!(n.kind(), ")" | "}"))
            {
                return None; // generated functions have no tuples or subscripts
            }
            Some(*child)
        })
        .collect();
    let transparent = (l == Language::Python && matches!(n.kind(), "type" | "type_parameter"))
        || (l == Language::Rust
            && n.kind() == "expression_statement"
            && kept.iter().any(|n| n.kind() == "return_expression"));
    if transparent {
        for child in kept {
            if l != Language::Rust || child.is_named() {
                normalise(l, child, src, mask, out);
            }
        }
    } else if kept.is_empty() {
        leaf(out, n.kind(), &src[n.byte_range()]);
    } else {
        out.push(b'(');
        part(out, n.kind().as_bytes());
        for child in kept {
            normalise(l, child, src, mask, out);
        }
        out.push(b')');
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub hash: Vec<u8>,
    pub masked: Vec<u8>,
    pub error: bool,
}
#[derive(Clone, Debug)]
pub struct File {
    pub has_error: bool,
    pub symbols: BTreeMap<String, Symbol>,
}

/// Top-level generated functions, their preceding attributes/directives, and
/// Python decorator wrappers. Group duplicate names in source order. Walking
/// only root children prevents accidental extraction inside function bodies.
pub fn parse(l: Language, src: &[u8]) -> File {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&grammar(l)).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();
    let mut groups: BTreeMap<String, (Vec<u8>, Vec<u8>, bool)> = BTreeMap::new();
    let mut pending = Vec::new();
    let mut c = root.walk();
    for n in root.children(&mut c) {
        let text = std::str::from_utf8(&src[n.byte_range()]).unwrap();
        if n.kind() == "attribute_item" || (comment(n) && directive(l, text)) {
            pending.push(n);
            continue;
        }
        if comment(n) {
            continue;
        }
        let declaration = if n.kind() == "decorated_definition" {
            n.child_by_field_name("definition").unwrap()
        } else {
            n
        };
        if !matches!(
            declaration.kind(),
            "function_item" | "function_definition" | "function_declaration"
        ) {
            pending.clear();
            continue;
        }
        let name = declaration.child_by_field_name("name").unwrap();
        let label = std::str::from_utf8(&src[name.byte_range()])
            .unwrap()
            .to_owned();
        let (plain, masked, error) = groups.entry(label).or_default();
        plain.push(b'O');
        masked.push(b'O');
        pending.push(n);
        for span in pending.drain(..) {
            *error |= span.has_error() || span.is_missing();
            normalise(l, span, src, None, plain);
            normalise(l, span, src, Some(name.id()), masked);
        }
    }
    File {
        has_error: root.has_error(),
        symbols: groups
            .into_iter()
            .map(|(name, (plain, masked, error))| {
                (
                    name,
                    Symbol {
                        hash: blake3::hash(&plain).as_bytes().to_vec(),
                        masked: blake3::hash(&masked).as_bytes().to_vec(),
                        error,
                    },
                )
            })
            .collect(),
    }
}

pub fn wire(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}
pub fn descriptor(repo: &str, path: &str, name: &str, l: Language, s: &Symbol) -> Vec<u8> {
    let mut out = b"CTXANC02".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    for value in [repo, path, name] {
        wire(&mut out, value.as_bytes());
    }
    out.push(match l {
        Language::Rust => 1,
        Language::TypeScript => 2,
        Language::Tsx => 3,
        Language::JavaScript => 4,
        Language::Python => 5,
        Language::Go => 6,
    });
    wire(&mut out, version(l).as_bytes());
    wire(&mut out, &s.hash);
    wire(&mut out, &s.masked);
    out
}
