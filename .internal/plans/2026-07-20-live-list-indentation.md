# Stable Live List Indentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Keep `Needs Attention` and `Recent` item text fixed horizontally when Live-tab selection moves between the two lists.

**Architecture:** Preserve the existing global `BrainApp::selection` and per-list `ListState` projection. Configure both Ratatui lists to reserve the highlight-symbol column unconditionally, and cover the behavior through the existing `TestBackend` render path.

**Tech Stack:** Rust 1.88, Ratatui 0.29, Crossterm 0.28, Cargo tests

## Global Constraints

- Only the selected row displays the `> ` marker.
- Do not change keyboard navigation, selection state, list contents, colors, borders, or detail rendering.
- Add no dependencies, persistent state, error paths, or security-sensitive behavior.
- Follow test-first development and observe the regression test fail before editing production code.

---

## File Map

- `crates/coding-brain-tui/src/ui/brain/mod.rs`: owns the existing `TestBackend` UI tests and receives the focus-transition regression.
- `crates/coding-brain-tui/src/ui/brain/live.rs`: owns both Live-tab `List` builders and receives the Ratatui spacing configuration.

### Task 1: Keep Live List Geometry Stable Across Focus Changes

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs:143-181,418-439`
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs:6-10,62-73,99-106`

**Interfaces:**
- Consumes: `BrainApp::handle_key(KeyEvent)`, the existing `fixture_app(MockBrainRuntime) -> BrainApp`, `render_text(&BrainApp) -> String`, and Ratatui `List::highlight_spacing(HighlightSpacing)`.
- Produces: stable two-column highlight spacing in both Live lists; no new public API.

**Acceptance Criteria:**
- Moving selection between `Needs Attention` and `Recent` does not move either list's item text horizontally.
- Exactly one selected row displays the `> ` marker.
- Existing navigation and selection behavior remain unchanged.
- The focused regression, TUI crate tests, workspace tests, formatting, lint, and build gates pass.

- [ ] **Step 1: Add the render-buffer regression test**

Add this test after `live_renders_attention_recent_detail_and_overflow_without_dashboard_actions` in `crates/coding-brain-tui/src/ui/brain/mod.rs`:

```rust
#[test]
fn live_list_indentation_stays_fixed_when_selection_moves_between_lists() {
    let mut recent = activity("recent-1", DeliveryState::Delivered);
    recent.state = ActivityState::Allowed;
    let mock = MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            attention: vec![AttentionItem {
                activity: activity("attention-1", DeliveryState::Unknown),
                occurrences: 1,
                unresolved_occurrences: 1,
            }],
            recent: vec![recent],
            unresolved_count: 1,
            diagnostics: Default::default(),
        },
        endpoint_health: online(),
        ..MockBrainRuntime::default()
    };
    let mut app = fixture_app(mock);

    let attention_focused = render_text(&app);
    app.handle_key(key(KeyCode::Down));
    let recent_focused = render_text(&app);

    assert_eq!(
        content_column(&attention_focused, "attention-1", "denied"),
        content_column(&recent_focused, "attention-1", "denied")
    );
    assert_eq!(
        content_column(&attention_focused, "recent-1", "allowed"),
        content_column(&recent_focused, "recent-1", "allowed")
    );
    assert_eq!(attention_focused.matches("> ").count(), 1);
    assert_eq!(recent_focused.matches("> ").count(), 1);
}
```

Add this helper immediately after `render_text`:

```rust
fn content_column(text: &str, row_id: &str, content: &str) -> usize {
    let line = text
        .lines()
        .find(|line| line.contains(row_id))
        .unwrap_or_else(|| panic!("missing row {row_id}:\n{text}"));
    let byte_index = line
        .find(content)
        .unwrap_or_else(|| panic!("missing content {content} in row {row_id}:\n{line}"));
    line[..byte_index].chars().count()
}
```

- [ ] **Step 2: Run the focused test and verify the RED state**

Run:

```bash
cargo test -p coding-brain-tui live_list_indentation_stays_fixed_when_selection_moves_between_lists -- --nocapture
```

Expected: FAIL on a content-column equality assertion. With Ratatui's default `HighlightSpacing::WhenSelected`, the selected list reserves the two-character `> ` column and the inactive list does not.

- [ ] **Step 3: Reserve highlight spacing in both Live lists**

Update the widget import in `crates/coding-brain-tui/src/ui/brain/live.rs`:

```rust
use ratatui::widgets::{
    Block, Borders, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
};
```

Update both list builder chains so the final calls are:

```rust
.highlight_symbol("> ")
.highlight_spacing(HighlightSpacing::Always);
```

Apply the same two calls to the `Needs Attention` and `Recent` lists. Do not change either `ListState` selection condition.

- [ ] **Step 4: Run the focused test and verify the GREEN state**

Run:

```bash
cargo test -p coding-brain-tui live_list_indentation_stays_fixed_when_selection_moves_between_lists -- --nocapture
```

Expected: PASS, with one test run and zero failures.

- [ ] **Step 5: Run the TUI crate regression suite**

Run:

```bash
cargo test -p coding-brain-tui
```

Expected: all `coding-brain-tui` unit and documentation tests pass with zero failures.

- [ ] **Step 6: Run the repository quality gates**

Run each command and require exit code 0:

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo build
```

Expected: formatting is unchanged, Clippy reports no warnings, all workspace tests pass, and the workspace builds successfully.

- [ ] **Step 7: Review the final change and prepare the handoff**

Run:

```bash
jj --no-pager diff --git
jj --no-pager st
```

Expected: the changeset contains only the approved design and plan documents, the focused render regression, and the two `HighlightSpacing::Always` calls. Do not push. Under the repository's conservative profile, report the proposed emoji conventional description and wait for explicit commit/integration authority.
