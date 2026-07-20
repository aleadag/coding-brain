# Lifecycle Activity and Project Label Design

## Context

Generic lifecycle hooks currently append `Abstained` activities. The Live
projection treats every unresolved abstention as needing attention, so
`SessionStart`, `Stop`, and similar audit events appear beside actual Brain
decisions. These records also omit `ProjectEvidence.label`; although
`.coding-brain/project.toml` supplies a stable project UUID, it intentionally
does not contain a display name, so the TUI renders `unknown project`.

## Decision

Add a persisted `ActivityKind` with `decision`, `lifecycle`, and `diagnostic`
variants. Keep generic lifecycle records in `activity.jsonl` as audit evidence,
but omit `lifecycle` activities from both Live activity lists. Filtering happens
inside core snapshot projection, before attention counts and lists are built, so
every snapshot consumer sees the same result. Ordinary Brain abstentions and
explicit diagnostics remain visible in Needs Attention.

The field is additive within activity schema version 1. New rows always write
an explicit kind. Existing rows default to `decision`, after which the reader
reclassifies the established `lifecycle_*` ID namespace for compatibility.
Compaction therefore upgrades retained legacy rows naturally without a log
rewrite, while old binaries continue ignoring the additive field.

When an activity has no explicit project label, the Live TUI displays the final
component of `ProjectEvidence.cwd`. If the path has no final component, it falls
back to the full path and then to `unknown project` only when neither produces a
usable label. The project manifest remains identity-only and its schema does not
change.

## Data Flow

1. Activity producers write a stable kind: permission decisions and their
   outcomes use `decision`, generic lifecycle hooks use `lifecycle`, and orphan
   attribution failures use `diagnostic`.
2. The activity reader normalizes legacy lifecycle rows that predate the field.
3. Activity snapshot projection excludes `lifecycle` before Needs Attention or
   Recent classification.
4. Live rendering prefers an explicit label and otherwise derives a display
   label from the recorded working directory.

## Error Handling and Safety

Kind affects persistence and presentation only; permission authorization never
consults it. Kind and payload must remain consistent across an activity
lifecycle: lifecycle rows cannot carry decision, outcome, or correction
evidence, and grouped rows cannot silently change kind. Diagnostics continue
through normal error classification. Raw records remain available for audit and
compaction.

Project labels are derived only from already recorded paths and continue
through the existing terminal rendering path; no additional filesystem access
or untrusted configuration is introduced. Explicit non-empty labels remain
authoritative. Missing labels use a lossy UTF-8 cwd basename, then the full path
for roots or unusual paths, and only then `unknown project`.

## Testing

- New activity kinds serialize and deserialize without changing schema version.
- A historical abstained `lifecycle_*` row is normalized to `lifecycle` and is
  absent from both Live lists.
- An ordinary decision abstention and a diagnostic error remain in Needs
  Attention.
- Mixed kinds or lifecycle rows carrying decision evidence are rejected or
  diagnosed.
- A missing project label renders the working-directory basename.
- An explicit project label remains authoritative.
- Root and unusual paths have deterministic fallbacks.
- Existing activity, lifecycle-hook, and TUI tests continue to pass.

## Stress Test Results: Lifecycle Activity and Project Labels

### Resolved Decisions

- Persist an explicit three-way activity kind instead of treating ID prefixes
  as the primary classification boundary.
- Filter lifecycle activity in core snapshot projection so counts and all
  consumers remain consistent.
- Keep diagnostics visible and reject mixed-kind activity lifecycles.
- Derive display labels from existing cwd evidence rather than expanding the
  identity manifest.
- Preserve schema version 1 and normalize legacy lifecycle IDs at read time.
- Keep kind out of every authorization path and cover the boundary with focused
  compatibility, projection, validation, and rendering tests.

### Changes Made

- Replaced prefix-only lifecycle filtering with an explicit persisted kind plus
  a legacy compatibility fallback.
- Added kind/payload consistency and deterministic path-label edge cases.

### Deferred / Parking Lot

- Revisit the activity-kind taxonomy only if future activity producers require
  categories beyond decision, lifecycle, and diagnostic.

### Confidence Assessment

- Overall: High
- Areas of concern: only the bounded legacy ID fallback, which remains necessary
  until old activity rows have aged out or been compacted.
