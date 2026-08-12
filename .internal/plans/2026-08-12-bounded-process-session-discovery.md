# Bounded Process Session Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development or beads-superpowers:executing-plans to implement this plan task-by-task. Create one child Bead for each task before implementation and keep `codexctl-5n458.8` in progress until installed Doctor acceptance.

**Goal:** Discover the affected host's 191 KiB process table while preserving bounded process execution, and stop Doctor from presenting a failed process scan as authoritative zero provider sessions.

**Architecture:** Parameterize the existing Unix bounded-command runner while retaining its 64 KiB/500 ms compatibility wrapper. Process discovery alone opts into 1 MiB per stream with the same deadline. Provider discovery returns sessions plus one process-snapshot availability bit; existing runtime callers keep their list-returning API, while Doctor consumes the status-aware API and renders unavailable/incomplete scans truthfully.

**Tech Stack:** Rust, Cargo workspace, in-module unit tests, Nix development shell, Nix flake/package checks

## Global Constraints

- Preserve the portable `ps -eo` columns, environment clearing, provider executable matchers, argv fallback, cwd/start identity, transcript assignment, Claude merge, and Antigravity projection.
- Preserve process-group termination, nonblocking reads, synchronous child reaping, and independent stdout/stderr bounds.
- Keep every terminal backend on the existing 64 KiB and 500 ms defaults.
- Process capture uses exactly 1 MiB per stream and 500 ms; overflow and timeout fail the complete snapshot.
- Doctor treats failed process discovery as a non-fatal advisory and never presents its provider counts as complete.
- Never expose raw process rows, argv, paths, or command errors in Doctor output.
- Keep the existing runtime `scan_agent_sessions_with_state` contract; do not add last-good process snapshots or stale-state lifecycle behavior.
- Generate large test fixtures in Rust and feed temporary files through portable Unix `cat`; do not use shell loops or giant command-line arguments.
- Resolve one portable `cat` executable consistently for runner and process fixtures, assert each fixture's exact byte size before spawn, and keep these behavioral tests under `#[cfg(unix)]`.
- Do not commit, push, publish, or close the parent Bead without explicit user authorization and installed Doctor acceptance.

---

### Task 1: Parameterize bounded capture without changing terminal defaults

**Files:**

- Modify: `crates/coding-brain-core/src/terminals/mod.rs:30`
- Modify: `crates/coding-brain-core/src/terminals/mod.rs:267-375`
- Test: `crates/coding-brain-core/src/terminals/mod.rs:3710-3760`

**Interfaces:**

- Produce crate-private `run_bounded_with(command: &mut Command, timeout: Duration, max_capture_bytes: usize) -> Result<BoundedOutput, String>` on Unix and the matching unsupported-platform stub.
- Preserve `run_bounded(command)` as a wrapper using `CAPTURE_TIMEOUT` and `MAX_CAPTURE_BYTES`.
- Pass the explicit byte ceiling into the stream reader; keep the error generic enough for both terminal and process callers.

**Acceptance Criteria:**

- Existing `run_bounded` callers and tests retain 64 KiB/500 ms behavior.
- The parameterized runner accepts a temporary-file payload above 64 KiB when given a larger limit.
- It rejects a payload one byte above its explicit limit.
- Its stderr stream independently accepts exactly its explicit limit and rejects limit-plus-one.
- Overflow errors are fixed and generic; they contain no captured bytes, argv, command lines, or caller-specific details.
- Timeout, nonzero exit, inherited-pipe, descendant cleanup, and process-group cleanup behavior remains unchanged.

- [ ] **Step 1: Add failing parameterized-limit tests**

Create Rust byte vectors for `64 * 1024 + 1`, exactly a small explicit limit, and that limit plus one. Assert each fixture's size before writing it to a `tempfile`. Resolve the same portable `cat` executable used by the process tests and stream the fixture through the child pipes. Assert the larger configured stdout bound succeeds, then redirect the same bounded fixture through stderr and prove exactly-at-limit succeeds while limit-plus-one fails. Retain the existing default-runner oversized test. Keep these tests under `#[cfg(unix)]`.

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core terminals::tests::bounded_command_runner_ -- --nocapture
```

Expected: the new tests fail to compile because `run_bounded_with` does not exist.

- [ ] **Step 2: Implement the smallest parameterized runner**

Thread `timeout` and `max_capture_bytes` through `run_bounded_with` and `read_available`. Keep `run_bounded` as the only default-policy wrapper. Preserve all cleanup paths and avoid changing terminal backend call sites.

- [ ] **Step 3: Verify bounded-runner behavior**

Run the focused command above. Expected: all bounded runner, timeout, and cleanup tests pass.

---

### Task 2: Give process snapshots their own bounded policy

**Files:**

- Modify: `crates/coding-brain-core/src/process.rs:1-100`
- Test: `crates/coding-brain-core/src/process.rs:430-475`

**Interfaces:**

- Add process-local constants for a 1 MiB stream limit and 500 ms deadline.
- Make `capture_process_snapshot_with` call `terminals::run_bounded_with` using those constants.
- Leave `ProcessSnapshot`, parsing, executable recognition, and public `capture_process_snapshot` behavior otherwise unchanged.

**Acceptance Criteria:**

- A valid generated `ps` fixture larger than 64 KiB and smaller than 1 MiB produces `succeeded: true` and recognized `.codex-wrapped` entries located before and after the old boundary.
- A generated fixture larger than 1 MiB produces `succeeded: false` with no entries.
- Successful empty, malformed rows, timeout, nonzero exit, and discovery-disable behavior remain fail-closed and distinguishable through `succeeded`.

- [ ] **Step 1: Replace the stale 70,000-byte expectation with failing boundary regressions**

Build syntactically valid `ps` rows in Rust, pad with valid unrelated rows, and write the result to a temporary file. Invoke `capture_process_snapshot_with(Command::new("cat").arg(path))`. Assert recognized Codex rows survive on both sides of 64 KiB. Add a separate fixture above 1 MiB and assert unsuccessful/empty.

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core process::tests::process_snapshot_command_ -- --nocapture
```

Expected: the above-64-KiB test fails because process capture still uses the terminal default.

- [ ] **Step 2: Apply the process-specific limit and deadline**

Add only the two policy constants and switch the process command to `run_bounded_with`. Do not change parsing or error projection.

- [ ] **Step 3: Verify process snapshot boundaries**

Run the focused command above plus:

```bash
nix develop path:. --command cargo test -p coding-brain-core process::tests -- --nocapture
```

Expected: both focused boundaries and all process parsing/identity tests pass.

---

### Task 3: Propagate snapshot availability through provider discovery

**Files:**

- Modify: `crates/coding-brain-core/src/discovery.rs:35-90`
- Modify: `crates/coding-brain-core/src/lib.rs:43-45`
- Test: `crates/coding-brain-core/src/discovery.rs:1110-1310`

**Interfaces:**

- Add public `ProviderSessionScan { pub sessions: Vec<AgentSession>, pub process_snapshot_succeeded: bool }`.
- Add `scan_agent_sessions_with_status(&mut ProviderDiscoveryState) -> ProviderSessionScan`.
- Change the internal runner seam to return `ProviderSessionScan` after exactly one process snapshot and one Claude inventory refresh.
- Keep `scan_agent_sessions_with_state` delegating to the status-aware function and returning only `.sessions`.
- Re-export the new result and function from `coding-brain-core` consistently with existing discovery exports.

**Acceptance Criteria:**

- Failed snapshot plus a surviving structured/stale Claude session returns that session with `process_snapshot_succeeded == false`.
- Successful empty snapshot returns no sessions with `process_snapshot_succeeded == true`.
- Normal merged discovery still takes one process snapshot, preserves sort order, and reports success.
- Existing list-returning callers compile and behave unchanged.
- Counter-based coverage proves both the status-aware path and compatibility wrapper cause exactly one process snapshot and one Claude inventory refresh.

- [ ] **Step 1: Convert runner-seam tests to assert status and add the failing partial-result case**

Update the one-snapshot merge test and failed/successful-empty tests to inspect `ProviderSessionScan`. First assert the new API and boolean before adding production types.

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core discovery::tests -- --nocapture
```

Expected: compile failure until the status-aware result exists.

- [ ] **Step 2: Implement the status-aware result and compatibility delegation**

Capture `snapshot.succeeded` before moving the derived session list into the result. Implement `scan_agent_sessions_with_state` as exactly one status-aware call followed by `.sessions`; avoid a second snapshot or inventory call. Keep all session derivation, transcript/cache side effects, and sorting unchanged.

- [ ] **Step 3: Verify provider discovery**

```bash
nix develop path:. --command cargo test -p coding-brain-core discovery::tests -- --nocapture
```

Expected: all Codex, Claude, Antigravity, transcript assignment, cache, and status tests pass.

---

### Task 4: Make Doctor distinguish unavailable from successful zero

**Files:**

- Modify: `src/doctor.rs:1748-1790`
- Test: `src/doctor.rs:3425-3460`

**Interfaces:**

- Make `check_session_discovery` call `scan_agent_sessions_with_status`.
- Change the local rendering seam to accept sessions plus process-snapshot availability, or accept `ProviderSessionScan` directly.
- Preserve existing successful-empty and successful-nonempty messages.
- On failed process scan, return `CheckStatus::Advisory`, a fixed message stating process-backed discovery is unavailable and provider counts may be incomplete, and a bounded retry hint.
- When failed discovery retains Claude inventory sessions, append only `partial Claude: N`; never render Codex or Antigravity zero counts from the failed process snapshot.

**Acceptance Criteria:**

- Failed process discovery never renders `Codex: 0, Claude: 0, Antigravity: 0` as authoritative.
- Failed discovery with a Claude result remains advisory and appends only `partial Claude: N`.
- Successful zero remains the existing advisory to start a selected provider session.
- Successful nonempty discovery remains a pass with stable provider counts and no private session IDs or argv.

- [ ] **Step 1: Add failing unavailable, partial, empty, and pass rendering tests**

Extend `discovery_check_reports_only_provider_counts` with explicit availability. Add failed-empty and failed-Claude-partial cases. Assert the empty failure has no provider counts, the partial case contains only `partial Claude: N`, and both exclude session IDs, process command text, and authoritative Codex/Antigravity zeroes.

```bash
nix develop path:. --command cargo test --bin cbrain doctor::tests::discovery_check_ -- --nocapture
```

Expected: new unavailable cases fail because the renderer accepts only a session slice.

- [ ] **Step 2: Implement truthful Doctor rendering**

Branch on availability before normal count status. Keep the failure advisory generic; do not pass command-runner errors into the `Check`.

- [ ] **Step 3: Verify Doctor regressions**

Run the focused command above, then:

```bash
nix develop path:. --command cargo test --bin cbrain doctor::tests -- --nocapture
```

Expected: all Doctor tests pass.

---

### Task 5: Document the operator-visible distinction

**Files:**

- Modify: `CHANGELOG.md:7-35`
- Modify: `docs/reference.md:105-120`
- Modify: `docs/troubleshooting.md:35-45`

**Acceptance Criteria:**

- `[Unreleased]` states that larger bounded process tables are supported and failed scans no longer masquerade as zero sessions.
- The reference states process discovery is bounded to 1 MiB per stream and distinguishes successful zero from unavailable/incomplete.
- Troubleshooting tells users that zero is authoritative only after a successful scan, while unavailable/incomplete means retry from the provider environment and inspect host load/process volume.
- Documentation does not claim transcript files create live sessions or that discovery is unbounded.

- [ ] **Step 1: Update the three approved documentation surfaces**

Keep the wording operator-focused and avoid architecture or migration additions.

- [ ] **Step 2: Review prose and scope**

```bash
rg -n "session discovery|process discovery|incomplete|1 MiB" CHANGELOG.md docs/reference.md docs/troubleshooting.md
git diff --check
```

Expected: the distinction is consistent and whitespace checks pass.

---

### Task 6: Run full verification and live acceptance

**Files:**

- Verify only: all changed source, test, design, plan, and documentation files

**Acceptance Criteria:**

- Focused regressions, full serial workspace tests, formatting, Clippy with warnings denied, and build pass.
- Core standalone tests pass.
- Nix evaluates all systems and the current-system package builds.
- A locally built `cbrain doctor --json`, run outside a PID sandbox on the affected host, reports at least one current Codex session from the approximately 191 KiB process table. This proves local source acceptance only.
- Final diff contains only approved paths, has no whitespace errors, and retains the pre-stress design stash.
- Installed Home Manager acceptance remains a separate post-merge step and is required before closing the parent Bead.

- [ ] **Step 1: Run Rust quality gates**

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test --workspace -- --test-threads=1
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
nix develop path:. --command cargo test -p coding-brain-core
```

Expected: every command exits zero.

- [ ] **Step 2: Run Nix evaluation and package gates**

```bash
nix flake check --all-systems --no-build
nix build --no-link --print-build-logs path:.#
```

Expected: evaluation and the current-system packaged build succeed. The storage-security VM and Home Manager module are unchanged and need not be rebuilt unless review reveals an affected contract.

- [ ] **Step 3: Verify the actual host regression outside PID isolation**

Build the debug binary, confirm the exact `ps` snapshot remains above 64 KiB, then run its JSON Doctor output with the user's real HOME/XDG environment. Assert `session discovery` is a pass with at least one Codex session, not an unavailable advisory or authoritative zero.

- [ ] **Step 4: Audit final scope and evidence**

```bash
git status --short
git diff --stat
git diff --check
git diff -- crates/coding-brain-core/src/terminals/mod.rs crates/coding-brain-core/src/process.rs crates/coding-brain-core/src/discovery.rs crates/coding-brain-core/src/lib.rs src/doctor.rs CHANGELOG.md docs/reference.md docs/troubleshooting.md .internal/specs/2026-08-12-bounded-process-session-discovery-design.md .internal/plans/2026-08-12-bounded-process-session-discovery.md
git stash list --format='%gd %s'
```

Expected: only approved files changed, no staged or whitespace errors, and the `codexctl-5n458.8 design stress-test restore point` stash remains present.

## Completion Boundary

After all tasks pass, request code review and run verification-before-completion. Report changed files, exact command evidence, Bead status, and the still-unperformed installed Home Manager acceptance. Local implementation may be reported complete after the real-host debug Doctor pass, but `codexctl-5n458.8` remains open. Do not commit, push, or create a PR without explicit user authorization. Close the parent Bead only after merge, Home Manager upgrade, and the user's installed `cbrain doctor` confirms nonzero Codex discovery.

## Stress Test Results

The approved implementation plan was challenged across seven branches:

1. Keep the configurable runner crate-private and its overflow errors generic and content-free.
2. Generate and size-check fixtures in Rust, then stream them through one portable `cat` path.
3. Test stdout and stderr limits independently, including exact-limit and limit-plus-one behavior.
4. Omit incomplete Codex and Antigravity zeroes; show only surviving Claude inventory as `partial Claude: N`.
5. Make the legacy discovery API one status-aware delegation and prove one process and one Claude scan.
6. Require Rust, standalone-core, Nix evaluation, and packaged-build gates; keep unrelated Home Manager and storage VM checks conditional on review evidence.
7. Separate local debug Doctor acceptance from post-merge installed Home Manager acceptance and Bead closure.

The final reflexion pass found no additional decision branch. It clarified that configurable-runner and large-fixture behavior is Unix-only, matching the existing implementation boundary; the non-Unix stub receives compile coverage rather than behavioral assertions.
