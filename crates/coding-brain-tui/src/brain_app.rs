use std::cell::Cell;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{
    ActivityItem, ActivitySnapshot, AttentionItem, CorrectionDisposition, SessionTargetProvenance,
    SnapshotLimits, redact_activity_text,
};
use coding_brain_core::runtime::{
    BrainEffect, BrainGateMode, BrainRuntime, BrainSourceError, CorrectionInput, EndpointHealth,
    ReviewItemSummary, ScorecardSummary, SessionActionAttempt, SessionActionAvailability,
    SessionActionCapability, SessionActionFailure, SessionActionPreflightRequest,
    SessionActionRequest, SessionActionTarget, SessionNavigation,
};
use coding_brain_core::terminals::TerminalSessionAction;
use coding_brain_core::theme::Theme;
use crossterm::event::{KeyCode, KeyEvent};

use crate::terminal_suspend::NavigationOutcome;

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_NOTE_CHARS: usize = 512;
const MAX_MANUAL_TEXT_BYTES: usize = 4_096;
const BUSY_RETRYING_STATUS: &str = "Brain data busy; retrying";
const BUSY_STALE_STATUS: &str = "Brain data busy; showing previous refresh";

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
        attempt: SessionActionAttempt,
        capabilities: Vec<SessionActionCapability>,
        text: Option<String>,
    },
}

#[derive(Debug, Clone, Copy)]
struct SessionActionKind {
    label: &'static str,
    manual_bytes: Option<usize>,
}

#[derive(Debug)]
enum SessionActionWorkerResult {
    Preflight(Result<SessionActionAvailability, SessionActionFailure>),
    Delivery {
        kind: SessionActionKind,
        result: Result<(), SessionActionFailure>,
    },
}

struct SessionActionWorker {
    receiver: Option<Receiver<SessionActionWorkerResult>>,
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
    has_successful_refresh: bool,
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
            has_successful_refresh: false,
            refreshed_at: Instant::now() - REFRESH_INTERVAL,
        };
        app.refresh();
        app
    }

    pub fn refresh(&mut self) {
        self.refresh_state();
    }

    fn refresh_state(&mut self) -> bool {
        let recovery = self.runtime.actions.poll_recovery();
        if let Some(status) = self.poll_session_action_worker() {
            self.pending_action_status = Some(status);
        }
        let mut source_error = None;
        let mut busy_status = None;
        match self.runtime.source.refresh(SnapshotLimits::default()) {
            Ok(refresh) => {
                let selected_recent_activity_id = self
                    .snapshot
                    .recent
                    .get(self.live_recent_selection)
                    .map(|item| item.activity_id.clone());
                self.snapshot = refresh.snapshot;
                if let Some(selected_recent_activity_id) = selected_recent_activity_id
                    && let Some(index) = self
                        .snapshot
                        .recent
                        .iter()
                        .position(|item| item.activity_id == selected_recent_activity_id)
                {
                    self.live_recent_selection = index;
                }
                self.review_queue = refresh.review_queue;
                self.scorecard = refresh.scorecard;
                self.has_successful_refresh = true;
                if matches!(
                    self.status.as_deref(),
                    Some(BUSY_RETRYING_STATUS | BUSY_STALE_STATUS)
                ) {
                    self.status = None;
                }
            }
            Err(BrainSourceError::Other(error)) => {
                source_error = Some(format!("Brain: {}", bounded_status(&error)));
            }
            Err(BrainSourceError::Busy) => {
                busy_status = Some(if self.has_successful_refresh {
                    BUSY_STALE_STATUS
                } else {
                    BUSY_RETRYING_STATUS
                });
            }
        }
        self.gate_mode = self.runtime.source.gate_mode();
        self.endpoint_health = self.runtime.source.endpoint_health();
        self.refreshed_at = Instant::now();
        self.clamp_selection();
        if self.discard_stale_session_action_input() {
            self.pending_action_status
                .get_or_insert_with(|| "Selection changed; action cancelled".into());
        }
        if let Some(status) = self.pending_action_status.take() {
            self.status = Some(status);
            true
        } else if let Some(error) = source_error {
            self.status = Some(error);
            false
        } else if !recovery.is_empty() {
            self.status = Some(recovery.join(" · "));
            false
        } else if let Some(status) = busy_status
            && self
                .status
                .as_deref()
                .is_none_or(|current| matches!(current, BUSY_RETRYING_STATUS | BUSY_STALE_STATUS))
        {
            self.status = Some(status.into());
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
        let request = SessionActionPreflightRequest::new(target);
        let actions = Arc::clone(&self.runtime.actions);
        let (sender, receiver) = sync_channel(1);
        self.status = Some("Checking available actions…".into());
        self.session_action_worker.receiver = Some(receiver);
        let spawn_result = std::thread::Builder::new()
            .name("coding-brain-session-action".into())
            .spawn(move || {
                let result = actions.preflight_session_action(request);
                let _ = sender.send(SessionActionWorkerResult::Preflight(result));
            });
        match spawn_result {
            Ok(handle) => self.session_action_worker.handle = Some(handle),
            Err(_) => {
                self.session_action_worker.receiver = None;
                self.status = Some("Could not start action availability check".into());
            }
        }
    }

    fn dispatch_session_action(
        &mut self,
        attempt: SessionActionAttempt,
        action: TerminalSessionAction,
    ) {
        if !self.session_action_attempt_is_visible(&attempt) {
            self.input = None;
            self.status = Some("Selection changed; action cancelled".into());
            return;
        }
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
                let result = actions.send_session_action(SessionActionRequest { attempt, action });
                let _ = sender.send(SessionActionWorkerResult::Delivery { kind, result });
            });
        match spawn_result {
            Ok(handle) => self.session_action_worker.handle = Some(handle),
            Err(_) => {
                self.session_action_worker.receiver = None;
                self.status = Some(format!("Could not start {} delivery", kind.label));
            }
        }
    }

    fn poll_session_action_worker(&mut self) -> Option<String> {
        let result = self.session_action_worker.receiver.as_ref()?.try_recv();
        match result {
            Ok(SessionActionWorkerResult::Preflight(Ok(availability))) => {
                self.session_action_worker.finish();
                if !self.session_action_attempt_is_visible(&availability.attempt) {
                    return Some("Selection changed; action cancelled".into());
                }
                self.status = None;
                self.input = Some(BrainInput::SessionAction {
                    attempt: availability.attempt,
                    capabilities: availability.capabilities,
                    text: None,
                });
                None
            }
            Ok(SessionActionWorkerResult::Preflight(Err(failure))) => {
                self.session_action_worker.finish();
                Some(session_action_failure_status(&failure))
            }
            Ok(SessionActionWorkerResult::Delivery { kind, result }) => {
                let status = match (kind.manual_bytes, result) {
                    (Some(bytes), Ok(())) => format!("Sent manual text ({bytes} bytes)"),
                    (Some(bytes), Err(failure)) => {
                        format!(
                            "Could not send manual text ({bytes} bytes): {}",
                            session_action_failure_status(&failure)
                        )
                    }
                    (None, Ok(())) => format!("Sent {}", kind.label),
                    (None, Err(failure)) => {
                        format!(
                            "Could not send {}: {}",
                            kind.label,
                            session_action_failure_status(&failure)
                        )
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
        if self.discard_stale_session_action_input() {
            self.status = Some("Selection changed; action cancelled".into());
            return None;
        }
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
                    attempt,
                    capabilities,
                    text: Some(text),
                }) if !text.is_empty()
                    && permits_session_action(
                        &capabilities,
                        &TerminalSessionAction::Text(text.clone()),
                    ) =>
                {
                    self.dispatch_session_action(attempt, TerminalSessionAction::Text(text));
                }
                Some(BrainInput::SessionAction { text: Some(_), .. }) => {
                    self.status = Some("Manual text cannot be empty".into());
                }
                Some(BrainInput::SessionAction { text: None, .. }) => {}
                None => {}
            },
            KeyCode::Char(character) => match self.input.clone() {
                Some(BrainInput::SessionAction {
                    attempt,
                    capabilities,
                    text: None,
                }) => match character {
                    'a' if permits_session_action(&capabilities, &TerminalSessionAction::Allow) => {
                        self.dispatch_session_action(attempt, TerminalSessionAction::Allow)
                    }
                    'd' if permits_session_action(&capabilities, &TerminalSessionAction::Deny) => {
                        self.dispatch_session_action(attempt, TerminalSessionAction::Deny)
                    }
                    'c' if permits_session_action(
                        &capabilities,
                        &TerminalSessionAction::Continue,
                    ) =>
                    {
                        self.dispatch_session_action(attempt, TerminalSessionAction::Continue)
                    }
                    't' if permits_session_action(
                        &capabilities,
                        &TerminalSessionAction::Text(String::new()),
                    ) =>
                    {
                        self.input = Some(BrainInput::SessionAction {
                            attempt,
                            capabilities,
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

    fn visible_live_action_target(&self) -> Option<SessionActionTarget> {
        (self.tab == BrainTab::Live)
            .then(|| self.selected_live_activity())
            .flatten()
            .and_then(|item| item.session.clone())
            .map(SessionActionTarget::from)
    }

    fn session_action_attempt_is_visible(&self, attempt: &SessionActionAttempt) -> bool {
        self.visible_live_action_target().as_ref() == Some(&attempt.target)
    }

    fn discard_stale_session_action_input(&mut self) -> bool {
        let stale = matches!(
            self.input.as_ref(),
            Some(BrainInput::SessionAction { attempt, .. })
                if !self.session_action_attempt_is_visible(attempt)
        );
        if stale {
            self.input = None;
        }
        stale
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
            Some(BrainInput::SessionAction {
                capabilities,
                text: None,
                ..
            }) => Some(capability_prompt(capabilities)),
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

fn permits_session_action(
    capabilities: &[SessionActionCapability],
    action: &TerminalSessionAction,
) -> bool {
    capabilities
        .iter()
        .any(|capability| capability.permits(action))
}

fn capability_prompt(capabilities: &[SessionActionCapability]) -> String {
    let mut actions = Vec::new();
    if capabilities.contains(&SessionActionCapability::Allow) {
        actions.push("[a] allow");
    }
    if capabilities.contains(&SessionActionCapability::Deny) {
        actions.push("[d] deny");
    }
    if capabilities.contains(&SessionActionCapability::Continue) {
        actions.push("[c] continue");
    }
    if capabilities.contains(&SessionActionCapability::ManualText) {
        actions.push("[t] manual text");
    }
    let mut prompt = format!("Action: {}", actions.join("  "));
    if capabilities == [SessionActionCapability::ManualText] {
        prompt.push_str(" · Continue requires a recognized recovery prompt");
    }
    prompt
}

fn session_action_failure_status(failure: &SessionActionFailure) -> String {
    let mut status = failure.safe_message().to_owned();
    if !failure.diagnostic_persisted {
        status.push_str("; diagnostic unavailable");
    }
    status
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use coding_brain_core::brain_activity::{
        ActivityItem, ActivityKind, ActivitySnapshot, ActivityState, AttentionItem,
        CorrectionDisposition, DeliveryState, ProjectEvidence, SessionTarget,
        SessionTargetProvenance,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{
        BrainActions, BrainEffect, BrainRefresh, BrainRuntime, BrainSource, BrainSourceError,
        CorrectionInput, DecisionSummary, EndpointHealth, MockBrainAction, MockBrainRuntime,
        ReviewItemSummary, SessionActionAvailability, SessionActionCapability,
        SessionActionFailure, SessionActionFailureCategory, SessionActionPreflightRequest,
        SessionActionRequest, SessionActionTarget,
    };
    use coding_brain_core::terminals::TerminalSessionAction;
    use coding_brain_core::theme::{Theme, ThemeMode};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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

            open_action_menu(&mut app);
            app.handle_key(key(KeyCode::Char(key_code)));
            wait_for_actions(&mut app, &mock, 2);

            assert!(matches!(
                non_poll_actions(&mock).as_slice(),
                [MockBrainAction::SessionActionPreflight(_), MockBrainAction::SessionAction(request)]
                    if request.attempt.target == SessionActionTarget::from(activity().session.unwrap())
                        && request.action == action
            ));
            assert_eq!(app.input_prompt(), None);
        }
    }

    #[test]
    fn x_starts_nonblocking_preflight_before_opening_action_menu() {
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (mut app, actions) = slow_preflight_fixture(Duration::from_millis(250), completed);

        let started = Instant::now();
        app.handle_key(key(KeyCode::Char('x')));

        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(app.input_prompt(), None);
        assert_eq!(app.status(), Some("Checking available actions…"));

        let refresh_started = Instant::now();
        app.refresh();
        assert!(refresh_started.elapsed() < Duration::from_millis(100));

        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let render_started = Instant::now();
        terminal
            .draw(|frame| crate::ui::brain::render(frame, &app))
            .unwrap();
        assert!(render_started.elapsed() < Duration::from_millis(100));

        wait_for_preflight(&mut app, &actions);
        assert_eq!(
            app.input_prompt(),
            Some("Action: [c] continue  [t] manual text".into())
        );
    }

    #[test]
    fn unavailable_semantic_key_cannot_dispatch_hidden_action() {
        let (mut app, mock) =
            fixture_app_with_capabilities(vec![SessionActionCapability::ManualText]);

        open_action_menu(&mut app);
        app.handle_key(key(KeyCode::Char('c')));

        assert!(
            non_poll_actions(&mock)
                .iter()
                .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
        );
        assert!(app.input_prompt().unwrap().contains("[t] manual text"));
        assert!(
            app.input_prompt()
                .unwrap()
                .contains("recognized recovery prompt")
        );
    }

    #[test]
    fn dispatch_reuses_attempt_identity_but_not_preflight_evidence() {
        let (mut app, mock) = fixture_app_with_capabilities(vec![
            SessionActionCapability::Continue,
            SessionActionCapability::ManualText,
        ]);

        open_action_menu(&mut app);
        let preflight_id = mock
            .actions()
            .into_iter()
            .find_map(|action| match action {
                MockBrainAction::SessionActionPreflight(request) => {
                    Some(request.attempt.attempt_id)
                }
                _ => None,
            })
            .unwrap();
        app.handle_key(key(KeyCode::Char('c')));
        wait_for_actions(&mut app, &mock, 2);
        let request = mock
            .actions()
            .into_iter()
            .find_map(|action| match action {
                MockBrainAction::SessionAction(request) => Some(request),
                _ => None,
            })
            .unwrap();

        assert_eq!(request.attempt.attempt_id, preflight_id);
    }

    #[test]
    fn preflight_continue_then_changed_prompt_is_rejected_and_diagnosable() {
        let (mut app, boundary) = prompt_change_boundary_fixture();

        open_action_menu(&mut app);
        assert_eq!(
            app.input_prompt(),
            Some("Action: [c] continue  [t] manual text".into())
        );
        let preflight_attempt = boundary.preflight_attempt();

        app.handle_key(key(KeyCode::Char('c')));
        wait_for_status(
            &mut app,
            "Could not send continue: Provider prompt changed before action",
        );

        let dispatch_attempt = boundary.dispatch_attempt();
        assert_eq!(dispatch_attempt, preflight_attempt);
        assert_eq!(boundary.terminal_inputs(), 0);
        let footer = render_brain_text(&app);
        assert!(
            footer.contains("Provider prompt changed before action"),
            "{footer}"
        );

        app.refresh();
        assert_eq!(app.snapshot.diagnostic_events.len(), 1);
        let diagnostic = &app.snapshot.diagnostic_events[0];
        assert_eq!(diagnostic.activity_id, preflight_attempt);
        assert_eq!(
            diagnostic.rule_id.as_deref(),
            Some("session_action_prompt_changed")
        );
        assert_eq!(
            diagnostic.reasoning.as_deref(),
            Some("Provider prompt changed before action")
        );
        assert_eq!(diagnostic.normalized_command, None);
        assert_eq!(diagnostic.note, None);
        let session = diagnostic.session.as_ref().expect("diagnostic session");
        assert_eq!(session.provider_session_id, None);
        assert_eq!(session.turn_id, None);
        assert_eq!(session.tool_use_id, None);
        assert!(session.provider_hints.is_empty());

        for _ in 0..3 {
            app.handle_key(key(KeyCode::Tab));
        }
        assert_eq!(app.tab(), BrainTab::Diagnostics);
        assert_eq!(
            app.selected_diagnostic()
                .map(|item| item.activity_id.as_str()),
            Some(preflight_attempt.as_str())
        );
    }

    #[test]
    fn preflight_result_is_discarded_when_selection_changes() {
        let (mut app, actions) = slow_preflight_fixture(
            Duration::from_millis(100),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Down));
        wait_for_preflight(&mut app, &actions);

        assert_eq!(app.input_prompt(), None);
        assert_eq!(app.status(), Some("Selection changed; action cancelled"));
    }

    #[test]
    fn preflight_result_is_discarded_when_tab_changes() {
        let (mut app, actions) = slow_preflight_fixture(
            Duration::from_millis(100),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Tab));
        wait_for_preflight(&mut app, &actions);

        assert_eq!(app.input_prompt(), None);
        assert_eq!(app.status(), Some("Selection changed; action cancelled"));
        assert_eq!(actions.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn refresh_discards_completed_preflight_when_selected_target_replaced() {
        let source = Arc::new(ScriptedBrainSource {
            refreshes: std::sync::Mutex::new(
                [
                    Ok(refresh_with_attention_session("session-a")),
                    Ok(refresh_with_attention_session("session-b")),
                ]
                .into_iter()
                .collect(),
            ),
        });
        let mut app = BrainApp::new(
            BrainRuntime::new(source, Arc::new(MockBrainRuntime::default())),
            Theme::from_mode(ThemeMode::Dark),
        );

        queue_preflight_result(&mut app, vec![SessionActionCapability::Continue]);
        app.refresh();

        assert_eq!(app.input_prompt(), None);
        assert_eq!(app.status(), Some("Selection changed; action cancelled"));
    }

    #[test]
    fn refresh_discards_open_action_menu_before_semantic_dispatch() {
        let source = Arc::new(ScriptedBrainSource {
            refreshes: std::sync::Mutex::new(
                [
                    Ok(refresh_with_attention_session("session-a")),
                    Ok(refresh_with_attention_session("session-a")),
                    Ok(refresh_with_attention_session("session-b")),
                ]
                .into_iter()
                .collect(),
            ),
        });
        let actions = Arc::new(MockBrainRuntime::default());
        let mut app = BrainApp::new(
            BrainRuntime::new(source, actions.clone()),
            Theme::from_mode(ThemeMode::Dark),
        );

        queue_preflight_result(&mut app, vec![SessionActionCapability::Continue]);
        app.refresh();
        assert!(app.input_prompt().is_some());

        app.refresh();
        assert_eq!(app.input_prompt(), None);
        assert_eq!(app.status(), Some("Selection changed; action cancelled"));
        app.handle_key(key(KeyCode::Char('c')));

        assert!(
            non_poll_actions(&actions)
                .iter()
                .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
        );
    }

    #[test]
    fn manual_text_backspace_removes_one_unicode_scalar() {
        let (mut app, _) = fixture_app_with_capabilities(vec![SessionActionCapability::ManualText]);
        open_action_menu(&mut app);
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Char('界')));
        app.handle_key(key(KeyCode::Char('x')));

        app.handle_key(key(KeyCode::Backspace));

        assert!(matches!(
            &app.input,
            Some(BrainInput::SessionAction {
                text: Some(text),
                ..
            }) if text == "界"
        ));
        let prompt = app.input_prompt().unwrap();
        assert!(prompt.contains("3 bytes"));
        assert!(!prompt.contains('界'));
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
        open_action_menu(&mut app);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.input_prompt(), None);
        assert!(
            non_poll_actions(&mock)
                .iter()
                .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
        );
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

        open_action_menu(&mut app);
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
        wait_for_actions(&mut app, &mock, 2);

        assert_eq!(app.input_prompt(), None);
        assert!(!app.status().unwrap().contains("top-secret-literal"));
        let actions = non_poll_actions(&mock);
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[1],
            MockBrainAction::SessionAction(request)
                if matches!(&request.action, TerminalSessionAction::Text(text) if text.len() == 4096)
        ));
    }

    #[test]
    fn escape_drops_manual_text_without_dispatch() {
        let (mut app, mock) = fixture_app(true);

        open_action_menu(&mut app);
        app.handle_key(key(KeyCode::Char('t')));
        for character in "top-secret-literal".chars() {
            app.handle_key(key(KeyCode::Char(character)));
        }
        app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.input_prompt(), None);
        assert!(
            non_poll_actions(&mock)
                .iter()
                .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
        );
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

        open_action_menu(&mut app);
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
                preflight_calls: AtomicUsize::new(0),
                completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                delay: Duration::from_millis(250),
                preflight_delay: Duration::ZERO,
                preflight_capabilities: vec![
                    SessionActionCapability::Allow,
                    SessionActionCapability::Deny,
                    SessionActionCapability::Continue,
                    SessionActionCapability::ManualText,
                ],
            });
            let runtime = BrainRuntime::new(source, actions.clone());
            let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

            open_action_menu(&mut app);
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
            error: "source failed".into(),
        });
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let actions = Arc::new(SlowBrainActions {
            error: None,
            calls: AtomicUsize::new(0),
            preflight_calls: AtomicUsize::new(0),
            completed,
            delay: Duration::from_millis(50),
            preflight_delay: Duration::ZERO,
            preflight_capabilities: vec![
                SessionActionCapability::Allow,
                SessionActionCapability::Deny,
                SessionActionCapability::Continue,
                SessionActionCapability::ManualText,
            ],
        });
        let runtime = BrainRuntime::new(source, actions);
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));
        dispatch_allow(&mut app);
        std::thread::sleep(Duration::from_millis(100));

        app.refresh();
        assert_eq!(app.status(), Some("Sent allow"));
        app.refresh();
        assert_eq!(app.status(), Some("Brain: source failed"));
    }

    #[test]
    fn refresh_source_error_is_redacted_and_bounded() {
        let source = Arc::new(ErrorAfterFirstSource {
            snapshot_calls: AtomicUsize::new(1),
            error: format!("source failed token=top-secret-literal {}", "x".repeat(700)),
        });
        let actions = Arc::new(MockBrainRuntime::default());
        let runtime = BrainRuntime::new(source, actions);
        let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        app.refresh();

        let status = app.status().unwrap();
        let error = status.strip_prefix("Brain: ").unwrap();
        assert!(!status.contains("top-secret-literal"));
        assert!(error.chars().count() <= MAX_NOTE_CHARS);
    }

    #[test]
    fn cold_start_busy_reports_retrying() {
        let app = scripted_app([Err(BrainSourceError::Busy)]);

        assert_eq!(app.status(), Some("Brain data busy; retrying"));
    }

    #[test]
    fn busy_refresh_retains_all_views_then_recovers_atomically() {
        let mut app = scripted_app([
            Ok(refresh_fixture("old", 1, 1)),
            Err(BrainSourceError::Busy),
            Ok(refresh_fixture("new", 2, 2)),
        ]);

        app.refresh();
        assert_refresh_fixture(&app, "old", 1, 1);
        assert_eq!(
            app.status(),
            Some("Brain data busy; showing previous refresh")
        );

        app.refresh();
        assert_refresh_fixture(&app, "new", 2, 2);
        assert_eq!(app.status(), None);
    }

    #[test]
    fn refresh_preserves_recent_selection_by_activity_id() {
        let mut app = scripted_app([
            Ok(refresh_with_recent(&["recent-2", "recent-1"])),
            Ok(refresh_with_recent(&["recent-3", "recent-2", "recent-1"])),
        ]);
        app.handle_key(key(KeyCode::Char('j')));
        app.update_live_evidence_metrics(5, 20);
        app.handle_key(key(KeyCode::PageDown));

        app.refresh();

        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "recent-1"
        );
        assert_eq!(app.selected_recent_index(), Some(2));
        assert_eq!(app.live_evidence_scroll(), 5);
    }

    #[test]
    fn refresh_removing_selected_recent_activity_uses_clamped_fallback() {
        let mut app = scripted_app([
            Ok(refresh_with_recent(&["recent-2", "recent-1"])),
            Ok(refresh_with_recent(&["recent-3"])),
        ]);
        app.handle_key(key(KeyCode::Char('j')));

        app.refresh();

        assert_eq!(
            app.selected_live_activity().unwrap().activity_id,
            "recent-3"
        );
        assert_eq!(app.selected_recent_index(), Some(0));
    }

    #[test]
    fn busy_refresh_does_not_overwrite_higher_priority_status() {
        let mut app = scripted_app([
            Ok(refresh_fixture("old", 1, 1)),
            Err(BrainSourceError::Busy),
        ]);
        app.status = Some("Sent allow".into());

        app.refresh();

        assert_eq!(app.status(), Some("Sent allow"));
    }

    #[test]
    fn recovery_warning_outranks_busy_information() {
        let source = Arc::new(ScriptedBrainSource {
            refreshes: std::sync::Mutex::new([Err(BrainSourceError::Busy)].into_iter().collect()),
        });
        let runtime = BrainRuntime::new(source, Arc::new(RecoveryWarningActions));
        let app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

        assert_eq!(app.status(), Some("Recovered interrupted activity"));
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
        let mut unrecognized = activity();
        let target = unrecognized.session.as_mut().unwrap();
        target.session_id = "process:7:9:4:pts0".into();
        target.provenance = SessionTargetProvenance::RecognizedProcessAttention;
        let (mut app, _mock) = fixture_app_with_live_activity(unrecognized.clone());

        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input_prompt(), None);

        unrecognized.rule_id = Some("actionable_prompt_attention".into());
        let (mut app, mock) = fixture_app_with_live_activity(unrecognized);
        open_action_menu(&mut app);
        assert!(app.input_prompt().is_some());
        assert!(
            non_poll_actions(&mock)
                .iter()
                .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
        );
    }

    #[test]
    fn opaque_native_prefixes_do_not_define_process_authority() {
        for session_id in ["live:opaque-native", "process:opaque-native"] {
            let mut item = activity();
            let target = item.session.as_mut().unwrap();
            target.session_id = session_id.into();
            target.provenance = SessionTargetProvenance::Structured;
            let (mut app, mock) = fixture_app_with_live_activity(item);

            open_action_menu(&mut app);

            assert!(app.input_prompt().is_some(), "rejected native {session_id}");
            assert!(
                non_poll_actions(&mock)
                    .iter()
                    .all(|action| !matches!(action, MockBrainAction::SessionAction(_)))
            );
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

    fn fixture_app_with_capabilities(
        capabilities: Vec<SessionActionCapability>,
    ) -> (BrainApp, Arc<MockBrainRuntime>) {
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
            session_action_capabilities: std::sync::Mutex::new(capabilities),
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            mock,
        )
    }

    fn fixture_app_with_live_activity(activity: ActivityItem) -> (BrainApp, Arc<MockBrainRuntime>) {
        let mock = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            ..MockBrainRuntime::default()
        });
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            mock,
        )
    }

    fn open_action_menu(app: &mut BrainApp) {
        app.handle_key(key(KeyCode::Char('x')));
        let deadline = Instant::now() + Duration::from_secs(1);
        while app.input_prompt().is_none() && Instant::now() < deadline {
            app.refresh();
            std::thread::yield_now();
        }
        assert!(app.input_prompt().is_some());
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

    fn wait_for_preflight(app: &mut BrainApp, actions: &SlowBrainActions) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while actions.preflight_calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            app.refresh();
            std::thread::yield_now();
        }
        while app.input_prompt().is_none()
            && app.status() == Some("Checking available actions…")
            && Instant::now() < deadline
        {
            app.refresh();
            std::thread::yield_now();
        }
    }

    fn queue_preflight_result(app: &mut BrainApp, capabilities: Vec<SessionActionCapability>) {
        let target = app
            .selected_live_activity()
            .unwrap()
            .session
            .clone()
            .unwrap();
        let availability = SessionActionAvailability {
            attempt: SessionActionPreflightRequest::new(target).attempt,
            capabilities,
        };
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .send(SessionActionWorkerResult::Preflight(Ok(availability)))
            .unwrap();
        app.session_action_worker.receiver = Some(receiver);
    }

    struct SlowBrainActions {
        error: Option<&'static str>,
        calls: AtomicUsize,
        preflight_calls: AtomicUsize,
        completed: Arc<std::sync::atomic::AtomicBool>,
        delay: Duration,
        preflight_delay: Duration,
        preflight_capabilities: Vec<SessionActionCapability>,
    }

    struct PromptChangeBoundary {
        preflight_attempt: std::sync::Mutex<Option<String>>,
        dispatch_attempt: std::sync::Mutex<Option<String>>,
        diagnostics: std::sync::Mutex<Vec<ActivityItem>>,
        prompt_changed: AtomicBool,
        terminal_inputs: AtomicUsize,
    }

    impl PromptChangeBoundary {
        fn preflight_attempt(&self) -> String {
            self.preflight_attempt
                .lock()
                .expect("preflight attempt poisoned")
                .clone()
                .expect("preflight attempt")
        }

        fn dispatch_attempt(&self) -> String {
            self.dispatch_attempt
                .lock()
                .expect("dispatch attempt poisoned")
                .clone()
                .expect("dispatch attempt")
        }

        fn terminal_inputs(&self) -> usize {
            self.terminal_inputs.load(Ordering::SeqCst)
        }
    }

    impl BrainSource for PromptChangeBoundary {
        fn refresh(&self, _limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError> {
            Ok(BrainRefresh {
                snapshot: ActivitySnapshot {
                    attention: vec![AttentionItem {
                        activity: activity(),
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    }],
                    diagnostic_events: self
                        .diagnostics
                        .lock()
                        .expect("prompt-change diagnostics poisoned")
                        .clone(),
                    unresolved_count: 1,
                    ..ActivitySnapshot::default()
                },
                ..BrainRefresh::default()
            })
        }

        fn gate_mode(&self) -> BrainGateMode {
            BrainGateMode::On
        }

        fn endpoint_health(&self) -> EndpointHealth {
            EndpointHealth::default()
        }
    }

    impl BrainActions for PromptChangeBoundary {
        fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
            Ok(())
        }

        fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
            Ok(())
        }

        fn preflight_session_action(
            &self,
            request: SessionActionPreflightRequest,
        ) -> Result<SessionActionAvailability, SessionActionFailure> {
            *self
                .preflight_attempt
                .lock()
                .expect("preflight attempt poisoned") = Some(request.attempt.attempt_id.clone());
            Ok(SessionActionAvailability {
                attempt: request.attempt,
                capabilities: vec![
                    SessionActionCapability::Continue,
                    SessionActionCapability::ManualText,
                ],
            })
        }

        fn send_session_action(
            &self,
            request: SessionActionRequest,
        ) -> Result<(), SessionActionFailure> {
            *self
                .dispatch_attempt
                .lock()
                .expect("dispatch attempt poisoned") = Some(request.attempt.attempt_id.clone());
            assert_eq!(request.action, TerminalSessionAction::Continue);
            if !self.prompt_changed.load(Ordering::SeqCst) {
                self.terminal_inputs.fetch_add(1, Ordering::SeqCst);
                return Ok(());
            }

            let mut diagnostics = self
                .diagnostics
                .lock()
                .expect("prompt-change diagnostics poisoned");
            if diagnostics.is_empty() {
                let mut diagnostic = diagnostic_activity(&request.attempt.attempt_id, 2);
                diagnostic.rule_id = Some("session_action_prompt_changed".into());
                diagnostic.reasoning = Some("Provider prompt changed before action".into());
                let session = diagnostic.session.as_mut().expect("diagnostic session");
                session.provider_session_id = None;
                session.turn_id = None;
                session.tool_use_id = None;
                session.provider_hints.clear();
                diagnostics.push(diagnostic);
            }
            Err(SessionActionFailure {
                category: SessionActionFailureCategory::Guarded(
                    coding_brain_core::terminals::GuardedActionFailureCategory::PromptChanged,
                ),
                diagnostic_persisted: true,
            })
        }
    }

    impl BrainActions for SlowBrainActions {
        fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
            Ok(())
        }

        fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
            Ok(())
        }

        fn preflight_session_action(
            &self,
            request: SessionActionPreflightRequest,
        ) -> Result<SessionActionAvailability, SessionActionFailure> {
            self.preflight_calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.preflight_delay);
            self.completed.store(true, Ordering::SeqCst);
            Ok(SessionActionAvailability {
                attempt: request.attempt,
                capabilities: self.preflight_capabilities.clone(),
            })
        }

        fn send_session_action(
            &self,
            _request: SessionActionRequest,
        ) -> Result<(), SessionActionFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.completed.store(true, Ordering::SeqCst);
            self.error.map_or(Ok(()), |_| {
                Err(SessionActionFailure {
                    category: SessionActionFailureCategory::Guarded(
                        coding_brain_core::terminals::GuardedActionFailureCategory::SendFailed,
                    ),
                    diagnostic_persisted: false,
                })
            })
        }
    }

    struct ErrorAfterFirstSource {
        snapshot_calls: AtomicUsize,
        error: String,
    }

    struct ScriptedBrainSource {
        refreshes: std::sync::Mutex<VecDeque<Result<BrainRefresh, BrainSourceError>>>,
    }

    impl BrainSource for ScriptedBrainSource {
        fn refresh(&self, _limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError> {
            self.refreshes
                .lock()
                .expect("scripted refreshes poisoned")
                .pop_front()
                .expect("unexpected refresh")
        }

        fn gate_mode(&self) -> BrainGateMode {
            BrainGateMode::On
        }

        fn endpoint_health(&self) -> EndpointHealth {
            EndpointHealth::default()
        }
    }

    struct RecoveryWarningActions;

    impl BrainActions for RecoveryWarningActions {
        fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
            Ok(())
        }

        fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
            Ok(())
        }

        fn preflight_session_action(
            &self,
            request: SessionActionPreflightRequest,
        ) -> Result<SessionActionAvailability, SessionActionFailure> {
            Ok(SessionActionAvailability {
                attempt: request.attempt,
                capabilities: vec![SessionActionCapability::ManualText],
            })
        }

        fn send_session_action(
            &self,
            _request: SessionActionRequest,
        ) -> Result<(), SessionActionFailure> {
            Ok(())
        }

        fn poll_recovery(&self) -> Vec<String> {
            vec!["Recovered interrupted activity".into()]
        }
    }

    fn scripted_app<const N: usize>(
        refreshes: [Result<BrainRefresh, BrainSourceError>; N],
    ) -> BrainApp {
        let source = Arc::new(ScriptedBrainSource {
            refreshes: std::sync::Mutex::new(refreshes.into_iter().collect()),
        });
        let actions = Arc::new(MockBrainRuntime::default());
        BrainApp::new(
            BrainRuntime::new(source, actions),
            Theme::from_mode(ThemeMode::Dark),
        )
    }

    fn prompt_change_boundary_fixture() -> (BrainApp, Arc<PromptChangeBoundary>) {
        let boundary = Arc::new(PromptChangeBoundary {
            preflight_attempt: std::sync::Mutex::new(None),
            dispatch_attempt: std::sync::Mutex::new(None),
            diagnostics: std::sync::Mutex::new(Vec::new()),
            prompt_changed: AtomicBool::new(true),
            terminal_inputs: AtomicUsize::new(0),
        });
        let runtime = BrainRuntime::new(boundary.clone(), boundary.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            boundary,
        )
    }

    fn render_brain_text(app: &BrainApp) -> String {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| crate::ui::brain::render(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn refresh_fixture(marker: &str, review_count: usize, total: usize) -> BrainRefresh {
        let mut live = activity();
        live.activity_id = marker.into();
        let mut review_decision = decision();
        review_decision.id = marker.into();
        BrainRefresh {
            snapshot: ActivitySnapshot {
                recent: vec![live],
                ..ActivitySnapshot::default()
            },
            review_queue: (0..review_count)
                .map(|_| ReviewItemSummary {
                    decision: review_decision.clone(),
                    reason: "fixture".into(),
                    score: 1.0,
                })
                .collect(),
            scorecard: ScorecardSummary {
                total_decisions: total,
                ..ScorecardSummary::default()
            },
        }
    }

    fn refresh_with_recent(activity_ids: &[&str]) -> BrainRefresh {
        BrainRefresh {
            snapshot: ActivitySnapshot {
                recent: activity_ids
                    .iter()
                    .map(|activity_id| {
                        let mut item = activity();
                        item.activity_id = (*activity_id).into();
                        item
                    })
                    .collect(),
                ..ActivitySnapshot::default()
            },
            ..BrainRefresh::default()
        }
    }

    fn refresh_with_attention_session(session_id: &str) -> BrainRefresh {
        let mut item = activity();
        item.session.as_mut().unwrap().session_id = session_id.into();
        BrainRefresh {
            snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: item,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            ..BrainRefresh::default()
        }
    }

    fn assert_refresh_fixture(app: &BrainApp, marker: &str, review_count: usize, total: usize) {
        assert_eq!(app.snapshot.recent[0].activity_id, marker);
        assert_eq!(app.review_queue.len(), review_count);
        assert!(
            app.review_queue
                .iter()
                .all(|item| item.decision.id == marker)
        );
        assert_eq!(app.scorecard.total_decisions, total);
    }

    impl BrainSource for ErrorAfterFirstSource {
        fn refresh(&self, _limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError> {
            if self.snapshot_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(BrainRefresh {
                    snapshot: ActivitySnapshot {
                        attention: vec![AttentionItem {
                            activity: activity(),
                            occurrences: 1,
                            unresolved_occurrences: 1,
                        }],
                        unresolved_count: 1,
                        ..ActivitySnapshot::default()
                    },
                    ..BrainRefresh::default()
                })
            } else {
                Err(BrainSourceError::Other(self.error.clone()))
            }
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
            preflight_calls: AtomicUsize::new(0),
            completed,
            delay,
            preflight_delay: Duration::ZERO,
            preflight_capabilities: vec![
                SessionActionCapability::Allow,
                SessionActionCapability::Deny,
                SessionActionCapability::Continue,
                SessionActionCapability::ManualText,
            ],
        });
        let runtime = BrainRuntime::new(source, actions.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            actions,
        )
    }

    fn slow_preflight_fixture(
        delay: Duration,
        completed: Arc<std::sync::atomic::AtomicBool>,
    ) -> (BrainApp, Arc<SlowBrainActions>) {
        let mut second = activity();
        second.activity_id = "activity-2".into();
        second.session.as_mut().unwrap().session_id = "session-2".into();
        let source = Arc::new(MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![
                    AttentionItem {
                        activity: activity(),
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    },
                    AttentionItem {
                        activity: second,
                        occurrences: 1,
                        unresolved_occurrences: 1,
                    },
                ],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            ..MockBrainRuntime::default()
        });
        let actions = Arc::new(SlowBrainActions {
            error: None,
            calls: AtomicUsize::new(0),
            preflight_calls: AtomicUsize::new(0),
            completed,
            delay: Duration::ZERO,
            preflight_delay: delay,
            preflight_capabilities: vec![
                SessionActionCapability::Continue,
                SessionActionCapability::ManualText,
            ],
        });
        let runtime = BrainRuntime::new(source, actions.clone());
        (
            BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)),
            actions,
        )
    }

    fn dispatch_allow(app: &mut BrainApp) {
        open_action_menu(app);
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
                provider_session_id: None,
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
            model: None,
            outcome_kind: None,
            outcome_detail: None,
            suggested_at: None,
            resolved_at: None,
        }
    }
}
