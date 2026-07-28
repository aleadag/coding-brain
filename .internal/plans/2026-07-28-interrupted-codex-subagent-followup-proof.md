# Interrupted Codex Subagent Follow-up Proof Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Allow an exactly proven Codex child to continue on a fresh follow-up turn after interruption or a permissionless intermediate turn without weakening permission fail-closed behavior.

**Architecture:** Extend the existing locked `LifecycleStore::reprove_codex_subagent_at` transition so exact bounded transcript evidence can replace either a stale active edge or a stopped-child tombstone. Active replacement removes only the child's old projected subtree before reinserting the fresh parent-child-turn edge; the ordinary permission event remains the final authorization gate.

**Tech Stack:** Rust 2024 workspace, Serde JSON lifecycle snapshots, filesystem-backed transcript evidence, Cargo unit and process integration tests.

## Global Constraints

- Parent `sub_agent_activity(kind = "interacted")` is never authority.
- Preserve every existing fixed-file, regular-file, 1 MiB head, 8 MiB tail, identity, path, timestamp, sequence, and five-second future-skew check.
- A transcript cannot bootstrap a never-proven child or transfer a child between parent/provider topologies.
- Active refresh replaces one edge without consuming growth capacity; stopped recovery retains the existing capacity check.
- Store, parsing, ordering, or persistence failure emits no allow response; native Codex permission handling remains authoritative.
- Do not change Claude or Antigravity behavior, lifecycle schema, configuration, or user-visible diagnostics.
- Do not commit, push, or publish without explicit user authorization.

---

### Task 1: Refresh stale active Codex child proof atomically

**Files:**

- Modify: `crates/coding-brain-core/src/lifecycle/store.rs:116`
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs:757`
- Test: `crates/coding-brain-core/src/lifecycle/store.rs:1569`
- Test: `tests/hook_activity.rs:800`

**Interfaces:**

- Consumes: `LifecycleStore::reprove_codex_subagent_at(&LifecycleIdentity, &CodexResumeEvidence, u64) -> Result<ApplyOutcome, StoreError>`, `LifecycleSnapshot::remove_linked_children`, and existing active/stopped topology helpers.
- Produces: unchanged public `LifecycleStore::reprove_codex_subagent` behavior with a new valid transition from stale active turn A to transcript-proven turn B.

**Acceptance Criteria:**

- Exact newer evidence replaces a stale active child edge and consumes one lifecycle sequence.
- The old child session and its descendants are removed; the parent, an active sibling and its projection, and the fresh child edge remain.
- The interrupted permission-hook trajectory fails with `SubagentTurnMismatch` before implementation and delivers the turn-B decision afterward without `AmbiguousTurn`.
- Exact-turn duplicates remain duplicates without consuming a sequence.
- Stale/replayed, cross-parent, mismatched identity/path/turn, future, and unproven evidence makes no state change.
- Active replacement succeeds at active-map capacity because it has no net growth; stopped recovery still rejects growth at capacity.
- Existing stopped-child and concurrent-stop tests remain green.

- [ ] **Step 1: Add a failing active-follow-up store regression**

Add an `active_store` setup beside `stopped_store`, then add a test that creates old child projection state and a nested descendant before re-proof:

```rust
fn active_store(
    transcript_path: &Path,
) -> (
    LifecycleStore,
    super::super::LifecycleIdentity,
    CodexResumeEvidence,
) {
    let store = store();
    assert_eq!(
        store.record_at(subagent_start("root-1", "child-1", "turn-1"), 1_000),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(linked_tool("child-1", "root-1", "turn-1"), 1_500),
        Ok(ApplyOutcome::Applied)
    );
    (
        store,
        linked_identity(
            AgentProvider::Codex,
            "child-1",
            "root-1",
            "turn-2",
            transcript_path,
        ),
        resume_evidence("child-1", "root-1", "turn-2", transcript_path, 2_500),
    )
}

#[test]
fn exact_newer_codex_resume_evidence_replaces_stale_active_turn() {
    let path = transcript_path("rollout-child-1.jsonl");
    let (store, identity, evidence) = active_store(&path);
    assert_eq!(
        store.record_at(subagent_start("root-1", "sibling", "sibling-turn"), 1_550),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(linked_tool("sibling", "root-1", "sibling-turn"), 1_575),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(
            linked_subagent_start("child-1", "root-1", "turn-1", "grandchild"),
            1_600,
        ),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(linked_tool("grandchild", "root-1", "turn-1"), 1_700),
        Ok(ApplyOutcome::Applied)
    );

    assert_eq!(
        store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
        Ok(ApplyOutcome::Applied)
    );

    let snapshot = store.read().unwrap().snapshot.unwrap();
    let parent = &snapshot.sessions[&key("root-1")];
    assert_eq!(parent.active_subagents["child-1"].turn_id, "turn-2");
    assert_eq!(
        parent.active_subagents["sibling"].turn_id,
        "sibling-turn"
    );
    assert!(!snapshot.sessions.contains_key(&key("child-1")));
    assert!(!snapshot.sessions.contains_key(&key("grandchild")));
    assert!(snapshot.sessions.contains_key(&key("sibling")));
    assert_eq!(snapshot.next_sequence, 8);
    assert_eq!(
        store.record_at(permission(identity), 3_001),
        Ok(ApplyOutcome::Applied)
    );
}
```

- [ ] **Step 2: Add the failing interrupted permission-hook regression**

Add this test beside `resumed_codex_child_permission_is_reproved_and_delivered`.
Use `child_pre_payload` on turn A so it also catches a fix that updates only the
parent edge but leaves the child projection open:

```rust
#[test]
fn interrupted_codex_child_permission_refreshes_active_turn_and_is_delivered() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-b",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            "turn-b",
            &transcript,
        ),
    );

    assert!(permission.status.success());
    assert!(
        !permission.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&permission.stderr)
    );
    assert!(!String::from_utf8_lossy(&permission.stderr).contains("AmbiguousTurn"));
    assert!(!String::from_utf8_lossy(&permission.stderr).contains("SubagentTurnMismatch"));
    assert_delivered_child_decision(home.path(), "child-a", "turn-b");
}
```

Extract the repeated delivered-decision activity assertion from
`resumed_codex_child_permission_is_reproved_and_delivered` into:

```rust
fn assert_delivered_child_decision(home: &Path, child_id: &str, turn_id: &str) {
    let events = activity(home).read().unwrap().events().to_vec();
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event.kind == ActivityKind::Decision
                    && event.session.as_ref().is_some_and(|session| {
                        session.session_id == child_id
                            && session.provider_session_id.as_deref() == Some("root-1")
                            && session.turn_id.as_deref() == Some(turn_id)
                    })
            })
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
```

- [ ] **Step 3: Run both regressions and verify the root-cause failures**

Run:

```bash
direnv exec . cargo test -p coding-brain-core \
  lifecycle::store::tests::exact_newer_codex_resume_evidence_replaces_stale_active_turn \
  -- --exact
direnv exec . cargo test --test hook_activity \
  interrupted_codex_child_permission_refreshes_active_turn_and_is_delivered \
  -- --exact
```

Expected: both FAIL because stale active proof causes
`Ignored(SubagentTurnMismatch)`; the permission hook emits no allow response.

- [ ] **Step 4: Implement the minimal stale-active transition**

Keep the generic projection helper private and add a narrow crate-internal
entry point:

```rust
pub(crate) fn remove_linked_subagent_projection(
    &mut self,
    provider: AgentProvider,
    child_id: &str,
) {
    self.remove_linked_children(provider, child_id, false, None);
}
```

In `reprove_codex_subagent_at`, preserve the current cross-topology and
exact-turn decisions, validate common evidence before mutation, and add this
active branch before the stopped-owner lookup:

```rust
let evidence_matches_identity = evidence.child_session_id == identity.session_id()
    && evidence.provider_session_id == parent_id
    && evidence.turn_id == turn_id
    && requested_path.as_deref() == Some(transcript_path)
    && canonical_identity_path.as_deref()
        == Some(evidence.canonical_transcript_path.as_path())
    && canonical_identity_is_file
    && evidence.started_at_ms <= received_at_ms.saturating_add(5_000);

let active = snapshot.sessions.iter().find_map(|(storage_key, state)| {
    (AgentSessionKey::from_storage_key(storage_key)
        .is_some_and(|key| key.provider == AgentProvider::Codex))
    .then(|| {
        state
            .active_subagents
            .get(identity.session_id())
            .cloned()
            .map(|active| (storage_key.clone(), active))
    })
    .flatten()
});
if let Some((owner_key, active)) = active {
    if !snapshot.topology_contains_session(AgentProvider::Codex, parent_id, &owner_key) {
        return ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch);
    }
    if active.turn_id == turn_id {
        return ApplyOutcome::Ignored(IgnoreReason::Duplicate);
    }
    if !evidence_matches_identity
        || evidence.started_at_ms <= active.received_at_ms
    {
        return ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent);
    }
    if snapshot.next_sequence == 0 || snapshot.next_sequence >= u64::MAX - 1 {
        return ApplyOutcome::Ignored(IgnoreReason::SequenceExhausted);
    }

    let sequence = snapshot.next_sequence;
    snapshot.next_sequence += 1;
    snapshot.remove_linked_subagent_projection(
        AgentProvider::Codex,
        identity.session_id(),
    );
    let parent = snapshot
        .sessions
        .get_mut(&owner_key)
        .expect("validated Codex topology owner");
    parent.active_subagents.insert(
        identity.session_id().to_owned(),
        ActiveSubagentState {
            started_sequence: sequence,
            received_at_ms,
            turn_id: turn_id.to_owned(),
        },
    );
    parent.latest_sequence = sequence;
    parent.latest_received_at_ms = received_at_ms;
    parent.ignored_reason = None;
    return ApplyOutcome::Applied;
}
```

Compute `requested_path`, `canonical_identity_path`, and
`canonical_identity_is_file` once before this branch, then reuse them in the
shared predicate and stopped-tombstone validation. The stopped branch adds
only its source-specific turn and timestamp checks. Do not add a capacity check
to the active branch.

- [ ] **Step 5: Run both regressions and focused stopped recovery tests**

Run:

```bash
direnv exec . cargo test -p coding-brain-core \
  lifecycle::store::tests::exact_newer_codex_resume_evidence_replaces_stale_active_turn \
  -- --exact
direnv exec . cargo test -p coding-brain-core \
  lifecycle::store::tests::exact_newer_codex_resume_evidence_reactivates_the_child \
  -- --exact
direnv exec . cargo test -p coding-brain-core \
  lifecycle::store::tests::concurrent_new_turn_stop_wins_over_already_read_resume_evidence \
  -- --exact
direnv exec . cargo test --test hook_activity \
  interrupted_codex_child_permission_refreshes_active_turn_and_is_delivered \
  -- --exact
```

Expected: all four tests PASS.

- [ ] **Step 6: Add and run the fail-closed active-proof matrix**

Add a table-driven test using a fresh `active_store(&path)` per case. Mutate
one field at a time for child, provider session, turn, requested path,
canonical path, stale boundary timestamp (`1_500`), and future timestamp
(`8_001`).
Assert each call returns the existing expected `IgnoreReason`, `next_sequence`
does not change, and the original active edge remains on `turn-1`.

Also fill the parent active map to `MAX_ACTIVE_SUBAGENTS` including `child-1`
and assert valid active replacement still returns `Applied`, while the existing
`reproof_rejects_active_capacity_and_expired_tombstone_without_a_sequence`
test continues to reject stopped-child growth.

For sequence exhaustion, persist the otherwise valid active snapshot with
`next_sequence = u64::MAX - 1`, then assert re-proof returns
`Ignored(SequenceExhausted)` and does not alter the original topology.

Run:

```bash
direnv exec . cargo test -p coding-brain-core lifecycle::store::tests::codex_ -- --nocapture
```

Expected: all matching lifecycle-store tests PASS.

- [ ] **Step 7: Review the task diff without committing**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-core/src/lifecycle/store.rs \
  crates/coding-brain-core/src/lifecycle/projection.rs \
  tests/hook_activity.rs
```

Expected: no whitespace errors; every changed line implements active re-proof,
its projection cleanup boundary, or its tests. Do not commit without user
authorization.

### Task 2: Prove remaining permission-hook boundaries and run full gates

**Files:**

- Modify: `tests/hook_activity.rs:178`
- Test: `tests/hook_activity.rs:800`

**Interfaces:**

- Consumes: unchanged permission-hook API and the Task 1 active re-proof behavior.
- Produces: the stopped A-to-permissionless-B-to-C regression, an active invalid-evidence no-allow regression, and complete verification evidence.

**Acceptance Criteria:**

- A stopped child can skip a permissionless follow-up turn B and receive a delivered decision on newer exact turn C.
- Permissionless turn B completion advances the stopped tombstone before turn C re-proof.
- Invalid evidence against stale active proof emits no allow response even when Brain proposes approval.
- The turn-C regression verifies `Observed`, `Evaluating`, `Allowed`, and `Delivered` activity states.
- Existing invalid-evidence and parent-interaction fail-closed tests remain green.
- Focused tests and workspace test, Clippy, format, build, and diff gates pass.

- [ ] **Step 1: Add the stopped, permissionless-intermediate regression**

Write metadata plus turn B start/completion, persist `SubagentStop(turn B)` and
assert the tombstone advances, then append newer turn C. The permission callback
targets turn C:

```rust
#[test]
fn stopped_codex_child_skips_permissionless_followup_and_delivers_next_turn() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let turn_b = one_second_from_now_rfc3339();
    let turn_c = (OffsetDateTime::now_utc() + time::Duration::seconds(2))
        .format(&Rfc3339)
        .unwrap();
    let transcript = home.path().join("rollout-child-a.jsonl");
    fs::write(
        &transcript,
        format!(
            "{}\
             {{\"timestamp\":\"{turn_b}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-b\"}}}}\n\
             {{\"timestamp\":\"{turn_b}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"turn-b\"}}}}\n",
            child_resume_metadata("child-a", "root-1", "root-1"),
        ),
    )
    .unwrap();
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-b"),
    );
    let lifecycle =
        LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let snapshot = lifecycle.read().unwrap().snapshot.unwrap();
    let root_key =
        coding_brain_core::provider::AgentSessionKey::native(
            AgentProvider::Codex,
            "root-1",
        )
        .storage_key();
    assert_eq!(
        snapshot.sessions[&root_key].stopped_subagents["child-a"].turn_id,
        "turn-b"
    );
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap(),
        "{{\"timestamp\":\"{turn_c}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"turn-c\"}}}}"
    )
    .unwrap();

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            "turn-c",
            &transcript,
        ),
    );

    assert!(permission.status.success());
    assert!(
        !permission.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&permission.stderr)
    );
    assert_delivered_child_decision(home.path(), "child-a", "turn-c");
}
```

- [ ] **Step 2: Add an active invalid-evidence no-allow regression**

Create active proof and open projection state on turn A, but make the child
transcript's newest `task_started` identify `turn-other` while the permission
callback requests turn B:

```rust
#[test]
fn invalid_active_codex_followup_evidence_emits_no_allow() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &child_pre_payload(home.path(), "child-a", "turn-a", "tool-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-other",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            "turn-b",
            &transcript,
        ),
    );

    let stderr = String::from_utf8_lossy(&permission.stderr);
    assert!(permission.status.success());
    assert!(permission.stdout.is_empty());
    assert!(stderr.contains("SubagentTurnMismatch"), "{stderr}");
    assert!(stderr.contains("Codex resume evidence:"), "{stderr}");
}
```

- [ ] **Step 3: Run the focused permission-hook suite**

Run:

```bash
direnv exec . cargo test --test hook_activity \
  stopped_codex_child_skips_permissionless_followup_and_delivers_next_turn \
  -- --exact
direnv exec . cargo test --test hook_activity \
  invalid_active_codex_followup_evidence_emits_no_allow \
  -- --exact
direnv exec . cargo test --test hook_activity \
  interrupted_codex_child_permission_refreshes_active_turn_and_is_delivered \
  -- --exact
direnv exec . cargo test --test hook_activity resumed_codex_child -- --nocapture
direnv exec . cargo test --test hook_activity invalid_codex_resume_evidence_remains_fail_closed -- --exact
direnv exec . cargo test --test hook_activity parent_interacted_event_is_not_codex_resume_authority -- --exact
```

Expected: every command PASS; invalid evidence and parent interaction still emit
no allow response.

- [ ] **Step 4: Run complete repository verification**

Run:

```bash
direnv exec . cargo test -- --test-threads=1
direnv exec . cargo clippy --all-targets -- -D warnings
direnv exec . cargo fmt --check
direnv exec . cargo build
git diff --check
git status --short
```

Expected: all commands exit 0. Status shows only the approved spec, plan,
`crates/coding-brain-core/src/lifecycle/store.rs`,
`crates/coding-brain-core/src/lifecycle/projection.rs`, and
`tests/hook_activity.rs`. Do not commit or push.

- [ ] **Step 5: Update Beads and hand off**

Close the implementation task beads and `codexctl-700u` only after Step 4
passes. Record the exact focused and workspace verification commands in the
issue close reason, then report changed files and authorization still required
for commit/push.

## Stress Test Results: Interrupted Codex Subagent Follow-up Proof Plan

### Resolved Decisions

- Task 1 adds both store-level and permission-hook interrupted-turn regressions
  before implementation and proves both fail as `SubagentTurnMismatch`.
- Shared identity, path, file, and future-skew validation is computed once;
  active and stopped sources add only their own ordering checks.
- Generic subtree cleanup stays private behind a narrow crate-internal child
  projection reset method.
- Active-path negatives explicitly cover sequence exhaustion, capacity
  replacement, stale/future evidence, identity and path mismatches,
  cross-parent ownership, and never-proven children.
- The permissionless turn-B trajectory persists `SubagentStop(turn B)` and
  verifies the stopped tombstone before turn-C re-proof.
- Full workspace tests run single-threaded, followed by all-targets Clippy,
  format, build, and diff checks through the project environment.
- Security coverage includes an end-to-end active invalid-evidence no-allow
  case and proves sibling projection survives child subtree cleanup.
- Task 1 owns red tests plus the minimal store fix; Task 2 owns the remaining
  integration boundary and full verification.

### Changes Made

- Moved the interrupted permission regression before implementation.
- Replaced broad helper visibility with a focused cleanup API.
- Made common validation, negative cases, lifecycle-transition fidelity, and
  security assertions executable.
- Updated verification commands and task ownership.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: implementation must preserve the stopped recovery outcome
  reasons while sharing validation, and the turn-B stop fixture must reflect
  the actual root-owned Codex hook identity.
