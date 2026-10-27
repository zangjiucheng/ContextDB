//! Walk 4 (ADR 0012 r0.9): the independent implementation review's
//! counterexamples, each a must-differ (or must-find) golden.
use contextdb_anchors::Language::{self, *};
use contextdb_symbols::{ParsedFile, parse};
use std::time::{Duration, Instant};

fn file(language: Language, src: &str) -> ParsedFile {
    parse(
        language,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(10),
    )
    .unwrap()
}
fn symbol(language: Language, src: &str, name: &str) -> contextdb_symbols::Symbol {
    let f = file(language, src);
    f.symbols
        .get(name)
        .unwrap_or_else(|| panic!("{name} not in {:?}", f.symbols.keys().collect::<Vec<_>>()))
        .clone()
}
fn differ(language: Language, name: &str, a: &str, b: &str) {
    assert_ne!(
        symbol(language, a, name).hash,
        symbol(language, b, name).hash,
        "should differ:\n{a}\n---\n{b}"
    );
}
fn same(language: Language, name: &str, a: &str, b: &str) {
    assert_eq!(
        symbol(language, a, name).hash,
        symbol(language, b, name).hash,
        "should match:\n{a}\n---\n{b}"
    );
}

/// B1: text between a node's children that no child covers.
#[test]
fn uncovered_text_is_hashed() {
    let py = |body: &str| format!("def f():\n    return {body}\n");
    for (a, b) in [
        (r#""hello\n""#, r#""bye\n""#),
        (r#""  a\n""#, r#""a\n""#),
        (r#"f"Hi {n}!\n""#, r#"f"Hi {n}?\n""#),
        (r#"f"Hi {{ {n}""#, r#"f"Yo {{ {n}""#),
        (r#"f"{x:>10}""#, r#"f"{x:<99}""#),
        (r#"b"\x00abc""#, r#"b"\x00xyz""#),
    ] {
        differ(Python, "f", &py(a), &py(b));
    }
    differ(
        Python,
        "f",
        "def f():\n    \"\"\"Tab\\there.\"\"\"\n",
        "def f():\n    \"\"\"Tab\\tthere.\"\"\"\n",
    );
    // Module-level strings feed context too.
    let g = "\ndef g():\n    return Q\n";
    differ(
        Python,
        "g",
        &format!("Q = \"DELETE x\\n\"{g}"),
        &format!("Q = \"SELECT x\\n\"{g}"),
    );
    let rs = |lit: &str| format!("fn f() -> &'static str {{ g({lit}) }}");
    for (a, b) in [
        (r#"r"   x""#, r#"r"x""#),
        (r#"r"x""#, r#"cr"x""#),
        (r##"r#"\n\nx"#"##, r#"r"x""#),
        (r#"br"abc""#, r#"r"abc""#),
    ] {
        differ(Rust, "f", &rs(a), &rs(b));
    }
    differ(
        Rust,
        "m",
        "macro_rules! m { ($($x:expr),*) => {} }",
        "macro_rules! m { ($($x:expr);*) => {} }",
    );
    // Layout between tokens stays neutral.
    same(
        Python,
        "f",
        &py(r#"f"{x:>10}""#),
        "def f():\n    return  f\"{x:>10}\"\n",
    );
    same(
        Rust,
        "m",
        "macro_rules! m { ($($x:expr),*) => {} }",
        "macro_rules! m {\n    ($($x:expr), *) => {}\n}",
    );
}

/// M2: a context item with a parse error makes the symbol erroneous.
#[test]
fn context_errors_are_symbol_errors() {
    let clean = "fn helper() -> u8 { 1 }\nfn a() -> u8 { helper() }\n";
    let broken = "fn helper() -> u8 { 1 + }\nfn a() -> u8 { helper() }\n";
    assert!(!symbol(Rust, clean, "a").error);
    assert!(symbol(Rust, broken, "a").error);
    // A symbol that does not use the broken item stays clean.
    let other = "fn helper() -> u8 { 1 + }\nfn b() -> u8 { 2 }\n";
    assert!(!symbol(Rust, other, "b").error);
    // Errors stay local: a name inside an ERROR within another symbol's span
    // does not spread.
    let local = "fn a(x: u8) -> u8 { x }\nfn b() { let y = x +; }\n";
    assert!(!symbol(Rust, local, "a").error);
    let local = "def a(x):\n    return x\ndef b():\n    y = x +\n";
    assert!(!symbol(Python, local, "a").error);
    // A helper too broken to be extracted: its name inside an ERROR node.
    // The helper is not extracted at all; a user of it is erroneous, a
    // neighbour is not.
    for (language, src) in [
        (
            Python,
            "def a():\n    return helper()\ndef c():\n    return 2\ndef helper()\n    return 1\n",
        ),
        (
            JavaScript,
            "function a() { return helper() }\nfunction c() { return 2 }\nfunction helper( { return 1 }\n",
        ),
    ] {
        let f = file(language, src);
        assert!(!f.symbols.contains_key("helper"), "{src}");
        assert!(f.symbols["a"].error, "{src}");
        assert!(!f.symbols["c"].error, "{src}");
    }
    // Broken top-level binders: a statement with an error, or code the
    // parser could not place. Users are erroneous; others are not.
    for (language, src) in [
        (
            Python,
            "X = foo(1 +)\ndef f():\n    return X\ndef g():\n    return 1\n",
        ),
        (
            Python,
            "import a.\ndef f():\n    return a\ndef g():\n    return 1\n",
        ),
        (
            Python,
            "if c:\n    X = 1 +\ndef f():\n    return X\ndef g():\n    return 1\n",
        ),
        (
            Rust,
            "use a::{B,;\nfn f() -> u8 { B }\nfn g() -> u8 { 1 }\n",
        ),
    ] {
        let f = file(language, src);
        assert!(f.symbols["f"].error, "{src}");
        assert!(!f.symbols["g"].error, "{src}");
    }
}

/// M3: Python statements in module-level conditional blocks are items on
/// their own.
#[test]
fn python_block_statements_are_their_own_items() {
    differ(
        Python,
        "f",
        "import sys\nif sys.platform == \"win32\":\n    SEP = \"a\"\n    def f():\n        return SEP\n",
        "import sys\nif sys.platform == \"win32\":\n    SEP = \"b\"\n    def f():\n        return SEP\n",
    );
    same(
        Python,
        "g",
        "try:\n    TIMEOUT = 1\n    def h():\n        return 1\nexcept E:\n    pass\ndef g():\n    return TIMEOUT\n",
        "try:\n    TIMEOUT = 1\n    def h():\n        return 2\nexcept E:\n    pass\ndef g():\n    return TIMEOUT\n",
    );
}

/// M4: a Go `for` clause's init and post statements.
#[test]
fn go_for_clause_positions() {
    let f = |clause: &str| format!("package p\nfunc f() {{ for {clause} {{}} }}\n");
    differ(Go, "f", &f("g(); ;"), &f("; ; g()"));
    differ(Go, "f", &f("x = 0; ;"), &f("; ; x = 0"));
    same(Go, "f", &f("i := 0; i < n; i++"), &f("i := 0; i < n; i++ "));
}

/// M5: grouped Go `var` declarations.
#[test]
fn go_grouped_vars_are_symbols() {
    let f = file(Go, "package p\nvar (\n\tA = 1\n\tB = 2\n)\n");
    assert!(f.symbols.contains_key("A") && f.symbols.contains_key("B"));
    let f = file(Go, "package p\nvar (\n\tA = 1\n)\n");
    assert!(f.symbols.contains_key("A"));
    differ(
        Go,
        "g",
        "package p\nvar (\n\tA = 1\n)\nfunc g() int { return A }\n",
        "package p\nvar (\n\tA = 2\n)\nfunc g() int { return A }\n",
    );
}

/// M6: deeply nested use trees and Python targets do not overflow.
#[test]
fn deep_context_binders_do_not_overflow() {
    let depth = 5_000;
    let src = format!(
        "use {}b{};\nfn f() -> u8 {{ b }}\n",
        "a::{".repeat(depth),
        "}".repeat(depth)
    );
    let parsed = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            parse(
                Rust,
                src.as_bytes(),
                Instant::now() + Duration::from_secs(60),
            )
            .map(|f| f.symbols.contains_key("f"))
        })
        .unwrap()
        .join()
        .expect("no stack overflow");
    assert!(parsed.unwrap());
    let src = format!(
        "{}x{} = 1\ndef f():\n    return x\n",
        "(".repeat(2_000),
        ",)".repeat(2_000)
    );
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            parse(
                Python,
                src.as_bytes(),
                Instant::now() + Duration::from_secs(60),
            )
        })
        .unwrap()
        .join()
        .expect("no stack overflow")
        .ok();
}

/// Minor findings closed in walk 4.
#[test]
fn script_forms() {
    // Array holes.
    differ(JavaScript, "X", "const X = [,];", "const X = [];");
    differ(JavaScript, "X", "const X = [a,,];", "const X = [a,];");
    same(JavaScript, "X", "const X = [a,];", "const X = [a];");
    // Several declarators keep the declaration's keywords.
    differ(
        TypeScript,
        "a",
        "export const a=1,b=2;",
        "export let a=1,b=2;",
    );
    differ(TypeScript, "a", "export const a=1,b=2;", "const a=1,b=2;");
    differ(
        TypeScript,
        "a",
        "export declare const a: 1, b: 2;",
        "export const a: 1, b: 2;",
    );
    // `export declare …` and dotted namespaces are symbols.
    let f = file(
        TypeScript,
        "export declare function g(): void;\nexport declare const c: number;\nnamespace A.B { export function h() {} }\n",
    );
    for s in ["g", "c", "A::B", "A::B::h"] {
        assert!(
            f.symbols.contains_key(s),
            "{s}: {:?}",
            f.symbols.keys().collect::<Vec<_>>()
        );
    }
}

/// Context names spelled differently from the items they name.
#[test]
fn context_name_spellings() {
    differ(
        TypeScript,
        "C::m",
        "class C { #p = 1; m() { return this.#p; } }",
        "class C { #p = 2; m() { return this.#p; } }",
    );
    differ(
        Rust,
        "f",
        "fn r#type() -> u8 { 1 }\nfn f() -> u8 { r#type() }",
        "fn r#type() -> u8 { 2 }\nfn f() -> u8 { r#type() }",
    );
    let g = "\nfunc f() { yaml.X(); mod.Y() }\n";
    differ(
        Go,
        "f",
        &format!("package p\nimport \"gopkg.in/yaml.v3\"{g}"),
        &format!("package p\nimport \"gopkg.in/yaml.v2\"{g}"),
    );
    differ(
        Go,
        "f",
        &format!("package p\nimport \"example.com/mod/v2\"{g}"),
        &format!("package p\nimport \"example.com/mod/v3\"{g}"),
    );
    // Only `v` + digits is a version element.
    let h = "\nfunc f() { vendor.X() }\n";
    differ(
        Go,
        "f",
        &format!("package p\nimport \"a/b/vendor\"{h}"),
        &format!("package p\nimport \"a/c/vendor\"{h}"),
    );
    assert_eq!(
        contextdb_symbols::Unavailable("parse deadline".into()).to_string(),
        "parse deadline"
    );
}

/// The pinned grammar and runtime versions in the normaliser version match
/// the lockfile (ADR 0012 §4: derived mechanically).
#[test]
fn normaliser_versions_match_the_lockfile() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
        .expect("Cargo.lock");
    let locked = |name: &str| -> String {
        let needle = format!("name = \"{name}\"\nversion = \"");
        let at = lock
            .find(&needle)
            .unwrap_or_else(|| panic!("{name} not locked"));
        let rest = &lock[at + needle.len()..];
        rest[..rest.find('"').unwrap()].to_owned()
    };
    for language in [Rust, TypeScript, Tsx, JavaScript, Python, Go] {
        let version = contextdb_symbols::normaliser_version(language);
        for part in version.split(';') {
            let pinned = part.rsplit(':').next().unwrap();
            if let Some((name, v)) = pinned.split_once('=')
                && name.starts_with("tree-sitter")
            {
                assert_eq!(locked(name), v, "{version}");
            }
        }
        assert!(version.ends_with(";walk=4"), "{version}");
    }
}
