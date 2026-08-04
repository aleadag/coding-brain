# Task 5 Report: Isolated SQLite Review State

## Scope and runtime boundary

Task 5 adds the inactive `ReviewDb` adapter and the pure runtime evidence seam. Production refresh and mutation still select `review-state.json`; `review.sqlite3` is not selected until Task 8. The adapter never attaches `brain.sqlite3`, and reset removes only the owner-only Review database and its validated SQLite sidecars.

The pre-activation schema correction adds nullable `review_meta.last_archive_revision` without changing schema version 1. A nonempty archive stores its new surface revision. Undo targets that exact revision and clears the slot, while pruning clears it when no exact rows from the remembered batch remain. Older archived rows are never promoted after a later archive is undone.

## RED evidence

The initial schema regression failed against the frozen fixture:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_schema_remembers_only_the_latest_undoable_archive_revision -- --exact --nocapture
test review_schema_remembers_only_the_latest_undoable_archive_revision ... FAILED
no such column: last_archive_revision
```

The first adapter round-trip did not compile because the Review evidence and database APIs did not exist:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_db_round_trips_one_surface_without_changing_other_revisions -- --exact --nocapture
error[E0432]: unresolved imports ... `ReviewEligibility`, `ReviewEligibleOccurrence`
error[E0599]: no method named `mutate` found for struct `ReviewDb`
error[E0599]: no method named `read_surface` found for struct `ReviewDb`
error: could not compile `coding-brain` (test "sqlite_storage")
```

Corrupt Review recovery initially failed at the missing reset API:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_reset_recovers_corruption_without_changing_brain -- --exact --nocapture
error[E0599]: no function or associated item named `reset` found for struct `ReviewDb`
error: could not compile `coding-brain` (test "sqlite_storage")
```

The compatibility review then exposed two evidence-contract gaps. The opaque legacy-key test first failed because the constructor did not preserve an existing key, and the cross-surface test failed because occurrences were not bound to their source surface:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_evidence_preserves_canonical_and_opaque_legacy_keys -- --exact --nocapture
error[E0599]: no function or associated item named `new` found for struct `ReviewEligibleOccurrence`

$ nix develop path:. --command cargo test --test sqlite_storage review_evidence_rejects_cross_surface_keys_before_sql -- --exact --nocapture
error[E0061]: this function takes 2 arguments but 3 arguments were supplied
error: could not compile `coding-brain` (test "sqlite_storage")
```

The reset concurrency review reproduced replacement while another process held an idle Review connection:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_reset_is_busy_while_another_process_holds_a_connection -- --exact --nocapture
thread 'review_reset_is_busy_while_another_process_holds_a_connection' panicked:
assertion failed: matches!(reset, Err(StorageError::Busy))
test result: FAILED. 0 passed; 1 failed
```

## Implementation

- `ReviewEligibility` binds one closed surface to at most `MAX_REVIEW_KEYS` exact key/cursor occurrences and a nondecreasing Brain high-water. Each occurrence retains its source surface and exact existing `ReviewKey`; its bounded SQLite group ID is the key's stable hexadecimal form, including opaque legacy Review identities.
- `ReviewDb::read_surface` loads one bounded indexed surface. A mark applies only when group ID and nonzero source cursor both match current captured evidence.
- `ReviewDb::mutate` validates before SQL, starts one immediate transaction, loads and prunes only the affected surface, applies the shared pure mutation rules, replaces exact rows, updates metadata, verifies counts, and commits within the caller's deadline.
- Every `ReviewDb` retains the validated state-root and database-directory descriptors, takes a shared lock on the state-root descriptor before the descriptor-relative `review-reset.lock`, and holds both for its connection lifetime. Reset takes the same anchor and gate exclusively in that order before touching SQLite. The independently anchored state-root lock still conflicts if the named gate or database-directory pathname is replaced, while post-lock descriptor/path checks fail closed on races.
- `ReviewDb::reset` validates the database, reset gate, and known sidecars descriptor-relatively; it rejects symlinks, hard links, broad modes, unknown sidecars, and path substitution, then recreates only `review.sqlite3`.
- SQLite revision `i64::MAX` maps to `ReviewRevisionOverflow` before any signed cast. Recent archive rows or metadata fail closed, and mutation loops recheck the absolute deadline.

## Verification

Focused evidence after final formatting and self-review:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_ -- --test-threads=1
test result: ok. 23 passed; 0 failed; 1 ignored; 0 measured; 74 filtered out

$ nix develop path:. --command cargo test --test brain_review_state -- --test-threads=1
test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured
```

The SQLite matrix covers independent revisions, selected and all-item archive/undo cycles, second-undo non-promotion, stale/count/disposition/Recent failures, cursor resurfacing, affected-surface orphan pruning, append races, corruption, busy isolation with concurrent Brain writes, reset recovery, unsafe reset paths, schema equality and rejection, capacity and revision bounds, and the primary-index query plan.

Focused reset-gate verification after the concurrency review:

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_reset_is_busy_while_another_process_holds_a_connection -- --exact --nocapture
test result: ok. 1 passed; 0 failed

$ nix develop path:. --command cargo test --test sqlite_storage review_reset_rejects_unsafe_sidecars_and_path_substitution -- --exact --nocapture
test result: ok. 1 passed; 0 failed

$ nix develop path:. --command cargo test --test sqlite_storage review_reset_recovers_corruption_without_changing_brain -- --exact --nocapture
test result: ok. 1 passed; 0 failed

$ nix develop path:. --command cargo test --test sqlite_storage review_reset_is_busy_while_a_local_connection_is_alive -- --exact --nocapture
test result: ok. 1 passed; 0 failed
```

Final workspace gates after the completed self-review:

```text
$ nix develop path:. --command cargo test --workspace --all-targets -- --test-threads=1
25 successful test binaries; 3145 passed; 0 failed

$ nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile; no warnings

$ nix develop path:. --command cargo fmt --all -- --check
exit 0

$ nix develop path:. --command cargo build --workspace --all-targets
Finished `dev` profile

$ nix develop path:. --command cargo tree -p coding-brain-core
exit 0; no `rusqlite` dependency

$ cmp -s src/brain/storage/schema-v1/review.sql tests/fixtures/storage/schema-v1/review.sql
exit 0
```

## Review follow-up

An independent review found that the named reset gate was itself a replaceable inode. The real regression unlinked and recreated a valid owner-only gate while a live `ReviewDb` held the old inode; reset incorrectly returned success and replaced `review.sqlite3`. A second corruption regression inserted an exact mark whose cursor exceeded the persisted surface high-water; `read_surface` incorrectly accepted the mark.

```text
$ nix develop path:. --command cargo test --test sqlite_storage review_reset_rejects_replaced_gate_while_a_live_connection_uses_the_old_inode -- --exact --nocapture
RED: reset returned success, so the expected Busy or InvalidStorage assertion failed
GREEN: test result: ok. 1 passed; 0 failed

$ nix develop path:. --command cargo test --test sqlite_storage review_rejects_exact_mark_beyond_persisted_source_high_water -- --exact --nocapture
RED: read_surface accepted the corrupt exact future mark, so the expected InvalidStorage assertion failed
GREEN: test result: ok. 1 passed; 0 failed
```

The fix retains the validated state-root descriptor as the lock anchor for every live Review connection and reset, with lock order `state-root anchor -> named gate -> SQLite`. Connection field order releases SQLite before the guard, and guard field order releases the named gate before the anchor. `load_surface` now rejects every stored cursor above `review_meta.source_high_water` before exact-evidence matching.

Fresh follow-up verification:

```text
$ nix develop path:. --command cargo test --test brain_review_state --test sqlite_storage review -- --test-threads=1
brain_review_state: 1 passed; 0 failed; 1 ignored
sqlite_storage: 25 passed; 0 failed; 1 ignored

$ nix develop path:. --command cargo test --workspace --all-targets --quiet -- --test-threads=1
25 successful test binaries; 3145 passed; 0 failed

$ nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
Finished `dev` profile; no warnings

$ nix develop path:. --command cargo fmt --all -- --check
exit 0

$ nix develop path:. --command cargo build --workspace --all-targets
Finished `dev` profile
```
