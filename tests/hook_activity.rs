#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use coding_brain::brain::activity::ActivityStore;
use coding_brain_core::brain_activity::{ActivityState, DeliveryState, SnapshotLimits};
use coding_brain_core::lifecycle::{LifecycleStore, ProjectedStatus};
use fs2::FileExt;

fn permission_payload(cwd: &Path, command: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "tool_use_id": "call-1",
        "cwd": cwd,
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": command}
    }))
    .unwrap()
}

fn run_permission_hook(home: &Path, payload: &[u8]) -> Output {
    let mut child = spawn_permission_hook(home);
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
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

fn activity(home: &Path) -> ActivityStore {
    ActivityStore::at(home.join(".local/state/coding-brain/activity.jsonl"))
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

fn read_json_envelope(reader: &mut impl Read) -> Vec<u8> {
    let mut result = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    loop {
        let read = reader.read(&mut buffer).unwrap();
        assert!(read > 0, "hook stdout closed before a complete envelope");
        for byte in &buffer[..read] {
            result.push(*byte);
            if in_string {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match *byte {
                b'"' => in_string = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return result;
                    }
                }
                _ => {}
            }
        }
    }
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
        lifecycle.read().unwrap().snapshot.unwrap().sessions["session-1"].projected_status,
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
fn killed_after_stdout_is_unknown_until_later_outcome() {
    let home = tempfile::tempdir().unwrap();
    let large_message = "x".repeat(512 * 1024);
    install_model_fixture_full(home.path(), "approve", 0.9, Some(&large_message));
    let mut child = spawn_permission_hook(home.path());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&permission_payload(home.path(), "cargo test"))
        .unwrap();

    let store = activity(home.path());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if store.read().is_ok_and(|log| {
            log.events()
                .iter()
                .any(|event| event.state == ActivityState::Allowed)
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal activity was not written"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(home.path().join(".local/state/coding-brain/activity.lock"))
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    let envelope = read_json_envelope(child.stdout.as_mut().unwrap());
    let response: serde_json::Value = serde_json::from_slice(&envelope).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    FileExt::unlock(&lock).unwrap();

    let before = store.snapshot(SnapshotLimits::default()).unwrap();
    assert_eq!(before.attention[0].delivery, DeliveryState::Unknown);
    assert!(!before.attention[0].tool_execution_confirmed);

    let outcome = serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "tool_use_id": "call-1",
        "cwd": home.path(),
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_response": {"exit_code": 0}
    }))
    .unwrap();
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
        .find(|item| item.activity_id == before.attention[0].activity_id)
        .unwrap();
    assert_eq!(confirmed.delivery, DeliveryState::Unknown);
    assert!(confirmed.tool_execution_confirmed);
}
