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
pub mod permission_hook;
pub mod permission_request_lock;
pub mod permission_transaction;
pub mod pref_store;
pub mod preferences;
pub mod prompts;
pub mod query;
pub mod recovery;
pub mod retrieval;
pub mod review;
pub(crate) mod review_state;
pub mod risk;
pub mod safety;
pub(crate) mod secure_state;
pub mod sequences;
#[doc(hidden)]
pub mod storage;

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

    #[test]
    fn sqlite_learning_page_records_drive_every_pure_consumer_seam() {
        use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
        use coding_brain_core::provider::AgentProvider;

        use super::decisions::{DecisionRecord, DecisionType};
        use super::storage::{ActivityCursor, DecisionKind, DecisionPayload, LearningDecisionPage};

        let record = DecisionRecord {
            provider: AgentProvider::Codex,
            timestamp: "2026-08-04T12:00:00Z".into(),
            pid: 1,
            project: "alpha".into(),
            tool: Some("Bash".into()),
            command: Some("cargo test".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "fixture".into(),
            user_action: "accept".into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some("decision-1".into()),
            brain_decision_ms: Some(1),
            cache_hit: Some(false),
            canonical: Some(false),
        };
        let records = LearningDecisionPage {
            decisions: vec![DecisionPayload::new(
                DecisionKind::Observation,
                ActivityCursor::try_from(1_u64).unwrap(),
                record,
            )],
            next_cursor: None,
            serialized_bytes: 1,
        }
        .into_records();

        assert_eq!(
            baseline::rules_baseline_classify(
                records[0].tool.as_deref(),
                records[0].command.as_deref()
            ),
            "approve"
        );
        assert_eq!(
            briefing::filter_recent_for_project(&records, Some("alpha")).len(),
            1
        );
        assert_eq!(
            retrieval::retrieve_similar_from(&records, Some("Bash"), "alpha", 1, None).len(),
            1
        );
        assert_eq!(metrics::summaries_from_decisions(&records).len(), 1);
        let preferences = preferences::distill_preferences(&records);
        let _ = insights::generate_insights(&records, &preferences);

        let temp = tempfile::tempdir().unwrap();
        let paths = CodingBrainPaths::resolve(&PathEnvironment::new(
            Some(temp.path().join("config")),
            Some(temp.path().join("state")),
            Some(temp.path().to_path_buf()),
        ))
        .unwrap();
        assert!(matches!(
            distill::run_once_with_inputs(&paths, &records, &records).unwrap(),
            distill::DistillOutcome::NotDue { .. }
        ));
    }
}
