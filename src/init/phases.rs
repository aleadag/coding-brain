//! Onboarding phases. Each phase is a self-contained step the wizard walks:
//! detect current state → ask the user → apply if accepted → record outcome.
//!
//! Phases share one `Phase` trait so the wizard, `init --check`, and
//! `init --remove` all walk the same registry without per-phase branching.

use std::io;

use coding_brain_core::provider::AgentProvider;

use super::hooks;
use super::marker::PhaseRecord;
use super::prompt;
use super::state::{self, PhaseStatus};

/// Pre-filled answers for the non-interactive path. The wizard either reads
/// these or asks the user; both forms produce the same outcome.
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct Answers {
    pub brain_url: Option<String>,
    pub skip_brain: bool,

    pub install_plugin: Option<bool>,

    pub skip_skills: bool,
}

/// Single uniform shape across all phases.
pub trait Phase {
    /// Stable identifier — keys `onboarding.json`'s `phases` map. Never
    /// rename without a migration.
    fn id(&self) -> &'static str;

    /// One-line label used in section headers and `--check` output.
    fn label(&self) -> &'static str;

    /// What's there now?
    fn detect(&self) -> PhaseStatus;

    /// Interactive run. Calls into `prompt::*`. Implementations should:
    /// 1. ask any phase-specific questions (with sensible defaults),
    /// 2. perform the install/configure work,
    /// 3. return the resulting `PhaseStatus`.
    fn run_interactive(&self) -> io::Result<PhaseStatus>;

    /// Non-interactive equivalent: take pre-filled answers, do the same work,
    /// return the same status. No prompting.
    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus>;

    /// Tear down whatever this phase installed. Idempotent.
    fn remove(&self) -> io::Result<()>;
}

/// Convert a status into a marker record. Callers stamp `applied_at`
/// themselves so the timestamp ties to the wizard's clock.
pub fn record_from_status(status: &PhaseStatus, applied_at: &str) -> PhaseRecord {
    PhaseRecord {
        status: status.label().to_string(),
        details: status.details().map(String::from),
        applied_at: Some(applied_at.to_string()),
    }
}

/// The full ordered registry the wizard walks. Skills stay last because they
/// are optional.
pub fn registry() -> Vec<Box<dyn Phase>> {
    vec![
        Box::new(BrainPhase),
        Box::new(PluginPhase),
        Box::new(SkillsPhase),
    ]
}

// ===================== Brain (local LLM) ================================

pub struct BrainPhase;

impl Phase for BrainPhase {
    fn id(&self) -> &'static str {
        "brain"
    }
    fn label(&self) -> &'static str {
        "Local-LLM brain auto-pilot"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_brain()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        println!(
            "The brain learns your preferences and can approve/deny tool calls automatically."
        );
        println!("Requires a local LLM endpoint (ollama / llama.cpp / LM Studio / vLLM).");

        let detected = state::detect_brain();
        if let PhaseStatus::Installed { details } = &detected {
            println!("  Detected: {details}");
            if prompt::yes_no("Use this endpoint?", true)? {
                return Ok(detected);
            }
        } else {
            // #324 — print a concrete install hint when no endpoint is
            // reachable, instead of silently moving on. Most users hitting
            // this won't know ollama exists.
            print_ollama_install_hint();
        }

        if !prompt::yes_no("Configure a custom endpoint?", false)? {
            return Ok(PhaseStatus::Skipped);
        }
        let url = prompt::line_or_default("  Endpoint URL", Some("http://localhost:11434"))?
            .unwrap_or_default();
        Ok(PhaseStatus::Installed {
            details: format!("endpoint at {url}"),
        })
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_brain {
            return Ok(PhaseStatus::Skipped);
        }
        if let Some(url) = &answers.brain_url {
            return Ok(PhaseStatus::Installed {
                details: format!("endpoint at {url}"),
            });
        }
        let status = state::detect_brain();
        // #324 — even non-interactive mode should surface the install hint
        // (printed once, doesn't change the recorded status). CI / dotfile
        // users skim the output; they shouldn't have to guess why brain
        // recorded `not_installed`.
        if !matches!(status, PhaseStatus::Installed { .. }) {
            print_ollama_install_hint();
        }
        Ok(status)
    }

    fn remove(&self) -> io::Result<()> {
        // We don't shut down the user's ollama install. Marker record drop is
        // handled by the orchestrator.
        Ok(())
    }
}

/// Three-line install hint shown when the Brain phase can't reach any
/// local-LLM endpoint. Mirrors `docs/quickstart.md` "Optional: add the
/// local LLM brain" so the wizard and the docs say the same thing.
fn print_ollama_install_hint() {
    println!("  No local-LLM endpoint detected on the common ports.");
    println!("  To enable the brain, install ollama and a small model:");
    println!("    brew install ollama && ollama serve &");
    println!("    ollama pull gemma4:e4b");
    println!("  Then re-run `coding-brain init` to wire it up.");
}

// ===================== Codex hooks ======================================

pub struct PluginPhase;

impl Phase for PluginPhase {
    fn id(&self) -> &'static str {
        "plugin"
    }
    fn label(&self) -> &'static str {
        "Codex hooks"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_plugin()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        println!("Wire Coding Brain hooks into ~/.codex/hooks.json. Existing hooks are preserved.");
        if !prompt::yes_no("Install Codex hooks?", true)? {
            return Ok(PhaseStatus::Skipped);
        }
        install_plugin_hooks()?;
        Ok(self.detect())
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        match answers.install_plugin {
            Some(true) => {
                install_plugin_hooks()?;
                Ok(self.detect())
            }
            Some(false) => Ok(PhaseStatus::Skipped),
            None => {
                // Unspecified non-interactive = install (the wizard's default).
                install_plugin_hooks()?;
                Ok(self.detect())
            }
        }
    }

    fn remove(&self) -> io::Result<()> {
        hooks::run_uninit(false)
    }
}

fn install_plugin_hooks() -> io::Result<()> {
    hooks::run_init(false, false)
}

/// Public entry for the `init plugin-only` shortcut: write hook entries without
/// running the rest of the wizard. Kept under the old name for CLI stability.
pub fn install_plugin_now() -> io::Result<()> {
    install_plugin_hooks()
}

pub fn install_provider_hooks(providers: &[AgentProvider]) -> io::Result<Vec<PhaseStatus>> {
    let plans = super::provider_hooks::stage_provider_hooks(
        providers,
        super::provider_hooks::HookScope::Global,
    )?;
    report_preserved_provider_entries(&plans);
    super::provider_hooks::apply_hook_transaction(&plans)?;
    Ok(providers
        .iter()
        .copied()
        .map(state::detect_provider_hooks)
        .collect())
}

pub fn remove_provider_hooks(providers: &[AgentProvider]) -> io::Result<()> {
    let plans = super::provider_hooks::stage_provider_hook_removal(
        providers,
        super::provider_hooks::HookScope::Global,
    )?;
    report_preserved_provider_entries(&plans);
    super::provider_hooks::apply_hook_transaction(&plans)
}

fn report_preserved_provider_entries(plans: &[super::provider_hooks::ProviderHookPlan]) {
    for plan in plans {
        for entry in &plan.preserved_modified_entries {
            eprintln!(
                "Preserved user-modified {} hook entry: {entry}",
                plan.provider
            );
        }
    }
}

// ===================== Skills ============================================

/// Suggestions only — we don't shell into Codex's plugin installer.
const SUGGESTED_SKILLS: &[(&str, &str)] = &[
    ("humanizer", "rewrite AI-shaped prose into natural language"),
    ("update-config", "edit hooks.json safely"),
    ("verify", "drive the app to confirm a change actually works"),
];

pub struct SkillsPhase;

impl Phase for SkillsPhase {
    fn id(&self) -> &'static str {
        "skills"
    }
    fn label(&self) -> &'static str {
        "Curated skill suggestions"
    }

    fn detect(&self) -> PhaseStatus {
        state::detect_skills()
    }

    fn run_interactive(&self) -> io::Result<PhaseStatus> {
        if !prompt::yes_no("Print suggested Codex skills?", false)? {
            return Ok(PhaseStatus::Skipped);
        }
        println!();
        for (name, blurb) in SUGGESTED_SKILLS {
            println!("  /plugin install {name:<14}  — {blurb}");
        }
        println!();
        println!(
            "  (Run these inside any Codex session. Coding Brain does not install \
             skills automatically.)"
        );
        Ok(PhaseStatus::Installed {
            details: format!("{} suggestion(s) shown", SUGGESTED_SKILLS.len()),
        })
    }

    fn run_non_interactive(&self, answers: &Answers) -> io::Result<PhaseStatus> {
        if answers.skip_skills {
            return Ok(PhaseStatus::Skipped);
        }
        Ok(PhaseStatus::Skipped)
    }

    fn remove(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lists_three_phases_in_canonical_order() {
        let r = registry();
        let ids: Vec<_> = r.iter().map(|p| p.id()).collect();
        assert_eq!(ids, vec!["brain", "plugin", "skills"]);
    }

    #[test]
    fn record_from_status_preserves_label_and_details() {
        let r = record_from_status(
            &PhaseStatus::Installed {
                details: "x".into(),
            },
            "2026-06-06T00:00:00Z",
        );
        assert_eq!(r.status, "installed");
        assert_eq!(r.details.as_deref(), Some("x"));
    }
}
