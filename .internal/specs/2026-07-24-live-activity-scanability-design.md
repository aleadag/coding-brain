# Live Activity Scanability Design

Date: 2026-07-24
Brainstorm: `codexctl-3rau`
Implementation: `codexctl-cpt5`

## Problem

The Live view renders status, provider, project, command, and occurrence count as
one undifferentiated string. Verbose lifecycle text and long commands dominate
the row, making Needs Attention and Recent difficult to scan. The Evidence pane
uses a flat key/value list, so the selected activity's conclusion is not visually
distinct from its metadata.

## Goals

- Make the activity state and project identifiable at a glance.
- Keep list density close to the current one-line layout.
- Make the selected activity's status, action, and outcome easy to distinguish.
- Preserve complete information in Evidence.
- Degrade predictably at narrow terminal widths.
- Never rely on color alone.

## Non-goals

- Changing activity inference or lifecycle semantics.
- Changing Needs Attention/Recent membership or ordering.
- Changing the responsive breakpoint, pane proportions, selection behavior, or
  `J`/`K` list switching introduced by `codexctl-q6o`.
- Adding animation, blinking, icons that require special fonts, or new themes.
- Adding theme configuration. A future theme-config feature may extend the
  existing semantic palette, but is outside this implementation.

## List rows

Use a structured, one-line row instead of a formatted sentence:

```text
> ERROR     coding-brain  Codex  Bash                            x3
  DONE      nix-configs   Antigravity  df -h
```

The fields are ordered by scanning priority:

1. Compact textual status badge.
2. Project, rendered in bold.
3. Provider, rendered with the theme's muted style.
4. Summarized action, consuming the flexible remainder.
5. Occurrence count, right-aligned when present.

The selection marker remains separate from row content. Color may reinforce the
status badge, but its text must carry the meaning.

Reuse the existing semantic theme palette; do not add palette fields. The
status badge uses semantic color plus bold text, project is bold, and provider
uses `text_muted`. Selection keeps the persistent `>` marker without overriding
per-field foreground colors. Badge text, headings, spacing, and the marker must
remain sufficient in `ThemeMode::None`.

### Status labels

Derive the badge from the same facts used by `activity_status`; do not alter
classification. Outcome and correction remain higher precedence than delivery
and decision state:

- successful or completed outcome: `DONE`
- failed outcome: `FAILED`
- cancelled outcome: `CANCEL`
- correction: `RESOLVED`
- denied and delivered: `BLOCKED`
- failed delivery: `SEND FAIL`
- unknown delivery: `SEND ?`
- otherwise map state exhaustively: observed → `OBSERVE`, evaluating → `EVAL`,
  allowed → `ALLOW`, denied → `DENY`, abstained → `ABSTAIN`, error → `ERROR`,
  delivered → `SENT`, delivery failed → `SEND FAIL`, interrupted → `STOPPED`,
  outcome → `OUTCOME`, and correction → `RESOLVED`

This precedence selects the most actionable current condition. The full
decision, delivery, and outcome combination remains visible in Evidence.
Badges occupy a common display width based on the longest supported label.

### Width degradation

Allocate widths explicitly from the row's render area rather than relying on
terminal clipping.

Calculate all widths using terminal display columns, including Unicode text.
Reserve the selection marker, fixed-width badge, separators, and occurrence
count before allocating flexible text. Degrade in explicit stages:

1. Full: badge, project, provider, action, and right-aligned count.
2. Omit provider.
3. Cap project at 16 display columns and give the remainder to action.
4. At extreme widths, show badge, project, and count; leave action to Evidence.

Project and action use a visible ellipsis when truncated and at least two
display columns are available. The renderer must never split a displayed
character or depend on terminal clipping. The full values remain available in
Evidence.

Action summarization is deterministic and presentation-only. It uses the
existing normalized command or tool label, collapses embedded whitespace for a
single-line row, and truncates to the allocated display width. It does not parse,
rewrite, or reinterpret the command.

Presentation continues to consume the existing bounded and redacted activity
fields; it adds no persistence, logging, clipboard, or export path. Before
rendering, replace non-whitespace control characters with safe visible escapes.
One-line rows collapse whitespace; Evidence preserves the stored meaning using
visible escapes rather than emitting raw terminal-control characters. Do not
add a second TUI-specific secret-redaction policy.

## Evidence pane

Use a restrained sectioned hierarchy:

```text
┌ Evidence ─────────────────────────┐
│ ERROR                  Needs attention
│
│ OUTCOME
│ orphan lifecycle identity is ambiguous
│
│ ACTION
│ Bash
│
│ CONTEXT
│ Project   coding-brain
│ Provider  Codex
│ Activity  orphan_...
└───────────────────────────────────┘
```

- The header line uses the same textual status badge as the selected row and a
  single status-colored accent.
- `OUTCOME`, `ACTION`, and `CONTEXT` are muted section labels, in that order.
- Field labels are muted; values use normal foreground. Project is bold.
- The action is placed on its own visually distinct line or inset surface.
- Outcome contains the verbose lifecycle status plus available confidence,
  reason, correction, and note values. Optional fields are omitted when absent.
- Activity ID remains available under Context but does not compete with status.
- Wrapping remains enabled for long values.
- Empty selection keeps the existing instructional message.
- A shared semantic line builder supports two density modes. Wide mode uses
  section-heading lines and restrained blank spacing. Narrow mode keeps the
  same Status, Outcome, Action, Context order and styles but uses compact inline
  labels with no decorative blank rows. This keeps Status and Outcome visible
  within the existing Evidence height cap; Action and Context follow in the
  remaining space.

The design uses no animation or blinking. Borders, text labels, and spacing
remain sufficient when colors are unavailable.

## Layout and data flow

Rendering remains local to the TUI. Small, pure TUI presentation helpers receive
the existing `ActivityItem`, available width, and theme to derive badge
text/style, visible fields, truncated action text, and sectioned Evidence lines.
They do not mutate or reinterpret lifecycle data. No compact-status type or
wording is added to `coding-brain-core`, and no core model or runtime-trait
changes are required.

The existing responsive layout remains:

- wide terminals: lists at 67%, Evidence at 33%
- narrow terminals: bounded Evidence below the two lists

Evidence height calculation must use the new sectioned lines so the narrow pane
continues to reserve enough space without exceeding its existing cap.

When wrapped Evidence exceeds its viewport, it remains reachable through
vertical scrolling:

- `PageUp` and `PageDown` move Evidence by one visible page.
- The scroll offset resets whenever the selected activity changes.
- Subtle `↑ more` and `↓ more` indicators appear in the Evidence border title
  when content exists above or below the viewport.
- `j`/`k` continue to select activities and `J`/`K` continue to switch lists.
- The footer documents Evidence scrolling.
- Wide mode uses the same behavior only when its Evidence content exceeds the
  available height.

Rendering remains stateless and linear. Activity snapshots cap attention and
recent lists at 100 items each, and activity fields are already bounded at 4
KiB. Each displayed field gets at most one display-width/truncation pass per
render. Do not add regex-based parsing, shell parsing, or a rendering cache;
profile the maximum-size snapshot before considering caching later.

## Verification

Add focused rendering/unit coverage for:

- exhaustive status-label mapping
- field order and styles for attention and recent rows
- occurrence counts
- long commands and projects
- provider omission before project/action at constrained widths
- Unicode display-width truncation
- safe rendering of whitespace and non-whitespace control characters
- Evidence sections with all optional fields and with none
- narrow Evidence height after sectioning
- selection visibility without depending on status color
- dark, light, and no-color theme rendering without hierarchy loss
- Outcome remaining visible inside capped narrow Evidence
- `PageUp`/`PageDown` clamping, overflow indicators, and scroll reset after
  selection changes
- existing list switching, selection synchronization, overflow, and breakpoint
  behavior

Use pure-helper unit tests for status mapping, control-character safety, Unicode
truncation, and width stages. Use Ratatui `TestBackend` integration tests at an
extreme narrow width, a typical narrow width, 119, 120, and a representative
wide width. Prefer focused structural assertions over full-screen golden
snapshots. Manually inspect one populated Live screen.

Run:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
```

No feature flag, state migration, or rollback procedure is required. The change
is confined to TUI presentation helpers and the Live renderer and can be
reverted independently.

## Acceptance criteria

- A user can identify status and project without parsing lifecycle prose.
- Long commands cannot crowd status and project out of a row.
- Full status, command, activity ID, and optional evidence remain visible in the
  Evidence pane and reachable when content exceeds its viewport.
- The selected row and status remain understandable in a monochrome terminal.
- Existing responsive layout and navigation behavior are unchanged.

## Stress Test Results: Live Activity Scanability

### Resolved Decisions

- Evidence order is Status, Outcome, Action, Context so the actionable reason
  survives narrow clipping pressure.
- Status badges represent the highest-priority current condition; full compound
  lifecycle text remains in Evidence.
- All derived presentation stays in pure TUI helpers; core inference and runtime
  interfaces remain unchanged.
- Row width degrades in deterministic Unicode-display-width stages.
- Evidence uses one semantic builder with wide and compact density modes.
- Rendering stays stateless and linear under existing list and field caps.
- Existing semantic colors are reused; selection does not erase field
  hierarchy, and monochrome remains understandable.
- Core redaction remains authoritative; display helpers safely escape terminal
  control characters and create no new persistence.
- Focused unit and TestBackend integration tests replace brittle full-screen
  snapshots.
- Overflowing Evidence is reachable with `PageUp`/`PageDown`, visible overflow
  indicators, and selection-change reset.

### Changes Made

- Reordered Evidence around urgency rather than metadata.
- Replaced vague delivery labels with `SEND FAIL` and `SEND ?`.
- Defined exact width-degradation and theme behavior.
- Added density modes, performance bounds, control-character handling, and
  explicit verification requirements.
- Added Evidence scrolling after reflexion exposed a contradiction between the
  narrow height cap and the requirement that full details remain reachable.

### Deferred / Parking Lot

- User-configurable themes are intentionally deferred to a future feature.
- Rendering caches are deferred unless profiling the bounded worst case shows a
  measurable problem.

### Confidence Assessment

- Overall: High
- Areas of concern: Unicode truncation and wrapped-height/scroll calculations
  need focused edge-case tests during implementation.
