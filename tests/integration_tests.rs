use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use coding_brain::discovery;
use coding_brain::monitor;
use coding_brain::process;
use coding_brain::session::{
    AgentSession, CodexTaskState, RawAgentSession, SessionStatus, TelemetryStatus,
};
use coding_brain_core::provider::AgentProvider;

#[test]
fn current_runtime_writes_only_sqlite_and_session_links() {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join(".git")).unwrap();
    let state_root = home.path().join(".local/state/coding-brain");

    let doctor = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["doctor", "--json"])
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(state_root.join("db/brain.sqlite3").exists(), "{doctor:?}");

    let lifecycle = serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "cwd": project.path(),
        "hook_event_name": "SessionStart"
    }))
    .unwrap();
    let lifecycle_output = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--lifecycle-hook", "--provider", "codex"])
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.as_mut().unwrap().write_all(&lifecycle)?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(lifecycle_output.status.success(), "{lifecycle_output:?}");

    let permission = serde_json::to_vec(&serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-1",
        "cwd": project.path(),
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": {"command": "rm -rf /"}
    }))
    .unwrap();
    let permission_output = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .arg("--permission-hook")
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            child.stdin.as_mut().unwrap().write_all(&permission)?;
            child.wait_with_output()
        })
        .unwrap();
    assert!(permission_output.status.success(), "{permission_output:?}");

    let review_queue = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--brain-review", "list"])
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(review_queue.status.success(), "{review_queue:?}");

    let missing_canonical = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--brain-mark-canonical", "missing-decision"])
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(!missing_canonical.status.success());

    assert!(state_root.join("db/brain.sqlite3").exists());
    // This fixture has no validated live-parent identity, so no authoritative
    // session link is expected. The link store remains a separate optional file.
    for legacy in [
        "activity.jsonl",
        "brain/decisions.jsonl",
        "review-state.json",
        "hooks/lifecycle.json",
    ] {
        assert!(
            !state_root.join(legacy).exists(),
            "unexpected live legacy store: {legacy}"
        );
    }
    let journal_guard = state_root.join("brain/permission-transactions");
    if journal_guard.exists() {
        assert_eq!(
            std::fs::read_dir(journal_guard).unwrap().count(),
            0,
            "live permission journals must not be created"
        );
    }
}

#[test]
fn init_help_lists_all_provider_selectors() {
    let project = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["init", "--help"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for provider in ["codex", "claude", "antigravity", "all"] {
        assert!(
            help.contains(provider),
            "missing provider {provider}: {help}"
        );
    }
}

#[test]
fn init_provider_contract_covers_managed_paths_commands_and_compatibility_warning() {
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join(".git")).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args([
            "init",
            "all",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ])
        .env("HOME", home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let managed = [
        (".codex/hooks.json", "--provider codex"),
        (".claude/settings.json", "--provider claude"),
        (".gemini/config/hooks.json", "--provider antigravity"),
    ];
    for (path, provider_arg) in managed {
        let contents = std::fs::read_to_string(home.path().join(path)).unwrap();
        assert!(
            contents.contains(provider_arg),
            "missing {provider_arg} in {path}"
        );
    }

    let compatibility_home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        compatibility_home.path(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let compatibility_project = tempfile::tempdir().unwrap();
    std::fs::create_dir(compatibility_project.path().join(".git")).unwrap();
    let compatibility = std::process::Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["init", "--non-interactive", "--skip-brain", "--skip-skills"])
        .env("HOME", compatibility_home.path())
        .env_remove("XDG_STATE_HOME")
        .current_dir(compatibility_project.path())
        .output()
        .unwrap();
    assert!(compatibility.status.success());
    let compatibility_stderr = String::from_utf8(compatibility.stderr).unwrap();
    assert_eq!(
        compatibility_stderr.lines().next(),
        Some(
            "warning: provider-less --non-interactive is deprecated; use `cbrain init codex --non-interactive` instead"
        )
    );
    assert!(compatibility_home.path().join(".codex/hooks.json").exists());
    assert!(
        !compatibility_home
            .path()
            .join(".claude/settings.json")
            .exists()
    );
    assert!(
        !compatibility_home
            .path()
            .join(".gemini/config/hooks.json")
            .exists()
    );
}

/// Helper: create a minimal session for testing status inference.
fn make_session(cpu: f32, last_message_age_secs: u64) -> AgentSession {
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test-session".into(),
        cwd: "/tmp/test-project".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    s.cpu_percent = cpu;
    s.telemetry_status = TelemetryStatus::Available;

    // Set last_message_ts relative to now
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    s.last_message_ts = now_ms.saturating_sub(last_message_age_secs * 1000);
    s
}

// ────────────────────────────────────────────────────────────────────────────
// Status Inference Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn status_high_cpu_always_processing() {
    let mut s = make_session(50.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_high_cpu_overrides_waiting_for_task() {
    let mut s = make_session(10.0, 0);
    monitor::infer_status(&mut s, "assistant", "end_turn", true);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_high_cpu_overrides_end_turn() {
    let mut s = make_session(20.0, 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_waiting_for_task_needs_input() {
    let mut s = make_session(0.5, 10);
    monitor::infer_status(&mut s, "", "", true);
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn status_end_turn_recent_waiting_input() {
    // Assistant said end_turn, 2 minutes ago, low CPU
    let mut s = make_session(0.5, 120);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::WaitingInput);
}

#[test]
fn status_end_turn_old_idle() {
    // Assistant said end_turn, 15 minutes ago → Idle
    let mut s = make_session(0.5, 15 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_end_turn_exactly_10min_still_waiting() {
    // 10 minutes = boundary, should still be WaitingInput (>10 is Idle)
    let mut s = make_session(0.5, 10 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::WaitingInput);
}

#[test]
fn status_end_turn_11min_idle() {
    let mut s = make_session(0.5, 11 * 60);
    monitor::infer_status(&mut s, "assistant", "end_turn", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_tool_use_low_cpu_old_stays_processing() {
    // A pending tool and low CPU are not approval evidence.
    let mut s = make_session(0.5, 30);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_tool_use_low_cpu_recent_processing() {
    // tool_use + low CPU + <5s ago = still processing (tool just fired)
    let mut s = make_session(0.5, 2);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_tool_use_high_cpu_processing() {
    // tool_use + high CPU = still crunching
    let mut s = make_session(15.0, 30);
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn pending_shell_with_low_cpu_is_processing_without_approval_evidence() {
    let mut s = make_session(0.1, 30);
    s.task_state = CodexTaskState::Processing;
    s.pending_tool_name = Some("exec_command".into());
    s.pending_tool_call_id = Some("call-7".into());

    monitor::refresh_status(&mut s);

    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_user_message_pending_processing() {
    let mut s = make_session(3.0, 5);
    monitor::infer_status(&mut s, "user", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_user_message_low_cpu_still_processing() {
    // User sent message, CPU low — could be waiting for API
    let mut s = make_session(0.5, 5);
    monitor::infer_status(&mut s, "user", "", false);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_no_signals_idle() {
    // No JSONL signals at all → Idle
    let mut s = make_session(0.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Idle);
}

#[test]
fn status_no_telemetry_unknown() {
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test-session".into(),
        cwd: "/tmp/test-project".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Unknown);
}

#[test]
fn status_cpu_threshold_boundary() {
    // CPU exactly 5.0 — should NOT trigger Processing (threshold is >5.0)
    let mut s = make_session(5.0, 0);
    monitor::infer_status(&mut s, "", "", false);
    assert_eq!(s.status, SessionStatus::Idle);

    // CPU 5.1 — should trigger Processing
    let mut s2 = make_session(5.1, 0);
    monitor::infer_status(&mut s2, "", "", false);
    assert_eq!(s2.status, SessionStatus::Processing);
}

#[test]
fn status_persisted_tool_use_survives_empty_tick() {
    // A pending tool stays Processing across empty ticks until separate approval
    // evidence is supplied.
    let mut s = make_session(0.5, 30);

    // Tick 1: new JSONL data — tool_use detected
    monitor::infer_status(&mut s, "assistant", "tool_use", false);
    assert_eq!(s.status, SessionStatus::Processing);

    // Simulate what update_tokens() now does: persist the signals
    s.last_msg_type = "assistant".into();
    s.last_stop_reason = "tool_use".into();
    s.is_waiting_for_task = false;

    // Tick 2: no new JSONL data — signals come from persisted fields
    let msg_type = s.last_msg_type.clone();
    let stop_reason = s.last_stop_reason.clone();
    let waiting = s.is_waiting_for_task;
    monitor::infer_status(&mut s, &msg_type, &stop_reason, waiting);
    assert_eq!(s.status, SessionStatus::Processing);
}

#[test]
fn status_null_stop_reason_with_tool_use_stays_processing() {
    // Tool-call transcripts can write stop_reason: null. The content still has a
    // tool_use block, but that alone is not approval evidence.
    let jsonl = r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","stop_reason":null,"content":[{"type":"tool_use","id":"toolu_01X","name":"Bash","input":{"command":"echo hi"}}],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    s.cpu_percent = 0.5;
    monitor::update_tokens(&mut s);

    // stop_reason was null in JSONL but must be inferred from tool_use content
    assert_eq!(s.last_stop_reason, "tool_use");
    // pending_tool_name is set (ToolUse parsed, no ToolResult yet), while status
    // remains Processing until terminal confirmation is added in Task 3.
    assert_eq!(s.pending_tool_name, Some("Bash".into()));
    assert_eq!(s.status, SessionStatus::Processing);
}

// ────────────────────────────────────────────────────────────────────────────
// Model Shortening Tests
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn shorten_model_gpt_55() {
    assert_eq!(monitor::shorten_model("codex-gpt-5.5-20260612"), "gpt-5.5");
}

#[test]
fn shorten_model_gpt_54() {
    assert_eq!(monitor::shorten_model("codex-gpt-5.4-20260612"), "gpt-5.4");
}

#[test]
fn shorten_model_gpt_54_mini() {
    assert_eq!(
        monitor::shorten_model("codex-gpt-5.4-mini-20260612"),
        "gpt-5.4-mini"
    );
    assert_eq!(monitor::shorten_model("gpt-5.4 mini"), "gpt-5.4-mini");
}

#[test]
fn shorten_model_unknown() {
    assert_eq!(monitor::shorten_model("custom-model"), "custom-model");
}

// ────────────────────────────────────────────────────────────────────────────
// JSONL Parsing Integration Tests (using temp files)
// ────────────────────────────────────────────────────────────────────────────

fn make_session_with_jsonl(content: &str) -> (AgentSession, tempfile::NamedTempFile) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(content.as_bytes()).unwrap();
    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(file).unwrap();
    }
    file.flush().unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    s.jsonl_path = Some(file.path().to_path_buf());
    (s, file)
}

fn make_codex_session_with_jsonl(content: &str) -> (AgentSession, tempfile::NamedTempFile) {
    let mut file = tempfile::Builder::new()
        .prefix("rollout-")
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    file.write_all(content.as_bytes()).unwrap();
    if !content.is_empty() && !content.ends_with('\n') {
        writeln!(file).unwrap();
    }
    file.flush().unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    session.jsonl_path = Some(file.path().to_path_buf());
    (session, file)
}

fn padded_jsonl(mut value: serde_json::Value, target_len: usize) -> String {
    value
        .as_object_mut()
        .unwrap()
        .insert("padding".into(), serde_json::Value::String(String::new()));
    let minimum_len = serde_json::to_vec(&value).unwrap().len() + 1;
    assert!(target_len >= minimum_len);
    value.as_object_mut().unwrap().insert(
        "padding".into(),
        serde_json::Value::String("x".repeat(target_len - minimum_len)),
    );
    let content = format!("{}\n", serde_json::to_string(&value).unwrap());
    assert_eq!(content.len(), target_len);
    content
}

fn replace_jsonl(file: &mut tempfile::NamedTempFile, content: &str) {
    file.as_file_mut().set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.flush().unwrap();
}

fn append_jsonl_line(file: &mut tempfile::NamedTempFile, line: &str) {
    writeln!(file, "{line}").unwrap();
    file.flush().unwrap();
}

fn assert_generic_replacement_rescanned(extra_bytes: usize) {
    let original = r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":129200,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1},"content":[]}}"#;
    let (mut session, mut file) = make_session_with_jsonl(original);

    monitor::update_tokens(&mut session);
    let original_len = session.jsonl_offset;
    assert_eq!(session.context_pressure, Some(50));
    assert_eq!(session.last_msg_type, "assistant");
    assert_eq!(session.status, SessionStatus::WaitingInput);

    let replacement = padded_jsonl(
        serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "model": "gpt-5.4",
                "content": []
            }
        }),
        usize::try_from(original_len).unwrap() + extra_bytes,
    );
    replace_jsonl(&mut file, &replacement);

    monitor::update_tokens(&mut session);

    assert_eq!(session.jsonl_offset, replacement.len() as u64);
    assert_eq!(session.context_pressure, None);
    assert_eq!(session.last_msg_type, "user");
    assert_eq!(session.last_stop_reason, "");
    assert_eq!(session.status, SessionStatus::Processing);
}

fn assert_codex_replacement_rescanned(extra_bytes: usize) {
    let original = concat!(
        r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":50},"model_context_window":100}}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        "\n",
    );
    let (mut session, mut file) = make_codex_session_with_jsonl(original);

    monitor::update_tokens(&mut session);
    let original_len = session.jsonl_offset;
    assert_eq!(session.context_pressure, Some(50));
    assert_eq!(session.task_state, CodexTaskState::WaitingInput);
    assert_eq!(session.last_stop_reason, "end_turn");

    let replacement = padded_jsonl(
        serde_json::json!({
            "type": "event_msg",
            "payload": {"type": "task_started"}
        }),
        usize::try_from(original_len).unwrap() + extra_bytes,
    );
    replace_jsonl(&mut file, &replacement);

    monitor::update_tokens(&mut session);

    assert_eq!(session.jsonl_offset, replacement.len() as u64);
    assert_eq!(session.context_pressure, None);
    assert_eq!(session.task_state, CodexTaskState::Processing);
    assert_eq!(session.last_stop_reason, "");
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn generic_monitor_rescans_same_size_replacement() {
    assert_generic_replacement_rescanned(0);
}

#[test]
fn generic_monitor_rescans_larger_replacement() {
    assert_generic_replacement_rescanned(64);
}

#[test]
fn codex_monitor_rescans_same_size_replacement() {
    assert_codex_replacement_rescanned(0);
}

#[test]
fn codex_monitor_rescans_larger_replacement() {
    assert_codex_replacement_rescanned(64);
}

#[test]
fn generic_monitor_does_not_rescan_an_ordinary_append() {
    let original = r#"{"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
    let (mut session, mut file) = make_session_with_jsonl(original);

    monitor::update_tokens(&mut session);
    assert_eq!(session.tool_usage["Bash"].calls, 1);

    append_jsonl_line(
        &mut file,
        r#"{"type":"user","message":{"role":"user","content":[]}}"#,
    );
    monitor::update_tokens(&mut session);

    assert_eq!(session.tool_usage["Bash"].calls, 1);
    assert_eq!(session.last_msg_type, "user");
}

#[test]
fn codex_monitor_does_not_rescan_an_ordinary_append() {
    let original = r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call-1"}}"#;
    let (mut session, mut file) = make_codex_session_with_jsonl(original);

    monitor::update_tokens(&mut session);
    assert_eq!(session.tool_usage["exec_command"].calls, 1);

    append_jsonl_line(
        &mut file,
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"continuing"}}"#,
    );
    monitor::update_tokens(&mut session);

    assert_eq!(session.tool_usage["exec_command"].calls, 1);
    assert_eq!(session.last_msg_type, "assistant");
}

#[test]
fn context_pressure_codex_monitor_uses_provider_then_fallback_and_resets() {
    let jsonl = concat!(
        r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":50},"model_context_window":100}}}"#,
        "\n",
    );
    let (mut session, mut file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    append_jsonl_line(
        &mut file,
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":2}}}}"#,
    );
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    file.as_file_mut().set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, None);
    assert_eq!(session.jsonl_offset, 0);
    assert_eq!(session.telemetry_status, TelemetryStatus::Pending);

    let fallback_jsonl = concat!(
        r#"{"type":"turn_context","payload":{"model":"gpt-5.5"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":129200}}}}"#,
        "\n",
    );
    let (mut fallback, _fallback_file) = make_codex_session_with_jsonl(fallback_jsonl);
    monitor::update_tokens(&mut fallback);
    assert_eq!(fallback.model, "gpt-5.5");
    assert_eq!(fallback.context_pressure, Some(50));

    let unknown_jsonl = fallback_jsonl.replace("gpt-5.5", "custom-model");
    let (mut unknown, _unknown_file) = make_codex_session_with_jsonl(&unknown_jsonl);
    monitor::update_tokens(&mut unknown);
    assert_eq!(unknown.context_pressure, None);
}

#[test]
fn context_pressure_generic_monitor_retains_valid_value_until_truncation() {
    let line = r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":129200,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1},"content":[]}}"#;
    let (mut session, mut file) = make_session_with_jsonl(line);

    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    append_jsonl_line(
        &mut file,
        r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":"not-a-number","cache_read_input_tokens":7,"cache_creation_input_tokens":9,"output_tokens":2},"content":[]}}"#,
    );
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    append_jsonl_line(
        &mut file,
        r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":1,"cache_read_input_tokens":"not-a-number","cache_creation_input_tokens":0,"output_tokens":0},"content":[]}}"#,
    );
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    append_jsonl_line(
        &mut file,
        r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":"not-a-number","output_tokens":0},"content":[]}}"#,
    );
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(50));

    file.as_file_mut().set_len(0).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, None);
    assert_eq!(session.jsonl_offset, 0);
    assert_eq!(session.telemetry_status, TelemetryStatus::Pending);
}

fn make_session_with_paths(
    cwd: String,
    session_id: String,
    jsonl_path: std::path::PathBuf,
) -> AgentSession {
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id,
        cwd,
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    s.jsonl_path = Some(jsonl_path);
    s
}

fn write_jsonl(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, format!("{content}\n")).unwrap();
}

#[test]
fn partial_jsonl_line_is_retried_after_newline() {
    let (mut session, mut file) = make_codex_session_with_jsonl("");
    monitor::update_tokens(&mut session);
    let complete_offset = session.jsonl_offset;
    write!(
        file,
        r#"{{"type":"event_msg","payload":{{"type":"task_started"}}}}"#
    )
    .unwrap();
    file.flush().unwrap();

    monitor::update_tokens(&mut session);
    assert_eq!(session.task_state, CodexTaskState::Unknown);
    assert_eq!(session.jsonl_offset, complete_offset);

    writeln!(file).unwrap();
    file.flush().unwrap();
    monitor::update_tokens(&mut session);
    assert_eq!(session.task_state, CodexTaskState::Processing);
    assert!(session.jsonl_offset > complete_offset);
}

#[test]
fn codex_discovery_ignores_history_without_live_processes() {
    let dir = tempfile::tempdir().unwrap();
    let codex_home = dir.path().join(".codex");
    let jsonl_path = codex_home
        .join("sessions")
        .join("2026")
        .join("06")
        .join("11")
        .join("rollout-2026-06-11T20-33-34-019eb6ac-6d30-7301-885d-ff4d354c0116.jsonl");
    write_jsonl(
        &jsonl_path,
        include_str!("fixtures/codex-session-meta.json"),
    );

    unsafe {
        std::env::set_var("CODEXCTL_CODEX_HOME", &codex_home);
        std::env::set_var("CODEXCTL_DISABLE_PROCESS_DISCOVERY", "1");
    }
    let sessions = discovery::scan_sessions();
    unsafe {
        std::env::remove_var("CODEXCTL_CODEX_HOME");
        std::env::remove_var("CODEXCTL_DISABLE_PROCESS_DISCOVERY");
    }

    assert!(
        sessions.is_empty(),
        "historical Codex transcripts are telemetry, not live sessions"
    );
}

#[test]
fn codex_monitor_records_function_calls() {
    let jsonl = concat!(
        r#"{"timestamp":"2026-06-11T12:33:54.694Z","type":"session_meta","payload":{"id":"019eb6ac-6d30-7301-885d-ff4d354c0116","timestamp":"2026-06-11T12:33:34.003Z","cwd":"/home/alexander/hacking/aleadag/codexctl","model_provider":"openai"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-11T12:34:01.791Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\",\"workdir\":\"/home/alexander/hacking/aleadag/codexctl\"}","call_id":"call_123"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-11T12:34:02.100Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_123","output":"test result: ok"}}"#,
        "\n",
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(jsonl.as_bytes()).unwrap();
    file.flush().unwrap();

    let mut session = AgentSession::from_codex_transcript(
        "019eb6ac-6d30-7301-885d-ff4d354c0116".into(),
        "/home/alexander/hacking/aleadag/codexctl".into(),
        0,
        file.path().to_path_buf(),
    );

    monitor::update_tokens(&mut session);

    assert_eq!(session.telemetry_status, TelemetryStatus::Available);
    assert_eq!(session.tool_usage.get("exec_command").unwrap().calls, 1);
    assert_eq!(session.pending_tool_name, None);
    assert!(!session.last_tool_error);
}

#[test]
fn mismatched_tool_output_does_not_close_pending_call() {
    let (mut session, _file) =
        make_codex_session_with_jsonl(include_str!("fixtures/codex-modern-lifecycle.jsonl"));

    monitor::update_tokens(&mut session);

    assert_eq!(session.pending_tool_call_id.as_deref(), Some("call-live"));
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn agent_message_does_not_close_pending_call() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"cargo test\"}","call_id":"call-live"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"still working"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert_eq!(session.pending_tool_call_id.as_deref(), Some("call-live"));
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn matching_tool_output_closes_pending_call() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"shell","input":"cargo test","call_id":"call-7"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-7","output":"ok"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert_eq!(session.pending_tool_call_id, None);
    assert_eq!(session.pending_tool_name, None);
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn request_user_input_requires_input_until_matching_output() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[]}","call_id":"ask-1"}}"#,
        "\n",
    );
    let (mut session, mut file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);
    assert!(session.explicit_input_required);
    assert_eq!(session.status, SessionStatus::NeedsInput);

    file.write_all(
        concat!(
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"ask-1","output":"answer"}}"#,
            "\n"
        )
        .as_bytes(),
    )
    .unwrap();
    file.flush().unwrap();
    monitor::update_tokens(&mut session);

    assert!(!session.explicit_input_required);
    assert_eq!(session.pending_tool_call_id, None);
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn continued_activity_dismisses_explicit_input_without_closing_call() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{\"questions\":[]}","call_id":"ask-1"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"agent_message","message":"continuing"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert!(!session.explicit_input_required);
    assert_eq!(session.pending_tool_call_id.as_deref(), Some("ask-1"));
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn task_complete_becomes_waiting_input() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert_eq!(session.task_state, CodexTaskState::WaitingInput);
    assert_eq!(session.status, SessionStatus::WaitingInput);
}

#[test]
fn turn_aborted_ends_processing_without_needs_input() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"turn_aborted"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert_eq!(session.task_state, CodexTaskState::Aborted);
    assert_eq!(session.status, SessionStatus::WaitingInput);
}

#[test]
fn unknown_modern_event_does_not_end_active_task() {
    let jsonl = concat!(
        r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
        "\n",
        r#"{"type":"event_msg","payload":{"type":"future_event"}}"#,
        "\n",
    );
    let (mut session, _file) = make_codex_session_with_jsonl(jsonl);

    monitor::update_tokens(&mut session);

    assert_eq!(session.task_state, CodexTaskState::Processing);
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn process_backed_codex_monitor_records_derived_context_pressure() {
    let jsonl = concat!(
        r#"{"timestamp":"2026-06-11T12:33:54.694Z","type":"session_meta","payload":{"id":"019eb6ac-6d30-7301-885d-ff4d354c0116","timestamp":"2026-06-11T12:33:34.003Z","cwd":"/home/alexander/hacking/aleadag/codexctl","model_provider":"openai"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-11T12:34:01.000Z","type":"turn_context","payload":{"cwd":"/home/alexander/hacking/aleadag/codexctl","model":"gpt-5-codex"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-11T12:34:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100000,"cached_input_tokens":25000,"output_tokens":12000,"reasoning_output_tokens":3000,"total_tokens":112000},"last_token_usage":{"input_tokens":42000,"cached_input_tokens":21000,"output_tokens":12000,"reasoning_output_tokens":3000,"total_tokens":54000},"model_context_window":258400}}}"#,
        "\n",
    );
    let mut file = tempfile::Builder::new()
        .prefix("rollout-")
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    file.write_all(jsonl.as_bytes()).unwrap();
    file.flush().unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "019eb6ac-6d30-7301-885d-ff4d354c0116".into(),
        cwd: "/home/alexander/hacking/aleadag/codexctl".into(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    session.jsonl_path = Some(file.path().to_path_buf());

    monitor::update_tokens(&mut session);

    assert_eq!(session.telemetry_status, TelemetryStatus::Available);
    assert_eq!(session.context_pressure, Some(16));
    assert_eq!(session.format_context(), "16%");
}

#[test]
fn process_backed_codex_monitor_preserves_context_pressure_on_idle_tick() {
    let jsonl = concat!(
        r#"{"timestamp":"2026-06-12T09:13:44.723Z","type":"session_meta","payload":{"id":"019ebb14-fa82-70b0-afc7-6daab97998ec","timestamp":"2026-06-12T09:06:14.788Z","cwd":"/home/alexander/hacking/aleadag/codexctl","model_provider":"openai"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-12T09:13:44.723Z","type":"turn_context","payload":{"cwd":"/home/alexander/hacking/aleadag/codexctl","model":"gpt-5.5"}}"#,
        "\n",
        r#"{"timestamp":"2026-06-12T09:13:44.723Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1539721,"cached_input_tokens":1251840,"output_tokens":8629,"reasoning_output_tokens":3422,"total_tokens":1548350},"last_token_usage":{"input_tokens":125980,"cached_input_tokens":115584,"output_tokens":143,"reasoning_output_tokens":43,"total_tokens":126123},"model_context_window":258400}}}"#,
        "\n",
    );
    let mut file = tempfile::Builder::new()
        .prefix("rollout-")
        .suffix(".jsonl")
        .tempfile()
        .unwrap();
    file.write_all(jsonl.as_bytes()).unwrap();
    file.flush().unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "019ebb14-fa82-70b0-afc7-6daab97998ec".into(),
        cwd: "/home/alexander/hacking/aleadag/codexctl".into(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    session.jsonl_path = Some(file.path().to_path_buf());

    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(48));
    assert_eq!(session.format_context(), "48%");

    monitor::update_tokens(&mut session);
    assert_eq!(session.context_pressure, Some(48));
    assert_eq!(session.format_context(), "48%");
}

#[test]
fn transcript_backed_sessions_are_not_marked_finished_by_ps() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(include_str!("fixtures/codex-session-meta.json").as_bytes())
        .unwrap();
    file.flush().unwrap();
    let session = AgentSession::from_codex_transcript(
        "019eb6ac-6d30-7301-885d-ff4d354c0116".into(),
        "/home/alexander/hacking/aleadag/codexctl".into(),
        0,
        file.path().to_path_buf(),
    );
    let mut sessions = vec![session];

    process::fetch_and_enrich(&mut sessions);

    assert_ne!(sessions[0].status, SessionStatus::Finished);
    assert!(!sessions[0].process_backed);
}

#[test]
fn jsonl_parses_model_status_and_context_pressure() {
    let jsonl = r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":50000,"output_tokens":10000,"cache_read_input_tokens":20000,"cache_creation_input_tokens":5000}}}"#;

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    assert_eq!(s.model, "gpt-5.5");
    assert_eq!(s.context_pressure, Some(29));
    assert_eq!(s.status, SessionStatus::WaitingInput);
    assert_eq!(s.telemetry_status, TelemetryStatus::Available);
}

#[test]
fn jsonl_parse_multiple_entries() {
    let jsonl = concat!(
        r#"{"type":"user","message":{"type":"user"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"gpt-5.4","stop_reason":"tool_use","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"model":"gpt-5.4","stop_reason":"end_turn","usage":{"input_tokens":2000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    assert_eq!(s.model, "gpt-5.4");
    assert_eq!(s.context_pressure, Some(0));
    assert_eq!(s.last_stop_reason, "end_turn");
}

#[test]
fn jsonl_incremental_reads() {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let line1 = r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
    writeln!(file, "{line1}").unwrap();
    file.flush().unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    s.jsonl_path = Some(file.path().to_path_buf());

    monitor::update_tokens(&mut s);
    let first_offset = s.jsonl_offset;
    assert!(first_offset > 0);
    assert_eq!(s.model, "gpt-5.5");

    monitor::update_tokens(&mut s);
    assert_eq!(s.jsonl_offset, first_offset);

    let line2 = r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":2000,"output_tokens":800,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#;
    writeln!(file, "{line2}").unwrap();
    file.flush().unwrap();

    monitor::update_tokens(&mut s);
    assert!(s.jsonl_offset > first_offset);
    assert_eq!(s.last_stop_reason, "end_turn");
}

#[test]
fn jsonl_empty_file() {
    let (mut s, _file) = make_session_with_jsonl("");
    monitor::update_tokens(&mut s);
    assert_eq!(s.telemetry_status, TelemetryStatus::Pending);
    assert_eq!(s.context_pressure, None);
}

#[test]
fn jsonl_corrupted_lines_skipped() {
    let jsonl = concat!(
        "not valid json at all\n",
        "{\"type\":\"something but no usage\"}\n",
        r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":5000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    monitor::update_tokens(&mut s);

    assert_eq!(s.telemetry_status, TelemetryStatus::Available);
    assert_eq!(s.model, "gpt-5.5");
    assert_eq!(s.context_pressure, Some(1));
}

#[test]
fn jsonl_waiting_for_task_detection() {
    let jsonl = concat!(
        r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"progress","data":"waiting_for_task"}"#,
    );

    let (mut s, _file) = make_session_with_jsonl(jsonl);
    s.cpu_percent = 0.5; // Low CPU
    monitor::update_tokens(&mut s);

    // Status should be NeedsInput (waiting_for_task + low CPU)
    assert_eq!(s.status, SessionStatus::NeedsInput);
}

#[test]
fn jsonl_missing_file() {
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    s.jsonl_path = Some(std::path::PathBuf::from("/nonexistent/path.jsonl"));

    monitor::update_tokens(&mut s);
    assert_eq!(s.telemetry_status, TelemetryStatus::UnreadableTranscript);
    assert_eq!(s.context_pressure, None);
}

#[test]
fn jsonl_no_path() {
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "test".into(),
        cwd: "/tmp/test".into(),
        started_at: 0,
    };
    let mut s = AgentSession::from_raw(raw);
    // jsonl_path is None

    monitor::update_tokens(&mut s);
    assert_eq!(s.telemetry_status, TelemetryStatus::MissingTranscript);
    assert_eq!(s.context_pressure, None);
}

#[test]
fn subagent_count_tracks_active_transcript_paths() {
    let temp = tempfile::tempdir().unwrap();
    let parent_jsonl = temp.path().join("parent.jsonl");
    write_jsonl(
        &parent_jsonl,
        r#"{"type":"assistant","message":{"model":"gpt-5.4","stop_reason":"end_turn"}}"#,
    );

    let session_id = format!("subagent-rollup-{}", std::process::id());
    let cwd = format!("/tmp/codexctl-rollup-{}", std::process::id());
    let slug = cwd.replace('/', "-");
    let uid = unsafe { libc::getuid() };
    let tasks_dir = std::path::PathBuf::from(format!("/tmp/codex-{uid}"))
        .join(&slug)
        .join(&session_id)
        .join("tasks");
    write_jsonl(
        &tasks_dir.join("agent-1.jsonl"),
        r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn"}}"#,
    );
    write_jsonl(
        &tasks_dir.join("nested/agent-2.jsonl"),
        r#"{"type":"assistant","message":{"model":"gpt-5.4-mini","stop_reason":"end_turn"}}"#,
    );

    let mut s = make_session_with_paths(cwd, session_id, parent_jsonl);
    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 2);
    assert_eq!(s.subagent_count, 2);

    let _ = std::fs::remove_dir_all(
        std::path::PathBuf::from(format!("/tmp/codex-{uid}"))
            .join(&slug)
            .join(&s.session_id),
    );
}

#[test]
fn subagent_count_clears_after_task_files_disappear() {
    let temp = tempfile::tempdir().unwrap();
    let parent_jsonl = temp.path().join("parent.jsonl");
    write_jsonl(
        &parent_jsonl,
        r#"{"type":"assistant","message":{"model":"gpt-5.4","stop_reason":"end_turn"}}"#,
    );

    let session_id = format!("subagent-persist-{}", std::process::id());
    let cwd = format!("/tmp/codexctl-persist-{}", std::process::id());
    let slug = cwd.replace('/', "-");
    let uid = unsafe { libc::getuid() };
    let subagent_root = std::path::PathBuf::from(format!("/tmp/codex-{uid}"))
        .join(&slug)
        .join(&session_id);
    let tasks_dir = subagent_root.join("tasks");
    write_jsonl(
        &tasks_dir.join("agent-1.jsonl"),
        r#"{"type":"assistant","message":{"model":"gpt-5.4","stop_reason":"end_turn"}}"#,
    );

    let mut s = make_session_with_paths(cwd, session_id, parent_jsonl);
    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 1);
    assert_eq!(s.subagent_count, 1);

    std::fs::remove_dir_all(&subagent_root).unwrap();

    discovery::scan_subagents(std::slice::from_mut(&mut s));
    monitor::update_tokens(&mut s);

    assert_eq!(s.active_subagent_count, 0);
    assert_eq!(s.subagent_count, 0);
}

// ────────────────────────────────────────────────────────────────────────────
// Session Formatting Edge Cases
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn context_percent_is_optional() {
    let s = make_session(0.0, 0);
    assert_eq!(s.context_percent(), None);
}

#[test]
fn context_percent_preserves_zero_pressure() {
    let mut s = make_session(0.0, 0);
    s.context_pressure = Some(0);
    assert_eq!(s.context_percent(), Some(0));
}

#[test]
fn context_percent_calculation() {
    let mut s = make_session(0.0, 0);
    s.context_pressure = Some(50);
    assert_eq!(s.context_percent(), Some(50));
}

#[test]
fn sparkline_empty() {
    let s = make_session(0.0, 0);
    assert_eq!(s.format_sparkline(), "-");
}

#[test]
fn sparkline_records_and_renders() {
    let mut s = make_session(0.0, 0);
    s.status = SessionStatus::Processing;
    s.record_activity();
    s.status = SessionStatus::Idle;
    s.record_activity();

    let sparkline = s.format_sparkline();
    assert_eq!(sparkline.chars().count(), 2);
}

#[test]
fn sparkline_ring_buffer_limit() {
    let mut s = make_session(0.0, 0);
    for _ in 0..20 {
        s.status = SessionStatus::Processing;
        s.record_activity();
    }
    // Should be capped at 15
    assert_eq!(s.activity_history.len(), 15);
}

#[test]
fn json_export_format() {
    let mut s = make_session(0.0, 0);
    s.model = "gpt-5.5".into();
    s.context_pressure = Some(42);
    s.elapsed = Duration::from_secs(300);

    let json = s.to_json_value();
    let encoded = serde_json::to_string(&json).unwrap();
    let forbidden: Vec<String> =
        serde_json::from_str(include_str!("fixtures/legacy-forbidden-output-keys.json")).unwrap();

    assert_eq!(json["pid"], 1);
    assert_eq!(json["status"], "Idle");
    assert_eq!(json["model"], "gpt-5.5");
    assert_eq!(json["context_pct"], 42);
    assert_eq!(json["elapsed_secs"], 300);
    for key in forbidden {
        assert!(
            !encoded.contains(&key),
            "session JSON retained forbidden output key {key}"
        );
    }
}

#[test]
fn mem_formatting() {
    let mut s = make_session(0.0, 0);
    assert_eq!(s.format_mem(), "-");

    s.mem_mb = 256.7;
    assert_eq!(s.format_mem(), "257M");
}

#[test]
fn context_bar_formatting() {
    let mut s = make_session(0.0, 0);
    assert_eq!(s.format_context_bar(10), "n/a");

    s.context_pressure = Some(50);
    let bar = s.format_context_bar(10);
    assert!(bar.contains("50%"));
    assert!(bar.contains("█████"));
    assert!(bar.contains("░░░░░"));
}

// ────────────────────────────────────────────────────────────────────────────
// Transcript Discovery Tests (Issue #161)
//
// These tests mutate the HOME env var so projects_dir() resolves to a temp dir.
// A mutex serializes them to prevent concurrent HOME changes across threads.
// ────────────────────────────────────────────────────────────────────────────

use std::sync::Mutex;
static HOME_LOCK: Mutex<()> = Mutex::new(());

struct HomeGuard {
    original: Option<String>,
    _tempdir: tempfile::TempDir,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

fn isolated_home() -> HomeGuard {
    let original = std::env::var("HOME").ok();
    let tempdir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HOME", tempdir.path()) };
    HomeGuard {
        original,
        _tempdir: tempdir,
    }
}

/// Helper: build a fake ~/.codex layout in a temp dir and run resolve_jsonl_paths.
/// Holds HOME_LOCK for the duration.
fn resolve_with_layout(
    cwd: &str,
    session_id: &str,
    slug_on_disk: &str,
) -> (AgentSession, tempfile::TempDir) {
    let _guard = HOME_LOCK.lock().unwrap();

    let home = tempfile::tempdir().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", home.path()) };

    let project_dir = home.path().join(".codex/sessions").join(slug_on_disk);
    std::fs::create_dir_all(&project_dir).unwrap();
    let jsonl_content = r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":1,"cache_creation_input_tokens":523,"cache_read_input_tokens":79425,"output_tokens":937}}}"#;
    std::fs::write(
        project_dir.join(format!("{session_id}.jsonl")),
        format!("{jsonl_content}\n"),
    )
    .unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 86131,
        process_start_identity: None,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        started_at: 1776421121745,
    };
    let mut session = AgentSession::from_raw(raw);
    discovery::resolve_jsonl_paths(std::slice::from_mut(&mut session));

    // Restore HOME
    if let Some(h) = original_home {
        unsafe { std::env::set_var("HOME", h) };
    }

    (session, home)
}

#[test]
fn resolve_jsonl_standard_cwd() {
    let (s, _home) = resolve_with_layout(
        "/Users/testuser/Repos/data-platform-answers",
        "db55eb53-8ff0-45b7-9f8f-0d5dfa51e701",
        "-Users-testuser-Repos-data-platform-answers",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL for standard cwd (no trailing slash)"
    );
}

#[test]
fn resolve_jsonl_trailing_slash_cwd() {
    let (s, _home) = resolve_with_layout(
        "/Users/testuser/Repos/data-platform-answers/",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        "-Users-testuser-Repos-data-platform-answers",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL even when cwd has trailing slash"
    );
}

#[test]
fn resolve_jsonl_cwd_with_hyphens() {
    let (s, _home) = resolve_with_layout(
        "/Users/dev/my-cool-project",
        "11111111-2222-3333-4444-555555555555",
        "-Users-dev-my-cool-project",
    );
    assert!(
        s.jsonl_path.is_some(),
        "should find JSONL when cwd contains hyphens"
    );
}

#[test]
fn resolve_jsonl_encoding_mismatch_fallback() {
    let _guard = HOME_LOCK.lock().unwrap();

    let home = tempfile::tempdir().unwrap();
    let original_home = std::env::var("HOME").ok();
    unsafe { std::env::set_var("HOME", home.path()) };

    let session_id = "deadbeef-1234-5678-9abc-def012345678";
    let cwd = "/Users/testuser/projects/webapp";

    // JSONL under a slug that does NOT match cwd_to_slug(cwd)
    let wrong_slug = "-some-other-encoding-of-the-cwd";
    let project_dir = home.path().join(".codex/sessions").join(wrong_slug);
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(
        project_dir.join(format!("{session_id}.jsonl")),
        r#"{"type":"assistant","message":{"model":"gpt-5.5","stop_reason":"end_turn","usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#,
    ).unwrap();

    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 99999,
        process_start_identity: None,
        session_id: session_id.to_string(),
        cwd: cwd.to_string(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    discovery::resolve_jsonl_paths(std::slice::from_mut(&mut session));

    if let Some(h) = original_home {
        unsafe { std::env::set_var("HOME", h) };
    }

    assert!(
        session.jsonl_path.is_some(),
        "should find JSONL via fallback scan when slug encoding differs"
    );
}

#[test]
fn resolve_jsonl_does_not_guess_latest_for_unknown_process_session() {
    let _guard = HOME_LOCK.lock().unwrap();
    let _home = isolated_home();
    let home_path = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());

    let cwd = "/Users/testuser/projects/webapp";
    let project_dir = home_path.join(".codex/sessions/-Users-testuser-projects-webapp");
    std::fs::create_dir_all(&project_dir).unwrap();
    std::fs::write(project_dir.join("unrelated.jsonl"), "{}\n").unwrap();

    let mut session = AgentSession::from_raw(RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 99998,
        process_start_identity: None,
        session_id: "codex-99998".into(),
        cwd: cwd.into(),
        started_at: 0,
    });
    discovery::resolve_jsonl_paths(std::slice::from_mut(&mut session));

    assert_eq!(session.jsonl_path, None);
}

#[test]
fn resolve_jsonl_telemetry_available_after_resolution() {
    let (mut s, _home) = resolve_with_layout(
        "/Users/testuser/myproject",
        "face0000-face-face-face-faceface0000",
        "-Users-testuser-myproject",
    );
    assert!(s.jsonl_path.is_some(), "precondition: jsonl_path found");

    monitor::update_tokens(&mut s);
    assert_eq!(
        s.telemetry_status,
        TelemetryStatus::Available,
        "telemetry should be Available after parsing JSONL, not {:?}",
        s.telemetry_status
    );
    assert_eq!(s.context_pressure, Some(30));
}
