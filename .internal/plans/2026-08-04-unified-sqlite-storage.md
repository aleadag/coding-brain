# Unified SQLite Brain Storage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Replace the live cross-store JSONL permission transaction with bounded SQLite-backed Brain and lifecycle persistence, migrate review state to an isolated SQLite database, and provide verified migration and rollback export without weakening permission safety.

**Architecture:** The root binary crate owns `db/brain.sqlite3` for decision identity/payload, complete lifecycle state, permission attempts/commits, activity, and learning cursors, while `db/review.sqlite3` owns visibility-only review marks. `coding-brain-core` retains pure lifecycle/activity types, and `session-links.jsonl` remains separate. Non-hook startup performs verified migration; hooks require an exact current schema and never migrate or replay responses.

**Tech Stack:** Rust 1.88, `rusqlite` with bundled SQLite, Serde, fs2 advisory locks, existing Coding Brain core/runtime traits, Cargo/Nix tests.

## Global Constraints

- Preserve ADR-0003's separate proposal, commitment, delivery, and execution meanings.
- Keep `rusqlite` in the root package only; `coding-brain-core` must not depend on SQLite or binary modules.
- Use `$XDG_STATE_HOME/coding-brain/db/brain.sqlite3` and `review.sqlite3` under a validated owner-only local-filesystem directory.
- Use WAL, `synchronous=FULL`, foreign keys, defensive mode, `trusted_schema=OFF`, disabled extension loading, secure deletion, and explicit SQLite limits.
- One absolute hook deadline starts before admission. Inference consumes it, and open, busy retries, commit, and delivery evidence receive only its remaining time; no phase resets it.
- Hooks never initialize or migrate schema. Missing, busy, incomplete, unsafe, corrupt, older, or newer Brain storage leaves native provider handling authoritative.
- Deterministic code-owned safety denies remain fail-closed if audit storage is unavailable.
- Never ship a partial live SQLite/JSONL permission split. Legacy readers exist only for migration/export fixtures until final cutover.
- Keep `session-links.jsonl` unchanged; missing or inconsistent links disable exact navigation/recovery action, not permission authority.
- Preserve a nonreusing 64-bit activity source cursor and the cursor in the last published immutable preference generation.
- Restrict activity cursors to exact positive SQLite integers (`1..=i64::MAX`) and fail closed before allocation at the upper bound.
- Treat `forget()` as verified logical erasure from Coding Brain-managed stores; do not promise physical-media, snapshot, or backup erasure.
- Task 8 is the only production activation point. Earlier tasks may add adapters and tests but must leave every live legacy writer intact.
- Do not commit raw runtime state, sensitive commands, fixture secrets, or user paths.
- Every behavior change is test-first and every task ends with a fresh focused gate before its atomic commit.

---

## Preflight Baseline

Before Task 1 changes any dependency or source file, run:

`nix develop path:. --command cargo test --workspace --all-targets -- --test-threads=1`

Record any pre-existing failure on `codexctl-dzlb9`; do not attribute it to the migration or weaken a production threshold to make it pass.

---

## File Structure

- `src/brain/storage/mod.rs`: public storage facade, paths, connection roles, deadlines, and shared errors.
- `src/brain/storage/schema.rs`: Brain/review SQL schemas, SQLite configuration, version checks, and schema upgrades.
- `src/brain/storage/security.rs`: owner-only directory/file validation and local-filesystem/open policy.
- `src/brain/storage/lifecycle.rs`: persistence adapter for pure core lifecycle snapshots.
- `src/brain/storage/activity.rs`: append ledger, monotonic cursors, bounded indexed reads, retention, and projections.
- `src/brain/storage/decisions.rs`: immutable decision identity, erasable payload, learning reads, and forget transaction.
- `src/brain/storage/review.rs`: isolated review database and optimistic mutations.
- `src/brain/storage/permissions.rs`: attempts, atomic commits, authority queries, delivery evidence, and commit uncertainty.
- `src/brain/storage/legacy.rs`: bounded legacy readers and frozen compatibility profile.
- `src/brain/storage/migration.rs`: non-hook staging import, source locking/fingerprints, cutover, freeze, and restart recovery.
- `src/brain/storage/export.rs`: audit and frozen-profile downgrade export.
- `src/brain/storage/maintenance.rs`: WAL thresholds/checkpoints, incremental vacuum/retention, health evidence, and deep checks.
- Existing `activity.rs`, `decisions.rs`, `review_state.rs`, `permission_transaction.rs`, and core lifecycle `store.rs` become legacy adapters or are removed only after every live caller is cut over.

---

### Task 1: Secure SQLite Foundation and Frozen Schema

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/brain/mod.rs`
- Create: `src/brain/storage/mod.rs`
- Create: `src/brain/storage/schema.rs`
- Create: `src/brain/storage/security.rs`
- Create: `tests/sqlite_storage.rs`
- Add fixture: `tests/fixtures/storage/schema-v1/**`

**Interfaces:**
- Produces: `StoragePaths::at(&Path)`, `StorageDeadline`, `OpenRole`, `BrainDb::create_current`, `BrainDb::open_current`, `ReviewDb::create_current`, `ReviewDb::open_current`, and `StorageError`.
- Produces: `BRAIN_APPLICATION_ID`, `BRAIN_SCHEMA_VERSION`, `REVIEW_APPLICATION_ID`, and `REVIEW_SCHEMA_VERSION`.
- Consumes: `CodingBrainPaths::state_root()` and existing `secure_state` ownership conventions.

**Acceptance Criteria:**
- Only the root package gains `rusqlite` with bundled SQLite.
- Fresh databases have exact application/schema versions, required pragmas, owner-only files, and no dependency on a system SQLite library.
- Unsafe ancestors, database files, or pre-existing sidecars are rejected before authoritative use.
- Hook-role opening never creates or upgrades a database and respects one absolute deadline.
- A frozen schema fixture and constraint tests enforce the authority invariant matrix below.

**Schema Invariant Matrix:**

| Table | Required identity and constraints | Required query path |
|---|---|---|
| `schema_meta` | Singleton row; exact application/schema generation; migration and erasure states use closed `CHECK` domains; `activity_high_water` is `0..=i64::MAX`. | Exact singleton lookup on every open. |
| `permission_attempts` | Primary key `attempt_id`; complete authority identity stored in typed columns; request identity is indexed but not unique; attempt state uses a closed domain. | Exact attempt lookup and active request-identity lookup. |
| `decision_identities` | Primary key `decision_id`; typed `permission` rows require the complete immutable authority identity/action columns used by commit references, while typed `observation` rows keep only real non-authoritative provider/timestamp identity and require every authority field to be null. | Exact decision and authority-identity lookup. |
| `decision_payloads` | Composite foreign key `(decision_id, payload_kind)` to the matching identity kind; bounded erasable learning fields plus the complete validated legacy-compatible decision payload. | Joined committed-learning query by source cursor. |
| `activity_events` | Primary key `source_cursor` with `1..=i64::MAX` check; indexed, non-unique logical activity ID permits observed/terminal/delivery/outcome/correction rows for one activity; typed terminal action/identity columns retain the composite uniqueness required by permission-commit references. | Cursor pages, activity ID, permission identity, outcome, correction, and distillation indexes. |
| `permission_commits` | One row per attempt; unique decision and terminal activity references; composite foreign keys require matching authority identity/action across attempt, decision, and terminal event; closed action/evidence/delivery domains; boolean `response_eligible` check. | Exact attempt/request authority and undelivered-audit lookup. |
| lifecycle session/turn/invocation tables | Provider-qualified composite keys, bounded sequence values, and foreign keys from turns/invocations to sessions; no duplicate active identity. | Exact provider/session/turn and active-topology indexes. |
| review `review_meta` / `review_marks` | Per-surface revision; exact surface/group/source-cursor key; closed disposition domain; no Brain tables or attachments. | Exact surface revision and bounded cursor-mark lookup. |

The checked-in schema fixture is authoritative. Approved pre-activation corrections amend schema v1 because no production activation or migration has occurred: Task 3 removes only accidental single-column `activity_id` uniqueness while retaining the terminal-identity composite authority key; Task 4 adds typed non-authoritative observation identities and a bounded complete learning payload without weakening permission identity constraints. Any later DDL change must update its version, fixture, invariant tests, and supported-upgrade coverage before the same atomic commit.

- [ ] **Step 1: Write failing foundation tests**

```rust
#[test]
fn hook_open_never_creates_or_migrates() {
    let root = tempfile::tempdir().unwrap();
    let paths = StoragePaths::at(root.path());
	let error = BrainDb::open_current(
		&paths,
		OpenRole::Hook,
		StorageDeadline::after(Duration::from_millis(50)),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::MigrationRequired));
    assert!(!paths.brain_db().exists());
}

#[test]
fn fresh_brain_database_has_exact_security_pragmas() {
    let root = tempfile::tempdir().unwrap();
    let paths = StoragePaths::at(root.path());
    let db = BrainDb::create_current(&paths).unwrap();
    assert_eq!(db.application_id().unwrap(), BRAIN_APPLICATION_ID);
    assert_eq!(db.user_version().unwrap(), BRAIN_SCHEMA_VERSION);
    assert_eq!(db.pragma_string("journal_mode").unwrap(), "wal");
    assert_eq!(db.pragma_i64("foreign_keys").unwrap(), 1);
    assert_eq!(db.pragma_i64("trusted_schema").unwrap(), 0);
}
```

- [ ] **Step 2: Verify the tests fail before the module exists**

Run: `nix develop path:. --command cargo test --test sqlite_storage hook_open_never_creates_or_migrates -- --exact`

Expected: compilation fails because `brain::storage` is not defined.

- [ ] **Step 3: Add the dependency and minimal secure facade**

```rust
pub(crate) const BRAIN_APPLICATION_ID: i32 = 0x4342_524e;
pub(crate) const BRAIN_SCHEMA_VERSION: i32 = 1;

pub(crate) struct StoragePaths {
    db_dir: PathBuf,
}

impl StoragePaths {
    pub(crate) fn at(state_root: &Path) -> Self {
		Self { db_dir: state_root.join("db") }
    }
    pub(crate) fn brain_db(&self) -> PathBuf { self.db_dir.join("brain.sqlite3") }
    pub(crate) fn review_db(&self) -> PathBuf { self.db_dir.join("review.sqlite3") }
}

#[derive(Clone, Copy)]
pub(crate) enum OpenRole { Hook, NonHook }

pub(crate) struct StorageDeadline(Instant);

impl StorageDeadline {
    pub(crate) fn after(duration: Duration) -> Self { Self(Instant::now() + duration) }
    pub(crate) fn remaining(&self) -> Result<Duration, StorageError> {
		self.0.checked_duration_since(Instant::now()).ok_or(StorageError::Busy)
    }
}
```

Implement the complete invariant-matrix DDL and check in its exact schema fixture. Configure each connection with static pragmas, `OpenFlags::SQLITE_OPEN_NO_FOLLOW`, disabled extension loading, defensive database config, and explicit length/column/SQL limits. Validate the dedicated directory and every pre-existing entry before open.

- [ ] **Step 4: Run foundation and packaging checks**

Run: `nix develop path:. --command cargo test --test sqlite_storage`

Expected: all foundation/security tests pass.

Run: `nix develop path:. --command cargo tree -i libsqlite3-sys`

Expected: `libsqlite3-sys` appears only below the root `coding-brain` package.

- [ ] **Step 5: Commit the atomic foundation**

```bash
git add Cargo.toml Cargo.lock src/brain/mod.rs src/brain/storage tests/sqlite_storage.rs tests/fixtures/storage/schema-v1
git commit -m "🗃️ feat: add secure SQLite storage foundation (codexctl-2o9fo)"
```

### Task 2: Pure Core Lifecycle State and SQLite Persistence

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/mod.rs`
- Create: `src/brain/storage/lifecycle.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Consumes: `BrainDb`, `LifecycleEvent`, `LifecycleIdentity`, `LifecycleSnapshot`, and `RecordedLifecycleEvent`.
- Produces: pure `LifecycleSnapshot::record_at(event, received_at_ms)` and `BrainDb::{read_lifecycle,record_lifecycle}`.
- Produces: database permission queries separately from topology; no core persistence method remains authoritative for permission disposition.

**Acceptance Criteria:**
- Core lifecycle transition behavior is unchanged and fully testable without filesystem or SQLite access.
- Complete non-permission lifecycle topology persists in typed session, turn, lease, invocation, and subagent tables; only provider-specific extras retain the existing 1 MiB aggregate bound.
- Permission disposition/authority is absent from live core snapshot persistence and remains available only through Brain permission tables.
- No SQLite dependency enters `coding-brain-core`.

- [ ] **Step 1: Add failing pure-transition and database round-trip tests**

```rust
#[test]
fn lifecycle_round_trip_preserves_topology_without_permission_authority() {
    let (db, identity) = fixture_brain_db_and_identity();
    let event = LifecycleEvent::session_start(identity.clone(), SessionStartSource::Hook);
    db.record_lifecycle(event, 100).unwrap();
    let snapshot = db.read_lifecycle().unwrap();
    assert!(snapshot.session(identity.provider(), identity.session_id()).is_some());
    assert_eq!(db.permission_decision(&identity, "request-a").unwrap(), None);
}
```

- [ ] **Step 2: Verify focused failure**

Run: `nix develop path:. --command cargo test lifecycle_round_trip_preserves_topology_without_permission_authority -- --exact`

Expected: compilation fails because `BrainDb::record_lifecycle` does not exist.

- [ ] **Step 3: Extract pure transitions and implement lifecycle adapter**

Expose one pure transition entry point in core. Load a bounded snapshot from relational session, turn, lease, invocation, and subagent rows, apply the transition, then persist the affected typed rows in one transaction. Bounded JSON is allowed only for validated provider-specific extras. Remove permission map mutation from the pure topology snapshot; permission events call the permission storage interface in Task 6.

```rust
pub(crate) fn record_lifecycle(
    &mut self,
    event: LifecycleEvent,
    received_at_ms: u64,
) -> Result<RecordedLifecycleEvent, StorageError> {
	self.transaction(TransactionBehavior::Immediate, |tx| {
		let mut snapshot = load_lifecycle_snapshot(tx)?;
		let recorded = snapshot.record_at(event, received_at_ms);
		persist_lifecycle_snapshot(tx, &snapshot)?;
		Ok(recorded)
    })
}
```

- [ ] **Step 4: Run lifecycle and layering gates**

Run: `nix develop path:. --command cargo test -p coding-brain-core lifecycle`

Expected: all existing core lifecycle tests pass.

Run: `nix develop path:. --command cargo test --test sqlite_storage lifecycle_`

Expected: all SQLite lifecycle tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/coding-brain-core/src/lifecycle src/brain/storage/lifecycle.rs tests/sqlite_storage.rs
git commit -m "♻️ refactor: separate lifecycle state from persistence (codexctl-2o9fo)"
```

### Task 3: Indexed Activity Ledger and Stable Cursors

**Files:**
- Create: `src/brain/storage/activity.rs`
- Modify: `src/brain/activity.rs`
- Modify: `crates/coding-brain-core/src/brain_activity.rs`
- Modify: `tests/activity_scale.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Produces: `BrainDb::{append_activity,append_activity_batch,read_activity_page,activity_by_id,activity_after_cursor}`.
- Produces: `ActivityPage { events, next_cursor, serialized_bytes }` and `ActivityCursor(u64)`.
- `activity_by_id` and ascending cursor reads accept an exclusive `after` cursor; recent reads accept an exclusive descending `before` cursor. `next_cursor` is present only when bounded lookahead proves another matching row remains.
- Consumes: existing `ActivityEvent`, `ActivityLog`, projection limits, and redaction validation.

**Acceptance Criteria:**
- Activity insert and high-water allocation are atomic and cursors never reuse after retention or rebuild.
- Cursor allocation is restricted to `1..=i64::MAX`; reaching the maximum returns a fail-closed storage error without inserting or coercing a numeric value.
- Live/Review/Scorecard inputs use indexed cursor/key queries with row and byte limits.
- Existing projection semantics, duplicate-terminal diagnostics, corrections, outcome correlation, and delivery states remain unchanged.
- Legacy JSONL parsing remains available only behind the migration adapter.

- [ ] **Step 1: Write cursor and query-plan regressions**

```rust
#[test]
fn activity_cursor_survives_delete_and_rebuild() {
    let db = fixture_brain_db();
    let first = db.append_activity(event("a", 1)).unwrap();
    db.delete_activity_before(first.next().unwrap()).unwrap();
    let second = db.append_activity(event("b", 2)).unwrap();
    assert!(second > first);
    assert_eq!(db.activity_high_water().unwrap(), second);
}

#[test]
fn recent_activity_query_uses_cursor_index() {
    let db = fixture_brain_db_with_events(50_000);
    assert!(db.explain_recent_activity().unwrap().contains("activity_events_cursor"));
}
```

- [ ] **Step 2: Verify the focused tests fail**

Run: `nix develop path:. --command cargo test --test sqlite_storage activity_cursor_ -- --nocapture`

Expected: compilation fails on missing activity APIs.

- [ ] **Step 3: Implement the ledger and adapt pure projection**

Use a transactionally updated `activity_high_water`; never derive it from retained rows. Insert typed columns and bounded payloads, then materialize bounded pages before calling existing pure projection code.

```rust
let cursor = checked_next_cursor(tx)?;
insert_activity(&tx, cursor, &event)?;
```

- [ ] **Step 4: Run activity compatibility and scale tests**

Run: `nix develop path:. --command cargo test --test activity_scale --test sqlite_storage activity`

Expected: cursor, projection, and indexed scale tests pass without full-table query plans.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/activity.rs src/brain/activity.rs crates/coding-brain-core/src/brain_activity.rs tests/activity_scale.rs tests/sqlite_storage.rs
git commit -m "🗃️ feat: add indexed SQLite activity ledger (codexctl-2o9fo)"
```

### Task 4: Decision Identity, Learning Payload, and Privacy Erasure

**Files:**
- Create: `src/brain/storage/decisions.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/distill.rs`
- Modify: `src/brain/{baseline,briefing,insights,metrics,retrieval}.rs`
- Modify: `tests/distill_process.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Produces: `DecisionIdentity`, `DecisionPayload`, `BrainDb::{insert_decision,learning_decisions,forget_learning}`.
- Consumes: immutable activity cursors and the existing immutable preference-generation publisher.
- Preserves: `read_learning_decisions()` as a compatibility facade backed by `BrainDb` until direct callers are migrated.

**Acceptance Criteria:**
- Permission commits can retain minimal immutable decision identity after learning payload deletion.
- Metrics, retrieval, briefing, insights, and distillation read only joined committed payloads.
- `forget()` uses a durable erasure generation that remains incomplete until payloads/canonical marks, published generations, and frozen legacy learning sources are gone.
- Learning reads and downgrade export fail closed while erasure is incomplete; startup resumes it before exposing learning data.
- Secure deletion and WAL truncation failures leave erasure incomplete; no supported reader or export can restore forgotten payloads after completion.

- [ ] **Step 1: Write failing forget and cursor-publication tests**

```rust
#[test]
fn forget_removes_payload_but_preserves_commit_identity() {
    let db = fixture_committed_decision();
    db.forget_learning(&fixture_paths()).unwrap();
    assert!(db.decision_identity("decision-1").unwrap().is_some());
    assert!(db.decision_payload("decision-1").unwrap().is_none());
    assert!(db.learning_decisions(100).unwrap().is_empty());
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test sqlite_storage forget_removes_payload -- --exact`

Expected: compilation fails on the new decision APIs.

- [ ] **Step 3: Implement split storage and erasure ordering**

Implement `decision_identities` and `decision_payloads` with `ON DELETE CASCADE` only from identity to payload, never from commit to identity. Under the global erasure gate, first persist an incomplete erasure generation. Delete database payloads, preference generations, and frozen legacy learning files; checkpoint/truncate and sync; then mark the generation complete. Startup resumes any incomplete generation before learning reads or downgrade export. The guarantee covers Coding Brain-managed logical copies, not filesystem snapshots, backups, or physical media.

- [ ] **Step 4: Run learning and process tests**

Run: `nix develop path:. --command cargo test --test distill_process --test sqlite_storage forget`

Expected: process-kill and privacy erasure tests pass; no forgotten payload is returned.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/decisions.rs src/brain/decisions.rs src/brain/distill.rs src/brain/baseline.rs src/brain/briefing.rs src/brain/insights.rs src/brain/metrics.rs src/brain/retrieval.rs tests/distill_process.rs tests/sqlite_storage.rs
git commit -m "🔒 feat: make SQLite learning payload erasable (codexctl-2o9fo)"
```

### Task 5: Isolated SQLite Review State

**Files:**
- Create: `src/brain/storage/review.rs`
- Modify: `src/brain/review_state.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `tests/brain_review_state.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Produces: `ReviewDb::{read_surface,mutate,reset}` using existing `ReviewMutationRequest`, `ReviewMutationResult`, `ReviewKey`, and `ReviewSurface`.
- Consumes: bounded eligible group/cursor evidence from a completed Brain read transaction.

**Acceptance Criteria:**
- Review state uses only `review.sqlite3` and cannot mutate Brain authority/audit tables.
- Revision conflicts, independent surfaces, archive/undo/reset, count validation, and new-occurrence resurfacing match current behavior.
- Review corruption/busy state degrades review operations without disabling coherent Brain reads or permissions.
- A review mark hides only its exact validated source cursor; later cursors always resurface and missing-cursor marks are harmless, pruneable orphans.

- [ ] **Step 1: Add failing isolation tests**

```rust
#[test]
fn corrupt_review_database_does_not_block_brain_database() {
    let fixture = fixture_storage();
    std::fs::write(fixture.paths.review_db(), b"not sqlite").unwrap();
    assert!(fixture.review.open().is_err());
    assert_eq!(fixture.brain.activity_by_id("a1").unwrap().len(), 1);
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test sqlite_storage corrupt_review_database -- --exact`

Expected: test fails because review state still uses JSON.

- [ ] **Step 3: Implement review schema and adapter**

Keep the current mutation validator pure. Load the relevant surface revision/marks, apply validation, and replace only affected exact-cursor rows inside one immediate review transaction. Never attach `brain.sqlite3`. Race tests append and retain Brain activity between validation and review commit; newer cursors must remain visible and orphaned marks must not affect Brain reads.

- [ ] **Step 4: Run review suites**

Run: `nix develop path:. --command cargo test --test brain_review_state --test sqlite_storage review`

Expected: all review behavior and failure-domain tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/review.rs src/brain/review_state.rs src/runtime/brain.rs tests/brain_review_state.rs tests/sqlite_storage.rs
git commit -m "🗃️ feat: isolate review state in SQLite (codexctl-2o9fo)"
```

### Task 6: Atomic Permission Attempts, Commits, and Delivery

**Files:**
- Create: `src/brain/storage/permissions.rs`
- Modify: `src/brain/permission_request_lock.rs`
- Modify: `tests/hook_activity.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Produces: `PermissionAttemptGuard`, `AttemptId`, `PreparedPermissionCommit`, and `BrainDb::{admit_permission,commit_permission,record_delivery,permission_decision}`.
- Consumes: existing request lock, `HookDecisionRecord`, `ActivityEvent`, `PermissionAuthority`, and absolute `StorageDeadline`.
- Legacy `PermissionTransactionJournal` remains the production path until Task 8 and later becomes parse-only for Task 7 migration.

**Acceptance Criteria:**
- Concurrent identical requests have one active inference winner; sequential identical requests receive distinct attempts.
- Proposal, terminal activity, and exact authority commit atomically before stdout.
- Failed/uncertain commit emits no model response; fresh-open state determines committed versus absent without replay.
- DeliveryFailed and DeliveryUnknown remain distinct; deterministic safety denies survive unavailable audit.
- SQLite permission APIs and process fixtures create no permission journal, but the production hook remains on the legacy path until Task 8.
- One absolute deadline begins before admission, includes inference time, and cannot be reset by later storage calls or busy retries.

- [ ] **Step 1: Add failing process-boundary tests**

```rust
#[test]
fn uncertain_commit_never_emits_and_fresh_open_decides_state() {
    let fixture = PermissionProcessFixture::new(CommitFault::AfterWalSync);
    let result = fixture.run_hook();
    assert!(result.stdout.is_empty());
    let reopened = fixture.reopen_brain();
	assert!(matches!(
		reopened.permission_state(&result.attempt_id).unwrap(),
		PermissionState::CommittedDeliveryUnknown | PermissionState::Absent
    ));
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test sqlite_storage uncertain_commit_never_emits -- --exact`

Expected: test fails against cross-store journal behavior.

- [ ] **Step 3: Implement atomic commit path**

```rust
pub(crate) fn commit_permission(
    &mut self,
    prepared: PreparedPermissionCommit,
) -> Result<CommittedPermission, StorageError> {
	self.immediate(|tx| {
		require_evaluating_attempt(tx, &prepared.attempt_id)?;
		insert_decision_identity_and_payload(tx, &prepared.decision)?;
		let cursor = insert_activity_with_cursor(tx, &prepared.terminal)?;
		insert_permission_commit(tx, &prepared, cursor)?;
		mark_attempt_committed(tx, &prepared.attempt_id)?;
		Ok(CommittedPermission::new(prepared.attempt_id, cursor))
    })
}
```

The SQLite adapter writes stdout only after this returns `Ok`. A delivery append error after stdout leaves unknown evidence unless stdout itself returned an error. Keep the production hook wired to the legacy adapter until Task 8; test this path through an explicit SQLite process fixture.

- [ ] **Step 4: Run provider and permission suites**

Run: `nix develop path:. --command cargo test --test sqlite_storage permission -- --test-threads=1`

Expected: SQLite atomicity, deadline exhaustion, busy-retry, and delivery tests pass serially while existing provider-hook tests remain unchanged.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/permissions.rs src/brain/permission_request_lock.rs tests/hook_activity.rs tests/sqlite_storage.rs
git commit -m "🔒 feat: commit permission authority atomically (codexctl-2o9fo)"
```

### Task 7: Restartable Legacy Migration and Split-Brain Cutover

**Files:**
- Create: `src/brain/storage/legacy.rs`
- Create: `src/brain/storage/migration.rs`
- Create: `tests/storage_migration.rs`
- Add fixtures: `tests/fixtures/storage/legacy-v0.59.1/**`
- Modify: `src/brain/{activity,decisions,permission_transaction,review_state}.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Produces: `MigrationCoordinator::{inspect,run_non_hook,resume}`, `MigrationStatus`, `LegacySourceSet`, `FrozenSourceManifest`, and `LEGACY_EXPORT_PROFILE`.
- Consumes: bounded legacy readers, both staging database constructors, fixed-order legacy locks, source fingerprints, and directory sync.

**Acceptance Criteria:**
- Non-hook migration streams every supported legacy store; hooks only report `MigrationRequired` or `MigrationActive`.
- Publication is restartable at every crash point and never exposes a response-eligible partial authority database.
- Final cutover locks/fingerprints all sources, publishes an incomplete generation, freezes legacy writers, then marks Brain complete.
- The completed generation includes a frozen-source manifest; every model attempt cheaply rejects changed inode/size/time metadata or recreated legacy paths as split-brain.
- Exact historical proposal plus terminal Allowed/Denied becomes response-ineligible commitment; mismatches remain incomplete/diagnostic.
- The exact `4vh58` shape migrates without blocking unrelated projection and preserves DeliveryUnknown plus later outcome.
- Review migration failure remains isolated and preserves `review-state.json`.

- [ ] **Step 1: Build frozen legacy fixtures and failing migration tests**

```rust
#[test]
fn migration_reconciles_4vh58_without_response_authority() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    MigrationCoordinator::at(fixture.state_root()).run_non_hook().unwrap();
    let db = fixture.open_brain();
    let commit = db.permission_commit("decision-4vh58").unwrap().unwrap();
    assert!(!commit.response_eligible);
    assert_eq!(commit.delivery, DeliveryState::Unknown);
    assert!(db.outcome_for("activity-4vh58").unwrap().is_some());
}
```

- [ ] **Step 2: Verify migration tests fail**

Run: `nix develop path:. --command cargo test --test storage_migration migration_reconciles_4vh58 -- --exact`

Expected: compilation fails on the migration coordinator.

- [ ] **Step 3: Implement staged import and cutover state machine**

Use explicit states `Building`, `Verified`, `BrainPublishedIncomplete`, `LegacyFrozen`, and `Complete`. Every resume validates exact files, ownership, modes, link counts, fingerprints, and database generation before advancing. At freeze, publish a manifest of exact path, inode, size, and modification metadata. Expose a bounded manifest check for Task 8 to activate before model inference; a mismatch or recreated path is split-brain. Do not delete uncertain staging or legacy evidence.

- [ ] **Step 4: Run crash/source-race migration matrix**

Run: `nix develop path:. --command cargo test --test storage_migration -- --test-threads=1`

Expected: all schema, tail, corruption, process-kill, source-race, pre-opened-writable-descriptor, frozen-manifest, and review-isolation cases pass.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/legacy.rs src/brain/storage/migration.rs src/brain/activity.rs src/brain/decisions.rs src/brain/permission_transaction.rs src/brain/review_state.rs crates/coding-brain-core/src/lifecycle/store.rs tests/storage_migration.rs tests/fixtures/storage
git commit -m "🗃️ feat: migrate legacy Brain state atomically (codexctl-2o9fo)"
```

### Task 8: Runtime, Lifecycle Hook, Recovery, and TUI Cutover

**Files:**
- Modify: `src/lifecycle_hook.rs`
- Modify: `src/brain/permission_hook.rs`
- Modify: `src/brain/permission_transaction.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `src/brain/recovery.rs`
- Modify: `src/doctor.rs`
- Modify: `src/main.rs`
- Modify: `tests/{hook_activity,lifecycle_hook_cli,headless_activity,brain_tui_smoke,integration_tests}.rs`

**Interfaces:**
- Consumes: current Brain/review database facades and `MigrationCoordinator` on non-hook startup.
- Produces: runtime sources/actions that no longer construct live JSONL stores or run permission-journal recovery.
- Preserves: `SessionLinkStore` append/projection order and fail-closed guarded action semantics.

**Acceptance Criteria:**
- Tasks 1-7, 9, and 10 are complete; verified migration, downgrade export, erasure recovery, and WAL admission safeguards exist before activation.
- Every live decision, lifecycle, activity, review, recovery, correction, Scorecard, and TUI path reads/writes SQLite.
- Hooks never migrate and return provider-neutral/native behavior for unavailable current storage.
- TUI Busy/error refresh retains the last coherent view and never blanks because storage recovery is blocked.
- Session-link failure disables navigation/recovery action without changing permission authority.
- No live code path references permission journals, `activity.jsonl`, `decisions.jsonl`, `review-state.json`, or lifecycle JSON except migration/export.

- [ ] **Step 1: Add failing no-live-JSONL integration assertions**

```rust
#[test]
fn current_runtime_writes_only_sqlite_and_session_links() {
    let run = CbrainFixture::new().run_permission_and_lifecycle();
    assert!(run.state_root.join("db/brain.sqlite3").exists());
    assert!(run.state_root.join("session-links.jsonl").exists());
	for legacy in ["activity.jsonl", "brain/decisions.jsonl", "review-state.json", "hooks/lifecycle.json"] {
		assert!(!run.state_root.join(legacy).exists(), "unexpected live legacy store: {legacy}");
    }
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test integration_tests current_runtime_writes_only_sqlite -- --exact`

Expected: legacy files are still created.

- [ ] **Step 3: Switch runtime constructors and remove recovery coupling**

Route non-hook startup through migration before building TUI sources. In this one task, switch permission and lifecycle hooks through `OpenRole::Hook`, disable every live permission-journal writer/recovery caller, and activate frozen-manifest plus WAL-hard-limit admission. Replace journal recovery status with storage migration/maintenance status. Keep old readers reachable only through `storage::legacy`.

- [ ] **Step 4: Run runtime and TUI suites**

Run: `nix develop path:. --command cargo test --test lifecycle_hook_cli --test headless_activity --test brain_tui_smoke --test integration_tests -- --test-threads=1`

Expected: all runtime paths pass with SQLite-only live storage and coherent degraded views.

- [ ] **Step 5: Commit**

```bash
git add src/lifecycle_hook.rs src/brain/permission_hook.rs src/brain/permission_transaction.rs src/runtime/brain.rs src/brain/recovery.rs src/doctor.rs src/main.rs tests/hook_activity.rs tests/lifecycle_hook_cli.rs tests/headless_activity.rs tests/brain_tui_smoke.rs tests/integration_tests.rs
git commit -m "♻️ refactor: cut runtime over to SQLite storage (codexctl-2o9fo)"
```

### Task 9: Audit Export, Frozen Downgrade Export, and Review Reset CLI

**Files:**
- Create: `src/brain/storage/export.rs`
- Modify: `src/main.rs`
- Modify: `tests/config_mode_cli.rs`
- Create: `tests/storage_export.rs`
- Modify: `tests/public_namespace.rs`

**Interfaces:**
- Produces CLI: `cbrain storage export-audit <directory>`, `cbrain storage export-legacy <directory>`, and `cbrain storage reset-review-state`.
- Produces: `AuditExporter` and `LegacyExporter` using short read snapshots and no live-state replacement.
- Consumes: frozen `legacy-v0.59.1` writers/readers and both SQLite databases.

**Acceptance Criteria:**
- Audit export is stable, bounded, redacted, and clearly non-executable.
- Legacy export writes only a new owner-only directory, refuses overwrite, round-trips through frozen readers, and rejects lossy evidence.
- Legacy export refuses to run while a privacy-erasure generation is incomplete and cannot emit logically erased payloads.
- Review reset affects only `review.sqlite3` and requires no deletion of Brain authority.
- No command swaps exported state into the live state root.

- [ ] **Step 1: Add failing CLI/export tests**

```rust
#[test]
fn legacy_export_round_trips_delivery_unknown() {
    let fixture = sqlite_fixture_with_delivery_unknown();
    let output = fixture.temp.path().join("legacy-export");
    fixture.run(["storage", "export-legacy", output.to_str().unwrap()]).success();
    let legacy = FrozenLegacyReader::open(&output).unwrap();
    assert_eq!(legacy.decision("decision-1").unwrap().delivery, DeliveryState::Unknown);
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test storage_export legacy_export_round_trips -- --exact`

Expected: command/API is missing.

- [ ] **Step 3: Implement static CLI and streaming exporters**

Add a `StorageAction` Clap subcommand. Use temporary files inside the new output directory, sync every file, validate with frozen readers, then atomically publish each final filename. Refuse any pre-existing output directory entry.

- [ ] **Step 4: Run export and CLI gates**

Run: `nix develop path:. --command cargo test --test storage_export --test config_mode_cli --test public_namespace storage`

Expected: export, overwrite refusal, semantic round-trip, and reset tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/export.rs src/main.rs tests/storage_export.rs tests/config_mode_cli.rs tests/public_namespace.rs
git commit -m "✨ feat: export and reset SQLite storage safely (codexctl-2o9fo)"
```

### Task 10: WAL Maintenance, Disk-Full Semantics, and Doctor Evidence

**Files:**
- Create: `src/brain/storage/maintenance.rs`
- Modify: `src/brain/storage/mod.rs`
- Modify: `src/doctor.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `tests/sqlite_storage.rs`
- Create: `tests/storage_faults.rs`

**Interfaces:**
- Produces: `StorageHealth`, `WalHealth`, `MaintenanceOutcome`, `BrainDb::{health,maintain_bounded,deep_integrity_check}`.
- Consumes: fixed warning/hard WAL thresholds, absolute deadlines, SQLite extended error codes, retention cursor, and last coherent runtime snapshot.

**Acceptance Criteria:**
- Headless hooks auto-checkpoint normally; warning/hard WAL thresholds are measured and fail closed without undoing committed WAL transactions.
- Disk-full/I/O errors preserve operation-specific admission/commit/delivery/checkpoint semantics.
- Doctor reports database path, schema, bundled SQLite version, migration status, WAL size, integrity state, and fixed redacted error categories.
- Hook paths never run vacuum, bulk retention, or full integrity checks.
- All maintenance and fault behavior is complete and directly tested before Task 8 activates SQLite production writers.

- [ ] **Step 1: Write failing WAL and disk-full tests**

```rust
#[test]
fn hard_wal_limit_pauses_model_inference_but_not_deterministic_deny() {
    let fixture = StorageFaultFixture::wal_above_hard_limit();
    assert!(matches!(fixture.brain.admit_model_attempt(), Err(StorageError::MaintenanceRequired)));
    assert_eq!(fixture.run_deterministic_deny().response, DenyResponse::Denied);
}
```

- [ ] **Step 2: Verify failure**

Run: `nix develop path:. --command cargo test --test storage_faults hard_wal_limit -- --exact`

Expected: maintenance/fault APIs are missing.

- [ ] **Step 3: Implement bounded maintenance and error mapping**

Map extended SQLite errors at each call site; do not collapse `FULL`, `BUSY`, `CORRUPT`, and commit uncertainty. Run manual checkpoints and incremental vacuum only from non-hook roles. Surface health without stored content or caller-controlled paths.

- [ ] **Step 4: Run fault and Doctor suites**

Run: `nix develop path:. --command cargo test --test storage_faults --test sqlite_storage doctor -- --test-threads=1`

Expected: page-limit, write, sync, checkpoint, corruption, and recovery tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/brain/storage/maintenance.rs src/brain/storage/mod.rs src/doctor.rs src/runtime/brain.rs tests/sqlite_storage.rs tests/storage_faults.rs
git commit -m "🩺 feat: diagnose and bound SQLite maintenance (codexctl-2o9fo)"
```

### Task 11: Adversarial Provider, Concurrency, Scale, and Package Verification

**Files:**
- Modify: `tests/hook_activity.rs`
- Modify: `tests/lifecycle_hook_cli.rs`
- Modify: `tests/activity_scale.rs`
- Modify: `tests/storage_migration.rs`
- Modify: `tests/storage_faults.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `flake.nix` only if bundled build inputs require it

**Interfaces:**
- Consumes all prior storage/runtime APIs; produces no new production abstraction unless a failing test proves one is required.

**Acceptance Criteria:**
- Separate-process same-request, independent-burst, reader pinning, checkpoint, source-race, crash, and disk-full matrices pass.
- A store larger than the old cumulative 16 MiB limit performs permission commit and critical projections without history scans.
- Codex, Claude, and Antigravity retain native-authority fallback and zero response replay.
- Linux, macOS, x86_64 musl, aarch64 musl, Nix debug checks, and crates.io packaging work without a system SQLite library.

- [ ] **Step 1: Add the full adversarial matrix before production adjustments**

Create parameterized process tests over:

```rust
enum FaultPoint {
    AdmissionWrite,
    InferenceExit,
    CommitBeforeSync,
    CommitAfterSync,
    StdoutWrite,
    DeliveryWrite,
    Checkpoint,
    MigrationPublish,
}
```

Assert exact persisted rows, stdout bytes, native fallback, and restart state for every provider/fault pair.

- [ ] **Step 2: Run the matrix and record any real failure**

Run: `nix develop path:. --command cargo test --test hook_activity --test lifecycle_hook_cli --test activity_scale --test storage_migration --test storage_faults -- --test-threads=1`

Expected: all adversarial tests pass; any failure is fixed at its owning earlier module without weakening thresholds.

- [ ] **Step 3: Add CI/package gates for bundled SQLite**

Add a CI assertion that release artifacts do not dynamically require `libsqlite3`, while preserving existing musl target commands and Nix `checkType = "debug"`.

- [ ] **Step 4: Run packageability separately**

Run: `nix develop path:. --command cargo package --workspace --allow-dirty`

Expected: every publishable crate packages; root package includes all storage modules and no local state.

Run: `nix build path:.`

Expected: Nix package builds and runs tests with bundled SQLite.

- [ ] **Step 5: Commit**

```bash
git add tests/hook_activity.rs tests/lifecycle_hook_cli.rs tests/activity_scale.rs tests/storage_migration.rs tests/storage_faults.rs .github/workflows/ci.yml flake.nix
git commit -m "✅ test: stress SQLite storage across providers (codexctl-2o9fo)"
```

### Task 12: Remove Legacy Live Storage and Complete Documentation

**Files:**
- Modify/remove live portions: `src/brain/{activity,decisions,permission_transaction,review_state}.rs`
- Modify/remove persistence portions: `crates/coding-brain-core/src/lifecycle/store.rs`
- Modify: `docs/decisions/ADR-0003-fail-safe-hook-and-learning-persistence.md`
- Verify: `docs/decisions/ADR-0006-use-sqlite-for-brain-and-lifecycle-state.md`
- Modify: `docs/decisions/INDEX.md`
- Modify: `docs/{architecture,configuration,reference,troubleshooting}.md`
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `.github/releases/` only when preparing an authorized release

**Interfaces:**
- Consumes the final SQLite runtime; removes obsolete live JSONL/journal APIs after `rg` proves no runtime caller remains.

**Acceptance Criteria:**
- No live code constructs or recovers JSONL decisions/activity/review/lifecycle permission stores.
- Legacy code is bounded under `storage::legacy` and reachable only from migration/export/tests.
- ADR-0003 explicitly points to ADR-0006 for superseded persistence mechanics.
- User documentation covers paths, automatic non-hook migration, pre-migration hook behavior, separate review failure, Doctor/deep checks, reset, audit export, downgrade export, local-filesystem/WAL limits, privacy erasure, and unchanged session links.
- Full workspace, Clippy, formatting, build, package, Nix, and normalized diff gates pass from a fresh invocation.

- [ ] **Step 1: Add source-boundary regressions**

Extend `tests/removed_surfaces.rs` to scan production source and allow legacy filenames only under `src/brain/storage/legacy.rs`, migration/export modules, documentation, and tests.

- [ ] **Step 2: Run the boundary test and remove every live hit**

Run: `nix develop path:. --command cargo test --test removed_surfaces`

Expected: passes with no live JSONL/journal constructor.

- [ ] **Step 3: Update ADR and human-facing documentation**

Document exact commands and failure semantics. Do not claim deployment or release acceptance. Keep the existing `session-links.jsonl` explanation and distinguish audit export from executable downgrade state.

- [ ] **Step 4: Run final fresh quality gates**

Run: `nix develop path:. --command cargo test --workspace --all-targets -- --test-threads=1`

Expected: all unit and integration suites pass.

Run: `nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings`

Expected: zero warnings.

Run: `nix develop path:. --command cargo fmt --all -- --check`

Expected: no formatting diff.

Run: `nix develop path:. --command cargo build --workspace --all-targets`

Expected: build succeeds.

Run: `nix develop path:. --command cargo package --workspace --allow-dirty`

Expected: every publishable crate packages from the final cleaned source tree.

Run: `nix build path:.`

Expected: the final Nix package builds and checks with bundled SQLite.

Run: `git diff --check`

Expected: no whitespace errors.

- [ ] **Step 5: Commit documentation and cleanup**

```bash
git add src/brain crates/coding-brain-core/src/lifecycle docs README.md CHANGELOG.md tests/removed_surfaces.rs
git commit -m "📝 docs: document SQLite storage cutover (codexctl-2o9fo)"
```

## Dependency Order

Implementation is serial because the schema and fixtures are shared mutable state, even when different workers own successive tasks:

1. Run the preflight baseline.
2. Tasks 1 through 7 run in numeric order and add only inactive SQLite adapters plus migration.
3. Task 9 adds verified rollback/export before activation.
4. Task 10 adds WAL, disk-failure, and maintenance safeguards before activation.
5. Task 8 is the sole production runtime cutover.
6. Task 11 runs the full adversarial/provider/package matrix against the active runtime.
7. Task 12 removes obsolete live surfaces, updates documentation, and repeats every final gate.

Subagent-driven execution may use a fresh worker and reviewers for each task, but only one implementation task may edit the worktree at a time.

## Stress Test Results: Unified SQLite Implementation Plan

### Resolved Decisions

- Task 8 alone activates SQLite in production; Task 6 builds and tests inactive permission APIs without creating a partial live split.
- Verified downgrade export and WAL/disk safeguards must exist before runtime activation.
- Task 1 freezes an explicit schema invariant matrix and fixture rather than leaving authority constraints to implementation judgment.
- Lifecycle topology uses typed relational tables, with bounded JSON limited to provider-specific extras.
- Privacy erasure is a durable, resumable generation; learning reads and downgrade export fail closed while it is incomplete.
- A frozen-source manifest detects post-cutover mutation, including legacy paths held open by older processes.
- Review marks are exact-cursor visibility hints; newer events resurface and missing-cursor marks are harmless orphans.
- One absolute hook deadline starts before admission and includes inference plus every later storage retry.
- Activity cursors use the exact positive SQLite integer domain and fail closed at `i64::MAX`.
- Implementation tasks run serially because they share the schema and integration fixtures.
- `forget()` guarantees logical erasure from Coding Brain-managed stores, not physical media, snapshots, or backups.
- Verification brackets the work with a full baseline and final workspace, package, Nix, and exact-head CI gates.

### Changes Made

- Added a preflight baseline and final package/Nix repetition.
- Added the schema invariant matrix, signed cursor boundary, resumable erasure protocol, frozen-source manifest, cross-database review race semantics, and absolute deadline contract.
- Moved production permission-hook activation out of Task 6 and made Task 8 depend operationally on migration, export, and maintenance completion.
- Replaced the parallel-looking dependency order with a single serial implementation sequence.

### Deferred / Parking Lot

- Physical-media and external-backup erasure remain outside Coding Brain's supported guarantee.
- Cross-platform and musl verification runs on the final exact commit when PR publication is separately authorized.

### Confidence Assessment

- Overall: High
- Areas of concern: migration freeze behavior with older live processes and privacy-erasure crash recovery remain the highest-risk implementation areas and require process-boundary fault injection before cutover.
