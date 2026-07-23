use std::sync::Arc;

use coding_brain_core::runtime::{
    BrainEffect, BrainRuntime, DecisionSummary, EndpointHealth, MockBrainAction, MockBrainRuntime,
    ReviewItemSummary,
};
use coding_brain_core::theme::{Theme, ThemeMode};
use coding_brain_tui::brain_app::{BrainApp, BrainTab};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn offline_brain_opens_live_keeps_review_and_exits_cleanly() {
    let mock = Arc::new(MockBrainRuntime {
        endpoint_health: EndpointHealth {
            reachable: false,
            endpoint: None,
            model: Some("local-fixture".into()),
            detail: Some("connection refused".into()),
        },
        review_queue: vec![ReviewItemSummary {
            decision: decision(),
            reason: "high-confidence miss".into(),
            score: 80.0,
        }],
        ..MockBrainRuntime::default()
    });
    let runtime = BrainRuntime::new(mock.clone(), mock.clone());
    let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();

    terminal
        .draw(|frame| coding_brain_tui::ui::brain::render(frame, &app))
        .unwrap();

    assert_eq!(app.tab(), BrainTab::Live);
    assert_eq!(app.review_queue().len(), 1);
    assert!(!app.endpoint_health().reachable);
    assert!(mock.actions().contains(&MockBrainAction::PollRecovery));
    let buffer = terminal.backend().buffer();
    let rendered = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Coding Brain"));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    terminal
        .draw(|frame| coding_brain_tui::ui::brain::render(frame, &app))
        .unwrap();
    let review = (0..terminal.backend().buffer().area.height)
        .map(|y| {
            (0..terminal.backend().buffer().area.width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(review.contains("Claude"));
    assert!(!review.contains("Usage"));
    assert!(!review.contains("Cost"));
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(BrainEffect::Exit)
    );
}

fn decision() -> DecisionSummary {
    DecisionSummary {
        provider: coding_brain_core::provider::AgentProvider::Claude,
        id: "decision-1".into(),
        timestamp: "1".into(),
        action: "deny".into(),
        confidence: Some(0.9),
        project: Some("project".into()),
        tool: Some("Bash".into()),
        pid: 1,
        command: Some("cargo test".into()),
        reasoning: Some("fixture".into()),
        user_action: Some("reject".into()),
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
    }
}
