use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use coding_brain_core::lifecycle::{LifecycleStore, ProjectedStatus};

const PROMPT: &[u8] = include_bytes!("fixtures/hooks/user-prompt-submit.json");

fn run_hook(home: &std::path::Path, input: &[u8]) -> Output {
    let normalized_input = serde_json::from_slice::<serde_json::Value>(input)
        .map(|mut value| {
            value["cwd"] = serde_json::json!(home);
            serde_json::to_vec(&value).unwrap()
        })
        .unwrap_or_else(|_| input.to_vec());
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .arg("--lifecycle-hook")
        .env("HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&normalized_input)
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_cli(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .args(args)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .unwrap()
}

fn prompt_payload(index: usize) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(PROMPT).unwrap();
    payload["turn_id"] = serde_json::json!(format!("turn-{index}"));
    serde_json::to_vec(&payload).unwrap()
}

#[cfg(unix)]
fn run_permission_hook(home: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-brain"))
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
    let config_dir = home.join(".config/coding-brain");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[brain]\nenabled = true\nendpoint = \"http://localhost/api/generate\"\n",
    )
    .unwrap();
    let gate_mode = home.join(".local/state/coding-brain/brain/gate-mode");
    fs::create_dir_all(gate_mode.parent().unwrap()).unwrap();
    fs::write(gate_mode, "auto\n").unwrap();
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
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );

    let snapshot = LifecycleStore::at(home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions[&coding_brain_core::provider::AgentSessionKey::native(
            coding_brain_core::provider::AgentProvider::Codex,
            "session-1",
        )
        .storage_key()]
            .projected_status,
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
    assert!(diagnostic.starts_with("coding-brain lifecycle hook:"));
    assert!(!diagnostic.contains("secret"));
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
}

#[test]
#[cfg(unix)]
fn permission_response_is_stable_across_lifecycle_failure() {
    let request = |cwd: &std::path::Path| {
        serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "transcript_path": "/tmp/rollout-1.jsonl",
            "cwd": cwd,
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" }
        })
        .to_string()
    };
    let healthy = tempfile::tempdir().unwrap();
    write_brain_config(healthy.path());
    let healthy_request = request(healthy.path());
    let healthy_output = run_permission_hook(healthy.path(), healthy_request.as_bytes());

    let blocked = tempfile::tempdir().unwrap();
    write_brain_config(blocked.path());
    fs::create_dir_all(blocked.path().join(".local/state/coding-brain")).unwrap();
    fs::write(
        blocked.path().join(".local/state/coding-brain/hooks"),
        b"occupied",
    )
    .unwrap();
    let blocked_request = request(blocked.path());
    let blocked_output = run_permission_hook(blocked.path(), blocked_request.as_bytes());

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
    assert!(
        !healthy
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
    assert!(
        !blocked
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
}

#[test]
#[ignore = "local warm hook latency smoke; not a CI timing gate"]
#[cfg(unix)]
fn warm_lifecycle_hook_latency_and_roundtrip() {
    let home = tempfile::tempdir().unwrap();
    let hooks_path = home.path().join(".codex/hooks.json");
    fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let unrelated = serde_json::json!({
        "allowedTools": ["Read"],
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": "echo keep-me",
                    "timeout": 9
                }]
            }]
        }
    });
    fs::write(
        &hooks_path,
        format!("{}\n", serde_json::to_string_pretty(&unrelated).unwrap()),
    )
    .unwrap();

    let init = run_cli(home.path(), &["init", "--plugin-only"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let installed: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    let expected = [
        (
            "SessionStart",
            Some("startup|resume|clear|compact"),
            "--lifecycle-hook",
            2,
        ),
        ("UserPromptSubmit", None, "--lifecycle-hook", 2),
        ("PreToolUse", Some("*"), "--lifecycle-hook", 2),
        ("PermissionRequest", Some("*"), "--permission-hook", 30),
        ("PostToolUse", Some("*"), "--lifecycle-hook", 2),
        ("SubagentStart", Some("*"), "--lifecycle-hook", 2),
        ("SubagentStop", Some("*"), "--lifecycle-hook", 2),
        ("Stop", None, "--lifecycle-hook", 2),
    ];
    for (event, matcher, argument, timeout) in expected {
        let expected_command = format!("codexctl {argument}");
        let groups = installed["hooks"][event].as_array().unwrap();
        let (group, handler) = groups
            .iter()
            .flat_map(|group| {
                group["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |handler| (group, handler))
            })
            .find(|(_, handler)| handler["command"].as_str() == Some(expected_command.as_str()))
            .unwrap_or_else(|| panic!("missing managed {event} handler"));
        assert_eq!(
            group.get("matcher").and_then(|value| value.as_str()),
            matcher
        );
        assert_eq!(handler["timeout"], timeout);
    }

    let mut samples = Vec::new();
    for index in 0..101 {
        let started = Instant::now();
        let output = run_hook(home.path(), &prompt_payload(index));
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        if index > 0 {
            samples.push(started.elapsed());
        }
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    eprintln!("warm lifecycle hook latency: p50={p50:?} p95={p95:?}; target <50ms");

    write_brain_config(home.path());
    let permission = serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-100",
        "transcript_path": "/tmp/rollout-1.jsonl",
        "cwd": "/work/codexctl",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" }
    });
    let permission_output = run_permission_hook(
        home.path(),
        serde_json::to_string(&permission).unwrap().as_bytes(),
    );
    assert!(permission_output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&permission_output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    let store = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let view = store.read().unwrap();
    assert_eq!(
        view.condition,
        coding_brain_core::lifecycle::StoreCondition::Healthy
    );
    let key = coding_brain_core::provider::AgentSessionKey::native(
        coding_brain_core::provider::AgentProvider::Codex,
        "session-1",
    )
    .storage_key();
    let state = &view.snapshot.unwrap().sessions[&key];
    assert_eq!(
        state.latest_event,
        Some(coding_brain_core::lifecycle::LifecycleEventName::PermissionRequest)
    );
    assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let removed: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    assert_eq!(removed, unrelated);
    assert!(store.snapshot_path().exists());
}
