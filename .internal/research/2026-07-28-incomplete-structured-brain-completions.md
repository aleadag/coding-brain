# Research: Incomplete Structured Brain Completions

> **Date:** 2026-07-28
> **Bead:** codexctl-e40y
> **Status:** Complete

## Summary

The Brain client currently preserves provider-envelope error precedence but discards completion metadata and collapses generated JSON syntax failures into an opaque string. The observed Ollama response can be complete at the transport/provider level while its `response` ends mid-object, so recovery must key off generated-content syntax rather than trusting `done` or `done_reason` alone.

## Key Findings

### Provider success and generated-content validity are separate boundaries

> **Confidence:** high — confirmed against current code and Ollama's official API reference.

A non-streaming Ollama success envelope contains generated text in `response` and may include `done`, `done_reason`, `prompt_eval_count`, and `eval_count`; those fields describe the provider completion, not whether the generated string is valid application JSON. [S1]

The endpoint-specific extractors correctly give API errors precedence over generated content, but they return only the generated string. `parse_ollama_response` and `parse_openai_response` then call `parse_suggestion_json` directly, without retaining Ollama completion metadata or OpenAI `finish_reason`. [S2]

### All inference errors currently become the same fail-safe activity shape

> **Confidence:** high — independently verified at the pinned repository commit.

`infer_with_program` performs one `curl_post` call. Any returned string error becomes a zero-confidence `abstain` with `source = "error"` in `query.rs`, regardless of whether it came from transport, an API envelope, generated JSON syntax, or a schema-invalid decision. [S2] [S3]

The permission hook maps that error source to `ActivityState::Error`, emits no executable allow/deny response, records `NeedsInput`, and persists only bounded, redacted reasoning. This is fail-safe, but the raw serde EOF message is not actionable. [S4]

### Retry activity must remain internal to inference

> **Confidence:** high — grounded in the current permission and projection paths.

Each permission request creates one activity lifecycle and only serializes an allow/deny response after inference returns a fully parsed suggestion. A retry inside `infer` can therefore preserve one final activity and one possible provider-hook delivery; retrying at the permission-hook or caller level would create additional decisions and unresolved activity. [S4]

Live groups repeated unresolved decisions visually by project and command, but still counts each unresolved lifecycle. A single internal retry avoids adding a second noisy Needs Attention lifecycle for the same permission request. [S5]

## Comparisons

| Criterion | No retry | One syntax-failure retry | Metadata-only retry |
|-----------|----------|--------------------------|---------------------|
| Fail-safe | Yes | Yes; each attempt parsed independently | Yes |
| Recovers transient malformed JSON | No | Yes, once | Only when provider metadata reports truncation |
| Handles observed `done=true`, `done_reason=stop`, mid-JSON case | Classifies only | Yes | No |
| Provider calls | One | At most two | At most two |
| Hook/activity delivery | One final outcome | One final outcome | One final outcome |
| Complexity | Lowest | Moderate | Moderate without covering the reported failure |

## Codebase Context

- `src/brain/client.rs` owns request construction, provider envelope extraction, and generated-decision parsing.
- `src/brain/query.rs` converts every client error into the same fail-safe Brain decision.
- `src/brain/permission_hook.rs` persists one terminal activity after inference and emits no executable response for an error.
- `src/brain/activity.rs` projects unresolved errors into Live Needs Attention and collapses rows while retaining occurrence counts.
- The previous provider-envelope fix intentionally kept native Ollama and OpenAI-compatible schemas separate; this work must not reintroduce a generic envelope parser.

## Recommendations

1. Add a private typed failure boundary in `src/brain/client.rs` that distinguishes transport/envelope/API failures, generated JSON syntax failures, and schema-invalid generated decisions.
2. Preserve endpoint-specific metadata as a small safe diagnostic summary; never retain the prompt or raw generated content.
3. Retry exactly once only for generated JSON syntax failure, within the configured inference budget. Never concatenate, repair, or infer an action from partial JSON.
4. Return one final actionable error after retry exhaustion so the existing permission path remains fail-safe and persists a single bounded reason.
5. Keep provider API-error precedence and successful Ollama/OpenAI parsing covered.

## Open Questions

The implementation design must choose between no retry and one internal syntax-failure retry. Metadata-only retry is ruled out because the reported response metadata said the provider completed normally even though the generated decision string was incomplete.

## Refuted / Discarded Claims

- **`done=true` or `done_reason=stop` proves the structured decision is complete.** Discarded: those fields describe generation completion, while application JSON validity is established only by parsing `response`.
- **Retry only when provider metadata reports truncation.** Discarded for this issue because it would not recover the evidenced mid-object response with normal completion metadata.

## Sources

- [Ollama API: Generate a completion](https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-completion) — Primary/Official — accessed 2026-07-28 — non-streaming response fields and examples. [S1]
- [Brain client at 11524fee](https://github.com/aleadag/coding-brain/blob/11524fee6c47b8502ae00c599efe4beafb8188e8/src/brain/client.rs) — Primary — 2026-07-28 — provider call, envelope extraction, and generated JSON parsing. [S2]
- [Brain query at 11524fee](https://github.com/aleadag/coding-brain/blob/11524fee6c47b8502ae00c599efe4beafb8188e8/src/brain/query.rs) — Primary — 2026-07-28 — inference error mapping. [S3]
- [Permission hook at 11524fee](https://github.com/aleadag/coding-brain/blob/11524fee6c47b8502ae00c599efe4beafb8188e8/src/brain/permission_hook.rs) — Primary — 2026-07-28 — fail-safe activity and delivery behavior. [S4]
- [Activity projection at 11524fee](https://github.com/aleadag/coding-brain/blob/11524fee6c47b8502ae00c599efe4beafb8188e8/src/brain/activity.rs) — Primary — 2026-07-28 — Needs Attention grouping and counts. [S5]
