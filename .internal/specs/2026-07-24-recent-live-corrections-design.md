# Recent Live Decision Corrections

## Context

The Live tab has two selectable lists: Needs Attention and Recent. The existing
correction interaction only reads `selected_attention()`, even though
`BrainApp` already exposes `selected_live_activity()` for the active row in
either list. As a result, operators cannot correct ordinary automatic decisions
shown in Recent.

The correction backend already validates Decision activity, persists Brain
right, Brain wrong, and Exception dispositions, and refreshes the existing
scorecard and Review projections. This change must reuse that path.

## Design

### Selection and correction flow

`BrainApp::begin_correction` will obtain the active item through
`selected_live_activity()`. It will reject an empty selection with a Live-wide
status message and reject non-Decision activity with the existing explicit
Decision-only message. For an eligible item, it will store that item's
`activity_id` in the existing `BrainInput::Correction`.

`BrainApp::choose_correction` will require the `activity_id` captured when the
prompt opened. If no correction input is active, it will append nothing and
report that no correction is in progress. It will not fall back to the current
selection, so later selection changes cannot redirect a correction. The
existing key handling, `CorrectionInput`, `record_correction` action,
success/error status, and refresh behavior otherwise remain unchanged.

After refresh, the existing runtime projections remain authoritative: a Brain
wrong disposition may make an eligible automatic decision appear in Review,
and all supported dispositions contribute through the existing scorecard
logic.

### Active-row highlight

Both Live lists will use the same selected-row style: the current theme's
`header` color plus bold text. This matches the existing Review-list convention
and degrades to bold when the no-color theme resets colors. Ratatui applies the
list highlight after rendering the row, so a selected row intentionally uses
the highlight foreground instead of its semantic foreground colors. Its textual
badges remain visible and continue to carry the status meaning.

Only the active list receives a selected `ListState`, as it does today.
`HighlightSpacing::Always` and the existing `> ` symbol remain on both lists, so
moving focus cannot change row indentation or target visibility.

### Errors and safety

No persistence or validation rule changes. Non-Decision activity remains
ineligible in both lists, and the runtime continues to reject non-Decision
corrections independently of the TUI. A correction targets the activity
captured when the prompt opens, so later selection movement cannot redirect it.
If that activity is no longer available when the correction is submitted, the
runtime rejects the write, the prompt remains open, and the existing error
status lets the operator retry or cancel. No fallback target is selected.

## Testing

- Add `BrainApp` regressions showing that a selected Recent Decision opens the
  existing prompt and records each supported disposition against the Recent
  activity ID.
- Preserve coverage for Needs Attention and add an explicit Recent
  non-Decision rejection assertion.
- Extend TestBackend rendering coverage to verify that the active Recent row
  receives the theme-derived foreground and bold highlight, its textual badge
  remains readable, the inactive row does not receive the highlight, and row
  content columns remain stable across focus changes in dark, light, and
  no-color modes.
- Exercise the existing runtime persistence and projection path in one focused
  test: persist a Brain wrong correction in a temporary activity store, then
  feed the resulting events through the existing Review and scorecard
  projections. This proves the refresh inputs without adding a mutable fake
  runtime or a parallel production path.

## Scope

This change is limited to Live correction selection, selected-row styling, and
focused regression coverage. It does not change correction storage, Review
eligibility rules, scorecard semantics, navigation, or list layout.

## Stress Test Results: Recent Live Decision Corrections

### Resolved Decisions

- Correction identity is captured only when the prompt opens; submission has no
  current-selection fallback.
- TUI Decision validation remains immediate feedback, while runtime validation
  remains authoritative and append-only correction semantics stay unchanged.
- The selected row uses the approved theme-header foreground plus bold style;
  textual badges preserve meaning while selected semantic foreground colors are
  intentionally flattened.
- TUI targeting, TestBackend styling, and persisted runtime projections are
  tested at their existing seams instead of through a new test-only abstraction.
- Missing source activity fails closed without an append or fallback target.

### Changes Made

- Removed the proposed selection fallback from correction submission.
- Documented selected-row color composition and the missing-source failure
  behavior.
- Tightened the test strategy around exact activity identity, cell styles, and
  persisted projection inputs.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: TestBackend assertions must identify the selected content
  row without coupling to unrelated layout details.
