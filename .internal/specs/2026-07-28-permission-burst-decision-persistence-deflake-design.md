# Permission-Burst Decision Persistence Deflake

## Context

`parallel_codex_permission_burst_preserves_complete_initial_lifecycles` starts
15 permission hooks together to prove that each request writes one atomic
`Observed` and `Evaluating` activity batch before inference. After all hooks
reach inference, the test releases them together. That second burst makes all
15 hooks append proposals to the shared decision store, where every append
flushes and syncs while holding the production lock. On a loaded CI runner, one
append can exhaust the fixed 100 ms decision-store timeout even though proposal
concurrency is not the behavior this test covers.

The resulting assertion panic occurs while the test holds `HOME_ENV_LOCK`.
Two runtime tests still acquire that mutex with `unwrap()`, so they report
`PoisonError` instead of testing their own behavior.

The production 100 ms decision-store timeout is intentional and must not
change.

## Design

Keep the barrier and ready channel that make all 15 initial activity batches
concurrent. Once every hook has reached inference, release one hook and wait
for that hook to finish before releasing the next. A five-second channel
deadline remains a test-only hang guard. This isolates the activity-store
regression from unrelated proposal-store contention without weakening the
assertions that all 15 requests complete with coherent
`Observed -> Evaluating -> Allowed -> Delivered` lifecycles.

Change the two remaining `HOME_ENV_LOCK.lock().unwrap()` calls in
`src/runtime/brain.rs` tests to recover the guard from `PoisonError`, matching
the existing poison-tolerant consumers. This keeps a failed environment-owning
test from cascading into unrelated failures; it does not ignore a failure in
the test that originally panicked.

No production interfaces, lock bounds, persistence ordering, or fail-closed
behavior change.

## Alternatives Considered

- Raising the decision-store timeout in all test builds would reduce the flake
  but could mask accidental contention and would not be local to this
  regression.
- Adding injectable decision-store timing or paths to the permission-hook API
  would provide more control but expands production-facing structure for a
  test that does not intend to exercise proposal concurrency.
- Removing process-global environment mutation from the runtime tests would
  eliminate mutex poisoning, but it is a broader refactor unrelated to this
  CI failure.

## Verification

Run the permission-burst regression repeatedly, run the two runtime tests that
previously failed after mutex poisoning, then run `cargo test --all-targets`,
`cargo clippy --all-targets -- -D warnings`, and `cargo fmt --check`.
