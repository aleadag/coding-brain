use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::discovery::transcript_summary_from_codex_jsonl;
use crate::provider::{AgentProvider, AgentSessionKey};
use crate::session::{AgentSession, SessionIdentityProvenance, SessionStatus};

use super::{
    LifecycleEventName, ProjectedStatus, SessionLifecycleState, StoreCondition, StoreError,
    StoreView,
};

const SHORT_LEASE_MS: u64 = 30_000;
const LONG_LEASE_MS: u64 = 10 * 60 * 1_000;
const MAX_FUTURE_SKEW_MS: u64 = 5_000;
const SESSION_START_HINT_LEASE_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TranscriptSemantic {
    Progress,
    Complete,
    ExplicitInput,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptEvidence {
    pub semantic: TranscriptSemantic,
    pub observed_at_ms: Option<u64>,
}

impl TranscriptEvidence {
    pub fn progress(observed_at_ms: Option<u64>) -> Self {
        Self {
            semantic: TranscriptSemantic::Progress,
            observed_at_ms,
        }
    }

    pub fn complete(observed_at_ms: Option<u64>) -> Self {
        Self {
            semantic: TranscriptSemantic::Complete,
            observed_at_ms,
        }
    }

    pub fn explicit_input(observed_at_ms: Option<u64>) -> Self {
        Self {
            semantic: TranscriptSemantic::ExplicitInput,
            observed_at_ms,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecycleEvidence {
    pub projected_status: ProjectedStatus,
    pub status_event: LifecycleEventName,
    pub status_received_at_ms: u64,
    pub latest_event: LifecycleEventName,
    pub latest_received_at_ms: u64,
    pub active_subagent_count: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LifecycleDiagnostic {
    pub available: bool,
    pub event: Option<LifecycleEventName>,
    pub age_ms: Option<u64>,
    pub contributing: bool,
    pub ignored_reason: Option<String>,
    pub store_condition: Option<StoreCondition>,
}

pub fn contributing_status(session: &mut AgentSession, now_ms: u64) -> Option<SessionStatus> {
    let Some(evidence) = session.lifecycle_evidence else {
        session.lifecycle_diagnostic.contributing = false;
        return None;
    };
    session.lifecycle_diagnostic.event = Some(evidence.latest_event);
    if evidence.status_received_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
        session.lifecycle_diagnostic.age_ms = None;
        session.lifecycle_diagnostic.contributing = false;
        session.lifecycle_diagnostic.ignored_reason = Some("future lifecycle timestamp".into());
        return None;
    }

    let age_ms = now_ms.saturating_sub(evidence.status_received_at_ms);
    session.lifecycle_diagnostic.age_ms = Some(age_ms);
    let lease_ms = match (evidence.projected_status, evidence.status_event) {
        (ProjectedStatus::NeedsInput, _) => LONG_LEASE_MS,
        (_, LifecycleEventName::UserPromptSubmit)
        | (_, LifecycleEventName::PermissionRequest)
        | (_, LifecycleEventName::PostToolUse) => SHORT_LEASE_MS,
        _ => LONG_LEASE_MS,
    };
    if age_ms >= lease_ms {
        session.lifecycle_diagnostic.contributing = false;
        session.lifecycle_diagnostic.ignored_reason = Some("lifecycle evidence expired".into());
        return None;
    }

    let superseded_by_hook = evidence.latest_received_at_ms > evidence.status_received_at_ms
        && match (evidence.status_event, evidence.latest_event) {
            (LifecycleEventName::SubagentStart, LifecycleEventName::SubagentStop) => {
                evidence.active_subagent_count == 0
            }
            (LifecycleEventName::PreToolUse, LifecycleEventName::PostToolUse)
            | (LifecycleEventName::PreToolUse, LifecycleEventName::Stop)
            | (LifecycleEventName::Stop, _) => true,
            _ => false,
        };
    if superseded_by_hook {
        session.lifecycle_diagnostic.contributing = false;
        session.lifecycle_diagnostic.ignored_reason = Some("superseded by lifecycle event".into());
        return None;
    }

    let invalidated_by_transcript = session.transcript_evidence.is_some_and(|transcript| {
        let Some(observed_at_ms) = transcript.observed_at_ms else {
            return false;
        };
        if observed_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS)
            || observed_at_ms <= evidence.status_received_at_ms
        {
            return false;
        }
        match transcript.semantic {
            TranscriptSemantic::Complete => {
                evidence.projected_status == ProjectedStatus::Processing
            }
            TranscriptSemantic::Progress => {
                evidence.projected_status == ProjectedStatus::Idle
                    || matches!(
                        evidence.status_event,
                        LifecycleEventName::PreToolUse | LifecycleEventName::SubagentStart
                    )
            }
            TranscriptSemantic::ExplicitInput => true,
        }
    });
    if invalidated_by_transcript {
        session.lifecycle_diagnostic.contributing = false;
        session.lifecycle_diagnostic.ignored_reason = Some("superseded by transcript".into());
        return None;
    }

    let status = match evidence.projected_status {
        ProjectedStatus::Processing => SessionStatus::Processing,
        ProjectedStatus::NeedsInput => SessionStatus::NeedsInput,
        ProjectedStatus::Idle => SessionStatus::Idle,
    };
    session.lifecycle_diagnostic.contributing = true;
    session.lifecycle_diagnostic.ignored_reason = None;
    Some(status)
}

pub fn apply_store_view(sessions: &mut [AgentSession], view: &StoreView, now_ms: u64) {
    for session in sessions
        .iter_mut()
        .filter(|session| eligible_local_session(session))
    {
        session.lifecycle_diagnostic.store_condition = Some(view.condition);
    }

    if view.condition != StoreCondition::Healthy {
        for session in sessions
            .iter_mut()
            .filter(|session| eligible_local_session(session))
        {
            clear_lifecycle(session, store_condition_reason(view.condition));
        }
        return;
    }
    let Some(snapshot) = view.snapshot.as_ref() else {
        for session in sessions
            .iter_mut()
            .filter(|session| eligible_local_session(session))
        {
            clear_lifecycle(session, "healthy lifecycle snapshot is unavailable");
        }
        return;
    };

    for session in sessions
        .iter_mut()
        .filter(|session| eligible_local_session(session))
    {
        session.lifecycle_evidence = None;
        session.lifecycle_diagnostic.available = false;
        session.lifecycle_diagnostic.event = None;
        session.lifecycle_diagnostic.age_ms = None;
        session.lifecycle_diagnostic.contributing = false;
        session.lifecycle_diagnostic.ignored_reason = None;
    }

    let states: Vec<(AgentSessionKey, &SessionLifecycleState)> = snapshot
        .sessions
        .iter()
        .filter_map(|(storage_key, state)| {
            AgentSessionKey::from_storage_key(storage_key).map(|key| (key, state))
        })
        .collect();
    let mut claimed_paths = Vec::new();
    for (session_key, state) in &states {
        let Some(session) = sessions.iter_mut().find(|session| {
            eligible_local_session(session)
                && session.provider == session_key.provider
                && session.session_id == session_key.session_id
        }) else {
            continue;
        };
        if !paths_match(Path::new(&session.cwd), &state.cwd)
            || !optional_paths_agree(
                session.jsonl_path.as_deref(),
                state.transcript_path.as_deref(),
            )
        {
            session.lifecycle_diagnostic.ignored_reason =
                Some("lifecycle identity does not match live session".into());
            continue;
        }
        if let Some(path) = state.transcript_path.as_deref() {
            claimed_paths.push(normalize_path(path));
        }
        attach_state(session, state, now_ms);
    }

    let transcript_claims = states
        .iter()
        .fold(HashMap::new(), |mut claims, (_, state)| {
            if state.latest_event == Some(LifecycleEventName::SessionStart)
                && state.latest_received_at_ms <= now_ms.saturating_add(MAX_FUTURE_SKEW_MS)
                && now_ms.saturating_sub(state.latest_received_at_ms) <= SESSION_START_HINT_LEASE_MS
                && let Some(path) = state.transcript_path.as_deref()
            {
                *claims.entry(normalize_path(path)).or_insert(0usize) += 1;
            }
            claims
        });

    let mut pending_bindings = Vec::new();
    for (session_key, state) in states {
        if state.latest_event != Some(LifecycleEventName::SessionStart) {
            continue;
        }
        if session_key.provider != AgentProvider::Codex {
            continue;
        }
        let Some(path) = state.transcript_path.as_deref() else {
            mark_placeholder_reason(sessions, &state.cwd, "SessionStart has no transcript path");
            continue;
        };
        let normalized_path = normalize_path(path);
        if claimed_paths.contains(&normalized_path) {
            continue;
        }
        if transcript_claims.get(&normalized_path).copied() != Some(1) {
            mark_placeholder_reason(
                sessions,
                &state.cwd,
                "multiple lifecycle sessions claim one transcript",
            );
            continue;
        }
        if state.latest_received_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
            mark_placeholder_reason(
                sessions,
                &state.cwd,
                "SessionStart timestamp is in the future",
            );
            continue;
        }
        if now_ms.saturating_sub(state.latest_received_at_ms) > SESSION_START_HINT_LEASE_MS {
            mark_placeholder_reason(sessions, &state.cwd, "SessionStart hint is stale");
            continue;
        }
        let Some(summary) = transcript_summary_from_codex_jsonl(path.to_path_buf()) else {
            mark_placeholder_reason(
                sessions,
                &state.cwd,
                "SessionStart transcript is unavailable",
            );
            continue;
        };
        if summary.session_id != session_key.session_id
            || !paths_match(Path::new(&summary.cwd), &state.cwd)
        {
            mark_placeholder_reason(
                sessions,
                &state.cwd,
                "SessionStart transcript metadata does not match",
            );
            continue;
        }

        let candidates: Vec<usize> = sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                eligible_local_session(session)
                    && session.provider == AgentProvider::Codex
                    && session.identity_provenance == SessionIdentityProvenance::ProcessOnly
                    && paths_match(Path::new(&session.cwd), &state.cwd)
                    && summary.mtime_ms >= session.started_at
            })
            .map(|(index, _)| index)
            .collect();
        if candidates.len() != 1 {
            mark_placeholder_reason(
                sessions,
                &state.cwd,
                if candidates.is_empty() {
                    "SessionStart has no compatible live process"
                } else {
                    "SessionStart matches multiple live processes"
                },
            );
            continue;
        }
        pending_bindings.push((candidates[0], session_key.session_id, summary, state));
    }

    let process_claims =
        pending_bindings
            .iter()
            .fold(HashMap::new(), |mut claims, (session_index, _, _, _)| {
                *claims.entry(*session_index).or_insert(0usize) += 1;
                claims
            });
    for (session_index, session_id, summary, state) in pending_bindings {
        if process_claims.get(&session_index).copied() != Some(1) {
            sessions[session_index].lifecycle_diagnostic.ignored_reason =
                Some("multiple SessionStart hints claim one live process".into());
            continue;
        }
        let session = &mut sessions[session_index];
        session.session_id = session_id;
        session.identity_provenance = SessionIdentityProvenance::Structured;
        session.jsonl_path = Some(summary.path);
        session.last_message_ts = session.last_message_ts.max(summary.mtime_ms);
        attach_state(session, state, now_ms);
    }
}

pub fn retain_after_store_error(sessions: &mut [AgentSession], error: &StoreError, now_ms: u64) {
    let error_reason = error.to_string();
    for session in sessions
        .iter_mut()
        .filter(|session| eligible_local_session(session))
    {
        session.lifecycle_diagnostic.store_condition = Some(StoreCondition::Unavailable);
        if !matches!(error, StoreError::Io | StoreError::LockTimeout) {
            clear_lifecycle(session, &error_reason);
            continue;
        }
        session.lifecycle_diagnostic.available = session.lifecycle_evidence.is_some();
        if session.lifecycle_evidence.is_some() && contributing_status(session, now_ms).is_none() {
            session.lifecycle_evidence = None;
            session.lifecycle_diagnostic.available = false;
        }
        session.lifecycle_diagnostic.ignored_reason = Some(error_reason.clone());
    }
}

fn attach_state(session: &mut AgentSession, state: &SessionLifecycleState, now_ms: u64) {
    session.lifecycle_diagnostic.available = true;
    session.lifecycle_diagnostic.event = state.latest_event;
    session.lifecycle_diagnostic.age_ms = (state.latest_received_at_ms
        <= now_ms.saturating_add(MAX_FUTURE_SKEW_MS))
    .then_some(now_ms.saturating_sub(state.latest_received_at_ms));
    session.lifecycle_diagnostic.contributing = false;
    session.lifecycle_diagnostic.ignored_reason = state
        .ignored_reason
        .map(|reason| format!("lifecycle event ignored: {reason:?}"));
    session.lifecycle_evidence = match (
        state.projected_status,
        state.status_event,
        state.status_received_at_ms,
        state.latest_event,
    ) {
        (
            Some(projected_status),
            Some(status_event),
            Some(status_received_at_ms),
            Some(latest_event),
        ) => Some(LifecycleEvidence {
            projected_status,
            status_event,
            status_received_at_ms,
            latest_event,
            latest_received_at_ms: state.latest_received_at_ms,
            active_subagent_count: state.active_subagents.len(),
        }),
        _ => None,
    };
    if state.latest_received_at_ms > session.last_message_ts {
        session.active_subagent_count = state.active_subagents.len();
    }
}

fn clear_lifecycle(session: &mut AgentSession, reason: &str) {
    session.lifecycle_evidence = None;
    session.lifecycle_diagnostic.available = false;
    session.lifecycle_diagnostic.event = None;
    session.lifecycle_diagnostic.age_ms = None;
    session.lifecycle_diagnostic.contributing = false;
    session.lifecycle_diagnostic.ignored_reason = Some(reason.into());
}

fn mark_placeholder_reason(sessions: &mut [AgentSession], cwd: &Path, reason: &str) {
    for session in sessions.iter_mut().filter(|session| {
        eligible_local_session(session)
            && session.provider == AgentProvider::Codex
            && session.identity_provenance == SessionIdentityProvenance::ProcessOnly
            && paths_match(Path::new(&session.cwd), cwd)
    }) {
        session.lifecycle_diagnostic.ignored_reason = Some(reason.into());
    }
}

fn eligible_local_session(session: &AgentSession) -> bool {
    session.process_backed && !session.is_remote() && session.status != SessionStatus::Finished
}

fn optional_paths_agree(left: Option<&Path>, right: Option<&Path>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => paths_match(left, right),
        _ => true,
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn store_condition_reason(condition: StoreCondition) -> &'static str {
    match condition {
        StoreCondition::Healthy => "lifecycle state is healthy",
        StoreCondition::Missing => "lifecycle state is missing",
        StoreCondition::Corrupt => "lifecycle state is corrupt",
        StoreCondition::NewerSchema(_) => "lifecycle state uses a newer schema",
        StoreCondition::Unavailable => "lifecycle state is unavailable",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;

    use crate::session::{
        AgentSession, ApprovalEvidence, ApprovalObservation, RawAgentSession,
        SessionIdentityProvenance, SessionStatus,
    };
    use crate::terminals::Terminal;

    use super::*;

    fn session_with_hook(
        status: ProjectedStatus,
        event: LifecycleEventName,
        received_at_ms: u64,
    ) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 7,
            process_start_identity: None,
            session_id: "session-7".into(),
            cwd: "/repo".into(),
            started_at: 0,
        });
        session.lifecycle_evidence = Some(LifecycleEvidence {
            projected_status: status,
            status_event: event,
            status_received_at_ms: received_at_ms,
            latest_event: event,
            latest_received_at_ms: received_at_ms,
            active_subagent_count: usize::from(event == LifecycleEventName::SubagentStart),
        });
        session.lifecycle_diagnostic.available = true;
        session
    }

    fn raw_session(pid: u32, session_id: &str, cwd: &Path, started_at: u64) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid,
            process_start_identity: None,
            session_id: session_id.into(),
            cwd: cwd.display().to_string(),
            started_at,
        });
        session.identity_provenance = SessionIdentityProvenance::ProcessOnly;
        session
    }

    fn snapshot_with_event(
        event_name: &str,
        session_id: &str,
        cwd: &Path,
        transcript_path: Option<&Path>,
        received_at_ms: u64,
    ) -> super::super::LifecycleSnapshot {
        let mut payload = serde_json::json!({
            "session_id": session_id,
            "cwd": cwd,
            "hook_event_name": event_name,
        });
        if let Some(path) = transcript_path {
            payload["transcript_path"] = serde_json::json!(path);
        }
        if event_name != "SessionStart" {
            payload["turn_id"] = serde_json::json!("turn-1");
        }
        match event_name {
            "SessionStart" => payload["source"] = serde_json::json!("startup"),
            "PreToolUse" => {
                payload["tool_name"] = serde_json::json!("Bash");
                payload["tool_input"] = serde_json::json!({"command": "ignored"});
                payload["tool_use_id"] = serde_json::json!("call-1");
            }
            "SubagentStart" | "SubagentStop" => {
                payload["agent_id"] = serde_json::json!("agent-1");
                payload["agent_type"] = serde_json::json!("worker");
            }
            _ => {}
        }
        let event =
            super::super::LifecycleEvent::parse(&serde_json::to_vec(&payload).unwrap()).unwrap();
        let mut snapshot = super::super::LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(event, received_at_ms),
            super::super::ApplyOutcome::Applied
        );
        snapshot
    }

    fn healthy_view(snapshot: super::super::LifecycleSnapshot) -> StoreView {
        StoreView {
            snapshot: Some(snapshot),
            condition: StoreCondition::Healthy,
        }
    }

    fn write_transcript(path: &Path, session_id: &str, cwd: &Path) {
        let mut file = std::fs::File::create(path).unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "timestamp": "2026-07-17T01:02:03Z",
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "timestamp": "2026-07-17T01:02:03Z",
                    "cwd": cwd,
                }
            })
        )
        .unwrap();
        file.flush().unwrap();
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn leases_expire_exactly_at_their_boundary() {
        let cases = [
            (
                LifecycleEventName::UserPromptSubmit,
                ProjectedStatus::Processing,
                30_000,
            ),
            (
                LifecycleEventName::PermissionRequest,
                ProjectedStatus::Processing,
                30_000,
            ),
            (
                LifecycleEventName::PostToolUse,
                ProjectedStatus::Processing,
                30_000,
            ),
            (
                LifecycleEventName::PreToolUse,
                ProjectedStatus::Processing,
                600_000,
            ),
            (
                LifecycleEventName::SubagentStart,
                ProjectedStatus::Processing,
                600_000,
            ),
            (
                LifecycleEventName::PermissionRequest,
                ProjectedStatus::NeedsInput,
                600_000,
            ),
            (LifecycleEventName::Stop, ProjectedStatus::Idle, 600_000),
        ];
        for (event, status, lease) in cases {
            let mut session = session_with_hook(status, event, 1_000);
            assert!(contributing_status(&mut session, 1_000 + lease - 1).is_some());
            assert_eq!(contributing_status(&mut session, 1_000 + lease), None);
        }
    }

    #[test]
    fn strictly_newer_transcript_semantics_invalidate_only_conflicting_hook_status() {
        let mut stopped = session_with_hook(ProjectedStatus::Idle, LifecycleEventName::Stop, 1_000);
        stopped.transcript_evidence = Some(TranscriptEvidence::progress(Some(2_000)));
        assert_eq!(contributing_status(&mut stopped, 3_000), None);

        let mut processing = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::UserPromptSubmit,
            1_000,
        );
        processing.transcript_evidence = Some(TranscriptEvidence::complete(Some(2_000)));
        assert_eq!(contributing_status(&mut processing, 3_000), None);

        let mut matching_stop =
            session_with_hook(ProjectedStatus::Idle, LifecycleEventName::Stop, 1_000);
        matching_stop.transcript_evidence = Some(TranscriptEvidence::complete(Some(2_000)));
        assert_eq!(
            contributing_status(&mut matching_stop, 3_000),
            Some(SessionStatus::Idle)
        );
    }

    #[test]
    fn equal_missing_and_future_transcript_timestamps_do_not_invalidate() {
        for observed_at_ms in [Some(500), Some(1_000), None, Some(10_001)] {
            let mut session =
                session_with_hook(ProjectedStatus::Idle, LifecycleEventName::Stop, 1_000);
            session.transcript_evidence = Some(TranscriptEvidence::progress(observed_at_ms));
            assert_eq!(
                contributing_status(&mut session, 5_000),
                Some(SessionStatus::Idle)
            );
        }
    }

    #[test]
    fn future_hook_timestamp_does_not_contribute() {
        let mut session = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::PreToolUse,
            10_001,
        );
        assert_eq!(contributing_status(&mut session, 5_000), None);
        assert!(!session.lifecycle_diagnostic.contributing);
    }

    #[test]
    fn reconciliation_does_not_mutate_actionable_fields() {
        let mut session = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::PreToolUse,
            1_000,
        );
        session.pending_tool_name = Some("exec_command".into());
        session.pending_tool_call_id = Some("call-7".into());
        session.pending_tool_input = Some("cargo test".into());
        session.pending_file_path = Some("src/main.rs".into());
        session.approval = ApprovalObservation::Confirmed(ApprovalEvidence {
            session_id: "session-7".into(),
            tty: "pts/7".into(),
            call_id: "call-7".into(),
            tool: "exec_command".into(),
            command: "cargo test".into(),
            backend: Terminal::Tmux,
            target: "main:1.0".into(),
            prompt_pattern_version: 1,
            prompt_fingerprint: 42,
        });
        let actionable = (
            session.pending_tool_name.clone(),
            session.pending_tool_call_id.clone(),
            session.pending_tool_input.clone(),
            session.pending_file_path.clone(),
            session.approval.clone(),
        );

        assert_eq!(
            contributing_status(&mut session, 2_000),
            Some(SessionStatus::Processing)
        );
        assert_eq!(
            (
                session.pending_tool_name,
                session.pending_tool_call_id,
                session.pending_tool_input,
                session.pending_file_path,
                session.approval,
            ),
            actionable
        );
    }

    #[test]
    fn exact_identity_attaches_compact_evidence() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("rollout.jsonl");
        let mut session = raw_session(7, "session-7", root.path(), 500);
        session.jsonl_path = Some(transcript.clone());
        let snapshot = snapshot_with_event(
            "PreToolUse",
            "session-7",
            root.path(),
            Some(&transcript),
            1_000,
        );

        let mut sessions = [session];
        apply_store_view(&mut sessions, &healthy_view(snapshot), 2_000);

        assert_eq!(
            sessions[0].lifecycle_evidence.unwrap().projected_status,
            ProjectedStatus::Processing
        );
        assert!(sessions[0].lifecycle_diagnostic.available);
        assert_eq!(
            sessions[0].lifecycle_diagnostic.event,
            Some(LifecycleEventName::PreToolUse)
        );
    }

    #[test]
    fn lifecycle_subagent_count_only_replaces_older_transcript_evidence() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = snapshot_with_event("SubagentStart", "session-7", root.path(), None, 1_000);
        let mut older = raw_session(7, "session-7", root.path(), 0);
        older.last_message_ts = 500;
        older.active_subagent_count = 4;
        let mut newer = raw_session(8, "session-7", root.path(), 0);
        newer.last_message_ts = 1_500;
        newer.active_subagent_count = 4;

        apply_store_view(
            std::slice::from_mut(&mut older),
            &healthy_view(snapshot.clone()),
            2_000,
        );
        apply_store_view(
            std::slice::from_mut(&mut newer),
            &healthy_view(snapshot),
            2_000,
        );

        assert_eq!(older.active_subagent_count, 1);
        assert_eq!(older.lifecycle_evidence.unwrap().active_subagent_count, 1);
        assert_eq!(newer.active_subagent_count, 4);
    }

    #[test]
    fn one_subagent_stop_does_not_hide_other_active_subagents() {
        let mut session = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::SubagentStart,
            1_000,
        );
        let evidence = session.lifecycle_evidence.as_mut().unwrap();
        evidence.latest_event = LifecycleEventName::SubagentStop;
        evidence.latest_received_at_ms = 2_000;
        evidence.active_subagent_count = 1;

        assert_eq!(
            contributing_status(&mut session, 3_000),
            Some(SessionStatus::Processing)
        );
        assert_eq!(
            session.lifecycle_diagnostic.event,
            Some(LifecycleEventName::SubagentStop)
        );

        session
            .lifecycle_evidence
            .as_mut()
            .unwrap()
            .active_subagent_count = 0;
        assert_eq!(contributing_status(&mut session, 3_000), None);
    }

    #[test]
    fn fresh_unambiguous_session_start_binds_placeholder() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("rollout.jsonl");
        write_transcript(&transcript, "session-7", root.path());
        let now_ms = now_ms();
        let snapshot = snapshot_with_event(
            "SessionStart",
            "session-7",
            root.path(),
            Some(&transcript),
            now_ms - 1_000,
        );
        let mut sessions = [raw_session(7, "codex-7", root.path(), now_ms - 2_000)];

        apply_store_view(&mut sessions, &healthy_view(snapshot), now_ms);

        assert_eq!(sessions[0].session_id, "session-7");
        assert_eq!(
            sessions[0].identity_provenance,
            SessionIdentityProvenance::Structured
        );
        assert_eq!(
            sessions[0].jsonl_path.as_deref(),
            Some(transcript.as_path())
        );
        assert!(sessions[0].lifecycle_diagnostic.available);
    }

    #[test]
    fn ambiguous_or_unsafe_session_start_never_binds() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("rollout.jsonl");
        let missing = root.path().join("missing.jsonl");
        write_transcript(&transcript, "session-7", root.path());
        let now_ms = now_ms();

        for (received_at_ms, path, started_at) in [
            (now_ms - 30_001, Some(transcript.as_path()), now_ms - 2_000),
            (now_ms + 5_001, Some(transcript.as_path()), now_ms - 2_000),
            (now_ms - 1_000, None, now_ms - 2_000),
            (now_ms - 1_000, Some(missing.as_path()), now_ms - 2_000),
            (now_ms - 1_000, Some(transcript.as_path()), now_ms + 1_000),
        ] {
            let snapshot = snapshot_with_event(
                "SessionStart",
                "session-7",
                root.path(),
                path,
                received_at_ms,
            );
            let mut sessions = [raw_session(7, "codex-7", root.path(), started_at)];
            apply_store_view(&mut sessions, &healthy_view(snapshot), now_ms);
            assert_eq!(sessions[0].session_id, "codex-7");
            assert!(sessions[0].lifecycle_diagnostic.ignored_reason.is_some());
        }

        let snapshot = snapshot_with_event(
            "SessionStart",
            "session-7",
            root.path(),
            Some(&transcript),
            now_ms - 1_000,
        );
        let mut sessions = [
            raw_session(7, "codex-7", root.path(), now_ms - 2_000),
            raw_session(8, "codex-8", root.path(), now_ms - 2_000),
        ];
        apply_store_view(&mut sessions, &healthy_view(snapshot), now_ms);
        assert_eq!(sessions[0].session_id, "codex-7");
        assert_eq!(sessions[1].session_id, "codex-8");
        assert!(sessions.iter().all(|session| {
            session.identity_provenance == SessionIdentityProvenance::ProcessOnly
        }));

        for remote in [false, true] {
            let snapshot = snapshot_with_event(
                "SessionStart",
                "session-7",
                root.path(),
                Some(&transcript),
                now_ms - 1_000,
            );
            let mut session = raw_session(7, "codex-7", root.path(), now_ms - 2_000);
            if remote {
                session.worker_origin = Some("remote".into());
            } else {
                session.process_backed = false;
            }
            apply_store_view(
                std::slice::from_mut(&mut session),
                &healthy_view(snapshot),
                now_ms,
            );
            assert_eq!(session.session_id, "codex-7");
        }
    }

    #[test]
    fn structured_codex_prefixed_session_is_not_treated_as_a_process_placeholder() {
        let root = tempfile::tempdir().unwrap();
        let transcript = root.path().join("rollout.jsonl");
        write_transcript(&transcript, "session-7", root.path());
        let now_ms = now_ms();
        let snapshot = snapshot_with_event(
            "SessionStart",
            "session-7",
            root.path(),
            Some(&transcript),
            now_ms - 1_000,
        );
        let mut session = raw_session(7, "codex-7", root.path(), now_ms - 2_000);
        session.identity_provenance = SessionIdentityProvenance::Structured;

        apply_store_view(
            std::slice::from_mut(&mut session),
            &healthy_view(snapshot),
            now_ms,
        );

        assert_eq!(session.session_id, "codex-7");
        assert_eq!(
            session.identity_provenance,
            SessionIdentityProvenance::Structured
        );
        assert_eq!(session.jsonl_path, None);
        assert_eq!(session.lifecycle_diagnostic.ignored_reason, None);
    }

    #[test]
    fn mismatched_metadata_and_duplicate_transcript_claims_do_not_bind() {
        let root = tempfile::tempdir().unwrap();
        let now_ms = now_ms();
        for (file_name, transcript_id, transcript_cwd) in [
            ("wrong-id.jsonl", "other-session", root.path()),
            ("wrong-cwd.jsonl", "session-7", Path::new("/other")),
        ] {
            let transcript = root.path().join(file_name);
            write_transcript(&transcript, transcript_id, transcript_cwd);
            let snapshot = snapshot_with_event(
                "SessionStart",
                "session-7",
                root.path(),
                Some(&transcript),
                now_ms - 1_000,
            );
            let mut session = raw_session(7, "codex-7", root.path(), now_ms - 2_000);
            apply_store_view(
                std::slice::from_mut(&mut session),
                &healthy_view(snapshot),
                now_ms,
            );
            assert_eq!(session.session_id, "codex-7");
            assert!(session.lifecycle_diagnostic.ignored_reason.is_some());
        }

        let transcript = root.path().join("claimed-twice.jsonl");
        write_transcript(&transcript, "session-7", root.path());
        let mut snapshot = snapshot_with_event(
            "SessionStart",
            "session-7",
            root.path(),
            Some(&transcript),
            now_ms - 1_000,
        );
        let second = snapshot_with_event(
            "SessionStart",
            "session-8",
            root.path(),
            Some(&transcript),
            now_ms - 1_000,
        );
        snapshot.sessions.extend(second.sessions);
        let mut session = raw_session(7, "codex-7", root.path(), now_ms - 2_000);
        apply_store_view(
            std::slice::from_mut(&mut session),
            &healthy_view(snapshot),
            now_ms,
        );
        assert_eq!(session.session_id, "codex-7");
        assert_eq!(
            session.lifecycle_diagnostic.ignored_reason.as_deref(),
            Some("multiple lifecycle sessions claim one transcript")
        );

        let transcript_7 = root.path().join("session-7.jsonl");
        let transcript_8 = root.path().join("session-8.jsonl");
        write_transcript(&transcript_7, "session-7", root.path());
        write_transcript(&transcript_8, "session-8", root.path());
        let mut snapshot = snapshot_with_event(
            "SessionStart",
            "session-7",
            root.path(),
            Some(&transcript_7),
            now_ms - 1_000,
        );
        let second = snapshot_with_event(
            "SessionStart",
            "session-8",
            root.path(),
            Some(&transcript_8),
            now_ms - 1_000,
        );
        snapshot.sessions.extend(second.sessions);
        let mut session = raw_session(7, "codex-7", root.path(), now_ms - 2_000);
        apply_store_view(
            std::slice::from_mut(&mut session),
            &healthy_view(snapshot),
            now_ms,
        );
        assert_eq!(session.session_id, "codex-7");
        assert_eq!(
            session.lifecycle_diagnostic.ignored_reason.as_deref(),
            Some("multiple SessionStart hints claim one live process")
        );
    }

    #[test]
    fn unhealthy_views_clear_evidence_and_transient_errors_retain_only_fresh_evidence() {
        for condition in [
            StoreCondition::Missing,
            StoreCondition::Corrupt,
            StoreCondition::NewerSchema(3),
        ] {
            let mut session = session_with_hook(
                ProjectedStatus::Processing,
                LifecycleEventName::PreToolUse,
                1_000,
            );
            apply_store_view(
                std::slice::from_mut(&mut session),
                &StoreView {
                    snapshot: None,
                    condition,
                },
                2_000,
            );
            assert!(session.lifecycle_evidence.is_none());
            assert_eq!(
                session.lifecycle_diagnostic.store_condition,
                Some(condition)
            );
        }

        let mut session = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::PreToolUse,
            1_000,
        );
        session.lifecycle_evidence = Some(LifecycleEvidence {
            projected_status: ProjectedStatus::Processing,
            status_event: LifecycleEventName::PreToolUse,
            status_received_at_ms: 1_000,
            latest_event: LifecycleEventName::PreToolUse,
            latest_received_at_ms: 1_000,
            active_subagent_count: 0,
        });
        retain_after_store_error(
            std::slice::from_mut(&mut session),
            &StoreError::LockTimeout,
            600_999,
        );
        assert!(session.lifecycle_evidence.is_some());
        assert_eq!(
            session.lifecycle_diagnostic.ignored_reason.as_deref(),
            Some("lifecycle store lock timed out")
        );
        assert_eq!(
            session.lifecycle_diagnostic.store_condition,
            Some(StoreCondition::Unavailable)
        );
        retain_after_store_error(
            std::slice::from_mut(&mut session),
            &StoreError::LockTimeout,
            601_000,
        );
        assert!(session.lifecycle_evidence.is_none());

        let mut non_transient = session_with_hook(
            ProjectedStatus::Processing,
            LifecycleEventName::PreToolUse,
            1_000,
        );
        retain_after_store_error(
            std::slice::from_mut(&mut non_transient),
            &StoreError::NewerSchema(3),
            2_000,
        );
        assert!(non_transient.lifecycle_evidence.is_none());
    }

    #[test]
    fn local_store_views_do_not_modify_remote_or_non_process_sessions() {
        for remote in [false, true] {
            let mut session = session_with_hook(
                ProjectedStatus::Processing,
                LifecycleEventName::PreToolUse,
                1_000,
            );
            if remote {
                session.worker_origin = Some("remote".into());
            } else {
                session.process_backed = false;
            }
            let before = (
                session.lifecycle_evidence,
                session.lifecycle_diagnostic.clone(),
            );

            apply_store_view(
                std::slice::from_mut(&mut session),
                &StoreView {
                    snapshot: None,
                    condition: StoreCondition::Missing,
                },
                2_000,
            );

            assert_eq!(
                (session.lifecycle_evidence, session.lifecycle_diagnostic),
                before
            );
        }
    }
}
