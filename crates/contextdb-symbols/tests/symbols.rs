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
