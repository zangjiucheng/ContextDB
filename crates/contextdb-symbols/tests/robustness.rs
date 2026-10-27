//! P5b G4 (ADR 0012 §9): deep and large trees stay within the stack and the
//! deadline; extraction and the walk are cancelled like the parse.
use contextdb_anchors::Language;
use contextdb_symbols::{MAX_NESTING, ParsedFile, Unavailable, parse};
use std::time::{Duration, Instant};

/// What the observer may spend past its deadline: noticing it (every 1,024
/// nodes) and freeing the tree and table of a file up to 16 MiB (measured
/// 0.2–0.6 s alone, more while other tests compete for the cores).
const SLACK: Duration = Duration::from_secs(2);

/// Parses on a thread with Rust's default 2 MiB spawned-thread stack (the
/// observer's main thread has 8 MiB), within `deadline`. A parse that misses
/// the deadline fails the test when the slack runs out, not when it ends.
fn bounded(language: Language, src: String, budget: Duration) -> Result<ParsedFile, Unavailable> {
    // Timed parses run one at a time, so their slack is not spent waiting
    // for cores the other tests hold.
    static TIMED: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = TIMED.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            let _ = tx.send(parse(language, src.as_bytes(), Instant::now() + budget));
        })
        .unwrap();
    let result = rx
        .recv_timeout(budget + SLACK)
        .unwrap_or_else(|e| panic!("{language:?}: no result within {budget:?} + slack: {e}"));
    let took = started.elapsed();
    assert!(
        took <= budget + SLACK,
        "{language:?}: {took:?} past {budget:?}"
    );
    if let Err(e) = &result {
        assert_eq!(e.0, "parse deadline", "{language:?}");
    }
    result
}
fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}
fn indented(line: &str, depth: usize, inner: &str) -> String {
    let mut s = String::new();
    for i in 0..depth {
        s += &" ".repeat(i);
        s += line;
        s += "\n";
    }
    s + &" ".repeat(depth) + inner + "\n"
}

/// Containers nest past the bound: the extraction stops at `MAX_NESTING`
/// levels, so its recursion stays small.
#[test]
fn deep_containers_stop_at_the_nesting_bound() {
    let deadline = Duration::from_secs(30);
    for depth in [MAX_NESTING - 1, MAX_NESTING, MAX_NESTING + 1, 20_000] {
        let cases = [
            (Language::Rust, nested("mod a {", "fn f() {}", "}", depth)),
            (
                Language::TypeScript,
                nested("namespace a {", "function f() {}", "}", depth),
            ),
            (
                Language::Tsx,
                nested("namespace a {", "function f() {}", "}", depth),
            ),
            (
                Language::Python,
                indented("class a:", depth.min(400), "def f(): pass"),
            ),
        ];
        for (language, src) in cases {
            let result = bounded(language, src, deadline);
            // Far past the bound, 256 nested spans each cover the whole
            // remaining nest: an unoptimised build may reach the deadline.
            let Ok(file) = result else {
                assert!(
                    depth > MAX_NESTING + 1,
                    "{language:?} at {depth}: {result:?}"
                );
                continue;
            };
            // Every container level is a symbol; `f` exists only inside the
            // bound. (Python's indentation scanner gives up near 500 levels.)
            let levels = depth.min(if language == Language::Python {
                400
            } else {
                depth
            });
            let expected = levels.min(MAX_NESTING) + usize::from(levels < MAX_NESTING);
            assert_eq!(file.symbols.len(), expected, "{language:?} at {depth}");
        }
    }
}

/// Deep expressions, blocks and lists: the walk is iterative and linear.
#[test]
fn deep_expressions_stay_linear() {
    let deadline = Duration::from_secs(5);
    let d = 50_000;
    let cases = [
        (
            Language::Rust,
            format!("fn f() {{ {} }}", nested("(", "1", ")", d)),
        ),
        (
            Language::Rust,
            format!("fn f() {}", nested("{", "1", "}", d)),
        ),
        (
            Language::Rust,
            format!("const X: u8 = {};", nested("[", "1", "]", d)),
        ),
        (
            Language::TypeScript,
            format!("const x = {};", nested("(", "1", ")", d)),
        ),
        (
            Language::TypeScript,
            format!("const x = {};", nested("[", "1", "]", d)),
        ),
        (
            Language::JavaScript,
            format!("const x = {};", nested("(", "a?.b", ")", d)),
        ),
        (
            Language::JavaScript,
            format!("function f() {}", nested("{", "", "}", d)),
        ),
        (
            Language::Python,
            format!("x = {}\n", nested("(", "1", ")", d)),
        ),
        (
            Language::Python,
            format!("x = {}\n", nested("[", "1", "]", d)),
        ),
        (
            Language::Go,
            format!("package p\nvar x = {}\n", nested("(", "1", ")", d)),
        ),
        (
            Language::Go,
            format!("package p\nfunc f() {}\n", nested("{", "", "}", d)),
        ),
    ];
    for (language, src) in cases {
        let _ = bounded(language, src, deadline);
    }
}

/// Large files up to the capture bound (16 MiB): many declarations, and one
/// declaration with millions of list elements (sibling lookups stay linear).
#[test]
fn large_files_within_the_deadline() {
    let deadline = Duration::from_secs(5);
    let bound = contextdb_anchors::capture::MAX_FILE;
    let fill = |head: &str, item: &str, tail: &str| {
        let n = (bound - head.len() - tail.len()) / item.len();
        format!("{head}{}{tail}", item.repeat(n))
    };
    let cases = [
        (Language::Rust, fill("", "fn f() -> u8 { 1 }\n", "")),
        (Language::Rust, fill("const X: [u8; 0] = [", "1,", "];")),
        (Language::TypeScript, fill("export const x = [", "1,", "];")),
        (Language::JavaScript, fill("const x = {", "a: (1),", "};")),
        (Language::Python, fill("x = [", "(1),", "]\n")),
        (Language::Go, fill("package p\nvar x = []int{", "1,", "}\n")),
    ];
    for (language, src) in cases {
        assert!(src.len() <= bound);
        let _ = bounded(language, src, deadline);
    }
}

/// The parse finishes early, yet extraction and the walk over 255 nested
/// copies of a large body still stop at the deadline.
#[test]
fn walk_is_cancelled_at_the_deadline() {
    let body = format!("fn f() -> u32 {{ {} }}", "1 + ".repeat(200_000) + "1");
    let flat = Instant::now();
    parse(
        Language::Rust,
        body.as_bytes(),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    let flat = flat.elapsed();
    let deep = nested("mod a {", &body, "}", MAX_NESTING - 1);
    let budget = flat * 4 + Duration::from_millis(200);
    let r = bounded(Language::Rust, deep, budget);
    assert_eq!(r.map(|_| ()), Err(Unavailable("parse deadline".into())));
}

/// A deadline already passed: unavailable, never a partial table.
#[test]
fn passed_deadline_is_unavailable_for_every_language() {
    for language in [
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Python,
        Language::Go,
    ] {
        let r = parse(language, b"x", Instant::now() - Duration::from_millis(1));
        assert_eq!(r.map(|_| ()), Err(Unavailable("parse deadline".into())));
    }
}

/// Error recovery inserts zero-width `MISSING` names: they are not symbols,
/// and every extracted name has a key (found by the `symbols_parse` fuzz
/// target: an empty JavaScript name made `ctx anchors symbols` fail).
#[test]
fn missing_names_are_not_symbols() {
    use contextdb_anchors::symbol::{decode_symbol, encode_symbol};
    let fuzz: &[u8] = b"//\x00/\x00const?:,,):U,,\x1b\x00\x00\xff\x17\x00\x00\x00EEcol,:),:U,,stw\x00\x00\x00\x00\x00\x00\x00\x00=:,,!:U,,\x1b/\x00/\x00co\x00EE";
    let cases: [(Language, &[u8]); 10] = [
        (Language::JavaScript, fuzz),
        (
            Language::JavaScript,
            b"const = 1;\nfunction () {}\nclass {}\n",
        ),
        (
            Language::TypeScript,
            b"interface {}\ntype = number;\nnamespace {}\n",
        ),
        (Language::Tsx, b"enum {}\nconst = <a/>;\n"),
        (Language::Rust, b"fn () {}\nstruct ;\nmod {}\nimpl {}\n"),
        (Language::Rust, b"trait { fn (); }\nconst : u8 = 1;\n"),
        (Language::Python, b"def ():\n  pass\nclass :\n  pass\n"),
        (Language::Go, b"package p\nfunc () {}\ntype = int\n"),
        (
            Language::Go,
            b"package p\nfunc (r *) m() {}\nconst ( = 1 )\n",
        ),
        (
            Language::JavaScript,
            b"export default class extends {}\nexport const ;\n",
        ),
    ];
    for (language, src) in cases {
        let file = parse(language, src, Instant::now() + Duration::from_secs(5)).unwrap();
        for symbol in file.symbols.keys() {
            let key =
                encode_symbol(symbol).unwrap_or_else(|e| panic!("{language:?} {symbol:?}: {e:?}"));
            assert_eq!(&decode_symbol(&key).unwrap(), symbol);
        }
    }
}

/// Sibling containers do not add up: the nesting bound counts depth only.
#[test]
fn sibling_containers_are_not_nesting() {
    let n = MAX_NESTING + 44;
    let src: String = (0..n)
        .map(|i| format!("mod m{i} {{ fn f() {{}} }}\n"))
        .collect();
    let file = parse(
        Language::Rust,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    assert_eq!(file.symbols.len(), 2 * n);
    assert!(file.symbols.contains_key(&format!("m{}::f", n - 1)));
}

/// When the whole file fails to parse, tree-sitter's root is itself `ERROR`:
/// the declarations below it are marked as errors although their own nodes
/// have none (found in the `symbols_parse` fuzz corpus; Go, so the
/// ancestor pass also runs for a language without transparent parentheses).
#[test]
fn declarations_under_an_error_root_are_errors() {
    let src = "type\x16:U e#\x16e\x18\x00\u{85}G((((((((((((#e(((";
    let file = parse(
        Language::Go,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert!(file.has_error);
    assert!(file.symbols["U"].error, "{:?}", file.symbols);
}

/// A symbol reached through an `ERROR` ancestor is an error even when
/// nothing in a top-level error region names it (walk 4 made the Go case
/// above erroneous twice over; this fuzz-corpus input is decided by the
/// ancestor alone).
#[test]
fn declarations_under_an_error_ancestor_alone_are_errors() {
    let src = b"=\x81{{{s;c{{{{|{{{{{{{v\xff\xff\xff\xffclass\xff{{{z;{";
    let file = parse(
        Language::JavaScript,
        src,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert!(!file.symbols.is_empty());
    assert!(file.symbols.values().all(|s| s.error), "{:?}", file.symbols);
    // Rust (no transparent parentheses): an ERROR root over the whole file.
    let src = "trait T { fn a(); }\nimpl T for X< { fn a() {} }\nconst C: u8 = 1;\n";
    let file = parse(
        Language::Rust,
        src.as_bytes(),
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    assert!(file.symbols.contains_key("T::a"));
    assert!(file.symbols.values().all(|s| s.error), "{:?}", file.symbols);
}

#[test]
fn unavailable_displays_its_reason() {
    assert_eq!(
        Unavailable("parse deadline".into()).to_string(),
        "parse deadline"
    );
}
