# Research: Needs Attention acknowledgement lifecycle

> **Date:** 2026-08-03
> **Bead:** codexctl-dkcue
> **Status:** Complete

## Summary

Needs Attention is a bounded projection over durable activity evidence, but its collapsed rows do not currently expose a stable group identity or member set to the TUI. The lowest-risk implementation direction is a separate, versioned operational-review store under the Coding Brain state root, joined into the projection before grouping, counting, ordering, and truncation; this preserves the authoritative activity, correction, scorecard, and learning evidence.

## Key Findings

### The active queue is a projection, not a mutable source of truth

> **Confidence:** high — current source and the product-boundary ADR agree.

`LiveBrainSource` reads `activity.jsonl`, derives the learning projections, and calls `ActivityStore::project_snapshot`; the TUI is therefore a cockpit over persisted evidence rather than the owner of the queue. The existing projection treats outcomes, corrections, and successful supersession as lifecycle resolution, while failed outcomes remain operational attention even though they add zero to `unresolved_count`. [S1] [S4]

Acknowledgement must not be encoded as a false correction. Corrections are already consumed by Review and Scorecard as learning feedback, so overloading them would change product semantics rather than merely cleaning the operational queue. [S2]

### Collapsed rows need an explicit durable identity

> **Confidence:** high — the projection implementation and public DTO directly establish the mismatch.

The projection groups qualifying lifecycles by project ID, rule ID, and fingerprint, falling back through normalized command to activity ID. It accumulates `occurrences` and `unresolved_occurrences`, chooses one representative by risk and recency, sorts the resulting groups, and only then applies the display limit. [S1]

`AttentionItem` exposes only that representative `ActivityItem` and the two counts. Persisting an acknowledgement for the representative `activity_id` would therefore acknowledge only one lifecycle, not the selected collapsed group. The projection should expose an opaque stable group ID plus a per-group occurrence cursor suitable for acknowledging all occurrences visible when the action was captured. [S2]

The cursor must use stable source order, not only a wall-clock timestamp. A newly appended occurrence must compare after an acknowledgement even if timestamps tie or the wall clock moves backward; `(append sequence or byte order, activity_id)` is safer than `recorded_at_ms` alone.

### Review state must be joined before operational counts and limits

> **Confidence:** high — this follows directly from the current projection order and the acceptance criteria.

Filtering acknowledged or archived occurrences after `project_snapshot` would leave `unresolved_count`, collapsed counts, overflow, representative selection, and the 100-group limit describing hidden work. Review state must instead classify each lifecycle before active groups are accumulated. Both recency-first and severity-first ordering can then operate on the same filtered group set, preserving deterministic tie-breaking for `codexctl-s611`. [S1]

An acknowledgement cursor should not suppress a later occurrence in the same group. The later occurrence becomes unseen and active immediately; the displayed counts describe only occurrences newer than the archived cursor plus reviewed-but-not-archived occurrences that remain in the operational queue.

### Runtime actions are the existing mutation boundary

> **Confidence:** high — current trait and TUI input flow are explicit.

`BrainSource::refresh` supplies the snapshot, while `BrainActions` owns correction, canonical marking, session-action delivery, and recovery. There is no acknowledgement or archive operation today. [S3]

The TUI already captures stable identity in `BrainInput`, confirms or cancels the action, persists through `BrainActions`, and refreshes afterward. The cleanup workflow should follow that pattern, especially for a bulk action: capture the exact reviewed group IDs/cursors at prompt creation, require explicit confirmation, and never reinterpret the current positional selection after a refresh. [S3] [S5]

### A side store is safer than extending activity evidence

> **Confidence:** high — current schemas constrain activity rows, while a durable atomic replacement primitive already exists.

Adding acknowledgement as a new `ActivityState` would mix an operational annotation into the authoritative audit schema, require a schema/rollback change, and risk older readers treating new rows as malformed. A dedicated versioned state file avoids changing activity, decision, outcome, correction, scorecard, or learning records. [S2]

The store should use a lock-protected read-modify-write transaction and `durable_replace`, which already enforces private temporary files, flush and file sync, atomic same-directory replacement, and directory sync. Corrupt or unsupported review state should fail visibly and leave the underlying attention evidence available rather than silently treating it as acknowledged. [S6]

## Comparisons

| Criterion | New activity event/state | Append-only acknowledgement side log | Versioned atomic review-state snapshot |
|-----------|--------------------------|--------------------------------------|----------------------------------------|
| Preserves audit semantics | Weak: mixes operational cleanup into evidence schema | Strong | Strong |
| Compatibility cost | High: activity schema and old-reader behavior change | Medium: new parser, tail repair, compaction | Low: isolated schema and reader |
| Concurrent updates | Can reuse ActivityStore | Requires hardened append locking | Requires one lock around read-modify-replace |
| Bounded growth | Requires activity compaction policy | Needs separate compaction | Naturally bounded to live group cursors |
| Recommendation | Reject | Viable if cleanup history itself must be audited | Use this option |

## Codebase Context

- `src/brain/activity.rs` owns lifecycle projection, grouping, counts, ordering, and limits.
- `crates/coding-brain-core/src/brain_activity.rs` owns the DTO shared with the TUI.
- `crates/coding-brain-core/src/runtime.rs` owns the source/action boundary.
- `crates/coding-brain-tui/src/brain_app.rs` owns captured input, confirmation, refresh, and selection behavior.
- `crates/coding-brain-core/src/durable_file.rs` provides the existing atomic durable replacement primitive.
- `codexctl-s611` defines the adjacent ordering requirements but has no written or approved implementation spec; composition must therefore be tested through an explicit ordering policy rather than assumed from an unavailable design.

KB check: no exact acknowledgement lifecycle memory or research entry existed. Related Beads `codexctl-s611` and `codexctl-bjr` were read; they constrain ordering composition and the distinction between actionable unresolved work and resolved activity.

## Recommendations

1. Add a separate schema-versioned `attention-review.json` under `$XDG_STATE_HOME/coding-brain/`, guarded by an owner-only lock and updated with durable atomic replacement.
2. Give every projected attention group an opaque stable ID and source-order cursor. Persist reviewed and archived cursors per group; never persist raw commands, reasoning, or project paths in the review store.
3. Keep acknowledged groups visible but visually marked as reviewed until individually archived or removed by a confirmed “archive all reviewed” action. Newer occurrences in the same group are unseen and active immediately.
4. Apply archived cursors before grouping, counts, representative selection, ordering, overflow, and truncation. Apply the active ordering mode afterward.
5. Capture group IDs and cursors when opening any confirmation. If refresh changes a target before submission, reject or apply only to the captured occurrences; never broaden the action to newly arrived work.
6. Treat malformed, unreadable, or unsupported review state as a visible refresh/action error with no hidden evidence.

## Open Questions

- Whether the stable occurrence cursor should be the activity JSONL byte offset or a projection-local monotonically increasing append index. This should be decided in design; both preserve source order, but byte offsets interact differently with activity compaction.
- Whether individual acknowledgement and individual archive need separate keys, or acknowledgement itself should archive. The acceptance criteria support both; the former makes “new versus reviewed” and safe bulk cleanup explicit but costs one extra state transition.

## Refuted / Discarded Claims

- “Persist the representative activity ID.” Discarded because a row can represent multiple independently durable lifecycles.
- “Filter rows only in the TUI.” Discarded because counts, overflow, grouping, ordering, and limits would remain inconsistent.
- “Reuse corrections for acknowledgement.” Discarded because corrections are learning feedback and already affect Review and Scorecard.

## Sources

- [Activity projection at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/src/brain/activity.rs#L1015-L1245) — Primary/Codebase — 2026-08-03 — lifecycle resolution, attention grouping, counts, ordering, limits, and group-key construction. [S1]
- [Brain activity DTO and schema at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/crates/coding-brain-core/src/brain_activity.rs#L49-L395) — Primary/Codebase — 2026-08-03 — activity states, validation, `AttentionItem`, and snapshot fields. [S2]
- [Runtime boundary at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/crates/coding-brain-core/src/runtime.rs#L350-L395) — Primary/Codebase — 2026-08-03 — `BrainSource`, `BrainActions`, and `BrainRuntime`. [S3]
- [Live runtime projection at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/src/runtime/brain.rs#L130-L190) — Primary/Codebase — 2026-08-03 — persisted activity read and projection into Live, Review, and Scorecard. [S4]
- [TUI captured-input workflow at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/crates/coding-brain-tui/src/brain_app.rs#L97-L120) — Primary/Codebase — 2026-08-03 — stable action identity captured before confirmation. [S5]
- [Durable replacement primitive at 0be14b64](https://github.com/aleadag/coding-brain/blob/0be14b645a89e06d99085c3c75bea3929d7c712d/crates/coding-brain-core/src/durable_file.rs#L1-L78) — Primary/Codebase — 2026-08-03 — owner-only, synced atomic replacement behavior. [S6]
