#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

/// Prompt template names.
pub const ADVISORY: &str = "advisory";
pub const AUTOPSY: &str = "autopsy";

/// Load a prompt template by name. Checks user overrides first, falls back to built-in.
pub fn load(name: &str) -> String {
    // Check a user override under the Coding Brain state root.
    if let Some(path) = user_prompt_path(name) {
        if let Ok(content) = fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }

    // Fall back to built-in default
    builtin(name).to_string()
}

/// Expand template variables in a prompt string.
pub fn expand(template: &str, vars: &[(&str, &str)]) -> String {
    let mut result = template.to_string();
    for (key, value) in vars {
        result = result.replace(&format!("{{{{{key}}}}}"), value);
    }
    result
}

/// Get the user override path for a prompt.
fn user_prompt_path(name: &str) -> Option<PathBuf> {
    codexctl_core::paths::CodingBrainPaths::resolve(
        &codexctl_core::paths::PathEnvironment::current(),
    )
    .ok()
    .map(|paths| {
        paths
            .state_root()
            .join("brain/prompts")
            .join(format!("{name}.md"))
    })
}

/// Return the built-in default prompt for a given name.
fn builtin(name: &str) -> &'static str {
    match name {
        ADVISORY => ADVISORY_PROMPT,
        AUTOPSY => AUTOPSY_PROMPT,
        _ => {
            "Respond with JSON: {\"action\": \"deny\", \"reasoning\": \"unknown prompt\", \"confidence\": 0.0}"
        }
    }
}

/// List all available prompt names and their source (builtin vs user override).
pub fn list_prompts() -> Vec<(String, String)> {
    let names = [ADVISORY, AUTOPSY];
    names
        .iter()
        .map(|name| {
            let source = if user_prompt_path(name).as_ref().is_some_and(|p| p.exists()) {
                "user override"
            } else {
                "built-in"
            };
            (name.to_string(), source.to_string())
        })
        .collect()
}

// ────────────────────────────────────────────────────────────────────────────
// Built-in prompt templates
// ────────────────────────────────────────────────────────────────────────────

const ADVISORY_PROMPT: &str = r#"You are a session supervisor for Codex. Analyze the session state and recent conversation to decide whether to approve or deny the pending tool call.

## Session State
{{session_summary}}{{git_context}}

## Recent Conversation
{{recent_transcript}}{{few_shot_examples}}

## Decision
{{decision_prompt}}"#;

const AUTOPSY_PROMPT: &str = r#"You are analyzing a completed Codex session post-mortem. Given the session statistics and detected issues, suggest what the session should have done differently.

## Session Summary
{{session_summary}}

## Detected Issues
{{findings}}

## Cost Breakdown
{{cost_breakdown}}

Provide 3-5 concise, actionable suggestions for what the session should have done differently. Focus on strategy, not syntax. Each suggestion should be one sentence.

Respond with JSON: {"suggestions": ["...", "..."]}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_advisory_exists() {
        let prompt = builtin(ADVISORY);
        assert!(prompt.contains("session supervisor"));
        assert!(prompt.contains("{{session_summary}}"));
    }

    #[test]
    fn advisory_prompt_has_no_external_coordination_slots() {
        let prompt = builtin(ADVISORY);
        assert!(!prompt.contains("coordination_context"));
        assert!(!prompt.contains("hive_context"));
    }

    #[test]
    fn builtin_autopsy_exists() {
        let prompt = builtin(AUTOPSY);
        assert!(prompt.contains("post-mortem"));
        assert!(prompt.contains("{{session_summary}}"));
        assert!(prompt.contains("{{findings}}"));
    }

    #[test]
    fn expand_replaces_variables() {
        let template = "Hello {{name}}, you have {{count}} items.";
        let result = expand(template, &[("name", "Alice"), ("count", "3")]);
        assert_eq!(result, "Hello Alice, you have 3 items.");
    }

    #[test]
    fn expand_no_variables_unchanged() {
        let template = "No variables here.";
        let result = expand(template, &[]);
        assert_eq!(result, "No variables here.");
    }

    #[test]
    fn load_falls_back_to_builtin() {
        // No user override exists, should return built-in
        let prompt = load(ADVISORY);
        assert!(prompt.contains("session supervisor"));
    }

    #[test]
    fn list_prompts_returns_all() {
        let prompts = list_prompts();
        assert_eq!(prompts.len(), 2);
        assert!(prompts.iter().any(|(n, _)| n == ADVISORY));
        assert!(prompts.iter().any(|(n, _)| n == AUTOPSY));
    }

    #[test]
    fn load_user_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.md");
        fs::write(&path, "Custom prompt for {{name}}").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let result = expand(&content, &[("name", "testing")]);
        assert_eq!(result, "Custom prompt for testing");
    }
}
