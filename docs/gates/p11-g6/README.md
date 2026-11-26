# P11 G6 tools

This directory will hold the raw evidence for P11 gate G6 (see the G6
section of [P11.md](../P11.md)). The tools below produce that evidence.
This file records no run and claims no pass.

- **A, the scenario:** `p11_g6` (`crates/contextdb-bench/src/bin/p11_g6.rs`)
  starts the real release `ctxd` on an isolated data directory and talks to it
  over its sockets, as the CLI does.
- **B, dogfood checkpoints:** `ctx g6 checkpoint` asks a running `ctxd` for a
  checkpoint pinned to one epoch. `ctx g6 verify` replays that op_log prefix
  eagerly in an isolated copy and compares every field.

Build every binary from one commit:

```sh
cargo build --release -p contextdb-daemon -p contextdb-cli -p contextdb-bench \
  --bin ctxd --bin ctx --bin p11_g6
```

## A. The scenario

```sh
target/release/p11_g6 hub                # 1,000,000 revisions, ctxd's default settings
target/release/p11_g6 gc                 # GC fixture, 2,000 records
```

Options: `--lazy-budget E` and `--read-settle-ms MS` write `ctxd.toml`.
`--chunk N` sets the ops per load request (default 50,000). `--rounds R` caps
the probe rounds per cascade (default 20). `--evidence DIR` sets where
checkpoints, verify output and `ctxd` logs go (default: a new temporary
directory). `--bin DIR` says where `ctxd` and `ctx` are (default: next to
`p11_g6`). `--keep` keeps the data directory.

Every step prints one line, `p11_g6 item=<hub|gc> step=<name> ... errors=N`,
and the run ends with `p11_g6 item=... evidence=DIR ... errors=N`. The exit
code is 1 when any error occurred. Each error is described on stderr.

**`hub` steps**

| Step | What it checks |
|---|---|
| `start` | The BLAKE3 of the running `ctxd` executable (reported by the process) and of the file this run launched, which must be equal; protocol, log format, `lazy_budget`, `read_settle_ms`, `keep_epochs`. |
| `load` | The shape: n records over one payload, bases 0, 1, 2, and n − 3 rules on all three. Requests, acks that were `settling`, and load and settle times. |
| `certificate` | A session reads a hub record and an unrelated record (Certified), then `ctx_check` issues a certificate, which must be `Valid`. |
| `retract_frontier`, `restore_frontier` | Revision 0's base is retracted (then restored). `settling_ack=true` is the deferred acknowledgement. Probe rounds then run until a round sees no frontier: a Certified read of a random hub record and of the unrelated record, the certificate validated by id, and the hook gate's check (`Request::Check` in `enforce` mode, which issues and validates). `frontier_answers` counts `PENDING(StructuralUnknown)`, `Settling` and `Pending` answers. A positive answer while the support is withdrawn is a false positive. A definite negative after the restore is wrong too. Both are errors. |
| `retract_settled`, `restore_settled` | One probe round after the settle must give the eager answers: `stale` and `Invalid` while retracted, `certified` and `Valid` once restored. |
| `checkpoint_<name>` | `ctx g6 checkpoint` and `ctx g6 verify` (see B), on the settled state. |
| `next_write` | A following write and its ack. |
| `mirror` | Another retract, and a mirror export into a temporary git repository right after the deferred ack. It pulls to the end, so it renders the settled tree (or answers `Settling`). An export after the settle must report the ref unchanged. |
| `kill`, `restart` | A restore is acknowledged `settling`, then `ctxd` gets SIGKILL. After the restart, every acknowledged client_txn_id must be committed at its acknowledged epoch (`acked=N present=N`). |

G6 asks for the default budget first. If `retract_frontier` reports
`settling_ack=false`, the run reports an error. Rerun with a smaller
`--lazy-budget` and record that change. At small n, `--read-settle-ms 0`
lets reads answer the frontier rather than pull past it.

**`gc` steps** (`[gc] keep_epochs = 100`)

| Step | What it checks |
|---|---|
| `load` | n records with distinct payloads. A quarter are superseded. `gc/pinned` v1 is named by a session's certificate, superseded and loses its base bit, so only K4 keeps it. Then 110 more epochs. |
| `gc_dry_run` | `ctx gc` dry run. It repeats until `keep_epochs` sets the cutoff and not a snapshot a reader still holds (K5); `polls` and `k5_wait_ms` record the wait. The collectable count must equal the superseded count. |
| `gc_apply` | `ctx gc --apply` collects exactly those. The superseded v1 reads `collected`, pinned v1 and a kept head read their bodies, and a `SetHead` back to a collected revision without its bytes fails with `MissingBlob`. The checkpoint taken after it has the same epoch and digests as before GC. |
| `gc_compact` | `ctxd` stops, `ctx gc --compact` runs, `ctxd` restarts. Records dropped, bytes reclaimed, and `blobs/` bytes before and after. |
| `restage` | After the restart, the collected revision still reads `collected` and pinned v1 still reads its body. Superseding back to the old text sends the bytes again, and the old revision reads again. |

Checkpoints `before_gc`, `after_apply` and `after_compact` must agree in
epoch and every branch's digests.

A small-n run takes seconds:

```sh
target/release/p11_g6 hub 20000 --lazy-budget 100 --read-settle-ms 0 --chunk 5000
target/release/p11_g6 gc 2000
```

## B. Dogfood checkpoints

```sh
ctx g6 checkpoint <data dir> [--out FILE] [--live-pos SCOPE=HEX]...
ctx g6 verify <data dir> <checkpoint.json> [--work DIR]
```

Take a checkpoint at the start, at the end and on every active day of each
deployment, and verify it soon after. Verify only reads the data directory,
so `ctxd` keeps running and appending meanwhile.

`checkpoint` sends the admin-only `Request::Checkpoint`. `ctxd` pulls to the
end of any pending cascade, waiting up to 10 s, and otherwise answers
`Settling`. The CLI asks again for up to 10 minutes. A checkpoint is never
partial. It prints:

```
g6 checkpoint epoch=E branches=B certs=C non_empty=N history_unavailable=H ctxd_pid=P ctxd_blake3=X exe_file_now=Y out=FILE
```

`verify` prints any `mismatch <path>: checkpoint=... replay=...` and
`error ...` lines, a `replay` line (frames replayed, time), a `payloads`
line, and finally:

```
g6 verify epoch=E branches=B certs=C non_empty=N mismatches=M errors=K
```

It exits 0 only when `M` and `K` are both 0.

### Checkpoint file

Canonical JSON: every object's keys are sorted.

| Field | Meaning |
|---|---|
| `format` | `contextdb.g6.checkpoint/1` |
| `meta` | Added by the CLI and not compared: `taken_at_utc`, `data_dir`, `ctxd_toml_now` (the file as read when the checkpoint was taken; `null` when absent), `exe_file_blake3_now` (the BLAKE3 of the file at `ctxd.exe` when the checkpoint was taken). |
| `epoch`, `log_checksum` | The settled snapshot's epoch and its op_log frame checksum. Each frame's checksum chains the previous one, so this pins the whole prefix. |
| `protocol`, `log_format` | 9 and 7 for this version. |
| `ctxd` | What the running process reports: `pid`, `exe`, `exe_blake3`, `lazy_budget` (`null` is eager), `read_settle_ms`, `keep_epochs`. |
| `live_pos_given` | Whether `--live-pos` was given. Otherwise each certificate was checked with its branch's watermarks at that epoch. |
| `branches[]` | `id`, `name`, `kernel_digest` (`BranchView::digest`), `derived_digest` (`Db::derived_digest`). |
| `certs[]` | Every certificate `ctxd` holds, ordered by `cert_id` and listed once. `sources` says where it is held: `session`, `root_proof`, `session_cert` (each also as `ended_...` for a recently ended session) and `handoff`. `cert` is its wire encoding. `live_pos` is the map it was checked with. `verdict` is the verdict's name, `verdict_wire` its full wire encoding, and `history_unavailable_since` is set instead when the kernel no longer holds the history a strict check needs. `principal`, `branch`, `issued_epoch`, `strict_no_aba` and `entries` are readable copies. |
| `counts` | `certs`, `non_empty` (verdicts other than `Empty`; `HistoryUnavailable` excluded), `history_unavailable`, `by_verdict`. |

Every certificate is validated by `validate_certificate` on its branch's view
at the snapshot (on `main` if the branch is unknown, as `ctx_check` does).
The `LogLink` is the one `ctx_check` uses. It matches when `issued_epoch` is
at most the epoch and the frame checksum at `issued_epoch` equals the
certificate's. Its current checksum is `log_checksum`.

### How verify stays read-only and prefix-exact

1. The work directory is `--work DIR` (absent or empty, kept afterwards) or a
   new temporary directory (removed afterwards). It is created with mode 0700,
   and its path is resolved before anything is created. A path inside the
   data directory, or one containing it, is refused.
2. It copies every `log/seg-*.log` file. Appends only add to the last segment
   or start a new one, so the copy holds every frame through the checkpoint's
   epoch. It scans the copy, finds that epoch's frame, refuses one that ends
   inside a commit group, cuts that segment after the frame and deletes later
   segments. A log broken before that epoch, or one that does not start at
   epoch 1 (after `ctx doctor --truncate-before`), is an error.
3. It copies `blobs/`, `keys/` and `gc/`. It does not copy `snap/`, so replay
   starts at genesis, and it skips the projections and the session log.
4. It opens the copy with `Store::open` (`lazy: None`, `strict_tail`, memory
   projections). Open replays every frame on an eager kernel and checks each
   result against the one recorded in the frame. Then the epoch must equal
   the checkpoint's, and the kernel may hold no frontier.
5. It recomputes the epoch, the checksum, every branch's digests, and every
   certificate's check with the same certificate, branch, `live_pos` and
   `LogLink` rule. It renders that as a checkpoint file and compares the
   whole file with it, leaf by leaf. Only `format` and `meta` are taken from
   the file. A forged digest, verdict, count or readable copy is a mismatch.
6. Every head on every branch must have its payload in the copy. A shredded
   payload is counted. A missing, corrupt or collected one is an error.

Verify never opens the live store, never takes its lock and never writes
under the data directory.

### The `ctxd` hash

`ctxd` opens its own executable at start (`Daemon::start`). On Linux it opens
`/proc/self/exe`, the running image itself. Elsewhere it opens the file
`current_exe()` names at that moment. It keeps that descriptor and hashes
it with BLAKE3 on the first checkpoint. Replacing the installed binary by
rename afterwards does not change the hash. `exe` is the path `current_exe()`
gave at start.

The hash is BLAKE3, not SHA-256: the daemon's reviewed dependencies
(`config/dependency-policy.toml`) include `blake3` but not `sha2`. `ctx g6
checkpoint` also hashes the file now at `exe` (`meta.exe_file_blake3_now`).
The two differ when the installed binary was replaced after `ctxd` started:
an installed binary is not proof of what runs. To compare with a release
artifact, hash it with `b3sum`.

## Where evidence goes

- Scenario runs: `docs/gates/p11-g6/scenario/<commit>/`, holding the
  stdout and stderr of `p11_g6 hub` and `p11_g6 gc` and the `--evidence`
  directory (checkpoint JSON, `*.verify.txt`, `ctxd-*.log`, `gc-*.txt`).
- Dogfood: `docs/gates/p11-g6/dogfood/<deployment>/<UTC date>-<epoch>.json`
  with the `ctx g6 verify` output next to it as `.verify.txt`.
- The report: `docs/gates/p11-g6-report.md`, per P11.md.

## Before using this as evidence

- A checkpoint holds only the certificates `ctxd` keeps in memory when it is
  taken: live sessions, recently ended ones and pending handoffs. It is not
  every certificate ever issued, and it has no daily read and write counts.
  Count real agent reads and writes, Pending, Settling, Busy, timeouts and
  latency from other sources.
- Without `--live-pos`, certificates are checked against the store's own
  watermarks, so no scope lags. That isolates kernel and certificate
  behaviour from the live observer. A check against current code positions
  needs `--live-pos`.
- `HistoryUnavailable` appears only when `ctxd` started from a kernel
  snapshot (`ctx snapshot`) newer than a strict certificate's window. It is
  reported, and verify compares it too: the replay from genesis has the
  history, so it would show up as a mismatch.
- Verify copies `keys/` and the GC ledger as they are when it runs. Run it
  before any `ctx gc --apply` or `ctx shred` on that store. Otherwise a head
  at the checkpoint that was collected or shredded since then reads
  differently.
- The copy holds the store's encryption keys and payloads. It lives in a
  0700 directory, and the temporary one is removed. Treat a kept `--work`
  directory like the store.
- `p11_g6 hub` at 1M renders a million files into the mirror repository in
  the `mirror` step. Expect that step to dominate the run time.
- The `gc` scenario waits for K5 before it collects. While its candidate
  generator walks an early snapshot, that snapshot sets the cutoff (seconds
  at n = 2,000). This is the designed K5 behaviour, recorded as `k5_wait_ms`.
