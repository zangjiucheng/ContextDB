# ADR 0012 — P5b symbol-level code anchors

Status: **accepted**, revision 0.6, 2026-10-23. G0 passed after an independent
review, a closure check and a closure confirmation (§11). SPEC v1.6.5 mirrors it.
r0.4 records implementation findings: walk revision 2 from the T45 formatter
corpus, and four clarifications (§11). r0.5 records the G4 resource findings:
a container nesting bound and the deadline over extraction and walk (§4, §11).
r0.6 fixes the G6 rate definitions before labelling (§9, §11).

## 0. Scope and delivery

Implements SPEC §5 P5b and SD18 for symbols: a CodeAnchor can name one symbol
(function, method, type, …) inside a file, and its revision changes only when
that symbol's normalised syntax changes. Gate items: T45, T45b, T42b, T42d,
T42e, T46d, T46e (§10), PROP12 extended to symbols, and the G6 replay
comparison against file-level anchors.

P5b builds on ADR 0011 unchanged except where this ADR says so. Like P5a, it may
be implemented during the P4 dogfood (A11), but its gate cannot pass before
P5a's, which cannot pass before P4's. The live dogfood data and binaries are not
touched. All four languages ship together (SPEC lists TS/JS → Rust → Python → Go
as an order of preference, not of delivery).

Out of scope: semantic equivalence beyond syntax (renamed locals, reordered
items), cross-file resolution, macro expansion, symbols inside function bodies,
other languages, branch isolation (P6).

## 1. Crates and dependencies

A new crate `contextdb-symbols` owns tree-sitter, the grammars, symbol
extraction and normalisation. Only the observer side depends on it (the CLI's
`ctx anchors` and the benches). **`ctxd` never links a grammar**
(`cargo tree -p contextdb-daemon` must show no tree-sitter; G1). The daemon's
live path stays parse-free (§6), but the daemon still changes: every place that
turns an anchor key into a path learns the symbol form (§7).

Pinned dependencies (MIT unless noted), added to `config/dependency-policy.toml`
as a `[symbols]` section, with `contextdb-symbols` among the light crates:

| crate | version | why |
|---|---|---|
| `tree-sitter` | =0.27.1 | parser runtime (C, built with `cc`) |
| `tree-sitter-language` | =0.1.9 | grammar ABI shim |
| `tree-sitter-rust` | =0.24.2 | Rust |
| `tree-sitter-typescript` | =0.23.2 | TypeScript and TSX |
| `tree-sitter-javascript` | =0.25.0 | JavaScript |
| `tree-sitter-python` | =0.25.0 | Python |
| `tree-sitter-go` | =0.25.0 | Go |

Transitive: `regex`, `regex-syntax`, `regex-automata`, `aho-corasick`, `memchr`
(MIT OR Apache-2.0; tree-sitter's query engine, unused by us),
`streaming-iterator`; build-only `serde_json`, `indexmap`, `hashbrown`,
`equivalent`, `foldhash`, `itoa`, `serde_core`, `cc`, `find-msvc-tools`,
`shlex`. Grammar C sources are compiled from the crates, never fetched at build
time.

## 2. Languages and symbol tables

Language is chosen by extension only: `.rs` Rust; `.ts`, `.mts`, `.cts`
TypeScript; `.tsx` TSX; `.js`, `.mjs`, `.cjs`, `.jsx` JavaScript; `.py` Python;
`.go` Go. Other files carry file-level anchors only; registering a symbol in
one is an error.

A **symbol path** is a sequence of segments. Only declarations reachable
through item nesting are symbols; nothing inside a function body is.

- **Rust**: `fn`, `struct`, `enum`, `union`, `trait`, `type`, `const`,
  `static`, `macro_rules!`, `mod` (adds a segment). Trait items (methods with
  or without a default, associated types and consts) are `Trait::item`. Items
  of `impl T` are `T::item`; of `impl Tr for T` are `T#Tr::item`, where `Tr` is
  the **last segment** of the trait path (`fmt::Display` → `Display`), followed
  by `#` and the **key hash** of the trait's generic arguments when it has any
  (`impl From<u8> for T` → `T#From#3fa1c2d0::from`). The self type is its last
  path segment with generics dropped (`Foo<X>` → `Foo`); a self type that is
  not a path (`&T`, `dyn X`, tuples, a bare type parameter) is `#` plus the key
  hash of its source text.

  The **key hash** is frozen forever (SD18.1) and independent of the
  normaliser: the first 8 lowercase hex digits of BLAKE3 over the UTF-8 source
  text of the node with every ASCII whitespace character removed. A
  normaliser or grammar change never changes a key. Differently written but
  equivalent arguments (`io::Error` vs `std::io::Error`) give different keys; the
  rebind suggestion (§5) finds the counterpart. Raw identifiers drop `r#` (`r#type` is `type`).
- **TypeScript / TSX / JavaScript**: function declarations, classes and their
  members (`C::m`, private `#p` as `p` with a `#private` segment before it),
  every top-level `const`/`let`/`var` declarator whatever its initialiser
  (so `const X = memo(…)` is a symbol), TS `interface`, `type`, `enum`,
  `namespace`/`module` (adds a segment), and `export default` (segment
  `default` when anonymous).
- **Python**: `def`, `async def`, `class` at module level, in classes
  (`C::m`), and inside module-level `if`/`try`/`with` blocks (the block adds no
  segment).
- **Go**: `func`; methods `T::M` with the receiver's base type name (`*List[T]`
  → `List`); each `type`, `const` and `var` spec name.

**Same-path occurrences are grouped, never ambiguous.** All declarations in a
file with the same symbol path form one symbol whose span is all of them in
source order: overload signatures with their implementation, getter + setter,
static + instance members of one name, `@property` + `.setter`/`.deleter`,
`@overload` stubs, interface/namespace merging, conditional (`cfg`, `if`/`try`)
duplicates, repeated Go `func init()`. In a Go `const ( … )` group where any
spec omits its value (implicit `iota` repetition), every spec's span is the
whole group, so changing `A`'s expression changes `B`.

Identifiers outside `[A-Za-z0-9_$]` (e.g. non-ASCII) are unsupported: refused
at registration; an existing anchor whose symbol can no longer be named is
`Broken("unsupported-identifier")`.

## 3. Keys

Logical key: `code:<repo-id>/<path>#<encoded-symbol>` (SD18.1). Keys keep the
P5a domain (≤ 256 bytes; lowercase ASCII letters, digits, `._:/#-`), with this
reversible encoding:

| symbol text | encoded |
|---|---|
| `a`–`z`, `0`–`9` | itself |
| `A`–`Z` | `-` + the lowercase letter |
| `_` | `.` |
| `$` | `-.` |
| segment separator | `:` |
| markers (`#` trait, `#<hex8>` hash, `#private`) | as written (`#` + lowercase) |

`-` is always followed by a letter or `.`, identifiers never contain `#`, and
markers have fixed forms and positions (a key hash is exactly 8 lowercase hex
digits, only directly after the self type or the trait name; `#private` only
before a member segment), so decoding is unique. These forms are normative;
G2 includes round-trip goldens for `#<hex8>#Trait#<hex8>`, a trait whose name is
all hex digits, and `#private`.
Example: `MyType#Display::fmt` → `-my-type#-display:fmt`. A key over 256 bytes
is refused at registration.

**File paths may no longer contain `#`** (amends ADR 0011 §2). Registration
refuses them. A legacy file anchor whose path contains `#` is
`Broken("reserved-character")` on the next scan and can only be retired by
renaming the file; its key may block a symbol key that encodes identically
(e.g. legacy file `a#foo` vs symbol `foo` in `a`), in which case that symbol's
registration fails with `LogicalExists` and says why.

## 4. Normalisation and identity

The **span** of a symbol is its declaration node(s) (§2 grouping) plus
attached attributes: preceding Rust `attribute_item`s, Python
`decorated_definition`, TypeScript decorators. Headers of an enclosing item
(`impl` generics/where clauses, class decorators such as `@dataclass`) and
`use`/`import` statements are outside the span by design; G6 reports them as
expected sources of missed invalidation.

The **walk** is iterative (an explicit stack; no recursion) over the span in
pre-order. Each kept inner node emits an open marker with its kind name and,
after its kept children, a close marker; each kept leaf emits its kind and
text, all length-prefixed. This is injective and carries the same shape as
kind plus child count, but streams without counting children first (r0.4).
Directive comments directly preceding a declaration belong to its span. The rules, per language,
remove what formatters change and keep what changes meaning:

- **Comments are dropped**, except **directive comments**, which are kept as
  leaves: Go `//go:…`, `//export …`, `//line …`, `// +build …`; TS/JS
  comments containing `@ts-ignore`, `@ts-expect-error`, `@ts-nocheck`,
  `@ts-check`; in `.js`/`.jsx`/`.mjs`/`.cjs` every JSDoc block (`/** … */`,
  types in checked JS); Python `# type:` comments. Children are counted
  **after** dropping.
- **Optional punctuation is dropped**: trailing commas before a closing
  bracket, **except** when the list has exactly one element, where the comma
  changes meaning (Python 1-tuples and subscripts such as `a[1,]`; Rust tuple
  expressions, tuple types and tuple patterns such as `(u8,)`); `;` leaves in
  JS/TS and Go (automatic semicolon insertion / gofmt); Python
  `line_continuation`.
- **Grouping parentheses are transparent** in Python, JS and TS
  (`parenthesized_expression` emits only its inner node; precedence is already
  in the tree's shape), **except** when the inner expression contains an
  optional chain (`(a?.b).c` short-circuits differently from `a?.b.c`) or the
  parenthesised expression is a statement-level string (`("use strict")` is
  not a directive).
- **String delimiters are normalised**: a string literal emits its prefix
  flags (Python `r`/`b`/`f`/`u`, JS template vs plain) and its source content
  between the delimiters, not the quote characters, so `'a'` and `"a"` agree.
  Content that needed different escaping does differ (a safe false change).
  The rule covers every language's string literal kinds; raw and interpreted
  strings (Rust `r"…"`, Go backquotes) keep their distinct kind names.
- **Python docstrings count**: they are string statements visible at run time
  (`__doc__`), not comments. Rust doc comments (`///`, `//!`) are comments and
  do not count; `#[doc = …]` attributes do.
- **Formatter-equivalent forms agree** (walk revision 2, r0.4, found by the
  T45 corpus). Each is a pair of grammar forms one formatter switches between
  for the same meaning; each has a must-differ golden for its nearest
  meaning change:
  - Rust: a match arm's or closure's block around a single value
    (`=> { e }` / `=> e,`, `|x| { e }` / `|x| e`) emits the value; the arm
    comma is dropped; an expression statement around `return`/`break`/
    `continue`, and a block-like tail without `;`, emits the expression
    (rustfmt adds `;` after a diverging tail). `{ f(); }` still differs from
    `f()`.
  - Python: `expression_list`/`pattern_list` are emitted as `tuple`/
    `tuple_pattern`, whose parentheses are optional (`return a, b` /
    `return (a, b)`), as are `from m import (…)` parentheses; a one-element
    `tuple_pattern` without a comma is grouping (`for (k) in`); the type
    grammar is mapped onto the expression grammar black's parentheses
    produce (`type` and `type_parameter` transparent, `union_type` →
    `binary_operator`, `generic_type` → `subscript`, `member_type` →
    `attribute`); `case_pattern` is transparent. Single-element trailing
    commas stay meaningful on all of them.
  - TypeScript/JavaScript: a `string` is one leaf of its content with `\'`
    and `\"` unescaped (prettier switches quotes to avoid escapes); a quoted
    object key that is an identifier equals the unquoted key; a leading `|`/`&`
    on a multi-line union or intersection is dropped with the one-member
    wrapper it creates; one plain arrow parameter equals its parenthesised
    form (`x => x` / `(x) => x`, not `(x: T) => x`).

**T45 is defined by real formatters**, not by the generator: on a corpus of the
three G6 repositories' symbols, running rustfmt, prettier, black and gofmt with
default settings (and comment-only and doc-comment-only edits) must leave every
normalised hash unchanged, apart from listed, explained exceptions. The G6
repositories contain only Rust and Python, so pinned TypeScript, JavaScript
and Go repositories supplement them, and each formatter also runs with a
stress style (narrow width and its non-default options) so that already
formatted code is actually rewritten (r0.4; results in
`docs/gates/p5b-t45/`). Listed exceptions: rewrites inside Rust macro token
trees (no syntax tree to normalise), and docstring content black re-wraps (a
real `__doc__` change).

**Parse errors are local** (r0.1 review B1). A symbol's observation is Present
if its span and the chain of enclosing declarations it is nested in contain no
`ERROR` or `MISSING` node, wherever else the file has errors. A symbol that
cannot be found, or whose span or enclosing chain has errors, while the file
has errors, is:

- **unavailable** if the host file is tracked and modified relative to `HEAD`:
  the filtered hash of the captured worktree bytes
  (`git --no-optional-locks hash-object --path=<path> --stdin`, so clean/EOL
  filters and `autocrlf` apply) differs from the path's blob OID at `HEAD`
  (`git --no-optional-locks ls-tree`). No stat cache or mtime is consulted
  (SPEC §3.5) and Git never writes the index. Then the file is
  being edited, and the scope's watermark is not published, as for any P5a
  unavailable capture, until the edit parses;
- **`Broken("parse-error")`** otherwise: the file is clean with respect to
  `HEAD` (the committed file is unparseable by the pinned grammar, e.g. Go
  `const ( A = iota + 1; B )` on one line), or it is untracked, or `HEAD` is
  unborn. The base is retracted and an `AnchorBroken` opened, so the scope
  stays live. When the file or a later grammar parses, the same descriptor
  reasserts the same RevisionId. Documented consequence: a symbol in an
  untracked host file flips to Broken while that file is mid-edit and back
  when it parses (reversible, same revision).

**Resource bounds.** Parsing uses `parse_with_options` with a progress callback
that cancels at the observer deadline; extraction and the walk check the same
deadline every 1,024 nodes (r0.5); a cancelled parse is unavailable. Both are
iterative or bounded and linear in the tree: no step climbs to a parent or
rescans a subtree. **Containers nested deeper than 256 levels (the file
counts as one) hold no symbols**: extraction does not enter them, so an anchor
on such a symbol observes Missing (r0.5). A
scan parses each distinct host content once and at most 32 MiB of distinct
host-file bytes, checked before any parse;
beyond that the scan is unavailable. (r0.5: a whole scan — parse, extraction,
walk — costs ≈ 0.38 s per MiB on the G5 baseline, not the parse-only
≈ 9 ms per 100 KB, so the 5 s deadline binds at ≈ 13 MiB of distinct host
bytes, before this budget.) In `watch`, parse results are cached in memory by
(raw blob OID, normaliser version), never by mtime (SPEC §3.5). A crash inside
the C runtime ends the observer process before anything is submitted: nothing
is published and the scope lags (fail-closed).

**Normaliser versions** (r0.1 M3, r0.2 N2). A version string is derived
mechanically, per language, from the **exact grammar crate version and the
tree-sitter runtime version plus a walk revision number**
(e.g. `rust:tree-sitter-rust=0.24.2;tree-sitter=0.27.1;walk=2`). There is no
corpus exemption: any grammar, runtime or walk change is a new version, because
code outside any corpus may hash differently. A binary implements exactly one
version per language. The descriptor carries its version (below) and so does
the position (§6). A binary upgrade that changes a version therefore leaves
every scope with symbol anchors of that language unavailable until
`ctx anchors migrate` runs; `ctx anchors status` and the scan error say so
explicitly. The reference-corpus walk check (G2) only reports how many symbols
a version change re-revisions, to inform the upgrade decision. An ordinary `scan`/`watch`
that finds a committed head made with a different version than its own is
**unavailable** for that scope and says so; only `ctx anchors migrate` crosses
versions, re-observing every anchor (changed symbols get new revisions:
dependents see STALE for `Cur`, PENDING for `Sem`). Two observers of different
versions therefore cannot flip heads back and forth.

**Descriptor** (model::Wire, as ADR 0011 §2): `CTXANC02`, u32LE version = 1,
String repo-id, String path, String symbol (exact, decoded), u8 language
(1 Rust, 2 TypeScript, 3 TSX, 4 JavaScript, 5 Python, 6 Go), String normaliser
version, Vec<u8> normalised hash (32 bytes), Vec<u8> name-masked hash
(32 bytes, §5; a function of the same span, so it does not affect
deduplication). No trailing bytes, no noncanonical forms. ContentHash = BLAKE3(descriptor). Identical normalised
syntax reuses the same RevisionId (SD18.4); restoring earlier text restores
dependents. File OIDs are not in the descriptor, so edits elsewhere never
create revisions.

## 5. Observation and classification

The observer captures files as in P5a (two stable samples, raw bytes, all
bounds), parses each **host file** (a file with at least one registered symbol
anchor) once per sample, and derives each symbol's observation: Present
(descriptor), Missing (host file missing, or symbol absent from a file without
errors), unavailable or Broken per §4, or Broken for a symlink/non-regular host
file (P5a reasons). The observer's committed-state map is keyed by **logical
key**, not path, so several symbols in one file coexist. The P5a
classification table applies per symbol logical (SD18.5).

**Rebind suggestions.** When a symbol goes missing, its `AnchorBroken` lists
candidates whose **name-masked hash** (the normalised hash with the declared
name leaf replaced by a placeholder) equals the missing symbol's last one,
searched in the scope's host files (already parsed) and, best effort, in the
supported files of the same directory under a separate small budget (4 MiB) that
never makes the scan unavailable; when it runs out, fewer candidates are
listed. This finds plain renames and moves to
a sibling file; it is a suggestion only, never a rebind.

## 6. Positions and freshness (parse-free)

A symbol observation is a pure function of its host file's bytes and the pinned
normaliser, so the watermark summarises host files, not symbols. Positions
keep **one entry per path** (r0.1 review M2):

- a path registered as a file anchor: the P5a entry (tag 1/2/3), which also
  covers its role as a host;
- a host-only path: tag 4 = HostFile with the raw blob OID; tag 2 if missing;
  tag 3 with P5a's reason bytes if broken (symlink, non-regular).

A scope without symbol anchors encodes exactly as before (`CTXPOS01`), so P5a
watermarks stay valid. A scope with symbol anchors uses `CTXPOS02`: as
`CTXPOS01`, plus tag 4, plus a header listing the normaliser version of each
language present among its symbol anchors (sorted). The daemon samples host
files exactly as it samples file anchors and never parses. Consequence,
documented: any byte change in a host file, even outside every anchored symbol,
makes the scope lag (`PENDING(ObservationLag)` / `Unknown`) until the
observer's next scan (≈ 250 ms in `watch`); unchanged symbols are then fresh
with no new revision and no STALE.

The announce record (ADR 0011 §9) is unchanged.

## 7. Daemon, filter and range changes (r0.1 review M1)

Every place that maps an anchor key to a file takes the path **from the
committed descriptor**, never by splitting the key: a `CTXANC01` (file)
descriptor's path is the whole key path (a legacy path containing `#` stays one
file), and a `CTXANC02` (symbol) descriptor carries its host path. A host file
stands for all its symbol anchors. The daemon also derives the `CTXPOS02`
normaliser-version header from the committed descriptors (it cannot parse).

- **Live evidence** (`anchor_evidence`): groups registered **paths** (file
  anchors and host files, deduplicated) per scope; never opens `a.rs#sym`.
- **Path filtering** (`anchor_checks`): a target path selects the file anchor
  key **and every symbol key hosted in that file**; matching is on the decoded
  host path, never on key substrings. (A literal implementation of r0.1 would
  have let an Edit to `src/lib.rs` skip decisions resting on `src/lib.rs#…`.)
- **Range checks** (`check_anchors`): a changed path selects its file anchor
  and its hosted symbol anchors.
- **Pre-push**: registered host files must match the pushed commit like
  registered files.
- **Observer state** (`AnchorState`): unchanged on the wire (it already carries
  descriptor bytes); clients key it by logical key.

No kernel Op, protocol message or session-log event changes; the protocol stays
at 3. A P5a daemon cannot validate `CTXANC02` descriptors and reports such
scopes Unknown (fail-closed); mixed versions are unsupported, as before.

## 8. CLI

- `ctx anchors add <dir> <repo> <path>#<symbol>` registers a symbol written
  with its real spelling (`src/lib.rs#Store::open`, `src/x.rs#T#From#3fa1c2d0::from`);
  it must exist in the file. On a miss the command lists the file's symbols.
- `ctx anchors symbols <repo> <path>` lists a file's symbols with their keys.
- `ctx anchors status` shows symbol anchors with their decoded symbols and any
  legacy `#` file paths.
- `ctx anchors migrate <dir> <repo>` crosses a normaliser version (§4) and
  reports how many anchors changed revision.

## 9. Verification

- **G1**: standard checks; `contextdb-symbols` under the light-client rule;
  `ctxd` grammar-free.
- **G2**: T45 (formatter corpus, §4), T45b (directive comments, attributes,
  Python docstrings, string-delimiter normalisation, Python block moves), T42b
  (edits outside the span: no new revision, lag until the scan, then fresh),
  T42d (parse errors: local Present, unavailable while edited, Broken when
  committed, recovery reasserts the same revision), T42e (normaliser-version
  mismatch refused; migrate crosses), T46d (an Edit to a host file selects its
  symbol roots), T46e (range and pre-push select symbol anchors), must-differ
  goldens for `a[1,]`/`a[1]`, `(u8,)`/`(u8)`, `(a?.b).c`/`a?.b.c` and
  `("use strict")`, legacy `#` file anchors mapped by descriptor, goldens per
  language for every §2 form and grouping, key encoding round trips, `#` path
  refusal, legacy `#` key collision, rebind suggestions, and the reference-corpus
  walk check for normaliser versions.
- **G3**: PROP12 extended. 10,000 fixed seeds generate repositories mixing
  file and symbol anchors (including both on one host file) in all four
  languages from a symbol model: insert, body edit, signature edit,
  formatting-only, comment-only, directive-comment edit, attribute edit, rename,
  same-path grouping, delete, restore earlier text, edits outside spans,
  syntax errors constructed to be fatal inside the span (each injection checked
  to produce `ERROR`/`MISSING` in the span; otherwise not counted). Two oracles:
  the **generator's model** for which symbols changed meaning, and the **SPEC
  form** — recomputation from the rendered Git history (fast-import, cat-file)
  with independently re-encoded descriptors and positions, including watermark
  equality. Coverage floors (share of transactions): all P5a floors (delete,
  identical restore, old-head ABA ≥ 5 %; CAS retry, lag ≥ 3 %; unavailable,
  unsupported paths ≥ 2 %; SHA-256 ≥ 1 %), plus formatting-only, comment-only,
  body edit, outside-span edit ≥ 5 % each; rename, grouping, `#`-path refusal,
  case alias ≥ 2 %; restore ≥ 3 %; syntax error ≥ 2 %; each language ≥ 10 %.
  Mutation of the normaliser and symbol extraction; survivors killed or
  explained.
- **G4**: as P5a, plus host files that stay broken while edited (scope lags,
  nothing retracted) and when committed (Broken, scope live), deep and large
  trees within bounds, parse cancellation at the deadline, the parse-byte
  budget, and an observer crash mid-scan (nothing published).
- **G5**: P4 hook budgets unchanged (no parse on the live path); observer scan
  cost reported for 10/100/1,000 symbol anchors over host files up to the size
  bound.
- **G6** (SPEC §5 P5b, agent-labelled per SPEC §4.4, owner rule v1.6.5): the
  same three repositories × 1,000 commits. **Symbol selection rule**: in each
  repository's P5a anchor files written in a supported language, the
  observer's symbols at the replay's first commit sorted by
  `git hash-object("<path>#<symbol>")`, the first 5 per file (all if fewer).
  **Pairs**: every (symbol, commit) where the host file or a same-directory
  file changed. **Sample**: stratified by repository × file-level fired ×
  symbol-level fired, 35 per stratum (all if fewer), first by
  `git hash-object("<commit>:<path>#<symbol>")`, selection fixed before
  labelling. Label: "does this commit invalidate a decision resting on this
  symbol?". Both predictors are scored on the same labelled pairs — file level
  (host file changed), symbol level (normalised hash changed, or Broken).
  Estimates are **stratum-weighted** (each stratum weighted by its population
  size over its sample size) **point estimates**; 95 % intervals are reported
  beside them, and the conditional miss rate among host-changed positives too.
  **Thresholds**: symbol false invalidation ≤ 10 % (initial) and symbol missed
  invalidation ≤ file missed invalidation + 2 percentage points (initial; owner
  decision 2026-10-22, SPEC v1.6.5). The per-repository rate of host files
  with parse errors is reported. Documented properties of this design: the
  stratum (file not fired, symbol fired) is empty by construction, so about
  three strata per repository are sampled (≈ 315 labels); and same-directory
  pairs mostly add (neither fired) positives that both predictors miss alike,
  so the +2 pp threshold depends on how broad the candidate set is — the
  conditional miss rate among host-changed positives is reported beside it for
  that reason. **Rates** (fixed before labelling, r0.6): a predictor's false
  invalidation is the weighted share of its fired pairs that do not
  invalidate (FP / fired); its missed invalidation is the weighted share of
  invalidating pairs it did not fire (FN / positives). Symbol level fires on
  a new head or a break; restoring the same revision is not a firing. Symbols
  are selected at the P5a selection commit (the replay's parent of its first
  commit) from those without a parse error. Intervals are stratified
  percentile bootstrap, 10,000 resamples, fixed seed; `unsure` labels are
  excluded and counted. Tool: `symbol_replay` (bench).
- **G7**: P0–P5a regressions. **G8**: README, semantics, CHANGELOG, status.

## 10. SPEC mirror (v1.6.5)

SD18.1 (symbol key encoding and the `#` path rule), SD18.2 (descriptor v2,
normaliser versions, migration), SD18.5 (grouping instead of ambiguity, the
local parse-error rule), SD14b (host-file positions, `CTXPOS02`), §5 P5b G6
(+2 pp tolerance, weighted point estimates, owner decision), and the T-matrix:
T45 (formatter corpus), T45b, T42b, T42d, T42e, T46d, T46e.

## 11. Review log

**r0.1 G0 review (2026-10-22)**: did not pass — 1 blocker, 8 majors, minors.
r0.2 addresses them:

- B1 a scope could lag forever on a file the pinned grammar cannot parse →
  local parse-error rule; Broken when the unparseable bytes are committed (§4).
- M1 key→path sites in the daemon, filter, range and observer would fail open
  → §7 enumerates them; T46d, T46e.
- M2 a file anchor that is also a host had two position entries; host Broken
  undefined → one entry per path; tags defined (§6).
- M3 observers at different normaliser versions would flip heads; position
  lacked the version → version in the descriptor and `CTXPOS02` header;
  mismatch is unavailable; only migrate crosses (§4).
- M4 formatting was not hash-neutral (child counts with comments, trailing
  commas, ASI semicolons, parentheses, quotes, line continuations; docstrings)
  → filtered counts and per-language rules; docstrings count; T45 on real
  formatters (§4).
- M5 directive comments carry meaning → kept as leaves (§4).
- M6 common forms were ambiguous or undefined → same-path grouping; trait
  last segment plus generic-argument hash; non-path self types; raw
  identifiers; trait items; TS private members and all top-level declarators;
  Python conditional definitions; Go generic receivers, `init`, implicit iota
  groups (§2).
- M7 PROP12 floors dropped P5a's and mixed cases; error injection unverifiable
  → all floors, mixed anchors, checked fatal injections, SPEC-form oracle (§9).
- M8 G6 selection rule, weighting, bound vs point estimate and authority
  unstated → defined (§9); the tolerance is an owner decision recorded in SPEC.
- Minors: name-masked rebind hash and sibling-file search (§5); parse
  cancellation, iterative walk, crash = nothing published, parse-byte budget,
  OID-keyed cache (§4); legacy `#` key collision documented (§3); SPEC mirror
  and T-IDs (§10); all four languages together (§0); spans outside the symbol
  named for G6 (§4).

**r0.2 closure check (2026-10-22)**: no blocker; three new majors, fixed in
r0.3:

- N1 r0.2's own rules made false no-ops (`a[1,]` vs `a[1]`, `(u8,)` vs `(u8)`,
  `(a?.b).c` vs `a?.b.c`, `("use strict")`) → single-element trailing commas
  and parentheses around optional chains or statement strings are kept;
  must-differ goldens (§4, §9).
- N2 "bump only when the corpus differs" let two binaries share a version yet
  hash differently → versions derived from exact grammar and runtime versions
  plus a walk revision, no exemption; upgrades leave scopes unavailable until
  migrate (§4).
- N3 key hash markers came from the normalised hash, so a version change would
  change immutable keys → frozen whitespace-stripped source-text hash (§2).
- Gaps: untracked, unborn-HEAD and filtered host files under the parse-error
  rule (Git's own diff decides; otherwise Broken); legacy `#` file anchors
  mapped by descriptor, not by splitting keys; daemon derives the `CTXPOS02`
  header from descriptors; parse budget 32 MiB, once per OID; rebind search
  best effort under its own budget; G6 stratum and denominator properties
  stated; injectivity rules normative with goldens; SPEC v1.6.5 written.

**r0.3 closure confirmation (2026-10-22)**: N1, N2, N3 and every gap closed; no
blocker or major remains; G0 can pass once four stale SPEC lines are fixed
(P5b scope bullet "缺失/歧义", the Gate P5b G2 T-list, the SD18 → T row, the
language-order note), all fixed. Its new minor — the modified-vs-HEAD test used
Git's stat cache — is adopted: the test compares the filtered hash of the
captured bytes with the `HEAD` blob OID, with `--no-optional-locks` (§4).
**G0 passed.**

**r0.4 implementation findings (2026-10-22)**: the T45 corpus found formatter
rewrites the walk-1 rules did not cover (rustfmt arm/closure blocks and
diverging-tail semicolons; black's tuple, target and type parentheses;
prettier's quotes, keys, leading union bars and arrow parentheses). Walk
revision 2 adds the rules in §4, each with a must-differ golden. As §4
requires, the version changes: on the reference corpus 2,226 of 6,820
symbols re-hash (ripgrep 681/3,613, fd 117/459, click 855/1,379, cobra
0/646, ky 63/116, express 510/607), so existing symbol anchors need
`ctx anchors migrate` and their dependents see one STALE/PENDING round.
Clarifications of existing behaviour: the walk's open/close markers (§4);
preceding directive comments belong to the span (§4); a symbol path the
grammar has no table entry for observes Missing; the descriptor's
name-masked hash field is named `masked`.

**r0.5 G4 resource findings (2026-10-23)**: an 80 KB file of 10,000 nested
`mod a {` overflowed the observer's stack (extraction recursed per container),
and an 8 KB file of 1,000 nested modules took 29 s against the 5 s deadline:
only the parse checked the deadline, and tree-sitter's `parent()` (which
descends from the root) and indexed `child(i)` made the walk super-linear.
Extraction now stops at 256 nested containers, extraction and walk check the
deadline, ancestor facts (inside `ERROR`, optional chains) are computed in one
pass, and parents are carried down the walk. Hashes are unchanged for every
input within the bound (the six T45 corpora, 6,820 symbols, are identical);
walk revision 2 was not yet released, so the bound is part of it and no
version changes. The scan also dedupes identical host contents before the
parse budget, as this section always said (per blob OID). G5 measured the whole scan at ≈ 0.38 s per MiB of host bytes, so the
deadline, not the 32 MiB budget, is the effective bound (≈ 13 MiB); §4 now
says so.

**r0.6 G6 definitions (2026-10-23)**: SPEC and §9 named the thresholds but
not the denominators. Before any label existed, false invalidation was fixed
as FP / fired and missed invalidation as FN / positives (P5a's reporting).
The selection point and the firing rule were fixed with them (§9).
