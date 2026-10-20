# ADR 0005 — P1 memory budget scope

Status: proposed; no budget change approved. 2026-10-20.

SPEC P1 G5 requires at most 64 B/incidence excluding payload, with 100K revisions
and about 300K incidences. The implementation measures complete retained database
allocations conservatively and also reports process RSS. No payload bodies exist
in core. Content hashes, input headers, all reverse indexes, PK graph/order,
query memo and all epoch-diff history remain included.

Measured lower bounds after compact CSR reverse indexes and sparse pending work:

| Shape | Input columns | Indexes | Maintenance | Topology | Query memo + full snapshot | History | B/incidence |
|---|---:|---:|---:|---:|---:|---:|---:|
| multi_support(0.3), 329888 incidences | 29703962 | 12374448 | 2839140 | 16704128 | 14088852 | 20790836 | 292.53 |
| hub, 299991 incidences | 29703962 | 12015684 | 2599964 | 15945632 | 13162045 | 20865236 | 314.32 |

These are lower bounds: B-tree node/allocator overhead and process/runtime memory
are omitted. Therefore they prove a failure, never a pass. RSS is separately
reported and includes freed allocator pages retained after bulk staging.

Hot counting state plus reverse/incidence indexes is about 46.1 B/incidence on
multi_support and 48.7 on hub, but that number is not substituted for total memory.
Input columns alone exceed 64 B/incidence in this one-logical-per-revision shape.
DIFF history also grows with committed epochs; a budget independent of history
retention needs an explicit scope/retention definition.

The current decision is to keep the 64-byte whole-database budget unchanged and
leave Gate P1 open. Closing it requires either a broader storage-layout redesign
(interned/sparse logical metadata, compact PK, packed history and caches), or an
explicit specification decision defining separate budgets for hot kernel state,
immutable inputs and retained history. No automatic calibration is applied.
