#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
    ProjectEvidence, SessionTarget, bounded_activity_identifier, lossless_redacted_activity_text,
};
use coding_brain_core::lifecycle::{
    ApplyOutcome, LifecycleEvent, LifecycleStore, RecordedLifecycleEvent, coding_brain_state_root,
};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::ProjectIdentity;
use coding_brain_core::provider::AgentProvider;
use coding_brain_core::session_links::{
    SESSION_IDENTITY_LINK_SCHEMA_VERSION, SessionIdentityLink, SessionLinkStore,
};
use serde::Deserialize;
use serde_json::Value;

use crate::brain::UNSUPPORTED_PERMISSION_TOOL_REASON;
use crate::brain::activity::{ActivityLog, ActivityStore};
#[cfg(test)]
use crate::provider_hooks::normalized_outcome;
use crate::provider_hooks::{ParsedLifecycleHook, parse_lifecycle};

pub(crate) const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;
static LIFECYCLE_ACTIVITY_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
pub(crate) struct RecordedProviderHook {
    pub parsed: ParsedLifecycleHook,
    pub event: LifecycleEvent,
    pub outcome: ApplyOutcome,
    pub sequence: u64,
    pub recovery_link_persisted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HookInputError {
    Read,
    TooLarge,
}

impl fmt::Display for HookInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read => f.write_str("could not read stdin"),
            Self::TooLarge => f.write_str("input exceeds 65536 bytes"),
        }
    }
}

pub(crate) fn read_bounded_hook_input(mut reader: impl Read) -> Result<Vec<u8>, HookInputError> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|_| HookInputError::Read)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        Err(HookInputError::TooLarge)
    } else {
        Ok(input)
    }
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let mut diagnostic =
        coding_brain_core::brain_activity::redact_activity_text(&diagnostic.to_string());
    if diagnostic.len() > 200 {
        let mut boundary = 200;
        while !diagnostic.is_char_boundary(boundary) {
            boundary -= 1;
        }
        diagnostic.truncate(boundary);
        diagnostic.push('…');
    }
    let _ = writeln!(stderr, "coding-brain lifecycle hook: {diagnostic}");
}

pub(crate) fn run_with<R: Read, W: Write, E: Write>(
    stdin: R,
    _stdout: W,
    stderr: E,
    store: &LifecycleStore,
) {
    run_provider_with_activity(AgentProvider::Codex, stdin, stderr, store, None, None, None);
}

#[derive(Debug, Deserialize)]
struct RawLifecycleActivityInput {
    #[serde(default)]
    tool_input: Value,
}

struct LifecycleActivityInput {
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    normalized_command: Option<String>,
    outcome: Option<ActivityOutcome>,
}

impl LifecycleActivityInput {
    fn from_parsed(parsed: &ParsedLifecycleHook, raw: &[u8]) -> Self {
        let normalized_command = (parsed.tool_name.as_deref() == Some("Bash"))
            .then(|| {
                serde_json::from_slice::<RawLifecycleActivityInput>(raw)
                    .ok()?
                    .tool_input
                    .get("command")?
                    .as_str()
                    .filter(|command| !command.trim().is_empty())
                    .and_then(lossless_redacted_activity_text)
            })
            .flatten();
        Self {
            tool_name: parsed.tool_name.clone(),
            tool_use_id: parsed.tool_use_id.clone(),
            normalized_command,
            outcome: parsed.outcome,
        }
    }

    fn normalized_bash_command(&self) -> Option<String> {
        self.normalized_command.clone()
    }

    fn normalized_tool_use_id(&self) -> Option<String> {
        self.tool_use_id.as_deref().map(bounded_activity_identifier)
    }
}

enum Correlation {
    Outcome(ActivityEvent),
    Diagnostic {
        event: ActivityEvent,
        message: &'static str,
    },
    None,
}

pub(crate) fn run_with_activity<R: Read, W: Write, E: Write>(
    stdin: R,
    _stdout: W,
    stderr: E,
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
) {
    run_provider_with_activity(
        AgentProvider::Codex,
        stdin,
        stderr,
        store,
        activity,
        None,
        None,
    );
}

fn run_provider_with_activity<R: Read, E: Write>(
    provider: AgentProvider,
    stdin: R,
    stderr: E,
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
    session_links: Option<&SessionLinkStore>,
    antigravity_event: Option<&str>,
) {
    run_provider_with_activity_and_live_process(
        provider,
        stdin,
        stderr,
        store,
        activity,
        session_links,
        antigravity_event,
        crate::provider_hooks::live_parent_process(provider),
        crate::provider_hooks::revalidate_live_process,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_provider_with_activity_and_live_process<R: Read, E: Write>(
    provider: AgentProvider,
    stdin: R,
    mut stderr: E,
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
    session_links: Option<&SessionLinkStore>,
    antigravity_event: Option<&str>,
    live_process: Option<coding_brain_core::provider::LiveProcessIdentity>,
    revalidate_live_process: impl Fn(&coding_brain_core::provider::LiveProcessIdentity) -> bool,
) {
    let input = match read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let mut parsed = match parse_lifecycle(provider, antigravity_event, &input) {
        Ok(parsed) => parsed,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    parsed.live_process = live_process;
    let activity_input = LifecycleActivityInput::from_parsed(&parsed, &input);
    let event = match LifecycleEvent::from_parts_with_turn_initial_step(
        parsed.identity.clone(),
        parsed.event.clone(),
        parsed.turn_initial_step,
    ) {
        Ok(event) => event,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            return;
        }
    };
    let lifecycle_applied = match store.record(event.clone()) {
        Ok(ApplyOutcome::Applied) => true,
        Ok(ApplyOutcome::Ignored(reason)) => {
            write_diagnostic(&mut stderr, format!("lifecycle event ignored: {reason:?}"));
            false
        }
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            false
        }
    };
    if lifecycle_applied
        && let (Some(session_links), Some(live_process)) =
            (session_links, parsed.live_process.clone())
        && revalidate_live_process(&live_process)
        && let Err(error) =
            session_links.append(session_identity_link(&parsed.identity, live_process))
    {
        write_diagnostic(&mut stderr, error);
    }
    if let Some(activity) = activity
        && lifecycle_applied
    {
        let result = if event.name().as_str() == "PostToolUse" {
            let observation = match observation_event(&event, &activity_input) {
                Ok(observation) => observation,
                Err(error) => {
                    write_diagnostic(&mut stderr, error);
                    return;
                }
            };
            let mut correlation_message = None;
            let result = activity
                .append_from_snapshot(|log| {
                    let mut events = vec![observation];
                    match correlate_outcome(log, &event, &activity_input) {
                        Correlation::Outcome(outcome) => events.push(outcome),
                        Correlation::Diagnostic { event, message } => {
                            correlation_message = Some(message);
                            events.push(event);
                        }
                        Correlation::None => {}
                    }
                    events
                })
                .map_err(|error| error.to_string());
            if result.is_ok()
                && let Some(message) = correlation_message
            {
                write_diagnostic(&mut stderr, message);
            }
            result
        } else {
            append_observation(activity, &event, &activity_input)
        };
        if let Err(error) = result {
            write_diagnostic(&mut stderr, error);
        }
        let _ = activity.compact_if_needed();
    }
}

pub(crate) fn persist_provider_hook(
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    input: &[u8],
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
    session_links: Option<&SessionLinkStore>,
) -> Result<RecordedProviderHook, String> {
    persist_provider_hook_with_live_process(
        provider,
        antigravity_event,
        input,
        store,
        activity,
        session_links,
        crate::provider_hooks::live_parent_process(provider),
        crate::provider_hooks::revalidate_live_process,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist_provider_hook_with_live_process(
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    input: &[u8],
    store: &LifecycleStore,
    activity: Option<&ActivityStore>,
    session_links: Option<&SessionLinkStore>,
    live_process: Option<coding_brain_core::provider::LiveProcessIdentity>,
    revalidate_live_process: impl Fn(&coding_brain_core::provider::LiveProcessIdentity) -> bool,
) -> Result<RecordedProviderHook, String> {
    let mut parsed = match parse_lifecycle(provider, antigravity_event, input) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Err(error.to_string());
        }
    };
    parsed.live_process = live_process;
    let activity_input = LifecycleActivityInput::from_parsed(&parsed, input);
    let event = match LifecycleEvent::from_parts_with_turn_initial_step(
        parsed.identity.clone(),
        parsed.event.clone(),
        parsed.turn_initial_step,
    ) {
        Ok(event) => event,
        Err(error) => {
            return Err(error.to_string());
        }
    };
    let (recorded, recovery_link_persisted) = persist_recovery_event_in_order(
        requires_recovery_link(&event),
        || {
            store
                .record_with_sequence(event.clone())
                .map_err(|error| error.to_string())
        },
        || {
            let (Some(session_links), Some(live_process)) =
                (session_links, parsed.live_process.clone())
            else {
                return Ok(false);
            };
            if !revalidate_live_process(&live_process) {
                return Ok(false);
            }
            let native_session_id = linked_native_session_id(&parsed.identity).to_string();
            session_links
                .append(session_identity_link(
                    &parsed.identity,
                    live_process.clone(),
                ))
                .map_err(|error| error.to_string())?;
            let projection = session_links
                .read_projection()
                .map_err(|error| error.to_string())?;
            let native =
                coding_brain_core::provider::AgentSessionKey::native(provider, &native_session_id);
            Ok(
                projection.native_for(&live_process) == Some(native_session_id.as_str())
                    && projection.live_for(&native) == Some(&live_process),
            )
        },
    )?;
    let lifecycle_applied = recorded.outcome == ApplyOutcome::Applied;
    if let Some(activity) = activity
        && lifecycle_applied
    {
        let result = if event.name().as_str() == "PostToolUse" {
            let observation = observation_event(&event, &activity_input)?;
            activity
                .append_from_snapshot(|log| {
                    let mut events = vec![observation];
                    match correlate_outcome(log, &event, &activity_input) {
                        Correlation::Outcome(outcome) => events.push(outcome),
                        Correlation::Diagnostic { event, .. } => events.push(event),
                        Correlation::None => {}
                    }
                    events
                })
                .map_err(|error| error.to_string())
        } else {
            append_observation(activity, &event, &activity_input)
        };
        result?;
        let _ = activity.compact_if_needed();
    }
    Ok(RecordedProviderHook {
        parsed,
        event,
        outcome: recorded.outcome,
        sequence: recorded.sequence,
        recovery_link_persisted,
    })
}

fn requires_recovery_link(event: &LifecycleEvent) -> bool {
    event.name() == coding_brain_core::lifecycle::LifecycleEventName::Stop
        && event.identity().provider_session_id().is_none()
}

fn persist_recovery_event_in_order(
    requires_link: bool,
    publish_lifecycle: impl FnOnce() -> Result<RecordedLifecycleEvent, String>,
    persist_and_verify_link: impl FnOnce() -> Result<bool, String>,
) -> Result<(RecordedLifecycleEvent, bool), String> {
    let recorded = publish_lifecycle()?;
    if !requires_link || recorded.outcome != ApplyOutcome::Applied {
        return Ok((recorded, false));
    }
    let link_persisted = persist_and_verify_link()?;
    if !link_persisted {
        return Err("exact recovery identity link unavailable".into());
    }
    Ok((recorded, true))
}

fn append_observation(
    activity: &ActivityStore,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Result<(), String> {
    activity
        .append(observation_event(lifecycle, input)?)
        .map_err(|error| error.to_string())
}

fn observation_event(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Result<ActivityEvent, String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    let (session_id, provider_session_id) = activity_session_identity(lifecycle);
    Ok(ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Lifecycle,
        activity_id: format!(
            "lifecycle_{}_{}_{}",
            epoch_ms(),
            std::process::id(),
            LIFECYCLE_ACTIVITY_COUNTER.fetch_add(1, Ordering::Relaxed)
        ),
        recorded_at_ms: epoch_ms(),
        project: ProjectEvidence {
            project_id: project_id.clone(),
            cwd: cwd.clone(),
            label: None,
        },
        session: Some(SessionTarget {
            provider: lifecycle.identity().provider(),
            session_id,
            provider_session_id,
            turn_id: lifecycle.identity().turn_id().map(str::to_string),
            tool_use_id: input
                .tool_use_id
                .as_deref()
                .map(bounded_activity_identifier),
            project_id,
            cwd,
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        }),
        state: ActivityState::Abstained,
        tool: Some(lifecycle.name().as_str().into()),
        normalized_command: None,
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: None,
        decision_id: None,
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    })
}

fn activity_session_identity(lifecycle: &LifecycleEvent) -> (String, Option<String>) {
    match (lifecycle.identity().provider(), lifecycle.kind()) {
        (
            AgentProvider::Codex,
            coding_brain_core::lifecycle::LifecycleEventKind::SubagentStart { agent_id }
            | coding_brain_core::lifecycle::LifecycleEventKind::SubagentStop { agent_id },
        ) => (
            agent_id.clone(),
            Some(lifecycle.identity().session_id().to_owned()),
        ),
        _ => (
            lifecycle.identity().session_id().to_owned(),
            lifecycle
                .identity()
                .provider_session_id()
                .map(str::to_owned),
        ),
    }
}

fn matches_lifecycle_identity(
    session: &SessionTarget,
    identity: &coding_brain_core::lifecycle::LifecycleIdentity,
) -> bool {
    session.provider == identity.provider()
        && session.session_id == identity.session_id()
        && session.provider_session_id.as_deref() == identity.provider_session_id()
        && session.turn_id.as_deref() == identity.turn_id()
}

fn linked_native_session_id(identity: &coding_brain_core::lifecycle::LifecycleIdentity) -> &str {
    identity
        .provider_session_id()
        .unwrap_or_else(|| identity.session_id())
}

fn session_identity_link(
    identity: &coding_brain_core::lifecycle::LifecycleIdentity,
    live_process: coding_brain_core::provider::LiveProcessIdentity,
) -> SessionIdentityLink {
    SessionIdentityLink {
        schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
        recorded_at_ms: epoch_ms(),
        provider: identity.provider(),
        native_session_id: linked_native_session_id(identity).to_owned(),
        live_process,
    }
}

fn correlate_outcome(
    log: &ActivityLog,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
) -> Correlation {
    let identity = lifecycle.identity();
    let Some(tool_use_id) = input.normalized_tool_use_id() else {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: lifecycle event has no tool_use_id",
        );
    };

    let exact_activity_ids = unique_activity_ids(log.events().iter().filter(|event| {
        event.kind == ActivityKind::Decision
            && event.session.as_ref().is_some_and(|session| {
                matches_lifecycle_identity(session, identity)
                    && session.tool_use_id.as_deref() == Some(tool_use_id.as_str())
            })
    }));
    if !exact_activity_ids.is_empty() {
        if exact_activity_ids.len() != 1 {
            return diagnostic_correlation(
                lifecycle,
                input,
                "orphan outcome: exact lifecycle identity is ambiguous or ineligible",
            );
        }
        let exact_activity_id = &exact_activity_ids[0];
        if identity.provider() == AgentProvider::Antigravity
            && first_terminal_with_index(log, exact_activity_id).is_some_and(|(_, event)| {
                event
                    .session
                    .as_ref()
                    .is_some_and(|session| matches_lifecycle_identity(session, identity))
                    && event.state == ActivityState::Abstained
                    && event.reasoning.as_deref() == Some(UNSUPPORTED_PERMISSION_TOOL_REASON)
            })
        {
            return Correlation::None;
        }
        return correlate_candidates(
            log,
            lifecycle,
            input,
            exact_activity_ids,
            "orphan outcome: exact lifecycle identity is ambiguous or ineligible",
        );
    }

    if input.tool_name.as_deref() != Some("Bash") {
        return Correlation::None;
    }

    let has_any_decision = log
        .events()
        .iter()
        .any(|event| event.kind == ActivityKind::Decision);
    let has_provider_decision = log.events().iter().any(|event| {
        event.kind == ActivityKind::Decision
            && event
                .session
                .as_ref()
                .is_some_and(|session| matches_lifecycle_identity(session, identity))
    });
    if has_any_decision && !has_provider_decision {
        return Correlation::None;
    }

    let anchors = log
        .events()
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PreToolUse")
                && event.session.as_ref().is_some_and(|session| {
                    matches_lifecycle_identity(session, identity)
                        && session.tool_use_id.as_deref() == Some(tool_use_id.as_str())
                })
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if anchors.len() != 1 {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: PreToolUse anchor is missing or ambiguous",
        );
    }
    let pre_index = anchors[0];
    let next_pre_index = log.events()[pre_index + 1..]
        .iter()
        .position(|event| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PreToolUse")
                && event
                    .session
                    .as_ref()
                    .is_some_and(|session| matches_lifecycle_identity(session, identity))
        })
        .map_or(log.events().len(), |offset| pre_index + 1 + offset);
    let interval = &log.events()[pre_index + 1..next_pre_index];
    if !interval
        .iter()
        .any(|event| event.kind == ActivityKind::Decision)
    {
        return Correlation::None;
    }
    if next_pre_index < log.events().len() {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: PreToolUse interval overlaps a later tool",
        );
    }
    let Some(command) = input.normalized_bash_command() else {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: Bash command is not losslessly correlatable",
        );
    };
    let activity_ids =
        unique_activity_ids(interval.iter().enumerate().filter_map(|(offset, event)| {
            let index = pre_index + 1 + offset;
            (event.kind == ActivityKind::Decision
                && event.schema_version == ACTIVITY_SCHEMA_VERSION
                && event.state == ActivityState::Allowed
                && event.decision_id.is_some()
                && event.tool.as_deref() == Some("Bash")
                && event.normalized_command.as_deref() == Some(command.as_str())
                && event
                    .session
                    .as_ref()
                    .is_some_and(|session| matches_lifecycle_identity(session, identity))
                && first_allowed_terminal_with_index(log, &event.activity_id)
                    .is_some_and(|(first_index, _)| first_index == index))
            .then_some(event)
        }));
    if activity_ids.is_empty() {
        return diagnostic_correlation(
            lifecycle,
            input,
            "orphan outcome: no eligible Decision in the PreToolUse interval",
        );
    }
    correlate_candidates(
        log,
        lifecycle,
        input,
        activity_ids,
        "orphan outcome: Decision correlation is ambiguous or ineligible",
    )
}

fn unique_activity_ids<'a>(events: impl Iterator<Item = &'a ActivityEvent>) -> Vec<String> {
    let mut ids = Vec::new();
    for event in events {
        if !ids.contains(&event.activity_id) {
            ids.push(event.activity_id.clone());
        }
    }
    ids
}

fn correlate_candidates(
    log: &ActivityLog,
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    activity_ids: Vec<String>,
    diagnostic: &'static str,
) -> Correlation {
    let candidates = activity_ids
        .iter()
        .filter_map(|activity_id| {
            first_allowed_terminal(log, activity_id).filter(|event| {
                event.session.as_ref().is_some_and(|session| {
                    matches_lifecycle_identity(session, lifecycle.identity())
                })
            })
        })
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return diagnostic_correlation(lifecycle, input, diagnostic);
    }
    let matched = candidates[0];
    let post_id = input.normalized_tool_use_id();
    let outcome = input.outcome.unwrap_or(ActivityOutcome::Completed);
    let post_already_recorded = log.events().iter().any(|event| {
        event.state == ActivityState::Outcome
            && event.activity_id == matched.activity_id
            && event.outcome == Some(outcome)
            && event.session.as_ref().is_some_and(|session| {
                matches_lifecycle_identity(session, lifecycle.identity())
                    && session.tool_use_id.as_deref() == post_id.as_deref()
            })
    });
    if post_already_recorded {
        return Correlation::None;
    }
    Correlation::Outcome(outcome_event(matched, input, outcome))
}

fn first_allowed_terminal<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<&'a ActivityEvent> {
    first_allowed_terminal_with_index(log, activity_id).map(|(_, event)| event)
}

fn first_allowed_terminal_with_index<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<(usize, &'a ActivityEvent)> {
    first_terminal_with_index(log, activity_id)
        .filter(|(_, event)| event.state == ActivityState::Allowed && event.decision_id.is_some())
}

fn first_terminal_with_index<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<(usize, &'a ActivityEvent)> {
    log.events()
        .iter()
        .enumerate()
        .find(|(_, event)| event.activity_id == activity_id && event.state.is_terminal())
}

fn outcome_event(
    matched: &ActivityEvent,
    input: &LifecycleActivityInput,
    outcome: ActivityOutcome,
) -> ActivityEvent {
    let mut outcome = ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: matched.kind,
        activity_id: matched.activity_id.clone(),
        recorded_at_ms: epoch_ms(),
        project: matched.project.clone(),
        session: matched.session.clone(),
        state: ActivityState::Outcome,
        tool: matched.tool.clone(),
        normalized_command: None,
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: None,
        decision_id: matched.decision_id.clone(),
        outcome: Some(outcome),
        correction: None,
        note: None,
        supersedes: None,
    };
    if let Some(session) = &mut outcome.session {
        session.tool_use_id = input.normalized_tool_use_id();
    }
    outcome
}

fn diagnostic_correlation(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    diagnostic: &'static str,
) -> Correlation {
    match diagnostic_event(lifecycle, input, diagnostic) {
        Ok(event) => Correlation::Diagnostic {
            event,
            message: diagnostic,
        },
        Err(_) => Correlation::None,
    }
}

fn diagnostic_event(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    diagnostic: &'static str,
) -> Result<ActivityEvent, String> {
    let paths = current_paths().ok_or_else(|| "Coding Brain paths unavailable".to_string())?;
    let identity = ProjectIdentity::load(lifecycle.identity().cwd(), &paths)
        .map_err(|error| error.to_string())?;
    let project_id = identity.id().clone();
    let cwd = lifecycle.identity().cwd().to_path_buf();
    let (session_id, provider_session_id) = activity_session_identity(lifecycle);
    Ok(ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Diagnostic,
        activity_id: format!("orphan_{}_{}", epoch_ms(), std::process::id()),
        recorded_at_ms: epoch_ms(),
        project: ProjectEvidence {
            project_id: project_id.clone(),
            cwd: cwd.clone(),
            label: None,
        },
        session: Some(SessionTarget {
            provider: lifecycle.identity().provider(),
            session_id,
            provider_session_id,
            turn_id: lifecycle.identity().turn_id().map(str::to_string),
            tool_use_id: input.normalized_tool_use_id(),
            project_id,
            cwd,
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        }),
        state: ActivityState::Error,
        tool: input.tool_name.clone(),
        normalized_command: None,
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: Some(diagnostic.into()),
        decision_id: None,
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    })
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn activity_store() -> Option<ActivityStore> {
    current_paths().map(|paths| ActivityStore::at(paths.state_root().join("activity.jsonl")))
}

fn current_paths() -> Option<CodingBrainPaths> {
    let environment = PathEnvironment::new(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    CodingBrainPaths::resolve(&environment).ok()
}

pub(crate) fn run(provider: AgentProvider, antigravity_event: Option<&str>) {
    let state_root = coding_brain_state_root();
    let store = LifecycleStore::at(&state_root);
    let session_links = SessionLinkStore::at(state_root.join("session-links.jsonl"));
    let activity = activity_store();
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    run_provider_with_activity(
        provider,
        stdin.lock(),
        stderr.lock(),
        &store,
        activity.as_ref(),
        Some(&session_links),
        antigravity_event,
    );
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
        MAX_ACTIVITY_FIELD_BYTES, ProjectEvidence, SessionTarget, SnapshotLimits,
        bounded_redacted_activity_text,
    };
    use coding_brain_core::lifecycle::{
        IgnoreReason, LifecycleEventKind, LifecycleIdentity, LifecycleStore, MAX_SESSIONS,
        StoreCondition,
    };
    use coding_brain_core::project::ProjectId;
    use fs2::FileExt;

    use crate::brain::activity::ActivityStore;

    use super::*;

    const PROMPT: &[u8] = include_bytes!("../tests/fixtures/hooks/user-prompt-submit.json");

    fn decision_event(
        cwd: &Path,
        activity_id: &str,
        recorded_at_ms: u64,
        tool_use_id: Option<&str>,
        command: &str,
        state: ActivityState,
    ) -> ActivityEvent {
        let project_id = ProjectId::Temporary("project-1".into());
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: activity_id.into(),
            recorded_at_ms,
            project: ProjectEvidence {
                project_id: project_id.clone(),
                cwd: cwd.to_path_buf(),
                label: Some("project".into()),
            },
            session: Some(SessionTarget {
                provider: AgentProvider::Codex,
                session_id: "session-1".into(),
                provider_session_id: None,
                turn_id: Some("turn-1".into()),
                tool_use_id: tool_use_id.map(str::to_owned),
                project_id,
                cwd: cwd.to_path_buf(),
                provider_hints: Vec::new(),
                provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
            }),
            state,
            tool: Some("Bash".into()),
            normalized_command: Some(bounded_redacted_activity_text(command)),
            fingerprint: None,
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.6),
            reasoning: Some("safe".into()),
            decision_id: Some(format!("decision-{activity_id}")),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn hook_payload(
        cwd: &Path,
        event: &str,
        call: &str,
        command: &str,
        response: Option<Value>,
    ) -> Value {
        let mut value = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": cwd,
            "hook_event_name": event,
            "tool_name": "Bash",
            "tool_use_id": call,
            "tool_input": {"command": command}
        });
        if let Some(response) = response {
            value["tool_response"] = response;
        }
        value
    }

    fn seed_codex_child(lifecycle: &LifecycleStore, cwd: &Path, child_id: &str, turn_id: &str) {
        let root = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "root-1".into(),
            Some(turn_id.into()),
            None,
            cwd.to_path_buf(),
        )
        .unwrap();
        assert_eq!(
            lifecycle
                .record(
                    LifecycleEvent::from_parts(
                        root,
                        LifecycleEventKind::SubagentStart {
                            agent_id: child_id.into(),
                        },
                    )
                    .unwrap(),
                )
                .unwrap(),
            ApplyOutcome::Applied
        );
    }

    fn seed_active_tool(lifecycle: &LifecycleStore, provider: AgentProvider, cwd: &Path) {
        if provider == AgentProvider::Antigravity {
            let invocation = LifecycleIdentity::try_new(
                provider,
                "agy-conversation-1".into(),
                Some("invocation-1".into()),
                None,
                cwd.to_path_buf(),
            )
            .unwrap();
            assert_eq!(
                lifecycle
                    .record(
                        LifecycleEvent::from_parts_with_turn_initial_step(
                            invocation,
                            LifecycleEventKind::UserPromptSubmit,
                            Some(0),
                        )
                        .unwrap(),
                    )
                    .unwrap(),
                ApplyOutcome::Applied
            );
        }
        let identity = match provider {
            AgentProvider::Codex => LifecycleIdentity::try_new(
                provider,
                "session-1".into(),
                Some("turn-1".into()),
                None,
                cwd.to_path_buf(),
            ),
            AgentProvider::Antigravity => LifecycleIdentity::try_new(
                provider,
                "agy-conversation-1".into(),
                Some("step-5".into()),
                None,
                cwd.to_path_buf(),
            ),
            AgentProvider::Claude => unreachable!(),
        }
        .unwrap();
        assert_eq!(
            lifecycle
                .record(
                    LifecycleEvent::from_parts(identity, LifecycleEventKind::PreToolUse).unwrap()
                )
                .unwrap(),
            ApplyOutcome::Applied
        );
    }

    fn child_post_payload(cwd: &Path, child_id: &str, turn_id: &str) -> Vec<u8> {
        let mut payload: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/hooks/codex-child-post-tool-use.json"
        ))
        .unwrap();
        payload["cwd"] = serde_json::json!(cwd);
        payload["session_id"] = serde_json::json!("root-1");
        payload["agent_id"] = serde_json::json!(child_id);
        payload["turn_id"] = serde_json::json!(turn_id);
        payload["tool_use_id"] = serde_json::json!("call-1");
        payload["tool_input"] = serde_json::json!({"command": "cargo test"});
        serde_json::to_vec(&payload).unwrap()
    }

    fn child_pre_payload(cwd: &Path, child_id: &str, turn_id: &str) -> Vec<u8> {
        let mut payload: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/hooks/codex-child-pre-tool-use.json"
        ))
        .unwrap();
        payload["cwd"] = serde_json::json!(cwd);
        payload["session_id"] = serde_json::json!("root-1");
        payload["agent_id"] = serde_json::json!(child_id);
        payload["turn_id"] = serde_json::json!(turn_id);
        payload["tool_use_id"] = serde_json::json!("call-1");
        payload["tool_input"] = serde_json::json!({"command": "cargo test"});
        serde_json::to_vec(&payload).unwrap()
    }

    fn root_stop_payload(cwd: &Path, session_id: &str, turn_id: &str) -> Vec<u8> {
        let mut payload: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/hooks/stop.json")).unwrap();
        payload["cwd"] = serde_json::json!(cwd);
        payload["session_id"] = serde_json::json!(session_id);
        payload["turn_id"] = serde_json::json!(turn_id);
        serde_json::to_vec(&payload).unwrap()
    }

    fn root_event(
        cwd: &Path,
        session_id: &str,
        turn_id: &str,
        kind: LifecycleEventKind,
    ) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            LifecycleIdentity::try_new(
                AgentProvider::Codex,
                session_id.into(),
                Some(turn_id.into()),
                None,
                cwd.to_path_buf(),
            )
            .unwrap(),
            kind,
        )
        .unwrap()
    }

    fn assert_only_trusted_process_link(
        link_path: &Path,
        trusted_process: &coding_brain_core::provider::LiveProcessIdentity,
        claimed_process: &coding_brain_core::provider::LiveProcessIdentity,
    ) {
        let rows = fs::read_to_string(link_path).unwrap();
        let links = rows
            .lines()
            .map(|row| serde_json::from_str::<SessionIdentityLink>(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].native_session_id, "trusted-root");
        assert_eq!(&links[0].live_process, trusted_process);

        let projection = SessionLinkStore::at(link_path).read_projection().unwrap();
        let trusted_key = coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Codex,
            "trusted-root",
        );
        let claimed_key = coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Codex,
            "claimed-root",
        );
        assert_eq!(projection.native_for(trusted_process), Some("trusted-root"));
        assert_eq!(projection.live_for(&trusted_key), Some(trusted_process));
        assert_eq!(projection.native_for(claimed_process), None);
        assert_eq!(projection.live_for(&claimed_key), None);
    }

    fn invoke_activity_hook(
        lifecycle: &LifecycleStore,
        activity: &ActivityStore,
        payload: Value,
    ) -> String {
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(payload.to_string()),
            Vec::new(),
            &mut stderr,
            lifecycle,
            Some(activity),
        );
        String::from_utf8(stderr).unwrap()
    }

    fn outcome_and_diagnostic_counts(store: &ActivityStore) -> (usize, usize) {
        let events = store.read().unwrap().events().to_vec();
        (
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            events
                .iter()
                .filter(|event| event.kind == ActivityKind::Diagnostic)
                .count(),
        )
    }

    fn exact_decision_event(
        cwd: &Path,
        provider: AgentProvider,
        activity_id: &str,
        recorded_at_ms: u64,
        state: ActivityState,
        reason: &str,
    ) -> ActivityEvent {
        let tool_use_id = match provider {
            AgentProvider::Antigravity => "step-5",
            AgentProvider::Codex => "call-1",
            AgentProvider::Claude => unreachable!(),
        };
        let mut event = decision_event(
            cwd,
            activity_id,
            recorded_at_ms,
            Some(tool_use_id),
            "cargo test",
            state,
        );
        let session = event.session.as_mut().unwrap();
        session.provider = provider;
        if provider == AgentProvider::Antigravity {
            session.session_id = "agy-conversation-1".into();
            session.turn_id = Some("step-5".into());
        }
        event.reasoning = Some(reason.into());
        event
    }

    fn invoke_exact_post(
        provider: AgentProvider,
        cwd: &Path,
        lifecycle: &LifecycleStore,
        activity: &ActivityStore,
    ) {
        seed_active_tool(lifecycle, provider, cwd);
        match provider {
            AgentProvider::Codex => {
                invoke_activity_hook(
                    lifecycle,
                    activity,
                    hook_payload(
                        cwd,
                        "PostToolUse",
                        "call-1",
                        "cargo test",
                        Some(serde_json::json!({"exit_code": 0})),
                    ),
                );
            }
            AgentProvider::Antigravity => {
                let mut payload: Value = serde_json::from_slice(include_bytes!(
                    "../tests/fixtures/hooks/antigravity-post-tool-use.json"
                ))
                .unwrap();
                payload["workspacePaths"] = serde_json::json!([cwd]);
                persist_provider_hook(
                    AgentProvider::Antigravity,
                    Some("PostToolUse"),
                    &serde_json::to_vec(&payload).unwrap(),
                    lifecycle,
                    Some(activity),
                    None,
                )
                .unwrap();
            }
            AgentProvider::Claude => unreachable!(),
        }
    }

    fn exact_correlation_counts(
        provider: AgentProvider,
        rows: &[(&str, ActivityState, &str)],
    ) -> (usize, usize) {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        for (index, (activity_id, state, reason)) in rows.iter().enumerate() {
            activity
                .append(exact_decision_event(
                    temp.path(),
                    provider,
                    activity_id,
                    index as u64 + 1,
                    *state,
                    reason,
                ))
                .unwrap();
        }
        invoke_exact_post(provider, temp.path(), &lifecycle, &activity);
        outcome_and_diagnostic_counts(&activity)
    }

    fn assert_diagnostics_are_metadata_only(activity: &ActivityStore, forbidden: &[&str]) {
        let events = activity.read().unwrap().events().to_vec();
        for event in events
            .iter()
            .filter(|event| event.kind == ActivityKind::Diagnostic)
        {
            assert!(event.normalized_command.is_none());
            assert!(event.fingerprint.is_none());
            assert!(event.note.is_none());
        }
        let serialized = serde_json::to_string(&events).unwrap();
        for value in forbidden {
            assert!(
                !serialized.contains(value),
                "persisted forbidden value: {value}"
            );
        }
    }

    #[test]
    fn unsupported_exception_is_antigravity_only() {
        assert_eq!(
            exact_correlation_counts(
                AgentProvider::Codex,
                &[(
                    "activity-1",
                    ActivityState::Abstained,
                    UNSUPPORTED_PERMISSION_TOOL_REASON,
                )],
            ),
            (0, 1)
        );
    }

    #[test]
    fn unsupported_exception_requires_the_exact_reason() {
        assert_eq!(
            exact_correlation_counts(
                AgentProvider::Antigravity,
                &[(
                    "activity-1",
                    ActivityState::Abstained,
                    "Brain model mode is off",
                )],
            ),
            (0, 1)
        );
    }

    #[test]
    fn unsupported_exception_requires_one_exact_activity_id() {
        assert_eq!(
            exact_correlation_counts(
                AgentProvider::Antigravity,
                &[
                    (
                        "activity-1",
                        ActivityState::Abstained,
                        UNSUPPORTED_PERMISSION_TOOL_REASON,
                    ),
                    (
                        "activity-2",
                        ActivityState::Abstained,
                        UNSUPPORTED_PERMISSION_TOOL_REASON,
                    ),
                ],
            ),
            (0, 1)
        );
    }

    #[test]
    fn unsupported_exception_respects_first_terminal_state() {
        assert_eq!(
            exact_correlation_counts(
                AgentProvider::Antigravity,
                &[
                    ("activity-1", ActivityState::Denied, "model denied"),
                    (
                        "activity-1",
                        ActivityState::Abstained,
                        UNSUPPORTED_PERMISSION_TOOL_REASON,
                    ),
                ],
            ),
            (0, 1)
        );
        assert_eq!(
            exact_correlation_counts(
                AgentProvider::Antigravity,
                &[
                    (
                        "activity-1",
                        ActivityState::Abstained,
                        UNSUPPORTED_PERMISSION_TOOL_REASON,
                    ),
                    ("activity-1", ActivityState::Denied, "model denied"),
                ],
            ),
            (0, 0)
        );
    }

    #[test]
    fn outcome_classification_requires_explicit_structured_evidence() {
        let cases = [
            (
                serde_json::json!("opaque unified-exec response"),
                ActivityOutcome::Completed,
            ),
            (
                serde_json::json!({"exit_code": 0}),
                ActivityOutcome::Succeeded,
            ),
            (
                serde_json::json!({"success": true}),
                ActivityOutcome::Succeeded,
            ),
            (serde_json::json!({"exit_code": 7}), ActivityOutcome::Failed),
            (
                serde_json::json!({"is_error": true}),
                ActivityOutcome::Failed,
            ),
            (
                serde_json::json!({"cancelled": true}),
                ActivityOutcome::Cancelled,
            ),
            (
                serde_json::json!({"status": "cancelled"}),
                ActivityOutcome::Cancelled,
            ),
            (
                serde_json::json!({"message": "done"}),
                ActivityOutcome::Completed,
            ),
        ];
        for (response, expected) in cases {
            assert_eq!(normalized_outcome(Some(&response)), expected);
        }
    }

    #[test]
    fn bounded_reader_accepts_the_limit_and_rejects_one_more_byte() {
        let exact = vec![b' '; MAX_HOOK_INPUT_BYTES];
        assert_eq!(
            read_bounded_hook_input(Cursor::new(&exact)).unwrap().len(),
            MAX_HOOK_INPUT_BYTES
        );
        let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
        assert!(matches!(
            read_bounded_hook_input(Cursor::new(&oversized)),
            Err(HookInputError::TooLarge)
        ));
    }

    #[test]
    fn valid_event_records_state_without_protocol_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(store.read().unwrap().condition, StoreCondition::Healthy);
    }

    #[test]
    fn strict_stop_persistence_keeps_accepted_stop_without_exact_link() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/hooks/antigravity-stop.json"
        ))
        .unwrap();
        payload["workspacePaths"] = serde_json::json!([temp.path()]);

        let result = persist_provider_hook(
            AgentProvider::Antigravity,
            Some("Stop"),
            &serde_json::to_vec(&payload).unwrap(),
            &lifecycle,
            Some(&activity),
            None,
        );

        assert!(result.is_err());
        assert!(lifecycle.read().unwrap().snapshot.is_some());
        assert!(activity.read().unwrap().events().is_empty());
    }

    #[test]
    fn rejected_root_stops_do_not_replace_a_trusted_process_link() {
        for failure in ["io", "newer-schema", "capacity"] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle_path = temp.path().join("lifecycle");
            let lifecycle = LifecycleStore::at(&lifecycle_path);
            match failure {
                "io" => fs::write(&lifecycle_path, b"occupied").unwrap(),
                "newer-schema" => {
                    fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
                    fs::write(lifecycle.snapshot_path(), br#"{"schema_version":4}"#).unwrap();
                }
                "capacity" => {
                    for index in 0..MAX_SESSIONS {
                        assert_eq!(
                            lifecycle
                                .record(root_event(
                                    temp.path(),
                                    &format!("session-{index}"),
                                    "turn-a",
                                    LifecycleEventKind::PreToolUse,
                                ))
                                .unwrap(),
                            ApplyOutcome::Applied
                        );
                    }
                }
                _ => unreachable!(),
            }
            let link_path = temp.path().join("session-links.jsonl");
            let links = SessionLinkStore::at(&link_path);
            let trusted_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4100,
                9001,
                "pts/7",
            )
            .unwrap();
            let claimed_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4200,
                9002,
                "pts/8",
            )
            .unwrap();
            links
                .append(SessionIdentityLink {
                    schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                    recorded_at_ms: 1,
                    provider: AgentProvider::Codex,
                    native_session_id: "trusted-root".into(),
                    live_process: trusted_process.clone(),
                })
                .unwrap();

            let result = persist_provider_hook_with_live_process(
                AgentProvider::Codex,
                None,
                &root_stop_payload(temp.path(), "claimed-root", "turn-a"),
                &lifecycle,
                None,
                Some(&links),
                Some(claimed_process.clone()),
                |_| true,
            );

            assert!(result.is_err(), "failure={failure}");
            assert_only_trusted_process_link(&link_path, &trusted_process, &claimed_process);
        }
    }

    #[test]
    fn ignored_root_stops_do_not_replace_a_trusted_process_link() {
        for ignored in ["duplicate", "stale", "ambiguous"] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            if ignored != "ambiguous" {
                assert_eq!(
                    lifecycle
                        .record(root_event(
                            temp.path(),
                            "claimed-root",
                            "turn-a",
                            LifecycleEventKind::Stop,
                        ))
                        .unwrap(),
                    ApplyOutcome::Applied
                );
            }
            if ignored != "duplicate" {
                assert_eq!(
                    lifecycle
                        .record(root_event(
                            temp.path(),
                            "claimed-root",
                            "turn-b",
                            LifecycleEventKind::UserPromptSubmit,
                        ))
                        .unwrap(),
                    ApplyOutcome::Applied
                );
            }
            let link_path = temp.path().join("session-links.jsonl");
            let links = SessionLinkStore::at(&link_path);
            let trusted_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4100,
                9001,
                "pts/7",
            )
            .unwrap();
            let claimed_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4200,
                9002,
                "pts/8",
            )
            .unwrap();
            links
                .append(SessionIdentityLink {
                    schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                    recorded_at_ms: 1,
                    provider: AgentProvider::Codex,
                    native_session_id: "trusted-root".into(),
                    live_process: trusted_process.clone(),
                })
                .unwrap();

            let recorded = persist_provider_hook_with_live_process(
                AgentProvider::Codex,
                None,
                &root_stop_payload(temp.path(), "claimed-root", "turn-a"),
                &lifecycle,
                None,
                Some(&links),
                Some(claimed_process.clone()),
                |_| true,
            )
            .unwrap();

            assert_eq!(
                recorded.outcome,
                ApplyOutcome::Ignored(match ignored {
                    "duplicate" => IgnoreReason::Duplicate,
                    "stale" => IgnoreReason::RecentTurn,
                    "ambiguous" => IgnoreReason::AmbiguousTurn,
                    _ => unreachable!(),
                })
            );
            assert!(!recorded.recovery_link_persisted);
            assert_only_trusted_process_link(&link_path, &trusted_process, &claimed_process);
        }
    }

    #[test]
    fn accepted_root_stop_persists_lifecycle_before_exact_recovery_link() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let link_path = temp.path().join("session-links.jsonl");
        let links = SessionLinkStore::at(&link_path);
        let live_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
            AgentProvider::Codex,
            4200,
            9002,
            "pts/8",
        )
        .unwrap();

        let recorded = persist_provider_hook_with_live_process(
            AgentProvider::Codex,
            None,
            &root_stop_payload(temp.path(), "claimed-root", "turn-a"),
            &lifecycle,
            None,
            Some(&links),
            Some(live_process.clone()),
            |_| true,
        )
        .unwrap();

        assert_eq!(recorded.outcome, ApplyOutcome::Applied);
        assert!(recorded.recovery_link_persisted);
        let snapshot = lifecycle.read().unwrap().snapshot.unwrap();
        let root_key = coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Codex,
            "claimed-root",
        );
        let root = &snapshot.sessions[&root_key.storage_key()];
        assert_eq!(
            root.latest_event.as_ref().map(|event| event.as_str()),
            Some("Stop")
        );
        let rows = fs::read_to_string(&link_path).unwrap();
        assert_eq!(rows.lines().count(), 1);
        let projection = links.read_projection().unwrap();
        assert_eq!(projection.native_for(&live_process), Some("claimed-root"));
        assert_eq!(projection.live_for(&root_key), Some(&live_process));
    }

    #[test]
    fn strict_stop_persistence_publishes_lifecycle_before_verifying_link() {
        let order = std::cell::RefCell::new(Vec::new());
        let result = persist_recovery_event_in_order(
            true,
            || {
                order.borrow_mut().push("stop");
                Ok(RecordedLifecycleEvent {
                    outcome: ApplyOutcome::Applied,
                    sequence: 7,
                })
            },
            || {
                order.borrow_mut().push("link");
                Ok(true)
            },
        )
        .unwrap();
        order.borrow_mut().push("evaluation");

        assert_eq!(
            result,
            (
                RecordedLifecycleEvent {
                    outcome: ApplyOutcome::Applied,
                    sequence: 7,
                },
                true
            )
        );
        assert_eq!(order.into_inner(), vec!["stop", "link", "evaluation"]);

        let published = std::cell::Cell::new(false);
        assert!(
            persist_recovery_event_in_order(
                true,
                || {
                    published.set(true);
                    Ok(RecordedLifecycleEvent {
                        outcome: ApplyOutcome::Applied,
                        sequence: 7,
                    })
                },
                || Ok(false),
            )
            .is_err()
        );
        assert!(published.get());
    }

    #[test]
    fn linked_child_stop_publishes_without_attempting_recovery_link() {
        let temp = tempfile::tempdir().unwrap();
        let root_identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "root-session".into(),
            Some("turn-1".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        let child_identity = LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            "child-a".into(),
            Some("root-session".into()),
            Some("turn-1".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        let root_stop =
            LifecycleEvent::from_parts(root_identity, LifecycleEventKind::Stop).unwrap();
        let child_stop =
            LifecycleEvent::from_parts(child_identity, LifecycleEventKind::Stop).unwrap();

        assert!(requires_recovery_link(&root_stop));
        assert!(!requires_recovery_link(&child_stop));

        let link_attempted = std::cell::Cell::new(false);
        let published = std::cell::Cell::new(false);
        let result = persist_recovery_event_in_order(
            requires_recovery_link(&child_stop),
            || {
                published.set(true);
                Ok(RecordedLifecycleEvent {
                    outcome: ApplyOutcome::Applied,
                    sequence: 7,
                })
            },
            || {
                link_attempted.set(true);
                Ok(true)
            },
        )
        .unwrap();

        assert_eq!(
            result,
            (
                RecordedLifecycleEvent {
                    outcome: ApplyOutcome::Applied,
                    sequence: 7,
                },
                false
            )
        );
        assert!(!link_attempted.get());
        assert!(published.get());
    }

    #[test]
    fn claude_subagent_topology_audit_remains_parent_scoped() {
        let temp = tempfile::tempdir().unwrap();
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Claude,
            "claude-session".into(),
            Some("turn-1".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        let event = LifecycleEvent::from_parts(
            identity,
            LifecycleEventKind::SubagentStart {
                agent_id: "claude-child".into(),
            },
        )
        .unwrap();

        assert_eq!(
            activity_session_identity(&event),
            ("claude-session".into(), None)
        );
    }

    #[test]
    fn generic_lifecycle_hook_records_only_normalized_activity_identity() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "UserPromptSubmit",
            "prompt": "secret raw prompt must not persist"
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].state, ActivityState::Abstained);
        assert_eq!(events[0].tool.as_deref(), Some("UserPromptSubmit"));
        assert!(events[0].normalized_command.is_none());
        assert!(
            !fs::read_to_string(temp.path().join("activity.jsonl"))
                .unwrap()
                .contains("secret raw prompt")
        );
    }

    #[test]
    fn malformed_or_oversized_input_is_bounded_and_fail_open() {
        for input in [b"secret malformed payload".to_vec(), vec![b'x'; 65_537]] {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with(Cursor::new(input), &mut stdout, &mut stderr, &store);
            assert!(stdout.is_empty());
            let diagnostic = String::from_utf8(stderr).unwrap();
            assert!(diagnostic.starts_with("coding-brain lifecycle hook:"));
            assert!(diagnostic.len() < 256);
            assert!(!diagnostic.contains("secret"));
            assert!(!store.snapshot_path().exists());
        }
    }

    #[test]
    fn lifecycle_diagnostics_are_redacted_and_bounded() {
        let mut stderr = Vec::new();
        write_diagnostic(
            &mut stderr,
            format!("api_key=sk-secret-value {}", "x".repeat(1_024)),
        );
        let diagnostic = String::from_utf8(stderr).unwrap();
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(!diagnostic.contains("sk-secret-value"));
        assert!(diagnostic.len() < 256);
    }

    #[test]
    fn persistence_failure_and_newer_schema_leave_stdout_empty() {
        let temp = tempfile::tempdir().unwrap();
        let blocked_root = temp.path().join("blocked");
        fs::write(&blocked_root, b"occupied").unwrap();
        let blocked = LifecycleStore::at(&blocked_root);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &blocked);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());

        let store = LifecycleStore::at(temp.path().join("newer"));
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let newer = br#"{"schema_version":4}"#;
        fs::write(store.snapshot_path(), newer).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with(Cursor::new(PROMPT), &mut stdout, &mut stderr, &store);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("newer"));
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), newer);
    }

    #[test]
    fn post_tool_use_joins_outcome_by_stable_hook_ids() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: Some(0.9),
                threshold: Some(0.6),
                reasoning: Some("safe".into()),
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"exit_code": 0}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[2].activity_id, "activity-1");
        assert_eq!(events[2].decision_id.as_deref(), Some("decision-1"));
        assert_eq!(events[2].state, ActivityState::Outcome);
        assert_eq!(events[2].outcome, Some(ActivityOutcome::Succeeded));
        assert!(events[2].normalized_command.is_none());
    }

    #[test]
    fn exact_id_retries_are_idempotent_and_upgrade_v1_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let mut decision = decision_event(
            temp.path(),
            "activity-v1",
            1,
            Some("call-1"),
            "cargo test",
            ActivityState::Allowed,
        );
        decision.schema_version = 1;
        fs::write(
            &activity_path,
            format!("{}\n", serde_json::to_string(&decision).unwrap()),
        )
        .unwrap();
        let post = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "ignored by exact matching",
            Some(serde_json::json!({"exit_code": 0})),
        );

        seed_active_tool(&lifecycle, AgentProvider::Codex, temp.path());
        assert!(invoke_activity_hook(&lifecycle, &activity, post.clone()).is_empty());
        assert!(
            invoke_activity_hook(&lifecycle, &activity, post).contains("lifecycle event ignored")
        );

        let events = activity.read().unwrap().events().to_vec();
        let outcomes = events
            .iter()
            .filter(|event| event.state == ActivityState::Outcome)
            .collect::<Vec<_>>();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].activity_id, "activity-v1");
        assert_eq!(outcomes[0].schema_version, ACTIVITY_SCHEMA_VERSION);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
    }

    #[test]
    fn review_regression_exact_identity_is_ambiguous_before_eligibility_filtering() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append(decision_event(
                temp.path(),
                "activity-allowed",
                1,
                Some("call-1"),
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        activity
            .append(decision_event(
                temp.path(),
                "activity-denied",
                2,
                Some("call-1"),
                "cargo test",
                ActivityState::Denied,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn post_tool_use_does_not_join_another_providers_same_native_ids() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "claude-activity".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Claude,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: Some(0.9),
                threshold: Some(0.6),
                reasoning: Some("safe".into()),
                decision_id: Some("decision-claude".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_response": {"exit_code": 0}
        });

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            Vec::new(),
            &lifecycle,
            Some(&activity),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].activity_id, "claude-activity");
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(
            events[1].session.as_ref().unwrap().provider,
            AgentProvider::Codex
        );
    }

    #[test]
    fn post_tool_use_ignores_newer_lifecycle_observation_when_joining_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: Some(0.9),
                threshold: Some(0.6),
                reasoning: Some("safe".into()),
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let observation = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1"
        });
        let mut observation_stderr = Vec::new();
        run_with_activity(
            Cursor::new(observation.to_string()),
            Vec::new(),
            &mut observation_stderr,
            &lifecycle,
            Some(&activity),
        );
        assert!(observation_stderr.is_empty());
        let before_outcome = activity.read().unwrap().events().to_vec();
        assert_eq!(before_outcome.len(), 2);
        assert_eq!(before_outcome[0].kind, ActivityKind::Decision);
        assert_eq!(before_outcome[1].kind, ActivityKind::Lifecycle);

        let outcome = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_input": {"command": "cargo test"},
            "tool_response": {"exit_code": 0}
        });
        let mut outcome_stderr = Vec::new();
        run_with_activity(
            Cursor::new(outcome.to_string()),
            Vec::new(),
            &mut outcome_stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(outcome_stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].kind, ActivityKind::Lifecycle);
        assert_eq!(events[2].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[3].activity_id, "activity-1");
        assert_eq!(events[3].kind, ActivityKind::Decision);
        assert_eq!(events[3].decision_id.as_deref(), Some("decision-1"));
        assert_eq!(events[3].state, ActivityState::Outcome);
        assert_eq!(events[3].outcome, Some(ActivityOutcome::Succeeded));
    }

    #[test]
    fn post_tool_use_without_decision_activity_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        assert!(
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-orphan", "cargo test", None),
            )
            .is_empty()
        );
        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-orphan",
                "cargo test",
                Some(serde_json::json!({"exit_code": 1})),
            ),
        );

        assert!(stderr.is_empty());
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, ActivityKind::Lifecycle);
        assert_eq!(events[0].state, ActivityState::Abstained);
        assert!(events[0].decision_id.is_none());
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
    }

    #[test]
    fn no_decision_review_lossy_post_is_observation_only() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PreToolUse",
                "call-1",
                "curl --token alpha",
                None,
            ),
        );

        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "curl --token alpha",
                Some(serde_json::json!("opaque response")),
            ),
        );

        assert!(stderr.is_empty());
        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 0));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque response")
        );
    }

    #[test]
    fn no_decision_review_missing_anchor_is_metadata_only_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque response")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["opaque response"]);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
    }

    #[test]
    fn post_tool_use_with_incomplete_decision_activity_is_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Observed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: None,
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_response": {"exit_code": 0}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("orphan outcome")
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, ActivityKind::Decision);
        assert_eq!(events[0].state, ActivityState::Observed);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].tool.as_deref(), Some("PostToolUse"));
        assert_eq!(events[2].kind, ActivityKind::Diagnostic);
        assert_eq!(events[2].state, ActivityState::Error);
    }

    #[test]
    fn missing_tool_use_id_never_matches_missing_activity_id() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project-1".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: None,
                },
                session: Some(SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: None,
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: None,
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let input = serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": temp.path(),
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {"exit_code": 0}
        });
        let mut stderr = Vec::new();

        run_with_activity(
            Cursor::new(input.to_string()),
            Vec::new(),
            &mut stderr,
            &lifecycle,
            Some(&activity),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[2].state, ActivityState::Error);
        assert_ne!(events[2].activity_id, "activity-1");
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("no tool_use_id")
        );
    }

    #[test]
    fn persist_provider_hook_ignored_child_post_does_not_record_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        seed_codex_child(&lifecycle, temp.path(), "child-a", "turn-a");
        let stop = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "root-1".into(),
            Some("turn-a".into()),
            None,
            temp.path().to_path_buf(),
        )
        .unwrap();
        assert_eq!(
            lifecycle
                .record(
                    LifecycleEvent::from_parts(
                        stop,
                        LifecycleEventKind::SubagentStop {
                            agent_id: "child-a".into(),
                        },
                    )
                    .unwrap(),
                )
                .unwrap(),
            ApplyOutcome::Applied
        );

        let mut decision = decision_event(
            temp.path(),
            "activity-1",
            1,
            Some("call-1"),
            "cargo test",
            ActivityState::Allowed,
        );
        let session = decision.session.as_mut().unwrap();
        session.session_id = "child-a".into();
        session.provider_session_id = Some("root-1".into());
        session.turn_id = Some("turn-a".into());
        activity.append(decision).unwrap();

        let recorded = persist_provider_hook(
            AgentProvider::Codex,
            None,
            &child_post_payload(temp.path(), "child-a", "turn-a"),
            &lifecycle,
            Some(&activity),
            None,
        )
        .unwrap();

        assert_eq!(
            recorded.outcome,
            ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent)
        );
        assert_eq!(outcome_and_diagnostic_counts(&activity).0, 0);
    }

    #[test]
    fn persist_provider_hook_ignored_child_pre_does_not_record_lifecycle_observation() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));

        let recorded = persist_provider_hook(
            AgentProvider::Codex,
            None,
            &child_pre_payload(temp.path(), "child-a", "turn-a"),
            &lifecycle,
            Some(&activity),
            None,
        )
        .unwrap();

        assert_eq!(
            recorded.outcome,
            ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent)
        );
        assert!(
            !activity
                .read()
                .unwrap()
                .events()
                .iter()
                .any(|event| event.kind == ActivityKind::Lifecycle)
        );
    }

    #[test]
    fn root_and_sibling_callbacks_keep_one_root_process_link() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
        let live_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
            AgentProvider::Codex,
            4242,
            9001,
            "pts/7",
        )
        .expect("live process");
        let root = LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            "root-session".into(),
            Option::<String>::None,
            Option::<String>::None,
            Option::<PathBuf>::None,
            temp.path().to_path_buf(),
        )
        .expect("root identity");
        let child_a = LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            "child-a".into(),
            Some("root-session".into()),
            Option::<String>::None,
            Option::<PathBuf>::None,
            temp.path().to_path_buf(),
        )
        .expect("child identity");
        let child_b = LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            "child-b".into(),
            Some("root-session".into()),
            Option::<String>::None,
            Option::<PathBuf>::None,
            temp.path().to_path_buf(),
        )
        .expect("child identity");

        for identity in [&root, &child_a, &child_b] {
            store
                .append(session_identity_link(identity, live_process.clone()))
                .expect("append link");
        }

        let projection = store.read_projection().expect("read projection");
        let root_key = coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Codex,
            "root-session",
        );
        let child_a_key =
            coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "child-a");
        let child_b_key =
            coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "child-b");
        assert_eq!(
            projection.native_for(&live_process),
            Some("root-session"),
            "sibling callbacks must not remap a shared root process to the last child"
        );
        assert_eq!(projection.live_for(&root_key), Some(&live_process));
        assert_eq!(projection.live_for(&child_a_key), None);
        assert_eq!(projection.live_for(&child_b_key), None);
    }

    #[test]
    fn accepted_root_and_child_callbacks_publish_only_root_process_link() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let link_path = temp.path().join("session-links.jsonl");
        let links = SessionLinkStore::at(&link_path);
        let live_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
            AgentProvider::Codex,
            4242,
            9001,
            "pts/7",
        )
        .unwrap();
        let root_key =
            coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "root-1");
        let child_key =
            coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "child-a");
        let mut root_payload: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/hooks/subagent-start.json"
        ))
        .unwrap();
        root_payload["cwd"] = serde_json::json!(temp.path());
        root_payload["session_id"] = serde_json::json!("root-1");
        root_payload["turn_id"] = serde_json::json!("turn-a");
        root_payload["agent_id"] = serde_json::json!("child-a");
        let mut stderr = Vec::new();

        run_provider_with_activity_and_live_process(
            AgentProvider::Codex,
            Cursor::new(serde_json::to_vec(&root_payload).unwrap()),
            &mut stderr,
            &lifecycle,
            None,
            Some(&links),
            None,
            Some(live_process.clone()),
            |_| true,
        );

        assert!(stderr.is_empty());
        let snapshot = lifecycle.read().unwrap().snapshot.unwrap();
        let root = &snapshot.sessions[&root_key.storage_key()];
        assert!(root.active_subagents.contains_key("child-a"));
        assert_eq!(fs::read_to_string(&link_path).unwrap().lines().count(), 1);
        let projection = links.read_projection().unwrap();
        assert_eq!(projection.native_for(&live_process), Some("root-1"));
        assert_eq!(projection.live_for(&root_key), Some(&live_process));
        assert_eq!(projection.live_for(&child_key), None);

        run_provider_with_activity_and_live_process(
            AgentProvider::Codex,
            Cursor::new(child_pre_payload(temp.path(), "child-a", "turn-a")),
            &mut stderr,
            &lifecycle,
            None,
            Some(&links),
            None,
            Some(live_process.clone()),
            |_| true,
        );

        assert!(stderr.is_empty());
        let snapshot = lifecycle.read().unwrap().snapshot.unwrap();
        let child = &snapshot.sessions[&child_key.storage_key()];
        assert_eq!(child.provider_session_id.as_deref(), Some("root-1"));
        assert_eq!(child.current_turn.as_deref(), Some("turn-a"));
        assert_eq!(
            child.latest_event.as_ref().map(|event| event.as_str()),
            Some("PreToolUse")
        );
        let rows = fs::read_to_string(&link_path).unwrap();
        let links = rows
            .lines()
            .map(|row| serde_json::from_str::<SessionIdentityLink>(row).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(links.len(), 2);
        assert!(links.iter().all(|link| {
            link.native_session_id == "root-1" && link.live_process == live_process
        }));
        let projection = SessionLinkStore::at(&link_path).read_projection().unwrap();
        assert_eq!(projection.native_for(&live_process), Some("root-1"));
        assert_eq!(projection.live_for(&root_key), Some(&live_process));
        assert_eq!(projection.live_for(&child_key), None);
    }

    #[test]
    fn rejected_child_callbacks_do_not_publish_session_links() {
        for mismatch in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            if mismatch {
                seed_codex_child(&lifecycle, temp.path(), "child-a", "turn-a");
            }
            let link_path = temp.path().join("session-links.jsonl");
            let links = SessionLinkStore::at(&link_path);
            let live_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4242,
                9001,
                "pts/7",
            )
            .unwrap();
            let trusted_root = if mismatch { "root-1" } else { "trusted-root" };
            links
                .append(SessionIdentityLink {
                    schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                    recorded_at_ms: 1,
                    provider: AgentProvider::Codex,
                    native_session_id: trusted_root.into(),
                    live_process: live_process.clone(),
                })
                .unwrap();
            let mut payload: Value =
                serde_json::from_slice(&child_pre_payload(temp.path(), "child-a", "turn-a"))
                    .unwrap();
            let claimed_root = if mismatch { "other-root" } else { "root-1" };
            payload["session_id"] = serde_json::json!(claimed_root);
            let mut stderr = Vec::new();

            run_provider_with_activity_and_live_process(
                AgentProvider::Codex,
                Cursor::new(serde_json::to_vec(&payload).unwrap()),
                &mut stderr,
                &lifecycle,
                None,
                Some(&links),
                None,
                Some(live_process.clone()),
                |_| true,
            );

            assert!(!stderr.is_empty(), "mismatch={mismatch}");
            assert_eq!(fs::read_to_string(&link_path).unwrap().lines().count(), 1);
            let projection = links.read_projection().unwrap();
            let trusted_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                trusted_root,
            );
            let claimed_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                claimed_root,
            );
            let child_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                "child-a",
            );
            assert_eq!(projection.native_for(&live_process), Some(trusted_root));
            assert_eq!(projection.live_for(&trusted_key), Some(&live_process));
            assert_eq!(projection.live_for(&claimed_key), None);
            assert_eq!(projection.live_for(&child_key), None);
        }
    }

    #[test]
    fn lifecycle_store_errors_do_not_publish_session_links() {
        for newer_schema in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle_path = temp.path().join("lifecycle");
            if newer_schema {
                let lifecycle = LifecycleStore::at(&lifecycle_path);
                fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
                fs::write(lifecycle.snapshot_path(), br#"{"schema_version":4}"#).unwrap();
            } else {
                fs::write(&lifecycle_path, b"occupied").unwrap();
            }
            let lifecycle = LifecycleStore::at(&lifecycle_path);
            let link_path = temp.path().join("session-links.jsonl");
            let links = SessionLinkStore::at(&link_path);
            let live_process = coding_brain_core::provider::LiveProcessIdentity::try_new(
                AgentProvider::Codex,
                4242,
                9001,
                "pts/7",
            )
            .unwrap();
            links
                .append(SessionIdentityLink {
                    schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                    recorded_at_ms: 1,
                    provider: AgentProvider::Codex,
                    native_session_id: "trusted-root".into(),
                    live_process: live_process.clone(),
                })
                .unwrap();
            let mut stderr = Vec::new();

            run_provider_with_activity_and_live_process(
                AgentProvider::Codex,
                Cursor::new(child_pre_payload(temp.path(), "child-a", "turn-a")),
                &mut stderr,
                &lifecycle,
                None,
                Some(&links),
                None,
                Some(live_process.clone()),
                |_| true,
            );

            assert!(!stderr.is_empty(), "newer_schema={newer_schema}");
            assert_eq!(fs::read_to_string(&link_path).unwrap().lines().count(), 1);
            let projection = links.read_projection().unwrap();
            let trusted_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                "trusted-root",
            );
            let claimed_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                "root-1",
            );
            let child_key = coding_brain_core::provider::AgentSessionKey::native(
                AgentProvider::Codex,
                "child-a",
            );
            assert_eq!(projection.native_for(&live_process), Some("trusted-root"));
            assert_eq!(projection.live_for(&trusted_key), Some(&live_process));
            assert_eq!(projection.live_for(&claimed_key), None);
            assert_eq!(projection.live_for(&child_key), None);
        }
    }

    #[test]
    fn post_tool_use_after_lifecycle_persistence_failure_does_not_record_outcome() {
        for newer_schema in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle_path = temp.path().join("lifecycle");
            if newer_schema {
                let store = LifecycleStore::at(&lifecycle_path);
                fs::create_dir_all(store.hooks_dir()).unwrap();
                fs::write(store.snapshot_path(), br#"{"schema_version":4}"#).unwrap();
            } else {
                fs::write(&lifecycle_path, b"occupied").unwrap();
            }
            let lifecycle = LifecycleStore::at(&lifecycle_path);
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    1,
                    Some("call-1"),
                    "cargo test",
                    ActivityState::Allowed,
                ))
                .unwrap();

            let stderr = invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!({"exit_code": 0})),
                ),
            );

            assert!(!stderr.is_empty(), "newer_schema={newer_schema}");
            assert_eq!(outcome_and_diagnostic_counts(&activity).0, 0);
        }
    }

    #[test]
    fn shared_activity_id_with_foreign_terminal_is_diagnostic_not_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        seed_codex_child(&lifecycle, temp.path(), "child-a", "turn-a");

        let mut current = decision_event(
            temp.path(),
            "shared-activity",
            1,
            Some("call-1"),
            "cargo test",
            ActivityState::Observed,
        );
        let session = current.session.as_mut().unwrap();
        session.session_id = "child-a".into();
        session.provider_session_id = Some("root-1".into());
        session.turn_id = Some("turn-a".into());
        activity.append(current.clone()).unwrap();

        let mut foreign = current;
        foreign.recorded_at_ms = 2;
        foreign.state = ActivityState::Allowed;
        foreign.session.as_mut().unwrap().session_id = "child-b".into();
        activity.append(foreign.clone()).unwrap();

        let mut matching_after_foreign = foreign;
        matching_after_foreign.recorded_at_ms = 3;
        matching_after_foreign.session.as_mut().unwrap().session_id = "child-a".into();
        activity.append(matching_after_foreign).unwrap();

        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            serde_json::from_slice(&child_post_payload(temp.path(), "child-a", "turn-a")).unwrap(),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert!(stderr.contains("orphan outcome"));
    }

    #[test]
    fn post_tool_use_falls_back_within_unique_pre_interval() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        assert!(
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
            )
            .is_empty()
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();

        let stderr = invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque unified-exec response")),
            ),
        );

        assert!(stderr.is_empty(), "{stderr}");
        let events = activity.read().unwrap().events().to_vec();
        let outcome = events
            .iter()
            .find(|event| event.state == ActivityState::Outcome)
            .unwrap();
        assert_eq!(outcome.activity_id, "activity-1");
        assert_eq!(outcome.schema_version, ACTIVITY_SCHEMA_VERSION);
        assert_eq!(outcome.outcome, Some(ActivityOutcome::Completed));
        assert_eq!(
            outcome.session.as_ref().unwrap().tool_use_id.as_deref(),
            Some("call-1")
        );
        assert!(events.iter().any(|event| {
            event.kind == ActivityKind::Lifecycle && event.tool.as_deref() == Some("PostToolUse")
        }));
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque unified-exec response")
        );
    }

    #[test]
    fn ignored_duplicate_pre_does_not_make_fallback_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-2", "cargo test", None),
        );
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        assert_eq!(outcome_and_diagnostic_counts(&activity), (1, 0));
    }

    #[test]
    fn interleaved_pre_interval_without_decision_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append(decision_event(
                temp.path(),
                "unrelated-activity",
                1,
                None,
                "cargo check",
                ActivityState::Denied,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-2", "cargo test", None),
        );

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 0));
    }

    #[test]
    fn ignored_duplicate_pre_does_not_end_the_fallback_interval() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Observed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-2", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                4,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (1, 0));
    }

    #[test]
    fn review_regression_fallback_requires_matching_metadata_on_terminal_row() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        let mut observed = decision_event(
            temp.path(),
            "activity-1",
            2,
            None,
            "cargo test",
            ActivityState::Observed,
        );
        observed.decision_id = None;
        activity.append(observed).unwrap();
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                3,
                None,
                "cargo check",
                ActivityState::Allowed,
            ))
            .unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn candidate_losslessness_rejects_v1_fallback_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        let mut decision = decision_event(
            temp.path(),
            "activity-v1",
            2,
            None,
            "cargo test",
            ActivityState::Allowed,
        );
        decision.schema_version = 1;
        let mut file = OpenOptions::new()
            .append(true)
            .open(&activity_path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&decision).unwrap()).unwrap();

        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("opaque result")),
            ),
        );

        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["opaque result"]);
    }

    #[test]
    fn repeated_identical_decisions_are_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        for (index, id) in ["activity-a", "activity-b"].into_iter().enumerate() {
            activity
                .append(decision_event(
                    temp.path(),
                    id,
                    index as u64 + 2,
                    None,
                    "cargo test",
                    ActivityState::Allowed,
                ))
                .unwrap();
        }
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-1",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
        assert_diagnostics_are_metadata_only(&activity, &["done"]);
    }

    #[test]
    fn lossy_commands_do_not_correlate() {
        for (decision_command, post_command) in [
            (
                "curl --token alpha".to_string(),
                "curl --token beta".to_string(),
            ),
            (
                format!("{}a", "x".repeat(MAX_ACTIVITY_FIELD_BYTES)),
                format!("{}b", "x".repeat(MAX_ACTIVITY_FIELD_BYTES)),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", &decision_command, None),
            );
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    2,
                    None,
                    &decision_command,
                    ActivityState::Allowed,
                ))
                .unwrap();
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    3,
                    None,
                    &decision_command,
                    ActivityState::Delivered,
                ))
                .unwrap();
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    &post_command,
                    Some(serde_json::json!("done")),
                ),
            );
            assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
            assert_diagnostics_are_metadata_only(
                &activity,
                &[post_command.as_str(), "alpha", "beta", "done"],
            );
            let snapshot = activity.snapshot(SnapshotLimits::default()).unwrap();
            assert!(snapshot.attention.is_empty());
            assert_eq!(snapshot.diagnostic_events.len(), 1);
            assert_eq!(
                snapshot.diagnostic_events[0].reasoning.as_deref(),
                Some("orphan outcome: Bash command is not losslessly correlatable")
            );
            assert_eq!(snapshot.recent.len(), 1);
            assert_eq!(snapshot.recent[0].activity_id, "activity-1");
        }
    }

    #[test]
    fn duplicate_post_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let post = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!("done")),
        );
        invoke_activity_hook(&lifecycle, &activity, post.clone());
        invoke_activity_hook(&lifecycle, &activity, post);
        assert_eq!(outcome_and_diagnostic_counts(&activity).0, 1);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
    }

    #[test]
    fn equivalent_evidence_appends_changed_outcome_and_dedupes_retry() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                1,
                Some("call-1"),
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let completed = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!("opaque response")),
        );
        let failed = hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!({"exit_code": 7})),
        );

        seed_active_tool(&lifecycle, AgentProvider::Codex, temp.path());
        invoke_activity_hook(&lifecycle, &activity, completed);
        invoke_activity_hook(&lifecycle, &activity, failed.clone());
        invoke_activity_hook(&lifecycle, &activity, failed);

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter_map(|event| event.outcome)
                .collect::<Vec<_>>(),
            [ActivityOutcome::Completed]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("opaque response")
        );
    }

    #[test]
    fn non_allowed_terminal_states_never_receive_outcomes() {
        for state in [
            ActivityState::Denied,
            ActivityState::Abstained,
            ActivityState::Error,
        ] {
            for tool_use_id in [None, Some("call-1")] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                invoke_activity_hook(
                    &lifecycle,
                    &activity,
                    hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
                );
                activity
                    .append(decision_event(
                        temp.path(),
                        "activity-1",
                        2,
                        tool_use_id,
                        "cargo test",
                        state,
                    ))
                    .unwrap();
                invoke_activity_hook(
                    &lifecycle,
                    &activity,
                    hook_payload(
                        temp.path(),
                        "PostToolUse",
                        "call-1",
                        "cargo test",
                        Some(serde_json::json!("done")),
                    ),
                );
                assert_eq!(outcome_and_diagnostic_counts(&activity).0, 0);
            }
        }
    }

    #[test]
    fn first_terminal_state_controls_outcome_eligibility() {
        for (first, second, expected) in [
            (ActivityState::Allowed, ActivityState::Denied, 1),
            (ActivityState::Denied, ActivityState::Allowed, 0),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
            );
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    2,
                    None,
                    "cargo test",
                    first,
                ))
                .unwrap();
            activity
                .append(decision_event(
                    temp.path(),
                    "activity-1",
                    3,
                    None,
                    "cargo test",
                    second,
                ))
                .unwrap();
            invoke_activity_hook(
                &lifecycle,
                &activity,
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!("done")),
                ),
            );
            assert_eq!(outcome_and_diagnostic_counts(&activity).0, expected);
        }
    }

    #[test]
    fn oversized_ids_use_the_same_bounded_comparison_form() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let call = "c".repeat(MAX_ACTIVITY_FIELD_BYTES + 100);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", &call, "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                &call,
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.state == ActivityState::Outcome)
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .filter_map(|event| event.session.as_ref())
                .filter_map(|session| session.tool_use_id.as_ref())
                .all(|id| id.len() <= MAX_ACTIVITY_FIELD_BYTES)
        );
    }

    #[test]
    fn concurrent_duplicate_post_appends_one_outcome() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle_path = temp.path().join("lifecycle");
        let activity_path = temp.path().join("activity.jsonl");
        let lifecycle = LifecycleStore::at(&lifecycle_path);
        let activity = ActivityStore::at(&activity_path);
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-1",
                2,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let cwd = temp.path().to_path_buf();
                let lifecycle_path = lifecycle_path.clone();
                let activity_path = activity_path.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    invoke_activity_hook(
                        &LifecycleStore::at(lifecycle_path),
                        &ActivityStore::at(activity_path),
                        hook_payload(
                            &cwd,
                            "PostToolUse",
                            "call-1",
                            "cargo test",
                            Some(serde_json::json!("done")),
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            let stderr = handle.join().unwrap();
            assert!(stderr.is_empty() || stderr.contains("lifecycle event ignored"));
        }
        assert_eq!(outcome_and_diagnostic_counts(&activity).0, 1);
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                .count(),
            1
        );
    }

    #[test]
    fn large_log_correlates_only_the_tail_decision() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        activity
            .append_from_snapshot(|_| {
                (0..10_000)
                    .map(|index| {
                        decision_event(
                            temp.path(),
                            &format!("irrelevant-{index}"),
                            index,
                            None,
                            "other command",
                            ActivityState::Allowed,
                        )
                    })
                    .collect()
            })
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(temp.path(), "PreToolUse", "call-tail", "cargo test", None),
        );
        activity
            .append(decision_event(
                temp.path(),
                "activity-tail",
                10_002,
                None,
                "cargo test",
                ActivityState::Allowed,
            ))
            .unwrap();
        invoke_activity_hook(
            &lifecycle,
            &activity,
            hook_payload(
                temp.path(),
                "PostToolUse",
                "call-tail",
                "cargo test",
                Some(serde_json::json!("done")),
            ),
        );
        let log = activity.read().unwrap();
        let outcomes = log
            .events()
            .iter()
            .filter(|event| event.state == ActivityState::Outcome)
            .map(|event| event.activity_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(outcomes, ["activity-tail"]);
    }

    #[test]
    fn activity_storage_failures_are_bounded_and_fail_open() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let lock_path = activity_path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!("secret response")),
                )
                .to_string(),
            ),
            &mut stdout,
            &mut stderr,
            &lifecycle,
            Some(&ActivityStore::at(&activity_path)),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(stderr.len() < 256);
        assert!(!String::from_utf8_lossy(&stderr).contains("secret response"));
        FileExt::unlock(&lock).unwrap();

        let blocked_path = temp.path().join("blocked-activity.jsonl");
        fs::create_dir(&blocked_path).unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_activity(
            Cursor::new(
                hook_payload(
                    temp.path(),
                    "PostToolUse",
                    "call-2",
                    "cargo test",
                    Some(serde_json::json!("another secret response")),
                )
                .to_string(),
            ),
            &mut stdout,
            &mut stderr,
            &lifecycle,
            Some(&ActivityStore::at(blocked_path)),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(stderr.len() < 256);
        assert!(!String::from_utf8_lossy(&stderr).contains("another secret response"));
    }
}
