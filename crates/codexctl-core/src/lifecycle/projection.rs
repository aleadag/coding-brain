use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::input::{
    LifecycleEvent, LifecycleEventKind, LifecycleEventName, PermissionDisposition, ProjectedStatus,
    SessionStartSource,
};

pub const LIFECYCLE_SCHEMA_VERSION: u32 = 1;
pub const MAX_RECENT_TURNS: usize = 32;
pub const MAX_ACTIVE_SUBAGENTS: usize = 64;

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
    }
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
        let session_id = event.identity().session_id().to_owned();
        let signature = EventSignature {
            turn_id: event.identity().turn_id().map(str::to_owned),
            kind: event.kind().clone(),
        };
        let state = self
            .sessions
            .entry(session_id)
            .or_insert_with(|| SessionLifecycleState::new(&event));

        if state.last_signature.as_ref() == Some(&signature) {
            return state.ignore(IgnoreReason::Duplicate);
        }

        if let LifecycleEventKind::SessionStart { source } = event.kind() {
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            state.cwd = event.identity().cwd().to_path_buf();
            state.transcript_path = event.identity().transcript_path().map(PathBuf::from);
            state.clear_transient_status();
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
            LifecycleEventKind::UserPromptSubmit
            | LifecycleEventKind::PreToolUse
            | LifecycleEventKind::PostToolUse => state.set_status(
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
            "session-1".into(),
            Some(turn.into()),
            None,
            "/work/codexctl".into(),
        )
        .unwrap();
        LifecycleEvent::permission(identity, disposition).unwrap()
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
        let state = snapshot.sessions.get("session-1").unwrap();
        assert_eq!(state.current_turn.as_deref(), Some("turn-2"));
        assert!(state.recent_turns.iter().any(|turn| turn == "turn-1"));
    }

    #[test]
    fn subagent_stop_is_idempotent_and_does_not_close_parent() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(subagent_start("turn-1", "agent-1"), 2_000);
        let status_time = snapshot.sessions["session-1"].status_received_at_ms;
        snapshot.apply(subagent_stop("turn-1", "agent-1"), 3_000);
        assert_eq!(
            snapshot.apply(subagent_stop("turn-1", "agent-1"), 4_000),
            ApplyOutcome::Ignored(IgnoreReason::Duplicate)
        );
        let state = snapshot.sessions.get("session-1").unwrap();
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
        assert_eq!(snapshot.sessions["session-1"].latest_received_at_ms, 1_000);
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
            snapshot.sessions["session-1"].projected_status,
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
        let state = &snapshot.sessions["session-1"];
        assert_eq!(state.latest_event, Some(LifecycleEventName::PreToolUse));
        assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));
    }

    #[test]
    fn recent_turn_guard_retains_only_the_latest_32_turns() {
        let mut snapshot = LifecycleSnapshot::default();
        for index in 0..34 {
            snapshot.apply(prompt(&format!("turn-{index}")), index + 1);
        }
        let state = &snapshot.sessions["session-1"];
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
        assert_eq!(snapshot.sessions["session-1"].active_subagents.len(), 64);
    }

    #[test]
    fn session_start_clears_transient_state_but_keeps_recent_turns() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(prompt("turn-2"), 2_000);
        snapshot.apply(event(LifecycleEventName::SessionStart, None, None), 3_000);
        let state = &snapshot.sessions["session-1"];
        assert_eq!(state.current_turn, None);
        assert!(!state.turn_open);
        assert_eq!(state.projected_status, None);
        assert!(state.recent_turns.iter().any(|turn| turn == "turn-1"));
        assert_eq!(
            state.session_start_source,
            Some(SessionStartSource::Startup)
        );
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
            assert_eq!(snapshot.sessions["session-1"].projected_status, expected);
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
        let state = &snapshot.sessions["session-1"];
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
