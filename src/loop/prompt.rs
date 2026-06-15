use super::config::LoopConfig;
use super::policy::LoopDecision;
use super::sources::SourceItem;

pub fn build_model_triage_prompt(cfg: &LoopConfig, item: &SourceItem) -> String {
    format!(
        "You are running loop `{}`.\n\nInstructions:\n{}\n\nSource item:\n- id: {}\n- title: {}\n- url: {}\n\nBody:\n{}\n\nReturn only JSON with action, risk, reason, task_name, task_prompt, worktree, verifiers.",
        cfg.name,
        cfg.triage
            .instructions
            .as_deref()
            .unwrap_or("Decide whether to act on this item."),
        item.source_item_id,
        item.title,
        item.url.as_deref().unwrap_or(""),
        item.body
    )
}

pub fn render_task_prompt(cfg: &LoopConfig, item: &SourceItem, decision: &LoopDecision) -> String {
    let skill = cfg
        .triage
        .skill
        .as_ref()
        .map(|skill| format!("Use skill `{skill}` before acting.\n\n"))
        .unwrap_or_default();
    format!(
        "{skill}Loop: {loop_name}\nSource: {source_kind} {source_id}\nURL: {url}\n\nTask:\n{task_prompt}\n\nWorkflow:\nIf this task changes code, create or update the pull request and include the PR URL in your final answer; codexctl records state from that URL.\n\nTriage reason:\n{reason}\n\nSource body:\n{body}\n",
        loop_name = cfg.name,
        source_kind = item.source_kind,
        source_id = item.source_item_id,
        url = item.url.as_deref().unwrap_or(""),
        task_prompt = decision
            .task_prompt
            .as_deref()
            .unwrap_or("Handle this source item."),
        reason = decision.reason,
        body = item.body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_prompt_requires_reporting_pr_url_for_state_tracking() {
        let cfg = LoopConfig::minimal_for_test("issue-triage");
        let item = SourceItem::for_test("github:aleadag/codexctl#1");
        let decision = crate::r#loop::policy::LoopDecision::submit_for_test("Fix it");

        let prompt = render_task_prompt(&cfg, &item, &decision);

        assert!(prompt.contains("create or update the pull request"));
        assert!(prompt.contains("include the PR URL in your final answer"));
        assert!(prompt.contains("codexctl records state from that URL"));
    }
}
