# Research: Antigravity post-invocation continuation

> **Date:** 2026-07-28
> **Bead:** codexctl-qcm5
> **Status:** Complete

## Summary

Antigravity CLI 1.1.7 does not define `PostInvocation` as terminal: it fires after tool calls finish and may force the execution loop to continue, while only `Stop` with `fullyIdle: true` proves that asynchronous work is finished. Coding Brain currently projects `PostInvocation` as lifecycle `Stop`, prematurely erases the bounded invocation-to-step correlation, and therefore rejects legitimate background-task continuation as `AmbiguousTurn`.

The narrow fix is to validate but not project `PostInvocation` as a terminal lifecycle event. The existing real `Stop` path, initial-step floor, per-step replay bits, session qualification, and capacity bound should remain the authority boundary.

## Key Findings

### PostInvocation is not terminal proof

> **Confidence:** high — the current official documentation was independently fetched and citation-verified, and the installed 1.1.7 documentation agrees.

The official contract says `PostInvocation` fires after tool calls finish and may return `terminationBehavior: "force_continue"`. It separately says `Stop` fires when the execution loop terminates, and that `fullyIdle: true` means all background commands or asynchronous tasks have completed. [S1]

The installed Antigravity CLI reports version 1.1.7. Its bundled hook reference gives the same distinction and documents `PostInvocation` as a point to inspect model output and potentially force continuation.

### The exact incident is a legitimate system-driven continuation

> **Confidence:** high — the provider transcript, Coding Brain activity log, and lifecycle snapshot agree on session identity, steps, ordering, and timestamps.

In native session `1791603e-8d81-460b-bc97-f3e400760d52`, `step-64` launched `nix fmt` as background task `task-64`. The provider emitted `PostInvocation` for `invocation-14` at `1785210041209`, which Coding Brain projected as a closed, idle turn.

At transcript step 68, about 8.8 seconds later, Antigravity appended a high-priority `SYSTEM_MESSAGE` stating that `task-64` had finished successfully and explicitly marking the message as not user-sent. The model then emitted a new `run_command` at step 70 without another `PreInvocation`; Coding Brain recorded the later permission attempt as `AmbiguousTurn`.

Evidence:

- `~/.gemini/antigravity-cli/brain/1791603e-8d81-460b-bc97-f3e400760d52/.system_generated/logs/transcript_full.jsonl`, steps 63-70.
- `~/.local/state/coding-brain/activity.jsonl`, lines 8619-8632.
- `~/.local/state/coding-brain/hooks/lifecycle.json`, key `antigravity:36:1791603e-8d81-460b-bc97-f3e400760d52`.

### Coding Brain revokes the only correlation proof at PostInvocation

> **Confidence:** high — directly established by current source and regression tests.

The provider parser maps `PreInvocation` to `UserPromptSubmit("invocation-N")` with an `initialNumSteps` floor, but maps `PostInvocation` to `Stop("invocation-N")`. [S2]

The projection accepts an Antigravity `step-N` child only while an `invocation-*` turn is open, `N` is at or above the initial-step floor, and its per-step event bit has not been replayed. It caps distinct steps at 256. `Stop` closes the turn and clears the floor and replay ledger, so any later recognizable `step-N` candidate becomes `AmbiguousTurn`. [S3]

These guards are already the required fail-safe boundary:

- provider and native session identity are qualified;
- the invocation must have been opened by trusted `PreInvocation`;
- steps below the invocation floor are rejected;
- event replays and unsafe permission reversal are rejected;
- distinct step state is bounded;
- real `Stop` clears authority, so post-stop steps cannot regain it.

## Comparisons

| Approach | Legitimate continuation | Fail-safe properties | Cost |
| --- | --- | --- | --- |
| Keep mapping `PostInvocation` to `Stop` | Rejected as `AmbiguousTurn` | Safe but incorrect | No change |
| Reopen from arbitrary post-invocation `step-N` | Accepted | Weakens trusted-opening proof | Unacceptable |
| Reset the invocation at `PostInvocation` | May be accepted | Loses replay history and can re-authorize old steps | Unacceptable |
| Validate `PostInvocation` but do not project it as terminal | Accepted until real `Stop` | Preserves floor, replay, capacity, session, and stop revocation | Smallest correct change |
| Add a new persisted lifecycle event kind | Accepted | Can preserve guards | Wider schema and exhaustive-match change without a current consumer |

## Codebase Context

- `src/provider_hooks/antigravity.rs` parses provider callbacks and currently turns `PostInvocation` into `LifecycleEventKind::Stop`.
- `src/provider_hooks/mod.rs` requires every parsed provider callback to carry a lifecycle event.
- `src/lifecycle_hook.rs` unconditionally constructs, persists, and projects that event.
- `crates/coding-brain-core/src/lifecycle/projection.rs` owns the bounded invocation/step authority and clears it only through `LifecycleEventKind::Stop`.
- `tests/lifecycle_hook_cli.rs` currently asserts that `PostInvocation` closes `invocation-3`; that assertion encodes the defect.
- `tests/hook_activity.rs` covers the executable permission path and is the correct place for a focused continuation regression.

## Recommendations

1. Represent a successfully parsed provider callback that has no lifecycle transition without inventing a persisted event kind.
2. Make Antigravity `PostInvocation` such a validation-only callback; keep all required payload validation.
3. Continue projecting only trusted Antigravity `Stop` with `fullyIdle: true` as terminal. Keep rejecting `fullyIdle: false` rather than claiming quiescence.
4. Add a regression sequence covering `PreInvocation -> step activity -> PostInvocation -> later step activity -> Stop`, plus stale, replayed, below-floor, capacity, cross-session, and post-stop rejection.
5. Do not parse transcript system-message fields as new authorization input; the supported hook ordering and real `Stop` boundary are sufficient.

## Open Questions

None are load-bearing for implementation. The transcript schema exposes useful system-message fields, but it is not a documented stable hook API and should not become part of lifecycle authority.

## Refuted / Discarded Claims

- Discarded: `PostInvocation` means the execution loop is idle. The official contract reserves termination and async-idle proof for `Stop`.
- Discarded: a later background completion creates a new `PreInvocation`. The exact incident has no such event between `invocation-14` and `step-70`.
- Discarded: arbitrary post-stop steps must be allowed to fix the incident. Real `Stop` remains the revocation boundary; only the premature `PostInvocation` terminal projection must be removed.

## Sources

- [Antigravity hooks](https://www.antigravity.google/docs/hooks) — Primary/Official — retrieved 2026-07-28 — event semantics, continuation output, and `fullyIdle`.
- [Coding Brain Antigravity parser](https://github.com/aleadag/coding-brain/blob/main/src/provider_hooks/antigravity.rs) — Primary/Project — inspected 2026-07-28 — current provider-to-lifecycle mapping.
- [Coding Brain lifecycle projection](https://github.com/aleadag/coding-brain/blob/main/crates/coding-brain-core/src/lifecycle/projection.rs) — Primary/Project — inspected 2026-07-28 — bounded step authority and stop revocation.
- Installed Antigravity CLI 1.1.7 bundled hook reference — Primary/Installed — inspected 2026-07-28 — `/home/alexander/.gemini/antigravity-cli/builtin/skills/agy-customizations/docs/hooks.md`.
- Exact Antigravity runtime session and Coding Brain state — Primary/Runtime — inspected 2026-07-28 — local transcript, activity, and lifecycle files listed above.

[S1]: https://www.antigravity.google/docs/hooks
[S2]: https://github.com/aleadag/coding-brain/blob/main/src/provider_hooks/antigravity.rs
[S3]: https://github.com/aleadag/coding-brain/blob/main/crates/coding-brain-core/src/lifecycle/projection.rs
