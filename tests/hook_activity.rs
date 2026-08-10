#![cfg(unix)]

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use coding_brain::brain::decisions::DecisionRecord;
use coding_brain::brain::storage::{
    BrainDb, DecisionIdentity, MigrationCoordinator, OpenRole, StorageDeadline, StorageError,
    StoragePaths,
};
use coding_brain_core::brain_activity::{
    ActivityEvent, ActivityKind, ActivityOutcome, ActivityState, DeliveryState,
    MAX_ACTIVITY_FIELD_BYTES, SessionTarget, SnapshotLimits,
};
use coding_brain_core::lifecycle::test_support::LifecycleStore;
use coding_brain_core::lifecycle::{
    ApplyOutcome, IgnoreReason, LifecycleEvent, LifecycleEventKind, LifecycleIdentity,
    PermissionAction, PermissionDisposition, ProjectedStatus,
};
use coding_brain_core::provider::AgentProvider;
use fs2::FileExt;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

fn secure_home(home: &Path) {
    for path in [
        home.to_path_buf(),
        home.join(".local"),
        home.join(".local/state"),
        home.join(".local/state/coding-brain"),
        home.join(".local/state/coding-brain/brain"),
        home.join(".local/state/coding-brain/hooks"),
    ] {
        if path.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let state_root = home.join(".local/state/coding-brain");
    let legacy_guard = state_root.join("brain/permission-transactions");
    if !state_root.join("db/brain.sqlite3").exists() && legacy_guard.is_dir() {
        fs::set_permissions(legacy_guard, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn prepare_current_storage(home: &Path) {
    secure_home(home);
    let state_root = home.join(".local/state/coding-brain");
    if !state_root.join("db/brain.sqlite3").exists() {
        MigrationCoordinator::at(&state_root)
            .run_non_hook()
            .unwrap();
    }
}

fn open_brain(home: &Path) -> Result<BrainDb, StorageError> {
    BrainDb::open_current(
        &StoragePaths::at(&home.join(".local/state/coding-brain")),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
}

fn lifecycle_snapshot(home: &Path) -> coding_brain_core::lifecycle::LifecycleSnapshot {
    open_brain(home).unwrap().read_lifecycle().unwrap()
}

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

fn permission_payload_for_request(
    cwd: &Path,
    session_id: &str,
    turn_id: &str,
    command: &str,
) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "session_id": session_id,
        "turn_id": turn_id,
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

fn child_fixture(
    fixture: &[u8],
    session_id: &str,
    agent_id: &str,
    turn_id: &str,
    tool_use_id: Option<&str>,
) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(fixture).unwrap();
    payload["session_id"] = serde_json::json!(session_id);
    payload["agent_id"] = serde_json::json!(agent_id);
    payload["turn_id"] = serde_json::json!(turn_id);
    if let Some(tool_use_id) = tool_use_id {
        payload["tool_use_id"] = serde_json::json!(tool_use_id);
    }
    serde_json::to_vec(&payload).unwrap()
}

fn subagent_start_payload(home: &Path, agent_id: &str, turn_id: &str) -> Vec<u8> {
    let payload = child_fixture(
        include_bytes!("fixtures/hooks/subagent-start.json"),
        "root-1",
        agent_id,
        turn_id,
        None,
    );
    with_payload_cwd(payload, home)
}

fn child_pre_payload(home: &Path, agent_id: &str, turn_id: &str, tool_use_id: &str) -> Vec<u8> {
    let payload = child_fixture(
        include_bytes!("fixtures/hooks/codex-child-pre-tool-use.json"),
        "root-1",
        agent_id,
        turn_id,
        Some(tool_use_id),
    );
    with_child_bash_command(payload, home)
}

fn child_post_payload(home: &Path, agent_id: &str, turn_id: &str, tool_use_id: &str) -> Vec<u8> {
    let payload = child_fixture(
        include_bytes!("fixtures/hooks/codex-child-post-tool-use.json"),
        "root-1",
        agent_id,
        turn_id,
        Some(tool_use_id),
    );
    with_child_bash_command(payload, home)
}

fn subagent_stop_payload(home: &Path, agent_id: &str, turn_id: &str) -> Vec<u8> {
    let payload = child_fixture(
        include_bytes!("fixtures/hooks/subagent-stop.json"),
        "root-1",
        agent_id,
        turn_id,
        None,
    );
    with_payload_cwd(payload, home)
}

fn child_permission_payload(home: &Path, agent_id: &str, turn_id: &str) -> Vec<u8> {
    let payload = child_fixture(
        include_bytes!("fixtures/hooks/codex-child-permission-request.json"),
        "root-1",
        agent_id,
        turn_id,
        None,
    );
    let mut value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(value.get("tool_use_id").is_none());
    // Preserve the real child callback's identity shape while using the
    // existing supported-tool decision path for this lifecycle test.
    value["tool_name"] = serde_json::json!("Bash");
    value["tool_input"] = serde_json::json!({"command": "printf child"});
    value["cwd"] = serde_json::json!(home);
    serde_json::to_vec(&value).unwrap()
}

fn child_permission_payload_with_transcript(
    home: &Path,
    agent_id: &str,
    turn_id: &str,
    transcript: &Path,
) -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&child_permission_payload(home, agent_id, turn_id)).unwrap();
    value["transcript_path"] = serde_json::json!(transcript);
    serde_json::to_vec(&value).unwrap()
}

fn child_resume_metadata(
    child_id: &str,
    provider_session_id: &str,
    immediate_parent_id: &str,
) -> String {
    format!(
        "{{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{child_id}\",\"session_id\":\"{provider_session_id}\",\"parent_thread_id\":\"{immediate_parent_id}\",\"cwd\":\"/work\"}}}}\n"
    )
}

fn write_child_resume_transcript(
    path: &Path,
    child_id: &str,
    provider_session_id: &str,
    immediate_parent_id: &str,
    turn_id: &str,
    timestamp: &str,
) {
    fs::write(
        path,
        format!(
            "{}{{\"timestamp\":\"{timestamp}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn_id}\"}}}}\n",
            child_resume_metadata(child_id, provider_session_id, immediate_parent_id),
        ),
    )
    .unwrap();
}

fn one_second_from_now_rfc3339() -> String {
    (OffsetDateTime::now_utc() + time::Duration::seconds(1))
        .format(&Rfc3339)
        .unwrap()
}

fn with_payload_cwd(payload: Vec<u8>, cwd: &Path) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    payload["cwd"] = serde_json::json!(cwd);
    serde_json::to_vec(&payload).unwrap()
}

fn with_child_bash_command(payload: Vec<u8>, cwd: &Path) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    payload["tool_input"] = serde_json::json!({"command": "printf child"});
    payload["cwd"] = serde_json::json!(cwd);
    serde_json::to_vec(&payload).unwrap()
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
    run_provider_permission_hook_from(home, home, provider, antigravity_event, payload)
}

fn run_provider_permission_hook_from(
    home: &Path,
    cwd: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    prepare_current_storage(home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command.args(["--permission-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

fn run_provider_lifecycle_hook(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    prepare_current_storage(home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command.args(["--lifecycle-hook", "--provider", provider]);
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

fn antigravity_invocation_payload(home: &Path, invocation: u64, initial_step: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "invocationNum": invocation,
        "initialNumSteps": initial_step,
        "conversationId": "agy-conversation-1",
        "workspacePaths": [home],
        "transcriptPath": "/tmp/agy-conversation-1/transcript.jsonl",
        "artifactDirectoryPath": "/tmp/agy-conversation-1/artifacts"
    }))
    .unwrap()
}

fn antigravity_permission_payload_for_step(home: &Path, step: u64) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(home, None)).unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    serde_json::to_vec(&payload).unwrap()
}

fn antigravity_stop_payload(home: &Path, execution: u64) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/hooks/antigravity-stop.json")).unwrap();
    payload["executionNum"] = serde_json::json!(execution);
    payload["workspacePaths"] = serde_json::json!([home]);
    serde_json::to_vec(&payload).unwrap()
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

fn unsupported_antigravity_permission_payload(cwd: &Path, step: u64, tool: &str) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(cwd, None)).unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    payload["toolCall"] = serde_json::json!({
        "name": tool,
        "args": {"AbsolutePath": "/tmp/example"}
    });
    serde_json::to_vec(&payload).unwrap()
}

fn antigravity_post_payload(cwd: &Path, step: u64) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/antigravity-post-tool-use.json"
    ))
    .unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    payload["workspacePaths"] = serde_json::json!([cwd]);
    serde_json::to_vec(&payload).unwrap()
}

fn spawn_permission_hook(home: &Path) -> Child {
    spawn_provider_permission_hook_process(home, "codex", None)
}

fn spawn_provider_permission_hook_process(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
) -> Child {
    prepare_current_storage(home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command.args(["--permission-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    command
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

fn provider_permission_case(
    home: &Path,
    provider: AgentProvider,
) -> (&'static str, Option<&'static str>, Vec<u8>) {
    match provider {
        AgentProvider::Codex => ("codex", None, permission_payload(home, "cargo test")),
        AgentProvider::Claude => ("claude", None, claude_permission_payload(home, None)),
        AgentProvider::Antigravity => {
            seed_antigravity_invocation(home, 5);
            (
                "antigravity",
                Some("PreToolUse"),
                antigravity_permission_payload(home, None),
            )
        }
    }
}

fn native_permission_fallback(provider: AgentProvider) -> &'static [u8] {
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => b"",
        AgentProvider::Antigravity => br#"{"decision":"ask","reason":"Coding Brain abstained"}"#,
    }
}

#[derive(Clone, Copy)]
struct ExpectedPermissionIdentity<'a> {
    provider: AgentProvider,
    session_id: &'a str,
    turn_id: &'a str,
    tool_use_id: Option<&'a str>,
    tool: &'a str,
    command: &'a str,
}

fn expected_permission_identity(provider: AgentProvider) -> ExpectedPermissionIdentity<'static> {
    match provider {
        AgentProvider::Codex => ExpectedPermissionIdentity {
            provider,
            session_id: "session-1",
            turn_id: "turn-1",
            tool_use_id: None,
            tool: "Bash",
            command: "cargo test",
        },
        AgentProvider::Claude => ExpectedPermissionIdentity {
            provider,
            session_id: "claude-session-1",
            turn_id: "claude-session-1",
            tool_use_id: None,
            tool: "Bash",
            command: "cargo test",
        },
        AgentProvider::Antigravity => ExpectedPermissionIdentity {
            provider,
            session_id: "agy-conversation-1",
            turn_id: "step-5",
            tool_use_id: Some("step-5"),
            tool: "run_command",
            command: "cargo test",
        },
    }
}

fn permission_activity_events(home: &Path) -> Vec<ActivityEvent> {
    activity(home)
        .read()
        .unwrap()
        .events()
        .iter()
        .filter(|event| event.kind == ActivityKind::Decision)
        .cloned()
        .collect()
}

fn permission_decisions(home: &Path) -> Vec<(DecisionIdentity, DecisionRecord)> {
    let database = open_brain(home).unwrap();
    database
        .learning_decisions_after(None, 4096, 16 * 1024 * 1024)
        .unwrap()
        .into_records()
        .into_iter()
        .map(|record| {
            let decision_id = record.decision_id.as_deref().unwrap();
            let identity = database.decision_identity(decision_id).unwrap().unwrap();
            (identity, record)
        })
        .collect()
}

fn assert_permission_proposal(
    identity: &DecisionIdentity,
    record: &DecisionRecord,
    expected: ExpectedPermissionIdentity<'_>,
) -> String {
    let DecisionIdentity::Permission {
        decision_id,
        provider: actual_provider,
        session_id: actual_session_id,
        turn_id: actual_turn_id,
        tool_use_id: actual_tool_use_id,
        authority_action,
        decision_source,
        ..
    } = identity
    else {
        panic!("expected a permission decision identity")
    };
    assert_eq!(*actual_provider, expected.provider);
    assert_eq!(actual_session_id, expected.session_id);
    assert_eq!(actual_turn_id, expected.turn_id);
    assert_eq!(actual_tool_use_id.as_deref(), expected.tool_use_id);
    assert_eq!(*authority_action, PermissionAction::Allow);
    assert_eq!(decision_source, "model");
    assert_eq!(record.decision_id.as_deref(), Some(decision_id.as_str()));
    assert_eq!(record.provider, expected.provider);
    assert_eq!(record.tool.as_deref(), Some(expected.tool));
    assert_eq!(record.command.as_deref(), Some(expected.command));
    assert_eq!(record.brain_action, "approve");
    decision_id.clone()
}

fn assert_correlated_permission_activity(
    events: &[&ActivityEvent],
    expected: ExpectedPermissionIdentity<'_>,
    expected_states: &[ActivityState],
    decision_id: Option<&str>,
) {
    assert_eq!(
        events.len(),
        expected_states.len(),
        "{:?}",
        expected.provider
    );
    let activity_id = &events[0].activity_id;
    for (event, expected_state) in events.iter().zip(expected_states) {
        let session = event.session.as_ref().unwrap();
        assert_eq!(
            event.kind,
            ActivityKind::Decision,
            "{:?}",
            expected.provider
        );
        assert_eq!(&event.activity_id, activity_id, "{:?}", expected.provider);
        assert_eq!(
            session.provider, expected.provider,
            "{:?}",
            expected.provider
        );
        assert_eq!(
            session.session_id, expected.session_id,
            "{:?}",
            expected.provider
        );
        assert_eq!(
            session.turn_id.as_deref(),
            Some(expected.turn_id),
            "{:?}",
            expected.provider
        );
        assert_eq!(
            session.tool_use_id.as_deref(),
            expected.tool_use_id,
            "{:?}",
            expected.provider
        );
        assert_eq!(
            event.tool.as_deref(),
            Some(expected.tool),
            "{:?}",
            expected.provider
        );
        assert_eq!(event.state, *expected_state, "{:?}", expected.provider);
        let expected_command = (!matches!(
            *expected_state,
            ActivityState::Observed | ActivityState::Evaluating
        ))
        .then_some(expected.command);
        assert_eq!(
            event.normalized_command.as_deref(),
            expected_command,
            "{:?} {expected_state:?}",
            expected.provider
        );
        let terminal_decision = matches!(
            *expected_state,
            ActivityState::Allowed
                | ActivityState::Denied
                | ActivityState::Delivered
                | ActivityState::DeliveryFailed
        )
        .then_some(decision_id)
        .flatten();
        assert_eq!(
            event.decision_id.as_deref(),
            terminal_decision,
            "{:?} {expected_state:?}",
            expected.provider
        );
    }
}

fn spawn_faulted_permission_hook(
    home: &Path,
    payload: &[u8],
    point: &str,
    marker: &Path,
    release: &Path,
) -> Child {
    prepare_current_storage(home);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .arg("--permission-hook")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .env("CBRAIN_TEST_PERMISSION_TX_FAULT", point)
        .env("CBRAIN_TEST_PERMISSION_TX_MARKER", marker)
        .env("CBRAIN_TEST_PERMISSION_TX_RELEASE", release)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child
}

fn permission_transaction_fault_dir(home: &Path) -> PathBuf {
    let path = home.join(".cbrain-test-permission-tx");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn run_permission_hook_with_fault_tuple(
    home: &Path,
    point: Option<&str>,
    marker: Option<&Path>,
    release: Option<&Path>,
) -> Output {
    static REQUEST_ID: AtomicU64 = AtomicU64::new(0);
    install_model_fixture(home, "approve");
    prepare_current_storage(home);
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let payload = permission_payload_for_request(
        home,
        &format!("fault-tuple-session-{request_id}"),
        &format!("fault-tuple-turn-{request_id}"),
        "cargo test",
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command
        .arg("--permission-hook")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(point) = point {
        command.env("CBRAIN_TEST_PERMISSION_TX_FAULT", point);
    }
    if let Some(marker) = marker {
        command.env("CBRAIN_TEST_PERMISSION_TX_MARKER", marker);
    }
    if let Some(release) = release {
        command.env("CBRAIN_TEST_PERMISSION_TX_RELEASE", release);
    }
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(&payload).unwrap();
    wait_child_output(child)
}

#[allow(clippy::too_many_arguments)]
fn spawn_synchronized_faulted_permission_hook(
    home: &Path,
    payload: &[u8],
    inference_marker: &Path,
    inference_release: &Path,
    transaction_marker: &Path,
    transaction_release: &Path,
) -> Child {
    prepare_current_storage(home);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .arg("--permission-hook")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .env("CBRAIN_TEST_MODEL_ACTION", "approve")
        .env("CBRAIN_TEST_INFERENCE_MARKER", inference_marker)
        .env("CBRAIN_TEST_INFERENCE_RELEASE", inference_release)
        .env("CBRAIN_TEST_PERMISSION_TX_FAULT", "after_prepare")
        .env("CBRAIN_TEST_PERMISSION_TX_MARKER", transaction_marker)
        .env("CBRAIN_TEST_PERMISSION_TX_RELEASE", transaction_release)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child
}

fn install_synchronized_model_fixture(home: &Path) {
    install_model_fixture(home, "approve");
    overwrite_curl(
        home,
        r#"dd of=/dev/null 2>/dev/null
printf '%s' "$CBRAIN_TEST_MODEL_ACTION" > "$CBRAIN_TEST_INFERENCE_MARKER"
while [ ! -e "$CBRAIN_TEST_INFERENCE_RELEASE" ]; do sleep 0.01; done
printf '{"response":"{\\"action\\":\\"%s\\",\\"reasoning\\":\\"fixture\\",\\"confidence\\":0.9}"}' "$CBRAIN_TEST_MODEL_ACTION""#,
    );
}

fn spawn_synchronized_permission_hook(
    home: &Path,
    payload: &[u8],
    action: &str,
    marker: &Path,
    release: &Path,
) -> Child {
    spawn_synchronized_provider_permission_hook(home, "codex", payload, action, marker, release)
}

fn spawn_synchronized_provider_permission_hook(
    home: &Path,
    provider: &str,
    payload: &[u8],
    action: &str,
    marker: &Path,
    release: &Path,
) -> Child {
    prepare_current_storage(home);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--permission-hook", "--provider", provider])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .env("CBRAIN_TEST_MODEL_ACTION", action)
        .env("CBRAIN_TEST_INFERENCE_MARKER", marker)
        .env("CBRAIN_TEST_INFERENCE_RELEASE", release)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child
}

fn wait_for_activity_state(home: &Path, expected: ActivityState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if activity(home)
            .read()
            .map(|log| log.events().iter().any(|event| event.state == expected))
            .unwrap_or(false)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected:?} activity"
        );
        std::thread::yield_now();
    }
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("failed to read {}: {error}", path.display()),
    }
}

fn transaction_entries(home: &Path) -> Vec<OsString> {
    let path = home.join(".local/state/coding-brain/brain/permission-transactions");
    let mut entries = match fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("failed to inspect transaction entries: {error}"),
    };
    entries.sort();
    entries
}

fn wait_child_output(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => return child.wait_with_output().unwrap(),
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "timed out waiting for hook: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

fn wait_children_output(mut children: Vec<Child>) -> Vec<Output> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if children
            .iter_mut()
            .all(|child| child.try_wait().unwrap().is_some())
        {
            return children
                .into_iter()
                .map(|child| child.wait_with_output().unwrap())
                .collect();
        }
        if Instant::now() >= deadline {
            for child in &mut children {
                let _ = child.kill();
            }
            let stderr = children
                .into_iter()
                .map(|child| child.wait_with_output().unwrap().stderr)
                .map(|stderr| String::from_utf8_lossy(&stderr).into_owned())
                .collect::<Vec<_>>();
            panic!("timed out waiting for permission hooks: {stderr:?}");
        }
        std::thread::yield_now();
    }
}

fn decision_records(home: &Path) -> Vec<serde_json::Value> {
    let events = activity(home).read().unwrap().events;
    open_brain(home)
        .unwrap()
        .learning_decisions_after(None, 4096, 16 * 1024 * 1024)
        .unwrap()
        .into_records()
        .into_iter()
        .map(|record| {
            let session_id = record.decision_id.as_deref().and_then(|decision_id| {
                events.iter().find_map(|event| {
                    (event.decision_id.as_deref() == Some(decision_id))
                        .then(|| {
                            event
                                .session
                                .as_ref()
                                .map(|session| session.session_id.clone())
                        })
                        .flatten()
                })
            });
            serde_json::json!({"session_id": session_id})
        })
        .collect()
}

fn assert_owner_only(path: &Path, expected_mode: u32) {
    let metadata = fs::metadata(path).unwrap();
    assert_eq!(metadata.mode() & 0o777, expected_mode, "{}", path.display());
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
}

fn isolated_path(home: &Path) -> OsString {
    let mut paths = vec![home.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

#[test]
#[ignore = "superseded by atomic SQLite permission fault coverage"]
fn permission_transaction_process_kill_recovers_once_after_proposal_persistence() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    overwrite_curl(home.path(), "dd of=/dev/null 2>/dev/null\nexit 28");
    let payload = permission_payload(home.path(), "cargo test");
    let fault_dir = permission_transaction_fault_dir(home.path());
    let marker = fault_dir.join("after-proposal.ready");
    let release = fault_dir.join("never.release");
    let mut child =
        spawn_faulted_permission_hook(home.path(), &payload, "after_proposal", &marker, &release);

    wait_for_path(&marker);
    let transaction_dir = home
        .path()
        .join(".local/state/coding-brain/brain/permission-transactions");
    let journals = transaction_entries(home.path());
    assert_eq!(journals.len(), 1);
    assert_eq!(decision_records(home.path()).len(), 1);
    assert_owner_only(&transaction_dir, 0o700);
    assert_owner_only(&transaction_dir.join(&journals[0]), 0o600);

    child.kill().unwrap();
    let killed = child.wait_with_output().unwrap();
    assert!(!killed.status.success());

    for recovery_number in 1..=2 {
        let recovery = run_permission_hook(home.path(), &payload);
        assert!(recovery.status.success());
        assert!(recovery.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&recovery.stderr).contains("duplicate"),
            "recovery {recovery_number}: {}",
            String::from_utf8_lossy(&recovery.stderr)
        );
        assert_eq!(decision_records(home.path()).len(), 1);
        let events = activity(home.path()).read().unwrap().events().to_vec();
        assert_eq!(
            events.iter().map(|event| event.state).collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Error,
            ]
        );
        let reason = events[2].reasoning.as_deref().unwrap();
        assert!(reason.starts_with("Brain query failed:"), "{reason}");
        assert!(reason.len() <= MAX_ACTIVITY_FIELD_BYTES);
        assert!(transaction_entries(home.path()).is_empty());
    }
}

#[test]
#[ignore = "legacy JSONL permission fault variables were removed"]
fn permission_transaction_fault_variables_require_an_exact_complete_tuple() {
    let home = tempfile::tempdir().unwrap();
    let fault_dir = permission_transaction_fault_dir(home.path());
    let marker = fault_dir.join("valid.ready");
    let release = fault_dir.join("valid.release");
    fs::write(&release, b"release").unwrap();

    for (point, marker_path, release_path) in [
        (None, None, None),
        (Some("after_prepare"), None, None),
        (None, Some(marker.as_path()), None),
        (None, None, Some(release.as_path())),
        (Some("after_prepare"), Some(marker.as_path()), None),
        (Some("after_prepare"), None, Some(release.as_path())),
        (None, Some(marker.as_path()), Some(release.as_path())),
    ] {
        let output =
            run_permission_hook_with_fault_tuple(home.path(), point, marker_path, release_path);
        assert!(output.status.success());
        assert!(!marker.exists());
    }

    let relative_marker = Path::new("relative.ready");
    let relative_release = Path::new("relative.release");
    fs::write(home.path().join(relative_release), b"release").unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(relative_marker),
        Some(relative_release),
    );
    assert!(output.status.success());
    assert!(!home.path().join(relative_marker).exists());

    let equal = fault_dir.join("equal.sentinel");
    fs::write(&equal, b"unchanged").unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&equal),
        Some(&equal),
    );
    assert!(output.status.success());
    assert_eq!(fs::read(&equal).unwrap(), b"unchanged");

    let other_dir = home.path().join("other");
    fs::create_dir(&other_dir).unwrap();
    let different_parent_marker = fault_dir.join("different-parent.ready");
    let different_parent_release = other_dir.join("different-parent.release");
    fs::write(&different_parent_release, b"release").unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&different_parent_marker),
        Some(&different_parent_release),
    );
    assert!(output.status.success());
    assert!(!different_parent_marker.exists());

    let outside_home = tempfile::tempdir().unwrap();
    let outside_dir = permission_transaction_fault_dir(outside_home.path());
    let outside_marker = outside_dir.join("outside.ready");
    let outside_release = outside_dir.join("outside.release");
    fs::write(&outside_release, b"release").unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&outside_marker),
        Some(&outside_release),
    );
    assert!(output.status.success());
    assert!(!outside_marker.exists());

    let existing_marker = fault_dir.join("existing.sentinel");
    fs::write(&existing_marker, b"unchanged").unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&existing_marker),
        Some(&release),
    );
    assert!(output.status.success());
    assert_eq!(fs::read(&existing_marker).unwrap(), b"unchanged");

    let symlink_target = home.path().join("symlink-target.sentinel");
    let symlink_marker = fault_dir.join("symlink.ready");
    fs::write(&symlink_target, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&symlink_target, &symlink_marker).unwrap();
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&symlink_marker),
        Some(&release),
    );
    assert!(output.status.success());
    assert_eq!(fs::read(&symlink_target).unwrap(), b"unchanged");

    let wrong_marker = fault_dir.join("wrong.ready");
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_terminal"),
        Some(&wrong_marker),
        Some(&release),
    );
    assert!(output.status.success());
    assert!(!wrong_marker.exists());

    let complete_marker = fault_dir.join("complete.ready");
    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&complete_marker),
        Some(&release),
    );
    assert!(output.status.success());
    #[cfg(debug_assertions)]
    {
        assert_eq!(fs::read(&complete_marker).unwrap(), b"after_prepare");
        assert_owner_only(&complete_marker, 0o600);
    }
    #[cfg(not(debug_assertions))]
    assert!(!complete_marker.exists());
}

#[test]
fn permission_transaction_fault_rejects_unsafe_fault_directory_metadata() {
    let home = tempfile::tempdir().unwrap();
    let fault_dir = permission_transaction_fault_dir(home.path());
    fs::set_permissions(&fault_dir, fs::Permissions::from_mode(0o755)).unwrap();
    let marker = fault_dir.join("unsafe.ready");
    let release = fault_dir.join("unsafe.release");
    fs::write(&release, b"release").unwrap();

    let output = run_permission_hook_with_fault_tuple(
        home.path(),
        Some("after_prepare"),
        Some(&marker),
        Some(&release),
    );

    assert!(output.status.success());
    assert!(!marker.exists());
}

#[test]
#[ignore = "superseded by atomic SQLite permission concurrency coverage"]
fn parallel_permission_transactions_use_distinct_owner_only_journals() {
    let home = tempfile::tempdir().unwrap();
    install_synchronized_model_fixture(home.path());
    let fault_dir = permission_transaction_fault_dir(home.path());
    let transaction_release = fault_dir.join("parallel.release");
    let mut requests = Vec::new();
    for index in 1..=3 {
        let session_id = format!("parallel-session-{index}");
        let turn_id = format!("parallel-turn-{index}");
        let command = format!("printf parallel-{index}");
        let payload = permission_payload_for_request(home.path(), &session_id, &turn_id, &command);
        let inference_marker = home
            .path()
            .join(format!("parallel-inference-{index}.ready"));
        let inference_release = home
            .path()
            .join(format!("parallel-inference-{index}.release"));
        let transaction_marker = fault_dir.join(format!("parallel-transaction-{index}.ready"));
        let child = spawn_synchronized_faulted_permission_hook(
            home.path(),
            &payload,
            &inference_marker,
            &inference_release,
            &transaction_marker,
            &transaction_release,
        );
        requests.push((
            session_id,
            command,
            inference_marker,
            inference_release,
            transaction_marker,
            child,
        ));
    }
    for (_, _, inference_marker, _, _, _) in &requests {
        wait_for_path(inference_marker);
    }
    for (_, _, _, inference_release, transaction_marker, _) in &requests {
        fs::write(inference_release, b"release").unwrap();
        wait_for_path(transaction_marker);
    }
    let transaction_dir = home
        .path()
        .join(".local/state/coding-brain/brain/permission-transactions");
    let journals = transaction_entries(home.path());
    assert_eq!(journals.len(), requests.len());
    assert_owner_only(&transaction_dir, 0o700);
    for journal in &journals {
        assert_owner_only(&transaction_dir.join(journal), 0o600);
    }

    fs::write(&transaction_release, b"release").unwrap();
    let outputs = wait_children_output(
        requests
            .into_iter()
            .map(|(_, _, _, _, _, child)| child)
            .collect(),
    );
    for output in outputs {
        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["hookSpecificOutput"]
                ["decision"]["behavior"],
            "allow"
        );
    }

    let proposals = decision_records(home.path());
    let events = activity(home.path()).read().unwrap().events().to_vec();
    for index in 1..=3 {
        let session_id = format!("parallel-session-{index}");
        let command = format!("printf parallel-{index}");
        assert_eq!(
            proposals
                .iter()
                .filter(|proposal| proposal["session_id"] == session_id)
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event
                        .session
                        .as_ref()
                        .is_some_and(|session| session.session_id == session_id)
                        && event.normalized_command.as_deref() == Some(command.as_str())
                        && event.state == ActivityState::Allowed
                })
                .count(),
            1
        );
    }
    assert_eq!(proposals.len(), 3);
    assert!(transaction_entries(home.path()).is_empty());
}

#[test]
fn independent_permission_process_burst_eventually_commits_every_request() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let (payloads, requests): (Vec<_>, Vec<_>) = (0..8)
        .map(|index| {
            let payload = permission_payload_for_request(
                home.path(),
                &format!("burst-session-{index}"),
                &format!("burst-turn-{index}"),
                &format!("printf burst-{index}"),
            );
            let mut child = spawn_permission_hook(home.path());
            child.stdin.take().unwrap().write_all(&payload).unwrap();
            (payload, child)
        })
        .unzip();

    for (index, (payload, mut output)) in payloads
        .iter()
        .zip(wait_children_output(requests))
        .enumerate()
    {
        assert!(
            output.status.success(),
            "burst {index}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if output.stdout.is_empty() {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(
                    "permission transaction admission blocked: duplicate or already active permission request"
                ),
                "burst {index}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output = run_permission_hook(home.path(), payload);
            assert!(
                output.status.success(),
                "burst {index} retry: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "burst {index}: invalid response ({error}); stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                )
            });
        assert_eq!(
            response["hookSpecificOutput"]["decision"]["behavior"], "allow",
            "burst {index}"
        );
    }

    let decisions = permission_decisions(home.path());
    let events = permission_activity_events(home.path());
    assert_eq!(decisions.len(), 8);
    assert_eq!(events.len(), 32);
    let mut activity_ids = Vec::new();
    for index in 0..8 {
        let session_id = format!("burst-session-{index}");
        let turn_id = format!("burst-turn-{index}");
        let command = format!("printf burst-{index}");
        let proposals = decisions
            .iter()
            .filter(|(identity, record)| {
                matches!(
                    identity,
                    DecisionIdentity::Permission {
                        provider: AgentProvider::Codex,
                        session_id: actual_session_id,
                        turn_id: actual_turn_id,
                        tool_use_id: None,
                        ..
                    } if actual_session_id == &session_id && actual_turn_id == &turn_id
                ) && record.command.as_deref() == Some(command.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(proposals.len(), 1, "burst {index} proposal identity");
        let (identity, record) = proposals[0];
        let expected = ExpectedPermissionIdentity {
            provider: AgentProvider::Codex,
            session_id: &session_id,
            turn_id: &turn_id,
            tool_use_id: None,
            tool: "Bash",
            command: &command,
        };
        let decision_id = assert_permission_proposal(identity, record, expected);
        let correlated_events = events
            .iter()
            .filter(|event| {
                event.session.as_ref().is_some_and(|session| {
                    session.provider == AgentProvider::Codex
                        && session.session_id == session_id
                        && session.turn_id.as_deref() == Some(turn_id.as_str())
                })
            })
            .collect::<Vec<_>>();
        assert_correlated_permission_activity(
            &correlated_events,
            expected,
            &[
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::Delivered,
            ],
            Some(&decision_id),
        );
        let activity_id = correlated_events[0].activity_id.clone();
        assert!(!activity_ids.contains(&activity_id), "burst {index}");
        activity_ids.push(activity_id);
    }
}

#[test]
fn same_request_model_winner_is_exclusive_in_both_action_orders() {
    for (winner_action, loser_action) in [("approve", "deny"), ("deny", "approve")] {
        let home = tempfile::tempdir().unwrap();
        install_synchronized_model_fixture(home.path());
        let payload = permission_payload(home.path(), "cargo test");
        let winner_marker = home.path().join(format!("winner-{winner_action}.ready"));
        let winner_release = home.path().join(format!("winner-{winner_action}.release"));
        let loser_marker = home.path().join(format!("loser-{loser_action}.ready"));
        let loser_release = home.path().join(format!("loser-{loser_action}.release"));
        let winner = spawn_synchronized_permission_hook(
            home.path(),
            &payload,
            winner_action,
            &winner_marker,
            &winner_release,
        );
        wait_for_path(&winner_marker);

        let state_root = home.path().join(".local/state/coding-brain");
        let lifecycle_path = state_root.join("hooks/lifecycle.json");
        let activity_path = state_root.join("activity.jsonl");
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let before = (
            optional_bytes(&lifecycle_path),
            optional_bytes(&activity_path),
            optional_bytes(&decisions_path),
            transaction_entries(home.path()),
        );

        let loser = spawn_synchronized_permission_hook(
            home.path(),
            &payload,
            loser_action,
            &loser_marker,
            &loser_release,
        );
        let loser_output = wait_child_output(loser);

        assert!(loser_output.status.success());
        assert!(loser_output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&loser_output.stderr).contains("already active"));
        assert!(!loser_marker.exists(), "loser reached model inference");
        assert_eq!(optional_bytes(&lifecycle_path), before.0);
        assert_eq!(optional_bytes(&activity_path), before.1);
        assert_eq!(optional_bytes(&decisions_path), before.2);
        assert_eq!(transaction_entries(home.path()), before.3);

        fs::write(&winner_release, b"release").unwrap();
        let winner_output = wait_child_output(winner);
        assert!(winner_output.status.success());
        assert!(
            winner_output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&winner_output.stderr)
        );
        let expected = if winner_action == "approve" {
            "allow"
        } else {
            "deny"
        };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&winner_output.stdout).unwrap()["hookSpecificOutput"]
                ["decision"]["behavior"],
            expected
        );
    }
}

#[test]
fn same_request_model_allow_holder_excludes_provider_policy_deny_process() {
    let home = tempfile::tempdir().unwrap();
    install_synchronized_model_fixture(home.path());
    let allow_payload = claude_permission_payload(home.path(), None);
    let deny_payload = claude_permission_payload(home.path(), Some("deny"));
    let allow_marker = home.path().join("allow.ready");
    let allow_release = home.path().join("allow.release");
    let deny_marker = home.path().join("deny.ready");
    let deny_release = home.path().join("deny.release");
    fs::write(&deny_release, b"release").unwrap();
    let allow = spawn_synchronized_provider_permission_hook(
        home.path(),
        "claude",
        &allow_payload,
        "approve",
        &allow_marker,
        &allow_release,
    );
    wait_for_path(&allow_marker);

    let state_root = home.path().join(".local/state/coding-brain");
    let lifecycle_path = state_root.join("hooks/lifecycle.json");
    let activity_path = state_root.join("activity.jsonl");
    let decisions_path = state_root.join("brain/decisions.jsonl");
    let before = (
        optional_bytes(&lifecycle_path),
        optional_bytes(&activity_path),
        optional_bytes(&decisions_path),
        transaction_entries(home.path()),
    );

    let deny = spawn_synchronized_provider_permission_hook(
        home.path(),
        "claude",
        &deny_payload,
        "deny",
        &deny_marker,
        &deny_release,
    );
    let deny_output = wait_child_output(deny);

    assert!(deny_output.status.success());
    assert!(deny_output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&deny_output.stderr).contains("already active"));
    assert!(!deny_marker.exists(), "deny loser reached model inference");
    assert_eq!(optional_bytes(&lifecycle_path), before.0);
    assert_eq!(optional_bytes(&activity_path), before.1);
    assert_eq!(optional_bytes(&decisions_path), before.2);
    assert_eq!(transaction_entries(home.path()), before.3);

    fs::write(&allow_release, b"release").unwrap();
    let allow_output = wait_child_output(allow);
    assert!(allow_output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&allow_output.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "allow"
    );
}

#[test]
#[ignore = "legacy lifecycle lock orchestration was replaced by SQLite admission"]
fn same_request_provider_policy_deny_holder_excludes_model_allow_process() {
    let home = tempfile::tempdir().unwrap();
    install_synchronized_model_fixture(home.path());
    let allow_payload = claude_permission_payload(home.path(), None);
    let deny_payload = claude_permission_payload(home.path(), Some("deny"));
    let state_root = home.path().join(".local/state/coding-brain");
    let lifecycle = LifecycleStore::at(&state_root);
    fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
    let lifecycle_lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(lifecycle.lock_path())
        .unwrap();
    FileExt::lock_shared(&lifecycle_lock).unwrap();
    let deny_marker = home.path().join("deny.ready");
    let deny_release = home.path().join("deny.release");
    fs::write(&deny_release, b"release").unwrap();
    let deny = spawn_synchronized_provider_permission_hook(
        home.path(),
        "claude",
        &deny_payload,
        "deny",
        &deny_marker,
        &deny_release,
    );
    wait_for_activity_state(home.path(), ActivityState::Evaluating);
    let decisions_path = state_root.join("brain/decisions.jsonl");
    wait_for_path(&decisions_path);

    let lifecycle_path = lifecycle.snapshot_path();
    let activity_path = state_root.join("activity.jsonl");
    let before = (
        optional_bytes(&lifecycle_path),
        optional_bytes(&activity_path),
        optional_bytes(&decisions_path),
        transaction_entries(home.path()),
    );
    let allow_marker = home.path().join("allow.ready");
    let allow_release = home.path().join("allow.release");
    fs::write(&allow_release, b"release").unwrap();
    let allow = spawn_synchronized_provider_permission_hook(
        home.path(),
        "claude",
        &allow_payload,
        "approve",
        &allow_marker,
        &allow_release,
    );
    let allow_output = wait_child_output(allow);

    assert!(allow_output.status.success());
    assert!(allow_output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&allow_output.stderr).contains("already active"));
    assert!(
        !allow_marker.exists(),
        "allow loser reached model inference"
    );
    assert_eq!(optional_bytes(&lifecycle_path), before.0);
    assert_eq!(optional_bytes(&activity_path), before.1);
    assert_eq!(optional_bytes(&decisions_path), before.2);
    assert_eq!(transaction_entries(home.path()), before.3);

    FileExt::unlock(&lifecycle_lock).unwrap();
    let deny_output = wait_child_output(deny);
    assert!(deny_output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&deny_output.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "deny"
    );
    assert!(
        !deny_marker.exists(),
        "provider-policy deny invoked the model"
    );
}

#[test]
#[ignore = "legacy JSONL journal recovery was removed"]
fn retained_allow_journal_cannot_be_completed_by_same_request_provider_policy_deny() {
    let home = tempfile::tempdir().unwrap();
    let fake_model = install_model_fixture(home.path(), "approve");
    let activity_path = home.path().join(".local/state/coding-brain/activity.jsonl");
    let saved_activity_path = home.path().join("activity-before-allow-failure.jsonl");
    overwrite_curl(
        home.path(),
        &format!(
            "dd of=/dev/null 2>/dev/null\nprintf 1 > '{}'\nmv '{}' '{}'\nmkdir '{}'\nprintf '%s' '{{\"response\":\"{{\\\"action\\\":\\\"approve\\\",\\\"reasoning\\\":\\\"fixture\\\",\\\"confidence\\\":0.9}}\"}}'",
            fake_model.script.with_extension("count").display(),
            activity_path.display(),
            saved_activity_path.display(),
            activity_path.display(),
        ),
    );
    let allow_payload = claude_permission_payload(home.path(), None);

    let allow = run_provider_permission_hook(home.path(), "claude", None, &allow_payload);

    assert!(allow.status.success());
    assert!(allow.stdout.is_empty());
    assert!(String::from_utf8_lossy(&allow.stderr).contains("permission transaction"));
    assert_eq!(transaction_entries(home.path()).len(), 1);
    fs::remove_dir(&activity_path).unwrap();
    fs::rename(&saved_activity_path, &activity_path).unwrap();

    let deny_payload = claude_permission_payload(home.path(), Some("deny"));
    let recovery = run_provider_permission_hook(home.path(), "claude", None, &deny_payload);

    assert!(recovery.status.success());
    assert!(recovery.stdout.is_empty());
    assert!(String::from_utf8_lossy(&recovery.stderr).contains("duplicate"));
    assert_eq!(fake_model.request_count(), 1);
    assert!(transaction_entries(home.path()).is_empty());
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(
        events
            .iter()
            .any(|event| event.state == ActivityState::Error)
    );
    assert!(
        events
            .iter()
            .all(|event| event.state != ActivityState::Allowed)
    );
    let lifecycle = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let key = coding_brain_core::provider::AgentSessionKey::native(
        AgentProvider::Claude,
        "claude-session-1",
    )
    .storage_key();
    assert_eq!(
        lifecycle.read().unwrap().snapshot.unwrap().sessions[&key].projected_status,
        Some(ProjectedStatus::NeedsInput)
    );
}

#[test]
#[ignore = "legacy request-lock process recovery was replaced by SQLite admission"]
fn killed_same_request_holder_releases_guard_for_next_process() {
    let home = tempfile::tempdir().unwrap();
    install_synchronized_model_fixture(home.path());
    let payload = permission_payload(home.path(), "cargo test");
    let holder_marker = home.path().join("holder.ready");
    let holder_release = home.path().join("holder.release");
    let mut holder = spawn_synchronized_permission_hook(
        home.path(),
        &payload,
        "approve",
        &holder_marker,
        &holder_release,
    );
    wait_for_path(&holder_marker);
    holder.kill().unwrap();
    let killed = holder.wait_with_output().unwrap();
    assert!(!killed.status.success());

    let next_marker = home.path().join("next.ready");
    let next_release = home.path().join("next.release");
    fs::write(&next_release, b"release").unwrap();
    let next = spawn_synchronized_permission_hook(
        home.path(),
        &payload,
        "deny",
        &next_marker,
        &next_release,
    );
    let output = wait_child_output(next);

    assert!(output.status.success());
    assert!(next_marker.exists());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "deny"
    );
}

fn run_lifecycle_hook(home: &Path, payload: &[u8]) -> Output {
    prepare_current_storage(home);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
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
    prepare_current_storage(home);
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
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
fn recovery_hook_without_trusted_live_link_persists_stop_without_recovery() {
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
        "cbrain recovery hook: Stop persistence failed\n"
    );
    assert!(!lifecycle_snapshot(home.path()).sessions.is_empty());
    assert!(activity(home.path()).read().unwrap().events().is_empty());
}

struct TestActivityStore {
    home: PathBuf,
}

struct TestActivityLog {
    events: Vec<coding_brain_core::brain_activity::ActivityEvent>,
}

impl TestActivityLog {
    fn events(&self) -> &[coding_brain_core::brain_activity::ActivityEvent] {
        &self.events
    }
}

impl TestActivityStore {
    fn read(&self) -> Result<TestActivityLog, StorageError> {
        let mut records = open_brain(&self.home)?
            .read_activity_page(None, 4096, 16 * 1024 * 1024)?
            .events;
        records.sort_by_key(|record| record.cursor);
        Ok(TestActivityLog {
            events: records.into_iter().map(|record| record.event).collect(),
        })
    }

    fn snapshot(
        &self,
        limits: SnapshotLimits,
    ) -> Result<coding_brain_core::brain_activity::ActivitySnapshot, StorageError> {
        let mut page = open_brain(&self.home)?.read_activity_page(None, 4096, 16 * 1024 * 1024)?;
        page.events.sort_by_key(|record| record.cursor);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .try_into()
            .unwrap();
        Ok(page.project_complete_window(limits, now_ms))
    }
}

fn activity(home: &Path) -> TestActivityStore {
    TestActivityStore {
        home: home.to_path_buf(),
    }
}

fn seed_antigravity_invocation(home: &Path, initial_step: u64) {
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-conversation-1".into(),
        Some("invocation-1".into()),
        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
        home.to_path_buf(),
    )
    .unwrap();
    let event = LifecycleEvent::from_parts_with_turn_initial_step(
        identity,
        LifecycleEventKind::UserPromptSubmit,
        Some(initial_step),
    )
    .unwrap();
    assert_eq!(
        LifecycleStore::at(home.join(".local/state/coding-brain"))
            .record(event)
            .unwrap(),
        ApplyOutcome::Applied
    );
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
        IgnoreReason::Duplicate => {
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home, 5);
            }
            record(
                LifecycleEvent::permission(identity(turn_id), PermissionDisposition::Decided)
                    .unwrap(),
            );
        }
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
        IgnoreReason::ActiveSubagentCapacity
        | IgnoreReason::SequenceExhausted
        | IgnoreReason::UnprovenSubagent
        | IgnoreReason::ProviderSessionMismatch
        | IgnoreReason::SubagentTurnMismatch => unreachable!(),
    }
}

struct FakeModel {
    script: PathBuf,
}

#[test]
fn fake_model_request_count_only_treats_a_missing_counter_as_zero() {
    let home = tempfile::tempdir().unwrap();
    let fake_model = FakeModel {
        script: home.path().join("bin/curl"),
    };
    let counter = fake_model.script.with_extension("count");

    assert_eq!(fake_model.request_count(), 0);

    fs::create_dir_all(&counter).unwrap();
    assert!(
        std::panic::catch_unwind(|| fake_model.request_count()).is_err(),
        "an unreadable counter must fail the test"
    );
    fs::remove_dir(&counter).unwrap();

    fs::write(&counter, "not-a-number").unwrap();
    assert!(
        std::panic::catch_unwind(|| fake_model.request_count()).is_err(),
        "a malformed counter must fail the test"
    );
}

impl FakeModel {
    fn request_count(&self) -> u64 {
        let counter = self.script.with_extension("count");
        let count = match fs::read_to_string(&counter) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
            Err(error) => panic!(
                "failed to read fake-model request counter {}: {error}",
                counter.display()
            ),
        };
        count.parse().unwrap_or_else(|error| {
            panic!(
                "invalid fake-model request counter {}: {error}",
                counter.display()
            )
        })
    }
}

fn install_model_fixture(home: &Path, action: &str) -> FakeModel {
    install_model_fixture_with_confidence(home, action, 0.9)
}

fn install_model_fixture_with_confidence(home: &Path, action: &str, confidence: f64) -> FakeModel {
    install_model_fixture_full(home, action, confidence, None)
}

fn install_model_fixture_full(
    home: &Path,
    action: &str,
    confidence: f64,
    message: Option<&str>,
) -> FakeModel {
    let config = home.join(".config/coding-brain/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        config,
        "[brain]\nenabled = true\nendpoint = \"http://brain.example.test/api/generate\"\n",
    )
    .unwrap();
    install_gate_mode_fixture(home, "auto");
    install_fake_model(home, action, confidence, message)
}

fn install_default_model_fixture(home: &Path, mode: &str, action: &str) -> FakeModel {
    install_gate_mode_fixture(home, mode);
    install_fake_model(home, action, 0.9, None)
}

fn install_gate_mode_fixture(home: &Path, mode: &str) {
    let gate_mode = home.join(".local/state/coding-brain/brain/gate-mode");
    fs::create_dir_all(gate_mode.parent().unwrap()).unwrap();
    fs::write(gate_mode, format!("{mode}\n")).unwrap();
}

fn install_fake_model(
    home: &Path,
    action: &str,
    confidence: f64,
    message: Option<&str>,
) -> FakeModel {
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
            "#!/bin/sh\nset -eu\ncount=0\nif [ -r \"${{0}}.count\" ]; then\n  IFS= read -r count < \"${{0}}.count\" || true\nfi\ncount=$((count + 1))\nprintf '%s' \"$count\" > \"${{0}}.count\"\nprintf '%s\\n' \"$@\" > \"${{0}}.args\"\ndd of=\"${{0}}.stdin\" 2>/dev/null\nprintf '%s' '{response}'\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
    FakeModel { script: curl }
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
#[ignore = "legacy request-lock storage was removed"]
fn invalid_request_lock_storage_blocks_deterministic_deny_before_mutation() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain")).unwrap();
    fs::write(
        home.path().join(".local/state/coding-brain/brain"),
        b"occupied",
    )
    .unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "rm -rf /"));

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("request lock"));
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(events.is_empty());
    let snapshot = activity(home.path())
        .snapshot(SnapshotLimits::default())
        .unwrap();
    assert!(snapshot.attention.is_empty());
    assert_eq!(snapshot.unresolved_count, 0);
    assert!(snapshot.recent.is_empty());
}

#[test]
#[ignore = "legacy request-lock and activity JSONL storage were removed"]
fn invalid_request_lock_and_activity_storage_emit_no_deterministic_deny() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain")).unwrap();
    fs::write(
        home.path().join(".local/state/coding-brain/brain"),
        b"occupied",
    )
    .unwrap();
    fs::create_dir_all(home.path().join(".local/state/coding-brain/activity.jsonl")).unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "rm -rf /"));

    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("request lock"));
}

fn assert_safety_deny(home: &Path, expected_rule_id: &str) {
    let events = activity(home).read().unwrap().events().to_vec();
    let denied = events
        .iter()
        .find(|event| event.state == ActivityState::Denied)
        .expect("missing deterministic deny activity");
    assert_eq!(denied.rule_id.as_deref(), Some(expected_rule_id));
    assert!(
        events
            .iter()
            .all(|event| event.state != ActivityState::Allowed)
    );
}

fn shell_permission_payload(
    home: &Path,
    provider: AgentProvider,
    command: &str,
    policy: Option<&str>,
) -> (&'static str, Option<&'static str>, Vec<u8>) {
    match provider {
        AgentProvider::Codex => ("codex", None, permission_payload(home, command)),
        AgentProvider::Claude => {
            let mut payload: serde_json::Value =
                serde_json::from_slice(&claude_permission_payload(home, policy)).unwrap();
            payload["tool_input"]["command"] = serde_json::json!(command);
            ("claude", None, serde_json::to_vec(&payload).unwrap())
        }
        AgentProvider::Antigravity => {
            let mut payload: serde_json::Value =
                serde_json::from_slice(&antigravity_permission_payload(home, policy)).unwrap();
            payload["toolCall"]["args"]["CommandLine"] = serde_json::json!(command);
            (
                "antigravity",
                Some("PreToolUse"),
                serde_json::to_vec(&payload).unwrap(),
            )
        }
    }
}

fn assert_needs_input(home: &Path, provider: AgentProvider, session_id: &str) {
    let events = activity(home).read().unwrap().events().to_vec();
    assert!(
        events
            .iter()
            .any(|event| event.state == ActivityState::Abstained),
        "{provider:?}: missing abstained activity"
    );
    let _ = (provider, session_id);
}

#[test]
fn reopened_shell_safety_corpus_denies_before_model_inference_for_every_provider() {
    for (command, expected_rule_id) in [
        (
            "/bin/r[]m] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "/bin/r[\\m] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "/bin/r[m] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "rm>/dev/null --no-preserve-root -rf /",
            "irreversible-root-delete",
        ),
        (
            ">'>' rm --no-preserve-root -rf /",
            "irreversible-root-delete",
        ),
        (
            "rm --no-preserve-root -rf /{,}",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "FLAGS=-rf; rm $FLAGS /",
            "unsafe-recursive-delete-expansion",
        ),
        ("rm -rf $'\\x2f'", "unsafe-recursive-delete-expansion"),
        (
            "cat <(rm --no-preserve-root -rf /)",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "{ rm --no-preserve-root -rf /; }",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "/bin/r[\"m\"] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "/bin/r[!]] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "/bin/r[^]] --no-preserve-root -rf /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "rm --no-preserve-root -rf /{'',}",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "X='safe -rf'; rm -f $X /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "IFS=e; X=safe-rf; rm -f $X /",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "ROOT=/tmp; export ROOT=/; rm -rf \"$ROOT\"",
            "unsafe-recursive-delete-expansion",
        ),
        ("HOME=-rf; rm -f ~ /", "unsafe-recursive-delete-expansion"),
        ("PWD=-Rf; rm -f ~+ /", "unsafe-recursive-delete-expansion"),
        ("rm --rec --no-preserve-root /", "irreversible-root-delete"),
        ("exec -ca display rm -rf /", "irreversible-root-delete"),
        (
            "busybox env sh -c 'rm --no-preserve-root -rf /'",
            "irreversible-root-delete",
        ),
        (
            "busybox time sh -c 'rm --no-preserve-root -rf /'",
            "irreversible-root-delete",
        ),
        (
            "/usr/bin/time -o \"$@\" 'rm --no-preserve-root -rf /'",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "builtin exec /usr/bin/env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "builtin command busybox time -f \"$@\" 'rm --no-preserve-root -rf /'",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "env -S 'rm --no-preserve-root -rf /'",
            "unsafe-recursive-delete-expansion",
        ),
        (
            "toybox env sh -c 'rm --no-preserve-root -rf /'",
            "irreversible-root-delete",
        ),
        (
            "toybox time sh -c 'rm --no-preserve-root -rf /'",
            "irreversible-root-delete",
        ),
    ] {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(response["decision"], "deny", "{provider:?}: {command}");
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "deny",
                    "{provider:?}: {command}"
                );
            }
            assert_eq!(fake_model.request_count(), 0, "{provider:?}: {command}");
            assert_safety_deny(home.path(), expected_rule_id);
        }
    }
}

#[test]
fn append_assignment_bypasses_are_denied_before_model_inference_for_every_provider() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        for case in ["recursive flag", "trusted home"] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let command = match case {
                "recursive flag" => "X=-; X+=rf; rm --no-preserve-root -f $X /".to_string(),
                "trusted home" => {
                    let home_text = home.path().to_str().unwrap();
                    let split = home_text
                        .char_indices()
                        .next_back()
                        .expect("temporary HOME must not be empty")
                        .0;
                    format!(
                        "X='{}'; X+='{}'; rm -rf \"$X\"",
                        &home_text[..split],
                        &home_text[split..]
                    )
                }
                _ => unreachable!(),
            };
            let expected_rule_id = match case {
                "recursive flag" => "unsafe-recursive-delete-expansion",
                "trusted home" => "irreversible-home-delete",
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    response["decision"], "deny",
                    "{provider:?}: {case}: {command}"
                );
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "deny",
                    "{provider:?}: {case}: {command}"
                );
            }
            assert_eq!(
                fake_model.request_count(),
                0,
                "{provider:?}: {case}: {command}"
            );
            assert_safety_deny(home.path(), expected_rule_id);
        }
    }
}

#[test]
fn home_alias_field_splitting_is_denied_before_model_inference_for_every_provider() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        for case in [
            "root cwd exact HOME",
            "root cwd descendant",
            "root cwd append descendant",
            "root cwd shared-parent sibling",
            "root cwd ancestor pattern",
            "root cwd single-field ancestor pattern",
            "root cwd globstar HOME pattern",
            "root cwd globstar HOME-ancestor pattern",
            "root cwd nocase HOME pattern",
            "globstar parent traversal",
            "root globstar parent traversal",
            "multiple globstar parent traversal",
            "conservative direct root pattern",
            "aggregate pathname budget exhaustion",
            "HOME-parent cwd normalized relative pattern",
            "HOME-parent cwd descendant",
            "shell changes to HOME parent",
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let home_text = home.path().to_str().unwrap();
            let home_parent = home
                .path()
                .parent()
                .expect("temporary HOME must have a parent");
            let descendant = home.path().join("safe");
            let sibling = home_parent.join("xa99-sibling");
            let home_name = home
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .expect("temporary HOME must end with a UTF-8 component");
            let home_root_component = home
                .path()
                .components()
                .find_map(|component| match component {
                    std::path::Component::Normal(part) => part.to_str(),
                    _ => None,
                })
                .expect("temporary HOME must start with a UTF-8 component");
            let root_pattern = format!(
                "{}*",
                home_root_component
                    .chars()
                    .next()
                    .expect("temporary HOME component must not be empty")
            );
            let mut globstar_home_parts = home_text
                .trim_start_matches('/')
                .split('/')
                .map(str::to_string)
                .collect::<Vec<_>>();
            globstar_home_parts
                .last_mut()
                .expect("temporary HOME must contain a component")
                .push('*');
            let globstar_home_pattern = format!("/**/**/{}", globstar_home_parts.join("/"));
            let globstar_ancestor_pattern = format!("/**/**/{home_root_component}");
            let nocase_home_pattern = format!("{}*", home_text.to_uppercase());
            let (command, command_cwd, hook_cwd) = match case {
                "root cwd exact HOME" => (
                    format!("IFS=/; X='{home_text}'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd descendant" => (
                    format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd append descendant" => (
                    format!("IFS=/; X='{home_text}'; X+=/safe; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd shared-parent sibling" => (
                    format!("IFS=/; X='{}'; rm -rf $X", sibling.display()),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd ancestor pattern" => (
                    format!("IFS=/; X='/{root_pattern}/safe'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd single-field ancestor pattern" => (
                    format!("IFS=/; X='{root_pattern}'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd globstar HOME pattern" => (
                    format!("shopt -s globstar; IFS=:; X='{globstar_home_pattern}'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd globstar HOME-ancestor pattern" => (
                    format!("shopt -s globstar; IFS=:; X='{globstar_ancestor_pattern}'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root cwd nocase HOME pattern" => (
                    format!("shopt -s nocaseglob; IFS=:; X='{nocase_home_pattern}'; rm -rf $X"),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "globstar parent traversal" => (
                    format!(
                        "shopt -s globstar; IFS=:; X='{home_text}/**/../{home_name}'; rm -rf $X"
                    ),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "root globstar parent traversal" => (
                    "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/..'; rm -rf $X".to_string(),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "multiple globstar parent traversal" => (
                    "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/**/..'; rm -rf $X".to_string(),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "conservative direct root pattern" => (
                    "shopt -s globstar; IFS=:; X='/**/xa99-unrelated/**'; rm -rf $X".to_string(),
                    Path::new("/"),
                    Path::new("/"),
                ),
                "aggregate pathname budget exhaustion" => {
                    let fields = std::iter::repeat_n("/xa99z*/**/**/**/zz", 500)
                        .collect::<Vec<_>>()
                        .join(":");
                    (
                        format!("IFS=:; X='{fields}'; rm -rf $X"),
                        Path::new("/"),
                        Path::new("/"),
                    )
                }
                "HOME-parent cwd normalized relative pattern" => (
                    format!("IFS=,; X='./{home_name}*'; rm -rf $X"),
                    home_parent,
                    home_parent,
                ),
                "HOME-parent cwd descendant" => (
                    format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
                    home_parent,
                    home_parent,
                ),
                "shell changes to HOME parent" => (
                    format!(
                        "cd '{}'; IFS=/; X='{}'; rm -rf $X",
                        home_parent.display(),
                        descendant.display()
                    ),
                    home.path(),
                    home.path(),
                ),
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(command_cwd, provider, &command, None);

            let output = run_provider_permission_hook_from(
                home.path(),
                hook_cwd,
                provider_name,
                event,
                &payload,
            );

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let decision = if provider == AgentProvider::Antigravity {
                response["decision"].as_str()
            } else {
                response["hookSpecificOutput"]["decision"]["behavior"].as_str()
            };
            assert_eq!(
                (decision, fake_model.request_count()),
                (Some("deny"), 0),
                "{provider:?}: {case}: {command}"
            );
            let expected_rule_id = if matches!(
                case,
                "root cwd globstar HOME pattern" | "root cwd nocase HOME pattern"
            ) {
                "unsafe-recursive-delete-expansion"
            } else if matches!(
                case,
                "root globstar parent traversal" | "multiple globstar parent traversal"
            ) {
                "irreversible-root-delete"
            } else if matches!(
                case,
                "conservative direct root pattern" | "aggregate pathname budget exhaustion"
            ) {
                "unsafe-recursive-delete-expansion"
            } else {
                "irreversible-home-delete"
            };
            assert_safety_deny(home.path(), expected_rule_id);
        }

        for case in [
            "quoted descendant",
            "unrelated split path",
            "unrelated split pattern",
            "unrelated single-field split pattern",
            "shared-prefix split pattern suffix mismatch",
            "quoted globstar parent traversal",
            "unrelated globstar parent traversal",
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let command = match case {
                "quoted descendant" => {
                    format!("X='{}/safe'; rm -rf \"$X\"", home.path().display())
                }
                "unrelated split path" => {
                    "IFS=/; X=/xa99-safe-control/target; rm -rf $X".to_string()
                }
                "unrelated split pattern" | "unrelated single-field split pattern" => {
                    let longest_component = home
                        .path()
                        .components()
                        .filter_map(|component| match component {
                            std::path::Component::Normal(part) => part.to_str().map(str::len),
                            _ => None,
                        })
                        .max()
                        .expect("temporary HOME must contain a UTF-8 component");
                    let unrelated_pattern = "x".repeat(longest_component + 1);
                    if case == "unrelated split pattern" {
                        format!("IFS=/; X='/{unrelated_pattern}*/safe'; rm -rf $X")
                    } else {
                        format!("IFS=/; X='{unrelated_pattern}*'; rm -rf $X")
                    }
                }
                "shared-prefix split pattern suffix mismatch" => {
                    let home_root_component = home
                        .path()
                        .components()
                        .find_map(|component| match component {
                            std::path::Component::Normal(part) => part.to_str(),
                            _ => None,
                        })
                        .expect("temporary HOME must start with a UTF-8 component");
                    format!("IFS=:; X='{home_root_component}*/safe'; rm -rf $X")
                }
                "quoted globstar parent traversal" => {
                    let home_text = home.path().to_str().unwrap();
                    let home_name = home
                        .path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        .expect("temporary HOME must end with a UTF-8 component");
                    format!("X='{home_text}/**/../{home_name}'; rm -rf \"$X\"")
                }
                "unrelated globstar parent traversal" => {
                    "shopt -s globstar; IFS=:; X='/xa99-unrelated/**/../safe'; rm -rf $X"
                        .to_string()
                }
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);
            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let decision = if provider == AgentProvider::Antigravity {
                response["decision"].as_str()
            } else {
                response["hookSpecificOutput"]["decision"]["behavior"].as_str()
            };
            assert_eq!(
                (decision, fake_model.request_count()),
                (Some("allow"), 1),
                "{provider:?}: {case}: {command}"
            );
        }
    }
}

#[test]
fn literal_home_delete_denies_before_model_inference_for_every_provider() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        for case in ["exact HOME", "literal ancestor", "resolved ancestor"] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let home_parent = home
                .path()
                .parent()
                .expect("temporary HOME must have a parent");
            assert_ne!(home_parent, Path::new("/"));
            let command = match case {
                "exact HOME" => format!("rm -rf \"{}\"", home.path().display()),
                "literal ancestor" => format!("rm -rf \"{}\"", home_parent.display()),
                "resolved ancestor" => {
                    format!("X='{}'; rm -rf \"$X\"", home_parent.display())
                }
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    response["decision"], "deny",
                    "{provider:?}: {case}: {command}"
                );
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "deny",
                    "{provider:?}: {case}: {command}"
                );
            }
            assert_eq!(
                fake_model.request_count(),
                0,
                "{provider:?}: {case}: {command}"
            );
            assert_safety_deny(home.path(), "irreversible-home-delete");
        }
    }
}

#[test]
fn review_state_and_parameter_glob_cases_use_the_real_provider_boundary() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        for case in [
            "expanded HOME",
            "tilde HOME",
            "unquoted HOME glob",
            "adjacent unquoted HOME glob",
            "printf percent-n mutation",
            "assignment arithmetic mutation",
            "parameter arithmetic mutation",
            "lastpipe parent mutation",
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let command = match case {
                "expanded HOME" => "HOME=/tmp; export HOME=-rf; rm -f \"$HOME\" /".into(),
                "tilde HOME" => "HOME=/tmp; export HOME=-rf; rm -f ~ /".into(),
                "unquoted HOME glob" => {
                    let home = home.path().to_str().unwrap();
                    let last = home
                        .char_indices()
                        .next_back()
                        .expect("temporary HOME must not be empty")
                        .0;
                    format!("X='{}?'; rm -rf $X", &home[..last])
                }
                "adjacent unquoted HOME glob" => {
                    let home = home.path().to_str().unwrap();
                    let last = home
                        .char_indices()
                        .next_back()
                        .expect("temporary HOME must not be empty")
                        .0;
                    format!("X='{}['; rm -rf ${{X}}{}]", &home[..last], &home[last..])
                }
                "printf percent-n mutation" => "X=; printf %n X; rm -f \"${X:+-rf}\" /".into(),
                "assignment arithmetic mutation" => "X=; Y=$((X=1)); rm -f \"${X:+-rf}\" /".into(),
                "parameter arithmetic mutation" => {
                    "X=; VALUE=abc; Y=\"${OUTER:-${VALUE:X=1}}\"; rm -f \"${X:+-rf}\" /".into()
                }
                "lastpipe parent mutation" => {
                    "shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /".into()
                }
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(response["decision"], "deny", "{provider:?}: {case}");
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "deny",
                    "{provider:?}: {case}"
                );
            }
            assert_eq!(fake_model.request_count(), 0, "{provider:?}: {case}");
            assert_safety_deny(home.path(), "unsafe-recursive-delete-expansion");
        }

        for quoted in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let home_text = home.path().to_str().unwrap();
            let last = home_text
                .char_indices()
                .next_back()
                .expect("temporary HOME must not be empty")
                .0;
            let pattern = format!("{}?", &home_text[..last]);
            let command = if quoted {
                format!("X='{pattern}'; rm -rf \"$X\"")
            } else {
                format!("X='{pattern}'; rm -rf $X-suffix")
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {command}");
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(response["decision"], "allow", "{provider:?}: {command}");
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "allow",
                    "{provider:?}: {command}"
                );
            }
            assert_eq!(fake_model.request_count(), 1, "{provider:?}: {command}");
        }

        for control in [
            "quoted closing fragment",
            "escaped closing bracket",
            "nonmatching suffix",
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let home_text = home.path().to_str().unwrap();
            let last = home_text
                .char_indices()
                .next_back()
                .expect("temporary HOME must not be empty")
                .0;
            let final_character = &home_text[last..];
            let suffix = match control {
                "quoted closing fragment" => format!("\"{final_character}]\""),
                "escaped closing bracket" => format!("{final_character}\\]"),
                "nonmatching suffix" => format!("{final_character}]-suffix"),
                _ => unreachable!(),
            };
            let command = format!("X='{}['; rm -rf ${{X}}{suffix}", &home_text[..last]);
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(
                output.status.success(),
                "{provider:?}: {control}: {command}"
            );
            let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    response["decision"], "allow",
                    "{provider:?}: {control}: {command}"
                );
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"], "allow",
                    "{provider:?}: {control}: {command}"
                );
            }
            assert_eq!(
                fake_model.request_count(),
                1,
                "{provider:?}: {control}: {command}"
            );
        }
    }
}

#[test]
fn real_shell_safety_indeterminate_corpus_preserves_native_confirmation_without_model_inference() {
    let mut nested = "literal".to_string();
    for _ in 0..80 {
        nested = format!("${{VALUE:-{nested}}}");
    }
    let cases = [
        ("malformed Bash", "if true; then".to_string()),
        (
            "continued command substitution",
            "$\\\n(printf rm) -rf /".to_string(),
        ),
        (
            "continued arithmetic expansion",
            "$\\\n((1+1)) -rf /".to_string(),
        ),
        (
            "quoted parameter depth limit",
            format!("printf '%s' \"{nested}\""),
        ),
        (
            "abbreviated GNU time option",
            "/usr/bin/time --out log rm -rf /".to_string(),
        ),
        (
            "ANSI-C env split-string option",
            "env $'-\\x53' 'rm -rf /'".to_string(),
        ),
    ];

    for (case, command) in cases {
        for (provider, session_id) in [
            (AgentProvider::Codex, "session-1"),
            (AgentProvider::Claude, "claude-session-1"),
            (AgentProvider::Antigravity, "agy-conversation-1"),
        ] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            if provider == AgentProvider::Antigravity {
                seed_antigravity_invocation(home.path(), 5);
            }
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}");
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
                    serde_json::json!({
                        "decision": "ask",
                        "reason": "Coding Brain abstained"
                    }),
                    "{case}"
                );
            } else {
                assert!(output.stdout.is_empty(), "{provider:?}: {case}");
            }
            assert_eq!(fake_model.request_count(), 0, "{provider:?}: {case}");
            assert_needs_input(home.path(), provider, session_id);
        }
    }
}

#[test]
fn deeply_nested_shell_input_is_contained_by_the_isolated_helper() {
    let home = tempfile::tempdir().unwrap();
    let fake_model = install_model_fixture(home.path(), "approve");
    let depth = 8_192;
    let command = format!(
        "printf '%s' \"$(({}1{}))\"",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    let (provider, event, payload) =
        shell_permission_payload(home.path(), AgentProvider::Codex, &command, None);

    let output = run_provider_permission_hook(home.path(), provider, event, &payload);

    assert!(output.status.success());
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(
        events
            .iter()
            .all(|event| event.state != ActivityState::Denied),
        "deep nesting must not become a deterministic safety deny"
    );
    if output.stdout.is_empty() {
        assert_eq!(fake_model.request_count(), 0);
        assert_needs_input(home.path(), AgentProvider::Codex, "session-1");
    } else {
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            response["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert_eq!(fake_model.request_count(), 1);
    }
}

#[test]
fn provider_policy_deny_precedes_real_shell_safety_parser_indeterminate() {
    let home = tempfile::tempdir().unwrap();
    let fake_model = install_model_fixture(home.path(), "approve");
    let (provider_name, event, payload) = shell_permission_payload(
        home.path(),
        AgentProvider::Claude,
        "if true; then",
        Some("deny"),
    );

    let output = run_provider_permission_hook(home.path(), provider_name, event, &payload);

    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert_eq!(fake_model.request_count(), 0);
    let terminal = activity(home.path())
        .read()
        .unwrap()
        .events()
        .iter()
        .find(|event| event.state == ActivityState::Denied)
        .cloned()
        .expect("missing provider-policy deny activity");
    assert!(terminal.rule_id.is_none());
}

#[test]
fn destructive_commands_are_denied_across_permission_providers() {
    let codex_home = tempfile::tempdir().unwrap();
    install_model_fixture(codex_home.path(), "approve");
    let codex = run_provider_permission_hook(
        codex_home.path(),
        "codex",
        None,
        &permission_payload(codex_home.path(), "rm -rf /"),
    );
    let response: serde_json::Value = serde_json::from_slice(&codex.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert_safety_deny(codex_home.path(), "irreversible-root-delete");
    assert!(!codex_home.path().join("bin/curl.args").exists());

    let claude_home = tempfile::tempdir().unwrap();
    install_model_fixture(claude_home.path(), "approve");
    let mut claude_payload: serde_json::Value =
        serde_json::from_slice(&claude_permission_payload(claude_home.path(), None)).unwrap();
    claude_payload["tool_input"]["command"] = serde_json::json!("rm -rf /");
    let claude = run_provider_permission_hook(
        claude_home.path(),
        "claude",
        None,
        &serde_json::to_vec(&claude_payload).unwrap(),
    );
    let response: serde_json::Value = serde_json::from_slice(&claude.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert_safety_deny(claude_home.path(), "irreversible-root-delete");
    assert!(!claude_home.path().join("bin/curl.args").exists());

    let antigravity_home = tempfile::tempdir().unwrap();
    install_model_fixture(antigravity_home.path(), "approve");
    seed_antigravity_invocation(antigravity_home.path(), 5);
    let mut antigravity_payload: serde_json::Value = serde_json::from_slice(
        &antigravity_permission_payload(antigravity_home.path(), None),
    )
    .unwrap();
    antigravity_payload["toolCall"]["args"]["CommandLine"] = serde_json::json!("rm -rf /");
    let antigravity = run_provider_permission_hook(
        antigravity_home.path(),
        "antigravity",
        Some("PreToolUse"),
        &serde_json::to_vec(&antigravity_payload).unwrap(),
    );
    let response: serde_json::Value = serde_json::from_slice(&antigravity.stdout).unwrap();
    assert_eq!(response["decision"], "deny");
    assert_safety_deny(antigravity_home.path(), "irreversible-root-delete");
    assert!(!antigravity_home.path().join("bin/curl.args").exists());
}

#[test]
fn antigravity_dynamic_rm_arguments_deny_before_inference() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    seed_antigravity_invocation(home.path(), 5);
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(home.path(), None)).unwrap();
    payload["toolCall"]["args"]["CommandLine"] = serde_json::json!("rm $(printf '%s\\n' -rf /)");

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &serde_json::to_vec(&payload).unwrap(),
    );

    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["decision"], "deny");
    assert_safety_deny(home.path(), "unsafe-recursive-delete-expansion");
    assert!(!home.path().join("bin/curl.args").exists());
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
fn interleaved_codex_children_receive_isolated_permission_decisions() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-b", "turn-b"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-b", "turn-b", "tool-b"),
    );

    let child_a = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-a", "turn-a"),
    );
    let child_b = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-b", "turn-b"),
    );

    assert!(
        !child_a.stdout.is_empty(),
        "child-a stderr: {}",
        String::from_utf8_lossy(&child_a.stderr)
    );
    assert!(
        !child_b.stdout.is_empty(),
        "child-b stderr: {}",
        String::from_utf8_lossy(&child_b.stderr)
    );
    assert!(!String::from_utf8_lossy(&child_a.stderr).contains("AmbiguousTurn"));
    assert!(!String::from_utf8_lossy(&child_b.stderr).contains("AmbiguousTurn"));

    let mut mismatched_provider: serde_json::Value = serde_json::from_slice(&child_post_payload(
        home.path(),
        "child-b",
        "turn-b",
        "tool-b",
    ))
    .unwrap();
    mismatched_provider["session_id"] = serde_json::json!("other-root");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &serde_json::to_vec(&mismatched_provider).unwrap(),
    );
    assert!(
        !activity(home.path())
            .read()
            .unwrap()
            .events()
            .iter()
            .any(|event| event.state == ActivityState::Outcome)
    );

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_post_payload(home.path(), "child-b", "turn-b", "tool-b"),
    );

    let log = activity(home.path()).read().unwrap();
    assert_eq!(
        log.events()
            .iter()
            .filter(|event| {
                event.state == ActivityState::Outcome
                    && event.session.as_ref().is_some_and(|session| {
                        session.session_id == "child-b"
                            && session.provider_session_id.as_deref() == Some("root-1")
                    })
            })
            .count(),
        1
    );
    assert!(!log.events().iter().any(|event| {
        event.state == ActivityState::Outcome
            && event
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == "child-a")
    }));

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-b", "turn-b"),
    );
    let replay = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-b", "turn-b"),
    );
    assert!(replay.stdout.is_empty());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("duplicate"));

    let log = activity(home.path()).read().unwrap();
    for (event_name, child_id) in [("SubagentStart", "child-a"), ("SubagentStop", "child-b")] {
        assert!(log.events().iter().any(|event| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some(event_name)
                && event.session.as_ref().is_some_and(|session| {
                    session.session_id == child_id
                        && session.provider_session_id.as_deref() == Some("root-1")
                })
        }));
    }
}

#[test]
fn resumed_codex_child_permission_is_reproved_and_delivered() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-b",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(home.path(), "child-a", "turn-b", &transcript),
    );

    assert!(permission.status.success());
    assert!(
        !permission.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&permission.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&permission.stdout).unwrap();
    assert_eq!(
        response,
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow"}
            }
        })
    );
    assert_delivered_child_decision(home.path(), "child-a", "turn-b");
}

#[test]
fn interrupted_codex_child_permission_refreshes_active_turn_and_is_delivered() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-b",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(home.path(), "child-a", "turn-b", &transcript),
    );

    assert!(permission.status.success());
    assert!(
        !permission.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&permission.stderr)
    );
    assert!(!String::from_utf8_lossy(&permission.stderr).contains("AmbiguousTurn"));
    assert!(!String::from_utf8_lossy(&permission.stderr).contains("SubagentTurnMismatch"));
    assert_delivered_child_decision(home.path(), "child-a", "turn-b");
}

#[test]
fn stopped_codex_child_skips_permissionless_followup_and_delivers_next_turn() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let turn_b = one_second_from_now_rfc3339();
    let transcript = home.path().join("rollout-child-a.jsonl");
    fs::write(
        &transcript,
        format!(
            "{}\
             {{\"timestamp\":\"{turn_b}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-b\"}}}}\n\
             {{\"timestamp\":\"{turn_b}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"turn-b\"}}}}\n",
            child_resume_metadata("child-a", "root-1", "root-1"),
        ),
    )
    .unwrap();
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-b"),
    );
    let snapshot = lifecycle_snapshot(home.path());
    let root_key =
        coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "root-1")
            .storage_key();
    let stopped = &snapshot.sessions[&root_key].stopped_subagents["child-a"];
    assert_eq!(stopped.turn_id, "turn-b");
    let turn_c = OffsetDateTime::from_unix_timestamp_nanos(
        i128::from(stopped.received_at_ms + 1_000) * 1_000_000,
    )
    .unwrap()
    .format(&Rfc3339)
    .unwrap();
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap(),
        "{{\"timestamp\":\"{turn_c}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-c\"}}}}"
    )
    .unwrap();

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(home.path(), "child-a", "turn-c", &transcript),
    );

    assert!(permission.status.success());
    assert!(
        !permission.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&permission.stderr)
    );
    assert_delivered_child_decision(home.path(), "child-a", "turn-c");
}

#[test]
fn invalid_active_codex_followup_evidence_emits_no_allow() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-other",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(home.path(), "child-a", "turn-b", &transcript),
    );

    let stderr = String::from_utf8_lossy(&permission.stderr);
    assert!(permission.status.success());
    assert!(permission.stdout.is_empty());
    assert!(stderr.contains("UnprovenSubagent"), "{stderr}");
    assert!(stderr.contains("Codex resume evidence:"), "{stderr}");
}

fn assert_delivered_child_decision(home: &Path, child_id: &str, turn_id: &str) {
    let events = activity(home).read().unwrap().events().to_vec();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == ActivityKind::Decision
                    && event.session.as_ref().is_some_and(|session| {
                        session.session_id == child_id
                            && session.provider_session_id.as_deref() == Some("root-1")
                            && session.turn_id.as_deref() == Some(turn_id)
                    })
            })
            .map(|event| event.state)
            .collect::<Vec<_>>(),
        [
            ActivityState::Observed,
            ActivityState::Evaluating,
            ActivityState::Allowed,
            ActivityState::Delivered,
        ]
    );
}

#[derive(Clone, Copy, Debug)]
enum ResumeCase {
    NoTaskStart,
    StoppedTurn,
    WrongChild,
    WrongProviderSession,
    WrongTurn,
    StaleTimestamp,
    FutureTimestamp,
}

fn run_resume_case(case: ResumeCase) -> Output {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );

    let transcript = home.path().join(format!("rollout-{case:?}.jsonl"));
    let child_id = if matches!(case, ResumeCase::WrongChild) {
        "child-other"
    } else {
        "child-a"
    };
    let provider_session_id = if matches!(case, ResumeCase::WrongProviderSession) {
        "root-other"
    } else {
        "root-1"
    };
    let permission_turn = if matches!(case, ResumeCase::StoppedTurn) {
        "turn-a"
    } else {
        "turn-b"
    };
    let transcript_turn = if matches!(case, ResumeCase::WrongTurn) {
        "turn-other"
    } else {
        permission_turn
    };
    let timestamp = match case {
        ResumeCase::StaleTimestamp => "1970-01-01T00:00:00Z".to_owned(),
        ResumeCase::FutureTimestamp => (OffsetDateTime::now_utc() + time::Duration::seconds(60))
            .format(&Rfc3339)
            .unwrap(),
        _ => one_second_from_now_rfc3339(),
    };
    if matches!(case, ResumeCase::NoTaskStart) {
        fs::write(
            &transcript,
            child_resume_metadata(child_id, provider_session_id, "root-1"),
        )
        .unwrap();
    } else {
        write_child_resume_transcript(
            &transcript,
            child_id,
            provider_session_id,
            "root-1",
            transcript_turn,
            &timestamp,
        );
    }

    run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            permission_turn,
            &transcript,
        ),
    )
}

#[test]
fn invalid_codex_resume_evidence_remains_fail_closed() {
    for case in [
        ResumeCase::NoTaskStart,
        ResumeCase::StoppedTurn,
        ResumeCase::WrongChild,
        ResumeCase::WrongProviderSession,
        ResumeCase::WrongTurn,
        ResumeCase::StaleTimestamp,
        ResumeCase::FutureTimestamp,
    ] {
        let output = run_resume_case(case);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{case:?}");
        assert!(output.stdout.is_empty(), "{case:?}");
        assert!(stderr.contains("UnprovenSubagent"), "{case:?}: {stderr}");
        assert!(
            stderr.contains("Codex resume evidence:"),
            "{case:?}: {stderr}"
        );
        assert!(!stderr.contains("rollout-"), "{case:?}: {stderr}");
        assert!(!stderr.contains("task_started"), "{case:?}: {stderr}");
    }
}

#[test]
fn parent_interacted_event_is_not_codex_resume_authority() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let parent_transcript = home.path().join("rollout-root-1.jsonl");
    fs::write(
        &parent_transcript,
        format!(
            "{}{{\"timestamp\":\"{}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"sub_agent_activity\",\"kind\":\"interacted\",\"agent_id\":\"child-a\"}}}}\n",
            child_resume_metadata("root-1", "root-1", "root-1"),
            one_second_from_now_rfc3339(),
        ),
    )
    .unwrap();
    let child_transcript = home.path().join("rollout-child-a.jsonl");
    fs::write(
        &child_transcript,
        child_resume_metadata("child-a", "root-1", "root-1"),
    )
    .unwrap();

    let output = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            "turn-b",
            &child_transcript,
        ),
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UnprovenSubagent"));
}

#[test]
fn depth_two_codex_resume_uses_shared_root_provider_identity() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-leaf", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-leaf", "turn-a"),
    );
    let transcript = home.path().join("rollout-child-leaf.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-leaf",
        "root-1",
        "child-parent",
        "turn-b",
        &one_second_from_now_rfc3339(),
    );

    let output = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(home.path(), "child-leaf", "turn-b", &transcript),
    );

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["hookSpecificOutput"]
            ["decision"]["behavior"],
        "allow"
    );
}

#[test]
fn stopped_codex_child_post_tool_use_never_records_an_outcome() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-a", "turn-a"),
    );
    assert!(!permission.stdout.is_empty());

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let post = run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_post_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );

    assert!(post.status.success());
    let log = activity(home.path()).read().unwrap();
    assert!(
        !log.events()
            .iter()
            .any(|event| event.state == ActivityState::Outcome)
    );
    assert!(!log.events().iter().any(|event| {
        event.kind == ActivityKind::Lifecycle
            && event.tool.as_deref() == Some("PostToolUse")
            && event
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == "child-a")
    }));
}

#[test]
fn ignored_codex_child_pre_tool_use_never_becomes_a_fallback_anchor() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");

    let ignored_pre = run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    assert!(ignored_pre.status.success());
    assert!(ignored_pre.stderr.is_empty());

    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-a", "turn-a"),
    );
    assert!(!permission.stdout.is_empty());
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_post_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );

    let log = activity(home.path()).read().unwrap();
    assert!(!log.events().iter().any(|event| {
        event.kind == ActivityKind::Lifecycle
            && event.tool.as_deref() == Some("PreToolUse")
            && event
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == "child-a")
    }));
    assert!(
        !log.events()
            .iter()
            .any(|event| event.state == ActivityState::Outcome)
    );
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
        seed_antigravity_invocation(home.path(), 5);

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
        for ignored_reason in [IgnoreReason::RecentTurn, IgnoreReason::AmbiguousTurn] {
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
            if !String::from_utf8_lossy(&output.stderr)
                .contains("permission transaction lifecycle admission ignored")
            {
                failures.push(format!(
                    "{provider_name} {ignored_reason}: missing transaction conflict diagnostic: {}",
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
        for ignored_reason in [IgnoreReason::RecentTurn, IgnoreReason::AmbiguousTurn] {
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
                    .all(|event| event.state != ActivityState::Allowed),
                "{provider_name} {ignored_reason:?}: fail-safe response projected as allow"
            );
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
fn antigravity_open_invocation_allows_in_range_step() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    seed_antigravity_invocation(home.path(), 5);

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(home.path(), None),
    );

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"decision": "allow"})
    );
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(
        events
            .iter()
            .any(|event| event.state == ActivityState::Allowed)
    );
    assert!(
        events
            .iter()
            .any(|event| event.state == ActivityState::Delivered)
    );
}

#[test]
fn antigravity_post_invocation_preserves_bounded_permission_authority_until_stop() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let invocation = antigravity_invocation_payload(home.path(), 14, 70);

    let pre = run_provider_lifecycle_hook(
        home.path(),
        "antigravity",
        Some("PreInvocation"),
        &invocation,
    );
    assert!(pre.status.success());
    assert!(pre.stderr.is_empty());

    for step in [70, 72] {
        if step == 72 {
            let before_snapshot = lifecycle_snapshot(home.path());
            let before_activity = activity(home.path()).read().unwrap().events().to_vec();
            let post = run_provider_lifecycle_hook(
                home.path(),
                "antigravity",
                Some("PostInvocation"),
                &invocation,
            );
            assert!(post.status.success());
            assert!(post.stderr.is_empty());
            assert_eq!(lifecycle_snapshot(home.path()), before_snapshot);
            assert_eq!(
                activity(home.path()).read().unwrap().events(),
                before_activity
            );
        }

        let before_permission = activity(home.path()).read().unwrap().events().to_vec();
        let output = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &antigravity_permission_payload_for_step(home.path(), step),
        );
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({"decision": "allow"})
        );
        let all_events = activity(home.path()).read().unwrap().events().to_vec();
        let new_events = &all_events[before_permission.len()..];
        let expected_tool_use_id = format!("step-{step}");
        let allowed = new_events
            .iter()
            .find(|event| {
                event.state == ActivityState::Allowed
                    && event.tool.as_deref() == Some("run_command")
                    && event
                        .session
                        .as_ref()
                        .and_then(|session| session.tool_use_id.as_deref())
                        == Some(expected_tool_use_id.as_str())
            })
            .unwrap();
        assert!(new_events.iter().any(|event| {
            event.state == ActivityState::Delivered && event.activity_id == allowed.activity_id
        }));
    }

    let before_stop_rejection = activity(home.path()).read().unwrap().events().to_vec();
    let stop = run_provider_lifecycle_hook(
        home.path(),
        "antigravity",
        Some("Stop"),
        &antigravity_stop_payload(home.path(), 3),
    );
    assert!(stop.status.success());
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("exact recovery identity link unavailable")
    );

    let after_stop = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload_for_step(home.path(), 74),
    );
    assert!(after_stop.status.success());
    assert!(String::from_utf8_lossy(&after_stop.stderr).contains("permission transaction"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&after_stop.stdout).unwrap(),
        serde_json::json!({
            "decision": "ask",
            "reason": "Coding Brain abstained"
        })
    );

    let all_events = activity(home.path()).read().unwrap().events().to_vec();
    let new_events = &all_events[before_stop_rejection.len()..];
    assert_eq!(
        new_events
            .iter()
            .filter(|event| event.state == ActivityState::Error)
            .count(),
        1
    );
    assert!(new_events.iter().all(|event| {
        !matches!(
            event.state,
            ActivityState::Allowed | ActivityState::Delivered
        )
    }));
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
        if provider == "antigravity" {
            seed_antigravity_invocation(home.path(), 5);
        }

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
    assert!(String::from_utf8_lossy(&output.stderr).contains("permission transaction"));
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
    let before_second = activity(home.path()).read().unwrap().events().to_vec();
    let second = run_provider_permission_hook(home.path(), "claude", None, &payload);

    assert!(first.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first.stdout).unwrap()["hookSpecificOutput"]["decision"]
            ["behavior"],
        "allow"
    );
    assert!(second.status.success());
    assert!(second.stdout.is_empty());
    assert!(String::from_utf8_lossy(&second.stderr).contains("duplicate"));
    assert_eq!(
        activity(home.path()).read().unwrap().events(),
        before_second
    );
}

#[test]
fn repeated_antigravity_synthesized_turn_id_asks_after_model_allow() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    seed_antigravity_invocation(home.path(), 5);
    let payload = antigravity_permission_payload(home.path(), None);

    let first =
        run_provider_permission_hook(home.path(), "antigravity", Some("PreToolUse"), &payload);
    let before_second = activity(home.path()).read().unwrap().events().to_vec();
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
    assert!(String::from_utf8_lossy(&second.stderr).contains("duplicate"));
    assert_eq!(
        activity(home.path()).read().unwrap().events(),
        before_second
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
        if provider == "antigravity" {
            seed_antigravity_invocation(home.path(), 5);
        }
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
fn unsupported_antigravity_post_tool_use_is_observation_only() {
    for (step, tool) in [(5, "view_file"), (6, "grep_search")] {
        let home = tempfile::tempdir().unwrap();
        seed_antigravity_invocation(home.path(), step);
        let permission = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &unsupported_antigravity_permission_payload(home.path(), step, tool),
        );
        assert!(permission.status.success(), "{tool}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&permission.stdout).unwrap()["decision"],
            "ask"
        );
        assert!(
            permission.stderr.is_empty(),
            "{tool}: {}",
            String::from_utf8_lossy(&permission.stderr)
        );

        let post = run_provider_lifecycle_hook(
            home.path(),
            "antigravity",
            Some("PostToolUse"),
            &antigravity_post_payload(home.path(), step),
        );
        assert!(post.status.success(), "{tool}");
        assert!(post.stdout.is_empty(), "{tool}");
        assert!(
            post.stderr.is_empty(),
            "{tool}: {}",
            String::from_utf8_lossy(&post.stderr)
        );

        let events = activity(home.path()).read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == ActivityKind::Lifecycle
                    && event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1,
            "{tool}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            0,
            "{tool}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == ActivityKind::Diagnostic)
                .count(),
            0,
            "{tool}"
        );
    }
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
    let lifecycle = lifecycle_snapshot(home.path());
    let session_key =
        coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "session-1")
            .storage_key();
    assert_eq!(
        lifecycle.sessions[&session_key].latest_event,
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
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert_eq!(
        events.iter().map(|event| event.state).collect::<Vec<_>>(),
        [
            ActivityState::Observed,
            ActivityState::Evaluating,
            ActivityState::Abstained,
        ]
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
#[ignore = "legacy JSONL decision-path fault injection was removed"]
fn model_proposal_failure_abstains_before_terminal_commit() {
    let home = tempfile::tempdir().unwrap();
    let fake_model = install_model_fixture(home.path(), "approve");
    fs::create_dir_all(
        home.path()
            .join(".local/state/coding-brain/brain/decisions.jsonl"),
    )
    .unwrap();

    let output = run_permission_hook(home.path(), &permission_payload(home.path(), "cargo test"));

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("decision evidence"));
    assert_eq!(fake_model.request_count(), 0);
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(events.is_empty());
}

#[test]
fn provider_inference_exit_preserves_native_authority_without_replay() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), "approve");
        overwrite_curl(home.path(), "exit 7");
        let (provider_name, event, payload) = provider_permission_case(home.path(), provider);

        let failed = run_provider_permission_hook(home.path(), provider_name, event, &payload);

        assert!(failed.status.success(), "{provider:?}");
        assert_eq!(
            failed.stdout,
            native_permission_fallback(provider),
            "{provider:?}"
        );
        let expected = expected_permission_identity(provider);
        let persisted_events = permission_activity_events(home.path());
        let correlated_events = persisted_events.iter().collect::<Vec<_>>();
        assert_correlated_permission_activity(
            &correlated_events,
            expected,
            &[
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Error,
            ],
            None,
        );
        assert!(permission_decisions(home.path()).is_empty(), "{provider:?}");

        let replay = run_provider_permission_hook(home.path(), provider_name, event, &payload);
        assert!(replay.status.success(), "{provider:?}");
        assert_eq!(
            replay.stdout,
            native_permission_fallback(provider),
            "{provider:?}"
        );
        assert!(
            String::from_utf8_lossy(&replay.stderr).contains("duplicate"),
            "{provider:?}: {}",
            String::from_utf8_lossy(&replay.stderr)
        );
        assert_eq!(
            permission_activity_events(home.path()),
            persisted_events,
            "{provider:?} restart activity"
        );
        assert!(permission_decisions(home.path()).is_empty(), "{provider:?}");
    }
}

#[test]
fn low_confidence_is_a_visible_abstention() {
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
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        let home = tempfile::tempdir().unwrap();
        install_model_fixture(home.path(), "approve");
        let (provider_name, event, payload) = provider_permission_case(home.path(), provider);
        let mut child = spawn_provider_permission_hook_process(home.path(), provider_name, event);
        drop(child.stdout.take());
        child.stdin.take().unwrap().write_all(&payload).unwrap();

        let output = child.wait_with_output().unwrap();

        assert!(output.status.success(), "{provider:?}");
        assert!(output.stdout.is_empty(), "{provider:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("write response"),
            "{provider:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = expected_permission_identity(provider);
        let persisted_decisions = permission_decisions(home.path());
        assert_eq!(persisted_decisions.len(), 1, "{provider:?}");
        let (identity, record) = &persisted_decisions[0];
        let decision_id = assert_permission_proposal(identity, record, expected);
        let persisted_events = permission_activity_events(home.path());
        let correlated_events = persisted_events.iter().collect::<Vec<_>>();
        assert_correlated_permission_activity(
            &correlated_events,
            expected,
            &[
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::DeliveryFailed,
            ],
            Some(&decision_id),
        );
        let store = activity(home.path());
        let snapshot = store.snapshot(SnapshotLimits::default()).unwrap();
        assert_eq!(snapshot.attention[0].delivery, DeliveryState::Failed);
        assert!(!snapshot.attention[0].tool_execution_confirmed);

        let replay = run_provider_permission_hook(home.path(), provider_name, event, &payload);
        assert!(replay.status.success(), "{provider:?}");
        assert_eq!(
            replay.stdout,
            native_permission_fallback(provider),
            "{provider:?}"
        );
        assert!(
            String::from_utf8_lossy(&replay.stderr).contains("duplicate"),
            "{provider:?}: {}",
            String::from_utf8_lossy(&replay.stderr)
        );
        assert_eq!(
            permission_activity_events(home.path()),
            persisted_events,
            "{provider:?} restart activity"
        );
        assert_eq!(
            permission_decisions(home.path()),
            persisted_decisions,
            "{provider:?} restart decision"
        );
    }
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
