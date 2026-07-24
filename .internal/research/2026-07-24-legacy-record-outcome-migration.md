# Research: Legacy `--record-outcome` migration

> **Date:** 2026-07-24
> **Bead:** codexctl-vwil.1
> **Status:** Complete

## Summary

The public `--record-outcome` pipeline is now a dormant compatibility surface: managed provider hooks use `--lifecycle-hook`, and the current local state has no pending, resolved, orphaned, or test-failure legacy files. The safest migration is to keep exact decision identity as the authority, import only strictly matchable legacy results into the activity ledger, move the useful reports to that ledger with a reduced schema, and remove rather than revive heuristic test-failure fan-out.

## Key Findings

### Managed hooks no longer produce legacy pending outcomes

> **Confidence:** high — verified in the installed hook research and current provider adapter code.

The current managed Codex path sends `PostToolUse` through `--lifecycle-hook`; `--record-outcome` remains a public manual flag but is not installed by managed provider setup. The provider-neutral lifecycle adapters normalize outcomes into `ActivityOutcome`, and the lifecycle hook appends them to `activity.jsonl`. [S1] [S2]

### Legacy reporting depends on data the activity ledger intentionally does not keep

> **Confidence:** high — verified directly against the serialized types and report handlers.

Legacy `ResolvedOutcome` files contain raw exit code, duration, and a bounded stderr tail. `--brain-outcomes` reads those files directly, while `--brain-baseline` joins them to decisions and reports success rate, sample count, median duration, and legacy decision cost. [S3]

The authoritative activity event stores only the normalized outcome class plus bounded, redacted decision context; it has no exit-code, duration, or stderr fields. Extending it to preserve the old report byte-for-byte would therefore be a new activity schema and privacy surface, not a mechanical migration. [S4]

### Existing legacy rows can only be imported safely by exact decision identity

> **Confidence:** high — verified against the current strict outcome contract and reaper.

The current reaper already refuses legacy pending rows unless provider, session, and tool-use identity resolve to one decision activity. A migration can safely append a coarse activity outcome for a resolved legacy file only when its `decision_id` identifies an existing decision activity and that activity has no outcome. Rows without that anchor must remain archived legacy evidence; command/project/time heuristics would violate the strict outcome contract. [S3] [S5]

### The test-failure marker subsystem is dormant and heuristic

> **Confidence:** high — verified by call tracing and the live state inventory.

The fan-out implementation attributes one failed test command to as many as five recent edit decisions by project and a five-minute window. However, `reap_with_runners` names its runner argument `_test_runners` and never calls `fanout_test_failures`, so no new marker can be produced on the current path. Distillation still reads old marker files, and the `test_runners` config remains public. [S3]

The current local state contains one activity ledger and zero legacy pending, resolved, orphaned, or test-failure files. Removing the dormant fan-out and its configuration therefore needs compatibility tests, but no live local data conversion. [S6]

## Comparisons

| Criterion | Coarse ledger migration | Extend activity detail | Archive and retire reports |
|-----------|-------------------------|------------------------|----------------------------|
| Authority | Exact activity decision identity | Exact activity decision identity | No new canonical report source |
| Retained reporting | Outcome/tool/project/command/time; baseline success and count | Preserves exit/duration/stderr and old report shape | Reports disappear |
| Privacy and schema risk | Low | High: adds stderr and duration fields to canonical activity | Low |
| Compatibility effort | Moderate | High | Low |
| Fit with issue note | Preserves useful reports and migrates safe state | Preserves every legacy detail | Does not preserve reporting |

## Codebase Context

- `src/main.rs` exposes `--record-outcome`, `--reap-outcomes`, `--brain-outcomes`, `--brain-baseline`, and their auxiliary flags in long help.
- `src/commands.rs` implements the legacy writer, reaper-triggering reports, and baseline output.
- `src/brain/outcomes.rs` owns all four legacy directories, detailed resolved records, baseline aggregation, and dormant test-failure fan-out.
- `crates/coding-brain-core/src/brain_activity.rs` defines the authoritative append-only activity schema and its coarse `ActivityOutcome`.
- `src/lifecycle_hook.rs` owns current exact outcome correlation and activity append behavior.
- `crates/coding-brain-core/src/config.rs` and `src/config.rs` still expose `brain.test_runners` solely for the dormant marker subsystem.

## Recommendations

1. Use a one-minor-release compatibility window: keep `--record-outcome` and `--reap-outcomes` functional but print a deprecation warning; hide them from help so no new integration adopts them.
2. Add an idempotent legacy import that appends only exact, previously-unrecorded resolved outcomes to the activity ledger. Preserve unmatched legacy files untouched for rollback and inspection.
3. Reimplement `--brain-outcomes` and `--brain-baseline` over the activity ledger. Retain filtering, JSON, top-N, success rate, and sample count; remove exit-code, duration, stderr, and cost fields that the current authoritative source cannot support.
4. Delete dormant test-failure fan-out, marker loading, `DecisionOutcome::TestFailed`, and `brain.test_runners` configuration instead of reactivating heuristic attribution.
5. In the following minor release, remove the deprecated writer/reaper flags and legacy pending/reaper code after the compatibility release has imported safe state.

## Open Questions

- Does “retained reporting” require byte-for-byte preservation of exit code, duration, and stderr, or is preserving report behavior on the authoritative coarse activity ledger sufficient?
- Should removal of the deprecated writer/reaper be implemented now behind an explicit versioned compatibility marker, or tracked as a follow-up bead for the next minor release?

## Refuted / Discarded Claims

- **“The current managed PostToolUse hook still needs `--record-outcome`.”** Refuted by current hook installation and provider adapter code.
- **“The test-failure marker subsystem is active.”** Refuted by the absent call from `reap_with_runners` and zero current marker files.
- **“All legacy state can be migrated by command and time.”** Discarded because it weakens the exact-identity outcome contract.

## Sources

- [Codex PostToolUse outcome-path research](https://github.com/aleadag/coding-brain/blob/main/.internal/research/2026-07-22-codex-post-tool-use-outcome-path.md) — Primary/Project — 2026-07-22 — managed hook path and current payload contract.
- [Provider hook adapters](https://github.com/aleadag/coding-brain/tree/main/src/provider_hooks) — Primary/Project — 2026-07-24 — current normalized lifecycle outcome path.
- [Legacy outcome implementation](https://github.com/aleadag/coding-brain/blob/main/src/brain/outcomes.rs) — Primary/Project — 2026-07-24 — persisted schemas, reaper, reporting inputs, and test-failure fan-out.
- [Activity event schema](https://github.com/aleadag/coding-brain/blob/main/crates/coding-brain-core/src/brain_activity.rs) — Primary/Project — 2026-07-24 — authoritative outcome representation and redaction bounds.
- [Strict outcome ADR](https://github.com/aleadag/coding-brain/blob/main/docs/decisions/ADR-0003-fail-safe-hook-and-learning-persistence.md) — Primary/Project — 2026-07-24 — separation of delivery and outcome evidence.
- Local state inventory under `$XDG_STATE_HOME/coding-brain` — Primary/Runtime — 2026-07-24 — aggregate filenames only; no record contents inspected.
