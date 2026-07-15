use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexEvent {
    SessionMeta(CodexSessionMeta),
    TurnContext(CodexTurnContext),
    TokenCount(CodexTokenCount),
    Lifecycle(CodexLifecycleEvent),
    ResponseItem(CodexResponseItem),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLifecycleEvent {
    TaskStarted,
    TaskComplete,
    TurnAborted,
    UserMessage,
    AgentMessage,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub session_id: String,
    pub cwd: String,
    pub timestamp: Option<String>,
    pub model_provider: Option<String>,
    pub cli_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexTurnContext {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub approval_policy: Option<String>,
    pub sandbox_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexTokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodexTokenCount {
    pub total: CodexTokenUsage,
    pub last: CodexTokenUsage,
    pub model_context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexResponseKind {
    Message,
    FunctionCall,
    FunctionCallOutput,
    CustomToolCall,
    CustomToolCallOutput,
    Reasoning,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexResponseItem {
    pub kind: CodexResponseKind,
    pub role: Option<String>,
    pub text: Option<String>,
    pub name: Option<String>,
    pub arguments: Option<String>,
    pub call_id: Option<String>,
    pub output: Option<String>,
}

pub fn parse_line(line: &str) -> Option<CodexEvent> {
    let entry: Value = serde_json::from_str(line).ok()?;
    match entry.get("type").and_then(|v| v.as_str())? {
        "session_meta" => parse_session_meta(entry.get("payload")?).map(CodexEvent::SessionMeta),
        "turn_context" => Some(CodexEvent::TurnContext(parse_turn_context(
            entry.get("payload")?,
        ))),
        "event_msg" => parse_event_msg(entry.get("payload")?),
        "response_item" => parse_response_item(entry.get("payload")?).map(CodexEvent::ResponseItem),
        _ => None,
    }
}

fn parse_session_meta(payload: &Value) -> Option<CodexSessionMeta> {
    Some(CodexSessionMeta {
        session_id: payload.get("id")?.as_str()?.to_string(),
        cwd: payload.get("cwd")?.as_str()?.to_string(),
        timestamp: payload
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        model_provider: payload
            .get("model_provider")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        cli_version: payload
            .get("cli_version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

fn parse_turn_context(payload: &Value) -> CodexTurnContext {
    CodexTurnContext {
        cwd: payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        model: payload
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        approval_policy: payload
            .get("approval_policy")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        sandbox_policy: payload
            .get("sandbox_policy")
            .and_then(|v| v.get("type").or(Some(v)))
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

fn parse_event_msg(payload: &Value) -> Option<CodexEvent> {
    let event_type = payload.get("type").and_then(|v| v.as_str())?;
    if event_type == "token_count" {
        return parse_token_count(payload).map(CodexEvent::TokenCount);
    }

    let event = match event_type {
        "task_started" => CodexLifecycleEvent::TaskStarted,
        "task_complete" => CodexLifecycleEvent::TaskComplete,
        "turn_aborted" => CodexLifecycleEvent::TurnAborted,
        "user_message" => CodexLifecycleEvent::UserMessage,
        "agent_message" => CodexLifecycleEvent::AgentMessage,
        other => CodexLifecycleEvent::Other(other.to_string()),
    };
    Some(CodexEvent::Lifecycle(event))
}

fn parse_token_count(payload: &Value) -> Option<CodexTokenCount> {
    let info = payload.get("info")?;
    Some(CodexTokenCount {
        total: parse_token_usage(info.get("total_token_usage")?),
        last: info
            .get("last_token_usage")
            .map(parse_token_usage)
            .unwrap_or_default(),
        model_context_window: info.get("model_context_window").and_then(|v| v.as_u64()),
    })
}

fn parse_token_usage(value: &Value) -> CodexTokenUsage {
    CodexTokenUsage {
        input_tokens: value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cached_input_tokens: value
            .get("cached_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        output_tokens: value
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        reasoning_output_tokens: value
            .get("reasoning_output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        total_tokens: value
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

fn parse_response_item(payload: &Value) -> Option<CodexResponseItem> {
    let item_type = payload.get("type").and_then(|v| v.as_str())?;
    let kind = match item_type {
        "message" => CodexResponseKind::Message,
        "function_call" => CodexResponseKind::FunctionCall,
        "function_call_output" => CodexResponseKind::FunctionCallOutput,
        "custom_tool_call" => CodexResponseKind::CustomToolCall,
        "custom_tool_call_output" => CodexResponseKind::CustomToolCallOutput,
        "reasoning" => CodexResponseKind::Reasoning,
        _ => CodexResponseKind::Other,
    };

    Some(CodexResponseItem {
        kind,
        role: payload
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        text: extract_text(payload),
        name: payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        arguments: payload
            .get("arguments")
            .or_else(|| payload.get("input"))
            .and_then(value_as_string),
        call_id: payload
            .get("call_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        output: payload
            .get("output")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

fn value_as_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| serde_json::to_string(value).ok())
}

fn extract_text(payload: &Value) -> Option<String> {
    if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
        return Some(text.to_string());
    }

    let content = payload.get("content")?.as_array()?;
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|block| {
            block
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| block.as_str())
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_meta() {
        let line = include_str!("../../../tests/fixtures/codex-session-meta.json");
        let Some(CodexEvent::SessionMeta(meta)) = parse_line(line.trim()) else {
            panic!("expected session meta");
        };
        assert_eq!(meta.session_id, "019eb6ac-6d30-7301-885d-ff4d354c0116");
        assert_eq!(meta.cwd, "/home/alexander/hacking/aleadag/codexctl");
        assert_eq!(meta.model_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn parses_function_call() {
        let line = include_str!("../../../tests/fixtures/codex-tool-call.json");
        let Some(CodexEvent::ResponseItem(item)) = parse_line(line.trim()) else {
            panic!("expected response item");
        };
        assert_eq!(item.kind, CodexResponseKind::FunctionCall);
        assert_eq!(item.name.as_deref(), Some("exec_command"));
        assert!(item.arguments.as_deref().unwrap().contains("cargo test"));
    }

    #[test]
    fn parses_token_count() {
        let line = r#"{"timestamp":"2026-06-11T12:34:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100000,"cached_input_tokens":25000,"output_tokens":12000,"reasoning_output_tokens":3000,"total_tokens":112000},"last_token_usage":{"input_tokens":42000,"cached_input_tokens":21000,"output_tokens":12000,"reasoning_output_tokens":3000,"total_tokens":54000},"model_context_window":258400}}}"#;
        let Some(CodexEvent::TokenCount(count)) = parse_line(line) else {
            panic!("expected token count");
        };
        assert_eq!(count.total.input_tokens, 100000);
        assert_eq!(count.total.cached_input_tokens, 25000);
        assert_eq!(count.total.output_tokens, 12000);
        assert_eq!(count.last.input_tokens, 42000);
        assert_eq!(count.model_context_window, Some(258400));
    }

    #[test]
    fn parses_task_lifecycle_events() {
        let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;

        assert_eq!(
            parse_line(started),
            Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted))
        );
        assert_eq!(
            parse_line(complete),
            Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskComplete))
        );
    }

    #[test]
    fn parses_custom_tool_call_and_output() {
        let call = r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"shell","input":"cargo test","call_id":"call-7"}}"#;
        let output = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-7","output":"ok"}}"#;

        let Some(CodexEvent::ResponseItem(call)) = parse_line(call) else {
            panic!("custom call");
        };
        let Some(CodexEvent::ResponseItem(output)) = parse_line(output) else {
            panic!("custom output");
        };
        assert_eq!(call.kind, CodexResponseKind::CustomToolCall);
        assert_eq!(call.arguments.as_deref(), Some("cargo test"));
        assert_eq!(output.kind, CodexResponseKind::CustomToolCallOutput);
    }
}
