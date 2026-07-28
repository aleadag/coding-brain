# Interrupted Codex Subagent Follow-up Proof Design

> **Date:** 2026-07-28
> **Issue:** codexctl-700u
> **Status:** Approved design; stress-tested

## Problem

The resumed-subagent proof added for codexctl-4gi0 can reactivate a Codex child
only from `stopped_subagents`. Codex can instead interrupt a child and later
continue it with `followup_task` without emitting `SubagentStop` or another
`SubagentStart`. The lifecycle store then retains a valid parent-to-child edge
for turn A, while the child transcript and permission callback identify fresh
turn B. Re-proof returns `SubagentTurnMismatch`, and final permission
persistence correctly fails closed.

A related sequence starts from a stopped child, completes a permissionless
follow-up turn, and reaches a later permission-bearing follow-up. That path
must keep working from the retained stopped proof even though no permission
request refreshed the edge during the intermediate turn.

## Decision

Generalize `LifecycleStore::reprove_codex_subagent` to refresh an exact prior
Codex child proof from either of two states:

- an active edge for the same child and provider topology whose proven turn
  differs from the requested turn and predates its transcript start; or
- the existing exact stopped-child tombstone path.

An exact active edge for the requested turn remains a duplicate. An active
edge under another provider topology remains a provider-session mismatch.
Never-proven children remain unproven.

The active-edge transition requires the same fixed-file transcript proof as
stopped-child recovery: exact child ID, shared provider session, callback turn,
requested and canonical transcript paths, regular-file identity, and bounded
`task_started` evidence. The new turn must differ from the active turn, and its
start timestamp must be strictly newer than the active edge's last trusted
receive timestamp and no more than five seconds in the future.

Because the active edge's receive timestamp advances with accepted child
lifecycle events, a delayed old-turn event may conservatively make a legitimate
transition ineligible for automation. That availability loss is intentional:
native Codex confirmation remains available, while re-proof never rolls trusted
turn ordering backward.

After validation and while holding the lifecycle-store exclusive lock, remove
the child's old projected session subtree, including descendants, and replace
the parent edge with a new active edge for the proven turn. This prevents the
old open turn's permission keys, status, or descendants from leaking into the
fresh turn. The parent topology itself remains intact. The operation consumes
one lifecycle sequence and does not consume additional active-child capacity
because it replaces an existing edge.

Stopped-child recovery retains its current validation, capacity, sequence, and
tombstone-removal behavior. A permissionless intermediate follow-up does not
need to grant authority: a later exact transcript-proven turn may still refresh
from the retained tombstone under the existing ordering rules.

## Permission Flow

The permission hook continues to perform best-effort re-proof before
inference:

1. Return immediately if the exact active parent-child-turn edge already
   exists.
2. Read the bounded child transcript snapshot.
3. Under the store lock, locate one exact active or stopped prior proof and
   revalidate all evidence and ordering constraints.
4. Atomically replace the stale proof with the fresh active edge.
5. Run inference and require ordinary exact lifecycle permission persistence
   before emitting an allow response.

No parent-side `sub_agent_activity(kind = "interacted")` row is read or treated
as authority. No topology is inferred from cwd, timestamps alone, command
content, or the latest child transcript.

## Failure and Security Properties

- Stale, replayed, unrelated, cross-parent, cross-provider, mismatched-child,
  mismatched-turn, mismatched-path, missing, unreadable, oversized, or
  future-dated evidence makes no state change and emits no allow response.
- Re-proof cannot transfer an active child between parent topologies.
- An older task-start cannot replace a newer active proof.
- Sequence exhaustion and persistence failures remain fail-closed.
- Replacing an active edge does not weaken the active-subagent capacity bound.
- A concurrent stop after re-proof still wins at final permission persistence.
- Existing stopped-child recovery and native Codex fallback behavior remain
  unchanged.

## Tests

Add store and permission-hook regressions for:

1. `SubagentStart(turn A)` → retained active proof → transcript
   `task_started(turn B)` → `PermissionRequest(turn B)`, proving the stale
   active edge and old child projection are replaced and the decision is
   delivered.
2. `SubagentStart(turn A)` → `SubagentStop(turn A)` → permissionless follow-up
   turn B completes → transcript `task_started(turn C)` →
   `PermissionRequest(turn C)`, proving stopped recovery survives the
   intermediate turn.

Cover exact duplicates plus stale/replayed evidence, wrong parent/provider,
wrong child, wrong turn, wrong transcript path, old/future timestamps,
unproven children, sequence exhaustion, nested descendants, and the existing
concurrent-stop ordering. Keep the codexctl-4gi0 stopped-child tests passing.

Run focused transcript, lifecycle store/projection, and permission-hook tests,
then:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo build
```

Use the repository development environment when the bare shell lacks required
tools.

## Documentation

This restores intended provider behavior without adding configuration or a
user-visible workflow. No external documentation change is required.

## Stress Test Results: Interrupted Codex Subagent Follow-up Proof

### Resolved Decisions

- Authority requires both an already trusted active or stopped child edge and
  exact bounded child-transcript evidence; transcripts cannot bootstrap an
  unknown child.
- New active-turn evidence must be strictly newer than the edge's last trusted
  receive time. Delayed evidence fails conservatively.
- Active refresh removes only the child's prior projected subtree before
  inserting the fresh edge, so old permission state and descendants cannot
  cross turns while siblings and parent state remain intact.
- Active refresh preserves the exact owner topology and replaces one edge
  without a growth-specific capacity check. Stopped recovery retains its
  existing capacity check.
- Separate locked re-proof and permission-persistence operations preserve the
  concurrent-stop safety boundary and make duplicate re-proofs converge.
- Parent interaction inference, parent-transcript reconstruction, polling, and
  new lifecycle watchers remain out of scope because they provide weaker or
  broader authority.
- All existing fixed-file, regular-file, bounds, identity, path, timestamp,
  sequence, and future-skew checks remain mandatory.
- Store-level transition tests and end-to-end permission regressions cover both
  interrupted and permissionless-intermediate follow-up trajectories.

### Changes Made

- Made the active-edge timestamp ordering rule and its conservative
  availability tradeoff explicit.
- Confirmed subtree cleanup, no-net-growth capacity behavior, concurrency, and
  test boundaries.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: implementation must remove and reinsert the active edge
  atomically without touching siblings or relaxing stopped-child validation.
