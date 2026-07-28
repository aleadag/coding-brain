# Antigravity PostInvocation Continuation Design

> **Date:** 2026-07-28
> **Issue:** codexctl-dfbn
> **Brainstorming:** codexctl-tkyk
> **Status:** Approved design; awaiting written-spec review

## Problem

Antigravity may resume an execution trajectory after `PostInvocation` when a
background task completes and injects a system message. Coding Brain currently
maps `PostInvocation(invocation-N)` to lifecycle `Stop(invocation-N)`. That
closes the trusted invocation, clears its initial-step floor and bounded replay
ledger, and causes legitimate resumed `step-N` permission and tool events to be
rejected as `AmbiguousTurn`.

The provider contract distinguishes these events:

- `PostInvocation` fires after tool calls finish and may force continuation.
- `Stop` fires when the execution loop terminates.
- `Stop.fullyIdle: true` proves all background and asynchronous work is done.

Therefore `PostInvocation` is not a lifecycle transition.

## Scope

Change Antigravity `PostInvocation` handling so the callback is fully parsed and
validated but causes no lifecycle or activity transition.

Preserve:

- Codex and Claude lifecycle behavior.
- All other Antigravity hook behavior.
- Trusted `PreInvocation` as the only opener of invocation-to-step authority.
- Real Antigravity `Stop(fullyIdle=true)` as the revocation boundary.
- The initial-step floor, per-step replay bits, permission-reversal guard,
  distinct-step capacity, provider/session qualification, and generic
  cross-turn guards.

Do not:

- Reopen authority from an arbitrary `step-N` callback.
- Parse transcript system-message fields as authorization evidence.
- Add polling, timeouts, retries, or a continuation grace period.
- Add a persisted `PostInvocation` or `InvocationComplete` event.
- Change persisted lifecycle schema.

## Design

### Parsed provider callback

Allow `ParsedLifecycleHook` to represent a successfully validated provider
callback with no lifecycle event. Codex, Claude, and all Antigravity events
except `PostInvocation` continue returning an event.

Antigravity `PostInvocation` must still require and validate:

- trusted event selection;
- `conversationId`;
- `workspacePaths`;
- `transcriptPath`;
- `artifactDirectoryPath`;
- `invocationNum`;
- `initialNumSteps`;
- bounded identity and path fields.

After validation it returns no lifecycle event.

### Lifecycle runner

After parsing and live-process attachment, the lifecycle runner checks whether
the callback contains an event. When absent, it returns successfully without:

- constructing a `LifecycleEvent`;
- writing the lifecycle store;
- changing projected status;
- appending lifecycle activity;
- adding a session link.

This is a successful no-op, not an ignored or erroneous lifecycle event, so it
must not emit an `AmbiguousTurn` diagnostic.

### Authority flow

The resulting legitimate sequence is:

1. `PreInvocation(invocation-N, initialNumSteps=F)` opens the invocation and
   establishes step floor `F`.
2. Permission and tool callbacks for `step-S`, where `S >= F`, use the existing
   bounded per-step replay ledger.
3. `PostInvocation(invocation-N)` validates successfully and changes nothing.
4. A background-task system message may cause later `step-T` callbacks without
   another `PreInvocation`; they remain children of the still-open invocation.
5. Trusted `Stop(fullyIdle=true, execution-E)` may cross Antigravity's
   `execution-*`/`invocation-*` namespace boundary only to close the currently
   open invocation and clear its authority.
6. Any later `step-U` callback remains fail-safe and is rejected.

### Terminal correlation

Antigravity identifies model invocations as `invocation-N` but identifies the
real execution-loop terminal callback as `execution-E`. Once `PostInvocation`
stops acting as a premature close, the generic mismatched-turn guard would
otherwise reject real `Stop` as `AmbiguousTurn`.

Add a projection exception limited to:

- provider `Antigravity`;
- event `Stop`;
- a valid `execution-*` event turn;
- an open current `invocation-*` turn.

The exception may only bypass the mismatched-turn rejection and proceed into
the existing `Stop` transition. It must not open a turn, restore cleared
authority, accept child steps, or affect any other provider or event kind.
`Stop` input validation continues requiring `fullyIdle: true`.

## Security and Failure Handling

The fix changes only the premature revocation point. It does not create a new
authorization source.

- A session without trusted `PreInvocation` remains unable to correlate steps.
- Steps below `initialNumSteps` remain rejected.
- Duplicate permission/tool event bits remain rejected.
- `NeedsInput -> Decided` permission reversal remains rejected.
- The distinct-step capacity remains enforced.
- Provider and native session identities remain isolated.
- A different open invocation remains protected by generic turn guards.
- Only trusted Antigravity `Stop(fullyIdle=true, execution-E)` may cross the
  execution/invocation namespace mismatch, and only to revoke the currently
  open invocation.
- Real `Stop` clears authority; post-stop steps cannot regain it.
- Malformed or incomplete `PostInvocation` payloads remain parser errors.

## Tests

Add focused regression coverage for:

1. Parser: valid `PostInvocation` produces no lifecycle event while retaining
   field validation.
2. CLI lifecycle flow:
   `PreInvocation -> child step -> PostInvocation -> later child step -> Stop`.
   Both child steps apply, `PostInvocation` emits no diagnostic or transition,
   and `Stop(execution-E)` closes the open `invocation-N`.
3. Permission flow: a model-approved continued `PreToolUse` after
   `PostInvocation` can persist and deliver without `AmbiguousTurn` or duplicate
   Needs Attention rows.
4. Fail-safe matrix after the change:
   below-floor, replayed, capacity-exceeding, cross-session, unrelated-turn,
   no-`PreInvocation`, non-Stop namespace mismatches, and post-`Stop` steps
   remain rejected.
5. Existing normal open-invocation correlation tests remain passing.

Run the repository gates required by codexctl-dfbn:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo build --workspace
```

Use the project environment wrapper if the bare shell lacks required tools.

## Documentation

No user-facing configuration or behavior documentation is required. If the
implementation changes a public lifecycle representation contrary to this
design, stop and revise the design rather than silently expanding scope.

## Stress Test Results: PostInvocation Continuation

### Resolved Decisions

- Represent a valid no-transition callback as `ParsedLifecycleHook.event:
  Option<LifecycleEventKind>`; do not special-case provider strings in the
  lifecycle runner.
- Keep invocation authority until trusted `Stop` or a later trusted
  `PreInvocation`; do not add a timeout that could reject long background work.
- Preserve strict `PostInvocation` payload validation, but do not compare its
  inert fields against projected state.
- Emit no lifecycle row, activity row, session link, or diagnostic for a valid
  `PostInvocation`.
- Leave projection guard logic unchanged and prove every existing fail-safe
  boundary with regression assertions.
- Do not migrate or reopen previously closed snapshots; only newly received
  callbacks use the corrected semantics.
- Make the executable permission sequence the primary red-to-green proof, with
  parser and lifecycle CLI tests as supporting coverage.
- Ship without a feature flag because the change is provider-specific,
  schema-neutral, and normally revertible.
- Allow trusted Antigravity `Stop(fullyIdle=true, execution-E)` to cross the
  provider's execution/invocation namespace mismatch only to revoke the open
  invocation; all other mismatched-turn events remain fail-safe.

### Changes Made

- Added explicit implementation constraints for callback representation,
  authority lifetime, terminal correlation, observability, existing-state
  handling, and rollout.
- No approved behavior was removed or weakened.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: Antigravity may omit `Stop` after an abnormal crash, leaving
  an invocation open until the next trusted `PreInvocation`. Existing
  provider/session qualification, step floor, replay ledger, and capacity bound
  constrain that state; time-based revocation would incorrectly reject valid
  long-running background tasks. The real terminal uses a distinct
  `execution-*` namespace, so its projection exception must remain strictly
  provider-, event-, direction-, and state-qualified.
