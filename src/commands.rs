//! Non-interactive Coding Brain command handlers.

use std::io;
use std::time::Duration;

use crate::Cli;
use crate::brain;
use crate::config;
use crate::discovery;

pub(crate) fn validate_config() -> io::Result<()> {
    use std::path::PathBuf;

    let mut total_warnings = 0;
    let mut any_errors = false;

    let files: Vec<PathBuf> = [
        config::Config::global_path(),
        Some(PathBuf::from(".coding-brain.toml")),
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
    let path = std::path::PathBuf::from(".coding-brain.toml");
    if path.exists() {
        eprintln!("Error: .coding-brain.toml already exists. Remove it first or edit directly.");
        return Err(io::Error::other(".coding-brain.toml already exists"));
    }

    let template = config::Config::template_string();
    std::fs::write(&path, template).map_err(|e| io::Error::other(format!("write: {e}")))?;
    println!("Created .coding-brain.toml with annotated defaults.");
    println!("Edit the file to customize, then run `coding-brain config validate` to check.");
    Ok(())
}

pub(crate) fn run_config_get(cfg: &config::Config, key: &str) -> io::Result<()> {
    let report = config_report_at(&brain::gate_mode_path(), cfg.brain.as_ref(), key)?;
    print!("{report}");
    Ok(())
}

pub(crate) fn run_config_set(key: &str, value: &str) -> io::Result<()> {
    set_config_at(&brain::gate_mode_path(), key, value)?;
    println!("mode: {value}");
    Ok(())
}

fn config_report_at(
    path: &std::path::Path,
    brain_config: Option<&config::BrainConfig>,
    key: &str,
) -> io::Result<String> {
    if key != "mode" {
        return Err(unsupported_config_key(key));
    }
    Ok(mode_report_at(path, brain_config))
}

fn mode_report_at(path: &std::path::Path, brain_config: Option<&config::BrainConfig>) -> String {
    let resolution = brain::resolve_gate_mode_at(path, brain_config);
    let mut report = format!("mode: {}\n", resolution.mode);
    if let Some(warning) = resolution.warning {
        report.push_str(&format!(
            "warning: {warning}\ncorrect with: coding-brain config set mode <off|on|auto>\n"
        ));
    }
    report
}

fn set_config_at(path: &std::path::Path, key: &str, value: &str) -> io::Result<()> {
    if key != "mode" {
        return Err(unsupported_config_key(key));
    }
    set_mode_at(path, value)
}

fn set_mode_at(path: &std::path::Path, value: &str) -> io::Result<()> {
    let mode = match value {
        "off" => coding_brain_core::runtime::BrainGateMode::Off,
        "on" => coding_brain_core::runtime::BrainGateMode::On,
        "auto" => coding_brain_core::runtime::BrainGateMode::Auto,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported mode value {value:?}; expected off, on, or auto"),
            ));
        }
    };
    brain::write_gate_mode_at(path, mode)
}

fn unsupported_config_key(key: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unsupported config key {key:?}; expected mode"),
    )
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

fn activity_envelope(
    event: &coding_brain_core::brain_activity::ActivityEvent,
) -> serde_json::Value {
    serde_json::json!({
        "type": "activity",
        "activity_id": event.activity_id,
        "recorded_at_ms": event.recorded_at_ms,
        "state": event.state,
        "project_id": event.project.project_id,
        "tool": event.tool,
        "fingerprint": event.fingerprint,
        "rule_id": event.rule_id,
        "confidence": event.confidence,
        "threshold": event.threshold,
        "decision_id": event.decision_id,
        "outcome": event.outcome,
        "correction": event.correction,
        "supersedes": event.supersedes,
    })
}

#[derive(Default)]
struct HeadlessActivityCursor {
    emitted: std::collections::HashSet<(String, usize)>,
}

impl HeadlessActivityCursor {
    fn take_unseen(
        &mut self,
        events: &[coding_brain_core::brain_activity::ActivityEvent],
    ) -> Result<Vec<coding_brain_core::brain_activity::ActivityEvent>, serde_json::Error> {
        let mut occurrences = std::collections::HashMap::<String, usize>::new();
        let mut current = std::collections::HashSet::new();
        let mut unseen = Vec::new();
        for event in events {
            let encoded = serde_json::to_string(event)?;
            let occurrence = occurrences.entry(encoded.clone()).or_default();
            *occurrence += 1;
            let key = (encoded, *occurrence);
            if !self.emitted.contains(&key) {
                unseen.push(event.clone());
            }
            current.insert(key);
        }
        self.emitted = current;
        Ok(unseen)
    }
}

/// Continuously emit normalized activity recorded by the hook/evaluation path.
pub(crate) fn run_headless(tick_rate: Duration, json_mode: bool) -> io::Result<()> {
    let paths = crate::brain::distill::current_paths()?;
    let activity =
        crate::brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"));
    let mut cursor = HeadlessActivityCursor::default();

    loop {
        if let Err(error) = crate::brain::distill::run_once(&paths) {
            eprintln!("Warning: Coding Brain preference catch-up failed: {error}");
        }
        let log = match activity.read() {
            Ok(log) => log,
            Err(crate::brain::activity::ActivityStoreError::LockTimeout) => {
                std::thread::sleep(tick_rate);
                continue;
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        for event in cursor.take_unseen(log.events()).map_err(io::Error::other)? {
            emit_activity(&event, json_mode);
        }
        let _ = activity.compact_if_needed();
        std::thread::sleep(tick_rate);
    }
}

fn emit_activity(event: &coding_brain_core::brain_activity::ActivityEvent, json_mode: bool) {
    let envelope = activity_envelope(event);
    if json_mode {
        println!("{}", serde_json::to_string(&envelope).unwrap_or_default());
    } else {
        println!(
            "[{}] activity={} state={:?} project={:?}",
            crate::logger::timestamp_now(),
            event.activity_id,
            event.state,
            event.project.project_id
        );
    }
}

/// Handle --insights: show insights or set mode (on/off/status).
/// Requires Brain mode on or auto.
pub(crate) fn run_insights(cfg: &config::Config, arg: &str) -> io::Result<()> {
    let mode = brain::resolve_gate_mode(cfg.brain.as_ref()).mode;
    let model_active = matches!(
        mode,
        coding_brain_core::runtime::BrainGateMode::On
            | coding_brain_core::runtime::BrainGateMode::Auto
    );

    if !model_active {
        eprintln!("Insights requires Brain mode on or auto.");
        std::process::exit(1);
    }

    match arg {
        "on" => {
            let _ = brain::insights::write_insights_mode("on");
            println!("Insights mode: on");
            println!("  Auto-generating insights every 10 decisions during brain distillation.");
            println!("  Run `coding-brain --insights` to view.");
        }
        "off" => {
            let _ = brain::insights::write_insights_mode("off");
            println!("Insights mode: off");
            println!(
                "  Auto-generation disabled. Run `coding-brain --insights` to generate on demand."
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
/// Hook scripts can pipe full tool call JSON to `coding-brain --brain-query`.
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
    let resolution = brain::resolve_gate_mode(cfg.brain.as_ref());
    if let Some(warning) = resolution.warning {
        eprintln!("Warning: {warning}");
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
    let diff_digest = if resolution.mode == coding_brain_core::runtime::BrainGateMode::Off {
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
    let result = evaluate_brain_query_with(cfg, &request, resolution.mode, brain::client::infer);
    println!("{}", serde_json::to_string(&result).unwrap());
    Ok(())
}

fn evaluate_brain_query_with<F>(
    cfg: &config::Config,
    request: &brain::query::BrainDecisionRequest,
    gate_mode: coding_brain_core::runtime::BrainGateMode,
    infer: F,
) -> serde_json::Value
where
    F: FnOnce(&config::BrainConfig, &str) -> Result<brain::client::BrainSuggestion, String>,
{
    let brain_cfg = cfg.brain.clone().unwrap_or_default();
    let decision = brain::query::evaluate_with(request, &brain_cfg, gate_mode.as_str(), infer);
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
    fn headless_json_is_normalized_activity_not_a_session_roster() {
        let envelope = activity_envelope(&event("activity-1", 1));

        assert_eq!(envelope["type"], "activity");
        assert_eq!(envelope["activity_id"], "activity-1");
        assert_eq!(envelope["state"], "denied");
        assert_eq!(envelope["project_id"]["kind"], "stable");
        assert!(envelope.get("sessions").is_none());
        assert!(envelope.get("session").is_none());
    }

    #[test]
    fn headless_cursor_survives_compaction_rewrites_without_replay_or_skip() {
        let mut cursor = HeadlessActivityCursor::default();
        let first = event("first", 1);
        let retained = event("retained", 2);
        let added = event("added", 3);

        assert_eq!(
            cursor
                .take_unseen(&[first, retained.clone()])
                .unwrap()
                .len(),
            2
        );
        let after_same_length_rewrite = cursor
            .take_unseen(&[retained.clone(), added.clone()])
            .unwrap();
        assert_eq!(after_same_length_rewrite, vec![added.clone()]);
        assert!(cursor.take_unseen(&[added]).unwrap().is_empty());
    }

    fn event(
        activity_id: &str,
        recorded_at_ms: u64,
    ) -> coding_brain_core::brain_activity::ActivityEvent {
        use coding_brain_core::brain_activity::{
            ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
        };
        use coding_brain_core::project::ProjectId;

        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: activity_id.into(),
            recorded_at_ms,
            project: ProjectEvidence {
                project_id: ProjectId::Stable("project-1".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: ActivityState::Denied,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: Some("fixture".into()),
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("fixture".into()),
            decision_id: Some("decision-1".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
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
    use crate::rules::RuleAction;

    #[test]
    fn injected_brain_decision_is_returned() {
        let cfg = config::Config {
            brain: Some(config::BrainConfig::default()),
            ..config::Config::default()
        };

        let request = brain::query::BrainDecisionRequest {
            project: "codexctl".into(),
            tool_name: "Bash".into(),
            tool_input: "cargo test".into(),
            diff_digest: None,
        };
        let result = evaluate_brain_query_with(
            &cfg,
            &request,
            coding_brain_core::runtime::BrainGateMode::On,
            |_, _| {
                Ok(BrainSuggestion {
                    action: RuleAction::Approve,
                    message: Some("brain chose this".into()),
                    reasoning: "injected brain decision".into(),
                    confidence: 0.9,
                    suggested_at: 0,
                })
            },
        );

        assert_eq!(result["action"], "approve");
        assert_eq!(result["source"], "brain");
        assert_eq!(result["reasoning"], "injected brain decision");
        assert_ne!(result["source"], "rule");
    }

    #[test]
    fn explicit_on_mode_ignores_disabled_legacy_config() {
        let cfg = config::Config {
            brain: Some(config::BrainConfig {
                enabled: false,
                ..config::BrainConfig::default()
            }),
            ..config::Config::default()
        };
        let request = brain::query::BrainDecisionRequest {
            project: "codexctl".into(),
            tool_name: "Bash".into(),
            tool_input: "cargo test".into(),
            diff_digest: None,
        };

        let result = evaluate_brain_query_with(
            &cfg,
            &request,
            coding_brain_core::runtime::BrainGateMode::On,
            |_, _| {
                Ok(BrainSuggestion {
                    action: RuleAction::Approve,
                    message: None,
                    reasoning: "explicit mode reached inference".into(),
                    confidence: 0.9,
                    suggested_at: 0,
                })
            },
        );

        assert_eq!(result["source"], "brain");
        assert_eq!(result["reasoning"], "explicit mode reached inference");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrainConfig;

    #[test]
    fn config_set_mode_rejects_unknown_value_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");

        let error = set_mode_at(&path, "automatic").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists());
    }

    #[test]
    fn config_set_rejects_unknown_key_without_writing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");

        let error = set_config_at(&path, "brain.mode", "auto").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!path.exists());
    }

    #[test]
    fn config_get_mode_reports_fail_closed_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::write(&path, "broken").unwrap();

        let report = mode_report_at(&path, Some(&BrainConfig::default()));

        assert!(report.contains("mode: off"));
        assert!(report.contains("config set mode <off|on|auto>"));
    }

    #[test]
    fn config_get_rejects_unknown_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");

        let error = config_report_at(&path, None, "brain.mode").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
