# Compact Recent Unseen Indicator

## Goal

Reduce the horizontal space used by the Live tab's Recent row lifecycle marker while keeping unseen items identifiable without relying on color alone.

## Design

Recent rows use a fixed two-column lifecycle prefix:

- unseen: `● `, rendered with the existing bold header-color style;
- seen: two spaces, with the row retaining its existing muted style.

The fixed width keeps seen and unseen row bodies aligned. The `Recent (N unseen)` title remains unchanged so the meaning of the bullet and the aggregate unseen count stay explicit.

Attention, Review, and Diagnostics keep their existing `NEW` and `reviewed` prefixes. Review-state persistence, seen actions, ordering, selection, evidence rendering, and lifecycle counts do not change.

## Implementation Boundary

Change only the Recent prefix selection in the TUI rendering layer. Do not alter review-state contracts, projections, runtime mutations, storage, themes, or other surfaces.

## Failure And Security Impact

This is a presentation-only change. It adds no input, storage, error path, authority decision, or security boundary.

## Verification

Update TUI rendering tests to verify:

- unseen Recent rows render `● `;
- the unseen prefix has a two-column terminal display width;
- seen Recent rows reserve the same two columns without a marker;
- row bodies remain aligned and distinguishable when either state is selected;
- `Recent (N unseen)` remains present;
- Attention, Review, and Diagnostics retain their current lifecycle labels;
- narrow and wide layouts render without regression.

## Stress Test Results: Compact Recent Unseen Indicator

### Resolved Decisions

- Architecture: reuse the existing prefix helper and keep the change confined to Recent presentation.
- Terminal assumptions: retain `●` because the TUI already requires Unicode-capable terminal rendering; assert its display width.
- Dependencies: keep the existing string-based internal mode selector for this surgical change.
- Edge cases: use a blank seen prefix so selection styling cannot erase the non-color distinction; test both selected states.
- Scale: add no allocation, state, or benchmark work for static prefixes on the existing bounded list.
- Failure and rollback: rely on the unchanged title and footer for meaning; do not add a legend or fallback path.
- Alternatives: keep review state in a dedicated prefix rather than mixing it with activity badges, selection, or color alone.
- Security and authority: treat this as presentation-only and audit the final diff boundary.
- Testing: extend existing lifecycle and responsive render coverage rather than adding a parallel suite.

### Changes Made

- Expanded verification to cover Unicode display width and selected mixed-state rows.

### Deferred / Parking Lot

- `codexctl-7xvlu`: replace the existing `review_prefix` string mode with a typed selector in a separate refactor.

### Confidence Assessment

- Overall: High
- Areas of concern: terminal font glyph availability remains environment-dependent, consistent with the TUI's existing Unicode requirement.
