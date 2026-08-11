# Task 1 report: lifecycle hook budget and bounded mechanics

Status: DONE

Commit: pending (created after this report is staged)

## Result

- Added a 1.5-second entry-scoped `HookBudget`, injected monotonic clock, closed timing fields, and opt-in `CBRAIN_HOOK_TIMING=1` diagnostic output. Normal hook output remains quiet.
- Captured the entry instant before CLI parsing and passed the same budget through the lifecycle hook path.
- Replaced the lifecycle stdin read with an absolute-deadline Unix nonblocking read/poll loop that retains the 64 KiB cap and returns a distinct timeout error before storage is opened.
- Added typed, absolute-deadline bounded child-process execution with shared output accounting and compatibility wrappers. The process group is terminated and reaped on timeout, I/O failure, or output overflow.
- Exposed `HookBudget::optional_child_deadline`, `OutputBudget`, `run_bounded_process_until`, and `live_parent_process_until` for subsequent tasks. Optional child work is skipped when it would consume the full 500 ms storage reserve.
- Did not change Brain schema/version, provider timeout, storage durability, migrations, lifecycle ordering, or permission authority. Project/Git cache wiring remains for later tasks.

## Test-driven development evidence

1. Added the timing test first; it failed with the expected unresolved `crate::lifecycle_timing` import before implementation.
2. Added bounded-process tests first; they failed because `OutputBudget`, `run_bounded_process_until`, and `BoundedProcessError` did not exist.
3. Added the deadline-aware stdin test first; it failed because the deadline reader and `HookInputError::Timeout` did not exist.
4. Implemented the minimum production code and reran the focused tests successfully.

## Verification

- `nix develop path:. --command cargo test -p coding-brain lifecycle_timing -- --nocapture` — passed (2 timing tests in both lib and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_process -- --nocapture` — passed.
- `nix develop path:. --command cargo test -p coding-brain bounded_reader_until -- --nocapture` — passed (one test in both lib and binary targets).
- `nix develop path:. --command cargo fmt --check` — passed.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.
- `git -c core.whitespace=blank-at-eol diff --check` — passed.

## Diagnostic notes

- The plan's combined Cargo filter command is not valid Cargo syntax because Cargo accepts one filter. I ran the three focused filters separately.
- Plain `git diff --check` reports every rustfmt-indented changed line due to this worktree's global `core.whitespace=indent-with-non-tab` setting. The source tree itself uses spaces; the scoped trailing-whitespace check above passed.
- An initial compile error showed that the binary and library each compile their own module tree. Declaring `lifecycle_timing` in both `main.rs` and `lib.rs` resolved that without changing behavior.

## Review correction pass

- Deferred `StorageDeadline` construction until after optional parent discovery. Parent discovery receives only the child deadline that preserves 500 ms; authoritative SQLite open now receives a fresh full 500 ms reserve.
- Linux `/proc` reads now check the stored absolute deadline before and after each accepted read, and debit stat/link bytes from the shared `OutputBudget`.
- Successful persisted hooks update `HookTiming` from the closed lifecycle event mapping; `other` remains the fallback when the event is not established.
- The stdin timeout test now writes partial input and holds the writer open through the deadline. The timeout occurs before `run_provider_with_sqlite` derives state paths or opens Brain storage, so this input-only fixture cannot touch cache or Brain state.
- Added focused coverage for closed event mapping and Linux deadline/output accounting. Existing typed output-limit and timeout coverage remains in place.
- `Spawn` and `ExitStatus` remain directly producible from commands, but the current public process API has no truthful deterministic injection point for post-spawn stdout I/O failure or unavailable reaper setup (`Cleanup`). I did not fabricate those outcomes; a separate seam would be needed if their deterministic coverage is required.

Correction verification:

- `nix develop path:. --command cargo test -p coding-brain bounded_reader_until -- --nocapture` — passed.
- `nix develop path:. --command cargo test -p coding-brain bounded_proc_reads -- --nocapture` — passed.
- `nix develop path:. --command cargo fmt` — passed.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.

Final correction commit `ef2dca41` adds a private resolved-reaper seam and cfg(test)-only stdout descriptor fault. It deterministically covers `Cleanup`, `Spawn`, `ExitStatus`, and post-spawn `Io` without changing the public process API.

Final process-group proof uses an inherited readiness socket and a descendant blocked on an inherited pipe; it waits for the parent/descendant PID handshake, observes the bounded timeout, and asserts both PIDs return `ESRCH` after group termination.

Post-`336656a5` verification:

- `nix develop path:. --command cargo fmt --check` initially found one rustfmt line wrap in the new test; `nix develop path:. --command cargo fmt` applied it, and the final commit records that mechanical correction.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.
- `nix develop path:. --command cargo test -p coding-brain bounded_process -- --nocapture` — passed.

Task 1 commit list: `3d5b60f7`, `7ae473d6`, `ef2dca41`, `336656a5`.
