# Guarded Live Continue Reliability Design

## Context

Live currently opens the same Allow, Deny, Continue, and manual-text menu for
any selected activity with recognized session provenance. Semantic terminal
actions are actually valid only when the runtime can rediscover one exact live
provider session and match the corresponding complete provider prompt.
Continue therefore appears available even when the selected session is active,
stale, ambiguous, missing, or showing an unrelated prompt.

Dispatch correctly fails closed by resolving a unique target, capturing a
bounded pane, matching a versioned provider prompt, recapturing the same
evidence, sending fixed semantic input, and confirming that the prompt
advanced. The mismatch is in the earlier affordance and in failure
observability: manual action failures are returned only as transient footer
status, while automatic recovery does not retain every abstained, rejected, or
uncertain result.

## Goals

- Show semantic Live actions only when a bounded runtime preflight recognizes
  the action on the exact current provider session.
- Preserve every existing dispatch-time identity, prompt, race, and postflight
  guard.
- Keep successful manual and automatic Codex Continue delivery working through
  the existing guarded terminal path.
- Persist safe, typed failure, abstention, and delivery-unknown diagnostics for
  manual session actions and automatic recovery.
- Keep the `x action` entry point discoverable in narrow and wide Live layouts.

## Non-goals

- Do not add raw or unguarded terminal input.
- Do not treat preflight evidence as dispatch authority.
- Do not persist captured terminal content or manual text.
- Do not restore a session dashboard, orchestration, routing, or launching.
- Do not change provider prompt patterns unless a failing regression proves an
  existing supported prompt is parsed incorrectly.

## Design

### Typed action preflight

Extend the Brain action boundary with an asynchronous session-action preflight.
The request contains the selected `SessionTarget`; the runtime then performs
the same exact-session rediscovery used by dispatch. It rejects unknown
provenance, no exact live session, and multiple exact matches as distinct safe
categories.

For one exact live session, preflight resolves the exact pane, performs one
bounded guarded capture, and classifies the currently recognized semantic
prompt:

- a permission prompt exposes Allow and Deny;
- a recovery prompt exposes Continue;
- no recognized complete semantic prompt exposes no semantic shortcut and
  supplies a fixed explanation;
- manual text remains available only after exact session authority, pane
  resolution, and bounded capture have succeeded.

Preflight returns typed availability and fixed safe display text. It never
returns pane contents, prompt fragments, raw backend errors, or manual text.

### Live interaction

Pressing `x` captures the selected target, generates a unique opaque manual
`action_attempt_id`, and starts preflight on a worker thread. Live remains
responsive and shows `Checking available actions…`. Only one preflight or
delivery may be in flight at a time.

When preflight completes, Live opens a context-specific menu:

- permission: `[a] allow  [d] deny  [t] manual text`;
- recovery: `[c] continue  [t] manual text`;
- no semantic prompt: `[t] manual text` plus a fixed explanation that Continue
  requires a recognized complete recovery prompt.

If exact session authority is unavailable, no menu opens. The footer shows the
safe category and the persisted diagnostic remains available after refresh.
Escape cancels an open menu without dispatch. Existing hidden and bounded
manual-text input remains unchanged.

`BrainInput` retains the preflight capability set and the
`action_attempt_id`. Rendering and key handling consult the same capability
set, so an unavailable semantic key cannot dispatch a hidden action. Menu
wording describes actions recognized during preflight rather than promising
that a later dispatch will succeed.

The Live footer and help text continue to advertise `x action` at narrow and
wide widths. Preflight wording must remain understandable when the footer is
clipped; durable details remain discoverable in Diagnostics.

### Dispatch remains authoritative

Preflight is advisory and its evidence is not carried forward as permission to
send. Dispatch independently repeats:

1. provider-qualified exact-session discovery;
2. exact live process validation;
3. unique terminal-pane resolution;
4. bounded initial capture;
5. complete versioned provider-prompt matching;
6. target resolution and prompt recapture equality;
7. fixed semantic input;
8. bounded post-send capture and prompt-advancement confirmation.

Any mismatch cancels the action. There is no fallback from a semantic action to
manual text and no fallback from an exact target to cwd, terminal focus, or
another process.

### Safe outcome classification

Use typed target-resolution and guarded-action categories at their source
rather than parsing error strings. Keep delivery certainty separate from the
category: `NotSent` applies only before any backend send call; any send-call
error or failed post-send check is `DeliveryUnknown`. The categories cover:

- exact session unavailable;
- exact session ambiguous;
- exact terminal target unavailable or ambiguous;
- recognized semantic prompt absent or incomplete;
- prompt or target changed before send;
- guarded send failure or uncertainty;
- delivery unknown because post-send capture or advancement confirmation
  failed;
- automatic model abstention or confidence below threshold;
- automatic evidence changed before delivery;
- automatic reservation duplicate or cooldown;
- automatic recovery postflight uncertainty.

Runtime maps exact-session categories, terminal core maps target, prompt, and
send categories, and recovery maps model, evidence, and reservation categories.
Display and persistence use fixed category descriptions. Lower-level error
strings remain bounded for transient local status where already permitted, but
pane content, prompt fragments, and manual text never enter activity records.

### Durable diagnostics

Persist every failed, abstained, or delivery-unknown manual action and automatic
recovery result as an `ActivityKind::Diagnostic` event in `activity.jsonl`.
Diagnostics use:

- provider-qualified session identity when available;
- the selected project evidence;
- the manual `action_attempt_id`, or a stable automatic recovery attempt and
  category identity;
- a fixed tool name identifying manual session action or recovery;
- a fixed rule/category identifier;
- a redacted fixed reasoning string.

Each operator invocation keeps its unique manual identity even when two
attempts target the same session. Stable automatic attempt/category activity
IDs collapse repeated observations into one visible Diagnostics row while
retaining append-only evidence. Diagnostic rows remain excluded from Live
attention and learning/scorecard inputs.

Successful automatic recovery retains the existing evaluating/delivered
decision audit. Failed automatic delivery retains its existing decision
transition and also emits the classified diagnostic needed to distinguish
send failure from delivery uncertainty. Successful manual delivery continues
to report through the Live footer; this issue does not add manual action
results to learning.

Activity persistence failure must not authorize or retry terminal input.
Automatic recovery retains its required pre-send evaluating audit and fails
closed if that audit cannot be persisted. If input may already have been sent,
a later persistence failure is reported as delivery unknown and must not
trigger a second send.

Manual actions do not gain a new pre-send authority record. After a manual
failure or uncertain send, append its diagnostic once. If that append fails,
the bounded footer reports both the action category and audit failure, without
retrying terminal input.

### Automatic recovery

Recovery candidate discovery, adaptive confidence threshold, ten-second
reservation/cooldown, cross-process deduplication, stable-evidence checks, and
postflight checks remain mandatory. The recovery evaluator returns a typed
execution result instead of a single undifferentiated `Abstained` value, so
each bounded exit can be audited without changing its authority.

An eligible Codex recovery prompt continues through
`execute_guarded_action_classified` exactly once after reservation. Model
leave-alone or below-threshold decisions, changed evidence, duplicate
reservation, send failure, and post-send uncertainty emit safe diagnostic
categories. Repeated polling must not create duplicate visible rows for the
same attempt and category.

Reuse `ActivityStore::append_if_absent` as the cross-process atomic guard for
automatic diagnostic IDs. `RecoveryCoordinator` also keeps a bounded in-memory
set of attempt/category diagnostics already reported by that process, avoiding
an activity-log scan on every unchanged poll. This does not consume or alter
the delivery reservation. Preserve the existing 100 ms production activity
lock bound, 32 MiB compaction threshold, and polling cadence.

If implementation proves that eligible manual guarded Continue succeeds while
automatic candidate discovery or evaluation independently fails, that separate
RecoveryCoordinator defect will be filed as follow-up work rather than hidden
inside a terminal-delivery change.

## Error and privacy handling

- Captures stay bounded and in memory.
- Prompt fingerprints may identify stable evidence but never reveal prompt
  text.
- Manual text stays hidden in the UI and absent from errors and persistence.
- Unknown, incomplete, stale, changed, ambiguous, and postflight-uncertain
  states fail closed.
- Routine refresh may replace footer status only after the classified result is
  durably inspectable in Diagnostics.
- A diagnostic append failure is surfaced as a bounded status and never causes
  an unguarded retry.

## Testing

### Core terminal coverage

- A complete idle Codex recovery composer exposes Continue and successfully
  sends literal `continue` plus Enter.
- Active, absent, incomplete, and stale recovery prompts do not expose or send
  Continue.
- Changed target or prompt recapture sends nothing.
- Send failure is classified separately from post-send capture failure and
  unchanged-prompt delivery uncertainty.
- Captured terminal content never appears in returned safe categories.

### Runtime coverage

- Preflight resolves one exact structured or recognized process target and
  returns the matching action set.
- Missing and stale sessions, ambiguous exact matches, missing panes, and
  unrecognized prompts return distinct categories.
- Dispatch repeats discovery and guarded validation after successful preflight.
- Manual failures persist one bounded diagnostic containing no terminal or
  manual text.

### TUI coverage

- Preflight is non-blocking and single-flight.
- Permission, recovery, and no-semantic-prompt menus expose only their valid
  shortcuts.
- Continue cannot be selected when preflight did not expose it.
- Preflight and dispatch failures survive refresh in Diagnostics.
- Existing hidden 4096-byte manual-text behavior remains unchanged.
- Narrow and wide Live layouts retain discoverable `x action` help.

### Recovery coverage

- Eligible automatic Codex recovery produces one guarded Continue delivery and
  evaluating/delivered audit.
- Leave-alone and below-threshold inference, changed evidence, duplicate and
  cooldown reservation, send failure, post-send non-advancement, and postflight
  uncertainty produce the expected safe diagnostic.
- Duplicate polling produces one visible diagnostic identity per attempt and
  category.

### Verification

Fixture-backed core tests and injected runtime/coordinator tests provide
deterministic exact-target coverage; CI does not require a live tmux process.
Run focused core, runtime, TUI, and recovery tests first, then:

```bash
nix develop path:. --command cargo test --workspace --all-targets
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
```

## Documentation

Update `docs/reference.md` and `docs/terminal-support.md` to describe
preflight-filtered action menus, dispatch-time revalidation, and the new
session-action/recovery diagnostic categories. Update concise overview wording
where it currently defines Diagnostics as excluding failed actions. Keep the
documentation clear that Diagnostics contains metadata-only safe categories,
not captured terminal or manual text.

## Acceptance mapping

1. Successful manual Continue is covered by typed preflight plus unchanged
   authoritative guarded dispatch and post-send advancement.
2. Automatic eligible recovery retains candidate evaluation, reservation,
   guarded delivery, and durable success audit.
3. Live exposes Continue only after recognizing a complete recovery prompt and
   otherwise explains the required state.
4. Typed safe categories distinguish identity, prompt, model, reservation,
   send, and delivery-unknown failures.
5. Classified failure, abstention, and uncertainty records persist in
   Diagnostics across refresh.
6. Exact identity, unique pane, versioned prompt matching, confidence,
   recapture, reservation, cooldown, postflight, and fail-closed semantics are
   unchanged.
7. The test matrix covers the required manual, automatic, stale, missing,
   changed, threshold, reservation, and postflight cases.
8. TUI regressions cover discoverability in narrow and wide Live layouts.

## Stress Test Results: Guarded Live Continue Reliability

### Resolved Decisions

- Preflight resolves and captures the exact pane before exposing any action,
  but returns no evidence reusable as dispatch authority.
- `BrainInput` retains the capability set so rendering and key dispatch cannot
  disagree.
- Manual operator invocations use unique opaque attempt IDs; automatic
  diagnostics use stable provider/session/epoch/category identities.
- Automatic recovery keeps its mandatory pre-send audit, while manual actions
  append classified failure diagnostics without adding a new authority gate.
- Automatic diagnostic deduplication combines the existing atomic
  `append_if_absent` store operation with a bounded coordinator-local cache.
- Delivery certainty becomes unknown after any backend send call is attempted;
  string parsing is not used for classification.
- The work remains one coupled safety spec with independently testable plan
  tasks and deterministic injected terminal/runtime tests.
- User-facing reference and terminal-support documentation updates are
  required.

### Changes Made

- Tightened preflight prerequisites and capability-key enforcement.
- Defined distinct manual and automatic diagnostic identities.
- Clarified persistence ordering and no-resend behavior.
- Added typed source-level failure and delivery-certainty requirements.
- Added bounded recovery diagnostic deduplication without changing reservation
  or lock semantics.
- Made documentation updates and non-live-tmux CI coverage explicit.

### Deferred / Parking Lot

- A distinct automatic candidate-discovery defect will be filed only if an
  eligible manual guarded Continue succeeds while RecoveryCoordinator
  discovery or evaluation independently fails.

### Confidence Assessment

- Overall: High
- Areas of concern: implementation must preserve the 100 ms activity-store
  lock bound and keep preflight evidence strictly non-authoritative.
