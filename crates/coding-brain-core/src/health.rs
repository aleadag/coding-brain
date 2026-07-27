#![allow(dead_code)]

use crate::provider::AgentProvider;
use crate::session::AgentSession;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSessionCount {
    pub provider: AgentProvider,
    pub count: usize,
}

/// Aggregate live discovery by provider without exposing session identities.
pub fn provider_session_counts(sessions: &[AgentSession]) -> Vec<ProviderSessionCount> {
    [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ]
    .into_iter()
    .map(|provider| ProviderSessionCount {
        provider,
        count: sessions
            .iter()
            .filter(|session| session.provider == provider)
            .count(),
    })
    .collect()
}

pub fn format_provider_session_counts(counts: &[ProviderSessionCount]) -> String {
    counts
        .iter()
        .map(|entry| format!("{}: {}", entry.provider.label(), entry.count))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Tunable thresholds for the health checks below. Lives here (not in
/// `config`) because health is a foundational `coding-brain-core` concern; the
/// binary's `config::Config` re-exports this type and parses TOML overrides
/// against it.
#[derive(Debug, Clone)]
pub struct HealthThresholds {
    pub loop_max_calls: u32,
    pub context_critical_pct: f64,
    pub context_warning_pct: f64,
    pub decay_compaction_pct: f64,
    pub error_accel_factor: f64,
    pub repetition_threshold: u32,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self {
            loop_max_calls: 10,
            context_critical_pct: 90.0,
            context_warning_pct: 80.0,
            decay_compaction_pct: 50.0,
            error_accel_factor: 2.0,
            repetition_threshold: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub icon: &'static str,
    pub name: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// Run all health checks against a session. Returns warnings sorted by severity.
pub fn check_session(session: &AgentSession, t: &HealthThresholds) -> Vec<HealthCheck> {
    let mut checks = Vec::new();

    if let Some(c) = check_loop_detection(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_context_saturation(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_cognitive_decay(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_proactive_compaction(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_error_acceleration(session, t) {
        checks.push(c);
    }
    if let Some(c) = check_repetition(session, t) {
        checks.push(c);
    }

    // Sort: Critical first, then Warning, then Info
    checks.sort_by_key(|c| match c.severity {
        Severity::Critical => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    });

    checks
}

/// Return the most severe health icon for display in the table, or empty string if healthy.
pub fn status_icon(session: &AgentSession, t: &HealthThresholds) -> &'static str {
    let checks = check_session(session, t);
    match checks.first() {
        Some(c) if c.severity == Severity::Critical => c.icon,
        Some(c) if c.severity == Severity::Warning => c.icon,
        _ => "",
    }
}

/// Format a compact health summary for the status bar.
pub fn format_health_summary(sessions: &[AgentSession], t: &HealthThresholds) -> Option<String> {
    let mut warnings = 0;
    let mut criticals = 0;
    let mut worst_msg = String::new();

    for session in sessions {
        for check in check_session(session, t) {
            match check.severity {
                Severity::Critical => {
                    criticals += 1;
                    if worst_msg.is_empty() {
                        worst_msg =
                            format!("{} {}: {}", check.icon, session.display_name(), check.name);
                    }
                }
                Severity::Warning => warnings += 1,
                Severity::Info => {}
            }
        }
    }

    if criticals == 0 && warnings == 0 {
        return None;
    }

    let count = criticals + warnings;
    Some(format!(
        "{} health issue{} | {}",
        count,
        if count == 1 { "" } else { "s" },
        worst_msg,
    ))
}

// ────────────────────────────────────────────────────────────────────────────
// Individual health checks
// ────────────────────────────────────────────────────────────────────────────

/// Detect tool error loops — same tool failing repeatedly.
fn check_loop_detection(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    if !session.last_tool_error {
        return None;
    }

    let max_calls = session
        .tool_usage
        .values()
        .map(|ts| ts.calls)
        .max()
        .unwrap_or(0);

    if max_calls >= t.loop_max_calls && session.last_tool_error {
        let tool_name = session
            .tool_usage
            .iter()
            .max_by_key(|(_, ts)| ts.calls)
            .map(|(name, _)| name.as_str())
            .unwrap_or("?");

        Some(HealthCheck {
            icon: "🔄",
            name: "looping",
            severity: Severity::Warning,
            message: format!(
                "{tool_name} called {max_calls} times with recent errors — may be stuck in a retry loop.",
            ),
        })
    } else {
        None
    }
}

/// Detect context window saturation.
fn check_context_saturation(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let pct = f64::from(session.context_percent()?);

    if pct > t.context_critical_pct {
        Some(HealthCheck {
            icon: "🧠",
            name: "context full",
            severity: Severity::Critical,
            message: format!(
                "Context at {:.0}% — session may degrade or auto-compact. \
                 Consider spawning a fresh session.",
                pct,
            ),
        })
    } else if pct > t.context_warning_pct {
        Some(HealthCheck {
            icon: "🧠",
            name: "context high",
            severity: Severity::Warning,
            message: format!("Context at {:.0}% — approaching limit.", pct),
        })
    } else {
        None
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Cognitive decay checks
// ────────────────────────────────────────────────────────────────────────────

/// Compute a composite cognitive decay score (0-100) from multiple signals.
pub fn compute_decay_score(session: &AgentSession, _t: &HealthThresholds) -> u32 {
    let mut score: f64 = 0.0;

    // Context contribution: 0-40 points (linear from 40% to 100%)
    let ctx_pct = session.context_percent().map(f64::from).unwrap_or(0.0);
    if ctx_pct > 40.0 {
        score += ((ctx_pct - 40.0) / 60.0) * 40.0;
    }

    // Error acceleration contribution: 0-25 points
    if let Some(baseline) = session.baseline_error_rate {
        if baseline > 0.0 && session.error_counts_per_window.len() >= 2 {
            let recent_count = session.error_counts_per_window.len().min(3);
            let recent: f64 = session
                .error_counts_per_window
                .iter()
                .rev()
                .take(recent_count)
                .sum::<u32>() as f64
                / recent_count as f64;
            let ratio = recent / baseline;
            score += (ratio - 1.0).clamp(0.0, 1.0) * 25.0;
        }
    }

    // Repetition contribution: 0-15 points
    let max_rereads = session
        .file_reads_since_edit
        .values()
        .copied()
        .max()
        .unwrap_or(0);
    if max_rereads >= 2 {
        score += ((max_rereads as f64 - 1.0) / 4.0).min(1.0) * 15.0;
    }

    (score.round() as u32).min(100)
}

/// Composite cognitive decay check — wraps the decay score into a HealthCheck.
fn check_cognitive_decay(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let score = compute_decay_score(session, t);
    if score >= 80 {
        Some(HealthCheck {
            icon: "⊘",
            name: "severe decay",
            severity: Severity::Critical,
            message: format!(
                "Decay score {}/100 — session is severely compromised. Restart with fresh context.",
                score,
            ),
        })
    } else if score >= 60 {
        Some(HealthCheck {
            icon: "◉",
            name: "significant decay",
            severity: Severity::Warning,
            message: format!(
                "Decay score {}/100 — consider restarting. Generate a state transfer summary first.",
                score,
            ),
        })
    } else if score >= 30 {
        Some(HealthCheck {
            icon: "◐",
            name: "early decay",
            severity: Severity::Info,
            message: format!(
                "Decay score {}/100 — consider /compact with preservation notes.",
                score,
            ),
        })
    } else {
        None
    }
}

/// Suggest proactive compaction at moderate context usage (before degradation starts).
fn check_proactive_compaction(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let pct = f64::from(session.context_percent()?);
    if pct > t.decay_compaction_pct && pct <= t.context_warning_pct {
        Some(HealthCheck {
            icon: "📋",
            name: "consider compact",
            severity: Severity::Info,
            message: format!(
                "Context at {:.0}% — research shows degradation begins here. Consider /compact.",
                pct,
            ),
        })
    } else {
        None
    }
}

/// Detect error rate acceleration — errors are increasing over time.
fn check_error_acceleration(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let baseline = session.baseline_error_rate?;
    if baseline <= 0.0 || session.error_counts_per_window.len() < 4 {
        return None;
    }

    let recent_count = session.error_counts_per_window.len().min(3);
    let recent: f64 = session
        .error_counts_per_window
        .iter()
        .rev()
        .take(recent_count)
        .sum::<u32>() as f64
        / recent_count as f64;
    let ratio = recent / baseline;

    if ratio > t.error_accel_factor {
        Some(HealthCheck {
            icon: "⚠",
            name: "error acceleration",
            severity: Severity::Warning,
            message: format!(
                "Error rate is {:.1}x baseline — agent may be stuck or confused.",
                ratio,
            ),
        })
    } else {
        None
    }
}

/// Detect file re-reads without intervening edits — possible confusion or looping.
fn check_repetition(session: &AgentSession, t: &HealthThresholds) -> Option<HealthCheck> {
    let max_rereads = session
        .file_reads_since_edit
        .values()
        .copied()
        .max()
        .unwrap_or(0);

    if max_rereads >= t.repetition_threshold {
        let file = session
            .file_reads_since_edit
            .iter()
            .max_by_key(|(_, v)| *v)
            .map(|(k, _)| {
                // Show just filename
                k.rsplit('/').next().unwrap_or(k)
            })
            .unwrap_or("?");
        Some(HealthCheck {
            icon: "🔁",
            name: "repetition",
            severity: Severity::Warning,
            message: format!(
                "{} read {} times without editing — agent may be looping.",
                file, max_rereads,
            ),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AgentProvider;
    use crate::session::{RawAgentSession, SessionStatus, TelemetryStatus};

    fn defaults() -> HealthThresholds {
        HealthThresholds::default()
    }

    fn provider_session(provider: AgentProvider, id: &str) -> AgentSession {
        AgentSession::from_raw(RawAgentSession {
            provider,
            pid: 1,
            process_start_identity: Some(1),
            session_id: id.into(),
            cwd: "/work".into(),
            started_at: 1,
        })
    }

    #[test]
    fn provider_session_counts_are_aggregate_stable_and_identity_free() {
        let sessions = vec![
            provider_session(AgentProvider::Claude, "secret-claude-id"),
            provider_session(AgentProvider::Codex, "secret-codex-id"),
            provider_session(AgentProvider::Claude, "another-secret-id"),
        ];

        let counts = provider_session_counts(&sessions);

        assert_eq!(
            counts,
            vec![
                ProviderSessionCount {
                    provider: AgentProvider::Codex,
                    count: 1,
                },
                ProviderSessionCount {
                    provider: AgentProvider::Claude,
                    count: 2,
                },
                ProviderSessionCount {
                    provider: AgentProvider::Antigravity,
                    count: 0,
                },
            ]
        );
        let summary = format_provider_session_counts(&counts);
        assert_eq!(summary, "Codex: 1, Claude: 2, Antigravity: 0");
        assert!(!summary.contains("secret"));
    }

    fn make_session() -> AgentSession {
        let raw = RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 1,
            process_start_identity: None,
            session_id: "test".into(),
            cwd: "/tmp/test".into(),
            started_at: 0,
        };
        let mut s = AgentSession::from_raw(raw);
        s.status = SessionStatus::Processing;
        s.telemetry_status = TelemetryStatus::Available;
        s.model = "gpt-5.5".into();
        s
    }

    #[test]
    fn healthy_session_no_warnings() {
        let s = make_session();
        assert!(check_session(&s, &defaults()).is_empty());
    }

    #[test]
    fn context_saturation_critical() {
        let mut s = make_session();
        s.context_pressure = Some(95);
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context full" && c.severity == Severity::Critical)
        );
    }

    #[test]
    fn context_saturation_warning() {
        let mut s = make_session();
        s.context_pressure = Some(85);
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context high" && c.severity == Severity::Warning)
        );
    }

    #[test]
    fn status_icon_returns_worst() {
        let mut s = make_session();
        s.context_pressure = Some(95);
        assert_eq!(status_icon(&s, &defaults()), "🧠");
    }

    #[test]
    fn status_icon_empty_when_healthy() {
        let s = make_session();
        assert_eq!(status_icon(&s, &defaults()), "");
    }

    #[test]
    fn sorted_by_severity() {
        let mut s = make_session();
        s.context_pressure = Some(95);
        s.file_reads_since_edit.insert("src/main.rs".into(), 4);
        let checks = check_session(&s, &defaults());
        assert!(checks.len() >= 2);
        assert_eq!(checks[0].severity, Severity::Critical);
    }

    #[test]
    fn custom_context_thresholds() {
        let mut s = make_session();
        s.context_pressure = Some(85);

        // With defaults, this triggers warning
        let checks = check_session(&s, &defaults());
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context high" && c.severity == Severity::Warning)
        );

        // With tighter threshold (84%), 85% usage should trigger critical
        let mut tight = defaults();
        tight.context_critical_pct = 84.0;
        let checks = check_session(&s, &tight);
        assert!(
            checks
                .iter()
                .any(|c| c.name == "context full" && c.severity == Severity::Critical)
        );
    }

    #[test]
    fn retained_context_checks_use_derived_pressure() {
        let mut warning = make_session();
        warning.context_pressure = Some(85);
        let check = check_context_saturation(&warning, &defaults()).unwrap();
        assert_eq!(check.name, "context high");
        assert_eq!(check.severity, Severity::Warning);

        let mut critical = make_session();
        critical.context_pressure = Some(95);
        let check = check_context_saturation(&critical, &defaults()).unwrap();
        assert_eq!(check.name, "context full");
        assert_eq!(check.severity, Severity::Critical);
    }

    #[test]
    fn retained_proactive_compaction_uses_derived_pressure() {
        let mut session = make_session();
        session.context_pressure = Some(55);

        let check = check_proactive_compaction(&session, &defaults()).unwrap();

        assert_eq!(check.name, "consider compact");
        assert_eq!(check.severity, Severity::Info);
    }

    #[test]
    fn retained_error_acceleration_snapshot() {
        let mut session = make_session();
        session.baseline_error_rate = Some(1.0);
        session.error_counts_per_window = vec![1, 1, 1, 2, 3, 4];

        let check = check_error_acceleration(&session, &defaults()).unwrap();

        assert_eq!(check.name, "error acceleration");
        assert_eq!(check.severity, Severity::Warning);
        assert_eq!(
            check.message,
            "Error rate is 3.0x baseline — agent may be stuck or confused."
        );
    }

    #[test]
    fn retained_reread_detection_snapshot() {
        let mut session = make_session();
        session
            .file_reads_since_edit
            .insert("/tmp/test/src/main.rs".into(), 4);

        let check = check_repetition(&session, &defaults()).unwrap();

        assert_eq!(check.name, "repetition");
        assert_eq!(check.severity, Severity::Warning);
        assert_eq!(
            check.message,
            "main.rs read 4 times without editing — agent may be looping."
        );
    }

    #[test]
    fn retained_decay_weights_are_not_renormalized() {
        let mut session = make_session();
        session.context_pressure = Some(100);
        session.baseline_error_rate = Some(1.0);
        session.error_counts_per_window = vec![1, 1, 1, 2, 2, 2];
        session
            .file_reads_since_edit
            .insert("/tmp/test/main.rs".into(), 5);

        assert_eq!(compute_decay_score(&session, &defaults()), 80);
    }

    // ── Cognitive decay tests ────────────────────────────────────────

    #[test]
    fn proactive_compaction_fires_at_50pct() {
        let mut s = make_session();
        s.context_pressure = Some(55);
        let check = check_proactive_compaction(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "consider compact");
        assert_eq!(c.severity, Severity::Info);
    }

    #[test]
    fn proactive_compaction_silent_below_threshold() {
        let mut s = make_session();
        s.context_pressure = Some(35);
        assert!(check_proactive_compaction(&s, &defaults()).is_none());
    }

    #[test]
    fn error_acceleration_detects_increase() {
        let mut s = make_session();
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 2, 3, 4]; // rising
        let check = check_error_acceleration(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "error acceleration");
        assert_eq!(c.severity, Severity::Warning);
    }

    #[test]
    fn error_acceleration_silent_when_stable() {
        let mut s = make_session();
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 1]; // stable
        assert!(check_error_acceleration(&s, &defaults()).is_none());
    }

    #[test]
    fn repetition_detects_rereads() {
        let mut s = make_session();
        s.file_reads_since_edit
            .insert("/tmp/test/src/main.rs".into(), 4);
        let check = check_repetition(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "repetition");
        assert_eq!(c.severity, Severity::Warning);
        assert!(c.message.contains("main.rs"));
    }

    #[test]
    fn repetition_silent_below_threshold() {
        let mut s = make_session();
        s.file_reads_since_edit.insert("/tmp/test/foo.rs".into(), 2);
        assert!(check_repetition(&s, &defaults()).is_none());
    }

    #[test]
    fn decay_score_zero_for_fresh_session() {
        let s = make_session();
        assert_eq!(compute_decay_score(&s, &defaults()), 0);
    }

    #[test]
    fn decay_score_context_only_contribution() {
        let mut s = make_session();
        s.context_pressure = Some(70);
        let score = compute_decay_score(&s, &defaults());
        // 70% context: (70-40)/60 * 40 = 20 points
        assert_eq!(score, 20);
    }

    #[test]
    fn decay_score_high_for_saturated_session() {
        let mut s = make_session();
        s.context_pressure = Some(90);
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 3, 4, 5]; // accelerating
        s.file_reads_since_edit
            .insert("/tmp/test/main.rs".into(), 5); // repetition
        let score = compute_decay_score(&s, &defaults());
        assert!(score >= 60, "expected >= 60, got {score}");
    }

    #[test]
    fn cognitive_decay_check_critical_at_80() {
        let mut s = make_session();
        s.context_pressure = Some(100);
        s.baseline_error_rate = Some(1.0);
        s.error_counts_per_window = vec![1, 1, 1, 3, 4, 5]; // → 25 error points
        s.file_reads_since_edit
            .insert("/tmp/test/main.rs".into(), 5); // → 15 repetition points
        let check = check_cognitive_decay(&s, &defaults());
        assert!(check.is_some());
        let c = check.unwrap();
        assert_eq!(c.name, "severe decay");
        assert_eq!(c.severity, Severity::Critical);
        assert!(c.icon == "⊘");
    }
}
