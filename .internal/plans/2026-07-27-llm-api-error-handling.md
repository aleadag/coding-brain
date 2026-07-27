# LLM API Error Handling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Report schema-specific Ollama and OpenAI API failures accurately while keeping malformed generated decisions distinct from malformed provider envelopes.

**Architecture:** Keep endpoint-path protocol selection unchanged. Add one content extractor for the native Ollama schema and one for the OpenAI-compatible schema; both decision inference and recovery completion reuse the selected extractor before parsing generated content.

**Tech Stack:** Rust 2024 workspace, `serde_json`, built-in unit tests, Cargo, rustfmt, Clippy

## Global Constraints

- `/api/generate` continues to use the native Ollama request and response format.
- `/v1/chat/completions` continues to use the OpenAI-compatible request and response format.
- Provider-specific extractors validate only their supported success and error shapes.
- Only successfully extracted generated content reaches decision or recovery parsing.
- Recognized API errors take precedence over generated content in the same envelope.
- Immediate diagnostics retain provider messages; persisted activity keeps its existing bounded secret redaction.
- Do not change HTTP status capture, curl arguments, redirects, retries, logging, configuration, public APIs, persistence, provider selection, or user-facing documentation.
- Change only `src/brain/client.rs`.

---

### Task 1: Validate provider envelopes before generated-content parsing

**Files:**
- Modify: `src/brain/client.rs:165-240`
- Test: `src/brain/client.rs:418-490`

**Interfaces:**
- Consumes: `serde_json::Value`, the existing endpoint-selected branches in `call_llm`, `parse_ollama_response`, `parse_openai_response`, `parse_suggestion_json`, and `parse_recovery_suggestion_json`.
- Produces: `fn api_error(provider: &str, message: &str) -> String`, `fn extract_ollama_content(json: &serde_json::Value) -> Result<&str, String>`, `fn extract_openai_content(json: &serde_json::Value) -> Result<&str, String>`, and the test seam `fn call_llm_with_program(config: &BrainConfig, prompt: &str, program: &Path) -> Result<String, String>`.

**Acceptance Criteria:**
- Native Ollama `{"error":"..."}` envelopes report `Ollama API error: ...`.
- OpenAI-compatible `{"error":{"message":"..."}}` envelopes report `OpenAI API error: ...`.
- API errors take precedence if an envelope also contains generated content.
- Missing or wrong-shaped provider fields report `invalid Ollama response` or `invalid OpenAI response`.
- Valid provider envelopes containing generated JSON without `action` retain the distinct `missing 'action' field` suggestion error.
- Decision inference and recovery completion use the same schema-specific extractors, with completion-path behavior covered through the existing fake-curl fixture.
- Existing successful Ollama and OpenAI response parsing remains unchanged.
- Focused tests and all workspace quality gates pass.

- [ ] **Step 1: Add failing provider-envelope regression tests**

Add these tests beside the existing wrapped-response tests:

```rust
#[test]
fn parse_ollama_api_error_is_not_a_suggestion_error() {
    let error =
        parse_ollama_response(r#"{"error":"unable to load model"}"#).unwrap_err();
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
        parse_ollama_response(r#"{"error":{"message":"wrong native shape"}}"#)
            .unwrap_err();
    assert_eq!(
        ollama,
        "invalid Ollama response: 'error' must be a string"
    );

    let openai =
        parse_openai_response(r#"{"error":"wrong OpenAI shape"}"#).unwrap_err();
    assert_eq!(
        openai,
        "invalid OpenAI response: missing string 'error.message' field"
    );
}

#[test]
fn malformed_generated_decisions_remain_suggestion_errors() {
    let ollama = parse_ollama_response(
        r#"{"response":"{\"reasoning\":\"no decision\"}"}"#,
    )
    .unwrap_err();
    assert_eq!(ollama, "missing 'action' field");

    let openai = parse_openai_response(
        r#"{"choices":[{"message":{"content":"{\"reasoning\":\"no decision\"}"}}]}"#,
    )
    .unwrap_err();
    assert_eq!(openai, "missing 'action' field");
}
```

- [ ] **Step 2: Run the focused tests and verify the red state**

Run:

```bash
direnv exec . cargo test brain::client::tests::parse_ollama_api_error_is_not_a_suggestion_error
```

Expected: FAIL because the current Ollama fallback reports
`missing 'action' field` instead of `Ollama API error: unable to load model`.

Then run:

```bash
direnv exec . cargo test brain::client::tests::
```

Expected: the newly added provider-error and malformed-envelope tests fail;
the existing successful wrapped-response tests pass.

- [ ] **Step 3: Add schema-specific content extractors**

Add these private helpers immediately before `call_llm`:

```rust
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
        .ok_or_else(|| {
            "invalid Ollama response: missing string 'response' field".to_string()
        })
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
            "invalid OpenAI response: missing string 'choices[0].message.content' field"
                .to_string()
        })
}
```

These functions deliberately validate different schemas. Do not replace them
with one generic top-level `error` extractor.

- [ ] **Step 4: Reuse the extractors in decision and recovery paths**

Keep `call_llm` as the production entry point and move its body into a private
program-injected function, matching the existing `infer_with_program` pattern:

```rust
fn call_llm(config: &BrainConfig, prompt: &str) -> Result<String, String> {
    call_llm_with_program(config, prompt, Path::new("curl"))
}

fn call_llm_with_program(
    config: &BrainConfig,
    prompt: &str,
    program: &Path,
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
    let stdout = curl_post(program, config, &body)?;
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
```

Replace the generated-content lookup in `parse_ollama_response` with:

```rust
let generated = extract_ollama_content(&json)?;
parse_suggestion_json(generated)
```

Replace the generated-content lookup in `parse_openai_response` with:

```rust
let content = extract_openai_content(&json)?;
parse_suggestion_json(content)
```

Keep endpoint detection, request payloads, JSON parsing errors, and
`parse_suggestion_json` unchanged.

- [ ] **Step 5: Add completion-path integration coverage**

Use the existing Unix fake-curl fixture to prove `call_llm`'s extracted content
and provider errors for both endpoint-selected formats:

```rust
#[cfg(unix)]
#[test]
fn completion_extracts_generated_recovery_content_for_both_formats() {
    let recovery = r#"{"action":"continue","confidence":0.9}"#;
    let (_ollama_temp, ollama_curl) = fake_curl(
        r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"response":"{\"action\":\"continue\",\"confidence\":0.9}"}'"#,
    );
    let ollama =
        call_llm_with_program(&BrainConfig::default(), "prompt", &ollama_curl).unwrap();
    assert_eq!(ollama, recovery);
    assert!(parse_recovery_suggestion_json(&ollama).is_ok());

    let (_openai_temp, openai_curl) = fake_curl(
        r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"choices":[{"message":{"content":"{\"action\":\"continue\",\"confidence\":0.9}"}}]}'"#,
    );
    let openai_config = BrainConfig {
        endpoint: "http://brain.example.test/v1/chat/completions".into(),
        ..BrainConfig::default()
    };
    let openai =
        call_llm_with_program(&openai_config, "prompt", &openai_curl).unwrap();
    assert_eq!(openai, recovery);
    assert!(parse_recovery_suggestion_json(&openai).is_ok());
}

#[cfg(unix)]
#[test]
fn completion_surfaces_schema_specific_api_errors() {
    let (_ollama_temp, ollama_curl) = fake_curl(
        r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":"unable to load model"}'"#,
    );
    assert_eq!(
        call_llm_with_program(&BrainConfig::default(), "prompt", &ollama_curl)
            .unwrap_err(),
        "Ollama API error: unable to load model"
    );

    let (_openai_temp, openai_curl) = fake_curl(
        r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":{"message":"model unavailable","type":"server_error"}}'"#,
    );
    let openai_config = BrainConfig {
        endpoint: "http://brain.example.test/v1/chat/completions".into(),
        ..BrainConfig::default()
    };
    assert_eq!(
        call_llm_with_program(&openai_config, "prompt", &openai_curl)
            .unwrap_err(),
        "OpenAI API error: model unavailable"
    );
}
```

- [ ] **Step 6: Run focused tests and format the change**

Run:

```bash
direnv exec . cargo test brain::client::tests::
direnv exec . cargo fmt --check
```

Expected: all brain-client tests pass and rustfmt reports no diff. If rustfmt
reports a diff, run `direnv exec . cargo fmt`, inspect only the resulting
`src/brain/client.rs` changes, then rerun both commands.

- [ ] **Step 7: Run workspace quality gates**

Run:

```bash
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
```

Expected: all workspace tests pass and Clippy exits successfully with no
warnings.

- [ ] **Step 8: Review the final scope and prepare the handoff**

Run:

```bash
git diff --check
git status --short
git diff -- src/brain/client.rs
```

Expected: implementation changes are confined to `src/brain/client.rs`;
the approved spec and plan are the only additional untracked files. Do not
commit or push without explicit user authorization. If authorization is later
given, the suggested commit message is:

```text
🐛 fix: surface LLM API errors accurately
```

## Stress Test Results: LLM API error-handling plan

### Resolved Decisions

- **Task boundary:** Keep one atomic task because both schemas, both consumers,
  and their tests change one file and one response contract.
- **Red state:** Initial regressions call existing parser entry points, so they
  compile before implementation and fail on the observed misleading error.
- **Schema precedence:** Each extractor validates its own error shape before
  success content; malformed errors cannot fall through to permission parsing.
- **Recovery integration:** Add `call_llm_with_program`, mirroring the existing
  inference seam, so fake-curl tests prove both success and error behavior
  through the completion path.
- **Verification and rollback:** Run focused tests, formatting, full tests, and
  Clippy. Keep commit and push outside current authority; rollback is a
  one-file implementation revert with no migration.

### Changes Made

- Replaced helper-only recovery coverage with completion-path integration
  tests.
- Added the exact `call_llm_with_program` signature and implementation.
- Added fake-curl success and API-error cases for both endpoint formats.

### Deferred / Parking Lot

- HTTP-status capture and non-JSON proxy error classification remain outside
  the evidenced bug and approved design.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** The fake-curl integration tests are Unix-only, matching
  the existing fixture; cross-platform parser tests still cover the core
  schema classification on every target.
