# Stable Live List Indentation Design

## Goal

Keep item text in the Live tab's `Needs Attention` and `Recent` lists at a fixed horizontal position when keyboard navigation moves focus between the lists. Only the selected row should display the `> ` marker.

## Root Cause

Both lists configure a two-character highlight symbol, but each `ListState` has a selection only while the global Live selection points into that list. Ratatui 0.29 defaults `List::highlight_spacing` to `HighlightSpacing::WhenSelected`, so the focused list reserves two columns for `> ` while the inactive list does not. Moving focus therefore shifts both lists' content.

## Design

Configure both Live-tab `List` widgets with `HighlightSpacing::Always`. Ratatui will reserve the same two-column marker gutter whether or not that list currently owns the selection. The active row will continue to show `> ` and the inactive list will render blank gutter space.

The change stays in `crates/coding-brain-tui/src/ui/brain/live.rs`. It does not change `BrainApp` selection state, keyboard navigation, list contents, colors, borders, or detail rendering. It adds no state, dependencies, error paths, or security-sensitive behavior.

## Testing

Add a render-buffer regression test in the existing TUI UI test module. The fixture contains one attention item and one recent item. The test renders the Live tab with the attention item selected, moves the selection down to the recent item, renders again, and verifies that item content in both lists starts at the same column in both focus states.

Follow test-first development: the new assertion must fail against Ratatui's default `WhenSelected` behavior before the production change is added. After the fix, run the focused regression test, the TUI crate tests, `cargo fmt --check`, and `cargo clippy -- -D warnings`.

## Alternatives Rejected

- Manually prefix every row with spaces or `> `. This duplicates behavior already provided by Ratatui and requires custom selection-aware item construction.
- Keep a selected row in both list states and hide the inactive style. This misrepresents focus and couples visual layout to false selection state.

## Acceptance Criteria

- Moving selection between `Needs Attention` and `Recent` does not move either list's item text horizontally.
- Exactly one selected row displays the `> ` marker.
- Existing navigation and selection behavior remain unchanged.
- The render-buffer regression and project quality gates pass.
