# Incomplete Structured Brain Completion Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. The tasks already exist under implementation epic `codexctl-z0qg`. Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Recover once from syntactically invalid permission-decision output, then fail safe with actionable metadata if no complete decision is produced.

**Architecture:** Keep endpoint-specific provider envelopes and API-error precedence unchanged. Parse generated decision JSON through a private classified boundary, retain only safe completion metadata, and run at most two independent attempts through a fresh-command factory within the existing total timeout. The permission hook receives only the final suggestion or error, so activity persistence and hook delivery remain single-shot.

**Tech Stack:** Rust 2024, `serde_json`, `std::process::Command`, Cargo tests, Nix flakes.

## Global Constraints

- `MAX_STRUCTURED_COMPLETION_ATTEMPTS` is private and fixed at `2`; do not add CLI or TOML configuration.
- Retry only generated JSON syntax failures. Do not retry transport failures, malformed provider envelopes, API errors, or valid-but-schema-invalid decisions.
- EOF is reported as an incomplete structured decision; other JSON syntax errors are reported as malformed.
- Parse attempts independently. Never concatenate, repair, or extract an action from partial output.
- Ollama and OpenAI-compatible decision inference share the retry policy but keep separate envelope parsing and metadata.
- Both attempts share the existing `timeout_ms`; start attempt two only when at least `1_000` milliseconds remain.
- Persist and deliver only the final result. A recovered first failure creates no activity row or hook response.
- Allow diagnostic metadata only from Ollama `done`, `done_reason`, and `eval_count`, or OpenAI-compatible `finish_reason`. Redact and character-bound reason strings before any error reaches stderr.
- Do not include prompts, generated output, model names, timestamps, contexts, or duration fields in errors.
- Leave `infer_recovery` unchanged.
- Reconcile the client test seam with `codexctl-6r0o`: generated scripts remain non-executable and each attempt invokes a fresh `sh <script>` command.
- Do not commit, push, or publish without explicit user authorization.

**Beads:** implementation epic `codexctl-z0qg`; tasks `codexctl-t0lu`,
`codexctl-mz29`, and `codexctl-9vr1`.

---

### Task 1: Classify Generated Decision Failures and Safe Metadata

**Files:**
- Modify: `src/brain/client.rs:7-20`
- Modify: `src/brain/client.rs:164-299`
- Test: `src/brain/client.rs:393-505`

**Interfaces:**
- Consumes: `extract_ollama_content(&serde_json::Value)`, `extract_openai_content(&serde_json::Value)`, `RuleAction::parse`, and `coding_brain_core::brain_activity::bounded_redacted_activity_text`.
- Produces: private `ProviderFormat`, `GeneratedJsonKind`, `CompletionMetadata`, `GeneratedJsonFailure`, `InferenceFailure`, `parse_generated_suggestion`, `parse_ollama_response_classified`, and `parse_openai_response_classified`.

**Acceptance Criteria:**
- A valid Ollama envelope whose `response` ends mid-object returns `InferenceFailure::GeneratedJson` with `GeneratedJsonKind::Incomplete`.
- Other generated JSON syntax errors use `GeneratedJsonKind::Malformed`.
- Ollama `done`, redacted and bounded `done_reason`, and `eval_count` survive only as diagnostic metadata.
- OpenAI-compatible `finish_reason` is retained through the same bounded redaction.
- Valid JSON without `action` remains the exact non-retryable `missing 'action' field` error.
- Provider API errors still take precedence over generated content.
- The legacy string-returning parse functions remain available to existing tests and callers.

- [ ] **Step 1: Add failing classification and disclosure tests**

Add these tests beside the existing provider-envelope tests:

```rust
#[test]
fn incomplete_ollama_decision_keeps_only_safe_completion_metadata() {
    let response = r#"{
        "model":"gemma4:e4b",
        "response":"{\"action\":\"approve\",\"reasoning\":\"sk-secret-output",
        "done":true,
        "done_reason":"stop",
        "eval_count":746,
        "total_duration":123456
    }"#;

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
```

- [ ] **Step 2: Run the new tests and verify the red state**

Run:

```bash
direnv exec . cargo test brain::client::tests::incomplete_ollama_decision_keeps_only_safe_completion_metadata -- --exact
direnv exec . cargo test brain::client::tests::generated_syntax_and_schema_failures_remain_distinct -- --exact
direnv exec . cargo test brain::client::tests::openai_generated_failure_bounds_finish_reason -- --exact
```

Expected: compilation fails because the classified types and parse functions do not exist. Existing provider-error tests still compile before the new test block is added.

- [ ] **Step 3: Add private classified failure and metadata types**

Import the existing redactor and add the private constants and types near `BrainSuggestion`:

```rust
use coding_brain_core::brain_activity::bounded_redacted_activity_text;

const MAX_STRUCTURED_COMPLETION_ATTEMPTS: usize = 2;
const MIN_STRUCTURED_COMPLETION_RETRY_MS: u64 = 1_000;
const MAX_COMPLETION_REASON_CHARS: usize = 64;

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
```

Add bounded provider-reason handling and fixed diagnostic formatting:

```rust
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
            format!("{} completion ({})", self.provider.label(), fields.join(", "))
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
```

- [ ] **Step 4: Split syntax parsing from schema validation**

Move the existing action, message, reasoning, confidence, and timestamp extraction into `suggestion_from_value`:

```rust
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
```

Implement generated parsing as two explicit stages so schema errors remain
`InferenceFailure::NonRetryable`:

```rust
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
```

Keep the public parser's current error text:

```rust
pub fn parse_suggestion_json(text: &str) -> Result<BrainSuggestion, String> {
    let json = serde_json::from_str(text.trim())
        .map_err(|error| format!("invalid suggestion JSON: {error}"))?;
    suggestion_from_value(json)
}
```

- [ ] **Step 5: Add endpoint-specific metadata and classified wrappers**

Add:

```rust
fn ollama_completion_metadata(json: &serde_json::Value) -> CompletionMetadata {
    CompletionMetadata {
        done: json.get("done").and_then(serde_json::Value::as_bool),
        done_reason: json
            .get("done_reason")
            .and_then(serde_json::Value::as_str)
            .map(bounded_completion_reason),
        eval_count: json
            .get("eval_count")
            .and_then(serde_json::Value::as_u64),
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

fn parse_ollama_response_classified(
    response: &str,
) -> Result<BrainSuggestion, InferenceFailure> {
    let json = serde_json::from_str(response)
        .map_err(|error| InferenceFailure::NonRetryable(
            format!("invalid JSON response: {error}")
        ))?;
    let generated = extract_ollama_content(&json)
        .map_err(InferenceFailure::NonRetryable)?;
    parse_generated_suggestion(
        generated,
        ProviderFormat::Ollama,
        ollama_completion_metadata(&json),
    )
}

fn parse_openai_response_classified(
    response: &str,
) -> Result<BrainSuggestion, InferenceFailure> {
    let json = serde_json::from_str(response)
        .map_err(|error| InferenceFailure::NonRetryable(
            format!("invalid JSON response: {error}")
        ))?;
    let content = extract_openai_content(&json)
        .map_err(InferenceFailure::NonRetryable)?;
    parse_generated_suggestion(
        content,
        ProviderFormat::OpenAiCompatible,
        openai_completion_metadata(&json),
    )
}
```

Leave `parse_ollama_response` and `parse_openai_response` on their current
string-returning path so existing parser behavior remains byte-for-byte
compatible:

```rust
fn parse_ollama_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;
    let generated = extract_ollama_content(&json)?;
    parse_suggestion_json(generated)
}

fn parse_openai_response(response: &str) -> Result<BrainSuggestion, String> {
    let json: serde_json::Value =
        serde_json::from_str(response).map_err(|e| format!("invalid JSON response: {e}"))?;
    let content = extract_openai_content(&json)?;
    parse_suggestion_json(content)
}
```

- [ ] **Step 6: Run focused client parsing tests**

Run:

```bash
direnv exec . cargo test brain::client::tests:: --lib
```

Expected: all client tests pass. The new tests distinguish incomplete, malformed, and schema-invalid generated content; the existing API-error precedence tests remain green.

- [ ] **Step 7: Inspect the Task 1 diff**

Run:

```bash
git diff --check
git diff -- src/brain/client.rs
```

Expected: only classified parsing, bounded metadata, wrapper preservation, and focused tests changed. There is no retry loop, configuration change, raw model output in errors, or recovery inference change.

---

### Task 2: Retry Syntax Failures Within One Timeout

**Files:**
- Modify: `src/brain/client.rs:22-145`
- Modify: `src/brain/client.rs:342-556`
- Test: `src/brain/client.rs`

**Interfaces:**
- Consumes: Task 1 `InferenceFailure`, `GeneratedJsonFailure`, and classified provider parsers; the stable-shell prepared-command seam from `codexctl-6r0o`.
- Produces: private `infer_with_command_factory<F>(&BrainConfig, &str, F) -> Result<BrainSuggestion, String>` where `F: FnMut() -> Command`, plus final failure formatting that preserves the second attempt's class.

**Acceptance Criteria:**
- A generated JSON syntax failure causes at most one retry.
- A valid second decision is returned without exposing or persisting the first failure.
- Two syntax failures return one actionable metadata-only error and make exactly two provider calls.
- Attempt two uses only the remaining `timeout_ms`; a budget below `1_000` milliseconds suppresses retry.
- A second transport, provider, envelope, or schema failure remains recognizable and includes bounded retry context.
- Provider and schema failures on attempt one are never retried.
- Ollama and OpenAI-compatible decision inference follow the same attempt policy.
- Fake scripts are non-executable and every attempt creates a fresh `sh <script>` command.

- [ ] **Step 1: Confirm the stable-shell prerequisite**

Run:

```bash
bd -C /home/alexander/.beads-planning show codexctl-6r0o
git log -1 --oneline
rg -n "infer_with_command|curl_post_command|fn fake_curl" src/brain/client.rs
```

Expected: `codexctl-6r0o` is closed and this worktree contains its stable-shell command seam. If the Bead is closed but the code is absent, stop and reconcile the landed revision before editing; do not copy executable fixture code from the old baseline.

- [ ] **Step 2: Make the stable-shell fixture reusable per attempt**

Change the post-`codexctl-6r0o` fixture to return the script path, and add a command constructor:

```rust
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
```

Update each existing one-attempt inference test from an owned command to a
one-call factory. For example, update
`inference_sends_prompt_only_over_stdin_and_disables_redirects` to:

```rust
let (temp, script) = fake_curl(
    r#"printf '%s\n' "$@" > "${0}.args"
dd of="${0}.stdin" 2>/dev/null
printf '%s' '{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}"}'"#,
);
let suggestion = infer_with_command_factory(&config, secret_prompt, || {
    fake_curl_command(&script)
})
.unwrap();
```

Retain the `mode() & 0o111 == 0` assertion introduced by `codexctl-6r0o`.

- [ ] **Step 3: Add failing retry sequencing tests**

Add helpers that append suffixes to the exact script path and serve one response
file per invocation:

```rust
#[cfg(unix)]
fn fixture_file(script: &Path, suffix: &str) -> std::path::PathBuf {
    let mut path = script.as_os_str().to_os_string();
    path.push(suffix);
    std::path::PathBuf::from(path)
}

#[cfg(unix)]
fn fake_curl_responses(
    responses: &[&str],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let (temp, script) = fake_curl(
        r#"count=0
if [ -r "${0}.count" ]; then
    IFS= read -r count < "${0}.count"
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
```

Add the complete retry cases:

```rust
#[cfg(unix)]
#[test]
fn incomplete_decision_retries_once() {
    let (_temp, script) = fake_curl_responses(&[
        r#"{"response":"{\"action\":\"approve\",\"reasoning\":\"partial","done":true,"done_reason":"stop","eval_count":746}"#,
        r#"{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}","done":true,"done_reason":"stop","eval_count":12}"#,
    ]);
    let suggestion = infer_with_command_factory(
        &BrainConfig::default(),
        "secret prompt",
        || fake_curl_command(&script),
    )
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
    let error = infer_with_command_factory(
        &BrainConfig::default(),
        "secret prompt",
        || fake_curl_command(&script),
    )
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
    let (_schema_temp, schema_script) = fake_curl_responses(&[
        r#"{"response":"{\"reasoning\":\"no decision\"}","done":true}"#,
    ]);
    let schema_error = infer_with_command_factory(
        &BrainConfig::default(),
        "prompt",
        || fake_curl_command(&schema_script),
    )
    .unwrap_err();
    assert_eq!(schema_error, "missing 'action' field");
    assert_eq!(invocation_count(&schema_script), 1);

    let (_api_temp, api_script) =
        fake_curl_responses(&[r#"{"error":"unable to load model"}"#]);
    let api_error = infer_with_command_factory(
        &BrainConfig::default(),
        "prompt",
        || fake_curl_command(&api_script),
    )
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
    let error = infer_with_command_factory(&config, "prompt", || {
        fake_curl_command(&script)
    })
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
    let error = infer_with_command_factory(
        &BrainConfig::default(),
        "prompt",
        || fake_curl_command(&script),
    )
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
```

- [ ] **Step 4: Run the sequencing tests and verify the red state**

Run:

```bash
direnv exec . cargo test brain::client::tests::incomplete_decision_retries_once -- --exact
direnv exec . cargo test brain::client::tests::repeated_incomplete_decisions_fail_once_with_safe_metadata -- --exact
direnv exec . cargo test brain::client::tests::retry_uses_remaining_budget_and_can_be_skipped -- --exact
```

Expected: compilation fails because `infer_with_command_factory` does not exist, or the first generated syntax failure returns immediately. No failure may come from executing the temporary script directly.

- [ ] **Step 5: Add a fresh-command inference attempt**

Factor one endpoint-selected provider attempt:

```rust
fn infer_once_with_command(
    config: &BrainConfig,
    body: &str,
    is_openai: bool,
    command: Command,
) -> Result<BrainSuggestion, InferenceFailure> {
    let stdout = curl_post_command(command, config, body)
        .map_err(InferenceFailure::NonRetryable)?;
    let stdout = String::from_utf8_lossy(&stdout);
    if is_openai {
        parse_openai_response_classified(&stdout)
    } else {
        parse_ollama_response_classified(&stdout)
    }
}
```

- [ ] **Step 6: Add bounded retry orchestration**

Make the path wrapper create a fresh production command on every attempt:

```rust
fn infer_with_program(
    config: &BrainConfig,
    prompt: &str,
    program: &Path,
) -> Result<BrainSuggestion, String> {
    infer_with_command_factory(config, prompt, || Command::new(program))
}
```

Replace the post-`codexctl-6r0o` owned-command inference helper with:

```rust
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
        serde_json::json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {"type": "json_object"},
            "stream": false,
        })
    } else {
        serde_json::json!({
            "model": config.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
        })
    };
    let body = serde_json::to_string(&payload)
        .map_err(|error| format!("json error: {error}"))?;
    let started = std::time::Instant::now();
    let mut first_generated_failure = None;

    for attempt in 1..=MAX_STRUCTURED_COMPLETION_ATTEMPTS {
        let mut attempt_config = config.clone();
        if attempt > 1 {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis())
                .unwrap_or(u64::MAX);
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
                        "structured decision retry failed after {}: {message}",
                        first.summary()
                    ),
                    None => message,
                });
            }
        }
    }
    unreachable!("structured completion attempt loop always returns")
}
```

Do not change `call_llm_with_command` or `infer_recovery`; their prepared `Command` remains one-shot.

- [ ] **Step 7: Run all client tests**

Run:

```bash
direnv exec . cargo test brain::client::tests:: --lib
```

Expected: all client tests pass. Verify the new call-count assertions show `2` only for generated syntax failures and `1` for schema/API failures or a skipped retry.

- [ ] **Step 8: Inspect the Task 2 diff**

Run:

```bash
git diff --check
git diff -- src/brain/client.rs
```

Expected: the diff contains a fresh-command factory for decision inference only, a two-attempt loop with remaining timeout, stable-shell test updates, and focused tests. There is no executable fixture, sleep, global state, configuration field, recovery retry, or raw generated output in errors.

---

### Task 3: Prove the Permission Fail-Safe Path and Validate the Workspace

**Files:**
- Modify: `src/brain/permission_hook.rs:1020-1185`
- Test: `src/brain/permission_hook.rs`
- Verify: workspace and Nix flake

**Interfaces:**
- Consumes: the final string error returned by Task 2 through `query::evaluate_with` and the existing injected `infer` seam.
- Produces: regression proof that one actionable inference failure creates one `Observed -> Evaluating -> Error` lifecycle, leaves the provider response empty, and records `NeedsInput`.

**Acceptance Criteria:**
- Retry exhaustion emits no executable permission response.
- Exactly one decision lifecycle is persisted with states `Observed`, `Evaluating`, and `Error`.
- The terminal event contains the actionable bounded reason and no raw model output.
- Lifecycle status is `NeedsInput`.
- Existing low-confidence, provider-error, successful Ollama/OpenAI, and API-error precedence tests pass.
- Formatting, workspace tests, all-target Clippy, build, and the stable-shell Nix derivation pass.

- [ ] **Step 1: Add a permission-path characterization regression**

Add this test beside `fallthrough_cases_leave_stdout_empty`:

```rust
#[test]
fn structured_completion_failure_persists_one_actionable_error_lifecycle() {
    let _guard = crate::config::HOME_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let _restore_home = set_test_home(home.path());
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let reason = "Ollama returned an incomplete structured decision after 2 attempts \
        (Ollama completion (done=true, done_reason=stop, eval_count=746)); \
        no action was taken";

    run_with_gate_and_stores(
        Cursor::new(payload()),
        &mut stdout,
        &mut stderr,
        Some(&enabled_config()),
        BrainGateMode::Auto,
        &lifecycle,
        Some(&activity),
        |_, _| Err(reason.into()),
    );

    assert!(stdout.is_empty());
    assert!(String::from_utf8(stderr).unwrap().contains(reason));
    assert_eq!(
        lifecycle.read().unwrap().snapshot.unwrap().sessions.values()
            .next().unwrap().projected_status,
        Some(ProjectedStatus::NeedsInput),
    );
    let events = activity.read().unwrap().events().to_vec();
    assert_eq!(
        events.iter().map(|event| event.state).collect::<Vec<_>>(),
        [
            ActivityState::Observed,
            ActivityState::Evaluating,
            ActivityState::Error,
        ]
    );
    let terminal = events.last().unwrap();
    assert_eq!(terminal.reasoning.as_deref(), Some(reason));
    assert_eq!(
        events.iter()
            .map(|event| event.activity_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        1,
    );
}
```

- [ ] **Step 2: Run the permission regression**

Run:

```bash
direnv exec . cargo test brain::permission_hook::tests::structured_completion_failure_persists_one_actionable_error_lifecycle -- --exact
```

Expected: PASS. The test characterizes the existing fail-safe projection while
Task 1 supplies the required initial red test. If it does not compile because
the store access shape differs, inspect the adjacent lifecycle tests and adjust
only the test accessors; do not change production permission behavior. It must
never emit stdout.

- [ ] **Step 3: Run focused regression suites**

Run:

```bash
direnv exec . cargo test brain::client::tests:: --lib
direnv exec . cargo test brain::permission_hook::tests:: --lib
direnv exec . cargo test --test hook_activity inference_failure_and_low_confidence_are_visible_abstentions -- --exact
```

Expected: all focused tests pass. The process integration test still reports endpoint failures as one `Error` activity and low-confidence suggestions as `Abstained`.

- [ ] **Step 4: Format and run workspace gates**

Run:

```bash
direnv exec . cargo fmt
direnv exec . cargo fmt --check
direnv exec . cargo test
direnv exec . cargo clippy --all-targets -- -D warnings
direnv exec . cargo build
```

Expected: every command exits `0`; Clippy emits no warnings.

- [ ] **Step 5: Run the stable-shell Nix gate**

Run:

```bash
nix build .# --rebuild
```

Expected: the new derivation builds and tests successfully. No client test reports `Text file busy`, and the generated fake-curl scripts remain non-executable.

- [ ] **Step 6: Inspect the final scope**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff -- src/brain/client.rs src/brain/permission_hook.rs
```

Expected: production behavior changes are confined to structured decision classification and bounded retry in `src/brain/client.rs`; `src/brain/permission_hook.rs` changes only in tests. The research, spec, and plan documents are the only additional files. No config, activity schema, TUI layout, recovery-session, dependency, or unrelated working-tree changes appear.

- [ ] **Step 7: Update Beads without committing or publishing**

Run:

```bash
bd -C /home/alexander/.beads-planning close codexctl-t0lu codexctl-mz29 codexctl-9vr1 --reason "Implemented and verified"
bd -C /home/alexander/.beads-planning close codexctl-z0qg codexctl-b7oe --reason "Incomplete structured decision recovery implemented and verified"
git status --short
```

Expected: all implementation tasks, the implementation epic, and `codexctl-b7oe` are closed only after every required gate passes. Report the uncommitted files and wait for explicit commit/push authorization.

## Stress Test Results: Incomplete Structured Completion Recovery Plan

### Resolved Decisions

- Dependency ordering: Task 1 may proceed independently; Task 2 waits for and
  reconciles the landed `codexctl-6r0o` command seam.
- Retry eligibility: retry incomplete and malformed generated JSON once, but
  never retry transport, provider-envelope, API, or valid schema failures.
- Timeout enforcement: both attempts share one monotonic budget, and retry
  requires at least one whole second of remaining curl time.
- Failure precedence: the second attempt's failure remains primary, with only
  bounded first-attempt context.
- Diagnostic security: new diagnostics expose only allowlisted, bounded, and
  redacted completion metadata; they never include prompts or generated text.
- Command-factory scope: fresh commands apply only to permission-decision
  inference; completion and recovery calls remain one-shot.
- Test determinism: response files and an invocation counter replace sleeps or
  executable temporary scripts.
- Lifecycle proof: exact client call counts compose with the permission-hook
  characterization to prove single delivery and one persisted lifecycle.
- Configuration and rollback: the total attempt count remains a private
  constant set to two; rollback requires no configuration or state migration.

### Changes Made

- No design changes were required.
- The plan's retry fixtures were already revised before interrogation to append
  state files to the exact script path and provide complete test bodies.

### Deferred / Parking Lot

- Make the retry count configurable only if operational evidence shows that a
  fixed two-attempt policy is insufficient.

### Confidence Assessment

- Overall: High
- Areas of concern: Task 2 remains intentionally blocked until
  `codexctl-6r0o` lands and its stable-shell command seam is present in this
  worktree.
