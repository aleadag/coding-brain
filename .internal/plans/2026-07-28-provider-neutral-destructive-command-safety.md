# Provider-Neutral Destructive-Command Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. The tasks already exist under implementation epic `codexctl-w03a`. Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Ensure every supported provider permission hook deterministically denies confirmed destructive shell commands before model inference, including dynamic `rm` arguments built with command substitution.

**Architecture:** Keep provider-specific tool names and payload parsing in the provider adapters, using `PermissionHookRequest.command` as the normalized shell-command capability. Make the safety evaluator consume that optional command directly, then extend its private tokenizer to mark active command substitution and deny it anywhere in `rm` arguments before flags or targets are classified.

**Tech Stack:** Rust 2024, Cargo unit and integration tests, `serde_json`.

## Global Constraints

- Provider parsers populate `PermissionHookRequest.command` only for recognized shell-command tools: Codex `Bash`, Claude `Bash`, and Antigravity `run_command`.
- Unknown or renamed provider tools have no command capability and fall back to manual input; they never reach automatic model approval.
- Raw provider tool names remain unchanged for permission identity, audit records, model context, and provider responses.
- Deterministic safety runs before provider policy and model inference.
- Active `$()` and backticks in any `rm` argument deny with `unsafe-recursive-delete-expansion`, including substitutions that can synthesize flags and targets.
- Single-quoted or escaped substitution syntax remains inert.
- Do not execute substitutions or recursively interpret `sh -c`, `bash -c`, `eval`, or equivalent nested shell programs.
- Provider-native permission policy is the preferred long-term enforcement point; retain centralized deterministic denial until equivalent provider coverage is proven.
- Do not add dependencies, configuration fields, activity schema changes, TUI changes, or public APIs.
- Do not commit, push, or publish without explicit user authorization.

**Beads:** implementation epic `codexctl-w03a`; tasks `codexctl-5u9w`, `codexctl-sqnm`, and `codexctl-in4t`.

---

### Task 1: Make Safety Applicability Provider-Neutral

**Files:**
- Modify: `src/provider_hooks/mod.rs:48-58`
- Modify: `src/brain/safety.rs:1-26`
- Modify: `src/brain/safety.rs:416-512`
- Modify: `src/brain/permission_hook.rs:230-265`
- Modify: `src/brain/permission_hook.rs:570-598`
- Test: `tests/hook_activity.rs:38-344`
- Test: `tests/hook_activity.rs:580-690`

**Interfaces:**
- Consumes: `PermissionHookRequest.command: Option<String>` produced by the Codex, Claude, and Antigravity provider parsers.
- Produces: `safety::evaluate(command: Option<&str>) -> Option<SafetyDeny>`.
- Preserves: `BrainDecisionRequest.tool_name` and `BrainDecisionRequest.tool_input` for model context and audit behavior.

**Acceptance Criteria:**
- Recognized shell-command tools for all three providers deny `rm -rf /` before model inference.
- Unsupported tools retain no command capability and fall back safely.
- Raw tool identity, permission identity, audit fields, and provider response schemas remain unchanged.
- Safety applicability contains no provider-specific tool-name comparison.

- [ ] **Step 1: Add the failing provider-matrix regression**

Add this helper and test to `tests/hook_activity.rs` beside the existing deterministic-deny integration tests:

```rust
fn assert_safety_deny(home: &Path, expected_rule_id: &str) {
    let events = activity(home).read().unwrap().events().to_vec();
    let denied = events
        .iter()
        .find(|event| event.state == ActivityState::Denied)
        .expect("missing deterministic deny activity");
    assert_eq!(denied.rule_id.as_deref(), Some(expected_rule_id));
    assert!(
        events
            .iter()
            .all(|event| event.state != ActivityState::Allowed)
    );
}

#[test]
fn destructive_commands_are_denied_across_permission_providers() {
    let codex_home = tempfile::tempdir().unwrap();
    install_model_fixture(codex_home.path(), "approve");
    let codex = run_provider_permission_hook(
        codex_home.path(),
        "codex",
        None,
        &permission_payload(codex_home.path(), "rm -rf /"),
    );
    let response: serde_json::Value = serde_json::from_slice(&codex.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert_safety_deny(codex_home.path(), "irreversible-root-delete");
    assert!(!codex_home.path().join("bin/curl.args").exists());

    let claude_home = tempfile::tempdir().unwrap();
    install_model_fixture(claude_home.path(), "approve");
    let mut claude_payload: serde_json::Value =
        serde_json::from_slice(&claude_permission_payload(claude_home.path(), None)).unwrap();
    claude_payload["tool_input"]["command"] = serde_json::json!("rm -rf /");
    let claude = run_provider_permission_hook(
        claude_home.path(),
        "claude",
        None,
        &serde_json::to_vec(&claude_payload).unwrap(),
    );
    let response: serde_json::Value = serde_json::from_slice(&claude.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert_safety_deny(claude_home.path(), "irreversible-root-delete");
    assert!(!claude_home.path().join("bin/curl.args").exists());

    let antigravity_home = tempfile::tempdir().unwrap();
    install_model_fixture(antigravity_home.path(), "approve");
    let mut antigravity_payload: serde_json::Value = serde_json::from_slice(
        &antigravity_permission_payload(antigravity_home.path(), None),
    )
    .unwrap();
    antigravity_payload["toolCall"]["args"]["CommandLine"] =
        serde_json::json!("rm -rf /");
    let antigravity = run_provider_permission_hook(
        antigravity_home.path(),
        "antigravity",
        Some("PreToolUse"),
        &serde_json::to_vec(&antigravity_payload).unwrap(),
    );
    let response: serde_json::Value =
        serde_json::from_slice(&antigravity.stdout).unwrap();
    assert_eq!(response["decision"], "deny");
    assert_safety_deny(antigravity_home.path(), "irreversible-root-delete");
    assert!(
        !antigravity_home.path().join("bin/curl.args").exists()
    );
}
```

The approving model fixture is deliberate: a provider wiring bypass produces
an allow response and makes the regression fail. Its `curl.args` side effect
also proves that the deterministic deny occurred before inference rather than
overriding a model result afterward.

- [ ] **Step 2: Run the provider matrix and verify the red state**

Run:

```bash
direnv exec . cargo test --test hook_activity destructive_commands_are_denied_across_permission_providers -- --exact
```

Expected: FAIL for Antigravity because `run_command` currently exits
`safety::evaluate` before inspecting `rm -rf /`; Codex and Claude deny.

- [ ] **Step 3: Make the safety boundary consume normalized capability**

Document the invariant in `src/provider_hooks/mod.rs`:

```rust
pub(crate) struct PermissionHookRequest {
    pub provider: AgentProvider,
    pub lifecycle: LifecycleIdentity,
    pub request_key: String,
    pub project: String,
    pub tool_name: String,
    /// Shell command extracted only for a provider's recognized command tool.
    pub command: Option<String>,
    pub tool_use_id: Option<String>,
    pub provider_policy: ProviderPermissionPolicy,
}
```

Remove the `BrainDecisionRequest` import from `src/brain/safety.rs` and change
the evaluator boundary:

```diff
-use super::query::BrainDecisionRequest;
-
-pub(crate) fn evaluate(request: &BrainDecisionRequest) -> Option<SafetyDeny> {
-    if request.tool_name != "Bash" {
-        return None;
-    }
-
+pub(crate) fn evaluate(command: Option<&str>) -> Option<SafetyDeny> {
+    let input = command?;

     let mut assignments = HashMap::new();
-    for command in tokenize_commands(&request.tool_input) {
+    for command in tokenize_commands(input) {
```

At the authoritative permission boundary in
`src/brain/permission_hook.rs`, pass the extracted capability:

```diff
-    let evaluation = if let Some(safety) = super::safety::evaluate(&brain_request) {
+    // This is the authoritative deterministic safety and provider-policy
+    // boundary; evaluate_request performs model evaluation only.
+    let evaluation = if let Some(safety) =
+        super::safety::evaluate(request.command.as_deref())
+    {
```

Remove the duplicate safety check at the start of `evaluate_request`, make the
function private to this module, and add a comment at its call site that
`run_with_gate_and_stores` is the authoritative safety and provider-policy
boundary. Same-module tests retain access:

```diff
-pub(crate) fn evaluate_request<F>(
+fn evaluate_request<F>(
     request: &BrainDecisionRequest,
     config: Option<&BrainConfig>,
     gate_mode: BrainGateMode,
     persistence_error: Option<&str>,
     supported: bool,
     infer: F,
 ) -> HookEvaluation
 where
     F: FnOnce(&BrainConfig, &str) -> Result<BrainSuggestion, String>,
 {
-    if let Some(safety) = super::safety::evaluate(request) {
-        return HookEvaluation::Deny {
-            brain: None,
-            deterministic: true,
-            safety: Some(safety),
-            terminal_state: ActivityState::Denied,
-        };
-    }
     if let Some(error) = persistence_error {
```

Update the safety unit-test helper and unsupported-capability regression:

```rust
fn evaluate_command(command: &str) -> Option<SafetyDeny> {
    evaluate(Some(command))
}

#[test]
fn missing_command_capability_has_no_deterministic_decision() {
    assert!(evaluate(None).is_none());
}
```

Replace existing `evaluate(&request(command))` calls with
`evaluate_command(command)` and remove the old raw-tool-name test.

- [ ] **Step 4: Run focused applicability tests and verify green**

Run:

```bash
direnv exec . cargo test brain::safety::tests --lib
direnv exec . cargo test brain::permission_hook::tests --lib
direnv exec . cargo test --test hook_activity destructive_commands_are_denied_across_permission_providers -- --exact
direnv exec . cargo test --test hook_activity unsupported_antigravity_post_tool_use_is_observation_only -- --exact
```

Expected: PASS. The provider matrix records
`irreversible-root-delete` for all three providers; unsupported Antigravity
tools still ask and never produce an allowed activity.

- [ ] **Step 5: Inspect the task diff without committing**

Run:

```bash
git diff --check
git diff -- src/provider_hooks/mod.rs src/brain/safety.rs src/brain/permission_hook.rs tests/hook_activity.rs
```

Expected: no whitespace errors and only provider-neutral applicability,
invariant documentation, and regression coverage. Do not commit without
explicit authorization.

---

### Task 2: Fail Closed for Command Substitution in `rm` Arguments

**Files:**
- Modify: `src/brain/safety.rs:13-18`
- Modify: `src/brain/safety.rs:39-73`
- Modify: `src/brain/safety.rs:286-415`
- Test: `src/brain/safety.rs:416-512`
- Test: `tests/hook_activity.rs:580-690`

**Interfaces:**
- Consumes: Task 1's `safety::evaluate(command: Option<&str>)`.
- Produces: private `ShellWord.command_substitution: bool` populated by `tokenize_commands`.
- Preserves: existing `variable_expansion`, `tilde_expansion`, assignment resolution, wrapper unwrapping, and safety rule IDs.

**Acceptance Criteria:**
- Active dollar-parenthesis and backtick substitutions in any `rm` argument use `unsafe-recursive-delete-expansion`.
- Substitutions that synthesize `-rf` and `/` are denied before flag or target classification.
- Single-quoted and escaped substitution syntax remains inert.
- Existing literal root, home, unresolved parameter, wrapper-command, and ordinary-command cases pass unchanged.

- [ ] **Step 1: Add failing active and inert substitution regressions**

Add these unit tests to `src/brain/safety.rs`:

```rust
#[test]
fn command_substitution_in_rm_arguments_denies() {
    for command in [
        "rm -rf \"$(resolve-target)\"",
        "rm -rf `resolve-target`",
        "rm -rf \"prefix-$(resolve-target)\"",
        "rm $(printf '%s\\n' -rf /)",
        "rm `printf '%s\\n' -rf /`",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(
            deny.rule_id,
            "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }
}

#[test]
fn inert_command_substitution_syntax_has_no_deterministic_decision() {
    for command in [
        "rm -rf '$(resolve-target)'",
        "rm -rf '`resolve-target`'",
        "rm -rf \"\\$(resolve-target)\"",
        "rm -rf \\`resolve-target\\`",
    ] {
        assert!(evaluate_command(command).is_none(), "{command}");
    }
}
```

Add this provider-level regression beside the Task 1 matrix:

```rust
#[test]
fn antigravity_dynamic_rm_arguments_deny_before_inference() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(home.path(), None))
            .unwrap();
    payload["toolCall"]["args"]["CommandLine"] =
        serde_json::json!("rm $(printf '%s\\n' -rf /)");

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &serde_json::to_vec(&payload).unwrap(),
    );

    let response: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["decision"], "deny");
    assert_safety_deny(home.path(), "unsafe-recursive-delete-expansion");
    assert!(!home.path().join("bin/curl.args").exists());
}
```

- [ ] **Step 2: Run the substitution tests and verify the red state**

Run:

```bash
direnv exec . cargo test brain::safety::tests::command_substitution_in_rm_arguments_denies --lib -- --exact
direnv exec . cargo test brain::safety::tests::inert_command_substitution_syntax_has_no_deterministic_decision --lib -- --exact
```

Expected: the active-substitution test FAILS because the tokenizer records only
generic variable expansion and the reference parser rejects `$()`; the inert
test passes before production changes.

- [ ] **Step 3: Track syntactically active command substitution**

Extend the private shell word:

```rust
struct ShellWord {
    text: String,
    variable_expansion: bool,
    tilde_expansion: bool,
    command_substitution: bool,
}
```

Add a `command_substitution: &mut bool` accumulator to `push_word` and
`push_command`, initialize it beside the existing tokenizer flags, and move it
into each `ShellWord` with `std::mem::take`.

In the active double-quote branch, mark `$(` and backticks:

```rust
if active_quote == '"' {
    if character == '$' {
        variable_expansion = true;
        if chars.peek() == Some(&'(') {
            command_substitution = true;
        }
    } else if character == '`' {
        command_substitution = true;
    }
}
```

Outside quotes, update the existing dollar arm and add a backtick arm:

```rust
'$' => {
    variable_expansion = true;
    command_substitution |= chars.peek() == Some(&'(');
    word_started = true;
    word.push(character);
}
'`' => {
    command_substitution = true;
    word_started = true;
    word.push(character);
}
```

Do not mark syntax in the existing escape path or single-quote branch.

- [ ] **Step 4: Deny dynamic `rm` arguments before classifying flags**

Immediately after unwrapping and recognizing `rm`, inspect every remaining
argument:

```rust
let args = &words[1..];
if args.iter().any(|argument| argument.command_substitution) {
    return Some(SafetyDeny {
        rule_id: "unsafe-recursive-delete-expansion",
        reason:
            "refusing deletion through an unresolved command substitution".into(),
    });
}
if !args.iter().any(|argument| is_recursive_flag(&argument.text)) {
    continue;
}
```

Keep the existing literal root, home, parameter-default, and assignment-based
expansion checks after this guard.

- [ ] **Step 5: Run focused tests and verify green**

Run:

```bash
direnv exec . cargo test brain::safety::tests --lib
direnv exec . cargo test brain::permission_hook::tests::deterministic_deny_precedes_inference --lib -- --exact
direnv exec . cargo test --test hook_activity destructive_commands_are_denied_across_permission_providers -- --exact
direnv exec . cargo test --test hook_activity antigravity_dynamic_rm_arguments_deny_before_inference -- --exact
```

Expected: PASS. Active substitution receives
`unsafe-recursive-delete-expansion`, inert syntax remains undecided, and every
provider still denies literal root deletion before inference.

- [ ] **Step 6: Inspect the task diff without committing**

Run:

```bash
git diff --check
git diff -- src/brain/safety.rs tests/hook_activity.rs
```

Expected: no whitespace errors and no nested-shell parsing, configuration, or
unrelated refactoring. Do not commit without explicit authorization.

---

### Task 3: Run Full Verification and Complete the Beads Work

**Files:**
- Verify: `src/provider_hooks/mod.rs`
- Verify: `src/brain/safety.rs`
- Verify: `src/brain/permission_hook.rs`
- Verify: `tests/hook_activity.rs`
- Verify: `.internal/specs/2026-07-28-provider-neutral-destructive-command-safety-design.md`
- Verify: `.internal/plans/2026-07-28-provider-neutral-destructive-command-safety.md`

**Interfaces:**
- Consumes: the completed Task 1 and Task 2 implementation.
- Produces: fresh quality-gate evidence and an accurate Beads/worktree handoff.

**Acceptance Criteria:**
- Focused tests, `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo build` pass.
- `git diff --check` is clean and only approved files changed.
- `codexctl-msos`, implementation tasks, implementation epic, and brainstorming session close only after verification succeeds.

Run the following steps in order and stop at the first failure. Return a
failure to the task that owns it; do not continue collecting mixed evidence
from a broken worktree.

- [ ] **Step 1: Run focused regression tests**

Run:

```bash
direnv exec . cargo test brain::safety::tests --lib
direnv exec . cargo test brain::permission_hook::tests --lib
direnv exec . cargo test --test hook_activity destructive_commands_are_denied_across_permission_providers -- --exact
direnv exec . cargo test --test hook_activity antigravity_dynamic_rm_arguments_deny_before_inference -- --exact
direnv exec . cargo test --test hook_activity unsupported_antigravity_post_tool_use_is_observation_only -- --exact
```

Expected: all focused tests PASS.

- [ ] **Step 2: Format and verify formatting**

Run:

```bash
direnv exec . cargo fmt
direnv exec . cargo fmt --check
```

Expected: `cargo fmt --check` exits 0.

- [ ] **Step 3: Run the full workspace test suite**

Run:

```bash
direnv exec . cargo test
```

Expected: all workspace unit, integration, and documentation tests PASS.

- [ ] **Step 4: Run the exact all-targets lint gate**

Run:

```bash
direnv exec . cargo clippy --all-targets -- -D warnings
```

Expected: exits 0 with no warnings.

- [ ] **Step 5: Build the workspace**

Run:

```bash
direnv exec . cargo build
```

Expected: exits 0.

- [ ] **Step 6: Verify scope and Beads state**

Run:

```bash
git diff --check
git status --short
bd -C /home/alexander/.beads-planning show codexctl-msos codexctl-w03a codexctl-5u9w codexctl-sqnm codexctl-in4t
```

Expected: only the approved implementation, tests, spec, and plan are changed;
all tests and lint evidence are fresh. Close the completed task beads, epic,
`codexctl-msos`, and brainstorming bead only after those checks succeed. Do
not commit, push, or sync without explicit authorization.

## Stress Test Results: Provider-Neutral Destructive-Command Safety Plan

### Resolved Decisions

- **Task boundaries:** Keep provider-neutral applicability and command
  substitution as sequential implementation tasks, followed by a dependent
  full-verification task.
- **Red states:** Use the Antigravity allow bypass and missing substitution
  deny as the two independent pre-fix failure signals.
- **Authoritative boundary:** Make `evaluate_request` module-private after
  removing its duplicate safety check so future external callers cannot bypass
  the permission-hook safety boundary.
- **Inference proof:** Assert the fake model's `curl.args` side effect is absent
  for deterministic denies on every provider.
- **Verification order:** Run focused tests, formatting, full tests,
  all-targets Clippy, and build in order, stopping at the first failure.
- **Completion:** Close implementation and parent Beads only after fresh
  verification; leave `codexctl-dchq` open and do not commit or push without
  authorization.
- **Test interface:** Parameterize the activity assertion by expected safety
  rule so literal-root and dynamic-argument integration tests share one
  consistent helper.

### Changes Made

- Made the planned model-evaluation helper private to the permission-hook
  module.
- Added direct evidence that deterministic denial precedes inference.
- Replaced the hard-coded root-rule assertion with a reusable safety-rule
  assertion.
- Added complete provider-level regression code for dynamic Antigravity `rm`
  arguments.
- Made fail-fast verification ordering explicit.

### Deferred / Parking Lot

- Nested-shell enforcement remains in `codexctl-dchq`.
- Provider-native permission policy remains the preferred future primary
  enforcement point after parity is proven.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** The production edit and both test layers touch
  `src/brain/safety.rs`, so Task 2 must begin only after Task 1 is green and
  reviewed.
