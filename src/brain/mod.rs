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
pub mod retrieval;
pub mod review;
pub mod risk;
pub mod safety;
pub mod sequences;

use std::path::PathBuf;

/// Path to the Brain gate mode file in the Coding Brain state root.
pub fn gate_mode_path() -> PathBuf {
    codexctl_core::paths::CodingBrainPaths::resolve(
        &codexctl_core::paths::PathEnvironment::current(),
    )
    .map(|paths| paths.state_root().join("brain/gate-mode"))
    .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain/brain/gate-mode"))
}

/// Read the current brain gate mode from disk. Returns `"on"` if no file exists.
pub fn read_gate_mode() -> String {
    let path = gate_mode_path();
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "on".into())
}
