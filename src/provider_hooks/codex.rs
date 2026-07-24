use std::path::PathBuf;

use coding_brain_core::lifecycle::{LifecycleEvent, LifecycleEventKind};
use coding_brain_core::provider::AgentProvider;
use serde::Deserialize;
use serde_json::Value;

use super::{
    HookInputError, ParsedLifecycleHook, PermissionHookRequest, ProviderPermissionPolicy, identity,
    linked_identity, normalized_outcome, optional_command, optional_id, permission_request,
};

#[derive(Debug, Deserialize)]
struct CodexActivityInput {
    #[serde(default)]
    agent_id: Option<String>,
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
    #[serde(default)]
    agent_id: Option<String>,
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
    let lifecycle = match optional_id(input.agent_id, "agent_id")? {
        Some(agent_id) => linked_identity(
            AgentProvider::Codex,
            agent_id,
            input.session_id,
            input.turn_id,
            input.transcript_path,
            PathBuf::from(input.cwd),
        )?,
        None => identity(
            AgentProvider::Codex,
            input.session_id,
            input.turn_id,
            input.transcript_path,
            PathBuf::from(input.cwd),
        )?,
    };
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
    let agent_id = optional_id(activity.agent_id, "agent_id")?;
    let topology_event = matches!(
        lifecycle.kind(),
        LifecycleEventKind::SubagentStart { .. } | LifecycleEventKind::SubagentStop { .. }
    );
    if topology_event && agent_id.as_deref() == Some(lifecycle.identity().session_id()) {
        return Err(HookInputError::Invalid("agent_id"));
    }
    let identity = if topology_event {
        lifecycle.identity().clone()
    } else if let Some(agent_id) = agent_id {
        linked_identity(
            AgentProvider::Codex,
            agent_id,
            lifecycle.identity().session_id().to_owned(),
            lifecycle.identity().turn_id().map(str::to_owned),
            lifecycle.identity().transcript_path().map(PathBuf::from),
            lifecycle.identity().cwd().to_path_buf(),
        )?
    } else {
        lifecycle.identity().clone()
    };
    let outcome = matches!(
        lifecycle.kind(),
        coding_brain_core::lifecycle::LifecycleEventKind::PostToolUse
    )
    .then(|| normalized_outcome(activity.tool_response.as_ref()));
    Ok(ParsedLifecycleHook {
        identity,
        event: lifecycle.kind().clone(),
        turn_initial_step: lifecycle.turn_initial_step(),
        tool_use_id: activity.tool_use_id,
        tool_name: activity.tool_name,
        outcome,
        live_process: None,
    })
}

#[cfg(test)]
mod tests {
    use coding_brain_core::lifecycle::MAX_ID_BYTES;
    use serde_json::Value;

    use super::*;

    const CHILD_PERMISSION_REQUEST: &[u8] =
        include_bytes!("../../tests/fixtures/hooks/codex-child-permission-request.json");
    const CHILD_PRE_TOOL_USE: &[u8] =
        include_bytes!("../../tests/fixtures/hooks/codex-child-pre-tool-use.json");
    const CHILD_POST_TOOL_USE: &[u8] =
        include_bytes!("../../tests/fixtures/hooks/codex-child-post-tool-use.json");

    #[test]
    fn child_permission_uses_agent_id_and_preserves_provider_session() {
        let request = parse_permission(CHILD_PERMISSION_REQUEST).unwrap();

        assert_eq!(request.lifecycle.session_id(), "child-1");
        assert_eq!(request.lifecycle.provider_session_id(), Some("root-1"));
        assert_eq!(request.tool_use_id, None);
    }

    #[test]
    fn child_pre_and_post_tool_use_preserve_linked_identity() {
        for payload in [CHILD_PRE_TOOL_USE, CHILD_POST_TOOL_USE] {
            let parsed = parse_lifecycle(payload).unwrap();

            assert_eq!(parsed.identity.session_id(), "child-1");
            assert_eq!(parsed.identity.provider_session_id(), Some("root-1"));
            assert_eq!(parsed.tool_use_id.as_deref(), Some("call-child-1"));
        }
    }

    #[test]
    fn subagent_topology_events_remain_provider_session_scoped() {
        for payload in [
            include_bytes!("../../tests/fixtures/hooks/subagent-start.json").as_slice(),
            include_bytes!("../../tests/fixtures/hooks/subagent-stop.json").as_slice(),
        ] {
            let parsed = parse_lifecycle(payload).unwrap();

            assert_eq!(parsed.identity.session_id(), "session-1");
            assert_eq!(parsed.identity.provider_session_id(), None);
        }
    }

    #[test]
    fn subagent_topology_events_reject_self_linked_agent_ids() {
        for raw in [
            include_bytes!("../../tests/fixtures/hooks/subagent-start.json").as_slice(),
            include_bytes!("../../tests/fixtures/hooks/subagent-stop.json").as_slice(),
        ] {
            let mut payload: Value = serde_json::from_slice(raw).unwrap();
            payload["agent_id"] = payload["session_id"].clone();

            assert_eq!(
                parse_lifecycle(&serde_json::to_vec(&payload).unwrap()).unwrap_err(),
                HookInputError::Invalid("agent_id")
            );
        }
    }

    #[test]
    fn root_callbacks_remain_unlinked() {
        let parsed = parse_lifecycle(include_bytes!(
            "../../tests/fixtures/hooks/pre-tool-use.json"
        ))
        .unwrap();

        assert_eq!(parsed.identity.session_id(), "session-1");
        assert_eq!(parsed.identity.provider_session_id(), None);
    }

    #[test]
    fn child_agent_id_must_be_bounded_and_not_self_linked() {
        for agent_id in [
            "".to_owned(),
            "x".repeat(MAX_ID_BYTES + 1),
            "root-1".to_owned(),
        ] {
            for raw in [
                CHILD_PERMISSION_REQUEST,
                CHILD_PRE_TOOL_USE,
                CHILD_POST_TOOL_USE,
            ] {
                let mut payload: Value = serde_json::from_slice(raw).unwrap();
                payload["agent_id"] = agent_id.clone().into();
                let payload = serde_json::to_vec(&payload).unwrap();

                if raw == CHILD_PERMISSION_REQUEST {
                    assert!(parse_permission(&payload).is_err());
                } else {
                    assert!(parse_lifecycle(&payload).is_err());
                }
            }
        }
    }
}
