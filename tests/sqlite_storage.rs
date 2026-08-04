use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use coding_brain::brain::storage::{
    BRAIN_APPLICATION_ID, BRAIN_SCHEMA_VERSION, BrainDb, OpenRole, REVIEW_APPLICATION_ID,
    REVIEW_SCHEMA_VERSION, ReviewDb, StorageDeadline, StorageError, StoragePaths,
};
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
    let root = tempfile::tempdir().unwrap();
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
    let root = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let linked_root = root.path().join("linked-state");
    symlink(target.path(), &linked_root).unwrap();

    let error = BrainDb::create_current(&StoragePaths::at(&linked_root)).unwrap_err();
    assert!(matches!(error, StorageError::InvalidStorage(_)));

    let state = root.path().join("state");
    fs::create_dir(&state).unwrap();
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
fn preexisting_sidecars_are_rejected_before_database_creation() {
    let root = tempfile::tempdir().unwrap();
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
    let root = tempfile::tempdir().unwrap();
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
fn expired_deadline_fails_before_opening_storage() {
    let root = tempfile::tempdir().unwrap();
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
    let root = tempfile::tempdir().unwrap();
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
}

#[test]
fn permission_commit_requires_matching_authority_identity_and_action() {
    let root = tempfile::tempdir().unwrap();
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
fn lifecycle_schema_enforces_provider_qualified_topology() {
    let root = tempfile::tempdir().unwrap();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    let orphan = connection.execute(
        "INSERT INTO lifecycle_turns (
            provider, session_id, turn_id, turn_state, sequence, updated_at_ms
         ) VALUES ('codex', 'missing', 'turn-1', 'active', 1, 1)",
        [],
    );
    assert!(orphan.is_err());

    connection
        .execute(
            "INSERT INTO lifecycle_sessions (
                provider, session_id, provider_session_id, lifecycle_state, sequence, updated_at_ms
             ) VALUES ('codex', 'session-1', 'root-1', 'active', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO lifecycle_turns (
                provider, session_id, turn_id, turn_state, sequence, updated_at_ms
             ) VALUES ('codex', 'session-1', 'turn-1', 'active', 1, 1)",
            [],
        )
        .unwrap();
    let duplicate_active_turn = connection.execute(
        "INSERT INTO lifecycle_turns (
            provider, session_id, turn_id, turn_state, sequence, updated_at_ms
         ) VALUES ('codex', 'session-1', 'turn-2', 'active', 2, 2)",
        [],
    );
    assert!(duplicate_active_turn.is_err());
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
        "lifecycle_invocations_active_identity",
        "lifecycle_invocations_exact",
        "lifecycle_sessions_active_identity",
        "lifecycle_sessions_active_topology",
        "lifecycle_turns_active_identity",
        "lifecycle_turns_exact",
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
    let root = tempfile::tempdir().unwrap();
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
    assert_eq!(
        connection
            .query_row("PRAGMA database_list", [], |row| row.get::<_, String>(1))
            .unwrap(),
        "main"
    );

    let invalid_disposition = connection.execute(
        "INSERT INTO review_marks (surface, group_id, source_cursor, disposition, revision)
         VALUES ('review', 'group-1', 1, 'maybe', 1)",
        [],
    );
    assert!(invalid_disposition.is_err());
}
