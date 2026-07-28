#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub const MAX_CODEX_RESUME_HEAD_BYTES: u64 = 1024 * 1024;
pub const MAX_CODEX_RESUME_TAIL_BYTES: u64 = 8 * 1024 * 1024;

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
    TaskStarted { turn_id: Option<String> },
    TaskComplete,
    TurnAborted,
    UserMessage,
    AgentMessage,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub session_id: String,
    pub provider_session_id: Option<String>,
    pub parent_thread_id: Option<String>,
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
pub struct CodexTokenCount {
    pub context_pressure: Option<u8>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedCodexEvent {
    pub event: CodexEvent,
    pub timestamp_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexResumeEvidence {
    pub child_session_id: String,
    pub provider_session_id: String,
    pub parent_thread_id: Option<String>,
    pub turn_id: String,
    pub started_at_ms: u64,
    pub requested_transcript_path: PathBuf,
    pub canonical_transcript_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexResumeEvidenceError {
    NotRegularFile,
    MetadataMissing,
    TaskStartMissing,
    InvalidRecord,
    BoundsExceeded,
}

impl std::fmt::Display for CodexResumeEvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRegularFile => "transcript is not a regular file",
            Self::MetadataMissing => "resume metadata is missing",
            Self::TaskStartMissing => "resume task start is missing",
            Self::InvalidRecord => "resume transcript record is invalid",
            Self::BoundsExceeded => "resume transcript bounds exceeded",
        })
    }
}

impl std::error::Error for CodexResumeEvidenceError {}

pub fn parse_line(line: &str) -> Option<CodexEvent> {
    parse_timed_line(line).map(|timed| timed.event)
}

pub fn parse_timed_line(line: &str) -> Option<TimedCodexEvent> {
    parse_timed_line_with_capacity(line, None)
}

pub fn read_codex_resume_evidence(
    path: &Path,
) -> Result<CodexResumeEvidence, CodexResumeEvidenceError> {
    let requested_transcript_path = path.to_path_buf();
    let canonical_transcript_path =
        std::fs::canonicalize(path).map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let mut file = options
        .open(&canonical_transcript_path)
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let metadata = file
        .metadata()
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    if !metadata.is_file() {
        return Err(CodexResumeEvidenceError::NotRegularFile);
    }
    let initial_len = metadata.len();

    let head_len = initial_len.min(MAX_CODEX_RESUME_HEAD_BYTES);
    let mut head = Vec::with_capacity(head_len as usize);
    Read::by_ref(&mut file)
        .take(head_len)
        .read_to_end(&mut head)
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    if !head.contains(&b'\n') {
        return Err(if initial_len > head_len {
            CodexResumeEvidenceError::BoundsExceeded
        } else {
            CodexResumeEvidenceError::MetadataMissing
        });
    }
    let first_line = head
        .split(|byte| *byte == b'\n')
        .next()
        .filter(|line| !line.is_empty())
        .ok_or(CodexResumeEvidenceError::MetadataMissing)?;
    let meta = match parse_line(
        std::str::from_utf8(first_line).map_err(|_| CodexResumeEvidenceError::InvalidRecord)?,
    ) {
        Some(CodexEvent::SessionMeta(meta)) => meta,
        _ => return Err(CodexResumeEvidenceError::MetadataMissing),
    };

    let tail_start = initial_len.saturating_sub(MAX_CODEX_RESUME_TAIL_BYTES);
    file.seek(SeekFrom::Start(tail_start))
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let mut tail = Vec::with_capacity((initial_len - tail_start) as usize);
    Read::by_ref(&mut file)
        .take(initial_len - tail_start)
        .read_to_end(&mut tail)
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    if !tail.ends_with(b"\n") {
        return Err(CodexResumeEvidenceError::TaskStartMissing);
    }
    let tail = if tail_start == 0 {
        tail.as_slice()
    } else {
        let newline = tail
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(CodexResumeEvidenceError::BoundsExceeded)?;
        &tail[newline + 1..]
    };
    let complete_tail = tail
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|last_newline| &tail[..last_newline])
        .unwrap_or_default();
    let mut newest = None;
    for line in complete_tail.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let line =
            std::str::from_utf8(line).map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
        serde_json::from_str::<Value>(line).map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
        let Some(timed) = parse_timed_line(line) else {
            continue;
        };
        if let CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted { turn_id }) = timed.event {
            newest = Some(match (turn_id, timed.timestamp_ms) {
                (Some(turn_id), Some(started_at_ms)) => Ok((turn_id, started_at_ms)),
                _ => Err(CodexResumeEvidenceError::TaskStartMissing),
            });
        }
    }
    let (turn_id, started_at_ms) =
        newest.unwrap_or(Err(CodexResumeEvidenceError::TaskStartMissing))?;
    Ok(CodexResumeEvidence {
        child_session_id: meta.session_id,
        provider_session_id: meta
            .provider_session_id
            .ok_or(CodexResumeEvidenceError::MetadataMissing)?,
        parent_thread_id: meta.parent_thread_id,
        turn_id,
        started_at_ms,
        requested_transcript_path,
        canonical_transcript_path,
    })
}

pub fn parse_timed_line_with_capacity(
    line: &str,
    fallback_capacity: Option<u64>,
) -> Option<TimedCodexEvent> {
    let entry: Value = serde_json::from_str(line).ok()?;
    let timestamp_ms = entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .and_then(|timestamp| u64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).ok());
    let event = match entry.get("type").and_then(|v| v.as_str())? {
        "session_meta" => parse_session_meta(entry.get("payload")?).map(CodexEvent::SessionMeta),
        "turn_context" => Some(CodexEvent::TurnContext(parse_turn_context(
            entry.get("payload")?,
        ))),
        "event_msg" => parse_event_msg(entry.get("payload")?, fallback_capacity),
        "response_item" => parse_response_item(entry.get("payload")?).map(CodexEvent::ResponseItem),
        _ => None,
    }?;
    Some(TimedCodexEvent {
        event,
        timestamp_ms,
    })
}

fn parse_session_meta(payload: &Value) -> Option<CodexSessionMeta> {
    Some(CodexSessionMeta {
        session_id: payload.get("id")?.as_str()?.to_string(),
        provider_session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        parent_thread_id: payload
            .get("parent_thread_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
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

fn parse_event_msg(payload: &Value, fallback_capacity: Option<u64>) -> Option<CodexEvent> {
    let event_type = payload.get("type").and_then(|v| v.as_str())?;
    if event_type == "token_count" {
        return parse_token_count(payload, fallback_capacity).map(CodexEvent::TokenCount);
    }

    let event = match event_type {
        "task_started" => CodexLifecycleEvent::TaskStarted {
            turn_id: payload
                .get("turn_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        "task_complete" => CodexLifecycleEvent::TaskComplete,
        "turn_aborted" => CodexLifecycleEvent::TurnAborted,
        "user_message" => CodexLifecycleEvent::UserMessage,
        "agent_message" => CodexLifecycleEvent::AgentMessage,
        other => CodexLifecycleEvent::Other(other.to_string()),
    };
    Some(CodexEvent::Lifecycle(event))
}

fn parse_token_count(payload: &Value, fallback_capacity: Option<u64>) -> Option<CodexTokenCount> {
    let info = payload.get("info")?;
    let capacity = info
        .get("model_context_window")
        .and_then(Value::as_u64)
        .or(fallback_capacity);
    let used = info
        .get("last_token_usage")
        .and_then(|value| value.get("input_tokens"))
        .and_then(Value::as_u64);
    Some(CodexTokenCount {
        context_pressure: used
            .zip(capacity)
            .and_then(|(used, capacity)| crate::context_pressure::percent(used, capacity)),
    })
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
    use std::{fs, sync::mpsc, thread, time::Duration};

    #[cfg(unix)]
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

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
    fn parses_child_provider_session_and_immediate_parent_separately() {
        let line = r#"{"timestamp":"2026-07-27T10:23:05.157Z","type":"session_meta","payload":{"id":"child-2","session_id":"root-1","parent_thread_id":"child-1","cwd":"/work/project","model_provider":"openai"}}"#;

        let CodexEvent::SessionMeta(meta) = parse_line(line).unwrap() else {
            panic!("expected session metadata");
        };
        assert_eq!(meta.session_id, "child-2");
        assert_eq!(meta.provider_session_id.as_deref(), Some("root-1"));
        assert_eq!(meta.parent_thread_id.as_deref(), Some("child-1"));
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
        assert_eq!(count.context_pressure, Some(16));
    }

    #[test]
    fn token_count_pressure_prefers_provider_capacity_over_fallback() {
        let provider_capacity = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":50},"model_context_window":100}}}"#;
        let Some(CodexEvent::TokenCount(count)) =
            parse_timed_line_with_capacity(provider_capacity, Some(200)).map(|timed| timed.event)
        else {
            panic!("expected token count");
        };
        assert_eq!(count.context_pressure, Some(50));

        let fallback_capacity = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1},"last_token_usage":{"input_tokens":50}}}}"#;
        let Some(CodexEvent::TokenCount(count)) =
            parse_timed_line_with_capacity(fallback_capacity, Some(200)).map(|timed| timed.event)
        else {
            panic!("expected token count");
        };
        assert_eq!(count.context_pressure, Some(25));
    }

    #[test]
    fn parses_task_lifecycle_events() {
        let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
        let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;

        assert_eq!(
            parse_line(started),
            Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted {
                turn_id: None,
            }))
        );
        assert_eq!(
            parse_line(complete),
            Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskComplete))
        );
    }

    #[test]
    fn parses_task_started_turn_and_outer_timestamp() {
        let timed = parse_timed_line(
            r#"{"timestamp":"2026-07-27T10:23:05.157Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
        )
        .unwrap();

        assert_eq!(timed.timestamp_ms, Some(1_785_147_785_157));
        assert_eq!(
            timed.event,
            CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted {
                turn_id: Some("turn-2".into()),
            })
        );
    }

    #[test]
    fn timed_parser_preserves_events_and_parses_top_level_rfc3339() {
        let cases = [
            r#"{"timestamp":"2026-07-17T01:02:03.456Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-07-17T01:02:03.456Z","type":"event_msg","payload":{"type":"task_complete"}}"#,
            r#"{"timestamp":"2026-07-17T01:02:03.456Z","type":"event_msg","payload":{"type":"user_message"}}"#,
            r#"{"timestamp":"2026-07-17T01:02:03.456Z","type":"response_item","payload":{"type":"function_call","name":"request_user_input","arguments":"{}","call_id":"call-1"}}"#,
            r#"{"timestamp":"2026-07-17T01:02:03.456Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"ok"}}"#,
        ];
        for line in cases {
            let timed = parse_timed_line(line).unwrap();
            assert_eq!(timed.timestamp_ms, Some(1_784_250_123_456));
            assert_eq!(parse_line(line), Some(timed.event));
        }
    }

    #[test]
    fn missing_or_malformed_timestamp_does_not_discard_event() {
        for line in [
            r#"{"type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"not-a-time","type":"event_msg","payload":{"type":"task_started"}}"#,
        ] {
            let timed = parse_timed_line(line).unwrap();
            assert_eq!(timed.timestamp_ms, None);
            assert_eq!(
                timed.event,
                CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted { turn_id: None })
            );
        }
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

    #[test]
    fn reads_newest_bounded_resume_evidence_from_one_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"parent_thread_id\":\"child-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-new\"}}\n",
            ),
        )
        .unwrap();

        let evidence = read_codex_resume_evidence(&path).unwrap();
        assert_eq!(evidence.child_session_id, "child-2");
        assert_eq!(evidence.provider_session_id, "root-1");
        assert_eq!(evidence.parent_thread_id.as_deref(), Some("child-1"));
        assert_eq!(evidence.turn_id, "turn-new");
        assert_eq!(evidence.started_at_ms, 1_785_146_520_000);
        assert_eq!(evidence.requested_transcript_path, path);
        assert_eq!(
            evidence.canonical_transcript_path,
            std::fs::canonicalize(&evidence.requested_transcript_path).unwrap()
        );
    }

    #[test]
    fn reads_depth_two_resume_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-3\",\"session_id\":\"root-1\",\"parent_thread_id\":\"child-2\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-3\"}}\n",
            ),
        )
        .unwrap();

        let evidence = read_codex_resume_evidence(&path).unwrap();
        assert_eq!(evidence.child_session_id, "child-3");
        assert_eq!(evidence.provider_session_id, "root-1");
        assert_eq!(evidence.parent_thread_id.as_deref(), Some("child-2"));
    }

    #[test]
    fn rejects_directory_as_resume_transcript() {
        let temp = tempfile::tempdir().unwrap();

        assert_eq!(
            read_codex_resume_evidence(temp.path()),
            Err(CodexResumeEvidenceError::NotRegularFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fifo_resume_transcript_without_blocking() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        let path_cstr = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_cstr.as_ptr(), 0o600) }, 0);

        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(read_codex_resume_evidence(&path));
        });

        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Ok(Err(CodexResumeEvidenceError::NotRegularFile))
        );
    }

    #[test]
    fn rejects_newest_task_start_without_turn_id() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
            ),
        )
        .unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }

    #[test]
    fn rejects_newest_task_start_without_outer_timestamp() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-new\"}}\n",
            ),
        )
        .unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }

    #[test]
    fn rejects_unterminated_final_task_start() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-new\"}}"
            ),
        )
        .unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }

    #[test]
    fn rejects_partial_final_row_after_complete_task_start() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\""
            ),
        )
        .unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }

    #[test]
    fn rejects_malformed_complete_tail_row() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
                "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
                "{not-json}\n",
            ),
        )
        .unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::InvalidRecord)
        );
    }

    #[test]
    fn ignores_partial_leading_tail_row() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        let mut transcript = concat!(
            "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
            "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"padding\":\""
        )
        .as_bytes()
        .to_vec();
        transcript.extend(std::iter::repeat_n(b'x', 8 * 1024 * 1024));
        transcript.extend_from_slice(
            b"\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-too-old\"}}\n",
        );
        fs::write(&path, transcript).unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }

    #[test]
    fn reports_missing_metadata_for_invalid_first_record() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        fs::write(&path, b"{not-json}\n").unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::MetadataMissing)
        );
    }

    #[test]
    fn bounds_metadata_to_the_first_mebibyte() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        let mut transcript = vec![b'x'; 1024 * 1024];
        transcript.extend_from_slice(b"\n");
        fs::write(&path, transcript).unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::BoundsExceeded)
        );
    }

    #[test]
    fn does_not_read_task_starts_before_the_last_eight_mebibytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rollout-child.jsonl");
        let mut transcript = concat!(
            "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"cwd\":\"/work\"}}\n",
            "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-too-old\"}}\n",
        )
        .as_bytes()
        .to_vec();
        transcript.extend(std::iter::repeat_n(b'x', 8 * 1024 * 1024));
        transcript.push(b'\n');
        fs::write(&path, transcript).unwrap();

        assert_eq!(
            read_codex_resume_evidence(&path),
            Err(CodexResumeEvidenceError::TaskStartMissing)
        );
    }
}
