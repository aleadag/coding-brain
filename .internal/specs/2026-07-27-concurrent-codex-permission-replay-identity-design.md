# Concurrent Codex Permission Replay Identity Design

> **Date:** 2026-07-27
> **Issue:** codexctl-lxxe
> **Status:** Approved design

## Problem

Codex permission callbacks do not currently provide a `tool_use_id`. Two
permission decisions for different commands in the same provider session and
turn therefore produce the same lifecycle signature:
`PermissionRequest(Decided)`. The lifecycle projection accepts the first and
rejects the second as `Duplicate`, so the permission hook correctly withholds
the second automatic allow response and records an Error activity.

The replay guard cannot simply use a new per-process activity ID. That would
distinguish concurrent callbacks, but it would also make every genuine retry or
duplicate callback look new and eligible for another automatic authorization.

## Goals

- Persist independent permission decisions for different Codex requests in one
  session turn, including callbacks executing concurrently.
- Retain exact replay rejection after other permission events have intervened.
- Never correlate or authorize one command from another command's evidence.
- Preserve the existing requirement that executable lifecycle state and
  terminal activity persist before an allow response is emitted.
- Bound all new per-turn replay state and fail closed at capacity.
- Preserve existing sequential Codex, concurrent subagent, Claude, and
  Antigravity behavior.

## Non-Goals

- Automatically authorizing two byte-identical Codex callbacks in the same
  turn. Without a provider request ID, Coding Brain cannot distinguish those
  from a genuine replay; they remain fail-safe duplicates.
- Inferring request identity from callback timing, process ID, activity ID,
  transcript ordering, or mutable activity-store state.
- Changing outcome correlation or addressing the separate unpaired
  `PreToolUse`/`PostToolUse` issue tracked by codexctl-0dis.
- Changing provider hook configuration or legacy state paths.

## Request Identity

Each parsed permission request carries a bounded opaque request key derived
consistently across Codex, Claude, and Antigravity.

Every adapter derives the key from a domain-separated encoding of:

- provider;
- valid `tool_use_id`, when supplied;
- exact tool name; and
- exact parsed `tool_input`.

When a provider omits `tool_use_id`, the exact tool name and input remain the
fallback identity. A supplied ID is an additional discriminator rather than a
replacement for the input scope: distinct IDs can safely separate
byte-identical requests, while a buggy reused ID cannot collapse different
commands into one key.

The encoded scope uses explicit length prefixes and deterministic
`serde_json::Value` serialization, then is hashed with SHA-256 before it enters
lifecycle state. Raw JSON bytes are not used because insignificant whitespace
or object-key ordering must not manufacture a new request identity. The raw
command or tool input is never persisted in the lifecycle snapshot. Domain
separation prevents a provider ID from colliding semantically with a fallback
input fingerprint. Existing input size limits bound hashing work; the resulting
key uses the existing lifecycle identifier bounds.

The fallback deliberately preserves exact payload distinctions rather than
lossy normalized or redacted command text. Different commands therefore cannot
share authority because their keys differ. Byte-identical payloads share a key
and are treated as replays.

## Lifecycle Projection

`PermissionRequest` lifecycle events carry the optional request key in addition
to their disposition. Current provider permission hooks always supply a key;
optional representation preserves compatibility for older serialized
signatures and internal fixtures that model legacy events.

Each `SessionLifecycleState` stores a bounded map of permission request keys to
the dispositions already accepted for the active turn. The map is consulted in
addition to the generic last-event signature:

- a new key with `Decided` or `NeedsInput` is accepted;
- an already-recorded key and disposition is `Duplicate`, even if another
  permission event intervened;
- `Decided` followed by `NeedsInput` remains allowed for the existing
  fail-safe compensation path;
- `NeedsInput` followed by `Decided` is rejected, so a repeated callback cannot
  escalate a request that Coding Brain previously left to native confirmation;
  and
- a sixty-fifth distinct key is rejected fail-closed without
  consuming a lifecycle sequence.

The map is capped at 64 distinct keys. It is cleared whenever existing
lifecycle rules replace or close the active turn, clear transient session
state, restart the session, or remove a linked child. `SessionStart(Compact)`
preserves the map because compact continuity retains the same active authority.
Root and child sessions keep separate maps through the existing exact Codex
topology.

The generic last-signature guard remains unchanged for all other lifecycle
events. Codex and Claude use the generic keyed per-turn replay map.
Antigravity carries the same composite key, but its invocation-step replay map
remains additionally authoritative because its provider-specific parent/child
and outcome-correlation semantics require a reused step ID to fail closed even
when payloads differ. Antigravity child permissions therefore do not acquire a
weaker 64-key path around the existing step guard.

## Permission Hook Flow

Each provider adapter computes the request key while parsing trusted bounded
input. The permission hook passes it to every lifecycle permission record,
including `Decided`, `NeedsInput`, and compensation. Codex and Claude use the
generic keyed replay guard. Antigravity also carries the key in its lifecycle
signature while retaining the stricter invocation-step replay map.

The executable allow path remains:

1. persist proposal/audit evidence;
2. persist `PermissionRequest(Decided)` with the exact request key;
3. append terminal `Allowed` activity;
4. write the provider allow response; and
5. append `Delivered` or `DeliveryFailed` activity.

If lifecycle persistence rejects a duplicate, reaches capacity, or fails, the
hook emits no allow response and records Error activity. If terminal activity
persistence fails after lifecycle persistence, the hook emits no allow and
best-effort records `NeedsInput` for the same request key. No new fallback
crosses command, session, turn, child, or provider boundaries.

## Persistence Compatibility

The new event-key and per-turn replay fields use Serde defaults. Existing
lifecycle snapshots load with an empty map and an absent key on the prior last
signature. New binaries conservatively begin tracking keyed permission events
from the first callback after upgrade.

Older binaries ignore the added snapshot fields and revert to the previous
more-conservative behavior, where a later same-turn decision may be rejected.
They do not gain authorization capability. Reusing `AmbiguousTurn` for capacity
exhaustion avoids adding a persisted ignore-reason variant that an older binary
could not deserialize. The lifecycle schema version and state paths therefore
remain unchanged.

## Failure and Security Properties

- Missing or invalid required permission input is rejected before inference.
- Different exact request scopes have independent replay entries where the
  provider's topology proof permits them.
- Identical request scopes cannot receive repeated automatic authorization in
  one turn.
- Replay detection is not limited to adjacent lifecycle events.
- A prior `NeedsInput` state cannot become `Decided` for the same request key.
- Capacity exhaustion rejects new permission authority and consumes no
  sequence.
- Lifecycle or activity persistence failure emits no allow response.
- Request keys reveal no raw command or tool input in lifecycle state.
- SHA-256 is an identifier, not encryption: the snapshot reveals equality and
  may permit offline guessing of predictable commands.
- Existing exact provider, session, child, and turn proof remains mandatory.

## Tests

### Adapter and input coverage

- Codex derives stable equal keys for byte-identical permission payloads.
- Different commands, tools, providers, or supplied tool IDs derive different
  keys.
- Reusing a supplied tool ID with different input does not collapse the keys.
- Raw command text is absent from the derived key and serialized lifecycle
  snapshot.
- Existing empty, oversized, and malformed input rejection remains covered.

### Projection coverage

- Two different keyed `Decided` events in one turn both apply.
- Replaying the first key after the second remains `Duplicate` and snapshot
  neutral.
- `Decided` to `NeedsInput` compensation applies for one key.
- `NeedsInput` to `Decided` is rejected for one key.
- The sixty-fifth distinct permission key is rejected as `AmbiguousTurn`
  without consuming a sequence.
- Prompt supersession, Stop, restart, compact continuity, and linked-child
  cleanup retain or clear the map according to existing turn semantics.
- Legacy unkeyed events retain existing adjacent replay behavior.

### Concurrent permission-hook regression

Run two permission hooks concurrently against the same lifecycle and activity
stores with the same Codex session and turn but different commands. Use a
timeout-bounded channel rendezvous inside both inference closures so both
requests are in flight before either reaches lifecycle persistence, while an
early callback failure cannot hang the test indefinitely. Verify:

- both lifecycle decisions apply and consume independent sequences;
- both activity IDs independently reach `Allowed` and `Delivered`;
- both provider responses are `allow`;
- neither activity reaches Error; and
- each activity retains its own exact command evidence.

Then replay one identical payload and verify lifecycle returns `Duplicate`, no
second allow response is emitted, and the unpersisted decision is represented
as Error rather than Allowed or Delivered.

Retain focused coverage for sequential Codex requests, interleaved Codex
children, keyed Claude requests, Antigravity composite keys plus invocation
steps, deterministic denies, and persistence failure.

## Verification

Run focused provider-hook, lifecycle projection, and permission-hook tests
first. Then run:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo build
```

Use the repository development environment when the bare shell lacks required
tools.

## Documentation

No user-facing configuration changes. Update external documentation only if
implementation changes visible diagnostics or provider requirements beyond the
behavior specified here.

## Stress Test Results: Concurrent Codex Permission Replay Identity

### Resolved Decisions

- Canonicalization: hash a versioned, length-prefixed provider/tool/input
  encoding rather than raw JSON bytes.
- Replay transitions: allow only `Decided` to `NeedsInput` compensation; reject
  repeats and `NeedsInput` to `Decided` escalation.
- Cleanup: scope keys to the exact active session turn, retain them across
  compact continuity, and clear them with existing turn/session cleanup.
- Capacity and rollback: cap each turn at 64 keys and reuse `AmbiguousTurn` so
  older binaries remain able to deserialize the snapshot.
- Concurrency: rely on the lifecycle and activity stores' cross-process locking
  rather than adding ineffective process-local coordination.
- Privacy: persist only SHA-256 identifiers while explicitly treating them as
  guessable identifiers, not encryption.
- Provider scope: derive composite keyed replay identity for Codex, Claude, and
  Antigravity while retaining Antigravity's stricter invocation-step guard.
- Test proof: combine deterministic projection tests with a timeout-bounded
  concurrent hook integration test and an exact replay assertion.
- Provider-ID collision: treat a provider tool ID as an additional discriminator
  alongside exact tool name and input, never as a replacement for command
  scope.

### Changes Made

- Fixed the per-turn capacity at 64.
- Defined shared request-key generation across all three providers without
  weakening Antigravity's provider-specific replay proof.
- Defined deterministic structured encoding and SHA-256 privacy limitations.
- Made compact retention, rollback behavior, and real concurrency proof
  explicit.

### Deferred / Parking Lot

- Automatically authorizing byte-identical concurrent Codex requests remains
  impossible without a provider-supplied unique request ID.

### Confidence Assessment

- Overall: High
- Areas of concern: Codex still cannot distinguish byte-identical concurrent
  callbacks from exact replays, so those requests intentionally remain under
  native manual authorization.
