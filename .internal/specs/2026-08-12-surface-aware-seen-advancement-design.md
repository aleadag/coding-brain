# Surface-Aware Seen Advancement

## Goal

After a successful explicit single-item `a` seen action, advance selection to the item that immediately followed the selected item in the active surface's pre-mutation ordering.

## Design

Before submitting the mutation, `BrainApp` captures the current `ReviewSurface` and the next target's display identity from that surface's projection. After the mutation succeeds and the normal refresh completes, it searches the refreshed projection for that identity and updates the selection owned by the same surface.

The lookup must use `review_projection(surface)` and the existing surface-specific selection ownership: Attention and Recent have independent Live selections, while Review and Diagnostics use the tab selection. It must not search the Review projection for identities captured from other surfaces.

The existing Review-tab `s` action retains its persist-then-advance behavior through the same surface-aware path. The `a` action changes from preserving the selected identity to requesting advancement after success.

## Failure And Refresh Behavior

Advancement happens only after `submit_review_mutation` reports success. Busy, stale, durability-uncertain, storage, and other mutation failures retain the existing selection behavior and never advance optimistically.

If the pre-mutation selection has no following item, no successor identity is captured and normal refresh restoration and clamping apply. If a captured successor is absent after refresh, no post-refresh selection override occurs; the normal identity restoration and clamping result remains in force. Empty and one-item surfaces therefore remain safe without wrapping.

Periodic refreshes and unrelated reorderings continue to restore selection by the currently selected display identity. No pending advancement state is stored across refreshes.

## Implementation Boundary

Change only `BrainApp`'s single-item review action and focused tests in `crates/coding-brain-tui/src/brain_app.rs`. Do not change runtime or storage contracts, projection construction, bulk mark-seen, archive, undo, correction, navigation, or rendering behavior.

## Verification

Add regression coverage that proves:

- successful `a` advancement on Attention, Recent, Review, and Diagnostics;
- each surface resolves the successor only within its own refreshed projection;
- last-item, one-item, and empty surfaces remain safely selected or clamped without wrapping;
- a successor that disappears during refresh does not cause an invalid or cross-surface selection;
- busy, stale, durability-uncertain, storage, and other failed mutations preserve selection;
- Review `s`, periodic identity restoration, bulk actions, archive, undo, correction, and navigation retain existing behavior.

Run the focused TUI tests, then workspace formatting, tests, Clippy with warnings denied, and build.
