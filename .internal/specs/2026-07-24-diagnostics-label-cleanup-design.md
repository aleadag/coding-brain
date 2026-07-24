# Diagnostics Label Cleanup Design

## Context

The Diagnostics view already establishes its context through the active
`Diagnostics` tab and the `Recent Diagnostics` list title. Repeating
`Diagnostic` on every row and `Status: Diagnostic` in Evidence consumes space
without distinguishing one event from another.

## Design

Keep the existing Diagnostics layout, data model, selection behavior, and
empty-state wording. Change only the presentation:

- Render each populated row from provider, project, and tool, without a
  `Diagnostic` prefix.
- Omit the constant `Status: Diagnostic` field from Evidence.
- Preserve the `Diagnostics` tab, `Recent Diagnostics` title and count, store
  integrity panel, and all useful evidence fields.

No new renderer abstraction or data transformation is needed.

## Testing

Update the existing TUI rendering coverage before changing production code.
The tests must fail against the current renderer because the redundant labels
remain, then pass after the minimal renderer change.

Coverage will assert at both narrow and wide breakpoints that:

- populated rows show provider, project, and tool;
- neither the row prefix nor `Status: Diagnostic` appears;
- the Diagnostics tab and Recent Diagnostics count remain visible;
- useful Evidence fields remain visible.

Existing empty-state and interaction tests remain unchanged and must continue
to pass.

## Scope

This change does not alter diagnostic collection, persistence, sorting,
selection, scrolling, navigation, store-health reporting, or non-Diagnostics
views.
