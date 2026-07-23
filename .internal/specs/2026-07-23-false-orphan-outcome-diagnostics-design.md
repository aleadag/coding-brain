# False Orphan Outcome Diagnostics Design

> Date: 2026-07-23
> Issue: `codexctl-81g`
> Related: `codexctl-5ah` is implemented separately by PR #24
> Status: Approved and stress-tested design

## Context

Antigravity permission handling intentionally persists unsupported tools
as terminal `Abstained` decisions with reason `unsupported permission tool`.
When the matching `PostToolUse` arrives, exact provider/session/turn/tool-use
identity selects that activity, but the outcome correlator accepts only an
`Allowed` terminal decision with a `decision_id`. It therefore reports the
unique, expected abstention as an ambiguous or ineligible orphan.

## Goals

- Keep `PostToolUse` lifecycle observations auditable.
- Suppress the confirmed false diagnostic without attaching a fabricated
  outcome.
- Preserve diagnostics for genuine exact-match ineligibility, ambiguity, and
  inconsistent histories.
- Leave permission behavior, activity schema, and persisted history unchanged.

## Non-goals

- Do not make all non-`Allowed` exact matches non-diagnostic.
- Do not correlate outcomes to denied, errored, or abstained decisions.
- Do not rewrite existing diagnostic rows.

## Design

### Unsupported Antigravity tools

Define one shared internal constant for the `unsupported permission tool`
semantic marker and use it both when persisting the abstention and when
correlating `PostToolUse`. After exact identity selects one activity, inspect
its first terminal decision. Return observation-only correlation when all of
these hold:

- the lifecycle provider is Antigravity;
- exactly one distinct activity ID has the exact identity;
- the first terminal decision is `Abstained`; and
- its reason equals the shared unsupported-tool marker.

The existing Decision history and `PostToolUse` lifecycle observation remain in
the activity log. No Outcome or Diagnostic is appended. Every other exact match
continues through the existing `Allowed` candidate validation, so denied,
errored, other abstained, incomplete, conflicting, cross-provider, and
multi-activity histories remain diagnostic. Later terminal rows do not
retroactively change the first-terminal classification.

## Error Handling and Safety

Exact stable identity remains primary. The exception does not attach an Outcome.
The Antigravity exception is provider-, state-, and reason-specific. Any
ambiguous or inconsistent exact history continues to fail closed with a
diagnostic. No raw hook payload data, commands, or responses are newly
persisted.

## Testing

- Reproduce repeated Antigravity `view_file` and `grep_search` steps in one
  session: each exact unsupported abstention retains its Decision and
  `PostToolUse` observation, with zero Outcomes and zero Diagnostics.
- Prove a different exact abstention reason remains diagnostic.
- Prove the same abstention marker under Codex remains diagnostic.
- Prove distinct activity IDs with the same exact lifecycle identity remain
  ambiguous and diagnostic.
- Run the focused lifecycle tests, provider hook activity tests, and workspace
  quality gates.

## Documentation Impact

None. This corrects false internal diagnostics without changing configuration,
commands, schemas, or intended user-facing behavior.

## Success Criteria

- The live Antigravity pattern from `codexctl-81g` no longer creates actionable
  orphan errors.
- Genuine ambiguous or ineligible outcome correlation remains diagnostic.
- Existing provider lifecycle and outcome tests pass.

## Stress Test Results: False Orphan Outcome Diagnostics

### Resolved Decisions

- Use one shared internal unsupported-tool marker rather than duplicating a
  prose literal across permission and lifecycle modules.
- Keep unsupported-tool suppression Antigravity-only until another provider has
  confirmed live evidence.
- Classify the first terminal row for the one exact activity ID; conflicting or
  duplicate exact histories remain diagnostic.
- Keep the exception post-execution, observation-only, and independent of
  permission responses or raw payload persistence.
- Verify narrow positive cases and adversarial controls before running all
  workspace gates.

### Changes Made

- Added a shared semantic marker and explicit first-terminal, provider, and
  unique-activity requirements.
- Expanded regression controls for cross-provider rows, alternate reasons,
  and duplicate exact identities.

### Deferred / Parking Lot

- Generalizing unsupported-tool suppression to providers without confirmed live
  evidence.
- Parallel fallback behavior, tracked by `codexctl-5ah` and PR #24.

### Confidence Assessment

- Overall: High.
- Remaining concern: the exception relies on an internal semantic marker; a
  shared constant and adversarial controls make drift explicit.
