# EventLog v0

P0 test traces use one batch per JSONL line:

```json
{"version":0,"batch_hex":"000000000000000000000000"}
```

This example is an empty MAIN/Agent batch (principal 0, no client transaction ID).
The reader accepts the canonical key order shown above, surrounding line whitespace,
blank lines, and either hex case. Unknown versions, extra fields, malformed hex,
truncation, invalid tags, duplicate/unordered map or set entries and trailing binary
bytes are rejected. It is a canonical trace reader, not a general JSON parser.

The encoder emits lowercase hex. Binary layout:

- Integers: unsigned little endian; ID width follows SPEC §6.1.
- bool: one byte 0 or 1. Option: bool presence then value when present.
- byte arrays: raw fixed-width bytes. String: UTF-8 bytes with u32 byte length.
- Vec/set/map: u32 element count followed by elements; set/map keys strictly sorted.
- Struct fields: declaration order in model. Tuples: left to right.
- Enum: u8 tag followed by fields in declaration order. Existing tags are frozen;
  future variants append. `Op` tags 0–13 correspond to CreateLogical, AddRevision,
  SetHead, AssertBase, RetractBase, AddRule, SetRuleEnabled, Rebind, RecordValidation,
  SetPolicy, ObserverWatermark, SetMetadata, OpenArtifact, ResolveArtifact.
- Edge tags: Ref=0, Pin=1, Sem=2, Cur=3. Channel: Agent=0, Observer=1, Admin=2.
- RecordKind: Fact, Assumption, Decision, Plan, Convention, Procedure, Artifact,
  Evidence, CodeAnchor, Summary (0–9).
- References: Existing=0, New=1; LogicalRef additionally has Key=1, New=2.
- Verdict: Verified=0, Rejected=1, Pending=2. ArtifactType: Conflict=0,
  PendingReview=1, ImportBroken=2, AnchorBroken=3, DriftReport=4.

Decode limits: 16 MiB per binary value, 1,000,000 elements per collection, 32 MiB
per JSONL document. Invalid batches can round-trip: semantic validation happens
only in InputModel/core. Limits protect this test import boundary; in-memory
transaction APIs have the checked ID limits in SPEC, not a u16 batch-size limit.

`Wire` also serializes input and derived snapshots for deterministic comparison and
digesting. Those encodings are not a durable storage/recovery API. P2 will define
its durable op_log envelope separately, including compatibility and crash behavior.
