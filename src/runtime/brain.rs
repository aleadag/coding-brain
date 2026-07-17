//! Bind Brain read contracts to the binary's brain subsystem.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codexctl_core::brain_activity::{
    ActivityEvent, ActivitySnapshot, CorrectionDisposition, SnapshotLimits,
};
use codexctl_core::runtime::{
    BrainGateMode, BrainSource, BrainView, CacheSummary, CounterfactualSummary, DecisionSummary,
    EndpointHealth, LatencySummary, ReviewItemSummary, RiskTierSummary, ScorecardSummary,
};

use crate::{brain, config};

pub struct LiveBrainView;

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

impl BrainView for LiveBrainView {
    fn gate_mode(&self) -> BrainGateMode {
        parse_gate_mode(&brain::read_gate_mode())
    }

    fn recent_decisions(&self, n: usize) -> Vec<DecisionSummary> {
        let mut all = brain::decisions::read_all_decisions();
        // brain::decisions::read_all_decisions returns oldest-first; the TUI
        // wants newest-first.
        all.reverse();
        all.into_iter().take(n).map(summary_from_record).collect()
    }

    fn decision_count(&self) -> usize {
        brain::decisions::read_all_decisions().len()
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
        parse_gate_mode(&brain::read_gate_mode())
    }

    fn endpoint_health(&self) -> EndpointHealth {
        let Some(brain_config) = config::Config::load().brain else {
            return EndpointHealth {
                detail: Some("Local model is not configured".into()),
                ..EndpointHealth::default()
            };
        };
        self.endpoint_health_for(&brain_config.endpoint, &brain_config.model)
    }
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
        .map(super::brain_review::item_summary_from)
        .collect()
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
    codexctl_core::brain_activity::redact_activity_text(value)
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

/// String → enum. Unknown values fall back to `On` to match
/// `brain::read_gate_mode`'s "no file" default.
fn parse_gate_mode(raw: &str) -> BrainGateMode {
    match raw.trim().to_lowercase().as_str() {
        "off" => BrainGateMode::Off,
        "auto" => BrainGateMode::Auto,
        _ => BrainGateMode::On,
    }
}

fn summary_from_record(r: brain::decisions::DecisionRecord) -> DecisionSummary {
    DecisionSummary::from(&r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_mode_parsing_recognizes_known_values() {
        assert_eq!(parse_gate_mode("on"), BrainGateMode::On);
        assert_eq!(parse_gate_mode("OFF"), BrainGateMode::Off);
        assert_eq!(parse_gate_mode(" auto "), BrainGateMode::Auto);
    }

    #[test]
    fn gate_mode_parsing_falls_back_to_on() {
        // Matches the file-missing default in `brain::read_gate_mode`.
        assert_eq!(parse_gate_mode(""), BrainGateMode::On);
        assert_eq!(parse_gate_mode("garbage"), BrainGateMode::On);
    }

    #[test]
    fn scorecard_preserves_accuracy_abstention_and_dangerous_false_approval() {
        let decisions = vec![
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

        let scorecard = scorecard_from(&decisions, &[]);

        assert_eq!(scorecard.total_decisions, 3);
        assert_eq!(scorecard.brain_decisions, 2);
        assert_eq!(scorecard.correct_decisions, 1);
        assert_eq!(scorecard.abstentions, 1);
        assert_eq!(scorecard.dangerous_false_approvals, 1);
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
    ) -> codexctl_core::brain_activity::ActivityEvent {
        codexctl_core::brain_activity::ActivityEvent {
            schema_version: codexctl_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            activity_id: format!("activity-{decision_id}"),
            recorded_at_ms: 1,
            project: codexctl_core::brain_activity::ProjectEvidence {
                project_id: codexctl_core::project::ProjectId::Stable("project".into()),
                cwd: "/work/project".into(),
                label: Some("project".into()),
            },
            session: None,
            state: codexctl_core::brain_activity::ActivityState::Correction,
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

    fn review_record() -> brain::decisions::DecisionRecord {
        brain::decisions::DecisionRecord {
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
}
