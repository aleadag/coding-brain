# Deterministic Historical Authority Reader Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make every migration-supported canonical historical permission source readable through learning, review, and TUI refresh without granting live response authority, and carry stable non-busy storage fault categories to the TUI.

**Architecture:** Define the canonical historical decision-source domain beside `DecisionIdentity`; use one migration-only legacy conversion in import and replay/accounting, and reconstruct historical validation identities from the stored canonical source. Keep Busy as its existing retryable runtime error and add a typed non-busy storage-unavailable error for Full, I/O, Corrupt, and Other so the TUI can retain the last coherent projection and render the real category.

**Tech Stack:** Rust 2024 workspace, `rusqlite`, JSONL migration fixtures, Ratatui application state, Cargo integration and unit tests.

## Global Constraints

- Canonical historical sources are exactly `model`, `deterministic_safety`, and `native_provider`.
- Legacy source aliases remain migration-only: `model|brain -> model`, `deterministic -> deterministic_safety`, and `provider_policy -> native_provider`.
- Unknown legacy or canonical sources and inconsistent authority/activity relationships remain fail-closed `InvalidStorage` errors.
- Historical allow and deny rows remain response-ineligible with delivery unknown and cannot satisfy live permission response or delivery APIs.
- Do not change the SQLite schema, export format, live deterministic-safety policy, migration publication/freeze protocol, or whole-directory recovery behavior.
- Preserve generic source-error redaction, status precedence, and Busy retry semantics.
- Run every state-dependent test command below with isolated `HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` from its first invocation, while preserving `CARGO_HOME=/home/alexander/.cargo`; bare `cargo test` examples never authorize use of operator state.
- Do not touch `~/.local/state/coding-brain/db.failed-fresh-20260810-2058` or any other live state during implementation or tests.
- Do not commit, push, sync, publish, open a PR, or close `codexctl-dzlb9.16` without separate user authorization.

---

## File Map

- Modify `src/brain/storage/decisions.rs`: own and parse the canonical historical-source type beside `DecisionIdentity`.
- Modify `src/brain/storage/migration.rs`: convert legacy proposal sources once and use the typed result in import and replay/accounting.
- Modify `src/brain/storage/permissions.rs`: validate and reconstruct historical authority with the stored canonical source.
- Create `src/brain/storage/test_support.rs`: provide one test-only deterministic historical database fixture for review and runtime unit tests.
- Modify `src/brain/storage/mod.rs`: expose `test_support` only under `cfg(test)`.
- Modify `tests/storage_migration.rs`: cover migrated deterministic allow/deny readability and live-authority isolation.
- Modify `tests/sqlite_storage.rs`: cover all canonical sources, the production-shaped cursor-1189 row, null/present tool-use identity, and fail-closed corruption.
- Modify `src/brain/review.rs`: exercise the SQLite-backed non-interactive review reader with deterministic historical evidence and its fault category.
- Modify `src/runtime/brain.rs`: exercise the real SQLite refresh loader; later map binary storage faults into the typed core error.
- Modify `crates/coding-brain-core/src/runtime.rs`: define the non-busy storage category and typed `BrainSourceError` variant.
- Modify `crates/coding-brain-tui/src/brain_app.rs`: render typed storage failures while retaining all coherent view state.

### Task 1: Canonical Historical Source Compatibility

**Files:**

- Modify: `src/brain/storage/decisions.rs:20-105`
- Modify: `src/brain/storage/migration.rs:4650-4685`
- Modify: `src/brain/storage/migration.rs:5380-5415`
- Modify: `src/brain/storage/permissions.rs:977-1095`
- Create: `src/brain/storage/test_support.rs`
- Modify: `src/brain/storage/mod.rs:1-75`
- Modify: `tests/storage_migration.rs:440-590`
- Modify: `tests/sqlite_storage.rs:3900-4210`
- Modify: `src/brain/review.rs:350-455`
- Modify: `src/runtime/brain.rs:270-455`
- Modify: `src/runtime/brain.rs:1440-1500`

**Interfaces:**

- Produces: `pub(super) enum CanonicalHistoricalDecisionSource { Model, DeterministicSafety, NativeProvider }`.
- Produces: `CanonicalHistoricalDecisionSource::parse(&str) -> Result<Self, StorageError>` and `as_str(self) -> &'static str`.
- Produces: migration-local `legacy_proposal_source(&str) -> Result<CanonicalHistoricalDecisionSource, StorageError>`.
- Consumes: existing `DecisionIdentity`, `DecisionPayload`, `learning_read_session`, `historical_permission_authority_after`, `PermissionAdmission`, `sqlite_decisions`, and `LiveBrainSource::refresh_from_sqlite_store` contracts.

**Acceptance Criteria:**

- Exactly correlated legacy deterministic allow and deny proposals migrate as `deterministic_safety` and read through learning, non-interactive review, and the real SQLite runtime refresh path.
- Model, deterministic-safety, and native-provider historical rows use the same canonical parser; arbitrary sources remain invalid.
- The production-shaped decision `dec_1786178933696120619_4155783_0` is validated at cursor 1189 with a null tool-use ID.
- Provider, session, turn, tool-use, action, cursor, provenance, response eligibility, delivery state, and source inconsistencies remain fail closed.
- Historical deterministic evidence creates no live permission attempts or commits, remains response-ineligible with delivery unknown, and cannot satisfy a fresh matching admission.

- [ ] **Step 1: Add the failing migrated deterministic allow/deny regression**

In `tests/storage_migration.rs`, add `legacy_deterministic_proposals_are_closed_and_readable`. Reuse `LegacyFixture::copy("legacy-brain-proposals")` and `rewrite_json_lines` to change both raw proposal sources from `brain` to `deterministic`, preserving the existing exact Allowed and Denied terminals:

```rust
#[test]
fn legacy_deterministic_proposals_are_closed_and_readable() {
    let fixture = LegacyFixture::copy("legacy-brain-proposals");
    rewrite_json_lines(
        &fixture.state_root().join("brain/decisions.jsonl"),
        |_index, value| value["brain_source"] = serde_json::Value::from("deterministic"),
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let connection =
        rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
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

    let paths = StoragePaths::at(fixture.state_root());
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let page = db
        .learning_read_session()
        .unwrap()
        .page_after(None, 10, 1024 * 1024)
        .unwrap();
    assert_eq!(page.decisions.len(), 2);
    for decision_id in ["legacy-brain-allow", "legacy-brain-deny"] {
        assert!(matches!(
            db.decision_identity(decision_id).unwrap(),
            Some(DecisionIdentity::Permission { ref decision_source, .. })
                if decision_source == "deterministic_safety"
        ));
    }
    let historical = db
        .historical_permission_authority_after(None, 10, 1024 * 1024)
        .unwrap();
    assert_eq!(historical.authorities.len(), 2);
    assert!(historical.authorities.iter().all(|row| {
        !row.response_eligible && row.delivery_state == HistoricalDeliveryState::Unknown
    }));

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
    assert_eq!(db.permission_state(guard.attempt_id()).unwrap(), PermissionState::Absent);
    assert_eq!(db.permission_decision(guard.attempt_id()).unwrap(), None);
}
```

Under `cfg(feature = "fault-injection")`, add a Verified-restart regression so
the shared conversion is exercised by replay/accounting rather than only by
initial import:

```rust
#[test]
#[cfg(feature = "fault-injection")]
fn legacy_deterministic_verified_restart_uses_shared_source_policy() {
    let fixture = LegacyFixture::copy("legacy-brain-proposals");
    rewrite_json_lines(
        &fixture.state_root().join("brain/decisions.jsonl"),
        |_index, value| value["brain_source"] = serde_json::Value::from("deterministic"),
    );
    assert!(!migration_child(fixture.state_root(), "verified").success());
    assert_eq!(migration_state(fixture.state_root())["status"], "verified");
    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .resume()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = BrainDb::open_current(
        &StoragePaths::at(fixture.state_root()),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    assert_eq!(db.learning_decisions(10, 1024 * 1024).unwrap().len(), 2);
}
```

Add the already-public storage types used above to the test imports. Do not add journal or lifecycle authority to the fixture.

Also add a `#[cfg(test)] mod tests` beside the migration conversion helper and
put this table-driven unit test inside it. It must cover the complete
migration-only alias contract independently of the production regression
fixture:

```rust
#[test]
fn legacy_proposal_sources_map_to_the_canonical_domain() {
    for (legacy, canonical) in [
        ("model", "model"),
        ("brain", "model"),
        ("deterministic", "deterministic_safety"),
        ("provider_policy", "native_provider"),
    ] {
        assert_eq!(legacy_proposal_source(legacy).unwrap().as_str(), canonical);
    }
    assert!(matches!(
        legacy_proposal_source("future_authority"),
        Err(StorageError::InvalidStorage(_))
    ));
}
```

- [ ] **Step 2: Add canonical-source and production-shaped failing tests**

In `tests/sqlite_storage.rs`, add a helper that creates a current database, advances the compacted high-water, appends one exact terminal, inserts the decision, and adds its historical anchor:

```rust
fn historical_source_fixture(
    decision_id: &str,
    source: &str,
    action: PermissionAction,
    tool_use_id: Option<&str>,
    high_water_before: i64,
) -> (tempfile::TempDir, StoragePaths) {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [high_water_before],
        )
        .unwrap();
    drop(connection);

    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let state = match action {
        PermissionAction::Allow => ActivityState::Allowed,
        PermissionAction::Deny => ActivityState::Denied,
    };
    let mut terminal = decision_activity_event(
        "production-historical-activity",
        decision_id,
        1_786_178_933_696,
        state,
        Some(AgentProvider::Codex),
    );
    let session = terminal.session.as_mut().unwrap();
    session.session_id = "production-redacted-session".into();
    session.turn_id = Some("production-redacted-turn".into());
    session.tool_use_id = tool_use_id.map(str::to_owned);
    let cursor = db.append_activity(terminal).unwrap();

    let mut record = complete_decision(decision_id, AgentProvider::Codex);
    record.user_action = "hook_proposal".into();
    record.brain_action = match action {
        PermissionAction::Allow => "approve".into(),
        PermissionAction::Deny => "deny".into(),
    };
    db.insert_decision(
        &DecisionIdentity::permission(
            decision_id,
            AgentProvider::Codex,
            "production-redacted-session",
            "production-redacted-turn",
            tool_use_id.map(str::to_owned),
            action,
            source,
            1_786_178_933_000,
        ),
        &DecisionPayload::new(DecisionKind::Permission, cursor, record),
    )
    .unwrap();
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    insert_historical_authority_for_action(
        &connection,
        decision_id,
        cursor.get() as i64,
        action,
    );
    drop(connection);
    (root, paths)
}
```

The existing `insert_historical_authority` helper hard-codes allow. Add this
separate helper for the source/action matrix and call it from
`historical_source_fixture` instead:

```rust
fn insert_historical_authority_for_action(
    connection: &Connection,
    decision_id: &str,
    terminal_source_cursor: i64,
    action: PermissionAction,
) {
    let (action, state) = match action {
        PermissionAction::Allow => ("allow", "allowed"),
        PermissionAction::Deny => ("deny", "denied"),
    };
    connection
        .execute(
            "INSERT INTO historical_permission_authority (
                decision_id, terminal_source_cursor, decision_kind, authority_action,
                terminal_event_kind, terminal_event_state, terminal_action,
                provenance_kind, transaction_id, request_key,
                response_eligible, delivery_state
             ) VALUES (?1, ?2, 'permission', ?3,
                       'decision', ?4, ?3, 'proposal_terminal',
                       NULL, NULL, 0, 'unknown')",
            params![decision_id, terminal_source_cursor, action, state],
        )
        .unwrap();
}
```

Add these tests:

```rust
#[test]
fn production_deterministic_historical_authority_is_readable() {
    let (_root, paths) = historical_source_fixture(
        "dec_1786178933696120619_4155783_0",
        "deterministic_safety",
        PermissionAction::Deny,
        None,
        1188,
    );
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let page = db
        .learning_read_session()
        .unwrap()
        .page_after(None, 10, 1024 * 1024)
        .unwrap();
    assert_eq!(page.decisions[0].source_cursor.get(), 1189);
    assert_eq!(page.decisions[0].record.decision_id.as_deref(), Some("dec_1786178933696120619_4155783_0"));
}

#[test]
fn every_canonical_historical_source_uses_the_same_validator() {
    for (source, action, tool_use_id) in [
        ("model", PermissionAction::Allow, Some("tool-model")),
        ("deterministic_safety", PermissionAction::Allow, Some("tool-deterministic")),
        ("native_provider", PermissionAction::Deny, Some("tool-provider")),
    ] {
        let (_root, paths) = historical_source_fixture(
            &format!("historical-{source}"),
            source,
            action,
            tool_use_id,
            0,
        );
        let db = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(1)),
        )
        .unwrap();
        assert_eq!(db.learning_decisions(10, 1024 * 1024).unwrap().len(), 1, "{source}");
    }
}
```

Extend `historical_permission_corruption_fails_closed_for_audit_and_learning`
with `unknown-source`, `provider`, `session`, `turn`, `tool-use`, `action`, and
`cursor` cases. After disabling foreign keys and check constraints in that
corruption-only connection, use these exact mutations in its existing match:

```rust
"unknown-source" => {
    "UPDATE decision_identities SET decision_source = 'future_source'"
}
"provider" => "UPDATE decision_identities SET provider = 'claude'",
"session" => "UPDATE decision_identities SET session_id = 'other-session'",
"turn" => "UPDATE decision_identities SET turn_id = 'other-turn'",
"tool-use" => {
    "UPDATE decision_identities SET tool_use_id = 'other-tool-use'"
}
"action" => "UPDATE decision_identities SET authority_action = 'deny'",
"cursor" => {
    "UPDATE historical_permission_authority SET terminal_source_cursor = 2"
}
```

Keep the existing assertions that both historical audit and learning reads
return `InvalidStorage`; do not relax constraints outside this negative test.

- [ ] **Step 3: Add failing non-interactive review and real runtime refresh tests**

Add `#[cfg(test)] pub(crate) mod test_support;` to
`src/brain/storage/mod.rs`. Create `src/brain/storage/test_support.rs` with one
fixture shared by the review and runtime unit tests:

```rust
use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SessionTarget, SessionTargetProvenance,
};
use coding_brain_core::lifecycle::PermissionAction;
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;
use rusqlite::params;

use crate::brain::decisions::{DecisionRecord, DecisionType};

use super::{
    BrainDb, DecisionIdentity, DecisionKind, DecisionPayload, ReviewDb, StoragePaths,
};

pub(crate) fn deterministic_historical_fixture(
    decision_id: &str,
) -> (tempfile::TempDir, StoragePaths) {
    let root = tempfile::tempdir().unwrap();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    drop(ReviewDb::create_current(&paths).unwrap());
    let project_id = ProjectId::Temporary("historical-test".into());
    let cursor = brain
        .append_activity(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: format!("activity-{decision_id}"),
            recorded_at_ms: 2_000,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: "/fixture".into(),
                label: Some("fixture".into()),
            },
            session: Some(SessionTarget {
                provider: AgentProvider::Codex,
                session_id: "historical-session".into(),
                provider_session_id: None,
                turn_id: Some("historical-turn".into()),
                tool_use_id: None,
                project_id,
                cwd: "/fixture".into(),
                provider_hints: Vec::new(),
                provenance: SessionTargetProvenance::Structured,
            }),
            state: ActivityState::Denied,
            tool: Some("Bash".into()),
            normalized_command: Some("printf redacted".into()),
            fingerprint: None,
            rule_id: None,
            confidence: Some(0.95),
            threshold: Some(0.8),
            reasoning: Some("fixture".into()),
            decision_id: Some(decision_id.into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .unwrap();
    let record = DecisionRecord {
        provider: AgentProvider::Codex,
        timestamp: "2026-08-10T00:00:00Z".into(),
        pid: 1,
        project: "fixture".into(),
        tool: Some("Bash".into()),
        command: Some("printf redacted".into()),
        brain_action: "deny".into(),
        brain_confidence: 0.95,
        brain_reasoning: "fixture".into(),
        user_action: "hook_proposal".into(),
        context: None,
        outcome: None,
        decision_type: DecisionType::Session,
        suggested_at: Some(1),
        resolved_at: Some(2),
        override_reason: None,
        decision_id: Some(decision_id.into()),
        brain_decision_ms: None,
        cache_hit: None,
        canonical: None,
    };
    brain
        .insert_decision(
            &DecisionIdentity::permission(
                decision_id,
                AgentProvider::Codex,
                "historical-session",
                "historical-turn",
                None,
                PermissionAction::Deny,
                "deterministic_safety",
                1_000,
            ),
            &DecisionPayload::new(DecisionKind::Permission, cursor, record),
        )
        .unwrap();
    brain
        .connection
        .execute(
            "INSERT INTO historical_permission_authority (
                decision_id, terminal_source_cursor, decision_kind, authority_action,
                terminal_event_kind, terminal_event_state, terminal_action,
                provenance_kind, transaction_id, request_key,
                response_eligible, delivery_state
             ) VALUES (?1, ?2, 'permission', 'deny', 'decision', 'denied', 'deny',
                       'proposal_terminal', NULL, NULL, 0, 'unknown')",
            params![decision_id, cursor.get() as i64],
        )
        .unwrap();
    drop(brain);
    (root, paths)
}
```

In `src/brain/review.rs` tests, import this helper and call the private
production reader directly:

```rust
#[test]
fn sqlite_review_reads_deterministic_historical_authority() {
    let (root, paths) = crate::brain::storage::test_support::deterministic_historical_fixture(
        "review-deterministic",
    );
    let decisions = sqlite_decisions(&paths).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].decision_id.as_deref(), Some("review-deterministic"));
    drop(root);
}

#[test]
fn sqlite_review_reports_historical_invariant_as_corrupt() {
    let error = sqlite_review_error(StorageError::InvalidStorage(
        "historical permission decision anchor is invalid",
    ));
    assert_eq!(error, "SQLite decision storage unavailable (corrupt)");
}
```

In `src/runtime/brain.rs` tests, use the same test-only fixture and assert the
actual loader succeeds:

```rust
#[test]
fn sqlite_refresh_reads_deterministic_historical_authority() {
    let (root, _paths) = crate::brain::storage::test_support::deterministic_historical_fixture(
        "runtime-deterministic",
    );
    let refresh = LiveBrainSource::refresh_from_sqlite_store(
        root.path(),
        SnapshotLimits::default(),
    )
    .unwrap();
    assert_eq!(refresh.snapshot.recent.len(), 1);
    drop(root);
}
```

- [ ] **Step 4: Run the four focused regressions and verify the current failure**

Run:

```bash
cargo test --test storage_migration legacy_deterministic_proposals_are_closed_and_readable -- --exact --nocapture
cargo test --test sqlite_storage production_deterministic_historical_authority_is_readable -- --exact --nocapture
cargo test brain::review::tests::sqlite_review_reads_deterministic_historical_authority -- --exact --nocapture
cargo test runtime::brain::tests::sqlite_refresh_reads_deterministic_historical_authority -- --exact --nocapture
```

Expected before implementation: each read that reaches `validated_historical_authority` fails with `InvalidStorage("historical permission decision anchor is invalid")`. If any test fails earlier, correct the fixture rather than changing production code.

- [ ] **Step 5: Add the canonical historical-source type**

In `src/brain/storage/decisions.rs`, add beside `DecisionKind`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CanonicalHistoricalDecisionSource {
    Model,
    DeterministicSafety,
    NativeProvider,
}

impl CanonicalHistoricalDecisionSource {
    pub(super) fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "model" => Ok(Self::Model),
            "deterministic_safety" => Ok(Self::DeterministicSafety),
            "native_provider" => Ok(Self::NativeProvider),
            _ => Err(StorageError::InvalidStorage(
                "historical permission decision source is invalid",
            )),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::DeterministicSafety => "deterministic_safety",
            Self::NativeProvider => "native_provider",
        }
    }
}
```

Do not change `DecisionIdentity::Permission.decision_source: String` or live decision construction.

- [ ] **Step 6: Share the migration-only legacy conversion**

Import `CanonicalHistoricalDecisionSource` in `src/brain/storage/migration.rs` and add:

```rust
fn legacy_proposal_source(
    value: &str,
) -> Result<CanonicalHistoricalDecisionSource, StorageError> {
    match value {
        "model" | "brain" => Ok(CanonicalHistoricalDecisionSource::Model),
        "deterministic" => Ok(CanonicalHistoricalDecisionSource::DeterministicSafety),
        "provider_policy" => Ok(CanonicalHistoricalDecisionSource::NativeProvider),
        _ => Err(StorageError::InvalidStorage(
            "legacy proposal decision source is unsupported",
        )),
    }
}
```

Replace both source matches in `ReplayAccounting::exact_hook_decision` and `MigrationImport::import_hook_decision` with:

```rust
let source = legacy_proposal_source(&record.brain_source)?;
```

Pass `source.as_str()` to `DecisionIdentity::permission` in both locations. Leave terminal correlation and accounting checks unchanged.

- [ ] **Step 7: Validate historical authority with its stored source**

In `src/brain/storage/permissions.rs`, import `CanonicalHistoricalDecisionSource`. After loading the decision anchor, parse its required source:

```rust
let decision_source = decision
    .7
    .as_deref()
    .ok_or(StorageError::InvalidStorage(
        "historical permission decision identity is incomplete",
    ))
    .and_then(CanonicalHistoricalDecisionSource::parse)?;
```

Remove `decision.7.as_deref() != Some("model")` from the invalid-anchor condition. Reconstruct the identity with `decision_source.as_str()` instead of `"model"`. Do not weaken any other tuple, provenance, cursor, or activity validation.

- [ ] **Step 8: Run focused and source-boundary tests**

Run:

```bash
cargo test --test storage_migration legacy_deterministic -- --nocapture
cargo test --features fault-injection --test storage_migration legacy_deterministic_verified_restart_uses_shared_source_policy -- --exact --nocapture
cargo test --test sqlite_storage historical_permission -- --nocapture
cargo test --test sqlite_storage canonical_historical -- --nocapture
cargo test --test sqlite_storage production_deterministic -- --nocapture
cargo test brain::review::tests::sqlite_review -- --nocapture
cargo test runtime::brain::tests::sqlite_refresh_reads_deterministic_historical_authority -- --exact --nocapture
```

Expected: all selected tests pass; unknown source and every inconsistent authority/activity mutation still return `InvalidStorage`.

- [ ] **Step 9: Inspect Task 1 scope**

Run:

```bash
git diff --check
git diff -- src/brain/storage/decisions.rs src/brain/storage/migration.rs src/brain/storage/permissions.rs tests/storage_migration.rs tests/sqlite_storage.rs src/brain/review.rs src/runtime/brain.rs
```

Expected: only the canonical source policy, its two migration consumers, source-preserving reader validation, and focused regressions differ. Do not commit.

### Task 2: Typed Storage Fault Propagation and TUI Retention

**Files:**

- Modify: `crates/coding-brain-core/src/runtime.rs:730-760`
- Modify: `src/runtime/brain.rs:20-32`
- Modify: `src/runtime/brain.rs:237-247`
- Modify: `src/runtime/brain.rs:610-626`
- Modify: `src/runtime/brain.rs:1384-1390`
- Modify: `src/runtime/brain.rs:1455-1480`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:265-300`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:2435-2540`

**Interfaces:**

- Produces: `pub enum BrainStorageFaultCategory { Full, Io, Corrupt, Other }` with `as_str(self) -> &'static str`.
- Extends: `BrainSourceError` with `StorageUnavailable(BrainStorageFaultCategory)` while retaining `Busy` and `Other(String)`.
- Consumes: binary `StorageFaultCategory`, existing `sqlite_storage_source_error`, `sqlite_runtime_action_error`, `sqlite_review_mutation_error`, and TUI `BrainApp::refresh` status precedence.

**Acceptance Criteria:**

- `InvalidStorage` reaches the TUI as typed Corrupt; full and I/O retain their exact types; migration-required and other non-busy conditions map to Other.
- Busy has one representation and preserves retry behavior.
- A warm corrupt refresh retains the complete previous refresh and selected row; a cold corrupt refresh shows an empty view and corrupt status.
- Completed action and recovery statuses still outrank storage diagnostics; generic source errors remain bounded and redacted.
- Every compiler-visible `BrainSourceError` match handles the new variant intentionally.

- [ ] **Step 1: Add failing core and binary category tests**

In `crates/coding-brain-core/src/runtime.rs` tests, add:

```rust
#[test]
fn brain_storage_fault_categories_have_stable_labels() {
    assert_eq!(BrainStorageFaultCategory::Full.as_str(), "full");
    assert_eq!(BrainStorageFaultCategory::Io.as_str(), "io");
    assert_eq!(BrainStorageFaultCategory::Corrupt.as_str(), "corrupt");
    assert_eq!(BrainStorageFaultCategory::Other.as_str(), "other");
}
```

Replace `sqlite_storage_runtime_error_preserves_last_view_with_fixed_category` in `src/runtime/brain.rs` with a table-driven test:

```rust
#[test]
fn sqlite_storage_runtime_error_preserves_typed_category() {
    for (error, expected) in [
        (
            StorageError::StorageFault {
                operation: crate::brain::storage::StorageOperation::Read,
                category: crate::brain::storage::StorageFaultCategory::Full,
            },
            BrainStorageFaultCategory::Full,
        ),
        (StorageError::Io(std::io::Error::other("/private/operator/path token-secret")), BrainStorageFaultCategory::Io),
        (StorageError::InvalidStorage("historical permission decision anchor is invalid"), BrainStorageFaultCategory::Corrupt),
        (StorageError::MigrationRequired, BrainStorageFaultCategory::Other),
    ] {
        assert_eq!(
            sqlite_storage_source_error(&error),
            BrainSourceError::StorageUnavailable(expected)
        );
    }

    assert_eq!(
        sqlite_storage_source_error(&StorageError::StorageFault {
            operation: crate::brain::storage::StorageOperation::Read,
            category: crate::brain::storage::StorageFaultCategory::Busy,
        }),
        BrainSourceError::Busy
    );
}
```

Expected before implementation: compile failure because both new types/variants are absent.

- [ ] **Step 2: Add failing TUI cold/warm retention tests**

Import `BrainStorageFaultCategory` in `crates/coding-brain-tui/src/brain_app.rs` tests and add:

```rust
#[test]
fn cold_start_corruption_reports_category_without_data() {
    let app = scripted_app([Err(BrainSourceError::StorageUnavailable(
        BrainStorageFaultCategory::Corrupt,
    ))]);
    assert_eq!(
        app.status(),
        Some("Brain: SQLite storage unavailable (corrupt); keeping the last coherent view")
    );
    assert!(app.snapshot().recent.is_empty());
    assert!(app.review_queue().is_empty());
}

#[test]
fn cold_start_missing_storage_remains_other_not_corrupt() {
    let app = scripted_app([Err(BrainSourceError::StorageUnavailable(
        BrainStorageFaultCategory::Other,
    ))]);
    assert_eq!(
        app.status(),
        Some("Brain: SQLite storage unavailable (other); keeping the last coherent view")
    );
    assert!(app.snapshot().recent.is_empty());
}

#[test]
fn corrupt_refresh_retains_complete_coherent_view_and_selection() {
    let mut app = scripted_app([
        Ok(refresh_fixture("old", 1, 1)),
        Err(BrainSourceError::StorageUnavailable(
            BrainStorageFaultCategory::Corrupt,
        )),
    ]);
    app.handle_key(key(KeyCode::Char('j')));
    let selected = app.selected_display_id(ReviewSurface::Attention).map(str::to_owned);

    app.refresh();

    assert_refresh_fixture(&app, "old", 1, 1);
    assert_eq!(
        app.selected_display_id(ReviewSurface::Attention),
        selected.as_deref()
    );
    assert_eq!(
        app.status(),
        Some("Brain: SQLite storage unavailable (corrupt); keeping the last coherent view")
    );
}
```

Add these typed-storage status-precedence tests. Keep the existing generic
source redaction test unchanged:

```rust
#[test]
fn completed_action_outranks_corrupt_refresh() {
    let mut app = scripted_app([
        Ok(refresh_fixture("old", 1, 1)),
        Err(BrainSourceError::StorageUnavailable(
            BrainStorageFaultCategory::Corrupt,
        )),
    ]);
    app.status = Some("Sent allow".into());
    app.refresh();
    assert_refresh_fixture(&app, "old", 1, 1);
    assert_eq!(app.status(), Some("Sent allow"));
}

#[test]
fn recovery_warning_outranks_corrupt_refresh() {
    let source = Arc::new(ScriptedBrainSource {
        refreshes: std::sync::Mutex::new(
            [Err(BrainSourceError::StorageUnavailable(
                BrainStorageFaultCategory::Corrupt,
            ))]
            .into_iter()
            .collect(),
        ),
    });
    let runtime = BrainRuntime::new(source, Arc::new(RecoveryWarningActions));
    let app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
    assert_eq!(app.status(), Some("Recovered interrupted activity"));
}
```

- [ ] **Step 3: Add the core fault type**

In `crates/coding-brain-core/src/runtime.rs`, add:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainStorageFaultCategory {
    Full,
    Io,
    Corrupt,
    Other,
}

impl BrainStorageFaultCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Io => "io",
            Self::Corrupt => "corrupt",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainSourceError {
    Busy,
    StorageUnavailable(BrainStorageFaultCategory),
    Other(String),
}
```

Extend `Display` so `StorageUnavailable(category)` renders `SQLite storage unavailable (<label>)`. Do not put Busy in `BrainStorageFaultCategory`.

- [ ] **Step 4: Map binary storage faults without strings**

Import `BrainStorageFaultCategory` in `src/runtime/brain.rs`. Replace `sqlite_storage_source_error` with:

```rust
fn sqlite_storage_source_error(error: &StorageError) -> BrainSourceError {
    if error.fault_category() == crate::brain::storage::StorageFaultCategory::Busy {
        return BrainSourceError::Busy;
    }
    let category = match error.fault_category() {
        crate::brain::storage::StorageFaultCategory::Full => BrainStorageFaultCategory::Full,
        crate::brain::storage::StorageFaultCategory::Io => BrainStorageFaultCategory::Io,
        crate::brain::storage::StorageFaultCategory::Corrupt => BrainStorageFaultCategory::Corrupt,
        crate::brain::storage::StorageFaultCategory::Busy => unreachable!("handled above"),
        crate::brain::storage::StorageFaultCategory::Other => BrainStorageFaultCategory::Other,
    };
    BrainSourceError::StorageUnavailable(category)
}

fn sqlite_storage_unavailable_message(category: BrainStorageFaultCategory) -> String {
    format!(
        "SQLite storage unavailable ({}); keeping the last coherent view",
        category.as_str()
    )
}
```

Handle the new variant in `sqlite_review_mutation_error` and `sqlite_runtime_action_error` by calling `sqlite_storage_unavailable_message`. Busy retains its existing action-specific messages; `Other(String)` remains unchanged.

- [ ] **Step 5: Render typed storage failures in the TUI**

In `BrainApp::refresh`, add an exhaustive arm before generic `Other`:

```rust
Err(BrainSourceError::StorageUnavailable(category)) => {
    source_error = Some(format!(
        "Brain: SQLite storage unavailable ({}); keeping the last coherent view",
        category.as_str()
    ));
}
```

Do not assign snapshot, review queue, scorecard, review state, selection, or successful-refresh flags in any error arm. Preserve the existing pending-action, source-error, recovery-warning, and busy-status precedence.

- [ ] **Step 6: Audit every `BrainSourceError` consumer**

Run:

```bash
rg -n -C 3 'BrainSourceError::' crates/coding-brain-core/src/runtime.rs crates/coding-brain-tui/src/brain_app.rs src/runtime/brain.rs
```

Update only exhaustive matches. Constructors that intentionally produce generic validation errors stay `Other(String)`. Scripted test sources need no change unless their test specifically models storage.

- [ ] **Step 7: Run focused fault and TUI tests**

Run:

```bash
cargo test -p coding-brain-core runtime::tests::brain_storage_fault_categories_have_stable_labels -- --exact --nocapture
cargo test runtime::brain::tests::sqlite_storage_runtime_error_preserves_typed_category -- --exact --nocapture
cargo test -p coding-brain-tui brain_app::tests::cold_start_corruption_reports_category_without_data -- --exact --nocapture
cargo test -p coding-brain-tui brain_app::tests::cold_start_missing_storage_remains_other_not_corrupt -- --exact --nocapture
cargo test -p coding-brain-tui brain_app::tests::corrupt_refresh_retains_complete_coherent_view_and_selection -- --exact --nocapture
cargo test -p coding-brain-tui brain_app::tests::refresh_source_error_is_redacted_and_bounded -- --exact --nocapture
```

Expected: all pass; Busy tests continue to report retrying/stale behavior and generic-source redaction remains bounded.

- [ ] **Step 8: Run focused feature verification**

Run:

```bash
cargo fmt
cargo fmt --check
cargo test --test storage_migration legacy_deterministic -- --nocapture
cargo test brain::storage::migration::tests::legacy_proposal_sources_map_to_the_canonical_domain -- --exact --nocapture
cargo test --test storage_migration legacy_brain -- --nocapture
cargo test --test sqlite_storage historical_permission -- --nocapture
cargo test --test sqlite_storage canonical_historical -- --nocapture
cargo test --test sqlite_storage production_deterministic -- --nocapture
cargo test brain::review::tests::sqlite_review -- --nocapture
cargo test runtime::brain::tests::sqlite_refresh_reads_deterministic_historical_authority -- --exact --nocapture
cargo test -p coding-brain-tui corrupt -- --nocapture
```

Expected: every command exits 0. `cargo fmt` may change only task-owned Rust files.

- [ ] **Step 9: Run workspace quality gates**

Create isolated state directories before the first state-dependent command;
do not use the operator's live HOME/XDG state as a fallback:

```bash
CBRAIN_TEST_ROOT="$(mktemp -d)"
mkdir -p "$CBRAIN_TEST_ROOT/home" "$CBRAIN_TEST_ROOT/config" "$CBRAIN_TEST_ROOT/state"
env HOME="$CBRAIN_TEST_ROOT/home" XDG_CONFIG_HOME="$CBRAIN_TEST_ROOT/config" XDG_STATE_HOME="$CBRAIN_TEST_ROOT/state" CARGO_HOME=/home/alexander/.cargo nix develop path:. --command cargo test
env HOME="$CBRAIN_TEST_ROOT/home" XDG_CONFIG_HOME="$CBRAIN_TEST_ROOT/config" XDG_STATE_HOME="$CBRAIN_TEST_ROOT/state" CARGO_HOME=/home/alexander/.cargo nix develop path:. --command cargo build
env HOME="$CBRAIN_TEST_ROOT/home" XDG_CONFIG_HOME="$CBRAIN_TEST_ROOT/config" XDG_STATE_HOME="$CBRAIN_TEST_ROOT/state" CARGO_HOME=/home/alexander/.cargo nix develop path:. --command cargo clippy -- -D warnings
```

Expected: all workspace tests pass, build succeeds, and Clippy emits no warnings.

- [ ] **Step 10: Verify surgical scope and hand off**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff -- src/brain/storage/decisions.rs src/brain/storage/migration.rs src/brain/storage/permissions.rs tests/storage_migration.rs tests/sqlite_storage.rs src/brain/review.rs src/runtime/brain.rs crates/coding-brain-core/src/runtime.rs crates/coding-brain-tui/src/brain_app.rs .internal/specs/2026-08-10-deterministic-historical-authority-reader-compatibility-design.md .internal/plans/2026-08-10-deterministic-historical-authority-reader-compatibility.md
```

Expected: every changed line traces to `codexctl-dzlb9.16`; live state and `db.failed-fresh-20260810-2058` are untouched. Report changed files, fresh command results, Beads task status, and that commit/push/close still await authorization.

---

## Stress Test Results

Plan stress test completed and approved branch-by-branch on 2026-08-10:

1. **Task boundaries:** Keep source compatibility end-to-end in Task 1 and typed fault propagation/TUI retention in Task 2.
2. **TDD sequence:** Add behavior-free fixture scaffolding first when needed, then preserve failing consumer tests before production changes.
3. **Fixture fidelity and privacy:** Retain the exact production decision ID and cursor only where behavior depends on them; redact all nonessential content and assert null tool-use identity and no live delivery.
4. **Migration aliases:** Test every supported alias and the unknown-source failure independently from the deterministic production regression.
5. **Busy normalization:** Preserve one top-level Busy representation even when Busy arrives through a categorized storage error.
6. **Security isolation:** Prove readable historical allow/deny evidence remains response-ineligible and cannot create or satisfy live authority.
7. **Environment isolation:** Use isolated HOME/XDG state from the first state-dependent test and never exercise the operator's live database.
8. **Scale:** Do not add a benchmark for a bounded per-row parser with unchanged queries; rely on focused, pagination, and full-workspace gates unless implementation changes boundedness.
9. **Failure behavior:** Cold faults show an empty projection; warm faults preserve the complete coherent projection and selection; status precedence and Busy semantics remain explicit.
10. **Execution controls:** Create implementation Beads from this final plan, require an explicit execution-workflow choice, and make no live-state, commit, push, sync, PR, or parent-closure mutation without separate authorization.
