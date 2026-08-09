use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use coding_brain::brain::storage::{BrainDb, StoragePaths};
use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
};
use coding_brain_core::project::ProjectId;

fn event(activity_id: impl Into<String>, recorded_at_ms: u64) -> ActivityEvent {
    let activity_id = activity_id.into();
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: activity_id.clone(),
        recorded_at_ms,
        project: ProjectEvidence {
            project_id: ProjectId::Temporary("scale-project".into()),
            cwd: PathBuf::from("/work/scale-project"),
            label: Some("scale-project".into()),
        },
        session: None,
        state: ActivityState::Denied,
        tool: Some("Bash".into()),
        normalized_command: Some(format!("command-{activity_id}")),
        fingerprint: Some(format!("fingerprint-{activity_id}")),
        rule_id: Some("scale".into()),
        confidence: Some(0.9),
        threshold: Some(0.8),
        reasoning: Some("bounded scale fixture".into()),
        decision_id: Some(format!("decision-{activity_id}")),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

#[test]
fn sqlite_activity_scale_reads_use_frozen_indexes() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(root.path());
    let mut db = BrainDb::create_current(&paths).unwrap();
    let events = (0..50_000_u64)
        .map(|index| event(format!("sqlite-activity-{index}"), index))
        .collect::<Vec<_>>();
    db.append_activity_batch(&events).unwrap();

    let recent_plan = db.explain_recent_activity().unwrap();
    let after_plan = db.explain_activity_after_cursor().unwrap();
    let id_plan = db.explain_activity_by_id().unwrap();
    assert!(recent_plan.contains("activity_events_cursor"));
    assert!(recent_plan.contains("USING COVERING INDEX"));
    assert!(after_plan.contains("activity_events_cursor"));
    assert!(after_plan.contains("source_cursor>?"));
    assert!(id_plan.contains("activity_events_activity_id"));
    let recent = db.read_activity_page(None, 100, 1024 * 1024).unwrap();
    assert_eq!(recent.events.len(), 100);
    assert_eq!(recent.events[0].cursor.get(), 50_000);
    assert_eq!(recent.events[99].cursor.get(), 49_901);
}
