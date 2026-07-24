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
    /// Compatibility marker set when legacy `enabled` or `auto` TOML is explicit.
    #[doc(hidden)]
    pub legacy_mode_configured: bool,
    pub endpoint: String,
    pub model: String,
    pub auto_mode: bool,
    pub timeout_ms: u64,
    pub max_context_tokens: u32,
    pub few_shot_count: usize,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            legacy_mode_configured: false,
            endpoint: "http://localhost:11434/api/generate".into(),
            model: "gemma4:e4b".into(),
            auto_mode: false,
            timeout_ms: 5000,
            max_context_tokens: 4000,
            few_shot_count: 5,
        }
    }
}
