# Codex Subagent Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Preserve exact provider and child session identities so Codex subagent permissions and outcomes remain isolated across concurrent turns.

**Architecture:** Add a provider-neutral optional `provider_session_id` to lifecycle and activity identity, while keeping provider adapters responsible for proving and populating that relationship. Codex normal child callbacks use `agent_id` as the actionable session and the shared Codex `session_id` as the provider session; lifecycle projection requires a matching active `SubagentStart` before accepting child state.

**Tech Stack:** Rust 2024 workspace, Serde JSON, fs2-locked JSON/JSONL state, Cargo tests, Clippy, rustfmt.

## Global Constraints

- `session_id` is always the exact actionable root or child identity and must
  be unique across that provider. Parent-local child IDs must be transformed
  by the adapter into a bounded collision-free effective ID or left unlinked.
- `provider_session_id` is optional, provider-neutral grouping metadata; it must never imply unavailable immediate ancestry.
- Only the Codex adapter populates the new relationship in this change. Claude and Antigravity behavior remains unchanged.
- Never infer identity from timing, command text, turn ordering, or the latest active child.
- A linked child callback requires an exact active provider-session→child topology entry.
- Missing or invalid proof suppresses authorizing decisions. Deterministic and
  provider-policy denials remain fail-closed because they cannot grant
  execution; ordinary abstention leaves the native provider boundary
  authoritative.
- Activity and lifecycle schemas advance from 2 to 3.
- Keep the existing bounds: 64 active children per provider session, 128 lifecycle states globally, and 24-hour inactive retention.
- Do not add configuration, terminal injection, or legacy-path migration.
- Keep the manual `--record-outcome` pending/resolved pipeline unchanged. Managed
  provider hooks use structured lifecycle correlation; its eventual deprecation
  and telemetry/report migration are tracked separately by `codexctl-vwil`.
- Do not commit, push, or publish unless the user explicitly authorizes it. Commit steps below are named checkpoints, not standing authorization.

## File Structure

- `crates/coding-brain-core/src/lifecycle/input.rs`: provider-neutral lifecycle identity and validation.
- `crates/coding-brain-core/src/lifecycle/projection.rs`: linked child topology, concurrency, cleanup, and schema-3 state.
- `crates/coding-brain-core/src/lifecycle/store.rs`: schema-1/2 migration and schema-3 snapshot validation.
- `crates/coding-brain-core/src/brain_activity.rs`: persisted activity identity and activity schema 3.
- `src/provider_hooks/codex.rs`: Codex 0.145.0 child payload mapping.
- `src/provider_hooks/mod.rs`: linked-identity constructor shared by provider adapters without changing other providers.
- `src/brain/permission_hook.rs`: child Decision targets.
- `src/lifecycle_hook.rs`: child Lifecycle/Diagnostic/Outcome targets and exact correlation.
- `src/brain/activity.rs`: activity-store comparisons that must include provider-session identity.
- Existing `SessionTarget` construction sites: add explicit `provider_session_id: None` to root/process-only fixtures and call sites.
- `tests/hook_activity.rs`: process-level permission and outcome isolation.
- `tests/lifecycle_hook_cli.rs`: CLI lifecycle persistence and fail-safe behavior.

---

### Task 1: Provider-Neutral Identity and Activity Schema

**Files:**

- Modify: `crates/coding-brain-core/src/lifecycle/input.rs`
- Modify: `crates/coding-brain-core/src/brain_activity.rs`
- Modify: `src/brain/activity.rs`
- Modify: every Rust `SessionTarget { ... }` construction returned by `rg -n "SessionTarget \\{" --glob '*.rs'`

**Interfaces:**

- Produces: `LifecycleIdentity::try_new_with_provider_session(...)`
- Produces: `LifecycleIdentity::provider_session_id() -> Option<&str>`
- Produces: `SessionTarget.provider_session_id: Option<String>`
- Produces: `ACTIVITY_SCHEMA_VERSION == 3`
- Consumes: existing `MAX_ID_BYTES`, `AgentProvider`, and Serde compatibility conventions.

**Acceptance Criteria:**

- Root lifecycle identities retain `provider_session_id == None`.
- Linked identities preserve distinct effective and provider session IDs.
- Empty, oversized, or self-linked provider session IDs are rejected.
- Activity schema-1/2 rows deserialize with no provider session.
- Activity schema-3 rows serialize and round-trip the provider session.
- A mixed schema-1/2/3 append-only activity log remains readable in order.
- All existing root/process-only `SessionTarget` call sites compile with explicit `None`.

- [ ] **Step 1: Add failing lifecycle identity tests**

Add to `crates/coding-brain-core/src/lifecycle/input.rs`:

```rust
#[test]
fn linked_identity_preserves_effective_and_provider_sessions() {
    let identity = LifecycleIdentity::try_new_with_provider_session(
        AgentProvider::Codex,
        "child-1".into(),
        Some("provider-1".into()),
        Some("turn-1".into()),
        Some(PathBuf::from("/tmp/rollout.jsonl")),
        PathBuf::from("/work/project"),
    )
    .unwrap();

    assert_eq!(identity.session_id(), "child-1");
    assert_eq!(identity.provider_session_id(), Some("provider-1"));
}

#[test]
fn linked_identity_rejects_unbounded_or_self_linked_provider_session() {
    for provider_session in ["", "child-1"] {
        assert!(
            LifecycleIdentity::try_new_with_provider_session(
                AgentProvider::Codex,
                "child-1".into(),
                Some(provider_session.into()),
                Some("turn-1".into()),
                None,
                PathBuf::from("/work/project"),
            )
            .is_err()
        );
    }
    assert_eq!(
        LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            "child-1".into(),
            Some("x".repeat(MAX_ID_BYTES + 1)),
            Some("turn-1".into()),
            None,
            PathBuf::from("/work/project"),
        )
        .unwrap_err(),
        LifecycleInputError::TooLong("provider_session_id")
    );
}
```

- [ ] **Step 2: Run the lifecycle identity tests and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-core linked_identity_ -- --nocapture
```

Expected: compilation fails because `try_new_with_provider_session` and `provider_session_id` do not exist.

- [ ] **Step 3: Implement the provider-neutral lifecycle identity**

In `crates/coding-brain-core/src/lifecycle/input.rs`, add:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleIdentity {
    provider: AgentProvider,
    session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
}

pub fn try_new_with_provider_session(
    provider: AgentProvider,
    session_id: String,
    provider_session_id: Option<String>,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
) -> Result<Self, LifecycleInputError> {
    validate_id("session_id", &session_id)?;
    if let Some(provider_session_id) = provider_session_id.as_deref() {
        validate_id("provider_session_id", provider_session_id)?;
        if provider_session_id == session_id {
            return Err(LifecycleInputError::Invalid("provider_session_id"));
        }
    }
    // Preserve the existing turn/path validation before constructing Self.
}

pub fn provider_session_id(&self) -> Option<&str> {
    self.provider_session_id.as_deref()
}
```

Keep `try_new(...)` as the compatibility constructor and delegate to
`try_new_with_provider_session(..., None, ...)`.

- [ ] **Step 4: Add failing activity schema tests**

In `crates/coding-brain-core/src/brain_activity.rs`, extend the existing target serialization tests:

```rust
#[test]
fn session_target_round_trips_provider_session_identity() {
    let target = SessionTarget {
        provider: AgentProvider::Codex,
        session_id: "child-1".into(),
        provider_session_id: Some("provider-1".into()),
        turn_id: Some("turn-1".into()),
        tool_use_id: None,
        project_id: ProjectId::Stable("project-1".into()),
        cwd: PathBuf::from("/work/project"),
        provider_hints: vec![],
        provenance: SessionTargetProvenance::Structured,
    };
    let value = serde_json::to_value(&target).unwrap();
    assert_eq!(value["provider_session_id"], "provider-1");
    assert_eq!(serde_json::from_value::<SessionTarget>(value).unwrap(), target);
}

#[test]
fn legacy_session_target_defaults_provider_session_to_none() {
    let target: SessionTarget = serde_json::from_value(json!({
        "session_id": "root-1",
        "project_id": {"kind": "stable", "value": "project-1"},
        "cwd": "/work/project"
    }))
    .unwrap();
    assert_eq!(target.provider_session_id, None);
}
```

In `src/brain/activity.rs`, add a store-level compatibility test. Serialize
three otherwise valid activity events to one temporary `activity.jsonl`, set
their schema versions to 1, 2, and 3, remove `provider_session_id` from the
legacy rows, and retain `"provider-1"` on the schema-3 row. Read the file
through `ActivityStore::read` and assert:

```rust
assert_eq!(log.events().len(), 3);
assert_eq!(
    log.events()
        .iter()
        .map(|event| event.schema_version)
        .collect::<Vec<_>>(),
    vec![1, 2, 3]
);
assert_eq!(log.events()[0].session.as_ref().unwrap().provider_session_id, None);
assert_eq!(log.events()[1].session.as_ref().unwrap().provider_session_id, None);
assert_eq!(
    log.events()[2]
        .session
        .as_ref()
        .unwrap()
        .provider_session_id
        .as_deref(),
    Some("provider-1")
);
```

- [ ] **Step 5: Run the activity tests and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-core provider_session -- --nocapture
direnv exec . cargo test mixed_activity_schema_versions_remain_readable -- --nocapture
```

Expected: compilation fails because `SessionTarget.provider_session_id` does
not exist.

- [ ] **Step 6: Implement activity schema 3**

In `crates/coding-brain-core/src/brain_activity.rs`:

```rust
pub const ACTIVITY_SCHEMA_VERSION: u32 = 3;

pub struct SessionTarget {
    #[serde(default)]
    pub provider: AgentProvider,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    // existing fields unchanged
}
```

Normalize the new field with the same bounded, non-redacting identifier path:

```rust
session.provider_session_id = session
    .provider_session_id
    .take()
    .map(|value| bounded(&value, false));
```

Add `provider_session_id: None` to every existing root/process-only
`SessionTarget` literal. Do not introduce a builder solely to avoid this
mechanical update.

- [ ] **Step 7: Run focused tests and compile every crate**

Run:

```bash
direnv exec . cargo test -p coding-brain-core linked_identity_ -- --nocapture
direnv exec . cargo test -p coding-brain-core provider_session -- --nocapture
direnv exec . cargo test mixed_activity_schema_versions_remain_readable -- --nocapture
direnv exec . cargo check --workspace
```

Expected: all focused tests pass and the workspace compiles without missing
`SessionTarget` fields.

- [ ] **Step 8: Commit checkpoint, only with explicit authorization**

```bash
git add crates/coding-brain-core/src/lifecycle/input.rs \
  crates/coding-brain-core/src/brain_activity.rs \
  src/brain/activity.rs
git commit -m "🏗️ refactor: model provider session identity"
```

### Task 2: Linked Lifecycle Projection and Cleanup

**Files:**

- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**

- Consumes: `LifecycleIdentity::provider_session_id()`
- Produces: schema-3 `SessionLifecycleState.provider_session_id`
- Produces: schema-3 child topology keyed by child ID with its exact active turn.
- Produces: fail-safe ignore reasons for unproven topology and provider-session mismatch.
- Produces: dedicated `SubagentStart`/`SubagentStop` topology handling before the generic turn guard.

**Acceptance Criteria:**

- Two linked Codex siblings can interleave events without `AmbiguousTurn`.
- A linked child is rejected until its exact `SubagentStart` exists.
- Linked callbacks and `SubagentStop` must match the active child's recorded turn.
- Parent topology events do not replace the provider session's current turn.
- `SubagentStop` removes only its exact linked child state.
- Root/provider-session `Stop` and restart remove all matching child state.
- A mismatched provider session cannot mutate or delete a child.
- Accepted linked activity refreshes its provider-session topology lease.
- Retention never persists a linked child after removing its provider session.
- Schema-1 and schema-2 snapshots migrate to schema 3 with no provider-session link.
- Existing capacity, replay, and retention bounds remain enforced.

- [ ] **Step 1: Add failing sibling and topology tests**

Add helpers and tests in `crates/coding-brain-core/src/lifecycle/projection.rs`:

```rust
fn linked_tool(child: &str, provider_session: &str, turn: &str) -> LifecycleEvent {
    LifecycleEvent::from_parts(
        LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            child.into(),
            Some(provider_session.into()),
            Some(turn.into()),
            None,
            PathBuf::from("/work/project"),
        )
        .unwrap(),
        LifecycleEventKind::PreToolUse,
    )
    .unwrap()
}

#[test]
fn interleaved_codex_siblings_have_independent_turn_state() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_start("root", "child-b", "turn-b"), 2), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-a", "root", "turn-a"), 3), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-b", "root", "turn-b"), 4), ApplyOutcome::Applied);

    assert_eq!(snapshot.sessions[&native_key("child-a")].current_turn.as_deref(), Some("turn-a"));
    assert_eq!(snapshot.sessions[&native_key("child-b")].current_turn.as_deref(), Some("turn-b"));
}

#[test]
fn linked_child_without_active_topology_is_rejected() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(
        snapshot.apply(linked_tool("child-a", "root", "turn-a"), 1),
        ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent)
    );
    assert!(!snapshot.sessions.contains_key(&native_key("child-a")));
}

#[test]
fn delayed_event_from_reused_child_id_is_rejected() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(subagent_start("root", "child-a", "turn-old"), 1), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_stop("root", "child-a", "turn-old"), 2), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_start("root", "child-a", "turn-new"), 3), ApplyOutcome::Applied);
    assert_eq!(
        snapshot.apply(linked_tool("child-a", "root", "turn-old"), 4),
        ApplyOutcome::Ignored(IgnoreReason::SubagentTurnMismatch)
    );
    assert!(!snapshot.sessions.contains_key(&native_key("child-a")));
    assert_eq!(
        snapshot.sessions[&native_key("root")]
            .active_subagents["child-a"]
            .turn_id,
        "turn-new"
    );
}
```

- [ ] **Step 2: Run the projection tests and confirm RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-core interleaved_codex_siblings -- --nocapture
direnv exec . cargo test -p coding-brain-core linked_child_without_active_topology -- --nocapture
direnv exec . cargo test -p coding-brain-core delayed_event_from_reused_child_id -- --nocapture
```

Expected: tests fail because linked proof and independent child states are not implemented.

- [ ] **Step 3: Implement linked-state proof and topology handling**

In `SessionLifecycleState`, add:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub provider_session_id: Option<String>,
```

Initialize it from `event.identity().provider_session_id()`.

Add explicit ignore reasons:

```rust
pub enum IgnoreReason {
    Duplicate,
    RecentTurn,
    AmbiguousTurn,
    ActiveSubagentCapacity,
    UnprovenSubagent,
    ProviderSessionMismatch,
    SubagentTurnMismatch,
}
```

Before inserting or mutating a linked child state:

```rust
let provider_key = AgentSessionKey::native(
    event.identity().provider(),
    provider_session_id,
)
.storage_key();
let proven = self
    .sessions
    .get(&provider_key)
    .and_then(|state| state.active_subagents.get(event.identity().session_id()));
let Some(proven) = proven else {
    return ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent);
};
if Some(proven.turn_id.as_str()) != event.identity().turn_id() {
    return ApplyOutcome::Ignored(IgnoreReason::SubagentTurnMismatch);
}
```

If the child key already exists, require its stored
`provider_session_id == event.identity().provider_session_id()`.
Extend `ActiveSubagentState` with a bounded `turn_id: String`, populate it from
`SubagentStart`, and require `SubagentStop` to match it before cleanup.

Handle `SubagentStart` and `SubagentStop` before the generic
`current_turn`/`turn_open` check. They update topology and sequence metadata
without assigning their child `turn_id` to the provider session's
`current_turn`.

After an accepted linked event, update the matching
`ActiveSubagentState.received_at_ms` and the provider session's
`latest_received_at_ms` to the same receipt time. Do not replace the provider
session's latest event, status event, turn, signature, or sequence with the
child event.

- [ ] **Step 4: Add failing cleanup tests**

Add projection tests:

```rust
#[test]
fn subagent_stop_removes_only_the_exact_linked_child() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_start("root", "child-b", "turn-b"), 2), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-a", "root", "turn-a"), 3), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-b", "root", "turn-b"), 4), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_stop("root", "child-a", "turn-a"), 5), ApplyOutcome::Applied);

    assert!(!snapshot.sessions.contains_key(&native_key("child-a")));
    assert!(snapshot.sessions.contains_key(&native_key("child-b")));
    assert!(!snapshot.sessions[&native_key("root")].active_subagents.contains_key("child-a"));
    assert!(snapshot.sessions[&native_key("root")].active_subagents.contains_key("child-b"));
}

#[test]
fn provider_stop_removes_all_linked_children() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(subagent_start("root", "child-b", "turn-b"), 2), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-a", "root", "turn-a"), 3), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-b", "root", "turn-b"), 4), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(root_stop("root", "root-turn"), 5), ApplyOutcome::Applied);

    assert!(!snapshot.sessions.contains_key(&native_key("child-a")));
    assert!(!snapshot.sessions.contains_key(&native_key("child-b")));
    assert!(snapshot.sessions[&native_key("root")].active_subagents.is_empty());
}

#[test]
fn mismatched_provider_session_cannot_clean_up_child() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(snapshot.apply(subagent_start("root-a", "child-a", "turn-a"), 1), ApplyOutcome::Applied);
    assert_eq!(snapshot.apply(linked_tool("child-a", "root-a", "turn-a"), 2), ApplyOutcome::Applied);
    assert_eq!(
        snapshot.apply(subagent_stop("root-b", "child-a", "turn-a"), 3),
        ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch)
    );

    assert!(snapshot.sessions.contains_key(&native_key("root-a")));
    assert!(snapshot.sessions.contains_key(&native_key("child-a")));
    assert!(snapshot.sessions[&native_key("root-a")].active_subagents.contains_key("child-a"));
    assert!(!snapshot.sessions.contains_key(&native_key("root-b")));
}
```

Implement `subagent_stop` and `root_stop` beside the existing event helpers,
using `LifecycleEventKind::SubagentStop` and `LifecycleEventKind::Stop`
respectively. Keep the assertions against both `snapshot.sessions` and
`active_subagents`; do not test only the returned `ApplyOutcome`.

- [ ] **Step 5: Implement lock-local idempotent cleanup**

Keep cleanup inside `LifecycleSnapshot::apply`, which already runs under the
store's exclusive lock. Perform topology lookup and all provider-session
validation before calling `entry`, incrementing sequence state, or mutating any
session. Validate the child state's stored provider session before removing
either the active-map entry or child state. A missing child state is a valid
stop for a child that used no tools. A mismatched state rejects the entire
cleanup and leaves the snapshot unchanged, including not creating a state for
the claimed provider session.

On provider-session `Stop` or `SessionStart`, collect only keys whose
`state.provider_session_id` matches the provider session and remove them after
the parent-state mutation so Rust borrows do not overlap.

- [ ] **Step 6: Add schema-2 migration tests**

In `crates/coding-brain-core/src/lifecycle/store.rs`, add a serialized
schema-2 snapshot fixture with no `provider_session_id`, read it through
`LifecycleStore`, and assert:

```rust
assert_eq!(view.condition, StoreCondition::Healthy);
assert_eq!(snapshot.schema_version, 3);
assert!(snapshot.sessions.values().all(|state| state.provider_session_id.is_none()));
assert!(snapshot.sessions.values().all(|state| state.active_subagents.is_empty()));
```

Also retain the existing newer-schema rejection test with version 4.

Add retention-boundary tests using `LifecycleStore::record_at`:

```rust
#[test]
fn linked_activity_refreshes_provider_topology_retention() {
    let store = store();
    store.record_at(subagent_start("root", "child-a", "turn-a"), 1).unwrap();
    store
        .record_at(
            linked_tool("child-a", "root", "turn-a"),
            SESSION_RETENTION_MS,
        )
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    assert!(snapshot.sessions.contains_key(&native_key("root")));
    assert!(snapshot.sessions.contains_key(&native_key("child-a")));
    assert_eq!(
        snapshot.sessions[&native_key("root")].latest_received_at_ms,
        SESSION_RETENTION_MS
    );
}

#[test]
fn retention_removes_expired_provider_and_linked_children_atomically() {
    let store = store_with_schema_three_snapshot(expired_linked_group(
        "root",
        "child-a",
        "turn-a",
    ));
    store
        .record_at(
            root_prompt("other-root", "other-turn"),
            SESSION_RETENTION_MS + 1,
        )
        .unwrap();

    let snapshot = store.snapshot().unwrap();
    assert!(!snapshot.sessions.contains_key(&native_key("root")));
    assert!(!snapshot.sessions.contains_key(&native_key("child-a")));
    assert!(snapshot.sessions.contains_key(&native_key("other-root")));
}
```

Implement `store_with_schema_three_snapshot` and `expired_linked_group` as
test-only helpers that write a valid schema-3 snapshot with both timestamps at
zero. Retention must first identify expired provider-session keys, then remove
those providers and every state whose `provider_session_id` names one of them
before applying the incoming event.

- [ ] **Step 7: Implement lifecycle schema 3 and migrations**

Set:

```rust
pub const LIFECYCLE_SCHEMA_VERSION: u32 = 3;
```

Accept persisted schema versions 1, 2, and 3. Project schema 1 through its
existing Codex provider-key migration, then apply the schema-2→3 defaulting
step. Clear migrated active-subagent maps because schemas 1 and 2 do not carry
the exact child-turn proof required by schema 3. Validate bounded, non-self
provider session IDs and require linked states to resolve to a provider-session
state with the same `AgentProvider`. Codex is the only adapter that creates
linked states in this delivery, but this core invariant must remain
provider-neutral.

- [ ] **Step 8: Run projection and store tests**

Run:

```bash
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests -- --nocapture
direnv exec . cargo test -p coding-brain-core lifecycle::store::tests -- --nocapture
```

Expected: sibling, proof, cleanup, replay, capacity, and migration tests pass.

- [ ] **Step 9: Commit checkpoint, only with explicit authorization**

```bash
git add crates/coding-brain-core/src/lifecycle/projection.rs crates/coding-brain-core/src/lifecycle/store.rs
git commit -m "🔒 fix: isolate linked subagent lifecycle state"
```

### Task 3: Codex Child Hook Contract Mapping

**Files:**

- Modify: `src/provider_hooks/mod.rs`
- Modify: `src/provider_hooks/codex.rs`
- Test: `src/provider_hooks/codex.rs`
- Test: `tests/fixtures/hooks/subagent-start.json`
- Test: `tests/fixtures/hooks/subagent-stop.json`
- Create: `tests/fixtures/hooks/codex-child-permission-request.json`
- Create: `tests/fixtures/hooks/codex-child-pre-tool-use.json`
- Create: `tests/fixtures/hooks/codex-child-post-tool-use.json`

**Interfaces:**

- Consumes: `LifecycleIdentity::try_new_with_provider_session(...)`
- Produces: `linked_identity(...) -> Result<LifecycleIdentity, HookInputError>`
- Produces: Codex normal child callbacks with effective `session_id = agent_id` and `provider_session_id = session_id`.
- Preserves: parent-scoped `SubagentStart` and `SubagentStop`.

**Acceptance Criteria:**

- Root Codex callbacks retain existing identity.
- Child `PermissionRequest`, `PreToolUse`, and `PostToolUse` use exact child and provider-session IDs.
- `SubagentStart` and `SubagentStop` remain projection-parent-scoped.
- Empty, oversized, or self-linked Codex child IDs are rejected.
- The child permission fixture matches the provider contract and omits
  `tool_use_id`; tool lifecycle fixtures retain their exact tool-use ID.
- Claude and Antigravity parser tests remain unchanged and passing.

- [ ] **Step 1: Add failing Codex parser tests**

Add the three captured-shape fixture files using the exact common fields from
Codex 0.145.0. The permission fixture must contain `session_id`, child
`turn_id`, `agent_id`, `agent_type`, `transcript_path`, `cwd`,
`hook_event_name`, `tool_name`, and `tool_input`, but no `tool_use_id`. The
PreToolUse and PostToolUse fixtures contain the same identity plus
`tool_use_id: "call-child-1"` and the event-specific response field.

Add `#[cfg(test)] mod tests` to `src/provider_hooks/codex.rs` and read those
fixtures directly:

```rust
#[test]
fn child_permission_uses_agent_id_and_preserves_provider_session() {
    let request = parse_permission(
        include_bytes!("../../tests/fixtures/hooks/codex-child-permission-request.json"),
    )
    .unwrap();
    assert_eq!(request.lifecycle.session_id(), "child-1");
    assert_eq!(request.lifecycle.provider_session_id(), Some("root-1"));
    assert_eq!(request.tool_use_id, None);
}

#[test]
fn child_pre_and_post_tool_use_preserve_linked_identity() {
    for payload in [
        include_bytes!("../../tests/fixtures/hooks/codex-child-pre-tool-use.json").as_slice(),
        include_bytes!("../../tests/fixtures/hooks/codex-child-post-tool-use.json").as_slice(),
    ] {
        let parsed = parse_lifecycle(payload).unwrap();
        assert_eq!(parsed.identity.session_id(), "child-1");
        assert_eq!(parsed.identity.provider_session_id(), Some("root-1"));
        assert_eq!(parsed.tool_use_id.as_deref(), Some("call-child-1"));
    }
}

#[test]
fn subagent_topology_events_remain_provider_session_scoped() {
    let parsed = parse_lifecycle(include_bytes!("../../tests/fixtures/hooks/subagent-start.json")).unwrap();
    assert_eq!(parsed.identity.session_id(), "session-1");
    assert_eq!(parsed.identity.provider_session_id(), None);
}
```

Add explicit empty and `MAX_ID_BYTES + 1` `agent_id` cases.

- [ ] **Step 2: Run parser tests and confirm RED**

Run:

```bash
direnv exec . cargo test child_permission_uses_agent_id -- --nocapture
direnv exec . cargo test child_pre_and_post_tool_use -- --nocapture
```

Expected: assertions fail because Codex currently discards `agent_id`.

- [ ] **Step 3: Add the linked identity helper**

In `src/provider_hooks/mod.rs`:

```rust
pub(super) fn linked_identity(
    provider: AgentProvider,
    session_id: String,
    provider_session_id: String,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
) -> Result<LifecycleIdentity, HookInputError> {
    LifecycleIdentity::try_new_with_provider_session(
        provider,
        session_id,
        Some(provider_session_id),
        turn_id,
        transcript_path,
        cwd,
    )
    .map_err(Into::into)
}
```

Do not change Claude or Antigravity call sites to use it.

- [ ] **Step 4: Parse and select Codex child identity**

Add optional `agent_id` to `CodexPermissionInput` and `CodexActivityInput`.
Validate it with `optional_id`.

For permission callbacks:

```rust
let lifecycle = match optional_id(input.agent_id, "agent_id")? {
    Some(agent_id) => linked_identity(
        AgentProvider::Codex,
        agent_id,
        input.session_id,
        input.turn_id,
        input.transcript_path,
        PathBuf::from(input.cwd),
    )?,
    None => identity(/* existing root arguments */)?,
};
```

For lifecycle callbacks, first parse the event with `LifecycleEvent::parse`.
If the event is `SubagentStart` or `SubagentStop`, keep its parent-scoped
identity. Otherwise, rebuild the identity with `agent_id` and the original
Codex session as `provider_session_id`.

- [ ] **Step 5: Run all provider parser tests**

Run:

```bash
direnv exec . cargo test provider_hooks:: -- --nocapture
```

Expected: Codex child/root tests and existing Claude/Antigravity tests pass.

- [ ] **Step 6: Commit checkpoint, only with explicit authorization**

```bash
git add src/provider_hooks/mod.rs src/provider_hooks/codex.rs tests/fixtures/hooks
git commit -m "🔌 fix: preserve Codex child hook identity"
```

### Task 4: Permission, Activity, and Outcome Isolation

**Files:**

- Modify: `src/brain/permission_hook.rs`
- Modify: `src/lifecycle_hook.rs`
- Modify: `src/brain/activity.rs`
- Test: `tests/hook_activity.rs`
- Test: `tests/lifecycle_hook_cli.rs`

**Interfaces:**

- Consumes: linked `LifecycleIdentity`
- Produces: Decision/Lifecycle/Diagnostic/Outcome `SessionTarget` rows with exact `provider_session_id`.
- Produces: one shared exact-identity predicate used by direct and bounded-fallback outcome correlation.
- Preserves: proposal→terminal activity→provider response ordering.

**Acceptance Criteria:**

- Child permission Decisions retain both child and provider-session IDs.
- Child PreToolUse/PostToolUse observations retain both IDs.
- `SubagentStart`/`SubagentStop` audit rows are child-centric and retain the provider session.
- Parent and sibling decisions are never outcome candidates.
- Interleaved sibling permissions can be delivered without `AmbiguousTurn`.
- Missing topology, persistence failure, stale replay, and mismatched provider
  session suppress model allows; deterministic and provider-policy denials
  remain deliverable with bounded diagnostics.
- Global lifecycle capacity rejects a new child without evicting existing state
  or emitting authorizing output.
- Existing root, Claude, and Antigravity hook integration tests pass.

- [ ] **Step 1: Add a failing process-level sibling permission test**

In `tests/hook_activity.rs`, add helpers that load the committed child fixtures
as `serde_json::Value` and replace only `session_id`, `agent_id`, `turn_id`,
and the lifecycle fixture's `tool_use_id`. The permission helper must assert
that the resulting object still has no `tool_use_id`. Then add:

```rust
#[test]
fn interleaved_codex_children_receive_isolated_permission_decisions() {
    let home = tempfile::tempdir().unwrap();
    write_brain_config(home.path());

    run_provider_lifecycle_hook(home.path(), "codex", None, &subagent_start_payload(home.path(), "child-a", "turn-a"));
    run_provider_lifecycle_hook(home.path(), "codex", None, &subagent_start_payload(home.path(), "child-b", "turn-b"));
    run_provider_lifecycle_hook(home.path(), "codex", None, &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"));
    run_provider_lifecycle_hook(home.path(), "codex", None, &child_pre_payload(home.path(), "child-b", "turn-b", "tool-b"));

    let child_a = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-a", "turn-a"),
    );
    let child_b = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload(home.path(), "child-b", "turn-b"),
    );

    assert!(!child_a.stdout.is_empty());
    assert!(!child_b.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&child_a.stderr).contains("AmbiguousTurn"));
    assert!(!String::from_utf8_lossy(&child_b.stderr).contains("AmbiguousTurn"));
}
```

- [ ] **Step 2: Run the sibling integration test and confirm RED**

Run:

```bash
direnv exec . cargo test --test hook_activity interleaved_codex_children_receive_isolated_permission_decisions -- --nocapture
```

Expected: fails because child callbacks are currently projected under the root
session and one permission is rejected as `AmbiguousTurn`.

- [ ] **Step 3: Persist provider session on permission and lifecycle activity**

In `HookActivity::from_request`, `observation_event`, `diagnostic_event`, and
all lifecycle outcome target construction, set:

```rust
provider_session_id: lifecycle
    .identity()
    .provider_session_id()
    .map(str::to_owned),
```

For `SubagentStart` and `SubagentStop` observations, make the audit target
child-centric:

```rust
let (session_id, provider_session_id) = match lifecycle.kind() {
    LifecycleEventKind::SubagentStart { agent_id }
    | LifecycleEventKind::SubagentStop { agent_id } => (
        agent_id.clone(),
        Some(lifecycle.identity().session_id().to_owned()),
    ),
    _ => (
        lifecycle.identity().session_id().to_owned(),
        lifecycle.identity().provider_session_id().map(str::to_owned),
    ),
};
```

Do not change the parent-scoped event passed to `LifecycleStore`.

- [ ] **Step 4: Centralize exact identity matching**

In `src/lifecycle_hook.rs`, add and use:

```rust
fn matches_lifecycle_identity(
    session: &SessionTarget,
    identity: &LifecycleIdentity,
) -> bool {
    session.provider == identity.provider()
        && session.session_id == identity.session_id()
        && session.provider_session_id.as_deref() == identity.provider_session_id()
        && session.turn_id.as_deref() == identity.turn_id()
}
```

Replace every direct provider/session/turn predicate in:

- exact outcome lookup;
- provider-decision detection;
- PreToolUse anchor collection;
- interval candidate collection;
- duplicate outcome detection.

Where `src/brain/activity.rs` compares two `SessionTarget`s for recovery
reservations or deduplication, include
`candidate.provider_session_id == current.provider_session_id`.

- [ ] **Step 5: Add failing sibling outcome tests**

In `tests/hook_activity.rs`, after two allowed child decisions:

```rust
run_provider_lifecycle_hook(
    home.path(),
    "codex",
    None,
    &child_post_payload(home.path(), "child-b", "turn-b", "tool-b"),
);

let log = ActivityStore::at(
    home.path().join(".local/state/coding-brain/activity.jsonl"),
)
.read()
.unwrap();
let child_b_outcomes = log.events().iter().filter(|event| {
    event.state == ActivityState::Outcome
        && event.session.as_ref().is_some_and(|session| {
            session.session_id == "child-b"
                && session.provider_session_id.as_deref() == Some("root-1")
        })
}).count();
assert_eq!(child_b_outcomes, 1);
assert!(!log.events().iter().any(|event| {
    event.state == ActivityState::Outcome
        && event.session.as_ref().is_some_and(|session| session.session_id == "child-a")
}));
```

Add negative cases for a sibling tool ID, mismatched provider session, missing
active topology, and replay after `SubagentStop`.

- [ ] **Step 6: Verify fail-safe permission ordering**

Extend `tests/lifecycle_hook_cli.rs::permission_allow_is_suppressed_across_lifecycle_failure`
with a child payload whose `agent_id` has no matching `SubagentStart`.
Assert successful hook process exit, empty stdout, bounded stderr containing a
lifecycle diagnostic, and no delivered allow activity.

Add
`deterministic_child_deny_survives_missing_topology`. Use the same unproven
child identity with a command rejected by the existing deterministic safety
policy. Assert the hook emits exactly one deny envelope, never an allow,
records bounded topology diagnostics, and does not create child lifecycle
state. Keep this behavior distinct from model decisions: an inferred allow
under the same missing topology must produce empty stdout.

Add
`child_permission_at_global_capacity_does_not_evict_or_authorize`. Using a
temporary state root and `LifecycleStore`, record one Codex provider session
plus `MAX_SESSIONS - 1` unrelated active sessions. Record `SubagentStart` for
`child-a` under the existing provider session, then invoke the real permission
hook with the matching child payload. Assert:

```rust
assert!(permission.stdout.is_empty());
assert!(String::from_utf8_lossy(&permission.stderr).contains("capacity"));

let after = lifecycle_store.snapshot().unwrap();
assert_eq!(after.sessions.len(), MAX_SESSIONS);
assert_eq!(
    after.sessions.keys().collect::<BTreeSet<_>>(),
    before.sessions.keys().collect::<BTreeSet<_>>()
);
assert!(!after.sessions.contains_key(&native_key("child-a")));
assert!(!activity_log.events().iter().any(|event| {
    event.kind == ActivityKind::Decision
        && event.session.as_ref().is_some_and(|session| {
            session.session_id == "child-a" && event.state == ActivityState::Delivered
        })
}));
```

Capture `before` after the topology event and before invoking the permission
hook. The test must compare the complete key set, not merely the session count.

- [ ] **Step 7: Run focused integration tests**

Run:

```bash
direnv exec . cargo test --test hook_activity codex_child -- --nocapture
direnv exec . cargo test --test hook_activity interleaved_codex_children -- --nocapture
direnv exec . cargo test --test lifecycle_hook_cli permission_allow_is_suppressed -- --nocapture
direnv exec . cargo test --test lifecycle_hook_cli deterministic_child_deny_survives_missing_topology -- --nocapture
```

Expected: child decisions and outcomes are exact; negative cases abstain.

- [ ] **Step 8: Run the complete quality gates**

Run:

```bash
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo fmt --check
git -c core.whitespace=trailing-space,space-before-tab diff --check
git status --short
```

Expected: all tests pass, Clippy reports no warnings, formatting and whitespace
checks pass (without inheriting the repository's incompatible
`indent-with-non-tab` setting for Rust), and status contains only the intended
identity implementation, tests, research, spec, and plan files.

- [ ] **Step 9: Update Beads completion state**

After fresh verification:

```bash
cd /home/alexander/.beads-planning
bd close codexctl-e9j2 codexctl-3i0a \
  --reason="Exact provider/child identity implemented; concurrent sibling permissions and outcomes verified."
```

- [ ] **Step 10: Commit checkpoint, only with explicit authorization**

```bash
git add crates src tests .internal/research/2026-07-24-codex-subagent-hook-identity.md .internal/specs/2026-07-24-codex-subagent-identity-design.md .internal/plans/2026-07-24-codex-subagent-identity.md
git commit -m "🔒 fix: isolate Codex subagent decisions"
```

## Stress Test Results: Codex Subagent Identity Implementation Plan

### Resolved Decisions

- Keep the legacy `--record-outcome` pipeline outside e9j2; migrate and
  deprecate it separately under `codexctl-vwil`.
- Prove append-only activity compatibility with a real mixed schema-1/2/3
  store test.
- Validate topology cleanup completely before mutation; rejected events must
  not create a claimed provider-session state.
- Bind active topology to both child ID and child turn so delayed events cannot
  attach after child-ID reuse.
- Refresh provider topology leases from accepted child activity and prune
  expired provider/child groups atomically.
- Preserve the 128-state limit; capacity rejects a new child without evicting
  existing state or emitting an authorization.
- Use committed Codex 0.145-shaped fixtures, including the absence of
  `tool_use_id` on child permission callbacks.
- Suppress allows when identity or persistence proof fails, while preserving
  deterministic and provider-policy denials.
- Require provider-global effective session IDs from every adapter that
  populates provider-session linkage.

### Changes Made

- Added mixed-version activity-store coverage.
- Added turn-bound topology proof, schema migration cleanup, atomic mismatch
  handling, retention-group behavior, and capacity coverage.
- Replaced fabricated child callback payloads with committed contract fixtures.
- Refined the failure policy so identity failures cannot authorize execution
  without weakening fail-closed denials.
- Added an explicit adapter contract for provider-global effective identity.

### Deferred / Parking Lot

- `--record-outcome` deprecation and detailed telemetry/report migration remain
  in `codexctl-vwil`.
- Claude and Antigravity linkage remain absent until their child identity
  contracts provide sufficient evidence.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Future provider schema changes or parent-local child IDs
  must fail validation or receive an explicit collision-free adapter mapping;
  they must never silently weaken topology proof.
