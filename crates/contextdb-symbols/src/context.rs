//! Same-file context of a symbol (ADR 0012 r0.7, G6 v2): the file's other
//! items that bind a name the symbol's span mentions. A symbol's context
//! hash changes when such an item's normalised syntax changes, appears or
//! disappears. Items are the file's other symbols (by their last path
//! segment) and its top-level non-symbol binders: imports (Rust `use`,
//! Python, Go, TS/JS), each a name bound to its source path, and Python
//! module-level assignments, including those in top-level conditional
//! blocks. Items that
//! overlap the symbol (itself, its containers, its members) never count.
use crate::{Unavailable, walk::Clock};
use contextdb_anchors::Language;
use std::{collections::BTreeSet, ops::Range};
use tree_sitter::Node;

/// A binder of names with its byte ranges and normalised hash.
pub(crate) struct Item {
    pub names: Vec<String>,
    pub ranges: Vec<Range<usize>>,
    pub hash: [u8; 32],
}

fn text<'a>(src: &'a [u8], n: Node) -> &'a str {
    std::str::from_utf8(&src[n.byte_range()]).unwrap_or("")
}
fn field<'a>(src: &'a [u8], n: Node, name: &str) -> Option<&'a str> {
    n.child_by_field_name(name).map(|c| text(src, c))
}
fn children(n: Node) -> Vec<Node> {
    n.children(&mut n.walk()).collect()
}

/// What a top-level statement binds: a name to an import source (formatting
/// and grouping of imports never matter), or a name to the statement's own
/// syntax (a module-level assignment).
pub(crate) enum Binding {
    Import(String),
    Statement,
}

fn squeeze(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}
/// Names a Rust use tree binds, each with its full path.
fn rust_use(src: &[u8], n: Node, prefix: &str, out: &mut Vec<(String, Binding)>) {
    let join = |p: &str| {
        if prefix.is_empty() {
            squeeze(p)
        } else {
            format!("{prefix}::{}", squeeze(p))
        }
    };
    match n.kind() {
        "use_as_clause" => {
            if let (Some(alias), Some(path)) = (field(src, n, "alias"), field(src, n, "path")) {
                out.push((alias.to_owned(), Binding::Import(join(path))));
            }
        }
        "scoped_identifier" => {
            if let Some(name) = field(src, n, "name") {
                out.push((name.to_owned(), Binding::Import(join(text(src, n)))));
            }
        }
        "identifier" => out.push((text(src, n).to_owned(), Binding::Import(join(text(src, n))))),
        "scoped_use_list" => {
            let inner = field(src, n, "path").map_or_else(|| prefix.to_owned(), &join);
            if let Some(list) = n.child_by_field_name("list") {
                rust_use(src, list, &inner, out);
            }
        }
        "use_list" => {
            for c in children(n) {
                rust_use(src, c, prefix, out);
            }
        }
        _ => {}
    }
}
/// Identifiers on the left of a Python assignment.
fn python_targets(src: &[u8], n: Node, out: &mut Vec<(String, Binding)>) {
    if n.kind() == "identifier" {
        out.push((text(src, n).to_owned(), Binding::Statement));
        return;
    }
    if matches!(n.kind(), "pattern_list" | "tuple_pattern" | "list_pattern") {
        for c in children(n) {
            python_targets(src, c, out);
        }
    }
}

/// The names a top-level non-symbol statement binds (empty if none).
pub(crate) fn binders(language: Language, src: &[u8], n: Node) -> Vec<(String, Binding)> {
    let mut out = Vec::new();
    match (language, n.kind()) {
        (Language::Rust, "use_declaration") => {
            if let Some(arg) = n.child_by_field_name("argument") {
                rust_use(src, arg, "", &mut out);
            }
        }
        (Language::Python, "import_statement") => {
            for c in children(n) {
                match c.kind() {
                    // `import a.b` binds `a` to module `a`.
                    "dotted_name" => {
                        if let Some(first) = c.named_child(0) {
                            let name = text(src, first).to_owned();
                            out.push((name.clone(), Binding::Import(name)));
                        }
                    }
                    "aliased_import" => {
                        if let (Some(alias), Some(module)) =
                            (field(src, c, "alias"), field(src, c, "name"))
                        {
                            out.push((alias.to_owned(), Binding::Import(squeeze(module))));
                        }
                    }
                    _ => {}
                }
            }
        }
        (Language::Python, "import_from_statement") => {
            let module_node = n.child_by_field_name("module_name");
            let module = module_node.map_or(String::new(), |m| squeeze(text(src, m)));
            for c in children(n) {
                if Some(c.id()) == module_node.map(|m| m.id()) {
                    continue;
                }
                match c.kind() {
                    "dotted_name" => {
                        let name = squeeze(text(src, c));
                        out.push((name.clone(), Binding::Import(format!("{module}.{name}"))));
                    }
                    "aliased_import" => {
                        if let (Some(alias), Some(name)) =
                            (field(src, c, "alias"), field(src, c, "name"))
                        {
                            out.push((
                                alias.to_owned(),
                                Binding::Import(format!("{module}.{}", squeeze(name))),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        (Language::Python, "expression_statement") => {
            for c in children(n) {
                if matches!(c.kind(), "assignment" | "augmented_assignment")
                    && let Some(left) = c.child_by_field_name("left")
                {
                    python_targets(src, left, &mut out);
                }
            }
        }
        // Module-level conditional blocks (compatibility imports), as the
        // extraction descends them for definitions.
        (
            Language::Python,
            "if_statement" | "try_statement" | "with_statement" | "elif_clause" | "else_clause"
            | "except_clause" | "finally_clause" | "block",
        ) => {
            for c in children(n) {
                out.extend(binders(language, src, c));
            }
        }
        (Language::Go, "import_declaration") => {
            let mut specs = Vec::new();
            for c in children(n) {
                match c.kind() {
                    "import_spec" => specs.push(c),
                    "import_spec_list" => specs.extend(
                        children(c)
                            .into_iter()
                            .filter(|s| s.kind() == "import_spec"),
                    ),
                    _ => {}
                }
            }
            for s in specs {
                let Some(path) = field(src, s, "path").map(|p| p.trim_matches('"').to_owned())
                else {
                    continue;
                };
                let name = field(src, s, "name")
                    .map(str::to_owned)
                    .or_else(|| path.rsplit('/').next().map(str::to_owned));
                out.extend(name.map(|n| (n, Binding::Import(path.clone()))));
            }
        }
        (Language::TypeScript | Language::Tsx | Language::JavaScript, "import_statement") => {
            let source = field(src, n, "source")
                .map(|s| s.trim_matches(['"', '\'', '`']).to_owned())
                .unwrap_or_default();
            let mut stack: Vec<Node> = children(n);
            while let Some(c) = stack.pop() {
                match c.kind() {
                    "import_clause" | "named_imports" => stack.extend(children(c)),
                    // A default import.
                    "identifier" => out.push((
                        text(src, c).to_owned(),
                        Binding::Import(format!("{source}:default")),
                    )),
                    "namespace_import" => out.extend(
                        children(c)
                            .into_iter()
                            .filter(|i| i.kind() == "identifier")
                            .map(|i| {
                                (
                                    text(src, i).to_owned(),
                                    Binding::Import(format!("{source}:*")),
                                )
                            }),
                    ),
                    "import_specifier" => {
                        if let Some(name) = field(src, c, "name") {
                            let local = field(src, c, "alias").unwrap_or(name);
                            out.push((
                                local.to_owned(),
                                Binding::Import(format!("{source}:{}", squeeze(name))),
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out.retain(|(s, _)| !s.is_empty());
    out
}

/// Identifier-like leaf texts under `nodes`, except the leaf `skip`.
pub(crate) fn mentions(src: &[u8], nodes: &[Node], skip: Option<usize>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack: Vec<Node> = nodes.to_vec();
    let mut cursor = nodes.first().map(|n| n.walk());
    while let Some(n) = stack.pop() {
        if n.child_count() == 0 {
            if n.kind().contains("identifier") && Some(n.id()) != skip {
                out.insert(text(src, n).to_owned());
            }
            continue;
        }
        if let Some(c) = cursor.as_mut() {
            stack.extend(n.children(c));
        }
    }
    out
}

/// The file's items by bound name.
pub(crate) struct Index<'a> {
    by_name: std::collections::HashMap<&'a str, Vec<&'a Item>>,
}
impl<'a> Index<'a> {
    pub fn new(items: &'a [Item]) -> Self {
        let mut by_name: std::collections::HashMap<&str, Vec<&Item>> = Default::default();
        for item in items {
            for name in &item.names {
                by_name.entry(name.as_str()).or_default().push(item);
            }
        }
        Index { by_name }
    }
    /// The context hash of a symbol whose (sorted, disjoint) `ranges` mention
    /// `names`: linear in the mentions and their items' ranges.
    pub fn hash(
        &self,
        ranges: &[Range<usize>],
        names: &BTreeSet<String>,
        clock: &mut Clock,
    ) -> Result<[u8; 32], Unavailable> {
        let overlaps = |r: &Range<usize>| {
            let i = ranges.partition_point(|s| s.end <= r.start);
            ranges.get(i).is_some_and(|s| s.start < r.end)
        };
        let mut found: BTreeSet<(&str, [u8; 32])> = BTreeSet::new();
        for name in names {
            for item in self.by_name.get(name.as_str()).into_iter().flatten() {
                clock.tick()?;
                let mut clear = true;
                for r in &item.ranges {
                    clock.tick()?;
                    if overlaps(r) {
                        clear = false;
                        break;
                    }
                }
                if clear {
                    found.insert((name, item.hash));
                }
            }
        }
        let mut h = blake3::Hasher::new();
        for (name, item) in found {
            h.update(&(name.len() as u64).to_le_bytes());
            h.update(name.as_bytes());
            h.update(&item);
        }
        Ok(*h.finalize().as_bytes())
    }
}
