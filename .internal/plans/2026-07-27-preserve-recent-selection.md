# Preserve Recent Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Keep the operator's selected Recent activity stable across successful Live snapshot refreshes when that activity remains present.

**Architecture:** Capture the remembered Recent activity identity before `BrainApp::refresh_state` replaces the snapshot. After replacement, restore the index matching that identity when possible, then let the existing clamp path handle removal and empty-list fallback.

**Tech Stack:** Rust, Ratatui application state, Cargo unit tests

## Global Constraints

- Limit production and test changes to `crates/coding-brain-tui/src/brain_app.rs`.
- Preserve Attention focus, navigation, correction/action targeting, refresh cadence, busy/error stale-snapshot behavior, and Evidence viewport semantics.
- Do not add configuration, dependencies, or new abstractions.
- Do not commit or push without explicit user authorization.

---

### Task 1: Reconcile Recent Selection by Activity Identity

**Bead:** `codexctl-ntii`

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs:210`
- Test: `crates/coding-brain-tui/src/brain_app.rs:1450`

**Interfaces:**
- Consumes: `BrainApp::refresh_state(&mut self) -> bool`, `ActivityItem::activity_id: String`, and the existing `BrainApp::clamp_selection`.
- Produces: no new public interface; successful refreshes update `live_recent_selection` to the refreshed row whose `activity_id` matched the prior remembered Recent row.

**Acceptance Criteria:**
- Inserting a newer Recent row ahead of the selection preserves the selected `activity_id` and Evidence viewport identity.
- Removing the selected Recent row falls back to a valid row through the existing clamp behavior.
- Navigation and action targeting continue to use the visibly selected activity.
- Focused tests, the `coding-brain-tui` suite, formatting, and Clippy pass.

- [ ] **Step 1: Add a test helper for snapshots with ordered Recent identities**

Add beside `refresh_fixture`:

```rust
fn refresh_with_recent(activity_ids: &[&str]) -> BrainRefresh {
    BrainRefresh {
        snapshot: ActivitySnapshot {
            recent: activity_ids
                .iter()
                .map(|activity_id| {
                    let mut item = activity();
                    item.activity_id = (*activity_id).into();
                    item
                })
                .collect(),
            ..ActivitySnapshot::default()
        },
        ..BrainRefresh::default()
    }
}
```

- [ ] **Step 2: Write refresh-level regression tests**

Add near the existing refresh tests:

```rust
#[test]
fn refresh_preserves_recent_selection_by_activity_id() {
    let mut app = scripted_app([
        Ok(refresh_with_recent(&["recent-2", "recent-1"])),
        Ok(refresh_with_recent(&["recent-3", "recent-2", "recent-1"])),
    ]);
    app.handle_key(key(KeyCode::Char('j')));
    app.update_live_evidence_metrics(5, 20);
    app.handle_key(key(KeyCode::PageDown));

    app.refresh();

    assert_eq!(
        app.selected_live_activity().unwrap().activity_id,
        "recent-1"
    );
    assert_eq!(app.selected_recent_index(), Some(2));
    assert_eq!(app.live_evidence_scroll(), 5);
}

#[test]
fn refresh_removing_selected_recent_activity_uses_clamped_fallback() {
    let mut app = scripted_app([
        Ok(refresh_with_recent(&["recent-2", "recent-1"])),
        Ok(refresh_with_recent(&["recent-3"])),
    ]);
    app.handle_key(key(KeyCode::Char('j')));

    app.refresh();

    assert_eq!(
        app.selected_live_activity().unwrap().activity_id,
        "recent-3"
    );
    assert_eq!(app.selected_recent_index(), Some(0));
}
```

- [ ] **Step 3: Run the focused tests and verify RED**

Run:

```bash
cargo test -p coding-brain-tui refresh_preserves_recent_selection
```

Expected: `refresh_preserves_recent_selection_by_activity_id` fails because index `1` selects `recent-2` after insertion; the removal fallback test passes.

- [ ] **Step 4: Restore the remembered identity after successful snapshot replacement**

In the successful `BrainSource::refresh` branch, capture the current Recent
identity before assigning `refresh.snapshot`, then restore its refreshed index:

```rust
let selected_recent_activity_id = self
    .snapshot
    .recent
    .get(self.live_recent_selection)
    .map(|item| item.activity_id.clone());
self.snapshot = refresh.snapshot;
if let Some(selected_recent_activity_id) = selected_recent_activity_id
    && let Some(index) = self
        .snapshot
        .recent
        .iter()
        .position(|item| item.activity_id == selected_recent_activity_id)
{
    self.live_recent_selection = index;
}
```

Keep the existing `self.clamp_selection()` call in its current position so a
missing identity retains the established deterministic fallback.

- [ ] **Step 5: Run focused and crate tests and verify GREEN**

Run:

```bash
cargo test -p coding-brain-tui refresh_preserves_recent_selection
cargo test -p coding-brain-tui
```

Expected: both regression tests and the complete `coding-brain-tui` test suite pass.

- [ ] **Step 6: Run formatting, lint, and diff checks**

Run:

```bash
cargo fmt --check
cargo clippy -p coding-brain-tui -- -D warnings
git diff --check
git diff -- crates/coding-brain-tui/src/brain_app.rs
```

Expected: formatting and Clippy exit successfully, `git diff --check` reports
no whitespace errors, and the diff contains only the approved tests and
identity restoration.

- [ ] **Step 7: Hand off without publishing**

Close `codexctl-ntii`, `codexctl-8cj3`, and `codexctl-sywt` only after all
verification passes. Report changed files, exact validation evidence, Beads
status, and a suggested emoji conventional commit message. Do not commit or
push without explicit user authorization.
