use std::borrow::Cow;

use coding_brain_core::brain_activity::{
    ActivityItem, ActivityOutcome, ActivityState, DeliveryState,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::brain_app::BrainApp;

const WIDE_BREAKPOINT: u16 = 120;
const MAX_NARROW_EVIDENCE_HEIGHT: u16 = 12;
const MIN_LIST_HEIGHT: u16 = 3;
const BADGE_WIDTH: usize = 9;
const FIELD_GAP: usize = 2;
const MIN_PROJECT_WIDTH: usize = 4;
const MIN_ACTION_WIDTH: usize = 4;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    if area.width >= WIDE_BREAKPOINT {
        render_wide(frame, area, app);
    } else {
        render_narrow(frame, area, app);
    }
}

fn render_wide(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(area);
    let lists = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(columns[0]);
    render_attention(frame, lists[0], app);
    render_recent(frame, lists[1], app);
    render_evidence(frame, columns[1], app, EvidenceDensity::Wide);
}

fn render_narrow(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let evidence_height = evidence_height(app, area.width)
        .min(MAX_NARROW_EVIDENCE_HEIGHT)
        .min(area.height.saturating_sub(MIN_LIST_HEIGHT * 2));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(MIN_LIST_HEIGHT * 2),
            Constraint::Length(evidence_height),
        ])
        .split(area);
    let lists = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[0]);
    render_attention(frame, lists[0], app);
    render_recent(frame, lists[1], app);
    render_evidence(frame, rows[1], app, EvidenceDensity::Compact);
}

fn evidence_height(app: &BrainApp, width: u16) -> u16 {
    let inner_width = usize::from(width.saturating_sub(2).max(1));
    let content_height = app.selected_live_activity().map_or(1, |item| {
        evidence_lines(
            item,
            EvidenceDensity::Compact,
            app.theme(),
            app.selected_live_is_attention(),
        )
        .iter()
        .map(|line| line.width().max(1).div_ceil(inner_width))
        .sum::<usize>()
    });
    content_height.saturating_add(2).min(usize::from(u16::MAX)) as u16
}

fn render_attention(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let snapshot = app.snapshot();
    let row_width = usize::from(area.width.saturating_sub(4));
    let mut items = snapshot
        .attention
        .iter()
        .map(|item| {
            ListItem::new(activity_row(
                &item.activity,
                (item.occurrences > 1).then_some(item.occurrences),
                row_width,
                app.theme(),
            ))
        })
        .collect::<Vec<_>>();
    let displayed_unresolved = snapshot
        .attention
        .iter()
        .map(|item| item.unresolved_occurrences)
        .sum::<usize>();
    let overflow = snapshot
        .unresolved_count
        .saturating_sub(displayed_unresolved);
    if overflow > 0 {
        items.push(ListItem::new(format!("+{overflow} more unresolved")));
    }
    if items.is_empty() {
        items.push(ListItem::new("No unresolved decisions"));
    }
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Needs Attention ({}) ", snapshot.unresolved_count))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .fg(app.theme().header)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = ListState::default();
    if let Some(index) = app.selected_attention_index() {
        state.select(Some(index));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_recent(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let snapshot = app.snapshot();
    let row_width = usize::from(area.width.saturating_sub(4));
    let items = if snapshot.recent.is_empty() {
        vec![ListItem::new("No recent resolved activity")]
    } else {
        snapshot
            .recent
            .iter()
            .map(|item| ListItem::new(activity_row(item, None, row_width, app.theme())))
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().title(" Recent ").borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .fg(app.theme().header)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = ListState::default();
    if let Some(index) = app.selected_recent_index() {
        state.select(Some(index));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceDensity {
    Wide,
    Compact,
}

fn evidence_lines(
    item: &ActivityItem,
    density: EvidenceDensity,
    theme: &coding_brain_core::theme::Theme,
    needs_attention: bool,
) -> Vec<Line<'static>> {
    let badge = activity_badge(item);
    let status_style = badge.style(theme);
    let label_style = Style::default()
        .fg(theme.text_muted)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(theme.text_primary);
    let project_style = value_style.add_modifier(Modifier::BOLD);
    let attention_label = if needs_attention {
        "Needs attention"
    } else {
        "Recent"
    };
    let outcome = safe_evidence_text(&activity_status(item));
    let action = safe_evidence_text(command_label(item));
    let project = safe_evidence_text(project_label(item).as_ref());
    let provider = safe_evidence_text(provider_label(item));
    let activity_id = safe_evidence_text(&item.activity_id);

    let mut optional = Vec::new();
    if let Some(confidence) = item.confidence {
        optional.push(("Confidence", format!("{:.0}%", confidence * 100.0)));
    }
    if let Some(reasoning) = &item.reasoning {
        optional.push(("Reason", safe_evidence_text(reasoning)));
    }
    if let Some(correction) = item.correction {
        optional.push(("Resolved", format!("{correction:?}")));
    }
    if let Some(note) = &item.note {
        optional.push(("Note", safe_evidence_text(note)));
    }

    match density {
        EvidenceDensity::Wide => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(format!("{:<BADGE_WIDTH$}", badge.label), status_style),
                    Span::raw("  "),
                    Span::styled(attention_label, value_style),
                ]),
                Line::raw(""),
                Line::styled("OUTCOME", label_style),
                Line::styled(outcome, value_style),
            ];
            for (label, value) in optional {
                lines.push(evidence_field(label, value, label_style, value_style));
            }
            lines.extend([
                Line::raw(""),
                Line::styled("ACTION", label_style),
                Line::styled(action, value_style),
                Line::raw(""),
                Line::styled("CONTEXT", label_style),
                evidence_field("Project", project, label_style, project_style),
                evidence_field("Provider", provider, label_style, value_style),
                evidence_field("Activity", activity_id, label_style, value_style),
            ]);
            lines
        }
        EvidenceDensity::Compact => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Status  ", label_style),
                    Span::styled(badge.label, status_style),
                    Span::raw("  "),
                    Span::styled(attention_label, value_style),
                ]),
                evidence_field("Outcome", outcome, label_style, value_style),
            ];
            for (label, value) in optional {
                lines.push(evidence_field(label, value, label_style, value_style));
            }
            lines.extend([
                evidence_field("Action", action, label_style, value_style),
                Line::styled("Context", label_style),
                evidence_field("Project", project, label_style, project_style),
                evidence_field("Provider", provider, label_style, value_style),
                evidence_field("Activity", activity_id, label_style, value_style),
            ]);
            lines
        }
    }
}

fn evidence_field(
    label: &'static str,
    value: String,
    label_style: Style,
    value_style: Style,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), label_style),
        Span::styled(value, value_style),
    ])
}

pub(super) fn safe_evidence_text(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

fn render_evidence(frame: &mut Frame<'_>, area: Rect, app: &BrainApp, density: EvidenceDensity) {
    let lines = match app.selected_live_activity() {
        Some(item) => evidence_lines(item, density, app.theme(), app.selected_live_is_attention()),
        None => vec![Line::raw("Select an activity to inspect its evidence")],
    };
    let inner_width = usize::from(area.width.saturating_sub(2).max(1));
    let content_height = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(inner_width))
        .sum::<usize>()
        .min(usize::from(u16::MAX)) as u16;
    let page_size = area.height.saturating_sub(2).max(1);
    let max_scroll = content_height.saturating_sub(page_size);
    app.update_live_evidence_metrics(page_size, max_scroll);
    let scroll = app.live_evidence_scroll();
    let above = (scroll > 0).then_some("↑ more");
    let below = (scroll < max_scroll).then_some("↓ more");
    let indicators = [above, below]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  ");
    let title = if indicators.is_empty() {
        " Evidence ".to_owned()
    } else {
        format!(" Evidence {indicators} ")
    };
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .scroll((scroll, 0))
        .block(Block::default().title(title).borders(Borders::ALL));
    frame.render_widget(paragraph, area);
}

fn provider_label(item: &ActivityItem) -> &'static str {
    item.session
        .as_ref()
        .map(|session| session.provider.label())
        .unwrap_or("Unknown")
}

pub(crate) fn activity_status(item: &ActivityItem) -> String {
    if let Some(outcome) = item.outcome {
        return format!(
            "{} · outcome confirmed: {}",
            decision_state(item.state),
            match outcome {
                ActivityOutcome::Succeeded => "succeeded",
                ActivityOutcome::Failed => "failed",
                ActivityOutcome::Cancelled => "cancelled",
                ActivityOutcome::Completed => "completed",
            }
        );
    }
    if let Some(correction) = item.correction {
        return format!(
            "resolved: {}",
            match correction {
                coding_brain_core::brain_activity::CorrectionDisposition::BrainRight =>
                    "brain right",
                coding_brain_core::brain_activity::CorrectionDisposition::BrainWrong =>
                    "brain wrong",
                coding_brain_core::brain_activity::CorrectionDisposition::Exception => "exception",
            }
        );
    }
    match item.delivery {
        DeliveryState::Failed => format!(
            "{} · delivery failed · execution not confirmed",
            decision_state(item.state)
        ),
        DeliveryState::Unknown => format!(
            "{} · delivery unknown · execution not confirmed",
            decision_state(item.state)
        ),
        DeliveryState::Delivered => {
            format!("{} · response emitted", decision_state(item.state))
        }
        DeliveryState::NotApplicable => decision_state(item.state).into(),
    }
}

fn decision_state(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Observed => "observed",
        ActivityState::Evaluating => "evaluating",
        ActivityState::Allowed => "allowed",
        ActivityState::Denied => "denied",
        ActivityState::Abstained => "abstained",
        ActivityState::Error => "error",
        ActivityState::Delivered => "delivered",
        ActivityState::DeliveryFailed => "delivery failed",
        ActivityState::Interrupted => "interrupted",
        ActivityState::Outcome => "outcome",
        ActivityState::Correction => "correction",
    }
}

pub(super) fn project_label(item: &ActivityItem) -> Cow<'_, str> {
    if let Some(label) = item
        .project
        .label
        .as_deref()
        .filter(|label| !label.is_empty())
    {
        return Cow::Borrowed(label);
    }
    if let Some(name) = item.project.cwd.file_name() {
        return name.to_string_lossy();
    }
    let path = item.project.cwd.to_string_lossy();
    if path.is_empty() {
        Cow::Borrowed("unknown project")
    } else {
        path
    }
}

fn command_label(item: &ActivityItem) -> &str {
    item.normalized_command
        .as_deref()
        .or(item.tool.as_deref())
        .unwrap_or("no command")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BadgeTone {
    Positive,
    Negative,
    Warning,
    Active,
    Muted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivityBadge {
    label: &'static str,
    tone: BadgeTone,
}

fn activity_badge(item: &ActivityItem) -> ActivityBadge {
    if let Some(outcome) = item.outcome {
        return match outcome {
            ActivityOutcome::Succeeded | ActivityOutcome::Completed => {
                ActivityBadge::new("DONE", BadgeTone::Positive)
            }
            ActivityOutcome::Failed => ActivityBadge::new("FAILED", BadgeTone::Negative),
            ActivityOutcome::Cancelled => ActivityBadge::new("CANCEL", BadgeTone::Warning),
        };
    }
    if item.correction.is_some() {
        return ActivityBadge::new("RESOLVED", BadgeTone::Positive);
    }
    if matches!(
        (item.state, item.delivery),
        (ActivityState::Denied, DeliveryState::Delivered)
    ) {
        return ActivityBadge::new("BLOCKED", BadgeTone::Negative);
    }
    match item.delivery {
        DeliveryState::Failed => return ActivityBadge::new("SEND FAIL", BadgeTone::Negative),
        DeliveryState::Unknown => return ActivityBadge::new("SEND ?", BadgeTone::Warning),
        DeliveryState::Delivered | DeliveryState::NotApplicable => {}
    }
    match item.state {
        ActivityState::Observed => ActivityBadge::new("OBSERVE", BadgeTone::Muted),
        ActivityState::Evaluating => ActivityBadge::new("EVAL", BadgeTone::Active),
        ActivityState::Allowed => ActivityBadge::new("ALLOW", BadgeTone::Positive),
        ActivityState::Denied => ActivityBadge::new("DENY", BadgeTone::Negative),
        ActivityState::Abstained => ActivityBadge::new("ABSTAIN", BadgeTone::Warning),
        ActivityState::Error => ActivityBadge::new("ERROR", BadgeTone::Negative),
        ActivityState::Delivered => ActivityBadge::new("SENT", BadgeTone::Positive),
        ActivityState::DeliveryFailed => ActivityBadge::new("SEND FAIL", BadgeTone::Negative),
        ActivityState::Interrupted => ActivityBadge::new("STOPPED", BadgeTone::Warning),
        ActivityState::Outcome => ActivityBadge::new("OUTCOME", BadgeTone::Active),
        ActivityState::Correction => ActivityBadge::new("RESOLVED", BadgeTone::Positive),
    }
}

impl ActivityBadge {
    const fn new(label: &'static str, tone: BadgeTone) -> Self {
        Self { label, tone }
    }

    fn style(self, theme: &coding_brain_core::theme::Theme) -> Style {
        let color = match self.tone {
            BadgeTone::Positive => theme.success,
            BadgeTone::Negative => theme.error,
            BadgeTone::Warning => theme.status_waiting,
            BadgeTone::Active => theme.header,
            BadgeTone::Muted => theme.text_muted,
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }
}

pub(super) fn safe_row_text(value: &str) -> String {
    let mut output = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
        } else {
            if pending_space {
                output.push(' ');
                pending_space = false;
            }
            if character.is_control() {
                output.extend(character.escape_default());
            } else {
                output.push(character);
            }
        }
    }
    output
}

pub(super) fn truncate_display(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(1);
    let mut width = 0;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + character_width > content_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

fn activity_row(
    item: &ActivityItem,
    occurrences: Option<usize>,
    width: usize,
    theme: &coding_brain_core::theme::Theme,
) -> Line<'static> {
    let badge = activity_badge(item);
    let project = safe_row_text(project_label(item).as_ref());
    let provider = safe_row_text(provider_label(item));
    let action = safe_row_text(command_label(item));
    let count = occurrences.map(|count| format!("x{count}"));
    let count_width = count.as_deref().map(UnicodeWidthStr::width).unwrap_or(0);
    let count_reserve = count.as_ref().map(|_| FIELD_GAP + count_width).unwrap_or(0);
    let body_width = width.saturating_sub(count_reserve);

    let mut spans = Vec::new();
    let badge_text = format!("{:<BADGE_WIDTH$}", badge.label);
    let visible_badge = truncate_display(&badge_text, body_width);
    spans.push(Span::styled(visible_badge, badge.style(theme)));
    let mut used = spans[0].width();

    if body_width > used + FIELD_GAP {
        spans.push(Span::raw(" ".repeat(FIELD_GAP)));
        used += FIELD_GAP;

        let remaining = body_width.saturating_sub(used);
        let full_fixed = UnicodeWidthStr::width(project.as_str())
            + FIELD_GAP
            + UnicodeWidthStr::width(provider.as_str())
            + FIELD_GAP
            + MIN_ACTION_WIDTH;
        let show_provider = remaining >= full_fixed;
        let provider_reserve = if show_provider {
            UnicodeWidthStr::width(provider.as_str()) + FIELD_GAP
        } else {
            0
        };
        let show_action =
            show_provider || remaining >= MIN_PROJECT_WIDTH + FIELD_GAP + MIN_ACTION_WIDTH;
        let action_reserve = if show_action {
            MIN_ACTION_WIDTH + FIELD_GAP
        } else {
            0
        };
        let project_limit =
            if remaining >= UnicodeWidthStr::width(project.as_str()) + action_reserve {
                UnicodeWidthStr::width(project.as_str())
            } else {
                remaining
                    .saturating_sub(action_reserve)
                    .min(16)
                    .max(remaining.min(1))
            };
        let visible_project = truncate_display(&project, project_limit);
        used += UnicodeWidthStr::width(visible_project.as_str());
        spans.push(Span::styled(
            visible_project,
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ));

        if show_provider && body_width >= used + provider_reserve {
            spans.push(Span::raw(" ".repeat(FIELD_GAP)));
            spans.push(Span::styled(
                provider.clone(),
                Style::default().fg(theme.text_muted),
            ));
            used += provider_reserve;
        }

        let action_room = body_width.saturating_sub(used + FIELD_GAP);
        if show_action && action_room >= MIN_ACTION_WIDTH {
            spans.push(Span::raw(" ".repeat(FIELD_GAP)));
            let visible_action = truncate_display(&action, action_room);
            spans.push(Span::styled(
                visible_action,
                Style::default().fg(theme.text_primary),
            ));
        }
    }

    if let Some(count) = count {
        let rendered_width = spans.iter().map(Span::width).sum::<usize>();
        let padding = width.saturating_sub(rendered_width + count_width);
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(
            count,
            Style::default()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use coding_brain_core::brain_activity::{
        ActivityKind, CorrectionDisposition, ProjectEvidence, SessionTarget,
        SessionTargetProvenance,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::provider::AgentProvider;
    use coding_brain_core::theme::{Theme, ThemeMode};

    use super::*;

    #[test]
    fn compact_badges_preserve_activity_status_precedence() {
        let mut item = activity();
        for (outcome, expected) in [
            (ActivityOutcome::Succeeded, "DONE"),
            (ActivityOutcome::Completed, "DONE"),
            (ActivityOutcome::Failed, "FAILED"),
            (ActivityOutcome::Cancelled, "CANCEL"),
        ] {
            item.outcome = Some(outcome);
            assert_eq!(activity_badge(&item).label, expected);
        }

        item.outcome = None;
        item.correction = Some(CorrectionDisposition::BrainWrong);
        assert_eq!(activity_badge(&item).label, "RESOLVED");

        item.correction = None;
        item.state = ActivityState::Denied;
        item.delivery = DeliveryState::Delivered;
        assert_eq!(activity_badge(&item).label, "BLOCKED");

        item.state = ActivityState::Allowed;
        item.delivery = DeliveryState::Failed;
        assert_eq!(activity_badge(&item).label, "SEND FAIL");
        item.delivery = DeliveryState::Unknown;
        assert_eq!(activity_badge(&item).label, "SEND ?");
    }

    #[test]
    fn compact_badges_cover_every_activity_state() {
        let mut item = activity();
        item.delivery = DeliveryState::NotApplicable;
        for (state, expected) in [
            (ActivityState::Observed, "OBSERVE"),
            (ActivityState::Evaluating, "EVAL"),
            (ActivityState::Allowed, "ALLOW"),
            (ActivityState::Denied, "DENY"),
            (ActivityState::Abstained, "ABSTAIN"),
            (ActivityState::Error, "ERROR"),
            (ActivityState::Delivered, "SENT"),
            (ActivityState::DeliveryFailed, "SEND FAIL"),
            (ActivityState::Interrupted, "STOPPED"),
            (ActivityState::Outcome, "OUTCOME"),
            (ActivityState::Correction, "RESOLVED"),
        ] {
            item.state = state;
            assert_eq!(activity_badge(&item).label, expected);
        }
    }

    #[test]
    fn display_truncation_uses_columns_and_keeps_a_visible_ellipsis() {
        assert_eq!(truncate_display("界面", 3), "界…");
        assert_eq!(truncate_display("界面", 4), "界面");
        assert_eq!(truncate_display("abc", 1), "…");
        assert_eq!(truncate_display("abc", 0), "");
    }

    #[test]
    fn row_text_collapses_whitespace_and_escapes_controls() {
        assert_eq!(
            safe_row_text("cargo\t test\n--all\u{1b}"),
            "cargo test --all\\u{1b}"
        );
    }

    #[test]
    fn constrained_rows_omit_provider_before_project_and_action() {
        let mut item = activity();
        item.project.label = Some("coding-brain".into());
        item.normalized_command = Some("cargo test --workspace".into());
        let theme = Theme::from_mode(ThemeMode::Dark);

        let full = line_text(&activity_row(&item, Some(3), 56, &theme));
        assert!(full.contains("Codex"));
        assert!(full.contains("cargo test"));
        assert!(full.ends_with("x3"));

        let constrained = line_text(&activity_row(&item, Some(3), 36, &theme));
        assert!(!constrained.contains("Codex"));
        assert!(constrained.contains("coding-brain"));
        assert!(constrained.ends_with("x3"));
    }

    #[test]
    fn occurrence_count_is_right_aligned_inside_row_width() {
        let theme = Theme::from_mode(ThemeMode::None);
        let row = activity_row(&activity(), Some(12), 48, &theme);

        assert_eq!(row.width(), 48);
        assert!(line_text(&row).ends_with("x12"));
    }

    #[test]
    fn extreme_rows_keep_badge_project_and_count_before_action() {
        let theme = Theme::from_mode(ThemeMode::Dark);
        let row = activity_row(&activity(), Some(3), 24, &theme);
        let text = line_text(&row);

        assert!(text.contains("SEND ?"), "{text}");
        assert!(text.contains("cod"), "{text}");
        assert!(!text.contains("cargo"), "{text}");
        assert!(text.ends_with("x3"), "{text}");
    }

    #[test]
    fn row_hierarchy_survives_dark_light_and_monochrome_themes() {
        let item = activity();
        let mut rendered = Vec::new();
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::None] {
            let theme = Theme::from_mode(mode);
            let row = activity_row(&item, None, 64, &theme);

            assert_eq!(row.spans[0].style.fg, Some(theme.status_waiting));
            assert!(row.spans[0].style.add_modifier.contains(Modifier::BOLD));
            assert!(row.spans[2].style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(row.spans[4].style.fg, Some(theme.text_muted));
            rendered.push(line_text(&row));
        }

        assert!(rendered.windows(2).all(|pair| pair[0] == pair[1]));
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn activity() -> ActivityItem {
        let project_id = ProjectId::Stable("project-1".into());
        ActivityItem {
            activity_id: "activity-1".into(),
            kind: ActivityKind::Decision,
            recorded_at_ms: 1,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: PathBuf::from("/work/coding-brain"),
                label: Some("coding-brain".into()),
            },
            session: Some(SessionTarget {
                provider: AgentProvider::Codex,
                session_id: "session-1".into(),
                turn_id: None,
                tool_use_id: None,
                project_id,
                cwd: PathBuf::from("/work/coding-brain"),
                provider_hints: Vec::new(),
                provenance: SessionTargetProvenance::Structured,
            }),
            state: ActivityState::Denied,
            delivery: DeliveryState::Unknown,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: Some("fingerprint".into()),
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
}
