use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};

use crate::brain_app::BrainApp;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Min(8)])
        .split(area);
    let items = if app.review_queue().is_empty() {
        vec![ListItem::new("No review-worthy decisions")]
    } else {
        app.review_queue()
            .iter()
            .map(|item| {
                ListItem::new(format!(
                    "{:.0}  {}  {}  {}",
                    item.score,
                    item.decision.provider.label(),
                    item.reason,
                    item.decision.id
                ))
            })
            .collect()
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Review Queue ({}) ", app.review_queue().len()))
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
    if !app.review_queue().is_empty() {
        state.select(Some(app.selection()));
    }
    frame.render_stateful_widget(list, areas[0], &mut state);

    let lines = app
        .review_queue()
        .get(app.selection())
        .map(|item| {
            vec![
                Line::raw(format!("Decision: {}", item.decision.id)),
                Line::raw(format!("Provider: {}", item.decision.provider.label())),
                Line::raw(format!("Why review: {}", item.reason)),
                Line::raw(format!("Brain: {}", item.decision.action)),
                Line::raw(format!(
                    "Confidence: {:.0}%",
                    item.decision.confidence.unwrap_or(0.0) * 100.0
                )),
                Line::raw(format!(
                    "Command: {}",
                    item.decision.command.as_deref().unwrap_or("no command")
                )),
                Line::raw("Mark canonical: m, or n to attach a note"),
            ]
        })
        .unwrap_or_else(|| vec![Line::raw("The review queue is clear.")]);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title(" Teaching ").borders(Borders::ALL)),
        areas[1],
    );
}
