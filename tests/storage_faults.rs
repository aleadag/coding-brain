use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use coding_brain::brain::decisions::{DecisionRecord, DecisionType};
use coding_brain::brain::storage::{
    BrainDb, DecisionIdentity, DecisionKind, DecisionPayload, OpenRole, StorageDeadline,
    StorageError, StorageFaultCategory, StorageOperation, StoragePaths, WAL_HARD_LIMIT_BYTES,
};
use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
    CorrectionDisposition, ProjectEvidence,
};
use coding_brain_core::lifecycle::{
    LifecycleEvent, LifecycleEventKind, LifecycleIdentity, SessionStartSource,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};

fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn event(id: &str, state: ActivityState) -> ActivityEvent {
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: id.into(),
        recorded_at_ms: 1,
        project: ProjectEvidence {
            project_id: ProjectId::Temporary("maintenance-project".into()),
            cwd: "/work/maintenance".into(),
            label: None,
        },
        session: None,
        state,
        tool: Some("Bash".into()),
        normalized_command: Some("printf safe".into()),
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: None,
        decision_id: Some(format!("decision-{id}")),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

fn decision(id: &str) -> DecisionRecord {
    DecisionRecord {
        provider: AgentProvider::Codex,
        timestamp: "2026-08-06T00:00:00Z".into(),
        pid: 1,
        project: "maintenance-project".into(),
        tool: Some("Bash".into()),
        command: Some("printf safe".into()),
        brain_action: "abstain".into(),
        brain_confidence: 0.5,
        brain_reasoning: "bounded".into(),
        user_action: "observed".into(),
        context: None,
        outcome: None,
        decision_type: DecisionType::Session,
        suggested_at: Some(1),
        resolved_at: Some(1),
        override_reason: None,
        decision_id: Some(id.into()),
        brain_decision_ms: None,
        cache_hit: None,
        canonical: None,
    }
}

#[test]
fn hard_wal_limit_pauses_model_inference_but_not_deterministic_deny() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let db = BrainDb::create_current(&paths).unwrap();
    let wal = paths.brain_db().with_extension("sqlite3-wal");
    fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&wal)
        .unwrap()
        .set_len(WAL_HARD_LIMIT_BYTES + 1)
        .unwrap();
    fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).unwrap();

    assert!(matches!(
        db.admit_model_attempt(),
        Err(StorageError::MaintenanceRequired)
    ));

    assert_eq!(
        db.admit_deterministic_safety_deny().unwrap(),
        coding_brain_core::lifecycle::PermissionAction::Deny
    );
}

#[test]
fn hook_role_cannot_run_manual_maintenance_or_integrity_checks() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let mut hook = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();

    assert!(matches!(
        hook.maintain_bounded(None, StorageDeadline::after(Duration::from_secs(1))),
        Err(StorageError::HookMaintenanceForbidden)
    ));
    assert!(matches!(
        hook.deep_integrity_check(StorageDeadline::after(Duration::from_secs(1))),
        Err(StorageError::HookMaintenanceForbidden)
    ));
}

#[test]
fn model_admission_honors_the_stored_hook_deadline() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    let db = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    assert!(matches!(db.admit_model_attempt(), Err(StorageError::Busy)));
}

#[test]
fn bounded_retention_preserves_live_and_authoritative_state() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();

    let mut disposable = event("disposable", ActivityState::Error);
    disposable.kind = ActivityKind::Diagnostic;
    disposable.decision_id = None;
    db.append_activity(disposable).unwrap();
    let mut incomplete = event("incomplete", ActivityState::Observed);
    incomplete.recorded_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    db.append_activity(incomplete).unwrap();
    let mut outcome = event("outcome", ActivityState::Outcome);
    outcome.outcome = Some(ActivityOutcome::Failed);
    db.append_activity(outcome).unwrap();
    let mut correction = event("correction", ActivityState::Correction);
    correction.correction = Some(CorrectionDisposition::BrainWrong);
    db.append_activity(correction).unwrap();
    let authority_cursor = db
        .append_activity(event("authority", ActivityState::Error))
        .unwrap();
    db.insert_decision(
        &DecisionIdentity::observation("decision-authority", AgentProvider::Codex, 1),
        &DecisionPayload::new(
            DecisionKind::Observation,
            authority_cursor,
            decision("decision-authority"),
        ),
    )
    .unwrap();
    for index in 0..40 {
        db.append_activity(event(
            &format!("interrupted-{index}"),
            ActivityState::Observed,
        ))
        .unwrap();
        db.append_activity(event(
            &format!("interrupted-{index}"),
            ActivityState::Interrupted,
        ))
        .unwrap();
    }
    let mut after_bound = event("after-bound", ActivityState::Error);
    after_bound.kind = ActivityKind::Diagnostic;
    after_bound.decision_id = None;
    let bound = db.append_activity(after_bound).unwrap();

    let lifecycle = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "maintenance-session".into(),
        None,
        None,
        "/work/maintenance".into(),
    )
    .unwrap();
    db.record_lifecycle(
        LifecycleEvent::from_parts(
            lifecycle,
            LifecycleEventKind::SessionStart {
                source: SessionStartSource::Startup,
            },
        )
        .unwrap(),
        1,
    )
    .unwrap();

    let outcome = db
        .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
        .unwrap();

    assert_eq!(outcome.deleted_activity_rows, 17);
    assert!(
        db.activity_by_id("disposable", None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty()
    );
    for retained in [
        "incomplete",
        "outcome",
        "correction",
        "authority",
        "after-bound",
    ] {
        assert_eq!(
            db.activity_by_id(retained, None, 10, 64 * 1024)
                .unwrap()
                .events
                .len(),
            1,
            "{retained}"
        );
    }
    for index in 0..40 {
        let retained = db
            .activity_by_id(&format!("interrupted-{index}"), None, 10, 64 * 1024)
            .unwrap()
            .events
            .len();
        assert_eq!(
            retained,
            if index >= 8 { 2 } else { 0 },
            "interrupted-{index}"
        );
    }
    assert!(db.read_lifecycle().unwrap().sessions.contains_key(
        &AgentSessionKey::native(AgentProvider::Codex, "maintenance-session").storage_key()
    ));
}

#[test]
fn bounded_retention_advances_past_a_protected_newest_window() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();

    let mut disposable = event("continuation-disposable", ActivityState::Error);
    disposable.kind = ActivityKind::Diagnostic;
    disposable.decision_id = None;
    db.append_activity(disposable).unwrap();
    for index in 0..40 {
        db.append_activity(event(
            &format!("continuation-interrupted-{index}"),
            ActivityState::Observed,
        ))
        .unwrap();
        db.append_activity(event(
            &format!("continuation-interrupted-{index}"),
            ActivityState::Interrupted,
        ))
        .unwrap();
    }
    db.append_activity(event("continuation-straddler", ActivityState::Observed))
        .unwrap();
    db.append_activity(event("continuation-straddler", ActivityState::Error))
        .unwrap();

    let fresh_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let protected = (0..511)
        .map(|index| {
            let mut protected = event(
                &format!("continuation-protected-{index}"),
                ActivityState::Observed,
            );
            protected.recorded_at_ms = fresh_at;
            protected
        })
        .collect::<Vec<_>>();
    db.append_activity_batch(&protected).unwrap();
    let mut bound = event("continuation-bound", ActivityState::Error);
    bound.kind = ActivityKind::Diagnostic;
    bound.decision_id = None;
    let bound = db.append_activity(bound).unwrap();

    let first = db
        .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
        .unwrap();
    assert_eq!(first.deleted_activity_rows, 0);
    assert_eq!(
        db.activity_by_id("continuation-straddler", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        2,
        "the first bounded window must not partially delete its boundary group"
    );

    let mut deleted = 0;
    for _ in 0..3 {
        deleted += db
            .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
            .unwrap()
            .deleted_activity_rows;
    }

    assert_eq!(deleted, 19);
    assert!(
        db.activity_by_id("continuation-disposable", None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty(),
        "repeated bounded calls must advance to older disposable rows"
    );
    assert!(
        db.activity_by_id("continuation-straddler", None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty(),
        "a normal straddling group must be deleted whole on a later window"
    );
    for index in 0..40 {
        let retained = db
            .activity_by_id(
                &format!("continuation-interrupted-{index}"),
                None,
                10,
                64 * 1024,
            )
            .unwrap()
            .events
            .len();
        assert_eq!(retained, if index >= 8 { 2 } else { 0 });
    }
}

#[test]
fn bounded_retention_skips_a_whole_group_that_cannot_fit() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();

    let mut large = event("retention-large", ActivityState::Error);
    large.kind = ActivityKind::Diagnostic;
    large.decision_id = None;
    db.append_activity_batch(&vec![large; 129]).unwrap();

    let mut small = event("retention-small", ActivityState::Error);
    small.kind = ActivityKind::Diagnostic;
    small.decision_id = None;
    db.append_activity(small).unwrap();

    let mut bound = event("retention-capacity-bound", ActivityState::Error);
    bound.kind = ActivityKind::Diagnostic;
    bound.decision_id = None;
    let bound = db.append_activity(bound).unwrap();

    let outcome = db
        .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
        .unwrap();

    assert_eq!(outcome.deleted_activity_rows, 1);
    assert_eq!(
        db.activity_by_id("retention-large", None, 200, 4 * 1024 * 1024)
            .unwrap()
            .events
            .len(),
        129,
        "a group larger than the delete batch must remain whole"
    );
    assert!(
        db.activity_by_id("retention-small", None, 10, 64 * 1024)
            .unwrap()
            .events
            .is_empty(),
        "a later group that fits must still make progress"
    );
}

#[test]
fn bounded_retention_ranks_a_straddling_history_group_by_source_cursor() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;

    for index in 0..33 {
        let mut older = event(
            &format!("cursor-history-older-{index}"),
            ActivityState::Interrupted,
        );
        older.recorded_at_ms = now_ms + 200_000;
        db.append_activity(older).unwrap();
    }

    let mut hidden_interrupted = event("cursor-history-straddler", ActivityState::Interrupted);
    hidden_interrupted.recorded_at_ms = 1;
    db.append_activity(hidden_interrupted).unwrap();
    let mut newest_fragment = event("cursor-history-straddler", ActivityState::Observed);
    newest_fragment.recorded_at_ms = now_ms + 100_000;
    db.append_activity(newest_fragment).unwrap();

    let fresh = (0..511)
        .map(|index| {
            let mut fresh = event(
                &format!("cursor-history-fresh-{index}"),
                ActivityState::Observed,
            );
            fresh.recorded_at_ms = now_ms;
            fresh
        })
        .collect::<Vec<_>>();
    db.append_activity_batch(&fresh).unwrap();

    let mut bound = event("cursor-history-bound", ActivityState::Error);
    bound.kind = ActivityKind::Diagnostic;
    bound.decision_id = None;
    let bound = db.append_activity(bound).unwrap();

    let first = db
        .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
        .unwrap();
    let second = db
        .maintain_bounded(Some(bound), StorageDeadline::after(Duration::from_secs(1)))
        .unwrap();

    assert_eq!(first.deleted_activity_rows, 0);
    assert_eq!(second.deleted_activity_rows, 2);
    assert_eq!(
        db.activity_by_id("cursor-history-straddler", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        2,
        "the newest history group is defined by its durable last source cursor"
    );
    for index in 0..33 {
        let retained = db
            .activity_by_id(
                &format!("cursor-history-older-{index}"),
                None,
                10,
                64 * 1024,
            )
            .unwrap()
            .events
            .len();
        assert_eq!(retained, usize::from(index >= 2), "older group {index}");
    }
}

#[test]
fn corrupt_database_open_has_fixed_corrupt_category() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    fs::write(paths.brain_db(), vec![0xa5; 4096]).unwrap();
    fs::set_permissions(paths.brain_db(), fs::Permissions::from_mode(0o600)).unwrap();

    let result = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    );
    assert!(
        matches!(
            &result,
            Err(StorageError::StorageFault {
                operation: StorageOperation::Open,
                category: StorageFaultCategory::Corrupt,
            })
        ),
        "{result:?}"
    );
}

#[test]
fn busy_checkpoint_preserves_committed_wal_rows() {
    let root = private_root();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let reader = rusqlite::Connection::open(paths.brain_db()).unwrap();
    reader
        .execute_batch("BEGIN; SELECT singleton FROM schema_meta;")
        .unwrap();
    let mut committed = event("checkpoint-committed", ActivityState::Error);
    committed.kind = ActivityKind::Diagnostic;
    committed.decision_id = None;
    db.append_activity(committed).unwrap();

    assert!(matches!(
        db.maintain_bounded(None, StorageDeadline::after(Duration::from_millis(100))),
        Err(StorageError::Busy)
    ));
    assert_eq!(
        db.activity_by_id("checkpoint-committed", None, 10, 64 * 1024)
            .unwrap()
            .events
            .len(),
        1
    );
    reader.execute_batch("ROLLBACK").unwrap();
}
