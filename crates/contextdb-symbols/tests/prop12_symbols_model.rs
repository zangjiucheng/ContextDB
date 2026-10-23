//! PROP12 extended to symbol anchors, model-driven variant (SPEC §8, ADR 0012
//! §9 G3). It cross-checks `prop12_symbols.rs`, which is written separately:
//! the two share no generator, renderer or oracle code.
//!
//! 10,000 fixed seeds each generate a history from a symbol model, for scopes
//! mixing file and symbol anchors (both on one host file too) in Rust,
//! TS/TSX/JS, Python and Go, sometimes two languages in one scope.
//!
//! - System under test: the production scan (`targets` + `prepare_scan`:
//!   parse, observation, rebind candidates, position, classification) and the
//!   kernel commit, with the observer's reload-and-recapture CAS retry.
//! - Oracle 1, the generator's model: states of a symbol with equal meaning
//!   (body, signature, attributes, directives; never layout or comments) share
//!   one revision, different meanings never do; a plain rename lists the new
//!   name as a rebind candidate.
//! - Oracle 2, the SPEC form: every committed tree (and every worktree that
//!   differs from it) goes through `git fast-import` and is read back with
//!   `cat-file`; descriptors (`CTXANC01`/`CTXANC02`), keys and positions
//!   (`CTXPOS01`/`CTXPOS02`) are re-encoded here from the ADR text, and the
//!   classification is replayed from the ADR 0011/0012 rules; it never calls
//!   the classifier or the observer. The normaliser (`parse`) defines symbol
//!   hashes and is the one shared function; its versions are spelled out.
//! - Coverage counts transitions that actually happened between successful
//!   scans, or faults that took effect, never branch labels.
use contextdb_anchors::{
    AnchorDescriptor, AnchorState, Descriptor, Language, ObjectFormat, Observation,
    capture::{blob_oid, git, position as live_position},
    validate_path,
};
use contextdb_core::{Db, IssueRequest, LogLink};
use contextdb_model::*;
use contextdb_symbols::{
    normaliser_version,
    observer::{Added, Mode, prepare_scan, targets},
    parse,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

const SEEDS: u64 = 10_000;
/// The full count, or a slice for mutation runs (`CONTEXTDB_SYMBOL_SEEDS`,
/// shared with `prop12_symbols`); coverage floors apply to the full count.
fn seeds() -> u64 {
    std::env::var("CONTEXTDB_SYMBOL_SEEDS")
        .map(|s| s.parse().expect("CONTEXTDB_SYMBOL_SEEDS"))
        .unwrap_or(SEEDS)
}
const STEPS: usize = 10;
/// Symbol names: `alpha`/`Alpha` are a case alias, `do_it` exercises `_`.
const NAMES: [&str; 7] = ["alpha", "beta", "gamma", "do_it", "Alpha", "omega", "zeta"];
/// Registration attempts that the path domain must reject.
const UNSUPPORTED: [&str; 5] = ["A.rs", "src/../a.rs", ".git/config", "/abs.rs", "src//x.rs"];
const HASH_PATHS: [&str; 3] = ["src/a#b.rs", "x#.py", "#go.go"];
const NOTES: &str = "notes.txt";

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, permille: u64) -> bool {
        self.below(1000) < permille
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

// ---------------------------------------------------------------- model --

/// What a symbol means: body, signature, attribute, directive variants, and
/// whether its body uses the file's `FILL` item (walk 3 same-file context:
/// then `FILL`'s value is part of what it means).
type Meaning = [u8; 5];
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sym {
    meaning: Meaning,
    /// Layout and comment variants: never meaning.
    layout: u8,
    comment: u8,
    /// A fatal syntax error injected inside the span.
    error: bool,
}
#[derive(Clone, PartialEq, Eq, Debug)]
struct HostModel {
    ext: &'static str,
    symbols: BTreeMap<&'static str, Sym>,
    /// The same-path group (`g`, Go `init`): one meaning per occurrence.
    group: [u8; 2],
    group_layout: u8,
    /// An unanchored item: edits to it are outside every span.
    filler: u8,
}
#[derive(Clone, PartialEq, Eq, Debug)]
enum HostOrText {
    Host(HostModel),
    Text(u8),
}
type World = BTreeMap<String, Option<HostOrText>>;

fn family(ext: &str) -> usize {
    match ext {
        "rs" => 0,
        "ts" | "tsx" | "js" => 1,
        "py" => 2,
        _ => 3,
    }
}
fn group_name(ext: &str) -> &'static str {
    if ext == "go" { "init" } else { "g" }
}

// ------------------------------------------------------------- rendering --

/// The body's use of the file's `FILL` item.
fn fill(s: &Sym, name: &str) -> String {
    if s.meaning[4] == 1 {
        format!(" + {name}")
    } else {
        String::new()
    }
}
fn render_rust(name: &str, s: &Sym) -> String {
    let [b, sig, attr, _, _] = s.meaning;
    let attr = ["", "#[inline]\n", "#[must_use]\n"][attr as usize];
    let params: &[&str] = if sig == 0 {
        &["x: u32"]
    } else {
        &["x: u32", "y: u32"]
    };
    let mut body = format!("x + {}{}", 10 + b, fill(s, "FILL"));
    if s.error {
        body.push_str(" +* ");
    }
    let (before, inside) = match s.comment {
        1 => ("// note\n", ""),
        2 => ("/// doc\n", ""),
        3 => ("", "/* c */ "),
        _ => ("", ""),
    };
    let one = params.join(", ");
    let decl = match s.layout {
        0 => format!("fn {name}({one}) -> u32 {{ {inside}{body} }}"),
        1 => {
            let each: String = params.iter().map(|p| format!("    {p},\n")).collect();
            format!("fn {name}(\n{each}) -> u32 {{\n    {inside}{body}\n}}")
        }
        2 => format!("fn {name}({one}) -> u32 {{\n\n\t{inside}{body}\n}}"),
        _ => format!("fn  {name}( {one} )  ->  u32{{{inside}{body}}}"),
    };
    format!("{before}{attr}{decl}\n\n")
}
fn render_python(name: &str, s: &Sym) -> String {
    let [b, sig, attr, directive, _] = s.meaning;
    let attr = ["", "@cache\n", "@staticmethod\n"][attr as usize];
    let params: &[&str] = if sig == 0 { &["x"] } else { &["x", "y=1"] };
    let line = ["y = x", "y = x  # type: int", "y = x  # type: str"][directive as usize];
    let b = format!("{}{}", 10 + b, fill(s, "FILL"));
    let tail = if s.error { " +* )" } else { "" };
    let (before, trailing, inside) = match s.comment {
        1 => ("# note\n", "", ""),
        2 => ("", "  # why", ""),
        3 => ("", "", "    # inside\n"),
        _ => ("", "", ""),
    };
    let one = params.join(", ");
    let def = match s.layout {
        0 => {
            format!("def {name}({one}):{trailing}\n{inside}    {line}\n    return x + {b}{tail}\n")
        }
        1 => {
            let each: String = params.iter().map(|p| format!("    {p},\n")).collect();
            format!(
                "def {name}(\n{each}):{trailing}\n{inside}    {line}\n    return (x + {b}){tail}\n"
            )
        }
        2 => format!(
            "def {name}({one}):{trailing}\n{inside}    {line}\n    return x + \\\n        {b}{tail}\n"
        ),
        _ => {
            format!("def {name}({one}) :{trailing}\n\n{inside}    {line}\n    return x+{b}{tail}\n")
        }
    };
    format!("{before}{attr}{def}\n\n")
}
fn render_ts(name: &str, s: &Sym, ext: &str) -> String {
    let [b, sig, attr, directive, _] = s.meaning;
    let typed = ext != "js";
    let attr = ["", "export ", "async "][attr as usize];
    let params: &[&str] = match (typed, sig) {
        (true, 0) => &["x: number"],
        (true, _) => &["x: number", "y?: number"],
        (false, 0) => &["x"],
        (false, _) => &["x", "y"],
    };
    let ret = if typed { ": number" } else { "" };
    let directive = ["", "  // @ts-ignore\n", "  // @ts-expect-error\n"][directive as usize];
    let b = format!("{}{}", 10 + b, fill(s, "FILL"));
    let expr = if s.error {
        format!("x + {b} + ")
    } else {
        format!("x + {b}")
    };
    let (before, inside) = match s.comment {
        1 => ("// note\n", ""),
        2 => ("", "/* c */ "),
        // JSDoc counts in checked JavaScript (§4); plain comments elsewhere.
        3 if typed => ("/** doc */\n", ""),
        3 => ("// other\n", ""),
        _ => ("", ""),
    };
    let one = params.join(", ");
    let decl = match s.layout {
        0 => format!(
            "function {name}({one}){ret} {{\n{directive}  {inside}const s = 'k';\n  return {expr};\n}}"
        ),
        1 => {
            let each: String = params.iter().map(|p| format!("  {p},\n")).collect();
            format!(
                "function {name}(\n{each}){ret} {{\n{directive}  {inside}const s = \"k\"\n  return ({expr})\n}}"
            )
        }
        2 => format!(
            "function {name}({one}){ret}{{\n{directive}{inside}const s='k';return {expr};}}"
        ),
        _ => format!(
            "function   {name}( {one} ){ret} {{\n\n{directive}  {inside}const s = \"k\";\n\n  return {expr};\n}}"
        ),
    };
    format!("{before}{attr}{decl}\n\n")
}
fn render_go(name: &str, s: &Sym) -> String {
    let [b, sig, _, directive, _] = s.meaning;
    let directive = ["", "//go:noinline\n", "//go:nosplit\n"][directive as usize];
    let params: &[&str] = if sig == 0 {
        &["x int"]
    } else {
        &["x int", "y int"]
    };
    let b = format!("{}{}", 10 + b, fill(s, "fill"));
    let expr = if s.error {
        format!("x + {b} +")
    } else {
        format!("x + {b}")
    };
    let (before, between, inside) = match s.comment {
        1 => ("// note\n", "", ""),
        2 => ("", "// plain\n", ""),
        3 => ("", "", "/* c */ "),
        _ => ("", "", ""),
    };
    let one = params.join(", ");
    let decl = match s.layout {
        0 => format!("func {name}({one}) int {{\n\t{inside}return {expr}\n}}"),
        1 => {
            let each: String = params.iter().map(|p| format!("\t{p},\n")).collect();
            format!("func {name}(\n{each}) int {{\n\t{inside}return {expr};\n}}")
        }
        2 => format!("func {name}({one}) int {{ {inside}return {expr}\n}}"),
        _ => format!("func {name}({one}) int {{\n\n\t{inside}return {expr}\n\n}}"),
    };
    format!("{before}{directive}{between}{decl}\n\n")
}
fn render_group(ext: &str, [m1, m2]: [u8; 2], layout: u8) -> String {
    let sep = if layout.is_multiple_of(2) {
        "\n"
    } else {
        "\n\n\n"
    };
    match family(ext) {
        0 => format!(
            "#[cfg(unix)]\nfn g() -> u32 {{ {} }}{sep}#[cfg(not(unix))]\nfn g() -> u32 {{ {} }}\n\n",
            20 + m1,
            20 + m2
        ),
        1 if ext == "js" => format!(
            "function g() {{ return {}; }}{sep}function g() {{ return {} }}\n\n",
            20 + m1,
            20 + m2
        ),
        1 => format!(
            "function g(a: {}): number;{sep}function g(a: any) {{ return {}; }}\n\n",
            ["string", "boolean", "bigint"][m1 as usize],
            20 + m2
        ),
        2 => format!(
            "if FLAG:\n    def g():\n        return {}\nelse:{sep}    def g():\n        return {}\n\n",
            20 + m1,
            20 + m2
        ),
        _ => format!(
            "func init() {{ _ = {} }}{sep}func init() {{\n\t_ = {}\n}}\n\n",
            20 + m1,
            20 + m2
        ),
    }
}
fn render(h: &HostModel) -> Vec<u8> {
    let mut out = match family(h.ext) {
        0 => format!("const FILL: u32 = {};\n\n", h.filler),
        1 => format!("const FILL = {};\n\n", h.filler),
        2 => format!("FILL = {}\n\n", h.filler),
        _ => format!("package p\n\nvar fill = {}\n\n", h.filler),
    };
    for (name, s) in &h.symbols {
        out.push_str(&match family(h.ext) {
            0 => render_rust(name, s),
            1 => render_ts(name, s, h.ext),
            2 => render_python(name, s),
            _ => render_go(name, s),
        });
    }
    out.push_str(&render_group(h.ext, h.group, h.group_layout));
    out.into_bytes()
}
fn bytes_of(v: &HostOrText) -> Vec<u8> {
    match v {
        HostOrText::Host(h) => render(h),
        HostOrText::Text(t) => format!("notes v{t}\r\n").into_bytes(),
    }
}

// ------------------------------------------------------------------ plan --

#[derive(Clone)]
struct Step {
    /// The tree committed at this step (HEAD) and the worktree the observer
    /// captures; they differ only in uncommitted steps.
    committed: World,
    live: World,
    register_files: Vec<String>,
    /// (host, symbol) registrations.
    register_symbols: Vec<(String, &'static str)>,
    unsupported: Option<&'static str>,
    hash_path: Option<&'static str>,
    unavailable: bool,
    race: bool,
    /// Symbols renamed at this step: (host, old, new).
    renames: Vec<(String, &'static str, &'static str)>,
}

fn fresh_sym(rng: &mut Rng, ext: &str) -> Sym {
    Sym {
        meaning: [
            rng.below(3) as u8,
            rng.below(2) as u8,
            if ext == "go" { 0 } else { rng.below(3) as u8 },
            if ext == "rs" { 0 } else { rng.below(3) as u8 },
            rng.below(2) as u8,
        ],
        layout: rng.below(4) as u8,
        comment: rng.below(4) as u8,
        error: false,
    }
}
fn fresh_host(rng: &mut Rng, ext: &'static str) -> HostModel {
    let mut symbols = BTreeMap::new();
    for name in NAMES {
        if rng.chance(if name == "alpha" || name == "Alpha" {
            700
        } else {
            450
        }) {
            symbols.insert(name, fresh_sym(rng, ext));
        }
    }
    HostModel {
        ext,
        symbols,
        group: [rng.below(3) as u8, rng.below(3) as u8],
        group_layout: 0,
        filler: 0,
    }
}
const EXTS: [[&str; 3]; 4] = [["rs"; 3], ["ts", "tsx", "js"], ["py"; 3], ["go"; 3]];
fn exts(seed: u64) -> (&'static str, Option<&'static str>) {
    let mut rng = Rng::new(seed ^ 0x5eed);
    let first = EXTS[(seed % 4) as usize][((seed / 4) % 3) as usize];
    let second = rng.chance(500).then(|| {
        if rng.chance(300) {
            EXTS[((seed + 1 + rng.below(3)) % 4) as usize][rng.below(3) as usize]
        } else {
            first
        }
    });
    (first, second)
}
fn host_path(n: usize, ext: &str) -> String {
    if n == 0 {
        format!("src/a.{ext}")
    } else {
        format!("b.{ext}")
    }
}

/// Evolves one host model by one step; returns renames (old, new).
fn evolve(
    rng: &mut Rng,
    h: &mut HostModel,
    history: &mut BTreeMap<&'static str, Vec<Meaning>>,
) -> Vec<(&'static str, &'static str)> {
    let mut renames = Vec::new();
    let ext = h.ext;
    for (name, s) in &h.symbols {
        let seen = history.entry(name).or_default();
        if !seen.contains(&s.meaning) {
            seen.push(s.meaning);
        }
    }
    let names: Vec<&'static str> = h.symbols.keys().copied().collect();
    for name in names {
        let s = h.symbols.get_mut(name).unwrap();
        if s.error {
            if rng.chance(600) {
                s.error = false;
            }
            continue;
        }
        match rng.below(1000) {
            0..=129 => s.layout = (s.layout + 1 + rng.below(3) as u8) % 4,
            130..=259 => s.comment = (s.comment + 1 + rng.below(3) as u8) % 4,
            260..=349 => s.meaning[0] = (s.meaning[0] + 1 + rng.below(2) as u8) % 3,
            350..=389 => s.meaning[1] ^= 1,
            390..=429 if ext != "go" => s.meaning[2] = (s.meaning[2] + 1) % 3,
            430..=469 if ext != "rs" => s.meaning[3] = (s.meaning[3] + 1) % 3,
            470..=539 => {
                // Restore an earlier meaning (a different one if any).
                let earlier: Vec<Meaning> = history[name]
                    .iter()
                    .filter(|m| **m != s.meaning)
                    .copied()
                    .collect();
                if !earlier.is_empty() {
                    s.meaning = rng.pick(&earlier);
                }
            }
            540..=569 => {
                let free: Vec<&'static str> = NAMES
                    .iter()
                    .copied()
                    .filter(|n| !h.symbols.contains_key(n))
                    .collect();
                if !free.is_empty() {
                    let new = rng.pick(&free);
                    let s = h.symbols.remove(name).unwrap();
                    h.symbols.insert(new, s);
                    history.entry(new).or_default().push(s.meaning);
                    renames.push((name, new));
                }
                continue;
            }
            570..=609 => {
                h.symbols.remove(name);
                continue;
            }
            610..=649 => s.error = true,
            650..=689 => s.meaning[4] ^= 1,
            _ => {}
        }
        let s = h.symbols[name];
        history.entry(name).or_default().push(s.meaning);
    }
    for name in NAMES {
        if !h.symbols.contains_key(name) && rng.chance(120) {
            // Reinsert: half the time with the last meaning it had.
            let mut s = fresh_sym(rng, ext);
            if let Some(last) = history.get(name).and_then(|v| v.last())
                && rng.chance(500)
            {
                s.meaning = *last;
            }
            history.entry(name).or_default().push(s.meaning);
            h.symbols.insert(name, s);
        }
    }
    if rng.chance(150) {
        h.filler = (h.filler + 1) % 4;
    }
    if rng.chance(100) {
        h.group[rng.below(2) as usize] = rng.below(3) as u8;
    }
    if rng.chance(100) {
        h.group_layout ^= 1;
    }
    renames
}

fn plan(seed: u64) -> Vec<Step> {
    let mut rng = Rng::new(seed);
    let (first, second) = exts(seed);
    let mut live: World = BTreeMap::new();
    live.insert(
        host_path(0, first),
        Some(HostOrText::Host(fresh_host(&mut rng, first))),
    );
    if let Some(ext) = second {
        live.insert(
            host_path(1, ext),
            Some(HostOrText::Host(fresh_host(&mut rng, ext))),
        );
    }
    live.insert(NOTES.into(), Some(HostOrText::Text(0)));
    let mut histories: BTreeMap<String, BTreeMap<&'static str, Vec<Meaning>>> = BTreeMap::new();
    let mut stash: BTreeMap<String, HostModel> = BTreeMap::new();
    let mut committed = live.clone();
    let mut registered_files: BTreeSet<String> = BTreeSet::new();
    let mut registered_symbols: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut steps = Vec::new();
    for step in 0..STEPS {
        let mut renames = Vec::new();
        if step > 0 {
            let paths: Vec<String> = live.keys().cloned().collect();
            for path in paths {
                let slot = live.get_mut(&path).unwrap();
                match slot {
                    Some(HostOrText::Host(h)) if rng.chance(30) => {
                        stash.insert(path.clone(), h.clone());
                        *slot = None;
                    }
                    Some(HostOrText::Host(h)) => {
                        let history = histories.entry(path.clone()).or_default();
                        for (old, new) in evolve(&mut rng, h, history) {
                            renames.push((path.clone(), old, new));
                        }
                    }
                    None if path != NOTES && rng.chance(500) => {
                        *slot = Some(HostOrText::Host(stash[&path].clone()));
                    }
                    Some(HostOrText::Text(_)) if rng.chance(150) => *slot = None,
                    Some(HostOrText::Text(v)) if rng.chance(300) => *v = (*v + 1) % 3,
                    None if path == NOTES && rng.chance(500) => {
                        *slot = Some(HostOrText::Text(rng.below(3) as u8));
                    }
                    _ => {}
                }
            }
        }
        // An uncommitted step keeps HEAD; otherwise the worktree is committed.
        let uncommitted = step > 0 && rng.chance(100);
        if !uncommitted {
            committed = live.clone();
        }
        let unavailable = step > 0 && rng.chance(60);
        let mut register_files = Vec::new();
        let mut register_symbols = Vec::new();
        if !unavailable && !uncommitted {
            for (path, v) in &live {
                let wanted = if path == NOTES {
                    step == 0 && rng.chance(700) || rng.chance(80)
                } else {
                    step == 0 && rng.chance(300) || rng.chance(30)
                };
                if wanted && v.is_some() && registered_files.insert(path.clone()) {
                    register_files.push(path.clone());
                }
                if let Some(HostOrText::Host(h)) = v {
                    for (name, s) in &h.symbols {
                        let wanted = step == 0 && rng.chance(650) || rng.chance(100);
                        let key = (path.clone(), *name);
                        if wanted && !s.error && !registered_symbols.contains(&key) {
                            registered_symbols.insert(key.clone());
                            register_symbols.push(key);
                        }
                    }
                    let key = (path.clone(), group_name(h.ext));
                    if (step == 0 && rng.chance(400) || rng.chance(50))
                        && registered_symbols.insert(key.clone())
                    {
                        register_symbols.push(key);
                    }
                }
            }
        }
        steps.push(Step {
            committed: committed.clone(),
            live: live.clone(),
            register_files,
            register_symbols,
            unsupported: rng.chance(40).then(|| rng.pick(&UNSUPPORTED)),
            hash_path: rng.chance(40).then(|| rng.pick(&HASH_PATHS)),
            unavailable,
            race: !unavailable && step > 0 && rng.chance(70),
            renames,
        });
    }
    steps
}
fn format_of(seed: u64) -> ObjectFormat {
    if seed.is_multiple_of(50) {
        ObjectFormat::Sha256
    } else {
        ObjectFormat::Sha1
    }
}

// ------------------------------------------------------------------- Git --

type Trees = BTreeMap<String, Vec<(String, String, Vec<u8>)>>;
/// path → blob OID.
type Manifest = BTreeMap<String, Vec<u8>>;
struct GitHistories {
    roots: Vec<std::path::PathBuf>,
    /// (seed, step) → (HEAD commit id, HEAD manifest, worktree manifest).
    commits: BTreeMap<(u64, usize), (Vec<u8>, Manifest, Manifest)>,
    /// blob OID → bytes, as Git stores them.
    blobs: BTreeMap<Vec<u8>, Vec<u8>>,
}
fn unhex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn emit_commit(input: &mut Vec<u8>, refname: &str, mark: usize, message: &str, world: &World) {
    input.extend_from_slice(
        format!(
            "commit {refname}\ncommitter T <t@example.invalid> {mark} +0000\ndata {}\n{message}deleteall\n",
            message.len()
        )
        .as_bytes(),
    );
    for (path, v) in world {
        if let Some(v) = v {
            let body = bytes_of(v);
            input.extend_from_slice(
                format!("M 100644 inline {path}\ndata {}\n", body.len()).as_bytes(),
            );
            input.extend_from_slice(&body);
            input.push(b'\n');
        }
    }
    input.push(b'\n');
}
impl GitHistories {
    fn new(plans: &BTreeMap<u64, Vec<Step>>) -> Self {
        // The smoke and full-count tests run concurrently in one process.
        // Give every history its own directory instead of deleting a peer's.
        static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run_id = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut out = Self {
            roots: Vec::new(),
            commits: BTreeMap::new(),
            blobs: BTreeMap::new(),
        };
        for (format, oid_len) in [("sha1", 20), ("sha256", 32)] {
            let root = std::env::temp_dir().join(format!(
                "p5b-prop12-model-git-{}-{run_id}-{format}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let deadline = Instant::now() + Duration::from_secs(1200);
            let init = format!("--object-format={format}");
            assert!(
                git(&root, &["init", "-q", &init], &[], 1 << 16, deadline)
                    .unwrap()
                    .0
            );
            let mut input = Vec::new();
            let mut mark = 0;
            for (seed, steps) in plans {
                if (format_of(*seed) == ObjectFormat::Sha256) != (format == "sha256") {
                    continue;
                }
                for (k, s) in steps.iter().enumerate() {
                    mark += 1;
                    let message = format!("head {seed} {k}\n");
                    emit_commit(&mut input, "refs/heads/main", mark, &message, &s.committed);
                    if s.live != s.committed {
                        mark += 1;
                        let message = format!("live {seed} {k}\n");
                        emit_commit(&mut input, "refs/heads/worktree", mark, &message, &s.live);
                    }
                }
            }
            assert!(
                git(
                    &root,
                    &["fast-import", "--quiet"],
                    &input,
                    1 << 20,
                    deadline
                )
                .unwrap()
                .0
            );
            let (ok, objects) = git(
                &root,
                &["cat-file", "--batch-all-objects", "--batch"],
                &[],
                1 << 31,
                deadline,
            )
            .unwrap();
            assert!(ok);
            let mut trees: Trees = BTreeMap::new();
            let mut heads: BTreeMap<(u64, usize), (Vec<u8>, String)> = BTreeMap::new();
            let mut lives: BTreeMap<(u64, usize), String> = BTreeMap::new();
            let mut cursor = 0;
            while cursor < objects.len() {
                let end = cursor + objects[cursor..].iter().position(|b| *b == b'\n').unwrap();
                let header = std::str::from_utf8(&objects[cursor..end])
                    .unwrap()
                    .to_owned();
                let fields: Vec<_> = header.split(' ').collect();
                let n: usize = fields[2].parse().unwrap();
                let body = &objects[end + 1..end + 1 + n];
                cursor = end + 2 + n;
                match fields[1] {
                    "tree" => {
                        let mut entries = Vec::new();
                        let mut at = 0;
                        while at < body.len() {
                            let nul = at + body[at..].iter().position(|b| *b == 0).unwrap();
                            let (mode, name) = std::str::from_utf8(&body[at..nul])
                                .unwrap()
                                .split_once(' ')
                                .unwrap();
                            let oid = body[nul + 1..nul + 1 + oid_len].to_vec();
                            entries.push((mode.to_owned(), name.to_owned(), oid));
                            at = nul + 1 + oid_len;
                        }
                        trees.insert(fields[0].to_owned(), entries);
                    }
                    "commit" => {
                        let text = std::str::from_utf8(body).unwrap();
                        let tree = text.lines().next().unwrap()["tree ".len()..].to_owned();
                        let message: Vec<_> = text
                            .split_once("\n\n")
                            .unwrap()
                            .1
                            .split_whitespace()
                            .collect();
                        let key = (message[1].parse().unwrap(), message[2].parse().unwrap());
                        if message[0] == "head" {
                            heads.insert(key, (unhex(fields[0]), tree));
                        } else {
                            lives.insert(key, tree);
                        }
                    }
                    "blob" => {
                        out.blobs.insert(unhex(fields[0]), body.to_vec());
                    }
                    other => panic!("unexpected {other}"),
                }
            }
            fn walk(trees: &Trees, tree: &str, prefix: &str, out: &mut Manifest) {
                for (mode, name, oid) in &trees[tree] {
                    let path = format!("{prefix}{name}");
                    match mode.as_str() {
                        "40000" => walk(trees, &hex(oid), &format!("{path}/"), out),
                        "100644" => {
                            out.insert(path, oid.clone());
                        }
                        other => panic!("mode {other}"),
                    }
                }
            }
            for (key, (commit, tree)) in heads {
                let mut head = Manifest::new();
                walk(&trees, &tree, "", &mut head);
                let live = match lives.get(&key) {
                    Some(t) => {
                        let mut m = Manifest::new();
                        walk(&trees, t, "", &mut m);
                        m
                    }
                    None => head.clone(),
                };
                out.commits.insert(key, (commit, head, live));
            }
            out.roots.push(root);
        }
        assert_eq!(out.commits.len(), plans.len() * STEPS);
        out
    }
}
impl Drop for GitHistories {
    fn drop(&mut self) {
        for root in &self.roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

// ------------------------------------------------------- SPEC-form oracle --

fn wire(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}
/// ADR 0011 §2.
fn file_descriptor(repo: &str, path: &str, format: ObjectFormat, oid: &[u8]) -> Vec<u8> {
    let mut out = b"CTXANC01".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    wire(&mut out, repo.as_bytes());
    wire(&mut out, path.as_bytes());
    out.push(match format {
        ObjectFormat::Sha1 => 1,
        ObjectFormat::Sha256 => 2,
    });
    wire(&mut out, oid);
    out
}
/// ADR 0012 §4: language code and normaliser version, spelled out.
fn language_of(path: &str) -> (u8, &'static str) {
    match path.rsplit_once('.').unwrap().1 {
        "rs" => (1, "rust:tree-sitter-rust=0.24.2;tree-sitter=0.27.1;walk=3"),
        "ts" => (
            2,
            "typescript:tree-sitter-typescript=0.23.2;tree-sitter=0.27.1;walk=3",
        ),
        "tsx" => (
            3,
            "tsx:tree-sitter-typescript=0.23.2;tree-sitter=0.27.1;walk=3",
        ),
        "js" => (
            4,
            "javascript:tree-sitter-javascript=0.25.0;tree-sitter=0.27.1;walk=3",
        ),
        "py" => (
            5,
            "python:tree-sitter-python=0.25.0;tree-sitter=0.27.1;walk=3",
        ),
        "go" => (6, "go:tree-sitter-go=0.25.0;tree-sitter=0.27.1;walk=3"),
        other => panic!("{other}"),
    }
}
fn symbol_descriptor(repo: &str, path: &str, symbol: &str, hash: &[u8], masked: &[u8]) -> Vec<u8> {
    let (code, version) = language_of(path);
    let mut out = b"CTXANC02".to_vec();
    out.extend_from_slice(&1u32.to_le_bytes());
    wire(&mut out, repo.as_bytes());
    wire(&mut out, path.as_bytes());
    wire(&mut out, symbol.as_bytes());
    out.push(code);
    wire(&mut out, version.as_bytes());
    wire(&mut out, hash);
    wire(&mut out, masked);
    out
}
/// ADR 0012 §3 key encoding for the names used here.
fn symbol_key(scope: &str, path: &str, symbol: &str) -> String {
    let mut encoded = String::new();
    for c in symbol.chars() {
        match c {
            'A'..='Z' => {
                encoded.push('-');
                encoded.push(c.to_ascii_lowercase());
            }
            '_' => encoded.push('.'),
            c => encoded.push(c),
        }
    }
    format!("{scope}/{path}#{encoded}")
}
/// One position entry: tag and value (ADR 0011 §4, ADR 0012 §6).
type Entry = (u8, Vec<u8>);
/// ADR 0011 §4 / ADR 0012 §6: `CTXPOS01` without symbol anchors, else
/// `CTXPOS02` with the sorted normaliser versions of the languages present.
fn spec_position(
    repo: &str,
    head: &[u8],
    entries: &BTreeMap<String, Entry>,
    versions: &BTreeSet<&str>,
) -> Vec<u8> {
    let mut out = if versions.is_empty() {
        b"CTXPOS01".to_vec()
    } else {
        b"CTXPOS02".to_vec()
    };
    out.extend_from_slice(&1u32.to_le_bytes());
    wire(&mut out, repo.as_bytes());
    wire(&mut out, head);
    if !versions.is_empty() {
        out.extend_from_slice(&(versions.len() as u32).to_le_bytes());
        for v in versions {
            wire(&mut out, v.as_bytes());
        }
    }
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (path, (tag, value)) in entries {
        wire(&mut out, path.as_bytes());
        out.push(*tag);
        wire(&mut out, value);
    }
    blake3::hash(&out).as_bytes().to_vec()
}

/// The observation Git implies for one anchor.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Implied {
    Present(Vec<u8>),
    Missing,
    Broken,
}
/// What history implies for one anchor key (ADR 0011 §5 table, per key).
#[derive(Default, Debug)]
struct Expected {
    seen: Vec<Vec<u8>>,
    head: Vec<u8>,
    base: BTreeMap<Vec<u8>, bool>,
    present: bool,
    breaks_opened: usize,
}
/// The oracle's reading of one capture, or `None` if the scan must be
/// unavailable (an edited host that does not parse where an anchored symbol
/// needs it, ADR 0012 §4).
struct Reading {
    anchors: BTreeMap<String, Implied>,
    position: Vec<u8>,
}
#[allow(clippy::too_many_arguments)]
fn read(
    git: &GitHistories,
    repo: &str,
    format: ObjectFormat,
    head_commit: &[u8],
    head: &Manifest,
    live: &Manifest,
    files: &BTreeSet<String>,
    symbols: &BTreeSet<(String, &'static str)>,
) -> Option<Reading> {
    let scope = format!("code:{repo}");
    let mut anchors = BTreeMap::new();
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    for path in files {
        let (implied, entry) = match live.get(path) {
            Some(oid) => {
                let d = file_descriptor(repo, path, format, oid);
                (Implied::Present(d.clone()), (1, d))
            }
            None => (Implied::Missing, (2, Vec::new())),
        };
        anchors.insert(format!("{scope}/{path}"), implied);
        entries.insert(path.clone(), entry);
    }
    let mut versions = BTreeSet::new();
    let mut parsed = BTreeMap::new();
    for (host, symbol) in symbols {
        versions.insert(language_of(host).1);
        if !entries.contains_key(host) {
            let entry = match live.get(host) {
                Some(oid) => (4, oid.clone()),
                None => (2, Vec::new()),
            };
            entries.insert(host.clone(), entry);
        }
        let implied = match live.get(host) {
            None => Implied::Missing,
            Some(oid) => {
                let file = parsed.entry(host.clone()).or_insert_with(|| {
                    let language = Language::of_path(host).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(30);
                    parse(language, &git.blobs[oid], deadline).unwrap()
                });
                match file.symbols.get(*symbol) {
                    Some(s) if !s.error => {
                        Implied::Present(symbol_descriptor(repo, host, symbol, &s.hash, &s.masked))
                    }
                    None if !file.has_error => Implied::Missing,
                    _ if head.get(host) != Some(oid) => return None,
                    _ => Implied::Broken,
                }
            }
        };
        anchors.insert(symbol_key(&scope, host, symbol), implied);
    }
    let position = spec_position(repo, head_commit, &entries, &versions);
    Some(Reading { anchors, position })
}

// ------------------------------------------------------------------- SUT --

struct Sut {
    db: Db,
    history: Vec<Inputs>,
    descriptors: BTreeMap<ContentHash, AnchorDescriptor>,
}
impl Sut {
    fn commit(&mut self, ops: Vec<Op>) -> Result<(), TxError> {
        let mut b = Batch::new(ops);
        b.channel = Channel::Observer;
        self.db.commit(&b).map(|_| ())?;
        self.history.push(self.db.inputs());
        Ok(())
    }
    /// What `AnchorState` serves: committed anchors by logical key.
    fn committed(&self, scope: &str) -> BTreeMap<String, (AnchorDescriptor, AnchorState)> {
        let i = self.db.inputs();
        let mut out = BTreeMap::new();
        for (l, logical) in i.logicals.iter().enumerate() {
            if logical.kind != RecordKind::CodeAnchor || logical.scope_lane != scope {
                continue;
            }
            let head = i.heads[l];
            let content = i.revisions[head.index()].content;
            let d = self.descriptors[&content].clone();
            let breaks = i
                .artifacts
                .iter()
                .enumerate()
                .filter(|(_, a)| {
                    a.open
                        && a.ty == ArtifactType::AnchorBroken
                        && i.revisions[a.target.index()].logical.index() == l
                })
                .map(|(k, _)| ArtifactId(k as u32))
                .collect();
            let state = AnchorState {
                logical: LogicalId(l as u32),
                head,
                content,
                oid: d.match_id().to_vec(),
                base: i.base[head.index()],
                breaks,
            };
            out.insert(logical.key.clone(), (d, state));
        }
        out
    }
}
/// The observer's capture of a worktree: entries for every target path and
/// the kept host bytes, from the plan (the SUT never reads the Git oracle).
fn capture(
    repo: &str,
    format: ObjectFormat,
    world: &World,
    paths: &BTreeSet<String>,
    hosts: &BTreeSet<String>,
) -> (BTreeMap<String, Observation>, BTreeMap<String, Vec<u8>>) {
    let mut entries = BTreeMap::new();
    let mut bytes = BTreeMap::new();
    for path in paths {
        match world.get(path).and_then(Option::as_ref) {
            Some(v) => {
                let content = bytes_of(v);
                let d = Descriptor {
                    repo: repo.to_owned(),
                    path: path.clone(),
                    format,
                    oid: blob_oid(format, &content).unwrap(),
                };
                entries.insert(path.clone(), Observation::Present(d));
                if hosts.contains(path) {
                    bytes.insert(path.clone(), content);
                }
            }
            None => {
                entries.insert(path.clone(), Observation::Missing);
            }
        }
    }
    (entries, bytes)
}
/// The daemon's parse-free live position for the committed targets.
fn daemon_position(
    repo: &str,
    format: ObjectFormat,
    commit: &[u8],
    world: &World,
    t: &contextdb_symbols::observer::Targets,
) -> Vec<u8> {
    let hosts: BTreeSet<String> = t.1.values().map(|(h, _)| h.clone()).collect();
    let paths: BTreeSet<String> = t.0.union(&hosts).cloned().collect();
    let (entries, _) = capture(repo, format, world, &paths, &hosts);
    let versions =
        t.1.values()
            .map(|(h, _)| {
                let l = Language::of_path(h).unwrap();
                (l, normaliser_version(l))
            })
            .collect();
    let hosts_only = hosts.difference(&t.0).cloned().collect();
    live_position(repo, commit, &entries, &hosts_only, &versions)
}

#[derive(Default)]
struct Coverage {
    transactions: usize,
    delete: usize,
    same_restore: usize,
    old_head_aba: usize,
    cas_retry: usize,
    lag: usize,
    unavailable: usize,
    unsupported: usize,
    sha256: usize,
    formatting: usize,
    comment: usize,
    body: usize,
    outside: usize,
    /// A same-file item the symbol uses changed (walk 3).
    context: usize,
    rename: usize,
    grouping: usize,
    hash_path: usize,
    case_alias: usize,
    restore: usize,
    syntax_error: usize,
    mixed: usize,
    language: [usize; 4],
}

fn host_model<'w>(world: &'w World, path: &str) -> Option<&'w HostModel> {
    match world.get(path) {
        Some(Some(HostOrText::Host(h))) => Some(h),
        _ => None,
    }
}

fn run_seed(seed: u64, steps: &[Step], git: &GitHistories, cov: &mut Coverage) {
    let format = format_of(seed);
    let repo = format!("{seed:032x}");
    let scope = format!("code:{repo}");
    let mut sut = Sut {
        db: Db::default(),
        history: vec![Inputs::default()],
        descriptors: BTreeMap::new(),
    };
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut symbols: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut expected: BTreeMap<String, Expected> = BTreeMap::new();
    // Oracle 1: meaning → content per symbol key.
    let mut by_meaning: BTreeMap<String, BTreeMap<Vec<u8>, ContentHash>> = BTreeMap::new();
    let mut last_scan: Option<(usize, Vec<u8>)> = None;
    let mut race = 0u8;
    for (k, step) in steps.iter().enumerate() {
        cov.transactions += 1;
        if format == ObjectFormat::Sha256 {
            cov.sha256 += 1;
        }
        let families: BTreeSet<usize> = step
            .live
            .keys()
            .filter(|p| *p != NOTES)
            .map(|p| family(p.rsplit_once('.').unwrap().1))
            .collect();
        for f in &families {
            cov.language[*f] += 1;
        }
        if families.len() > 1 {
            cov.mixed += 1;
        }
        let (commit, head, live) = &git.commits[&(seed, k)];
        // Git stores exactly the bytes the plan renders.
        for (world, manifest) in [(&step.committed, head), (&step.live, live)] {
            for (path, v) in world {
                let want = v.as_ref().map(|v| blob_oid(format, &bytes_of(v)).unwrap());
                assert_eq!(
                    manifest.get(path),
                    want.as_ref(),
                    "seed {seed} step {k} {path}"
                );
            }
        }
        // Path-domain refusals change nothing.
        if let Some(bad) = step.unsupported {
            assert!(validate_path(&repo, bad).is_err(), "{bad:?} accepted");
            cov.unsupported += 1;
        }
        if let Some(bad) = step.hash_path {
            assert!(validate_path(&repo, bad).is_err(), "{bad:?} accepted");
            cov.hash_path += 1;
        }

        let next_files: BTreeSet<String> = files
            .iter()
            .cloned()
            .chain(step.register_files.iter().cloned())
            .collect();
        let next_symbols: BTreeSet<(String, &'static str)> = symbols
            .iter()
            .cloned()
            .chain(step.register_symbols.iter().cloned())
            .collect();
        let reading = read(
            git,
            &repo,
            format,
            commit,
            head,
            live,
            &next_files,
            &next_symbols,
        );

        // The SUT's scan of the live worktree.
        let added = Added {
            files: step.register_files.iter().cloned().collect(),
            symbols: step
                .register_symbols
                .iter()
                .map(|(h, s)| (h.clone(), (*s).to_owned()))
                .collect(),
        };
        let attempt = |sut: &Sut| {
            let committed = sut.committed(&scope);
            let t = targets(&scope, &committed, &added, Mode::Scan).unwrap();
            let hosts: BTreeSet<String> = t.1.values().map(|(h, _)| h.clone()).collect();
            let paths: BTreeSet<String> = t.0.union(&hosts).cloned().collect();
            let (entries, bytes) = capture(&repo, format, &step.live, &paths, &hosts);
            let mut edited = |host: &str, content: &[u8]| {
                let at_head = step
                    .committed
                    .get(host)
                    .and_then(Option::as_ref)
                    .map(bytes_of);
                Ok::<_, contextdb_anchors::Unavailable>(at_head.as_deref() != Some(content))
            };
            let prior = committed.into_iter().map(|(k, (_, s))| (k, s)).collect();
            let daemon = daemon_position(&repo, format, commit, &step.live, &t);
            prepare_scan(
                &repo,
                &t,
                commit,
                &entries,
                &bytes,
                prior,
                sut.db.watermark(&scope).map(<[u8]>::to_vec),
                &mut edited,
                Instant::now() + Duration::from_secs(30),
            )
            .map(|p| (p, daemon))
        };

        if step.unavailable || reading.is_none() {
            cov.unavailable += 1;
            if !step.unavailable {
                // An edited host that does not parse: the scan refuses.
                let before = sut.db.inputs();
                assert!(
                    attempt(&sut).is_err(),
                    "seed {seed} step {k}: edited unparseable host accepted"
                );
                assert_eq!(before, sut.db.inputs());
                cov.syntax_error += 1;
            }
            // Freshness against the live worktree: Unknown iff it moved.
            if let Some((_, last_position)) = &last_scan {
                let committed = sut.committed(&scope);
                let t = targets(&scope, &committed, &Added::default(), Mode::Scan).unwrap();
                let live_pos = daemon_position(&repo, format, commit, &step.live, &t);
                if let Some(r) = read(git, &repo, format, commit, head, live, &files, &symbols) {
                    assert_eq!(
                        live_pos, r.position,
                        "seed {seed} step {k}: daemon vs SPEC position"
                    );
                }
                let entries: Vec<_> = committed
                    .values()
                    .filter(|(_, a)| a.base)
                    .map(|(_, a)| CertItem {
                        logical: a.logical,
                        rev: a.head,
                        required: true,
                        via: Via::Head,
                        since_epoch: sut.db.epoch(),
                    })
                    .collect();
                if !entries.is_empty() {
                    let fresh = sut.db.inputs().watermarks;
                    let request = IssueRequest {
                        session_id: [1; 16],
                        principal: PrincipalId(0),
                        branch: MAIN,
                        log_checksum: [0; 32],
                        code_commit: String::new(),
                        strict_no_aba: seed.is_multiple_of(2),
                    };
                    let cert = sut
                        .db
                        .issue_certificate(&request, &entries, &fresh)
                        .unwrap();
                    let live_map = BTreeMap::from([(scope.clone(), live_pos.clone())]);
                    let link = LogLink {
                        matches: true,
                        current_checksum: [0; 32],
                    };
                    let actual = sut.db.validate_certificate(&cert, &live_map, link).unwrap();
                    assert_eq!(
                        actual,
                        contextdb_oracle::cert_validate_oracle(
                            &sut.history,
                            &cert,
                            &live_map,
                            true,
                            [0; 32]
                        )
                    );
                    let moved = live_pos != *last_position;
                    assert_eq!(
                        moved,
                        matches!(actual, Validation::Unknown { .. }),
                        "seed {seed} step {k}: lag"
                    );
                    if moved {
                        cov.lag += 1;
                    }
                }
            }
            continue;
        }
        let reading = reading.unwrap();
        let (mut prepared, daemon) =
            attempt(&sut).unwrap_or_else(|e| panic!("seed {seed} step {k}: {e}"));
        assert_eq!(
            daemon, reading.position,
            "seed {seed} step {k}: daemon vs SPEC position"
        );
        if step.race {
            race += 1;
            sut.commit(vec![Op::ObserverWatermark {
                scope: scope.clone(),
                expected: sut.db.watermark(&scope).map(<[u8]>::to_vec),
                position: vec![race],
            }])
            .unwrap();
            let before = sut.db.inputs();
            assert!(
                sut.commit(prepared.ops.clone()).is_err(),
                "stale batch committed"
            );
            assert_eq!(before, sut.db.inputs(), "failed CAS is atomic");
            cov.cas_retry += 1;
            prepared = attempt(&sut).unwrap().0;
        }
        let payloads: Vec<String> = prepared
            .blobs
            .iter()
            .filter_map(|b| std::str::from_utf8(b).ok())
            .filter(|t| t.starts_with("anchor-broken:"))
            .map(str::to_owned)
            .collect();
        for blob in &prepared.blobs {
            if let Ok(d) = AnchorDescriptor::from_bytes(blob) {
                sut.descriptors.insert(d.content(), d);
            }
        }
        if !prepared.ops.is_empty() {
            sut.commit(prepared.ops).unwrap();
        }
        assert_eq!(
            sut.db.derived(),
            &contextdb_oracle::jacobi(sut.history.last().unwrap())
        );

        // Oracle 2: advance every registered anchor from Git alone.
        let fresh_keys: BTreeSet<String> = step
            .register_files
            .iter()
            .map(|p| format!("{scope}/{p}"))
            .chain(
                step.register_symbols
                    .iter()
                    .map(|(h, s)| symbol_key(&scope, h, s)),
            )
            .collect();
        let previous: BTreeMap<String, bool> = expected
            .iter()
            .map(|(k, e)| (k.clone(), e.present))
            .collect();
        let mut restored_symbol = false;
        let mut broke = false;
        for (key, implied) in &reading.anchors {
            let fresh = fresh_keys.contains(key);
            let e = expected.entry(key.clone()).or_default();
            match implied {
                Implied::Present(descriptor) => {
                    let symbol = key.contains('#');
                    if !fresh && !e.present && *descriptor == e.head {
                        cov.same_restore += 1;
                        restored_symbol |= symbol;
                    }
                    if !fresh && *descriptor != e.head && e.seen.contains(descriptor) {
                        cov.old_head_aba += 1;
                        restored_symbol |= symbol;
                    }
                    if !e.seen.contains(descriptor) {
                        e.seen.push(descriptor.clone());
                    }
                    e.head = descriptor.clone();
                    e.base.insert(descriptor.clone(), true);
                    e.present = true;
                }
                Implied::Missing | Implied::Broken => {
                    assert!(
                        !fresh,
                        "seed {seed} step {k}: registration of a missing anchor {key}"
                    );
                    assert!(
                        e.present || e.breaks_opened > 0,
                        "an anchor is registered present"
                    );
                    if e.present {
                        cov.delete += 1;
                        e.breaks_opened += 1;
                        e.base.insert(e.head.clone(), false);
                        broke |= *implied == Implied::Broken;
                    }
                    e.present = false;
                }
            }
        }
        if restored_symbol {
            cov.restore += 1;
        }
        if broke {
            cov.syntax_error += 1;
        }
        files = next_files;
        symbols = next_symbols;

        // Oracle 1: meaning classes, renames, formatting and comment no-ops.
        let i = sut.db.inputs();
        let committed = sut.committed(&scope);
        let mut kinds = BTreeSet::new();
        for (host, symbol) in &symbols {
            let key = symbol_key(&scope, host, symbol);
            let Some(h) = host_model(&step.live, host) else {
                continue;
            };
            let grouped = *symbol == group_name(h.ext);
            let meaning: Vec<u8> = if grouped {
                h.group.to_vec()
            } else {
                match h.symbols.get(symbol) {
                    // A used `FILL` is part of the meaning (walk 3).
                    Some(s) if !s.error => {
                        let mut m = s.meaning.to_vec();
                        m.push(if s.meaning[4] == 1 { h.filler } else { u8::MAX });
                        m
                    }
                    _ => continue,
                }
            };
            let (_, state) = &committed[&key];
            assert!(
                state.base,
                "seed {seed} step {k} {key}: present but not base"
            );
            let content = i.revisions[state.head.index()].content;
            let classes = by_meaning.entry(key.clone()).or_default();
            match classes.get(&meaning) {
                Some(c) => assert_eq!(
                    *c, content,
                    "seed {seed} step {k} {key}: equal meaning, other revision"
                ),
                None => {
                    assert!(
                        !classes.values().any(|c| *c == content),
                        "seed {seed} step {k} {key}: different meaning, same revision"
                    );
                    classes.insert(meaning.clone(), content);
                }
            }
            // What changed for this symbol since the last successful scan.
            if let Some((last, _)) = &last_scan
                && previous.get(&key) == Some(&true)
                && let Some(before) = host_model(&steps[*last].live, host)
            {
                if grouped {
                    if (before.group, before.group_layout) != (h.group, h.group_layout) {
                        kinds.insert("grouping");
                    }
                } else if let Some(b) = before.symbols.get(symbol) {
                    let s = h.symbols[symbol];
                    if b.meaning == s.meaning && !b.error {
                        if b.layout != s.layout {
                            kinds.insert("formatting");
                        }
                        if b.comment != s.comment {
                            kinds.insert("comment");
                        }
                    }
                    if b.meaning[0] != s.meaning[0] {
                        kinds.insert("body");
                    }
                    if b.meaning == s.meaning
                        && s.meaning[4] == 1
                        && !b.error
                        && before.filler != h.filler
                    {
                        kinds.insert("context");
                    }
                }
                let unchanged = if grouped {
                    (before.group, before.group_layout) == (h.group, h.group_layout)
                } else {
                    before.symbols.get(symbol) == h.symbols.get(symbol)
                };
                let uses_fill = !grouped && h.symbols[symbol].meaning[4] == 1;
                if unchanged
                    && render(before) != render(h)
                    && !(uses_fill && before.filler != h.filler)
                {
                    kinds.insert("outside");
                }
            }
        }
        if ["alpha", "Alpha"]
            .iter()
            .all(|n| symbols.iter().any(|(_, s)| s == n))
        {
            kinds.insert("case_alias");
        }
        // A plain rename lists the new name as a rebind candidate when the
        // old anchor's break opens in this scan.
        for (host, old, new) in &step.renames {
            let key = symbol_key(&scope, host, old);
            let Some((last, _)) = &last_scan else {
                continue;
            };
            if previous.get(&key) != Some(&true) || expected[&key].present {
                continue;
            }
            let h = host_model(&step.live, host).unwrap();
            let renamed = h.symbols[new];
            let label = format!("anchor-broken:{host}#{old}:");
            let payload = payloads
                .iter()
                .find(|p| p.starts_with(&label))
                .unwrap_or_else(|| panic!("seed {seed} step {k}: no break for {key}"));
            let was = host_model(&steps[*last].live, host).unwrap().symbols[old];
            if was.meaning == renamed.meaning && !renamed.error {
                let candidates = payload.rsplit_once("candidates=").unwrap().1;
                assert!(
                    candidates.split(',').any(|c| c == format!("{host}#{new}")),
                    "seed {seed} step {k}: rename {old} -> {new} not suggested: {payload}"
                );
                kinds.insert("rename");
            }
        }
        for kind in kinds {
            *match kind {
                "formatting" => &mut cov.formatting,
                "comment" => &mut cov.comment,
                "body" => &mut cov.body,
                "outside" => &mut cov.outside,
                "context" => &mut cov.context,
                "grouping" => &mut cov.grouping,
                "case_alias" => &mut cov.case_alias,
                _ => &mut cov.rename,
            } += 1;
        }

        // Oracle 2: compare the committed state with the reconstruction.
        assert_eq!(
            sut.db.watermark(&scope),
            Some(&reading.position[..]),
            "seed {seed} step {k}: watermark"
        );
        let anchors: Vec<_> = i
            .logicals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.kind == RecordKind::CodeAnchor)
            .collect();
        assert_eq!(
            anchors.len(),
            expected.len(),
            "seed {seed} step {k}: anchor count"
        );
        let hash = |d: &Vec<u8>| -> ContentHash { *blake3::hash(d).as_bytes() };
        for (l, logical) in anchors {
            let at = format!("seed {seed} step {k} {}", logical.key);
            let e = expected
                .get(&logical.key)
                .unwrap_or_else(|| panic!("{at}: unexpected key"));
            let revisions: Vec<RevisionId> = (0..i.revisions.len())
                .filter(|r| i.revisions[*r].logical.index() == l)
                .map(|r| RevisionId(r as u32))
                .collect();
            let contents: Vec<ContentHash> = revisions
                .iter()
                .map(|r| i.revisions[r.index()].content)
                .collect();
            let want: Vec<ContentHash> = e.seen.iter().map(hash).collect();
            assert_eq!(contents, want, "{at}: revisions");
            assert_eq!(
                i.revisions[i.heads[l].index()].content,
                hash(&e.head),
                "{at}: head"
            );
            for r in &revisions {
                let content = i.revisions[r.index()].content;
                let d = &e.seen[contents.iter().position(|c| *c == content).unwrap()];
                assert_eq!(i.base[r.index()], e.base[d], "{at}: base");
            }
            let breaks: Vec<_> = i
                .artifacts
                .iter()
                .filter(|a| {
                    a.ty == ArtifactType::AnchorBroken
                        && i.revisions[a.target.index()].logical.index() == l
                })
                .collect();
            assert_eq!(breaks.len(), e.breaks_opened, "{at}: breaks");
            assert_eq!(
                breaks.iter().filter(|a| a.open).count(),
                usize::from(!e.present),
                "{at}: open breaks"
            );
        }
        last_scan = Some((k, reading.position));
    }
}

fn run(seeds: std::ops::Range<u64>) -> Coverage {
    let plans: BTreeMap<u64, Vec<Step>> = seeds.map(|s| (s, plan(s))).collect();
    let git = GitHistories::new(&plans);
    let mut cov = Coverage::default();
    for (seed, steps) in &plans {
        run_seed(*seed, steps, &git, &mut cov);
    }
    cov
}

#[test]
fn model_prop12_symbols_10000_fixed_seeds() {
    let seeds = seeds();
    let cov = run(0..seeds);
    let t = cov.transactions;
    println!(
        "PROP12-symbols-model seeds={seeds} transactions={t} delete={} same_restore={} old_head_aba={} cas_retry={} lag={} unavailable={} unsupported={} sha256={} formatting={} comment={} body={} outside={} context={} rename={} grouping={} hash_path={} case_alias={} restore={} syntax_error={} mixed={} rust={} ts={} python={} go={}",
        cov.delete,
        cov.same_restore,
        cov.old_head_aba,
        cov.cas_retry,
        cov.lag,
        cov.unavailable,
        cov.unsupported,
        cov.sha256,
        cov.formatting,
        cov.comment,
        cov.body,
        cov.outside,
        cov.context,
        cov.rename,
        cov.grouping,
        cov.hash_path,
        cov.case_alias,
        cov.restore,
        cov.syntax_error,
        cov.mixed,
        cov.language[0],
        cov.language[1],
        cov.language[2],
        cov.language[3]
    );
    // Floors fixed by ADR 0012 §9 before the run, as % of transactions.
    for (name, count, floor) in [
        ("delete", cov.delete, 5),
        ("same_restore", cov.same_restore, 5),
        ("old_head_aba", cov.old_head_aba, 5),
        ("cas_retry", cov.cas_retry, 3),
        ("lag", cov.lag, 3),
        ("unavailable", cov.unavailable, 2),
        ("unsupported", cov.unsupported, 2),
        ("sha256", cov.sha256, 1),
        ("formatting", cov.formatting, 5),
        ("comment", cov.comment, 5),
        ("body", cov.body, 5),
        ("outside", cov.outside, 5),
        ("context", cov.context, 2),
        ("rename", cov.rename, 2),
        ("grouping", cov.grouping, 2),
        ("hash_path", cov.hash_path, 2),
        ("case_alias", cov.case_alias, 2),
        ("restore", cov.restore, 3),
        ("syntax_error", cov.syntax_error, 2),
        ("rust", cov.language[0], 10),
        ("ts", cov.language[1], 10),
        ("python", cov.language[2], 10),
        ("go", cov.language[3], 10),
    ] {
        assert!(
            seeds < SEEDS || count * 100 >= t * floor,
            "coverage {name}: {count} / {t} < {floor}%"
        );
    }
}

/// Every injected syntax error is fatal inside its span and local to it
/// (ADR 0012 §9: an injection without `ERROR`/`MISSING` in the span would not
/// count; here none may exist).
#[test]
fn model_injected_errors_are_inside_the_span() {
    for ext in ["rs", "ts", "tsx", "js", "py", "go"] {
        let language = Language::of_path(&format!("x.{ext}")).unwrap();
        for layout in 0..4 {
            for comment in 0..4 {
                let s = Sym {
                    meaning: [1, 1, 0, 0, 1],
                    layout,
                    comment,
                    error: true,
                };
                let mut h = HostModel {
                    ext,
                    symbols: BTreeMap::new(),
                    group: [0, 1],
                    group_layout: 0,
                    filler: 0,
                };
                h.symbols.insert("alpha", s);
                h.symbols.insert("beta", Sym { error: false, ..s });
                let src = render(&h);
                let deadline = Instant::now() + Duration::from_secs(5);
                let file = parse(language, &src, deadline).unwrap();
                let text = String::from_utf8_lossy(&src);
                assert!(
                    file.symbols.get("alpha").is_none_or(|a| a.error),
                    "{ext} {layout} {comment}: not fatal\n{text}"
                );
                assert!(
                    file.symbols.get("beta").is_some_and(|b| !b.error),
                    "{ext} {layout} {comment}: not local\n{text}"
                );
            }
        }
    }
}

/// Layout and comment variants never change a hash; meaning variants always
/// do, and so does the value of a used `FILL` (walk 3 context) but not of an
/// unused one (the generator's premise, checked on every rendering).
#[test]
fn model_renderer_premise() {
    for ext in ["rs", "ts", "tsx", "js", "py", "go"] {
        let language = Language::of_path(&format!("x.{ext}")).unwrap();
        let mut seen: BTreeMap<Vec<u8>, [u8; 32]> = BTreeMap::new();
        let attrs = if ext == "go" { 1 } else { 3 };
        let directives = if ext == "rs" { 1 } else { 3 };
        for b in 0..3 {
            for sig in 0..2 {
                for attr in 0..attrs {
                    for (directive, uses, filler) in (0..directives)
                        .flat_map(|d| (0..2).flat_map(move |u| (0..2).map(move |f| (d, u, f))))
                    {
                        let meaning = [b, sig, attr, directive, uses];
                        let key: Vec<u8> = meaning
                            .iter()
                            .copied()
                            .chain([if uses == 1 { filler } else { u8::MAX }])
                            .collect();
                        for layout in 0..4 {
                            for comment in 0..4 {
                                let mut h = HostModel {
                                    ext,
                                    symbols: BTreeMap::new(),
                                    group: [0, 1],
                                    group_layout: layout % 2,
                                    filler,
                                };
                                let s = Sym {
                                    meaning,
                                    layout,
                                    comment,
                                    error: false,
                                };
                                h.symbols.insert("alpha", s);
                                let src = render(&h);
                                let text = String::from_utf8_lossy(&src);
                                let deadline = Instant::now() + Duration::from_secs(5);
                                let file = parse(language, &src, deadline).unwrap();
                                assert!(!file.has_error, "{ext}: parse error\n{text}");
                                let hash = file.symbols["alpha"].hash;
                                match seen.get(&key) {
                                    Some(h) => assert_eq!(
                                        *h, hash,
                                        "{ext} {meaning:?} {layout} {comment}: layout or comment changed the hash\n{text}"
                                    ),
                                    None => {
                                        assert!(
                                            !seen.values().any(|h| *h == hash),
                                            "{ext} {meaning:?}: two meanings, one hash"
                                        );
                                        seen.insert(key.clone(), hash);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A short run for iteration; the gate run is the 10,000-seed test.
#[test]
fn model_prop12_symbols_smoke() {
    assert_eq!(run(0..200).transactions, 2000);
}
