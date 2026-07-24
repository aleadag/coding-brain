use coding_brain_core::brain_activity::{ActivityItem, MAX_ACTIVITY_FIELD_BYTES};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
use unicode_width::UnicodeWidthStr;

use crate::brain_app::BrainApp;

use super::live;

const WIDE_BREAKPOINT: u16 = 120;
const MAX_NARROW_EVIDENCE_HEIGHT: u16 = 12;
const MIN_LIST_HEIGHT: u16 = 3;
const STORE_HEALTH_HEIGHT: u16 = 3;
const MAX_EVIDENCE_WIDTH: usize = MAX_ACTIVITY_FIELD_BYTES;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let health_height = store_health_height(app, area.width).min(area.height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(health_height), Constraint::Min(0)])
        .split(area);
    render_store_health(frame, sections[0], app);
    if area.width >= WIDE_BREAKPOINT {
        render_wide(frame, sections[1], app);
    } else {
        render_narrow(frame, sections[1], app);
    }
}

fn render_store_health(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let lines = store_health_lines(app, area.width)
        .into_iter()
        .map(Line::raw)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(app.theme().text_muted))
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .title(" Store integrity ")
                    .borders(Borders::ALL),
            ),
        area,
    );
}

fn store_health_height(app: &BrainApp, width: u16) -> u16 {
    let inner_width = usize::from(width.saturating_sub(2).max(1));
    let content_height = wrapped_content_height(
        store_health_lines(app, width)
            .into_iter()
            .map(Line::raw)
            .collect(),
        inner_width as u16,
    );
    content_height.saturating_add(2).max(STORE_HEALTH_HEIGHT)
}

fn store_health_lines(app: &BrainApp, width: u16) -> Vec<String> {
    let diagnostics = &app.snapshot().diagnostics;
    let fields = [
        format!("malformed rows: {}", diagnostics.malformed_rows),
        format!(
            "duplicate terminals: {}",
            diagnostics.duplicate_terminal_states
        ),
        format!("truncated tails: {}", diagnostics.truncated_tails),
        format!("discarded bytes: {}", diagnostics.discarded_tail_bytes),
    ];
    let inner_width = usize::from(width.saturating_sub(2).max(1));
    let mut lines = Vec::new();
    let mut line = String::new();
    for field in fields {
        let separator_width = if line.is_empty() { 0 } else { 2 };
        let next_width = UnicodeWidthStr::width(line.as_str())
            + separator_width
            + UnicodeWidthStr::width(field.as_str());
        if !line.is_empty() && next_width > inner_width {
            lines.push(line);
            line = field;
        } else {
            if !line.is_empty() {
                line.push_str("  ");
            }
            line.push_str(&field);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn render_wide(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    render_list(frame, columns[0], app);
    render_evidence(frame, columns[1], app);
}

fn render_narrow(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let evidence_height = evidence_height(app, area.width)
        .min(MAX_NARROW_EVIDENCE_HEIGHT)
        .min(area.height.saturating_sub(MIN_LIST_HEIGHT));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(MIN_LIST_HEIGHT),
            Constraint::Length(evidence_height),
        ])
        .split(area);
    render_list(frame, rows[0], app);
    render_evidence(frame, rows[1], app);
}

fn evidence_height(app: &BrainApp, width: u16) -> u16 {
    let inner_width = width.saturating_sub(2).max(1);
    wrapped_content_height(
        evidence_lines(app.selected_diagnostic(), app.theme()),
        inner_width,
    )
    .saturating_add(2)
}

fn render_list(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let events = &app.snapshot().diagnostic_events;
    let row_width = usize::from(area.width.saturating_sub(4));
    let items = if events.is_empty() {
        vec![ListItem::new("No recent diagnostic events")]
    } else {
        events
            .iter()
            .map(|item| ListItem::new(diagnostic_row(item, row_width, app)))
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Recent Diagnostics ({}) ", events.len()))
                .borders(Borders::ALL),
        )
        .highlight_style(Style::default())
        .highlight_symbol("> ")
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = ListState::default();
    if app.selected_diagnostic().is_some() {
        state.select(Some(app.selection()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn diagnostic_row(item: &ActivityItem, width: usize, app: &BrainApp) -> Line<'static> {
    let theme = app.theme();
    let provider = item
        .session
        .as_ref()
        .map(|session| session.provider.label())
        .unwrap_or("Unknown");
    let project = live::safe_row_text(live::project_label(item).as_ref());
    let tool = item.tool.as_deref().unwrap_or("unknown");
    let text = format!(
        "{}  {}  {}",
        live::safe_row_text(provider),
        project,
        live::safe_row_text(tool),
    );
    let text = if UnicodeWidthStr::width(text.as_str()) > width {
        live::truncate_display(&text, width)
    } else {
        text
    };
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(theme.header)
            .add_modifier(Modifier::BOLD),
    ))
}

fn render_evidence(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let lines = evidence_lines(app.selected_diagnostic(), app.theme());
    let content_height = wrapped_content_height(lines.clone(), area.width.saturating_sub(2).max(1));
    let page_size = area.height.saturating_sub(2).max(1);
    let max_scroll = content_height.saturating_sub(page_size);
    app.update_diagnostics_evidence_metrics(page_size, max_scroll);
    let scroll = app.diagnostics_evidence_scroll();
    let indicators = [
        (scroll > 0).then_some("↑ more"),
        (scroll < max_scroll).then_some("↓ more"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("  ");
    let title = if indicators.is_empty() {
        " Evidence ".to_owned()
    } else {
        format!(" Evidence {indicators} ")
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .scroll((scroll, 0))
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn wrapped_content_height(lines: Vec<Line<'static>>, width: u16) -> u16 {
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .line_count(width.max(1))
        .min(usize::from(u16::MAX)) as u16
}

fn evidence_lines(
    item: Option<&ActivityItem>,
    theme: &coding_brain_core::theme::Theme,
) -> Vec<Line<'static>> {
    let Some(item) = item else {
        return vec![Line::raw("No recent diagnostic events")];
    };
    let provider = item
        .session
        .as_ref()
        .map(|session| session.provider.label())
        .unwrap_or("Unknown");
    let session = item
        .session
        .as_ref()
        .map(|session| session.session_id.as_str())
        .unwrap_or("unknown");
    let tool = item.tool.as_deref().unwrap_or("unknown");
    let reason = item.reasoning.as_deref().unwrap_or("unavailable");
    let label_style = Style::default().fg(theme.text_muted);
    [
        ("Activity", bounded(&item.activity_id)),
        ("Recorded", item.recorded_at_ms.to_string()),
        ("Provider", bounded(provider)),
        ("Session", bounded(session)),
        ("Project", bounded(live::project_label(item).as_ref())),
        ("Tool", bounded(tool)),
        ("Reason", bounded(reason)),
    ]
    .into_iter()
    .map(|(label, value)| {
        Line::from(vec![
            Span::styled(format!("{label}: "), label_style),
            Span::styled(value, Style::default().fg(theme.text_primary)),
        ])
    })
    .collect()
}

fn bounded(value: &str) -> String {
    live::truncate_display(&live::safe_evidence_text(value), MAX_EVIDENCE_WIDTH)
}
