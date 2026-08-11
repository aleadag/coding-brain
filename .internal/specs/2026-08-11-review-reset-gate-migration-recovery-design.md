# Review Reset Gate Migration Recovery Design

## Context

The SQLite migrator publishes `review.sqlite3` directly from a verified staging
database. Runtime Review access instead goes through `ReviewDb::open_current`,
which requires the owner-only `db/review-reset.lock` coordination gate to
already exist. A completed migration can therefore contain a valid published
Review database but remain unusable by the TUI, Review mutation, and Review
reset APIs.

The repair must preserve the existing security boundary. Runtime reads must not
silently create coordination state, and an unsafe or foreign gate must never be
removed or replaced. Recovery must preserve the published Brain database and
logical Review rows and remain restart-safe at every durable boundary. Existing
completed migrations may contain a closed Review artifact in
`journal_mode=DELETE`, while runtime Review access requires `WAL`; recovery
therefore includes a controlled, manifest-tracked normalization of that
artifact.

## Decision

The migration coordinator owns Review reset-gate creation and recovery.

For a fresh migration, the coordinator exclusively creates, locks, validates,
and durably syncs the reset gate before publishing the verified Review database.
It retains the validated exclusive guard across Review publication and the
durable transition from `Verified` to `Published`.

For an existing `Complete` migration whose manifest records a published Review
artifact, `resume` repairs a missing gate while holding the migration lock. It
first validates the closed published Review database against the recorded
artifact, row digest, and row count. Only then may it create and durably sync a
new gate. If the validated artifact is still in the legacy closed `DELETE`
representation, recovery transitions it to `WAL` under the held migration and
Review guards, revalidates the same logical rows, and durably updates the
published artifact in the manifest. It validates the complete runtime Review
open contract before returning `Complete`.

An existing gate always follows the ordinary secure open path. Symlinks,
non-regular files, foreign ownership, wrong modes, unexpected link counts,
path/descriptor identity changes, and replacement races remain errors. Recovery
does not unlink, chmod, or replace an existing entry.

## Components and Data Flow

### Migration-owned gate guard

The storage module exposes a narrow migration-internal operation built from the
same `SecureDatabaseDirectory`, state-root lock, reset-gate lock, and identity
validation used by `ReviewDb`. The operation may create a missing gate, returns
an exclusive held guard, and syncs the created gate and containing database
directory before callers cross a publication or completion boundary. Exclusive
state-root locking ensures that recovery cannot create a new gate while a live
Review connection still holds an unlinked old gate inode.

Migration recovery always acquires locks in one direction: `migration.lock`
exclusively, then the state-root lock exclusively, then `review-reset.lock`
exclusively. Code holding a Review guard never attempts to acquire the migration
lock. Runtime Review paths retain their existing state-root-then-gate order and
do not enter migration recovery.

`ReviewDb::open_current` continues to require an existing gate. Review mutation
and reset continue using the same gate and locking rules; no fallback or lazy
creation is added to runtime APIs.

### Fresh Review publication

When `publish_verified_review` has revalidated the staged Review database, it:

1. exclusively acquires or creates the validated reset gate;
2. durably syncs gate creation;
3. publishes or finishes publishing `review.sqlite3` while retaining the guard;
4. transitions the published database into the runtime `WAL` contract;
5. revalidates its identity and logical row digest;
6. persists the normalized `Published` Review result; and
7. releases the guard after the durable state transition.

A crash after gate creation but before publication leaves a legitimate managed
gate that the next migration resume validates and reuses. A crash after Review
publication but before the `Published` result also resumes through the existing
verified-publication recovery without replacing either artifact.

### Completed-migration recovery

After pending migration state is recovered and while the migration lock is
held, a `Complete` state with `ReviewMigrationResult::Published` performs a
bounded closed-database validation against the manifest. If the reset gate is
missing, the coordinator creates and syncs it through the migration-only guard.
If it already exists, the same operation validates and exclusively locks it
without mutation. A live holder of an unlinked old gate makes recovery return
`Busy` rather than permitting split coordination. A legacy closed `DELETE`
artifact is normalized once to `WAL`; the coordinator proves its row digest and
count are unchanged and persists the replacement artifact before opening
`ReviewDb` through its ordinary runtime path and returning `Complete`.

`Complete` states whose Review migration is deliberately degraded are not
invented into successful Review storage. They remain complete for Brain
authority, while full runtime health reports Review unavailable.

### Doctor validation

The SQLite Doctor check validates both halves of the runtime TUI contract after
migration is complete: `BrainDb::open_current` plus `ReviewDb::open_current`.
Failures identify whether Brain or Review open failed and report the matching
redacted database path while retaining fixed storage error categories. Doctor
fails when Review cannot open and passes only when both databases are usable.
Doctor does not perform the repair; a supported non-hook migration resume
performs it.

## Error Handling and Security

- Missing gate during migration resume is the only repairable condition.
- Missing or changed published Review data, manifest mismatches, corrupt schema,
  unsafe sidecars, and unsafe existing gates fail closed.
- Gate creation uses exclusive no-follow creation, owner-only mode, descriptor
  and pathname identity checks, link-count validation, and directory
  correspondence checks already used by storage security code.
- Recovery never deletes or rewrites `brain.sqlite3`, Review rows, or existing
  gate entries. It may rewrite only the SQLite journal-mode representation of a
  validated published Review artifact and must atomically record the resulting
  artifact after proving the logical rows are unchanged.
- Busy locks remain `StorageError::Busy`; they are not bypassed or retried with
  weaker locking.
- Migration gate creation and repair use exclusive state-root and gate locks;
  ordinary runtime Review opens remain shared.
- Migration repair follows the fixed migration-lock, state-root, reset-gate
  acquisition order; no Review-guarded path acquires the migration lock.
- Runtime open, mutation, and reset keep one shared gate contract.

## Verification

Regression coverage will prove:

1. fresh migration creates an owner-only gate and opens the published Review
   database through `ReviewDb::open_current`;
2. a production-shaped `Complete` migration with a valid published Review
   database and missing gate resumes successfully without changing Brain bytes
   or Review rows, while a legacy `DELETE` artifact is normalized to `WAL` and
   its manifest artifact is updated;
3. symlinked, hard-linked, wrong-mode, foreign-owned, non-regular, and
   identity-changing gates remain fail-closed and are not replaced;
4. fault injection around gate sync, Review publication, Review result
   persistence, and complete-state repair resumes without manual deletion or
   Review reset;
5. a live Review connection holding an unlinked old gate makes recovery return
   `Busy`, and recovery succeeds after that connection exits;
6. a full TUI SQLite refresh and Review mutation/reset work after recovery;
7. Doctor identifies Review storage before recovery and passes after the
   complete Review open contract is restored; and
8. focused migration, Review storage, runtime, Doctor, security, formatting,
   lint, and workspace tests pass.

## Scope

This change does not alter Review schemas, migration manifest schemas, legacy
source mapping, Review degradation policy, runtime lock timeouts, release
versioning, or logical operator data. It adds the missing migration-owned gate
lifecycle, a one-time journal-mode normalization for validated legacy published
Review artifacts, and complete runtime health validation.

## Stress Test Results: Review Reset Gate Migration Recovery

### Resolved Decisions

- Gate durability precedes Review publication, and the held guard spans the
  durable `Published` transition.
- Repair authority is limited to a validated `Complete` manifest with an exact
  published Review artifact.
- Existing or racing unsafe entries are validated and rejected, never replaced.
- Deliberately degraded Review migration remains unavailable rather than being
  converted into empty healthy state.
- Doctor remains read-only and reports the failing Brain or Review stage.
- Migration uses exclusive state-root and gate locks so an unlinked old gate
  cannot create split coordination.
- Lock acquisition is one-way from migration lock to state root to reset gate,
  preventing an AB/BA deadlock with runtime Review paths.
- Existing migration phases are sufficient; runtime lazy creation and manifest
  schema expansion are rejected.
- Existing completed `DELETE` artifacts are normalized once under migration and
  Review locks; logical rows are verified before and after and the manifest's
  published artifact is durably replaced.
- Verification includes content preservation, crash recovery, live-old-inode
  contention, unsafe entries, runtime consumers, and workspace gates.

### Changes Made

- Changed the migration guard from shared to exclusive after identifying a race
  with a live connection retaining an externally unlinked old gate inode.
- Required stage-aware Doctor evidence and an explicit old-inode contention
  regression.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: filesystem durability and identity-change tests must prove
  the intended boundary with deterministic fault injection rather than timing.
