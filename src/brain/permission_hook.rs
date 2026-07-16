#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::client::BrainSuggestion;
use super::decisions::{HookDecisionAudit, log_hook_decision};
use super::query::{self, BrainDecision, BrainDecisionRequest};
use crate::config::BrainConfig;

const HOOK_INFERENCE_TIMEOUT_MS: u64 = 25_000;

#[derive(Debug, Deserialize)]
struct PermissionRequestInput {
    session_id: String,
    turn_id: String,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_input: PermissionToolInput,
}

#[derive(Debug, Deserialize)]
struct PermissionToolInput {
    command: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PermissionRequest {
    session_id: String,
    turn_id: String,
    cwd: String,
    project: String,
    tool_name: String,
    command: String,
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
    let parsed: PermissionRequestInput = serde_json::from_str(input).map_err(|error| {
        HookDiagnostic::new(format!("invalid PermissionRequest payload: {error}"))
    })?;
    if parsed.hook_event_name != "PermissionRequest" {
        return Err(HookDiagnostic::new(format!(
            "unsupported hook event: {}",
            parsed.hook_event_name
        )));
    }
    for (field, value) in [
        ("session_id", parsed.session_id.as_str()),
        ("turn_id", parsed.turn_id.as_str()),
        ("cwd", parsed.cwd.as_str()),
        ("tool_name", parsed.tool_name.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(HookDiagnostic::new(format!(
                "PermissionRequest field {field} must not be empty"
            )));
        }
    }
    if parsed.tool_name != "Bash" {
        return Err(HookDiagnostic::new(format!(
            "unsupported PermissionRequest tool: {}",
            parsed.tool_name
        )));
    }
    if parsed.tool_input.command.trim().is_empty() {
        return Err(HookDiagnostic::new(
            "PermissionRequest field tool_input.command must not be empty",
        ));
    }

    let cwd = parsed.cwd.trim().to_string();
    let project = Path::new(&cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&cwd)
        .to_string();
    Ok(PermissionRequest {
        session_id: parsed.session_id,
        turn_id: parsed.turn_id,
        cwd,
        project,
        tool_name: parsed.tool_name,
        command: parsed.tool_input.command,
    })
}

fn handle_with<F>(
    input: &str,
    config: Option<&BrainConfig>,
    gate_mode: &str,
    infer: F,
) -> Result<Option<HookDecision>, HookDiagnostic>
where
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let request = parse_request(input)?;
    let Some(config) = config.filter(|config| config.enabled) else {
        return Ok(None);
    };
    if gate_mode == "off" {
        return Ok(None);
    }

    let mut hook_config = config.clone();
    hook_config.timeout_ms = hook_config.timeout_ms.min(HOOK_INFERENCE_TIMEOUT_MS);
    let brain = query::evaluate_with(
        &BrainDecisionRequest {
            project: request.project.clone(),
            tool_name: request.tool_name.clone(),
            tool_input: request.command.clone(),
            diff_digest: None,
        },
        &hook_config,
        gate_mode,
        infer,
    );
    if brain.source == "error" {
        return Err(HookDiagnostic::new(brain.reasoning.clone()));
    }
    if brain.source != "brain" || brain.below_threshold != Some(false) {
        return Ok(None);
    }
    let behavior = match brain.action.as_str() {
        "approve" => PermissionBehavior::Allow,
        "deny" => PermissionBehavior::Deny,
        _ => return Ok(None),
    };
    Ok(Some(HookDecision {
        request,
        brain,
        behavior,
    }))
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let _ = writeln!(stderr, "codexctl permission hook: {diagnostic}");
}

fn run_with_gate<R, W, E, F>(
    mut stdin: R,
    mut stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: &str,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let mut input = String::new();
    if let Err(error) = stdin.read_to_string(&mut input) {
        write_diagnostic(&mut stderr, format!("could not read stdin: {error}"));
        return;
    }
    let decision = match handle_with(&input, config, gate_mode, infer) {
        Ok(Some(decision)) => decision,
        Ok(None) => return,
        Err(diagnostic) => {
            write_diagnostic(&mut stderr, diagnostic);
            return;
        }
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
        command: &decision.request.command,
        brain_action: &decision.brain.action,
        brain_confidence: decision.brain.confidence,
        brain_reasoning: &decision.brain.reasoning,
        brain_source: decision.brain.source,
        brain_threshold: decision.brain.threshold,
        user_action: decision.behavior.user_action(),
        session_id: &decision.request.session_id,
        turn_id: &decision.request.turn_id,
    };
    if let Err(error) = log_hook_decision(&audit) {
        write_diagnostic(
            &mut stderr,
            format!("could not persist prepared decision: {error}"),
        );
        return;
    }
    if let Err(error) = stdout.write_all(&serialized) {
        write_diagnostic(&mut stderr, format!("could not write response: {error}"));
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::brain::client::BrainSuggestion;
    use crate::config::BrainConfig;
    use crate::rules::RuleAction;

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

    #[test]
    fn parses_valid_bash_permission_request() {
        let request = parse_request(&payload()).unwrap();
        assert_eq!(request.session_id, "session-1");
        assert_eq!(request.turn_id, "turn-1");
        assert_eq!(request.cwd, "/work/codexctl");
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(request.command, "cargo test");
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
    fn rejects_non_bash_tool() {
        let input = payload().replace("\"Bash\"", "\"apply_patch\"");
        assert!(parse_request(&input).is_err());
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
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            run_with_gate(
                Cursor::new(input.to_string()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                "on",
                |_, _| panic!("empty command must not reach inference"),
            );

            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
        }
    }

    #[test]
    fn preserves_exact_nonempty_command() {
        let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        input["tool_input"]["command"] = serde_json::json!("  cargo test --lib  ");

        let request = parse_request(&input.to_string()).unwrap();

        assert_eq!(request.command, "  cargo test --lib  ");
    }

    #[test]
    fn confident_approve_emits_allow_after_persisting() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
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
        let log =
            std::fs::read_to_string(home.path().join(".codexctl/brain/decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["project"], "codexctl");
        assert_eq!(record["tool"], "Bash");
        assert_eq!(record["command"], "cargo test");
        assert_eq!(record["brain_action"], "approve");
        assert_eq!(record["brain_source"], "brain");
        assert_eq!(record["user_action"], "hook_allow");
        assert_eq!(record["session_id"], "session-1");
        assert_eq!(record["turn_id"], "turn-1");
    }

    #[test]
    fn confident_deny_emits_deny() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| Ok(suggestion(RuleAction::Deny, 0.9)),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        let log =
            std::fs::read_to_string(home.path().join(".codexctl/brain/decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["user_action"], "hook_deny");
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
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with_gate(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&config),
                gate_mode,
                |_, _| inference,
            );
            assert!(stdout.is_empty(), "fallthrough wrote stdout");
        }

        let mut disabled = enabled_config();
        disabled.enabled = false;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&disabled),
            |_, _| panic!("disabled hook must not infer"),
        );
        assert!(stdout.is_empty());
    }

    #[test]
    fn malformed_payload_leaves_stdout_empty() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(
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
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let home_file = dir.path().join("not-a-directory");
        std::fs::write(&home_file, "occupied").unwrap();
        let _restore_home = set_test_home(&home_file);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with(
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
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with(
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
        let log =
            std::fs::read_to_string(home.path().join(".codexctl/brain/decisions.jsonl")).unwrap();
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
