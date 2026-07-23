use crate::session::AgentSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleAction {
    Approve,
    Deny,
}

impl RuleAction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutoRule {
    pub name: String,
    pub match_status: Vec<String>,
    pub match_tool: Vec<String>,
    pub match_command: Vec<String>,
    pub match_project: Vec<String>,
    pub match_cost_above: Option<f64>,
    pub match_last_error: Option<bool>,
    pub match_file_conflict: Option<bool>,
    pub action: RuleAction,
    pub message: Option<String>,
}

impl AutoRule {
    pub fn new(name: String, action: RuleAction) -> Self {
        Self {
            name,
            match_status: Vec::new(),
            match_tool: Vec::new(),
            match_command: Vec::new(),
            match_project: Vec::new(),
            match_cost_above: None,
            match_last_error: None,
            match_file_conflict: None,
            action,
            message: None,
        }
    }
}

/// Result of evaluating rules against a session.
#[derive(Debug, Clone)]
pub struct RuleMatch {
    pub rule_name: String,
    pub action: RuleAction,
    pub message: Option<String>,
}

/// Evaluate all rules against a session. Deny rules take precedence.
/// Among non-deny rules, first match in config order wins.
pub fn evaluate(rules: &[AutoRule], session: &AgentSession) -> Option<RuleMatch> {
    let mut first_non_deny: Option<RuleMatch> = None;

    for rule in rules {
        if !matches_rule(rule, session) {
            continue;
        }

        if rule.action == RuleAction::Deny {
            return Some(RuleMatch {
                rule_name: rule.name.clone(),
                action: RuleAction::Deny,
                message: rule.message.clone(),
            });
        }

        if first_non_deny.is_none() {
            first_non_deny = Some(RuleMatch {
                rule_name: rule.name.clone(),
                action: rule.action.clone(),
                message: rule.message.clone(),
            });
        }
    }

    first_non_deny
}

/// Check if all of a rule's conditions match the session.
/// Omitted conditions (empty vec / None) are treated as wildcards.
fn matches_rule(rule: &AutoRule, session: &AgentSession) -> bool {
    if !rule.match_status.is_empty() {
        let status_str = session.status.to_string().to_lowercase();
        let any_match = rule
            .match_status
            .iter()
            .any(|s| status_str == s.to_lowercase());
        if !any_match {
            return false;
        }
    }

    if !rule.match_tool.is_empty() {
        let tool = match session.actionable_tool_name() {
            Some(tool) => tool.to_lowercase(),
            None => return false,
        };
        let any_match = rule
            .match_tool
            .iter()
            .any(|value| tool == value.to_lowercase());
        if !any_match {
            return false;
        }
    }

    if !rule.match_command.is_empty() {
        let command = match session.actionable_tool_input() {
            Some(command) => command.to_lowercase(),
            None => return false,
        };
        let any_match = rule
            .match_command
            .iter()
            .any(|pattern| command.contains(&pattern.to_lowercase()));
        if !any_match {
            return false;
        }
    }

    if !rule.match_project.is_empty() {
        let project = session.display_name().to_lowercase();
        let any_match = rule
            .match_project
            .iter()
            .any(|p| project.contains(&p.to_lowercase()));
        if !any_match {
            return false;
        }
    }

    if let Some(threshold) = rule.match_cost_above {
        if session.cost_usd <= threshold {
            return false;
        }
    }

    if let Some(expected) = rule.match_last_error {
        if session.last_tool_error != expected {
            return false;
        }
    }

    if let Some(expected) = rule.match_file_conflict {
        if session.has_file_conflict != expected {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        AgentSession, ApprovalEvidence, ApprovalObservation, RawAgentSession, SessionStatus,
        TelemetryStatus,
    };
    use crate::terminals::Terminal;

    fn make_session() -> AgentSession {
        let raw = RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 100,
            process_start_identity: None,
            session_id: "test".into(),
            cwd: "/tmp/my-project".into(),
            started_at: 0,
        };
        let mut s = AgentSession::from_raw(raw);
        s.status = SessionStatus::NeedsInput;
        s.telemetry_status = TelemetryStatus::Available;
        s.pending_tool_name = Some("Bash".into());
        s.pending_tool_input = Some("cargo test".into());
        s.cost_usd = 5.0;
        s
    }

    fn approve_rule(name: &str) -> AutoRule {
        AutoRule::new(name.into(), RuleAction::Approve)
    }

    fn deny_rule(name: &str) -> AutoRule {
        AutoRule::new(name.into(), RuleAction::Deny)
    }

    #[test]
    fn no_rules_returns_none() {
        let s = make_session();
        assert!(evaluate(&[], &s).is_none());
    }

    #[test]
    fn wildcard_rule_matches_any_session() {
        let s = make_session();
        let rules = vec![approve_rule("catch_all")];
        let m = evaluate(&rules, &s).unwrap();
        assert_eq!(m.action, RuleAction::Approve);
    }

    #[test]
    fn match_status_filters() {
        let mut s = make_session();
        s.status = SessionStatus::WaitingInput;

        let mut rule = approve_rule("only_needs_input");
        rule.match_status = vec!["Needs Input".into()];

        assert!(evaluate(&[rule.clone()], &s).is_none());

        s.status = SessionStatus::NeedsInput;
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn match_tool_filters() {
        let s = make_session(); // pending_tool_name = "Bash"

        let mut rule = approve_rule("only_read");
        rule.match_tool = vec!["Read".into()];
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("bash_ok");
        rule2.match_tool = vec!["Bash".into()];
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn match_tool_case_insensitive() {
        let s = make_session();

        let mut rule = approve_rule("bash_lower");
        rule.match_tool = vec!["bash".into()];
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn match_command_substring() {
        let s = make_session(); // pending_tool_input = "cargo test"

        let mut rule = deny_rule("deny_rm");
        rule.match_command = vec!["rm -rf".into()];
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("approve_cargo");
        rule2.match_command = vec!["cargo".into()];
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn confirmed_wrapper_rules_match_displayed_command() {
        let mut session = make_session();
        session.pending_tool_name = Some("exec".into());
        session.pending_tool_call_id = Some("call-1".into());
        session.pending_tool_input = Some("await tools.exec_command(args);".into());
        session.approval = ApprovalObservation::Confirmed(ApprovalEvidence {
            session_id: session.session_id.clone(),
            tty: session.tty.clone(),
            call_id: "call-1".into(),
            tool: "exec_command".into(),
            command: "install -m 664 source target".into(),
            backend: Terminal::Tmux,
            target: "main:1.0".into(),
            prompt_pattern_version: 1,
            prompt_fingerprint: 42,
        });

        let mut displayed = approve_rule("displayed-command");
        displayed.match_tool = vec!["exec_command".into()];
        displayed.match_command = vec!["install -m 664".into()];
        assert!(evaluate(&[displayed], &session).is_some());

        let mut wrapper = approve_rule("wrapper-source");
        wrapper.match_tool = vec!["exec".into()];
        wrapper.match_command = vec!["tools.exec_command".into()];
        assert!(evaluate(&[wrapper], &session).is_none());
    }

    #[test]
    fn match_project_substring() {
        let s = make_session(); // project_name = "my-project"

        let mut rule = approve_rule("my_proj");
        rule.match_project = vec!["my-project".into()];
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("other");
        rule2.match_project = vec!["other-project".into()];
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_cost_above() {
        let s = make_session(); // cost = 5.0

        let mut rule = approve_rule("cheap");
        rule.match_cost_above = Some(10.0);
        assert!(evaluate(&[rule], &s).is_none());

        let mut rule2 = approve_rule("expensive");
        rule2.match_cost_above = Some(3.0);
        assert!(evaluate(&[rule2], &s).is_some());
    }

    #[test]
    fn match_last_error() {
        let mut s = make_session();
        s.last_tool_error = true;

        let mut rule = approve_rule("on_error");
        rule.match_last_error = Some(true);
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("no_error");
        rule2.match_last_error = Some(false);
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_file_conflict() {
        let mut s = make_session();
        s.has_file_conflict = true;

        let mut rule = deny_rule("deny_conflict");
        rule.match_file_conflict = Some(true);
        assert!(evaluate(&[rule], &s).is_some());

        let mut rule2 = approve_rule("no_conflict");
        rule2.match_file_conflict = Some(false);
        assert!(evaluate(&[rule2], &s).is_none());
    }

    #[test]
    fn match_file_conflict_false_matches_clean() {
        let s = make_session(); // has_file_conflict defaults to false

        let mut rule = approve_rule("clean");
        rule.match_file_conflict = Some(false);
        assert!(evaluate(&[rule], &s).is_some());
    }

    #[test]
    fn deny_takes_precedence() {
        let s = make_session();

        let approve = approve_rule("approve_all");
        let deny = deny_rule("deny_all");

        // Approve first in config order, deny second — deny still wins
        let rules = vec![approve, deny];
        let m = evaluate(&rules, &s).unwrap();
        assert_eq!(m.action, RuleAction::Deny);
    }

    #[test]
    fn multiple_conditions_are_and() {
        let s = make_session(); // Bash + "cargo test" + cost 5.0

        let mut rule = approve_rule("bash_cargo_cheap");
        rule.match_tool = vec!["Bash".into()];
        rule.match_command = vec!["cargo".into()];
        rule.match_cost_above = Some(10.0); // cost 5.0 does NOT exceed 10.0
        assert!(evaluate(&[rule], &s).is_none());
    }

    #[test]
    fn no_pending_tool_fails_tool_match() {
        let mut s = make_session();
        s.pending_tool_name = None;

        let mut rule = approve_rule("bash");
        rule.match_tool = vec!["Bash".into()];
        assert!(evaluate(&[rule], &s).is_none());
    }
}
