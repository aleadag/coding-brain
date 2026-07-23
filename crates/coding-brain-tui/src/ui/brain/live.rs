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

use crate::brain_app::BrainApp;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(38),
            Constraint::Percentage(25),
            Constraint::Min(7),
        ])
        .split(area);
    render_attention(frame, areas[0], app);
    render_recent(frame, areas[1], app);
    render_detail(frame, areas[2], app);
}

fn render_attention(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let snapshot = app.snapshot();
    let mut items = snapshot
        .attention
        .iter()
        .map(|item| {
            let occurrences = if item.occurrences > 1 {
                format!(" x{}", item.occurrences)
            } else {
                String::new()
            };
            ListItem::new(format!(
                "{}  {}  {}  {}{}",
                activity_status(&item.activity),
                provider_label(&item.activity),
                project_label(&item.activity),
                command_label(&item.activity),
                occurrences
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
    if !snapshot.attention.is_empty() && app.selection() < snapshot.attention.len() {
        state.select(Some(app.selection()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_recent(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let snapshot = app.snapshot();
    let items = if snapshot.recent.is_empty() {
        vec![ListItem::new("No recent resolved activity")]
    } else {
        snapshot
            .recent
            .iter()
            .map(|item| {
                ListItem::new(format!(
                    "{}  {}  {}  {}",
                    activity_status(item),
                    provider_label(item),
                    project_label(item),
                    command_label(item)
                ))
            })
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
    if app.selection() >= snapshot.attention.len() && !snapshot.recent.is_empty() {
        state.select(Some(app.selection() - snapshot.attention.len()));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_detail(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let Some(item) = app.selected_live_activity() else {
        frame.render_widget(
            Paragraph::new("Select an activity to inspect its evidence")
                .block(Block::default().title(" Decision ").borders(Borders::ALL)),
            area,
        );
        return;
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(activity_status(item)),
        ]),
        Line::raw(format!("Activity: {}", item.activity_id)),
        Line::raw(format!("Provider: {}", provider_label(item))),
        Line::raw(format!("Project: {}", project_label(item))),
        Line::raw(format!("Command: {}", command_label(item))),
    ];
    if let Some(confidence) = item.confidence {
        lines.push(Line::raw(format!("Confidence: {:.0}%", confidence * 100.0)));
    }
    if let Some(reasoning) = &item.reasoning {
        lines.push(Line::raw(format!("Reason: {reasoning}")));
    }
    if let Some(correction) = item.correction {
        lines.push(Line::raw(format!("Resolved: {correction:?}")));
    }
    if let Some(note) = &item.note {
        lines.push(Line::raw(format!("Note: {note}")));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title(" Decision ").borders(Borders::ALL)),
        area,
    );
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
    if matches!(
        (item.state, item.delivery),
        (ActivityState::Denied, DeliveryState::Delivered)
    ) {
        return "blocked · command did not execute".into();
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
            format!("{} · response delivered", decision_state(item.state))
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
