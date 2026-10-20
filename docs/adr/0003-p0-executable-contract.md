# ADR 0003 — P0 executable contract details

Status: accepted for P0 implementation, 2026-10-19.

SPEC v1.4 already fixes the principal semantics. v1.4.1 records the remaining
deterministic choices rather than hiding them in the implementation:

- Rebind preserves the original rule's enabled value, then disables the original.
  Changing a disabled rule must not enable it as a side effect.
- Both independent validators report the first sorted DFS cycle path. The core
  detects cycles with Kahn first; path reporting is a separate diagnostic pass.
- ArtifactNotOpen belongs with CAS/state preconditions. PolicyViolation includes
  the reason already required by A5. A5 scans revision IDs, preferring BaseNotAllowed
  over NoEnabledRule for one revision.
- Metadata directories start unrestricted, then narrow by path segments. Invalid
  paths or widening return InvalidMetadata. They never change derivation.
- Synthetic PENDING precedence is ObservationLag, StructuralUnknown, Semantic.
  This matches certificate Unknown's priority over Pending. P0 snapshots have N_A
  observation and no structural Unknown; these branches are pure-function tests.
- EventLog v0 wraps a dependency-free binary codec in versioned JSONL. This keeps
  model free of serde, JSON and crypto dependencies; durable storage is a P2 format.
- BLAKE3 1.8.7 is the core's only external direct dependency. Its portable hash,
  constant-time comparison, platform selection and build helper crates are permitted
  by an explicit transitive allowlist. ChaCha8 dependencies stay in testkit.

No incremental counting, durable acknowledgement, full certificate issuance, ACL,
branch isolation, Conflict gate or observer transport is claimed by this decision.
