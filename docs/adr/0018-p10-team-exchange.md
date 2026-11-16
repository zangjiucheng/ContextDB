# ADR 0018 — P10 team exchange (root / edge)

Status: **Accepted (r0.6), G0 passed 2026-11-16.** SPEC v1.10.0 incorporates §12; [P10 gate](../gates/P10.md) records the design acceptance. Implementation and G1–G8 remain pending. The owner's full
review of r0.5 found 1 blocker, 5 majors and 4 further points (R5-B1,
R5-M1–M5, R5-S1–S4), answered in r0.6 (§14); the owner's closure check
of r0.6 found 1 P1 and 3 P2, answered in place (§14). r0.1's review found 5
blockers, 10 majors and 15 minors; r0.2's closure check closed most of them
and found 3 blockers, 7 majors and 17 minors in r0.2's new join between the
two stores; r0.3 replaced that join with ordinary local records, and its
closure check found 2 blockers, 9 majors and 17 minors in how mirrors fit
the kernel (§14). r0.4 gave mirrors a replica origin the kernel enforces; its closure check
found no blockers, 5 majors and 14 minors, answered in r0.5.
Owner decisions of 2026-11-15 are in §0. O1–O7 in §0.1 are confirmed; the owner authorised acceptance and SPEC v1.10.0 on 2026-11-16 after the closure review.

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

### 0.1 Decisions (owner)

**Confirmed 2026-11-16:** O1–O7 take the recommended option. The owner
authorised the final clarification, SPEC v1.10.0 and G0 acceptance after
the closure review. The options below are kept for the record.

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

Verified on git 2.56 (2026-11-16): a `reference-transaction` hook that
exits non-zero, or is killed (signal 9), in the `prepared` phase of a fetch
aborts the update (`fatal: in 'prepared' phase, update aborted by the
reference-transaction hook`); the tracking ref keeps its old value (§8.3).

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
  it was given at join (out of band), and successors that an accepted
  `Compact` entry on a chain it follows commits to by tree and digest,
  verified against both (§8.6).
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
  CERTIFIED while a tracking ref of *T* holds a root that is not mirrored
  on that branch, provided the ref was moved by a git that ran the
  ContextDB hooks, or by `ctxd` itself; nor while import is stopped (§8).
  The ref cannot move before `ctxd` has durably learned the new root
  (§8.3, the announce fence), so this holds at every instant, not after a
  polling interval. Ref moves by clients that bypass the hooks are covered
  only within `lag_check` (§2.3).

### 2.3 Not guaranteed (documented)

- Freshness against the remote: an edge that has not fetched cannot know
  what it missed. `[team] warn_after` warns; it cannot fail closed offline.
- Fetches that bypass the hooks (`core.hooksPath` overridden, JGit,
  libgit2, IDE fetchers) move a tracking ref without the announce fence;
  `ctxd` sees the new root within one `lag_check` (default 2 s), and until
  then a read can be CERTIFIED on the older root (§8.3).
- The fence fails closed: if `ctxd` is running but cannot record an
  announce, or the announce file cannot be written, the hook aborts that
  fetch's ref transaction (the whole fetch, normal branches included) with
  a message; `ctx team hooks off` disables the component and the fence
  with it (§8.3).
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
- **Positions and sequence numbers (R5-S1).** A generation's entries are
  the commits on the first-parent path from its tip back to its first
  entry, which must be the trust anchor the edge holds for that generation
  (genesis, or the first entry a verified `Compact` named, §8.6); a path
  that does not reach it is a rewrite (§8.2). Every commit on the path is an
  entry with a **position**, and its sequence number is fixed by position
  alone: the first entry of generation 0 is 0, and each entry is its
  first parent's plus one; a generation's first entry is the `Compact`
  entry's plus one (N9). Nothing an entry declares changes the numbering
  of later entries.
- `accept` rejects, as a structural check, an entry whose `Ctx-Team-Seq` is
  absent or differs from its sequence number, an entry other than the
  generation's first with more than one parent, and a generation's first
  entry with any parent. A rejected entry keeps its position and number,
  and its first-parent child is decided next as usual. No entry is ever
  skipped: there is no "missing parent" case, since the path is defined by
  first parents and an absent object stalls (`ReplicaIncomplete`).
- **Genesis** (generation 0's first entry) holds the team document only. A
  later generation's first entry holds a **checkpoint** (§8.6), as its
  whole tree, and `Ctx-Compacts: <OID of the Compact entry>`.
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
  proposal in the entry; on a later generation's first entry,
  `Ctx-Compacts` instead of `Ctx-Batch-Hash` and `Ctx-Proposal`.
- Author and committer must be exactly `ContextDB <team@contextdb>` at time
  0 +0000, and `Ctx-Accept-Version` must equal the team document's
  `accept_version`; otherwise the entry is rejected (m-N7).
- **Signatures**: SSH `sshsig`, namespace `git`, ed25519 keys only
  (`ssh-ed25519`; security-key `sk-ssh-ed25519` keys are refused in P10),
  verified in process against the team document at the parent, with no
  time-dependent options. GPG is not supported. In SHA-256 repositories the
  `gpgsig-sha256` header is the one checked.
- **Limits of an ordinary entry**: `batch` ≤ 1 MiB and ≤ 1,000 ops, each
  blob ≤ 1 MiB, the entry ≤ 8 MiB in all, checked before decoding. They do
  not apply to a checkpoint, which has its own format and limits (§8.6).
- **Team state limit (R5-M5).** The team document's
  `[limits] state_bytes` (default 1 GiB, at most 4 GiB) bounds the size
  of a checkpoint of the replica's state, with room for the `Compact`
  that would produce it (§8.6). The check is **per proposal**, like every
  kernel check (§4.2, m1): after each proposal's sub-batch, that proposal
  alone is rejected (`TeamStateLimit`) if the limit would be exceeded;
  proposals of the same entry accepted before it stay accepted, and later
  ones are decided on the state without it (r0.6 closure P2). A
  `SetTeamAccess` that lowers the limit below what the current state needs
  is rejected the same way.

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
  (admins, §11), `Compact{generation, checkpoint_tree, digest}` (sole op,
  admins, §8.6).
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
  and `TeamStateLimit` included — rejects. Import **stops** (`ReplicaStalled`) only on I/O
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
`certified` if `T(head)` holds and `stale` otherwise. On the replica's
state, SPEC SD11's L1 restricted to Pin, Cur and Ref, with SD3's
non-empty premise condition written out (R5-S2):

```
cur(p)    = head(l(p)) = p
lit(j,i)  = T(p_i) ∧ (kind_i ≠ Cur ∨ cur(p_i))
                   ∧ (kind_i ≠ Pin ∨ cur(p_i) ∨ kind(l(p_i)) = Evidence)   -- Pin-strict
T(r)      = base(r) ∨ ∃ j: concl(j) = r ∧ enabled(j) ∧ P(j) ≠ ∅ ∧ ∀ i ∈ P(j): lit(j,i)
```

`T` is the least fixpoint; `P(j)` is the set of premise incidences, so a
reference-only rule (`Ref` only) is never active and supports nothing
(SD3), however its references stand. A `Pin` on a revision that is no
longer its record's head fails `lit` (Pin-strict), so it supports nothing
in team status, although SD3's `S` may still count it. With no Sem, no
validators and no Conflicts in the replica (§4.4), L1 = L2 here and
`T(head)` is exactly the replica's CERTIFIED summary of the head; SD21
states team support v1 in this form. There is
no separate "retracted" status: a root retraction is a `RetractBase` on the
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
- **one net batch per branch per import** (Q5), split so that no change to
  a mirror cancels out inside a batch (R5-M3): if a mirror's pair
  `(head content, base bit of the head)` returns within the import to a
  value it had earlier in it — head a→b→a, or base true→false→true on an
  unchanged head, or any mix — the batch is cut: walking the entries in
  order, a new batch starts before any entry that would bring a mirror back
  to a value it already had within the current batch (its value at the
  batch's start included). So no departure is cancelled by a return in the
  same batch, and each lands in the branch's change log (SPEC §6.4
  history). The
  replica logs a per-entry delta of the keyed state, so the projection at
  every entry the replica imported is available; catching up from any
  position in that log is split exactly, whatever the number of entries,
  including a long-offline edge and an edge that followed a `Compact`;
- **history gap**: when the branch's watermark position is not in the
  replica's per-entry log (a `ctx team rebuild` that replayed from a later
  checkpoint than the branch's position, a rejoin, a joiner whose mirrors
  survived an earlier leave), the net batch cannot prove what happened in
  between. Its watermark op then carries `gap: true`, which strict
  validation treats as a change to every mirror of the team (§5.3);
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
for any record, and lag is decided at read time (P5). For `strict_no_aba`
(SPEC §6.4 step 1) the proof of "nothing happened" for a mirror comes from
the branch's own change log, not from the team watermark (R5-M3):

- the head-moved and base-flipped clauses apply to mirrors as to any
  record; the split rule of §5.2 guarantees that every head move and base
  flip of a mirror at the root since the last import is a separate local
  change, so an ABA at the root, within one import or across several, is
  seen;
- the clause "a scope's watermark advanced" applies to code observation
  scopes (SD14b) only. A `team:` scope's watermark advancing does **not**
  invalidate a strict certificate, so a team entry about other records
  leaves it valid; a `team:` watermark op with `gap: true` since the
  entry's `since_epoch` is `Invalid{StrictChange}` for every vector item
  that is a mirror of that team, since the history cannot prove the
  mirror unchanged;
- a non-strict certificate is unaffected by either; `gap` changes nothing
  in L0/L1/L2.

So a strict certificate over a mirror is Valid only when the branch's log
proves, entry by entry since `since_epoch`, that the mirror's head and base
never changed; where that proof is missing it is not Valid.

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
- **Two layers of authorisation (R5-M2).** A proposal is checked twice,
  and both must pass; neither stands in for the other:
  1. **Local layer**, as the requesting connection's local principal *P*
     under the current local access document, with P9's own checks
     (SPEC SD11 authorisation, `txn.rs` `Auth`), as if the proposal were a
     local batch on the mirrors: `Read` on every record and revision it
     names — a superseded head, a premise, a `Ref`, a rule's conclusion,
     every `(key, content seen)` of its read set and every earlier queued
     proposal it depends on (below) — then `Write` on the lane of every
     record it creates or changes, and `Curate` where P9 requires it
     (`adopt_as_base`, `AssertBase`, `RetractBase`). A name *P* cannot
     read is refused `Unauthorized(Read)` exactly as P9 refuses it for a
     local batch, before anything about the named record's existence or
     state is used (`logical_first`). Granting an ordinary local writer the
     curator role on a team lane is how the store owner delegates base
     promotion through the signer; it is never implied.
  2. **Team layer**, the scratch `accept` of §6.2 as the member bound to
     the signing key, under the team document's roles (§9.2). In
     multi-user mode the actual signer is the pusher; if that member lacks
     a role, the proposal becomes `conflicted` at publish (m-N5).
- **Egress (R5-M1).** A proposal into a lane the local document marks
  egress-off is refused (`EgressDenied`). Egress, and the whole local
  layer, are checked again at publish against the policy then in force
  (§7.2); passing at enqueue is not a licence to leave the machine later.

### 6.2 Checking and storing

The proposal is translated to portable ops (§4.1); one larger than an
entry's limits (§3.1) is refused (`ProposalTooLarge`, Q11). It is checked
by a scratch `accept` on a clone of the replica with every earlier
`queued` proposal applied — its `Cur(content seen)` is the content in that
overlay, so a record an earlier queued proposal created or changed is
seen as that proposal left it, and the dependency is recorded (Q16) — and appended to `<data>/teams/<team-id>/proposals`, an append-only
queue with checksummed frames. Each proposal records the earlier proposals
it depends on (any it reads or names) and its local author *P*. The reply
is `proposed: <id>, publishes on the next git push`.

**Queue visibility (R5-M2).** A queued proposal is local data of its
author, in the lanes it writes. *P* may read a queued record (and depend
on it) only if *P* may read its lane under the local document, as for a
local record; reading it requires the local layer of §6.1 over the
proposal that created or changed it. The local layer resolves names
against mirrors and *P*-readable queued records only:

- naming a record that exists only in a queued proposal *P* cannot read
  is refused `Unauthorized(Read)`, the same reply as for a hidden mirror,
  so a reply never shows whether a hidden proposal exists;
- creating a key that a hidden queued proposal also creates follows
  SPEC's one existing exception, the global key namespace (SD19, T75):
  `LogicalExists`, which says the key is taken and nothing else;
- the scratch `accept` then runs on the full overlay. A rejection there
  caused by a hidden proposal is reported to *P* as `conflicted{queue}`
  with no key, reason or proposal named. This reveals that some queued
  proposal conflicts with *P*'s, as P9's global key namespace already
  reveals key use; declared in SD21;
- `ctx team status`, `ctx_read`'s `proposed` marker and WHAT_IF replies to
  *P* list only proposals *P* may read; counts of the others are not
  shown.

### 6.3 States

`queued` → `published{commit}` → `accepted` | `rejected{reason}`;
`queued` → `conflicted{key, reason}` (§7.3) → `queued` (re-proposed) |
`dropped`; `published` → `queued` when the push was refused, or when the
outcome was unknown and a fetch shows the chain does not hold it (N7).
`published` is recorded at the publish linearization point (§7.2 step 7),
just before the push, and means "may be on the remote". A proposal that
fails the local layer at publish (§6.1: egress now off, its author no
longer allowed to read, write or curate what it needs) becomes
`conflicted{policy: <need>}` (R5-M1); it is never published later by
itself, even if the policy is restored: its author re-proposes or drops
it. A proposal whose dependency becomes `conflicted`, `rejected` or
`dropped` becomes `conflicted{dependency}` itself (N7). Every transition
is a queue frame.

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
5. **Fix the publish set.** `ctxd` takes every `queued` proposal, in queue
   order, and in one read of its state:
   - **local layer (R5-M1, R5-M2)**: re-checks each against the local
     access document now in force, at its epoch *E*, as its recorded
     author (§6.1 layer 1, egress included). Failures become
     `conflicted{policy}`, and their dependents follow (§6.3), before
     anything else is done;
   - **policy (R5-S4)**: if `publish = "confirm"`, or the remaining set
     holds proposals of more than one local author (O5, m-N15), the hook
     lists them with their authors and a **set token** (BLAKE3 of the
     sorted proposal ids, stored with the ids as a `listed` queue frame)
     and publishes nothing; the code push goes on.
     `ctx team publish --yes <token>` (store owner) later publishes from
     exactly that set: proposals queued after the listing are not in it,
     and a proposal of the set that fails this step drops out with its
     dependents. The published set is always a subset of the confirmed
     one, never a superset;
   - **team layer**: rebuilds the remaining ones on the replica tip
     (§7.3) and runs the scratch `accept` on them in queue order, each as
     its own sub-batch, exactly as import will (§4.2). `Cur(content seen)`
     is compared with the state after the earlier sub-batches of the same
     entry (m1). Those that pass go into **one entry**, split into several
     consecutive entries when the limits of §3.1 would be exceeded.
     Because each entry's parent is the tip just imported and `accept` is
     deterministic, the local outcome is every edge's outcome.
   Proposals queued after this step wait for the next publish.
6. The hook signs the entry locally. Nothing has left the machine: the
   commit and its objects are unreferenced, so no other push sends them.
7. **Linearization point (R5-M1).** The hook asks `ctxd` to begin the
   publish of that commit. In one write, `ctxd` checks that the local
   access document is still at epoch *E* and that no proposal of the entry
   changed state, and records them `published{commit}`; if either check
   fails it refuses, and the hook goes back to 5 under the new policy.
   A `SetAccess` committed before this write therefore stops the entry; one
   committed after it applies from the next publish, and the entry in
   flight, checked under the policy that was in force at the write, may
   still go out. SD21 states this order.
8. The hook pushes `<commit>:refs/contextdb/team/<team-id>/g<n>` with
   `core.hooksPath=/dev/null`.
9. On a non-fast-forward refusal, `ctxd` returns the entry's proposals to
   `queued`, and the hook goes back to 1's fetch and import, then 5 (whose
   local layer runs again), at most 3 times in all.
10. Print each proposal's outcome. The code push proceeds whatever happened,
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
| Before step 7 | unreferenced commit object; proposals `queued` | ignored; next publish rebuilds and re-checks the local layer |
| Between step 7 and the push | `published`, not on the remote | next fetch: absent → `queued`, then the next publish re-checks the local layer |
| Push outcome unknown | `published`, maybe on the remote | next fetch: found → outcome from the replica; absent → `queued` |
| Confirm listing shown, not confirmed | proposals `queued`; a `listed` frame | `--yes <token>` publishes from that frame's ids; an unknown token is refused |
| Pushed, not imported | `published` | next import marks it accepted or rejected |
| Lock holder killed | stale lock | broken by pid check |

### 7.5 Hooks

The P5a installer becomes a **dispatcher**: one ContextDB script per hook
name, running registered components in a fixed order, chaining the user's
original hook with a backup. Components:

- P5a: `post-commit`, `post-checkout`, `post-merge`, `pre-push` (gate);
- P10: `reference-transaction` (announce in `prepared`, withdraw in
  `aborted`, notify in `committed`; §8.1, §8.3), `post-merge` (notify),
  `pre-push` (publish, §7.2).

The fetch refspec is added with `git config --add` and removed on leave.
`ctx mirror status` lists only `refs/contextdb/b*` refs; team refs appear
in `ctx team status` (m-N12).

## 8. Import and lag

### 8.1 Import steps

1. `git fetch` (any fetch or pull) updates the tracking refs.
2. The `reference-transaction` hook acts only on updates of
   `refs/contextdb/remotes/*/team/*` for a team this store has joined. In
   `prepared` it **announces** each new OID before the ref can move
   (§8.3); in `aborted` it withdraws the announce; in `committed` it
   notifies `ctxd` to import. In a partial clone, a
   plain fetch brings the team commits but not their blobs, and a later
   `--no-filter` fetch of the same refs brings nothing (P10, verified on
   git 2.56). So the hook starts, detached in the user's environment,
   `git rev-list --objects --missing=print` over the new team commits and
   fetches the missing objects by OID through the promisor remote
   (`git fetch <remote> <oid>…`), then notifies `ctxd` again (Q13). The
   hook vetoes only when it cannot make an announce durable (§8.3, §2.3);
   otherwise it returns the chained user hook's status in `preparing` and
   `prepared` (M8, m-N2 of r0.2).
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
- the live position is taken on the **newest generation this edge
  follows**; older generations' refs are never watched, so entries
  appended to them cannot cause lag (Q3). `ctxd` keeps it in memory, so a
  read never spawns git (m-N11);
- **the announce fence (R5-M4).** The fetched tip a read must not run
  ahead of is the one in the tracking ref, which a hook-running git moves
  only after the `prepared` phase. There the hook, for each new team OID:
  1. appends `(ref, OID, pid of the git process)` to
     `<data>/teams/<team-id>/announce` and fsyncs it;
  2. if `ctxd`'s socket accepts a connection, sends the announce and waits
     for its acknowledgement (≤ 500 ms); `ctxd` records it in memory under
     the lock reads take, then acknowledges;
  3. if 1 fails, or `ctxd` accepted the connection but did not
     acknowledge in time, exits non-zero, so git aborts the ref update
     (verified, §1); if no `ctxd` is listening, the file alone is the
     fence.

  `ctxd` binds its socket before it reads the announce file at start, and
  serves no read before that; so an announce written after the read finds
  the socket and is acknowledged. A crash anywhere in the hook before
  step 3 returns leaves either no ref change (git aborts on the hook's
  death) or a durable announce;
- an announce is **pending** until `ctxd` sees the tracking ref contain
  its OID (then it is the fetched tip), or the hook's `aborted` call
  withdraws it, or its git process is gone and the ref does not contain
  the OID (the transaction ended without it). `ctxd`'s own team fetches
  (start, `ctx team sync`, the bounded sync of §7.2) mark the team
  **fetch in flight** until the ref is read back;
- **lag**: a read whose canonical L2-witness closure contains a mirror of
  *T* on a branch is `PENDING(ReplicationLag)`, with certificate
  `Unknown{lag}`, if the branch's watermark differs from the fetched tip
  `ctxd` holds, or an announce of *T* is pending, or a fetch of *T* is in
  flight, or import is stopped (`ReplicaStalled`, `ReplicaDiverged`,
  `ReplicaIncomplete`); direct reads of a mirror are included (N5);
- `lag_check` (default 2 s) re-reads the refs, catching ref moves that
  bypassed the hooks (§2.3) and pending announces whose git process died;
- never stored in L0/L1/L2. The oracle checks it at read time only, and its
  input model has two positions per team: the **actual** tracking-ref tip
  and the **daemon-observed** tip with the pending set. G-LAG is the
  property that, for hook-running fetches, whenever the actual tip is not
  mirrored on a branch, the observed state already shows lag.

`ReplicationLag` joins SD13's PENDING reasons right after `ObservationLag`.

### 8.4 Crash and intermediate states

| Point | State left | Recovery |
|---|---|---|
| Replica entry not committed | replica at the previous entry | next import redoes it |
| Replica ahead of a branch's mirrors | that branch's watermark behind; reads lag | next import or start writes the branch's batch (CAS on its watermark) |
| Some branches written, others not | each branch consistent with its own watermark | the rest are written next |
| Hook killed in `prepared` before the announce is durable | git aborts the update; ref unchanged | nothing to recover |
| Hook killed in `prepared` after the announce | durable announce; git aborts the update | announce pending until its git process is gone, then dropped (§8.3); reads lag meanwhile |
| Ref moved, `committed` notify lost (hook killed, `ctxd` restarting) | announce pending, or recorded by `ctxd` at start; ref ahead of the replica | reads lag at once (R5-M4); next `lag_check`, start or `ctx team sync` imports |
| Ref moved by a client without hooks | no announce | `lag_check` sees it within its interval (§2.3) |

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

**Commitments run one way (R5-B1).** r0.5 had `Compact` name the first
commit of generation *n+1* and that commit name `Compact`: since a git OID
is a hash of the object's content, neither could be built first. Now
`Compact` commits only to things that exist before it — a git **tree** and
a digest — and only the new generation's first commit names `Compact`.

- **The checkpoint** holds everything `accept` reads (P9): the keyed team
  state of §4.3 — every record with all its revisions and their bytes
  (except shredded ones), per-revision base, metadata, every rule with its
  `enabled` flag — plus the team document, the shred set, the accepted
  `(member, proposal id)` pairs and the sequence number. The checkpoint of
  a state Σ for generation *g*, `ckpt(Σ, g)`, is a git tree (in the
  repository's object format) and a deterministic function of Σ and *g*
  alone, so anyone computing it from the same inputs gets the same tree
  OID:
  - `checkpoint/index` (`CXTK` format): `accept_version`, team id,
    *g*, Σ's sequence number, `digest(Σ)`, the total size, and for
    each chunk, in number order, its BLAKE3, byte length and item count
    (44 bytes per chunk);
  - `checkpoint/state/<six digits>`, numbered from `000000`: the canonical
    state as a sequence of items (a record header, a revision, a rule, the
    team document, a shred entry, an accepted proposal id) in digest
    order, packed greedily into chunks of ≤ 1 MiB; an item is never split;
  - `blobs/<hex>`: each content a non-shredded revision holds, once,
    ≤ 1 MiB as for ordinary entries.

  **Limits (R5-M5).** The ordinary entry limits of §3.1 do not apply. The
  format's bounds hold by construction for every state `accept` can
  reach (r0.6 closure P2):
  - **items**: every item is ≤ 1 MiB because the team ops bound their
    parts: a key or lane ≤ 256 bytes, a record's tags ≤ 64 KiB, a team
    rule ≤ 1,024 incidences of any kind (≤ 320 KiB encoded), the team
    document ≤ 512 KiB, a member name ≤ 64 bytes; an op that would exceed
    one is rejected (`InvalidMetadata`, `InvalidRule(TooManyPremises)`,
    `TeamDocTooLarge`) whatever the entry's other limits;
  - **size**: `ckpt_size(Σ)` (independent of *g*) is the exact byte total of the index, the
    chunks and the blobs, computed by the encoder without building the
    tree. `state_bytes` ≤ 4 GiB (a team document above it is rejected);
  - **chunks and index**: greedy packing leaves any two consecutive chunks
    over 1 MiB together, so a checkpoint within 4 GiB has ≤ 8,193 chunks
    (six-digit names suffice) and an index ≤ 512 KiB, both checked before
    decoding with the total against `state_bytes`;
  - **headroom for `Compact`**: an ordinary proposal reserves space for
    one subsequent `Compact`. It is rejected (`TeamStateLimit`) if
    `ckpt_size` of the state after it, with one more accepted proposal id
    of a 64-byte member name added (the future `Compact` entry's own),
    exceeds `state_bytes`. `Compact` consumes that reserved space: its
    capacity check is the actual `ckpt_size(Σc) ≤ state_bytes` and the
    format bounds above; it does **not** reserve a second Compact id.
    The reserve guarantee applies to states after ordinary proposals;
    repeated compactions are still subject to their actual encoded size.
  **Verification is streaming**: the index first, then each chunk against
  its BLAKE3 and length as it is read, each blob's name against its bytes,
  every content an item names present unless shredded, and at the end the
  digest of the rebuilt state against the index's digest. A missing
  object stalls (`ReplicaIncomplete`) and is fetched by OID (§8.1); any
  other failure makes the commit an invalid successor (below). The format
  is part of `accept_version`.
- **One state, Σc (r0.6 closure P1).** Everything below is computed from
  the single **post-`Compact` state** Σc: the replica state at the tip *t*
  that `Compact` extends, with exactly two changes — the sequence number
  becomes the `Compact` entry's (*t*'s plus one), and the `Compact` entry's
  `(member, proposal id)` joins the accepted set, as any accepted
  proposal's does (so publish idempotency of §7.4 covers it). `Compact`
  records nothing else in the state: not *T*, *D*, the generation or any
  OID, so Σc is known before the entry is built. Then *T* =
  `ckpt(Σc, n+1)` and *D* = `digest(Σc)`; *n+1* is the generation the
  entry extends plus one, known to every edge.
- **Step 1, `Compact`.** An admin's `ctxd` picks the proposal id, derives
  Σc from its replica at *t*, computes *T* and *D*, and the admin appends
  `Compact{generation: n+1, checkpoint_tree: T, digest: D}` to generation
  *n* (sole op, one `Ctx-Proposal`, signed, pushed by CAS as any entry).
  `accept` derives the same Σc from its own replica and the entry, and
  accepts only if the author is an admin, the generation field is *n+1*,
  *D* = `digest(Σc)` and *T* = `ckpt(Σc, n+1)`'s tree OID: deterministic, from bytes every edge holds (the
  blob set), needing neither the next generation nor any fetch. Checking
  *T* as well as *D* means an accepted `Compact` always has a valid
  successor. A rejected `Compact` is an ordinary rejected entry: it keeps
  its position and sequence number, its proposal id is not accepted, and
  generation *n* goes on.
- **Step 2, the first entry of *n+1*.** A root commit *F* with tree *T*,
  `Ctx-Team-Seq` = the `Compact` entry's plus one,
  `Ctx-Compacts: <OID of the Compact entry>`, the usual author and
  `Ctx-Accept-Version`, no `Ctx-Proposal`, signed by any admin of the team
  document in Σc, pushed to the new ref
  `refs/contextdb/team/<team-id>/g<n+1>`. *F* takes the next position and
  changes nothing but the sequence number: the state after *F* is Σc with
  the sequence number one higher, on every edge. The old ref is not
  rewritten, so an edge that was offline sees `Compact` when it next
  fetches and follows (N9).
- **Following.** After an accepted `Compact`, generation *n* is
  **terminal**: its ref is no longer watched, and entries appended to it
  after `Compact` are **ignored** (R5-S3) — not decided, given no sequence
  number, never in the decision log or any digest, and no cause of lag
  (Q3); `ctx team status` counts them as a diagnostic only. The edge then
  takes generation *n+1*'s first entry as the root commit of the
  first-parent path of `g<n+1>` and accepts it as successor only if its
  tree is *T*, its `Ctx-Compacts` and sequence number are as above, and
  its signature verifies. Its replica already is Σc, whose checkpoint is
  *T* by the check at `Compact`, so it need not read the checkpoint. It
  applies *F* (sequence number plus one), records *F* as its trust anchor
  and continues with *F*'s first-parent children.
- **Between the steps.** With `Compact` accepted and no valid successor
  yet, the team is `CompactPending`: reads do not lag (the edge has
  mirrored everything up to `Compact`), publish waits, and
  `ctx team status` says so. `ctx team compact --resume` (any admin)
  takes Σc — the replica after the accepted `Compact`, which is Σc by
  definition — rebuilds `ckpt(Σc, n+1)` (equal to the committed *T*, which
  `accept` checked; a mismatch is a bug and is refused), signs a new *F*
  and pushes it. Two admins resuming
  at once: the second push is refused as non-fast-forward and the first
  successor stands; both were valid. A `g<n+1>` whose root does not
  verify (a squatted ref) is ignored with a warning, and an admin replaces
  it with `ctx team compact --resume --force-ref`.
- A **joiner** that starts at a later generation is given that
  generation's first-entry OID out of band (§8.7), as genesis is. It
  verifies *F*'s signature against the team document in the checkpoint,
  rebuilds Σc from the checkpoint with the streaming checks above (the
  index's sequence number is the `Compact` entry's, and its digest
  `digest(Σc)`), then applies *F* exactly as a follower does. Follower and
  joiner thus hold the same state and digest from *F* on.
- The team id and `Ctx-Team-Seq` continue, so mirror keys and local
  dependents are unaffected. `ctx team rebuild` replays from the earliest
  generation still on the remote; if that is later than a branch's
  watermark position, the branch's next catch-up has a history gap
  (§5.2).
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
(m-N15). Which local principals may propose, and what they may name, is
set by the local P9 document alone (§6.1 layer 1): its writers and curators
for the lane, and its read grants for everything the proposal names. The
bound member's team roles (layer 2) only decide whether the root accepts
the entry; that the member can read every team lane in the replica gives
no local principal any read right (R5-M2).

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

## 12. SPEC changes (landed in v1.10.0 at G0)

- §5 phase plan: P10 = team exchange; P11 = the old P10; P12 = the recursion
  half of the old P11 (research). Text tied to the old numbers is reworded.
- §1.4: "co-located, with team lanes replicated through git".
- **SD21, team knowledge** (SD20 is taken): the chain, positional
  sequence numbers, the replica, `accept` and its versions, team support
  v1 in the fixpoint form of §4.3, the team digest, the team state limit,
  compaction with one-way commitments and the checkpoint format,
  replica-origin logicals and their catch-up (split rule, history gap),
  `ReplicationLag` with the announce fence, the two authorisation layers
  of proposals, queue visibility and what a `conflicted{queue}` reply
  reveals, and the publish linearization point.
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
- §6.4 step 1: for mirrors, strict uses the branch's head and base
  history (complete by §5.2's split rule); a `team:` watermark advance
  does not count, a `team:` watermark op with `gap: true` does (§5.3).
- §3.5 / §6 / §7 / §8.3: op_log format 6 (`Channel::Replica`, origin,
  `MirrorAttach`, the `team:` scope and the `gap` flag for
  `ObserverWatermark`), protocol 8
  (team requests, proposal replies, `[team_lanes]`), errors
  `ReservedNamespace`, `ReplicaReadOnly`, `MixedTeamBatch`, `Unpublished`,
  `TeamDepOutside`, `ReplicaStalled`, `ReplicaDiverged`,
  `ReplicaIncomplete`, `NotTeamMember`, `EgressDenied`,
  `TeamStateLimit`, `TeamDocTooLarge`, `CompactPending`; queue states `conflicted{policy}`
  and `conflicted{queue}`.
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
  the limits. Policy at publish (R5-M1): enqueue, turn egress off, push —
  the remote ref and the remote's object set gain nothing — checked on
  the bare remote with `git cat-file -e` for the entry's commit, tree and
  every blob OID the entry would have held (computed locally), and by
  comparing the complete object enumeration
  `git cat-file --batch-all-objects --batch-check` before and after,
  which, unlike `rev-list --objects --all`, includes unreachable objects
  (r0.6 closure P2) — the proposal is `conflicted{policy}` and its dependents
  `conflicted{dependency}`; the same after a non-fast-forward retry; a
  `SetAccess` committed between step 5 and step 7 stops the entry, one
  after step 7 does not. Confirm (R5-S4): under `confirm`, and under
  `auto` with two local authors, a plain push lists and sends nothing;
  `--yes <token>` sends exactly the listed set, not a proposal queued
  after the listing, and a listed proposal that fails step 5 drops out
  with its dependents.
- **T112** one corpus chain per `TxError` and per structural check: each is
  a rejection on every edge; an unknown `accept_version`, a missing object
  and an injected I/O error stall. Every rejection chain continues with a
  valid entry after the rejected one (R5-S1): a wrong, absent or
  duplicated `Ctx-Team-Seq`, a merge commit (two parents) mid-chain, a
  parent on a generation's first entry; the next entry is numbered by
  position and accepted. Team status (R5-S2): a reference-only rule never
  supports its conclusion; a `Pin` to a revision no longer its record's
  head supports nothing; a `Pin` to an Evidence revision does.
- **T113** lag: fetched not caught up, stalled, diverged, incomplete,
  archived branch, `left`; direct reads included; per-branch watermarks;
  mirror scope from origin, not lane. The fence (R5-M4), each read issued
  immediately, with `lag_check` disabled so it cannot help: the ref
  updated and the `committed` notify suppressed → lag; the hook killed in
  `prepared` before and after the announce → ref unchanged, and lag only
  while the announce is pending; `ctxd` down during the fetch, then
  started → lag before its first read; a `ctxd` that does not acknowledge
  → the fetch fails; a fetch with hooks disabled → CERTIFIED until
  `lag_check` runs (declared). The oracle compares actual and observed
  tips.
- **T114** crash at every row of §7.4 and §8.4, and between the two
  compaction steps of §8.6; each read after a crash issued at once, not
  after `lag_check`.
- **T115** precise invalidation on every open branch: `Pin` dependents
  STALE and `Cur` dependents unsupported on a root supersede; all
  dependents unsupported on a root retraction or loss of certification;
  recovery when restored; leave.
- **T116** certificates over mirrors: a→b→a across two imports and within
  one import is caught by `strict_no_aba`; a catch-up that changes nothing
  a certificate read leaves it valid; the pre-push gate under lag (O7) and
  after the bounded sync. Strict history (R5-M3): team entries about other
  records leave a strict certificate Valid; a root `RetractBase` then
  `AssertBase` on an unchanged head, in one import and across two, is
  `Invalid{StrictChange}`; head ABA mixed with a base flip; an edge
  offline for many entries that catches up in one import; after
  following a `Compact`; after a `ctx team rebuild` from a later
  checkpoint (history gap) → `Invalid{StrictChange}`, never Valid.
- **T117** P9: `[team_lanes]` and its absent-section hash; non-owner
  redaction of mirrors; local shred of a mirror's bytes, then a root entry
  reusing them (no stall); `TeamShred` with bytes a local record holds.
  The local layer (R5-M2), by a non-owner *P* who can write lane A but not
  read lane B: a proposal in A naming a mirror of B as premise, as `Ref`,
  in its read set, or as the head it supersedes → `Unauthorized(Read)`,
  byte-identical to the reply for a key that does not exist; the same for
  a record that exists only in another principal's queued proposal in B;
  `adopt_as_base` without the local curator role → refused; `ctx team
  status` and WHAT_IF for *P* show neither the hidden proposal nor its
  count; a collision with a hidden proposal at publish is
  `conflicted{queue}` with nothing named. Egress off at enqueue →
  `EgressDenied`.
- **T118** compaction, with real signed objects and real OIDs throughout,
  no placeholders (R5-B1): an admin creates two generations, pushes them to
  a bare remote, and an online edge, an edge offline across the
  compaction and a checkpoint joiner all follow, with equal team digests,
  including after a `SetHead` back to old content. Entries appended to
  the compacted generation after `Compact` are ignored (R5-S3): no
  sequence number, no decision-log record, no digest change, no lag. A
  `Compact` with a wrong digest or by a non-admin is rejected and the
  generation continues. A crash between `Compact` and the successor push
  → `CompactPending`, then `--resume` by another admin; two resuming
  admins; a squatted `g<n+1>` ignored, then replaced with `--force-ref`.
  The follower's and the joiner's state and digest are equal after *F*;
  the `Compact`'s proposal id is in the accepted set on both, and *F*
  consumes one sequence number; a `Compact` whose *T* or *D* does not
  match Σc is rejected; `--resume` rebuilds the committed *T* (r0.6
  closure P1). A legal state larger than 8 MiB (R5-M5) is compacted and
  joined; a state at exactly the limit (with the `Compact` headroom) still
  compacts by consuming the reserve, without reserving a second Compact id; a proposal that would pass the limit is rejected while the
  others of its entry stand; a
  checkpoint chunk with a wrong BLAKE3 or length, a missing blob and an
  index over `state_bytes` make an invalid successor; a missing object
  stalls until fetched; an entry that would exceed `state_bytes` is
  `TeamStateLimit`. Prune, and an edge offline past it.
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
  invalid entries, invalid ones followed by valid ones; reference-only
  rules and `Pin`s to old revisions among the rules) applied to edges with
  random local histories: the same accepted proposals and team digest on
  every edge, equal to an independent oracle that implements
  `accept_version 1` and team support v1 over the oracle's input model
  (in `contextdb-oracle`, not the kernel); 10,000 seeds.
- **PROP18**: publish then import equals applying the proposals to the
  replica directly whenever the scratch `accept` passes.
- **PROP19** (the mirror invariant): after every import, catch-up, fork,
  merge, crash recovery and rebuild in random histories, every open branch
  whose watermark is *s* has mirror state equal to the replica projection
  at *s*; and (R5-M3) whenever a team record's `(head, base)` changed at
  some entry between two of a branch's catch-up positions, the branch's
  change log records a head move or base flip of its mirror between those
  catch-ups, unless a `gap` watermark covers them; 10,000 seeds.
- **Golden corpus**: `tests/team-chains/` outcomes per `accept_version`,
  run by every release.
- **G4**: fuzz `CXTB`, the checkpoint index and chunks (`CXTK`), the
  trailer parser, `sshsig` verification and the team document; kill points around import and publish; a remote that
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
  - the `reference-transaction` hook adds ≤ 20 ms to `git commit`, and
    its announce (fsync and acknowledgement, §8.3) ≤ 50 ms to a fetch that
    moves a team ref;
  - compacting and joining a team at 100 MiB of state, time and peak
    memory reported (streaming verification keeps memory independent of
    the checkpoint size);
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
- **r0.5 → r0.6** (the owner's full review of r0.5, 2026-11-16: 1 blocker,
  5 majors, 4 further points; recommended keeping the root/edge and git
  transport architecture):
  - R5-B1 (`Compact` and the next generation's first commit named each
    other's OIDs, which cannot be built) → §8.6: `Compact` commits to the
    checkpoint tree and digest, which exist first; only the successor
    names `Compact`; push order, `CompactPending`, `--resume`, squatted
    refs; §4.1; T118 with real objects.
  - R5-M1 (egress checked only at enqueue) → §6.1, §6.3
    `conflicted{policy}`, §7.2 steps 5 and 7: the local layer re-checked
    at publish, a linearization point that fixes which `SetAccess` wins;
    §7.4; T111.
  - R5-M2 (no local Read check on what a proposal names; the signer's
    team rights stood in for the caller's) → §6.1 two layers, §6.2 queue
    visibility and `conflicted{queue}`, §9.2; T117.
  - R5-M3 (strict certificates against team watermarks and net batches)
    → §5.2 the split rule over `(head, base)` and the `gap` flag, §5.3
    strict from the branch's own change log; §12 §6.4; T116, PROP19.
  - R5-M4 (G-LAG had a window between a ref update and the notify) →
    §8.3 the announce fence in `prepared` (git's abort verified, §1),
    pending announces, fetch in flight, the oracle's two tips; G-LAG and
    §2.3 narrowed only for hook-less fetches; §8.4; T113, T114.
  - R5-M5 (no size rule for checkpoints) → §8.6 checkpoint format,
    limits and streaming verification, §3.1 `state_bytes` and
    `TeamStateLimit` so every accepted state can be compacted; T118, G4,
    G5.
  - R5-S1 → §3.1 positional sequence numbers, T112; R5-S2 → §4.3 team
    status in fixpoint form with `P(j) ≠ ∅` and Pin-strict, T112, PROP17;
    R5-S3 → §8.6 ignored entries, T118; R5-S4 → §7.2 step 5 confirm and
    the set token, §7.4, T111.
- **r0.6 closure** (owner, 2026-11-16: 1 P1, 3 P2): P1 (*T* computed
  before `Compact`, *D* after it, `--resume` after it) → §8.6 one
  post-`Compact` state Σc for *T*, *D*, `accept`, `--resume` and joiners;
  `Compact`'s proposal id accepted; *F* consumes one sequence number;
  `accept` checks *T* as well as *D*. P2 → §3.1 `TeamStateLimit` per
  proposal; §8.6 item, index, chunk-number bounds and the `Compact`
  headroom, `TeamDocTooLarge`; T111 checks objects with `cat-file`.

- **Final closure and G0 (2026-11-16).** The r0.6 closure's P1 and three
  P2 points are closed. The final review found no remaining blocker or
  major; its non-blocking headroom wording is resolved above: ordinary
  proposals reserve one Compact id, Compact consumes that reserve and
  checks its actual Σc without reserving another. The owner authorised
  SPEC v1.10.0, accepted status and G0; G1–G8 are not implementation
  results of this design review.
