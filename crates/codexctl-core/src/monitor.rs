use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use serde_json::Value;

use crate::codex_transcript::{
    CodexEvent, CodexLifecycleEvent, CodexResponseItem, CodexResponseKind,
    parse_line as parse_codex_line,
};
use crate::models;
use crate::session::{
    CodexSession, CodexTaskState, SessionStatus, SubagentRollup, TelemetryStatus,
};
use crate::transcript::{TranscriptBlock, TranscriptEvent, TranscriptRole, parse_line};

#[derive(Default)]
struct UsageRollup {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    cost_usd: f64,
    usage_metrics_available: bool,
    cost_estimate_unverified: bool,
}

impl UsageRollup {
    fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Read new JSONL entries since last offset, accumulate token stats.
pub fn update_tokens(session: &mut CodexSession) {
    if should_use_codex_parser(session) {
        update_codex_tokens(session);
        return;
    }

    // Seed from persisted state so status inference works on ticks with no new JSONL.
    let mut last_type = session.last_msg_type.clone();
    let mut last_stop_reason = session.last_stop_reason.clone();
    let mut is_waiting_for_task = session.is_waiting_for_task;
    let mut saw_non_empty_line = false;
    let mut recognized_events = 0usize;
    let mut saw_parent_usage = false;
    let jsonl_path = session.jsonl_path.clone();

    match jsonl_path.as_ref() {
        Some(path) => {
            let mut file = match File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    session.telemetry_status = TelemetryStatus::UnreadableTranscript;
                    finalize_usage(
                        session,
                        &last_type,
                        &last_stop_reason,
                        is_waiting_for_task,
                        false,
                    );
                    return;
                }
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);

            if file_len == 0 {
                session.telemetry_status = TelemetryStatus::Pending;
            } else {
                if session.jsonl_offset > file_len {
                    session.jsonl_offset = 0;
                    session.own_input_tokens = 0;
                    session.own_output_tokens = 0;
                    session.own_cache_read_tokens = 0;
                    session.own_cache_write_tokens = 0;
                    // Reset persisted inference state on file truncation
                    last_type.clear();
                    last_stop_reason.clear();
                    is_waiting_for_task = false;
                }

                if session.jsonl_offset < file_len {
                    if session.jsonl_offset > 0
                        && file.seek(SeekFrom::Start(session.jsonl_offset)).is_err()
                    {
                        finalize_usage(
                            session,
                            &last_type,
                            &last_stop_reason,
                            is_waiting_for_task,
                            false,
                        );
                        return;
                    }

                    let reader = BufReader::new(&file);

                    for line in reader.lines() {
                        let line = match line {
                            Ok(l) => l,
                            Err(_) => break,
                        };

                        if line.trim().is_empty() {
                            continue;
                        }
                        saw_non_empty_line = true;

                        let Some(event) = parse_line(&line) else {
                            continue;
                        };
                        recognized_events += 1;

                        match event {
                            TranscriptEvent::WaitingForTask => {
                                is_waiting_for_task = true;
                            }
                            TranscriptEvent::Message(message) => {
                                is_waiting_for_task = false;
                                last_type = match message.role {
                                    TranscriptRole::Assistant => "assistant".to_string(),
                                    TranscriptRole::User => "user".to_string(),
                                };

                                if let Some(reason) = message.stop_reason {
                                    last_stop_reason = reason;
                                } else {
                                    // Some transcripts write assistant messages
                                    // with stop_reason: null when a tool_use block is
                                    // awaiting user approval.  Infer from content.
                                    let has_tool_use = message
                                        .content
                                        .iter()
                                        .any(|b| matches!(b, TranscriptBlock::ToolUse { .. }));
                                    if has_tool_use {
                                        last_stop_reason = "tool_use".to_string();
                                    } else {
                                        last_stop_reason.clear();
                                    }
                                }

                                if let Some(usage) = message.usage {
                                    let input = usage.input_tokens;
                                    let cache_read = usage.cache_read_input_tokens;
                                    let cache_create = usage.cache_creation_input_tokens;
                                    let output = usage.output_tokens;

                                    session.own_input_tokens += input + cache_read + cache_create;
                                    session.own_output_tokens += output;
                                    session.own_cache_read_tokens += cache_read;
                                    session.own_cache_write_tokens += cache_create;
                                    saw_parent_usage = true;

                                    // Track context window: the input_tokens of the LAST API call
                                    // represents the current prompt/context size
                                    let context_size = input + cache_read + cache_create;
                                    if context_size > 0 {
                                        session.context_tokens = context_size;
                                    }
                                }

                                if let Some(model) = message.model {
                                    session.model = shorten_model(&model);
                                }

                                for block in message.content {
                                    match &block {
                                        TranscriptBlock::ToolUse { name, input } => {
                                            record_tool_usage(name, input, session);
                                            // Track pending tool for rule-based auto-actions
                                            session.pending_tool_name = Some(name.clone());
                                            session.pending_tool_input = input
                                                .get("command")
                                                .and_then(|v| v.as_str())
                                                .map(|s| s.to_string());
                                            // Track pending file path for conflict detection
                                            session.pending_file_path = if matches!(
                                                name.as_str(),
                                                "Edit" | "Write" | "NotebookEdit"
                                            ) {
                                                input
                                                    .get("file_path")
                                                    .and_then(|v| v.as_str())
                                                    .map(|s| s.to_string())
                                            } else {
                                                None
                                            };
                                        }
                                        TranscriptBlock::ToolResult {
                                            is_error, content, ..
                                        } => {
                                            session.last_tool_error = *is_error;
                                            if *is_error {
                                                session.total_error_count += 1;
                                                session.current_window_errors += 1;
                                                let truncated = if content.len() > 256 {
                                                    format!(
                                                        "{}...",
                                                        crate::session::truncate_str(content, 256)
                                                    )
                                                } else {
                                                    content.clone()
                                                };
                                                let tool_name = session
                                                    .pending_tool_name
                                                    .clone()
                                                    .unwrap_or_else(|| "?".into());
                                                session.last_error_message =
                                                    Some(truncated.clone());
                                                session.recent_errors.push(
                                                    crate::session::ErrorEntry {
                                                        tool_name,
                                                        message: truncated,
                                                    },
                                                );
                                                if session.recent_errors.len() > 5 {
                                                    session.recent_errors.remove(0);
                                                }
                                            } else {
                                                session.last_error_message = None;
                                            }
                                            // Tool was executed — no longer pending
                                            session.pending_tool_name = None;
                                            session.pending_tool_input = None;
                                            session.pending_file_path = None;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }

                if recognized_events > 0 || session.telemetry_status.is_available() {
                    session.telemetry_status = TelemetryStatus::Available;
                } else if saw_non_empty_line {
                    session.telemetry_status = TelemetryStatus::UnsupportedTranscript;
                } else {
                    session.telemetry_status = TelemetryStatus::Pending;
                }

                session.jsonl_offset = file_len;
            }

            if let Ok(meta) = std::fs::metadata(path) {
                if let Ok(modified) = meta.modified() {
                    let mtime_ms = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    session.last_message_ts = mtime_ms;
                }
            }
        }
        None => {
            session.telemetry_status = TelemetryStatus::MissingTranscript;
        }
    }

    finalize_usage(
        session,
        &last_type,
        &last_stop_reason,
        is_waiting_for_task,
        saw_parent_usage,
    );
}

fn should_use_codex_parser(session: &CodexSession) -> bool {
    !session.process_backed
        || session.model_profile_source == "codex-transcript"
        || session
            .jsonl_path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-"))
}

fn update_codex_tokens(session: &mut CodexSession) {
    let mut last_type = session.last_msg_type.clone();
    let mut last_stop_reason = session.last_stop_reason.clone();
    let mut saw_non_empty_line = false;
    let mut recognized_events = 0usize;
    let mut saw_parent_usage = false;
    let mut codex_context_max = None;
    let previous_context_max = session.context_max;
    let jsonl_path = session.jsonl_path.clone();

    match jsonl_path.as_ref() {
        Some(path) => {
            let mut file = match File::open(path) {
                Ok(file) => file,
                Err(_) => {
                    session.telemetry_status = TelemetryStatus::UnreadableTranscript;
                    finalize_usage(session, &last_type, &last_stop_reason, false, false);
                    return;
                }
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len == 0 {
                session.telemetry_status = TelemetryStatus::Pending;
            } else {
                if session.jsonl_offset > file_len {
                    session.jsonl_offset = 0;
                    last_type.clear();
                    last_stop_reason.clear();
                    session.task_state = CodexTaskState::Unknown;
                    session.explicit_input_required = false;
                    clear_pending_tool(session);
                }

                if session.jsonl_offset < file_len {
                    if session.jsonl_offset > 0
                        && file.seek(SeekFrom::Start(session.jsonl_offset)).is_err()
                    {
                        finalize_usage(session, &last_type, &last_stop_reason, false, false);
                        return;
                    }

                    let reader = BufReader::new(&file);
                    for line in reader.lines().map_while(Result::ok) {
                        if line.trim().is_empty() {
                            continue;
                        }
                        saw_non_empty_line = true;

                        let Some(event) = parse_codex_line(&line) else {
                            continue;
                        };
                        recognized_events += 1;

                        match event {
                            CodexEvent::SessionMeta(meta) => {
                                if session.cwd.is_empty() {
                                    session.cwd = meta.cwd;
                                }
                            }
                            CodexEvent::TurnContext(ctx) => {
                                if let Some(model) = ctx.model {
                                    session.model = shorten_model(&model);
                                }
                            }
                            CodexEvent::TokenCount(count) => {
                                session.own_input_tokens = count.total.input_tokens;
                                session.own_output_tokens = count.total.output_tokens;
                                session.own_cache_read_tokens = count.total.cached_input_tokens;
                                session.own_cache_write_tokens = 0;
                                session.context_tokens = count.last.input_tokens;
                                codex_context_max = count.model_context_window;
                                saw_parent_usage = true;
                            }
                            CodexEvent::Lifecycle(event) => {
                                match &event {
                                    CodexLifecycleEvent::TaskStarted => {
                                        last_stop_reason.clear();
                                    }
                                    CodexLifecycleEvent::TaskComplete => {
                                        last_type = "assistant".into();
                                        last_stop_reason = "end_turn".into();
                                    }
                                    CodexLifecycleEvent::TurnAborted => {
                                        last_type = "assistant".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexLifecycleEvent::UserMessage => {
                                        last_type = "user".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexLifecycleEvent::AgentMessage => {
                                        last_type = "assistant".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexLifecycleEvent::Other(_) => {}
                                }
                                apply_lifecycle(event, session);
                            }
                            CodexEvent::ResponseItem(item) => {
                                let kind = item.kind;
                                let role = item.role.clone();
                                apply_codex_response_item(item, session);
                                match kind {
                                    CodexResponseKind::Message => {
                                        if let Some(role) = role {
                                            match role.as_str() {
                                                "user" => {
                                                    last_type = "user".into();
                                                    last_stop_reason.clear();
                                                }
                                                "assistant" => {
                                                    last_type = "assistant".into();
                                                    last_stop_reason = "end_turn".into();
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                    CodexResponseKind::FunctionCall => {
                                        last_type = "assistant".into();
                                        last_stop_reason = "tool_use".into();
                                    }
                                    CodexResponseKind::FunctionCallOutput => {
                                        last_type = "assistant".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexResponseKind::CustomToolCall => {
                                        last_type = "assistant".into();
                                        last_stop_reason = "tool_use".into();
                                    }
                                    CodexResponseKind::CustomToolCallOutput => {
                                        last_type = "assistant".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexResponseKind::Reasoning => {
                                        last_type = "assistant".into();
                                        last_stop_reason.clear();
                                    }
                                    CodexResponseKind::Other => {}
                                }
                            }
                        }
                    }
                }

                if recognized_events > 0 || session.telemetry_status.is_available() {
                    session.telemetry_status = TelemetryStatus::Available;
                } else if saw_non_empty_line {
                    session.telemetry_status = TelemetryStatus::UnsupportedTranscript;
                } else {
                    session.telemetry_status = TelemetryStatus::Pending;
                }

                session.jsonl_offset = file_len;
            }

            if let Ok(meta) = std::fs::metadata(path) {
                if let Ok(modified) = meta.modified() {
                    session.last_message_ts = modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                }
            }
        }
        None => {
            session.telemetry_status = TelemetryStatus::MissingTranscript;
        }
    }

    finalize_usage(
        session,
        &last_type,
        &last_stop_reason,
        false,
        saw_parent_usage,
    );
    if let Some(max) = codex_context_max {
        session.context_max = max;
    } else if previous_context_max > 0 && session.context_tokens > 0 {
        session.context_max = previous_context_max;
    }
}

fn apply_lifecycle(event: CodexLifecycleEvent, session: &mut CodexSession) {
    match event {
        CodexLifecycleEvent::TaskStarted | CodexLifecycleEvent::UserMessage => {
            session.task_state = CodexTaskState::Processing;
            session.explicit_input_required = false;
            clear_pending_tool(session);
        }
        CodexLifecycleEvent::AgentMessage => {
            session.task_state = CodexTaskState::Processing;
            session.explicit_input_required = false;
        }
        CodexLifecycleEvent::TaskComplete => {
            session.task_state = CodexTaskState::WaitingInput;
            session.explicit_input_required = false;
            clear_pending_tool(session);
        }
        CodexLifecycleEvent::TurnAborted => {
            session.task_state = CodexTaskState::Aborted;
            session.explicit_input_required = false;
            clear_pending_tool(session);
        }
        CodexLifecycleEvent::Other(_) => {}
    }
}

fn apply_codex_response_item(item: CodexResponseItem, session: &mut CodexSession) {
    match item.kind {
        CodexResponseKind::Message | CodexResponseKind::Reasoning => {
            session.task_state = CodexTaskState::Processing;
            session.explicit_input_required = false;
        }
        CodexResponseKind::FunctionCall | CodexResponseKind::CustomToolCall => {
            let is_custom = item.kind == CodexResponseKind::CustomToolCall;
            let tool_name = item.name.unwrap_or_else(|| "unknown".into());
            let raw_input = item.arguments.unwrap_or_default();
            let input = serde_json::from_str::<Value>(&raw_input).unwrap_or(Value::Null);
            record_tool_usage(&tool_name, &input, session);
            session.task_state = CodexTaskState::Processing;
            session.explicit_input_required = tool_name == "request_user_input";
            session.pending_tool_call_id = item.call_id;
            session.pending_tool_input = if is_custom {
                (!raw_input.is_empty()).then_some(raw_input)
            } else {
                input
                    .get("cmd")
                    .or_else(|| input.get("command"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            };
            session.pending_file_path = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            session.pending_tool_name = Some(tool_name);
        }
        CodexResponseKind::FunctionCallOutput | CodexResponseKind::CustomToolCallOutput => {
            session.task_state = CodexTaskState::Processing;
            if item.call_id.is_some() && item.call_id == session.pending_tool_call_id {
                session.last_tool_error = false;
                session.last_error_message = None;
                session.explicit_input_required = false;
                clear_pending_tool(session);
            }
        }
        CodexResponseKind::Other => {}
    }
}

fn clear_pending_tool(session: &mut CodexSession) {
    session.pending_tool_name = None;
    session.pending_tool_call_id = None;
    session.pending_tool_input = None;
    session.pending_file_path = None;
}

fn finalize_usage(
    session: &mut CodexSession,
    last_type: &str,
    last_stop_reason: &str,
    is_waiting_for_task: bool,
    saw_parent_usage: bool,
) {
    let resolved_profile = models::resolve(&session.model);
    session.context_max = resolved_profile.profile.context_max;
    session.model_profile_source = resolved_profile.source.label().to_string();

    let subagent_rollup = refresh_subagent_rollups(session);
    session.subagent_input_tokens = subagent_rollup.total_input_tokens();
    session.subagent_output_tokens = subagent_rollup.output_tokens;
    session.subagent_cache_read_tokens = subagent_rollup.cache_read_tokens;
    session.subagent_cache_write_tokens = subagent_rollup.cache_write_tokens;
    session.subagent_count = session.subagent_rollups.len();

    session.total_input_tokens = session.own_input_tokens + session.subagent_input_tokens;
    session.total_output_tokens = session.own_output_tokens + session.subagent_output_tokens;
    session.cache_read_tokens = session.own_cache_read_tokens + session.subagent_cache_read_tokens;
    session.cache_write_tokens =
        session.own_cache_write_tokens + session.subagent_cache_write_tokens;

    let own_usage_metrics_available = saw_parent_usage
        || session.own_input_tokens > 0
        || session.own_output_tokens > 0
        || session.own_cache_read_tokens > 0
        || session.own_cache_write_tokens > 0;
    let (own_cost, own_cost_unverified) = estimate_cost_components(
        &session.model,
        session.own_input_tokens,
        session.own_output_tokens,
        session.own_cache_read_tokens,
        session.own_cache_write_tokens,
    );
    session.cost_usd = own_cost + subagent_rollup.cost_usd;
    session.usage_metrics_available =
        own_usage_metrics_available || subagent_rollup.usage_metrics_available;
    session.cost_estimate_unverified = (own_usage_metrics_available && own_cost_unverified)
        || subagent_rollup.cost_estimate_unverified;

    // Persist for next tick (so status inference works when no new JSONL arrives).
    session.last_msg_type = last_type.to_string();
    session.last_stop_reason = last_stop_reason.to_string();
    session.is_waiting_for_task = is_waiting_for_task;

    infer_status(session, last_type, last_stop_reason, is_waiting_for_task);
}

pub fn refresh_status(session: &mut CodexSession) {
    let last_type = session.last_msg_type.clone();
    let stop_reason = session.last_stop_reason.clone();
    infer_status(
        session,
        &last_type,
        &stop_reason,
        session.is_waiting_for_task,
    );
}

pub fn infer_status(
    session: &mut CodexSession,
    last_msg_type: &str,
    last_stop_reason: &str,
    is_waiting_for_task: bool,
) {
    if session.explicit_input_required {
        session.status = SessionStatus::NeedsInput;
        return;
    }

    match session.task_state {
        CodexTaskState::Processing => {
            session.status = SessionStatus::Processing;
            return;
        }
        CodexTaskState::WaitingInput | CodexTaskState::Aborted => {
            session.status = recent_waiting_or_idle(session.last_message_ts);
            return;
        }
        CodexTaskState::Unknown => {}
    }

    // High CPU is evidence of processing, but low CPU never authorizes input.
    if session.cpu_percent > 5.0 {
        session.status = SessionStatus::Processing;
        return;
    }

    // Preserve the legacy explicit waiting signal.
    if is_waiting_for_task {
        session.status = SessionStatus::NeedsInput;
        return;
    }

    if !session.telemetry_status.is_available() && last_msg_type.is_empty() {
        session.status = SessionStatus::Unknown;
        return;
    }

    if last_msg_type == "assistant" && last_stop_reason == "end_turn" {
        session.status = recent_waiting_or_idle(session.last_message_ts);
        return;
    }

    if last_msg_type == "assistant" && last_stop_reason == "tool_use" {
        session.status = SessionStatus::Processing;
        return;
    }

    if last_msg_type == "user" {
        session.status = SessionStatus::Processing;
        return;
    }

    session.status = SessionStatus::Idle;
}

fn recent_waiting_or_idle(last_message_ts: u64) -> SessionStatus {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let age_mins = (now_ms.saturating_sub(last_message_ts)) / 60_000;
    if age_mins > 10 {
        SessionStatus::Idle
    } else {
        SessionStatus::WaitingInput
    }
}

/// Estimate USD cost based on token usage and model.
#[allow(dead_code)]
pub fn estimate_cost(session: &CodexSession) -> f64 {
    estimate_cost_components(
        &session.model,
        session.total_input_tokens,
        session.total_output_tokens,
        session.cache_read_tokens,
        session.cache_write_tokens,
    )
    .0
}

/// Max context window tokens by model.
pub fn model_context_max(model: &str) -> u64 {
    models::resolve(model).profile.context_max
}

/// Extract tool usage stats and file paths from tool_use content blocks.
fn record_tool_usage(tool_name: &str, input: &Value, session: &mut CodexSession) {
    if tool_name.is_empty() {
        return;
    }

    session
        .tool_usage
        .entry(tool_name.to_string())
        .or_default()
        .calls += 1;

    if matches!(tool_name, "Edit" | "Write" | "NotebookEdit") {
        if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
            *session.files_modified.entry(path.to_string()).or_insert(0) += 1;
            // Reset file-read tracker for this path (it was just edited)
            session.file_reads_since_edit.remove(path);
        }
        // Track token efficiency: cumulative tokens at each edit event
        let total_tokens = session.total_input_tokens + session.total_output_tokens;
        session.total_tokens_at_edit_count += total_tokens;
        session.edit_event_count += 1;
        // Freeze baseline tokens-per-edit after first 5 edits
        if session.baseline_tokens_per_edit.is_none() && session.edit_event_count >= 5 {
            session.baseline_tokens_per_edit =
                Some(session.total_tokens_at_edit_count as f64 / session.edit_event_count as f64);
        }
    }

    // Track file reads for repetition detection
    if matches!(tool_name, "Read" | "Grep" | "Glob") {
        if let Some(path) = input.get("file_path").and_then(|p| p.as_str()) {
            *session
                .file_reads_since_edit
                .entry(path.to_string())
                .or_insert(0) += 1;
        }
    }
}

pub fn shorten_model(model: &str) -> String {
    models::shorten_model(model)
}

fn refresh_subagent_rollups(session: &mut CodexSession) -> UsageRollup {
    for path in session.active_subagent_jsonl_paths.clone() {
        let rollup = session.subagent_rollups.entry(path.clone()).or_default();
        update_subagent_rollup(&path, rollup, &session.model);
    }

    let mut totals = UsageRollup::default();
    for rollup in session.subagent_rollups.values() {
        totals.input_tokens += rollup.input_tokens;
        totals.output_tokens += rollup.output_tokens;
        totals.cache_read_tokens += rollup.cache_read_tokens;
        totals.cache_write_tokens += rollup.cache_write_tokens;
        totals.cost_usd += rollup.cost_usd;
        totals.usage_metrics_available |= rollup.usage_metrics_available;
        totals.cost_estimate_unverified |= rollup.cost_estimate_unverified;
    }
    totals
}

fn update_subagent_rollup(
    path: &std::path::Path,
    rollup: &mut SubagentRollup,
    default_model: &str,
) {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return,
    };

    let file_len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    if rollup.jsonl_offset > file_len {
        *rollup = SubagentRollup::default();
    }

    if rollup.jsonl_offset >= file_len {
        rollup.jsonl_offset = file_len;
        return;
    }

    if rollup.jsonl_offset > 0 && file.seek(SeekFrom::Start(rollup.jsonl_offset)).is_err() {
        return;
    }

    let mut current_model = if rollup.model.is_empty() {
        default_model.to_string()
    } else {
        rollup.model.clone()
    };

    let reader = BufReader::new(&file);
    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let Some(TranscriptEvent::Message(message)) = parse_line(&line) else {
            continue;
        };

        if let Some(model) = message.model {
            current_model = shorten_model(&model);
            rollup.model = current_model.clone();
        }

        let Some(usage) = message.usage else {
            continue;
        };

        rollup.input_tokens += usage.input_tokens;
        rollup.output_tokens += usage.output_tokens;
        rollup.cache_read_tokens += usage.cache_read_input_tokens;
        rollup.cache_write_tokens += usage.cache_creation_input_tokens;
        rollup.usage_metrics_available = true;

        let input_with_cache =
            usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
        let model_for_cost = if current_model.is_empty() {
            default_model
        } else {
            current_model.as_str()
        };
        let (delta_cost, unverified) = estimate_cost_components(
            model_for_cost,
            input_with_cache,
            usage.output_tokens,
            usage.cache_read_input_tokens,
            usage.cache_creation_input_tokens,
        );
        rollup.cost_usd += delta_cost;
        rollup.cost_estimate_unverified |= unverified;
    }

    rollup.jsonl_offset = file_len;
}

fn estimate_cost_components(
    model: &str,
    total_input_tokens: u64,
    total_output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> (f64, bool) {
    let plain_input = total_input_tokens
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens);
    let resolved = models::resolve(model);

    let cost = (plain_input as f64 / 1_000_000.0) * resolved.profile.input_per_m
        + (total_output_tokens as f64 / 1_000_000.0) * resolved.profile.output_per_m
        + (cache_read_tokens as f64 / 1_000_000.0) * resolved.profile.cache_read_per_m
        + (cache_write_tokens as f64 / 1_000_000.0) * resolved.profile.cache_write_per_m;

    (
        cost,
        resolved.source == models::ModelProfileSource::Fallback,
    )
}
