# Quick start

## Install and activate

```bash
cargo install coding-brain
coding-brain init codex              # or: claude, antigravity, several names, all
coding-brain doctor
# Restart the configured agents after doctor reports current managed hooks.
coding-brain
```

The crates.io package and installed executable are both named `coding-brain`.

An explicit selector skips provider detection. Bare interactive `coding-brain init` detects installed executables, selects them by default, and lets you choose providers you plan to install later. `all` selects Codex, Claude Code, and Antigravity CLI and cannot be combined with another selector.

Init detects a local model endpoint, installs the selected managed hooks, offers optional skill suggestions, and creates `.coding-brain/project.toml`. Restart the configured agents after setup. For non-interactive setup, name at least one provider:

```bash
coding-brain init codex claude --non-interactive --skip-brain --skip-skills
```

The provider-less non-interactive form remains a deprecated Codex-only compatibility path and prints:

```text
warning: provider-less --non-interactive is deprecated; use `coding-brain init codex --non-interactive` instead
```

Managed provider files are:

| Provider | Path |
| --- | --- |
| Codex | project `.codex/hooks.json` or user `~/.codex/hooks.json` |
| Claude Code | `~/.claude/settings.json` |
| Antigravity CLI | `~/.gemini/config/hooks.json` |

Init validates and stages every selected file before replacement. Unrelated and user-modified former managed entries are preserved; a failed multi-provider replacement is recovered or rolled back only when the recorded file evidence still matches.

## Use the TUI

Run `coding-brain` to open Live. Press the view keys shown in the footer to move between Live, Review, and Scorecard. Live presents provider-tagged Brain activity and attention; Review concentrates decisions worth correcting or retaining; Scorecard summarizes decision quality. These are Brain views, not a general session dashboard. Usage/cost tracking is outside the supported product surface; this provider feature adds no usage/cost ingestion or dashboard/view.

Within Live, use `j`/`k` or the arrow keys to move inside the selected list. Press `J` for Recent or `K` for Needs Attention; each list restores its last valid selection.

Press Enter in Live to switch to the selected activity's source session. Coding Brain can use exact provider-qualified Agent Deck navigation, native `claude attach` for a background identity, or exact terminal focus; cancellation returns directly to the Brain TUI.

For an activity with exact current authority, press `x` to enter one-shot action mode, then `a` to allow, `d` to deny, `c` to continue, or `t` to enter bounded hidden literal text. Press Enter to send manual text and Escape to cancel. Outside action mode, Enter keeps its navigation behavior. Review and Scorecard remain read-only for session actions.

## Add a local model

```bash
ollama pull gemma4:e4b
ollama serve
coding-brain config set mode on
coding-brain
```

The mode setting is global and persists after the command exits. New installs default to `off`, which disables model evaluation while keeping deterministic safety checks and lifecycle recording active. Use `on` for advisory model evaluation, review its suggestions and corrections, then choose `auto` if you want high-confidence automatic decisions:

```bash
coding-brain --brain-review list
coding-brain --brain-stats scorecard
coding-brain config set mode auto
```

## Cutover from an older build

Coding Brain does not read the old config/state namespace and does not install a `codexctl` compatibility executable. Normal startup and doctor can diagnose exact stale managed hooks, but only init changes them:

```bash
cargo install coding-brain
coding-brain init codex
coding-brain doctor
# Restart Codex and review /hooks.
```

Old global data remains untouched so you can roll back by reinstalling the old build and rerunning its init. Once rollback is no longer needed, `coding-brain init --purge` previews the exact current and legacy global targets and asks for confirmation. Purge is irreversible; it does not remove `.coding-brain.toml` or `.coding-brain/project.toml` from projects.

For a fork that should learn independently, remove its `.coding-brain/project.toml` and rerun `coding-brain init`. Do not edit the stored UUID.
