//! Bind Brain read contracts to the binary's brain subsystem.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{
    ActivityEvent, ActivityKind, ActivitySnapshot, CorrectionDisposition, SessionTargetProvenance,
    SnapshotLimits,
};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use coding_brain_core::runtime::{
    BrainActions, BrainGateMode, BrainSource, CacheSummary, CorrectionInput, CounterfactualSummary,
    DecisionSummary, EndpointHealth, LatencySummary, ProviderScoreSummary, ReviewItemSummary,
    RiskTierSummary, ScorecardSummary, SessionActionRequest,
};
use coding_brain_core::session::AgentSession;
use coding_brain_core::terminals::execute_guarded_action;

use crate::{brain, config};

pub struct LiveBrainSource {
    endpoint_probe: Arc<Mutex<EndpointProbeState>>,
    probe: fn(&str) -> bool,
}

#[derive(Default)]
struct EndpointProbeState {
    key: Option<(String, String)>,
    checked_at: Option<Instant>,
    health: Option<EndpointHealth>,
    in_flight: bool,
}

impl Default for LiveBrainSource {
    fn default() -> Self {
        Self {
            endpoint_probe: Arc::new(Mutex::new(EndpointProbeState::default())),
            probe: endpoint_reachable,
        }
    }
}

impl LiveBrainSource {
    #[cfg(test)]
    fn with_probe(probe: fn(&str) -> bool) -> Self {
        Self {
            endpoint_probe: Arc::new(Mutex::new(EndpointProbeState::default())),
            probe,
        }
    }

    fn endpoint_health_for(&self, endpoint: &str, model: &str) -> EndpointHealth {
        let key = (endpoint.to_owned(), bounded_display(model));
        let mut state = self
            .endpoint_probe
            .lock()
            .expect("endpoint health cache poisoned");
        if state.key.as_ref() != Some(&key) {
            state.key = Some(key.clone());
            state.checked_at = None;
            state.health = None;
            state.in_flight = false;
        }
        if state
            .checked_at
            .is_some_and(|checked_at| checked_at.elapsed() < Duration::from_secs(5))
        {
            return state.health.clone().unwrap_or_default();
        }

        let visible = state.health.clone().unwrap_or_else(|| EndpointHealth {
            model: Some(key.1.clone()),
            detail: Some("Checking the local model…".into()),
            ..EndpointHealth::default()
        });
        if state.in_flight {
            return visible;
        }
        state.in_flight = true;
        drop(state);

        let shared = Arc::clone(&self.endpoint_probe);
        let probe = self.probe;
        let thread_key = key.clone();
        if std::thread::Builder::new()
            .name("coding-brain-health".into())
            .spawn(move || {
                let reachable = probe(&thread_key.0);
                let health = EndpointHealth {
                    reachable,
                    endpoint: None,
                    model: Some(thread_key.1.clone()),
                    detail: (!reachable).then(|| "Start the local model or run `cb doctor`".into()),
                };
                let mut state = shared.lock().expect("endpoint health cache poisoned");
                if state.key.as_ref() == Some(&thread_key) {
                    state.health = Some(health);
                    state.checked_at = Some(Instant::now());
                    state.in_flight = false;
                }
            })
            .is_err()
        {
            let mut state = self
                .endpoint_probe
                .lock()
                .expect("endpoint health cache poisoned");
            if state.key.as_ref() == Some(&key) {
                state.in_flight = false;
                state.health = Some(EndpointHealth {
                    model: Some(key.1),
                    detail: Some("Could not start local model health check".into()),
                    ..EndpointHealth::default()
                });
                state.checked_at = Some(Instant::now());
            }
        }
        visible
    }
}

impl BrainSource for LiveBrainSource {
    fn snapshot(&self, limits: SnapshotLimits) -> Result<ActivitySnapshot, String> {
        let paths = brain::distill::current_paths().map_err(|error| error.to_string())?;
        brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"))
            .snapshot(limits)
            .map_err(|error| error.to_string())
    }

    fn review_queue(&self) -> Result<Vec<ReviewItemSummary>, String> {
        let records = brain::decisions::read_learning_decisions();
        let paths = brain::distill::current_paths().map_err(|error| error.to_string())?;
        let events = brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"))
            .read()
            .map_err(|error| error.to_string())?;
        Ok(review_queue_from(records, events.events()))
    }

    fn scorecard(&self) -> Result<ScorecardSummary, String> {
        let decisions = brain::decisions::read_learning_decisions()
            .into_iter()
            .map(|record| DecisionSummary::from(&record))
            .collect::<Vec<_>>();
        let paths = brain::distill::current_paths().map_err(|error| error.to_string())?;
        let events = brain::activity::ActivityStore::at(paths.state_root().join("activity.jsonl"))
            .read()
            .map_err(|error| error.to_string())?;
        Ok(scorecard_from(&decisions, events.events()))
    }

    fn gate_mode(&self) -> BrainGateMode {
        let config = config::Config::load();
        brain::resolve_gate_mode(config.brain.as_ref()).mode
    }

    fn endpoint_health(&self) -> EndpointHealth {
        let config = config::Config::load();
        let gate_mode = brain::resolve_gate_mode(config.brain.as_ref()).mode;
        let Some(brain_config) = endpoint_config_for_mode(config.brain.as_ref(), gate_mode) else {
            return EndpointHealth {
                detail: Some("Local model is not configured".into()),
                ..EndpointHealth::default()
            };
        };
        self.endpoint_health_for(&brain_config.endpoint, &brain_config.model)
    }
}

fn endpoint_config_for_mode(
    config: Option<&config::BrainConfig>,
    gate_mode: BrainGateMode,
) -> Option<config::BrainConfig> {
    config
        .cloned()
        .or_else(|| (gate_mode != BrainGateMode::Off).then(config::BrainConfig::default))
}

fn scorecard_from(decisions: &[DecisionSummary], events: &[ActivityEvent]) -> ScorecardSummary {
    let corrections = latest_corrections(events);
    let projected = decisions
        .iter()
        .cloned()
        .map(|mut decision| {
            if let Some(correction) = corrections.get(decision.id.as_str()) {
                decision.user_action = Some(correction_user_action(*correction).into());
            }
            decision
        })
        .collect::<Vec<_>>();
    let scored = projected
        .iter()
        .filter(|decision| {
            !decision.action.is_empty() && (decision.is_positive() || decision.is_negative())
        })
        .cloned()
        .collect::<Vec<_>>();
    let brain_decisions = scored.len();
    let correct_decisions = scored
        .iter()
        .filter(|decision| decision.is_positive())
        .count();
    let accuracy_pct = if brain_decisions == 0 {
        0.0
    } else {
        correct_decisions as f64 / brain_decisions as f64 * 100.0
    };
    let abstentions = projected
        .iter()
        .filter(|decision| decision.action == "abstain")
        .count();
    let tier_stats = brain::metrics::compute_tier_stats(&scored);
    let dangerous_false_approvals = tier_stats
        .iter()
        .filter(|summary| {
            matches!(
                summary.tier,
                brain::risk::RiskTier::High | brain::risk::RiskTier::Critical
            )
        })
        .map(|summary| summary.false_approves)
        .sum();
    let override_window = scored.iter().rev().take(50).collect::<Vec<_>>();
    let override_rate_pct = if override_window.is_empty() {
        0.0
    } else {
        override_window
            .iter()
            .filter(|decision| decision.is_negative())
            .count() as f64
            / override_window.len() as f64
            * 100.0
    };
    let latency = brain::metrics::compute_latency(&projected);
    let cache = brain::metrics::compute_cache(&projected);
    let counterfactuals = brain::metrics::compute_counterfactuals(&projected);
    let brain_was_right = counterfactuals
        .iter()
        .filter(|counterfactual| counterfactual.brain_was_right)
        .count();
    let providers = [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ]
    .into_iter()
    .filter_map(|provider| {
        let decisions = scored
            .iter()
            .filter(|decision| decision.provider == provider)
            .count();
        (decisions > 0).then(|| ProviderScoreSummary {
            provider,
            decisions,
            correct: scored
                .iter()
                .filter(|decision| decision.provider == provider && decision.is_positive())
                .count(),
        })
    })
    .collect();

    ScorecardSummary {
        total_decisions: projected.len(),
        brain_decisions,
        correct_decisions,
        accuracy_pct,
        abstentions,
        dangerous_false_approvals,
        override_rate_pct,
        canonical_decisions: projected
            .iter()
            .filter(|decision| decision.canonical == Some(true))
            .count(),
        risk_tiers: tier_stats
            .into_iter()
            .map(|summary| RiskTierSummary {
                tier: summary.tier.label().into(),
                samples: summary.n,
                correct: summary.correct,
                false_approvals: summary.false_approves,
                false_denials: summary.false_denies,
                override_rate_pct: summary.override_rate * 100.0,
            })
            .collect(),
        providers,
        latency: LatencySummary {
            samples: latency.n,
            p50_ms: latency.p50_ms,
            p95_ms: latency.p95_ms,
            p99_ms: latency.p99_ms,
            mean_ms: latency.mean_ms,
            max_ms: latency.max_ms,
        },
        cache: CacheSummary {
            instrumented: cache.instrumented,
            hits: cache.hits,
            misses: cache.misses,
            hit_rate_pct: cache.hit_rate(),
        },
        counterfactuals: CounterfactualSummary {
            brain_was_right,
            user_was_right: counterfactuals.len().saturating_sub(brain_was_right),
        },
    }
}

fn review_queue_from(
    mut records: Vec<brain::decisions::DecisionRecord>,
    events: &[ActivityEvent],
) -> Vec<ReviewItemSummary> {
    let corrections = latest_corrections(events);
    for record in &mut records {
        if let Some(correction) = record
            .decision_id
            .as_deref()
            .and_then(|decision_id| corrections.get(decision_id))
        {
            record.user_action = correction_user_action(*correction).into();
        }
    }
    brain::review::build_queue(&records)
        .into_iter()
        .map(item_summary_from)
        .collect()
}

fn item_summary_from(item: brain::review::ReviewItem) -> ReviewItemSummary {
    ReviewItemSummary {
        decision: DecisionSummary::from(&item.record),
        reason: item.reason,
        score: item.score as f64,
    }
}

fn latest_corrections(events: &[ActivityEvent]) -> HashMap<&str, CorrectionDisposition> {
    let mut corrections = HashMap::new();
    for event in events {
        if let (Some(decision_id), Some(correction)) =
            (event.decision_id.as_deref(), event.correction)
        {
            corrections.insert(decision_id, correction);
        }
    }
    corrections
}

fn correction_user_action(correction: CorrectionDisposition) -> &'static str {
    match correction {
        CorrectionDisposition::BrainRight => "accept",
        CorrectionDisposition::BrainWrong => "reject",
        CorrectionDisposition::Exception => "exception",
    }
}

fn bounded_display(value: &str) -> String {
    coding_brain_core::brain_activity::redact_activity_text(value)
        .chars()
        .take(80)
        .collect()
}

fn endpoint_reachable(endpoint: &str) -> bool {
    std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--max-time",
            "1",
            endpoint,
        ])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() != "000")
}

#[derive(Default)]
pub struct LiveBrainActions {
    recovery: brain::recovery::RecoveryCoordinator,
    action_discovery: Mutex<coding_brain_core::discovery::ProviderDiscoveryState>,
}

impl BrainActions for LiveBrainActions {
    fn poll_recovery(&self) -> Vec<String> {
        self.recovery.poll()
    }

    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String> {
        let paths = brain::distill::current_paths().map_err(|error| error.to_string())?;
        record_correction_at_path(&paths.state_root().join("activity.jsonl"), correction)
    }

    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String> {
        brain::review::mark_by_id(decision_id, note.as_deref())
    }

    fn send_session_action(&self, request: SessionActionRequest) -> Result<(), String> {
        if request.target.provenance == SessionTargetProvenance::Unknown {
            return Err("session action authority is unavailable".into());
        }
        let mut discovery = self
            .action_discovery
            .lock()
            .map_err(|_| "provider discovery state is unavailable".to_string())?;
        let sessions = coding_brain_core::discovery::scan_agent_sessions_with_state(&mut discovery);
        drop(discovery);
        let state_root = coding_brain_core::lifecycle::coding_brain_state_root();
        let link_path = state_root.join("session-links.jsonl");
        let projection = match link_path.try_exists() {
            Ok(false) => coding_brain_core::session_links::SessionIdentityProjection::default(),
            Ok(true) => coding_brain_core::session_links::SessionLinkStore::at(&link_path)
                .read_projection()
                .map_err(|_| "session identity evidence is unavailable".to_string())?,
            Err(_) => return Err("session identity evidence is unavailable".into()),
        };
        let native = AgentSessionKey::native(request.target.provider, &request.target.session_id);
        let projected_live = projection.live_for(&native);
        let exact = sessions
            .iter()
            .filter(|session| {
                action_target_matches(
                    session,
                    request.target.provider,
                    request.target.provenance,
                    &request.target.session_id,
                    projected_live,
                )
            })
            .collect::<Vec<_>>();
        let session = match exact.as_slice() {
            [session] => *session,
            [] => return Err("no exact live provider session for action".into()),
            many => {
                return Err(format!(
                    "exact live provider session is ambiguous ({} matches)",
                    many.len()
                ));
            }
        };
        execute_guarded_action(session, request.action)
            .map(|_| ())
            .map_err(|error| bounded_display(&error))
    }
}

fn action_target_matches(
    session: &AgentSession,
    target_provider: AgentProvider,
    target_provenance: SessionTargetProvenance,
    target_session_id: &str,
    projected_live: Option<&coding_brain_core::provider::LiveProcessIdentity>,
) -> bool {
    let live = session.live_process_identity();
    if session.provider != target_provider || live.is_none() {
        return false;
    }
    let live = live.as_ref().expect("live identity checked above");
    match target_provenance {
        SessionTargetProvenance::Unknown => false,
        SessionTargetProvenance::Structured => {
            if let Some(projected_live) = projected_live {
                return live == projected_live;
            }
            session.session_id == target_session_id
                && session.identity_provenance
                    == coding_brain_core::session::SessionIdentityProvenance::Structured
        }
        SessionTargetProvenance::RecognizedProcessAttention => {
            process_session_id(live) == target_session_id
                || (is_process_only_session(session) && session.session_id == target_session_id)
        }
    }
}

fn process_session_id(identity: &coding_brain_core::provider::LiveProcessIdentity) -> String {
    format!(
        "live:{}:{}:{}:{}",
        identity.pid,
        identity.process_start_identity,
        identity.tty.len(),
        identity.tty
    )
}

#[cfg(test)]
fn discovery_process_session_id(
    identity: &coding_brain_core::provider::LiveProcessIdentity,
) -> String {
    format!(
        "process:{}:{}:{}:{}",
        identity.pid,
        identity.process_start_identity,
        identity.tty.len(),
        identity.tty
    )
}

fn is_process_only_session(session: &AgentSession) -> bool {
    session.identity_provenance
        == coding_brain_core::session::SessionIdentityProvenance::ProcessOnly
}

fn record_correction_at_path(path: &Path, correction: CorrectionInput) -> Result<(), String> {
    let store = brain::activity::ActivityStore::at(path);
    let source = store
        .read()
        .map_err(|error| error.to_string())?
        .events()
        .iter()
        .rev()
        .find(|event| event.activity_id == correction.activity_id)
        .cloned()
        .ok_or_else(|| format!("activity {} not found", correction.activity_id))?;
    if source.kind != ActivityKind::Decision {
        return Err(format!(
            "correction requires Decision activity; {} is {:?}",
            bounded_display(&source.activity_id),
            source.kind
        ));
    }
    store
        .append(ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: correction.activity_id,
            recorded_at_ms: epoch_ms(),
            project: source.project,
            session: source.session,
            state: coding_brain_core::brain_activity::ActivityState::Correction,
            tool: None,
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: source.decision_id,
            outcome: None,
            correction: Some(correction.disposition),
            note: correction.note,
            supersedes: None,
        })
        .map_err(|error| error.to_string())
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_actions_reject_unknown_authority_before_discovery() {
        let actions = LiveBrainActions::default();
        let request = SessionActionRequest {
            target: coding_brain_core::brain_activity::SessionTarget {
                provider: AgentProvider::Codex,
                session_id: "live:opaque-native".into(),
                turn_id: None,
                tool_use_id: None,
                project_id: coding_brain_core::project::ProjectId::Temporary("project".into()),
                cwd: "/work/project".into(),
                provider_hints: Vec::new(),
                provenance: SessionTargetProvenance::Unknown,
            },
            action: coding_brain_core::terminals::TerminalSessionAction::Continue,
        };

        assert_eq!(
            actions.send_session_action(request).unwrap_err(),
            "session action authority is unavailable"
        );
    }

    #[test]
    fn gate_mode_resolution_fails_closed_for_invalid_explicit_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("gate-mode");
        std::fs::write(&path, "garbage").unwrap();

        let resolved = brain::resolve_gate_mode_at(&path, Some(&config::BrainConfig::default()));

        assert_eq!(resolved.mode, BrainGateMode::Off);
        assert!(resolved.warning.is_some());
    }

    #[test]
    fn active_mode_without_config_uses_default_endpoint_config() {
        for mode in [BrainGateMode::On, BrainGateMode::Auto] {
            let config = endpoint_config_for_mode(None, mode).unwrap();

            assert_eq!(config.endpoint, config::BrainConfig::default().endpoint);
            assert_eq!(config.model, config::BrainConfig::default().model);
        }
        assert!(endpoint_config_for_mode(None, BrainGateMode::Off).is_none());
    }

    #[test]
    fn diagnostic_correction_is_rejected_without_appending() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let store = brain::activity::ActivityStore::at(path.clone());
        let activity_id = format!("diagnostic-{}", "x".repeat(200));
        let mut diagnostic = source_event(ActivityKind::Diagnostic);
        diagnostic.activity_id = activity_id.clone();
        store.append(diagnostic).unwrap();
        let before = std::fs::read(&path).unwrap();

        let error = record_correction_at_path(
            &path,
            CorrectionInput {
                activity_id: activity_id.clone(),
                disposition: CorrectionDisposition::BrainWrong,
                note: Some("not a decision".into()),
            },
        )
        .unwrap_err();

        assert!(error.contains("correction requires Decision activity"));
        assert!(error.contains("Diagnostic"));
        assert!(error.chars().count() <= 160);
        assert!(!error.contains(&activity_id));
        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn decision_correction_still_appends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let store = brain::activity::ActivityStore::at(path.clone());
        store.append(source_event(ActivityKind::Decision)).unwrap();

        record_correction_at_path(
            &path,
            CorrectionInput {
                activity_id: "activity-1".into(),
                disposition: CorrectionDisposition::BrainRight,
                note: Some("confirmed".into()),
            },
        )
        .unwrap();

        let events = store.read().unwrap();
        assert_eq!(events.events().len(), 2);
        let correction = &events.events()[1];
        assert_eq!(correction.kind, ActivityKind::Decision);
        assert_eq!(
            correction.state,
            coding_brain_core::brain_activity::ActivityState::Correction
        );
        assert_eq!(
            correction.correction,
            Some(CorrectionDisposition::BrainRight)
        );
        assert_eq!(correction.decision_id.as_deref(), Some("decision-1"));
    }

    #[test]
    fn scorecard_preserves_accuracy_abstention_and_dangerous_false_approval() {
        let mut decisions = vec![
            summary(
                "safe",
                "approve",
                Some("accept"),
                Some("Read"),
                Some("README.md"),
            ),
            summary(
                "dangerous",
                "approve",
                Some("reject"),
                Some("Bash"),
                Some("rm -rf /tmp/build"),
            ),
            summary(
                "abstain",
                "abstain",
                None,
                Some("Edit"),
                Some("src/main.rs"),
            ),
        ];
        decisions[1].provider = coding_brain_core::provider::AgentProvider::Claude;

        let scorecard = scorecard_from(&decisions, &[]);

        assert_eq!(scorecard.total_decisions, 3);
        assert_eq!(scorecard.brain_decisions, 2);
        assert_eq!(scorecard.correct_decisions, 1);
        assert_eq!(scorecard.abstentions, 1);
        assert_eq!(scorecard.dangerous_false_approvals, 1);
        assert_eq!(
            scorecard.providers,
            vec![
                coding_brain_core::runtime::ProviderScoreSummary {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    decisions: 1,
                    correct: 1,
                },
                coding_brain_core::runtime::ProviderScoreSummary {
                    provider: coding_brain_core::provider::AgentProvider::Claude,
                    decisions: 1,
                    correct: 0,
                },
            ]
        );
    }

    #[test]
    fn hook_proposals_are_unscored_until_the_user_corrects_them() {
        let decisions = vec![
            summary(
                "right",
                "approve",
                Some("hook_proposal"),
                Some("Read"),
                Some("README.md"),
            ),
            summary(
                "wrong",
                "approve",
                Some("hook_proposal"),
                Some("Bash"),
                Some("rm -rf /tmp/build"),
            ),
            summary(
                "unscored",
                "deny",
                Some("hook_proposal"),
                Some("Bash"),
                Some("cargo test"),
            ),
        ];
        let events = vec![
            correction("right", CorrectionDisposition::BrainRight),
            correction("wrong", CorrectionDisposition::BrainWrong),
        ];

        let scorecard = scorecard_from(&decisions, &events);

        assert_eq!(scorecard.total_decisions, 3);
        assert_eq!(scorecard.brain_decisions, 2);
        assert_eq!(scorecard.correct_decisions, 1);
        assert_eq!(scorecard.accuracy_pct, 50.0);
        assert_eq!(scorecard.dangerous_false_approvals, 1);
    }

    #[test]
    fn brain_wrong_correction_enters_review_but_brain_right_does_not() {
        let wrong = review_queue_from(
            vec![review_record()],
            &[correction("review", CorrectionDisposition::BrainWrong)],
        );
        assert_eq!(wrong.len(), 1);
        assert_eq!(wrong[0].decision.id, "review");
        assert_eq!(
            wrong[0].reason,
            "Critical-tier false-approve (safety review)"
        );

        let right = review_queue_from(
            vec![review_record()],
            &[correction("review", CorrectionDisposition::BrainRight)],
        );
        assert!(right.is_empty());
    }

    #[test]
    fn persisted_brain_wrong_correction_updates_review_and_scorecard_projections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("activity.jsonl");
        let store = brain::activity::ActivityStore::at(path.clone());
        let mut source = source_event(ActivityKind::Decision);
        source.decision_id = Some("review".into());
        store.append(source).unwrap();

        record_correction_at_path(
            &path,
            CorrectionInput {
                activity_id: "activity-1".into(),
                disposition: CorrectionDisposition::BrainWrong,
                note: None,
            },
        )
        .unwrap();

        let events = store.read().unwrap();
        let review = review_queue_from(vec![review_record()], events.events());
        let scorecard = scorecard_from(
            &[summary(
                "review",
                "approve",
                Some("hook_proposal"),
                Some("Bash"),
                Some("rm -rf /tmp/build"),
            )],
            events.events(),
        );

        assert_eq!(review.len(), 1);
        assert_eq!(review[0].decision.id, "review");
        assert_eq!(scorecard.total_decisions, 1);
        assert_eq!(scorecard.brain_decisions, 1);
        assert_eq!(scorecard.correct_decisions, 0);
        assert_eq!(scorecard.accuracy_pct, 0.0);
    }

    #[test]
    fn review_preserves_durable_decision_provider() {
        let mut record = review_record();
        record.provider = AgentProvider::Antigravity;

        let queue = review_queue_from(
            vec![record],
            &[correction("review", CorrectionDisposition::BrainWrong)],
        );

        assert_eq!(queue[0].decision.provider, AgentProvider::Antigravity);
    }

    #[test]
    fn action_target_match_requires_provenance_provider_and_exact_identity() {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let session = discovered_session(provider, "native-1");
            let live = session.live_process_identity().unwrap();
            let synthetic_live = process_session_id(&live);

            assert!(action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::Structured,
                "native-1",
                None
            ));
            assert!(!action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::Structured,
                &synthetic_live,
                None
            ));
            assert!(action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::RecognizedProcessAttention,
                &synthetic_live,
                None
            ));
            assert!(!action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::Unknown,
                "native-1",
                None
            ));
        }
    }

    #[test]
    fn structured_target_cannot_collide_with_process_only_identity() {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let mut session = discovered_session(provider, "placeholder");
            let live = session.live_process_identity().unwrap();
            let synthetic_process = discovery_process_session_id(&live);
            session.session_id = synthetic_process.clone();
            session.identity_provenance =
                coding_brain_core::session::SessionIdentityProvenance::ProcessOnly;

            assert!(!action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::Structured,
                &synthetic_process,
                None
            ));
            assert!(action_target_matches(
                &session,
                provider,
                SessionTargetProvenance::RecognizedProcessAttention,
                &synthetic_process,
                None
            ));
            let wrong_provider = if provider == AgentProvider::Codex {
                AgentProvider::Claude
            } else {
                AgentProvider::Codex
            };
            assert!(!action_target_matches(
                &session,
                wrong_provider,
                SessionTargetProvenance::RecognizedProcessAttention,
                &synthetic_process,
                None
            ));
        }
    }

    #[test]
    fn structured_target_uses_trusted_link_and_keeps_opaque_native_prefixes() {
        let session = discovered_session(AgentProvider::Claude, "live:opaque-native");
        let live = session.live_process_identity().unwrap();
        let wrong_live = coding_brain_core::provider::LiveProcessIdentity::try_new(
            AgentProvider::Claude,
            77,
            8_001,
            "pts/8",
        )
        .unwrap();
        assert!(!action_target_matches(
            &session,
            AgentProvider::Claude,
            SessionTargetProvenance::Structured,
            "live:opaque-native",
            Some(&wrong_live)
        ));
        assert!(action_target_matches(
            &session,
            AgentProvider::Claude,
            SessionTargetProvenance::Structured,
            "linked-native",
            Some(&live)
        ));
        assert!(action_target_matches(
            &session,
            AgentProvider::Claude,
            SessionTargetProvenance::Structured,
            "live:opaque-native",
            None
        ));
    }

    #[test]
    fn structured_discovery_identity_is_not_inferred_from_process_shaped_text() {
        let mut session = discovered_session(AgentProvider::Claude, "placeholder");
        let live = session.live_process_identity().unwrap();
        let colliding_id = discovery_process_session_id(&live);
        session.session_id = colliding_id.clone();

        assert!(action_target_matches(
            &session,
            AgentProvider::Claude,
            SessionTargetProvenance::Structured,
            &colliding_id,
            None
        ));
        assert!(!action_target_matches(
            &session,
            AgentProvider::Claude,
            SessionTargetProvenance::RecognizedProcessAttention,
            &colliding_id,
            None
        ));
    }

    #[test]
    fn endpoint_probe_returns_immediately_while_slow_check_runs_in_background() {
        fn slow_probe(_endpoint: &str) -> bool {
            std::thread::sleep(Duration::from_millis(500));
            true
        }

        let source = LiveBrainSource::with_probe(slow_probe);
        let started = Instant::now();
        let initial = source.endpoint_health_for("http://fixture", "fixture-model");

        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(!initial.reachable);
        assert_eq!(initial.model.as_deref(), Some("fixture-model"));

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if source
                .endpoint_health_for("http://fixture", "fixture-model")
                .reachable
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("background endpoint probe did not publish its result");
    }

    fn summary(
        id: &str,
        action: &str,
        user_action: Option<&str>,
        tool: Option<&str>,
        command: Option<&str>,
    ) -> DecisionSummary {
        DecisionSummary {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            id: id.into(),
            timestamp: "1".into(),
            action: action.into(),
            confidence: Some(0.9),
            project: Some("project".into()),
            tool: tool.map(str::to_owned),
            pid: 1,
            command: command.map(str::to_owned),
            reasoning: None,
            user_action: user_action.map(str::to_owned),
            override_reason: None,
            brain_decision_ms: None,
            canonical: None,
            cache_hit: None,
            cost_usd: None,
            model: None,
            outcome_kind: None,
            outcome_detail: None,
            suggested_at: None,
            resolved_at: None,
        }
    }

    fn correction(
        decision_id: &str,
        disposition: CorrectionDisposition,
    ) -> coding_brain_core::brain_activity::ActivityEvent {
        coding_brain_core::brain_activity::ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: format!("activity-{decision_id}"),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Stable("project".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: coding_brain_core::brain_activity::ActivityState::Correction,
            tool: None,
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: Some(decision_id.into()),
            outcome: None,
            correction: Some(disposition),
            note: None,
            supersedes: None,
        }
    }

    fn source_event(kind: ActivityKind) -> ActivityEvent {
        ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind,
            activity_id: "activity-1".into(),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Stable("project".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: if kind == ActivityKind::Diagnostic {
                coding_brain_core::brain_activity::ActivityState::Error
            } else {
                coding_brain_core::brain_activity::ActivityState::Denied
            },
            tool: None,
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: (kind == ActivityKind::Decision).then(|| "decision-1".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn review_record() -> brain::decisions::DecisionRecord {
        brain::decisions::DecisionRecord {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            timestamp: "1".into(),
            pid: 1,
            project: "project".into(),
            tool: Some("Bash".into()),
            command: Some("rm -rf /tmp/build".into()),
            brain_action: "approve".into(),
            brain_confidence: 0.95,
            brain_reasoning: "fixture".into(),
            user_action: "hook_proposal".into(),
            context: None,
            outcome: None,
            decision_type: brain::decisions::DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some("review".into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    fn discovered_session(provider: AgentProvider, id: &str) -> AgentSession {
        let mut session = AgentSession::from_raw(coding_brain_core::session::RawAgentSession {
            provider,
            pid: 42,
            process_start_identity: Some(9_001),
            session_id: id.into(),
            cwd: "/work/provider".into(),
            started_at: 1,
        });
        session.tty = "pts/7".into();
        session.identity_provenance =
            coding_brain_core::session::SessionIdentityProvenance::Structured;
        session
    }
}
