# Legacy Brain Proposal Migration Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Migrate valid pre-SQLite hook proposals with `brain_source: "brain"` as canonical model-sourced historical permission evidence without granting live response authority or weakening unknown-source rejection.

**Architecture:** Keep the compatibility alias local and explicit in the two legacy migration source matches: initial import and exact replay/accounting validation. Exercise both paths with a dedicated raw two-decision fixture and focused integration tests covering authority isolation, negative correlation cases, and Building/Verified restart recovery.

**Tech Stack:** Rust 2024 workspace, `rusqlite`, JSONL legacy fixtures, Cargo integration tests, feature-gated migration fault injection.

## Global Constraints

- Map only the legitimate historical proposal source `brain` to canonical SQLite source `model`.
- Preserve existing mappings for `model`, `deterministic`, and `provider_policy`; arbitrary unknown values remain fail-closed.
- Preserve the original `brain` value in the legacy decision payload while storing `model` in the typed decision identity.
- Historical rows remain response-ineligible with delivery unknown and cannot satisfy live permission authority APIs.
- Incomplete, abstaining, mismatched, and non-terminal proposals remain skipped or non-authoritative.
- Building recovery may rebuild only the coordinator-owned staging database; it must preserve legacy bytes and the migration generation.
- Do not change schema, export, hook, provider, or runtime permission behavior.
- Do not refactor the duplicate migration matches into a general normalization helper.
- Local commits in the existing `fix-dzlb9` worktree are authorized. Do not
  push, sync, publish, open a PR, or close `codexctl-dzlb9.13` without further
  explicit user authorization.

---

## File Map

- Create `tests/fixtures/storage/legacy-brain-proposals/activity.jsonl`: raw Allowed and Denied terminal events for two Antigravity proposals.
- Create `tests/fixtures/storage/legacy-brain-proposals/brain/decisions.jsonl`: raw approve and deny hook proposals using `brain_source: "brain"`.
- Modify `tests/storage_migration.rs`: focused import, authority, negative-source, and restart regressions.
- Modify `src/brain/storage/migration.rs`: add the migration-only `brain` to `model` alias in import and exact-accounting matches.

### Task 1: Legacy Brain Proposal Compatibility

**Files:**

- Create: `tests/fixtures/storage/legacy-brain-proposals/activity.jsonl`
- Create: `tests/fixtures/storage/legacy-brain-proposals/brain/decisions.jsonl`
- Modify: `tests/storage_migration.rs`
- Modify: `src/brain/storage/migration.rs:4532-4541`
- Modify: `src/brain/storage/migration.rs:5264-5273`

**Interfaces:**

- Consumes: `LegacyFixture::copy`, `MigrationCoordinator`, `BrainDb`, `DecisionIdentity`, `HistoricalDeliveryState`, `PermissionAdmission`, `PermissionState`, and the existing feature-gated `migration_child` fault harness.
- Produces: no new public API; both migration paths translate legacy source `brain` to canonical decision source `model`.

**Acceptance Criteria:**

- Exactly correlated legacy allow and deny proposals with `brain_source: "brain"` import successfully.
- Typed decision identities use source `model`; preserved decision payloads retain source `brain`.
- Historical authority is readable as proposal-terminal evidence, response-ineligible, and delivery unknown.
- No live permission attempt or commit is synthesized, and a fresh live attempt with matching provider/session context has no committed decision.
- Missing-terminal, mismatched, and abstaining `brain` proposals remain incomplete; an exactly correlated arbitrary source is rejected.
- Building and Verified interruptions resume with unchanged legacy bytes and generation, publish only after success, and remain idempotently Complete.
- Focused tests and all workspace quality gates pass.

- [ ] **Step 1: Add the raw pre-cutover fixture**

Create `tests/fixtures/storage/legacy-brain-proposals/brain/decisions.jsonl` with two newline-terminated `HookDecisionRecord` objects. Use these exact identity/action/source combinations while retaining every bounded field present in the existing `permission-journal-4vh58` fixture:

```json
{"provider":"antigravity","ts":"2026-08-01T00:00:00Z","pid":1,"project":"fixture","tool":"Bash","command":"cargo test --allow","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"fixture allow","brain_source":"brain","brain_threshold":0.8,"user_action":"hook_proposal","decision_type":"session","suggested_at":1,"resolved_at":1,"decision_id":"legacy-brain-allow","session_id":"legacy-brain-session","turn_id":"step-5"}
{"provider":"antigravity","ts":"2026-08-01T00:00:01Z","pid":1,"project":"fixture","tool":"Bash","command":"cargo test --deny","brain_action":"deny","brain_confidence":0.9,"brain_reasoning":"fixture deny","brain_source":"brain","brain_threshold":0.8,"user_action":"hook_proposal","decision_type":"session","suggested_at":2,"resolved_at":2,"decision_id":"legacy-brain-deny","session_id":"legacy-brain-session","turn_id":"step-6"}
```

Create `tests/fixtures/storage/legacy-brain-proposals/activity.jsonl` with these two newline-terminated schema-v3 Decision events. Do not add outcome, journal, lifecycle, or review rows.

```json
{"schema_version":3,"kind":"decision","activity_id":"legacy-brain-activity-allow","recorded_at_ms":1000,"project":{"project_id":{"kind":"temporary","value":"fixture"},"cwd":"/fixture","label":"fixture"},"session":{"provider":"antigravity","session_id":"legacy-brain-session","turn_id":"step-5","tool_use_id":"step-5","project_id":{"kind":"temporary","value":"fixture"},"cwd":"/fixture","provenance":"structured"},"state":"allowed","tool":"Bash","normalized_command":"cargo test --allow","confidence":0.9,"threshold":0.8,"reasoning":"fixture allow","decision_id":"legacy-brain-allow"}
{"schema_version":3,"kind":"decision","activity_id":"legacy-brain-activity-deny","recorded_at_ms":2000,"project":{"project_id":{"kind":"temporary","value":"fixture"},"cwd":"/fixture","label":"fixture"},"session":{"provider":"antigravity","session_id":"legacy-brain-session","turn_id":"step-6","tool_use_id":"step-6","project_id":{"kind":"temporary","value":"fixture"},"cwd":"/fixture","provenance":"structured"},"state":"denied","tool":"Bash","normalized_command":"cargo test --deny","confidence":0.9,"threshold":0.8,"reasoning":"fixture deny","decision_id":"legacy-brain-deny"}
```

- [ ] **Step 2: Write the focused import/authority regression**

Extend imports in `tests/storage_migration.rs` with the exact public types used by the assertions:

```rust
use coding_brain::brain::storage::{
    BrainDb, DecisionIdentity, FrozenSourceManifest, HistoricalDeliveryState,
    LEGACY_EXPORT_PROFILE, LegacyFreezeArtifact, LegacySourceKind, LegacySourceSet,
    LegacyWriterGuard, MigrationCoordinator, MigrationStatus, OpenRole,
    PermissionAdmission, PermissionState, StorageDeadline, StorageError, StoragePaths,
};
use coding_brain_core::project::ProjectId;
```

Add `legacy_brain_proposals_import_as_model_historical_non_authority` next to `migration_reconciles_4vh58_without_response_authority`. The test must:

```rust
let fixture = LegacyFixture::copy("legacy-brain-proposals");
assert_eq!(
    MigrationCoordinator::at(fixture.state_root())
        .run_non_hook()
        .unwrap(),
    MigrationStatus::Complete
);

let paths = StoragePaths::at(fixture.state_root());
let mut db = BrainDb::open_current(
    &paths,
    OpenRole::NonHook,
    StorageDeadline::after(Duration::from_secs(2)),
)
.unwrap();

for (decision_id, action) in [
    ("legacy-brain-allow", PermissionAction::Allow),
    ("legacy-brain-deny", PermissionAction::Deny),
] {
    let identity = db.decision_identity(decision_id).unwrap().unwrap();
    assert!(matches!(
        identity,
        DecisionIdentity::Permission {
            authority_action,
            ref decision_source,
            ..
        } if authority_action == action && decision_source == "model"
    ));
    assert_eq!(
        db.decision_payload(decision_id)
            .unwrap()
            .unwrap()
            .record
            .brain_source,
        "brain"
    );
}

let historical = db
    .historical_permission_authority_after(None, 10, 1024 * 1024)
    .unwrap();
assert_eq!(historical.authorities.len(), 2);
assert!(historical.authorities.iter().all(|row| {
    !row.response_eligible && row.delivery_state == HistoricalDeliveryState::Unknown
}));
```

Before opening `BrainDb`, prove migration synthesized no live rows:

```rust
let connection = rusqlite::Connection::open(
    fixture.state_root().join("db/brain.sqlite3"),
)
.unwrap();
assert_eq!(
    connection
        .query_row(
            "SELECT (SELECT count(*) FROM permission_attempts),
                    (SELECT count(*) FROM permission_commits)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
    (0, 0)
);
drop(connection);
```

After the historical assertions, admit a fresh request with matching provider/session/project context and prove it has no committed decision:

```rust
let live_identity = LifecycleIdentity::try_new(
    AgentProvider::Antigravity,
    "legacy-brain-session".into(),
    Some("step-5".into()),
    None,
    "/fixture".into(),
)
.unwrap();
let admission = PermissionAdmission::new(
    live_identity,
    "a".repeat(64),
    ProjectId::Temporary("fixture".into()),
    "Bash",
    Some("live-tool-use".into()),
    "live-activity",
    3000,
    3001,
);
let guard = db.admit_permission(admission).unwrap().unwrap();
assert_eq!(
    db.permission_state(guard.attempt_id()).unwrap(),
    PermissionState::Absent
);
assert_eq!(
    db.permission_decision(guard.attempt_id()).unwrap(),
    None
);
```

This behaviorally proves that historical evidence cannot become a live response authority.

- [ ] **Step 3: Run the focused import test and verify the current failure**

Run:

```bash
cargo test --test storage_migration legacy_brain_proposals_import_as_model_historical_non_authority -- --exact --nocapture
```

Expected before implementation: FAIL with `InvalidStorage("legacy proposal decision source is unsupported")`.

- [ ] **Step 4: Add the minimal compatibility mapping to initial import**

In `MigrationImport::import_hook_decision`, change only the source match:

```rust
let source = match record.brain_source.as_str() {
    "model" | "brain" => "model",
    "deterministic" => "deterministic_safety",
    "provider_policy" => "native_provider",
    _ => {
        return Err(StorageError::InvalidStorage(
            "legacy proposal decision source is unsupported",
        ));
    }
};
```

- [ ] **Step 5: Run the import test and confirm restart-accounting still needs parity**

Run the same focused test. Expected: PASS for the normal import. Do not treat this as completion; Verified restart coverage in Step 8 must exercise `MigrationAccountingValidator::exact_hook_decision`.

- [ ] **Step 6: Add the identical compatibility mapping to exact accounting/revalidation**

In `MigrationAccountingValidator::exact_hook_decision`, make the same one-arm change and leave every error and correlation check intact:

```rust
let source = match record.brain_source.as_str() {
    "model" | "brain" => "model",
    "deterministic" => "deterministic_safety",
    "provider_policy" => "native_provider",
    _ => {
        return Err(StorageError::InvalidStorage(
            "legacy proposal decision source is unsupported",
        ));
    }
};
```

- [ ] **Step 7: Add negative source/correlation coverage**

Add this focused removal helper next to `rewrite_json_lines`:

```rust
fn remove_json_lines(
    path: &std::path::Path,
    mut remove: impl FnMut(&serde_json::Value) -> bool,
) {
    let input = fs::read_to_string(path).unwrap();
    let mut output = String::new();
    for line in input.lines() {
        let value = serde_json::from_str(line).unwrap();
        if !remove(&value) {
            output.push_str(line);
            output.push('\n');
        }
    }
    write_private(path, output.as_bytes());
}
```

Add a table-driven `legacy_brain_source_alias_does_not_promote_incomplete_proposals` test. For each case, copy `legacy-brain-proposals`, mutate only the allow proposal/event, run migration, and assert `legacy-brain-allow` has no identity or historical authority while accounting increments `skips.incomplete_proposals`:

```rust
for case in ["missing-terminal", "session-mismatch", "abstain"] {
    let fixture = LegacyFixture::copy("legacy-brain-proposals");
    match case {
        "missing-terminal" => remove_json_lines(
            &fixture.state_root().join("activity.jsonl"),
            |value| value["decision_id"] == "legacy-brain-allow",
        ),
        "session-mismatch" => rewrite_json_lines(
            &fixture.state_root().join("brain/decisions.jsonl"),
            |index, value| {
                if index == 0 {
                    value["session_id"] = serde_json::Value::from("other-session");
                }
            },
        ),
        "abstain" => rewrite_json_lines(
            &fixture.state_root().join("brain/decisions.jsonl"),
            |index, value| {
                if index == 0 {
                    value["brain_action"] = serde_json::Value::from("abstain");
                }
            },
        ),
        _ => unreachable!(),
    }

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete,
        "{case}"
    );
    let db = BrainDb::open_current(
        &StoragePaths::at(fixture.state_root()),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    assert_eq!(db.decision_identity("legacy-brain-allow").unwrap(), None);
    assert!(
        db.historical_permission_authority_after(None, 10, 1024 * 1024)
            .unwrap()
            .authorities
            .iter()
            .all(|row| row.decision_id != "legacy-brain-allow"),
        "{case}"
    );
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["skips"]
            ["incomplete_proposals"],
        1,
        "{case}"
    );
}
```

Add `legacy_migration_rejects_unknown_exact_proposal_source`. Change only the allow proposal source to `future_source`, retain its exact Allowed terminal, and assert:

```rust
let fixture = LegacyFixture::copy("legacy-brain-proposals");
rewrite_json_lines(
    &fixture.state_root().join("brain/decisions.jsonl"),
    |index, value| {
        if index == 0 {
            value["brain_source"] = serde_json::Value::from("future_source");
        }
    },
);
assert!(matches!(
    MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
    Err(StorageError::InvalidStorage(
        "legacy proposal decision source is unsupported"
    ))
));
```

Do not weaken `validate_hook_decision_record`; the unknown non-empty source must reach the migration-specific fail-closed match.

- [ ] **Step 8: Add Building and Verified restart regressions**

Under `#[cfg(feature = "fault-injection")]`, add `legacy_brain_proposal_building_and_verified_restarts_complete_safely`. Iterate over `["building", "verified"]`; for each fault, the test must:

```rust
let fixture = LegacyFixture::copy("legacy-brain-proposals");
let decisions_before = fs::read(fixture.state_root().join("brain/decisions.jsonl")).unwrap();
let activity_before = fs::read(fixture.state_root().join("activity.jsonl")).unwrap();

assert!(!migration_child(fixture.state_root(), fault).success());
let state_before = migration_state(fixture.state_root());
let generation = state_before["generation"].as_u64().unwrap();
assert_eq!(state_before["status"], fault);
assert!(!fixture.state_root().join("db/brain.sqlite3").exists());

let coordinator = MigrationCoordinator::at(fixture.state_root());
assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
assert_eq!(migration_state(fixture.state_root())["generation"], generation);
assert_eq!(fs::read(fixture.state_root().join("brain/decisions.jsonl")).unwrap(), decisions_before);
assert_eq!(fs::read(fixture.state_root().join("activity.jsonl")).unwrap(), activity_before);
assert!(fixture.state_root().join("db/brain.sqlite3").exists());
assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
```

For the Verified iteration, successful resume must necessarily pass exact replay/accounting with the legacy `brain` payload and canonical `model` identity. For the Building iteration, assert an owned staging database exists before resume so the test covers discard-and-rebuild recovery.

- [ ] **Step 9: Run focused tests**

Run:

```bash
cargo test --features fault-injection --test storage_migration legacy_brain -- --nocapture
```

Expected: every `legacy_brain*` test passes, including both fault boundaries.

- [ ] **Step 10: Format and run workspace quality gates**

Run in order:

```bash
cargo fmt
cargo fmt --check
cargo test
cargo test --features fault-injection --test storage_migration
cargo build
cargo clippy -- -D warnings
```

Expected: all commands exit 0 with no test failures, formatting differences, build errors, or Clippy warnings. If formatting fails, run `cargo fmt`, inspect that only task-owned Rust files changed, then rerun `cargo fmt --check`.

- [ ] **Step 11: Verify surgical scope and hand off**

Run:

```bash
git diff --check
git status --short
git diff -- src/brain/storage/migration.rs tests/storage_migration.rs tests/fixtures/storage/legacy-brain-proposals .internal/specs/2026-08-10-legacy-brain-proposal-migration-compatibility-design.md .internal/plans/2026-08-10-legacy-brain-proposal-migration-compatibility.md
```

Expected: only the two migration match arms, focused tests/fixtures, and approved workflow documents differ. Local commits are authorized; report validation evidence and await explicit authorization before closing `codexctl-dzlb9.13`, syncing, pushing, publishing, or opening a PR.
