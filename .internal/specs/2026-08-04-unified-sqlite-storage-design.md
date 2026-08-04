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
- Do not move non-permission provider topology out of
  `hooks/lifecycle.json`. Only permission disposition and authority leave that
  snapshot.
- Do not continuously mirror SQLite writes into legacy JSONL.
- Do not infer delivery, tool execution, or permission authority from missing
  events, transcripts, legacy proposals, or unresolved migration evidence.
- Do not add at-rest encryption. The confidentiality boundary remains
  owner-only local state.

## Chosen Approach

Use one relational SQLite database with typed and indexed columns for stable,
query-critical fields. Retain bounded JSON only for provider-specific or
evolving payloads that do not determine permission authority.

This is preferred over a single JSON event table because database constraints
should enforce the permission boundary rather than merely store the existing
serialization. It is preferred over full normalization because deeply nested
provider metadata changes independently and does not justify a large join
surface.

## Storage Boundary

The canonical file is:

```text
$XDG_STATE_HOME/coding-brain/brain.sqlite3
```

with the existing fallback to `~/.local/state/coding-brain/brain.sqlite3`.
The containing directory remains owner-only. The database, rollback journal,
WAL, and shared-memory files must be regular, single-link, current-user files
with owner-only permissions.

The binary Brain storage layer owns database creation, migrations, queries,
permission commits, activity appends, learning reads, review state, and
exports. Core activity and lifecycle types remain reusable data contracts.
Non-permission `LifecycleStore` topology and `SessionLinkStore` stay in
`coding-brain-core`.

Use `rusqlite` with bundled SQLite so Linux, musl artifacts, and macOS do not
depend on an ambient runtime SQLite library. Enable foreign keys on every
connection. Use WAL after canonical publication; use a self-contained journal
mode for a migration staging database and checkpoint it fully before rename.

## Logical Schema

The implementation plan will spell out exact SQL, but the following tables and
constraints are part of the design contract.

### `schema_meta`

One row records:

- application identifier;
- schema version;
- completed migration generation;
- legacy schema versions imported;
- migration completion timestamp.

The database is usable only when the application identifier and supported
schema version match and the migration generation is complete.

### `permission_attempts`

Each row represents one exact permission request. It stores a canonical,
validated request-identity key plus typed provider, session, provider-session,
turn, request-key, project, tool, activity ID, state, and timestamps.

The request-identity key is unique and includes the complete validated
lifecycle identity plus request key. It avoids nullable-column uniqueness
ambiguity and selects one winner across processes. Attempt states distinguish
evaluation in progress, native/needs-input disposition, committed decision,
and legacy incomplete evidence. An attempt state alone never grants executable
authority.

The winner transaction also appends the initial `Observed` and `Evaluating`
activity rows. Model inference runs only after that transaction commits and
outside all database transactions. A stale evaluation remains a projection as
`Incomplete`; elapsed time does not serialize an execution outcome.

### `decisions`

Each decision or learning-evidence row has a unique decision ID and typed record
kind, provider, session, turn, project, tool, normalized command, action,
confidence, threshold, source, reasoning, user action, decision type, and
timestamps. This includes hook proposals and the existing non-hook learning
records carried by `decisions.jsonl`. Bounded JSON may retain validated provider
metadata that is not part of authority.

Proposal existence does not grant permission authority and does not prove that
the provider received a response or executed a tool.

### `permission_commits`

Each row is an immutable authoritative permission commit. It has a one-to-one
relationship with a permission attempt and references exactly one proposal and
one terminal activity event. It stores the immutable `Allow` or `Deny` action,
the exact authority identity, commit timestamp, and transaction identifier.

Foreign keys and uniqueness constraints require the attempt identity,
proposal, authority action, decision ID, activity ID, and terminal state to
agree. A model-derived allow or deny exists only if this row and all referenced
rows commit in the same SQLite transaction.

### `activity_events`

This is the append-ordered audit ledger. An integer primary key is the stable
source cursor. Rows contain typed event kind, activity ID, recorded time,
provider/session/turn/tool-use identity, project, state, decision ID, outcome,
correction, supersession, and bounded payload fields.

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

### `review_meta` and `review_marks`

Review metadata stores an independent optimistic revision for each surface.
Review marks key the surface plus stable group identity and source cursor to a
visibility disposition. They are joined before grouping, counts, ordering,
overflow, and truncation.

No foreign-key direction permits a review mutation to update or delete
decisions, permission commits, or activity. Review reset is an explicit
`cbrain storage reset-review-state` operation rather than deletion of the
database.

## Permission Data Flow

### Admission and inference

1. Parse and validate the provider request and exact lifecycle identity.
2. Run deterministic safety classification before model inference.
3. Open a bounded database connection and begin a short admission transaction.
4. Insert the exact `permission_attempts` row and its initial activity events.
   The unique request identity selects one winner. A contender performs no
   inference and emits no model-derived response.
5. Commit admission, close the transaction, then run inference.

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
delivery unknown.

### Response delivery

Only after the atomic commit may the hook serialize and write the provider
response. The response pipe remains outside SQLite:

1. commit permission evidence;
2. write and flush stdout;
3. append `Delivered` or `DeliveryFailed` in a second short transaction.

A crash or uncertain flush after commit leaves `DeliveryUnknown`. Recovery
never replays stdout. Only later exact lifecycle or outcome evidence may claim
tool execution.

Deterministic safety denies continue to fail closed when persistence is
unavailable. Their audit failure is reported through a bounded diagnostic, but
storage failure never turns a deterministic deny into native/model approval.

## Query and Maintenance Bounds

All interactive and hook queries use exact keys or indexed cursors plus
explicit row and serialized-byte limits. Live, Review, Scorecard, Diagnostics,
distillation, outcome correlation, and Doctor must not rely on unbounded table
scans.

The implementation must inspect critical query plans and reject accidental
full scans in regression tests. Permission commit cost must not grow with total
decision or activity history.

Hook processes never run `VACUUM`, bulk retention, full integrity checks, or
unbounded WAL checkpoints. Retention, checkpointing, and compaction run only in
bounded non-hook maintenance. Retention preserves all fresh incomplete
lifecycles, bounded interrupted history, current review/correction semantics,
and all activity at or after the distillation cursor. It cannot delete evidence
needed for permission authority or unprocessed learning.

## Automatic Legacy Migration

### Trigger boundary

Automatic migration runs only from a non-hook `cbrain` startup path, including
the TUI and Doctor. Permission and lifecycle hook entrypoints never perform a
legacy import. If legacy state exists but `brain.sqlite3` does not, hooks return
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
- permission disposition and authority in `hooks/lifecycle.json`;
- `brain/permission-transactions/` journals;
- `review-state.json`.

`session-links.jsonl` and non-permission lifecycle topology are not imported.
Legacy inputs remain unchanged after success.

### Staging and publication

One owner-only migration lock selects the migrator. It creates a unique,
owner-only, same-directory staging database such as:

```text
.brain.sqlite3.migrate-<random>
```

The file is not created under `/tmp`. Legacy rows are streamed with established
per-record and total-size bounds. The migrator imports in source order and does
not hold entire logs in memory.

Before publication it:

1. validates every imported critical record;
2. runs foreign-key and SQLite integrity checks;
3. verifies source/import counts and stable-order cursors;
4. verifies permission and review-state invariants;
5. checkpoints and closes the staging database;
6. flushes the database and containing directory; and
7. atomically renames it to `brain.sqlite3`.

The canonical filename is absent until all checks pass. A crash leaves legacy
state canonical and the staging artifact noncanonical. A later migrator may
remove or replace an artifact only after validating its exact bounded name,
regular-file type, ownership, mode, link count, and location.

### Legacy permission reconciliation

A legacy proposal, matching exact lifecycle authority, and matching terminal
activity become one permission commit. Proposal-only or nonterminal activity
becomes an incomplete attempt without authority.

Pending journal evidence is reconciled inside the staging database. A legacy
`Allowed` row without current exact lifecycle authority is preserved as
historical audit evidence but does not create `permission_commits`. It gains a
bounded integrity diagnostic and is excluded from committed learning
authority. This covers the `codexctl-4vh58` shape without deleting the source
journal, inventing delivery, replaying a response, or blocking projection of
unrelated coherent activity.

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

`cbrain storage export-legacy <directory>` additionally reconstructs the
immediately preceding supported legacy layout, including lifecycle permission
fields and `review-state.json`. Export writes a new owner-only directory and
refuses to overwrite existing targets. It verifies the result by reading it
through the legacy readers and comparing its semantic projections with the
SQLite source, including delivery-unknown state.

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

Every process validates the state directory and database family before open.
Existing database, WAL, shared-memory, journal, and migration paths must not be
symlinks, directories, multi-link files, foreign-owned files, or broader than
the owner-only mode contract. SQLite no-follow opening is used where supported;
post-open identity is revalidated where the API permits it.

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
preserved. Physical database or schema corruption disables every database
domain; a structurally valid but invalid review mark disables review operations
without changing permission authority or otherwise hiding coherent audit data.

Bounded JSON is validated before writes and after reads. Invalid authoritative
data fails the operation. A malformed non-authoritative audit payload becomes a
bounded integrity diagnostic only when its row boundary and typed identity are
still trustworthy; otherwise the affected read fails. It never becomes
executable evidence.

## Testing

### Migration and compatibility

- Import every supported decision, activity, lifecycle, journal, and review
  schema.
- Cover valid unterminated tails, malformed complete rows, oversized records,
  duplicate/conflicting terminal evidence, and newer schemas.
- Include the exact `codexctl-4vh58` pending-journal relationship.
- Kill migration during staging writes, verification, flush, and publication;
  prove canonical state is always entirely legacy or entirely SQLite.
- Re-import audit and legacy exports through the corresponding legacy readers
  and compare semantic projections and stable ordering.

### Permission atomicity and delivery

- Kill separate hook processes before admission commit, during inference,
  during permission writes, before commit, after commit, during stdout, and
  before delivery recording.
- Prove no partial transaction grants authority and no restart replays stdout.
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

### Review state

- Preserve independent surface cursors and optimistic revisions.
- Preserve count/conflict/reset behavior.
- Prove review mutations cannot change audit, learning, permission, delivery,
  or execution authority.

### Corruption and filesystem security

- Cover invalid database headers, damaged pages, foreign-key failures,
  unsupported schemas, unsafe modes/ownership, symlinks, hard links, replaced
  sidecars, and maliciously named staging artifacts.
- Prove a bad database leaves provider-native handling intact and the TUI's
  last coherent view visible with an explicit error.

### Release gates

- Run workspace tests, strict Clippy, formatting, and build checks.
- Verify Linux, macOS, and musl packaging.
- Verify the packaged application does not require a system SQLite library.
- Verify crates.io packageability separately from workspace builds.

## Documentation and ADR

Implementation must add an ADR that supersedes the cross-store permission
transaction and JSONL canonical-storage portions of ADR-0003 while retaining
its proposal/commit/delivery/execution evidence semantics. It must update the
ADR index, configuration, reference, troubleshooting, architecture, and release
documentation for:

- the canonical database path;
- automatic non-hook migration;
- hook behavior before migration;
- audit and downgrade export;
- review-state reset;
- corruption/Doctor behavior; and
- the unchanged `session-links.jsonl` boundary.

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
8. review state is transactional but remains visibility-only;
9. `session-links.jsonl` and non-permission lifecycle topology remain separate;
10. the `codexctl-4vh58` fixture no longer blocks coherent Brain projection or
    response handling; and
11. an approved ADR and implementation plan record the final schema and rollout
    sequence before code changes begin.
