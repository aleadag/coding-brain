#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::transcript::{TranscriptBlock, TranscriptEvent, parse_line};

// ────────────────────────────────────────────────────────────────────────────
// Data structures
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCategory {
    ErrorCascade,
    WastedReads,
    UndoRedoLoop,
}

impl FindingCategory {
    pub fn label(&self) -> &'static str {
        match self {
            FindingCategory::ErrorCascade => "Error Cascade",
            FindingCategory::WastedReads => "Wasted Reads",
            FindingCategory::UndoRedoLoop => "Undo-Redo Loop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

impl FindingSeverity {
    pub fn label(&self) -> &'static str {
        match self {
            FindingSeverity::Info => "info",
            FindingSeverity::Warning => "warning",
            FindingSeverity::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AutopsyFinding {
    pub category: FindingCategory,
    pub severity: FindingSeverity,
    pub summary: String,
    pub detail: Option<String>,
    pub message_range: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct QualityScore {
    pub overall: u8,
    pub ran_tests: bool,
    pub tests_passed: Option<bool>,
    pub ran_lint: bool,
    pub edit_efficiency: u8,
}

#[derive(Debug, Clone)]
pub struct AutopsyReport {
    pub session_id: String,
    pub project: String,
    pub model: String,
    pub duration_secs: u64,
    pub total_messages: usize,
    pub total_tool_calls: u32,
    pub total_errors: u32,
    pub quality: QualityScore,
    pub findings: Vec<AutopsyFinding>,
    pub generated_at: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Transcript walker — builds analysis state from JSONL
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct ToolCall {
    pub message_idx: usize,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub is_error: bool,
    pub result_content: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TranscriptWalker {
    pub tool_calls: Vec<ToolCall>,
    pub message_count: usize,
    /// file_path -> message indices for each read
    pub read_history: HashMap<String, Vec<usize>>,
    /// file_path -> message indices for each edit
    pub edit_history: HashMap<String, Vec<usize>>,
    /// Model seen in transcript
    pub model: String,
    /// Duration: last message timestamp - first message timestamp (approximate via message count)
    pub duration_secs: u64,
}

impl TranscriptWalker {
    /// Build a walker by reading an entire JSONL file.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let content =
            fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Self::from_lines(content.lines())
    }

    /// Build a walker from an iterator of JSONL lines.
    pub fn from_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Self, String> {
        let mut tool_calls = Vec::new();
        let mut read_history: HashMap<String, Vec<usize>> = HashMap::new();
        let mut edit_history: HashMap<String, Vec<usize>> = HashMap::new();
        let mut message_idx: usize = 0;
        let mut model = String::new();

        // Track pending tool uses within a single message so we can pair
        // them with the ToolResult blocks that follow in the same message
        // (user messages contain interleaved ToolResult blocks) or match
        // them across assistant→user message boundaries.
        let mut pending_tools: Vec<(String, serde_json::Value, usize)> = Vec::new();

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(event) = parse_line(line) else {
                continue;
            };
            match event {
                TranscriptEvent::WaitingForTask => {}
                TranscriptEvent::Message(msg) => {
                    if let Some(ref m) = msg.model {
                        model = m.clone();
                    }

                    for block in &msg.content {
                        match block {
                            TranscriptBlock::ToolUse { name, input } => {
                                pending_tools.push((name.clone(), input.clone(), message_idx));

                                // Track reads
                                if matches!(name.as_str(), "Read" | "Grep" | "Glob") {
                                    if let Some(fp) =
                                        input.get("file_path").and_then(|v| v.as_str())
                                    {
                                        read_history
                                            .entry(fp.to_string())
                                            .or_default()
                                            .push(message_idx);
                                    }
                                }

                                // Track edits
                                if matches!(name.as_str(), "Edit" | "Write" | "NotebookEdit") {
                                    if let Some(fp) =
                                        input.get("file_path").and_then(|v| v.as_str())
                                    {
                                        edit_history
                                            .entry(fp.to_string())
                                            .or_default()
                                            .push(message_idx);
                                    }
                                }
                            }
                            TranscriptBlock::ToolResult { is_error, content } => {
                                // Match with the oldest pending tool
                                if let Some((name, input, call_idx)) =
                                    pending_tools.first().cloned()
                                {
                                    pending_tools.remove(0);
                                    tool_calls.push(ToolCall {
                                        message_idx: call_idx,
                                        tool_name: name,
                                        input,
                                        is_error: *is_error,
                                        result_content: Some(content.clone()),
                                    });
                                }
                            }
                            TranscriptBlock::Text(_) => {}
                        }
                    }

                    message_idx += 1;
                }
            }
        }

        // Flush any remaining pending tools (no result received — session may have been interrupted)
        for (name, input, call_idx) in pending_tools {
            tool_calls.push(ToolCall {
                message_idx: call_idx,
                tool_name: name,
                input,
                is_error: false,
                result_content: None,
            });
        }

        Ok(Self {
            tool_calls,
            message_count: message_idx,
            read_history,
            edit_history,
            model,
            duration_secs: 0, // Filled by caller if needed
        })
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Detectors
// ────────────────────────────────────────────────────────────────────────────

/// Detect chains of consecutive errors on the same tool (2+ in a row).
pub(crate) fn detect_error_cascades(walker: &TranscriptWalker) -> Vec<AutopsyFinding> {
    let mut findings = Vec::new();
    let mut i = 0;
    let calls = &walker.tool_calls;

    while i < calls.len() {
        if !calls[i].is_error {
            i += 1;
            continue;
        }

        let chain_start = i;
        let tool_name = &calls[i].tool_name;
        let mut chain_len = 1;

        while i + chain_len < calls.len()
            && calls[i + chain_len].is_error
            && calls[i + chain_len].tool_name == *tool_name
        {
            chain_len += 1;
        }

        if chain_len >= 2 {
            let last = &calls[chain_start + chain_len - 1];

            let severity = if chain_len >= 5 {
                FindingSeverity::Critical
            } else if chain_len >= 3 {
                FindingSeverity::Warning
            } else {
                FindingSeverity::Info
            };

            let first_error = calls[chain_start]
                .result_content
                .as_deref()
                .unwrap_or("(unknown)");
            let truncated = if first_error.len() > 120 {
                format!("{}...", &first_error[..120])
            } else {
                first_error.to_string()
            };

            findings.push(AutopsyFinding {
                category: FindingCategory::ErrorCascade,
                severity,
                summary: format!(
                    "[{tool_name}] {chain_len} consecutive errors starting at message {}",
                    calls[chain_start].message_idx
                ),
                detail: Some(format!("First error: {truncated}")),
                message_range: (calls[chain_start].message_idx, last.message_idx),
            });
        }

        i += chain_len;
    }

    findings
}

/// Detect files read multiple times without an intervening edit.
pub(crate) fn detect_wasted_reads(walker: &TranscriptWalker) -> Vec<AutopsyFinding> {
    let mut findings = Vec::new();

    for (file_path, reads) in &walker.read_history {
        if reads.len() < 3 {
            continue;
        }

        // Check if there are edits interspersed
        let edits = walker.edit_history.get(file_path.as_str());
        let edit_indices = edits.cloned().unwrap_or_default();

        // Count reads with no edit between them
        let mut consecutive_reads = 0u32;
        let mut prev_read_idx: Option<usize> = None;

        for &read_idx in reads {
            let had_edit = if let Some(prev) = prev_read_idx {
                edit_indices.iter().any(|&e| e > prev && e < read_idx)
            } else {
                true // First read is never wasted
            };

            if had_edit {
                consecutive_reads = 1;
            } else {
                consecutive_reads += 1;
            }
            prev_read_idx = Some(read_idx);
        }

        if consecutive_reads >= 3 {
            let short_path = shorten_path(file_path);
            findings.push(AutopsyFinding {
                category: FindingCategory::WastedReads,
                severity: if consecutive_reads >= 5 {
                    FindingSeverity::Warning
                } else {
                    FindingSeverity::Info
                },
                summary: format!(
                    "{short_path} read {consecutive_reads} times without an intervening edit"
                ),
                detail: None,
                message_range: (
                    reads.first().copied().unwrap_or(0),
                    reads.last().copied().unwrap_or(0),
                ),
            });
        }
    }

    findings
}

/// Detect edit-error-edit cycles on the same file (undo-redo pattern).
pub(crate) fn detect_undo_redo_loops(walker: &TranscriptWalker) -> Vec<AutopsyFinding> {
    let mut findings = Vec::new();

    for (file_path, edits) in &walker.edit_history {
        if edits.len() < 3 {
            continue;
        }

        // Check if edits to this file are interspersed with errors
        let mut edit_error_edit_count = 0u32;
        for window in edits.windows(2) {
            let idx_a = window[0];
            let idx_b = window[1];

            // Look for error tool calls between these two edits
            let has_error_between = walker
                .tool_calls
                .iter()
                .any(|tc| tc.message_idx > idx_a && tc.message_idx < idx_b && tc.is_error);

            if has_error_between {
                edit_error_edit_count += 1;
            }
        }

        if edit_error_edit_count >= 2 {
            let short_path = shorten_path(file_path);

            findings.push(AutopsyFinding {
                category: FindingCategory::UndoRedoLoop,
                severity: if edit_error_edit_count >= 4 {
                    FindingSeverity::Critical
                } else {
                    FindingSeverity::Warning
                },
                summary: format!(
                    "{short_path}: {edit_error_edit_count} edit-error-edit cycles ({} total edits)",
                    edits.len()
                ),
                detail: Some(
                    "Session repeatedly edited this file, hit errors, and re-edited".to_string(),
                ),
                message_range: (
                    edits.first().copied().unwrap_or(0),
                    edits.last().copied().unwrap_or(0),
                ),
            });
        }
    }

    findings
}

/// Compute a quality score by examining what happened at the end of the session.
pub(crate) fn compute_quality_score(walker: &TranscriptWalker) -> QualityScore {
    let mut ran_tests = false;
    let mut tests_passed: Option<bool> = None;
    let mut ran_lint = false;

    // Scan all Bash tool calls for test/lint commands
    for call in &walker.tool_calls {
        if call.tool_name != "Bash" {
            continue;
        }
        let cmd = call
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let is_test = cmd.contains("cargo test")
            || cmd.contains("npm test")
            || cmd.contains("pytest")
            || cmd.contains("jest")
            || cmd.contains("go test")
            || cmd.contains("make test");

        let is_lint = cmd.contains("cargo clippy")
            || cmd.contains("cargo fmt")
            || cmd.contains("eslint")
            || cmd.contains("prettier")
            || cmd.contains("ruff")
            || cmd.contains("black");

        if is_test {
            ran_tests = true;
            tests_passed = Some(!call.is_error);
        }
        if is_lint {
            ran_lint = true;
        }
    }

    // Edit efficiency: unique files edited / total edit operations
    let total_edits: usize = walker.edit_history.values().map(|v| v.len()).sum();
    let unique_files = walker.edit_history.len();
    let edit_efficiency = if total_edits > 0 {
        ((unique_files as f64 / total_edits as f64) * 100.0).clamp(0.0, 100.0) as u8
    } else {
        100
    };

    // Overall score
    let mut score: u8 = 50;
    if ran_tests {
        score += 15;
    }
    if tests_passed == Some(true) {
        score += 15;
    }
    if tests_passed == Some(false) {
        score = score.saturating_sub(20);
    }
    if ran_lint {
        score += 10;
    }
    // Bonus for edit efficiency
    if edit_efficiency > 70 {
        score += 10;
    }
    // Penalty for high error rate
    let error_count = walker.tool_calls.iter().filter(|c| c.is_error).count();
    let total_calls = walker.tool_calls.len().max(1);
    let error_rate = error_count as f64 / total_calls as f64;
    if error_rate > 0.3 {
        score = score.saturating_sub(15);
    }
    if error_rate > 0.5 {
        score = score.saturating_sub(10);
    }

    QualityScore {
        overall: score.min(100),
        ran_tests,
        tests_passed,
        ran_lint,
        edit_efficiency,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Main entry point
// ────────────────────────────────────────────────────────────────────────────

/// Run a full autopsy on a JSONL transcript file.
pub fn run_autopsy(jsonl_path: &Path) -> Result<AutopsyReport, String> {
    let walker = TranscriptWalker::from_file(jsonl_path)?;

    let mut findings = Vec::new();
    findings.extend(detect_error_cascades(&walker));
    findings.extend(detect_wasted_reads(&walker));
    findings.extend(detect_undo_redo_loops(&walker));

    // Sort by severity descending
    findings.sort_by_key(|f| std::cmp::Reverse(f.severity));

    let quality = compute_quality_score(&walker);
    let total_errors = walker.tool_calls.iter().filter(|c| c.is_error).count() as u32;

    // Derive session_id from filename
    let session_id = jsonl_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let generated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(AutopsyReport {
        session_id,
        project: String::new(), // Filled by caller from directory context
        model: walker.model,
        duration_secs: walker.duration_secs,
        total_messages: walker.message_count,
        total_tool_calls: walker.tool_calls.len() as u32,
        total_errors,
        quality,
        findings,
        generated_at,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Formatting
// ────────────────────────────────────────────────────────────────────────────

pub fn format_report(report: &AutopsyReport) -> String {
    let mut out = Vec::new();

    out.push(format!("Session Autopsy: {}", report.session_id));
    out.push("\u{2500}".repeat(50));
    out.push(String::new());

    // Summary
    out.push(format!(
        "  Messages: {}  |  Tool calls: {}  |  Errors: {}",
        report.total_messages, report.total_tool_calls, report.total_errors
    ));
    if !report.model.is_empty() {
        out.push(format!("  Model: {}", report.model));
    }
    out.push(String::new());

    // Quality
    let q = &report.quality;
    let test_status = match q.tests_passed {
        Some(true) => "passed",
        Some(false) => "FAILED",
        None => "not run",
    };
    out.push(format!("  Quality Score: {}/100", q.overall));
    out.push(format!(
        "    Tests: {}  |  Lint: {}  |  Edit efficiency: {}%",
        test_status,
        if q.ran_lint { "ran" } else { "not run" },
        q.edit_efficiency,
    ));
    out.push(String::new());

    // Findings
    if report.findings.is_empty() {
        out.push("  No issues detected.".to_string());
    } else {
        out.push(format!("  Findings ({})", report.findings.len()));
        out.push("  \u{2500}".to_string() + &"\u{2500}".repeat(40));
        for (i, f) in report.findings.iter().enumerate() {
            let severity_marker = match f.severity {
                FindingSeverity::Critical => "!!",
                FindingSeverity::Warning => " !",
                FindingSeverity::Info => "  ",
            };
            out.push(format!(
                "  {severity_marker} {}. [{}] {}",
                i + 1,
                f.category.label(),
                f.summary
            ));
            if let Some(ref detail) = f.detail {
                out.push(format!("       {detail}"));
            }
        }
    }

    out.push(String::new());
    out.join("\n")
}

pub fn report_to_json(report: &AutopsyReport) -> serde_json::Value {
    serde_json::json!({
        "session_id": report.session_id,
        "project": report.project,
        "model": report.model,
        "duration_secs": report.duration_secs,
        "total_messages": report.total_messages,
        "total_tool_calls": report.total_tool_calls,
        "total_errors": report.total_errors,
        "quality": {
            "overall": report.quality.overall,
            "ran_tests": report.quality.ran_tests,
            "tests_passed": report.quality.tests_passed,
            "ran_lint": report.quality.ran_lint,
            "edit_efficiency": report.quality.edit_efficiency,
        },
        "findings": report.findings.iter().map(|f| {
            serde_json::json!({
                "category": f.category.label(),
                "severity": f.severity.label(),
                "summary": f.summary,
                "detail": f.detail,
                "message_range": [f.message_range.0, f.message_range.1],
            })
        }).collect::<Vec<_>>(),
        "generated_at": report.generated_at,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Storage
// ────────────────────────────────────────────────────────────────────────────

fn autopsies_dir() -> PathBuf {
    super::decisions::decisions_dir().join("autopsies")
}

/// Save an autopsy report under the Coding Brain state root.
pub fn save_report(report: &AutopsyReport) -> Result<PathBuf, String> {
    let dir = autopsies_dir();
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.json", report.session_id));
    let json = serde_json::to_string_pretty(&report_to_json(report)).map_err(|e| format!("{e}"))?;
    fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn shorten_path(path: &str) -> &str {
    // Show just the filename or last two components
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    if parts.len() >= 2 {
        // "src/main.rs" from "/Users/foo/project/src/main.rs"
        let start = path.len() - parts[0].len() - 1 - parts[1].len();
        &path[start..]
    } else {
        path
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_assistant_with_tool_use(tool: &str, input_json: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","model":"gpt-5.4","stop_reason":"tool_use","content":[{{"type":"tool_use","name":"{tool}","input":{input_json}}}]}}}}"#
        )
    }

    fn make_user_with_result(is_error: bool, content: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","is_error":{is_error},"content":"{content}"}}]}}}}"#
        )
    }

    fn make_user_with_result_and_tool(
        is_error: bool,
        content: &str,
        tool: &str,
        input_json: &str,
    ) -> String {
        // A user message that contains a tool result followed by an assistant tool use
        // This doesn't happen in practice — tool uses are always in assistant messages.
        // Use make_assistant_with_tool_use + make_user_with_result in sequence instead.
        let _ = (tool, input_json);
        make_user_with_result(is_error, content)
    }

    #[test]
    fn walker_parses_basic_transcript() {
        let lines = [
            make_assistant_with_tool_use("Bash", r#"{"command":"ls"}"#),
            make_user_with_result(false, "file1.rs file2.rs"),
            make_assistant_with_tool_use("Read", r#"{"file_path":"/src/main.rs"}"#),
            make_user_with_result(false, "fn main() {}"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();

        assert_eq!(walker.message_count, 4);
        assert_eq!(walker.tool_calls.len(), 2);
        assert!(!walker.tool_calls[0].is_error);
        assert_eq!(walker.tool_calls[0].tool_name, "Bash");
        assert_eq!(walker.tool_calls[1].tool_name, "Read");
        assert!(walker.read_history.contains_key("/src/main.rs"));
    }

    #[test]
    fn detect_error_cascade_basic() {
        let lines = [
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "error: compilation failed"),
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "error: compilation failed"),
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "error: compilation failed"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let findings = detect_error_cascades(&walker);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::ErrorCascade);
        assert!(findings[0].summary.contains("3 consecutive errors"));
    }

    #[test]
    fn no_cascade_on_single_error() {
        let lines = [
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "error"),
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo test"}"#),
            make_user_with_result(false, "ok"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let findings = detect_error_cascades(&walker);
        assert!(findings.is_empty());
    }

    #[test]
    fn detect_wasted_reads_basic() {
        let mut lines = Vec::new();
        for _ in 0..4 {
            lines.push(make_assistant_with_tool_use(
                "Read",
                r#"{"file_path":"/src/lib.rs"}"#,
            ));
            lines.push(make_user_with_result(false, "contents"));
        }
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let findings = detect_wasted_reads(&walker);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::WastedReads);
        assert!(findings[0].summary.contains("read"));
    }

    #[test]
    fn reads_before_edit_not_flagged() {
        let lines = [
            make_assistant_with_tool_use("Read", r#"{"file_path":"/src/lib.rs"}"#),
            make_user_with_result(false, "old content"),
            make_assistant_with_tool_use(
                "Edit",
                r#"{"file_path":"/src/lib.rs","old_string":"old","new_string":"new"}"#,
            ),
            make_user_with_result(false, "ok"),
            make_assistant_with_tool_use("Read", r#"{"file_path":"/src/lib.rs"}"#),
            make_user_with_result(false, "new content"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let findings = detect_wasted_reads(&walker);
        assert!(findings.is_empty());
    }

    #[test]
    fn detect_undo_redo_basic() {
        let lines = [
            // Edit 1
            make_assistant_with_tool_use(
                "Edit",
                r#"{"file_path":"/src/main.rs","old_string":"a","new_string":"b"}"#,
            ),
            make_user_with_result(false, "ok"),
            // Error
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "compilation error"),
            // Edit 2 (fix attempt)
            make_assistant_with_tool_use(
                "Edit",
                r#"{"file_path":"/src/main.rs","old_string":"b","new_string":"c"}"#,
            ),
            make_user_with_result(false, "ok"),
            // Error again
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo build"}"#),
            make_user_with_result(true, "another error"),
            // Edit 3 (another fix)
            make_assistant_with_tool_use(
                "Edit",
                r#"{"file_path":"/src/main.rs","old_string":"c","new_string":"d"}"#,
            ),
            make_user_with_result(false, "ok"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let findings = detect_undo_redo_loops(&walker);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, FindingCategory::UndoRedoLoop);
    }

    #[test]
    fn quality_score_with_passing_tests() {
        let lines = [
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo test"}"#),
            make_user_with_result(false, "test result: ok. 10 passed"),
            make_assistant_with_tool_use("Bash", r#"{"command":"cargo clippy"}"#),
            make_user_with_result(false, "no warnings"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let quality = compute_quality_score(&walker);

        assert!(quality.ran_tests);
        assert_eq!(quality.tests_passed, Some(true));
        assert!(quality.ran_lint);
        assert!(quality.overall >= 75);
    }

    #[test]
    fn quality_score_no_tests() {
        let lines = [
            make_assistant_with_tool_use(
                "Edit",
                r#"{"file_path":"/a.rs","old_string":"x","new_string":"y"}"#,
            ),
            make_user_with_result(false, "ok"),
        ];
        let joined = lines.join("\n");
        let walker = TranscriptWalker::from_lines(joined.lines()).unwrap();
        let quality = compute_quality_score(&walker);

        assert!(!quality.ran_tests);
        assert_eq!(quality.tests_passed, None);
        assert!(!quality.ran_lint);
    }

    #[test]
    fn format_report_contains_sections() {
        let report = AutopsyReport {
            session_id: "test-session".into(),
            project: "test-proj".into(),
            model: "gpt-5.4".into(),
            duration_secs: 120,
            total_messages: 10,
            total_tool_calls: 5,
            total_errors: 2,
            quality: QualityScore {
                overall: 65,
                ran_tests: true,
                tests_passed: Some(true),
                ran_lint: false,
                edit_efficiency: 80,
            },
            findings: vec![AutopsyFinding {
                category: FindingCategory::ErrorCascade,
                severity: FindingSeverity::Warning,
                summary: "3 consecutive Bash errors".into(),
                detail: Some("first error: compilation failed".into()),
                message_range: (2, 7),
            }],
            generated_at: 0,
        };

        let output = format_report(&report);
        assert!(output.contains("Session Autopsy: test-session"));
        assert!(output.contains("Quality Score: 65/100"));
        assert!(output.contains("Error Cascade"));
        assert!(output.contains("3 consecutive Bash errors"));
        let forbidden: Vec<String> = serde_json::from_str(include_str!(
            "../../tests/fixtures/legacy-forbidden-output-keys.json"
        ))
        .unwrap();
        for key in forbidden {
            assert!(
                !output.contains(&key),
                "autopsy text retained forbidden output key {key}"
            );
        }
        assert!(!output.contains("Cost Efficiency"));
        assert!(!output.contains("tokens wasted"));
    }

    #[test]
    fn report_to_json_roundtrip() {
        let report = AutopsyReport {
            session_id: "test".into(),
            project: "proj".into(),
            model: "gpt-5.4".into(),
            duration_secs: 60,
            total_messages: 5,
            total_tool_calls: 3,
            total_errors: 1,
            quality: QualityScore {
                overall: 50,
                ran_tests: false,
                tests_passed: None,
                ran_lint: false,
                edit_efficiency: 100,
            },
            findings: vec![],
            generated_at: 1234567890,
        };

        let json = report_to_json(&report);
        assert_eq!(json["session_id"].as_str().unwrap(), "test");
        assert_eq!(json["total_messages"].as_u64().unwrap(), 5);
        assert_eq!(json["quality"]["overall"].as_u64().unwrap(), 50);
        assert!(json.get("cost").is_none());
        let encoded = serde_json::to_string(&json).unwrap();
        let forbidden: Vec<String> = serde_json::from_str(include_str!(
            "../../tests/fixtures/legacy-forbidden-output-keys.json"
        ))
        .unwrap();
        for key in forbidden {
            assert!(
                !encoded.contains(&key),
                "autopsy JSON retained forbidden output key {key}"
            );
        }
        assert!(!encoded.contains("tokens_wasted"));
        assert!(!encoded.contains("total_tokens"));
        assert!(json["findings"].as_array().unwrap().is_empty());
    }

    #[test]
    fn shorten_path_works() {
        assert_eq!(
            shorten_path("/Users/foo/project/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(shorten_path("main.rs"), "main.rs");
    }

    #[test]
    fn walker_handles_empty_input() {
        let walker = TranscriptWalker::from_lines(std::iter::empty()).unwrap();
        assert_eq!(walker.message_count, 0);
        assert!(walker.tool_calls.is_empty());
    }
}
