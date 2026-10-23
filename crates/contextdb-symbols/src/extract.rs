//! Symbol tables per language (ADR 0012 §2). Only declarations reachable
//! through item nesting are symbols; nothing inside a function body is.
//! Same-path declarations are grouped in source order.
use crate::{
    Unavailable,
    walk::{Clock, Rules},
};
use contextdb_anchors::{Language, symbol::key_hash};
use std::collections::BTreeMap;
use tree_sitter::Node;

/// One occurrence of a symbol: attached attributes, decorators and directive
/// comments, then the declaration; and its name node (masked in the
/// name-masked hash).
pub(crate) struct Occurrence<'t> {
    pub nodes: Vec<Node<'t>>,
    pub name: Option<Node<'t>>,
}
pub(crate) type Table<'t> = BTreeMap<String, Vec<Occurrence<'t>>>;

/// Containers nested deeper than this hold no symbols (ADR 0012 §4): the
/// extraction recursion stays far inside the observer's stack.
pub const MAX_NESTING: usize = 256;

pub(crate) struct Extract<'a, 't> {
    pub language: Language,
    pub rules: &'a Rules,
    pub src: &'a [u8],
    pub table: Table<'t>,
    pub clock: Clock,
    /// Containers entered and not yet left.
    pub depth: usize,
    /// Set when the deadline passed; the table is then incomplete.
    pub failed: Option<Unavailable>,
}
fn ident_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}
impl<'a, 't> Extract<'a, 't> {
    fn text(&self, n: Node) -> &'a str {
        std::str::from_utf8(&self.src[n.byte_range()]).unwrap_or("")
    }
    /// Adds an occurrence; names outside the supported identifier set are
    /// skipped (an anchor on such a name observes Missing).
    fn add(&mut self, path: &[String], nodes: Vec<Node<'t>>, name: Option<Node<'t>>) {
        let ok = path.iter().all(|seg| {
            seg == "#private"
                || seg
                    .split('#')
                    .enumerate()
                    .all(|(i, part)| (i == 0 && part.is_empty()) || ident_ok(part))
        });
        if !ok || path.is_empty() {
            return;
        }
        self.table
            .entry(path.join("::"))
            .or_default()
            .push(Occurrence { nodes, name });
    }
    fn is_comment(&self, n: Node) -> bool {
        matches!(
            n.kind(),
            "line_comment" | "block_comment" | "comment" | "html_comment"
        )
    }
    fn directive(&self, n: Node) -> bool {
        self.is_comment(n) && crate::walk_directive(self.rules, self.text(n))
    }
    /// Walks a container's children, collecting attachments that precede a
    /// declaration (attributes, decorators, directive comments).
    pub fn container(&mut self, node: Node<'t>, prefix: &[String]) {
        if self.depth >= MAX_NESTING || self.failed.is_some() {
            return;
        }
        self.depth += 1;
        self.children(node, prefix);
        self.depth -= 1;
    }
    fn children(&mut self, node: Node<'t>, prefix: &[String]) {
        let mut pending: Vec<Node<'t>> = Vec::new();
        let mut cursor = node.walk();
        let children: Vec<Node<'t>> = node.children(&mut cursor).collect();
        for child in children {
            if let Err(e) = self.clock.tick() {
                self.failed = Some(e);
                return;
            }
            match child.kind() {
                "attribute_item" | "decorator" => {
                    pending.push(child);
                    continue;
                }
                k if self.is_comment(child) => {
                    if self.directive(child) {
                        pending.push(child);
                    }
                    let _ = k;
                    continue;
                }
                _ => {}
            }
            let attached = std::mem::take(&mut pending);
            match self.language {
                Language::Rust => self.rust(child, attached, prefix),
                Language::Python => self.python(child, attached, prefix),
                Language::Go => self.go(child, attached, prefix),
                _ => self.script(child, attached, prefix),
            }
        }
    }
    fn with(nodes: &[Node<'t>], node: Node<'t>) -> Vec<Node<'t>> {
        let mut v = nodes.to_vec();
        v.push(node);
        v
    }
    fn path(prefix: &[String], seg: impl Into<String>) -> Vec<String> {
        let mut v = prefix.to_vec();
        v.push(seg.into());
        v
    }

    fn rust_name(&self, n: Node) -> Option<String> {
        let t = self.text(n);
        Some(t.strip_prefix("r#").unwrap_or(t).to_owned())
    }
    /// The last path segment of a type, generics dropped; None if the type is
    /// not a path.
    fn rust_type_name(&self, t: Node) -> Option<String> {
        match t.kind() {
            "type_identifier" | "identifier" => self.rust_name(t),
            "scoped_type_identifier" | "scoped_identifier" => t
                .child_by_field_name("name")
                .and_then(|n| self.rust_name(n)),
            "generic_type" => t
                .child_by_field_name("type")
                .and_then(|n| self.rust_type_name(n)),
            _ => None,
        }
    }
    fn rust(&mut self, n: Node<'t>, attached: Vec<Node<'t>>, prefix: &[String]) {
        let kind = n.kind();
        let named = |s: &mut Self| n.child_by_field_name("name").and_then(|x| s.rust_name(x));
        match kind {
            "function_item"
            | "function_signature_item"
            | "struct_item"
            | "enum_item"
            | "union_item"
            | "type_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
            | "associated_type" => {
                if let Some(name) = named(self) {
                    self.add(
                        &Self::path(prefix, name),
                        Self::with(&attached, n),
                        n.child_by_field_name("name"),
                    );
                }
            }
            "trait_item" | "mod_item" => {
                if let Some(name) = named(self) {
                    let path = Self::path(prefix, name);
                    self.add(
                        &path,
                        Self::with(&attached, n),
                        n.child_by_field_name("name"),
                    );
                    if let Some(body) = n.child_by_field_name("body") {
                        self.container(body, &path);
                    }
                }
            }
            "impl_item" => {
                let Some(ty) = n.child_by_field_name("type") else {
                    return;
                };
                let mut seg = match self.rust_type_name(ty) {
                    Some(name) => name,
                    None => format!("#{}", key_hash(self.text(ty))),
                };
                if let Some(tr) = n.child_by_field_name("trait") {
                    let (base, args) = match tr.kind() {
                        "generic_type" => (
                            tr.child_by_field_name("type"),
                            tr.child_by_field_name("type_arguments"),
                        ),
                        _ => (Some(tr), None),
                    };
                    let Some(name) = base.and_then(|b| self.rust_type_name(b)) else {
                        return;
                    };
                    seg.push('#');
                    seg.push_str(&name);
                    if let Some(args) = args {
                        seg.push('#');
                        seg.push_str(&key_hash(self.text(args)));
                    }
                }
                if let Some(body) = n.child_by_field_name("body") {
                    self.container(body, &Self::path(prefix, seg));
                }
            }
            "foreign_mod_item" => {
                if let Some(body) = n.child_by_field_name("body") {
                    self.container(body, prefix);
                }
            }
            _ => {}
        }
    }

    fn python(&mut self, n: Node<'t>, attached: Vec<Node<'t>>, prefix: &[String]) {
        let (decl, span) = match n.kind() {
            "decorated_definition" => match n.child_by_field_name("definition") {
                Some(d) => (d, n),
                None => return,
            },
            "function_definition" | "class_definition" => (n, n),
            "if_statement" | "try_statement" | "with_statement" | "elif_clause" | "else_clause"
            | "except_clause" | "finally_clause" => {
                // Conditional definitions: the block adds no segment.
                let mut c = n.walk();
                let parts: Vec<Node<'t>> = n.children(&mut c).collect();
                for part in parts {
                    if part.kind() == "block" {
                        self.container(part, prefix);
                    } else {
                        self.python(part, Vec::new(), prefix);
                    }
                }
                return;
            }
            _ => return,
        };
        let Some(name_node) = decl.child_by_field_name("name") else {
            return;
        };
        let path = Self::path(prefix, self.text(name_node));
        self.add(&path, Self::with(&attached, span), Some(name_node));
        if decl.kind() == "class_definition"
            && let Some(body) = decl.child_by_field_name("body")
        {
            self.container(body, &path);
        }
    }

    fn go_receiver(&self, n: Node) -> Option<String> {
        let params = n.child_by_field_name("receiver")?;
        let decl = params.named_child(0)?;
        let mut t = decl.child_by_field_name("type")?;
        loop {
            match t.kind() {
                "pointer_type" => t = t.named_child(0)?,
                "generic_type" => t = t.child_by_field_name("type")?,
                "type_identifier" => return Some(self.text(t).to_owned()),
                _ => return None,
            }
        }
    }
    fn go(&mut self, n: Node<'t>, attached: Vec<Node<'t>>, prefix: &[String]) {
        match n.kind() {
            "function_declaration" => {
                if let Some(name) = n.child_by_field_name("name") {
                    self.add(
                        &Self::path(prefix, self.text(name)),
                        Self::with(&attached, n),
                        Some(name),
                    );
                }
            }
            "method_declaration" => {
                if let (Some(recv), Some(name)) =
                    (self.go_receiver(n), n.child_by_field_name("name"))
                {
                    let path = Self::path(&Self::path(prefix, recv), self.text(name));
                    self.add(&path, Self::with(&attached, n), Some(name));
                }
            }
            "type_declaration" | "const_declaration" | "var_declaration" => {
                let mut c = n.walk();
                let specs: Vec<Node<'t>> = n
                    .named_children(&mut c)
                    .filter(|s| s.kind().ends_with("_spec") || s.kind() == "type_alias")
                    .collect();
                // Implicit iota repetition: every spec's span is the group.
                let whole = n.kind() == "const_declaration"
                    && specs
                        .iter()
                        .any(|s| s.child_by_field_name("value").is_none());
                for spec in specs {
                    let mut c = spec.walk();
                    let names: Vec<Node<'t>> =
                        spec.children_by_field_name("name", &mut c).collect();
                    for name in names {
                        let span = if whole { n } else { spec };
                        self.add(
                            &Self::path(prefix, self.text(name)),
                            Self::with(&attached, span),
                            Some(name),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// TypeScript, TSX and JavaScript.
    fn script(&mut self, n: Node<'t>, attached: Vec<Node<'t>>, prefix: &[String]) {
        match n.kind() {
            "export_statement" => {
                if let Some(decl) = n.child_by_field_name("declaration") {
                    let default = (0..n.child_count())
                        .filter_map(|i| n.child(i))
                        .any(|c| c.kind() == "default");
                    if default && decl.child_by_field_name("name").is_none() {
                        self.add(
                            &Self::path(prefix, "default"),
                            Self::with(&attached, n),
                            None,
                        );
                    } else {
                        self.declaration(decl, Self::with(&attached, n), prefix);
                    }
                } else if n.child_by_field_name("value").is_some() {
                    self.add(
                        &Self::path(prefix, "default"),
                        Self::with(&attached, n),
                        None,
                    );
                }
            }
            "expression_statement" | "ambient_declaration" => {
                if let Some(inner) = n.named_child(0)
                    && matches!(inner.kind(), "internal_module" | "module")
                {
                    self.declaration(inner, Self::with(&attached, n), prefix);
                } else if n.kind() == "ambient_declaration"
                    && let Some(inner) = n.named_child(0)
                {
                    self.declaration(inner, Self::with(&attached, n), prefix);
                }
            }
            _ => self.declaration(n, Self::with(&attached, n), prefix),
        }
    }
    /// A declaration `d` whose span is `span` (the declaration itself, or a
    /// wrapper such as `export`, plus attachments).
    fn declaration(&mut self, d: Node<'t>, span: Vec<Node<'t>>, prefix: &[String]) {
        let name = d.child_by_field_name("name");
        match d.kind() {
            "function_declaration"
            | "generator_function_declaration"
            | "function_signature"
            | "interface_declaration"
            | "type_alias_declaration"
            | "enum_declaration" => {
                if let Some(name) = name {
                    self.add(&Self::path(prefix, self.text(name)), span, Some(name));
                }
            }
            "class_declaration" | "abstract_class_declaration" | "class" => {
                if let Some(name) = name {
                    let path = Self::path(prefix, self.text(name));
                    self.add(&path, span, Some(name));
                    if let Some(body) = d.child_by_field_name("body") {
                        self.container(body, &path);
                    }
                }
            }
            "internal_module" | "module" => {
                if let Some(name) = name
                    && name.kind() == "identifier"
                {
                    let path = Self::path(prefix, self.text(name));
                    self.add(&path, span, Some(name));
                    if let Some(body) = d.child_by_field_name("body") {
                        self.container(body, &path);
                    }
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut c = d.walk();
                let declarators: Vec<Node<'t>> = d
                    .named_children(&mut c)
                    .filter(|x| x.kind() == "variable_declarator")
                    .collect();
                let single = declarators.len() == 1;
                for v in declarators {
                    if let Some(name) = v.child_by_field_name("name")
                        && name.kind() == "identifier"
                    {
                        let nodes = if single {
                            span.clone()
                        } else {
                            let mut s: Vec<Node<'t>> = span[..span.len() - 1].to_vec();
                            s.push(v);
                            s
                        };
                        self.add(&Self::path(prefix, self.text(name)), nodes, Some(name));
                    }
                }
            }
            "method_definition"
            | "method_signature"
            | "abstract_method_signature"
            | "public_field_definition"
            | "field_definition" => {
                let Some(name) = name.or_else(|| d.child_by_field_name("property")) else {
                    return;
                };
                match name.kind() {
                    "property_identifier" | "identifier" => {
                        self.add(&Self::path(prefix, self.text(name)), span, Some(name));
                    }
                    "private_property_identifier" => {
                        let bare = self.text(name).trim_start_matches('#').to_owned();
                        let path = Self::path(&Self::path(prefix, "#private"), bare);
                        self.add(&path, span, Some(name));
                    }
                    _ => {} // computed or string-named members are not symbols
                }
            }
            _ => {}
        }
    }
}
