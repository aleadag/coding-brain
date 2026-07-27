# Deflake Live Refresh Contention Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the Live refresh lock-contention regression deterministic under CI load without changing the production 100 ms activity-store timeout.

**Architecture:** Move the store-dependent refresh logic into a private associated helper. The production `BrainSource::refresh` path constructs the same default `ActivityStore`; only the expected-success test phase supplies a store with a 5,000 ms test-local timeout.

**Tech Stack:** Rust 2024, `fs2` file locks, `std::sync::mpsc`, Cargo test tooling

## Global Constraints

- Keep the production activity-store lock timeout at 100 ms.
- Preserve explicit coverage of short-overlap success, `BrainSourceError::Busy`, and post-unlock recovery.
- Change production/test code only in `src/runtime/brain.rs`; do not alter public APIs or persisted formats.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Make the Live refresh contention test deterministic

**Files:**

- Modify: `src/runtime/brain.rs`
- Test: `src/runtime/brain.rs`

**Interfaces:**

- Consumes: `brain::activity::ActivityStore`, `SnapshotLimits`, and the existing `BrainRefresh`/`BrainSourceError` runtime contract.
- Produces: private `LiveBrainSource::refresh_from_store(store, limits) -> Result<BrainRefresh, BrainSourceError>`.

**Acceptance Criteria:**

- The expected-success contention phase uses a 5,000 ms test-local lock timeout and explicit unlocker readiness synchronization.
- The normal `LiveBrainSource::refresh` path still uses `ActivityLimits::default()`, whose lock timeout remains 100 ms.
- Holding the normal activity lock through its timeout still returns `BrainSourceError::Busy`.
- A normal refresh succeeds after the held lock is released.
- The focused test passes 50 consecutive runs, and the binary/full workspace checks pass.

- [ ] **Step 1: Change the test first to require the private store helper**

Replace the first contention phase after `let source = LiveBrainSource::default();`
with:

```rust
let success_store =
    brain::activity::ActivityStore::at(state_root.join("activity.jsonl")).with_limits(
        brain::activity::ActivityLimits {
            lock_timeout_ms: 5_000,
            ..brain::activity::ActivityLimits::default()
        },
    );
let (unlocker_ready_tx, unlocker_ready_rx) = std::sync::mpsc::channel();
let unlocker = std::thread::spawn(move || {
    unlocker_ready_tx.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(25));
    FileExt::unlock(&lock).unwrap();
});
unlocker_ready_rx.recv().unwrap();
assert!(
    LiveBrainSource::refresh_from_store(success_store, SnapshotLimits::default()).is_ok()
);
unlocker.join().unwrap();
```

Leave the later `source.refresh(...)` Busy and recovery assertions unchanged.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
direnv exec . cargo test --bin coding-brain runtime::brain::tests::live_brain_refresh_reports_busy_during_activity_lock_contention -- --exact
```

Expected: compilation fails with `E0599` because
`LiveBrainSource::refresh_from_store` does not exist.

- [ ] **Step 3: Extract the minimal private helper**

Add this associated helper to `impl LiveBrainSource`:

```rust
fn refresh_from_store(
    store: brain::activity::ActivityStore,
    limits: SnapshotLimits,
) -> Result<BrainRefresh, BrainSourceError> {
    let activity = store.read().map_err(|error| match error {
        brain::activity::ActivityStoreError::LockTimeout => BrainSourceError::Busy,
        other => BrainSourceError::Other(other.to_string()),
    })?;
    let records = brain::decisions::read_learning_decisions_from_activity(activity.events());
    let decisions = records
        .iter()
        .map(DecisionSummary::from)
        .collect::<Vec<_>>();

    Ok(BrainRefresh {
        snapshot: store.project_snapshot(&activity, limits),
        review_queue: review_queue_from(records, activity.events()),
        scorecard: scorecard_from(&decisions, activity.events()),
    })
}
```

Reduce `BrainSource::refresh` to path resolution plus delegation:

```rust
fn refresh(&self, limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError> {
    let paths = brain::distill::current_paths()
        .map_err(|error| BrainSourceError::Other(error.to_string()))?;
    Self::refresh_from_store(
        brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl")),
        limits,
    )
}
```

- [ ] **Step 4: Verify GREEN and repeat the regression**

Run the focused test once:

```bash
direnv exec . cargo test --bin coding-brain runtime::brain::tests::live_brain_refresh_reports_busy_during_activity_lock_contention -- --exact
```

Expected: one test passes.

Then run it 50 consecutive times:

```bash
direnv exec . zsh -c 'for run in {1..50}; do cargo test --quiet --bin coding-brain runtime::brain::tests::live_brain_refresh_reports_busy_during_activity_lock_contention -- --exact || exit 1; done'
```

Expected: all 50 invocations exit successfully.

- [ ] **Step 5: Run the relevant and repository-wide checks**

Run:

```bash
direnv exec . cargo test --bin coding-brain runtime::brain::tests
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo fmt --check
```

Expected: every command exits with status 0 and no test, lint, or formatting failures.

- [ ] **Step 6: Review and hand off without committing**

Run:

```bash
git diff --check
git diff -- src/runtime/brain.rs
git status --short
```

Confirm every changed code line maps to `codexctl-g9nj`, record the fresh
verification evidence on the issue, close it only if all acceptance criteria
are satisfied, and report that commit/push remain awaiting authorization.
