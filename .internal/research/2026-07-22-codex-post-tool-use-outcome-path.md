# Research: Codex PostToolUse outcome path

> **Date:** 2026-07-22
> **Bead:** codexctl-i0y.1
> **Status:** Complete

## Summary

Coding Brain's missing outcomes are caused by a contract mismatch, not evidence that current Codex lacks `PostToolUse`: Codex supplies `tool_use_id` on `PreToolUse` and `PostToolUse`, but its documented `PermissionRequest` payload omits that field. Coding Brain copies the absent permission field into Decision activity, then requires exact ID equality when `PostToolUse` arrives, so the join silently misses; the fix should preserve exact-ID joins, add a bounded fallback for current Bash/unified-exec payloads, record PostToolUse receipt explicitly, and diagnose sustained zero outcome coverage.

## Key Findings

### PermissionRequest cannot supply the ID required by the current join

> **Confidence:** high — verified against current official hook tables, captured fixtures, repository code, and the live activity store.

The official event tables list `tool_use_id` for `PreToolUse` and `PostToolUse`, while the complete `PermissionRequest` event-specific table lists only `turn_id`, `tool_name`, `tool_input`, and `tool_input.description`; omission is inferred from the authoritative field table rather than stated in prose. [S1]

Coding Brain nevertheless copies optional `PermissionRequestInput.tool_use_id` into every Decision `SessionTarget`, and `append_outcome` requires exact `(session_id, turn_id, tool_use_id)` equality. The committed captured PermissionRequest fixture omits the field, but the current end-to-end test fabricates `call-1` on PermissionRequest and repeats it on PostToolUse, masking the production mismatch. [S3]

Current runtime evidence agrees: on Coding Brain 0.58.0 with Codex 0.144.6, all 215 terminal Decision rows had no `tool_use_id`, while 3,328 recorded `PreToolUse` rows carried IDs and no Outcome rows existed. These are local aggregate counts only; no command content was retained in this document. [S4]

### The miss is silent and PostToolUse receipt is not persisted

> **Confidence:** high — direct repository trace.

`append_outcome` sets `has_decision_activity` only after the exact ID predicate succeeds. A Decision with matching session and turn but `tool_use_id: None` therefore takes the `Ok(())` branch: no Outcome, no orphan diagnostic, and no evidence that the PostToolUse hook ran. [S3]

The active managed PostToolUse command is `coding-brain --lifecycle-hook`. Unlike other lifecycle events, that path currently attempts an Outcome instead of also appending a Lifecycle observation, so counting zero PostToolUse lifecycle rows does not by itself prove Codex failed to emit the event. Doctor validates configured definitions and lifecycle snapshot readability but never reads activity outcome coverage. [S3]

### Unified exec uses the PostToolUse path, but its response is opaque

> **Confidence:** high for hook coverage and payload identity; medium for result semantics because the public contract intentionally leaves `tool_response` tool-specific.

Current official documentation says unified `exec_command` receives both tool hooks as `Bash`; when the initial call yields, a later `write_stdin` poll can deliver the original command's `PostToolUse` and does not emit a second PreToolUse. [S1] The official config reference marks unified exec stable and enabled by default except on Windows. [S2]

Current OpenAI Codex source constructs PostToolUse with the originating tool-use ID, the original `{"command": ...}` input, and a tool-specific response. Unified exec emits the original command through `post_tool_use_input`; its `tool_response` is a JSON string containing truncated raw output rather than a documented object with a stable exit-code field. [S5] Coding Brain should therefore test the current string-shaped payload and must not invent an undocumented exit-code schema.

## Comparisons

| Approach | Correctness | Scope | Operational visibility |
|-----------|-------------|-------|------------------------|
| Require PermissionRequest `tool_use_id` | Cannot work with the documented current contract | Small but ineffective | Miss remains silent |
| Infer every missing ID from the latest PreToolUse | Handles more tools but can misattribute parallel calls in one turn | Cross-hook state coupling | Better only if ambiguity is explicit |
| Keep exact-ID join, add unique Bash command fallback, and record PostToolUse receipt | Safe when there is one unambiguous current unified-exec candidate; refuses ambiguous matches | Localized to lifecycle outcome path | Enables direct telemetry diagnosis |

## Codebase Context

- `src/init/hooks.rs` installs PostToolUse as `--lifecycle-hook`; the legacy pending-outcome spool is not the active managed path.
- `src/brain/permission_hook.rs` stores the optional permission-side ID verbatim and stores a redacted normalized Bash command.
- `src/lifecycle_hook.rs` has the exact-ID join and the silent no-match branch.
- `crates/coding-brain-tui/src/ui/brain/live.rs` appends `execution not confirmed` to every Delivered row without Outcome.
- `src/doctor.rs` has injected-store health-check patterns, but no activity telemetry check.

## Recommendations

1. Keep `(session_id, turn_id, tool_use_id)` as the primary join whenever both sides provide it.
2. For the documented current Bash/unified-exec gap only, fall back to `(session_id, turn_id, tool_name, normalized redacted command)` and accept the match only when exactly one terminal Decision without an Outcome qualifies; emit a diagnostic on ambiguity instead of guessing.
3. Persist a lightweight PostToolUse lifecycle observation as well as any matched Outcome so Doctor can distinguish absent telemetry from failed attribution.
4. Add one Doctor activity-telemetry check that is advisory for sustained eligible activity with no PostToolUse/Outcome evidence, and pass/skip for healthy or insufficient samples.
5. Remove `execution not confirmed` from ordinary Delivered rows, retain it for Failed/Unknown delivery where uncertainty changes operator action, and preserve explicit succeeded/failed/cancelled Outcome text.
6. Replace the fabricated PermissionRequest test input with the captured current contract and add a current unified-exec PostToolUse fixture whose `tool_response` is a JSON string.

## Open Questions

- The public hook contract does not expose stable unified-exec exit status inside `tool_response`; this issue should preserve existing outcome rendering but should not claim that arbitrary string output proves process success or failure.
- Non-Bash permission outcomes still lack a general cross-event correlation key. The proposed fallback deliberately leaves those unmatched rather than risk misattribution; explicit telemetry makes that limitation visible for a later, evidence-backed design.

## Follow-up Audit: `--record-outcome` Deprecation (2026-07-24)

> **Confidence:** high — independently verified against every managed provider
> hook, the CLI dispatch, the pending/resolved stores, and their report
> consumers.

No managed Codex, Claude, or Antigravity hook invokes `--record-outcome`;
current `PostToolUse` definitions invoke `--lifecycle-hook`. The flag remains a
public manual ingestion surface, however, and its separate spool preserves
exit code, duration, stderr tail, and command detail that structured lifecycle
activity currently reduces to a categorical outcome.

Deprecate and eventually remove `--record-outcome` in a dedicated migration,
not as part of the subagent-identity fix. Removal is safe only after deciding
which detailed telemetry remains supported, moving retained
`--brain-outcomes`/`--brain-baseline` behavior to structured activity, preserving
or explicitly retiring existing resolved files, and giving external
hand-written hook users a compatibility window. The dormant test-failure
marker producer and legacy marker reader should be resolved in the same
cleanup.

## Sources

- [Codex hooks](https://learn.chatgpt.com/docs/hooks.md) — Primary/Official — retrieved 2026-07-22 — event fields and unified-exec hook coverage.
- [Codex advanced configuration](https://learn.chatgpt.com/docs/config-file/config-advanced.md) — Primary/Official — retrieved 2026-07-22 — unified-exec feature status/default.
- [Coding Brain hook and outcome implementation](https://github.com/aleadag/coding-brain) — Primary/Project — inspected 2026-07-22 — active hook command, correlation, rendering, Doctor, and tests.
- [Coding Brain current source at audit revision](https://github.com/aleadag/coding-brain/tree/025c41222f4e09a4dca50bc8ced2b870a509e8f4) — Primary/Project — inspected 2026-07-24 — managed hook commands, public CLI surface, raw outcome telemetry, and report consumers.
- [Codex-only migration](https://github.com/aleadag/coding-brain/commit/1cc010c97808cf6f1ee5d230ebad9bfbcf00d6dc) — Primary/Project history — inspected 2026-07-24 — removed the only repository-owned `outcome-record.sh` producer while retaining the CLI pipeline.
- [Local activity store aggregates](file:///home/alexander/.local/state/coding-brain/activity.jsonl) — Primary/Runtime — inspected 2026-07-22 — ID and outcome coverage counts; content not copied.
- [OpenAI Codex unified-exec handler](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/unified_exec.rs) and [tool output contract](https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/context.rs) — Primary/Official source — inspected 2026-07-22 — original ID/input and string-shaped response.

[S1]: https://learn.chatgpt.com/docs/hooks.md
[S2]: https://learn.chatgpt.com/docs/config-file/config-advanced.md
[S3]: https://github.com/aleadag/coding-brain
[S4]: file:///home/alexander/.local/state/coding-brain/activity.jsonl
[S5]: https://github.com/openai/codex/blob/main/codex-rs/core/src/tools/handlers/unified_exec.rs
