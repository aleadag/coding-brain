# Darwin ARM64 Lock Test Deflake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the activity-store lock contention regression deterministic on macOS ARM64 without changing the production 100 ms lock timeout.

**Architecture:** Keep `lock_with_timeout` and `ActivityLimits::default()` unchanged. Change only the test harness: run the contended append in a worker and receive its result through a channel with a five-second test-only deadline, then retain the existing compaction-under-contention assertion.

**Tech Stack:** Rust standard-library threads and MPSC channels, Cargo test, Nix development shell.

## Global Constraints

- Keep the production activity-store lock timeout at 100 ms.
- Preserve explicit `LockTimeout` coverage.
- Preserve the assertion that compaction skips while the lock is held.
- Make no changes outside the focused test and its design/plan documentation.
- Do not commit or publish without explicit user authorization.

---

### Task 1: Deflake the activity-store lock contention regression

**Files:**

- Modify: `src/brain/activity.rs:1884`
- Test: `src/brain/activity.rs:1884`

**Interfaces:**

- Consumes: `ActivityStore::append(ActivityEvent) -> Result<(), ActivityStoreError>` and `std::sync::mpsc::Receiver::recv_timeout(Duration)`.
- Produces: deterministic coverage that a contended append returns `ActivityStoreError::LockTimeout` within a five-second test-only deadline.

**Acceptance Criteria:**

- The contended append result is received within five seconds and matches `Err(ActivityStoreError::LockTimeout)`.
- The worker thread is joined before the test continues.
- `compact_if_needed()` still returns `false` while the lock is held.
- `ActivityLimits::default().lock_timeout_ms` remains 100.
- The focused regression passes repeatedly and the full workspace quality gates pass.

- [ ] **Step 1: Confirm the existing regression evidence**

Use the reopened `codexctl-fge` evidence from GitHub Actions run `30328927800`,
job `90179886065`: the existing test fails at the 125 ms elapsed-time
assertion on `macos-26-arm64` even though the branch does not change
`src/brain/activity.rs`.

Run the current test locally to establish that it is load-dependent:

```bash
nix develop path:. --command cargo test brain::activity::tests::lock_wait_is_bounded_and_busy_compaction_skips -- --exact --nocapture
```

Expected: PASS locally in both binary test targets; the remote ARM64 failure is
the failing regression evidence.

- [ ] **Step 2: Replace the scheduler-sensitive elapsed assertion**

In `lock_wait_is_bounded_and_busy_compaction_skips`, replace:

```rust
let started = Instant::now();
assert!(matches!(
    store.append(event("a2", ActivityState::Denied)),
    Err(ActivityStoreError::LockTimeout)
));
assert!(
    started.elapsed()
        < Duration::from_millis(store.limits.lock_timeout_ms.saturating_add(25))
);
```

with:

```rust
let (result_tx, result_rx) = std::sync::mpsc::channel();
let append_store = store.clone();
let append = std::thread::spawn(move || {
    result_tx
        .send(append_store.append(event("a2", ActivityState::Denied)))
        .unwrap();
});
assert!(matches!(
    result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
    Err(ActivityStoreError::LockTimeout)
));
append.join().unwrap();
```

Do not change `lock_with_timeout`, `LOCK_RETRY`, or `ActivityLimits::default()`.

- [ ] **Step 3: Run the focused regression repeatedly**

```bash
for _ in $(seq 1 50); do
  nix develop path:. --command cargo test brain::activity::tests::lock_wait_is_bounded_and_busy_compaction_skips -- --exact --quiet
done
```

Expected: all 50 iterations pass in both binary test targets.

- [ ] **Step 4: Run relevant and full quality gates**

```bash
nix develop path:. --command cargo test brain::activity::tests
nix develop path:. --command cargo test
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo fmt --check
```

Expected: every command exits 0 with no test failures, Clippy warnings, or
formatting differences.

- [ ] **Step 5: Verify the surgical diff**

```bash
git diff --check
git diff -- src/brain/activity.rs .internal/specs/2026-07-28-darwin-arm64-lock-test-deflake-design.md .internal/plans/2026-07-28-darwin-arm64-lock-test-deflake.md
git status --short
```

Expected: the only product-source change is the focused test harness in
`src/brain/activity.rs`; the design and plan documents are the only additional
files. Do not commit or publish.
