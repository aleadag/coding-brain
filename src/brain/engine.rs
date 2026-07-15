#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use crate::config::BrainConfig;
use crate::rules::{self, RuleAction, RuleMatch};
use crate::session::{ApprovalObservation, CodexSession, SessionStatus};

use super::client::BrainSuggestion;
use super::context;
use super::decisions::DecisionType;

/// Result sent back from inference thread.
pub struct BrainResult {
    pub pid: u32,
    target: Option<BrainTargetIdentity>,
    pub suggestion: Result<BrainSuggestion, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrainTargetIdentity {
    session_id: String,
    status: SessionStatus,
    last_message_ts: u64,
    pending_tool_call_id: Option<String>,
    pending_tool_name: Option<String>,
    pending_tool_input: Option<String>,
    approval: ApprovalObservation,
}

impl BrainTargetIdentity {
    fn from_session(session: &CodexSession) -> Self {
        Self {
            session_id: session.session_id.clone(),
            status: session.status,
            last_message_ts: session.last_message_ts,
            pending_tool_call_id: session.pending_tool_call_id.clone(),
            pending_tool_name: session.pending_tool_name.clone(),
            pending_tool_input: session.pending_tool_input.clone(),
            approval: session.approval.clone(),
        }
    }

    fn matches(&self, session: &CodexSession) -> bool {
        self == &Self::from_session(session)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingBrainSuggestion {
    pub suggestion: BrainSuggestion,
    target: Option<BrainTargetIdentity>,
}

impl PendingBrainSuggestion {
    pub(crate) fn unbound(suggestion: BrainSuggestion) -> Self {
        Self {
            suggestion,
            target: None,
        }
    }

    fn bound(suggestion: BrainSuggestion, target: Option<BrainTargetIdentity>) -> Self {
        Self { suggestion, target }
    }

    fn matches(&self, session: &CodexSession) -> bool {
        self.target
            .as_ref()
            .is_none_or(|target| target.matches(session))
    }
}

/// The brain inference engine. Manages async inference threads and collects results.
pub struct BrainEngine {
    config: BrainConfig,
    tx: Sender<BrainResult>,
    rx: Receiver<BrainResult>,
    /// PIDs currently being inferred (prevents duplicate requests).
    inflight: HashSet<u32>,
    /// Per-PID cooldown to avoid hammering the LLM.
    cooldown: HashMap<u32, Instant>,
    /// Pending suggestions waiting for user confirmation (advisory mode).
    pub(crate) pending: HashMap<u32, PendingBrainSuggestion>,
    /// Last time orchestration evaluation ran.
    last_orchestrate: Option<Instant>,
    /// Whether an orchestration inference is in-flight.
    orchestrate_inflight: bool,
    /// PIDs that have been restarted due to context saturation (prevents restart loops).
    restarted_pids: HashSet<u32>,
}

const COOLDOWN_SECS: u64 = 10;

impl BrainEngine {
    pub fn new(config: BrainConfig) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            config,
            tx,
            rx,
            inflight: HashSet::new(),
            cooldown: HashMap::new(),
            pending: HashMap::new(),
            last_orchestrate: None,
            orchestrate_inflight: false,
            restarted_pids: HashSet::new(),
        }
    }

    /// Run one tick of the brain engine. Call this from app.tick() after refresh().
    ///
    /// 1. Collect results from completed inference threads
    /// 2. Spawn new inference threads for eligible sessions
    ///
    /// Returns a list of (pid, status_message) for actions taken this tick.
    pub fn tick(
        &mut self,
        sessions: &[CodexSession],
        deny_rules: &[crate::rules::AutoRule],
    ) -> Vec<(u32, String)> {
        let mut actions = Vec::new();

        // Phase 1: Collect results from completed inferences
        while let Ok(result) = self.rx.try_recv() {
            // PID 0 = orchestration result
            if result.pid == 0 {
                if let Ok(suggestion) = result.suggestion {
                    let orch_actions = self.handle_orchestration_result(&suggestion, sessions);
                    actions.extend(orch_actions);
                }
                continue;
            }

            self.inflight.remove(&result.pid);
            self.cooldown.insert(result.pid, Instant::now());

            match result.suggestion {
                Ok(suggestion) => {
                    // Check if a deny rule overrides the brain
                    let session = sessions.iter().find(|s| s.pid == result.pid);
                    if !matches!(
                        (result.target.as_ref(), session),
                        (Some(target), Some(session)) if target.matches(session)
                    ) && result.target.is_some()
                    {
                        actions.push((
                            result.pid,
                            "Brain suggestion expired because the session changed".into(),
                        ));
                        continue;
                    }
                    if let Some(session) = session {
                        let deny_match = rules::evaluate(deny_rules, session);
                        if let Some(dm) = &deny_match {
                            if dm.action == RuleAction::Deny {
                                // Log the override so the brain learns deny-rule boundaries
                                super::decisions::log_decision(
                                    result.pid,
                                    session.display_name(),
                                    session.pending_tool_name.as_deref(),
                                    session.pending_tool_input.as_deref(),
                                    &suggestion,
                                    "deny_rule_override",
                                    Some(session),
                                    DecisionType::Session,
                                    None,
                                );
                                actions.push((
                                    result.pid,
                                    format!(
                                        "Brain suggested {}, but deny rule '{}' overrides",
                                        suggestion.action.label(),
                                        dm.rule_name,
                                    ),
                                ));
                                continue;
                            }
                        }
                    }

                    if self.config.auto_mode {
                        // Auto mode: check adaptive confidence threshold before executing.
                        // If the brain's track record for this tool is poor, require
                        // higher confidence before auto-executing.
                        if let Some(session) = session {
                            let tool_name = session.pending_tool_name.as_deref();
                            let threshold =
                                super::decisions::adaptive_threshold(tool_name).unwrap_or(0.6);
                            if suggestion.confidence < threshold {
                                // Below adaptive threshold — demote to advisory mode
                                super::decisions::log_decision(
                                    result.pid,
                                    session.display_name(),
                                    tool_name,
                                    session.pending_tool_input.as_deref(),
                                    &suggestion,
                                    "deferred_low_confidence",
                                    Some(session),
                                    DecisionType::Session,
                                    None,
                                );
                                self.pending.insert(
                                    result.pid,
                                    PendingBrainSuggestion::bound(
                                        suggestion,
                                        result.target.clone(),
                                    ),
                                );
                                continue;
                            }
                        }

                        // Check for file conflicts before executing
                        if let Some(session) = session {
                            if let Some(conflict_msg) = check_file_conflicts(session, sessions) {
                                // Demote to advisory — require user confirmation
                                super::decisions::log_decision(
                                    result.pid,
                                    session.display_name(),
                                    session.pending_tool_name.as_deref(),
                                    session.pending_tool_input.as_deref(),
                                    &suggestion,
                                    "deferred_file_conflict",
                                    Some(session),
                                    DecisionType::Session,
                                    None,
                                );
                                let mut flagged = suggestion.clone();
                                flagged.reasoning =
                                    format!("{} [CONFLICT: {}]", flagged.reasoning, conflict_msg);
                                self.pending.insert(
                                    result.pid,
                                    PendingBrainSuggestion::bound(flagged, result.target.clone()),
                                );
                                actions
                                    .push((result.pid, format!("File conflict: {conflict_msg}")));
                                continue;
                            }
                        }

                        // Confidence meets threshold — execute
                        if let Some(session) = session {
                            match &suggestion.action {
                                RuleAction::Route { target_pid } => {
                                    let target = sessions.iter().find(|s| s.pid == *target_pid);
                                    if let Some(target) = target {
                                        match self.execute_route(session, target) {
                                            Ok(msg) => {
                                                super::decisions::log_decision(
                                                    result.pid,
                                                    session.display_name(),
                                                    session.pending_tool_name.as_deref(),
                                                    session.pending_tool_input.as_deref(),
                                                    &suggestion,
                                                    "auto",
                                                    Some(session),
                                                    DecisionType::Session,
                                                    None,
                                                );
                                                actions.push((result.pid, msg));
                                            }
                                            Err(e) => actions
                                                .push((result.pid, format!("Route error: {e}"))),
                                        }
                                    } else {
                                        actions.push((
                                            result.pid,
                                            format!(
                                                "Route error: target PID {} not found",
                                                target_pid
                                            ),
                                        ));
                                    }
                                }
                                RuleAction::Spawn { .. } => {
                                    // Enforce max_sessions limit
                                    if sessions.len() >= self.config.max_sessions {
                                        actions.push((
                                            result.pid,
                                            format!(
                                                "Spawn blocked: {} sessions active (max {})",
                                                sessions.len(),
                                                self.config.max_sessions
                                            ),
                                        ));
                                    } else {
                                        let rule_match = suggestion_to_rule_match(&suggestion);
                                        match rules::execute(&rule_match, session) {
                                            Ok(msg) => {
                                                super::decisions::log_decision(
                                                    result.pid,
                                                    session.display_name(),
                                                    session.pending_tool_name.as_deref(),
                                                    session.pending_tool_input.as_deref(),
                                                    &suggestion,
                                                    "auto",
                                                    Some(session),
                                                    DecisionType::Session,
                                                    None,
                                                );
                                                actions.push((result.pid, msg));
                                            }
                                            Err(e) => actions
                                                .push((result.pid, format!("Spawn error: {e}"))),
                                        }
                                    }
                                }
                                _ => {
                                    let rule_match = suggestion_to_rule_match(&suggestion);
                                    match rules::execute(&rule_match, session) {
                                        Ok(msg) => {
                                            super::decisions::log_decision(
                                                result.pid,
                                                session.display_name(),
                                                session.pending_tool_name.as_deref(),
                                                session.pending_tool_input.as_deref(),
                                                &suggestion,
                                                "auto",
                                                Some(session),
                                                DecisionType::Session,
                                                None,
                                            );
                                            actions.push((result.pid, msg));
                                        }
                                        Err(e) => {
                                            actions.push((result.pid, format!("Brain error: {e}")))
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // Advisory mode: store for user confirmation
                        self.pending.insert(
                            result.pid,
                            PendingBrainSuggestion::bound(suggestion, result.target.clone()),
                        );
                    }
                }
                Err(e) => {
                    crate::logger::log(
                        "BRAIN",
                        &format!("Inference failed for PID {}: {e}", result.pid),
                    );
                }
            }
        }

        // Phase 2: Spawn inference for eligible sessions
        for session in sessions {
            if !matches!(
                session.status,
                SessionStatus::NeedsInput | SessionStatus::WaitingInput
            ) {
                continue;
            }

            if self.inflight.contains(&session.pid) {
                continue;
            }

            if let Some(last) = self.cooldown.get(&session.pid) {
                if last.elapsed().as_secs() < COOLDOWN_SECS {
                    continue;
                }
            }

            // Already have a pending suggestion for this PID
            if self.pending.contains_key(&session.pid) {
                continue;
            }

            self.spawn_inference(session, sessions);
        }

        // Phase 3: Orchestration evaluation (less frequent)
        let orch_actions = self.maybe_orchestrate(sessions);
        actions.extend(orch_actions);

        actions
    }

    fn spawn_inference(&mut self, session: &CodexSession, all_sessions: &[CodexSession]) {
        let pid = session.pid;
        let target = BrainTargetIdentity::from_session(session);
        let config = self.config.clone();
        let tx = self.tx.clone();

        // Build context on the main thread (reads JSONL files)
        let mut brain_ctx =
            context::build_context(session, all_sessions, config.max_context_tokens);

        // Load distilled preferences: prefer project-specific, fall back to global
        if let Some(prefs) = super::decisions::load_preferences_for_project(session.display_name())
        {
            brain_ctx.preference_summary = super::decisions::format_preference_summary(&prefs);
        }

        // Inject raw few-shot examples (outcome-weighted retrieval)
        // When preferences exist, reduce few-shot count to save context budget
        let few_shot_limit = if brain_ctx.preference_summary.is_empty() {
            config.few_shot_count
        } else {
            // Preferences cover learned patterns; fewer raw examples needed
            config.few_shot_count.min(3)
        };

        if few_shot_limit > 0 {
            let similar = super::decisions::retrieve_similar(
                session.pending_tool_name.as_deref(),
                session.display_name(),
                few_shot_limit,
                Some(DecisionType::Session),
            );
            brain_ctx.few_shot_examples = super::decisions::format_few_shot_examples(&similar);
        }

        let prompt = context::format_brain_prompt(&brain_ctx);

        self.inflight.insert(pid);

        std::thread::spawn(move || {
            let suggestion = super::client::infer(&config, &prompt);
            let _ = tx.send(BrainResult {
                pid,
                target: Some(target),
                suggestion,
            });
        });
    }

    /// Execute a route: read source's recent transcript, summarize via LLM,
    /// and either send directly (if target is waiting) or queue in mailbox.
    fn execute_route(
        &self,
        source: &CodexSession,
        target: &CodexSession,
    ) -> Result<String, String> {
        // Build source context to get recent transcript
        let source_ctx = context::build_context(
            source,
            std::slice::from_ref(source),
            self.config.max_context_tokens,
        );

        // Summarize for target's task
        let summary = super::client::summarize_for_routing(
            &self.config,
            &source_ctx.recent_transcript,
            source.display_name(),
            target.display_name(),
        )?;

        // If target is waiting for input, deliver directly; otherwise queue in mailbox
        if target.status == SessionStatus::WaitingInput {
            rules::execute_route(source, target, &summary, "brain")
        } else {
            super::mailbox::enqueue(source.pid, source.display_name(), target.pid, &summary);
            Ok(format!(
                "Brain: queued message from {} → {} (mailbox, target is {})",
                source.display_name(),
                target.display_name(),
                target.status,
            ))
        }
    }

    /// Accept a pending brain suggestion (user pressed 'b').
    pub fn accept(&mut self, pid: u32, session: &CodexSession) -> Option<String> {
        let pending = self.pending.remove(&pid)?;
        if !pending.matches(session) {
            return Some("Brain suggestion expired because the session changed".into());
        }
        let suggestion = pending.suggestion;
        let rule_match = suggestion_to_rule_match(&suggestion);
        match rules::execute(&rule_match, session) {
            Ok(msg) => Some(msg),
            Err(e) => Some(format!("Brain execute error: {e}")),
        }
    }

    /// Reject a pending brain suggestion (user pressed 'B').
    pub fn reject(&mut self, pid: u32) -> Option<BrainSuggestion> {
        self.pending.remove(&pid).map(|pending| pending.suggestion)
    }

    /// Check for sessions with saturated context and auto-restart them.
    /// Saves a checkpoint and spawns a fresh session with the summary as prompt.
    pub fn maybe_restart_saturated(
        &mut self,
        sessions: &[CodexSession],
        lifecycle: &crate::config::LifecycleConfig,
        is_idle: bool,
    ) -> Vec<(u32, String)> {
        if !lifecycle.auto_restart {
            return Vec::new();
        }
        if lifecycle.restart_only_when_idle && !is_idle {
            return Vec::new();
        }

        let threshold = lifecycle.restart_threshold_pct / 100.0;
        let mut actions = Vec::new();

        for session in sessions {
            if self.restarted_pids.contains(&session.pid) {
                continue;
            }
            if session.context_max == 0 {
                continue;
            }
            let pct = session.context_tokens as f64 / session.context_max as f64;
            if pct < threshold {
                continue;
            }
            // Don't restart if actively waiting for tool approval
            if session.status == SessionStatus::NeedsInput {
                continue;
            }

            // Build summary for checkpoint
            let ctx = context::build_context(
                session,
                std::slice::from_ref(session),
                self.config.max_context_tokens,
            );
            let summary = format!(
                "Continue the work from a previous session that hit context limits.\n\
                 Project: {}\nModel: {}\nCost so far: ${:.2}\n\n\
                 Recent context:\n{}",
                session.display_name(),
                session.model,
                session.cost_usd,
                &ctx.recent_transcript,
            );

            // Save checkpoint
            if let Err(e) = save_checkpoint(&session.session_id, session, &summary) {
                crate::logger::log("BRAIN", &format!("Checkpoint save failed: {e}"));
            }

            // Spawn fresh session
            match crate::terminals::launch_session(&session.cwd, Some(&summary), None) {
                Ok(msg) => {
                    self.restarted_pids.insert(session.pid);
                    actions.push((
                        session.pid,
                        format!(
                            "Lifecycle: restarted {} (context at {:.0}%) → {msg}",
                            session.display_name(),
                            pct * 100.0,
                        ),
                    ));
                }
                Err(e) => {
                    actions.push((
                        session.pid,
                        format!(
                            "Lifecycle: restart failed for {}: {e}",
                            session.display_name()
                        ),
                    ));
                }
            }
        }

        actions
    }

    /// Clear pending suggestions for PIDs that are no longer in NeedsInput/WaitingInput.
    pub fn cleanup(&mut self, sessions: &[CodexSession]) {
        let active_pids: HashSet<u32> = sessions.iter().map(|s| s.pid).collect();
        self.pending.retain(|pid, pending| {
            active_pids.contains(pid)
                && sessions.iter().any(|s| {
                    s.pid == *pid
                        && matches!(
                            s.status,
                            SessionStatus::NeedsInput | SessionStatus::WaitingInput
                        )
                        && pending.matches(s)
                })
        });
        self.inflight.retain(|pid| active_pids.contains(pid));
    }

    /// Run orchestration evaluation: ask the brain if any cross-session actions
    /// should be taken (spawn, route, terminate). Runs less frequently than
    /// per-session advisory (every orchestrate_interval_secs).
    pub fn maybe_orchestrate(&mut self, sessions: &[CodexSession]) -> Vec<(u32, String)> {
        if !self.config.orchestrate || !self.config.auto_mode {
            return Vec::new();
        }

        if sessions.len() < 2 {
            return Vec::new();
        }

        // Check interval
        let interval = std::time::Duration::from_secs(self.config.orchestrate_interval_secs);
        if let Some(last) = self.last_orchestrate {
            if last.elapsed() < interval {
                return Vec::new();
            }
        }

        if self.orchestrate_inflight {
            return Vec::new();
        }

        self.last_orchestrate = Some(Instant::now());
        self.orchestrate_inflight = true;

        // Build orchestration prompt with all sessions
        let prompt = build_orchestration_prompt(sessions, &self.config);
        let config = self.config.clone();
        let tx = self.tx.clone();

        // Use PID 0 as sentinel for orchestration results
        std::thread::spawn(move || {
            let suggestion = super::client::infer(&config, &prompt);
            let _ = tx.send(BrainResult {
                pid: 0,
                target: None,
                suggestion,
            });
        });

        Vec::new()
    }

    /// Check if a result is an orchestration response (pid == 0).
    pub fn handle_orchestration_result(
        &mut self,
        suggestion: &BrainSuggestion,
        sessions: &[CodexSession],
    ) -> Vec<(u32, String)> {
        self.orchestrate_inflight = false;
        let mut actions = Vec::new();

        // Log orchestration decisions with the Orchestration type.
        // Use the action label as the user_action so "deny" (no action) isn't
        // misleadingly logged as "auto" (executed).
        let project = sessions
            .first()
            .map(|s| s.display_name().to_string())
            .unwrap_or_default();
        let orch_user_action = if suggestion.action == RuleAction::Deny {
            "deny"
        } else {
            "auto"
        };
        super::decisions::log_decision(
            0,
            &project,
            None,
            None,
            suggestion,
            orch_user_action,
            None,
            DecisionType::Orchestration,
            None,
        );

        // The orchestration response may suggest multiple actions.
        // For now, handle the primary action.
        match &suggestion.action {
            RuleAction::Spawn { .. } => {
                if sessions.len() >= self.config.max_sessions {
                    actions.push((
                        0,
                        format!(
                            "Orchestrate: spawn blocked ({} sessions, max {})",
                            sessions.len(),
                            self.config.max_sessions
                        ),
                    ));
                } else {
                    let rule_match = suggestion_to_rule_match(suggestion);
                    // Need a dummy session for execute — use first available
                    if let Some(session) = sessions.first() {
                        match rules::execute(&rule_match, session) {
                            Ok(msg) => actions.push((0, format!("Orchestrate: {msg}"))),
                            Err(e) => actions.push((0, format!("Orchestrate error: {e}"))),
                        }
                    }
                }
            }
            RuleAction::Route { target_pid } => {
                // Find source (most recently active) and target
                if let Some(target) = sessions.iter().find(|s| s.pid == *target_pid) {
                    if let Some(source) = sessions
                        .iter()
                        .find(|s| s.pid != *target_pid && s.status == SessionStatus::WaitingInput)
                    {
                        match self.execute_route(source, target) {
                            Ok(msg) => actions.push((0, format!("Orchestrate: {msg}"))),
                            Err(e) => actions.push((0, format!("Orchestrate error: {e}"))),
                        }
                    }
                }
            }
            RuleAction::Terminate => {
                // Orchestration terminate — brain should include which PID in reasoning
                actions.push((
                    0,
                    format!(
                        "Orchestrate: terminate suggested — {}",
                        suggestion.reasoning
                    ),
                ));
            }
            _ => {
                // approve/deny/send don't make sense at the orchestration level
                actions.push((
                    0,
                    format!(
                        "Orchestrate: {} — {}",
                        suggestion.action.label(),
                        suggestion.reasoning
                    ),
                ));
            }
        }

        actions
    }
}

/// Build the orchestration prompt from the prompt library.
fn build_orchestration_prompt(sessions: &[CodexSession], _config: &BrainConfig) -> String {
    let session_map = context::format_global_session_map_public(sessions);
    let template = super::prompts::load(super::prompts::ORCHESTRATION);
    super::prompts::expand(
        &template,
        &[
            ("session_count", &sessions.len().to_string()),
            ("session_map", &session_map),
        ],
    )
}

/// Check if a Write/Edit/NotebookEdit tool call targets a file that another
/// running session has in its `files_modified` map.
/// Returns a warning message if a conflict is found, or None if clear.
fn check_file_conflicts(session: &CodexSession, all_sessions: &[CodexSession]) -> Option<String> {
    let tool = session.pending_tool_name.as_deref()?;
    if !matches!(tool, "Write" | "Edit" | "NotebookEdit") {
        return None;
    }

    let input = session.pending_tool_input.as_deref()?;

    // Extract file path from the tool input.
    // Write/Edit inputs typically start with or contain the absolute file path.
    let target_path = extract_file_path(input)?;

    for other in all_sessions {
        if other.pid == session.pid {
            continue;
        }
        if other.files_modified.contains_key(&target_path) {
            return Some(format!(
                "{} is also being modified by session {} (PID {})",
                target_path,
                other.display_name(),
                other.pid,
            ));
        }
    }
    None
}

/// Extract a file path from tool input. Looks for the first path-like token
/// (absolute path starting with / or relative path with a file extension).
fn extract_file_path(input: &str) -> Option<String> {
    // Try to find an absolute path
    for token in input.split_whitespace() {
        let cleaned = token.trim_matches('"').trim_matches('\'');
        if cleaned.starts_with('/') && cleaned.len() > 1 {
            return Some(cleaned.to_string());
        }
    }
    // Try to find a relative path with common extensions
    for token in input.split_whitespace() {
        let cleaned = token.trim_matches('"').trim_matches('\'');
        if cleaned.contains('.')
            && (cleaned.starts_with("./")
                || cleaned.starts_with("src/")
                || cleaned.starts_with("tests/")
                || cleaned.contains(".rs")
                || cleaned.contains(".ts")
                || cleaned.contains(".py")
                || cleaned.contains(".js")
                || cleaned.contains(".toml")
                || cleaned.contains(".json")
                || cleaned.contains(".md"))
        {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn save_checkpoint(session_id: &str, session: &CodexSession, summary: &str) -> Result<(), String> {
    let home = std::env::var("HOME").map_err(|e| format!("HOME not set: {e}"))?;
    let dir = std::path::PathBuf::from(home)
        .join(".codexctl")
        .join("brain")
        .join("checkpoints");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir failed: {e}"))?;

    let path = dir.join(format!("{session_id}.md"));
    let content = format!(
        "# Session Checkpoint\n\n\
         - Session: {}\n\
         - Project: {}\n\
         - Model: {}\n\
         - Cost: ${:.2}\n\
         - Context: {}/{}  ({:.0}%)\n\n\
         ## Summary\n\n{}\n",
        session_id,
        session.display_name(),
        session.model,
        session.cost_usd,
        session.context_tokens,
        session.context_max,
        if session.context_max > 0 {
            session.context_tokens as f64 / session.context_max as f64 * 100.0
        } else {
            0.0
        },
        summary,
    );
    std::fs::write(&path, content).map_err(|e| format!("write failed: {e}"))?;
    Ok(())
}

fn suggestion_to_rule_match(suggestion: &BrainSuggestion) -> RuleMatch {
    RuleMatch {
        rule_name: format!(
            "brain ({}% confidence)",
            (suggestion.confidence * 100.0) as u32
        ),
        action: suggestion.action.clone(),
        message: suggestion.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ApprovalEvidence, ApprovalObservation, RawSession, TelemetryStatus};
    use codexctl_core::terminals::Terminal;

    fn make_config() -> BrainConfig {
        BrainConfig {
            enabled: true,
            endpoint: "http://localhost:11434/api/generate".into(),
            model: "test".into(),
            auto_mode: false,
            timeout_ms: 1000,
            max_context_tokens: 1000,
            few_shot_count: 5,
            max_sessions: 10,
            orchestrate: false,
            orchestrate_interval_secs: 30,
            test_runners: crate::config::default_test_runners(),
        }
    }

    fn make_session(pid: u32, status: SessionStatus) -> CodexSession {
        let raw = RawSession {
            pid,
            session_id: "test".into(),
            cwd: "/tmp/test".into(),
            started_at: 0,
        };
        let mut s = CodexSession::from_raw(raw);
        s.status = status;
        s.telemetry_status = TelemetryStatus::Available;
        s.pending_tool_name = Some("Bash".into());
        s
    }

    fn suggestion(action: RuleAction, confidence: f64) -> BrainSuggestion {
        BrainSuggestion {
            action,
            message: None,
            reasoning: "fixture".into(),
            confidence,
            suggested_at: 0,
        }
    }

    fn inject(engine: &BrainEngine, pid: u32, suggestion: Result<BrainSuggestion, String>) {
        engine
            .tx
            .send(BrainResult {
                pid,
                target: None,
                suggestion,
            })
            .unwrap();
    }

    fn confirmed_shell_session(call_id: &str, command: &str) -> CodexSession {
        let mut session = make_session(100, SessionStatus::NeedsInput);
        session.session_id = "session-100".into();
        session.tty = "pts/100".into();
        session.pending_tool_name = Some("exec_command".into());
        session.pending_tool_call_id = Some(call_id.into());
        session.pending_tool_input = Some(command.into());
        session.approval = ApprovalObservation::Confirmed(ApprovalEvidence {
            session_id: session.session_id.clone(),
            tty: session.tty.clone(),
            call_id: call_id.into(),
            tool: "exec_command".into(),
            command: command.into(),
            backend: Terminal::Tmux,
            target: "main:1.0".into(),
            prompt_pattern_version: 1,
            prompt_fingerprint: 42,
        });
        session
    }

    #[test]
    fn stale_inference_result_is_discarded_when_call_changes() {
        let mut engine = BrainEngine::new(make_config());
        let original = confirmed_shell_session("call-a", "cargo test");
        let replacement = confirmed_shell_session("call-b", "cargo clippy");
        engine
            .tx
            .send(BrainResult {
                pid: original.pid,
                target: Some(BrainTargetIdentity::from_session(&original)),
                suggestion: Ok(suggestion(RuleAction::Approve, 1.0)),
            })
            .unwrap();

        let actions = engine.tick(&[replacement], &[]);

        assert_eq!(actions.len(), 1);
        assert!(actions[0].1.contains("expired"));
        assert!(engine.pending.is_empty());
    }

    #[test]
    fn stale_pending_suggestion_is_not_accepted_for_replacement_call() {
        let mut engine = BrainEngine::new(make_config());
        let original = confirmed_shell_session("call-a", "cargo test");
        let replacement = confirmed_shell_session("call-b", "cargo clippy");
        engine.pending.insert(
            original.pid,
            PendingBrainSuggestion::bound(
                suggestion(RuleAction::Approve, 1.0),
                Some(BrainTargetIdentity::from_session(&original)),
            ),
        );

        let message = engine.accept(original.pid, &replacement).unwrap();

        assert!(message.contains("expired"));
        assert!(engine.pending.is_empty());
    }

    #[test]
    fn cleanup_removes_suggestion_bound_to_replaced_call() {
        let mut engine = BrainEngine::new(make_config());
        let original = confirmed_shell_session("call-a", "cargo test");
        let replacement = confirmed_shell_session("call-b", "cargo clippy");
        engine.pending.insert(
            original.pid,
            PendingBrainSuggestion::bound(
                suggestion(RuleAction::Approve, 1.0),
                Some(BrainTargetIdentity::from_session(&original)),
            ),
        );

        engine.cleanup(&[replacement]);

        assert!(engine.pending.is_empty());
    }

    #[test]
    fn advisory_mode_queues_high_confidence_suggestion() {
        let mut engine = BrainEngine::new(make_config());
        let session = make_session(100, SessionStatus::Processing);
        inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 1.0)));

        assert!(engine.tick(&[session], &[]).is_empty());
        assert!(engine.pending.contains_key(&100));
    }

    #[test]
    fn auto_mode_defers_low_confidence_suggestion() {
        let mut config = make_config();
        config.auto_mode = true;
        let mut engine = BrainEngine::new(config);
        let mut session = make_session(100, SessionStatus::Processing);
        session.pending_tool_name = Some("stress-test-unknown-tool".into());
        inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 0.0)));

        assert!(engine.tick(&[session], &[]).is_empty());
        assert!(engine.pending.contains_key(&100));
    }

    #[test]
    fn deny_rule_overrides_auto_mode() {
        let mut config = make_config();
        config.auto_mode = true;
        let mut engine = BrainEngine::new(config);
        let session = make_session(100, SessionStatus::Processing);
        let deny = crate::rules::AutoRule::new("deny all".into(), RuleAction::Deny);
        inject(&engine, 100, Ok(suggestion(RuleAction::Approve, 1.0)));

        let actions = engine.tick(&[session], &[deny]);

        assert!(actions[0].1.contains("deny rule"));
        assert!(!engine.pending.contains_key(&100));
    }

    #[test]
    fn auto_spawn_respects_max_sessions() {
        let mut config = make_config();
        config.auto_mode = true;
        config.max_sessions = 1;
        let mut engine = BrainEngine::new(config);
        let session = make_session(100, SessionStatus::Processing);
        inject(
            &engine,
            100,
            Ok(suggestion(
                RuleAction::Spawn {
                    prompt: "run tests".into(),
                    cwd: "/tmp/test".into(),
                },
                1.0,
            )),
        );

        let actions = engine.tick(&[session], &[]);

        assert!(actions[0].1.contains("Spawn blocked"));
    }

    #[test]
    fn auto_route_requires_active_target() {
        let mut config = make_config();
        config.auto_mode = true;
        let mut engine = BrainEngine::new(config);
        let session = make_session(100, SessionStatus::Processing);
        inject(
            &engine,
            100,
            Ok(suggestion(RuleAction::Route { target_pid: 999 }, 1.0)),
        );

        let actions = engine.tick(&[session], &[]);

        assert!(actions[0].1.contains("target PID 999 not found"));
    }

    #[test]
    fn inference_failure_creates_no_pending_or_task_state() {
        let mut engine = BrainEngine::new(make_config());
        let session = make_session(100, SessionStatus::Processing);
        inject(&engine, 100, Err("endpoint unavailable".into()));

        assert!(engine.tick(&[session], &[]).is_empty());
        assert!(engine.pending.is_empty());
        assert!(engine.inflight.is_empty());
    }

    #[test]
    fn engine_creates_without_panic() {
        let _engine = BrainEngine::new(make_config());
    }

    #[test]
    fn suggestion_to_rule_match_format() {
        let suggestion = BrainSuggestion {
            action: RuleAction::Approve,
            message: None,
            reasoning: "safe".into(),
            confidence: 0.95,
            suggested_at: 0,
        };
        let rm = suggestion_to_rule_match(&suggestion);
        assert_eq!(rm.action, RuleAction::Approve);
        assert!(rm.rule_name.contains("95%"));
    }

    #[test]
    fn cleanup_removes_stale_pending() {
        let mut engine = BrainEngine::new(make_config());
        engine.pending.insert(
            999,
            PendingBrainSuggestion::unbound(BrainSuggestion {
                action: RuleAction::Approve,
                message: None,
                reasoning: "test".into(),
                confidence: 0.9,
                suggested_at: 0,
            }),
        );

        // PID 999 not in sessions list → should be cleaned up
        engine.cleanup(&[]);
        assert!(engine.pending.is_empty());
    }

    #[test]
    fn cleanup_keeps_active_pending() {
        let mut engine = BrainEngine::new(make_config());
        let session = make_session(100, SessionStatus::NeedsInput);
        engine.pending.insert(
            100,
            PendingBrainSuggestion::unbound(BrainSuggestion {
                action: RuleAction::Approve,
                message: None,
                reasoning: "test".into(),
                confidence: 0.9,
                suggested_at: 0,
            }),
        );

        engine.cleanup(&[session]);
        assert!(engine.pending.contains_key(&100));
    }

    #[test]
    fn file_conflict_detected_same_file() {
        let mut s1 = make_session(100, SessionStatus::NeedsInput);
        s1.pending_tool_name = Some("Write".into());
        s1.pending_tool_input = Some("/tmp/project/src/main.rs".into());

        let mut s2 = make_session(200, SessionStatus::Processing);
        s2.files_modified
            .insert("/tmp/project/src/main.rs".to_string(), 1);

        let result = check_file_conflicts(&s1, &[s1.clone(), s2]);
        assert!(result.is_some());
        assert!(result.unwrap().contains("main.rs"));
    }

    #[test]
    fn file_conflict_no_conflict_different_files() {
        let mut s1 = make_session(100, SessionStatus::NeedsInput);
        s1.pending_tool_name = Some("Edit".into());
        s1.pending_tool_input = Some("/tmp/project/src/lib.rs".into());

        let mut s2 = make_session(200, SessionStatus::Processing);
        s2.files_modified
            .insert("/tmp/project/src/main.rs".to_string(), 1);

        let result = check_file_conflicts(&s1, &[s1.clone(), s2]);
        assert!(result.is_none());
    }

    #[test]
    fn file_conflict_no_self_conflict() {
        let mut s1 = make_session(100, SessionStatus::NeedsInput);
        s1.pending_tool_name = Some("Write".into());
        s1.pending_tool_input = Some("/tmp/project/src/main.rs".into());
        s1.files_modified
            .insert("/tmp/project/src/main.rs".to_string(), 1);

        let result = check_file_conflicts(&s1, &[s1.clone()]);
        assert!(result.is_none());
    }

    #[test]
    fn file_conflict_skips_non_write_tools() {
        let mut s1 = make_session(100, SessionStatus::NeedsInput);
        s1.pending_tool_name = Some("Bash".into());
        s1.pending_tool_input = Some("/tmp/project/src/main.rs".into());

        let mut s2 = make_session(200, SessionStatus::Processing);
        s2.files_modified
            .insert("/tmp/project/src/main.rs".to_string(), 1);

        let result = check_file_conflicts(&s1, &[s1.clone(), s2]);
        assert!(result.is_none());
    }

    #[test]
    fn extract_file_path_absolute() {
        assert_eq!(
            extract_file_path("/tmp/project/src/main.rs"),
            Some("/tmp/project/src/main.rs".into())
        );
    }

    #[test]
    fn extract_file_path_relative() {
        assert_eq!(extract_file_path("src/main.rs"), Some("src/main.rs".into()));
    }

    #[test]
    fn extract_file_path_none_for_plain_text() {
        assert_eq!(extract_file_path("hello world"), None);
    }

    #[test]
    fn lifecycle_below_threshold_no_restart() {
        let config = crate::config::LifecycleConfig {
            auto_restart: true,
            restart_threshold_pct: 90.0,
            restart_only_when_idle: false,
        };
        let mut engine = BrainEngine::new(make_config());
        let mut s = make_session(100, SessionStatus::Processing);
        s.context_tokens = 50_000;
        s.context_max = 200_000;

        let actions = engine.maybe_restart_saturated(&[s], &config, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn lifecycle_above_threshold_flags_restart() {
        let config = crate::config::LifecycleConfig {
            auto_restart: true,
            restart_threshold_pct: 90.0,
            restart_only_when_idle: false,
        };
        let mut engine = BrainEngine::new(make_config());
        let mut s = make_session(100, SessionStatus::Processing);
        s.context_tokens = 190_000;
        s.context_max = 200_000;

        let actions = engine.maybe_restart_saturated(&[s], &config, true);
        assert!(!actions.is_empty());
        assert!(actions[0].1.contains("Lifecycle:"));
    }

    #[test]
    fn lifecycle_no_restart_loop() {
        let config = crate::config::LifecycleConfig {
            auto_restart: true,
            restart_threshold_pct: 90.0,
            restart_only_when_idle: false,
        };
        let mut engine = BrainEngine::new(make_config());
        engine.restarted_pids.insert(100);
        let mut s = make_session(100, SessionStatus::Processing);
        s.context_tokens = 190_000;
        s.context_max = 200_000;

        let actions = engine.maybe_restart_saturated(&[s], &config, true);
        assert!(actions.is_empty(), "Should skip already-restarted PID");
    }

    #[test]
    fn lifecycle_respects_idle_only() {
        let config = crate::config::LifecycleConfig {
            auto_restart: true,
            restart_threshold_pct: 90.0,
            restart_only_when_idle: true,
        };
        let mut engine = BrainEngine::new(make_config());
        let mut s = make_session(100, SessionStatus::Processing);
        s.context_tokens = 190_000;
        s.context_max = 200_000;

        let actions = engine.maybe_restart_saturated(&[s], &config, false);
        assert!(actions.is_empty());
    }

    #[test]
    fn lifecycle_disabled_no_restart() {
        let config = crate::config::LifecycleConfig::default();
        let mut engine = BrainEngine::new(make_config());
        let mut s = make_session(100, SessionStatus::Processing);
        s.context_tokens = 190_000;
        s.context_max = 200_000;

        let actions = engine.maybe_restart_saturated(&[s], &config, true);
        assert!(actions.is_empty());
    }

    #[test]
    fn reject_removes_and_returns_suggestion() {
        let mut engine = BrainEngine::new(make_config());
        engine.pending.insert(
            100,
            PendingBrainSuggestion::unbound(BrainSuggestion {
                action: RuleAction::Approve,
                message: None,
                reasoning: "test".into(),
                confidence: 0.9,
                suggested_at: 0,
            }),
        );

        let rejected = engine.reject(100);
        assert!(rejected.is_some());
        assert!(engine.pending.is_empty());
    }
}
