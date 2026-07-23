#![allow(dead_code)]

use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, MAX_ACTIVITY_FIELD_BYTES,
    ProjectEvidence, SessionTarget,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};
use coding_brain_core::runtime::BrainGateMode;
use coding_brain_core::session::{AgentSession, RawAgentSession, SessionStatus};
use coding_brain_core::terminals::{
    GuardedActionFailure, TerminalSessionAction, execute_guarded_action_classified,
    probe_actionable_prompt, probe_recovery_prompt,
};
use serde::{Deserialize, Serialize};

use super::activity::{ActivityStore, ActivityStoreError, AtomicReservationOutcome};
use crate::config::BrainConfig;

pub const MAX_RECOVERY_REASON_BYTES: usize = 160;
const MAX_RECOVERY_QUEUE: usize = 64;
const MAX_RECOVERY_WORKERS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    Continue(String),
    LeaveAlone,
}

impl RecoveryDecision {
    pub fn delivery_text(&self) -> Option<&'static str> {
        matches!(self, Self::Continue(_)).then_some("continue")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoverySuggestion {
    pub decision: RecoveryDecision,
    pub reasoning: String,
    pub confidence: f64,
    pub suggested_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecoveryEpoch {
    LifecycleSequence(u64),
    ProcessPrompt {
        last_message_ts: u64,
        prompt_fingerprint: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecoveryAttemptKey {
    pub session: AgentSessionKey,
    pub epoch: RecoveryEpoch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryTargetSnapshot {
    pub attempt: RecoveryAttemptKey,
    pub turn_id: Option<String>,
    pub live_process: Option<LiveProcessIdentity>,
    pub status: SessionStatus,
    pub last_message_ts: u64,
    pub pending_tool_use_id: Option<String>,
    pub prompt_fingerprint: Option<u64>,
}

impl RecoveryTargetSnapshot {
    fn has_consistent_evidence(&self) -> bool {
        let provider = self.attempt.session.provider;
        let live_matches = self
            .live_process
            .as_ref()
            .is_some_and(|identity| identity.provider == provider);
        let epoch_matches = match self.attempt.epoch {
            RecoveryEpoch::LifecycleSequence(sequence) => {
                sequence > 0 && self.prompt_fingerprint.is_some()
            }
            RecoveryEpoch::ProcessPrompt {
                last_message_ts,
                prompt_fingerprint,
            } => {
                self.last_message_ts == last_message_ts
                    && self.prompt_fingerprint == Some(prompt_fingerprint)
            }
        };
        live_matches
            && epoch_matches
            && matches!(
                self.status,
                SessionStatus::WaitingInput | SessionStatus::Idle | SessionStatus::Unknown
            )
    }

    fn evidence_json(&self) -> Result<String, String> {
        #[derive(Serialize)]
        struct Evidence<'a> {
            attempt: &'a RecoveryAttemptKey,
            turn_id: &'a Option<String>,
            live_process: &'a Option<LiveProcessIdentity>,
            status: &'static str,
            last_message_ts: u64,
            pending_tool_use_id: &'a Option<String>,
            prompt_fingerprint: Option<u64>,
        }
        let status = match self.status {
            SessionStatus::NeedsInput => "needs_input",
            SessionStatus::Processing => "processing",
            SessionStatus::WaitingInput => "waiting_input",
            SessionStatus::Unknown => "unknown",
            SessionStatus::Idle => "idle",
            SessionStatus::Finished => "finished",
        };
        let encoded = serde_json::to_string(&Evidence {
            attempt: &self.attempt,
            turn_id: &self.turn_id,
            live_process: &self.live_process,
            status,
            last_message_ts: self.last_message_ts,
            pending_tool_use_id: &self.pending_tool_use_id,
            prompt_fingerprint: self.prompt_fingerprint,
        })
        .map_err(|_| "recovery evidence serialization failed".to_string())?;
        if encoded.len() > MAX_ACTIVITY_FIELD_BYTES {
            return Err("recovery evidence exceeds persistence bound".into());
        }
        Ok(encoded)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingRecovery {
    pub suggestion: RecoverySuggestion,
    pub target: RecoveryTargetSnapshot,
}

impl PendingRecovery {
    pub fn bound(suggestion: RecoverySuggestion, target: RecoveryTargetSnapshot) -> Self {
        Self { suggestion, target }
    }

    pub fn matches(&self, current: &RecoveryTargetSnapshot) -> bool {
        self.target == *current
    }
}

pub fn evaluate_recovery(
    mode: BrainGateMode,
    target: &RecoveryTargetSnapshot,
    suggestion: &RecoverySuggestion,
    threshold: f64,
) -> RecoveryDecision {
    if mode != BrainGateMode::Auto
        || !target.has_consistent_evidence()
        || !suggestion.confidence.is_finite()
        || suggestion.confidence < threshold
        || !matches!(suggestion.decision, RecoveryDecision::Continue(_))
    {
        return RecoveryDecision::LeaveAlone;
    }
    RecoveryDecision::Continue("continue".into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationOutcome {
    Reserved,
    Duplicate,
    Cooldown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryExecution {
    Continued,
    Abstained,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDeliveryFailure {
    Failed,
    Unknown,
}

#[allow(clippy::too_many_arguments)]
pub fn execute_recovery_with<Infer, Snapshot, Reserve, Audit, Deliver, Postflight>(
    mode: BrainGateMode,
    target: RecoveryTargetSnapshot,
    threshold: f64,
    mut infer: Infer,
    mut snapshot: Snapshot,
    mut reserve: Reserve,
    mut audit: Audit,
    mut deliver: Deliver,
    mut postflight: Postflight,
) -> RecoveryExecution
where
    Infer: FnMut() -> Result<RecoverySuggestion, String>,
    Snapshot: FnMut() -> Result<RecoveryTargetSnapshot, String>,
    Reserve: FnMut(&RecoveryTargetSnapshot) -> Result<ReservationOutcome, String>,
    Audit: FnMut(ActivityState) -> Result<(), String>,
    Deliver: FnMut(&RecoveryTargetSnapshot) -> Result<(), RecoveryDeliveryFailure>,
    Postflight: FnMut(&RecoveryTargetSnapshot) -> Result<(), String>,
{
    if mode != BrainGateMode::Auto || !target.has_consistent_evidence() {
        return RecoveryExecution::Abstained;
    }
    let Ok(suggestion) = infer() else {
        return RecoveryExecution::Abstained;
    };
    if !matches!(
        evaluate_recovery(mode, &target, &suggestion, threshold),
        RecoveryDecision::Continue(_)
    ) {
        return RecoveryExecution::Abstained;
    }
    let pending = PendingRecovery::bound(suggestion, target.clone());
    if !snapshot().is_ok_and(|current| pending.matches(&current)) {
        return RecoveryExecution::Abstained;
    }
    if !matches!(reserve(&target), Ok(ReservationOutcome::Reserved)) {
        return RecoveryExecution::Abstained;
    }
    if audit(ActivityState::Evaluating).is_err() {
        return RecoveryExecution::Abstained;
    }
    if !snapshot().is_ok_and(|current| pending.matches(&current)) {
        return RecoveryExecution::Abstained;
    }
    match deliver(&target) {
        Ok(()) => {}
        Err(RecoveryDeliveryFailure::Failed) => {
            let _ = audit(ActivityState::DeliveryFailed);
            return RecoveryExecution::Abstained;
        }
        Err(RecoveryDeliveryFailure::Unknown) => return RecoveryExecution::Abstained,
    }
    if postflight(&target).is_err() {
        return RecoveryExecution::Abstained;
    }
    if audit(ActivityState::Delivered).is_err() {
        return RecoveryExecution::Abstained;
    }
    RecoveryExecution::Continued
}

#[allow(clippy::too_many_arguments)]
fn execute_antigravity_recovery_with<Infer, Snapshot, Reserve, Audit, Deliver>(
    mode: BrainGateMode,
    target: RecoveryTargetSnapshot,
    threshold: f64,
    mut infer: Infer,
    mut snapshot: Snapshot,
    mut reserve: Reserve,
    mut audit: Audit,
    mut deliver: Deliver,
) -> RecoveryExecution
where
    Infer: FnMut() -> Result<RecoverySuggestion, String>,
    Snapshot: FnMut() -> Result<RecoveryTargetSnapshot, String>,
    Reserve: FnMut(&RecoveryTargetSnapshot) -> Result<ReservationOutcome, String>,
    Audit: FnMut(ActivityState) -> Result<(), String>,
    Deliver: FnMut(&RecoveryTargetSnapshot) -> Result<(), String>,
{
    if mode != BrainGateMode::Auto || !target.has_consistent_evidence() {
        return RecoveryExecution::Abstained;
    }
    let Ok(suggestion) = infer() else {
        return RecoveryExecution::Abstained;
    };
    if !matches!(
        evaluate_recovery(mode, &target, &suggestion, threshold),
        RecoveryDecision::Continue(_)
    ) {
        return RecoveryExecution::Abstained;
    }
    let pending = PendingRecovery::bound(suggestion, target.clone());
    if !snapshot().is_ok_and(|current| pending.matches(&current)) {
        return RecoveryExecution::Abstained;
    }
    if !matches!(reserve(&target), Ok(ReservationOutcome::Reserved)) {
        return RecoveryExecution::Abstained;
    }
    if audit(ActivityState::Evaluating).is_err() {
        return RecoveryExecution::Abstained;
    }
    // This is the final evidence check before writing the irreversible Stop response.
    if !snapshot().is_ok_and(|current| pending.matches(&current)) {
        return RecoveryExecution::Abstained;
    }
    if deliver(&target).is_err() {
        let _ = audit(ActivityState::DeliveryFailed);
        return RecoveryExecution::Abstained;
    }
    if audit(ActivityState::Delivered).is_err() {
        // The response cannot be retracted. The persisted Evaluating state therefore
        // remains Unknown rather than falsely claiming Delivered.
        return RecoveryExecution::Abstained;
    }
    RecoveryExecution::Continued
}

#[derive(Debug, Clone)]
pub struct RecoveryReservationStore {
    activity: ActivityStore,
    cooldown_duration: Duration,
}

impl RecoveryReservationStore {
    pub fn at(path: impl Into<PathBuf>, cooldown_duration: Duration) -> Self {
        Self {
            activity: ActivityStore::at(path),
            cooldown_duration,
        }
    }

    pub fn reserve(
        &self,
        target: &RecoveryTargetSnapshot,
        now_ms: u64,
    ) -> Result<ReservationOutcome, ActivityStoreError> {
        let evidence = target
            .evidence_json()
            .map_err(|_| ActivityStoreError::InvalidEvent)?;
        let attempt =
            serde_json::to_string(&target.attempt).map_err(|_| ActivityStoreError::InvalidEvent)?;
        if attempt.len() + "recovery_reservation:".len() > MAX_ACTIVITY_FIELD_BYTES {
            return Err(ActivityStoreError::InvalidEvent);
        }
        let session = &target.attempt.session;
        let project_id = ProjectId::Temporary(format!("recovery:{}", session.storage_key()));
        let event = ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: format!("recovery_reservation:{attempt}"),
            recorded_at_ms: now_ms,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: PathBuf::from("."),
                label: None,
            },
            session: Some(SessionTarget {
                provider: session.provider,
                session_id: session.session_id.clone(),
                turn_id: target.turn_id.clone(),
                tool_use_id: target.pending_tool_use_id.clone(),
                project_id,
                cwd: PathBuf::from("."),
                provider_hints: Vec::new(),
            }),
            state: ActivityState::Observed,
            tool: Some("recovery".into()),
            normalized_command: None,
            fingerprint: Some(evidence),
            rule_id: Some("recovery_reservation".into()),
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        };
        self.activity
            .reserve_recovery_event(event, self.cooldown_duration.as_millis() as u64)
            .map(|outcome| match outcome {
                AtomicReservationOutcome::Reserved => ReservationOutcome::Reserved,
                AtomicReservationOutcome::Duplicate => ReservationOutcome::Duplicate,
                AtomicReservationOutcome::Cooldown => ReservationOutcome::Cooldown,
            })
    }
}

pub fn antigravity_continue_envelope(_reason: &str) -> serde_json::Value {
    serde_json::json!({
        "decision": "continue",
        "reason": "recovery approved by local model",
    })
}

const RECOVERY_PROMPT: &str = "A supported agent has stopped before completing its task and its exact recovery prompt is still visible. Decide whether to continue or leave it alone. Return JSON only: {\"action\":\"continue\"|\"leave_alone\",\"reasoning\":\"brief explanation\",\"confidence\":0.0-1.0}. You cannot choose or provide terminal input.";

pub(crate) fn run_hook(
    config: Option<&BrainConfig>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_hook_with(
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        config,
        provider,
        antigravity_event,
    );
}

fn run_hook_with<R: Read, W: Write, E: Write>(
    stdin: R,
    mut stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
) {
    let input = match crate::lifecycle_hook::read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(_) => {
            write_recovery_diagnostic(&mut stderr, "invalid recovery hook input");
            return;
        }
    };
    let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
    let lifecycle = coding_brain_core::lifecycle::LifecycleStore::at(&state_root);
    let activity_path = state_root.join("activity.jsonl");
    let activity = ActivityStore::at(&activity_path);
    let links = coding_brain_core::session_links::SessionLinkStore::at(
        state_root.join("session-links.jsonl"),
    );
    let recorded = match crate::lifecycle_hook::persist_provider_hook(
        provider,
        antigravity_event,
        &input,
        &lifecycle,
        Some(&activity),
        Some(&links),
    ) {
        Ok(recorded) => recorded,
        Err(_) => {
            write_recovery_diagnostic(&mut stderr, "Stop persistence failed");
            return;
        }
    };
    if recorded.outcome != coding_brain_core::lifecycle::ApplyOutcome::Applied
        || recorded.event.name() != coding_brain_core::lifecycle::LifecycleEventName::Stop
        || !recorded.recovery_link_persisted
    {
        return;
    }
    let mode = super::resolve_gate_mode(config).mode;
    if mode != BrainGateMode::Auto {
        return;
    }
    let Some(session) = hook_session(&recorded) else {
        return;
    };
    let initial = match hook_target(&lifecycle, &session, recorded.sequence) {
        Ok(target) => target,
        Err(_) => return,
    };
    let reservations = RecoveryReservationStore::at(&activity_path, Duration::from_secs(10));
    let threshold = super::pref_store::adaptive_threshold(Some("recovery")).unwrap_or(0.60);
    let reserve = |target: &RecoveryTargetSnapshot| {
        reservations
            .reserve(target, epoch_ms())
            .map_err(|_| "recovery reservation failed".to_string())
    };
    let outcome = if provider == AgentProvider::Antigravity {
        execute_antigravity_recovery_with(
            mode,
            initial.clone(),
            threshold,
            || {
                infer_configured_recovery(config, |config| {
                    super::client::infer_recovery(config, RECOVERY_PROMPT)
                })
            },
            || hook_target(&lifecycle, &session, recorded.sequence),
            reserve,
            |state| append_recovery_audit(&activity, &session, &initial, state, threshold),
            |_| {
                let bytes = serde_json::to_vec(&antigravity_continue_envelope(
                    "recovery approved by local model",
                ))
                .map_err(|_| "recovery envelope serialization failed".to_string())?;
                stdout
                    .write_all(&bytes)
                    .and_then(|_| stdout.flush())
                    .map_err(|_| "structured recovery delivery failed".to_string())
            },
        )
    } else {
        execute_recovery_with(
            mode,
            initial.clone(),
            threshold,
            || {
                infer_configured_recovery(config, |config| {
                    super::client::infer_recovery(config, RECOVERY_PROMPT)
                })
            },
            || hook_target(&lifecycle, &session, recorded.sequence),
            reserve,
            |state| append_recovery_audit(&activity, &session, &initial, state, threshold),
            |_| {
                execute_guarded_action_classified(&session, TerminalSessionAction::Continue)
                    .map(|_| ())
                    .map_err(recovery_delivery_failure)
            },
            |target| hook_postflight_matches(&lifecycle, &session, target),
        )
    };
    let _ = outcome;
}

fn hook_session(recorded: &crate::lifecycle_hook::RecordedProviderHook) -> Option<AgentSession> {
    let live = recorded.parsed.live_process.as_ref()?;
    let identity = recorded.event.identity();
    let mut session = AgentSession::from_raw(RawAgentSession {
        provider: identity.provider(),
        pid: live.pid,
        process_start_identity: Some(live.process_start_identity),
        session_id: identity.session_id().to_string(),
        cwd: identity.cwd().to_string_lossy().into_owned(),
        started_at: 0,
    });
    session.tty.clone_from(&live.tty);
    session.status = SessionStatus::Idle;
    session.pending_tool_call_id = recorded.parsed.tool_use_id.clone();
    Some(session)
}

fn hook_target(
    lifecycle: &coding_brain_core::lifecycle::LifecycleStore,
    session: &AgentSession,
    sequence: u64,
) -> Result<RecoveryTargetSnapshot, String> {
    let key = AgentSessionKey::native(session.provider, &session.session_id);
    let view = lifecycle.read().map_err(|_| "lifecycle read failed")?;
    let state = view
        .snapshot
        .and_then(|snapshot| snapshot.sessions.get(&key.storage_key()).cloned())
        .ok_or("lifecycle evidence missing")?;
    if state.latest_sequence != sequence
        || state.latest_event != Some(coding_brain_core::lifecycle::LifecycleEventName::Stop)
    {
        return Err("lifecycle evidence changed".into());
    }
    let evidence = probe_recovery_prompt(session)?;
    Ok(RecoveryTargetSnapshot {
        attempt: RecoveryAttemptKey {
            session: key,
            epoch: RecoveryEpoch::LifecycleSequence(sequence),
        },
        turn_id: state.current_turn,
        live_process: session.live_process_identity(),
        status: SessionStatus::Idle,
        last_message_ts: state.latest_received_at_ms,
        pending_tool_use_id: session.pending_tool_call_id.clone(),
        prompt_fingerprint: Some(evidence.fingerprint),
    })
}

fn hook_postflight_matches(
    lifecycle: &coding_brain_core::lifecycle::LifecycleStore,
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
) -> Result<(), String> {
    let Some(live) = target.live_process.as_ref() else {
        return Err("live process evidence missing".into());
    };
    if session.live_process_identity().as_ref() != Some(live)
        || !crate::provider_hooks::revalidate_live_process(live)
    {
        return Err("live process evidence changed".into());
    }
    let RecoveryEpoch::LifecycleSequence(sequence) = target.attempt.epoch else {
        return Err("recovery epoch changed".into());
    };
    let view = lifecycle.read().map_err(|_| "lifecycle read failed")?;
    let state = view
        .snapshot
        .and_then(|snapshot| {
            snapshot
                .sessions
                .get(&target.attempt.session.storage_key())
                .cloned()
        })
        .ok_or("lifecycle evidence missing")?;
    if state.latest_sequence != sequence
        || state.latest_event != Some(coding_brain_core::lifecycle::LifecycleEventName::Stop)
        || state.current_turn != target.turn_id
        || state.latest_received_at_ms != target.last_message_ts
    {
        return Err("postflight lifecycle evidence changed".into());
    }
    Ok(())
}

fn append_recovery_audit(
    activity: &ActivityStore,
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
    state: ActivityState,
    threshold: f64,
) -> Result<(), String> {
    let attempt = serde_json::to_string(&target.attempt)
        .map_err(|_| "recovery audit serialization failed".to_string())?;
    let fingerprint = target.evidence_json()?;
    let project_id =
        ProjectId::Temporary(format!("recovery:{}", target.attempt.session.storage_key()));
    activity
        .append(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: format!("recovery_delivery:{attempt}"),
            recorded_at_ms: epoch_ms(),
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: PathBuf::from(&session.cwd),
                label: None,
            },
            session: Some(SessionTarget {
                provider: session.provider,
                session_id: session.session_id.clone(),
                turn_id: target.turn_id.clone(),
                tool_use_id: target.pending_tool_use_id.clone(),
                project_id,
                cwd: PathBuf::from(&session.cwd),
                provider_hints: Vec::new(),
            }),
            state,
            tool: Some("recovery".into()),
            normalized_command: None,
            fingerprint: Some(fingerprint),
            rule_id: Some("recovery".into()),
            confidence: None,
            threshold: Some(threshold),
            reasoning: Some(
                match state {
                    ActivityState::DeliveryFailed => "guarded recovery delivery failed",
                    ActivityState::Delivered => "guarded recovery delivered",
                    _ => "guarded recovery evaluating",
                }
                .into(),
            ),
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|_| "recovery audit persistence failed".to_string())
}

fn write_recovery_diagnostic(stderr: &mut impl Write, message: &str) {
    let _ = writeln!(stderr, "coding-brain recovery hook: {message}");
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn compact_fingerprint(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Clone)]
struct RecoveryPollWork {
    session: AgentSession,
    target: RecoveryTargetSnapshot,
}

enum RecoveryPollResult {
    Scan(Vec<RecoveryPollWork>),
    Evaluated {
        attempt: RecoveryAttemptKey,
        message: Option<String>,
    },
}

struct RecoveryPollState {
    queue: VecDeque<RecoveryPollWork>,
    inflight: HashSet<RecoveryAttemptKey>,
    active_workers: usize,
    scan_inflight: bool,
    saturation_reported: bool,
    tx: SyncSender<RecoveryPollResult>,
    rx: Receiver<RecoveryPollResult>,
}

type RecoveryScan = dyn Fn() -> Vec<RecoveryPollWork> + Send + Sync;
type RecoveryEvaluate = dyn Fn(&RecoveryPollWork) -> Option<String> + Send + Sync;

pub struct RecoveryCoordinator {
    state: Mutex<RecoveryPollState>,
    scan: Arc<RecoveryScan>,
    evaluate: Arc<RecoveryEvaluate>,
}

impl Default for RecoveryCoordinator {
    fn default() -> Self {
        let discovery = Arc::new(Mutex::new(
            coding_brain_core::discovery::ProviderDiscoveryState::default(),
        ));
        let scan = Arc::new(move || scan_recovery_work(&discovery));
        Self::with_workers(scan, Arc::new(evaluate_poll_work))
    }
}

impl RecoveryCoordinator {
    fn with_workers(scan: Arc<RecoveryScan>, evaluate: Arc<RecoveryEvaluate>) -> Self {
        let (tx, rx) = sync_channel(MAX_RECOVERY_QUEUE);
        Self {
            state: Mutex::new(RecoveryPollState {
                queue: VecDeque::new(),
                inflight: HashSet::new(),
                active_workers: 0,
                scan_inflight: false,
                saturation_reported: false,
                tx,
                rx,
            }),
            scan,
            evaluate,
        }
    }

    pub fn poll(&self) -> Vec<String> {
        let mut messages = Vec::new();
        let Ok(mut state) = self.state.try_lock() else {
            return messages;
        };
        loop {
            match state.rx.try_recv() {
                Ok(RecoveryPollResult::Scan(work)) => {
                    state.active_workers = state.active_workers.saturating_sub(1);
                    state.scan_inflight = false;
                    for candidate in work {
                        let duplicate = state.inflight.contains(&candidate.target.attempt)
                            || state
                                .queue
                                .iter()
                                .any(|queued| queued.target.attempt == candidate.target.attempt);
                        if duplicate {
                            continue;
                        }
                        if state.queue.len() >= MAX_RECOVERY_QUEUE {
                            if !state.saturation_reported {
                                messages
                                    .push("Recovery polling saturated; candidate skipped".into());
                                state.saturation_reported = true;
                            }
                            continue;
                        }
                        state.queue.push_back(candidate);
                    }
                }
                Ok(RecoveryPollResult::Evaluated { attempt, message }) => {
                    state.active_workers = state.active_workers.saturating_sub(1);
                    state.inflight.remove(&attempt);
                    if let Some(message) = message {
                        messages.push(message);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        while state.active_workers < MAX_RECOVERY_WORKERS {
            let Some(work) = state.queue.pop_front() else {
                break;
            };
            let attempt = work.target.attempt.clone();
            if !state.inflight.insert(attempt.clone()) {
                continue;
            }
            state.active_workers += 1;
            let tx = state.tx.clone();
            let evaluate = Arc::clone(&self.evaluate);
            let failed_attempt = attempt.clone();
            if std::thread::Builder::new()
                .name("coding-brain-recovery".into())
                .spawn(move || {
                    let message = evaluate(&work);
                    let _ = tx.send(RecoveryPollResult::Evaluated { attempt, message });
                })
                .is_err()
            {
                state.active_workers = state.active_workers.saturating_sub(1);
                state.inflight.remove(&failed_attempt);
            }
        }
        if !state.scan_inflight && state.active_workers < MAX_RECOVERY_WORKERS {
            state.scan_inflight = true;
            state.active_workers += 1;
            let tx = state.tx.clone();
            let scan = Arc::clone(&self.scan);
            if std::thread::Builder::new()
                .name("coding-brain-recovery-scan".into())
                .spawn(move || {
                    let work = scan();
                    let _ = tx.send(RecoveryPollResult::Scan(work));
                })
                .is_err()
            {
                state.scan_inflight = false;
                state.active_workers = state.active_workers.saturating_sub(1);
            }
        }
        messages
    }
}

fn scan_recovery_work(
    discovery: &Mutex<coding_brain_core::discovery::ProviderDiscoveryState>,
) -> Vec<RecoveryPollWork> {
    let Ok(mut discovery) = discovery.lock() else {
        return Vec::new();
    };
    let sessions = coding_brain_core::discovery::scan_agent_sessions_with_state(&mut discovery);
    drop(discovery);
    let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
    let lifecycle = coding_brain_core::lifecycle::LifecycleStore::at(&state_root)
        .read()
        .ok()
        .and_then(|view| view.snapshot);
    let links = coding_brain_core::session_links::SessionLinkStore::at(
        state_root.join("session-links.jsonl"),
    )
    .read_projection()
    .ok();
    let activity = ActivityStore::at(state_root.join("activity.jsonl"));
    scan_recovery_sessions(
        sessions,
        lifecycle.as_ref(),
        links.as_ref(),
        &activity,
        |session| {
            probe_actionable_prompt(session).map(|evidence| {
                (
                    evidence.fingerprint,
                    evidence.action == TerminalSessionAction::Continue,
                )
            })
        },
    )
}

fn scan_recovery_sessions(
    sessions: Vec<AgentSession>,
    lifecycle: Option<&coding_brain_core::lifecycle::LifecycleSnapshot>,
    links: Option<&coding_brain_core::session_links::SessionIdentityProjection>,
    activity: &ActivityStore,
    probe: impl Fn(&AgentSession) -> Result<(u64, bool), String>,
) -> Vec<RecoveryPollWork> {
    sessions
        .into_iter()
        .filter_map(|session| {
            let (prompt_fingerprint, recoverable) = probe(&session).ok()?;
            let (target, process_only) =
                recovery_target_for_session(&session, prompt_fingerprint, lifecycle, links)?;
            if process_only {
                append_process_attention(activity, &session, &target).ok()?;
            }
            recoverable.then_some(RecoveryPollWork { session, target })
        })
        .take(MAX_RECOVERY_QUEUE + 1)
        .collect()
}

fn recovery_target_for_session(
    session: &AgentSession,
    prompt_fingerprint: u64,
    lifecycle: Option<&coding_brain_core::lifecycle::LifecycleSnapshot>,
    links: Option<&coding_brain_core::session_links::SessionIdentityProjection>,
) -> Option<(RecoveryTargetSnapshot, bool)> {
    let live = session.live_process_identity()?;
    if let Some(native_id) = links.and_then(|projection| projection.native_for(&live)) {
        let native = AgentSessionKey::native(session.provider, native_id);
        if let Some(state) =
            lifecycle.and_then(|snapshot| snapshot.sessions.get(&native.storage_key()))
            && state.latest_event == Some(coding_brain_core::lifecycle::LifecycleEventName::Stop)
            && state.latest_sequence > 0
        {
            return Some((
                RecoveryTargetSnapshot {
                    attempt: RecoveryAttemptKey {
                        session: native,
                        epoch: RecoveryEpoch::LifecycleSequence(state.latest_sequence),
                    },
                    turn_id: state.current_turn.clone(),
                    live_process: Some(live),
                    status: session.status,
                    last_message_ts: session.last_message_ts,
                    pending_tool_use_id: None,
                    prompt_fingerprint: Some(prompt_fingerprint),
                },
                false,
            ));
        }
        return None;
    }
    let synthetic = AgentSessionKey::native(
        session.provider,
        format!(
            "live:{}:{}:{}:{}",
            live.pid,
            live.process_start_identity,
            live.tty.len(),
            live.tty
        ),
    );
    Some((
        RecoveryTargetSnapshot {
            attempt: RecoveryAttemptKey {
                session: synthetic,
                epoch: RecoveryEpoch::ProcessPrompt {
                    last_message_ts: session.last_message_ts,
                    prompt_fingerprint,
                },
            },
            turn_id: None,
            live_process: Some(live),
            status: session.status,
            last_message_ts: session.last_message_ts,
            pending_tool_use_id: None,
            prompt_fingerprint: Some(prompt_fingerprint),
        },
        true,
    ))
}

struct RefreshedPollEvidence {
    session: AgentSession,
    target: RecoveryTargetSnapshot,
}

fn resolve_current_poll_session(
    work: &RecoveryPollWork,
    sessions: Vec<AgentSession>,
) -> Result<AgentSession, String> {
    let expected = work
        .session
        .live_process_identity()
        .ok_or("recovery live process identity missing")?;
    let mut matches = sessions
        .into_iter()
        .filter(|session| session.live_process_identity().as_ref() == Some(&expected));
    let current = matches
        .next()
        .ok_or("recovery live process identity disappeared")?;
    if matches.next().is_some() {
        return Err("recovery live process identity is ambiguous".into());
    }
    Ok(current)
}

fn refresh_poll_evidence_from(
    work: &RecoveryPollWork,
    sessions: Vec<AgentSession>,
    lifecycle: Option<&coding_brain_core::lifecycle::LifecycleSnapshot>,
    links: Option<&coding_brain_core::session_links::SessionIdentityProjection>,
    probe: impl FnOnce(&AgentSession) -> Result<u64, String>,
) -> Result<RefreshedPollEvidence, String> {
    let session = resolve_current_poll_session(work, sessions)?;
    let prompt_fingerprint = probe(&session)?;
    let target = recovery_target_for_session(&session, prompt_fingerprint, lifecycle, links)
        .map(|(target, _)| target)
        .ok_or_else(|| "recovery target unavailable".to_string())?;
    Ok(RefreshedPollEvidence { session, target })
}

fn refresh_poll_evidence(work: &RecoveryPollWork) -> Result<RefreshedPollEvidence, String> {
    let sessions = coding_brain_core::discovery::scan_agent_sessions_with_state(
        &mut coding_brain_core::discovery::ProviderDiscoveryState::default(),
    );
    let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
    let lifecycle = coding_brain_core::lifecycle::LifecycleStore::at(&state_root)
        .read()
        .ok()
        .and_then(|view| view.snapshot);
    let links = coding_brain_core::session_links::SessionLinkStore::at(
        state_root.join("session-links.jsonl"),
    )
    .read_projection()
    .ok();
    refresh_poll_evidence_from(
        work,
        sessions,
        lifecycle.as_ref(),
        links.as_ref(),
        |session| probe_recovery_prompt(session).map(|evidence| evidence.fingerprint),
    )
}

fn refresh_poll_target(work: &RecoveryPollWork) -> Result<RecoveryTargetSnapshot, String> {
    refresh_poll_evidence(work).map(|evidence| evidence.target)
}

fn evaluate_poll_work(work: &RecoveryPollWork) -> Option<String> {
    let config = crate::config::Config::load();
    let mode = super::resolve_gate_mode(config.brain.as_ref()).mode;
    let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
    let activity = ActivityStore::at(state_root.join("activity.jsonl"));
    let reservations =
        RecoveryReservationStore::at(state_root.join("activity.jsonl"), Duration::from_secs(10));
    let threshold = super::pref_store::adaptive_threshold(Some("recovery")).unwrap_or(0.60);
    let outcome = execute_recovery_with(
        mode,
        work.target.clone(),
        threshold,
        || {
            infer_configured_recovery(config.brain.as_ref(), |config| {
                super::client::infer_recovery(config, RECOVERY_PROMPT)
            })
        },
        || refresh_poll_target(work),
        |target| {
            reservations
                .reserve(target, epoch_ms())
                .map_err(|_| "recovery reservation failed".to_string())
        },
        |state| append_recovery_audit(&activity, &work.session, &work.target, state, threshold),
        |target| {
            let refreshed =
                refresh_poll_evidence(work).map_err(|_| RecoveryDeliveryFailure::Failed)?;
            if refreshed.target != *target {
                return Err(RecoveryDeliveryFailure::Failed);
            }
            execute_guarded_action_classified(&refreshed.session, TerminalSessionAction::Continue)
                .map(|_| ())
                .map_err(recovery_delivery_failure)
        },
        |target| poll_postflight_matches(work, target),
    );
    (outcome == RecoveryExecution::Continued)
        .then(|| format!("Recovered {} session", work.session.provider.label()))
}

fn infer_configured_recovery(
    config: Option<&BrainConfig>,
    infer: impl FnOnce(&BrainConfig) -> Result<RecoverySuggestion, String>,
) -> Result<RecoverySuggestion, String> {
    let default_config;
    let config = match config {
        Some(config) => config,
        None => {
            default_config = BrainConfig::default();
            &default_config
        }
    };
    infer(config)
}

fn recovery_delivery_failure(error: GuardedActionFailure) -> RecoveryDeliveryFailure {
    match error {
        GuardedActionFailure::NotSent(_) => RecoveryDeliveryFailure::Failed,
        GuardedActionFailure::DeliveryUnknown(_) => RecoveryDeliveryFailure::Unknown,
    }
}

fn poll_postflight_matches(
    work: &RecoveryPollWork,
    target: &RecoveryTargetSnapshot,
) -> Result<(), String> {
    let session = resolve_current_poll_session(
        work,
        coding_brain_core::discovery::scan_agent_sessions_with_state(
            &mut coding_brain_core::discovery::ProviderDiscoveryState::default(),
        ),
    )?;
    let live = target
        .live_process
        .as_ref()
        .ok_or("live process evidence missing")?;
    if session.live_process_identity().as_ref() != Some(live)
        || !crate::provider_hooks::revalidate_live_process(live)
    {
        return Err("live process evidence changed".into());
    }
    if let RecoveryEpoch::LifecycleSequence(sequence) = target.attempt.epoch {
        let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
        let view = coding_brain_core::lifecycle::LifecycleStore::at(state_root)
            .read()
            .map_err(|_| "lifecycle read failed")?;
        let state = view
            .snapshot
            .and_then(|snapshot| {
                snapshot
                    .sessions
                    .get(&target.attempt.session.storage_key())
                    .cloned()
            })
            .ok_or("lifecycle evidence missing")?;
        if state.latest_sequence != sequence || state.current_turn != target.turn_id {
            return Err("postflight lifecycle evidence changed".into());
        }
    }
    Ok(())
}

fn append_process_attention(
    activity: &ActivityStore,
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
) -> Result<(), String> {
    let fingerprint = target.evidence_json()?;
    let attempt = serde_json::to_string(&target.attempt)
        .map_err(|_| "recovery attention identity failed".to_string())?;
    let attempt_id = compact_fingerprint(&attempt);
    let project_id =
        ProjectId::Temporary(format!("recovery:{}", target.attempt.session.storage_key()));
    let event = ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: format!("actionable_prompt_attention:{attempt_id:016x}"),
        recorded_at_ms: epoch_ms(),
        project: ProjectEvidence {
            project_id: project_id.clone(),
            cwd: PathBuf::from(&session.cwd),
            label: None,
        },
        session: Some(SessionTarget {
            provider: target.attempt.session.provider,
            session_id: target.attempt.session.session_id.clone(),
            turn_id: None,
            tool_use_id: None,
            project_id,
            cwd: PathBuf::from(&session.cwd),
            provider_hints: Vec::new(),
        }),
        state: ActivityState::Abstained,
        tool: Some("agent_prompt".into()),
        normalized_command: None,
        fingerprint: Some(fingerprint),
        rule_id: Some("actionable_prompt_attention".into()),
        confidence: None,
        threshold: None,
        reasoning: Some("recognized actionable agent prompt".into()),
        decision_id: None,
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    };
    activity
        .append_if_absent(event)
        .map(|_| ())
        .map_err(|_| "recovery attention persistence failed".into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use coding_brain_core::lifecycle::{
        LifecycleEvent, LifecycleEventKind, LifecycleIdentity, LifecycleSnapshot,
    };
    use coding_brain_core::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};
    use coding_brain_core::runtime::BrainGateMode;
    use coding_brain_core::session::{AgentSession, RawAgentSession, SessionStatus};
    use coding_brain_core::session_links::{
        SESSION_IDENTITY_LINK_SCHEMA_VERSION, SessionIdentityLink, SessionLinkStore,
    };

    use super::*;

    type EvidenceChange = Box<dyn Fn(&mut RecoveryTargetSnapshot)>;

    fn target(provider: AgentProvider) -> RecoveryTargetSnapshot {
        RecoveryTargetSnapshot {
            attempt: RecoveryAttemptKey {
                session: AgentSessionKey::native(provider, "same-session"),
                epoch: RecoveryEpoch::ProcessPrompt {
                    last_message_ts: 1_000,
                    prompt_fingerprint: 42,
                },
            },
            turn_id: Some("turn-1".into()),
            live_process: LiveProcessIdentity::try_new(provider, 7, 99, "pts/7"),
            status: SessionStatus::WaitingInput,
            last_message_ts: 1_000,
            pending_tool_use_id: Some("tool-1".into()),
            prompt_fingerprint: Some(42),
        }
    }

    fn suggestion(confidence: f64) -> RecoverySuggestion {
        RecoverySuggestion {
            decision: RecoveryDecision::Continue("untrusted model text".into()),
            reasoning: "model selected continuation".into(),
            confidence,
            suggested_at: 1_000,
        }
    }

    fn process_session(provider: AgentProvider) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider,
            pid: 7,
            process_start_identity: Some(99),
            session_id: "discovery-only".into(),
            cwd: "/tmp/recovery-test".into(),
            started_at: 1,
        });
        session.tty = "pts/7".into();
        session.last_message_ts = 1_000;
        session
    }

    fn poll_work(sequence: u64) -> RecoveryPollWork {
        let session = process_session(AgentProvider::Claude);
        let mut target = target(AgentProvider::Claude);
        target.attempt.epoch = RecoveryEpoch::LifecycleSequence(sequence);
        RecoveryPollWork { session, target }
    }

    #[test]
    fn accepted_waiting_recovery_defaults_to_literal_continue() {
        let decision = evaluate_recovery(
            BrainGateMode::Auto,
            &target(AgentProvider::Claude),
            &suggestion(0.91),
            0.60,
        );

        assert_eq!(decision, RecoveryDecision::Continue("continue".into()));
        assert_eq!(decision.delivery_text(), Some("continue"));
    }

    #[test]
    fn off_on_and_low_confidence_recovery_abstain() {
        let target = target(AgentProvider::Claude);
        assert_eq!(
            evaluate_recovery(BrainGateMode::Off, &target, &suggestion(0.99), 0.60),
            RecoveryDecision::LeaveAlone
        );
        assert_eq!(
            evaluate_recovery(BrainGateMode::On, &target, &suggestion(0.99), 0.60),
            RecoveryDecision::LeaveAlone
        );
        assert_eq!(
            evaluate_recovery(BrainGateMode::Auto, &target, &suggestion(0.59), 0.60),
            RecoveryDecision::LeaveAlone
        );
    }

    #[test]
    fn missing_brain_section_uses_default_config_for_auto_inference() {
        let inferred = std::cell::Cell::new(false);

        let suggestion = infer_configured_recovery(None, |config| {
            inferred.set(true);
            assert_eq!(config.endpoint, BrainConfig::default().endpoint);
            assert_eq!(config.model, BrainConfig::default().model);
            Ok(suggestion(0.91))
        })
        .unwrap();

        assert!(inferred.get());
        assert_eq!(suggestion.confidence, 0.91);
    }

    #[test]
    fn every_actionable_evidence_change_expires_pending_recovery() {
        let original = target(AgentProvider::Claude);
        let pending = PendingRecovery::bound(suggestion(0.91), original.clone());
        let mut changes: Vec<EvidenceChange> = vec![
            Box::new(|target| target.turn_id = Some("turn-2".into())),
            Box::new(|target| target.pending_tool_use_id = Some("tool-2".into())),
            Box::new(|target| target.status = SessionStatus::Processing),
            Box::new(|target| target.last_message_ts += 1),
            Box::new(|target| target.prompt_fingerprint = Some(43)),
            Box::new(|target| target.attempt.epoch = RecoveryEpoch::LifecycleSequence(9)),
            Box::new(|target| {
                target.live_process =
                    LiveProcessIdentity::try_new(AgentProvider::Claude, 7, 100, "pts/7")
            }),
        ];
        assert!(pending.matches(&original));
        for change in &mut changes {
            let mut changed = original.clone();
            change(&mut changed);
            assert!(!pending.matches(&changed));
        }
    }

    #[test]
    fn recovery_attempt_key_is_provider_qualified() {
        assert_ne!(
            target(AgentProvider::Codex).attempt,
            target(AgentProvider::Claude).attempt
        );
    }

    #[test]
    fn durable_reservation_deduplicates_attempt_and_enforces_session_cooldown() {
        let temp = tempfile::tempdir().unwrap();
        let store = RecoveryReservationStore::at(
            temp.path().join("activity.jsonl"),
            Duration::from_secs(10),
        );
        let first = target(AgentProvider::Claude);
        let mut later = first.clone();
        later.attempt.epoch = RecoveryEpoch::LifecycleSequence(2);

        assert_eq!(
            store.reserve(&first, 1_000).unwrap(),
            ReservationOutcome::Reserved
        );
        assert_eq!(
            store.reserve(&first, 1_001).unwrap(),
            ReservationOutcome::Duplicate
        );
        assert_eq!(
            store.reserve(&later, 10_999).unwrap(),
            ReservationOutcome::Cooldown
        );
        assert_eq!(
            store.reserve(&later, 11_000).unwrap(),
            ReservationOutcome::Reserved
        );
        assert_eq!(
            store.reserve(&target(AgentProvider::Codex), 1_001).unwrap(),
            ReservationOutcome::Reserved
        );
    }

    #[test]
    fn separate_process_views_share_one_exclusive_reservation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let attempt = target(AgentProvider::Antigravity);
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            let attempt = attempt.clone();
            workers.push(std::thread::spawn(move || {
                let store = RecoveryReservationStore::at(path, Duration::from_secs(10));
                barrier.wait();
                store.reserve(&attempt, 1_000).unwrap()
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == ReservationOutcome::Reserved)
                .count(),
            1
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| **outcome == ReservationOutcome::Duplicate)
                .count(),
            1
        );
    }

    #[test]
    fn stop_hook_and_tui_projection_share_the_native_attempt_key() {
        let temp = tempfile::tempdir().unwrap();
        let session = process_session(AgentProvider::Claude);
        let live = session.live_process_identity().unwrap();
        let links = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
        links
            .append(SessionIdentityLink {
                schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                recorded_at_ms: 1_000,
                provider: AgentProvider::Claude,
                native_session_id: "native-stop".into(),
                live_process: live,
            })
            .unwrap();
        let projection = links.read_projection().unwrap();
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Claude,
            "native-stop".into(),
            Some("turn-1".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        let mut lifecycle = LifecycleSnapshot::default();
        lifecycle.apply(
            LifecycleEvent::from_parts(identity.clone(), LifecycleEventKind::UserPromptSubmit)
                .unwrap(),
            1_000,
        );
        lifecycle.apply(
            LifecycleEvent::from_parts(identity, LifecycleEventKind::Stop).unwrap(),
            1_001,
        );

        let (tui_target, process_only) =
            recovery_target_for_session(&session, 42, Some(&lifecycle), Some(&projection)).unwrap();
        let hook_attempt = RecoveryAttemptKey {
            session: AgentSessionKey::native(AgentProvider::Claude, "native-stop"),
            epoch: RecoveryEpoch::LifecycleSequence(2),
        };
        assert!(!process_only);
        assert_eq!(tui_target.attempt, hook_attempt);

        let mut advanced_session = session.clone();
        advanced_session.last_message_ts += 1;
        advanced_session.status = SessionStatus::Processing;
        let (advanced_target, _) =
            recovery_target_for_session(&advanced_session, 42, Some(&lifecycle), Some(&projection))
                .unwrap();
        assert_ne!(advanced_target, tui_target);
        assert_eq!(advanced_target.last_message_ts, 1_001);
        assert_eq!(advanced_target.status, SessionStatus::Processing);

        let reservations = RecoveryReservationStore::at(
            temp.path().join("activity.jsonl"),
            Duration::from_secs(10),
        );
        assert_eq!(
            reservations.reserve(&tui_target, 2_000).unwrap(),
            ReservationOutcome::Reserved
        );
        assert_eq!(
            reservations.reserve(&tui_target, 2_001).unwrap(),
            ReservationOutcome::Duplicate
        );
    }

    #[test]
    fn native_link_without_matching_latest_stop_never_falls_back_to_process_authority() {
        let temp = tempfile::tempdir().unwrap();
        let session = process_session(AgentProvider::Claude);
        let live = session.live_process_identity().unwrap();
        let links = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
        links
            .append(SessionIdentityLink {
                schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                recorded_at_ms: 1_000,
                provider: AgentProvider::Claude,
                native_session_id: "native-stop".into(),
                live_process: live,
            })
            .unwrap();
        let projection = links.read_projection().unwrap();

        assert!(
            recovery_target_for_session(&session, 42, None, Some(&projection)).is_none(),
            "link-first window must not create synthetic authority"
        );

        let identity = LifecycleIdentity::try_new(
            AgentProvider::Claude,
            "native-stop".into(),
            Some("turn-1".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        let mut lifecycle = LifecycleSnapshot::default();
        lifecycle.apply(
            LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap(),
            1_001,
        );
        assert!(
            recovery_target_for_session(&session, 42, Some(&lifecycle), Some(&projection))
                .is_none(),
            "in-progress native lifecycle must not create synthetic authority"
        );
    }

    #[test]
    fn poll_refresh_resolves_one_current_live_session_and_expires_stale_status_or_timestamp() {
        let mut initial_session = process_session(AgentProvider::Claude);
        initial_session.status = SessionStatus::WaitingInput;
        let (initial_target, _) =
            recovery_target_for_session(&initial_session, 42, None, None).unwrap();
        let work = RecoveryPollWork {
            session: initial_session.clone(),
            target: initial_target.clone(),
        };
        let mut current = initial_session.clone();
        current.status = SessionStatus::Processing;
        current.last_message_ts += 1;

        let refreshed =
            refresh_poll_evidence_from(&work, vec![current], None, None, |_| Ok(42)).unwrap();
        assert_eq!(refreshed.target.status, SessionStatus::Processing);
        assert_eq!(refreshed.target.last_message_ts, 1_001);

        let sends = std::cell::Cell::new(0);
        let outcome = execute_recovery_with(
            BrainGateMode::Auto,
            initial_target,
            0.60,
            || Ok(suggestion(0.91)),
            || Ok(refreshed.target.clone()),
            |_| Ok(ReservationOutcome::Reserved),
            |_| Ok(()),
            |_| {
                sends.set(sends.get() + 1);
                Ok(())
            },
            |_| Ok(()),
        );
        assert_eq!(outcome, RecoveryExecution::Abstained);
        assert_eq!(sends.get(), 0);

        assert!(resolve_current_poll_session(&work, Vec::new()).is_err());
        assert!(
            resolve_current_poll_session(&work, vec![initial_session.clone(), initial_session])
                .is_err()
        );
    }

    #[test]
    fn process_attention_requires_a_recognized_prompt_and_deduplicates() {
        let temp = tempfile::tempdir().unwrap();
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let session = process_session(AgentProvider::Antigravity);

        let unrecognized =
            scan_recovery_sessions(vec![session.clone()], None, None, &activity, |_| {
                Err("unsupported prompt".into())
            });
        assert!(unrecognized.is_empty());
        assert!(activity.read().unwrap().events().is_empty());

        for _ in 0..2 {
            let recognized =
                scan_recovery_sessions(vec![session.clone()], None, None, &activity, |_| {
                    Ok((42, true))
                });
            assert_eq!(recognized.len(), 1);
        }
        let events = activity.read().unwrap();
        assert_eq!(events.events().len(), 1);
        let anchor = &events.events()[0];
        assert_eq!(anchor.state, ActivityState::Abstained);
        assert_eq!(anchor.tool.as_deref(), Some("agent_prompt"));
        assert_eq!(
            anchor.reasoning.as_deref(),
            Some("recognized actionable agent prompt")
        );
        assert_eq!(
            anchor.session.as_ref().map(|target| target.provider),
            Some(AgentProvider::Antigravity)
        );
        assert!(
            anchor
                .session
                .as_ref()
                .is_some_and(|target| target.session_id.starts_with("live:"))
        );

        let permission_activity = ActivityStore::at(temp.path().join("permission-activity.jsonl"));
        for _ in 0..2 {
            let work = scan_recovery_sessions(
                vec![session.clone()],
                None,
                None,
                &permission_activity,
                |_| Ok((43, false)),
            );
            assert!(work.is_empty());
        }
        assert_eq!(permission_activity.read().unwrap().events().len(), 1);
    }

    #[test]
    fn antigravity_continue_envelope_is_exact_bounded_and_redacted() {
        let envelope = antigravity_continue_envelope(
            "token=private-value ééé local model selected continuation",
        );
        assert_eq!(envelope["decision"], "continue");
        assert!(envelope.get("permissionOverrides").is_none());
        assert!(envelope.get("message").is_none());
        let reason = envelope["reason"].as_str().unwrap();
        assert!(reason.len() <= MAX_RECOVERY_REASON_BYTES);
        assert!(!reason.contains("private-value"));
        assert!(reason.is_char_boundary(reason.len()));
    }

    #[test]
    fn recovery_poll_tick_is_nonblocking_while_discovery_runs_off_thread() {
        let blocked = Arc::new(Barrier::new(2));
        let worker = Arc::clone(&blocked);
        let coordinator = RecoveryCoordinator::with_workers(
            Arc::new(move || {
                worker.wait();
                Vec::new()
            }),
            Arc::new(|_| None),
        );

        let started = Instant::now();
        assert!(coordinator.poll().is_empty());
        assert!(started.elapsed() < Duration::from_millis(100));
        blocked.wait();
    }

    #[test]
    fn recovery_poll_queue_is_bounded_deduplicated_and_has_two_workers() {
        let scan_count = Arc::new(AtomicUsize::new(0));
        let scans = Arc::clone(&scan_count);
        let (scan_tx, scan_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_release = Arc::clone(&release);
        let evaluations = Arc::new(AtomicUsize::new(0));
        let evaluation_count = Arc::clone(&evaluations);
        let (evaluation_tx, evaluation_rx) = mpsc::channel();
        let coordinator = RecoveryCoordinator::with_workers(
            Arc::new(move || {
                scans.fetch_add(1, Ordering::SeqCst);
                let mut work = (1..=65).map(poll_work).collect::<Vec<_>>();
                work.push(poll_work(1));
                let _ = scan_tx.send(());
                work
            }),
            Arc::new(move |_| {
                evaluation_count.fetch_add(1, Ordering::SeqCst);
                let _ = evaluation_tx.send(());
                let (lock, ready) = &*worker_release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = ready.wait(released).unwrap();
                }
                None
            }),
        );

        assert!(coordinator.poll().is_empty());
        scan_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let messages = loop {
            let messages = coordinator.poll();
            if !messages.is_empty() {
                break messages;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(
            messages,
            vec!["Recovery polling saturated; candidate skipped"]
        );
        evaluation_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        evaluation_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(evaluations.load(Ordering::SeqCst), 2);

        assert!(coordinator.poll().is_empty());
        assert_eq!(evaluations.load(Ordering::SeqCst), 2);
        assert_eq!(scan_count.load(Ordering::SeqCst), 1);
        let state = coordinator.state.lock().unwrap();
        assert_eq!(state.active_workers, 2);
        assert_eq!(state.inflight.len(), 2);
        assert_eq!(state.queue.len(), 62);
        drop(state);

        let (lock, ready) = &*release;
        *lock.lock().unwrap() = true;
        ready.notify_all();
    }

    #[test]
    fn auto_recovery_sends_only_after_stable_evidence_reservation_and_audits() {
        let original = target(AgentProvider::Antigravity);
        let sends = std::cell::Cell::new(0);
        let audits = std::cell::RefCell::new(Vec::new());

        let outcome = execute_recovery_with(
            BrainGateMode::Auto,
            original.clone(),
            0.60,
            || Ok(suggestion(0.91)),
            || Ok(original.clone()),
            |_| Ok(ReservationOutcome::Reserved),
            |state| {
                audits.borrow_mut().push(state);
                Ok(())
            },
            |_| {
                sends.set(sends.get() + 1);
                Ok(())
            },
            |_| Ok(()),
        );

        assert_eq!(outcome, RecoveryExecution::Continued);
        assert_eq!(sends.get(), 1);
        assert_eq!(
            audits.into_inner(),
            vec![ActivityState::Evaluating, ActivityState::Delivered]
        );
    }

    #[test]
    fn antigravity_recovery_emits_only_after_final_stable_preflight() {
        let original = target(AgentProvider::Antigravity);
        let deliveries = std::cell::Cell::new(0);
        let snapshots = std::cell::Cell::new(0);
        let outcome = execute_antigravity_recovery_with(
            BrainGateMode::Auto,
            original.clone(),
            0.60,
            || Ok(suggestion(0.91)),
            || {
                let call = snapshots.get();
                snapshots.set(call + 1);
                let mut current = original.clone();
                if call == 1 {
                    current.prompt_fingerprint = Some(99);
                }
                Ok(current)
            },
            |_| Ok(ReservationOutcome::Reserved),
            |_| Ok(()),
            |_| {
                deliveries.set(deliveries.get() + 1);
                Ok(())
            },
        );

        assert_eq!(outcome, RecoveryExecution::Abstained);
        assert_eq!(deliveries.get(), 0);
    }

    #[test]
    fn antigravity_stable_auto_recovery_emits_once_and_audits_delivery() {
        let original = target(AgentProvider::Antigravity);
        let deliveries = std::cell::Cell::new(0);
        let audits = std::cell::RefCell::new(Vec::new());
        let outcome = execute_antigravity_recovery_with(
            BrainGateMode::Auto,
            original.clone(),
            0.60,
            || Ok(suggestion(0.91)),
            || Ok(original.clone()),
            |_| Ok(ReservationOutcome::Reserved),
            |state| {
                audits.borrow_mut().push(state);
                Ok(())
            },
            |_| {
                deliveries.set(deliveries.get() + 1);
                Ok(())
            },
        );

        assert_eq!(outcome, RecoveryExecution::Continued);
        assert_eq!(deliveries.get(), 1);
        assert_eq!(
            audits.into_inner(),
            vec![ActivityState::Evaluating, ActivityState::Delivered]
        );
    }

    #[test]
    fn antigravity_non_auto_and_failure_paths_emit_nothing() {
        for failure in [
            AntigravityFailure::Off,
            AntigravityFailure::On,
            AntigravityFailure::LowConfidence,
            AntigravityFailure::Inference,
            AntigravityFailure::Reservation,
            AntigravityFailure::Audit,
            AntigravityFailure::Delivery,
        ] {
            let original = target(AgentProvider::Antigravity);
            let emissions = std::cell::Cell::new(0);
            let mode = match failure {
                AntigravityFailure::Off => BrainGateMode::Off,
                AntigravityFailure::On => BrainGateMode::On,
                _ => BrainGateMode::Auto,
            };
            let outcome = execute_antigravity_recovery_with(
                mode,
                original.clone(),
                0.60,
                || match failure {
                    AntigravityFailure::Inference => Err("model failure".into()),
                    AntigravityFailure::LowConfidence => Ok(suggestion(0.59)),
                    _ => Ok(suggestion(0.91)),
                },
                || Ok(original.clone()),
                |_| match failure {
                    AntigravityFailure::Reservation => Err("lock failure".into()),
                    _ => Ok(ReservationOutcome::Reserved),
                },
                |_| match failure {
                    AntigravityFailure::Audit => Err("audit failure".into()),
                    _ => Ok(()),
                },
                |_| match failure {
                    AntigravityFailure::Delivery => Err("write failure".into()),
                    _ => {
                        emissions.set(emissions.get() + 1);
                        Ok(())
                    }
                },
            );

            assert_eq!(outcome, RecoveryExecution::Abstained, "{failure:?}");
            assert_eq!(emissions.get(), 0, "{failure:?}");
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AntigravityFailure {
        Off,
        On,
        LowConfidence,
        Inference,
        Reservation,
        Audit,
        Delivery,
    }

    #[test]
    fn recovery_failures_never_send() {
        let original = target(AgentProvider::Antigravity);
        for failure in [
            RecoveryFailurePoint::Inference,
            RecoveryFailurePoint::EvidenceAfterProposal,
            RecoveryFailurePoint::Reservation,
            RecoveryFailurePoint::PreSendAudit,
            RecoveryFailurePoint::EvidenceBeforeSend,
            RecoveryFailurePoint::Delivery,
            RecoveryFailurePoint::PostflightEvidence,
            RecoveryFailurePoint::PostSendAudit,
        ] {
            let sends = std::cell::Cell::new(0);
            let snapshots = std::cell::Cell::new(0);
            let audits = std::cell::Cell::new(0);
            let outcome = execute_recovery_with(
                BrainGateMode::Auto,
                original.clone(),
                0.60,
                || {
                    if failure == RecoveryFailurePoint::Inference {
                        Err("model raw secret".into())
                    } else {
                        Ok(suggestion(0.91))
                    }
                },
                || {
                    let call = snapshots.get();
                    snapshots.set(call + 1);
                    let mut current = original.clone();
                    if (failure == RecoveryFailurePoint::EvidenceAfterProposal && call == 0)
                        || (failure == RecoveryFailurePoint::EvidenceBeforeSend && call == 1)
                    {
                        current.prompt_fingerprint = Some(99);
                    }
                    Ok(current)
                },
                |_| {
                    if failure == RecoveryFailurePoint::Reservation {
                        Err("lock failed".into())
                    } else {
                        Ok(ReservationOutcome::Reserved)
                    }
                },
                |state| {
                    let call = audits.get();
                    audits.set(call + 1);
                    if (failure == RecoveryFailurePoint::PreSendAudit && call == 0)
                        || (failure == RecoveryFailurePoint::PostSendAudit && call == 1)
                    {
                        Err("persistence failed".into())
                    } else {
                        assert!(matches!(
                            state,
                            ActivityState::Evaluating
                                | ActivityState::Delivered
                                | ActivityState::DeliveryFailed
                        ));
                        Ok(())
                    }
                },
                |_| {
                    sends.set(sends.get() + 1);
                    if failure == RecoveryFailurePoint::Delivery {
                        Err(RecoveryDeliveryFailure::Failed)
                    } else {
                        Ok(())
                    }
                },
                |_| {
                    if failure == RecoveryFailurePoint::PostflightEvidence {
                        Err("postflight evidence changed".into())
                    } else {
                        Ok(())
                    }
                },
            );

            assert_eq!(outcome, RecoveryExecution::Abstained, "{failure:?}");
            let expected_sends = usize::from(matches!(
                failure,
                RecoveryFailurePoint::Delivery
                    | RecoveryFailurePoint::PostflightEvidence
                    | RecoveryFailurePoint::PostSendAudit
            ));
            assert_eq!(sends.get(), expected_sends, "{failure:?}");
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum RecoveryFailurePoint {
        Inference,
        EvidenceAfterProposal,
        Reservation,
        PreSendAudit,
        EvidenceBeforeSend,
        Delivery,
        PostflightEvidence,
        PostSendAudit,
    }

    #[test]
    fn post_send_uncertainty_never_claims_failure_or_delivery() {
        let original = target(AgentProvider::Claude);
        let audits = std::cell::RefCell::new(Vec::new());

        let outcome = execute_recovery_with(
            BrainGateMode::Auto,
            original.clone(),
            0.60,
            || Ok(suggestion(0.91)),
            || Ok(original.clone()),
            |_| Ok(ReservationOutcome::Reserved),
            |state| {
                audits.borrow_mut().push(state);
                Ok(())
            },
            |_| Err(RecoveryDeliveryFailure::Unknown),
            |_| Ok(()),
        );

        assert_eq!(outcome, RecoveryExecution::Abstained);
        assert_eq!(audits.into_inner(), vec![ActivityState::Evaluating]);
    }
}
