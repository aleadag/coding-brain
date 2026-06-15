use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::LoopResult;
use super::config::LoopConfig;
use super::sources::SourceItem;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlan {
    pub repo_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub workspace_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

pub trait CommandRunner {
    fn run(&mut self, spec: CommandSpec) -> LoopResult<String>;
}

pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, spec: CommandSpec) -> LoopResult<String> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args);
        if let Some(cwd) = spec.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        let output = cmd
            .output()
            .map_err(|e| format!("run {}: {e}", spec.program))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            return Err(format!("{} failed: {detail}", spec.program));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

pub fn plan_for_source_item(
    project_root: &Path,
    cfg: &LoopConfig,
    item: &SourceItem,
) -> LoopResult<WorktreePlan> {
    plan_for_source_id(project_root, cfg, &item.source_item_id)
}

pub fn plan_for_source_id(
    project_root: &Path,
    cfg: &LoopConfig,
    source_item_id: &str,
) -> LoopResult<WorktreePlan> {
    let repo_root = resolve_repo_root(project_root, &cfg.execution.cwd);
    let slug = sanitize_slug(&format!("{}-{source_item_id}", cfg.name));
    let worktree_root = cfg
        .execution
        .worktree_root
        .as_deref()
        .map(|root| resolve_path(&repo_root, root))
        .unwrap_or_else(|| default_worktree_root(&repo_root));
    let path = worktree_root.join(&slug);
    let branch = cfg
        .execution
        .branch_template
        .as_deref()
        .map(|template| {
            template
                .replace("{loop}", &cfg.name)
                .replace("{source_item_id}", &sanitize_slug(source_item_id))
                .replace("{slug}", &slug)
        })
        .unwrap_or_else(|| format!("loop/{}/{}", cfg.name, sanitize_slug(source_item_id)));

    Ok(WorktreePlan {
        repo_root,
        path,
        branch,
        workspace_name: slug,
    })
}

pub fn prepare(
    project_root: &Path,
    cfg: &LoopConfig,
    item: &SourceItem,
) -> LoopResult<WorktreePlan> {
    let mut runner = SystemRunner;
    prepare_with_runner(project_root, cfg, item, &mut runner)
}

pub fn prepare_with_runner(
    project_root: &Path,
    cfg: &LoopConfig,
    item: &SourceItem,
    runner: &mut impl CommandRunner,
) -> LoopResult<WorktreePlan> {
    let plan = plan_for_source_item(project_root, cfg, item)?;
    if plan.path.exists() {
        if plan.path.is_dir() {
            return Ok(plan);
        }
        return Err(format!(
            "worktree path exists and is not a directory: {}",
            plan.path.display()
        ));
    }
    let parent = plan
        .path
        .parent()
        .ok_or_else(|| format!("worktree path has no parent: {}", plan.path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create worktree root: {e}"))?;

    if has_marker(&plan.repo_root, ".jj") {
        runner.run(CommandSpec {
            program: "jj".into(),
            args: vec![
                "--no-pager".into(),
                "workspace".into(),
                "add".into(),
                "--name".into(),
                plan.workspace_name.clone(),
                "-m".into(),
                format!("loop task {}", item.source_item_id),
                plan.path.to_string_lossy().into_owned(),
            ],
            cwd: Some(plan.repo_root.clone()),
        })?;
    } else if has_marker(&plan.repo_root, ".git") {
        runner.run(CommandSpec {
            program: "git".into(),
            args: vec![
                "worktree".into(),
                "add".into(),
                "-b".into(),
                plan.branch.clone(),
                plan.path.to_string_lossy().into_owned(),
                "HEAD".into(),
            ],
            cwd: Some(plan.repo_root.clone()),
        })?;
    } else {
        return Err(format!(
            "cannot prepare worktree: {} is not a jj or git repository",
            plan.repo_root.display()
        ));
    }

    Ok(plan)
}

pub fn has_marker(path: &Path, marker: &str) -> bool {
    path.ancestors()
        .any(|ancestor| ancestor.join(marker).exists())
}

fn resolve_repo_root(project_root: &Path, cwd: &str) -> PathBuf {
    let path = resolve_path(project_root, cwd);
    path.canonicalize().unwrap_or(path)
}

fn resolve_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn default_worktree_root(repo_root: &Path) -> PathBuf {
    let name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    repo_root
        .parent()
        .unwrap_or(repo_root)
        .join(format!("{name}-worktrees"))
}

fn sanitize_slug(value: &str) -> String {
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
        "item".into()
    } else {
        trimmed.chars().take(96).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeRunner {
        commands: Vec<CommandSpec>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, spec: CommandSpec) -> LoopResult<String> {
            self.commands.push(spec);
            Ok(String::new())
        }
    }

    #[test]
    fn plan_uses_default_sibling_worktree_root_and_sanitized_branch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("codexctl");
        fs::create_dir_all(&repo).unwrap();
        let mut cfg = LoopConfig::minimal_for_test("issue-triage");
        cfg.execution.cwd = repo.to_string_lossy().into_owned();

        let plan = plan_for_source_id(temp.path(), &cfg, "github:aleadag/codexctl#1").unwrap();
        let expected_worktree_root = plan.repo_root.parent().unwrap().join("codexctl-worktrees");

        assert_eq!(
            plan.path,
            expected_worktree_root.join("issue-triage-github-aleadag-codexctl-1")
        );
        assert_eq!(plan.branch, "loop/issue-triage/github-aleadag-codexctl-1");
        assert_eq!(
            plan.workspace_name,
            "issue-triage-github-aleadag-codexctl-1"
        );
    }

    #[test]
    fn prepare_uses_jj_workspace_when_repo_has_jj_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("codexctl");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        let mut cfg = LoopConfig::minimal_for_test("issue-triage");
        cfg.execution.cwd = repo.to_string_lossy().into_owned();
        let item = SourceItem::for_test("github:aleadag/codexctl#1");
        let mut runner = FakeRunner::default();

        let plan = prepare_with_runner(temp.path(), &cfg, &item, &mut runner).unwrap();

        assert_eq!(runner.commands.len(), 1);
        assert_eq!(runner.commands[0].program, "jj");
        assert_eq!(
            runner.commands[0].args,
            vec![
                "--no-pager",
                "workspace",
                "add",
                "--name",
                "issue-triage-github-aleadag-codexctl-1",
                "-m",
                "loop task github:aleadag/codexctl#1",
                plan.path.to_str().unwrap()
            ]
        );
        assert_eq!(
            runner.commands[0].cwd.as_deref(),
            Some(plan.repo_root.as_path())
        );
    }
}
