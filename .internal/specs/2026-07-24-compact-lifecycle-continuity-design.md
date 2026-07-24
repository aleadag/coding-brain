# Compact Lifecycle Continuity Design

## Problem

Codex emits `SessionStart` with source `compact` while continuing the active
turn. The lifecycle projection currently handles every `SessionStart` as a full
session reset. Its reset path moves `current_turn` into `recent_turns`, clears
the current identity, and closes the turn. A later permission event for the
continuing turn is therefore rejected as `RecentTurn`.

Startup, resume, and clear are real identity boundaries and must keep the
existing full-reset behavior.

## Decision

Make lifecycle reset behavior aware of `SessionStartSource`.

For `SessionStartSource::Compact`, do not reset lifecycle projection state.
Preserve the current turn, open/closed state, bounded recent-turn history,
projected status, active subagents, and provider-specific correlation state.
Compact changes transcript context, not execution lifecycle. Existing status
leases and transcript supersession keep preserved status evidence from
remaining stale indefinitely.

For startup, resume, and clear, retain the current behavior: archive the current
turn in the bounded recent-turn queue, clear its identity, close it, and clear
all transient state.

The accepted `SessionStart` event still updates lifecycle metadata, including
the source, cwd, transcript path, signature, sequence, and receipt time.

## Implementation Boundary

Keep the change inside
`crates/coding-brain-core/src/lifecycle/projection.rs`. Extend the existing
`SessionStart` projection path to skip the full lifecycle reset for compact.
The source has the same meaning for every provider; do not add a Codex-only
exception. No public schema, hook input, persistence format, CLI, configuration,
reconciliation, or UI change is required.

Context-length and compaction telemetry remain owned by transcript discovery.
The compact lifecycle event still refreshes cwd, transcript path, source,
signature, sequence, and receipt time.

## Safety

Replay and ambiguity protections remain unchanged. Compact may preserve only
already accepted, bounded state; it cannot introduce or replace an identity.
Events for turns already in `recent_turns` remain rejected, a mismatched turn
still follows the existing `AmbiguousTurn` rules, and stopped turns remain
closed. Existing recent-turn, active-subagent, and provider-correlation caps are
unchanged.

## Tests

Add projection regression coverage proving:

1. `SessionStart(compact)` preserves the active turn, status, subagents, and
   provider correlation state.
2. A permission request for the active turn is applied after compact rather than
   rejected as `RecentTurn`.
3. Compact with no active turn stays empty, and compact after `Stop` does not
   reopen or remove the stopped turn from recent history.
4. A mismatched turn remains `AmbiguousTurn`, and stopped/recent turns remain
   `RecentTurn` after compact.
5. Startup, resume, and clear continue to archive and reject events for the old
   turn.
6. Compact continuity has the same source-defined semantics across providers.

Run the focused lifecycle projection tests, then workspace formatting, tests,
and clippy with warnings denied.

## Non-goals

- Changing concurrent-turn support or turn identity rules.
- Changing lifecycle persistence schemas or hook payload contracts.
- Moving transcript context-length or compaction telemetry into lifecycle
  projection.

## Stress Test Results: Compact Lifecycle Continuity

### Resolved Decisions

- Define compact continuity by `SessionStartSource`, consistently across
  providers.
- Preserve lifecycle state exactly, including closed state; compact never
  reopens a stopped turn.
- Preserve active subagents and provider correlation because compact emits no
  lifecycle events proving that they ended.
- Preserve projected status because existing leases and transcript evidence
  already bound and supersede it safely.
- Keep context-length changes in transcript discovery while refreshing compact
  event metadata in lifecycle projection.
- Keep consecutive identical compact events subject to the existing duplicate
  guard when no lifecycle activity intervenes.
- Retain all existing ambiguity, replay, and capacity protections.

### Changes Made

- Replaced the identity-only preservation design with full bounded lifecycle
  preservation for compact.
- Added empty, stopped, mismatched-turn, recent-turn, provider-scope, and bounded
  state test requirements.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: provider hook ordering is external, so regression tests must
  exercise preservation without assuming an extra event between compact and the
  next permission.
