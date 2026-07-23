#![allow(unknown_lints)]
#![allow(
    clippy::collapsible_if,
    clippy::manual_is_multiple_of,
    clippy::io_other_error
)]

// Foundational modules from coding-brain-core (epic #279). Re-aliased so existing
// `crate::session::*` paths still resolve. See `lib.rs` for the rationale.
use coding_brain_core::{discovery, hooks, logger, rules, session, theme, transcript};
use coding_brain_tui::ui;

mod brain;
mod commands;
mod config;
mod doctor;
mod init;
mod lifecycle_hook;
mod provider_hooks;
mod runtime;

use std::io;
use std::time::Duration;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use coding_brain_core::provider::AgentProvider;
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Show resolved configuration and file locations.
    Show,
    /// Print one resolved configuration value.
    Get { key: String },
    /// Persist one configuration value.
    Set { key: String, value: String },
    /// Print an annotated default config template to stdout.
    Template,
    /// Validate config files and report unknown keys or malformed values.
    Validate,
    /// Write a sample .coding-brain.toml in the current directory.
    Init,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum InitProvider {
    Codex,
    Claude,
    Antigravity,
    All,
}

fn normalize_init_providers(values: &[InitProvider]) -> Result<Vec<AgentProvider>, String> {
    if values.contains(&InitProvider::All) {
        if values.iter().any(|value| *value != InitProvider::All) {
            return Err("`all` cannot be combined with another provider selector".into());
        }
        return Ok(vec![
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ]);
    }

    let mut providers = Vec::new();
    for candidate in [
        (InitProvider::Codex, AgentProvider::Codex),
        (InitProvider::Claude, AgentProvider::Claude),
        (InitProvider::Antigravity, AgentProvider::Antigravity),
    ] {
        if values.contains(&candidate.0) {
            providers.push(candidate.1);
        }
    }
    Ok(providers)
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Inspect or update Coding Brain configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Onboarding wizard (brain, hooks, skills). See issue #257.
    Init {
        /// Agent providers whose managed hooks should be configured.
        #[arg(
            value_enum,
            conflicts_with_all = ["check", "upgrade", "reset", "remove", "purge", "plugin_only"]
        )]
        providers: Vec<InitProvider>,
        /// Drift report comparing recorded onboarding against current state.
        #[arg(long, conflicts_with_all = ["reset", "remove", "non_interactive"])]
        check: bool,
        /// Clear the onboarding marker so the next `init` starts fresh.
        #[arg(long, conflicts_with_all = ["check", "remove", "non_interactive"])]
        reset: bool,
        /// Uninstall every Coding Brain-managed artifact (hooks, marker).
        /// Preserves Brain decision logs, legacy codexctl state, and the config
        /// file. Pair with `--purge` to wipe everything.
        #[arg(long, conflicts_with_all = ["check", "reset", "non_interactive", "purge"])]
        remove: bool,
        /// Hard uninstall: remove current Coding Brain config/state plus
        /// documented legacy codexctl global config/state. Use to start over from a clean
        /// slate. Requires `--yes` to proceed without prompt.
        #[arg(long, conflicts_with_all = ["check", "reset", "non_interactive", "remove"])]
        purge: bool,
        /// Skip the confirmation prompt for `--purge`.
        #[arg(long)]
        yes: bool,
        /// Deprecated Codex-only hook installer. Use `init codex`.
        #[arg(
            long,
            conflicts_with_all = ["check", "reset", "remove", "purge", "non_interactive"]
        )]
        plugin_only: bool,
        /// Re-sync everything the previous `init` wrote to match the
        /// running binary (#327): hook entries and the onboarding marker version. Use
        /// after upgrading or reinstalling Coding Brain.
        #[arg(
            long,
            conflicts_with_all = ["check", "reset", "remove", "purge", "plugin_only", "non_interactive"]
        )]
        upgrade: bool,
        /// Run every phase without prompting. Combine with the per-phase
        /// flags below (`--brain-url`, etc.).
        #[arg(long)]
        non_interactive: bool,

        /// Local-LLM endpoint URL for the brain phase.
        #[arg(long)]
        brain_url: Option<String>,
        /// Skip the brain phase.
        #[arg(long)]
        skip_brain: bool,

        /// Install the selected provider hooks (default in --non-interactive).
        #[arg(long)]
        install_plugin: bool,
        /// Skip the selected provider hook phase.
        #[arg(long)]
        skip_plugin: bool,

        /// Skip the skills phase.
        #[arg(long)]
        skip_skills: bool,
    },

    /// Print a shell completion script for the given shell (bash, zsh, fish, …) to stdout
    Completions {
        /// Shell to emit completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print a roff-formatted man page to stdout
    Man,

    /// Install + runtime health check. Answers "is everything wired up?"
    /// in one command — PATH, hooks, brain endpoint, session discovery,
    /// and terminal integration.
    /// Exits non-zero on any failure; advisories don't affect exit code.
    Doctor {
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "coding-brain",
    version,
    about = "Supervise Codex sessions with a local brain that learns from you."
)]
pub(crate) struct Cli {
    /// Color theme: dark, light, or none (respects NO_COLOR env var)
    #[arg(long, help_heading = "Brain TUI")]
    pub(crate) theme: Option<String>,

    /// Emit JSON from supported non-interactive Brain commands
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) json: bool,

    /// Run headless with brain evaluation and context rot prevention active (no TUI).
    /// Activity remains available to the Brain TUI in another terminal.
    #[arg(long, help_heading = "Output Modes")]
    pub(crate) headless: bool,

    // ── Brain (Local LLM) ──────────────────────────────────────────────
    /// LLM endpoint URL for brain
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) url: Option<String>,

    /// Override brain model name
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_model: Option<String>,

    /// Run brain eval scenarios against the local LLM and report results
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_eval: bool,

    /// List brain prompt templates and their source (built-in vs user override)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_prompts: bool,

    /// Brain statistics and metrics (subcommands: scorecard, tier, latency, cache, counterfactual, learning-curve, accuracy, baseline, false-approve, help)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_stats: Option<String>,

    /// Interactive review of the highest-value brain decisions. Walks through
    /// counterfactual hits, Critical-tier safety cases, and high-confidence
    /// misses; lets you mark each as canonical (teaching material).
    /// Pass "list" / "queue" to print the queue non-interactively.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_review: Option<Option<String>>,

    /// Mark a specific decision_id as canonical without going through the
    /// interactive review (used by `brain counterfactual` output).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_mark_canonical: Option<String>,

    /// Query the brain for a single tool-call decision and exit (JSON output).
    /// Used by Codex hooks for inline approve/deny.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_query: bool,

    /// Internal provider permission hook adapter.
    #[arg(long, hide = true)]
    pub(crate) permission_hook: bool,

    /// Internal Codex lifecycle hook adapter.
    #[arg(long, hide = true)]
    pub(crate) lifecycle_hook: bool,

    /// Internal guarded Stop recovery adapter.
    #[arg(long, hide = true)]
    pub(crate) recovery_hook: bool,

    /// Internal hook provider dispatch.
    #[arg(long, hide = true, value_enum)]
    pub(crate) provider: Option<provider_hooks::HookProvider>,

    /// Trusted Antigravity hook event selected by the hook registration.
    #[arg(long, hide = true)]
    pub(crate) antigravity_hook_event: Option<String>,

    /// Internal one-shot preference distiller.
    #[arg(long, hide = true)]
    pub(crate) distill_once: bool,

    /// Tool name for --brain-query (e.g., "Bash", "Write", "Edit")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool: Option<String>,

    /// Command or input for --brain-query (e.g., "rm -rf /tmp")
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool_input: Option<String>,

    /// Project name for --brain-query context (defaults to current directory name)
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) project: Option<String>,

    /// Record a tool-call outcome to the pending-outcomes spool.
    /// Used by the Codex PostToolUse hook for #220 baselining.
    /// Reads pending-outcome JSON from stdin (preferred) or builds one from
    /// --tool, --tool-input, --project, --exit-code, --duration-ms, --stderr-tail.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) record_outcome: bool,

    /// Tool exit code for --record-outcome (0 = success).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) exit_code: Option<i32>,

    /// Tool wall-clock duration in milliseconds for --record-outcome.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) duration_ms: Option<u64>,

    /// Tail of stderr / tool error output for --record-outcome
    /// (truncated to MAX_STDERR_TAIL_BYTES).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) stderr_tail: Option<String>,

    /// Codex session id (passed through hook payload), optional.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) session_id: Option<String>,

    /// Codex tool call id (passed through hook payload), optional.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) tool_use_id: Option<String>,

    /// Reap pending outcomes: attribute each to a matching decision and
    /// archive orphans older than 24h. Exits with the reap stats as JSON
    /// when --json is set, otherwise human-readable.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) reap_outcomes: bool,

    /// List resolved tool-call outcomes attributed to brain decisions.
    /// Filterable by --tool and --project. Honours --json.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_outcomes: bool,

    /// Rank approaches by outcome data (success_rate * sample_count).
    /// Filterable by --tool and --project. Honours --json.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_baseline: bool,

    /// Limit baseline ranking output to top N rows.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) top: Option<usize>,

    /// Show auto-generated insights, or set mode (on/off/status).
    /// Requires Brain mode on or auto.
    /// Without argument: show current insights.
    /// With argument: set insights mode (on = auto-generate, off = disable).
    #[arg(long, help_heading = "Brain (Local LLM)", num_args = 0..=1, default_missing_value = "")]
    pub(crate) insights: Option<String>,

    /// Propose AGENTS.md additions from high-confidence brain preferences.
    /// Use --project to scope to a specific project's preferences, and --apply
    /// to write the suggestions to AGENTS.md.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_garden: bool,

    /// Apply changes alongside actions that propose them
    /// (currently used with --brain-garden).
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) apply: bool,

    /// Print a markdown session briefing for the given --project.
    /// Aggregates recent decisions, learned preferences, and known
    /// anti-patterns into context suitable for injection at session start.
    #[arg(long, help_heading = "Brain (Local LLM)")]
    pub(crate) brain_briefing: bool,

    // ── Subcommands ──────────────────────────────────────────────────
    #[command(subcommand)]
    command: Option<Command>,

    // ── History & Diagnostics ──────────────────────────────────────────
    /// Run post-mortem analysis on a completed session transcript
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) autopsy: bool,

    /// Session ID or JSONL path for --autopsy (defaults to most recent session)
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) session: Option<String>,

    /// List configured event hooks and exit
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) hooks: bool,

    /// Write diagnostic logs to a file (for debugging/bug reports)
    #[arg(long, help_heading = "History & Diagnostics")]
    pub(crate) log: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunMode {
    BrainTui,
    Headless { json: bool },
}

fn select_mode(cli: &Cli) -> RunMode {
    if cli.headless {
        RunMode::Headless { json: cli.json }
    } else {
        RunMode::BrainTui
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let is_internal_hook =
        cli.permission_hook || cli.lifecycle_hook || cli.recovery_hook || cli.distill_once;
    let result = run_main(cli);
    if result.is_ok() && !is_internal_hook {
        maybe_print_star_prompt();
    }
    result
}

fn maybe_print_star_prompt() {
    let marker = coding_brain_core::paths::CodingBrainPaths::resolve(
        &coding_brain_core::paths::PathEnvironment::current(),
    )
    .map(|paths| paths.state_root().join(".star-prompted"))
    .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain/.star-prompted"));

    let first_run = !marker.exists();

    if first_run {
        eprintln!();
        eprintln!(
            "\u{2b50} If Coding Brain is useful, star it: https://github.com/aleadag/codexctl"
        );

        if first_run {
            if let Some(parent) = marker.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&marker, "");
        }
    }
}

/// First-run detection for the activation nudge (#322). Returns true when
/// the user has neither onboarded nor installed Coding Brain Codex hooks.
/// entries). When either is present, we assume the operator
/// knows what they're doing and stay quiet.
fn is_first_run() -> bool {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return false;
    };
    let marker = init::marker::default_path();
    let settings = home.join(".codex").join("hooks.json");
    let onboarded = marker.exists();
    let hooked = std::fs::read_to_string(&settings)
        .map(|s| s.contains("coding-brain"))
        .unwrap_or(false);
    !onboarded && !hooked
}

/// One-screen banner shown above Brain when the user hasn't
/// onboarded yet (#322). Goes to stderr so it doesn't pollute stdout
/// piping; appears before the alt-screen swap.
fn print_first_run_banner() {
    eprintln!();
    eprintln!("┌─────────────────────────────────────────────────────────────────┐");
    eprintln!("│  Welcome to Coding Brain.                                       │");
    eprintln!("│                                                                 │");
    eprintln!("│  Brain activity will be empty until Codex hooks are installed.  │");
    eprintln!("│  Quit and run:                                                   │");
    eprintln!("│                                                                 │");
    eprintln!("│    coding-brain init    Interactive setup wizard                │");
    eprintln!("│                                                                 │");
    eprintln!("│  Silence this with CODING_BRAIN_SKIP_FIRST_RUN=1.             │");
    eprintln!("└─────────────────────────────────────────────────────────────────┘");
    eprintln!();
    // Tiny delay so the user actually reads the banner before the TUI
    // grabs the alt-screen and hides it.
    std::thread::sleep(std::time::Duration::from_millis(800));
}

fn apply_brain_cli_overrides(cfg: &mut config::Config, cli: &Cli) {
    if cli.url.is_some() || cli.brain_model.is_some() {
        let brain = cfg.brain.get_or_insert_with(|| config::BrainConfig {
            enabled: false,
            ..config::BrainConfig::default()
        });
        if let Some(ref endpoint) = cli.url {
            brain.endpoint = endpoint.clone();
        }
        if let Some(ref model) = cli.brain_model {
            brain.model = model.clone();
        }
    }
}

fn early_config_action(cli: &Cli) -> Option<&ConfigAction> {
    match cli.command.as_ref() {
        Some(Command::Config { action }) => Some(action),
        _ => None,
    }
}

fn run_main(cli: Cli) -> io::Result<()> {
    if cli.distill_once {
        let paths = brain::distill::current_paths()?;
        brain::distill::run_once(&paths).map_err(io::Error::other)?;
        return Ok(());
    }
    if cli.lifecycle_hook {
        lifecycle_hook::run(
            cli.provider.map(Into::into).unwrap_or_default(),
            cli.antigravity_hook_event.as_deref(),
        );
        return Ok(());
    }

    // Initialize diagnostic logger if --log is set
    if let Some(ref log_path) = cli.log {
        if let Err(e) = logger::init(log_path) {
            eprintln!("Warning: could not open log file {log_path}: {e}");
        }
    }

    // Load config from files, then let endpoint/model flags override.
    let mut cfg = config::Config::load();
    for (path, warning) in config::legacy_config_warnings() {
        eprintln!(
            "Warning: {}:{}: {}",
            path.display(),
            warning.line,
            warning.message
        );
    }

    apply_brain_cli_overrides(&mut cfg, &cli);
    if cli.recovery_hook {
        brain::recovery::run_hook(
            cfg.brain.as_ref(),
            cli.provider.map(Into::into).unwrap_or_default(),
            cli.antigravity_hook_event.as_deref(),
        );
        return Ok(());
    }
    if let Some(action) = early_config_action(&cli) {
        return match action {
            ConfigAction::Show => {
                cfg.print_resolved();
                Ok(())
            }
            ConfigAction::Get { key } => commands::run_config_get(&cfg, key),
            ConfigAction::Set { key, value } => commands::run_config_set(key, value),
            ConfigAction::Template => {
                config::Config::print_template();
                Ok(())
            }
            ConfigAction::Validate => commands::validate_config(),
            ConfigAction::Init => commands::write_config_init(),
        };
    }
    if cli.permission_hook {
        brain::permission_hook::run(
            cfg.brain.as_ref(),
            cli.provider.map(Into::into).unwrap_or_default(),
            cli.antigravity_hook_event.as_deref(),
        );
        return Ok(());
    }
    let model_active = matches!(
        brain::resolve_gate_mode(cfg.brain.as_ref()).mode,
        coding_brain_core::runtime::BrainGateMode::On
            | coding_brain_core::runtime::BrainGateMode::Auto
    );
    if let Some(brain) = cfg.brain.as_ref().filter(|_| model_active) {
        if let Some(warning) = doctor::endpoint_warning(&brain.endpoint) {
            eprintln!("Warning: {warning}");
        }
    }

    // Load event hooks from config
    let hook_registry = config::load_hooks();

    if cli.hooks {
        hook_registry.print_list();
        return Ok(());
    }

    if cli.brain_prompts {
        println!("Brain Prompt Templates");
        println!("======================");
        for (name, source) in brain::prompts::list_prompts() {
            println!("  {name}: {source}");
        }
        println!();
        println!("Override: create a prompt under the Coding Brain state directory.");
        return Ok(());
    }

    if cli.brain_eval {
        let brain_cfg = cfg.brain.clone().unwrap_or_default();
        println!("Loading eval scenarios...");
        let scenarios = brain::evals::load_scenarios();
        println!(
            "Running {} scenarios against {}...",
            scenarios.len(),
            brain_cfg.endpoint
        );
        println!();
        let results = brain::evals::run_evals(&brain_cfg, &scenarios);
        brain::evals::print_results(&results);
        return Ok(());
    }

    if let Some(ref subcommand) = cli.brain_stats {
        brain::metrics::dispatch(subcommand);
        return Ok(());
    }

    if let Some(ref id) = cli.brain_mark_canonical {
        match brain::review::mark_by_id(id, None) {
            Ok(()) => {
                println!("Marked decision {id} as canonical.");
                return Ok(());
            }
            Err(e) => {
                eprintln!("Could not mark decision {id} canonical: {e}");
                std::process::exit(1);
            }
        }
    }

    if let Some(ref maybe_arg) = cli.brain_review {
        match maybe_arg.as_deref() {
            Some("list") | Some("queue") | Some("print") => {
                brain::review::print_queue();
            }
            _ => {
                brain::review::run_interactive();
            }
        }
        return Ok(());
    }

    if let Some(ref command) = cli.command {
        match command {
            Command::Config { .. } => unreachable!("config commands are dispatched after loading"),
            Command::Init {
                providers,
                check,
                reset,
                remove,
                purge,
                yes,
                plugin_only,
                upgrade,
                non_interactive,
                brain_url,
                skip_brain,
                install_plugin,
                skip_plugin,
                skip_skills,
            } => {
                let providers = normalize_init_providers(providers)
                    .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
                if *check {
                    return init::run_check();
                }
                if *reset {
                    return init::run_reset();
                }
                if *remove {
                    return init::run_remove();
                }
                if *purge {
                    return init::run_purge(*yes);
                }
                if *upgrade {
                    // #327 — re-sync after `brew upgrade`. Hooks + marker version, with a
                    // per-step report.
                    return init::run_upgrade();
                }
                if *plugin_only {
                    // #325 — install just the hook
                    // entries. The other three wizard phases stay where
                    // the previous run left them (no marker rewrite).
                    eprintln!(
                        "warning: --plugin-only is deprecated; use `coding-brain init codex` instead"
                    );
                    return init::phases::install_plugin_now();
                }
                if *non_interactive {
                    let providers = if providers.is_empty() {
                        eprintln!(
                            "warning: provider-less --non-interactive is deprecated; use `coding-brain init codex --non-interactive` instead"
                        );
                        vec![AgentProvider::Codex]
                    } else {
                        providers
                    };
                    let install_plugin_opt = if *skip_plugin {
                        Some(false)
                    } else if *install_plugin {
                        Some(true)
                    } else {
                        None
                    };
                    let answers = init::phases::Answers {
                        brain_url: brain_url.clone(),
                        skip_brain: *skip_brain,
                        install_plugin: install_plugin_opt,
                        skip_skills: *skip_skills,
                    };
                    return init::run_non_interactive(&answers, &providers);
                }
                let providers = if providers.is_empty() {
                    let detected = init::state::detect_provider_executables();
                    init::prompt::select_providers(&detected)?
                } else {
                    providers
                };
                return init::run_wizard(&providers);
            }

            Command::Completions { shell } => {
                let mut cmd = Cli::command();
                let name = cmd.get_name().to_string();
                clap_complete::generate(*shell, &mut cmd, name, &mut io::stdout());
                return Ok(());
            }

            Command::Man => {
                let cmd = Cli::command();
                clap_mangen::Man::new(cmd)
                    .render(&mut io::stdout())
                    .map_err(io::Error::other)?;
                return Ok(());
            }

            Command::Doctor { json } => {
                let checks = doctor::run_all_checks();
                if *json {
                    println!("{}", doctor::render_checks_json(&checks)?);
                } else {
                    print!("{}", doctor::render_checks(&checks));
                }
                let code = doctor::exit_code(&checks);
                if code != 0 {
                    // Use a clean process exit so the caller (CI script,
                    // shell pipeline) sees a non-zero status without us
                    // printing a backtrace.
                    std::process::exit(code);
                }
                return Ok(());
            }
        }
    }

    if cli.brain_query {
        return commands::run_brain_query(&cfg, &cli);
    }

    if cli.record_outcome {
        return commands::run_record_outcome(&cli);
    }

    if cli.reap_outcomes {
        return commands::run_reap_outcomes(&cli);
    }

    if cli.brain_outcomes {
        return commands::run_brain_outcomes(&cli);
    }

    if cli.brain_baseline {
        return commands::run_brain_baseline(&cli);
    }

    if let Some(ref insights_arg) = cli.insights {
        return commands::run_insights(&cfg, insights_arg);
    }

    if cli.brain_garden {
        return commands::run_brain_garden(&cli);
    }

    if cli.brain_briefing {
        return commands::run_brain_briefing(&cli);
    }

    if cli.autopsy {
        return commands::run_autopsy(cli.session.as_deref(), cli.json);
    }

    if let RunMode::Headless { json } = select_mode(&cli) {
        return commands::run_headless(Duration::from_secs(2), &cfg, json);
    }

    catch_up_distillation();
    let theme_mode = theme::ThemeMode::detect(cli.theme.as_deref().or(cfg.theme.as_deref()));
    let app_theme = theme::Theme::from_mode(theme_mode);

    // #322 — first-run nudge. If the user has neither onboarded nor installed
    // hooks, explain why Brain activity will initially be empty.
    if std::env::var("CODING_BRAIN_SKIP_FIRST_RUN").is_err() && is_first_run() {
        print_first_run_banner();
    }

    debug_assert_eq!(select_mode(&cli), RunMode::BrainTui);
    launch_brain_tui(app_theme)
}

fn launch_brain_tui(theme: theme::Theme) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error);
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            return Err(error);
        }
    };
    let app = coding_brain_tui::brain_app::BrainApp::new(runtime::build_brain_runtime(), theme);
    let result = run_brain_tui(&mut terminal, app);
    let raw_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();
    result.and(raw_result).and(screen_result).and(cursor_result)
}

fn run_brain_tui<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    mut app: coding_brain_tui::brain_app::BrainApp,
) -> io::Result<()> {
    use coding_brain_core::runtime::BrainEffect;
    use coding_brain_tui::terminal_suspend::{CrosstermTerminalControl, navigate_to_session};

    loop {
        terminal.draw(|frame| ui::brain::render(frame, &app))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && let Some(effect) = app.handle_key(key)
        {
            match effect {
                BrainEffect::Exit => return Ok(()),
                BrainEffect::SwitchToSession(target) => {
                    let navigation = app.navigation();
                    let result = navigate_to_session(
                        &CrosstermTerminalControl,
                        navigation.as_ref(),
                        &target,
                    );
                    app.complete_navigation(result);
                    terminal.clear()?;
                }
            }
        }
        app.refresh_if_due();
    }
}

fn catch_up_distillation() {
    let result = brain::distill::current_paths()
        .map_err(brain::distill::DistillError::Io)
        .and_then(|paths| brain::distill::run_once(&paths));
    if let Err(error) = result {
        eprintln!("Warning: Coding Brain preference catch-up failed: {error}");
    }
}

#[cfg(test)]
mod default_brain_cli_tests {
    use super::*;

    #[test]
    fn no_mode_selects_brain_tui() {
        let cli = Cli::try_parse_from(["coding-brain"]).unwrap();

        assert_eq!(select_mode(&cli), RunMode::BrainTui);
    }

    #[test]
    fn headless_is_the_only_continuous_non_tui_mode() {
        let cli = Cli::try_parse_from(["coding-brain", "--headless", "--json"]).unwrap();

        assert_eq!(select_mode(&cli), RunMode::Headless { json: true });
    }

    #[test]
    fn endpoint_and_model_overrides_do_not_enable_the_brain() {
        for args in [
            ["coding-brain", "--url", "http://localhost:9999"],
            ["coding-brain", "--brain-model", "local-model"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let mut config = config::Config::default();

            apply_brain_cli_overrides(&mut config, &cli);

            assert!(!config.brain.unwrap().enabled);
        }
    }

    #[test]
    fn config_subcommand_is_selected_ahead_of_brain_actions() {
        let cli = Cli::try_parse_from(["coding-brain", "--brain-eval", "config", "show"]).unwrap();

        assert!(matches!(
            early_config_action(&cli),
            Some(ConfigAction::Show)
        ));
    }
}

#[cfg(test)]
mod provider_init_cli_tests {
    use super::*;

    #[test]
    fn init_accepts_and_normalizes_positional_provider_selectors() {
        let cli =
            Cli::try_parse_from(["coding-brain", "init", "claude", "codex", "claude"]).unwrap();
        let Some(Command::Init { providers, .. }) = cli.command else {
            panic!("expected init command");
        };

        assert_eq!(
            normalize_init_providers(&providers).unwrap(),
            vec![AgentProvider::Codex, AgentProvider::Claude]
        );
    }

    #[test]
    fn init_all_is_exclusive_and_expands_to_every_provider() {
        assert_eq!(
            normalize_init_providers(&[InitProvider::All]).unwrap(),
            vec![
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ]
        );
        assert_eq!(
            normalize_init_providers(&[InitProvider::All, InitProvider::All]).unwrap(),
            vec![
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ]
        );
        assert!(normalize_init_providers(&[InitProvider::All, InitProvider::Codex]).is_err());
    }

    #[test]
    fn init_provider_selectors_conflict_with_administrative_modes_and_plugin_only() {
        for mode in [
            "--check",
            "--upgrade",
            "--remove",
            "--reset",
            "--purge",
            "--plugin-only",
        ] {
            assert!(
                Cli::try_parse_from(["coding-brain", "init", "claude", mode]).is_err(),
                "provider selector unexpectedly accepted with {mode}"
            );
        }
    }
}

#[cfg(test)]
mod brain_only_cli_tests {
    use super::*;

    #[test]
    fn clap_and_cargo_expose_only_coding_brain() {
        assert_eq!(Cli::command().get_name(), "coding-brain");
        assert_eq!(env!("CARGO_BIN_NAME"), "coding-brain");
    }

    const REMOVED_ARGS: &[&str] = &[
        "--interval",
        "--debug",
        "--demo",
        "--list",
        "--watch",
        "--summary",
        "--since",
        "--filter-status",
        "--focus",
        "--search",
        "--new",
        "--cwd",
        "--prompt",
        "--resume",
        "--budget",
        "--kill-on-budget",
        "--notify",
        "--webhook",
        "--webhook-on",
        "--terminal-auto-approve-fallback",
        "--record",
        "--duration",
        "--clean",
        "--older-than",
        "--finished",
        "--history",
        "--stats",
        "--scope",
        "--init",
        "--uninstall",
        "--doctor",
        "--brain",
        "--auto-run",
        "--mode",
        "--config",
        "--config-template",
        "--config-validate",
        "--config-init",
    ];

    const RETAINED_ARGS: &[&str] = &[
        "--headless",
        "--json",
        "--theme",
        "--url",
        "--brain-model",
        "--brain-eval",
        "--brain-prompts",
        "--brain-stats",
        "--brain-review",
        "--brain-mark-canonical",
        "--brain-query",
        "--tool",
        "--tool-input",
        "--project",
        "--record-outcome",
        "--exit-code",
        "--duration-ms",
        "--stderr-tail",
        "--session-id",
        "--tool-use-id",
        "--reap-outcomes",
        "--brain-outcomes",
        "--brain-baseline",
        "--top",
        "--insights",
        "--brain-garden",
        "--apply",
        "--brain-briefing",
        "--autopsy",
        "--session",
        "--hooks",
        "--log",
    ];

    #[test]
    fn removed_args_fail_and_retained_args_are_in_long_help() {
        let help = Cli::command().render_long_help().to_string();
        for arg in REMOVED_ARGS {
            assert!(Cli::try_parse_from(["coding-brain", arg]).is_err(), "{arg}");
            assert!(!help_has_long_flag(&help, arg), "{arg}");
        }
        for arg in RETAINED_ARGS {
            assert!(help_has_long_flag(&help, arg), "{arg}");
        }
    }

    fn help_has_long_flag(help: &str, flag: &str) -> bool {
        help.split_whitespace().any(|token| {
            let token = token.trim_matches(|character: char| matches!(character, ',' | '[' | ']'));
            token == flag
                || token
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    }

    #[test]
    fn removed_product_surfaces_are_rejected() {
        for command in [
            "coord",
            "bus",
            "relay",
            "hive",
            "supervisor",
            "loop",
            "ingest",
        ] {
            assert!(
                Cli::try_parse_from(["coding-brain", command]).is_err(),
                "{command}"
            );
        }

        assert!(Cli::try_parse_from(["coding-brain", "--run", "tasks.json"]).is_err());
        assert!(Cli::try_parse_from(["coding-brain", "--parallel"]).is_err());
        assert!(Cli::try_parse_from(["coding-brain", "--decompose", "split this work"]).is_err());
    }

    #[test]
    fn generated_help_omits_removed_surfaces() {
        let help = Cli::command().render_long_help().to_string();
        for command in [
            "coord",
            "bus",
            "relay",
            "hive",
            "supervisor",
            "loop",
            "ingest",
        ] {
            assert!(
                !help.lines().any(|line| {
                    line.trim_start().strip_prefix(command).is_some_and(|rest| {
                        rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')
                    })
                }),
                "{command} remains in generated help"
            );
        }
        for flag in ["--run", "--parallel", "--decompose"] {
            assert!(!help.contains(flag), "{flag} remains in generated help");
        }
    }

    #[test]
    fn persistent_mode_uses_config_subcommand() {
        let cli = Cli::try_parse_from(["coding-brain", "config", "set", "mode", "auto"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Config {
                action: ConfigAction::Set { ref key, ref value }
            }) if key == "mode" && value == "auto"
        ));
    }

    #[test]
    fn overlapping_brain_flags_are_removed() {
        for flag in [
            "--brain",
            "--auto-run",
            "--mode",
            "--config",
            "--config-template",
            "--config-validate",
            "--config-init",
        ] {
            assert!(
                Cli::try_parse_from(["coding-brain", flag]).is_err(),
                "{flag}"
            );
        }
    }
}

#[cfg(test)]
mod first_run_tests {
    use super::*;
    use std::fs;

    fn set_home(p: &std::path::Path) {
        // Cargo's test harness shares a process; reset HOME after each test
        // by calling this with the original value (we just leak temp dirs
        // since they're under /tmp anyway).
        // SAFETY: tests are serialized via HOME_ENV_LOCK; nothing else
        // here races on env reads inside the lock window.
        unsafe { std::env::set_var("HOME", p) };
    }

    #[test]
    fn first_run_when_neither_marker_nor_hooks_present() {
        let _g = config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        set_home(tmp.path());
        assert!(is_first_run(), "fresh home should be first-run");
    }

    #[test]
    fn not_first_run_when_onboarding_marker_present() {
        let _g = config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".local/state/coding-brain")).unwrap();
        fs::write(
            tmp.path().join(".local/state/coding-brain/onboarding.json"),
            "{}",
        )
        .unwrap();
        set_home(tmp.path());
        assert!(
            !is_first_run(),
            "onboarding marker present should suppress first-run"
        );
    }

    #[test]
    fn not_first_run_when_settings_mentions_coding_brain() {
        let _g = config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        fs::write(
            tmp.path().join(".codex").join("hooks.json"),
            r#"{"hooks":{"PostToolUse":[{"hooks":[{"command":"coding-brain --lifecycle-hook"}]}]}}"#,
        )
        .unwrap();
        set_home(tmp.path());
        assert!(
            !is_first_run(),
            "hook install should suppress first-run even without onboarding marker"
        );
    }

    #[test]
    fn not_first_run_when_home_missing() {
        let _g = config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: serialized via HOME_ENV_LOCK; nothing else reads HOME inside
        // this critical section.
        unsafe { std::env::remove_var("HOME") };
        assert!(
            !is_first_run(),
            "no HOME should be treated as not-first-run (no nudge possible)"
        );
    }
}

#[cfg(test)]
mod permission_hook_cli_tests {
    use super::*;

    #[test]
    fn permission_hook_flag_is_hidden() {
        let cli = Cli::try_parse_from(["coding-brain", "--permission-hook"]).unwrap();
        assert!(cli.permission_hook);
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("--permission-hook"));
    }

    #[test]
    fn lifecycle_hook_flag_is_hidden() {
        let cli = Cli::try_parse_from([
            "coding-brain",
            "--lifecycle-hook",
            "--provider",
            "antigravity",
            "--antigravity-hook-event",
            "Stop",
        ])
        .unwrap();
        assert!(cli.lifecycle_hook);
        assert_eq!(
            cli.provider,
            Some(provider_hooks::HookProvider::Antigravity)
        );
        assert_eq!(cli.antigravity_hook_event.as_deref(), Some("Stop"));
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("--lifecycle-hook"));
        assert!(!help.contains("--provider"));
        assert!(!help.contains("--antigravity-hook-event"));
    }

    #[test]
    fn recovery_hook_flag_is_hidden_and_provider_scoped() {
        let cli = Cli::try_parse_from([
            "coding-brain",
            "--recovery-hook",
            "--provider",
            "antigravity",
            "--antigravity-hook-event",
            "Stop",
        ])
        .unwrap();
        assert!(cli.recovery_hook);
        assert_eq!(
            cli.provider,
            Some(provider_hooks::HookProvider::Antigravity)
        );
        assert_eq!(cli.antigravity_hook_event.as_deref(), Some("Stop"));
        assert!(
            !Cli::command()
                .render_long_help()
                .to_string()
                .contains("--recovery-hook")
        );
    }

    #[test]
    fn distill_once_flag_is_hidden() {
        let cli = Cli::try_parse_from(["coding-brain", "--distill-once"]).unwrap();
        assert!(cli.distill_once);
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("--distill-once"));
    }
}
