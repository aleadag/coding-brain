//! Coding Brain's durable onboarding record.
//! ran, when, and against which Coding Brain version.
//!
//! The marker exists so:
//!
//! * `coding-brain init` (no args) on an already-onboarded environment can skip
//!   the wizard and report status instead of re-prompting.
//! * `coding-brain init --check` has a baseline to diff against environment
//!   detection (drift = recorded as installed but no longer detected).
//! * `coding-brain init --remove` knows exactly which artifacts to clean up.
//!
//! Lives outside the SQLite stores (coord, bus, history) so it's
//! human-readable and trivially deletable when someone wants to factory-reset
//! their Coding Brain install.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use coding_brain_core::provider::AgentProvider;
use serde::{Deserialize, Serialize};

/// Snapshot of a single phase's recorded outcome.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseRecord {
    /// Last status string we wrote — see `PhaseStatus::label` in `state.rs`.
    pub status: String,
    /// Free-form one-liner the phase wants to remember (a URL, a budget, a
    /// settings path, the role bindings count). Used to render `--check`.
    #[serde(default)]
    pub details: Option<String>,
    /// ISO timestamp when this phase was last applied.
    #[serde(default)]
    pub applied_at: Option<String>,
}

/// Full marker contents. New fields should be added with `#[serde(default)]`
/// so older marker files still parse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OnboardingMarker {
    /// Coding Brain version that last completed onboarding.
    pub version: String,
    /// ISO timestamp when onboarding last completed.
    pub completed_at: String,
    /// Per-phase records keyed by phase id (`budget`, `brain`, …).
    #[serde(default)]
    pub phases: std::collections::BTreeMap<String, PhaseRecord>,
}

impl OnboardingMarker {
    /// Providers explicitly represented by provider hook phase keys. Legacy
    /// `plugin` records are projected as Codex without rewriting the marker.
    pub fn recorded_providers(&self) -> Vec<AgentProvider> {
        let mut providers = Vec::new();
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let key = format!("hooks.{}", provider.as_str());
            if self.phases.contains_key(&key)
                || (provider == AgentProvider::Codex && self.phases.contains_key("plugin"))
            {
                providers.push(provider);
            }
        }
        providers
    }

    /// Providers whose hook phase was recorded as installed.
    pub fn selected_providers(&self) -> Vec<AgentProvider> {
        self.recorded_providers()
            .into_iter()
            .filter(|provider| {
                let key = format!("hooks.{}", provider.as_str());
                self.phases
                    .get(&key)
                    .or_else(|| {
                        (*provider == AgentProvider::Codex)
                            .then(|| self.phases.get("plugin"))
                            .flatten()
                    })
                    .is_some_and(|record| record.status == "installed")
            })
            .collect()
    }

    /// Providers whose prior hook phase should be retried during upgrade.
    /// A stable provider key takes precedence over the legacy `plugin` key.
    pub fn upgrade_providers(&self) -> Vec<AgentProvider> {
        let selected = self.selected_providers();
        self.recorded_providers()
            .into_iter()
            .filter(|provider| {
                selected.contains(provider)
                    || self
                        .provider_record(*provider)
                        .is_some_and(|record| record.status == "drift")
            })
            .collect()
    }

    pub fn provider_record(&self, provider: AgentProvider) -> Option<&PhaseRecord> {
        let key = format!("hooks.{}", provider.as_str());
        self.phases.get(&key).or_else(|| {
            (provider == AgentProvider::Codex)
                .then(|| self.phases.get("plugin"))
                .flatten()
        })
    }
}

/// Default location: `<state-root>/onboarding.json`. Used in production;
/// tests inject their own path.
pub fn default_path() -> PathBuf {
    coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    )
    .map(|paths| paths.state_root().join("onboarding.json"))
    .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain/onboarding.json"))
}

/// Load the marker, returning `None` when the file doesn't exist (i.e. the
/// user has never run `init`). Invalid JSON returns `Ok(None)` rather than
/// erroring so a corrupted marker never blocks a fresh init pass.
pub fn load(path: &Path) -> io::Result<Option<OnboardingMarker>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw).ok())
}

/// Save the marker atomically: write to a sibling temp file then rename, so a
/// crash mid-write never leaves a half-written marker.
pub fn save(path: &Path, marker: &OnboardingMarker) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(marker)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(&tmp, format!("{json}\n"))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Delete the marker. Idempotent — missing file is success.
pub fn clear(path: &Path) -> io::Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        assert!(load(&p).unwrap().is_none());
    }

    #[test]
    fn save_then_load_roundtrips() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nested").join("onboarding.json");
        let mut m = OnboardingMarker {
            version: "0.99.0".into(),
            completed_at: "2026-06-06T00:00:00Z".into(),
            ..Default::default()
        };
        m.phases.insert(
            "budget".into(),
            PhaseRecord {
                status: "installed".into(),
                details: Some("$50/wk".into()),
                applied_at: Some("2026-06-06T00:00:00Z".into()),
            },
        );
        save(&p, &m).unwrap();
        let loaded = load(&p).unwrap().expect("present");
        assert_eq!(loaded.version, "0.99.0");
        assert_eq!(loaded.phases["budget"].details.as_deref(), Some("$50/wk"));
    }

    #[test]
    fn load_returns_none_on_invalid_json() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        fs::write(&p, "{not json").unwrap();
        // Corrupted marker should NOT error — we treat it as missing so
        // `init` can recover by overwriting it.
        assert!(load(&p).unwrap().is_none());
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("onboarding.json");
        clear(&p).unwrap(); // missing — OK
        fs::write(&p, "{}").unwrap();
        clear(&p).unwrap(); // present — removed
        assert!(!p.exists());
        clear(&p).unwrap(); // missing again — OK
    }

    #[test]
    fn selected_providers_use_stable_hook_keys_and_project_legacy_plugin_to_codex() {
        let installed = PhaseRecord {
            status: "installed".into(),
            ..Default::default()
        };
        let mut current = OnboardingMarker::default();
        current
            .phases
            .insert("hooks.claude".into(), installed.clone());
        current
            .phases
            .insert("hooks.antigravity".into(), installed.clone());
        assert_eq!(
            current.selected_providers(),
            vec![AgentProvider::Claude, AgentProvider::Antigravity]
        );

        let mut legacy = OnboardingMarker::default();
        legacy.phases.insert("plugin".into(), installed);
        assert_eq!(legacy.selected_providers(), vec![AgentProvider::Codex]);
        assert!(!legacy.phases.contains_key("hooks.codex"));
    }

    #[test]
    fn upgrade_providers_include_installed_and_drift_with_stable_override() {
        let record = |status: &str| PhaseRecord {
            status: status.into(),
            ..Default::default()
        };
        let mut marker = OnboardingMarker::default();
        marker.phases.insert("plugin".into(), record("drift"));
        marker
            .phases
            .insert("hooks.codex".into(), record("skipped"));
        marker.phases.insert("hooks.claude".into(), record("drift"));
        marker
            .phases
            .insert("hooks.antigravity".into(), record("not_installed"));
        assert_eq!(marker.upgrade_providers(), vec![AgentProvider::Claude]);

        marker.phases.remove("hooks.codex");
        assert_eq!(
            marker.upgrade_providers(),
            vec![AgentProvider::Codex, AgentProvider::Claude]
        );
    }
}
