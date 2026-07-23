use std::path::PathBuf;

use coding_brain_core::brain_activity::ActivityOutcome;
use coding_brain_core::lifecycle::{LifecycleEventKind, MAX_PATH_BYTES};
use coding_brain_core::provider::AgentProvider;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use super::{HookInputError, ParsedLifecycleHook, identity, optional_id, required};
use super::{
    PermissionHookRequest, ProviderPermissionPolicy, optional_command, permission_request,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityLifecycleInput {
    conversation_id: Option<String>,
    workspace_paths: Option<Vec<PathBuf>>,
    transcript_path: Option<PathBuf>,
    artifact_directory_path: Option<PathBuf>,
    invocation_num: Option<u64>,
    initial_num_steps: Option<u64>,
    execution_num: Option<u64>,
    termination_reason: Option<String>,
    fully_idle: Option<bool>,
    step_idx: Option<u64>,
    tool_call: Option<AntigravityToolCall>,
    #[serde(default, deserialize_with = "deserialize_optional_error")]
    error: AntigravityError,
}

#[derive(Debug, Deserialize)]
struct AntigravityToolCall {
    name: Option<String>,
    args: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityPermissionInput {
    conversation_id: Option<String>,
    workspace_paths: Option<Vec<PathBuf>>,
    transcript_path: Option<PathBuf>,
    artifact_directory_path: Option<PathBuf>,
    step_idx: Option<u64>,
    tool_call: Option<AntigravityPermissionToolCall>,
    decision: Option<Value>,
    permission_overrides: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AntigravityPermissionToolCall {
    name: Option<String>,
    args: Option<Value>,
}

pub(crate) fn parse_permission(
    trusted_event: Option<&str>,
    raw: &[u8],
) -> Result<PermissionHookRequest, HookInputError> {
    if TrustedAntigravityEvent::parse(trusted_event)? != TrustedAntigravityEvent::PreToolUse {
        return Err(HookInputError::UnsupportedEvent);
    }
    let raw_input: Value = serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    let decision_present = raw_input.get("decision").is_some();
    let overrides_present = raw_input.get("permissionOverrides").is_some();
    let input: AntigravityPermissionInput =
        serde_json::from_value(raw_input).map_err(|_| HookInputError::InvalidJson)?;
    let session_id = required(input.conversation_id, "conversationId")?;
    let transcript_path = required_path(input.transcript_path, "transcriptPath")?;
    let _artifact_directory_path =
        required_path(input.artifact_directory_path, "artifactDirectoryPath")?;
    let cwd = required_workspace(input.workspace_paths)?;
    let step = input.step_idx.ok_or(HookInputError::Missing("stepIdx"))?;
    let tool_call = input.tool_call.ok_or(HookInputError::Missing("toolCall"))?;
    let tool_name = optional_id(tool_call.name, "toolCall.name")?
        .ok_or(HookInputError::Missing("toolCall.name"))?;
    let args = tool_call
        .args
        .ok_or(HookInputError::Missing("toolCall.args"))?;
    if !args.is_object() {
        return Err(HookInputError::Invalid("toolCall.args"));
    }
    let command = (tool_name == "run_command")
        .then(|| {
            optional_command(
                args.get("CommandLine")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                "toolCall.args.CommandLine",
            )?
            .ok_or(HookInputError::Missing("toolCall.args.CommandLine"))
        })
        .transpose()?;
    let overrides_require_ask = overrides_present
        && !input
            .permission_overrides
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|overrides| overrides.is_empty());
    let provider_policy = match input.decision.as_ref().and_then(Value::as_str) {
        Some("deny") => ProviderPermissionPolicy::Denies,
        Some("ask" | "force_ask") => ProviderPermissionPolicy::RequiresAsk,
        Some("allow") | None if !decision_present => {
            if overrides_require_ask {
                ProviderPermissionPolicy::RequiresAsk
            } else {
                ProviderPermissionPolicy::PermitsBrainDecision
            }
        }
        Some("allow") if overrides_require_ask => ProviderPermissionPolicy::RequiresAsk,
        Some("allow") => ProviderPermissionPolicy::PermitsBrainDecision,
        Some(_) | None => ProviderPermissionPolicy::RequiresAsk,
    };
    let turn_id = format!("step-{step}");
    let lifecycle = identity(
        AgentProvider::Antigravity,
        session_id,
        Some(turn_id.clone()),
        Some(transcript_path),
        cwd,
    )?;
    Ok(permission_request(
        lifecycle,
        tool_name,
        command,
        Some(turn_id),
        provider_policy,
    ))
}

#[derive(Debug, Default)]
enum AntigravityError {
    #[default]
    Absent,
    Message(String),
}

impl AntigravityError {
    fn outcome(&self) -> ActivityOutcome {
        match self {
            Self::Message(message) if !message.is_empty() => ActivityOutcome::Failed,
            Self::Absent | Self::Message(_) => ActivityOutcome::Succeeded,
        }
    }
}

fn deserialize_optional_error<'de, D>(deserializer: D) -> Result<AntigravityError, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(AntigravityError::Message)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrustedAntigravityEvent {
    Stop,
    PreToolUse,
    PostToolUse,
    Invocation,
}

impl TrustedAntigravityEvent {
    fn parse(value: Option<&str>) -> Result<Self, HookInputError> {
        match value {
            Some("Stop") => Ok(Self::Stop),
            Some("PreToolUse") => Ok(Self::PreToolUse),
            Some("PostToolUse") => Ok(Self::PostToolUse),
            Some("PreInvocation" | "PostInvocation") => Ok(Self::Invocation),
            Some(_) => Err(HookInputError::Invalid("antigravity hook event")),
            None => Err(HookInputError::Missing("antigravity hook event")),
        }
    }
}

pub(crate) fn parse_lifecycle(
    trusted_event: Option<&str>,
    raw: &[u8],
) -> Result<ParsedLifecycleHook, HookInputError> {
    let trusted_event = TrustedAntigravityEvent::parse(trusted_event)?;
    let input: AntigravityLifecycleInput =
        serde_json::from_slice(raw).map_err(|_| HookInputError::InvalidJson)?;
    let session_id = required(input.conversation_id, "conversationId")?;
    let transcript_path = required_path(input.transcript_path, "transcriptPath")?;
    let _artifact_directory_path =
        required_path(input.artifact_directory_path, "artifactDirectoryPath")?;
    let cwd = required_workspace(input.workspace_paths)?;

    let (event, turn_id, tool_use_id, tool_name, outcome) = match trusted_event {
        TrustedAntigravityEvent::Stop => {
            let execution = input
                .execution_num
                .ok_or(HookInputError::Missing("executionNum"))?;
            optional_id(input.termination_reason, "terminationReason")?
                .ok_or(HookInputError::Missing("terminationReason"))?;
            match input.fully_idle {
                Some(true) => {}
                Some(false) => return Err(HookInputError::Invalid("fullyIdle")),
                None => return Err(HookInputError::Missing("fullyIdle")),
            }
            (
                LifecycleEventKind::Stop,
                format!("execution-{execution}"),
                None,
                None,
                None,
            )
        }
        TrustedAntigravityEvent::Invocation => {
            let invocation = input
                .invocation_num
                .ok_or(HookInputError::Missing("invocationNum"))?;
            input
                .initial_num_steps
                .ok_or(HookInputError::Missing("initialNumSteps"))?;
            (
                LifecycleEventKind::UserPromptSubmit,
                format!("invocation-{invocation}"),
                None,
                None,
                None,
            )
        }
        TrustedAntigravityEvent::PreToolUse => {
            let step = input.step_idx.ok_or(HookInputError::Missing("stepIdx"))?;
            let tool_call = input.tool_call.ok_or(HookInputError::Missing("toolCall"))?;
            let tool_name = optional_id(tool_call.name, "toolCall.name")?
                .ok_or(HookInputError::Missing("toolCall.name"))?;
            if !tool_call.args.is_some_and(|args| args.is_object()) {
                return Err(HookInputError::Invalid("toolCall.args"));
            }
            (
                LifecycleEventKind::PreToolUse,
                format!("step-{step}"),
                Some(format!("step-{step}")),
                Some(tool_name),
                None,
            )
        }
        TrustedAntigravityEvent::PostToolUse => {
            let step = input.step_idx.ok_or(HookInputError::Missing("stepIdx"))?;
            (
                LifecycleEventKind::PostToolUse,
                format!("step-{step}"),
                Some(format!("step-{step}")),
                None,
                Some(input.error.outcome()),
            )
        }
    };
    let identity = identity(
        AgentProvider::Antigravity,
        session_id,
        Some(turn_id),
        Some(transcript_path),
        cwd,
    )?;
    Ok(ParsedLifecycleHook {
        identity,
        event,
        tool_use_id,
        tool_name,
        outcome,
        live_process: None,
    })
}

fn required_workspace(paths: Option<Vec<PathBuf>>) -> Result<PathBuf, HookInputError> {
    let paths = paths.ok_or(HookInputError::Missing("workspacePaths"))?;
    if paths.is_empty() {
        return Err(HookInputError::Empty("workspacePaths"));
    }
    let mut paths = paths
        .into_iter()
        .map(|path| validate_path(path, "workspacePaths"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(paths.remove(0))
}

fn required_path(path: Option<PathBuf>, field: &'static str) -> Result<PathBuf, HookInputError> {
    validate_path(path.ok_or(HookInputError::Missing(field))?, field)
}

fn validate_path(path: PathBuf, field: &'static str) -> Result<PathBuf, HookInputError> {
    if path.as_os_str().is_empty() {
        Err(HookInputError::Empty(field))
    } else if path.to_string_lossy().len() > MAX_PATH_BYTES {
        Err(HookInputError::TooLong(field))
    } else if !path.is_absolute() {
        Err(HookInputError::Invalid(field))
    } else {
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRE_TOOL_USE: &[u8] =
        include_bytes!("../../tests/fixtures/hooks/antigravity-pre-tool-use.json");
    const POST_TOOL_USE: &[u8] =
        include_bytes!("../../tests/fixtures/hooks/antigravity-post-tool-use.json");

    #[test]
    fn pre_tool_use_ignores_undocumented_authority_fields() {
        let mut payload: Value = serde_json::from_slice(PRE_TOOL_USE).unwrap();
        payload["hookEventName"] = serde_json::json!("Stop");
        payload["toolUseId"] = serde_json::json!("payload-controlled-id");
        payload["toolName"] = serde_json::json!("payload-controlled-tool");

        let parsed =
            parse_lifecycle(Some("PreToolUse"), &serde_json::to_vec(&payload).unwrap()).unwrap();

        assert_eq!(parsed.event, LifecycleEventKind::PreToolUse);
        assert_eq!(parsed.tool_use_id.as_deref(), Some("step-5"));
        assert_eq!(parsed.tool_name.as_deref(), Some("run_command"));
    }

    #[test]
    fn post_tool_use_ignores_undocumented_authority_fields() {
        let mut payload: Value = serde_json::from_slice(POST_TOOL_USE).unwrap();
        payload["hookEventName"] = serde_json::json!("Stop");
        payload["toolUseId"] = serde_json::json!("payload-controlled-id");
        payload["toolName"] = serde_json::json!("payload-controlled-tool");

        let parsed =
            parse_lifecycle(Some("PostToolUse"), &serde_json::to_vec(&payload).unwrap()).unwrap();

        assert_eq!(parsed.event, LifecycleEventKind::PostToolUse);
        assert_eq!(parsed.tool_use_id.as_deref(), Some("step-5"));
        assert_eq!(parsed.tool_name, None);
    }
}
