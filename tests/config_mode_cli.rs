use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn command(temp: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1");
    command
}

fn run(temp: &tempfile::TempDir, args: &[&str]) -> Output {
    let mut child = command(temp)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn coding-brain {}: {error}", args.join(" ")));
    let deadline = Instant::now() + PROCESS_TIMEOUT;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child.wait_with_output().unwrap_or_else(|error| {
                    panic!(
                        "failed to collect coding-brain {} output: {error}",
                        args.join(" ")
                    )
                });
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap_or_else(|error| {
                    panic!(
                        "coding-brain {} timed out and output collection failed: {error}",
                        args.join(" ")
                    )
                });
                panic!(
                    "coding-brain {} timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    PROCESS_TIMEOUT,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("failed to poll coding-brain {}: {error}", args.join(" "));
            }
        }
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn mode_path(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("state/coding-brain/brain/gate-mode")
}

fn write_user_config(temp: &tempfile::TempDir, content: &str) {
    let path = temp.path().join("config/coding-brain/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn every_config_action_exits_without_entering_the_tui() {
    let temp = tempfile::tempdir().unwrap();

    for args in [
        &["config", "set", "mode", "auto"][..],
        &["config", "get", "mode"],
        &["config", "show"],
        &["config", "template"],
        &["config", "init"],
        &["config", "validate"],
    ] {
        assert_success(&run(&temp, args));
    }
}

#[test]
fn config_set_then_get_mode_shares_isolated_xdg_state() {
    let temp = tempfile::tempdir().unwrap();
    let set = run(&temp, &["config", "set", "mode", "auto"]);
    assert_success(&set);
    assert_eq!(stdout(&set), "mode: auto\n");

    let get = run(&temp, &["config", "get", "mode"]);
    assert_success(&get);
    assert_eq!(stdout(&get), "mode: auto\n");
    assert_eq!(std::fs::read_to_string(mode_path(&temp)).unwrap(), "auto\n");
}

#[test]
fn config_get_mode_maps_all_legacy_combinations_when_state_is_absent() {
    for (enabled, automatic, expected) in [
        (false, false, "off"),
        (true, false, "on"),
        (true, true, "auto"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write_user_config(
            &temp,
            &format!("[brain]\nenabled = {enabled}\nauto = {automatic}\n"),
        );

        let get = run(&temp, &["config", "get", "mode"]);
        assert_success(&get);
        assert_eq!(stdout(&get), format!("mode: {expected}\n"));
        assert!(!mode_path(&temp).exists());
    }
}

#[test]
fn active_brain_config_without_legacy_mode_fields_defaults_off() {
    let temp = tempfile::tempdir().unwrap();
    write_user_config(
        &temp,
        concat!(
            "[brain]\n",
            "endpoint = \"http://localhost:11434/api/generate\"\n",
            "model = \"local-model\"\n",
            "timeout_ms = 7500\n",
        ),
    );

    let get = run(&temp, &["config", "get", "mode"]);

    assert_success(&get);
    assert_eq!(stdout(&get), "mode: off\n");
    assert!(!mode_path(&temp).exists());
}

#[test]
fn auto_only_legacy_config_still_maps_to_auto() {
    let temp = tempfile::tempdir().unwrap();
    write_user_config(&temp, "[brain]\nauto = true\n");

    let get = run(&temp, &["config", "get", "mode"]);

    assert_success(&get);
    assert_eq!(stdout(&get), "mode: auto\n");
    assert!(!mode_path(&temp).exists());
}

#[test]
fn explicit_mode_state_wins_over_legacy_config_without_rewriting_config() {
    let temp = tempfile::tempdir().unwrap();
    let legacy = "[brain]\nenabled = false\nauto = false\n";
    write_user_config(&temp, legacy);
    assert_success(&run(&temp, &["config", "set", "mode", "auto"]));

    let get = run(&temp, &["config", "get", "mode"]);
    assert_success(&get);
    assert_eq!(stdout(&get), "mode: auto\n");
    assert_eq!(
        std::fs::read_to_string(temp.path().join("config/coding-brain/config.toml")).unwrap(),
        legacy
    );
}

#[test]
fn corrupt_explicit_mode_fails_closed_and_is_not_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let path = mode_path(&temp);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "automatic\n").unwrap();

    let get = run(&temp, &["config", "get", "mode"]);
    assert_success(&get);
    let output = stdout(&get);
    assert!(output.starts_with("mode: off\nwarning: "), "{output}");
    assert!(output.contains("coding-brain config set mode <off|on|auto>"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "automatic\n");
}

#[test]
fn invalid_keys_and_values_fail_without_writing_mode_state() {
    for args in [
        &["config", "get", "theme"][..],
        &["config", "set", "theme", "dark"],
        &["config", "set", "mode", "automatic"],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let output = run(&temp, args);
        assert!(
            !output.status.success(),
            "{} unexpectedly succeeded",
            args.join(" ")
        );
        assert!(!mode_path(&temp).exists());
    }
}

#[test]
fn removed_mode_flags_fail_at_process_boundary() {
    let temp = tempfile::tempdir().unwrap();
    for flag in [
        "--brain",
        "--auto-run",
        "--mode",
        "--config",
        "--config-template",
        "--config-validate",
        "--config-init",
    ] {
        assert!(
            !run(&temp, &[flag]).status.success(),
            "{flag} unexpectedly succeeded"
        );
    }
}

#[test]
fn active_config_outputs_omit_legacy_mode_keys() {
    let temp = tempfile::tempdir().unwrap();
    write_user_config(
        &temp,
        "[brain]\nenabled = true\nauto = true\nmodel = \"local-model\"\n",
    );

    let show = run(&temp, &["config", "show"]);
    assert_success(&show);
    let show = stdout(&show);
    assert!(!show.contains("enabled:"), "{show}");
    assert!(!show.contains("auto:"), "{show}");
    assert!(show.contains("model:    local-model"), "{show}");

    let template = run(&temp, &["config", "template"]);
    assert_success(&template);
    assert_active_config_omits_legacy_keys(&stdout(&template));

    let init = run(&temp, &["config", "init"]);
    assert_success(&init);
    let initialized = std::fs::read_to_string(temp.path().join(".coding-brain.toml")).unwrap();
    assert_active_config_omits_legacy_keys(&initialized);
}

#[test]
fn explicit_on_allows_insights_and_emits_endpoint_warning_over_legacy_disabled() {
    let temp = tempfile::tempdir().unwrap();
    write_user_config(
        &temp,
        "[brain]\nenabled = false\nendpoint = \"http://brain.example.test/api/generate\"\n",
    );
    assert_success(&run(&temp, &["config", "set", "mode", "on"]));

    let insights = run(&temp, &["--insights", "status"]);

    assert_success(&insights);
    assert!(stdout(&insights).contains("Insights mode:"));
    assert!(
        String::from_utf8_lossy(&insights.stderr).contains("remote plaintext HTTP"),
        "{}",
        String::from_utf8_lossy(&insights.stderr)
    );
}

#[test]
fn explicit_off_rejects_insights_and_suppresses_endpoint_warning_over_legacy_enabled() {
    let temp = tempfile::tempdir().unwrap();
    write_user_config(
        &temp,
        "[brain]\nenabled = true\nendpoint = \"http://brain.example.test/api/generate\"\n",
    );
    assert_success(&run(&temp, &["config", "set", "mode", "off"]));

    let insights = run(&temp, &["--insights", "status"]);

    assert!(!insights.status.success());
    let stderr = String::from_utf8_lossy(&insights.stderr);
    assert!(
        stderr.contains("requires Brain mode on or auto"),
        "{stderr}"
    );
    assert!(!stderr.contains("remote plaintext HTTP"), "{stderr}");
}

fn assert_active_config_omits_legacy_keys(config: &str) {
    for key in ["enabled =", "auto =", "terminal_auto_approve_fallback"] {
        assert!(
            !config.contains(key),
            "active config contains {key:?}:\n{config}"
        );
    }
    assert!(config.contains("endpoint ="), "{config}");
    assert!(config.contains("model ="), "{config}");
    assert!(config.contains("timeout_ms ="), "{config}");
}
