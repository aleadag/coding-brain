# coding-brain

Local-brain companion for supervising and learning from coding-agent activity.

## Build And Test

```bash
cargo build
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Run `cargo fmt` and `cargo clippy -- -D warnings` before committing.

## Architecture

This is a three-crate Cargo workspace. Dependencies flow downward:

```text
coding-brain -> coding-brain-tui -> coding-brain-core
```

The workspace crates are `coding-brain-core` and `coding-brain-tui`; runtime integration supports Codex, Claude Code, and Antigravity CLI.

```text
crates/
├── coding-brain-core/    # session types, Codex transcript discovery, monitor, runtime traits
└── coding-brain-tui/     # Live/Review/Scorecard/Diagnostics UI, terminal suspend/restore
src/                   # coding-brain binary: brain, config, init, runtime
```

`coding-brain-core` must not depend on binary-only modules. The TUI communicates with the binary through runtime traits in `crates/coding-brain-core/src/runtime.rs`.

## Provider Integration

- Session discovery reads recursive `~/.codex/sessions/**/rollout-*.jsonl`.
- Claude discovery prefers bounded `claude agents --json`; Antigravity process fallback recognizes `agy`.
- Hook install writes Codex `.codex/hooks.json` or `~/.codex/hooks.json`, Claude `~/.claude/settings.json`, and Antigravity `~/.gemini/config/hooks.json`.
- Skill discovery scans `~/.codex/skills`, `~/.codex/plugins/*/skills`, and project `.codex/skills`.
- Navigation may invoke Agent Deck, `claude attach`, or a supported terminal backend; guarded input uses tmux.

## Conventions

- Keep changes surgical and tied to the request.
- Do not add abstractions for one-off behavior.
- Match existing style unless the migration requires a public rename.
- Run Beads commands against the writable planning checkout with `bd -C ~/.beads-planning <command>` (uppercase `-C`); plain `bd` uses the repository-local contributor store, which is read-only.
- Config fields must be added to all three layers: CLI args in `main.rs`, TOML structs in `src/config.rs`, and merge logic in `src/config.rs`.
- All jj descriptions created or updated in this repo must use the emoji conventional format: `<emoji> <type>: <imperative summary>`.
- When starting a jj changeset before editing, set the initial description in that format; do not use a plain temporary subject.
- When asked to write, update, or curate a commit message, use the `commit-message` skill if available.
- In jj repos, honor the exact user-provided revset for commit-message work; do not assume `@`.
- For jj commit messages, inspect with `jj --no-pager show --git <revset>` or `jj --no-pager diff --git`, apply with `jj describe -r <revset> -m "<emoji> <type>: <imperative summary>"`, then verify with `jj --no-pager st` and `jj --no-pager log -r '<revset>|@' --no-graph`.
- Status inference logic has extensive coverage; update tests when changing it.
- Health checks in `crates/coding-brain-core/src/health.rs` have unit coverage; add tests for new checks.
- Terminal backends implement the pattern in `crates/coding-brain-core/src/terminals/mod.rs`.

## Compatibility Notes

Current config and state live under `$XDG_CONFIG_HOME/coding-brain` and `$XDG_STATE_HOME/coding-brain`. Legacy codexctl paths remain untouched for rollback. Do not rename or remove legacy paths unless the task explicitly includes a data migration.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:970c3bf2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Agent Context Profiles

The managed Beads block is task-tracking guidance, not permission to override repository, user, or orchestrator instructions.

- **Conservative (default)**: Use `bd` for task tracking. Do not run git commits, git pushes, or Dolt remote sync unless explicitly asked. At handoff, report changed files, validation, and suggested next commands.
- **Minimal**: Keep tool instruction files as pointers to `bd prime`; use the same conservative git policy unless active instructions say otherwise.
- **Team-maintainer**: Only when the repository explicitly opts in, agents may close beads, run quality gates, commit, and push as part of session close. A current "do not commit" or "do not push" instruction still wins.

## Session Completion

This protocol applies when ending a Beads implementation workflow. It is subordinate to explicit user, repository, and orchestrator instructions.

1. **File issues for remaining work** - Create beads for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **Handle git/sync by active profile**:
   ```bash
   # Conservative/minimal/default: report status and proposed commands; wait for approval.
   git status

   # Team-maintainer opt-in only, unless current instructions forbid it:
   git pull --rebase
   bd dolt push
   git push
   git status
   ```
5. **Hand off** - Summarize changes, validation, issue status, and any blocked sync/commit/push step

**Critical rules:**
- Explicit user or orchestrator instructions override this Beads block.
- Do not commit or push without clear authority from the active profile or the current user request.
- If a required sync or push is blocked, stop and report the exact command and error.
<!-- END BEADS INTEGRATION -->

<!-- BEGIN BEADS CODEX SETUP: generated by bd setup codex -->
## Beads Issue Tracker

Use Beads (`bd`) for durable task tracking in repositories that include it. Use the `beads` skill at `.agents/skills/beads/SKILL.md` (project install) or `~/.agents/skills/beads/SKILL.md` (global install) for Beads workflow guidance, then use the `bd` CLI for issue operations.

### Quick Reference

```bash
bd ready                # Find available work
bd show <id>            # View issue details
bd update <id> --claim  # Claim work
bd close <id>           # Complete work
bd prime                # Refresh Beads context
```

### Rules

- Use `bd` for all task tracking; do not create markdown TODO lists.
- Run `bd prime` when Beads context is missing or stale. Codex 0.129.0+ can load Beads context automatically through native hooks; use `/hooks` to inspect or toggle them.
- Keep persistent project memory in Beads via `bd remember`; do not create ad hoc memory files.

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.
<!-- END BEADS CODEX SETUP -->
