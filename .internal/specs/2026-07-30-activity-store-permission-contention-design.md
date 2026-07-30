# Activity-store permission contention design

## Context

All Coding Brain projects and providers share
`$XDG_STATE_HOME/coding-brain/activity.jsonl`. `ActivityStore` protects the
file with a separate advisory lock and currently gives every blocking lock
acquisition 100 ms.

`codexctl-rcdi` made the initial permission lifecycle atomic by appending
`Observed` and `Evaluating` in one exclusive acquisition. Production evidence
after that change still shows ordinary permission requests failing closed with
`initial activity persistence failed: activity store lock timed out`. The
reported store was about 23.4 MB with 32,448 rows. The permission request
timed out after 101 ms, while an unrelated lifecycle write was recorded two
milliseconds before the request began.

The evidence proves that initial permission persistence exhausted its lock
bound. It does not identify whether the holder was the adjacent writer, a
refresh reader, or another short-lived process. The implementation must
therefore address both known low-concurrency sources without pretending the
runtime evidence identifies one holder conclusively.

## Goals

- Let an ordinary permission request survive transient contention from one
  cross-project refresh reader or lifecycle writer.
- Preserve the atomic initial `Observed` and `Evaluating` rows.
- Preserve durable audit-before-allow, including `flush` and `sync_data`.
- Keep genuine or continuous lock unavailability bounded and fail closed with
  the concrete `ActivityStoreError`.
- Preserve coherent TUI refreshes, stale/busy handling, tail repair, and
  compaction safety.

## Non-goals

- No daemon, in-process writer coordinator, or separate permission ledger.
- No removal of advisory locking, `sync_data`, repair, or compaction.
- No change to the default lock bound for lifecycle hooks, TUI reads,
  compaction, recovery, or other activity-store operations.
- No attempt to infer which process held the lock in the historical incident.

## Design

### Parse public reads after releasing the lock

`ActivityStore::read` will:

1. acquire the shared lock using the existing 100 ms default;
2. copy the complete activity file into an in-memory byte buffer;
3. release the shared lock; and
4. parse and validate the captured bytes into `ActivityLog`.

The byte copy remains protected, so every read observes one coherent file
snapshot. Parsing the captured snapshot cannot block writers.

The existing parser will be separated from file capture so it can consume a
byte slice or owned buffer without changing schema validation, malformed-row
diagnostics, legacy lifecycle-kind handling, duplicate-terminal detection, or
event order.

Operations that read and then mutate under one exclusive transaction retain
their current locking semantics. In particular, compaction and
snapshot-dependent append/reservation paths must not release their exclusive
lock between reading state and writing the derived result.

### Give only initial permission persistence a larger bound

`ActivityStore` will expose a narrowly scoped way for the permission hook to
perform its atomic initial batch with a 500 ms lock-acquisition bound. The
ordinary `ActivityLimits::default().lock_timeout_ms` remains 100 ms.

The 500 ms bound applies only while acquiring the exclusive lock for the
initial `Observed` plus `Evaluating` batch. Once acquired, the existing append,
tail-repair, flush, and `sync_data` sequence is unchanged. No allow response is
emitted until that durable append succeeds.

The managed permission hook has a 30-second provider timeout, so a worst-case
500 ms persistence wait consumes a small, explicit part of its latency budget.
Lifecycle hooks retain their two-second provider timeout and their existing
100 ms activity-store lock behavior.

This is one bounded acquisition, not an unbounded retry loop. If the lock
remains unavailable for 500 ms, the hook retains the exact
`ActivityStoreError`, skips model inference, records `NeedsInput` where
possible, emits no executable allow decision, and leaves the provider's native
confirmation path authoritative.

### Permission data flow

The permission path remains:

1. parse and validate the provider request;
2. build one activity context;
3. acquire the activity lock within the permission-specific bound;
4. durably append `Observed` and `Evaluating` atomically;
5. perform deterministic safety and provider-policy evaluation;
6. perform model inference only when applicable;
7. durably append the terminal decision before emitting it; and
8. append delivery evidence after the response write.

Only step 3 receives the permission-specific bound. Later activity appends keep
their existing behavior; this change does not weaken proposal, terminal, or
delivery ordering.

## Error and safety behavior

- A malformed, unsupported, oversized, or unwritable initial batch fails
  closed with its existing concrete error.
- A continuously held lock fails closed after the 500 ms initial-persistence
  bound and does not invoke the model.
- A public read that cannot acquire its shared lock within 100 ms still returns
  `LockTimeout`, preserving TUI busy/stale behavior.
- A read whose byte capture succeeds completes parsing outside the lock and
  returns the coherent captured snapshot even if a writer appends afterward.
- Tail repair remains writer-only and protected by the exclusive lock.
- Compaction continues to parse, select, and durably replace while holding its
  exclusive lock so no successful append can be lost.

## Testing

Tests will use synchronization channels or barriers rather than scheduler
timing to establish holder and waiter states.

1. Build a realistically sized activity log comparable to the reported
   production row count and byte size.
2. Prove a public reader releases its shared lock after byte capture and before
   parsing by pausing parsing through test-only synchronization, then
   successfully appending while parsing remains paused.
3. Run the permission path against the large log while one transient shared
   reader overlaps it; assert normal inference and the complete
   `Observed -> Evaluating -> terminal -> Delivered` lifecycle.
4. Hold the activity lock exclusively beyond 100 ms but below 500 ms to model
   one transient lifecycle writer; assert the permission path reaches its
   normal decision with a complete lifecycle.
5. Hold the lock continuously past 500 ms; assert bounded completion, no model
   call, native-confirmation/`NeedsInput` behavior, and the exact
   `activity store lock timed out` cause.
6. Keep the `codexctl-rcdi` parallel-burst and atomic-initial-row regressions
   passing.
7. Run focused activity-store, permission-hook, runtime/TUI contention, repair,
   and compaction tests, followed by workspace test, Clippy, formatting, and
   build gates.

## Expected files

- `src/brain/activity.rs`: split coherent capture from parsing and add the
  narrow permission-bound append entry point.
- `src/brain/permission_hook.rs`: use the permission-specific bound for only
  the initial atomic activity batch and add end-to-end regressions.

No user-facing configuration or documentation changes are expected.

## Rollback

The change is local to activity read locking and the initial permission append
call. Reverting those call-path changes restores the prior 100 ms behavior
without a data migration or state-format change.
