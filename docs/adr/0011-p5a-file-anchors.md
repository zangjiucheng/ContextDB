# ADR 0011 — P5a file anchors and observation freshness

Status: accepted, revision 0.3, 2026-10-21. G0 independent review passed.

## 0. Scope and delivery

Implements SPEC SD18, SD14b, P5a, T42a/c, T43/b, T44, T46b/c,
S13-rows-e2e (ObservationLag), PROP12, and the pre-push range check deferred
from P4. P4 G6 continues against `.contextdb/bin` and `.contextdb/db`; builds,
tests and development daemons use different paths. Do not upgrade or erase those
live files. P5a can be implemented during dogfood (A11), but its Gate cannot pass
before P4. Human replay labels are required evidence, never generated claims.

P5b symbol parsing, branch isolation, validators and ACL remain later phases.
The existing cooperative same-OS-user trust boundary remains unchanged.

## 1. Boundaries

`contextdb-anchors` owns pure observation classification, Git worktree capture,
manifest fingerprints, bounded filesystem polling and observer-client commits.
It depends on model/proto/BLAKE3, nix (safe openat with O_NOFOLLOW/O_NONBLOCK),
and std, never store/SQLite/daemon. The daemon
may use its pure capture/registry types. SHA1-checked 0.10.0 and sha2 0.10.9 (MIT OR Apache-2.0) compute
Git blob OIDs over `blob <len>\0` + the captured raw bytes; SHA-1 collision
detection failure is ObservationUnavailable (never a mitigated replacement hash).
This equivalent implementation was independently reviewed; Git remains the
independent byte/OID reference. Git is executed with argument arrays,
never shell interpolation. No Git object is written while hashing.

The existing segmented op_log remains the sole kernel truth. Registration is
represented by CodeAnchor logicals and content-addressed descriptor bodies;
no new kernel Op or alternate event log is introduced. Local repository routing
configuration is trusted operator configuration, not a derivation input.

Start with a portable polling filesystem watcher (bounded interval, full rescan
after errors); Git hooks and PostToolUse request immediate reconciliation. Missed
notifications affect latency only: every certified read/check samples live state.
Native event backends may later replace wakeups without changing classification.

## 2. Identity and Git bytes

Each canonical worktree root has a 32-hex repo id (BLAKE3 of its canonical absolute
path, first 16 bytes). Separate linked worktrees have separate ids/scopes before
P6. Scope is `code:<repo-id>`; logical key is `code:<repo-id>/<relative-path>`.
Keys remain subject to the existing 256-byte limit and lowercase ASCII key
alphabet (letters, digits and ._:/#-). Paths outside that domain are rejected,
not lowercased or silently renamed. Reject overlong keys, non-UTF8
paths, absolute paths, `..`, `.git`, runtime data paths and submodule traversal
explicitly at registration; never silently drop an invalid registered anchor.
Git-tracked regular files are supported, including staged/uncommitted edits;
registration may explicitly include an untracked regular file. A symlink or
non-regular replacement is classified Broken. All parent symlinks are unsupported:
reject at registration, classify an existing anchor Broken, even within the root. Registration errors do not create a certified placeholder.

Revision identity is the Git blob OID of the *captured worktree bytes*, using
`git hash-object --no-filters --stdin` in that worktree. It deliberately ignores
attributes/clean filters and avoids writing loose objects. Support SHA-1 and
SHA-256 repositories; Git's algorithm and complete OID are explicit in the body.
Executable-mode changes do not change byte content identity in P5a.

The model's 32-byte ContentHash remains a BLAKE3 blob address, not a padded Git OID.
The canonical descriptor is model::Wire: bytes `CTXANC01`, u32LE version=1,
Wire String(repo-id), Wire String(relative-path), u8 format (1=SHA1, 2=SHA256),
Wire Vec<u8>(raw OID). Wire strings/vectors have u32LE byte/count lengths. Enforce
exactly 20/32 OID bytes, lowercase 32-hex repo id and no trailing bytes. Unknown
versions/tags and all noncanonical encodings fail. BLAKE3(descriptor)
is the revision ContentHash. Thus identical logical/path/OID reuses the same
RevisionId and store blob checks remain unchanged. Raw source bytes are not
retained as payload; the descriptor is audit evidence of identity, not a backup.

References: [Git hash-object](https://git-scm.com/docs/git-hash-object),
[Git ls-files](https://git-scm.com/docs/git-ls-files). NUL-delimited Git path
output must be parsed as bytes before validating the supported path domain.

## 3. Classification and atomic batches

The pure classifier accepts the prior committed head/base/artifacts and one
observation: Present(descriptor), Missing, or Broken(reason). Read/permission,
Git-process and resource-limit failures are ObservationUnavailable, not Missing:
they do not retract a file or advance its watermark.

| Prior state | Observation | Transaction |
|---|---|---|
| No logical | Present | CreateLogical(CodeAnchor, base=true) |
| Same identity, base=true, no open break | Present | No content operation |
| Changed identity | Present | AddRevision(base=true), SetHead(expected prior head), resolve this anchor's breaks |
| Same identity, retracted | Present | AssertBase, resolve this anchor's breaks |
| New registration | Missing/Broken | Error; no logical |
| Existing anchor, base=true | Missing/Broken | RetractBase(current head), OpenArtifact(AnchorBroken) |
| Existing anchor already broken | Same missing/broken observation | No repeated break artifact |

AddRevision dedup allocation bindings are used even when content already exists;
no assumption of a new dense ID is made. Old revision bases are preserved on
head moves. A historical deleted identity restored while another head is current
uses AddRevision(dedup) + AssertBase on that identity + SetHead with old-head CAS.
Only open AnchorBroken artifacts for this logical are resolved. No validation
record is created by the observer.

Supported resource bounds are <=1000 anchors/scope, <=16 MiB/file, <=256 MiB
captured bytes/scan, <=3 attempts within a 5-second observer deadline, 250-ms
watcher interval, and <=40 ms for daemon live capture inside existing hook budgets.
Exceeding any bound is Unknown/unavailable for the whole scope: never partial
watermark publication. Git output is bounded by the expected OID/HEAD/path counts;
child processes are killed and reaped on the remaining deadline. File reads use
safe component-by-component openat from an open root fd, O_NOFOLLOW on each
component and O_NONBLOCK on the leaf, followed by fstat regular-file validation.
FIFO/device/symlink replacements cannot block or escape through a followed parent.
Root identity is checked before and after capture. Retries discard all prior bytes.

Capture all registered paths, then submit their changes and one
ObserverWatermark CAS in one bounded batch on the observer socket. Duplicate
creations, head CAS or watermark conflicts discard the entire attempt; reload
published state and recapture, with a bounded retry count. Never retry an old
captured batch against a newly loaded watermark. The committed descriptor blobs
are durable before the op_log batch that references them. Partial blob writes
without a batch have no kernel effect.

Deletion is not inferred from a Git index removal alone: the worktree is the
source. Git rename is deletion of an old path and creation at the new registered
path, with a non-binding rebind suggestion in the break descriptor. An unregistered
new path is not silently made a dependency. Checkout-induced churn is documented.

## 4. Watermark and read-time freshness

A committed position is a 32-byte BLAKE3 digest of model::Wire bytes: `CTXPOS01`,
u32LE version=1, Wire String(repo-id), Wire Vec<u8>(raw HEAD OID; empty for unborn),
then a Wire Vec of sorted entries `(Wire String(path), u8 tag, Wire Vec<u8>(value))`.
Tags: 1=Present (canonical descriptor bytes), 2=Missing (empty), 3=Broken (bounded
stable reason-code bytes, never a changing OS error string). Duplicate/out-of-order
paths, unsupported versions and trailing bytes are rejected. It fits the existing
64-byte position bound. Session/outbox facts are NOT mixed into this digest. A successful scan fingerprints every
registered path; a changed registration set is a different position.

Capture uses two complete successive reads of HEAD and manifest, requiring equality
before publishing. A writer can still race after the sample (the documented
TOCTOU window); the next live sample detects it. Repeated instability is Unknown,
never a partially captured watermark. Even an unchanged manifest can advance the
watermark after a HEAD move. A true no-op advances neither content nor watermark.

Freshness is never stored in DerivedSnapshot and never reconstructed from session
telemetry. Missing live evidence, a routing/configuration error, a pending edit,
capture failure, deadline or mismatch is ObservationLag/Unknown for affected
certificate entries. At P5a, missing live_pos for a CodeAnchor scope becomes
lagging (amendment to the P3 API convention that absent evidence meant fresh).
The observation/path support closure is separate from the effective certificate
vector. Starting at each required root, follow its canonical L2 witness; for Sem
visit BOTH the asserted premise p and current head h, for Pin/Cur visit p, for Ref
visit neither. Deduplicate by RevisionId, not LogicalId; old p and h can coexist.
Collect all CodeAnchor scopes/paths. The effective certificate vector may still
represent Sem by h to avoid a false Ambiguous. Freshness/strict watermark auditing
must include every scope in this separate closure, even a scope hidden behind p.
The hand-written regression has p supported by anchor scope A and h by B: changing
A without reconciliation must make the root Unknown even while B is fresh.

Pending outbox edit hints independently make affected scopes Unknown until a
successful stable full scan covers an explicit per-stream sequence cutoff. Atomically
snapshot that cutoff BEFORE the first capture; only those sequence numbers can be
covered. Hints arriving during or after sampling remain pending, never swept up
by an end-of-scan cutoff. Record
covered cutoffs as observer runtime/session delivery metadata, never kernel truth.
The registry/manifest must also match that scan. Missing/corrupt hint state is
Unknown. Acknowledging a covered hint does not change the position or advance a
watermark; a newer hint beyond the covered cutoff conservatively restores Unknown.
Restart loses the in-memory coverage proof and needs a new stable full scan.

The daemon derives the scopes from committed CodeAnchor inputs and trusted repo
routing, then captures live positions for certified Recall/Read and Check.
Client-supplied live positions cannot override a more conservative local result.
Certified read failures are withheld from the required read set; preserve the
failed/advisory audit fact with required=false under the existing read contract. Advisory
reads can show the reason. MCP may not turn a pending anchor into CERTIFIED merely
because its client omitted live_pos. Raw core tests must supply matching positions
to request fresh summaries for anchors.

Unknown routing must also fail closed after daemon restart. Routing is configured
explicitly by the admin/observer; it is not inferred from arbitrary agent bodies.
The registry can be reconstructed from committed anchor descriptors, with root
paths supplied by trusted local configuration. Any missing/corrupt descriptor or
config yields Unknown until repaired/reconciled, not a fallback fresh result.

## 5. Path-filtered checks

For Edit/Write/Patch with a nonempty, successfully normalised path set, select
required session reads whose stored L2 witness closure reaches an anchor on any
target path, using the union of original and current support closures. Recursively examine all required reads, not merely reads whose own
kind is CodeAnchor. The closure follows canonical witnesses, preserves Pin/Cur/Sem
revision semantics and never follows Ref edges.

Include unanchored required reads conservatively. Bash/unknown/empty/invalid path
sets check the full session certificate. Selection is scoped to the bound agent's
view and must not mutate or acknowledge the full session read ledger/certificate.
Persist a per-required-root certificate at its first certification, with its
original since_epoch/log link, original observation support paths/scopes and
scope-watermark baseline. Re-reading explicitly can recertify, but selecting a
path never reissues or resets a strict window. Validate selected root proofs with
their original historical windows; merge their effective vectors and preserve
Ambiguous detection. Shared scopes use the earliest selected root's since_epoch.
Return relevant diffs while leaving full-session context_update unchanged.
Old certificates without root proofs yield Unknown{MissingBaseline} for filtered
checks and any strict/historical observation check (including full session and
explicit cert_id checks) until the caller explicitly rereads. Non-strict full
checks may use the current complete closure without claiming an old baseline; do not infer a historical proof from
current witnesses. Test relevant ABA rejection and unrelated ABA exclusion. An explicit
cert_id with an action is filtered by the same rules, never by a client-supplied
list of logical ids. No selected roots means Empty/warn, not invented validity.

Normalise repo-relative and absolute paths using the same supported domain as
registration. Compare filesystem identity (device/inode) as well as spelling
for existing targets; a case alias such as src/A.rs == src/a.rs must not exclude a
required root. Component matching alone is insufficient on this machine's default
case-insensitive volume. A target whose identity cannot be established, a missing
target, parent symlink, nlink>1 or inconsistent canonical spelling falls back to
full checking. Prefix matching is component-aware; src/a does not match src/ab.
Paths outside the bound repo, symlink ambiguity and unknown repos fall back to a
full check. Same-user callers remain cooperative, not a P9 security boundary.

## 6. CLI, hooks and protocol

Provide `ctx anchors add <dir> <repo> <paths...>`, `scan`, `watch`, `status` and
Git-hook install/uninstall. Installation composes with existing hooks and keeps
backups; it may not silently replace an existing command. Post-commit,
post-checkout and post-merge request reconciliation, never determine content from
HEAD blobs alone. PostToolUse delivers dirty-path hints through the existing
durable outbox. A dirty hint is not itself a kernel mutation or fresh watermark.

`ctx check --strict --range <a..b>` resolves commits without executing shell code,
gets NUL-delimited changed paths (renames include both old/new), reconciles anchors,
and selects ALL current-head records reachable from those registered anchors via
support incidences and Sem virtual edges (never Ref). It does not intersect away
roots merely because this session has not read them. It checks these roots
and exits nonzero on stale, conflicted, unknown or unavailable dependencies.
It does not silently reuse the interactive session's unrelated read set. The
pre-push hook handles ALL pushed ref pairs from stdin. A deletion is skipped;
new refs use the object-format-specific empty tree as the diff base. Existing
remote OIDs must resolve locally or fail Unknown (no implicit network fetch).
Initially every nondeleted local OID must equal the current HEAD, and the complete
registered worktree manifest must match that commit's raw blobs/types. Non-HEAD
pushes, dirty registered files, filter/CRLF differences and missing objects fail
Unknown; never use a worktree sample as proof for another pushed tree. Unrelated
unregistered dirt is not a false claim about registered anchors. The hard gate
is installed only on operator request and rejects every non-Valid result,
including Pending, Empty, missing registration/routing and unavailable evidence.

Range is a point-in-time repository check: it issues a new audit certificate and
its strict window begins at that issuance, then validates against live positions.
It does NOT claim to prove historical ABA freedom throughout a..b or for earlier
agent actions. The commit range selects paths only. Existing per-session --strict
retains its original read/certificate window. CLI/docs and tests must distinguish
these meanings explicitly; range never silently presents a newly issued proof as
an old session's historical proof.

Observer queries expose bounded per-scope committed anchor state (head/base,
descriptor and breaks/watermark), not mutable Db access. If new request variants
are required, bump the protocol version and fail mismatches explicitly. Do not
rewrite the running dogfood binaries during development. P4 session/outbox logs
must remain readable. Add a root-proof session event without changing existing
Certificate canonical bytes. New readers accept legacy format-1 events and write
new format-2 segments. Before the first new event, force rollover from any
format-1 tail regardless of segment size; never append a new tag to a legacy
segment. Old readers must reject a future segment header BEFORE
any doctor truncation. Old root proofs are not fabricated during replay. Protocol
version becomes 3. Tests open P4 session logs with P5a and reject P5a logs with
old-version readers without modifying them. Downgrade after writing new events is
unsupported. No live upgrade is performed as part of development.

## 7. Verification and honest completion

G0: independent counterexample review of this ADR and any SPEC amendments before
implementation. G1: standard check script; anchors added to light-client dependency
rules. G2: exact snapshot/counters for each P5a T-ID, plus byte/path/Git-OID bounds,
atomic retry, symlinks, duplicate deletion, restore-old-head and closure freshness.

G3: PROP12 uses an independent Git-history reconstruction oracle (does not call
the production classifier); 10,000 fixed seeds with insert/edit/delete/restore,
CAS races and observer-lag coverage floors fixed before the run: delete >=5%,
identical restore >=5%, old-head ABA >=5%, CAS retries >=3%, lag >=3%, unavailable
capture >=2%, alias/unsupported paths >=2%, SHA256 repositories >=1% of generated
transactions. Counts are actual exercised events, not labels on test branches. Mutation classifier survivors
are killed or explained; oracle may not depend on anchors.

G4: interrupted observer before/after blob and batch durability, watcher missed
events, corrupted descriptors, restart without routing and concurrent wakeups.
G5: retain P4 gate budgets; report capture/reconcile costs separately for 10/100/
1,000 anchors, including deadline/failure outcomes rather than omitting them.

G6: replay >=3 real repositories, >=1,000 commits each, with >=200 HUMAN labelled
anchor-change pairs and test changes only auxiliary. Record repo URLs, commit ids,
selection rule and every unlabelled pair. Synthetic fixture success is not a
scientific invalidation-quality result. Automated replay can prepare labels but
cannot approve them. P4's dogfood and this human ground truth can independently
keep Gate P5a open after all implementation checks pass.

G7: all P0–P4 regression gates carried forward. G8: update README, semantics,
CHANGELOG and implementation status, retaining every pending/failing layer.

## 8. Review log

r0.1 independent review found six groups: Sem asserted/head observation closure;
pending-hint versus watermark separation; preserved per-root filtered strict
proofs; range roots/pushed-tree/baseline contract; real filesystem aliases; and
encoding/limits/coverage/log compatibility. r0.3 also freezes pre-capture hint cutoffs, forced log rollover, all-parent
symlink rejection and missing legacy strict baselines. SPEC v1.6.1 mirrors these
constraints. A non-implementing reviewer confirmed all findings closed on
2026-10-21: no remaining blocker/major. G0 passed; implementation is authorised
under A11. All implementation gates and the final independent review remain open.
