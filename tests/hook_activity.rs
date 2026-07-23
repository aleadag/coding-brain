#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use coding_brain::brain::activity::ActivityStore;
use coding_brain_core::brain_activity::{
    ActivityKind, ActivityOutcome, ActivityState, DeliveryState, MAX_ACTIVITY_FIELD_BYTES,
    SessionTarget, SnapshotLimits,
};
use coding_brain_core::lifecycle::{
    ApplyOutcome, IgnoreReason, LifecycleEvent, LifecycleEventKind, LifecycleIdentity,
    LifecycleStore, PermissionDisposition, ProjectedStatus,
};
use coding_brain_core::provider::AgentProvider;

#[test]
fn legacy_activity_target_defaults_to_codex_without_reemitting_provider_hints() {
    let target: SessionTarget = serde_json::from_value(serde_json::json!({
        "session_id": "legacy",
        "project_id": {"kind": "stable", "value": "project"},
        "cwd": "/tmp/project",
        "provider_hints": ["agent-deck"]
    }))
    .unwrap();

    assert_eq!(target.provider, AgentProvider::Codex);
    let encoded = serde_json::to_value(target).unwrap();
    assert_eq!(encoded["provider"], "codex");
    assert!(encoded.get("provider_hints").is_none());
}

fn permission_payload(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "cwd": cwd,
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": command}
    }))
    .unwrap()
}

fn pre_tool_payload(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "tool_use_id": "call-1",
        "cwd": cwd,
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command}
    }))
    .unwrap()
}

fn post_tool_payload(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "tool_use_id": "call-1",
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "tool_response": "Process exited with code 0"
    }))
    .unwrap()
}

fn run_permission_hook(home: &Path, payload: &[u8]) -> Output {
    let mut child = spawn_permission_hook(home);
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

fn run_provider_permission_hook(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command.args(["--permission-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

fn claude_permission_payload(cwd: &Path, policy: Option<&str>) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/claude-permission-request.json"
    ))
    .unwrap();
    payload["cwd"] = serde_json::json!(cwd);
    payload["provider"] = serde_json::json!("codex");
    if let Some(policy) = policy {
        payload["permission_suggestions"] = serde_json::json!([{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo test"}],
            "behavior": policy,
            "destination": "session"
        }]);
    }
    serde_json::to_vec(&payload).unwrap()
}

fn antigravity_permission_payload(cwd: &Path, policy: Option<&str>) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/antigravity-pre-tool-use.json"
    ))
    .unwrap();
    payload["workspacePaths"] = serde_json::json!([cwd]);
    payload["provider"] = serde_json::json!("claude");
    payload["hookEventName"] = serde_json::json!("PermissionRequest");
    if let Some(policy) = policy {
        payload["decision"] = serde_json::json!(policy);
        payload["permissionOverrides"] = serde_json::json!(["command(cargo test)"]);
    }
    serde_json::to_vec(&payload).unwrap()
}

fn spawn_permission_hook(home: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .arg("--permission-hook")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn isolated_path(home: &Path) -> OsString {
    let mut paths = vec![home.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

fn run_lifecycle_hook(home: &Path, payload: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .arg("--lifecycle-hook")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

fn run_provider_recovery_hook(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command.args(["--recovery-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn recovery_hook_without_trusted_live_link_publishes_no_stop() {
    let home = tempfile::tempdir().unwrap();
    let mut payload: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/hooks/antigravity-stop.json")).unwrap();
    payload["workspacePaths"] = serde_json::json!([home.path()]);

    let output = run_provider_recovery_hook(
        home.path(),
        "antigravity",
        Some("Stop"),
        &serde_json::to_vec(&payload).unwrap(),
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "coding-brain recovery hook: Stop persistence failed\n"
    );
    let lifecycle = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let view = lifecycle.read().unwrap();
    assert!(view.snapshot.is_none());
    assert!(activity(home.path()).read().unwrap().events().is_empty());
}

fn activity(home: &Path) -> ActivityStore {
    ActivityStore::at(home.join(".local/state/coding-brain/activity.jsonl"))
}

fn seed_ignored_permission(home: &Path, provider: AgentProvider, ignored_reason: IgnoreReason) {
    let (session_id, turn_id) = match provider {
        AgentProvider::Claude => ("claude-session-1", "claude-session-1"),
        AgentProvider::Antigravity => ("agy-conversation-1", "step-5"),
        AgentProvider::Codex => unreachable!(),
    };
    let identity = |turn_id: &str| {
        LifecycleIdentity::try_new(
            provider,
            session_id.into(),
            Some(turn_id.into()),
            None,
            home.to_path_buf(),
        )
        .unwrap()
    };
    let lifecycle = LifecycleStore::at(home.join(".local/state/coding-brain"));
    let record = |event| assert_eq!(lifecycle.record(event).unwrap(), ApplyOutcome::Applied);
    match ignored_reason {
        IgnoreReason::Duplicate => record(
            LifecycleEvent::permission(identity(turn_id), PermissionDisposition::Decided).unwrap(),
        ),
        IgnoreReason::RecentTurn => {
            record(
                LifecycleEvent::from_parts(identity(turn_id), LifecycleEventKind::UserPromptSubmit)
                    .unwrap(),
            );
            record(
                LifecycleEvent::from_parts(identity(turn_id), LifecycleEventKind::Stop).unwrap(),
            );
        }
        IgnoreReason::AmbiguousTurn => record(
            LifecycleEvent::from_parts(
                identity("different-open-turn"),
                LifecycleEventKind::UserPromptSubmit,
            )
            .unwrap(),
        ),
        IgnoreReason::ActiveSubagentCapacity => unreachable!(),
    }
}

fn install_model_fixture(home: &Path, action: &str) {
    install_model_fixture_with_confidence(home, action, 0.9);
}

fn install_model_fixture_with_confidence(home: &Path, action: &str, confidence: f64) {
    install_model_fixture_full(home, action, confidence, None);
}

fn install_model_fixture_full(home: &Path, action: &str, confidence: f64, message: Option<&str>) {
    let config = home.join(".config/coding-brain/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "[brain]\nenabled = true\nendpoint = \"http://brain.example.test/api/generate\"\n",
    )
    .unwrap();
    install_gate_mode_fixture(home, "auto");
    install_fake_model(home, action, confidence, message);
}

fn install_default_model_fixture(home: &Path, mode: &str, action: &str) {
    install_gate_mode_fixture(home, mode);
    install_fake_model(home, action, 0.9, None);
}

fn install_gate_mode_fixture(home: &Path, mode: &str) {
    let gate_mode = home.join(".local/state/coding-brain/brain/gate-mode");
    fs::create_dir_all(gate_mode.parent().unwrap()).unwrap();
    fs::write(gate_mode, format!("{mode}\n")).unwrap();
}

fn install_fake_model(home: &Path, action: &str, confidence: f64, message: Option<&str>) {
    let suggestion = serde_json::json!({
        "action": action,
        "message": message,
        "reasoning": "fixture decision",
        "confidence": confidence
    })
    .to_string();
    let response = serde_json::json!({"response": suggestion}).to_string();
    let bin = home.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let curl = bin.join("curl");
    fs::write(
        &curl,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > \"${{0}}.args\"\ndd of=\"${{0}}.stdin\" 2>/dev/null\nprintf '%s' '{response}'\n"
        ),
    )
    .unwrap();
    fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
}

fn assert_default_model_request(home: &Path) {
    assert!(
        !home.join(".config/coding-brain/config.toml").exists(),
        "default-model fixture unexpectedly wrote TOML"
    );
    let args = fs::read_to_string(home.join("bin/curl.args")).unwrap();
    assert!(
        args.contains("http://localhost:11434/api/generate"),
        "missing default endpoint in curl args: {args}"
    );
    let stdin = fs::read_to_string(home.join("bin/curl.stdin")).unwrap();
    assert!(
        stdin.contains("\"model\":\"gemma4:e4b\""),
        "missing default model in curl request: {stdin}"
    );
}

fn overwrite_curl(home: &Path, script: &str) {
    let curl = home.join("bin/curl");
    fs::write(&curl, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
    fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn deterministic_deny_is_delivered_when_decision_audit_is_down() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain")).unwrap();
    fs::write(
        home.path().join(".local/state/coding-brain/brain"),
        b"occupied",
    )
    .unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "rm -rf /"));

    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("audit"));
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert_eq!(
        events.iter().map(|event| event.state).collect::<Vec<_>>(),
        [
            ActivityState::Observed,
            ActivityState::Evaluating,
            ActivityState::Denied,
            ActivityState::Delivered,
        ]
    );
    let snapshot = activity(home.path())
        .snapshot(SnapshotLimits::default())
        .unwrap();
    assert!(snapshot.attention.is_empty());
    assert_eq!(snapshot.unresolved_count, 0);
    assert_eq!(snapshot.recent.len(), 1);
    assert_eq!(snapshot.recent[0].state, ActivityState::Denied);
    assert_eq!(snapshot.recent[0].delivery, DeliveryState::Delivered);
}

#[test]
fn deterministic_deny_survives_both_audits_being_down() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain")).unwrap();
    fs::write(
        home.path().join(".local/state/coding-brain/brain"),
        b"occupied",
    )
    .unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain/activity.jsonl")).unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "rm -rf /"));

    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("audit"));
}

#[test]
fn model_action_requires_proposal_and_terminal_before_delivery() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    let store = activity(home.path());
    let events = store.read().unwrap().events().to_vec();
    assert_eq!(events[2].state, ActivityState::Allowed);
    assert!(events[2].decision_id.is_some());
    assert_eq!(events[3].state, ActivityState::Delivered);
    assert_eq!(events[3].decision_id, events[2].decision_id);
    let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
    assert_eq!(snapshot.recent[0].delivery, DeliveryState::Delivered);
    assert!(!snapshot.recent[0].tool_execution_confirmed);
}

#[test]
fn claude_permission_uses_exact_schema_and_cli_provider_authority() {
    for (action, behavior) in [("approve", "allow"), ("deny", "deny")] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), action);

        let output = run_provider_permission_hook(
            home.path(),
            "claude",
            None,
            &claude_permission_payload(home.path(), None),
        );

        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": behavior}
                }
            })
        );
        let events = activity(home.path()).read().unwrap().events().to_vec();
        assert!(events.iter().all(|event| {
            event
                .session
                .as_ref()
                .is_none_or(|session| session.provider == AgentProvider::Claude)
        }));
    }
}

#[test]
fn provider_ask_or_deny_policy_never_becomes_claude_allow() {
    for policy in ["ask", "deny"] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), "approve");

        let output = run_provider_permission_hook(
            home.path(),
            "claude",
            None,
            &claude_permission_payload(home.path(), Some(policy)),
        );

        assert!(output.status.success());
        if policy == "deny" {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {"behavior": "deny"}
                    }
                })
            );
        } else {
            assert!(output.stdout.is_empty());
        }
    }
}

#[test]
fn provider_ask_policy_preserves_claude_model_deny() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "deny");

    let output = run_provider_permission_hook(
        home.path(),
        "claude",
        None,
        &claude_permission_payload(home.path(), Some("ask")),
    );

    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny"}
            }
        })
    );
}

#[test]
fn antigravity_permission_uses_exact_decisions_without_forbidden_overrides() {
    for (action, decision) in [("approve", "allow"), ("deny", "deny")] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), action);

        let output = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &antigravity_permission_payload(home.path(), None),
        );

        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({"decision": decision})
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("force_ask"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("permissionOverrides"));
        let events = activity(home.path()).read().unwrap().events().to_vec();
        assert!(events.iter().all(|event| {
            event
                .session
                .as_ref()
                .is_none_or(|session| session.provider == AgentProvider::Antigravity)
        }));
    }
}

#[test]
fn antigravity_abstention_and_provider_force_ask_preserve_native_prompt() {
    for policy in [None, Some("force_ask")] {
        let home = tempfile::tempdir().unwrap();
        if policy.is_none() {
            install_gate_mode_fixture(home.path(), "off");
        } else {
            install_model_fixture(home.path(), "approve");
        }

        let output = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &antigravity_permission_payload(home.path(), policy),
        );

        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({
                "decision": "ask",
                "reason": "Coding Brain abstained"
            })
        );
        let encoded = String::from_utf8_lossy(&output.stdout);
        assert!(!encoded.contains("force_ask"));
        assert!(!encoded.contains("permissionOverrides"));
    }
}

#[test]
fn provider_ask_policy_preserves_antigravity_model_deny() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "deny");

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(home.path(), Some("force_ask")),
    );

    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"decision": "deny"})
    );
}

#[test]
fn provider_ask_and_model_deny_survive_ignored_lifecycle_decision() {
    let mut failures = Vec::new();
    for provider in [AgentProvider::Claude, AgentProvider::Antigravity] {
        for ignored_reason in [
            IgnoreReason::Duplicate,
            IgnoreReason::RecentTurn,
            IgnoreReason::AmbiguousTurn,
        ] {
            let home = tempfile::tempdir().unwrap();
            install_model_fixture(home.path(), "deny");
            seed_ignored_permission(home.path(), provider, ignored_reason);
            let (provider_name, event, payload) = match provider {
                AgentProvider::Claude => (
                    "claude",
                    None,
                    claude_permission_payload(home.path(), Some("ask")),
                ),
                AgentProvider::Antigravity => (
                    "antigravity",
                    Some("PreToolUse"),
                    antigravity_permission_payload(home.path(), Some("force_ask")),
                ),
                AgentProvider::Codex => unreachable!(),
            };

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success());
            let expected = if provider == AgentProvider::Claude {
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {"behavior": "deny"}
                    }
                })
            } else {
                serde_json::json!({"decision": "deny"})
            };
            let ignored_reason = format!("{ignored_reason:?}");
            match serde_json::from_slice::<serde_json::Value>(&output.stdout) {
                Ok(response) if response == expected => {}
                Ok(response) => failures.push(format!(
                    "{provider_name} {ignored_reason}: expected {expected}, got {response}"
                )),
                Err(error) => failures.push(format!(
                    "{provider_name} {ignored_reason}: invalid response ({error})"
                )),
            }
            if !String::from_utf8_lossy(&output.stderr).contains(&ignored_reason) {
                failures.push(format!(
                    "{provider_name} {ignored_reason}: missing diagnostic: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn model_allow_requires_applied_lifecycle_decision() {
    for provider in [AgentProvider::Claude, AgentProvider::Antigravity] {
        for ignored_reason in [
            IgnoreReason::Duplicate,
            IgnoreReason::RecentTurn,
            IgnoreReason::AmbiguousTurn,
        ] {
            let home = tempfile::tempdir().unwrap();
            install_model_fixture(home.path(), "approve");
            seed_ignored_permission(home.path(), provider, ignored_reason);
            let (provider_name, event, payload) = match provider {
                AgentProvider::Claude => {
                    ("claude", None, claude_permission_payload(home.path(), None))
                }
                AgentProvider::Antigravity => (
                    "antigravity",
                    Some("PreToolUse"),
                    antigravity_permission_payload(home.path(), None),
                ),
                AgentProvider::Codex => unreachable!(),
            };

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success());
            if provider == AgentProvider::Claude {
                assert!(output.stdout.is_empty(), "{ignored_reason:?}");
            } else {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                    serde_json::json!({
                        "decision": "ask",
                        "reason": "Coding Brain abstained"
                    }),
                    "{ignored_reason:?}"
                );
            }
            let events = activity(home.path()).read().unwrap().events().to_vec();
            assert!(
                events
                    .iter()
                    .all(|event| event.state != ActivityState::Delivered),
                "{provider_name} {ignored_reason:?}"
            );
            assert_eq!(
                events.last().unwrap().state,
                ActivityState::Error,
                "{provider_name} {ignored_reason:?}"
            );
        }
    }
}

#[test]
fn provider_allow_responses_omit_model_message() {
    for provider in ["codex", "claude", "antigravity"] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture_full(
            home.path(),
            "approve",
            0.9,
            Some("approval detail must not escape"),
        );

        let (event, payload) = match provider {
            "codex" => (None, permission_payload(home.path(), "cargo test")),
            "claude" => (None, claude_permission_payload(home.path(), None)),
            _ => (
                Some("PreToolUse"),
                antigravity_permission_payload(home.path(), None),
            ),
        };
        let output = run_provider_permission_hook(home.path(), provider, event, &payload);

        assert!(output.status.success());
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if provider != "antigravity" {
            assert_eq!(
                response,
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {"behavior": "allow"}
                    }
                })
            );
        } else {
            assert_eq!(response, serde_json::json!({"decision": "allow"}));
        }
    }
}

#[test]
fn claude_allow_is_suppressed_for_open_turn_mismatch() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let lifecycle = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Claude,
        "claude-session-1".into(),
        Some("different-open-turn".into()),
        None,
        home.path().to_path_buf(),
    )
    .unwrap();
    lifecycle
        .record(LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap())
        .unwrap();

    let output = run_provider_permission_hook(
        home.path(),
        "claude",
        None,
        &claude_permission_payload(home.path(), None),
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ignored"));
    assert_eq!(
        activity(home.path())
            .read()
            .unwrap()
            .events()
            .last()
            .unwrap()
            .state,
        ActivityState::Error
    );
}

#[test]
fn repeated_claude_synthesized_turn_id_suppresses_second_allow() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let payload = claude_permission_payload(home.path(), None);

    let first = run_provider_permission_hook(home.path(), "claude", None, &payload);
    let second = run_provider_permission_hook(home.path(), "claude", None, &payload);

    assert!(first.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.stdout).unwrap()["hookSpecificOutput"]["decision"]
            ["behavior"],
        "allow"
    );
    assert!(second.status.success());
    assert!(second.stdout.is_empty());
    assert!(String::from_utf8_lossy(&second.stderr).contains("ignored"));
}

#[test]
fn repeated_antigravity_synthesized_turn_id_asks_after_model_allow() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let payload = antigravity_permission_payload(home.path(), None);

    let first =
        run_provider_permission_hook(home.path(), "antigravity", Some("PreToolUse"), &payload);
    let second =
        run_provider_permission_hook(home.path(), "antigravity", Some("PreToolUse"), &payload);

    assert!(first.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.stdout).unwrap(),
        serde_json::json!({"decision": "allow"})
    );
    assert!(second.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second.stdout).unwrap(),
        serde_json::json!({
            "decision": "ask",
            "reason": "Coding Brain abstained"
        })
    );
    assert!(String::from_utf8_lossy(&second.stderr).contains("ignored"));
    assert_eq!(
        activity(home.path())
            .read()
            .unwrap()
            .events()
            .last()
            .unwrap()
            .state,
        ActivityState::Error
    );
}

#[test]
fn antigravity_permission_requires_trusted_pre_tool_use_dispatch() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let payload = antigravity_permission_payload(home.path(), None);

    for event in [None, Some("Stop")] {
        let output = run_provider_permission_hook(home.path(), "antigravity", event, &payload);
        assert!(output.status.success());
        assert_ne!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["decision"],
            "allow"
        );
    }
}

#[test]
fn antigravity_invalid_present_policy_evidence_never_allows() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let base: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(home.path(), None)).unwrap();
    let mut cases = Vec::new();
    for value in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!("unexpected"),
    ] {
        let mut payload = base.clone();
        payload["decision"] = value;
        cases.push(serde_json::to_vec(&payload).unwrap());
    }
    for value in [serde_json::Value::Null, serde_json::json!({})] {
        let mut payload = base.clone();
        payload["permissionOverrides"] = value;
        cases.push(serde_json::to_vec(&payload).unwrap());
    }
    let mut oversized = serde_json::to_vec(&base).unwrap();
    oversized.extend(vec![b' '; 65_537]);
    cases.push(oversized);

    for payload in cases {
        let output =
            run_provider_permission_hook(home.path(), "antigravity", Some("PreToolUse"), &payload);
        assert!(output.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["decision"],
            "ask"
        );
        let encoded = String::from_utf8_lossy(&output.stdout);
        assert!(!encoded.contains("force_ask"));
        assert!(!encoded.contains("permissionOverrides"));
    }
}

#[test]
fn omitted_provider_is_byte_equivalent_to_explicit_codex_for_8k_command() {
    let implicit_home = tempfile::tempdir().unwrap();
    let explicit_home = tempfile::tempdir().unwrap();
    install_model_fixture(implicit_home.path(), "approve");
    install_model_fixture(explicit_home.path(), "approve");
    let command = "x".repeat(8 * 1024);

    let implicit = run_permission_hook(
        implicit_home.path(),
        &permission_payload(implicit_home.path(), &command),
    );
    let explicit = run_provider_permission_hook(
        explicit_home.path(),
        "codex",
        None,
        &permission_payload(explicit_home.path(), &command),
    );

    assert_eq!(implicit.stdout, explicit.stdout);
    assert_eq!(implicit.stderr, explicit.stderr);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&implicit.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "allow"
    );
}

#[test]
fn provider_permissions_accept_8k_commands_with_bounded_activity() {
    for provider in ["claude", "antigravity"] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), "approve");
        let command = "x".repeat(8 * 1024);
        let (event, payload) = if provider == "claude" {
            let mut payload: serde_json::Value =
                serde_json::from_slice(&claude_permission_payload(home.path(), None)).unwrap();
            payload["tool_input"]["command"] = serde_json::json!(command);
            (None, serde_json::to_vec(&payload).unwrap())
        } else {
            let mut payload: serde_json::Value =
                serde_json::from_slice(&antigravity_permission_payload(home.path(), None)).unwrap();
            payload["toolCall"]["args"]["CommandLine"] = serde_json::json!(command);
            (Some("PreToolUse"), serde_json::to_vec(&payload).unwrap())
        };

        let output = run_provider_permission_hook(home.path(), provider, event, &payload);

        assert!(output.status.success());
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if provider == "claude" {
            assert_eq!(
                response["hookSpecificOutput"]["decision"]["behavior"],
                "allow"
            );
        } else {
            assert_eq!(response["decision"], "allow");
        }
        assert!(output.stdout.len() < 256);
        assert!(
            activity(home.path())
                .read()
                .unwrap()
                .events()
                .iter()
                .all(|event| {
                    event
                        .normalized_command
                        .as_ref()
                        .is_none_or(|command| command.len() <= MAX_ACTIVITY_FIELD_BYTES)
                })
        );
    }
}

#[test]
fn malformed_provider_fields_preserve_each_native_prompt() {
    let claude_home = tempfile::tempdir().unwrap();
    install_model_fixture(claude_home.path(), "approve");
    let claude_base: serde_json::Value =
        serde_json::from_slice(&claude_permission_payload(claude_home.path(), None)).unwrap();
    for (field, value) in [
        ("session_id", serde_json::json!("")),
        ("tool_name", serde_json::json!("x".repeat(513))),
        (
            "tool_input",
            serde_json::json!({"command": "x".repeat(65_537)}),
        ),
    ] {
        let mut payload = claude_base.clone();
        payload[field] = value;
        let output = run_provider_permission_hook(
            claude_home.path(),
            "claude",
            None,
            &serde_json::to_vec(&payload).unwrap(),
        );
        assert!(output.stdout.is_empty());
    }
    let mut unsupported_claude = claude_base.clone();
    unsupported_claude["tool_name"] = serde_json::json!("Read");
    unsupported_claude["tool_input"] = serde_json::json!({"file_path": "/tmp/example"});
    let output = run_provider_permission_hook(
        claude_home.path(),
        "claude",
        None,
        &serde_json::to_vec(&unsupported_claude).unwrap(),
    );
    assert!(output.stdout.is_empty());

    let antigravity_home = tempfile::tempdir().unwrap();
    install_model_fixture(antigravity_home.path(), "approve");
    let antigravity_base: serde_json::Value = serde_json::from_slice(
        &antigravity_permission_payload(antigravity_home.path(), None),
    )
    .unwrap();
    for mutate in [
        ("conversationId", serde_json::json!("")),
        ("conversationId", serde_json::json!("x".repeat(513))),
        (
            "toolCall",
            serde_json::json!({"name": "run_command", "args": {}}),
        ),
        (
            "toolCall",
            serde_json::json!({"name": "x".repeat(513), "args": {}}),
        ),
        (
            "toolCall",
            serde_json::json!({
                "name": "run_command",
                "args": {"CommandLine": "x".repeat(65_537)}
            }),
        ),
    ] {
        let mut payload = antigravity_base.clone();
        payload[mutate.0] = mutate.1;
        let output = run_provider_permission_hook(
            antigravity_home.path(),
            "antigravity",
            Some("PreToolUse"),
            &serde_json::to_vec(&payload).unwrap(),
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["decision"],
            "ask"
        );
    }
    let mut unsupported_antigravity = antigravity_base;
    unsupported_antigravity["toolCall"] = serde_json::json!({
        "name": "view_file",
        "args": {"AbsolutePath": "/tmp/example"}
    });
    let output = run_provider_permission_hook(
        antigravity_home.path(),
        "antigravity",
        Some("PreToolUse"),
        &serde_json::to_vec(&unsupported_antigravity).unwrap(),
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["decision"],
        "ask"
    );
}

#[test]
fn antigravity_inference_and_persistence_failures_ask() {
    let inference_home = tempfile::tempdir().unwrap();
    install_model_fixture(inference_home.path(), "approve");
    overwrite_curl(inference_home.path(), "exit 7");
    let inference = run_provider_permission_hook(
        inference_home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(inference_home.path(), None),
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&inference.stdout).unwrap()["decision"],
        "ask"
    );

    let persistence_home = tempfile::tempdir().unwrap();
    install_model_fixture(persistence_home.path(), "approve");
    fs::create_dir_all(
        persistence_home
            .path()
            .join(".local/state/coding-brain/brain/decisions.jsonl"),
    )
    .unwrap();
    let persistence = run_provider_permission_hook(
        persistence_home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(persistence_home.path(), None),
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&persistence.stdout).unwrap()["decision"],
        "ask"
    );
}

#[test]
fn antigravity_reason_is_redacted_and_bounded() {
    let home = tempfile::tempdir().unwrap();
    let message = format!("token sk-secret-value {}", "x".repeat(16_000));
    install_model_fixture_full(home.path(), "deny", 0.9, Some(&message));

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(home.path(), None),
    );

    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let reason = response["reason"].as_str().unwrap();
    assert!(reason.contains("[REDACTED]"));
    assert!(!reason.contains("sk-secret-value"));
    assert!(reason.len() <= coding_brain_core::brain_activity::MAX_ACTIVITY_FIELD_BYTES);
}

#[test]
fn current_codex_post_tool_use_confirms_idless_permission_decision() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let command = "cargo test --workspace";

    let pre = run_lifecycle_hook(home.path(), &pre_tool_payload(home.path(), command));
    assert!(pre.status.success());
    assert!(pre.stderr.is_empty());
    let permission = run_permission_hook(home.path(), &permission_payload(home.path(), command));
    assert!(permission.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&permission.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "allow"
    );
    let before = activity(home.path()).read().unwrap().events().to_vec();
    let decision = before
        .iter()
        .find(|event| event.state == ActivityState::Allowed)
        .unwrap();
    let activity_id = decision.activity_id.clone();
    let decision_id = decision.decision_id.clone();
    assert_eq!(decision.session.as_ref().unwrap().tool_use_id, None);

    let post = run_lifecycle_hook(home.path(), &post_tool_payload(home.path(), command));
    assert!(post.status.success());
    assert!(
        post.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&post.stderr)
    );
    let store = activity(home.path());
    let events = store.read().unwrap().events().to_vec();
    assert!(events.iter().any(|event| {
        event.kind == ActivityKind::Lifecycle && event.tool.as_deref() == Some("PostToolUse")
    }));
    let outcome = events
        .iter()
        .find(|event| event.activity_id == activity_id && event.state == ActivityState::Outcome)
        .unwrap();
    assert_eq!(outcome.decision_id, decision_id);
    assert_eq!(outcome.outcome, Some(ActivityOutcome::Completed));
    let projected = store
        .snapshot(SnapshotLimits::default())
        .unwrap()
        .recent
        .into_iter()
        .find(|item| item.activity_id == activity_id)
        .unwrap();
    assert_eq!(projected.outcome, Some(ActivityOutcome::Completed));
    assert!(projected.tool_execution_confirmed);

    let persisted =
        std::fs::read_to_string(home.path().join(".local/state/coding-brain/activity.jsonl"))
            .unwrap();
    let diagnostic_rows = events
        .iter()
        .filter(|event| event.kind != ActivityKind::Decision)
        .collect::<Vec<_>>();
    for event in &diagnostic_rows {
        assert!(event.normalized_command.is_none());
        assert!(event.fingerprint.is_none());
        assert!(event.note.is_none());
    }
    let diagnostic_rows = serde_json::to_string(&diagnostic_rows).unwrap();
    assert!(!diagnostic_rows.contains(command));
    assert!(!diagnostic_rows.contains("Process exited with code 0"));
    assert!(!persisted.contains("Process exited with code 0"));
    let lifecycle = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let lifecycle = lifecycle.read().unwrap().snapshot.unwrap();
    assert_eq!(
        lifecycle.sessions["session-1"].latest_event,
        Some(coding_brain_core::lifecycle::LifecycleEventName::PostToolUse)
    );
    let lifecycle = serde_json::to_string(&lifecycle).unwrap();
    assert!(!lifecycle.contains(command));
    assert!(!lifecycle.contains("Process exited with code 0"));
}

#[test]
fn explicit_on_without_toml_uses_defaults_and_audits_without_response() {
    let home = tempfile::tempdir().unwrap();
    install_default_model_fixture(home.path(), "on", "approve");

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_default_model_request(home.path());
    let proposal = fs::read_to_string(
        home.path()
            .join(".local/state/coding-brain/brain/decisions.jsonl"),
    )
    .unwrap();
    let proposal: serde_json::Value = serde_json::from_str(proposal.trim()).unwrap();
    assert_eq!(proposal["brain_action"], "approve");
    assert_eq!(proposal["user_action"], "hook_proposal");
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert_eq!(
        events.iter().map(|event| event.state).collect::<Vec<_>>(),
        [
            ActivityState::Observed,
            ActivityState::Evaluating,
            ActivityState::Abstained,
        ]
    );
    let lifecycle = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    assert_eq!(
        lifecycle.read().unwrap().snapshot.unwrap().sessions
            [&coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                "session-1",
            )
            .storage_key()]
            .projected_status,
        Some(ProjectedStatus::NeedsInput)
    );
}

#[test]
fn explicit_auto_without_toml_uses_defaults_and_emits_allow() {
    let home = tempfile::tempdir().unwrap();
    install_default_model_fixture(home.path(), "auto", "approve");

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_default_model_request(home.path());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
}

#[test]
fn model_proposal_failure_abstains_before_terminal_commit() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    fs::create_dir_all(
        home.path()
            .join(".local/state/coding-brain/brain/decisions.jsonl"),
    )
    .unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("proposal"));
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert_eq!(
        events.iter().map(|event| event.state).collect::<Vec<_>>(),
        [ActivityState::Observed, ActivityState::Evaluating]
    );
}

#[test]
fn model_terminal_failure_abstains_with_proposal_only() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let activity_path = home.path().join(".local/state/coding-brain/activity.jsonl");
    let saved_activity_path = home
        .path()
        .join(".local/state/coding-brain/activity-before-failure.jsonl");
    overwrite_curl(
        home.path(),
        &format!(
            "dd of=/dev/null 2>/dev/null\nmv '{}' '{}'\nmkdir '{}'\nprintf '%s' '{{\"response\":\"{{\\\"action\\\":\\\"approve\\\",\\\"reasoning\\\":\\\"fixture\\\",\\\"confidence\\\":0.9}}\"}}'",
            activity_path.display(),
            saved_activity_path.display(),
            activity_path.display(),
        ),
    );

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal activity"));
    let proposal = fs::read_to_string(
        home.path()
            .join(".local/state/coding-brain/brain/decisions.jsonl"),
    )
    .unwrap();
    assert_eq!(proposal.lines().count(), 1);
    let events = ActivityStore::at(saved_activity_path)
        .read()
        .unwrap()
        .events()
        .to_vec();
    assert_eq!(events.len(), 2);
}

#[test]
fn inference_failure_and_low_confidence_are_visible_abstentions() {
    let endpoint_home = tempfile::tempdir().unwrap();
    install_model_fixture(endpoint_home.path(), "approve");
    overwrite_curl(endpoint_home.path(), "exit 7");
    let endpoint = run_permission_hook(
        endpoint_home.path(),
        &permission_payload(endpoint_home.path(), "cargo test"),
    );
    assert!(endpoint.stdout.is_empty());
    let endpoint_events = activity(endpoint_home.path())
        .read()
        .unwrap()
        .events()
        .to_vec();
    assert_eq!(endpoint_events[2].state, ActivityState::Error);

    let low_home = tempfile::tempdir().unwrap();
    install_model_fixture_with_confidence(low_home.path(), "approve", 0.1);
    let low = run_permission_hook(
        low_home.path(),
        &permission_payload(low_home.path(), "cargo test"),
    );
    assert!(low.stdout.is_empty());
    let low_events = activity(low_home.path()).read().unwrap().events().to_vec();
    assert_eq!(low_events[2].state, ActivityState::Abstained);
}

#[test]
fn malformed_and_unsupported_process_inputs_never_emit_permission_output() {
    let malformed_home = tempfile::tempdir().unwrap();
    let malformed = run_permission_hook(malformed_home.path(), b"not json");
    assert!(malformed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&malformed.stderr).contains("invalid"));

    let unsupported_home = tempfile::tempdir().unwrap();
    let unsupported = serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "tool_use_id": "call-1",
        "cwd": unsupported_home.path(),
        "hook_event_name": "PermissionRequest",
        "tool_name": "Read",
        "tool_input": {"file_path": "/tmp/example"}
    }))
    .unwrap();
    let output = run_permission_hook(unsupported_home.path(), &unsupported);
    assert!(output.stdout.is_empty());
    let events = activity(unsupported_home.path())
        .read()
        .unwrap()
        .events()
        .to_vec();
    assert_eq!(events[2].state, ActivityState::Abstained);
}

#[test]
fn closed_stdout_pipe_records_delivery_failed() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let mut child = spawn_permission_hook(home.path());
    drop(child.stdout.take());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&permission_payload(home.path(), "cargo test"))
        .unwrap();

    let output = child.wait_with_output().unwrap();

    assert!(String::from_utf8_lossy(&output.stderr).contains("write response"));
    let store = activity(home.path());
    let events = store.read().unwrap().events().to_vec();
    assert_eq!(events.last().unwrap().state, ActivityState::DeliveryFailed);
    let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
    assert_eq!(snapshot.attention[0].delivery, DeliveryState::Failed);
    assert!(!snapshot.attention[0].tool_execution_confirmed);
}

#[test]
fn bounded_permission_response_records_delivery_before_later_outcome() {
    let home = tempfile::tempdir().unwrap();
    let large_message = "x".repeat(512 * 1024);
    install_model_fixture_full(home.path(), "approve", 0.9, Some(&large_message));
    let pre = run_lifecycle_hook(home.path(), &pre_tool_payload(home.path(), "cargo test"));
    assert!(pre.status.success());
    assert!(pre.stderr.is_empty());
    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    let store = activity(home.path());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    assert!(
        output.stdout.len() <= coding_brain_core::brain_activity::MAX_ACTIVITY_FIELD_BYTES + 256
    );

    let before = store.snapshot(SnapshotLimits::default()).unwrap();
    assert_eq!(before.recent[0].delivery, DeliveryState::Delivered);
    assert!(!before.recent[0].tool_execution_confirmed);

    let outcome = post_tool_payload(home.path(), "cargo test");
    let lifecycle = run_lifecycle_hook(home.path(), &outcome);
    assert!(
        lifecycle.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&lifecycle.stderr)
    );
    let after = store.snapshot(SnapshotLimits::default()).unwrap();
    let confirmed = after
        .recent
        .iter()
        .chain(after.attention.iter().map(|item| &item.activity))
        .find(|item| item.activity_id == before.recent[0].activity_id)
        .unwrap();
    assert_eq!(confirmed.delivery, DeliveryState::Delivered);
    assert!(confirmed.tool_execution_confirmed);
}
