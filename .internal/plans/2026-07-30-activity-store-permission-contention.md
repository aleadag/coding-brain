# Activity-store Permission Contention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Prevent ordinary low-concurrency cross-project activity from exhausting permission-hook persistence while preserving coherent reads and bounded fail-closed audit behavior.

**Architecture:** Public activity reads copy one coherent byte snapshot under the existing shared lock and parse it after releasing the lock; transactional readers retain their existing exclusive critical section. A private 500 ms permission latency class applies to every blocking activity append in a permission hook, permission-triggered compaction stays non-blocking, and every non-permission store retains the 100 ms default.

**Tech Stack:** Rust 2024, `fs2` advisory file locks, `serde_json`, standard-library threads/channels/condition variables, Cargo workspace tests through the repository Nix development shell.

## Global Constraints

- Preserve durable audit-before-allow; executable decisions require successful proposal and terminal persistence before stdout.
- Preserve `flush`, `sync_data`, tail repair, durable compaction replacement, file modes, corruption diagnostics, and `codexctl-rcdi` atomic `Observed + Evaluating` rows.
- Keep `ActivityLimits::default().lock_timeout_ms` at exactly 100 ms.
- Use exactly 500 ms as a private permission-hook activity lock bound; do not add CLI or TOML configuration.
- Apply 500 ms to blocking initial, terminal, error, and delivery acquisitions by scoping the `ActivityStore` instance, not by duplicating append APIs; keep permission-triggered compaction non-blocking.
- Continuous unavailability remains bounded and fail-closed with the concrete `ActivityStoreError`.
- Keep TUI Busy/stale-refresh behavior and coherent Live/Review/Scorecard snapshots unchanged.
- Do not add a daemon, writer queue, process-local mutex, separate ledger, runtime feature flag, dependency, or state migration.
- Use channels, barriers, or condition variables with per-result deadlines; do not use unbounded thread joins as correctness evidence.

---

### Task 1: Release the public read lock before activity-log parsing

**Files:**
- Modify: `src/brain/activity.rs:1-675`

**Interfaces:**
- Consumes: existing `ActivityStore::read`, `ActivityStore::read_unlocked`, `ActivityLog`, `ActivityStoreError`, and `lock_with_timeout`.
- Produces: private `ActivityStore::read_bytes_unlocked() -> Result<Vec<u8>, ActivityStoreError>` and `parse_activity_log(&[u8]) -> Result<ActivityLog, ActivityStoreError>`.
- Produces for tests only: `ReadParseGate` and `ActivityStore::with_read_parse_gate(Arc<ReadParseGate>)`.

**Acceptance Criteria:**
- Public `ActivityStore::read` holds the shared lock only while capturing an owned coherent byte snapshot and parses after dropping the guard.
- Compaction, recovery reservation, append-if-absent, and snapshot-dependent append paths continue parsing while their caller-owned exclusive guard remains held.
- A deterministic roughly 23.4 MB / 32,448-row regression pauses public parsing and proves a writer can durably append before parsing resumes.
- The paused reader returns exactly its captured pre-append snapshot; a later read includes the append.
- Existing malformed-row, legacy-kind, duplicate-terminal, repair, compaction, and lock-timeout behavior remains unchanged.

- [ ] **Step 1: Add a test-only gate that can pause public parsing deterministically**

Add `Condvar` and `Mutex` to the existing `#[cfg(test)]` synchronization imports and define this test-only helper beside `LockGuard`:

```rust
#[cfg(test)]
#[derive(Default)]
pub(crate) struct ReadParseGate {
    state: Mutex<ReadParseGateState>,
    changed: Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct ReadParseGateState {
    reached: bool,
    released: bool,
}

#[cfg(test)]
impl ReadParseGate {
    fn pause(&self) {
        let mut state = self.state.lock().unwrap();
        state.reached = true;
        self.changed.notify_all();
        while !state.released {
            state = self.changed.wait(state).unwrap();
        }
    }

    pub(crate) fn wait_until_reached(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| !state.reached)
            .unwrap();
        state.reached
    }

    pub(crate) fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.released = true;
        self.changed.notify_all();
    }
}
```

Add a `#[cfg(test)] read_parse_gate: Option<Arc<ReadParseGate>>` field to `ActivityStore`, initialize it to `None` in `ActivityStore::at`, and add:

```rust
#[cfg(test)]
pub(crate) fn with_read_parse_gate(mut self, gate: Arc<ReadParseGate>) -> Self {
    self.read_parse_gate = Some(gate);
    self
}
```

- [ ] **Step 2: Write the failing realistic-size lock-scope regression**

In `src/brain/activity.rs` tests, add a helper that creates 32,448 valid rows in one `append_batch` call. Pad `reasoning` to 512 bytes and assert the resulting fixture is at least 20 MiB so the test cannot silently regress to an empty-log fixture:

```rust
fn realistically_sized_events() -> Vec<ActivityEvent> {
    (0..32_448)
        .map(|index| {
            let mut event = event_at(
                &format!("scale-{index}"),
                ActivityState::Denied,
                index,
            );
            event.reasoning = Some("x".repeat(512));
            event
        })
        .collect()
}

#[test]
fn public_read_releases_lock_before_parsing_captured_snapshot() {
    let (root, store) = fixture_store();
    store.append_batch(&realistically_sized_events()).unwrap();
    assert!(fs::metadata(root.path().join("activity.jsonl")).unwrap().len() >= 20 * 1024 * 1024);

    let gate = Arc::new(ReadParseGate::default());
    let reader = store.clone().with_read_parse_gate(Arc::clone(&gate));
    let (read_tx, read_rx) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        read_tx.send(reader.read()).unwrap();
    });

    assert!(gate.wait_until_reached(Duration::from_secs(5)));
    store
        .append(event_at("after-capture", ActivityState::Denied, 40_000))
        .unwrap();
    gate.release();

    let captured = read_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    handle.join().unwrap();
    assert_eq!(captured.events().len(), 32_448);
    assert!(!captured.events().iter().any(|event| event.activity_id == "after-capture"));
    assert!(store
        .read()
        .unwrap()
        .events()
        .iter()
        .any(|event| event.activity_id == "after-capture"));
}
```

- [ ] **Step 3: Run the new test and verify the current lock scope fails**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  brain::activity::tests::public_read_releases_lock_before_parsing_captured_snapshot \
  -- --exact
```

Expected: FAIL before the refactor because the paused public reader still owns the shared guard and `append` returns `ActivityStoreError::LockTimeout`.

- [ ] **Step 4: Split byte capture from parsing and scope the public guard**

Replace `read_unlocked` with a byte-capture helper plus the existing parser body:

```rust
fn read_bytes_unlocked(&self) -> Result<Vec<u8>, ActivityStoreError> {
    let mut contents = Vec::new();
    match File::open(&self.path) {
        Ok(mut file) => {
            file.read_to_end(&mut contents)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(contents)
}

fn read_unlocked(&self) -> Result<ActivityLog, ActivityStoreError> {
    let contents = self.read_bytes_unlocked()?;
    parse_activity_log(&contents)
}
```

Move the unchanged row parsing, schema checks, diagnostic accounting, legacy lifecycle-kind handling, and duplicate-terminal calculation into:

```rust
fn parse_activity_log(contents: &[u8]) -> Result<ActivityLog, ActivityStoreError> {
    let mut log = ActivityLog::default();
    let mut activity_kinds = HashMap::<String, ActivityKind>::new();
    let mut offset = 0_u64;
    for raw_line in contents.split_inclusive(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
        if !line.is_empty() {
            if let Ok(event) = serde_json::from_slice::<ActivityEvent>(line) {
                let kind_was_absent = serde_json::from_slice::<serde_json::Value>(line)?
                    .get("kind")
                    .is_none();
                let mut event = event;
                if kind_was_absent && event.activity_id.starts_with("lifecycle_") {
                    event.kind = ActivityKind::Lifecycle;
                }
                if !supported_activity_schema(event.schema_version)
                    || !event.has_consistent_payload()
                    || activity_kinds
                        .get(&event.activity_id)
                        .is_some_and(|kind| *kind != event.kind)
                {
                    record_malformed(&mut log.diagnostics, offset);
                } else {
                    activity_kinds.insert(event.activity_id.clone(), event.kind);
                    log.events.push(event);
                }
            } else if let Ok(row) = serde_json::from_slice::<DiagnosticRow>(line) {
                apply_diagnostic(&mut log.diagnostics, row);
            } else {
                record_malformed(&mut log.diagnostics, offset);
            }
        }
        offset = offset.saturating_add(raw_line.len() as u64);
    }
    log.diagnostics.duplicate_terminal_states = duplicate_terminal_count(&log.events);
    Ok(log)
}
```

Change only public `read` to drop the shared guard before parsing:

```rust
pub fn read(&self) -> Result<ActivityLog, ActivityStoreError> {
    let contents = {
        let lock = self.open_lock()?;
        let _guard =
            lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Shared)?;
        self.read_bytes_unlocked()?
    };
    #[cfg(test)]
    if let Some(gate) = &self.read_parse_gate {
        gate.pause();
    }
    parse_activity_log(&contents)
}
```

Do not change callers of `read_unlocked`; their existing exclusive guard must continue covering both capture and parsing.

- [ ] **Step 5: Run focused activity-store tests**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain brain::activity::tests
```

Expected: all activity-store unit tests PASS, including the new 32,448-row lock-scope regression, `lock_wait_is_bounded_and_busy_compaction_skips`, tail repair, and compaction cases.

- [ ] **Step 6: Format and inspect the Task 1 diff**

Run:

```bash
nix develop path:. --command cargo fmt --all
git diff --check
git diff -- src/brain/activity.rs
```

Expected: formatting and whitespace checks pass; the diff contains only the read capture/parser split and test-only synchronization/fixture.

- [ ] **Step 7: Commit the independently revertible read-lock change**

After explicit commit authorization, run:

```bash
git add src/brain/activity.rs
git commit -m "⚡️ perf: release activity lock before parsing (codexctl-3a4i)"
```

Expected: one commit containing only Task 1.

---

### Task 2: Apply bounded permission activity persistence and verify both contention paths

**Files:**
- Modify: `src/brain/activity.rs:187-217`
- Modify: `src/brain/permission_hook.rs:20-30`
- Modify: `src/brain/permission_hook.rs:517-565`
- Modify: `src/brain/permission_hook.rs:1910-1990`
- Modify: `src/brain/permission_hook.rs:2600-2735`

**Interfaces:**
- Consumes: Task 1 `ReadParseGate`, `ActivityStore::with_read_parse_gate`, and public parsing outside the lock.
- Produces: `ActivityStore::with_lock_timeout_ms(u64) -> Self`.
- Produces: private `PERMISSION_ACTIVITY_LOCK_TIMEOUT_MS: u64 = 500`.
- Changes: `run_provider_with_gate_and_stores_and_safety` clones and scopes its provided `ActivityStore` to the permission timeout before any blocking activity append.

**Acceptance Criteria:**
- Every blocking activity-store acquisition in one permission-hook invocation uses exactly 500 ms; permission-triggered compaction remains non-blocking; lifecycle/TUI/recovery stores retain exactly 100 ms.
- One transient cross-project reader on a realistically sized log cannot block the permission lifecycle while its parsing is paused.
- One exclusive holder retained beyond 100 ms but released before 500 ms reaches inference and persists `Observed -> Evaluating -> Allowed -> Delivered`.
- A continuously held lock makes no model call, emits no Codex allow, reports `activity store lock timed out`, reaches `NeedsInput`, and completes within a two-second per-result deadline.
- The initial-failure path may make the existing second diagnostic append attempt; no acquisition retries indefinitely.
- The 15-request `codexctl-rcdi` burst remains coherent and the default store timeout remains 100 ms.

- [ ] **Step 1: Write failing tests for the private latency class**

Add a builder unit test in `src/brain/activity.rs`:

```rust
#[test]
fn lock_timeout_builder_changes_only_the_derived_store() {
    let (_root, store) = fixture_store();
    let permission = store.clone().with_lock_timeout_ms(500);
    assert_eq!(store.limits.lock_timeout_ms, 100);
    assert_eq!(permission.limits.lock_timeout_ms, 500);
}
```

In `src/brain/permission_hook.rs`, update
`locked_activity_store_fails_closed_with_specific_bounded_diagnostic` to use a
result channel and a two-second deadline. Keep the lock held until the result
arrives, then assert:

```rust
assert!(stdout.is_empty());
assert_eq!(calls.load(Ordering::SeqCst), 0);
assert!(String::from_utf8(stderr)
    .unwrap()
    .contains("activity store lock timed out"));
assert_eq!(projected_status(&lifecycle), Some(ProjectedStatus::NeedsInput));
```

- [ ] **Step 2: Write the failing transient-exclusive-holder regression**

Add `transient_cross_project_writer_outlasting_default_bound_reaches_normal_permission_decision`.
Acquire the activity lock before starting the worker, signal immediately before
the worker enters the permission path, and use the result channel itself to
hold beyond the old bound without an unbounded join:

```rust
let lock = OpenOptions::new()
    .create(true)
    .read(true)
    .write(true)
    .truncate(false)
    .open(activity_path.with_extension("lock"))
    .unwrap();
lock.lock_exclusive().unwrap();

let (started_tx, started_rx) = mpsc::sync_channel(0);
let (result_tx, result_rx) = mpsc::sync_channel(1);
let worker = std::thread::spawn(move || {
    started_tx.send(()).unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    run_with_gate_and_stores(
        Cursor::new(payload()),
        &mut stdout,
        &mut stderr,
        Some(&enabled_config()),
        BrainGateMode::Auto,
        &lifecycle_for_worker,
        Some(&activity_for_worker),
        |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
    );
    result_tx.send((stdout, stderr)).unwrap();
});

started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
assert!(result_rx.recv_timeout(Duration::from_millis(200)).is_err());
FileExt::unlock(&lock).unwrap();
let (stdout, stderr) = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
worker.join().unwrap();
```

Assert empty stderr, Codex `allow`, one inference call, and the exact four-state lifecycle.

- [ ] **Step 3: Write the failing large-log reader-overlap permission regression**

Add `large_log_reader_parsing_does_not_block_permission_lifecycle`. Reuse Task
1's `realistically_sized_events` logic through a local helper in the permission
tests, write all 32,448 rows through one `append_batch`, then pause a reader
through `ReadParseGate`. While parsing remains paused, run one Codex permission
request and assert it completes normally before releasing the reader:

```rust
let gate = Arc::new(ReadParseGate::default());
let reader = activity
    .clone()
    .with_read_parse_gate(Arc::clone(&gate));
let (read_tx, read_rx) = mpsc::sync_channel(1);
let reader_thread = std::thread::spawn(move || {
    read_tx.send(reader.read()).unwrap();
});
assert!(gate.wait_until_reached(Duration::from_secs(5)));

run_with_gate_and_stores(
    Cursor::new(payload()),
    &mut stdout,
    &mut stderr,
    Some(&enabled_config()),
    BrainGateMode::Auto,
    &lifecycle,
    Some(&activity),
    |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
);

gate.release();
read_rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
reader_thread.join().unwrap();
```

Assert empty stderr, Codex `allow`, and exactly
`Observed, Evaluating, Allowed, Delivered` for the new permission activity ID.

- [ ] **Step 4: Run the new permission regressions and verify they fail on the 100 ms path**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  brain::permission_hook::tests::transient_cross_project_writer_outlasting_default_bound_reaches_normal_permission_decision \
  -- --exact
nix develop path:. --command cargo test --bin cbrain \
  brain::permission_hook::tests::large_log_reader_parsing_does_not_block_permission_lifecycle \
  -- --exact
```

Expected before the permission policy change: the transient-holder test FAILS because the worker returns a lock-timeout result within the 200 ms observation window. The reader-overlap test passes only after Task 1 and protects the combined production path.

- [ ] **Step 5: Implement the private builder and permission-scoped store**

Add this builder to `ActivityStore` without changing `ActivityLimits::default`:

```rust
pub(crate) fn with_lock_timeout_ms(mut self, lock_timeout_ms: u64) -> Self {
    self.limits.lock_timeout_ms = lock_timeout_ms;
    self
}
```

Add near the permission-hook constants:

```rust
const PERMISSION_ACTIVITY_LOCK_TIMEOUT_MS: u64 = 500;
```

At the beginning of `run_provider_with_gate_and_stores_and_safety`, derive and
shadow the permission-scoped store before parsing or persisting any activity:

```rust
let permission_activity_store = activity_store.cloned().map(|store| {
    store.with_lock_timeout_ms(PERMISSION_ACTIVITY_LOCK_TIMEOUT_MS)
});
let activity_store = permission_activity_store.as_ref();
```

Leave all existing `append`, `append_batch`, error compensation, delivery
append, and `compact_if_needed` call sites unchanged so every permission
activity operation uses the same derived store. Do not change decision-proposal
or lifecycle-store lock policies.

- [ ] **Step 6: Run all focused contention and permission tests**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain brain::activity::tests
nix develop path:. --command cargo test --bin cbrain brain::permission_hook::tests
nix develop path:. --command cargo test --test activity_scale
nix develop path:. --command cargo test -p coding-brain-tui brain_app
```

Expected: all focused suites PASS. Confirm specifically that the continuous-lock test finishes within its two-second deadline, the transient holder reaches allow, the large-log reader overlap completes, and `parallel_codex_permission_burst_preserves_complete_initial_lifecycles` retains complete four-state lifecycles.

- [ ] **Step 7: Run the complete repository quality gates serially**

Run:

```bash
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo build --workspace
git diff --check
```

Expected: every command exits 0. Run Cargo/Nix gates serially to avoid shared target and Nix-store contention.

- [ ] **Step 8: Inspect scope and commit the permission policy**

Run:

```bash
git status --short
git diff --stat
git diff -- src/brain/activity.rs src/brain/permission_hook.rs
```

Expected: Task 2 changes only the private timeout builder, permission-scoped store construction, and focused regressions; the already committed design spec plus stress-test update and plan are reported separately.

After explicit commit authorization, run:

```bash
git add src/brain/activity.rs src/brain/permission_hook.rs
git commit -m "🐛 fix: tolerate permission activity contention (codexctl-3a4i)"
```

Expected: one implementation commit containing only Task 2.

- [ ] **Step 9: Final tracker and documentation audit**

Compare the final behavior against
`.internal/specs/2026-07-30-activity-store-permission-contention-design.md`.
No user-facing configuration or operational command changes are expected, so
README/configuration/troubleshooting edits are unnecessary unless implementation
changes that conclusion.

Run:

```bash
bd -C /home/alexander/.beads-planning show codexctl-3a4i
git status --short
```

Expected: all acceptance criteria have fresh test evidence, no unrelated files
are modified, and `codexctl-3a4i` is ready to close. Do not push or publish.
