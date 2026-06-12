use serde::{Deserialize, Serialize};

use super::LoopResult;
use super::config::{LoopConfig, LoopMode, WorktreeMode};
use super::sources::SourceItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopAction {
    Ignore,
    Report,
    Submit,
    Escalate,
}

impl LoopAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Report => "report",
            Self::Submit => "submit",
            Self::Escalate => "escalate",
        }
    }

    fn parse(value: &str) -> LoopResult<Self> {
        match value {
            "ignore" => Ok(Self::Ignore),
            "report" => Ok(Self::Report),
            "submit" => Ok(Self::Submit),
            "escalate" => Ok(Self::Escalate),
            other => Err(format!("unknown loop action {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopDecision {
    pub action: LoopAction,
    pub risk: String,
    pub reason: String,
    pub task_name: Option<String>,
    pub task_prompt: Option<String>,
    pub worktree: Option<WorktreeMode>,
    pub verifiers: Vec<String>,
}

impl LoopDecision {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "action": self.action.as_str(),
            "risk": self.risk,
            "reason": self.reason,
            "task_name": self.task_name,
            "task_prompt": self.task_prompt,
            "worktree": self.worktree.map(|w| w.as_str()),
            "verifiers": self.verifiers,
        })
    }

    #[cfg(test)]
    pub fn submit_for_test(name: &str) -> Self {
        Self {
            action: LoopAction::Submit,
            risk: "low".into(),
            reason: "test".into(),
            task_name: Some(name.into()),
            task_prompt: Some("Fix it".into()),
            worktree: Some(WorktreeMode::None),
            verifiers: vec!["cargo test".into()],
        }
    }
}

pub fn deterministic_decision(cfg: &LoopConfig, item: &SourceItem) -> LoopResult<LoopDecision> {
    if cfg.mode == LoopMode::Report {
        return Ok(LoopDecision {
            action: LoopAction::Report,
            risk: "low".into(),
            reason: "loop is in report mode".into(),
            task_name: None,
            task_prompt: None,
            worktree: None,
            verifiers: Vec::new(),
        });
    }

    Ok(LoopDecision {
        action: LoopAction::Submit,
        risk: "low".into(),
        reason: "deterministic policy accepted the source item".into(),
        task_name: Some(item.title.clone()),
        task_prompt: Some(format!("Handle source item {}.", item.source_item_id)),
        worktree: Some(cfg.execution.worktree),
        verifiers: cfg.verify.iter().map(|v| v.command.clone()).collect(),
    })
}

pub fn parse_and_validate_decision(text: &str, cfg: &LoopConfig) -> LoopResult<LoopDecision> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid decision JSON: {e}"))?;
    let action_text = value
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "decision missing action".to_string())?;
    let action = LoopAction::parse(action_text)?;
    validate_allowed_action(cfg, action)?;
    if cfg.mode == LoopMode::Report && action == LoopAction::Submit {
        return Err("report mode cannot submit coord tasks".into());
    }

    let worktree = value
        .get("worktree")
        .and_then(|v| v.as_str())
        .map(WorktreeMode::parse)
        .transpose()?;
    if let Some(mode) = worktree {
        if !cfg.triage.allowed_worktree.contains(&mode) {
            return Err(format!("worktree mode {} is not allowed", mode.as_str()));
        }
    }

    let verifiers = value
        .get("verifiers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let allowed = allowed_verifiers(cfg);
    for verifier in &verifiers {
        if !allowed.contains(verifier) {
            return Err(format!("verifier {verifier} is not allowed"));
        }
    }

    let task_name = value
        .get("task_name")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    let task_prompt = value
        .get("task_prompt")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned);
    if action == LoopAction::Submit && task_prompt.as_deref().unwrap_or("").trim().is_empty() {
        return Err("submit decision missing task_prompt".into());
    }

    Ok(LoopDecision {
        action,
        risk: value
            .get("risk")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string(),
        reason: value
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        task_name,
        task_prompt,
        worktree,
        verifiers,
    })
}

fn validate_allowed_action(cfg: &LoopConfig, action: LoopAction) -> LoopResult<()> {
    let label = action.as_str();
    if cfg.triage.allowed_actions.iter().any(|a| a == label) {
        Ok(())
    } else {
        Err(format!("action {label} is not allowed"))
    }
}

fn allowed_verifiers(cfg: &LoopConfig) -> Vec<String> {
    if cfg.triage.allowed_verifiers.is_empty() {
        cfg.verify.iter().map(|v| v.command.clone()).collect()
    } else {
        cfg.triage.allowed_verifiers.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_model_decision_rejects_unallowed_verifier() {
        let cfg = crate::r#loop::config::LoopConfig::minimal_for_test("issue-triage");
        let json = r#"{
          "action": "submit",
          "risk": "low",
          "reason": "clear",
          "task_name": "Fix issue",
          "task_prompt": "Fix it",
          "worktree": "none",
          "verifiers": ["rm -rf /"]
        }"#;

        let err = parse_and_validate_decision(json, &cfg).unwrap_err();

        assert!(err.contains("verifier rm -rf / is not allowed"));
    }

    #[test]
    fn deterministic_report_mode_reports_items() {
        let mut cfg = crate::r#loop::config::LoopConfig::minimal_for_test("daily-email");
        cfg.mode = crate::r#loop::config::LoopMode::Report;
        let item = crate::r#loop::sources::SourceItem::for_test("msg-1");

        let decision = deterministic_decision(&cfg, &item).unwrap();

        assert_eq!(decision.action, LoopAction::Report);
    }
}
