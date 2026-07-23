#![allow(dead_code)]

use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};

use super::preferences::{
    DistilledPreferences, PreferenceCondition, PreferencePattern, TemporalPattern, ToolAccuracy,
};

// ────────────────────────────────────────────────────────────────────────────
// JSON serialization
// ────────────────────────────────────────────────────────────────────────────

/// Convert DistilledPreferences to serde_json::Value for saving.
pub(crate) fn preferences_to_json(prefs: &DistilledPreferences) -> serde_json::Value {
    serde_json::json!({
        "patterns": prefs.patterns.iter().map(|p| {
            serde_json::json!({
                "tool": p.tool,
                "command_pattern": p.command_pattern,
                "preferred_action": p.preferred_action,
                "sample_count": p.sample_count,
                "accept_rate": p.accept_rate,
                "conditions": p.conditions.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
                "confidence": p.confidence,
            })
        }).collect::<Vec<_>>(),
        "tool_accuracy": prefs.tool_accuracy.iter().map(|ta| {
            serde_json::json!({
                "tool": ta.tool,
                "total": ta.total,
                "correct": ta.correct,
                "confidence_threshold": ta.confidence_threshold,
            })
        }).collect::<Vec<_>>(),
        "total_decisions": prefs.total_decisions,
        "overall_accuracy": prefs.overall_accuracy,
        "temporal": prefs.temporal.iter().map(|tp| {
            serde_json::json!({
                "description": tp.description,
                "sample_count": tp.sample_count,
                "strength": tp.strength,
            })
        }).collect::<Vec<_>>(),
    })
}

/// Parse a DistilledPreferences from JSON.
pub(crate) fn parse_preferences_json(json: &serde_json::Value) -> Option<DistilledPreferences> {
    let patterns = json
        .get("patterns")?
        .as_array()?
        .iter()
        .filter_map(|p| {
            let conditions = p
                .get("conditions")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(PreferenceCondition::from_json)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let confidence = p.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0);
            Some(PreferencePattern {
                tool: p.get("tool")?.as_str()?.to_string(),
                command_pattern: p
                    .get("command_pattern")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                preferred_action: p.get("preferred_action")?.as_str()?.to_string(),
                sample_count: p.get("sample_count")?.as_u64()? as u32,
                accept_rate: p.get("accept_rate")?.as_f64()?,
                conditions,
                confidence,
            })
        })
        .collect();

    let tool_accuracy = json
        .get("tool_accuracy")?
        .as_array()?
        .iter()
        .filter_map(|ta| {
            Some(ToolAccuracy {
                tool: ta.get("tool")?.as_str()?.to_string(),
                total: ta.get("total")?.as_u64()? as u32,
                correct: ta.get("correct")?.as_u64()? as u32,
                confidence_threshold: ta.get("confidence_threshold")?.as_f64()?,
            })
        })
        .collect();

    let temporal = json
        .get("temporal")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|tp| {
                    Some(TemporalPattern {
                        description: tp.get("description")?.as_str()?.to_string(),
                        sample_count: tp.get("sample_count")?.as_u64()? as u32,
                        strength: tp.get("strength")?.as_f64()?,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(DistilledPreferences {
        patterns,
        tool_accuracy,
        total_decisions: json.get("total_decisions")?.as_u64()? as u32,
        overall_accuracy: json.get("overall_accuracy")?.as_f64()?,
        temporal,
    })
}

/// Load distilled preferences from disk.
pub fn load_preferences() -> Option<DistilledPreferences> {
    super::distill::load_global(&current_paths()?)
}

/// Minimum number of per-project decisions before using project-specific preferences.
const MIN_PROJECT_DECISIONS: usize = 10;

/// Load distilled preferences for a specific project.
/// Falls back to global preferences when the project has fewer than
/// `MIN_PROJECT_DECISIONS` decisions.
pub fn load_preferences_for_project(project: &str) -> Option<DistilledPreferences> {
    let paths = current_paths()?;
    if let Some(preferences) = super::distill::load_project(&paths, project) {
        if preferences.total_decisions >= MIN_PROJECT_DECISIONS as u32 {
            return Some(preferences);
        }
    }
    super::distill::load_global(&paths)
}

fn current_paths() -> Option<CodingBrainPaths> {
    CodingBrainPaths::resolve(&PathEnvironment::current()).ok()
}

/// Get the adaptive confidence threshold for a specific tool.
/// Returns None if no preference data exists (use default threshold).
pub fn adaptive_threshold(tool: Option<&str>) -> Option<f64> {
    let prefs = load_preferences()?;
    let tool_name = tool?;
    prefs
        .tool_accuracy
        .iter()
        .find(|ta| ta.tool == tool_name)
        .map(|ta| ta.confidence_threshold)
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::decisions::{DecisionRecord, DecisionType, project_slug};
    use super::super::preferences::distill_preferences;
    use super::*;

    fn make_decision(tool: &str, project: &str, user_action: &str) -> DecisionRecord {
        DecisionRecord {
            provider: coding_brain_core::provider::AgentProvider::Codex,
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

    #[test]
    fn test_preferences_to_json_roundtrip() {
        let prefs = DistilledPreferences {
            patterns: vec![PreferencePattern {
                tool: "Bash".into(),
                command_pattern: Some("cargo test".into()),
                preferred_action: "approve".into(),
                sample_count: 10,
                accept_rate: 0.9,
                conditions: vec![PreferenceCondition::HourRange(8, 18)],
                confidence: 0.8,
            }],
            tool_accuracy: vec![ToolAccuracy {
                tool: "Bash".into(),
                total: 10,
                correct: 9,
                confidence_threshold: 0.5,
            }],
            total_decisions: 10,
            overall_accuracy: 0.9,
            temporal: vec![TemporalPattern {
                description: "test pattern".into(),
                sample_count: 5,
                strength: 0.8,
            }],
        };

        let json = preferences_to_json(&prefs);
        let parsed = parse_preferences_json(&json).unwrap();

        assert_eq!(parsed.patterns.len(), 1);
        assert_eq!(parsed.patterns[0].tool, "Bash");
        assert_eq!(parsed.tool_accuracy.len(), 1);
        assert_eq!(parsed.total_decisions, 10);
        assert!((parsed.overall_accuracy - 0.9).abs() < f64::EPSILON);
        assert_eq!(parsed.temporal.len(), 1);
    }

    #[test]
    fn test_project_slug() {
        assert_eq!(project_slug("my-project"), "my-project");
        assert_eq!(project_slug("My Project"), "my_project");
        assert_eq!(project_slug("/tmp/foo/bar"), "_tmp_foo_bar");
        assert_eq!(project_slug("proj_123"), "proj_123");
        assert_eq!(project_slug(""), "unknown");
        assert_eq!(project_slug("   "), "unknown");
    }

    #[test]
    fn test_project_filtered_decisions() {
        let decisions = [
            make_decision("Bash", "alpha", "accept"),
            make_decision("Bash", "beta", "reject"),
            make_decision("Read", "alpha", "accept"),
            make_decision("Read", "beta", "accept"),
        ];

        let alpha: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| d.project.to_lowercase() == "alpha")
            .collect();
        assert_eq!(alpha.len(), 2);
        assert!(alpha.iter().all(|d| d.project == "alpha"));

        let beta: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| d.project.to_lowercase() == "beta")
            .collect();
        assert_eq!(beta.len(), 2);
    }

    #[test]
    fn test_project_distillation_with_enough_data() {
        // 12 decisions for "alpha" — above MIN_PROJECT_DECISIONS threshold
        let decisions: Vec<DecisionRecord> = (0..12)
            .map(|_| make_decision("Read", "alpha", "accept"))
            .collect();

        let project_decisions: Vec<DecisionRecord> = decisions
            .iter()
            .filter(|d| d.project == "alpha")
            .cloned()
            .collect();

        assert!(project_decisions.len() >= MIN_PROJECT_DECISIONS);
        let prefs = distill_preferences(&project_decisions);
        assert!(!prefs.patterns.is_empty());
    }

    #[test]
    fn test_project_fallback_with_insufficient_data() {
        // Only 5 decisions for "tiny-proj" — below threshold, should need fallback
        let decisions: Vec<DecisionRecord> = (0..5)
            .map(|_| make_decision("Read", "tiny-proj", "accept"))
            .collect();

        let project_decisions: Vec<DecisionRecord> = decisions
            .iter()
            .filter(|d| d.project == "tiny-proj")
            .cloned()
            .collect();

        assert!(project_decisions.len() < MIN_PROJECT_DECISIONS);
    }
}
