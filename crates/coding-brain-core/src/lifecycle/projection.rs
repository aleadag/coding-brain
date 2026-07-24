use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::provider::{AgentProvider, AgentSessionKey};

use super::input::{
    LifecycleEvent, LifecycleEventKind, LifecycleEventName, PermissionDisposition, ProjectedStatus,
    SessionStartSource,
};

pub const LIFECYCLE_SCHEMA_VERSION: u32 = 2;
pub const MAX_RECENT_TURNS: usize = 32;
pub const MAX_ACTIVE_SUBAGENTS: usize = 64;
pub const MAX_ANTIGRAVITY_INVOCATION_STEPS: usize = 256;
const ANTIGRAVITY_PERMISSION_DECIDED_BIT: u8 = 1 << 0;
const ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT: u8 = 1 << 1;
const ANTIGRAVITY_PRE_TOOL_BIT: u8 = 1 << 2;
const ANTIGRAVITY_POST_TOOL_BIT: u8 = 1 << 3;
pub(crate) const ANTIGRAVITY_CHILD_BITS: u8 = ANTIGRAVITY_PERMISSION_DECIDED_BIT
    | ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT
    | ANTIGRAVITY_PRE_TOOL_BIT
    | ANTIGRAVITY_POST_TOOL_BIT;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IgnoreReason {
    Duplicate,
    RecentTurn,
    AmbiguousTurn,
    ActiveSubagentCapacity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    Ignored(IgnoreReason),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveSubagentState {
    pub started_sequence: u64,
    pub received_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct EventSignature {
    turn_id: Option<String>,
    kind: LifecycleEventKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionLifecycleState {
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub current_turn: Option<String>,
    pub turn_open: bool,
    pub recent_turns: VecDeque<String>,
    pub latest_event: Option<LifecycleEventName>,
    pub latest_sequence: u64,
    pub latest_received_at_ms: u64,
    pub status_event: Option<LifecycleEventName>,
    pub status_sequence: Option<u64>,
    pub status_received_at_ms: Option<u64>,
    pub projected_status: Option<ProjectedStatus>,
    pub active_subagents: BTreeMap<String, ActiveSubagentState>,
    pub session_start_source: Option<SessionStartSource>,
    pub ignored_reason: Option<IgnoreReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub antigravity_initial_step: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub antigravity_child_events: BTreeMap<u64, u8>,
    last_signature: Option<EventSignature>,
}

impl SessionLifecycleState {
    fn new(event: &LifecycleEvent) -> Self {
        Self {
            cwd: event.identity().cwd().to_path_buf(),
            transcript_path: event.identity().transcript_path().map(PathBuf::from),
            current_turn: None,
            turn_open: false,
            recent_turns: VecDeque::new(),
            latest_event: None,
            latest_sequence: 0,
            latest_received_at_ms: 0,
            status_event: None,
            status_sequence: None,
            status_received_at_ms: None,
            projected_status: None,
            active_subagents: BTreeMap::new(),
            session_start_source: None,
            ignored_reason: None,
            antigravity_initial_step: None,
            antigravity_child_events: BTreeMap::new(),
            last_signature: None,
        }
    }

    fn ignore(&mut self, reason: IgnoreReason) -> ApplyOutcome {
        self.ignored_reason = Some(reason);
        ApplyOutcome::Ignored(reason)
    }

    fn remember_turn(&mut self, turn_id: &str) {
        if let Some(position) = self.recent_turns.iter().position(|turn| turn == turn_id) {
            self.recent_turns.remove(position);
        }
        self.recent_turns.push_back(turn_id.to_owned());
        while self.recent_turns.len() > MAX_RECENT_TURNS {
            self.recent_turns.pop_front();
        }
    }

    fn set_status(
        &mut self,
        event: LifecycleEventName,
        status: ProjectedStatus,
        sequence: u64,
        received_at_ms: u64,
    ) {
        self.status_event = Some(event);
        self.status_sequence = Some(sequence);
        self.status_received_at_ms = Some(received_at_ms);
        self.projected_status = Some(status);
    }

    fn clear_transient_status(&mut self) {
        if let Some(turn) = self.current_turn.take() {
            self.remember_turn(&turn);
        }
        self.turn_open = false;
        self.status_event = None;
        self.status_sequence = None;
        self.status_received_at_ms = None;
        self.projected_status = None;
        self.active_subagents.clear();
        self.antigravity_initial_step = None;
        self.antigravity_child_events.clear();
    }
}

fn prefixed_index(value: &str, prefix: &str) -> Option<u64> {
    value.strip_prefix(prefix)?.parse().ok()
}

fn antigravity_child_bit(kind: &LifecycleEventKind) -> Option<u8> {
    match kind {
        LifecycleEventKind::PermissionRequest {
            disposition: PermissionDisposition::Decided,
        } => Some(ANTIGRAVITY_PERMISSION_DECIDED_BIT),
        LifecycleEventKind::PermissionRequest {
            disposition: PermissionDisposition::NeedsInput,
        } => Some(ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT),
        LifecycleEventKind::PreToolUse => Some(ANTIGRAVITY_PRE_TOOL_BIT),
        LifecycleEventKind::PostToolUse => Some(ANTIGRAVITY_POST_TOOL_BIT),
        _ => None,
    }
}

fn antigravity_child(
    state: &SessionLifecycleState,
    event: &LifecycleEvent,
    turn_id: &str,
) -> Option<(u64, u8)> {
    if event.identity().provider() != AgentProvider::Antigravity
        || !state.turn_open
        || state
            .current_turn
            .as_deref()
            .and_then(|turn| prefixed_index(turn, "invocation-"))
            .is_none()
    {
        return None;
    }
    let step = prefixed_index(turn_id, "step-")?;
    let floor = state.antigravity_initial_step?;
    let bit = antigravity_child_bit(event.kind())?;
    (step >= floor).then_some((step, bit))
}

fn is_antigravity_child_candidate(event: &LifecycleEvent, turn_id: &str) -> bool {
    event.identity().provider() == AgentProvider::Antigravity
        && prefixed_index(turn_id, "step-").is_some()
        && antigravity_child_bit(event.kind()).is_some()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleSnapshot {
    pub schema_version: u32,
    pub next_sequence: u64,
    pub sessions: BTreeMap<String, SessionLifecycleState>,
}

impl Default for LifecycleSnapshot {
    fn default() -> Self {
        Self {
            schema_version: LIFECYCLE_SCHEMA_VERSION,
            next_sequence: 1,
            sessions: BTreeMap::new(),
        }
    }
}

impl LifecycleSnapshot {
    pub fn apply(&mut self, event: LifecycleEvent, received_at_ms: u64) -> ApplyOutcome {
        let session_key =
            AgentSessionKey::native(event.identity().provider(), event.identity().session_id())
                .storage_key();
        let signature = EventSignature {
            turn_id: event.identity().turn_id().map(str::to_owned),
            kind: event.kind().clone(),
        };
        let state = self
            .sessions
            .entry(session_key)
            .or_insert_with(|| SessionLifecycleState::new(&event));

        if state.last_signature.as_ref() == Some(&signature) {
            return state.ignore(IgnoreReason::Duplicate);
        }

        if let LifecycleEventKind::SessionStart { source } = event.kind() {
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            state.cwd = event.identity().cwd().to_path_buf();
            state.transcript_path = event.identity().transcript_path().map(PathBuf::from);
            if *source != SessionStartSource::Compact {
                state.clear_transient_status();
            }
            state.session_start_source = Some(*source);
            accept_event(state, &event, signature, sequence, received_at_ms);
            return ApplyOutcome::Applied;
        }

        let turn_id = event
            .identity()
            .turn_id()
            .expect("validated turn-scoped lifecycle event");
        if state.recent_turns.iter().any(|recent| recent == turn_id) {
            return state.ignore(IgnoreReason::RecentTurn);
        }

        if let Some((step, bit)) = antigravity_child(state, &event, turn_id) {
            let previous = state
                .antigravity_child_events
                .get(&step)
                .copied()
                .unwrap_or(0);
            let unsafe_permission_reversal = bit == ANTIGRAVITY_PERMISSION_DECIDED_BIT
                && previous & ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT != 0;
            if previous & bit != 0 || unsafe_permission_reversal {
                return state.ignore(IgnoreReason::Duplicate);
            }
            if previous == 0
                && state.antigravity_child_events.len() >= MAX_ANTIGRAVITY_INVOCATION_STEPS
            {
                return state.ignore(IgnoreReason::AmbiguousTurn);
            }
            state.antigravity_child_events.insert(step, previous | bit);
        } else if is_antigravity_child_candidate(&event, turn_id) {
            return state.ignore(IgnoreReason::AmbiguousTurn);
        } else {
            match state.current_turn.as_deref() {
                Some(current) if state.turn_open && current != turn_id => {
                    if !matches!(event.kind(), LifecycleEventKind::UserPromptSubmit) {
                        return state.ignore(IgnoreReason::AmbiguousTurn);
                    }
                    let current = current.to_owned();
                    state.remember_turn(&current);
                    state.current_turn = Some(turn_id.to_owned());
                }
                Some(current) if !state.turn_open && current == turn_id => {
                    return state.ignore(IgnoreReason::RecentTurn);
                }
                Some(current) if current != turn_id => {
                    state.current_turn = Some(turn_id.to_owned());
                }
                None => state.current_turn = Some(turn_id.to_owned()),
                _ => {}
            }
        }

        match event.kind() {
            LifecycleEventKind::SubagentStart { agent_id }
                if state.active_subagents.contains_key(agent_id) =>
            {
                return state.ignore(IgnoreReason::Duplicate);
            }
            LifecycleEventKind::SubagentStart { .. }
                if state.active_subagents.len() >= MAX_ACTIVE_SUBAGENTS =>
            {
                return state.ignore(IgnoreReason::ActiveSubagentCapacity);
            }
            LifecycleEventKind::SubagentStop { agent_id }
                if !state.active_subagents.contains_key(agent_id) =>
            {
                return state.ignore(IgnoreReason::Duplicate);
            }
            _ => {}
        }

        let sequence = self.next_sequence;
        self.next_sequence += 1;
        state.cwd = event.identity().cwd().to_path_buf();
        state.transcript_path = event.identity().transcript_path().map(PathBuf::from);
        state.turn_open = true;
        state.session_start_source = None;

        match event.kind() {
            LifecycleEventKind::UserPromptSubmit => {
                state.antigravity_initial_step = (event.identity().provider()
                    == AgentProvider::Antigravity)
                    .then(|| event.turn_initial_step())
                    .flatten();
                state.antigravity_child_events.clear();
                state.set_status(
                    event.name(),
                    ProjectedStatus::Processing,
                    sequence,
                    received_at_ms,
                );
            }
            LifecycleEventKind::PreToolUse | LifecycleEventKind::PostToolUse => state.set_status(
                event.name(),
                ProjectedStatus::Processing,
                sequence,
                received_at_ms,
            ),
            LifecycleEventKind::PermissionRequest { disposition } => state.set_status(
                event.name(),
                match disposition {
                    PermissionDisposition::Decided => ProjectedStatus::Processing,
                    PermissionDisposition::NeedsInput => ProjectedStatus::NeedsInput,
                },
                sequence,
                received_at_ms,
            ),
            LifecycleEventKind::SubagentStart { agent_id } => {
                state.active_subagents.insert(
                    agent_id.clone(),
                    ActiveSubagentState {
                        started_sequence: sequence,
                        received_at_ms,
                    },
                );
                state.set_status(
                    event.name(),
                    ProjectedStatus::Processing,
                    sequence,
                    received_at_ms,
                );
            }
            LifecycleEventKind::SubagentStop { agent_id } => {
                state.active_subagents.remove(agent_id);
            }
            LifecycleEventKind::Stop => {
                state.turn_open = false;
                state.active_subagents.clear();
                state.antigravity_initial_step = None;
                state.antigravity_child_events.clear();
                state.remember_turn(turn_id);
                state.set_status(
                    event.name(),
                    ProjectedStatus::Idle,
                    sequence,
                    received_at_ms,
                );
            }
            LifecycleEventKind::SessionStart { .. } => unreachable!(),
        }

        accept_event(state, &event, signature, sequence, received_at_ms);
        ApplyOutcome::Applied
    }
}

fn accept_event(
    state: &mut SessionLifecycleState,
    event: &LifecycleEvent,
    signature: EventSignature,
    sequence: u64,
    received_at_ms: u64,
) {
    state.latest_event = Some(event.name());
    state.latest_sequence = sequence;
    state.latest_received_at_ms = received_at_ms;
    state.last_signature = Some(signature);
    state.ignored_reason = None;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use serde_json::{Map, Value, json};

    use super::super::input::LifecycleIdentity;
    use super::*;
    use crate::provider::{AgentProvider, AgentSessionKey};

    fn event(name: LifecycleEventName, turn: Option<&str>, agent: Option<&str>) -> LifecycleEvent {
        let mut raw = Map::from_iter([
            ("session_id".into(), json!("session-1")),
            ("cwd".into(), json!("/work/codexctl")),
            ("hook_event_name".into(), json!(name.as_str())),
        ]);
        if let Some(turn) = turn {
            raw.insert("turn_id".into(), json!(turn));
        }
        if let Some(agent) = agent {
            raw.insert("agent_id".into(), json!(agent));
        }
        if name == LifecycleEventName::SessionStart {
            raw.insert("source".into(), json!("startup"));
        }
        LifecycleEvent::parse(Value::Object(raw).to_string().as_bytes()).unwrap()
    }

    fn session_start(
        provider: AgentProvider,
        session_id: &str,
        cwd: &str,
        source: SessionStartSource,
    ) -> LifecycleEvent {
        let identity =
            LifecycleIdentity::try_new(provider, session_id.into(), None, None, cwd.into())
                .unwrap();
        LifecycleEvent::from_parts(identity, LifecycleEventKind::SessionStart { source }).unwrap()
    }

    fn session_key() -> String {
        AgentSessionKey::native(AgentProvider::Codex, "session-1").storage_key()
    }

    fn prompt(turn: &str) -> LifecycleEvent {
        event(LifecycleEventName::UserPromptSubmit, Some(turn), None)
    }

    fn pre_tool(turn: &str) -> LifecycleEvent {
        event(LifecycleEventName::PreToolUse, Some(turn), None)
    }

    fn post_tool(turn: &str) -> LifecycleEvent {
        event(LifecycleEventName::PostToolUse, Some(turn), None)
    }

    fn stop(turn: &str) -> LifecycleEvent {
        event(LifecycleEventName::Stop, Some(turn), None)
    }

    fn subagent_start(turn: &str, agent: &str) -> LifecycleEvent {
        event(LifecycleEventName::SubagentStart, Some(turn), Some(agent))
    }

    fn subagent_stop(turn: &str, agent: &str) -> LifecycleEvent {
        event(LifecycleEventName::SubagentStop, Some(turn), Some(agent))
    }

    fn permission(turn: &str, disposition: PermissionDisposition) -> LifecycleEvent {
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "session-1".into(),
            Some(turn.into()),
            None,
            "/work/codexctl".into(),
        )
        .unwrap();
        LifecycleEvent::permission(identity, disposition).unwrap()
    }

    fn antigravity_identity(turn: &str) -> LifecycleIdentity {
        LifecycleIdentity::try_new(
            AgentProvider::Antigravity,
            "agy-conversation-1".into(),
            Some(turn.into()),
            None,
            "/work/antigravity".into(),
        )
        .unwrap()
    }

    fn invocation(turn: &str, initial_step: u64) -> LifecycleEvent {
        LifecycleEvent::from_parts_with_turn_initial_step(
            antigravity_identity(turn),
            LifecycleEventKind::UserPromptSubmit,
            Some(initial_step),
        )
        .unwrap()
    }

    #[test]
    fn antigravity_steps_are_children_of_the_open_invocation() {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(invocation("invocation-1", 5), 1),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(
                LifecycleEvent::permission(
                    antigravity_identity("step-5"),
                    PermissionDisposition::Decided,
                )
                .unwrap(),
                2,
            ),
            ApplyOutcome::Applied
        );
        let key =
            AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
        assert_eq!(
            snapshot.sessions[&key].current_turn.as_deref(),
            Some("invocation-1")
        );
        assert!(snapshot.sessions[&key].turn_open);

        let stale = LifecycleEvent::permission(
            antigravity_identity("step-4"),
            PermissionDisposition::Decided,
        )
        .unwrap();
        assert_eq!(
            snapshot.apply(stale, 3),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );

        let close = LifecycleEvent::from_parts(
            antigravity_identity("invocation-1"),
            LifecycleEventKind::Stop,
        )
        .unwrap();
        assert_eq!(snapshot.apply(close, 4), ApplyOutcome::Applied);
        assert!(!snapshot.sessions[&key].turn_open);
    }

    #[test]
    fn antigravity_child_replay_and_permission_reversal_fail_safe() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(invocation("invocation-1", 5), 1);
        let decided = || {
            LifecycleEvent::permission(
                antigravity_identity("step-5"),
                PermissionDisposition::Decided,
            )
            .unwrap()
        };
        let needs_input = || {
            LifecycleEvent::permission(
                antigravity_identity("step-5"),
                PermissionDisposition::NeedsInput,
            )
            .unwrap()
        };
        let post_tool = || {
            LifecycleEvent::from_parts(
                antigravity_identity("step-5"),
                LifecycleEventKind::PostToolUse,
            )
            .unwrap()
        };

        assert_eq!(snapshot.apply(decided(), 2), ApplyOutcome::Applied);
        assert_eq!(snapshot.apply(post_tool(), 3), ApplyOutcome::Applied);
        assert_eq!(
            snapshot.apply(decided(), 4),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        assert_eq!(snapshot.apply(needs_input(), 5), ApplyOutcome::Applied);
        assert_eq!(
            snapshot.apply(post_tool(), 6),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        assert_eq!(
            snapshot.apply(decided(), 7),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
    }

    #[test]
    fn antigravity_child_capacity_does_not_prevent_invocation_closure() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(invocation("invocation-1", 0), 1);
        for step in 0..MAX_ANTIGRAVITY_INVOCATION_STEPS {
            let permission = LifecycleEvent::permission(
                antigravity_identity(&format!("step-{step}")),
                PermissionDisposition::Decided,
            )
            .unwrap();
            assert_eq!(
                snapshot.apply(permission, step as u64 + 2),
                ApplyOutcome::Applied
            );
        }
        let overflow = LifecycleEvent::permission(
            antigravity_identity(&format!("step-{MAX_ANTIGRAVITY_INVOCATION_STEPS}")),
            PermissionDisposition::Decided,
        )
        .unwrap();
        assert_eq!(
            snapshot.apply(overflow, 300),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );

        let close = LifecycleEvent::from_parts(
            antigravity_identity("invocation-1"),
            LifecycleEventKind::Stop,
        )
        .unwrap();
        assert_eq!(snapshot.apply(close, 301), ApplyOutcome::Applied);
        let key =
            AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
        assert_eq!(snapshot.sessions[&key].antigravity_initial_step, None);
        assert!(snapshot.sessions[&key].antigravity_child_events.is_empty());

        let after_close = LifecycleEvent::permission(
            antigravity_identity("step-300"),
            PermissionDisposition::Decided,
        )
        .unwrap();
        assert_eq!(
            snapshot.apply(after_close, 302),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );
    }

    #[test]
    fn unproven_antigravity_children_do_not_weaken_generic_turn_guards() {
        let mut antigravity = LifecycleSnapshot::default();
        let ordinary_turn = LifecycleEvent::from_parts_with_turn_initial_step(
            antigravity_identity("turn-1"),
            LifecycleEventKind::UserPromptSubmit,
            Some(0),
        )
        .unwrap();
        antigravity.apply(ordinary_turn, 1);
        let step = LifecycleEvent::from_parts(
            antigravity_identity("step-0"),
            LifecycleEventKind::PreToolUse,
        )
        .unwrap();
        assert_eq!(
            antigravity.apply(step, 2),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );

        let claude_identity = |turn: &str| {
            LifecycleIdentity::try_new(
                AgentProvider::Claude,
                "claude-session-1".into(),
                Some(turn.into()),
                None,
                "/work/claude".into(),
            )
            .unwrap()
        };
        let prompt = LifecycleEvent::from_parts(
            claude_identity("turn-1"),
            LifecycleEventKind::UserPromptSubmit,
        )
        .unwrap();
        let tool =
            LifecycleEvent::from_parts(claude_identity("turn-2"), LifecycleEventKind::PreToolUse)
                .unwrap();
        let mut claude = LifecycleSnapshot::default();
        claude.apply(prompt, 1);
        assert_eq!(
            claude.apply(tool, 2),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );
    }

    #[test]
    fn lifecycle_projection_is_provider_qualified() {
        let event = |provider| {
            let identity = LifecycleIdentity::try_new(
                provider,
                "same".into(),
                Some("turn-1".into()),
                None,
                "/work/codexctl".into(),
            )
            .unwrap();
            LifecycleEvent::permission(identity, PermissionDisposition::Decided).unwrap()
        };
        let mut snapshot = LifecycleSnapshot::default();

        snapshot.apply(event(AgentProvider::Codex), 1);
        snapshot.apply(event(AgentProvider::Claude), 2);

        assert_eq!(snapshot.sessions.len(), 2);
        assert!(
            snapshot
                .sessions
                .contains_key(&AgentSessionKey::native(AgentProvider::Codex, "same").storage_key())
        );
        assert!(
            snapshot.sessions.contains_key(
                &AgentSessionKey::native(AgentProvider::Claude, "same").storage_key()
            )
        );
    }

    #[test]
    fn only_user_prompt_can_supersede_an_open_turn() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        assert_eq!(
            snapshot.apply(pre_tool("turn-2"), 2_000),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );
        assert_eq!(
            snapshot.apply(prompt("turn-2"), 3_000),
            ApplyOutcome::Applied
        );
        let state = snapshot.sessions.get(&session_key()).unwrap();
        assert_eq!(state.current_turn.as_deref(), Some("turn-2"));
        assert!(state.recent_turns.iter().any(|turn| turn == "turn-1"));
    }

    #[test]
    fn subagent_stop_is_idempotent_and_does_not_close_parent() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(subagent_start("turn-1", "agent-1"), 2_000);
        let status_time = snapshot.sessions[&session_key()].status_received_at_ms;
        snapshot.apply(subagent_stop("turn-1", "agent-1"), 3_000);
        assert_eq!(
            snapshot.apply(subagent_stop("turn-1", "agent-1"), 4_000),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        let state = snapshot.sessions.get(&session_key()).unwrap();
        assert!(state.turn_open);
        assert!(state.active_subagents.is_empty());
        assert_eq!(state.status_received_at_ms, status_time);
        assert_eq!(state.latest_event, Some(LifecycleEventName::SubagentStop));
    }

    #[test]
    fn duplicate_events_do_not_consume_a_sequence() {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(prompt("turn-1"), 1_000),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(prompt("turn-1"), 2_000),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        assert_eq!(snapshot.next_sequence, 2);
        assert_eq!(
            snapshot.sessions[&session_key()].latest_received_at_ms,
            1_000
        );
    }

    #[test]
    fn delayed_events_for_recent_turns_are_ignored() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(prompt("turn-2"), 2_000);
        assert_eq!(
            snapshot.apply(stop("turn-1"), 3_000),
            ApplyOutcome::Ignored(IgnoreReason::RecentTurn)
        );
        assert_eq!(
            snapshot.sessions[&session_key()].projected_status,
            Some(ProjectedStatus::Processing)
        );
    }

    #[test]
    fn unknown_subagent_stop_does_not_change_parent_status() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(pre_tool("turn-1"), 1_000);
        assert_eq!(
            snapshot.apply(subagent_stop("turn-1", "missing"), 2_000),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        let state = &snapshot.sessions[&session_key()];
        assert_eq!(state.latest_event, Some(LifecycleEventName::PreToolUse));
        assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));
    }

    #[test]
    fn recent_turn_guard_retains_only_the_latest_32_turns() {
        let mut snapshot = LifecycleSnapshot::default();
        for index in 0..34 {
            snapshot.apply(prompt(&format!("turn-{index}")), index + 1);
        }
        let state = &snapshot.sessions[&session_key()];
        assert_eq!(state.recent_turns.len(), 32);
        assert_eq!(
            state.recent_turns.front().map(String::as_str),
            Some("turn-1")
        );
        assert_eq!(
            state.recent_turns.back().map(String::as_str),
            Some("turn-32")
        );
    }

    #[test]
    fn active_subagent_capacity_rejects_the_65th_agent() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1);
        for index in 0..64 {
            assert_eq!(
                snapshot.apply(
                    subagent_start("turn-1", &format!("agent-{index}")),
                    index + 2
                ),
                ApplyOutcome::Applied
            );
        }
        let next_sequence = snapshot.next_sequence;
        assert_eq!(
            snapshot.apply(subagent_start("turn-1", "agent-64"), 100),
            ApplyOutcome::Ignored(IgnoreReason::ActiveSubagentCapacity)
        );
        assert_eq!(snapshot.next_sequence, next_sequence);
        assert_eq!(snapshot.sessions[&session_key()].active_subagents.len(), 64);
    }

    #[test]
    fn compact_preserves_active_lifecycle_and_turn_guards() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(
            permission("turn-1", PermissionDisposition::NeedsInput),
            2_000,
        );
        snapshot.apply(subagent_start("turn-1", "agent-1"), 3_000);

        let before = snapshot.sessions[&session_key()].clone();
        assert_eq!(
            snapshot.apply(
                session_start(
                    AgentProvider::Codex,
                    "session-1",
                    "/work/after-compact",
                    SessionStartSource::Compact,
                ),
                4_000,
            ),
            ApplyOutcome::Applied
        );

        let state = &snapshot.sessions[&session_key()];
        assert_eq!(state.current_turn, before.current_turn);
        assert_eq!(state.turn_open, before.turn_open);
        assert_eq!(state.recent_turns, before.recent_turns);
        assert_eq!(state.status_event, before.status_event);
        assert_eq!(state.status_sequence, before.status_sequence);
        assert_eq!(state.status_received_at_ms, before.status_received_at_ms);
        assert_eq!(state.projected_status, before.projected_status);
        assert_eq!(state.active_subagents, before.active_subagents);
        assert_eq!(
            state.session_start_source,
            Some(SessionStartSource::Compact)
        );
        assert_eq!(state.cwd, PathBuf::from("/work/after-compact"));
        assert_eq!(state.latest_event, Some(LifecycleEventName::SessionStart));
        assert_eq!(state.latest_received_at_ms, 4_000);

        assert_eq!(
            snapshot.apply(permission("turn-1", PermissionDisposition::Decided), 5_000,),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(pre_tool("turn-2"), 6_000),
            ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
        );
    }

    #[test]
    fn compact_does_not_create_or_reopen_a_turn() {
        let compact = || {
            session_start(
                AgentProvider::Codex,
                "session-1",
                "/work/codexctl",
                SessionStartSource::Compact,
            )
        };

        let mut empty = LifecycleSnapshot::default();
        empty.apply(compact(), 1_000);
        let empty_state = &empty.sessions[&session_key()];
        assert_eq!(empty_state.current_turn, None);
        assert!(!empty_state.turn_open);

        let mut stopped = LifecycleSnapshot::default();
        stopped.apply(prompt("turn-1"), 1_000);
        stopped.apply(stop("turn-1"), 2_000);
        stopped.apply(compact(), 3_000);
        let stopped_state = &stopped.sessions[&session_key()];
        assert_eq!(stopped_state.current_turn.as_deref(), Some("turn-1"));
        assert!(!stopped_state.turn_open);
        assert!(
            stopped_state
                .recent_turns
                .iter()
                .any(|turn| turn == "turn-1")
        );
        assert_eq!(
            stopped.apply(pre_tool("turn-1"), 4_000),
            ApplyOutcome::Ignored(IgnoreReason::RecentTurn)
        );
    }

    #[test]
    fn consecutive_compact_events_remain_duplicates() {
        let mut snapshot = LifecycleSnapshot::default();
        let compact = || {
            session_start(
                AgentProvider::Codex,
                "session-1",
                "/work/codexctl",
                SessionStartSource::Compact,
            )
        };

        assert_eq!(snapshot.apply(compact(), 1_000), ApplyOutcome::Applied);
        let next_sequence = snapshot.next_sequence;
        assert_eq!(
            snapshot.apply(compact(), 2_000),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        assert_eq!(snapshot.next_sequence, next_sequence);
    }

    #[test]
    fn non_compact_session_starts_keep_full_reset_semantics() {
        for source in [
            SessionStartSource::Startup,
            SessionStartSource::Resume,
            SessionStartSource::Clear,
        ] {
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(prompt("turn-1"), 1_000);
            snapshot.apply(subagent_start("turn-1", "agent-1"), 2_000);
            snapshot.apply(
                session_start(AgentProvider::Codex, "session-1", "/work/codexctl", source),
                3_000,
            );

            let state = &snapshot.sessions[&session_key()];
            assert_eq!(state.current_turn, None);
            assert!(!state.turn_open);
            assert_eq!(state.projected_status, None);
            assert!(state.active_subagents.is_empty());
            assert!(state.recent_turns.iter().any(|turn| turn == "turn-1"));
            assert_eq!(state.session_start_source, Some(source));
        }
    }

    #[test]
    fn compact_preserves_provider_specific_correlation_state() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(invocation("invocation-1", 5), 1);
        snapshot.apply(
            LifecycleEvent::permission(
                antigravity_identity("step-5"),
                PermissionDisposition::Decided,
            )
            .unwrap(),
            2,
        );
        let key =
            AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
        let before = snapshot.sessions[&key].clone();

        snapshot.apply(
            session_start(
                AgentProvider::Antigravity,
                "agy-conversation-1",
                "/work/antigravity",
                SessionStartSource::Compact,
            ),
            3,
        );

        let state = &snapshot.sessions[&key];
        assert_eq!(state.current_turn, before.current_turn);
        assert_eq!(state.turn_open, before.turn_open);
        assert_eq!(
            state.antigravity_initial_step,
            before.antigravity_initial_step
        );
        assert_eq!(
            state.antigravity_child_events,
            before.antigravity_child_events
        );
    }

    #[test]
    fn compact_continuity_is_source_defined_across_providers() {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let identity = LifecycleIdentity::try_new(
                provider,
                "provider-session".into(),
                Some("turn-1".into()),
                None,
                "/work/provider".into(),
            )
            .unwrap();
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(
                LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap(),
                1,
            );
            snapshot.apply(
                session_start(
                    provider,
                    "provider-session",
                    "/work/provider",
                    SessionStartSource::Compact,
                ),
                2,
            );

            let key = AgentSessionKey::native(provider, "provider-session").storage_key();
            let state = &snapshot.sessions[&key];
            assert_eq!(state.current_turn.as_deref(), Some("turn-1"));
            assert!(state.turn_open);
            assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));
        }
    }

    #[test]
    fn every_event_has_the_approved_status_effect() {
        let cases = [
            (prompt("turn-1"), Some(ProjectedStatus::Processing)),
            (pre_tool("turn-1"), Some(ProjectedStatus::Processing)),
            (
                permission("turn-1", PermissionDisposition::Decided),
                Some(ProjectedStatus::Processing),
            ),
            (
                permission("turn-1", PermissionDisposition::NeedsInput),
                Some(ProjectedStatus::NeedsInput),
            ),
            (post_tool("turn-1"), Some(ProjectedStatus::Processing)),
            (
                subagent_start("turn-1", "agent-1"),
                Some(ProjectedStatus::Processing),
            ),
            (stop("turn-1"), Some(ProjectedStatus::Idle)),
        ];

        for (event, expected) in cases {
            let mut snapshot = LifecycleSnapshot::default();
            assert_eq!(snapshot.apply(event, 1_000), ApplyOutcome::Applied);
            assert_eq!(snapshot.sessions[&session_key()].projected_status, expected);
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ReferenceEvent {
        Prompt,
        PreTool,
        PostTool,
        Stop,
    }

    #[derive(Default)]
    struct ReferenceState {
        current_turn: Option<String>,
        turn_open: bool,
        recent_turns: VecDeque<String>,
        status: Option<ProjectedStatus>,
        last: Option<(ReferenceEvent, String)>,
        ignored: Option<IgnoreReason>,
    }

    impl ReferenceState {
        fn apply(&mut self, event: ReferenceEvent, turn: &str) -> ApplyOutcome {
            if self.last.as_ref() == Some(&(event, turn.to_owned())) {
                self.ignored = Some(IgnoreReason::Duplicate);
                return ApplyOutcome::Ignored(IgnoreReason::Duplicate);
            }
            if self.recent_turns.iter().any(|recent| recent == turn) {
                self.ignored = Some(IgnoreReason::RecentTurn);
                return ApplyOutcome::Ignored(IgnoreReason::RecentTurn);
            }
            match self.current_turn.as_deref() {
                Some(current) if self.turn_open && current != turn => {
                    if event != ReferenceEvent::Prompt {
                        self.ignored = Some(IgnoreReason::AmbiguousTurn);
                        return ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn);
                    }
                    self.recent_turns.push_back(current.to_owned());
                    self.current_turn = Some(turn.to_owned());
                }
                Some(current) if !self.turn_open && current == turn => {
                    self.ignored = Some(IgnoreReason::RecentTurn);
                    return ApplyOutcome::Ignored(IgnoreReason::RecentTurn);
                }
                _ if self.current_turn.as_deref() != Some(turn) => {
                    self.current_turn = Some(turn.to_owned());
                }
                _ => {}
            }

            self.turn_open = event != ReferenceEvent::Stop;
            self.status = Some(match event {
                ReferenceEvent::Stop => ProjectedStatus::Idle,
                _ => ProjectedStatus::Processing,
            });
            if event == ReferenceEvent::Stop {
                self.recent_turns.push_back(turn.to_owned());
            }
            self.last = Some((event, turn.to_owned()));
            self.ignored = None;
            ApplyOutcome::Applied
        }
    }

    fn assert_reference_sequence(sequence: &[(ReferenceEvent, &str)]) {
        let mut reference = ReferenceState::default();
        let mut snapshot = LifecycleSnapshot::default();
        for (index, (kind, turn)) in sequence.iter().enumerate() {
            let actual_event = match kind {
                ReferenceEvent::Prompt => prompt(turn),
                ReferenceEvent::PreTool => pre_tool(turn),
                ReferenceEvent::PostTool => post_tool(turn),
                ReferenceEvent::Stop => stop(turn),
            };
            assert_eq!(
                snapshot.apply(actual_event, index as u64 + 1),
                reference.apply(*kind, turn),
                "sequence: {sequence:?}"
            );
        }
        let state = &snapshot.sessions[&session_key()];
        assert_eq!(
            state.current_turn, reference.current_turn,
            "sequence: {sequence:?}"
        );
        assert_eq!(
            state.turn_open, reference.turn_open,
            "sequence: {sequence:?}"
        );
        assert_eq!(
            state.projected_status, reference.status,
            "sequence: {sequence:?}"
        );
        assert_eq!(
            state.ignored_reason, reference.ignored,
            "sequence: {sequence:?}"
        );
    }

    #[test]
    fn short_event_permutations_match_the_reference_model() {
        let choices = [
            (ReferenceEvent::Prompt, "turn-1"),
            (ReferenceEvent::Prompt, "turn-2"),
            (ReferenceEvent::PreTool, "turn-1"),
            (ReferenceEvent::PreTool, "turn-2"),
            (ReferenceEvent::PostTool, "turn-1"),
            (ReferenceEvent::PostTool, "turn-2"),
            (ReferenceEvent::Stop, "turn-1"),
            (ReferenceEvent::Stop, "turn-2"),
        ];
        for first in choices {
            assert_reference_sequence(&[first]);
            for second in choices {
                assert_reference_sequence(&[first, second]);
                for third in choices {
                    assert_reference_sequence(&[first, second, third]);
                }
            }
        }
    }
}
