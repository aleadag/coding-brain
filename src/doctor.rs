//! `cbrain doctor` — install + runtime health check.
//!
//! Top-down checklist that answers "is everything wired up?" in one
//! command. Replaces what was scattered across:
//!
//! * `cbrain doctor` (complete install and runtime health)
//! * `cbrain init --check` (onboarding-marker drift only)
//! * scattered "is X reachable?" probes the user had to chain manually
//!
//! Each check returns a `Check` with status + a fix hint. The renderer
//! shows ✓ / ⚠ / ✗ icons and a one-line message; advisories are
//! non-fatal so optional brain configuration does not make doctor fail.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use coding_brain_core::brain_activity::{ActivityKind, ActivityState};
#[cfg(test)]
use coding_brain_core::lifecycle::StoreCondition;
use coding_brain_core::lifecycle::coding_brain_state_root;
#[cfg(test)]
use coding_brain_core::lifecycle::test_support::LifecycleStore;

#[cfg(test)]
use crate::brain::activity::ActivityStore;
#[cfg(test)]
use crate::brain::permission_transaction::{
    RecoveryReport as PermissionRecoveryReport, TransactionError as PermissionTransactionError,
};
use crate::brain::storage::{
    BrainDb, IntegrityHealth, MigrationCoordinator, MigrationHealth, MigrationStatus, OpenRole,
    ReviewDb, StorageDeadline, StorageError, StorageHealth, StoragePaths, WalHealth,
};

use coding_brain_core::provider::AgentProvider;

use crate::init::provider_hooks::{
    HookScope, ProviderHookDiagnosticReason, ProviderHookFileInspection, ProviderHookFileState,
    ProviderHookInspection, ProviderHookOwnership, ProviderHookState,
};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence: Option<CheckEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_files: Vec<ProviderFileEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<StorageEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StorageEvidence {
    database_path: String,
    schema_version: Option<i32>,
    sqlite_version: String,
    migration_status: String,
    wal_bytes: Option<u64>,
    wal_health: Option<String>,
    integrity_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderFileEvidence {
    path: String,
    path_lossy: bool,
    scope: HookScope,
    ownership: ProviderHookOwnership,
    state: ProviderHookFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<ProviderHookDiagnosticReason>,
}

impl From<&ProviderHookFileInspection> for ProviderFileEvidence {
    fn from(file: &ProviderHookFileInspection) -> Self {
        let path = file.path.to_string_lossy();
        let path_lossy = matches!(path, std::borrow::Cow::Owned(_));
        Self {
            path: path.into_owned(),
            path_lossy,
            scope: file.scope,
            ownership: file.ownership,
            state: file.state,
            reason: file.reason,
        }
    }
}

/// Run every health check, in display order. Order is meaningful: PATH
/// first because everything else depends on the binary being callable;
/// session discovery last because it's the integration that ties it all
/// together.
pub fn run_all_checks() -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(sqlite_storage_check_at(&coding_brain_state_root()));
    if let Some(check) =
        provider_hook_recovery_check(crate::init::provider_hooks::recover_hook_transaction())
    {
        checks.push(check);
    }
    checks.extend([check_binary_on_path()]);
    checks.extend(check_provider_setups());
    checks.extend(check_antigravity_hook_contract());
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
            evidence: None,
        }),
        Err(_) => Some(Check {
            name: "Provider hook recovery".into(),
            status: CheckStatus::Fail,
            message: "pending provider hook transaction could not be recovered".into(),
            fix_hint: Some(
                "Inspect the provider configurations and hook transaction journal before retrying."
                    .into(),
            ),
            evidence: None,
        }),
    }
}

#[cfg(test)]
fn permission_transaction_recovery_check(
    result: Result<PermissionRecoveryReport, PermissionTransactionError>,
) -> Option<Check> {
    let report = match result {
        Ok(report) => report,
        Err(_) => {
            return Some(Check {
                name: "Permission transaction recovery".into(),
                status: CheckStatus::Fail,
                message: "permission transaction recovery could not complete safely".into(),
                fix_hint: Some(
                    "Inspect the permission transaction journal directory and destination stores, then rerun `cbrain doctor`."
                        .into(),
                ),
                evidence: None,
            });
        }
    };
    if report == PermissionRecoveryReport::default() {
        return None;
    }
    if report.invalid != 0
        || report.over_budget != 0
        || report.pending != 0
        || report.removal_sync_uncertain != 0
    {
        let mut blockers = Vec::new();
        if report.active != 0 {
            blockers.push(format!("active={}", report.active));
        }
        if report.invalid != 0 {
            blockers.push(format!("invalid={}", report.invalid));
        }
        if report.over_budget != 0 {
            blockers.push(format!("over_budget={}", report.over_budget));
            if let Some(detail) = report.over_budget_detail {
                blockers.push(format!("over_budget_store={}", detail.source.store_label()));
                blockers.push(format!("over_budget_limit={}", detail.limit));
            }
        }
        if report.pending != 0 {
            blockers.push(format!("unresolved={}", report.pending));
        }
        if report.removal_sync_uncertain != 0 {
            blockers.push(format!(
                "removal_sync_uncertain={}",
                report.removal_sync_uncertain
            ));
        }
        return Some(Check {
            name: "Permission transaction recovery".into(),
            status: CheckStatus::Fail,
            message: format!("recovery blocked: {}", blockers.join(", ")),
            fix_hint: Some(
                "Inspect the permission transaction journal directory and destination stores, then rerun `cbrain doctor`."
                    .into(),
            ),
            evidence: None,
        });
    }
    if report.active != 0 {
        return Some(Check {
            name: "Permission transaction recovery".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "recovered {} transaction(s); {} active transaction(s) remain locked",
                report.completed, report.active
            ),
            fix_hint: Some(
                "Allow active permission hooks to finish, then rerun `cbrain doctor`.".into(),
            ),
            evidence: None,
        });
    }
    debug_assert!(report.rollback_ready());
    Some(Check {
        name: "Permission transaction recovery".into(),
        status: CheckStatus::Pass,
        message: format!("recovered {} permission transaction(s)", report.completed),
        fix_hint: None,
        evidence: None,
    })
}

#[derive(Clone, Copy)]
enum SqliteStorageStage {
    Migration,
    Brain,
    Review,
}

impl SqliteStorageStage {
    fn label(self) -> &'static str {
        match self {
            Self::Migration => "migration",
            Self::Brain => "Brain",
            Self::Review => "Review",
        }
    }

    fn redacted_path(self) -> &'static str {
        match self {
            Self::Migration => "$XDG_STATE_HOME/coding-brain/db",
            Self::Brain => "$XDG_STATE_HOME/coding-brain/db/brain.sqlite3",
            Self::Review => "$XDG_STATE_HOME/coding-brain/db/review.sqlite3",
        }
    }
}

struct SqliteStorageCheckFailure {
    stage: SqliteStorageStage,
    error: StorageError,
}

fn sqlite_storage_check_at(state_root: &Path) -> Check {
    let migration = match MigrationCoordinator::at(state_root).inspect() {
        Ok(status) => status,
        Err(error) => {
            return sqlite_storage_check_from_result(Err(SqliteStorageCheckFailure {
                stage: SqliteStorageStage::Migration,
                error,
            }));
        }
    };
    if migration != MigrationStatus::Complete {
        return Check {
            name: "SQLite storage".into(),
            status: CheckStatus::Advisory,
            message: "SQLite storage migration is not complete".into(),
            fix_hint: Some(
                "Run a non-hook Coding Brain command to complete storage migration.".into(),
            ),
            evidence: Some(CheckEvidence {
                provider_files: Vec::new(),
                storage: Some(StorageEvidence {
                    database_path: "$XDG_STATE_HOME/coding-brain/db/brain.sqlite3".into(),
                    schema_version: None,
                    sqlite_version: rusqlite::version().into(),
                    migration_status: migration_status_label(migration).into(),
                    wal_bytes: None,
                    wal_health: None,
                    integrity_state: "not_checked".into(),
                    error_category: None,
                }),
            }),
        };
    }
    let paths = StoragePaths::at(state_root);
    let deadline = StorageDeadline::after(Duration::from_millis(250));
    let health = BrainDb::open_current(&paths, OpenRole::NonHook, deadline)
        .and_then(|database| database.health())
        .map_err(|error| SqliteStorageCheckFailure {
            stage: SqliteStorageStage::Brain,
            error,
        });
    let health = match health {
        Ok(health) => health,
        Err(error) => return sqlite_storage_check_from_result(Err(error)),
    };
    if let Err(error) = ReviewDb::open_current(&paths, OpenRole::NonHook, deadline) {
        return sqlite_storage_check_from_result(Err(SqliteStorageCheckFailure {
            stage: SqliteStorageStage::Review,
            error,
        }));
    }
    sqlite_storage_check_from_result(Ok(health))
}

fn sqlite_storage_check_from_result(
    result: Result<StorageHealth, SqliteStorageCheckFailure>,
) -> Check {
    match result {
        Ok(health) => {
            let (status, message, fix_hint) = match health.wal {
                WalHealth::Normal => (
                    CheckStatus::Pass,
                    "SQLite storage is current; integrity requires an explicit deep check".into(),
                    None,
                ),
                WalHealth::Warning => (
                    CheckStatus::Advisory,
                    "SQLite WAL is above the warning threshold".into(),
                    Some("Run bounded non-hook storage maintenance.".into()),
                ),
                WalHealth::HardLimit => (
                    CheckStatus::Fail,
                    "SQLite WAL requires maintenance before model inference".into(),
                    Some("Run bounded non-hook storage maintenance before retrying.".into()),
                ),
            };
            Check {
                name: "SQLite storage".into(),
                status,
                message,
                fix_hint,
                evidence: Some(CheckEvidence {
                    provider_files: Vec::new(),
                    storage: Some(StorageEvidence {
                        database_path: health.database_path.into(),
                        schema_version: Some(health.schema_version),
                        sqlite_version: health.sqlite_version.into(),
                        migration_status: match health.migration {
                            MigrationHealth::Complete => "complete",
                            MigrationHealth::InProgress => "in_progress",
                        }
                        .into(),
                        wal_bytes: Some(health.wal_bytes),
                        wal_health: Some(
                            match health.wal {
                                WalHealth::Normal => "normal",
                                WalHealth::Warning => "warning",
                                WalHealth::HardLimit => "hard_limit",
                            }
                            .into(),
                        ),
                        integrity_state: match health.integrity {
                            IntegrityHealth::NotChecked => "not_checked",
                            IntegrityHealth::Ok => "ok",
                            IntegrityHealth::Corrupt => "corrupt",
                        }
                        .into(),
                        error_category: None,
                    }),
                }),
            }
        }
        Err(failure) => {
            let category = failure.error.fault_category().as_str();
            let stage = failure.stage.label();
            Check {
                name: "SQLite storage".into(),
                status: CheckStatus::Fail,
                message: format!("SQLite {stage} storage check failed ({category})"),
                fix_hint: Some(
                    "Inspect storage capacity, ownership, and integrity; then rerun `cbrain doctor`."
                        .into(),
                ),
                evidence: Some(CheckEvidence {
                    provider_files: Vec::new(),
                    storage: Some(StorageEvidence {
                        database_path: failure.stage.redacted_path().into(),
                        schema_version: None,
                        sqlite_version: rusqlite::version().into(),
                        migration_status: "unknown".into(),
                        wal_bytes: None,
                        wal_health: None,
                        integrity_state: "unknown".into(),
                        error_category: Some(category.into()),
                    }),
                }),
            }
        }
    }
}

fn migration_status_label(status: MigrationStatus) -> &'static str {
    match status {
        MigrationStatus::Building => "building",
        MigrationStatus::Verified => "verified",
        MigrationStatus::BrainPublishedIncomplete => "brain_published_incomplete",
        MigrationStatus::LegacyFrozen => "legacy_frozen",
        MigrationStatus::Complete => "complete",
    }
}

/// Human-readable renderer. Lays out one row per check, two-space
/// indent, fixed-width name column so messages align.
pub fn render_checks(checks: &[Check]) -> String {
    let mut out = String::new();
    out.push_str("cbrain doctor\n");
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
        if let Some(evidence) = &c.evidence {
            for file in &evidence.provider_files {
                let mut classification = format!(
                    "{}/{}/{}",
                    file.scope.as_str(),
                    file.ownership.as_str(),
                    file.state.as_str(),
                );
                if let Some(reason) = file.reason {
                    classification.push_str(", ");
                    classification.push_str(reason.as_str());
                }
                if file.path_lossy {
                    classification.push_str(", lossy path");
                }
                out.push_str(&format!(
                    "      {} — {}\n",
                    escape_provider_path(&file.path),
                    classification,
                ));
            }
            if let Some(storage) = &evidence.storage {
                out.push_str(&format!(
                    "      {} — schema={}, SQLite={}, migration={}, WAL={} ({}) integrity={}{}\n",
                    storage.database_path,
                    storage
                        .schema_version
                        .map_or_else(|| "unknown".into(), |version| version.to_string()),
                    storage.sqlite_version,
                    storage.migration_status,
                    storage
                        .wal_bytes
                        .map_or_else(|| "unknown".into(), |bytes| bytes.to_string()),
                    storage.wal_health.as_deref().unwrap_or("unknown"),
                    storage.integrity_state,
                    storage
                        .error_category
                        .as_deref()
                        .map_or_else(String::new, |category| format!(", error={category}")),
                ));
            }
        }
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

fn escape_provider_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        let dangerous_format = matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}'
                | '\u{200b}'..='\u{200d}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        );
        if character.is_control() || dangerous_format {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
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

#[derive(Debug, Clone)]
struct ProviderSetupEvidence {
    recorded: bool,
    executable_available: bool,
    hooks: ProviderHookInspection,
}

const ANTIGRAVITY_VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
const ANTIGRAVITY_VERSION_OUTPUT_LIMIT: usize = 128;

fn parse_antigravity_version(output: &[u8]) -> Option<[u64; 3]> {
    let text = std::str::from_utf8(output).ok()?;
    let token = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.bytes().all(|byte| byte.is_ascii_digit())
                || (part.len() > 1 && part.starts_with('0'))
        })
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}

fn check_antigravity_hook_contract_with(
    evidence: ProviderSetupEvidence,
    probe: impl FnOnce() -> Option<[u64; 3]>,
) -> Option<Check> {
    if !evidence.executable_available || evidence.hooks.state != ProviderHookState::Current {
        return None;
    }
    (probe()? == [1, 1, 5]).then(|| Check {
        name: "Antigravity hook contract".into(),
        status: CheckStatus::Advisory,
        message: "agy 1.1.5 may ignore PreToolUse decisions and retain the native prompt".into(),
        fix_hint: Some(
            "Keep the native prompt authoritative; upgrade agy, then revalidate the real hook contract."
                .into(),
        ),
        evidence: None,
    })
}

#[cfg(unix)]
fn probe_antigravity_version() -> Option<[u64; 3]> {
    let mut command = std::process::Command::new("agy");
    command.arg("--version");
    let output = crate::provider_hooks::run_bounded_process(
        &mut command,
        ANTIGRAVITY_VERSION_TIMEOUT,
        ANTIGRAVITY_VERSION_OUTPUT_LIMIT,
    )?;
    parse_antigravity_version(&output)
}

#[cfg(not(unix))]
fn probe_antigravity_version() -> Option<[u64; 3]> {
    None
}

fn check_antigravity_hook_contract() -> Option<Check> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cwd = std::env::current_dir().ok()?;
    let executable_available =
        crate::init::state::detect_provider_executables().contains(&AgentProvider::Antigravity);
    let hooks = crate::init::provider_hooks::inspect_provider_hooks_at(
        AgentProvider::Antigravity,
        &home,
        &cwd,
    );
    check_antigravity_hook_contract_with(
        ProviderSetupEvidence {
            recorded: false,
            executable_available,
            hooks,
        },
        probe_antigravity_version,
    )
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
                    hooks: ProviderHookInspection {
                        state: ProviderHookState::Invalid,
                        ownership: ProviderHookOwnership::Unsupported,
                        files: Vec::new(),
                    },
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
    let (state, message) = match evidence.hooks.state {
        ProviderHookState::Invalid => (
            ProviderSetupState::Stale,
            "invalid or unsafe managed definitions".to_string(),
        ),
        ProviderHookState::Stale => (
            ProviderSetupState::Stale,
            "managed definition stale".to_string(),
        ),
        ProviderHookState::Duplicate if !evidence.executable_available => (
            ProviderSetupState::Unavailable,
            "unavailable: provider executable is absent; managed definitions are duplicated"
                .to_string(),
        ),
        ProviderHookState::Duplicate => (
            ProviderSetupState::Degraded,
            "degraded: managed definitions are duplicated across scopes".to_string(),
        ),
        ProviderHookState::Missing
            if !evidence.executable_available
                && (evidence.recorded
                    || matches!(evidence.hooks.ownership, ProviderHookOwnership::HomeManager)) =>
        {
            (
                ProviderSetupState::Unavailable,
                "unavailable: provider executable is absent and managed definitions are missing"
                    .to_string(),
            )
        }
        ProviderHookState::Missing if evidence.executable_available => (
            ProviderSetupState::Degraded,
            "degraded: executable available with process fallback; structured hooks missing"
                .to_string(),
        ),
        ProviderHookState::Missing => (
            ProviderSetupState::Skipped,
            "skipped: provider was not selected and executable is absent".to_string(),
        ),
        ProviderHookState::Current if !evidence.executable_available => (
            ProviderSetupState::Unavailable,
            "unavailable: managed definitions current, but provider executable is absent"
                .to_string(),
        ),
        ProviderHookState::Current => (
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
    let fix_hint = match (state, evidence.hooks.ownership) {
        (ProviderSetupState::Current | ProviderSetupState::Skipped, _) => None,
        (ProviderSetupState::Unavailable, ProviderHookOwnership::HomeManager)
            if evidence.hooks.state == ProviderHookState::Current =>
        {
            Some(format!(
                "Install or enable the {} provider executable; managed definitions are current, then rerun `cbrain doctor`.",
                provider.label()
            ))
        }
        (_, ProviderHookOwnership::HomeManager) => Some(format!(
            "Repair the Home Manager-owned {} definitions in your Nix configuration, rebuild Home Manager, then rerun `cbrain doctor`.",
            provider.label()
        )),
        (_, ProviderHookOwnership::Mixed) => Some(format!(
            "Remove the duplicate scope for {} from either Home Manager or the regular provider configuration, then rerun `cbrain doctor`.",
            provider.label()
        )),
        (_, ProviderHookOwnership::Unsupported) => Some(format!(
            "Replace the unsafe {} provider file or link before rerunning setup.",
            provider.label()
        )),
        (_, ProviderHookOwnership::Imperative | ProviderHookOwnership::Absent) => Some(format!(
            "Repair {} setup with `cbrain init {}`.",
            provider.label(),
            provider.as_str()
        )),
    };
    let evidence = (state != ProviderSetupState::Current).then(|| CheckEvidence {
        provider_files: evidence.hooks.files.iter().map(Into::into).collect(),
        storage: None,
    });
    Check {
        name: format!("{} setup", provider.label()),
        status,
        message,
        fix_hint,
        evidence,
    }
}

fn check_binary_on_path() -> Check {
    // Compare the running binary against what `which cbrain` resolves
    // to. Mismatches mean the user is running one binary while their
    // hooks resolve a different one (typical after `cargo install` on top
    // of a previous `brew install`).
    let running = std::env::current_exe().ok();
    let on_path = std::process::Command::new("which")
        .arg("cbrain")
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
            evidence: None,
        },
        (Some(r), Some(p)) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: format!("running {}, PATH resolves {}", r.display(), p.display()),
            fix_hint: Some(
                "Two installs detected. Re-run `cbrain init` so hooks use the running immutable executable."
                    .into(),
            ),
            evidence: None,
        },
        (Some(r), None) => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Fail,
            message: format!("{} not on PATH", r.display()),
            fix_hint: Some(
                "Add the install dir to PATH so `cbrain` is directly available.".into(),
            ),
            evidence: None,
        },
        _ => Check {
            name: "binary on PATH".into(),
            status: CheckStatus::Advisory,
            message: "could not resolve running binary".into(),
            fix_hint: None,
            evidence: None,
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
            evidence: None,
        };
    };
    let discovery = crate::init::hooks::discover_lifecycle_hooks_at(Some(home), cwd);
    if !discovery.configured() {
        return Check {
            name: "Codex hooks".into(),
            status: CheckStatus::Fail,
            message: "managed lifecycle definitions missing".into(),
            fix_hint: Some("Run `cbrain init` (or `cbrain init --plugin-only`).".into()),
            evidence: None,
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
            evidence: None,
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
                fix_hint: Some("Run `cbrain init`, restart Codex, and review `/hooks`.".into()),
                evidence: None,
            };
        }
        if state.unavailable {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Fail,
                message: format!("{} {} executable unavailable", scope.1, event.as_str()),
                fix_hint: Some(
                    "Reinstall Coding Brain or rerun `cbrain init`, then review `/hooks`.".into(),
                ),
                evidence: None,
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
                evidence: None,
            };
        }
        if state.stale || !state.current {
            return Check {
                name: "Codex hooks".into(),
                status: CheckStatus::Advisory,
                message: format!("{} {} definition stale", scope.1, event.as_str()),
                fix_hint: Some(
                    "Run `cbrain init`, restart Codex, and review the changed definition with `/hooks`."
                        .into(),
                ),
                evidence: None,
            };
        }
    }
    debug_assert!(scope.0.definitions_current());

    Check {
        name: "Codex hooks".into(),
        status: CheckStatus::Pass,
        message: format!("{} definitions current", scope.1),
        fix_hint: None,
        evidence: None,
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
            evidence: None,
        };
    }

    Check {
        name: "Codex hook trust".into(),
        status: CheckStatus::Advisory,
        message: "trust unverified; review /hooks".into(),
        fix_hint: Some("Restart Codex and confirm the managed commands through `/hooks`.".into()),
        evidence: None,
    }
}

fn check_lifecycle_state() -> Check {
    check_lifecycle_state_at(&coding_brain_state_root())
}

fn check_lifecycle_state_at(state_root: &Path) -> Check {
    let paths = StoragePaths::at(state_root);
    match BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    )
    .and_then(|database| database.read_lifecycle())
    {
        Ok(_) => Check {
            name: "lifecycle state".into(),
            status: CheckStatus::Pass,
            message: "SQLite lifecycle state readable".into(),
            fix_hint: None,
            evidence: None,
        },
        Err(error) => Check {
            name: "lifecycle state".into(),
            status: CheckStatus::Advisory,
            message: format!(
                "SQLite lifecycle state unavailable ({})",
                error.fault_category().as_str()
            ),
            fix_hint: Some("Run `cbrain doctor` after storage migration or maintenance.".into()),
            evidence: None,
        },
    }
}

#[cfg(test)]
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
        evidence: None,
    }
}

fn check_outcome_telemetry() -> Check {
    let paths = match coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    ) {
        Ok(paths) => paths,
        Err(_) => return outcome_telemetry_unavailable(),
    };
    check_outcome_telemetry_at(paths.state_root())
}

fn check_outcome_telemetry_at(state_root: &Path) -> Check {
    let storage_paths = StoragePaths::at(state_root);
    let database = match BrainDb::open_current(
        &storage_paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_millis(250)),
    ) {
        Ok(database) => database,
        Err(_) => return outcome_telemetry_unavailable(),
    };
    let mut page = match database.read_activity_page(None, 4_096, 32 * 1024 * 1024) {
        Ok(page) => page,
        Err(_) => return outcome_telemetry_unavailable(),
    };
    page.events.sort_by_key(|record| record.cursor);
    let events = page
        .events
        .into_iter()
        .map(|record| record.event)
        .collect::<Vec<_>>();
    check_outcome_telemetry_with_events(&events)
}

fn outcome_telemetry_unavailable() -> Check {
    Check {
        name: "outcome telemetry".into(),
        status: CheckStatus::Advisory,
        message: "SQLite activity unavailable".into(),
        fix_hint: Some("Check state-directory ownership and permissions.".into()),
        evidence: None,
    }
}

#[cfg(test)]
fn check_outcome_telemetry_with_store(store: &ActivityStore) -> Check {
    let log = match store.read() {
        Ok(log) => log,
        Err(_) => return outcome_telemetry_unavailable(),
    };
    check_outcome_telemetry_with_events(log.events())
}

fn check_outcome_telemetry_with_events(
    events: &[coding_brain_core::brain_activity::ActivityEvent],
) -> Check {
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
    for event in events {
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
    for event in events.iter().rev() {
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
            evidence: None,
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
                "Upgrade or restart Codex, review `/hooks`, complete local tools, and rerun `cbrain doctor`."
                    .into(),
            ),
            evidence: None,
        };
    }

    #[derive(Default)]
    struct DecisionEvidence {
        first_terminal: Option<ActivityState>,
        delivered_at_ms: Option<u64>,
        has_outcome: bool,
    }

    let mut decisions = HashMap::<&str, DecisionEvidence>::new();
    for event in events
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
            evidence: None,
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
            evidence: None,
        };
    }

    Check {
        name: "outcome telemetry".into(),
        status: CheckStatus::Pass,
        message: format!(
            "PostToolUse {post_count}/{pre_count} recent invocations; outcomes {outcome_count}/{eligible_count} recent decisions"
        ),
        fix_hint: None,
        evidence: None,
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
            evidence: None,
        },
        _ => Check {
            name: "brain endpoint".into(),
            status: CheckStatus::Advisory,
            message: format!("local brain endpoint is not reachable at {endpoint}"),
            fix_hint: Some(
                "Brain is optional. To enable: `brew install ollama && ollama serve &` + `ollama pull gemma4:e4b`."
                    .into(),
            ),
            evidence: None,
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
                evidence: None,
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
            evidence: None,
        },
        Ok(_) => Check {
            name: "project identity".into(),
            status: CheckStatus::Advisory,
            message: "no manifest or usable Git origin; memory is temporary".into(),
            fix_hint: Some(
                "Run `cbrain init` to create an explicit identity override at the project-root `.coding-brain/project.toml`. Removing the project-root `.coding-brain/project.toml` before rerunning init deliberately creates a new identity."
                    .into(),
            ),
            evidence: None,
        },
        Err(error) => Check {
            name: "project identity".into(),
            status: CheckStatus::Advisory,
            message: format!("project manifest is malformed: {error}"),
            fix_hint: Some(
                "Fix the project-root `.coding-brain/project.toml`, or remove it before `cbrain init` to deliberately create a new identity."
                    .into(),
            ),
            evidence: None,
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
            evidence: None,
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
            evidence: None,
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
            fix_hint: Some("Start a selected provider session and re-run `cbrain doctor`.".into()),
            evidence: None,
        }
    } else {
        Check {
            name: "session discovery".into(),
            status: CheckStatus::Pass,
            message,
            fix_hint: None,
            evidence: None,
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
            evidence: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
        ProjectEvidence, SessionTarget,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::provider::AgentProvider;

    use crate::brain::activity::ActivityStore;
    use crate::brain::permission_transaction::{
        OverBudgetDetail, OverBudgetSource, RecoveryReport, TransactionError,
    };
    use crate::init::provider_hooks::{
        HookScope, ProviderHookDiagnosticReason, ProviderHookFileInspection, ProviderHookFileState,
        ProviderHookInspection, ProviderHookOwnership, ProviderHookState,
    };

    fn synthetic_inspection(
        state: ProviderHookState,
        ownership: ProviderHookOwnership,
    ) -> ProviderHookInspection {
        ProviderHookInspection {
            state,
            ownership,
            files: Vec::new(),
        }
    }

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
                provider_session_id: None,
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
        assert!(check.message.contains("SQLite activity unavailable"));
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
        assert!(hint.contains("cbrain init"));
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
        assert!(hint.contains("cbrain init"));
    }

    #[test]
    fn render_handles_empty_check_list() {
        let out = render_checks(&[]);
        assert!(out.contains("cbrain doctor"));
        assert!(out.contains("0 passed"));
    }

    #[test]
    fn sqlite_storage_doctor_evidence_is_complete_and_redacted() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        drop(crate::brain::storage::BrainDb::create_current(&paths).unwrap());
        drop(crate::brain::storage::ReviewDb::create_current(&paths).unwrap());

        let check = sqlite_storage_check_at(root.path());
        let json = serde_json::to_string(&check).unwrap();

        assert_eq!(check.name, "SQLite storage");
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(json.contains("$XDG_STATE_HOME/coding-brain/db/brain.sqlite3"));
        assert!(json.contains("schema_version"));
        assert!(json.contains("sqlite_version"));
        assert!(json.contains("migration_status"));
        assert!(json.contains("wal_bytes"));
        assert!(json.contains("integrity_state"));
        assert!(json.contains("not_checked"));
        assert!(!json.contains(&root.path().display().to_string()));
    }

    #[test]
    fn sqlite_storage_doctor_identifies_an_unusable_review_database() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        drop(crate::brain::storage::ReviewDb::create_current(&paths).unwrap());
        std::fs::remove_file(paths.db_dir().join("review-reset.lock")).unwrap();

        let check = sqlite_storage_check_at(root.path());
        let json = serde_json::to_string(&check).unwrap();
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("Review"));
        assert!(json.contains("$XDG_STATE_HOME/coding-brain/db/review.sqlite3"));
        assert!(!json.contains(&root.path().display().to_string()));
    }

    #[test]
    fn sqlite_storage_doctor_uses_fixed_error_categories() {
        let check = sqlite_storage_check_from_result(Err(SqliteStorageCheckFailure {
            stage: SqliteStorageStage::Brain,
            error: StorageError::Io(io::Error::other("/private/operator/path: token-secret")),
        }));
        let json = serde_json::to_string(&check).unwrap();

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("Brain"));
        assert!(json.contains("\"error_category\":\"io\""));
        assert!(!json.contains("private/operator"));
        assert!(!json.contains("token-secret"));

        let migration = sqlite_storage_check_from_result(Err(SqliteStorageCheckFailure {
            stage: SqliteStorageStage::Migration,
            error: StorageError::Io(io::Error::other("/private/migration/path")),
        }));
        let json = serde_json::to_string(&migration).unwrap();
        assert!(migration.message.contains("migration"));
        assert!(json.contains("$XDG_STATE_HOME/coding-brain/db"));
        assert!(!json.contains("private/migration"));
    }

    #[test]
    fn lifecycle_and_activity_doctor_checks_use_only_sqlite() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());

        let lifecycle = check_lifecycle_state_at(root.path());
        let activity = check_outcome_telemetry_at(root.path());

        assert_eq!(lifecycle.status, CheckStatus::Pass);
        assert_eq!(activity.status, CheckStatus::Skipped);
        for legacy in [
            "activity.jsonl",
            "brain/decisions.jsonl",
            "review-state.json",
            "hooks/lifecycle.json",
        ] {
            assert!(!root.path().join(legacy).exists(), "created {legacy}");
        }
    }

    #[test]
    fn exit_code_zero_when_all_pass() {
        let checks = vec![Check {
            name: "x".into(),
            status: CheckStatus::Pass,
            message: "ok".into(),
            fix_hint: None,
            evidence: None,
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
            evidence: None,
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
                evidence: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Fail,
                message: "broken".into(),
                fix_hint: Some("fix it".into()),
                evidence: None,
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
    fn permission_transaction_recovery_clean_store_adds_no_check() {
        assert!(permission_transaction_recovery_check(Ok(RecoveryReport::default())).is_none());
    }

    #[test]
    fn permission_transaction_recovery_reports_bounded_completed_count() {
        let check = permission_transaction_recovery_check(Ok(RecoveryReport {
            completed: 7,
            ..RecoveryReport::default()
        }))
        .expect("completed recovery must be visible");

        assert_eq!(check.name, "Permission transaction recovery");
        assert_eq!(check.status, CheckStatus::Pass);
        assert!(check.message.contains('7'));
        assert!(check.message.len() <= 256);
        assert_eq!(check.fix_hint, None);
    }

    #[test]
    fn permission_transaction_recovery_reports_active_without_failure() {
        let check = permission_transaction_recovery_check(Ok(RecoveryReport {
            completed: 2,
            active: 1,
            ..RecoveryReport::default()
        }))
        .expect("active recovery must be visible");

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("2"));
        assert!(check.message.contains("active"));
        assert_eq!(exit_code(&[check]), 0);
    }

    #[test]
    fn permission_transaction_recovery_invalid_and_pending_are_failing() {
        let check = permission_transaction_recovery_check(Ok(RecoveryReport {
            invalid: 1,
            pending: 1,
            ..RecoveryReport::default()
        }))
        .expect("invalid recovery evidence must be visible");

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("invalid=1"));
        assert!(check.message.contains("unresolved=1"));
        assert!(!check.message.contains("secret"));
        assert!(check.message.len() <= 256);
        assert!(check.fix_hint.as_deref().unwrap().len() <= 256);
    }

    #[test]
    fn permission_transaction_recovery_over_budget_names_fixed_store_and_limit() {
        for (source, store, limit) in [
            (OverBudgetSource::JournalCount, "journal_count", 1),
            (OverBudgetSource::JournalBytes, "journal_bytes", 1_048_576),
            (
                OverBudgetSource::DecisionEvidence,
                "decisions.jsonl",
                16_777_216,
            ),
            (
                OverBudgetSource::ActivityEvidence,
                "activity.jsonl",
                16_777_216,
            ),
        ] {
            let check = permission_transaction_recovery_check(Ok(RecoveryReport {
                over_budget: 1,
                over_budget_detail: Some(OverBudgetDetail { source, limit }),
                ..RecoveryReport::default()
            }))
            .expect("over-budget recovery evidence must be visible");

            assert_eq!(check.status, CheckStatus::Fail);
            assert!(check.message.contains("over_budget=1"));
            assert!(
                check
                    .message
                    .contains(&format!("over_budget_store={store}"))
            );
            assert!(
                check
                    .message
                    .contains(&format!("over_budget_limit={limit}"))
            );
            assert!(!check.message.contains('/'));
            assert!(check.message.len() <= 256);
        }
    }

    #[test]
    fn permission_transaction_recovery_pending_is_failing() {
        let check = permission_transaction_recovery_check(Ok(RecoveryReport {
            pending: 1,
            ..RecoveryReport::default()
        }))
        .expect("unresolved recovery evidence must be visible");

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("unresolved=1"));
    }

    #[test]
    fn permission_transaction_removal_sync_uncertainty_is_not_called_pending() {
        let check = permission_transaction_recovery_check(Ok(RecoveryReport {
            removal_sync_uncertain: 1,
            ..RecoveryReport::default()
        }))
        .expect("removal sync uncertainty must be visible");

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.message.contains("removal_sync_uncertain=1"));
        assert!(!check.message.contains("pending"));
        assert!(!check.message.contains("unresolved"));
    }

    #[test]
    fn permission_transaction_recovery_error_is_failing_and_redacted() {
        let check = permission_transaction_recovery_check(Err(TransactionError::Filesystem(
            "secret path and content",
        )))
        .expect("recovery error must be visible");

        assert_eq!(check.status, CheckStatus::Fail);
        assert!(!check.message.contains("secret"));
        assert!(!check.fix_hint.as_deref().unwrap().contains("secret"));
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
                evidence: None,
            },
            Check {
                name: "b".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
                evidence: None,
            },
            Check {
                name: "c".into(),
                status: CheckStatus::Advisory,
                message: "".into(),
                fix_hint: None,
                evidence: None,
            },
            Check {
                name: "d".into(),
                status: CheckStatus::Fail,
                message: "".into(),
                fix_hint: None,
                evidence: None,
            },
            Check {
                name: "e".into(),
                status: CheckStatus::Skipped,
                message: "".into(),
                fix_hint: None,
                evidence: None,
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
            evidence: None,
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
            evidence: None,
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
            evidence: None,
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
                "SessionStart": [{ "matcher": "startup|resume|clear|compact", "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "PermissionRequest": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "cbrain --permission-hook", "timeout": 30, "statusMessage": "Brain reviewing permission…" }] }],
                "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "SubagentStart": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "SubagentStop": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "cbrain --lifecycle-hook", "timeout": 2 }] }],
                "Stop": [{ "hooks": [{ "type": "command", "command": "cbrain --recovery-hook", "timeout": 30 }] }]
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
        let setup = check_provider_setup(
            AgentProvider::Codex,
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Current,
                    ProviderHookOwnership::HomeManager,
                ),
            },
        );
        let trust = check_codex_hook_trust_at(Some(&home), &cwd);

        assert_eq!(definitions.status, CheckStatus::Pass);
        assert_eq!(definitions.message, "global definitions current");
        assert_eq!(setup.status, CheckStatus::Pass);
        assert!(setup.fix_hint.is_none());
        assert_eq!(trust.status, CheckStatus::Advisory);
        assert_eq!(trust.message, "trust unverified; review /hooks");
        assert!(!definitions.message.contains("/hooks"));
        assert!(!setup.message.contains("/hooks"));
        assert!(trust.message.contains("/hooks"));
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
            serde_json::json!("/definitely/missing/cbrain --lifecycle-hook");
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
            for state in [
                ProviderHookState::Missing,
                ProviderHookState::Current,
                ProviderHookState::Duplicate,
                ProviderHookState::Stale,
                ProviderHookState::Invalid,
            ] {
                for recorded in [false, true] {
                    for executable_available in [false, true] {
                        let (status, state_label) = match state {
                            ProviderHookState::Invalid => (CheckStatus::Fail, "invalid"),
                            ProviderHookState::Stale => (CheckStatus::Fail, "definition stale"),
                            ProviderHookState::Duplicate if executable_available => {
                                (CheckStatus::Advisory, "degraded")
                            }
                            ProviderHookState::Duplicate => (CheckStatus::Advisory, "unavailable"),
                            ProviderHookState::Missing if executable_available => {
                                (CheckStatus::Advisory, "degraded")
                            }
                            ProviderHookState::Missing if recorded => {
                                (CheckStatus::Advisory, "unavailable")
                            }
                            ProviderHookState::Missing => (CheckStatus::Skipped, "skipped"),
                            ProviderHookState::Current if executable_available => {
                                (CheckStatus::Pass, "current")
                            }
                            ProviderHookState::Current => (CheckStatus::Advisory, "unavailable"),
                        };
                        let check = check_provider_setup(
                            provider,
                            ProviderSetupEvidence {
                                recorded,
                                executable_available,
                                hooks: synthetic_inspection(
                                    state,
                                    ProviderHookOwnership::Imperative,
                                ),
                            },
                        );
                        assert_eq!(check.name, format!("{} setup", provider.label()));
                        assert_eq!(check.status, status, "{provider} {state_label}");
                        assert!(
                            check.message.contains(state_label),
                            "{provider} {state_label}"
                        );
                        if let Some(hint) = check.fix_hint {
                            assert!(hint.contains(provider.label()));
                            assert!(hint.contains(&format!("cbrain init {}", provider.as_str())));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn provider_setup_routes_remediation_by_ownership() {
        for (
            state,
            ownership,
            recorded,
            executable_available,
            expected_status,
            expected_message,
            expected_hint,
            expects_definition_repair,
        ) in [
            (
                ProviderHookState::Missing,
                ProviderHookOwnership::HomeManager,
                false,
                false,
                CheckStatus::Advisory,
                "unavailable",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Missing,
                ProviderHookOwnership::HomeManager,
                false,
                true,
                CheckStatus::Advisory,
                "degraded",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Missing,
                ProviderHookOwnership::HomeManager,
                true,
                false,
                CheckStatus::Advisory,
                "unavailable",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Missing,
                ProviderHookOwnership::HomeManager,
                true,
                true,
                CheckStatus::Advisory,
                "degraded",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Current,
                ProviderHookOwnership::HomeManager,
                false,
                false,
                CheckStatus::Advisory,
                "unavailable",
                Some("provider executable"),
                false,
            ),
            (
                ProviderHookState::Current,
                ProviderHookOwnership::HomeManager,
                false,
                true,
                CheckStatus::Pass,
                "current",
                None,
                false,
            ),
            (
                ProviderHookState::Current,
                ProviderHookOwnership::HomeManager,
                true,
                false,
                CheckStatus::Advisory,
                "unavailable",
                Some("provider executable"),
                false,
            ),
            (
                ProviderHookState::Current,
                ProviderHookOwnership::HomeManager,
                true,
                true,
                CheckStatus::Pass,
                "current",
                None,
                false,
            ),
            (
                ProviderHookState::Stale,
                ProviderHookOwnership::HomeManager,
                true,
                true,
                CheckStatus::Fail,
                "stale",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Invalid,
                ProviderHookOwnership::HomeManager,
                true,
                true,
                CheckStatus::Fail,
                "unsafe",
                Some("Home Manager"),
                true,
            ),
            (
                ProviderHookState::Duplicate,
                ProviderHookOwnership::Mixed,
                true,
                true,
                CheckStatus::Advisory,
                "degraded",
                Some("duplicate scope"),
                false,
            ),
            (
                ProviderHookState::Invalid,
                ProviderHookOwnership::Unsupported,
                true,
                true,
                CheckStatus::Fail,
                "unsafe",
                Some("unsafe"),
                false,
            ),
        ] {
            let check = check_provider_setup(
                AgentProvider::Claude,
                ProviderSetupEvidence {
                    recorded,
                    executable_available,
                    hooks: synthetic_inspection(state, ownership),
                },
            );
            assert_eq!(check.status, expected_status);
            assert!(check.message.contains(expected_message));
            match (check.fix_hint.as_deref(), expected_hint) {
                (None, None) => {}
                (Some(hint), Some(phrase)) => {
                    assert!(hint.contains(phrase));
                    assert!(!hint.contains("cbrain init"));
                    if !expects_definition_repair {
                        assert!(!hint.contains("rebuild Home Manager"));
                    }
                }
                pair => panic!("unexpected hint pair: {pair:?}"),
            }
        }
    }

    #[test]
    fn missing_unselected_home_manager_setup_is_advisory_with_declarative_repair() {
        let check = check_provider_setup(
            AgentProvider::Claude,
            ProviderSetupEvidence {
                recorded: false,
                executable_available: false,
                hooks: synthetic_inspection(
                    ProviderHookState::Missing,
                    ProviderHookOwnership::HomeManager,
                ),
            },
        );

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("unavailable"));
        let hint = check
            .fix_hint
            .expect("missing Home Manager setup needs repair");
        assert!(hint.contains("Home Manager"));
        assert!(!hint.contains("cbrain init"));
    }

    #[test]
    fn current_home_manager_setup_without_executable_repairs_the_executable() {
        let check = check_provider_setup(
            AgentProvider::Claude,
            ProviderSetupEvidence {
                recorded: true,
                executable_available: false,
                hooks: synthetic_inspection(
                    ProviderHookState::Current,
                    ProviderHookOwnership::HomeManager,
                ),
            },
        );

        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("unavailable"));
        let hint = check
            .fix_hint
            .expect("current definitions still need executable guidance");
        assert!(hint.contains("provider executable"));
        assert!(hint.contains("definitions are current"));
        assert!(!hint.contains("rebuild Home Manager"));
        assert!(!hint.contains("cbrain init"));
    }

    #[test]
    fn unsafe_or_stale_definition_fails_even_when_provider_was_not_selected() {
        for state in [ProviderHookState::Stale, ProviderHookState::Invalid] {
            let check = check_provider_setup(
                AgentProvider::Claude,
                ProviderSetupEvidence {
                    recorded: false,
                    executable_available: false,
                    hooks: synthetic_inspection(state, ProviderHookOwnership::Imperative),
                },
            );
            assert_eq!(check.status, CheckStatus::Fail);
        }
    }

    #[test]
    fn antigravity_1_1_5_with_current_hooks_is_advisory() {
        let check = check_antigravity_hook_contract_with(
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Current,
                    ProviderHookOwnership::Imperative,
                ),
            },
            || Some([1, 1, 5]),
        )
        .expect("affected version must be visible");

        assert_eq!(check.name, "Antigravity hook contract");
        assert_eq!(check.status, CheckStatus::Advisory);
        assert!(check.message.contains("agy 1.1.5"));
        assert!(check.message.contains("native prompt"));
        assert!(
            check
                .fix_hint
                .as_deref()
                .is_some_and(|hint| hint.contains("upgrade"))
        );
        assert_eq!(exit_code(&[check]), 0);
    }

    #[test]
    fn antigravity_compatibility_probe_is_gated_by_current_setup() {
        use std::cell::Cell;

        for evidence in [
            ProviderSetupEvidence {
                recorded: true,
                executable_available: false,
                hooks: synthetic_inspection(
                    ProviderHookState::Current,
                    ProviderHookOwnership::Imperative,
                ),
            },
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Missing,
                    ProviderHookOwnership::Absent,
                ),
            },
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Stale,
                    ProviderHookOwnership::Imperative,
                ),
            },
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Duplicate,
                    ProviderHookOwnership::Imperative,
                ),
            },
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: synthetic_inspection(
                    ProviderHookState::Invalid,
                    ProviderHookOwnership::Imperative,
                ),
            },
        ] {
            let calls = Cell::new(0);
            let check = check_antigravity_hook_contract_with(evidence, || {
                calls.set(calls.get() + 1);
                Some([1, 1, 5])
            });
            assert!(check.is_none());
            assert_eq!(calls.get(), 0);
        }
    }

    #[test]
    fn antigravity_unverified_versions_have_no_compatibility_claim() {
        for version in [None, Some([1, 1, 4]), Some([1, 1, 6]), Some([2, 0, 0])] {
            let check = check_antigravity_hook_contract_with(
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available: true,
                    hooks: synthetic_inspection(
                        ProviderHookState::Current,
                        ProviderHookOwnership::Imperative,
                    ),
                },
                || version,
            );
            assert!(check.is_none(), "{version:?}");
        }
    }

    #[test]
    fn antigravity_version_parser_accepts_only_one_simple_semver_token() {
        assert_eq!(parse_antigravity_version(b"1.1.5\n"), Some([1, 1, 5]));
        assert_eq!(parse_antigravity_version(b"1.1.5\r\n"), Some([1, 1, 5]));

        for malformed in [
            b"agy 1.1.5".as_slice(),
            b"1.1".as_slice(),
            b"1.1.5-beta".as_slice(),
            b"1.1.5 extra".as_slice(),
            b" 1.1.5".as_slice(),
            b"1.1.5 ".as_slice(),
            b"01.1.5".as_slice(),
            b"\xff\xfe".as_slice(),
        ] {
            assert_eq!(parse_antigravity_version(malformed), None, "{malformed:?}");
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

    fn provider_file(
        path: &str,
        scope: HookScope,
        state: ProviderHookFileState,
        ownership: ProviderHookOwnership,
        reason: Option<ProviderHookDiagnosticReason>,
    ) -> ProviderHookFileInspection {
        ProviderHookFileInspection {
            path: PathBuf::from(path),
            scope,
            state,
            ownership,
            reason,
        }
    }

    #[test]
    fn non_current_provider_rows_render_stable_file_evidence() {
        let check = check_provider_setup(
            AgentProvider::Claude,
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: ProviderHookInspection {
                    state: ProviderHookState::Duplicate,
                    ownership: ProviderHookOwnership::Mixed,
                    files: vec![
                        provider_file(
                            "/home/example/.claude/settings.json",
                            HookScope::Global,
                            ProviderHookFileState::Current,
                            ProviderHookOwnership::HomeManager,
                            None,
                        ),
                        provider_file(
                            "/work/project/.claude/settings.json",
                            HookScope::Project,
                            ProviderHookFileState::Current,
                            ProviderHookOwnership::Imperative,
                            None,
                        ),
                    ],
                },
            },
        );

        let value = serde_json::to_value(&check).unwrap();
        assert_eq!(value["evidence"]["provider_files"][0]["scope"], "global");
        assert_eq!(
            value["evidence"]["provider_files"][0]["ownership"],
            "home_manager"
        );
        assert_eq!(value["evidence"]["provider_files"][1]["scope"], "project");
        assert_eq!(value["evidence"]["provider_files"][1]["state"], "current");
        assert_eq!(value["evidence"]["provider_files"][1]["path_lossy"], false);
        let human = render_checks(&[check]);
        assert!(human.contains("global"));
        assert!(human.contains("/home/example/.claude/settings.json"));
        assert!(human.contains("project"));
        assert!(human.contains("/work/project/.claude/settings.json"));
    }

    #[test]
    fn current_setup_omits_evidence_but_unavailable_current_hooks_include_it() {
        for (executable_available, expects_evidence) in [(true, false), (false, true)] {
            let check = check_provider_setup(
                AgentProvider::Codex,
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available,
                    hooks: ProviderHookInspection {
                        state: ProviderHookState::Current,
                        ownership: ProviderHookOwnership::Imperative,
                        files: vec![provider_file(
                            "/home/example/.codex/hooks.json",
                            HookScope::Global,
                            ProviderHookFileState::Current,
                            ProviderHookOwnership::Imperative,
                            None,
                        )],
                    },
                },
            );
            assert_eq!(check.evidence.is_some(), expects_evidence);
            assert_eq!(
                serde_json::to_value(check)
                    .unwrap()
                    .get("evidence")
                    .is_some(),
                expects_evidence
            );
        }
    }

    #[test]
    fn legacy_check_json_without_evidence_deserializes() {
        let check: Check = serde_json::from_str(
            r#"{"name":"Codex setup","status":"pass","message":"current","fix_hint":null}"#,
        )
        .unwrap();
        assert!(check.evidence.is_none());
    }

    #[test]
    fn human_provider_paths_escape_terminal_controls_and_bidi() {
        let escaped =
            escape_provider_path("/work/line\n\r\t\u{001b}\u{200d}\u{202e}\u{2060}\u{feff}界.json");
        for character in [
            '\n', '\r', '\t', '\u{001b}', '\u{200d}', '\u{202e}', '\u{2060}', '\u{feff}',
        ] {
            assert!(!escaped.contains(character));
        }
        for escape in [
            "\\u{a}",
            "\\u{d}",
            "\\u{9}",
            "\\u{1b}",
            "\\u{200d}",
            "\\u{202e}",
            "\\u{2060}",
            "\\u{feff}",
        ] {
            assert!(escaped.contains(escape), "{escaped}");
        }
        assert!(escaped.contains('界'));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_provider_path_serializes_with_lossy_marker() {
        use std::os::unix::ffi::OsStringExt;

        let file = ProviderHookFileInspection {
            path: PathBuf::from(std::ffi::OsString::from_vec(
                b"/work/invalid-\xff.json".to_vec(),
            )),
            scope: HookScope::Project,
            state: ProviderHookFileState::Invalid,
            ownership: ProviderHookOwnership::Unsupported,
            reason: Some(ProviderHookDiagnosticReason::UnsupportedTopology),
        };
        let evidence = ProviderFileEvidence::from(&file);
        assert!(evidence.path_lossy);
        assert!(serde_json::to_string(&evidence).is_ok());
    }

    #[test]
    fn human_and_json_provider_evidence_is_deterministic_and_safe() {
        let checks = [
            check_provider_setup(
                AgentProvider::Codex,
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available: true,
                    hooks: synthetic_inspection(
                        ProviderHookState::Current,
                        ProviderHookOwnership::Imperative,
                    ),
                },
            ),
            check_provider_setup(
                AgentProvider::Claude,
                ProviderSetupEvidence {
                    recorded: true,
                    executable_available: false,
                    hooks: synthetic_inspection(
                        ProviderHookState::Current,
                        ProviderHookOwnership::Imperative,
                    ),
                },
            ),
            check_provider_setup(
                AgentProvider::Antigravity,
                ProviderSetupEvidence {
                    recorded: false,
                    executable_available: false,
                    hooks: synthetic_inspection(
                        ProviderHookState::Missing,
                        ProviderHookOwnership::Absent,
                    ),
                },
            ),
        ];

        let human = render_checks(&checks);
        let json = render_checks_json(&checks).unwrap();

        assert!(human.find("Codex setup").unwrap() < human.find("Claude setup").unwrap());
        assert!(human.find("Claude setup").unwrap() < human.find("Antigravity setup").unwrap());
        assert!(json.len() < 2_048);
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
