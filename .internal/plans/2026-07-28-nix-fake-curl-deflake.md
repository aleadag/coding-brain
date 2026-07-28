# Nix Fake-Curl Fixture Deflake Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Eliminate intermittent Nix `ETXTBSY` failures by running dynamically generated fake-curl scripts through `sh` instead of executing their freshly written inodes.

**Architecture:** Preserve the existing path-only production helpers for `curl`. Add private prepared-command variants that share the existing payload and response logic, and make the shared process runner consume an owned `Command`. Unix tests will create non-executable scripts and prepare `sh <script>` commands without changing curl arguments, stdin, parsing, timeouts, or error behavior.

**Tech Stack:** Rust standard library (`std::process::Command`), Cargo tests, Nix flakes.

## Global Constraints

- Modify only `src/brain/client.rs`; the approved spec and this plan are documentation artifacts, not runtime changes.
- Do not add dependencies, retries, sleeps, synchronization, global state, public APIs, or production behavior changes.
- Resolve `sh` from the test process `PATH`; do not use `$SHELL` or `/bin/sh`.
- Pass the script path as a distinct argument and keep script bodies as test literals; do not construct an interpolated shell command string.
- Keep one `TempDir` and one owned `Command` per fixture invocation so parallel tests remain independent.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Run Fake Curl Through a Stable Shell Interpreter

**Files:**
- Modify: `src/brain/client.rs:27-130`
- Modify: `src/brain/client.rs:208-241`
- Test: `src/brain/client.rs:342-556`

**Interfaces:**
- Consumes: Existing private `infer_with_program(&BrainConfig, &str, &Path)`, `call_llm_with_program(&BrainConfig, &str, &Path)`, curl argument ordering, and response parsing.
- Produces: Private `infer_with_command(&BrainConfig, &str, Command) -> Result<BrainSuggestion, String>`, `call_llm_with_command(&BrainConfig, &str, Command) -> Result<String, String>`, and `curl_post_command(Command, &BrainConfig, &str) -> Result<Vec<u8>, String>`.

**Acceptance Criteria:**
- Production `infer` and completion paths still execute `curl` with the same arguments, stdin, timeout, bounded output handling, parsing, and errors.
- Unix fake-curl fixtures are ordinary non-executable files invoked as `sh <script> <curl arguments...>`.
- Existing tests continue to prove curl argv, prompt stdin, response-size, endpoint-schema success, and endpoint-schema error behavior.
- A regression assertion proves the generated fixture has no execute bits.
- Parallel tests use unique temporary directories and no global environment mutation or serialization.
- Focused tests, `cargo fmt --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, `nix build .#`, and `nix build .# --rebuild` pass.

- [ ] **Step 1: Change the fixture and callers first**

Replace the executable fixture with a non-executable script and prepared shell
command:

```rust
#[cfg(unix)]
fn fake_curl(
    script: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::process::Command,
) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("curl");
    std::fs::write(&path, format!("set -eu\n{script}\n")).unwrap();
    let mut command = std::process::Command::new("sh");
    command.arg(&path);
    (temp, path, command)
}
```

Update the inference tests to consume prepared commands:

```rust
let (temp, script, curl) = fake_curl(
    r#"printf '%s\n' "$@" > "${0}.args"
dd of="${0}.stdin" 2>/dev/null
printf '%s' '{"response":"{\"action\":\"approve\",\"reasoning\":\"safe\",\"confidence\":0.9}"}'"#,
);
assert_eq!(
    std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
    0
);
let suggestion = infer_with_command(&config, secret_prompt, curl).unwrap();
```

```rust
let (_temp, _script, curl) =
    fake_curl("dd if=/dev/zero bs=1048577 count=1 2>/dev/null");
let error = infer_with_command(&BrainConfig::default(), "prompt", curl).unwrap_err();
```

Update the completion success test:

```rust
let (_ollama_temp, _ollama_script, ollama_curl) = fake_curl(
    r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"response":"{\"action\":\"continue\",\"confidence\":0.9}"}'"#,
);
let ollama =
    call_llm_with_command(&BrainConfig::default(), "prompt", ollama_curl).unwrap();

let (_openai_temp, _openai_script, openai_curl) = fake_curl(
    r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"choices":[{"message":{"content":"{\"action\":\"continue\",\"confidence\":0.9}"}}]}'"#,
);
let openai = call_llm_with_command(&openai_config, "prompt", openai_curl).unwrap();
```

Update the completion error test:

```rust
let (_ollama_temp, _ollama_script, ollama_curl) = fake_curl(
    r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":"unable to load model"}'"#,
);
assert_eq!(
    call_llm_with_command(&BrainConfig::default(), "prompt", ollama_curl).unwrap_err(),
    "Ollama API error: unable to load model"
);

let (_openai_temp, _openai_script, openai_curl) = fake_curl(
    r#"dd of=/dev/null 2>/dev/null
printf '%s' '{"error":{"message":"model unavailable","type":"server_error"}}'"#,
);
assert_eq!(
    call_llm_with_command(&openai_config, "prompt", openai_curl).unwrap_err(),
    "OpenAI API error: model unavailable"
);
```

- [ ] **Step 2: Run the focused tests and verify the test-first failure**

Run:

```bash
cargo test brain::client::tests --lib
```

Expected: compilation fails because `infer_with_command` and
`call_llm_with_command` do not exist yet. The failure must not be a syntax error
or an unrelated test failure.

- [ ] **Step 3: Add the private prepared-command seams**

Make the path-only inference helper construct a command and delegate:

```rust
fn infer_with_program(
    config: &BrainConfig,
    prompt: &str,
    program: &Path,
) -> Result<BrainSuggestion, String> {
    infer_with_command(config, prompt, Command::new(program))
}

fn infer_with_command(
    config: &BrainConfig,
    prompt: &str,
    command: Command,
) -> Result<BrainSuggestion, String> {
    let is_openai = is_openai_compatible(&config.endpoint);

    let payload = if is_openai {
        serde_json::json!({
            "model": config.model,
            "messages": [
                {"role": "user", "content": prompt}
            ],
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

    let body = serde_json::to_string(&payload).map_err(|e| format!("json error: {e}"))?;
    let stdout = curl_post_command(command, config, &body)?;
    let stdout = String::from_utf8_lossy(&stdout);
    if is_openai {
        parse_openai_response(&stdout)
    } else {
        parse_ollama_response(&stdout)
    }
}
```

Rename the existing `curl_post` runner and make it consume the prepared
command; keep its body otherwise byte-for-byte unchanged:

```rust
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
```

Make the completion path delegate in the same way:

```rust
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
```

- [ ] **Step 4: Format and run the focused regression suite**

Run:

```bash
cargo fmt
cargo test brain::client::tests --lib
```

Expected: all `brain::client::tests` pass, including
`oversized_inference_response_abstains`,
`completion_extracts_generated_recovery_content_for_both_formats`, and
`completion_surfaces_schema_specific_api_errors`.

- [ ] **Step 5: Inspect the surgical diff**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git diff -- src/brain/client.rs
```

Expected: only the command delegation, non-executable fixture, fixture callers,
and regression assertion changed. There are no retries, sleeps, dependency
changes, public interfaces, or unrelated formatting edits. The command uses
Git's default whitespace checks because this checkout's user-local
`indent-with-non-tab` setting conflicts with Rustfmt's required space
indentation.

- [ ] **Step 6: Run repository quality gates**

Run:

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Expected: every command exits successfully with no warnings.

- [ ] **Step 7: Rebuild and test the modified source in Nix**

Run:

```bash
nix build .#
nix build .# --rebuild
```

Expected: Nix computes and builds a new derivation from the modified source,
then successfully executes its package checks again. Both executions pass
without `Text file busy (os error 26)`.

- [ ] **Step 8: Hand off without publishing**

Run:

```bash
git status --short
```

Expected: only `src/brain/client.rs`, the approved design spec, and this plan
are changed. Report verification evidence and wait for explicit authorization
before any commit, push, or publication.
