# Executable semantics

SPEC v1.4.1 is normative. This guide maps every semantic decision to executable
P0 evidence and explicitly identifies future behavior. P0 exposes a Rust API:
`Db::commit(Batch)` returns allocations, epoch, NetDelta, l0_flips and status_diff;
all derived reads go through `DerivedView`. CLI database commands are not exposed.

## Input and transaction contract

Logical IDs identify immutable kind/scope and stable keys. Revision IDs identify
immutable `(logical, content_hash)` values. Creating a logical sets its first head;
adding a revision never moves the head. Head and base are independent input tables.
Rules, validations, artifacts, watermarks, metadata and final branch policy are
inputs too. No query mutates these tables or derived state.

Each batch runs in order against staged values. `New(k)` binds the kth prior output
of its type; content dedup still consumes a revision binding. Core has separate
resolution, canonicalization and validation from InputModel. Core stages append-only
SoA revision/rule arenas, CSR premises and an enabled-column undo log. Other mutable
input tables stay private in the staged transaction. Rejection truncates arenas and
restores enabled values; publication changes epoch and snapshots only after all
checks. The reference model instead clones the complete input snapshot.

Canonical incidences are sorted by RevisionId, strongest `Cur > Sem > Pin > Ref`.
Ref duplicated by a supporting edge is dropped. Reject in order: empty premises,
self-premise, two revisions of one logical (including Ref), >1024 premises,
Evidence/CodeAnchor conclusion, Summary used by anything except Sem. Duplicate
identity includes conclusion, canonical premises and references, even if disabled.
Rebind keeps edge kinds and the original enabled value, disables the original,
and creates a new rule; any failure rejects the complete batch.

Error priority is SPEC §6.2: resolve all references, channel, restricted/immutable
kind, key/policy/metadata, logical/Rebind checks, rule admission, duplicate, NoSem,
CAS/state preconditions, cycle, capacity; final A5 policy last. Per-kind allocation
is dense u32; Epoch is checked u64. Unknown branches are rejected in P0.

## Support and certification

L0 is the least fixed point of `base OR enabled AND all premises satisfied`.
Pin and Sem satisfaction is L0(p); Cur also requires p to equal its current head.
Ref is provenance only and enters no support index or cycle edge. Disabled rules
still retain their incidence satisfaction/unsat counters and participate in cycles.
Support counts active rules (base is a separate boolean). Witness prefers base,
then the lowest active RuleId, independently in every layer.

For rule j, A(j) is the sorted `(logical,premise)` vector for Sem edges. curvec(j)
uses the same logicals with their current heads. sem(j) is NotRequired without Sem,
Verified if A equals curvec (even if an explicit record says otherwise), otherwise
the latest validation at `(j,curvec)` or Pending. Validation uses staged vector CAS.

L1 requires enabled, sem != Rejected, every premise in L1, Cur currentness,
Pin currentness except Evidence, and for Sem the current head in L1 too.
L2 uses the same formula with L2 and sem in {Verified,NotRequired}.
Both layers also include base. Thus L2 is a subset of L1, and L1 of L0. P0 has no
Conflict gate. Rejected affects L1/L2, never L0. Pending propagates through L2.

The cycle graph includes all revisions, all supporting premise-to-conclusion
edges, and for each Sem logical l a virtual V_l with every revision h of l -> V_l
and V_l -> each Sem conclusion. Disabled rules remain in this graph. Core uses
NaiveTopo/Kahn detection, oracle input validation uses DFS. Sorted DFS determines
the public diagnostic path. Jacobi accepts raw cyclic fixtures to compute least
fixed points; production transactions reject cycles. Kahn is a second independent
rank-ordered evaluator for legal DAGs. Their final snapshots include every counter,
layer, semantic state, currency and canonical witness.

## Summaries and explanations

Summary priority: Unavailable > Conflicted > Stale > Pending > Certified.
Stale reason: non-current -> HeadSuperseded; no L0 -> Unsupported; L0 but not L1
uses the canonical L0 witness. A Rejected gate wins; otherwise inspect the first
failed incidence by RevisionId. A superseded non-Evidence Pin produces
SupersededPinnedPremise(p); failing Sem current-head L1 inherits the current head's
reason; otherwise inherit the premise's reason. This recursion follows the DAG.

Pending priority is ObservationLag > StructuralUnknown > Semantic. Future
observation, branch-only currency, conflict and visibility branches are synthesized
for P0 function tests. P0 snapshots always have observation N_A, authorized=true,
conflicted=false, and exact Supported/Unsupported structural status.

Revision semantic state takes the first available witness in L2, L1, L0 order:
base -> NotRequired; rule -> that rule's sem; no L0 -> N_A. WHY lists all active
rules, base and canonical witness, and separately lists non-supporting Ref links.
WHY_NOT includes disabled/gate reasons, asserted/current vectors, failed incidences
and bounded recursive upstream explanations.

The P0 certificate function checks required vector entries against current inputs
and caller-supplied live positions. Its signature contains no epoch. Advisory
entries do not gate. Full certificate closure issuance, strict history and
recertification belong to P3; P0 does not claim those APIs.

## Kind admission and metadata

A1 Evidence/CodeAnchor cannot be conclusions. A2 Summary is Sem-only. A3 no Op can
replace revision content; metadata directories can only narrow and do not affect
derivation. A4 unresolved import creates ImportBroken, never a dummy support rule.
A5 final policy requires every newly allocated selected-kind revision to have an
enabled rule and forbids base for all selected-kind revisions. Evidence and
CodeAnchor are exempt. Disabling the last rule later is allowed, yielding Stale.
A6 Evidence rejects AddRevision/SetHead, including dedup/no-op requests.
A7 only Observer can write CodeAnchor inputs, watermarks and CodeAnchor-targeted
AnchorBroken artifacts; Observer cannot write ordinary rules or records. Only
Admin can SetPolicy. Trusted transport binding is deferred to P3.

Policy hash uses BLAKE3 over sorted-key whitespace-free JSON with kind name strings
in alphabetical order. Core digest hashes canonical full inputs plus derived
snapshot, excluding the top-level epoch and stats. Stored origin epochs remain
inputs. Metadata changes the full digest but leaves the derived digest unchanged.
See [EVENTLOG.md](EVENTLOG.md) for the dependency-free versioned trace format.

## Complete SD to test mapping

All test functions live in `crates/contextdb-testkit/tests/p0.rs` unless indicated.
Grouped names intentionally cover several T IDs, each with explicit expectations.

| Decision | Contract and current phase | Evidence |
|---|---|---|
| SD1 | Independent base/head; immutable revisions | T64, T09q, T12 |
| SD2 | Explicit Pin/Cur/Sem/Ref; Ref provenance only | T06abc, T07ac, T09abcd |
| SD3 | Set AND/OR, exact support and unsat | T01–T04, T09abcd |
| SD4 | Disabled rules retain counters and structure | T09abcd, T25 |
| SD5 | Revision DAG plus Sem virtual logical nodes | T10, T11, T15e, T25, T26 |
| SD6 | Canon/dedup/admission and fixed rejection order | T09a–h, T09j, T09o |
| SD7 | Derivation is a pure function of committed inputs | 1,000-seed I1/PROP8, T09k/l |
| SD8 | Ordered staged CAS, atomic rollback, final net delta | T12, T29, T11; properties.rs diff checks |
| SD9 | Per-revision support and Rebind | T09n, T26; Cur Rebind product convenience deferred |
| SD10 | Exact Sem vector, latest VR, validation CAS | T15bc/d, T16b-sem |
| SD11 | L0/L1/L2, Pin-strict and Sem-head | T06abc, T15bc/d, T24 |
| SD12 | ABA permitted; Evidence immutable | T16b-sem, T09i |
| SD13 | Summary precedence, recursive reasons, semantic witness | S13-rows-fn, T15bc/d, T24 |
| SD14 | Future Unknown/freshness never Certified | S13-rows-fn; live read integration P5/P10 |
| SD15 | WHY active alternatives, Ref separate; WHY_NOT vectors | T63, T06a, T15bc/d |
| SD16 | Global epoch, version-based verdict | T12, T62 oracle; strict certificate history P3 |
| SD17 | Conflict=None P0; raw cyclic oracle lfp | T09u, T11 Motik; Conflict gate P7, recursive product P11 |
| SD18 | Observer-only CodeAnchor/watermark inputs and dedup | T09h/q/r/t, T24; filesystem observer P5 |
| SD19 | MAIN branch only; reserve origin branch data | T15e, T26; multi-branch product tests P6 |
| §2.5 A1–A7 | Kind admission, immutability, policy, channels | T09h–v, compile-fail model tests |
| §6.1–6.2 | IDs/local refs, canonical identity and errors | T09p–v, T29, wire.rs round trips |
| API isolation | No mutable database or payload rewrite API | T28 core compile-fail, T09k model compile-fail |

`properties.rs` runs exactly 1,000 ChaCha8 seeds, 12,000 transactions: input equality,
exact errors, atomic rejection, full Jacobi/Kahn snapshot equality, layer inclusion,
canonical wire round trips and both diff sets. Coverage counters are descriptive in
P0; P1 adds the required mix/alternative/cycle/CAS/PK thresholds.

## Prior art and claim boundaries

Counting and truth maintenance originate in Gupta et al. (1993) and Forbus & de
Kleer (1993), not this implementation. Motik et al. (2019) Example 5 is represented
by the two ground R(a,b)/R(b,a) rules in T11 to prevent cyclic self-support after
base retraction. This project evaluates the combination and implementation; it
does not claim invention of counting, fixed-point evaluation or CAS. Bibliographic
sources and archived experiments remain in SPEC §1.3/§13 and docs/research.
