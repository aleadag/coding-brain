# Activity Store Concurrency Test Deflake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> beads-superpowers:subagent-driven-development (recommended) or
> beads-superpowers:executing-plans to implement this plan task-by-task. Each
> Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within
> tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the concurrent snapshot idempotency regression test exercise
lock contention deterministically without racing the production 100 ms lock
timeout against host fsync latency.

**Architecture:** Change only the existing unit test in
`src/brain/activity.rs`. A designated first writer signals after entering its
snapshot builder, then holds the exclusive lock long enough for the second
writer to contend; this test fixture receives a five-second lock timeout while
production defaults and the bounded-lock test remain unchanged.

**Tech Stack:** Rust standard-library threads and channels, `fs2` file locking,
Cargo test tooling, Nix development shell.

## Global Constraints

- Do not change `ActivityLimits::default()` or production locking code.
- Preserve exactly two marker events and one outcome event in the final log.
- Keep `lock_wait_is_bounded_and_busy_compaction_skips` on production defaults.
- Modify only `src/brain/activity.rs` plus the approved design and plan files.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Make the concurrent idempotency test deterministic

**Files:**

- Modify: `src/brain/activity.rs:1222`
- Test: `src/brain/activity.rs:1222`

**Interfaces:**

- Consumes: `ActivityStore::with_limits(ActivityLimits)` and
  `ActivityStore::append_from_snapshot(FnOnce(&ActivityLog) ->
  Vec<ActivityEvent>)`.
- Produces: no new interface; only a deterministic regression test.

**Acceptance Criteria:**

- The test deliberately overlaps two `append_from_snapshot` calls under
  exclusive lock contention.
- The test uses an explicit test-only timeout large enough for the synchronized
  writer to wait.
- The activity log contains both markers and exactly one outcome.
- The production default timeout and bounded-lock test remain unchanged.
- The focused test passes repeatedly and all Rust quality gates pass.

- [ ] **Step 1: Add deterministic contention while retaining the default timeout**

Replace the barrier-based writer setup with a designated first writer. The
first snapshot builder sends on a channel after acquiring the store lock and
holds that lock for 200 ms. Start the second writer only after receiving that
signal:

```rust
let store = std::sync::Arc::new(store);
let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel();

let first_store = store.clone();
let first = std::thread::spawn(move || {
    let marker = event_at("marker-0", ActivityState::Observed, 2);
    let mut outcome = event_at("target", ActivityState::Outcome, 4);
    outcome.outcome = Some(ActivityOutcome::Completed);
    first_store
        .append_from_snapshot(|log| {
            lock_held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let mut rows = vec![marker];
            if !log.events().iter().any(|event| {
                event.activity_id == "target" && event.state == ActivityState::Outcome
            }) {
                rows.push(outcome);
            }
            rows
        })
        .unwrap();
});

lock_held_rx.recv().unwrap();
let second_store = store.clone();
let second = std::thread::spawn(move || {
    let marker = event_at("marker-1", ActivityState::Observed, 3);
    let mut outcome = event_at("target", ActivityState::Outcome, 5);
    outcome.outcome = Some(ActivityOutcome::Completed);
    second_store
        .append_from_snapshot(|log| {
            let mut rows = vec![marker];
            if !log.events().iter().any(|event| {
                event.activity_id == "target" && event.state == ActivityState::Outcome
            }) {
                rows.push(outcome);
            }
            rows
        })
        .unwrap();
});

first.join().unwrap();
second.join().unwrap();
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
nix develop path:. --command cargo test --lib \
  brain::activity::tests::append_from_snapshot_serializes_concurrent_idempotency_checks \
  -- --exact --nocapture
```

Expected: FAIL because the second writer returns
`ActivityStoreError::LockTimeout` after 100 ms while the first writer still
holds the lock.

- [ ] **Step 3: Give only this fixture a contention-safe timeout**

Configure the store before placing it in `Arc`:

```rust
let store = store.with_limits(ActivityLimits {
    lock_timeout_ms: 5_000,
    ..ActivityLimits::default()
});
let store = std::sync::Arc::new(store);
```

Do not change `ActivityLimits::default()` or
`lock_wait_is_bounded_and_busy_compaction_skips`.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run the focused command from Step 2.

Expected: PASS with two marker events and exactly one outcome event.

- [ ] **Step 5: Stress the focused test**

Run:

```bash
nix develop path:. --command sh -c 'for _iteration in $(seq 1 25); do
  cargo test --lib \
    brain::activity::tests::append_from_snapshot_serializes_concurrent_idempotency_checks \
    -- --exact || exit 1
done'
```

Expected: 25 consecutive passes.

- [ ] **Step 6: Run the full Rust test suite**

Run:

```bash
nix develop path:. --command cargo test --all-targets
```

Expected: all targets pass with no failed tests.

- [ ] **Step 7: Run formatting and lint gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy --all-targets -- -D warnings
```

Expected: both commands exit successfully with no formatting diff or Clippy
warnings.

- [ ] **Step 8: Prepare the conservative handoff**

Run:

```bash
git diff --check
git status --short
```

Expected: only `src/brain/activity.rs`, the approved design, and this plan are
changed. Leave them uncommitted pending explicit user authorization.
