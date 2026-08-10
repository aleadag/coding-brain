# Legacy Writer Guard Journal Race

## Context

PR #86 run `31359558474`, attempt 1, failed on macOS in
`legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal`.
The test discarded the returned `StorageError` behind
`acquire.join().unwrap().is_ok()`. Attempt 2 passed on the unchanged
`d4519149` head.

The interleaving is valid production behavior. A permission transaction writer
acquires the journal directory gate, creates and locks a temporary journal,
then releases the directory gate while it writes and publishes that journal.
Migration can acquire the directory gate while this pre-admitted writer still
holds the journal lock. The writer may then rename or remove its journal as the
legacy guard drains the entry.

`LegacyWriterGuard` already treats a journal that disappears before open, after
lock acquisition, or during path revalidation as a changed enumeration and
starts another bounded pass. One interval is handled differently: the generic
secure `openat` helper performs path metadata, descriptor open, and path
metadata again. If the name disappears after descriptor open, the helper
propagates an I/O `NotFound`; if a different entry occupies the name, it
returns `InvalidStorage("legacy source changed during descriptor open")`. The
failed assertion hid which error macOS returned. The test can race inside this
post-open interval, so scheduler timing decides whether acquisition
re-enumerates or fails.

## Decision

Add a private journal-specific secure-open helper for the drain path. It will
preserve the generic helper's descriptor anchoring and `O_NOFOLLOW` behavior
and return an explicit private result: `Stable(File)` or `Changed`. It will
represent these expected concurrent outcomes as `Changed`:

- the enumerated name disappears before or during descriptor open;
- the opened descriptor no longer matches the enumerated identity; or
- the path disappears or changes identity after descriptor open.

`drain_journal_entry` will translate `Changed` to `Ok(false)`, using the
existing re-enumeration loop and the original `StorageDeadline`. A stable entry
continues through the existing exclusive-lock and path-validation checks.

The helper will validate every identity it observes before comparing it with
the enumerated identity: the pre-open path, opened descriptor, and post-open
path. Only disappearance or a mismatch among otherwise safe owner-only,
single-link regular files may return `Changed`. An unsafe name, symlink,
unsupported type, invalid owner, invalid mode, extra hard link, unexpected I/O
failure, or deadline expiry remains an error. The generic `openat` helper and
every non-journal caller remain unchanged.

## Deterministic Regression

First, change the integration assertion only so failure reports the rename or
removal case and exact returned `StorageError`. The failed macOS job is the
observed TDD red evidence.

Next, extract the journal open sequence behind a private callback seam without
changing its current error classification, and verify the existing tests before
and after that refactor. The seam runs after descriptor open and validation but
before the post-open path lookup; production passes a no-op. Unit tests then
use it to rename, remove, and safely replace the journal at that exact point.
Those tests must fail with the current post-open `NotFound` or identity-change
error before the implementation classifies the race as `Changed`.

The deterministic coverage will also replace the path with a symlink at the
same seam and assert that the unsafe replacement remains an error. These tests
prove the formerly timing-dependent branches without sleeps or a larger
timeout.

The integration regression will retain the writer-shaped end-to-end scenario,
but split rename and removal into named cases. Its final assertion will include
the case and the exact returned `StorageError`, so a future macOS failure cannot
collapse back to an opaque `is_ok` assertion.

## Alternatives Rejected

- Changing the generic secure-open helper would broaden a concurrency exception
  to static legacy sources and freeze artifacts that must remain fail closed.
- Adding retries or increasing the two-second test deadline would not classify
  the race and would leave the same scheduler-dependent failure.
- Synchronizing only the test at journal contention would hide the matching
  production interleaving created by a pre-admitted permission writer.

## Verification

1. Run the deterministic helper tests for both rename and removal.
2. Run the focused integration regression repeatedly and confirm failures, if
   any, print the case and exact `StorageError`.
3. Run the storage migration test target, then `cargo test --all-targets --
   --test-threads=1`.
4. Run `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` in
   the Nix development environment.
5. Use the full serialized macOS GitHub Actions job as the platform acceptance
   gate and record its run in `codexctl-zha56`.

## Scope

The change is limited to journal draining in
`src/brain/storage/legacy.rs`, the existing regression in
`tests/storage_migration.rs`, and this design/plan documentation. It does not
change lock acquisition order, production deadlines, public APIs, migration
state, or any non-journal legacy source behavior.

## Stress Test Results: Journal Race Classification

### Resolved Decisions

- Use an explicit private `Stable(File)` or `Changed` result; do not overload
  `Option` or inspect error text.
- Return `Changed` only for disappearance or identity mismatch among entries
  that pass the existing journal safety validation.
- Keep the helper journal-specific and retain the generic fail-closed `openat`
  behavior for static sources and freeze artifacts.
- Preserve the single absolute deadline and existing enumeration bounds; do
  not add retry counts, sleeps, or longer timeouts.
- Treat the failed macOS job as the original red test, then use a behavior-
  preserving extraction at the post-open lookup to create the deterministic
  red-green regression.

### Changes Made

- Made the open result and validation order explicit.
- Added an unsafe-replacement regression to the rename/removal matrix.
- Replaced the proposed compile-failure TDD step with a behavior-based sequence.
- Corrected the deterministic seam to the post-open lookup and kept the hidden
  historical `StorageError` unspecified until a diagnostic assertion captures
  it.

### Deferred / Parking Lot

- The full serialized macOS job requires a published branch or PR run and
  remains the final platform gate after local implementation.

### Confidence Assessment

- Overall: High
- Areas of concern: macOS remains necessary to confirm the hosted-runner flake
  is eliminated under the exact platform that exposed it.

## Hosted macOS Amendment: Post-lock Removal

The first published candidate fixed namespace churn during descriptor open,
but an unchanged macOS repeat exposed the same removal after the journal lock
was acquired. At that point the held descriptor correctly reports zero links;
validating it before observing the missing pathname incorrectly rejects normal
removal as unsafe storage.

Post-lock validation must snapshot the held descriptor, classify the pathname
with the existing fail-closed matcher, and only validate the held descriptor
when the pathname still matches. A missing pathname or safe identity change
returns `Changed`; an unsafe replacement still returns `InvalidStorage`. This
does not change lock order, deadlines, enumeration bounds, or generic opens.
