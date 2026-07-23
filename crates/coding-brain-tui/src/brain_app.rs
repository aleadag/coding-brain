use std::sync::Arc;
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{
    ActivityItem, ActivitySnapshot, AttentionItem, CorrectionDisposition, SnapshotLimits,
    redact_activity_text,
};
use coding_brain_core::runtime::{
    BrainEffect, BrainGateMode, BrainRuntime, CorrectionInput, EndpointHealth, ReviewItemSummary,
    ScorecardSummary, SessionNavigation,
};
use coding_brain_core::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent};

use crate::terminal_suspend::NavigationOutcome;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_NOTE_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainTab {
    Live,
    Review,
    Scorecard,
}

impl BrainTab {
    fn next(self) -> Self {
        match self {
            Self::Live => Self::Review,
            Self::Review => Self::Scorecard,
            Self::Scorecard => Self::Live,
        }
    }
}

#[derive(Debug, Clone)]
enum BrainInput {
    Correction {
        activity_id: String,
        disposition: Option<CorrectionDisposition>,
        note: String,
    },
    Canonical {
        decision_id: String,
        note: String,
    },
}

pub struct BrainApp {
    runtime: BrainRuntime,
    theme: Theme,
    tab: BrainTab,
    snapshot: ActivitySnapshot,
    review_queue: Vec<ReviewItemSummary>,
    scorecard: ScorecardSummary,
    gate_mode: BrainGateMode,
    endpoint_health: EndpointHealth,
    selection: usize,
    input: Option<BrainInput>,
    status: Option<String>,
    refreshed_at: Instant,
}

impl BrainApp {
    pub fn new(runtime: BrainRuntime, theme: Theme) -> Self {
        let mut app = Self {
            runtime,
            theme,
            tab: BrainTab::Live,
            snapshot: ActivitySnapshot::default(),
            review_queue: Vec::new(),
            scorecard: ScorecardSummary::default(),
            gate_mode: BrainGateMode::On,
            endpoint_health: EndpointHealth::default(),
            selection: 0,
            input: None,
            status: None,
            refreshed_at: Instant::now() - REFRESH_INTERVAL,
        };
        app.refresh();
        app
    }

    pub fn refresh(&mut self) {
        let mut errors = Vec::new();
        let recovery = self.runtime.actions.poll_recovery();
        match self.runtime.source.snapshot(SnapshotLimits::default()) {
            Ok(snapshot) => self.snapshot = snapshot,
            Err(error) => errors.push(format!("Live: {error}")),
        }
        match self.runtime.source.review_queue() {
            Ok(queue) => self.review_queue = queue,
            Err(error) => errors.push(format!("Review: {error}")),
        }
        match self.runtime.source.scorecard() {
            Ok(scorecard) => self.scorecard = scorecard,
            Err(error) => errors.push(format!("Scorecard: {error}")),
        }
        self.gate_mode = self.runtime.source.gate_mode();
        self.endpoint_health = self.runtime.source.endpoint_health();
        self.refreshed_at = Instant::now();
        self.clamp_selection();
        if !errors.is_empty() {
            self.status = Some(errors.join(" · "));
        } else if !recovery.is_empty() {
            self.status = Some(recovery.join(" · "));
        }
    }

    pub fn refresh_if_due(&mut self) {
        if self.refreshed_at.elapsed() >= REFRESH_INTERVAL {
            self.refresh();
        }
    }

    pub fn navigation(&self) -> Arc<dyn SessionNavigation> {
        self.runtime.navigation.clone()
    }

    pub fn complete_navigation(&mut self, result: Result<NavigationOutcome, String>) {
        let tab = self.tab;
        let selection = self.selection;
        self.refresh();
        self.tab = tab;
        self.selection = selection;
        self.clamp_selection();
        self.status = Some(match result {
            Ok(NavigationOutcome::Attached) => "Returned from session".into(),
            Ok(NavigationOutcome::Cancelled {
                restore_error: None,
            }) => "Session switch cancelled".into(),
            Ok(NavigationOutcome::Cancelled {
                restore_error: Some(error),
            }) => format!(
                "Session switch cancelled; terminal restore warning: {}",
                bounded_status(&error)
            ),
            Ok(NavigationOutcome::FocusedFallback) => "Focused session terminal".into(),
            Err(error) => format!("Could not switch session: {}", bounded_status(&error)),
        });
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> Option<BrainEffect> {
        if self.input.is_some() {
            return self.handle_input(event.code);
        }
        match event.code {
            KeyCode::Char('q') => Some(BrainEffect::Exit),
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.selection = 0;
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let len = self.current_len();
                if len > 0 {
                    self.selection = (self.selection + 1).min(len - 1);
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selection = self.selection.saturating_sub(1);
                None
            }
            KeyCode::Char('r') => {
                self.refresh();
                None
            }
            KeyCode::Enter => self.navigation_effect(),
            KeyCode::Char('c') if self.tab == BrainTab::Live => {
                self.begin_correction();
                None
            }
            KeyCode::Char('m') if self.tab == BrainTab::Review => {
                self.mark_selected_canonical(None);
                None
            }
            KeyCode::Char('n') if self.tab == BrainTab::Review => {
                if let Some(item) = self.review_queue.get(self.selection) {
                    self.input = Some(BrainInput::Canonical {
                        decision_id: item.decision.id.clone(),
                        note: String::new(),
                    });
                }
                None
            }
            KeyCode::Char('s') if self.tab == BrainTab::Review => {
                let len = self.review_queue.len();
                if len > 0 {
                    self.selection = (self.selection + 1).min(len - 1);
                }
                None
            }
            _ => None,
        }
    }

    pub fn begin_correction(&mut self) {
        let Some(item) = self.snapshot.attention.get(self.selection) else {
            self.status = Some("Select a Needs Attention item first".into());
            return;
        };
        if item.kind != coding_brain_core::brain_activity::ActivityKind::Decision {
            self.status = Some("Corrections are only available for Decision activity".into());
            return;
        }
        self.input = Some(BrainInput::Correction {
            activity_id: item.activity_id.clone(),
            disposition: None,
            note: String::new(),
        });
    }

    pub fn choose_correction(&mut self, disposition: CorrectionDisposition, note: Option<String>) {
        let activity_id = match &self.input {
            Some(BrainInput::Correction { activity_id, .. }) => activity_id.clone(),
            _ => match self.snapshot.attention.get(self.selection) {
                Some(item) => item.activity_id.clone(),
                None => return,
            },
        };
        let correction = CorrectionInput {
            activity_id,
            disposition,
            note: note.and_then(|note| bounded_note(&note)),
        };
        match self.runtime.actions.record_correction(correction) {
            Ok(()) => {
                self.status = Some("Correction recorded".into());
                self.input = None;
                self.refresh();
            }
            Err(error) => self.status = Some(format!("Could not record correction: {error}")),
        }
    }

    fn handle_input(&mut self, code: KeyCode) -> Option<BrainEffect> {
        match code {
            KeyCode::Esc => self.input = None,
            KeyCode::Backspace => match self.input.as_mut() {
                Some(BrainInput::Correction { note, .. })
                | Some(BrainInput::Canonical { note, .. }) => {
                    note.pop();
                }
                None => {}
            },
            KeyCode::Enter => match self.input.clone() {
                Some(BrainInput::Correction {
                    disposition: Some(disposition),
                    note,
                    ..
                }) => self.choose_correction(disposition, (!note.is_empty()).then_some(note)),
                Some(BrainInput::Correction { .. }) => {
                    self.status = Some("Choose r, w, or e first".into());
                }
                Some(BrainInput::Canonical { decision_id, note }) => {
                    self.mark_canonical(&decision_id, (!note.is_empty()).then_some(note));
                }
                None => {}
            },
            KeyCode::Char(character) => match self.input.as_mut() {
                Some(BrainInput::Correction {
                    disposition, note, ..
                }) if disposition.is_none() => {
                    *disposition = match character {
                        'r' => Some(CorrectionDisposition::BrainRight),
                        'w' => Some(CorrectionDisposition::BrainWrong),
                        'e' => Some(CorrectionDisposition::Exception),
                        _ => None,
                    };
                }
                Some(BrainInput::Correction { note, .. })
                | Some(BrainInput::Canonical { note, .. }) => push_bounded(note, character),
                None => {}
            },
            _ => {}
        }
        None
    }

    fn navigation_effect(&mut self) -> Option<BrainEffect> {
        if self.tab != BrainTab::Live {
            return None;
        }
        match self
            .selected_live_activity()
            .and_then(|item| item.session.clone())
        {
            Some(target) => Some(BrainEffect::SwitchToSession(target)),
            None => {
                self.status = Some("No navigable session for this activity".into());
                None
            }
        }
    }

    fn mark_selected_canonical(&mut self, note: Option<String>) {
        let Some(item) = self.review_queue.get(self.selection) else {
            return;
        };
        self.mark_canonical(&item.decision.id.clone(), note);
    }

    fn mark_canonical(&mut self, decision_id: &str, note: Option<String>) {
        let note = note.and_then(|note| bounded_note(&note));
        match self.runtime.actions.mark_canonical(decision_id, note) {
            Ok(()) => {
                self.status = Some(format!("Marked canonical: {decision_id}"));
                self.input = None;
                self.refresh();
            }
            Err(error) => self.status = Some(format!("Could not mark canonical: {error}")),
        }
    }

    fn current_len(&self) -> usize {
        match self.tab {
            BrainTab::Live => self.snapshot.attention.len() + self.snapshot.recent.len(),
            BrainTab::Review => self.review_queue.len(),
            BrainTab::Scorecard => 0,
        }
    }

    fn clamp_selection(&mut self) {
        self.selection = self.selection.min(self.current_len().saturating_sub(1));
    }

    pub fn selected_live_activity(&self) -> Option<&ActivityItem> {
        self.snapshot
            .attention
            .get(self.selection)
            .map(|item| &item.activity)
            .or_else(|| {
                self.snapshot
                    .recent
                    .get(self.selection.saturating_sub(self.snapshot.attention.len()))
            })
    }

    pub fn tab(&self) -> BrainTab {
        self.tab
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn snapshot(&self) -> &ActivitySnapshot {
        &self.snapshot
    }

    pub fn review_queue(&self) -> &[ReviewItemSummary] {
        &self.review_queue
    }

    pub fn scorecard(&self) -> &ScorecardSummary {
        &self.scorecard
    }

    pub fn gate_mode(&self) -> BrainGateMode {
        self.gate_mode
    }

    pub fn endpoint_health(&self) -> &EndpointHealth {
        &self.endpoint_health
    }

    pub fn selection(&self) -> usize {
        self.selection
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn input_prompt(&self) -> Option<String> {
        match &self.input {
            Some(BrainInput::Correction {
                disposition, note, ..
            }) => Some(match disposition {
                None => "Correction: [r] brain right  [w] brain wrong  [e] exception".into(),
                Some(disposition) => format!("Correction {disposition:?} note: {note}"),
            }),
            Some(BrainInput::Canonical { note, .. }) => Some(format!("Canonical note: {note}")),
            None => None,
        }
    }

    pub fn selected_attention(&self) -> Option<&AttentionItem> {
        self.snapshot.attention.get(self.selection)
    }
}

fn push_bounded(value: &mut String, character: char) {
    if value.chars().count() < MAX_NOTE_CHARS {
        value.push(character);
    }
}

fn bounded_note(note: &str) -> Option<String> {
    let redacted = redact_activity_text(note.trim());
    if redacted.is_empty() {
        return None;
    }
    Some(redacted.chars().take(MAX_NOTE_CHARS).collect())
}

fn bounded_status(status: &str) -> String {
    redact_activity_text(status.trim())
        .chars()
        .take(MAX_NOTE_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use coding_brain_core::brain_activity::{
        ActivityItem, ActivityKind, ActivitySnapshot, ActivityState, AttentionItem,
        CorrectionDisposition, DeliveryState, ProjectEvidence, SessionTarget,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{
        BrainEffect, BrainRuntime, CorrectionInput, DecisionSummary, MockBrainAction,
        MockBrainRuntime, ReviewItemSummary,
    };
    use coding_brain_core::theme::{Theme, ThemeMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    #[test]
    fn defaults_to_live_and_cycles_all_tabs() {
        let (mut app, _) = fixture_app(false);

        assert_eq!(app.tab(), BrainTab::Live);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab(), BrainTab::Review);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab(), BrainTab::Scorecard);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab(), BrainTab::Live);
    }

    #[test]
    fn enter_emits_navigation_without_mutating_decision() {
        let (mut app, mock) = fixture_app(true);

        let effect = app.handle_key(key(KeyCode::Enter));

        assert!(matches!(effect, Some(BrainEffect::SwitchToSession(_))));
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn g_does_not_change_the_read_only_gate_mode() {
        let (mut app, mock) = fixture_app(false);

        app.handle_key(key(KeyCode::Char('g')));

        assert_eq!(app.gate_mode(), BrainGateMode::On);
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn navigation_completion_restores_tab_selection_and_bounded_status() {
        let mock = Arc::new(MockBrainRuntime {
            review_queue: vec![
                ReviewItemSummary {
                    decision: decision(),
                    reason: "first".into(),
                    score: 80.0,
                },
                ReviewItemSummary {
                    decision: DecisionSummary {
                        id: "decision-2".into(),
                        ..decision()
                    },
                    reason: "second".into(),
                    score: 70.0,
                },
            ],
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock);
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));

        app.complete_navigation(Err("x".repeat(700)));

        assert_eq!(app.tab(), BrainTab::Review);
        assert_eq!(app.selection(), 1);
        assert!(app.status().unwrap().chars().count() <= 512 + 26);
    }

    #[test]
    fn correction_records_right_wrong_or_exception() {
        let (mut app, mock) = fixture_app(true);

        for disposition in [
            CorrectionDisposition::BrainRight,
            CorrectionDisposition::BrainWrong,
            CorrectionDisposition::Exception,
        ] {
            app.begin_correction();
            app.choose_correction(disposition, Some("safe fixture".into()));
        }

        assert_eq!(
            non_poll_actions(&mock),
            [
                CorrectionDisposition::BrainRight,
                CorrectionDisposition::BrainWrong,
                CorrectionDisposition::Exception,
            ]
            .into_iter()
            .map(
                |disposition| MockBrainAction::RecordCorrection(CorrectionInput {
                    activity_id: "activity-1".into(),
                    disposition,
                    note: Some("safe fixture".into()),
                })
            )
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn diagnostic_attention_does_not_open_correction_input() {
        let mut diagnostic = activity();
        diagnostic.kind = ActivityKind::Diagnostic;
        diagnostic.state = ActivityState::Error;
        diagnostic.decision_id = None;
        let mock = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: diagnostic,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                recent: Vec::new(),
                unresolved_count: 1,
                diagnostics: Default::default(),
            },
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        app.begin_correction();

        assert_eq!(app.input_prompt(), None);
        assert_eq!(
            app.status(),
            Some("Corrections are only available for Decision activity")
        );
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn review_mark_records_exact_decision_id_without_dashboard_actions() {
        let mock = MockBrainRuntime {
            review_queue: vec![ReviewItemSummary {
                decision: decision(),
                reason: "high-confidence miss".into(),
                score: 80.0,
            }],
            ..MockBrainRuntime::default()
        };
        let mock = Arc::new(mock);
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('m')));

        assert_eq!(
            non_poll_actions(&mock),
            vec![MockBrainAction::MarkCanonical {
                decision_id: "decision-1".into(),
                note: None,
            }]
        );
    }

    fn fixture_app(attention: bool) -> (BrainApp, Arc<MockBrainRuntime>) {
        let mut mock = MockBrainRuntime::default();
        if attention {
            mock.activity_snapshot = ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity(),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                recent: Vec::new(),
                unresolved_count: 1,
                diagnostics: Default::default(),
            };
        }
        let mock = Arc::new(mock);
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
        (app, mock)
    }

    fn non_poll_actions(mock: &MockBrainRuntime) -> Vec<MockBrainAction> {
        mock.actions()
            .into_iter()
            .filter(|action| *action != MockBrainAction::PollRecovery)
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
                cwd: PathBuf::from("/work/project"),
                label: Some("project".into()),
            },
            session: Some(SessionTarget {
                provider: coding_brain_core::provider::AgentProvider::Codex,
                session_id: "session-1".into(),
                turn_id: Some("turn-1".into()),
                tool_use_id: Some("tool-1".into()),
                project_id,
                cwd: PathBuf::from("/work/project"),
                provider_hints: vec!["tmux:brain".into()],
            }),
            state: ActivityState::Denied,
            delivery: DeliveryState::Delivered,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: Some("fixture".into()),
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("fixture".into()),
            decision_id: Some("decision-1".into()),
            outcome: None,
            correction: None,
            note: None,
            tool_execution_confirmed: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn decision() -> DecisionSummary {
        DecisionSummary {
            id: "decision-1".into(),
            timestamp: "1".into(),
            action: "approve".into(),
            confidence: Some(0.9),
            project: Some("project".into()),
            tool: Some("Bash".into()),
            pid: 1,
            command: Some("cargo test".into()),
            reasoning: Some("fixture".into()),
            user_action: Some("accept".into()),
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
}
