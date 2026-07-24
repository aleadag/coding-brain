# Compact Lifecycle Continuity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Preserve the complete bounded lifecycle projection across `SessionStart(compact)` so continuing permission events remain valid without weakening replay or ambiguity protection.

**Architecture:** Keep `SessionStart` handling in `LifecycleSnapshot::apply`. Skip the existing full-reset operation only when the source is `SessionStartSource::Compact`; still accept the event and refresh its metadata. All other session-start sources retain the current reset path.

**Tech Stack:** Rust 2024 workspace, built-in unit tests, Cargo, rustfmt, Clippy

## Global Constraints

- Compact continuity is defined by `SessionStartSource`, consistently across providers.
- Compact preserves current turn state, recent turns, projected status, active subagents, and provider-specific correlation state.
- Compact still updates cwd, transcript path, source, signature, sequence, and receipt time.
- Startup, resume, and clear retain the existing full-reset behavior.
- Existing duplicate, replay, ambiguity, and capacity protections remain unchanged.
- Context-length and compaction telemetry remain owned by transcript discovery.
- Do not change public schemas, hook payloads, configuration, reconciliation, CLI, or UI code.

---

### Task 1: Preserve lifecycle projection across compact

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Test: `crates/coding-brain-core/src/lifecycle/projection.rs`

**Interfaces:**
- Consumes: `LifecycleEventKind::SessionStart { source: SessionStartSource }`, `SessionLifecycleState::clear_transient_status()`, and the existing `LifecycleSnapshot::apply` event path.
- Produces: source-aware `SessionStart` projection behavior; no new public interface.

**Acceptance Criteria:**
- `SessionStart(compact)` preserves the full bounded lifecycle projection and allows a later permission for the continuing active turn.
- Compact with no active turn remains empty; compact after `Stop` does not reopen the stopped turn or remove its recent-turn protection.
- A mismatched turn after compact remains `AmbiguousTurn`, and a stopped/recent turn remains `RecentTurn`.
- Active subagents and Antigravity child-correlation state survive compact without duplication or capacity changes.
- Compact has the same source-defined behavior for Codex, Claude, and Antigravity projection state.
- Startup, resume, and clear continue to archive the active turn and clear transient lifecycle state.
- Focused tests and all workspace quality gates pass.

- [ ] **Step 1: Add an exact SessionStart test helper**

Add this helper beside the existing test helpers:

```rust
fn session_start(
    provider: AgentProvider,
    session_id: &str,
    cwd: &str,
    source: SessionStartSource,
) -> LifecycleEvent {
    let identity = LifecycleIdentity::try_new(
        provider,
        session_id.into(),
        None,
        None,
        cwd.into(),
    )
    .unwrap();
    LifecycleEvent::from_parts(identity, LifecycleEventKind::SessionStart { source }).unwrap()
}
```

Keep the existing generic `event` helper unchanged for unrelated tests.

- [ ] **Step 2: Write the failing active-turn continuity test**

Add a test that captures every relevant Codex field before compact, applies the compact event, verifies preservation, then exercises positive and negative permission correlation:

```rust
#[test]
fn compact_preserves_active_lifecycle_and_turn_guards() {
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.apply(prompt("turn-1"), 1_000);
    snapshot.apply(
        permission("turn-1", PermissionDisposition::NeedsInput),
        2_000,
    );
    snapshot.apply(subagent_start("turn-1", "agent-1"), 3_000);

    let before = snapshot.sessions[&session_key()].clone();
    assert_eq!(
        snapshot.apply(
            session_start(
                AgentProvider::Codex,
                "session-1",
                "/work/after-compact",
                SessionStartSource::Compact,
            ),
            4_000,
        ),
        ApplyOutcome::Applied
    );

    let state = &snapshot.sessions[&session_key()];
    assert_eq!(state.current_turn, before.current_turn);
    assert_eq!(state.turn_open, before.turn_open);
    assert_eq!(state.recent_turns, before.recent_turns);
    assert_eq!(state.status_event, before.status_event);
    assert_eq!(state.status_sequence, before.status_sequence);
    assert_eq!(state.status_received_at_ms, before.status_received_at_ms);
    assert_eq!(state.projected_status, before.projected_status);
    assert_eq!(state.active_subagents, before.active_subagents);
    assert_eq!(
        state.session_start_source,
        Some(SessionStartSource::Compact)
    );
    assert_eq!(state.cwd, PathBuf::from("/work/after-compact"));
    assert_eq!(state.latest_event, Some(LifecycleEventName::SessionStart));
    assert_eq!(state.latest_received_at_ms, 4_000);

    assert_eq!(
        snapshot.apply(
            permission("turn-1", PermissionDisposition::Decided),
            5_000,
        ),
        ApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply(pre_tool("turn-2"), 6_000),
        ApplyOutcome::Ignored(IgnoreReason::AmbiguousTurn)
    );
}
```

- [ ] **Step 3: Write failing empty, stopped, and reset-source tests**

Add explicit boundary coverage:

```rust
#[test]
fn compact_does_not_create_or_reopen_a_turn() {
    let compact = || {
        session_start(
            AgentProvider::Codex,
            "session-1",
            "/work/codexctl",
            SessionStartSource::Compact,
        )
    };

    let mut empty = LifecycleSnapshot::default();
    empty.apply(compact(), 1_000);
    let empty_state = &empty.sessions[&session_key()];
    assert_eq!(empty_state.current_turn, None);
    assert!(!empty_state.turn_open);

    let mut stopped = LifecycleSnapshot::default();
    stopped.apply(prompt("turn-1"), 1_000);
    stopped.apply(stop("turn-1"), 2_000);
    stopped.apply(compact(), 3_000);
    let stopped_state = &stopped.sessions[&session_key()];
    assert_eq!(stopped_state.current_turn.as_deref(), Some("turn-1"));
    assert!(!stopped_state.turn_open);
    assert!(stopped_state.recent_turns.iter().any(|turn| turn == "turn-1"));
    assert_eq!(
        stopped.apply(pre_tool("turn-1"), 4_000),
        ApplyOutcome::Ignored(IgnoreReason::RecentTurn)
    );
}

#[test]
fn consecutive_compact_events_remain_duplicates() {
    let mut snapshot = LifecycleSnapshot::default();
    let compact = || {
        session_start(
            AgentProvider::Codex,
            "session-1",
            "/work/codexctl",
            SessionStartSource::Compact,
        )
    };

    assert_eq!(snapshot.apply(compact(), 1_000), ApplyOutcome::Applied);
    let next_sequence = snapshot.next_sequence;
    assert_eq!(
        snapshot.apply(compact(), 2_000),
        ApplyOutcome::Ignored(IgnoreReason::Duplicate)
    );
    assert_eq!(snapshot.next_sequence, next_sequence);
}

#[test]
fn non_compact_session_starts_keep_full_reset_semantics() {
    for source in [
        SessionStartSource::Startup,
        SessionStartSource::Resume,
        SessionStartSource::Clear,
    ] {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("turn-1"), 1_000);
        snapshot.apply(subagent_start("turn-1", "agent-1"), 2_000);
        snapshot.apply(
            session_start(
                AgentProvider::Codex,
                "session-1",
                "/work/codexctl",
                source,
            ),
            3_000,
        );

        let state = &snapshot.sessions[&session_key()];
        assert_eq!(state.current_turn, None);
        assert!(!state.turn_open);
        assert_eq!(state.projected_status, None);
        assert!(state.active_subagents.is_empty());
        assert!(state.recent_turns.iter().any(|turn| turn == "turn-1"));
        assert_eq!(state.session_start_source, Some(source));
    }
}
```

Replace `session_start_clears_transient_state_but_keeps_recent_turns` with the table-driven non-compact test rather than retaining overlapping coverage.

- [ ] **Step 4: Write the failing provider-state preservation test**

Use the existing Antigravity helpers to prove compact preserves bounded provider-specific correlation:

```rust
#[test]
fn compact_preserves_provider_specific_correlation_state() {
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.apply(invocation("invocation-1", 5), 1);
    snapshot.apply(
        LifecycleEvent::permission(
            antigravity_identity("step-5"),
            PermissionDisposition::Decided,
        )
        .unwrap(),
        2,
    );
    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    let before = snapshot.sessions[&key].clone();

    snapshot.apply(
        session_start(
            AgentProvider::Antigravity,
            "agy-conversation-1",
            "/work/antigravity",
            SessionStartSource::Compact,
        ),
        3,
    );

    let state = &snapshot.sessions[&key];
    assert_eq!(state.current_turn, before.current_turn);
    assert_eq!(state.turn_open, before.turn_open);
    assert_eq!(state.antigravity_initial_step, before.antigravity_initial_step);
    assert_eq!(
        state.antigravity_child_events,
        before.antigravity_child_events
    );
}
```

The core event model is provider-qualified but source-generic, so this test is the concrete proof that no provider-only exception was introduced.

Add an explicit provider table test as well:

```rust
#[test]
fn compact_continuity_is_source_defined_across_providers() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        let identity = LifecycleIdentity::try_new(
            provider,
            "provider-session".into(),
            Some("turn-1".into()),
            None,
            "/work/provider".into(),
        )
        .unwrap();
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(
            LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap(),
            1,
        );
        snapshot.apply(
            session_start(
                provider,
                "provider-session",
                "/work/provider",
                SessionStartSource::Compact,
            ),
            2,
        );

        let key = AgentSessionKey::native(provider, "provider-session").storage_key();
        let state = &snapshot.sessions[&key];
        assert_eq!(state.current_turn.as_deref(), Some("turn-1"));
        assert!(state.turn_open);
        assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));
    }
}
```

- [ ] **Step 5: Run the focused tests and verify red**

Run:

```bash
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::compact_ -- --nocapture
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::non_compact_session_starts_keep_full_reset_semantics -- --nocapture
```

Expected: the compact preservation tests fail because the current `SessionStart` path calls `clear_transient_status()` for every source. The non-compact reset test passes.

- [ ] **Step 6: Implement the minimal source-aware reset**

Change only the existing `SessionStart` branch in `LifecycleSnapshot::apply`:

```rust
if let LifecycleEventKind::SessionStart { source } = event.kind() {
    let sequence = self.next_sequence;
    self.next_sequence += 1;
    state.cwd = event.identity().cwd().to_path_buf();
    state.transcript_path = event.identity().transcript_path().map(PathBuf::from);
    if *source != SessionStartSource::Compact {
        state.clear_transient_status();
    }
    state.session_start_source = Some(*source);
    accept_event(state, &event, signature, sequence, received_at_ms);
    return ApplyOutcome::Applied;
}
```

Do not alter `clear_transient_status`, turn guards, capacity limits, duplicate signatures, serialization, or reconciliation.

- [ ] **Step 7: Run focused tests and verify green**

Run:

```bash
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::compact_ -- --nocapture
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests::non_compact_session_starts_keep_full_reset_semantics -- --nocapture
direnv exec . cargo test -p coding-brain-core lifecycle::projection::tests -- --nocapture
```

Expected: all selected projection tests pass with zero failures.

- [ ] **Step 8: Run workspace quality gates**

Run:

```bash
direnv exec . cargo fmt --all --check
direnv exec . cargo test --workspace
direnv exec . cargo clippy --workspace --all-targets -- -D warnings
direnv exec . cargo build --workspace
```

Expected: every command exits 0; tests report zero failures and Clippy reports no warnings.

- [ ] **Step 9: Inspect the final diff**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-core/src/lifecycle/projection.rs
git status --short
```

Expected: no whitespace errors; production and test changes are confined to `projection.rs`, alongside this approved spec and plan.

- [ ] **Step 10: Commit only after explicit authorization**

Do not commit by default. If the user explicitly authorizes a commit after verification:

```bash
git add crates/coding-brain-core/src/lifecycle/projection.rs \
  .internal/specs/2026-07-24-compact-lifecycle-continuity-design.md \
  .internal/plans/2026-07-24-compact-lifecycle-continuity.md
git commit -m "🐛 fix: preserve lifecycle across compact (codexctl-rlno)"
```

Expected: one surgical commit containing the approved spec, plan, regression tests, and implementation.

## Stress Test Results: Compact Lifecycle Implementation Plan

### Resolved Decisions

- Keep one task because tests and implementation form one independently
  reviewable invariant in one file.
- Retain both cross-provider and Antigravity-state tests because they prove
  different requirements.
- Use focused red tests plus a passing non-compact control before implementation.
- Implement one conditional around the existing reset call; do not add an
  abstraction or save-and-restore path.
- Rely on existing capacity fixtures plus new preservation equality and negative
  turn-guard tests instead of duplicating maximum-size fixtures.
- Run focused and full workspace gates through `direnv`; keep commit and
  publication outside default execution authority.

### Changes Made

- None. The plan already covered every resolved branch.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: none beyond external provider hook ordering, which the
  event-sequence regression tests intentionally avoid assuming.
