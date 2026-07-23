#![allow(dead_code)]

use std::fmt;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SessionTarget, bounded_redacted_activity_text, lossless_redacted_activity_text,
};
use coding_brain_core::lifecycle::{
    ApplyOutcome, IgnoreReason, LifecycleEvent, LifecycleIdentity, LifecycleStore,
    PermissionDisposition, coding_brain_state_root,
};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::ProjectIdentity;
use coding_brain_core::provider::AgentProvider;
use coding_brain_core::runtime::BrainGateMode;

use super::activity::ActivityStore;
use super::client::BrainSuggestion;
use super::decisions::{HookDecisionAudit, append_deterministic, append_hook_proposal};
use super::query::{self, BrainDecision, BrainDecisionRequest};
use super::safety::SafetyDeny;
use crate::config::BrainConfig;
use crate::lifecycle_hook::read_bounded_hook_input;
use crate::provider_hooks::{PermissionHookRequest, ProviderPermissionPolicy, parse_permission};

const HOOK_INFERENCE_TIMEOUT_MS: u64 = 25_000;
static ACTIVITY_ID_COUNTER: AtomicU32 = AtomicU32::new(0);

#[derive(Debug)]
struct HookDiagnostic(String);

impl HookDiagnostic {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HookDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug)]
enum PermissionRecordError {
    Ignored(IgnoreReason),
    Failed(HookDiagnostic),
}

impl fmt::Display for PermissionRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ignored(reason) => write!(formatter, "lifecycle event was ignored: {reason:?}"),
            Self::Failed(diagnostic) => diagnostic.fmt(formatter),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum PermissionBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum AntigravityDecision {
    Allow,
    Deny,
    Ask,
}

enum ProviderPermissionResponse {
    CodexOrClaude {
        behavior: PermissionBehavior,
        message: Option<String>,
    },
    Antigravity {
        decision: AntigravityDecision,
        reason: Option<String>,
    },
}

impl PermissionBehavior {
    fn user_action(self) -> &'static str {
        match self {
            Self::Allow => "hook_allow",
            Self::Deny => "hook_deny",
        }
    }
}

#[derive(Debug)]
pub(crate) enum HookEvaluation {
    Allow {
        brain: BrainDecision,
        terminal_state: ActivityState,
    },
    Deny {
        brain: Option<BrainDecision>,
        deterministic: bool,
        safety: Option<SafetyDeny>,
        terminal_state: ActivityState,
    },
    Abstain {
        brain: Option<BrainDecision>,
        reason: String,
        terminal_state: ActivityState,
    },
}

#[derive(Debug, Clone)]
struct HookActivity {
    activity_id: String,
    project: ProjectEvidence,
    session: SessionTarget,
    tool: String,
    command: Option<String>,
    terminal_command: Option<String>,
}

impl HookActivity {
    fn from_request(
        request: &PermissionHookRequest,
        paths: &CodingBrainPaths,
    ) -> Result<Self, HookDiagnostic> {
        let identity = ProjectIdentity::load(request.lifecycle.cwd(), paths).map_err(|error| {
            HookDiagnostic::new(format!("could not resolve project identity: {error}"))
        })?;
        let project = ProjectEvidence {
            project_id: identity.id().clone(),
            cwd: request.lifecycle.cwd().to_path_buf(),
            label: Some(request.project.clone()),
        };
        let session = SessionTarget {
            provider: request.lifecycle.provider(),
            session_id: request.lifecycle.session_id().to_string(),
            turn_id: request.lifecycle.turn_id().map(str::to_string),
            tool_use_id: request.tool_use_id.clone(),
            project_id: identity.id().clone(),
            cwd: request.lifecycle.cwd().to_path_buf(),
            provider_hints: Vec::new(),
        };
        Ok(Self {
            activity_id: gen_activity_id(),
            project,
            session,
            tool: request.tool_name.clone(),
            command: request
                .command
                .as_deref()
                .map(bounded_redacted_activity_text),
            terminal_command: request
                .command
                .as_deref()
                .and_then(lossless_redacted_activity_text),
        })
    }

    fn event(&self, state: ActivityState) -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: self.activity_id.clone(),
            recorded_at_ms: epoch_ms(),
            project: self.project.clone(),
            session: Some(self.session.clone()),
            state,
            tool: Some(self.tool.clone()),
            normalized_command: if state.is_terminal() {
                self.terminal_command.clone()
            } else {
                self.command.clone()
            },
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
        }
    }
}

fn gen_activity_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = ACTIVITY_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("activity_{nanos}_{}_{sequence}", std::process::id())
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn current_paths() -> Result<CodingBrainPaths, HookDiagnostic> {
    let environment = PathEnvironment::new(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    CodingBrainPaths::resolve(&environment).map_err(|error| {
        HookDiagnostic::new(format!("could not resolve Coding Brain paths: {error:?}"))
    })
}

pub(crate) fn evaluate_request<F>(
    request: &BrainDecisionRequest,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    persistence_ready: bool,
    supported: bool,
    infer: F,
) -> HookEvaluation
where
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    if let Some(safety) = super::safety::evaluate(request) {
        return HookEvaluation::Deny {
            brain: None,
            deterministic: true,
            safety: Some(safety),
            terminal_state: ActivityState::Denied,
        };
    }
    if !persistence_ready {
        return HookEvaluation::Abstain {
            brain: None,
            reason: "initial activity persistence failed".into(),
            terminal_state: ActivityState::Error,
        };
    }
    if !supported {
        return HookEvaluation::Abstain {
            brain: None,
            reason: "unsupported permission tool".into(),
            terminal_state: ActivityState::Abstained,
        };
    }
    if gate_mode == BrainGateMode::Off {
        return HookEvaluation::Abstain {
            brain: None,
            reason: "Brain model mode is off".into(),
            terminal_state: ActivityState::Abstained,
        };
    }
    let mut hook_config = config.cloned().unwrap_or_default();
    hook_config.timeout_ms = hook_config.timeout_ms.min(HOOK_INFERENCE_TIMEOUT_MS);
    let brain = query::evaluate_with(request, &hook_config, gate_mode.as_str(), infer);
    if gate_mode == BrainGateMode::On {
        let reason = brain.reasoning.clone();
        return HookEvaluation::Abstain {
            terminal_state: if brain.source == "error" {
                ActivityState::Error
            } else {
                ActivityState::Abstained
            },
            brain: Some(brain),
            reason,
        };
    }
    if brain.source == "brain" && brain.below_threshold == Some(false) {
        return match brain.action.as_str() {
            "approve" => HookEvaluation::Allow {
                brain,
                terminal_state: ActivityState::Allowed,
            },
            "deny" => HookEvaluation::Deny {
                brain: Some(brain),
                deterministic: false,
                safety: None,
                terminal_state: ActivityState::Denied,
            },
            _ => HookEvaluation::Abstain {
                reason: "model returned a non-executable action".into(),
                brain: Some(brain),
                terminal_state: ActivityState::Abstained,
            },
        };
    }
    let reason = brain.reasoning.clone();
    HookEvaluation::Abstain {
        terminal_state: if brain.source == "error" {
            ActivityState::Error
        } else {
            ActivityState::Abstained
        },
        brain: Some(brain),
        reason,
    }
}

#[derive(Serialize)]
struct HookResponse {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: HookSpecificOutput,
}

#[derive(Serialize)]
struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    hook_event_name: &'static str,
    decision: HookResponseDecision,
}

#[derive(Serialize)]
struct HookResponseDecision {
    behavior: PermissionBehavior,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn parse_request(input: &str) -> Result<PermissionHookRequest, HookDiagnostic> {
    parse_permission(AgentProvider::Codex, None, input.as_bytes())
        .map_err(|error| HookDiagnostic::new(format!("invalid PermissionRequest payload: {error}")))
}

fn serialize_response(response: ProviderPermissionResponse) -> Result<Vec<u8>, serde_json::Error> {
    match response {
        ProviderPermissionResponse::CodexOrClaude { behavior, message } => {
            serde_json::to_vec(&HookResponse {
                hook_specific_output: HookSpecificOutput {
                    hook_event_name: "PermissionRequest",
                    decision: HookResponseDecision { behavior, message },
                },
            })
        }
        ProviderPermissionResponse::Antigravity { decision, reason } => {
            #[derive(Serialize)]
            struct Response {
                decision: AntigravityDecision,
                #[serde(skip_serializing_if = "Option::is_none")]
                reason: Option<String>,
            }
            serde_json::to_vec(&Response { decision, reason })
        }
    }
}

fn response_for_behavior(
    provider: AgentProvider,
    behavior: PermissionBehavior,
    message: Option<&str>,
) -> ProviderPermissionResponse {
    let deny_message = (behavior == PermissionBehavior::Deny)
        .then(|| message.map(bounded_redacted_activity_text))
        .flatten();
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => ProviderPermissionResponse::CodexOrClaude {
            behavior,
            message: deny_message,
        },
        AgentProvider::Antigravity => ProviderPermissionResponse::Antigravity {
            decision: match behavior {
                PermissionBehavior::Allow => AntigravityDecision::Allow,
                PermissionBehavior::Deny => AntigravityDecision::Deny,
            },
            reason: deny_message,
        },
    }
}

fn antigravity_ask() -> ProviderPermissionResponse {
    ProviderPermissionResponse::Antigravity {
        decision: AntigravityDecision::Ask,
        reason: Some("Coding Brain abstained".into()),
    }
}

fn write_diagnostic(stderr: &mut impl Write, diagnostic: impl fmt::Display) {
    let diagnostic = bounded_redacted_activity_text(&diagnostic.to_string());
    let _ = writeln!(stderr, "coding-brain permission hook: {diagnostic}");
}

fn record_permission(
    store: &LifecycleStore,
    identity: &LifecycleIdentity,
    disposition: PermissionDisposition,
) -> Result<(), PermissionRecordError> {
    let event = LifecycleEvent::permission(identity.clone(), disposition).map_err(|error| {
        PermissionRecordError::Failed(HookDiagnostic::new(format!(
            "invalid lifecycle event: {error}"
        )))
    })?;
    match store.record(event) {
        Ok(ApplyOutcome::Applied) => Ok(()),
        Ok(ApplyOutcome::Ignored(reason)) => Err(PermissionRecordError::Ignored(reason)),
        Err(error) => Err(PermissionRecordError::Failed(HookDiagnostic::new(format!(
            "could not persist lifecycle state: {error}"
        )))),
    }
}

fn run_with_gate_and_store<R, W, E, F>(
    stdin: R,
    stdout: W,
    stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    store: &LifecycleStore,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let activity = ActivityStore::at(store.hooks_dir().join("activity.jsonl"));
    run_with_gate_and_stores(
        stdin,
        stdout,
        stderr,
        config,
        gate_mode,
        store,
        Some(&activity),
        infer,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_with_gate_and_stores<R, W, E, F>(
    stdin: R,
    stdout: W,
    stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    lifecycle_store: &LifecycleStore,
    activity_store: Option<&ActivityStore>,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    run_provider_with_gate_and_stores(
        stdin,
        stdout,
        stderr,
        config,
        gate_mode,
        lifecycle_store,
        activity_store,
        AgentProvider::Codex,
        None,
        infer,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_provider_with_gate_and_stores<R, W, E, F>(
    stdin: R,
    mut stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    lifecycle_store: &LifecycleStore,
    activity_store: Option<&ActivityStore>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let input = match read_bounded_hook_input(stdin) {
        Ok(input) => input,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    };
    let request = match parse_permission(provider, antigravity_event, &input) {
        Ok(request) => request,
        Err(error) => {
            write_diagnostic(
                &mut stderr,
                HookDiagnostic::new(format!("invalid permission payload: {error}")),
            );
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    };
    let needs_input = |stderr: &mut E| {
        if let Err(error) = record_permission(
            lifecycle_store,
            &request.lifecycle,
            PermissionDisposition::NeedsInput,
        ) {
            write_diagnostic(stderr, error);
        }
    };
    let activity_context =
        current_paths().and_then(|paths| HookActivity::from_request(&request, &paths));
    let mut persistence_error = match (&activity_context, activity_store) {
        (Err(error), _) => Some(error.to_string()),
        (_, None) => Some("activity store unavailable".into()),
        (Ok(context), Some(activity_store)) => {
            let observed = activity_store
                .append(context.event(ActivityState::Observed))
                .err();
            let evaluating = activity_store
                .append(context.event(ActivityState::Evaluating))
                .err();
            observed.or(evaluating).map(|error| error.to_string())
        }
    };
    let brain_request = BrainDecisionRequest {
        project: request.project.clone(),
        tool_name: request.tool_name.clone(),
        tool_input: request.command.clone().unwrap_or_default(),
        diff_digest: None,
    };
    let evaluation = if let Some(safety) = super::safety::evaluate(&brain_request) {
        HookEvaluation::Deny {
            brain: None,
            deterministic: true,
            safety: Some(safety),
            terminal_state: ActivityState::Denied,
        }
    } else if request.provider_policy == ProviderPermissionPolicy::Denies {
        HookEvaluation::Deny {
            brain: None,
            deterministic: true,
            safety: None,
            terminal_state: ActivityState::Denied,
        }
    } else {
        let model_evaluation = evaluate_request(
            &brain_request,
            config,
            gate_mode,
            persistence_error.is_none(),
            request.command.is_some(),
            infer,
        );
        match (request.provider_policy, model_evaluation) {
            (ProviderPermissionPolicy::RequiresAsk, HookEvaluation::Allow { brain, .. }) => {
                HookEvaluation::Abstain {
                    brain: Some(brain),
                    reason: "provider permission policy requires confirmation".into(),
                    terminal_state: ActivityState::Abstained,
                }
            }
            (_, evaluation) => evaluation,
        }
    };
    if let HookEvaluation::Deny {
        deterministic: true,
        safety,
        terminal_state,
        ..
    } = &evaluation
    {
        let reason = safety
            .as_ref()
            .map(|deny| deny.reason.as_str())
            .unwrap_or("provider permission policy denied request");
        let serialized = match serialize_response(response_for_behavior(
            provider,
            PermissionBehavior::Deny,
            safety.as_ref().map(|deny| deny.reason.as_str()),
        )) {
            Ok(serialized) => serialized,
            Err(error) => {
                write_diagnostic(
                    &mut stderr,
                    format!("could not serialize response: {error}"),
                );
                if provider == AgentProvider::Antigravity {
                    write_failsafe_ask(&mut stdout, &mut stderr);
                }
                return;
            }
        };
        let audit = HookDecisionAudit {
            provider: request.lifecycle.provider(),
            project: &request.project,
            tool: &request.tool_name,
            command: activity_context
                .as_ref()
                .ok()
                .and_then(|context| context.command.as_deref())
                .unwrap_or_default(),
            brain_action: "deny",
            brain_confidence: 1.0,
            brain_reasoning: reason,
            brain_source: if safety.is_some() {
                "deterministic"
            } else {
                "provider_policy"
            },
            brain_threshold: None,
            session_id: request.lifecycle.session_id(),
            turn_id: request.lifecycle.turn_id().unwrap_or_default(),
        };
        let decision_id = match append_deterministic(&audit) {
            Ok(decision_id) => Some(decision_id),
            Err(error) => {
                persistence_error.get_or_insert_with(|| error.to_string());
                None
            }
        };
        if let (Ok(context), Some(activity_store)) = (&activity_context, activity_store) {
            let mut terminal = context.event(*terminal_state);
            terminal.rule_id = safety.as_ref().map(|deny| deny.rule_id.into());
            terminal.reasoning = Some(reason.into());
            terminal.decision_id.clone_from(&decision_id);
            if let Err(error) = activity_store.append(terminal) {
                persistence_error.get_or_insert_with(|| error.to_string());
            }
        }
        if let Some(error) = &persistence_error {
            write_diagnostic(&mut stderr, format!("deterministic deny audit: {error}"));
        }
        if let Err(error) = record_permission(
            lifecycle_store,
            &request.lifecycle,
            PermissionDisposition::Decided,
        ) {
            write_diagnostic(&mut stderr, error);
        }
        let delivery = match write_response(&mut stdout, &serialized) {
            Ok(()) => ActivityState::Delivered,
            Err(error) => {
                write_diagnostic(&mut stderr, format!("could not write response: {error}"));
                ActivityState::DeliveryFailed
            }
        };
        if let (Ok(context), Some(activity_store)) = (&activity_context, activity_store) {
            let mut event = context.event(delivery);
            event.decision_id = decision_id;
            event.reasoning = (delivery == ActivityState::DeliveryFailed)
                .then(|| "hook response write failed".into());
            let _ = activity_store.append(event);
            let _ = activity_store.compact_if_needed();
        }
        return;
    }
    let (brain, behavior, terminal_state) = match evaluation {
        HookEvaluation::Allow {
            brain,
            terminal_state,
        } => (brain, Some(PermissionBehavior::Allow), terminal_state),
        HookEvaluation::Deny {
            brain: Some(brain),
            deterministic: false,
            safety: None,
            terminal_state,
        } => (brain, Some(PermissionBehavior::Deny), terminal_state),
        HookEvaluation::Abstain {
            brain: Some(brain),
            terminal_state,
            ..
        } => (brain, None, terminal_state),
        HookEvaluation::Abstain {
            brain: None,
            reason,
            terminal_state,
        } => {
            if let Some(error) = persistence_error {
                write_diagnostic(
                    &mut stderr,
                    format!("could not persist hook activity: {error}"),
                );
            }
            if let (Ok(context), Some(activity_store)) = (&activity_context, activity_store) {
                let mut event = context.event(terminal_state);
                event.reasoning = Some(reason);
                let _ = activity_store.append(event);
                let _ = activity_store.compact_if_needed();
            }
            needs_input(&mut stderr);
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
        _ => unreachable!("deterministic deny was handled before model persistence"),
    };

    // Serialize first so a serialization error can never leave a prepared
    // audit record without a response ready to write.
    let serialized = if let Some(behavior) = behavior {
        match serialize_response(response_for_behavior(
            provider,
            behavior,
            brain.message.as_deref(),
        )) {
            Ok(serialized) => Some(serialized),
            Err(error) => {
                write_diagnostic(
                    &mut stderr,
                    format!("could not serialize response: {error}"),
                );
                return;
            }
        }
    } else {
        None
    };

    let audit = HookDecisionAudit {
        provider: request.lifecycle.provider(),
        project: &request.project,
        tool: &request.tool_name,
        command: activity_context
            .as_ref()
            .ok()
            .and_then(|context| context.command.as_deref())
            .unwrap_or_default(),
        brain_action: &brain.action,
        brain_confidence: brain.confidence,
        brain_reasoning: &bounded_redacted_activity_text(&brain.reasoning),
        brain_source: brain.source,
        brain_threshold: brain.threshold,
        session_id: request.lifecycle.session_id(),
        turn_id: request.lifecycle.turn_id().unwrap_or_default(),
    };
    let decision_id = match append_hook_proposal(&audit) {
        Ok(decision_id) => decision_id,
        Err(error) => {
            write_diagnostic(
                &mut stderr,
                format!("could not persist decision proposal: {error}"),
            );
            needs_input(&mut stderr);
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    };
    let mut terminal = activity_context.as_ref().unwrap().event(terminal_state);
    terminal.confidence = Some(brain.confidence);
    terminal.threshold = brain.threshold;
    terminal.reasoning = Some(bounded_redacted_activity_text(&brain.reasoning));
    terminal.decision_id = Some(decision_id.clone());
    if let Err(error) = activity_store.unwrap().append(terminal) {
        write_diagnostic(
            &mut stderr,
            format!("could not persist terminal activity: {error}"),
        );
        needs_input(&mut stderr);
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    }
    let Some(serialized) = serialized else {
        let _ = activity_store.unwrap().compact_if_needed();
        if brain.source == "error" {
            write_diagnostic(&mut stderr, &brain.reasoning);
        }
        needs_input(&mut stderr);
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    };
    if let Err(error) = record_permission(
        lifecycle_store,
        &request.lifecycle,
        PermissionDisposition::Decided,
    ) {
        let message = format!("could not persist executable permission state: {error}");
        write_diagnostic(&mut stderr, &message);
        if behavior == Some(PermissionBehavior::Allow) {
            if let (Ok(context), Some(activity_store)) = (&activity_context, activity_store) {
                let mut event = context.event(ActivityState::Error);
                event.decision_id = Some(decision_id);
                event.reasoning = Some(bounded_redacted_activity_text(&message));
                if let Err(error) = activity_store.append(event) {
                    write_diagnostic(
                        &mut stderr,
                        format!("could not persist permission failure activity: {error}"),
                    );
                }
                let _ = activity_store.compact_if_needed();
            }
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    }
    let (delivery, failure) = match write_response(&mut stdout, &serialized) {
        Ok(()) => (ActivityState::Delivered, None),
        Err(error) => {
            let message = format!("could not write response: {error}");
            write_diagnostic(&mut stderr, &message);
            (ActivityState::DeliveryFailed, Some(message))
        }
    };
    let mut event = activity_context.as_ref().unwrap().event(delivery);
    event.decision_id = Some(decision_id);
    event.reasoning = failure;
    if let Err(error) = activity_store.unwrap().append(event) {
        write_diagnostic(
            &mut stderr,
            format!("could not persist delivery activity: {error}"),
        );
    }
    let _ = activity_store.unwrap().compact_if_needed();
}

fn write_response(stdout: &mut impl Write, serialized: &[u8]) -> std::io::Result<()> {
    stdout.write_all(serialized)?;
    stdout.flush()
}

fn write_failsafe_ask(stdout: &mut impl Write, stderr: &mut impl Write) {
    match serialize_response(antigravity_ask()) {
        Ok(serialized) => {
            if let Err(error) = write_response(stdout, &serialized) {
                write_diagnostic(stderr, format!("could not write response: {error}"));
            }
        }
        Err(error) => write_diagnostic(stderr, format!("could not serialize response: {error}")),
    }
}

fn run_with_gate<R, W, E, F>(
    stdin: R,
    stdout: W,
    stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let lifecycle_store = LifecycleStore::at(coding_brain_state_root());
    let activity_store = current_paths()
        .ok()
        .map(|paths| ActivityStore::at(paths.state_root().join("activity.jsonl")));
    run_with_gate_and_stores(
        stdin,
        stdout,
        stderr,
        config,
        gate_mode,
        &lifecycle_store,
        activity_store.as_ref(),
        infer,
    );
}

fn run_with<R, W, E, F>(stdin: R, stdout: W, mut stderr: E, config: Option<&BrainConfig>, infer: F)
where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let resolved = super::resolve_gate_mode(config);
    if let Some(warning) = resolved.warning {
        write_diagnostic(&mut stderr, warning);
    }
    run_with_gate(stdin, stdout, stderr, config, resolved.mode, infer);
}

fn run_provider_with<R, W, E, F>(
    stdin: R,
    stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let resolved = super::resolve_gate_mode(config);
    if let Some(warning) = resolved.warning {
        write_diagnostic(&mut stderr, warning);
    }
    let lifecycle_store = LifecycleStore::at(coding_brain_state_root());
    let activity_store = current_paths()
        .ok()
        .map(|paths| ActivityStore::at(paths.state_root().join("activity.jsonl")));
    run_provider_with_gate_and_stores(
        stdin,
        stdout,
        stderr,
        config,
        resolved.mode,
        &lifecycle_store,
        activity_store.as_ref(),
        provider,
        antigravity_event,
        infer,
    );
}

pub(crate) fn run(
    config: Option<&BrainConfig>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_provider_with(
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        config,
        provider,
        antigravity_event,
        super::client::infer,
    );
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::io::Cursor;
    use std::panic::AssertUnwindSafe;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::brain::activity::ActivityStore;
    use crate::brain::client::BrainSuggestion;
    use crate::brain::decisions::decisions_dir;
    use crate::config::BrainConfig;
    use crate::rules::RuleAction;
    use coding_brain_core::brain_activity::{
        ActivityKind, ActivityState, MAX_ACTIVITY_FIELD_BYTES, bounded_redacted_activity_text,
    };
    use coding_brain_core::lifecycle::{LifecycleStore, ProjectedStatus};

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct FailingFlushWriter;

    impl Write for FailingFlushWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture flush failed",
            ))
        }
    }

    #[derive(Clone)]
    struct VisibleThenPanicWriter(Rc<RefCell<Vec<u8>>>);

    impl Write for VisibleThenPanicWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            panic!("simulated abrupt termination after response bytes became visible")
        }
    }

    struct RestoreHome(Option<OsString>);

    impl Drop for RestoreHome {
        fn drop(&mut self) {
            // SAFETY: every test that changes HOME holds HOME_ENV_LOCK.
            unsafe {
                match self.0.take() {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    fn set_test_home(path: &Path) -> RestoreHome {
        let original = std::env::var_os("HOME");
        // SAFETY: every caller holds HOME_ENV_LOCK.
        unsafe { std::env::set_var("HOME", path) };
        RestoreHome(original)
    }

    fn payload() -> String {
        payload_with_command("cargo test")
    }

    fn expected_project() -> String {
        std::env::current_dir()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn payload_with_command(command: &str) -> String {
        let cwd = std::env::current_dir().unwrap();
        serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": cwd,
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": command }
        })
        .to_string()
    }

    fn suggestion(action: RuleAction, confidence: f64) -> BrainSuggestion {
        BrainSuggestion {
            action,
            message: Some("reviewed by brain".into()),
            reasoning: "test reasoning".into(),
            confidence,
            suggested_at: 123,
        }
    }

    fn enabled_config() -> BrainConfig {
        BrainConfig {
            enabled: true,
            legacy_mode_configured: true,
            timeout_ms: 60_000,
            ..BrainConfig::default()
        }
    }

    fn run_test_with_gate<R, W, E, F>(
        stdin: R,
        stdout: W,
        stderr: E,
        config: Option<&BrainConfig>,
        gate_mode: BrainGateMode,
        infer: F,
    ) where
        R: Read,
        W: Write,
        E: Write,
        F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
    {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        run_with_gate_and_store(stdin, stdout, stderr, config, gate_mode, &store, infer);
    }

    fn run_test<R, W, E, F>(stdin: R, stdout: W, stderr: E, config: Option<&BrainConfig>, infer: F)
    where
        R: Read,
        W: Write,
        E: Write,
        F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
    {
        run_test_with_gate(stdin, stdout, stderr, config, BrainGateMode::On, infer);
    }

    fn projected_status(store: &LifecycleStore) -> Option<ProjectedStatus> {
        let key =
            coding_brain_core::provider::AgentSessionKey::native(AgentProvider::Codex, "session-1")
                .storage_key();
        store.read().unwrap().snapshot.unwrap().sessions[&key].projected_status
    }

    #[test]
    fn parses_valid_bash_permission_request() {
        let request = parse_request(&payload()).unwrap();
        assert_eq!(request.lifecycle.session_id(), "session-1");
        assert_eq!(request.lifecycle.turn_id(), Some("turn-1"));
        assert_eq!(request.lifecycle.cwd(), std::env::current_dir().unwrap());
        assert_eq!(request.tool_name, "Bash");
        assert_eq!(request.command.as_deref(), Some("cargo test"));
        assert_eq!(request.project, expected_project());
    }

    #[test]
    fn rejects_wrong_event() {
        let input = payload().replace("PermissionRequest", "PreToolUse");
        assert!(parse_request(&input).is_err());
    }

    #[test]
    fn rejects_empty_identity_fields() {
        for field in ["session_id", "turn_id", "cwd", "tool_name"] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input[field] = serde_json::json!("   ");
            assert!(
                parse_request(&input.to_string()).is_err(),
                "accepted empty {field}"
            );
        }
    }

    #[test]
    fn non_bash_records_needs_input_without_inference_or_response() {
        let home = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(home.path().join(".codexctl"));
        let input = payload().replace("\"Bash\"", "\"apply_patch\"");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(input),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::On,
            &store,
            |_, _| panic!("non-Bash permission must not reach inference"),
        );
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
    }

    #[test]
    fn oversized_permission_input_never_infers_audits_or_persists() {
        let home = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(home.path().join(".codexctl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(vec![b'x'; 65_537]),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::On,
            &store,
            |_, _| panic!("oversized permission must not reach inference"),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert!(!store.snapshot_path().exists());
        assert!(!decisions_dir().join("decisions.jsonl").exists());
    }

    #[test]
    fn lifecycle_failure_suppresses_allow_after_recording_error_activity() {
        let temp = tempfile::tempdir().unwrap();
        let healthy = LifecycleStore::at(temp.path().join("healthy"));
        let healthy_activity = ActivityStore::at(temp.path().join("healthy-activity.jsonl"));
        let blocked_root = temp.path().join("blocked");
        std::fs::write(&blocked_root, b"occupied").unwrap();
        let blocked = LifecycleStore::at(blocked_root);
        let blocked_activity = ActivityStore::at(temp.path().join("blocked-activity.jsonl"));

        let mut healthy_stdout = Vec::new();
        let mut healthy_stderr = Vec::new();
        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut healthy_stdout,
            &mut healthy_stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &healthy,
            Some(&healthy_activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );
        let mut failed_stdout = Vec::new();
        let mut failed_stderr = Vec::new();
        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut failed_stdout,
            &mut failed_stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &blocked,
            Some(&blocked_activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert!(!healthy_stdout.is_empty());
        assert!(failed_stdout.is_empty());
        assert!(healthy_stderr.is_empty());
        assert!(
            String::from_utf8(failed_stderr)
                .unwrap()
                .contains("lifecycle")
        );
        assert_eq!(
            blocked_activity
                .read()
                .unwrap()
                .events()
                .iter()
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::Error,
            ]
        );
    }

    #[test]
    fn rejects_missing_or_non_string_command() {
        for tool_input in [serde_json::json!({}), serde_json::json!({"command": 7})] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input["tool_input"] = tool_input;
            assert!(parse_request(&input.to_string()).is_err());
        }
    }

    #[test]
    fn empty_or_whitespace_command_falls_through_without_inference() {
        for command in ["", "  \t\n  "] {
            let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
            input["tool_input"]["command"] = serde_json::json!(command);
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            run_with_gate_and_store(
                Cursor::new(input.to_string()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::On,
                &store,
                |_, _| panic!("empty command must not reach inference"),
            );

            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
            assert!(!store.snapshot_path().exists());
        }
    }

    #[test]
    fn preserves_exact_nonempty_command() {
        let mut input: serde_json::Value = serde_json::from_str(&payload()).unwrap();
        input["tool_input"]["command"] = serde_json::json!("  cargo test --lib  ");

        let request = parse_request(&input.to_string()).unwrap();

        assert_eq!(request.command.as_deref(), Some("  cargo test --lib  "));
    }

    #[test]
    fn auto_approve_emits_allow_after_persisting() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &store,
            |config, _| {
                assert_eq!(config.timeout_ms, 25_000);
                Ok(suggestion(RuleAction::Approve, 0.9))
            },
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(
            output["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert!(stderr.is_empty());
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["provider"], "codex");
        assert_eq!(record["project"], expected_project());
        assert_eq!(record["tool"], "Bash");
        assert_eq!(record["command"], "cargo test");
        assert_eq!(record["brain_action"], "approve");
        assert_eq!(record["brain_source"], "brain");
        assert_eq!(record["user_action"], "hook_proposal");
        assert_eq!(record["session_id"], "session-1");
        assert_eq!(record["turn_id"], "turn-1");
        assert_eq!(projected_status(&store), Some(ProjectedStatus::Processing));
        let activity = ActivityStore::at(store.hooks_dir().join("activity.jsonl"));
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events.iter().map(|event| event.state).collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::Delivered,
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| event.activity_id == events[0].activity_id)
        );
        assert!(events[2].decision_id.is_some());
        assert_eq!(
            events[0].session.as_ref().unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
    }

    #[test]
    fn candidate_losslessness_blocks_asymmetric_fallbacks() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());

        for raw_command in [
            "curl --token alpha".to_string(),
            format!("{}tail", "x".repeat(MAX_ACTIVITY_FIELD_BYTES)),
            "cargo   test".to_string(),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
            let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
            let cwd = std::env::current_dir().unwrap();
            let pre = serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-1",
                "cwd": cwd,
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_use_id": "call-1",
                "tool_input": {"command": raw_command}
            });
            let mut pre_stderr = Vec::new();
            crate::lifecycle_hook::run_with_activity(
                Cursor::new(pre.to_string()),
                Vec::new(),
                &mut pre_stderr,
                &lifecycle,
                Some(&activity),
            );
            assert!(pre_stderr.is_empty());

            let mut permission_stdout = Vec::new();
            let mut permission_stderr = Vec::new();
            run_with_gate_and_stores(
                Cursor::new(payload_with_command(&raw_command)),
                &mut permission_stdout,
                &mut permission_stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle,
                Some(&activity),
                |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
            );
            assert!(!permission_stdout.is_empty());
            assert!(permission_stderr.is_empty());

            let persisted_form = bounded_redacted_activity_text(&raw_command);
            let post = serde_json::json!({
                "session_id": "session-1",
                "turn_id": "turn-1",
                "cwd": cwd,
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_use_id": "call-1",
                "tool_input": {"command": persisted_form},
                "tool_response": "opaque response"
            });
            let mut post_stderr = Vec::new();
            crate::lifecycle_hook::run_with_activity(
                Cursor::new(post.to_string()),
                Vec::new(),
                &mut post_stderr,
                &lifecycle,
                Some(&activity),
            );

            let events = activity.read().unwrap().events().to_vec();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.state == ActivityState::Outcome)
                    .count(),
                0,
                "lossy candidate correlated for {raw_command:?}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.kind == ActivityKind::Diagnostic)
                    .count(),
                1
            );
            let diagnostic = events
                .iter()
                .find(|event| event.kind == ActivityKind::Diagnostic)
                .unwrap();
            assert!(diagnostic.normalized_command.is_none());
            assert!(diagnostic.fingerprint.is_none());
            assert!(diagnostic.note.is_none());
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.tool.as_deref() == Some("PostToolUse"))
                    .count(),
                1
            );
            let serialized = serde_json::to_string(&events).unwrap();
            assert!(!serialized.contains("opaque response"));
            assert!(!serialized.contains(&raw_command));
        }
    }

    #[test]
    fn auto_deny_emits_deny() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &store,
            |_, _| Ok(suggestion(RuleAction::Deny, 0.9)),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["user_action"], "hook_proposal");
        assert_eq!(projected_status(&store), Some(ProjectedStatus::Processing));
    }

    #[test]
    fn deterministic_deny_precedes_inference() {
        let calls = AtomicUsize::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_test(
            Cursor::new(payload_with_command("rm -rf /")),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("deterministic deny must not invoke the model")
            },
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn mode_off_skips_model_inference() {
        let evaluation = evaluate_request(
            &BrainDecisionRequest {
                project: "project".into(),
                tool_name: "Bash".into(),
                tool_input: "cargo test".into(),
                diff_digest: None,
            },
            Some(&enabled_config()),
            BrainGateMode::Off,
            true,
            true,
            |_, _| panic!("mode off must not invoke the model"),
        );

        assert!(matches!(
            evaluation,
            HookEvaluation::Abstain { brain: None, .. }
        ));
    }

    #[test]
    fn active_modes_without_config_use_defaults() {
        for mode in [BrainGateMode::On, BrainGateMode::Auto] {
            let evaluation = evaluate_request(
                &BrainDecisionRequest {
                    project: "project".into(),
                    tool_name: "Bash".into(),
                    tool_input: "cargo test".into(),
                    diff_digest: None,
                },
                None,
                mode,
                true,
                true,
                |config, _| {
                    let defaults = BrainConfig::default();
                    assert_eq!(config.endpoint, defaults.endpoint);
                    assert_eq!(config.model, defaults.model);
                    assert_eq!(config.timeout_ms, defaults.timeout_ms);
                    Ok(suggestion(RuleAction::Approve, 0.9))
                },
            );

            assert!(match mode {
                BrainGateMode::On =>
                    matches!(evaluation, HookEvaluation::Abstain { brain: Some(_), .. }),
                BrainGateMode::Auto => matches!(evaluation, HookEvaluation::Allow { .. }),
                BrainGateMode::Off => unreachable!(),
            });
        }
    }

    #[test]
    fn explicit_mode_on_overrides_legacy_disabled_config_advisorially() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::write(&path, "on").unwrap();
        let mut disabled = enabled_config();
        disabled.enabled = false;
        let resolved = super::super::resolve_gate_mode_at(&path, Some(&disabled));

        let evaluation = evaluate_request(
            &BrainDecisionRequest {
                project: "project".into(),
                tool_name: "Bash".into(),
                tool_input: "cargo test".into(),
                diff_digest: None,
            },
            Some(&disabled),
            resolved.mode,
            true,
            true,
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert!(matches!(
            evaluation,
            HookEvaluation::Abstain { brain: Some(_), .. }
        ));
    }

    #[test]
    fn on_approve_is_audited_without_executable_response() {
        assert_advisory_suggestion(RuleAction::Approve);
    }

    #[test]
    fn on_deny_is_audited_without_executable_response() {
        assert_advisory_suggestion(RuleAction::Deny);
    }

    fn assert_advisory_suggestion(action: RuleAction) {
        let action_label = action.label();
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::On,
            &store,
            |_, _| Ok(suggestion(action, 0.9)),
        );

        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
        assert_eq!(record["brain_action"], action_label);
        assert_eq!(record["user_action"], "hook_proposal");
        assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
        let activity = ActivityStore::at(store.hooks_dir().join("activity.jsonl"));
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Abstained,
            ]
        );
    }

    #[test]
    fn fallthrough_cases_leave_stdout_empty() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());

        let cases = [
            (
                enabled_config(),
                BrainGateMode::On,
                Ok(suggestion(RuleAction::Approve, 0.1)),
            ),
            (
                enabled_config(),
                BrainGateMode::Off,
                Ok(suggestion(RuleAction::Approve, 0.9)),
            ),
            (
                enabled_config(),
                BrainGateMode::On,
                Err("endpoint unavailable".into()),
            ),
        ];
        for (config, gate_mode, inference) in cases {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with_gate_and_store(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&config),
                gate_mode,
                &store,
                |_, _| inference,
            );
            assert!(stdout.is_empty(), "fallthrough wrote stdout");
            assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
        }

        let mut disabled = enabled_config();
        disabled.enabled = false;
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_store(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&disabled),
            super::super::resolve_gate_mode_at(
                &temp.path().join("missing-gate-mode"),
                Some(&disabled),
            )
            .mode,
            &store,
            |_, _| panic!("disabled hook must not infer"),
        );
        assert!(stdout.is_empty());
        assert_eq!(projected_status(&store), Some(ProjectedStatus::NeedsInput));
    }

    #[test]
    fn malformed_payload_leaves_stdout_empty() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_test(
            Cursor::new("not json"),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| panic!("malformed hook must not infer"),
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
    }

    #[test]
    fn persistence_failure_leaves_stdout_empty() {
        let brain_dir = decisions_dir();
        std::fs::create_dir_all(brain_dir.parent().unwrap()).unwrap();
        std::fs::write(&brain_dir, "occupied").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_test(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("persist"));
    }

    #[test]
    fn deterministic_deny_survives_audit_failure() {
        let brain_dir = decisions_dir();
        std::fs::create_dir_all(brain_dir.parent().unwrap()).unwrap();
        std::fs::write(&brain_dir, "occupied").unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_test(
            Cursor::new(payload_with_command("rm -rf /")),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| panic!("deterministic deny must not infer"),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(String::from_utf8(stderr).unwrap().contains("audit"));
    }

    #[test]
    fn failed_stdout_write_records_delivery_failed_without_execution_claim() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            FailingWriter,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events[2].state, ActivityState::Allowed);
        assert_eq!(events[3].state, ActivityState::DeliveryFailed);
        let snapshot = activity.snapshot(Default::default()).unwrap();
        assert!(!snapshot.attention[0].tool_execution_confirmed);
    }

    #[test]
    fn failed_stdout_flush_records_delivery_failed_without_execution_claim() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            FailingFlushWriter,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events[2].state, ActivityState::Allowed);
        assert_eq!(events[3].state, ActivityState::DeliveryFailed);
        assert!(String::from_utf8(stderr).unwrap().contains("flush failed"));
        assert!(
            !activity.snapshot(Default::default()).unwrap().attention[0].tool_execution_confirmed
        );
    }

    #[test]
    fn abrupt_termination_after_visible_bytes_leaves_delivery_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let visible = Rc::new(RefCell::new(Vec::new()));
        let writer = VisibleThenPanicWriter(Rc::clone(&visible));
        let mut stderr = Vec::new();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_with_gate_and_stores(
                Cursor::new(payload()),
                writer,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle,
                Some(&activity),
                |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
            );
        }));

        assert!(result.is_err());
        let response: serde_json::Value = serde_json::from_slice(&visible.borrow()).unwrap();
        assert_eq!(
            response["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert_eq!(
            activity
                .read()
                .unwrap()
                .events()
                .iter()
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
            ]
        );
        let snapshot = activity.snapshot(Default::default()).unwrap();
        assert_eq!(
            snapshot.attention[0].delivery,
            coding_brain_core::brain_activity::DeliveryState::Unknown
        );
        assert!(!snapshot.attention[0].tool_execution_confirmed);
    }

    #[test]
    fn inference_diagnostic_is_redacted_and_bounded() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_test(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            |_, _| Err(format!("token sk-secret-value {}", "x".repeat(16_000))),
        );

        assert!(stdout.is_empty());
        let diagnostic = String::from_utf8(stderr).unwrap();
        assert!(!diagnostic.contains("sk-secret-value"));
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(diagnostic.len() <= MAX_ACTIVITY_FIELD_BYTES + 64);
    }

    #[test]
    fn identical_payloads_are_evaluated_independently() {
        let calls = AtomicUsize::new(0);

        for _ in 0..2 {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_test_with_gate(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                |_, _| {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok(suggestion(RuleAction::Approve, 0.9))
                },
            );
            assert!(!stdout.is_empty());
        }

        assert_eq!(calls.load(Ordering::Relaxed), 2);
        let log = std::fs::read_to_string(decisions_dir().join("decisions.jsonl")).unwrap();
        let records = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        let first_id = records[0]["decision_id"].as_str().unwrap();
        let second_id = records[1]["decision_id"].as_str().unwrap();
        assert!(!first_id.is_empty());
        assert!(!second_id.is_empty());
        assert_ne!(first_id, second_id);
    }
}
