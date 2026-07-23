use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use coding_brain_core::lifecycle::{LifecycleEventName, LifecycleStore, ProjectedStatus};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use sha2::{Digest, Sha256};

const PROMPT: &[u8] = include_bytes!("fixtures/hooks/user-prompt-submit.json");
const CLAUDE_STOP: &[u8] = include_bytes!("fixtures/hooks/claude-stop.json");
const ANTIGRAVITY_STOP: &[u8] = include_bytes!("fixtures/hooks/antigravity-stop.json");
const ANTIGRAVITY_PRE_TOOL_USE: &[u8] =
    include_bytes!("fixtures/hooks/antigravity-pre-tool-use.json");
const ANTIGRAVITY_POST_TOOL_USE: &[u8] =
    include_bytes!("fixtures/hooks/antigravity-post-tool-use.json");

fn run_hook(home: &std::path::Path, input: &[u8]) -> Output {
    run_provider_hook(home, None, input)
}

fn run_provider_hook(home: &std::path::Path, provider: Option<&str>, input: &[u8]) -> Output {
    run_provider_hook_with_event(home, provider, None, input)
}

fn run_provider_hook_with_event(
    home: &std::path::Path,
    provider: Option<&str>,
    antigravity_event: Option<&str>,
    input: &[u8],
) -> Output {
    let normalized_input = serde_json::from_slice::<serde_json::Value>(input)
        .map(|mut value| {
            value["cwd"] = serde_json::json!(home);
            if value.get("workspacePaths").is_some() {
                value["workspacePaths"] = serde_json::json!([home]);
            }
            serde_json::to_vec(&value).unwrap()
        })
        .unwrap_or_else(|_| input.to_vec());
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command.arg("--lifecycle-hook");
    if let Some(provider) = provider {
        command.args(["--provider", provider]);
    }
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
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

fn assert_antigravity_rejected(event: Option<&str>, payload: &serde_json::Value, label: &str) {
    let home = tempfile::tempdir().unwrap();
    let output = run_provider_hook_with_event(
        home.path(),
        Some("antigravity"),
        event,
        &serde_json::to_vec(payload).unwrap(),
    );
    assert!(output.status.success(), "{label}");
    assert!(output.stdout.is_empty(), "{label}");
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.starts_with("coding-brain lifecycle hook:"),
        "{label}: {diagnostic:?}"
    );
    assert!(diagnostic.len() < 256, "{label}");
    for path in [
        "hooks/lifecycle.json",
        "activity.jsonl",
        "session-links.jsonl",
    ] {
        assert!(
            !home
                .path()
                .join(".local/state/coding-brain")
                .join(path)
                .exists(),
            "{label}: unexpectedly persisted {path}"
        );
    }
}

#[test]
fn claude_lifecycle_hook_records_provider_qualified_stop() {
    let home = tempfile::tempdir().unwrap();
    let output = run_provider_hook(home.path(), Some("claude"), CLAUDE_STOP);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let snapshot = LifecycleStore::at(home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    let key = AgentSessionKey::native(AgentProvider::Claude, "claude-session-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::Stop)
    );

    let activity =
        fs::read_to_string(home.path().join(".local/state/coding-brain/activity.jsonl")).unwrap();
    let row: serde_json::Value = serde_json::from_str(activity.trim()).unwrap();
    assert_eq!(row["session"]["provider"], "claude");
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/session-links.jsonl")
            .exists(),
        "a non-provider test parent must not become live identity evidence"
    );
}

#[test]
fn antigravity_trusted_cli_events_record_provider_qualified_lifecycle() {
    let post_home = tempfile::tempdir().unwrap();
    let post = run_provider_hook_with_event(
        post_home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        ANTIGRAVITY_POST_TOOL_USE,
    );
    assert!(post.status.success());
    assert!(post.stdout.is_empty());
    assert!(post.stderr.is_empty());
    let snapshot = LifecycleStore::at(post_home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );
    let activity = fs::read_to_string(
        post_home
            .path()
            .join(".local/state/coding-brain/activity.jsonl"),
    )
    .unwrap();
    let row: serde_json::Value = serde_json::from_str(activity.trim()).unwrap();
    assert_eq!(row["kind"], "lifecycle");
    assert_eq!(row["tool"], "PostToolUse");
    assert_eq!(row["session"]["provider"], "antigravity");

    let adversarial_home = tempfile::tempdir().unwrap();
    let mut adversarial: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    adversarial["hookEventName"] = serde_json::json!("Stop");
    adversarial["toolUseId"] = serde_json::json!("payload-controlled-id");
    adversarial["toolName"] = serde_json::json!("payload-controlled-tool");
    adversarial["executionNum"] = serde_json::json!(99);
    adversarial["terminationReason"] = serde_json::json!("payload-stop");
    adversarial["fullyIdle"] = serde_json::json!(true);
    let adversarial = run_provider_hook_with_event(
        adversarial_home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        &serde_json::to_vec(&adversarial).unwrap(),
    );
    assert!(adversarial.status.success());
    assert!(adversarial.stderr.is_empty());
    let snapshot = LifecycleStore::at(adversarial_home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );
    assert_eq!(
        snapshot.sessions[&key].current_turn.as_deref(),
        Some("step-5")
    );

    let pre_home = tempfile::tempdir().unwrap();
    let mut pre_payload: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_PRE_TOOL_USE).unwrap();
    pre_payload["hookEventName"] = serde_json::json!("Stop");
    pre_payload["toolUseId"] = serde_json::json!("payload-controlled-id");
    pre_payload["toolName"] = serde_json::json!("payload-controlled-tool");
    let pre = run_provider_hook_with_event(
        pre_home.path(),
        Some("antigravity"),
        Some("PreToolUse"),
        &serde_json::to_vec(&pre_payload).unwrap(),
    );
    assert!(pre.status.success());
    assert!(pre.stdout.is_empty());
    assert!(pre.stderr.is_empty());
    let snapshot = LifecycleStore::at(pre_home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PreToolUse)
    );
    assert_eq!(
        snapshot.sessions[&key].current_turn.as_deref(),
        Some("step-5")
    );
    let activity = fs::read_to_string(
        pre_home
            .path()
            .join(".local/state/coding-brain/activity.jsonl"),
    )
    .unwrap();
    let row: serde_json::Value = serde_json::from_str(activity.trim()).unwrap();
    assert_eq!(row["session"]["tool_use_id"], "step-5");

    let stop_home = tempfile::tempdir().unwrap();
    let mut stop_payload: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_STOP).unwrap();
    stop_payload.as_object_mut().unwrap().remove("error");
    let stop = run_provider_hook_with_event(
        stop_home.path(),
        Some("antigravity"),
        Some("Stop"),
        &serde_json::to_vec(&stop_payload).unwrap(),
    );
    assert!(stop.status.success());
    assert!(stop.stdout.is_empty());
    assert!(stop.stderr.is_empty());
    let snapshot = LifecycleStore::at(stop_home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::Stop)
    );
    let activity = fs::read_to_string(
        stop_home
            .path()
            .join(".local/state/coding-brain/activity.jsonl"),
    )
    .unwrap();
    let row: serde_json::Value = serde_json::from_str(activity.trim()).unwrap();
    assert_eq!(row["session"]["provider"], "antigravity");

    let invocation_home = tempfile::tempdir().unwrap();
    let invocation = serde_json::json!({
        "invocationNum": 3,
        "initialNumSteps": 10,
        "conversationId": "agy-conversation-1",
        "workspacePaths": [invocation_home.path()],
        "transcriptPath": "/tmp/agy-conversation-1/transcript.jsonl",
        "artifactDirectoryPath": "/tmp/agy-conversation-1/artifacts"
    });
    let invocation = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("PreInvocation"),
        &serde_json::to_vec(&invocation).unwrap(),
    );
    assert!(invocation.status.success());
    assert!(invocation.stderr.is_empty());
    let snapshot = LifecycleStore::at(invocation_home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::UserPromptSubmit)
    );
}

#[test]
fn antigravity_optional_error_is_typed_and_false_idle_is_rejected() {
    let mut post_without_error: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    post_without_error.as_object_mut().unwrap().remove("error");
    let home = tempfile::tempdir().unwrap();
    let output = run_provider_hook_with_event(
        home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        &serde_json::to_vec(&post_without_error).unwrap(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let snapshot = LifecycleStore::at(home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );

    let mut false_idle: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_STOP).unwrap();
    false_idle["fullyIdle"] = serde_json::json!(false);
    assert_antigravity_rejected(Some("Stop"), &false_idle, "Stop with fullyIdle=false");

    for event in ["Stop", "PostToolUse"] {
        let fixture = if event == "Stop" {
            ANTIGRAVITY_STOP
        } else {
            ANTIGRAVITY_POST_TOOL_USE
        };
        for invalid_error in [
            serde_json::Value::Null,
            serde_json::json!({"message": "boom"}),
        ] {
            let mut payload: serde_json::Value = serde_json::from_slice(fixture).unwrap();
            payload["error"] = invalid_error;
            assert_antigravity_rejected(
                Some(event),
                &payload,
                &format!("{event} with non-string error"),
            );
        }
    }
}

#[test]
fn antigravity_missing_or_unknown_trusted_event_fails_open() {
    let payload: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    assert_antigravity_rejected(None, &payload, "missing trusted event");
    assert_antigravity_rejected(Some("FutureEvent"), &payload, "unknown trusted event");
}

#[test]
fn antigravity_rejects_each_missing_required_event_field() {
    let shapes = [
        (
            "stop",
            "Stop",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_STOP).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "executionNum",
                "terminationReason",
                "fullyIdle",
            ][..],
        ),
        (
            "pre-tool-use",
            "PreToolUse",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_PRE_TOOL_USE).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "stepIdx",
                "toolCall",
                "toolCall.name",
                "toolCall.args",
            ][..],
        ),
        (
            "post-tool-use",
            "PostToolUse",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_POST_TOOL_USE).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "stepIdx",
            ][..],
        ),
        (
            "invocation",
            "PostInvocation",
            serde_json::json!({
                "invocationNum": 3,
                "initialNumSteps": 10,
                "conversationId": "agy-conversation-1",
                "workspacePaths": ["/tmp"],
                "transcriptPath": "/tmp/transcript.jsonl",
                "artifactDirectoryPath": "/tmp/artifacts"
            }),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "invocationNum",
                "initialNumSteps",
            ][..],
        ),
    ];

    for (shape, event, payload, fields) in shapes {
        for field in fields {
            let mut payload = payload.clone();
            if let Some((parent, child)) = field.split_once('.') {
                payload[parent].as_object_mut().unwrap().remove(child);
            } else {
                payload.as_object_mut().unwrap().remove(*field);
            }
            assert_antigravity_rejected(Some(event), &payload, &format!("{shape} without {field}"));
        }
    }
}

#[test]
fn lifecycle_provider_comes_only_from_cli_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(CLAUDE_STOP).unwrap();
    payload["provider"] = serde_json::json!("codex");
    let output = run_provider_hook(
        home.path(),
        Some("claude"),
        &serde_json::to_vec(&payload).unwrap(),
    );
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let snapshot = LifecycleStore::at(home.path().join(".local/state/coding-brain"))
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    assert!(snapshot.sessions.contains_key(
        &AgentSessionKey::native(AgentProvider::Claude, "claude-session-1").storage_key()
    ));
    assert!(!snapshot.sessions.contains_key(
        &AgentSessionKey::native(AgentProvider::Codex, "claude-session-1").storage_key()
    ));
}

#[test]
fn provider_hook_rejects_oversized_missing_and_unknown_input_without_activity() {
    for payload in [
        vec![b'x'; 65_537],
        br#"{"hook_event_name":"Stop","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"","cwd":"/tmp","hook_event_name":"Stop","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"session","turn_id":"turn","cwd":"/tmp","hook_event_name":"PostToolUse","tool_use_id":"","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"session","cwd":"/tmp","hook_event_name":"FutureEvent","secret":"do not echo"}"#.to_vec(),
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = run_provider_hook(home.path(), Some("claude"), &payload);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.starts_with("coding-brain lifecycle hook:"));
        assert!(diagnostic.len() < 256);
        assert!(!diagnostic.contains("secret"));
        assert!(!home.path().join(".local/state/coding-brain/activity.jsonl").exists());
        assert!(!home.path().join(".local/state/coding-brain/hooks/lifecycle.json").exists());
    }
}

fn run_cli(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .args(args)
        .env("HOME", home)
        .current_dir(home)
        .output()
        .unwrap()
}

fn run_init_check(home: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .args(["init", "--check"])
        .env("HOME", home)
        .env("PATH", "")
        .current_dir(home)
        .output()
        .unwrap()
}

#[test]
fn init_noninteractive_selectors_write_stable_provider_marker_keys() {
    let home = tempfile::tempdir().unwrap();
    let output = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(
            home.path()
                .join(".local/state/coding-brain/onboarding.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["phases"]["hooks.claude"]["status"], "installed");
    assert_eq!(marker["phases"]["hooks.antigravity"]["status"], "installed");
    assert!(marker["phases"].get("plugin").is_none());
    assert!(marker["phases"].get("hooks.codex").is_none());
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(home.path().join(".gemini/config/hooks.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());
}

#[test]
fn subsequent_init_preserves_previous_provider_records_for_check_and_remove() {
    let home = tempfile::tempdir().unwrap();
    for provider in ["claude", "codex"] {
        let output = run_cli(
            home.path(),
            &[
                "init",
                provider,
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        );
        assert!(output.status.success());
    }

    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(
            home.path()
                .join(".local/state/coding-brain/onboarding.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["phases"]["hooks.codex"]["status"], "installed");
    assert_eq!(marker["phases"]["hooks.claude"]["status"], "installed");
    assert!(run_init_check(home.path()).status.success());

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(remove.status.success());
    for path in [".codex/hooks.json", ".claude/settings.json"] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }
}

#[test]
fn init_legacy_noninteractive_and_plugin_only_print_exact_replacements() {
    let home = tempfile::tempdir().unwrap();
    let noninteractive = run_cli(
        home.path(),
        &["init", "--non-interactive", "--skip-brain", "--skip-skills"],
    );
    assert!(noninteractive.status.success());
    assert_eq!(
        String::from_utf8(noninteractive.stderr)
            .unwrap()
            .lines()
            .next(),
        Some(
            "warning: provider-less --non-interactive is deprecated; use `coding-brain init codex --non-interactive` instead"
        )
    );

    let plugin_only = run_cli(home.path(), &["init", "--plugin-only"]);
    assert!(plugin_only.status.success());
    assert_eq!(
        String::from_utf8(plugin_only.stderr)
            .unwrap()
            .lines()
            .next(),
        Some("warning: --plugin-only is deprecated; use `coding-brain init codex` instead")
    );
}

#[test]
fn init_all_cannot_be_combined_with_another_selector() {
    let home = tempfile::tempdir().unwrap();
    let output = run_cli(home.path(), &["init", "all", "codex", "--non-interactive"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("`all` cannot be combined with another provider selector")
    );
}

#[test]
fn init_check_upgrade_and_remove_use_recorded_providers() {
    let home = tempfile::tempdir().unwrap();
    let init = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(init.status.success());
    assert!(run_init_check(home.path()).status.success());

    fs::remove_file(home.path().join(".claude/settings.json")).unwrap();
    assert!(!run_init_check(home.path()).status.success());
    assert!(
        run_cli(home.path(), &["init", "--upgrade"])
            .status
            .success()
    );
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());

    let claude_path = home.path().join(".claude/settings.json");
    let mut claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    claude["keep"] = serde_json::json!("claude-user-setting");
    fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();
    let antigravity_path = home.path().join(".gemini/config/hooks.json");
    let mut antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    antigravity["keep"] = serde_json::json!({"command": "antigravity-user-setting"});
    fs::write(
        &antigravity_path,
        serde_json::to_vec_pretty(&antigravity).unwrap(),
    )
    .unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(
        remove.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&remove.stdout),
        String::from_utf8_lossy(&remove.stderr)
    );
    let claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    let antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    assert_eq!(claude, serde_json::json!({"keep": "claude-user-setting"}));
    assert_eq!(
        antigravity,
        serde_json::json!({"keep": {"command": "antigravity-user-setting"}})
    );
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/onboarding.json")
            .exists()
    );
}

#[test]
fn init_upgrade_retries_drift_and_leaves_skipped_providers_untouched() {
    let home = tempfile::tempdir().unwrap();
    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(
        &marker,
        br#"{"version":"0.0.1","completed_at":"now","phases":{"hooks.claude":{"status":"drift"},"hooks.antigravity":{"status":"skipped"}}}"#,
    )
    .unwrap();

    let upgrade = run_cli(home.path(), &["init", "--upgrade"]);

    assert!(
        upgrade.status.success(),
        "{}",
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(!home.path().join(".gemini/config/hooks.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());
}

#[test]
fn init_remove_keeps_marker_when_multi_provider_staging_fails() {
    let home = tempfile::tempdir().unwrap();
    let init = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(init.status.success());
    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    let marker_before = fs::read(&marker).unwrap();
    let claude_path = home.path().join(".claude/settings.json");
    let claude_before = fs::read(&claude_path).unwrap();
    let antigravity = home.path().join(".gemini/config/hooks.json");
    fs::remove_file(&antigravity).unwrap();
    fs::create_dir(&antigravity).unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);

    assert!(!remove.status.success());
    assert_eq!(fs::read(&marker).unwrap(), marker_before);
    assert_eq!(fs::read(&claude_path).unwrap(), claude_before);
}

#[test]
fn init_remove_cleans_all_exact_provider_hooks_without_marker_authority() {
    for marker_state in ["missing", "corrupt", "subset"] {
        let home = tempfile::tempdir().unwrap();
        let init = run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        );
        assert!(init.status.success());
        let marker = home
            .path()
            .join(".local/state/coding-brain/onboarding.json");
        match marker_state {
            "missing" => fs::remove_file(&marker).unwrap(),
            "corrupt" => fs::write(&marker, b"{broken").unwrap(),
            "subset" => fs::write(
                &marker,
                br#"{"version":"0.58.0","completed_at":"now","phases":{"hooks.codex":{"status":"installed"}}}"#,
            )
            .unwrap(),
            _ => unreachable!(),
        }

        let remove = run_cli(home.path(), &["init", "--remove"]);
        assert!(
            remove.status.success(),
            "{marker_state}: {}",
            String::from_utf8_lossy(&remove.stderr)
        );
        for path in [
            ".codex/hooks.json",
            ".claude/settings.json",
            ".gemini/config/hooks.json",
        ] {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
            assert_eq!(value, serde_json::json!({}), "{marker_state}: {path}");
        }
    }
}

#[test]
fn init_remove_preserves_unrelated_and_modified_entries_without_marker_authority() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );
    let codex_path = home.path().join(".codex/hooks.json");
    let mut codex: serde_json::Value =
        serde_json::from_slice(&fs::read(&codex_path).unwrap()).unwrap();
    codex["keep"] = serde_json::json!("codex-user-setting");
    fs::write(&codex_path, serde_json::to_vec_pretty(&codex).unwrap()).unwrap();

    let claude_path = home.path().join(".claude/settings.json");
    let mut claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    claude["keep"] = serde_json::json!("claude-user-setting");
    let command = claude["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    claude["hooks"]["Stop"][0]["hooks"][0]["command"] =
        serde_json::json!(format!("{command} --user-option"));
    fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();

    let antigravity_path = home.path().join(".gemini/config/hooks.json");
    let mut antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    antigravity["keep"] = serde_json::json!({"command": "user-setting"});
    let command = antigravity["coding-brain"]["Stop"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    antigravity["coding-brain"]["Stop"][0]["command"] =
        serde_json::json!(format!("{command} --user-option"));
    fs::write(
        &antigravity_path,
        serde_json::to_vec_pretty(&antigravity).unwrap(),
    )
    .unwrap();
    fs::write(
        home.path()
            .join(".local/state/coding-brain/onboarding.json"),
        b"{broken",
    )
    .unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);

    assert!(remove.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&codex_path).unwrap()).unwrap(),
        serde_json::json!({"keep": "codex-user-setting"})
    );
    let claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    assert_eq!(claude["keep"], "claude-user-setting");
    assert!(
        claude["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("--user-option")
    );
    let antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    assert_eq!(antigravity["keep"]["command"], "user-setting");
    assert!(
        antigravity["coding-brain"]["Stop"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("--user-option")
    );
}

#[test]
fn init_purge_removes_all_exact_provider_hooks_without_a_marker() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );
    fs::remove_file(
        home.path()
            .join(".local/state/coding-brain/onboarding.json"),
    )
    .unwrap();

    let purge = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(purge.status.success());
    for path in [
        ".codex/hooks.json",
        ".claude/settings.json",
        ".gemini/config/hooks.json",
    ] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({}), "{path}");
    }
}

#[cfg(unix)]
#[test]
fn init_purge_stops_before_deleting_targets_when_provider_staging_fails() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );

    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    let state_sentinel = home.path().join(".local/state/coding-brain/keep");
    let config = home.path().join(".config/coding-brain/config.toml");
    let legacy_state = home.path().join(".codexctl/keep");
    let legacy_config = home.path().join(".config/codexctl/config.toml");
    for (path, contents) in [
        (&state_sentinel, b"state".as_slice()),
        (&config, b"config".as_slice()),
        (&legacy_state, b"legacy-state".as_slice()),
        (&legacy_config, b"legacy-config".as_slice()),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    let codex = home.path().join(".codex/hooks.json");
    let claude = home.path().join(".claude/settings.json");
    let antigravity = home.path().join(".gemini/config/hooks.json");
    let antigravity_target = home.path().join("antigravity-hooks-target.json");
    let antigravity_before = fs::read(&antigravity).unwrap();
    fs::remove_file(&antigravity).unwrap();
    fs::write(&antigravity_target, &antigravity_before).unwrap();
    symlink(&antigravity_target, &antigravity).unwrap();

    let preserved_files = [
        (&marker, fs::read(&marker).unwrap()),
        (&state_sentinel, fs::read(&state_sentinel).unwrap()),
        (&config, fs::read(&config).unwrap()),
        (&legacy_state, fs::read(&legacy_state).unwrap()),
        (&legacy_config, fs::read(&legacy_config).unwrap()),
        (&codex, fs::read(&codex).unwrap()),
        (&claude, fs::read(&claude).unwrap()),
    ];

    let purge = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(!purge.status.success());
    for (path, contents) in preserved_files {
        assert_eq!(
            fs::read(path).unwrap(),
            contents,
            "{} changed",
            path.display()
        );
    }
    assert!(
        antigravity
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&antigravity_target).unwrap(), antigravity_before);
}

fn install_crash_journal(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let target = home.join(name);
    let original = b"{\"original\":true}\n".to_vec();
    let replacement = b"{\"replacement\":true}\n".to_vec();
    fs::write(&target, &replacement).unwrap();
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let journal = serde_json::json!({
        "schema_version": 2,
        "transaction_id": "integration-crash",
        "edits": [{
            "path": target,
            "original": original,
            "original_mode": null,
            "original_hash": hash(&original),
            "replacement": replacement,
            "replacement_hash": hash(&replacement)
        }],
        "replaced_paths": [target],
        "in_flight": null
    });
    let path = home.join(".local/state/coding-brain/brain/hook-install-transaction.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
    target
}

#[test]
fn init_plugin_only_recovers_before_the_current_hooks_early_return() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(home.path(), &["init", "--plugin-only"])
            .status
            .success()
    );
    let target = install_crash_journal(home.path(), "plugin-recovery.json");

    let output = run_cli(home.path(), &["init", "--plugin-only"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn init_plugin_only_reports_preserved_collision_without_calling_it_current() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(home.path(), &["init", "--plugin-only"])
            .status
            .success()
    );
    let hooks_path = home.path().join(".codex/hooks.json");
    let mut hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    hooks["hooks"]["Stop"][0]["hooks"][0]["command"] = serde_json::json!(format!(
        "{} --user-option",
        hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
    ));
    let original = serde_json::to_vec_pretty(&hooks).unwrap();
    fs::write(&hooks_path, &original).unwrap();

    let output = run_cli(home.path(), &["init", "--plugin-only"]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Preserved user-modified"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("hooks are current"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("No managed hook changes applied"));
    assert_eq!(fs::read(hooks_path).unwrap(), original);
}

#[test]
fn init_reset_recovers_a_pending_hook_transaction_first() {
    let home = tempfile::tempdir().unwrap();
    let target = install_crash_journal(home.path(), "reset-recovery.json");

    let output = run_cli(home.path(), &["init", "--reset"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn init_purge_recovers_a_pending_hook_transaction_first() {
    let home = tempfile::tempdir().unwrap();
    let target = install_crash_journal(home.path(), "purge-recovery.json");

    let output = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn doctor_fails_when_pending_hook_recovery_is_invalid() {
    let home = tempfile::tempdir().unwrap();
    let journal = home
        .path()
        .join(".local/state/coding-brain/brain/hook-install-transaction.json");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(journal, b"not json").unwrap();

    let output = run_cli(home.path(), &["doctor"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Provider hook recovery"));
}

fn prompt_payload(index: usize) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(PROMPT).unwrap();
    payload["turn_id"] = serde_json::json!(format!("turn-{index}"));
    serde_json::to_vec(&payload).unwrap()
}

#[cfg(unix)]
fn run_permission_hook(home: &std::path::Path, input: &[u8]) -> Output {
    let mut paths = vec![home.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(paths).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .arg("--permission-hook")
        .env("HOME", home)
        .env("PATH", path)
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
    fs::write(&curl, format!("#!/usr/bin/env sh\nprintf '%s' '{body}'\n")).unwrap();
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
fn permission_allow_is_suppressed_across_lifecycle_failure() {
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
    assert!(blocked_output.stdout.is_empty());
    assert!(
        !healthy_output.stdout.is_empty(),
        "healthy permission hook wrote no response; stderr: {}",
        String::from_utf8_lossy(&healthy_output.stderr)
    );
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
