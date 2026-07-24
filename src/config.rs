use std::fs;
use std::path::Path;
use std::path::PathBuf;

use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment, PathError};

#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Configuration loaded from TOML files, merged with CLI flags.
/// Priority: CLI flags > project config > user config > defaults.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub theme: Option<String>,
    pub brain: Option<BrainConfig>,
}

/// `BrainConfig` and friends live in `coding_brain_core::config` so the future
/// TUI crate (#275) can hold them without depending on the binary. Re-exported
/// here so existing `crate::config::BrainConfig` callers keep resolving.
pub use coding_brain_core::config::BrainConfig;

/// Raw TOML representation — all fields optional for partial overrides.
#[derive(Debug, Default)]
struct RawConfig {
    theme: Option<String>,
    brain: Option<RawBrainConfig>,
}

#[derive(Debug, Default)]
struct RawBrainConfig {
    enabled: Option<bool>,
    endpoint: Option<String>,
    model: Option<String>,
    auto_mode: Option<bool>,
    timeout_ms: Option<u64>,
    max_context_tokens: Option<u32>,
    few_shot_count: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSource {
    User,
    Project,
}

impl Config {
    /// Load configuration from global and project config files.
    pub fn load() -> Self {
        Self::load_from(
            &PathEnvironment::current(),
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
        .unwrap_or_default()
    }

    pub fn load_from(env: &PathEnvironment, cwd: &Path) -> Result<Self, PathError> {
        let paths = CodingBrainPaths::resolve(env)?;
        let mut config = Config::default();

        // Layer 1: User config.
        if let Some(raw) = parse_config_file(&paths.config_file().to_path_buf()) {
            config.apply_from(raw, ConfigSource::User);
        }

        // Layer 2: Project config.
        let project_path = paths.project_config(cwd);
        if let Some(raw) = parse_config_file(&project_path) {
            for warning in config.apply_from(raw, ConfigSource::Project) {
                eprintln!(
                    "Warning: {}:{}: {}",
                    project_path.display(),
                    warning.line,
                    warning.message
                );
            }
        }

        Ok(config)
    }

    /// Apply a raw config layer on top, overriding only set fields.
    #[cfg(test)]
    fn apply(&mut self, raw: RawConfig) {
        self.apply_from(raw, ConfigSource::User);
    }

    fn apply_from(&mut self, raw: RawConfig, source: ConfigSource) -> Vec<ConfigWarning> {
        let mut warnings = Vec::new();
        if let Some(theme) = raw.theme {
            self.theme = Some(theme);
        }
        if let Some(raw_brain) = raw.brain {
            let legacy_mode_configured =
                raw_brain.enabled.is_some() || raw_brain.auto_mode.is_some();
            let brain = self.brain.get_or_insert_with(BrainConfig::default);
            brain.legacy_mode_configured |= legacy_mode_configured;
            if let Some(value) = raw_brain.enabled {
                brain.enabled = value;
            }
            if let Some(value) = raw_brain.endpoint {
                if source == ConfigSource::User {
                    brain.endpoint = value;
                } else {
                    warnings.push(ConfigWarning {
                        line: 0,
                        message:
                            "project configuration cannot select brain.endpoint; value ignored"
                                .into(),
                    });
                }
            }
            if let Some(value) = raw_brain.model {
                brain.model = value;
            }
            if let Some(value) = raw_brain.auto_mode {
                brain.auto_mode = value;
            }
            if let Some(value) = raw_brain.timeout_ms {
                brain.timeout_ms = value;
            }
            if let Some(value) = raw_brain.max_context_tokens {
                brain.max_context_tokens = value;
            }
            if let Some(value) = raw_brain.few_shot_count {
                brain.few_shot_count = value;
            }
        }
        warnings
    }

    /// Show resolved config and file locations.
    pub fn print_resolved(&self) {
        println!("Resolved configuration:");
        println!();

        if let Some(p) = Self::global_path() {
            println!(
                "  User config: {}{}",
                p.display(),
                if p.exists() { "" } else { " (not found)" }
            );
        }

        let project_path = PathBuf::from(".coding-brain.toml");
        if project_path.exists() {
            println!("  Project config: {}", project_path.display());
        } else {
            println!("  Project config: .coding-brain.toml (not found)");
        }

        println!();
        println!("  theme: {}", self.theme.as_deref().unwrap_or("auto"));
        if let Some(brain) = &self.brain {
            println!();
            println!("  [brain]");
            println!("  endpoint: {}", brain.endpoint);
            println!("  model:    {}", brain.model);
        }
    }

    /// Print an annotated default config template to stdout.
    pub fn print_template() {
        print!("{}", Self::template_string());
    }

    /// Return the config template as a string.
    pub fn template_string() -> &'static str {
        r#"# Coding Brain configuration
# Place this file at:
#   Project: .coding-brain.toml (in your project root)
#   User:    ~/.config/coding-brain/config.toml
#
# Priority: CLI flags > project config > user config > defaults
# Project config cannot override brain.endpoint.
# Only set values you want to override — unset keys use defaults.

# TUI color theme: dark, light, or none. Omit for automatic detection.
# theme = "dark"

# ── Brain (Local LLM) ──────────────────────────────────────────────

# [brain]
# endpoint = "http://localhost:11434/api/generate"
# model = "gemma4:e4b"
# timeout_ms = 5000
# max_context_tokens = 4000
# few_shot_count = 5
"#
    }
}

fn global_config_path() -> Option<PathBuf> {
    CodingBrainPaths::resolve(&PathEnvironment::current())
        .ok()
        .map(|paths| paths.config_file().to_path_buf())
}

impl Config {
    /// Path to the global config file (for validation and diagnostics).
    pub fn global_path() -> Option<PathBuf> {
        global_config_path()
    }
}

/// Minimal TOML parser — avoids adding a toml crate dependency.
/// Supports: key = value pairs, [sections], # comments, strings, numbers, booleans, arrays.
fn parse_config_file(path: &PathBuf) -> Option<RawConfig> {
    let content = fs::read_to_string(path).ok()?;
    let mut raw = RawConfig::default();
    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Section headers
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Key = value
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        // Strip inline comments
        let value = value.split('#').next().unwrap_or(value).trim();

        match (section.as_str(), key) {
            ("" | "defaults", "theme") => raw.theme = Some(unquote(value)),
            ("brain", _) => {
                let brain = raw.brain.get_or_insert_with(RawBrainConfig::default);
                match key {
                    "enabled" => {
                        brain.enabled = parse_bool(value);
                    }
                    "endpoint" => brain.endpoint = Some(unquote(value)),
                    "model" => brain.model = Some(unquote(value)),
                    "auto" => {
                        brain.auto_mode = parse_bool(value);
                    }
                    "timeout_ms" => {
                        brain.timeout_ms = value.parse().ok();
                    }
                    "max_context_tokens" => {
                        brain.max_context_tokens = value.parse().ok();
                    }
                    "few_shot_count" => {
                        brain.few_shot_count = value.parse().ok();
                    }
                    _ => {}
                }
            }
            _ => {} // Ignore unknown keys
        }
    }

    Some(raw)
}

// ────────────────────────────────────────────────────────────────────────────
// Config validation
// ────────────────────────────────────────────────────────────────────────────

/// A warning or error from config validation.
pub struct ConfigWarning {
    pub line: usize,
    pub message: String,
}

/// Known sections and their valid keys.
fn known_keys(section: &str) -> Option<&'static [&'static str]> {
    match section {
        "" | "defaults" => Some(&["theme"]),
        "brain" => Some(&[
            "enabled",
            "endpoint",
            "model",
            "auto",
            "timeout_ms",
            "max_context_tokens",
            "few_shot_count",
        ]),
        _ => None,
    }
}

fn removed_section_message(section: &str) -> Option<&'static str> {
    match section.split('.').next().unwrap_or(section) {
        "webhook" | "budget" | "context" | "orchestrate" | "health" | "lifecycle" | "models"
        | "rules" => Some("is no longer supported by Coding Brain"),
        "relay" | "hive" | "idle" | "agents" => Some(
            "is no longer supported by brain-only codexctl; use Beads or an external worker for durable coordination",
        ),
        _ => None,
    }
}

fn removed_key_message(section: &str, key: &str) -> Option<&'static str> {
    match (section, key) {
        (
            "" | "defaults",
            "interval"
            | "notify"
            | "debug"
            | "grouped"
            | "sort"
            | "budget"
            | "kill_on_budget"
            | "context_warn"
            | "context_warn_threshold"
            | "file_conflicts"
            | "auto_deny_file_conflicts",
        ) => Some("this dashboard or session-management setting is no longer supported"),
        ("brain", "test_runners") => {
            Some("legacy heuristic test-failure attribution was removed; delete this setting")
        }
        (
            "brain",
            "terminal_auto_approve_fallback"
            | "max_sessions"
            | "orchestrate"
            | "orchestrate_interval"
            | "orchestrate_interval_secs",
        ) => Some("this Brain session-management setting is no longer supported"),
        _ => None,
    }
}

/// Validate a config file and return warnings for unknown keys/sections.
pub fn validate_config_file(path: &PathBuf) -> (Vec<ConfigWarning>, bool) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return (
                vec![ConfigWarning {
                    line: 0,
                    message: format!("cannot read file: {e}"),
                }],
                true,
            );
        }
    };

    let mut warnings = Vec::new();
    let mut section = String::new();
    let mut has_errors = false;

    for (line_num, line) in content.lines().enumerate() {
        let line_1 = line_num + 1;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Section header
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            if let Some(message) = removed_section_message(&section) {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("[{section}] {message}"),
                });
                continue;
            }

            // Hook sections remain dynamic; all other sections are explicit.
            let base = section.split('.').next().unwrap_or(&section);
            let is_known = known_keys(&section).is_some() || base == "hooks";
            if !is_known {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("unknown section [{section}]"),
                });
            }
            continue;
        }

        // Key = value
        let Some((key, _value)) = line.split_once('=') else {
            warnings.push(ConfigWarning {
                line: line_1,
                message: format!("malformed line (expected key = value): {line}"),
            });
            has_errors = true;
            continue;
        };
        let key = key.trim();

        if removed_section_message(&section).is_some() {
            continue;
        }

        if let Some(message) = removed_key_message(&section, key) {
            warnings.push(ConfigWarning {
                line: line_1,
                message: message.into(),
            });
            continue;
        }

        // Skip dynamic hook sections.
        let base = section.split('.').next().unwrap_or(&section);
        if base == "hooks" {
            continue;
        }

        // Check if key is known for this section
        if let Some(valid_keys) = known_keys(&section) {
            if !valid_keys.contains(&key) {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("unknown key \"{key}\" in [{section}]"),
                });
            }
        }
    }

    (warnings, has_errors)
}

fn legacy_config_warnings_for_paths(paths: &[PathBuf]) -> Vec<(PathBuf, ConfigWarning)> {
    paths
        .iter()
        .filter(|path| path.exists())
        .flat_map(|path| {
            let (warnings, _) = validate_config_file(path);
            warnings
                .into_iter()
                .filter(|warning| warning.message.contains("no longer supported"))
                .map(|warning| (path.clone(), warning))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn legacy_config_warnings() -> Vec<(PathBuf, ConfigWarning)> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    paths.push(PathBuf::from(".coding-brain.toml"));
    legacy_config_warnings_for_paths(&paths)
}

/// Load hooks from global and project config files.
pub fn load_hooks() -> crate::hooks::HookRegistry {
    let mut registry = crate::hooks::HookRegistry::new();

    if let Some(global) = global_config_path() {
        parse_hooks_from_file(&global, &mut registry);
    }
    parse_hooks_from_file(&PathBuf::from(".coding-brain.toml"), &mut registry);

    registry
}

fn parse_hooks_from_file(path: &PathBuf, registry: &mut crate::hooks::HookRegistry) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Only process hooks sections
        if !section.starts_with("hooks.") {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let value = value.split('#').next().unwrap_or(value).trim();

        if key == "run" {
            if let Some(event) = crate::hooks::HookEvent::from_section(&section) {
                registry.add(event, unquote(value));
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').trim_matches('\'').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runners_is_explicitly_unsupported() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[brain]\ntest_runners = [\"cargo test\"]").unwrap();
        file.flush().unwrap();

        let (warnings, has_errors) = validate_config_file(&file.path().to_path_buf());

        assert!(!has_errors);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].message,
            "legacy heuristic test-failure attribution was removed; delete this setting"
        );
        assert!(!Config::template_string().contains("test_runners"));
    }

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("yes"), None);
    }

    #[test]
    fn test_unquote() {
        assert_eq!(unquote("\"hello\""), "hello");
        assert_eq!(unquote("'hello'"), "hello");
        assert_eq!(unquote("hello"), "hello");
    }

    #[test]
    fn test_parse_config_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
# User Coding Brain config
[defaults]
theme = "dark"

[brain]
enabled = true
model = "local-model"
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.theme.as_deref(), Some("dark"));
        let brain = raw.brain.expect("brain config");
        assert_eq!(brain.enabled, Some(true));
        assert_eq!(brain.model.as_deref(), Some("local-model"));
    }

    #[test]
    fn test_config_layering() {
        let mut config = Config::default();
        assert!(config.theme.is_none());
        assert!(config.brain.is_none());

        // Apply user config.
        config.apply(RawConfig {
            theme: Some("dark".into()),
            brain: Some(RawBrainConfig {
                model: Some("user-model".into()),
                ..RawBrainConfig::default()
            }),
        });

        // Apply another layer that overrides only fields it sets.
        config.apply(RawConfig {
            brain: Some(RawBrainConfig {
                timeout_ms: Some(7500),
                ..RawBrainConfig::default()
            }),
            ..RawConfig::default()
        });
        assert_eq!(config.theme.as_deref(), Some("dark"));
        let brain = config.brain.unwrap();
        assert!(!brain.legacy_mode_configured);
        assert_eq!(brain.model, "user-model");
        assert_eq!(brain.timeout_ms, 7500);
    }

    #[test]
    fn test_parse_brain_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[brain]
enabled = true
endpoint = "http://localhost:8080/v1/chat"
model = "llama3:8b"
auto = true
timeout_ms = 3000
max_context_tokens = 8000
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        let mut config = Config::default();
        config.apply(raw);
        let brain = config.brain.expect("brain config should be parsed");
        assert!(brain.legacy_mode_configured);
        assert!(brain.enabled);
        assert_eq!(brain.endpoint, "http://localhost:8080/v1/chat");
        assert_eq!(brain.model, "llama3:8b");
        assert!(brain.auto_mode);
        assert_eq!(brain.timeout_ms, 3000);
        assert_eq!(brain.max_context_tokens, 8000);
    }

    #[test]
    fn legacy_mode_presence_survives_later_config_layers() {
        let mut config = Config::default();
        config.apply(RawConfig {
            brain: Some(RawBrainConfig {
                enabled: Some(true),
                ..RawBrainConfig::default()
            }),
            ..RawConfig::default()
        });
        config.apply(RawConfig {
            brain: Some(RawBrainConfig {
                model: Some("project-model".into()),
                ..RawBrainConfig::default()
            }),
            ..RawConfig::default()
        });

        assert!(config.brain.unwrap().legacy_mode_configured);
    }

    #[test]
    fn terminal_fallback_is_explicitly_unsupported() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[brain]\nterminal_auto_approve_fallback = true").unwrap();
        file.flush().unwrap();

        let (warnings, has_errors) = validate_config_file(&file.path().to_path_buf());
        assert!(!has_errors);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("no longer supported"));
        assert!(!Config::template_string().contains("terminal_auto_approve_fallback"));
    }

    #[test]
    fn brain_config_layers_merge_field_by_field() {
        use std::io::Write;

        let mut global = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            global,
            r#"
[brain]
endpoint = "http://localhost:8080/v1/chat"
model = "local-model"
timeout_ms = 3210
max_context_tokens = 7654
few_shot_count = 7
"#
        )
        .unwrap();
        global.flush().unwrap();
        let mut project = tempfile::NamedTempFile::new().unwrap();
        writeln!(project, "[brain]\nmodel = \"project-model\"").unwrap();
        project.flush().unwrap();

        let mut config = Config::default();
        config.apply(parse_config_file(&global.path().to_path_buf()).unwrap());
        config.apply(parse_config_file(&project.path().to_path_buf()).unwrap());

        let brain = config.brain.unwrap();
        assert_eq!(brain.endpoint, "http://localhost:8080/v1/chat");
        assert_eq!(brain.model, "project-model");
        assert_eq!(brain.timeout_ms, 3210);
        assert_eq!(brain.max_context_tokens, 7654);
        assert_eq!(brain.few_shot_count, 7);
    }

    #[test]
    fn test_no_brain_config_returns_none() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[defaults]\ntheme = \"dark\"").unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert!(raw.brain.is_none());
    }

    #[test]
    fn dashboard_and_management_config_are_explicitly_unsupported() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
interval = 2000
notify = true
budget = 10

[webhook]
url = "https://example.invalid"

[brain]
orchestrate = true
max_sessions = 4
terminal_auto_approve_fallback = true

[lifecycle]
auto_restart = true
"#,
        )
        .unwrap();
        file.flush().unwrap();
        let path = file.path().to_path_buf();

        let (warnings, has_errors) = validate_config_file(&path);
        assert!(!has_errors);
        assert!(!warnings.is_empty());
        assert!(
            warnings
                .iter()
                .all(|warning| warning.message.contains("no longer supported"))
        );
    }

    #[test]
    fn project_endpoint_is_ignored_with_a_source_warning() {
        use std::io::Write;

        let mut user = tempfile::NamedTempFile::new().unwrap();
        writeln!(user, "[brain]\nendpoint = \"http://127.0.0.1:1\"").unwrap();
        let mut project = tempfile::NamedTempFile::new().unwrap();
        writeln!(project, "[brain]\nendpoint = \"https://remote.invalid\"").unwrap();

        let mut config = Config::default();
        config.apply_from(
            parse_config_file(&user.path().to_path_buf()).unwrap(),
            ConfigSource::User,
        );
        let warnings = config.apply_from(
            parse_config_file(&project.path().to_path_buf()).unwrap(),
            ConfigSource::Project,
        );
        let brain = config.brain.unwrap();
        assert_eq!(brain.endpoint, "http://127.0.0.1:1");
        assert!(warnings.iter().any(|warning| {
            warning
                .message
                .contains("project configuration cannot select brain.endpoint")
        }));
    }

    #[test]
    fn project_config_cannot_redirect_endpoint() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let config_home = fixture.path().join("config");
        let state_home = fixture.path().join("state");
        let cwd = fixture.path().join("project");
        std::fs::create_dir_all(config_home.join("coding-brain")).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(
            config_home.join("coding-brain/config.toml"),
            "[brain]\nendpoint = \"http://127.0.0.1:11434\"\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".coding-brain.toml"),
            "[brain]\nendpoint = \"https://remote.invalid\"\n",
        )
        .unwrap();

        let environment = PathEnvironment::new(Some(config_home), Some(state_home), Some(home));
        let loaded = Config::load_from(&environment, &cwd).unwrap();

        assert_eq!(loaded.brain.unwrap().endpoint, "http://127.0.0.1:11434");
    }

    #[test]
    fn old_config_and_state_are_ignored_and_untouched() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let cwd = fixture.path().join("project");
        std::fs::create_dir_all(home.join(".config/codexctl")).unwrap();
        std::fs::create_dir_all(home.join(".codexctl/brain")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let old_config = home.join(".config/codexctl/config.toml");
        let old_state = home.join(".codexctl/brain/decisions.jsonl");
        std::fs::write(&old_config, "[brain]\nenabled = true\n").unwrap();
        std::fs::write(&old_state, "legacy\n").unwrap();

        let environment = PathEnvironment::new(None, None, Some(home));
        let loaded = Config::load_from(&environment, &cwd).unwrap();

        assert!(loaded.brain.is_none());
        assert_eq!(
            std::fs::read_to_string(old_config).unwrap(),
            "[brain]\nenabled = true\n"
        );
        assert_eq!(std::fs::read_to_string(old_state).unwrap(), "legacy\n");
    }
}
