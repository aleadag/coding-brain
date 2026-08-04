# Shared Brain review lifecycle design

> **Date:** 2026-08-03
> **Issue:** codexctl-k9h8x
> **Brainstorming:** codexctl-2lw4q
> **Research:** `.internal/research/2026-08-03-needs-attention-acknowledgement-lifecycle.md`
> **Status:** Approved and stress-tested; implementation plan approved and stress-tested

## Context

The Brain TUI exposes four itemized surfaces with inconsistent review behavior:

- Live Needs Attention is a grouped operational queue with no acknowledgement or cleanup action.
- Review is a teaching queue whose `s` action advances only in memory, so skipped items return after refresh or restart.
- Diagnostics is a bounded operational event list with no seen or cleanup state.
- Live Recent is a bounded chronological feed with no indication of what arrived since the user's last review.

Scorecard is different: it is an aggregate over committed decision and activity evidence and is not an itemized review surface.

A Needs-Attention-only acknowledgement file would fix one symptom while preserving inconsistent semantics elsewhere. The product needs one durable review-state mechanism with a small policy adapter per itemized surface. This review state is operational metadata only. It must never erase or rewrite activity, decision, outcome, correction, canonical, scorecard, or learning evidence.

## Goals

1. Give Attention, Review, and Diagnostics an explicit `new -> reviewed -> archived` lifecycle.
2. Give Recent a durable `unseen -> seen` lifecycle without turning the bounded feed into another archive queue.
3. Use consistent keys and interaction semantics across surfaces.
4. Preserve exact-target safety when refreshes or new events race with a user action.
5. Keep review state private, bounded, restart-persistent, and independent from authoritative evidence.
6. Make Attention cleanup compose with grouping, counts, overflow, selection, and both ordering modes tracked by `codexctl-s611`.

## Non-goals

- Purging or modifying authoritative evidence.
- Browsing full archived operational history or restoring an archive older than the latest surface-local archive transaction.
- Synchronizing review state between machines.
- Filtering Scorecard or learning inputs through review state.
- Implementing the user-facing recency/severity configuration owned by `codexctl-s611`.
- Changing correction, canonical, outcome, or learning semantics.

## Architecture

### Shared review-state store

Add a versioned store at:

```text
$XDG_STATE_HOME/coding-brain/review-state.json
```

with the existing `~/.local/state` fallback supplied by `CodingBrainPaths`.

The store is namespaced by a closed `ReviewSurface` enum:

```text
attention
review
diagnostics
recent
```

Each namespace maps a fixed review key to a disposition:

```text
reviewed
archived
```

Recent accepts only `reviewed`; attempting to archive Recent is rejected. Scorecard has no namespace. Each archivable namespace also retains the fixed keys from its latest archive transaction as `last_archive`; a later archive replaces that slot. Recent has no undo slot.

The serialized shape is intentionally small:

```json
{
  "schema_version": 1,
  "surfaces": {
    "attention": {
      "revision": 4,
      "last_archive": ["8d969eef6ecad3c29a3a629280e686cff8ca6116a687e466c0f8e92a0c6a6f5a"],
      "items": {
        "7f83b1657ff1fc53b92dc18148a1d65dfa13514e5d14d42d7f32a0f8f2f6f14a": "reviewed",
        "8d969eef6ecad3c29a3a629280e686cff8ca6116a687e466c0f8e92a0c6a6f5a": "archived"
      }
    },
    "review": {
      "revision": 2,
      "items": {
        "35e995c107a71ca9eb83302b5c236b1c74b92b83a67611a3a27f14e9c132ce24": "reviewed"
      }
    }
  }
}
```

Every item is persisted as a fixed, domain-separated review key:

```text
SHA256("review-item:v1" || surface || length(source_identity) || source_identity)
```

The source identity is normally the authoritative activity or decision ID. The only fallback is the opaque legacy Review identity described below. Fresh evidence is rehashed during every mutation; duplicate derived keys in one source snapshot fail closed. Review keys are operational identity, never execution authority. They contain no commands, reasoning, project paths, raw group keys, or copies of evidence. A surface namespace prevents an action in one view from changing another view's projection of the same underlying evidence. A missing surface has revision zero; revision increment is checked for overflow and never wraps. Revisions are per-surface so an unrelated Recent action does not invalidate an Attention confirmation.

The store owns `review-state.lock`. Every mutation:

1. opens the private lock file;
2. acquires an exclusive bounded lock;
3. rereads and validates the current state;
4. verifies the request's expected surface revision and applies an exact transition;
5. writes with `durable_replace` to a private `0600` temporary file;
6. flushes and syncs the file;
7. atomically replaces the destination with that surface's incremented revision and syncs the state directory.

Reads use the same bounded lock discipline needed to avoid observing a state being concurrently replaced. State-root ancestry and final state/lock entries are opened through the repository's existing secure state-directory primitives where possible; the implementation must not introduce a weaker helper. The store rejects unsupported schema versions, duplicate JSON fields, invalid surface/disposition combinations, malformed review keys, more entries than the retained-source bound, files larger than 8 MiB, symlinks, non-regular files, multiple-link files, wrong ownership, and unsafe permissions. The byte cap is checked before parsing or replacement. A missing file is valid empty state.

### Stable projected identities

Every itemized runtime DTO exposes a stable action target:

```text
ReviewTarget {
    surface,
    display_id,
    new_member_keys,
    reviewed_member_keys,
}
```

- Attention `display_id` is an opaque SHA-256 digest of the current canonical group-key fields. The digest prevents a normalized command fallback from leaking through UI state or status text. The two member lists contain the fixed review keys for active authoritative activities represented by the row, partitioned by current disposition.
- Review uses a non-empty decision ID as both display and sole member identity. A legacy candidate without a decision ID uses an opaque SHA-256 digest of a length-delimited encoding of its immutable persisted identity fields. This makes legacy candidates reviewable and archiveable without inventing a canonical decision ID; the existing canonical actions remain unavailable for those candidates.
- Diagnostics and Recent use the activity ID as both display and sole member identity.

`display_id` preserves selection across refresh. Only fixed keys taken from the member lists are persisted in the review-state store. Acknowledge targets `new_member_keys`; individual archive targets `reviewed_member_keys`. This is important for an Attention row containing both older reviewed occurrences and a later NEW occurrence. It also avoids unbounded raw IDs, clock-based cursors, unstable JSONL byte offsets, and compaction-sensitive ordinal cursors.

Store growth is bounded by retained, surface-eligible authoritative evidence: at most 10,000 fixed keys per namespace and an 8 MiB encoded file. Mutations accept at most 10,000 keys and prune keys that are no longer eligible for their namespace because of resolution, correction, canonical marking, compaction, or retention. Reads ignore stale keys without rewriting state. A later mutation performs the cleanup transactionally.

### Runtime boundary

Extend `BrainActions` with one typed mutation boundary rather than allowing the TUI to write files:

```text
mutate_review_state(
    SetDisposition { surface, keys, disposition, expected_surface_revision }
  | ArchiveAllReviewed { surface, expected_surface_revision, expected_count }
  | UndoLastArchive { surface, expected_surface_revision, expected_count }
)
```

Individual and displayed-bulk requests include one surface and a non-empty set of at most 10,000 captured fixed keys. Mixed-surface requests, duplicates, oversized requests, invalid transitions, and unknown keys are rejected. Archive-all uses the revision-bound surface operation described below instead of serializing every retained key through the TUI.

Every request also carries the active surface revision observed by the projection. Individual actions and `A` carry exact member keys. `D` carries the surface, expected surface revision, and prompted count; after pruning keys absent from the fresh surface-eligible set, it archives every remaining reviewed entry in that surface only if the revision and eligible count still match. `u` carries the expected surface revision and eligible-key count from `last_archive`; it restores keys in `last_archive ∩ eligible ∩ archived`. A concurrent mutation of the same surface rejects the request and forces a fresh prompt instead of broadening the action or losing an update. Mutations in other surfaces do not invalidate it.

The live implementation resolves `CodingBrainPaths`, rereads authoritative evidence, recomputes the exact surface-eligible set before review filtering, rederives the review-key map, rejects duplicate derived keys, revalidates every captured key and transition against that set, and then mutates `ReviewStateStore`. Existence in a different surface is insufficient. Mock actions record the same typed requests for TUI tests.

## Surface policies

### Live Needs Attention

Review classification happens per lifecycle before grouping:

- no review-state entry: NEW and active;
- `reviewed`: reviewed and active;
- `archived`: omitted from the operational projection.

Only active occurrences contribute to group membership, `occurrences`, `unresolved_occurrences`, the active `unresolved_count`, representative selection, overflow, and display limits. Each `AttentionItem` additionally exposes `new_occurrences` and its `ReviewTarget`.

A selected acknowledgement reviews every current member in the group. When a later matching lifecycle arrives, its activity ID has no review-state entry. The same group therefore returns to the NEW partition and renders total plus new counts, for example `x5 · 2 new`. Previously reviewed members remain reviewed and are not broadened into a later bulk action.

Failed outcomes remain eligible Attention items with zero unresolved occurrences. They appear as NEW once, can be reviewed, and leave the active operational queue only through archive. Their underlying outcome remains visible to other projections and evidence readers.

Attention ordering has two stages:

1. NEW groups before fully reviewed groups;
2. the active recency-first or severity-first comparator within each partition.

This feature makes partitioning independent of the comparator and tests both comparator policies. `codexctl-s611` remains responsible for exposing configuration and choosing the default.

### Review

Review candidates are keyed by decision ID.

- no review-state entry: NEW candidate;
- `reviewed`: reviewed candidate, retained but de-emphasized;
- `archived`: omitted from the teaching queue;
- canonical decision: omitted by the existing canonical rule, independently of review state.

`s` becomes an alias for review selected and move next. `m` and `n` retain their existing canonical and note-plus-canonical semantics. Canonical state is learning evidence and is not encoded in `review-state.json`.

### Diagnostics

Diagnostic items are keyed by activity ID and use the full NEW, reviewed, and archived lifecycle. Archived diagnostics no longer occupy the operational list or its count, but remain in `activity.jsonl` and any raw audit tooling.

### Live Recent

Recent is a bounded resolved feed, not a queue. Items are unseen without a state entry and seen when `reviewed`; archive requests are invalid. The chronological order and limit remain unchanged. The UI distinguishes unseen rows and reports the unseen count.

### Scorecard

Scorecard does not read `review-state.json`. It continues to project all committed decisions, corrections, outcomes, and canonical evidence.

## TUI interaction

The same keys apply wherever the active surface supports them:

- `a`: review the selected item or group;
- `A`: open confirmation to review all currently displayed NEW targets in the active surface;
- `d`: open confirmation to archive the selected reviewed target;
- `D`: open confirmation to archive all currently reviewed targets in the active surface.
- `u`: undo the latest archive transaction in the active surface, restoring its still-retained keys to reviewed.

Recent supports `a` and `A` only. Review also keeps `s` as review-and-next. Attention, Review, and Diagnostics expose `u` only when their surface snapshot has a non-empty retained `last_archive`.

An acknowledgement is non-destructive operational metadata; the selected `a` action applies immediately. Every bulk action and every archive action opens a `BrainInput` confirmation containing:

- the surface;
- the action;
- the exact captured target count;
- `y` to confirm and `Esc`/any other key to cancel.

The confirmation stores the exact `ReviewTarget` values and active surface revision captured from the visible snapshot. It never reconstructs targets from the current positional selection. If the source item disappears before submission, that captured target is rejected. If new evidence arrives, it is absent from the request and remains NEW.

`D` is the one exception to carrying every member key: it targets all reviewed entries in the active surface at the captured surface revision, including reviewed entries beyond the display limit. The prompt shows that retained-surface count. Any same-surface review-state change after the prompt opens causes a revision mismatch and cancels the operation for refresh and retry.

Every successful individual or bulk archive replaces that surface's `last_archive` with the exact archived keys. `u` is revision-bound, restores only keys still eligible for that surface and still archived, then clears `last_archive`. It is safe without confirmation because it only returns operational entries as reviewed; it never changes evidence or marks anything NEW. The undo slot survives restart and is independent per surface.

After a successful mutation the TUI refreshes and preserves selection by `display_id`, choosing the nearest remaining row only when that ID left the active projection. Evidence scroll resets only when the effective selection changes.

Footer help is surface-aware so archive keys are not advertised for Recent or Scorecard.

## Refresh and consistency

Refresh performs:

1. permission-transaction recovery;
2. one authoritative evidence snapshot;
3. one review-state snapshot;
4. per-surface policy projection;
5. grouping, counts, ordering, overflow, and limits;
6. stable-identity selection restoration.

The two stores do not require a cross-store atomic transaction because review state can name only existing authoritative IDs and cannot create or modify evidence. The safe race outcomes are:

- new evidence after the evidence read appears on the next refresh and has no review state, so it is NEW;
- review state written after the review-state read appears on the next refresh;
- concurrent review-state writers cannot lose updates because every mutation checks and increments the applicable surface revision under one exclusive lock;
- resolution, correction, canonical marking, compaction, or retention can remove a key from one surface's eligible set even while its evidence exists elsewhere; the stale surface key is ignored and later pruned;
- a user mutation revalidates its captured keys against fresh same-surface eligibility before writing.

No race can cause a newly arrived ID to inherit an older item's state.

## Errors and recovery

- Lock contention maps to the existing Brain `Busy` behavior: initial load retries; later refresh retains the last good snapshot and reports stale data.
- Invalid, unsupported, unsafe, or oversized review state maps to a bounded `BrainSourceError::Other`. The TUI retains its last good snapshot and reports the error.
- If review state cannot be read on initial load, the refresh fails. It must not treat the store as empty because doing so would falsely mark archived items NEW and invite duplicate cleanup actions.
- Mutation failure leaves the confirmation open when retry is safe, or cancels it with a bounded status when target revalidation failed.
- Revision mismatch cancels the stale confirmation, refreshes, and asks the user to retry; it never auto-replays against a larger target set.
- Undo ignores keys no longer eligible for that surface, but a disposition conflict or revision change rejects the whole undo rather than partially restoring a newer archive.
- A durable-replace error before rename preserves the old state. A post-rename directory-sync error reports uncertain durability; the next read determines which complete version is visible. Partial JSON is never accepted.
- Review-state errors never append corrections, canonical marks, decisions, outcomes, or diagnostics to compensate.

## Security and privacy

- Permission hooks, deterministic safety rules, corrections, learning, and Scorecard never read review state. Review keys and dispositions cannot authorize, deny, execute, interrupt, or purge anything.
- State and lock files are owner-only; state-root ancestry follows existing Coding Brain path trust rules.
- Destination and lock symlinks, non-regular files, multiple-link files, wrong ownership, and unsafe modes are rejected without following or mutating their targets.
- The state file contains only fixed domain-separated review keys and dispositions.
- Group display IDs and review keys use SHA-256 over length-delimited, versioned canonical encodings. They are bounded identifiers, not secrets, credentials, or authorization tokens; freshly rederived eligible member-key membership is the mutation authority for operational state only.
- Status and confirmation text include surface, action, and count only—never command, reasoning, or raw group-key material.
- Bulk requests have the 10,000-key target bound and the state has an 8 MiB encoded-size bound before persistence.

## Compatibility

- Missing review state means every retained item starts NEW.
- First run after upgrade intentionally shows retained items as NEW. Coding Brain never silently seeds historical items as reviewed; the explicit bulk actions provide the cutover workflow.
- Current Coding Brain configuration and activity/state paths are unchanged.
- Legacy codexctl paths remain untouched and are not read or migrated.
- Unknown schema versions fail visibly instead of being rewritten.
- Archiving one surface does not affect another surface, Scorecard, raw audit, correction, canonical, or learning projections.
- The Review `s` key keeps its user-facing skip intent but gains persistence; existing `m` and `n` behavior is unchanged.
- Removing `review-state.json` is an explicit recovery reset that makes retained items NEW on the next successful refresh. Coding Brain never deletes it automatically.

## Test strategy

### Store tests

- missing-file empty state and round-trip serialization;
- schema, enum, duplicate-field, key, item-count, and byte-size validation;
- domain separation, deterministic key derivation, and duplicate-derived-key rejection;
- private file and lock permissions;
- destination/lock symlink and non-regular-file rejection;
- old-or-new visibility across replacement and injected sync failures;
- bounded lock contention and concurrent read-modify-write without lost updates;
- stale-revision rejection for individual, displayed-bulk, and archive-all requests;
- stale-key pruning after resolution, correction, canonical marking, compaction, and retention;
- same evidence moving from Attention to Recent while an action is open;
- Recent archive rejection and mixed-surface request rejection.
- one-level per-surface archive undo, replacement by a later archive, restart persistence, and pruning of no-longer-retained undo keys;

### Projection tests

- repeated Attention occurrences with mixed NEW, reviewed, and archived members;
- acknowledgement followed by a new matching occurrence;
- total, new, unresolved, overflow, grouping, representative, and limit correctness after archive;
- resolved failed outcomes review and cleanup;
- NEW partitioning under both recency-first and severity-first comparators;
- Review candidate persistence, canonical independence, and `s` behavior;
- Diagnostics review/archive and Recent unseen/seen behavior;
- namespace independence and Scorecard invariance.
- archive/undo round trips with new evidence arriving between the two actions;

### TUI tests

- consistent keys and surface-aware footer help;
- exact captured target for individual and bulk confirmations;
- archive-all includes reviewed entries beyond display limits and rejects revision changes;
- surface-local undo survives restart, restores to reviewed, and cannot affect a newer archive;
- new arrival while a confirmation is open remains untouched;
- confirmation cancel, revalidation failure, Busy retry, and bounded error text;
- stable selection and evidence-scroll behavior after refresh/archive;
- restart persistence for every surface;
- TestBackend rendering of NEW, reviewed, new-count, unseen-count, and bulk prompts.

### Process and quality gates

Process tests set `HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` explicitly and isolate any environment mutation under the repository's existing locks. Final verification runs:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo build
```

through the repository's Nix development shell, plus the relevant Nix checks.

## Documentation impact

Update the TUI key guidance and configuration/state documentation to describe `review-state.json`, its surface-local semantics, first-run bulk cleanup, failure behavior, explicit reset-to-NEW recovery, durable one-level undo, and the distinction between operational archive and evidence purge. No migration or purge documentation is added because neither operation exists in this scope.

## Stress Test Results: shared Brain review lifecycle

### Resolved Decisions

- Review state is surface-local. The same evidence entering another surface is NEW there because operational handling, outcome review, teaching review, and diagnostics are different jobs.
- Persisted state uses versioned, domain-separated fixed review keys. Group hashes remain display/selection aids; fresh eligible evidence membership is rederived for every mutation.
- `k9h8x` introduces only the internal recency/severity ordering policy seam. It does not ship `codexctl-s611` configuration.
- Authoritative stores are fully read and unlocked before the review-state lock is acquired. Same-surface revisions prevent lost updates and stale bulk actions without coupling unrelated surfaces.
- The store is bounded to 10,000 keys per namespace and 8 MiB encoded. Raw evidence IDs are never persisted or transported as an unbounded bulk request.
- Every archivable surface retains one durable `last_archive`; `u` restores its still-eligible keys to reviewed and survives restart.
- Review state is visibility metadata only and is excluded from all permission, execution, evidence, learning, and Scorecard authority.
- First run fails toward visibility: all retained items are NEW. The store and all four surface adapters ship atomically, with focused gates per implementation task and full Rust/Nix gates at completion.
- Mutation authority is same-surface eligibility, not mere evidence existence. Cross-surface transitions invalidate stale targets and cannot resurrect items in their old operational surface.

### Changes Made

- Broadened the original Attention-only design into one shared cross-view lifecycle.
- Replaced raw persisted member IDs with bounded fixed review keys.
- Added per-surface revisions and revision-bound archive-all behavior.
- Added one-level durable archive undo per archivable surface.
- Strengthened path, ownership, hard-link, size, collision, and authority boundaries.
- Made first-run and reset-to-NEW behavior explicit.
- Added fresh same-surface eligibility validation for every mutation, archive-all, undo, and pruning pass.

### Deferred / Parking Lot

- Full archived-history browsing or restoration older than `last_archive`.
- User-facing Attention ordering configuration (`codexctl-s611`).
- Cross-machine review-state synchronization.

### Confidence Assessment

- Overall: High.
- Remaining concern: implementation breadth is material, so the plan must preserve the shared invariant through small TDD tasks and must not ship partially wired surface behavior.
