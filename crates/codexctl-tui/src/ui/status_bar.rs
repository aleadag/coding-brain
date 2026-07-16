use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::App;

fn brain_mode_label(
    fallback_configured: bool,
    mode: codexctl_core::runtime::BrainGateMode,
) -> &'static str {
    if fallback_configured {
        return "Brain: auto/fallback configured ⚠";
    }
    match mode {
        codexctl_core::runtime::BrainGateMode::Off => "Brain: off",
        codexctl_core::runtime::BrainGateMode::Auto => "Brain: auto",
        codexctl_core::runtime::BrainGateMode::On => "Brain: on",
    }
}

enum TransientStatus<'a> {
    Generic(&'a str),
    Brain(&'a str),
}

fn transient_status_at(app: &App, now: std::time::Instant) -> Option<TransientStatus<'_>> {
    if !app.status_msg.is_empty() {
        Some(TransientStatus::Generic(&app.status_msg))
    } else {
        app.brain_decision_notice_at(now)
            .map(TransientStatus::Brain)
    }
}

pub fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    if app.search_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " / ",
                Style::default()
                    .fg(t.highlight_key)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.search_buffer, Style::default().fg(t.text_primary)),
            Span::styled("_", Style::default().fg(t.text_muted)),
        ]));
        frame.render_widget(msg, area);
    } else if app.launch_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" new[{}]> ", app.launch_form.field.label()),
                Style::default().fg(t.success).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                app.launch_form.active_buffer(),
                Style::default().fg(t.text_primary),
            ),
            Span::styled("_", Style::default().fg(t.text_muted)),
            Span::styled(
                format!("  {}", app.launch_form.summary()),
                Style::default().fg(t.text_muted),
            ),
            Span::styled(
                "  Enter next  Ctrl+Enter launch",
                Style::default().fg(t.text_muted),
            ),
        ]));
        frame.render_widget(msg, area);
    } else if app.input_mode {
        let msg = Paragraph::new(Line::from(vec![
            Span::styled(
                " > ",
                Style::default()
                    .fg(t.input_accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(&*app.input_buffer, Style::default().fg(t.text_primary)),
            Span::styled("_", Style::default().fg(t.text_muted)),
        ]));
        frame.render_widget(msg, area);
    } else if let Some(status) = transient_status_at(app, std::time::Instant::now()) {
        let (text, color) = match status {
            TransientStatus::Generic(text) => {
                let color = if text.starts_with("Error") {
                    t.error
                } else {
                    t.success
                };
                (text, color)
            }
            TransientStatus::Brain(text) => {
                let color = if text.starts_with("Brain denied") {
                    t.error
                } else {
                    t.success
                };
                (text, color)
            }
        };
        let msg = Paragraph::new(Span::styled(format!(" {text}"), Style::default().fg(color)));
        frame.render_widget(msg, area);
    } else if app.has_active_filters() {
        let msg = Paragraph::new(Span::styled(
            format!(" {}", app.filter_summary()),
            Style::default().fg(t.header),
        ));
        frame.render_widget(msg, area);
    } else if !app.session_recordings.is_empty() {
        let count = app.session_recordings.len();
        let names: Vec<&str> = app
            .session_recordings
            .keys()
            .filter_map(|pid| {
                app.sessions
                    .iter()
                    .find(|s| s.pid == *pid)
                    .map(|s| s.display_name())
            })
            .collect();
        let label = names.join(", ");
        let text = if count == 1 {
            format!(" REC {label}  (R to stop)")
        } else {
            format!(" REC {count} sessions: {label}  (R to stop)")
        };
        let msg = Paragraph::new(Span::styled(
            text,
            Style::default().fg(t.error).add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(msg, area);
    } else if let Some(ref driver) = app.brain_driver {
        if driver.pending_count() > 0 {
            let count = driver.pending_count();
            let label = if count == 1 {
                "1 suggestion".into()
            } else {
                format!("{count} suggestions")
            };
            let text = format!(" Brain: {label} pending  (b accept / B reject)");
            let msg = Paragraph::new(Span::styled(
                text,
                Style::default().fg(t.header).add_modifier(Modifier::BOLD),
            ));
            frame.render_widget(msg, area);
        } else {
            use codexctl_core::runtime::BrainGateMode;
            let mode = app.runtime.brain.gate_mode();
            let color = match mode {
                BrainGateMode::Off => t.text_muted,
                BrainGateMode::Auto => t.header,
                BrainGateMode::On => t.success,
            };
            let label = brain_mode_label(app.terminal_auto_fallback_configured(), mode);
            let msg = Paragraph::new(Span::styled(
                format!(" {label}  (Ctrl+b toggle)"),
                Style::default().fg(color),
            ));
            frame.render_widget(msg, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexctl_core::runtime::{BrainGateMode, DecisionSummary, MockRuntime};

    fn app_with_brain_notice(now: std::time::Instant) -> App {
        let mut app = App::new();
        app.runtime = MockRuntime {
            decisions: vec![DecisionSummary {
                id: "new".into(),
                timestamp: "2026-07-16T00:00:00Z".into(),
                action: "approve".into(),
                confidence: Some(0.95),
                project: Some("test".into()),
                tool: Some("Bash".into()),
                pid: 42,
                command: Some("cargo test".into()),
                reasoning: Some("safe".into()),
                user_action: Some("hook_allow".into()),
                override_reason: None,
                brain_decision_ms: None,
                canonical: None,
                cache_hit: None,
                cost_usd: None,
                model: None,
                outcome_kind: None,
                outcome_detail: None,
                suggested_at: None,
                resolved_at: None,
            }],
            ..Default::default()
        }
        .into_runtime();
        app.poll_brain_decision_notice_at(now);
        app
    }

    #[test]
    fn unsafe_fallback_has_an_explicit_warning_label() {
        assert_eq!(
            brain_mode_label(true, BrainGateMode::Auto),
            "Brain: auto/fallback configured ⚠"
        );
    }

    #[test]
    fn brain_notice_reappears_after_generic_status_clears() {
        let now = std::time::Instant::now();
        let mut app = app_with_brain_notice(now);
        app.status_msg = "Approved test".into();

        assert!(matches!(
            transient_status_at(&app, now),
            Some(TransientStatus::Generic("Approved test"))
        ));

        app.status_msg.clear();
        assert!(matches!(
            transient_status_at(&app, now),
            Some(TransientStatus::Brain("Brain allowed Bash — safe"))
        ));
    }
}
