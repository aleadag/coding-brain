use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use codexctl_core::lifecycle::{LifecycleStore, ProjectedStatus};

const PROMPT: &[u8] = include_bytes!("fixtures/hooks/user-prompt-submit.json");

fn run_hook(home: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .arg("--lifecycle-hook")
        .env("HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn run_permission_hook(home: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codexctl"))
        .arg("--permission-hook")
        .env("HOME", home)
        .env("PATH", home.join("bin"))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn write_brain_config(home: &std::path::Path) {
    let config_dir = home.join(".config/codexctl");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[brain]\nenabled = true\nendpoint = \"http://localhost/api/generate\"\n",
    )
    .unwrap();
    let suggestion = serde_json::json!({
        "action": "approve",
        "message": "reviewed by brain",
        "reasoning": "test reasoning",
        "confidence": 0.9
    })
    .to_string();
    let body = serde_json::json!({ "response": suggestion }).to_string();
    let bin_dir = home.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let curl = bin_dir.join("curl");
    fs::write(&curl, format!("#!/bin/sh\nprintf '%s' '{body}'\n")).unwrap();
    fs::set_permissions(curl, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn lifecycle_hook_binary_is_silent_and_records_under_temporary_home() {
    let home = tempfile::tempdir().unwrap();
    let output = run_hook(home.path(), PROMPT);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!home.path().join(".codexctl/.star-prompted").exists());

    let snapshot = LifecycleStore::at(home.path().join(".codexctl"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions["session-1"].projected_status,
        Some(ProjectedStatus::Processing)
    );
}

#[test]
fn lifecycle_hook_binary_fails_open_with_empty_stdout() {
    let home = tempfile::tempdir().unwrap();
    let output = run_hook(home.path(), b"malformed secret");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.starts_with("codexctl lifecycle hook:"));
    assert!(!diagnostic.contains("secret"));
    assert!(!home.path().join(".codexctl/.star-prompted").exists());
}

#[test]
#[cfg(unix)]
fn permission_response_is_stable_across_lifecycle_failure() {
    let request = serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "transcript_path": "/tmp/rollout-1.jsonl",
        "cwd": "/work/codexctl",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" }
    })
    .to_string();
    let healthy = tempfile::tempdir().unwrap();
    write_brain_config(healthy.path());
    let healthy_output = run_permission_hook(healthy.path(), request.as_bytes());

    let blocked = tempfile::tempdir().unwrap();
    write_brain_config(blocked.path());
    fs::create_dir_all(blocked.path().join(".codexctl")).unwrap();
    fs::write(blocked.path().join(".codexctl/hooks"), b"occupied").unwrap();
    let blocked_output = run_permission_hook(blocked.path(), request.as_bytes());

    assert!(healthy_output.status.success());
    assert!(blocked_output.status.success());
    assert_eq!(blocked_output.stdout, healthy_output.stdout);
    let response: serde_json::Value = serde_json::from_slice(&healthy_output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    assert!(healthy_output.stderr.is_empty());
    assert!(
        String::from_utf8(blocked_output.stderr)
            .unwrap()
            .contains("lifecycle")
    );
    assert!(!healthy.path().join(".codexctl/.star-prompted").exists());
    assert!(!blocked.path().join(".codexctl/.star-prompted").exists());
}
