# Preserve Recent Selection Across Refresh

## Context

The Live tab stores the selected Recent row as a numeric index. Recent activities
are sorted newest-first, so a successful refresh can insert a new row ahead of
the selection. Retaining the old index then highlights a different activity and
changes the Evidence pane without operator input.

## Design

Immediately before replacing a successfully refreshed snapshot,
`BrainApp::refresh_state` captures the currently selected Recent activity's
`activity_id`. After installing the new snapshot, it looks up that identity in
the refreshed Recent rows and, when found, updates `live_recent_selection` to
the matching index.

If the activity is absent, the code leaves the numeric selection unchanged and
the existing `clamp_selection` path supplies the current deterministic fallback.
Failed or busy refreshes do not replace the snapshot and therefore do not
reconcile the selection.

The change applies only to Recent. Attention focus, remembered selections,
navigation, corrections, actions, refresh cadence, and Evidence viewport reset
behavior remain unchanged.

## Testing

Refresh-level TUI tests will cover:

- inserting a newer Recent activity ahead of the selected activity preserves
  the selected `activity_id` and Evidence identity;
- removing the selected Recent activity uses the existing valid-row fallback;
- normal navigation remains covered by the existing Live selection tests.

The focused tests will be observed failing before production code changes, then
the full `coding-brain-tui` test suite, formatting check, and Clippy will verify
the implementation.
