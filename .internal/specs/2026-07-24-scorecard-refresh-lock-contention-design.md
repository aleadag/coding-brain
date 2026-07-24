# Make Scorecard refresh coherent under activity-store contention

> **Date:** 2026-07-24
> **Issue:** codexctl-2iay
> **Status:** Approved design

## Context

One TUI refresh currently asks `BrainSource` for Live, Review, and Scorecard
independently. In the production source, those calls perform five bounded reads
of `activity.jsonl`: one for Live, two for Review, and two for Scorecard because
each learning-decision load also reads activity before the explicit projection
read. Review and Scorecard also read the decision records separately.

Every activity read independently races with lifecycle and permission-hook
writers. The store correctly bounds lock acquisition to 100 ms, but repeated
acquisitions make transient contention more likely and allow one refresh to
combine projections from different ledger snapshots. A late Scorecard timeout
then surfaces as a disruptive global error even when Live and Review already
updated.

## Decision

Replace the three independent `BrainSource` view methods with one bundled
refresh operation returning the Live snapshot, Review queue, and Scorecard
summary together.

The production source will:

1. Acquire the activity-store read lock once and parse the ledger once.
2. Read the decision records once.
3. Derive learning decisions, Live, Review, and Scorecard from those in-memory
   records.
4. Return one complete refresh value.

Activity is read before decisions. This follows the enforced persistence order
in which a decision proposal is written before terminal activity: every
decision referenced by the captured activity should already exist, while a
proposal arriving after that activity snapshot remains uncommitted for this
refresh. The guarantee is one coherent activity snapshot, not a transaction
across the two independently persisted stores.

The Live projector will borrow the parsed activity log rather than consume or
clone its full event history. Only the view models and small diagnostics value
needed by the existing UI are allocated.

The TUI will replace all three views only after the bundled operation succeeds.
It will never apply a partial refresh.

## Performance

The steady-state one-second refresh changes from five activity-file reads and
parses plus two decision-record reads to one of each. It adds no retry loop,
background worker, cache, additional polling, or full activity-history clone.
Projection remains linear in the records already loaded.

The existing 100 ms bounded activity-lock timeout and one-second TUI refresh
interval remain unchanged.

## Error handling

The bundled source contract returns a typed error that distinguishes
activity-lock contention from other failures. Only
`ActivityStoreError::LockTimeout` maps to the busy case; the TUI does not match
error strings.

If the bundled read cannot acquire the activity lock within the existing bound,
the TUI retains the entire previous successful Live, Review, and Scorecard
value. After at least one successful refresh, it displays:

> Brain data busy; showing previous refresh

If the initial refresh is busy before any valid value exists, it instead
displays:

> Brain data busy; retrying

These statuses are informational rather than component failures. Completed
session-action outcomes, non-contention source errors, and recovery errors take
priority over them. The next successful refresh replaces all three views and
clears only a busy status; it does not erase unrelated action or error status.

Non-contention source errors retain the existing bounded, redacted error status
behavior while also preserving the last coherent view.

Activity corruption handling, tail repair, idempotent appends, compaction, and
exclusive writer behavior do not change.

## Scope

Change only:

- the core `BrainSource` refresh contract and its mock;
- production refresh projection in `src/runtime/brain.rs`;
- activity snapshot projection as needed to borrow one parsed log;
- TUI refresh application and focused tests.

Do not change lock timeout values, refresh frequency, activity persistence,
hook behavior, decision semantics, or unrelated UI rendering.

## Verification

Add deterministic coverage that holds the activity lock across a production
refresh and proves:

1. the attempt returns within the existing bound;
2. the production source returns the typed busy error;
3. releasing the lock lets the next source refresh succeed.

Use a scripted TUI source to prove:

1. cold-start contention shows the retrying status;
2. later contention leaves the previous Live, Review, and Scorecard data
   intact;
3. higher-priority statuses are not hidden or cleared by the busy state;
4. the next successful refresh atomically updates all three views and clears
   only the busy status.

Add or update focused source tests proving all projections use one activity-log
read and preserve existing correction semantics. Keep the activity-store
bounded-lock, corruption, idempotency, and compaction tests passing.

Run the focused regressions, relevant TUI/runtime and activity-store test
modules, `cargo fmt --check`, `cargo test`, and
`cargo clippy -- -D warnings`.

## Consequences

- Normal refreshes use substantially less file I/O, JSON parsing, and lock
  acquisition.
- Live, Review, and Scorecard advance together from one activity snapshot.
- Genuine contention briefly shows stale but coherent data instead of a
  partial update and alarming Scorecard failure.
- The core runtime trait becomes explicitly view-bundled, matching how the TUI
  consumes it.

## Stress Test Results: coherent Scorecard refresh

### Resolved Decisions

- **Runtime contract:** Replace the three independent view methods rather than
  retaining an adapter that permits incoherent refreshes.
- **Cross-store ordering:** Read activity before decisions and promise one
  coherent activity snapshot, not transactional atomicity across stores.
- **Contention classification:** Use a typed busy error and never match display
  strings.
- **Status recovery:** Preserve higher-priority action and error feedback, and
  clear only the transient busy status after success.
- **Performance and scale:** Keep one synchronous uncached read per second with
  the existing compaction and timeout bounds.
- **Corruption and security:** Leave store parsing and persistence untouched;
  bound and redact other errors before display.
- **Testing:** Combine a real production-source lock test with deterministic
  scripted TUI state transitions and the existing strict store timing test.
- **Rollback and scope:** Keep one code-only change with no schema, config,
  persistence, hook, timeout, or user-documentation migration.
- **Cold start:** Report retrying rather than claiming that empty defaults are a
  previous successful refresh.

### Changes Made

- Clarified causal read ordering and the limit of the coherence guarantee.
- Added typed contention classification and explicit status priority.
- Added distinct cold-start and stale-view messages.
- Split verification between real lock contention and deterministic UI state
  transitions.

### Deferred / Parking Lot

- Profiling or caching projection work is deferred unless measurements show
  projection, rather than repeated file parsing, is a future bottleneck.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Decision and activity stores remain independently
  persisted by design; the causal write/read ordering must remain covered by
  existing persistence tests.
