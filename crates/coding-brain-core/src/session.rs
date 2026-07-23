use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::lifecycle::{LifecycleDiagnostic, LifecycleEvidence, TranscriptEvidence};
use crate::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};
use crate::terminals::Terminal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SessionStatus {
    NeedsInput,   // Blocked — waiting for user to approve/confirm (permission prompt)
    Processing,   // Actively generating or executing tools
    WaitingInput, // Done responding, waiting for user's next prompt
    Unknown,      // Process is alive, but transcript telemetry is unavailable
    Idle,         // No recent activity, stale session
    Finished,     // Process exited
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionIdentityProvenance {
    #[default]
    Unknown,
    Structured,
    ProcessOnly,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedsInput => write!(f, "Needs Input"),
            Self::Processing => write!(f, "Processing"),
            Self::WaitingInput => write!(f, "Waiting"),
            Self::Unknown => write!(f, "Unknown"),
            Self::Idle => write!(f, "Idle"),
            Self::Finished => write!(f, "Finished"),
        }
    }
}

impl SessionStatus {
    pub fn sort_key(&self) -> u8 {
        match self {
            Self::NeedsInput => 0,
            Self::Processing => 1,
            Self::WaitingInput => 2,
            Self::Unknown => 3,
            Self::Idle => 4,
            Self::Finished => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTaskState {
    #[default]
    Unknown,
    Processing,
    WaitingInput,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEvidence {
    pub session_id: String,
    pub tty: String,
    pub call_id: String,
    pub tool: String,
    pub command: String,
    pub backend: Terminal,
    pub target: String,
    pub prompt_pattern_version: u16,
    pub prompt_fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApprovalObservation {
    #[default]
    NotChecked,
    Confirmed(ApprovalEvidence),
    Unknown(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryStatus {
    Pending,
    Available,
    MissingTranscript,
    UnreadableTranscript,
    UnsupportedTranscript,
}

impl TelemetryStatus {
    pub fn is_available(self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Available => "Available",
            Self::MissingTranscript => "No transcript",
            Self::UnreadableTranscript => "Unreadable transcript",
            Self::UnsupportedTranscript => "Unsupported transcript",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Available => "Available",
            Self::MissingTranscript => "No transcript",
            Self::UnreadableTranscript => "Unreadable",
            Self::UnsupportedTranscript => "Unsupported",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RawAgentSession {
    #[serde(default)]
    pub provider: AgentProvider,
    pub pid: u32,
    #[serde(default, rename = "processStartIdentity")]
    pub process_start_identity: Option<u64>,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
}

#[derive(Debug, Clone)]
pub struct AgentSession {
    pub provider: AgentProvider,
    pub pid: u32,
    pub process_start_identity: Option<u64>,
    pub process_backed: bool,
    pub identity_provenance: SessionIdentityProvenance,
    #[allow(dead_code)]
    pub session_id: String,
    /// Provider-native attachment evidence, when discovery supplies it.
    pub native_attach_id: Option<String>,
    pub cwd: String,
    pub project_name: String,
    pub started_at: u64,
    pub elapsed: Duration,
    pub tty: String,
    pub status: SessionStatus,
    pub cpu_percent: f32,
    pub cpu_history: Vec<f32>, // Last N CPU readings for smoothing
    pub mem_mb: f64,
    pub own_input_tokens: u64,
    pub own_output_tokens: u64,
    pub own_cache_read_tokens: u64,
    pub own_cache_write_tokens: u64,
    pub subagent_input_tokens: u64,
    pub subagent_output_tokens: u64,
    pub subagent_cache_read_tokens: u64,
    pub subagent_cache_write_tokens: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub model: String,
    pub command_args: String,
    pub session_name: String,
    pub jsonl_path: Option<PathBuf>,
    pub jsonl_offset: u64,
    pub last_message_ts: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub own_cost_usd: f64,
    pub priced_total_tokens: u64,
    pub cost_ledger_frozen: bool,
    pub context_tokens: u64,
    pub context_max: u64,
    pub prev_cost_usd: f64,
    pub burn_rate_per_hr: f64,
    pub subagent_count: usize,
    pub active_subagent_count: usize,
    pub active_subagent_jsonl_paths: Vec<PathBuf>,
    pub subagent_rollups: HashMap<PathBuf, SubagentRollup>,
    pub activity_history: Vec<u8>, // Ring buffer of status levels (0-7) for sparkline, one per tick
    pub files_modified: HashMap<String, u32>, // file path -> edit count
    pub tool_usage: HashMap<String, ToolStats>, // tool name -> call count & tokens
    pub worktree_id: Option<String>, // Resolved git toplevel + git-dir, for conflict detection
    pub telemetry_status: TelemetryStatus,
    pub usage_metrics_available: bool,
    pub cost_estimate_unverified: bool,
    pub model_profile_source: String,
    /// Persisted across ticks so status inference works when no new JSONL arrives.
    pub last_msg_type: String,
    pub last_stop_reason: String,
    pub is_waiting_for_task: bool,
    pub task_state: CodexTaskState,
    pub transcript_evidence: Option<TranscriptEvidence>,
    pub lifecycle_evidence: Option<LifecycleEvidence>,
    pub lifecycle_diagnostic: LifecycleDiagnostic,
    pub explicit_input_required: bool,
    pub approval: ApprovalObservation,
    pub approval_checked_at_ms: u64,
    /// Pending tool call details for rule-based auto-actions.
    pub pending_tool_name: Option<String>,
    pub pending_tool_call_id: Option<String>,
    pub pending_tool_input: Option<String>, // Extracted command string (for Bash)
    pub pending_file_path: Option<String>,  // File path for pending Edit/Write/NotebookEdit
    pub has_file_conflict: bool,            // Pending file edit conflicts with another session
    pub last_tool_error: bool,
    pub last_error_message: Option<String>,
    pub recent_errors: Vec<ErrorEntry>, // Last 5 errors (ring buffer)
    // ── Cognitive health tracking ────────────────────────────────────
    /// Cumulative tokens at each Edit/Write event (for efficiency trending).
    pub total_tokens_at_edit_count: u64,
    /// Number of Edit/Write events (for averaging tokens-per-edit).
    pub edit_event_count: u32,
    /// Baseline tokens-per-edit, frozen after first 5 edits.
    pub baseline_tokens_per_edit: Option<f64>,
    /// Error count ring buffer: one entry per window (~10s each).
    pub error_counts_per_window: Vec<u32>, // max 10 entries
    /// Accumulator for current error window.
    pub current_window_errors: u32,
    /// Ticks since last window flush.
    pub window_tick_counter: u32,
    /// Baseline error rate (errors per window), frozen after 3 windows.
    pub baseline_error_rate: Option<f64>,
    /// File reads since last edit: path -> read count. Reset when file is edited.
    pub file_reads_since_edit: HashMap<String, u32>,
    /// All-time error count.
    pub total_error_count: u32,
    /// Cached composite decay score (0-100), recomputed each tick.
    pub decay_score: u32,
    /// If set, this session is from a remote worker (not local).
    /// Terminal actions (approve, kill, etc.) are disabled for remote sessions.
    pub worker_origin: Option<String>,
}

/// A captured tool error with context.
#[derive(Debug, Clone)]
pub struct ErrorEntry {
    pub tool_name: String,
    pub message: String,
}

/// Per-tool usage statistics.
#[derive(Debug, Clone, Default)]
pub struct ToolStats {
    pub calls: u32,
}

#[derive(Debug, Clone, Default)]
pub struct SubagentRollup {
    pub jsonl_offset: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub cost_ledger_frozen: bool,
    pub model: String,
    pub cost_estimate_unverified: bool,
    pub usage_metrics_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Active,
    Completed,
}

#[derive(Debug, Clone)]
pub struct SubagentBreakdown {
    pub label: String,
    pub state: SubagentState,
    pub count: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub usage_metrics_available: bool,
    pub cost_estimate_unverified: bool,
}

impl SubagentBreakdown {
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn state_label(&self) -> String {
        match self.state {
            SubagentState::Active => "Active".to_string(),
            SubagentState::Completed if self.count > 1 => format!("Completed ({})", self.count),
            SubagentState::Completed => "Completed".to_string(),
        }
    }

    pub fn display_label(&self) -> String {
        if self.state == SubagentState::Completed && self.label == "completed" && self.count > 1 {
            format!("completed ({})", self.count)
        } else {
            self.label.clone()
        }
    }

    pub fn format_tokens(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let total = self.total_input_tokens() + self.output_tokens;
        if total == 0 {
            return "-".to_string();
        }
        format_count(self.total_input_tokens()) + "/" + &format_count(self.output_tokens)
    }

    pub fn format_cost(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.cost_usd < 0.01 {
            return "-".to_string();
        }
        if self.cost_usd < 1.0 {
            format!(
                "${:.2}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }
}

impl AgentSession {
    pub fn from_raw(raw: RawAgentSession) -> Self {
        let project_name = raw.cwd.rsplit('/').next().unwrap_or("unknown").to_string();

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let elapsed_ms = now_ms.saturating_sub(raw.started_at);
        let elapsed = Duration::from_millis(elapsed_ms);

        Self {
            provider: raw.provider,
            pid: raw.pid,
            process_start_identity: raw.process_start_identity,
            process_backed: true,
            identity_provenance: SessionIdentityProvenance::Unknown,
            session_id: raw.session_id,
            native_attach_id: None,
            cwd: raw.cwd,
            project_name,
            started_at: raw.started_at,
            elapsed,
            tty: String::new(),
            status: SessionStatus::Idle,
            cpu_percent: 0.0,
            cpu_history: Vec::new(),
            mem_mb: 0.0,
            own_input_tokens: 0,
            own_output_tokens: 0,
            own_cache_read_tokens: 0,
            own_cache_write_tokens: 0,
            subagent_input_tokens: 0,
            subagent_output_tokens: 0,
            subagent_cache_read_tokens: 0,
            subagent_cache_write_tokens: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            model: String::new(),
            command_args: String::new(),
            session_name: String::new(),
            jsonl_path: None,
            jsonl_offset: 0,
            last_message_ts: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 0.0,
            own_cost_usd: 0.0,
            priced_total_tokens: 0,
            cost_ledger_frozen: false,
            context_tokens: 0,
            context_max: 0,
            prev_cost_usd: 0.0,
            burn_rate_per_hr: 0.0,
            subagent_count: 0,
            active_subagent_count: 0,
            active_subagent_jsonl_paths: Vec::new(),
            subagent_rollups: HashMap::new(),
            activity_history: Vec::new(),
            files_modified: HashMap::new(),
            tool_usage: HashMap::new(),
            worktree_id: None,
            telemetry_status: TelemetryStatus::Pending,
            usage_metrics_available: false,
            cost_estimate_unverified: false,
            model_profile_source: "built-in".into(),
            last_msg_type: String::new(),
            last_stop_reason: String::new(),
            is_waiting_for_task: false,
            task_state: CodexTaskState::Unknown,
            transcript_evidence: None,
            lifecycle_evidence: None,
            lifecycle_diagnostic: LifecycleDiagnostic::default(),
            explicit_input_required: false,
            approval: ApprovalObservation::NotChecked,
            approval_checked_at_ms: 0,
            pending_tool_name: None,
            pending_tool_call_id: None,
            pending_tool_input: None,
            pending_file_path: None,
            has_file_conflict: false,
            last_tool_error: false,
            last_error_message: None,
            recent_errors: Vec::new(),
            total_tokens_at_edit_count: 0,
            edit_event_count: 0,
            baseline_tokens_per_edit: None,
            error_counts_per_window: Vec::new(),
            current_window_errors: 0,
            window_tick_counter: 0,
            baseline_error_rate: None,
            file_reads_since_edit: HashMap::new(),
            total_error_count: 0,
            decay_score: 0,
            worker_origin: None,
        }
    }

    pub fn key(&self) -> AgentSessionKey {
        AgentSessionKey::native(self.provider, &self.session_id)
    }

    pub fn live_process_identity(&self) -> Option<LiveProcessIdentity> {
        if !self.process_backed || self.is_remote() {
            return None;
        }
        LiveProcessIdentity::try_new(
            self.provider,
            self.pid,
            self.process_start_identity?,
            &self.tty,
        )
    }

    pub fn supports_structured_discovery(&self) -> bool {
        self.provider.supports_structured_discovery()
    }

    pub fn has_lifecycle_evidence(&self) -> bool {
        self.lifecycle_evidence.is_some()
    }

    pub fn has_transcript_context(&self) -> bool {
        self.transcript_evidence.is_some()
    }

    pub fn has_permission_observation(&self) -> bool {
        self.explicit_input_required
            || matches!(self.approval, ApprovalObservation::Confirmed(_))
            || self.lifecycle_evidence.is_some_and(|evidence| {
                evidence.status_event == crate::lifecycle::LifecycleEventName::PermissionRequest
            })
    }

    pub fn supports_executable_permission_response(&self) -> bool {
        self.has_permission_observation()
            && (self.lifecycle_evidence.is_some_and(|evidence| {
                matches!(
                    evidence.status_event,
                    crate::lifecycle::LifecycleEventName::PermissionRequest
                        | crate::lifecycle::LifecycleEventName::PreToolUse
                )
            }) || self.live_process_identity().is_some())
    }

    pub fn has_outcome_evidence(&self) -> bool {
        self.lifecycle_evidence.is_some_and(|evidence| {
            matches!(
                evidence.latest_event,
                crate::lifecycle::LifecycleEventName::PostToolUse
                    | crate::lifecycle::LifecycleEventName::Stop
            )
        }) || self.transcript_evidence.is_some_and(|evidence| {
            matches!(
                evidence.semantic,
                crate::lifecycle::TranscriptSemantic::Complete
            )
        })
    }

    pub fn supports_native_attach(&self) -> bool {
        self.provider.supports_native_attach()
    }

    pub fn supports_terminal_focus_fallback(&self) -> bool {
        self.live_process_identity().is_some()
    }

    pub fn supports_guarded_terminal_input(&self) -> bool {
        self.supports_terminal_focus_fallback() && self.has_permission_observation()
    }

    pub fn supports_guarded_recovery_response(&self) -> bool {
        self.supports_terminal_focus_fallback() && self.has_outcome_evidence()
    }

    /// Tool identity currently presented to consumers.
    ///
    /// This is a projection, not approval authorization. Guarded input still
    /// requires terminal-confirmed evidence and final revalidation.
    pub fn actionable_tool_name(&self) -> Option<&str> {
        match &self.approval {
            ApprovalObservation::Confirmed(evidence) => Some(evidence.tool.as_str()),
            ApprovalObservation::NotChecked | ApprovalObservation::Unknown(_) => {
                self.pending_tool_name.as_deref()
            }
        }
    }

    /// Tool input currently presented to consumers.
    ///
    /// This is a projection, not approval authorization. Guarded input still
    /// requires terminal-confirmed evidence and final revalidation.
    pub fn actionable_tool_input(&self) -> Option<&str> {
        match &self.approval {
            ApprovalObservation::Confirmed(evidence) => Some(evidence.command.as_str()),
            ApprovalObservation::NotChecked | ApprovalObservation::Unknown(_) => {
                self.pending_tool_input.as_deref()
            }
        }
    }

    /// Whether the pending tool call is a shell permission request.
    ///
    /// This classification is intentionally independent of terminal capture:
    /// capture decides whether guarded input is authorized, while this method
    /// decides which policy path owns the request.
    pub fn is_shell_permission_request(&self) -> bool {
        if self.pending_tool_call_id.is_none() {
            return false;
        }

        let direct_shell = |tool: &str| matches!(tool, "Bash" | "exec_command" | "shell");
        if self.actionable_tool_name().is_some_and(direct_shell) {
            return true;
        }

        self.pending_tool_name.as_deref() == Some("exec")
            && self
                .pending_tool_input
                .as_deref()
                .is_some_and(|input| input.contains("tools.exec_command("))
    }

    pub fn from_codex_transcript(
        session_id: String,
        cwd: String,
        started_at: u64,
        jsonl_path: PathBuf,
    ) -> Self {
        let pid = stable_synthetic_pid(&session_id);
        let mut session = Self::from_raw(RawAgentSession {
            provider: AgentProvider::Codex,
            pid,
            process_start_identity: None,
            session_id,
            cwd,
            started_at,
        });
        session.process_backed = false;
        session.identity_provenance = SessionIdentityProvenance::Structured;
        session.jsonl_path = Some(jsonl_path);
        session.telemetry_status = TelemetryStatus::Pending;
        session.model_profile_source = "codex-transcript".into();
        session
    }

    /// Record current status into the activity sparkline ring buffer.
    /// Max 15 entries (one per tick, at 2s default = 30s of history).
    pub fn record_activity(&mut self) {
        let level = match self.status {
            SessionStatus::Processing => 7,
            SessionStatus::NeedsInput => 4,
            SessionStatus::WaitingInput => 2,
            SessionStatus::Unknown => 2,
            SessionStatus::Idle => 1,
            SessionStatus::Finished => 0,
        };
        self.activity_history.push(level);
        if self.activity_history.len() > 15 {
            self.activity_history.remove(0);
        }

        // Flush error window every 5 ticks (~10s at default 2s interval)
        self.window_tick_counter += 1;
        if self.window_tick_counter >= 5 {
            self.error_counts_per_window
                .push(self.current_window_errors);
            if self.error_counts_per_window.len() > 10 {
                self.error_counts_per_window.remove(0);
            }
            // Freeze baseline error rate after 3 windows
            if self.baseline_error_rate.is_none() && self.error_counts_per_window.len() >= 3 {
                let sum: u32 = self.error_counts_per_window.iter().sum();
                self.baseline_error_rate =
                    Some(sum as f64 / self.error_counts_per_window.len() as f64);
            }
            self.current_window_errors = 0;
            self.window_tick_counter = 0;
        }
    }

    /// Render the sparkline as unicode block characters.
    pub fn format_sparkline(&self) -> String {
        const BLOCKS: &[char] = &[
            ' ', '\u{2581}', '\u{2582}', '\u{2583}', '\u{2584}', '\u{2585}', '\u{2586}',
            '\u{2587}', '\u{2588}',
        ];
        if self.activity_history.is_empty() {
            return String::from("-");
        }
        self.activity_history
            .iter()
            .map(|&level| BLOCKS[level.min(8) as usize])
            .collect()
    }

    pub fn display_name(&self) -> &str {
        if !self.session_name.is_empty() {
            &self.session_name
        } else {
            &self.project_name
        }
    }

    /// Whether this session is from a remote worker (not local).
    pub fn is_remote(&self) -> bool {
        self.worker_origin.is_some()
    }

    /// Build a AgentSession from remote JSON (as received via heartbeat/HTTP).
    #[allow(dead_code)]
    pub fn from_remote_json(worker_id: &str, json: &serde_json::Value) -> Option<Self> {
        let pid = json.get("pid")?.as_u64()? as u32;
        let project = json.get("project")?.as_str()?;
        let status_str = json
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        let status = match status_str {
            "Needs Input" => SessionStatus::NeedsInput,
            "Processing" => SessionStatus::Processing,
            "Waiting" => SessionStatus::WaitingInput,
            "Idle" => SessionStatus::Idle,
            "Finished" => SessionStatus::Finished,
            _ => SessionStatus::Unknown,
        };

        let elapsed_secs = json
            .get("elapsed_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut session = Self::from_raw(RawAgentSession {
            provider: AgentProvider::Codex,
            pid,
            process_start_identity: None,
            session_id: format!("remote-{worker_id}-{pid}"),
            cwd: project.to_string(),
            started_at: now_ms.saturating_sub(elapsed_secs * 1000),
        });
        session.status = status;
        session.worker_origin = Some(worker_id.to_string());
        session.project_name = format!("[{worker_id}] {project}");

        // Populate metrics from JSON when available
        if let Some(cost) = json.get("cost_usd").and_then(|v| v.as_f64()) {
            session.cost_usd = cost;
            session.usage_metrics_available = true;
        }
        if let Some(burn) = json.get("burn_rate_per_hr").and_then(|v| v.as_f64()) {
            session.burn_rate_per_hr = burn;
        }
        if let Some(ctx) = json.get("context_pct").and_then(|v| v.as_f64()) {
            // Reverse-engineer context_tokens/context_max from percentage
            session.context_max = 200_000; // reasonable default
            session.context_tokens = ((ctx / 100.0) * session.context_max as f64) as u64;
        }
        if let Some(t_in) = json.get("tokens_in").and_then(|v| v.as_u64()) {
            session.total_input_tokens = t_in;
            session.usage_metrics_available = true;
        }
        if let Some(t_out) = json.get("tokens_out").and_then(|v| v.as_u64()) {
            session.total_output_tokens = t_out;
        }
        if let Some(model) = json.get("model").and_then(|v| v.as_str()) {
            session.model = model.to_string();
        }
        if let Some(subs) = json.get("subagents").and_then(|v| v.as_u64()) {
            session.subagent_count = subs as usize;
        }
        if let Some(decay) = json.get("decay_score").and_then(|v| v.as_u64()) {
            session.decay_score = decay as u32;
        }

        Some(session)
    }

    pub fn format_subagent_summary(&self) -> String {
        if self.subagent_count == 0 {
            return "0".to_string();
        }
        if self.active_subagent_count == 0 || self.active_subagent_count == self.subagent_count {
            return self.subagent_count.to_string();
        }
        format!(
            "{} total ({} active)",
            self.subagent_count, self.active_subagent_count
        )
    }

    pub fn subagent_breakdown(&self) -> Vec<SubagentBreakdown> {
        if self.subagent_rollups.is_empty() {
            return Vec::new();
        }

        let active_paths: HashSet<&PathBuf> = self.active_subagent_jsonl_paths.iter().collect();
        let mut active_rows = Vec::new();
        let mut completed_rows = Vec::new();

        for (path, rollup) in &self.subagent_rollups {
            let row = SubagentBreakdown {
                label: subagent_label(path),
                state: if active_paths.contains(path) {
                    SubagentState::Active
                } else {
                    SubagentState::Completed
                },
                count: 1,
                input_tokens: rollup.input_tokens,
                output_tokens: rollup.output_tokens,
                cache_read_tokens: rollup.cache_read_tokens,
                cache_write_tokens: rollup.cache_write_tokens,
                cost_usd: rollup.cost_usd,
                usage_metrics_available: rollup.usage_metrics_available,
                cost_estimate_unverified: rollup.cost_estimate_unverified,
            };

            if row.state == SubagentState::Active {
                active_rows.push(row);
            } else {
                completed_rows.push(row);
            }
        }

        active_rows.sort_by(|a, b| a.label.cmp(&b.label));

        let mut rows = Vec::new();
        if !completed_rows.is_empty() {
            let mut aggregate = SubagentBreakdown {
                label: "completed".to_string(),
                state: SubagentState::Completed,
                count: completed_rows.len(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                cost_usd: 0.0,
                usage_metrics_available: false,
                cost_estimate_unverified: false,
            };

            for row in completed_rows {
                aggregate.input_tokens += row.input_tokens;
                aggregate.output_tokens += row.output_tokens;
                aggregate.cache_read_tokens += row.cache_read_tokens;
                aggregate.cache_write_tokens += row.cache_write_tokens;
                aggregate.cost_usd += row.cost_usd;
                aggregate.usage_metrics_available |= row.usage_metrics_available;
                aggregate.cost_estimate_unverified |= row.cost_estimate_unverified;
            }

            rows.push(aggregate);
        }

        rows.extend(active_rows);
        rows
    }

    pub fn format_elapsed(&self) -> String {
        let secs = self.elapsed.as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        if h > 0 {
            format!("{h:02}:{m:02}:{s:02}")
        } else {
            format!("{m:02}:{s:02}")
        }
    }

    pub fn format_tokens(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let total = self.total_input_tokens + self.total_output_tokens;
        if total == 0 {
            return String::from("-");
        }
        format_count(self.total_input_tokens) + "/" + &format_count(self.total_output_tokens)
    }

    pub fn format_mem(&self) -> String {
        if self.mem_mb < 1.0 {
            return String::from("-");
        }
        format!("{:.0}M", self.mem_mb)
    }

    pub fn format_cost(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.cost_usd < 0.01 {
            return String::from("-");
        }
        if self.cost_usd < 1.0 {
            format!(
                "${:.2}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}{}",
                self.cost_usd,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }

    pub fn context_percent(&self) -> f64 {
        if !self.usage_metrics_available {
            return 0.0;
        }
        if self.context_max == 0 || self.context_tokens == 0 {
            return 0.0;
        }
        (self.context_tokens as f64 / self.context_max as f64) * 100.0
    }

    /// Format context as "450k/1M 45%" or a visual bar
    pub fn format_context(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.context_tokens == 0 {
            return String::from("-");
        }
        let pct = self.context_percent();
        format!("{}%", pct as u32)
    }

    /// Visual bar for context usage: ████░░ 62%
    pub fn format_context_bar(&self, width: usize) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        let pct = self.context_percent();
        if pct == 0.0 {
            return String::from("-");
        }
        let filled = ((pct / 100.0) * width as f64).round() as usize;
        let empty = width.saturating_sub(filled);
        format!(
            "{}{} {}%",
            "█".repeat(filled),
            "░".repeat(empty),
            pct as u32
        )
    }

    /// Produce a JSON-serializable value for --json export.
    pub fn to_json_value(&self) -> serde_json::Value {
        let cost_usd = if self.usage_metrics_available {
            serde_json::json!((self.cost_usd * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let burn_rate = if self.usage_metrics_available {
            serde_json::json!((self.burn_rate_per_hr * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let context_pct = if self.usage_metrics_available {
            serde_json::json!((self.context_percent() * 100.0).round() / 100.0)
        } else {
            serde_json::Value::Null
        };
        let tokens_in = if self.usage_metrics_available {
            serde_json::json!(self.total_input_tokens)
        } else {
            serde_json::Value::Null
        };
        let tokens_out = if self.usage_metrics_available {
            serde_json::json!(self.total_output_tokens)
        } else {
            serde_json::Value::Null
        };
        let lifecycle = &self.lifecycle_diagnostic;

        serde_json::json!({
            "pid": self.pid,
            "project": self.display_name(),
            "status": self.status.to_string(),
            "telemetry": {
                "state": self.telemetry_status.label(),
                "usage_metrics_available": self.usage_metrics_available,
            },
            "estimate": {
                "verified": !self.cost_estimate_unverified,
                "profile_source": self.model_profile_source,
            },
            "context_pct": context_pct,
            "cost_usd": cost_usd,
            "burn_rate_per_hr": burn_rate,
            "elapsed_secs": self.elapsed.as_secs(),
            "cpu": self.cpu_percent,
            "mem_mb": (self.mem_mb * 100.0).round() / 100.0,
            "tokens_in": tokens_in,
            "tokens_out": tokens_out,
            "subagents": self.subagent_count,
            "active_subagents": self.active_subagent_count,
            "subagent_breakdown": self.subagent_breakdown().into_iter().map(|row| {
                serde_json::json!({
                    "label": row.display_label(),
                    "state": row.state_label(),
                    "count": row.count,
                    "tokens_in": if row.usage_metrics_available {
                        serde_json::json!(row.total_input_tokens())
                    } else {
                        serde_json::Value::Null
                    },
                    "tokens_out": if row.usage_metrics_available {
                        serde_json::json!(row.output_tokens)
                    } else {
                        serde_json::Value::Null
                    },
                    "cost_usd": if row.usage_metrics_available {
                        serde_json::json!((row.cost_usd * 100.0).round() / 100.0)
                    } else {
                        serde_json::Value::Null
                    },
                })
            }).collect::<Vec<_>>(),
            "decay_score": if self.usage_metrics_available { serde_json::json!(self.decay_score) } else { serde_json::Value::Null },
            "last_error": self.last_error_message,
            "recent_errors": self.recent_errors.iter().map(|e| {
                serde_json::json!({
                    "tool": e.tool_name,
                    "message": e.message,
                })
            }).collect::<Vec<_>>(),
            "files_modified": self.files_modified,
            "tool_usage": self.tool_usage.iter().map(|(k, v)| {
                (k.clone(), serde_json::json!({"calls": v.calls}))
            }).collect::<serde_json::Map<String, serde_json::Value>>(),
            "lifecycle": {
                "available": lifecycle.available,
                "store_condition": lifecycle.store_condition.map(|condition| condition.as_str()),
                "last_event": lifecycle.event.map(|event| event.as_str()),
                "age_ms": lifecycle.age_ms,
                "contributing": lifecycle.contributing,
                "ignored_reason": lifecycle.ignored_reason,
            },
            "worker_origin": self.worker_origin,
        })
    }

    pub fn format_burn_rate(&self) -> String {
        if !self.usage_metrics_available {
            return "n/a".to_string();
        }
        if self.burn_rate_per_hr < 0.01 {
            return String::from("-");
        }
        if self.burn_rate_per_hr < 1.0 {
            format!(
                "${:.2}/h{}",
                self.burn_rate_per_hr,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        } else {
            format!(
                "${:.1}/h{}",
                self.burn_rate_per_hr,
                if self.cost_estimate_unverified {
                    "?"
                } else {
                    ""
                }
            )
        }
    }

    pub fn telemetry_label(&self) -> &'static str {
        self.telemetry_status.label()
    }

    pub fn has_usage_metrics(&self) -> bool {
        self.usage_metrics_available
    }
}

fn stable_synthetic_pid(session_id: &str) -> u32 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    let hash = hasher.finish();
    // Keep synthetic identifiers away from common tiny real PIDs.
    100_000 + (hash % 1_000_000) as u32
}

/// Truncate a string to at most `max_bytes` bytes, landing on a valid
/// UTF-8 character boundary. Returns the original string if already short enough.
pub fn truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn subagent_label(path: &Path) -> String {
    let components: Vec<String> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect();

    if let Some(tasks_idx) = components.iter().position(|component| component == "tasks") {
        let relative = &components[tasks_idx + 1..];
        if !relative.is_empty() {
            let mut label = relative.join("/");
            if let Some(stripped) = label.strip_suffix(".jsonl") {
                label = stripped.to_string();
            }
            return label;
        }
    }

    path.file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("subagent")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AgentProvider;

    fn make_session() -> AgentSession {
        AgentSession::from_raw(RawAgentSession {
            provider: AgentProvider::Codex,
            pid: 1,
            process_start_identity: None,
            session_id: "session-1".into(),
            cwd: "/tmp/project".into(),
            started_at: 0,
        })
    }

    #[test]
    fn session_key_includes_provider() {
        let mut session = make_session();
        session.provider = AgentProvider::Claude;

        assert_eq!(session.key().provider, AgentProvider::Claude);
        assert_eq!(session.key().session_id, "session-1");
    }

    #[test]
    fn legacy_raw_session_defaults_to_codex_without_process_start_evidence() {
        let raw: RawAgentSession =
            serde_json::from_str(r#"{"pid":7,"sessionId":"legacy","cwd":"/repo","startedAt":1}"#)
                .unwrap();

        assert_eq!(raw.provider, AgentProvider::Codex);
        assert_eq!(raw.process_start_identity, None);
        assert_eq!(
            AgentSession::from_raw(raw).identity_provenance,
            SessionIdentityProvenance::Unknown
        );
    }

    #[test]
    fn live_process_identity_requires_current_process_evidence() {
        let mut session = make_session();
        session.tty = "/dev/pts/4".into();
        assert_eq!(session.live_process_identity(), None);

        session.process_start_identity = Some(99);
        let identity = session.live_process_identity().unwrap();
        assert_eq!(identity.provider, AgentProvider::Codex);
        assert_eq!(identity.tty, "pts/4");
    }

    #[test]
    fn evidence_capabilities_follow_current_session_evidence() {
        let mut session = make_session();
        assert!(session.supports_structured_discovery());
        assert!(!session.has_lifecycle_evidence());
        assert!(!session.has_transcript_context());
        assert!(!session.has_permission_observation());
        assert!(!session.supports_executable_permission_response());
        assert!(!session.has_outcome_evidence());
        assert!(!session.supports_native_attach());
        assert!(!session.supports_terminal_focus_fallback());
        assert!(!session.supports_guarded_terminal_input());
        assert!(!session.supports_guarded_recovery_response());

        session.provider = AgentProvider::Antigravity;
        assert!(!session.supports_structured_discovery());
        session.provider = AgentProvider::Claude;
        assert!(session.supports_native_attach());
        session.process_start_identity = Some(99);
        session.tty = "/dev/pts/4".into();
        assert!(session.supports_terminal_focus_fallback());
        assert!(!session.supports_guarded_terminal_input());
        assert!(!session.supports_guarded_recovery_response());

        session.lifecycle_evidence = Some(LifecycleEvidence {
            projected_status: crate::lifecycle::ProjectedStatus::NeedsInput,
            status_event: crate::lifecycle::LifecycleEventName::PermissionRequest,
            status_received_at_ms: 1,
            latest_event: crate::lifecycle::LifecycleEventName::PostToolUse,
            latest_received_at_ms: 2,
            active_subagent_count: 0,
        });
        session.transcript_evidence = Some(TranscriptEvidence::complete(Some(2)));
        session.explicit_input_required = true;

        assert!(session.has_lifecycle_evidence());
        assert!(session.has_transcript_context());
        assert!(session.has_permission_observation());
        assert!(session.supports_executable_permission_response());
        assert!(session.has_outcome_evidence());
        assert!(session.supports_guarded_terminal_input());
        assert!(session.supports_guarded_recovery_response());
    }

    #[test]
    fn actionable_identity_prefers_confirmed_evidence() {
        let mut session = make_session();
        session.pending_tool_name = Some("exec".into());
        session.pending_tool_call_id = Some("call-1".into());
        session.pending_tool_input = Some("await tools.exec_command(args);".into());
        session.approval = ApprovalObservation::Confirmed(ApprovalEvidence {
            session_id: session.session_id.clone(),
            tty: session.tty.clone(),
            call_id: "call-1".into(),
            tool: "exec_command".into(),
            command: "install -m 664 source target".into(),
            backend: Terminal::Tmux,
            target: "main:1.0".into(),
            prompt_pattern_version: 1,
            prompt_fingerprint: 42,
        });

        assert_eq!(session.actionable_tool_name(), Some("exec_command"));
        assert_eq!(
            session.actionable_tool_input(),
            Some("install -m 664 source target")
        );
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(
            session.pending_tool_input.as_deref(),
            Some("await tools.exec_command(args);")
        );
    }

    #[test]
    fn non_confirmed_identity_falls_back_to_pending_call() {
        for approval in [
            ApprovalObservation::NotChecked,
            ApprovalObservation::Unknown("no matching prompt".into()),
        ] {
            let mut session = make_session();
            session.pending_tool_name = Some("exec".into());
            session.pending_tool_input = Some("await tools.exec_command(args);".into());
            session.approval = approval;

            assert_eq!(session.actionable_tool_name(), Some("exec"));
            assert_eq!(
                session.actionable_tool_input(),
                Some("await tools.exec_command(args);")
            );
        }
    }

    #[test]
    fn shell_permission_request_classifies_direct_tools_without_terminal_evidence() {
        for tool in ["Bash", "exec_command", "shell"] {
            let mut session = make_session();
            session.pending_tool_call_id = Some("call-1".into());
            session.pending_tool_name = Some(tool.into());
            session.pending_tool_input = Some("cargo test".into());

            assert!(session.is_shell_permission_request(), "tool={tool}");
        }
    }

    #[test]
    fn shell_permission_request_classifies_unknown_exec_wrapper() {
        let mut session = make_session();
        session.pending_tool_call_id = Some("call-1".into());
        session.pending_tool_name = Some("exec".into());
        session.pending_tool_input = Some("await tools.exec_command(args);".into());
        session.approval = ApprovalObservation::Unknown("capture unavailable".into());

        assert!(session.is_shell_permission_request());
    }

    #[test]
    fn shell_permission_request_rejects_non_shell_and_incomplete_calls() {
        let mut session = make_session();
        session.pending_tool_call_id = Some("call-1".into());
        session.pending_tool_name = Some("request_user_input".into());
        session.pending_tool_input = Some("question".into());
        assert!(!session.is_shell_permission_request());

        session.pending_tool_name = Some("exec".into());
        session.pending_tool_input = Some("text(true);".into());
        assert!(!session.is_shell_permission_request());

        session.pending_tool_name = Some("Bash".into());
        session.pending_tool_call_id = None;
        assert!(!session.is_shell_permission_request());
    }

    #[test]
    fn subagent_breakdown_groups_completed_and_lists_active_rows() {
        let mut session = make_session();
        let completed = PathBuf::from("/tmp/codex-1/-tmp-project/session-1/tasks/agent-1.jsonl");
        let active =
            PathBuf::from("/tmp/codex-1/-tmp-project/session-1/tasks/nested/agent-2.jsonl");

        session.active_subagent_jsonl_paths = vec![active.clone()];
        session.subagent_rollups.insert(
            completed,
            SubagentRollup {
                input_tokens: 10_000,
                output_tokens: 2_000,
                cost_usd: 0.25,
                usage_metrics_available: true,
                ..SubagentRollup::default()
            },
        );
        session.subagent_rollups.insert(
            active,
            SubagentRollup {
                input_tokens: 40_000,
                output_tokens: 8_000,
                cost_usd: 1.5,
                usage_metrics_available: true,
                ..SubagentRollup::default()
            },
        );

        let rows = session.subagent_breakdown();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].display_label(), "completed");
        assert_eq!(rows[0].state, SubagentState::Completed);
        assert_eq!(rows[0].count, 1);
        assert_eq!(rows[0].format_tokens(), "10.0k/2.0k");
        assert_eq!(rows[1].display_label(), "nested/agent-2");
        assert_eq!(rows[1].state, SubagentState::Active);
        assert_eq!(rows[1].format_cost(), "$1.5");
    }

    #[test]
    fn subagent_breakdown_collapses_multiple_completed_rows() {
        let mut session = make_session();

        for name in ["agent-1.jsonl", "agent-2.jsonl"] {
            let path = PathBuf::from(format!("/tmp/codex-1/-tmp-project/session-1/tasks/{name}"));
            session.subagent_rollups.insert(
                path,
                SubagentRollup {
                    input_tokens: 10_000,
                    output_tokens: 1_000,
                    cost_usd: 0.2,
                    usage_metrics_available: true,
                    ..SubagentRollup::default()
                },
            );
        }

        let rows = session.subagent_breakdown();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].display_label(), "completed (2)");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].format_tokens(), "20.0k/2.0k");
    }

    // ── Cognitive health tracking tests ──────────────────────────────

    #[test]
    fn error_window_flush() {
        let mut s = make_session();
        s.current_window_errors = 3;
        // Call record_activity 5 times to trigger one window flush
        for _ in 0..5 {
            s.record_activity();
        }
        assert_eq!(s.error_counts_per_window.len(), 1);
        assert_eq!(s.error_counts_per_window[0], 3);
        assert_eq!(s.current_window_errors, 0);
        assert_eq!(s.window_tick_counter, 0);
    }

    #[test]
    fn baseline_error_rate_freezes() {
        let mut s = make_session();
        // Simulate 3 windows of errors
        for errors in [2, 3, 4] {
            s.current_window_errors = errors;
            for _ in 0..5 {
                s.record_activity();
            }
        }
        assert_eq!(s.error_counts_per_window.len(), 3);
        let baseline = s.baseline_error_rate.expect("baseline should be set");
        // baseline = (2+3+4)/3 = 3.0
        assert!((baseline - 3.0).abs() < 0.01);

        // Add another window — baseline should NOT change
        s.current_window_errors = 10;
        for _ in 0..5 {
            s.record_activity();
        }
        assert_eq!(s.baseline_error_rate.unwrap(), baseline);
    }

    // ── Remote session tests ────────────────────────────────────────

    #[test]
    fn local_session_is_not_remote() {
        let s = make_session();
        assert!(!s.is_remote());
        assert!(s.worker_origin.is_none());
    }

    #[test]
    fn from_remote_json_parses_basic_fields() {
        let json = serde_json::json!({
            "pid": 42,
            "project": "backend",
            "status": "Processing",
            "cost_usd": 1.23,
            "elapsed_secs": 600,
            "tokens_in": 50000,
            "tokens_out": 10000,
        });
        let session = AgentSession::from_remote_json("macbook-02", &json).unwrap();
        assert!(session.is_remote());
        assert_eq!(session.worker_origin.as_deref(), Some("macbook-02"));
        assert_eq!(session.pid, 42);
        assert_eq!(session.project_name, "[macbook-02] backend");
        assert_eq!(session.status, SessionStatus::Processing);
        assert!((session.cost_usd - 1.23).abs() < 0.01);
        assert_eq!(session.total_input_tokens, 50000);
        assert_eq!(session.total_output_tokens, 10000);
        assert!(session.usage_metrics_available);
    }

    #[test]
    fn from_remote_json_handles_all_statuses() {
        for (label, expected) in [
            ("Needs Input", SessionStatus::NeedsInput),
            ("Processing", SessionStatus::Processing),
            ("Waiting", SessionStatus::WaitingInput),
            ("Idle", SessionStatus::Idle),
            ("Finished", SessionStatus::Finished),
            ("SomethingElse", SessionStatus::Unknown),
        ] {
            let json = serde_json::json!({"pid": 1, "project": "p", "status": label});
            let session = AgentSession::from_remote_json("w", &json).unwrap();
            assert_eq!(session.status, expected, "status mismatch for {label}");
        }
    }

    #[test]
    fn from_remote_json_returns_none_on_missing_fields() {
        // Missing pid
        let json = serde_json::json!({"project": "x", "status": "Idle"});
        assert!(AgentSession::from_remote_json("w", &json).is_none());

        // Missing project
        let json = serde_json::json!({"pid": 1, "status": "Idle"});
        assert!(AgentSession::from_remote_json("w", &json).is_none());
    }

    #[test]
    fn remote_session_display_name_shows_worker_prefix() {
        let json = serde_json::json!({"pid": 1, "project": "api-server", "status": "Idle"});
        let session = AgentSession::from_remote_json("laptop-01", &json).unwrap();
        assert_eq!(session.display_name(), "[laptop-01] api-server");
    }

    #[test]
    fn remote_session_json_includes_worker_origin() {
        let json = serde_json::json!({"pid": 1, "project": "test", "status": "Idle"});
        let session = AgentSession::from_remote_json("remote-w", &json).unwrap();
        let output = session.to_json_value();
        assert_eq!(
            output.get("worker_origin").and_then(|v| v.as_str()),
            Some("remote-w")
        );
    }

    #[test]
    fn session_json_exposes_only_lifecycle_provenance() {
        let mut session = make_session();
        session.lifecycle_diagnostic = crate::lifecycle::LifecycleDiagnostic {
            available: true,
            event: Some(crate::lifecycle::LifecycleEventName::PreToolUse),
            age_ms: Some(125),
            contributing: true,
            ignored_reason: None,
            store_condition: Some(crate::lifecycle::StoreCondition::Healthy),
        };

        let lifecycle = session.to_json_value()["lifecycle"].clone();

        assert_eq!(lifecycle["available"], true);
        assert_eq!(lifecycle["store_condition"], "healthy");
        assert_eq!(lifecycle["last_event"], "PreToolUse");
        assert_eq!(lifecycle["age_ms"], 125);
        assert_eq!(lifecycle["contributing"], true);
        assert!(lifecycle.get("prompt").is_none());
        assert!(lifecycle.get("tool_input").is_none());
        assert!(lifecycle.get("tool_output").is_none());
    }
}
