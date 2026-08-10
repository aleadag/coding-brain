use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SessionTarget, SessionTargetProvenance,
};
use coding_brain_core::lifecycle::PermissionAction;
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;
use rusqlite::params;

use crate::brain::decisions::{DecisionRecord, DecisionType};

use super::{BrainDb, DecisionIdentity, DecisionKind, DecisionPayload, ReviewDb, StoragePaths};

pub(crate) fn deterministic_historical_fixture(
    decision_id: &str,
) -> (tempfile::TempDir, StoragePaths) {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(root.path());
    let mut brain = BrainDb::create_current(&paths).unwrap();
    drop(ReviewDb::create_current(&paths).unwrap());
    let project_id = ProjectId::Temporary("historical-test".into());
    let terminal = ActivityEvent {
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
    };
    let cursor = brain.append_activity(terminal).unwrap();
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
use std::fs;
use std::os::unix::fs::PermissionsExt;
