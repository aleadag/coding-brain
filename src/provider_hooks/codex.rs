use std::path::PathBuf;

use coding_brain_core::lifecycle::LifecycleEvent;
use coding_brain_core::provider::AgentProvider;
use serde::Deserialize;
use serde_json::Value;

use super::{
    HookInputError, ParsedLifecycleHook, PermissionHookRequest, ProviderPermissionPolicy, identity,
    normalized_outcome, optional_command, optional_id, permission_request,
};

#[derive(Debug, Deserialize)]
struct CodexActivityInput {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    tool_response: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct CodexPermissionInput {
    session_id: String,
    turn_id: Option<String>,
    tool_use_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: String,
    hook_event_name: String,
    tool_name: String,
    tool_input: Value,
}

pub(crate) fn parse_permission(raw: &[u8]) -> Result<PermissionHookRequest, HookInputError> {
    let input: CodexPermissionInput =
        serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    if input.hook_event_name != "PermissionRequest" {
        return Err(HookInputError::UnsupportedEvent);
    }
    let tool_name = optional_id(Some(input.tool_name), "tool_name")?
        .ok_or(HookInputError::Missing("tool_name"))?;
    let tool_use_id = optional_id(input.tool_use_id, "tool_use_id")?;
    let command = (tool_name == "Bash")
        .then(|| {
            optional_command(
                input
                    .tool_input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "tool_input.command",
            )?
            .ok_or(HookInputError::Missing("tool_input.command"))
        })
        .transpose()?;
    let lifecycle = identity(
        AgentProvider::Codex,
        input.session_id,
        input.turn_id,
        input.transcript_path,
        PathBuf::from(input.cwd),
    )?;
    if lifecycle.turn_id().is_none() {
        return Err(HookInputError::Missing("turn_id"));
    }
    Ok(permission_request(
        lifecycle,
        tool_name,
        command,
        tool_use_id,
        ProviderPermissionPolicy::PermitsBrainDecision,
    ))
}

pub(crate) fn parse_lifecycle(raw: &[u8]) -> Result<ParsedLifecycleHook, HookInputError> {
    let lifecycle = LifecycleEvent::parse(raw).map_err(HookInputError::from)?;
    let activity: CodexActivityInput =
        serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    let outcome = matches!(
        lifecycle.kind(),
        coding_brain_core::lifecycle::LifecycleEventKind::PostToolUse
    )
    .then(|| normalized_outcome(activity.tool_response.as_ref()));
    Ok(ParsedLifecycleHook {
        identity: lifecycle.identity().clone(),
        event: lifecycle.kind().clone(),
        turn_initial_step: lifecycle.turn_initial_step(),
        tool_use_id: activity.tool_use_id,
        tool_name: activity.tool_name,
        outcome,
        live_process: None,
    })
}
