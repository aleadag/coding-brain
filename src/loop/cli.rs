use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::LoopResult;
use super::config::{LoopConfig, TriageMode, WorktreeMode, discover_project_loops};
use super::daemon;
use super::policy::{LoopAction, deterministic_decision, parse_and_validate_decision};
use super::prompt;
use super::sources::{SourceItem, source_from_config};
use super::store::{self, LoopItemState, NewLoopItem};
use super::submit;
use super::worktree;

#[derive(Debug, Subcommand)]
pub enum LoopCommand {
    /// List project-local loop definitions.
    List,
    /// Validate loop definitions and required skills.
    Validate {
        /// Loop name. When omitted, validates every project-local loop.
        name: Option<String>,
    },
    /// Run one loop once.
    Run {
        name: String,
        /// Fetch and decide items without writing coord tasks.
        #[arg(long)]
        dry_run: bool,
        /// Override source/config item limit for this run.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Run due project loops once and reconcile completed loop tasks.
    Tick {
        /// Loop name. When omitted, ticks every project-local loop.
        #[arg(long)]
        name: Option<String>,
        /// Emit JSON status lines.
        #[arg(long)]
        json: bool,
    },
    /// Run enabled project loops in a foreground scheduler.
    Daemon {
        /// Loop name. When omitted, manages every project-local loop.
        #[arg(long)]
        name: Option<String>,
        /// Run due loops once and exit.
        #[arg(long)]
        once: bool,
        /// Emit JSON status lines.
        #[arg(long)]
        json: bool,
    },
    /// Show loop item status.
    Status { name: Option<String> },
    /// Show loop item logs/status details.
    Logs {
        name: String,
        #[arg(long)]
        item: Option<String>,
    },
    /// Pause a loop by writing a local marker.
    Pause { name: String },
    /// Resume a paused loop.
    Resume { name: String },
    /// Export loop state.
    Export {
        name: String,
        #[arg(long, default_value = "md")]
        format: String,
    },
}

pub fn dispatch(cmd: &LoopCommand, cfg: &crate::config::Config) -> io::Result<()> {
    dispatch_inner(cmd, cfg).map_err(io::Error::other)
}

fn dispatch_inner(cmd: &LoopCommand, cfg: &crate::config::Config) -> LoopResult<()> {
    match cmd {
        LoopCommand::List => list_loops(Path::new(".")),
        LoopCommand::Validate { name } => validate_loops(Path::new("."), name.as_deref()),
        LoopCommand::Run {
            name,
            dry_run,
            limit,
        } => run_loop(Path::new("."), name, *dry_run, *limit, cfg),
        LoopCommand::Tick { name, json } => {
            daemon::run_tick(Path::new("."), name.as_deref(), *json, cfg)
        }
        LoopCommand::Daemon { name, once, json } => {
            daemon::run_daemon(Path::new("."), name.as_deref(), *once, *json, cfg)
        }
        LoopCommand::Status { name } => status(name.as_deref()),
        LoopCommand::Logs { name, item } => logs(name, item.as_deref()),
        LoopCommand::Pause { name } => set_paused(name, true),
        LoopCommand::Resume { name } => set_paused(name, false),
        LoopCommand::Export { name, format } => export(name, format),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_accepts_tick_for_due_loop_polling() {
        let cmd = clap::Command::new("loop").subcommand_required(true);
        let cmd = LoopCommand::augment_subcommands(cmd);
        let matches = cmd
            .try_get_matches_from(["loop", "tick", "--name", "issue-triage", "--json"])
            .unwrap();

        let (subcommand, args) = matches.subcommand().unwrap();
        assert_eq!(subcommand, "tick");
        assert_eq!(
            args.get_one::<String>("name").map(String::as_str),
            Some("issue-triage")
        );
        assert!(args.get_flag("json"));
    }
}

fn list_loops(root: &Path) -> LoopResult<()> {
    let loops = discover_project_loops(root)?;
    if loops.is_empty() {
        println!("(no loops found in .codexctl/loops)");
        return Ok(());
    }
    for cfg in loops {
        println!(
            "{:<24} enabled={} mode={:?} source={}",
            cfg.name,
            cfg.enabled,
            cfg.mode,
            cfg.source.kind.as_str()
        );
    }
    Ok(())
}

fn validate_loops(root: &Path, name: Option<&str>) -> LoopResult<()> {
    let loops = select_loops(root, name)?;
    let skills = available_skill_names(root);
    for cfg in loops {
        cfg.validate_with_skills(&skills)?;
        println!("{}: ok", cfg.name);
    }
    Ok(())
}

fn run_loop(
    root: &Path,
    name: &str,
    dry_run: bool,
    limit_override: Option<usize>,
    app_cfg: &crate::config::Config,
) -> LoopResult<()> {
    let cfg = select_one_loop(root, name)?;
    if is_paused(&cfg.name) {
        return Err(format!("loop {} is paused", cfg.name));
    }
    if !cfg.enabled {
        return Err(format!("loop {} is disabled", cfg.name));
    }

    let loop_conn = store::open()?;
    let coord_conn = crate::coord::store::open()?;
    let result = run_loop_config(
        root,
        &cfg,
        &loop_conn,
        &coord_conn,
        dry_run,
        limit_override,
        app_cfg,
    );
    if let Ok(summary) = &result {
        println!(
            "{}: seen={} submitted={} ignored={} dry_run={}",
            cfg.name, summary.seen, summary.submitted, summary.ignored, dry_run
        );
    }
    result.map(|_| ())
}

pub(crate) struct RunSummary {
    pub(crate) seen: usize,
    pub(crate) submitted: usize,
    pub(crate) ignored: usize,
}

pub(crate) fn run_loop_config(
    root: &Path,
    cfg: &LoopConfig,
    loop_conn: &rusqlite::Connection,
    coord_conn: &rusqlite::Connection,
    dry_run: bool,
    limit_override: Option<usize>,
    app_cfg: &crate::config::Config,
) -> LoopResult<RunSummary> {
    cfg.validate_with_skills(&available_skill_names(root))?;
    let run_id = store::begin_run(loop_conn, &cfg.name, &cfg.path)?;
    let result = run_loop_once(
        root,
        cfg,
        loop_conn,
        coord_conn,
        dry_run,
        limit_override,
        app_cfg,
    );
    let finish = match &result {
        Ok(_) => store::finish_run(loop_conn, &run_id, "success", None),
        Err(err) => store::finish_run(loop_conn, &run_id, "failed", Some(err)),
    };
    finish?;
    result
}

fn run_loop_once(
    root: &Path,
    cfg: &LoopConfig,
    loop_conn: &rusqlite::Connection,
    coord_conn: &rusqlite::Connection,
    dry_run: bool,
    limit_override: Option<usize>,
    app_cfg: &crate::config::Config,
) -> LoopResult<RunSummary> {
    let source = source_from_config(cfg)?;
    let limit = limit_override
        .unwrap_or(cfg.gates.max_items_per_run)
        .min(cfg.source.limit);
    let fetched = source.fetch(None, limit)?;
    let mut summary = RunSummary {
        seen: fetched.items.len(),
        submitted: 0,
        ignored: 0,
    };
    for item in fetched.items.into_iter().take(limit) {
        let loop_item_id =
            store::upsert_item(loop_conn, &NewLoopItem::from_source(&cfg.name, &item))?;
        if store::get_item(loop_conn, &loop_item_id)?
            .and_then(|row| row.coord_task_id)
            .is_some()
        {
            summary.ignored += 1;
            continue;
        }
        let decision = decide_item(cfg, &item, app_cfg)?;
        let state = state_for_decision(decision.action);
        store::mark_decision(loop_conn, &loop_item_id, state, &decision.to_json())?;
        if decision.action == LoopAction::Submit {
            if dry_run {
                println!(
                    "[dry-run] would submit {} ({})",
                    item.title, item.source_item_id
                );
                continue;
            }
            let worktree_path = resolve_worktree_path(root, cfg, &item, &decision)?;
            submit::submit_coord_task(
                coord_conn,
                loop_conn,
                cfg,
                &loop_item_id,
                &item,
                &decision,
                worktree_path.as_deref(),
            )?;
            summary.submitted += 1;
        } else {
            summary.ignored += 1;
            println!(
                "{}: {:?} ({})",
                item.source_item_id, decision.action, decision.reason
            );
        }
    }
    Ok(summary)
}

fn decide_item(
    cfg: &LoopConfig,
    item: &SourceItem,
    app_cfg: &crate::config::Config,
) -> LoopResult<super::policy::LoopDecision> {
    match cfg.triage.mode {
        TriageMode::Deterministic => deterministic_decision(cfg, item),
        TriageMode::Model => {
            let brain = app_cfg
                .brain
                .as_ref()
                .ok_or_else(|| "model triage requires [brain] config".to_string())?;
            let prompt = prompt::build_model_triage_prompt(cfg, item);
            let reply = crate::brain::client::complete(brain, &prompt)?;
            parse_and_validate_decision(&reply, cfg)
        }
    }
}

fn state_for_decision(action: LoopAction) -> LoopItemState {
    match action {
        LoopAction::Ignore => LoopItemState::Ignored,
        LoopAction::Report => LoopItemState::Reported,
        LoopAction::Submit => LoopItemState::Seen,
        LoopAction::Escalate => LoopItemState::Escalated,
    }
}

fn resolve_worktree_path(
    root: &Path,
    cfg: &LoopConfig,
    item: &SourceItem,
    decision: &super::policy::LoopDecision,
) -> LoopResult<Option<String>> {
    let mode = decision.worktree.unwrap_or(cfg.execution.worktree);
    match mode {
        WorktreeMode::None | WorktreeMode::Existing => Ok(None),
        WorktreeMode::Required | WorktreeMode::Auto => {
            let plan = worktree::prepare(root, cfg, item)?;
            Ok(Some(plan.path.to_string_lossy().into_owned()))
        }
    }
}

fn status(name: Option<&str>) -> LoopResult<()> {
    let conn = store::open()?;
    let rows = store::list_items(&conn, name)?;
    if rows.is_empty() {
        println!("(no loop items)");
        return Ok(());
    }
    for row in rows {
        println!(
            "{:<18} {:<10} {:<14} {}",
            row.loop_name,
            row.state.as_str(),
            row.source_kind,
            row.title
        );
    }
    Ok(())
}

fn logs(name: &str, item: Option<&str>) -> LoopResult<()> {
    let conn = store::open()?;
    let rows = store::list_items(&conn, Some(name))?;
    for row in rows {
        if item.is_some_and(|wanted| wanted != row.id && wanted != row.source_item_id) {
            continue;
        }
        println!("{}  {}  {}", row.id, row.state.as_str(), row.title);
        if let Some(task_id) = row.coord_task_id {
            println!("  coord_task_id={task_id}");
        }
        if let Some(result_url) = row.result_url {
            println!("  result_url={result_url}");
        }
        if let Some(error) = row.last_error {
            println!("  error={error}");
        }
    }
    Ok(())
}

fn export(name: &str, format: &str) -> LoopResult<()> {
    if format != "md" {
        return Err(format!("unsupported export format {format}"));
    }
    let conn = store::open()?;
    let rows = store::list_items(&conn, Some(name))?;
    println!("# Loop {name}");
    println!();
    for row in rows {
        println!(
            "- `{}` {} - {}",
            row.state.as_str(),
            row.source_item_id,
            row.title
        );
    }
    Ok(())
}

pub(crate) fn select_loops(root: &Path, name: Option<&str>) -> LoopResult<Vec<LoopConfig>> {
    let loops = discover_project_loops(root)?;
    if let Some(name) = name {
        let selected: Vec<_> = loops.into_iter().filter(|cfg| cfg.name == name).collect();
        if selected.is_empty() {
            return Err(format!("loop {name} not found"));
        }
        Ok(selected)
    } else {
        Ok(loops)
    }
}

fn select_one_loop(root: &Path, name: &str) -> LoopResult<LoopConfig> {
    select_loops(root, Some(name))?
        .into_iter()
        .next()
        .ok_or_else(|| format!("loop {name} not found"))
}

fn available_skill_names(root: &Path) -> HashSet<String> {
    codexctl_core::skills::discover(Some(root))
        .into_iter()
        .map(|skill| skill.name)
        .collect()
}

fn paused_path(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".codexctl")
        .join("loop")
        .join("paused")
        .join(name)
}

pub(crate) fn is_paused(name: &str) -> bool {
    paused_path(name).exists()
}

fn set_paused(name: &str, paused: bool) -> LoopResult<()> {
    let path = paused_path(name);
    if paused {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create pause dir: {e}"))?;
        }
        std::fs::write(&path, b"paused\n").map_err(|e| format!("pause loop: {e}"))?;
        println!("paused {name}");
    } else {
        match std::fs::remove_file(&path) {
            Ok(()) => println!("resumed {name}"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => println!("{name} was not paused"),
            Err(e) => return Err(format!("resume loop: {e}")),
        }
    }
    Ok(())
}
