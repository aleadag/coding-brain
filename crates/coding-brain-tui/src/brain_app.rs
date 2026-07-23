use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{
    ActivityItem, ActivitySnapshot, AttentionItem, CorrectionDisposition, SessionTargetProvenance,
    SnapshotLimits, redact_activity_text,
};
use coding_brain_core::runtime::{
    BrainEffect, BrainGateMode, BrainRuntime, CorrectionInput, EndpointHealth, ReviewItemSummary,
    ScorecardSummary, SessionActionRequest, SessionNavigation,
};
use coding_brain_core::terminals::TerminalSessionAction;
use coding_brain_core::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent};

use crate::terminal_suspend::NavigationOutcome;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_NOTE_CHARS: usize = 512;
const MAX_MANUAL_TEXT_BYTES: usize = 4_096;

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
    SessionAction {
        target: coding_brain_core::brain_activity::SessionTarget,
        text: Option<String>,
    },
}

#[derive(Debug, Clone, Copy)]
struct SessionActionKind {
    label: &'static str,
    manual_bytes: Option<usize>,
}

#[derive(Debug)]
struct SessionActionDelivery {
    kind: SessionActionKind,
    result: Result<(), String>,
}

struct SessionActionWorker {
    receiver: Option<Receiver<SessionActionDelivery>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SessionActionWorker {
    fn new() -> Self {
        Self {
            receiver: None,
            handle: None,
        }
    }

    fn is_in_flight(&self) -> bool {
        self.receiver.is_some()
    }

    fn finish(&mut self) {
        self.receiver = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionActionWorker {
    fn drop(&mut self) {
        self.finish();
    }
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
    session_action_worker: SessionActionWorker,
    pending_action_status: Option<String>,
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
            session_action_worker: SessionActionWorker::new(),
            pending_action_status: None,
            status: None,
            refreshed_at: Instant::now() - REFRESH_INTERVAL,
        };
        app.refresh();
        app
    }

    pub fn refresh(&mut self) {
        self.refresh_state();
    }

    fn refresh_state(&mut self) -> bool {
        let mut errors = Vec::new();
        let recovery = self.runtime.actions.poll_recovery();
        if let Some(status) = self.poll_session_action_delivery() {
            self.pending_action_status = Some(status);
        }
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
        if let Some(status) = self.pending_action_status.take() {
            self.status = Some(status);
            true
        } else if !errors.is_empty() {
            self.status = Some(errors.join(" · "));
            false
        } else if !recovery.is_empty() {
            self.status = Some(recovery.join(" · "));
            false
        } else {
            false
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
        let navigation_status = match result {
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
        };
        let tab = self.tab;
        let selection = self.selection;
        let surfaced_action = self.refresh_state();
        self.tab = tab;
        self.selection = selection;
        self.clamp_selection();
        if !surfaced_action {
            self.status = Some(navigation_status);
        }
    }

    pub fn handle_key(&mut self, event: KeyEvent) -> Option<BrainEffect> {
        if self.session_action_worker.is_in_flight()
            && matches!(event.code, KeyCode::Char('q') | KeyCode::Enter)
        {
            self.status = Some("Session action is still in progress".into());
            return None;
        }
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
            KeyCode::Char('x') if self.tab == BrainTab::Live => {
                self.begin_session_action();
                None
            }
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

    fn begin_session_action(&mut self) {
        if self.session_action_worker.is_in_flight() {
            self.status = Some("A session action is already in progress".into());
            return;
        }
        let Some(item) = self.selected_live_activity() else {
            self.status = Some("No actionable session for this activity".into());
            return;
        };
        let Some(target) = item.session.clone() else {
            self.status = Some("No actionable session for this activity".into());
            return;
        };
        match target.provenance {
            SessionTargetProvenance::Unknown => {
                self.status = Some("Session action authority is unavailable".into());
                return;
            }
            SessionTargetProvenance::RecognizedProcessAttention
                if self.selected_attention().is_none_or(|attention| {
                    attention.rule_id.as_deref() != Some("actionable_prompt_attention")
                }) =>
            {
                self.status =
                    Some("Process-only action requires recognized prompt evidence".into());
                return;
            }
            SessionTargetProvenance::Structured
            | SessionTargetProvenance::RecognizedProcessAttention => {}
        }
        self.input = Some(BrainInput::SessionAction { target, text: None });
    }

    fn dispatch_session_action(
        &mut self,
        target: coding_brain_core::brain_activity::SessionTarget,
        action: TerminalSessionAction,
    ) {
        let kind = match &action {
            TerminalSessionAction::Allow => SessionActionKind {
                label: "allow",
                manual_bytes: None,
            },
            TerminalSessionAction::Deny => SessionActionKind {
                label: "deny",
                manual_bytes: None,
            },
            TerminalSessionAction::Continue => SessionActionKind {
                label: "continue",
                manual_bytes: None,
            },
            TerminalSessionAction::Text(text) => SessionActionKind {
                label: "manual text",
                manual_bytes: Some(text.len()),
            },
        };
        self.input = None;
        if self.session_action_worker.is_in_flight() {
            self.status = Some("A session action is already in progress".into());
            return;
        }
        self.status = Some(format!("Sending {}…", kind.label));
        let actions = Arc::clone(&self.runtime.actions);
        let (sender, receiver) = sync_channel(1);
        self.session_action_worker.receiver = Some(receiver);
        let spawn_result = std::thread::Builder::new()
            .name("coding-brain-session-action".into())
            .spawn(move || {
                let result = actions.send_session_action(SessionActionRequest { target, action });
                let result = match (kind.manual_bytes, result) {
                    (_, Ok(())) => Ok(()),
                    (Some(_), Err(_)) => Err(String::new()),
                    (None, Err(error)) => Err(bounded_status(&error)),
                };
                let _ = sender.send(SessionActionDelivery { kind, result });
            });
        match spawn_result {
            Ok(handle) => self.session_action_worker.handle = Some(handle),
            Err(_) => {
                self.session_action_worker.receiver = None;
                self.status = Some(format!("Could not start {} delivery", kind.label));
            }
        }
    }

    fn poll_session_action_delivery(&mut self) -> Option<String> {
        let result = self.session_action_worker.receiver.as_ref()?.try_recv();
        match result {
            Ok(delivery) => {
                let status = match (delivery.kind.manual_bytes, delivery.result) {
                    (Some(bytes), Ok(())) => format!("Sent manual text ({bytes} bytes)"),
                    (Some(bytes), Err(_)) => {
                        format!("Could not send manual text ({bytes} bytes)")
                    }
                    (None, Ok(())) => format!("Sent {}", delivery.kind.label),
                    (None, Err(error)) => {
                        format!("Could not send {}: {error}", delivery.kind.label)
                    }
                };
                self.session_action_worker.finish();
                Some(status)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.session_action_worker.finish();
                Some("Session action worker stopped unexpectedly".into())
            }
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
                Some(BrainInput::SessionAction {
                    text: Some(text), ..
                }) => {
                    text.pop();
                }
                None => {}
                Some(BrainInput::SessionAction { text: None, .. }) => {}
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
                Some(BrainInput::SessionAction {
                    target,
                    text: Some(text),
                }) if !text.is_empty() => {
                    self.dispatch_session_action(target, TerminalSessionAction::Text(text));
                }
                Some(BrainInput::SessionAction { text: Some(_), .. }) => {
                    self.status = Some("Manual text cannot be empty".into());
                }
                Some(BrainInput::SessionAction { text: None, .. }) => {}
                None => {}
            },
            KeyCode::Char(character) => match self.input.clone() {
                Some(BrainInput::SessionAction { target, text: None }) => match character {
                    'a' => self.dispatch_session_action(target, TerminalSessionAction::Allow),
                    'd' => self.dispatch_session_action(target, TerminalSessionAction::Deny),
                    'c' => self.dispatch_session_action(target, TerminalSessionAction::Continue),
                    't' => {
                        self.input = Some(BrainInput::SessionAction {
                            target,
                            text: Some(String::new()),
                        });
                    }
                    _ => {}
                },
                Some(BrainInput::SessionAction { text: Some(_), .. }) => {
                    if let Some(BrainInput::SessionAction {
                        text: Some(text), ..
                    }) = self.input.as_mut()
                    {
                        push_bounded_bytes(text, character, MAX_MANUAL_TEXT_BYTES);
                    }
                }
                _ => match self.input.as_mut() {
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
                    None | Some(BrainInput::SessionAction { .. }) => {}
                },
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
            Some(BrainInput::SessionAction { text: None, .. }) => {
                Some("Action: [a] allow  [d] deny  [c] continue  [t] manual text".into())
            }
            Some(BrainInput::SessionAction {
                text: Some(text), ..
            }) => Some(format!(
                "Manual text: {} bytes / {MAX_MANUAL_TEXT_BYTES} [hidden]",
                text.len()
            )),
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

fn push_bounded_bytes(value: &mut String, character: char, max_bytes: usize) {
    if value.len() + character.len_utf8() <= max_bytes {
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use coding_brain_core::brain_activity::{
        ActivityItem, ActivityKind, ActivitySnapshot, ActivityState, AttentionItem,
        CorrectionDisposition, DeliveryState, ProjectEvidence, SessionTarget,
        SessionTargetProvenance,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{
        BrainActions, BrainEffect, BrainRuntime, BrainSource, CorrectionInput, DecisionSummary,
        EndpointHealth, MockBrainAction, MockBrainRuntime, ReviewItemSummary, ScorecardSummary,
        SessionActionRequest,
    };
    use coding_brain_core::terminals::TerminalSessionAction;
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
    fn live_action_mode_dispatches_semantic_action_to_exact_target() {
        for (key_code, action) in [
            ('a', TerminalSessionAction::Allow),
            ('d', TerminalSessionAction::Deny),
            ('c', TerminalSessionAction::Continue),
        ] {
            let (mut app, mock) = fixture_app(true);

            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(key(KeyCode::Char(key_code)));
            wait_for_actions(&mut app, &mock, 1);

            assert_eq!(
                non_poll_actions(&mock),
                vec![MockBrainAction::SessionAction(
                    coding_brain_core::runtime::SessionActionRequest {
                        target: activity().session.unwrap(),
                        action,
                    }
                )]
            );
            assert_eq!(app.input_prompt(), None);
        }
    }

    #[test]
    fn live_action_mode_requires_exact_target_and_escape_cancels() {
        let (mut app, mock) = fixture_app(true);
        app.snapshot.attention[0].activity.session = None;

        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.input_prompt(), None);
        assert_eq!(
            app.status(),
            Some("No actionable session for this activity")
        );
        assert!(non_poll_actions(&mock).is_empty());

        app.snapshot.attention[0].activity.session = activity().session;
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.input_prompt(), None);
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn manual_text_is_bounded_hidden_and_dropped_after_failure() {
        let mock = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity(),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            session_action_error: std::sync::Mutex::new(Some(
                "delivery failed for top-secret-literal".into(),
            )),
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('t')));
        for character in "top-secret-literal".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        let prompt = app.input_prompt().unwrap();
        assert!(prompt.contains("18 bytes"));
        assert!(!prompt.contains("top-secret-literal"));
        for _ in 0..5000 {
            app.handle_key(key(KeyCode::Char('x')));
        }
        assert!(app.input_prompt().unwrap().contains("4096 bytes"));

        app.handle_key(key(KeyCode::Enter));
        wait_for_actions(&mut app, &mock, 1);

        assert_eq!(app.input_prompt(), None);
        assert!(!app.status().unwrap().contains("top-secret-literal"));
        let actions = non_poll_actions(&mock);
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            &actions[0],
            MockBrainAction::SessionAction(request)
                if matches!(&request.action, TerminalSessionAction::Text(text) if text.len() == 4096)
        ));
    }

    #[test]
    fn escape_drops_manual_text_without_dispatch() {
        let (mut app, mock) = fixture_app(true);

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('t')));
        for character in "top-secret-literal".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.input_prompt(), None);
        assert!(non_poll_actions(&mock).is_empty());
        assert!(app.status().is_none());
    }

    #[test]
    fn semantic_delivery_failure_is_bounded_status() {
        let mock = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity(),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            session_action_error: std::sync::Mutex::new(Some("x".repeat(700))),
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock);
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('a')));
        wait_for_status(&mut app, "Could not send allow");

        assert!(app.status().unwrap().chars().count() <= MAX_NOTE_CHARS + 22);
        assert_eq!(app.input_prompt(), None);
    }

    #[test]
    fn slow_action_delivery_is_nonblocking_single_flight_and_reports_completion() {
        for (error, expected) in [
            (None, "Sent manual text (18 bytes)"),
            (
                Some("delivery failed for top-secret-literal"),
                "Could not send manual text (18 bytes)",
            ),
        ] {
            let source = MockBrainRuntime {
                activity_snapshot: ActivitySnapshot {
                    attention: vec![AttentionItem {
                        activity: activity(),
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    }],
                    unresolved_count: 1,
                    ..ActivitySnapshot::default()
                },
                ..MockBrainRuntime::default()
            };
            let source = Arc::new(source);
            let actions = Arc::new(SlowBrainActions {
                error,
                calls: AtomicUsize::new(0),
                completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                delay: Duration::from_millis(250),
            });
            let runtime = BrainRuntime::new(source, actions.clone());
            let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

            app.handle_key(key(KeyCode::Char('x')));
            app.handle_key(key(KeyCode::Char('t')));
            for character in "top-secret-literal".chars() {
                app.handle_key(key(KeyCode::Char(character)));
            }
            let started = Instant::now();
            app.handle_key(key(KeyCode::Enter));

            assert!(started.elapsed() < Duration::from_millis(100));
            assert_eq!(app.input_prompt(), None);
            app.handle_key(key(KeyCode::Char('x')));
            assert_eq!(
                app.status(),
                Some("A session action is already in progress")
            );
            wait_for_status(&mut app, expected);
            assert_eq!(actions.calls.load(Ordering::SeqCst), 1);
            assert!(!app.status().unwrap().contains("top-secret-literal"));
        }
    }

    #[test]
    fn in_flight_action_blocks_exit_and_navigation_until_completion() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut app, actions) = slow_fixture_app(Duration::from_millis(150), completed.clone());
        dispatch_allow(&mut app);

        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
        assert_eq!(app.status(), Some("Session action is still in progress"));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.status(), Some("Session action is still in progress"));

        wait_for_status(&mut app, "Sent allow");
        assert!(completed.load(Ordering::SeqCst));
        assert_eq!(actions.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            app.handle_key(key(KeyCode::Char('q'))),
            Some(BrainEffect::Exit)
        );
    }

    #[test]
    fn app_drop_joins_in_flight_action_worker() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut app, _) = slow_fixture_app(Duration::from_millis(100), completed.clone());
        dispatch_allow(&mut app);

        drop(app);

        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn completed_action_outcome_has_priority_over_source_error_once() {
        let source = Arc::new(ErrorAfterFirstSource {
            snapshot_calls: AtomicUsize::new(0),
        });
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let actions = Arc::new(SlowBrainActions {
            error: None,
            calls: AtomicUsize::new(0),
            completed,
            delay: Duration::from_millis(50),
        });
        let runtime = BrainRuntime::new(source, actions);
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
        dispatch_allow(&mut app);
        std::thread::sleep(Duration::from_millis(100));

        app.refresh();
        assert_eq!(app.status(), Some("Sent allow"));
        app.refresh();
        assert_eq!(app.status(), Some("Live: source failed"));
    }

    #[test]
    fn navigation_completion_does_not_overwrite_completed_action_outcome() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut app, _) = slow_fixture_app(Duration::from_millis(50), completed);
        dispatch_allow(&mut app);
        std::thread::sleep(Duration::from_millis(100));

        app.complete_navigation(Ok(NavigationOutcome::Attached));
        assert_eq!(app.status(), Some("Sent allow"));
        app.complete_navigation(Ok(NavigationOutcome::Attached));
        assert_eq!(app.status(), Some("Returned from session"));
    }

    #[test]
    fn refresh_polls_recovery_once_without_exposing_session_collections() {
        let (mut app, mock) = fixture_app(false);
        let before = mock
            .actions()
            .into_iter()
            .filter(|action| *action == MockBrainAction::PollRecovery)
            .count();

        app.refresh();

        let after = mock
            .actions()
            .into_iter()
            .filter(|action| *action == MockBrainAction::PollRecovery)
            .count();
        assert_eq!(after, before + 1);
    }

    #[test]
    fn action_mode_is_live_only_and_correction_key_is_unchanged() {
        let (mut app, mock) = fixture_app(true);

        app.handle_key(key(KeyCode::Char('c')));
        assert!(app.input_prompt().unwrap().starts_with("Correction:"));
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.input_prompt(), None);
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn process_only_action_requires_recognized_attention_row() {
        let (mut app, mock) = fixture_app(true);
        let target = app.snapshot.attention[0].activity.session.as_mut().unwrap();
        target.session_id = "process:7:9:4:pts0".into();
        target.provenance = SessionTargetProvenance::RecognizedProcessAttention;

        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input_prompt(), None);

        app.snapshot.attention[0].activity.rule_id = Some("actionable_prompt_attention".into());
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.input_prompt().is_some());
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn opaque_native_prefixes_do_not_define_process_authority() {
        for session_id in ["live:opaque-native", "process:opaque-native"] {
            let (mut app, mock) = fixture_app(true);
            let target = app.snapshot.attention[0].activity.session.as_mut().unwrap();
            target.session_id = session_id.into();
            target.provenance = SessionTargetProvenance::Structured;

            app.handle_key(key(KeyCode::Char('x')));

            assert!(app.input_prompt().is_some(), "rejected native {session_id}");
            assert!(non_poll_actions(&mock).is_empty());
        }
    }

    #[test]
    fn unknown_target_provenance_fails_closed() {
        let (mut app, mock) = fixture_app(true);
        app.snapshot.attention[0]
            .activity
            .session
            .as_mut()
            .unwrap()
            .provenance = SessionTargetProvenance::Unknown;

        app.handle_key(key(KeyCode::Char('x')));

        assert_eq!(app.input_prompt(), None);
        assert_eq!(
            app.status(),
            Some("Session action authority is unavailable")
        );
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

    fn wait_for_actions(app: &mut BrainApp, mock: &MockBrainRuntime, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while non_poll_actions(mock).len() < count && Instant::now() < deadline {
            app.refresh();
            std::thread::sleep(Duration::from_millis(5));
        }
        app.refresh();
    }

    fn wait_for_status(app: &mut BrainApp, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !app
            .status()
            .is_some_and(|status| status.starts_with(expected))
            && Instant::now() < deadline
        {
            app.refresh();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            app.status()
                .is_some_and(|status| status.starts_with(expected)),
            "expected status prefix {expected:?}, got {:?}",
            app.status()
        );
    }

    struct SlowBrainActions {
        error: Option<&'static str>,
        calls: AtomicUsize,
        completed: Arc<std::sync::atomic::AtomicBool>,
        delay: Duration,
    }

    impl BrainActions for SlowBrainActions {
        fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
            Ok(())
        }

        fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
            Ok(())
        }

        fn send_session_action(&self, _request: SessionActionRequest) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.completed.store(true, Ordering::SeqCst);
            self.error.map_or(Ok(()), |error| Err(error.into()))
        }
    }

    struct ErrorAfterFirstSource {
        snapshot_calls: AtomicUsize,
    }

    impl BrainSource for ErrorAfterFirstSource {
        fn snapshot(&self, _limits: SnapshotLimits) -> Result<ActivitySnapshot, String> {
            if self.snapshot_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(ActivitySnapshot {
                    attention: vec![AttentionItem {
                        activity: activity(),
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    }],
                    unresolved_count: 1,
                    ..ActivitySnapshot::default()
                })
            } else {
                Err("source failed".into())
            }
        }

        fn review_queue(&self) -> Result<Vec<ReviewItemSummary>, String> {
            Ok(Vec::new())
        }

        fn scorecard(&self) -> Result<ScorecardSummary, String> {
            Ok(ScorecardSummary::default())
        }

        fn gate_mode(&self) -> BrainGateMode {
            BrainGateMode::On
        }

        fn endpoint_health(&self) -> EndpointHealth {
            EndpointHealth::default()
        }
    }

    fn slow_fixture_app(
        delay: Duration,
        completed: Arc<std::sync::atomic::AtomicBool>,
    ) -> (BrainApp, Arc<SlowBrainActions>) {
        let source = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: activity(),
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            ..MockBrainRuntime::default()
        });
        let actions = Arc::new(SlowBrainActions {
            error: None,
            calls: AtomicUsize::new(0),
            completed,
            delay,
        });
        let runtime = BrainRuntime::new(source, actions.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            actions,
        )
    }

    fn dispatch_allow(app: &mut BrainApp) {
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Char('a')));
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
                provenance: SessionTargetProvenance::Structured,
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
            provider: coding_brain_core::provider::AgentProvider::Codex,
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
