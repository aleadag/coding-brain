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

- Deferred the authoritative storage deadline until after optional parent discovery. Parent discovery receives only a child deadline that preserves 500 ms when possible; SQLite receives the absolute minimum of the original entry deadline and storage start plus 500 ms, so slow input leaves only the remaining entry budget or prevents storage from starting.
- Linux `/proc` reads now check the stored absolute deadline before and after each accepted read, and debit stat/link bytes from the shared `OutputBudget`.
- Successful parsing immediately updates `HookTiming` from the closed lifecycle event mapping, including `SessionStart` and Antigravity preparse. Later storage or persistence failures therefore retain the parsed event; `other` remains the fallback only when parsing cannot establish one.
- The stdin timeout test now writes partial input and holds the writer open through the deadline. The timeout occurs before `run_provider_with_sqlite` derives state paths or opens Brain storage, so this input-only fixture cannot touch cache or Brain state.
- Added focused coverage for closed event mapping and Linux deadline/output accounting. Existing typed output-limit and timeout coverage remains in place.
- `Spawn` and `ExitStatus` remain directly producible from commands, but the current public process API has no truthful deterministic injection point for post-spawn stdout I/O failure or unavailable reaper setup (`Cleanup`). I did not fabricate those outcomes; a separate seam would be needed if their deterministic coverage is required.

Correction verification:

- `nix develop path:. --command cargo test -p coding-brain bounded_reader_until -- --nocapture` — passed.
- `nix develop path:. --command cargo test -p coding-brain bounded_proc_reads -- --nocapture` — passed.
- `nix develop path:. --command cargo fmt` — passed.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.

Final correction commit `ef2dca41` adds a private resolved-reaper seam and cfg(test)-only stdout descriptor fault. The final pass injects an invalid descriptor at descriptor setup so the real production `fcntl` error branch maps the failure to `Io`; the test no longer returns a seam-selected classification directly.

Final process-group proof uses an inherited readiness socket and a descendant blocked on an inherited pipe; it waits for the parent/descendant PID handshake, observes the bounded timeout, and asserts both PIDs return `ESRCH` after group termination.

Post-`336656a5` verification:

- `nix develop path:. --command cargo fmt --check` initially found one rustfmt line wrap in the new test; `nix develop path:. --command cargo fmt` applied it, and the final commit records that mechanical correction.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.
- `nix develop path:. --command cargo test -p coding-brain bounded_process -- --nocapture` — passed.

Task 1 commit list before this final pass: `3d5b60f7`, `7ae473d6`, `ef2dca41`, `336656a5`, `1a3e8809`.

## Final correction pass

- Carried the authoritative SQLite budget as one absolute instant through `StorageDeadline::at`, and rejected an expired hook deadline before migration preflight. A fake-clock contract test proves the deadline is exactly storage start plus 500 ms when available, the original entry deadline when less remains, and absent at entry-budget exhaustion.
- Changed bounded `/proc` file reads to probe one sentinel byte at exact per-file or shared-budget exhaustion without debiting it. Exact-limit inputs now succeed; one-byte overflow fails with unchanged accepted-byte accounting.
- Preparses lifecycle input immediately after the bounded stdin read and records every supported lifecycle class, including `SessionStart`, before parent discovery or storage. A subprocess test removes current storage after migration and proves a parsed SessionStart still reports `event=session_start` on the real storage-unavailable path.
- Moved the cfg(test) process fault to descriptor setup (`-1`) and let the production `fcntl` branch perform cleanup and return `BoundedProcessError::Io`.
- Increased only the synchronized descendant-cleanup test allowance from 100 ms to the established 250 ms parent-process timeout; production timing constants are unchanged.

Final-pass TDD evidence:

1. The absolute storage-deadline test first failed to compile because `authoritative_storage_deadline` did not exist; it passed after carrying the absolute child deadline into storage.
2. The per-file and shared-budget exact-limit tests first returned `None` for exact-size input; they passed after the non-debiting sentinel probe.
3. The preparse timing test first failed to compile because the preparse helper did not exist; the real SessionStart storage-failure test then failed with `event=other` before the closed class was added. Both now pass.
4. The descriptor fault test first failed to compile against the renamed injection point; it passes through the production `fcntl` error branch.
5. The synchronized descendant test first failed its 250 ms contract assertion while the local allowance was 100 ms; it passes with the test-only allowance set to 250 ms.

Final-pass verification from the completed working tree:

- `nix develop path:. --command cargo test -p coding-brain lifecycle_timing -- --nocapture` — passed (3 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_reader_until -- --nocapture` — passed (1 test in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_proc -- --nocapture` — passed (9 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_process -- --nocapture` — passed (6 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain lifecycle_hook::tests -- --nocapture` — passed (64 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain --test lifecycle_hook_cli -- --nocapture` — passed (31 passed, 1 existing latency smoke ignored).
- `nix develop path:. --command cargo test -p coding-brain --test sqlite_storage -- --nocapture` — passed (113 passed, 1 subprocess helper ignored).
- `nix develop path:. --command cargo fmt --check` — passed.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.

## Controller correction pass

- Reserved the full worst-case optional-work tail before parent discovery starts: 250 ms for bounded cleanup plus 500 ms for authoritative storage. The shared `BOUNDED_PROCESS_CLEANUP_BUDGET` constant is crate-visible so later Git callers can make the same admission decision without duplicating the cleanup bound.
- Replaced the capacity-16 synchronous reaper queue with the process-wide unbounded `mpsc::Sender`. A completed 250 ms kill/wait interval can therefore hand the child to the reaper without blocking on queue capacity. Receiver disconnection cannot occur while the successful static sender and worker live; the defensive injected-disconnection path performs a nonblocking `try_wait` and otherwise delegates the wait to a detached fallback reaper.
- Preserved process-group `SIGKILL` and reaping. The synchronized real-time regression budgets 250 ms for child work, 250 ms for cleanup, and 500 ms for storage; after timeout and cleanup it observes at least the full storage reserve and verifies both parent and blocked descendant return `ESRCH`.
- Added a private reader/clock seam for bounded `/proc` input. Both ordinary EOF and exact-bound sentinel EOF now recheck the absolute deadline before returning success; a fake-clock reader advances during the EOF read, so the regression is deterministic and contains no sleep.
- Gave `Cleanup`, `Spawn`, `ExitStatus`, and `Io` classification checks separate fresh 250 ms absolute deadlines. Production provider and process timeouts remain unchanged.

Controller-pass TDD evidence:

1. The reserve-admission test first failed to compile because `optional_parent_process_deadline` and `BOUNDED_PROCESS_CLEANUP_BUDGET` did not exist. It passed after the admission reserve became cleanup plus storage.
2. The unbounded-handoff test first failed to compile because the production seam still required `SyncSender`; it passed after the static reaper moved to `mpsc::Sender`.
3. The controlled EOF test first failed to compile because `read_bounded_reader_until` did not exist. It passed after both EOF success branches gained post-read deadline checks.

Controller-pass verification from the completed working tree:

- `nix develop path:. --command cargo test -p coding-brain bounded_process -- --nocapture` — passed (6 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_proc -- --nocapture` — passed (10 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain reaper -- --nocapture` — passed (4 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain bounded_reader_until -- --nocapture` — passed (1 test in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain lifecycle_timing -- --nocapture` — passed (3 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain lifecycle_hook::tests -- --nocapture` — passed (65 tests in both library and binary targets).
- `nix develop path:. --command cargo test -p coding-brain --test lifecycle_hook_cli -- --nocapture` — passed (31 passed, 1 existing latency smoke ignored).
- `nix develop path:. --command cargo fmt --check` — passed.
- `nix develop path:. --command cargo clippy -p coding-brain --all-targets -- -D warnings` — passed.
