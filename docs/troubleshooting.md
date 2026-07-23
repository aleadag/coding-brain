# Troubleshooting

Start with:

```bash
coding-brain doctor
```

Doctor reports separate `Codex setup`, `Claude setup`, and `Antigravity setup` rows. An unselected provider with no executable is skipped. A selected provider whose executable disappeared is advisory, while invalid, unsafe, or stale managed definitions fail. The report also keeps Agent Deck navigation, Claude native attach, guarded semantic input, and focus-only fallback separate so focus is never mistaken for input authority.

## Hooks are missing or stale

Run the exact repair command in the provider row, for example:

```bash
coding-brain init codex
coding-brain init claude
coding-brain init antigravity
coding-brain doctor
```

Restart the repaired provider. For Codex, inspect `/hooks`; Coding Brain cannot observe whether Codex trusts a command, so trust remains advisory even when every definition is current.

Init removes only exact Coding Brain-owned definitions. Lookalike, unrelated, disabled, and user-modified former managed entries remain in place. A multi-provider change is fully staged and validated before replacement. If the process is interrupted, recovery completes or rolls back only while recorded hashes still prove which version Coding Brain wrote; a concurrent edit is preserved.

Managed files are `.codex/hooks.json` or `~/.codex/hooks.json` for Codex, `~/.claude/settings.json` for Claude, and `~/.gemini/config/hooks.json` for Antigravity. Do not copy a provider's JSON shape into another file.

## Project identity is missing or malformed

Identity resolution first uses the project-root `.coding-brain/project.toml`, then a canonical network `origin`, and finally a path-derived temporary identity. A normal Git clone with a usable network origin therefore has stable identity without `coding-brain init`. Local paths and `file:` origins are not network origins, so they use the temporary fallback unless a manifest overrides them.

Use `coding-brain init` to create an explicit override when the origin is unusable or when you want to pin identity independently of the remote. Fix malformed TOML in the project-root manifest rather than editing its UUID. If a fork should intentionally learn as a separate project, remove its project-root `.coding-brain/project.toml` and rerun init.

## Provider activity does not appear in Live

Live shows persisted Brain activity, not every idle process or a general session dashboard. Confirm the provider is running, then check its setup row in `coding-brain doctor`. Codex uses rollout evidence under `~/.codex/sessions/`; Claude prefers bounded `claude agents --json` inventory and falls back to its live process; Antigravity uses `agy` process evidence until hooks provide a conversation identity.

Hook events may appear before Codex transcript or Claude inventory evidence can enrich the activity. Claude and Antigravity hook transcript paths are retained as lifecycle identity/status evidence, but their records are not parsed into `AgentSession` context. Run doctor from the same terminal environment as the agent. For terminal-specific setup, see the [navigation matrix](terminal-support.md#navigation-matrix).

## Permission or recovery stayed at the native prompt

Codex and Claude use their structured `PermissionRequest` responses for allow and deny. Antigravity uses structured `PreToolUse`; when Coding Brain abstains or cannot validate input, it returns `ask` and leaves the native prompt in control. Antigravity `Stop` can return structured `continue` after a validated automatic recovery decision.

Codex and Claude continuation, process-only sessions, and prompts outside a structured response contract require guarded tmux input. Coding Brain acts only when current process identity maps to one pane and immediate prompt recapture reproduces the expected provider-specific evidence. If tmux is missing, a pane is ambiguous, or the prompt changed, the action remains unresolved instead of sending input. Use `x`, then `a`, `d`, or `c` from the exact Live activity to retry manually; `x`, then `t` sends bounded hidden literal text only after you confirm with Enter.

## Brain endpoint warnings

The default endpoint is loopback. A remote HTTPS endpoint produces an advisory that transcript context may leave the machine. Remote plaintext HTTP adds a stronger warning because context and credentials may be exposed in transit.

Project `.coding-brain.toml` cannot change the endpoint. Set it in `$XDG_CONFIG_HOME/coding-brain/config.toml` or pass `--url` explicitly.

## State is unavailable or corrupt

Coding Brain state is under `$XDG_STATE_HOME/coding-brain/`, normally `~/.local/state/coding-brain/`. Check ownership and permissions for that directory. A newer-schema advisory means the state was written by a newer build; upgrade before writing it again.

Activity and preference files use bounded, repair-aware writes. If doctor reports corrupt lifecycle state, let the next hook event quarantine and rebuild the snapshot, or remove only that snapshot after inspecting it.

## Agent Deck attach fails

Agent Deck is optional. Confirm its command is on `PATH` and that it can reach the tmux session itself. Cancelling or failing an attach should restore Coding Brain; use the terminal-native switch path when Agent Deck does not own the selected session.

## Rollback and purge

Normal startup and doctor do not modify old data. Before purge, reinstall the old build and rerun its init command if you need to roll back.

`coding-brain init --remove` removes managed hooks and the onboarding marker while preserving data. `coding-brain init --purge` previews the documented current and legacy global config/state targets, rechecks each target after confirmation, and deletes them. Purge is irreversible. It preserves project `.coding-brain.toml`, `.coding-brain/project.toml`, unrelated hooks, and sibling XDG files.

For declarative Home Manager Codex hooks, disable `programs.coding-brain.codexHooks.enable` or revert the module configuration and rebuild. The module does not own Claude or Antigravity JSON; manage those with `coding-brain init claude antigravity`. Do not use imperative removal as the primary rollback for declaratively managed Codex definitions.
