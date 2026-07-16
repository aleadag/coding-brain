use std::fs;
use std::path::PathBuf;

use crate::models::{ModelOverride, ModelProfile};
use crate::rules::{AutoRule, RuleAction};

#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Configuration loaded from TOML files, merged with CLI flags.
/// Priority: CLI flags > project config > global config > defaults.
#[derive(Debug, Clone)]
pub struct Config {
    pub interval: u64,
    pub notify: bool,
    pub debug: bool,
    pub grouped: bool,
    pub sort: Option<String>,
    pub budget: Option<f64>,
    pub kill_on_budget: bool,
    pub webhook: Option<String>,
    pub webhook_on: Option<Vec<String>>,
    pub daily_limit: Option<f64>,
    pub weekly_limit: Option<f64>,
    pub context_warn_threshold: u8, // 0-100, fires on_context_high when context % crosses this
    pub model_overrides: Vec<ModelOverride>,
    pub rules: Vec<AutoRule>,
    pub health: HealthThresholds,
    pub file_conflicts: bool, // Detect file-level conflicts across sessions
    pub auto_deny_file_conflicts: bool, // Auto-deny writes to conflicting files
    pub brain: Option<BrainConfig>,
    pub lifecycle: LifecycleConfig,
}

/// Configurable thresholds for session health checks.
/// Re-exported from `codexctl_core::health` so existing `config::HealthThresholds`
/// callers still resolve. The struct itself lives with the health module that
/// owns it, so the binary's TOML parsing (`RawHealthThresholds`, below) is the
/// only piece that needs to know about config-layer concerns.
pub use codexctl_core::health::HealthThresholds;

/// Raw TOML representation for health thresholds — all fields optional.
#[derive(Debug, Default)]
struct RawHealthThresholds {
    cache_critical_pct: Option<f64>,
    cache_warning_pct: Option<f64>,
    cache_min_tokens: Option<u64>,
    cost_spike_critical: Option<f64>,
    cost_spike_warning: Option<f64>,
    loop_max_calls: Option<u32>,
    stall_min_cost: Option<f64>,
    stall_min_minutes: Option<u64>,
    context_critical_pct: Option<f64>,
    context_warning_pct: Option<f64>,
    decay_compaction_pct: Option<f64>,
    efficiency_critical_factor: Option<f64>,
    error_accel_factor: Option<f64>,
    repetition_threshold: Option<u32>,
}

/// `BrainConfig` and friends live in `codexctl_core::config` so the future
/// TUI crate (#275) can hold them without depending on the binary. Re-exported
/// here so existing `crate::config::BrainConfig` callers keep resolving.
pub use codexctl_core::config::{BrainConfig, default_test_runners};

/// Configuration for session lifecycle management (auto-restart on context saturation).
#[derive(Debug, Clone)]
pub struct LifecycleConfig {
    pub auto_restart: bool,
    pub restart_threshold_pct: f64,
    pub restart_only_when_idle: bool,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            auto_restart: false,
            restart_threshold_pct: 90.0,
            restart_only_when_idle: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: 2000,
            notify: false,
            debug: false,
            grouped: false,
            sort: None,
            budget: None,
            kill_on_budget: false,
            webhook: None,
            webhook_on: None,
            daily_limit: None,
            weekly_limit: None,
            context_warn_threshold: 75,
            model_overrides: Vec::new(),
            rules: Vec::new(),
            health: HealthThresholds::default(),
            file_conflicts: true,
            auto_deny_file_conflicts: false,
            brain: None,
            lifecycle: LifecycleConfig::default(),
        }
    }
}

/// Raw TOML representation — all fields optional for partial overrides.
#[derive(Debug, Default)]
struct RawConfig {
    interval: Option<u64>,
    notify: Option<bool>,
    debug: Option<bool>,
    grouped: Option<bool>,
    sort: Option<String>,
    budget: Option<f64>,
    kill_on_budget: Option<bool>,
    webhook_url: Option<String>,
    webhook_events: Option<Vec<String>>,
    daily_limit: Option<f64>,
    weekly_limit: Option<f64>,
    context_warn_threshold: Option<u8>,
    model_overrides: Vec<ModelOverride>,
    rules: Vec<AutoRule>,
    health: Option<RawHealthThresholds>,
    file_conflicts: Option<bool>,
    auto_deny_file_conflicts: Option<bool>,
    brain: Option<RawBrainConfig>,
    lifecycle: Option<RawLifecycleConfig>,
}

#[derive(Debug, Default)]
struct RawBrainConfig {
    enabled: Option<bool>,
    endpoint: Option<String>,
    model: Option<String>,
    auto_mode: Option<bool>,
    terminal_auto_approve_fallback: Option<bool>,
    timeout_ms: Option<u64>,
    max_context_tokens: Option<u32>,
    few_shot_count: Option<usize>,
    max_sessions: Option<usize>,
    orchestrate: Option<bool>,
    orchestrate_interval_secs: Option<u64>,
    test_runners: Option<Vec<String>>,
}

impl RawBrainConfig {
    fn only_terminal_fallback_is_set(&self) -> bool {
        self.terminal_auto_approve_fallback.is_some()
            && self.enabled.is_none()
            && self.endpoint.is_none()
            && self.model.is_none()
            && self.auto_mode.is_none()
            && self.timeout_ms.is_none()
            && self.max_context_tokens.is_none()
            && self.few_shot_count.is_none()
            && self.max_sessions.is_none()
            && self.orchestrate.is_none()
            && self.orchestrate_interval_secs.is_none()
            && self.test_runners.is_none()
    }
}

#[derive(Debug, Default)]
struct RawLifecycleConfig {
    auto_restart: Option<bool>,
    restart_threshold_pct: Option<f64>,
    restart_only_when_idle: Option<bool>,
}

impl Config {
    /// Load configuration from global and project config files.
    pub fn load() -> Self {
        let mut config = Config::default();

        // Layer 1: Global config
        if let Some(global) = global_config_path() {
            if let Some(raw) = parse_config_file(&global) {
                config.apply(raw);
            }
        }

        // Layer 2: Project config (.codexctl.toml in cwd)
        if let Some(raw) = parse_config_file(&PathBuf::from(".codexctl.toml")) {
            config.apply(raw);
        }

        config
    }

    /// Apply a raw config layer on top, overriding only set fields.
    fn apply(&mut self, raw: RawConfig) {
        if let Some(v) = raw.interval {
            self.interval = v;
        }
        if let Some(v) = raw.notify {
            self.notify = v;
        }
        if let Some(v) = raw.debug {
            self.debug = v;
        }
        if let Some(v) = raw.grouped {
            self.grouped = v;
        }
        if let Some(v) = raw.sort {
            self.sort = Some(v);
        }
        if let Some(v) = raw.budget {
            self.budget = Some(v);
        }
        if let Some(v) = raw.kill_on_budget {
            self.kill_on_budget = v;
        }
        if let Some(v) = raw.webhook_url {
            self.webhook = Some(v);
        }
        if let Some(v) = raw.webhook_events {
            self.webhook_on = Some(v);
        }
        if let Some(v) = raw.daily_limit {
            self.daily_limit = Some(v);
        }
        if let Some(v) = raw.weekly_limit {
            self.weekly_limit = Some(v);
        }
        if let Some(v) = raw.context_warn_threshold {
            self.context_warn_threshold = v.min(100);
        }
        if let Some(h) = raw.health {
            if let Some(v) = h.cache_critical_pct {
                self.health.cache_critical_pct = v;
            }
            if let Some(v) = h.cache_warning_pct {
                self.health.cache_warning_pct = v;
            }
            if let Some(v) = h.cache_min_tokens {
                self.health.cache_min_tokens = v;
            }
            if let Some(v) = h.cost_spike_critical {
                self.health.cost_spike_critical = v;
            }
            if let Some(v) = h.cost_spike_warning {
                self.health.cost_spike_warning = v;
            }
            if let Some(v) = h.loop_max_calls {
                self.health.loop_max_calls = v;
            }
            if let Some(v) = h.stall_min_cost {
                self.health.stall_min_cost = v;
            }
            if let Some(v) = h.stall_min_minutes {
                self.health.stall_min_minutes = v;
            }
            if let Some(v) = h.context_critical_pct {
                self.health.context_critical_pct = v;
            }
            if let Some(v) = h.context_warning_pct {
                self.health.context_warning_pct = v;
            }
            if let Some(v) = h.decay_compaction_pct {
                self.health.decay_compaction_pct = v;
            }
            if let Some(v) = h.efficiency_critical_factor {
                self.health.efficiency_critical_factor = v;
            }
            if let Some(v) = h.error_accel_factor {
                self.health.error_accel_factor = v;
            }
            if let Some(v) = h.repetition_threshold {
                self.health.repetition_threshold = v;
            }
        }
        if let Some(v) = raw.file_conflicts {
            self.file_conflicts = v;
        }
        if let Some(v) = raw.auto_deny_file_conflicts {
            self.auto_deny_file_conflicts = v;
        }
        for override_ in raw.model_overrides {
            upsert_model_override(&mut self.model_overrides, override_);
        }
        for rule in raw.rules {
            // Replace rule with same name, or append
            if let Some(pos) = self.rules.iter().position(|r| r.name == rule.name) {
                self.rules[pos] = rule;
            } else {
                self.rules.push(rule);
            }
        }
        if let Some(raw_brain) = raw.brain {
            let fallback_only = raw_brain.only_terminal_fallback_is_set();
            let brain = self.brain.get_or_insert_with(|| BrainConfig {
                enabled: !fallback_only,
                ..BrainConfig::default()
            });
            if let Some(value) = raw_brain.enabled {
                brain.enabled = value;
            }
            if let Some(value) = raw_brain.endpoint {
                brain.endpoint = value;
            }
            if let Some(value) = raw_brain.model {
                brain.model = value;
            }
            if let Some(value) = raw_brain.auto_mode {
                brain.auto_mode = value;
            }
            if let Some(value) = raw_brain.terminal_auto_approve_fallback {
                brain.terminal_auto_approve_fallback = value;
            }
            if let Some(value) = raw_brain.timeout_ms {
                brain.timeout_ms = value;
            }
            if let Some(value) = raw_brain.max_context_tokens {
                brain.max_context_tokens = value;
            }
            if let Some(value) = raw_brain.few_shot_count {
                brain.few_shot_count = value;
            }
            if let Some(value) = raw_brain.max_sessions {
                brain.max_sessions = value;
            }
            if let Some(value) = raw_brain.orchestrate {
                brain.orchestrate = value;
            }
            if let Some(value) = raw_brain.orchestrate_interval_secs {
                brain.orchestrate_interval_secs = value;
            }
            if let Some(value) = raw_brain.test_runners {
                brain.test_runners = value;
            }
        }
        if let Some(lc) = raw.lifecycle {
            if let Some(v) = lc.auto_restart {
                self.lifecycle.auto_restart = v;
            }
            if let Some(v) = lc.restart_threshold_pct {
                self.lifecycle.restart_threshold_pct = v;
            }
            if let Some(v) = lc.restart_only_when_idle {
                self.lifecycle.restart_only_when_idle = v;
            }
        }
    }

    /// Show resolved config and file locations (for `codexctl config`).
    pub fn print_resolved(&self) {
        println!("Resolved configuration:");
        println!();

        if let Some(p) = global_config_path() {
            if p.exists() {
                println!("  Global config: {}", p.display());
            } else {
                println!("  Global config: {} (not found)", p.display());
            }
        }

        let project_path = PathBuf::from(".codexctl.toml");
        if project_path.exists() {
            println!("  Project config: {}", project_path.display());
        } else {
            println!("  Project config: .codexctl.toml (not found)");
        }

        println!();
        println!("  interval:       {}ms", self.interval);
        println!("  notify:         {}", self.notify);
        println!("  debug:          {}", self.debug);
        println!("  grouped:        {}", self.grouped);
        println!(
            "  sort:           {}",
            self.sort.as_deref().unwrap_or("default")
        );
        println!(
            "  budget:         {}",
            self.budget
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!("  kill_on_budget: {}", self.kill_on_budget);
        println!(
            "  webhook:        {}",
            self.webhook.as_deref().unwrap_or("none")
        );
        println!(
            "  webhook_on:     {}",
            self.webhook_on
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_else(|| "all".into())
        );
        println!(
            "  daily_limit:    {}",
            self.daily_limit
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!(
            "  weekly_limit:   {}",
            self.weekly_limit
                .map(|b| format!("${b:.2}"))
                .unwrap_or_else(|| "none".into())
        );
        println!("  context_warn: {}%", self.context_warn_threshold);
        println!();
        println!("  [orchestrate]");
        println!("  file_conflicts:           {}", self.file_conflicts);
        println!(
            "  auto_deny_file_conflicts: {}",
            self.auto_deny_file_conflicts
        );
        println!();
        println!("  [health]");
        println!(
            "  cache:    critical <{:.0}%, warning <{:.0}%, min {}",
            self.health.cache_critical_pct,
            self.health.cache_warning_pct,
            self.health.cache_min_tokens,
        );
        println!(
            "  cost:     critical >{:.1}x, warning >{:.1}x",
            self.health.cost_spike_critical, self.health.cost_spike_warning,
        );
        println!("  loop:     {} calls", self.health.loop_max_calls);
        println!(
            "  stall:    >${:.0} and >{}min",
            self.health.stall_min_cost, self.health.stall_min_minutes,
        );
        println!(
            "  context:  critical >{:.0}%, warning >{:.0}%",
            self.health.context_critical_pct, self.health.context_warning_pct,
        );
        println!(
            "  decay:    compact >{:.0}%, efficiency >{:.1}x, errors >{:.1}x, repeats >{}",
            self.health.decay_compaction_pct,
            self.health.efficiency_critical_factor,
            self.health.error_accel_factor,
            self.health.repetition_threshold,
        );
        println!();
        println!("  [lifecycle]");
        println!("  auto_restart:     {}", self.lifecycle.auto_restart);
        println!(
            "  restart_threshold: {:.0}%",
            self.lifecycle.restart_threshold_pct
        );
        println!(
            "  restart_idle_only: {}",
            self.lifecycle.restart_only_when_idle
        );
        if let Some(brain) = &self.brain {
            println!();
            println!("  [brain]");
            println!("  enabled:                        {}", brain.enabled);
            println!("  auto:                           {}", brain.auto_mode);
            println!(
                "  terminal_auto_approve_fallback: {}",
                brain.terminal_auto_approve_fallback
            );
        }
        if self.model_overrides.is_empty() {
            println!("  model_overrides: none");
        } else {
            println!("  model_overrides:");
            for override_ in &self.model_overrides {
                println!(
                    "    {} => in ${:.2}/M, out ${:.2}/M, ctx {}",
                    override_.name,
                    override_.profile.input_per_m,
                    override_.profile.output_per_m,
                    override_.profile.context_max
                );
            }
        }
    }

    /// Print an annotated default config template to stdout.
    pub fn print_template() {
        print!("{}", Self::template_string());
    }

    /// Return the config template as a string.
    pub fn template_string() -> &'static str {
        r#"# codexctl configuration
# Place this file at:
#   Project: .codexctl.toml (in your project root)
#   Global:  ~/.config/codexctl/config.toml
#
# Priority: CLI flags > project config > global config > defaults
# Only set values you want to override — unset keys use defaults.

# ── General ─────────────────────────────────────────────────────────

[defaults]
# Refresh interval in milliseconds
# interval = 2000

# Enable desktop notifications on NeedsInput transitions
# notify = false

# Show debug timing metrics in the footer
# debug = false

# Group sessions by project in the table view
# grouped = false

# Default sort column: "Status", "Context", "Cost", "$/hr", "Elapsed"
# sort = "Status"

# Per-session budget in USD (alert at 80%, optionally kill at 100%)
# budget = 10.00

# Auto-kill sessions that exceed the budget (requires budget)
# kill_on_budget = false

# ── Webhook ─────────────────────────────────────────────────────────

[webhook]
# POST JSON on status changes
# url = "https://hooks.slack.com/services/..."

# Only fire on these status transitions (omit for all)
# events = ["NeedsInput", "Finished"]

# ── Budget Limits ───────────────────────────────────────────────────

[budget]
# Daily spending limit in USD
# daily_limit = 50.00

# Weekly spending limit in USD
# weekly_limit = 200.00

# ── Context ─────────────────────────────────────────────────────────

[context]
# Fire on_context_high hook when context usage crosses this percentage
# warn_threshold = 75

# ── Orchestration ───────────────────────────────────────────────────

[orchestrate]
# Detect file-level conflicts when multiple sessions edit the same file
# file_conflicts = true

# Auto-deny writes to files being edited by another session
# auto_deny_file_conflicts = false

# ── Health Check Thresholds ─────────────────────────────────────────

[health]
# Cache hit ratio thresholds (percentage, 0-100)
# cache_critical_pct = 10.0
# cache_warning_pct = 30.0
# cache_min_tokens = 10000

# Cost spike detection (multiplier of session average burn rate)
# cost_spike_critical = 5.0
# cost_spike_warning = 2.5

# Loop detection (tool call count threshold when errors are present)
# loop_max_calls = 10

# Stall detection (minimum cost in USD and minutes with no file edits)
# stall_min_cost = 5.0
# stall_min_minutes = 10

# Context saturation thresholds (percentage, 0-100)
# context_critical_pct = 90.0
# context_warning_pct = 80.0

# Cognitive decay detection
# decay_compaction_pct = 50.0         # Context % to suggest proactive /compact
# efficiency_critical_factor = 2.0    # Tokens-per-edit ratio vs baseline to trigger
# error_accel_factor = 2.0            # Error rate ratio vs baseline to trigger
# repetition_threshold = 3            # File re-reads without edit to trigger

# ── Model Pricing Overrides ─────────────────────────────────────────
# Override built-in pricing for specific models.
#
# [models."gpt-5.5"]
# input_per_m = 5.00
# output_per_m = 30.00
# cache_read_per_m = 0.50
# cache_write_per_m = 5.00
# context_max = 258400
# long_context_threshold = 272000
# long_context_input_multiplier = 2.0
# long_context_output_multiplier = 1.5

# ── Auto-Rules ──────────────────────────────────────────────────────
# Match sessions by status/tool/command/project/cost, then take action.
# Deny rules always take precedence regardless of order.
#
# [rules.approve_reads]
# match_status = ["Needs Input"]
# match_tool = ["Read", "Glob", "Grep"]
# action = "approve"
#
# [rules.deny_destructive]
# match_tool = ["Bash"]
# match_command = ["rm -rf", "git push --force"]
# action = "deny"
#
# [rules.kill_runaway]
# match_cost_above = 20.0
# action = "terminate"
#
# [rules.auto_continue]
# match_status = ["Waiting"]
# action = "send"
# message = "continue"

# ── Event Hooks ─────────────────────────────────────────────────────
# Run shell commands on session events.
#
# [hooks.on_needs_input]
# run = "notify-send 'Session needs input'"
#
# [hooks.on_finished]
# run = "say 'Session finished'"
#
# Available events: on_needs_input, on_finished, on_budget_80,
#   on_budget_exceeded, on_context_high, on_status_change

# ── Lifecycle ──────────────────────────────────────────────────────

# [lifecycle]
# auto_restart = false
# restart_threshold_pct = 90.0
# restart_only_when_idle = true

# ── Brain (Local LLM) ──────────────────────────────────────────────

# [brain]
# enabled = true
# endpoint = "http://localhost:11434/api/generate"
# model = "gemma4:e4b"
# auto = false
# UNSAFE compatibility mode: allow guarded Enter on a terminal-confirmed shell
# prompt only when auto=true and no managed PermissionRequest hook is configured.
# terminal_auto_approve_fallback = false
# timeout_ms = 5000
# max_context_tokens = 4000
# few_shot_count = 5
# max_sessions = 10
# orchestrate = false
# orchestrate_interval = 30
# test_runners = ["cargo test", "npm test", "pytest", "go test", "bun test"]
"#
    }
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("codexctl")
            .join("config.toml")
    })
}

impl Config {
    /// Path to the global config file (for validation and diagnostics).
    pub fn global_path() -> Option<PathBuf> {
        global_config_path()
    }
}

/// Minimal TOML parser — avoids adding a toml crate dependency.
/// Supports: key = value pairs, [sections], # comments, strings, numbers, booleans, arrays.
fn parse_config_file(path: &PathBuf) -> Option<RawConfig> {
    let content = fs::read_to_string(path).ok()?;
    let mut raw = RawConfig::default();
    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Section headers
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Key = value
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        // Strip inline comments
        let value = value.split('#').next().unwrap_or(value).trim();

        match (section.as_str(), key) {
            ("" | "defaults", "interval") => {
                raw.interval = value.parse().ok();
            }
            ("" | "defaults", "notify") => {
                raw.notify = parse_bool(value);
            }
            ("" | "defaults", "debug") => {
                raw.debug = parse_bool(value);
            }
            ("" | "defaults", "grouped") => {
                raw.grouped = parse_bool(value);
            }
            ("" | "defaults", "sort") => {
                raw.sort = Some(unquote(value));
            }
            ("" | "defaults", "budget") => {
                raw.budget = value.parse().ok();
            }
            ("" | "defaults", "kill_on_budget") => {
                raw.kill_on_budget = parse_bool(value);
            }
            ("webhook", "url") => {
                raw.webhook_url = Some(unquote(value));
            }
            ("webhook", "events") => {
                raw.webhook_events = Some(parse_string_array(value));
            }
            ("budget", "daily_limit") => {
                raw.daily_limit = value.parse().ok();
            }
            ("budget", "weekly_limit") => {
                raw.weekly_limit = value.parse().ok();
            }
            ("context", "warn_threshold") => {
                raw.context_warn_threshold = value.parse().ok();
            }
            ("orchestrate", "file_conflicts") => {
                raw.file_conflicts = parse_bool(value);
            }
            ("orchestrate", "auto_deny_file_conflicts") => {
                raw.auto_deny_file_conflicts = parse_bool(value);
            }
            ("health", key) => {
                let h = raw.health.get_or_insert_with(RawHealthThresholds::default);
                match key {
                    "cache_critical_pct" => h.cache_critical_pct = value.parse().ok(),
                    "cache_warning_pct" => h.cache_warning_pct = value.parse().ok(),
                    "cache_min_tokens" => h.cache_min_tokens = value.parse().ok(),
                    "cost_spike_critical" => h.cost_spike_critical = value.parse().ok(),
                    "cost_spike_warning" => h.cost_spike_warning = value.parse().ok(),
                    "loop_max_calls" => h.loop_max_calls = value.parse().ok(),
                    "stall_min_cost" => h.stall_min_cost = value.parse().ok(),
                    "stall_min_minutes" => h.stall_min_minutes = value.parse().ok(),
                    "context_critical_pct" => h.context_critical_pct = value.parse().ok(),
                    "context_warning_pct" => h.context_warning_pct = value.parse().ok(),
                    "decay_compaction_pct" => h.decay_compaction_pct = value.parse().ok(),
                    "efficiency_critical_factor" => {
                        h.efficiency_critical_factor = value.parse().ok()
                    }
                    "error_accel_factor" => h.error_accel_factor = value.parse().ok(),
                    "repetition_threshold" => h.repetition_threshold = value.parse().ok(),
                    _ => {}
                }
            }
            _ if parse_model_section(&section).is_some() => {
                let Some(model_name) = parse_model_section(&section) else {
                    continue;
                };
                let profile = ensure_model_override(&mut raw.model_overrides, &model_name);
                match key {
                    "input_per_m" => {
                        profile.input_per_m = value.parse().unwrap_or(profile.input_per_m);
                    }
                    "output_per_m" => {
                        profile.output_per_m = value.parse().unwrap_or(profile.output_per_m);
                    }
                    "cache_read_per_m" => {
                        profile.cache_read_per_m =
                            value.parse().unwrap_or(profile.cache_read_per_m);
                    }
                    "cache_write_per_m" => {
                        profile.cache_write_per_m =
                            value.parse().unwrap_or(profile.cache_write_per_m);
                    }
                    "context_max" => {
                        profile.context_max = value.parse().unwrap_or(profile.context_max);
                    }
                    "long_context_threshold" => {
                        profile.long_context_threshold = value.parse().ok();
                    }
                    "long_context_input_multiplier" => {
                        profile.long_context_input_multiplier = value
                            .parse()
                            .unwrap_or(profile.long_context_input_multiplier);
                    }
                    "long_context_output_multiplier" => {
                        profile.long_context_output_multiplier = value
                            .parse()
                            .unwrap_or(profile.long_context_output_multiplier);
                    }
                    _ => {}
                }
            }
            _ if parse_rule_section(&section).is_some() => {
                let Some(rule_name) = parse_rule_section(&section) else {
                    continue;
                };
                let rule = ensure_rule(&mut raw.rules, &rule_name);
                match key {
                    "match_status" => rule.match_status = parse_string_array(value),
                    "match_tool" => rule.match_tool = parse_string_array(value),
                    "match_command" => rule.match_command = parse_string_array(value),
                    "match_project" => rule.match_project = parse_string_array(value),
                    "match_cost_above" => rule.match_cost_above = value.parse().ok(),
                    "match_last_error" => rule.match_last_error = parse_bool(value),
                    "match_file_conflict" => rule.match_file_conflict = parse_bool(value),
                    "action" => {
                        if let Some(a) = RuleAction::parse(&unquote(value)) {
                            rule.action = a;
                        }
                    }
                    "message" => rule.message = Some(unquote(value)),
                    _ => {}
                }
            }
            ("lifecycle", key) => {
                let lc = raw
                    .lifecycle
                    .get_or_insert_with(RawLifecycleConfig::default);
                match key {
                    "auto_restart" => lc.auto_restart = parse_bool(value),
                    "restart_threshold_pct" => lc.restart_threshold_pct = value.parse().ok(),
                    "restart_only_when_idle" => lc.restart_only_when_idle = parse_bool(value),
                    _ => {}
                }
            }
            ("brain", _) => {
                let brain = raw.brain.get_or_insert_with(RawBrainConfig::default);
                match key {
                    "enabled" => {
                        brain.enabled = parse_bool(value);
                    }
                    "endpoint" => brain.endpoint = Some(unquote(value)),
                    "model" => brain.model = Some(unquote(value)),
                    "auto" => {
                        brain.auto_mode = parse_bool(value);
                    }
                    "terminal_auto_approve_fallback" => {
                        brain.terminal_auto_approve_fallback = parse_bool(value);
                    }
                    "timeout_ms" => {
                        brain.timeout_ms = value.parse().ok();
                    }
                    "max_context_tokens" => {
                        brain.max_context_tokens = value.parse().ok();
                    }
                    "few_shot_count" => {
                        brain.few_shot_count = value.parse().ok();
                    }
                    "max_sessions" => {
                        brain.max_sessions = value.parse().ok();
                    }
                    "orchestrate" => {
                        brain.orchestrate = parse_bool(value);
                    }
                    "orchestrate_interval" => {
                        brain.orchestrate_interval_secs = value.parse().ok();
                    }
                    "test_runners" => {
                        let parsed = parse_string_array(value);
                        if !parsed.is_empty() {
                            brain.test_runners = Some(parsed);
                        }
                    }
                    _ => {}
                }
            }
            _ => {} // Ignore unknown keys
        }
    }

    Some(raw)
}

// ────────────────────────────────────────────────────────────────────────────
// Config validation
// ────────────────────────────────────────────────────────────────────────────

/// A warning or error from config validation.
pub struct ConfigWarning {
    pub line: usize,
    pub message: String,
}

/// Known sections and their valid keys.
fn known_keys(section: &str) -> Option<&'static [&'static str]> {
    match section {
        "" | "defaults" => Some(&[
            "interval",
            "notify",
            "debug",
            "grouped",
            "sort",
            "budget",
            "kill_on_budget",
            "context_warn",
            "context_warn_threshold",
            "file_conflicts",
            "auto_deny_file_conflicts",
        ]),
        "webhook" => Some(&["url", "on"]),
        "budget" => Some(&["daily", "daily_limit", "weekly", "weekly_limit"]),
        "context" => Some(&["warn", "warn_threshold"]),
        "orchestrate" => Some(&["file_conflicts", "auto_deny_file_conflicts"]),
        "health" => Some(&[
            "cache_critical_pct",
            "cache_warning_pct",
            "cache_min_tokens",
            "cost_spike_critical",
            "cost_spike_warning",
            "loop_max_calls",
            "stall_min_cost",
            "stall_min_minutes",
            "context_critical_pct",
            "context_warning_pct",
            "decay_compaction_pct",
            "efficiency_critical_factor",
            "error_accel_factor",
            "repetition_threshold",
        ]),
        "lifecycle" => Some(&[
            "auto_restart",
            "restart_threshold_pct",
            "restart_only_when_idle",
        ]),
        "brain" => Some(&[
            "enabled",
            "endpoint",
            "model",
            "auto",
            "terminal_auto_approve_fallback",
            "timeout_ms",
            "max_context_tokens",
            "few_shot_count",
            "max_sessions",
            "orchestrate",
            "orchestrate_interval",
            "orchestrate_interval_secs",
            "test_runners",
        ]),
        _ => None,
    }
}

fn removed_section_message(section: &str) -> Option<&'static str> {
    match section.split('.').next().unwrap_or(section) {
        "relay" | "hive" | "idle" | "agents" => Some(
            "is no longer supported by brain-only codexctl; use Beads or an external worker for durable coordination",
        ),
        _ => None,
    }
}

fn removed_key_message(section: &str, key: &str) -> Option<&'static str> {
    match (section, key) {
        ("lifecycle", "retention_days") => Some(
            "lifecycle.retention_days is no longer supported; codexctl no longer prunes coordination state",
        ),
        _ => None,
    }
}

/// Validate a config file and return warnings for unknown keys/sections.
pub fn validate_config_file(path: &PathBuf) -> (Vec<ConfigWarning>, bool) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            return (
                vec![ConfigWarning {
                    line: 0,
                    message: format!("cannot read file: {e}"),
                }],
                true,
            );
        }
    };

    let mut warnings = Vec::new();
    let mut section = String::new();
    let mut has_errors = false;

    for (line_num, line) in content.lines().enumerate() {
        let line_1 = line_num + 1;
        let line = line.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Section header
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            if let Some(message) = removed_section_message(&section) {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("[{section}] {message}"),
                });
                continue;
            }

            // Check if section is known (accounting for dynamic sections like models.*, rules.*, hooks.*)
            let base = section.split('.').next().unwrap_or(&section);
            let is_known =
                known_keys(&section).is_some() || matches!(base, "models" | "rules" | "hooks");
            if !is_known {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("unknown section [{section}]"),
                });
            }
            continue;
        }

        // Key = value
        let Some((key, _value)) = line.split_once('=') else {
            warnings.push(ConfigWarning {
                line: line_1,
                message: format!("malformed line (expected key = value): {line}"),
            });
            has_errors = true;
            continue;
        };
        let key = key.trim();

        if removed_section_message(&section).is_some() {
            continue;
        }

        if let Some(message) = removed_key_message(&section, key) {
            warnings.push(ConfigWarning {
                line: line_1,
                message: message.into(),
            });
            continue;
        }

        // Skip dynamic sections (models.*, rules.*, hooks.*)
        let base = section.split('.').next().unwrap_or(&section);
        if matches!(base, "models" | "rules" | "hooks") {
            continue;
        }

        // Check if key is known for this section
        if let Some(valid_keys) = known_keys(&section) {
            if !valid_keys.contains(&key) {
                warnings.push(ConfigWarning {
                    line: line_1,
                    message: format!("unknown key \"{key}\" in [{section}]"),
                });
            }
        }
    }

    (warnings, has_errors)
}

fn legacy_config_warnings_for_paths(paths: &[PathBuf]) -> Vec<(PathBuf, ConfigWarning)> {
    paths
        .iter()
        .filter(|path| path.exists())
        .flat_map(|path| {
            let (warnings, _) = validate_config_file(path);
            warnings
                .into_iter()
                .filter(|warning| warning.message.contains("no longer supported"))
                .map(|warning| (path.clone(), warning))
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn legacy_config_warnings() -> Vec<(PathBuf, ConfigWarning)> {
    let mut paths = Vec::new();
    if let Some(global) = global_config_path() {
        paths.push(global);
    }
    paths.push(PathBuf::from(".codexctl.toml"));
    legacy_config_warnings_for_paths(&paths)
}

/// Load hooks from global and project config files.
pub fn load_hooks() -> crate::hooks::HookRegistry {
    let mut registry = crate::hooks::HookRegistry::new();

    if let Some(global) = global_config_path() {
        parse_hooks_from_file(&global, &mut registry);
    }
    parse_hooks_from_file(&PathBuf::from(".codexctl.toml"), &mut registry);

    registry
}

fn parse_hooks_from_file(path: &PathBuf, registry: &mut crate::hooks::HookRegistry) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let mut section = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }

        // Only process hooks sections
        if !section.starts_with("hooks.") {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let value = value.split('#').next().unwrap_or(value).trim();

        if key == "run" {
            if let Some(event) = crate::hooks::HookEvent::from_section(&section) {
                registry.add(event, unquote(value));
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').trim_matches('\'').to_string()
}

fn parse_string_array(s: &str) -> Vec<String> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

fn parse_model_section(section: &str) -> Option<String> {
    section.strip_prefix("models.").map(unquote)
}

fn ensure_model_override<'a>(
    overrides: &'a mut Vec<ModelOverride>,
    model_name: &str,
) -> &'a mut ModelProfile {
    if let Some(index) = overrides.iter().position(|item| item.name == model_name) {
        return &mut overrides[index].profile;
    }

    overrides.push(ModelOverride {
        name: model_name.to_string(),
        profile: ModelProfile {
            input_per_m: 0.0,
            output_per_m: 0.0,
            cache_read_per_m: 0.0,
            cache_write_per_m: 0.0,
            context_max: 0,
            long_context_threshold: None,
            long_context_input_multiplier: 1.0,
            long_context_output_multiplier: 1.0,
        },
    });

    &mut overrides
        .last_mut()
        .expect("override was just pushed")
        .profile
}

fn upsert_model_override(overrides: &mut Vec<ModelOverride>, incoming: ModelOverride) {
    if let Some(existing) = overrides.iter_mut().find(|item| item.name == incoming.name) {
        *existing = incoming;
    } else {
        overrides.push(incoming);
    }
}

fn parse_rule_section(section: &str) -> Option<String> {
    section.strip_prefix("rules.").map(unquote)
}

fn ensure_rule<'a>(rules: &'a mut Vec<AutoRule>, name: &str) -> &'a mut AutoRule {
    if let Some(index) = rules.iter().position(|r| r.name == name) {
        return &mut rules[index];
    }
    rules.push(AutoRule::new(name.to_string(), RuleAction::Approve));
    rules.last_mut().expect("rule was just pushed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("yes"), None);
    }

    #[test]
    fn test_unquote() {
        assert_eq!(unquote("\"hello\""), "hello");
        assert_eq!(unquote("'hello'"), "hello");
        assert_eq!(unquote("hello"), "hello");
    }

    #[test]
    fn test_parse_string_array() {
        let result = parse_string_array("[\"NeedsInput\", \"Finished\"]");
        assert_eq!(result, vec!["NeedsInput", "Finished"]);
    }

    #[test]
    fn test_parse_config_file() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
# Global codexctl config
[defaults]
interval = 1000
notify = true
grouped = true
sort = "cost"
budget = 5.00
kill_on_budget = false

[webhook]
url = "https://hooks.slack.com/test"
events = ["NeedsInput", "Finished"]

[models."gpt-5.5"]
input_per_m = 5.0
output_per_m = 30.0
cache_read_per_m = 0.5
cache_write_per_m = 5.0
context_max = 258400
long_context_threshold = 272000
long_context_input_multiplier = 2.0
long_context_output_multiplier = 1.5

[models."custom"]
input_per_m = 1.0
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.interval, Some(1000));
        assert_eq!(raw.notify, Some(true));
        assert_eq!(raw.grouped, Some(true));
        assert_eq!(raw.sort, Some("cost".into()));
        assert_eq!(raw.budget, Some(5.0));
        assert_eq!(raw.kill_on_budget, Some(false));
        assert_eq!(raw.webhook_url, Some("https://hooks.slack.com/test".into()));
        assert_eq!(
            raw.webhook_events,
            Some(vec!["NeedsInput".into(), "Finished".into()])
        );
        assert_eq!(raw.model_overrides.len(), 2);
        assert_eq!(raw.model_overrides[0].name, "gpt-5.5");
        assert_eq!(raw.model_overrides[0].profile.context_max, 258_400);
        assert_eq!(
            raw.model_overrides[0].profile.long_context_threshold,
            Some(272_000)
        );
        assert_eq!(
            raw.model_overrides[0].profile.long_context_input_multiplier,
            2.0
        );
        assert_eq!(
            raw.model_overrides[0]
                .profile
                .long_context_output_multiplier,
            1.5
        );
        assert_eq!(raw.model_overrides[1].name, "custom");
        assert_eq!(raw.model_overrides[1].profile.long_context_threshold, None);
        assert_eq!(
            raw.model_overrides[1].profile.long_context_input_multiplier,
            1.0
        );
        assert_eq!(
            raw.model_overrides[1]
                .profile
                .long_context_output_multiplier,
            1.0
        );
    }

    #[test]
    fn test_config_layering() {
        let mut config = Config::default();
        assert_eq!(config.interval, 2000);
        assert!(!config.notify);

        // Apply global config
        config.apply(RawConfig {
            interval: Some(1000),
            notify: Some(true),
            budget: Some(5.0),
            ..RawConfig::default()
        });
        assert_eq!(config.interval, 1000);
        assert!(config.notify);
        assert_eq!(config.budget, Some(5.0));

        // Apply project config — overrides some fields
        config.apply(RawConfig {
            budget: Some(10.0),
            grouped: Some(true),
            ..RawConfig::default()
        });
        assert_eq!(config.interval, 1000); // Unchanged
        assert!(config.notify); // Unchanged
        assert_eq!(config.budget, Some(10.0)); // Overridden
        assert!(config.grouped); // New
    }

    #[test]
    fn test_parse_rules_from_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[rules.approve_reads]
match_status = ["Needs Input"]
match_tool = ["Read", "Glob", "Grep"]
action = "approve"

[rules.deny_destructive]
match_status = ["Needs Input"]
match_tool = ["Bash"]
match_command = ["rm -rf", "git push --force"]
action = "deny"

[rules.auto_continue]
match_status = ["Waiting"]
action = "send"
message = "continue"

[rules.kill_expensive]
match_cost_above = 10.0
action = "terminate"
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.rules.len(), 4);

        let r0 = &raw.rules[0];
        assert_eq!(r0.name, "approve_reads");
        assert_eq!(r0.match_tool, vec!["Read", "Glob", "Grep"]);
        assert_eq!(r0.action, RuleAction::Approve);

        let r1 = &raw.rules[1];
        assert_eq!(r1.name, "deny_destructive");
        assert_eq!(r1.match_command, vec!["rm -rf", "git push --force"]);
        assert_eq!(r1.action, RuleAction::Deny);

        let r2 = &raw.rules[2];
        assert_eq!(r2.name, "auto_continue");
        assert_eq!(r2.action, RuleAction::Send);
        assert_eq!(r2.message, Some("continue".into()));

        let r3 = &raw.rules[3];
        assert_eq!(r3.name, "kill_expensive");
        assert_eq!(r3.match_cost_above, Some(10.0));
        assert_eq!(r3.action, RuleAction::Terminate);
    }

    #[test]
    fn test_parse_brain_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[brain]
enabled = true
endpoint = "http://localhost:8080/v1/chat"
model = "llama3:8b"
auto = true
timeout_ms = 3000
max_context_tokens = 8000
terminal_auto_approve_fallback = true
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        let mut config = Config::default();
        config.apply(raw);
        let brain = config.brain.expect("brain config should be parsed");
        assert!(brain.enabled);
        assert_eq!(brain.endpoint, "http://localhost:8080/v1/chat");
        assert_eq!(brain.model, "llama3:8b");
        assert!(brain.auto_mode);
        assert!(brain.terminal_auto_approve_fallback);
        assert_eq!(brain.timeout_ms, 3000);
        assert_eq!(brain.max_context_tokens, 8000);
    }

    #[test]
    fn terminal_fallback_is_a_known_brain_key_and_is_in_the_template() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[brain]\nterminal_auto_approve_fallback = true").unwrap();
        file.flush().unwrap();

        let (warnings, has_errors) = validate_config_file(&file.path().to_path_buf());
        assert!(!has_errors);
        assert!(warnings.is_empty());
        assert!(Config::template_string().contains("terminal_auto_approve_fallback = false"));

        let mut config = Config::default();
        config.apply(parse_config_file(&file.path().to_path_buf()).unwrap());
        let brain = config.brain.unwrap();
        assert!(!brain.enabled);
        assert!(!brain.auto_mode);
    }

    #[test]
    fn brain_config_layers_merge_field_by_field() {
        use std::io::Write;

        let mut global = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            global,
            r#"
[brain]
enabled = true
endpoint = "http://localhost:8080/v1/chat"
model = "local-model"
auto = true
terminal_auto_approve_fallback = false
timeout_ms = 3210
max_context_tokens = 7654
few_shot_count = 7
max_sessions = 12
orchestrate = true
orchestrate_interval = 45
test_runners = ["just test", "cargo test"]
"#
        )
        .unwrap();
        global.flush().unwrap();
        let mut project = tempfile::NamedTempFile::new().unwrap();
        writeln!(project, "[brain]\nterminal_auto_approve_fallback = true").unwrap();
        project.flush().unwrap();

        let mut config = Config::default();
        config.apply(parse_config_file(&global.path().to_path_buf()).unwrap());
        config.apply(parse_config_file(&project.path().to_path_buf()).unwrap());

        let brain = config.brain.unwrap();
        assert!(brain.enabled);
        assert_eq!(brain.endpoint, "http://localhost:8080/v1/chat");
        assert_eq!(brain.model, "local-model");
        assert!(brain.auto_mode);
        assert!(brain.terminal_auto_approve_fallback);
        assert_eq!(brain.timeout_ms, 3210);
        assert_eq!(brain.max_context_tokens, 7654);
        assert_eq!(brain.few_shot_count, 7);
        assert_eq!(brain.max_sessions, 12);
        assert!(brain.orchestrate);
        assert_eq!(brain.orchestrate_interval_secs, 45);
        assert_eq!(brain.test_runners, vec!["just test", "cargo test"]);
    }

    #[test]
    fn test_no_brain_config_returns_none() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[defaults]\ninterval = 1000").unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert!(raw.brain.is_none());
    }

    #[test]
    fn test_parse_health_thresholds() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[health]
cache_critical_pct = 5.0
cache_warning_pct = 20.0
cache_min_tokens = 50000
cost_spike_critical = 8.0
cost_spike_warning = 3.0
loop_max_calls = 15
stall_min_cost = 10.0
stall_min_minutes = 20
context_critical_pct = 95.0
context_warning_pct = 85.0
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        let h = raw.health.expect("health config should be parsed");
        assert_eq!(h.cache_critical_pct, Some(5.0));
        assert_eq!(h.cache_warning_pct, Some(20.0));
        assert_eq!(h.cache_min_tokens, Some(50000));
        assert_eq!(h.cost_spike_critical, Some(8.0));
        assert_eq!(h.cost_spike_warning, Some(3.0));
        assert_eq!(h.loop_max_calls, Some(15));
        assert_eq!(h.stall_min_cost, Some(10.0));
        assert_eq!(h.stall_min_minutes, Some(20));
        assert_eq!(h.context_critical_pct, Some(95.0));
        assert_eq!(h.context_warning_pct, Some(85.0));
    }

    #[test]
    fn test_health_thresholds_layering() {
        let mut config = Config::default();
        assert_eq!(config.health.cache_critical_pct, 10.0); // default

        config.apply(RawConfig {
            health: Some(RawHealthThresholds {
                cache_critical_pct: Some(5.0),
                ..RawHealthThresholds::default()
            }),
            ..RawConfig::default()
        });
        assert_eq!(config.health.cache_critical_pct, 5.0); // overridden
        assert_eq!(config.health.cache_warning_pct, 30.0); // unchanged default
    }

    #[test]
    fn test_parse_orchestrate_config() {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
[orchestrate]
file_conflicts = true
auto_deny_file_conflicts = true
"#
        )
        .unwrap();
        file.flush().unwrap();

        let raw = parse_config_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(raw.file_conflicts, Some(true));
        assert_eq!(raw.auto_deny_file_conflicts, Some(true));
    }

    #[test]
    fn test_orchestrate_defaults() {
        let config = Config::default();
        assert!(config.file_conflicts); // on by default
        assert!(!config.auto_deny_file_conflicts); // off by default
    }

    #[test]
    fn removed_sections_warn_but_brain_orchestration_still_parses() {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
[brain]
orchestrate = true
orchestrate_interval = 45
max_sessions = 6

[lifecycle]
auto_restart = true
retention_days = 30

[relay]
enabled = true

[hive]
enabled = true

[idle]
enabled = true

[agents.reviewer]
model = "gpt-5"
"#,
        )
        .unwrap();
        file.flush().unwrap();
        let path = file.path().to_path_buf();

        let (warnings, has_errors) = validate_config_file(&path);
        assert!(!has_errors);
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("[relay] is no longer supported"))
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("[hive] is no longer supported"))
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.message.contains("[idle] is no longer supported"))
        );
        assert!(warnings.iter().any(|w| {
            w.message
                .contains("[agents.reviewer] is no longer supported")
        }));
        assert!(warnings.iter().any(|w| {
            w.message
                .contains("lifecycle.retention_days is no longer supported")
        }));
        let startup_warnings = legacy_config_warnings_for_paths(std::slice::from_ref(&path));
        assert_eq!(startup_warnings.len(), 5);

        let raw = parse_config_file(&path).unwrap();
        let mut config = Config::default();
        config.apply(raw);
        let brain = config.brain.unwrap();
        assert!(brain.orchestrate);
        assert_eq!(brain.orchestrate_interval_secs, 45);
        assert_eq!(brain.max_sessions, 6);
        assert!(config.lifecycle.auto_restart);
    }
}
