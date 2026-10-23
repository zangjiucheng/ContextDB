//! The normalising walk (ADR 0012 §4, walk revision 2): an iterative
//! pre-order traversal streaming into BLAKE3. A kept inner node emits
//! `(` + kind and, after its kept children, `)`; a kept leaf emits `L` + kind +
//! text; every string is length-prefixed, so the encoding is injective and
//! carries the tree's shape (equivalent to kind + child count, but streamable).
use crate::Unavailable;
use contextdb_anchors::Language;
use std::{collections::HashSet, time::Instant};
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
    /// Kinds emitted under another kind's name: two grammar forms of one
    /// construct that formatters switch between (walk 2).
    renamed: &'static [(&'static str, &'static str)],
    /// Kinds whose `(`/`)` leaves are optional (Python tuples, imports).
    optional_parens: &'static [&'static str],
    /// Language-specific wrappers emitted as their children only (walk 2),
    /// given the node's parent and grandparent.
    unwrap: Unwrapper,
    /// JS/TS `string`s are emitted as one leaf of their decoded content, so a
    /// quote switch that drops `\'` escapes agrees; an identifier-like object
    /// key agrees with the unquoted key (walk 2).
    decode_strings: bool,
}
/// How a transparent node is emitted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unwrap {
    /// Its kept children, in place of the node.
    All,
    /// Only its named children (grouping punctuation dropped too).
    Named,
}
type Unwrapper = fn(Node, Option<Node>, Option<Node>) -> Option<Unwrap>;
fn no_unwrap(_: Node, _: Option<Node>, _: Option<Node>) -> Option<Unwrap> {
    None
}
fn rust_unwrap(node: Node, parent: Option<Node>, _: Option<Node>) -> Option<Unwrap> {
    fn named(n: Node) -> Vec<Node> {
        let mut c = n.walk();
        n.named_children(&mut c)
            .filter(|c| !matches!(c.kind(), "line_comment" | "block_comment"))
            .collect()
    }
    let has_semicolon = |n: Node| n.children(&mut n.walk()).any(|c| c.kind() == ";");
    // `return x;` and a tail `return x` diverge alike; rustfmt adds the
    // semicolon (also `break`, `continue`). A block-like tail (`match`,
    // `if`, …) without `;` is the block's value.
    let value_statement = |n: Node| {
        let inner = named(n);
        let diverging = inner.len() == 1
            && matches!(
                inner[0].kind(),
                "return_expression" | "break_expression" | "continue_expression"
            );
        diverging || !has_semicolon(n)
    };
    match node.kind() {
        "expression_statement" => value_statement(node).then_some(Unwrap::Named),
        // A match arm's `=> { e }` / `=> e,` and a closure's `|| { e }` /
        // `|| e`: rustfmt switches both ways by line width.
        "block"
            if parent.is_some_and(|p| matches!(p.kind(), "match_arm" | "closure_expression")) =>
        {
            let inner = named(node);
            let single = inner.len() == 1
                && match inner[0].kind() {
                    "expression_statement" => value_statement(inner[0]),
                    k => {
                        !matches!(k, "let_declaration" | "label" | "empty_statement")
                            && !k.ends_with("_item")
                            && !k.starts_with("attribute")
                            && !k.ends_with("_declaration")
                    }
                };
            single.then_some(Unwrap::Named)
        }
        _ => None,
    }
}
fn ts_unwrap(node: Node, parent: Option<Node>, grandparent: Option<Node>) -> Option<Unwrap> {
    // `(x) => …` and `x => …` (prettier's arrowParens): one plain parameter
    // without type, default or modifier is emitted as the bare identifier.
    fn plain_arrow_parameters(n: Node, parent: Option<Node>) -> bool {
        let Some(arrow) = parent.filter(|p| p.kind() == "arrow_function") else {
            return false;
        };
        if arrow
            .child_by_field_name("parameters")
            .is_none_or(|p| p.id() != n.id())
            || n.named_child_count() != 1
        {
            return false;
        }
        let only = n.named_child(0).expect("one named child");
        only.kind() == "identifier"
            || (only.kind() == "required_parameter"
                && only.child_count() == 1
                && only.child(0).is_some_and(|c| c.kind() == "identifier"))
    }
    match node.kind() {
        // Prettier's leading `|`/`&` on a multi-line union nests a one-member
        // union around the first member.
        "union_type" | "intersection_type" if node.named_child_count() == 1 => Some(Unwrap::Named),
        "formal_parameters" if plain_arrow_parameters(node, parent) => Some(Unwrap::Named),
        "required_parameter" if parent.is_some_and(|p| plain_arrow_parameters(p, grandparent)) => {
            Some(Unwrap::Named)
        }
        _ => None,
    }
}
fn python_unwrap(node: Node, _: Option<Node>, _: Option<Node>) -> Option<Unwrap> {
    match node.kind() {
        // Type-grammar wrappers: `x: (str | None)` parses as an expression.
        "type" | "type_parameter" => Some(Unwrap::All),
        // A pure wrapper; black's parentheses around a case pattern nest one.
        "case_pattern" => Some(Unwrap::All),
        // `for (x) in …`: one element and no comma is grouping, not a tuple.
        "tuple_pattern" => {
            let mut c = node.walk();
            let children: Vec<Node> = node.children(&mut c).collect();
            let elements = children
                .iter()
                .filter(|c| c.is_named() && c.kind() != "comment")
                .count();
            (elements == 1 && !children.iter().any(|c| c.kind() == ",")).then_some(Unwrap::Named)
        }
        _ => None,
    }
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
            renamed: &[],
            optional_parens: &[],
            unwrap: rust_unwrap,
            decode_strings: false,
        },
        Language::TypeScript | Language::Tsx => Rules {
            comments: &["comment", "html_comment"],
            directive: ts_directive,
            drop_semicolons: true,
            transparent_parens: true,
            single_comma: &[],
            strings: &["string"],
            dropped: &[],
            renamed: &[],
            optional_parens: &[],
            unwrap: ts_unwrap,
            decode_strings: true,
        },
        Language::JavaScript => Rules {
            comments: &["comment", "html_comment"],
            directive: js_directive,
            drop_semicolons: true,
            transparent_parens: true,
            single_comma: &[],
            strings: &["string"],
            dropped: &[],
            renamed: &[],
            optional_parens: &[],
            unwrap: ts_unwrap,
            decode_strings: true,
        },
        Language::Python => Rules {
            comments: &["comment"],
            directive: py_directive,
            drop_semicolons: false,
            transparent_parens: true,
            single_comma: &[
                "tuple",
                "subscript",
                "tuple_pattern",
                "expression_list",
                "pattern_list",
                "type_parameter",
            ],
            strings: &["string"],
            dropped: &["line_continuation"],
            renamed: &[
                ("union_type", "binary_operator"),
                ("generic_type", "subscript"),
                ("member_type", "attribute"),
                ("expression_list", "tuple"),
                ("pattern_list", "tuple_pattern"),
            ],
            optional_parens: &["tuple", "tuple_pattern", "import_from_statement"],
            unwrap: python_unwrap,
            decode_strings: false,
        },
        Language::Go => Rules {
            comments: &["comment"],
            directive: go_directive,
            drop_semicolons: true,
            transparent_parens: false,
            single_comma: &[],
            strings: &[],
            dropped: &[],
            renamed: &[],
            optional_parens: &[],
            unwrap: no_unwrap,
            decode_strings: false,
        },
    }
}

impl Rules {
    pub(crate) fn is_directive(&self, text: &str) -> bool {
        (self.directive)(text)
    }
    pub(crate) fn is_comment(&self, kind: &str) -> bool {
        self.comments.contains(&kind)
    }
}
fn closing(kind: &str) -> bool {
    matches!(kind, ")" | "]" | "}" | ">")
}
/// The deadline, checked every 1,024 visited nodes: extraction and the walk
/// are cancelled like the parse (ADR 0012 §4), never left to run long.
pub(crate) struct Clock {
    pub deadline: Instant,
    ticks: u32,
}
impl Clock {
    pub fn new(deadline: Instant) -> Self {
        Clock { deadline, ticks: 0 }
    }
    pub fn tick(&mut self) -> Result<(), Unavailable> {
        self.ticks = self.ticks.wrapping_add(1);
        if self.ticks.is_multiple_of(1024) && Instant::now() >= self.deadline {
            return Err(Unavailable("parse deadline".into()));
        }
        Ok(())
    }
}

/// Facts about ancestors and subtrees, computed in one pass over the tree so
/// the walk never climbs to a parent (O(depth) in tree-sitter) or rescans a
/// subtree: deep trees stay linear.
#[derive(Default)]
pub(crate) struct Facts {
    /// Nodes with an `ERROR` ancestor.
    under_error: HashSet<usize>,
    /// Parenthesised expressions containing an optional chain.
    optional: HashSet<usize>,
}
impl Facts {
    pub fn of(rules: &Rules, root: Node, clock: &mut Clock) -> Result<Facts, Unavailable> {
        let mut facts = Facts::default();
        if !root.has_error() && !rules.transparent_parens {
            return Ok(facts);
        }
        enum Step<'t> {
            Enter(Node<'t>, bool),
            Exit(Node<'t>),
        }
        // `found[i]`: an optional chain below the i-th open node.
        let mut found = vec![false];
        let mut stack = vec![Step::Enter(root, false)];
        let mut cursor = root.walk();
        while let Some(step) = stack.pop() {
            clock.tick()?;
            match step {
                Step::Enter(n, under_error) => {
                    if under_error {
                        facts.under_error.insert(n.id());
                    }
                    found.push(false);
                    stack.push(Step::Exit(n));
                    let below = under_error || n.is_error();
                    let children: Vec<Node> = n.children(&mut cursor).collect();
                    stack.extend(children.into_iter().rev().map(|c| Step::Enter(c, below)));
                }
                Step::Exit(n) => {
                    let mine = found.pop().expect("entered")
                        || matches!(n.kind(), "?." | "optional_chain");
                    if mine && n.kind() == "parenthesized_expression" {
                        facts.optional.insert(n.id());
                    }
                    *found.last_mut().expect("root's slot") |= mine;
                }
            }
        }
        Ok(facts)
    }
    /// The node sits inside an `ERROR` node (error recovery wrapped it).
    pub fn under_error(&self, node: Node) -> bool {
        self.under_error.contains(&node.id())
    }
}
fn is_string(rules: &Rules, node: Node) -> bool {
    rules.strings.contains(&node.kind())
}

/// One normalised span stream, and optionally in the same pass the
/// name-masked stream: identical except that the declared name node is a
/// placeholder.
pub(crate) struct Walk<'a> {
    pub rules: &'a Rules,
    pub facts: &'a Facts,
    pub clock: &'a mut Clock,
    pub src: &'a [u8],
    pub hasher: &'a mut blake3::Hasher,
    /// The name-masked stream's hasher and the declared name node's id.
    pub masked: Option<(&'a mut blake3::Hasher, usize)>,
    /// A readable copy of the stream, for corpus triage (never hashed).
    pub trace: Option<&'a mut Vec<String>>,
    /// Inside the name node: the masked stream already has its placeholder.
    pub in_name: bool,
}
fn put(hasher: &mut blake3::Hasher, tag: u8, parts: &[&[u8]]) {
    hasher.update(&[tag]);
    for p in parts {
        hasher.update(&(p.len() as u64).to_le_bytes());
        hasher.update(p);
    }
}
impl Walk<'_> {
    fn bytes(&mut self, tag: u8, parts: &[&[u8]]) {
        if let Some(trace) = self.trace.as_deref_mut() {
            let parts: Vec<_> = parts.iter().map(|p| String::from_utf8_lossy(p)).collect();
            trace.push(format!("{}{}", tag as char, parts.join(" ")));
        }
        put(self.hasher, tag, parts);
        if let Some((masked, _)) = self.masked.as_mut()
            && !self.in_name
        {
            put(masked, tag, parts);
        }
    }
    fn text(&self, node: Node) -> &[u8] {
        &self.src[node.byte_range()]
    }
    /// Whether the `index`-th of `parent`'s `children` is dropped.
    fn dropped(&self, parent: Node, children: &[Node], index: usize) -> bool {
        let child = children[index];
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
        if matches!(kind, "(" | ")")
            && !child.is_named()
            && self.rules.optional_parens.contains(&parent.kind())
        {
            return true;
        }
        if matches!(kind, "|" | "&")
            && index == 0
            && matches!(parent.kind(), "union_type" | "intersection_type")
        {
            return true; // prettier's leading `|` on a multi-line union
        }
        if kind == "," && parent.kind() == "match_arm" {
            return true; // required or not by the arm's form, never meaning
        }
        if kind == "," && !child.is_named() {
            // Trailing comma: the next non-comment sibling closes the list.
            let mut next = index + 1;
            while let Some(&n) = children.get(next) {
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
                    children
                        .iter()
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
    fn transparent(&self, node: Node, parent: Option<Node>, grand: Option<Node>) -> Option<Unwrap> {
        if let Some(how) = (self.rules.unwrap)(node, parent, grand) {
            return Some(how);
        }
        if !self.rules.transparent_parens || node.kind() != "parenthesized_expression" {
            return None;
        }
        let statement_string = parent.is_some_and(|p| p.kind() == "expression_statement")
            && node
                .named_children(&mut node.walk())
                .any(|c| is_string(self.rules, c));
        (!statement_string && !self.facts.optional.contains(&node.id())).then_some(Unwrap::Named)
    }
    /// A JS/TS string's content without its quotes, `\'` and `\"` unescaped.
    fn decoded(&self, node: Node) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            let text = self.text(child);
            match (child.kind(), text) {
                ("escape_sequence", b"\\'") => out.push(b'\''),
                ("escape_sequence", b"\\\"") => out.push(b'"'),
                _ => out.extend_from_slice(text),
            }
        }
        out
    }
    fn kind<'n>(&self, node: Node<'n>) -> &'n str {
        let kind = node.kind();
        self.rules
            .renamed
            .iter()
            .find(|(from, _)| *from == kind)
            .map_or(kind, |(_, to)| to)
    }
    /// Streams one node's subtree (iteratively: no recursion on deep trees;
    /// each node carries its parent and grandparent down).
    pub fn node(&mut self, root: Node) -> Result<(), Unavailable> {
        enum Step<'t> {
            Visit(Node<'t>, Option<Node<'t>>, Option<Node<'t>>),
            Close,
            /// Leaving the name node.
            LeaveName,
        }
        let parent = root.parent();
        let mut stack = vec![Step::Visit(root, parent, parent.and_then(|p| p.parent()))];
        let mut cursor = root.walk();
        // Reused per node: the children, and the indices of the kept ones.
        let mut children: Vec<Node> = Vec::new();
        let mut kept: Vec<usize> = Vec::new();
        while let Some(step) = stack.pop() {
            self.clock.tick()?;
            let (node, parent, grand) = match step {
                Step::Close => {
                    if let Some(trace) = self.trace.as_deref_mut() {
                        trace.push(")".into());
                    }
                    self.hasher.update(b")");
                    if let Some((masked, _)) = self.masked.as_mut()
                        && !self.in_name
                    {
                        masked.update(b")");
                    }
                    continue;
                }
                Step::LeaveName => {
                    self.in_name = false;
                    continue;
                }
                Step::Visit(n, p, g) => (n, p, g),
            };
            if let Some((masked, name)) = self.masked.as_mut()
                && *name == node.id()
            {
                put(masked, b'L', &[node.kind().as_bytes(), b"\0name"]);
                self.in_name = true;
                stack.push(Step::LeaveName);
            }
            children.clear();
            children.extend(node.children(&mut cursor));
            kept.clear();
            kept.extend((0..children.len()).filter(|&i| !self.dropped(node, &children, i)));
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
            if self.rules.decode_strings && is_string(self.rules, node) {
                let text = self.decoded(node);
                let key = parent
                    .and_then(|p| p.child_by_field_name("key"))
                    .is_some_and(|k| k.id() == node.id());
                let identifier = text.first().is_some_and(|b| !b.is_ascii_digit())
                    && text
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$'));
                let kind: &[u8] = if key && identifier {
                    b"property_identifier"
                } else {
                    b"string"
                };
                self.bytes(b'L', &[kind, &text]);
                continue;
            }
            if kept.is_empty() {
                let text = self.text(node).to_vec();
                self.bytes(b'L', &[self.kind(node).as_bytes(), &text]);
                continue;
            }
            let transparent = self.transparent(node, parent, grand);
            if transparent.is_none() {
                self.bytes(b'(', &[self.kind(node).as_bytes()]);
                stack.push(Step::Close);
            }
            for &i in kept.iter().rev() {
                let child = children[i];
                if transparent == Some(Unwrap::Named) && !child.is_named() {
                    continue; // the grouping punctuation itself
                }
                stack.push(Step::Visit(child, Some(node), parent));
            }
        }
        Ok(())
    }
}
