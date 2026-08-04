use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use coding_brain_core::review_state::{ReviewSurface, ReviewTarget, SurfaceReviewProjection};
use coding_brain_core::theme::Theme;

use crate::brain_app::{BrainApp, BrainTab};

pub mod diagnostics;
pub mod live;
pub mod review;
pub mod scorecard;

pub fn render(frame: &mut Frame<'_>, app: &BrainApp) {
    let footer_height = footer_height(app, frame.area().width);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(footer_height),
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
            .unwrap_or_else(|| footer_help(app, area.width))
    });
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(theme.footer))
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn footer_height(app: &BrainApp, width: u16) -> u16 {
    if app.tab() == BrainTab::Scorecard {
        return 2;
    }
    if app.input_prompt().is_none() && app.status().is_none() && !normal_footer_fits(app, width) {
        return compact_footer_help(app).lines().count() as u16 + 1;
    }
    3
}

fn footer_help(app: &BrainApp, width: u16) -> String {
    if app.tab() == BrainTab::Scorecard {
        return normal_footer_help(app);
    }
    packed_normal_footer_help(app, width).unwrap_or_else(|| compact_footer_help(app))
}

fn normal_footer_fits(app: &BrainApp, width: u16) -> bool {
    packed_normal_footer_help(app, width).is_some()
}

fn packed_normal_footer_help(app: &BrainApp, width: u16) -> Option<String> {
    let width = usize::from(width.max(1));
    let help = normal_footer_help(app);
    let mut lines = Vec::new();
    let mut line = String::new();
    for control in help.split("  ").filter(|control| !control.is_empty()) {
        let control_width = UnicodeWidthStr::width(control);
        if control_width > width {
            return None;
        }
        let separator_width = usize::from(!line.is_empty()) * 2;
        if UnicodeWidthStr::width(line.as_str()) + separator_width + control_width > width {
            lines.push(line);
            line = control.to_owned();
        } else {
            if !line.is_empty() {
                line.push_str("  ");
            }
            line.push_str(control);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    (lines.len() <= 2).then(|| lines.join("\n"))
}

fn normal_footer_help(app: &BrainApp) -> String {
    match app.tab() {
        BrainTab::Live => match app.current_review_surface() {
            Some(ReviewSurface::Attention) => lifecycle_footer(
                "j/k select  J/K lists  PgUp/PgDn evidence  x action  c correct  a review  A review all  d archive  D archive reviewed",
                "Enter switch  Tab tabs  r refresh  q quit",
                app.review_projection(ReviewSurface::Attention),
            ),
            Some(ReviewSurface::Recent) => {
                "j/k select  J/K lists  PgUp/PgDn evidence  x action  c correct  a seen  A seen all  Enter switch  Tab tabs  r refresh  q quit".into()
            }
            _ => unreachable!("Live always has an active itemized surface"),
        },
        BrainTab::Review => lifecycle_footer(
            "j/k select  a review  A review all  d archive  D archive reviewed",
            "m canonical  n note+mark  s review+next  Tab tabs  r refresh  q quit",
            app.review_projection(ReviewSurface::Review),
        ),
        BrainTab::Scorecard => "Tab tabs  r refresh  q quit".into(),
        BrainTab::Diagnostics => lifecycle_footer(
            "j/k select  a review  A review all  d archive  D archive reviewed",
            "PgUp/PgDn evidence  Tab tabs  r refresh  q quit",
            app.review_projection(ReviewSurface::Diagnostics),
        ),
    }
}

fn compact_footer_help(app: &BrainApp) -> String {
    let with_undo = |mut lines: Vec<&str>, surface| {
        if app.review_projection(surface).last_archive_count > 0 {
            lines[1] = match surface {
                ReviewSurface::Attention | ReviewSurface::Review => "A review all  u undo",
                ReviewSurface::Diagnostics => "A review all  u undo",
                ReviewSurface::Recent => unreachable!("Recent cannot be archived"),
            };
        }
        lines.join("\n")
    };

    match app.tab() {
        BrainTab::Live => match app.current_review_surface() {
            Some(ReviewSurface::Attention) => with_undo(
                vec![
                    "j/k select  J/K lists",
                    "A review all",
                    "PgUp/PgDn evidence  x action",
                    "c correct  a review",
                    "d archive  D archive reviewed",
                    "Enter switch  Tab tabs",
                    "r refresh  q quit",
                ],
                ReviewSurface::Attention,
            ),
            Some(ReviewSurface::Recent) => [
                "j/k select  J/K lists",
                "PgUp/PgDn evidence  x action",
                "c correct  a seen",
                "A seen all  Enter switch",
                "Tab tabs  r refresh  q quit",
            ]
            .join("\n"),
            _ => unreachable!("Live always has an active itemized surface"),
        },
        BrainTab::Review => with_undo(
            vec![
                "j/k select  a review",
                "A review all",
                "d archive  D archive reviewed",
                "m canonical  n note+mark",
                "s review+next  Tab tabs",
                "r refresh  q quit",
            ],
            ReviewSurface::Review,
        ),
        BrainTab::Scorecard => "Tab tabs  r refresh  q quit".into(),
        BrainTab::Diagnostics => with_undo(
            vec![
                "j/k select  a review",
                "A review all",
                "d archive  D archive reviewed",
                "PgUp/PgDn evidence",
                "Tab tabs  r refresh  q quit",
            ],
            ReviewSurface::Diagnostics,
        ),
    }
}

fn lifecycle_footer(actions: &str, existing: &str, projection: &SurfaceReviewProjection) -> String {
    if projection.last_archive_count > 0 {
        format!("{actions}  u undo  {existing}")
    } else {
        format!("{actions}  {existing}")
    }
}

pub(super) fn review_prefix(target: &ReviewTarget, unseen_label: &str) -> &'static str {
    if target.new_member_keys.is_empty() {
        if unseen_label == "unseen" {
            "seen     "
        } else {
            "reviewed "
        }
    } else if unseen_label == "unseen" {
        "unseen   "
    } else {
        "NEW      "
    }
}

pub(super) fn review_title(label: &str, projection: &SurfaceReviewProjection) -> String {
    format!(
        " {label} ({} new, {} reviewed) ",
        projection.new_count, projection.reviewed_count
    )
}

pub(super) fn review_style(target: &ReviewTarget, theme: &Theme) -> Style {
    if target.new_member_keys.is_empty() {
        Style::default().fg(theme.text_muted)
    } else {
        Style::default()
            .fg(theme.header)
            .add_modifier(Modifier::BOLD)
    }
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
    use coding_brain_core::review_state::{
        BrainReviewProjection, ReviewKey, ReviewSurface, ReviewTarget, SurfaceReviewProjection,
    };
    use coding_brain_core::runtime::{
        BrainRuntime, DecisionSummary, EndpointHealth, MockBrainRuntime, ReviewItemSummary,
        RiskTierSummary, ScorecardSummary,
    };
    use coding_brain_core::theme::{Theme, ThemeMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

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
        let removed_labels = serde_json::from_str::<Vec<String>>(include_str!(
            "../../../../../tests/fixtures/legacy-ui-labels.json"
        ))
        .unwrap();
        for forbidden in removed_labels {
            assert!(!text.contains(&forbidden), "found {forbidden}:\n{text}");
        }
    }

    #[test]
    fn all_itemized_surfaces_share_new_and_reviewed_language() {
        let mut app = lifecycle_render_app(1);

        let live = render_text_at(&app, 140, 38);
        assert!(live.contains("NEW"), "missing NEW marker:\n{live}");
        assert!(
            live.contains("x5 · 2 new"),
            "missing mixed Attention count:\n{live}"
        );
        assert!(
            live.contains("Recent (2 unseen)"),
            "missing Recent unseen count:\n{live}"
        );

        app.handle_key(key(KeyCode::Tab));
        let review = render_text_at(&app, 140, 38);
        assert!(
            review.contains("Review Queue (1 new, 1 reviewed)"),
            "missing Review lifecycle title:\n{review}"
        );
        assert!(
            review.contains("NEW"),
            "missing Review NEW marker:\n{review}"
        );
        assert!(
            review.contains("reviewed"),
            "missing Review reviewed marker:\n{review}"
        );

        app.handle_key(key(KeyCode::Tab));
        let scorecard = render_text_at(&app, 140, 38);
        assert!(
            !scorecard.contains("NEW"),
            "Scorecard changed:\n{scorecard}"
        );

        app.handle_key(key(KeyCode::Tab));
        let diagnostics = render_text_at(&app, 140, 38);
        assert!(
            diagnostics.contains("Diagnostics (1 new, 1 reviewed)"),
            "missing Diagnostics lifecycle title:\n{diagnostics}"
        );
        assert!(
            diagnostics.contains("NEW") && diagnostics.contains("reviewed"),
            "missing Diagnostics lifecycle markers:\n{diagnostics}"
        );
    }

    #[test]
    fn recent_has_no_archive_or_undo_affordance() {
        let mut app = lifecycle_render_app(1);
        app.handle_key(key(KeyCode::Char('J')));

        let text = render_text_at(&app, 140, 38);

        assert!(text.contains("Recent (2 unseen)"), "{text}");
        assert!(text.contains("a seen  A seen all"), "{text}");
        for forbidden in ["d archive", "D archive reviewed", "u undo"] {
            assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
        }
    }

    #[test]
    fn lifecycle_rows_keep_layout_and_reviewed_rows_are_deemphasized() {
        for width in [119, 120, 140] {
            let text = render_text_at(&lifecycle_render_app(1), width, 38);
            assert!(text.contains("NEW"), "missing NEW at {width}:\n{text}");
            assert!(
                text.contains("x5 · 2 new"),
                "missing mixed count at {width}:\n{text}"
            );
            assert!(
                text.contains("Recent (2 unseen)"),
                "missing Recent title at {width}:\n{text}"
            );
            assert!(
                text.contains("x2 · 0 new"),
                "missing fully reviewed Attention count at {width}:\n{text}"
            );
        }

        let mut app = lifecycle_render_app(1);
        let live_theme = *app.theme();
        let live_buffer = render_buffer_at(&app, 140, 38);
        let live_text = buffer_text(&live_buffer);
        let live_reviewed_row = live_text
            .lines()
            .position(|line| line.contains("attention-reviewed"))
            .unwrap();
        let live_reviewed_content =
            content_column(&live_text, "attention-reviewed", "attention-reviewed");
        assert_eq!(
            live_buffer[(live_reviewed_content as u16, live_reviewed_row as u16)].fg,
            live_theme.text_muted
        );

        app.handle_key(key(KeyCode::Tab));
        let theme = *app.theme();
        let before = render_buffer_at(&app, 140, 38);
        let before_text = buffer_text(&before);
        let reviewed_row = before_text
            .lines()
            .position(|line| line.contains("review-reviewed"))
            .unwrap();
        let reviewed_prefix = content_column(&before_text, "review-reviewed", "reviewed");
        let reviewed_content = content_column(&before_text, "review-reviewed", "review-reviewed");
        assert_eq!(
            before[(reviewed_prefix as u16, reviewed_row as u16)].fg,
            theme.text_muted
        );
        assert_eq!(
            before[(reviewed_content as u16, reviewed_row as u16)].fg,
            theme.text_muted
        );

        app.handle_key(key(KeyCode::Char('j')));
        let after_text = render_text_at(&app, 140, 38);
        assert_eq!(
            reviewed_content,
            content_column(&after_text, "review-reviewed", "review-reviewed")
        );
    }

    #[test]
    fn footer_shows_only_surface_valid_lifecycle_controls_and_available_undo() {
        let mut available = lifecycle_render_app(1);
        let attention = render_text_at(&available, 160, 38);
        for expected in [
            "a review",
            "A review all",
            "d archive",
            "D archive reviewed",
            "u undo",
        ] {
            assert!(
                attention.contains(expected),
                "missing {expected}:\n{attention}"
            );
        }

        available.handle_key(key(KeyCode::Char('J')));
        let recent = render_text_at(&available, 160, 38);
        assert!(recent.contains("a seen  A seen all"), "{recent}");
        for forbidden in ["d archive", "D archive reviewed", "u undo"] {
            assert!(!recent.contains(forbidden), "found {forbidden}:\n{recent}");
        }

        available.handle_key(key(KeyCode::Tab));
        let review = render_text_at(&available, 160, 38);
        assert!(review.contains("u undo"), "{review}");
        assert!(review.contains("s review+next"), "{review}");

        let unavailable = render_text_at(&lifecycle_render_app(0), 160, 38);
        assert!(!unavailable.contains("u undo"), "{unavailable}");
    }

    #[test]
    fn scorecard_rendering_does_not_gain_lifecycle_language_or_controls() {
        let mut app = lifecycle_render_app(1);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));

        let text = render_text_at(&app, 140, 38);

        assert!(text.contains("[ Scorecard ]"), "{text}");
        for forbidden in ["NEW", "unseen", "reviewed", "d archive", "u undo"] {
            assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
        }
        assert!(text.contains("Tab tabs  r refresh  q quit"), "{text}");
    }

    #[test]
    fn scorecard_preserves_pre_lifecycle_content_and_footer_geometry() {
        let mut app = lifecycle_render_app(1);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));

        let text = render_text_at(&app, 140, 38);
        let rows = text.lines().collect::<Vec<_>>();

        assert!(
            rows[35].starts_with('└') && rows[35].ends_with('┘'),
            "{text}"
        );
        assert!(rows[36].chars().all(|character| character == '─'), "{text}");
        assert!(rows[37].contains("Tab tabs  r refresh  q quit"), "{text}");
    }

    #[test]
    fn extreme_narrow_lifecycle_footers_show_all_valid_controls() {
        assert_lifecycle_footer_controls_at_width(30);
    }

    #[test]
    fn width_41_lifecycle_footers_show_all_valid_controls() {
        assert_lifecycle_footer_controls_at_width(41);
    }

    #[test]
    fn itemized_footer_fit_transitions_preserve_controls_and_content() {
        let mut app = lifecycle_render_app(1);
        assert_footer_fit_transition(
            &app,
            &[
                "j/k select",
                "J/K lists",
                "PgUp/PgDn evidence",
                "x action",
                "c correct",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "Enter switch",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
            &[
                "Needs Attention",
                "+1 more unresolved",
                "Recent (2 unseen)",
                "Evidence",
                "> NEW",
            ],
        );

        app.handle_key(key(KeyCode::Char('J')));
        assert_footer_fit_transition(
            &app,
            &[
                "j/k select",
                "J/K lists",
                "PgUp/PgDn evidence",
                "x action",
                "c correct",
                "a seen",
                "A seen all",
                "Enter switch",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &["d archive", "D archive reviewed", "u undo"],
            &[
                "Needs Attention",
                "Recent (2 unseen)",
                "Evidence",
                "> unseen",
            ],
        );

        app.handle_key(key(KeyCode::Tab));
        assert_footer_fit_transition(
            &app,
            &[
                "j/k select",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "m canonical",
                "n note+mark",
                "s review+next",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
            &["Review Queue", "Teaching", "> NEW"],
        );

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        assert_footer_fit_transition(
            &app,
            &[
                "j/k select",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "PgUp/PgDn evidence",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
            &["Store integrity", "Diagnostics", "Evidence", "> NEW"],
        );
    }

    fn assert_lifecycle_footer_controls_at_width(width: u16) {
        let mut app = lifecycle_render_app(1);

        let attention = render_text_at(&app, width, 38);
        assert_footer_controls(
            &attention,
            &[
                "j/k select",
                "J/K lists",
                "PgUp/PgDn evidence",
                "x action",
                "c correct",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "Enter switch",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
        );

        app.handle_key(key(KeyCode::Char('J')));
        let recent = render_text_at(&app, width, 38);
        assert_footer_controls(
            &recent,
            &[
                "j/k select",
                "J/K lists",
                "PgUp/PgDn evidence",
                "x action",
                "c correct",
                "a seen",
                "A seen all",
                "Enter switch",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &["d archive", "D archive reviewed", "u undo"],
        );

        app.handle_key(key(KeyCode::Tab));
        let review = render_text_at(&app, width, 38);
        assert_footer_controls(
            &review,
            &[
                "j/k select",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "m canonical",
                "n note+mark",
                "s review+next",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
        );

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Tab));
        let diagnostics = render_text_at(&app, width, 38);
        assert_footer_controls(
            &diagnostics,
            &[
                "j/k select",
                "a review",
                "A review all",
                "d archive",
                "D archive reviewed",
                "u undo",
                "PgUp/PgDn evidence",
                "Tab tabs",
                "r refresh",
                "q quit",
            ],
            &[],
        );
    }

    #[test]
    fn extreme_narrow_archivable_footers_hide_unavailable_undo() {
        let mut app = lifecycle_render_app(0);
        for tab_steps in [0, 1, 3] {
            while app.tab() as usize != tab_steps {
                app.handle_key(key(KeyCode::Tab));
            }
            let text = render_text_at(&app, 30, 38);
            assert!(!text.contains("u undo"), "{text}");
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
            "Diagnostics (2 new, 0 reviewed)",
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
    fn wide_and_narrow_live_keep_action_entry_point_discoverable() {
        for width in [119, 120] {
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
                session_action_preflight_failure: std::sync::Mutex::new(Some(
                    coding_brain_core::runtime::SessionActionFailure {
                        category: coding_brain_core::runtime::SessionActionFailureCategory::ExactSessionUnavailable,
                        diagnostic_persisted: false,
                    },
                )),
                ..MockBrainRuntime::default()
            };
            let mut app = fixture_app(mock);

            let live = render_text_at(&app, width, 24);
            assert!(live.contains("x action"), "{live}");

            app.handle_key(key(KeyCode::Char('x')));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while app.status()
                != Some("No exact live provider session for action; diagnostic unavailable")
                && std::time::Instant::now() < deadline
            {
                app.refresh();
                std::thread::yield_now();
            }
            assert_eq!(
                app.status(),
                Some("No exact live provider session for action; diagnostic unavailable")
            );
            for _ in 0..3 {
                app.handle_key(key(KeyCode::Tab));
            }

            let diagnostics = render_text_at(&app, width, 24);
            assert!(diagnostics.contains("[ Diagnostics ]"), "{diagnostics}");
        }
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
        assert!(text.contains("NEW      Codex  project"), "{text}");
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
                "Diagnostics (2 new, 0 reviewed)",
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
    fn live_active_row_uses_theme_highlight_without_shifting_content() {
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::None] {
            let mut app = populated_live_app_with_theme(mode);
            let theme = *app.theme();

            let attention_buffer = render_buffer_at(&app, 110, 38);
            let attention_text = buffer_text(&attention_buffer);
            let attention_row = attention_text
                .lines()
                .position(|line| line.contains("attention-1"))
                .unwrap();
            let attention_column = content_column(&attention_text, "attention-1", "attention-1");
            let recent_row = attention_text
                .lines()
                .position(|line| line.contains("recent-1"))
                .unwrap();
            let recent_column = content_column(&attention_text, "recent-1", "recent-1");

            assert_eq!(
                attention_buffer[(attention_column as u16, attention_row as u16)].fg,
                theme.header
            );
            assert!(
                attention_buffer[(attention_column as u16, attention_row as u16)]
                    .modifier
                    .contains(Modifier::BOLD)
            );
            assert!(
                !attention_buffer[(recent_column as u16, recent_row as u16)]
                    .modifier
                    .contains(Modifier::BOLD)
            );
            assert_eq!(attention_text.matches("> ").count(), 1);

            app.handle_key(key(KeyCode::Char('J')));
            let recent_buffer = render_buffer_at(&app, 110, 38);
            let recent_text = buffer_text(&recent_buffer);
            let attention_row_after = recent_text
                .lines()
                .position(|line| line.contains("attention-1"))
                .unwrap();
            let attention_column_after = content_column(&recent_text, "attention-1", "attention-1");
            let recent_row_after = recent_text
                .lines()
                .position(|line| line.contains("recent-1"))
                .unwrap();
            let recent_column_after = content_column(&recent_text, "recent-1", "recent-1");

            assert!(
                !recent_buffer[(attention_column_after as u16, attention_row_after as u16)]
                    .modifier
                    .contains(Modifier::BOLD)
            );
            assert_eq!(
                recent_buffer[(recent_column_after as u16, recent_row_after as u16)].fg,
                theme.header
            );
            assert!(
                recent_buffer[(recent_column_after as u16, recent_row_after as u16)]
                    .modifier
                    .contains(Modifier::BOLD)
            );
            assert_eq!(attention_column, attention_column_after);
            assert_eq!(recent_column, recent_column_after);
            assert!(
                recent_text
                    .lines()
                    .nth(recent_row_after)
                    .unwrap()
                    .contains("ALLOW")
            );
            assert_eq!(recent_text.matches("> ").count(), 1);
        }
    }

    #[test]
    fn selectable_non_live_lists_use_theme_highlight_for_active_row() {
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::None] {
            let mut review = fixture_app_with_theme(
                MockBrainRuntime {
                    review_queue: vec![ReviewItemSummary {
                        decision: decision(),
                        reason: "review-highlight-target".into(),
                        score: 90.0,
                    }],
                    ..MockBrainRuntime::default()
                },
                mode,
            );
            review.handle_key(key(KeyCode::Tab));
            let theme = *review.theme();
            let review_buffer = render_buffer_at(&review, 110, 38);
            let review_text = buffer_text(&review_buffer);
            let review_row = review_text
                .lines()
                .position(|line| line.contains("review-highlight-target"))
                .unwrap();
            let review_column = content_column(
                &review_text,
                "review-highlight-target",
                "review-highlight-target",
            );
            let review_cell = &review_buffer[(review_column as u16, review_row as u16)];
            assert_eq!(review_cell.fg, theme.header);
            assert!(review_cell.modifier.contains(Modifier::BOLD));
            assert_eq!(review_text.matches("> ").count(), 1);

            let mut diagnostics = populated_diagnostics_app(mode);
            let diagnostics_buffer = render_buffer_at(&diagnostics, 120, 38);
            let diagnostics_text = buffer_text(&diagnostics_buffer);
            let rows = diagnostics_text
                .lines()
                .enumerate()
                .filter_map(|(row, line)| {
                    line.contains("Codex  project  Bash")
                        .then(|| line.find("Codex").map(|column| (row, column)))
                        .flatten()
                })
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 2);
            let selected = &diagnostics_buffer[(rows[0].1 as u16, rows[0].0 as u16)];
            let inactive = &diagnostics_buffer[(rows[1].1 as u16, rows[1].0 as u16)];
            assert_eq!(selected.fg, theme.header);
            assert!(selected.modifier.contains(Modifier::BOLD));
            assert!(!inactive.modifier.contains(Modifier::BOLD));
            assert_eq!(diagnostics_text.matches("> ").count(), 1);

            diagnostics.handle_key(key(KeyCode::Char('j')));
            let moved_buffer = render_buffer_at(&diagnostics, 120, 38);
            let formerly_selected = &moved_buffer[(rows[0].1 as u16, rows[0].0 as u16)];
            let now_selected = &moved_buffer[(rows[1].1 as u16, rows[1].0 as u16)];
            assert!(!formerly_selected.modifier.contains(Modifier::BOLD));
            assert_eq!(now_selected.fg, theme.header);
            assert!(now_selected.modifier.contains(Modifier::BOLD));
        }
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

        assert!(row.contains("proje"), "{row}");
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
            attention
                .lines()
                .any(|line| line.contains("> NEW      SEND ?")),
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
        let mock = Arc::new(aligned_mock(mock));
        BrainApp::new(
            BrainRuntime::new(mock.clone(), mock),
            Theme::from_mode(mode),
        )
    }

    fn lifecycle_render_app(last_archive_count: usize) -> BrainApp {
        let attention = AttentionItem {
            activity: activity("attention-mixed", DeliveryState::Unknown),
            occurrences: 5,
            unresolved_occurrences: 5,
        };
        let reviewed_attention = AttentionItem {
            activity: activity("attention-reviewed", DeliveryState::Unknown),
            occurrences: 2,
            unresolved_occurrences: 2,
        };
        let mut recent_first = activity("recent-new-1", DeliveryState::Delivered);
        recent_first.state = ActivityState::Allowed;
        let mut recent_second = activity("recent-new-2", DeliveryState::Delivered);
        recent_second.state = ActivityState::Allowed;
        let mut diagnostic_first = activity("diagnostic-new", DeliveryState::NotApplicable);
        diagnostic_first.kind = ActivityKind::Diagnostic;
        let mut diagnostic_second = activity("diagnostic-reviewed", DeliveryState::NotApplicable);
        diagnostic_second.kind = ActivityKind::Diagnostic;

        let mut review_first = decision();
        review_first.id = "review-new".into();
        let mut review_second = decision();
        review_second.id = "review-reviewed".into();
        let review_queue = vec![
            ReviewItemSummary {
                decision: review_first,
                reason: "new review reason".into(),
                score: 90.0,
            },
            ReviewItemSummary {
                decision: review_second,
                reason: "reviewed reason".into(),
                score: 80.0,
            },
        ];
        let snapshot = ActivitySnapshot {
            attention: vec![attention.clone(), reviewed_attention.clone()],
            recent: vec![recent_first, recent_second],
            diagnostic_events: vec![diagnostic_first, diagnostic_second],
            unresolved_count: 8,
            diagnostics: Default::default(),
        };
        let review_state = BrainReviewProjection {
            attention: lifecycle_projection(
                ReviewSurface::Attention,
                vec![
                    (attention.review_display_id(), 2, 3),
                    (reviewed_attention.review_display_id(), 0, 2),
                ],
                2,
                5,
                last_archive_count,
            ),
            review: lifecycle_projection(
                ReviewSurface::Review,
                vec![
                    (review_queue[0].review_display_id(), 1, 0),
                    (review_queue[1].review_display_id(), 0, 1),
                ],
                1,
                1,
                last_archive_count,
            ),
            diagnostics: lifecycle_projection(
                ReviewSurface::Diagnostics,
                vec![
                    ("diagnostic-new".into(), 1, 0),
                    ("diagnostic-reviewed".into(), 0, 1),
                ],
                1,
                1,
                last_archive_count,
            ),
            recent: lifecycle_projection(
                ReviewSurface::Recent,
                vec![("recent-new-1".into(), 1, 0), ("recent-new-2".into(), 1, 0)],
                2,
                0,
                0,
            ),
        };
        let mock = Arc::new(MockBrainRuntime {
            activity_snapshot: snapshot,
            review_queue,
            review_state,
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        });
        BrainApp::new(
            BrainRuntime::new(mock.clone(), mock),
            Theme::from_mode(ThemeMode::Dark),
        )
    }

    fn lifecycle_projection(
        surface: ReviewSurface,
        rows: Vec<(String, usize, usize)>,
        new_count: usize,
        reviewed_count: usize,
        last_archive_count: usize,
    ) -> SurfaceReviewProjection {
        let visible_items = rows.len();
        let targets = rows
            .into_iter()
            .map(|(display_id, new_members, reviewed_members)| ReviewTarget {
                surface,
                new_member_keys: (0..new_members)
                    .map(|index| {
                        ReviewKey::derive(surface, format!("{display_id}:new:{index}").as_bytes())
                    })
                    .collect(),
                reviewed_member_keys: (0..reviewed_members)
                    .map(|index| {
                        ReviewKey::derive(
                            surface,
                            format!("{display_id}:reviewed:{index}").as_bytes(),
                        )
                    })
                    .collect(),
                display_id,
            })
            .collect();
        SurfaceReviewProjection::from_items(
            surface,
            7,
            targets,
            visible_items,
            new_count,
            reviewed_count,
            last_archive_count,
        )
        .unwrap()
    }

    fn aligned_mock(mut mock: MockBrainRuntime) -> MockBrainRuntime {
        mock.review_state = aligned_review_state(&mock.activity_snapshot, &mock.review_queue);
        mock
    }

    fn aligned_review_state(
        snapshot: &ActivitySnapshot,
        review_queue: &[ReviewItemSummary],
    ) -> BrainReviewProjection {
        BrainReviewProjection {
            attention: fixture_attention_projection(&snapshot.attention),
            review: fixture_review_projection(review_queue),
            diagnostics: fixture_projection(
                ReviewSurface::Diagnostics,
                snapshot
                    .diagnostic_events
                    .iter()
                    .map(|item| item.activity_id.as_str()),
            ),
            recent: fixture_projection(
                ReviewSurface::Recent,
                snapshot.recent.iter().map(|item| item.activity_id.as_str()),
            ),
        }
    }

    fn fixture_attention_projection(items: &[AttentionItem]) -> SurfaceReviewProjection {
        let targets = items
            .iter()
            .map(|item| ReviewTarget {
                surface: ReviewSurface::Attention,
                display_id: item.review_display_id(),
                new_member_keys: (0..item.occurrences)
                    .map(|occurrence| {
                        ReviewKey::derive(
                            ReviewSurface::Attention,
                            format!("{}:{occurrence}", item.activity_id).as_bytes(),
                        )
                    })
                    .collect(),
                reviewed_member_keys: Vec::new(),
            })
            .collect::<Vec<_>>();
        let new_count = targets
            .iter()
            .map(|target| target.new_member_keys.len())
            .sum();
        SurfaceReviewProjection::from_items(
            ReviewSurface::Attention,
            0,
            targets,
            items.len(),
            new_count,
            0,
            0,
        )
        .unwrap()
    }

    fn fixture_review_projection(items: &[ReviewItemSummary]) -> SurfaceReviewProjection {
        let targets = items
            .iter()
            .map(|item| ReviewTarget {
                surface: ReviewSurface::Review,
                display_id: item.review_display_id(),
                new_member_keys: vec![ReviewKey::derive(
                    ReviewSurface::Review,
                    &item.decision.review_source_identity(),
                )],
                reviewed_member_keys: Vec::new(),
            })
            .collect::<Vec<_>>();
        SurfaceReviewProjection::from_items(
            ReviewSurface::Review,
            0,
            targets,
            items.len(),
            items.len(),
            0,
            0,
        )
        .unwrap()
    }

    fn fixture_projection<'a>(
        surface: ReviewSurface,
        display_ids: impl IntoIterator<Item = &'a str>,
    ) -> SurfaceReviewProjection {
        let items = display_ids
            .into_iter()
            .map(|display_id| ReviewTarget {
                surface,
                display_id: display_id.into(),
                new_member_keys: vec![ReviewKey::derive(surface, display_id.as_bytes())],
                reviewed_member_keys: Vec::new(),
            })
            .collect::<Vec<_>>();
        let visible_items = items.len();
        SurfaceReviewProjection::from_items(surface, 0, items, visible_items, visible_items, 0, 0)
            .unwrap()
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

    fn populated_live_app_with_theme(mode: ThemeMode) -> BrainApp {
        let mut attention = activity("attention-1", DeliveryState::Unknown);
        attention.normalized_command = Some("attention-1".into());
        let mut recent = activity("recent-1", DeliveryState::Delivered);
        recent.state = ActivityState::Allowed;
        fixture_app_with_theme(
            MockBrainRuntime {
                activity_snapshot: ActivitySnapshot {
                    attention: vec![AttentionItem {
                        activity: attention,
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    }],
                    recent: vec![recent],
                    unresolved_count: 1,
                    ..ActivitySnapshot::default()
                },
                endpoint_health: online(),
                ..MockBrainRuntime::default()
            },
            mode,
        )
    }

    fn render_buffer_at(app: &BrainApp, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_text_at(app: &BrainApp, width: u16, height: u16) -> String {
        buffer_text(&render_buffer_at(app, width, height))
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

    fn assert_footer_controls(text: &str, expected: &[&str], forbidden: &[&str]) {
        for control in expected {
            assert!(text.contains(control), "missing {control}:\n{text}");
        }
        for control in forbidden {
            assert!(!text.contains(control), "found {control}:\n{text}");
        }
    }

    fn assert_footer_fit_transition(
        app: &BrainApp,
        expected_controls: &[&str],
        forbidden_controls: &[&str],
        expected_content: &[&str],
    ) {
        let transition = (30..=200)
            .find(|width| normal_footer_fits(app, *width))
            .expect("normal footer should fit by 200 columns");
        assert!(transition > 30);
        assert!(!normal_footer_fits(app, transition - 1));
        assert!(normal_footer_fits(app, transition));
        assert!(normal_footer_fits(app, transition + 1));

        for width in [transition - 1, transition, transition + 1] {
            let text = render_text_at(app, width, 38);
            assert_footer_controls(&text, expected_controls, forbidden_controls);
            for content in expected_content {
                assert!(
                    text.contains(content),
                    "missing {content} at {width}:\n{text}"
                );
            }
            if width < transition {
                assert!(footer_height(app, width) > 3);
            } else {
                assert_eq!(footer_height(app, width), 3);
            }
        }
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
                provider_session_id: None,
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
