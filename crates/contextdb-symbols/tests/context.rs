//! Walk 3 same-file context goldens (ADR 0012 §4 r0.7): which items a
//! symbol's revision covers, per language and import form.
use contextdb_anchors::Language;
use contextdb_symbols::parse;
use std::time::{Duration, Instant};

fn hash(language: Language, src: &str, symbol: &str) -> [u8; 32] {
    let f = parse(
        language,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    f.symbols
        .get(symbol)
        .unwrap_or_else(|| panic!("{symbol} not in {:?}", f.symbols.keys().collect::<Vec<_>>()))
        .hash
}
fn same(language: Language, symbol: &str, a: &str, b: &str) {
    assert_eq!(
        hash(language, a, symbol),
        hash(language, b, symbol),
        "should match:\n{a}\n---\n{b}"
    );
}
fn differ(language: Language, symbol: &str, a: &str, b: &str) {
    assert_ne!(
        hash(language, a, symbol),
        hash(language, b, symbol),
        "should differ:\n{a}\n---\n{b}"
    );
}

#[test]
fn rust_use_binds_names_to_paths() {
    use Language::Rust;
    let f = "\nfn f() -> u8 { B + X + D }\n";
    // A path change behind a used name re-revisions; order, grouping and
    // whitespace inside the path do not.
    differ(
        Rust,
        "f",
        &format!("use a::B;{f}"),
        &format!("use c::B;{f}"),
    );
    same(
        Rust,
        "f",
        &format!("use a::B;\nuse c::D;{f}"),
        &format!("use c::D;\nuse a::B;{f}"),
    );
    same(
        Rust,
        "f",
        &format!("use a::{{B, D}};{f}"),
        &format!("use a::B;\nuse a::D;{f}"),
    );
    same(
        Rust,
        "f",
        &format!("use a :: B;{f}"),
        &format!("use a::B;{f}"),
    );
    same(
        Rust,
        "f",
        &format!("use a::{{b::{{B}}}};{f}"),
        &format!("use a::b::B;{f}"),
    );
    differ(
        Rust,
        "f",
        &format!("use a::{{b::{{B}}}};{f}"),
        &format!("use a::c::B;{f}"),
    );
    // Aliases bind the alias to the original path.
    differ(
        Rust,
        "f",
        &format!("use a::B as X;{f}"),
        &format!("use a::C as X;{f}"),
    );
    // A bare identifier import, and its appearance.
    differ(Rust, "f", &format!("use B;{f}"), &format!("use z::B;{f}"));
    differ(Rust, "f", f, &format!("use a::B;{f}"));
    // An import the symbol does not name changes nothing.
    same(
        Rust,
        "f",
        &format!("use a::Q;{f}"),
        &format!("use c::Q;{f}"),
    );
}

#[test]
fn python_imports_and_assignments() {
    use Language::Python;
    let f = "\ndef f():\n    return os.sep + b + c + J + X\n";
    differ(
        Python,
        "f",
        &format!("import os{f}"),
        &format!("import posix as os{f}"),
    );
    differ(Python, "f", f, &format!("import os.path{f}"));
    // An aliased module import binds the alias to the module.
    differ(
        Python,
        "f",
        &format!("import posix as os{f}"),
        &format!("import nt as os{f}"),
    );
    // `import a.b` binds `a` but imports `a.b` (walk 4): `import a.c`
    // instead breaks `a.b.x`.
    differ(
        Python,
        "f",
        &format!("import os.path{f}"),
        &format!("import os{f}"),
    );
    differ(
        Python,
        "f",
        &format!("import os.path{f}"),
        &format!("import os.sep{f}"),
    );
    differ(
        Python,
        "f",
        &format!("from a import b{f}"),
        &format!("from z import b{f}"),
    );
    differ(
        Python,
        "f",
        &format!("from a import d as c{f}"),
        &format!("from a import e as c{f}"),
    );
    same(
        Python,
        "f",
        &format!("from a import b, c{f}"),
        &format!("from a import c, b{f}"),
    );
    same(
        Python,
        "f",
        &format!("from a import (b,\n    c){f}"),
        &format!("from a import b, c{f}"),
    );
    // The module name itself binds nothing: `f` never names `a` here.
    let g = "\ndef g():\n    return a\n";
    same(
        Python,
        "g",
        &format!("from a import q{g}"),
        &format!("from z import q{g}"),
    );
    // Compatibility imports in top-level conditional blocks.
    differ(
        Python,
        "f",
        &format!("try:\n    import json as J\nexcept ImportError:\n    J = None{f}"),
        &format!("try:\n    import simplejson as J\nexcept ImportError:\n    J = None{f}"),
    );
    // Module-level assignments by value; tuple targets too.
    differ(Python, "f", &format!("X = 1{f}"), &format!("X = 2{f}"));
    differ(
        Python,
        "f",
        &format!("X, Y = 1, 2{f}"),
        &format!("X, Y = 3, 2{f}"),
    );
    same(Python, "f", &format!("Q = 1{f}"), &format!("Q = 2{f}"));
}

#[test]
fn go_imports() {
    use Language::Go;
    let f = "\nfunc f() { fmt.Println(); strings.Repeat() }\n";
    differ(
        Go,
        "f",
        &format!("package p\nimport \"fmt\"{f}"),
        &format!("package p\nimport fmt \"example.com/fmt\"{f}"),
    );
    same(
        Go,
        "f",
        &format!("package p\nimport (\n\t\"fmt\"\n\t\"strings\"\n){f}"),
        &format!("package p\nimport (\n\t\"strings\"\n\t\"fmt\"\n){f}"),
    );
    differ(
        Go,
        "f",
        &format!("package p\nimport (\n\t\"fmt\"\n\t\"strings\"\n){f}"),
        &format!("package p\nimport (\n\t\"fmt\"\n\t\"example.com/strings\"\n){f}"),
    );
    same(
        Go,
        "f",
        &format!("package p\nimport (\n\t\"os\"\n){f}"),
        &format!("package p\nimport (\n\tos \"example.com/os\"\n){f}"),
    );
}

#[test]
fn script_imports() {
    for language in [Language::TypeScript, Language::JavaScript] {
        let f = "\nfunction f() { return d + ns.x + a + b; }\n";
        differ(
            language,
            "f",
            &format!("import d from \"m\";{f}"),
            &format!("import d from \"n\";{f}"),
        );
        differ(
            language,
            "f",
            &format!("import * as ns from \"m\";{f}"),
            &format!("import * as ns from \"n\";{f}"),
        );
        differ(
            language,
            "f",
            &format!("import {{ a }} from \"m\";{f}"),
            &format!("import {{ a }} from \"n\";{f}"),
        );
        differ(
            language,
            "f",
            &format!("import {{ x as b }} from \"m\";{f}"),
            &format!("import {{ y as b }} from \"m\";{f}"),
        );
        same(
            language,
            "f",
            &format!("import {{ a, x as b }} from 'm';{f}"),
            &format!("import {{ x as b, a }} from \"m\";{f}"),
        );
    }
}

/// Containers, members and the symbol itself never count; an item that
/// merely touches the span (no whitespace between) still does.
#[test]
fn overlapping_items_never_count() {
    use Language::{Python, Rust};
    let class = |n: &str| {
        format!(
            "class C:\n    def m(self):\n        return C\n    def n(self):\n        return {n}\n"
        )
    };
    same(Python, "C::m", &class("1"), &class("2"));
    differ(Python, "C::n", &class("1"), &class("2"));
    differ(
        Rust,
        "f",
        "const A: u8 = 1;fn f() -> u8 { A }",
        "const A: u8 = 2;fn f() -> u8 { A }",
    );
    differ(
        Rust,
        "f",
        "fn f() -> u8 { A }const A: u8 = 1;",
        "fn f() -> u8 { A }const A: u8 = 2;",
    );
    differ(
        Rust,
        "f",
        "fn f() -> u8 { A + B }const A: u8 = 1;const B: u8 = 1;",
        "fn f() -> u8 { A + B }const A: u8 = 1;const B: u8 = 2;",
    );
}

/// Only identifier leaves are mentions, and never the declared name.
#[test]
fn mentions_are_identifiers() {
    use Language::{Python, Rust};
    // The string "B" is not a mention of `B`.
    same(
        Python,
        "f",
        "B = 1\ndef f():\n    return \"B\"\n",
        "B = 2\ndef f():\n    return \"B\"\n",
    );
    // A recursive call names the symbol itself: not context.
    same(
        Rust,
        "f",
        "fn f(n: u8) -> u8 { if n == 0 { 0 } else { f(n - 1) } }\nfn g() {}",
        "fn f(n: u8) -> u8 { if n == 0 { 0 } else { f(n - 1) } }\nfn g() { }",
    );
    // A type, field or package identifier is a mention.
    differ(
        Rust,
        "f",
        "struct S;\nfn f() -> S { S }",
        "struct S(u8);\nfn f() -> S { S }",
    );
}

/// A one-element tuple's comma keeps its meaning with or without comments
/// inside the brackets (element counting skips comments).
#[test]
fn one_element_tuple_with_comments() {
    use Language::{Python, Rust};
    differ(
        Python,
        "f",
        "def f():\n    return (1,)\n",
        "def f():\n    return (1)\n",
    );
    differ(
        Python,
        "f",
        "def f():\n    return (1,  # one\n    )\n",
        "def f():\n    return (1  # one\n    )\n",
    );
    same(
        Python,
        "f",
        "def f():\n    return (1, 2,  # two\n    )\n",
        "def f():\n    return (1, 2  # two\n    )\n",
    );
    differ(
        Rust,
        "f",
        "fn f() -> (u8,) { (1,) }",
        "fn f() -> (u8) { (1) }",
    );
    differ(
        Rust,
        "f",
        "fn f() -> (u8, /* c */) { (1,) }",
        "fn f() -> (u8 /* c */) { (1,) }",
    );
}
