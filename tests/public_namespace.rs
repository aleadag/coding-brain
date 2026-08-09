use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn isolated_command(temp: &tempfile::TempDir) -> Command {
    let home = temp.path().join("home");
    let config = temp.path().join("config");
    let state = temp.path().join("state");
    let project = temp.path().join("project");
    for directory in [temp.path(), &home, &config, &state, &project] {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command
        .current_dir(project)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1");
    command
}

#[test]
fn cli_is_cbrain_while_public_namespaces_remain_coding_brain() {
    let temp = tempfile::tempdir().unwrap();
    let help = isolated_command(&temp).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Usage: cbrain"));

    let config = isolated_command(&temp)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&config.stdout).contains("coding-brain/config.toml"));
}

#[test]
fn storage_namespace_exposes_only_static_export_and_reset_actions() {
    let temp = tempfile::tempdir().unwrap();
    let help = isolated_command(&temp)
        .args(["storage", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("export-audit"), "{help}");
    assert!(help.contains("export-legacy"), "{help}");
    assert!(help.contains("reset-review-state"), "{help}");
    assert!(!help.contains("activate"), "{help}");
    assert!(!help.contains("import"), "{help}");
}

#[test]
fn current_documentation_uses_cbrain_commands_and_preserves_namespaces() {
    let launch_posts = include_str!("../LAUNCH_POSTS.md");
    let contributing = include_str!("../docs/contributing.md");
    let agents = include_str!("../AGENTS.md");
    let current_docs = [
        ("README", include_str!("../README.md")),
        ("configuration", include_str!("../docs/configuration.md")),
        ("quickstart", include_str!("../docs/quickstart.md")),
        ("reference", include_str!("../docs/reference.md")),
        (
            "troubleshooting",
            include_str!("../docs/troubleshooting.md"),
        ),
        ("launch posts", launch_posts),
        ("contributing", contributing),
        ("AGENTS", agents),
    ];
    for (name, document) in current_docs {
        assert!(document.contains("cbrain"), "{name}");
        assert!(!document.contains("Run `coding-brain"), "{name}");
        assert!(
            document.contains("coding-brain"),
            "{name} must retain package or path context"
        );
    }

    let stale_launch_block =
        "cargo install coding-brain\ncoding-brain init all\ncoding-brain doctor\ncoding-brain";
    let current_launch_block = "cargo install coding-brain\ncbrain init all\ncbrain doctor\ncbrain";
    assert!(!launch_posts.contains(stale_launch_block), "launch posts");
    assert!(launch_posts.contains(current_launch_block), "launch posts");
    assert!(
        !contributing.contains("the `coding-brain` CLI"),
        "contributing"
    );
    assert!(contributing.contains("the `cbrain` CLI"), "contributing");
    assert!(
        !agents.contains("# coding-brain binary:"),
        "AGENTS architecture"
    );
    assert!(agents.contains("# cbrain binary:"), "AGENTS architecture");
}

#[test]
fn current_source_executable_surfaces_use_cbrain() {
    let justfile = include_str!("../justfile");
    assert!(justfile.contains("cargo run --bin cbrain -- {{args}}"));
    assert!(!justfile.contains("cargo run --bin coding-brain"));

    let terminals = include_str!("../crates/coding-brain-core/src/terminals/mod.rs");
    assert!(terminals.contains("cbrain doctor"));
    assert!(!terminals.contains("coding-brain doctor"));

    let hooks = include_str!("../crates/coding-brain-core/src/hooks.rs");
    assert!(hooks.contains("`cbrain --hooks`"));
    assert!(!hooks.contains("`coding-brain --hooks`"));

    for (name, source, current, stale) in [
        (
            "permission hook",
            include_str!("../src/brain/permission_hook.rs"),
            "cbrain permission hook:",
            "coding-brain permission hook:",
        ),
        (
            "recovery hook",
            include_str!("../src/brain/recovery.rs"),
            "cbrain recovery hook:",
            "coding-brain recovery hook:",
        ),
        (
            "lifecycle hook",
            include_str!("../src/lifecycle_hook.rs"),
            "cbrain lifecycle hook:",
            "coding-brain lifecycle hook:",
        ),
    ] {
        assert!(source.contains(current), "{name}");
        assert!(!source.contains(stale), "{name}");
    }
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
    let help_stdout = String::from_utf8_lossy(&help.stdout);
    assert!(help_stdout.starts_with("Supervise coding-agent activity"));
    assert!(!help_stdout.starts_with("Supervise Codex"));

    let config = isolated_command(&temp)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(config.status.success());
    let config_stdout = String::from_utf8_lossy(&config.stdout);
    assert!(config_stdout.contains("coding-brain/config.toml"));
    assert!(!config_stdout.contains("legacy-model"));

    let doctor = isolated_command(&temp).arg("doctor").output().unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("cbrain doctor"));

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
fn front_door_metadata_is_provider_aware() {
    let cargo = include_str!("../Cargo.toml");
    let flake = include_str!("../flake.nix");
    let agents = include_str!("../AGENTS.md");
    let homebrew_renderer = include_str!("../scripts/render-homebrew-formula.sh");
    let aur_renderer = include_str!("../scripts/render-aur-bin-files.sh");
    let homebrew_formula = include_str!("../packaging/homebrew-core/coding-brain.rb");
    let aur_pkgbuild = include_str!("../packaging/aur/coding-brain-bin/PKGBUILD");
    let aur_srcinfo = include_str!("../packaging/aur/coding-brain-bin/.SRCINFO");
    let nixpkgs_readme = include_str!("../packaging/nixpkgs/README.md");

    let description = "Local brain for supervising and learning from coding-agent activity.";
    for (name, metadata) in [
        ("Cargo.toml", cargo),
        ("flake.nix", flake),
        ("Homebrew renderer", homebrew_renderer),
        ("AUR renderer", aur_renderer),
        ("Homebrew formula", homebrew_formula),
        ("AUR PKGBUILD", aur_pkgbuild),
        ("AUR .SRCINFO", aur_srcinfo),
        ("nixpkgs README", nixpkgs_readme),
    ] {
        assert!(metadata.contains(description), "{name}");
        assert!(!metadata.contains("Codex sessions"), "{name}");
    }

    assert!(agents.starts_with("# coding-brain\n\nLocal-brain companion for supervising and learning from coding-agent activity."));
    assert!(agents.contains("$XDG_CONFIG_HOME/coding-brain"));
    assert!(agents.contains("$XDG_STATE_HOME/coding-brain"));
    assert!(agents.contains("Legacy codexctl paths remain untouched for rollback."));

    for (name, metadata) in [
        ("flake.nix", flake),
        ("Homebrew renderer", homebrew_renderer),
        ("AUR renderer", aur_renderer),
    ] {
        assert!(metadata.contains("aleadag/coding-brain"), "{name}");
        assert!(!metadata.contains("aleadag/codexctl"), "{name}");
    }
    assert!(flake.contains("pname = \"coding-brain\""));
    assert!(flake.contains("mainProgram = \"cbrain\""));
    assert!(!flake.contains("mainProgram = \"coding-brain\""));

    for (name, metadata) in [
        ("Homebrew renderer", homebrew_renderer),
        ("Homebrew formula", homebrew_formula),
    ] {
        assert!(metadata.contains("bin/\"cbrain\""), "{name}");
        assert!(metadata.contains("man1/\"cbrain.1\""), "{name}");
        assert!(metadata.contains("#{bin}/cbrain"), "{name}");
        assert!(!metadata.contains("#{bin}/coding-brain"), "{name}");
    }
    assert!(homebrew_renderer.contains("bin.install \"cbrain\""));
    assert!(!homebrew_renderer.contains("bin.install \"coding-brain\""));

    let aur_install = r#"install -Dm755 "\${srcdir}/cbrain" "\${pkgdir}/usr/bin/cbrain""#;
    assert!(aur_renderer.contains(aur_install));
    assert!(aur_pkgbuild.contains(&aur_install.replace("\\$", "$")));
    for (name, metadata) in [
        ("AUR renderer", aur_renderer),
        ("AUR PKGBUILD", aur_pkgbuild),
    ] {
        assert!(!metadata.contains("/usr/bin/coding-brain"), "{name}");
        assert!(metadata.contains("provides=('coding-brain')"), "{name}");
    }
    assert!(aur_srcinfo.contains("pkgbase = coding-brain-bin"));
    assert!(aur_srcinfo.contains("provides = coding-brain"));

    assert!(nixpkgs_readme.contains("pname = \"coding-brain\""));
    assert!(nixpkgs_readme.contains("mainProgram = \"cbrain\""));
    assert!(nixpkgs_readme.contains("confirm `cbrain --help` runs"));
}

#[test]
fn provider_documentation_scopes_usage_and_transcript_context() {
    let readme = include_str!("../README.md");
    let index = include_str!("../docs/index.md");
    let llms = include_str!("../docs/llms.txt");
    let quickstart = include_str!("../docs/quickstart.md");
    let reference = include_str!("../docs/reference.md");
    let boundary = "Coding Brain does not collect or display token usage or cost.";
    let retained = "Coding Brain may derive a bounded context-window percentage for context-rot prevention, but it does not retain the provider token counts used to derive it.";
    let only = "Only that bounded percentage is retained.";
    let capacity = "The percentage uses provider-supplied context capacity when available and otherwise a known-model fallback; it is not raw usage or cost accounting.";

    for (name, documentation) in [
        ("README", readme),
        ("documentation index", index),
        ("LLM index", llms),
        ("quick start", quickstart),
        ("reference", reference),
    ] {
        assert!(documentation.contains(boundary), "{name}");
        assert!(documentation.contains(retained), "{name}");
        assert!(documentation.contains(only), "{name}");
        assert!(documentation.contains(capacity), "{name}");
        assert!(
            !documentation.contains("Intentionally not collected or displayed"),
            "{name}"
        );
    }

    assert!(reference.contains("not parsed into `AgentSession` context"));
    assert!(reference.contains("retained as lifecycle identity/status evidence"));
    assert!(reference.contains("SQLite is not read"));
}

#[test]
fn production_source_has_no_usage_or_cost_surfaces() {
    const FORBIDDEN: &[&str] = &[
        "cost_usd",
        "burn_rate_per_hr",
        "priced_total_tokens",
        "usage_metrics_available",
        "cost_estimate_unverified",
        "input_per_m",
        "output_per_m",
        "cache_read_per_m",
        "cache_write_per_m",
        "CostBelow",
        "CostAbove",
        "median_cost_usd",
        "avg_downstream_cost",
    ];

    fn scan_rust(root: &Path, violations: &mut Vec<String>) {
        let mut paths = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();

        for path in paths {
            if path.is_dir() {
                scan_rust(&path, violations);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                for forbidden in FORBIDDEN {
                    if source.contains(forbidden) {
                        violations.push(format!("{}: {forbidden}", path.display()));
                    }
                }
            }
        }
    }

    let mut violations = Vec::new();
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for root in [
        "src",
        "crates/coding-brain-core/src",
        "crates/coding-brain-tui/src",
    ] {
        scan_rust(&manifest_dir.join(root), &mut violations);
    }

    assert!(
        violations.is_empty(),
        "forbidden production identifiers:\n{}",
        violations.join("\n")
    );
}

#[test]
fn stale_hooks_are_diagnostic_until_init() {
    for program in ["codexctl", "coding-brain"] {
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
                "command": format!("{program} {argument}"),
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
        hooks
            .get_mut("PermissionRequest")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "coding-brain-wrapper --permission-hook",
                    "timeout": 30,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }));
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
            env!("CARGO_BIN_EXE_cbrain")
        )));
        assert!(!rewritten.contains(&format!("\"{program} --permission-hook\"")));
        assert!(rewritten.contains("coding-brain-wrapper --permission-hook"));
        assert!(rewritten.contains("notify-send keep"));
    }
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
