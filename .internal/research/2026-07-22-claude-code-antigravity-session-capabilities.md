# Research: Claude Code and Antigravity session capabilities

> **Date:** 2026-07-22
> **Bead:** codexctl-hfk
> **Status:** Complete

## Summary

Both providers expose enough structured data to avoid screen-scraping as the primary integration. Claude Code should use `claude agents --json` plus hooks and JSONL transcripts. Antigravity should use its structured `PreToolUse` and `Stop` decisions plus process identity, while guarded terminal evidence remains a fallback for process-only sessions, manual input, and prompts outside the hook contracts. Usage and cost are documented here only to define the provider boundary; Coding Brain does not collect or display them.

## Key findings

### Claude Code has a supported live-session inventory

> **Confidence:** high — the official Agent View reference was independently checked, with one over-broad interpretation corrected.

Claude Code 2.1.139 and later exposes `claude agents --json`. It returns every live interactive or background session with `cwd`, `kind`, and `startedAt`; live processes may include `pid` and `status`, and `sessionId` is the resumable full UUID when set. Background sessions additionally have a short `id` accepted by `claude attach`, while foreground sessions do not. [S1]

The interactive `claude agents` screen is narrower than the JSON command: foreground sessions in other terminals do not appear as rows until backgrounded. This does not limit the JSON inventory. [S1]

### Claude Code hooks and transcripts provide structured evidence

> **Confidence:** high — official hook, session, and cost references agree, and the hook contract was independently verified.

Claude hooks provide `session_id`, `transcript_path`, `cwd`, and `permission_mode`, with lifecycle-specific fields for session, prompt, tool, notification, permission, stop, and subagent events. Hook payloads remain untrusted input, and a hook allow does not override matching deny or ask rules. [S2]

Session transcripts are continuously stored as JSONL under `~/.claude/projects/<project>/<session-id>.jsonl`, are resumable by ID, may move under `CLAUDE_CONFIG_DIR`, and can be disabled or cleaned up. The hook-supplied transcript path is therefore preferable to directory guessing. Anthropic documents the file as messages, tool calls, and metadata, but does not publish a stable versioned third-party parser schema. [S3]

Claude can expose local estimated cost and token information through `/usage`, status-line JSON, or opt-in OpenTelemetry. Monetary values are estimates rather than authoritative billing. Coding Brain deliberately leaves both usage and cost outside its product surface. [S4]

### Antigravity exposes correlation and transcript paths through hooks

> **Confidence:** high — the official hook contract was independently verified.

Antigravity hooks receive JSON with a unique `conversationId`, `workspacePaths`, and the absolute persistent `transcriptPath`. The CLI transcript path is documented as `~/.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript.jsonl`. Supported events are `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, and `Stop`. [S5]

`PreToolUse` has a documented synchronous decision response: `allow`, `deny`, `ask`, or `force_ask`, with an optional reason and permission overrides. `Stop` accepts `decision: "continue"` with an optional reason. These are the primary Antigravity permission and recovery paths; tmux injection is not required when a managed hook can deliver the action. [S5]

`agy --conversation <conversation-id>` resumes an exact conversation. Antigravity's changelog says SQLite is the conversation format, but Google does not document its schema, locking contract, or relationship to the hook-provided JSONL. Coding Brain should not parse that database in this feature. [S6]

### Antigravity status-line JSON documents useful optional telemetry

> **Confidence:** high — the official schema was independently checked.

Antigravity status-line scripts receive `conversation_id`, optional `transcript_path`, `agent_state`, total and current input/output tokens, context-window utilization, quota state, and `tool_confirmation_pending`. Coding Brain does not install or replace the user's status line in this delivery; hooks and process identity supply its required correlation. Token, quota, and cost tracking remain outside the product boundary. [S7]

Antigravity documents `agy` as the executable. It has no documented external attach or terminal-focus API; `/resume` loads conversation state and internal keybindings move among TUI panels, neither of which focuses the original terminal. [S6][S8]

## Capability matrix

| Capability | Codex today | Claude Code | Antigravity CLI |
| --- | --- | --- | --- |
| Process discovery | `ps` plus Codex executable matching | Prefer `claude agents --json`; `ps` fallback | `agy` process fallback; no documented external live-session list |
| Lifecycle/hooks | Managed Codex hooks | Rich structured hooks including PermissionRequest and notifications | Structured tool/invocation/stop hooks; no distinct documented PermissionRequest event |
| Session identity | rollout UUID joined to live process | hook/session UUID; JSON inventory | hook `conversationId` |
| Transcript | Codex rollout JSONL | hook path to project JSONL; schema not versioned | hook path to persistent transcript JSONL; do not parse undocumented SQLite |
| Usage and cost | Not a Coding Brain product capability | Documented provider capability; not collected or displayed | Documented provider capability; not collected or displayed |
| Permission state | hooks plus guarded terminal evidence | hooks expose requests/mode; deny and ask remain authoritative | `PreToolUse` returns `allow`, `deny`, `ask`, or `force_ask` |
| Terminal action | structured permission guard plus guarded input/recovery | attach background session by short ID; structured permission guard plus guarded recovery input | structured `PreToolUse` and `Stop` decisions first; guarded tmux fallback for process-only/manual/uncovered prompts |

## Codebase context

- `crates/coding-brain-core/src/session.rs:123` is generic in shape but named `CodexSession`; adding an explicit provider discriminator is narrower than renaming every consumer.
- `crates/coding-brain-core/src/discovery.rs:36` is a Codex-only process/transcript monolith. Provider-specific discovery can be aggregated at this entry point without adding speculative traits.
- `crates/coding-brain-core/src/monitor.rs:285` chooses the Codex parser using process state, a magic source string, and filename shape; provider identity should replace that heuristic.
- `crates/coding-brain-core/src/lifecycle/input.rs:101` and `lifecycle/projection.rs:125` key lifecycle evidence without a provider, so raw session IDs can collide across providers.
- `src/runtime/navigation.rs:110` already provides terminal focus fallback, but re-scans only Codex and returns Codex-specific errors.
- `src/init/hooks.rs:14`, `src/doctor.rs:184`, and `README.md:87` are explicitly Codex-only.
- The pre-migration Claude implementation in commit `1cc010c9^` relied on stale `~/.claude/sessions` files. Its surviving generic transcript parser is useful, but discovery should use the current `claude agents --json` contract instead of restoring that scanner.

## Recommendations

1. Add `AgentProvider`, rename the generic record to `AgentSession` in an isolated behavior-neutral step, and then add provider behavior.
2. Namespace lifecycle and navigation identity by provider plus native session ID or an expiring PID/start-time/TTY identity before ingesting non-Codex hooks.
3. Split discovery into provider-specific functions aggregated by `scan_sessions`: preserve Codex unchanged, query bounded `claude agents --json` with `ps` fallback, and detect `agy` by process until hooks supply durable correlation.
4. Route only Brain-relevant identity, lifecycle, permission, outcome, and navigation evidence by provider. Do not add usage/cost ingestion or UI, and do not parse Antigravity SQLite.
5. Use structured hooks as the primary permission guard for all three providers. Use Antigravity `PreToolUse` for allow/deny and `Stop` for continue. Retain provider-specific prompt capture, exact pane revalidation, semantic input, and post-action verification for process-only sessions, manual text, and prompts that have no structured response path.
6. Make init and doctor report each provider separately: structured setup current, executable/process-only degraded, or unavailable.

## Design resolutions

- Init installs managed hooks for every provider selected explicitly or through the interactive provider picker.
- Claude inventory is attempted with a short timeout and output bound; command or schema failure degrades to process discovery without hiding the session.
- Persisted records add provider identity with missing legacy values defaulted to Codex. Derived projections rebuild under a new schema version; raw history is not rewritten.
- Process-only sessions remain actionable through an expiring provider/PID/start-time/TTY identity and exact pane revalidation.
- Antigravity `PreToolUse` and `Stop` responses are primary; guarded tmux input is a bounded fallback rather than the normal permission path.

## Refuted / discarded claims

- Discarded: "`claude agents --json` excludes foreground interactive sessions until they are backgrounded." The official reference says the JSON result includes every live session and identifies each as `interactive` or `background`; only the interactive Agent View rows have the foreground omission.
- Discarded: parse Antigravity SQLite for session telemetry. The format exists, but its schema and concurrency contract are undocumented; the hook and status-line contracts already expose the required supported fields.

## Sources

- [Claude Code Agent View](https://code.claude.com/docs/en/agent-view) — Primary/Official — 2026-07-22 — live inventory fields, background attach, version boundary.
- [Claude Code hooks reference](https://code.claude.com/docs/en/hooks) — Primary/Official — 2026-07-22 — structured event fields, permission decisions, trust guidance.
- [Claude Code sessions](https://code.claude.com/docs/en/sessions) — Primary/Official — 2026-07-22 — JSONL location, resume identity, retention and disable controls.
- [Claude Code costs](https://code.claude.com/docs/en/costs) — Primary/Official — 2026-07-22 — token usage and estimated local cost.
- [Antigravity hooks](https://www.antigravity.google/docs/hooks) — Primary/Official — 2026-07-22 — event list, conversation identity, transcript path.
- [Antigravity resume](https://antigravity.google/docs/cli/commands/resume) — Primary/Official — 2026-07-22 — exact conversation resume and cache behavior.
- [Antigravity status line](https://antigravity.google/docs/cli/statusline) — Primary/Official — 2026-07-22 — live token, state, quota, and confirmation schema.
- [Antigravity permissions](https://antigravity.google/docs/cli/permissions) — Primary/Official — 2026-07-22 — deny/ask/allow rules and defaults.
- [Antigravity CLI changelog](https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md) — Primary/Official — 2026-07-22 — SQLite migration status.
