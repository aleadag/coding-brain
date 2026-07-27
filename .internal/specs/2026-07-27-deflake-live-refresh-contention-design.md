# Deterministic Live Refresh Contention Test

## Context

`runtime::brain::tests::live_brain_refresh_reports_busy_during_activity_lock_contention`
holds `activity.lock`, starts an unlocker that sleeps for 25 ms, and expects
`LiveBrainSource::refresh` to acquire the lock before the production 100 ms
timeout. On a saturated macOS runner, the unlocker can remain unscheduled past
that deadline, so the expected-success phase reports `BrainSourceError::Busy`.

The production timeout is not the defect and must remain 100 ms. The test must
continue to cover a refresh that succeeds after short lock contention, a refresh
that maps lock timeout to `Busy`, and a refresh that recovers after the lock is
released.

## Design

Extract the store-dependent portion of `LiveBrainSource::refresh` into a private
helper that accepts an `ActivityStore`. The `BrainSource` implementation will
continue to resolve the normal state path, construct an `ActivityStore` with
`ActivityLimits::default()`, and delegate to the helper. Production behavior and
the public runtime contract remain unchanged.

For the expected-success phase, the test will construct an `ActivityStore` with
a 5,000 ms lock timeout and pass it to the private helper. A channel will confirm
that the unlocker thread has started before the refresh begins; the unlocker will
then retain the existing short delay and release the lock. The longer budget is
local to this test phase and follows the existing deterministic concurrency-test
pattern in `src/brain/activity.rs`.

The explicit `Busy` phase will call the normal `LiveBrainSource::refresh` while
the lock remains held, preserving coverage of the production 100 ms timeout and
its error mapping. After unlocking, the same normal refresh call must succeed,
preserving recovery coverage.

## Verification

The focused test will first be changed to use the proposed private helper so it
fails to compile before the helper exists. After the minimal implementation, run
the focused test repeatedly, then run the binary crate's full test suite and the
repository formatting and lint checks required by `AGENTS.md`.

No configuration, persisted data, public API, or human-facing product behavior
changes. Documentation outside this design note is not required.
