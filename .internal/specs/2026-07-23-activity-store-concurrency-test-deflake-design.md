# Activity Store Concurrency Test Deflake

## Context

`append_from_snapshot_serializes_concurrent_idempotency_checks` starts two
writers together and unwraps both results. Each writer uses the production
`ActivityStore` lock timeout of 100 ms while the other may still be reading,
writing, flushing, and calling `sync_data` under the exclusive lock. A loaded
GitHub Actions runner can therefore turn expected lock contention into
`ActivityStoreError::LockTimeout`.

The production timeout remains intentional. The separate
`lock_wait_is_bounded_and_busy_compaction_skips` test verifies that foreground
writes stop waiting and busy compaction skips work within a bounded interval.

## Decision

Change only the concurrency test:

- Give its `ActivityStore` an explicit lock timeout large enough for a
  synchronized test writer to wait on another writer without depending on host
  fsync latency.
- Coordinate the writers so one snapshot transaction deliberately holds the
  lock while the other begins its append. This makes the contention path part
  of the test rather than an incidental scheduler outcome.
- Preserve the existing result assertions: both marker events are appended,
  while the idempotency check appends exactly one outcome for `target`.

No production timeout, locking implementation, or compaction behavior changes.

## Test Method

First, add the contention coordination while retaining the default 100 ms
timeout and run the focused test. The expected failure is `LockTimeout` from
the waiting writer, which demonstrates the CI failure deterministically.

Then configure only this fixture with a larger timeout and rerun the focused
test repeatedly. Both writers must succeed, and the final log must contain two
markers and one outcome.

## Validation

Run:

1. The focused concurrency test repeatedly.
2. `cargo test --all-targets`.
3. `cargo fmt --check`.
4. `cargo clippy --all-targets -- -D warnings`.

The bounded-lock test must continue to use the default production limits and
remain meaningful.

## Scope

The implementation is confined to the test module in `src/brain/activity.rs`.
Documentation and public behavior do not change.
