//! `codexctl doctor` — install + runtime health check (#326).
//!
//! Top-down checklist that answers "is everything wired up?" in one
//! command. Replaces what was scattered across:
//!
//! * `codexctl --doctor` (terminal compat only)
//! * `codexctl init --check` (onboarding-marker drift only)
//! * scattered "is X reachable?" probes the user had to chain manually
//!
//! Each check returns a `Check` with status + a fix hint. The renderer
//! shows ✓ / ⚠ / ✗ icons and a one-line message; advisories are
//! non-fatal so optional brain configuration does not make doctor fail.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// Wired up and working.
    Pass,
    /// Wired up partially; works but suboptimal.
    Advisory,
    /// Broken in a way that affects functionality.
    Fail,
    /// Not applicable to this install path / feature set.
    Skipped,
}

impl CheckStatus {
    fn icon(self) -> &'static str {
        match self {
            CheckStatus::Pass => "\u{2713}",     // ✓
            CheckStatus::Advisory => "\u{26a0}", // ⚠
            CheckStatus::Fail => "\u{2717}",     // ✗
            CheckStatus::Skipped => "\u{2014}",  // —
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    /// Short name, fits on one line.
    pub name: String,
    pub status: CheckStatus,
    /// One-line summary of the result.
    pub message: String,
    /// Hint for fixing a Fail or following an Advisory. None when status
    /// is Pass.
    pub fix_hint: Option<String>,
}

/// Run every health check, in display order. Order is meaningful: PATH
/// first because everything else depends on the binary being callable;
/// session discovery last because it's the integration that ties it all
/// together.
pub fn run_all_checks() -> Vec<Check> {
    vec![
        check_binary_on_path(),
        check_codex_hooks(),
        check_brain_endpoint(),
        check_session_discovery(),
        check_terminal_integration(),
    ]
}

/// Human-readable renderer. Lays out one row per check, two-space
/// indent, fixed-width name column so messages align.
pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    out.push_str("codexctl doctor\n");
    out.push_str("=================\n\n");
    let max_name = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    for c in checks {
        out.push_str(&format!(
            "  {} {:<width$}  {}\n",
            c.status.icon(),
            c.name,
            c.message,
            width = max_name
        ));
        if let Some(hint) = &c.fix_hint {
            out.push_str(&format!("      \u{2192} {hint}\n"));
        }
    }
    out.push('\n');
    let (pass, advisory, fail) = counts(checks);
    out.push_str(&format!(
        "{pass} passed, {advisory} advisory, {fail} failed.\n"
    ));
    out
}

pub fn render_checks_json(checks: &[Check]) -> io::Result<String> {
    serde_json::to_string_pretty(checks).map_err(io::Error::other)
}

/// Exit code: 0 when all Pass + Advisory + Skipped, non-zero when any
/// Fail. Matches the "exit non-zero on any actual problem" rule the
/// epic spec called for.
pub fn exit_code(checks: &[Check]) -> i32 {
    if checks.iter().any(|c| c.status == CheckStatus::Fail) {
        1
    } else {
        0
    }
}

fn counts(checks: &[Check]) -> (usize, usize, usize) {
    let mut pass = 0;
    let mut advisory = 0;
    let mut fail = 0;
    for c in checks {
        match c.status {
            CheckStatus::Pass => pass += 1,
            CheckStatus::Advisory => advisory += 1,
            CheckStatus::Fail => fail += 1,
            CheckStatus::Skipped => {}
        }
    }
    (pass, advisory, fail)
}

// ─── individual checks ──────────────────────────────────────────────────────

fn check_binary_on_path() -> Check {
    // Compare the running binary against what `which codexctl` resolves
    // to. Mismatches mean the user is running one binary while their
    // hooks resolve a different one (typical after `cargo install` on top
    // of a previous `brew install`).
    let running = std::env::current_exe().ok();
    let on_path = std::process::Command::new("which")
        .arg("codexctl")
        .output()
        .ok()
        .and_then(|o| {
            if !o.status.success() {
                return None;
            }
            String::from_utf8(o.stdout)
                .ok()
                .map(|s| PathBuf::from(s.trim()))
        });
    match (running, on_path) {
        (Some(r), Some(p)) if r.canonicalize().ok() == p.canonicalize().ok() => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Pass,
            message: p.display().to_string(),
            fix_hint: None,
        },
        (Some(r), Some(p)) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: format!("running {}, PATH resolves {}", r.display(), p.display()),
            fix_hint: Some(
                "Two installs detected. Hooks call `codexctl` by name — \
                 verify they use the version you expect."
                    .into(),
            ),
        },
        (Some(r), None) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Fail,
            message: format!("{} not on PATH", r.display()),
            fix_hint: Some(
                "Add the install dir to PATH so hooks can find `codexctl` by name.".into(),
            ),
        },
        _ => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: "could not resolve running binary".into(),
            fix_hint: None,
        },
    }
}

fn check_codex_hooks() -> Check {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    check_codex_hooks_at(home.as_deref(), &cwd)
}

fn check_codex_hooks_at(home: Option<&std::path::Path>, cwd: &std::path::Path) -> Check {
    let Some(home) = home else {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Fail,
            message: "HOME not set".into(),
            fix_hint: None,
        };
    };
    let discovery = crate::init::hooks::discover_permission_hooks_at(Some(home), cwd);
    if !discovery.configured() {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Fail,
            message: "managed PermissionRequest hook not found".into(),
            fix_hint: Some("Run `codexctl init` (or `codexctl init --plugin-only`).".into()),
        };
    }

    if discovery.duplicate_scopes() {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Advisory,
            message: "managed permission hook is installed in global and project scopes".into(),
            fix_hint: Some(
                "Keep the managed PermissionRequest hook in one scope, restart Codex, and verify it with `/hooks`."
                    .into(),
            ),
        };
    }

    let scope = if discovery.global.configured {
        (&discovery.global, "global")
    } else {
        (&discovery.project, "project")
    };
    if scope.0.stale {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Advisory,
            message: format!("{0} managed permission hook is outdated", scope.1),
            fix_hint: Some(
                "Run `codexctl init`, restart Codex, and verify the changed command with `/hooks`."
                    .into(),
            ),
        };
    }
    if scope.0.disabled {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Advisory,
            message: format!("{} managed permission hook is disabled", scope.1),
            fix_hint: Some("Enable and trust the hook through Codex `/hooks`.".into()),
        };
    }

    Check {
        name: "Codex hooks".into(),
        status: CheckStatus::Pass,
        message: format!("{} managed permission hook is current", scope.1),
        fix_hint: None,
    }
}

fn check_brain_endpoint() -> Check {
    let endpoint = crate::config::Config::load()
        .brain
        .map(|brain| brain.endpoint)
        .unwrap_or_else(|| "http://localhost:11434/api/generate".into());
    if !is_loopback_endpoint(&endpoint) {
        return check_brain_endpoint_url(&endpoint);
    }

    let curl = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "1", &endpoint])
        .output();
    match curl {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => Check {
            name: "brain endpoint".into(),
            status: CheckStatus::Pass,
            message: format!("local brain reachable at {endpoint}"),
            fix_hint: None,
        },
        _ => Check {
            name: "brain endpoint".into(),
            status: CheckStatus::Advisory,
            message: format!("local brain endpoint is not reachable at {endpoint}"),
            fix_hint: Some(
                "Brain is optional. To enable: `brew install ollama && ollama serve &` + `ollama pull gemma4:e4b`."
                    .into(),
            ),
        },
    }
}

fn endpoint_host(endpoint: &str) -> Option<&str> {
    let authority = endpoint.split_once("://")?.1.split('/').next()?;
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if let Some(bracketed) = authority.strip_prefix('[') {
        return bracketed.split_once(']').map(|(host, _)| host);
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

pub(crate) fn is_loopback_endpoint(endpoint: &str) -> bool {
    endpoint_host(endpoint).is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || matches!(host, "127.0.0.1" | "::1")
    })
}

fn check_brain_endpoint_url(endpoint: &str) -> Check {
    if is_loopback_endpoint(endpoint) {
        Check {
            name: "brain endpoint privacy".into(),
            status: CheckStatus::Pass,
            message: format!("{endpoint} is loopback-only"),
            fix_hint: None,
        }
    } else {
        Check {
            name: "brain endpoint privacy".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "{endpoint} is not loopback; transcript context may leave this machine"
            ),
            fix_hint: Some(
                "Use a loopback endpoint or confirm the remote endpoint's privacy policy.".into(),
            ),
        }
    }
}

fn check_session_discovery() -> Check {
    // Discovery never errors per se — it returns 0 sessions when nothing
    // matches. The signal we want is "the scanner runs and finds at
    // least one session." Zero sessions is normal if no Codex is
    // running; advise instead of fail.
    let sessions = codexctl_core::discovery::scan_sessions();
    if sessions.is_empty() {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Advisory,
            message: "0 sessions discovered (no Codex running?)".into(),
            fix_hint: Some(
                "Start a Codex session in another terminal (`codex`) and re-run `codexctl doctor`."
                    .into(),
            ),
        }
    } else {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Pass,
            message: format!("{} session(s) discovered", sessions.len()),
            fix_hint: None,
        }
    }
}

fn check_terminal_integration() -> Check {
    // Re-use the existing terminal doctor report. We collapse it to a
    // one-line summary (the detailed view is still available via the
    // legacy `--doctor` flag).
    let report = codexctl_core::terminals::doctor_report();
    if report.terminal == "Unknown" {
        return Check {
            name: "terminal integration".into(),
            status: CheckStatus::Advisory,
            message: "terminal not recognized".into(),
            fix_hint: Some(
                "Tab switching + input automation work in: Ghostty, Kitty, tmux, WezTerm, Warp, iTerm2, Terminal.app, Gnome Terminal, Windows Terminal."
                    .into(),
            ),
        };
    }
    let action_count = report.actions.len();
    Check {
        name: "terminal integration".into(),
        status: CheckStatus::Pass,
        message: format!(
            "{} on {} ({} actions supported)",
            report.terminal, report.platform, action_count
        ),
        fix_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_handles_empty_check_list() {
        let out = render_checks(&[]);
        assert!(out.contains("codexctl doctor"));
        assert!(out.contains("0 passed"));
    }

    #[test]
    fn exit_code_zero_when_all_pass() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        assert_eq!(exit_code(&checks), 0);
    }

    #[test]
    fn exit_code_zero_when_only_advisories() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Advisory,
            message: "not configured".into(),
            fix_hint: None,
        }];
        assert_eq!(exit_code(&checks), 0);
    }

    #[test]
    fn exit_code_nonzero_on_any_fail() {
        let checks = vec![
            Check {
                name: "a".into(),
                status: CheckStatus::Pass,
                message: "ok".into(),
                fix_hint: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "broken".into(),
                fix_hint: Some("fix it".into()),
            },
        ];
        assert_eq!(exit_code(&checks), 1);
    }

    #[test]
    fn counts_split_correctly() {
        let checks = vec![
            Check {
                name: "a".into(),
                status: CheckStatus::Pass,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "c".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "d".into(),
                status: CheckStatus::Fail,
                message: "".into(),
                fix_hint: None,
            },
            Check {
                name: "e".into(),
                status: CheckStatus::Skipped,
                message: "".into(),
                fix_hint: None,
            },
        ];
        assert_eq!(counts(&checks), (1, 2, 1));
    }

    #[test]
    fn render_includes_fix_hint_when_present() {
        let checks = vec![Check {
            name: "test".into(),
            status: CheckStatus::Fail,
            message: "broken".into(),
            fix_hint: Some("run this".into()),
        }];
        let out = render_checks(&checks);
        assert!(out.contains("run this"));
    }

    #[test]
    fn render_omits_hint_when_none() {
        let checks = vec![Check {
            name: "test".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        let out = render_checks(&checks);
        // No arrow line.
        assert!(!out.contains("\u{2192}"));
    }

    #[test]
    fn json_round_trips() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
        }];
        let json = render_checks_json(&checks).unwrap();
        let parsed: Vec<Check> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].status, CheckStatus::Pass);
    }

    #[test]
    fn non_loopback_brain_endpoint_is_advisory() {
        let check = check_brain_endpoint_url("https://brain.example.com/v1/chat/completions");
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(
            check
                .message
                .contains("transcript context may leave this machine")
        );
    }

    #[test]
    fn loopback_endpoint_detection_is_exact_and_case_insensitive() {
        assert!(is_loopback_endpoint("http://LOCALHOST:11434/api/generate"));
        assert!(is_loopback_endpoint("http://127.0.0.1:8080/v1/chat"));
        assert!(is_loopback_endpoint("http://[::1]:8080/v1/chat"));
        assert!(!is_loopback_endpoint(
            "http://localhost.example.com/v1/chat"
        ));
    }

    fn write_permission_hook(path: &std::path::Path, timeout: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "codexctl --permission-hook",
                    "timeout": timeout,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }] }
        });
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn duplicate_global_and_project_permission_hooks_are_advisory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_permission_hook(&home.join(".codex/hooks.json"), 30);
        write_permission_hook(&cwd.join(".codex/hooks.json"), 30);

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("global and project"));
        assert!(check.fix_hint.unwrap().contains("one scope"));
    }

    #[test]
    fn stale_permission_hook_is_configured_but_advisory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_permission_hook(&home.join(".codex/hooks.json"), 5);

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("outdated"));
        assert!(check.fix_hint.unwrap().contains("codexctl init"));
    }

    #[test]
    fn conservative_only_ancestor_does_not_report_active_hook() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let jj_root = temp.path().join("project");
        let git_root = jj_root.join("nested");
        let cwd = git_root.join("work");
        std::fs::create_dir_all(jj_root.join(".jj")).unwrap();
        std::fs::create_dir_all(git_root.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        write_permission_hook(&jj_root.join(".codex/hooks.json"), 30);

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("not found"));
    }
}
