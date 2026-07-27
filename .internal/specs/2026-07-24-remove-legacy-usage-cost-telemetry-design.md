# Remove Legacy Usage and Cost Telemetry

**Date:** 2026-07-24
**Issue:** `codexctl-iyk`
**Status:** Approved design

## Summary

Coding Brain will remove its remaining token-usage, cost, quota, pricing, and
burn-rate surfaces end to end. It will retain only a derived context-window
percentage because context pressure is an input to context-rot prevention; raw
token counts used to derive that percentage will be discarded immediately and
will not be displayed or persisted.

The change is surgical rather than a monitor rewrite. Existing identity,
status, tool, error, lifecycle, permission, outcome, provider recovery, and
navigation behavior remains intact.

## Goals

- Remove usage and cost data from every active CLI, TUI, review, insight, JSON,
  decision, correction, and runtime surface.
- Remove dormant pricing, cost-ledger, history, helper, hook-placeholder, and
  rule paths that could reintroduce the unsupported product surface.
- Preserve context-rot detection through a derived context-window percentage.
- Keep legacy persisted records readable without a destructive migration.
- Preserve lifecycle, permission, outcome-correlation, provider-recovery, and
  navigation behavior.
- Make the public documentation truthfully state that Coding Brain does not
  collect or display usage or cost.

## Non-Goals

- Do not remove transcript or process monitoring.
- Do not remove model identity where it supports Brain decisions or provider
  behavior.
- Do not remove context-pressure analysis or context-rot prevention.
- Do not change permission enforcement, lifecycle projection, outcome
  correlation, provider discovery, recovery, or terminal navigation.
- Do not rewrite, delete, or migrate existing state files.
- Do not add replacement usage, billing, quota, or provider-account telemetry.

## Chosen Approach

Use an end-to-end surgical removal. Delete usage and cost fields and logic from
active data models and consumers, then remove dormant helpers whose only
purpose was usage or cost tracking. Retain the existing monitor structure and
narrow it to the non-telemetry behavior Coding Brain still owns.

This approach is preferred over merely hiding fields because dormant DTOs and
helpers would leave the obsolete capability available. It is preferred over
deleting the monitor stack because that stack also supplies status, lifecycle,
tool, error, context-pressure, and recovery evidence.

## Architecture

### Transcript and provider ingestion

Transcript and provider readers continue to extract:

- session and provider identity;
- status and lifecycle evidence;
- model identity where still required;
- tool calls and results;
- errors and actionable input state; and
- context-window pressure.

Readers must not return or retain raw input, output, cache, reasoning, or total
token counters as product DTO fields. When a provider event supplies raw counts
needed to compute current context pressure, the monitor calculates a percentage
transiently and discards the counts. Context pressure is represented as a
bounded `Option<u8>`: `None` means unavailable and must not be interpreted as
`0%`.

The monitor prefers a provider-supplied context-window maximum, then a known
model's context-window mapping. An unknown model without a provider maximum
produces no context-pressure value rather than using a guessed denominator.

### Session and runtime models

The session model loses raw token totals, cost totals, pricing-verification
state, burn rate, and cost-ledger bookkeeping. It retains a directly
represented optional context-window percentage instead of reconstructing that
percentage from persisted token counters.

Runtime DTOs used by the TUI, review, headless JSON, and adapters lose cost and
usage fields. The TUI remains a Brain-activity surface and gains no replacement
session dashboard.

### Brain decision context and analytics

Decision context retains `context_pct` plus existing non-telemetry evidence
such as tool errors, model, elapsed time, modified-file count, status, and
subagent count. It drops cost and burn rate.

The following consumers lose their cost-specific branches:

- decision persistence and projection;
- interactive and printed review;
- insights and detectors;
- learned preference conditions;
- sequence analysis;
- outcome summaries and CLI rendering;
- correction and runtime projections; and
- prompt/session summaries.

Context-pressure preferences and detectors remain because they serve
context-rot prevention rather than usage reporting.

### Dormant legacy infrastructure

Remove modules or module portions whose only remaining responsibility is
usage/cost telemetry, including:

- model pricing data and cost estimation;
- token/cost session history and summaries;
- cost-oriented JSON helpers;
- cost and token hook template placeholders;
- budget/cost rules and events;
- cost-ledger and burn-rate helpers; and
- tests and fixtures that exist only to validate those paths.

If a module also owns retained behavior, narrow the module instead of deleting
it.

Model support is narrowed to model-name normalization and known context-window
lookup. Pricing fields, long-context price multipliers, override storage, and
fallback pricing are removed.

The affected workspace crates are pre-1.0. Public telemetry fields and modules
are removed without deprecated compatibility shims. This is an intentional
breaking API cleanup; release versioning and publication remain outside this
task.

## Compatibility

Legacy JSON and JSONL records may contain removed usage, cost, and burn-rate
fields. Serde and value-based readers must ignore those unknown fields and
construct current records from the remaining data.

Legacy decision context must not be rejected merely because an old cost field
is present. Conversely, current readers must not require a removed field before
constructing otherwise valid context. New writers never emit the removed
fields.

Retained decision-context fields are parsed independently so one absent removed
field cannot collapse the whole context. A legacy preference containing a
removed cost condition is discarded as a whole preference; dropping only the
condition could silently broaden a multi-condition preference. Legacy derived
cost insights and summaries are ignored or rebuilt rather than copied forward.

There is no state rewrite or destructive migration. Existing files remain in
place and older records remain available to the extent their retained fields
are valid.

Forward compatibility is supported: the upgraded version reads old records.
Semantic downgrade is not supported because an older binary can resume
collecting and writing cost data. Rolling back does not migrate or corrupt
state, and a later re-upgrade ignores any reintroduced legacy fields.

## Error Handling and Security

- Malformed retained fields continue to follow the existing safe parse and
  recovery behavior.
- Removed fields are ignored; they do not influence decisions.
- Raw provider token counts used for transient context-pressure calculation are
  not logged, serialized, or copied into diagnostic output.
- One conversion path validates a non-zero denominator, uses overflow-safe
  arithmetic, clamps over-capacity input to `100%`, and returns only
  `Option<u8>`.
- Existing redaction and bounded-input rules remain unchanged.
- Permission enforcement and ambiguity handling remain fail-closed.
- Lifecycle observations remain outside authorization decisions.
- No external billing, quota, or pricing endpoint is introduced.

The latest valid context event updates the percentage. Incremental refreshes
without new context evidence retain the last valid percentage. Transcript
truncation, replacement, or session reconstruction clears the previous value
before rescanning. Malformed evidence is ignored and never affects
authorization. No new cache, index, or freshness subsystem is introduced.

## Documentation

Public product-boundary language will be strengthened from the historical
provider-specific wording to a direct statement: Coding Brain does not collect
or display token usage or cost.

Documentation may still describe context pressure and context-rot prevention,
but it must not imply that raw token counts, quota, pricing, or billing data are
retained.

Historical marketing and blog text that presents cost, burn rate, or a session
dashboard as current behavior is updated as well. Minimal provider-native input
fixtures and explicit legacy persistence fixtures may retain usage/cost fields
when required to test transient context derivation or compatibility; they are
not product output.

## Testing

Implementation follows test-driven development.

Focused regression coverage must prove:

1. Legacy decision and state records containing removed fields still load.
2. New decision, correction, runtime, and JSON output omits usage and cost
   fields.
3. Provider input can still produce derived context pressure without retaining
   raw token counters.
4. Context-rot health checks, preferences, and detectors still respond to
   context pressure.
5. Review, insights, outcomes, sequences, and runtime DTOs contain no
   cost-specific behavior.
6. Status inference, lifecycle projection, permission enforcement, outcome
   correlation, provider recovery, and navigation retain their existing
   behavior.
7. Public documentation consistently states the new boundary.
8. A focused production-source boundary test rejects obsolete cost, burn-rate,
   pricing, and cost-preference identifiers.

Obsolete pricing and token-accounting tests are removed or replaced with tests
for the retained behavior that previously shared their fixtures.

The source boundary does not ban every `input_tokens` occurrence because
provider parsers may consume raw counts transiently. Narrowly justified
provider-native and legacy compatibility fixtures are excluded from that scan;
behavioral tests prove their fields never enter retained DTOs or output.

Before implementation, capture focused passing baselines for status, lifecycle,
permission, recovery, and context-rot behavior. Add no feature flag: the
unsupported telemetry surface must not remain toggleable. Rollback is a code
revert only, subject to the documented downgrade limitation.

Final verification:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
```

Run these through the worktree development environment, using `direnv exec .`
or `nix develop path:.` when bare Cargo is unavailable.

## Success Criteria

- No active or dormant product path displays, analyzes, or writes token usage,
  cost, quota, pricing, or burn rate.
- Raw token counts are not retained; only derived context-window percentage is
  allowed for context-rot behavior.
- Current output and persisted records omit removed fields.
- Legacy records remain safely readable without migration.
- Non-telemetry monitoring and Brain behavior remain covered and unchanged.
- Focused compatibility tests and all workspace quality gates pass.

## Stress Test Results: Legacy Usage and Cost Telemetry Removal

### Resolved Decisions

- Represent context pressure as bounded `Option<u8>`; unavailable evidence is
  never treated as `0%`.
- Parse retained legacy decision fields independently, discard whole
  cost-conditioned preferences, and ignore or rebuild derived cost artifacts.
- Support upgraded readers over old state, while documenting semantic downgrade
  as unsupported.
- Remove public telemetry APIs without shims as a simple pre-1.0 breaking
  cleanup; release work remains separate.
- Reduce model profiles to normalization and known context-window lookup, with
  no guessed denominator for unknown models.
- Prove removal through behavioral tests plus a focused production-source
  boundary.
- Convert raw provider counts through one overflow-safe, non-logging function
  that returns only the derived percentage.
- Preserve incremental monitoring semantics while clearing stale pressure on
  transcript reset or session reconstruction.
- Use test-driven removal, full workspace gates, no feature flag, and code-only
  rollback.
- Update stale marketing text while retaining realistic provider-native and
  legacy compatibility fixtures.

### Changes Made

- Made context pressure optional and specified unknown-model behavior.
- Added legacy preference safety and downgrade semantics.
- Clarified the pre-1.0 public API break and model-profile simplification.
- Added source-boundary, transient-conversion, monitor-reset, rollback, and
  documentation/fixture requirements.

### Deferred / Parking Lot

- Release versioning and publication.
- Backward semantic compatibility with older telemetry-collecting binaries.

### Confidence Assessment

- **Overall:** High.
- **Areas of concern:** The implementation spans many legacy modules, so the
  production-source boundary and full retained-behavior baseline are required
  to catch omissions and accidental monitoring regressions.
