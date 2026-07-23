use std::path::PathBuf;

use coding_brain_core::lifecycle::LifecycleEventKind;
use coding_brain_core::provider::AgentProvider;
use serde::Deserialize;
use serde_json::Value;

use super::{
    HookInputError, ParsedLifecycleHook, PermissionHookRequest, ProviderPermissionPolicy,
    event_kind, identity, normalized_outcome, optional_command, optional_id, permission_request,
    require_tool_use_id, required,
};

#[derive(Debug, Deserialize)]
struct ClaudeLifecycleInput {
    session_id: Option<String>,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: Option<String>,
    hook_event_name: Option<String>,
    source: Option<String>,
    agent_id: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_response: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ClaudePermissionInput {
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_use_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: Option<String>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    #[serde(default)]
    permission_suggestions: Vec<ClaudePermissionSuggestion>,
}

#[derive(Debug, Deserialize)]
struct ClaudePermissionSuggestion {
    behavior: Option<String>,
}

pub(crate) fn parse_permission(raw: &[u8]) -> Result<PermissionHookRequest, HookInputError> {
    let input: ClaudePermissionInput =
        serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    if required(input.hook_event_name, "hook_event_name")? != "PermissionRequest" {
        return Err(HookInputError::UnsupportedEvent);
    }
    let session_id = required(input.session_id, "session_id")?;
    let tool_name =
        optional_id(input.tool_name, "tool_name")?.ok_or(HookInputError::Missing("tool_name"))?;
    let tool_use_id = optional_id(input.tool_use_id, "tool_use_id")?;
    let turn_id = input
        .turn_id
        .or_else(|| tool_use_id.clone())
        .or_else(|| Some(session_id.clone()));
    let lifecycle = identity(
        AgentProvider::Claude,
        session_id,
        turn_id,
        input.transcript_path,
        PathBuf::from(required(input.cwd, "cwd")?),
    )?;
    let tool_input = input
        .tool_input
        .ok_or(HookInputError::Missing("tool_input"))?;
    if !tool_input.is_object() {
        return Err(HookInputError::Invalid("tool_input"));
    }
    let command = (tool_name == "Bash")
        .then(|| {
            optional_command(
                tool_input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "tool_input.command",
            )?
            .ok_or(HookInputError::Missing("tool_input.command"))
        })
        .transpose()?;
    let provider_policy = input.permission_suggestions.iter().fold(
        ProviderPermissionPolicy::PermitsBrainDecision,
        |policy, suggestion| match suggestion.behavior.as_deref() {
            Some("deny") => ProviderPermissionPolicy::Denies,
            Some("ask") | None if policy != ProviderPermissionPolicy::Denies => {
                ProviderPermissionPolicy::RequiresAsk
            }
            Some("allow") => policy,
            Some(_) if policy != ProviderPermissionPolicy::Denies => {
                ProviderPermissionPolicy::RequiresAsk
            }
            _ => policy,
        },
    );
    Ok(permission_request(
        lifecycle,
        tool_name,
        command,
        tool_use_id,
        provider_policy,
    ))
}

pub(crate) fn parse_lifecycle(raw: &[u8]) -> Result<ParsedLifecycleHook, HookInputError> {
    let input: ClaudeLifecycleInput =
        serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    let session_id = required(input.session_id, "session_id")?;
    let event_name = required(input.hook_event_name, "hook_event_name")?;
    let event = event_kind(&event_name, input.source.as_deref(), input.agent_id)?;
    let tool_use_id = require_tool_use_id(&event, input.tool_use_id, "tool_use_id")?;
    let tool_name = optional_id(input.tool_name, "tool_name")?;
    let turn_id = if matches!(event, LifecycleEventKind::SessionStart { .. }) {
        input.turn_id
    } else {
        input
            .turn_id
            .or_else(|| tool_use_id.clone())
            .or_else(|| Some(session_id.clone()))
    };
    let identity = identity(
        AgentProvider::Claude,
        session_id,
        turn_id,
        input.transcript_path,
        PathBuf::from(required(input.cwd, "cwd")?),
    )?;
    let outcome = matches!(event, LifecycleEventKind::PostToolUse)
        .then(|| normalized_outcome(input.tool_response.as_ref()));
    Ok(ParsedLifecycleHook {
        identity,
        event,
        tool_use_id,
        tool_name,
        outcome,
        live_process: None,
    })
}
