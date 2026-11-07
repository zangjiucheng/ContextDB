# ADR 0017 — P9 principals, permissions and security

Status: **Accepted (r0.5), G0 passed 2026-11-06.** The owner confirmed
F1–F13 as recommended (a) and F14 in the narrowed form of §0.1. r0.1's
independent review found 8 blockers, 9 majors and 9 minors; r0.2's
closure check found them closed or partly closed, with 7 new majors and 12
minors (N1–N19), answered in r0.3; two focused checks and three owner
reviews followed (§14). The SPEC amendments of §12 landed as SPEC v1.9.0.

## 0. Scope and decisions to take

P9 implements SPEC §5 P9. Its goal is that several principals can share
one ContextDB without any of them reading what it may not read, or writing
as somebody else.

- principals bound to OS users through the transport, with the peer uid as
  the only identity source (§3);
- an access document committed through the op_log: principals, groups,
  lanes, grants (§4);
- `Authorized(principal, logical)` computed for real, in the order owner >
  membership > visibility > role > ACL, with UNAVAILABLE first in SD13
  (§4.4);
- every response filtered or redacted per principal, including diffs,
  commit results, errors and certificates (§5);
- write authorisation inside the kernel transaction, with dry-run parity,
  proof of possession for blobs, and merge rules (§6);
- egress control for validators, the embedder and the git mirror (§7);
- crypto-shred: per-content keys in an append-only key log, a
  `ShredContent` op, and purges of every derived copy (§8);
- the documented limits: same-uid processes, side channels, the plaintext
  `content_hash`, denial of service (§2.3).

Gate items: T17, T17b, T58, T59, T60, S13-rows-e2e (UNAVAILABLE rows), and
the new T98–T108, with PROP15 and PROP16 (§11).

**Hard isolation holds between OS users, and only there.** A process
running as the daemon's OS user can read every file in the data
directory, and can read any key the daemon can read. No check inside the
daemon stops it. Such a process is therefore the **store owner**, as in
P3–P8. The store owner reads everything; there is no privacy from it
(F12). Personal lanes are private from the *other* principals.

**Single-user stores do not change.** Without an access document every
store behaves as in P8: one principal, every lane readable and writable.
The P8 test suite must pass with the same results (T106). A team opts in
by committing an access document.

### 0.1 Open points (owner)

The recommended option, which this draft takes, is listed first.

| # | Question | Recommended (a) | Alternative |
|---|---|---|---|
| F1 | Identity source | One principal per OS user, mapped from the peer uid by the access document. | (b) Token files presented on connect. Every same-uid process can read them, so they are no stronger than (a). |
| F2 | Default lane for new records | Unchanged: MCP defaults to `"repo"`. Each lane says who may assert bases (`curators`, which may be `*writers`), so a team decides whether agents' `adopt_as_base` records need review. | (b) A default lane per principal, set in the access document. |
| F3 | How shred destroys a payload | One key per content hash, kept in an append-only key log. Shred rewrites the key log without the key. Existing packs are re-encrypted by `ctx migrate`. Cost decides: one key-log rewrite against a pack rewrite per shred. | (b) Rewrite the pack segments without the blob. |
| F4 | Signed log head (SPEC: optional) | Deferred to its own ADR. Against another OS user the files are unreadable already; against a same-uid process a local key signs nothing it cannot forge. | (b) Ed25519 log checkpoints in P9. |
| F5 | G6 environment | Two OS users on **macOS** (production platform: `getpeereid`, `F_FULLFSYNC`), with a second local user created by the owner once with `sudo`; plus Linux if the remote host allows a rootless container with two users. | (b) Linux only. |
| F6 | Egress default for personal lanes | Off. Validators marked `local = true` in `validators.toml` (the owner declares that the command keeps data on the machine) are exempt, so personal-lane rules can still be validated. | (b) On everywhere, as today. |
| F7 | Merge under authorisation | A merge needs the merging principal to read and write every logical with a classified change. Otherwise it is `MergeUnauthorized{count}`, naming nothing. The store owner can always merge. | (b) Merge carries changes the merger cannot see, as a transport. |
| F8 | Non-owner users and hooks, observer, freshness | In P9, non-owner principals use **MCP only**. They have no hooks, outbox or observer, and their reads report observation freshness against the owner's observed worktree, as documented. | (b) Per-principal outboxes and observers (a later phase). |
| F9 | `ctx ui` in multi-user mode | A random token per launch, given in the URL fragment, kept in `sessionStorage` (per origin, so per port) and sent as an `Authorization` header; every request also checks `Origin`. No cookie, since cookies are shared across ports. Record links render as text, not as navigable links. Without the token mode, the UI refuses to start while other principals are mapped. | (b) Serve over a 0600 Unix socket only. |
| F10 | Blob dedup across principals | Proof of possession: a non-owner commit must carry the bytes of every content hash its ops introduce. Without them the daemon answers `MissingBlob`, whether or not the blob exists. | (b) Per-principal blob namespaces, with no cross-principal dedup. |
| F11 | Branch ownership | A branch's creator is recorded (from the creating batch's principal). Only the creator or the store owner archives it, and sessions auto-bind only to branches created by their principal or by the store owner. | (b) Branches are shared, as today. |
| F12 | Does the store owner see personal lanes? | Yes, on every channel: it can read the files anyway, and claiming otherwise would mislead. As a hygiene default (not security), the owner's agent-channel recall with no explicit scope leaves out other principals' personal lanes. | (b) The owner's agent channel follows personal-lane visibility everywhere (cooperative only). |
| F14 | What may a non-owner merge carry that it could not write directly? | **Owner-confirmed, narrowed (2026-11-06).** (1) **Into the merger's own branch** (created by it), every mergeable change is transported: only its creator and the store owner commit there (F11), and reads still go through §4.4. (2) **Into `main` and branches the store owner created:** a merge that actually changes the target's Policy must be run by the store owner; ValidationRecords are transported with their exact `(rule, curvec)` and their original `validator` source; CodeAnchor objects are transported as not base and `unobserved`, so they carry no base authority; every other change needs the direct-op permission (curator for base, `Read` on every rule premise, the artifact targets' rules). | (b) The direct-op rule for every class and every target: non-owners cannot catch their branch up with `main` once `main` holds a policy change, a verdict, an anchor or any record they cannot read; the store owner performs those merges. |
| F13 | May a principal who holds the exact bytes learn that they were shredded? | Yes, accepted and documented: re-sending shredded bytes is `ContentShredded`. Dry runs of non-owners need the bytes too, so the hash alone never suffices (§6.3). | (b) Accept the bytes silently into a new, unrelated content (breaks dedup and the shred set's meaning). |

Out of scope:
- signed logs and certificates (F4a);
- keyed content hashes (§8.7);
- several principals per OS user (F1a);
- per-lane recall indexes against ranking side channels (§5.9; P10);
- per-principal quotas (§2.3);
- networked transports (P11);
- hooks and observers for non-owner principals (F8a).

## 1. Today

At `5355556` (P8 passed):

- **Identity.**
  - Three sockets under `<data>/run/` (or `$TMPDIR/ctxd-<uid>/…` when the
    path is too long): `agent.sock`, `observer.sock` and `admin.sock`.
  - The run directory is forced to 0700 and each socket to 0600
    (`server.rs:154–176`).
  - The principal is fixed per socket: Agent 1, Observer 2, Admin 0
    (`server.rs:44`).
  - The peer uid is read, but only to refuse other uids
    (`server.rs:329`).
  - Requests carry no identity. Every commit path overwrites
    `batch.channel` and `batch.principal` from the connection
    (`service.rs:1214, 1341, 1877, 1902`).
  - The principal is recorded in the op_log row, `Revision.author`,
    `Logical.owner`, the session log's `Start` and the certificate body.
  - The data directory and its files are created with the default umask.
- **Clients find the daemon through `ctxd.lease`.** That covers MCP, the
  hooks, the CLI and the UI. The outbox lives in `<data>/outbox`, and the
  sweeper delivers it as principal 1.
- **Sessions.** `end_session` refuses another principal's session.
  `bind()` checks no principal for `Session`, `Key` or `Call`
  (`service.rs:1838–1900`).
- **Lanes.**
  - `scope_lane` is any string, immutable after creation. `SetMetadata`
    narrows `dirs`; from empty, the first value is free.
  - The values in use are `"repo"` and `"code:<repo id>"`.
  - `owner` is never read for authorisation, and `personal` and `shared`
    exist only in the SPEC.
- **Authorized.**
  - `Dimensions.authorized` is stored in the derived snapshot and
    hard-coded `true` (`layers.rs:409`, `eval.rs:269`).
  - `summarize` already ranks `Unavailable` first, and `entry_outcome`
    maps it to `Denied` (`cert.rs:98`).
  - `validate_certificate` ignores `cert.principal`.
- **Responses that name ids.**
  - WHY, WHY_NOT and IMPACT are unbound requests. They carry ids, not
    payloads, but `StaleReason::SupersededPinnedPremise(RevisionId)`,
    `IncidenceFailure`, `RuleFailure` head vectors and
    `NodeState.witness` name revisions and rules.
  - So do `CommitResult` (`status_diff`, `l0_flips`, `net_delta`),
    `WhatIf`, `ImpactExact`, the unbound `Diff` (the whole branch's
    `EpochDiff`), the `Merged` trailer, `TxnStatus`, certificate
    `Invalid{diffs}`, and errors (`HeadConflict`, `ReadSetConflict`,
    `UnknownRef`, `CycleDetected{path}`, `MergeNotCertified`).
- **`ctx ui`.** HTTP on 127.0.0.1 with a `Host` check and no credential.
  It reads `recall.sqlite` directly.
- **Blobs.**
  - Plaintext `CXBP` pack records, content-addressed by BLAKE3 and global
    across lanes.
  - The kernel's `MissingBlob` is checked against the global store
    (`txn.rs:374`), so referencing any existing hash succeeds without the
    bytes.
  - `stage_blobs` rewrites any blob that is not `Available`
    (`store.rs:1000`).
  - Packs are durable through ordered syncs plus the op_log group's full
    sync (`store.rs:988`).
- **Plaintext copies of bodies** outside the packs:
  - `recall.sqlite`: body (64 KiB), abstract, outline, vocabulary; WAL
    mode;
  - `vectors.sqlite`: WAL;
  - `jobs.sqlite` and `candidates.sqlite`: validator output; WAL;
  - the embedder's and validators' inputs;
  - the git mirror and its backups;
  - `meta` and note blobs, which may quote bodies.

  The session log holds handoff summaries and the free-text supersede
  `reason`. The outbox holds tool facts, paths and shell commands. The
  op_log and `ctx export` hold keys, lanes, tags, dirs, branch names and
  content hashes.
- **Op log.** Format 4, with tags up to 19 and an unkeyed hash chain.
  Frame checksums exclude the segment header. `SetPolicy{PolicyDoc v1}`
  is admin only and per branch.
- **Channel checks.**
  - Admin only: `Mirror`, `Reindex`, `Validate`, `Validators`, `Shutdown`,
    `Telemetry`, the anchor range check.
  - Refused on the agent channel: raw batches, `RecordValidation`.
  - The observer may send anchor ops, `ObserverWatermark`, `AnchorBroken`
    and `ExpectHeads` (`txn.rs:204–241`).
  - Anchor writes from other channels are `KindRestricted`.
- **The protocol.** The daemon accepts exactly its own protocol version
  (`proto.rs:2513`, `server.rs:348`).

## 2. Threat model

### 2.1 Attackers in scope

1. **Another OS user** with a shell, who can run any client and connect to
   any socket it can reach.
2. **A legitimate principal's agent** trying to read or change records
   outside its grants, through MCP, the CLI or a raw socket. It may forge
   any request field and send any bytes.
3. **An external receiver** of data the daemon sends out: a validator, an
   embedder, a git repository.
4. **A later reader of the disk** after a shred.

### 2.2 Guarantees

- **G-ID.** A connection's principal is the access document's mapping of
  its peer uid. Nothing in a request changes it (T58).
- **G-READ.** No response to principal *p* names the key, id, revision,
  rule, witness, payload or status of a logical *p* is not authorised
  for. The exceptions are the counts listed in §2.3 and the key-echo
  shape of §5.2, for a key *p* itself supplied (T17, T17b, T100, PROP15).
- **G-WRITE.** Every committed write was authorised for its principal at
  its epoch (T60, T102).
- **G-POSSESS.** A non-owner principal can attach only content whose bytes
  it sent, or content already attached to a logical it may read (T107).
- **G-EGRESS.** No body of a non-egress lane reaches a validator not
  marked `local`, the embedder or the mirror (T105).
- **G-SHRED.** After `ShredContent` commits and its purge completes, no
  file the daemon controls lets anyone recover the payload: not the packs,
  the key log, the projections or their WAL and free pages. The session
  log and outbox are outside G-SHRED (§8.6). Topology and status are
  unchanged (T59, T103, PROP16).
- **G-FS.** In multi-user mode every entry in `<data>` is owned by the
  daemon's uid and carries no ACL; every file is 0600 and every directory
  0700, whichever process wrote it, except `<data>`, `run` and
  `run/shared` (0711), `run/shared/endpoint` (0644) and the shared socket.
  Only the shared socket is usable by others (T98, G6).

### 2.3 Not guaranteed (documented)

- **Same-uid processes are the store owner** (F12). To isolate an agent,
  run it as another OS user.
- **Backups, snapshots, swap and SSD remanence.** These may keep payloads,
  or key-log versions, from before a shred. The same holds for freed
  filesystem blocks: an unlinked generation is gone from the directory,
  but its blocks are not overwritten, and on APFS (copy-on-write) an
  overwrite would not reach them either. G-SHRED is a filesystem-level
  guarantee, and a shred is complete for the live data directory only. `ctx shred` and the README say so.
- **Counts and timing other principals can observe:**
  - `Redacted{count}` and `redacted_rules` in WHY, WHY_NOT and IMPACT;
  - `redacted: count` in diff-shaped responses (§5.5);
  - `dropped_unauthorized` in handoff;
  - Summary `premises{moved, total}`;
  - `vector_size` in certificate responses, and `MergeUnauthorized{count}`;
  - global epoch numbers and dense global ids in commit results;
  - empty `ctx_changes` pages, which show activity.
- **Side channels.**
  - Recall latency and bm25 scores depend on unauthorised rows (§5.9).
  - An authorised record's status can change because a hidden premise
    moved: SD11 is principal-independent (§4.5).
  - A record conflicted with a hidden partner reads `CONFLICTED`, and its
    reader cannot resolve it.
- **Shredded bytes (F13).** A principal holding the exact bytes of a
  shredded text learns that it was shredded (`ContentShredded`).
- **Key existence.** A read by a key the caller supplies answers
  UNAVAILABLE for a hidden logical, not "no such key" (§5.2).
- **Confirmation attacks on `content_hash`.** These are open to holders of
  the op_log and projections: the store owner and its copies (§8.7).
  Through the socket G-POSSESS closes them.
- **Data already sent out** before a lane's egress turned off, or before a
  shred.
- **Denial of service by a mapped principal.** It can fill the disk, which
  poisons the store, or saturate the writer and the connection threads.
  There are no per-principal quotas in P9. The store owner removes the
  principal (`AccessRevoked`).
- **Reused names.** A name keeps its principal id forever (§3.1). Giving a
  removed person's name to someone new gives them that principal's owned
  logicals. Use a new name.

## 3. Principals and the transport

### 3.1 Principals and ids

```toml
[principals.alice]
uid = 501        # must be the uid the daemon runs as: the store owner
[principals.bob]
uid = 502
```

- `PrincipalId` stays `u32`. **0–15 and `0x5641_0000–0x5641_FFFF` are
  reserved:**
  - 0 is the store owner; 1 and 2 are the owner's agent and observer
    channel principals;
  - 3–15 are reserved;
  - the `0x5641_…` range is the validator workers.
- Access-document principals get ids from 16 upwards, in order of first
  appearance in the access-document history. An id is never reused, and
  a name always maps to its first id.
- The store owner is the daemon's uid: principals 0, 1 and 2, and the
  owner's entry in the document, which maps to 0.
- In-process principals (the validator workers, the outbox sweeper) act
  for the store owner, subject to egress (§7).
- Without an access document, only the store owner exists (T106).

### 3.2 Sockets and directories (B1)

- In multi-user mode:
  - `<data>`, `<data>/run` and `<data>/run/shared` are **0711**: others may
    traverse them but not list them;
  - every other entry is 0700 or 0600;
  - **the store layer creates every file 0600 and every directory 0700
    explicitly** (`open` / `mkdir` with a mode), so the modes hold whatever
    process writes: `ctxd`, `ctx migrate`, `ctx export`, `doctor`, the
    mirror, and the owner's hooks writing the outbox;
  - every binary that writes `<data>` (`ctxd`, `ctx`, `ctx-hook`, `ctx-mcp`)
    also sets `umask(077)`, which covers SQLite's `-wal`, `-shm` and
    journal files;
    - **modes are not enough: ownership and ACLs are checked too.** macOS
    extended ACLs (and POSIX ACLs on Linux) can grant another user access
    regardless of the mode bits, and an inheritable ACE on a directory is
    copied onto every new file in it. So:
    - every file and directory the store layer creates is created with
      an **empty ACL** (macOS `acl_set_fd_np` / `acl_set_file` with no
      entries, like `chmod -N`; Linux: the `system.posix_acl_access` and,
      for directories, `system.posix_acl_default` xattrs removed), and its
      owner is checked to be the daemon's uid (`fstat`);
    - at start, and after every `ctx migrate` and `doctor --fix-modes`,
      `ctxd` walks `<data>` and refuses to run, naming the entry and the
      reason, if any entry: is not owned by the daemon's uid; has a mode
      that grants group or other more than the public entries allow; or
      carries **any** ACL entry (allow or deny, explicit or inherited,
      including inheritable entries on `<data>` itself). Deny-only ACLs
      are refused too: the rule stays "no ACLs inside `<data>`", which is
      simple to check and to explain;
    - the public entries (`<data>`, `run`, `run/shared`, `endpoint`, the
      shared socket) follow the same rule: their access comes from the
      mode bits only;
    - `ctx doctor --fix-modes` repairs modes and strips ACLs; it never
      changes an owner (a foreign owner needs the owner's attention);
        - SQLite creates its own `-wal`, `-shm` and journal files with
      `open()`, so the empty-ACL-on-create rule does not reach them; their
      ACL can only come from their directory's inheritable ACEs (macOS)
      or default ACL (Linux, which also overrides the umask). They are
      covered **because no directory inside `<data>` may carry any ACL**,
      `<data>` included. That is why the rule refuses deny-only entries
      too, and it must not be relaxed for directories;
    - files an external process creates in `<data>` follow the same
      rules: `ctx service install` sets `Umask = 63` (077) in the launchd
      plist and `UMask=0077` in the systemd unit, so the service log
      `<data>/ctxd.log` is created 0600;
    - the ancestor check (below) reads ancestors' ACLs as well, and
      reports a deny of `search` for a mapped principal or its groups.
      It is a best-effort pre-check; the authoritative check is the real
      two-user run (G6), because access cannot be evaluated as another
      user without being that user.
- `observer.sock` and `admin.sock` stay 0600 in `<data>/run/private/`
  (0700).
- `agent.sock` moves to `<data>/run/shared/` (0711). It is 0600 while no
  other principal is mapped and 0666 when one is. The daemon `chmod`s it
  on start and on every `SetAccess`. 0666 lets any uid connect, and the
  uid check refuses unmapped ones (§3.3).
- **Every ancestor directory of `<data>` must be searchable by each mapped
  principal**, through the other or the group `x` bit (macOS homes are
  `drwxr-x---`, group `staff`). At start, multi-user mode checks the chain
  against each mapped uid's groups and refuses to run, naming the first
  directory that blocks. Making a home directory searchable exposes its
  world-readable files, so the README recommends a data directory outside
    home, in a directory the owner creates (for example
  `/Users/Shared/<owner>-contextdb/<project>` or
  `/srv/contextdb/<owner>/<project>`).
- **Ancestor ownership (ssh `StrictModes` style).** Every ancestor of
  `<data>` must be owned by root or the owner, and must not be group- or
  other-writable, unless it is sticky and the next path component is
  owned by root or the owner (`/Users/Shared`, `/tmp`). Otherwise another
  user could rename the owner's directory away and plant their own, with
  their own daemon behind the socket. Multi-user mode refuses to start
  otherwise, naming the directory.
- **Clients authenticate the server.** `run/shared/endpoint` records the
  owner's uid. A client (`ctx-mcp --data`, `ctx`) checks, before sending
  anything, that the socket's peer uid (`getpeereid` / `SO_PEERCRED` on
  the client side) equals that uid and the owner of `<data>`, and that
  `<data>`'s ancestors pass the ownership rule above; otherwise it
  refuses (`ServerNotOwner`). A planted directory therefore cannot serve
  forged records to other principals. `$TMPDIR` is
  per-user on macOS, so the long-path fallback cannot be used and the
  same check refuses it.
- **Discovery without the lease.** `ctxd.lease` stays owner-only.
  `run/shared/endpoint` (0644) holds only the protocol version and the
  socket file name. Non-owner clients take `--data <dir>` (or
  `CTXDB_DATA`) and derive the path: `ctx-mcp --data <dir>`.

### 3.3 Connection

- On accept the daemon reads the peer uid.
  - The owner uid becomes the socket's channel principal, as today.
  - A uid the current access document maps becomes its principal, on
    `agent.sock` only.
  - Any other uid is refused before a byte is read.
- The mapping is checked again on every request. A connection whose uid
  was removed gets `AccessRevoked`, and the connection closes.
- Non-owner principals get MCP-level requests only (F8). The list is
  closed:
      - sessions: `SessionStart`, bind, `SessionAttach`, end (their own;
    harness keys looked up per principal), and `Updates` (a peek on its
    own session, redacted per §5.5): the calls `ctx-mcp` makes;
  - reads: recall, read (all levels, which the UI uses for abstracts),
    check, why, why_not, impact, `ImpactBatch`, `Diff`, `BranchList`, and
    `OnBranch` wrapping any of these;
  - writes: record, supersede, commit, what_if, `TxnStatus` (own rows
    only, §5.5);
  - changes, conflicts (list, `ConflictOpen`), resolve;
  - handoff begin and accept;
  - branch create, branch archive (their own branches), merge.

  `Status` (store totals) is store-owner only.

  Hook requests (PreToolUse, PostToolUse, Stop, the outbox) need the
  store owner's uid, so a non-owner gets `ChannelRestricted`.

### 3.4 Sessions and handoff (T58, T99)

- A session belongs to the principal that started it.
  - `bind()` refuses `Binding::Session` and `Binding::Call` for another
    principal's session (`SessionOfAnotherPrincipal`).
  - Harness keys `(harness, harness_session)` are looked up per principal.
- **Handoff across principals.** The sender must name the recipient:
  `ctx_handoff_begin{to?: principal name}`. An unnamed handoff is
  same-principal only.
  - The listing and accept show only handoffs started by, or addressed
    to, the caller.
  - On accept, the copied reads are filtered to those the acceptor may
    read. The rest are dropped and counted (`dropped_unauthorized`).
  - The acceptor's own validation of the copied entries decides the
    verdict. The sender's certificate is never validated for, or shown
    to, another principal.
- A certificate presented by a principal other than the one it names:
  `Denied`.

## 4. The access document

### 4.1 Representation

- **The op.** `SetAccess{doc: AccessDoc}`: admin only, **sole op in its
  batch**, inline canonical JSON (like `PolicyDoc`, SPEC §6.1 contract 8).
  Implementation note (milestone 2): the canonical form also names the
  owner's entry (`owner`), filled by `ctx access apply` from the caller's
  uid and checked by the daemon; a name never switches between the owner
  and another principal (gate P9).
- **Global.** It applies to every branch from its epoch (an SD19
  exception, §12). Its frame is committed on `main`. It cannot appear in
  a WHAT_IF or a merge, and `SetPolicy` stays per branch.
- **The CLI.**
  - `ctx access apply <file>` (admin), from `.contextdb/access.toml`.
  - `ctx access show`.
  - `ctx access check <principal> <key> [--branch B]`: the decision, and
    the step that made it.
- **Reads use the current access document,** including reads of
  historical epochs. A revocation applies at once: to the ledger, to
  certificates, and to history.

```toml
version = 1

[principals.alice]
uid = 501
[principals.bob]
uid = 502
[principals.carol]
uid = 503

[groups]
billing = ["bob", "carol"]

[lanes.repo]                       # shared lane
members  = ["*members"]            # every principal in the document
writers  = ["*members"]
curators = ["alice"]               # or "*writers": writers may assert bases
egress   = true

[lanes."billing-notes"]
members  = ["@billing"]
writers  = ["@billing"]
curators = ["bob"]
egress   = false

[[lanes."billing-notes".grants]]   # dir narrowing for one member
principal = "carol"
dirs = ["src/billing/invoices"]

[[lanes.repo.deny]]                # ACL: key-prefix denies
principal = "carol"
prefix = "secrets."

[code_lanes]                       # every "code:<repo id>" lane not listed
members  = ["*members"]            # anchors: paths and hashes of repository content
writers  = ["*owner"]              # only the observer writes anchors
curators = ["*owner"]
egress   = true

[personal]                         # the synthesised personal:<name> entries (§4.3)
egress   = false

[default_lane]                     # every other lane not listed
members  = ["*owner"]              # fail closed (the default if omitted)
writers  = ["*owner"]
curators = ["*owner"]
egress   = true
```

### 4.2 Validation

Refused with `InvalidAccess{reason}`:
- an unknown version or field (the known top-level tables are
  `principals`, `groups`, `lanes`, `code_lanes`, `personal` (only
  `egress`) and `default_lane`);
- a missing owner entry, or an owner uid that is not the daemon's;
- a uid mapped twice, or a name not matching `[a-z0-9_-]{1,32}`;
- a group naming an unknown principal, or nested groups;
- a lane entry naming anything but a principal, `@group`, `*members`,
  `*owner` or (curators only) `*writers`;
- grant `dirs` with an empty, `.`, `..` or `*` segment;
- an empty deny prefix;
- a lane named `personal:…` (personal lanes are implicit), or a `code:…`
  lane (or `code_lanes`) whose `writers` go beyond `*owner`;
- a grant or deny for a principal that is not a member.

`ctx migrate` (§10) refuses a store that has lanes named `personal:<x>`
for an `<x>` that is not a valid name.

### 4.3 Lane entries

Every lane has exactly one **entry** `{members, writers, curators,
egress, grants, deny}`, found by one function `entry(lane)` that every
read and write rule uses:

1. a lane listed under `[lanes."<name>"]` → that entry;
2. `personal:<name>` for a principal `<name>` in the document → the
   synthesised entry `members = writers = curators = ["<name>"]`, no grants,
   no denies, `egress` from `[personal] egress` (default `false`, F6);
3. `personal:<x>` for an `<x>` that is not a principal → the empty entry
   (nobody but the store owner);
4. an unlisted `code:<repo id>` lane → `code_lanes`;
5. any other unlisted lane → `default_lane`.

Personal lanes cannot be listed (§4.2), so case 2 is the only way their
entry arises, and a principal therefore creates, changes, curates and
reads its own personal records under the same rules as any lane: it is
the lane's only member, writer and curator.

### 4.4 Authorized(p, l, b)

The inputs:
- the current access document *A*;
- the principal *p*;
- the logical *l*'s lane and owner (global, immutable);
- *l*'s `dirs` metadata on the read's branch *b*, as of the current epoch.

There are no anchor paths and no rules, so a writer cannot widen coverage
by adding a rule (r0.1 B4). The steps run in the SPEC's order; the first
decisive step wins.

1. **Owner.** *p* is the store owner (§3.1) → allow. `owner(l) = p` →
   allow.
2. **Membership.** *p* is not a member of `entry(lane(l))` (§4.3) → deny.
3. **Visibility.** `personal:<name>` with `<name>` ≠ *p* → deny. (This
   restates the synthesised entry; it is kept as its own step because the
   SPEC orders visibility before role, and because it holds even if a
   future document format let personal entries be edited.)
4. **Role.** Members read. If *p* has grants with `dirs` in this lane,
   they narrow it: allow only if a grant dir covers one of *l*'s `dirs`
   on *b*. A logical without dirs is not covered (fail closed).
5. **ACL.** A `deny` entry for *p* (directly or through a group) such
   that `key(l)` starts with its prefix → deny. Otherwise → allow.

**Dependence on the branch.** `dirs` is per branch (SD19), so a
dir-narrowed principal's answer can differ between branches. It is stable
while *A* and that branch's `dirs` stay unchanged. Changing `dirs` needs
read and write access to the logical first (§6). A record without `dirs`
is invisible to dir-narrowed principals, and its first `dirs` value is
free (SPEC §6.1 contract 10): **its writers decide which narrowed
principals may see it**. Later changes only narrow. T13 is amended:
historical reads are byte-stable **per principal** while *A* and the
branch's `dirs` are unchanged.

### 4.5 Where it enters the status

- `Certified(r) = Current(r) ∧ L2(r) ∧ Authorized(p, l(r), b)` (SD11).
  Authorisation is **not transitive**.
- The derived snapshot stays principal-independent: counters, L0/L1/L2
  and digests. `Dimensions.authorized` stays a constant `true` in the
  snapshot, so no digest changes. Visibility is applied when a response
  is rendered, like observation freshness (SD14b, SD13 visibility
  dimension).
- `summary_at` gains a principal and a branch. The oracle's `summarize`
  already has the row.

## 5. Read paths

### 5.1 Principles

1. A response to *p* names only logicals that *p* is authorised for,
   together with their revisions, rules and artifacts. A key *p* supplied
   itself is the exception (§5.2).
2. Everything else is either **dropped**, or **collapsed into a count**
   where the response's shape already counts things: §5.4–§5.6, listed in
   §2.3.
3. Redaction is one function, `redact(p, b, response)`, applied at a
   single point: the daemon's response encoder for non-owner principals.
   The store owner gets responses unfiltered. PROP15 checks the function
   on every response type.

Implementation note (milestone 3): the counts of §5.4–§5.6 travel in one
`redaction` record on the response envelope, so the durable values (commit
results, merge reports, certificates) keep their encodings. A hidden target
answers `Unavailable{target}`. `Witness::Redacted` and
`SupersededPinnedPremiseRedacted` exist only in responses. Until milestone 4,
error details are withheld wholesale from non-owners. WHY, WHY_NOT, IMPACT
and Diff stay unbound: redaction keys off the connection's principal and the
branch the request reads (gate P9).

### 5.2 Items and `ctx_read` (T17)

A hidden logical read by a key *p* supplied gives the item
`{key, status: Unavailable}`: no `logical`, handle, revision, kind, body,
levels, `NodeState`, witness, anchors or premises. It gets no ledger
entry and no certificate entry. Recall never returns this shape (§5.3).

Authorised items are redacted inside (M1):
- `StaleReason::SupersededPinnedPremise(rev)` with a hidden `rev` becomes
  `SupersededPinnedPremise(Redacted)`;
- an inherited STALE reason is a copied value (SD13), not a pointer; it
  is shown as is, except that a `SupersededPinnedPremise` it carries is
  redacted as above. The copied value is the status side channel of
  §2.3;
- `NodeState.witness` naming a rule with any hidden premise becomes
  `Redacted`;
- `Item.anchors` keeps only authorised anchors (anchor lanes are lanes
  like any other);
- a pointer to an open Conflict whose other side is hidden becomes
  `conflict: Redacted`.

### 5.3 `ctx_recall`

- `admit()` checks `Authorized` first.
- A hidden match is skipped as if it did not match: it is not withheld,
  not counted, and takes no slot.
- Paging continues past hidden matches, so a page is short only when the
  matches *p* may see are exhausted (PROP14 per principal).
- A scope naming a lane *p* is not a member of returns no items and no
  error.
- Vectors are used only for heads in egress lanes (§7).

### 5.4 WHY, WHY_NOT, IMPACT (T17b)

These requests become bound. A hidden target gives `Unavailable` and
nothing else.

- **WHY.** Base flag, authorised premises and references.
  - A rule with hidden premises shows its authorised premises plus
    `Redacted{count}`.
  - A rule whose premises are all hidden is counted in `redacted_rules`.
  - The witness is shown only if every premise of the witness rule is
    authorised.
  - A Conflict with a hidden side is `Redacted`.
- **WHY_NOT.** Each failing incidence on a hidden premise is
  `Redacted{count}`. `RuleFailure.asserted` and `current` keep only
  authorised entries plus a count. `SemanticHeadUnsupported(r)` with a
  hidden `r` becomes `Redacted`. The recursion stops at redacted nodes.
- **IMPACT and `ImpactExact`.** Authorised dependants only, plus
  `redacted: count`, split into supported and reference-only.

### 5.5 Diff-shaped responses (B2)

The rule for every response that lists changes: entries for hidden
logicals are dropped, and the response carries one `redacted: count`.
The responses are:
- `CommitResult`: `status_diff`, `l0_flips`, `net_delta`, the allocations
  of *p*'s own batch (always authorised);
- `WhatIf`;
- `TxnStatus`, which answers a non-owner only for txn ids whose op_log
  row carries its own principal (others: `UnknownTxn`, as for an unknown
  id);
- `Diff` (now bound; an empty page keeps its cursor);
- `ctx_changes`;
- the `Merged` trailer, including the Conflicts a merge opened;
- `Updates` (`context_update`);
- certificate `Invalid{diffs}` and `ValidRecertified{diffs}`.

### 5.6 Certificates (M2)

- An issued certificate's `vector` keeps the full closure, so validation
  stays exact. A non-owner never receives the vector: the response carries
  the cert id, the entries *p* read, and `vector_size`.
- `validate_certificate` recomputes `Authorized(cert.principal, l, b)`
  under the current *A* for every entry. A revocation makes the
  certificate `Denied` (T101). Diffs are redacted as in §5.5.
- The PreToolUse gate exists only for the store owner (F8), so it is
  unchanged.

### 5.7 Errors (B6)

Every error a non-owner receives names only:
- refs *p* itself sent, echoed as sent (the key, `key@rev` or handle);
- op indexes in *p*'s batch;
- authorised logicals.

The kernel errors are mapped at the redaction point:
- `HeadConflict` and `ReadSetConflict` are reached only after the
  `Unauthorized` check (§6.1), so they can never concern a hidden
  logical;
- `CycleDetected{path}` keeps only authorised nodes, plus a count;
- `MergeNotCertified{logical, status}` for a hidden logical becomes
  `MergeNotCertified{redacted: count}`;
- `Unauthorized{op_index, ref, need}` echoes *p*'s own ref, never an id.

### 5.8 Feeds, listings and the UI (B3)

- **Filtered with no count:** `ctx_conflicts` (a pair is listed only if
  both sides are authorised), the briefing, the Stop report, the handoff
  listing (§3.4).
- **`ctx ui` (F9).** The UI connects on `agent.sock` as its OS user's
  principal and shows what that principal may read.
    - In multi-user mode it needs its launch token (F9): URL fragment,
    `sessionStorage`, `Authorization` header, `Origin` check, no cookie,
    record links rendered as text. Without it, `ctx ui` refuses to start
    while other principals are mapped.
  - Abstracts come from the daemon (`Read{level: abstract}`), never from
    `recall.sqlite`, for any user once an access document exists.
  - The three current deployments are single-user and unchanged.

### 5.9 Recall side channels (documented)

`recall.sqlite` is one FTS5 table. Document frequencies and skipping cost
depend on hidden rows. A principal may learn, statistically, that
frequent terms exist in lanes it cannot read, but never which records
hold them, or their text. Per-lane indexes are a P10 option.

### 5.10 The store owner

The store owner (any channel of the daemon's uid) is never filtered. It
reads every lane, including other principals' personal lanes (F12).

## 6. Write paths

### 6.1 Placement and order (B6)

All checks run inside the kernel transaction against the staged access
document and `batch.principal`. A dry run (`ctx_what_if`,
`ImpactBatch`) and the real commit therefore share one predicate. Replay
never re-checks: the batch was valid at its epoch. The store owner passes
every check.

SPEC §6.2 order, amended per op: right after `UnknownRef` for the refs the
op names,
- `Unauthorized{op_index, ref, need}`, with `need ∈ {Read, Write,
  Curate}`;
- for a ref to a hidden logical, `Unauthorized(Read)` whether or not the
  revision named in it exists (no enumeration).

`ContentShredded` sits right after `MissingBlob`. `ExpectHeads` entries,
from the client or appended from a certificate (appended **before** the
kernel checks), need `Read` on each logical.

Implementation note (milestone 4): the kernel always authorises, and
replay runs the same deterministic predicate. `Unauthorized` carries the
ref as written. The read check covers every revision an op names (gate
P9).

### 6.2 Rules

Every rule below looks up permissions through `entry(lane)` (§4.3);
personal, code, listed and default lanes have no special cases.

1. **Create** (`CreateLogical` in lane *L*): *p* is a writer of
   `entry(L)`. For `personal:<name>` that is exactly `<name>`; a
   `personal:` lane naming an unknown principal has the empty entry, so
   only the store owner could create there, and it is refused for
   everyone as `InvalidLane` instead.
2. **Change** (`AddRevision`, `SetHead`, `SetMetadata` on *l*). *p* reads
   *l* and is a writer of `entry(lane(l))`.
3. **Bases** (`adopt_as_base`, `AssertBase`, `RetractBase`): *p* is a
   curator of `entry(lane(l))`, and reads *l*. This is the provenance
   gate: a shared base needs a curator's batch; in a personal lane the
   principal is its own curator. Observer anchor ops are exempt (SD18.3):
   only the store owner has an observer.
4. **Rules** (`AddRule`, `Rebind`, `SetRuleEnabled`, and the deps of
   `ctx_record` / `ctx_supersede`; T60). *p* reads the conclusion and
   every premise (old and new for `Rebind`), `Ref` included, and writes
   the conclusion's lane.
5. **Artifacts.** Opening or resolving a Conflict or PendingReview needs
   `Read` on every target and `Write` on one. `RecordValidation` stays
   admin and validators only.
6. **Branches (F11).**
   - `CreateBranch` records its creator: the batch principal, already in
     the op_log row.
   - `ArchiveBranch` needs the creator or the store owner.
   - Sessions auto-bind (SD19 D1) only to open branches created by their
     principal or by the store owner. A squatted name is skipped and reported.
   - Commits on a branch, by explicit binding or `OnBranch`, are open
     only to its creator and the store owner (`BranchOfAnotherPrincipal`);
     `main` and branches the store owner created are open to every writer.
     This keeps one principal from planting changes that block another's
     merge.
7. **Merge (F7, B5).** `Op::Merge` (ADR 0013, `kernel_merge.rs`) applies
      to the target every change it takes from the source: an S-side value
   taken, a convergence, the objects newly visible to the target
   (whether S created them or received them by an earlier merge or by
   cross-branch dedup), and the Conflicts the merge opens. "Keep T"
   entries apply nothing and need nothing.

   **The rule (F14, owner-confirmed).** A merge into `main`, or into a
   branch the store owner created, may apply a change only if the merging
   principal could have made that change directly on the target with the
   ops of 1–6 and 8; the two transported classes below (ValidationRecords
   and CodeAnchor objects) are the only exceptions, and they keep their
   provenance. A merge into a branch
   **the merging principal created** transports every mergeable change
   without these checks (F14): only its creator and the store owner can commit there
   (6), and carrying a change makes nothing readable, because reads are
   decided per logical by §4.4 wherever the record lives. Class by class,
   for merges into `main` and owner branches, against the target's state
   and the current access document:

   | Class the merge applies | Permission the merging principal needs (direct-op equivalent) |
   |---|---|
   | **Policy** (taken from S) | the store owner, when the merge actually changes the target's policy, as for `SetPolicy` (8). A merge that leaves the target's policy unchanged (S's policy equals T's, or "keep T") needs nothing. A policy conflict still refuses the merge, as in ADR 0013 |
   | **New logical** (created on S) | writer of `entry(lane)` (1) |
   | **New revision / head** of a logical | `Read` and writer of `entry(lane)` (2) |
   | **Metadata** (`dirs`, `tags`) of a logical | `Read` and writer (2); for `dirs`, also the narrowing rule of SPEC §6.1 contract 10 against the target's value |
   | **Base bit** of a revision (asserted or retracted, including a new revision arriving as base) | curator of `entry(lane)` and `Read` (3) |
   | **CodeAnchor logicals and revisions** (observer domain) | none: transported. Only the observer writes them (SD18.3); they arrive as not base and `unobserved` (`kernel_merge.rs`), so they carry no base authority from the merger, as SD19 already lets the agent channel merge them |
   | **New rule**, or a rule's **enabled** flag | `Read` on the conclusion and on every premise (`Ref` included), writer of the conclusion's lane (4) |
   | **ValidationRecord** `(rule, curvec)` | none: transported. Only the admin and the validator workers can `RecordValidation` (5), so a record on S was written by one of them. The merge carries it unchanged: the exact `(rule, curvec)` it was recorded for (never re-keyed to the target's current vector) and its original `validator` source and `meta`; the merger writes no verdict of its own. Different verdicts on S and T open a Conflict, as in ADR 0013 (the merger needs `Read` on the rule's conclusion to open it, row below) |
   | **Artifact** opened or resolved on S (Conflict, PendingReview) | `Read` on every target and writer of one target's lane (5). `AnchorBroken` artifacts are never carried by a merge (`kernel_merge.rs`) |
   | **Conflict opened by the merge** itself (a both-changed head, enabled flag, verdict or metadata key) | `Read` on every side; it adds no new authority, but a principal may not open a Conflict on a record it cannot see |

   - Any change failing its row refuses the whole merge with
     `MergeUnauthorized{count}`, naming nothing: the count is the number
     of failing changes, an accepted count (§2.3).
   - The checks run inside the merge transaction, after classification and
     before anything is applied, so a dry run (`what_if` of a merge) and
     the real merge give the same answer (§6.1).
   - The store owner passes every row, as for direct ops.
   - The merge report and `MergeNotCertified` are redacted (§5.5, §5.7).
      - PROP15's write half includes merges: for random source and target
     histories and access documents, a merge into `main` or an owner
     branch is refused exactly when some applied change other than a
     transported ValidationRecord or CodeAnchor object fails the
     direct-op reference for its class (a Policy change by anyone but the
     store owner included); transported records keep their `(rule,
     curvec)`, `validator`, base bit and `unobserved` flag,
     and a merge into the merger's own branch is never refused for
     authorisation.
8. **Global ops.** `SetAccess`, `ShredContent` and `SetPolicy` are
   admin only. The two global ones are sole ops (§4.1, §8.3).

### 6.3 Proof of possession (F10, B8)

For a non-owner principal the daemon checks, before the kernel, **every
content-hash field** its batch carries: `CreateLogical.content`,
`AddRevision.content`, `ResolveArtifact.note`, `OpenArtifact.payload_hash`
(and `RecordValidation.meta`, should it ever open to non-owners). The
same check runs for dry runs (`WhatIf`, `ImpactBatch`), which therefore
accept blobs, so a hash alone never answers differently in a dry run
than in a commit (F13).
- The batch must carry that hash's bytes, and BLAKE3 must match, **unless**
  the hash already belongs to a revision of a logical *p* may read on the
  batch's branch.
- Otherwise the answer is `MissingBlob{content}`: the same error, and the
  same timing class, as for a hash nobody has. Existence therefore stays
  hidden.
- **Order for non-owners:** (1) proof of possession, `MissingBlob` unless
  the bytes match or the hash belongs to a revision *p* may read; (2) then
  the shred set, `ContentShredded`; (3) then staging. A shredded hash sent
  without its bytes therefore gets `MissingBlob`. For the store owner the
  shred set is checked before staging, so `stage_blobs` never stores a
  shredded hash again (§8.4).

## 7. Egress

`egress` on a lane defaults to `true` for listed shared lanes and
`default_lane`, and to `false` for personal lanes (F6). It never enters
`Authorized`.

- **Validators and the judge.** Jobs and pairs touching a non-egress lane
  go only to validators marked `local = true`.
  - That flag is the owner's statement, in `validators.toml`, that the
    command keeps data on the machine. `claude` validators cannot be
    marked.
  - Other jobs are `Skipped{egress}`, with no attempt counted. They are
    re-queued when a lane's egress turns on, or when a local validator is
    added.
  - A rule left without a validator stays `Pending` until a local
    validator or the store owner validates it.
- **The embedder.** It never receives a non-egress body. Vector ranking
  and embedding candidates use only heads in egress lanes. A vector
  computed for another logical with the same content is not used for a
  non-egress head.
- **The git mirror (M7).** A repository is readable by whoever can read
  it, so in multi-user mode the mirror exports only lanes that are egress
  **and** readable by every principal: members `*members`, with no grants
  and no denies.
  - Turning a lane's egress off, or restricting it, drops its files at the
    next export, with a `Ctx-Egress-Removed` trailer. The bodies remain in
    older commits until `ctx mirror export --rebuild` (§8.5).
    - Who can read the target repository is the owner's responsibility.
    In multi-user mode `ctx mirror export` refuses when the repository's
    `.git` directory is group- or world-readable, unless `--readable-ok`
    is given.
  - Single-user stores export as in P8.
- **`ctx export` and `doctor`** are store-owner only and unchanged.

Implementation note (milestone 5): without an access document every lane
egresses (T106). The mirror's multi-user condition is read from the
committed document (a principal besides the owner is mapped), so its render
stays a function of the log; skipped jobs stay `Queued` and are re-examined
on every claim, and skipped pairs are postponed and woken by a `SetAccess`
or a reload (gate P9).

## 8. Crypto-shred

### 8.1 Encrypted packs

- New pack record:

  ```
  "CXBE" | len u32 | content_hash 32 | nonce 24 | XChaCha20-Poly1305(key, nonce, plaintext, aad = content_hash)
  ```

  `content_hash` stays BLAKE3 of the plaintext, so addresses, dedup,
  anchors and the op_log are unchanged.
- A read decrypts, then checks BLAKE3. A mismatch is `Corrupt`.
- The key is random, 32 bytes, one per content hash, from `getrandom`. The
  nonce is random, 24 bytes, one per record.
- The crate is `chacha20poly1305` (RustCrypto, Apache-2.0/MIT), under
  `cargo deny`.
- The encryption serves **erasure**. Confidentiality from other OS users
  comes from file modes (G-FS).

### 8.2 The key log (M3)

- `<data>/keys/keylog-NNNNNNNN`: append-only records
  `"CXKY" | content_hash 32 | key 32 | blake3 check 8`.
- **Durability equals the packs'.** A blob's key record joins the same
  ordered syncs as its pack record, before the op_log group's full sync
  (`F_FULLFSYNC` on macOS, as configured for the log). An acknowledged
  payload therefore always has a durable key.
- The key log is a **truth store**, like the packs. SPEC §7 and §3.5 #7
  name it as part of the blob store. It cannot be rebuilt, and losing it
  loses every payload; backups must include it (§2.3).
- On open, the daemon loads every key into memory: 64 B per content, so
  6.4 MB at 10⁵ contents.
- **A shred rewrites the key log, on the writer thread.** Commits that
  append keys run on the same thread, so no key can be appended to an old
  segment during the copy. The rewrite writes `keys/keylog-<gen>.tmp`
  holding every key but the shredded ones, syncs it, renames it to
  generation `<gen>` and syncs the directory, both through the store's
  `Durability::sync` / `sync_dir` (`F_FULLFSYNC` on macOS by default), and
  only then unlinks every older generation and every `.tmp` file, and
  **syncs the directory again**. An unlink is durable only once its
  directory is synced (fsync(2)); without the second sync a power cut
  after the purge was reported could bring an old generation back, and
  with it the key. Recovery on open cannot help here, because someone
  may read the disk before any daemon starts. The key-log step of the
  purge (§8.4) completes only after this second sync. The cost is
  O(number of contents) per shred batch; several shreds share one batch.
- **Recovery.** Keys load only from the newest complete generation and the
  segments appended after it; for a hash with several records the last
  wins. On open, if any shredded hash still has a key in any file (a crash
  between the rename and the unlinks, or before the rename), the rewrite
  runs again before the daemon accepts requests. `.tmp` files are
  deleted. Recovery ends with the same unlink-then-`sync_dir` sequence.
  Before the purge reports completion this covers a crash at any point;
  after it, the disk already holds no old generation, so there is nothing
  to recover.
- `doctor` checks that every non-shredded content referenced by the
  op_log has a key and a record. It reports keys without records, which
  are harmless after a crash, and removes them.

### 8.3 `ShredContent`

- **The op.** `ShredContent{content}`: admin only, global, sole op (one
  or more `ShredContent` and nothing else in the batch), frame on `main`.
  It cannot appear in a WHAT_IF.
- **The CLI.** `ctx shred <key>[@rev] [--with-reviews] [--yes]`.
  - It resolves the content hash and lists every revision and logical
    sharing it on every branch.
  - `--with-reviews` adds the `meta` and note blobs on those revisions.
  - It refuses CodeAnchor descriptors, which hold hashes, not text.
  - It prints the §2.3 limits.
- **The kernel.**
  - No effect on derivation: topology, base, heads, rules, counters,
    statuses and digests are unchanged (T59).
  - The input layer records the shredded set.
  - A later op naming a shredded hash is `ContentShredded{content}`.
- **Reads.** `Payload::Unavailable(Shredded)`, checked against the shred
  set **before** the blob store. `ctx_read` returns the status,
  `payload: null` and `unavailable: "shredded"`.

### 8.4 Purge (M4)

After the commit:
1. **The key log** is rewritten (§8.2).
2. **The projections** are purged: `recall.sqlite`, `vectors.sqlite`,
   `jobs.sqlite`, `candidates.sqlite`.
   - Each projection opens with `PRAGMA secure_delete = ON`, from P9 on.
   - The purge rewrites or deletes the affected rows: recall rows are
     reindexed with an empty body, abstract and outline; vectors are
     deleted; jobs and pairs that need the body become
     `Skipped{shredded}`, and their stored validator output is deleted.
   - A validator or judge call in flight at shred time may return output
     quoting the body. The worker re-checks the shred set at its token
     fence, before committing, and drops such output (`Skipped{shredded}`).
   - Then: FTS5 `optimize`, `wal_checkpoint(TRUNCATE)`, and `fsync`.
3. **Durable progress.** Each projection records the last shred epoch it
   purged (`purged_through`). The key log records it too, in the header of
   each generation it writes. On open, every shred above that is purged
   again: derived from the op_log, idempotent, safe after any crash
   (T103).
   - **Completion.** A shred's purge is complete when the key log has
     covered its epoch, including the final `sync_dir` of §8.2, and every
     projection's `purged_through` covers it, synced. Only then does
     `ctx shred` report `purged`. Until then it reports `committed,
     purge pending`, and `ctx status` and `doctor` list the pending
     epochs. G-SHRED is stated from completion, not from the commit.
4. **The blob store.** `stage_blobs` refuses a shredded hash, so
   re-sending the bytes cannot store a new copy. The old pack record stays
   on disk as unreadable ciphertext, and compaction may drop it later
   (P10).
5. **The mirror.** An export of a ref containing the content is
   `MirrorStale{PayloadGone}`. `ctx mirror export --rebuild` writes a
   fresh history and deletes the old ref and every
   `refs/contextdb-backup/` ref. Git keeps the objects in its object
   store and reflogs until `git reflog expire --expire=now --all && git gc
   --prune=now`. ContextDB does not run that, because it would rewrite the
   user's repository; `--rebuild` prints the command.

### 8.5 Packs written before P9

`ctx migrate` 4 → 5 (§10) re-encrypts every plaintext record into new
segments with keys, syncs them, verifies every hash, and then deletes the
old segments. It needs free space for one copy of the packs, and checks
that before starting. It also rebuilds every body-bearing projection
(`recall.sqlite`, `vectors.sqlite`, `jobs.sqlite`, `candidates.sqlite`):
a fresh file with `secure_delete` on, renamed into place, with the old
file and its `-wal` deleted, so free pages from before P9 hold no
plaintext. A format-4 store cannot be opened by a P9 binary
without migration (`FormatMigrationRequired`), so every P9 store is fully
encrypted.

### 8.6 Outside G-SHRED

- The session log: handoff summaries, the supersede `reason`.
- The outbox: tool facts, commands, paths.
- Text that agents copied into other records.
- `meta` and note blobs, unless `--with-reviews` is given.
- Anything sent out earlier.

These hold no record bodies by design, but they may quote them. Deleting
session-log segments would break its hash chain and the ledger, so there
is no supported way to remove them in P9; `ctx shred` says so. Session-log
compaction is a P10 item.

### 8.7 The plaintext `content_hash`

Holders of the op_log, the snapshot or the projections can confirm a
guessed text: the store owner, and its copies. Through the socket, G-POSSESS
closes the oracle. A keyed hash would change every content address, and
stays out of scope until a deployment needs it (SPEC "需要时").

## 9. Signed log head

Deferred (F4a). The unkeyed hash chain and `doctor` stay. SPEC §10's row
says "optional signature: deferred, ADR 0017 §9".

## 10. Formats, protocol and migration

- **op_log format 5.** It adds `SetAccess` and `ShredContent`, with
  canonical op tags 20 and 21 (format 4 ended at 19). No older op changes,
  and frame checksums exclude the header. Implementation note (milestone 2): format
  5 and `SetAccess` land first, with today's header-only `ctx migrate`;
  the completion procedure below replaces it in milestone 6, whose open
  check and rerun must also handle a format-5 store without the marker
  that a milestone-2 build migrated (gate P9).
- **`ctx migrate` 4 → 5** (offline, store owner). The log's format alone
  does not mean the migration is finished: the header switch is the first
  step, and the packs, projections and modes follow it. Today's `migrate`
  returns success as soon as the log is at the current format
  (`migrate.rs`); for 4 → 5 that check is replaced by the completion rule
  below. `ctx migrate` holds the store lock throughout.
  0. **Pre-checks, read-only.** Lane names (§4.2) and free space (§8.5).
     A failure here changes nothing.
  1. **The marker.** It writes `<data>/MIGRATING-5`, syncs it and syncs
     `<data>`. From here until step 7 the store is *mid-migration*.
  2. **The log headers**, through the existing `log.migrating` swap. The
     frame checksums are unchanged, so certificates stay valid. Older
     binaries refuse format 5 from this point, so none of them ever opens
     a store whose packs have changed.
  3. **The packs** (§8.5): every plaintext segment is re-encrypted into a
     new segment, synced with its keys and checked against every hash;
     only then is the old segment unlinked and the directory synced. A segment's header says whether it is
     encrypted, so a rerun redoes only the segments still in plaintext.
  4. **The projections** (§8.5): each body-bearing projection is rebuilt
     into a fresh file, renamed into place, and its old file and `-wal`
     unlinked, then the directory synced. The fresh file records
     `rebuilt_at_format = 5`; a rerun rebuilds any projection without it.
  5. **The modes and ACLs** (§3.2), applied to every entry.
  6. **Verification**, against the disk rather than the marker: `log/`
     is present at format 5, with no `log.migrating` and no swap in
     progress (`migration_incomplete` false); every
     pack segment is encrypted, every content referenced by the op_log
     has a key and decrypts to its hash, every body-bearing projection has
     `rebuilt_at_format = 5`, no `.tmp`, old segment or old projection file
     remains, and §3.2's ownership and ACL check passes.
  7. **Completion.** Only after step 6 passes is `MIGRATING-5` unlinked
     and `<data>` synced. `ctx migrate` reports success here, and nowhere
     earlier.

  **Opening a mid-migration store.** The daemon, `ctx` and `doctor` refuse
  a store with `MIGRATING-5` present (`MigrationIncomplete{step}`, where
  `step` is the first step whose check fails), and print "run `ctx
  migrate` again". They do not try to finish the migration themselves. A
  format-5 store without the marker is still checked at open, cheaply:
  a plaintext segment header or a projection without `rebuilt_at_format
  = 5` is `MigrationIncomplete` too, so a lost or deleted marker does not
  let plaintext through.

  **Rerunning.** `ctx migrate` on a store with the marker resumes, and
  decides step 2 from the log itself, never from the marker alone:
  - a swap in progress (`log/` missing, `log.migrating` or the backup
    present): the swap is finished, as today;
  - `log/` at format 4 with no swap in progress (a crash after step 1 and
    before step 2 began, or after a discarded staging copy): step 2 runs
    from the start;
  - `log/` at format 5 with no swap in progress: step 2 is skipped.

  Then it runs steps 3 to 7, each of which redoes only what its own check
  finds missing. Step 6 checks the log again, so a run that somehow skipped
  step 2 cannot report success. A P8 binary may open a marked store still
  at format 4 (it ignores the marker); the packs are untouched until step 2,
  so this is safe, and the next rerun starts step 2 from the log it finds. On a
  store without the marker that passes the step 6 checks, it reports
  "already at format 5" and changes nothing. T104 covers every
  intermediate state.
- **Projections.** Schemas are unchanged, apart from new states in
  `jobs.sqlite` and `candidates.sqlite` (`Skipped{egress | shredded}`).
  `purged_through` is added to each. The key log is new.
- **Protocol 7.** It is required: the daemon accepts exactly its own
  version, as today, so clients upgrade together (M8). It adds:
  - the redacted shapes, `redacted: count`, `vector_size`;
  - the errors `Unauthorized`, `InvalidAccess`, `ContentShredded`,
    `AccessRevoked`, `SessionOfAnotherPrincipal`, `MergeUnauthorized`,
    `BranchOfAnotherPrincipal`, `MigrationIncomplete`, and the client-side
    `ServerNotOwner`;
  - the requests `SetAccess`, `AccessCheck` and `Shred`;
  - `handoff_begin{to}` and `ctx ui` tokens.
- **The session log** stays format 4. Implementation note (milestone 1): an addressed `HandoffBegin` (sender or recipient other than the store owner) is a new event tag 23; unaddressed ones keep tag 20's bytes (gate P9).

## 11. Verification plan

| Gate | Items |
|---|---|
| G0 | this ADR accepted after independent review; SPEC amendments (§12); no SPEC §3.5 mechanism introduced (the key log is classified under #7 as blob store) |
| G1 | fmt, clippy `-D warnings`, the workspace tests, `cargo deny` with the crypto crates |
| G2 | T17, T17b, T58, T59, T60, S13-rows-e2e (UNAVAILABLE rows), T98–T108 |
| G3 | PROP15 and PROP16 at 10,000 seeds; PROP14 per principal; PROP13 and T14 with the global ops |
| G4 | the security review against §2 and SPEC §10, item by item; IPC fuzz ≥ 24 h from a non-owner uid, including the AccessDoc parser and the `CXBE` / `CXKY` record parsers; kill points around shred, purge and migration; a full disk during a key-log rewrite |
| G5 | the budgets below |
| G6 | real two-user runs (F5) |
| G7 | the earlier phases' G2/G3/G4; the P8 suite (T106) |
| G8 | README (multi-user setup, the threat model, shred limits, backups); CHANGELOG; SPEC; independent implementation review |

New golden tests:
- **T98, uid mapping and the file system.** Mapped and unmapped uids;
  observer and admin refuse other uids; socket modes; file and directory
  modes, including `-wal` and `-shm`, and files written by `ctx migrate`,
  `ctx export`, `doctor` and the hooks (outbox); the ancestor ownership rule (a planted directory under a world-writable,
  non-sticky ancestor is refused); a client refusing a server whose peer
  uid differs from the `endpoint` owner; the service log created 0600;
  the start-up mode check, including a file with an ACL entry, a
  directory with an inheritable ACE, and an entry owned by another uid
  (each refused; `--fix-modes` strips the ACLs and refuses the foreign
  owner); new files carry no ACL even under an inheriting directory; the ancestor check; discovery from
  `--data`. A peer-uid injection hook covers the logic (debug builds); G6
  covers the real path traversal.
- **T99, sessions and handoff.** Binding another principal's session by id, call id or harness key is
  refused. Every request `ctx-mcp` sends for its tools is on the non-owner
  allowlist (a check over the MCP client's calls). Handoff is addressed: listing,
  accept, `dropped_unauthorized`, and the acceptor's verdict.
- **T100, no hidden names anywhere.** Every response type is listed in
  §5. For each one, a hidden logical touched in the same epoch appears
  only as a count where §2.3 allows a count, and never as a key, id,
  revision or rule. This covers commit results, WhatIf, Diff, changes,
  merge reports, certificates and errors.
- **T101, revocation.** Certificates and ledger reads issued before a
  revocation validate `Denied` after it. A removed uid gets
  `AccessRevoked`.
- **T102, writes.** Every rule in §6.2 is checked. `what_if` and
  `ImpactBatch` refuse exactly what the commit refuses. `Unauthorized`
  never depends on whether a revision exists. Observer exemption.
- **T103, shred.** Kill points after the commit, before and after the
  key-log rename, between the rename and the unlinks, between the unlinks
  and the final `sync_dir`, and during each projection purge; a simulated
  power cut on `SimDisk` at each of these points; an in-flight validator
  call whose output quotes the body.
  - **Before recovery.** After each power cut, the durable `SimDisk`
    state is inspected without opening the store. Once the purge has
    reported completion, no file in `keys/` holds the shredded key, and
    a power cut at that point leaves none either. Before completion,
    `ctx shred` has not reported `purged`.
  - **After recovery.** On restart the key is gone
  and every projection is purged. `strings` over the data directory,
  including `-wal` files and free pages, finds no shredded text.
  Re-sending the bytes is refused. The mirror reports `PayloadGone`, and
  `--rebuild` removes the old refs.
- **T104, migration.** Payloads are readable with equal hashes, frame
  checksums are unchanged, and certificates stay valid. No plaintext
  segment remains, and the rebuilt projections hold no pre-P9 plaintext in
  free pages or `-wal` files (`strings`).
  - **Intermediate states.** A kill, and a simulated power cut on
    `SimDisk`, before and after each of steps 1 to 7 of §10 and inside
    steps 3 and 4 (between segments and between projections). After
    each:
    - the P9 daemon, `ctx` and `doctor` refuse with `MigrationIncomplete`
      and the right `step`;
    - a P8 binary opens the untouched store before step 2 and refuses
      format 5 after it;
    - rerunning `ctx migrate` completes, and the result equals that of an
      uninterrupted migration: the same payload hashes, projections that
      answer the same queries, and the same modes.
  - **Marker durable, step 2 not begun.** A fixed case: `MIGRATING-5`
    synced and `log/` still at format 4 with no `log.migrating`. The P9
    daemon refuses with `MigrationIncomplete{step: 2}`; the rerun upgrades
    the log and completes; step 6 refuses success if the log is left at
    format 4 (checked by a fault that skips step 2).
  - **No success from the log version.** A store at format 5 with the
    marker, and one without the marker but holding a plaintext segment or
    an unrebuilt projection, are both refused at open, and `ctx migrate`
    does not report success on either until step 6 passes.
  - **Idempotence.** A second `ctx migrate` on a completed store changes
    nothing.
- **T105, egress.** Validators (local and not), the judge, the embedder
  and the mirror never receive a non-egress body, checked with recording
  mocks. `Skipped{egress}` jobs re-queue when egress turns on.
- **T106, single-user compatibility.** Without an access document the P8
  suite passes. Responses are semantically equal to P8 for the store
  owner (protocol 7 framing aside).
- **T107, proof of possession.** For a non-owner, a known hash without its
  bytes and an unknown hash both get the same `MissingBlob`, through
  every content-hash field (`content`, `note`, `payload_hash`) and in dry
  runs as in commits. Shredded bytes give `ContentShredded` (F13); a
  shredded hash without bytes gives `MissingBlob`. A hash
  attached to a readable logical works without bytes.
- **T108, branches and merge.** One case per row of §6.2.7, into `main`:
  a non-owner merge carrying a base change in a lane it does not curate,
  an artifact on a hidden target, a rule with a hidden premise, or a head
  in a lane it cannot write, or a change to the target's policy, is
  `MergeUnauthorized`; a merge carrying verdicts or new anchors applies,
  and the transported verdicts keep their exact `(rule, curvec)` and
  `validator`, the anchors stay not base and `unobserved`; a merge whose
  S-side policy equals the target's applies; the same merges by the store
  owner apply. Catching up: a non-owner merges
  `main` (with other principals' personal records, a policy change,
  verdicts and anchors) into its own branch. Archive by a non-creator is refused,
  auto-binding skips squatted names, and commits on another principal's
  branch are `BranchOfAnotherPrincipal`. A merge with an unauthorised change
  gives `MergeUnauthorized{count}`, naming nothing. The store owner can
  merge anything.

Existing items:
- T17: read and recall for unauthorised principals.
- T17b: WHY redaction.
- T58: forgery through every request field and every binding.
- T59: shred keeps topology and status, and WHY leaks nothing.
- T60: cross-lane references.
- S13-rows-e2e: UNAVAILABLE, now reachable end to end.

Properties:
- **PROP15, authorisation.** Random `generate_p7` histories, with random
  access documents: 2–5 principals, groups, shared and personal lanes,
  dir grants, denies, `curators = *writers`, `code_lanes`, and revocations and
  `SetMetadata` in the middle. It checks that:
  - `Authorized` equals an independent decision-table reference in
    `contextdb-oracle`;
  - for every response type of §5, `redact` leaves no hidden key, id,
    revision, rule or witness, and the counts equal the reference;
  - the derived snapshot and digests are identical whatever the
    principal;
  - recall per principal returns `min(limit, |authorised eligible|)`
    items;
  - write refusals equal the reference, including merges;
  - principal ids are never in the reserved ranges.
- **PROP16, shred.** Random histories with shreds. The generator never
  re-writes a shredded content. Comparing against the same history with
  the shred batches removed, with epochs renumbered:
  - topology, statuses and digests modulo the renumbering are equal;
  - payloads are `Shredded` for exactly the shredded contents;
  - replay and `doctor` agree.

G5 budgets (initial, ADR-calibrated):
- **Recall p99** at 10⁴ and 10⁵ records, within +10 % of P8. The access
  document has 5 principals, 10 lanes and 50 denies. Measured for a
  principal who may read every row and one who may read about 1 %, where
  skipping dominates.
- **Commit p99** with encrypted blobs and key-log syncs, within +10 % of
  P8.
- **Read p99** of a 4 KiB payload, within +0.5 ms.
- **Shred of one content** (commit, key-log rewrite, purge of every
  projection) at 10⁵ records, under 5 s.
- **Reported:** `ctx migrate` on 1 GiB of packs; key-log load at open.

G6 (F5), on macOS with a second local user (and on Linux if possible):
- The owner's store has a shared lane, a billing lane the second user
  belongs to with a dir grant, a deny, and personal lanes.
- The second user runs a real agent (Claude Code or Codex) through MCP
  for a scripted session, including records with anchor deps (`code_lanes`). Every response is logged and checked against
  the reference.
- The second user then tries, and each must fail:
    - reading the data files directly (`EACCES`), including after the owner
    has added an inheritable ACE granting the second user read on
    `<data>` (the daemon must refuse to start, and files it created
    before that point carry no inherited entry);
  - the admin and observer sockets;
  - forged fields, and another session's id;
  - a known content hash without its bytes;
    - the owner's `ctx ui` without its token;
  - planting a directory with its own daemon on the shared path (refused
    by the ancestor rule; a client pointed at it refuses the server).
- A shred is then checked with `strings` over the data directory.

## 12. SPEC amendments (before implementation)

- **§5 P9:** the scope of §0; the test list T98–T108 and PROP15/16; F8's
  MCP-only rule for non-owner principals.
- **SD11:** `Authorized` per §4.4, with lane entries per §4.3 (owner first; branch-dependent through
  `dirs`; not transitive).
- **SD13:** visibility computed when a response is rendered;
  `Dimensions.authorized` constant in the snapshot.
- **SD15:** the redaction shapes of §5.2–§5.5.
- **SD19:** two more global, non-merged, sole, admin-only ops:
  `SetAccess`, `ShredContent`. Branch creators (F11).
- **§6.1:**
  - the ops;
  - input contract 8 extended to `AccessDoc`;
  - the reserved principal ids;
  - the errors;
  - proof of possession.
- **§6.2:** the placement of `Unauthorized` and `ContentShredded`.
- **§6.3:** the item shapes, certificate responses without vectors, and
  revocation.
- **§7 and §3.5 #7:** the key log as part of the blob store; the `CXBE`
  record; `purged_through`.
- **§8.1:** T13 amended to per-principal stability.
- **§10:**
  - same-uid processes are the store owner;
  - side channels and counts;
  - DoS out of scope;
  - signature deferred;
  - "P9 前不支持 erasure" replaced by shred and its limits.
- **Appendix C row.**

## 13. Milestones

1. **Principals and transport.** Reserved ids, the uid mapping, the
   directory layout and modes, `umask`, discovery, per-principal sessions,
   addressed handoff, `AccessRevoked`, the request allowlist (T58, T98,
   T99).
2. **The access document.** `SetAccess`, op_log format 5 (ops only),
   `ctx access`, `Authorized` and its oracle reference.
3. **Reads.** The single redaction point; items, recall, WHY / WHY_NOT /
   IMPACT, diffs, certificates, errors, feeds; UI tokens (T17, T17b,
   T100, T101, S13-rows-e2e; PROP15, read half).
4. **Writes.** Kernel checks and their order, proof of possession, branch
   creators, merge (T60, T102, T107, T108; PROP15, write half).
5. **Egress.** Validators (`local`), the embedder, the mirror (T105).
6. **Crypto-shred.** `CXBE`, the key log, `ShredContent`, purges and
   `purged_through`, the mirror rebuild, `ctx migrate` 4 → 5 (T59, T103,
   T104; PROP16).
7. **Verification.** G3–G8: the fuzz run, the security review, G5, G6 on
   macOS, T106.

## 14. Review history

| Round | Findings | Result |
|---|---|---|
| r0.1 review | 8 blockers, 9 majors, 9 minors; 6 new owner decisions | not acceptable |
| r0.2 (author) | see below | |
| r0.2 closure check | 19 closed, 7 partly; N1–N7 majors, N8–N19 minors; F13 | acceptable after text fixes |
| r0.3 (author) | N1–N19 answered (below) | |
| r0.3 check | N1–N19 closed (N10 after N20); every r0.1 item closed; N20 allowlist missing `SessionStart` / `Updates`, N21 proof of possession before the shred set, N22 key-log syncs through `Durability`: fixed in place | **G0 acceptable**, pending the owner's §0.1 decisions |

r0.2 dispositions:

| Finding | Change |
|---|---|
| B1 shared socket unreachable | `<data>` and `run` are 0711, every other entry is 0700/0600, the ancestor check runs at start, discovery from `--data` (§3.2); T98 and G6 test real traversal. |
| B2 unfiltered diff responses | §5.5 names every diff-shaped response: dropped, plus one count; `Diff` is bound; T100. |
| B3 `ctx ui` | F9: launch token and cookie, abstracts through the daemon (§5.8). |
| B4 dir grants widened by rules | Anchor paths are gone from `Authorized`, which uses `dirs` only (§4.3). Rules, `Rebind`, `SetRuleEnabled` and `SetMetadata` need read access to the target (§6.2). Branch dependence is stated, and T13 is amended. |
| B5 merge | F7: merge rule by classified change, `MergeUnauthorized{count}`, redacted reports (§6.2.7); T108. |
| B6 check order and error leaks | §6.1 places `Unauthorized` and `ContentShredded`. Refs to hidden logicals get one answer. §5.7 maps every error to the caller's own refs. `ExpectHeads` needs read access. |
| B7 id collision | 0–15 and the validator range are reserved; document ids start at 16 (§3.1); PROP15 checks it. |
| B8 dedup oracle | F10: proof of possession (§6.3); T107. |
| M1 hidden ids inside authorised items | §5.2 lists the redacted fields. |
| M2 certificates and handoff | No vectors to non-owners, redacted diffs (§5.6); addressed handoff, the acceptor's verdict (§3.4). |
| M3 key-store durability | An append-only key log in the packs' ordered syncs, rewritten by rename on shred, classified as blob store, with `doctor` checks (§8.2). |
| M4 incomplete purge | `secure_delete`, FTS5 `optimize`, WAL truncate, `purged_through` recovery, `stage_blobs` refusal, the shred set checked before the store, and the session log and outbox placed outside G-SHRED (§8.4, §8.6). |
| M5 SD19 | The SD19 amendment lists both global ops as sole, on `main`, not in a WHAT_IF (§4.1, §8.3, §12). |
| M6 hooks for non-owners | F8: MCP only, a closed allowlist (§3.3). |
| M7 egress and mirror | The mirror exports only fully readable egress lanes. `local` validators (F6). Egress is out of `Authorized`. Vectors only for egress heads. Re-queue (§7). |
| M8 protocol 6 | The claim is dropped: protocol 7 is required; T106 is semantic (§10). |
| M9 branch squatting | F11 (§6.2.6); T108. |
| m1 §1 accuracy | Corrected: observer `ExpectHeads`, ids in status types, the stored `authorized`, the extra plaintext copies. |
| m2 accepted leaks | §2.3 lists them. |
| m3 DoS and fuzzing | §2.3; G4 fuzz from a non-owner uid with the new parsers. |
| m4 PROP16 | Compared modulo epoch renumbering; the generator never re-writes shredded content. |
| m5 migration | `log.migrating` swap, a free-space check, lane names; the "refuses until migrated" sentence is removed. |
| m6 file modes | `umask(077)` (§3.2). |
| m7 `default_lane` | Defaults to `*owner` (fail closed). |
| m8 reuse | Documented (§2.3). |
| m9 text fixes | `ExpectHeads` order; F3's rationale is cost; CodeAnchor shred refused; the 1 % recall budget; macOS G6. |

r0.2 closure check (2026-11-06): **acceptable after text fixes**. Of r0.1's
findings, 19 were closed and 7 partly closed (B3, B8, M3, M4, M9, m2, m6).
The check raised 7 new majors (N1–N7) and 12 minors (N8–N19), and one new
owner decision (F13).

r0.3 dispositions:

| Finding | Change |
|---|---|
| N1 modes of files other processes write | The store layer creates with explicit modes; `umask(077)` in every binary; the start-up mode check; `doctor --fix-modes` (§3.2); T98. |
| N2 `ContentShredded` as an oracle | F13: the bytes form is accepted and documented (§2.3). Non-owner dry runs need bytes too, so a hash alone answers as in a commit (§6.3); T107. |
| N3 key-log race and recovery | The rewrite runs on the writer thread, through `.tmp`, rename and directory sync. It is redone on open while any shredded key remains. Keys load from the newest generation; for a hash with several records, the last wins (§8.2); T103. |
| N4 cookie shared across ports | F9: `Authorization` header from `sessionStorage`, `Origin` check, no cookie, record links as text (§0.1, §5.8). |
| N5 anchors locked out | `code_lanes` default, readable by members, written only by the store owner (§4.1, §4.3); PROP15, G6. |
| N6 pre-P9 free pages | `ctx migrate` rebuilds the body-bearing projections (§8.5, §10); T104. |
| N7 other hash fields | Proof of possession covers every content-hash field (§6.3); T107. |
| N8 coverage from empty `dirs` | Stated: writers decide (§4.3). |
| N9 merge wording, branch planting | "Changes the merge applies"; commits on a branch are open to its creator and the store owner only (§6.2); T108. |
| N10 allowlist and `Status` | The allowlist is completed; `Status` is store-owner only (§3.3); removed from §2.3. |
| N11 `run/shared` mode, groups, home | G-FS lists the public entries; the ancestor check counts group traversal; the README recommends a data directory outside home (§2.2, §3.2). |
| N12 `TxnStatus` | Own rows only (§5.5). |
| N13 counts | `vector_size` and `MergeUnauthorized{count}` are added to §2.3. |
| N14 `Inherited(Redacted)` | Dropped: inherited reasons are copied values, and only their pinned-premise id is redacted (§5.2). |
| N15 session-log advice | Dropped; session-log compaction is a P10 item (§8.6). |
| N16 in-flight validator output | Re-checked at the token fence (§8.4); T103. |
| N17 deny wording | Fixed (§4.3). |
| N18 mirror target readability | Export refuses a group- or world-readable `.git` without `--readable-ok` (§7). |
| N19 owner recall hygiene | F12: no-scope recall on the owner's agent channel leaves out others' personal lanes (§0.1). |

Owner review of r0.3 (2026-11-06), three findings, fixed in place:

| Finding | Change |
|---|---|
| Merge permissions incomplete: only Read/Write, while a direct base change needs Curate and a policy change needs the store owner; merge also carries Policy (`kernel_merge.rs` classifies it first) and new objects | §6.2.7 rewritten as a per-class table: a merge may apply a change only if the merger could make it directly (Policy and ValidationRecords: store owner; base: curator; rules: read every premise; artifacts, new logicals, revisions, metadata as for the direct ops); dry-run parity; PROP15 and T108 cover every class. |
| macOS isolation by mode only: extended ACLs can grant access regardless of mode and are inherited by new files | §3.2 and G-FS: owner uid and the absence of any ACL are checked for every entry at start and after migrate; new entries are created with an empty ACL; ancestors' ACLs are read for traversal denies; T98 and G6 add ACL and foreign-owner cases. |
| Personal lane permissions incomplete: creation by the principal, updates by "lane writer" under `default_lane = *owner` | §4.3 `entry(lane)`: one lookup for every lane, with the synthesised `personal:<name>` entry (members = writers = curators = the principal); every rule in §4.4 and §6.2 uses it. |

Focused check of the owner-review fixes (2026-11-06): acceptable after text
fixes, plus one owner decision.

| Finding | Change |
|---|---|
| N23 non-owner merges made unusable: anchor row contradictory, verdicts and policy blocked, hidden records on `main` block every catch-up | F14: author-bound classes (Policy, ValidationRecords, CodeAnchor objects) are transported; merges into the merger's own branch transport everything; merges into `main` and owner branches keep the per-class direct-op rule (§6.2.7); "objects newly visible to the target"; the `AnchorBroken` clause says never carried; PROP15 and T108 updated. |
| N24 ancestor ownership and server authenticity | StrictModes-style ancestor rule; the recommended location is an owner-created directory; clients check the server's peer uid against `endpoint` and `<data>`'s owner (`ServerNotOwner`); T98 and G6. |
| N25 SQLite's own files and the service log | Stated: SQLite files are covered by the ACL-free-directory rule, which therefore refuses deny-only entries; `Umask 077` in the launchd plist and the systemd unit. |
| `[personal]` in §4.2 | Already listed among the known tables. |

Owner re-review of r0.3 (2026-11-06): two P1 gaps, so G0 stays open.

| Finding | Change |
|---|---|
| P1 key-log unlinks not durable: no `sync_dir` after deleting old generations, so a power cut after "purged" can restore an old generation and its key, and recovery on open does not protect a reader who reads the disk first | §8.2: unlink every older generation and `.tmp`, then sync the directory again; the key-log step completes only after that. §8.4: the key log records `purged_through`; a shred's purge is complete only when the key log and every projection cover its epoch, durably, and only then does `ctx shred` report `purged` (G-SHRED counts from completion). §2.3: freed blocks named. T103 adds the power-cut point between the unlinks and the final sync, and inspects the durable disk state before any recovery. |
| P1 migration completion undefined: format 5 is visible after the first step while plaintext may remain, the `log.migrating` swap covers the log only, and `migrate` treats the log version as success | §10: a durable `MIGRATING-5` marker written before the header switch and removed only after a disk-based verification (step 6); open refuses a marked store, and checks segment headers and projection markers on an unmarked format-5 store (`MigrationIncomplete{step}`); a rerun resumes and redoes only what each step's check finds missing; success is never inferred from the log version. T104 covers a kill and a power cut at every step boundary and inside steps 3 and 4, P8 and P9 binaries in each state, and equality with an uninterrupted run. |

Owner review of r0.4 (2026-11-06): F1–F13 confirmed as recommended (a);
F14 confirmed in a narrowed form; one P1.

| Finding | Change |
|---|---|
| P1 a rerun can skip the log upgrade: a crash after `MIGRATING-5` is durable and before step 2 begins leaves no swap to resume, and step 6 did not check the log | §10: a rerun decides step 2 from the log (swap in progress: finish it; format 4: run step 2; format 5 and no swap: skip); step 6 requires `log/` at format 5 with no swap in progress. T104 adds the fixed state "marker durable, step 2 not begun", and a fault that skips step 2 to show step 6 refuses success. |
| F14 too wide: a non-owner could carry a Policy change into `main` | Policy into `main` or an owner branch: the store owner only, when the target's policy actually changes. ValidationRecords keep their exact `(rule, curvec)` and original `validator` source; CodeAnchor objects stay not base and `unobserved`; everything else by its direct op. Into the merger's own branch every mergeable change is transported, reads through §4.4. §0.1, §6.2.7, PROP15 updated. |

Accepted 2026-11-06 by the owner ("确认，标为 accepted 并提交"), after the r0.5 fixes above.
