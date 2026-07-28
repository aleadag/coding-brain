# Antigravity PostInvocation Continuation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Allow legitimate Antigravity background-task continuation after `PostInvocation` while preserving bounded invocation authority and fail-safe revocation at real `Stop`.

**Architecture:** Parse `PostInvocation` as a valid provider callback with no lifecycle event, so it cannot mutate persisted lifecycle or activity state. Preserve the open `invocation-N` trajectory until trusted Antigravity `Stop(fullyIdle=true, execution-E)` crosses the provider's terminal namespace only to revoke that trajectory; keep all step-floor, replay, capacity, provider/session, and post-stop guards unchanged.

**Tech Stack:** Rust 2024, Cargo workspace tests, serde/serde_json, Coding Brain lifecycle projection/store, integration-test subprocesses.

## Global Constraints

- `PostInvocation` validates all current required fields but causes no lifecycle, activity, session-link, or diagnostic transition.
- Only trusted Antigravity `Stop(fullyIdle=true, execution-E)` may cross an open `invocation-N` namespace mismatch, and only to revoke authority.
- Do not change persisted lifecycle schema.
- Do not parse transcript system-message fields as authorization evidence.
- Do not add timeouts, grace periods, polling, retries, configuration, or a feature flag.
- Preserve all below-floor, replay, permission-reversal, capacity, cross-session, unrelated-turn, no-`PreInvocation`, and post-stop rejection.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Correlate real Antigravity Stop as revocation-only

**Files:**

- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs:192`
- Test: `crates/coding-brain-core/src/lifecycle/projection.rs:2715`

**Interfaces:**

- Consumes: `AgentProvider`, `LifecycleEventKind`, `SessionLifecycleState`, and the existing `prefixed_index` helper.
- Produces: private predicate
  `is_antigravity_execution_stop(&SessionLifecycleState, &LifecycleEvent, &str) -> bool`.
- Produces: projection behavior that accepts `Stop(execution-E)` only as a close of an open Antigravity `invocation-N`.

**Acceptance Criteria:**

- Trusted Antigravity `Stop(execution-E)` closes the open `invocation-N`, clears its initial-step floor and replay ledger, and remembers both identities as recent.
- A child step after real Stop is rejected.
- A replayed closed invocation remains rejected after another invocation opens.
- Mismatched Stop for other providers and non-Stop Antigravity mismatches remain rejected.
- Existing Antigravity child correlation, replay, and capacity tests remain passing.

- [ ] **Step 1: Write the failing projection test**

Add beside `antigravity_steps_are_children_of_the_open_invocation`:

```rust
#[test]
fn antigravity_execution_stop_revokes_open_invocation() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(
        snapshot.apply(invocation("invocation-1", 5), 1),
        ApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply(
            LifecycleEvent::permission(
                antigravity_identity("step-5"),
                PermissionDisposition::Decided,
            )
            .unwrap(),
            2,
        ),
        ApplyOutcome::Applied
    );

    let stop = LifecycleEvent::from_parts(
        antigravity_identity("execution-1"),
        LifecycleEventKind::Stop,
    )
    .unwrap();
    assert_eq!(snapshot.apply(stop, 3), ApplyOutcome::Applied);

    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    let state = &snapshot.sessions[&key];
    assert!(!state.turn_open);
    assert_eq!(state.current_turn.as_deref(), Some("invocation-1"));
    assert_eq!(state.antigravity_initial_step, None);
    assert!(state.antigravity_child_events.is_empty());
    assert!(state.recent_turns.iter().any(|turn| turn == "invocation-1"));
    assert!(state.recent_turns.iter().any(|turn| turn == "execution-1"));

    let after_stop = LifecycleEvent::permission(
        antigravity_identity("step-6"),
        PermissionDisposition::Decided,
    )
    .unwrap();
    assert_eq!(
        snapshot.apply(after_stop, 4),
        ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
    );

    assert_eq!(
        snapshot.apply(invocation("invocation-2", 7), 5),
        ApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply(invocation("invocation-1", 5), 6),
        ApplyOutcome::Ignored(IgnoreReason::RecentTurn)
    );
}
```

- [ ] **Step 2: Run the focused test and verify the current mismatch fails**

Run:

```bash
direnv exec . cargo test -p coding-brain-core \
  lifecycle::projection::tests::antigravity_execution_stop_revokes_open_invocation -- --exact
```

Expected: FAIL because `Stop(execution-1)` is currently
`Ignored(AmbiguousTurn)`.

- [ ] **Step 3: Add the revocation-only predicate**

Add after `is_antigravity_child_candidate`:

```rust
fn is_antigravity_execution_stop(
    state: &SessionLifecycleState,
    event: &LifecycleEvent,
    turn_id: &str,
) -> bool {
    event.identity().provider() == AgentProvider::Antigravity
        && matches!(event.kind(), LifecycleEventKind::Stop)
        && prefixed_index(turn_id, "execution-").is_some()
        && state.turn_open
        && state
            .current_turn
            .as_deref()
            .and_then(|turn| prefixed_index(turn, "invocation-"))
            .is_some()
}
```

- [ ] **Step 4: Narrowly bypass the mismatched-turn rejection**

In the `Some(current) if state.turn_open && current != turn_id` projection
branch, distinguish terminal revocation from prompt supersession:

```rust
Some(current) if state.turn_open && current != turn_id => {
    if is_antigravity_execution_stop(state, &event, turn_id) {
        let current = current.to_owned();
        state.remember_turn(&current);
    } else {
        if !matches!(event.kind(), LifecycleEventKind::UserPromptSubmit) {
            return state.ignore(IgnoreReason::AmbiguousTurn);
        }
        let current = current.to_owned();
        state.remember_turn(&current);
        state.permission_request_events.clear();
        state.current_turn = Some(turn_id.to_owned());
    }
}
```

Do not alter the existing `LifecycleEventKind::Stop` transition. It will close
the turn, clear Antigravity authority, remember `execution-E`, and leave
`current_turn` pointing at the revoked `invocation-N`.

- [ ] **Step 5: Run focused and neighboring projection tests**

Run:

```bash
direnv exec . cargo test -p coding-brain-core \
  lifecycle::projection::tests::antigravity_execution_stop_revokes_open_invocation -- --exact
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::antigravity_
direnv exec . cargo test -p coding-brain-core \
  lifecycle::projection::tests::only_user_prompt_can_supersede_an_open_turn -- --exact
```

Expected: all PASS.

- [ ] **Step 6: Inspect the task diff**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-core/src/lifecycle/projection.rs
```

Expected: no whitespace errors; every change is projection test or the
revocation-only exception.

### Task 2: Make PostInvocation a validated no-transition callback

**Files:**

- Modify: `src/provider_hooks/mod.rs:37`
- Modify: `src/provider_hooks/antigravity.rs:190`
- Modify: `src/provider_hooks/claude.rs:116`
- Modify: `src/provider_hooks/codex.rs:98`
- Modify: `src/lifecycle_hook.rs:197`
- Modify: `src/lifecycle_hook.rs:306`
- Test: `src/provider_hooks/antigravity.rs:365`
- Test: `tests/lifecycle_hook_cli.rs:294`

**Interfaces:**

- Consumes: existing `ParsedLifecycleHook`, provider parsers, and lifecycle CLI runner.
- Produces: `ParsedLifecycleHook.event: Option<LifecycleEventKind>`, where
  `None` means “valid callback with no lifecycle transition.”
- Produces: CLI runner behavior that returns silently before constructing
  activity input, lifecycle events, state writes, or session links.
- Preserves: `persist_provider_hook(...) -> Result<RecordedProviderHook, String>`
  as a persistence-only API that requires `Some(event)`.

**Acceptance Criteria:**

- Valid Antigravity `PostInvocation` returns `event == None`.
- Missing or invalid current `PostInvocation` fields remain parser errors.
- Running the lifecycle CLI for `PostInvocation` leaves lifecycle and activity bytes unchanged and emits no diagnostic.
- A later in-range Antigravity child applies under the still-open invocation.
- Real `Stop(execution-E)` then closes the invocation.
- Post-stop children remain rejected.
- Codex, Claude, and other Antigravity callbacks retain their existing events.

- [ ] **Step 1: Rewrite the CLI regression to express the complete provider sequence**

Replace the current assertion that `PostInvocation` closes `invocation-3` in
`antigravity_trusted_cli_events_record_provider_qualified_lifecycle` with:

```rust
let state_root = invocation_home.path().join(".local/state/coding-brain");
let lifecycle = LifecycleStore::at(&state_root);
let before_post = lifecycle.read().unwrap().snapshot.unwrap();
let activity_path = state_root.join("activity.jsonl");
let activity_before_post = fs::read(&activity_path).unwrap();
let links_path = state_root.join("session-links.jsonl");
let links_before_post = fs::read(&links_path).ok();

let post_invocation = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("PostInvocation"),
    &serde_json::to_vec(&invocation).unwrap(),
);
assert!(post_invocation.status.success());
assert!(post_invocation.stderr.is_empty());
assert_eq!(
    lifecycle.read().unwrap().snapshot.unwrap(),
    before_post,
    "PostInvocation changed lifecycle state"
);
assert_eq!(
    fs::read(&activity_path).unwrap(),
    activity_before_post,
    "PostInvocation appended activity"
);
assert_eq!(
    fs::read(&links_path).ok(),
    links_before_post,
    "PostInvocation appended a session link"
);

let mut continued_payload: serde_json::Value =
    serde_json::from_slice(ANTIGRAVITY_PRE_TOOL_USE).unwrap();
continued_payload["stepIdx"] = serde_json::json!(10);
let continued_output = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("PreToolUse"),
    &serde_json::to_vec(&continued_payload).unwrap(),
);
assert!(continued_output.status.success());
assert!(continued_output.stderr.is_empty());

let stop = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("Stop"),
    ANTIGRAVITY_STOP,
);
assert!(stop.status.success());
assert!(stop.stderr.is_empty());
let snapshot = lifecycle.read().unwrap().snapshot.unwrap();
let state = &snapshot.sessions[&key];
assert_eq!(state.latest_event, Some(LifecycleEventName::Stop));
assert_eq!(state.current_turn.as_deref(), Some("invocation-3"));
assert!(!state.turn_open);

continued_payload["stepIdx"] = serde_json::json!(11);
let after_stop = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("PreToolUse"),
    &serde_json::to_vec(&continued_payload).unwrap(),
);
assert!(after_stop.status.success());
assert!(
    String::from_utf8_lossy(&after_stop.stderr).contains("AmbiguousTurn")
);
```

Keep the existing missing-field matrix, including `PostInvocation`
`invocationNum` and `initialNumSteps`.

- [ ] **Step 2: Run the CLI regression and verify it fails at PostInvocation**

Run:

```bash
direnv exec . cargo test --test lifecycle_hook_cli \
  antigravity_trusted_cli_events_record_provider_qualified_lifecycle -- --exact
```

Expected: FAIL because current `PostInvocation` changes the snapshot to closed
`Stop` state.

- [ ] **Step 3: Make parsed lifecycle events optional**

Change the provider boundary:

```rust
pub(crate) struct ParsedLifecycleHook {
    pub identity: LifecycleIdentity,
    pub event: Option<LifecycleEventKind>,
    pub turn_initial_step: Option<u64>,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<ActivityOutcome>,
    pub live_process: Option<LiveProcessIdentity>,
}
```

Wrap existing Codex and Claude results:

```rust
event: Some(event),
```

and:

```rust
event: Some(lifecycle.kind().clone()),
```

- [ ] **Step 4: Parse PostInvocation as `None` without weakening validation**

In the Antigravity match, make the tuple event optional. Every existing
transition becomes `Some(LifecycleEventKind::...)`; `PostInvocation` becomes:

```rust
TrustedAntigravityEvent::PostInvocation => {
    let invocation = input
        .invocation_num
        .ok_or(HookInputError::Missing("invocationNum"))?;
    input
        .initial_num_steps
        .ok_or(HookInputError::Missing("initialNumSteps"))?;
    (
        None,
        format!("invocation-{invocation}"),
        None,
        None,
        None,
        None,
    )
}
```

Update parser assertions:

```rust
assert_eq!(pre.event, Some(LifecycleEventKind::UserPromptSubmit));
assert_eq!(post.event, None);
assert_eq!(post.identity.turn_id(), Some("invocation-3"));
```

- [ ] **Step 5: Make the CLI runner return silently on `None`**

After attaching `live_process`, take the event before building activity input:

```rust
parsed.live_process = live_process;
let Some(event_kind) = parsed.event.clone() else {
    return;
};
let activity_input = LifecycleActivityInput::from_parsed(&parsed, &input);
let event = match LifecycleEvent::from_parts_with_turn_initial_step(
    parsed.identity.clone(),
    event_kind,
    parsed.turn_initial_step,
) {
    // existing handling
};
```

This early return must precede lifecycle construction, state persistence,
session-link persistence, and activity append.

- [ ] **Step 6: Keep the persistence-only helper explicit**

In `persist_provider_hook_with_live_process`, reject an absent event because
that API promises a `RecordedProviderHook`:

```rust
parsed.live_process = live_process;
let event_kind = parsed
    .event
    .clone()
    .ok_or_else(|| "provider hook has no lifecycle transition".to_owned())?;
let activity_input = LifecycleActivityInput::from_parsed(&parsed, input);
let event = LifecycleEvent::from_parts_with_turn_initial_step(
    parsed.identity.clone(),
    event_kind,
    parsed.turn_initial_step,
)
.map_err(|error| error.to_string())?;
```

No production caller passes `PostInvocation` to this persistence-only helper;
the installed callback uses the lifecycle CLI runner.

- [ ] **Step 7: Run parser, CLI, and recovery-adjacent tests**

Run:

```bash
direnv exec . cargo test provider_hooks::antigravity::tests
direnv exec . cargo test --test lifecycle_hook_cli \
  antigravity_trusted_cli_events_record_provider_qualified_lifecycle -- --exact
direnv exec . cargo test lifecycle_hook::tests::strict_stop_persistence
direnv exec . cargo test brain::recovery::tests
```

Expected: all PASS; the CLI test proves `PostInvocation` is byte-for-byte inert.

- [ ] **Step 8: Inspect the task diff**

Run:

```bash
git diff --check
git diff -- src/provider_hooks/mod.rs src/provider_hooks/antigravity.rs \
  src/provider_hooks/claude.rs src/provider_hooks/codex.rs \
  src/lifecycle_hook.rs tests/lifecycle_hook_cli.rs
```

Expected: no persisted schema changes and no provider-string special case in
the lifecycle runner.

### Task 3: Prove executable permission continuation and all quality gates

**Files:**

- Modify: `tests/hook_activity.rs:210`
- Test: `tests/hook_activity.rs:1472`
- Verify only: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Verify only: workspace Cargo targets

**Interfaces:**

- Consumes: the no-transition lifecycle CLI path from Task 2 and revocation-only Stop projection from Task 1.
- Produces: end-to-end regression
  `PreInvocation -> allowed step -> PostInvocation -> later allowed step -> Stop -> rejected step`.

**Acceptance Criteria:**

- Two distinct in-range permissions, one before and one after `PostInvocation`, persist and return Antigravity `{"decision":"allow"}`.
- `PostInvocation` changes neither lifecycle snapshot nor activity log.
- Real `Stop(execution-E)` closes the invocation.
- A model-approved permission after real Stop returns fail-safe `ask`, emits `AmbiguousTurn`, and records no effective `Allowed` or `Delivered`.
- Exactly one error activity represents the rejected post-stop permission; no repeated Needs Attention row is created.
- Workspace test, all-target Clippy, formatting, and build gates pass.

- [ ] **Step 1: Add exact Antigravity payload helpers**

Add near `run_provider_lifecycle_hook`:

```rust
fn antigravity_invocation_payload(home: &Path, invocation: u64, initial_step: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "invocationNum": invocation,
        "initialNumSteps": initial_step,
        "conversationId": "agy-conversation-1",
        "workspacePaths": [home],
        "transcriptPath": "/tmp/agy-conversation-1/transcript.jsonl",
        "artifactDirectoryPath": "/tmp/agy-conversation-1/artifacts"
    }))
    .unwrap()
}

fn antigravity_permission_payload_for_step(home: &Path, step: u64) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(&antigravity_permission_payload(home, None)).unwrap();
    payload["stepIdx"] = serde_json::json!(step);
    serde_json::to_vec(&payload).unwrap()
}

fn antigravity_stop_payload(home: &Path, execution: u64) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/antigravity-stop.json"
    ))
    .unwrap();
    payload["executionNum"] = serde_json::json!(execution);
    payload["workspacePaths"] = serde_json::json!([home]);
    serde_json::to_vec(&payload).unwrap()
}
```

- [ ] **Step 2: Add the executable continuation regression**

Add after `antigravity_open_invocation_allows_in_range_step`:

```rust
#[test]
fn antigravity_post_invocation_preserves_bounded_permission_authority_until_stop() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    let invocation = antigravity_invocation_payload(home.path(), 14, 70);

    let pre = run_provider_lifecycle_hook(
        home.path(),
        "antigravity",
        Some("PreInvocation"),
        &invocation,
    );
    assert!(pre.status.success());
    assert!(pre.stderr.is_empty());

    for step in [70, 72] {
        if step == 72 {
            let lifecycle =
                LifecycleStore::at(home.path().join(".local/state/coding-brain"));
            let before_snapshot = lifecycle.read().unwrap().snapshot.unwrap();
            let before_activity = activity(home.path()).read().unwrap().events().to_vec();
            let post = run_provider_lifecycle_hook(
                home.path(),
                "antigravity",
                Some("PostInvocation"),
                &invocation,
            );
            assert!(post.status.success());
            assert!(post.stderr.is_empty());
            assert_eq!(
                lifecycle.read().unwrap().snapshot.unwrap(),
                before_snapshot
            );
            assert_eq!(
                activity(home.path()).read().unwrap().events(),
                before_activity
            );
        }

        let output = run_provider_permission_hook(
            home.path(),
            "antigravity",
            Some("PreToolUse"),
            &antigravity_permission_payload_for_step(home.path(), step),
        );
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::json!({"decision": "allow"})
        );
    }

    let before_stop_rejection = activity(home.path()).read().unwrap().events().to_vec();
    let stop = run_provider_lifecycle_hook(
        home.path(),
        "antigravity",
        Some("Stop"),
        &antigravity_stop_payload(home.path(), 3),
    );
    assert!(stop.status.success());
    assert!(stop.stderr.is_empty());

    let after_stop = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload_for_step(home.path(), 74),
    );
    assert!(after_stop.status.success());
    assert!(String::from_utf8_lossy(&after_stop.stderr).contains("AmbiguousTurn"));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&after_stop.stdout).unwrap(),
        serde_json::json!({
            "decision": "ask",
            "reason": "Coding Brain abstained"
        })
    );

    let all_events = activity(home.path()).read().unwrap().events().to_vec();
    let new_events = &all_events[before_stop_rejection.len()..];
    assert_eq!(
        new_events
            .iter()
            .filter(|event| event.state == ActivityState::Error)
            .count(),
        1
    );
    assert!(
        new_events.iter().all(|event| {
            !matches!(
                event.state,
                ActivityState::Allowed | ActivityState::Delivered
            )
        })
    );
}
```

If the Stop lifecycle observation is included in `new_events`, keep the
state-based assertions above; they deliberately ignore benign observation
rows and assert only the security-relevant terminal states.

- [ ] **Step 3: Run the new integration test**

Run:

```bash
direnv exec . cargo test --test hook_activity \
  antigravity_post_invocation_preserves_bounded_permission_authority_until_stop -- --exact
```

Expected: PASS. Before Tasks 1 and 2 it would fail at the inert snapshot or the
continued permission.

- [ ] **Step 4: Run focused fail-safe suites**

Run:

```bash
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::antigravity_
direnv exec . cargo test --test lifecycle_hook_cli antigravity_
direnv exec . cargo test --test hook_activity antigravity_
direnv exec . cargo test --test hook_activity model_allow_requires_applied_lifecycle_decision -- --exact
```

Expected: all PASS, including existing below-floor, replay, reversal, capacity,
no-open-invocation, and post-stop guards.

- [ ] **Step 5: Format and run the complete repository gates**

Run:

```bash
direnv exec . cargo fmt
direnv exec . cargo test --workspace
direnv exec . cargo clippy --workspace --all-targets -- -D warnings
direnv exec . cargo fmt --check
direnv exec . cargo build --workspace
```

Expected: all commands exit 0 with no Clippy warnings or formatting changes
remaining.

- [ ] **Step 6: Verify scope and working tree**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff -- crates/coding-brain-core/src/lifecycle/projection.rs \
  src/provider_hooks/mod.rs src/provider_hooks/antigravity.rs \
  src/provider_hooks/claude.rs src/provider_hooks/codex.rs \
  src/lifecycle_hook.rs tests/lifecycle_hook_cli.rs tests/hook_activity.rs
```

Expected: only files named by this plan changed, plus the approved internal
research/spec/plan artifacts; no persisted schema, configuration, or
user-facing documentation changes.

- [ ] **Step 7: Update Beads completion evidence**

After all gates pass:

```bash
bd -C /home/alexander/.beads-planning note codexctl-dfbn \
  "Implemented validation-only PostInvocation and revocation-only execution Stop correlation. Verified focused continuation/fail-safe tests plus workspace test, all-target Clippy, fmt, and build gates."
bd -C /home/alexander/.beads-planning close codexctl-dfbn \
  --reason "Fixed and verified Antigravity post-invocation continuation."
```

Expected: `codexctl-dfbn` is closed only after fresh verification evidence.

- [ ] **Step 8: Prepare, but do not perform, publication**

Report the exact changed files and verification output. Do not commit, push,
sync Dolt, or open a pull request until the user explicitly authorizes that
consequential action.

## Stress Test Results: Implementation Plan

### Resolved Decisions

- Keep the task order: real-Stop revocation, validation-only
  `PostInvocation`, then executable end-to-end proof.
- Do not equate `execution-E` with `invocation-N`; qualify the terminal bridge
  by provider, event kind, valid prefixes, parser-proven `fullyIdle=true`, and
  an open invocation.
- Keep the optional event at the parsed-provider boundary without widening the
  persistence-only recovery API.
- Return on `None` before every persistent or user-visible lifecycle side
  effect.
- Compare lifecycle, activity, and optional session-link state to prove
  `PostInvocation` is inert.
- Retain workspace tests, all-target Clippy, formatting, and workspace build
  gates.
- Close the Beads issue only after fresh verification, while leaving commit,
  sync, push, and PR creation for explicit authorization.

### Changes Made

- Strengthened Task 2 to compare optional `session-links.jsonl` bytes across
  `PostInvocation`.
- No task, security constraint, or acceptance criterion was removed.

### Deferred / Parking Lot

- Commit and publication workflow remains intentionally deferred pending user
  authorization.

### Confidence Assessment

- Overall: High.
- Areas of concern: the projection exception is security-sensitive and must
  remain revocation-only; the focused tests and unchanged generic mismatch
  tests are mandatory evidence before completion.
