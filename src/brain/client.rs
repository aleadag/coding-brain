#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::BrainConfig;
use crate::rules::RuleAction;

/// The brain's suggestion for a session, parsed from the LLM response.
#[derive(Debug, Clone)]
pub struct BrainSuggestion {
    pub action: RuleAction,
    pub message: Option<String>,
    pub reasoning: String,
    pub confidence: f64,
    /// Epoch seconds when this suggestion was created.
    /// Used by time-to-correct analysis to measure user reaction latency.
    pub suggested_at: u64,
}

/// Call the local LLM endpoint via curl and parse the response.
pub fn infer(config: &BrainConfig, prompt: &str) -> Result<BrainSuggestion, String> {
    infer_with_program(config, prompt, Path::new("curl"))
}

fn infer_with_program(
    config: &BrainConfig,
    prompt: &str,
    program: &Path,
) -> Result<BrainSuggestion, String> {
    let is_openai = is_openai_compatible(&config.endpoint);

    let payload = if is_openai {
        // OpenAI-compatible format (llama.cpp, vLLM, LM Studio)
        serde_json::json!({
            "model": config.model,
            "messages": [
                {"role": "user", "content": prompt}
            ],
            "response_format": {"type": "json_object"},
            "stream": false,
        })
    } else {
        // Ollama /api/generate format (default)
        serde_json::json!({
            "model": config.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
        })
    };

    let body = serde_json::to_string(&payload).map_err(|e| format!("json error: {e}"))?;
    let stdout = curl_post(program, config, &body)?;
    let stdout = String::from_utf8_lossy(&stdout);
    if is_openai {
        parse_openai_response(&stdout)
    } else {
        parse_ollama_response(&stdout)
    }
}

fn curl_post(program: &Path, config: &BrainConfig, body: &str) -> Result<Vec<u8>, String> {
    let timeout_secs = ((config.timeout_ms / 1000).max(1)).to_string();
    let mut child = Command::new(program)
        .args([
            "--silent",
            "--show-error",
            "--request",
            "POST",
            "--header",
            "Content-Type: application/json",
            "--max-redirs",
            "0",
            "--max-filesize",
            "1048576",
            "--data-binary",
            "@-",
            "--max-time",
            &timeout_secs,
            &config.endpoint,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("curl failed: {e}"))?;

    let stdout = child.stdout.take().expect("piped curl stdout");
    let stderr = child.stderr.take().expect("piped curl stderr");
    let stdout_reader = std::thread::spawn(move || read_bounded_draining(stdout, 1024 * 1024));
    let stderr_reader = std::thread::spawn(move || read_bounded_draining(stderr, 64 * 1024));
    let write_result = child
        .stdin
        .take()
        .expect("piped curl stdin")
        .write_all(body.as_bytes());
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        return Err(format!("curl stdin failed: {error}"));
    }
    let status = child
        .wait()
        .map_err(|error| format!("curl wait failed: {error}"))?;
    let (stdout, stdout_exceeded) = stdout_reader
        .join()
        .map_err(|_| "curl stdout reader panicked".to_string())?
        .map_err(|error| format!("curl stdout failed: {error}"))?;
    let (stderr, _) = stderr_reader
        .join()
        .map_err(|_| "curl stderr reader panicked".to_string())?
        .map_err(|error| format!("curl stderr failed: {error}"))?;

    if stdout_exceeded {
        return Err("curl response exceeds 1 MiB".into());
    }

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!("curl error (exit {status}): {stderr}"));
    }

    Ok(stdout)
}

fn read_bounded_draining(mut reader: impl Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        let keep = read.min(remaining);
        retained.extend_from_slice(&buffer[..keep]);
        exceeded |= keep < read;
    }
    Ok((retained, exceeded))
}

/// Detect if the endpoint is OpenAI-compatible based on URL path.
fn is_openai_compatible(endpoint: &str) -> bool {
    endpoint.contains("/v1/chat") || endpoint.contains("/v1/completions")
}

/// Make an LLM API call, auto-detecting ollama vs OpenAI format from the endpoint URL.
pub fn complete(config: &BrainConfig, prompt: &str) -> Result<String, String> {
    call_llm(config, prompt)
}

pub fn infer_recovery(
    config: &BrainConfig,
    prompt: &str,
) -> Result<super::recovery::RecoverySuggestion, String> {
    parse_recovery_suggestion_json(&complete(config, prompt)?)
}

/// Make an LLM API call, auto-detecting ollama vs OpenAI format from the endpoint URL.
fn call_llm(config: &BrainConfig, prompt: &str) -> Result<String, String> {
    let is_openai = is_openai_compatible(&config.endpoint);

    let payload = if is_openai {
        serde_json::json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}],
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": config.model,
            "prompt": prompt,
            "stream": false,
        })
    };

    let body = serde_json::to_string(&payload).map_err(|e| format!("json error: {e}"))?;
    let stdout = curl_post(Path::new("curl"), config, &body)?;
    let stdout = String::from_utf8_lossy(&stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("invalid response: {e}"))?;

    if is_openai {
        // OpenAI: choices[0].message.content
        Ok(json
            .get("choices")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|msg| msg.get("content"))
            .and_then(|v| v.as_str())
            .unwrap_or(&stdout)
            .to_string())
    } else {
        // Ollama: response field
        Ok(json
            .get("response")
            .and_then(|v| v.as_str())
            .unwrap_or(&stdout)
            .to_string())
    }
}

/// Parse the ollama `/api/generate` response format.
fn parse_ollama_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    // Ollama wraps the generated text in a "response" field
    let generated = json
        .get("response")
        .and_then(|v| v.as_str())
        .unwrap_or(response);

    parse_suggestion_json(generated)
}

/// Parse OpenAI-compatible /v1/chat/completions response.
fn parse_openai_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    // OpenAI format: choices[0].message.content
    let content = json
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|msg| msg.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or(response);

    parse_suggestion_json(content)
}

/// Parse the structured JSON that the brain LLM is expected to produce.
pub fn parse_suggestion_json(text: &str) -> Result<BrainSuggestion, String> {
    // The LLM should produce JSON like:
    // {"action": "approve", "message": null, "reasoning": "safe command", "confidence": 0.95}
    let json: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid suggestion JSON: {e}"))?;

    let action_str = json
        .get("action")
        .and_then(|v| v.as_str())
        .ok_or("missing 'action' field")?;

    let action =
        RuleAction::parse(action_str).ok_or_else(|| format!("unknown action '{action_str}'"))?;

    let message = json
        .get("message")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let reasoning = json
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let confidence = json
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);

    Ok(BrainSuggestion {
        action,
        message,
        reasoning,
        confidence: confidence.clamp(0.0, 1.0),
        suggested_at: epoch_secs(),
    })
}

pub fn parse_recovery_suggestion_json(
    text: &str,
) -> Result<super::recovery::RecoverySuggestion, String> {
    use super::recovery::{RecoveryDecision, RecoverySuggestion};

    let json: serde_json::Value = serde_json::from_str(text.trim())
        .map_err(|_| "invalid recovery suggestion JSON".to_string())?;
    let action = json
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing recovery 'action' field")?;
    let decision = match action {
        "continue" => RecoveryDecision::Continue("continue".into()),
        "leave_alone" => RecoveryDecision::LeaveAlone,
        _ => return Err(format!("unknown recovery action '{action}'")),
    };
    let confidence = json
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    let reasoning = match decision {
        RecoveryDecision::Continue(_) => "local model selected continuation",
        RecoveryDecision::LeaveAlone => "local model declined continuation",
    }
    .into();
    Ok(RecoverySuggestion {
        decision,
        reasoning,
        confidence,
        suggested_at: epoch_secs(),
    })
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[cfg(unix)]
    fn fake_curl(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("curl");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (temp, path)
    }

    #[cfg(unix)]
    #[test]
    fn inference_sends_prompt_only_over_stdin_and_disables_redirects() {
        let (temp, curl) = fake_curl(
            r#"printf '%s\n' "$@" > "${0}.args"
dd of="${0}.stdin" 2>/dev/null
printf '%s' '{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}"}'"#,
        );
        let config = BrainConfig {
            endpoint: "http://brain.example.test/api/generate".into(),
            ..BrainConfig::default()
        };
        let secret_prompt = "unique prompt fragment";

        let suggestion = infer_with_program(&config, secret_prompt, &curl).unwrap();

        assert_eq!(suggestion.action, RuleAction::Approve);
        let args = std::fs::read_to_string(temp.path().join("curl.args")).unwrap();
        assert!(!args.contains(secret_prompt));
        assert!(args.contains("--data-binary\n@-"));
        assert!(args.contains("--max-redirs\n0"));
        assert!(args.contains("--max-filesize\n1048576"));
        assert!(args.contains(&config.endpoint));
        let stdin = std::fs::read_to_string(temp.path().join("curl.stdin")).unwrap();
        assert!(stdin.contains(secret_prompt));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_inference_response_abstains() {
        let (_temp, curl) = fake_curl("dd if=/dev/zero bs=1048577 count=1 2>/dev/null");
        let error = infer_with_program(&BrainConfig::default(), "prompt", &curl).unwrap_err();
        assert!(error.contains("exceeds 1 MiB"), "{error}");
    }

    #[test]
    fn parse_approve_suggestion() {
        let json = r#"{"action": "approve", "reasoning": "safe read command", "confidence": 0.95}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Approve);
        assert_eq!(s.reasoning, "safe read command");
        assert!((s.confidence - 0.95).abs() < f64::EPSILON);
        assert!(s.message.is_none());
    }

    #[test]
    fn parse_deny_suggestion() {
        let json = r#"{"action": "deny", "reasoning": "dangerous command", "confidence": 0.99}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.action, RuleAction::Deny);
    }

    #[test]
    fn parse_missing_action_fails() {
        let json = r#"{"reasoning": "no action"}"#;
        assert!(parse_suggestion_json(json).is_err());
    }

    #[test]
    fn parse_unknown_action_fails() {
        for action in ["send", "terminate", "route", "spawn", "dance"] {
            let json = format!(r#"{{"action":"{action}","reasoning":"invalid"}}"#);
            assert!(parse_suggestion_json(&json).is_err(), "{action}");
        }
    }

    #[test]
    fn delegate_suggestion_is_rejected() {
        let json = r#"{"action":"delegate","agent":"reviewer","delegate_prompt":"review"}"#;
        assert!(parse_suggestion_json(json).is_err());
    }

    #[test]
    fn parse_confidence_clamped() {
        let json = r#"{"action": "approve", "reasoning": "test", "confidence": 1.5}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert!((s.confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_ollama_wrapped_response() {
        let ollama_response = r#"{"model":"gemma4","response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}","done":true}"#;
        let s = parse_ollama_response(ollama_response).unwrap();
        assert_eq!(s.action, RuleAction::Approve);
    }

    #[test]
    fn defaults_on_missing_optional_fields() {
        let json = r#"{"action": "approve"}"#;
        let s = parse_suggestion_json(json).unwrap();
        assert_eq!(s.reasoning, "");
        assert!((s.confidence - 0.5).abs() < f64::EPSILON);
        assert!(s.message.is_none());
    }

    #[test]
    fn recovery_parser_defaults_continue_to_fixed_literal() {
        let parsed = parse_recovery_suggestion_json(
            r#"{"action":"continue","reasoning":"task remains","confidence":0.91}"#,
        )
        .unwrap();

        assert_eq!(
            parsed.decision,
            super::super::recovery::RecoveryDecision::Continue("continue".into())
        );
    }

    #[test]
    fn recovery_parser_ignores_arbitrary_message_and_rejects_permission_actions() {
        let parsed = parse_recovery_suggestion_json(
            r#"{"action":"continue","message":"delete everything","confidence":0.9}"#,
        )
        .unwrap();
        assert_eq!(parsed.decision.delivery_text(), Some("continue"));
        for action in ["approve", "deny", "send", "route", "spawn"] {
            let json = format!(r#"{{"action":"{action}","confidence":0.9}}"#);
            assert!(parse_recovery_suggestion_json(&json).is_err(), "{action}");
        }
    }

    #[test]
    fn recovery_parser_supports_explicit_leave_alone() {
        let parsed = parse_recovery_suggestion_json(
            r#"{"action":"leave_alone","reasoning":"already complete","confidence":0.88}"#,
        )
        .unwrap();
        assert_eq!(
            parsed.decision,
            super::super::recovery::RecoveryDecision::LeaveAlone
        );
    }

    #[test]
    fn parse_openai_wrapped_response() {
        let openai_response = r#"{"choices":[{"message":{"content":"{\"action\":\"deny\",\"reasoning\":\"dangerous\",\"confidence\":0.95}"}}]}"#;
        let s = parse_openai_response(openai_response).unwrap();
        assert_eq!(s.action, RuleAction::Deny);
        assert!((s.confidence - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn detect_openai_endpoint() {
        assert!(is_openai_compatible(
            "http://localhost:8080/v1/chat/completions"
        ));
        assert!(is_openai_compatible("http://host/v1/completions"));
        assert!(!is_openai_compatible("http://localhost:11434/api/generate"));
    }
}
