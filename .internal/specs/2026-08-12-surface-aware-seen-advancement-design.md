# Surface-Aware Seen Advancement

## Goal

After a successful explicit single-item `a` seen action, advance selection to the item that immediately followed the selected item in the active surface's pre-mutation ordering.

## Design

Before submitting the mutation, `BrainApp` captures the current `ReviewSurface` plus the next target's display identity and index from that surface's projection. After the mutation succeeds and the normal refresh observes at least the mutation result's surface revision, it confirms that the successor identity remains in the refreshed surface and passes the captured identity and successor index to `restore_surface_selection`.

The existence check must use `review_projection(surface)`, while `restore_surface_selection` retains the existing surface-specific selection ownership: Attention and Recent have independent Live selections, while Review and Diagnostics use the tab selection. Passing the successor's index preserves the helper's nearest-index behavior when display identities repeat. The action must not search the Review projection for identities captured from other surfaces.

The existing Review-tab `s` action retains its persist-then-advance behavior through the same surface-aware path. The `a` action changes from preserving the selected identity to requesting advancement after success.

## Failure And Refresh Behavior

Advancement happens only after `submit_review_mutation` returns the successful mutation result and the refreshed projection reaches that result's revision. Busy, stale, durability-uncertain, storage, and other mutation failures retain the existing selection behavior and never advance optimistically. A successful mutation followed by a failed or older refresh also preserves selection and stores no pending advancement.

If the pre-mutation selection has no following item, no successor identity is captured and normal refresh restoration and clamping apply. If a captured successor is absent after refresh, `restore_surface_selection` is not called for advancement; the normal identity restoration and clamping result remains in force. Empty and one-item surfaces therefore remain safe without wrapping or selecting an unrelated row by index fallback.

Periodic refreshes and unrelated reorderings continue to restore selection by the currently selected display identity. No pending advancement state is stored across refreshes.

## Implementation Boundary

Change only `BrainApp`'s single-item review action, the shared mutation helper's internal return value, and focused tests in `crates/coding-brain-tui/src/brain_app.rs`. Return the existing `ReviewMutationResult` from successful submissions so the single-item caller can verify the observed revision; callers that ignore the result remain behaviorally unchanged. Do not change runtime or storage contracts, projection construction, bulk mark-seen, archive, undo, correction, navigation, or rendering behavior.

## Verification

Add regression coverage that proves:

- successful `a` advancement on Attention, Recent, Review, and Diagnostics;
- each surface resolves the successor only within its own refreshed projection;
- repeated display identities resolve to the successor nearest its captured index;
- last-item, one-item, and empty surfaces remain safely selected or clamped without wrapping;
- a successor that disappears during refresh does not cause an invalid or cross-surface selection;
- busy, stale, durability-uncertain, storage, and other failed mutations preserve selection;
- a successful mutation followed by a failed refresh preserves selection;
- Review `s`, periodic identity restoration, bulk actions, archive, undo, correction, and navigation retain existing behavior.

Run the focused TUI tests, then workspace formatting, tests, Clippy with warnings denied, and build.

## Stress Test Results: Surface-Aware Seen Advancement

### Resolved Decisions

- Architecture: reuse `restore_surface_selection` rather than duplicate surface-specific selection ownership.
- Refresh observation: advance only when the refreshed projection reaches the successful mutation result's surface revision.
- Identity handling: capture the successor's display identity and pre-mutation index so repeated identities use the existing nearest-index rule.
- Disappearing successor: require the successor identity to remain present before applying the advancement override.
- Scale: retain the existing linear surface scan; projections are bounded and no cache is warranted.
- Failure handling: cover distinct busy, stale, durability-uncertain, storage, and post-success refresh-failure paths without pending advancement state.
- API boundary: keep advancement local to `review_selected` and return `Option<ReviewMutationResult>` from the shared submission helper.
- Security and authority: preserve the selected keys and expected surface revision checks; the change adds no new authority surface.
- Testing: use existing mocks for ordinary cases and local scripted refresh fixtures for boundary cases without widening core test APIs.

### Changes Made

- Required observation of the committed surface revision before advancement.
- Added successor-index capture for repeated display identities.
- Added an existence guard so a missing successor cannot select an unrelated fallback row.
- Expanded failure and refresh-boundary regression coverage.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: refresh-boundary fixtures must keep visible rows and review projections aligned while simulating the post-mutation revision.
