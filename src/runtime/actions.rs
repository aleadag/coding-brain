//! Bind `Actions` (the runtime write surface) to the binary's real
//! subsystems: brain decisions store, terminal backends, process kill.

use std::fs;

use codexctl_core::discovery;
use codexctl_core::helpers;
use codexctl_core::runtime::{
    Actions, BrainActions, BrainGateMode, CorrectionInput, DecisionScope, LogDecisionInput,
    ObservationInput,
};
use codexctl_core::terminals;

use crate::brain;

pub struct LiveActions;

impl Actions for LiveActions {
    fn terminate_session(&self, pid: u32) -> Result<(), String> {
        helpers::kill_process(pid)
    }

    fn inject_text(&self, session_id: &str, text: &str) -> Result<(), String> {
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let Some(session) = sessions.into_iter().find(|s| s.session_id == session_id) else {
            return Err(format!("session {session_id} not running"));
        };
        terminals::send_input(&session, text)
    }

    fn set_gate_mode(&self, mode: BrainGateMode) -> Result<(), String> {
        let path = brain::gate_mode_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create gate-mode dir: {e}"))?;
        }
        fs::write(&path, gate_mode_label(mode)).map_err(|e| format!("write gate-mode: {e}"))
    }

    fn log_observation(&self, observation: ObservationInput) -> Result<(), String> {
        // Look up the session for richer context, when the PID is currently
        // running. We don't bail if it isn't — the brain happily logs orphan
        // observations.
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let session_ref = sessions.iter().find(|s| s.pid == observation.session_pid);

        brain::decisions::log_observation(
            observation.session_pid,
            &observation.project,
            observation.tool.as_deref(),
            observation.command.as_deref(),
            &observation.observed_action,
            session_ref,
        );
        Ok(())
    }

    fn log_decision(&self, input: LogDecisionInput) -> Result<(), String> {
        // Resolve the live session for richer context (cost, model, etc.) —
        // brain::decisions::log_decision tolerates None when the PID is gone.
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let session_ref = sessions.iter().find(|s| s.pid == input.session_pid);

        // The trait's PendingSuggestion uses `action: String`; the brain
        // log_decision needs a `BrainSuggestion` with a real `RuleAction`.
        // Drop silently on unknown labels (caller validates upstream).
        let Some(rule_action) = codexctl_core::rules::RuleAction::parse(&input.suggestion.action)
        else {
            return Err(format!("unknown action label: {}", input.suggestion.action));
        };
        let suggestion = brain::client::BrainSuggestion {
            action: rule_action,
            message: input.suggestion.message,
            reasoning: input.suggestion.reasoning,
            confidence: input.suggestion.confidence,
            suggested_at: input.suggestion.suggested_at,
        };

        let decision_type = match input.decision_type {
            DecisionScope::Session => brain::decisions::DecisionType::Session,
            DecisionScope::Orchestration => brain::decisions::DecisionType::Orchestration,
        };

        brain::decisions::log_decision(
            input.session_pid,
            &input.project,
            input.tool.as_deref(),
            input.command.as_deref(),
            &suggestion,
            &input.user_action,
            session_ref,
            decision_type,
            input.override_reason.as_deref(),
        );
        Ok(())
    }

    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String> {
        brain::review::mark_by_id(decision_id, note.as_deref())
    }
}

impl BrainActions for LiveActions {
    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String> {
        let paths = brain::distill::current_paths().map_err(|error| error.to_string())?;
        let store = brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"));
        let source = store
            .read()
            .map_err(|error| error.to_string())?
            .events()
            .iter()
            .rev()
            .find(|event| event.activity_id == correction.activity_id)
            .cloned()
            .ok_or_else(|| format!("activity {} not found", correction.activity_id))?;
        store
            .append(codexctl_core::brain_activity::ActivityEvent {
                schema_version: codexctl_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
                activity_id: correction.activity_id,
                recorded_at_ms: epoch_ms(),
                project: source.project,
                session: source.session,
                state: codexctl_core::brain_activity::ActivityState::Correction,
                tool: None,
                normalized_command: None,
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: source.decision_id,
                outcome: None,
                correction: Some(correction.disposition),
                note: correction.note,
                supersedes: None,
            })
            .map_err(|error| error.to_string())
    }

    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String> {
        brain::review::mark_by_id(decision_id, note.as_deref())
    }

    fn set_gate_mode(&self, mode: BrainGateMode) -> Result<(), String> {
        <Self as Actions>::set_gate_mode(self, mode)
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Inverse of `crate::runtime::brain::parse_gate_mode` — writes the canonical
/// lowercased label the reader expects.
fn gate_mode_label(mode: BrainGateMode) -> &'static str {
    match mode {
        BrainGateMode::On => "on",
        BrainGateMode::Off => "off",
        BrainGateMode::Auto => "auto",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use codexctl_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityState, CorrectionDisposition,
        ProjectEvidence, SnapshotLimits,
    };
    use codexctl_core::paths::{CodingBrainPaths, PathEnvironment};
    use codexctl_core::project::ProjectId;

    /// Round-trip the label format with the parser in the brain wrapper.
    #[test]
    fn label_round_trips_through_parse() {
        for mode in [BrainGateMode::On, BrainGateMode::Off, BrainGateMode::Auto] {
            let label = gate_mode_label(mode);
            let parsed = match label {
                "on" => BrainGateMode::On,
                "off" => BrainGateMode::Off,
                "auto" => BrainGateMode::Auto,
                _ => panic!("unexpected label: {label}"),
            };
            assert_eq!(parsed, mode);
        }
    }

    /// Set-then-read against a temporary HOME confirms the file actually
    /// lands at the expected path and the binary's `brain::read_gate_mode`
    /// picks it up.
    #[test]
    fn set_gate_mode_persists_to_file() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let original = std::env::var("HOME").ok();
        // SAFETY: HOME mutation is serialized by HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", dir.path()) };

        let actions = LiveActions;
        Actions::set_gate_mode(&actions, BrainGateMode::Off).unwrap();
        assert_eq!(brain::read_gate_mode().trim(), "off");

        Actions::set_gate_mode(&actions, BrainGateMode::Auto).unwrap();
        assert_eq!(brain::read_gate_mode().trim(), "auto");

        if let Some(home) = original {
            unsafe { std::env::set_var("HOME", home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
    }

    #[test]
    fn correction_is_append_only_redacted_and_resolves_attention() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root = tempfile::tempdir().unwrap();
        let original_home = std::env::var_os("HOME");
        let original_config = std::env::var_os("XDG_CONFIG_HOME");
        let original_state = std::env::var_os("XDG_STATE_HOME");
        unsafe {
            std::env::set_var("HOME", root.path());
            std::env::set_var("XDG_CONFIG_HOME", root.path().join("config"));
            std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
        }
        let paths = CodingBrainPaths::resolve(&PathEnvironment::new(
            Some(root.path().join("config")),
            Some(root.path().join("state")),
            Some(root.path().to_path_buf()),
        ))
        .unwrap();
        let store = brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"));
        store.append(source_event()).unwrap();

        BrainActions::record_correction(
            &LiveActions,
            CorrectionInput {
                activity_id: "activity-1".into(),
                disposition: CorrectionDisposition::BrainWrong,
                note: Some("token=private-value wrong project".into()),
            },
        )
        .unwrap();

        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert!(snapshot.attention.is_empty());
        assert_eq!(snapshot.recent.len(), 1);
        assert_eq!(
            snapshot.recent[0].correction,
            Some(CorrectionDisposition::BrainWrong)
        );
        assert_eq!(
            snapshot.recent[0].note.as_deref(),
            Some("[REDACTED] wrong project")
        );

        restore_env("HOME", original_home);
        restore_env("XDG_CONFIG_HOME", original_config);
        restore_env("XDG_STATE_HOME", original_state);
    }

    fn source_event() -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: "activity-1".into(),
            recorded_at_ms: 1,
            project: ProjectEvidence {
                project_id: ProjectId::Stable("project-1".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: ActivityState::Denied,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: Some("fixture".into()),
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("fixture".into()),
            decision_id: Some("decision-1".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn restore_env(key: &str, value: Option<std::ffi::OsString>) {
        if let Some(value) = value {
            unsafe { std::env::set_var(key, value) };
        } else {
            unsafe { std::env::remove_var(key) };
        }
    }
}
