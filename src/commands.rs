//! CLI subcommand handlers extracted from main.rs.
//!
//! Each function implements a standalone CLI mode (--doctor, --clean, --list, etc.)
//! called from `run_main()` dispatch in main.rs.

use std::io;
use std::time::Duration;

use crate::Cli;
use crate::ViewFilters;
use crate::app::{App, FocusFilter, StatusFilter};
use crate::brain;
use crate::config;
use crate::demo;
use crate::discovery;
use crate::launch;
use crate::process;
use crate::session;

pub(crate) fn launch_session(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
) -> io::Result<()> {
    let request = launch::prepare(cwd, prompt, resume).map_err(io::Error::other)?;

    match launch::launch(&request) {
        Ok(target) => {
            println!(
                "Launched Codex session in {} at {}{}",
                target,
                request.cwd_path.display(),
                request.option_summary()
            );
            Ok(())
        }
        Err(e) => Err(io::Error::other(e)),
    }
}

fn print_doctor_transcripts() {
    println!();
    println!("Transcript Discovery");

    let sessions_dir = discovery::projects_dir();

    let sessions_exists = sessions_dir.exists();
    println!(
        "  [{}] Codex sessions dir: {}",
        if sessions_exists { "ok" } else { "!!" },
        sessions_dir.display()
    );

    if !sessions_exists {
        println!("      No Codex transcripts found — Codex may not have run yet");
        return;
    }

    let mut sessions = discovery::scan_sessions();
    if sessions.is_empty() {
        println!("  [--] no Codex transcripts found");
        return;
    }

    process::fetch_and_enrich(&mut sessions);
    let alive: Vec<_> = sessions
        .iter()
        .filter(|s| s.status != session::SessionStatus::Finished)
        .collect();

    if alive.is_empty() {
        println!("  [--] no active Codex sessions");
        return;
    }

    let mut alive_sessions: Vec<_> = alive.into_iter().cloned().collect();
    for s in &mut alive_sessions {
        discovery::resolve_jsonl_paths(std::slice::from_mut(s));
    }

    for s in &alive_sessions {
        let found = s.jsonl_path.is_some();

        println!(
            "  [{}] PID {} ({})",
            if found { "ok" } else { "!!" },
            s.pid,
            s.project_name
        );
        println!("      cwd:  {}", s.cwd);
        println!("      session: {}", s.session_id);
        if let Some(ref path) = s.jsonl_path {
            println!("      jsonl: {}", path.display());
        } else {
            println!(
                "      fix: check that the rollout JSONL still exists under ~/.codex/sessions"
            );
        }
    }
}

pub(crate) fn print_doctor() -> io::Result<()> {
    use crate::terminals;

    let report = terminals::doctor_report();
    println!("{}", terminals::format_doctor_report(&report));

    // Transcript discovery diagnostics
    print_doctor_transcripts();

    // Brain diagnostics
    let cfg = config::Config::load();
    println!();
    println!("Brain (local LLM)");

    // Check curl
    let curl_ok = std::process::Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    println!(
        "  [{}] curl: {}",
        if curl_ok { "ok" } else { "!!" },
        if curl_ok {
            "available (required for brain HTTP calls)"
        } else {
            "not found — brain requires curl on PATH"
        }
    );

    // Check ollama binary
    let ollama_ok = std::process::Command::new("ollama")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    println!(
        "  [{}] ollama: {}",
        if ollama_ok { "ok" } else { "--" },
        if ollama_ok {
            "installed"
        } else {
            "not found (install: brew install ollama)"
        }
    );

    // Check endpoint reachability
    if let Some(ref brain) = cfg.brain {
        println!(
            "  Config: enabled={}, model={}, auto={}, few_shot={}",
            brain.enabled, brain.model, brain.auto_mode, brain.few_shot_count
        );
        let endpoint_ok = check_brain_endpoint(&brain.endpoint, brain.timeout_ms);
        println!(
            "  [{}] endpoint {}: {}",
            if endpoint_ok { "ok" } else { "!!" },
            brain.endpoint,
            if endpoint_ok {
                "reachable"
            } else {
                "not reachable"
            }
        );
        if !endpoint_ok {
            println!("      fix: start ollama with `ollama serve`, or check --brain-endpoint URL");
        }
    } else {
        println!("  Config: not configured");
        println!("  To enable: add [brain] section to .codexctl.toml or use --brain flag");
    }

    Ok(())
}

pub(crate) fn validate_config() -> io::Result<()> {
    use std::path::PathBuf;

    let mut total_warnings = 0;
    let mut any_errors = false;

    let files: Vec<PathBuf> = [
        config::Config::global_path(),
        Some(PathBuf::from(".codexctl.toml")),
    ]
    .into_iter()
    .flatten()
    .collect();

    for path in &files {
        if !path.exists() {
            println!("  {}: not found (skipped)", path.display());
            continue;
        }
        let (warnings, has_errors) = config::validate_config_file(path);
        any_errors |= has_errors;

        if warnings.is_empty() {
            println!("  {}: ok", path.display());
        } else {
            println!("  {}:", path.display());
            for w in &warnings {
                let prefix = if w.message.starts_with("unknown") {
                    "warn"
                } else {
                    "error"
                };
                if w.line > 0 {
                    println!("    [{prefix}] line {}: {}", w.line, w.message);
                } else {
                    println!("    [{prefix}] {}", w.message);
                }
            }
            total_warnings += warnings.len();
        }
    }

    println!();
    if total_warnings == 0 {
        println!("Config is valid.");
    } else {
        println!(
            "{total_warnings} warning(s) found. Unknown keys are ignored but may indicate typos."
        );
    }

    if any_errors {
        Err(io::Error::other("config has errors"))
    } else {
        Ok(())
    }
}

pub(crate) fn write_config_init() -> io::Result<()> {
    let path = std::path::PathBuf::from(".codexctl.toml");
    if path.exists() {
        eprintln!("Error: .codexctl.toml already exists. Remove it first or edit directly.");
        return Err(io::Error::other(".codexctl.toml already exists"));
    }

    let template = config::Config::template_string();
    std::fs::write(&path, template).map_err(|e| io::Error::other(format!("write: {e}")))?;
    println!("Created .codexctl.toml with annotated defaults.");
    println!("Edit the file to customize, then run `codexctl --config-validate` to check.");
    Ok(())
}

pub(crate) fn check_brain_endpoint(endpoint: &str, timeout_ms: u64) -> bool {
    let timeout_secs = (timeout_ms / 1000).max(1);
    std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            &timeout_secs.to_string(),
            endpoint,
        ])
        .output()
        .is_ok_and(|o| {
            let code = String::from_utf8_lossy(&o.stdout);
            // Any HTTP response (even 404/405) means the server is up
            code.trim() != "000"
        })
}

pub(crate) fn parse_duration_str(s: &str) -> Duration {
    let s = s.trim();
    if let Some(hours) = s.strip_suffix('h') {
        if let Ok(h) = hours.parse::<u64>() {
            return Duration::from_secs(h * 3600);
        }
    }
    if let Some(mins) = s.strip_suffix('m') {
        if let Ok(m) = mins.parse::<u64>() {
            return Duration::from_secs(m * 60);
        }
    }
    if let Some(days) = s.strip_suffix('d') {
        if let Ok(d) = days.parse::<u64>() {
            return Duration::from_secs(d * 86400);
        }
    }
    Duration::from_secs(24 * 3600) // default 24h
}

pub(crate) fn parse_status_filter(value: Option<&str>) -> io::Result<StatusFilter> {
    match value {
        Some(raw) => StatusFilter::parse(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid --filter-status value: {raw}. Expected one of: all, needs-input, processing, waiting, unknown, idle, finished"
                ),
            )
        }),
        None => Ok(StatusFilter::All),
    }
}

pub(crate) fn parse_focus_filter(value: Option<&str>) -> io::Result<FocusFilter> {
    match value {
        Some(raw) => FocusFilter::parse(raw).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Invalid --focus value: {raw}. Expected one of: all, attention, over-budget, high-context, unknown-telemetry, conflict"
                ),
            )
        }),
        None => Ok(FocusFilter::All),
    }
}

pub(crate) fn apply_filters(app: &mut App, filters: &ViewFilters) {
    app.status_filter = filters.status_filter;
    app.focus_filter = filters.focus_filter;
    app.search_query = filters.search.trim().to_string();
    app.search_buffer.clear();
    app.search_mode = false;
    let len = app.visible_session_count();
    if len == 0 {
        app.table_state.select(None);
    } else if app.table_state.selected().is_none() {
        app.table_state.select(Some(0));
    } else if let Some(sel) = app.table_state.selected() {
        if sel >= len {
            app.table_state.select(Some(len - 1));
        }
    }
}

/// Build a per-project session briefing (#198) and print to stdout.
pub(crate) fn run_brain_briefing(cli: &Cli) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let project = cli.project.clone().or_else(|| {
        cwd.file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
    });
    let opts = brain::briefing::BriefingOptions {
        project,
        max_decisions: None,
        include_agents_md_check: true,
    };
    let briefing = brain::briefing::build_briefing(&opts, &cwd);
    if cli.json {
        let json = serde_json::json!({
            "project": opts.project,
            "briefing": briefing,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("{briefing}");
    }
    Ok(())
}

/// Propose AGENTS.md additions from high-confidence brain preferences (#199).
pub(crate) fn run_brain_garden(cli: &Cli) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let report = brain::garden::run_garden(cli.project.as_deref(), cli.apply, &cwd);
    if cli.json {
        let json = serde_json::json!({
            "project": report.project,
            "agents_md_path": report.agents_md_path.as_ref().map(|p| p.display().to_string()),
            "considered": report.considered,
            "already_covered": report.already_covered,
            "applied": report.applied,
            "suggestions": report.kept.iter().map(|s| serde_json::json!({
                "kind": match s.kind {
                    brain::garden::SuggestionKind::Codify => "codify",
                    brain::garden::SuggestionKind::Contradiction => "contradiction",
                },
                "line": s.line,
                "rationale": s.rationale,
                "tool": s.tool,
                "cmd_keyword": s.cmd_keyword,
            })).collect::<Vec<_>>(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("{}", brain::garden::format_report(&report));
    }
    Ok(())
}

/// Run a post-mortem autopsy on a completed session transcript.
/// Resolves the session from: direct JSONL path, session ID, or most recent.
pub(crate) fn run_autopsy(session_arg: Option<&str>, json_output: bool) -> io::Result<()> {
    let jsonl_path = resolve_jsonl_for_autopsy(session_arg)?;

    eprintln!("Analyzing: {}", jsonl_path.display());

    let mut report = brain::autopsy::run_autopsy(&jsonl_path).map_err(io::Error::other)?;

    // Try to infer project from directory name
    if let Some(parent) = jsonl_path.parent() {
        if let Some(name) = parent.file_name().and_then(|n| n.to_str()) {
            report.project = name.to_string();
        }
    }

    if json_output {
        let json = brain::autopsy::report_to_json(&report);
        println!(
            "{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );
    } else {
        print!("{}", brain::autopsy::format_report(&report));
    }

    // Save the report
    match brain::autopsy::save_report(&report) {
        Ok(path) => eprintln!("Saved: {}", path.display()),
        Err(e) => eprintln!("Warning: could not save autopsy report: {e}"),
    }

    Ok(())
}

/// Resolve a session reference to a JSONL path.
/// Accepts: a .jsonl file path, a session ID, or None (most recent).
fn resolve_jsonl_for_autopsy(session_arg: Option<&str>) -> io::Result<std::path::PathBuf> {
    if let Some(arg) = session_arg {
        // Direct path?
        let path = std::path::PathBuf::from(arg);
        if arg.ends_with(".jsonl") && path.exists() {
            return Ok(path);
        }

        // Search for the session ID across Codex rollout transcripts.
        for candidate in collect_jsonl_files(&discovery::projects_dir()) {
            let filename = candidate
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if filename.contains(arg) {
                return Ok(candidate);
            }
        }

        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Session not found: {arg}"),
        ));
    }

    // No argument: find the most recently modified JSONL
    find_most_recent_jsonl()
}

/// Find the most recently modified JSONL file across Codex session directories.
fn find_most_recent_jsonl() -> io::Result<std::path::PathBuf> {
    let mut best: Option<(std::path::PathBuf, std::time::SystemTime)> = None;

    for path in collect_jsonl_files(&discovery::projects_dir()) {
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                let dominated = best.as_ref().is_none_or(|(_, t)| modified > *t);
                if dominated {
                    best = Some((path, modified));
                }
            }
        }
    }

    best.map(|(p, _)| p).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "No JSONL transcripts found. Run some Codex sessions first.",
        )
    })
}

fn collect_jsonl_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    collect_jsonl_files_inner(root, &mut files);
    files
}

fn collect_jsonl_files_inner(root: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files_inner(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

pub(crate) fn run_clean(
    older_than: Option<&str>,
    finished_only: bool,
    dry_run: bool,
) -> io::Result<()> {
    let min_age = older_than.map(parse_duration_str);
    let now = std::time::SystemTime::now();

    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"));

    // Collect active PIDs to avoid deleting live sessions
    let mut removed_jsonl = 0u64;
    let mut freed_bytes = 0u64;

    let sessions_dir = home.join(".codex/sessions");
    for file_path in collect_jsonl_files(&sessions_dir) {
        let metadata = match std::fs::metadata(&file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        // Check age if --older-than is set
        if let Some(min_age) = min_age {
            let modified = metadata.modified().ok();
            if let Some(modified) = modified {
                let age = now.duration_since(modified).unwrap_or_default();
                if age < min_age {
                    continue;
                }
            }
        }

        // If --finished only, skip JSONL files whose corresponding session is still active
        if finished_only {
            let app = App::new();
            let is_active = app.sessions.iter().any(|s| {
                s.jsonl_path
                    .as_ref()
                    .map(|p| p == &file_path)
                    .unwrap_or(false)
            });
            if is_active {
                continue;
            }
        }

        let size = metadata.len();
        if dry_run {
            println!("  would remove: {} ({} bytes)", file_path.display(), size);
        } else {
            let _ = std::fs::remove_file(&file_path);
        }
        removed_jsonl += 1;
        freed_bytes += size;
    }

    let freed_str = if freed_bytes >= 1_073_741_824 {
        format!("{:.1} GB", freed_bytes as f64 / 1_073_741_824.0)
    } else if freed_bytes >= 1_048_576 {
        format!("{:.1} MB", freed_bytes as f64 / 1_048_576.0)
    } else if freed_bytes >= 1024 {
        format!("{:.1} KB", freed_bytes as f64 / 1024.0)
    } else {
        format!("{freed_bytes} bytes")
    };

    if dry_run {
        println!();
        println!(
            "Dry run: would remove {} Codex transcripts, freeing {}",
            removed_jsonl, freed_str
        );
    } else if removed_jsonl == 0 {
        println!("Nothing to clean up.");
    } else {
        println!(
            "Removed {} Codex transcripts, freed {}",
            removed_jsonl, freed_str
        );
    }

    Ok(())
}

pub(crate) fn print_summary(since: &str) -> io::Result<()> {
    let since_duration = parse_duration_str(since);
    let app = App::new();

    if app.sessions.is_empty() {
        println!("No active Codex sessions.");
        return Ok(());
    }

    for s in &app.sessions {
        let status_color = match s.status {
            session::SessionStatus::Processing => "\x1b[32m",
            session::SessionStatus::NeedsInput => "\x1b[35m",
            session::SessionStatus::WaitingInput => "\x1b[33m",
            session::SessionStatus::Unknown => "\x1b[34m",
            session::SessionStatus::Idle => "\x1b[90m",
            session::SessionStatus::Finished => "\x1b[31m",
        };
        let reset = "\x1b[0m";
        let status_text = if s.status == session::SessionStatus::Unknown {
            format!("Unknown: {}", s.telemetry_label())
        } else {
            s.status.to_string()
        };

        println!(
            "=== {} ({}, {}, {status_color}{}{reset}) ===",
            s.display_name(),
            s.format_elapsed(),
            s.format_cost(),
            status_text,
        );

        // Git stats from session's cwd
        let since_secs = since_duration.as_secs();
        let git_since = format!("{since_secs} seconds ago");

        let git_log = std::process::Command::new("git")
            .args(["log", "--oneline", &format!("--since={git_since}")])
            .current_dir(&s.cwd)
            .output();

        if let Ok(output) = git_log {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let commits: Vec<&str> = stdout.lines().collect();
            if !commits.is_empty() {
                println!("  Commits: {}", commits.len());
                for c in commits.iter().take(5) {
                    println!("    {c}");
                }
                if commits.len() > 5 {
                    println!("    ... and {} more", commits.len() - 5);
                }
            }
        }

        let git_diff = std::process::Command::new("git")
            .args(["diff", "--stat", "HEAD"])
            .current_dir(&s.cwd)
            .output();

        if let Ok(output) = git_diff {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            if !lines.is_empty() {
                let file_count = lines.len().saturating_sub(1); // last line is summary
                if file_count > 0 {
                    println!("  Files changed: {file_count}");
                }
            }
        }

        // Token summary
        let total_tokens = s.total_input_tokens + s.total_output_tokens;
        if total_tokens > 0 {
            println!(
                "  Tokens: {} in / {} out",
                format_count(s.total_input_tokens),
                format_count(s.total_output_tokens)
            );
        }

        // Model and context
        if !s.model.is_empty() {
            let context_text = if s.has_usage_metrics() {
                format!("{}%", s.context_percent() as u32)
            } else {
                "n/a".to_string()
            };
            let estimate_note = if s.cost_estimate_unverified {
                " [fallback estimate]"
            } else if s.model_profile_source == "override" {
                " [config override]"
            } else {
                ""
            };
            println!(
                "  Model: {}{} (context: {})",
                s.model, estimate_note, context_text
            );
        }
        if s.status == session::SessionStatus::Unknown || !s.has_usage_metrics() {
            println!("  Telemetry: {}", s.telemetry_label());
        }

        if s.subagent_count > 0 {
            println!("  Subagents: {}", s.format_subagent_summary());
        }

        println!();
    }

    let total_cost: f64 = app.sessions.iter().map(|s| s.cost_usd).sum();
    println!("Total cost: ${total_cost:.2}");

    Ok(())
}

pub(crate) fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn make_app(demo: bool, filters: &ViewFilters) -> App {
    let mut app = if demo {
        let mut app = App::new();
        app.demo_mode = true;
        app.sessions = demo::generate_sessions(10);
        app
    } else {
        App::new()
    };
    apply_filters(&mut app, filters);
    app
}

pub(crate) fn print_json(demo: bool, filters: &ViewFilters) -> io::Result<()> {
    let app = make_app(demo, filters);
    let values: Vec<serde_json::Value> = app
        .visible_sessions()
        .iter()
        .map(|s| s.to_json_value())
        .collect();
    let json = serde_json::to_string_pretty(&values).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
    Ok(())
}

pub(crate) fn print_list(demo: bool, filters: &ViewFilters) -> io::Result<()> {
    let app = make_app(demo, filters);
    let visible_sessions = app.visible_sessions();

    if visible_sessions.is_empty() {
        if app.has_active_filters() {
            println!("No sessions match the current filters.");
        } else {
            println!("No active Codex sessions.");
        }
        if app.has_active_filters() {
            println!("  ({})", app.filter_summary());
        }
        return Ok(());
    }

    println!(
        "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6} {:<6} TOKENS",
        "PID", "PROJECT", "STATUS", "CTX%", "COST", "$/HR", "ELAPSED", "CPU%", "MEM"
    );
    println!("{}", "-".repeat(105));

    for s in visible_sessions {
        let status_text = if s.status == session::SessionStatus::Unknown {
            s.telemetry_status.short_label().to_string()
        } else {
            s.status.to_string()
        };
        println!(
            "{:<7} {:<16} {:<12} {:<8} {:<8} {:<9} {:<10} {:<6.1} {:<6} {}",
            s.pid,
            s.display_name(),
            status_text,
            s.format_context(),
            s.format_cost(),
            s.format_burn_rate(),
            s.format_elapsed(),
            s.cpu_percent,
            s.format_mem(),
            s.format_tokens(),
        );
    }

    let total_cost: f64 = app.visible_sessions().iter().map(|s| s.cost_usd).sum();
    println!("{}", "-".repeat(105));
    println!("Total cost: ${total_cost:.2}");
    if app.has_active_filters() {
        println!("{}", app.filter_summary());
    }

    Ok(())
}

pub(crate) fn run_watch(
    tick_rate: Duration,
    json_mode: bool,
    format_str: &str,
    filters: &ViewFilters,
) -> io::Result<()> {
    use crate::session::SessionStatus;
    use std::collections::HashMap;

    let mut app = App::new();
    apply_filters(&mut app, filters);
    let mut prev_statuses: HashMap<u32, SessionStatus> =
        app.sessions.iter().map(|s| (s.pid, s.status)).collect();

    // Print initial state for all sessions
    for s in app.visible_sessions() {
        if json_mode {
            let obj = serde_json::json!({
                "event": "initial",
                "pid": s.pid,
                "project": s.display_name(),
                "status": s.status.to_string(),
                "telemetry": s.telemetry_label(),
                "cost_usd": if s.has_usage_metrics() { serde_json::json!((s.cost_usd * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                "context_pct": if s.has_usage_metrics() { serde_json::json!((s.context_percent() * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                "elapsed_secs": s.elapsed.as_secs(),
            });
            println!("{}", serde_json::to_string(&obj).unwrap_or_default());
        } else {
            println!("{}", format_session(format_str, s));
        }
    }

    loop {
        std::thread::sleep(tick_rate);
        app.tick();
        let visible_pids: std::collections::HashSet<u32> =
            app.visible_sessions().iter().map(|s| s.pid).collect();

        for s in &app.sessions {
            let prev = prev_statuses.get(&s.pid).copied();
            let changed = prev.is_none_or(|p| p != s.status);

            if !changed || !visible_pids.contains(&s.pid) {
                continue;
            }

            if json_mode {
                let obj = serde_json::json!({
                    "event": "status_change",
                    "pid": s.pid,
                    "project": s.display_name(),
                    "old_status": prev.map(|p| p.to_string()).unwrap_or_default(),
                    "new_status": s.status.to_string(),
                    "telemetry": s.telemetry_label(),
                    "cost_usd": if s.has_usage_metrics() { serde_json::json!((s.cost_usd * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                    "context_pct": if s.has_usage_metrics() { serde_json::json!((s.context_percent() * 100.0).round() / 100.0) } else { serde_json::Value::Null },
                    "elapsed_secs": s.elapsed.as_secs(),
                });
                println!("{}", serde_json::to_string(&obj).unwrap_or_default());
            } else {
                println!("{}", format_session(format_str, s));
            }
        }

        prev_statuses = app.sessions.iter().map(|s| (s.pid, s.status)).collect();
    }
}

#[derive(Debug)]
struct HeadlessEvent {
    kind: &'static str,
    data: serde_json::Value,
}

fn headless_tick_events(
    app: &App,
    previous: &std::collections::HashMap<u32, crate::session::SessionStatus>,
) -> Vec<HeadlessEvent> {
    let mut events = app
        .sessions
        .iter()
        .filter(|session| {
            previous
                .get(&session.pid)
                .is_none_or(|old| *old != session.status)
        })
        .map(|session| HeadlessEvent {
            kind: "status_change",
            data: serde_json::json!({
                "pid": session.pid,
                "project": session.display_name(),
                "old_status": previous.get(&session.pid).map(ToString::to_string),
                "new_status": session.status.to_string(),
                "cost_usd": session.cost_usd,
                "context_pct": session.context_percent(),
                "decay_score": session.decay_score,
            }),
        })
        .collect::<Vec<_>>();

    if !app.status_msg.is_empty()
        && (app.status_msg.starts_with("Brain:") || app.status_msg.starts_with("MAILBOX"))
    {
        events.push(HeadlessEvent {
            kind: "action",
            data: serde_json::json!({"detail": app.status_msg}),
        });
    }
    events
}

/// Run headless with the same session and brain behavior as the TUI.
pub(crate) fn run_headless(
    tick_rate: Duration,
    cfg: &crate::config::Config,
    json_mode: bool,
) -> io::Result<()> {
    use crate::session::SessionStatus;
    use std::collections::HashMap;

    let mut app = App::new();

    // Configure the full stack (same as TUI setup in main.rs)
    app.hooks = crate::config::load_hooks();
    app.rules = cfg.rules.clone();
    app.health_thresholds = cfg.health.clone();
    app.file_conflicts_enabled = cfg.file_conflicts;
    app.auto_deny_file_conflicts = cfg.auto_deny_file_conflicts;
    app.brain_config = cfg.brain.clone();
    app.budget_usd = cfg.budget;
    app.kill_on_budget = cfg.kill_on_budget;
    app.notify = cfg.notify;
    app.context_warn_threshold = cfg.context_warn_threshold;
    app.daily_limit = cfg.daily_limit;
    app.weekly_limit = cfg.weekly_limit;

    // Initialize brain engine
    if let Some(ref brain_cfg) = cfg.brain {
        if brain_cfg.enabled {
            if check_brain_endpoint(&brain_cfg.endpoint, brain_cfg.timeout_ms) {
                let engine = headless_brain_engine(brain_cfg.clone());
                app.brain_driver = Some(Box::new(crate::runtime::LiveBrainDriver::new(engine)));
                emit_headless_event(
                    "startup",
                    serde_json::json!({
                        "brain": true,
                        "endpoint": brain_cfg.endpoint,
                        "model": brain_cfg.model,
                        "auto_mode": brain_cfg.auto_mode,
                    }),
                    json_mode,
                );
            } else {
                eprintln!(
                    "Warning: brain endpoint {} not reachable -- running without brain",
                    brain_cfg.endpoint
                );
                emit_headless_event(
                    "startup",
                    serde_json::json!({"brain": false, "reason": "endpoint not reachable"}),
                    json_mode,
                );
            }
        }
    } else {
        emit_headless_event(
            "startup",
            serde_json::json!({"brain": false, "reason": "not configured"}),
            json_mode,
        );
    }

    emit_headless_event(
        "startup",
        serde_json::json!({
            "rules": app.rules.len(),
            "sessions": app.sessions.len(),
            "interval_ms": tick_rate.as_millis(),
        }),
        json_mode,
    );

    let mut prev_statuses: HashMap<u32, SessionStatus> =
        app.sessions.iter().map(|s| (s.pid, s.status)).collect();

    loop {
        std::thread::sleep(tick_rate);
        app.tick();

        for event in headless_tick_events(&app, &prev_statuses) {
            emit_headless_event(event.kind, event.data, json_mode);
        }

        prev_statuses = app.sessions.iter().map(|s| (s.pid, s.status)).collect();
    }
}

fn headless_brain_engine(config: crate::config::BrainConfig) -> crate::brain::engine::BrainEngine {
    let mut engine = crate::brain::engine::BrainEngine::new(config);
    engine.set_terminal_fallback_blocker(|cwd| {
        crate::init::hooks::discover_permission_hooks(cwd).blocks_terminal_fallback()
    });
    engine
}

fn emit_headless_event(event: &str, data: serde_json::Value, json_mode: bool) {
    let ts = crate::logger::timestamp_now();
    if json_mode {
        let obj = serde_json::json!({"ts": ts, "event": event, "data": data});
        println!("{}", serde_json::to_string(&obj).unwrap_or_default());
    } else {
        // Compact human-readable format
        let detail = if let Some(obj) = data.as_object() {
            obj.iter()
                .map(|(k, v)| {
                    let val = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}={val}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            data.to_string()
        };
        println!("[{ts}] {event}: {detail}");
    }
}

pub(crate) fn format_session(fmt: &str, s: &session::CodexSession) -> String {
    let cost = if s.has_usage_metrics() {
        format!("{:.2}", s.cost_usd)
    } else {
        "n/a".to_string()
    };
    let context = if s.has_usage_metrics() {
        format!("{}", s.context_percent() as u32)
    } else {
        "n/a".to_string()
    };
    fmt.replace("{pid}", &s.pid.to_string())
        .replace("{project}", s.display_name())
        .replace("{status}", &s.status.to_string())
        .replace("{cost}", &cost)
        .replace("{context}", &context)
}

/// Path to the brain gate mode state file.
pub(crate) fn brain_gate_mode_path() -> std::path::PathBuf {
    codexctl::brain::gate_mode_path()
}

/// Read the current brain gate mode from disk. Returns "on" if no file exists.
pub(crate) fn read_brain_gate_mode() -> String {
    codexctl::brain::read_gate_mode()
}

/// Set the brain gate mode (on/off/auto) and print confirmation.
pub(crate) fn run_brain_mode(mode: &str) -> io::Result<()> {
    match mode {
        "on" | "off" | "auto" => {}
        "status" | "" => {
            let current = read_brain_gate_mode();
            println!("Brain gate mode: {current}");
            println!();
            println!("Modes:");
            println!("  on   — brain evaluates tool calls, denies dangerous ones (default)");
            println!("  off  — brain disabled, all tool calls pass through");
            println!("  auto — brain auto-approves above confidence threshold");
            return Ok(());
        }
        _ => {
            eprintln!("Unknown brain mode: {mode}");
            eprintln!("Valid modes: on, off, auto, status");
            std::process::exit(1);
        }
    }

    let path = brain_gate_mode_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if mode == "on" {
        // "on" is the default — remove the file so absence = on
        let _ = std::fs::remove_file(&path);
    } else {
        std::fs::write(&path, mode)?;
    }

    let description = match mode {
        "on" => "brain evaluates tool calls, denies dangerous ones",
        "off" => "brain disabled — all tool calls pass through to normal permission flow",
        "auto" => "brain auto-approves tool calls above confidence threshold",
        _ => unreachable!(),
    };

    println!("Brain gate mode set to: {mode}");
    println!("  {description}");
    Ok(())
}

/// Handle --insights: show insights or set mode (on/off/status).
/// Requires brain to be enabled.
pub(crate) fn run_insights(cfg: &config::Config, cli: &Cli, arg: &str) -> io::Result<()> {
    let brain_enabled = cfg.brain.as_ref().map(|b| b.enabled).unwrap_or(false) || cli.brain;

    if !brain_enabled {
        eprintln!(
            "Insights requires the brain. Use --brain or set brain.enabled = true in config."
        );
        std::process::exit(1);
    }

    match arg {
        "on" => {
            let _ = brain::insights::write_insights_mode("on");
            println!("Insights mode: on");
            println!("  Auto-generating insights every 10 decisions during brain distillation.");
            println!("  Run `codexctl --brain --insights` to view.");
        }
        "off" => {
            let _ = brain::insights::write_insights_mode("off");
            println!("Insights mode: off");
            println!(
                "  Auto-generation disabled. Run `codexctl --brain --insights` to generate on demand."
            );
        }
        "status" => {
            let mode = brain::insights::read_insights_mode();
            println!("Insights mode: {mode}");
            println!();
            println!("Modes:");
            println!("  on   — auto-generate insights every 10 decisions");
            println!("  off  — disabled, generate on demand only (default)");
        }
        "" => {
            // No argument: show insights
            brain::insights::print_insights();
        }
        _ => {
            eprintln!("Unknown insights argument: {arg}");
            eprintln!("Usage: --insights [on|off|status]");
            eprintln!("  No argument: show current insights");
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Record a tool-call outcome to the pending-outcomes spool.
/// Reads PendingOutcome JSON from stdin if present and non-empty;
/// otherwise builds one from CLI flags (`--tool`, `--exit-code`, …).
/// Used by the Codex PostToolUse hook for #220 baselining.
pub(crate) fn run_record_outcome(cli: &Cli) -> io::Result<()> {
    use brain::outcomes::{PendingOutcome, truncate_stderr, write_pending};
    use std::io::Read;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Try stdin first — hook scripts pipe a JSON blob in.
    let mut stdin_buf = String::new();
    let stdin_has_data =
        std::io::stdin().read_to_string(&mut stdin_buf).is_ok() && !stdin_buf.trim().is_empty();

    let outcome = if stdin_has_data {
        match serde_json::from_str::<PendingOutcome>(&stdin_buf) {
            Ok(mut p) => {
                if p.ts == 0 {
                    p.ts = ts;
                }
                if let Some(s) = p.stderr_tail.as_ref() {
                    p.stderr_tail = Some(truncate_stderr(s));
                }
                p
            }
            Err(e) => {
                eprintln!("--record-outcome: invalid JSON on stdin: {e}");
                std::process::exit(2);
            }
        }
    } else {
        let tool = cli.tool.clone().unwrap_or_default();
        if tool.is_empty() {
            eprintln!("--record-outcome: --tool is required when stdin is empty");
            std::process::exit(2);
        }
        let project = cli.project.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "unknown".into())
        });
        PendingOutcome {
            tool,
            command: cli.tool_input.clone().filter(|s| !s.is_empty()),
            project,
            session_id: cli.session_id.clone(),
            tool_use_id: cli.tool_use_id.clone(),
            exit_code: cli.exit_code,
            duration_ms: cli.duration_ms,
            stderr_tail: cli.stderr_tail.as_deref().map(truncate_stderr),
            ts,
        }
    };

    match write_pending(&outcome) {
        Ok(path) => {
            if cli.json {
                let v = serde_json::json!({
                    "status": "recorded",
                    "path": path.display().to_string(),
                });
                println!("{}", serde_json::to_string(&v).unwrap());
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("--record-outcome: write failed: {e}");
            std::process::exit(1);
        }
    }
}

/// List resolved outcomes (joined with their decisions). Filterable by
/// --tool and --project.
pub(crate) fn run_brain_outcomes(cli: &Cli) -> io::Result<()> {
    // Reap first so the freshest data shows up.
    let _ = brain::outcomes::reap();
    let resolved = brain::outcomes::load_resolved_map();
    if resolved.is_empty() {
        if cli.json {
            println!("[]");
        } else {
            println!(
                "No attributed outcomes yet. Pending: {}",
                brain::outcomes::list_pending().len()
            );
        }
        return Ok(());
    }

    let tool_filter = cli.tool.as_deref();
    let project_filter = cli.project.as_deref();

    let mut rows: Vec<&brain::outcomes::ResolvedOutcome> = resolved
        .values()
        .filter(|o| tool_filter.is_none_or(|t| o.tool == t))
        .filter(|o| project_filter.is_none_or(|p| o.project.eq_ignore_ascii_case(p)))
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.ts));

    if let Some(n) = cli.top {
        rows.truncate(n);
    }

    if cli.json {
        println!(
            "{}",
            serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
        );
    } else {
        println!(
            "{:<10} {:<8} {:<10} {:<28} COMMAND",
            "EXIT", "DURMS", "TOOL", "PROJECT"
        );
        for r in rows {
            let exit = r
                .exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into());
            let dur = r
                .duration_ms
                .map(|d| d.to_string())
                .unwrap_or_else(|| "?".into());
            let cmd = r.command.as_deref().unwrap_or("");
            let cmd_short = if cmd.len() > 60 { &cmd[..60] } else { cmd };
            println!(
                "{:<10} {:<8} {:<10} {:<28} {}",
                exit,
                dur,
                truncate_col(&r.tool, 10),
                truncate_col(&r.project, 28),
                cmd_short
            );
        }
    }
    Ok(())
}

/// Rank approaches by outcome data: success_rate * sample_count.
pub(crate) fn run_brain_baseline(cli: &Cli) -> io::Result<()> {
    let _ = brain::outcomes::reap();
    let decisions = brain::decisions::read_all_decisions();
    let resolved = brain::outcomes::load_resolved_map();
    let mut rows = brain::outcomes::rank_approaches(&decisions, &resolved, cli.project.as_deref());
    if let Some(tool) = cli.tool.as_deref() {
        rows.retain(|row| row.approach_ref.contains(&format!(":{tool}:")));
    }

    if let Some(n) = cli.top {
        rows.truncate(n);
    }

    if cli.json {
        println!("{}", serde_json::to_string(&rows).unwrap());
    } else if rows.is_empty() {
        println!("No baseline data yet. Need at least 1 attributed outcome.");
    } else {
        println!(
            "{:<8} {:<6} {:<10} {:<10} APPROACH",
            "SUCC%", "N", "MED_COST", "MED_MS"
        );
        for row in rows {
            let cost = row
                .median_cost_usd
                .map(|x| format!("${x:.4}"))
                .unwrap_or_else(|| "-".into());
            let dur = row
                .median_duration_ms
                .map(|x| x.to_string())
                .unwrap_or_else(|| "-".into());
            println!(
                "{:<8.0} {:<6} {:<10} {:<10} {}",
                row.success_rate * 100.0,
                row.sample_count,
                cost,
                dur,
                row.approach_ref
            );
        }
    }
    Ok(())
}

fn truncate_col(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(width.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

/// Reap pending outcomes and attribute them to decisions.
pub(crate) fn run_reap_outcomes(cli: &Cli) -> io::Result<()> {
    let stats = brain::outcomes::reap();
    if cli.json {
        let v = serde_json::json!({
            "scanned": stats.scanned,
            "attributed": stats.attributed,
            "orphaned": stats.orphaned,
            "still_pending": stats.still_pending,
            "errors": stats.errors,
        });
        println!("{}", serde_json::to_string(&v).unwrap());
    } else {
        println!(
            "Outcomes reaped: scanned={} attributed={} orphaned={} still_pending={} errors={}",
            stats.scanned, stats.attributed, stats.orphaned, stats.still_pending, stats.errors
        );
    }
    Ok(())
}

/// Pure parser: turn a Codex hook payload string into a `DiffDigest`.
/// Returns `None` on missing/blank input, invalid JSON, or missing `tool_input`.
fn digest_from_hook_payload(
    tool_name: &str,
    payload: &str,
) -> Option<brain::diff_digest::DiffDigest> {
    let trimmed = payload.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let tool_input = value.get("tool_input")?;
    Some(brain::diff_digest::build_digest(tool_name, tool_input))
}

/// Try to parse a `DiffDigest` from stdin (the raw Codex hook payload).
///
/// Hook scripts can pipe full tool call JSON to `codexctl --brain-query`.
/// We only read when stdin is not a TTY — otherwise `read_to_string` would
/// block waiting for EOF. Failures here are not fatal: missing stdin just
/// means we degrade to pre-#237 behaviour.
fn read_diff_digest_from_stdin(tool_name: &str) -> Option<brain::diff_digest::DiffDigest> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut buf = String::new();
    if std::io::stdin().read_to_string(&mut buf).is_err() {
        return None;
    }
    digest_from_hook_payload(tool_name, &buf)
}

/// Standalone brain query: builds a minimal context from CLI args, calls the
/// local LLM, and prints a JSON decision to stdout. Designed to be called
/// by Codex hooks for inline approve/deny.
pub(crate) fn run_brain_query(cfg: &config::Config, cli: &Cli) -> io::Result<()> {
    let gate_mode = read_brain_gate_mode();
    let brain_cfg = cfg.brain.clone().unwrap_or_default();

    if gate_mode != "off" && !brain_cfg.enabled && !cli.brain {
        eprintln!("Brain is not enabled. Use --brain or set brain.enabled = true in config.");
        std::process::exit(1);
    }

    let tool_name = cli.tool.clone().unwrap_or_else(|| "unknown".into());
    let command = cli.tool_input.clone().unwrap_or_default();
    let project = cli.project.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "unknown".into())
    });

    // #237: hooks may pipe the full Codex hook payload on stdin.
    // We try to parse it into a `DiffDigest` for richer prompt context and
    // structured decision-log attribution. Missing/invalid stdin is fine —
    // the rest of the flow degrades to the pre-#237 behaviour.
    let diff_digest = if gate_mode == "off" {
        None
    } else {
        read_diff_digest_from_stdin(&tool_name)
    };

    let request = brain::query::BrainDecisionRequest {
        project,
        tool_name,
        tool_input: command,
        diff_digest,
    };
    let result = brain_query_json_with(cfg, &request, &gate_mode, brain::client::infer);
    println!("{}", serde_json::to_string(&result).unwrap());
    Ok(())
}

fn brain_query_json_with<F>(
    cfg: &config::Config,
    request: &brain::query::BrainDecisionRequest,
    gate_mode: &str,
    infer: F,
) -> serde_json::Value
where
    F: FnOnce(&config::BrainConfig, &str) -> Result<brain::client::BrainSuggestion, String>,
{
    let brain_cfg = cfg.brain.clone().unwrap_or_default();
    let decision = brain::query::evaluate_with(request, &brain_cfg, gate_mode, infer);
    let mut result = serde_json::json!({
        "action": decision.action,
        "reasoning": decision.reasoning,
        "confidence": decision.confidence,
        "source": decision.source,
    });
    if decision.source == "brain" {
        result["message"] = serde_json::to_value(decision.message).unwrap();
        result["below_threshold"] = serde_json::to_value(decision.below_threshold).unwrap();
        result["threshold"] = serde_json::to_value(decision.threshold).unwrap();
        if let Some(digest) = decision.diff_digest {
            result["diff_digest"] = digest;
        }
    }
    result
}

#[cfg(test)]
mod headless_tests {
    use super::*;

    #[test]
    fn headless_tick_emits_brain_state_without_coordination_events() {
        let mut app = App::new();
        app.status_msg = "Brain: approved Bash".into();
        let events = headless_tick_events(&app, &std::collections::HashMap::new());

        assert!(events.iter().any(|event| event.kind == "action"));
        assert!(events.iter().all(|event| {
            !matches!(
                event.kind,
                "supervisor_tick" | "coord_summary" | "loop_outcome"
            )
        }));
    }

    #[test]
    fn headless_engine_resolves_managed_hooks_for_the_session_project() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let clean_project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".codex")).unwrap();
        std::fs::create_dir_all(project.path().join(".git")).unwrap();
        std::fs::create_dir_all(clean_project.path().join(".git")).unwrap();
        std::fs::write(
            project.path().join(".codex/hooks.json"),
            r#"{"hooks":{"PermissionRequest":[{"matcher":"Bash","hooks":[{"type":"command","command":"codexctl --permission-hook"}]}]}}"#,
        )
        .unwrap();
        let original_home = std::env::var_os("HOME");
        // SAFETY: HOME reads in config-sensitive tests are serialized by HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", home.path()) };
        let engine = headless_brain_engine(crate::config::BrainConfig::default());
        let hooked_session = crate::session::CodexSession::from_raw(crate::session::RawSession {
            pid: 1,
            session_id: "session-1".into(),
            cwd: project.path().display().to_string(),
            started_at: 0,
        });
        let clean_session = crate::session::CodexSession::from_raw(crate::session::RawSession {
            pid: 2,
            session_id: "session-2".into(),
            cwd: clean_project.path().display().to_string(),
            started_at: 0,
        });

        assert!(engine.terminal_fallback_blocked_for(&hooked_session));
        assert!(!engine.terminal_fallback_blocked_for(&clean_session));
        // SAFETY: restore HOME before releasing HOME_ENV_LOCK.
        unsafe {
            match original_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[cfg(test)]
mod digest_parser_tests {
    use super::*;

    #[test]
    fn parses_edit_payload() {
        let payload = r#"{
            "tool_name": "Edit",
            "tool_input": {
                "file_path": ".env",
                "old_string": "",
                "new_string": "DB_PASSWORD=hunter2"
            }
        }"#;
        let d = digest_from_hook_payload("Edit", payload).expect("digest");
        assert_eq!(d.files, vec![".env".to_string()]);
        assert!(d.risky_paths.iter().any(|t| t == ".env"));
        assert!(
            d.risky_tokens
                .iter()
                .any(|t| t.eq_ignore_ascii_case("password="))
        );
    }

    #[test]
    fn returns_none_when_payload_missing_tool_input() {
        assert!(digest_from_hook_payload("Edit", "{}").is_none());
    }

    #[test]
    fn returns_none_for_blank_input() {
        assert!(digest_from_hook_payload("Edit", "   ").is_none());
        assert!(digest_from_hook_payload("Edit", "").is_none());
    }

    #[test]
    fn returns_none_for_invalid_json() {
        assert!(digest_from_hook_payload("Edit", "{not json").is_none());
    }
}

#[cfg(test)]
mod brain_query_adapter_tests {
    use super::*;
    use crate::brain::client::BrainSuggestion;
    use crate::rules::{AutoRule, RuleAction};

    #[test]
    fn matching_auto_rules_do_not_override_injected_brain_decision() {
        let mut cfg = config::Config::default();
        let mut deny = AutoRule::new("deny cargo".into(), RuleAction::Deny);
        deny.match_tool.push("Bash".into());
        deny.match_command.push("cargo test".into());
        let mut approve = AutoRule::new("approve cargo".into(), RuleAction::Approve);
        approve.match_tool.push("Bash".into());
        approve.match_command.push("cargo test".into());
        cfg.rules = vec![deny, approve];
        cfg.brain = Some(config::BrainConfig::default());

        let request = brain::query::BrainDecisionRequest {
            project: "codexctl".into(),
            tool_name: "Bash".into(),
            tool_input: "cargo test".into(),
            diff_digest: None,
        };
        let result = brain_query_json_with(&cfg, &request, "on", |_, _| {
            Ok(BrainSuggestion {
                action: RuleAction::Approve,
                message: Some("brain chose this".into()),
                reasoning: "injected brain decision".into(),
                confidence: 0.9,
                suggested_at: 0,
            })
        });

        assert_eq!(result["action"], "approve");
        assert_eq!(result["source"], "brain");
        assert_eq!(result["reasoning"], "injected brain decision");
        assert_ne!(result["source"], "rule");
    }
}
