# Concurrent Codex Permission Replay Identity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Let distinct concurrent permission requests persist and receive independent provider responses while exact same-turn replays remain fail-safe duplicates.

**Architecture:** Provider adapters derive a stable SHA-256 request key from a versioned, length-prefixed tuple of provider, optional tool ID, exact tool name, and canonical parsed input. Lifecycle events carry that key; Codex and Claude track a bounded per-turn disposition map, while Antigravity retains its stricter invocation-step guard. The permission hook continues to persist lifecycle and activity state before emitting an allow response.

**Tech Stack:** Rust 2024 workspace, `serde`/`serde_json`, `sha2`, filesystem-locked lifecycle and activity stores, built-in Rust test framework.

## Global Constraints

- Distinct provider, session, linked-child, turn, tool ID, tool name, or exact input evidence must never share permission authority.
- Byte-identical callbacks without distinct provider tool IDs remain duplicates and require native manual authorization.
- A request may transition from `Decided` to `NeedsInput` only for fail-safe compensation; `NeedsInput` to `Decided` is rejected.
- Per-turn generic keyed replay state is capped at exactly 64 request keys and fails closed as `AmbiguousTurn`.
- `SessionStart(Compact)` preserves keyed replay state; turn replacement, Stop, non-compact session reset, and child removal clear it.
- Antigravity's invocation-step replay map remains authoritative in addition to the shared composite key.
- Lifecycle and terminal activity persistence must succeed before an allow response is emitted.
- The lifecycle schema version remains 3; new serialized fields require Serde defaults and conservative rollback behavior.
- Raw command and tool input must never appear in lifecycle request keys or snapshots.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Derive and carry composite permission request keys

**Files:**
- Modify: `src/provider_hooks/mod.rs:53-62,126-149`
- Modify: `src/provider_hooks/codex.rs:25-88`
- Modify: `src/provider_hooks/claude.rs:28-107`
- Modify: `src/provider_hooks/antigravity.rs:40-130`
- Modify: `crates/coding-brain-core/src/lifecycle/input.rs:78-100,189-310`
- Modify: `src/brain/permission_hook.rs:397-413,500-850`
- Test: `src/provider_hooks/mod.rs`
- Test: `src/provider_hooks/codex.rs`
- Test: `src/provider_hooks/claude.rs`
- Test: `src/provider_hooks/antigravity.rs`
- Test: `crates/coding-brain-core/src/lifecycle/input.rs`

**Interfaces:**
- Consumes: parsed `AgentProvider`, optional `tool_use_id`, exact `tool_name`, and parsed `serde_json::Value` tool input.
- Produces: `permission_request_key(provider: AgentProvider, tool_use_id: Option<&str>, tool_name: &str, tool_input: &Value) -> String`; `PermissionHookRequest::request_key: String`; `LifecycleEventKind::PermissionRequest { disposition, request_key: Option<String> }`; `LifecycleEvent::permission_with_request_key(identity, disposition, request_key)`.

**Acceptance Criteria:**
- Every Codex, Claude, and Antigravity permission request receives a stable 64-character lowercase SHA-256 request key.
- Object-key ordering and raw JSON whitespace do not change the key.
- Provider, tool ID, tool name, or exact input differences change the key.
- A reused tool ID with different input cannot collapse two commands.
- Serialized lifecycle state contains the digest but no raw command or tool input.
- Legacy unkeyed lifecycle constructors and payloads remain readable.

- [ ] **Step 0a: Add the failing concurrent distinct-request reproduction**

Before changing production code, add this test to
`src/brain/permission_hook.rs`:

```rust
fn run_concurrent_approvals(
    payloads: &[String; 2],
    lifecycle: &LifecycleStore,
    activity: &ActivityStore,
    config: &BrainConfig,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let (ready_tx, ready_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut releases = Vec::new();
        for payload in payloads {
            let ready_tx = ready_tx.clone();
            let (release_tx, release_rx) = mpsc::sync_channel(0);
            releases.push(release_tx);
            handles.push(scope.spawn(move || {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                run_with_gate_and_stores(
                    Cursor::new(payload),
                    &mut stdout,
                    &mut stderr,
                    Some(config),
                    BrainGateMode::Auto,
                    lifecycle,
                    Some(activity),
                    |_, _| {
                        ready_tx.send(()).unwrap();
                        release_rx
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                        Ok(suggestion(RuleAction::Approve, 0.9))
                    },
                );
                (stdout, stderr)
            }));
        }
        drop(ready_tx);
        for _ in payloads {
            ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for release in releases {
            release.send(()).unwrap();
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    })
}

#[test]
fn concurrent_distinct_codex_permissions_both_deliver() {
    let _guard = crate::config::HOME_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let _restore_home = set_test_home(home.path());
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    let config = enabled_config();
    let payloads = [
        payload_with_command("gh run view --job 89897083575 --log"),
        payload_with_command("gh run view --job 89897083607 --log"),
    ];
    let results = run_concurrent_approvals(&payloads, &lifecycle, &activity, &config);

    for (stdout, stderr) in &results {
        assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(stderr));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(stdout).unwrap()
                ["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
    }
    let events = activity.read().unwrap().events().to_vec();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.state == ActivityState::Allowed)
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.state == ActivityState::Delivered)
            .count(),
        2
    );
    assert!(!events.iter().any(|event| event.state == ActivityState::Error));
}
```

Add `std::sync::mpsc` and `std::time::Duration` to the test module imports.

- [ ] **Step 0b: Run the reproduction and verify RED**

Run:

```bash
direnv exec . cargo test brain::permission_hook::tests::concurrent_distinct_codex_permissions_both_deliver -- --exact --nocapture
```

Expected: one callback reports `lifecycle event was ignored: Duplicate`, emits
no response, and records Error.

- [ ] **Step 1: Add failing shared key-derivation tests**

Add a `#[cfg(test)] mod tests` in `src/provider_hooks/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_request_key_is_canonical_and_scope_complete() {
        let first = permission_request_key(
            AgentProvider::Codex,
            Some("call-1"),
            "Bash",
            &json!({"command": "cargo test", "timeout": 30}),
        );
        let reordered = permission_request_key(
            AgentProvider::Codex,
            Some("call-1"),
            "Bash",
            &serde_json::from_str(r#"{"timeout":30,"command":"cargo test"}"#).unwrap(),
        );
        assert_eq!(first, reordered);
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));

        for changed in [
            permission_request_key(
                AgentProvider::Claude,
                Some("call-1"),
                "Bash",
                &json!({"command": "cargo test", "timeout": 30}),
            ),
            permission_request_key(
                AgentProvider::Codex,
                Some("call-2"),
                "Bash",
                &json!({"command": "cargo test", "timeout": 30}),
            ),
            permission_request_key(
                AgentProvider::Codex,
                Some("call-1"),
                "Write",
                &json!({"command": "cargo test", "timeout": 30}),
            ),
            permission_request_key(
                AgentProvider::Codex,
                Some("call-1"),
                "Bash",
                &json!({"command": "cargo clippy", "timeout": 30}),
            ),
        ] {
            assert_ne!(first, changed);
        }
        assert!(!first.contains("cargo test"));
    }
}
```

- [ ] **Step 2: Run the shared key test and verify RED**

Run:

```bash
direnv exec . cargo test provider_hooks::tests::permission_request_key_is_canonical_and_scope_complete -- --exact
```

Expected: compile failure because `permission_request_key` does not exist.

- [ ] **Step 3: Implement the minimal canonical SHA-256 helper**

In `src/provider_hooks/mod.rs`, add:

```rust
use sha2::{Digest, Sha256};

const PERMISSION_REQUEST_KEY_DOMAIN: &[u8] =
    b"coding-brain:permission-request-key:v1";

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

pub(super) fn permission_request_key(
    provider: AgentProvider,
    tool_use_id: Option<&str>,
    tool_name: &str,
    tool_input: &Value,
) -> String {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, PERMISSION_REQUEST_KEY_DOMAIN);
    hash_part(&mut hasher, provider.as_str().as_bytes());
    match tool_use_id {
        Some(tool_use_id) => {
            hash_part(&mut hasher, b"tool-use-id");
            hash_part(&mut hasher, tool_use_id.as_bytes());
        }
        None => hash_part(&mut hasher, b"no-tool-use-id"),
    }
    hash_part(&mut hasher, tool_name.as_bytes());
    hash_part(
        &mut hasher,
        &serde_json::to_vec(tool_input).expect("serde_json::Value is serializable"),
    );
    format!("{:x}", hasher.finalize())
}
```

If `AgentProvider::as_str` is not public, match the three variants locally and test the exact domain strings rather than changing the provider API.

- [ ] **Step 4: Run the shared key test and verify GREEN**

Run the exact command from Step 2.

Expected: `1 passed; 0 failed`.

- [ ] **Step 5: Add failing adapter tests for composite population**

Extend each adapter's existing parser tests to assert:

```rust
let parsed = parse_permission(/* existing fixture */).unwrap();
assert_eq!(parsed.request_key.len(), 64);
assert!(!parsed.request_key.contains("cargo test"));
```

For Codex and Claude, clone the fixture, reuse the same `tool_use_id`, change only the command, and assert different keys. For Codex, remove `tool_use_id` from two otherwise equal fixtures and assert equal keys. For Antigravity, reuse `stepIdx`, change only `toolCall.args.CommandLine`, and assert different keys.

- [ ] **Step 6: Run adapter tests and verify RED**

Run:

```bash
direnv exec . cargo test provider_hooks::
```

Expected: compile failure because `PermissionHookRequest` has no `request_key`.

- [ ] **Step 7: Thread exact input through all provider adapters**

Add `pub request_key: String` to `PermissionHookRequest` and a `request_key: String` argument to `permission_request`. Compute it before moving each adapter's `tool_input`/`args` into command extraction:

```rust
let request_key =
    permission_request_key(AgentProvider::Codex, tool_use_id.as_deref(), &tool_name, &input.tool_input);
```

Use the same shape for Claude's `tool_input` and Antigravity's `args`. Pass the key into `permission_request`. Do not hash redacted or normalized command text.

- [ ] **Step 8: Add failing keyed lifecycle-event tests**

In `crates/coding-brain-core/src/lifecycle/input.rs`, add tests that construct a keyed permission, round-trip JSON, and assert that legacy unkeyed events still parse:

```rust
#[test]
fn keyed_permission_round_trips_without_raw_input() {
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "session-1".into(),
        Some("turn-1".into()),
        None,
        PathBuf::from("/work/coding-brain"),
    )
    .unwrap();
    let event = LifecycleEvent::permission_with_request_key(
        identity,
        PermissionDisposition::Decided,
        "a".repeat(64),
    )
    .unwrap();
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(encoded.contains(&"a".repeat(64)));
    assert!(!encoded.contains("cargo test"));
    assert_eq!(serde_json::from_str::<LifecycleEvent>(&encoded).unwrap(), event);
}
```

- [ ] **Step 9: Run the lifecycle input test and verify RED**

Run:

```bash
direnv exec . cargo test lifecycle::input::tests::keyed_permission_round_trips_without_raw_input -- --exact
```

Expected: compile failure because `permission_with_request_key` does not exist.

- [ ] **Step 10: Add optional request keys to permission lifecycle events**

Change the event variant and add a keyed constructor while retaining the legacy constructor:

```rust
PermissionRequest {
    disposition: PermissionDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_key: Option<String>,
},
```

```rust
pub fn permission(
    identity: LifecycleIdentity,
    disposition: PermissionDisposition,
) -> Result<Self, LifecycleInputError> {
    Self::permission_from_parts(identity, disposition, None)
}

pub fn permission_with_request_key(
    identity: LifecycleIdentity,
    disposition: PermissionDisposition,
    request_key: String,
) -> Result<Self, LifecycleInputError> {
    validate_id("request_key", &request_key)?;
    Self::permission_from_parts(identity, disposition, Some(request_key))
}

fn permission_from_parts(
    identity: LifecycleIdentity,
    disposition: PermissionDisposition,
    request_key: Option<String>,
) -> Result<Self, LifecycleInputError> {
    require_turn(&identity)?;
    Ok(Self {
        identity,
        kind: LifecycleEventKind::PermissionRequest {
            disposition,
            request_key,
        },
        turn_initial_step: None,
    })
}
```

Update permission pattern matches to use `{ disposition, .. }` or `{ .. }`.

- [ ] **Step 11: Pass request keys through every permission record**

Change `record_permission` to accept `request_key: &str` and call:

```rust
LifecycleEvent::permission_with_request_key(
    identity.clone(),
    disposition,
    request_key.to_owned(),
)
```

At every call in `src/brain/permission_hook.rs`, pass `&request.request_key`, including deterministic decisions, abstentions, automatic allows, denials, and `Decided -> NeedsInput` compensation.

- [ ] **Step 12: Run Task 1 focused tests**

Run:

```bash
direnv exec . cargo test provider_hooks::
direnv exec . cargo test lifecycle::input::tests
direnv exec . cargo test brain::permission_hook::tests
```

Expected: all selected tests pass with no warnings or panics.

- [ ] **Step 13: Review checkpoint**

Run:

```bash
git diff --check
git diff --stat
```

Expected: only Task 1 files plus the approved spec/plan are changed. Keep changes uncommitted until the user explicitly authorizes a commit.

---

### Task 2: Enforce bounded non-adjacent replay protection per active turn

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs:1-180,429-565,648-660`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs:368-462`
- Test: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Test: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Consumes: `LifecycleEventKind::PermissionRequest.request_key` from Task 1.
- Produces: `MAX_PERMISSION_REQUESTS_PER_TURN: usize = 64`; `SessionLifecycleState::permission_request_events: BTreeMap<String, u8>`; keyed replay decisions enforced by `LifecycleSnapshot::apply`.

**Acceptance Criteria:**
- Different request keys in one Codex or Claude turn apply independently.
- A keyed replay remains `Duplicate` after another permission event intervenes.
- `Decided -> NeedsInput` applies; `NeedsInput -> Decided` is `Duplicate`.
- Antigravity continues to use its invocation-step replay proof.
- The sixty-fifth distinct generic key is rejected as `AmbiguousTurn` without consuming a sequence.
- Compact continuity retains replay state; turn replacement, Stop, non-compact reset, and child removal clear it.
- Legacy snapshots load with an empty map; malformed or oversized maps are rejected.

- [ ] **Step 1: Add failing projection tests for distinct keys and non-adjacent replay**

Add a helper:

```rust
fn keyed_permission(
    provider: AgentProvider,
    turn: &str,
    disposition: PermissionDisposition,
    key: &str,
) -> LifecycleEvent {
    LifecycleEvent::permission_with_request_key(
        LifecycleIdentity::try_new(
            provider,
            "session-1".into(),
            Some(turn.into()),
            None,
            "/work/coding-brain".into(),
        )
        .unwrap(),
        disposition,
        key.into(),
    )
    .unwrap()
}
```

Add:

```rust
#[test]
fn keyed_permissions_are_independent_and_replay_safe() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(
        snapshot.apply(keyed_permission(AgentProvider::Codex, "turn-1", PermissionDisposition::Decided, "key-a"), 1),
        ApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply(keyed_permission(AgentProvider::Codex, "turn-1", PermissionDisposition::Decided, "key-b"), 2),
        ApplyOutcome::Applied
    );
    let before = snapshot.clone();
    assert_eq!(
        snapshot.apply(keyed_permission(AgentProvider::Codex, "turn-1", PermissionDisposition::Decided, "key-a"), 3),
        ApplyOutcome::Ignored(IgnoreReason::Duplicate)
    );
    assert_eq!(snapshot.next_sequence, before.next_sequence);
    assert_eq!(snapshot.sessions[&session_key()].latest_sequence, before.sessions[&session_key()].latest_sequence);
}
```

Use valid 64-character test keys in the final code.

- [ ] **Step 2: Add failing transition, capacity, and cleanup tests**

Add separate tests for:

```rust
// Decided -> NeedsInput applies; the reverse order rejects Decided.
// Keys 0..64 apply; key 64 returns AmbiguousTurn and next_sequence is unchanged.
// SessionStart(Compact) keeps the map and rejects a replay.
// A new prompt turn, Stop, and non-compact SessionStart leave the map empty.
// Linked Codex child state is removed by SubagentStop/root cleanup.
// Antigravity's existing replay/reversal tests remain unchanged.
```

Use one behavior per test with explicit `ApplyOutcome` and map-size assertions.

- [ ] **Step 3: Run projection tests and verify RED**

Run:

```bash
direnv exec . cargo test lifecycle::projection::tests::keyed_permissions -- --nocapture
```

Expected: at least the non-adjacent replay assertion fails because only `last_signature` is tracked.

- [ ] **Step 4: Add bounded keyed replay state**

In `projection.rs`, add:

```rust
pub const MAX_PERMISSION_REQUESTS_PER_TURN: usize = 64;
const PERMISSION_DECIDED_BIT: u8 = 1 << 0;
const PERMISSION_NEEDS_INPUT_BIT: u8 = 1 << 1;
const PERMISSION_BITS: u8 = PERMISSION_DECIDED_BIT | PERMISSION_NEEDS_INPUT_BIT;
```

Add to `SessionLifecycleState` and initialize/clear it:

```rust
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub permission_request_events: BTreeMap<String, u8>,
```

`clear_transient_status`, Stop, and exact current-turn replacement clear the map. `SessionStart(Compact)` does not call `clear_transient_status`, so it retains the map.

- [ ] **Step 5: Apply keyed replay checks after exact turn correlation**

Add helpers:

```rust
fn permission_event(event: &LifecycleEvent) -> Option<(&str, u8)> {
    if event.identity().provider() == AgentProvider::Antigravity {
        return None;
    }
    match event.kind() {
        LifecycleEventKind::PermissionRequest {
            disposition,
            request_key: Some(request_key),
        } => Some((
            request_key,
            match disposition {
                PermissionDisposition::Decided => PERMISSION_DECIDED_BIT,
                PermissionDisposition::NeedsInput => PERMISSION_NEEDS_INPUT_BIT,
            },
        )),
        _ => None,
    }
}
```

After the existing current-turn/Antigravity correlation succeeds but before consuming a sequence:

```rust
if let Some((request_key, bit)) = permission_event(&event) {
    let previous = state
        .permission_request_events
        .get(request_key)
        .copied()
        .unwrap_or(0);
    let unsafe_permission_reversal =
        bit == PERMISSION_DECIDED_BIT && previous & PERMISSION_NEEDS_INPUT_BIT != 0;
    if previous & bit != 0 || unsafe_permission_reversal {
        return state.ignore(IgnoreReason::Duplicate);
    }
    if previous == 0
        && state.permission_request_events.len() >= MAX_PERMISSION_REQUESTS_PER_TURN
    {
        return state.ignore(IgnoreReason::AmbiguousTurn);
    }
    state
        .permission_request_events
        .insert(request_key.to_owned(), previous | bit);
}
```

Ensure every branch that changes `current_turn` to a different value clears `permission_request_events` first. Do not clear it for a repeated same-turn prompt.

- [ ] **Step 6: Run projection tests and verify GREEN**

Run:

```bash
direnv exec . cargo test lifecycle::projection::tests
```

Expected: all projection tests pass, including existing Antigravity and concurrent-child cases.

- [ ] **Step 7: Add failing snapshot validation and compatibility tests**

In `store.rs`, add tests that:

```rust
// Serialize a pre-change JSON snapshot with no permission_request_events,
// read it, and assert the map defaults empty.
// Insert 65 keys and assert the snapshot is invalid.
// Insert an empty/oversized/non-hex key and assert invalid.
// Insert a zero or unknown disposition bit and assert invalid.
// Put a valid key on a closed or current-turn-less state and assert invalid.
```

- [ ] **Step 8: Run store tests and verify RED**

Run:

```bash
direnv exec . cargo test lifecycle::store::tests
```

Expected: malformed keyed state is accepted before validation is implemented.

- [ ] **Step 9: Validate serialized keyed replay state**

Extend `valid_snapshot_shape`:

```rust
let permission_events_valid = state.permission_request_events.len()
    <= MAX_PERMISSION_REQUESTS_PER_TURN
    && state.permission_request_events.iter().all(|(request_key, bits)| {
        request_key.len() == 64
            && request_key.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && *bits != 0
            && *bits & !PERMISSION_BITS == 0
    })
    && (state.permission_request_events.is_empty()
        || state.turn_open && state.current_turn.is_some());
```

Require `permission_events_valid` alongside `antigravity_state_valid`. Do not advance `LIFECYCLE_SCHEMA_VERSION`.

- [ ] **Step 10: Run Task 2 focused tests**

Run:

```bash
direnv exec . cargo test lifecycle::projection::tests
direnv exec . cargo test lifecycle::store::tests
```

Expected: all selected tests pass.

- [ ] **Step 11: Review checkpoint**

Run:

```bash
git diff --check
git diff --stat
```

Expected: only planned lifecycle/provider/hook files and internal spec/plan files are changed. Keep changes uncommitted pending explicit authorization.

---

### Task 3: Prove concurrent hook delivery and exact replay suppression

**Files:**
- Modify: `src/brain/permission_hook.rs:1000-1950` (tests only unless the regression exposes a root-cause defect)
- Test: `src/brain/permission_hook.rs`

**Interfaces:**
- Consumes: shared `PermissionHookRequest::request_key`, keyed lifecycle projection, `run_with_gate_and_stores`, `LifecycleStore`, and `ActivityStore`.
- Produces: an end-to-end regression proving two in-flight callbacks reach independent terminal/delivery states and an exact replay emits no response.

**Acceptance Criteria:**
- Two callbacks with the same Codex session/turn and different commands are simultaneously in flight before lifecycle persistence.
- Both callbacks emit `allow`, and both activity IDs reach exactly `Observed`, `Evaluating`, `Allowed`, and `Delivered`.
- Neither concurrent activity reaches Error.
- Replaying either exact payload emits no response, records Error, and does not advance lifecycle sequence.
- Existing sequential Codex, linked-child, Claude, Antigravity, deterministic-deny, and persistence-failure tests remain green.
- Workspace test, clippy, formatting, and build gates pass.

- [ ] **Step 1: Extend the bounded concurrent regression with exact replay**

Reuse the exact `run_concurrent_approvals` helper produced by Task 1 and add:

```rust
#[test]
fn concurrent_distinct_codex_permissions_deliver_and_exact_replay_fails_safe() {
    let _guard = crate::config::HOME_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let _restore_home = set_test_home(home.path());
    let temp = tempfile::tempdir().unwrap();
    let lifecycle = LifecycleStore::at(temp.path().join("lifecycle"));
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    let config = enabled_config();
    let payloads = [
        payload_with_command("gh run view --job 89897083575 --log"),
        payload_with_command("gh run view --job 89897083607 --log"),
    ];
    let results = run_concurrent_approvals(&payloads, &lifecycle, &activity, &config);

    for (stdout, stderr) in &results {
        assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(stderr));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(stdout).unwrap()
                ["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
    }

    let before_replay = lifecycle.read().unwrap().snapshot.unwrap();
    let events = activity.read().unwrap().events().to_vec();
    let allowed_ids = events
        .iter()
        .filter(|event| event.state == ActivityState::Allowed)
        .map(|event| event.activity_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(allowed_ids.len(), 2);
    for activity_id in &allowed_ids {
        assert_eq!(
            events
                .iter()
                .filter(|event| &event.activity_id == activity_id)
                .map(|event| event.state)
                .collect::<Vec<_>>(),
            [
                ActivityState::Observed,
                ActivityState::Evaluating,
                ActivityState::Allowed,
                ActivityState::Delivered,
            ]
        );
    }

    let mut replay_stdout = Vec::new();
    let mut replay_stderr = Vec::new();
    run_with_gate_and_stores(
        Cursor::new(&payloads[0]),
        &mut replay_stdout,
        &mut replay_stderr,
        Some(&config),
        BrainGateMode::Auto,
        &lifecycle,
        Some(&activity),
        |_, _| Ok(suggestion(RuleAction::Approve, 0.9)),
    );
    assert!(replay_stdout.is_empty());
    assert!(String::from_utf8(replay_stderr).unwrap().contains("Duplicate"));
    let after_replay = lifecycle.read().unwrap().snapshot.unwrap();
    assert_eq!(after_replay.next_sequence, before_replay.next_sequence);
    assert_eq!(
        activity
            .read()
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.state == ActivityState::Error)
            .count(),
        1
    );
}
```

Adjust only ownership syntax required by the compiler; retain the bounded
rendezvous and every behavioral assertion.

- [ ] **Step 2: Run the integrated regression**

Run:

```bash
direnv exec . cargo test brain::permission_hook::tests::concurrent_distinct_codex_permissions_deliver_and_exact_replay_fails_safe -- --exact --nocapture
```

Expected after Tasks 1-2: the test passes. The distinct-request half was
already observed RED in Task 1 Step 0b, and the non-adjacent replay behavior was
already observed RED in Task 2 Step 3.

- [ ] **Step 3: Make only evidence-driven integration corrections**

If the new test fails after Tasks 1-2, trace the failing boundary before editing production code:

```text
request key derivation -> lifecycle lock/apply -> terminal activity append
-> response write -> delivery append
```

Change only the boundary proven incorrect. Do not add retries, relax persistence gates, or serialize hooks in-process.

- [ ] **Step 4: Run the regression and verify GREEN**

Run the exact command from Step 2.

Expected: `1 passed; 0 failed`; both concurrent responses are `allow`, and the replay emits no response.

- [ ] **Step 5: Run focused provider and lifecycle regression suites**

Run:

```bash
direnv exec . cargo test provider_hooks::
direnv exec . cargo test lifecycle::
direnv exec . cargo test brain::permission_hook::tests
```

Expected: all tests pass, including existing linked Codex child, Claude, Antigravity, deterministic deny, and persistence failure cases.

- [ ] **Step 6: Format and inspect the surgical diff**

Run:

```bash
direnv exec . cargo fmt
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
git diff --stat
```

Expected: formatting succeeds; normalized whitespace check is empty; every changed production line traces to request identity, replay projection, or hook plumbing.

- [ ] **Step 7: Run full workspace quality gates**

Run:

```bash
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo fmt --check
direnv exec . cargo build
```

Expected: every command exits 0 with no warnings.

- [ ] **Step 8: Run review and completion-verification gates**

Invoke `beads-superpowers:requesting-code-review` against the final diff, address
any verified actionable findings through the same TDD cycle, then invoke
`beads-superpowers:verification-before-completion` and rerun any evidence it
requires. Do not close work based on earlier or partial command output.

- [ ] **Step 9: Update durable task status and report**

Close child implementation beads only after their acceptance criteria and the
review/verification gates pass. Close `codexctl-lxxe` only after all full gates
pass. Report changed files, exact verification commands/results, Beads status,
both retained path-scoped restore stashes, and that commit, stash removal,
push, and publication remain awaiting explicit authorization.

## Stress Test Results: Concurrent Permission Replay Implementation Plan

### Resolved Decisions

- Task boundaries: keep request-key plumbing, replay projection, and integrated
  verification as three independently reviewable tasks.
- TDD ordering: observe the real concurrent failure before production edits,
  then observe non-adjacent replay and malformed-state failures before their
  respective implementations.
- Interfaces: use `String` in the internal hook request and optional serialized
  event keys only for backward compatibility; avoid a one-use wrapper type.
- Provider edges: derive keys for all policies without changing existing
  provider parsing, linked identity, turn fallback, or Antigravity topology.
- State validation: keyed replay state is valid only on an open state with a
  current turn.
- Concurrency proof: use timeout-bounded channel rendezvous rather than an
  indefinitely blocking barrier.
- Verification: use deterministic focused suites and full workspace gates,
  without timing loops.
- Handoff: require code-review and completion-verification workflows before
  closing durable work; preserve stashes and external-change consent.

### Changes Made

- Added closed/turnless keyed-state corruption cases and validation.
- Replaced both barrier examples with a shared five-second channel rendezvous.
- Added explicit requesting-code-review and verification-before-completion
  gates before issue closure.

### Deferred / Parking Lot

- No additional provider behavior or schema migration was added.
- Commit, stash removal, push, and publication remain outside the plan without
  explicit user authorization.

### Confidence Assessment

- Overall: High
- Areas of concern: the exact Rust ownership details of the scoped-thread test
  helper must be compiler-checked during RED, but its synchronization and
  timeout behavior are fully specified.
