//! Runtime contract between the Coding Brain binary and TUI.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::brain_activity::{
    ActivitySnapshot, CorrectionDisposition, SessionTarget, SnapshotLimits,
};
use crate::provider::AgentProvider;
use crate::review_state::{
    BrainReviewProjection, MAX_REVIEW_KEYS, ReviewDisposition, ReviewKey, ReviewMutation,
    ReviewMutationRequest, ReviewMutationResult, ReviewRequestError, ReviewSurface, ReviewTarget,
    SurfaceReviewProjection,
};
use crate::terminals::{GuardedActionFailureCategory, TerminalSessionAction};

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

    pub fn canonical_available(&self) -> bool {
        !self.id.trim().is_empty()
    }

    pub fn review_source_identity(&self) -> Vec<u8> {
        if self.canonical_available() {
            return self.id.as_bytes().to_vec();
        }

        let mut hash = Sha256::new();
        hash.update(b"legacy-review:v1");
        hash_review_field(&mut hash, self.provider.as_str().as_bytes());
        hash_review_field(&mut hash, self.timestamp.as_bytes());
        hash_review_field(&mut hash, &self.pid.to_be_bytes());
        hash_review_field(
            &mut hash,
            self.project.as_deref().unwrap_or_default().as_bytes(),
        );
        hash_optional_review_field(&mut hash, self.tool.as_deref().map(str::as_bytes));
        hash_optional_review_field(&mut hash, self.command.as_deref().map(str::as_bytes));
        hash_review_field(&mut hash, self.action.as_bytes());
        hash_review_field(
            &mut hash,
            &self.confidence.unwrap_or_default().to_bits().to_be_bytes(),
        );
        hash.finalize().to_vec()
    }

    pub fn review_display_id(&self) -> String {
        if self.canonical_available() {
            return self.id.clone();
        }
        hex_identity(&self.review_source_identity())
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

impl ReviewItemSummary {
    pub fn canonical_available(&self) -> bool {
        self.decision.canonical_available()
    }

    pub fn review_display_id(&self) -> String {
        self.decision.review_display_id()
    }
}

fn hash_review_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn hash_optional_review_field(hash: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hash.update((value.len() as u64 + 1).to_be_bytes());
            hash.update([1]);
            hash.update(value);
        }
        None => {
            hash.update(1_u64.to_be_bytes());
            hash.update([0]);
        }
    }
}

fn hex_identity(identity: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(identity.len() * 2);
    for byte in identity {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
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
pub struct SessionActionTarget {
    pub provider: AgentProvider,
    pub session_id: String,
    pub project_id: crate::project::ProjectId,
    pub cwd: std::path::PathBuf,
    pub provenance: crate::brain_activity::SessionTargetProvenance,
}

impl From<SessionTarget> for SessionActionTarget {
    fn from(target: SessionTarget) -> Self {
        Self {
            provider: target.provider,
            session_id: target.session_id,
            project_id: target.project_id,
            cwd: target.cwd,
            provenance: target.provenance,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionAttempt {
    pub attempt_id: String,
    pub target: SessionActionTarget,
}

impl SessionActionAttempt {
    pub fn new(target: SessionTarget) -> Self {
        Self {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            target: target.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionPreflightRequest {
    pub attempt: SessionActionAttempt,
}

impl SessionActionPreflightRequest {
    pub fn new(target: SessionTarget) -> Self {
        Self {
            attempt: SessionActionAttempt::new(target),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionCapability {
    Allow,
    Deny,
    Continue,
    ManualText,
}

impl SessionActionCapability {
    pub fn permits(self, action: &TerminalSessionAction) -> bool {
        matches!(
            (self, action),
            (Self::Allow, TerminalSessionAction::Allow)
                | (Self::Deny, TerminalSessionAction::Deny)
                | (Self::Continue, TerminalSessionAction::Continue)
                | (Self::ManualText, TerminalSessionAction::Text(_))
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionAvailability {
    pub attempt: SessionActionAttempt,
    pub capabilities: Vec<SessionActionCapability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionFailureCategory {
    AuthorityUnavailable,
    ExactSessionUnavailable,
    ExactSessionAmbiguous,
    Guarded(GuardedActionFailureCategory),
}

impl SessionActionFailureCategory {
    pub fn rule_id(self) -> String {
        match self {
            Self::AuthorityUnavailable => "session_action_authority_unavailable".into(),
            Self::ExactSessionUnavailable => "session_action_session_unavailable".into(),
            Self::ExactSessionAmbiguous => "session_action_session_ambiguous".into(),
            Self::Guarded(category) => format!("session_action_{}", category.rule_suffix()),
        }
    }

    pub fn safe_message(self) -> &'static str {
        match self {
            Self::AuthorityUnavailable => "Session action authority is unavailable",
            Self::ExactSessionUnavailable => "No exact live provider session for action",
            Self::ExactSessionAmbiguous => "Exact live provider session is ambiguous",
            Self::Guarded(category) => category.safe_message(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionFailure {
    pub category: SessionActionFailureCategory,
    pub diagnostic_persisted: bool,
}

impl SessionActionFailure {
    pub fn safe_message(&self) -> &'static str {
        self.category.safe_message()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionRequest {
    pub attempt: SessionActionAttempt,
    pub action: TerminalSessionAction,
}

#[derive(Debug, Clone, Default)]
pub struct BrainRefresh {
    pub snapshot: ActivitySnapshot,
    pub review_queue: Vec<ReviewItemSummary>,
    pub scorecard: ScorecardSummary,
    pub review_state: BrainReviewProjection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAlignmentError {
    ItemCount {
        surface: ReviewSurface,
        visible_items: usize,
        review_targets: usize,
    },
    TargetSurface {
        expected: ReviewSurface,
        actual: ReviewSurface,
        index: usize,
    },
    RowIdentity {
        surface: ReviewSurface,
        index: usize,
    },
    EmptyDisplayId {
        surface: ReviewSurface,
        index: usize,
    },
    EmptyMembers {
        surface: ReviewSurface,
        index: usize,
    },
    MemberOverlap {
        surface: ReviewSurface,
        index: usize,
    },
    DuplicateMember {
        surface: ReviewSurface,
        index: usize,
    },
    MemberCount {
        surface: ReviewSurface,
        index: usize,
        visible_members: usize,
        review_members: usize,
    },
    NewCount {
        surface: ReviewSurface,
        declared: usize,
        actual: usize,
    },
    ReviewedCount {
        surface: ReviewSurface,
        declared: usize,
        actual: usize,
    },
}

impl std::fmt::Display for ReviewAlignmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ItemCount {
                surface,
                visible_items,
                review_targets,
            } => write!(
                formatter,
                "{} review alignment mismatch: {visible_items} visible items, {review_targets} targets",
                surface.as_str()
            ),
            Self::TargetSurface {
                expected,
                actual,
                index,
            } => write!(
                formatter,
                "{} review target {index} is tagged for {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::RowIdentity { surface, index } => write!(
                formatter,
                "{} review target {index} does not identify its visible row",
                surface.as_str()
            ),
            Self::EmptyDisplayId { surface, index } => write!(
                formatter,
                "{} review target {index} has an empty display identity",
                surface.as_str()
            ),
            Self::EmptyMembers { surface, index } => write!(
                formatter,
                "{} review target {index} has no member identities",
                surface.as_str()
            ),
            Self::MemberOverlap { surface, index } => write!(
                formatter,
                "{} review target {index} contains a member in both states",
                surface.as_str()
            ),
            Self::DuplicateMember { surface, index } => write!(
                formatter,
                "{} review target {index} reuses a member identity",
                surface.as_str()
            ),
            Self::MemberCount {
                surface,
                index,
                visible_members,
                review_members,
            } => write!(
                formatter,
                "{} review target {index} has {review_members} members for {visible_members} visible occurrences",
                surface.as_str()
            ),
            Self::NewCount {
                surface,
                declared,
                actual,
            } => write!(
                formatter,
                "{} review projection declares {declared} new members but contains {actual}",
                surface.as_str()
            ),
            Self::ReviewedCount {
                surface,
                declared,
                actual,
            } => write!(
                formatter,
                "{} review projection declares {declared} reviewed members but contains {actual}",
                surface.as_str()
            ),
        }
    }
}

impl std::error::Error for ReviewAlignmentError {}

impl SurfaceReviewProjection {
    pub fn from_items(
        surface: ReviewSurface,
        revision: u64,
        items: Vec<ReviewTarget>,
        visible_items: usize,
        new_count: usize,
        reviewed_count: usize,
        last_archive_count: usize,
    ) -> Result<Self, ReviewAlignmentError> {
        let projection = Self {
            revision,
            items,
            new_count,
            reviewed_count,
            last_archive_count,
        };
        projection.validate(surface, visible_items)?;
        Ok(projection)
    }

    fn validate(
        &self,
        surface: ReviewSurface,
        visible_items: usize,
    ) -> Result<(), ReviewAlignmentError> {
        if self.items.len() != visible_items {
            return Err(ReviewAlignmentError::ItemCount {
                surface,
                visible_items,
                review_targets: self.items.len(),
            });
        }
        let mut all_members = BTreeSet::new();
        let mut actual_new = 0;
        let mut actual_reviewed = 0;
        for (index, target) in self.items.iter().enumerate() {
            if target.surface != surface {
                return Err(ReviewAlignmentError::TargetSurface {
                    expected: surface,
                    actual: target.surface,
                    index,
                });
            }
            if target.display_id.trim().is_empty() {
                return Err(ReviewAlignmentError::EmptyDisplayId { surface, index });
            }
            let review_members = target.new_member_keys.len() + target.reviewed_member_keys.len();
            if review_members == 0 {
                return Err(ReviewAlignmentError::EmptyMembers { surface, index });
            }
            if target
                .new_member_keys
                .iter()
                .any(|key| target.reviewed_member_keys.contains(key))
            {
                return Err(ReviewAlignmentError::MemberOverlap { surface, index });
            }
            if surface != ReviewSurface::Attention && review_members != 1 {
                return Err(ReviewAlignmentError::MemberCount {
                    surface,
                    index,
                    visible_members: 1,
                    review_members,
                });
            }
            for key in target
                .new_member_keys
                .iter()
                .chain(&target.reviewed_member_keys)
            {
                if !all_members.insert(*key) {
                    return Err(ReviewAlignmentError::DuplicateMember { surface, index });
                }
            }
            actual_new += target.new_member_keys.len();
            actual_reviewed += target.reviewed_member_keys.len();
        }
        if self.new_count < actual_new {
            return Err(ReviewAlignmentError::NewCount {
                surface,
                declared: self.new_count,
                actual: actual_new,
            });
        }
        if self.reviewed_count < actual_reviewed {
            return Err(ReviewAlignmentError::ReviewedCount {
                surface,
                declared: self.reviewed_count,
                actual: actual_reviewed,
            });
        }
        Ok(())
    }
}

impl BrainRefresh {
    pub fn validate_review_alignment(&self) -> Result<(), ReviewAlignmentError> {
        self.review_state
            .attention
            .validate(ReviewSurface::Attention, self.snapshot.attention.len())?;
        self.review_state
            .review
            .validate(ReviewSurface::Review, self.review_queue.len())?;
        self.review_state.diagnostics.validate(
            ReviewSurface::Diagnostics,
            self.snapshot.diagnostic_events.len(),
        )?;
        self.review_state
            .recent
            .validate(ReviewSurface::Recent, self.snapshot.recent.len())?;
        for (index, (item, target)) in self
            .snapshot
            .attention
            .iter()
            .zip(&self.review_state.attention.items)
            .enumerate()
        {
            if target.display_id != item.review_display_id() {
                return Err(ReviewAlignmentError::RowIdentity {
                    surface: ReviewSurface::Attention,
                    index,
                });
            }
            let review_members = target.new_member_keys.len() + target.reviewed_member_keys.len();
            if review_members != item.occurrences {
                return Err(ReviewAlignmentError::MemberCount {
                    surface: ReviewSurface::Attention,
                    index,
                    visible_members: item.occurrences,
                    review_members,
                });
            }
        }
        for (index, (item, target)) in self
            .review_queue
            .iter()
            .zip(&self.review_state.review.items)
            .enumerate()
        {
            if target.display_id != item.review_display_id() {
                return Err(ReviewAlignmentError::RowIdentity {
                    surface: ReviewSurface::Review,
                    index,
                });
            }
        }
        validate_activity_row_identities(
            ReviewSurface::Diagnostics,
            &self.snapshot.diagnostic_events,
            &self.review_state.diagnostics.items,
        )?;
        validate_activity_row_identities(
            ReviewSurface::Recent,
            &self.snapshot.recent,
            &self.review_state.recent.items,
        )?;
        Ok(())
    }
}

fn validate_activity_row_identities(
    surface: ReviewSurface,
    visible: &[crate::brain_activity::ActivityItem],
    targets: &[ReviewTarget],
) -> Result<(), ReviewAlignmentError> {
    for (index, (item, target)) in visible.iter().zip(targets).enumerate() {
        if target.display_id != item.activity_id {
            return Err(ReviewAlignmentError::RowIdentity { surface, index });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrainSourceError {
    Busy,
    Other(String),
}

impl std::fmt::Display for BrainSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("brain data is busy"),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

pub trait BrainSource: Send + Sync {
    fn refresh(&self, limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError>;
    fn gate_mode(&self) -> BrainGateMode;
    fn endpoint_health(&self) -> EndpointHealth;
}

pub trait BrainActions: Send + Sync {
    fn mutate_review_state(
        &self,
        _request: ReviewMutationRequest,
    ) -> Result<ReviewMutationResult, ReviewMutationError> {
        Err(ReviewMutationError::Unsupported)
    }
    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String>;
    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String>;
    fn preflight_session_action(
        &self,
        request: SessionActionPreflightRequest,
    ) -> Result<SessionActionAvailability, SessionActionFailure>;
    fn send_session_action(
        &self,
        request: SessionActionRequest,
    ) -> Result<(), SessionActionFailure>;
    fn poll_recovery(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMutationError {
    Busy,
    DurabilityUncertain,
    InvalidRequest(ReviewRequestError),
    StaleRevision,
    TargetNoLongerEligible,
    CountMismatch,
    DispositionConflict,
    CapacityExceeded,
    Unsupported,
    Other(String),
}

impl std::fmt::Display for ReviewMutationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => formatter.write_str("review state is busy"),
            Self::DurabilityUncertain => {
                formatter.write_str("review state durability is uncertain")
            }
            Self::InvalidRequest(error) => write!(formatter, "invalid review request: {error:?}"),
            Self::StaleRevision => formatter.write_str("review surface revision changed"),
            Self::TargetNoLongerEligible => {
                formatter.write_str("review target is no longer eligible")
            }
            Self::CountMismatch => formatter.write_str("review target count changed"),
            Self::DispositionConflict => formatter.write_str("review target disposition changed"),
            Self::CapacityExceeded => formatter.write_str("review state key limit exceeded"),
            Self::Unsupported => formatter.write_str("review mutation is unsupported"),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for ReviewMutationError {}

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

pub struct MockBrainRuntime {
    pub activity_snapshot: ActivitySnapshot,
    pub review_queue: Vec<ReviewItemSummary>,
    pub scorecard: ScorecardSummary,
    pub review_state: BrainReviewProjection,
    pub endpoint_health: EndpointHealth,
    pub gate_mode: std::sync::Mutex<Option<BrainGateMode>>,
    pub actions_log: std::sync::Mutex<Vec<MockBrainAction>>,
    pub review_surface_states: std::sync::Mutex<BTreeMap<ReviewSurface, MockReviewSurfaceState>>,
    pub review_mutation_failures: std::sync::Mutex<VecDeque<ReviewMutationError>>,
    pub session_action_capabilities: std::sync::Mutex<Vec<SessionActionCapability>>,
    pub session_action_preflight_failure: std::sync::Mutex<Option<SessionActionFailure>>,
    pub session_action_failure: std::sync::Mutex<Option<SessionActionFailure>>,
    pub session_action_error: std::sync::Mutex<Option<String>>,
}

impl Default for MockBrainRuntime {
    fn default() -> Self {
        Self {
            activity_snapshot: ActivitySnapshot::default(),
            review_queue: Vec::new(),
            scorecard: ScorecardSummary::default(),
            review_state: BrainReviewProjection::default(),
            endpoint_health: EndpointHealth::default(),
            gate_mode: std::sync::Mutex::new(None),
            actions_log: std::sync::Mutex::new(Vec::new()),
            review_surface_states: std::sync::Mutex::new(BTreeMap::new()),
            review_mutation_failures: std::sync::Mutex::new(VecDeque::new()),
            session_action_capabilities: std::sync::Mutex::new(vec![
                SessionActionCapability::Allow,
                SessionActionCapability::Deny,
                SessionActionCapability::Continue,
                SessionActionCapability::ManualText,
            ]),
            session_action_preflight_failure: std::sync::Mutex::new(None),
            session_action_failure: std::sync::Mutex::new(None),
            session_action_error: std::sync::Mutex::new(None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MockReviewSurfaceState {
    pub eligible_keys: BTreeSet<ReviewKey>,
    pub dispositions: BTreeMap<ReviewKey, ReviewDisposition>,
    pub last_archive: BTreeSet<ReviewKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockBrainAction {
    PollRecovery,
    ReviewMutation(ReviewMutationRequest),
    RecordCorrection(CorrectionInput),
    MarkCanonical {
        decision_id: String,
        note: Option<String>,
    },
    SessionActionPreflight(SessionActionPreflightRequest),
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

    pub fn with_review_surface_state(
        self,
        surface: ReviewSurface,
        state: MockReviewSurfaceState,
    ) -> Self {
        self.review_surface_states
            .lock()
            .expect("brain review_surface_states poisoned")
            .insert(surface, state);
        self
    }

    pub fn fail_next_review_mutation(&self, error: ReviewMutationError) {
        self.review_mutation_failures
            .lock()
            .expect("brain review_mutation_failures poisoned")
            .push_back(error);
    }
}

impl BrainSource for MockBrainRuntime {
    fn refresh(&self, _limits: SnapshotLimits) -> Result<BrainRefresh, BrainSourceError> {
        let review_model = self
            .mock_review_model()
            .map_err(|error| BrainSourceError::Other(error.to_string()))?;
        let review_state = review_model.projection;
        let active_ids = |surface: ReviewSurface| {
            review_projection(&review_state, surface)
                .items
                .iter()
                .map(|target| target.display_id.as_str())
                .collect::<BTreeSet<_>>()
        };
        let attention_ids = active_ids(ReviewSurface::Attention);
        let review_ids = active_ids(ReviewSurface::Review);
        let diagnostic_ids = active_ids(ReviewSurface::Diagnostics);
        let recent_ids = active_ids(ReviewSurface::Recent);
        let mut snapshot = self.activity_snapshot.clone();
        snapshot
            .attention
            .retain(|item| attention_ids.contains(item.review_display_id().as_str()));
        snapshot
            .diagnostic_events
            .retain(|item| diagnostic_ids.contains(item.activity_id.as_str()));
        snapshot
            .recent
            .retain(|item| recent_ids.contains(item.activity_id.as_str()));
        let review_queue = self
            .review_queue
            .iter()
            .filter(|item| review_ids.contains(item.review_display_id().as_str()))
            .cloned()
            .collect();
        let refresh = BrainRefresh {
            snapshot,
            review_queue,
            scorecard: self.scorecard.clone(),
            review_state,
        };
        refresh
            .validate_review_alignment()
            .map_err(|error| BrainSourceError::Other(error.to_string()))?;
        Ok(refresh)
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
    fn mutate_review_state(
        &self,
        request: ReviewMutationRequest,
    ) -> Result<ReviewMutationResult, ReviewMutationError> {
        if let Some(error) = self
            .review_mutation_failures
            .lock()
            .expect("brain review_mutation_failures poisoned")
            .pop_front()
        {
            return Err(error);
        }
        let review_surface_states = self
            .review_surface_states
            .lock()
            .expect("brain review_surface_states poisoned")
            .clone();
        let mut actions = self.actions_log.lock().expect("brain actions_log poisoned");
        let mut model =
            MockReviewModel::from_projection(self.review_state.clone(), &review_surface_states);
        for action in actions.iter() {
            if let MockBrainAction::ReviewMutation(previous) = action {
                model.apply(previous)?;
            }
        }
        let result = model.apply(&request)?;
        actions.push(MockBrainAction::ReviewMutation(request));
        Ok(result)
    }

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

    fn preflight_session_action(
        &self,
        request: SessionActionPreflightRequest,
    ) -> Result<SessionActionAvailability, SessionActionFailure> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::SessionActionPreflight(request.clone()));
        if let Some(failure) = self
            .session_action_preflight_failure
            .lock()
            .expect("brain session_action_preflight_failure poisoned")
            .clone()
        {
            return Err(failure);
        }
        Ok(SessionActionAvailability {
            attempt: request.attempt,
            capabilities: self
                .session_action_capabilities
                .lock()
                .expect("brain session_action_capabilities poisoned")
                .clone(),
        })
    }

    fn send_session_action(
        &self,
        request: SessionActionRequest,
    ) -> Result<(), SessionActionFailure> {
        self.actions_log
            .lock()
            .expect("brain actions_log poisoned")
            .push(MockBrainAction::SessionAction(request));
        if let Some(failure) = self
            .session_action_failure
            .lock()
            .expect("brain session_action_failure poisoned")
            .clone()
        {
            return Err(failure);
        }
        if self
            .session_action_error
            .lock()
            .expect("brain session_action_error poisoned")
            .is_some()
        {
            return Err(SessionActionFailure {
                category: SessionActionFailureCategory::Guarded(
                    GuardedActionFailureCategory::SendFailed,
                ),
                diagnostic_persisted: false,
            });
        }
        Ok(())
    }
}

impl MockBrainRuntime {
    fn mock_review_model(&self) -> Result<MockReviewModel, ReviewMutationError> {
        let review_surface_states = self
            .review_surface_states
            .lock()
            .expect("brain review_surface_states poisoned")
            .clone();
        let actions = self.actions_log.lock().expect("brain actions_log poisoned");
        let mut model =
            MockReviewModel::from_projection(self.review_state.clone(), &review_surface_states);
        for action in actions.iter() {
            if let MockBrainAction::ReviewMutation(request) = action {
                model.apply(request)?;
            }
        }
        Ok(model)
    }
}

#[derive(Clone)]
struct MockReviewRow {
    display_id: String,
    keys: Vec<ReviewKey>,
}

struct MockReviewModel {
    projection: BrainReviewProjection,
    rows: std::collections::BTreeMap<ReviewSurface, Vec<MockReviewRow>>,
    eligible: std::collections::BTreeMap<ReviewSurface, BTreeSet<ReviewKey>>,
    dispositions: std::collections::BTreeMap<
        ReviewSurface,
        std::collections::BTreeMap<ReviewKey, ReviewDisposition>,
    >,
    last_archive: std::collections::BTreeMap<ReviewSurface, BTreeSet<ReviewKey>>,
}

impl MockReviewModel {
    fn from_projection(
        projection: BrainReviewProjection,
        review_surface_states: &BTreeMap<ReviewSurface, MockReviewSurfaceState>,
    ) -> Self {
        let mut rows = std::collections::BTreeMap::new();
        let mut eligible = std::collections::BTreeMap::new();
        let mut dispositions = std::collections::BTreeMap::new();
        let mut last_archive = std::collections::BTreeMap::new();
        for surface in [
            ReviewSurface::Attention,
            ReviewSurface::Review,
            ReviewSurface::Diagnostics,
            ReviewSurface::Recent,
        ] {
            let surface_projection = review_projection(&projection, surface);
            let mut surface_rows = Vec::new();
            let mut surface_dispositions = std::collections::BTreeMap::new();
            for target in &surface_projection.items {
                let keys = target
                    .new_member_keys
                    .iter()
                    .chain(&target.reviewed_member_keys)
                    .copied()
                    .collect();
                surface_dispositions.extend(
                    target
                        .reviewed_member_keys
                        .iter()
                        .map(|key| (*key, ReviewDisposition::Reviewed)),
                );
                surface_rows.push(MockReviewRow {
                    display_id: target.display_id.clone(),
                    keys,
                });
            }
            let visible_eligible = surface_rows
                .iter()
                .flat_map(|row| row.keys.iter().copied())
                .collect();
            if let Some(state) = review_surface_states.get(&surface) {
                eligible.insert(surface, state.eligible_keys.clone());
                dispositions.insert(surface, state.dispositions.clone());
                last_archive.insert(surface, state.last_archive.clone());
            } else {
                eligible.insert(surface, visible_eligible);
                dispositions.insert(surface, surface_dispositions);
                last_archive.insert(surface, BTreeSet::new());
            }
            rows.insert(surface, surface_rows);
        }
        Self {
            projection,
            rows,
            eligible,
            dispositions,
            last_archive,
        }
    }

    fn apply(
        &mut self,
        request: &ReviewMutationRequest,
    ) -> Result<ReviewMutationResult, ReviewMutationError> {
        request
            .validate()
            .map_err(ReviewMutationError::InvalidRequest)?;
        if review_projection(&self.projection, request.surface).revision
            != request.expected_surface_revision
        {
            return Err(ReviewMutationError::StaleRevision);
        }
        let eligible = self
            .eligible
            .get(&request.surface)
            .expect("mock review model contains every surface");
        let dispositions = self
            .dispositions
            .get_mut(&request.surface)
            .expect("mock review model contains every surface");
        let last_archive = self
            .last_archive
            .get_mut(&request.surface)
            .expect("mock review model contains every surface");
        dispositions.retain(|key, _| eligible.contains(key));
        last_archive.retain(|key| {
            eligible.contains(key) && dispositions.get(key) == Some(&ReviewDisposition::Archived)
        });
        match &request.operation {
            ReviewMutation::SetDisposition { keys, disposition } => {
                if !keys.iter().all(|key| eligible.contains(key)) {
                    return Err(ReviewMutationError::TargetNoLongerEligible);
                }
                match disposition {
                    ReviewDisposition::Reviewed => {
                        if keys.iter().any(|key| dispositions.contains_key(key)) {
                            return Err(ReviewMutationError::DispositionConflict);
                        }
                        if dispositions.len().saturating_add(keys.len()) > MAX_REVIEW_KEYS {
                            return Err(ReviewMutationError::CapacityExceeded);
                        }
                    }
                    ReviewDisposition::Archived => {
                        if keys
                            .iter()
                            .any(|key| dispositions.get(key) != Some(&ReviewDisposition::Reviewed))
                        {
                            return Err(ReviewMutationError::DispositionConflict);
                        }
                        last_archive.clone_from(keys);
                    }
                }
                dispositions.extend(keys.iter().map(|key| (*key, *disposition)));
            }
            ReviewMutation::ArchiveAllReviewed { expected_count } => {
                let reviewed = dispositions
                    .iter()
                    .filter_map(|(key, disposition)| {
                        (*disposition == ReviewDisposition::Reviewed).then_some(*key)
                    })
                    .collect::<BTreeSet<_>>();
                if reviewed.len() != *expected_count {
                    return Err(ReviewMutationError::CountMismatch);
                }
                dispositions.extend(
                    reviewed
                        .iter()
                        .map(|key| (*key, ReviewDisposition::Archived)),
                );
                *last_archive = reviewed;
            }
            ReviewMutation::UndoLastArchive { expected_count } => {
                if last_archive.len() != *expected_count {
                    return Err(ReviewMutationError::CountMismatch);
                }
                if last_archive.iter().any(|key| {
                    !eligible.contains(key)
                        || dispositions.get(key) != Some(&ReviewDisposition::Archived)
                }) {
                    return Err(ReviewMutationError::DispositionConflict);
                }
                dispositions.extend(
                    last_archive
                        .iter()
                        .map(|key| (*key, ReviewDisposition::Reviewed)),
                );
                last_archive.clear();
            }
        }
        let projection = review_projection_mut(&mut self.projection, request.surface);
        projection.revision = projection
            .revision
            .checked_add(1)
            .ok_or_else(|| ReviewMutationError::Other("review surface revision overflow".into()))?;
        projection.items = self.rows[&request.surface]
            .iter()
            .filter_map(|row| {
                let new_member_keys = row
                    .keys
                    .iter()
                    .filter(|key| eligible.contains(key) && !dispositions.contains_key(key))
                    .copied()
                    .collect::<Vec<_>>();
                let reviewed_member_keys = row
                    .keys
                    .iter()
                    .filter(|key| {
                        eligible.contains(key)
                            && dispositions.get(key) == Some(&ReviewDisposition::Reviewed)
                    })
                    .copied()
                    .collect::<Vec<_>>();
                (!new_member_keys.is_empty() || !reviewed_member_keys.is_empty()).then(|| {
                    ReviewTarget {
                        surface: request.surface,
                        display_id: row.display_id.clone(),
                        new_member_keys,
                        reviewed_member_keys,
                    }
                })
            })
            .collect();
        projection.new_count = eligible
            .iter()
            .filter(|key| !dispositions.contains_key(key))
            .count();
        projection.reviewed_count = dispositions
            .values()
            .filter(|disposition| **disposition == ReviewDisposition::Reviewed)
            .count();
        projection.last_archive_count = last_archive.len();
        Ok(ReviewMutationResult {
            surface: request.surface,
            surface_revision: projection.revision,
            reviewed_count: projection.reviewed_count,
            archived_count: dispositions
                .values()
                .filter(|disposition| **disposition == ReviewDisposition::Archived)
                .count(),
            last_archive_count: projection.last_archive_count,
        })
    }
}

fn review_projection(
    projection: &BrainReviewProjection,
    surface: ReviewSurface,
) -> &SurfaceReviewProjection {
    match surface {
        ReviewSurface::Attention => &projection.attention,
        ReviewSurface::Review => &projection.review,
        ReviewSurface::Diagnostics => &projection.diagnostics,
        ReviewSurface::Recent => &projection.recent,
    }
}

fn review_projection_mut(
    projection: &mut BrainReviewProjection,
    surface: ReviewSurface,
) -> &mut SurfaceReviewProjection {
    match surface {
        ReviewSurface::Attention => &mut projection.attention,
        ReviewSurface::Review => &mut projection.review,
        ReviewSurface::Diagnostics => &mut projection.diagnostics,
        ReviewSurface::Recent => &mut projection.recent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::AgentProvider;
    use crate::review_state::{
        ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest, ReviewSurface,
        ReviewTarget, SurfaceReviewProjection,
    };
    use crate::terminals::TerminalSessionAction;

    #[test]
    fn review_alignment_rejects_attention_mismatch() {
        assert_refresh_alignment_rejects(ReviewSurface::Attention);
    }

    #[test]
    fn review_alignment_rejects_review_mismatch() {
        assert_refresh_alignment_rejects(ReviewSurface::Review);
    }

    #[test]
    fn review_alignment_rejects_diagnostics_mismatch() {
        assert_refresh_alignment_rejects(ReviewSurface::Diagnostics);
    }

    #[test]
    fn review_alignment_rejects_recent_mismatch() {
        assert_refresh_alignment_rejects(ReviewSurface::Recent);
    }

    #[test]
    fn review_projection_constructor_rejects_wrong_surface_target() {
        assert_eq!(
            SurfaceReviewProjection::from_items(
                ReviewSurface::Review,
                0,
                vec![review_target(ReviewSurface::Attention)],
                1,
                1,
                0,
                0,
            ),
            Err(ReviewAlignmentError::TargetSurface {
                expected: ReviewSurface::Review,
                actual: ReviewSurface::Attention,
                index: 0,
            })
        );
    }

    #[test]
    fn review_alignment_rejects_same_length_recent_reordering() {
        let mut first = activity_item("activity-a");
        let mut second = activity_item("activity-b");
        first.recorded_at_ms = 2;
        second.recorded_at_ms = 1;
        let mut refresh = BrainRefresh {
            snapshot: ActivitySnapshot {
                recent: vec![first, second],
                ..ActivitySnapshot::default()
            },
            ..BrainRefresh::default()
        };
        refresh.review_state.recent = SurfaceReviewProjection::from_items(
            ReviewSurface::Recent,
            0,
            vec![
                review_target_for(ReviewSurface::Recent, "activity-b", b"activity-b"),
                review_target_for(ReviewSurface::Recent, "activity-a", b"activity-a"),
            ],
            2,
            2,
            0,
            0,
        )
        .unwrap();

        assert_eq!(
            refresh.validate_review_alignment(),
            Err(ReviewAlignmentError::RowIdentity {
                surface: ReviewSurface::Recent,
                index: 0,
            })
        );
    }

    #[test]
    fn review_projection_rejects_empty_display_identity() {
        let target = review_target_for(ReviewSurface::Review, "", b"decision-1");

        assert_eq!(
            SurfaceReviewProjection::from_items(ReviewSurface::Review, 0, vec![target], 1, 1, 0, 0,),
            Err(ReviewAlignmentError::EmptyDisplayId {
                surface: ReviewSurface::Review,
                index: 0,
            })
        );
    }

    #[test]
    fn review_projection_rejects_empty_member_identity() {
        let target = ReviewTarget {
            surface: ReviewSurface::Review,
            display_id: "decision-1".into(),
            new_member_keys: Vec::new(),
            reviewed_member_keys: Vec::new(),
        };

        assert_eq!(
            SurfaceReviewProjection::from_items(ReviewSurface::Review, 0, vec![target], 1, 0, 0, 0,),
            Err(ReviewAlignmentError::EmptyMembers {
                surface: ReviewSurface::Review,
                index: 0,
            })
        );
    }

    #[test]
    fn review_projection_rejects_new_reviewed_member_overlap() {
        let key = ReviewKey::derive(ReviewSurface::Review, b"decision-1");
        let target = ReviewTarget {
            surface: ReviewSurface::Review,
            display_id: "decision-1".into(),
            new_member_keys: vec![key],
            reviewed_member_keys: vec![key],
        };

        assert_eq!(
            SurfaceReviewProjection::from_items(ReviewSurface::Review, 0, vec![target], 1, 1, 1, 0,),
            Err(ReviewAlignmentError::MemberOverlap {
                surface: ReviewSurface::Review,
                index: 0,
            })
        );
    }

    #[test]
    fn review_projection_rejects_duplicate_member_across_targets() {
        let key = ReviewKey::derive(ReviewSurface::Recent, b"activity-a");
        let targets = vec![
            ReviewTarget {
                surface: ReviewSurface::Recent,
                display_id: "activity-a".into(),
                new_member_keys: vec![key],
                reviewed_member_keys: Vec::new(),
            },
            ReviewTarget {
                surface: ReviewSurface::Recent,
                display_id: "activity-b".into(),
                new_member_keys: vec![key],
                reviewed_member_keys: Vec::new(),
            },
        ];

        assert_eq!(
            SurfaceReviewProjection::from_items(ReviewSurface::Recent, 0, targets, 2, 2, 0, 0,),
            Err(ReviewAlignmentError::DuplicateMember {
                surface: ReviewSurface::Recent,
                index: 1,
            })
        );
    }

    #[test]
    fn review_projection_rejects_inconsistent_new_and_reviewed_counts() {
        let new_target = review_target_for(ReviewSurface::Recent, "activity-a", b"activity-a");
        assert_eq!(
            SurfaceReviewProjection::from_items(
                ReviewSurface::Recent,
                0,
                vec![new_target],
                1,
                0,
                0,
                0,
            ),
            Err(ReviewAlignmentError::NewCount {
                surface: ReviewSurface::Recent,
                declared: 0,
                actual: 1,
            })
        );

        let key = ReviewKey::derive(ReviewSurface::Recent, b"activity-b");
        let reviewed_target = ReviewTarget {
            surface: ReviewSurface::Recent,
            display_id: "activity-b".into(),
            new_member_keys: Vec::new(),
            reviewed_member_keys: vec![key],
        };
        assert_eq!(
            SurfaceReviewProjection::from_items(
                ReviewSurface::Recent,
                0,
                vec![reviewed_target],
                1,
                0,
                0,
                0,
            ),
            Err(ReviewAlignmentError::ReviewedCount {
                surface: ReviewSurface::Recent,
                declared: 0,
                actual: 1,
            })
        );
    }

    #[test]
    fn review_projection_allows_retained_counts_above_visible_member_sums() {
        let new_target = review_target_for(ReviewSurface::Recent, "activity-a", b"activity-a");
        let key = ReviewKey::derive(ReviewSurface::Recent, b"activity-b");
        let reviewed_target = ReviewTarget {
            surface: ReviewSurface::Recent,
            display_id: "activity-b".into(),
            new_member_keys: Vec::new(),
            reviewed_member_keys: vec![key],
        };

        assert!(
            SurfaceReviewProjection::from_items(
                ReviewSurface::Recent,
                0,
                vec![new_target, reviewed_target],
                2,
                3,
                4,
                0,
            )
            .is_ok()
        );
    }

    #[test]
    fn review_alignment_rejects_attention_occurrence_member_mismatch() {
        let attention = crate::brain_activity::AttentionItem {
            activity: activity_item("activity-a"),
            occurrences: 2,
            unresolved_occurrences: 2,
        };
        let display_id = attention.review_display_id();
        let mut refresh = BrainRefresh {
            snapshot: ActivitySnapshot {
                attention: vec![attention],
                unresolved_count: 2,
                ..ActivitySnapshot::default()
            },
            ..BrainRefresh::default()
        };
        refresh.review_state.attention = SurfaceReviewProjection::from_items(
            ReviewSurface::Attention,
            0,
            vec![review_target_for(
                ReviewSurface::Attention,
                &display_id,
                b"activity-a",
            )],
            1,
            1,
            0,
            0,
        )
        .unwrap();

        assert_eq!(
            refresh.validate_review_alignment(),
            Err(ReviewAlignmentError::MemberCount {
                surface: ReviewSurface::Attention,
                index: 0,
                visible_members: 2,
                review_members: 1,
            })
        );
    }

    #[test]
    fn legacy_review_identity_preserves_v1_storage_grammar() {
        fn field(hash: &mut Sha256, value: &[u8]) {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value);
        }
        fn optional_field(hash: &mut Sha256, value: Option<&str>) {
            match value {
                Some(value) => {
                    hash.update((value.len() as u64 + 1).to_be_bytes());
                    hash.update([1]);
                    hash.update(value.as_bytes());
                }
                None => {
                    hash.update(1_u64.to_be_bytes());
                    hash.update([0]);
                }
            }
        }

        let decision = legacy_decision_summary();
        let mut expected = Sha256::new();
        expected.update(b"legacy-review:v1");
        field(&mut expected, b"codex");
        field(&mut expected, b"1");
        field(&mut expected, &7_u32.to_be_bytes());
        field(&mut expected, b"project");
        optional_field(&mut expected, Some("Bash"));
        optional_field(&mut expected, Some("cargo test"));
        field(&mut expected, b"deny");
        field(&mut expected, &0.9_f64.to_bits().to_be_bytes());

        assert_eq!(
            decision.review_source_identity(),
            expected.finalize().to_vec()
        );
    }

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
        let preflight = SessionActionPreflightRequest::new(session_target());
        let availability = runtime
            .actions
            .preflight_session_action(preflight.clone())
            .unwrap();
        let request = SessionActionRequest {
            attempt: availability.attempt,
            action: TerminalSessionAction::Continue,
        };

        runtime
            .actions
            .send_session_action(request.clone())
            .unwrap();

        assert!(matches!(
            mock.actions()[0],
            MockBrainAction::SessionActionPreflight(_)
        ));
        assert_eq!(mock.actions()[1], MockBrainAction::SessionAction(request));
    }

    #[test]
    fn session_action_preflight_creates_opaque_attempt_identity() {
        let request = SessionActionPreflightRequest::new(session_target());

        assert!(uuid::Uuid::parse_str(&request.attempt.attempt_id).is_ok());
        assert_eq!(
            request.attempt.target,
            SessionActionTarget::from(session_target())
        );
    }

    #[test]
    fn session_action_preflight_discards_sensitive_target_fields() {
        let mock = Arc::new(MockBrainRuntime::default());
        let runtime = BrainRuntime::new(mock.clone(), mock);
        let request = SessionActionPreflightRequest::new(sensitive_session_target());
        let availability = runtime
            .actions
            .preflight_session_action(request.clone())
            .unwrap();

        for secret in [
            "provider-session-secret",
            "turn-secret",
            "tool-use-secret",
            "provider-hint-secret",
        ] {
            assert!(!format!("{:?}", request.attempt).contains(secret));
            assert!(!format!("{:?}", availability).contains(secret));
        }
    }

    #[test]
    fn brain_runtime_exposes_only_brain_source_and_actions() {
        let runtime = MockBrainRuntime::default().into_runtime();

        assert_eq!(runtime.source.gate_mode(), BrainGateMode::On);
        assert!(
            runtime
                .source
                .refresh(crate::brain_activity::SnapshotLimits::default())
                .unwrap()
                .snapshot
                .recent
                .is_empty()
        );
    }

    #[test]
    fn mock_source_returns_one_refresh_bundle() {
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                unresolved_count: 2,
                ..ActivitySnapshot::default()
            },
            review_queue: Vec::new(),
            scorecard: ScorecardSummary {
                total_decisions: 3,
                ..ScorecardSummary::default()
            },
            ..MockBrainRuntime::default()
        };

        let refresh = mock.refresh(SnapshotLimits::default()).unwrap();

        assert_eq!(refresh.snapshot.unresolved_count, 2);
        assert!(refresh.review_queue.is_empty());
        assert_eq!(refresh.scorecard.total_decisions, 3);
    }

    #[test]
    fn mock_review_mutation_validates_and_advances_revision() {
        let key = ReviewKey::derive(ReviewSurface::Diagnostics, b"diagnostic-1");
        let mock = MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                diagnostic_events: vec![activity_item("diagnostic-1")],
                ..ActivitySnapshot::default()
            },
            review_state: BrainReviewProjection {
                diagnostics: SurfaceReviewProjection::from_items(
                    ReviewSurface::Diagnostics,
                    0,
                    vec![ReviewTarget {
                        surface: ReviewSurface::Diagnostics,
                        display_id: "diagnostic-1".into(),
                        new_member_keys: vec![key],
                        reviewed_member_keys: Vec::new(),
                    }],
                    1,
                    1,
                    0,
                    0,
                )
                .unwrap(),
                ..BrainReviewProjection::default()
            },
            ..MockBrainRuntime::default()
        };
        let request = ReviewMutationRequest {
            surface: ReviewSurface::Diagnostics,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        };

        let result = mock.mutate_review_state(request.clone()).unwrap();

        assert_eq!(result.surface_revision, 1);
        let refresh = mock.refresh(SnapshotLimits::default()).unwrap();
        assert_eq!(refresh.review_state.diagnostics.revision, 1);
        assert_eq!(refresh.review_state.diagnostics.new_count, 0);
        assert_eq!(refresh.review_state.diagnostics.reviewed_count, 1);
        assert_eq!(
            mock.mutate_review_state(request),
            Err(ReviewMutationError::StaleRevision)
        );
        assert!(matches!(
            mock.actions().last(),
            Some(MockBrainAction::ReviewMutation(_))
        ));
    }

    #[test]
    fn mock_review_mutation_can_inject_a_typed_failure_without_recording() {
        let mock = MockBrainRuntime::default();
        mock.fail_next_review_mutation(ReviewMutationError::DurabilityUncertain);
        let request = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::ArchiveAllReviewed { expected_count: 0 },
        };

        assert_eq!(
            mock.mutate_review_state(request),
            Err(ReviewMutationError::DurabilityUncertain)
        );
        assert!(mock.actions().is_empty());
    }

    #[test]
    fn mock_archive_all_uses_explicit_retained_state_beyond_visible_rows() {
        let visible = ReviewKey::derive(ReviewSurface::Attention, b"visible");
        let hidden = ReviewKey::derive(ReviewSurface::Attention, b"hidden");
        let retained = [visible, hidden].into_iter().collect::<BTreeSet<_>>();
        let mock = MockBrainRuntime {
            review_state: BrainReviewProjection {
                attention: SurfaceReviewProjection::from_items(
                    ReviewSurface::Attention,
                    7,
                    vec![ReviewTarget {
                        surface: ReviewSurface::Attention,
                        display_id: "visible".into(),
                        new_member_keys: Vec::new(),
                        reviewed_member_keys: vec![visible],
                    }],
                    1,
                    0,
                    2,
                    0,
                )
                .unwrap(),
                ..BrainReviewProjection::default()
            },
            ..MockBrainRuntime::default()
        }
        .with_review_surface_state(
            ReviewSurface::Attention,
            MockReviewSurfaceState {
                eligible_keys: retained.clone(),
                dispositions: retained
                    .iter()
                    .map(|key| (*key, ReviewDisposition::Reviewed))
                    .collect(),
                last_archive: BTreeSet::new(),
            },
        );

        assert_eq!(
            mock.mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 7,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 1 },
            }),
            Err(ReviewMutationError::CountMismatch)
        );
        let result = mock
            .mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 7,
                operation: ReviewMutation::ArchiveAllReviewed { expected_count: 2 },
            })
            .unwrap();
        assert_eq!(result.archived_count, 2);
        assert_eq!(result.last_archive_count, 2);
    }

    #[test]
    fn restarted_mock_can_undo_explicit_persisted_archive_slot() {
        let keys = [
            ReviewKey::derive(ReviewSurface::Diagnostics, b"one"),
            ReviewKey::derive(ReviewSurface::Diagnostics, b"two"),
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let mock = MockBrainRuntime {
            review_state: BrainReviewProjection {
                diagnostics: SurfaceReviewProjection::from_items(
                    ReviewSurface::Diagnostics,
                    4,
                    Vec::new(),
                    0,
                    0,
                    0,
                    2,
                )
                .unwrap(),
                ..BrainReviewProjection::default()
            },
            ..MockBrainRuntime::default()
        }
        .with_review_surface_state(
            ReviewSurface::Diagnostics,
            MockReviewSurfaceState {
                eligible_keys: keys.clone(),
                dispositions: keys
                    .iter()
                    .map(|key| (*key, ReviewDisposition::Archived))
                    .collect(),
                last_archive: keys,
            },
        );

        let result = mock
            .mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Diagnostics,
                expected_surface_revision: 4,
                operation: ReviewMutation::UndoLastArchive { expected_count: 2 },
            })
            .unwrap();

        assert_eq!(result.reviewed_count, 2);
        assert_eq!(result.archived_count, 0);
        assert_eq!(result.last_archive_count, 0);
    }

    #[test]
    fn mock_undo_prunes_archive_slot_to_eligible_archived_intersection() {
        let stale = ReviewKey::derive(ReviewSurface::Review, b"stale");
        let retained = ReviewKey::derive(ReviewSurface::Review, b"retained");
        let mock = MockBrainRuntime {
            review_state: BrainReviewProjection {
                review: SurfaceReviewProjection::from_items(
                    ReviewSurface::Review,
                    3,
                    Vec::new(),
                    0,
                    0,
                    0,
                    1,
                )
                .unwrap(),
                ..BrainReviewProjection::default()
            },
            ..MockBrainRuntime::default()
        }
        .with_review_surface_state(
            ReviewSurface::Review,
            MockReviewSurfaceState {
                eligible_keys: [retained].into_iter().collect(),
                dispositions: [stale, retained]
                    .into_iter()
                    .map(|key| (key, ReviewDisposition::Archived))
                    .collect(),
                last_archive: [stale, retained].into_iter().collect(),
            },
        );

        let result = mock
            .mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Review,
                expected_surface_revision: 3,
                operation: ReviewMutation::UndoLastArchive { expected_count: 1 },
            })
            .unwrap();

        assert_eq!(result.reviewed_count, 1);
        assert_eq!(result.archived_count, 0);
        assert_eq!(result.last_archive_count, 0);
    }

    #[test]
    fn mock_review_enforces_total_retained_capacity_at_the_shared_boundary() {
        fn fixture(retained_count: usize) -> (MockBrainRuntime, ReviewKey) {
            let retained = (0..retained_count)
                .map(|index| ReviewKey::derive(ReviewSurface::Attention, &index.to_be_bytes()))
                .collect::<BTreeSet<_>>();
            let extra = ReviewKey::derive(ReviewSurface::Attention, b"eligible-new");
            let eligible_keys = retained
                .iter()
                .copied()
                .chain([extra])
                .collect::<BTreeSet<_>>();
            let mock = MockBrainRuntime {
                review_state: BrainReviewProjection {
                    attention: SurfaceReviewProjection::from_items(
                        ReviewSurface::Attention,
                        0,
                        Vec::new(),
                        0,
                        1,
                        retained_count,
                        0,
                    )
                    .unwrap(),
                    ..BrainReviewProjection::default()
                },
                ..MockBrainRuntime::default()
            }
            .with_review_surface_state(
                ReviewSurface::Attention,
                MockReviewSurfaceState {
                    eligible_keys,
                    dispositions: retained
                        .into_iter()
                        .map(|key| (key, ReviewDisposition::Reviewed))
                        .collect(),
                    last_archive: BTreeSet::new(),
                },
            );
            (mock, extra)
        }

        let (at_boundary, boundary_key) =
            fixture(crate::review_state::MAX_REVIEW_KEYS.saturating_sub(1));
        let boundary_result = at_boundary
            .mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::SetDisposition {
                    keys: [boundary_key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            })
            .unwrap();
        assert_eq!(
            boundary_result.reviewed_count,
            crate::review_state::MAX_REVIEW_KEYS
        );

        let (over_capacity, extra_key) = fixture(crate::review_state::MAX_REVIEW_KEYS);
        assert_eq!(
            over_capacity.mutate_review_state(ReviewMutationRequest {
                surface: ReviewSurface::Attention,
                expected_surface_revision: 0,
                operation: ReviewMutation::SetDisposition {
                    keys: [extra_key].into_iter().collect(),
                    disposition: ReviewDisposition::Reviewed,
                },
            }),
            Err(ReviewMutationError::CapacityExceeded)
        );
    }

    fn assert_refresh_alignment_rejects(surface: ReviewSurface) {
        let mut refresh = BrainRefresh::default();
        let projection = SurfaceReviewProjection {
            items: vec![review_target(surface)],
            ..SurfaceReviewProjection::default()
        };
        match surface {
            ReviewSurface::Attention => refresh.review_state.attention = projection,
            ReviewSurface::Review => refresh.review_state.review = projection,
            ReviewSurface::Diagnostics => refresh.review_state.diagnostics = projection,
            ReviewSurface::Recent => refresh.review_state.recent = projection,
        }

        assert_eq!(
            refresh.validate_review_alignment(),
            Err(ReviewAlignmentError::ItemCount {
                surface,
                visible_items: 0,
                review_targets: 1,
            })
        );
    }

    fn review_target(surface: ReviewSurface) -> ReviewTarget {
        review_target_for(surface, "item-1", b"item-1")
    }

    fn review_target_for(
        surface: ReviewSurface,
        display_id: &str,
        source_identity: &[u8],
    ) -> ReviewTarget {
        ReviewTarget {
            surface,
            display_id: display_id.into(),
            new_member_keys: vec![ReviewKey::derive(surface, source_identity)],
            reviewed_member_keys: Vec::new(),
        }
    }

    fn activity_item(activity_id: &str) -> crate::brain_activity::ActivityItem {
        crate::brain_activity::ActivityItem {
            activity_id: activity_id.into(),
            kind: crate::brain_activity::ActivityKind::Decision,
            recorded_at_ms: 1,
            project: crate::brain_activity::ProjectEvidence {
                project_id: crate::project::ProjectId::Stable("project-1".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: crate::brain_activity::ActivityState::Allowed,
            delivery: crate::brain_activity::DeliveryState::Delivered,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: Some("fixture".into()),
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: None,
            decision_id: Some(activity_id.into()),
            outcome: None,
            correction: None,
            note: None,
            tool_execution_confirmed: false,
        }
    }

    fn legacy_decision_summary() -> DecisionSummary {
        DecisionSummary {
            provider: AgentProvider::Codex,
            id: String::new(),
            timestamp: "1".into(),
            action: "deny".into(),
            confidence: Some(0.9),
            project: Some("project".into()),
            tool: Some("Bash".into()),
            pid: 7,
            command: Some("cargo test".into()),
            reasoning: None,
            user_action: Some("reject".into()),
            override_reason: None,
            brain_decision_ms: None,
            canonical: None,
            cache_hit: None,
            model: None,
            outcome_kind: None,
            outcome_detail: None,
            suggested_at: None,
            resolved_at: None,
        }
    }

    fn session_target() -> SessionTarget {
        SessionTarget {
            provider: AgentProvider::Claude,
            session_id: "session-42".into(),
            provider_session_id: None,
            turn_id: Some("turn-7".into()),
            tool_use_id: None,
            project_id: crate::project::ProjectId::Stable("project-1".into()),
            cwd: "/work/project".into(),
            provider_hints: Vec::new(),
            provenance: crate::brain_activity::SessionTargetProvenance::Structured,
        }
    }

    fn sensitive_session_target() -> SessionTarget {
        let mut target = session_target();
        target.provider_session_id = Some("provider-session-secret".into());
        target.turn_id = Some("turn-secret".into());
        target.tool_use_id = Some("tool-use-secret".into());
        target.provider_hints = vec!["provider-hint-secret".into()];
        target
    }
}
