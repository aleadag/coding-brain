# Compact Recent Unseen Indicator Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Replace the Live tab's verbose Recent `unseen`/`seen` row prefixes with an aligned two-column bullet/blank indicator while preserving lifecycle behavior and all other surfaces.

**Architecture:** Keep review state and projections unchanged. Adjust only the existing TUI prefix formatter's Recent-specific return values, and extend the existing lifecycle render fixture and tests to cover mixed seen/unseen rows, selection, width, styling, responsive layouts, and unchanged non-Recent labels.

**Tech Stack:** Rust, Ratatui, `unicode-width`, Cargo workspace tests in the Nix development shell.

## Global Constraints

- Recent unseen rows use `● ` with the existing bold header-color style.
- Recent seen rows use two spaces and retain the existing muted row style when unselected.
- The two prefixes have equal terminal display width so row bodies stay aligned.
- Keep `Recent (N unseen)` semantics and the `a seen` / `A seen all` actions unchanged.
- Attention, Review, and Diagnostics retain `NEW` / `reviewed` markers.
- Do not change review-state contracts, persistence, mutations, ordering, selection, evidence, themes, security boundaries, or Scorecard behavior.
- Keep the diff confined to `crates/coding-brain-tui/src/ui/brain/mod.rs`, this plan, and the approved design spec.
- The execution epic must depend on brainstorming bead `codexctl-rmcjh` with dependency type `discovered-from`.
- Do not commit unless the user explicitly authorizes it.

## File Structure

- Modify `crates/coding-brain-tui/src/ui/brain/mod.rs`: change the existing Recent prefix strings, make the lifecycle fixture include one unseen and one seen Recent row, and extend render assertions.
- Retain `.internal/specs/2026-08-11-compact-recent-unseen-indicator-design.md`: approved design and stress-test record; no implementation-time rewrite is expected.

---

### Task 1: Render compact aligned Recent lifecycle indicators

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs:268-280`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs:374-493`
- Test fixture: `crates/coding-brain-tui/src/ui/brain/mod.rs:1649-1732`

**Interfaces:**
- Consumes: `review_prefix(target: &ReviewTarget, unseen_label: &str) -> &'static str`, `review_style(target: &ReviewTarget, theme: &Theme) -> Style`, and `SurfaceReviewProjection` state already supplied by `BrainApp`.
- Produces: Recent prefixes `● ` for targets with new member keys and `  ` for targets without new member keys; all non-Recent prefix outputs remain byte-for-byte unchanged.

**Acceptance Criteria:**
- Unseen Recent rows render a `● ` prefix with a terminal display width of two columns.
- Seen Recent rows render a two-space prefix and use the same body column as unseen rows.
- Selected seen and unseen rows remain distinguishable by the bullet's presence.
- Unselected seen rows remain muted; unseen rows retain existing bold/header styling.
- `Recent (N unseen)`, seen actions, Attention/Review/Diagnostics labels, responsive layout, and Scorecard behavior remain unchanged.
- Focused TUI tests and full workspace format, test, and clippy gates pass.
- The final source diff is confined to the TUI renderer and its tests.

- [ ] **Step 1: Make the lifecycle fixture exercise one unseen and one seen Recent row**

In `lifecycle_render_app`, replace the Recent projection with:

```rust
recent: lifecycle_projection(
    ReviewSurface::Recent,
    vec![
        ("recent-new-1".into(), 1, 0),
        ("recent-new-2".into(), 0, 1),
    ],
    1,
    1,
    0,
),
```

Update fixture-backed title assertions from `Recent (2 unseen)` to `Recent (1 unseen)`. This changes only test data and expected counts; production count semantics remain unchanged.

- [ ] **Step 2: Write failing compact-prefix and alignment assertions**

Add this test beside `lifecycle_rows_keep_layout_and_reviewed_rows_are_deemphasized`:

```rust
#[test]
fn recent_rows_use_compact_aligned_seen_and_unseen_prefixes() {
    let mut app = lifecycle_render_app(1);
    let theme = *app.theme();
    let projection = app.review_projection(ReviewSurface::Recent);
    let unseen_prefix = review_prefix(&projection.items[0], "unseen");
    let seen_prefix = review_prefix(&projection.items[1], "unseen");

    assert_eq!(unseen_prefix, "● ");
    assert_eq!(seen_prefix, "  ");
    assert_eq!(UnicodeWidthStr::width(unseen_prefix), 2);
    assert_eq!(UnicodeWidthStr::width(seen_prefix), 2);

    app.handle_key(key(KeyCode::Char('J')));
    for width in [41, 119, 120, 140] {
        let text = render_text_at(&app, width, 38);
        assert!(text.contains("> ●"), "missing unseen marker at {width}:\n{text}");
        assert!(
            text.contains("Recent (1 unseen)"),
            "missing Recent count at {width}:\n{text}"
        );
    }

    let unseen_buffer = render_buffer_at(&app, 140, 38);
    let unseen_text = buffer_text(&unseen_buffer);
    let unseen_content = content_column(&unseen_text, "recent-new-1", "recent-new-1");
    let seen_row = unseen_text
        .lines()
        .position(|line| line.contains("recent-new-2"))
        .unwrap();
    let seen_content = content_column(&unseen_text, "recent-new-2", "recent-new-2");
    assert_eq!(unseen_content, seen_content);
    assert_eq!(
        unseen_buffer[(seen_content as u16, seen_row as u16)].fg,
        theme.text_muted
    );

    app.handle_key(key(KeyCode::Char('j')));
    let seen_selected = render_text_at(&app, 140, 38);
    let selected_line = seen_selected
        .lines()
        .find(|line| line.contains("recent-new-2"))
        .unwrap();
    assert!(selected_line.contains(">   "), "{seen_selected}");
    assert!(!selected_line.contains('●'), "{seen_selected}");
    assert_eq!(
        unseen_content,
        content_column(&seen_selected, "recent-new-2", "recent-new-2")
    );
}
```

Update the existing responsive footer expectation from `"> unseen"` to `"> ●"`. Keep assertions that Review and Diagnostics contain `NEW` and `reviewed`, and that Scorecard contains none of the lifecycle language.

- [ ] **Step 3: Run the focused test and confirm the intended failure**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::recent_rows_use_compact_aligned_seen_and_unseen_prefixes -- --exact
```

Expected: FAIL because `review_prefix` still returns `"unseen   "` and `"seen     "`.

- [ ] **Step 4: Implement the minimal Recent-only prefix change**

Replace `review_prefix` with:

```rust
pub(super) fn review_prefix(target: &ReviewTarget, unseen_label: &str) -> &'static str {
    if target.new_member_keys.is_empty() {
        if unseen_label == "unseen" {
            "  "
        } else {
            "reviewed "
        }
    } else if unseen_label == "unseen" {
        "● "
    } else {
        "NEW      "
    }
}
```

Do not refactor the string mode selector in this task; that follow-up is tracked by `codexctl-7xvlu`.

- [ ] **Step 5: Run focused lifecycle rendering tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::recent_rows_use_compact_aligned_seen_and_unseen_prefixes -- --exact
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::all_itemized_surfaces_share_new_and_reviewed_language -- --exact
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::lifecycle_rows_keep_layout_and_reviewed_rows_are_deemphasized -- --exact
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::itemized_footer_fit_transitions_preserve_controls_and_content -- --exact
```

Expected: all four tests PASS.

- [ ] **Step 6: Run formatting and full workspace verification**

Run:

```bash
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
```

Expected: each command exits 0 with no failed tests, formatting diff, or Clippy warning.

- [ ] **Step 7: Audit scope and documentation impact**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-tui/src/ui/brain/mod.rs .internal/specs/2026-08-11-compact-recent-unseen-indicator-design.md .internal/plans/2026-08-11-compact-recent-unseen-indicator.md
git status --short
```

Expected: production source changes are confined to `review_prefix`; test changes are confined to the existing lifecycle rendering module; the two pre-existing untracked research files remain untouched. No public documentation update is needed because the documented Recent lifecycle and keys do not change.

- [ ] **Step 8: Stop at the commit authorization boundary**

No commit is authorized by this plan. Report the verified files and proposed emoji conventional commit subject, including the actual implementation Bead ID created during execution, then wait for explicit user authorization.

Expected: verified changes remain uncommitted, with no staging, push, or publication side effect.
