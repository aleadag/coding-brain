#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
    ProjectEvidence, SessionTarget, bounded_activity_identifier, lossless_redacted_activity_text,
};
use coding_brain_core::lifecycle::{LifecycleEvent, LifecycleStore, coding_brain_state_root};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::ProjectIdentity;
use serde::Deserialize;
use serde_json::Value;

use crate::brain::activity::{ActivityLog, ActivityStore};

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
    tool_input: Value,
    #[serde(default)]
    tool_response: Option<Value>,
}

impl LifecycleActivityInput {
    fn normalized_bash_command(&self) -> Option<String> {
        (self.tool_name.as_deref() == Some("Bash"))
            .then(|| self.tool_input.get("command")?.as_str())
            .flatten()
            .filter(|command| !command.trim().is_empty())
            .and_then(lossless_redacted_activity_text)
    }

    fn normalized_tool_use_id(&self) -> Option<String> {
        self.tool_use_id.as_deref().map(bounded_activity_identifier)
    }
}

enum Correlation {
    Outcome(ActivityEvent),
    Diagnostic {
        event: ActivityEvent,
        message: &'static str,
    },
    None,
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
            let observation = match observation_event(&event, &activity_input) {
                Ok(observation) => observation,
                Err(error) => {
                    write_diagnostic(&mut stderr, error);
                    return;
                }
            };
            let mut correlation_message = None;
            let result = activity
                .append_from_snapshot(|log| {
                    let mut events = vec![observation];
                    match correlate_outcome(log, &event, &activity_input) {
                        Correlation::Outcome(outcome) => events.push(outcome),
                        Correlation::Diagnostic { event, message } => {
                            correlation_message = Some(message);
                            events.push(event);
                        }
                        Correlation::None => {}
                    }
                    events
                })
                .map_err(|error| error.to_string());
            if result.is_ok()
                && let Some(message) = correlation_message
            {
                write_diagnostic(&mut stderr, message);
            }
            result
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
    activity
        .append(observation_event(lifecycle, input)?)
        .map_err(|error| error.to_string())
}

fn observation_event(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Result<ActivityEvent, String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    Ok(ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Lifecycle,
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
            provider: lifecycle.identity().provider(),
            session_id: lifecycle.identity().session_id().to_string(),
            turn_id: lifecycle.identity().turn_id().map(str::to_string),
            tool_use_id: input
                .tool_use_id
                .as_deref()
                .map(bounded_activity_identifier),
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
}

fn correlate_outcome(
    log: &ActivityLog,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Correlation {
    let identity = lifecycle.identity();
    let Some(tool_use_id) = input.normalized_tool_use_id() else {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: lifecycle event has no tool_use_id",
        );
    };

    let exact_activity_ids = unique_activity_ids(log.events().iter().filter(|event| {
        event.kind == ActivityKind::Decision
            && event.session.as_ref().is_some_and(|session| {
                session.provider == identity.provider()
                    && session.session_id == identity.session_id()
                    && session.turn_id.as_deref() == identity.turn_id()
                    && session.tool_use_id.as_deref() == Some(tool_use_id.as_str())
            })
    }));
    if !exact_activity_ids.is_empty() {
        if exact_activity_ids.len() != 1 {
            return diagnostic_correlation(
                lifecycle,
                input,
                "orphan outcome: exact lifecycle identity is ambiguous or ineligible",
            );
        }
        return correlate_candidates(
            log,
            lifecycle,
            input,
            exact_activity_ids,
            "orphan outcome: exact lifecycle identity is ambiguous or ineligible",
        );
    }

    if input.tool_name.as_deref() != Some("Bash") {
        return Correlation::None;
    }

    let anchors = log
        .events()
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PreToolUse")
                && event.session.as_ref().is_some_and(|session| {
                    session.session_id == identity.session_id()
                        && session.turn_id.as_deref() == identity.turn_id()
                        && session.tool_use_id.as_deref() == Some(tool_use_id.as_str())
                })
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if anchors.len() != 1 {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: PreToolUse anchor is missing or ambiguous",
        );
    }
    let pre_index = anchors[0];
    let next_pre_index = log.events()[pre_index + 1..]
        .iter()
        .position(|event| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PreToolUse")
                && event.session.as_ref().is_some_and(|session| {
                    session.session_id == identity.session_id()
                        && session.turn_id.as_deref() == identity.turn_id()
                })
        })
        .map_or(log.events().len(), |offset| pre_index + 1 + offset);
    if next_pre_index < log.events().len() {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: PreToolUse interval overlaps a later tool",
        );
    }
    let interval = &log.events()[pre_index + 1..next_pre_index];
    if !interval
        .iter()
        .any(|event| event.kind == ActivityKind::Decision)
    {
        return Correlation::None;
    }
    let Some(command) = input.normalized_bash_command() else {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: Bash command is not losslessly correlatable",
        );
    };
    let activity_ids =
        unique_activity_ids(interval.iter().enumerate().filter_map(|(offset, event)| {
            let index = pre_index + 1 + offset;
            (event.kind == ActivityKind::Decision
                && event.schema_version == ACTIVITY_SCHEMA_VERSION
                && event.state == ActivityState::Allowed
                && event.decision_id.is_some()
                && event.tool.as_deref() == Some("Bash")
                && event.normalized_command.as_deref() == Some(command.as_str())
                && event.session.as_ref().is_some_and(|session| {
                    session.session_id == identity.session_id()
                        && session.turn_id.as_deref() == identity.turn_id()
                })
                && first_allowed_terminal_with_index(log, &event.activity_id)
                    .is_some_and(|(first_index, _)| first_index == index))
            .then_some(event)
        }));
    if activity_ids.is_empty() {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: no eligible Decision in the PreToolUse interval",
        );
    }
    correlate_candidates(
        log,
        lifecycle,
        input,
        activity_ids,
        "orphan outcome: Decision correlation is ambiguous or ineligible",
    )
}

fn unique_activity_ids<'a>(events: impl Iterator<Item = &'a ActivityEvent>) -> Vec<String> {
    let mut ids = Vec::new();
    for event in events {
        if !ids.contains(&event.activity_id) {
            ids.push(event.activity_id.clone());
        }
    }
    ids
}

fn correlate_candidates(
    log: &ActivityLog,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    activity_ids: Vec<String>,
    diagnostic: &'static str,
) -> Correlation {
    let candidates = activity_ids
        .iter()
        .filter_map(|activity_id| first_allowed_terminal(log, activity_id))
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return diagnostic_correlation(lifecycle, input, diagnostic);
    }
    let matched = candidates[0];
    let post_id = input.normalized_tool_use_id();
    let outcome = normalized_outcome(input.tool_response.as_ref());
    let post_already_recorded = log.events().iter().any(|event| {
        event.state == ActivityState::Outcome
            && event.activity_id == matched.activity_id
            && event.outcome == Some(outcome)
            && event.session.as_ref().is_some_and(|session| {
                session.session_id == lifecycle.identity().session_id()
                    && session.turn_id.as_deref() == lifecycle.identity().turn_id()
                    && session.tool_use_id.as_deref() == post_id.as_deref()
            })
    });
    if post_already_recorded {
        return Correlation::None;
    }
    Correlation::Outcome(outcome_event(matched, input, outcome))
}

fn first_allowed_terminal<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<&'a ActivityEvent> {
    first_allowed_terminal_with_index(log, activity_id).map(|(_, event)| event)
}

fn first_allowed_terminal_with_index<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<(usize, &'a ActivityEvent)> {
    log.events()
        .iter()
        .enumerate()
        .find(|(_, event)| event.activity_id == activity_id && event.state.is_terminal())
        .filter(|(_, event)| event.state == ActivityState::Allowed && event.decision_id.is_some())
}

fn outcome_event(
    matched: &ActivityEvent,
    input: &LifecycleActivityInput,
    outcome: ActivityOutcome,
) -> ActivityEvent {
    let mut outcome = ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: matched.kind,
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
        outcome: Some(outcome),
        correction: None,
        note: None,
        supersedes: None,
    };
    if let Some(session) = &mut outcome.session {
        session.tool_use_id = input.normalized_tool_use_id();
    }
    outcome
}

fn diagnostic_correlation(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    diagnostic: &'static str,
) -> Correlation {
    match diagnostic_event(lifecycle, input, diagnostic) {
        Ok(event) => Correlation::Diagnostic {
            event,
            message: diagnostic,
        },
        Err(_) => Correlation::None,
    }
}

fn diagnostic_event(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    diagnostic: &'static str,
) -> Result<ActivityEvent, String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    Ok(ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Diagnostic,
        activity_id: format!("orphan_{}_{}", epoch_ms(), std::process::id()),
        recorded_at_ms: epoch_ms(),
        project: ProjectEvidence {
            project_id: project_id.clone(),
            cwd: cwd.clone(),
            label: None,
        },
        session: Some(SessionTarget {
            provider: lifecycle.identity().provider(),
            session_id: lifecycle.identity().session_id().to_string(),
            turn_id: lifecycle.identity().turn_id().map(str::to_string),
            tool_use_id: input.normalized_tool_use_id(),
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
}

fn normalized_outcome(response: Option<&Value>) -> ActivityOutcome {
    let Some(Value::Object(response)) = response else {
        return ActivityOutcome::Completed;
    };
    let status = response.get("status").and_then(Value::as_str);
    if response.get("cancelled").and_then(Value::as_bool) == Some(true)
        || matches!(status, Some("cancelled" | "canceled"))
    {
        ActivityOutcome::Cancelled
    } else if response.get("is_error").and_then(Value::as_bool) == Some(true)
        || response
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        || response.get("success").and_then(Value::as_bool) == Some(false)
        || matches!(status, Some("failed" | "error"))
    {
        ActivityOutcome::Failed
    } else if response.get("exit_code").and_then(Value::as_i64) == Some(0)
        || response.get("success").and_then(Value::as_bool) == Some(true)
        || response.get("is_error").and_then(Value::as_bool) == Some(false)
        || matches!(status, Some("succeeded" | "success"))
    {
        ActivityOutcome::Succeeded
    } else {
        ActivityOutcome::Completed
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
    use std::fs::{self, OpenOptions};
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
        MAX_ACTIVITY_FIELD_BYTES, ProjectEvidence, SessionTarget, bounded_redacted_activity_text,
    };
    use coding_brain_core::lifecycle::{LifecycleStore, StoreCondition};
    use coding_brain_core::project::ProjectId;
    use fs2::FileExt;

    use crate::brain::activity::ActivityStore;

    use super::*;

    const PROMPT: &[u8] = include_bytes!("../tests/fixtures/hooks/user-prompt-submit.json");

    fn decision_event(
        cwd: &Path,
        activity_id: &str,
        recorded_at_ms: u64,
        tool_use_id: Option<&str>,
        command: &str,
        state: ActivityState,
    ) -> ActivityEvent {
        let project_id = ProjectId::Temporary("project-1".into());
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: activity_id.into(),
            recorded_at_ms,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: cwd.to_path_buf(),
                label: Some("project".into()),
            },
            session: Some(SessionTarget {
                session_id: "session-1".into(),
                turn_id: Some("turn-1".into()),
                tool_use_id: tool_use_id.map(str::to_owned),
                project_id,
                cwd: cwd.to_path_buf(),
                provider_hints: Vec::new(),
            }),
            state,
            tool: Some("Bash".into()),
            normalized_command: Some(bounded_redacted_activity_text(command)),
            fingerprint: None,
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.6),
            reasoning: Some("safe".into()),
            decision_id: Some(format!("decision-{activity_id}")),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn hook_payload(
        cwd: &Path,
        event: &str,
        call: &str,
        command: &str,
        response: Option<Value>,
    ) -> Value {
        let mut value = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": cwd,
            "hook_event_name": event,
            "tool_name": "Bash",
            "tool_use_id": call,
            "tool_input": {"command": command}
        });
        if let Some(response) = response {
            value["tool_response"] = response;
        }
        value
    }

    fn invoke_activity_hook(
        lifecycle: &LifecycleStore,
        activity: &ActivityStore,
        payload: Value,
    ) -> String {
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(payload.to_string()),
            Vec::new(),
            &mut stderr,
            lifecycle,
            Some(activity),
        );
        String::from_utf8(stderr).unwrap()
    }

    fn outcome_and_diagnostic_counts(store: &ActivityStore) -> (usize, usize) {
        let events = store.read().unwrap().events().to_vec();
        (
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            events
                .iter()
                .filter(|event| event.kind == ActivityKind::Diagnostic)
                .count(),
        )
    }

    fn assert_diagnostics_are_metadata_only(activity: &ActivityStore, forbidden: &[&str]) {
        let events = activity.read().unwrap().events().to_vec();
        for event in events
            .iter()
            .filter(|event| event.kind == ActivityKind::Diagnostic)
        {
            assert!(event.normalized_command.is_none());
            assert!(event.fingerprint.is_none());
            assert!(event.note.is_none());
        }
        let serialized = serde_json::to_string(&events).unwrap();
        for value in forbidden {
            assert!(
                !serialized.contains(value),
                "persisted forbidden value: {value}"
            );
        }
    }

    #[test]
    fn outcome_classification_requires_explicit_structured_evidence() {
        let cases = [
            (
                serde_json::json!("opaque unified-exec response"),
                ActivityOutcome::Completed,
            ),
            (
                serde_json::json!({"exit_code": 0}),
                ActivityOutcome::Succeeded,
            ),
            (
                serde_json::json!({"success": true}),
                ActivityOutcome::Succeeded,
            ),
            (serde_json::json!({"exit_code": 7}), ActivityOutcome::Failed),
            (
                serde_json::json!({"is_error": true}),
                ActivityOutcome::Failed,
            ),
            (
                serde_json::json!({"cancelled": true}),
                ActivityOutcome::Cancelled,
            ),
            (
                serde_json::json!({"status": "cancelled"}),
                ActivityOutcome::Cancelled,
            ),
            (
                serde_json::json!({"message": "done"}),
                ActivityOutcome::Completed,
            ),
        ];
        for (response, expected) in cases {
            assert_eq!(normalized_outcome(Some(&response)), expected);
        }
    }

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
        let newer = br#"{"schema_version":3}"#;
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
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
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
            "tool_input": {"command": "cargo test"},
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
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[2].activity_id, "activity-1");
        assert_eq!(events[2].decision_id.as_deref(), Some("decision-1"));
        assert_eq!(events[2].state, ActivityState::Outcome);
        assert_eq!(events[2].outcome, Some(ActivityOutcome::Succeeded));
        assert!(events[2].normalized_command.is_none());
    }

    #[test]
    fn exact_id_retries_are_idempotent_and_upgrade_v1_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let mut decision = decision_event(
            temp.path(),
            "activity-v1",
            1,
            Some("call-1"),
            "cargo test",
            ActivityState::Allowed,
        );
        decision.schema_version = 1;
        fs::write(
            &activity_path,
            format!("{}\n", serde_json::to_string(&decision).unwrap()),
        )
        .unwrap();
        let post = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "ignored by exact matching",
            Some(serde_json::json!({"exit_code": 0})),
        );

        assert!(invoke_activity_hook(&lifecycle, &activity, post.clone()).is_empty());
        assert!(invoke_activity_hook(&lifecycle, &activity, post).is_empty());

        let events = activity.read().unwrap().events().to_vec();
        let outcomes = events
            .iter()
            .filter(|event| event.state == ActivityState::Outcome)
            .collect::<Vec<_>>();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].activity_id, "activity-v1");
        assert_eq!(outcomes[0].schema_version, ACTIVITY_SCHEMA_VERSION);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            2
        );
    }

    #[test]
    fn review_regression_exact_identity_is_ambiguous_before_eligibility_filtering() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append(decision_event(
                temp.path(),
                "activity-allowed",
                1,
                Some("call-1"),
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        activity
            .append(decision_event(
                temp.path(),
                "activity-denied",
                2,
                Some("call-1"),
                "cargo test",
                ActivityState::Denied,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn post_tool_use_does_not_join_another_providers_same_native_ids() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "claude-activity".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Claude,
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
                decision_id: Some("decision-claude".into()),
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

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            Vec::new(),
            &lifecycle,
            Some(&activity),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].activity_id, "claude-activity");
    }

    #[test]
    fn post_tool_use_ignores_newer_lifecycle_observation_when_joining_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
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
        let observation = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1"
        });
        let mut observation_stderr = Vec::new();
        run_with_activity(
            Cursor::new(observation.to_string()),
            Vec::new(),
            &mut observation_stderr,
            &lifecycle,
            Some(&activity),
        );
        assert!(observation_stderr.is_empty());
        let before_outcome = activity.read().unwrap().events().to_vec();
        assert_eq!(before_outcome.len(), 2);
        assert_eq!(before_outcome[0].kind, ActivityKind::Decision);
        assert_eq!(before_outcome[1].kind, ActivityKind::Lifecycle);

        let outcome = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"exit_code": 0}
        });
        let mut outcome_stderr = Vec::new();
        run_with_activity(
            Cursor::new(outcome.to_string()),
            Vec::new(),
            &mut outcome_stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(outcome_stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].kind, ActivityKind::Lifecycle);
        assert_eq!(events[2].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[3].activity_id, "activity-1");
        assert_eq!(events[3].kind, ActivityKind::Decision);
        assert_eq!(events[3].decision_id.as_deref(), Some("decision-1"));
        assert_eq!(events[3].state, ActivityState::Outcome);
        assert_eq!(events[3].outcome, Some(ActivityOutcome::Succeeded));
    }

    #[test]
    fn post_tool_use_without_decision_activity_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        assert!(
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-orphan", "cargo test", None),
            )
            .is_empty()
        );
        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-orphan",
                "cargo test",
                Some(serde_json::json!({"exit_code": 1})),
            ),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ActivityKind::Lifecycle);
        assert_eq!(events[0].state, ActivityState::Abstained);
        assert!(events[0].decision_id.is_none());
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
    }

    #[test]
    fn no_decision_review_lossy_post_is_observation_only() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PreToolUse",
                "call-1",
                "curl --token alpha",
                None,
            ),
        );

        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "curl --token alpha",
                Some(serde_json::json!("opaque response")),
            ),
        );

        assert!(stderr.is_empty());
        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 0));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque response")
        );
    }

    #[test]
    fn no_decision_review_missing_anchor_is_metadata_only_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque response")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["opaque response"]);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
    }

    #[test]
    fn post_tool_use_with_incomplete_decision_activity_is_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                }),
                state: ActivityState::Observed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
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

        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("orphan outcome")
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, ActivityKind::Decision);
        assert_eq!(events[0].state, ActivityState::Observed);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[2].kind, ActivityKind::Diagnostic);
        assert_eq!(events[2].state, ActivityState::Error);
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
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: None,
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
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
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[2].state, ActivityState::Error);
        assert_ne!(events[2].activity_id, "activity-1");
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("no tool_use_id")
        );
    }

    #[test]
    fn post_tool_use_falls_back_within_unique_pre_interval() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        assert!(
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
            )
            .is_empty()
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();

        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque unified-exec response")),
            ),
        );

        assert!(stderr.is_empty(), "{stderr}");
        let events = activity.read().unwrap().events().to_vec();
        let outcome = events
            .iter()
            .find(|event| event.state == ActivityState::Outcome)
            .unwrap();
        assert_eq!(outcome.activity_id, "activity-1");
        assert_eq!(outcome.schema_version, ACTIVITY_SCHEMA_VERSION);
        assert_eq!(outcome.outcome, Some(ActivityOutcome::Completed));
        assert_eq!(
            outcome.session.as_ref().unwrap().tool_use_id.as_deref(),
            Some("call-1")
        );
        assert!(events.iter().any(|event| {
            event.kind == ActivityKind::Lifecycle && event.tool.as_deref() == Some("PostToolUse")
        }));
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque unified-exec response")
        );
    }

    #[test]
    fn interleaved_pre_tools_do_not_guess() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-2", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                3,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["done"]);
    }

    #[test]
    fn terminal_after_next_pre_is_outside_the_fallback_interval() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Observed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-2", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                4,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
    }

    #[test]
    fn review_regression_fallback_requires_matching_metadata_on_terminal_row() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        let mut observed = decision_event(
            temp.path(),
            "activity-1",
            2,
            None,
            "cargo test",
            ActivityState::Observed,
        );
        observed.decision_id = None;
        activity.append(observed).unwrap();
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                3,
                None,
                "cargo check",
                ActivityState::Allowed,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn candidate_losslessness_rejects_v1_fallback_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        let mut decision = decision_event(
            temp.path(),
            "activity-v1",
            2,
            None,
            "cargo test",
            ActivityState::Allowed,
        );
        decision.schema_version = 1;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&activity_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&decision).unwrap()).unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn repeated_identical_decisions_are_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        for (index, id) in ["activity-a", "activity-b"].into_iter().enumerate() {
            activity
                .append(decision_event(
                    temp.path(),
                    id,
                    index as u64 + 2,
                    None,
                    "cargo test",
                    ActivityState::Allowed,
                ))
                .unwrap();
        }
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["done"]);
    }

    #[test]
    fn lossy_commands_do_not_correlate() {
        for (decision_command, post_command) in [
            (
                "curl --token alpha".to_string(),
                "curl --token beta".to_string(),
            ),
            (
                format!("{}a", "x".repeat(MAX_ACTIVITY_FIELD_BYTES)),
                format!("{}b", "x".repeat(MAX_ACTIVITY_FIELD_BYTES)),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", &decision_command, None),
            );
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    2,
                    None,
                    &decision_command,
                    ActivityState::Allowed,
                ))
                .unwrap();
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    &post_command,
                    Some(serde_json::json!("done")),
                ),
            );
            assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
            assert_diagnostics_are_metadata_only(
                &activity,
                &[post_command.as_str(), "alpha", "beta", "done"],
            );
        }
    }

    #[test]
    fn duplicate_post_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let post = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!("done")),
        );
        invoke_activity_hook(&lifecycle, &activity, post.clone());
        invoke_activity_hook(&lifecycle, &activity, post);
        assert_eq!(outcome_and_diagnostic_counts(&activity).0, 1);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            2
        );
    }

    #[test]
    fn equivalent_evidence_appends_changed_outcome_and_dedupes_retry() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                1,
                Some("call-1"),
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let completed = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!("opaque response")),
        );
        let failed = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!({"exit_code": 7})),
        );

        invoke_activity_hook(&lifecycle, &activity, completed);
        invoke_activity_hook(&lifecycle, &activity, failed.clone());
        invoke_activity_hook(&lifecycle, &activity, failed);

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.outcome)
                .collect::<Vec<_>>(),
            [ActivityOutcome::Completed, ActivityOutcome::Failed]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            3
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque response")
        );
    }

    #[test]
    fn non_allowed_terminal_states_never_receive_outcomes() {
        for state in [
            ActivityState::Denied,
            ActivityState::Abstained,
            ActivityState::Error,
        ] {
            for tool_use_id in [None, Some("call-1")] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                invoke_activity_hook(
                    &lifecycle,
                    &activity,
                    hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
                );
                activity
                    .append(decision_event(
                        temp.path(),
                        "activity-1",
                        2,
                        tool_use_id,
                        "cargo test",
                        state,
                    ))
                    .unwrap();
                invoke_activity_hook(
                    &lifecycle,
                    &activity,
                    hook_payload(
                        temp.path(),
                        "PostToolUse",
                        "call-1",
                        "cargo test",
                        Some(serde_json::json!("done")),
                    ),
                );
                assert_eq!(outcome_and_diagnostic_counts(&activity).0, 0);
            }
        }
    }

    #[test]
    fn first_terminal_state_controls_outcome_eligibility() {
        for (first, second, expected) in [
            (ActivityState::Allowed, ActivityState::Denied, 1),
            (ActivityState::Denied, ActivityState::Allowed, 0),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
            );
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    2,
                    None,
                    "cargo test",
                    first,
                ))
                .unwrap();
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    3,
                    None,
                    "cargo test",
                    second,
                ))
                .unwrap();
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!("done")),
                ),
            );
            assert_eq!(outcome_and_diagnostic_counts(&activity).0, expected);
        }
    }

    #[test]
    fn oversized_ids_use_the_same_bounded_comparison_form() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let call = "c".repeat(MAX_ACTIVITY_FIELD_BYTES + 100);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", &call, "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                &call,
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .filter_map(|event| event.session.as_ref())
                .filter_map(|session| session.tool_use_id.as_ref())
                .all(|id| id.len() <= MAX_ACTIVITY_FIELD_BYTES)
        );
    }

    #[test]
    fn concurrent_duplicate_post_appends_one_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle_path = temp.path().join("lifecycle");
        let activity_path = temp.path().join("activity.jsonl");
        let lifecycle = LifecycleStore::at(&lifecycle_path);
        let activity = ActivityStore::at(&activity_path);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let cwd = temp.path().to_path_buf();
                let lifecycle_path = lifecycle_path.clone();
                let activity_path = activity_path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    invoke_activity_hook(
                        &LifecycleStore::at(lifecycle_path),
                        &ActivityStore::at(activity_path),
                        hook_payload(
                            &cwd,
                            "PostToolUse",
                            "call-1",
                            "cargo test",
                            Some(serde_json::json!("done")),
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert!(handle.join().unwrap().is_empty());
        }
        assert_eq!(outcome_and_diagnostic_counts(&activity).0, 1);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            2
        );
    }

    #[test]
    fn large_log_correlates_only_the_tail_decision() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append_from_snapshot(|_| {
                (0..10_000)
                    .map(|index| {
                        decision_event(
                            temp.path(),
                            &format!("irrelevant-{index}"),
                            index,
                            None,
                            "other command",
                            ActivityState::Allowed,
                        )
                    })
                    .collect()
            })
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-tail", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-tail",
                10_002,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-tail",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        let log = activity.read().unwrap();
        let outcomes = log
            .events()
            .iter()
            .filter(|event| event.state == ActivityState::Outcome)
            .map(|event| event.activity_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(outcomes, ["activity-tail"]);
    }

    #[test]
    fn activity_storage_failures_are_bounded_and_fail_open() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let lock_path = activity_path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!("secret response")),
                )
                .to_string(),
            ),
            &mut stdout,
            &mut stderr,
            &lifecycle,
            Some(&ActivityStore::at(&activity_path)),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(stderr.len() < 256);
        assert!(!String::from_utf8_lossy(&stderr).contains("secret response"));
        FileExt::unlock(&lock).unwrap();

        let blocked_path = temp.path().join("blocked-activity.jsonl");
        fs::create_dir(&blocked_path).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-2",
                    "cargo test",
                    Some(serde_json::json!("another secret response")),
                )
                .to_string(),
            ),
            &mut stdout,
            &mut stderr,
            &lifecycle,
            Some(&ActivityStore::at(blocked_path)),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(stderr.len() < 256);
        assert!(!String::from_utf8_lossy(&stderr).contains("another secret response"));
    }
}
