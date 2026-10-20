# ADR 0004 — P1 maintenance and measurement

Status: implemented; P1 Gate remains open for memory and remote validation, 2026-10-20.

Counting maintains L0 s/support, incidence sat, rule unsat/counted and canonical
witnesses. Only stored-bit versus final-truth transitions adjust counts. All seeds
are queued before any pop; a min-heap uses final topological ranks. A node is popped
at most once, and only S flips propagate to premise indexes. Disabled rules retain
incidence state. Ref remains provenance only.

Pearce–Kelly maintains graph ranks with bounded forward/backward searches and stable
reordering of the affected rank positions. Revision and Sem virtual logical nodes
share the graph. Sorted DFS diagnostics preserve the P0 error contract. NaiveTopo
remains an independent acceptance/order-validity reference.

Maintainer modes share transaction validation and differ in L0 maintenance.
FullRecompute is the exact reference mode. Stats are deterministic counters; NoStats
is a zero-sized sink. Any wall-clock timing is kept outside deterministic counters.
Paranoid rebuild checks run in addition to normal differential tests.

L1/L2 remain query-time memoized derivation in P1; incremental layer columns belong
to P3. The public P0 snapshot and final-diff contracts remain unchanged. Performance
must include the front-end cost, not just the propagation loop. Reports must retain
failed budgets/coverage/mutation results until resolved, without relabeling them as
a completed Gate.

The mutable commit path uses an undo overlay and a before-row journal instead of
cloning the database. Its status walker visits final PK order and propagates only
truth or inherited stale-reason changes. Support/witness-only changes are still
published in status_diff but do not invalidate descendants. Direct head seeds also
cover currency, Pin and Sem-current dependencies. Every memo entry is warm at
publication, so the old entry remains valid until its own ordered visit.

An existing complete DerivedSnapshot is patched in place (changed node rows and
rules whose conclusion was visited). It is not freed during a local commit: doing
so would add O(database size) deallocation even with zero visited incidences.
Structural commits still use staged inputs, rebuilt indexes and a fresh query memo;
no structural-insertion latency budget is claimed from the mutable-path results.
DA checks are active in debug OR paranoid, including release+paranoid builds.

T08b's old unqualified word "diff" conflicted with SD8.4. Its table now names
l0_flips explicitly and requires C in status_diff when its canonical witness changes.
This clarifies the existing contract; it does not relax a truth or budget gate.
