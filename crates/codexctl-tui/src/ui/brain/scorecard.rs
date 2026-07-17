use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::brain_app::BrainApp;

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let scorecard = app.scorecard();
    let mut lines = vec![
        Line::raw(format!(
            "Accuracy: {:.1}% ({} / {})",
            scorecard.accuracy_pct, scorecard.correct_decisions, scorecard.brain_decisions
        )),
        Line::raw(format!("Abstentions: {}", scorecard.abstentions)),
        Line::raw(format!(
            "Dangerous false approvals: {} (target 0)",
            scorecard.dangerous_false_approvals
        )),
        Line::raw(format!(
            "Override rate (last 50): {:.1}%",
            scorecard.override_rate_pct
        )),
        Line::raw(format!(
            "Latency: p50 {} ms  p95 {} ms  p99 {} ms",
            scorecard.latency.p50_ms, scorecard.latency.p95_ms, scorecard.latency.p99_ms
        )),
        Line::raw(format!(
            "Cache: {:.1}% ({} / {})",
            scorecard.cache.hit_rate_pct, scorecard.cache.hits, scorecard.cache.instrumented
        )),
        Line::raw(format!(
            "Counterfactual: brain right {}  user right {}",
            scorecard.counterfactuals.brain_was_right, scorecard.counterfactuals.user_was_right
        )),
        Line::raw(format!(
            "Canonical: {} / {}",
            scorecard.canonical_decisions, scorecard.total_decisions
        )),
        Line::raw(""),
        Line::raw("Per-risk-tier accuracy"),
    ];
    for tier in &scorecard.risk_tiers {
        let accuracy = if tier.samples == 0 {
            0.0
        } else {
            tier.correct as f64 / tier.samples as f64 * 100.0
        };
        lines.push(Line::raw(format!(
            "  {:<10} {:>5.1}%  n={}  false approvals={}",
            tier.tier, accuracy, tier.samples, tier.false_approvals
        )));
    }
    if scorecard.total_decisions == 0 {
        lines.push(Line::raw(
            "No decisions yet. Coding Brain will learn as you work.",
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(Block::default().title(" Scorecard ").borders(Borders::ALL)),
        area,
    );
}
