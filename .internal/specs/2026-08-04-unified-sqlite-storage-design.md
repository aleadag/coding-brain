# Unified SQLite Brain Storage Design

- Status: Approved
- Date: 2026-08-04
- Bead: `codexctl-2o9fo`
- Brainstorm session: `codexctl-od4lq`
- Related bug: `codexctl-4vh58`
- Revises: `docs/decisions/ADR-0003-fail-safe-hook-and-learning-persistence.md`

## Context

Coding Brain currently commits a model-derived permission decision across
`brain/decisions.jsonl`, permission authority embedded in
`hooks/lifecycle.json`, and terminal evidence in `activity.jsonl`. An immutable
file journal makes a crash between those writes recoverable. Response delivery
is necessarily separate because a provider response pipe cannot participate in
a filesystem transaction.

This design repaired proposal-only failure modes, but its idempotency and
recovery checks repeatedly read complete destination files. `codexctl-4vh58`
demonstrates the resulting failure: the unique destination data fits within the
configured evidence bound, but repeated reads exceed the cumulative 16 MiB
budget. A committed proposal and `Allowed` activity can then remain behind a
pending journal without delivery evidence, and recovery can prevent Brain from
projecting otherwise intact history.

The same global activity log is shared by providers and projects. Large reads,
append locks, tail repair, compaction, and cross-store verification therefore
compete with short-lived permission hooks. Continuing to harden those separate
JSON/JSONL stores would retain the cross-store transaction that causes the
complexity.

Operational review state is currently a separate `review-state.json`. It does
not confer permission or execution authority, but it benefits from the same
indexed, transactional storage and migration machinery.

## Goals

1. Make proposal, exact permission authority, and terminal activity one atomic
   SQLite commit.
2. Preserve the distinction between committed, delivery-unknown,
   delivery-failed, delivered, and outcome-confirmed evidence.
3. Keep permission-hook storage work bounded as history grows.
4. Preserve same-request single-winner admission and independent inference
   concurrency across processes.
5. Import the complete supported legacy storage set automatically and
   atomically without exposing a partial database.
6. Preserve legacy audit and downgrade compatibility through explicit,
   verified exports rather than live dual writes.
7. Move operational review state into SQLite without changing its authority or
   visibility-only semantics.
8. Preserve owner-only storage, fail-closed permission behavior, bounded
   corruption handling, and explicit operator recovery.

## Non-goals

- Do not migrate provider transcript JSONL files. Coding Brain does not own
  them.
- Do not migrate `session-links.jsonl`. It remains bounded identity evidence
  shared by `coding-brain-core` navigation, guarded actions, and recovery.
- Do not continuously mirror SQLite writes into legacy JSONL.
- Do not infer delivery, tool execution, or permission authority from missing
  events, transcripts, legacy proposals, or unresolved migration evidence.
- Do not add at-rest encryption. The confidentiality boundary remains
  owner-only local state.

## Chosen Approach

Use one relational SQLite database for authoritative Brain and lifecycle state,
plus a separate SQLite database for disposable operational review state. Use
typed and indexed columns for stable, query-critical fields. Retain bounded JSON
only for provider-specific or evolving payloads that do not determine
permission authority.

This is preferred over a single JSON event table because database constraints
should enforce the permission boundary rather than merely store the existing
serialization. It is preferred over full normalization because deeply nested
provider metadata changes independently and does not justify a large join
surface.

## Storage Boundary

The canonical files are:

```text
$XDG_STATE_HOME/coding-brain/db/brain.sqlite3
$XDG_STATE_HOME/coding-brain/db/review.sqlite3
```

with the existing fallback under `~/.local/state/coding-brain/db/`. The
dedicated database directory remains owner-only and contains only
SQLite-managed files. The databases, rollback journals, WALs, and shared-memory
files must be regular, single-link, current-user files with owner-only
permissions.

The binary Brain storage layer owns database creation, migrations, queries,
permission commits, activity appends, learning reads, review state, and
exports. Core activity and lifecycle types remain reusable data contracts.
`coding-brain-core` retains pure lifecycle types and projection logic, while
the binary store owns persistence for the complete lifecycle snapshot.
`SessionLinkStore` remains in `coding-brain-core` and stays JSONL-backed.

Use `rusqlite` only in the root binary crate, with bundled SQLite so Linux, musl
artifacts, and macOS do not depend on an ambient runtime SQLite library. Enable
foreign keys on every connection. Use WAL after canonical publication; use a
self-contained journal mode for migration staging databases and checkpoint them
fully before rename. Never `ATTACH` the Brain and review databases or imply
cross-database atomicity.

## Logical Schema

The implementation plan will spell out exact SQL, but the following Brain and
review tables and constraints are part of the design contract.

### `schema_meta`

One row records:

- application identifier;
- schema version;
- completed migration generation;
- legacy schema versions imported;
- migration completion timestamp.

The database is usable only when SQLite `application_id`, `user_version`, the
richer metadata, and the supported schema version agree and the migration
generation is complete. The metadata also stores a nondecreasing activity
high-water cursor and the frozen legacy export profile.

### `permission_attempts`

Each row represents one exact permission request. It stores a canonical,
validated request-identity key plus typed provider, session, provider-session,
turn, request-key, project, tool, activity ID, state, and timestamps.

The request-identity key includes the complete validated lifecycle identity
plus request key and is indexed but not permanently unique. Every invocation
has a unique attempt ID. The existing owner-only per-request OS advisory lock
selects one concurrently active winner; it stores no authority. A later
sequential invocation with identical request content may create another
attempt. Attempt states distinguish evaluation in progress,
native/needs-input disposition, committed decision, and legacy incomplete
evidence. An attempt state alone never grants executable authority.

The winner transaction also appends the initial `Observed` and `Evaluating`
activity rows. Model inference runs only after that transaction commits and
outside all database transactions. A stale evaluation remains a projection as
`Incomplete`; elapsed time does not serialize an execution outcome.

### `decision_identities` and `decision_payloads`

Each identity row has a unique decision ID and a closed identity kind.
Permission identities require the complete immutable correlation and authority
facts used by commit foreign keys. Non-authoritative observation identities
retain their real provider and timestamp but require permission authority
fields to be null, so compatibility never depends on invented session, turn,
tool-use, or action evidence. Each optional payload row has the same closed
kind through a composite foreign key and contains the typed indexed learning
fields plus one complete validated payload bounded to the final legacy decision
record limit. This includes hook proposals and the existing non-hook learning
records carried by `decisions.jsonl`.

Proposal existence does not grant permission authority and does not prove that
the provider received a response or executed a tool. `forget()` deletes
payloads and learning marks while retaining the minimal identities required by
immutable permission and activity audit relationships.

### `permission_commits`

Each row is an immutable authoritative permission commit. It has a one-to-one
relationship with a permission attempt and references exactly one decision
identity and one terminal activity event. It stores the immutable `Allow` or
`Deny` action, the exact authority identity, commit timestamp, transaction
identifier, evidence basis, and whether the current invocation is eligible to
emit a response.

Foreign keys and uniqueness constraints require the attempt identity,
proposal, authority action, decision ID, activity ID, and terminal state to
agree. A model-derived allow or deny exists only if this row and all referenced
rows commit in the same SQLite transaction.

### `activity_events`

This is the append-ordered audit ledger. A strictly increasing 64-bit integer is
the stable source cursor. Rows contain typed event kind, an indexed logical activity
ID, recorded time, provider/session/turn/tool-use identity, project, state,
decision ID, outcome, correction, supersession, and bounded payload fields. The
logical activity ID is intentionally non-unique because one activity can have
observed, terminal, delivery, outcome, and correction rows. Permission terminal
rows retain a unique activity-plus-authority identity tuple for commit references.

Cursor allocation and event insertion share one transaction. The high-water
value never decreases or reuses a cursor after retention, rebuild, or restore;
overflow fails closed. Wall-clock time is presentation evidence, not an
authority or ordering substitute.

Indexes cover:

- descending source cursor and recorded time;
- activity ID and decision ID;
- provider/session/turn/tool-use identity;
- project identity;
- state and event kind;
- outcome and correction joins used by projections.

Permission terminal events referenced from `permission_commits` are immutable.
Later delivery, outcome, correction, lifecycle, and diagnostic rows remain
append-only evidence.

### Lifecycle tables

The Brain database stores the complete current lifecycle snapshot: sessions,
turn sequence and lease state, permission disposition/authority, active
subagent topology, provider invocation state, and the bounded recent continuity
required by reconciliation. Core applies pure lifecycle transitions; the
binary store loads and persists their relational representation. No live
permission or topology authority remains in `hooks/lifecycle.json`.

### Review database: `review_meta` and `review_marks`

Review metadata stores an independent optimistic revision and source
high-water for each surface. Its nullable `last_archive_revision` is either
`1..=revision` or null and is always null for Recent. A nonempty archive batch
records its new revision; undo restores only rows at that exact revision and
clears the slot, so an older archived batch cannot become undoable again.
Pruning clears the slot when no exact rows from the remembered batch remain.
Review marks key the surface plus stable group identity and source cursor to a
visibility disposition. They are joined before grouping, counts, ordering,
overflow, and truncation.

The review database contains no permission, activity, decision, or lifecycle
tables. A review mutation validates an exact bounded group/cursor against a
short Brain snapshot, then commits with an optimistic review revision. New
activity naturally receives a greater cursor and resurfaces. Review reset
replaces only `review.sqlite3` through the explicit
`cbrain storage reset-review-state` operation. Every Review connection holds a
shared owner-only reset gate for its lifetime. Reset acquires the same gate
exclusively before SQLite access, returns Busy while a connection remains
alive, and holds it through validated deletion and direct recreation.

## Permission Data Flow

### Admission and inference

1. Parse and validate the provider request and exact lifecycle identity.
2. Run deterministic safety classification before model inference.
3. Acquire the exact-request advisory lock before persistent mutation,
   deterministic-deny auditing, or model inference. A concurrent contender
   performs no mutation or inference and emits no model-derived response.
4. Open a bounded database connection and begin a short admission transaction.
5. Insert the unique attempt ID, indexed request identity, and initial activity
   events.
6. Commit admission, close the transaction, then run inference.

Independent requests may infer concurrently. SQLite serializes only the short
admission and commit writes.

### Atomic permission commit

For a model-derived allow or deny, one `BEGIN IMMEDIATE` transaction:

1. revalidates the exact attempt and its winner state;
2. inserts the proposal;
3. inserts the terminal `Allowed` or `Denied` activity;
4. inserts the matching immutable `permission_commits` authority row;
5. verifies affected-row counts and relational invariants; and
6. commits.

No filesystem transaction journal is needed. A crash before commit leaves no
authority. A crash after commit leaves one complete committed permission with
delivery unknown. A failed or uncertain SQLite commit never permits the current
hook to emit a model-derived response. A later fresh connection determines
whether the complete transaction exists; recovery never turns that observation
into response replay.

### Response delivery

Only after the atomic commit may the hook serialize and write the provider
response. The response pipe remains outside SQLite:

1. commit permission evidence;
2. write and flush stdout;
3. append `Delivered` or `DeliveryFailed` in a second short transaction.

A crash or uncertain flush after commit leaves `DeliveryUnknown`. A stdout
error records `DeliveryFailed` best-effort. Successful stdout followed by a
failed delivery-event transaction remains `DeliveryUnknown`, because the
provider may have received the response. Recovery never replays stdout. Only
later exact lifecycle or outcome evidence may claim tool execution.

Deterministic safety denies continue to fail closed when persistence is
unavailable. Their audit failure is reported through a bounded diagnostic, but
storage failure never turns a deterministic deny into native/model approval.

## Query and Maintenance Bounds

All interactive and hook queries use exact keys or indexed cursors plus
explicit row and serialized-byte limits. Live, Review, Scorecard, Diagnostics,
distillation, outcome correlation, and Doctor must not rely on unbounded table
scans. Bounded activity pages expose an exclusive continuation cursor only when
lookahead proves another matching row remains; logical activity-ID queries can
continue from that cursor without conflating truncation and end-of-data.

The implementation must inspect critical query plans and reject accidental
full scans in regression tests. Permission commit cost must not grow with total
decision or activity history.

Use WAL with `synchronous=FULL`. Every hook has one monotonic storage deadline
shared by connection opening, busy retries, admission, commit, and delivery
evidence; no operation resets it. A fixed page-based auto-checkpoint threshold
serves ordinary headless operation. Checkpoint failure does not undo an already
committed WAL transaction.

Track WAL size. Doctor and the TUI warn at a tested degradation threshold; at a
conservative hard threshold, model inference pauses until bounded non-hook
maintenance checkpoints it. Deterministic denies remain available. Hook
processes never run `VACUUM`, bulk retention, full integrity checks, or
unbounded manual checkpoints. Non-hook maintenance performs bounded
checkpointing, incremental vacuum, retention, and compaction. Retention
preserves all fresh incomplete lifecycles, bounded interrupted history, current
review/correction semantics, and all activity after the cursor stored in the
last successfully published immutable preference generation. It cannot delete
evidence needed for permission authority or unprocessed learning.

## Automatic Legacy Migration

### Trigger boundary

Automatic migration runs only from a non-hook `cbrain` startup path, including
the TUI and Doctor. Permission and lifecycle hook entrypoints never perform a
legacy import. If legacy state exists but `db/brain.sqlite3` does not, hooks return
promptly to native provider handling with a bounded migration-required
diagnostic and perform no model inference. Hooks also do not initialize a fresh
schema; `cbrain init` or another non-hook startup publishes an empty database on
a new installation.

While legacy-only state or a migration lock is present, new-version hooks do not
mutate legacy activity, decision, permission, or review stores. Lifecycle hooks
return their provider-neutral response without recording new legacy audit
evidence. The migrator fingerprints every source before and after streaming and
aborts publication if any source changed, covering a concurrently running older
hook binary.

### Inputs

The migrator recognizes and validates the complete supported legacy set:

- `brain/decisions.jsonl`;
- `activity.jsonl`;
- the complete `hooks/lifecycle.json` snapshot;
- `brain/permission-transactions/` journals;
- `review-state.json`.

`session-links.jsonl` is not imported. Legacy file contents remain unchanged
after success, but the migrated mutable stores are frozen read-only at cutover
so an older process cannot silently create a second live history.

### Staging and publication

One owner-only migration lock selects the migrator. It creates unique,
owner-only, same-directory staging databases such as:

```text
.brain.sqlite3.migrate-<random>
.review.sqlite3.migrate-<random>
```

The file is not created under `/tmp`. Legacy rows are streamed with established
per-record and total-size bounds. The migrator imports in source order and does
not hold entire logs in memory.

Before publication it:

1. validates every imported critical record;
2. runs foreign-key and SQLite integrity checks;
3. verifies source/import counts and stable-order cursors;
4. verifies permission, lifecycle, and review-state invariants;
5. checkpoints and closes the staging databases;
6. acquires every legacy store lock in one fixed order and revalidates source
   fingerprints;
7. flushes the databases and containing directory;
8. atomically publishes `brain.sqlite3` with its migration generation still
   incomplete, independently publishes `review.sqlite3`, freezes the migrated
   legacy stores read-only, then marks the Brain generation complete before
   releasing their locks.

The canonical Brain filename is absent until all authority checks pass, and
hooks reject a published database whose migration generation is incomplete.
Brain publication does not depend on review publication: failed review migration
preserves `review-state.json` and reports review unavailable rather than
discarding marks or blocking permission handling. A crash leaves staging
artifacts noncanonical; explicit migration state makes cutover restartable. A
later migrator may remove or replace an artifact only after validating its
exact bounded name, regular-file type, ownership, mode, link count, and
location. Any recreated or mutated legacy path after cutover is a split-brain
error that disables model-derived responses and is never auto-merged.

### Legacy permission reconciliation

A legacy proposal and exact matching terminal `Allowed` or `Denied` activity
become one historical permission commit under ADR-0003's existing contract that
activity is the authoritative decision-commit audit. A matching validated
journal or still-retained lifecycle authority corroborates the import but is not
required after lifecycle compaction. Migrated commits record their evidence
basis and `response_eligible = false`, so no imported row can emit or replay a
provider response. Proposal-only or nonterminal activity becomes an incomplete
attempt without authority.

Pending journal evidence is reconciled inside the staging database. This
preserves the `codexctl-4vh58` proposal, `Allowed`, delivery-unknown, and later
outcome relationship without deleting the source journal, inventing delivery,
replaying a response, or blocking projection of unrelated coherent activity.
Mismatched or conflicting journal/activity evidence remains incomplete or
diagnostic and creates no commit.

Malformed, conflicting, oversized, unsupported, or newer-schema critical
evidence aborts migration. Non-authoritative malformed audit rows retain the
existing bounded integrity-diagnostic treatment when they can be skipped
without changing permission or learning authority.

## Export and Rollback

SQLite is the only live writer after publication.

`cbrain storage export-audit <directory>` streams decisions and activity in
stable source-cursor order using the established JSONL audit schemas and
redaction limits. It does not expose review state or private internal authority
beyond the established audit contract.

`cbrain storage export-legacy <directory>` reconstructs one named, immutable
compatibility profile for the final pre-SQLite layout. The profile is currently
`legacy-v0.59.1` and changes only if another JSONL release ships before this
feature. It includes lifecycle state and `review-state.json`. Export writes a
new owner-only directory and refuses to overwrite existing targets. It verifies
the result with frozen readers/fixtures for that exact profile and rejects
SQLite evidence that cannot be represented losslessly, including
delivery-unknown state.

The legacy profile remains supported for at least one complete release cycle.
Removing it requires a separate ADR and release note. Audit export is a
versioned archival format and is never presented as executable rollback state.

Downgrade procedure:

1. stop all Coding Brain processes;
2. create and verify a legacy export;
3. back up the complete state root;
4. install the exported legacy layout as documented; and
5. run the older binary.

Normal startup never rereads or dual-writes legacy files after SQLite is
published. Keeping the original migration inputs is a rollback aid, not a live
compatibility protocol.

## Busy, Corruption, and Security Behavior

Every process validates the state directory and dedicated database directory
before open. Existing database, WAL, shared-memory, journal, and migration paths
must not be symlinks, directories, multi-link files, foreign-owned files, or
broader than the owner-only mode contract. Use `SQLITE_OPEN_NOFOLLOW` for the
main database and revalidate identity where the API permits it.

SQLite creates VFS-managed sidecars whose complete set is not an application
contract. The supported threat boundary therefore rejects other-user access and
pre-existing unsafe paths but does not claim containment against a concurrently
malicious process with the same UID. A custom SQLite VFS is out of scope. The
database requires a local filesystem with working WAL and lock semantics;
unsupported or network-like behavior fails closed.

Disable extension loading and URI-controlled open options. Enable foreign keys,
defensive mode, `trusted_schema=OFF`, explicit SQLite resource limits,
memory-backed temporary tables, and secure deletion. SQL statements are static;
stored content is always bound data.

Permission hooks share one total storage-wait budget rather than resetting a
timeout for every statement or retry:

- busy before admission means no inference;
- busy or I/O failure during a model-derived commit means no response;
- provider-native confirmation remains authoritative;
- deterministic code-owned denies still deny.

TUI reads use short snapshot transactions. Busy or transient I/O retains the
last coherent view and reports a distinct degraded status; it never replaces
the view with an empty projection.

`SQLITE_CORRUPT`, `SQLITE_NOTADB`, unsupported schema, failed critical
constraints, or unsafe paths disable model-derived permission responses.
Coding Brain never automatically deletes, recreates, or overwrites a published
database from stale legacy files.

Doctor performs bounded schema and foreign-key checks during normal diagnosis
and offers an explicit deeper integrity check. Repair, restore, or salvage is
an operator action against a verified backup/export. Uncertain evidence remains
preserved. Physical Brain database or schema corruption disables every Brain
database domain. Review database damage disables review operations without
changing permission authority or otherwise hiding coherent audit data.

Bounded JSON is validated before writes and after reads. Invalid authoritative
data fails the operation. A malformed non-authoritative audit payload becomes a
bounded integrity diagnostic only when its row boundary and typed identity are
still trustworthy; otherwise the affected read fails. It never becomes
executable evidence.

### Privacy erasure

`forget()` takes one global erasure gate and the established distillation lock
order. It transactionally removes decision payloads and learning/canonical
marks while retaining only minimal decision identities required by immutable
audit relationships. It removes every published preference generation and
pointer and securely erases matching legacy decision/preference snapshots.
Activity evidence remains retained under the existing contract.

After erasure, secure deletion is followed by a WAL checkpoint/truncation. Any
uncertain durable erasure is reported as failure. Downgrade export cannot
resurrect forgotten payloads.

### Disk exhaustion and I/O failure

`SQLITE_FULL` and relevant `SQLITE_IOERR` results preserve operation-specific
semantics:

- admission failure performs no inference;
- permission commit failure or uncertainty emits no model-derived response;
- delivery-event failure after stdout leaves `DeliveryUnknown`;
- checkpoint failure retains authoritative WAL data and enters degraded mode;
- deterministic safety denies still deny.

Free-space and WAL thresholds can pause new model attempts but are not treated
as proof a commit will succeed. Coding Brain never deletes WAL, staging files,
audit rows, or journals automatically to make room. Migration and export publish
only fully flushed staging targets. After space is restored, a fresh connection
lets SQLite resolve transaction state without replaying provider output.

## Schema Evolution

Hooks open only the exact current `application_id` and `user_version`. Older,
newer, incomplete, or migrating schemas return promptly to native handling.
Only non-hook startup upgrades schemas. Compatible changes run in one exclusive
SQLite transaction; rebuild-required changes use verified staging and atomic
publication. There are no automatic down-migrations.

Authority invariants use `CHECK`, unique, and composite foreign-key constraints
rather than correctness-critical triggers alone. Every released schema is kept
as a frozen database fixture. Bundled SQLite is pinned through `Cargo.lock`, its
effective version is visible in diagnostics, and security updates follow the
normal release process.

## Testing

### Migration and compatibility

- Import every supported decision, activity, lifecycle, journal, and review
  schema.
- Cover valid unterminated tails, malformed complete rows, oversized records,
  duplicate/conflicting terminal evidence, and newer schemas.
- Include the exact `codexctl-4vh58` pending-journal relationship.
- Kill migration during staging writes, verification, flush, and publication;
  prove canonical state is always entirely legacy or entirely SQLite.
- Race a legacy writer at final fingerprinting, prove the fixed-order lock
  protocol prevents publication over changed input, and prove frozen legacy
  stores cannot silently diverge after cutover.
- Re-import audit and legacy exports through the corresponding legacy readers
  and compare semantic projections and stable ordering.
- Verify the frozen final-JSONL compatibility profile and reject an
  unrepresentable downgrade rather than producing a lossy export.

### Permission atomicity and delivery

- Kill separate hook processes before admission commit, during inference,
  during permission writes, before commit, after commit, during stdout, and
  before delivery recording.
- Prove no partial transaction grants authority and no restart replays stdout.
- Inject an uncertain commit result, then test both possible fresh-connection
  outcomes without allowing the original hook to emit a response.
- Preserve committed, delivery-unknown, delivery-failed, delivered, and exact
  outcome-confirmed projections across Codex, Claude, and Antigravity.
- Preserve deterministic-deny behavior during busy, corrupt, and unavailable
  storage.

### Concurrency and bounds

- Race separate processes on the same exact request and prove one inference
  winner.
- Run independent permission bursts alongside lifecycle/activity writes, long
  readers, WAL checkpoints, and a continuously held writer.
- Use a realistic store larger than the former cumulative 16 MiB recovery
  budget and prove permission commits and indexed reads do not scan history.
- Assert intended indexes with SQLite query-plan tests for permission,
  projection, Scorecard, distillation, review, and outcome paths.
- Pin WAL readers and grow headless hook traffic through warning and hard
  thresholds; prove normal auto-checkpoints, bounded commit latency, degraded
  reporting, and fail-closed inference suspension.
- Delete retained rows and rebuild/upgrade fixtures while proving source cursor
  and high-water values never decrease or reuse an identity.

### Review state

- Preserve independent surface cursors and optimistic revisions.
- Preserve count/conflict/reset behavior.
- Prove review mutations cannot change audit, learning, permission, delivery,
  or execution authority.
- Corrupt or block `review.sqlite3` and prove permission/audit access through
  `brain.sqlite3` remains coherent.

### Corruption and filesystem security

- Cover invalid database headers, damaged pages, foreign-key failures,
  unsupported schemas, unsafe modes/ownership, symlinks, hard links, replaced
  sidecars, and maliciously named staging artifacts.
- Prove a bad database leaves provider-native handling intact and the TUI's
  last coherent view visible with an explicit error.
- Cover local-filesystem/WAL capability rejection, disabled extension/URI
  controls, defensive schema behavior, and bounded SQLite resource limits.
- Exercise `forget()` across decision payloads, immutable preference
  generations, WAL pages, and frozen legacy snapshots; prove audit identity is
  retained and forgotten learning content cannot reappear in downgrade export.
- Inject page-limit, disk-full, write, flush, directory-sync, and checkpoint
  failures at every admission, commit, delivery, migration, and export boundary.

### Release gates

- Run workspace tests, strict Clippy, formatting, and build checks.
- Verify Linux, macOS, and musl packaging.
- Verify the packaged application does not require a system SQLite library.
- Verify crates.io packageability separately from workspace builds.
- Exercise frozen schema fixtures through every supported upgrade and reject
  newer, older, and partially migrated schemas from hook paths.

## Documentation and ADR

Implementation must add an ADR that supersedes the cross-store permission
transaction and JSONL canonical-storage portions of ADR-0003 while retaining
its proposal/commit/delivery/execution evidence semantics. It must update the
ADR index, configuration, reference, troubleshooting, architecture, and release
documentation for:

- the canonical database path;
- the separate Brain/review database failure domains;
- automatic non-hook migration;
- hook behavior before migration;
- audit and downgrade export;
- review-state reset;
- corruption/Doctor behavior; and
- the unchanged `session-links.jsonl` boundary;
- the frozen legacy export profile and support window;
- WAL maintenance thresholds and local-filesystem requirement; and
- privacy erasure across SQLite, preferences, and frozen legacy snapshots.

## Acceptance Criteria

The design is satisfied when:

1. proposal, exact permission authority, and terminal activity commit in one
   SQLite transaction;
2. response delivery remains separately and accurately represented;
3. permission transaction journals and cross-store recovery are removed from
   the live path;
4. permission work stays bounded as history grows;
5. concurrent hooks remain single-winner per exact request and fail closed on
   busy/corrupt storage;
6. automatic migration is atomic, non-hook-only, restartable, and preserves
   legacy inputs;
7. audit and downgrade exports are stable, bounded, and verified;
8. review state is transactional, isolated in its own database, and remains
   visibility-only;
9. `session-links.jsonl` remains separate while complete lifecycle persistence
   moves into the Brain database;
10. the `codexctl-4vh58` fixture no longer blocks coherent Brain projection or
    response handling; and
11. an approved ADR and implementation plan record the final schema and rollout
    sequence before code changes begin.

## Stress Test Results: Unified SQLite Brain Storage

### Resolved Decisions

- Keep `rusqlite` and persistence in the root binary crate; core retains pure
  lifecycle and activity contracts.
- Retain per-request OS locks for concurrent admission, but use unique attempt
  IDs so sequential identical requests are not permanently deduplicated.
- Treat commit errors as response-ineligible uncertainty and never reconstruct
  provider delivery.
- Migrate complete lifecycle persistence and freeze legacy mutable stores to
  prevent a post-cutover split brain.
- Preserve exact historical `Allowed`/`Denied` activity as migrated commitment
  without making it response-eligible.
- Support one frozen final-JSONL downgrade profile rather than arbitrary old
  versions.
- Use full-synchronous WAL with bounded headless checkpointing and hard
  fail-closed growth limits.
- Use an implementable dedicated-directory security boundary rather than claim
  no-follow guarantees for every SQLite VFS sidecar.
- Isolate operational review state in `review.sqlite3`.
- Make schema upgrades non-hook-only, versioned, fixture-backed, and never
  partial across permission stores.
- Split immutable decision identity from erasable learning payload and extend
  privacy erasure to WAL and preserved legacy snapshots.
- Define disk-full behavior at admission, commit, delivery, checkpoint,
  migration, and export boundaries.
- Preserve a nondecreasing activity high-water cursor across retention,
  rebuilds, and distillation publication.

### Changes Made

- Expanded migration from permission-only lifecycle data to the complete
  lifecycle snapshot.
- Split authoritative Brain and operational review state into two SQLite
  databases under a dedicated owner-only directory.
- Replaced permanent request-identity uniqueness with live advisory admission
  and per-invocation attempt identity.
- Added commit-uncertainty, split-brain prevention, fixed legacy export,
  headless WAL, privacy erasure, disk-full, schema-evolution, and monotonic
  cursor contracts.

### Deferred / Parking Lot

- No custom SQLite VFS. The same-UID adversary is outside the supported local
  state threat boundary.
- No arbitrary historical downgrade formats; only the frozen final-JSONL
  profile is supported.
- `session-links.jsonl` remains a separate bounded evidence store.

### Confidence Assessment

- Overall: High
- Areas of concern: the implementation plan must keep cutover, frozen legacy
  handling, privacy erasure, and cross-platform SQLite filesystem behavior in
  independently testable stages; none may be deferred past runtime cutover.
