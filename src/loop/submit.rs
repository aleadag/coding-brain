use rusqlite::Connection;

use super::LoopResult;
use super::config::LoopConfig;
use super::policy::LoopDecision;
use super::prompt;
use super::sources::SourceItem;
use super::store;

pub fn submit_coord_task(
    coord_conn: &Connection,
    loop_conn: &Connection,
    cfg: &LoopConfig,
    loop_item_id: &str,
    source_item: &SourceItem,
    decision: &LoopDecision,
    worktree_path: Option<&str>,
) -> LoopResult<String> {
    let cwd = worktree_path.unwrap_or(&cfg.execution.cwd);
    let task_name = decision
        .task_name
        .as_deref()
        .unwrap_or(&source_item.title)
        .to_string();
    let task_prompt = prompt::render_task_prompt(cfg, source_item, decision);
    let verifiers = decision
        .verifiers
        .iter()
        .map(|command| crate::coord::verify::Verifier::Run {
            command: command.clone(),
        })
        .collect();
    let model = cfg
        .execution
        .model
        .as_ref()
        .filter(|model| model.as_str() != "default")
        .cloned();
    let new_task = crate::coord::tasks::NewTask {
        name: task_name,
        role: None,
        cwd: cwd.into(),
        prompt: task_prompt,
        model,
        budget_usd: cfg.execution.budget_usd,
        max_retries: cfg.execution.max_retries,
        timeout_min: cfg.execution.timeout_min,
        depends_on: Vec::new(),
        policy: None,
        verifiers,
    };
    let task_id = crate::coord::tasks::insert_task(coord_conn, &new_task)?;
    store::mark_submitted(loop_conn, loop_item_id, &task_id, worktree_path)?;
    Ok(task_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_decision_creates_coord_task_and_marks_item_submitted() {
        let loop_conn = crate::r#loop::store::open_memory();
        let coord_conn = crate::coord::store::open_memory();
        let cfg = crate::r#loop::config::LoopConfig::minimal_for_test("issue-triage");
        let source_item = crate::r#loop::sources::SourceItem::for_test("github:repo#1");
        let loop_item_id = crate::r#loop::store::upsert_item(
            &loop_conn,
            &crate::r#loop::store::NewLoopItem::from_source("issue-triage", &source_item),
        )
        .unwrap();
        let decision = crate::r#loop::policy::LoopDecision::submit_for_test("Fix it");

        let task_id = submit_coord_task(
            &coord_conn,
            &loop_conn,
            &cfg,
            &loop_item_id,
            &source_item,
            &decision,
            None,
        )
        .unwrap();

        let task = crate::coord::tasks::get_task(&coord_conn, &task_id)
            .unwrap()
            .unwrap();
        let item = crate::r#loop::store::get_item(&loop_conn, &loop_item_id)
            .unwrap()
            .unwrap();
        assert_eq!(task.name, "Fix it");
        assert_eq!(item.coord_task_id.as_deref(), Some(task_id.as_str()));
    }
}
