# Coherent Scorecard Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Refresh Live, Review, and Scorecard from one bounded activity-store read while retaining the last coherent view during lock contention.

**Architecture:** Replace the three independent `BrainSource` view methods with one typed bundled refresh. `LiveBrainSource` reads activity once, then decisions once, and derives all three projections from those records; `BrainApp` applies the bundle atomically and treats lock contention as informational stale data.

**Tech Stack:** Rust 2024 workspace, `fs2` file locks, Ratatui TUI state, Cargo unit tests.

## Global Constraints

- Keep `ActivityLimits::default().lock_timeout_ms` at exactly `100`.
- Keep the TUI refresh interval at exactly one second.
- Add no retry loop, cache, background worker, dependency, schema, config, hook, or persistence change.
- Promise one coherent activity snapshot, not a transaction across the activity and decision stores.
- Read activity before decisions to preserve the proposal-before-terminal causal order.
- Preserve corruption diagnostics, tail handling, idempotent appends, compaction, and exclusive writer behavior.
- Bound and redact non-contention source errors before displaying them.
- Do not update user documentation; the visible feature and refresh cadence are unchanged.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Bundle Runtime Projections Behind One Activity Read

**Files:**
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: `src/brain/activity.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/runtime/brain.rs`

**Interfaces:**
- Produces: `BrainRefresh { snapshot, review_queue, scorecard }`.
- Produces: `BrainSourceError::{Busy, Other(String)}` with a `Display` implementation.
- Produces: `BrainSource::refresh(SnapshotLimits) -> Result<BrainRefresh, BrainSourceError>`.
- Produces: `ActivityStore::project_snapshot(&ActivityLog, SnapshotLimits) -> ActivitySnapshot` as a crate-local projection over a borrowed log.
- Produces: `read_learning_decisions_from_activity(&[ActivityEvent]) -> Vec<DecisionRecord>`.
- Consumes: existing `review_queue_from`, `scorecard_from`, `filter_learning_decisions`, and activity-store parsing.

**Acceptance Criteria:**
- A production refresh performs one bounded activity-store read and one decision-record read.
- Live, Review, and Scorecard are derived from the same captured activity events.
- Only `ActivityStoreError::LockTimeout` becomes `BrainSourceError::Busy`.
- A held production activity lock deterministically produces `Busy`; releasing it lets the next refresh succeed.
- Existing correction, corruption, idempotency, compaction, and bounded-lock behavior remains unchanged.

- [ ] **Step 1: Add a failing core contract test**

In `crates/coding-brain-core/src/runtime.rs`, replace the mock’s three independent view methods with a test that expects one bundle:

```rust
#[test]
fn mock_source_returns_one_refresh_bundle() {
    let mock = MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            unresolved_count: 2,
            ..ActivitySnapshot::default()
        },
        review_queue: Vec::new(),
        scorecard: ScorecardSummary {
            total_decisions: 3,
            ..ScorecardSummary::default()
        },
        ..MockBrainRuntime::default()
    };

    let refresh = mock.refresh(SnapshotLimits::default()).unwrap();

    assert_eq!(refresh.snapshot.unresolved_count, 2);
    assert!(refresh.review_queue.is_empty());
    assert_eq!(refresh.scorecard.total_decisions, 3);
}
```

- [ ] **Step 2: Run the core test and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-core mock_source_returns_one_refresh_bundle -- --exact
```

Expected: compilation fails because `BrainRefresh`, `BrainSourceError`, and `BrainSource::refresh` do not exist.

- [ ] **Step 3: Implement the bundled core contract**

In `crates/coding-brain-core/src/runtime.rs`, add:

```rust
#[derive(Debug, Clone, Default)]
pub struct BrainRefresh {
    pub snapshot: ActivitySnapshot,
    pub review_queue: Vec<ReviewItemSummary>,
    pub scorecard: ScorecardSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainSourceError {
    Busy,
    Other(String),
}

impl std::fmt::Display for BrainSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("brain data is busy"),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

pub trait BrainSource: Send + Sync {
    fn refresh(&self, limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError>;
    fn gate_mode(&self) -> BrainGateMode;
    fn endpoint_health(&self) -> EndpointHealth;
}
```

Update `MockBrainRuntime` to return cloned fields in one `BrainRefresh`. Update the temporary `ErrorAfterFirstSource` test implementation in `crates/coding-brain-tui/src/brain_app.rs` only far enough to compile; Task 2 will replace its behavior-focused tests.

Use this exact replacement for that test source:

```rust
impl BrainSource for ErrorAfterFirstSource {
    fn refresh(
        &self,
        _limits: SnapshotLimits,
    ) -> Result<BrainRefresh, BrainSourceError> {
        if self.snapshot_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(BrainRefresh {
                snapshot: ActivitySnapshot {
                    attention: vec![AttentionItem {
                        activity: activity(),
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    }],
                    unresolved_count: 1,
                    ..ActivitySnapshot::default()
                },
                ..BrainRefresh::default()
            })
        } else {
            Err(BrainSourceError::Other("source failed".into()))
        }
    }

    fn gate_mode(&self) -> BrainGateMode {
        BrainGateMode::On
    }

    fn endpoint_health(&self) -> EndpointHealth {
        EndpointHealth::default()
    }
}
```

- [ ] **Step 4: Borrow the parsed activity log during Live projection**

In `src/brain/activity.rs`, make `project_snapshot` borrow `ActivityLog` and clone only diagnostics:

```rust
pub(crate) fn project_snapshot(
    &self,
    log: &ActivityLog,
    limits: SnapshotLimits,
) -> ActivitySnapshot {
    project_snapshot(
        log,
        limits,
        self.now_ms.unwrap_or_else(epoch_ms),
    )
}

pub fn snapshot(&self, limits: SnapshotLimits) -> Result<ActivitySnapshot, ActivityStoreError> {
    let log = self.read()?;
    Ok(self.project_snapshot(&log, limits))
}

fn project_snapshot(
    log: &ActivityLog,
    limits: SnapshotLimits,
    now_ms: u64,
) -> ActivitySnapshot {
    let mut groups = HashMap::<String, Vec<&ActivityEvent>>::new();
    for event in &log.events {
        groups
            .entry(event.activity_id.clone())
            .or_default()
            .push(event);
    }
    // The remaining grouping, ordering, truncation, and activity projection
    // statements stay byte-for-byte unchanged.
    ActivitySnapshot {
        attention,
        recent,
        diagnostic_events,
        unresolved_count,
        diagnostics: log.diagnostics.clone(),
    }
}
```

Only the shown signature, call site, event borrow, and diagnostics clone change;
all statements between group construction and `ActivitySnapshot` remain
byte-for-byte unchanged. Do not change grouping, ordering, truncation,
interrupted-state, or diagnostic semantics.

- [ ] **Step 5: Reuse captured activity when filtering decisions**

In `src/brain/decisions.rs`, add:

```rust
pub(crate) fn read_learning_decisions_from_activity(
    events: &[ActivityEvent],
) -> Vec<DecisionRecord> {
    filter_learning_decisions(read_all_decisions(), events)
}
```

Keep `read_learning_decisions` and `read_distillation_decisions` unchanged for their other callers.

- [ ] **Step 6: Add a failing production lock-contention test**

In `src/runtime/brain.rs`, use `crate::config::HOME_ENV_LOCK` to serialize environment changes. Point `XDG_STATE_HOME` and `HOME` at a temporary root, derive the production `activity.jsonl` and `activity.lock` paths, lock the latter exclusively with `fs2::FileExt`, and assert:

```rust
struct RefreshEnvGuard {
    home: Option<std::ffi::OsString>,
    xdg_config_home: Option<std::ffi::OsString>,
    xdg_state_home: Option<std::ffi::OsString>,
}

impl Drop for RefreshEnvGuard {
    fn drop(&mut self) {
        // SAFETY: the test holds HOME_ENV_LOCK for the guard's lifetime.
        unsafe {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.xdg_config_home.take() {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match self.xdg_state_home.take() {
                Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                None => std::env::remove_var("XDG_STATE_HOME"),
            }
        }
    }
}

let _env_lock = crate::config::HOME_ENV_LOCK.lock().unwrap();
let temp = tempfile::tempdir().unwrap();
let config_home = temp.path().join("config");
let state_home = temp.path().join("state");
let _env = RefreshEnvGuard {
    home: std::env::var_os("HOME"),
    xdg_config_home: std::env::var_os("XDG_CONFIG_HOME"),
    xdg_state_home: std::env::var_os("XDG_STATE_HOME"),
};
// SAFETY: this test holds HOME_ENV_LOCK and the guard restores all values.
unsafe {
    std::env::set_var("HOME", temp.path());
    std::env::set_var("XDG_CONFIG_HOME", &config_home);
    std::env::set_var("XDG_STATE_HOME", &state_home);
}
let state_root = state_home.join("coding-brain");
std::fs::create_dir_all(&state_root).unwrap();
let lock = std::fs::OpenOptions::new()
    .create(true)
    .read(true)
    .write(true)
    .truncate(false)
    .open(state_root.join("activity.lock"))
    .unwrap();
lock.lock_exclusive().unwrap();

let source = LiveBrainSource::default();

let unlocker = std::thread::spawn(move || {
    std::thread::sleep(Duration::from_millis(25));
    FileExt::unlock(&lock).unwrap();
});
assert!(source.refresh(SnapshotLimits::default()).is_ok());
unlocker.join().unwrap();

let lock = std::fs::OpenOptions::new()
    .read(true)
    .write(true)
    .open(state_root.join("activity.lock"))
    .unwrap();
lock.lock_exclusive().unwrap();
assert!(matches!(
    source.refresh(SnapshotLimits::default()),
    Err(BrainSourceError::Busy)
));
FileExt::unlock(&lock).unwrap();
assert!(source.refresh(SnapshotLimits::default()).is_ok());
```

Use an RAII environment guard that restores both variables on drop, matching existing `HOME_ENV_LOCK` tests. Do not assert a tight wall-clock bound here; `brain::activity::tests::lock_wait_is_bounded_and_busy_compaction_skips` remains the strict timing proof.

- [ ] **Step 7: Run the production contention test and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain live_brain_refresh_reports_busy_during_activity_lock_contention -- --exact
```

Expected: FAIL because production `LiveBrainSource` still implements independent reads and string errors.

More precisely, this RED is initially a compilation failure because
`LiveBrainSource` has not yet implemented the new bundled trait method. The
behavioral RED remains in Task 2; after Step 8 this test must prove both
within-bound waiting and typed timeout fallback against the real lock.

- [ ] **Step 8: Implement the one-read production refresh**

In `src/runtime/brain.rs`, replace the three source methods with:

```rust
fn refresh(
    &self,
    limits: SnapshotLimits,
) -> Result<BrainRefresh, BrainSourceError> {
    let paths = brain::distill::current_paths()
        .map_err(|error| BrainSourceError::Other(error.to_string()))?;
    let store = brain::activity::ActivityStore::at(
        paths.state_root().join("activity.jsonl"),
    );
    let activity = store.read().map_err(|error| match error {
        brain::activity::ActivityStoreError::LockTimeout => BrainSourceError::Busy,
        other => BrainSourceError::Other(other.to_string()),
    })?;
    let records =
        brain::decisions::read_learning_decisions_from_activity(activity.events());
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

Preserve gate-mode and endpoint-health methods exactly.

- [ ] **Step 9: Run focused runtime and store tests**

Run:

```bash
direnv exec . cargo test -p coding-brain live_brain_refresh_reports_busy_during_activity_lock_contention
direnv exec . cargo test -p coding-brain runtime::brain::tests
direnv exec . cargo test -p coding-brain brain::activity::tests
direnv exec . cargo test -p coding-brain-core runtime::tests
direnv exec . cargo check --workspace
```

Expected: all focused tests pass, including existing correction projection and
bounded-lock coverage, and every workspace crate compiles against the bundled
trait.

- [ ] **Step 10: Review Task 1 diff**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-core/src/runtime.rs src/brain/activity.rs src/brain/decisions.rs src/runtime/brain.rs crates/coding-brain-tui/src/brain_app.rs
```

Expected: only the bundled contract, borrowed projection, captured-activity decision filter, production implementation, contention test, and compile migration are present.

- [ ] **Step 11: Commit Task 1 only if explicitly authorized**

If the user has authorized commits:

```bash
git add crates/coding-brain-core/src/runtime.rs src/brain/activity.rs src/brain/decisions.rs src/runtime/brain.rs crates/coding-brain-tui/src/brain_app.rs
git commit -m "🐛 fix: bundle brain refresh projections (codexctl-2iay)"
```

Otherwise leave the verified diff uncommitted and continue only within the same authorized worktree.

---

### Task 2: Apply TUI Refreshes Atomically and Recover from Busy State

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs`
- Verify: `crates/coding-brain-tui/src/ui/brain/mod.rs`
- Verify: `.internal/specs/2026-07-24-scorecard-refresh-lock-contention-design.md`

**Interfaces:**
- Consumes: `BrainSource::refresh(SnapshotLimits) -> Result<BrainRefresh, BrainSourceError>`.
- Consumes: `BrainSourceError::{Busy, Other(String)}`.
- Produces: atomic assignment of `snapshot`, `review_queue`, and `scorecard`.
- Produces: `has_successful_refresh: bool`.
- Produces: cold-start status `Brain data busy; retrying`.
- Produces: stale-view status `Brain data busy; showing previous refresh`.

**Acceptance Criteria:**
- A successful bundled refresh replaces Live, Review, and Scorecard together.
- Busy contention never partially updates the three views.
- Cold-start contention says retrying; later contention retains data and says it is showing the previous refresh.
- Completed action, real source error, and recovery feedback outrank busy information.
- A later successful refresh clears only a busy status.
- Relevant TUI/runtime/store tests and all workspace quality gates pass.

- [ ] **Step 1: Add scripted failing TUI regressions**

In `crates/coding-brain-tui/src/brain_app.rs`, add a test source backed by:

```rust
struct ScriptedBrainSource {
    refreshes: std::sync::Mutex<
        std::collections::VecDeque<Result<BrainRefresh, BrainSourceError>>,
    >,
}

impl BrainSource for ScriptedBrainSource {
    fn refresh(
        &self,
        _limits: SnapshotLimits,
    ) -> Result<BrainRefresh, BrainSourceError> {
        self.refreshes
            .lock()
            .expect("scripted refreshes poisoned")
            .pop_front()
            .expect("unexpected refresh")
    }

    fn gate_mode(&self) -> BrainGateMode {
        BrainGateMode::On
    }

    fn endpoint_health(&self) -> EndpointHealth {
        EndpointHealth::default()
    }
}
```

Add three tests:

```rust
fn scripted_app<const N: usize>(
    refreshes: [Result<BrainRefresh, BrainSourceError>; N],
) -> BrainApp {
    let source = Arc::new(ScriptedBrainSource {
        refreshes: std::sync::Mutex::new(refreshes.into_iter().collect()),
    });
    let actions = Arc::new(MockBrainRuntime::default());
    BrainApp::new(
        BrainRuntime::new(source, actions),
        Theme::from_mode(ThemeMode::Dark),
    )
}

fn refresh_fixture(marker: &str, review_count: usize, total: usize) -> BrainRefresh {
    let mut live = activity();
    live.activity_id = marker.into();
    let mut review_decision = decision();
    review_decision.id = marker.into();
    BrainRefresh {
        snapshot: ActivitySnapshot {
            recent: vec![live],
            ..ActivitySnapshot::default()
        },
        review_queue: (0..review_count)
            .map(|_| ReviewItemSummary {
                decision: review_decision.clone(),
                reason: "fixture".into(),
                score: 1.0,
            })
            .collect(),
        scorecard: ScorecardSummary {
            total_decisions: total,
            ..ScorecardSummary::default()
        },
    }
}

fn assert_refresh_fixture(
    app: &BrainApp,
    marker: &str,
    review_count: usize,
    total: usize,
) {
    assert_eq!(app.snapshot.recent[0].activity_id, marker);
    assert_eq!(app.review_queue.len(), review_count);
    assert!(app
        .review_queue
        .iter()
        .all(|item| item.decision.id == marker));
    assert_eq!(app.scorecard.total_decisions, total);
}

#[test]
fn cold_start_busy_reports_retrying() {
    let app = scripted_app([Err(BrainSourceError::Busy)]);
    assert_eq!(app.status(), Some("Brain data busy; retrying"));
}

#[test]
fn busy_refresh_retains_all_views_then_recovers_atomically() {
    let mut app = scripted_app([
        Ok(refresh_fixture("old", 1, 1)),
        Err(BrainSourceError::Busy),
        Ok(refresh_fixture("new", 2, 2)),
    ]);

    app.refresh();
    assert_refresh_fixture(&app, "old", 1, 1);
    assert_eq!(
        app.status(),
        Some("Brain data busy; showing previous refresh")
    );

    app.refresh();
    assert_refresh_fixture(&app, "new", 2, 2);
    assert_eq!(app.status(), None);
}

#[test]
fn busy_refresh_does_not_overwrite_higher_priority_status() {
    let mut app = scripted_app([
        Ok(refresh_fixture("old", 1, 1)),
        Err(BrainSourceError::Busy),
    ]);
    app.status = Some("Sent allow".into());

    app.refresh();

    assert_eq!(app.status(), Some("Sent allow"));
}

struct RecoveryWarningActions;

impl BrainActions for RecoveryWarningActions {
    fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
        Ok(())
    }

    fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
        Ok(())
    }

    fn send_session_action(&self, _request: SessionActionRequest) -> Result<(), String> {
        Ok(())
    }

    fn poll_recovery(&self) -> Vec<String> {
        vec!["Recovered interrupted activity".into()]
    }
}

#[test]
fn recovery_warning_outranks_busy_information() {
    let source = Arc::new(ScriptedBrainSource {
        refreshes: std::sync::Mutex::new(
            [Err(BrainSourceError::Busy)].into_iter().collect(),
        ),
    });
    let runtime = BrainRuntime::new(source, Arc::new(RecoveryWarningActions));
    let app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

    assert_eq!(app.status(), Some("Recovered interrupted activity"));
}
```

`refresh_fixture` must put the marker into the Live activity ID, Review decision ID, and Scorecard count so one assertion detects partial application.

- [ ] **Step 2: Run TUI regressions and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui cold_start_busy_reports_retrying
direnv exec . cargo test -p coding-brain-tui busy_refresh_retains_all_views_then_recovers_atomically
direnv exec . cargo test -p coding-brain-tui busy_refresh_does_not_overwrite_higher_priority_status
direnv exec . cargo test -p coding-brain-tui recovery_warning_outranks_busy_information
```

Expected: tests fail because `BrainApp` still calls independent view methods and has no successful-refresh state.

- [ ] **Step 3: Implement atomic refresh and busy-state handling**

In `BrainApp`, add:

```rust
has_successful_refresh: bool,
```

Initialize it to `false`. Add constants:

```rust
const BUSY_RETRYING_STATUS: &str = "Brain data busy; retrying";
const BUSY_STALE_STATUS: &str =
    "Brain data busy; showing previous refresh";
```

Replace the three source calls in `refresh_state` with one call. On success,
destructure and assign all three view fields before setting
`has_successful_refresh = true`. On `Busy`, preserve every field and select the
appropriate informational text. On `Other(error)`, build:

```rust
format!("Brain: {}", bounded_status(&error))
```

Apply status priority in this order:

1. newly completed session-action status;
2. non-contention source error;
3. recovery error;
4. busy information, but only when the current status is absent or already a busy status;
5. after success, clear the current status only when it equals one of the two busy constants.

Keep `refresh_state`'s existing return meaning for completed action delivery so navigation completion behavior remains unchanged.

- [ ] **Step 4: Run focused TUI tests**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui brain_app::tests
direnv exec . cargo test -p coding-brain-tui ui::brain::tests
```

Expected: all application and rendering tests pass; the three new regressions prove cold start, coherent retention, recovery, and priority.

- [ ] **Step 5: Format and run all workspace quality gates**

Run:

```bash
direnv exec . cargo fmt
direnv exec . cargo fmt --check
direnv exec . cargo test --all-targets
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo build
```

Expected: every command exits zero with no warnings.

- [ ] **Step 6: Verify scope and acceptance criteria**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff
```

Confirm:

- no timeout, interval, schema, config, hook, or persistence changes;
- one production activity read and one decision read per refresh;
- no string matching for contention;
- no full activity-history clone;
- all three view fields update together;
- both busy messages and priority behavior are covered;
- design and plan artifacts match the implementation.

- [ ] **Step 7: Commit Task 2 only if explicitly authorized**

If the user has authorized commits:

```bash
git add crates/coding-brain-tui/src/brain_app.rs .internal/specs/2026-07-24-scorecard-refresh-lock-contention-design.md .internal/plans/2026-07-24-scorecard-refresh-lock-contention.md
git commit -m "🐛 fix: retain scorecard during activity contention (codexctl-2iay)"
```

Otherwise leave the fully verified worktree uncommitted for handoff.

## Stress Test Results: coherent Scorecard refresh implementation plan

### Resolved Decisions

- **Task independence:** Keep two tasks and require `cargo check --workspace`
  before Task 1 is considered deliverable.
- **API and types:** Keep the minimal bundled refresh, typed error, crate-local
  projection, and activity-aware decision helper without generalized caching.
- **RED/GREEN validity:** Describe the production test's initial compile RED
  accurately and rely on Task 2 for behavioral UI RED.
- **Environment determinism:** Set and restore `HOME`, `XDG_CONFIG_HOME`, and
  `XDG_STATE_HOME` under the existing environment lock.
- **Status precedence:** Add an explicit recovery-warning-over-busy regression.
- **Verification:** Run all targets plus the repository-prescribed formatting,
  Clippy, build, and Task 1 workspace compilation gates.
- **Consent and rollback:** Keep commits conditional, never push or publish,
  and retain a code-only revert path.
- **Normal concurrent writes:** Test both within-bound lock release and
  beyond-bound typed fallback against the real production source.

### Changes Made

- Added the Task 1 workspace compile gate.
- Corrected the production test's expected RED description.
- Expanded test environment isolation to all path inputs.
- Added recovery-priority coverage.
- Expanded the production lock regression to cover normal and prolonged
  contention.
- Upgraded the full test command to `cargo test --all-targets`.

### Deferred / Parking Lot

- No benchmark or cache work is planned; profile later only if the single
  read/parse projection becomes a measured bottleneck.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** The lock test uses real scheduling, but the lock is
  acquired before synchronous refresh and released only after a fixed 25 ms,
  so overlap does not depend on the refresh thread winning a race.
