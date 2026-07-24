use std::cell::Cell;
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
    Diagnostics,
}

impl BrainTab {
    fn next(self) -> Self {
        match self {
            Self::Live => Self::Review,
            Self::Review => Self::Scorecard,
            Self::Scorecard => Self::Diagnostics,
            Self::Diagnostics => Self::Live,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveList {
    Attention,
    Recent,
}

#[derive(Debug, Default)]
struct EvidenceViewport {
    activity_id: Option<String>,
    scroll: Cell<u16>,
    page_size: Cell<u16>,
    max_scroll: Cell<u16>,
}

impl EvidenceViewport {
    fn reset(&self) {
        self.scroll.set(0);
    }

    fn page_down(&self) {
        self.scroll.set(
            self.scroll
                .get()
                .saturating_add(self.page_size.get().max(1))
                .min(self.max_scroll.get()),
        );
    }

    fn page_up(&self) {
        self.scroll.set(
            self.scroll
                .get()
                .saturating_sub(self.page_size.get().max(1)),
        );
    }

    fn update_metrics(&self, page_size: u16, max_scroll: u16) {
        self.page_size.set(page_size.max(1));
        self.max_scroll.set(max_scroll);
        self.scroll.set(self.scroll.get().min(max_scroll));
    }

    fn reset_if_selection_changed(&mut self, selected: Option<&str>) {
        if selected != self.activity_id.as_deref() {
            self.reset();
            self.activity_id = selected.map(str::to_owned);
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
    live_list: LiveList,
    live_attention_selection: usize,
    live_recent_selection: usize,
    live_evidence: EvidenceViewport,
    diagnostics_evidence: EvidenceViewport,
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
            live_list: LiveList::Attention,
            live_attention_selection: 0,
            live_recent_selection: 0,
            live_evidence: EvidenceViewport::default(),
            diagnostics_evidence: EvidenceViewport::default(),
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
                self.reset_live_evidence_scroll();
                self.diagnostics_evidence.reset();
                None
            }
            KeyCode::PageDown if self.tab == BrainTab::Live => {
                self.live_evidence.page_down();
                None
            }
            KeyCode::PageUp if self.tab == BrainTab::Live => {
                self.live_evidence.page_up();
                None
            }
            KeyCode::PageDown if self.tab == BrainTab::Diagnostics => {
                self.diagnostics_evidence.page_down();
                None
            }
            KeyCode::PageUp if self.tab == BrainTab::Diagnostics => {
                self.diagnostics_evidence.page_up();
                None
            }
            KeyCode::Char('J') if self.tab == BrainTab::Live => {
                self.jump_live_list(LiveList::Recent);
                None
            }
            KeyCode::Char('K') if self.tab == BrainTab::Live => {
                self.jump_live_list(LiveList::Attention);
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.tab == BrainTab::Live {
                    self.move_live_selection_down();
                } else {
                    let len = self.current_len();
                    if len > 0 {
                        self.selection = (self.selection + 1).min(len - 1);
                    }
                    if self.tab == BrainTab::Diagnostics {
                        self.reset_diagnostics_evidence_scroll_if_selection_changed();
                    }
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.tab == BrainTab::Live {
                    self.move_live_selection_up();
                } else {
                    self.selection = self.selection.saturating_sub(1);
                    if self.tab == BrainTab::Diagnostics {
                        self.reset_diagnostics_evidence_scroll_if_selection_changed();
                    }
                }
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
        let Some(item) = self.selected_live_activity() else {
            self.status = Some("Select a Live activity first".into());
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
        let Some(BrainInput::Correction { activity_id, .. }) = &self.input else {
            self.status = Some("No correction in progress".into());
            return;
        };
        let correction = CorrectionInput {
            activity_id: activity_id.clone(),
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
            BrainTab::Live => self.live_len(self.live_list),
            BrainTab::Review => self.review_queue.len(),
            BrainTab::Scorecard => 0,
            BrainTab::Diagnostics => self.snapshot.diagnostic_events.len(),
        }
    }

    fn live_len(&self, list: LiveList) -> usize {
        match list {
            LiveList::Attention => self.snapshot.attention.len(),
            LiveList::Recent => self.snapshot.recent.len(),
        }
    }

    fn live_selection(&self, list: LiveList) -> usize {
        match list {
            LiveList::Attention => self.live_attention_selection,
            LiveList::Recent => self.live_recent_selection,
        }
    }

    fn live_selection_mut(&mut self, list: LiveList) -> &mut usize {
        match list {
            LiveList::Attention => &mut self.live_attention_selection,
            LiveList::Recent => &mut self.live_recent_selection,
        }
    }

    fn move_live_selection_down(&mut self) {
        let len = self.live_len(self.live_list);
        if len > 0 {
            let next = (self.live_selection(self.live_list) + 1).min(len - 1);
            *self.live_selection_mut(self.live_list) = next;
            self.reset_live_evidence_scroll_if_selection_changed();
        }
    }

    fn move_live_selection_up(&mut self) {
        let current = self.live_selection(self.live_list);
        *self.live_selection_mut(self.live_list) = current.saturating_sub(1);
        self.reset_live_evidence_scroll_if_selection_changed();
    }

    fn jump_live_list(&mut self, target: LiveList) {
        let len = self.live_len(target);
        if len > 0 {
            let clamped = self.live_selection(target).min(len - 1);
            *self.live_selection_mut(target) = clamped;
            self.live_list = target;
            self.reset_live_evidence_scroll_if_selection_changed();
        }
    }

    fn clamp_live_selection(&mut self) {
        let attention_len = self.live_len(LiveList::Attention);
        let recent_len = self.live_len(LiveList::Recent);
        self.live_attention_selection = self
            .live_attention_selection
            .min(attention_len.saturating_sub(1));
        self.live_recent_selection = self.live_recent_selection.min(recent_len.saturating_sub(1));
        self.live_list = match (self.live_list, attention_len, recent_len) {
            (_, 0, 0) => LiveList::Attention,
            (LiveList::Attention, 0, _) => LiveList::Recent,
            (LiveList::Recent, _, 0) => LiveList::Attention,
            (list, _, _) => list,
        };
    }

    fn clamp_selection(&mut self) {
        self.clamp_live_selection();
        self.selection = self.selection.min(self.current_len().saturating_sub(1));
        self.reset_live_evidence_scroll_if_selection_changed();
        self.reset_diagnostics_evidence_scroll_if_selection_changed();
    }

    fn reset_live_evidence_scroll(&self) {
        self.live_evidence.reset();
    }

    fn reset_live_evidence_scroll_if_selection_changed(&mut self) {
        let selected = self
            .selected_live_activity()
            .map(|item| item.activity_id.clone());
        self.live_evidence
            .reset_if_selection_changed(selected.as_deref());
    }

    fn reset_diagnostics_evidence_scroll_if_selection_changed(&mut self) {
        let selected = self
            .selected_diagnostic()
            .map(|item| item.activity_id.clone());
        self.diagnostics_evidence
            .reset_if_selection_changed(selected.as_deref());
    }

    pub fn selected_live_activity(&self) -> Option<&ActivityItem> {
        match self.live_list {
            LiveList::Attention => self
                .snapshot
                .attention
                .get(self.live_attention_selection)
                .map(|item| &item.activity),
            LiveList::Recent => self.snapshot.recent.get(self.live_recent_selection),
        }
    }

    pub fn selected_attention_index(&self) -> Option<usize> {
        (self.live_list == LiveList::Attention)
            .then_some(self.live_attention_selection)
            .filter(|index| *index < self.snapshot.attention.len())
    }

    pub fn selected_recent_index(&self) -> Option<usize> {
        (self.live_list == LiveList::Recent)
            .then_some(self.live_recent_selection)
            .filter(|index| *index < self.snapshot.recent.len())
    }

    pub(crate) fn selected_live_is_attention(&self) -> bool {
        self.live_list == LiveList::Attention
    }

    pub(crate) fn live_evidence_scroll(&self) -> u16 {
        self.live_evidence.scroll.get()
    }

    pub(crate) fn update_live_evidence_metrics(&self, page_size: u16, max_scroll: u16) {
        self.live_evidence.update_metrics(page_size, max_scroll);
    }

    pub fn selected_diagnostic(&self) -> Option<&ActivityItem> {
        if self.tab == BrainTab::Diagnostics {
            self.snapshot.diagnostic_events.get(self.selection)
        } else {
            None
        }
    }

    pub(crate) fn diagnostics_evidence_scroll(&self) -> u16 {
        self.diagnostics_evidence.scroll.get()
    }

    pub(crate) fn update_diagnostics_evidence_metrics(&self, page_size: u16, max_scroll: u16) {
        self.diagnostics_evidence
            .update_metrics(page_size, max_scroll);
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
        if self.tab == BrainTab::Live {
            self.live_selection(self.live_list)
        } else {
            self.selection
        }
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
        self.selected_attention_index()
            .and_then(|index| self.snapshot.attention.get(index))
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
        assert_eq!(app.tab(), BrainTab::Diagnostics);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.tab(), BrainTab::Live);
    }

    #[test]
    fn diagnostics_selection_is_bounded_and_read_only() {
        let (mut app, _) = fixture_app(false);
        app.snapshot.diagnostic_events = vec![
            diagnostic_activity("diagnostic-1", 200),
            diagnostic_activity("diagnostic-2", 100),
        ];

        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(app.tab(), BrainTab::Diagnostics);
        assert_eq!(
            app.selected_diagnostic().unwrap().activity_id,
            "diagnostic-1"
        );

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected_diagnostic().unwrap().activity_id,
            "diagnostic-2"
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected_diagnostic().unwrap().activity_id,
            "diagnostic-2"
        );
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(
            app.selected_diagnostic().unwrap().activity_id,
            "diagnostic-1"
        );
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    }

    #[test]
    fn diagnostics_evidence_page_keys_use_viewport_and_reset() {
        let (mut app, _) = fixture_app(false);
        app.snapshot.diagnostic_events = vec![
            diagnostic_activity("diagnostic-1", 200),
            diagnostic_activity("diagnostic-2", 100),
        ];
        app.update_live_evidence_metrics(5, 12);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.live_evidence_scroll(), 5);

        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.update_diagnostics_evidence_metrics(5, 12);
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.diagnostics_evidence_scroll(), 12);
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.diagnostics_evidence_scroll(), 7);

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.diagnostics_evidence_scroll(), 0);
        assert_eq!(app.live_evidence_scroll(), 0);

        app.update_diagnostics_evidence_metrics(5, 12);
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.diagnostics_evidence_scroll(), 0);
        assert_eq!(app.live_evidence_scroll(), 0);
    }

    #[test]
    fn refresh_removing_selected_diagnostic_clamps_selection_and_resets_evidence() {
        let (mut app, _) = fixture_app(false);
        app.snapshot.diagnostic_events = vec![
            diagnostic_activity("diagnostic-1", 200),
            diagnostic_activity("diagnostic-2", 100),
        ];
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        app.handle_key(key(KeyCode::Char('j')));
        app.update_diagnostics_evidence_metrics(5, 12);
        app.handle_key(key(KeyCode::PageDown));

        app.snapshot.diagnostic_events = vec![diagnostic_activity("diagnostic-1", 200)];
        app.clamp_selection();

        assert_eq!(
            app.selected_diagnostic().unwrap().activity_id,
            "diagnostic-1"
        );
        assert_eq!(app.diagnostics_evidence_scroll(), 0);
    }

    #[test]
    fn live_moves_within_lists_and_restores_each_list_selection() {
        let (mut app, _) = fixture_app(true);
        let mut second_attention = activity();
        second_attention.activity_id = "attention-2".into();
        app.snapshot.attention.push(AttentionItem {
            activity: second_attention,
            occurrences: 1,
            unresolved_occurrences: 1,
        });
        let mut recent_1 = activity();
        recent_1.activity_id = "recent-1".into();
        let mut recent_2 = activity();
        recent_2.activity_id = "recent-2".into();
        app.snapshot.recent = vec![recent_1, recent_2];
        app.clamp_selection();

        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "attention-2"
        );
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "attention-2"
        );

        app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "recent-1"
        );
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "recent-2"
        );

        app.handle_key(key(KeyCode::Char('K')));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "attention-2"
        );
        app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "recent-2"
        );
    }

    #[test]
    fn live_evidence_page_keys_use_viewport_and_clamp() {
        let (mut app, _) = fixture_app(true);
        app.update_live_evidence_metrics(5, 12);

        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.live_evidence_scroll(), 5);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.live_evidence_scroll(), 10);
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.live_evidence_scroll(), 12);

        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.live_evidence_scroll(), 7);
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.live_evidence_scroll(), 2);
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.live_evidence_scroll(), 0);
    }

    #[test]
    fn live_evidence_scroll_resets_when_selection_changes() {
        let (mut app, _) = fixture_app(true);
        let mut second_attention = activity();
        second_attention.activity_id = "attention-2".into();
        app.snapshot.attention.push(AttentionItem {
            activity: second_attention,
            occurrences: 1,
            unresolved_occurrences: 1,
        });
        let mut recent = activity();
        recent.activity_id = "recent-1".into();
        app.snapshot.recent.push(recent);
        app.clamp_selection();

        app.update_live_evidence_metrics(5, 20);
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.live_evidence_scroll(), 0);

        app.update_live_evidence_metrics(5, 20);
        app.handle_key(key(KeyCode::PageDown));
        app.handle_key(key(KeyCode::Char('J')));
        assert_eq!(app.live_evidence_scroll(), 0);

        app.update_live_evidence_metrics(5, 20);
        app.handle_key(key(KeyCode::PageDown));
        app.snapshot.recent.clear();
        app.clamp_selection();
        assert_eq!(app.live_evidence_scroll(), 0);
    }

    #[test]
    fn live_empty_jump_target_keeps_the_visible_selection() {
        let (mut app, _) = fixture_app(true);
        let selected = app.selected_live_activity().unwrap().activity_id.clone();

        app.handle_key(key(KeyCode::Char('J')));

        assert_eq!(app.selected_live_activity().unwrap().activity_id, selected);
        assert_eq!(app.selected_attention_index(), Some(0));
        assert_eq!(app.selected_recent_index(), None);
    }

    #[test]
    fn live_clamps_remembered_rows_and_falls_back_from_an_empty_active_list() {
        let (mut app, _) = fixture_app(true);
        let mut recent = activity();
        recent.activity_id = "recent-1".into();
        app.snapshot.recent = vec![recent];
        app.handle_key(key(KeyCode::Char('J')));
        app.snapshot.recent.clear();

        app.clamp_selection();

        assert_eq!(app.selected_attention_index(), Some(0));
        assert_eq!(app.selected_recent_index(), None);
        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            app.snapshot.attention[0].activity_id
        );
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
    fn recent_decision_correction_records_exact_activity_for_every_disposition() {
        for (key_code, disposition) in [
            ('r', CorrectionDisposition::BrainRight),
            ('w', CorrectionDisposition::BrainWrong),
            ('e', CorrectionDisposition::Exception),
        ] {
            let (mut app, mock) = fixture_app(false);
            let mut recent = activity();
            recent.activity_id = "recent-decision".into();
            app.snapshot.recent = vec![recent];
            app.clamp_selection();

            app.handle_key(key(KeyCode::Char('c')));
            assert!(app.input_prompt().unwrap().starts_with("Correction:"));
            app.handle_key(key(KeyCode::Char(key_code)));
            app.handle_key(key(KeyCode::Enter));

            assert_eq!(
                non_poll_actions(&mock),
                vec![MockBrainAction::RecordCorrection(CorrectionInput {
                    activity_id: "recent-decision".into(),
                    disposition,
                    note: None,
                })]
            );
        }
    }

    #[test]
    fn correction_submission_without_prompt_fails_closed() {
        let (mut app, mock) = fixture_app(true);

        app.choose_correction(CorrectionDisposition::BrainWrong, None);

        assert_eq!(app.status(), Some("No correction in progress"));
        assert!(non_poll_actions(&mock).is_empty());
    }

    #[test]
    fn diagnostic_recent_does_not_open_correction_input() {
        let (mut app, mock) = fixture_app(false);
        app.snapshot.recent = vec![diagnostic_activity("diagnostic-recent", 1)];
        app.clamp_selection();

        app.handle_key(key(KeyCode::Char('c')));

        assert_eq!(app.input_prompt(), None);
        assert_eq!(
            app.status(),
            Some("Corrections are only available for Decision activity")
        );
        assert!(non_poll_actions(&mock).is_empty());
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
                diagnostic_events: Vec::new(),
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
                diagnostic_events: Vec::new(),
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

    fn diagnostic_activity(id: &str, recorded_at_ms: u64) -> ActivityItem {
        let mut item = activity();
        item.activity_id = id.into();
        item.kind = ActivityKind::Diagnostic;
        item.recorded_at_ms = recorded_at_ms;
        item.state = ActivityState::Error;
        item.delivery = DeliveryState::NotApplicable;
        item.tool = None;
        item.normalized_command = None;
        item.reasoning = Some("orphan outcome: Bash command is not losslessly correlatable".into());
        item.decision_id = None;
        item.outcome = None;
        item.correction = None;
        item.note = None;
        item
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
