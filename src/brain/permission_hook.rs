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
use coding_brain_core::codex_transcript::{CodexResumeEvidenceError, read_codex_resume_evidence};
#[cfg(test)]
use coding_brain_core::lifecycle::LifecycleEvent;
use coding_brain_core::lifecycle::{
    ApplyOutcome, IgnoreReason, LifecycleIdentity, LifecycleStore, PermissionDisposition,
    coding_brain_state_root,
};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::ProjectIdentity;
use coding_brain_core::provider::AgentProvider;
use coding_brain_core::runtime::BrainGateMode;

use super::UNSUPPORTED_PERMISSION_TOOL_REASON;
use super::activity::{ActivityStore, LiveEvidenceBudget};
use super::client::BrainSuggestion;
use super::decisions::{self, HookDecisionAudit, HookDecisionRecord};
use super::permission_request_lock::PermissionRequestLockStore;
use super::permission_transaction::{
    PermissionTransactionJournal, PermissionTransactionStore, RecoveryLimits, RecoveryReport,
    TransactionError, commit_live, recover_pending_with_guard,
};
use super::query::{self, BrainDecision, BrainDecisionRequest};
use super::safety::SafetyDeny;
use crate::config::BrainConfig;
use crate::lifecycle_hook::read_bounded_hook_input;
use crate::provider_hooks::{
    PermissionHookRequest, ProviderPermissionPolicy, ShellCommandInput, parse_permission,
};

const HOOK_INFERENCE_TIMEOUT_MS: u64 = 25_000;
const PERMISSION_ACTIVITY_LOCK_TIMEOUT_MS: u64 = 500;
const PERMISSION_TRANSACTION_SCHEMA_VERSION: u32 = 2;
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
            provider_session_id: request.lifecycle.provider_session_id().map(str::to_string),
            turn_id: request.lifecycle.turn_id().map(str::to_string),
            tool_use_id: request.tool_use_id.clone(),
            project_id: identity.id().clone(),
            cwd: request.lifecycle.cwd().to_path_buf(),
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        };
        Ok(Self {
            activity_id: gen_activity_id(),
            project,
            session,
            tool: request.tool_name.clone(),
            command: request
                .command
                .as_ref()
                .map(|command| command.source.as_str())
                .map(bounded_redacted_activity_text),
            terminal_command: request
                .command
                .as_ref()
                .map(|command| command.source.as_str())
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

fn evaluate_request<F>(
    request: &BrainDecisionRequest,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    persistence_error: Option<&str>,
    supported: bool,
    infer: F,
) -> HookEvaluation
where
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    if let Some(error) = persistence_error {
        return HookEvaluation::Abstain {
            brain: None,
            reason: format!("initial activity persistence failed: {error}"),
            terminal_state: ActivityState::Error,
        };
    }
    if !supported {
        return HookEvaluation::Abstain {
            brain: None,
            reason: UNSUPPORTED_PERMISSION_TOOL_REASON.into(),
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

fn abstain_without_brain(reason: &str) -> HookEvaluation {
    HookEvaluation::Abstain {
        brain: None,
        reason: reason.into(),
        terminal_state: ActivityState::Abstained,
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
    let _ = writeln!(stderr, "cbrain permission hook: {diagnostic}");
}

fn try_reprove_codex_subagent(
    store: &LifecycleStore,
    identity: &LifecycleIdentity,
) -> Option<CodexResumeEvidenceError> {
    if identity.provider() != AgentProvider::Codex || identity.provider_session_id().is_none() {
        return None;
    }
    match store.codex_subagent_is_proven(identity) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(_) => return Some(CodexResumeEvidenceError::InvalidRecord),
    }
    let Some(path) = identity.transcript_path() else {
        return Some(CodexResumeEvidenceError::MetadataMissing);
    };
    let evidence = match read_codex_resume_evidence(path) {
        Ok(evidence) => evidence,
        Err(error) => return Some(error),
    };
    match store.reprove_codex_subagent(identity, &evidence) {
        Ok(ApplyOutcome::Applied | ApplyOutcome::Ignored(IgnoreReason::Duplicate)) => None,
        Ok(ApplyOutcome::Ignored(_)) | Err(_) => Some(CodexResumeEvidenceError::InvalidRecord),
    }
}

fn permission_transaction_paths() -> Result<(PathBuf, PathBuf), HookDiagnostic> {
    let decisions_path = decisions::decisions_path();
    let Some(state_root) = decisions_path.parent().and_then(|brain| brain.parent()) else {
        return Err(HookDiagnostic::new(
            "could not resolve permission transaction state root",
        ));
    };
    Ok((state_root.to_owned(), decisions_path))
}

fn recovery_blocks_inference(report: RecoveryReport) -> bool {
    report.active != 0
        || report.invalid != 0
        || report.over_budget != 0
        || report.removal_sync_uncertain != 0
        || report.pending != 0
}

fn preflight_blocks_recovery(report: RecoveryReport) -> bool {
    report.active != 0
        || report.invalid != 0
        || report.over_budget != 0
        || report.removal_sync_uncertain != 0
}

fn recover_before_inference(
    state_root: &std::path::Path,
    guard: &super::permission_request_lock::PermissionRequestGuard,
) -> Result<(), HookDiagnostic> {
    let store = PermissionTransactionStore::at(state_root);
    let preflight = store.preflight_live().map_err(|error| {
        HookDiagnostic::new(format!("permission transaction preflight failed: {error}"))
    })?;
    if preflight_blocks_recovery(preflight) {
        return Err(HookDiagnostic::new(format!(
            "permission transaction preflight blocked: active={}, invalid={}, over_budget={}, \
             pending={}, removal_sync_uncertain={}",
            preflight.active,
            preflight.invalid,
            preflight.over_budget,
            preflight.pending,
            preflight.removal_sync_uncertain,
        )));
    }
    let report =
        recover_pending_with_guard(state_root, RecoveryLimits::live(), guard).map_err(|error| {
            HookDiagnostic::new(format!("permission transaction recovery failed: {error}"))
        })?;
    if recovery_blocks_inference(report) {
        return Err(HookDiagnostic::new(format!(
            "permission transaction recovery blocked: active={}, invalid={}, over_budget={}, \
             pending={}, removal_sync_uncertain={}",
            report.active,
            report.invalid,
            report.over_budget,
            report.pending,
            report.removal_sync_uncertain,
        )));
    }
    Ok(())
}

fn preflight_live_decision_evidence(
    path: &std::path::Path,
    budget: &mut LiveEvidenceBudget,
) -> Result<(), HookDiagnostic> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(HookDiagnostic::new("decision evidence is unreadable")),
    };
    let length = usize::try_from(
        file.metadata()
            .map_err(|_| HookDiagnostic::new("decision evidence is unreadable"))?
            .len(),
    )
    .map_err(|_| HookDiagnostic::new("decision evidence exceeds its byte budget"))?;
    if length > budget.remaining() {
        return Err(HookDiagnostic::new(
            "decision evidence exceeds its byte budget",
        ));
    }
    let limit = u64::try_from(budget.remaining()).unwrap_or(u64::MAX);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| HookDiagnostic::new("decision evidence is unreadable"))?;
    if bytes.len() > budget.remaining() {
        return Err(HookDiagnostic::new(
            "decision evidence exceeds its byte budget",
        ));
    }
    budget
        .charge(bytes.len())
        .map_err(|_| HookDiagnostic::new("decision evidence exceeds its byte budget"))?;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.last() != Some(&b'\n')
            || line.len() == 1
            || serde_json::from_slice::<serde_json::Value>(&line[..line.len() - 1]).is_err()
        {
            return Err(HookDiagnostic::new("decision evidence is unreadable"));
        }
    }
    Ok(())
}

fn preflight_live_destinations(
    decisions_path: &std::path::Path,
    activity_store: Option<&ActivityStore>,
) -> Result<(), HookDiagnostic> {
    let mut budget = LiveEvidenceBudget::new(RecoveryLimits::live().max_destination_bytes);
    preflight_live_decision_evidence(decisions_path, &mut budget)?;
    if let Some(activity_store) = activity_store {
        activity_store.read_bounded(&mut budget).map_err(|error| {
            HookDiagnostic::new(format!("activity evidence preflight failed: {error}"))
        })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_permission_transaction(
    state_root: &std::path::Path,
    decisions_path: &std::path::Path,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    request: &PermissionHookRequest,
    audit: &HookDecisionAudit<'_>,
    mut terminal: ActivityEvent,
    proposal_action: &str,
    disposition: PermissionDisposition,
) -> Result<String, TransactionError> {
    let decision_id = decisions::gen_decision_id();
    let proposal = HookDecisionRecord::from_audit(audit, decision_id.clone(), proposal_action);
    terminal.decision_id = Some(decision_id.clone());
    let journal = PermissionTransactionJournal {
        schema_version: PERMISSION_TRANSACTION_SCHEMA_VERSION,
        transaction_id: format!("transaction-{decision_id}"),
        proposal,
        allow_requires_lifecycle_authority: terminal.state == ActivityState::Allowed,
        terminal,
        lifecycle_identity: request.lifecycle.clone(),
        request_key: request.request_key.clone(),
        disposition,
    };
    let limits = RecoveryLimits::live();
    let prepared = PermissionTransactionStore::at(state_root).prepare_live(journal)?;
    let mut budget = LiveEvidenceBudget::new(limits.max_destination_bytes);
    commit_live(
        prepared,
        lifecycle_store,
        activity_store,
        decisions_path,
        &mut budget,
    )?;
    decisions::trigger_distill();
    Ok(decision_id)
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
    stdout: W,
    stderr: E,
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
    #[cfg(not(test))]
    let safety_evaluator = super::safety::evaluate_isolated;
    #[cfg(test)]
    let safety_evaluator = super::safety::evaluate_in_process;
    run_provider_with_gate_and_stores_and_safety(
        stdin,
        stdout,
        stderr,
        config,
        gate_mode,
        lifecycle_store,
        activity_store,
        provider,
        antigravity_event,
        safety_evaluator,
        infer,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_provider_with_gate_and_stores_and_safety<R, W, E, S, F>(
    stdin: R,
    mut stdout: W,
    mut stderr: E,
    config: Option<&BrainConfig>,
    gate_mode: BrainGateMode,
    lifecycle_store: &LifecycleStore,
    activity_store: Option<&ActivityStore>,
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    safety_evaluator: S,
    infer: F,
) where
    R: Read,
    W: Write,
    E: Write,
    S: FnOnce(Option<&ShellCommandInput>) -> super::safety::SafetyEvaluation,
    F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
{
    let permission_activity_store = activity_store
        .cloned()
        .map(|store| store.with_lock_timeout_ms(PERMISSION_ACTIVITY_LOCK_TIMEOUT_MS));
    let activity_store = permission_activity_store.as_ref();
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
    let transaction_paths = match permission_transaction_paths() {
        Ok(paths) => paths,
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    };
    let request_guard = match PermissionRequestLockStore::at(&transaction_paths.0)
        .try_acquire(&request.lifecycle, &request.request_key)
    {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            write_diagnostic(&mut stderr, "permission request is already active");
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
        Err(error) => {
            write_diagnostic(&mut stderr, error);
            if provider == AgentProvider::Antigravity {
                write_failsafe_ask(&mut stdout, &mut stderr);
            }
            return;
        }
    };
    if let Err(error) = recover_before_inference(&transaction_paths.0, &request_guard) {
        write_diagnostic(&mut stderr, error);
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    }
    if let Err(error) = preflight_live_destinations(&transaction_paths.1, activity_store) {
        write_diagnostic(&mut stderr, error);
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    }
    let existing_decision =
        match lifecycle_store.permission_decision(&request.lifecycle, &request.request_key) {
            Ok(decision) => decision,
            Err(error) => {
                write_diagnostic(
                    &mut stderr,
                    format!("permission transaction admission failed: {error}"),
                );
                if provider == AgentProvider::Antigravity {
                    write_failsafe_ask(&mut stdout, &mut stderr);
                }
                return;
            }
        };
    let legacy_disposition = lifecycle_store
        .permission_disposition(&request.lifecycle, &request.request_key)
        .ok()
        .flatten();
    if existing_decision.is_some() || legacy_disposition.is_some() {
        write_diagnostic(
            &mut stderr,
            "permission transaction admission blocked: duplicate permission request",
        );
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    }
    let needs_input = |stderr: &mut E| {
        if let Err(error) = lifecycle_store.ensure_permission_decision(
            &request.lifecycle,
            &request.request_key,
            coding_brain_core::lifecycle::PermissionDecision::NeedsInput,
        ) {
            write_diagnostic(stderr, error);
        }
    };
    let _request_guard = request_guard;
    let activity_context =
        current_paths().and_then(|paths| HookActivity::from_request(&request, &paths));
    let reproof_error = try_reprove_codex_subagent(lifecycle_store, &request.lifecycle);
    let initial_activity_error = match (&activity_context, activity_store) {
        (Err(error), _) => Some(error.to_string()),
        (_, None) => Some("activity store unavailable".into()),
        (Ok(context), Some(activity_store)) => activity_store
            .append_batch(&[
                context.event(ActivityState::Observed),
                context.event(ActivityState::Evaluating),
            ])
            .err()
            .map(|error| error.to_string()),
    };
    let persistence_error = initial_activity_error.or_else(|| {
        reproof_error.map(|error| format!("UnprovenSubagent; Codex resume evidence: {error}"))
    });
    let brain_request = BrainDecisionRequest {
        project: request.project.clone(),
        tool_name: request.tool_name.clone(),
        tool_input: request
            .command
            .as_ref()
            .map(|command| command.source.clone())
            .unwrap_or_default(),
        diff_digest: None,
    };
    // This is the authoritative deterministic safety and provider-policy
    // boundary; evaluate_request performs model evaluation only.
    let safety_evaluation = safety_evaluator(request.command.as_ref());
    let evaluation = match safety_evaluation {
        super::safety::SafetyEvaluation::Deny(safety) => HookEvaluation::Deny {
            brain: None,
            deterministic: true,
            safety: Some(safety),
            terminal_state: ActivityState::Denied,
        },
        _ if request.provider_policy == ProviderPermissionPolicy::Denies => HookEvaluation::Deny {
            brain: None,
            deterministic: true,
            safety: None,
            terminal_state: ActivityState::Denied,
        },
        super::safety::SafetyEvaluation::Indeterminate(error) => {
            abstain_without_brain(error.reason())
        }
        super::safety::SafetyEvaluation::NoDeterministicDecision => {
            let model_evaluation = evaluate_request(
                &brain_request,
                config,
                gate_mode,
                persistence_error.as_deref(),
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
        let mut transaction_error = persistence_error
            .as_ref()
            .map(|error| format!("permission transaction unavailable: {error}"));
        let decision_id = if transaction_error.is_none()
            && let ((state_root, decisions_path), Ok(context), Some(activity_store)) =
                (&transaction_paths, &activity_context, activity_store)
        {
            let mut terminal = context.event(*terminal_state);
            terminal.rule_id = safety.as_ref().map(|deny| deny.rule_id.into());
            terminal.reasoning = Some(reason.into());
            match commit_permission_transaction(
                state_root,
                decisions_path,
                lifecycle_store,
                activity_store,
                &request,
                &audit,
                terminal,
                "deterministic_deny",
                PermissionDisposition::Decided,
            ) {
                Ok(decision_id) => Some(decision_id),
                Err(error) => {
                    transaction_error = Some(format!("permission transaction failed: {error}"));
                    None
                }
            }
        } else {
            transaction_error.get_or_insert_with(|| {
                "permission transaction unavailable for deterministic deny".into()
            });
            None
        };
        if let Some(error) = &transaction_error {
            let mut diagnostic = format!("deterministic deny audit: {error}");
            if let Some(reproof_error) = reproof_error {
                diagnostic.push_str(&format!("; Codex resume evidence: {reproof_error}"));
            }
            write_diagnostic(&mut stderr, diagnostic);
            if let Err(error) = lifecycle_store.ensure_permission_decision(
                &request.lifecycle,
                &request.request_key,
                coding_brain_core::lifecycle::PermissionDecision::NeedsInput,
            ) {
                write_diagnostic(
                    &mut stderr,
                    format!("could not persist deny state: {error}"),
                );
            }
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

    let bounded_reasoning = bounded_redacted_activity_text(&brain.reasoning);
    let persisted_action = match terminal_state {
        ActivityState::Allowed => "approve",
        ActivityState::Denied => "deny",
        ActivityState::Abstained => match brain.action.as_str() {
            "approve" => "approve",
            "deny" => "deny",
            _ => "abstain",
        },
        ActivityState::Error => "abstain",
        _ => unreachable!("permission transaction requires a terminal state"),
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
        brain_action: persisted_action,
        brain_confidence: brain.confidence,
        brain_reasoning: &bounded_reasoning,
        brain_source: brain.source,
        brain_threshold: brain.threshold,
        session_id: request.lifecycle.session_id(),
        turn_id: request.lifecycle.turn_id().unwrap_or_default(),
    };
    let mut terminal = activity_context.as_ref().unwrap().event(terminal_state);
    terminal.confidence = Some(brain.confidence);
    terminal.threshold = brain.threshold;
    terminal.reasoning = Some(bounded_reasoning.clone());
    let disposition = if behavior.is_some() {
        PermissionDisposition::Decided
    } else {
        PermissionDisposition::NeedsInput
    };
    let transaction_result = match activity_store {
        Some(activity_store) => commit_permission_transaction(
            &transaction_paths.0,
            &transaction_paths.1,
            lifecycle_store,
            activity_store,
            &request,
            &audit,
            terminal,
            "hook_proposal",
            disposition,
        )
        .map_err(|error| format!("permission transaction failed: {error}")),
        None => Err("permission transaction unavailable: activity store unavailable".into()),
    };
    let decision_id = match transaction_result {
        Ok(decision_id) => Some(decision_id),
        Err(mut error) => {
            if behavior == Some(PermissionBehavior::Allow)
                && let Some(reproof_error) = reproof_error
            {
                error.push_str(&format!("; Codex resume evidence: {reproof_error}"));
            }
            write_diagnostic(&mut stderr, &error);
            if behavior == Some(PermissionBehavior::Deny) {
                if let Err(lifecycle_error) = lifecycle_store.ensure_permission_decision(
                    &request.lifecycle,
                    &request.request_key,
                    coding_brain_core::lifecycle::PermissionDecision::NeedsInput,
                ) {
                    write_diagnostic(
                        &mut stderr,
                        format!("could not persist deny state: {lifecycle_error}"),
                    );
                }
                None
            } else {
                needs_input(&mut stderr);
                if provider == AgentProvider::Antigravity {
                    write_failsafe_ask(&mut stdout, &mut stderr);
                }
                return;
            }
        }
    };
    let Some(serialized) = serialized else {
        let _ = activity_store.unwrap().compact_if_needed();
        if brain.source == "error" {
            write_diagnostic(&mut stderr, &brain.reasoning);
        }
        if provider == AgentProvider::Antigravity {
            write_failsafe_ask(&mut stdout, &mut stderr);
        }
        return;
    };
    let (delivery, failure) = match write_response(&mut stdout, &serialized) {
        Ok(()) => (ActivityState::Delivered, None),
        Err(error) => {
            let message = format!("could not write response: {error}");
            write_diagnostic(&mut stderr, &message);
            (ActivityState::DeliveryFailed, Some(message))
        }
    };
    let mut event = activity_context.as_ref().unwrap().event(delivery);
    event.decision_id = decision_id;
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
    // run_with_gate_and_stores is the authoritative safety and provider-policy boundary.
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
    use std::fs::OpenOptions;
    use std::io::Cursor;
    use std::panic::AssertUnwindSafe;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    use super::*;
    use crate::brain::activity::{ActivityStore, ReadParseGate};
    use crate::brain::client::BrainSuggestion;
    use crate::brain::decisions::decisions_dir;
    use crate::config::BrainConfig;
    use crate::rules::RuleAction;
    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState,
        MAX_ACTIVITY_FIELD_BYTES, ProjectEvidence, bounded_redacted_activity_text,
    };
    use coding_brain_core::lifecycle::{LifecycleEventKind, LifecycleStore, ProjectedStatus};
    use fs2::FileExt;

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

    struct RestorePathEnvironment {
        home: Option<OsString>,
        config: Option<OsString>,
        state: Option<OsString>,
    }

    impl Drop for RestorePathEnvironment {
        fn drop(&mut self) {
            // SAFETY: every test that changes these variables holds HOME_ENV_LOCK.
            unsafe {
                for (name, value) in [
                    ("HOME", self.home.take()),
                    ("XDG_CONFIG_HOME", self.config.take()),
                    ("XDG_STATE_HOME", self.state.take()),
                ] {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    fn set_test_path_environment(path: &Path) -> RestorePathEnvironment {
        let restore = RestorePathEnvironment {
            home: std::env::var_os("HOME"),
            config: std::env::var_os("XDG_CONFIG_HOME"),
            state: std::env::var_os("XDG_STATE_HOME"),
        };
        // SAFETY: every caller holds HOME_ENV_LOCK.
        unsafe {
            std::env::set_var("HOME", path);
            std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
            std::env::set_var("XDG_STATE_HOME", path.join(".local/state"));
        }
        restore
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

    fn realistic_activity_event(index: u64) -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: format!("scale-{index}"),
            recorded_at_ms: index,
            project: ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Temporary(
                    "scale-project".into(),
                ),
                cwd: PathBuf::from("/work/scale-project"),
                label: Some("scale-project".into()),
            },
            session: None,
            state: ActivityState::Denied,
            tool: Some("Bash".into()),
            normalized_command: Some(format!("command-{index}")),
            fingerprint: Some(format!("fingerprint-{index}")),
            rule_id: Some("scale".into()),
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("x".repeat(512)),
            decision_id: Some(format!("decision-{index}")),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn realistically_sized_permission_events() -> Vec<ActivityEvent> {
        (0..32_448).map(realistic_activity_event).collect()
    }

    fn permission_payload_for_provider(provider: AgentProvider, provider_deny: bool) -> Vec<u8> {
        let cwd = std::env::current_dir().unwrap();
        let mut payload: serde_json::Value = match provider {
            AgentProvider::Codex => serde_json::from_str(&payload()).unwrap(),
            AgentProvider::Claude => serde_json::from_slice(include_bytes!(
                "../../tests/fixtures/hooks/claude-permission-request.json"
            ))
            .unwrap(),
            AgentProvider::Antigravity => serde_json::from_slice(include_bytes!(
                "../../tests/fixtures/hooks/antigravity-pre-tool-use.json"
            ))
            .unwrap(),
        };
        match provider {
            AgentProvider::Codex | AgentProvider::Claude => {
                payload["cwd"] = serde_json::json!(cwd);
            }
            AgentProvider::Antigravity => {
                payload["workspacePaths"] = serde_json::json!([cwd]);
            }
        }
        if provider_deny {
            payload["permission_suggestions"] = serde_json::json!([{ "behavior": "deny" }]);
        }
        serde_json::to_vec(&payload).unwrap()
    }

    fn permission_payload_for_provider_command(provider: AgentProvider, command: &str) -> Vec<u8> {
        let mut payload: serde_json::Value =
            serde_json::from_slice(&permission_payload_for_provider(provider, false)).unwrap();
        match provider {
            AgentProvider::Codex | AgentProvider::Claude => {
                payload["tool_input"]["command"] = serde_json::json!(command);
            }
            AgentProvider::Antigravity => {
                payload["toolCall"]["args"]["CommandLine"] = serde_json::json!(command);
            }
        }
        serde_json::to_vec(&payload).unwrap()
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
        assert_eq!(
            request
                .command
                .as_ref()
                .map(|command| command.source.as_str()),
            Some("cargo test")
        );
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
        assert!(blocked_activity.read().unwrap().events().is_empty());
    }

    #[test]
    fn terminal_activity_failure_compensates_antigravity_allow_to_needs_input() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(temp.path());
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let saved_activity_path = temp.path().join("activity-before-failure.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Antigravity,
            "agy-conversation-1".into(),
            Some("invocation-1".into()),
            Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
            temp.path().to_path_buf(),
        )
        .unwrap();
        assert_eq!(
            lifecycle
                .record(
                    LifecycleEvent::from_parts_with_turn_initial_step(
                        identity,
                        LifecycleEventKind::UserPromptSubmit,
                        Some(5),
                    )
                    .unwrap(),
                )
                .unwrap(),
            ApplyOutcome::Applied
        );
        let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
            "../../tests/fixtures/hooks/antigravity-pre-tool-use.json"
        ))
        .unwrap();
        payload["workspacePaths"] = serde_json::json!([temp.path()]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_provider_with_gate_and_stores(
            Cursor::new(serde_json::to_vec(&payload).unwrap()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            AgentProvider::Antigravity,
            Some("PreToolUse"),
            |_, _| {
                std::fs::rename(&activity_path, &saved_activity_path).unwrap();
                std::fs::create_dir(&activity_path).unwrap();
                Ok(suggestion(RuleAction::Approve, 0.9))
            },
        );

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
            "ask"
        );
        let key = coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Antigravity,
            "agy-conversation-1",
        )
        .storage_key();
        assert_eq!(
            lifecycle.read().unwrap().snapshot.unwrap().sessions[&key].projected_status,
            Some(ProjectedStatus::NeedsInput)
        );
        let saved = ActivityStore::at(&saved_activity_path).read().unwrap();
        assert_eq!(
            saved
                .events()
                .iter()
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [ActivityState::Observed, ActivityState::Evaluating]
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

        assert_eq!(
            request
                .command
                .as_ref()
                .map(|command| command.source.as_str()),
            Some("  cargo test --lib  ")
        );
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
    fn every_indeterminate_shell_analysis_preserves_native_confirmation_without_inference() {
        for error in [
            super::super::safety::ShellAnalysisError::UnsupportedDialect,
            super::super::safety::ShellAnalysisError::UnsupportedSyntax,
            super::super::safety::ShellAnalysisError::ResourceLimit,
            super::super::safety::ShellAnalysisError::HelperFailure,
        ] {
            for provider in [
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                let input = permission_payload_for_provider(provider, false);
                if provider == AgentProvider::Antigravity {
                    let identity = LifecycleIdentity::try_new(
                        AgentProvider::Antigravity,
                        "agy-conversation-1".into(),
                        Some("invocation-1".into()),
                        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
                        std::env::current_dir().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        lifecycle
                            .record(
                                LifecycleEvent::from_parts_with_turn_initial_step(
                                    identity,
                                    LifecycleEventKind::UserPromptSubmit,
                                    Some(5),
                                )
                                .unwrap(),
                            )
                            .unwrap(),
                        ApplyOutcome::Applied
                    );
                }
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();

                run_provider_with_gate_and_stores_and_safety(
                    Cursor::new(input),
                    &mut stdout,
                    &mut stderr,
                    Some(&enabled_config()),
                    BrainGateMode::Auto,
                    &lifecycle,
                    Some(&activity),
                    provider,
                    (provider == AgentProvider::Antigravity).then_some("PreToolUse"),
                    |_| super::super::safety::SafetyEvaluation::Indeterminate(error),
                    |_, _| panic!("indeterminate shell analysis must not invoke the model"),
                );

                if provider == AgentProvider::Antigravity {
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
                        "ask",
                        "{error:?}"
                    );
                } else {
                    assert!(stdout.is_empty(), "{provider:?}: {error:?}");
                }
                assert!(
                    stderr.is_empty(),
                    "{provider:?}: {error:?}: {}",
                    String::from_utf8_lossy(&stderr)
                );
                assert!(
                    activity
                        .read()
                        .unwrap()
                        .events()
                        .iter()
                        .any(|event| event.state == ActivityState::Abstained),
                    "{provider:?}: {error:?}"
                );
                let session_id = match provider {
                    AgentProvider::Codex => "session-1",
                    AgentProvider::Claude => "claude-session-1",
                    AgentProvider::Antigravity => "agy-conversation-1",
                };
                let key =
                    coding_brain_core::provider::AgentSessionKey::native(provider, session_id)
                        .storage_key();
                assert_eq!(
                    lifecycle.read().unwrap().snapshot.unwrap().sessions[&key].projected_status,
                    Some(ProjectedStatus::NeedsInput),
                    "{provider:?}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn nested_shell_safety_precedes_inference_for_every_provider() {
        for (command, proven_deny) in [
            ("sh -c 'rm --no-preserve-root -rf /'", true),
            ("busybox env sh -c 'rm --no-preserve-root -rf /'", true),
            ("busybox time sh -c 'rm --no-preserve-root -rf /'", true),
            (
                "VALUE='log sh'; /usr/bin/time -o $VALUE 'rm --no-preserve-root -rf /'",
                true,
            ),
            (
                "/usr/bin/env -u {HOME,sh} 'rm --no-preserve-root -rf /'",
                true,
            ),
            ("busybox time -o * 'rm --no-preserve-root -rf /'", true),
            (
                "VALUE='HOME sh'; busybox env -u $VALUE 'rm --no-preserve-root -rf /'",
                true,
            ),
            ("env -S 'rm --no-preserve-root -rf /'", true),
            ("toybox env sh -c 'rm --no-preserve-root -rf /'", true),
            ("toybox time sh -c 'rm --no-preserve-root -rf /'", true),
            ("busybox ls", false),
            (
                "busybox time --unknown sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("busybox time -h sh -c 'rm --no-preserve-root -rf /'", false),
            (
                "busybox env --help sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("busybox env -v sh -c 'rm --no-preserve-root -rf /'", false),
            (
                "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            (
                "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            (
                "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            (
                "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'", false),
            ("env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'", false),
            (
                "/usr/bin/time \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("env \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'", false),
            (
                "env FOO=bar \"$COMMAND\" sh -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("env --split-string 'rm -rf /'", false),
            ("env --split='rm -rf /'", false),
            ("env $'-\\x53' 'rm -rf /'", false),
            ("/usr/bin/time -p sh -c 'rm --no-preserve-root -rf /'", true),
            ("/usr/bin/env -i sh -c 'rm --no-preserve-root -rf /'", true),
            ("time -- eval 'rm --no-preserve-root -rf /'", true),
            ("time -p -- ! sh -c 'rm --no-preserve-root -rf /'", true),
            ("eval -- 'rm --no-preserve-root -rf /'", true),
            ("builtin -- eval 'rm --no-preserve-root -rf /'", true),
            ("source /definitely/not-read-by-safety", false),
            (". /dev/stdin", false),
            ("builtin -- source \"$FILE\"", false),
            ("trap 'rm --no-preserve-root -rf /' EXIT", true),
            (
                "TARGET=/; TARGET=/tmp/safe trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
                true,
            ),
            (
                "TARGET=/tmp/safe; TARGET=/ trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
                false,
            ),
            ("builtin -- trap ':' EXIT", false),
            ("trap \"$ACTION\" EXIT", false),
            ("sh -c 'trap -p EXIT'", false),
            ("mapfile -c1 -C 'rm --no-preserve-root -rf /'", true),
            ("mapfile -c +1 -C 'rm --no-preserve-root -rf /'", true),
            ("mapfile -C 'rm --no-preserve-root -rf /' -c 0", false),
            (
                "mapfile -C 'rm --no-preserve-root -rf /' -n 4294967296",
                false,
            ),
            ("mapfile -C 'rm --no-preserve-root -rf /' -c '1\n'", false),
            ("builtin -- readarray -c1 -C ':'", false),
            ("mapfile -c1 -C \"$CALLBACK\"", false),
            ("mapfile -c", false),
            (
                "bash --posix -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
                true,
            ),
            (
                "sh -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
                false,
            ),
            (
                "sh -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
            ),
            (
                "bash --posix -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
            ),
            (
                "set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
                true,
            ),
            (
                "(set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\")",
                true,
            ),
            (
                "bash --posix -c 'alias wipe=\"rm --no-preserve-root -rf /\"\nwipe'",
                false,
            ),
            (
                "eval 'shopt -s expand_aliases\nalias wipe=\"rm --no-preserve-root -rf /\"\nwipe'",
                false,
            ),
            (
                "bash --posix -c 'alias load=source\nload /tmp/attacker-controlled-script'",
                false,
            ),
            (
                "eval 'shopt -s expand_aliases\nalias load=source\nload /tmp/attacker-controlled-script'",
                false,
            ),
            (
                "hash -p /tmp/attacker-controlled-executable wipe; wipe",
                false,
            ),
            (
                "enable -f /tmp/attacker-controlled-builtin.so wipe; wipe",
                false,
            ),
            (
                "dash -c 'TARGET=/; time eval \"TARGET=/tmp/safe\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
            ),
            (
                "sh -c 'TARGET=/; builtin eval \"TARGET=/tmp/safe\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
            ),
            ("arbitrary_mutator; rm -f \"$HOME\" /", true),
            ("sh -c \"$PROGRAM\"", false),
            ("builtin exec sh -c \"$PROGRAM\"", false),
            ("builtin command sh -c \"$PROGRAM\"", false),
            ("builtin builtin eval \"$PROGRAM\"", false),
            ("BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'", false),
            (
                "env BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
                false,
            ),
            ("env -a displayed bash -c 'printf ok'", false),
            (
                "env --argv0=displayed /bin/bash -c 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("env --argv0", false),
            ("exec -cla displayed bash -c 'printf ok'", false),
            (
                "exec -cla displayed /bin/bash -c 'rm --no-preserve-root -rf /'",
                true,
            ),
            ("HOME=/tmp/safe; sh -c 'rm -rf \"$HOME\"'", true),
            ("sudo bash -c 'rm --no-preserve-root -rf \"$HOME\"'", true),
            ("sudo bash -c 'rm --no-preserve-root -rf /root'", false),
            (
                "sudo -H bash -c 'rm --no-preserve-root -rf \"$HOME\"'",
                true,
            ),
            ("bash -cz 'rm --no-preserve-root -rf /'", false),
            (
                "shopt -s lastpipe; export BASHOPTS; bash -c 'TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
            ),
            (
                "shopt -s lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
                true,
            ),
            (
                "shopt -s lastpipe; env shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
                true,
            ),
            (
                "shopt -s lastpipe; sudo shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
                true,
            ),
            (
                "shopt -s lastpipe; /usr/bin/time shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
                true,
            ),
            (
                "shopt -s lastpipe; exec shopt -u lastpipe; X=; printf 1 | read X; rm -f \"${X:+-rf}\" /",
                true,
            ),
            (
                "BASH_ENV=/dev/fd/3; export BASH_ENV; bash -c 'rm -rf /tmp/safe' 3<<<'rm(){ printf OVERRIDDEN; }'",
                false,
            ),
            ("time -- BASH_ENV=/tmp/startup sh -c 'printf ok'", false),
            (
                "time -- BASH_ENV=/tmp/startup sh -c 'rm --no-preserve-root -rf /'",
                true,
            ),
            ("eval \"$UNKNOWN\" | cat", false),
            (
                "sudo BASH_ENV=/tmp/attacker-startup bash -c 'printf ok'",
                false,
            ),
            ("sudo X-Y=z sh -c 'rm --no-preserve-root -rf /'", true),
            ("env /X=z sh -c 'rm --no-preserve-root -rf /'", true),
            ("sudo -nE bash -c 'rm --no-preserve-root -rf /'", true),
            ("sudo -E bash -c 'printf ok'", false),
            ("sudo --preserve-env=BASH_ENV bash -c 'printf ok'", false),
            ("sudo -ni bash -c 'printf ok'", false),
            ("sudo --shell bash -c 'printf ok'", false),
            ("sudo -i printf ok", false),
            ("sudo -s '$DANGER'", false),
            (
                "sudo DANGER='rm --no-preserve-root -rf /' -s '$DANGER'",
                false,
            ),
            (
                "builtin exec sudo DANGER='rm --no-preserve-root -rf /' -s '$DANGER'",
                false,
            ),
            ("sudo -E", false),
            ("sudo --shell rm --no-preserve-root -rf /", true),
            (
                "builtin exec sudo --shell rm --no-preserve-root -rf /",
                true,
            ),
            (
                "TARGET=/; sudo eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; builtin exec sudo eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; env eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; /usr/bin/time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; command time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; FOO=bar time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; 'time' eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; >/dev/null time eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; command -x eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; builtin command -P eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; command FOO=bar eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; 'FOO=bar' eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; time time FOO\\=bar eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; builtin command -- FOO=bar eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "TARGET=/; sudo -s eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\"",
                true,
            ),
            (
                "command sudo -s time eval 'rm --no-preserve-root -rf /'",
                true,
            ),
            (
                "builtin command sudo -i time eval 'rm --no-preserve-root -rf /'",
                true,
            ),
            ("sudo -s 'time' eval 'rm --no-preserve-root -rf /'", true),
            (
                "sudo -s -- FOO=bar eval 'rm --no-preserve-root -rf /'",
                false,
            ),
            ("sudo --unknown bash -c 'printf ok'", true),
            ("sudo --pre bash -c 'printf ok'", true),
            ("sudo \"$OPTIONS\" bash -c 'printf ok'", true),
            ("zsh -c 'printf ok'", false),
        ] {
            for provider in [
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                if provider == AgentProvider::Antigravity {
                    let identity = LifecycleIdentity::try_new(
                        AgentProvider::Antigravity,
                        "agy-conversation-1".into(),
                        Some("invocation-1".into()),
                        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
                        std::env::current_dir().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        lifecycle
                            .record(
                                LifecycleEvent::from_parts_with_turn_initial_step(
                                    identity,
                                    LifecycleEventKind::UserPromptSubmit,
                                    Some(5),
                                )
                                .unwrap(),
                            )
                            .unwrap(),
                        ApplyOutcome::Applied
                    );
                }
                let calls = AtomicUsize::new(0);
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();

                run_provider_with_gate_and_stores(
                    Cursor::new(permission_payload_for_provider_command(provider, command)),
                    &mut stdout,
                    &mut stderr,
                    Some(&enabled_config()),
                    BrainGateMode::Auto,
                    &lifecycle,
                    Some(&activity),
                    provider,
                    (provider == AgentProvider::Antigravity).then_some("PreToolUse"),
                    |_, _| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        panic!("nested shell safety must not invoke the model")
                    },
                );

                assert_eq!(calls.load(Ordering::SeqCst), 0, "{provider:?}: {command}");
                if provider == AgentProvider::Antigravity {
                    let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
                    assert_eq!(
                        output["decision"],
                        if proven_deny { "deny" } else { "ask" },
                        "{provider:?}: {command}"
                    );
                } else if proven_deny {
                    let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
                    assert_eq!(
                        output["hookSpecificOutput"]["decision"]["behavior"], "deny",
                        "{provider:?}: {command}"
                    );
                } else {
                    assert!(stdout.is_empty(), "{provider:?}: {command}");
                }
                assert!(
                    stderr.is_empty(),
                    "{provider:?}: {command}: {}",
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    }

    #[test]
    fn inherited_shell_state_precedes_inference_for_every_provider() {
        for (command, proven_deny, safety) in [
            (
                "printf ok",
                false,
                super::super::safety::evaluate_in_process_with_inherited_startup
                    as fn(Option<&ShellCommandInput>) -> super::super::safety::SafetyEvaluation,
            ),
            (
                "rm --no-preserve-root -rf /",
                true,
                super::super::safety::evaluate_in_process_with_inherited_startup,
            ),
            (
                "bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
                true,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
            (
                "bash -pc 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
                false,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
            (
                "BASH_ENV=/tmp/attacker-startup set -o posix; bash -c 'printf ok'",
                false,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
            (
                "BASH_ENV=/tmp/attacker-startup builtin set -o posix; bash -c 'printf ok'",
                false,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
            (
                "BASH_ENV=/tmp/attacker-startup command set -o posix; bash -c 'printf ok'",
                false,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
            (
                "BASH_ENV=/tmp/attacker-startup set -o posix; bash -c 'printf ok'; rm --no-preserve-root -rf /",
                true,
                super::super::safety::evaluate_in_process_with_inherited_posix,
            ),
        ] {
            for provider in [
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                if provider == AgentProvider::Antigravity {
                    let identity = LifecycleIdentity::try_new(
                        AgentProvider::Antigravity,
                        "agy-conversation-1".into(),
                        Some("invocation-1".into()),
                        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
                        std::env::current_dir().unwrap(),
                    )
                    .unwrap();
                    lifecycle
                        .record(
                            LifecycleEvent::from_parts_with_turn_initial_step(
                                identity,
                                LifecycleEventKind::UserPromptSubmit,
                                Some(5),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                }
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();

                run_provider_with_gate_and_stores_and_safety(
                    Cursor::new(permission_payload_for_provider_command(provider, command)),
                    &mut stdout,
                    &mut stderr,
                    Some(&enabled_config()),
                    BrainGateMode::Auto,
                    &lifecycle,
                    Some(&activity),
                    provider,
                    (provider == AgentProvider::Antigravity).then_some("PreToolUse"),
                    safety,
                    |_, _| panic!("inherited startup uncertainty must not invoke the model"),
                );

                if provider == AgentProvider::Antigravity {
                    let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
                    assert_eq!(
                        output["decision"],
                        if proven_deny { "deny" } else { "ask" },
                        "{provider:?}: {command}"
                    );
                } else if proven_deny {
                    let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
                    assert_eq!(
                        output["hookSpecificOutput"]["decision"]["behavior"], "deny",
                        "{provider:?}: {command}"
                    );
                } else {
                    assert!(stdout.is_empty(), "{provider:?}: {command}");
                }
                assert!(
                    stderr.is_empty(),
                    "{provider:?}: {command}: {}",
                    String::from_utf8_lossy(&stderr)
                );
            }
        }
    }

    #[test]
    fn benign_literal_nested_shell_invokes_inference_once_for_every_provider() {
        for command in [
            "sh -c 'printf %s ok'",
            "source",
            ".",
            "trap -p EXIT",
            "trap - EXIT",
            "trap '' EXIT",
            "mapfile",
            "readarray -c1 -- '-Cprintf DANGER'",
        ] {
            for provider in [
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ] {
                let temp = tempfile::tempdir().unwrap();
                let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
                let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
                if provider == AgentProvider::Antigravity {
                    let identity = LifecycleIdentity::try_new(
                        AgentProvider::Antigravity,
                        "agy-conversation-1".into(),
                        Some("invocation-1".into()),
                        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
                        std::env::current_dir().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        lifecycle
                            .record(
                                LifecycleEvent::from_parts_with_turn_initial_step(
                                    identity,
                                    LifecycleEventKind::UserPromptSubmit,
                                    Some(5),
                                )
                                .unwrap(),
                            )
                            .unwrap(),
                        ApplyOutcome::Applied
                    );
                }
                let calls = AtomicUsize::new(0);
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();

                run_provider_with_gate_and_stores(
                    Cursor::new(permission_payload_for_provider_command(provider, command)),
                    &mut stdout,
                    &mut stderr,
                    Some(&enabled_config()),
                    BrainGateMode::Auto,
                    &lifecycle,
                    Some(&activity),
                    provider,
                    (provider == AgentProvider::Antigravity).then_some("PreToolUse"),
                    |_, _| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(suggestion(RuleAction::Approve, 0.9))
                    },
                );

                assert_eq!(calls.load(Ordering::SeqCst), 1, "{provider:?}: {command}");
                assert!(stderr.is_empty(), "{provider:?}: {command}");
            }
        }
    }

    #[test]
    fn provider_policy_deny_precedes_indeterminate_shell_analysis() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let input = permission_payload_for_provider(AgentProvider::Claude, true);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_provider_with_gate_and_stores_and_safety(
            Cursor::new(input),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            AgentProvider::Claude,
            None,
            |_| {
                super::super::safety::SafetyEvaluation::Indeterminate(
                    super::super::safety::ShellAnalysisError::UnsupportedSyntax,
                )
            },
            |_, _| panic!("provider deny must not invoke the model"),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(stderr.is_empty());
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
            None,
            true,
            |_, _| panic!("mode off must not invoke the model"),
        );

        assert!(matches!(
            evaluation,
            HookEvaluation::Abstain { brain: None, .. }
        ));
    }

    #[test]
    fn initial_persistence_failure_reports_specific_cause() {
        let evaluation = evaluate_request(
            &BrainDecisionRequest {
                project: "project".into(),
                tool_name: "Bash".into(),
                tool_input: "cargo test".into(),
                diff_digest: None,
            },
            Some(&enabled_config()),
            BrainGateMode::Auto,
            Some("activity store lock timed out"),
            true,
            |_, _| panic!("persistence failure must not invoke the model"),
        );

        assert!(matches!(
            evaluation,
            HookEvaluation::Abstain {
                brain: None,
                reason,
                terminal_state: ActivityState::Error,
            } if reason
                == "initial activity persistence failed: activity store lock timed out"
        ));
    }

    #[test]
    fn locked_activity_store_fails_closed_with_specific_bounded_diagnostic() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(activity_path.with_extension("lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_worker = Arc::clone(&calls);
        let lifecycle_for_worker = lifecycle.clone();
        let activity_for_worker = activity.clone();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with_gate_and_stores(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle_for_worker,
                Some(&activity_for_worker),
                |_, _| {
                    calls_for_worker.fetch_add(1, Ordering::SeqCst);
                    panic!("locked activity store must not invoke the model")
                },
            );
            result_tx.send((stdout, stderr)).unwrap();
        });

        let (stdout, stderr) = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(stdout.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("activity store lock timed out")
        );
        assert!(!lifecycle.snapshot_path().exists());
        assert!(!activity_path.exists());
        FileExt::unlock(&lock).unwrap();
    }

    #[test]
    fn transient_cross_project_writer_outlasting_default_bound_reaches_normal_permission_decision()
    {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(activity_path.with_extension("lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_worker = Arc::clone(&calls);
        let lifecycle_for_worker = lifecycle.clone();
        let activity_for_worker = activity.clone();
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_with_gate_and_stores(
                Cursor::new(payload()),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle_for_worker,
                Some(&activity_for_worker),
                |_, _| {
                    calls_for_worker.fetch_add(1, Ordering::SeqCst);
                    Ok(suggestion(RuleAction::Approve, 0.9))
                },
            );
            result_tx.send((stdout, stderr)).unwrap();
        });

        assert!(result_rx.recv_timeout(Duration::from_millis(150)).is_err());
        FileExt::unlock(&lock).unwrap();
        let (stdout, stderr) = result_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();

        assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["hookSpecificOutput"]["decision"]
                ["behavior"],
            "allow"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let events = activity.read().unwrap().events().to_vec();
        let activity_ids = events
            .iter()
            .map(|event| event.activity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(activity_ids.len(), 1);
        let activity_id = activity_ids.into_iter().next().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.activity_id == activity_id)
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::Delivered,
            ]
        );
    }

    #[test]
    fn large_log_reader_parsing_does_not_block_permission_lifecycle() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity_path = temp.path().join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        activity
            .append_batch(&realistically_sized_permission_events())
            .unwrap();
        assert!(std::fs::metadata(&activity_path).unwrap().len() >= 20 * 1024 * 1024);

        let gate = Arc::new(ReadParseGate::default());
        let reader = activity.clone().with_read_parse_gate(Arc::clone(&gate));
        let (read_tx, read_rx) = mpsc::sync_channel(1);
        let reader_thread = std::thread::spawn(move || {
            read_tx.send(reader.read()).unwrap();
        });
        assert!(gate.wait_until_reached(Duration::from_secs(5)));

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        gate.release();
        read_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        reader_thread.join().unwrap();

        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&stderr).contains("byte budget"));
        let events = activity.read().unwrap().events().to_vec();
        let permission_activity_ids = events
            .iter()
            .filter(|event| !event.activity_id.starts_with("scale-"))
            .map(|event| event.activity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(permission_activity_ids.is_empty());
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
                None,
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
            None,
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
    fn structured_completion_failure_persists_one_actionable_error_lifecycle() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let reason = "Ollama returned an incomplete structured decision after 2 attempts \
            (Ollama completion (done=true, done_reason=stop, eval_count=746)); \
            no action was taken";
        let terminal_reason = format!("Brain query failed: {reason}");

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Err(reason.into()),
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains(reason));
        assert_eq!(
            lifecycle
                .read()
                .unwrap()
                .snapshot
                .unwrap()
                .sessions
                .values()
                .next()
                .unwrap()
                .projected_status,
            Some(ProjectedStatus::NeedsInput),
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events.iter().map(|event| event.state).collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Error,
            ]
        );
        let terminal = events.last().unwrap();
        assert_eq!(
            terminal.reasoning.as_deref(),
            Some(terminal_reason.as_str())
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.activity_id.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1,
        );
    }

    #[test]
    fn model_terminal_failure_retains_recoverable_transaction() {
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let lifecycle = LifecycleStore::at(&state_root);
        let activity_path = state_root.join("activity.jsonl");
        let saved_activity_path = state_root.join("activity-before-failure.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| {
                std::fs::rename(&activity_path, &saved_activity_path).unwrap();
                std::fs::create_dir(&activity_path).unwrap();
                Err("inference timed out".into())
            },
        );

        assert!(stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&stderr).contains("permission transaction"),
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        let transaction_dir = state_root.join("brain/permission-transactions");
        assert_eq!(std::fs::read_dir(&transaction_dir).unwrap().count(), 1);

        std::fs::remove_dir(&activity_path).unwrap();
        std::fs::rename(&saved_activity_path, &activity_path).unwrap();
        super::super::permission_transaction::recover_pending(
            &state_root,
            super::super::permission_transaction::RecoveryLimits::default(),
        )
        .unwrap();

        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(
            events.iter().map(|event| event.state).collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Error,
            ]
        );
    }

    #[test]
    fn recovered_inference_error_retains_bounded_query_diagnostic() {
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let lifecycle = LifecycleStore::at(&state_root);
        let activity_path = state_root.join("activity.jsonl");
        let saved_activity_path = state_root.join("activity-before-failure.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| {
                std::fs::rename(&activity_path, &saved_activity_path).unwrap();
                std::fs::create_dir(&activity_path).unwrap();
                Err("endpoint unavailable".into())
            },
        );

        std::fs::remove_dir(&activity_path).unwrap();
        std::fs::rename(&saved_activity_path, &activity_path).unwrap();
        super::super::permission_transaction::recover_pending(
            &state_root,
            super::super::permission_transaction::RecoveryLimits::default(),
        )
        .unwrap();

        let terminal = activity
            .read()
            .unwrap()
            .events()
            .iter()
            .find(|event| event.state == ActivityState::Error)
            .cloned()
            .unwrap();
        assert_eq!(
            terminal.reasoning.as_deref(),
            Some("Brain query failed: endpoint unavailable")
        );
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
    fn invalid_request_lock_storage_leaves_stdout_empty_before_inference() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
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
            |_, _| panic!("invalid request lock storage must block inference"),
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr).unwrap().contains("request lock"));
    }

    #[test]
    fn model_allow_journal_creation_failure_leaves_stdout_empty() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let lifecycle = LifecycleStore::at(&state_root);
        let activity_path = state_root.join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| {
                std::fs::create_dir_all(&decisions_path).unwrap();
                Ok(suggestion(RuleAction::Approve, 0.9))
            },
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&stderr).contains("permission transaction"));
    }

    #[test]
    fn antigravity_model_allow_journal_failure_returns_ask() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_provider_with_gate_and_stores(
            Cursor::new(permission_payload_for_provider(
                AgentProvider::Antigravity,
                false,
            )),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            AgentProvider::Antigravity,
            Some("PreToolUse"),
            |_, _| {
                std::fs::create_dir_all(&decisions_path).unwrap();
                Ok(suggestion(RuleAction::Approve, 0.9))
            },
        );

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
            "ask"
        );
        assert!(String::from_utf8_lossy(&stderr).contains("permission transaction"));
    }

    #[test]
    fn model_allow_destination_conflict_preserves_native_confirmation() {
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let request = parse_request(&payload()).unwrap();
        lifecycle
            .ensure_permission_disposition(
                &request.lifecycle,
                &request.request_key,
                PermissionDisposition::NeedsInput,
            )
            .unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );

        assert!(stdout.is_empty());
        assert!(String::from_utf8_lossy(&stderr).contains("permission transaction"));
        assert_eq!(
            projected_status(&lifecycle),
            Some(ProjectedStatus::NeedsInput)
        );
    }

    #[test]
    fn invalid_prior_transaction_blocks_inference_for_every_provider() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let transaction_dir = state_root.join("brain/permission-transactions");
        std::fs::create_dir_all(&transaction_dir).unwrap();
        let raw = "raw journal content must not leak";
        std::fs::write(transaction_dir.join("unexpected.json"), raw).unwrap();
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let calls = AtomicUsize::new(0);

        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            run_provider_with_gate_and_stores(
                Cursor::new(permission_payload_for_provider(provider, false)),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle,
                Some(&activity),
                provider,
                (provider == AgentProvider::Antigravity).then_some("PreToolUse"),
                |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    panic!("invalid prior transaction must block inference")
                },
            );

            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
                    "ask"
                );
            } else {
                assert!(stdout.is_empty());
            }
            let diagnostic = String::from_utf8(stderr).unwrap();
            assert!(diagnostic.contains("permission transaction preflight"));
            assert!(!diagnostic.contains(raw));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn busy_request_guard_blocks_every_provider_without_mutation_or_inference() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let (state_root, _) = permission_transaction_paths().unwrap();
        let lifecycle = LifecycleStore::at(&state_root);
        let activity_path = state_root.join("activity.jsonl");
        let activity = ActivityStore::at(&activity_path);

        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let event = (provider == AgentProvider::Antigravity).then_some("PreToolUse");
            let payload = permission_payload_for_provider(provider, false);
            let request = parse_permission(provider, event, &payload).unwrap();
            let guard = PermissionRequestLockStore::at(&state_root)
                .try_acquire(&request.lifecycle, &request.request_key)
                .unwrap()
                .unwrap();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            run_provider_with_gate_and_stores(
                Cursor::new(payload),
                &mut stdout,
                &mut stderr,
                Some(&enabled_config()),
                BrainGateMode::Auto,
                &lifecycle,
                Some(&activity),
                provider,
                event,
                |_, _| panic!("busy request guard must block inference"),
            );

            drop(guard);
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
                    "ask"
                );
            } else {
                assert!(stdout.is_empty());
            }
            assert!(String::from_utf8_lossy(&stderr).contains("already active"));
            assert!(!lifecycle.snapshot_path().exists());
            assert!(!activity_path.exists());
            assert!(!state_root.join("brain/permission-transactions").exists());
        }
    }

    #[test]
    fn provider_policy_deny_survives_journal_failure() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_provider_with_gate_and_stores_and_safety(
            Cursor::new(permission_payload_for_provider(AgentProvider::Claude, true)),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            AgentProvider::Claude,
            None,
            |_| {
                std::fs::create_dir_all(&decisions_path).unwrap();
                super::super::safety::SafetyEvaluation::NoDeterministicDecision
            },
            |_, _| panic!("provider deny must not infer"),
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(String::from_utf8_lossy(&stderr).contains("permission transaction"));
    }

    #[test]
    fn model_deny_survives_journal_failure_with_diagnostic() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_with_gate_and_stores(
            Cursor::new(payload()),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| {
                std::fs::create_dir_all(&decisions_path).unwrap();
                Ok(suggestion(RuleAction::Deny, 0.9))
            },
        );

        let output: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert!(String::from_utf8_lossy(&stderr).contains("permission transaction"));
    }

    #[test]
    fn deterministic_deny_survives_audit_failure() {
        let _environment_guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore = set_test_path_environment(home.path());
        let state_root = decisions_dir().parent().unwrap().to_path_buf();
        let decisions_path = state_root.join("brain/decisions.jsonl");
        let lifecycle = LifecycleStore::at(&state_root);
        let activity = ActivityStore::at(state_root.join("activity.jsonl"));
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        run_provider_with_gate_and_stores_and_safety(
            Cursor::new(payload_with_command("rm -rf /")),
            &mut stdout,
            &mut stderr,
            Some(&enabled_config()),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            AgentProvider::Codex,
            None,
            |_| {
                std::fs::create_dir_all(&decisions_path).unwrap();
                super::super::safety::SafetyEvaluation::Deny(SafetyDeny {
                    rule_id: "test-deny",
                    reason: "deterministic test deny".into(),
                })
            },
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
        assert!(diagnostic.starts_with("cbrain permission hook:"));
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

    fn run_concurrent_approvals(
        payloads: &[String; 2],
        lifecycle: &LifecycleStore,
        activity: &ActivityStore,
        config: &BrainConfig,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for payload in payloads {
                let ready_tx = ready_tx.clone();
                let (release_tx, release_rx) = mpsc::sync_channel(0);
                let (result_tx, result_rx) = mpsc::sync_channel(0);
                let handle = scope.spawn(move || {
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    run_with_gate_and_stores(
                        Cursor::new(payload),
                        &mut stdout,
                        &mut stderr,
                        Some(config),
                        BrainGateMode::Auto,
                        lifecycle,
                        Some(activity),
                        |_, _| {
                            ready_tx.send(()).unwrap();
                            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                            Ok(suggestion(RuleAction::Approve, 0.9))
                        },
                    );
                    result_tx.send((stdout, stderr)).unwrap();
                });
                workers.push((release_tx, result_rx, handle));
            }
            drop(ready_tx);
            for _ in payloads {
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let mut results = Vec::with_capacity(workers.len());
            for (release, result, handle) in workers {
                release.send(()).unwrap();
                results.push(result.recv_timeout(Duration::from_secs(5)).unwrap());
                handle.join().unwrap();
            }
            results
        })
    }

    #[test]
    fn concurrent_distinct_codex_permissions_deliver_and_exact_replay_fails_safe() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let config = enabled_config();
        let payloads = [
            payload_with_command("gh run view --job 89897083575 --log"),
            payload_with_command("gh run view --job 89897083607 --log"),
        ];
        let results = run_concurrent_approvals(&payloads, &lifecycle, &activity, &config);

        for (stdout, stderr) in &results {
            assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(stderr));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(stdout).unwrap()["hookSpecificOutput"]
                    ["decision"]["behavior"],
                "allow"
            );
        }
        let before_replay = lifecycle.read().unwrap().snapshot.unwrap();
        let events = activity.read().unwrap().events().to_vec();
        let allowed_ids = events
            .iter()
            .filter(|event| event.state == ActivityState::Allowed)
            .map(|event| event.activity_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(allowed_ids.len(), 2);
        let commands_by_activity_id = allowed_ids
            .iter()
            .map(|activity_id| {
                (
                    activity_id,
                    events
                        .iter()
                        .find(|event| {
                            &event.activity_id == activity_id
                                && event.state == ActivityState::Observed
                        })
                        .and_then(|event| event.normalized_command.as_deref())
                        .expect("successful activity retains its command"),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            commands_by_activity_id
                .values()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "gh run view --job 89897083575 --log",
                "gh run view --job 89897083607 --log",
            ])
        );
        for activity_id in &allowed_ids {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| &event.activity_id == activity_id)
                    .map(|event| event.state)
                    .collect::<Vec<_>>(),
                [
                    ActivityState::Observed,
                    ActivityState::Evaluating,
                    ActivityState::Allowed,
                    ActivityState::Delivered,
                ]
            );
        }
        assert!(
            !events
                .iter()
                .any(|event| event.state == ActivityState::Error)
        );
        let replay_events_before = events.clone();

        let mut replay_stdout = Vec::new();
        let mut replay_stderr = Vec::new();
        run_with_gate_and_stores(
            Cursor::new(&payloads[0]),
            &mut replay_stdout,
            &mut replay_stderr,
            Some(&config),
            BrainGateMode::Auto,
            &lifecycle,
            Some(&activity),
            |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
        );
        assert!(replay_stdout.is_empty());
        assert!(
            String::from_utf8(replay_stderr)
                .unwrap()
                .contains("duplicate")
        );
        let after_replay = lifecycle.read().unwrap().snapshot.unwrap();
        assert_eq!(after_replay.next_sequence, before_replay.next_sequence);
        assert_eq!(
            activity.read().unwrap().events(),
            replay_events_before.as_slice()
        );
    }

    #[test]
    fn parallel_codex_permission_burst_preserves_complete_initial_lifecycles() {
        let _guard = crate::config::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempfile::tempdir().unwrap();
        let _restore_home = set_test_home(home.path());
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
        let initial_lock_acquisitions = Arc::new(AtomicUsize::new(0));
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"))
            .with_lock_acquisition_counter(Arc::clone(&initial_lock_acquisitions));
        let config = enabled_config();
        let request_locks =
            PermissionRequestLockStore::at(&permission_transaction_paths().unwrap().0);
        let mut shards = std::collections::BTreeSet::new();
        let mut payloads = Vec::new();
        for index in 0..10_000 {
            let payload = payload_with_command(&format!("cargo info crate-{index}"));
            let request = parse_request(&payload).unwrap();
            if shards.insert(request_locks.shard_for(&request.lifecycle, &request.request_key)) {
                payloads.push(payload);
                if payloads.len() == 15 {
                    break;
                }
            }
        }
        assert_eq!(payloads.len(), 15);
        let start = Arc::new(Barrier::new(payloads.len()));
        let (ready_tx, ready_rx) = mpsc::channel();

        let results = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for payload in &payloads {
                let start = Arc::clone(&start);
                let ready_tx = ready_tx.clone();
                let (release_tx, release_rx) = mpsc::sync_channel(0);
                let (result_tx, result_rx) = mpsc::channel();
                let lifecycle = &lifecycle;
                let activity = &activity;
                let config = &config;
                workers.push((
                    release_tx,
                    result_rx,
                    scope.spawn(move || {
                        start.wait();
                        let mut stdout = Vec::new();
                        let mut stderr = Vec::new();
                        run_with_gate_and_stores(
                            Cursor::new(payload),
                            &mut stdout,
                            &mut stderr,
                            Some(config),
                            BrainGateMode::Auto,
                            lifecycle,
                            Some(activity),
                            |_, _| {
                                ready_tx.send(()).unwrap();
                                release_rx.recv().unwrap();
                                Ok(suggestion(RuleAction::Approve, 0.9))
                            },
                        );
                        result_tx.send((stdout, stderr)).unwrap();
                    }),
                ));
            }
            drop(ready_tx);
            for _ in &payloads {
                ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            let initial_lock_acquisitions = initial_lock_acquisitions.load(Ordering::SeqCst);
            let results = workers
                .into_iter()
                .map(|(release, result_rx, handle)| {
                    release.send(()).unwrap();
                    let result = result_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    handle.join().unwrap();
                    result
                })
                .collect::<Vec<_>>();
            (initial_lock_acquisitions, results)
        });
        let (initial_lock_acquisitions, results) = results;

        assert_eq!(initial_lock_acquisitions, payloads.len());
        for (stdout, stderr) in &results {
            assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(stderr));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(stdout).unwrap()["hookSpecificOutput"]
                    ["decision"]["behavior"],
                "allow"
            );
        }
        let events = activity.read().unwrap().events().to_vec();
        let activity_ids = events
            .iter()
            .map(|event| event.activity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(activity_ids.len(), payloads.len());
        for activity_id in activity_ids {
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.activity_id == activity_id)
                    .map(|event| event.state)
                    .collect::<Vec<_>>(),
                [
                    ActivityState::Observed,
                    ActivityState::Evaluating,
                    ActivityState::Allowed,
                    ActivityState::Delivered,
                ]
            );
        }
    }
}
