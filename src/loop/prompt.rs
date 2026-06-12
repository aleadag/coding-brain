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
        "{skill}Loop: {loop_name}\nSource: {source_kind} {source_id}\nURL: {url}\n\nTask:\n{task_prompt}\n\nTriage reason:\n{reason}\n\nSource body:\n{body}\n",
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
