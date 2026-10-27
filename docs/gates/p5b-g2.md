# P5b G2 — golden-test closure

Status: **passed**, 2026-10-23. This is a layer result, not the complete P5b
phase gate. SPEC §4.1 permits each layer to pass independently; A11's P4/P5a
dependency constrains phase closure.

The earlier status "Evidence complete, pending gate" was inaccurate. The T45
formatter corpus was complete, but the ADR §3/§9 legacy `#` key-collision case
had no explicit end-to-end golden. The closure test found a real defect:
registering `src/lib.rs#foo` could change an existing P5a file anchor with that
literal path into the symbol `foo` in `src/lib.rs` under the same logical key.

`observer::targets` now rejects that registration with `LogicalExists` before
capture or submission and tells the operator to rename the legacy file. The
regression seeds a P5a descriptor, then asserts:

- its path remains the complete literal `src/lib.rs#foo`, read from the
  descriptor rather than split at `#`;
- the next scan retracts its base and opens one break, with capture reporting
  `Broken("reserved-character")`, as ADR §3 requires;
- edits to `src/lib.rs` do not retarget it or change its revision;
- attempting the colliding symbol registration fails with the diagnostic and
  leaves the complete anchor state and watermark unchanged.

Original closure validation (2026-10-23, base `ac03628` plus this fix):

| Check | Result |
|---|---|
| `contextdb-symbols --test symbols` | 15 passed: per-language extraction, grouping, T45b, must-differ/formatter-equivalence pairs, stream boundaries |
| `contextdb-anchors --test anchors` | 11 passed: key/descriptor round trips, path restrictions, observer/classifier state and freshness goldens |
| `contextdb-cli --test symbols_cli` (G2 subset) | 3 passed: symbol lifecycle/path/range/rebind scenario, T42e version migration, legacy path/key collision |
| Independent-normalisation PROP12 smoke | 60 seeds / 1,200 transactions, all floors met |
| Separate model PROP12 smoke | 200 seeds / 2,000 transactions |

Before committing on 2026-10-24, the registration fix was revalidated on
`a906be1` (P6 store format 3, normaliser walk 3): 16 symbol goldens, 11 anchor
tests and the same three CLI scenarios passed. The added symbol golden covers
the end-of-list trailing comma found during G6. Fmt, targeted Clippy and the
two property smoke suites were also rerun on this tree. This registration
guard changes neither the normaliser nor the descriptor encoding.

The formatter corpus and normaliser-version walk check are the existing
[T45 evidence](p5b-t45/README.md): six pinned corpora, 6,820 symbols and ten
explained exceptions. They were not rerun for this registration-only fix;
the normaliser version and hashes are unchanged by this fix. Since the
original closure, [ADR 0012 r0.8](../adr/0012-p5b-symbol-anchors.md) records
the walk-3 trailing-comma correction: 59 spans change relative to walk 2,
and the formatter corpus has no new exception. Full walk-3 descriptor hashes
include same-file context and require migration, as that ADR specifies.

```sh
cargo test --release --locked -p contextdb-symbols --test symbols
cargo test --release --locked -p contextdb-anchors --test anchors
cargo test --release --locked -p contextdb-cli --test symbols_cli -- \
  --skip observer_killed_mid_scan_publishes_all_or_nothing \
  --skip parse_byte_budget_end_to_end
```

The two skipped cases belong to G4, which has its own report. CLI integration
tests require access to their temporary Unix sockets. G6 has its own report
and verdict.

## Walk 4 (ADR 0012 r0.9, 2026-10-26)

The independent implementation review showed the G2 goldens missed real
meaning changes. Walk 4 fixes them, and `tests/walk4.rs` adds must-differ
and must-find goldens for each counterexample:

- **Uncovered text**: Python escapes, f-strings and format specifiers, bytes,
  Rust raw strings, `macro_rules!` separators.
- **Context errors**: broken helpers, assignments and imports; local errors
  stay local.
- **Python block statements**.
- **Go**: `for` clauses, grouped `var`, versioned imports.
- **JS/TS**: array holes, multi-declarator keywords, `export declare`, dotted
  namespaces, `this.#p`.
- **Rust**: `r#` identifiers.
- **Normaliser version**: pinned to `Cargo.lock`.

T42e also gained a CLI case, a Missing anchor across `migrate`. It fails
without either half of the fix. Formatter corpus: identical under walk 4.
Walk check 3 → 4: 886 of 6,820 symbols re-hash ([T45](p5b-t45/README.md)).
`contextdb-symbols`, `contextdb-anchors` and `contextdb-cli --test
symbols_cli` pass on the final tree.
