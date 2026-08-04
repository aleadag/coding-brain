use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use coding_brain::brain::storage::{
    BRAIN_APPLICATION_ID, BRAIN_SCHEMA_VERSION, BrainDb, OpenRole, REVIEW_APPLICATION_ID,
    REVIEW_SCHEMA_VERSION, ReviewDb, StorageDeadline, StorageError, StoragePaths,
};
use coding_brain_core::lifecycle::{
    LifecycleEvent, LifecycleEventKind, LifecycleIdentity, LifecycleSnapshot, PermissionAction,
    PermissionAuthority,
};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, params};

const BRAIN_SCHEMA_V1: &str = include_str!("fixtures/storage/schema-v1/brain.sql");
const REVIEW_SCHEMA_V1: &str = include_str!("fixtures/storage/schema-v1/review.sql");

fn open_for_constraints(path: &std::path::Path) -> Connection {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .unwrap();
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    connection
}

fn private_tempdir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

#[test]
fn lifecycle_round_trip_preserves_topology_without_permission_authority() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let codex_parent = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "codex-parent".into(),
        Some("codex-turn".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();
    let codex_child = LifecycleIdentity::try_new_with_provider_session(
        AgentProvider::Codex,
        "codex-child".into(),
        Some("codex-parent".into()),
        Some("codex-turn".into()),
        Some("/tmp/child-rollout.jsonl".into()),
        "/work/project".into(),
    )
    .unwrap();
    let claude = LifecycleIdentity::try_new(
        AgentProvider::Claude,
        "claude-session".into(),
        Some("claude-turn".into()),
        None,
        "/work/claude".into(),
    )
    .unwrap();
    let antigravity_invocation = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-session".into(),
        Some("invocation-7".into()),
        None,
        "/work/agy".into(),
    )
    .unwrap();
    let antigravity_step = |step: &str| {
        LifecycleIdentity::try_new(
            AgentProvider::Antigravity,
            "agy-session".into(),
            Some(step.into()),
            None,
            "/work/agy".into(),
        )
        .unwrap()
    };
    let events = vec![
        LifecycleEvent::from_parts(
            codex_parent,
            LifecycleEventKind::SubagentStart {
                agent_id: "codex-child".into(),
            },
        )
        .unwrap(),
        LifecycleEvent::from_parts(codex_child, LifecycleEventKind::PreToolUse).unwrap(),
        LifecycleEvent::from_parts(
            claude,
            LifecycleEventKind::SubagentStart {
                agent_id: "claude-session".into(),
            },
        )
        .unwrap(),
        LifecycleEvent::from_parts(
            antigravity_step("setup-turn"),
            LifecycleEventKind::SubagentStart {
                agent_id: "agy-session".into(),
            },
        )
        .unwrap(),
        LifecycleEvent::from_parts_with_turn_initial_step(
            antigravity_invocation,
            LifecycleEventKind::UserPromptSubmit,
            Some(5),
        )
        .unwrap(),
        LifecycleEvent::from_parts(antigravity_step("step-5"), LifecycleEventKind::PreToolUse)
            .unwrap(),
        LifecycleEvent::from_parts(antigravity_step("step-5"), LifecycleEventKind::PostToolUse)
            .unwrap(),
        LifecycleEvent::permission_with_authority(
            antigravity_step("step-6"),
            "a".repeat(64),
            PermissionAuthority {
                transaction_id: "transaction-secret".into(),
                action: PermissionAction::Allow,
            },
        )
        .unwrap(),
    ];
    let mut expected = LifecycleSnapshot::default();
    for (offset, event) in events.into_iter().enumerate() {
        let received_at_ms = 100 + offset as u64;
        expected.record_at(event.clone(), received_at_ms);
        db.record_lifecycle(event, received_at_ms).unwrap();
    }
    expected.remove_permission_state();
    let snapshot = db.read_lifecycle().unwrap();

    assert_eq!(snapshot, expected);
    let agy = &snapshot.sessions
        [&AgentSessionKey::native(AgentProvider::Antigravity, "agy-session").storage_key()];
    assert_eq!(
        agy.latest_event,
        Some(coding_brain_core::lifecycle::LifecycleEventName::PermissionRequest)
    );
    assert!(agy.status_event.is_some());
    assert!(agy.permission_request_events.is_empty());
    assert!(agy.permission_authorities.is_empty());
    assert!(agy.antigravity_permission_requests.is_empty());
    assert!(!format!("{snapshot:?}").contains("transaction-secret"));
}

#[test]
fn lifecycle_stop_round_trip_preserves_closed_current_continuity() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "session-1".into(),
        Some("turn-1".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts(identity.clone(), LifecycleEventKind::UserPromptSubmit).unwrap(),
        100,
    )
    .unwrap();

    db.record_lifecycle(
        LifecycleEvent::from_parts(identity, LifecycleEventKind::Stop).unwrap(),
        101,
    )
    .unwrap();
    let snapshot = db.read_lifecycle().unwrap();
    let state = snapshot.sessions.values().next().unwrap();

    assert_eq!(state.current_turn.as_deref(), Some("turn-1"));
    assert!(!state.turn_open);
    assert_eq!(
        state.recent_turns.front().map(String::as_str),
        Some("turn-1")
    );
}

#[test]
fn lifecycle_round_trip_inserts_parent_before_lexically_earlier_child() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let parent = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "z-parent".into(),
        Some("turn-1".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts(
            parent,
            LifecycleEventKind::SubagentStart {
                agent_id: "a-child".into(),
            },
        )
        .unwrap(),
        100,
    )
    .unwrap();
    let child = LifecycleIdentity::try_new_with_provider_session(
        AgentProvider::Codex,
        "a-child".into(),
        Some("z-parent".into()),
        Some("turn-1".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();

    db.record_lifecycle(
        LifecycleEvent::from_parts(child, LifecycleEventKind::PreToolUse).unwrap(),
        101,
    )
    .unwrap();

    assert_eq!(db.read_lifecycle().unwrap().sessions.len(), 2);
}

#[test]
fn lifecycle_load_rejects_mismatched_active_invocation() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-1".into(),
        Some("invocation-1".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts_with_turn_initial_step(
            identity,
            LifecycleEventKind::UserPromptSubmit,
            Some(1),
        )
        .unwrap(),
        100,
    )
    .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE lifecycle_invocations SET invocation_id = 'invocation-wrong'",
            [],
        )
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.read_lifecycle(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn lifecycle_load_rejects_invocation_sequence_at_next_sequence() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-1".into(),
        Some("invocation-1".into()),
        None,
        "/work/project".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts_with_turn_initial_step(
            identity,
            LifecycleEventKind::UserPromptSubmit,
            Some(1),
        )
        .unwrap(),
        100,
    )
    .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute("UPDATE lifecycle_invocations SET state_sequence = 2", [])
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.read_lifecycle(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn lifecycle_load_rejects_cross_row_sequence_lease_topology_and_cardinality_corruption() {
    for mutation in [
        "UPDATE lifecycle_meta SET next_sequence = 1",
        "UPDATE lifecycle_meta SET next_sequence = 3;
         UPDATE lifecycle_leases SET status_sequence = 2",
        "PRAGMA ignore_check_constraints = ON;
         UPDATE lifecycle_sessions SET signature_detail_id = 'smuggled-detail'",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, provider_session_id, latest_event,
            latest_sequence, latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES (
            'codex', 'unowned-child', X'2F', 'session-1', 'pre_tool_use',
            1, 1, 'pre_tool_use', 'turn-1'
         )",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, provider_session_id, latest_event,
            latest_sequence, latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES (
            'codex', 'mismatched-child', X'2F', 'session-1', 'pre_tool_use',
            1, 1, 'pre_tool_use', 'child-turn'
         );
         INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open
         ) VALUES ('codex', 'mismatched-child', 'current', 'child-turn', 1);
         INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'session-1', 'mismatched-child', 'owner-turn',
                   'active', 0, 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES (
            'codex', 'unrelated-parent', X'2F', 'pre_tool_use', 1, 1,
            'pre_tool_use', 'turn-1'
         );
         INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, provider_session_id, latest_event,
            latest_sequence, latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES (
            'codex', 'wrong-tree-child', X'2F', 'unrelated-parent', 'pre_tool_use',
            1, 1, 'pre_tool_use', 'owner-turn'
         );
         INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open
         ) VALUES ('codex', 'wrong-tree-child', 'current', 'owner-turn', 1);
         INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'session-1', 'wrong-tree-child', 'owner-turn',
                   'active', 0, 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES (
            'codex', 'session-2', X'2F', 'pre_tool_use', 1, 1,
            'pre_tool_use', 'turn-2'
         );
         INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES
            ('codex', 'session-1', 'session-2', 'turn-1', 'active', 0, 1, 1),
            ('codex', 'session-2', 'session-1', 'turn-2', 'active', 0, 1, 1)",
        "PRAGMA ignore_check_constraints = ON;
         WITH RECURSIVE slots(value) AS (
            VALUES(0) UNION ALL SELECT value + 1 FROM slots WHERE value < 64
         )
         INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         )
         SELECT 'codex', 'session-1', printf('child-%d', value), 'turn-1',
                'active', value, 1, 1 FROM slots",
    ] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "session-1".into(),
            Some("turn-1".into()),
            None,
            "/work/project".into(),
        )
        .unwrap();
        db.record_lifecycle(
            LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap(),
            1,
        )
        .unwrap();
        drop(db);
        let connection = open_for_constraints(&paths.brain_db());
        connection.execute_batch(mutation).unwrap();
        drop(connection);
        let db = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_millis(250)),
        )
        .unwrap();

        assert!(
            matches!(db.read_lifecycle(), Err(StorageError::InvalidStorage(_))),
            "mutation unexpectedly loaded: {mutation}"
        );
    }
}

fn insert_attempt(connection: &Connection, attempt_id: &str, request_key: &str) {
    connection
        .execute(
            "INSERT INTO permission_attempts (
                attempt_id, provider, session_id, turn_id, tool_use_id, request_key,
                authority_action, attempt_state, created_at_ms, updated_at_ms
             ) VALUES (?1, 'codex', 'session-1', 'turn-1', 'tool-1', ?2,
                       'allow', 'decided', 1, 1)",
            params![attempt_id, request_key],
        )
        .unwrap();
}

fn insert_decision(connection: &Connection, decision_id: &str) {
    connection
        .execute(
            "INSERT INTO decision_identities (
                decision_id, provider, session_id, turn_id, tool_use_id,
                authority_action, decision_source, decided_at_ms
             ) VALUES (?1, 'codex', 'session-1', 'turn-1', 'tool-1',
                       'allow', 'model', 1)",
            [decision_id],
        )
        .unwrap();
}

fn insert_terminal_event(connection: &Connection, cursor: i64, activity_id: &str) {
    connection
        .execute(
            "INSERT INTO activity_events (
                source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                terminal_provider, terminal_session_id, terminal_turn_id,
                terminal_tool_use_id, terminal_action, event_payload
             ) VALUES (?1, ?2, 'decision', 'allowed', 1,
                       'codex', 'session-1', 'turn-1', 'tool-1', 'allow', X'')",
            params![cursor, activity_id],
        )
        .unwrap();
}

fn insert_commit(
    connection: &Connection,
    attempt_id: &str,
    decision_id: &str,
    activity_id: &str,
    evidence_kind: &str,
    delivery_state: &str,
    response_eligible: i64,
) -> rusqlite::Result<usize> {
    connection.execute(
        "INSERT INTO permission_commits (
            attempt_id, decision_id, terminal_activity_id, provider, session_id,
            turn_id, tool_use_id, authority_action, evidence_kind, delivery_state,
            response_eligible, committed_at_ms
         ) VALUES (?1, ?2, ?3, 'codex', 'session-1',
                   'turn-1', 'tool-1', 'allow', ?4, ?5, ?6, 1)",
        params![
            attempt_id,
            decision_id,
            activity_id,
            evidence_kind,
            delivery_state,
            response_eligible
        ],
    )
}

fn assert_statement_rejected(connection: &Connection, sql: &str) {
    assert!(
        connection.execute_batch(sql).is_err(),
        "statement unexpectedly accepted: {sql}"
    );
}

fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({table})");
    connection
        .prepare(&sql)
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn insert_lifecycle_session(
    connection: &Connection,
    provider: &str,
    session_id: &str,
    provider_session_id: Option<&str>,
) {
    connection
        .execute(
            "INSERT INTO lifecycle_sessions (
                provider, session_id, cwd, provider_session_id,
                latest_event, latest_sequence, latest_received_at_ms,
                session_start_source, signature_event, signature_session_start_source
             ) VALUES (?1, ?2, X'2F', ?3, 'session_start', 1, 1,
                       'startup', 'session_start', 'startup')",
            params![provider, session_id, provider_session_id],
        )
        .unwrap();
}

fn assert_current_brain_open_is_invalid(paths: &StoragePaths) {
    let error = BrainDb::open_current(
        paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap_err();
    assert!(
        matches!(error, StorageError::InvalidStorage(_)),
        "{error:?}"
    );
}

fn assert_current_review_open_is_invalid(paths: &StoragePaths) {
    let error = ReviewDb::open_current(
        paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap_err();
    assert!(
        matches!(error, StorageError::InvalidStorage(_)),
        "{error:?}"
    );
}

#[test]
fn hook_open_never_creates_or_migrates() {
    let root = private_tempdir();
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
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());

    let db = BrainDb::create_current(&paths).unwrap();

    assert_eq!(db.application_id().unwrap(), BRAIN_APPLICATION_ID);
    assert_eq!(db.user_version().unwrap(), BRAIN_SCHEMA_VERSION);
    assert_eq!(db.pragma_string("journal_mode").unwrap(), "wal");
    assert_eq!(db.pragma_i64("synchronous").unwrap(), 2);
    assert_eq!(db.pragma_i64("foreign_keys").unwrap(), 1);
    assert_eq!(db.pragma_i64("trusted_schema").unwrap(), 0);
    assert_eq!(db.pragma_i64("secure_delete").unwrap(), 1);
    assert!(db.defensive_mode().unwrap());
    assert_eq!(db.limit(Limit::SQLITE_LIMIT_ATTACHED).unwrap(), 0);
    assert_eq!(db.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(), 1024 * 1024);
    assert_eq!(
        db.limit(Limit::SQLITE_LIMIT_SQL_LENGTH).unwrap(),
        1024 * 1024
    );
    assert_eq!(db.limit(Limit::SQLITE_LIMIT_COLUMN).unwrap(), 128);
}

#[test]
fn storage_paths_use_dedicated_database_directory() {
    let paths = StoragePaths::at(std::path::Path::new("/state/coding-brain"));

    assert_eq!(
        paths.brain_db(),
        std::path::Path::new("/state/coding-brain/db/brain.sqlite3")
    );
    assert_eq!(
        paths.review_db(),
        std::path::Path::new("/state/coding-brain/db/review.sqlite3")
    );
}

#[test]
fn frozen_schema_fixture_is_the_executed_schema() {
    assert_eq!(BrainDb::schema_sql(), BRAIN_SCHEMA_V1);
    assert_eq!(ReviewDb::schema_sql(), REVIEW_SCHEMA_V1);
}

#[test]
fn database_directory_and_files_are_owner_only() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());

    let brain = BrainDb::create_current(&paths).unwrap();
    let review = ReviewDb::create_current(&paths).unwrap();

    assert_eq!(
        fs::metadata(paths.db_dir()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(paths.brain_db()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(paths.review_db())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop((brain, review));
}

#[test]
fn unsafe_ancestor_and_database_entries_are_rejected() {
    let root = private_tempdir();
    let target = private_tempdir();
    let linked_root = root.path().join("linked-state");
    symlink(target.path(), &linked_root).unwrap();

    let error = BrainDb::create_current(&StoragePaths::at(&linked_root)).unwrap_err();
    assert!(matches!(error, StorageError::InvalidStorage(_)));

    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(&state);
    fs::create_dir(paths.db_dir()).unwrap();
    fs::set_permissions(paths.db_dir(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(paths.brain_db(), b"not sqlite").unwrap();
    fs::set_permissions(paths.brain_db(), fs::Permissions::from_mode(0o640)).unwrap();

    let error = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(50)),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::InvalidStorage(_)));
}

#[test]
fn unsafe_state_root_mode_is_rejected_before_database_creation() {
    let outer = private_tempdir();
    let state_root = outer.path().join("state");
    fs::create_dir(&state_root).unwrap();
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o770)).unwrap();
    let paths = StoragePaths::at(&state_root);

    let error = BrainDb::create_current(&paths).unwrap_err();

    assert!(matches!(error, StorageError::InvalidStorage(_)));
    assert!(!paths.db_dir().exists());
}

#[test]
fn replaceable_non_sticky_ancestor_is_rejected_before_database_creation() {
    let outer = private_tempdir();
    let ancestor = outer.path().join("replaceable");
    let state_root = ancestor.join("state");
    fs::create_dir(&ancestor).unwrap();
    fs::create_dir(&state_root).unwrap();
    fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(&state_root);

    let error = BrainDb::create_current(&paths).unwrap_err();

    assert!(matches!(error, StorageError::InvalidStorage(_)));
    assert!(!paths.db_dir().exists());
}

#[test]
fn root_owned_sticky_temp_ancestor_remains_supported() {
    let root = tempfile::Builder::new()
        .prefix("coding-brain-sqlite-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(root.path());

    drop(BrainDb::create_current(&paths).unwrap());

    assert!(paths.brain_db().is_file());
}

#[test]
fn preexisting_sidecars_are_rejected_before_database_creation() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    fs::create_dir(paths.db_dir()).unwrap();
    fs::set_permissions(paths.db_dir(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(paths.brain_db().with_extension("sqlite3-wal"), b"untrusted").unwrap();

    let error = BrainDb::create_current(&paths).unwrap_err();

    assert!(matches!(error, StorageError::InvalidStorage(_)));
    assert!(!paths.brain_db().exists());
}

#[test]
fn hook_open_rejects_incomplete_and_unsupported_schema_without_repair() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE schema_meta SET migration_state = 'in_progress' WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);

    let error = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(50)),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::MigrationRequired));

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute_batch("PRAGMA user_version = 2;")
        .unwrap();
    drop(connection);
    let error = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(50)),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::UnsupportedSchema { .. }));
}

#[test]
fn current_brain_open_rejects_missing_or_forged_frozen_schema_objects() {
    for mutation in [
        "DROP TABLE decision_payloads;",
        "DROP INDEX permission_commits_undelivered_audit;
         CREATE INDEX permission_commits_undelivered_audit
         ON permission_commits (attempt_id);",
    ] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let connection = open_for_constraints(&paths.brain_db());
        connection.execute_batch(mutation).unwrap();
        drop(connection);

        assert_current_brain_open_is_invalid(&paths);
    }
}

#[test]
fn current_review_open_rejects_missing_or_forged_frozen_schema_objects() {
    for mutation in [
        "DROP TABLE review_marks;",
        "DROP INDEX review_marks_surface_cursor;
         CREATE INDEX review_marks_surface_cursor ON review_marks (group_id);",
    ] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(ReviewDb::create_current(&paths).unwrap());
        let connection = open_for_constraints(&paths.review_db());
        connection.execute_batch(mutation).unwrap();
        drop(connection);

        assert_current_review_open_is_invalid(&paths);
    }
}

#[test]
fn current_open_fails_closed_when_sqlite_schema_is_corrupt() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    let schema_version = connection
        .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    connection
        .execute_batch("PRAGMA writable_schema = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE sqlite_schema
             SET sql = 'CREATE TABLE permission_attempts('
             WHERE name = 'permission_attempts'",
            [],
        )
        .unwrap();
    connection
        .pragma_update(None, "schema_version", schema_version + 1)
        .unwrap();
    drop(connection);

    let error = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::InvalidStorage(_) | StorageError::Sqlite(_)
        ),
        "{error:?}"
    );
}

#[test]
fn expired_deadline_fails_before_opening_storage() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());

    let error = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::ZERO),
    )
    .unwrap_err();

    assert!(matches!(error, StorageError::Busy));
}

#[test]
fn brain_schema_enforces_closed_domains_and_nonunique_request_identity() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    insert_attempt(&connection, "attempt-1", "same-request");
    insert_attempt(&connection, "attempt-2", "same-request");
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM permission_attempts WHERE request_key = 'same-request'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );

    let invalid_state = connection.execute(
        "UPDATE permission_attempts SET attempt_state = 'maybe' WHERE attempt_id = 'attempt-1'",
        [],
    );
    assert!(invalid_state.is_err());
    let invalid_cursor = connection.execute(
        "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms, event_payload
         ) VALUES (0, 'activity-0', 'diagnostic', 'observed', 1, X'')",
        [],
    );
    assert!(invalid_cursor.is_err());
    let invalid_meta = connection.execute(
        "UPDATE schema_meta SET erasure_state = 'unknown' WHERE singleton = 1",
        [],
    );
    assert!(invalid_meta.is_err());
    for rejected in [
        "UPDATE schema_meta SET migration_state = 'unknown' WHERE singleton = 1",
        "UPDATE schema_meta SET activity_high_water = -1 WHERE singleton = 1",
        "UPDATE schema_meta SET application_id = 0 WHERE singleton = 1",
        "UPDATE permission_attempts SET provider = 'unknown' WHERE attempt_id = 'attempt-1'",
        "UPDATE permission_attempts SET authority_action = 'maybe' WHERE attempt_id = 'attempt-1'",
        "INSERT INTO permission_attempts (
            attempt_id, provider, session_id, turn_id, tool_use_id, request_key,
            authority_action, attempt_state, created_at_ms, updated_at_ms
         ) VALUES ('attempt-1', 'codex', 'session-1', 'turn-1', 'tool-1',
                   'other-request', 'allow', 'decided', 1, 1)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }
}

#[test]
fn decision_and_learning_payload_constraints_are_executable() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    insert_decision(&connection, "decision-1");

    for rejected in [
        "UPDATE decision_identities SET decision_source = 'unknown'
         WHERE decision_id = 'decision-1'",
        "UPDATE decision_identities SET authority_action = 'maybe'
         WHERE decision_id = 'decision-1'",
        "INSERT INTO decision_identities (
            decision_id, provider, session_id, turn_id, tool_use_id,
            authority_action, decision_source, decided_at_ms
         ) VALUES ('decision-1', 'codex', 'session-1', 'turn-1', 'tool-1',
                   'allow', 'model', 2)",
        "INSERT INTO decision_payloads (decision_id) VALUES ('missing')",
        "INSERT INTO decision_payloads (decision_id, source_cursor)
         VALUES ('decision-1', 0)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }
    assert!(
        connection
            .execute(
                "INSERT INTO decision_payloads (decision_id, normalized_command)
                 VALUES ('decision-1', ?1)",
                ["x".repeat(4097)],
            )
            .is_err()
    );
}

#[test]
fn activity_terminal_identity_is_all_or_none_and_typed() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    for rejected in [
        "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
            terminal_provider, event_payload
         ) VALUES (1, 'partial', 'decision', 'allowed', 1, 'codex', X'')",
        "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
            terminal_provider, terminal_session_id, terminal_turn_id,
            terminal_tool_use_id, terminal_action, event_payload
         ) VALUES (1, 'bad-action', 'decision', 'allowed', 1,
                   'codex', 'session-1', 'turn-1', 'tool-1', 'maybe', X'')",
    ] {
        assert_statement_rejected(&connection, rejected);
    }
    insert_terminal_event(&connection, 1, "activity-1");
    assert_statement_rejected(
        &connection,
        "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
            event_payload
         ) VALUES (2, 'activity-1', 'diagnostic', 'observed', 1, X'')",
    );
}

#[test]
fn permission_commit_requires_matching_authority_identity_and_action() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    insert_attempt(&connection, "attempt-1", "request-1");
    connection
        .execute(
            "INSERT INTO decision_identities (
                decision_id, provider, session_id, turn_id, tool_use_id,
                authority_action, decision_source, decided_at_ms
             ) VALUES ('decision-1', 'codex', 'session-1', 'turn-1', 'tool-1',
                       'allow', 'model', 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO activity_events (
                source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                terminal_provider, terminal_session_id, terminal_turn_id,
                terminal_tool_use_id, terminal_action, event_payload
             ) VALUES (1, 'activity-1', 'decision', 'allowed', 1,
                       'codex', 'session-1', 'turn-1', 'tool-1', 'allow', X'')",
            [],
        )
        .unwrap();

    let mismatch = connection.execute(
        "INSERT INTO permission_commits (
            attempt_id, decision_id, terminal_activity_id, provider, session_id,
            turn_id, tool_use_id, authority_action, evidence_kind, delivery_state,
            response_eligible, committed_at_ms
         ) VALUES ('attempt-1', 'decision-1', 'activity-1', 'codex', 'session-1',
                   'turn-1', 'tool-1', 'deny', 'provider_authority', 'pending', 1, 1)",
        [],
    );
    assert!(mismatch.is_err());

    connection
        .execute(
            "INSERT INTO permission_commits (
                attempt_id, decision_id, terminal_activity_id, provider, session_id,
                turn_id, tool_use_id, authority_action, evidence_kind, delivery_state,
                response_eligible, committed_at_ms
             ) VALUES ('attempt-1', 'decision-1', 'activity-1', 'codex', 'session-1',
                       'turn-1', 'tool-1', 'allow', 'provider_authority', 'pending', 1, 1)",
            [],
        )
        .unwrap();
}

#[test]
fn permission_commit_enforces_unique_references_and_closed_domains() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    for (attempt, decision, cursor, activity) in [
        ("attempt-1", "decision-1", 1, "activity-1"),
        ("attempt-2", "decision-2", 2, "activity-2"),
    ] {
        insert_attempt(&connection, attempt, attempt);
        insert_decision(&connection, decision);
        insert_terminal_event(&connection, cursor, activity);
    }
    insert_commit(
        &connection,
        "attempt-1",
        "decision-1",
        "activity-1",
        "provider_authority",
        "pending",
        1,
    )
    .unwrap();

    assert!(
        insert_commit(
            &connection,
            "attempt-2",
            "decision-1",
            "activity-2",
            "provider_authority",
            "pending",
            1,
        )
        .is_err()
    );
    assert!(
        insert_commit(
            &connection,
            "attempt-2",
            "decision-2",
            "activity-1",
            "provider_authority",
            "pending",
            1,
        )
        .is_err()
    );
    assert!(
        insert_commit(
            &connection,
            "attempt-1",
            "decision-2",
            "activity-2",
            "provider_authority",
            "pending",
            1,
        )
        .is_err()
    );
    for (evidence, delivery, eligible) in [
        ("unknown", "pending", 1),
        ("provider_authority", "maybe", 1),
        ("provider_authority", "pending", 2),
    ] {
        assert!(
            insert_commit(
                &connection,
                "attempt-2",
                "decision-2",
                "activity-2",
                evidence,
                delivery,
                eligible,
            )
            .is_err()
        );
    }
    insert_commit(
        &connection,
        "attempt-2",
        "decision-2",
        "activity-2",
        "deterministic_safety",
        "delivered",
        0,
    )
    .unwrap();
}

#[test]
fn lifecycle_schema_enforces_provider_qualified_topology() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    for rejected in [
        "UPDATE lifecycle_meta SET next_sequence = 0 WHERE singleton = 1",
        "INSERT INTO lifecycle_meta (singleton, next_sequence) VALUES (2, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, provider_session_id,
            latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'orphan', X'2F', 'missing', 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open
         ) VALUES ('codex', 'missing', 'current', 'turn-1', 1)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }

    insert_lifecycle_session(&connection, "codex", "root-1", None);
    insert_lifecycle_session(&connection, "codex", "root-2", None);
    insert_lifecycle_session(&connection, "codex", "child-1", Some("root-1"));
    insert_lifecycle_session(&connection, "claude", "claude-1", None);
    insert_lifecycle_session(&connection, "antigravity", "agy-1", None);

    connection
        .execute_batch(
            "INSERT INTO lifecycle_sessions (
                provider, session_id, cwd, latest_event, latest_sequence,
                latest_received_at_ms, session_start_source
             ) VALUES (
                'codex', 'permission-fact', X'2F', 'permission_request', 2, 2, NULL
             );
             INSERT INTO lifecycle_leases (
                provider, session_id, status_event, status_sequence,
                status_received_at_ms, projected_status
             ) VALUES (
                'codex', 'permission-fact', 'permission_request', 2, 2, 'needs_input'
             );
             INSERT INTO lifecycle_turns (
                provider, session_id, continuity_state, turn_id, turn_open, recent_position
             ) VALUES
                ('codex', 'root-1', 'current', 'turn-1', 0, NULL),
                ('codex', 'root-1', 'recent', 'turn-1', 0, 0);",
        )
        .unwrap();

    for rejected in [
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('unknown', 'bad-provider', X'2F', 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', '', X'2F', 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', printf('%0513d', 0), X'2F', 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'empty-cwd', X'', 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'long-cwd', zeroblob(4097), 'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, transcript_path,
            latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'long-transcript', X'2F', zeroblob(4097),
                   'permission_request', 1, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'zero-sequence', X'2F', 'permission_request', 0, 1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence, latest_received_at_ms
         ) VALUES ('codex', 'negative-time', X'2F', 'permission_request', 1, -1)",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, ignored_reason
         ) VALUES ('codex', 'bad-ignore', X'2F', 'permission_request', 1, 1, 'unknown')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES ('codex', 'permission-signature', X'2F',
                   'permission_request', 1, 1, 'permission_request', 'turn-1')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_detail_id
         ) VALUES ('codex', 'detached-detail', X'2F', 'stop', 1, 1, 'request-key')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, session_start_source
         ) VALUES ('codex', 'permission-source', X'2F',
                   'permission_request', 1, 1, 'startup')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_event, signature_session_start_source
         ) VALUES ('codex', 'missing-start-source', X'2F',
                   'session_start', 1, 1, 'session_start', 'startup')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, signature_event, signature_turn_id
         ) VALUES ('codex', 'mismatched-signature', X'2F',
                   'pre_tool_use', 1, 1, 'post_tool_use', 'turn-1')",
        "INSERT INTO lifecycle_sessions (
            provider, session_id, cwd, latest_event, latest_sequence,
            latest_received_at_ms, session_start_source,
            signature_event, signature_session_start_source
         ) VALUES ('codex', 'mismatched-source', X'2F', 'session_start', 1, 1,
                   'startup', 'session_start', 'resume')",
        "INSERT INTO lifecycle_leases (
            provider, session_id, status_event, status_sequence,
            status_received_at_ms, projected_status
         ) VALUES ('codex', 'missing', 'stop', 1, 1, 'idle')",
        "INSERT INTO lifecycle_leases (
            provider, session_id, status_event, status_sequence,
            status_received_at_ms, projected_status
         ) VALUES ('codex', 'root-1', 'subagent_stop', 1, 1, 'idle')",
        "INSERT INTO lifecycle_leases (
            provider, session_id, status_event, status_sequence,
            status_received_at_ms, projected_status
         ) VALUES ('codex', 'root-1', 'stop', 0, 1, 'idle')",
        "INSERT INTO lifecycle_leases (
            provider, session_id, status_event, status_sequence,
            status_received_at_ms, projected_status
         ) VALUES ('codex', 'root-1', 'stop', 1, 1, 'processing')",
        "INSERT INTO lifecycle_leases (
            provider, session_id, status_event, status_sequence,
            status_received_at_ms, projected_status
         ) VALUES ('codex', 'root-1', 'pre_tool_use', 1, 1, 'idle')",
        "INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open
         ) VALUES ('codex', 'root-1', 'current', 'turn-2', 1)",
        "INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open, recent_position
         ) VALUES ('codex', 'root-1', 'recent', 'turn-2', 1, 1)",
        "INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open, recent_position
         ) VALUES ('codex', 'root-1', 'recent', 'turn-2', 0, 0)",
        "INSERT INTO lifecycle_turns (
            provider, session_id, continuity_state, turn_id, turn_open, recent_position
         ) VALUES ('codex', 'root-1', 'recent', 'turn-2', 0, 32)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }

    connection
        .execute_batch(
            "INSERT INTO lifecycle_subagents (
                provider, parent_session_id, agent_id, turn_id, subagent_state,
                topology_slot, state_sequence, received_at_ms
             ) VALUES ('codex', 'root-1', 'child-1', 'turn-1', 'active', 0, 2, 2);
             INSERT INTO lifecycle_subagents (
                provider, parent_session_id, agent_id, turn_id, subagent_state,
                topology_slot, state_sequence, received_at_ms
             ) VALUES ('codex', 'root-1', 'stopped-child', 'turn-0', 'stopped', 0, 1, 1);
             INSERT INTO lifecycle_subagents (
                provider, parent_session_id, agent_id, turn_id, subagent_state,
                topology_slot, state_sequence, received_at_ms
             ) VALUES ('claude', 'claude-1', 'claude-1', 'turn-1', 'active', 0, 1, 1);
             INSERT INTO lifecycle_subagents (
                provider, parent_session_id, agent_id, turn_id, subagent_state,
                topology_slot, state_sequence, received_at_ms
             ) VALUES ('antigravity', 'agy-1', 'agy-1', 'turn-1', 'active', 0, 1, 1);
             INSERT INTO lifecycle_invocations (
                provider, session_id, invocation_id, invocation_state,
                initial_step, state_sequence, received_at_ms
             ) VALUES ('antigravity', 'agy-1', 'invocation-1', 'active', 3, 2, 2);
             INSERT INTO lifecycle_invocations (
                provider, session_id, invocation_id, invocation_state,
                initial_step, state_sequence, received_at_ms
             ) VALUES ('antigravity', 'agy-1', 'invocation-0', 'stopped', NULL, 1, 1);
             INSERT INTO lifecycle_invocation_steps (
                provider, session_id, invocation_id, step, step_slot,
                pre_tool_seen, post_tool_seen
             ) VALUES ('antigravity', 'agy-1', 'invocation-1', 3, 0, 1, 0);",
        )
        .unwrap();

    for rejected in [
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'missing', 'orphan-child', 'turn-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'root-2', 'child-1', 'turn-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'root-1', 'other-active', 'turn-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'root-1', 'overflow', 'turn-1', 'active', 64, 1, 1)",
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('codex', 'root-2', 'root-2', 'turn-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES ('claude', 'claude-1', 'stopped', 'turn-1', 'stopped', 0, 1, 1)",
        "INSERT INTO lifecycle_invocations (
            provider, session_id, invocation_id, invocation_state,
            initial_step, state_sequence, received_at_ms
         ) VALUES ('antigravity', 'agy-1', 'invocation-2', 'active', 4, 3, 3)",
        "INSERT INTO lifecycle_invocations (
            provider, session_id, invocation_id, invocation_state,
            initial_step, state_sequence, received_at_ms
         ) VALUES ('codex', 'root-1', 'invocation-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_invocations (
            provider, session_id, invocation_id, invocation_state,
            initial_step, state_sequence, received_at_ms
         ) VALUES ('antigravity', 'missing', 'invocation-1', 'active', 0, 1, 1)",
        "INSERT INTO lifecycle_invocation_steps (
            provider, session_id, invocation_id, step, step_slot,
            pre_tool_seen, post_tool_seen
         ) VALUES ('antigravity', 'agy-1', 'missing', 4, 1, 1, 0)",
        "INSERT INTO lifecycle_invocation_steps (
            provider, session_id, invocation_id, step, step_slot,
            pre_tool_seen, post_tool_seen
         ) VALUES ('antigravity', 'agy-1', 'invocation-1', 4, 1, 0, 0)",
        "INSERT INTO lifecycle_invocation_steps (
            provider, session_id, invocation_id, step, step_slot,
            pre_tool_seen, post_tool_seen
         ) VALUES ('antigravity', 'agy-1', 'invocation-1', 4, 256, 1, 0)",
        "INSERT INTO lifecycle_invocation_steps (
            provider, session_id, invocation_id, step, step_slot,
            pre_tool_seen, post_tool_seen
         ) VALUES ('antigravity', 'agy-1', 'invocation-1', 4, 0, 0, 1)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }
}

#[test]
fn lifecycle_schema_maps_the_complete_non_permission_snapshot() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    let expected = [
        ("lifecycle_meta", ["singleton", "next_sequence"].as_slice()),
        (
            "lifecycle_sessions",
            [
                "provider",
                "session_id",
                "cwd",
                "transcript_path",
                "provider_session_id",
                "latest_event",
                "latest_sequence",
                "latest_received_at_ms",
                "session_start_source",
                "ignored_reason",
                "signature_event",
                "signature_turn_id",
                "signature_detail_id",
                "signature_session_start_source",
            ]
            .as_slice(),
        ),
        (
            "lifecycle_leases",
            [
                "provider",
                "session_id",
                "status_event",
                "status_sequence",
                "status_received_at_ms",
                "projected_status",
            ]
            .as_slice(),
        ),
        (
            "lifecycle_turns",
            [
                "provider",
                "session_id",
                "continuity_state",
                "turn_id",
                "turn_open",
                "recent_position",
            ]
            .as_slice(),
        ),
        (
            "lifecycle_subagents",
            [
                "provider",
                "parent_session_id",
                "agent_id",
                "turn_id",
                "subagent_state",
                "topology_slot",
                "state_sequence",
                "received_at_ms",
            ]
            .as_slice(),
        ),
        (
            "lifecycle_invocations",
            [
                "provider",
                "session_id",
                "invocation_id",
                "invocation_state",
                "initial_step",
                "state_sequence",
                "received_at_ms",
            ]
            .as_slice(),
        ),
        (
            "lifecycle_invocation_steps",
            [
                "provider",
                "session_id",
                "invocation_id",
                "step",
                "step_slot",
                "pre_tool_seen",
                "post_tool_seen",
            ]
            .as_slice(),
        ),
    ];

    for (table, columns) in expected {
        let actual = table_columns(&connection, table);
        assert_eq!(actual, columns, "unexpected columns for {table}");
        assert!(
            actual.iter().all(|column| !column.contains("permission")),
            "permission state leaked into {table}"
        );
    }
    assert_eq!(
        connection
            .query_row(
                "SELECT next_sequence FROM lifecycle_meta WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
}

#[test]
fn required_query_indexes_are_frozen_in_brain_schema() {
    let required = [
        "activity_events_activity_id",
        "activity_events_correction",
        "activity_events_cursor",
        "activity_events_distillation",
        "activity_events_outcome",
        "activity_events_permission_identity",
        "decision_identities_authority",
        "decision_payloads_source_cursor",
        "lifecycle_invocation_steps_exact",
        "lifecycle_invocations_active",
        "lifecycle_invocations_state",
        "lifecycle_leases_status",
        "lifecycle_sessions_latest",
        "lifecycle_sessions_provider_parent",
        "lifecycle_subagents_child",
        "lifecycle_subagents_parent_state",
        "lifecycle_turns_current",
        "lifecycle_turns_exact",
        "lifecycle_turns_recent_position",
        "permission_attempts_request_active",
        "permission_commits_request_authority",
        "permission_commits_undelivered_audit",
    ];

    for index in required {
        assert!(
            BRAIN_SCHEMA_V1.contains(&format!("CREATE INDEX {index}"))
                || BRAIN_SCHEMA_V1.contains(&format!("CREATE UNIQUE INDEX {index}")),
            "missing frozen query index {index}"
        );
    }
    assert!(BRAIN_SCHEMA_V1.contains(
        "ON decision_identities (provider, session_id, turn_id, tool_use_id, authority_action, decided_at_ms DESC)"
    ));
    assert!(BRAIN_SCHEMA_V1.contains(
        "ON activity_events (terminal_provider, terminal_session_id, terminal_turn_id, terminal_tool_use_id, terminal_action, source_cursor DESC)"
    ));
    assert!(BRAIN_SCHEMA_V1.contains(
        "ON permission_commits (provider, session_id, turn_id, tool_use_id, authority_action, committed_at_ms DESC)"
    ));
}

#[test]
fn fresh_review_database_is_isolated_and_constrained() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.review_db());

    assert_eq!(
        connection
            .pragma_query_value(None, "application_id", |row| row.get::<_, i32>(0))
            .unwrap(),
        REVIEW_APPLICATION_ID
    );
    assert_eq!(
        connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i32>(0))
            .unwrap(),
        REVIEW_SCHEMA_VERSION
    );
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(tables, ["review_marks", "review_meta"]);
    let attached = connection
        .prepare("PRAGMA database_list")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(attached, ["main"]);

    let invalid_disposition = connection.execute(
        "INSERT INTO review_marks (surface, group_id, source_cursor, disposition, revision)
         VALUES ('review', 'group-1', 1, 'maybe', 1)",
        [],
    );
    assert!(invalid_disposition.is_err());

    for rejected in [
        "INSERT INTO review_meta (surface, revision, source_high_water)
         VALUES ('unknown', 0, 0)",
        "UPDATE review_meta SET revision = -1 WHERE surface = 'review'",
        "UPDATE review_meta SET source_high_water = -1 WHERE surface = 'review'",
    ] {
        assert_statement_rejected(&connection, rejected);
    }
    for (surface, group_id, cursor, revision) in [
        ("unknown", "group-1", 1, 1),
        ("review", "", 1, 1),
        ("review", "group-1", 0, 1),
        ("review", "group-1", 1, 0),
    ] {
        assert!(
            connection
                .execute(
                    "INSERT INTO review_marks (
                        surface, group_id, source_cursor, disposition, revision
                     ) VALUES (?1, ?2, ?3, 'reviewed', ?4)",
                    params![surface, group_id, cursor, revision],
                )
                .is_err()
        );
    }
    connection
        .execute(
            "INSERT INTO review_marks (
                surface, group_id, source_cursor, disposition, revision
             ) VALUES ('review', 'group-1', 1, 'reviewed', 1)",
            [],
        )
        .unwrap();
    assert_statement_rejected(
        &connection,
        "INSERT INTO review_marks (
            surface, group_id, source_cursor, disposition, revision
         ) VALUES ('review', 'group-1', 1, 'archived', 2)",
    );
    connection
        .execute(
            "INSERT INTO review_marks (
                surface, group_id, source_cursor, disposition, revision
             ) VALUES ('attention', 'group-1', 1, 'archived', 1)",
            [],
        )
        .unwrap();
}
