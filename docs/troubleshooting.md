# Troubleshooting

Start with:

```bash
cbrain doctor
```

Doctor reports separate `Codex setup`, `Claude setup`, and `Antigravity setup` rows. An unselected provider with no executable is skipped. A selected provider whose executable disappeared is advisory, while invalid, unsafe, or stale managed definitions fail. For a non-current setup, Doctor lists each resolved candidate in inspection order with its path, scope, ownership, state, and a bounded reason when applicable; the list is empty when no candidate path can be resolved, such as when `HOME` is unavailable. `cbrain doctor --json` exposes the same records under `evidence.provider_files`. The report also keeps Agent Deck navigation, Claude native attach, guarded semantic input, and focus-only fallback separate so focus is never mistaken for input authority.

## Hooks are missing or stale

For hooks managed declaratively through Home Manager, rebuild Home Manager and restart the affected provider. For Codex, inspect `/hooks`, then run `cbrain doctor`.

For providers that are not managed declaratively, run the exact repair command in the provider row, for example:

```bash
cbrain init codex
cbrain init claude
cbrain init antigravity
cbrain doctor
```

Restart the repaired provider. For Codex, inspect `/hooks`; Coding Brain cannot observe whether Codex trusts a command, so trust remains advisory even when every definition is current.

Init removes only exact Coding Brain-owned definitions. Lookalike, unrelated, disabled, and user-modified former managed entries remain in place. A multi-provider change is fully staged and validated before replacement. If the process is interrupted, recovery completes or rolls back only while recorded hashes still prove which version Coding Brain wrote; a concurrent edit is preserved.

Managed files are `.codex/hooks.json` or `~/.codex/hooks.json` for Codex, `~/.claude/settings.json` for Claude, and `~/.gemini/config/hooks.json` for Antigravity. Do not copy a provider's JSON shape into another file.

## Project identity is missing or malformed

Identity resolution first uses the project-root `.coding-brain/project.toml`, then a canonical network `origin`, and finally a path-derived temporary identity. A normal Git clone with a usable network origin therefore has stable identity without `cbrain init`. Local paths and `file:` origins are not network origins, so they use the temporary fallback unless a manifest overrides them.

Use `cbrain init` to create an explicit override when the origin is unusable or when you want to pin identity independently of the remote. Fix malformed TOML in the project-root manifest rather than editing its UUID. If a fork should intentionally learn as a separate project, remove its project-root `.coding-brain/project.toml` and rerun init.

## Provider activity does not appear in Live

Live shows persisted Brain activity, not every idle process or a general session dashboard. Confirm the provider is running, then check its setup row in `cbrain doctor`. Codex uses rollout evidence under `~/.codex/sessions/`; Claude prefers bounded `claude agents --json` inventory and falls back to its live process; Antigravity uses `agy` process evidence until hooks provide a conversation identity.

Hook events may appear before Codex transcript or Claude inventory evidence can enrich the activity. Claude and Antigravity hook transcript paths are retained as lifecycle identity/status evidence, but their records are not parsed into `AgentSession` context. Run doctor from the same terminal environment as the agent. For terminal-specific setup, see the [navigation matrix](terminal-support.md#navigation-matrix).

## Permission or recovery stayed at the native prompt

Codex and Claude use their structured `PermissionRequest` responses for allow and deny. Antigravity uses structured `PreToolUse`; when Coding Brain abstains or cannot validate input, it returns `ask` and leaves the native prompt in control. Antigravity `Stop` can return structured `continue` after a validated automatic recovery decision.

Antigravity CLI (`agy`) 1.1.5 has a confirmed provider-side contract failure: it can invoke the managed `PreToolUse` hook, receive a valid `{"decision":"allow"}` response with a successful exit, and still retain the native tool confirmation. `cbrain doctor` reports `Antigravity hook contract` when it detects this exact affected version with current managed hooks. Keep the native prompt authoritative and upgrade `agy`; do not enable always-proceed or automatic terminal input as a workaround.

Live's `response emitted` status proves that Coding Brain wrote the hook response successfully. It does not prove that the provider accepted the decision or that the tool ran. Only later lifecycle outcome evidence supports an execution claim.

Before treating a future `agy` release as fixed, repeat the isolated real-binary check with a temporary hook that consumes stdin, emits only `{"decision":"allow"}` on stdout, writes nothing to stderr, and exits zero. Use a harmless command and confirm both automatic execution and the matching `PostToolUse` event. Versions other than 1.1.5 remain unverified until that check passes.

Codex and Claude continuation, process-only sessions, and prompts outside a structured response contract require guarded tmux input. Coding Brain acts only when current process identity maps to one pane and immediate prompt recapture reproduces the expected provider-specific evidence. If tmux is missing, a pane is ambiguous, or the prompt changed, the semantic action remains unresolved instead of sending input. Press `x` on the exact Live activity to preflight current availability; only the recognized semantic actions appear, while `t` accepts bounded hidden manual text after you confirm with Enter. Semantic dispatch independently revalidates the exact target and prompt. Manual-text dispatch revalidates the exact target, backend, and bounded capture but does not require prompt equality.

## Live shows INCOMPLETE or Doctor reports permission transaction recovery

Run `cbrain doctor` before changing any state. `INCOMPLETE` with `permission evaluation timed out` means a permission evaluation became stale without terminal evidence. It does not prove that the hook died, that Coding Brain delivered a response, or that the provider executed the command.

Interpret the `Permission transaction recovery` row as follows:

- Pass means Doctor recovered the reported number of valid transactions.
- Advisory means a permission hook still owns an active transaction. Let the hook finish, then rerun `cbrain doctor`.
- Fail means recovery found invalid, over-budget, unresolved, removal-sync-uncertain, or unreadable evidence. The evidence remains retained and permission handling stays fail-closed.

For an over-budget failure, `over_budget_store` identifies `journal_count`, `journal_bytes`, `decisions.jsonl`, or `activity.jsonl`, and `over_budget_limit` gives the enforced count or byte limit. The private journal directory is `$XDG_STATE_HOME/coding-brain/brain/permission-transactions/`; the destination stores are under the same Coding Brain state root.

Do not delete, rename, truncate, or edit a retained journal to clear the row. Stop the affected agent processes, back up the complete `$XDG_STATE_HOME/coding-brain/` directory, and inspect the named store and its permissions before planning maintenance. Preserve invalid or uncertain evidence for diagnosis. After correcting the underlying ownership, capacity, or store-integrity problem, rerun `cbrain doctor` and let its bounded recovery confirm the result.

## Brain endpoint warnings

The default endpoint is loopback. A remote HTTPS endpoint produces an advisory that transcript context may leave the machine. Remote plaintext HTTP adds a stronger warning because context and credentials may be exposed in transit.

Project `.coding-brain.toml` cannot change the endpoint. Set it in `$XDG_CONFIG_HOME/coding-brain/config.toml` or pass `--url` explicitly.

## State is unavailable or corrupt

Coding Brain state is under `$XDG_STATE_HOME/coding-brain/`, normally `~/.local/state/coding-brain/`. Check ownership and permissions for that directory. A newer-schema advisory means the state was written by a newer build; upgrade before writing it again.

Activity and preference files use bounded, repair-aware writes. Do not assume the next hook event repairs every lifecycle failure: permission hooks preserve corrupt or newer lifecycle authority evidence and fail closed without quarantining, replacing, initializing, or writing a fallback snapshot. Back up and inspect the state reported by Doctor before changing it.

Current activity rows use schema v3 and lifecycle snapshots use schema v4. A rollback binary that supports only lifecycle schema v3 rejects a v4 snapshot. If rollback matters, back up the complete `$XDG_STATE_HOME/coding-brain/` directory before upgrading rather than copying only `activity.jsonl`.

## Agent Deck attach fails

Agent Deck is optional. Confirm its command is on `PATH` and that it can reach the tmux session itself. Cancelling or failing an attach should restore Coding Brain; use the terminal-native switch path when Agent Deck does not own the selected session.

## Raw installer stops before download

The raw installer exits before downloading or writing when `${INSTALL_DIR:-/usr/local/bin}/coding-brain` exists, including as a broken symlink. Inspect that path and remove it only after confirming it is the old Coding Brain executable, then rerun the installer. The installer does not delete a path whose ownership it cannot prove.

## Rollback and purge

Normal startup and doctor do not modify old data. Before purge, reinstall the old build and rerun its init command if you need to roll back.

`cbrain init --remove` removes managed hooks and the onboarding marker while preserving data. `cbrain init --purge` previews the documented current and legacy global config/state targets, rechecks each target after confirmation, and deletes them. Purge is irreversible. It preserves project `.coding-brain.toml`, `.coding-brain/project.toml`, unrelated hooks, and sibling XDG files.

For declarative Codex or Claude hooks, disable the corresponding `programs.coding-brain` hook option and rebuild; other provider-module hooks remain. For declarative Antigravity hooks, disable `antigravityHooks.enable` and rebuild. To return Antigravity to mutable configuration, restore the migration backup, remove only its top-level `coding-brain` definition, run `cbrain init antigravity` to install a fresh Coding Brain entry, and verify with `cbrain doctor`. The targeted removal is required because the installer preserves a modified managed definition instead of overwriting it.

`cbrain init --remove` is a full uninstall of all exact Coding Brain-managed provider hooks and the onboarding marker. Do not use it as a single-provider migration or rollback command.
