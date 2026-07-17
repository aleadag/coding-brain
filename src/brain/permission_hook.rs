#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use codexctl_core::lifecycle::{
    LifecycleEvent, LifecycleIdentity, LifecycleStore, PermissionDisposition,
    compatibility_state_root,
};

use super::client::BrainSuggestion;
use super::decisions::{HookDecisionAudit, log_hook_decision};
use super::query::{self, BrainDecision, BrainDecisionRequest};
use crate::config::BrainConfig;
use crate::lifecycle_hook::read_bounded_hook_input;

const HOOK_INFERENCE_TIMEOUT_MS: u64 = 25_000;

#[derive(Debug, Deserialize)]
struct PermissionRequestInput {
    session_id: String,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_input: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionRequest {
    lifecycle: LifecycleIdentity,
    project: String,
    tool_name: String,
    command: Option<String>,
}

#[derive(Debug)]
struct HookDiagnostic(String);

impl HookDiagnostic {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HookDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum PermissionBehavior {
    Allow,
    Deny,
}

impl PermissionBehavior {
    fn user_action(self) -> &'static str {
        match self {
            Self::Allow => "hook_allow",
            Self::Deny => "hook_deny",
        }
    }
}

#[derive(Debug)]
struct HookDecision {
    request: PermissionRequest,
    brain: BrainDecision,
    behavior: PermissionBehavior,
}

#[derive(Serialize)]
struct HookResponse<'a> {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput<'a>,
}

#[derive(Serialize)]
struct HookSpecificOutput<'a> {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    decision: HookResponseDecision<'a>,
}

#[derive(Serialize)]
struct HookResponseDecision<'a> {
    behavior: PermissionBehavior,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
}

fn parse_request(input: &str) -> Result<PermissionRequest, HookDiagnostic> {
    let parsed: PermissionRequestInput = serde_json::from_str(input)
        .map_err(|_| HookDiagnostic::new("invalid PermissionRequest payload"))?;
    if parsed.hook_event_name != "PermissionRequest" {
        return Err(HookDiagnostic::new("unsupported hook event"));
    }
    for (field, value) in [
        ("session_id", Some(parsed.session_id.as_str())),
        ("turn_id", parsed.turn_id.as_deref()),
        ("cwd", Some(parsed.cwd.as_str())),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(HookDiagnostic::new(format!(
                "PermissionRequest field {field} must not be empty"
            )));
        }
    }
    if parsed.tool_name.trim().is_empty() {
        return Err(HookDiagnostic::new(
            "PermissionRequest field tool_name must not be empty",
        ));
    }
    let lifecycle = LifecycleIdentity::try_new(
        parsed.session_id,
        parsed.turn_id,
        parsed.transcript_path,
        PathBuf::from(parsed.cwd),
    )
    .map_err(|error| HookDiagnostic::new(format!("invalid PermissionRequest identity: {error}")))?;
    if lifecycle.turn_id().is_none() {
        return Err(HookDiagnostic::new(
            "PermissionRequest field turn_id must not be empty",
        ));
    }
    let command = if parsed.tool_name == "Bash" {
        let command = parsed
            .tool_input
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.trim().is_empty())
            .ok_or_else(|| {
                HookDiagnostic::new("PermissionRequest field tool_input.command must not be empty")
            })?;
        Some(command.to_string())
    } else {
        None
    };
    let project = lifecycle
        .cwd()
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| lifecycle.cwd().to_string_lossy().into_owned());
    Ok(PermissionRequest {
        lifecycle,
        project,
        tool_name: parsed.tool_name,
        command,
    })
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let _ = writeln!(stderr, "codexctl permission hook: {diagnostic}");
}

fn record_permission(
    store: &LifecycleStore,
    identity: &LifecycleIdentity,
    disposition: PermissionDisposition,
) -> Result<(), HookDiagnostic> {
    let event = LifecycleEvent::permission(identity.clone(), disposition)
        .map_err(|error| HookDiagnostic::new(format!("invalid lifecycle event: {error}")))?;
    store
        .record(event)
        .map(|_| ())
        .map_err(|error| HookDiagnostic::new(format!("could not persist lifecycle state: {error}")))
}

fn run_with_gate_and_store<R, W, E, F>(
    stdin: R,
    mut stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: &str,
    store: &LifecycleStore,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let input = match read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let input = match std::str::from_utf8(&input) {
        Ok(input) => input,
        Err(_) => {
            write_diagnostic(&mut stderr, "invalid PermissionRequest payload");
            return;
        }
    };
    let request = match parse_request(input) {
        Ok(request) => request,
        Err(diagnostic) => {
            write_diagnostic(&mut stderr, diagnostic);
            return;
        }
    };
    let needs_input = |stderr: &mut E| {
        if let Err(error) =
            record_permission(store, &request.lifecycle, PermissionDisposition::NeedsInput)
        {
            write_diagnostic(stderr, error);
        }
    };
    let Some(command) = request.command.as_deref() else {
        needs_input(&mut stderr);
        return;
    };
    let Some(config) = config.filter(|config| config.enabled) else {
        needs_input(&mut stderr);
        return;
    };
    if gate_mode == "off" {
        needs_input(&mut stderr);
        return;
    }

    let mut hook_config = config.clone();
    hook_config.timeout_ms = hook_config.timeout_ms.min(HOOK_INFERENCE_TIMEOUT_MS);
    let brain = query::evaluate_with(
        &BrainDecisionRequest {
            project: request.project.clone(),
            tool_name: request.tool_name.clone(),
            tool_input: command.to_string(),
            diff_digest: None,
        },
        &hook_config,
        gate_mode,
        infer,
    );
    if brain.source == "error" {
        write_diagnostic(&mut stderr, &brain.reasoning);
        needs_input(&mut stderr);
        return;
    }
    if brain.source != "brain" || brain.below_threshold != Some(false) {
        needs_input(&mut stderr);
        return;
    }
    let behavior = match brain.action.as_str() {
        "approve" => PermissionBehavior::Allow,
        "deny" => PermissionBehavior::Deny,
        _ => {
            needs_input(&mut stderr);
            return;
        }
    };
    let decision = HookDecision {
        request,
        brain,
        behavior,
    };

    // Serialize first so a serialization error can never leave a prepared
    // audit record without a response ready to write.
    let response = HookResponse {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: "PermissionRequest",
            decision: HookResponseDecision {
                behavior: decision.behavior,
                message: decision.brain.message.as_deref(),
            },
        },
    };
    let serialized = match serde_json::to_vec(&response) {
        Ok(serialized) => serialized,
        Err(error) => {
            write_diagnostic(
                &mut stderr,
                format!("could not serialize response: {error}"),
            );
            return;
        }
    };

    let audit = HookDecisionAudit {
        project: &decision.request.project,
        tool: &decision.request.tool_name,
        command: decision.request.command.as_deref().unwrap_or_default(),
        brain_action: &decision.brain.action,
        brain_confidence: decision.brain.confidence,
        brain_reasoning: &decision.brain.reasoning,
        brain_source: decision.brain.source,
        brain_threshold: decision.brain.threshold,
        user_action: decision.behavior.user_action(),
        session_id: decision.request.lifecycle.session_id(),
        turn_id: decision.request.lifecycle.turn_id().unwrap_or_default(),
    };
    if let Err(error) = log_hook_decision(&audit) {
        write_diagnostic(
            &mut stderr,
            format!("could not persist prepared decision: {error}"),
        );
        return;
    }
    if let Err(error) = record_permission(
        store,
        &decision.request.lifecycle,
        PermissionDisposition::Decided,
    ) {
        write_diagnostic(&mut stderr, error);
    }
    if let Err(error) = stdout.write_all(&serialized) {
        write_diagnostic(&mut stderr, format!("could not write response: {error}"));
    }
}

fn run_with_gate<R, W, E, F>(
    stdin: R,
    stdout: W,
    stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: &str,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let store = LifecycleStore::at(compatibility_state_root());
    run_with_gate_and_store(stdin, stdout, stderr, config, gate_mode, &store, infer);
}

fn run_with<R, W, E, F>(stdin: R, stdout: W, stderr: E, config: Option<&BrainConfig>, infer: F)
where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    run_with_gate(
        stdin,
        stdout,
        stderr,
        config,
        &super::read_gate_mode(),
        infer,
    );
}

pub(crate) fn run(config: Option<&BrainConfig>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_with(
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        config,
        super::client::infer,
    );
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::brain::client::BrainSuggestion;
    use crate::brain::decisions::decisions_dir;
    use crate::config::BrainConfig;
    use crate::rules::RuleAction;
    use codexctl_core::lifecycle::{LifecycleStore, ProjectedStatus};

    struct RestoreHome(Option<OsString>);

    impl Drop for RestoreHome {
        fn drop(&mut self) {
            // SAFETY: every test that changes HOME holds HOME_ENV_LOCK.
            unsafe {
                match self.0.take() {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    fn set_test_home(path: &Path) -> RestoreHome {
        let original = std::env::var_os("HOME");
        // SAFETY: every caller holds HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", path) };
        RestoreHome(original)
    }

    fn payload() -> String {
        serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": "/work/codexctl",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" }
        })
        .to_string()
    }

    fn suggestion(action: RuleAction, confidence: f64) -> BrainSuggestion {
        BrainSuggestion {
            action,
            message: Some("reviewed by brain".into()),
            reasoning: "test reasoning".into(),
            confidence,
            suggested_at: 123,
        }
    }

    fn enabled_config() -> BrainConfig {
        BrainConfig {
            enabled: true,
            timeout_ms: 60_000,
            ..BrainConfig::default()
        }
    }

    fn run_test_with_gate<R, W, E, F>(
        stdin: R,
        stdout: W,
        stderr: E,
        config: Option<&BrainConfig>,
        gate_mode: &str,
        infer: F,
    ) where
        R: Read,
        W: Write,
        E: Write,
        F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
    {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        run_with_gate_and_store(stdin, stdout, stderr, config, gate_mode, &store, infer);
    }

    fn run_test<R, W, E, F>(stdin: R, stdout: W, stderr: E, config: Option<&BrainConfig>, infer: F)
    where
        R: Read,
        W: Write,
        E: Write,
        F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
    {
        run_test_with_gate(stdin, stdout, stderr, config, "on", infer);
    }

    fn projected_status(store: &LifecycleStore) -> Option<ProjectedStatus> {
        store.read().unwrap().snapshot.unwrap().sessions["session-1"].projected_status
    }

    #[test]
    fn parses_valid_bash_permission_request() {
        let request = parse_request(&payload()).unwrap();
        assert_eq!(request.lifecycle.session_id(), "session-1");
        assert_eq!(request.lifecycle.turn_id(), Some("turn-1"));
        assert_eq!(request.lifecycle.cwd(), Path::new("/work/codexctl"));
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(request.command.as_deref(), Some("cargo test"));
        assert_eq!(request.project, "codexctl");
    }

    #[test]
    fn rejects_wrong_event() {
        let input = payload().replace("PermissionRequest", "PreToolUse");
        assert!(parse_request(&input).is_err());
    }

    #[test]
    fn rejects_empty_identity_fields() {
        for field in ["session_id", "turn_id", "cwd", "tool_name"] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input[field] = serde_json::json!("   ");
            assert!(
                parse_request(&input.to_string()).is_err(),
                "accepted empty {field}"
            );
        }
    }

    #[test]
    fn non_bash_records_needs_input_without_inference_or_response() {
        let home = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(home.path().join(".codexctl"));
        let input = payload().replace("\"Bash\"", "\"apply_patch\"");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(input),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            "on",
            &store,
            |_, _| panic!("non-Bash permission must not reach inference"),
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().sessions["session-1"].projected_status,
            Some(ProjectedStatus::NeedsInput)
        );
    }

    #[test]
    fn oversized_permission_input_never_infers_audits_or_persists() {
        let home = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(home.path().join(".codexctl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(vec![b'x'; 65_537]),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            "on",
            &store,
            |_, _| panic!("oversized permission must not reach inference"),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(!store.snapshot_path().exists());
        assert!(!decisions_dir().join("decisions.jsonl").exists());
    }

    #[test]
    fn lifecycle_failure_does_not_change_valid_decision_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let healthy = LifecycleStore::at(temp.path().join("healthy"));
        let blocked_root = temp.path().join("blocked");
        std::fs::write(&blocked_root, b"occupied").unwrap();
        let blocked = LifecycleStore::at(blocked_root);

        let mut healthy_stdout = Vec::new();
        let mut healthy_stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut healthy_stdout,
            &mut healthy_stderr,
            Some(&enabled_config()),
            "on",
            &healthy,
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );
        let mut failed_stdout = Vec::new();
        let mut failed_stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut failed_stdout,
            &mut failed_stderr,
            Some(&enabled_config()),
            "on",
            &blocked,
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert_eq!(failed_stdout, healthy_stdout);
        assert!(healthy_stderr.is_empty());
        assert!(
            String::from_utf8(failed_stderr)
                .unwrap()
                .contains("lifecycle")
        );
    }

    #[test]
    fn rejects_missing_or_non_string_command() {
        for tool_input in [serde_json::json!({}), serde_json::json!({"command": 7})] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input["tool_input"] = tool_input;
            assert!(parse_request(&input.to_string()).is_err());
        }
    }

    #[test]
    fn empty_or_whitespace_command_falls_through_without_inference() {
        for command in ["", "  \t\n  "] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input["tool_input"]["command"] = serde_json::json!(command);
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            run_with_gate_and_store(
                Cursor::new(input.to_string()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                "on",
                &store,
                |_, _| panic!("empty command must not reach inference"),
            );

            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
            assert!(!store.snapshot_path().exists());
        }
    }

    #[test]
    fn preserves_exact_nonempty_command() {
        let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        input["tool_input"]["command"] = serde_json::json!("  cargo test --lib  ");

        let request = parse_request(&input.to_string()).unwrap();

        assert_eq!(request.command.as_deref(), Some("  cargo test --lib  "));
    }

    #[test]
    fn confident_approve_emits_allow_after_persisting() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            "on",
            &store,
            |config, _| {
                assert_eq!(config.timeout_ms, 25_000);
                Ok(suggestion(RuleAction::Approve, 0.9))
            },
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(
            output["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert!(stderr.is_empty());
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["project"], "codexctl");
        assert_eq!(record["tool"], "Bash");
        assert_eq!(record["command"], "cargo test");
        assert_eq!(record["brain_action"], "approve");
        assert_eq!(record["brain_source"], "brain");
        assert_eq!(record["user_action"], "hook_allow");
        assert_eq!(record["session_id"], "session-1");
        assert_eq!(record["turn_id"], "turn-1");
        assert_eq!(projected_status(&store), Some(ProjectedStatus::Processing));
    }

    #[test]
    fn confident_deny_emits_deny() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            "on",
            &store,
            |_, _| Ok(suggestion(RuleAction::Deny, 0.9)),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["user_action"], "hook_deny");
        assert_eq!(projected_status(&store), Some(ProjectedStatus::Processing));
    }

    #[test]
    fn fallthrough_cases_leave_stdout_empty() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());

        let cases = [
            (
                enabled_config(),
                "on",
                Ok(suggestion(RuleAction::Approve, 0.1)),
            ),
            (
                enabled_config(),
                "off",
                Ok(suggestion(RuleAction::Approve, 0.9)),
            ),
            (
                enabled_config(),
                "on",
                Ok(suggestion(RuleAction::Send, 0.9)),
            ),
            (enabled_config(), "on", Err("endpoint unavailable".into())),
        ];
        for (config, gate_mode, inference) in cases {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with_gate_and_store(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&config),
                gate_mode,
                &store,
                |_, _| inference,
            );
            assert!(stdout.is_empty(), "fallthrough wrote stdout");
            assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
        }

        let mut disabled = enabled_config();
        disabled.enabled = false;
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&disabled),
            "on",
            &store,
            |_, _| panic!("disabled hook must not infer"),
        );
        assert!(stdout.is_empty());
        assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
    }

    #[test]
    fn malformed_payload_leaves_stdout_empty() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_test(
            Cursor::new("not json"),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| panic!("malformed hook must not infer"),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
    }

    #[test]
    fn persistence_failure_leaves_stdout_empty() {
        let brain_dir = decisions_dir();
        std::fs::create_dir_all(brain_dir.parent().unwrap()).unwrap();
        std::fs::write(&brain_dir, "occupied").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_test(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("persist"));
    }

    #[test]
    fn identical_payloads_are_evaluated_independently() {
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_test(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                |_, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(suggestion(RuleAction::Approve, 0.9))
                },
            );
            assert!(!stdout.is_empty());
        }

        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let records = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        let first_id = records[0]["decision_id"].as_str().unwrap();
        let second_id = records[1]["decision_id"].as_str().unwrap();
        assert!(!first_id.is_empty());
        assert!(!second_id.is_empty());
        assert_ne!(first_id, second_id);
    }
}
