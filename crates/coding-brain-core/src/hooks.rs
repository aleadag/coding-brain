use std::collections::HashMap;
use std::process::Command;

use crate::session::AgentSession;

/// Event types that can trigger hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    SessionStart,
    StatusChange,
    NeedsInput,
    Finished,
    Idle,
    ContextHigh,
    ConflictDetected,
}

impl HookEvent {
    pub fn from_section(s: &str) -> Option<Self> {
        match s {
            "hooks.on_session_start" => Some(Self::SessionStart),
            "hooks.on_status_change" => Some(Self::StatusChange),
            "hooks.on_needs_input" => Some(Self::NeedsInput),
            "hooks.on_finished" => Some(Self::Finished),
            "hooks.on_idle" => Some(Self::Idle),
            "hooks.on_context_high" => Some(Self::ContextHigh),
            "hooks.on_conflict_detected" => Some(Self::ConflictDetected),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SessionStart => "on_session_start",
            Self::StatusChange => "on_status_change",
            Self::NeedsInput => "on_needs_input",
            Self::Finished => "on_finished",
            Self::Idle => "on_idle",
            Self::ContextHigh => "on_context_high",
            Self::ConflictDetected => "on_conflict_detected",
        }
    }
}

/// Registry of all configured hooks.
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: HashMap<HookEvent, Vec<String>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, event: HookEvent, command: String) {
        self.hooks.entry(event).or_default().push(command);
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Fire all hooks for an event with session context.
    pub fn fire(&self, event: HookEvent, session: &AgentSession) {
        let Some(commands) = self.hooks.get(&event) else {
            return;
        };

        for template in commands {
            let cmd = expand_template(template, session);

            crate::logger::log("DEBUG", &format!("hook {}: {}", event.name(), cmd));

            // Spawn async — don't block the TUI
            let _ = Command::new("sh")
                .args(["-c", &cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    /// Fire hooks with just a status string (for events without a specific session, like StatusChange).
    pub fn fire_with_status(
        &self,
        event: HookEvent,
        session: &AgentSession,
        old_status: &str,
        new_status: &str,
    ) {
        let Some(commands) = self.hooks.get(&event) else {
            return;
        };

        for template in commands {
            let cmd = expand_template(template, session)
                .replace("{old_status}", old_status)
                .replace("{new_status}", new_status);

            crate::logger::log("DEBUG", &format!("hook {}: {}", event.name(), cmd));

            let _ = Command::new("sh")
                .args(["-c", &cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }

    /// List all configured hooks (for `cbrain --hooks`).
    pub fn print_list(&self) {
        if self.hooks.is_empty() {
            println!("No hooks configured.");
            println!();
            println!("Add hooks in ~/.config/coding-brain/config.toml:");
            println!();
            println!("  [hooks.on_needs_input]");
            println!("  run = \"say 'Codex needs input'\"");
            return;
        }

        println!("Configured hooks:");
        println!();
        for (event, commands) in &self.hooks {
            for cmd in commands {
                let display = if cmd.len() > 60 {
                    format!("{}...", crate::session::truncate_str(cmd, 57))
                } else {
                    cmd.clone()
                };
                println!("  {:<22} {}", event.name(), display);
            }
        }
    }
}

/// Replace template placeholders with session data.
fn expand_template(template: &str, session: &AgentSession) -> String {
    template
        .replace("{pid}", &session.pid.to_string())
        .replace("{project}", session.display_name())
        .replace("{status}", &session.status.to_string())
        .replace("{model}", &session.model)
        .replace("{cwd}", &session.cwd)
        .replace("{elapsed}", &session.format_elapsed())
        .replace("{session_id}", &session.session_id)
        .replace(
            "{context_pct}",
            &session
                .context_pressure
                .map(|value| value.to_string())
                .unwrap_or_default(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentSession, RawAgentSession, TelemetryStatus};

    fn make_session() -> AgentSession {
        let raw = RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 12345,
            process_start_identity: None,
            session_id: "abc-def-123".into(),
            cwd: "/Users/test/projects/my-app".into(),
            started_at: 0,
        };
        let mut s = AgentSession::from_raw(raw);
        s.model = "gpt-5.5".into();
        s.telemetry_status = TelemetryStatus::Available;
        s
    }

    #[test]
    fn legacy_removed_placeholders_remain_literal() {
        let s = make_session();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/legacy-hook-template.json"
        ))
        .unwrap();
        let template = fixture["template"].as_str().unwrap();
        let expected = fixture["expected"].as_str().unwrap();

        assert_eq!(expand_template(template, &s), expected);
    }

    #[test]
    fn retained_template_variables_expand() {
        let s = make_session();
        let result = expand_template(
            "{pid}|{project}|{status}|{model}|{cwd}|{elapsed}|{session_id}",
            &s,
        );
        assert!(result.contains("12345"));
        assert!(result.contains("my-app"));
        assert!(result.contains("gpt-5.5"));
        assert!(result.contains("/Users/test/projects/my-app"));
        assert!(result.contains("abc-def-123"));
    }

    #[test]
    fn test_hook_event_from_section() {
        assert_eq!(
            HookEvent::from_section("hooks.on_needs_input"),
            Some(HookEvent::NeedsInput)
        );
        assert_eq!(
            HookEvent::from_section("hooks.on_finished"),
            Some(HookEvent::Finished)
        );
        assert_eq!(
            HookEvent::from_section("hooks.on_context_high"),
            Some(HookEvent::ContextHigh)
        );
        assert_eq!(HookEvent::from_section("hooks.unknown"), None);
        assert_eq!(HookEvent::from_section("defaults"), None);
    }

    #[test]
    fn test_registry_add_and_fire() {
        let mut reg = HookRegistry::new();
        assert!(reg.is_empty());
        reg.add(HookEvent::NeedsInput, "echo test".into());
        assert!(!reg.is_empty());
    }

    #[test]
    fn test_expand_context_pct() {
        let mut s = make_session();
        s.context_pressure = Some(75);
        let result = expand_template("context at {context_pct}%", &s);
        assert_eq!(result, "context at 75%");
    }

    #[test]
    fn unavailable_context_pressure_expands_to_empty() {
        let mut s = make_session();
        s.context_pressure = None;
        let result = expand_template("context at {context_pct}%", &s);
        assert_eq!(result, "context at %");
    }
}
