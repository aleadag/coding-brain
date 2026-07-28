#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use coding_brain_core::brain_activity::bounded_redacted_activity_text;

use crate::config::BrainConfig;
use crate::rules::RuleAction;

const MAX_STRUCTURED_COMPLETION_ATTEMPTS: usize = 2;
const MIN_STRUCTURED_COMPLETION_RETRY_MS: u64 = 1_000;
const MAX_COMPLETION_REASON_CHARS: usize = 64;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderFormat {
    Ollama,
    OpenAiCompatible,
}

impl ProviderFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Ollama => "Ollama",
            Self::OpenAiCompatible => "OpenAI-compatible",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneratedJsonKind {
    Incomplete,
    Malformed,
}

impl GeneratedJsonKind {
    fn label(self) -> &'static str {
        match self {
            Self::Incomplete => "incomplete",
            Self::Malformed => "malformed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompletionMetadata {
    done: Option<bool>,
    done_reason: Option<String>,
    eval_count: Option<u64>,
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeneratedJsonFailure {
    provider: ProviderFormat,
    kind: GeneratedJsonKind,
    metadata: CompletionMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InferenceFailure {
    NonRetryable(String),
    GeneratedJson(GeneratedJsonFailure),
}

fn bounded_completion_reason(value: &str) -> String {
    let redacted = bounded_redacted_activity_text(value);
    let mut characters = redacted.chars();
    let mut bounded = characters
        .by_ref()
        .take(MAX_COMPLETION_REASON_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        bounded.push_str("...");
    }
    bounded
}

impl GeneratedJsonFailure {
    fn summary(&self) -> String {
        let mut fields = Vec::new();
        if let Some(done) = self.metadata.done {
            fields.push(format!("done={done}"));
        }
        if let Some(reason) = &self.metadata.done_reason {
            fields.push(format!("done_reason={reason}"));
        }
        if let Some(eval_count) = self.metadata.eval_count {
            fields.push(format!("eval_count={eval_count}"));
        }
        if let Some(reason) = &self.metadata.finish_reason {
            fields.push(format!("finish_reason={reason}"));
        }
        if fields.is_empty() {
            format!("{} generated output", self.provider.label())
        } else {
            format!(
                "{} completion ({})",
                self.provider.label(),
                fields.join(", ")
            )
        }
    }

    fn diagnostic(&self, attempts: usize, retry_skipped: bool) -> String {
        let plural = if attempts == 1 { "" } else { "s" };
        let retry = if retry_skipped {
            "; retry skipped because less than 1 second remained"
        } else {
            ""
        };
        format!(
            "{} returned an {} structured decision after {attempts} attempt{plural} ({}){retry}; no action was taken",
            self.provider.label(),
            self.kind.label(),
            self.summary(),
        )
    }
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
    infer_with_command_factory(config, prompt, || Command::new(program))
}

fn infer_with_command_factory<F>(
    config: &BrainConfig,
    prompt: &str,
    mut command: F,
) -> Result<BrainSuggestion, String>
where
    F: FnMut() -> Command,
{
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

    let body = serde_json::to_string(&payload).map_err(|error| format!("json error: {error}"))?;
    let started = std::time::Instant::now();
    let mut first_generated_failure: Option<GeneratedJsonFailure> = None;

    for attempt in 1..=MAX_STRUCTURED_COMPLETION_ATTEMPTS {
        let mut attempt_config = config.clone();
        if attempt > 1 {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let remaining_ms = config.timeout_ms.saturating_sub(elapsed_ms);
            if remaining_ms < MIN_STRUCTURED_COMPLETION_RETRY_MS {
                return Err(first_generated_failure
                    .expect("retry requires a generated JSON failure")
                    .diagnostic(attempt - 1, true));
            }
            attempt_config.timeout_ms = remaining_ms;
        }

        match infer_once_with_command(&attempt_config, &body, is_openai, command()) {
            Ok(suggestion) => return Ok(suggestion),
            Err(InferenceFailure::GeneratedJson(failure))
                if attempt < MAX_STRUCTURED_COMPLETION_ATTEMPTS =>
            {
                first_generated_failure.get_or_insert(failure);
            }
            Err(InferenceFailure::GeneratedJson(failure)) => {
                let diagnostic = failure.diagnostic(attempt, false);
                return Err(match &first_generated_failure {
                    Some(first) => format!(
                        "{diagnostic}; first attempt was {} ({})",
                        first.kind.label(),
                        first.summary(),
                    ),
                    None => diagnostic,
                });
            }
            Err(InferenceFailure::NonRetryable(message)) => {
                return Err(match &first_generated_failure {
                    Some(first) => format!(
                        "{message}; first attempt was {} ({})",
                        first.kind.label(),
                        first.summary(),
                    ),
                    None => message,
                });
            }
        }
    }

    unreachable!("structured completion attempt loop always returns")
}

fn infer_once_with_command(
    config: &BrainConfig,
    body: &str,
    is_openai: bool,
    command: Command,
) -> Result<BrainSuggestion, InferenceFailure> {
    let stdout =
        curl_post_command(command, config, body).map_err(InferenceFailure::NonRetryable)?;
    let stdout = String::from_utf8_lossy(&stdout);
    if is_openai {
        parse_openai_response_classified(&stdout)
    } else {
        parse_ollama_response_classified(&stdout)
    }
}

fn curl_post_command(
    mut command: Command,
    config: &BrainConfig,
    body: &str,
) -> Result<Vec<u8>, String> {
    let timeout_secs = ((config.timeout_ms / 1000).max(1)).to_string();
    let mut child = command
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

fn api_error(provider: &str, message: &str) -> String {
    format!("{provider} API error: {message}")
}

fn extract_ollama_content(json: &serde_json::Value) -> Result<&str, String> {
    if let Some(error) = json.get("error") {
        let message = error
            .as_str()
            .ok_or_else(|| "invalid Ollama response: 'error' must be a string".to_string())?;
        return Err(api_error("Ollama", message));
    }

    json.get("response")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "invalid Ollama response: missing string 'response' field".to_string())
}

fn extract_openai_content(json: &serde_json::Value) -> Result<&str, String> {
    if let Some(error) = json.get("error") {
        let message = error
            .get("message")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                "invalid OpenAI response: missing string 'error.message' field".to_string()
            })?;
        return Err(api_error("OpenAI", message));
    }

    json.get("choices")
        .and_then(serde_json::Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            "invalid OpenAI response: missing string 'choices[0].message.content' field".to_string()
        })
}

/// Make an LLM API call, auto-detecting ollama vs OpenAI format from the endpoint URL.
fn call_llm(config: &BrainConfig, prompt: &str) -> Result<String, String> {
    call_llm_with_program(config, prompt, Path::new("curl"))
}

fn call_llm_with_program(
    config: &BrainConfig,
    prompt: &str,
    program: &Path,
) -> Result<String, String> {
    call_llm_with_command(config, prompt, Command::new(program))
}

fn call_llm_with_command(
    config: &BrainConfig,
    prompt: &str,
    command: Command,
) -> Result<String, String> {
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
    let stdout = curl_post_command(command, config, &body)?;
    let stdout = String::from_utf8_lossy(&stdout);
    let json: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("invalid response: {e}"))?;

    let content = if is_openai {
        extract_openai_content(&json)?
    } else {
        extract_ollama_content(&json)?
    };
    Ok(content.to_string())
}

/// Parse the ollama `/api/generate` response format.
fn parse_ollama_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    let generated = extract_ollama_content(&json)?;
    parse_suggestion_json(generated)
}

/// Parse OpenAI-compatible /v1/chat/completions response.
fn parse_openai_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;

    let content = extract_openai_content(&json)?;
    parse_suggestion_json(content)
}

fn ollama_completion_metadata(json: &serde_json::Value) -> CompletionMetadata {
    CompletionMetadata {
        done: json.get("done").and_then(serde_json::Value::as_bool),
        done_reason: json
            .get("done_reason")
            .and_then(serde_json::Value::as_str)
            .map(bounded_completion_reason),
        eval_count: json.get("eval_count").and_then(serde_json::Value::as_u64),
        finish_reason: None,
    }
}

fn openai_completion_metadata(json: &serde_json::Value) -> CompletionMetadata {
    CompletionMetadata {
        finish_reason: json
            .get("choices")
            .and_then(serde_json::Value::as_array)
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("finish_reason"))
            .and_then(serde_json::Value::as_str)
            .map(bounded_completion_reason),
        ..CompletionMetadata::default()
    }
}

fn parse_ollama_response_classified(response: &str) -> Result<BrainSuggestion, InferenceFailure> {
    let json = serde_json::from_str(response).map_err(|error| {
        InferenceFailure::NonRetryable(format!("invalid JSON response: {error}"))
    })?;
    let generated = extract_ollama_content(&json).map_err(InferenceFailure::NonRetryable)?;
    parse_generated_suggestion(
        generated,
        ProviderFormat::Ollama,
        ollama_completion_metadata(&json),
    )
}

fn parse_openai_response_classified(response: &str) -> Result<BrainSuggestion, InferenceFailure> {
    let json = serde_json::from_str(response).map_err(|error| {
        InferenceFailure::NonRetryable(format!("invalid JSON response: {error}"))
    })?;
    let content = extract_openai_content(&json).map_err(InferenceFailure::NonRetryable)?;
    parse_generated_suggestion(
        content,
        ProviderFormat::OpenAiCompatible,
        openai_completion_metadata(&json),
    )
}

fn parse_generated_suggestion(
    text: &str,
    provider: ProviderFormat,
    metadata: CompletionMetadata,
) -> Result<BrainSuggestion, InferenceFailure> {
    let json = serde_json::from_str(text.trim()).map_err(|error| {
        let kind = if error.classify() == serde_json::error::Category::Eof {
            GeneratedJsonKind::Incomplete
        } else {
            GeneratedJsonKind::Malformed
        };
        InferenceFailure::GeneratedJson(GeneratedJsonFailure {
            provider,
            kind,
            metadata,
        })
    })?;
    suggestion_from_value(json).map_err(InferenceFailure::NonRetryable)
}

/// Parse the structured JSON that the brain LLM is expected to produce.
pub fn parse_suggestion_json(text: &str) -> Result<BrainSuggestion, String> {
    // The LLM should produce JSON like:
    // {"action": "approve", "message": null, "reasoning": "safe command", "confidence": 0.95}
    let json: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|e| format!("invalid suggestion JSON: {e}"))?;

    suggestion_from_value(json)
}

fn suggestion_from_value(json: serde_json::Value) -> Result<BrainSuggestion, String> {
    let action_str = json
        .get("action")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing 'action' field")?;

    let action =
        RuleAction::parse(action_str).ok_or_else(|| format!("unknown action '{action_str}'"))?;

    let message = json
        .get("message")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let reasoning = json
        .get("reasoning")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();

    let confidence = json
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
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
        std::fs::write(&path, format!("set -eu\n{script}\n")).unwrap();
        (temp, path)
    }

    #[cfg(unix)]
    fn fake_curl_command(script: &Path) -> Command {
        let mut command = Command::new("sh");
        command.arg(script);
        command
    }

    #[cfg(unix)]
    fn fixture_file(script: &Path, suffix: &str) -> std::path::PathBuf {
        let mut path = script.as_os_str().to_os_string();
        path.push(suffix);
        std::path::PathBuf::from(path)
    }

    #[cfg(unix)]
    fn fake_curl_responses(responses: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let (temp, script) = fake_curl(
            r#"count=0
if [ -r "${0}.count" ]; then
    IFS= read -r count < "${0}.count" || true
fi
count=$((count + 1))
printf '%s' "$count" > "${0}.count"
dd of=/dev/null 2>/dev/null
dd if="${0}.response${count}" 2>/dev/null"#,
        );
        for (index, response) in responses.iter().enumerate() {
            std::fs::write(
                fixture_file(&script, &format!(".response{}", index + 1)),
                response,
            )
            .unwrap();
        }
        (temp, script)
    }

    #[cfg(unix)]
    fn invocation_count(script: &Path) -> u64 {
        std::fs::read_to_string(fixture_file(script, ".count"))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn inference_sends_prompt_only_over_stdin_and_disables_redirects() {
        let (temp, script) = fake_curl(
            r#"printf '%s\n' "$@" > "${0}.args"
dd of="${0}.stdin" 2>/dev/null
printf '%s' '{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}"}'"#,
        );
        let config = BrainConfig {
            endpoint: "http://brain.example.test/api/generate".into(),
            ..BrainConfig::default()
        };
        let secret_prompt = "unique prompt fragment";

        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
            0
        );
        let suggestion =
            infer_with_command_factory(&config, secret_prompt, || fake_curl_command(&script))
                .unwrap();

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
        let (_temp, script) = fake_curl("dd if=/dev/zero bs=1048577 count=1 2>/dev/null");
        let error = infer_with_command_factory(&BrainConfig::default(), "prompt", || {
            fake_curl_command(&script)
        })
        .unwrap_err();
        assert!(error.contains("exceeds 1 MiB"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_decision_retries_once() {
        let (_temp, script) = fake_curl_responses(&[
            r#"{"response":"{\"action\":\"approve\",\"reasoning\":\"partial","done":true,"done_reason":"stop","eval_count":746}"#,
            r#"{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}","done":true,"done_reason":"stop","eval_count":12}"#,
        ]);
        let suggestion =
            infer_with_command_factory(&BrainConfig::default(), "secret prompt", || {
                fake_curl_command(&script)
            })
            .unwrap();
        assert_eq!(suggestion.action, RuleAction::Approve);
        assert_eq!(invocation_count(&script), 2);
    }

    #[cfg(unix)]
    #[test]
    fn repeated_incomplete_decisions_fail_once_with_safe_metadata() {
        let (_temp, script) = fake_curl_responses(&[
            r#"{"model":"private-model","response":"{\"action\":\"approve\",\"reasoning\":\"first-secret","done":true,"done_reason":"stop","eval_count":746,"total_duration":123456}"#,
            r#"{"model":"private-model","response":"{\"action\":\"approve\",\"reasoning\":\"second-secret","done":true,"done_reason":"length","eval_count":512,"total_duration":654321}"#,
        ]);
        let error = infer_with_command_factory(&BrainConfig::default(), "secret prompt", || {
            fake_curl_command(&script)
        })
        .unwrap_err();
        assert_eq!(invocation_count(&script), 2);
        assert!(error.contains("incomplete structured decision"));
        assert!(error.contains("done=true"));
        assert!(error.contains("done_reason=length"));
        assert!(error.contains("eval_count=512"));
        for secret in [
            "first-secret",
            "second-secret",
            "secret prompt",
            "private-model",
            "654321",
        ] {
            assert!(!error.contains(secret), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn schema_and_api_errors_are_not_retried() {
        let (_schema_temp, schema_script) =
            fake_curl_responses(&[r#"{"response":"{\"reasoning\":\"no decision\"}","done":true}"#]);
        let schema_error = infer_with_command_factory(&BrainConfig::default(), "prompt", || {
            fake_curl_command(&schema_script)
        })
        .unwrap_err();
        assert_eq!(schema_error, "missing 'action' field");
        assert_eq!(invocation_count(&schema_script), 1);

        let (_api_temp, api_script) = fake_curl_responses(&[r#"{"error":"unable to load model"}"#]);
        let api_error = infer_with_command_factory(&BrainConfig::default(), "prompt", || {
            fake_curl_command(&api_script)
        })
        .unwrap_err();
        assert_eq!(api_error, "Ollama API error: unable to load model");
        assert_eq!(invocation_count(&api_script), 1);
    }

    #[cfg(unix)]
    #[test]
    fn retry_uses_remaining_budget_and_can_be_skipped() {
        let config = BrainConfig {
            timeout_ms: 999,
            ..BrainConfig::default()
        };
        let (_temp, script) = fake_curl_responses(&[
            r#"{"response":"{\"action\":\"approve","done":true,"done_reason":"stop","eval_count":8}"#,
        ]);
        let error = infer_with_command_factory(&config, "prompt", || fake_curl_command(&script))
            .unwrap_err();
        assert_eq!(invocation_count(&script), 1);
        assert!(error.contains("incomplete structured decision"));
        assert!(error.contains("retry skipped because less than 1 second remained"));
    }

    #[cfg(unix)]
    #[test]
    fn retry_preserves_second_provider_error() {
        let (_temp, script) = fake_curl_responses(&[
            r#"{"response":"{\"action\":\"approve\"]","done":true,"done_reason":"stop","eval_count":4}"#,
            r#"{"error":"retry unavailable"}"#,
        ]);
        let error = infer_with_command_factory(&BrainConfig::default(), "prompt", || {
            fake_curl_command(&script)
        })
        .unwrap_err();
        assert_eq!(invocation_count(&script), 2);
        assert!(error.contains("Ollama API error: retry unavailable"));
        assert!(error.contains("first attempt was malformed"));
        assert!(error.contains("done_reason=stop"));
    }

    #[cfg(unix)]
    #[test]
    fn openai_syntax_failure_retries_once() {
        let (_temp, script) = fake_curl_responses(&[
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"{\"action\":\"deny"}}]}"#,
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"{\"action\":\"deny\",\"reasoning\":\"unsafe\",\"confidence\":0.95}"}}]}"#,
        ]);
        let config = BrainConfig {
            endpoint: "http://brain.example.test/v1/chat/completions".into(),
            ..BrainConfig::default()
        };
        let suggestion =
            infer_with_command_factory(&config, "prompt", || fake_curl_command(&script)).unwrap();
        assert_eq!(suggestion.action, RuleAction::Deny);
        assert_eq!(invocation_count(&script), 2);
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
    fn parse_ollama_api_error_is_not_a_suggestion_error() {
        let error = parse_ollama_response(r#"{"error":"unable to load model"}"#).unwrap_err();
        assert_eq!(error, "Ollama API error: unable to load model");
    }

    #[test]
    fn parse_openai_api_error_is_not_a_suggestion_error() {
        let error = parse_openai_response(
            r#"{"error":{"message":"model unavailable","type":"server_error"}}"#,
        )
        .unwrap_err();
        assert_eq!(error, "OpenAI API error: model unavailable");
    }

    #[test]
    fn provider_errors_take_precedence_over_generated_content() {
        let ollama = r#"{
            "error":"generation failed",
            "response":"{\"action\":\"approve\"}"
        }"#;
        assert_eq!(
            parse_ollama_response(ollama).unwrap_err(),
            "Ollama API error: generation failed"
        );

        let openai = r#"{
            "error":{"message":"generation failed"},
            "choices":[{"message":{"content":"{\"action\":\"approve\"}"}}]
        }"#;
        assert_eq!(
            parse_openai_response(openai).unwrap_err(),
            "OpenAI API error: generation failed"
        );
    }

    #[test]
    fn malformed_provider_envelopes_do_not_reach_suggestion_parser() {
        let ollama =
            parse_ollama_response(r#"{"error":{"message":"wrong native shape"}}"#).unwrap_err();
        assert_eq!(ollama, "invalid Ollama response: 'error' must be a string");

        let openai = parse_openai_response(r#"{"error":"wrong OpenAI shape"}"#).unwrap_err();
        assert_eq!(
            openai,
            "invalid OpenAI response: missing string 'error.message' field"
        );
    }

    #[test]
    fn malformed_generated_decisions_remain_suggestion_errors() {
        let ollama =
            parse_ollama_response(r#"{"response":"{\"reasoning\":\"no decision\"}"}"#).unwrap_err();
        assert_eq!(ollama, "missing 'action' field");

        let openai = parse_openai_response(
            r#"{"choices":[{"message":{"content":"{\"reasoning\":\"no decision\"}"}}]}"#,
        )
        .unwrap_err();
        assert_eq!(openai, "missing 'action' field");
    }

    #[test]
    fn incomplete_ollama_decision_keeps_only_safe_completion_metadata() {
        let response = r#"{"model":"gemma4:e4b","response":"{\"action\":\"approve\",\"reasoning\":\"sk-secret-output","done":true,"done_reason":"stop","eval_count":746,"total_duration":123456}"#;

        let error = parse_ollama_response_classified(response).unwrap_err();
        let InferenceFailure::GeneratedJson(failure) = error else {
            panic!("expected generated JSON failure");
        };
        assert_eq!(failure.provider, ProviderFormat::Ollama);
        assert_eq!(failure.kind, GeneratedJsonKind::Incomplete);
        assert_eq!(failure.metadata.done, Some(true));
        assert_eq!(failure.metadata.done_reason.as_deref(), Some("stop"));
        assert_eq!(failure.metadata.eval_count, Some(746));

        let diagnostic = failure.diagnostic(1, false);
        assert!(diagnostic.contains("incomplete structured decision"));
        assert!(diagnostic.contains("done=true"));
        assert!(diagnostic.contains("done_reason=stop"));
        assert!(diagnostic.contains("eval_count=746"));
        assert!(!diagnostic.contains("sk-secret-output"));
        assert!(!diagnostic.contains("gemma4"));
        assert!(!diagnostic.contains("123456"));
    }

    #[test]
    fn generated_syntax_and_schema_failures_remain_distinct() {
        let malformed = parse_ollama_response_classified(
            r#"{"response":"{\"action\":\"approve\"]","done":true}"#,
        )
        .unwrap_err();
        assert!(matches!(
            malformed,
            InferenceFailure::GeneratedJson(GeneratedJsonFailure {
                kind: GeneratedJsonKind::Malformed,
                ..
            })
        ));

        let schema = parse_ollama_response_classified(
            r#"{"response":"{\"reasoning\":\"no decision\"}","done":true}"#,
        )
        .unwrap_err();
        assert!(matches!(
            schema,
            InferenceFailure::NonRetryable(ref message)
                if message == "missing 'action' field"
        ));
    }

    #[test]
    fn openai_generated_failure_bounds_finish_reason() {
        let finish_reason = format!("token=private {}", "x".repeat(200));
        let response = serde_json::json!({
            "choices": [{
                "finish_reason": finish_reason,
                "message": {"content": "{\"action\":"}
            }]
        })
        .to_string();

        let error = parse_openai_response_classified(&response).unwrap_err();
        let InferenceFailure::GeneratedJson(failure) = error else {
            panic!("expected generated JSON failure");
        };
        let diagnostic = failure.diagnostic(1, false);
        assert!(diagnostic.contains("OpenAI-compatible"));
        assert!(diagnostic.contains("[REDACTED]"));
        assert!(!diagnostic.contains("private"));
        assert!(diagnostic.len() < 512);
    }

    #[cfg(unix)]
    #[test]
    fn completion_extracts_generated_recovery_content_for_both_formats() {
        let recovery = r#"{"action":"continue","confidence":0.9}"#;
        let (_ollama_temp, ollama_script) = fake_curl(
            r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"response":"{\"action\":\"continue\",\"confidence\":0.9}"}'"#,
        );
        let ollama = call_llm_with_command(
            &BrainConfig::default(),
            "prompt",
            fake_curl_command(&ollama_script),
        )
        .unwrap();
        assert_eq!(ollama, recovery);
        assert!(parse_recovery_suggestion_json(&ollama).is_ok());

        let (_openai_temp, openai_script) = fake_curl(
            r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"choices":[{"message":{"content":"{\"action\":\"continue\",\"confidence\":0.9}"}}]}'"#,
        );
        let openai_config = BrainConfig {
            endpoint: "http://brain.example.test/v1/chat/completions".into(),
            ..BrainConfig::default()
        };
        let openai =
            call_llm_with_command(&openai_config, "prompt", fake_curl_command(&openai_script))
                .unwrap();
        assert_eq!(openai, recovery);
        assert!(parse_recovery_suggestion_json(&openai).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn completion_surfaces_schema_specific_api_errors() {
        let (_ollama_temp, ollama_script) = fake_curl(
            r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":"unable to load model"}'"#,
        );
        assert_eq!(
            call_llm_with_command(
                &BrainConfig::default(),
                "prompt",
                fake_curl_command(&ollama_script),
            )
            .unwrap_err(),
            "Ollama API error: unable to load model"
        );

        let (_openai_temp, openai_script) = fake_curl(
            r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":{"message":"model unavailable","type":"server_error"}}'"#,
        );
        let openai_config = BrainConfig {
            endpoint: "http://brain.example.test/v1/chat/completions".into(),
            ..BrainConfig::default()
        };
        assert_eq!(
            call_llm_with_command(&openai_config, "prompt", fake_curl_command(&openai_script))
                .unwrap_err(),
            "OpenAI API error: model unavailable"
        );
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
