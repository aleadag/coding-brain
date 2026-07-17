#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use codexctl_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityOutcome, ActivityState, ProjectEvidence,
    SessionTarget,
};
use codexctl_core::lifecycle::{LifecycleEvent, LifecycleStore, coding_brain_state_root};
use codexctl_core::paths::{CodingBrainPaths, PathEnvironment};
use codexctl_core::project::ProjectIdentity;
use serde::Deserialize;
use serde_json::Value;

use crate::brain::activity::ActivityStore;

pub(crate) const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;
static LIFECYCLE_ACTIVITY_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookInputError {
    Read,
    TooLarge,
}

impl fmt::Display for HookInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => f.write_str("could not read stdin"),
            Self::TooLarge => f.write_str("input exceeds 65536 bytes"),
        }
    }
}

pub(crate) fn read_bounded_hook_input(mut reader: impl Read) -> Result<Vec<u8>, HookInputError> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|_| HookInputError::Read)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        Err(HookInputError::TooLarge)
    } else {
        Ok(input)
    }
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let _ = writeln!(stderr, "coding-brain lifecycle hook: {diagnostic}");
}

pub(crate) fn run_with<R: Read, W: Write, E: Write>(
    stdin: R,
    stdout: W,
    stderr: E,
    store: &LifecycleStore,
) {
    run_with_activity(stdin, stdout, stderr, store, None);
}

#[derive(Debug, Deserialize)]
struct LifecycleActivityInput {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_response: Option<Value>,
}

pub(crate) fn run_with_activity<R: Read, W: Write, E: Write>(
    stdin: R,
    _stdout: W,
    mut stderr: E,
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
) {
    let input = match read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let event = match LifecycleEvent::parse(&input) {
        Ok(event) => event,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let activity_input = serde_json::from_slice::<LifecycleActivityInput>(&input).ok();
    if let Err(error) = store.record(event.clone()) {
        write_diagnostic(&mut stderr, error);
    }
    if let (Some(activity), Some(activity_input)) = (activity, activity_input) {
        let result = if event.name().as_str() == "PostToolUse" {
            append_outcome(activity, &event, &activity_input)
        } else {
            append_observation(activity, &event, &activity_input)
        };
        if let Err(error) = result {
            write_diagnostic(&mut stderr, error);
        }
        let _ = activity.compact_if_needed();
    }
}

fn append_observation(
    activity: &ActivityStore,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Result<(), String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    activity
        .append(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: format!(
                "lifecycle_{}_{}_{}",
                epoch_ms(),
                std::process::id(),
                LIFECYCLE_ACTIVITY_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            recorded_at_ms: epoch_ms(),
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: cwd.clone(),
                label: None,
            },
            session: Some(SessionTarget {
                session_id: lifecycle.identity().session_id().to_string(),
                turn_id: lifecycle.identity().turn_id().map(str::to_string),
                tool_use_id: input.tool_use_id.clone(),
                project_id,
                cwd,
                provider_hints: Vec::new(),
            }),
            state: ActivityState::Abstained,
            tool: Some(lifecycle.name().as_str().into()),
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|error| error.to_string())
}

fn append_outcome(
    activity: &ActivityStore,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Result<(), String> {
    let log = activity.read().map_err(|error| error.to_string())?;
    let identity = lifecycle.identity();
    let Some(tool_use_id) = input.tool_use_id.as_deref() else {
        let diagnostic = "orphan outcome: lifecycle event has no tool_use_id";
        append_orphan(activity, lifecycle, input, diagnostic)?;
        return Err(diagnostic.into());
    };
    let matched = log.events().iter().rev().find(|event| {
        event.state.is_terminal()
            && event.session.as_ref().is_some_and(|session| {
                session.session_id == identity.session_id()
                    && session.turn_id.as_deref() == identity.turn_id()
                    && session.tool_use_id.as_deref() == Some(tool_use_id)
            })
    });
    let Some(matched) = matched else {
        let diagnostic = "orphan outcome: no activity matches the lifecycle identity";
        append_orphan(activity, lifecycle, input, diagnostic)?;
        return Err(diagnostic.into());
    };
    let mut outcome = ActivityEvent {
        schema_version: matched.schema_version,
        activity_id: matched.activity_id.clone(),
        recorded_at_ms: epoch_ms(),
        project: matched.project.clone(),
        session: matched.session.clone(),
        state: ActivityState::Outcome,
        tool: matched.tool.clone(),
        normalized_command: None,
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: None,
        decision_id: matched.decision_id.clone(),
        outcome: Some(normalized_outcome(input.tool_response.as_ref())),
        correction: None,
        note: None,
        supersedes: None,
    };
    if let Some(session) = &mut outcome.session {
        session.tool_use_id.clone_from(&input.tool_use_id);
    }
    activity.append(outcome).map_err(|error| error.to_string())
}

fn append_orphan(
    activity: &ActivityStore,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    diagnostic: &str,
) -> Result<(), String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    activity
        .append(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: format!("orphan_{}_{}", epoch_ms(), std::process::id()),
            recorded_at_ms: epoch_ms(),
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: cwd.clone(),
                label: None,
            },
            session: Some(SessionTarget {
                session_id: lifecycle.identity().session_id().to_string(),
                turn_id: lifecycle.identity().turn_id().map(str::to_string),
                tool_use_id: input.tool_use_id.clone(),
                project_id,
                cwd,
                provider_hints: Vec::new(),
            }),
            state: ActivityState::Error,
            tool: input.tool_name.clone(),
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: Some(diagnostic.into()),
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|error| error.to_string())
}

fn normalized_outcome(response: Option<&Value>) -> ActivityOutcome {
    if response.is_some_and(|response| {
        response.get("is_error").and_then(Value::as_bool) == Some(true)
            || response
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0)
            || response.get("success").and_then(Value::as_bool) == Some(false)
    }) {
        ActivityOutcome::Failed
    } else {
        ActivityOutcome::Succeeded
    }
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn activity_store() -> Option<ActivityStore> {
    current_paths().map(|paths| ActivityStore::at(paths.state_root().join("activity.jsonl")))
}

fn current_paths() -> Option<CodingBrainPaths> {
    let environment = PathEnvironment::new(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    CodingBrainPaths::resolve(&environment).ok()
}

pub(crate) fn run() {
    let store = LifecycleStore::at(coding_brain_state_root());
    let activity = activity_store();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_with_activity(
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        &store,
        activity.as_ref(),
    );
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Cursor;

    use codexctl_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityOutcome, ActivityState, ProjectEvidence,
        SessionTarget,
    };
    use codexctl_core::lifecycle::{LifecycleStore, StoreCondition};
    use codexctl_core::project::ProjectId;

    use crate::brain::activity::ActivityStore;

    use super::*;

    const PROMPT: &[u8] = include_bytes!("../tests/fixtures/hooks/user-prompt-submit.json");

    #[test]
    fn bounded_reader_accepts_the_limit_and_rejects_one_more_byte() {
        let exact = vec![b' '; MAX_HOOK_INPUT_BYTES];
        assert_eq!(
            read_bounded_hook_input(Cursor::new(&exact)).unwrap().len(),
            MAX_HOOK_INPUT_BYTES
        );
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(matches!(
            read_bounded_hook_input(Cursor::new(&oversized)),
            Err(HookInputError::TooLarge)
        ));
    }

    #[test]
    fn valid_event_records_state_without_protocol_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(store.read().unwrap().condition, StoreCondition::Healthy);
    }

    #[test]
    fn generic_lifecycle_hook_records_only_normalized_activity_identity() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "UserPromptSubmit",
            "prompt": "secret raw prompt must not persist"
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ActivityState::Abstained);
        assert_eq!(events[0].tool.as_deref(), Some("UserPromptSubmit"));
        assert!(events[0].normalized_command.is_none());
        assert!(
            !fs::read_to_string(temp.path().join("activity.jsonl"))
                .unwrap()
                .contains("secret raw prompt")
        );
    }

    #[test]
    fn malformed_or_oversized_input_is_bounded_and_fail_open() {
        for input in [b"secret malformed payload".to_vec(), vec![b'x'; 65_537]] {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with(Cursor::new(input), &mut stdout, &mut stderr, &store);
            assert!(stdout.is_empty());
            let diagnostic = String::from_utf8(stderr).unwrap();
            assert!(diagnostic.starts_with("coding-brain lifecycle hook:"));
            assert!(diagnostic.len() < 256);
            assert!(!diagnostic.contains("secret"));
            assert!(!store.snapshot_path().exists());
        }
    }

    #[test]
    fn persistence_failure_and_newer_schema_leave_stdout_empty() {
        let temp = tempfile::tempdir().unwrap();
        let blocked_root = temp.path().join("blocked");
        fs::write(&blocked_root, b"occupied").unwrap();
        let blocked = LifecycleStore::at(&blocked_root);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &blocked);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());

        let store = LifecycleStore::at(temp.path().join("newer"));
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let newer = br#"{"schema_version":2}"#;
        fs::write(store.snapshot_path(), newer).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("newer"));
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), newer);
    }

    #[test]
    fn post_tool_use_joins_outcome_by_stable_hook_ids() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    session_id: "session-1".into(),
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: Some(0.9),
                threshold: Some(0.6),
                reasoning: Some("safe".into()),
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_response": {"exit_code": 0}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].activity_id, "activity-1");
        assert_eq!(events[1].decision_id.as_deref(), Some("decision-1"));
        assert_eq!(events[1].state, ActivityState::Outcome);
        assert_eq!(events[1].outcome, Some(ActivityOutcome::Succeeded));
        assert!(events[1].normalized_command.is_none());
    }

    #[test]
    fn unmatched_post_tool_use_appends_orphan_diagnostic_without_guessing() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let input = serde_json::json!({
            "session_id": "session-orphan",
            "turn_id": "turn-orphan",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-orphan",
            "tool_response": {"exit_code": 1}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("orphan outcome")
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ActivityState::Error);
        assert!(events[0].decision_id.is_none());
        assert!(events[0].normalized_command.is_none());
        assert!(
            events[0]
                .reasoning
                .as_deref()
                .unwrap()
                .contains("orphan outcome")
        );
    }

    #[test]
    fn missing_tool_use_id_never_matches_missing_activity_id() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: None,
                },
                session: Some(SessionTarget {
                    session_id: "session-1".into(),
                    turn_id: Some("turn-1".into()),
                    tool_use_id: None,
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: None,
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {"exit_code": 0}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].state, ActivityState::Error);
        assert_ne!(events[1].activity_id, "activity-1");
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("no tool_use_id")
        );
    }
}
