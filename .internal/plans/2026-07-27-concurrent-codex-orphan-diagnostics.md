# Concurrent Codex Orphan Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Suppress the confirmed false Codex orphan diagnostic for one mismatched concurrent Pre/Post lifecycle batch without attaching an outcome or weakening diagnostics for attributable Decision evidence.

**Architecture:** Keep exact Decision and exact PreToolUse correlation unchanged. In the zero-exact-anchor Codex Bash fallback only, identify an open same-identity lifecycle batch after the latest PostToolUse and return observation-only correlation when that batch contains a different-ID PreToolUse but no matching-identity Decision.

**Tech Stack:** Rust, existing `ActivityLog`/`ActivityEvent` lifecycle audit model, existing unit-test helpers in `src/lifecycle_hook.rs`.

## Global Constraints

- This is a bounded mitigation for the upstream Codex opaque-ID mismatch tracked by `codexctl-prbe`; it does not synthesize or claim to repair provider identity.
- Exact Decision identity remains the first correlation path.
- Multiple exact PreToolUse anchors remain diagnostic.
- Only provider/session/provider-session/turn-matching Decisions block suppression; physically interleaved foreign Decisions do not.
- The first mismatched PostToolUse closes the open batch, so one stale PreToolUse cannot suppress multiple later Posts.
- Do not reuse the hashed PermissionRequest `request_key`, correlate by command/order, persist command-derived hashes, or attach an Outcome.
- Do not change schemas, hook responses, permission evaluation, lifecycle projection, or public documentation.
- Do not commit, push, or publish without explicit user authorization under the repository's conservative profile.

---

### Task 1: Classify one no-Decision mismatched Codex lifecycle batch as observation-only

**Files:**
- Modify: `src/lifecycle_hook.rs:537`
- Test: `src/lifecycle_hook.rs:2244`

**Interfaces:**
- Consumes: `matches_lifecycle_identity(&SessionTarget, &LifecycleIdentity) -> bool`, `ActivityLog::events() -> &[ActivityEvent]`, and the existing `Correlation` enum.
- Produces: `open_codex_pre_batch<'a>(&'a ActivityLog, &LifecycleIdentity, &str) -> Option<&'a [ActivityEvent]>`, used only by `correlate_outcome`.

**Acceptance Criteria:**
- A prior unrelated Decision followed by a different-ID Codex PreToolUse/PostToolUse pair with no matching-identity Decision in the open batch appends no Outcome and no Diagnostic.
- A standalone missing-anchor PostToolUse, an in-batch matching-identity Decision, multiple exact anchors, a second unmatched Post without fresh Pre evidence, and a non-Codex mismatch remain diagnostic.
- A foreign-session Decision interleaved in the physical activity interval does not create a false diagnostic.
- Existing exact-ID correlation, sequential ID-less PermissionRequest fallback, and matched parallel empty-interval behavior remain unchanged.

- [ ] **Step 1: Write the failing runtime regression**

Add this test beside `post_tool_use_without_decision_activity_is_ignored` in
`src/lifecycle_hook.rs`:

```rust
#[test]
fn concurrent_codex_id_mismatch_without_in_batch_decision_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    activity
        .append(decision_event(
            temp.path(),
            "prior-unrelated",
            1,
            None,
            "cargo check",
            ActivityState::Denied,
        ))
        .unwrap();
    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PreToolUse",
            "exec-pre",
            "cargo test",
            None,
        ),
    );

    let stderr = invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "exec-post",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );

    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 0));
    assert_eq!(
        activity
            .read()
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.kind == ActivityKind::Lifecycle)
            .map(|event| event.tool.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["PreToolUse", "PostToolUse"]
    );
}
```

- [ ] **Step 2: Run the regression and verify RED**

Run:

```bash
nix develop path:. --command cargo test --bin coding-brain \
  lifecycle_hook::tests::concurrent_codex_id_mismatch_without_in_batch_decision_is_ignored \
  -- --exact
```

Expected: FAIL because stderr contains
`orphan outcome: PreToolUse anchor is missing or ambiguous` and the diagnostic
count is `1`.

- [ ] **Step 3: Implement the minimal open-batch classifier**

Add this helper immediately after `correlate_outcome`:

```rust
fn open_codex_pre_batch<'a>(
    log: &'a ActivityLog,
    identity: &coding_brain_core::lifecycle::LifecycleIdentity,
    tool_use_id: &str,
) -> Option<&'a [ActivityEvent]> {
    if identity.provider() != AgentProvider::Codex {
        return None;
    }
    let open_start = log
        .events()
        .iter()
        .rposition(|event| {
            event.kind == ActivityKind::Lifecycle
                && event.tool.as_deref() == Some("PostToolUse")
                && event
                    .session
                    .as_ref()
                    .is_some_and(|session| matches_lifecycle_identity(session, identity))
        })
        .map_or(0, |index| index + 1);
    let open_batch = &log.events()[open_start..];
    let first_mismatched_pre = open_batch.iter().position(|event| {
        event.kind == ActivityKind::Lifecycle
            && event.tool.as_deref() == Some("PreToolUse")
            && event.session.as_ref().is_some_and(|session| {
                matches_lifecycle_identity(session, identity)
                    && session
                        .tool_use_id
                        .as_deref()
                        .is_some_and(|pre_id| pre_id != tool_use_id)
            })
    })?;
    Some(&open_batch[first_mismatched_pre..])
}
```

Replace the existing `anchors.len() != 1` branch with:

```rust
if anchors.is_empty()
    && let Some(open_batch) = open_codex_pre_batch(log, identity, &tool_use_id)
    && !open_batch.iter().any(|event| {
        event.kind == ActivityKind::Decision
            && event
                .session
                .as_ref()
                .is_some_and(|session| matches_lifecycle_identity(session, identity))
    })
{
    return Correlation::None;
}
if anchors.len() != 1 {
    return diagnostic_correlation(
        lifecycle,
        input,
        "orphan outcome: PreToolUse anchor is missing or ambiguous",
    );
}
```

Do not change the exact Decision path, the matched-anchor interval path, or
`correlate_candidates`.

- [ ] **Step 4: Run the regression and verify GREEN**

Run:

```bash
nix develop path:. --command cargo test --bin coding-brain \
  lifecycle_hook::tests::concurrent_codex_id_mismatch_without_in_batch_decision_is_ignored \
  -- --exact
```

Expected: PASS with zero Diagnostics and zero Outcomes.

- [ ] **Step 5: Add adversarial boundary tests**

Add these tests beside the RED regression:

```rust
#[test]
fn concurrent_codex_id_mismatch_with_in_batch_decision_is_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PreToolUse",
            "exec-pre",
            "cargo test",
            None,
        ),
    );
    activity
        .append(decision_event(
            temp.path(),
            "in-batch",
            2,
            None,
            "cargo check",
            ActivityState::Observed,
        ))
        .unwrap();

    let stderr = invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "exec-post",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );

    assert!(stderr.contains("orphan outcome"), "{stderr}");
    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
}

#[test]
fn foreign_decision_inside_concurrent_codex_batch_is_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    activity
        .append(decision_event(
            temp.path(),
            "prior-unrelated",
            1,
            None,
            "cargo check",
            ActivityState::Denied,
        ))
        .unwrap();
    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PreToolUse",
            "exec-pre",
            "cargo test",
            None,
        ),
    );
    let mut foreign = decision_event(
        temp.path(),
        "foreign",
        3,
        None,
        "cargo check",
        ActivityState::Denied,
    );
    foreign.session.as_mut().unwrap().session_id = "foreign-session".into();
    activity.append(foreign).unwrap();

    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "exec-post",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );

    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 0));
}

#[test]
fn mismatched_codex_post_closes_the_open_batch() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    activity
        .append(decision_event(
            temp.path(),
            "prior-unrelated",
            1,
            None,
            "cargo check",
            ActivityState::Denied,
        ))
        .unwrap();
    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PreToolUse",
            "exec-pre",
            "cargo test",
            None,
        ),
    );
    let first_stderr = invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "exec-post-a",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );
    let second_lifecycle = LifecycleStore::at(temp.path().join("second-lifecycle"));
    let second_stderr = invoke_activity_hook(
        &second_lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "exec-post-b",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );

    assert!(first_stderr.is_empty(), "{first_stderr}");
    assert!(second_stderr.contains("orphan outcome"), "{second_stderr}");
    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
}

#[test]
fn duplicate_exact_pre_anchors_remain_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(temp.path(), "PreToolUse", "call-1", "cargo test", None),
    );
    let mut duplicate = activity.read().unwrap().events()[0].clone();
    duplicate.activity_id = "duplicate-pre".into();
    duplicate.recorded_at_ms += 1;
    activity.append(duplicate).unwrap();

    invoke_activity_hook(
        &lifecycle,
        &activity,
        hook_payload(
            temp.path(),
            "PostToolUse",
            "call-1",
            "cargo test",
            Some(serde_json::json!({"exit_code": 0})),
        ),
    );

    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
}

#[test]
fn non_codex_id_mismatch_remains_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    let payload = |event: &str, call: &str, response: Option<Value>| {
        let mut value = serde_json::json!({
            "session_id": "claude-session",
            "turn_id": "claude-turn",
            "cwd": temp.path(),
            "hook_event_name": event,
            "tool_name": "Bash",
            "tool_use_id": call,
            "tool_input": {"command": "cargo test"}
        });
        if let Some(response) = response {
            value["tool_response"] = response;
        }
        serde_json::to_vec(&value).unwrap()
    };
    persist_provider_hook(
        AgentProvider::Claude,
        None,
        &payload("PreToolUse", "exec-pre", None),
        &lifecycle,
        Some(&activity),
        None,
    )
    .unwrap();
    persist_provider_hook(
        AgentProvider::Claude,
        None,
        &payload(
            "PostToolUse",
            "exec-post",
            Some(serde_json::json!({"exit_code": 0})),
        ),
        &lifecycle,
        Some(&activity),
        None,
    )
    .unwrap();

    assert_eq!(outcome_and_diagnostic_counts(&activity), (0, 1));
}
```

- [ ] **Step 6: Run the focused lifecycle correlation suite**

Run:

```bash
nix develop path:. --command cargo test --bin coding-brain lifecycle_hook::tests::
```

Expected: all lifecycle-hook unit tests pass, including the new mismatch and
boundary tests plus the existing exact-ID, sequential fallback, and parallel
empty-interval tests.

- [ ] **Step 7: Format and run repository quality gates**

Run:

```bash
nix develop path:. --command cargo fmt --all
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets --all-features -- -D warnings
nix develop path:. --command cargo build --workspace
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
```

Expected: every command exits `0`; tests report no failures; Clippy reports no
warnings; normalized whitespace validation reports no errors.

- [ ] **Step 8: Review scope and hand off without publishing**

Run:

```bash
git status --short
git diff -- src/lifecycle_hook.rs
```

Expected: production and regression changes are limited to
`src/lifecycle_hook.rs`; the approved spec and this plan remain the only
workflow documents added. Update and close the implementation Bead only after
all gates pass. Do not commit, push, or publish without explicit user
authorization.

## Stress Test Results: Concurrent Codex Orphan Diagnostics Plan

### Resolved Decisions

- Keep the prior same-identity Decision before the mismatched Pre/Post in the
  RED test so the current failure is specifically the missing-anchor branch.
- Use append order and the latest same-identity Post as the open-batch boundary;
  do not use timestamps, commands, or global adjacency.
- Exercise an incomplete `Observed` in-batch Decision rather than only terminal
  states.
- Run every Cargo command through `nix develop path:. --command`; the worktree's
  `.envrc` is blocked, while the Nix command and exact test filter were verified.
- Stop and return to design if implementation needs provider parsing, schema,
  permission, request-key, or multi-file runtime changes.
- Use a fresh lifecycle-store fixture for the second unmatched Post control so
  the lifecycle replay guard cannot prevent the correlation branch from
  running.

### Changes Made

- Hardened the in-batch Decision test to assert an orphan diagnostic for
  `ActivityState::Observed`.
- Replaced all blocked `direnv exec` commands with the verified Nix development
  environment.
- Corrected the closed-batch regression to isolate outcome correlation from the
  lifecycle store's duplicate-callback guard, and added the required formatting
  write before the format check.

### Deferred / Parking Lot

- Upstream Codex identity repair remains tracked by `codexctl-prbe`.
- Commit, push, and publication remain subject to separate user authorization.

### Confidence Assessment

- Overall: High.
- Remaining concern: the implementation deliberately mitigates one confirmed
  Post per open batch and does not synthesize provider identity.
