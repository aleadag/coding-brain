# Resumed Codex Subagent Proof Design

> **Date:** 2026-07-27
> **Issue:** codexctl-4gi0
> **Status:** Approved design

## Problem

Codex CLI 0.145.0 emits `SubagentStart` when a child thread is first spawned and
`SubagentStop` when that turn completes. Coding Brain correctly removes the
parent-to-child lifecycle edge at `SubagentStop`.

When `followup_task` resumes the same child thread, Codex emits a structured
parent-side `sub_agent_activity` event with `kind = "interacted"` and starts a
new exact turn in the child transcript, but it does not emit another
`SubagentStart` hook. The resumed child's permission callback therefore carries
an exact child, provider-session, and new turn identity but fails lifecycle
persistence as `UnprovenSubagent`. Coding Brain correctly withholds an allow
response, but a legitimate resumed child cannot receive an automatic decision.

## Goals

- Re-establish exact parent-to-child-and-turn proof for a legitimately resumed
  Codex child.
- Keep stopped children unauthorized until fresh resume evidence is verified.
- Reject stale, replayed, mismatched, missing, unreadable, or oversized
  evidence without weakening the native Codex permission boundary.
- Preserve the existing proposal, lifecycle, terminal activity, and delivery
  persistence order for executable decisions.
- Bound retained stop state and transcript reads.

## Non-Goals

- Treating parent-side `sub_agent_activity(kind = "interacted")` as authority.
- Inferring lineage from timestamps, cwd, agent path, command text, or latest
  child activity.
- Adding a lifecycle watcher, daemon, polling loop, or configuration option.
- Changing Claude or Antigravity topology semantics.
- Reconstructing immediate ancestry beyond the provider-session relationship
  Codex supplies to child hooks.

## Retained Stop Proof

Each Codex provider-session lifecycle state retains a bounded
`stopped_subagents` map alongside `active_subagents`. An entry contains:

- the exact child ID;
- the exact stopped child turn;
- the stop sequence; and
- the stop receive timestamp.

An accepted Codex `SubagentStop` moves the matching child from the active map to
the stopped map while removing its transient child session state exactly as it
does today. This tombstone records prior trusted topology but grants no
authority: all ordinary linked child events continue to require an active edge.

The stopped map uses the existing `MAX_ACTIVE_SUBAGENTS` bound independently
from the active map. Inserting a new tombstone for the same child replaces its
older tombstone. At capacity, the oldest stop sequence is evicted; losing a
tombstone only makes a later resume unprovable. Tombstones expire after the
existing 24-hour lifecycle retention window. A real `SubagentStart` for a child
removes any matching tombstone. Root/provider `Stop`, non-compact
`SessionStart`, retention cleanup, and linked subtree removal clear tombstones
with the corresponding exact provider-session topology, including nested
descendants.

The field uses a Serde default in lifecycle schema 3. Older snapshots load with
no tombstones and conservatively reject resume re-proof. Older writers may
discard the field and likewise revert to conservative rejection, so no schema
version change is required.

## Transcript Resume Evidence

Re-proof uses only the child transcript path supplied by the permission hook.
The parser verifies two independent records:

1. The first complete `session_meta` row has `id` equal to the actionable child
   ID and its provider-level `session_id` equal to the callback's
   `provider_session_id`. For nested children, `parent_thread_id` names the
   immediate parent and is not equal to the root/shared provider session
   exposed by normal child callbacks, so it is retained only as bounded
   informational metadata and never used to infer actionable ancestry.
2. The newest complete `event_msg` row with `type = "task_started"` has a
   `turn_id` equal to the callback's exact turn and a valid outer timestamp.

The task-start timestamp must be strictly newer than the matching tombstone's
stop receive timestamp, no more than five seconds in the future under the
existing lifecycle clock-skew policy, and the started turn must differ from the
stopped turn. Evidence retains both the lexically normalized path requested by
the callback and the canonical regular file opened by the reader. The requested
path must match the lifecycle identity; canonicalization establishes the fixed
file snapshot without making legitimate symlinked callback paths compare
unequal. No parent transcript row, cwd, agent path, or timing-only relationship
can satisfy proof.

The proof reader opens one regular-file descriptor and captures its initial
length. All reads use that descriptor and remain within the captured length, so
a symlink swap or later append cannot splice records into the proof. Reads are
fixed and bounded: at most 1 MiB is read from the head to obtain the first
complete metadata row and at most 8 MiB from the tail to find the newest
complete task-start row. A partial leading tail row is discarded. A missing
newline, oversized required row, invalid JSON, absent timestamp, or required
record outside those bounds yields no proof. A resumed turn that emits more
than 8 MiB before its first permission therefore falls back to native Codex
confirmation rather than making hook latency proportional to an arbitrarily
large transcript.

The existing Codex transcript model gains distinct optional provider-session
and `parent_thread_id` metadata fields plus exact `task_started` turn identity,
so the proof parser and normal transcript consumers share one schema
interpretation. Other event handling remains unchanged.

## Permission Flow

For a linked Codex permission callback, the permission hook makes a best-effort
re-proof attempt before inference:

1. If the lifecycle store already contains the exact active parent, child, and
   turn edge, no transcript read or state change occurs.
2. Otherwise, the hook obtains bounded child transcript evidence.
3. Under the lifecycle-store exclusive lock, re-check the active edge and then
   require the exact matching tombstone, parent, child, turn, and ordering.
4. Atomically remove the tombstone and insert a fresh active edge for the
   resumed turn.

If the re-proof attempt fails, the hook continues through the existing path.
An authorizing proposal still cannot be delivered because final permission
persistence again requires the active exact edge and records the existing
bounded error evidence. Deterministic denials retain their current fail-closed
behavior. The eventual diagnostic may include a bounded reason category such
as missing metadata, stale turn, future timestamp, or bounds exceeded, but it
must not persist transcript content or raw paths.

If a concurrent `SubagentStop` removes the refreshed edge between re-proof and
permission persistence, final persistence returns `UnprovenSubagent` and no
allow response is emitted. Re-proof therefore adds no bypass around the
existing executable permission gate.

## Failure and Security Properties

- A stopped child using its old turn remains `UnprovenSubagent`.
- A never-proven child has no tombstone and cannot bootstrap authority from a
  transcript.
- A child transcript naming another parent or child cannot establish an edge.
- A task start for another turn cannot establish an edge.
- A task start at or before the accepted stop cannot establish an edge.
- A task start over five seconds in the future cannot establish an edge.
- A parent `interacted` row alone cannot establish an edge.
- Opening one fixed regular-file snapshot prevents cross-file evidence
  splicing.
- Transcript I/O, parsing, bounds, store locking, capacity, invariant, or
  persistence failure cannot emit an allow response.
- A real concurrent stop wins at final permission persistence.
- Tombstone eviction, upgrade from older state, and rollback to an older writer
  fail conservatively by disabling resume re-proof.

## Tests

### Transcript parsing

- Parse `parent_thread_id` from child `session_meta`.
- Parse the exact turn and timestamp from `task_started`.
- Select the newest complete task start from the bounded tail.
- Reject mismatched IDs, missing timestamps, partial rows, invalid JSON, and
  required evidence beyond the head or tail bounds.

### Lifecycle projection and store

- `SubagentStart -> SubagentStop` removes active authority and retains an exact
  bounded tombstone.
- A matching newer resume proof replaces the tombstone with an active edge for
  the new turn.
- The stopped turn, mismatched parent, child, turn, transcript path, and stale
  timestamp all remain rejected.
- A never-proven child remains rejected.
- A real new `SubagentStart`, parent stop/restart, retention, subtree cleanup,
  capacity eviction, and schema-3 defaulting maintain the specified tombstone
  behavior.
- A stop racing after re-proof causes final permission persistence to reject.
- Two concurrent matching re-proofs converge on one active edge.

### Permission-hook regression

Cover the full sequence:

1. accepted `SubagentStart`;
2. accepted `SubagentStop`;
3. bounded child transcript metadata plus a newer exact resumed
   `task_started`;
4. child `PermissionRequest`; and
5. an approved decision persisted and delivered.

Repeat with no fresh resume row and with mismatched parent, child, turn, path,
stale timestamp, and future timestamp. Each case must emit no allow response
and retain `UnprovenSubagent` error evidence. Exercise the two lock-visible
concurrency orderings deterministically: two matching re-proofs converge, while
re-proof followed by stop causes final permission persistence to reject. Avoid
timing-sensitive thread races.

## Verification

Run focused transcript, lifecycle projection/store, Codex adapter, lifecycle
hook, and permission-hook tests first. Then run:

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
user-visible workflow. No external documentation changes are required unless
implementation changes diagnostics beyond the existing
`UnprovenSubagent` failure.

## Stress Test Results: Resumed Codex Subagent Proof

### Resolved Decisions

- Authority requires both a trusted stopped-child tombstone and exact bounded
  child-transcript evidence; transcript rows can refresh but never bootstrap
  child authority.
- Child transcript `session_id` proves the root/shared provider session;
  `parent_thread_id` is immediate-parent metadata and never substitutes for the
  provider identity available on normal child callbacks.
- One regular-file descriptor and a captured initial length prevent symlink or
  append races from combining proof records.
- Requested and canonical transcript paths remain distinct: the former binds
  evidence to the callback identity, while the latter identifies the opened
  regular file.
- The 1 MiB head and 8 MiB tail limits intentionally trade rare long-turn
  automatic approval for bounded permission-hook latency and fail-safe native
  confirmation.
- Re-proof and final permission persistence remain separate lock-serialized
  operations; final persistence safely catches a concurrent stop.
- Tombstones are exact to one provider-session topology, expire after 24 hours,
  and use an independent 64-entry bound with oldest-stop fail-safe eviction.
- Lifecycle schema 3 remains compatible because older readers can only discard
  tombstones and disable resume automation.
- Re-proof failures retain deterministic denials and existing final allow
  suppression while exposing only bounded diagnostic categories.
- Deterministic store interleavings cover concurrency without flaky timing
  tests.
- Task-start evidence uses the existing five-second future-skew limit and
  cannot be replayed after another stop.

### Changes Made

- Added fixed-file snapshot semantics and regular-file validation.
- Distinguished normalized callback paths from canonical opened-file paths.
- Documented the intentional 8 MiB availability bound.
- Made tombstone retention, capacity, nesting, compatibility, diagnostics, and
  future-clock behavior explicit.
- Corrected nested-child proof to compare the callback provider session with
  transcript `session_id`, not immediate `parent_thread_id`.
- Replaced timing-sensitive race testing with deterministic lock-visible
  orderings.

### Deferred / Parking Lot

- No watcher or transcript index is added for resumed turns whose task-start
  row falls outside the fixed tail bound.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Future Codex transcript schema changes must fail proof
  parsing closed; they must not trigger a heuristic lineage fallback.
