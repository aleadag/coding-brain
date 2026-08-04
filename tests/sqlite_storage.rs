use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use coding_brain::brain::decisions::{
    DecisionContext, DecisionOutcome, DecisionRecord, DecisionType,
};
use coding_brain::brain::storage::{
    ActivityCursor, BRAIN_APPLICATION_ID, BRAIN_SCHEMA_VERSION, BrainDb, DecisionIdentity,
    DecisionKind, DecisionPayload, LearningErasePaths, OpenRole, REVIEW_APPLICATION_ID,
    REVIEW_SCHEMA_VERSION, ReviewDb, ReviewEligibility, ReviewEligibleOccurrence, StorageDeadline,
    StorageError, StoragePaths,
};
use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
    CorrectionDisposition, ProjectEvidence, SessionTarget, SessionTargetProvenance, SnapshotLimits,
};
use coding_brain_core::lifecycle::{
    LifecycleEvent, LifecycleEventKind, LifecycleIdentity, LifecycleSnapshot, PermissionAction,
    PermissionAuthority, SessionStartSource,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use coding_brain_core::review_state::{
    MAX_REVIEW_KEYS, ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest,
    ReviewRequestError, ReviewSurface,
};
use fs2::FileExt;
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

fn create_managed_dir(path: &std::path::Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_managed_file(path: &std::path::Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn review_eligibility(
    surface: ReviewSurface,
    source_high_water: u64,
    occurrences: &[(&str, u64)],
) -> ReviewEligibility {
    ReviewEligibility::try_new(
        surface,
        (source_high_water != 0).then(|| ActivityCursor::try_from(source_high_water).unwrap()),
        occurrences
            .iter()
            .map(|(group_id, source_cursor)| {
                ReviewEligibleOccurrence::new(
                    surface,
                    ReviewKey::derive(surface, group_id.as_bytes()),
                    ActivityCursor::try_from(*source_cursor).unwrap(),
                )
            })
            .collect(),
    )
    .unwrap()
}

fn complete_decision(decision_id: &str, provider: AgentProvider) -> DecisionRecord {
    DecisionRecord {
        provider,
        timestamp: "2026-08-04T12:00:00Z".into(),
        pid: 42,
        project: "sqlite-project".into(),
        tool: Some("Bash".into()),
        command: Some("cargo test".into()),
        brain_action: "approve".into(),
        brain_confidence: 0.91,
        brain_reasoning: "bounded reason".into(),
        user_action: "user_approve".into(),
        context: Some(DecisionContext {
            context_pct: Some(73),
            last_tool_error: true,
            error_message: Some("opaque supported detail".into()),
            model: "model-v1".into(),
            elapsed_secs: 123,
            files_modified_count: 4,
            total_tool_calls: 9,
            has_file_conflict: true,
            status: "waiting".into(),
            recent_error_count: 2,
            subagent_count: 1,
            hour: Some(12),
        }),
        outcome: Some(DecisionOutcome::Error("exit 101".into())),
        decision_type: DecisionType::Session,
        suggested_at: Some(100),
        resolved_at: Some(103),
        override_reason: Some("explicit exception".into()),
        decision_id: Some(decision_id.into()),
        brain_decision_ms: Some(321),
        cache_hit: Some(false),
        canonical: Some(true),
    }
}

fn activity_event(activity_id: &str, recorded_at_ms: u64, state: ActivityState) -> ActivityEvent {
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: activity_id.into(),
        recorded_at_ms,
        project: ProjectEvidence {
            project_id: ProjectId::Temporary("sqlite-project".into()),
            cwd: "/work/sqlite-project".into(),
            label: Some("sqlite-project".into()),
        },
        session: None,
        state,
        tool: Some("Bash".into()),
        normalized_command: Some("printf safe".into()),
        fingerprint: Some("fingerprint".into()),
        rule_id: Some("rule".into()),
        confidence: Some(0.9),
        threshold: Some(0.8),
        reasoning: Some("bounded reason".into()),
        decision_id: Some(format!("decision-{activity_id}")),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

fn identified_activity_event(
    activity_id: &str,
    recorded_at_ms: u64,
    state: ActivityState,
    provider: AgentProvider,
) -> ActivityEvent {
    let mut event = activity_event(activity_id, recorded_at_ms, state);
    event.session = Some(SessionTarget {
        provider,
        session_id: format!("session-{activity_id}"),
        provider_session_id: Some(format!("native-{activity_id}")),
        turn_id: Some(format!("turn-{activity_id}")),
        tool_use_id: Some(format!("tool-{activity_id}")),
        project_id: event.project.project_id.clone(),
        cwd: event.project.cwd.clone(),
        provider_hints: vec!["hint".into()],
        provenance: SessionTargetProvenance::Structured,
    });
    event
}

fn decision_activity_event(
    activity_id: &str,
    decision_id: &str,
    recorded_at_ms: u64,
    state: ActivityState,
    provider: Option<AgentProvider>,
) -> ActivityEvent {
    let mut event = match provider {
        Some(provider) => identified_activity_event(activity_id, recorded_at_ms, state, provider),
        None => activity_event(activity_id, recorded_at_ms, state),
    };
    event.decision_id = Some(decision_id.into());
    event
}

#[test]
fn activity_cursor_allows_repeated_logical_ids_and_survives_delete() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let first = db
        .append_activity(activity_event("activity-1", 1, ActivityState::Observed))
        .unwrap();
    let second = db
        .append_activity(activity_event("activity-1", 2, ActivityState::Denied))
        .unwrap();

    assert!(second > first);
    assert_eq!(
        db.activity_by_id("activity-1", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        2
    );
    db.delete_activity_before(second).unwrap();
    let third = db
        .append_activity(activity_event("activity-2", 3, ActivityState::Denied))
        .unwrap();
    assert!(third > second);
    assert_eq!(db.activity_high_water().unwrap(), Some(third));
    assert_eq!(
        ActivityCursor::try_from(i64::MAX as u64).unwrap().get(),
        i64::MAX as u64
    );
}

#[test]
fn activity_batch_is_atomic_and_keeps_high_water_on_failure() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let duplicate =
        identified_activity_event("duplicate", 1, ActivityState::Denied, AgentProvider::Codex);

    assert!(
        db.append_activity_batch(&[duplicate.clone(), duplicate])
            .is_err()
    );
    assert!(
        db.activity_after_cursor(None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(db.activity_high_water().unwrap(), None);

    let cursors = db
        .append_activity_batch(&[
            activity_event("one", 1, ActivityState::Denied),
            activity_event("two", 2, ActivityState::Denied),
        ])
        .unwrap();
    assert_eq!(
        cursors
            .iter()
            .map(|cursor| cursor.get())
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn activity_append_rejects_mixed_kinds_for_one_logical_id() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    db.append_activity(activity_event("one-kind", 1, ActivityState::Denied))
        .unwrap();
    let mut diagnostic = activity_event("one-kind", 2, ActivityState::Error);
    diagnostic.kind = ActivityKind::Diagnostic;
    diagnostic.decision_id = None;

    assert!(matches!(
        db.append_activity(diagnostic),
        Err(StorageError::InvalidStorage(_))
    ));
    assert_eq!(db.activity_high_water().unwrap().unwrap().get(), 1);
}

#[test]
fn activity_pages_are_bounded_and_ordered_by_cursor() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursors = db
        .append_activity_batch(&[
            activity_event("one", 30, ActivityState::Denied),
            activity_event("two", 10, ActivityState::Denied),
            activity_event("three", 20, ActivityState::Denied),
        ])
        .unwrap();

    let recent = db.read_activity_page(None, 2, 128 * 1024).unwrap();
    assert_eq!(
        recent
            .events
            .iter()
            .map(|row| row.cursor)
            .collect::<Vec<_>>(),
        [cursors[2], cursors[1]]
    );
    assert_eq!(recent.next_cursor, Some(cursors[1]));
    assert!(recent.serialized_bytes > 0);
    let older = db
        .read_activity_page(recent.next_cursor, 2, 128 * 1024)
        .unwrap();
    assert_eq!(
        older
            .events
            .iter()
            .map(|row| row.cursor)
            .collect::<Vec<_>>(),
        [cursors[0]]
    );
    assert_eq!(older.next_cursor, None);

    let ascending = db.activity_after_cursor(None, 2, 128 * 1024).unwrap();
    assert_eq!(
        ascending
            .events
            .iter()
            .map(|row| row.cursor)
            .collect::<Vec<_>>(),
        [cursors[0], cursors[1]]
    );
    let rest = db
        .activity_after_cursor(ascending.next_cursor, 2, 128 * 1024)
        .unwrap();
    assert_eq!(
        rest.events.iter().map(|row| row.cursor).collect::<Vec<_>>(),
        [cursors[2]]
    );
    assert_eq!(rest.next_cursor, None);
    assert!(
        db.activity_after_cursor(Some(cursors[2]), 2, 128 * 1024)
            .unwrap()
            .events
            .is_empty()
    );

    let one_payload_bytes = db
        .activity_after_cursor(None, 1, 128 * 1024)
        .unwrap()
        .serialized_bytes;
    assert!(matches!(
        db.activity_after_cursor(None, 3, one_payload_bytes - 1),
        Err(StorageError::InvalidStorage(_))
    ));
    let byte_page = db
        .activity_after_cursor(None, 3, one_payload_bytes)
        .unwrap();
    assert_eq!(byte_page.events.len(), 1);
    assert_eq!(byte_page.next_cursor, Some(cursors[0]));
    let byte_rest = db
        .activity_after_cursor(byte_page.next_cursor, 3, 128 * 1024)
        .unwrap();
    assert_eq!(byte_rest.events.len(), 2);
    assert_eq!(byte_rest.next_cursor, None);
    assert!(db.activity_after_cursor(None, 0, 1).is_err());
    assert!(db.activity_after_cursor(None, 1, 0).is_err());
}

#[test]
fn activity_terminal_identity_keeps_opposite_actions_and_rejects_same_action_duplicates() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let denied =
        identified_activity_event("permission", 1, ActivityState::Denied, AgentProvider::Codex);
    let allowed = identified_activity_event(
        "permission",
        2,
        ActivityState::Allowed,
        AgentProvider::Codex,
    );

    db.append_activity(denied.clone()).unwrap();
    db.append_activity(allowed).unwrap();
    assert_eq!(
        db.activity_by_id("permission", None, 10, 128 * 1024)
            .unwrap()
            .events
            .len(),
        2
    );
    assert!(db.append_activity(denied).is_err());
    assert_eq!(db.activity_high_water().unwrap().unwrap().get(), 2);
    assert_eq!(
        db.activity_by_id("permission", None, 10, 128 * 1024)
            .unwrap()
            .project_complete_window(SnapshotLimits::default(), 3)
            .diagnostics
            .duplicate_terminal_states,
        1
    );
}

#[test]
fn activity_cursor_exhaustion_fails_before_insert() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE schema_meta SET activity_high_water = ?1",
            [i64::MAX],
        )
        .unwrap();
    drop(connection);
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.append_activity(activity_event("overflow", 1, ActivityState::Denied)),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(
        db.activity_after_cursor(None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty()
    );
    assert_eq!(
        db.activity_high_water().unwrap().unwrap().get(),
        i64::MAX as u64
    );
}

#[test]
fn activity_high_water_rejects_a_value_below_retained_rows() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    db.append_activity(activity_event("retained", 1, ActivityState::Denied))
        .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute("UPDATE schema_meta SET activity_high_water = 0", [])
        .unwrap();
    drop(connection);
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.activity_high_water(),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(matches!(
        db.append_activity(activity_event("next", 2, ActivityState::Denied)),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn activity_retention_cannot_hide_a_lowered_high_water() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    db.append_activity_batch(&[
        activity_event("first", 1, ActivityState::Denied),
        activity_event("second", 2, ActivityState::Denied),
    ])
    .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute("UPDATE schema_meta SET activity_high_water = 1", [])
        .unwrap();
    drop(connection);
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.delete_activity_before(ActivityCursor::try_from(3_u64).unwrap()),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(matches!(
        db.append_activity(activity_event("third", 3, ActivityState::Denied)),
        Err(StorageError::InvalidStorage(_))
    ));
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM activity_events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn activity_id_pages_reconstruct_repeated_logical_sequence() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let mut observed = activity_event("sequence", 1, ActivityState::Observed);
    observed.normalized_command = None;
    observed.fingerprint = None;
    observed.reasoning = None;
    db.append_activity_batch(&[
        observed,
        activity_event("sequence", 2, ActivityState::Denied),
        activity_event("sequence", 3, ActivityState::Delivered),
    ])
    .unwrap();

    let mut after = None;
    let mut states = Vec::new();
    loop {
        let page = db.activity_by_id("sequence", after, 1, 64 * 1024).unwrap();
        states.extend(page.events.into_iter().map(|row| row.event.state));
        let Some(cursor) = page.next_cursor else {
            break;
        };
        after = Some(cursor);
    }
    assert_eq!(
        states,
        [
            ActivityState::Observed,
            ActivityState::Denied,
            ActivityState::Delivered
        ]
    );

    let first_bytes = db
        .activity_by_id("sequence", None, 1, 64 * 1024)
        .unwrap()
        .serialized_bytes;
    let first = db
        .activity_by_id("sequence", None, 10, first_bytes)
        .unwrap();
    assert_eq!(first.events.len(), 1);
    assert!(first.next_cursor.is_some());
    let rest = db
        .activity_by_id("sequence", first.next_cursor, 10, 64 * 1024)
        .unwrap();
    assert_eq!(rest.events.len(), 2);
    assert_eq!(rest.next_cursor, None);
}

#[test]
fn activity_read_rejects_mixed_kinds_even_when_rows_straddle_pages() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    db.append_activity(activity_event("mixed", 1, ActivityState::Denied))
        .unwrap();
    drop(db);
    let mut diagnostic = activity_event("mixed", 2, ActivityState::Error);
    diagnostic.kind = ActivityKind::Diagnostic;
    diagnostic.decision_id = None;
    let diagnostic = diagnostic.normalized();
    let payload = serde_json::to_vec(&diagnostic).unwrap();
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "INSERT INTO activity_events (
                source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                event_payload
             ) VALUES (2, 'mixed', 'diagnostic', 'error', 2, ?1)",
            [payload],
        )
        .unwrap();
    connection
        .execute("UPDATE schema_meta SET activity_high_water = 2", [])
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.activity_by_id("mixed", None, 1, 64 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(matches!(
        db.activity_after_cursor(None, 1, 64 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn activity_reads_reject_typed_payload_disagreement_and_unsupported_payloads() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    db.append_activity(activity_event("corrupt", 1, ActivityState::Denied))
        .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute("UPDATE activity_events SET event_state = 'allowed'", [])
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    assert!(matches!(
        db.activity_after_cursor(None, 10, 64 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    let mut payload: serde_json::Value = serde_json::from_slice(
        &connection
            .query_row("SELECT event_payload FROM activity_events", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .unwrap(),
    )
    .unwrap();
    payload["schema_version"] = serde_json::json!(ACTIVITY_SCHEMA_VERSION + 1);
    connection
        .execute(
            "UPDATE activity_events SET event_state = 'denied', event_payload = ?1",
            [serde_json::to_vec(&payload).unwrap()],
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
        db.activity_after_cursor(None, 10, 64 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn activity_queries_use_frozen_indexes() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let events = (0..2_000)
        .map(|index| activity_event(&format!("activity-{index}"), index, ActivityState::Denied))
        .collect::<Vec<_>>();
    db.append_activity_batch(&events).unwrap();

    assert!(
        db.explain_recent_activity()
            .unwrap()
            .contains("activity_events_cursor")
    );
    assert!(
        db.explain_activity_by_id()
            .unwrap()
            .contains("activity_events_activity_id")
    );
    let after_plan = db.explain_activity_after_cursor().unwrap();
    assert!(after_plan.contains("activity_events_cursor"));
    assert!(after_plan.contains("source_cursor>?"));
    assert!(
        db.explain_recent_activity()
            .unwrap()
            .contains("USING COVERING INDEX")
    );
}

#[test]
fn activity_round_trip_preserves_supported_kinds_states_and_providers() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let mut events = vec![
        identified_activity_event("observed", 1, ActivityState::Observed, AgentProvider::Codex),
        identified_activity_event(
            "evaluating",
            2,
            ActivityState::Evaluating,
            AgentProvider::Claude,
        ),
        identified_activity_event(
            "allowed",
            3,
            ActivityState::Allowed,
            AgentProvider::Antigravity,
        ),
        activity_event("denied", 4, ActivityState::Denied),
        activity_event("abstained", 5, ActivityState::Abstained),
        activity_event("error", 6, ActivityState::Error),
        activity_event("delivered", 7, ActivityState::Delivered),
        activity_event("delivery-failed", 8, ActivityState::DeliveryFailed),
        activity_event("interrupted", 9, ActivityState::Interrupted),
    ];
    let mut outcome = activity_event("outcome", 10, ActivityState::Outcome);
    outcome.outcome = Some(ActivityOutcome::Failed);
    events.push(outcome);
    let mut correction = activity_event("correction", 11, ActivityState::Correction);
    correction.correction = Some(CorrectionDisposition::BrainWrong);
    correction.note = Some("token secret-value".into());
    events.push(correction);
    let mut cancelled = activity_event("cancelled", 12, ActivityState::Outcome);
    cancelled.outcome = Some(ActivityOutcome::Cancelled);
    events.push(cancelled);
    let mut completed = activity_event("completed", 13, ActivityState::Outcome);
    completed.outcome = Some(ActivityOutcome::Completed);
    events.push(completed);
    let mut exception = activity_event("exception", 14, ActivityState::Correction);
    exception.correction = Some(CorrectionDisposition::Exception);
    exception.note = Some("bounded exception".into());
    events.push(exception);
    let mut lifecycle = activity_event("lifecycle", 15, ActivityState::Abstained);
    lifecycle.kind = ActivityKind::Lifecycle;
    lifecycle.decision_id = None;
    lifecycle.normalized_command = None;
    lifecycle.rule_id = None;
    lifecycle.confidence = None;
    lifecycle.threshold = None;
    events.push(lifecycle);
    let mut diagnostic = activity_event("diagnostic", 16, ActivityState::Error);
    diagnostic.kind = ActivityKind::Diagnostic;
    diagnostic.decision_id = None;
    events.push(diagnostic);
    let mut expected = events
        .iter()
        .cloned()
        .map(ActivityEvent::normalized)
        .collect::<Vec<_>>();
    for event in &mut expected {
        if let Some(session) = &mut event.session {
            session.provider_hints.clear();
        }
    }

    db.append_activity_batch(&events).unwrap();
    let actual = db
        .activity_after_cursor(None, 100, 1024 * 1024)
        .unwrap()
        .events
        .into_iter()
        .map(|row| row.event)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(actual[10].note.as_deref(), Some("token [REDACTED]"));
}

#[test]
fn concurrent_activity_writers_allocate_unique_nonreusing_cursors() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let writers = (0..4)
        .map(|writer| {
            let paths = paths.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut db = BrainDb::open_current(
                    &paths,
                    OpenRole::NonHook,
                    StorageDeadline::after(Duration::from_secs(5)),
                )
                .unwrap();
                barrier.wait();
                (0..20)
                    .map(|index| {
                        db.append_activity(activity_event(
                            &format!("writer-{writer}-{index}"),
                            writer * 20 + index,
                            ActivityState::Denied,
                        ))
                        .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let mut cursors = writers
        .into_iter()
        .flat_map(|writer| writer.join().unwrap())
        .collect::<Vec<_>>();
    cursors.sort_unstable();
    cursors.dedup();

    assert_eq!(cursors.len(), 80);
    assert_eq!(cursors.first().unwrap().get(), 1);
    assert_eq!(cursors.last().unwrap().get(), 80);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert_eq!(db.activity_high_water().unwrap().unwrap().get(), 80);
}

#[test]
fn activity_round_trip_preserves_opaque_paths_and_projects_incomplete_state() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let mut event = identified_activity_event(
        "opaque",
        1,
        ActivityState::Evaluating,
        AgentProvider::Claude,
    );
    let opaque = std::path::PathBuf::from(OsString::from_vec(b"/work/opaque-\xff".to_vec()));
    event.project.cwd = opaque.clone();
    event.session.as_mut().unwrap().cwd = opaque.clone();
    db.append_activity(event).unwrap();

    let page = db.activity_after_cursor(None, 10, 64 * 1024).unwrap();
    assert_eq!(page.events[0].event.project.cwd, opaque);
    assert_eq!(page.events[0].event.session.as_ref().unwrap().cwd, opaque);
    assert_eq!(
        page.project_complete_window(
            SnapshotLimits {
                interrupted_after_ms: 1,
                ..SnapshotLimits::default()
            },
            10,
        )
        .attention[0]
            .state,
        ActivityState::Incomplete
    );
}

#[test]
fn activity_sqlite_page_projects_like_the_legacy_log() {
    use coding_brain::brain::activity::ActivityStore;

    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let store = ActivityStore::at(root.path().join("activity.jsonl"));
    let observed = activity_event("sequence", 10, ActivityState::Observed);
    let denied = activity_event("sequence", 11, ActivityState::Denied);
    let delivered = activity_event("sequence", 12, ActivityState::Delivered);
    let mut outcome = activity_event("sequence", 13, ActivityState::Outcome);
    outcome.outcome = Some(ActivityOutcome::Succeeded);
    outcome.supersedes = Some("older-sequence".into());
    let mut correction = activity_event("sequence", 14, ActivityState::Correction);
    correction.correction = Some(CorrectionDisposition::BrainRight);
    let older = activity_event("older-sequence", 1, ActivityState::Denied);
    let events = [older, observed, denied, delivered, outcome, correction];
    for event in &events {
        store.append(event.clone()).unwrap();
    }
    db.append_activity_batch(&events).unwrap();

    let limits = SnapshotLimits::default();
    let legacy = store.snapshot(limits).unwrap();
    let sqlite = db
        .activity_after_cursor(None, 100, 1024 * 1024)
        .unwrap()
        .project_complete_window(limits, 20);
    assert_eq!(sqlite, legacy);
}

#[test]
fn activity_projection_requires_a_complete_caller_assembled_window() {
    use coding_brain::brain::activity::ActivityStore;

    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let store = ActivityStore::at(root.path().join("activity.jsonl"));
    let events = [
        activity_event("boundary", 1, ActivityState::Observed),
        activity_event("boundary", 2, ActivityState::Denied),
        activity_event("boundary", 3, ActivityState::Delivered),
    ];
    for event in &events {
        store.append(event.clone()).unwrap();
    }
    db.append_activity_batch(&events).unwrap();

    let first = db.activity_after_cursor(None, 2, 128 * 1024).unwrap();
    let second = db
        .activity_after_cursor(first.next_cursor, 2, 128 * 1024)
        .unwrap();
    assert_eq!(first.events.len(), 2);
    assert_eq!(second.events.len(), 1);
    let mut records = first.events;
    records.extend(second.events);
    let assembled = coding_brain::brain::storage::ActivityPage {
        next_cursor: records.last().map(|record| record.cursor),
        serialized_bytes: 0,
        events: records,
    };

    assert_eq!(
        assembled.project_complete_window(SnapshotLimits::default(), 4),
        store.snapshot(SnapshotLimits::default()).unwrap()
    );
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
fn lifecycle_load_rejects_non_numeric_invocation_identity() {
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
        .execute_batch(
            "UPDATE lifecycle_invocations SET invocation_id = 'invocation-invalid';
             UPDATE lifecycle_turns SET turn_id = 'invocation-invalid'
             WHERE continuity_state = 'current';",
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
fn lifecycle_load_rejects_mismatched_session_start_sources() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "session-1".into(),
        None,
        None,
        "/work/project".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts(
            identity,
            LifecycleEventKind::SessionStart {
                source: SessionStartSource::Startup,
            },
        )
        .unwrap(),
        100,
    )
    .unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute_batch(
            "PRAGMA ignore_check_constraints = ON;
             UPDATE lifecycle_sessions SET session_start_source = 'resume';",
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
                decision_id, identity_kind, provider, session_id, turn_id, tool_use_id,
                authority_action, decision_source, decided_at_ms
             ) VALUES (?1, 'permission', 'codex', 'session-1', 'turn-1', 'tool-1',
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
    assert_eq!(
        db.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
        1024 * 1024 + 64 * 1024
    );
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
fn current_open_rejects_unknown_database_sidecars() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    fs::write(paths.db_dir().join("brain.sqlite3-unknown"), b"untrusted").unwrap();

    let error = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();

    assert!(matches!(error, StorageError::InvalidStorage(_)));
}

#[test]
fn current_open_rejects_unsafe_known_database_sidecars() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let sidecar = paths.db_dir().join(format!("brain.sqlite3{suffix}"));
        fs::write(&sidecar, b"untrusted").unwrap();
        fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();

        let error = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(1)),
        )
        .unwrap_err();

        assert!(matches!(error, StorageError::InvalidStorage(_)), "{suffix}");
    }

    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let target = paths.db_dir().join("target");
    fs::write(&target, b"untrusted").unwrap();
    symlink(&target, paths.db_dir().join("brain.sqlite3-wal")).unwrap();

    let error = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap_err();
    assert!(matches!(error, StorageError::InvalidStorage(_)));
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
fn decision_kinds_reject_incomplete_or_fabricated_authority() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());

    for rejected in [
        "INSERT INTO decision_identities (
            decision_id, identity_kind, provider, decided_at_ms
         ) VALUES ('permission-incomplete', 'permission', 'codex', 1)",
        "INSERT INTO decision_identities (
            decision_id, identity_kind, provider, session_id, turn_id, tool_use_id,
            authority_action, decision_source, decided_at_ms
         ) VALUES ('observation-smuggled', 'observation', 'codex', 'fake-session',
                   'fake-turn', 'fake-tool', 'allow', 'native_provider', 1)",
        "INSERT INTO decision_identities (
            decision_id, identity_kind, provider, decided_at_ms
         ) VALUES ('unknown-kind', 'unknown', 'codex', 1)",
    ] {
        assert_statement_rejected(&connection, rejected);
    }

    connection
        .execute(
            "INSERT INTO decision_identities (
                decision_id, identity_kind, provider, decided_at_ms
             ) VALUES ('observation-1', 'observation', 'claude', 1)",
            [],
        )
        .unwrap();
}

#[test]
fn payload_kind_and_source_cursor_are_database_enforced() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "INSERT INTO decision_identities (
                decision_id, identity_kind, provider, decided_at_ms
             ) VALUES ('observation-1', 'observation', 'codex', 1)",
            [],
        )
        .unwrap();
    insert_terminal_event(&connection, 1, "activity-1");

    for rejected in [
        "INSERT INTO decision_payloads (
            decision_id, payload_kind, source_cursor, decision_record
         ) VALUES ('observation-1', 'permission', 1, X'7b7d')",
        "INSERT INTO decision_payloads (
            decision_id, payload_kind, source_cursor, decision_record
         ) VALUES ('observation-1', 'observation', 2, X'7b7d')",
    ] {
        assert_statement_rejected(&connection, rejected);
    }

    connection
        .execute(
            "INSERT INTO decision_payloads (
                decision_id, payload_kind, source_cursor, decision_record
             ) VALUES ('observation-1', 'observation', 1, X'7b7d')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO decision_identities (
                decision_id, identity_kind, provider, decided_at_ms
             ) VALUES ('observation-2', 'observation', 'codex', 2)",
            [],
        )
        .unwrap();
    assert_statement_rejected(
        &connection,
        "INSERT INTO decision_payloads (
            decision_id, payload_kind, source_cursor, decision_record
         ) VALUES ('observation-2', 'observation', 1, X'7b7d')",
    );
}

#[test]
fn decision_storage_round_trips_complete_permission_and_observation_records() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let observation_cursor = db
        .append_activity(decision_activity_event(
            "observation",
            "observation-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    let permission_cursor = db
        .append_activity(decision_activity_event(
            "permission",
            "permission-decision",
            2,
            ActivityState::Allowed,
            Some(AgentProvider::Codex),
        ))
        .unwrap();
    let observation = complete_decision("observation-decision", AgentProvider::Claude);
    let mut permission = complete_decision("permission-decision", AgentProvider::Codex);
    permission.user_action = "hook_proposal".into();

    db.insert_decision(
        &DecisionIdentity::observation("observation-decision", AgentProvider::Claude, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            observation_cursor,
            observation.clone(),
        ),
    )
    .unwrap();
    db.insert_decision(
        &DecisionIdentity::permission(
            "permission-decision",
            AgentProvider::Codex,
            "session-permission",
            "turn-permission",
            "tool-permission",
            PermissionAction::Allow,
            "model",
            2,
        ),
        &DecisionPayload::new(
            DecisionKind::Permission,
            permission_cursor,
            permission.clone(),
        ),
    )
    .unwrap();

    assert_eq!(
        db.decision_payload("observation-decision")
            .unwrap()
            .unwrap()
            .record,
        observation
    );
    assert_eq!(
        db.decision_payload("permission-decision")
            .unwrap()
            .unwrap()
            .record,
        permission
    );
}

#[test]
fn maximum_decision_record_round_trips_with_bounded_typed_projections() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "maximum",
            "maximum-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    let mut record = complete_decision("maximum-decision", AgentProvider::Codex);
    record.command = Some("c".repeat(5000));
    record.brain_reasoning.clear();
    let mut payload = DecisionPayload::new(DecisionKind::Observation, cursor, record);
    let base = payload.serialized_len().unwrap();
    payload.record.brain_reasoning = "r".repeat(1024 * 1024 - base);
    assert_eq!(payload.serialized_len().unwrap(), 1024 * 1024);

    db.insert_decision(
        &DecisionIdentity::observation("maximum-decision", AgentProvider::Codex, 1),
        &payload,
    )
    .unwrap();
    assert_eq!(
        db.decision_payload("maximum-decision")
            .unwrap()
            .unwrap()
            .record,
        payload.record
    );
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    let (command_len, reasoning_len): (i64, i64) = connection
        .query_row(
            "SELECT length(normalized_command), length(reasoning)
             FROM decision_payloads WHERE decision_id = 'maximum-decision'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((command_len, reasoning_len), (4096, 4096));

    let mut oversized = payload;
    oversized.record.brain_reasoning.push('r');
    assert!(matches!(
        oversized.serialized_len(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn learning_requires_exact_permission_commit_and_paginates_by_source_cursor() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let proposal_cursor = db
        .append_activity(decision_activity_event(
            "proposal",
            "proposal-decision",
            1,
            ActivityState::Allowed,
            Some(AgentProvider::Codex),
        ))
        .unwrap();
    let observation_cursor = db
        .append_activity(decision_activity_event(
            "observation",
            "observation-decision",
            2,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    let committed_cursor = db
        .append_activity(decision_activity_event(
            "committed",
            "committed-decision",
            3,
            ActivityState::Allowed,
            Some(AgentProvider::Codex),
        ))
        .unwrap();

    for (decision_id, cursor, activity) in [
        ("proposal-decision", proposal_cursor, "proposal"),
        ("committed-decision", committed_cursor, "committed"),
    ] {
        let mut record = complete_decision(decision_id, AgentProvider::Codex);
        record.user_action = "hook_proposal".into();
        db.insert_decision(
            &DecisionIdentity::permission(
                decision_id,
                AgentProvider::Codex,
                format!("session-{activity}"),
                format!("turn-{activity}"),
                format!("tool-{activity}"),
                PermissionAction::Allow,
                "model",
                cursor.get(),
            ),
            &DecisionPayload::new(DecisionKind::Permission, cursor, record),
        )
        .unwrap();
    }
    db.insert_decision(
        &DecisionIdentity::observation("observation-decision", AgentProvider::Claude, 2),
        &DecisionPayload::new(
            DecisionKind::Observation,
            observation_cursor,
            complete_decision("observation-decision", AgentProvider::Claude),
        ),
    )
    .unwrap();
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "INSERT INTO permission_attempts (
                attempt_id, provider, session_id, turn_id, tool_use_id, request_key,
                authority_action, attempt_state, created_at_ms, updated_at_ms
             ) VALUES ('attempt-committed', 'codex', 'session-committed', 'turn-committed',
                       'tool-committed', 'request-committed', 'allow', 'decided', 3, 3)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO permission_commits (
                attempt_id, decision_id, terminal_activity_id, provider, session_id,
                turn_id, tool_use_id, authority_action, evidence_kind, delivery_state,
                response_eligible, committed_at_ms
             ) VALUES ('attempt-committed', 'committed-decision', 'committed', 'codex',
                       'session-committed', 'turn-committed', 'tool-committed', 'allow',
                       'provider_authority', 'pending', 1, 3)",
            [],
        )
        .unwrap();
    drop(connection);

    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    let learned = db.learning_decisions(10, 3 * 1024 * 1024).unwrap();
    assert_eq!(
        learned
            .iter()
            .filter_map(|payload| payload.record.decision_id.as_deref())
            .collect::<Vec<_>>(),
        vec!["observation-decision", "committed-decision"]
    );

    let first = db
        .learning_decisions_after(None, 1, 3 * 1024 * 1024)
        .unwrap();
    assert_eq!(
        first.decisions[0].record.decision_id.as_deref(),
        Some("observation-decision")
    );
    assert_eq!(first.next_cursor, Some(observation_cursor));
    let second = db
        .learning_decisions_after(first.next_cursor, 1, 3 * 1024 * 1024)
        .unwrap();
    assert_eq!(
        second.decisions[0].record.decision_id.as_deref(),
        Some("committed-decision")
    );
    assert!(second.next_cursor.is_none());

    let erase = LearningErasePaths::new(root.path().join("brain"), Vec::new());
    db.forget_learning(&erase).unwrap();
    drop(db);
    let connection = open_for_constraints(&paths.brain_db());
    let identity_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM decision_identities WHERE decision_id = 'committed-decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let commit_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM permission_commits WHERE decision_id = 'committed-decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let payload_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM decision_payloads WHERE decision_id = 'committed-decision'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((identity_count, commit_count, payload_count), (1, 1, 0));
}

#[test]
fn decision_reads_reject_a_source_activity_with_another_decision_id() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let original = db
        .append_activity(decision_activity_event(
            "original",
            "observation-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    let unrelated = db
        .append_activity(decision_activity_event(
            "unrelated",
            "another-decision",
            2,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("observation-decision", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            original,
            complete_decision("observation-decision", AgentProvider::Codex),
        ),
    )
    .unwrap();
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE decision_payloads SET source_cursor = ?1 WHERE decision_id = ?2",
            params![unrelated.get() as i64, "observation-decision"],
        )
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();

    assert!(matches!(
        db.decision_payload("observation-decision"),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(matches!(
        db.learning_decisions(10, 1024 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn decision_reads_reject_malformed_records_and_typed_projection_disagreement() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "observation",
            "observation-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("observation-decision", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            cursor,
            complete_decision("observation-decision", AgentProvider::Codex),
        ),
    )
    .unwrap();
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE decision_payloads SET normalized_command = 'different' \
             WHERE decision_id = 'observation-decision'",
            [],
        )
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(matches!(
        db.decision_payload("observation-decision"),
        Err(StorageError::InvalidStorage(_))
    ));
    drop(db);

    let connection = open_for_constraints(&paths.brain_db());
    connection
        .execute(
            "UPDATE decision_payloads SET normalized_command = 'cargo test', decision_record = X'7b7d' \
             WHERE decision_id = 'observation-decision'",
            [],
        )
        .unwrap();
    drop(connection);
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(matches!(
        db.learning_decisions(10, 1024 * 1024),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn decision_reads_reuse_complete_activity_row_validation() {
    for corruption in ["unsupported-schema", "inconsistent-kind", "typed-column"] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let cursor = db
            .append_activity(decision_activity_event(
                "observation",
                "observation-decision",
                1,
                ActivityState::Observed,
                None,
            ))
            .unwrap();
        db.insert_decision(
            &DecisionIdentity::observation("observation-decision", AgentProvider::Codex, 1),
            &DecisionPayload::new(
                DecisionKind::Observation,
                cursor,
                complete_decision("observation-decision", AgentProvider::Codex),
            ),
        )
        .unwrap();
        drop(db);

        let connection = open_for_constraints(&paths.brain_db());
        match corruption {
            "typed-column" => {
                connection
                    .execute("UPDATE activity_events SET event_state = 'evaluating'", [])
                    .unwrap();
            }
            variant => {
                let bytes: Vec<u8> = connection
                    .query_row("SELECT event_payload FROM activity_events", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                let mut payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if variant == "unsupported-schema" {
                    payload["schema_version"] = serde_json::json!(ACTIVITY_SCHEMA_VERSION + 1);
                } else {
                    payload["kind"] = serde_json::json!("lifecycle");
                    connection
                        .execute("UPDATE activity_events SET event_kind = 'lifecycle'", [])
                        .unwrap();
                }
                connection
                    .execute(
                        "UPDATE activity_events SET event_payload = ?1",
                        [serde_json::to_vec(&payload).unwrap()],
                    )
                    .unwrap();
            }
        }
        drop(connection);
        let db = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(1)),
        )
        .unwrap();
        assert!(
            matches!(
                db.decision_payload("observation-decision"),
                Err(StorageError::InvalidStorage(_))
            ),
            "{corruption}"
        );
        assert!(
            matches!(
                db.learning_decisions(10, 1024 * 1024),
                Err(StorageError::InvalidStorage(_))
            ),
            "{corruption}"
        );
    }
}

#[test]
fn learning_row_lookahead_is_safe_at_the_sqlite_integer_boundary() {
    let Ok(max_rows) = usize::try_from(i64::MAX) else {
        return;
    };
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let db = BrainDb::create_current(&paths).unwrap();

    let page = db.learning_decisions_after(None, max_rows, 1024).unwrap();

    assert!(page.decisions.is_empty());
    assert!(page.next_cursor.is_none());
}

#[test]
fn learning_read_session_stabilizes_erasure_not_the_sqlite_snapshot() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut writer = BrainDb::create_current(&paths).unwrap();
    for index in 1..=2 {
        let decision_id = format!("observation-{index}");
        let cursor = writer
            .append_activity(decision_activity_event(
                &format!("activity-{index}"),
                &decision_id,
                index,
                ActivityState::Observed,
                None,
            ))
            .unwrap();
        writer
            .insert_decision(
                &DecisionIdentity::observation(decision_id.clone(), AgentProvider::Codex, index),
                &DecisionPayload::new(
                    DecisionKind::Observation,
                    cursor,
                    complete_decision(&decision_id, AgentProvider::Codex),
                ),
            )
            .unwrap();
    }
    drop(writer);

    let reader = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let session = reader.learning_read_session().unwrap();
    let first = session.page_after(None, 1, 2 * 1024 * 1024).unwrap();
    assert!(first.next_cursor.is_some());

    let mut concurrent_writer = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let cursor = concurrent_writer
        .append_activity(decision_activity_event(
            "activity-3",
            "observation-3",
            3,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    concurrent_writer
        .insert_decision(
            &DecisionIdentity::observation("observation-3", AgentProvider::Codex, 3),
            &DecisionPayload::new(
                DecisionKind::Observation,
                cursor,
                complete_decision("observation-3", AgentProvider::Codex),
            ),
        )
        .unwrap();
    drop(concurrent_writer);

    let mut eraser = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let erase = LearningErasePaths::new(root.path().join("brain"), Vec::new());
    assert!(matches!(
        eraser.forget_learning(&erase),
        Err(StorageError::Busy)
    ));
    assert!(eraser.erasure_state().unwrap().complete);

    let second = session
        .page_after(first.next_cursor, 1, 2 * 1024 * 1024)
        .unwrap();
    assert_eq!(second.decisions.len(), 1);
    let third = session
        .page_after(second.next_cursor, 1, 2 * 1024 * 1024)
        .unwrap();
    assert_eq!(third.decisions.len(), 1);
    assert_eq!(
        third.decisions[0].record.decision_id.as_deref(),
        Some("observation-3")
    );
    drop(session);
    eraser.forget_learning(&erase).unwrap();
}

#[test]
fn erasure_rejects_a_caller_supplied_non_derived_brain_root() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let wrong = root.path().join("other-brain");

    assert!(matches!(
        db.forget_learning(&LearningErasePaths::new(wrong.clone(), Vec::new())),
        Err(StorageError::InvalidStorage(_))
    ));
    assert!(!wrong.exists());
    assert!(db.erasure_state().unwrap().complete);
}

#[test]
fn writer_obeys_the_shared_erasure_gate_and_post_erasure_state() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "observation",
            "observation-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    let identity = DecisionIdentity::observation("observation-decision", AgentProvider::Codex, 1);
    let payload = DecisionPayload::new(
        DecisionKind::Observation,
        cursor,
        complete_decision("observation-decision", AgentProvider::Codex),
    );
    fs::create_dir(root.path().join("brain")).unwrap();
    fs::set_permissions(root.path().join("brain"), fs::Permissions::from_mode(0o700)).unwrap();
    let gate = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.path().join("brain/erasure.lock"))
        .unwrap();
    gate.set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    gate.try_lock_exclusive().unwrap();

    assert!(matches!(
        db.insert_decision(&identity, &payload),
        Err(StorageError::Busy)
    ));
    FileExt::unlock(&gate).unwrap();
    db.forget_learning(&LearningErasePaths::new(
        root.path().join("brain"),
        Vec::new(),
    ))
    .unwrap();
    db.insert_decision(&identity, &payload).unwrap();
    assert_eq!(db.learning_decisions(10, 1024 * 1024).unwrap().len(), 1);
}

#[test]
fn paused_decision_writer_process_helper() {
    let Some(root) = std::env::var_os("CODING_BRAIN_DECISION_WRITE_ROOT") else {
        return;
    };
    let paths = StoragePaths::at(std::path::Path::new(&root));
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(30)),
    )
    .unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "paused-writer",
            "paused-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("paused-decision", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            cursor,
            complete_decision("paused-decision", AgentProvider::Codex),
        ),
    )
    .unwrap();
}

#[test]
fn in_flight_writer_finishes_before_erasure_can_delete_and_complete() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let marker = root.path().join("writer-marker");
    let release = root.path().join("writer-release");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("paused_decision_writer_process_helper")
        .arg("--nocapture")
        .env("CODING_BRAIN_DECISION_WRITE_ROOT", root.path())
        .env("CODING_BRAIN_DECISION_WRITE_PAUSE", "before-commit")
        .env("CODING_BRAIN_DECISION_WRITE_MARKER", &marker)
        .env("CODING_BRAIN_DECISION_WRITE_RELEASE", &release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }

    let mut eraser = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    let erase = LearningErasePaths::new(root.path().join("brain"), Vec::new());
    assert!(matches!(
        eraser.forget_learning(&erase),
        Err(StorageError::Busy)
    ));
    assert!(eraser.erasure_state().unwrap().complete);
    fs::write(&release, b"continue").unwrap();
    assert!(child.wait().unwrap().success());

    assert_eq!(eraser.learning_decisions(10, 1024 * 1024).unwrap().len(), 1);
    eraser.forget_learning(&erase).unwrap();
    assert!(
        eraser
            .learning_decisions(10, 1024 * 1024)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn failed_later_erasure_gate_releases_earlier_legacy_decision_locks() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let brain_root = root.path().join("brain");
    let legacy = root.path().join("legacy");
    fs::create_dir(&brain_root).unwrap();
    fs::create_dir(&legacy).unwrap();
    for directory in [&brain_root, &legacy] {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let erasure = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(brain_root.join("erasure.lock"))
        .unwrap();
    erasure
        .set_permissions(fs::Permissions::from_mode(0o600))
        .unwrap();
    erasure.try_lock_exclusive().unwrap();

    assert!(matches!(
        db.forget_learning(&LearningErasePaths::new(
            brain_root.clone(),
            vec![legacy.clone()],
        )),
        Err(StorageError::Busy)
    ));
    let decisions = File::open(legacy.join("decisions.lock")).unwrap();
    decisions.try_lock_exclusive().unwrap();
}

#[test]
fn unsafe_managed_generation_entries_leave_erasure_incomplete_without_repair() {
    for attack in ["symlink", "hardlink", "broad-mode"] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let brain_root = root.path().join("brain");
        create_managed_dir(&brain_root);
        let generations = brain_root.join("preferences-generations");
        create_managed_dir(&generations);
        let outside = root.path().join("outside");
        create_managed_dir(&outside);
        let target = outside.join("retained");
        write_managed_file(&target, b"private");
        match attack {
            "symlink" => symlink(&target, generations.join("unsafe-entry")).unwrap(),
            "hardlink" => fs::hard_link(&target, generations.join("unsafe-entry")).unwrap(),
            "broad-mode" => {
                let entry = generations.join("unsafe-entry");
                write_managed_file(&entry, b"private");
                fs::set_permissions(entry, fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => unreachable!(),
        }

        assert!(
            matches!(
                db.forget_learning(&LearningErasePaths::new(brain_root, Vec::new())),
                Err(StorageError::InvalidStorage(_))
            ),
            "{attack}"
        );
        assert!(!db.erasure_state().unwrap().complete, "{attack}");
        assert_eq!(fs::read(&target).unwrap(), b"private", "{attack}");
    }
}

#[test]
fn erasure_rejects_symlink_hardlink_and_broad_lock_entries_without_repair() {
    for attack in ["symlink", "hardlink", "broad-mode"] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let brain_root = root.path().join("brain");
        create_managed_dir(&brain_root);
        let target = root.path().join("lock-target");
        write_managed_file(&target, b"retained");
        match attack {
            "symlink" => symlink(&target, brain_root.join("erasure.lock")).unwrap(),
            "hardlink" => fs::hard_link(&target, brain_root.join("erasure.lock")).unwrap(),
            "broad-mode" => {
                write_managed_file(&brain_root.join("erasure.lock"), b"");
                fs::set_permissions(
                    brain_root.join("erasure.lock"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }

        assert!(
            matches!(
                db.forget_learning(&LearningErasePaths::new(brain_root, Vec::new())),
                Err(StorageError::InvalidStorage(_))
            ),
            "{attack}"
        );
        assert!(db.erasure_state().unwrap().complete, "{attack}");
        if attack != "broad-mode" {
            assert_eq!(fs::read(&target).unwrap(), b"retained", "{attack}");
        }
    }
}

#[test]
fn erasure_rejects_symlink_or_broad_managed_roots_without_following_them() {
    for attack in ["brain-symlink", "legacy-broad"] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let brain_root = root.path().join("brain");
        let legacy = root.path().join("legacy");
        let outside = root.path().join("outside");
        create_managed_dir(&outside);
        write_managed_file(&outside.join("retained"), b"private");
        let legacy_sources = if attack == "brain-symlink" {
            symlink(&outside, &brain_root).unwrap();
            Vec::new()
        } else {
            create_managed_dir(&brain_root);
            create_managed_dir(&legacy);
            fs::set_permissions(&legacy, fs::Permissions::from_mode(0o755)).unwrap();
            vec![legacy]
        };

        assert!(
            matches!(
                db.forget_learning(&LearningErasePaths::new(brain_root, legacy_sources)),
                Err(StorageError::InvalidStorage(_))
            ),
            "{attack}"
        );
        assert!(db.erasure_state().unwrap().complete, "{attack}");
        assert_eq!(fs::read(outside.join("retained")).unwrap(), b"private");
    }
}

#[test]
fn forget_removes_payload_and_managed_learning_files_but_preserves_audit() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "observation",
            "observation-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("observation-decision", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            cursor,
            complete_decision("observation-decision", AgentProvider::Codex),
        ),
    )
    .unwrap();
    let brain_root = root.path().join("brain");
    let generation = brain_root.join("preferences-generations/gen-1");
    create_managed_dir(&generation);
    create_managed_dir(generation.parent().unwrap());
    write_managed_file(&generation.join("global.json"), b"preferences");
    write_managed_file(&brain_root.join("distill-watermark.json"), b"watermark");
    write_managed_file(&brain_root.join("distill-trigger"), b"trigger");
    let legacy = root.path().join("frozen-legacy");
    create_managed_dir(&legacy);
    for name in ["decisions.jsonl", "canonical.jsonl", "preferences.json"] {
        write_managed_file(&legacy.join(name), b"learning");
    }
    create_managed_dir(&legacy.join("preferences"));
    write_managed_file(&legacy.join("preferences/project.json"), b"learning");
    let erase = LearningErasePaths::new(brain_root.clone(), vec![legacy.clone()]);

    let generation = db.forget_learning(&erase).unwrap();

    assert_eq!(db.erasure_state().unwrap().generation, generation);
    assert!(db.erasure_state().unwrap().complete);
    assert!(
        db.decision_identity("observation-decision")
            .unwrap()
            .is_some()
    );
    assert!(
        db.decision_payload("observation-decision")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.activity_by_id("observation", None, 10, 65536)
            .unwrap()
            .events
            .len(),
        1
    );
    assert!(!brain_root.join("preferences-generations").exists());
    assert!(!brain_root.join("distill-watermark.json").exists());
    assert!(!brain_root.join("distill-trigger").exists());
    assert!(!legacy.join("decisions.jsonl").exists());
    assert!(!legacy.join("canonical.jsonl").exists());
    assert!(!legacy.join("preferences.json").exists());
    assert!(!legacy.join("preferences").exists());

    let next_generation = db.forget_learning(&erase).unwrap();
    assert_eq!(next_generation, generation + 1);
    assert!(db.erasure_state().unwrap().complete);
}

#[test]
fn erasure_locks_are_private_and_absent_legacy_roots_stay_absent() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let brain_root = root.path().join("brain");
    let existing = root.path().join("existing-legacy");
    let absent = root.path().join("absent-legacy");
    fs::create_dir(&existing).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o700)).unwrap();
    let erase = LearningErasePaths::new(brain_root.clone(), vec![absent.clone(), existing.clone()]);

    db.forget_learning(&erase).unwrap();

    assert!(!absent.exists());
    assert_eq!(
        fs::metadata(&brain_root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for lock in [
        existing.join("decisions.lock"),
        brain_root.join("erasure.lock"),
        brain_root.join("distill.lock"),
    ] {
        assert_eq!(
            fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn erasure_process_helper() {
    let Some(root) = std::env::var_os("CODING_BRAIN_ERASURE_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let paths = StoragePaths::at(&root);
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(30)),
    )
    .unwrap();
    let erase = LearningErasePaths::new(root.join("brain"), vec![root.join("legacy")]);
    db.forget_learning(&erase).unwrap();
}

fn spawn_releasable_erasure(root: &std::path::Path) -> (std::process::Child, std::path::PathBuf) {
    let marker = root.join("erasure-race-marker");
    let release = root.join("erasure-race-release");
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("erasure_process_helper")
        .arg("--nocapture")
        .env("CODING_BRAIN_ERASURE_TEST_ROOT", root)
        .env("CODING_BRAIN_ERASURE_TEST_PAUSE", "after-in-progress")
        .env("CODING_BRAIN_ERASURE_TEST_MARKER", &marker)
        .env("CODING_BRAIN_ERASURE_TEST_RELEASE", &release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !marker.exists() {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }
    (child, release)
}

#[test]
fn erasure_never_reopens_a_replaced_locked_legacy_root() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let legacy = root.path().join("legacy");
    let original = root.path().join("legacy-original");
    create_managed_dir(&legacy);
    write_managed_file(&legacy.join("decisions.jsonl"), b"original");

    let (mut child, release) = spawn_releasable_erasure(root.path());
    fs::rename(&legacy, &original).unwrap();
    create_managed_dir(&legacy);
    write_managed_file(&legacy.join("decisions.jsonl"), b"replacement");
    fs::write(release, b"continue").unwrap();

    assert!(!child.wait().unwrap().success());
    assert_eq!(
        fs::read(legacy.join("decisions.jsonl")).unwrap(),
        b"replacement"
    );
    assert!(!original.join("decisions.jsonl").exists());
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    assert!(!db.erasure_state().unwrap().complete);
}

#[test]
fn erasure_never_opens_a_supplied_legacy_root_that_appears_after_locking() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let legacy = root.path().join("legacy");

    let (mut child, release) = spawn_releasable_erasure(root.path());
    create_managed_dir(&legacy);
    write_managed_file(&legacy.join("decisions.jsonl"), b"new");
    fs::write(release, b"continue").unwrap();

    assert!(!child.wait().unwrap().success());
    assert_eq!(fs::read(legacy.join("decisions.jsonl")).unwrap(), b"new");
    let db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    assert!(!db.erasure_state().unwrap().complete);
}

#[test]
fn raw_wal_reader_keeps_erasure_incomplete_until_same_generation_resume() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let cursor = db
        .append_activity(decision_activity_event(
            "raw-reader",
            "raw-reader-decision",
            1,
            ActivityState::Observed,
            None,
        ))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("raw-reader-decision", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            cursor,
            complete_decision("raw-reader-decision", AgentProvider::Codex),
        ),
    )
    .unwrap();

    let raw = open_for_constraints(&paths.brain_db());
    raw.execute_batch("BEGIN").unwrap();
    assert_eq!(
        raw.query_row("SELECT COUNT(*) FROM decision_payloads", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    let erase = LearningErasePaths::new(root.path().join("brain"), Vec::new());
    assert!(matches!(
        db.forget_learning(&erase),
        Err(StorageError::Busy)
    ));
    let interrupted = db.erasure_state().unwrap();
    assert!(!interrupted.complete);
    assert!(matches!(
        db.learning_decisions(10, 1024 * 1024),
        Err(StorageError::MigrationRequired)
    ));

    raw.execute_batch("ROLLBACK").unwrap();
    drop(raw);
    drop(db);
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    assert_eq!(
        db.resume_forget_learning(&erase).unwrap(),
        interrupted.generation
    );
    assert_eq!(
        db.erasure_state().unwrap().generation,
        interrupted.generation
    );
    assert!(db.erasure_state().unwrap().complete);
}

#[test]
fn interrupted_erasure_fails_closed_and_resumes_at_the_same_generation() {
    for stage in [
        "after-in-progress",
        "after-database-delete",
        "after-external-delete",
        "after-generation-delete",
        "before-wal-truncate",
        "after-wal-truncate",
        "before-complete",
        "after-complete",
    ] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let cursor = db
            .append_activity(decision_activity_event(
                "process-observation",
                "process-decision",
                1,
                ActivityState::Observed,
                None,
            ))
            .unwrap();
        db.insert_decision(
            &DecisionIdentity::observation("process-decision", AgentProvider::Codex, 1),
            &DecisionPayload::new(
                DecisionKind::Observation,
                cursor,
                complete_decision("process-decision", AgentProvider::Codex),
            ),
        )
        .unwrap();
        drop(db);
        let brain_root = root.path().join("brain");
        let legacy = root.path().join("legacy");
        create_managed_dir(&brain_root.join("preferences-generations"));
        create_managed_dir(&brain_root.join("preferences-generations/gen-1"));
        write_managed_file(
            &brain_root.join("preferences-generations/gen-1/global.json"),
            b"preferences",
        );
        write_managed_file(&brain_root.join("distill-watermark.json"), b"watermark");
        create_managed_dir(&legacy);
        write_managed_file(&legacy.join("decisions.jsonl"), b"learning");
        write_managed_file(&legacy.join("canonical.jsonl"), b"learning");
        let marker = root.path().join("erasure-marker");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("erasure_process_helper")
            .arg("--nocapture")
            .env("CODING_BRAIN_ERASURE_TEST_ROOT", root.path())
            .env("CODING_BRAIN_ERASURE_TEST_PAUSE", stage)
            .env("CODING_BRAIN_ERASURE_TEST_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !marker.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "erasure child did not reach {stage}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();

        let mut db = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(2)),
        )
        .unwrap();
        let interrupted = db.erasure_state().unwrap();
        if stage == "after-complete" {
            assert!(interrupted.complete, "{stage}");
        } else {
            assert!(!interrupted.complete, "{stage}");
            assert!(matches!(
                db.learning_decisions(10, 1024 * 1024),
                Err(StorageError::MigrationRequired)
            ));
        }
        let erase = LearningErasePaths::new(brain_root.clone(), vec![legacy.clone()]);
        let generation = if stage == "after-in-progress" {
            db.forget_learning(&erase).unwrap()
        } else {
            db.resume_forget_learning(&erase).unwrap()
        };
        assert_eq!(generation, interrupted.generation, "{stage}");
        assert!(db.erasure_state().unwrap().complete, "{stage}");
        assert_eq!(
            db.resume_forget_learning(&erase).unwrap(),
            generation,
            "resume of complete generation must be a no-op at {stage}"
        );
        assert!(db.decision_payload("process-decision").unwrap().is_none());
        assert!(!legacy.join("decisions.jsonl").exists());
        assert!(!legacy.join("canonical.jsonl").exists());
        assert!(!brain_root.join("preferences-generations").exists());
        assert!(!brain_root.join("distill-watermark.json").exists());
    }
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
    connection
        .execute(
            "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
            event_payload
         ) VALUES (2, 'activity-1', 'diagnostic', 'observed', 1, X'')",
            [],
        )
        .unwrap();
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
                decision_id, identity_kind, provider, session_id, turn_id, tool_use_id,
                authority_action, decision_source, decided_at_ms
             ) VALUES ('decision-1', 'permission', 'codex', 'session-1', 'turn-1', 'tool-1',
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
        "UPDATE review_meta
         SET revision = 1, last_archive_revision = 1
         WHERE surface = 'recent'",
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

#[test]
fn review_schema_remembers_only_the_latest_undoable_archive_revision() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.review_db());

    assert_eq!(
        connection
            .query_row(
                "SELECT last_archive_revision FROM review_meta WHERE surface = 'review'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap(),
        None
    );
    connection
        .execute(
            "UPDATE review_meta
             SET revision = 1, last_archive_revision = 1
             WHERE surface = 'review'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO review_marks (
                surface, group_id, source_cursor, disposition, revision
             ) VALUES ('review', 'older', 1, 'archived', 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE review_meta
             SET revision = 2, last_archive_revision = 2
             WHERE surface = 'review'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO review_marks (
                surface, group_id, source_cursor, disposition, revision
             ) VALUES ('review', 'latest', 2, 'archived', 2)",
            [],
        )
        .unwrap();

    connection
        .execute(
            "UPDATE review_meta SET last_archive_revision = NULL WHERE surface = 'review'",
            [],
        )
        .unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT last_archive_revision FROM review_meta WHERE surface = 'review'",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .unwrap(),
        None
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM review_marks
                 WHERE surface = 'review' AND disposition = 'archived'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2,
        "clearing the latest undo slot must not promote an older archive batch"
    );
}

#[test]
fn review_db_round_trips_one_surface_without_changing_other_revisions() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let attention = review_eligibility(ReviewSurface::Attention, 2, &[("attention-a", 1)]);
    let diagnostics = review_eligibility(ReviewSurface::Diagnostics, 2, &[("diagnostic-a", 2)]);
    let attention_key = ReviewKey::derive(ReviewSurface::Attention, b"attention-a");

    let result = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::SetDisposition {
                    keys: [attention_key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            },
            &attention,
        )
        .unwrap();
    assert_eq!(result.surface_revision, 1);
    assert_eq!(result.reviewed_count, 1);

    let reopened = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    let attention_state = reopened.read_surface(&attention).unwrap();
    let diagnostics_state = reopened.read_surface(&diagnostics).unwrap();
    assert_eq!(attention_state.surface_revision(), 1);
    assert_eq!(
        attention_state.disposition(&attention_key),
        Some(ReviewDisposition::Reviewed)
    );
    assert_eq!(diagnostics_state.surface_revision(), 0);
    assert_eq!(diagnostics_state.reviewed_count(), 0);
}

#[test]
fn review_reset_recovers_corruption_without_changing_brain() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    brain
        .append_activity(activity_event("brain-authority", 1, ActivityState::Denied))
        .unwrap();
    drop(brain);
    drop(ReviewDb::create_current(&paths).unwrap());
    let brain_before = fs::read(paths.brain_db()).unwrap();
    write_managed_file(&paths.review_db(), b"not sqlite");

    ReviewDb::reset(&paths).unwrap();

    assert_eq!(fs::read(paths.brain_db()).unwrap(), brain_before);
    assert_eq!(
        fs::metadata(paths.review_db())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let review = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    let state = review
        .read_surface(&review_eligibility(ReviewSurface::Attention, 1, &[]))
        .unwrap();
    assert_eq!(state.surface_revision(), 0);
    let brain = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    assert_eq!(
        brain
            .activity_by_id("brain-authority", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        1
    );
}

#[test]
fn review_evidence_preserves_canonical_and_opaque_legacy_keys() {
    let canonical = ReviewKey::derive(ReviewSurface::Attention, b"activity-1");
    let legacy = ReviewKey::derive(ReviewSurface::Review, &[0xff; 32]);
    let occurrences = [canonical, legacy]
        .into_iter()
        .enumerate()
        .map(|(index, key)| {
            ReviewEligibleOccurrence::new(
                if index == 0 {
                    ReviewSurface::Attention
                } else {
                    ReviewSurface::Review
                },
                key,
                ActivityCursor::try_from(index as u64 + 1).unwrap(),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(occurrences[0].group_id(), canonical.to_string());
    assert_eq!(occurrences[1].group_id(), legacy.to_string());
    assert_ne!(
        ReviewKey::derive(ReviewSurface::Review, occurrences[1].group_id().as_bytes()),
        legacy,
        "the stable SQL identity must preserve rather than re-derive the existing key"
    );
    ReviewEligibility::try_new(
        ReviewSurface::Review,
        Some(ActivityCursor::try_from(2_u64).unwrap()),
        vec![occurrences[1].clone()],
    )
    .unwrap();
}

#[test]
fn review_evidence_rejects_cross_surface_keys_before_sql() {
    let occurrence = ReviewEligibleOccurrence::new(
        ReviewSurface::Recent,
        ReviewKey::derive(ReviewSurface::Recent, b"activity-1"),
        ActivityCursor::try_from(1_u64).unwrap(),
    );

    assert!(matches!(
        ReviewEligibility::try_new(
            ReviewSurface::Attention,
            Some(ActivityCursor::try_from(1_u64).unwrap()),
            vec![occurrence],
        ),
        Err(StorageError::InvalidStorage(
            "review occurrence and evidence surfaces disagree"
        ))
    ));
}

#[test]
fn review_revision_at_sqlite_limit_fails_without_wrapping() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.review_db());
    connection
        .execute(
            "UPDATE review_meta SET revision = ?1 WHERE surface = 'attention'",
            [i64::MAX],
        )
        .unwrap();
    drop(connection);
    let mut db = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: i64::MAX as u64,
                operation: ReviewMutation::UndoLastArchive { expected_count: 0 },
            },
            &review_eligibility(ReviewSurface::Attention, 0, &[]),
        ),
        Err(StorageError::ReviewRevisionOverflow)
    ));
    assert_eq!(
        open_for_constraints(&paths.review_db())
            .query_row(
                "SELECT revision FROM review_meta WHERE surface = 'attention'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        i64::MAX
    );
}

#[test]
fn review_recent_rejects_archive_metadata_even_without_marks() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.review_db());
    connection
        .execute_batch("PRAGMA ignore_check_constraints = ON;")
        .unwrap();
    connection
        .execute(
            "UPDATE review_meta
             SET revision = 1, last_archive_revision = 1
             WHERE surface = 'recent'",
            [],
        )
        .unwrap();
    drop(connection);
    let db = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();

    assert!(matches!(
        db.read_surface(&review_eligibility(ReviewSurface::Recent, 0, &[])),
        Err(StorageError::InvalidStorage(
            "Recent contains archive metadata"
        ))
    ));
}

#[test]
fn review_newer_cursor_resurfaces_the_same_key() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let key = ReviewKey::derive(ReviewSurface::Attention, b"shared-activity");
    let first = ReviewEligibility::try_new(
        ReviewSurface::Attention,
        Some(ActivityCursor::try_from(1_u64).unwrap()),
        vec![ReviewEligibleOccurrence::new(
            ReviewSurface::Attention,
            key,
            ActivityCursor::try_from(1_u64).unwrap(),
        )],
    )
    .unwrap();
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &first,
    )
    .unwrap();
    let newer = ReviewEligibility::try_new(
        ReviewSurface::Attention,
        Some(ActivityCursor::try_from(2_u64).unwrap()),
        vec![ReviewEligibleOccurrence::new(
            ReviewSurface::Attention,
            key,
            ActivityCursor::try_from(2_u64).unwrap(),
        )],
    )
    .unwrap();

    assert_eq!(db.read_surface(&newer).unwrap().disposition(&key), None);
    let result = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 1,
                operation: ReviewMutation::SetDisposition {
                    keys: [key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            },
            &newer,
        )
        .unwrap();
    assert_eq!(result.surface_revision, 2);
    assert_eq!(result.reviewed_count, 1);
}

#[test]
fn review_only_the_latest_archive_batch_is_undoable() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let evidence = review_eligibility(
        ReviewSurface::Diagnostics,
        2,
        &[("diagnostic-a", 1), ("diagnostic-b", 2)],
    );
    let a = ReviewKey::derive(ReviewSurface::Diagnostics, b"diagnostic-a");
    let b = ReviewKey::derive(ReviewSurface::Diagnostics, b"diagnostic-b");
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Diagnostics,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [a, b].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &evidence,
    )
    .unwrap();
    for (revision, key) in [(1, a), (2, b)] {
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Diagnostics,
                expected_surface_revision: revision,
                operation: ReviewMutation::SetDisposition {
                    keys: [key].into_iter().collect(),
                    disposition: ReviewDisposition::Archived,
                },
            },
            &evidence,
        )
        .unwrap();
    }

    let result = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Diagnostics,
                expected_surface_revision: 3,
                operation: ReviewMutation::UndoLastArchive { expected_count: 1 },
            },
            &evidence,
        )
        .unwrap();
    assert_eq!(result.archived_count, 1);
    assert_eq!(result.reviewed_count, 1);
    assert_eq!(result.last_archive_count, 0);
    let state = db.read_surface(&evidence).unwrap();
    assert_eq!(state.disposition(&a), Some(ReviewDisposition::Archived));
    assert_eq!(state.disposition(&b), Some(ReviewDisposition::Reviewed));
    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Diagnostics,
                expected_surface_revision: 4,
                operation: ReviewMutation::UndoLastArchive { expected_count: 1 },
            },
            &evidence,
        ),
        Err(StorageError::ReviewCountMismatch)
    ));
    let unchanged = db.read_surface(&evidence).unwrap();
    assert_eq!(unchanged.surface_revision(), 4);
    assert_eq!(unchanged.disposition(&a), Some(ReviewDisposition::Archived));
}

#[test]
fn review_archive_all_and_second_archive_undo_cycle_match_pure_rules() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let evidence = review_eligibility(
        ReviewSurface::Review,
        2,
        &[("review-a", 1), ("review-b", 2)],
    );
    let a = ReviewKey::derive(ReviewSurface::Review, b"review-a");
    let b = ReviewKey::derive(ReviewSurface::Review, b"review-b");
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Review,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [a, b].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &evidence,
    )
    .unwrap();
    let archived = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Review,
                expected_surface_revision: 1,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 2 },
            },
            &evidence,
        )
        .unwrap();
    assert_eq!(
        (archived.archived_count, archived.last_archive_count),
        (2, 2)
    );
    let first_undo = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Review,
                expected_surface_revision: 2,
                operation: ReviewMutation::UndoLastArchive { expected_count: 2 },
            },
            &evidence,
        )
        .unwrap();
    assert_eq!(
        (first_undo.reviewed_count, first_undo.last_archive_count),
        (2, 0)
    );
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Review,
            expected_surface_revision: 3,
            operation: ReviewMutation::SetDisposition {
                keys: [a].into_iter().collect(),
                disposition: ReviewDisposition::Archived,
            },
        },
        &evidence,
    )
    .unwrap();
    let second_undo = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Review,
                expected_surface_revision: 4,
                operation: ReviewMutation::UndoLastArchive { expected_count: 1 },
            },
            &evidence,
        )
        .unwrap();
    assert_eq!(second_undo.surface_revision, 5);
    assert_eq!(
        (second_undo.reviewed_count, second_undo.last_archive_count),
        (2, 0)
    );
}

#[test]
fn review_prunes_missing_exact_occurrences_and_never_decreases_high_water() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let first = review_eligibility(ReviewSurface::Review, 2, &[("review-a", 1)]);
    let key = ReviewKey::derive(ReviewSurface::Review, b"review-a");
    let diagnostics = review_eligibility(ReviewSurface::Diagnostics, 1, &[("diagnostic-a", 1)]);
    let diagnostic_key = ReviewKey::derive(ReviewSurface::Diagnostics, b"diagnostic-a");
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Diagnostics,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [diagnostic_key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &diagnostics,
    )
    .unwrap();
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Review,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &first,
    )
    .unwrap();
    let missing = review_eligibility(ReviewSurface::Review, 3, &[]);
    let result = db
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Review,
                expected_surface_revision: 1,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 0 },
            },
            &missing,
        )
        .unwrap();
    assert_eq!(result.reviewed_count, 0);
    assert_eq!(result.archived_count, 0);
    assert_eq!(db.read_surface(&missing).unwrap().source_high_water(), 3);
    assert!(matches!(
        db.read_surface(&review_eligibility(ReviewSurface::Review, 2, &[])),
        Err(StorageError::InvalidStorage(
            "review evidence source high-water decreased"
        ))
    ));
    let diagnostics_state = db.read_surface(&diagnostics).unwrap();
    assert_eq!(diagnostics_state.surface_revision(), 1);
    assert_eq!(
        diagnostics_state.disposition(&diagnostic_key),
        Some(ReviewDisposition::Reviewed)
    );
}

#[test]
fn review_busy_deadline_does_not_change_brain_or_review() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    brain
        .append_activity(activity_event("brain-busy", 1, ActivityState::Denied))
        .unwrap();
    drop(brain);
    drop(ReviewDb::create_current(&paths).unwrap());
    let blocker = open_for_constraints(&paths.review_db());
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let mut review = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(20)),
    )
    .unwrap();

    assert!(matches!(
        review.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::UndoLastArchive { expected_count: 0 },
            },
            &review_eligibility(ReviewSurface::Attention, 0, &[]),
        ),
        Err(StorageError::Busy)
    ));
    let mut brain = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    brain
        .append_activity(activity_event("brain-busy", 2, ActivityState::Observed))
        .unwrap();
    blocker.execute_batch("ROLLBACK;").unwrap();
    assert_eq!(
        open_for_constraints(&paths.review_db())
            .query_row(
                "SELECT revision FROM review_meta WHERE surface = 'attention'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert_eq!(
        brain
            .activity_by_id("brain-busy", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        2
    );
}

#[test]
fn review_adapter_rejects_stale_count_conflict_and_recent_archive() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut db = ReviewDb::create_current(&paths).unwrap();
    let evidence = review_eligibility(ReviewSurface::Attention, 1, &[("attention-a", 1)]);
    let key = ReviewKey::derive(ReviewSurface::Attention, b"attention-a");
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &evidence,
    )
    .unwrap();

    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 1 },
            },
            &evidence,
        ),
        Err(StorageError::StaleReviewRevision)
    ));
    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 1,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 2 },
            },
            &evidence,
        ),
        Err(StorageError::ReviewCountMismatch)
    ));
    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 1,
                operation: ReviewMutation::SetDisposition {
                    keys: [key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            },
            &evidence,
        ),
        Err(StorageError::ReviewDispositionConflict)
    ));
    assert_eq!(db.read_surface(&evidence).unwrap().surface_revision(), 1);
    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Diagnostics,
                expected_surface_revision: 0,
                operation: ReviewMutation::UndoLastArchive { expected_count: 0 },
            },
            &evidence,
        ),
        Err(StorageError::InvalidStorage(
            "review request and evidence surfaces disagree"
        ))
    ));

    let recent = review_eligibility(ReviewSurface::Recent, 1, &[("recent-a", 1)]);
    let recent_key = ReviewKey::derive(ReviewSurface::Recent, b"recent-a");
    db.mutate(
        &ReviewMutationRequest {
            surface: ReviewSurface::Recent,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [recent_key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        },
        &recent,
    )
    .unwrap();
    assert!(matches!(
        db.mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Recent,
                expected_surface_revision: 1,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 1 },
            },
            &recent,
        ),
        Err(StorageError::InvalidReviewRequest(
            ReviewRequestError::UnsupportedOperation
        ))
    ));
}

#[test]
fn corrupt_review_open_fails_while_brain_remains_readable() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    brain
        .append_activity(activity_event(
            "brain-corrupt-review",
            1,
            ActivityState::Denied,
        ))
        .unwrap();
    drop(brain);
    drop(ReviewDb::create_current(&paths).unwrap());
    write_managed_file(&paths.review_db(), b"not sqlite");

    assert!(
        ReviewDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_millis(250)),
        )
        .is_err()
    );
    assert_eq!(
        BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_millis(250)),
        )
        .unwrap()
        .activity_by_id("brain-corrupt-review", None, 10, 64 * 1024)
        .unwrap()
        .events
        .len(),
        1
    );
}

#[test]
fn review_mutation_uses_captured_cursor_when_brain_advances() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    let first_cursor = brain
        .append_activity(activity_event(
            "racing-activity",
            1,
            ActivityState::Observed,
        ))
        .unwrap();
    let key = ReviewKey::derive(ReviewSurface::Attention, b"racing-activity");
    let captured = ReviewEligibility::try_new(
        ReviewSurface::Attention,
        Some(first_cursor),
        vec![ReviewEligibleOccurrence::new(
            ReviewSurface::Attention,
            key,
            first_cursor,
        )],
    )
    .unwrap();
    let newer_cursor = brain
        .append_activity(activity_event("racing-activity", 2, ActivityState::Denied))
        .unwrap();
    let brain_after_append = fs::read(paths.brain_db()).unwrap();
    let mut review = ReviewDb::create_current(&paths).unwrap();

    review
        .mutate(
            &ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::SetDisposition {
                    keys: [key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            },
            &captured,
        )
        .unwrap();
    assert_eq!(fs::read(paths.brain_db()).unwrap(), brain_after_append);
    assert_eq!(brain.activity_high_water().unwrap(), Some(newer_cursor));
    assert_eq!(
        review.read_surface(&captured).unwrap().source_high_water(),
        1
    );

    let refreshed = ReviewEligibility::try_new(
        ReviewSurface::Attention,
        Some(newer_cursor),
        vec![ReviewEligibleOccurrence::new(
            ReviewSurface::Attention,
            key,
            newer_cursor,
        )],
    )
    .unwrap();
    assert_eq!(
        review.read_surface(&refreshed).unwrap().disposition(&key),
        None
    );
}

#[test]
fn review_evidence_capacity_is_rejected_before_sql() {
    let occurrences = (0..=MAX_REVIEW_KEYS)
        .map(|index| {
            let cursor = ActivityCursor::try_from(index as u64 + 1).unwrap();
            ReviewEligibleOccurrence::new(
                ReviewSurface::Attention,
                ReviewKey::derive(ReviewSurface::Attention, &index.to_le_bytes()),
                cursor,
            )
        })
        .collect();

    assert!(matches!(
        ReviewEligibility::try_new(
            ReviewSurface::Attention,
            Some(ActivityCursor::try_from(MAX_REVIEW_KEYS as u64 + 1).unwrap()),
            occurrences,
        ),
        Err(StorageError::ReviewCapacityExceeded)
    ));
}

#[test]
fn review_evidence_rejects_duplicate_keys_and_cursors_above_high_water() {
    let first_cursor = ActivityCursor::try_from(1_u64).unwrap();
    let second_cursor = ActivityCursor::try_from(2_u64).unwrap();
    let key = ReviewKey::derive(ReviewSurface::Attention, b"duplicate");
    let occurrence = ReviewEligibleOccurrence::new(ReviewSurface::Attention, key, first_cursor);

    assert!(matches!(
        ReviewEligibility::try_new(
            ReviewSurface::Attention,
            Some(first_cursor),
            vec![occurrence.clone(), occurrence],
        ),
        Err(StorageError::InvalidStorage(
            "review evidence contains duplicate keys"
        ))
    ));
    assert!(matches!(
        ReviewEligibility::try_new(
            ReviewSurface::Attention,
            Some(first_cursor),
            vec![ReviewEligibleOccurrence::new(
                ReviewSurface::Attention,
                ReviewKey::derive(ReviewSurface::Attention, b"too-new"),
                second_cursor,
            )],
        ),
        Err(StorageError::InvalidStorage(
            "review cursor exceeds its source high-water"
        ))
    ));
}

#[test]
fn review_surface_lookup_uses_the_bounded_primary_index() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let connection = open_for_constraints(&paths.review_db());
    let detail = connection
        .prepare(
            "EXPLAIN QUERY PLAN
             SELECT group_id, source_cursor, disposition, revision
             FROM review_marks INDEXED BY sqlite_autoindex_review_marks_1
             WHERE surface = ?1
             ORDER BY group_id, source_cursor
             LIMIT ?2",
        )
        .unwrap()
        .query_map(params!["attention", MAX_REVIEW_KEYS as i64 + 1], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");

    assert!(
        detail.contains("sqlite_autoindex_review_marks_1"),
        "{detail}"
    );
    assert!(detail.contains("surface=?"), "{detail}");
}

#[test]
fn review_reset_rejects_unsafe_sidecars_and_path_substitution() {
    for attack in [
        "sidecar-symlink",
        "sidecar-mode",
        "sidecar-hardlink",
        "gate-symlink",
        "gate-mode",
        "gate-hardlink",
        "database-symlink",
        "database-hardlink",
    ] {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        drop(ReviewDb::create_current(&paths).unwrap());
        let brain_before = fs::read(paths.brain_db()).unwrap();
        let outside = root.path().join("outside");
        write_managed_file(&outside, b"outside");
        match attack {
            "sidecar-symlink" => {
                symlink(&outside, paths.db_dir().join("review.sqlite3-wal")).unwrap();
            }
            "sidecar-mode" => {
                let sidecar = paths.db_dir().join("review.sqlite3-journal");
                fs::write(&sidecar, b"unsafe").unwrap();
                fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
            }
            "sidecar-hardlink" => {
                fs::hard_link(&outside, paths.db_dir().join("review.sqlite3-journal")).unwrap();
            }
            "gate-symlink" => {
                fs::remove_file(paths.db_dir().join("review-reset.lock")).unwrap();
                symlink(&outside, paths.db_dir().join("review-reset.lock")).unwrap();
            }
            "gate-mode" => {
                fs::set_permissions(
                    paths.db_dir().join("review-reset.lock"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap();
            }
            "gate-hardlink" => {
                fs::remove_file(&outside).unwrap();
                fs::hard_link(paths.db_dir().join("review-reset.lock"), &outside).unwrap();
            }
            "database-symlink" => {
                fs::remove_file(paths.review_db()).unwrap();
                symlink(&outside, paths.review_db()).unwrap();
            }
            "database-hardlink" => {
                fs::remove_file(&outside).unwrap();
                fs::hard_link(paths.review_db(), &outside).unwrap();
            }
            _ => unreachable!(),
        }
        let outside_before = fs::read(&outside).unwrap();

        assert!(matches!(
            ReviewDb::reset(&paths),
            Err(StorageError::InvalidStorage(_))
        ));
        assert_eq!(fs::read(&outside).unwrap(), outside_before);
        assert_eq!(fs::read(paths.brain_db()).unwrap(), brain_before);
    }
}

const REVIEW_GATE_CHILD_ROOT: &str = "CODING_BRAIN_REVIEW_GATE_CHILD_ROOT";

#[test]
#[ignore = "subprocess helper"]
fn review_reset_gate_child() {
    let Some(root) = std::env::var_os(REVIEW_GATE_CHILD_ROOT) else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let paths = StoragePaths::at(&root);
    let _review = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(5)),
    )
    .unwrap();
    fs::write(root.join("review-gate-ready"), b"ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("review-gate-release").exists() {
        assert!(
            Instant::now() < deadline,
            "parent did not release gate child"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn review_reset_is_busy_while_another_process_holds_a_connection() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    drop(ReviewDb::create_current(&paths).unwrap());
    let inode_before = fs::metadata(paths.review_db()).unwrap().ino();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "review_reset_gate_child",
            "--ignored",
            "--nocapture",
        ])
        .env(REVIEW_GATE_CHILD_ROOT, root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(5);
    while !root.path().join("review-gate-ready").exists() {
        assert!(
            Instant::now() < ready_deadline,
            "review gate child did not become ready"
        );
        thread::sleep(Duration::from_millis(10));
    }

    let reset = ReviewDb::reset(&paths);
    let inode_after = fs::metadata(paths.review_db()).unwrap().ino();
    fs::write(root.path().join("review-gate-release"), b"release").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "gate child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(matches!(reset, Err(StorageError::Busy)));
    assert_eq!(
        inode_after, inode_before,
        "Busy reset replaced the main inode"
    );
    ReviewDb::reset(&paths).unwrap();
}

#[test]
fn review_reset_is_busy_while_a_local_connection_is_alive() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let review = ReviewDb::create_current(&paths).unwrap();
    let inode_before = fs::metadata(paths.review_db()).unwrap().ino();

    assert!(matches!(ReviewDb::reset(&paths), Err(StorageError::Busy)));
    assert_eq!(fs::metadata(paths.review_db()).unwrap().ino(), inode_before);
    assert_eq!(review.user_version().unwrap(), REVIEW_SCHEMA_VERSION);
    drop(review);
    ReviewDb::reset(&paths).unwrap();
}
