use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::LoopResult;
use super::config::{LoopConfig, SourceKind, discover_project_loops};
use super::store::{self, LoopItemRow};
use super::worktree::{self, CommandRunner, CommandSpec, SystemRunner};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PublishSummary {
    pub published: usize,
    pub skipped: usize,
    pub failed: usize,
}

pub fn publish_completed(project_root: &Path) -> LoopResult<PublishSummary> {
    let loop_conn = store::open()?;
    let coord_conn = crate::coord::store::open()?;
    let mut runner = SystemRunner;
    publish_completed_with_runner(project_root, &loop_conn, &coord_conn, &mut runner)
}

pub fn publish_completed_with_runner(
    project_root: &Path,
    loop_conn: &Connection,
    coord_conn: &Connection,
    runner: &mut impl CommandRunner,
) -> LoopResult<PublishSummary> {
    let configs = discover_project_loops(project_root)?
        .into_iter()
        .map(|cfg| (cfg.name.clone(), cfg))
        .collect::<HashMap<_, _>>();
    let mut summary = PublishSummary::default();

    for item in store::list_publishable_items(loop_conn)? {
        let Some(task_id) = item.coord_task_id.as_deref() else {
            summary.skipped += 1;
            continue;
        };
        let Some(task) = crate::coord::tasks::get_task(coord_conn, task_id)? else {
            summary.skipped += 1;
            continue;
        };
        if task.state != crate::coord::tasks::TaskState::Done {
            summary.skipped += 1;
            continue;
        }
        let Some(cfg) = configs.get(&item.loop_name) else {
            summary.failed += 1;
            store::mark_failed(loop_conn, &item.id, "loop config not found during publish")?;
            continue;
        };
        if !cfg.gates.allow_pr_create {
            summary.skipped += 1;
            continue;
        }

        match publish_item(project_root, cfg, &item, runner) {
            Ok(Some(url)) => {
                store::mark_done(loop_conn, &item.id, Some(&url))?;
                store::log_event(
                    loop_conn,
                    None,
                    Some(&item.id),
                    "info",
                    "pr_created",
                    "created pull request for completed loop task",
                    serde_json::json!({ "url": url }),
                )?;
                summary.published += 1;
            }
            Ok(None) => {
                let error = "completed loop task had no worktree changes; PR not created";
                store::mark_failed(loop_conn, &item.id, error)?;
                store::log_event(
                    loop_conn,
                    None,
                    Some(&item.id),
                    "error",
                    "no_changes",
                    error,
                    serde_json::json!({}),
                )?;
                summary.failed += 1;
            }
            Err(err) => {
                store::mark_failed(loop_conn, &item.id, &err)?;
                store::log_event(
                    loop_conn,
                    None,
                    Some(&item.id),
                    "error",
                    "publish_failed",
                    &err,
                    serde_json::json!({}),
                )?;
                summary.failed += 1;
            }
        }
    }

    Ok(summary)
}

fn publish_item(
    project_root: &Path,
    cfg: &LoopConfig,
    item: &LoopItemRow,
    runner: &mut impl CommandRunner,
) -> LoopResult<Option<String>> {
    let worktree_path = item
        .worktree_path
        .as_deref()
        .ok_or_else(|| "loop item has no worktree path".to_string())
        .map(PathBuf::from)?;
    if !worktree_path.exists() {
        return Err(format!(
            "worktree path does not exist: {}",
            worktree_path.display()
        ));
    }

    let plan = worktree::plan_for_source_id(project_root, cfg, &item.source_item_id)?;
    let body = pr_body(item);
    if worktree::has_marker(&worktree_path, ".jj") {
        publish_jj(cfg, item, &worktree_path, &plan.branch, &body, runner)
    } else {
        publish_git(cfg, item, &worktree_path, &plan.branch, &body, runner)
    }
}

fn publish_jj(
    cfg: &LoopConfig,
    item: &LoopItemRow,
    worktree_path: &Path,
    branch: &str,
    body: &str,
    runner: &mut impl CommandRunner,
) -> LoopResult<Option<String>> {
    let diff = runner.run(CommandSpec {
        program: "jj".into(),
        args: vec![
            "--no-pager".into(),
            "-R".into(),
            worktree_path.to_string_lossy().into_owned(),
            "diff".into(),
            "--summary".into(),
        ],
        cwd: None,
    })?;
    if diff.trim().is_empty() {
        return Ok(None);
    }
    runner.run(CommandSpec {
        program: "jj".into(),
        args: vec![
            "--no-pager".into(),
            "-R".into(),
            worktree_path.to_string_lossy().into_owned(),
            "bookmark".into(),
            "set".into(),
            branch.into(),
            "-r".into(),
            "@".into(),
        ],
        cwd: None,
    })?;
    runner.run(CommandSpec {
        program: "jj".into(),
        args: vec![
            "--no-pager".into(),
            "-R".into(),
            worktree_path.to_string_lossy().into_owned(),
            "git".into(),
            "push".into(),
            "--bookmark".into(),
            branch.into(),
        ],
        cwd: None,
    })?;
    create_pr(cfg, item, worktree_path, branch, body, runner).map(Some)
}

fn publish_git(
    cfg: &LoopConfig,
    item: &LoopItemRow,
    worktree_path: &Path,
    branch: &str,
    body: &str,
    runner: &mut impl CommandRunner,
) -> LoopResult<Option<String>> {
    let status = runner.run(CommandSpec {
        program: "git".into(),
        args: vec!["status".into(), "--porcelain".into()],
        cwd: Some(worktree_path.to_path_buf()),
    })?;
    if status.trim().is_empty() {
        return Ok(None);
    }
    runner.run(CommandSpec {
        program: "git".into(),
        args: vec!["add".into(), "-A".into()],
        cwd: Some(worktree_path.to_path_buf()),
    })?;
    runner.run(CommandSpec {
        program: "git".into(),
        args: vec!["commit".into(), "-m".into(), pr_title(cfg, item)],
        cwd: Some(worktree_path.to_path_buf()),
    })?;
    runner.run(CommandSpec {
        program: "git".into(),
        args: vec!["push".into(), "-u".into(), "origin".into(), branch.into()],
        cwd: Some(worktree_path.to_path_buf()),
    })?;
    create_pr(cfg, item, worktree_path, branch, body, runner).map(Some)
}

fn create_pr(
    cfg: &LoopConfig,
    item: &LoopItemRow,
    worktree_path: &Path,
    branch: &str,
    body: &str,
    runner: &mut impl CommandRunner,
) -> LoopResult<String> {
    let mut args = vec![
        "pr".into(),
        "create".into(),
        "--base".into(),
        "main".into(),
        "--head".into(),
        branch.into(),
        "--title".into(),
        pr_title(cfg, item),
        "--body".into(),
        body.into(),
    ];
    if cfg.source.kind == SourceKind::GithubIssues {
        if let Some(repo) = cfg.source.repo.as_deref() {
            args.push("--repo".into());
            args.push(repo.into());
        }
    }
    let output = runner.run(CommandSpec {
        program: "gh".into(),
        args,
        cwd: Some(worktree_path.to_path_buf()),
    })?;
    output
        .lines()
        .find(|line| line.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| format!("gh pr create did not return a URL: {output}"))
}

fn pr_title(cfg: &LoopConfig, item: &LoopItemRow) -> String {
    format!("{}: {}", cfg.name, item.title)
}

fn pr_body(item: &LoopItemRow) -> String {
    let mut body = format!(
        "Loop item: `{}`\n\nCoord task: `{}`",
        item.source_item_id,
        item.coord_task_id.as_deref().unwrap_or("(unknown)")
    );
    if let Some(url) = item.url.as_deref() {
        body.push_str(&format!("\n\nSource: {url}"));
    }
    body
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::coord::tasks::{NewTask, TaskState};

    #[derive(Default)]
    struct FakeRunner {
        commands: Vec<CommandSpec>,
        jj_diff_summary: String,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, spec: CommandSpec) -> LoopResult<String> {
            let output = match (spec.program.as_str(), spec.args.as_slice()) {
                ("jj", args) if args.ends_with(&["diff".into(), "--summary".into()]) => {
                    self.jj_diff_summary.clone()
                }
                ("gh", _) => "https://github.com/aleadag/codexctl/pull/42".into(),
                _ => String::new(),
            };
            self.commands.push(spec);
            Ok(output)
        }
    }

    #[test]
    fn publishes_done_jj_task_and_marks_loop_item_done() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("codexctl");
        let worktree = temp.path().join("codexctl-worktrees/task-1");
        fs::create_dir_all(project.join(".codexctl/loops")).unwrap();
        fs::create_dir_all(worktree.join(".jj")).unwrap();
        fs::write(
            project.join(".codexctl/loops/issue-triage.toml"),
            r#"
name = "issue-triage"

[source]
kind = "github_issues"
repo = "aleadag/codexctl"

[execution]
cwd = "."
worktree = "auto"

[gates]
allow_pr_create = true
"#,
        )
        .unwrap();
        let loop_conn = store::open_memory();
        let mut coord_conn = crate::coord::store::open_memory();
        let source_item = crate::r#loop::sources::SourceItem::for_test("github:aleadag/codexctl#1");
        let item_id = store::upsert_item(
            &loop_conn,
            &store::NewLoopItem::from_source("issue-triage", &source_item),
        )
        .unwrap();
        let task_id = crate::coord::tasks::insert_task(
            &coord_conn,
            &NewTask {
                name: "Fix it".into(),
                role: None,
                cwd: worktree.to_string_lossy().into_owned(),
                prompt: "Fix it".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: Vec::new(),
                policy: None,
                verifiers: Vec::new(),
            },
        )
        .unwrap();
        crate::coord::tasks::transition(
            &mut coord_conn,
            &task_id,
            TaskState::Pending,
            TaskState::Done,
            "test",
        )
        .unwrap();
        store::mark_submitted(
            &loop_conn,
            &item_id,
            &task_id,
            Some(worktree.to_str().unwrap()),
        )
        .unwrap();
        let mut runner = FakeRunner {
            jj_diff_summary: "M src/lib.rs".into(),
            ..Default::default()
        };

        let summary =
            publish_completed_with_runner(&project, &loop_conn, &coord_conn, &mut runner).unwrap();
        let row = store::get_item(&loop_conn, &item_id).unwrap().unwrap();

        assert_eq!(summary.published, 1);
        assert_eq!(row.state, store::LoopItemState::Done);
        assert_eq!(
            row.result_url.as_deref(),
            Some("https://github.com/aleadag/codexctl/pull/42")
        );
        assert!(runner.commands.iter().any(|cmd| cmd.program == "gh"));
        assert!(runner.commands.iter().any(|cmd| {
            cmd.program == "jj"
                && cmd.args
                    == vec![
                        "--no-pager",
                        "-R",
                        worktree.to_str().unwrap(),
                        "bookmark",
                        "set",
                        "loop/issue-triage/github-aleadag-codexctl-1",
                        "-r",
                        "@",
                    ]
        }));
    }

    #[test]
    fn marks_publish_failure_when_done_jj_task_has_no_changes() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("codexctl");
        let worktree = temp.path().join("codexctl-worktrees/task-1");
        fs::create_dir_all(project.join(".codexctl/loops")).unwrap();
        fs::create_dir_all(worktree.join(".jj")).unwrap();
        fs::write(
            project.join(".codexctl/loops/issue-triage.toml"),
            r#"
name = "issue-triage"

[source]
kind = "github_issues"
repo = "aleadag/codexctl"

[execution]
cwd = "."
worktree = "auto"

[gates]
allow_pr_create = true
"#,
        )
        .unwrap();
        let loop_conn = store::open_memory();
        let mut coord_conn = crate::coord::store::open_memory();
        let source_item = crate::r#loop::sources::SourceItem::for_test("github:aleadag/codexctl#1");
        let item_id = store::upsert_item(
            &loop_conn,
            &store::NewLoopItem::from_source("issue-triage", &source_item),
        )
        .unwrap();
        let task_id = crate::coord::tasks::insert_task(
            &coord_conn,
            &NewTask {
                name: "Fix it".into(),
                role: None,
                cwd: worktree.to_string_lossy().into_owned(),
                prompt: "Fix it".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: Vec::new(),
                policy: None,
                verifiers: Vec::new(),
            },
        )
        .unwrap();
        crate::coord::tasks::transition(
            &mut coord_conn,
            &task_id,
            TaskState::Pending,
            TaskState::Done,
            "test",
        )
        .unwrap();
        store::mark_submitted(
            &loop_conn,
            &item_id,
            &task_id,
            Some(worktree.to_str().unwrap()),
        )
        .unwrap();
        let mut runner = FakeRunner::default();

        let summary =
            publish_completed_with_runner(&project, &loop_conn, &coord_conn, &mut runner).unwrap();
        let row = store::get_item(&loop_conn, &item_id).unwrap().unwrap();

        assert_eq!(summary.failed, 1);
        assert_eq!(row.state, store::LoopItemState::Failed);
        assert_eq!(row.result_url, None);
        assert!(
            row.last_error
                .as_deref()
                .unwrap()
                .contains("no worktree changes")
        );
        assert!(!runner.commands.iter().any(|cmd| cmd.program == "gh"));
    }
}
