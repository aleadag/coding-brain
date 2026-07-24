use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::brain_app::{BrainApp, BrainTab};

pub mod diagnostics;
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
        BrainTab::Diagnostics => diagnostics::render(frame, areas[1], app),
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
        Span::raw("  "),
        tab("Diagnostics", app.tab() == BrainTab::Diagnostics, theme),
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
                    "j/k select  J/K lists  PgUp/PgDn evidence  Enter switch  x action  c correct  Tab tabs  r refresh  q quit"
                        .into()
                }
                BrainTab::Review => {
                    "j/k select  m mark  n note+mark  s skip  Tab tabs  q quit".into()
                }
                BrainTab::Scorecard => "Tab tabs  r refresh  q quit".into(),
                BrainTab::Diagnostics => {
                    "j/k select  PgUp/PgDn evidence  Tab tabs  r refresh  q quit".into()
                }
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
        ActivityDiagnostics, ActivityItem, ActivityKind, ActivityOutcome, ActivitySnapshot,
        ActivityState, AttentionItem, CorrectionDisposition, DeliveryState, ProjectEvidence,
        SessionTarget,
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
        recent.session.as_mut().unwrap().provider =
            coding_brain_core::provider::AgentProvider::Claude;
        mock.activity_snapshot = ActivitySnapshot {
            attention: vec![AttentionItem {
                activity: unknown,
                occurrences: 3,
                unresolved_occurrences: 3,
            }],
            recent: vec![recent],
            diagnostic_events: Vec::new(),
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
            "Evidence",
            "delivery unknown",
            "execution not confirmed",
            "x3",
            "+1 more unresolved",
            "Codex",
            "Claude",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        for forbidden in ["PID", "send", "terminate", "route", "spawn"] {
            assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
        }
        for forbidden in ["Usage", "Cost", "Quota", "Burn rate", "Token"] {
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
    fn diagnostics_renders_store_health_events_and_neutral_evidence() {
        let app = populated_diagnostics_app(ThemeMode::Dark);
        let text = render_text_at(&app, 140, 38);

        for expected in [
            "[ Diagnostics ]",
            "Store integrity",
            "malformed rows: 2",
            "duplicate terminals: 1",
            "truncated tails: 1",
            "discarded bytes: 17",
            "Recent Diagnostics (2)",
            "Codex  project  Bash",
            "Activity: diagnostic-1",
            "Provider: Codex",
            "Project: project",
            "Tool: Bash",
            "Reason: orphan outcome: Bash command is not losslessly correlatable",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        for forbidden in [
            "Diagnostic  Codex",
            "Status: Diagnostic",
            "Status: error",
            "failed command",
            "secret command",
        ] {
            assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
        }
    }

    #[test]
    fn diagnostics_empty_state_is_explicit() {
        let mut app = fixture_app(MockBrainRuntime::default());
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }

        let text = render_text(&app);

        assert!(text.contains("No recent diagnostic events"), "{text}");
    }

    #[test]
    fn diagnostics_store_health_remains_visible_without_events() {
        let mut mock = MockBrainRuntime::default();
        mock.activity_snapshot.diagnostics = ActivityDiagnostics {
            malformed_rows: 2,
            malformed_offsets: vec![12, 24],
            duplicate_terminal_states: 1,
            truncated_tails: 1,
            discarded_tail_bytes: 17,
        };
        let mut app = fixture_app(mock);
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }

        let text = render_text(&app);

        for expected in [
            "Store integrity",
            "malformed rows: 2",
            "duplicate terminals: 1",
            "truncated tails: 1",
            "discarded bytes: 17",
            "No recent diagnostic events",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
    }

    #[test]
    fn diagnostics_escapes_controls_and_truncates_unicode_by_display_width() {
        let app = populated_diagnostics_app_with_reason(
            ThemeMode::Dark,
            format!("unsafe\u{1b} {}", "界".repeat(80)),
        );
        let text = render_text_at(&app, 30, 38);

        assert!(text.contains("\\u{1b}"), "missing escaped control:\n{text}");
        assert!(!text.contains('\u{1b}'), "raw control:\n{text}");
        assert!(text.contains("Codex  project  Bash"), "{text}");
        for expected in [
            "malformed rows: 2",
            "duplicate terminals: 1",
            "truncated tails: 1",
            "discarded bytes: 17",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
    }

    #[test]
    fn diagnostics_evidence_scrolls_and_resets() {
        for width in [119, 120] {
            let mut app = populated_diagnostics_app_with_reason(
                ThemeMode::Dark,
                (1..=40)
                    .map(|number| format!("diagnostic-{number:02}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
            let initial = render_text_at(&app, width, 24);
            assert!(initial.contains("↓ more"), "{initial}");
            app.handle_key(key(KeyCode::PageDown));
            let scrolled = render_text_at(&app, width, 24);
            assert!(scrolled.contains("↑ more"), "{scrolled}");
            let mut bottom = scrolled;
            for _ in 0..10 {
                if !bottom.contains("↓ more") {
                    break;
                }
                app.handle_key(key(KeyCode::PageDown));
                bottom = render_text_at(&app, width, 24);
            }
            assert!(bottom.contains("diagnostic-40"), "{bottom}");
            assert!(!bottom.contains("↓ more"), "{bottom}");
            app.handle_key(key(KeyCode::Char('j')));
            let reset = render_text_at(&app, width, 24);
            assert!(!reset.contains("↑ more"), "{reset}");
        }

        for mode in [ThemeMode::Dark, ThemeMode::None] {
            let text = render_text_at(&populated_diagnostics_app(mode), 120, 38);
            assert!(text.contains("[ Diagnostics ]"), "{text}");
        }
        for width in [119, 120] {
            let text = render_text_at(&populated_diagnostics_app(ThemeMode::Dark), width, 38);
            let normalized = text
                .replace('│', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            for expected in [
                "[ Diagnostics ]",
                "Recent Diagnostics (2)",
                "Store integrity",
                "Codex  project  Bash",
                "Activity: diagnostic-1",
                "Recorded: 200",
                "Provider: Codex",
                "Session: session-1",
                "Project: project",
                "Tool: Bash",
                "Reason: orphan outcome: Bash command is not losslessly correlatable",
            ] {
                assert!(
                    if expected.starts_with("Reason:") {
                        normalized.contains(expected)
                    } else {
                        text.contains(expected)
                    },
                    "missing {expected} at {width}:\n{text}"
                );
            }
            for forbidden in ["Diagnostic  Codex", "Status: Diagnostic"] {
                assert!(
                    !text.contains(forbidden),
                    "found {forbidden} at {width}:\n{text}"
                );
            }
        }
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
                diagnostic_events: Vec::new(),
                unresolved_count: 1,
                diagnostics: Default::default(),
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };
        let mut app = fixture_app(mock);

        let attention_focused = render_text(&app);
        app.handle_key(key(KeyCode::Char('J')));
        let recent_focused = render_text(&app);

        assert_eq!(
            content_column(&attention_focused, "attention-1", "SEND ?"),
            content_column(&recent_focused, "attention-1", "SEND ?")
        );
        assert_eq!(
            content_column(&attention_focused, "recent-1", "ALLOW"),
            content_column(&recent_focused, "recent-1", "ALLOW")
        );
        assert_eq!(attention_focused.matches("> ").count(), 1);
        assert_eq!(recent_focused.matches("> ").count(), 1);
    }

    #[test]
    fn live_switches_to_side_by_side_evidence_at_120_columns() {
        let app = populated_live_app();
        let narrow = render_text_at(&app, 119, 38);
        let wide = render_text_at(&app, 120, 38);

        let (narrow_attention_row, _) = title_position(&narrow, "Needs Attention");
        let (narrow_recent_row, _) = title_position(&narrow, "Recent");
        let (narrow_evidence_row, _) = title_position(&narrow, "Evidence");
        assert!(narrow_attention_row < narrow_recent_row);
        assert!(narrow_recent_row < narrow_evidence_row);

        let (wide_attention_row, _) = title_position(&wide, "Needs Attention");
        let (wide_recent_row, _) = title_position(&wide, "Recent");
        let (wide_evidence_row, wide_evidence_column) = title_position(&wide, "Evidence");
        assert_eq!(wide_attention_row, wide_evidence_row);
        assert!(wide_recent_row > wide_attention_row);
        assert!(wide_evidence_column >= 75);
    }

    #[test]
    fn live_extreme_narrow_width_keeps_condition_and_project_visible() {
        let text = render_text_at(&populated_live_app(), 30, 38);
        let row = text
            .lines()
            .find(|line| line.contains("SEND ?"))
            .unwrap_or_else(|| panic!("missing condition row:\n{text}"));

        assert!(row.contains("project"), "{row}");
        assert!(!row.contains("Codex"), "{row}");
    }

    #[test]
    fn live_narrow_evidence_height_is_content_bounded() {
        let app = populated_live_app_with_note(Some("wrapped evidence ".repeat(200)));

        let text = render_text_at(&app, 119, 73);
        let (evidence_top, _) = title_position(&text, "Evidence");
        let footer_text = text
            .lines()
            .position(|line| line.contains("j/k select"))
            .expect("Live footer");

        assert!(footer_text - evidence_top - 1 <= 12, "{text}");
        assert!(title_position(&text, "Recent").0 < evidence_top);
    }

    #[test]
    fn live_evidence_is_urgency_first_complete_and_control_safe() {
        let mut item = activity("attention-1", DeliveryState::Unknown);
        item.project.label = Some("coding-brain".into());
        item.normalized_command = Some("cargo test\n--workspace\u{1b}".into());
        item.reasoning = Some("unsafe\u{1b} reason".into());
        item.correction = Some(CorrectionDisposition::BrainRight);
        item.note = Some("operator note".into());
        let app = fixture_app(MockBrainRuntime {
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
        });

        let text = render_text_at(&app, 150, 38);
        let outcome = text.find("OUTCOME").expect("Outcome section");
        let action = text.find("ACTION").expect("Action section");
        let context = text.find("CONTEXT").expect("Context section");

        assert!(outcome < action && action < context, "{text}");
        for expected in [
            "RESOLVED",
            "Confidence",
            "Reason",
            "Resolved",
            "Note",
            "cargo test\\n--workspace\\u{1b}",
            "Project",
            "coding-brain",
            "Provider",
            "Codex",
            "Activity",
            "attention-1",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        assert!(!text.contains('\u{1b}'), "raw escape in Evidence:\n{text}");
    }

    #[test]
    fn live_compact_evidence_keeps_outcome_before_action_and_omits_absent_fields() {
        let mut item = activity("attention-1", DeliveryState::Unknown);
        item.confidence = None;
        item.reasoning = None;
        item.correction = None;
        item.note = None;
        let app = fixture_app(MockBrainRuntime {
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
        });

        let text = render_text_at(&app, 70, 30);
        let status = text.find("Status").expect("Status");
        let outcome = text.find("Outcome").expect("Outcome");
        let action = text.find("Action").expect("Action");
        let context = text.find("Context").expect("Context");

        assert!(
            status < outcome && outcome < action && action < context,
            "{text}"
        );
        for absent in ["Confidence", "Reason", "Resolved", "Note"] {
            assert!(
                !text.contains(absent),
                "found absent field {absent}:\n{text}"
            );
        }
    }

    #[test]
    fn live_evidence_scroll_shows_overflow_indicators_and_moves_content() {
        let mut app = populated_live_app_with_note(Some(
            (1..=40)
                .map(|number| format!("evidence-{number:02}"))
                .collect::<Vec<_>>()
                .join(" "),
        ));

        let initial = render_text_at(&app, 120, 24);
        assert!(initial.contains("↓ more"), "{initial}");

        app.handle_key(key(KeyCode::PageDown));
        let scrolled = render_text_at(&app, 120, 24);
        assert!(scrolled.contains("↑ more"), "{scrolled}");
        assert_ne!(initial, scrolled);

        app.handle_key(key(KeyCode::Char('J')));
        let reset = render_text_at(&app, 120, 24);
        assert!(!reset.contains("↑ more"), "{reset}");
    }

    #[test]
    fn live_list_jumps_keep_highlight_and_evidence_in_sync() {
        let mut app = populated_live_app();

        app.handle_key(key(KeyCode::Char('J')));
        let recent = render_text_at(&app, 120, 38);
        assert_eq!(recent.matches("> ").count(), 1);
        assert!(
            recent
                .lines()
                .any(|line| line.contains("> ") && line.contains("recent-1"))
        );
        assert!(recent.contains("Activity    recent-1"));

        app.handle_key(key(KeyCode::Char('K')));
        let attention = render_text_at(&app, 120, 38);
        assert_eq!(attention.matches("> ").count(), 1);
        assert!(
            attention.lines().any(|line| line.contains("> SEND ?")),
            "{attention}"
        );
        assert!(attention.contains("Activity    attention-1"));
    }

    #[test]
    fn live_footer_documents_list_jumps() {
        let text = render_text(&populated_live_app());

        assert!(text.contains("J/K lists"), "{text}");
        assert!(text.contains("PgUp/PgDn evidence"), "{text}");
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
    fn delivered_deny_is_recent_and_reports_response_emission() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                recent: vec![activity("deny-1", DeliveryState::Delivered)],
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        };

        let text = render_text(&fixture_app(mock));

        assert!(text.contains("denied · response emitted"));
        assert!(!text.contains("blocked"));
        assert!(!text.contains("command did not execute"));
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
                decision: DecisionSummary {
                    provider: coding_brain_core::provider::AgentProvider::Antigravity,
                    ..decision()
                },
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
                providers: vec![coding_brain_core::runtime::ProviderScoreSummary {
                    provider: coding_brain_core::provider::AgentProvider::Antigravity,
                    decisions: 3,
                    correct: 2,
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
        assert!(review.contains("Antigravity"));

        app.handle_key(key(KeyCode::Tab));
        let scorecard = render_text(&app);
        assert!(scorecard.contains("Accuracy"));
        assert!(scorecard.contains("Dangerous false approvals"));
        assert!(scorecard.contains("Counterfactual"));
        assert!(scorecard.contains("Antigravity"));
        assert!(!scorecard.contains("Usage"));
        assert!(!scorecard.contains("Cost"));

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('c')));
        let correction = render_text(&app);
        assert!(correction.contains("brain right"));
        assert!(correction.contains("brain wrong"));
        assert!(correction.contains("exception"));
    }

    #[test]
    fn live_status_distinguishes_outcomes_and_delivery_evidence() {
        for (outcome, label) in [
            (ActivityOutcome::Completed, "completed"),
            (ActivityOutcome::Succeeded, "succeeded"),
            (ActivityOutcome::Failed, "failed"),
            (ActivityOutcome::Cancelled, "cancelled"),
        ] {
            let mut item = activity(label, DeliveryState::Delivered);
            item.state = ActivityState::Allowed;
            item.outcome = Some(outcome);
            item.tool_execution_confirmed = true;
            assert!(live::activity_status(&item).contains(&format!("outcome confirmed: {label}")));
        }

        let mut delivered_allow = activity("delivered-allow", DeliveryState::Delivered);
        delivered_allow.state = ActivityState::Allowed;
        assert_eq!(
            live::activity_status(&delivered_allow),
            "allowed · response emitted"
        );

        let mut delivered_deny = activity("delivered-deny", DeliveryState::Delivered);
        delivered_deny.state = ActivityState::Denied;
        assert_eq!(
            live::activity_status(&delivered_deny),
            "denied · response emitted"
        );

        let mut unknown = activity("unknown", DeliveryState::Unknown);
        unknown.state = ActivityState::Allowed;
        assert!(live::activity_status(&unknown).contains("execution not confirmed"));

        let mut failed = activity("failed", DeliveryState::Failed);
        failed.state = ActivityState::Allowed;
        assert!(live::activity_status(&failed).contains("execution not confirmed"));
    }

    fn fixture_app(mock: MockBrainRuntime) -> BrainApp {
        fixture_app_with_theme(mock, ThemeMode::Dark)
    }

    fn fixture_app_with_theme(mock: MockBrainRuntime, mode: ThemeMode) -> BrainApp {
        let mock = Arc::new(mock);
        BrainApp::new(
            BrainRuntime::new(mock.clone(), mock),
            Theme::from_mode(mode),
        )
    }

    fn populated_diagnostics_app(mode: ThemeMode) -> BrainApp {
        populated_diagnostics_app_with_reason(
            mode,
            "orphan outcome: Bash command is not losslessly correlatable".into(),
        )
    }

    fn populated_diagnostics_app_with_reason(mode: ThemeMode, reason: String) -> BrainApp {
        let mut first = activity("diagnostic-1", DeliveryState::NotApplicable);
        first.kind = ActivityKind::Diagnostic;
        first.state = ActivityState::Error;
        first.recorded_at_ms = 200;
        first.tool = Some("Bash".into());
        first.normalized_command = Some("secret command".into());
        first.reasoning = Some(reason);
        first.decision_id = None;
        first.outcome = None;
        first.correction = None;
        first.note = None;

        let mut second = first.clone();
        second.activity_id = "diagnostic-2".into();
        second.recorded_at_ms = 100;

        let mut app = fixture_app_with_theme(
            MockBrainRuntime {
                activity_snapshot: ActivitySnapshot {
                    diagnostic_events: vec![first, second],
                    diagnostics: ActivityDiagnostics {
                        malformed_rows: 2,
                        malformed_offsets: vec![12, 24],
                        duplicate_terminal_states: 1,
                        truncated_tails: 1,
                        discarded_tail_bytes: 17,
                    },
                    ..ActivitySnapshot::default()
                },
                endpoint_health: online(),
                ..MockBrainRuntime::default()
            },
            mode,
        );
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app
    }

    fn populated_live_app_with_note(note: Option<String>) -> BrainApp {
        let mut attention = activity("attention-1", DeliveryState::Unknown);
        attention.note = note;
        let mut recent = activity("recent-1", DeliveryState::Delivered);
        recent.state = ActivityState::Allowed;
        fixture_app(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: attention,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                recent: vec![recent],
                diagnostic_events: Vec::new(),
                unresolved_count: 1,
                diagnostics: Default::default(),
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        })
    }

    fn populated_live_app() -> BrainApp {
        populated_live_app_with_note(None)
    }

    fn render_text_at(app: &BrainApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
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

    fn render_text(app: &BrainApp) -> String {
        render_text_at(app, 110, 38)
    }

    fn title_position(text: &str, title: &str) -> (usize, usize) {
        text.lines()
            .enumerate()
            .find_map(|(row, line)| line.find(title).map(|column| (row, column)))
            .unwrap_or_else(|| panic!("missing title {title}:\n{text}"))
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
                provider: coding_brain_core::provider::AgentProvider::Codex,
                session_id: "session-1".into(),
                turn_id: None,
                tool_use_id: None,
                project_id,
                cwd: PathBuf::from("/work/project"),
                provider_hints: Vec::new(),
                provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
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
            provider: coding_brain_core::provider::AgentProvider::Codex,
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
