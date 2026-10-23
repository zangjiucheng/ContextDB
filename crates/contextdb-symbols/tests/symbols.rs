//! Per-language goldens for ADR 0012 §2 (symbol tables) and §4
//! (normalisation): formatting is hash-neutral, meaning is not.
use contextdb_anchors::Language;
use contextdb_symbols::{ParsedFile, parse};
use std::time::{Duration, Instant};

fn file(language: Language, src: &str) -> ParsedFile {
    parse(
        language,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap()
}
fn names(language: Language, src: &str) -> Vec<String> {
    file(language, src).symbols.into_keys().collect()
}
fn hash(language: Language, src: &str, symbol: &str) -> [u8; 32] {
    let f = file(language, src);
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
fn rust_symbol_table() {
    let src = r#"
#[derive(Debug)]
pub struct S(u8);
impl S { pub fn new() -> Self { S(0) } const C: u8 = 1; }
impl<T> std::fmt::Display for Foo<T> { fn fmt(&self) {} }
impl From<u8> for T { fn from(_: u8) -> T { T } }
impl From<u16> for T { fn from(_: u16) -> T { T } }
impl &Bar { fn by_ref() {} }
trait Tr { type A; fn f(&self); fn g() {} }
mod m { fn r#type() { fn inner() {} } }
macro_rules! mm { () => {} }
"#;
    let n = names(Language::Rust, src);
    for expected in [
        "S",
        "S::new",
        "S::C",
        "Foo#Display::fmt",
        "Tr",
        "Tr::A",
        "Tr::f",
        "Tr::g",
        "m",
        "m::type",
        "mm",
    ] {
        assert!(
            n.contains(&expected.to_owned()),
            "{expected} missing from {n:?}"
        );
    }
    // Distinct trait arguments: distinct symbols (frozen key hashes).
    let froms: Vec<_> = n.iter().filter(|s| s.starts_with("T#From#")).collect();
    assert_eq!(froms.len(), 2, "{n:?}");
    assert!(
        n.iter()
            .any(|s| s.starts_with('#') && s.ends_with("::by_ref")),
        "{n:?}"
    );
    assert!(
        !n.iter().any(|s| s.contains("inner")),
        "nothing inside bodies: {n:?}"
    );
}

#[test]
fn rust_formatting_is_neutral_meaning_is_not() {
    let base = "/// doc\n#[inline]\npub fn f(a: u8, b: u8) -> u8 { a + b }";
    same(
        Language::Rust,
        "f",
        base,
        "/// other doc\n#[inline]\npub fn f(\n    a: u8,\n    b: u8,\n) -> u8 {\n    // note\n    a + b\n}",
    );
    differ(
        Language::Rust,
        "f",
        base,
        "/// doc\n#[inline]\npub fn f(a: u8, b: u8) -> u8 { a - b }",
    );
    differ(
        Language::Rust,
        "f",
        base,
        "/// doc\n#[inline(never)]\npub fn f(a: u8, b: u8) -> u8 { a + b }",
    );
    differ(
        Language::Rust,
        "f",
        base,
        "/// doc\n#[inline]\nfn f(a: u8, b: u8) -> u8 { a + b }",
    );
    // A one-element tuple's comma is meaning, not formatting.
    differ(
        Language::Rust,
        "g",
        "fn g() -> (u8,) { (1,) }",
        "fn g() -> (u8) { (1) }",
    );
    // Cfg duplicates are grouped, not ambiguous.
    let grouped = "#[cfg(unix)]\nfn h() {}\n#[cfg(windows)]\nfn h() {}";
    assert_eq!(names(Language::Rust, grouped), vec!["h".to_owned()]);
    differ(
        Language::Rust,
        "h",
        grouped,
        "#[cfg(unix)]\nfn h() {}\n#[cfg(windows)]\nfn h() { x() }",
    );
}

#[test]
fn python_rules() {
    let base = "@dec\ndef f(a, b):\n    \"\"\"Doc.\"\"\"\n    if a:\n        x = 1\n    return (a) + 'b'\n";
    same(
        Language::Python,
        "f",
        base,
        "@dec\ndef f(\n    a,\n    b,\n):  # comment\n    \"\"\"Doc.\"\"\"\n    if a:\n        x = 1\n    return a + \"b\"\n",
    );
    // Docstrings are runtime-visible strings.
    differ(Language::Python, "f", base, &base.replace("Doc.", "Other."));
    // Moving a statement out of its block changes the tree's shape.
    differ(
        Language::Python,
        "f",
        base,
        "@dec\ndef f(a, b):\n    \"\"\"Doc.\"\"\"\n    if a:\n        pass\n    x = 1\n    return (a) + 'b'\n",
    );
    differ(
        Language::Python,
        "g",
        "def g(a):\n    return a[1,]\n",
        "def g(a):\n    return a[1]\n",
    );
    differ(
        Language::Python,
        "g",
        "def g():\n    return (1,)\n",
        "def g():\n    return (1)\n",
    );
    differ(Language::Python, "f", base, &base.replace("@dec", "@other"));
    // Properties group getter and setter; conditional definitions count.
    let n = names(
        Language::Python,
        "class C:\n    @property\n    def p(self): ...\n    @p.setter\n    def p(self, v): ...\nif X:\n    def k(): ...\nelse:\n    def k(): ...\n",
    );
    assert_eq!(n, vec!["C", "C::p", "k"]);
}

#[test]
fn typescript_rules() {
    let base =
        "export function f(a: number): number;\nexport function f(a: any) { return (a?.b).c; }\n";
    // Overload signature and implementation are one symbol.
    assert_eq!(names(Language::TypeScript, base), vec!["f"]);
    same(
        Language::TypeScript,
        "f",
        base,
        "export function f(a: number): number\nexport function f(\n  a: any,\n) {\n  // why\n  return (a?.b).c\n}\n",
    );
    differ(
        Language::TypeScript,
        "f",
        base,
        "export function f(a: number): number;\nexport function f(a: any) { return a?.b.c; }\n",
    );
    differ(
        Language::TypeScript,
        "f",
        base,
        "export function f(a: number): number;\n// @ts-expect-error\nexport function f(a: any) { return (a?.b).c; }\n",
    );
    same(
        Language::TypeScript,
        "g",
        "function g() { return ('x' + (1 + 2)); }",
        "function g() { return \"x\" + (1 + 2) }",
    );
    // Parentheses group; the tree's shape keeps precedence ('x3' vs 'x12').
    differ(
        Language::TypeScript,
        "g",
        "function g() { return 'x' + (1 + 2); }",
        "function g() { return 'x' + 1 + 2; }",
    );
    let n = names(
        Language::TypeScript,
        "export default class C { @d m(): void {} get x() { return 1 } set x(v) {} static #p = 1; }\nexport const X = memo(() => 1);\ninterface I { a: 1 }\nnamespace N { export type T = 1 }\n",
    );
    for expected in ["C", "C::m", "C::x", "C::#private::p", "X", "I", "N", "N::T"] {
        assert!(
            n.contains(&expected.to_owned()),
            "{expected} missing from {n:?}"
        );
    }
    differ(
        Language::TypeScript,
        "C::m",
        "class C { @d m(): void {} }",
        "class C { @e m(): void {} }",
    );
}

#[test]
fn javascript_jsdoc_counts() {
    differ(
        Language::JavaScript,
        "f",
        "/** @param {number} a */\nfunction f(a) { return a }",
        "/** @param {string} a */\nfunction f(a) { return a }",
    );
    same(
        Language::JavaScript,
        "f",
        "// note\nfunction f(a) { return a }",
        "// other\nfunction f(a) {\n  return a;\n}",
    );
}

#[test]
fn go_rules() {
    let src = "package p\n//go:noinline\nfunc (l *List[T]) Push(v T) { l.n++ }\nconst (\n\tA = iota + 1\n\tB\n)\nfunc init() {}\nfunc init() { x() }\n";
    let n = names(Language::Go, src);
    assert_eq!(n, vec!["A", "B", "List::Push", "init"]);
    differ(
        Language::Go,
        "List::Push",
        src,
        &src.replace("//go:noinline", "//go:nosplit"),
    );
    same(
        Language::Go,
        "List::Push",
        src,
        &src.replace("//go:noinline\n", "//go:noinline\n// plain comment\n"),
    );
    // Implicit iota: B's meaning follows A's expression.
    differ(Language::Go, "B", src, &src.replace("iota + 1", "iota + 2"));
    same(
        Language::Go,
        "List::Push",
        src,
        &src.replace("{ l.n++ }", "{\n\tl.n++;\n}"),
    );
}

#[test]
fn parse_errors_are_local() {
    let ok = "fn a() { 1 }\nfn b() { 2 }\n";
    let broken = "fn a() { 1 }\nfn b() { 2 +* }\n";
    let f = file(Language::Rust, broken);
    assert!(f.has_error);
    assert!(!f.symbols["a"].error, "an error elsewhere leaves a Present");
    assert_eq!(
        f.symbols["a"].hash,
        file(Language::Rust, ok).symbols["a"].hash
    );
    assert!(f.symbols.get("b").is_none_or(|s| s.error));
}

#[test]
fn masked_hash_finds_renames() {
    let a = file(Language::Rust, "fn old_name(x: u8) -> u8 { x + 1 }");
    let b = file(Language::Rust, "fn new_name(x: u8) -> u8 { x + 1 }");
    assert_ne!(a.symbols["old_name"].hash, b.symbols["new_name"].hash);
    assert_eq!(a.symbols["old_name"].masked, b.symbols["new_name"].masked);
}

#[test]
fn deadline_cancels() {
    let big = "fn f() { 1 }\n".repeat(200_000);
    assert!(parse(Language::Rust, big.as_bytes(), Instant::now()).is_err());
}

/// T45b: the §4 rules that are neither pure formatting nor obvious meaning.
#[test]
fn t45b_directives_attributes_docstrings_strings() {
    // Rust doc comments are comments; `#[doc = …]` attributes count.
    same(
        Language::Rust,
        "f",
        "/// a\nfn f() {}",
        "//! x\n/// b\nfn f() {}",
    );
    differ(
        Language::Rust,
        "f",
        "#[doc = \"a\"]\nfn f() {}",
        "#[doc = \"b\"]\nfn f() {}",
    );
    // Raw and interpreted strings keep distinct kinds.
    differ(
        Language::Rust,
        "f",
        "fn f() -> &'static str { \"a\" }",
        "fn f() -> &'static str { r\"a\" }",
    );
    differ(
        Language::Go,
        "F",
        "package p\nfunc F() string { return \"a\" }\n",
        "package p\nfunc F() string { return `a` }\n",
    );
    // Go: build-constraint and line directives count; plain comments do not.
    differ(
        Language::Go,
        "F",
        "package p\n//export F\nfunc F() {}\n",
        "package p\n//export G\nfunc F() {}\n",
    );
    // Python: quote style is neutral, prefixes and `# type:` comments are not.
    same(
        Language::Python,
        "f",
        "def f():\n    return 'a'\n",
        "def f():\n    return \"a\"\n",
    );
    differ(
        Language::Python,
        "f",
        "def f():\n    return 'a'\n",
        "def f():\n    return b'a'\n",
    );
    differ(
        Language::Python,
        "f",
        "def f(a):\n    x = a  # type: int\n",
        "def f(a):\n    x = a  # type: str\n",
    );
    same(
        Language::Python,
        "f",
        "def f(a):\n    x = a + \\\n        1\n",
        "def f(a):\n    x = a + 1\n",
    );
    // JS/TS: a statement-level parenthesised string is not a directive.
    differ(
        Language::JavaScript,
        "f",
        "function f() { \"use strict\"; return 1 }",
        "function f() { (\"use strict\"); return 1 }",
    );
    same(
        Language::JavaScript,
        "f",
        "function f() { return `a${1}` }",
        "function f() {\n  return `a${1}`;\n}",
    );
    differ(
        Language::JavaScript,
        "f",
        "function f() { return 'a' }",
        "function f() { return `a` }",
    );
    // TS comments other than @ts-* directives are dropped; JSDoc in .ts too.
    same(
        Language::TypeScript,
        "f",
        "/** @param a x */\nfunction f(a: number) {}",
        "/** @param a y */\nfunction f(a: number) {}",
    );
    differ(
        Language::TypeScript,
        "f",
        "// @ts-ignore\nfunction f(a: number) {}",
        "// @ts-nocheck\nfunction f(a: number) {}",
    );
}

/// Walk 2 (T45 formatter corpus): each formatter-equivalence rule, with the
/// nearest pair that must still differ.
#[test]
fn walk2_formatter_equivalences() {
    use Language::{JavaScript, Python, Rust, TypeScript};
    // Rust: match-arm and closure blocks around one value; tail divergence.
    same(
        Rust,
        "f",
        "fn f(x: u8) -> u8 { match x { 0 => { g(1) } _ => g(2), } }",
        "fn f(x: u8) -> u8 { match x { 0 => g(1), _ => g(2) } }",
    );
    same(
        Rust,
        "f",
        "fn f(x: Option<u8>) -> u8 { match x { None => return 0, Some(v) => v } }",
        "fn f(x: Option<u8>) -> u8 { match x { None => { return 0; } Some(v) => v } }",
    );
    same(
        Rust,
        "f",
        "fn f() { v.map(|x| g(x)); }",
        "fn f() { v.map(|x| { g(x) }); }",
    );
    same(
        Rust,
        "f",
        "fn f() { let Some(x) = y else { return }; }",
        "fn f() { let Some(x) = y else { return; }; }",
    );
    differ(
        Rust,
        "f",
        "fn f() { v.map(|x| g(x)); }",
        "fn f() { v.map(|x| { g(x); }); }",
    );
    differ(
        Rust,
        "f",
        "fn f(x: u8) -> u8 { match x { 0 => { h(); g(1) } _ => g(2) } }",
        "fn f(x: u8) -> u8 { match x { 0 => g(1), _ => g(2) } }",
    );
    differ(
        Rust,
        "f",
        "fn f() -> u8 { loop { break 1; } }",
        "fn f() -> u8 { loop { continue; } }",
    );
    // Python: tuple and pattern parentheses, grouping in targets, types.
    same(
        Python,
        "f",
        "def f():\n    return a, b\n",
        "def f():\n    return (a, b)\n",
    );
    same(
        Python,
        "f",
        "def f():\n    x = a,\n",
        "def f():\n    x = (a,)\n",
    );
    differ(
        Python,
        "f",
        "def f():\n    x = a,\n",
        "def f():\n    x = (a)\n",
    );
    same(
        Python,
        "f",
        "def f():\n    for k, v in x:\n        pass\n",
        "def f():\n    for (k, v) in x:\n        pass\n",
    );
    same(
        Python,
        "f",
        "def f():\n    for k in x:\n        pass\n",
        "def f():\n    for (k) in x:\n        pass\n",
    );
    differ(
        Python,
        "f",
        "def f():\n    for k, in x:\n        pass\n",
        "def f():\n    for (k) in x:\n        pass\n",
    );
    same(
        Python,
        "f",
        "def f(x: type[A] | None) -> int: ...\n",
        "def f(x: (type[A] | None)) -> (int): ...\n",
    );
    differ(
        Python,
        "f",
        "def f(x: int | None): ...\n",
        "def f(x: None | int): ...\n",
    );
    same(
        Python,
        "f",
        "def f():\n    from m import a, b\n",
        "def f():\n    from m import (\n        a,\n        b,\n    )\n",
    );
    same(
        Python,
        "f",
        "def f(x):\n    match x:\n        case A() | B():\n            pass\n",
        "def f(x):\n    match x:\n        case (A() | B()):\n            pass\n",
    );
    // TypeScript/JavaScript: leading union bar, quotes and escapes, keys,
    // arrow parameter parentheses.
    same(
        TypeScript,
        "T",
        "type T = A | B;",
        "type T =\n  | A\n  | B;",
    );
    differ(TypeScript, "T", "type T = A | B;", "type T = B | A;");
    same(
        JavaScript,
        "f",
        "function f() { return 'it\\'s' }",
        "function f() { return \"it's\" }",
    );
    same(
        JavaScript,
        "f",
        "function f() { return '' }",
        "function f() { return \"\" }",
    );
    differ(
        JavaScript,
        "f",
        "function f() { return 'a\\nb' }",
        "function f() { return 'a\\\\nb' }",
    );
    same(
        JavaScript,
        "f",
        "function f() { return {'a': 1} }",
        "function f() { return {a: 1} }",
    );
    differ(
        JavaScript,
        "f",
        "function f() { return {'a-b': 1} }",
        "function f() { return {ab: 1} }",
    );
    differ(
        JavaScript,
        "f",
        "function f() { return {a: 'a'} }",
        "function f() { return {a: a} }",
    );
    same(TypeScript, "f", "const f = (x) => x;", "const f = x => x;");
    differ(
        TypeScript,
        "f",
        "const f = (x: number) => x;",
        "const f = x => x;",
    );
    differ(
        TypeScript,
        "f",
        "const f = (x = 1) => x;",
        "const f = x => x;",
    );
    same(JavaScript, "f", "const f = (x) => x;", "const f = x => x;");
}

/// G3 mutation follow-up: supported names and declaration boundaries. These
/// assertions check complete tables and cross-symbol isolation, not merely
/// the presence of selected names.
#[test]
fn extraction_boundaries() {
    use Language::{Go, JavaScript, Python, Rust, TypeScript};
    assert_eq!(names(Rust, "fn café() {} fn ok() {}"), ["ok"]);
    assert_eq!(names(Python, "def café(): pass\ndef ok(): pass\n"), ["ok"]);
    assert_eq!(
        names(JavaScript, "function $helper() {} function café() {}"),
        ["$helper"]
    );
    assert_eq!(
        names(Rust, "unsafe extern \"C\" { fn ffi(); static X: u8; }"),
        ["X", "ffi"]
    );
    assert_eq!(
        names(
            TypeScript,
            "declare function f(): void; declare const X: number;"
        ),
        ["X", "f"]
    );
    assert_eq!(names(Go, "package p\ntype Alias = int\n"), ["Alias"]);
    let vars = "export const a = 1, b = 2;";
    assert_eq!(names(TypeScript, vars), ["a", "b"]);
    same(TypeScript, "a", vars, "export const a = 1, b = 3;");
    differ(TypeScript, "b", vars, "export const a = 1, b = 3;");
    let types = "package p\ntype (\n A int\n B string\n)\n";
    assert_eq!(names(Go, types), ["A", "B"]);
    same(Go, "A", types, &types.replace("B string", "B bool"));
    differ(Go, "B", types, &types.replace("B string", "B bool"));
    // Never descend into function bodies, in any supported grammar.
    assert_eq!(names(Python, "def f():\n    def nested(): pass\n"), ["f"]);
    assert_eq!(
        names(JavaScript, "function f() { function nested() {} }"),
        ["f"]
    );
}

#[test]
fn normalisation_boundaries() {
    use Language::{JavaScript, Python, Rust, TypeScript};
    same(
        Rust,
        "f",
        "fn f() { v.map(|x| { if x { 1 } else { 2 } }); }",
        "fn f() { v.map(|x| if x { 1 } else { 2 }); }",
    );
    same(
        Python,
        "f",
        "def f():\n    for (x, y) in z:\n        pass\n",
        "def f():\n    for x, y in z:\n        pass\n",
    );
    same(
        Python,
        "f",
        "def f():\n    for (x,) in z:\n        pass\n",
        "def f():\n    for x, in z:\n        pass\n",
    );
    same(
        JavaScript,
        "f",
        r#"function f() { return "say \"yes\"" }"#,
        r#"function f() { return 'say "yes"' }"#,
    );
    // A comma followed by a comment is still a trailing comma. Changing the
    // skip index to subtraction/multiplication must not pass or loop forever.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let a = hash(Rust, "fn f(a: u8, /* why */) {}", "f");
        let b = hash(Rust, "fn f(a: u8) {}", "f");
        let _ = tx.send((a, b));
    });
    let (a, b) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("trailing-comment walk did not terminate");
    assert_eq!(a, b);
    // Quote elision applies to identifier object keys, not values or digit keys.
    same(
        JavaScript,
        "f",
        "function f() { return {'a': 1} }",
        "function f() { return {a: 1} }",
    );
    differ(
        JavaScript,
        "f",
        "function f() { return {'1a': 1} }",
        "function f() { return {'a': 1} }",
    );
    same(TypeScript, "f", "const f = (x) => x;", "const f = x => x;");
    // Parenthesised strings as values are ordinary grouping, unlike statement
    // directives (which the must-differ G2 pair above already tests).
    same(
        JavaScript,
        "f",
        "function f() { return ('a') }",
        "function f() { return 'a' }",
    );
}

/// Normaliser versions freeze stream shape, even where deleting a wrapper
/// happens not to collapse a particular same/differ pair. Check SPEC-required
/// markers directly through the diagnostic stream (the hashed walk itself).
#[test]
fn walk2_keeps_required_stream_shape() {
    use Language::{JavaScript, Python, Rust, TypeScript};
    let trace = |l, src: &str, name| {
        contextdb_symbols::trace(
            l,
            src.as_bytes(),
            name,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap()
        .unwrap()
    };
    for (src, blocks) in [
        ("fn f() { v.map(|x| { g(x); }); }", 2),
        ("fn f() { v.map(|x| { fn local() {} }); }", 3),
        ("fn f() { v.map(|x| { let y = x; }); }", 2),
        ("fn f() { v.map(|x| { if x {} 1 }); }", 3),
    ] {
        assert_eq!(
            trace(Rust, src, "f")
                .iter()
                .filter(|s| *s == "(block")
                .count(),
            blocks,
            "{src}"
        );
    }
    assert!(trace(TypeScript, "type T = A | B;", "T").contains(&"(union_type".into()));
    assert!(
        trace(TypeScript, "const f = (x: number) => x;", "f")
            .contains(&"(formal_parameters".into())
    );
    assert!(
        trace(Python, "def f():\n    for x, in z:\n        pass\n", "f")
            .contains(&"(tuple_pattern".into())
    );
    assert!(trace(Rust, "fn f() {}", "f").contains(&"L{ {".into()));
    let strings = trace(
        JavaScript,
        "function f() { return {'123': 'a', '': 1} }",
        "f",
    );
    for text in ["Lstring 123", "Lstring a", "Lstring "] {
        assert!(strings.contains(&text.into()), "{text} not in {strings:?}");
    }
}

#[test]
fn comment_range_api_excludes_directives_and_code() {
    let src = "// plain\n// @ts-ignore\nfunction f() { /* ordinary */ return 1; }";
    let ranges = contextdb_symbols::dropped_comments(
        Language::TypeScript,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(
        ranges.into_iter().map(|r| &src[r]).collect::<Vec<_>>(),
        ["// plain", "/* ordinary */"]
    );
}

/// A trailing comma with no closing bracket after it (found by the G6
/// retest, P5B-G6R-211: rustfmt adds one to a multi-line `where` clause) is
/// optional like any other trailing comma; a one-element tuple's comma
/// still means a tuple.
#[test]
fn trailing_comma_without_closing_bracket() {
    use Language::{Python, Rust};
    same(
        Rust,
        "f",
        "fn f<T>() where T: Clone {}",
        "fn f<T>()\nwhere\n    T: Clone,\n{\n}",
    );
    same(
        Rust,
        "f",
        "fn f<T, U>() where T: Clone, U: Copy {}",
        "fn f<T, U>()\nwhere\n    T: Clone,\n    U: Copy,\n{\n}",
    );
    differ(
        Rust,
        "f",
        "fn f<T, U>() where T: Clone, U: Copy {}",
        "fn f<T, U>() where T: Clone {}",
    );
    same(
        Python,
        "f",
        "def f():\n    x = 1, 2\n",
        "def f():\n    x = 1, 2,\n",
    );
    differ(
        Python,
        "f",
        "def f():\n    x = 1\n",
        "def f():\n    x = 1,\n",
    );
    differ(
        Python,
        "f",
        "def f():\n    for a in b: pass\n",
        "def f():\n    for a, in b: pass\n",
    );
}
