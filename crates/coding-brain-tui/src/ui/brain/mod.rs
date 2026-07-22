use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::brain_app::{BrainApp, BrainTab};

pub mod live;
pub mod review;
pub mod scorecard;

pub fn render(frame: &mut Frame<'_>, app: &BrainApp) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(frame.area());
    render_header(frame, areas[0], app);
    match app.tab() {
        BrainTab::Live => live::render(frame, areas[1], app),
        BrainTab::Review => review::render(frame, areas[1], app),
        BrainTab::Scorecard => scorecard::render(frame, areas[1], app),
    }
    render_footer(frame, areas[2], app);
}

fn render_header(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &BrainApp) {
    let theme = app.theme();
    let health = app.endpoint_health();
    let active = if health.reachable {
        "BRAIN ACTIVE"
    } else {
        "BRAIN OFFLINE"
    };
    let model = health.model.as_deref().unwrap_or("no model");
    let title = Line::from(vec![
        Span::styled(
            "Coding Brain",
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            active,
            Style::default().fg(if health.reachable {
                theme.success
            } else {
                theme.error
            }),
        ),
        Span::raw(format!(
            " | {} | {model}",
            match app.gate_mode() {
                coding_brain_core::runtime::BrainGateMode::On => "advisory",
                coding_brain_core::runtime::BrainGateMode::Auto => "automatic",
                coding_brain_core::runtime::BrainGateMode::Off => "model off",
            }
        )),
    ]);
    let tabs = Line::from(vec![
        tab("Live", app.tab() == BrainTab::Live, theme),
        Span::raw("  "),
        tab("Review", app.tab() == BrainTab::Review, theme),
        Span::raw("  "),
        tab("Scorecard", app.tab() == BrainTab::Scorecard, theme),
    ]);
    let guidance = if health.reachable {
        Line::raw("")
    } else {
        Line::styled(
            health
                .detail
                .as_deref()
                .unwrap_or("Start the local model or run `cb doctor`"),
            Style::default().fg(theme.error),
        )
    };
    frame.render_widget(Paragraph::new(vec![title, tabs, guidance]), area);
}

fn tab<'a>(label: &'a str, active: bool, theme: &coding_brain_core::theme::Theme) -> Span<'a> {
    if active {
        Span::styled(
            format!("[ {label} ]"),
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(label, Style::default().fg(theme.text_muted))
    }
}

fn render_footer(frame: &mut Frame<'_>, area: ratatui::layout::Rect, app: &BrainApp) {
    let theme = app.theme();
    let text = app.input_prompt().unwrap_or_else(|| {
        app.status()
            .map(str::to_owned)
            .unwrap_or_else(|| match app.tab() {
                BrainTab::Live => {
                    "j/k select  Enter switch  c correct  Tab tabs  r refresh  q quit".into()
                }
                BrainTab::Review => {
                    "j/k select  m mark  n note+mark  s skip  Tab tabs  q quit".into()
                }
                BrainTab::Scorecard => "Tab tabs  r refresh  q quit".into(),
            })
    });
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.footer))
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use coding_brain_core::brain_activity::{
        ActivityItem, ActivityKind, ActivityOutcome, ActivitySnapshot, ActivityState,
        AttentionItem, CorrectionDisposition, DeliveryState, ProjectEvidence, SessionTarget,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{
        BrainRuntime, DecisionSummary, EndpointHealth, MockBrainRuntime, ReviewItemSummary,
        RiskTierSummary, ScorecardSummary,
    };
    use coding_brain_core::theme::{Theme, ThemeMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn live_renders_attention_recent_detail_and_overflow_without_dashboard_actions() {
        let mut mock = MockBrainRuntime::default();
        let mut unknown = activity("attention-1", DeliveryState::Unknown);
        unknown.normalized_command = Some("cargo test".into());
        let mut recent = activity("recent-1", DeliveryState::Delivered);
        recent.state = ActivityState::Allowed;
        mock.activity_snapshot = ActivitySnapshot {
            attention: vec![AttentionItem {
                activity: unknown,
                occurrences: 3,
                unresolved_occurrences: 3,
            }],
            recent: vec![recent],
            unresolved_count: 4,
            diagnostics: Default::default(),
        };
        mock.endpoint_health = online();
        let app = fixture_app(mock);

        let text = render_text(&app);

        for expected in [
            "Coding Brain",
            "[ Live ]",
            "Needs Attention",
            "Recent",
            "Decision",
            "delivery unknown",
            "execution not confirmed",
            "x3",
            "+1 more unresolved",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        for forbidden in ["PID", "send", "terminate", "route", "spawn"] {
            assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
        }
    }

    #[test]
    fn header_describes_off_as_model_off() {
        let mock = MockBrainRuntime {
            gate_mode: std::sync::Mutex::new(Some(coding_brain_core::runtime::BrainGateMode::Off)),
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };

        let text = render_text(&fixture_app(mock));

        assert!(
            text.contains("model off"),
            "missing model-off label:\n{text}"
        );
    }

    #[test]
    fn live_list_indentation_stays_fixed_when_selection_moves_between_lists() {
        let mut recent = activity("recent-1", DeliveryState::Delivered);
        recent.state = ActivityState::Allowed;
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity("attention-1", DeliveryState::Unknown),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                recent: vec![recent],
                unresolved_count: 1,
                diagnostics: Default::default(),
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };
        let mut app = fixture_app(mock);

        let attention_focused = render_text(&app);
        app.handle_key(key(KeyCode::Down));
        let recent_focused = render_text(&app);

        assert_eq!(
            content_column(&attention_focused, "attention-1", "denied"),
            content_column(&recent_focused, "attention-1", "denied")
        );
        assert_eq!(
            content_column(&attention_focused, "recent-1", "allowed"),
            content_column(&recent_focused, "recent-1", "allowed")
        );
        assert_eq!(attention_focused.matches("> ").count(), 1);
        assert_eq!(recent_focused.matches("> ").count(), 1);
    }

    #[test]
    fn live_derives_missing_project_label_from_cwd() {
        let mut item = activity("attention-1", DeliveryState::Unknown);
        item.project.label = None;
        item.project.cwd = PathBuf::from("/work/codexctl");
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: item,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };
        let text = render_text(&fixture_app(mock));
        assert!(text.contains("codexctl"));
        assert!(!text.contains("unknown project"));
    }

    #[test]
    fn live_keeps_explicit_label_and_handles_root_cwd() {
        let mut explicit = activity("explicit", DeliveryState::Unknown);
        explicit.project.label = Some("friendly".into());
        explicit.project.cwd = PathBuf::from("/work/ignored");
        assert_eq!(live::project_label(&explicit), "friendly");

        explicit.project.label = None;
        explicit.project.cwd = PathBuf::from("/");
        assert_eq!(live::project_label(&explicit), "/");
    }

    #[test]
    fn live_handles_empty_project_label_and_empty_cwd() {
        let mut item = activity("empty", DeliveryState::Unknown);
        item.project.label = Some(String::new());
        item.project.cwd = PathBuf::from("/work/codexctl");
        assert_eq!(live::project_label(&item), "codexctl");

        item.project.label = None;
        item.project.cwd = PathBuf::new();
        assert_eq!(live::project_label(&item), "unknown project");
    }

    #[cfg(unix)]
    #[test]
    fn live_uses_lossy_utf8_for_project_basename() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let basename = OsString::from_vec(vec![b'c', 0xff, b't']);
        let expected = basename.to_string_lossy();
        let mut item = activity("non-utf8", DeliveryState::Unknown);
        item.project.label = None;
        item.project.cwd = PathBuf::from("/work").join(&basename);

        assert_eq!(live::project_label(&item), expected);
    }

    #[test]
    fn duplicate_collapse_does_not_create_phantom_overflow() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity("attention-1", DeliveryState::Unknown),
                    occurrences: 101,
                    unresolved_occurrences: 101,
                }],
                unresolved_count: 101,
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };

        let text = render_text(&fixture_app(mock));

        assert!(text.contains("x101"));
        assert!(!text.contains("more unresolved"));
    }

    #[test]
    fn delivered_deny_is_recent_and_reports_blocked_execution() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                recent: vec![activity("deny-1", DeliveryState::Delivered)],
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };

        let text = render_text(&fixture_app(mock));

        assert!(text.contains("blocked · command did not execute"));
        assert!(!text.contains("denied · response delivered · execution not confirmed"));
        assert!(text.contains("No unresolved decisions"));
    }

    #[test]
    fn offline_banner_keeps_persisted_live_data_visible() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity("attention-1", DeliveryState::Failed),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            endpoint_health: EndpointHealth {
                reachable: false,
                detail: Some("Start Ollama or run `cb doctor`".into()),
                ..EndpointHealth::default()
            },
            ..MockBrainRuntime::default()
        };
        let app = fixture_app(mock);

        let text = render_text(&app);

        assert!(text.contains("BRAIN OFFLINE"));
        assert!(text.contains("Start Ollama"));
        assert!(text.contains("attention-1"));
        assert!(text.contains("delivery failed"));
    }

    #[test]
    fn live_empty_state_and_resolved_correction_are_explicit() {
        let empty = fixture_app(MockBrainRuntime {
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        });
        let empty_text = render_text(&empty);
        assert!(empty_text.contains("No unresolved decisions"));
        assert!(empty_text.contains("No recent resolved activity"));
        assert!(empty_text.contains("Select an activity to inspect its evidence"));

        let mut corrected = activity("corrected-1", DeliveryState::Delivered);
        corrected.correction = Some(CorrectionDisposition::BrainWrong);
        let resolved = fixture_app(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                recent: vec![corrected],
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        });
        assert!(render_text(&resolved).contains("resolved: brain wrong"));
    }

    #[test]
    fn review_scorecard_and_correction_prompt_render() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity("attention-1", DeliveryState::Unknown),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            review_queue: vec![ReviewItemSummary {
                decision: decision(),
                reason: "Critical-tier false-approve".into(),
                score: 90.0,
            }],
            scorecard: ScorecardSummary {
                total_decisions: 12,
                brain_decisions: 10,
                correct_decisions: 9,
                accuracy_pct: 90.0,
                abstentions: 2,
                dangerous_false_approvals: 1,
                counterfactuals: coding_brain_core::runtime::CounterfactualSummary {
                    brain_was_right: 2,
                    user_was_right: 1,
                },
                risk_tiers: vec![RiskTierSummary {
                    tier: "critical".into(),
                    samples: 2,
                    correct: 1,
                    false_approvals: 1,
                    ..RiskTierSummary::default()
                }],
                ..ScorecardSummary::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };
        let mut app = fixture_app(mock);

        app.handle_key(key(KeyCode::Tab));
        let review = render_text(&app);
        assert!(review.contains("[ Review ]"));
        assert!(review.contains("Critical-tier false-approve"));
        assert!(review.contains("Mark canonical"));

        app.handle_key(key(KeyCode::Tab));
        let scorecard = render_text(&app);
        assert!(scorecard.contains("Accuracy"));
        assert!(scorecard.contains("Dangerous false approvals"));
        assert!(scorecard.contains("Counterfactual"));

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('c')));
        let correction = render_text(&app);
        assert!(correction.contains("brain right"));
        assert!(correction.contains("brain wrong"));
        assert!(correction.contains("exception"));
    }

    #[test]
    fn outcome_is_the_only_execution_confirmation() {
        let mut mock = MockBrainRuntime::default();
        let mut confirmed = activity("confirmed", DeliveryState::Delivered);
        confirmed.outcome = Some(ActivityOutcome::Succeeded);
        confirmed.tool_execution_confirmed = true;
        mock.activity_snapshot.recent = vec![confirmed];
        mock.endpoint_health = online();
        let app = fixture_app(mock);

        let text = render_text(&app);

        assert!(text.contains("outcome confirmed: succeeded"));
    }

    fn fixture_app(mock: MockBrainRuntime) -> BrainApp {
        let mock = Arc::new(mock);
        BrainApp::new(
            BrainRuntime::new(mock.clone(), mock),
            Theme::from_mode(ThemeMode::Dark),
        )
    }

    fn render_text(app: &BrainApp) -> String {
        let backend = TestBackend::new(110, 38);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn content_column(text: &str, row_id: &str, content: &str) -> usize {
        let line = text
            .lines()
            .find(|line| line.contains(row_id))
            .unwrap_or_else(|| panic!("missing row {row_id}:\n{text}"));
        let byte_index = line
            .find(content)
            .unwrap_or_else(|| panic!("missing content {content} in row {row_id}:\n{line}"));
        line[..byte_index].chars().count()
    }

    fn online() -> EndpointHealth {
        EndpointHealth {
            reachable: true,
            model: Some("qwen-local".into()),
            ..EndpointHealth::default()
        }
    }

    fn activity(id: &str, delivery: DeliveryState) -> ActivityItem {
        let project_id = ProjectId::Stable("project-1".into());
        ActivityItem {
            activity_id: id.into(),
            kind: ActivityKind::Decision,
            recorded_at_ms: 1,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: PathBuf::from("/work/project"),
                label: Some("project".into()),
            },
            session: Some(SessionTarget {
                session_id: "session-1".into(),
                turn_id: None,
                tool_use_id: None,
                project_id,
                cwd: PathBuf::from("/work/project"),
                provider_hints: Vec::new(),
            }),
            state: ActivityState::Denied,
            delivery,
            tool: Some("Bash".into()),
            normalized_command: Some(id.into()),
            fingerprint: Some(id.into()),
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("fixture reasoning".into()),
            decision_id: Some("decision-1".into()),
            outcome: None,
            correction: None,
            note: None,
            tool_execution_confirmed: false,
        }
    }

    fn decision() -> DecisionSummary {
        DecisionSummary {
            id: "decision-1".into(),
            timestamp: "1".into(),
            action: "approve".into(),
            confidence: Some(0.95),
            project: Some("project".into()),
            tool: Some("Bash".into()),
            pid: 1,
            command: Some("rm -rf /tmp/build".into()),
            reasoning: Some("fixture".into()),
            user_action: Some("reject".into()),
            override_reason: None,
            brain_decision_ms: Some(30),
            canonical: None,
            cache_hit: Some(false),
            cost_usd: None,
            model: Some("qwen-local".into()),
            outcome_kind: None,
            outcome_detail: None,
            suggested_at: None,
            resolved_at: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}
