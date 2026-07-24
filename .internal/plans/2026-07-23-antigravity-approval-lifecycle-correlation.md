# Antigravity Approval Lifecycle Correlation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Correlate Antigravity step-scoped permission and tool events to their active invocation while ensuring that a fail-safe `ask` is never projected as an effective allow.

**Architecture:** The Antigravity adapter will expose invocation trajectory metadata and distinct open/close events. Lifecycle projection will retain the invocation as the active turn, validate child steps against its trajectory floor, and keep bounded replay state. The permission hook will record an allow only after executable lifecycle state applies, compensate to `NeedsInput` if later activity persistence fails, and preserve existing deny behavior.

**Tech Stack:** Rust 2024, serde/serde_json, Cargo workspace tests, tempfile integration fixtures, Beads.

## Global Constraints

- Preserve the generic `AmbiguousTurn` guard for Codex, Claude Code, and unrelated Antigravity events.
- Correlation requires the exact provider-qualified session, an open `invocation-N`, a supported `step-N` event, and `stepIdx >= initialNumSteps`.
- Track at most 256 distinct child steps per open invocation; capacity must fail safe to `ask` and must not prevent invocation closure.
- Keep lifecycle snapshot schema version 2; all added persisted fields require serde defaults and must remain ignorable by older binaries.
- Never write Antigravity `allow` unless lifecycle and terminal activity persistence both succeed.
- Keep model, deterministic, and provider-policy deny behavior unchanged.
- Do not change first-terminal-wins activity projection.
- Execute the three dependent tasks inline with a review checkpoint after each task; do not dispatch subagents.
- Do not commit, push, or sync without explicit user authorization.

---

### Task 1: Preserve Antigravity Invocation Boundaries and Trajectory Metadata

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/input.rs`
- Modify: `src/provider_hooks/mod.rs`
- Modify: `src/provider_hooks/antigravity.rs`
- Modify: `src/provider_hooks/claude.rs`
- Modify: `src/provider_hooks/codex.rs`
- Modify: `src/lifecycle_hook.rs`
- Test: `src/provider_hooks/antigravity.rs`
- Test: `tests/lifecycle_hook_cli.rs`

**Interfaces:**
- Produces: `LifecycleEvent::from_parts_with_turn_initial_step(identity, kind, Option<u64>) -> Result<LifecycleEvent, LifecycleInputError>`.
- Produces: `LifecycleEvent::turn_initial_step(&self) -> Option<u64>`.
- Produces: `ParsedLifecycleHook::turn_initial_step: Option<u64>`.
- Consumes: existing validated `LifecycleIdentity`, `LifecycleEventKind`, and Antigravity `invocationNum`/`initialNumSteps`.

**Acceptance Criteria:**
- `PreInvocation` produces `UserPromptSubmit` for `invocation-N` with `turn_initial_step = Some(initialNumSteps)`.
- `PostInvocation` produces `Stop` for the same `invocation-N` without reopening the turn.
- Codex, Claude, tool, and provider Stop parsing produce no turn-initial-step metadata.
- Existing lifecycle input validation and provider-qualified identity behavior remain unchanged.

- [x] **Step 1: Write failing adapter tests for distinct invocation events**

Add focused unit coverage in `src/provider_hooks/antigravity.rs`:

```rust
#[test]
fn invocation_events_open_and_close_the_same_trajectory() {
    let payload = serde_json::json!({
        "invocationNum": 3,
        "initialNumSteps": 10,
        "conversationId": "agy-conversation-1",
        "workspacePaths": ["/work/antigravity"],
        "transcriptPath": "/tmp/agy-conversation-1/transcript.jsonl",
        "artifactDirectoryPath": "/tmp/agy-conversation-1/artifacts"
    });
    let raw = serde_json::to_vec(&payload).unwrap();

    let pre = parse_lifecycle(Some("PreInvocation"), &raw).unwrap();
    assert_eq!(pre.event, LifecycleEventKind::UserPromptSubmit);
    assert_eq!(pre.identity.turn_id(), Some("invocation-3"));
    assert_eq!(pre.turn_initial_step, Some(10));

    let post = parse_lifecycle(Some("PostInvocation"), &raw).unwrap();
    assert_eq!(post.event, LifecycleEventKind::Stop);
    assert_eq!(post.identity.turn_id(), Some("invocation-3"));
    assert_eq!(post.turn_initial_step, None);
}
```

- [x] **Step 2: Run the adapter test and verify the current conflation fails**

Run:

```bash
nix develop path:. --command cargo test --lib \
  provider_hooks::antigravity::tests::invocation_events_open_and_close_the_same_trajectory -- --exact
```

Expected: FAIL because `ParsedLifecycleHook` has no `turn_initial_step` and `PostInvocation` currently parses as `UserPromptSubmit`.

- [x] **Step 3: Add optional turn-start metadata to lifecycle events**

In `crates/coding-brain-core/src/lifecycle/input.rs`, extend `LifecycleEvent` without changing its schema:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleEvent {
    identity: LifecycleIdentity,
    kind: LifecycleEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_initial_step: Option<u64>,
}

impl LifecycleEvent {
    pub fn from_parts(
        identity: LifecycleIdentity,
        kind: LifecycleEventKind,
    ) -> Result<Self, LifecycleInputError> {
        Self::from_parts_with_turn_initial_step(identity, kind, None)
    }

    pub fn from_parts_with_turn_initial_step(
        identity: LifecycleIdentity,
        kind: LifecycleEventKind,
        turn_initial_step: Option<u64>,
    ) -> Result<Self, LifecycleInputError> {
        if !matches!(kind, LifecycleEventKind::SessionStart { .. }) {
            require_turn(&identity)?;
        }
        if turn_initial_step.is_some()
            && !matches!(kind, LifecycleEventKind::UserPromptSubmit)
        {
            return Err(LifecycleInputError::Invalid("turn_initial_step"));
        }
        if let LifecycleEventKind::SubagentStart { agent_id }
        | LifecycleEventKind::SubagentStop { agent_id } = &kind
        {
            validate_id("agent_id", agent_id)?;
        }
        Ok(Self {
            identity,
            kind,
            turn_initial_step,
        })
    }

    pub fn turn_initial_step(&self) -> Option<u64> {
        self.turn_initial_step
    }
}
```

Update `LifecycleEvent::parse` and `LifecycleEvent::permission` initializers to set `turn_initial_step: None`, preserving their current public behavior.

- [x] **Step 4: Carry the metadata through provider parsing**

Add the field in `src/provider_hooks/mod.rs`:

```rust
pub(crate) struct ParsedLifecycleHook {
    pub identity: LifecycleIdentity,
    pub event: LifecycleEventKind,
    pub turn_initial_step: Option<u64>,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<ActivityOutcome>,
    pub live_process: Option<LiveProcessIdentity>,
}
```

Set it to `None` in the Codex and Claude adapters. In the Antigravity adapter, split the trusted events and return metadata only for `PreInvocation`:

```rust
enum TrustedAntigravityEvent {
    Stop,
    PreToolUse,
    PostToolUse,
    PreInvocation,
    PostInvocation,
}

Some("PreInvocation") => Ok(Self::PreInvocation),
Some("PostInvocation") => Ok(Self::PostInvocation),

TrustedAntigravityEvent::PreInvocation => (
    LifecycleEventKind::UserPromptSubmit,
    format!("invocation-{invocation}"),
    Some(initial_num_steps),
    None,
    None,
    None,
),
TrustedAntigravityEvent::PostInvocation => (
    LifecycleEventKind::Stop,
    format!("invocation-{invocation}"),
    None,
    None,
    None,
    None,
),
```

Keep `initialNumSteps` required for both invocation payloads, as required by the provider contract. Populate `turn_initial_step` from the tuple in `ParsedLifecycleHook`.

- [x] **Step 5: Construct lifecycle events with provider metadata**

In `src/lifecycle_hook.rs`, replace the provider event construction with:

```rust
let event = match LifecycleEvent::from_parts_with_turn_initial_step(
    parsed.identity.clone(),
    parsed.event.clone(),
    parsed.turn_initial_step,
) {
    Ok(event) => event,
    Err(error) => {
        write_diagnostic(&mut stderr, error);
        return;
    }
};
```

- [x] **Step 6: Add CLI regression coverage for opening and closing**

Extend `antigravity_trusted_cli_events_record_provider_qualified_lifecycle` in `tests/lifecycle_hook_cli.rs` to run `PreInvocation` and `PostInvocation` against the same temporary home:

```rust
let pre = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("PreInvocation"),
    &serde_json::to_vec(&invocation).unwrap(),
);
assert!(pre.status.success());

let post = run_provider_hook_with_event(
    invocation_home.path(),
    Some("antigravity"),
    Some("PostInvocation"),
    &serde_json::to_vec(&invocation).unwrap(),
);
assert!(post.status.success());

let state = &LifecycleStore::at(
    invocation_home.path().join(".local/state/coding-brain"),
)
.read()
.unwrap()
.snapshot
.unwrap()
.sessions[&key];
assert_eq!(state.current_turn.as_deref(), Some("invocation-3"));
assert!(!state.turn_open);
assert_eq!(state.latest_event, Some(LifecycleEventName::Stop));
```

- [x] **Step 7: Run focused provider and CLI tests**

Run:

```bash
nix develop path:. --command cargo test --lib \
  provider_hooks::antigravity::tests -- --nocapture
nix develop path:. --command cargo test --test lifecycle_hook_cli \
  antigravity_trusted_cli_events_record_provider_qualified_lifecycle -- --exact
```

Expected: PASS.

### Task 2: Correlate and Bound Antigravity Child Steps

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`
- Test: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Test: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Consumes: `LifecycleEvent::turn_initial_step()`.
- Produces: `MAX_ANTIGRAVITY_INVOCATION_STEPS: usize = 256`.
- Produces: serde-defaulted `SessionLifecycleState::antigravity_initial_step: Option<u64>`.
- Produces: serde-defaulted `SessionLifecycleState::antigravity_child_events: BTreeMap<u64, u8>`.
- Preserves: `ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)` for any unprovable child.

**Acceptance Criteria:**
- Valid Antigravity permission, pre-tool, and post-tool steps at or above the active invocation floor apply without replacing `current_turn`.
- Repeated child evidence is rejected after intervening events; only permission
  `Decided → NeedsInput` compensation may add a second permission disposition.
- Child steps below the floor, under the wrong turn/provider/session, or beyond capacity are rejected without weakening generic ambiguity handling.
- `PostInvocation` closes and clears invocation-only state even at capacity.
- Existing schema-2 snapshots without the new fields deserialize and validate.

- [x] **Step 1: Add failing projection tests for valid children, floors, and closure**

In `crates/coding-brain-core/src/lifecycle/projection.rs`, add Antigravity helpers and a test:

```rust
fn antigravity_identity(turn: &str) -> LifecycleIdentity {
    LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-conversation-1".into(),
        Some(turn.into()),
        None,
        "/work/antigravity".into(),
    )
    .unwrap()
}

fn invocation(turn: &str, initial_step: u64) -> LifecycleEvent {
    LifecycleEvent::from_parts_with_turn_initial_step(
        antigravity_identity(turn),
        LifecycleEventKind::UserPromptSubmit,
        Some(initial_step),
    )
    .unwrap()
}

#[test]
fn antigravity_steps_are_children_of_the_open_invocation() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(invocation("invocation-1", 5), 1), ApplyOutcome::Applied);
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
    let key = AgentSessionKey::native(
        AgentProvider::Antigravity,
        "agy-conversation-1",
    )
    .storage_key();
    assert_eq!(snapshot.sessions[&key].current_turn.as_deref(), Some("invocation-1"));
    assert!(snapshot.sessions[&key].turn_open);

    let stale = LifecycleEvent::permission(
        antigravity_identity("step-4"),
        PermissionDisposition::Decided,
    )
    .unwrap();
    assert_eq!(
        snapshot.apply(stale, 3),
        ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
    );

    let close = LifecycleEvent::from_parts(
        antigravity_identity("invocation-1"),
        LifecycleEventKind::Stop,
    )
    .unwrap();
    assert_eq!(snapshot.apply(close, 4), ApplyOutcome::Applied);
    assert!(!snapshot.sessions[&key].turn_open);
}
```

- [x] **Step 2: Run the projection test and verify `AmbiguousTurn`**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core \
  lifecycle::projection::tests::antigravity_steps_are_children_of_the_open_invocation -- --exact
```

Expected: FAIL because `step-5` is currently rejected as `AmbiguousTurn`.

- [x] **Step 3: Add invocation state and narrow child correlation**

In `crates/coding-brain-core/src/lifecycle/projection.rs`:

```rust
pub const MAX_ANTIGRAVITY_INVOCATION_STEPS: usize = 256;
const ANTIGRAVITY_PERMISSION_DECIDED_BIT: u8 = 1 << 0;
const ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT: u8 = 1 << 1;
const ANTIGRAVITY_PRE_TOOL_BIT: u8 = 1 << 2;
const ANTIGRAVITY_POST_TOOL_BIT: u8 = 1 << 3;
pub(crate) const ANTIGRAVITY_CHILD_BITS: u8 =
    ANTIGRAVITY_PERMISSION_DECIDED_BIT
        | ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT
        | ANTIGRAVITY_PRE_TOOL_BIT
        | ANTIGRAVITY_POST_TOOL_BIT;

#[serde(default, skip_serializing_if = "Option::is_none")]
pub antigravity_initial_step: Option<u64>,
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub antigravity_child_events: BTreeMap<u64, u8>,
```

Insert those two fields immediately before `last_signature` in
`SessionLifecycleState`, and initialize them to `None` and `BTreeMap::new()` in
`SessionLifecycleState::new`.

Add narrow parsing and bit selection:

```rust
fn prefixed_index(value: &str, prefix: &str) -> Option<u64> {
    value.strip_prefix(prefix)?.parse().ok()
}

fn antigravity_child_bit(kind: &LifecycleEventKind) -> Option<u8> {
    match kind {
        LifecycleEventKind::PermissionRequest {
            disposition: PermissionDisposition::Decided,
        } => Some(ANTIGRAVITY_PERMISSION_DECIDED_BIT),
        LifecycleEventKind::PermissionRequest {
            disposition: PermissionDisposition::NeedsInput,
        } => Some(ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT),
        LifecycleEventKind::PreToolUse => Some(ANTIGRAVITY_PRE_TOOL_BIT),
        LifecycleEventKind::PostToolUse => Some(ANTIGRAVITY_POST_TOOL_BIT),
        _ => None,
    }
}

fn antigravity_child(
    state: &SessionLifecycleState,
    event: &LifecycleEvent,
    turn_id: &str,
) -> Option<(u64, u8)> {
    if event.identity().provider() != AgentProvider::Antigravity
        || !state.turn_open
        || state
            .current_turn
            .as_deref()
            .and_then(|turn| prefixed_index(turn, "invocation-"))
            .is_none()
    {
        return None;
    }
    let step = prefixed_index(turn_id, "step-")?;
    let floor = state.antigravity_initial_step?;
    let bit = antigravity_child_bit(event.kind())?;
    (step >= floor).then_some((step, bit))
}
```

Before the generic differing-turn rejection, branch explicitly:

```rust
if let Some((step, bit)) = antigravity_child(state, &event, turn_id) {
    let previous = state
        .antigravity_child_events
        .get(&step)
        .copied()
        .unwrap_or(0);
    let unsafe_permission_reversal =
        bit == ANTIGRAVITY_PERMISSION_DECIDED_BIT
            && previous & ANTIGRAVITY_PERMISSION_NEEDS_INPUT_BIT != 0;
    if previous & bit != 0 || unsafe_permission_reversal {
        return state.ignore(IgnoreReason::Duplicate);
    }
    if previous == 0
        && state.antigravity_child_events.len() >= MAX_ANTIGRAVITY_INVOCATION_STEPS
    {
        return state.ignore(IgnoreReason::AmbiguousTurn);
    }
    state
        .antigravity_child_events
        .insert(step, previous | bit);
} else {
    // Keep the existing current-turn match unchanged in this branch:
    // open differing non-prompt turns remain AmbiguousTurn, prompts may
    // supersede, and closed/recent turns retain their current behavior.
    match state.current_turn.as_deref() {
        Some(current) if state.turn_open && current != turn_id => {
            if !matches!(event.kind(), LifecycleEventKind::UserPromptSubmit) {
                return state.ignore(IgnoreReason::AmbiguousTurn);
            }
            let current = current.to_owned();
            state.remember_turn(&current);
            state.current_turn = Some(turn_id.to_owned());
        }
        Some(current) if !state.turn_open && current == turn_id => {
            return state.ignore(IgnoreReason::RecentTurn);
        }
        Some(current) if current != turn_id => {
            state.current_turn = Some(turn_id.to_owned());
        }
        None => state.current_turn = Some(turn_id.to_owned()),
        _ => {}
    }
}
```

When an Antigravity `UserPromptSubmit` applies, set
`antigravity_initial_step = event.turn_initial_step()` and clear the child map.
When the current invocation stops, clear both invocation-only fields after
accepting closure.

- [x] **Step 4: Add failing replay, capacity, and mismatch tests**

Add tests that:

```rust
// Decided permission and PostToolUse for the same step are distinct.
// Decided -> NeedsInput applies as compensation.
// NeedsInput -> Decided and exact permission replays are rejected.
// 256 distinct step keys apply; the 257th permission is AmbiguousTurn.
// Stop for invocation-1 still applies and clears the map at capacity.
// Claude turn-1 -> tool turn-2 remains AmbiguousTurn.
// Antigravity non-invocation current turn -> step child remains AmbiguousTurn.
```

Use `MAX_ANTIGRAVITY_INVOCATION_STEPS` in the capacity loop rather than copying
the number into the test.

- [x] **Step 5: Run the new projection matrix**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core \
  lifecycle::projection::tests::antigravity -- --nocapture
```

Expected before implementation completion: replay/capacity cases FAIL. Expected after the minimal state logic: PASS.

- [x] **Step 6: Validate new state and old schema-2 snapshots**

In `crates/coding-brain-core/src/lifecycle/store.rs`, validate every persisted
authority invariant. Parse the storage key once and require:

```rust
let antigravity_state_valid = match (
    key.provider,
    state.antigravity_initial_step,
    state.antigravity_child_events.is_empty(),
) {
    (_, None, true) => true,
    (AgentProvider::Antigravity, Some(floor), _) => {
        state.turn_open
            && state
                .current_turn
                .as_deref()
                .and_then(|turn| turn.strip_prefix("invocation-"))
                .and_then(|value| value.parse::<u64>().ok())
                .is_some()
            && state.antigravity_child_events.len()
                <= MAX_ANTIGRAVITY_INVOCATION_STEPS
            && state.antigravity_child_events.iter().all(|(step, bits)| {
                *step >= floor
                    && *bits != 0
                    && *bits & !ANTIGRAVITY_CHILD_BITS == 0
            })
    }
    _ => false,
};
```

Add a store test that serializes a schema-2 snapshot fixture without
`antigravity_initial_step` or `antigravity_child_events`, loads it, and asserts
both default to empty/`None`. Also assert a state exceeding the cap is rejected
by validation. Add malformed cases for a non-Antigravity provider, closed or
non-invocation turn, below-floor step, zero bits, and unknown bits.

- [x] **Step 7: Run lifecycle core tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core lifecycle -- --nocapture
```

Expected: PASS with unchanged generic provider tests.

### Task 3: Commit Effective Allow Activity Only After Correlation

**Files:**
- Modify: `src/brain/permission_hook.rs`
- Test: `src/brain/permission_hook.rs`
- Test: `tests/hook_activity.rs`

**Interfaces:**
- Consumes: `record_permission(..., PermissionDisposition::Decided)` applying child-step correlation from Task 2.
- Consumes: existing first-terminal-wins `ActivityStore` projection.
- Produces: allow ordering `proposal -> lifecycle Decided -> terminal Allowed -> provider response -> Delivered`.
- Produces: compensation `Decided -> NeedsInput` when terminal activity append fails.

**Acceptance Criteria:**
- An open Antigravity invocation plus an in-range step permission produces `allow`, terminal `Allowed`, and `Delivered`.
- Any allow correlation failure produces `ask`, first terminal `Error`, no terminal `Allowed`, and no delivery evidence.
- Terminal activity failure after lifecycle preparation produces `ask` and best-effort lifecycle `NeedsInput`.
- Model proposals and diagnostics remain auditable.
- Model, deterministic, and provider-policy deny responses remain unchanged.
- Focused tests and all workspace quality gates pass.

The required phase ordering is:

| Outcome | Durable and delivery phases |
| --- | --- |
| Model allow | proposal → lifecycle `Decided` → terminal `Allowed` → response → delivery |
| Model deny | proposal → terminal `Denied` → best-effort lifecycle → response → delivery |
| Abstain | proposal when present → terminal `Abstained` → lifecycle `NeedsInput` → provider fallback |

Only the model-allow row changes existing ordering.

- [x] **Step 1: Strengthen the existing failing projection regression**

In `tests/hook_activity.rs`, extend `model_allow_requires_applied_lifecycle_decision`:

```rust
assert!(
    events
        .iter()
        .all(|event| event.state != ActivityState::Allowed),
    "{provider_name} {ignored_reason:?}: fail-safe response projected as allow"
);
assert!(
    events
        .iter()
        .all(|event| event.state != ActivityState::Delivered),
    "{provider_name} {ignored_reason:?}"
);
assert_eq!(events.last().unwrap().state, ActivityState::Error);
```

- [x] **Step 2: Run the regression and prove the current false allow**

Run:

```bash
nix develop path:. --command cargo test --test hook_activity \
  model_allow_requires_applied_lifecycle_decision -- --exact
```

Expected: FAIL because current events contain terminal `Allowed` before `Error`.

- [x] **Step 3: Add a successful open-invocation integration test**

Add a helper that seeds the trusted invocation through the lifecycle store:

```rust
fn seed_antigravity_invocation(home: &Path, initial_step: u64) {
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-conversation-1".into(),
        Some("invocation-1".into()),
        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
        home.to_path_buf(),
    )
    .unwrap();
    let event = LifecycleEvent::from_parts_with_turn_initial_step(
        identity,
        LifecycleEventKind::UserPromptSubmit,
        Some(initial_step),
    )
    .unwrap();
    assert_eq!(
        LifecycleStore::at(home.join(".local/state/coding-brain"))
            .record(event)
            .unwrap(),
        ApplyOutcome::Applied
    );
}
```

Then add:

```rust
#[test]
fn antigravity_open_invocation_allows_in_range_step() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    seed_antigravity_invocation(home.path(), 5);

    let output = run_provider_permission_hook(
        home.path(),
        "antigravity",
        Some("PreToolUse"),
        &antigravity_permission_payload(home.path(), None),
    );

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"decision": "allow"})
    );
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(events.iter().any(|event| event.state == ActivityState::Allowed));
    assert!(events.iter().any(|event| event.state == ActivityState::Delivered));
}
```

- [x] **Step 4: Reorder only model allow terminal persistence**

In `run_provider_with_gate_and_stores`, keep proposal persistence unchanged.
Build the terminal event but append it immediately only for deny/abstain paths.
For `behavior == Some(PermissionBehavior::Allow)`:

```rust
if let Err(error) = record_permission(
    lifecycle_store,
    &request.lifecycle,
    PermissionDisposition::Decided,
) {
    let message = format!("could not persist executable permission state: {error}");
    write_diagnostic(&mut stderr, &message);
    let mut event = activity_context.as_ref().unwrap().event(ActivityState::Error);
    event.decision_id = Some(decision_id);
    event.reasoning = Some(bounded_redacted_activity_text(&message));
    let _ = activity_store.unwrap().append(event);
    let _ = activity_store.unwrap().compact_if_needed();
    if provider == AgentProvider::Antigravity {
        write_failsafe_ask(&mut stdout, &mut stderr);
    }
    return;
}

if let Err(error) = activity_store.unwrap().append(terminal) {
    write_diagnostic(
        &mut stderr,
        format!("could not persist terminal activity: {error}"),
    );
    if let Err(error) = record_permission(
        lifecycle_store,
        &request.lifecycle,
        PermissionDisposition::NeedsInput,
    ) {
        write_diagnostic(
            &mut stderr,
            format!("could not compensate executable permission state: {error}"),
        );
    }
    if provider == AgentProvider::Antigravity {
        write_failsafe_ask(&mut stdout, &mut stderr);
    }
    return;
}
```

For non-allow model outcomes, preserve the existing terminal append and
response behavior. Do not alter deterministic deny ordering.

- [x] **Step 5: Prove lifecycle compensation is a valid transition**

Add a projection regression before the failure-injection test:

```rust
#[test]
fn permission_decided_can_compensate_to_needs_input() {
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.apply(invocation("invocation-1", 5), 1);
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
    assert_eq!(
        snapshot.apply(
            LifecycleEvent::permission(
                antigravity_identity("step-5"),
                PermissionDisposition::NeedsInput,
            )
            .unwrap(),
            3,
        ),
        ApplyOutcome::Applied
    );
}
```

`EventSignature` already embeds the full `LifecycleEventKind`, including
permission disposition. Do not change its identity representation.

- [x] **Step 6: Add a unit test for the compensation window**

In `src/brain/permission_hook.rs`, use the inference closure passed to
`run_provider_with_gate_and_stores` to make the temporary activity path
unwritable only after `Observed` and `Evaluating` were appended:

```rust
let _guard = crate::config::HOME_ENV_LOCK
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
let temp = tempfile::tempdir().unwrap();
let _restore_home = set_test_home(temp.path());
let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
let activity_path = temp.path().join("activity.jsonl");
let saved_activity_path = temp.path().join("activity-before-failure.jsonl");
let activity = ActivityStore::at(&activity_path);
let identity = LifecycleIdentity::try_new(
    AgentProvider::Antigravity,
    "agy-conversation-1".into(),
    Some("invocation-1".into()),
    Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
    temp.path().to_path_buf(),
)
.unwrap();
lifecycle
    .record(
        LifecycleEvent::from_parts_with_turn_initial_step(
            identity,
            LifecycleEventKind::UserPromptSubmit,
            Some(5),
        )
        .unwrap(),
    )
    .unwrap();
let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
    "../../tests/fixtures/hooks/antigravity-pre-tool-use.json"
))
.unwrap();
payload["workspacePaths"] = serde_json::json!([temp.path()]);
let mut stdout = Vec::new();
let mut stderr = Vec::new();

run_provider_with_gate_and_stores(
    Cursor::new(serde_json::to_vec(&payload).unwrap()),
    &mut stdout,
    &mut stderr,
    Some(&enabled_config()),
    BrainGateMode::Auto,
    &lifecycle,
    Some(&activity),
    AgentProvider::Antigravity,
    Some("PreToolUse"),
    |_, _| {
        std::fs::rename(&activity_path, &saved_activity_path).unwrap();
        std::fs::create_dir(&activity_path).unwrap();
        Ok(suggestion(RuleAction::Approve, 0.9))
    },
);

assert_eq!(
    serde_json::from_slice::<serde_json::Value>(&stdout).unwrap()["decision"],
    "ask"
);
assert_eq!(projected_status(&lifecycle), Some(ProjectedStatus::NeedsInput));
let saved = ActivityStore::at(&saved_activity_path).read().unwrap();
assert_eq!(
    saved
        .events()
        .iter()
        .map(|event| event.state)
        .collect::<Vec<_>>(),
    [ActivityState::Observed, ActivityState::Evaluating]
);
```

Assert stdout is Antigravity `ask`, no allow response was written, and the
lifecycle snapshot projects `NeedsInput`. The temporary directory owns cleanup.

- [x] **Step 7: Run focused permission tests**

Run:

```bash
nix develop path:. --command cargo test --test hook_activity \
  model_allow_requires_applied_lifecycle_decision -- --exact
nix develop path:. --command cargo test --test hook_activity \
  antigravity_open_invocation_allows_in_range_step -- --exact
nix develop path:. --command cargo test --lib \
  brain::permission_hook::tests -- --nocapture
```

Expected: PASS. Existing `provider_ask_and_model_deny_survive_ignored_lifecycle_decision` and deterministic deny tests must remain green.

- [x] **Step 8: Run formatting and complete workspace quality gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo build --workspace
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
```

Expected: all commands exit 0 with no formatting diff or clippy warnings.

- [x] **Step 9: Inspect the final surgical diff and Beads state**

Run:

```bash
git diff --check
git status --short
bd -C /home/alexander/.beads-planning show codexctl-3fbo
```

Expected: only the approved spec/plan and files named by these tasks are
changed; `codexctl-3fbo` remains in progress until verification evidence is
recorded and the issue is closed.

## Stress Test Results: Antigravity approval implementation plan

### Resolved Decisions

- Core interface: carry trajectory floor as optional, validated, serde-defaulted
  lifecycle event metadata instead of encoding it into identity.
- Task boundaries: keep provider metadata, core correlation, and permission
  ordering as three sequential review units.
- Correlation control flow: use an exact helper and explicit mutation branch;
  preserve generic turn handling verbatim in the alternative branch.
- Persistence ordering: delay only model allow terminal state; retain deny and
  abstain phases and prove compensation as a distinct disposition.
- Replay and compensation: use separate permission-disposition bits, permit
  only `Decided → NeedsInput`, and reject the reverse escalation.
- Failure injection: isolate HOME, preserve the pre-failure log, and verify both
  absence of allow and lifecycle compensation.
- Snapshot validation: validate every provider, turn, floor, bitmask, and
  capacity invariant while accepting absent compatibility defaults.
- Verification: use the verified `nix develop path:.` environment, library
  targets for module tests, and true workspace-wide final gates.
- Execution: run the dependent tasks inline with checkpoints and no subagents.

### Changes Made

- Replaced an invalid mixed boolean/`Option` expression with exact correlation
  code.
- Added outcome phase ordering and a compensation-transition regression.
- Made replay state disposition-aware so compensation remains executable.
- Strengthened failure injection and persisted-state validation.
- Corrected Cargo test targets and workspace gate coverage.
- Set inline execution as the plan's execution policy.

### Deferred / Parking Lot

- Commits, pushes, and remote Beads synchronization remain subject to explicit
  user authorization.

### Confidence Assessment

- Overall: High
- Areas of concern: cross-store persistence is compensating rather than atomic;
  the tests explicitly cover the safe provider response and observable state.
