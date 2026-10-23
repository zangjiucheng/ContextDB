//! P5b T45 formatter corpus (ADR 0012 §4): every supported file of a pinned
//! commit is reformatted by the real formatter, with default settings and with
//! a stress style, and has its comments (and separately its doc comments)
//! rewritten; every symbol's normalised hash must be unchanged, apart from
//! listed, explained exceptions.
//!
//!   symbol_corpus <clone> <commit> <out.tsv> [--edition E] [--prettier BIN]
//!
//! Formatters come from PATH (`rustfmt`, `black`, `gofmt`) and `--prettier`
//! (default `prettier`); input goes through stdin, so nothing in the clone is
//! touched and rustfmt never follows `mod` declarations. A file the
//! formatter rejects is counted, not compared. `out.tsv` lists every
//! difference; stdout is the per-language summary.
use contextdb_anchors::Language;
use contextdb_symbols::{ParsedFile, dropped_comments, parse, trace};
use std::{
    collections::BTreeMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_FILE: usize = 1 << 20;

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let o = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(o.stdout)
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr)
        ))
    }
}
/// Runs a formatter over stdin; `None` when it rejects the input.
fn pipe(program: &str, args: &[&str], input: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    Ok(out.status.success().then_some(out.stdout))
}

struct Tools {
    edition: String,
    prettier: String,
}
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Variant {
    Default,
    Stress,
    Comments,
    DocComments,
}
impl Variant {
    const ALL: [Variant; 4] = [
        Variant::Default,
        Variant::Stress,
        Variant::Comments,
        Variant::DocComments,
    ];
    fn name(self) -> &'static str {
        match self {
            Variant::Default => "fmt-default",
            Variant::Stress => "fmt-stress",
            Variant::Comments => "comments",
            Variant::DocComments => "doc-comments",
        }
    }
}

/// The formatter's output, `Ok(None)` when it rejects the file, or the
/// variant does not apply (Go has no stress style).
fn format(
    tools: &Tools,
    language: Language,
    path: &str,
    src: &[u8],
    stress: bool,
) -> Result<Option<Vec<u8>>, String> {
    match language {
        Language::Rust => {
            let edition = tools.edition.as_str();
            let mut args = vec!["--edition", edition, "--emit", "stdout"];
            if stress {
                args.extend([
                    "--config",
                    "max_width=40,hard_tabs=true,tab_spaces=2,use_small_heuristics=Max,fn_params_layout=Vertical",
                ]);
            }
            pipe("rustfmt", &args, src)
        }
        Language::Python => {
            let mut args = vec!["-q"];
            if stress {
                args.extend(["-l", "30", "--skip-magic-trailing-comma"]);
            }
            args.push("-");
            pipe("black", &args, src)
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            let mut args = vec!["--stdin-filepath", path];
            if stress {
                args.extend([
                    "--print-width",
                    "30",
                    "--no-semi",
                    "--single-quote",
                    "--trailing-comma",
                    "none",
                    "--arrow-parens",
                    "avoid",
                    "--tab-width",
                    "8",
                ]);
            }
            pipe(&tools.prettier, &args, src)
        }
        Language::Go if stress => Ok(None),
        Language::Go => pipe("gofmt", &[], src),
    }
}

fn is_doc(text: &str) -> bool {
    ["///", "//!", "/**", "/*!"]
        .iter()
        .any(|p| text.starts_with(p))
}
/// Rewrites each dropped comment's text, keeping its delimiters (and so its
/// doc-ness) and any newline the comment node includes.
fn edit_comments(
    language: Language,
    src: &[u8],
    doc_only: bool,
) -> Result<Option<Vec<u8>>, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let ranges = dropped_comments(language, src, deadline).map_err(|e| e.0)?;
    let mut out = Vec::with_capacity(src.len());
    let mut at = 0;
    let mut edited = 0;
    for r in ranges {
        let text = String::from_utf8_lossy(&src[r.clone()]).into_owned();
        if doc_only && !is_doc(&text) {
            continue;
        }
        let newline = if text.ends_with('\n') { "\n" } else { "" };
        let replacement = if text.starts_with('#') {
            "# edited".to_owned()
        } else if text.starts_with("/*") {
            let open = &text[..3.min(text.len())];
            let open = if open == "/**" || open == "/*!" {
                open
            } else {
                "/*"
            };
            format!("{open} edited */")
        } else if text.starts_with("<!--") {
            "<!-- edited -->".to_owned()
        } else {
            let marker = ["///", "//!", "//"]
                .into_iter()
                .find(|m| text.starts_with(m))
                .unwrap_or("//");
            format!("{marker} edited")
        };
        out.extend_from_slice(&src[at..r.start]);
        out.extend_from_slice(replacement.as_bytes());
        out.extend_from_slice(newline.as_bytes());
        at = r.end;
        edited += 1;
    }
    out.extend_from_slice(&src[at..]);
    Ok((edited > 0).then_some(out))
}

#[derive(Default)]
struct Tally {
    files: usize,
    symbols: usize,
    rejected: usize,
    not_applicable: usize,
    changed: usize,
    missing: usize,
    extra: usize,
}

fn parsed(language: Language, bytes: &[u8]) -> Result<ParsedFile, String> {
    parse(language, bytes, Instant::now() + Duration::from_secs(30)).map_err(|e| e.0)
}

/// The first differing normalised tokens of a symbol, with two tokens of
/// context: `before ⟨a | b⟩ after`.
fn first_difference(
    language: Language,
    a: &[u8],
    b: &[u8],
    symbol: &str,
) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let get = |bytes| {
        trace(language, bytes, symbol, deadline)
            .map_err(|e| e.0)
            .map(Option::unwrap_or_default)
    };
    let (a, b) = (get(a)?, get(b)?);
    let at = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let show = |t: &[String], from: usize, to: usize| {
        t.get(from..to.min(t.len()))
            .unwrap_or_default()
            .join(" ")
            .replace(['\t', '\n'], " ")
    };
    Ok(format!(
        "{} <{} | {}>",
        show(&a, at.saturating_sub(4), at),
        show(&a, at, at + 3),
        show(&b, at, at + 3)
    ))
}

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let [clone, commit, out] = [0, 1, 2].map(|i| args.get(i).cloned());
    let (Some(clone), Some(commit), Some(out)) = (clone, commit, out) else {
        return Err(
            "usage: symbol_corpus <clone> <commit> <out.tsv> [--edition E] [--prettier BIN]".into(),
        );
    };
    let tools = Tools {
        edition: flag("--edition").unwrap_or_else(|| "2021".into()),
        prettier: flag("--prettier").unwrap_or_else(|| "prettier".into()),
    };
    let root = Path::new(&clone);
    let listing = git(
        root,
        &["ls-tree", "-r", "-z", "--name-only", "--full-tree", &commit],
    )?;
    let mut report = std::fs::File::create(&out).map_err(|e| e.to_string())?;
    writeln!(
        report,
        "# symbol_corpus {} at {commit}\nlanguage\tvariant\tpath\tsymbol\tdifference\tfirst difference",
        root.display()
    )
    .map_err(|e| e.to_string())?;
    let mut tally: BTreeMap<(&str, Variant), Tally> = BTreeMap::new();
    let mut skipped_errors: BTreeMap<&str, usize> = BTreeMap::new();
    for path in listing.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path).into_owned();
        let Some(language) = Language::of_path(&path) else {
            continue;
        };
        let src = git(root, &["cat-file", "blob", &format!("{commit}:{path}")])?;
        if src.len() > MAX_FILE {
            continue;
        }
        let original = parsed(language, &src)?;
        if original.has_error {
            *skipped_errors.entry(language.name()).or_default() += 1;
            continue;
        }
        for variant in Variant::ALL {
            let t = tally.entry((language.name(), variant)).or_default();
            let changed = match variant {
                Variant::Default => format(&tools, language, &path, &src, false)?,
                Variant::Stress => format(&tools, language, &path, &src, true)?,
                Variant::Comments => edit_comments(language, &src, false)?,
                Variant::DocComments => edit_comments(language, &src, true)?,
            };
            let Some(changed) = changed else {
                // Formatter output is absent only on rejection (Go has no
                // stress style); comment edits are absent when there are none.
                let formatter = variant == Variant::Default
                    || (variant == Variant::Stress && language != Language::Go);
                if formatter {
                    t.rejected += 1;
                } else {
                    t.not_applicable += 1;
                }
                continue;
            };
            t.files += 1;
            let after = parsed(language, &changed)?;
            for (symbol, s) in &original.symbols {
                if s.error {
                    continue;
                }
                t.symbols += 1;
                let difference = match after.symbols.get(symbol) {
                    None => {
                        t.missing += 1;
                        "missing"
                    }
                    Some(a) if a.error => {
                        t.changed += 1;
                        "parse-error"
                    }
                    Some(a) if a.span != s.span => {
                        t.changed += 1;
                        "changed"
                    }
                    Some(_) => continue,
                };
                let detail = if difference == "changed" {
                    first_difference(language, &src, &changed, symbol)?
                } else {
                    String::new()
                };
                writeln!(
                    report,
                    "{}\t{}\t{path}\t{symbol}\t{difference}\t{detail}",
                    language.name(),
                    variant.name()
                )
                .map_err(|e| e.to_string())?;
            }
            for symbol in after.symbols.keys() {
                if !original.symbols.contains_key(symbol) {
                    t.extra += 1;
                    writeln!(
                        report,
                        "{}\t{}\t{path}\t{symbol}\textra",
                        language.name(),
                        variant.name()
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
    }
    println!("language\tvariant\tfiles\tsymbols\tchanged\tmissing\textra\trejected\tn/a");
    for ((language, variant), t) in &tally {
        println!(
            "{language}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            variant.name(),
            t.files,
            t.symbols,
            t.changed,
            t.missing,
            t.extra,
            t.rejected,
            t.not_applicable
        );
    }
    for (language, n) in skipped_errors {
        println!("# {language}: {n} files with parse errors at {commit} not compared");
    }
    Ok(())
}
