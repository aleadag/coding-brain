# Research: Codex subagent hook identity

> **Date:** 2026-07-24
> **Bead:** codexctl-e9j2
> **Status:** Complete

## Summary

Codex CLI 0.145.0 does invoke `PreToolUse`, `PermissionRequest`, and `PostToolUse`
inside thread-spawned subagents, but those callbacks retain the parent
`session_id` and carry the child thread identity in `agent_id`. Coding Brain
currently discards `agent_id`, so child activity is persisted under the parent
session and concurrent child turns can fail lifecycle projection with
`AmbiguousTurn`.

## Key Findings

### Codex dispatches normal tool hooks inside thread-spawned subagents

> **Confidence:** high — verified against the tagged 0.145.0 source and a fresh
> local run.

The 0.145.0 hook runtime constructs `PreToolUse`, `PermissionRequest`, and
`PostToolUse` requests with `thread_spawn_subagent_hook_context(...)`. [S1]
The helper supplies an `agent_id` equal to the child thread ID and an
`agent_type` for thread-spawned subagents. [S1]

A fresh local child rollout produced tool-hook activity during this research.
The activity rows used the parent session ID and the child's turn ID, directly
refuting the original inference that zero rows keyed by the child rollout ID
meant zero callbacks. [S4]

### Coding Brain drops the explicit child identity

> **Confidence:** high — direct code inspection and runtime evidence agree.

The Codex permission payload parser reads `session_id`, `turn_id`, tool fields,
and project fields but has no `agent_id` field. The lifecycle parser similarly
delegates to the generic lifecycle input, whose raw normal-tool event shape has
no `agent_id`. [S5]

Consequently, child callbacks enter the lifecycle store under the parent
session key. When multiple parent and child turns are active, a child
permission decision can be rejected as `AmbiguousTurn`; the fresh reproduction
recorded exactly that error while still showing the child command reaching
Brain evaluation. [S4]

### Provider identity semantics are safe enough for exact child attribution

> **Confidence:** high — tagged source and official documentation agree.

Codex documents that subagent hooks use the parent session ID. [S2] In the
tagged runtime, all three normal tool hook requests combine that session ID
with optional subagent context; the context derives `agent_id` from the child
thread ID. [S1] The correct adapter boundary is therefore:

- keep `SubagentStart` and `SubagentStop` projected onto the parent session so
  the parent retains active-child status;
- for normal child callbacks with `agent_id`, use that exact `agent_id` as the
  Brain session identity;
- preserve current behavior when `agent_id` is absent;
- never infer child identity from parent lifecycle timing or command content.

## Comparisons

| Approach | Attribution | Security behavior | Verdict |
|---|---|---|---|
| Keep parent `session_id` | Conflates concurrent parent/children | Can reject or misjoin child evidence | Reject |
| Infer from parent `SubagentStart` | Ambiguous with parallel/nested children | Risks wrong approval attribution | Reject |
| Use callback `agent_id` | Exact child thread identity | Native prompt remains authoritative on absent/invalid identity | Use |
| Doctor-only advisory | Reports a gap but leaves known adapter defect | Safe but incomplete | Insufficient |

## Codebase Context

- `src/provider_hooks/codex.rs` parses Codex permission and lifecycle payloads
  but does not read child `agent_id`.
- `crates/coding-brain-core/src/lifecycle/input.rs` stores `agent_id` only for
  `SubagentStart` and `SubagentStop`.
- `src/brain/permission_hook.rs` and `src/lifecycle_hook.rs` already preserve
  exact parsed session identity in activity rows and outcome correlation.
- `crates/coding-brain-core/src/lifecycle/projection.rs` keys state by provider
  plus session ID, so selecting the child `agent_id` at the adapter boundary
  isolates concurrent child turns without changing the store schema.

## Recommendations

1. Extend the Codex adapter's normal permission and lifecycle inputs with an
   optional bounded `agent_id`.
2. For `PermissionRequest`, `PreToolUse`, and `PostToolUse`, select `agent_id`
   as the lifecycle session ID when present; otherwise retain `session_id`.
3. Keep `SubagentStart` and `SubagentStop` parent-scoped.
4. Add provider-contract tests covering parent `SubagentStart`, a child
   permission/pre/post sequence, exact child attribution, and parent/child
   isolation.
5. Keep existing fail-safe behavior for missing, empty, oversized, or ambiguous
   identity; do not add terminal injection or parent-event inference.

## Open Questions

None load-bearing. Codex does not expose full lineage such as depth or agent
path in these callbacks, but exact child thread identity is sufficient for this
issue's permission and outcome attribution.

## Refuted / Discarded Claims

- **“Codex 0.145.0 omits child tool callbacks.”** Refuted by the tagged source
  and a fresh local reproduction. The earlier count searched activity by child
  rollout ID even though Codex sends the parent `session_id`.
- **“A Doctor advisory is the only safe fix.”** Refuted because current Codex
  supplies an exact child `agent_id`; no heuristic correlation is required.

## Sources

- **[S1]** [Codex 0.145.0 hook runtime](https://github.com/openai/codex/blob/rust-v0.145.0/codex-rs/core/src/hook_runtime.rs) — Primary/Official — 2026-07-24 — tagged request construction and child context derivation.
- **[S2]** [Codex hooks documentation](https://learn.chatgpt.com/docs/hooks) — Primary/Official — 2026-07-24 — common session identity and local-tool coverage contract.
- **[S3]** [Subagent context commit](https://github.com/openai/codex/commit/16d85e270817) — Primary/Official — 2026-07-24 — addition of child context to normal hooks.
- **[S4]** Local Codex 0.145.0 rollout and Coding Brain `activity.jsonl` evidence — Primary/Runtime — 2026-07-24 — child turn `019f92d8-b57d-7bc2-95c3-2719f7db9b19` appears under parent session `019f92d6-f4ac-7de2-9ebb-180e0c4a610c`, including an `AmbiguousTurn` decision error.
- **[S5]** `src/provider_hooks/codex.rs`, `crates/coding-brain-core/src/lifecycle/input.rs`, `src/brain/permission_hook.rs`, and `src/lifecycle_hook.rs` — Primary/Codebase — 2026-07-24 — current adapter and persistence behavior.
