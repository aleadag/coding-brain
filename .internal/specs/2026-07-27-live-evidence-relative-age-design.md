# Live Evidence Relative Age Design

Date: 2026-07-27  
Issue: `codexctl-zsyk`  
Brainstorm: `codexctl-kp54`

## Goal

Replace the redundant `Recent` suffix in selected resolved Live Evidence with
the activity's relative age while preserving the actionable
`Needs attention` label.

## Design

Capture the current Unix epoch time once per Live render and pass it through the
existing Evidence rendering path. The shared Evidence line builder derives the
selected Recent item's age from `ActivityItem.recorded_at_ms`, so wide and
compact layouts use identical wording.

Format elapsed time with floored whole units:

- less than 60 seconds: `Ns ago`
- less than 60 minutes: `Nm ago`
- less than 24 hours: `Nh ago`
- 24 hours or more: `Nd ago`

Use saturating subtraction so future or clock-skewed timestamps render as
`0s ago`. Attention selections continue to render `Needs attention`; the
change does not alter list membership, ordering, badges, Evidence fields,
navigation, persistence, or activity projection.

The application already refreshes Live once per second. Recomputing the age
during each render therefore advances the displayed value without adding
timers or state to `BrainApp`.

## Verification

Add pure formatting tests for representative seconds, minutes, hours, days,
boundary values, and future timestamps. Add renderer coverage with a controlled
current time proving that both wide and compact Recent Evidence show the same
age and that Attention Evidence remains unchanged.

Run the focused TUI tests, workspace tests, formatting check, Clippy with
warnings denied, and build.
