//! The normalising walk (ADR 0012 §4, walk revision 1): an iterative
//! pre-order traversal streaming into BLAKE3. A kept inner node emits
//! `(` + kind and, after its kept children, `)`; a kept leaf emits `L` + kind +
//! text; every string is length-prefixed, so the encoding is injective and
//! carries the tree's shape (equivalent to kind + child count, but streamable).
use contextdb_anchors::Language;
use tree_sitter::Node;

/// Per-language rules: what formatters change is dropped, what changes
/// meaning is kept.
pub(crate) struct Rules {
    comments: &'static [&'static str],
    directive: fn(&str) -> bool,
    drop_semicolons: bool,
    transparent_parens: bool,
    /// Lists whose single trailing comma changes meaning (`(u8,)`, `a[1,]`).
    single_comma: &'static [&'static str],
    /// String literal kinds: quote leaves dropped, contents kept.
    strings: &'static [&'static str],
    /// Extra nodes dropped outright (Python line continuations).
    dropped: &'static [&'static str],
}
fn go_directive(t: &str) -> bool {
    ["//go:", "//export ", "//line ", "// +build", "//go:build"]
        .iter()
        .any(|p| t.starts_with(p))
}
fn ts_directive(t: &str) -> bool {
    ["@ts-ignore", "@ts-expect-error", "@ts-nocheck", "@ts-check"]
        .iter()
        .any(|d| t.contains(d))
}
fn js_directive(t: &str) -> bool {
    // Checked JavaScript reads types from JSDoc blocks.
    t.starts_with("/**") || ts_directive(t)
}
fn py_directive(t: &str) -> bool {
    t.trim_start_matches('#').trim_start().starts_with("type:")
}
fn never(_: &str) -> bool {
    false
}
pub(crate) fn rules(language: Language) -> Rules {
    match language {
        Language::Rust => Rules {
            comments: &["line_comment", "block_comment"],
            directive: never,
            drop_semicolons: false,
            transparent_parens: false,
            single_comma: &["tuple_expression", "tuple_type", "tuple_pattern"],
            strings: &[],
            dropped: &[],
        },
        Language::TypeScript | Language::Tsx => Rules {
            comments: &["comment", "html_comment"],
            directive: ts_directive,
            drop_semicolons: true,
            transparent_parens: true,
            single_comma: &[],
            strings: &["string"],
            dropped: &[],
        },
        Language::JavaScript => Rules {
            comments: &["comment", "html_comment"],
            directive: js_directive,
            drop_semicolons: true,
            transparent_parens: true,
            single_comma: &[],
            strings: &["string"],
            dropped: &[],
        },
        Language::Python => Rules {
            comments: &["comment"],
            directive: py_directive,
            drop_semicolons: false,
            transparent_parens: true,
            single_comma: &["tuple", "subscript", "tuple_pattern"],
            strings: &["string"],
            dropped: &["line_continuation"],
        },
        Language::Go => Rules {
            comments: &["comment"],
            directive: go_directive,
            drop_semicolons: true,
            transparent_parens: false,
            single_comma: &[],
            strings: &[],
            dropped: &[],
        },
    }
}

impl Rules {
    pub(crate) fn is_directive(&self, text: &str) -> bool {
        (self.directive)(text)
    }
}
fn closing(kind: &str) -> bool {
    matches!(kind, ")" | "]" | "}" | ">")
}
fn contains_optional_chain(node: Node) -> bool {
    let mut cursor = node.walk();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "?." || n.kind() == "optional_chain" {
            return true;
        }
        stack.extend(n.children(&mut cursor));
    }
    false
}
fn is_string(rules: &Rules, node: Node) -> bool {
    rules.strings.contains(&node.kind())
}

/// One normalised span stream. `mask` is the declared name node, emitted as a
/// placeholder for the name-masked hash.
pub(crate) struct Walk<'a> {
    pub rules: &'a Rules,
    pub src: &'a [u8],
    pub hasher: &'a mut blake3::Hasher,
    pub mask: Option<usize>,
}
impl Walk<'_> {
    fn bytes(&mut self, tag: u8, parts: &[&[u8]]) {
        self.hasher.update(&[tag]);
        for p in parts {
            self.hasher.update(&(p.len() as u64).to_le_bytes());
            self.hasher.update(p);
        }
    }
    fn text(&self, node: Node) -> &[u8] {
        &self.src[node.byte_range()]
    }
    /// Whether `child` (the `index`-th child of `parent`) is dropped.
    fn dropped(&self, parent: Node, index: usize, child: Node) -> bool {
        let kind = child.kind();
        if self.rules.comments.contains(&kind) {
            return !(self.rules.directive)(&String::from_utf8_lossy(self.text(child)));
        }
        if self.rules.dropped.contains(&kind) {
            return true;
        }
        if kind == ";" && !child.is_named() && self.rules.drop_semicolons {
            return true;
        }
        if kind == "," && !child.is_named() {
            // Trailing comma: the next non-comment sibling closes the list.
            let mut next = index + 1;
            while let Some(n) = parent.child(next as u32) {
                if self.rules.comments.contains(&n.kind()) {
                    next += 1;
                    continue;
                }
                if !closing(n.kind()) {
                    return false;
                }
                // A subscript's elements are its `subscript` fields (not the
                // indexed value); other lists' elements are their named children.
                let elements = if parent.kind() == "subscript" {
                    parent
                        .children_by_field_name("subscript", &mut parent.walk())
                        .count()
                } else {
                    (0..parent.child_count())
                        .filter_map(|i| parent.child(i))
                        .filter(|c| c.is_named() && !self.rules.comments.contains(&c.kind()))
                        .count()
                };
                return !(elements == 1 && self.rules.single_comma.contains(&parent.kind()));
            }
            return false;
        }
        if is_string(self.rules, parent) && !child.is_named() {
            // Quote characters of a string literal.
            return true;
        }
        false
    }
    /// Parentheses that only group (precedence is already in the tree's shape).
    fn transparent(&self, node: Node) -> bool {
        if !self.rules.transparent_parens || node.kind() != "parenthesized_expression" {
            return false;
        }
        let statement_string = node
            .parent()
            .is_some_and(|p| p.kind() == "expression_statement")
            && node
                .named_children(&mut node.walk())
                .any(|c| is_string(self.rules, c));
        !statement_string && !contains_optional_chain(node)
    }
    /// Streams one node's subtree (iteratively: no recursion on deep trees).
    pub fn node(&mut self, root: Node) {
        enum Step<'t> {
            Visit(Node<'t>),
            Close,
        }
        let mut stack = vec![Step::Visit(root)];
        while let Some(step) = stack.pop() {
            let node = match step {
                Step::Close => {
                    self.hasher.update(b")");
                    continue;
                }
                Step::Visit(n) => n,
            };
            let kept: Vec<Node> = (0..node.child_count())
                .filter_map(|i| node.child(i).map(|c| (i, c)))
                .filter(|(i, c)| !self.dropped(node, *i as usize, *c))
                .map(|(_, c)| c)
                .collect();
            if self.mask == Some(node.id()) {
                self.bytes(b'L', &[node.kind().as_bytes(), b"\0name"]);
                continue;
            }
            if node.kind() == "string_start" {
                // Python string prefix flags (r, b, f, u), without quotes.
                let flags: Vec<u8> = self
                    .text(node)
                    .iter()
                    .filter(|b| b.is_ascii_alphabetic())
                    .map(u8::to_ascii_lowercase)
                    .collect();
                self.bytes(b'L', &[b"prefix", &flags]);
                continue;
            }
            if node.kind() == "string_end" {
                continue;
            }
            if kept.is_empty() {
                let text = self.text(node).to_vec();
                self.bytes(b'L', &[node.kind().as_bytes(), &text]);
                continue;
            }
            let transparent = self.transparent(node);
            if !transparent {
                self.bytes(b'(', &[node.kind().as_bytes()]);
                stack.push(Step::Close);
            }
            for child in kept.into_iter().rev() {
                if transparent && !child.is_named() {
                    continue; // the grouping parentheses themselves
                }
                stack.push(Step::Visit(child));
            }
        }
    }
}
