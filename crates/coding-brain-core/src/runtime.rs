//! Runtime contract between the Coding Brain binary and TUI.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::brain_activity::{
    ActivitySnapshot, CorrectionDisposition, SessionTarget, SnapshotLimits,
};
use crate::provider::AgentProvider;
use crate::terminals::TerminalSessionAction;

// ============================================================================
// Brain
// ============================================================================

/// Mirrors the binary's `brain::GateMode` without depending on the brain
/// crate. Persisted as the lowercased label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainGateMode {
    On,
    Off,
    Auto,
}

impl BrainGateMode {
    /// Canonical lowercase label — the form persisted to
    /// the Coding Brain state root and emitted by the TUI status messages.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Auto => "auto",
        }
    }
}

impl std::fmt::Display for BrainGateMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single past brain decision, projected for display.
///
/// The first six fields are the common shape used by `BrainView::recent_decisions`.
/// The remaining fields support the Brain Review surface (`BrainReviewView`); they
/// are `Option`-wrapped + `#[serde(default)]` so older `BrainView` callers can
/// keep treating them as opaque.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionSummary {
    #[serde(default)]
    pub provider: AgentProvider,
    pub id: String,
    pub timestamp: String,
    pub action: String,
    pub confidence: Option<f64>,
    pub project: Option<String>,
    pub tool: Option<String>,
    /// PID of the session this decision belongs to. Used by counterfactual
    /// analysis to pair decisions with their subsequent outcome from the
    /// same session.
    #[serde(default)]
    pub pid: u32,

    /// Tool input string when the decision was about a specific command.
    #[serde(default)]
    pub command: Option<String>,
    /// Brain's free-form rationale for the suggestion.
    #[serde(default)]
    pub reasoning: Option<String>,
    /// What the user did with the suggestion — `"accept"`, `"reject"`,
    /// `"deny_rule_override"`, etc.
    #[serde(default)]
    pub user_action: Option<String>,
    /// Why the user overrode the brain (if applicable).
    #[serde(default)]
    pub override_reason: Option<String>,
    /// Wall-clock latency of the brain decision in milliseconds.
    #[serde(default)]
    pub brain_decision_ms: Option<u64>,
    /// Whether the operator has marked this decision as canonical (teaching
    /// material). `None` for records written before the field existed.
    #[serde(default)]
    pub canonical: Option<bool>,
    /// Cache hit flag — served from the few-shot store without an LLM call.
    /// `None` before instrumentation.
    #[serde(default)]
    pub cache_hit: Option<bool>,
    /// Cost in USD when this decision was made (context snapshot).
    #[serde(default)]
    pub cost_usd: Option<f64>,
    /// Model that produced the suggestion.
    #[serde(default)]
    pub model: Option<String>,
    /// Resolved outcome category, when known. Mirrors the variants of the
    /// binary's `brain::decisions::DecisionOutcome` enum, flattened to a
    /// string so the contract doesn't pull the enum upward.
    #[serde(default)]
    pub outcome_kind: Option<String>,
    /// Free-form detail for failure outcomes, such as the error message.
    #[serde(default)]
    pub outcome_detail: Option<String>,
    /// Epoch seconds when the brain suggestion was first surfaced. Used by
    /// time-to-correct analysis. `None` for records pre-instrumentation or
    /// passive observations.
    #[serde(default)]
    pub suggested_at: Option<u64>,
    /// Epoch seconds when the user acted on the suggestion. `None` for
    /// passive observations or records still in flight.
    #[serde(default)]
    pub resolved_at: Option<u64>,
}

impl DecisionSummary {
    /// Whether the user agreed with the brain (or the call was auto-executed).
    /// Mirrors `brain::decisions::DecisionRecord::is_positive`.
    pub fn is_positive(&self) -> bool {
        matches!(
            self.user_action.as_deref(),
            Some("accept" | "auto" | "user_approve" | "rule_approve")
        )
    }

    /// Whether the user disagreed with the brain. Mirrors
    /// `brain::decisions::DecisionRecord::is_negative`.
    pub fn is_negative(&self) -> bool {
        matches!(
            self.user_action.as_deref(),
            Some("reject" | "deny_rule_override" | "rule_deny" | "conflict_deny")
        )
    }
}

/// One entry in the Brain Review queue — a decision worth showing the operator
/// for canonical-marking review, with a reason and a priority score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewItemSummary {
    pub decision: DecisionSummary,
    /// Free-form rationale for why this decision was queued for review.
    pub reason: String,
    /// Priority score (higher = more important to review first).
    pub score: f64,
}

// ============================================================================
// Coding Brain primary runtime
// ============================================================================

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScorecardSummary {
    pub total_decisions: usize,
    pub brain_decisions: usize,
    pub correct_decisions: usize,
    pub accuracy_pct: f64,
    pub abstentions: usize,
    pub dangerous_false_approvals: usize,
    pub override_rate_pct: f64,
    pub canonical_decisions: usize,
    pub risk_tiers: Vec<RiskTierSummary>,
    pub providers: Vec<ProviderScoreSummary>,
    pub latency: LatencySummary,
    pub cache: CacheSummary,
    pub counterfactuals: CounterfactualSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderScoreSummary {
    pub provider: AgentProvider,
    pub decisions: usize,
    pub correct: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RiskTierSummary {
    pub tier: String,
    pub samples: usize,
    pub correct: usize,
    pub false_approvals: usize,
    pub false_denials: usize,
    pub override_rate_pct: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatencySummary {
    pub samples: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub mean_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheSummary {
    pub instrumented: usize,
    pub hits: usize,
    pub misses: usize,
    pub hit_rate_pct: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CounterfactualSummary {
    pub brain_was_right: usize,
    pub user_was_right: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndpointHealth {
    pub reachable: bool,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrectionInput {
    pub activity_id: String,
    pub disposition: CorrectionDisposition,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionRequest {
    pub target: SessionTarget,
    pub action: TerminalSessionAction,
}

pub trait BrainSource: Send + Sync {
    fn snapshot(&self, limits: SnapshotLimits) -> Result<ActivitySnapshot, String>;
    fn review_queue(&self) -> Result<Vec<ReviewItemSummary>, String>;
    fn scorecard(&self) -> Result<ScorecardSummary, String>;
    fn gate_mode(&self) -> BrainGateMode;
    fn endpoint_health(&self) -> EndpointHealth;
}

pub trait BrainActions: Send + Sync {
    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String>;
    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String>;
    fn send_session_action(&self, request: SessionActionRequest) -> Result<(), String>;
    fn poll_recovery(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone)]
pub struct BrainRuntime {
    pub source: Arc<dyn BrainSource>,
    pub actions: Arc<dyn BrainActions>,
    pub navigation: Arc<dyn SessionNavigation>,
}

impl BrainRuntime {
    pub fn new(source: Arc<dyn BrainSource>, actions: Arc<dyn BrainActions>) -> Self {
        Self {
            source,
            actions,
            navigation: Arc::new(UnavailableSessionNavigation),
        }
    }

    pub fn with_navigation(mut self, navigation: Arc<dyn SessionNavigation>) -> Self {
        self.navigation = navigation;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainEffect {
    SwitchToSession(SessionTarget),
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl ExternalCommand {
    pub fn new<P, I, S>(program: P, args: I) -> Self
    where
        P: Into<PathBuf>,
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationPlan {
    External(ExternalCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationError {
    Unavailable(String),
    QueryFailed(String),
    TimedOut,
    OutputTooLarge { limit: usize },
    Malformed(String),
    MissingIdentity { index: usize, field: &'static str },
    IdentityProjectionFailed(String),
    DiscoveryFailed(String),
    NoMatch,
    Ambiguous { matches: usize },
}

impl std::fmt::Display for NavigationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "Agent Deck unavailable: {detail}"),
            Self::QueryFailed(detail) => write!(formatter, "Agent Deck query failed: {detail}"),
            Self::TimedOut => formatter.write_str("Agent Deck query timed out"),
            Self::OutputTooLarge { limit } => {
                write!(formatter, "Agent Deck output exceeded {limit} bytes")
            }
            Self::Malformed(detail) => write!(formatter, "invalid Agent Deck JSON: {detail}"),
            Self::MissingIdentity { index, field } => {
                write!(formatter, "Agent Deck session {index} is missing {field}")
            }
            Self::IdentityProjectionFailed(detail) => {
                write!(formatter, "session identity projection failed: {detail}")
            }
            Self::DiscoveryFailed(detail) => {
                write!(formatter, "provider session discovery failed: {detail}")
            }
            Self::NoMatch => formatter.write_str("no matching Agent Deck session"),
            Self::Ambiguous { matches } => {
                write!(
                    formatter,
                    "Agent Deck session match is ambiguous ({matches} matches)"
                )
            }
        }
    }
}

impl std::error::Error for NavigationError {}

pub trait SessionNavigation: Send + Sync {
    fn resolve(&self, target: &SessionTarget) -> Result<NavigationPlan, NavigationError>;
    fn focus_fallback(&self, target: &SessionTarget) -> Result<(), String>;
}

struct UnavailableSessionNavigation;

impl SessionNavigation for UnavailableSessionNavigation {
    fn resolve(&self, _target: &SessionTarget) -> Result<NavigationPlan, NavigationError> {
        Err(NavigationError::Unavailable(
            "optional navigator is not configured".into(),
        ))
    }

    fn focus_fallback(&self, _target: &SessionTarget) -> Result<(), String> {
        Err("session navigation is not configured".into())
    }
}

#[derive(Default)]
pub struct MockBrainRuntime {
    pub activity_snapshot: ActivitySnapshot,
    pub review_queue: Vec<ReviewItemSummary>,
    pub scorecard: ScorecardSummary,
    pub endpoint_health: EndpointHealth,
    pub gate_mode: std::sync::Mutex<Option<BrainGateMode>>,
    pub actions_log: std::sync::Mutex<Vec<MockBrainAction>>,
    pub session_action_error: std::sync::Mutex<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockBrainAction {
    PollRecovery,
    RecordCorrection(CorrectionInput),
    MarkCanonical {
        decision_id: String,
        note: Option<String>,
    },
    SessionAction(SessionActionRequest),
}

impl MockBrainRuntime {
    pub fn into_runtime(self) -> BrainRuntime {
        let runtime = Arc::new(self);
        BrainRuntime::new(runtime.clone(), runtime)
    }

    pub fn actions(&self) -> Vec<MockBrainAction> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .clone()
    }
}

impl BrainSource for MockBrainRuntime {
    fn snapshot(&self, _limits: SnapshotLimits) -> Result<ActivitySnapshot, String> {
        Ok(self.activity_snapshot.clone())
    }

    fn review_queue(&self) -> Result<Vec<ReviewItemSummary>, String> {
        Ok(self.review_queue.clone())
    }

    fn scorecard(&self) -> Result<ScorecardSummary, String> {
        Ok(self.scorecard.clone())
    }

    fn gate_mode(&self) -> BrainGateMode {
        self.gate_mode
            .lock()
            .expect("brain gate_mode poisoned")
            .unwrap_or(BrainGateMode::On)
    }

    fn endpoint_health(&self) -> EndpointHealth {
        self.endpoint_health.clone()
    }
}

impl BrainActions for MockBrainRuntime {
    fn poll_recovery(&self) -> Vec<String> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::PollRecovery);
        Vec::new()
    }

    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::RecordCorrection(correction));
        Ok(())
    }

    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::MarkCanonical {
                decision_id: decision_id.into(),
                note,
            });
        Ok(())
    }

    fn send_session_action(&self, request: SessionActionRequest) -> Result<(), String> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::SessionAction(request));
        self.session_action_error
            .lock()
            .expect("brain session_action_error poisoned")
            .clone()
            .map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AgentProvider;
    use crate::terminals::TerminalSessionAction;

    #[test]
    fn brain_runtime_records_exact_correction_and_canonical_inputs() {
        let mock = Arc::new(MockBrainRuntime::default());
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let correction = CorrectionInput {
            activity_id: "activity-42".into(),
            disposition: crate::brain_activity::CorrectionDisposition::BrainWrong,
            note: Some("wrong project".into()),
        };

        runtime
            .actions
            .record_correction(correction.clone())
            .unwrap();
        runtime
            .actions
            .mark_canonical("decision-42", Some("teach this".into()))
            .unwrap();

        assert_eq!(
            mock.actions(),
            vec![
                MockBrainAction::RecordCorrection(correction),
                MockBrainAction::MarkCanonical {
                    decision_id: "decision-42".into(),
                    note: Some("teach this".into()),
                },
            ]
        );
    }

    #[test]
    fn brain_runtime_records_exact_session_action_request() {
        let mock = Arc::new(MockBrainRuntime::default());
        let runtime = BrainRuntime::new(mock.clone(), mock.clone());
        let request = SessionActionRequest {
            target: SessionTarget {
                provider: AgentProvider::Claude,
                session_id: "session-42".into(),
                turn_id: Some("turn-7".into()),
                tool_use_id: None,
                project_id: crate::project::ProjectId::Stable("project-1".into()),
                cwd: "/work/project".into(),
                provider_hints: Vec::new(),
                provenance: crate::brain_activity::SessionTargetProvenance::Structured,
            },
            action: TerminalSessionAction::Continue,
        };

        runtime
            .actions
            .send_session_action(request.clone())
            .unwrap();

        assert_eq!(
            mock.actions(),
            vec![MockBrainAction::SessionAction(request)]
        );
    }

    #[test]
    fn brain_runtime_exposes_only_brain_source_and_actions() {
        let runtime = MockBrainRuntime::default().into_runtime();

        assert_eq!(runtime.source.gate_mode(), BrainGateMode::On);
        assert!(
            runtime
                .source
                .snapshot(crate::brain_activity::SnapshotLimits::default())
                .unwrap()
                .recent
                .is_empty()
        );
    }
}
