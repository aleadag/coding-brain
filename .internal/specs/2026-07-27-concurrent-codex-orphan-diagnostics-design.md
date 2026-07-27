# Concurrent Codex Orphan Diagnostics Design

> Date: 2026-07-27
> Issue: `codexctl-0dis`
> Status: Approved and stress-tested design

## Context

One Codex `functions.exec` call can launch multiple nested
`tools.exec_command` calls concurrently. Current Codex hooks may then persist a
`PreToolUse` and `PostToolUse` for the outer Bash activity with different
`tool_use_id` values. `correlate_outcome` finds no exact Decision and no exact
`PreToolUse` anchor for the Post ID, but a session/turn-wide Decision guard sees
an unrelated earlier Decision and emits an actionable orphan diagnostic.

The confirmed runtime sequence contains a different-ID `PreToolUse` immediately
before the `PostToolUse`, with no Decision between them. Both nested commands
completed successfully, and there is no Brain outcome to attribute.

The mismatched opaque IDs originate in Codex's hook emission. Coding Brain
receives no other stable invocation identifier that can repair the pairing
without guessing. This design is therefore a bounded defensive mitigation for
the provider defect, not a synthetic identity repair.

## Goals

- Keep the mismatched Codex lifecycle observations auditable.
- Suppress the false actionable diagnostic without fabricating an outcome.
- Preserve exact-ID outcome correlation and sequential ID-less fallback.
- Preserve diagnostics whenever Decision evidence may belong to the unmatched
  lifecycle or when no concurrent `PreToolUse` evidence exists.
- Track the provider identity defect separately so this exception can be
  removed if Codex later guarantees paired Pre/Post IDs.

## Non-goals

- Do not correlate outcomes by command text or ordering across concurrent tools.
- Do not reuse or extend the hashed permission `request_key` for lifecycle
  outcome correlation.
- Do not claim to repair the upstream Codex hook identity defect.
- Do not suppress every missing-anchor diagnostic.
- Do not rewrite existing activity rows or change hook payload schemas.
- Do not change permission evaluation or lifecycle status projection.

## Design

Keep exact Decision identity as the first correlation path. Keep non-Bash
`PostToolUse` behavior unchanged.

In the Bash fallback, distinguish zero exact anchors from multiple exact
anchors. Multiple exact anchors remain ambiguous and diagnostic. For zero exact
anchors, recognize a benign concurrent-ID mismatch only when all of these hold:

- the provider is Codex;
- after the latest same-identity `PostToolUse`, the open lifecycle batch
  contains at least one `PreToolUse` observation with a different tool-use ID;
  and
- the activity interval from the first `PreToolUse` in that open batch to the
  current `PostToolUse` contains no potentially attributable Decision.

When all conditions hold, return observation-only correlation. The caller still
appends the current `PostToolUse` lifecycle row, but appends neither Outcome nor
Diagnostic. Once appended, that Post closes the open batch, so one stale
`PreToolUse` cannot suppress multiple later missing-anchor events.

If no such unmatched `PreToolUse` exists, retain the current missing-or-
ambiguous-anchor diagnostic. If the interval contains any Decision, retain a
diagnostic rather than guessing which concurrent tool owns it. For this check,
a Decision is potentially attributable only when its provider, session,
provider-session, and turn match the `PostToolUse`; Decisions from other
identities may be physically interleaved in the global activity log and do not
block suppression. Exact matching Decision activity continues through the
existing eligibility checks before this fallback.

The hashed permission `request_key` remains confined to PermissionRequest
replay identity. Ordinary auto-approved lifecycle hooks have no such key, and
command-derived hashes would not safely distinguish identical concurrent
commands.

## Error Handling and Safety

The exception is Codex-specific and requires positive lifecycle evidence of the
known concurrent mismatch. It never attaches an Outcome. A standalone
`PostToolUse` with no anchor remains diagnostic, as do duplicate exact anchors,
incomplete exact Decisions, ambiguous Decisions, and intervals containing
Decision evidence. Raw hook responses and commands are not newly persisted.

The residual observability trade-off is narrow: a genuine missing
`PreToolUse` could be treated as benign if it coincides with an unmatched
different-ID Codex `PreToolUse` and no Decision exists. In that case there is
still no Brain decision or outcome at risk, and both lifecycle observations
remain in the audit log.

Codex's opaque ID mismatch remains the root cause. The exception should be
removed rather than generalized once the provider supplies a stable shared
Pre/Post invocation identity.

## Testing

- Add a regression that records a prior unrelated Decision in the same Codex
  session/turn, then a different-ID `PreToolUse` and `PostToolUse` with no
  Decision between them; expect zero Outcomes and zero Diagnostics.
- Preserve the existing standalone missing-anchor test and its diagnostic.
- Prove a matching-identity Decision inside the open batch remains diagnostic.
- Prove a foreign-session Decision physically interleaved inside the batch does
  not create a false diagnostic.
- Prove the first mismatched Post closes the open batch, so a second unmatched
  Post without fresh Pre evidence remains diagnostic.
- Prove multiple exact anchors and the same mismatched pattern from non-Codex
  providers remain diagnostic.
- Preserve coverage for exact-ID correlation, incomplete exact Decisions,
  sequential ID-less PermissionRequest fallback, and matched parallel
  empty-interval behavior.
- Run focused lifecycle-hook tests, the workspace test suite, formatting,
  Clippy with warnings denied, and the workspace build.

## Documentation Impact

None. This corrects internal false-positive classification without changing
configuration, commands, schemas, or intended public behavior.

## Success Criteria

- The `codexctl-0dis` concurrent mismatched-ID sequence appends only lifecycle
  observations.
- Genuine incomplete or ambiguous Decision lifecycles remain actionable.
- Existing outcome-correlation behavior and repository quality gates pass.

## Stress Test Results: Concurrent Codex Orphan Diagnostics

### Resolved Decisions

- Define an open concurrent batch after the latest same-identity Post and close
  it with the first subsequent Post, preventing one stale Pre from suppressing
  multiple missing-anchor events.
- Treat only Decisions with matching provider/session/provider-session/turn as
  potentially attributable; remain fail-closed for every such in-batch
  Decision.
- Apply the exception only when the exact anchor count is zero; duplicate exact
  anchors remain diagnostic.
- Support only the confirmed single-Post mitigation rather than inventing
  count- or order-based pairing for additional Posts.
- Keep the hashed PermissionRequest key out of lifecycle correlation.
- Describe the change as a bounded mitigation for an upstream Codex identity
  defect, tracked separately by `codexctl-prbe`.
- Protect each suppression boundary with an adversarial regression.

### Changes Made

- Tightened the concurrent evidence to an explicit open lifecycle batch.
- Scoped Decision evidence to the full lifecycle identity rather than global
  log position.
- Added the provider-root-cause limitation, removal condition, hashed-key
  non-goal, and expanded regression controls.

### Deferred / Parking Lot

- Provider-facing reproduction and upstream tracking of the mismatched opaque
  IDs (`codexctl-prbe`).
- Removal of this mitigation once Codex supplies paired IDs or another stable
  invocation identity.

### Confidence Assessment

- Overall: High for suppressing the confirmed false diagnostic without
  weakening Decision attribution.
- Remaining concern: Coding Brain cannot repair the provider's opaque identity
  mismatch; the lifecycle observations remain intentionally unpaired.
