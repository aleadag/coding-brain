//! Environment detection for the `init` wizard.
//!
//! Each phase has a corresponding probe here that answers "what's the current
//! state?" with no side effects. The wizard uses these to decide what to ask
//! and what to skip; `init --check` uses them to diff against the recorded
//! marker; the install/remove paths use them to be idempotent.
//!
//! All probes are tiny and synchronous — file checks, `curl --max-time 1`,
//! reading a TOML — so the whole detection pass takes well under a second
//! even on a cold machine.

use std::path::PathBuf;
use std::process::Command;

use coding_brain_core::provider::AgentProvider;

use super::hooks;
use super::provider_hooks::{self, HookScope};

/// The shape every phase's probe returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseStatus {
    /// Phase has not been configured. The wizard will offer to set it up.
    NotInstalled,
    /// Phase is currently configured. `details` is one human line ("ollama at
    /// http://localhost:11434", "$50/wk", "2 roles bound") rendered in
    /// `--check`.
    Installed { details: String },
    /// Phase was recorded as installed in the marker but no longer detected
    /// in the environment. The wizard treats this as a re-prompt case.
    ///
    /// Currently no probe synthesizes this directly; `init --check` derives
    /// it by comparing detection against the recorded marker. Kept on the
    /// enum so phases can return it once we add drift-aware detection.
    #[allow(dead_code)]
    Drift { reason: String },
    /// User opted out of this phase last time and we should respect that
    /// until `--reset` is run.
    Skipped,
}

impl PhaseStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::Installed { .. } => "installed",
            Self::Drift { .. } => "drift",
            Self::Skipped => "skipped",
        }
    }

    pub fn details(&self) -> Option<&str> {
        match self {
            Self::Installed { details } => Some(details.as_str()),
            Self::Drift { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

// ---------------- Brain (local LLM) -----------------------------------------

/// Known local-LLM endpoints worth probing. First hit wins.
const BRAIN_PROBES: &[(&str, &str, &str)] = &[
    ("ollama", "http://localhost:11434", "/api/tags"),
    ("llama.cpp", "http://localhost:8080", "/v1/models"),
    ("lm-studio", "http://localhost:1234", "/v1/models"),
    ("vllm", "http://localhost:8000", "/v1/models"),
];

/// Probe each candidate endpoint with a 1-second `curl`. We do not require an
/// LLM model to be loaded — only that the endpoint answers — because the user
/// might be about to pull one.
pub fn detect_brain() -> PhaseStatus {
    for (name, base, path) in BRAIN_PROBES {
        if probe_http(&format!("{base}{path}")) {
            return PhaseStatus::Installed {
                details: format!("{name} at {base}"),
            };
        }
    }
    PhaseStatus::NotInstalled
}

fn probe_http(url: &str) -> bool {
    Command::new("curl")
        .args(["-s", "-o", "/dev/null", "--max-time", "1", url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------- Codex hooks ------------------------------------------------

/// Plugin is "installed" when the managed permission hook is configured in
/// either the global or an applicable project scope. Stale and disabled
/// handlers still count as configured; `doctor` reports those diagnostics.
pub fn detect_plugin() -> PhaseStatus {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    detect_plugin_at(home.as_deref(), &cwd)
}

pub fn detect_provider_hooks(provider: AgentProvider) -> PhaseStatus {
    match provider_hooks::stage_provider_hooks(&[provider], HookScope::Global) {
        Ok(plans)
            if plans.iter().all(|plan| {
                plan.edits.is_empty() && plan.preserved_modified_entries.is_empty()
            }) =>
        {
            PhaseStatus::Installed {
                details: format!("managed {} hooks", provider.label()),
            }
        }
        Ok(plans)
            if plans
                .iter()
                .any(|plan| !plan.preserved_modified_entries.is_empty()) =>
        {
            PhaseStatus::Drift {
                reason: format!("user-modified {} hook entries preserved", provider.label()),
            }
        }
        Ok(_) => PhaseStatus::NotInstalled,
        Err(error) => PhaseStatus::Drift {
            reason: error.to_string(),
        },
    }
}

pub fn detect_provider_executables() -> Vec<AgentProvider> {
    detect_provider_executables_with(executable_on_path)
}

fn detect_provider_executables_with(mut available: impl FnMut(&str) -> bool) -> Vec<AgentProvider> {
    [
        (AgentProvider::Codex, "codex"),
        (AgentProvider::Claude, "claude"),
        (AgentProvider::Antigravity, "agy"),
    ]
    .into_iter()
    .filter_map(|(provider, executable)| available(executable).then_some(provider))
    .collect()
}

fn executable_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| executable_file(&directory.join(name)))
}

#[cfg(unix)]
fn executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable_file(path: &std::path::Path) -> bool {
    path.is_file() || path.with_extension("exe").is_file()
}

fn detect_plugin_at(home: Option<&std::path::Path>, cwd: &std::path::Path) -> PhaseStatus {
    let discovery = hooks::discover_permission_hooks_at(home, cwd);
    if !discovery.configured() {
        return PhaseStatus::NotInstalled;
    }
    let scope = if discovery.duplicate_scopes() {
        "global and project scopes"
    } else if discovery.global.configured {
        "global scope"
    } else if discovery.project.configured {
        "project scope"
    } else {
        unreachable!("configured state checked above")
    };
    PhaseStatus::Installed {
        details: format!("managed hooks in {scope}"),
    }
}

// ---------------- Skills (curated list) -------------------------------------

/// Skills installation is owned by Codex itself (`/plugin install`),
/// not by codexctl. We treat the phase as "installed" only when the user
/// recorded acknowledging the suggestions, via the marker — so detection
/// here always returns NotInstalled and the wizard relies on the marker for
/// idempotency.
pub fn detect_skills() -> PhaseStatus {
    PhaseStatus::NotInstalled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_status_labels_are_stable() {
        assert_eq!(PhaseStatus::NotInstalled.label(), "not_installed");
        assert_eq!(
            PhaseStatus::Installed {
                details: "x".into()
            }
            .label(),
            "installed"
        );
        assert_eq!(PhaseStatus::Drift { reason: "y".into() }.label(), "drift");
        assert_eq!(PhaseStatus::Skipped.label(), "skipped");
    }

    #[test]
    fn plugin_detection_accepts_project_permission_hook() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::create_dir_all(cwd.join(".codex")).unwrap();
        std::fs::write(
            cwd.join(".codex/hooks.json"),
            r#"{"hooks":{"PermissionRequest":[{"matcher":"*","hooks":[{"type":"command","command":"codexctl --permission-hook","timeout":30,"statusMessage":"Brain reviewing permission…"}]}]}}"#,
        )
        .unwrap();

        let status = detect_plugin_at(Some(&home), &cwd);

        assert!(matches!(status, PhaseStatus::Installed { .. }));
        assert!(status.details().unwrap().contains("project"));
    }

    #[test]
    fn plugin_detection_requires_managed_permission_hook() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"matcher":"*","hooks":[{"type":"command","command":"codexctl --json 2>/dev/null || true","timeout":5}]}]}}"#,
        )
        .unwrap();

        assert_eq!(
            detect_plugin_at(Some(&home), &cwd),
            PhaseStatus::NotInstalled
        );
    }

    #[test]
    fn plugin_detection_ignores_conservative_only_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let jj_root = temp.path().join("project");
        let git_root = jj_root.join("nested");
        let cwd = git_root.join("work");
        std::fs::create_dir_all(jj_root.join(".jj")).unwrap();
        std::fs::create_dir_all(git_root.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(jj_root.join(".codex")).unwrap();
        std::fs::write(
            jj_root.join(".codex/hooks.json"),
            r#"{"hooks":{"PermissionRequest":[{"matcher":"*","hooks":[{"type":"command","command":"codexctl --permission-hook","timeout":30,"statusMessage":"Brain reviewing permission…"}]}]}}"#,
        )
        .unwrap();

        assert_eq!(
            detect_plugin_at(Some(&home), &cwd),
            PhaseStatus::NotInstalled
        );
    }

    #[test]
    fn provider_executable_detection_uses_provider_command_names_in_stable_order() {
        let detected = detect_provider_executables_with(|name| matches!(name, "claude" | "agy"));
        assert_eq!(
            detected,
            vec![AgentProvider::Claude, AgentProvider::Antigravity]
        );
    }
}
