# Antigravity approval lifecycle correlation

- Date: 2026-07-23
- Bead: `codexctl-3fbo`
- Status: Approved

## Summary

Antigravity tool and permission hooks identify work as `step-N`, while
invocation hooks identify the containing turn as `invocation-N`. Coding Brain
will treat trusted Antigravity step events within the invocation's trajectory
range as children of that invocation without weakening the generic
ambiguous-turn guard. A model allow will become terminal activity only after its
executable lifecycle decision is persisted, so a fail-safe `ask` can never
project as an effective allow.

## Problem

An Antigravity invocation opens lifecycle turn `invocation-N`. A later
`PreToolUse`, `PostToolUse`, or permission event for the same provider-qualified
session carries `step-N`. The generic lifecycle projection interprets that
different turn identifier as an attempt by a non-prompt event to replace an
open turn and rejects it as `AmbiguousTurn`.

The permission hook currently appends terminal `Allowed` activity before it
records the executable lifecycle decision. If lifecycle persistence is rejected,
the hook correctly returns Antigravity's fail-safe `ask` and appends `Error`, but
activity projection intentionally keeps the first terminal event. Live therefore
shows an undelivered allow even though Antigravity is still asking the user.

## Design

### Provider-aware child-step correlation

The provider adapter will distinguish invocation boundaries:

- `PreInvocation` opens `invocation-N` and carries `initialNumSteps`, the
  trajectory length when the invocation starts.
- `PostInvocation` closes the same `invocation-N`.

Lifecycle projection will recognize one narrow child relationship:

- the provider is Antigravity;
- the session already has an open current turn named `invocation-N`;
- the incoming trusted event has a turn named `step-N`; and
- the event is a tool or permission event; and
- the numeric step is greater than or equal to the invocation's
  `initialNumSteps`.

Such an event belongs to the open invocation. Projection applies its status,
sequence, and timestamp normally but retains the invocation as
`current_turn`. The event's original `step-N` identity remains available for
tool-use correlation.

While the invocation is open, projection stores a compact event bitmask for
each accepted step. Permission `Decided`, permission `NeedsInput`, pre-tool,
and post-tool evidence use distinct bits. Repeated evidence is rejected after
intervening events. The only permitted permission transition is the compensating
`Decided` to `NeedsInput`; the reverse is rejected so a repeated hook cannot
escalate a prior prompt into an automatic allow. The map is capped at 256
distinct steps per invocation and cleared when `PostInvocation` closes the
turn. Reaching the cap prevents further permission correlation and therefore
fails safe to `ask`, but never prevents invocation closure.

All other differing-turn behavior remains unchanged. A step event cannot attach
to another provider, another provider-qualified session, a non-invocation
current turn, a closed or recent turn, an earlier part of the trajectory, or an
event shape outside the supported Antigravity tool and permission kinds. Those
cases continue to use the existing duplicate, recent-turn, and ambiguous-turn
protections.

Another `PostInvocation` hook may inject trajectory steps after Coding Brain has
closed the invocation. Coding Brain cannot observe the aggregate output of other
hooks and therefore cannot prove that those steps belong to the closed
invocation. Their permission requests conservatively return `ask` until a new
`PreInvocation` supplies fresh authority.

This logic belongs in lifecycle projection rather than the parser. The
Antigravity permission payload does not carry `invocationNum`, so the parser
cannot safely manufacture the active invocation identifier. Collapsing all
Antigravity events into a synthetic turn would discard real invocation
boundaries.

### Effective activity ordering

The model proposal remains append-only audit evidence in the decision log. For
a model allow, the permission hook then:

1. persists the executable lifecycle decision;
2. appends terminal `Allowed` activity only if lifecycle persistence applied;
3. writes the serialized allow response to Antigravity; and
4. appends delivery evidence.

If lifecycle persistence is rejected or fails, the hook appends terminal
`Error` as the first terminal activity, preserves the diagnostic, returns
Antigravity's fail-safe `ask`, and appends no allow delivery evidence. Live then
reflects the effective provider response rather than the earlier model proposal.

Model and deterministic denies retain their existing fail-safe semantics. A
deny response remains executable even when positive lifecycle confirmation
cannot be recorded, so this change does not turn denials into allows or weaken
provider policy.

If terminal activity persistence fails after lifecycle persistence, the hook
still returns `ask`; it never sends allow without both lifecycle and activity
evidence. It also best-effort records `NeedsInput` to compensate the prepared
`Decided` lifecycle state. Permission disposition participates in lifecycle
duplicate identity so this `Decided` to `NeedsInput` transition is accepted.
This design does not claim an atomic transaction between the lifecycle and
activity stores; compensation may itself fail, but that can never deliver an
allow.

## Persistence compatibility

The lifecycle snapshot schema does not change. New invocation-floor and
child-signature fields use serde defaults, so existing snapshots load without
migration and older binaries ignore the added fields. Invocation open and close
use the existing `UserPromptSubmit` and `Stop` event kinds, and a child that
cannot be correlated uses the existing `AmbiguousTurn` result.

Rolling back therefore restores the former conservative behavior: an older
binary may reject invocation/step differences and return `ask`, but it does not
misread the snapshot or require state deletion.

## Security properties

- No generic ambiguity check is relaxed.
- Child-step correlation requires an exact provider-qualified session and a
  currently open Antigravity invocation.
- A step older than the invocation's initial trajectory length is rejected.
- Invocation-scoped replay state is bounded; capacity fails safe.
- Only adapter-generated `invocation-N` and `step-N` identifiers participate.
- A correlation or persistence failure fails safe to `ask`.
- An undelivered model proposal is not represented as an effective allow.

## Tests

Provider-adapter coverage will verify:

- `PreInvocation` opens `invocation-N` with its `initialNumSteps`; and
- `PostInvocation` closes the same invocation.

Lifecycle projection unit coverage will verify:

- an Antigravity invocation followed by step-scoped tool and permission events
  applies successfully while retaining the invocation as the current turn;
- permission, pre-tool, and post-tool signatures remain distinct;
- replay after intervening events, steps below the trajectory floor, and child
  capacity are rejected;
- invocation closure remains possible at capacity;
- differing sessions, non-invocation turns, unsupported event kinds, and generic
  provider turn mismatches remain ambiguous or otherwise rejected as before; and
- snapshots without the new defaulted fields remain readable.

Permission-hook integration coverage will reproduce an open invocation followed
by a later step permission and verify:

- successful correlation produces Antigravity `allow`, terminal `Allowed`, and
  delivery evidence;
- failed correlation produces Antigravity `ask`, terminal `Error`, no terminal
  `Allowed`, and no delivery evidence; and
- terminal activity failure returns `ask` and compensates lifecycle to
  `NeedsInput`;
- model and deterministic deny behavior remains unchanged; and
- the model proposal and persistence diagnostic remain auditable.

Focused tests will run before the complete workspace build, test, formatting,
and clippy gates.

## Non-goals

- Changing Antigravity's external hook schema.
- Parsing undocumented Antigravity storage to infer invocation membership.
- Relaxing lifecycle ambiguity rules for Codex, Claude Code, or generic events.
- Redesigning activity terminal-state precedence.
- Providing atomic transactions across lifecycle and activity files.

## Stress Test Results: Antigravity approval lifecycle correlation

### Resolved Decisions

- Invocation authority: `PreInvocation` opens and `PostInvocation` closes the
  same invocation; child steps attach only while it is open.
- Step authority: `initialNumSteps` is retained and older `stepIdx` values are
  rejected.
- Replay protection: disposition-aware child evidence is tracked per step;
  only `Decided` to `NeedsInput` compensation may add a second permission bit.
- Cross-store ordering: lifecycle is prepared before terminal `Allowed`;
  activity failure returns `ask` and best-effort compensates to `NeedsInput`.
- Deny behavior: fail-safe model and deterministic denials remain unchanged.
- Scale: compact child state is capped at 256 steps and capacity fails safe.
- Rollback: defaulted fields preserve the current snapshot schema and allow
  older binaries to degrade conservatively.
- Testing: adapter, projection, compatibility, and end-to-end failure matrices
  are required before workspace gates.
- Hook composition: steps injected by another `PostInvocation` hook after the
  invocation closes remain uncorrelated and fail safe to `ask`.

### Changes Made

- Added explicit invocation closure instead of treating both invocation hooks
  as turn-open events.
- Added trajectory-floor validation and bounded replay protection.
- Added lifecycle compensation for the post-lifecycle activity failure window.
- Added persistence compatibility and expanded regression requirements.
- Defined conservative behavior for unseen output from composed invocation
  hooks.

### Deferred / Parking Lot

- Atomic transactions across lifecycle and activity stores remain out of scope.
  The executable response still fails safe if either durable record fails.

### Confidence Assessment

- Overall: High
- Areas of concern: the two stores cannot commit atomically, so the design uses
  conservative response ordering and best-effort lifecycle compensation rather
  than claiming impossible atomicity.
