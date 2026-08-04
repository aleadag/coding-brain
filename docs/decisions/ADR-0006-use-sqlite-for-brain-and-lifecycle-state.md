# ADR-0006: Use SQLite for Brain and Lifecycle State

- Status: Accepted
- Date: 2026-08-04
- Bead: `codexctl-2o9fo`
- Design: `.internal/specs/2026-08-04-unified-sqlite-storage-design.md`

## Context

Coding Brain permission handling currently commits one decision across
`brain/decisions.jsonl`, permission authority in `hooks/lifecycle.json`, and
terminal evidence in `activity.jsonl`. An immutable permission journal makes
crashes recoverable, but recovery and idempotency repeatedly scan whole files.
`codexctl-4vh58` demonstrates the limit of that design: repeated verification
can exhaust the bounded evidence budget even when unique stored data remains
below it, leaving a pending journal that degrades every Brain projection.

The separate stores also make large-history reads, append locks, tail repair,
compaction, and short-lived permission hooks contend on global files. Further
hardening would retain the cross-store transaction that causes the complexity.
Operational review state is not authority, but its JSON replacement and locking
logic has similar transactional needs.

ADR-0003 remains authoritative for the distinction between proposal,
commitment, delivery, and execution. This ADR replaces only its canonical
JSONL/cross-store permission persistence and journal-recovery mechanism.

## Decision

### Make one SQLite transaction the permission authority boundary

Store decision identities and erasable payloads, complete lifecycle state,
permission attempts and commits, activity, delivery and outcome evidence,
corrections, and stable learning cursors in the binary-owned
`$XDG_STATE_HOME/coding-brain/db/brain.sqlite3`. Keep SQLite out of
`coding-brain-core`; core retains pure types and projection logic. Keep
`session-links.jsonl` as a separate bounded native-session to live-process
identity log.

Each invocation has a unique attempt ID. The existing owner-only per-request OS
lock suppresses only concurrent duplicates, so a later identical request may be
evaluated normally. Model inference runs outside database transactions. One
short transaction inserts the proposal, matching terminal activity, and exact
immutable `Allow` or `Deny` authority. A failed or uncertain commit never makes
the current hook response-eligible.

Only after commit may the hook write stdout. Delivery evidence is appended in a
second short transaction. A crash or failed audit after successful stdout
remains `DeliveryUnknown`; recovery never replays a provider response.
Deterministic code-owned safety denies remain fail-closed when audit storage is
unavailable.

### Isolate operational review state

Store review revisions and per-surface cursor/disposition marks in
`$XDG_STATE_HOME/coding-brain/db/review.sqlite3`. Never attach it to the Brain
database or imply cross-database atomicity. Review failure disables review
mutations without disabling coherent permission or audit state. Reset replaces
only the review database.

### Migrate automatically outside hook deadlines

Only non-hook `cbrain` startup paths initialize or migrate databases. Hooks that
see legacy-only, migrating, unsupported, busy, corrupt, or unsafe authoritative
storage return promptly to native provider handling and perform no model
inference.

Migration streams and validates the complete decisions, activity, lifecycle,
permission-journal, and review legacy set into same-directory staging
databases. Final cutover acquires legacy locks in a fixed order, revalidates
source fingerprints, publishes an incomplete Brain generation, freezes migrated
legacy stores read-only, and only then marks the generation complete. Any later
legacy mutation is a split-brain error and is never auto-merged. Review
migration failure remains isolated from Brain publication.

Exact matching historical proposal plus terminal `Allowed` or `Denied`
activity imports as committed audit under ADR-0003, but every migrated commit is
response-ineligible. Proposal-only, nonterminal, mismatched, or conflicting
evidence creates no authority.

### Make rollback explicit and bounded

SQLite is the sole live writer. Audit export remains a versioned archival
format. Downgrade export supports one frozen compatibility profile for the final
pre-SQLite release and verifies it with frozen legacy readers; it never
dual-writes or swaps live state. Removing that compatibility profile requires a
separate ADR and release note.

### Preserve durability, boundedness, and privacy

Use bundled SQLite in a dedicated owner-only local-filesystem directory, WAL,
`synchronous=FULL`, foreign keys, defensive mode, disabled extension loading,
trusted-schema restrictions, secure deletion, and explicit SQLite resource
limits. Hook operations share one monotonic storage deadline. Auto-checkpointing
serves headless use; warning and hard WAL thresholds surface degradation and
eventually suspend model inference without affecting deterministic denies.

Activity uses a nonreusing 64-bit source cursor with a persistent high-water
mark. Its logical activity ID is indexed but non-unique so one activity can
retain observed, terminal, delivery, outcome, and correction evidence; the
terminal activity and authority identity tuple remains unique for permission
commit references. Distillation advances from the cursor in the last atomically
published preference generation. Decision identity is separated from erasable
learning payload so `forget()` can delete learning content, published generations,
WAL residue, and preserved legacy decision snapshots without breaking immutable
audit relationships. Permission identities retain complete authority fields;
non-authoritative observation identities use a distinct closed kind and cannot
carry fabricated permission identity or action fields. The matching erasable
payload remains bounded while preserving the complete supported legacy learning
record.

Disk-full, I/O, checkpoint, corruption, newer-schema, unsafe-path, and migration
failures preserve the last coherent TUI view and never become permission or
delivery evidence. Coding Brain does not automatically delete authoritative
files to recover space or rebuild a published corrupt database from stale
legacy state.

## Rationale

SQLite makes the security-critical permission invariant a local database
constraint and atomic transaction instead of an application-level protocol
across growing files. Indexed key and cursor queries keep hook and projection
cost independent of total history. Full lifecycle migration prevents old and
new permission/topology authorities from coexisting after cutover.

Separating review storage keeps disposable visibility preferences from sharing
the authority database's corruption and writer-contention domain. Retaining
session links as a bounded core-owned log avoids expanding the database boundary
into navigation evidence that requires no atomic relationship with permission
commitment.

Explicit export is safer than live dual writes: it makes compatibility testable
without recreating cross-store permission transactions. Keeping delivery
outside the commit acknowledges the response pipe's real limit instead of
claiming execution from persistence.

## Consequences

- The live permission transaction journal and JSONL destination-verification
  path are removed after cutover.
- The root binary gains bundled SQLite and owns all Brain/lifecycle persistence;
  core lifecycle storage becomes pure state and transition logic.
- Provider hooks require a current, complete database and never perform schema
  migration.
- Migration and every later schema upgrade need crash, concurrency, source-race,
  disk-full, filesystem-safety, and frozen-fixture coverage.
- Runtime queries require explicit row/byte bounds and verified indexes; hook
  paths cannot run bulk retention, vacuum, or unbounded integrity work.
- Operational review reset, corruption, and migration are isolated in a second
  database.
- Rollback requires an offline verified export; preserved legacy files are not
  a live mirror.
- Privacy erasure must cover SQLite payloads, WAL/free pages, immutable
  preference generations, and frozen legacy learning sources.
- ADR-0003's proposal/commit/delivery/execution semantics remain in force, but
  its JSONL canonical-store and cross-store recovery implementation is
  superseded by this ADR.
