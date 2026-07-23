#![allow(dead_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::brain::client::BrainSuggestion;
use coding_brain_core::brain_activity::ActivityEvent;
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::provider::AgentProvider;
use fs2::FileExt;

// ────────────────────────────────────────────────────────────────────────────
// Re-exports from sub-modules so that existing `brain::decisions::*` paths
// continue to resolve without changes to callers.
// ────────────────────────────────────────────────────────────────────────────

#[allow(unused_imports)]
pub use super::preferences::{
    DistilledPreferences, PreferenceCondition, PreferencePattern, TemporalPattern, ToolAccuracy,
    adaptive_threshold, backfill_outcomes, distill_preferences, format_preference_summary,
    load_preferences, load_preferences_for_project,
};

#[allow(unused_imports)]
pub use super::retrieval::{format_few_shot_examples, retrieve_similar};

// ────────────────────────────────────────────────────────────────────────────
// Atomics and constants
// ────────────────────────────────────────────────────────────────────────────

/// Monotonic counter for decision_id uniqueness within a process.
static DECISION_ID_COUNTER: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static TEST_RUN_ID: std::sync::OnceLock<u128> = std::sync::OnceLock::new();

const MAX_DECISION_RECORD_BYTES: u64 = 1024 * 1024;

// ────────────────────────────────────────────────────────────────────────────
// Core types
// ────────────────────────────────────────────────────────────────────────────

/// Scope of a Brain decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionType {
    /// A tool permission decision for one session.
    Session,
}

impl DecisionType {
    pub fn label(&self) -> &'static str {
        "session"
    }

    pub fn from_label(_s: &str) -> Self {
        DecisionType::Session
    }
}

/// A single decision record: what the brain suggested and what the user did.
#[derive(Debug, Clone)]
pub struct DecisionRecord {
    pub provider: AgentProvider,
    pub timestamp: String,
    pub pid: u32,
    pub project: String,
    pub tool: Option<String>,
    pub command: Option<String>,
    pub brain_action: String,
    pub brain_confidence: f64,
    pub brain_reasoning: String,
    pub user_action: String, // "accept", "reject", "auto", "deny_rule_override"
    pub context: Option<DecisionContext>,
    pub outcome: Option<DecisionOutcome>,
    /// Decision scope. Historical values are normalized to Session.
    pub decision_type: DecisionType,
    /// Epoch seconds when the brain suggestion was created.
    /// None for old records or observations. Used by time-to-correct analysis.
    pub suggested_at: Option<u64>,
    /// Epoch seconds when the user acted on the suggestion.
    pub resolved_at: Option<u64>,
    /// Why the user overrode a brain denial (if applicable).
    pub override_reason: Option<String>,
    /// Stable id for outcome attribution (#220 baselining). None on records
    /// written before the field existed; outcomes for those can't be joined.
    pub decision_id: Option<String>,
    /// Wall-clock latency of the brain decision in milliseconds (LLM call +
    /// few-shot retrieval). None for records before instrumentation or for
    /// pure observations where the brain wasn't invoked.
    pub brain_decision_ms: Option<u64>,
    /// True when the suggestion was satisfied entirely from the few-shot
    /// cache without an LLM call. None for records before the field existed.
    pub cache_hit: Option<bool>,
    /// True when the user has marked this decision as canonical training
    /// material via `coding-brain --brain-review`. Canonical decisions get a
    /// large score boost in few-shot retrieval. None == not reviewed.
    pub canonical: Option<bool>,
}

/// Generate a unique decision id.
pub fn gen_decision_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let seq = DECISION_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("dec_{nanos}_{pid}_{seq}")
}

/// Outcome of a decision, backfilled during distillation by looking at
/// consecutive same-PID records and resolved test-runner outcomes.
#[derive(Debug, Clone)]
pub enum DecisionOutcome {
    Success,
    Error(String),
    /// A test-runner command failed within the attribution window after this
    /// edit was approved (#238). Carries the failing command for diagnostics.
    /// Weighted more strongly than `Error` in distillation because a broken
    /// build is a stronger negative signal than a transient tool error.
    TestFailed(String),
}

/// Snapshot of session state captured at decision time.
/// Stored in JSONL for rich distillation. NOT sent to LLM directly.
#[derive(Debug, Clone)]
pub struct DecisionContext {
    pub cost_usd: f64,
    pub context_pct: u8,
    pub last_tool_error: bool,
    pub error_message: Option<String>,
    pub model: String,
    pub elapsed_secs: u64,
    pub files_modified_count: u32,
    pub total_tool_calls: u32,
    pub has_file_conflict: bool,
    pub status: String,
    pub burn_rate_per_hr: f64,
    pub recent_error_count: u8,
    pub subagent_count: u8,
    /// Hour of day (0-23) when this decision was made. Used for time-of-day
    /// preference distillation. None for records from before this field existed.
    pub hour: Option<u8>,
}

/// Project a `DecisionRecord` into the core `DecisionSummary` DTO. Used by
/// every runtime adapter (`BrainView`, `BrainReviewView`) plus the metrics
/// pipeline once it migrates to operate on summaries. Conversion lives here
/// because `DecisionRecord` is local to the binary crate, satisfying the
/// orphan rules for the foreign `DecisionSummary` impl.
impl From<&DecisionRecord> for coding_brain_core::runtime::DecisionSummary {
    fn from(r: &DecisionRecord) -> Self {
        Self {
            provider: r.provider,
            id: r.decision_id.clone().unwrap_or_default(),
            timestamp: r.timestamp.clone(),
            action: r.brain_action.clone(),
            confidence: Some(r.brain_confidence),
            project: Some(r.project.clone()),
            tool: r.tool.clone(),
            pid: r.pid,
            command: r.command.clone(),
            reasoning: Some(r.brain_reasoning.clone()).filter(|s| !s.is_empty()),
            user_action: Some(r.user_action.clone()),
            override_reason: r.override_reason.clone(),
            brain_decision_ms: r.brain_decision_ms,
            canonical: r.canonical,
            cache_hit: r.cache_hit,
            cost_usd: r.context.as_ref().map(|c| c.cost_usd),
            model: r.context.as_ref().map(|c| c.model.clone()),
            outcome_kind: r.outcome.as_ref().map(|o| match o {
                DecisionOutcome::Success => "success".to_string(),
                DecisionOutcome::Error(_) => "error".to_string(),
                DecisionOutcome::TestFailed(_) => "test_failed".to_string(),
            }),
            outcome_detail: r.outcome.as_ref().and_then(|o| match o {
                DecisionOutcome::Error(msg) => Some(msg.clone()),
                DecisionOutcome::TestFailed(cmd) => Some(cmd.clone()),
                _ => None,
            }),
            suggested_at: r.suggested_at,
            resolved_at: r.resolved_at,
        }
    }
}

impl DecisionRecord {
    /// Whether this decision represents a positive outcome (user agreed or auto-executed).
    pub fn is_positive(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "accept" | "auto" | "user_approve" | "rule_approve"
        )
    }

    /// Whether this decision represents a negative outcome (user disagreed).
    pub fn is_negative(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "reject" | "deny_rule_override" | "rule_deny" | "conflict_deny"
        )
    }

    /// Whether this is a passive observation (brain was NOT involved).
    pub fn is_observation(&self) -> bool {
        matches!(
            self.user_action.as_str(),
            "user_approve"
                | "user_input"
                | "rule_approve"
                | "rule_deny"
                | "rule_send"
                | "conflict_deny"
        )
    }
}

#[derive(Debug, Default)]
pub struct DecisionStats {
    pub total: u32,
    pub accepted: u32,
    pub rejected: u32,
    pub auto_executed: u32,
    pub observations: u32,
}

impl DecisionStats {
    pub fn accuracy_pct(&self) -> f64 {
        let decided = self.accepted + self.rejected;
        if decided == 0 {
            return 0.0;
        }
        (self.accepted as f64 / decided as f64) * 100.0
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Path helpers
// ────────────────────────────────────────────────────────────────────────────

pub(super) fn decisions_dir() -> PathBuf {
    #[cfg(test)]
    {
        let thread = std::thread::current();
        let scope = project_slug(thread.name().unwrap_or("unnamed-test"));
        let run_id = TEST_RUN_ID.get_or_init(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        });
        std::env::temp_dir()
            .join("codexctl-tests")
            .join(std::process::id().to_string())
            .join(format!("{run_id}-{scope}"))
            .join("brain")
    }

    #[cfg(not(test))]
    {
        CodingBrainPaths::resolve(&PathEnvironment::current())
            .map(|paths| paths.state_root().join("brain"))
            .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain/brain"))
    }
}

fn decisions_path() -> PathBuf {
    decisions_dir().join("decisions.jsonl")
}

/// Convert a project name to a filesystem-safe slug.
/// Returns "unknown" for empty or whitespace-only names.
pub(super) fn project_slug(project: &str) -> String {
    let slug: String = project
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_lowercase();
    if slug.is_empty() || slug.chars().all(|c| c == '_') {
        "unknown".to_string()
    } else {
        slug
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Time helpers
// ────────────────────────────────────────────────────────────────────────────

fn timestamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-ish format without chrono dependency
    format!("{secs}")
}

fn append_json_line(path: &std::path::Path, record: &serde_json::Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
    line.push(b'\n');
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_directory_mode(parent)?;
    }
    let lock = acquire_decisions_lock(path)?;
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    set_file_mode(&file)?;
    repair_jsonl_tail(&mut file)?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&line)?;
    file.flush()?;
    file.sync_data()?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    FileExt::unlock(&lock)
}

fn acquire_decisions_lock(path: &std::path::Path) -> io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_directory_mode(parent)?;
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    set_file_mode(&lock)?;
    let started = Instant::now();
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= Duration::from_millis(100) {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "decision store lock timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(lock)
}

fn repair_jsonl_tail(file: &mut fs::File) -> io::Result<()> {
    let length = file.metadata()?.len();
    if length == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    let tail_start = find_tail_start(file, length)?;
    let tail_length = length.saturating_sub(tail_start);
    if tail_length <= MAX_DECISION_RECORD_BYTES {
        file.seek(SeekFrom::Start(tail_start))?;
        let mut tail = vec![0_u8; tail_length as usize];
        file.read_exact(&mut tail)?;
        if serde_json::from_slice::<serde_json::Value>(&tail).is_ok() {
            file.seek(SeekFrom::End(0))?;
            file.write_all(b"\n")?;
            return Ok(());
        }
    }
    file.set_len(tail_start)?;
    Ok(())
}

fn find_tail_start(file: &mut fs::File, length: u64) -> io::Result<u64> {
    let mut cursor = length;
    let mut buffer = [0_u8; 8 * 1024];
    while cursor > 0 {
        let chunk_len = usize::try_from(cursor.min(buffer.len() as u64)).unwrap_or(buffer.len());
        cursor -= chunk_len as u64;
        file.seek(SeekFrom::Start(cursor))?;
        file.read_exact(&mut buffer[..chunk_len])?;
        if let Some(index) = buffer[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            return Ok(cursor + index as u64 + 1);
        }
    }
    Ok(0)
}

#[cfg(unix)]
fn set_directory_mode(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &std::path::Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &std::path::Path) -> io::Result<()> {
    Ok(())
}

/// Compute the current local hour (0-23) without chrono.
/// Uses libc::localtime_r for timezone-aware hour so that work-hours
/// pattern detection aligns with the user's actual schedule.
pub(super) fn current_hour() -> u8 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    local_hour_from_epoch(secs as i64)
}

pub(super) fn local_hour_from_epoch(epoch_secs: i64) -> u8 {
    #[cfg(unix)]
    {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        unsafe { libc::localtime_r(&epoch_secs, &mut tm) };
        tm.tm_hour as u8
    }
    #[cfg(not(unix))]
    {
        // Fallback to UTC on non-unix platforms
        ((epoch_secs as u64 % 86400) / 3600) as u8
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Context snapshot
// ────────────────────────────────────────────────────────────────────────────

/// Build a JSON snapshot of session state for embedding in a JSONL record.
fn snapshot_context(session: &crate::session::AgentSession) -> serde_json::Value {
    let context_pct = if session.context_max > 0 {
        ((session.context_tokens as f64 / session.context_max as f64) * 100.0) as u8
    } else {
        0
    };
    serde_json::json!({
        "cost_usd": session.cost_usd,
        "context_pct": context_pct,
        "last_tool_error": session.last_tool_error,
        "error_message": session.last_error_message.as_deref().map(|m| crate::session::truncate_str(m, 100)),
        "model": session.model,
        "elapsed_secs": session.elapsed.as_secs(),
        "files_modified_count": session.files_modified.len() as u32,
        "total_tool_calls": session.tool_usage.values().map(|t| t.calls).sum::<u32>(),
        "has_file_conflict": session.has_file_conflict,
        "status": session.status.to_string(),
        "burn_rate_per_hr": session.burn_rate_per_hr,
        "recent_error_count": session.recent_errors.len() as u8,
        "subagent_count": session.subagent_count as u8,
        "hour": current_hour(),
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Logging
// ────────────────────────────────────────────────────────────────────────────

/// Log a brain decision (suggestion + user response) to the local JSONL file.
/// `decision_type` is retained in the record format for historical readers.
#[allow(clippy::too_many_arguments)]
pub fn log_decision(
    pid: u32,
    project: &str,
    tool: Option<&str>,
    command: Option<&str>,
    suggestion: &BrainSuggestion,
    user_action: &str,
    session: Option<&crate::session::AgentSession>,
    decision_type: DecisionType,
    override_reason: Option<&str>,
) {
    log_decision_full(
        pid,
        project,
        tool,
        command,
        suggestion,
        user_action,
        session,
        decision_type,
        override_reason,
        None,
        None,
    );
}

/// Same as `log_decision` but accepts measured latency and cache-hit signals.
/// Use this from the engine call site once instrumentation is wired; legacy
/// `log_decision` call sites continue to work and emit `None` for those fields.
#[allow(clippy::too_many_arguments)]
pub fn log_decision_full(
    pid: u32,
    project: &str,
    tool: Option<&str>,
    command: Option<&str>,
    suggestion: &BrainSuggestion,
    user_action: &str,
    session: Option<&crate::session::AgentSession>,
    decision_type: DecisionType,
    override_reason: Option<&str>,
    brain_decision_ms: Option<u64>,
    cache_hit: Option<bool>,
) {
    let resolved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let decision_id = gen_decision_id();
    let mut record = serde_json::json!({
        "provider": session.map(|session| session.provider).unwrap_or_default(),
        "ts": timestamp_now(),
        "pid": pid,
        "project": project,
        "tool": tool,
        "command": command,
        "brain_action": suggestion.action.label(),
        "brain_confidence": suggestion.confidence,
        "brain_reasoning": suggestion.reasoning,
        "user_action": user_action,
        "decision_type": decision_type.label(),
        "suggested_at": suggestion.suggested_at,
        "resolved_at": resolved_at,
        "override_reason": override_reason,
        "decision_id": decision_id,
        "brain_decision_ms": brain_decision_ms,
        "cache_hit": cache_hit,
    });
    if let Some(s) = session {
        record["context"] = snapshot_context(s);
    }

    if append_json_line(&decisions_path(), &record).is_ok() {
        trigger_distill();
    }
}

/// Log a passive observation: a user action the brain was NOT involved in.
/// These provide ground-truth training data — what the user does when
/// deciding on their own. Same JSONL format so distillation picks them up.
pub fn log_observation(
    pid: u32,
    project: &str,
    tool: Option<&str>,
    command: Option<&str>,
    observed_action: &str, // "user_approve", "user_input", "rule_approve", "rule_deny", etc.
    session: Option<&crate::session::AgentSession>,
) {
    let decision_id = gen_decision_id();
    let mut record = serde_json::json!({
        "provider": session.map(|session| session.provider).unwrap_or_default(),
        "ts": timestamp_now(),
        "pid": pid,
        "project": project,
        "tool": tool,
        "command": command,
        "brain_action": null,
        "brain_confidence": 0.0,
        "brain_reasoning": "",
        "user_action": observed_action,
        "decision_id": decision_id,
    });
    if let Some(s) = session {
        record["context"] = snapshot_context(s);
    }

    if append_json_line(&decisions_path(), &record).is_ok() {
        trigger_distill();
    }
}

pub(crate) struct HookDecisionAudit<'a> {
    pub provider: AgentProvider,
    pub project: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
    pub brain_action: &'a str,
    pub brain_confidence: f64,
    pub brain_reasoning: &'a str,
    pub brain_source: &'a str,
    pub brain_threshold: Option<f64>,
    pub session_id: &'a str,
    pub turn_id: &'a str,
}

/// Persist a permission-hook decision before it is returned to Codex.
///
/// `hook_allow` and `hook_deny` mean that the decision was prepared; this
/// hook does not receive a later execution confirmation from Codex.
pub(crate) fn append_hook_proposal(audit: &HookDecisionAudit<'_>) -> io::Result<String> {
    append_hook_audit(audit, "hook_proposal")
}

pub(crate) fn append_deterministic(audit: &HookDecisionAudit<'_>) -> io::Result<String> {
    append_hook_audit(audit, "deterministic_deny")
}

fn append_hook_audit(audit: &HookDecisionAudit<'_>, user_action: &str) -> io::Result<String> {
    let resolved_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let decision_id = gen_decision_id();
    let record = serde_json::json!({
        "provider": audit.provider,
        "ts": timestamp_now(),
        "pid": 0,
        "project": audit.project,
        "tool": audit.tool,
        "command": audit.command,
        "brain_action": audit.brain_action,
        "brain_confidence": audit.brain_confidence,
        "brain_reasoning": audit.brain_reasoning,
        "brain_source": audit.brain_source,
        "brain_threshold": audit.brain_threshold,
        "user_action": user_action,
        "decision_type": DecisionType::Session.label(),
        "suggested_at": resolved_at,
        "resolved_at": resolved_at,
        "decision_id": decision_id,
        "session_id": audit.session_id,
        "turn_id": audit.turn_id,
    });
    append_json_line(&decisions_path(), &record)?;
    trigger_distill();
    Ok(decision_id)
}

fn trigger_distill() {
    let environment = PathEnvironment::current();
    if let Ok(paths) = CodingBrainPaths::resolve(&environment) {
        let _ = super::distill::spawn_one_shot_if_due(&paths);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Reading decisions and stats
// ────────────────────────────────────────────────────────────────────────────

/// Read decision stats for display.
pub fn read_stats() -> DecisionStats {
    let path = decisions_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return DecisionStats::default(),
    };

    let mut total = 0u32;
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut auto_executed = 0u32;
    let mut observations = 0u32;

    for line in content.lines() {
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        total += 1;
        match json.get("user_action").and_then(|v| v.as_str()) {
            Some("accept") => accepted += 1,
            Some("reject") => rejected += 1,
            Some("auto") => auto_executed += 1,
            Some(
                "user_approve" | "user_input" | "rule_approve" | "rule_deny" | "rule_send"
                | "conflict_deny",
            ) => observations += 1,
            _ => {}
        }
    }

    DecisionStats {
        total,
        accepted,
        rejected,
        auto_executed,
        observations,
    }
}

/// Clear all decision history and distilled preferences.
pub fn forget() -> Result<(), String> {
    let environment = PathEnvironment::current();
    let paths = CodingBrainPaths::resolve(&environment)
        .map_err(|error| format!("failed to resolve Coding Brain state: {error:?}"))?;
    forget_at(&paths, &decisions_dir())
}

fn forget_at(paths: &CodingBrainPaths, source_root: &std::path::Path) -> Result<(), String> {
    forget_at_with(paths, source_root, || {})
}

pub(crate) fn forget_at_with(
    paths: &CodingBrainPaths,
    source_root: &std::path::Path,
    after_source_erased: impl FnOnce(),
) -> Result<(), String> {
    let path = source_root.join("decisions.jsonl");
    let decisions_lock = acquire_decisions_lock(&path)
        .map_err(|error| format!("failed to lock {}: {error}", path.display()))?;
    let result = super::distill::forget_preferences_with(paths, || {
        erase_decision_source(source_root)?;
        after_source_erased();
        Ok(())
    })
    .map_err(|error| error.to_string());
    let unlock = FileExt::unlock(&decisions_lock)
        .map_err(|error| format!("failed to unlock {}: {error}", path.display()));
    result.and(unlock)
}

fn erase_decision_source(source_root: &std::path::Path) -> io::Result<()> {
    let path = source_root.join("decisions.jsonl");
    if path.exists() {
        fs::remove_file(&path)?;
    }
    let pref_path = source_root.join("preferences.json");
    if pref_path.exists() {
        fs::remove_file(&pref_path)?;
    }
    // Also clean per-project preference files
    let proj_dir = source_root.join("preferences");
    if proj_dir.is_dir() {
        fs::remove_dir_all(&proj_dir)?;
    }
    Ok(())
}

pub fn read_all_decisions() -> Vec<DecisionRecord> {
    let path = decisions_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let canonical_set = read_canonical_ids();

    content
        .lines()
        .filter_map(|line| {
            let json: serde_json::Value = serde_json::from_str(line).ok()?;
            let context = json.get("context").and_then(|ctx| {
                Some(DecisionContext {
                    cost_usd: ctx.get("cost_usd")?.as_f64()?,
                    context_pct: ctx.get("context_pct")?.as_u64()? as u8,
                    last_tool_error: ctx.get("last_tool_error")?.as_bool()?,
                    error_message: ctx
                        .get("error_message")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                    model: ctx.get("model")?.as_str()?.to_string(),
                    elapsed_secs: ctx.get("elapsed_secs")?.as_u64()?,
                    files_modified_count: ctx.get("files_modified_count")?.as_u64()? as u32,
                    total_tool_calls: ctx.get("total_tool_calls")?.as_u64()? as u32,
                    has_file_conflict: ctx.get("has_file_conflict")?.as_bool()?,
                    status: ctx.get("status")?.as_str()?.to_string(),
                    burn_rate_per_hr: ctx.get("burn_rate_per_hr")?.as_f64()?,
                    recent_error_count: ctx.get("recent_error_count")?.as_u64()? as u8,
                    subagent_count: ctx.get("subagent_count")?.as_u64()? as u8,
                    // Backwards-compatible: old records won't have "hour" field
                    hour: ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8),
                })
            });
            // Backwards-compatible: old records won't have "decision_type" field
            let decision_type = json
                .get("decision_type")
                .and_then(|v| v.as_str())
                .map(DecisionType::from_label)
                .unwrap_or(DecisionType::Session);
            let provider = match json.get("provider") {
                None => AgentProvider::Codex,
                Some(value) => serde_json::from_value(value.clone()).ok()?,
            };
            Some(DecisionRecord {
                provider,
                timestamp: json.get("ts")?.to_string(),
                pid: json.get("pid")?.as_u64()? as u32,
                project: json.get("project")?.as_str()?.to_string(),
                tool: json
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                command: json
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                // Handle null brain_action (observations log it as null)
                brain_action: json
                    .get("brain_action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                brain_confidence: json
                    .get("brain_confidence")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0),
                brain_reasoning: json
                    .get("brain_reasoning")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                user_action: json.get("user_action")?.as_str()?.to_string(),
                context,
                outcome: None, // Backfilled during distillation
                decision_type,
                // Backwards-compatible: old records won't have these fields
                suggested_at: json.get("suggested_at").and_then(|v| v.as_u64()),
                resolved_at: json.get("resolved_at").and_then(|v| v.as_u64()),
                override_reason: json
                    .get("override_reason")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                decision_id: json
                    .get("decision_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                brain_decision_ms: json.get("brain_decision_ms").and_then(|v| v.as_u64()),
                cache_hit: json.get("cache_hit").and_then(|v| v.as_bool()),
                canonical: {
                    // Inline canonical flag wins; otherwise check the side store.
                    let inline = json.get("canonical").and_then(|v| v.as_bool());
                    let dec_id = json.get("decision_id").and_then(|v| v.as_str());
                    match (inline, dec_id) {
                        (Some(b), _) => Some(b),
                        (None, Some(id)) if canonical_set.contains(id) => Some(true),
                        _ => None,
                    }
                },
            })
        })
        .collect()
}

pub(crate) fn read_learning_decisions() -> Vec<DecisionRecord> {
    read_distillation_decisions().1
}

pub(crate) fn read_distillation_decisions() -> (Vec<DecisionRecord>, Vec<DecisionRecord>) {
    let decisions = read_all_decisions();
    let environment = PathEnvironment::new(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    let events = CodingBrainPaths::resolve(&environment)
        .ok()
        .and_then(|paths| {
            super::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"))
                .read()
                .ok()
        })
        .map(|log| log.events().to_vec())
        .unwrap_or_default();
    let learning = filter_learning_decisions(decisions.clone(), &events);
    (decisions, learning)
}

pub(crate) fn filter_learning_decisions(
    decisions: Vec<DecisionRecord>,
    events: &[ActivityEvent],
) -> Vec<DecisionRecord> {
    let mut terminal_activities = std::collections::HashSet::new();
    let mut committed = std::collections::HashSet::new();
    for event in events.iter().filter(|event| event.state.is_terminal()) {
        if terminal_activities.insert(event.activity_id.as_str()) {
            if let Some(decision_id) = event.decision_id.as_deref() {
                committed.insert(decision_id);
            }
        }
    }
    decisions
        .into_iter()
        .filter(|decision| {
            decision.user_action != "hook_proposal"
                || decision
                    .decision_id
                    .as_deref()
                    .is_some_and(|decision_id| committed.contains(decision_id))
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Canonical-marks side store
// ────────────────────────────────────────────────────────────────────────────

fn canonical_path() -> PathBuf {
    decisions_dir().join("canonical.jsonl")
}

/// Persist a canonical mark for the given decision id.
/// Idempotent: appending the same id twice is harmless — the set dedupes on read.
pub fn mark_canonical(decision_id: &str, note: Option<&str>) -> Result<(), String> {
    let path = canonical_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let record = serde_json::json!({
        "decision_id": decision_id,
        "marked_at": ts,
        "note": note,
    });
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open canonical store: {e}"))?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&record).unwrap_or_default()
    )
    .map_err(|e| format!("write canonical mark: {e}"))?;
    Ok(())
}

/// Read the set of decision ids that have been marked canonical.
pub fn read_canonical_ids() -> std::collections::HashSet<String> {
    let path = canonical_path();
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return std::collections::HashSet::new(),
    };
    content
        .lines()
        .filter_map(|line| {
            let json: serde_json::Value = serde_json::from_str(line).ok()?;
            json.get("decision_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::RuleAction;
    use coding_brain_core::brain_activity::ActivityKind;

    #[test]
    fn unit_test_decision_paths_are_thread_scoped() {
        let path_for = |name: &str| {
            std::thread::Builder::new()
                .name(name.into())
                .spawn(decisions_dir)
                .unwrap()
                .join()
                .unwrap()
        };

        let first = path_for("brain-test-first");
        let second = path_for("brain-test-second");
        let root = std::env::temp_dir()
            .join("codexctl-tests")
            .join(std::process::id().to_string());

        assert_ne!(first, second);
        assert!(first.starts_with(&root));
        assert!(second.starts_with(&root));
        assert!(first.ends_with("brain"));
        assert!(second.ends_with("brain"));
    }

    fn make_suggestion() -> BrainSuggestion {
        BrainSuggestion {
            action: RuleAction::Approve,
            message: None,
            reasoning: "safe command".into(),
            confidence: 0.95,
            suggested_at: 0,
        }
    }

    #[test]
    fn decision_records_default_legacy_provider_and_retain_explicit_provider() {
        let root = decisions_dir();
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("decisions.jsonl"),
            concat!(
                "{\"ts\":\"1\",\"pid\":1,\"project\":\"legacy\",\"user_action\":\"accept\"}\n",
                "{\"provider\":\"claude\",\"ts\":\"2\",\"pid\":2,\"project\":\"new\",\"user_action\":\"accept\"}\n",
                "{\"provider\":\"future-provider\",\"ts\":\"3\",\"pid\":3,\"project\":\"invalid\",\"user_action\":\"accept\"}\n"
            ),
        )
        .unwrap();

        let decisions = read_all_decisions();

        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].provider, AgentProvider::Codex);
        assert_eq!(decisions[1].provider, AgentProvider::Claude);
    }

    fn make_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            provider: AgentProvider::Codex,
            timestamp: "0".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some("test cmd".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "test".into(),
            user_action: user_action.into(),
            context: None,
            outcome: None,
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: None,
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    fn make_context(cost_usd: f64, context_pct: u8, last_tool_error: bool) -> DecisionContext {
        DecisionContext {
            cost_usd,
            context_pct,
            last_tool_error,
            error_message: if last_tool_error {
                Some("test error".to_string())
            } else {
                None
            },
            model: "gpt-5.4".into(),
            elapsed_secs: 60,
            files_modified_count: 2,
            total_tool_calls: 10,
            has_file_conflict: false,
            status: "Working".into(),
            burn_rate_per_hr: 1.0,
            recent_error_count: if last_tool_error { 1 } else { 0 },
            subagent_count: 0,
            hour: None,
        }
    }

    fn make_context_with_hour(
        cost_usd: f64,
        context_pct: u8,
        last_tool_error: bool,
        hour: u8,
    ) -> DecisionContext {
        DecisionContext {
            hour: Some(hour),
            ..make_context(cost_usd, context_pct, last_tool_error)
        }
    }

    #[test]
    fn log_and_read_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");

        // Write directly to a temp path
        let record = serde_json::json!({
            "user_action": "accept",
            "brain_action": "approve",
        });
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();

        let record2 = serde_json::json!({
            "user_action": "reject",
            "brain_action": "approve",
        });
        writeln!(file, "{}", serde_json::to_string(&record2).unwrap()).unwrap();
        drop(file);

        // Parse the file
        let content = fs::read_to_string(&path).unwrap();
        let mut accepted = 0;
        let mut rejected = 0;
        for line in content.lines() {
            let json: serde_json::Value = serde_json::from_str(line).unwrap();
            match json["user_action"].as_str() {
                Some("accept") => accepted += 1,
                Some("reject") => rejected += 1,
                _ => {}
            }
        }
        assert_eq!(accepted, 1);
        assert_eq!(rejected, 1);
    }

    #[test]
    fn concurrent_single_buffer_appends_are_parseable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let mut threads = Vec::new();
        for worker in 0..8 {
            let path = path.clone();
            threads.push(std::thread::spawn(move || {
                for item in 0..50 {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    loop {
                        match append_json_line(
                            &path,
                            &serde_json::json!({"worker": worker, "item": item}),
                        ) {
                            Ok(()) => break,
                            Err(error)
                                if error.kind() == io::ErrorKind::TimedOut
                                    && Instant::now() < deadline =>
                            {
                                std::thread::yield_now();
                            }
                            Err(error) => panic!("append failed: {error}"),
                        }
                    }
                }
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }

        let log = fs::read_to_string(path).unwrap();
        assert_eq!(log.lines().count(), 400);
        for line in log.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }

    #[test]
    fn append_repairs_partial_crash_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        fs::write(&path, b"{\"complete\":true}\n{\"partial\":").unwrap();

        append_json_line(&path, &serde_json::json!({"next": true})).unwrap();

        let log = fs::read_to_string(path).unwrap();
        let records = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["complete"], true);
        assert_eq!(records[1]["next"], true);
    }

    #[test]
    fn append_preserves_valid_unterminated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        fs::write(&path, b"{\"complete\":true}").unwrap();

        append_json_line(&path, &serde_json::json!({"next": true})).unwrap();

        let log = fs::read_to_string(path).unwrap();
        assert_eq!(log.lines().count(), 2);
        for line in log.lines() {
            serde_json::from_str::<serde_json::Value>(line).unwrap();
        }
    }

    #[test]
    fn learning_join_keeps_only_terminally_committed_hook_proposals() {
        let mut paired = make_decision("Bash", "proj", "hook_proposal");
        paired.decision_id = Some("paired".into());
        let mut unpaired = make_decision("Bash", "proj", "hook_proposal");
        unpaired.decision_id = Some("unpaired".into());
        let accepted = make_decision("Bash", "proj", "accept");
        let event = ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: "activity-1".into(),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Temporary("project".into()),
                cwd: std::env::current_dir().unwrap(),
                label: None,
            },
            session: None,
            state: coding_brain_core::brain_activity::ActivityState::Allowed,
            tool: Some("Bash".into()),
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: Some("paired".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        };
        let mut first_error = event.clone();
        first_error.activity_id = "activity-2".into();
        first_error.state = coding_brain_core::brain_activity::ActivityState::Error;
        first_error.decision_id = None;
        let mut late_duplicate = first_error.clone();
        late_duplicate.state = coding_brain_core::brain_activity::ActivityState::Allowed;
        late_duplicate.decision_id = Some("unpaired".into());

        let learning = filter_learning_decisions(
            vec![paired, unpaired, accepted],
            &[event, first_error, late_duplicate],
        );

        assert_eq!(learning.len(), 2);
        assert!(
            learning
                .iter()
                .any(|decision| decision.decision_id.as_deref() == Some("paired"))
        );
        assert!(
            !learning
                .iter()
                .any(|decision| decision.decision_id.as_deref() == Some("unpaired"))
        );
        assert!(
            learning
                .iter()
                .any(|decision| decision.user_action == "accept")
        );
    }

    #[test]
    fn stats_accuracy() {
        let stats = DecisionStats {
            total: 10,
            accepted: 8,
            rejected: 2,
            auto_executed: 0,
            observations: 0,
        };
        assert!((stats.accuracy_pct() - 80.0).abs() < f64::EPSILON);
    }

    #[test]
    fn decision_id_uses_subsecond_timestamp() {
        let id = gen_decision_id();
        let timestamp = id.split('_').nth(1).unwrap().parse::<u128>().unwrap();
        let current_seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u128;
        assert!(timestamp > current_seconds.saturating_sub(1) * 1_000_000_000);
    }

    #[test]
    fn stats_accuracy_no_decisions() {
        let stats = DecisionStats::default();
        assert!((stats.accuracy_pct() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn suggestion_label_used() {
        let s = make_suggestion();
        assert_eq!(s.action.label(), "approve");
    }

    #[test]
    fn decision_record_outcome_classification() {
        let accept = make_decision("Bash", "proj", "accept");
        assert!(accept.is_positive());
        assert!(!accept.is_negative());
        assert!(!accept.is_observation());

        let reject = make_decision("Bash", "proj", "reject");
        assert!(!reject.is_positive());
        assert!(reject.is_negative());
        assert!(!reject.is_observation());

        let auto = make_decision("Bash", "proj", "auto");
        assert!(auto.is_positive());
        assert!(!auto.is_negative());
        assert!(!auto.is_observation());

        let deny_override = make_decision("Bash", "proj", "deny_rule_override");
        assert!(!deny_override.is_positive());
        assert!(deny_override.is_negative());
    }

    // ── Passive observation tests ─────────────────────────────────────

    #[test]
    fn observation_user_approve_is_positive() {
        let d = make_decision("Read", "proj", "user_approve");
        assert!(d.is_positive());
        assert!(!d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_rule_approve_is_positive() {
        let d = make_decision("Bash", "proj", "rule_approve");
        assert!(d.is_positive());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_rule_deny_is_negative() {
        let d = make_decision("Bash", "proj", "rule_deny");
        assert!(d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_conflict_deny_is_negative() {
        let d = make_decision("Write", "proj", "conflict_deny");
        assert!(d.is_negative());
        assert!(d.is_observation());
    }

    #[test]
    fn observation_user_input_is_observation() {
        let d = make_decision("Bash", "proj", "user_input");
        assert!(d.is_observation());
        // user_input is neither approve nor deny
        assert!(!d.is_positive());
        assert!(!d.is_negative());
    }

    // ── Snapshot context tests ────────────────────────────────────────

    #[test]
    fn test_snapshot_context_fields() {
        use crate::session::{AgentSession, SessionStatus};
        use std::collections::HashMap;
        use std::time::Duration;

        let mut tool_usage = HashMap::new();
        tool_usage.insert("Bash".to_string(), crate::session::ToolStats { calls: 5 });
        tool_usage.insert("Read".to_string(), crate::session::ToolStats { calls: 3 });

        let mut files = HashMap::new();
        files.insert("src/main.rs".to_string(), 2u32);

        let session = AgentSession {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            pid: 42,
            process_start_identity: None,
            process_backed: true,
            identity_provenance: coding_brain_core::session::SessionIdentityProvenance::Unknown,
            session_id: "test-session".into(),
            native_attach_id: None,
            cwd: "/tmp".into(),
            project_name: "test-proj".into(),
            started_at: 0,
            elapsed: Duration::from_secs(120),
            tty: "/dev/pts/0".into(),
            status: SessionStatus::Processing,
            cpu_percent: 50.0,
            cpu_history: vec![],
            mem_mb: 100.0,
            own_input_tokens: 1000,
            own_output_tokens: 500,
            own_cache_read_tokens: 0,
            own_cache_write_tokens: 0,
            subagent_input_tokens: 0,
            subagent_output_tokens: 0,
            subagent_cache_read_tokens: 0,
            subagent_cache_write_tokens: 0,
            total_input_tokens: 1000,
            total_output_tokens: 500,
            model: "gpt-5.4".into(),
            command_args: "".into(),
            session_name: "test".into(),
            jsonl_path: None,
            jsonl_offset: 0,
            last_message_ts: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cost_usd: 3.50,
            own_cost_usd: 3.50,
            priced_total_tokens: 0,
            cost_ledger_frozen: false,
            context_tokens: 80000,
            context_max: 100000,
            prev_cost_usd: 3.0,
            burn_rate_per_hr: 2.5,
            subagent_count: 1,
            active_subagent_count: 0,
            active_subagent_jsonl_paths: vec![],
            subagent_rollups: HashMap::new(),
            activity_history: vec![],
            files_modified: files,
            tool_usage,
            worktree_id: None,
            telemetry_status: crate::session::TelemetryStatus::Available,
            usage_metrics_available: true,
            cost_estimate_unverified: false,
            model_profile_source: "builtin".into(),
            last_msg_type: "".into(),
            last_stop_reason: "".into(),
            is_waiting_for_task: false,
            task_state: crate::session::CodexTaskState::Unknown,
            transcript_evidence: None,
            lifecycle_evidence: None,
            lifecycle_diagnostic: coding_brain_core::lifecycle::LifecycleDiagnostic::default(),
            explicit_input_required: false,
            approval: crate::session::ApprovalObservation::NotChecked,
            approval_checked_at_ms: 0,
            pending_tool_name: None,
            pending_tool_call_id: None,
            pending_tool_input: None,
            pending_file_path: None,
            has_file_conflict: false,
            last_tool_error: true,
            last_error_message: Some("command failed".into()),
            recent_errors: vec![crate::session::ErrorEntry {
                tool_name: "Bash".into(),
                message: "exit code 1".into(),
            }],
            total_tokens_at_edit_count: 0,
            edit_event_count: 0,
            baseline_tokens_per_edit: None,
            error_counts_per_window: vec![],
            current_window_errors: 0,
            window_tick_counter: 0,
            baseline_error_rate: None,
            file_reads_since_edit: HashMap::new(),
            total_error_count: 0,
            decay_score: 0,
            worker_origin: None,
        };

        let ctx = snapshot_context(&session);

        // Verify all 13 original fields + hour
        assert_eq!(ctx["cost_usd"].as_f64().unwrap(), 3.5);
        assert_eq!(ctx["context_pct"].as_u64().unwrap(), 80);
        assert!(ctx["last_tool_error"].as_bool().unwrap());
        assert_eq!(ctx["error_message"].as_str().unwrap(), "command failed");
        assert_eq!(ctx["model"].as_str().unwrap(), "gpt-5.4");
        assert_eq!(ctx["elapsed_secs"].as_u64().unwrap(), 120);
        assert_eq!(ctx["files_modified_count"].as_u64().unwrap(), 1);
        assert_eq!(ctx["total_tool_calls"].as_u64().unwrap(), 8); // 5+3
        assert!(!ctx["has_file_conflict"].as_bool().unwrap());
        assert_eq!(ctx["status"].as_str().unwrap(), "Processing");
        assert_eq!(ctx["burn_rate_per_hr"].as_f64().unwrap(), 2.5);
        assert_eq!(ctx["recent_error_count"].as_u64().unwrap(), 1);
        assert_eq!(ctx["subagent_count"].as_u64().unwrap(), 1);
        // Hour should be present (0-23)
        let hour = ctx["hour"].as_u64().unwrap();
        assert!(hour < 24, "hour should be 0-23, got {hour}");
    }

    #[test]
    fn test_backward_compat_no_context() {
        // Simulate a JSONL record without the "context" field (old format)
        let json_str = r#"{"ts":"123","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept"}"#;
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();

        // Parse context — should be None
        let context = json.get("context").and_then(|ctx| {
            Some(DecisionContext {
                cost_usd: ctx.get("cost_usd")?.as_f64()?,
                context_pct: ctx.get("context_pct")?.as_u64()? as u8,
                last_tool_error: ctx.get("last_tool_error")?.as_bool()?,
                error_message: None,
                model: ctx.get("model")?.as_str()?.to_string(),
                elapsed_secs: ctx.get("elapsed_secs")?.as_u64()?,
                files_modified_count: ctx.get("files_modified_count")?.as_u64()? as u32,
                total_tool_calls: ctx.get("total_tool_calls")?.as_u64()? as u32,
                has_file_conflict: ctx.get("has_file_conflict")?.as_bool()?,
                status: ctx.get("status")?.as_str()?.to_string(),
                burn_rate_per_hr: ctx.get("burn_rate_per_hr")?.as_f64()?,
                recent_error_count: ctx.get("recent_error_count")?.as_u64()? as u8,
                subagent_count: ctx.get("subagent_count")?.as_u64()? as u8,
                hour: ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8),
            })
        });
        assert!(context.is_none());

        // Also verify the record still parses with null brain_action (observation)
        let obs_str = r#"{"ts":"124","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":null,"brain_confidence":0.0,"brain_reasoning":"","user_action":"user_approve"}"#;
        let obs_json: serde_json::Value = serde_json::from_str(obs_str).unwrap();
        let brain_action = obs_json
            .get("brain_action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert_eq!(brain_action, "");

        // Verify decision_type defaults to Session for old records
        let decision_type = json
            .get("decision_type")
            .and_then(|v| v.as_str())
            .map(DecisionType::from_label)
            .unwrap_or(DecisionType::Session);
        assert_eq!(decision_type, DecisionType::Session);
    }

    // ── Decision type tests ──────────────────────────────────────────

    #[test]
    fn test_decision_type_labels() {
        assert_eq!(DecisionType::Session.label(), "session");
    }

    #[test]
    fn test_decision_type_from_label() {
        assert_eq!(DecisionType::from_label("session"), DecisionType::Session);
        // Historical and unknown scopes normalize to Session.
        assert_eq!(
            DecisionType::from_label("orchestration"),
            DecisionType::Session
        );
        assert_eq!(DecisionType::from_label("unknown"), DecisionType::Session);
        assert_eq!(DecisionType::from_label(""), DecisionType::Session);
    }

    #[test]
    fn test_session_decision_tagged() {
        let d = make_decision("Bash", "proj", "accept");
        assert_eq!(d.decision_type, DecisionType::Session);
    }

    #[test]
    fn test_backward_compat_decision_type() {
        // Old records without decision_type should default to Session
        let json_str = r#"{"ts":"123","pid":1,"project":"proj","tool":"Bash","command":"ls","brain_action":"approve","brain_confidence":0.9,"brain_reasoning":"safe","user_action":"accept"}"#;
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let dt = json
            .get("decision_type")
            .and_then(|v| v.as_str())
            .map(DecisionType::from_label)
            .unwrap_or(DecisionType::Session);
        assert_eq!(dt, DecisionType::Session);
    }

    #[test]
    fn test_backward_compat_no_hour_in_context() {
        // Old context records without hour field → hour should be None
        let json_str = r#"{"cost_usd":1.0,"context_pct":50,"last_tool_error":false,"model":"gpt-5.4","elapsed_secs":60,"files_modified_count":2,"total_tool_calls":10,"has_file_conflict":false,"status":"Working","burn_rate_per_hr":1.0,"recent_error_count":0,"subagent_count":0}"#;
        let ctx: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let hour: Option<u8> = ctx.get("hour").and_then(|v| v.as_u64()).map(|v| v as u8);
        assert!(hour.is_none());
    }

    #[test]
    fn test_current_hour_is_valid() {
        let hour = current_hour();
        assert!(hour < 24, "current_hour() returned {hour}, expected 0-23");
    }

    #[test]
    fn test_hour_captured_in_context() {
        // The make_context_with_hour helper sets the hour field
        let ctx = make_context_with_hour(1.0, 50, false, 14);
        assert_eq!(ctx.hour, Some(14));
    }
}
