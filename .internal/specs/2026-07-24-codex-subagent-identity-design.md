# Codex Subagent Identity Design

> **Date:** 2026-07-24
> **Issues:** codexctl-e9j2, codexctl-3i0a
> **Status:** Approved design

## Problem

Codex CLI 0.145.0 invokes normal tool hooks inside thread-spawned subagents,
but those callbacks retain the parent `session_id` and carry the exact child
thread identity in `agent_id`. Coding Brain currently discards `agent_id`.
Child tool activity is consequently projected into the parent's single-turn
lifecycle state, which conflates siblings and can reject valid permission
events with `AmbiguousTurn`.

The original report inferred that child hooks were absent because no activity
rows used the child rollout ID. Fresh runtime evidence disproved that
inference: the rows exist under the parent session ID and child turn ID.

## Goals

- Preserve both exact child and parent identities from Codex callbacks.
- Evaluate and persist child permissions under the child identity.
- Isolate concurrent parent and sibling lifecycle state.
- Correlate permission delivery and outcomes to the exact child, turn, and
  tool invocation.
- Bound and clean up transient child state.
- Preserve the native Codex permission boundary whenever child identity cannot
  be validated or proven active.

## Non-Goals

- Inferring child identity from timestamps, commands, turn ordering, or parent
  lifecycle timing.
- Adding terminal-input approval fallback.
- Reconstructing lineage fields Codex does not provide, such as depth or agent
  path.
- Changing Claude or Antigravity adapter semantics without a verified provider
  contract.
- Migrating or removing legacy codexctl paths.

## Identity Model

Extend the provider-neutral `LifecycleIdentity` and persisted `SessionTarget`
with an optional, bounded `provider_session_id`. `session_id` remains the exact
actionable identity; `provider_session_id` groups that identity under a
provider-defined shared session when the provider supplies one.

The core representation is general across providers, but population is
adapter-specific:

- Codex populates the field for child callbacks from its shared `session_id`
  while using `agent_id` as the actionable child identity.
- Every adapter that populates the field must supply an effective `session_id`
  unique across that provider, not merely within one provider session. An
  adapter with parent-local child IDs must derive a bounded, collision-free
  effective ID or leave linkage unsupported.
- Claude retains its current native session identity until its normal
  child-hook relationship is independently verified.
- Antigravity leaves the field absent because its documented contract exposes a
  conversation and invocation steps, not this subagent relationship.

For Codex, the provider session is the root/shared session for a child, not
necessarily its immediate parent thread.

| Callback | Effective `session_id` | `provider_session_id` |
|---|---|---|
| Root callback | Codex `session_id` | None |
| Normal child callback | Codex `agent_id` | Codex `session_id` |
| `SubagentStart` / `SubagentStop` projection | Codex parent `session_id` | None |
| `SubagentStart` / `SubagentStop` audit target | Codex `agent_id` | Codex parent `session_id` |

For Codex callbacks other than `SubagentStart` and `SubagentStop`, a present
`agent_id` selects the child identity. This covers current tool callbacks and
keeps any child `SessionStart`, `UserPromptSubmit`, or `Stop` events internally
consistent. An absent `agent_id` retains current root behavior.

`SubagentStart` and `SubagentStop` remain parent-scoped topology events in the
lifecycle projection. Their `turn_id` belongs to the child and must not replace
or conflict with the parent's current turn.

## Validation and Proof

All provider and effective session identifiers use the existing identifier
size and emptiness limits. A linked callback is actionable only when:

1. `agent_id` is present and valid;
2. `provider_session_id` is present and valid;
3. the provider-session lifecycle state has an active `SubagentStart` entry for
   that exact child and child turn when the adapter declares topology proof
   required; and
4. the child state's stored provider session, if already established, exactly
   matches.

Failure at any step rejects lifecycle persistence. A permission callback may
never emit an allow without this proof and successful activity persistence.
Deterministic or provider-policy denial remains fail-closed and may still emit
a deny because it cannot authorize execution; the failure is recorded as
bounded diagnostic evidence. Ordinary abstention emits no response, leaving
the native Codex prompt authoritative. There is no heuristic or latest-child
fallback.

## Lifecycle Projection

Root and child states continue to use `AgentSessionKey`, but child states are
keyed by `agent_id`. Each child `SessionLifecycleState` stores its
`provider_session_id`.

Parent topology events follow a dedicated path before the generic
single-current-turn guard:

- `SubagentStart` inserts the bounded child ID and exact child turn into the
  parent's active map. It does not set the parent's `current_turn` from the
  child turn.
- `SubagentStop` removes the exact active child and its matching transient child
  state.
- Root/provider-session `Stop` removes every child state whose stored provider
  session matches.
- Parent `SessionStart` clears child states linked to the previous parent
  lifecycle before rebuilding the parent state.

Normal child events use the existing single-turn rules inside their own child
state. Concurrent siblings therefore cannot conflict. Sequential turns inside
one child retain the current ambiguity and replay protections.

Cleanup checks both the child ID and stored provider-session relationship
before removing state. A mismatched provider session cannot delete another
provider session's child state.
`MAX_ACTIVE_SUBAGENTS`, `MAX_SESSIONS`, and serialized snapshot limits continue
to bound memory and disk usage.

## Activity and Outcome Data

`SessionTarget` persists `provider_session_id` on child Decision, Lifecycle,
Diagnostic, Delivery, Outcome, and Correction rows. `SubagentStart` and
`SubagentStop` audit observations use a child-centric target with the parent
reference, while lifecycle projection remains parent-scoped.

All exact correlation predicates include:

- provider;
- effective child or root `session_id`;
- `provider_session_id`;
- turn ID; and
- tool-use ID where the provider supplies it.

The existing bounded Bash fallback may operate only within the same exact
provider, effective session, parent, and turn interval. Parent and sibling
events are never candidates.

Project attribution continues to resolve from the callback `cwd`, independently
for every child callback.

## Schema Compatibility

The activity schema advances from 2 to 3 because
`provider_session_id` changes security-relevant correlation semantics. Existing
supported activity rows remain readable with `provider_session_id = None`; new
rows require readers that understand the new field.

The lifecycle snapshot schema also advances from 2 to 3. Schema-2 snapshots migrate by
setting `provider_session_id = None` on existing states. Schema-1 migration
continues through its existing provider-key projection and then receives the
same default. New snapshot invariants require:

- bounded parent IDs;
- no self-provider-session relationship;
- a child state's provider-session key to identify a state for the same
  provider; and
- parent-linked cleanup to remain within existing state limits.

No state path is renamed, and no legacy codexctl data is modified.

Schema 3 is intentionally fail-closed for older lifecycle writers. To
deliberately roll back, stop all Codex sessions and either restore a current
Coding Brain binary or remove only the transient lifecycle snapshot before
starting the old binary. `activity.jsonl` remains append-only and is never
rewritten or downgraded; older readers may not understand schema-3 evidence.

## Failure Behavior

- Missing `agent_id`: treat as a root callback, matching the provider contract.
- Empty or oversized `agent_id`: reject the hook input.
- Child callback without matching active parent topology: reject lifecycle
  persistence and suppress authorizing decisions. Preserve deterministic and
  provider-policy denials.
- Child callback whose turn does not match the active topology entry: reject it
  as stale or replayed.
- Parent mismatch for an existing child: reject as ambiguous/unproven.
- Unknown, duplicate, stale, or replayed child event: retain existing fail-safe
  ignore behavior.
- Cleanup mismatch: retain state and report the ignored event; never delete by
  child ID alone.

## Tests

### Provider contract

- Root Codex callbacks retain the provider session ID and no parent.
- Child `PermissionRequest`, `PreToolUse`, and `PostToolUse` select `agent_id`
  and preserve the parent session.
- Parent `SubagentStart` followed by normal child callbacks produces exact child
  identity.
- Empty and oversized child IDs are rejected.

### Projection and cleanup

- Two sibling child turn IDs interleave without `AmbiguousTurn`.
- Parent and both siblings retain independent current-turn state.
- A child without matching `SubagentStart` is rejected.
- A delayed callback from an earlier turn is rejected after child-ID reuse.
- `SubagentStop` removes only the matching child state.
- Parent `Stop` and restart remove all linked child state.
- A mismatched parent cannot mutate or clean up a child.
- Duplicate, stale, and replayed events remain rejected.
- Active-child and total-session bounds remain enforced.
- Accepted child activity refreshes its provider topology lease; retention
  removes an expired provider and its linked children atomically.

### Activity and outcomes

- Child Decision, Lifecycle, Delivery, Outcome, Diagnostic, and Correction rows
  retain both identities.
- Exact child outcomes cannot join parent or sibling decisions.
- The bounded Bash fallback cannot cross a parent/child boundary.
- Existing schema rows load with no parent; new schema rows round-trip both
  identities.

### Verification

Run focused provider-hook, lifecycle projection, permission, outcome, and
schema tests first. Then run:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Use the repository's Nix or direnv development environment when bare Cargo
lacks dependencies.

## Documentation

Update troubleshooting or configuration documentation only if user-visible
Doctor/runtime behavior changes. The identity correction itself should not add
new configuration.

## Stress Test Results: Codex Subagent Identity

### Resolved Decisions

- Child identity is actionable only after the exact provider-session
  `SubagentStart` topology entry is persisted.
- Parent-scoped topology events bypass the parent's single-current-turn guard
  because their turn IDs belong to children.
- Stored lineage is named `provider_session_id`; Codex does not expose the
  immediate parent thread on normal child tool callbacks.
- Cleanup is lock-serialized and idempotent. Delayed events after cleanup are
  rejected once topology proof is gone.
- Outcome correlation retains the bounded Bash fallback but confines it to the
  exact child, provider session, and turn interval.
- Activity and lifecycle schemas advance to 3; lifecycle rollback is
  intentionally fail-closed.
- Existing active-child, global-session, retention, and snapshot-size bounds
  remain fixed and non-configurable.
- Persistence or cleanup failure never weakens identity checks or emits an
  unpersisted authorizing decision; fail-closed denials remain available.
- Provider fixtures, projection tests, and end-to-end hook tests jointly prove
  parsing, concurrency, cleanup, and outcome isolation.
- Provider adapters may populate linkage only when their effective session IDs
  are provider-global; parent-local IDs require collision-free derivation.

### Changes Made

- Renamed `parent_session_id` to `provider_session_id` after depth-2 runtime
  evidence showed that Codex retains the root/shared provider session rather
  than exposing immediate ancestry.
- Added an explicit schema-3 rollback contract for transient lifecycle state
  and append-only activity history.

### Deferred / Parking Lot

- Immediate parent-thread lineage, depth, and agent path remain unavailable in
  normal Codex child tool callbacks and are not inferred.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Provider hook schema changes in future Codex releases
  must fail input validation or preserve the same exact identity guarantees;
  they must not trigger heuristic fallback.
