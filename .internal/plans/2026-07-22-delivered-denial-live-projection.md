# Delivered Denial Live Projection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Treat a successfully delivered automatic denial as resolved Live activity and describe it as a blocked command that did not execute.

**Architecture:** Keep persisted activity and permission-hook behavior unchanged. Adjust the existing activity projection predicate so only delivered denials leave Needs Attention, then special-case the delivered-denial copy in the existing TUI status renderer.

**Tech Stack:** Rust, Cargo workspace tests, Ratatui `TestBackend`, Beads.

## Global Constraints

- Preserve `ActivityState::Denied` and `DeliveryState::Delivered` in persisted activity.
- Do not change permission-hook evaluation, response generation, or advisory behavior.
- Failed or unknown denial delivery remains in Needs Attention.
- Allowed response delivery remains distinct from confirmed command execution.
- Do not commit or push under the repository's conservative profile without explicit user authorization.

---

### Task 1: Resolve Delivered Denials in Activity Projection

**Files:**

- Modify: `src/brain/activity.rs:544`
- Test: `src/brain/activity.rs:1006`
- Test: `tests/hook_activity.rs:191`

**Interfaces:**

- Consumes: `ActivityItem.state`, `ActivityItem.delivery`, existing outcome and correction resolution.
- Produces: an `ActivitySnapshot` where `Denied + Delivered` is in `recent`, while `Denied + Unknown|Failed` remains in `attention`.

**Acceptance Criteria:**

- A delivered denial appears once under Recent and leaves `unresolved_count == 0`.
- Unknown and failed denial delivery remain actionable.
- The automatic-deny process regression projects successful delivery under Recent.
- The advisory process regression still emits no response and records `NeedsInput`.

- [ ] **Step 1: Add the failing denial-delivery projection matrix**

Add this test beside `delivery_and_outcome_evidence_are_distinct` in `src/brain/activity.rs`:

```rust
#[test]
fn denial_delivery_controls_attention() {
    let (_root, delivered_store) = fixture_store();
    delivered_store
        .append(event("delivered", ActivityState::Denied))
        .unwrap();
    delivered_store
        .append(event_at("delivered", ActivityState::Delivered, 101))
        .unwrap();

    let delivered = delivered_store
        .snapshot(SnapshotLimits::default())
        .unwrap();
    assert!(delivered.attention.is_empty());
    assert_eq!(delivered.unresolved_count, 0);
    assert_eq!(delivered.recent.len(), 1);
    assert_eq!(delivered.recent[0].state, ActivityState::Denied);
    assert_eq!(delivered.recent[0].delivery, DeliveryState::Delivered);

    let (_root, unknown_store) = fixture_store();
    unknown_store
        .append(event("unknown", ActivityState::Denied))
        .unwrap();
    let unknown = unknown_store.snapshot(SnapshotLimits::default()).unwrap();
    assert_eq!(unknown.attention.len(), 1);
    assert_eq!(unknown.attention[0].delivery, DeliveryState::Unknown);
    assert_eq!(unknown.unresolved_count, 1);
}
```

- [ ] **Step 2: Run the projection test and verify RED**

Run:

```bash
direnv exec . cargo test -p coding-brain denial_delivery_controls_attention -- --nocapture
```

Expected: FAIL because the delivered denial is still present in `attention`.

- [ ] **Step 3: Add the failing process-level projection assertion**

Extend `deterministic_deny_is_delivered_when_decision_audit_is_down` in `tests/hook_activity.rs` after its event-state assertion:

```rust
let snapshot = activity(home.path())
    .snapshot(SnapshotLimits::default())
    .unwrap();
assert!(snapshot.attention.is_empty());
assert_eq!(snapshot.unresolved_count, 0);
assert_eq!(snapshot.recent.len(), 1);
assert_eq!(snapshot.recent[0].state, ActivityState::Denied);
assert_eq!(snapshot.recent[0].delivery, DeliveryState::Delivered);
```

- [ ] **Step 4: Run the process regression and verify RED**

Run:

```bash
direnv exec . cargo test --test hook_activity deterministic_deny_is_delivered_when_decision_audit_is_down -- --exact --nocapture
```

Expected: FAIL because the snapshot still classifies the delivered denial under Needs Attention.

- [ ] **Step 5: Exclude delivered denials from the attention predicate**

Change the existing predicate in `project_snapshot` to:

```rust
let needs_attention = !resolved
    && (matches!(
        item.state,
        ActivityState::Denied
            | ActivityState::Abstained
            | ActivityState::Error
            | ActivityState::Interrupted
    ) || matches!(
        item.delivery,
        DeliveryState::Unknown | DeliveryState::Failed
    ))
    && !matches!(
        (item.state, item.delivery),
        (ActivityState::Denied, DeliveryState::Delivered)
    );
```

`failed_outcome` remains a separate condition immediately below this predicate, so failed outcomes continue to enter Needs Attention.

- [ ] **Step 6: Run focused projection and permission-hook tests and verify GREEN**

Run:

```bash
direnv exec . cargo test -p coding-brain denial_delivery_controls_attention -- --nocapture
direnv exec . cargo test --test hook_activity deterministic_deny_is_delivered_when_decision_audit_is_down -- --exact --nocapture
direnv exec . cargo test --test hook_activity explicit_on_without_toml_uses_defaults_and_audits_without_response -- --exact --nocapture
direnv exec . cargo test -p coding-brain delivery_failure_needs_attention -- --nocapture
direnv exec . cargo test -p coding-brain delivery_and_outcome_evidence_are_distinct -- --nocapture
```

Expected: all focused tests PASS. The advisory test still has empty stdout and `ProjectedStatus::NeedsInput`; delivered allow remains execution-unconfirmed.

- [ ] **Step 7: Record the review checkpoint**

Run:

```bash
git diff --check
git diff -- src/brain/activity.rs tests/hook_activity.rs
```

Expected: only the projection predicate and its regressions changed. Do not commit without explicit authorization.

---

### Task 2: Render Delivered Denials as Blocked

**Files:**

- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs:156`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs:319`

**Interfaces:**

- Consumes: the unchanged `ActivityItem` with `state == Denied` and `delivery == Delivered` supplied through `ActivitySnapshot.recent` by Task 1.
- Produces: exact Live status text `blocked · command did not execute` for that state pair only.

**Acceptance Criteria:**

- A delivered denial renders under Recent with `blocked · command did not execute`.
- The old `execution not confirmed` copy is absent for a delivered denial.
- Delivered allows and all failed or unknown delivery states retain their existing copy.
- Workspace format, tests, Clippy, and build pass.

- [ ] **Step 1: Rewrite the TUI regression and verify its fixture represents Recent**

Replace `delivered_deny_shows_decision_and_delivery_as_separate_evidence` in `crates/coding-brain-tui/src/ui/brain/mod.rs` with:

```rust
#[test]
fn delivered_deny_is_recent_and_reports_blocked_execution() {
    let mock = MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            recent: vec![activity("deny-1", DeliveryState::Delivered)],
            ..ActivitySnapshot::default()
        },
        endpoint_health: online(),
        ..MockBrainRuntime::default()
    };

    let text = render_text(&fixture_app(mock));

    assert!(text.contains("blocked · command did not execute"));
    assert!(!text.contains("denied · response delivered · execution not confirmed"));
    assert!(text.contains("No unresolved decisions"));
}
```

- [ ] **Step 2: Run the TUI regression and verify RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui delivered_deny_is_recent_and_reports_blocked_execution -- --nocapture
```

Expected: FAIL because `activity_status` still returns `denied · response delivered · execution not confirmed`.

- [ ] **Step 3: Add the delivered-denial status special case**

In `activity_status`, after outcome and correction handling but before the delivery match, add:

```rust
if matches!(
    (item.state, item.delivery),
    (ActivityState::Denied, DeliveryState::Delivered)
) {
    return "blocked · command did not execute".into();
}
```

- [ ] **Step 4: Run focused TUI tests and verify GREEN**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui delivered_deny_is_recent_and_reports_blocked_execution -- --nocapture
direnv exec . cargo test -p coding-brain-tui live_renders_attention_recent_detail_and_overflow_without_dashboard_actions -- --nocapture
direnv exec . cargo test -p coding-brain-tui offline_banner_keeps_persisted_live_data_visible -- --nocapture
```

Expected: all focused TUI tests PASS; failed and unknown delivery copy remains unchanged.

- [ ] **Step 5: Run full quality gates**

Run:

```bash
direnv exec . cargo fmt --all --check
direnv exec . cargo test --workspace --quiet
direnv exec . cargo clippy --workspace --all-targets -- -D warnings
direnv exec . cargo build --workspace
```

Expected: every command exits 0 with no warnings.

- [ ] **Step 6: Verify the final scope**

Run:

```bash
git diff --check
git status --short
git diff --stat
```

Expected: changes are limited to the approved spec, this plan, `src/brain/activity.rs`, `tests/hook_activity.rs`, `crates/coding-brain-tui/src/ui/brain/live.rs`, and `crates/coding-brain-tui/src/ui/brain/mod.rs`. Do not commit or push without explicit authorization.
