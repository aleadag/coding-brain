# Darwin ARM64 Lock Test Deflake

## Context

`brain::activity::tests::lock_wait_is_bounded_and_busy_compaction_skips`
holds the activity-store lock, verifies that an append returns
`ActivityStoreError::LockTimeout`, and then asserts that total wall-clock time
is below the production 100 ms timeout plus 25 ms. A saturated macOS ARM64
runner can deschedule the test thread after the append returns but before the
elapsed-time assertion, causing a false failure.

The production 100 ms lock timeout is intentional and must not change.

## Design

Run the contended append in a worker thread and receive its result through a
channel with a five-second test-only deadline. Assert that the received result
is `LockTimeout`, join the worker, and retain the existing assertion that
compaction skips while the lock remains held.

This keeps the test's behavioral guarantees:

- a contended append returns `LockTimeout`;
- the operation completes within a generous test-only bound rather than
  hanging;
- compaction remains a no-op while the activity lock is busy;
- the production 100 ms timeout remains unchanged.

## Alternatives Considered

- Removing the timing assertion entirely would eliminate the flake but lose the
  hang guard.
- Increasing the elapsed-time tolerance would remain scheduler-sensitive and
  would only make the flake less frequent.
- Injecting a fake clock and sleeper into `lock_with_timeout` would permit exact
  deterministic timing assertions but adds production abstraction solely for
  one test.

## Verification

Run the focused test repeatedly, then run the activity tests and workspace
quality gates (`cargo test`, `cargo clippy -- -D warnings`, and
`cargo fmt --check`) in the Nix development environment.
