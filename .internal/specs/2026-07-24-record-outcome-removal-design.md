# Remove the legacy record-outcome pipeline

> **Date:** 2026-07-24
> **Issue:** codexctl-vwil
> **Status:** Approved and stress-tested design

## Context

Coding Brain currently has two outcome paths:

- Managed Codex, Claude, and Antigravity hooks send provider lifecycle events through the hidden `--lifecycle-hook` adapter. Exact provider/session/turn/tool evidence is normalized into the authoritative `activity.jsonl` ledger.
- The older public `--record-outcome` command writes detailed pending JSON files. A reaper attributes those files to decisions, writes resolved JSON files, and supplies two standalone reports: `--brain-outcomes` and `--brain-baseline`.

No managed provider hook invokes `--record-outcome`. The legacy reports have no internal consumers, and the current local state contains no pending, resolved, orphaned, or test-failure legacy files. The legacy test-failure fan-out is also dormant: its implementation remains present, but the current reaper does not call it.

## Decision

Remove the complete legacy record-outcome pipeline now, without a deprecation window or state migration.

Existing files under the legacy pending, resolved, orphaned, and test-failure directories remain untouched. Coding Brain stops reading and writing them. The active lifecycle-hook outcome pipeline and its strict identity rules do not change.

## Scope

### Remove the public CLI surface

Remove:

- `--record-outcome`
- `--reap-outcomes`
- `--brain-outcomes`
- `--brain-baseline`
- `--exit-code`
- `--duration-ms`
- `--stderr-tail`
- `--session-id`
- `--tool-use-id`
- `--top` when it has no remaining consumer

Clap will reject these flags as unknown arguments. No compatibility aliases or custom migration messages are added.

Retain shared flags such as `--tool`, `--tool-input`, and `--project` when they still serve supported Brain commands.

### Remove legacy implementation

Delete:

- Pending and resolved outcome data types and directory helpers.
- Pending writer, reaper, fuzzy and strict legacy attribution, orphan handling, and detailed report formatting.
- Approach-baseline aggregation over resolved legacy files.
- Headless-loop reaper invocation, the now-single-purpose `run_after_outcome_change` entry point, and the outcome-triggered legacy distillation branch.
- The legacy outcomes module when no supported caller remains.

### Remove dormant test-failure attribution

Delete:

- Test-runner matching and five-minute edit fan-out.
- Test-failure marker persistence and distillation overlay.
- `DecisionOutcome::TestFailed` and its special preference weighting.
- `test_failed` projection and counterfactual-report handling.
- `brain.test_runners`, its defaults, TOML parsing and merge logic, configuration display/template entries, and tests.

Existing `test-failures/*.json` files are not deleted or migrated.

Keep `test_runners` in the explicit removed-key diagnostics after deleting its parser support. Startup and `config validate` must report that legacy heuristic test-failure attribution was removed and instruct the user to delete the setting.

### Update documentation and tests

Remove obsolete examples and reference entries for the four deleted commands and `test_runners`. Add a breaking-change entry to `CHANGELOG.md` naming every removed flag, explaining that legacy state files remain untouched, and noting that those files may contain historical command or stderr data.

Replace legacy outcome/reaper/report integration coverage with regression tests that assert the removed CLI flags are absent from long help and rejected by parsing. Keep all active lifecycle outcome, activity projection, Doctor telemetry, review, scorecard, and distillation tests.

## Runtime behavior

After this change:

1. Provider lifecycle hooks continue to record exact outcome evidence through `--lifecycle-hook`.
2. The activity ledger remains the sole current outcome store.
3. Doctor continues to assess current lifecycle/outcome telemetry from activity.
4. Review and Scorecard continue to use committed decisions and activity corrections.
5. Legacy outcome directories, if present, are inert rollback artifacts.

No startup scan, migration marker, archive move, or data deletion occurs.

The implementation must not make behavioral changes to provider adapters, `src/lifecycle_hook.rs`, the activity schema or store, Doctor outcome telemetry, or TUI outcome rendering. If removal exposes a required active-path change, capture it as separate work.

## Error handling

- Invoking a removed flag produces Clap's standard unknown-argument error and nonzero exit.
- A configured `[brain].test_runners` key produces a targeted removal warning rather than being silently ignored.
- Existing legacy files never cause startup warnings because supported code no longer opens them.
- Errors in the active lifecycle-hook and activity-store path retain their current fail-safe behavior.

## Security and privacy

This removal reduces stored sensitive material by ending new writes of command text and stderr tails to legacy JSON files. It does not broaden outcome correlation, weaken exact provider identity, reinterpret old evidence, or delete user data.

The implementation must not copy legacy stderr, duration, cost, or command fields into `activity.jsonl`.

## Verification

Implementation is complete when:

1. Searches find no supported CLI or documentation references to the four removed commands or `brain.test_runners`.
2. Removed flags are absent from long help and fail parsing.
3. `[brain].test_runners` produces the targeted removal warning.
4. Active lifecycle-hook outcome tests still pass.
5. Doctor outcome-telemetry tests still pass.
6. Distillation and preference tests pass without `TestFailed`.
7. `cargo fmt --check`, `cargo test`, and `cargo clippy -- -D warnings` pass.
8. A final diff confirms no legacy state files are deleted and no active outcome path changed beyond removing obsolete callers.

## Consequences

- Any unknown external/manual caller of the legacy flags will fail immediately and must stop using them.
- Detailed legacy exit-code, duration, stderr, cost, and baseline reports disappear.
- Existing legacy files remain available for manual rollback inspection but have no supported reader.
- The supported outcome model becomes simpler: provider lifecycle evidence flows into the activity ledger, with no parallel pending-file pipeline.

## Stress Test Results: record-outcome removal

### Resolved Decisions

- **Hidden consumers:** Remove the public flags immediately because no repository or managed-hook consumer remains; document the breaking surface explicitly.
- **State rollback:** Leave every legacy outcome file in place. Do not scan, rename, migrate, archive, or delete it.
- **Configuration compatibility:** Remove `brain.test_runners` behavior but retain a targeted removed-key diagnostic.
- **Lifecycle isolation:** Prohibit behavioral edits to active provider, lifecycle, activity, Doctor, and TUI outcome paths.
- **Learning semantics:** Stop loading test-failure markers and remove their special weighting without converting them into another outcome type.
- **Failure behavior:** Use standard Clap unknown-argument errors instead of compatibility shims.
- **Testing:** Cover removed CLI parsing, the removed-config warning, unchanged active lifecycle behavior, and full workspace quality gates.
- **Security and privacy:** Stop future legacy command/stderr writes, preserve existing files to avoid data loss, and disclose their possible sensitive content.

### Changes Made

- Required a breaking-change changelog entry naming removed flags and legacy-state handling.
- Required a targeted warning for obsolete `[brain].test_runners`.
- Added an explicit active-lifecycle isolation boundary.
- Expanded verification to cover configuration diagnostics.
- Added removal of the orphaned outcome-triggered distillation entry point.

### Deferred / Parking Lot

- No migration, archive command, or narrow legacy-data cleanup command is planned.
- Existing `init --purge` remains the explicit broad state-deletion mechanism.

### Confidence Assessment

- Overall: High
- Areas of concern: Unknown external scripts will fail immediately; this is accepted and documented. Existing legacy files may retain command or stderr data until the user explicitly removes state.
