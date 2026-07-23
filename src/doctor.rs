//! `coding-brain doctor` — install + runtime health check.
//!
//! Top-down checklist that answers "is everything wired up?" in one
//! command. Replaces what was scattered across:
//!
//! * `coding-brain doctor` (complete install and runtime health)
//! * `coding-brain init --check` (onboarding-marker drift only)
//! * scattered "is X reachable?" probes the user had to chain manually
//!
//! Each check returns a `Check` with status + a fix hint. The renderer
//! shows ✓ / ⚠ / ✗ icons and a one-line message; advisories are
//! non-fatal so optional brain configuration does not make doctor fail.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use coding_brain_core::brain_activity::{ActivityKind, ActivityState};
use coding_brain_core::lifecycle::{LifecycleStore, StoreCondition, coding_brain_state_root};

use crate::brain::activity::ActivityStore;

use coding_brain_core::provider::AgentProvider;

use crate::init::provider_hooks::ProviderHookInspection;

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
    let mut checks = Vec::new();
    if let Some(check) =
        provider_hook_recovery_check(crate::init::provider_hooks::recover_hook_transaction())
    {
        checks.push(check);
    }
    checks.extend([check_binary_on_path()]);
    checks.extend(check_provider_setups());
    checks.extend([
        check_codex_hook_trust(),
        check_lifecycle_state(),
        check_outcome_telemetry(),
        check_project_identity(),
        check_brain_endpoint(),
        check_session_discovery(),
    ]);
    checks.extend(check_terminal_capabilities());
    checks
}

fn provider_hook_recovery_check(
    result: io::Result<crate::init::provider_hooks::RecoveryReport>,
) -> Option<Check> {
    match result {
        Ok(report) if report.concurrent_paths.is_empty() => None,
        Ok(report) => Some(Check {
            name: "Provider hook recovery".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "preserved {} concurrently modified provider configuration(s)",
                report.concurrent_paths.len()
            ),
            fix_hint: Some("Review the preserved provider hook configuration files.".into()),
        }),
        Err(_) => Some(Check {
            name: "Provider hook recovery".into(),
            status: CheckStatus::Fail,
            message: "pending provider hook transaction could not be recovered".into(),
            fix_hint: Some(
                "Inspect the provider configurations and hook transaction journal before retrying."
                    .into(),
            ),
        }),
    }
}

/// Human-readable renderer. Lays out one row per check, two-space
/// indent, fixed-width name column so messages align.
pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    out.push_str("coding-brain doctor\n");
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderSetupState {
    Current,
    Degraded,
    Stale,
    Unavailable,
    Skipped,
}

#[derive(Debug, Clone, Copy)]
struct ProviderSetupEvidence {
    recorded: bool,
    executable_available: bool,
    hooks: ProviderHookInspection,
}

fn check_provider_setups() -> Vec<Check> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let marker = crate::init::marker::load(&crate::init::marker::default_path())
        .ok()
        .flatten();
    let executables = crate::init::state::detect_provider_executables();
    let Some(home) = home.as_deref() else {
        return [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ]
        .into_iter()
        .map(|provider| {
            check_provider_setup(
                provider,
                ProviderSetupEvidence {
                    recorded: false,
                    executable_available: executables.contains(&provider),
                    hooks: ProviderHookInspection::Invalid,
                },
            )
        })
        .collect();
    };
    check_provider_setups_at(home, &cwd, marker.as_ref(), &executables)
}

fn check_provider_setups_at(
    home: &Path,
    cwd: &Path,
    marker: Option<&crate::init::marker::OnboardingMarker>,
    executables: &[AgentProvider],
) -> Vec<Check> {
    let recorded = marker
        .map(crate::init::marker::OnboardingMarker::upgrade_providers)
        .unwrap_or_default();

    [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ]
    .into_iter()
    .map(|provider| {
        let hooks = crate::init::provider_hooks::inspect_provider_hooks_at(provider, home, cwd);
        check_provider_setup(
            provider,
            ProviderSetupEvidence {
                recorded: recorded.contains(&provider),
                executable_available: executables.contains(&provider),
                hooks,
            },
        )
    })
    .collect()
}

fn check_provider_setup(provider: AgentProvider, evidence: ProviderSetupEvidence) -> Check {
    let (state, message) = match evidence.hooks {
        ProviderHookInspection::Invalid => (
            ProviderSetupState::Stale,
            "invalid or unsafe managed definitions".to_string(),
        ),
        ProviderHookInspection::Stale => (
            ProviderSetupState::Stale,
            "managed definition stale".to_string(),
        ),
        ProviderHookInspection::Duplicate if !evidence.executable_available => (
            ProviderSetupState::Unavailable,
            "unavailable: provider executable is absent; managed definitions are duplicated"
                .to_string(),
        ),
        ProviderHookInspection::Duplicate => (
            ProviderSetupState::Degraded,
            "degraded: managed definitions are duplicated across scopes".to_string(),
        ),
        ProviderHookInspection::Missing if evidence.recorded && !evidence.executable_available => (
            ProviderSetupState::Unavailable,
            "unavailable: provider executable is absent and managed definitions are missing"
                .to_string(),
        ),
        ProviderHookInspection::Missing if evidence.executable_available => (
            ProviderSetupState::Degraded,
            "degraded: executable available with process fallback; structured hooks missing"
                .to_string(),
        ),
        ProviderHookInspection::Missing => (
            ProviderSetupState::Skipped,
            "skipped: provider was not selected and executable is absent".to_string(),
        ),
        ProviderHookInspection::Current if !evidence.executable_available => (
            ProviderSetupState::Unavailable,
            "unavailable: managed definitions current, but provider executable is absent"
                .to_string(),
        ),
        ProviderHookInspection::Current => (
            ProviderSetupState::Current,
            "current: executable and managed definitions are available".to_string(),
        ),
    };
    let status = match state {
        ProviderSetupState::Current => CheckStatus::Pass,
        ProviderSetupState::Degraded | ProviderSetupState::Unavailable => CheckStatus::Advisory,
        ProviderSetupState::Stale => CheckStatus::Fail,
        ProviderSetupState::Skipped => CheckStatus::Skipped,
    };
    let fix_hint = (!matches!(
        state,
        ProviderSetupState::Current | ProviderSetupState::Skipped
    ))
    .then(|| {
        format!(
            "Repair {} setup with `coding-brain init {}`.",
            provider.label(),
            provider.as_str()
        )
    });
    Check {
        name: format!("{} setup", provider.label()),
        status,
        message,
        fix_hint,
    }
}

fn check_binary_on_path() -> Check {
    // Compare the running binary against what `which coding-brain` resolves
    // to. Mismatches mean the user is running one binary while their
    // hooks resolve a different one (typical after `cargo install` on top
    // of a previous `brew install`).
    let running = std::env::current_exe().ok();
    let on_path = std::process::Command::new("which")
        .arg("coding-brain")
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
                "Two installs detected. Re-run `coding-brain init` so hooks use the running immutable executable."
                    .into(),
            ),
        },
        (Some(r), None) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Fail,
            message: format!("{} not on PATH", r.display()),
            fix_hint: Some(
                "Add the install dir to PATH so `coding-brain` is directly available.".into(),
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

#[cfg(test)]
fn check_codex_hooks_at(home: Option<&std::path::Path>, cwd: &std::path::Path) -> Check {
    let Some(home) = home else {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Fail,
            message: "HOME not set".into(),
            fix_hint: None,
        };
    };
    let discovery = crate::init::hooks::discover_lifecycle_hooks_at(Some(home), cwd);
    if !discovery.configured() {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Fail,
            message: "managed lifecycle definitions missing".into(),
            fix_hint: Some(
                "Run `coding-brain init` (or `coding-brain init --plugin-only`).".into(),
            ),
        };
    }

    if discovery.duplicate_scopes() {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Advisory,
            message: "managed definitions duplicated in global and project scopes".into(),
            fix_hint: Some(
                "Keep the managed hook set in one scope, restart Codex, and review `/hooks`."
                    .into(),
            ),
        };
    }

    let scope = if discovery.global.configured() {
        (&discovery.global, "global")
    } else {
        (&discovery.project, "project")
    };
    for event in crate::init::hooks::ManagedHookEvent::ALL {
        let state = &scope.0.events[&event];
        if !state.configured {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Fail,
                message: format!("{} {} definition missing", scope.1, event.as_str()),
                fix_hint: Some(
                    "Run `coding-brain init`, restart Codex, and review `/hooks`.".into(),
                ),
            };
        }
        if state.unavailable {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Fail,
                message: format!("{} {} executable unavailable", scope.1, event.as_str()),
                fix_hint: Some(
                    "Reinstall Coding Brain or rerun `coding-brain init`, then review `/hooks`."
                        .into(),
                ),
            };
        }
        if state.disabled {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Advisory,
                message: format!("{} {} definition disabled", scope.1, event.as_str()),
                fix_hint: Some(
                    "Enable the definition and review it through Codex `/hooks`.".into(),
                ),
            };
        }
        if state.stale || !state.current {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Advisory,
                message: format!("{} {} definition stale", scope.1, event.as_str()),
                fix_hint: Some(
                    "Run `coding-brain init`, restart Codex, and review the changed definition with `/hooks`."
                        .into(),
                ),
            };
        }
    }
    debug_assert!(scope.0.definitions_current());

    Check {
        name: "Codex hooks".into(),
        status: CheckStatus::Pass,
        message: format!("{} definitions current", scope.1),
        fix_hint: None,
    }
}

fn check_codex_hook_trust() -> Check {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    check_codex_hook_trust_at(home.as_deref(), &cwd)
}

fn check_codex_hook_trust_at(home: Option<&std::path::Path>, cwd: &std::path::Path) -> Check {
    let discovery = crate::init::hooks::discover_lifecycle_hooks_at(home, cwd);
    if !discovery.trust_unverified {
        return Check {
            name: "Codex hook trust".into(),
            status: CheckStatus::Skipped,
            message: "no enabled managed definitions".into(),
            fix_hint: None,
        };
    }

    Check {
        name: "Codex hook trust".into(),
        status: CheckStatus::Advisory,
        message: "trust unverified; review /hooks".into(),
        fix_hint: Some("Restart Codex and confirm the managed commands through `/hooks`.".into()),
    }
}

fn check_lifecycle_state() -> Check {
    check_lifecycle_state_with_store(&LifecycleStore::at(coding_brain_state_root()))
}

fn check_lifecycle_state_with_store(store: &LifecycleStore) -> Check {
    let (status, message, fix_hint) = match store.read() {
        Ok(view) => match view.condition {
            StoreCondition::Healthy => (CheckStatus::Pass, "state readable".into(), None),
            StoreCondition::Missing => (
                CheckStatus::Advisory,
                "state not created yet".into(),
                Some("Run a Codex turn after enabling and trusting the managed hooks.".into()),
            ),
            StoreCondition::Corrupt => (
                CheckStatus::Advisory,
                "lifecycle state is corrupt".into(),
                Some(
                    "Let the next lifecycle event quarantine and rebuild it, or remove only the corrupt snapshot."
                        .into(),
                ),
            ),
            StoreCondition::NewerSchema(version) => (
                CheckStatus::Advisory,
                format!("lifecycle state uses newer schema {version}"),
                Some("Upgrade Coding Brain before writing lifecycle state.".into()),
            ),
            StoreCondition::Unavailable => (
                CheckStatus::Advisory,
                "lifecycle state is unavailable".into(),
                Some("Check state-directory ownership and permissions.".into()),
            ),
        },
        Err(error) => (
            CheckStatus::Advisory,
            format!("lifecycle state unavailable: {error}"),
            Some("Check state-directory ownership and permissions.".into()),
        ),
    };
    Check {
        name: "lifecycle state".into(),
        status,
        message,
        fix_hint,
    }
}

fn check_outcome_telemetry() -> Check {
    let paths = match coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    ) {
        Ok(paths) => paths,
        Err(_) => return outcome_telemetry_unavailable(),
    };
    check_outcome_telemetry_with_store(&ActivityStore::at(
        paths.state_root().join("activity.jsonl"),
    ))
}

fn outcome_telemetry_unavailable() -> Check {
    Check {
        name: "outcome telemetry".into(),
        status: CheckStatus::Advisory,
        message: "activity store unavailable".into(),
        fix_hint: Some("Check state-directory ownership and permissions.".into()),
    }
}

fn check_outcome_telemetry_with_store(store: &ActivityStore) -> Check {
    let log = match store.read() {
        Ok(log) => log,
        Err(_) => return outcome_telemetry_unavailable(),
    };

    fn invocation_key(
        event: &coding_brain_core::brain_activity::ActivityEvent,
    ) -> Option<(&str, &str, &str)> {
        let session = event.session.as_ref()?;
        Some((
            session.session_id.as_str(),
            session.turn_id.as_deref()?,
            session.tool_use_id.as_deref()?,
        ))
    }

    let mut invocation_recency = HashMap::new();
    for event in log.events() {
        if event.kind != ActivityKind::Lifecycle
            || !matches!(event.tool.as_deref(), Some("PreToolUse" | "PostToolUse"))
        {
            continue;
        }
        let Some(key) = invocation_key(event) else {
            continue;
        };
        invocation_recency
            .entry(key)
            .and_modify(|recorded_at_ms: &mut u64| {
                *recorded_at_ms = (*recorded_at_ms).max(event.recorded_at_ms);
            })
            .or_insert(event.recorded_at_ms);
    }
    let mut invocation_recency = invocation_recency.into_iter().collect::<Vec<_>>();
    invocation_recency
        .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    let selected_invocations = invocation_recency
        .into_iter()
        .take(100)
        .map(|(key, _)| key)
        .collect::<HashSet<_>>();

    let mut invocation_evidence = HashMap::new();
    for event in log.events().iter().rev() {
        if event.kind != ActivityKind::Lifecycle {
            continue;
        }
        let Some(key) = invocation_key(event) else {
            continue;
        };
        if !selected_invocations.contains(&key) {
            continue;
        }
        let evidence = invocation_evidence.entry(key).or_insert((false, false));
        match event.tool.as_deref() {
            Some("PreToolUse") => evidence.0 = true,
            Some("PostToolUse") => evidence.1 = true,
            _ => {}
        }
    }

    let pre_count = invocation_evidence
        .values()
        .filter(|(has_pre, _)| *has_pre)
        .count();
    if pre_count < 10 {
        return Check {
            name: "outcome telemetry".into(),
            status: CheckStatus::Skipped,
            message: format!("insufficient activity ({pre_count}/10 tool invocations)"),
            fix_hint: None,
        };
    }

    let post_count = invocation_evidence
        .values()
        .filter(|(_, has_post)| *has_post)
        .count();
    if post_count == 0 {
        return Check {
            name: "outcome telemetry".into(),
            status: CheckStatus::Advisory,
            message: format!("no PostToolUse evidence across {pre_count} recent invocations"),
            fix_hint: Some(
                "Upgrade or restart Codex, review `/hooks`, complete local tools, and rerun `coding-brain doctor`."
                    .into(),
            ),
        };
    }

    #[derive(Default)]
    struct DecisionEvidence {
        first_terminal: Option<ActivityState>,
        delivered_at_ms: Option<u64>,
        has_outcome: bool,
    }

    let mut decisions = HashMap::<&str, DecisionEvidence>::new();
    for event in log
        .events()
        .iter()
        .filter(|event| event.kind == ActivityKind::Decision)
    {
        let evidence = decisions.entry(&event.activity_id).or_default();
        if evidence.first_terminal.is_none() && event.state.is_terminal() {
            evidence.first_terminal = Some(event.state);
        }
        if event.state == ActivityState::Delivered {
            evidence.delivered_at_ms = Some(
                evidence
                    .delivered_at_ms
                    .map_or(event.recorded_at_ms, |current| {
                        current.max(event.recorded_at_ms)
                    }),
            );
        }
        evidence.has_outcome |= event.state == ActivityState::Outcome;
    }

    let mut eligible = decisions
        .into_iter()
        .filter_map(|(activity_id, evidence)| {
            (evidence.first_terminal == Some(ActivityState::Allowed))
                .then_some(evidence.delivered_at_ms)
                .flatten()
                .map(|delivered_at_ms| (activity_id, delivered_at_ms, evidence.has_outcome))
        })
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    eligible.truncate(20);

    let eligible_count = eligible.len();
    if eligible_count < 5 {
        return Check {
            name: "outcome telemetry".into(),
            status: CheckStatus::Skipped,
            message: format!("insufficient decisions ({eligible_count}/5 eligible decisions)"),
            fix_hint: None,
        };
    }

    let outcome_count = eligible
        .iter()
        .filter(|(_, _, has_outcome)| *has_outcome)
        .count();
    if outcome_count == 0 {
        return Check {
            name: "outcome telemetry".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "PostToolUse observed but 0/{eligible_count} recent decisions have outcomes"
            ),
            fix_hint: Some(
                "Run current Codex hooks and inspect lifecycle-hook attribution diagnostics."
                    .into(),
            ),
        };
    }

    Check {
        name: "outcome telemetry".into(),
        status: CheckStatus::Pass,
        message: format!(
            "PostToolUse {post_count}/{pre_count} recent invocations; outcomes {outcome_count}/{eligible_count} recent decisions"
        ),
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

fn check_project_identity() -> Check {
    let paths = match coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    ) {
        Ok(paths) => paths,
        Err(error) => {
            return Check {
                name: "project identity".into(),
                status: CheckStatus::Advisory,
                message: format!("path resolution failed: {error:?}"),
                fix_hint: Some("Set HOME or absolute XDG config/state directories.".into()),
            };
        }
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    check_project_identity_at(&cwd, &paths)
}

fn check_project_identity_at(
    cwd: &Path,
    paths: &coding_brain_core::paths::CodingBrainPaths,
) -> Check {
    match coding_brain_core::project::ProjectIdentity::load(cwd, paths) {
        Ok(identity) if identity.is_durable() => Check {
            name: "project identity".into(),
            status: CheckStatus::Pass,
            message: "stable project identity loaded".into(),
            fix_hint: None,
        },
        Ok(_) => Check {
            name: "project identity".into(),
            status: CheckStatus::Advisory,
            message: "no manifest or usable Git origin; memory is temporary".into(),
            fix_hint: Some(
                "Run `coding-brain init` to create an explicit identity override at the project-root `.coding-brain/project.toml`. Removing the project-root `.coding-brain/project.toml` before rerunning init deliberately creates a new identity."
                    .into(),
            ),
        },
        Err(error) => Check {
            name: "project identity".into(),
            status: CheckStatus::Advisory,
            message: format!("project manifest is malformed: {error}"),
            fix_hint: Some(
                "Fix the project-root `.coding-brain/project.toml`, or remove it before `coding-brain init` to deliberately create a new identity."
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
        let message = endpoint_warning(endpoint).unwrap_or_default();
        Check {
            name: "brain endpoint privacy".into(),
            status: CheckStatus::Advisory,
            message,
            fix_hint: Some(
                "Use a loopback endpoint or confirm the remote endpoint's privacy policy.".into(),
            ),
        }
    }
}

pub(crate) fn endpoint_warning(endpoint: &str) -> Option<String> {
    if is_loopback_endpoint(endpoint) {
        None
    } else if endpoint.to_ascii_lowercase().starts_with("http://") {
        Some(format!(
            "{endpoint} is remote plaintext HTTP; transcript context and credentials may be exposed in transit"
        ))
    } else {
        Some(format!(
            "{endpoint} is not loopback; transcript context may leave this machine"
        ))
    }
}

fn check_session_discovery() -> Check {
    // Discovery never errors per se — it returns 0 sessions when nothing
    // matches. The signal we want is "the scanner runs and finds at
    // least one session." Zero sessions is normal if no Codex is
    // running; advise instead of fail.
    let sessions = coding_brain_core::discovery::scan_agent_sessions_with_state(
        &mut coding_brain_core::discovery::ProviderDiscoveryState::default(),
    );
    check_session_discovery_for(&sessions)
}

fn check_session_discovery_for(sessions: &[coding_brain_core::session::AgentSession]) -> Check {
    let counts = coding_brain_core::health::provider_session_counts(sessions);
    let message = coding_brain_core::health::format_provider_session_counts(&counts);
    if sessions.is_empty() {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Advisory,
            message,
            fix_hint: Some(
                "Start a selected provider session and re-run `coding-brain doctor`.".into(),
            ),
        }
    } else {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Pass,
            message,
            fix_hint: None,
        }
    }
}

fn check_terminal_capabilities() -> Vec<Check> {
    coding_brain_core::terminals::provider_capability_diagnostics()
        .into_iter()
        .map(|capability| Check {
            name: capability.name.into(),
            status: match capability.status {
                coding_brain_core::terminals::DoctorStatus::Ready => CheckStatus::Pass,
                coding_brain_core::terminals::DoctorStatus::Blocked => CheckStatus::Fail,
                coding_brain_core::terminals::DoctorStatus::Unsupported => CheckStatus::Advisory,
            },
            message: capability.detail,
            fix_hint: capability.fix,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
        ProjectEvidence, SessionTarget,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::provider::AgentProvider;

    use crate::brain::activity::ActivityStore;
    use crate::init::provider_hooks::ProviderHookInspection;

    fn telemetry_event(
        activity_id: &str,
        kind: ActivityKind,
        state: ActivityState,
        recorded_at_ms: u64,
        tool: &str,
        tool_use_id: Option<&str>,
        outcome: Option<ActivityOutcome>,
    ) -> ActivityEvent {
        let project_id = ProjectId::Temporary("doctor-project".into());
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind,
            activity_id: activity_id.into(),
            recorded_at_ms,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: PathBuf::from("/work/doctor-project"),
                label: Some("doctor-project".into()),
            },
            session: Some(SessionTarget {
                provider: coding_brain_core::provider::AgentProvider::Codex,
                session_id: "doctor-session".into(),
                turn_id: Some("doctor-turn".into()),
                tool_use_id: tool_use_id.map(str::to_owned),
                project_id,
                cwd: PathBuf::from("/work/doctor-project"),
                provider_hints: Vec::new(),
                provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
            }),
            state,
            tool: Some(tool.into()),
            normalized_command: (kind == ActivityKind::Decision).then(|| "cargo test".into()),
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: (kind == ActivityKind::Decision)
                .then(|| format!("decision-{activity_id}")),
            outcome,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn fixture_activity_store() -> (tempfile::TempDir, ActivityStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = ActivityStore::at(temp.path().join("activity.jsonl"));
        (temp, store)
    }

    fn append_tool_invocation(store: &ActivityStore, index: usize, with_post: bool) {
        let call = format!("call-{index}");
        store
            .append(telemetry_event(
                &format!("pre-{index}"),
                ActivityKind::Lifecycle,
                ActivityState::Abstained,
                (index * 2) as u64,
                "PreToolUse",
                Some(&call),
                None,
            ))
            .unwrap();
        if with_post {
            store
                .append(telemetry_event(
                    &format!("post-{index}"),
                    ActivityKind::Lifecycle,
                    ActivityState::Abstained,
                    (index * 2 + 1) as u64,
                    "PostToolUse",
                    Some(&call),
                    None,
                ))
                .unwrap();
        }
    }

    fn append_delivered_decision(store: &ActivityStore, index: usize, with_outcome: bool) {
        let id = format!("activity-{index}");
        store
            .append(telemetry_event(
                &id,
                ActivityKind::Decision,
                ActivityState::Allowed,
                (10_000 + index * 3) as u64,
                "Bash",
                None,
                None,
            ))
            .unwrap();
        store
            .append(telemetry_event(
                &id,
                ActivityKind::Decision,
                ActivityState::Delivered,
                (10_001 + index * 3) as u64,
                "Bash",
                None,
                None,
            ))
            .unwrap();
        if with_outcome {
            store
                .append(telemetry_event(
                    &id,
                    ActivityKind::Decision,
                    ActivityState::Outcome,
                    (10_002 + index * 3) as u64,
                    "Bash",
                    Some(&format!("call-{index}")),
                    Some(ActivityOutcome::Completed),
                ))
                .unwrap();
        }
    }

    #[test]
    fn outcome_telemetry_has_exact_minimum_boundaries() {
        let (_, store) = fixture_activity_store();
        for index in 0..9 {
            append_tool_invocation(&store, index, false);
        }
        assert_eq!(
            check_outcome_telemetry_with_store(&store).status,
            CheckStatus::Skipped
        );
        append_tool_invocation(&store, 9, false);
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert_eq!(exit_code(&[check]), 0);

        let (_, store) = fixture_activity_store();
        for index in 0..10 {
            append_tool_invocation(&store, index, true);
        }
        for index in 0..4 {
            append_delivered_decision(&store, index, false);
        }
        assert_eq!(
            check_outcome_telemetry_with_store(&store).status,
            CheckStatus::Skipped
        );
        append_delivered_decision(&store, 4, false);
        assert_eq!(
            check_outcome_telemetry_with_store(&store).status,
            CheckStatus::Advisory
        );
    }

    #[test]
    fn outcome_telemetry_retries_do_not_inflate_unique_invocations() {
        let (_, store) = fixture_activity_store();
        for index in 0..11 {
            store
                .append(telemetry_event(
                    &format!("retry-{index}"),
                    ActivityKind::Lifecycle,
                    ActivityState::Abstained,
                    index as u64,
                    "PreToolUse",
                    Some("same-call"),
                    None,
                ))
                .unwrap();
        }
        assert_eq!(
            check_outcome_telemetry_with_store(&store).status,
            CheckStatus::Skipped
        );
    }

    #[test]
    fn outcome_telemetry_counts_post_receipt_independently_from_pre_threshold() {
        let (_, store) = fixture_activity_store();
        for index in 0..9 {
            append_tool_invocation(&store, index, false);
        }
        store
            .append(telemetry_event(
                "post-only",
                ActivityKind::Lifecycle,
                ActivityState::Abstained,
                100,
                "PostToolUse",
                Some("post-only-call"),
                None,
            ))
            .unwrap();

        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Skipped);
        assert!(check.message.contains("9/10 tool invocations"));

        append_tool_invocation(&store, 9, false);
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Skipped);
        assert!(check.message.contains("0/5 eligible decisions"));
        assert!(!check.message.contains("no PostToolUse evidence"));
    }

    #[test]
    fn outcome_telemetry_old_post_evidence_expires_from_the_hundred_key_window() {
        let (_, store) = fixture_activity_store();
        append_tool_invocation(&store, 0, true);
        for index in 1..=100 {
            append_tool_invocation(&store, index, false);
        }
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("no PostToolUse evidence"));
    }

    #[test]
    fn outcome_telemetry_delayed_outcome_does_not_reorder_the_decision_window() {
        let (_, store) = fixture_activity_store();
        for index in 0..10 {
            append_tool_invocation(&store, index, true);
        }
        for index in 0..21 {
            append_delivered_decision(&store, index, false);
        }
        store
            .append(telemetry_event(
                "activity-0",
                ActivityKind::Decision,
                ActivityState::Outcome,
                99_999,
                "Bash",
                Some("call-0"),
                Some(ActivityOutcome::Completed),
            ))
            .unwrap();
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("0/20"));
    }

    #[test]
    fn outcome_telemetry_reverse_post_rows_do_not_hide_selected_pre_rows() {
        let (_, store) = fixture_activity_store();
        for index in 0..100 {
            append_tool_invocation(&store, index, true);
        }
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Skipped);
        assert!(!check.message.contains("insufficient activity"));
        assert!(!check.message.contains("no PostToolUse evidence"));
    }

    #[test]
    fn outcome_telemetry_passes_with_current_bounded_evidence() {
        let (_, store) = fixture_activity_store();
        for index in 0..10 {
            append_tool_invocation(&store, index, true);
        }
        for index in 0..5 {
            append_delivered_decision(&store, index, index == 4);
        }
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.message.contains("10/10"));
        assert!(check.message.contains("1/5"));
    }

    #[test]
    fn outcome_telemetry_decision_retries_do_not_inflate_eligible_count() {
        let (_, store) = fixture_activity_store();
        for index in 0..10 {
            append_tool_invocation(&store, index, true);
        }
        for index in 0..6 {
            store
                .append(telemetry_event(
                    "same-activity",
                    ActivityKind::Decision,
                    if index % 2 == 0 {
                        ActivityState::Allowed
                    } else {
                        ActivityState::Delivered
                    },
                    (10_000 + index) as u64,
                    "Bash",
                    None,
                    None,
                ))
                .unwrap();
        }
        let check = check_outcome_telemetry_with_store(&store);
        assert_eq!(check.status, CheckStatus::Skipped);
        assert!(check.message.contains("1/5"));
    }

    #[test]
    fn outcome_telemetry_store_read_failures_are_non_fatal_and_metadata_safe() {
        let temp = tempfile::tempdir().unwrap();
        let store = ActivityStore::at(temp.path());

        let check = check_outcome_telemetry_with_store(&store);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert_eq!(exit_code(std::slice::from_ref(&check)), 0);
        assert!(check.message.contains("activity store unavailable"));
        assert!(!check.message.contains(&temp.path().display().to_string()));
        assert!(
            check
                .fix_hint
                .unwrap()
                .contains("state-directory ownership and permissions")
        );
    }
    fn fixture_paths(home: &Path) -> coding_brain_core::paths::CodingBrainPaths {
        coding_brain_core::paths::CodingBrainPaths::resolve(
            &coding_brain_core::paths::PathEnvironment::new(None, None, Some(home.to_path_buf())),
        )
        .unwrap()
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn project_identity_passes_for_manifest() {
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path());
        coding_brain_core::project::ProjectManifest::create(root.path(), &paths).unwrap();

        let check = check_project_identity_at(root.path(), &paths);

        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.message, "stable project identity loaded");
        assert_eq!(check.fix_hint, None);
    }

    #[test]
    fn project_identity_passes_for_git_origin_without_manifest() {
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init", "--quiet"]);
        run_git(
            root.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        );
        let paths = fixture_paths(root.path());

        let check = check_project_identity_at(root.path(), &paths);

        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.message, "stable project identity loaded");
        assert_eq!(check.fix_hint, None);
    }

    #[test]
    fn project_identity_advises_init_without_manifest_or_origin() {
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init", "--quiet"]);
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(root.path());

        let check = check_project_identity_at(&nested, &paths);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert_eq!(
            check.message,
            "no manifest or usable Git origin; memory is temporary"
        );
        let hint = check.fix_hint.unwrap();
        assert!(hint.contains("coding-brain init"));
        assert!(hint.contains("project-root `.coding-brain/project.toml`"));
    }

    #[test]
    fn project_identity_advises_actionable_fix_for_malformed_manifest() {
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init", "--quiet"]);
        let paths = fixture_paths(root.path());
        let project_dir = root.path().join(".coding-brain");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("project.toml"), "not valid toml = [").unwrap();
        let nested = root.path().join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let check = check_project_identity_at(&nested, &paths);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("project manifest is malformed"));
        let hint = check.fix_hint.unwrap();
        assert!(hint.contains("Fix the project-root `.coding-brain/project.toml`"));
        assert!(hint.contains("coding-brain init"));
    }

    #[test]
    fn render_handles_empty_check_list() {
        let out = render_checks(&[]);
        assert!(out.contains("coding-brain doctor"));
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
    fn provider_hook_recovery_failure_is_a_failing_check() {
        let check = provider_hook_recovery_check(Err(io::Error::other("invalid journal")))
            .expect("recovery failure must be visible");

        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(exit_code(&[check]), 1);
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
    fn plaintext_remote_endpoint_has_stronger_warning() {
        let plaintext = endpoint_warning("http://brain.example.com/v1/chat").unwrap();
        let tls = endpoint_warning("https://brain.example.com/v1/chat").unwrap();
        assert!(plaintext.contains("plaintext HTTP"));
        assert!(plaintext.contains("exposed in transit"));
        assert!(!tls.contains("plaintext HTTP"));
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

    fn current_hooks() -> serde_json::Value {
        serde_json::json!({
            "hooks": {
                "SessionStart": [{ "matcher": "startup|resume|clear|compact", "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "coding-brain --permission-hook", "timeout": 30, "statusMessage": "Brain reviewing permission…" }] }],
                "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "SubagentStart": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "SubagentStop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "coding-brain --lifecycle-hook", "timeout": 2 }] }],
                "Stop": [{ "hooks": [{ "type": "command", "command": "coding-brain --recovery-hook", "timeout": 30 }] }]
            }
        })
    }

    fn write_hooks(path: &std::path::Path, value: &serde_json::Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn current_definitions_pass_while_trust_remains_advisory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(&home.join(".codex/hooks.json"), &current_hooks());

        let definitions = check_codex_hooks_at(Some(&home), &cwd);
        let trust = check_codex_hook_trust_at(Some(&home), &cwd);

        assert_eq!(definitions.status, CheckStatus::Pass);
        assert_eq!(definitions.message, "global definitions current");
        assert_eq!(trust.status, CheckStatus::Advisory);
        assert_eq!(trust.message, "trust unverified; review /hooks");
    }

    #[test]
    fn duplicate_global_and_project_hook_sets_are_advisory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(&home.join(".codex/hooks.json"), &current_hooks());
        write_hooks(&cwd.join(".codex/hooks.json"), &current_hooks());

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("global and project"));
        assert!(check.fix_hint.unwrap().contains("one scope"));
    }

    #[test]
    fn missing_stale_disabled_and_unavailable_definitions_name_the_event() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        let path = home.join(".codex/hooks.json");

        let mut missing = current_hooks();
        missing["hooks"]
            .as_object_mut()
            .unwrap()
            .remove("PostToolUse");
        write_hooks(&path, &missing);
        let check = check_codex_hooks_at(Some(&home), &cwd);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("PostToolUse definition missing"));

        let mut stale = current_hooks();
        stale["hooks"]["PermissionRequest"][0]["matcher"] = serde_json::json!("Bash");
        write_hooks(&path, &stale);
        let check = check_codex_hooks_at(Some(&home), &cwd);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("PermissionRequest definition stale"));

        let mut disabled = current_hooks();
        disabled["hooks"]["SubagentStop"][0]["disabled"] = serde_json::json!(true);
        write_hooks(&path, &disabled);
        let check = check_codex_hooks_at(Some(&home), &cwd);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("SubagentStop definition disabled"));

        let mut unavailable = current_hooks();
        unavailable["hooks"]["SessionStart"][0]["hooks"][0]["command"] =
            serde_json::json!("/definitely/missing/coding-brain --lifecycle-hook");
        write_hooks(&path, &unavailable);
        let check = check_codex_hooks_at(Some(&home), &cwd);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(
            check
                .message
                .contains("SessionStart executable unavailable")
        );
    }

    #[test]
    fn lifecycle_state_diagnoses_corrupt_and_newer_schema_separately() {
        let temp = tempfile::tempdir().unwrap();
        let corrupt = LifecycleStore::at(temp.path().join("corrupt"));
        std::fs::create_dir_all(corrupt.hooks_dir()).unwrap();
        std::fs::write(corrupt.snapshot_path(), b"not json").unwrap();
        let check = check_lifecycle_state_with_store(&corrupt);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("corrupt"));
        assert!(check.fix_hint.unwrap().contains("quarantine"));

        let newer = LifecycleStore::at(temp.path().join("newer"));
        std::fs::create_dir_all(newer.hooks_dir()).unwrap();
        std::fs::write(newer.snapshot_path(), br#"{"schema_version":99}"#).unwrap();
        let check = check_lifecycle_state_with_store(&newer);
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("newer schema 99"));
        assert!(check.fix_hint.unwrap().contains("Upgrade"));
    }

    #[test]
    fn unrelated_hooks_only_are_reported_missing() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join(".git")).unwrap();
        write_hooks(
            &home.join(".codex/hooks.json"),
            &serde_json::json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "echo stop" }] }] } }),
        );

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("definitions missing"));
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
        write_hooks(&jj_root.join(".codex/hooks.json"), &current_hooks());

        let check = check_codex_hooks_at(Some(&home), &cwd);

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("definitions missing"));
    }

    #[test]
    fn provider_setup_matrix_maps_internal_states_to_existing_severity() {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            for hooks in [
                ProviderHookInspection::Missing,
                ProviderHookInspection::Current,
                ProviderHookInspection::Duplicate,
                ProviderHookInspection::Stale,
                ProviderHookInspection::Invalid,
            ] {
                for recorded in [false, true] {
                    for executable_available in [false, true] {
                        let (status, state) = match hooks {
                            ProviderHookInspection::Invalid => (CheckStatus::Fail, "invalid"),
                            ProviderHookInspection::Stale => {
                                (CheckStatus::Fail, "definition stale")
                            }
                            ProviderHookInspection::Duplicate if executable_available => {
                                (CheckStatus::Advisory, "degraded")
                            }
                            ProviderHookInspection::Duplicate => {
                                (CheckStatus::Advisory, "unavailable")
                            }
                            ProviderHookInspection::Missing if executable_available => {
                                (CheckStatus::Advisory, "degraded")
                            }
                            ProviderHookInspection::Missing if recorded => {
                                (CheckStatus::Advisory, "unavailable")
                            }
                            ProviderHookInspection::Missing => (CheckStatus::Skipped, "skipped"),
                            ProviderHookInspection::Current if executable_available => {
                                (CheckStatus::Pass, "current")
                            }
                            ProviderHookInspection::Current => {
                                (CheckStatus::Advisory, "unavailable")
                            }
                        };
                        let check = check_provider_setup(
                            provider,
                            ProviderSetupEvidence {
                                recorded,
                                executable_available,
                                hooks,
                            },
                        );
                        assert_eq!(check.name, format!("{} setup", provider.label()));
                        assert_eq!(check.status, status, "{provider} {state}");
                        assert!(check.message.contains(state), "{provider} {state}");
                        if let Some(hint) = check.fix_hint {
                            assert!(hint.contains(provider.label()));
                            assert!(
                                hint.contains(&format!("coding-brain init {}", provider.as_str()))
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn unsafe_or_stale_definition_fails_even_when_provider_was_not_selected() {
        for hooks in [
            ProviderHookInspection::Stale,
            ProviderHookInspection::Invalid,
        ] {
            let check = check_provider_setup(
                AgentProvider::Claude,
                ProviderSetupEvidence {
                    recorded: false,
                    executable_available: false,
                    hooks,
                },
            );
            assert_eq!(check.status, CheckStatus::Fail);
        }
    }

    #[test]
    fn skipped_marker_provider_is_unselected_while_installed_marker_is_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let record = |status: &str| crate::init::marker::PhaseRecord {
            status: status.into(),
            ..Default::default()
        };
        let mut marker = crate::init::marker::OnboardingMarker::default();
        marker
            .phases
            .insert("hooks.claude".into(), record("skipped"));
        let checks = check_provider_setups_at(&home, &project, Some(&marker), &[]);
        assert_eq!(checks[1].status, CheckStatus::Skipped);

        marker
            .phases
            .insert("hooks.claude".into(), record("installed"));
        let plan = crate::init::provider_hooks::stage_provider_hooks_at(
            &[AgentProvider::Claude],
            crate::init::provider_hooks::HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let edit = &plan[0].edits[0];
        std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
        std::fs::write(&edit.path, &edit.replacement).unwrap();
        let checks = check_provider_setups_at(&home, &project, Some(&marker), &[]);
        assert_eq!(checks[1].status, CheckStatus::Advisory);
        assert!(checks[1].message.contains("unavailable"));
    }

    #[test]
    fn nested_cwd_provider_setup_uses_ancestor_project_scope() {
        for provider in [AgentProvider::Codex, AgentProvider::Claude] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("project");
            let cwd = root.join("nested/work");
            std::fs::create_dir_all(root.join(".git")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            let plans = crate::init::provider_hooks::stage_provider_hooks_at(
                &[provider],
                crate::init::provider_hooks::HookScope::Project,
                &home,
                &root,
            )
            .unwrap();
            let edit = &plans[0].edits[0];
            std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
            std::fs::write(&edit.path, &edit.replacement).unwrap();
            let mut marker = crate::init::marker::OnboardingMarker::default();
            marker.phases.insert(
                format!("hooks.{}", provider.as_str()),
                crate::init::marker::PhaseRecord {
                    status: "installed".into(),
                    ..Default::default()
                },
            );

            let checks = check_provider_setups_at(&home, &cwd, Some(&marker), &[provider]);
            let check = checks
                .iter()
                .find(|check| check.name == format!("{} setup", provider.label()))
                .unwrap();

            assert_eq!(check.status, CheckStatus::Pass);
            assert!(check.message.contains("current"));
        }
    }

    #[test]
    fn home_project_alias_is_one_current_provider_setup() {
        for provider in [AgentProvider::Codex, AgentProvider::Claude] {
            for nested in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let home = temp.path().join("home");
                let cwd = if nested {
                    home.join("nested/work")
                } else {
                    home.clone()
                };
                std::fs::create_dir_all(home.join(".git")).unwrap();
                std::fs::create_dir_all(&cwd).unwrap();
                let plans = crate::init::provider_hooks::stage_provider_hooks_at(
                    &[provider],
                    crate::init::provider_hooks::HookScope::Global,
                    &home,
                    &cwd,
                )
                .unwrap();
                let edit = &plans[0].edits[0];
                std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
                std::fs::write(&edit.path, &edit.replacement).unwrap();

                let checks = check_provider_setups_at(&home, &cwd, None, &[provider]);
                let check = checks
                    .iter()
                    .find(|check| check.name == format!("{} setup", provider.label()))
                    .unwrap();

                assert_eq!(
                    check.status,
                    CheckStatus::Pass,
                    "{provider} nested={nested}"
                );
                assert!(check.message.contains("current"));
            }
        }
    }

    #[test]
    fn discovery_check_reports_only_provider_counts() {
        let sessions = [
            provider_session(AgentProvider::Claude, "private-session-id"),
            provider_session(AgentProvider::Codex, "another-private-id"),
        ];

        let check = check_session_discovery_for(&sessions);

        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.message, "Codex: 1, Claude: 1, Antigravity: 0");
        assert!(!check.message.contains("private"));
    }

    fn provider_session(
        provider: AgentProvider,
        id: &str,
    ) -> coding_brain_core::session::AgentSession {
        coding_brain_core::session::AgentSession::from_raw(
            coding_brain_core::session::RawAgentSession {
                provider,
                pid: 1,
                process_start_identity: Some(1),
                session_id: id.into(),
                cwd: "/work".into(),
                started_at: 1,
            },
        )
    }

    #[test]
    fn human_and_json_provider_rows_are_deterministic_and_bounded() {
        let checks = [
            check_provider_setup(
                AgentProvider::Codex,
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available: true,
                    hooks: ProviderHookInspection::Current,
                },
            ),
            check_provider_setup(
                AgentProvider::Claude,
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available: false,
                    hooks: ProviderHookInspection::Current,
                },
            ),
            check_provider_setup(
                AgentProvider::Antigravity,
                ProviderSetupEvidence {
                    recorded: false,
                    executable_available: false,
                    hooks: ProviderHookInspection::Missing,
                },
            ),
        ];

        let human = render_checks(&checks);
        let json = render_checks_json(&checks).unwrap();

        assert!(human.find("Codex setup").unwrap() < human.find("Claude setup").unwrap());
        assert!(human.find("Claude setup").unwrap() < human.find("Antigravity setup").unwrap());
        assert!(json.len() < 2_048);
        assert!(!json.contains("hooks.json"));
        assert!(!json.contains("/home/"));
        assert_eq!(json, render_checks_json(&checks).unwrap());
    }

    #[test]
    fn terminal_capability_rows_render_separately_in_human_and_json_output() {
        let checks = check_terminal_capabilities();

        assert_eq!(
            checks
                .iter()
                .map(|check| check.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Agent Deck navigation",
                "Claude native attach",
                "Guarded semantic input",
                "Focus-only fallback",
            ]
        );
        let human = render_checks(&checks);
        let json = render_checks_json(&checks).unwrap();
        for name in [
            "Agent Deck navigation",
            "Claude native attach",
            "Guarded semantic input",
            "Focus-only fallback",
        ] {
            assert!(human.contains(name));
            assert!(json.contains(name));
        }
    }
}
