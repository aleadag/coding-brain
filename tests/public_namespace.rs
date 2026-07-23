use std::io::Write;
use std::process::{Command, Stdio};

fn isolated_command(temp: &tempfile::TempDir) -> Command {
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command
        .current_dir(project)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1");
    command
}

#[test]
fn ordinary_commands_ignore_and_preserve_legacy_namespace() {
    let temp = tempfile::tempdir().unwrap();
    let old_config = temp.path().join("home/.config/codexctl/config.toml");
    let old_state = temp.path().join("home/.codexctl/brain/decisions.jsonl");
    std::fs::create_dir_all(old_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(old_state.parent().unwrap()).unwrap();
    std::fs::write(&old_config, b"[brain]\nmodel = \"legacy-model\"\n").unwrap();
    std::fs::write(&old_state, b"legacy-state\n").unwrap();
    let before_config = std::fs::read(&old_config).unwrap();
    let before_state = std::fs::read(&old_state).unwrap();

    let help = isolated_command(&temp).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).starts_with("Supervise Codex"));

    let config = isolated_command(&temp)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(config.status.success());
    let config_stdout = String::from_utf8_lossy(&config.stdout);
    assert!(config_stdout.contains("coding-brain/config.toml"));
    assert!(!config_stdout.contains("legacy-model"));

    let doctor = isolated_command(&temp).arg("doctor").output().unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("coding-brain doctor"));

    let mut hook = isolated_command(&temp)
        .arg("--permission-hook")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(b"{}\n").unwrap();
    let hook = hook.wait_with_output().unwrap();
    assert!(hook.status.success());
    assert!(hook.stdout.is_empty());

    assert_eq!(std::fs::read(old_config).unwrap(), before_config);
    assert_eq!(std::fs::read(old_state).unwrap(), before_state);
}

#[test]
fn stale_hooks_are_diagnostic_until_init() {
    let temp = tempfile::tempdir().unwrap();
    let hooks_path = temp.path().join("home/.codex/hooks.json");
    std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let mut hooks = serde_json::Map::new();
    for (event, matcher, argument, timeout) in [
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
        ("Stop", None, "--recovery-hook", 30),
    ] {
        let mut handler = serde_json::json!({
            "type": "command",
            "command": format!("codexctl {argument}"),
            "timeout": timeout,
        });
        if event == "PermissionRequest" {
            handler["statusMessage"] = serde_json::json!("Brain reviewing permission…");
        }
        let mut entry = serde_json::json!({ "hooks": [handler] });
        if let Some(matcher) = matcher {
            entry["matcher"] = serde_json::json!(matcher);
        }
        hooks.insert(event.into(), serde_json::json!([entry]));
    }
    hooks.insert(
        "Notification".into(),
        serde_json::json!([{ "hooks": [{ "type": "command", "command": "notify-send keep" }] }]),
    );
    std::fs::write(
        &hooks_path,
        serde_json::to_vec_pretty(&serde_json::json!({ "hooks": hooks })).unwrap(),
    )
    .unwrap();

    let doctor = isolated_command(&temp).arg("doctor").output().unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("definition stale"));
    let unchanged = std::fs::read(&hooks_path).unwrap();

    let init = isolated_command(&temp)
        .args(["init", "--plugin-only"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let rewritten = std::fs::read_to_string(&hooks_path).unwrap();
    assert_ne!(rewritten.as_bytes(), unchanged);
    assert!(rewritten.contains(&format!(
        "{} --permission-hook",
        env!("CARGO_BIN_EXE_coding-brain")
    )));
    assert!(!rewritten.contains("\"codexctl --permission-hook\""));
    assert!(rewritten.contains("notify-send keep"));
}

#[test]
fn doctor_reports_identity_and_remote_endpoint_risks() {
    let temp = tempfile::tempdir().unwrap();
    let config_path = temp.path().join("config/coding-brain/config.toml");
    let manifest_path = temp.path().join("project/.coding-brain/project.toml");
    std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
    std::fs::write(&manifest_path, "not valid toml").unwrap();
    std::fs::write(
        &config_path,
        "[brain]\nendpoint = \"https://brain.example.invalid/v1\"\n",
    )
    .unwrap();

    let https = isolated_command(&temp).arg("doctor").output().unwrap();
    let https_stdout = String::from_utf8_lossy(&https.stdout);
    assert!(https_stdout.contains("project manifest is malformed"));
    assert!(https_stdout.contains("transcript context may leave this machine"));
    assert!(!https_stdout.contains("plaintext HTTP"));

    std::fs::write(
        &config_path,
        "[brain]\nendpoint = \"http://brain.example.invalid/v1\"\n",
    )
    .unwrap();
    let http = isolated_command(&temp).arg("doctor").output().unwrap();
    let http_stdout = String::from_utf8_lossy(&http.stdout);
    assert!(http_stdout.contains("remote plaintext HTTP"));
    assert!(http_stdout.contains("exposed in transit"));
}
