use serde_json::Value;

use crate::models;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    Assistant,
    User,
}

#[derive(Debug, Clone)]
pub enum TranscriptBlock {
    Text(String),
    ToolUse { name: String, input: Value },
    ToolResult { content: String, is_error: bool },
}

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub role: TranscriptRole,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub context_pressure: Option<u8>,
    pub content: Vec<TranscriptBlock>,
}

#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    WaitingForTask,
    Message(TranscriptMessage),
}

pub fn parse_line(line: &str) -> Option<TranscriptEvent> {
    let entry: Value = serde_json::from_str(line).ok()?;

    if is_waiting_for_task(&entry) {
        return Some(TranscriptEvent::WaitingForTask);
    }

    let msg = entry.get("message")?;
    let role = message_role(&entry, msg)?;

    let content = msg
        .get("content")
        .and_then(|v| v.as_array())
        .map(|blocks| blocks.iter().filter_map(parse_block).collect())
        .unwrap_or_default();

    let model = msg
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let raw_usage = msg.get("usage");
    let context_pressure =
        raw_usage.and_then(|usage| derive_context_pressure(model.as_deref(), usage));

    Some(TranscriptEvent::Message(TranscriptMessage {
        role,
        model,
        stop_reason: msg
            .get("stop_reason")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        context_pressure,
        content,
    }))
}

fn is_waiting_for_task(entry: &Value) -> bool {
    if entry.get("type").and_then(|v| v.as_str()) != Some("progress") {
        return false;
    }

    match entry.get("data") {
        Some(Value::String(s)) => s.contains("waiting_for_task"),
        Some(Value::Object(map)) => map.values().any(|v| {
            v.as_str()
                .map(|s| s.contains("waiting_for_task"))
                .unwrap_or(false)
        }),
        _ => false,
    }
}

fn message_role(entry: &Value, msg: &Value) -> Option<TranscriptRole> {
    let role = msg
        .get("role")
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("type").and_then(|v| v.as_str()))?;

    match role {
        "assistant" => Some(TranscriptRole::Assistant),
        "user" => Some(TranscriptRole::User),
        _ => None,
    }
}

fn derive_context_pressure(model: Option<&str>, usage: &Value) -> Option<u8> {
    let capacity = model.and_then(models::context_window)?;
    let input_tokens = usage.get("input_tokens")?.as_u64()?;
    let cache_read_input_tokens = optional_usage_counter(usage, "cache_read_input_tokens")?;
    let cache_creation_input_tokens = optional_usage_counter(usage, "cache_creation_input_tokens")?;
    let used = input_tokens
        .saturating_add(cache_read_input_tokens)
        .saturating_add(cache_creation_input_tokens);
    crate::context_pressure::percent(used, capacity)
}

fn optional_usage_counter(usage: &Value, field: &str) -> Option<u64> {
    match usage.get(field) {
        Some(value) => value.as_u64(),
        None => Some(0),
    }
}

fn parse_block(block: &Value) -> Option<TranscriptBlock> {
    match block.get("type").and_then(|v| v.as_str())? {
        "text" => block
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| TranscriptBlock::Text(s.to_string())),
        "tool_use" => Some(TranscriptBlock::ToolUse {
            name: block
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
            input: block.get("input").cloned().unwrap_or(Value::Null),
        }),
        "tool_result" => Some(TranscriptBlock::ToolResult {
            content: block
                .get("content")
                .and_then(extract_text_content)
                .unwrap_or_default(),
            is_error: block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        _ => None,
    }
}

fn extract_text_content(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    let blocks = value.as_array()?;
    let mut parts = Vec::new();
    for block in blocks {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            parts.push(text);
        }
    }
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
    fn parse_real_fixture_line() {
        let line = include_str!("../../../tests/fixtures/real-transcript-line.json");
        let Some(TranscriptEvent::Message(msg)) = parse_line(line.trim()) else {
            panic!("expected message event");
        };
        assert_eq!(msg.role, TranscriptRole::Assistant);
        assert_eq!(msg.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(msg.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(msg.content.len(), 2);
    }

    #[test]
    fn parse_legacy_fixture_line() {
        // Fixture lives at the workspace root, not in this crate.
        let line = include_str!("../../../tests/fixtures/legacy-transcript-line.json");
        let Some(TranscriptEvent::Message(msg)) = parse_line(line.trim()) else {
            panic!("expected message event");
        };
        assert_eq!(msg.role, TranscriptRole::Assistant);
        assert_eq!(msg.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn message_context_pressure_uses_known_model_capacity() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":100000,"cache_read_input_tokens":20000,"cache_creation_input_tokens":10000,"output_tokens":1},"content":[]}}"#;
        let Some(TranscriptEvent::Message(message)) = parse_line(line) else {
            panic!("expected message");
        };
        assert_eq!(message.context_pressure, Some(50));

        let unknown = line.replace("gpt-5.5", "custom-model");
        let Some(TranscriptEvent::Message(message)) = parse_line(&unknown) else {
            panic!("expected message");
        };
        assert_eq!(message.context_pressure, None);

        let without_caches = r#"{"type":"assistant","message":{"role":"assistant","model":"gpt-5.5","usage":{"input_tokens":129200},"content":[]}}"#;
        let Some(TranscriptEvent::Message(message)) = parse_line(without_caches) else {
            panic!("expected message");
        };
        assert_eq!(message.context_pressure, Some(50));
    }

    #[test]
    fn parse_waiting_for_task_progress() {
        let line = r#"{"type":"progress","data":"waiting_for_task"}"#;
        assert!(matches!(
            parse_line(line),
            Some(TranscriptEvent::WaitingForTask)
        ));
    }
}
