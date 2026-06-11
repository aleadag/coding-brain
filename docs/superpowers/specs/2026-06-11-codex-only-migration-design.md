# Codex-Only Migration Design

Date: 2026-06-11

## Goal

Migrate the project from a Claude Code control plane into a Codex-only control
plane. The first implementation phase should make live behavior and primary
user-facing surfaces Codex-native while avoiding a high-churn crate rename until
the behavior is verified.

## Current State

The codebase is still centered on Claude Code:

- Session discovery reads `~/.claude/sessions/*.json` and resolves transcripts
  under `~/.claude/projects/*/*.jsonl`.
- Runtime session state is represented by `ClaudeSession`.
- Transcript parsing expects Claude-style `message.content` blocks with
  `tool_use` and `tool_result`.
- Process enrichment filters for commands containing `claude`.
- Init installs Claude Code hooks into `~/.claude/settings.json` and writes an
  embedded `claude-plugin/` bundle.
- The coord layer has a Codex adapter, but it is a stub that discovers no
  sessions and cannot send input.

Codex has a different local shape:

- Codex session transcripts live under
  `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
- Transcript events include `session_meta`, `turn_context`, `event_msg`, and
  `response_item`.
- Codex uses native hook sources such as `~/.codex/hooks.json`,
  `~/.codex/config.toml`, project `.codex/hooks.json`, and project
  `.codex/config.toml`.
- Codex supports hook events including `PreToolUse`, `PermissionRequest`,
  `PostToolUse`, `UserPromptSubmit`, and `Stop`.

## Chosen Approach

Use a Codex-native migration.

The implementation should replace the active Claude runtime path with Codex
discovery, transcript parsing, and hooks. It should remove Claude support from
active code paths instead of preserving a multi-agent abstraction.

The implementation should not immediately rename every crate or module. Keeping
`claudectl-core` and `claudectl-tui` temporarily avoids mixing mechanical
renames with behavioral migration. Public naming and docs that users see in
phase 1 should move toward `codexctl`.

## Phase 1 Scope

### Session Discovery

Add Codex discovery that scans `~/.codex/sessions/**/rollout-*.jsonl`.

Discovery should:

- Read only enough from each JSONL file to identify the session.
- Use `session_meta.payload.id` as the session id.
- Use `session_meta.payload.cwd` as the working directory.
- Use `session_meta.payload.timestamp` or file metadata for start time.
- Use file modification time for recency.
- Prefer recent sessions first and tolerate unreadable or malformed files.

The live session source should use Codex discovery only.

### Transcript Parsing

Add a Codex transcript parser instead of adapting the Claude parser in place.

The parser should recognize:

- `session_meta` for id, cwd, model provider, CLI version, and start metadata.
- `turn_context` for current cwd, sandbox, approval policy, model, and related
  runtime context.
- `event_msg` for lightweight progress/status messages.
- `response_item` for user messages, assistant messages, tool calls, tool call
  outputs, and reasoning items when present.

The parser should produce the existing internal facts the monitor needs:

- Last user/assistant activity.
- Pending tool name and command or file path when available.
- Tool completion and error state.
- Last visible output or status text.
- Context or token data only when the Codex event includes it.

If a metric is not available in Codex JSONL, the UI should show an unavailable
state rather than carrying a Claude-specific estimate.

### Session Model

Introduce neutral naming for new code, such as `AgentSession` or
`CodexSession`, while keeping compatibility shims where broad renames would
create churn.

The first phase should focus on behavior:

- Codex sessions appear in the TUI and JSON output.
- Claude-only fields are either populated from Codex data or marked
  unavailable.
- Existing health and brain code consumes the same normalized session facts.

### Process and Terminal Integration

Replace Claude process filtering with Codex process detection where live process
data is needed.

The first version should use process data only as a supplemental signal. Codex
JSONL is the source of truth for session identity and transcript state.

Terminal input and switching should be limited to what can be verified against
Codex sessions. If a terminal backend cannot reliably target a Codex session,
that action should return a clear unsupported error instead of pretending to
work.

### Hooks and Init

Remove Claude Code hook installation from the active init path.

Add Codex-native hook installation using documented Codex hook sources:

- User scope: `~/.codex/hooks.json`.
- Project scope: `.codex/hooks.json`.

The first hook set should be minimal:

- `PermissionRequest` for policy decisions before approvals.
- `PostToolUse` for observing completed tools.
- `Stop` for end-of-turn/session accounting.

Commands should call the `codexctl` binary. Hook install, remove, check, and
upgrade flows should preserve unrelated user hooks and require Codex's normal
hook trust review.

### Plugin and Slash Command Assets

Remove `claude-plugin/` from the active install path.

Do not port Claude slash commands one-for-one. Codex plugins and skills have a
different model, so the first phase should expose equivalent workflows through
the CLI and hooks. A later phase can package Codex skills or a plugin once the
runtime behavior is stable.

### Public Naming

Move primary user-facing naming to `codexctl` in phase 1:

- Binary name.
- CLI help text.
- README quickstart and core feature description.
- Init and doctor messages.
- Hook command strings.

Defer full historical cleanup:

- Crate package names.
- Old blog posts.
- Packaging and release metadata not needed for local verification.
- Large documentation rewrites outside the current behavior.

## Out of Scope

Phase 1 will not:

- Preserve Claude Code as a secondary runtime.
- Fully rename all crates, modules, tests, and internal types.
- Recreate Claude plugin slash commands as Codex commands.
- Implement cloud Codex task orchestration.
- Infer private Codex SQLite schemas without a stable public contract.
- Depend on undocumented Codex internals when the JSONL and hook surfaces are
  sufficient.

## Testing

Add focused tests for:

- Codex JSONL fixture parsing.
- Session discovery from a temporary `~/.codex/sessions` tree.
- Malformed or partial transcript tolerance.
- Hook JSON merge and removal behavior that preserves unrelated hooks.
- The real Codex adapter returning discovered sessions from fixtures.

Update or remove tests that only validate Claude behavior when the behavior has
no Codex equivalent.

## Verification

Success criteria:

- A local Codex transcript from `~/.codex/sessions` appears in `codexctl --json`.
- The TUI can list Codex sessions without Claude files present.
- `codexctl init` writes Codex hook config, not Claude settings.
- Claude plugin assets are no longer installed by init.
- `cargo fmt` passes.
- `cargo test` passes.
- `cargo clippy -- -D warnings` passes, or any remaining failures are documented
  as pre-existing or explicitly out of scope.

## References

- OpenAI Codex hooks: https://developers.openai.com/codex/hooks
- OpenAI Codex plugins: https://developers.openai.com/codex/plugins
- OpenAI Codex permissions: https://developers.openai.com/codex/permissions
- OpenAI migrate to Codex: https://developers.openai.com/codex/migrate
