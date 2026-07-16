//! Configuration data types shared between the binary and (future) TUI crate.
//!
//! The binary still owns *parsing* (TOML, CLI flags, layering) — but the
//! resulting `BrainConfig` struct lives here so downstream
//! crates (notably the TUI extracted under #275) can hold them without
//! depending back on the binary's `crate::config` module.

/// Configuration for the optional local LLM brain.
/// When `None`, brain is completely disabled with zero overhead.
#[derive(Debug, Clone)]
pub struct BrainConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub auto_mode: bool,
    /// Unsafe compatibility fallback: allow the asynchronous brain to send
    /// Enter to a terminal-confirmed shell prompt when no managed native
    /// permission hook is configured. Requires `auto_mode` as a second opt-in.
    pub terminal_auto_approve_fallback: bool,
    pub timeout_ms: u64,
    pub max_context_tokens: u32,
    pub few_shot_count: usize,
    pub max_sessions: usize,
    pub orchestrate: bool,
    pub orchestrate_interval_secs: u64,
    /// Command prefixes that identify test-runner invocations. When one of
    /// these fails (non-zero exit), the reaper fans the failure out to recent
    /// brain-approved edits as a `TestFailed` outcome (#238). Empty disables
    /// test-failure attribution.
    pub test_runners: Vec<String>,
}

/// Default test-runner command prefixes. Matched as command-line prefix on
/// the normalized command (whitespace-collapsed, lowercased). Users override
/// via `test_runners` in the `[brain]` config section.
pub fn default_test_runners() -> Vec<String> {
    [
        "cargo test",
        "cargo nextest",
        "npm test",
        "npm run test",
        "pnpm test",
        "yarn test",
        "bun test",
        "pytest",
        "go test",
        "jest",
        "vitest",
        "mix test",
        "rspec",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: "http://localhost:11434/api/generate".into(),
            model: "gemma4:e4b".into(),
            auto_mode: false,
            terminal_auto_approve_fallback: false,
            timeout_ms: 5000,
            max_context_tokens: 4000,
            few_shot_count: 5,
            max_sessions: 10,
            orchestrate: false,
            orchestrate_interval_secs: 30,
            test_runners: default_test_runners(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_auto_approve_fallback_defaults_off() {
        let config = BrainConfig::default();

        assert!(!config.terminal_auto_approve_fallback);
        assert!(!config.auto_mode);
    }
}
