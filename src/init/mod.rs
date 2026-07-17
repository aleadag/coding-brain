//! `coding-brain init` — opinionated onboarding wizard.
//!
//! Tracking issue: <https://github.com/aleadag/codexctl/issues/257>.
//!
//! This module owns the single canonical first-run flow for getting a
//! Coding Brain install ready: local-LLM brain detection, Codex hook install, and
//! curated skill suggestions.
//!
//! Public surface:
//!
//! * [`run_wizard`] — interactive flow. The default `coding-brain init`.
//! * [`run_non_interactive`] — same flow with pre-filled answers. For CI and
//!   dotfile automation.
//! * [`run_check`] — drift report comparing the recorded marker against
//!   current environment detection.
//! * [`run_remove`] — uninstall every Coding Brain-managed artifact.
//! * [`run_reset`] — clear the marker so the next `init` run prompts again.
//!
//! Module layout:
//!
//! * `hooks.rs` — managed Codex hook installation.
//! * `marker.rs` — Coding Brain onboarding marker read/write.
//! * `prompt.rs` — minimal stdin/stdout prompt helpers.
//! * `state.rs` — environment detection (probes ollama and hooks.json).
//! * `phases.rs` — `Phase` trait + Brain/Plugin/Skills impls.

pub mod hooks;
pub mod marker;
pub mod phases;
pub mod prompt;
pub mod state;

use std::io;
use std::path::{Component, Path, PathBuf};

use codexctl_core::paths::{CodingBrainPaths, PathEnvironment};

use marker::{OnboardingMarker, PhaseRecord};
use phases::{Answers, Phase};
use state::PhaseStatus;

/// Interactive wizard — walks every phase in `registry()` order, prompts,
/// applies, and updates the onboarding marker.
pub fn run_wizard() -> io::Result<()> {
    let registry = phases::registry();
    print_banner(&registry);

    let report = state::EnvironmentReport::detect();
    println!("Current state:");
    print!("{}", report.render_human());

    let total = registry.len();
    let mut new_records = std::collections::BTreeMap::new();
    let stamp = timestamp_now();

    for (idx, phase) in registry.iter().enumerate() {
        prompt::section_header(idx + 1, total, phase.label());
        let status = phase.run_interactive()?;
        print_outcome(phase.label(), &status);
        new_records.insert(
            phase.id().to_string(),
            phases::record_from_status(&status, &stamp),
        );
    }

    ensure_project_identity()?;
    persist_marker(new_records, &stamp)?;
    println!();
    println!(
        "Onboarding complete. Re-run with `coding-brain init --check` any time to inspect drift."
    );
    Ok(())
}

/// Non-interactive wizard. Same phases, no prompts. Skipped phases produce
/// a `PhaseStatus::Skipped` record so `--check` knows the difference between
/// "not configured because you don't want it" and "should be configured but
/// isn't yet."
pub fn run_non_interactive(answers: &Answers) -> io::Result<()> {
    let registry = phases::registry();
    let mut new_records = std::collections::BTreeMap::new();
    let stamp = timestamp_now();
    for phase in &registry {
        let status = phase.run_non_interactive(answers)?;
        println!(
            "{label}: {status_label}{detail}",
            label = phase.label(),
            status_label = status.label(),
            detail = status
                .details()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default(),
        );
        new_records.insert(
            phase.id().to_string(),
            phases::record_from_status(&status, &stamp),
        );
    }
    ensure_project_identity()?;
    persist_marker(new_records, &stamp)?;
    Ok(())
}

fn ensure_project_identity() -> io::Result<()> {
    let paths = CodingBrainPaths::resolve(&PathEnvironment::current())
        .map_err(|error| io::Error::other(format!("path resolution failed: {error:?}")))?;
    let cwd = std::env::current_dir()?;
    codexctl_core::project::ProjectManifest::create(&cwd, &paths)
        .map(|_| ())
        .map_err(|error| io::Error::other(format!("project identity setup failed: {error}")))
}

/// Drift report: detect each phase's current state and diff against the
/// marker. Exits with code 1 (via returned `io::Result`) when drift is
/// detected so CI can gate on `init --check`.
pub fn run_check() -> io::Result<()> {
    let registry = phases::registry();
    let recorded = marker::load(&marker::default_path())?;

    if recorded.is_none() {
        println!("Coding Brain has not been onboarded — run `coding-brain init` to begin.");
        return Err(io::Error::other("not onboarded"));
    }
    let recorded = recorded.unwrap();

    println!("coding-brain init --check");
    println!(
        "  recorded version : {}",
        if recorded.version.is_empty() {
            "(unknown)"
        } else {
            &recorded.version
        }
    );
    println!("  last completed   : {}", recorded.completed_at);
    println!();
    println!("Phase status (current → recorded):");

    let mut drift_count = 0;
    for phase in &registry {
        let current = phase.detect();
        let recorded_status = recorded
            .phases
            .get(phase.id())
            .map(|r| r.status.as_str())
            .unwrap_or("(no record)");

        let drifted = is_drift(&current, recorded_status);
        let marker_char = if drifted { "⚠" } else { "✓" };
        let current_detail = current.details().unwrap_or("");
        println!(
            "  {marker} {label:<10} {cur:<14} ← {rec}{detail}",
            marker = marker_char,
            label = phase.id(),
            cur = current.label(),
            rec = recorded_status,
            detail = if current_detail.is_empty() {
                String::new()
            } else {
                format!("   [{current_detail}]")
            }
        );
        if drifted {
            drift_count += 1;
        }
    }

    if drift_count > 0 {
        println!();
        println!("⚠  {drift_count} phase(s) have drifted from the recorded onboarding.");
        println!(
            "   Run `coding-brain init` to re-apply, or `coding-brain init --reset` to start over."
        );
        return Err(io::Error::other(format!("{drift_count} phase(s) drifted")));
    }
    println!();
    println!("✓ all phases match the recorded state.");
    Ok(())
}

/// Remove every Coding Brain-managed artifact without erasing user-owned setup.
pub fn run_remove() -> io::Result<()> {
    let registry = phases::registry();
    let mut errors = Vec::new();
    for phase in &registry {
        if let Err(e) = phase.remove() {
            errors.push(format!("{}: {e}", phase.id()));
        } else {
            println!("  removed: {}", phase.label());
        }
    }
    marker::clear(&marker::default_path())?;
    println!("Cleared onboarding marker.");
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "remove errors: {}",
            errors.join("; ")
        )))
    }
}

/// Clear the marker so the next `init` run prompts again. Does NOT touch any
/// installed artifacts.
pub fn run_reset() -> io::Result<()> {
    marker::clear(&marker::default_path())?;
    println!("Cleared onboarding marker — `coding-brain init` will start from scratch next run.");
    Ok(())
}

/// Re-sync everything the previous `init` wrote so it tracks the current
/// binary (#327). Used after upgrading or reinstalling `coding-brain` — the
/// new binary may expect a different schema and might
/// have a fresher marker version, but the on-disk artifacts were written by
/// the old binary.
///
/// Two refresh paths, in order — failures don't abort the rest so a
/// half-broken install can still partially recover:
///
/// 1. Hook entries in `~/.codex/hooks.json` — re-runs `init::hooks::run_init`
///    which is idempotent.
/// 2. Onboarding marker version bump — if the recorded version differs
///    from the running binary's `CARGO_PKG_VERSION`, rewrite the version
///    field (other phase records preserved).
pub fn run_upgrade() -> io::Result<()> {
    println!("coding-brain init upgrade");
    println!("======================");
    println!();

    let mut had_error = false;

    // 1. Hook entries. `hooks::run_init` prints its own report; we follow
    // it with our progress line so the operator sees both the file path
    // touched and the per-step ✓ summary.
    println!("  [1/2] Codex hook entries");
    match hooks::run_init(false, false) {
        Ok(()) => println!("        \u{2713} refreshed"),
        Err(e) => {
            println!("        \u{2717} {e}");
            had_error = true;
        }
    }

    // 2. Onboarding marker version stamp
    print!("  [2/2] Onboarding marker version ................. ");
    match upgrade_marker_version() {
        Ok(Some((from, to))) => println!("\u{2713} {from} \u{2192} {to}"),
        Ok(None) => println!("\u{2014} already current"),
        Err(e) => {
            println!("\u{2717} {e}");
            had_error = true;
        }
    }

    println!();
    if had_error {
        return Err(io::Error::other(
            "one or more upgrade steps failed — run `coding-brain doctor` for details",
        ));
    }
    println!("Upgrade complete. Run `coding-brain doctor` to verify.");
    Ok(())
}

/// Bump the marker's `version` field to the running binary's
/// `CARGO_PKG_VERSION` when they differ. Returns `Some((from, to))` on
/// bump, `None` when already current or when no marker exists yet (a
/// fresh install isn't an upgrade case).
fn upgrade_marker_version() -> io::Result<Option<(String, String)>> {
    let path = marker::default_path();
    let Some(mut m) = marker::load(&path)? else {
        return Ok(None);
    };
    let current = env!("CARGO_PKG_VERSION").to_string();
    if m.version == current {
        return Ok(None);
    }
    let from = std::mem::replace(&mut m.version, current.clone());
    marker::save(&path, &m)?;
    Ok(Some((from, current)))
}

pub fn run_purge(assume_yes: bool) -> io::Result<()> {
    let environment = PathEnvironment::current();
    let targets = preview_purge_targets(&environment)?;

    println!("This will delete:");
    println!("  • exact managed Coding Brain and legacy codexctl Codex hook entries");
    for target in &targets {
        println!(
            "  • {} ({})",
            target.path.display(),
            target.identity.label()
        );
    }
    println!();
    println!("User-edited files outside these paths are preserved. To remove only");
    println!("the hooks and onboarding marker (keep user data), use `init --remove`.");
    println!();

    if !assume_yes && !prompt::yes_no("Proceed with purge?", false)? {
        println!("Aborted.");
        return Ok(());
    }

    let remove_errors = match remove_managed_hooks_silent() {
        Ok(()) => Vec::new(),
        Err(e) => vec![format!("hook/marker removal: {e}")],
    };

    let mut errors = remove_errors;
    for target in &targets {
        if let Err(error) = remove_previewed_target(target) {
            errors.push(format!("{}: {error}", target.path.display()));
        } else {
            println!("  removed: {}", target.path.display());
        }
    }

    if errors.is_empty() {
        println!();
        println!("Purge complete. `coding-brain init` will start fresh.");
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "purge errors: {}",
            errors.join("; ")
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PurgeIdentity {
    Missing,
    File(u128),
    Directory(u128),
    Symlink(u128),
}

impl PurgeIdentity {
    fn label(&self) -> &'static str {
        match self {
            Self::Missing => "not present",
            Self::File(_) => "file",
            Self::Directory(_) => "directory",
            Self::Symlink(_) => "symlink",
        }
    }
}

#[derive(Debug, Clone)]
struct PurgeTarget {
    path: PathBuf,
    identity: PurgeIdentity,
}

fn preview_purge_targets(environment: &PathEnvironment) -> io::Result<Vec<PurgeTarget>> {
    for (name, base) in [
        ("HOME", environment.home()),
        ("XDG_CONFIG_HOME", environment.xdg_config_home()),
        ("XDG_STATE_HOME", environment.xdg_state_home()),
    ] {
        if let Some(base) = base {
            validate_purge_base(name, base)?;
        }
    }
    let home = environment
        .home()
        .ok_or_else(|| io::Error::other("HOME is required for purge"))?;
    let paths = CodingBrainPaths::resolve(environment)
        .map_err(|error| io::Error::other(format!("unsafe purge environment: {error:?}")))?;
    [
        paths.state_root().to_path_buf(),
        paths.config_file().to_path_buf(),
        home.join(".codexctl"),
        home.join(".config/codexctl/config.toml"),
    ]
    .into_iter()
    .map(|path| {
        validate_purge_target(&path)?;
        Ok(PurgeTarget {
            identity: purge_identity(&path)?,
            path,
        })
    })
    .collect()
}

fn validate_purge_base(name: &str, base: &Path) -> io::Result<()> {
    if !base.is_absolute() || base.parent().is_none() || base == Path::new("/") {
        return Err(io::Error::other(format!(
            "{name} must be an absolute non-root path"
        )));
    }
    Ok(())
}

fn validate_purge_target(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(io::Error::other(format!(
            "refusing unsafe purge target {}",
            path.display()
        )));
    }
    Ok(())
}

fn purge_identity(path: &Path) -> io::Result<PurgeIdentity> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Ok(PurgeIdentity::Symlink(metadata_identity(&metadata)))
        }
        Ok(metadata) if metadata.is_dir() => {
            Ok(PurgeIdentity::Directory(metadata_identity(&metadata)))
        }
        Ok(metadata) => Ok(PurgeIdentity::File(metadata_identity(&metadata))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(PurgeIdentity::Missing),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> u128 {
    use std::os::unix::fs::MetadataExt;
    (u128::from(metadata.dev()) << 64) | u128::from(metadata.ino())
}

#[cfg(not(unix))]
fn metadata_identity(metadata: &std::fs::Metadata) -> u128 {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    modified ^ u128::from(metadata.len())
}

fn remove_previewed_target(target: &PurgeTarget) -> io::Result<()> {
    let current = purge_identity(&target.path)?;
    if current != target.identity {
        return Err(io::Error::other(
            "target identity or type changed after preview",
        ));
    }
    match current {
        PurgeIdentity::Missing => Ok(()),
        PurgeIdentity::Symlink(_) | PurgeIdentity::File(_) => std::fs::remove_file(&target.path),
        PurgeIdentity::Directory(_) => std::fs::remove_dir_all(&target.path),
    }
}

/// Remove only the managed hook entries. The marker lives under the previewed
/// state root, so purge must not access it before the target identity check.
fn remove_managed_hooks_silent() -> io::Result<()> {
    phases::PluginPhase.remove()
}

// ---------------- internal helpers ------------------------------------------

fn print_banner(registry: &[Box<dyn Phase>]) {
    println!();
    println!(
        "coding-brain init — opinionated onboarding ({} phases)",
        registry.len()
    );
    println!("══════════════════════════════════════════════════════════════");
    println!();
}

fn print_outcome(label: &str, status: &PhaseStatus) {
    match status {
        PhaseStatus::Installed { details } => prompt::phase_outcome(label, details),
        PhaseStatus::Skipped => prompt::phase_skipped(label, "user declined"),
        PhaseStatus::NotInstalled => prompt::phase_skipped(label, "not configured"),
        PhaseStatus::Drift { reason } => prompt::phase_skipped(label, reason),
    }
}

fn persist_marker(
    phase_records: std::collections::BTreeMap<String, PhaseRecord>,
    stamp: &str,
) -> io::Result<()> {
    let marker_value = OnboardingMarker {
        version: env!("CARGO_PKG_VERSION").to_string(),
        completed_at: stamp.to_string(),
        phases: phase_records,
    };
    marker::save(&marker::default_path(), &marker_value)
}

fn timestamp_now() -> String {
    crate::logger::timestamp_now()
}

/// Decide whether the current state diverges from what the marker recorded.
///
/// We treat "not_installed" and "skipped" as equivalent for drift purposes —
/// both mean "phase is not configured." Drift triggers only when:
///
/// * recorded "installed" but current state is missing it (artifact removed
///   out-of-band), or
/// * recorded "skipped"/"not_installed" but the current state now detects an
///   install (an artifact appeared since onboarding — could be intentional or
///   could be a stale install the user wants to clean up).
fn is_drift(current: &PhaseStatus, recorded_label: &str) -> bool {
    let current_label = current.label();
    let cur_configured = matches!(current_label, "installed");
    let rec_configured = matches!(recorded_label, "installed");
    cur_configured != rec_configured
}

#[cfg(test)]
mod drift_tests {
    use super::*;

    fn installed() -> PhaseStatus {
        PhaseStatus::Installed {
            details: "x".into(),
        }
    }

    #[test]
    fn not_installed_and_skipped_treated_as_equivalent() {
        assert!(!is_drift(&PhaseStatus::NotInstalled, "skipped"));
        assert!(!is_drift(&PhaseStatus::Skipped, "not_installed"));
        assert!(!is_drift(&PhaseStatus::NotInstalled, "not_installed"));
        assert!(!is_drift(&PhaseStatus::Skipped, "skipped"));
    }

    #[test]
    fn matched_installed_is_not_drift() {
        assert!(!is_drift(&installed(), "installed"));
    }

    #[test]
    fn installed_then_missing_is_drift() {
        assert!(is_drift(&PhaseStatus::NotInstalled, "installed"));
        assert!(is_drift(&PhaseStatus::Skipped, "installed"));
    }

    #[test]
    fn unexpected_install_is_drift() {
        assert!(is_drift(&installed(), "skipped"));
        assert!(is_drift(&installed(), "not_installed"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_preserves_legacy_state() {
        use std::ffi::OsString;

        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let old_home: Option<OsString> = std::env::var_os("HOME");
        let home = tempfile::tempdir().unwrap();
        // SAFETY: HOME mutation is serialized by HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", home.path()) };

        let sentinels = [
            (".codexctl/coord/coord.db", b"coord".as_slice()),
            (".codexctl/bus/bus.db", b"bus".as_slice()),
            (".codexctl/hive/store.json", b"hive".as_slice()),
            (".codexctl/relay/identity.json", b"relay".as_slice()),
            (".codexctl/loop/loop.db", b"loop".as_slice()),
        ];
        for (relative, contents) in sentinels {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        crate::config::Config::load();
        let _ = state::EnvironmentReport::detect();
        run_upgrade().unwrap();

        for (relative, contents) in sentinels {
            assert_eq!(std::fs::read(home.path().join(relative)).unwrap(), contents);
        }

        match old_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn upgrade_marker_helper_bumps_a_stale_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("onboarding.json");
        let m = marker::OnboardingMarker {
            version: "0.0.1".into(),
            completed_at: "2026-01-01T00:00:00Z".into(),
            phases: Default::default(),
        };
        marker::save(&path, &m).unwrap();
        // Re-read + bump
        let mut loaded = marker::load(&path).unwrap().unwrap();
        let from = std::mem::replace(&mut loaded.version, "0.99.0".into());
        marker::save(&path, &loaded).unwrap();
        let after = marker::load(&path).unwrap().unwrap();
        assert_eq!(from, "0.0.1");
        assert_eq!(after.version, "0.99.0");
    }

    #[test]
    fn purge_targets_are_exact_and_preserve_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let config = temp.path().join("config");
        let state = temp.path().join("state");
        std::fs::create_dir_all(state.join("coding-brain")).unwrap();
        std::fs::create_dir_all(config.join("coding-brain")).unwrap();
        std::fs::create_dir_all(home.join(".codexctl")).unwrap();
        std::fs::create_dir_all(home.join(".config/codexctl")).unwrap();
        std::fs::write(config.join("coding-brain/config.toml"), "current").unwrap();
        std::fs::write(home.join(".config/codexctl/config.toml"), "legacy").unwrap();
        std::fs::write(config.join("sibling.toml"), "keep").unwrap();

        let environment = PathEnvironment::new(Some(config.clone()), Some(state), Some(home));
        let targets = preview_purge_targets(&environment).unwrap();
        assert_eq!(targets.len(), 4);
        for target in &targets {
            remove_previewed_target(target).unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(config.join("sibling.toml")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn purge_rejects_unsafe_or_changed_targets() {
        let relative = PathEnvironment::new(None, None, Some(PathBuf::from("relative")));
        assert!(preview_purge_targets(&relative).is_err());
        let root = PathEnvironment::new(None, None, Some(PathBuf::from("/")));
        assert!(preview_purge_targets(&root).is_err());

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let environment = PathEnvironment::new(None, None, Some(home));
        let targets = preview_purge_targets(&environment).unwrap();
        let config_target = &targets[1];
        std::fs::create_dir_all(config_target.path.parent().unwrap()).unwrap();
        std::fs::write(&config_target.path, "appeared after preview").unwrap();
        assert!(remove_previewed_target(config_target).is_err());
        assert!(config_target.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn purge_unlinks_symlink_without_following_it() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep"), "safe").unwrap();
        let link = temp.path().join("state-link");
        symlink(&outside, &link).unwrap();
        let target = PurgeTarget {
            identity: purge_identity(&link).unwrap(),
            path: link.clone(),
        };

        remove_previewed_target(&target).unwrap();
        assert!(!link.exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "safe"
        );
    }

    #[test]
    fn non_interactive_records_marker_for_every_phase() {
        // Drive the wizard in non-interactive mode with all-skip answers and
        // assert the marker captures one record per phase.
        let registry = phases::registry();
        let answers = Answers {
            skip_brain: true,
            install_plugin: Some(false),
            skip_skills: true,
            ..Answers::default()
        };

        let mut records = std::collections::BTreeMap::new();
        let stamp = "2026-06-06T00:00:00Z";
        for phase in &registry {
            let status = phase.run_non_interactive(&answers).unwrap();
            records.insert(
                phase.id().to_string(),
                phases::record_from_status(&status, stamp),
            );
        }
        // Three entries, one per phase, all skipped.
        assert_eq!(records.len(), 3);
        for record in records.values() {
            assert_eq!(record.status, "skipped");
        }
    }
}
