# ADR 0018 — P10 team exchange (root / edge)

Status: **Draft (r0.5), G0 not yet passed; open points decided (§0.1).** r0.1's review found 5
blockers, 10 majors and 15 minors; r0.2's closure check closed most of them
and found 3 blockers, 7 majors and 17 minors in r0.2's new join between the
two stores; r0.3 replaced that join with ordinary local records, and its
closure check found 2 blockers, 9 majors and 17 minors in how mirrors fit
the kernel (§14). r0.4 gave mirrors a replica origin the kernel enforces; its closure check
found no blockers, 5 majors and 14 minors, answered in r0.5.
Owner decisions of 2026-11-15 are in §0; the open points in §0.1 need the
owner before G0.

## 0. Scope and decisions taken

Several developers each run their own ContextDB (an *edge*: one `ctxd` per
clone). They want a shared body of team knowledge — conventions,
architecture decisions, plans the whole team relies on — kept in one place
(the *root*), seen by every edge, usable as a dependency, and invalidated
precisely when it changes.

Owner decisions (2026-11-15):

- **D1, phase order.** Team exchange is the new P10. The old P10 (eager /
  lazy maintenance, scale, payload GC) becomes P11; the recursion half of
  the old P11 becomes P12 (research). §12 lists the SPEC changes.
- **D2, the root decides.** Heads of team records are decided at the root,
  not by each edge. Sync runs from git hooks on every fetch and push. The
  owner asked whether an automatic branch should handle it (§6 answers: no
  kernel branch for proposals; an automatic, durable proposal queue).
- **D3, transport is the git repository** the team already shares. There is
  no root daemon to run.
- **D4, only explicit team lanes leave a machine.** A lane is shared only if
  the team document lists it and local P9 egress allows it. Everything
  else, personal lanes included, stays local.

The design in one paragraph. The root is an append-only, linear chain of
signed git commits under one ref of the shared remote; the git server's
refusal of non-fast-forward pushes makes the chain order the root's single
decision (§3). Every edge replays the chain into a **replica**, a separate
store that holds nothing but the chain, so which entries are accepted is a
pure function of the chain (§4). Each accepted team record is then
**mirrored** into the edge's own store as an ordinary local record in a
reserved namespace, written on every open branch by a trusted replica
channel, the way the observer writes code anchors: its head is the root's
head, and it is base exactly while the record is certified at the root
(§5). Mirrors carry a **replica origin** that the kernel enforces where it
today protects observer-written code anchors (admission, policy, merges,
observation scopes). Local records depend on mirrors with the existing edge
kinds, so a change at the root invalidates them precisely, and
certificates, WHY, recall and P9 treat mirrors as local records. Lag reuses SD14b's
observation watermark (§8). Proposals wait in a durable queue and the
developer's own `git push` signs and publishes them (§6, §7).

Out of scope for P10: multi-master writes; live (non-git) sync; partial
replication of a lane; replicating sessions, certificates, validator jobs,
embeddings or the git mirror; Sem premises and validators inside team
knowledge (O6); code anchors in team knowledge (O1); recursion.

### 0.1 Open points (owner)

**Decided 2026-11-16:** the owner chose the recommended option for O1, O2,
O5 and O7; O3, O4 and O6 take the recommended option (presented to the
owner, no objection). The options below are kept for the record.

- **O1, code anchors in team knowledge.** Recommended: none in P10; team
  records name code in text or `Ref`. A later milestone anchors team
  knowledge to the blob OIDs of the pushed commit (§10).
- **O2, shred.** Recommended: chain payloads are plaintext git objects; a
  team shred removes the content from every replica and mirror on import,
  and the remote keeps the bytes until a compaction and everyone's
  `git gc` (§11). Declared. Alternative: encrypt chain payloads with a
  team key distributed out of band.
- **O3, depending on one's own unpublished proposal.** Recommended: refused
  (`Unpublished`): a local record can depend only on mirrors, which exist
  only for accepted team records. Cost: an agent cannot depend on a team
  decision it just proposed until a push publishes it and an import
  mirrors it. Alternative: allow it, with the local dependent
  `PENDING(Unpublished)` until then.
- **O4, ref location.** Recommended: `refs/contextdb/team/<team-id>/g<n>`
  (generation *n*, §8.6). Fallback per team: an orphan branch
  `contextdb-team/<team-id>/g<n>` for hosts that refuse custom refs, which
  also allows branch protection.
- **O5, publish policy.** Recommended: `publish = "auto"` lists the
  proposals in the `git push` output and publishes them; it becomes
  `confirm` automatically while the queue holds proposals of more than one
  local principal. `confirm` publishes only after `ctx team publish --yes`.
  A failed publish never fails the code push unless `strict_push = true`.
- **O6, semantic premises in team knowledge.** Recommended: team rules use
  `Pin`, `Cur` and `Ref` only; Sem inside team knowledge would need the
  validator's verdict as a team op to stay deterministic. Local rules may
  use any edge kind on mirrors, Sem included, because a local verdict is
  local (§5.3).
- **O7, lag at the pre-push gate.** Recommended: `ReplicationLag` alone
  does not fail P5a's pre-push gate (it warns), and the gate runs after a
  bounded synchronous team sync (§7.2). Alternative: lag fails the gate like
  any non-Valid certificate.

## 1. Today

- Each clone has its own store; ids (`LogicalId`, `RevisionId`, `RuleId`,
  `BranchId`, `PrincipalId`) are dense integers local to that store
  (SPEC §6.1 contract 3, `contextdb-core/src/stage.rs`). Only `key`,
  `ContentHash` (BLAKE3 of the payload) and git blob OIDs mean the same
  thing in two stores.
- op_log ops reference local ids (`LogicalRef::Existing`, `HeadVector`,
  `Merge.source`, the batch principal); frames cannot be replayed
  elsewhere.
- The kernel's checks read store-wide state: the global key namespace
  (`LogicalExists`), per-branch A5 policy, the shred set, the blob store
  (`MissingBlob`), P9 authorisation, u32 capacity.
- SD19 (P6): every branch is its own copy-on-write `Db`; heads, base,
  enabled flags, artifacts and ValidationRecords are in the per-branch
  overlay; the only store-global ops today, `SetAccess` and `ShredContent`,
  change no derived state (m-N1).
- P5a: the observer commits CodeAnchors on the observer channel to the
  branch of the worktree's git branch, and an `ObserverWatermark{scope}`
  per branch, compared at read time with a live position, yields
  `PENDING(ObservationLag)` (SD14b, `contextdb-core/src/cert.rs`).
- P5a installs `post-commit`, `post-checkout`, `post-merge` and optionally
  `pre-push` under one marker (`contextdb-cli/src/anchors.rs`); P8 runs
  git with `core.hooksPath=/dev/null` and moves refs only by CAS.

Verified on git 2.56 (2026-11-15):

- an added `remote.<r>.fetch` refspec brings a custom ref along with every
  plain `git fetch` / `git pull`;
- `reference-transaction` runs, in the phases `preparing`, `prepared`,
  `committed` or `aborted`, whenever fetch updates that ref;
- of two clients appending to the same ref, the second push is refused as
  non-fast-forward;
- a nested `git push` from inside `pre-push` works;
- `pre-push` also runs for `git push --dry-run`, with identical arguments
  and stdin;
- in a `--filter=blob:none` clone a plain fetch does not bring the team
  blobs, and `git fetch --no-filter` does.

## 2. Threat model

### 2.1 Parties

- **Team members**: hold a signing key the team document lists and push
  access to the remote. Trusted cooperatively within their roles: a writer
  can write wrong content, as on one machine.
- **Agents** acting for a member: can propose, and with `publish = "auto"`
  publish, under the member's signature. A prompt-injected agent can write
  wrong team knowledge within the member's roles; `confirm` (O5) and the
  push-time listing reduce it.
- **The remote host**: trusted for availability, not integrity. It can
  withhold commits, serve stale refs, delete refs or rewrite history.
- **Anyone with push access but no listed key**: can append entries, which
  every edge rejects; can delete or rewrite refs (§8.6); can append a huge
  entry that every edge downloads before any size check (m-N14).

### 2.2 Guarantees

- **G-ROOT.** Two edges that imported the same chain prefix with the same
  `accept_version` accepted exactly the same entries, and have the same
  **team digest** (§4.3), whatever their local stores hold and whichever
  ContextDB versions they run.
- **G-AUTH.** A team op takes effect only if its entry is signed by a key
  the team document at the parent lists for the member named in
  `Ctx-Author`, and that member's roles allow the op.
- **G-TRUST.** An edge follows only a chain that starts at the genesis OID
  it was given at join (out of band), and successors declared by a signed
  `Compact` entry on a chain it follows (§8.6).
- **G-ORDER.** The root's order is the chain order. A head changes only by
  an entry whose expected values match the replica at its parent.
- **G-LOCAL.** Only proposals into listed team lanes are ever written to
  the chain; local records, local rules and egress-off lanes never are.
- **G-PREC.** On every open branch, a local dependent of a team record
  follows the root precisely once that branch is caught up: `Pin`
  dependents go STALE and `Cur` dependents lose support when the root moves
  the head, and every dependent loses support when the record stops being
  certified at the root, e.g. its base is retracted there (§5.3).
- **G-LAG.** A read whose support includes a mirror of team *T* is never
  CERTIFIED while the edge has fetched a root it has not mirrored on that
  branch, or while import is stopped (§8).

### 2.3 Not guaranteed (documented)

- Freshness against the remote: an edge that has not fetched cannot know
  what it missed. `[team] warn_after` warns; it cannot fail closed offline.
- Availability under a hostile host or member: ref deletion, rewrites, a
  flood of rejected entries, or one huge entry can stop or slow progress
  (§8.6, §8.8). Custom refs cannot be branch-protected; teams that need it
  use the orphan-branch fallback (O4).
- The revocation race: a member being removed can win the ref CAS against
  the admin's removal entry; entries before the removal stay.
- Removing bytes from the remote's history or from other clones (O2).
- Secrecy from anyone who can read the repository. The commit's author line
  is fixed (§3.1), but the signature still identifies the key.
- Archived branches are never caught up: what depends on a mirror there
  reads `PENDING(ReplicationLag)` permanently (§5.2).
- An edge offline longer than the retention of old generations stops with
  `ReplicaDiverged` and must rejoin (§8.6).
- Publish runs inside `pre-push`, before git sends the code: if the code
  push then fails, the team entry is already published (Q17).
- A contradiction between a local record and a team record can only be a
  one-sided local Conflict: no Conflict artifact may name a mirror, so the
  local side records it against its own record (Q17).

## 3. The root chain

### 3.1 Refs and entries

- **Team id**: a random 128-bit id chosen at creation and stored in the
  team document; it survives compaction (N9). Names use its 32 hex digits.
- **Generations**: the chain lives under `refs/contextdb/team/<team-id>/g<n>`
  (O4); a compaction starts generation *n+1* (§8.6). Each edge tracks every
  generation of its team as
  `refs/contextdb/remotes/<remote>/team/<team-id>/*` through one added fetch
  refspec.
- The chain within a generation is linear. `accept` checks that every entry
  but the generation's first has exactly one parent, and the first none;
  that `Ctx-Team-Seq` is the previous entry's plus one (it continues across
  generations, N9).
- **Genesis** (generation 0's first entry) holds the team document only. A
  later generation's first entry holds a **checkpoint** (§8.6) and
  `Ctx-Compacts: <OID of the Compact entry>`.
- Every other entry carries one or more **proposals**, each a separate
  sub-batch decided in order (§4.2). Its tree holds exactly `batch`
  (the portable ops of every sub-batch, §4.1, `CXTB` format) and `blobs/<hex>` for each
  payload it references that is not in the replica's **blob set**; any
  other tree entry rejects it. The blob set is defined by the chain alone
  (Q2): the bytes of every revision of an accepted proposal that is not
  shredded, plus the current entry's own blobs. Bytes carried only by a
  rejected proposal are not in it and must be sent again. Each blob's name must equal the BLAKE3 of its bytes.
- Trailers, parsed by ContextDB's own strict parser (duplicates and unknown
  keys reject): `Ctx-Team`, `Ctx-Team-Seq`, `Ctx-Author`, `Ctx-Batch-Hash`,
  `Ctx-Accept-Version`, and one `Ctx-Proposal: <128-bit random id>` per
  proposal in the entry.
- Author and committer must be exactly `ContextDB <team@contextdb>` at time
  0 +0000, and `Ctx-Accept-Version` must equal the team document's
  `accept_version`; otherwise the entry is rejected (m-N7).
- **Signatures**: SSH `sshsig`, namespace `git`, ed25519 keys only
  (`ssh-ed25519`; security-key `sk-ssh-ed25519` keys are refused in P10),
  verified in process against the team document at the parent, with no
  time-dependent options. GPG is not supported. In SHA-256 repositories the
  `gpgsig-sha256` header is the one checked.
- **Limits**: `batch` ≤ 1 MiB and ≤ 1,000 ops, each blob ≤ 1 MiB, an entry
  ≤ 8 MiB in all, checked before decoding.

### 3.2 Why this makes the root decide

The git server updates a ref atomically and refuses a non-fast-forward
push. An edge appends by pushing a child of the tip it imported; if anyone
appended first, the push is refused and the edge rebuilds on the new tip
(§7.3). The chain order is one total order, chosen by the server's ref CAS.
No timestamp or HLC decides anything.

## 4. The replica and `accept`

### 4.1 Portable team ops

A team batch names nothing by local id:

- **Keys**: a team record's key is `team/<team-id>/<path>` and its scope
  lane `team:<team-id>/<lane>`, `<lane>` being one of the team document's
  lanes. The `team/` key prefix and `team:` lane prefix are **reserved**:
  every local channel except the replica channel refuses them
  (`ReservedNamespace`). A join is refused while a local key uses the
  prefix.
- **Kinds**: Decision, Plan, Fact, Convention, Evidence. Summary is not a
  team kind in P10 (it needs Sem, O6; m-N4), nor is CodeAnchor (O1).
- **Create**: `New{key, lane, kind, body, adopt_as_base}`.
- **Revisions**: `(key, content hash)`; bytes from `blobs/` or the replica.
- **Every state change carries its expected value** (M3): supersede and set
  head carry the expected head; assert / retract base the expected base
  bit; enable / disable the expected flag; metadata the expected metadata
  hash.
- **Rules**: `(conclusion key, conclusion content)` and premises
  `(key, kind)` with kind `Pin(content)`, `Cur(content seen)` or `Ref`. A
  `Cur` premise records the content its proposer saw and is rejected if the
  head moved since (N7). `Rebind` is `disable` plus `add`.
- **Others**: `SetTeamAccess` (sole op of its entry, admins), `TeamShred`
  (admins, §11), `Compact` (sole op, admins, §8.6).
- **Not allowed**: CodeAnchor, Sem premises, ObserverWatermark,
  ValidationRecord, Conflict artifacts, branch ops, `SetAccess`,
  `ShredContent`.
- **Closure**: every premise and reference of a team rule is a team record
  (`TeamDepOutside`).

### 4.2 The replica and a pure `accept`

Each joined team has a **replica**: a separate single-branch store under
`<data>/teams/<team-id>/`, whose only input is the chain. It contains no
local data: no local keys, policy, shred set, blob store, access document,
validators or Conflicts.

`accept(replica at parent, entry)` runs, in order: the structural checks of
§3.1; the signature; `Ctx-Author` and roles from the team document at the
parent; decoding; then the kernel on the replica under the team document's
access mapping (§9.1) and A5 policy. The outcome is `Accepted` (the batch
commits on the replica) or `Rejected(reason)`.

- **Each proposal is decided separately (m1).** An entry's proposals are
  applied in order, each as its own kernel batch on the replica (A5 at the
  end of each); each is `Accepted` or `Rejected(reason)` on its own. An
  entry-level check failure (structure, signature, author, trailers, size)
  rejects every proposal in it.
- **Every deterministic outcome rejects; only the undecidable stalls (P6).**
  Any error the checks or the kernel return for the entry's content —
  every `TxError` of SPEC §6.1, `ImmutableKind`, `InvalidKey`, `UnknownRef`,
  `LogicalMismatch`, `PolicyViolation`, `InvalidMetadata`, `CapacityExceeded`
  included — rejects. Import **stops** (`ReplicaStalled`) only on I/O
  errors, missing objects (`ReplicaIncomplete`), an `accept_version` this
  edge does not implement, or a panic. So no crafted entry stops a team.
- **`accept_version` freezes acceptance and team support (P7).** A version
  fixes the parsers, the checks, the rejection decision and **team support
  v1**: the function from the replica's inputs to each team record's team
  status (§4.3). Team rules have no Sem and no recursion, so v1 is the
  eager counting semantics of SPEC SD1–SD12 restricted to Pin, Cur and Ref,
  specified in SPEC SD21 and frozen. Later phases (P11 lazy maintenance,
  P12 recursion) change the local kernel, not team support v1; changing it
  is a new `accept_version`.
- **Golden corpus**: `tests/team-chains/` holds chains with their expected
  accept / reject / stall outcomes for every version; every release runs it.
- **Decisions are logged** in the replica's op_log, so a restart reads them;
  `ctx team rebuild` recomputes them with the same version.
- **Scratch accept**: the WHAT_IF of §6.2 and the pre-push check of §7.2 run
  `accept` on a copy-on-write clone of the replica's `Db`, never on the
  replica itself (m-N6).

### 4.3 The team digest and team status

**Team status** of a record (under team support v1) has two values (Q4):
`certified` if its head is supported — the head revision is base, or it is
the conclusion of an enabled team rule whose premises are certified (for
`Pin` premises, at the pinned content) — and `stale` otherwise. There is no
separate "retracted" status: a root retraction is a `RetractBase` on the
head, after which the record is `stale` unless a team rule still supports
it.

The **team digest** is a BLAKE3 over the canonical encoding of the
**keyed team state**, sorted by key (m9):

- per record: key, kind, lane, metadata (tags), team status, and every revision `(content, base bit, shredded bit)` in
  creation order with the head marked;
- per rule: `(conclusion key, content)`, premises, `enabled`;
- the team document, the shred set (content hashes), the set of accepted
  `(member, proposal id)` pairs, and the sequence number.

It names no id, so edges that replayed the whole chain and edges that joined
from a checkpoint compare equal, and it covers everything `accept` reads
(P9). Replica principal ids affect neither `accept` (roles are by member
name) nor the digest (m10).

### 4.4 No validators, no Conflicts

Team rules have no Sem premises (O6): nothing in the replica waits for a
validator, and no Conflict artifact is a team op. In P10 the replica is
eagerly maintained with no Sem and no Conflicts, so a replica summary is
CERTIFIED or STALE, never PENDING (m-N3).

## 5. Mirrors in the local store

### 5.1 What a mirror is

For every team record, the edge's store holds a **mirror**: a local logical
with the team record's key, kind and scope lane, whose **origin** is
`Replica{team}`. Origin is a new attribute of a logical, set once at
creation by the replica channel and part of the logical's identity in the
op_log, snapshots and digest. The kernel checks origin, not the key prefix
and not the kind, everywhere P5a checks `RecordKind::CodeAnchor` for the
observer (P3 of r0.3):

- **Admission** (§2.5, new A8): only `Channel::Replica` may create a
  replica-origin logical, add its revisions, move its head, change its base
  or metadata; every other channel gets `ReplicaReadOnly`. A replica-origin
  logical is never the conclusion of a local rule, the target of a
  Rebind, or a side of a Conflict artifact (`ReplicaReadOnly`).
- **A5**: replica-origin logicals are exempt from the local policy's
  dependency requirements, both when written and in the full scan a later
  `SetPolicy` runs (`txn.rs` A5 scan).
- **Shred**: a mirror revision whose content is in the local shred set is
  created as unavailable instead of refused (`ContentShredded` never
  refuses a replica-channel write), and reads UNAVAILABLE (P4).
- **Merges**: P6 merges never take replica-origin state (head, base,
  metadata, revisions, visibility): mirrors are excluded from three-way
  classification, and unlike CodeAnchors (which a merge carries as
  `anchor_unobserved`) a mirror is never made visible on the target by a
  merge; only catch-up attaches it (P2). A merge that would carry a local
  rule whose premise or reference is a mirror revision not visible on the
  target is refused with `MirrorLag` (deterministic, kernel and oracle),
  and `ctxd` catches the target up before it executes any merge (Q1).
- **Other local refusals**: a replica-channel batch can be refused only by
  `CapacityExceeded` or I/O; the channel admits only replica-origin ops,
  `MirrorAttach`, `ObserverWatermark` on `team:` scopes and the shred step
  of §11 (Q10). A refused catch-up stalls that branch (`ReplicaStalled`
  with the branch named), never the others.
- **Owner**: the replica channel's batches run as the store owner's
  principal, so mirrors are owned by the store owner (m4).

A mirror holds (P8):

- **revisions**: every team revision the edge has imported, with the same
  content (payload copied into the local blob store, encrypted like every
  payload);
- **head**: the team record's head;
- **base**: on the head revision only, true exactly while its **team
  status** (§4.3) is certified; an old head's base is retracted before the
  head moves;
- **metadata**: the team record's tags only. Team `dirs` are never mirrored,
  so a team member cannot change local P9 dir grants (Q9). The team status
  and sequence are not stored in the mirror: `ctx_read` shows them from the
  replica;
- **no rules**: the team record's justification stays in the replica.

### 5.2 Making mirrors visible on every branch

The key namespace is store-global (SD19) and logicals are made visible on a
branch only by creation there or by merge, so r0.3's "create the mirror on
each branch" fails with `LogicalExists` on the second branch (P1). So:

- a mirror is **created once**, store-wide, by the first catch-up batch that
  needs it; catch-up always runs `main` first, then the other open branches
  in id order (Q7);
- a new replica-channel op, `MirrorAttach{key, head, base}`, makes an
  existing replica-origin logical visible on another branch and sets its
  head and base in the same op. Its operand is the key, resolved through a
  kernel index from reserved keys to replica-origin logicals (the kernel
  otherwise keeps keys only as a set), because an invisible logical id
  would be `UnknownRef` (Q7). It is refused for any other logical.

**Catch-up** brings one open branch to the replica's sequence *s*:

- it diffs that branch's mirror state against the **replica projection** at
  *s* (not against the branch's previous watermark), so whatever a fork or
  an old snapshot left is corrected;
- each batch holds `MirrorAttach`, revisions, base retractions, head moves,
  base assertions and tags, in that order, plus
  `ObserverWatermark{scope: team:<team-id>, position: <entry OID>}` as a CAS
  on the branch's previous position;
- **one net batch per branch per import** (Q5), except that when a mirror's
  head returns within the import to a content it had earlier in that
  import (a→b→a), the import is split at those entries so each head move is
  its own batch and `strict_no_aba` sees it (m7). The replica logs a
  per-entry delta of the keyed state, so the projection at any entry since
  the last checkpoint is available; a join or a long-offline edge gets one
  net batch per branch;
- **invariant** (checked by `ctx doctor` and after every catch-up in
  `paranoid` builds): on every open branch whose watermark is *s*, mirror
  state equals the replica projection at *s* (P2, m16, PROP19).

Forks inherit the parent's mirrors and watermark and are caught up from the
projection; a branch untouched since its fork gets its first copy-on-write
copy at that catch-up, which G5 reports. Archived branches are not written;
SD19 has no unarchive, so anything on them that depends on a mirror reads
`PENDING(ReplicationLag)` from then on (m5). A merge never changes a
mirror, so after a merge the target's mirrors are still its own and its
watermark still describes them.

### 5.3 Local dependents

Local rules depend on mirrors with the existing edge kinds:

- `Pin(r)`: STALE when the root moves the head off *r* (Pin-strict);
- `Cur`: loses support when the head moves (SD3 `sat0` requires the head),
  until the dependent is rebound or re-recorded (m3);
- `Sem`: allowed. A local validator verdict is local and does not affect
  any other edge (m12); PENDING after a root head move until re-validated;
- `Ref`: no propagation.

A root retraction, or the record ceasing to be certified at the root,
retracts the mirror's base, so dependents lose support. Certificates, read
sets, WHY / WHY_NOT / IMPACT and the pre-push gate treat mirrors as local
records (G-PREC).

**Certificates.** A certificate binds mirror `(logical, revision)` pairs as
for any record. A catch-up that leaves a mirror's head and base unchanged
does not invalidate certificates that read it; only the watermark advances,
and lag is decided at read time (P5).

### 5.4 Reading team knowledge

Mirrors are local records: `ctx_read`, `ctx_recall` (one FTS index; local
embeddings follow local egress rules), certificates and the UI need no
second store. WHY on a mirror answers `team record, <status> at the root
(seq s)`, and `ctx team why <key>` gives the replica's justification with
team records named by `(key, content)`, never by replica ids (m15).

Which local principals read team lanes is set by the local P9 document. A
new access-document section `[team_lanes]` gives the default entry for
`team:*` lanes, as `[code_lanes]` does for `code:*`; without it they fall to
`default_lane` (`*owner`). An absent section leaves `SetAccess`'s canonical
form and hash as today (m11). No team member is ever a local principal.

## 6. Proposals (D2): a queue, not a branch

### 6.1 Entering the queue

- A batch whose ops all target `team/<team-id>/` keys is a **team
  proposal**; a batch mixing team and local ops is refused
  (`MixedTeamBatch`). Its read set is in team terms: `(key, content seen)`
  for every team record it relies on, taken from the mirrors (M2).
- Request idempotency uses the client transaction id under SPEC §6.1
  contract 1: unique per data directory, so a team proposal's id can never
  be reused by a local batch or the reverse (`TxnIdReuse`) (m13).
- Egress is checked here: a proposal into a lane the local document marks
  egress-off is refused (`EgressDenied`).
- Roles: the WHAT_IF checks roles as the member bound to the store owner's
  signing key (§9.2). In multi-user mode the actual signer is the pusher;
  if that member lacks a role, the proposal becomes `conflicted` at publish
  (m-N5).

### 6.2 Checking and storing

The proposal is translated to portable ops (§4.1); one larger than an
entry's limits (§3.1) is refused (`ProposalTooLarge`, Q11). It is checked
by a scratch `accept` on a clone of the replica with every earlier
`queued` proposal applied — its `Cur(content seen)` is the content in that
overlay, so a record an earlier queued proposal created or changed is
seen as that proposal left it, and the dependency is recorded (Q16) — and appended to `<data>/teams/<team-id>/proposals`, an append-only
queue with checksummed frames. Each proposal records the earlier proposals
it depends on (any it reads or names). The reply is `proposed: <id>,
publishes on the next git push`.

### 6.3 States

`queued` → `published{commit}` → `accepted` | `rejected{reason}`;
`queued` → `conflicted{key, reason}` (§7.3) → `queued` (re-proposed) |
`dropped`; `published` → `queued` when the outcome was unknown and a fetch
shows the chain does not hold it (N7). A proposal whose dependency becomes
`conflicted`, `rejected` or `dropped` becomes `conflicted{dependency}`
itself (N7). Every transition is a queue frame.

Proposals are shown by `ctx team status` and as `proposed` next to the
mirror in `ctx_read`; they are never part of any branch's state (O3).

## 7. Publish (push)

### 7.1 Where it runs

The hook process only fetches, signs and pushes, in the developer's
environment (ssh-agent, credentials, the remote's push URL). Everything
that reads or writes a store — import, the scratch `accept`, building the
entry's `batch` and tree, queue transitions — is a request to `ctxd` on its
single writer (m14). If `ctxd` is down, publish is skipped with a warning
and the proposals stay queued.

Signing uses `-c gpg.format=ssh -c user.signingkey=<the listed key>`
whatever the user's git configuration says (the owner's global config is
OpenPGP with `commit.gpgsign`, N8). The key is bound at join (§9.2).

### 7.2 Steps

1. The `pre-push` dispatcher first runs a **bounded team sync** (fetch the
   team generations, import, catch up; ≤ `push_timeout`), so the gate does
   not see lag a sync would have removed (P11).
2. It runs P5a's gate. `ReplicationLag` alone does not fail the gate; it
   warns (O7).
3. Publish runs only if the gate passed, the push goes to the team's remote
   (by name and URL), and it is not a dry run. A dry run is recognised from
   the `git push` process's arguments, found by walking up from the hook's
   parent shell (`/proc/<pid>/cmdline` on Linux, `sysctl KERN_PROCARGS2` on
   macOS) and parsed with git's option grammar (`-n`, `--dry-run`, the
   abbreviation `--dry`, grouped short options such as `-nv`,
   `--no-dry-run`, `--`). If the arguments cannot be found or parsed,
   or the subcommand is not literally `push` (a git alias such as
   `git p` for `push -n`), publish is **skipped** with a warning:
   publishing is irrevocable (m2, Q11).
   The dispatcher refuses pushes that delete `refs/contextdb/team/*` or
   upload `refs/contextdb/remotes/*`.
4. Take the publish lock `<data>/teams/<team-id>/publish.lock` (pid
   recorded; a dead holder's lock is broken).
5. `ctxd` rebuilds every `queued` proposal on the replica tip (§7.3) and runs
   the scratch `accept` on them in queue order, each as its own sub-batch,
   exactly as import will (§4.2). `Cur(content seen)` is compared with the
   state after the earlier sub-batches of the same entry (m1). Those that
   pass go into **one entry**, split into several consecutive entries when
   the limits of §3.1 would be exceeded. Because each entry's parent is the
   tip just imported and `accept` is deterministic, the local outcome is
   every edge's outcome.
6. The hook signs and pushes `<commit>:refs/contextdb/team/<team-id>/g<n>`
   with `core.hooksPath=/dev/null`; `ctxd` records `published`.
7. On a non-fast-forward refusal, go back to 1's fetch and import, then 5,
   at most 3 times in all.
8. Print each proposal's outcome. The code push proceeds whatever happened,
   unless `strict_push` (O5).

`pre-push` does not run under `--no-verify` or from clients without hooks
(JGit, libgit2); `ctx team publish` does the same by hand, and
`ctx team status` warns about proposals older than `warn_after`.

### 7.3 Rebuilding on a new tip

Each proposal is re-translated against the new tip and checked with the
full `accept`. If it would be rejected for any listed reason — an expected
value changed, a `Cur` premise's head moved, a key now exists, a duplicate
rule, a cycle, a role removed, a content shredded — it is left out and
becomes `conflicted{key, reason}`, shown in the hook output; its dependents
follow (§6.3). The author's agent re-proposes on top of the new state or
drops it. No Conflict artifact is written anywhere.

### 7.4 Idempotency and crashes

Before pushing, publish searches the chain for each `(Ctx-Author,
Ctx-Proposal)`; proposal ids are 128-bit random, so two of one member's
machines never collide (N7). A proposal already on the chain is never
pushed again; its outcome is read from the replica.

| Point | State left | Recovery |
|---|---|---|
| Before the push | unreferenced commit object | ignored; next publish rebuilds |
| Push outcome unknown | `published`, maybe on the remote | next fetch: found → outcome from the replica; absent → `queued` |
| Pushed, not imported | `published` | next import marks it accepted or rejected |
| Lock holder killed | stale lock | broken by pid check |

### 7.5 Hooks

The P5a installer becomes a **dispatcher**: one ContextDB script per hook
name, running registered components in a fixed order, chaining the user's
original hook with a backup. Components:

- P5a: `post-commit`, `post-checkout`, `post-merge`, `pre-push` (gate);
- P10: `reference-transaction` (notify, §8.1), `post-merge` (notify),
  `pre-push` (publish, §7.2).

The fetch refspec is added with `git config --add` and removed on leave.
`ctx mirror status` lists only `refs/contextdb/b*` refs; team refs appear
in `ctx team status` (m-N12).

## 8. Import and lag

### 8.1 Import steps

1. `git fetch` (any fetch or pull) updates the tracking refs.
2. The `reference-transaction` hook, on `committed` only and only for
   `refs/contextdb/remotes/*/team/*`, notifies `ctxd`. In a partial clone, a
   plain fetch brings the team commits but not their blobs, and a later
   `--no-filter` fetch of the same refs brings nothing (P10, verified on
   git 2.56). So the hook starts, detached in the user's environment,
   `git rev-list --objects --missing=print` over the new team commits and
   fetches the missing objects by OID through the promisor remote
   (`git fetch <remote> <oid>…`), then notifies `ctxd` again (Q13). It never vetoes: in `preparing` and `prepared` it
   runs only the chained user hook and returns that hook's status (M8,
   m-N2 of r0.2).
3. `ctxd` resolves the tracking refs through git, applies the cases of
   §8.2, and imports into the replica entry by entry (`accept`), each
   outcome one replica batch, so a crash leaves a prefix.
4. It then brings every open branch's mirrors to the replica's sequence
   (§5.2).
5. `ctxd` also imports at start and on `ctx team sync`. Missing objects
   stall with `ReplicaIncomplete` until the hook's by-OID fetch (or
   `ctx team sync`, which runs it in the caller's environment) brings them.

### 8.2 Tracking-ref cases

| Tracking ref vs replica | Action |
|---|---|
| equal | nothing |
| descendant | import |
| ancestor (a stale fetch) | nothing |
| missing (pruned after the remote deleted it) | nothing; `ctx team status` warns `RootMissing` |
| unrelated and not a declared successor | stop: `ReplicaDiverged` |

### 8.3 Replication lag

SD14b's mechanism, with the team as an observation scope (m12):

- each branch's committed position is its `ObserverWatermark` for scope
  `team:<team-id>` (§5.2), written only by the replica channel; the
  observer channel is refused `team:` scopes (P3);
- a mirror's observation scope is derived from its **origin**
  (`Replica{team}` → `team:<team-id>`), not from its lane: `cert.rs`'s
  observation-support closure adds replica-origin logicals beside
  CodeAnchors (P5);
- the live position is the newest fetched entry of the **newest
  generation this edge follows**; older generations' refs are never
  watched, so entries appended to them cannot cause lag (Q3). It is the
  newest imported-or-fetched entry: `ctxd` keeps
  it in memory, updated by the hook's notify, by its own fetches and every
  `lag_check` (default 2 s) from the ref, so a read never spawns git
  (m-N11);
- a read whose canonical L2-witness closure contains a mirror of *T* on a
  branch whose watermark differs from the live position, or while import is
  stopped (`ReplicaStalled`, `ReplicaDiverged`, `ReplicaIncomplete`), is
  `PENDING(ReplicationLag)` with certificate `Unknown{lag}`; direct reads
  of a mirror are included (N5);
- never stored in L0/L1/L2; the oracle checks it at read time only.

`ReplicationLag` joins SD13's PENDING reasons right after `ObservationLag`.

### 8.4 Crash and intermediate states

| Point | State left | Recovery |
|---|---|---|
| Replica entry not committed | replica at the previous entry | next import redoes it |
| Replica ahead of a branch's mirrors | that branch's watermark behind; reads lag | next import or start writes the branch's batch (CAS on its watermark) |
| Some branches written, others not | each branch consistent with its own watermark | the rest are written next |
| Hook killed | nothing | next fetch, start or `ctx team sync` |

### 8.5 Leaving and rebuilding

- `ctx team leave` first records `leaving` durably in the team's state
  file; while it is set, start-up and import never catch up, so a crash
  mid-leave cannot re-assert bases (Q12). It then writes, on every open
  branch, a replica-channel batch
  that retracts every mirror's base and sets the watermark to the sentinel
  position `left` (m6): dependents lose support, and lag reads treat a
  `left` scope as not lagging, since nothing in it is base any more; then deletes the replica, queue and refspec after a
  confirmation. Mirror records stay as retracted history (the op_log is
  append-only); a later rejoin of the same team id reuses them.
- `ctx team rebuild` discards and replays the replica; mirrors are then
  brought to it by ordinary batches. The local op_log is never rewritten.

### 8.6 Compaction and history rewrites

- A compaction is an admin `Compact{generation: n+1, checkpoint: <OID>}`
  entry appended to generation *n*, naming the first entry of generation
  *n+1*. `Compact` is terminal: entries appended to generation *n* after it
  are **ignored** — not decided, not counted in the sequence, not part of
  any digest — because generation *n+1* continues the sequence (Q3). Generation *n+1* is pushed to a **new ref**; the old ref
  is not rewritten, so an edge that was offline sees `Compact` when it next
  fetches and follows (N9).
- The **checkpoint** holds everything `accept` reads (P9): the keyed team
  state of §4.3 — every record with all its revisions and their bytes
  (except shredded ones), per-revision base, metadata, every rule with its
  `enabled` flag — plus the team document, the shred set, the accepted
  `(member, proposal id)` pairs and the sequence number. It is signed by
  the admin who wrote `Compact`.
- An edge that imported generation *n* follows only if the checkpoint's
  team digest equals its own at the `Compact` entry. A **joiner** that
  starts at a later generation is given that generation's first-entry OID
  out of band (§8.7), as genesis is.
- The team id and `Ctx-Team-Seq` continue, so mirror keys and local
  dependents are unaffected. `ctx team rebuild` replays from the earliest
  generation still on the remote.
- Old generations are deleted by an admin through `ctx team prune` (it runs
  git with `core.hooksPath=/dev/null`, since the dispatcher refuses team-ref
  deletions) after `retain_generations` (default: never automatically). An
  edge offline past that point no longer sees `Compact` and stops with
  `ReplicaDiverged` (§2.3, m17).
- Any other rewrite stops import (`ReplicaDiverged`); recovery is
  `ctx team rejoin --genesis <oid>` by the store owner, with an OID
  obtained out of band.

### 8.7 Joining

`ctx team join <remote> --genesis <oid> [--key <ssh pubkey>]` (store owner):
the genesis OID — or, for a team compacted past generation 0, the current
generation's first-entry OID — comes out of band from an admin, never from
the remote. The
join checks that `--key` (default: the user's git signing key) is listed
for one member, installs the hooks (§7.5), fetches, verifies the chain from
that genesis, and imports. Joining the same team id twice is refused. A
rejoin whose chain gives an existing mirror key another kind or lane is
refused (`TeamKeyMismatch`): local keys are permanent, so such a team must
be joined under a new team id (Q14). After following a `Compact`, the edge
records the new generation's first-entry OID as its trust anchor, so
`ctx team rebuild` after a prune replays from a checkpoint it has already
verified (Q13).

### 8.8 Expensive or hostile entries

Size limits reject before decoding. A rejected entry costs its download,
one signature check and the size checks. A huge entry is downloaded before
any check (m-N14), and a member can slow publish by keeping the ref moving;
§2.3 declares both.

## 9. Identity and authorisation

### 9.1 The team document

```toml
version = 1
accept_version = 1
id = "5f0c…"                   # 128-bit, fixed at creation
name = "acme-backend"          # display only
admins = ["alice"]

[members.alice]
keys = ["ssh-ed25519 AAAA… alice@laptop"]
[members.bob]
keys = ["ssh-ed25519 AAAA… bob@desk"]

[lanes.conventions]            # scope team:<id>/conventions
writers = ["*members"]
curators = ["alice"]
[lanes.architecture]
writers = ["alice", "bob"]
curators = ["alice"]

[policy]                       # the replica's A5 policy
require_deps = []
```

Rules: a key belongs to one member only; at least one admin remains;
`SetTeamAccess` is the sole op of its entry and governs entries from the
next one on; removing a lane with live records is refused; `*members` means
every member, `*admins` the admins.

**Mapping to the replica's access document** (m-N13): the replica's
`AccessDoc` has a reserved owner principal that never signs and is never a
member; members get replica principal ids by P9's own rule; there are no
uids (the replica has no transport). Replica principal ids affect neither
`accept`, which checks roles by member name, nor the team digest (m10). Every team lane is readable by every
member; writers and curators map to P9 writers and curators.

### 9.2 Keys and local principals

`ctx team join` binds the store to one member through `--key`; publishing
signs with that key. In multi-user mode, every local principal's proposals
are published under the pusher's key: the WHAT_IF checks roles as the bound
member, the queue records the local author, and `publish` becomes
`confirm` while the queue holds proposals of more than one local principal
(m-N15). Which local principals may propose into a team lane is set by the
local P9 document's writers for that lane.

## 10. Code anchors (O1)

A code anchor's key and scope carry a repo-id derived from the local path,
which differs between clones; only the blob OID is portable. P10 has no
CodeAnchor in team knowledge. A later design: team anchors keyed by
repository-relative path under the team prefix, set at publish from the
blob OIDs of the pushed commit, observed by each edge as a read-time
dimension.

## 11. Shred and history (O2)

Shred stays content-global, as in P9 (m8):

- `TeamShred{content}` (admins) adds the content hash to the replica's shred
  set: every team revision with those bytes reads UNAVAILABLE in the
  replica, and no later entry may reintroduce them under any key
  (`ContentShredded`). Two team records sharing the bytes both lose them
  (m-N9).
- On the edge, before catch-up creates or attaches any mirror revision with
  those bytes, `ctxd` commits a local `ShredContent` as its own batch on
  `main` (SPEC: admin, sole op, `main` only), as the store owner (Q6) —
  **only if**
  no local record that is not a mirror holds them, so a team admin cannot
  destroy a developer's private records by naming their bytes (m5).
  Otherwise the local bytes stay readable through both records, and
  `ctx team status` reports it; declared.
- A local shred of bytes a mirror holds makes that mirror revision
  UNAVAILABLE locally and never refuses a mirror write (§5.1).
- The remote and every clone keep the bytes until a compaction removes
  them from the live generation, admins prune old generations, and
  everyone runs `git gc`. The orphan-branch fallback needs an admin to lift
  branch protection for that deletion. Declared in SPEC §10.

## 12. SPEC changes (at G0)

- §5 phase plan: P10 = team exchange; P11 = the old P10; P12 = the recursion
  half of the old P11 (research). Text tied to the old numbers is reworded.
- §1.4: "co-located, with team lanes replicated through git".
- **SD21, team knowledge** (SD20 is taken): the chain, the replica,
  `accept` and its versions, team support v1 (§4.2, §4.3), the team digest,
  replica-origin logicals and their catch-up, `ReplicationLag`.
- SD3, SD6, SD7, SD11, SD12, SD15: logical origin; mirrors as records whose
  head, base and metadata are inputs of the replica channel; team kinds;
  reserved namespaces.
- SD13: `ReplicationLag` after `ObservationLag` in PENDING reasons and
  precedence.
- SD14b: a replica-origin logical's observation scope is `team:<team-id>`;
  the `left` sentinel.
- SD19: replica-origin logicals are excluded from merge classification, as
  observer state is; `MirrorAttach`; open branches are caught up, archived
  branches are not.
- SD20: no Conflict artifact may name a replica-origin logical.
- §2.5: new admission rule A8 (replica origin), the A5 exemption, A1 for
  `MirrorAttach`.
- §6.1 contracts 1, 5, 6, 7 and 15: the client transaction id across team
  proposals and local batches; the replica channel's principal; origin in
  the op encoding.
- §6.4: strict certificates over mirrors (a catch-up that leaves a mirror
  unchanged does not invalidate).
- §3.5 / §6 / §7 / §8.3: op_log format 6 (`Channel::Replica`, origin,
  `MirrorAttach`, the `team:` scope for `ObserverWatermark`), protocol 8
  (team requests, proposal replies, `[team_lanes]`), errors
  `ReservedNamespace`, `ReplicaReadOnly`, `MixedTeamBatch`, `Unpublished`,
  `TeamDepOutside`, `ReplicaStalled`, `ReplicaDiverged`,
  `ReplicaIncomplete`, `NotTeamMember`, `EgressDenied`.
- `[team_lanes]` is part of `AccessDoc`; an absent section leaves
  `SetAccess`'s canonical form and hash unchanged.
- §10: the threat model of §2; P8's "ContextDB never pushes" is narrowed to
  the mirror.
- Migration: `ctx migrate` 5 → 6 changes the log header and snapshot format;
  existing ops keep their meaning; origin `Local` is encoded as absent, so
  existing logicals, snapshots and digests are unchanged (Q15); protocol 8
  is required.
- Further rows (Q8): A7 and SD18.3 / SD18.6 (observer-domain rules extended
  to the replica channel and `team:` scopes); the SD19 merge sentence (merges
  exclude replica-origin state and refuse `MirrorLag`); the SD19 P9
  supplement on `ShredContent` (the catch-up shred step); the §6.2 check
  order for `ReplicaReadOnly`, `MirrorLag`, `ReservedNamespace` and
  `MixedTeamBatch`; the oracle's input model and PROP13 extended with
  origin, A8, `MirrorAttach` and `MirrorLag`.

## 13. Tests and gate

- **T109** portable encoding round trip; reserved namespaces on every
  channel; no local id in a batch; `TeamDepOutside`; `MixedTeamBatch`;
  Summary and CodeAnchor refused in team knowledge.
- **T110** import through the real hook with two clones and a bare remote;
  every row of §8.2; idempotent re-import; a partial clone (plain fetch,
  then the hook's by-OID fetch; `ReplicaIncomplete` until it lands).
- **T111** publish through the real `pre-push`: a lost race rebuilds; every
  rejection reason of §7.3 gives `conflicted`, dependents follow;
  `--dry-run`, `--dry`, `-nv` and `--no-verify` publish nothing, and an
  unreadable command line skips publish; a push to another remote
  publishes nothing; signing uses SSH under an OpenPGP global config;
  several proposals in one entry are decided one by one; entry splitting at
  the limits.
- **T112** one corpus chain per `TxError` and per structural check: each is
  a rejection on every edge; an unknown `accept_version`, a missing object
  and an injected I/O error stall.
- **T113** lag: fetched not caught up, stalled, diverged, incomplete,
  archived branch, `left`; direct reads included; per-branch watermarks;
  mirror scope from origin, not lane.
- **T114** crash at every row of §7.4 and §8.4.
- **T115** precise invalidation on every open branch: `Pin` dependents
  STALE and `Cur` dependents unsupported on a root supersede; all
  dependents unsupported on a root retraction or loss of certification;
  recovery when restored; leave.
- **T116** certificates over mirrors: a→b→a across two imports and within
  one import is caught by `strict_no_aba`; a catch-up that changes nothing
  a certificate read leaves it valid; the pre-push gate under lag (O7) and
  after the bounded sync.
- **T117** P9: `[team_lanes]` and its absent-section hash; non-owner
  redaction of mirrors; local shred of a mirror's bytes, then a root entry
  reusing them (no stall); `TeamShred` with bytes a local record holds.
- **T118** compaction: an offline edge follows `Compact`; entries after
  `Compact` rejected; checkpoint digest check; a joiner from a checkpoint
  has the same team digest, including after a `SetHead` back to old
  content; prune and an edge offline past it.
- **T119** migration 5 → 6 and protocol 8; the hook dispatcher with a user
  hook chained in every hook name, including `reference-transaction`'s
  veto in `prepared`.
- **T121** r0.5 cases: a merge whose carried rule names a mirror the target
  cannot see is `MirrorLag`, and `ctxd` catches up before a merge (Q1); a
  rejected proposal's blob reused later must be re-sent, on full-replay and
  checkpoint edges alike (Q2); junk appended to a compacted generation is
  ignored and causes no lag (Q3); a team `RetractBase` on a head makes the
  record stale and its mirror's dependents unsupported (Q4); the shred step
  precedes mirror creation (Q6); a git alias for `push -n` and an oversized
  proposal (Q11); a crash in the middle of `leave` (Q12).
- **T120** mirrors in the kernel: a branch forked before a record exists
  gets it by `MirrorAttach`; a merge from a lagging or archived source
  carries no mirror state and the target stays consistent; every
  non-replica op on a mirror is `ReplicaReadOnly`; a local `SetPolicy` full
  scan with base mirrors; a local rule concluding a mirror is refused.
- **PROP17** (G3): random chains from several simulated members (valid and
  invalid entries) applied to edges with random local histories: the same
  accepted proposals and team digest on every edge, equal to an independent
  oracle that implements `accept_version 1` and team support v1 over the
  oracle's input model (in `contextdb-oracle`, not the kernel); 10,000
  seeds.
- **PROP18**: publish then import equals applying the proposals to the
  replica directly whenever the scratch `accept` passes.
- **PROP19** (the mirror invariant): after every import, catch-up, fork,
  merge, crash recovery and rebuild in random histories, every open branch
  whose watermark is *s* has mirror state equal to the replica projection
  at *s*; 10,000 seeds.
- **Golden corpus**: `tests/team-chains/` outcomes per `accept_version`,
  run by every release.
- **G4**: fuzz `CXTB`, the trailer parser, `sshsig` verification and the
  team document; kill points around import and publish; a remote that
  serves stale, deleted, rewritten, oversized or garbage refs.
- **G5** (initial budgets, ADR-calibrated):
  - import of 10,000 entries into the replica ≤ 60 s on the remote Linux
    host, fsyncs reported;
  - joining a team of 10,000 entries with 10 open branches ≤ 120 s (replica
    replay plus one net catch-up batch per branch), with copy-on-write
    memory reported (Q5, Q18);
  - catching up one entry on 10 open branches ≤ 50 ms, with op_log batches
    and fsyncs per import (branches × entries) and copy-on-write memory
    reported;
  - publish adds ≤ 2 s to `git push` for ten proposals on a local remote;
  - the `reference-transaction` hook adds ≤ 20 ms to `git commit`;
  - recall p99 with 10⁴ mirrors within +10 % of the same store without
    them (same record count).
- **G6** (pass criteria): two clones on one machine with two signing keys
  and a bare remote, then a real team remote, agents running on both edges
  for ≥ 5 days and ≥ 50 accepted entries from ≥ 2 members, with: equal team
  digests checked daily; 0 lost proposals (every one accepted, rejected,
  conflicted or dropped); every root supersede or retraction reflected as
  STALE or unsupported on the other edge within one fetch, measured by
  probe records that depend on each changed team record and are read
  after every fetch; 0 code pushes blocked by publish or by lag; no edge reads
  `ReplicationLag` for longer than one fetch after the last import (Q18).

## 14. Review history

- **r0.1 → r0.2** (review of r0.1: 5 blockers, 10 majors, 15 minors; section numbers in that review refer to r0.1): team
  state moved into a replica fed only by the chain; proposals became a
  queue. The r0.2 closure check found B1–B3, B5, M7, M9 and most minors
  closed.
- **r0.2 → r0.3** (closure check of r0.2: 3 blockers, 7 majors, 17 minors;
  partly closed B4, M1–M4, M6, M8, M10, m5, m8, m9, m15):
  - N1, N2, N3 (the global team-premise join did not exist in the kernel,
    could not be certified, and inverted `Cur`) → §5: mirrors as ordinary
    per-branch local records on the replica channel, base = certified at
    the root, existing edge kinds; B4 → §4.2 frozen rejection list,
    golden corpus; M1 → §5.2, §12 SD19.
  - N4 → §4.2, §4.3; N5 → §5.4, §8.3; N6 → §5.4 `[team_lanes]`, §9.1
    reserved owner; N7 → §4.1 `Cur(content seen)`, §6.2 dependencies, §6.3
    states, §7.4 random ids; N8 → §7.1 forced SSH signing, §7.2 dry run,
    one entry per push, team remote only, §8.1 partial clone; N9 → §3.1
    random team id, generations, §8.6; N10 → §13 G5 numbers, G6 criteria,
    independent PROP17 oracle, T116–T119.
  - M2 → §6.1; M3 → §4.1 expected values; M4 → §6.3, §7.4; M6 → §7.2;
    M8 → §8.1; m5 → §11; m8 → §7.2; m9 → §7.5; m15 → §13.
  - m-N1 → §1; m-N2 → §5.2 (per-branch batch with the watermark CAS);
    m-N3 → §4.4, §5.1; m-N4 → §4.1; m-N5 → §6.1; m-N6 → §4.2; m-N7 → §3.1;
    m-N8 → §5.4; m-N9 → §11; m-N10 → §5.4; m-N11 → §8.3; m-N12 → §7.2,
    §7.5; m-N13 → §9.1; m-N14 → §2.1, §8.8; m-N15 → §9.2; m-N16 → §12;
    m-N17 → §11.
- **r0.3 → r0.4** (closure check of r0.3: 2 blockers, 9 majors, 17 minors;
  mirrors were the right shape, but every protection they relied on is
  keyed on `RecordKind::CodeAnchor` in the code):
  - P1 → §5.2 mirrors created once, `MirrorAttach`; P2 → §5.1 merges
    exclude replica-origin state, §5.2 catch-up diffs against the replica
    projection, PROP19 invariant; P3 → §5.1 logical origin, A8, A5
    exemption, §8.3 `team:` scopes on the replica channel only; P4 → §5.1
    shred never refuses a mirror write; P5 → §8.3 scope from origin, §5.3
    certificates; P6 → §4.2 every deterministic error rejects; P7 → §4.2
    team support v1 frozen with `accept_version`; P8 → §5.1 what a mirror
    holds; P9 → §4.3, §8.6 checkpoint and digest cover what `accept` reads,
    `Compact` terminal, joiners; P10 → §8.1 by-OID fetch; P11 → §7.2
    bounded sync before the gate, O7.
  - m1 → §4.2, §7.2 sub-batches; m2 → §7.2 dry-run parsing, skip when
    unsure; m3 → §5.3; m4 → §5.1; m5 → §5.2, §2.3; m6 → §8.5 `left`;
    m7 → §5.2 one batch per entry; m8 → §11 content-global; m9 → §4.3;
    m10 → §4.3, §9.1; m11 → §12; m12 → O6, §5.3; m13 → §6.1; m14 → §7.1;
    m15 → §5.4; m16 → §13 T120, PROP19; m17 → §8.6, §2.3.
- **r0.4 → r0.5** (closure check of r0.4: no blockers, 5 majors, 14
  minors): Q1 → §5.1 `MirrorLag`, catch-up before merges; Q2 → §3.1 blob set
  defined by the chain; Q3 → §8.6 entries after `Compact` ignored, §8.3 live
  position on the newest generation; Q4 → §4.3 two team statuses, no
  "retracted"; Q5 → §5.2 one net batch per branch per import, split only for
  a→b→a, per-entry deltas in the replica, G5 join budget. Q6 → §11; Q7 →
  §5.2 `MirrorAttach{key, head, base}`, `main` first; Q8 → §12; Q9 → §5.1
  tags only; Q10 → §5.1; Q11 → §6.2, §7.2; Q12 → §8.5; Q13 → §8.1, §8.7;
  Q14 → §8.7; Q15 → §12; Q16 → §6.2; Q17 → §2.3; Q18 → §13 T121, G5, G6.
