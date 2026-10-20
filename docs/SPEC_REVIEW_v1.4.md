# SPEC v1.4 bootstrap review

Date: 2026-10-19. This is a targeted follow-up to the v1.2/v1.3 reviews, not a full
implementation gate or a fresh validation of external research claims.

The remaining v1.3 findings have received substantive specification changes:

| Finding | v1.4 disposition |
|---|---|
| Q01 observer AnchorBroken | Explicit observer Open/Resolve allowlist; T09t |
| Q02 branch/global state | Branch Policy; key namespace exception; revision visibility on dedup; T73/T75 |
| Q03 PROP7/Summary | Conservative read/write condition, renamed comparison, corrected Summary explanation |
| Q04 diff contract | Separate l0_flips and status_diff, consumers specified, PROP2b |
| Q05 artifacts | ArtifactId/ArtifactRef/new_artifacts and same-batch resolution; T09u |
| Q06 persistence/boundaries | Committed-only idempotency, one batch per log row, hash-only payload references, P2 ADR precondition |
| Earlier A5/TOCTOU | Enabled dependency/no-base constraint; external atomicity claim narrowed; T09v |

No remaining issue in this follow-up blocks initializing Git or reserving workspace
crate boundaries. This is **not** a claim that P0 is complete or that future-phase
designs are fully verified. No semantic API has been fabricated to make the build pass.

## Small catalog details to settle before the P0 model is frozen

- §2.5 A5 gives PolicyViolation a reason field, while the §6.2 catalog still shows
  only kind/logical. Synchronize the error shape before implementing golden assertions.
- §6.2 introduces ArtifactNotOpen but does not place it in the normative per-Op
  check order. Define its precedence before oracle/front-end error equivalence testing.
- P2 G4 still says txn_status may return Unknown, while §6.1/§6.3 now specify
  NotCommitted. Use the revised name when that phase is specified and implemented.

These are tracked explicitly rather than silently selecting behavior in code. The
workspace scaffold does not depend on a choice for any of them.
