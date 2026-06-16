use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::LoopResult;
use super::config::{
    GateConfig, LoopConfig, LoopMode, SandboxMode, SourceConfig, SourceKind, TriageConfig,
    TriageMode, VerifierConfig, WorktreeMode, discover_project_loops,
};
use super::policy::{
    LoopAction, LoopDecision, deterministic_decision, parse_and_validate_decision,
};
use super::prompt;
use super::sources::{SourceItem, source_from_config};
use super::store::{self, LoopItemState, NewLoopItem};
use super::submit;
use super::tick;
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
    /// Submit a local spec file as a one-shot coord task.
    Handoff {
        /// Markdown/design/spec file to submit.
        #[arg(long)]
        file: PathBuf,
        /// Task title. Defaults to the file stem.
        #[arg(long)]
        name: Option<String>,
        /// Override the loop execution worktree mode.
        #[arg(long, value_parser = parse_worktree_mode_arg)]
        worktree: Option<WorktreeMode>,
        /// Loop config to reuse for cwd, sandbox, model, timeout, verifiers, and worktree root.
        #[arg(long = "loop")]
        loop_name: Option<String>,
        /// Print the planned task/worktree without writing coord state.
        #[arg(long)]
        dry_run: bool,
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
        LoopCommand::Handoff {
            file,
            name,
            worktree,
            loop_name,
            dry_run,
        } => handoff(
            Path::new("."),
            file,
            name.as_deref(),
            *worktree,
            loop_name.as_deref(),
            *dry_run,
        ),
        LoopCommand::Tick { name, json } => {
            tick::run_tick(Path::new("."), name.as_deref(), *json, cfg)
        }
        LoopCommand::Status { name } => status(name.as_deref()),
        LoopCommand::Logs { name, item } => logs(name, item.as_deref()),
        LoopCommand::Pause { name } => set_paused(name, true),
        LoopCommand::Resume { name } => set_paused(name, false),
        LoopCommand::Export { name, format } => export(name, format),
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

fn parse_worktree_mode_arg(value: &str) -> Result<WorktreeMode, String> {
    WorktreeMode::parse(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffPlan {
    pub(crate) task_name: String,
    pub(crate) prompt_source: String,
    pub(crate) cwd: String,
    pub(crate) worktree_path: Option<String>,
    pub(crate) verifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffOutcome {
    pub(crate) task_id: Option<String>,
    pub(crate) plan: HandoffPlan,
}

fn handoff(
    root: &Path,
    file: &Path,
    name: Option<&str>,
    worktree: Option<WorktreeMode>,
    loop_name: Option<&str>,
    dry_run: bool,
) -> LoopResult<()> {
    let cfg = if let Some(loop_name) = loop_name {
        select_one_loop(root, loop_name)?
    } else {
        manual_handoff_config(root)
    };
    let loop_conn = store::open()?;
    let coord_conn = crate::coord::store::open()?;
    let outcome = submit_handoff(
        root,
        &cfg,
        &loop_conn,
        &coord_conn,
        file,
        name,
        worktree,
        dry_run,
    )?;
    if let Some(task_id) = outcome.task_id {
        println!("submitted {task_id}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_handoff(
    root: &Path,
    cfg: &LoopConfig,
    loop_conn: &rusqlite::Connection,
    coord_conn: &rusqlite::Connection,
    file: &Path,
    name: Option<&str>,
    worktree: Option<WorktreeMode>,
    dry_run: bool,
) -> LoopResult<HandoffOutcome> {
    let item = handoff_source_item_from_file(file, name)?;
    let task_name = item.title.clone();
    let plan = plan_handoff(root, cfg, &item, &task_name, worktree, dry_run)?;
    if dry_run {
        print_handoff_dry_run(&plan);
        return Ok(HandoffOutcome {
            task_id: None,
            plan,
        });
    }

    let mode = worktree.unwrap_or(cfg.execution.worktree);
    let worktree_path = match mode {
        WorktreeMode::None | WorktreeMode::Existing => None,
        WorktreeMode::Required | WorktreeMode::Auto => {
            let prepared = worktree::prepare(root, cfg, &item)?;
            Some(prepared.path.to_string_lossy().into_owned())
        }
    };
    let decision = handoff_decision(cfg, &task_name, Some(mode));
    let loop_item_id = store::upsert_item(loop_conn, &NewLoopItem::from_source(&cfg.name, &item))?;
    store::mark_decision(
        loop_conn,
        &loop_item_id,
        LoopItemState::Seen,
        &decision.to_json(),
    )?;
    let task_id = submit::submit_coord_task(
        coord_conn,
        loop_conn,
        cfg,
        &loop_item_id,
        &item,
        &decision,
        worktree_path.as_deref(),
    )?;

    Ok(HandoffOutcome {
        task_id: Some(task_id),
        plan: HandoffPlan {
            cwd: worktree_path.unwrap_or_else(|| cfg.execution.cwd.clone()),
            ..plan
        },
    })
}

pub(crate) fn handoff_source_item_from_file(
    file: &Path,
    name: Option<&str>,
) -> LoopResult<SourceItem> {
    let body = std::fs::read_to_string(file)
        .map_err(|e| format!("read handoff file {}: {e}", file.display()))?;
    let prompt_source = file
        .canonicalize()
        .unwrap_or_else(|_| file.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let title = name
        .and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_else(|| {
            file.file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
                .unwrap_or("manual handoff")
                .to_string()
        });
    let fingerprint = fingerprint(&body);

    Ok(SourceItem {
        source_kind: "manual".into(),
        source_item_id: format!(
            "manual:{}:{fingerprint:016x}",
            sanitize_source_id(&prompt_source)
        ),
        title,
        body,
        url: None,
        raw_json: serde_json::json!({
            "file": prompt_source,
            "fingerprint": format!("{fingerprint:016x}"),
        }),
    })
}

pub(crate) fn plan_handoff(
    root: &Path,
    cfg: &LoopConfig,
    item: &SourceItem,
    task_name: &str,
    worktree: Option<WorktreeMode>,
    _dry_run: bool,
) -> LoopResult<HandoffPlan> {
    let mode = worktree.unwrap_or(cfg.execution.worktree);
    let worktree_path = match mode {
        WorktreeMode::None | WorktreeMode::Existing => None,
        WorktreeMode::Required | WorktreeMode::Auto => Some(
            worktree::plan_for_source_item(root, cfg, item)?
                .path
                .to_string_lossy()
                .into_owned(),
        ),
    };
    let cwd = worktree_path
        .as_ref()
        .cloned()
        .unwrap_or_else(|| cfg.execution.cwd.clone());
    let prompt_source = item
        .raw_json
        .get("file")
        .and_then(|value| value.as_str())
        .unwrap_or(&item.source_item_id)
        .to_string();

    Ok(HandoffPlan {
        task_name: task_name.into(),
        prompt_source,
        cwd,
        worktree_path,
        verifiers: cfg.verify.iter().map(|v| v.command.clone()).collect(),
    })
}

fn handoff_decision(
    cfg: &LoopConfig,
    task_name: &str,
    worktree: Option<WorktreeMode>,
) -> LoopDecision {
    LoopDecision {
        action: LoopAction::Submit,
        risk: "low".into(),
        reason: "manual handoff from local spec".into(),
        task_name: Some(task_name.into()),
        task_prompt: Some("Implement the local spec from the source body.".into()),
        worktree,
        verifiers: cfg.verify.iter().map(|v| v.command.clone()).collect(),
    }
}

fn print_handoff_dry_run(plan: &HandoffPlan) {
    println!("[dry-run] task: {}", plan.task_name);
    println!("[dry-run] prompt source: {}", plan.prompt_source);
    println!("[dry-run] cwd: {}", plan.cwd);
    println!(
        "[dry-run] worktree: {}",
        plan.worktree_path.as_deref().unwrap_or("(none)")
    );
    if plan.verifiers.is_empty() {
        println!("[dry-run] verifiers: (none)");
    } else {
        println!("[dry-run] verifiers:");
        for verifier in &plan.verifiers {
            println!("  - {verifier}");
        }
    }
}

fn manual_handoff_config(root: &Path) -> LoopConfig {
    LoopConfig {
        name: "manual-handoff".into(),
        enabled: true,
        mode: LoopMode::Assisted,
        cadence: None,
        path: root.join("<manual-handoff>"),
        source: SourceConfig {
            kind: SourceKind::Shell,
            repo: None,
            query: None,
            command: Some("manual".into()),
            limit: 1,
        },
        triage: TriageConfig {
            mode: TriageMode::Deterministic,
            skill: None,
            instructions: None,
            allowed_actions: vec!["submit".into()],
            allowed_worktree: vec![
                WorktreeMode::None,
                WorktreeMode::Existing,
                WorktreeMode::Required,
                WorktreeMode::Auto,
            ],
            allowed_verifiers: Vec::new(),
        },
        execution: super::config::ExecutionConfig {
            cwd: root
                .canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .into_owned(),
            worktree: WorktreeMode::Existing,
            sandbox: SandboxMode::WorkspaceWrite,
            worktree_root: None,
            branch_template: None,
            session: "headless".into(),
            model: None,
            budget_usd: None,
            max_retries: None,
            timeout_min: None,
        },
        verify: Vec::<VerifierConfig>::new(),
        gates: GateConfig {
            max_items_per_run: 1,
            max_concurrent: 1,
            require_human_for: Vec::new(),
        },
    }
}

fn sanitize_source_id(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "spec".into()
    } else {
        trimmed.chars().take(96).collect()
    }
}

fn fingerprint(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::FromArgMatches;

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

    #[test]
    fn clap_rejects_removed_daemon_command() {
        let cmd = clap::Command::new("loop").subcommand_required(true);
        let cmd = LoopCommand::augment_subcommands(cmd);

        let err = cmd.try_get_matches_from(["loop", "daemon"]).unwrap_err();

        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn clap_accepts_handoff_options() {
        let cmd = clap::Command::new("loop").subcommand_required(true);
        let cmd = LoopCommand::augment_subcommands(cmd);
        let matches = cmd
            .try_get_matches_from([
                "loop",
                "handoff",
                "--file",
                "docs/design.md",
                "--name",
                "implement design",
                "--worktree",
                "required",
                "--loop",
                "issue-triage",
                "--dry-run",
            ])
            .unwrap();

        let parsed = LoopCommand::from_arg_matches(&matches).unwrap();

        match parsed {
            LoopCommand::Handoff {
                file,
                name,
                worktree,
                loop_name,
                dry_run,
            } => {
                assert_eq!(file, PathBuf::from("docs/design.md"));
                assert_eq!(name.as_deref(), Some("implement design"));
                assert_eq!(worktree, Some(WorktreeMode::Required));
                assert_eq!(loop_name.as_deref(), Some("issue-triage"));
                assert!(dry_run);
            }
            _ => panic!("expected handoff command"),
        }
    }

    #[test]
    fn handoff_source_item_reads_markdown_spec() {
        let temp = tempfile::tempdir().unwrap();
        let spec = temp.path().join("design.md");
        std::fs::write(&spec, "# Design\n\nBuild the thing.\n").unwrap();

        let item = handoff_source_item_from_file(&spec, Some("implement design")).unwrap();

        assert_eq!(item.source_kind, "manual");
        assert_eq!(item.title, "implement design");
        assert_eq!(item.body, "# Design\n\nBuild the thing.\n");
        assert!(item.source_item_id.starts_with("manual:"));
        let canonical_spec = spec.canonicalize().unwrap();
        assert_eq!(
            item.raw_json
                .get("file")
                .and_then(|value| value.as_str())
                .map(PathBuf::from),
            Some(canonical_spec)
        );
    }

    #[test]
    fn handoff_plan_uses_worktree_for_required_mode() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("codexctl");
        std::fs::create_dir_all(&repo).unwrap();
        let mut cfg = LoopConfig::minimal_for_test("manual-handoff");
        cfg.execution.cwd = repo.to_string_lossy().into_owned();
        let item = SourceItem::for_test("manual:docs-design-md");

        let plan = plan_handoff(
            temp.path(),
            &cfg,
            &item,
            "implement design",
            Some(WorktreeMode::Required),
            true,
        )
        .unwrap();

        assert!(plan.worktree_path.is_some());
        assert_eq!(
            plan.cwd,
            plan.worktree_path.as_deref().expect("worktree path")
        );
    }

    #[test]
    fn handoff_submission_inserts_coord_task_and_loop_item() {
        let temp = tempfile::tempdir().unwrap();
        let spec = temp.path().join("design.md");
        std::fs::write(&spec, "# Design\n\nBuild the thing.\n").unwrap();
        let loop_conn = store::open_memory();
        let coord_conn = crate::coord::store::open_memory();
        let mut cfg = LoopConfig::minimal_for_test("manual-handoff");
        cfg.execution.cwd = temp.path().to_string_lossy().into_owned();
        cfg.execution.worktree = WorktreeMode::None;
        cfg.verify.clear();

        let outcome = submit_handoff(
            temp.path(),
            &cfg,
            &loop_conn,
            &coord_conn,
            &spec,
            Some("implement design"),
            Some(WorktreeMode::None),
            false,
        )
        .unwrap();

        let task = crate::coord::tasks::get_task(&coord_conn, &outcome.task_id.unwrap())
            .unwrap()
            .unwrap();
        let rows = store::list_items(&loop_conn, Some("manual-handoff")).unwrap();

        assert_eq!(task.name, "implement design");
        assert!(task.prompt.contains("# Design"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source_kind, "manual");
        assert_eq!(rows[0].state, LoopItemState::Submitted);
        assert_eq!(rows[0].coord_task_id.as_deref(), Some(task.id.as_str()));
    }

    #[test]
    fn handoff_dry_run_does_not_insert_coord_or_loop_rows() {
        let temp = tempfile::tempdir().unwrap();
        let spec = temp.path().join("design.md");
        std::fs::write(&spec, "# Design\n\nBuild the thing.\n").unwrap();
        let loop_conn = store::open_memory();
        let coord_conn = crate::coord::store::open_memory();
        let mut cfg = LoopConfig::minimal_for_test("manual-handoff");
        cfg.execution.cwd = temp.path().to_string_lossy().into_owned();

        let outcome = submit_handoff(
            temp.path(),
            &cfg,
            &loop_conn,
            &coord_conn,
            &spec,
            Some("implement design"),
            Some(WorktreeMode::None),
            true,
        )
        .unwrap();

        assert!(outcome.task_id.is_none());
        assert!(
            store::list_items(&loop_conn, Some("manual-handoff"))
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::coord::tasks::list_tasks(&coord_conn, None)
                .unwrap()
                .is_empty()
        );
    }
}
