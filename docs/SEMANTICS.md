# Semantics implementation guide

`SPEC.md` v1.4 is the normative source. This document is a navigation and implementation
checklist; it does not duplicate or replace the formal definitions. The executable
semantics and the P0 golden/oracle suites have not been implemented yet.

| Contract | Normative location | P0 implementation focus |
|---|---|---|
| SD1, SD2, SD3 | SPEC §2.2–2.3 | Separate base/head; explicit incidence type; AND/OR set counting |
| SD4, SD5, SD6 | SPEC §2.3 | Disabled rules, canonicalization, revision and Sem logical cycle edges |
| SD7, SD8 | SPEC §2.3 | Pure derivation from committed inputs; staged ordered Ops; atomic rejection; two diffs |
| SD9, SD10 | SPEC §2.3 | Per-revision rules, Rebind, exact semantic vectors and validation CAS |
| SD11, SD12 | SPEC §2.3 | L0/L1/L2, Pin-strict, Sem-head, Evidence immutability, ABA |
| SD13, SD14 | SPEC §2.3 | Deterministic summaries; synthetic future dimensions in P0 goldens |
| SD15, SD16 | SPEC §2.3 | Witnesses, explanations, global epoch, version-based certification |
| SD17, SD18, SD19 | SPEC §2.3 | Reserve later-phase dimensions; main-only input model initially |
| Kind admission | SPEC §2.5 | Summary/Evidence/CodeAnchor restrictions, channel policy, required dependencies |
| Ops and errors | SPEC §6.1–6.2 | New(k), canonical identities, sequential checks and stable errors |
| Test traceability | SPEC §8.3 and docs/gates/P0.md | Every implemented contract must point to its golden/property evidence |

Future behavior must stay explicitly phased: no multi-branch product API before P6,
no Conflict node gate before P7, no multi-user ACL before P9, and no recursive
production support before P11. The independent Jacobi oracle may accept raw cyclic
fixtures for the P0 least-fixpoint regression required by T11.

Prior art and claim boundaries are recorded in SPEC §1.3 and §13. The counting core
is based on established truth-maintenance/view-maintenance work; this project tests
the combination and its implementation rather than claiming invention of counting.
