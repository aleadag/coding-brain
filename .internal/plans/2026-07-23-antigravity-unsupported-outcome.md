# Antigravity Unsupported Outcome Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Stop exact Antigravity `PostToolUse` events for unsupported permission tools from creating false orphan diagnostics while preserving lifecycle audit evidence and genuine ambiguity diagnostics.

**Architecture:** Keep the behavior change inside exact-identity correlation. Own one unsupported-tool semantic marker in the neutral Brain module, use it in permission persistence and lifecycle correlation, and return observation-only correlation only for one exact Antigravity activity whose first terminal row is the intentional abstention.

**Tech Stack:** Rust 2024 workspace, Cargo tests, Nix development shell, Beads.

## Global Constraints

- PR #24 owns `codexctl-5ah`; do not change Bash fallback interval behavior.
- Never attach an Outcome to `Denied`, `Abstained`, or `Error`.
- Keep the exception Antigravity-only, first-terminal-only, and exact-identity-only.
- Multiple distinct exact activity IDs remain diagnostic.
- Preserve `PostToolUse` lifecycle observations and raw activity history.
- Add no schema, configuration, dependency, public API, or documentation change.
- Do not commit, push, publish, or merge without separate authorization.

---

## File Structure

- `src/brain/mod.rs`: owns the shared internal semantic marker.
- `src/brain/permission_hook.rs`: persists the marker on intentional unsupported-tool abstention.
- `src/lifecycle_hook.rs`: applies the narrow exact-correlation exception and unit controls.
- `tests/hook_activity.rs`: reproduces the real provider permission-to-lifecycle binary flow with isolated paths.

### Task 1: Suppress the exact unsupported Antigravity correlation diagnostic

**Files:**
- Modify: `src/brain/mod.rs:1-22`
- Modify: `src/brain/permission_hook.rs:24-27,253-258`
- Modify: `src/lifecycle_hook.rs:20-24,432-455,613-629`
- Test: `src/lifecycle_hook.rs:763-900,1113-1451`
- Test: `tests/hook_activity.rs:76-178,994-1082`

**Interfaces:**
- Consumes: the exact `(provider, session_id, turn_id, tool_use_id)` match and first terminal Decision row.
- Produces: `crate::brain::UNSUPPORTED_PERMISSION_TOOL_REASON` and `first_terminal_with_index`.

**Acceptance Criteria:**
- A unique exact Antigravity activity whose first terminal row is `Abstained` with the shared marker yields `Correlation::None`.
- Repeated `view_file` and `grep_search` steps retain `PostToolUse` observations with no Outcome or Diagnostic.
- Different providers, different reasons, and multiple exact activity IDs remain diagnostic.
- A later marker cannot override the first terminal row.
- Existing exact allowed correlation and provider lifecycle tests pass.

- [x] **Step 1: Add isolated provider lifecycle test helpers**

In `tests/hook_activity.rs`, add:

```rust
fn run_provider_lifecycle_hook(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command.args(["--lifecycle-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}

fn unsupported_antigravity_permission_payload(
    cwd: &Path,
    step: u64,
    tool: &str,
) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(cwd, None)).unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    payload["toolCall"] = serde_json::json!({
        "name": tool,
        "args": {"AbsolutePath": "/tmp/example"}
    });
    serde_json::to_vec(&payload).unwrap()
}

fn antigravity_post_payload(cwd: &Path, step: u64) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/antigravity-post-tool-use.json"
    ))
    .unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    payload["workspacePaths"] = serde_json::json!([cwd]);
    serde_json::to_vec(&payload).unwrap()
}
```

- [x] **Step 2: Write the failing end-to-end regression**

Add:

```rust
#[test]
fn unsupported_antigravity_post_tool_use_is_observation_only() {
    let home = tempfile::tempdir().unwrap();
    for (step, tool) in [(5, "view_file"), (6, "grep_search")] {
        let permission = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &unsupported_antigravity_permission_payload(home.path(), step, tool),
        );
        assert!(permission.status.success(), "{tool}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&permission.stdout).unwrap()["decision"],
            "ask"
        );
        assert!(permission.stderr.is_empty(), "{tool}");

        let post = run_provider_lifecycle_hook(
            home.path(),
            "antigravity",
            Some("PostToolUse"),
            &antigravity_post_payload(home.path(), step),
        );
        assert!(post.status.success(), "{tool}");
        assert!(post.stdout.is_empty(), "{tool}");
        assert!(
            post.stderr.is_empty(),
            "{tool}: {}",
            String::from_utf8_lossy(&post.stderr)
        );
    }

    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PostToolUse"))
            .count(),
        2
    );
    assert_eq!(
        events.iter().filter(|event| event.state == ActivityState::Outcome).count(),
        0
    );
    assert_eq!(
        events.iter().filter(|event| event.kind == ActivityKind::Diagnostic).count(),
        0
    );
}
```

- [x] **Step 3: Verify the regression is RED**

Run:

```bash
nix develop path:.# --command cargo test --test hook_activity \
  unsupported_antigravity_post_tool_use_is_observation_only -- --exact
```

Expected: FAIL with `orphan outcome: exact lifecycle identity is ambiguous or ineligible`.

- [x] **Step 4: Add unit-level adversarial controls**

In `src/lifecycle_hook.rs`, add helpers based on `decision_event`:

```rust
fn exact_decision_event(
    cwd: &Path,
    provider: AgentProvider,
    activity_id: &str,
    recorded_at_ms: u64,
    state: ActivityState,
    reason: &str,
) -> ActivityEvent {
    let tool_use_id = match provider {
        AgentProvider::Antigravity => "step-5",
        AgentProvider::Codex => "call-1",
        AgentProvider::Claude => unreachable!(),
    };
    let mut event = decision_event(
        cwd,
        activity_id,
        recorded_at_ms,
        Some(tool_use_id),
        "cargo test",
        state,
    );
    let session = event.session.as_mut().unwrap();
    session.provider = provider;
    if provider == AgentProvider::Antigravity {
        session.session_id = "agy-conversation-1".into();
        session.turn_id = Some("step-5".into());
    }
    event.reasoning = Some(reason.into());
    event
}

fn invoke_exact_post(
    provider: AgentProvider,
    cwd: &Path,
    lifecycle: &LifecycleStore,
    activity: &ActivityStore,
) {
    match provider {
        AgentProvider::Codex => {
            invoke_activity_hook(
                lifecycle,
                activity,
                hook_payload(
                    cwd,
                    "PostToolUse",
                    "call-1",
                    "cargo test",
                    Some(serde_json::json!({"exit_code": 0})),
                ),
            );
        }
        AgentProvider::Antigravity => {
            let mut payload: Value = serde_json::from_slice(include_bytes!(
                "../tests/fixtures/hooks/antigravity-post-tool-use.json"
            ))
            .unwrap();
            payload["workspacePaths"] = serde_json::json!([cwd]);
            persist_provider_hook(
                AgentProvider::Antigravity,
                Some("PostToolUse"),
                &serde_json::to_vec(&payload).unwrap(),
                lifecycle,
                Some(activity),
                None,
            )
            .unwrap();
        }
        AgentProvider::Claude => unreachable!(),
    }
}

fn exact_correlation_counts(
    provider: AgentProvider,
    rows: &[(&str, ActivityState, &str)],
) -> (usize, usize) {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    for (index, (activity_id, state, reason)) in rows.iter().enumerate() {
        activity
            .append(exact_decision_event(
                temp.path(),
                provider,
                activity_id,
                index as u64 + 1,
                *state,
                reason,
            ))
            .unwrap();
    }
    invoke_exact_post(provider, temp.path(), &lifecycle, &activity);
    outcome_and_diagnostic_counts(&activity)
}
```

Then add the controls:

```rust
#[test]
fn unsupported_exception_is_antigravity_only() {
    assert_eq!(
        exact_correlation_counts(
            AgentProvider::Codex,
            &[(
                "activity-1",
                ActivityState::Abstained,
                UNSUPPORTED_PERMISSION_TOOL_REASON,
            )],
        ),
        (0, 1)
    );
}

#[test]
fn unsupported_exception_requires_the_exact_reason() {
    assert_eq!(
        exact_correlation_counts(
            AgentProvider::Antigravity,
            &[("activity-1", ActivityState::Abstained, "model mode is off")],
        ),
        (0, 1)
    );
}

#[test]
fn unsupported_exception_requires_one_exact_activity_id() {
    assert_eq!(
        exact_correlation_counts(
            AgentProvider::Antigravity,
            &[
                (
                    "activity-1",
                    ActivityState::Abstained,
                    UNSUPPORTED_PERMISSION_TOOL_REASON,
                ),
                (
                    "activity-2",
                    ActivityState::Abstained,
                    UNSUPPORTED_PERMISSION_TOOL_REASON,
                ),
            ],
        ),
        (0, 1)
    );
}

#[test]
fn unsupported_exception_respects_first_terminal_state() {
    assert_eq!(
        exact_correlation_counts(
            AgentProvider::Antigravity,
            &[
                ("activity-1", ActivityState::Denied, "model denied"),
                (
                    "activity-1",
                    ActivityState::Abstained,
                    UNSUPPORTED_PERMISSION_TOOL_REASON,
                ),
            ],
        ),
        (0, 1)
    );
    assert_eq!(
        exact_correlation_counts(
            AgentProvider::Antigravity,
            &[
                (
                    "activity-1",
                    ActivityState::Abstained,
                    UNSUPPORTED_PERMISSION_TOOL_REASON,
                ),
                ("activity-1", ActivityState::Denied, "model denied"),
            ],
        ),
        (0, 0)
    );
}
```

Run:

```bash
nix develop path:.# --command cargo test --lib \
  lifecycle_hook::tests::unsupported_exception
```

Expected before implementation: all existing diagnostic characterizations pass;
the first-marker observation-only case fails.

- [x] **Step 5: Add the shared marker in the neutral module**

In `src/brain/mod.rs`, add:

```rust
pub(crate) const UNSUPPORTED_PERMISSION_TOOL_REASON: &str =
    "unsupported permission tool";
```

In `src/brain/permission_hook.rs`, import and use it:

```rust
use super::UNSUPPORTED_PERMISSION_TOOL_REASON;
```

```rust
if !supported {
    return HookEvaluation::Abstain {
        brain: None,
        reason: UNSUPPORTED_PERMISSION_TOOL_REASON.into(),
        terminal_state: ActivityState::Abstained,
    };
}
```

- [x] **Step 6: Implement the narrow exact-match exception**

In `src/lifecycle_hook.rs`, import:

```rust
use crate::brain::UNSUPPORTED_PERMISSION_TOOL_REASON;
```

Split first-terminal lookup without changing allowed eligibility:

```rust
fn first_terminal_with_index<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<(usize, &'a ActivityEvent)> {
    log.events()
        .iter()
        .enumerate()
        .find(|(_, event)| event.activity_id == activity_id && event.state.is_terminal())
}

fn first_allowed_terminal_with_index<'a>(
    log: &'a ActivityLog,
    activity_id: &str,
) -> Option<(usize, &'a ActivityEvent)> {
    first_terminal_with_index(log, activity_id)
        .filter(|(_, event)| event.state == ActivityState::Allowed && event.decision_id.is_some())
}
```

After confirming `exact_activity_ids.len() == 1`, before
`correlate_candidates`, add:

```rust
let exact_activity_id = &exact_activity_ids[0];
if identity.provider() == AgentProvider::Antigravity
    && first_terminal_with_index(log, exact_activity_id).is_some_and(|(_, event)| {
        event.state == ActivityState::Abstained
            && event.reasoning.as_deref() == Some(UNSUPPORTED_PERMISSION_TOOL_REASON)
    })
{
    return Correlation::None;
}
```

Do not change the multiple-ID diagnostic or Bash fallback.

- [x] **Step 7: Run focused tests and verify GREEN**

Run:

```bash
nix develop path:.# --command cargo test --test hook_activity \
  unsupported_antigravity_post_tool_use_is_observation_only -- --exact
nix develop path:.# --command cargo test --lib \
  lifecycle_hook::tests::unsupported_exception
nix develop path:.# --command cargo test --lib lifecycle_hook::tests
```

Expected: all focused tests PASS.

- [x] **Step 8: Run provider integration coverage**

Run:

```bash
nix develop path:.# --command cargo test --test hook_activity
nix develop path:.# --command cargo test --test lifecycle_hook_cli
```

Expected: PASS, excluding explicitly ignored timing tests. If the known
fake-`curl` fixture flakes, rerun that exact test and report it separately; do
not weaken or skip the gate.

- [x] **Step 9: Run workspace quality gates**

Run:

```bash
nix develop path:.# --command cargo fmt --check
nix develop path:.# --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:.# --command cargo test --workspace --all-targets
nix develop path:.# --command cargo build --workspace
git diff --check
git status --short
```

Expected: all gates PASS. Status lists only approved implementation/spec/plan
changes. No migration or state rewrite occurs, so rollback is a code revert.
Do not commit or push.

## Stress Test Results: Antigravity Unsupported Outcome Plan

### Resolved Decisions

- Own the shared marker in `src/brain/mod.rs` to avoid bidirectional module coupling.
- Prove the positive behavior through real permission and lifecycle binaries.
- Lock first-terminal ordering with both later-marker and later-conflict controls.
- Isolate HOME, XDG paths, and PATH without installing a fake model.
- Treat the existing fake-`curl` race as a separately reported test flake, never as permission to skip verification.

### Changes Made

- Added `src/brain/mod.rs` and `tests/hook_activity.rs` to the file map.
- Replaced the manually constructed positive test with an end-to-end provider sequence.
- Expanded adversarial ordering controls and verification guidance.

### Deferred / Parking Lot

- PR #24 fallback behavior.
- Generalization to providers without confirmed evidence.

### Confidence Assessment

- Overall: High.
- Remaining concern: a prose marker remains an internal semantic contract; neutral ownership and explicit controls make drift visible.
