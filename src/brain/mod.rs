pub mod activity;
pub mod autopsy;
pub mod baseline;
pub mod briefing;
pub mod client;
pub mod context;
pub mod decisions;
pub mod detectors;
pub mod diff_digest;
pub mod distill;
pub mod evals;
pub mod garden;
pub mod insights;
pub mod metrics;
pub mod outcomes;
pub mod permission_hook;
pub mod pref_store;
pub mod preferences;
pub mod prompts;
pub mod query;
pub mod recovery;
pub mod retrieval;
pub mod review;
pub mod risk;
pub mod safety;
pub mod sequences;

pub(crate) const UNSUPPORTED_PERMISSION_TOOL_REASON: &str = "unsupported permission tool";

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use coding_brain_core::runtime::BrainGateMode;

use crate::config::BrainConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateModeResolution {
    pub mode: BrainGateMode,
    pub warning: Option<String>,
}

/// Path to the Brain gate mode file in the Coding Brain state root.
pub fn gate_mode_path() -> PathBuf {
    coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    )
    .map(|paths| paths.state_root().join("brain/gate-mode"))
    .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain/brain/gate-mode"))
}

pub fn resolve_gate_mode(config: Option<&BrainConfig>) -> GateModeResolution {
    resolve_gate_mode_at(&gate_mode_path(), config)
}

#[allow(dead_code)] // Used by the settings command introduced in the next task.
pub fn write_gate_mode(mode: BrainGateMode) -> io::Result<()> {
    write_gate_mode_at(&gate_mode_path(), mode)
}

pub(crate) fn write_gate_mode_at(path: &Path, mode: BrainGateMode) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(temporary, "{}", mode.as_str())?;
    temporary.flush()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[allow(dead_code)] // The binary duplicates this module; its legacy CLI calls the library copy.
pub fn read_gate_mode() -> String {
    let config = crate::config::Config::load();
    resolve_gate_mode(config.brain.as_ref()).mode.to_string()
}

pub(crate) fn resolve_gate_mode_at(
    path: &Path,
    config: Option<&BrainConfig>,
) -> GateModeResolution {
    match std::fs::read_to_string(path) {
        Ok(value) => match value.trim() {
            "off" => resolved_mode(BrainGateMode::Off),
            "on" => resolved_mode(BrainGateMode::On),
            "auto" => resolved_mode(BrainGateMode::Auto),
            invalid => GateModeResolution {
                mode: BrainGateMode::Off,
                warning: Some(format!(
                    "invalid Brain gate mode {invalid:?} in {}",
                    path.display()
                )),
            },
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            resolved_mode(legacy_gate_mode(config))
        }
        Err(error) => GateModeResolution {
            mode: BrainGateMode::Off,
            warning: Some(format!(
                "could not read Brain gate mode from {}: {error}",
                path.display()
            )),
        },
    }
}

fn legacy_gate_mode(config: Option<&BrainConfig>) -> BrainGateMode {
    match config {
        Some(config) if !config.legacy_mode_configured => BrainGateMode::Off,
        Some(config) if !config.enabled => BrainGateMode::Off,
        Some(config) if config.auto_mode => BrainGateMode::Auto,
        Some(_) => BrainGateMode::On,
        None => BrainGateMode::Off,
    }
}

fn resolved_mode(mode: BrainGateMode) -> GateModeResolution {
    GateModeResolution {
        mode,
        warning: None,
    }
}

#[cfg(test)]
mod tests {
    use coding_brain_core::runtime::BrainGateMode;

    use super::*;
    use crate::config::BrainConfig;

    #[test]
    fn explicit_mode_wins_over_legacy_config() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::write(&path, "auto").unwrap();
        let legacy = BrainConfig {
            enabled: false,
            auto_mode: false,
            ..BrainConfig::default()
        };

        let resolved = resolve_gate_mode_at(&path, Some(&legacy));

        assert_eq!(resolved.mode, BrainGateMode::Auto);
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn missing_state_uses_legacy_config_then_defaults_off() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        let advisory = BrainConfig {
            enabled: true,
            legacy_mode_configured: true,
            auto_mode: false,
            ..BrainConfig::default()
        };
        let automatic = BrainConfig {
            enabled: true,
            legacy_mode_configured: true,
            auto_mode: true,
            ..BrainConfig::default()
        };

        assert_eq!(resolve_gate_mode_at(&path, None).mode, BrainGateMode::Off);
        assert_eq!(
            resolve_gate_mode_at(&path, Some(&BrainConfig::default())).mode,
            BrainGateMode::Off
        );
        assert_eq!(
            resolve_gate_mode_at(&path, Some(&advisory)).mode,
            BrainGateMode::On
        );
        assert_eq!(
            resolve_gate_mode_at(&path, Some(&automatic)).mode,
            BrainGateMode::Auto
        );
    }

    #[test]
    fn invalid_explicit_state_fails_closed_without_rewriting() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::write(&path, "automatic").unwrap();

        let resolved = resolve_gate_mode_at(&path, Some(&BrainConfig::default()));

        assert_eq!(resolved.mode, BrainGateMode::Off);
        assert!(resolved.warning.as_deref().unwrap().contains("automatic"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "automatic");
    }

    #[test]
    fn unreadable_explicit_state_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::create_dir(&path).unwrap();

        let resolved = resolve_gate_mode_at(&path, Some(&BrainConfig::default()));

        assert_eq!(resolved.mode, BrainGateMode::Off);
        assert!(resolved.warning.is_some());
    }

    #[test]
    fn non_directory_parent_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("brain");
        std::fs::write(&parent, "occupied").unwrap();
        let legacy = BrainConfig {
            enabled: true,
            auto_mode: false,
            ..BrainConfig::default()
        };

        let resolved = resolve_gate_mode_at(&parent.join("gate-mode"), Some(&legacy));

        assert_eq!(resolved.mode, BrainGateMode::Off);
        assert!(resolved.warning.is_some());
    }

    #[test]
    fn writer_persists_every_mode_explicitly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("brain/gate-mode");

        write_gate_mode_at(&path, BrainGateMode::On).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "on\n");

        write_gate_mode_at(&path, BrainGateMode::Auto).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "auto\n");
    }
}
