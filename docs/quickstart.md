# Quick start

## Install and activate

```bash
cargo install coding-brain
coding-brain init
coding-brain doctor
# Restart Codex after doctor reports the new managed hooks.
coding-brain
```

The crates.io package and installed executable are both named `coding-brain`.

During init, Coding Brain detects a local model endpoint, installs managed Codex hooks, offers optional skill suggestions, and creates `.coding-brain/project.toml`. Restart Codex and inspect `/hooks` before trusting the new commands. For non-interactive setup:

```bash
coding-brain init --non-interactive --skip-brain --skip-skills
```

## Use the TUI

Run `coding-brain` to open Live. Press the view keys shown in the footer to move between Live, Review, and Scorecard. Live presents active activity and attention; Review concentrates decisions worth correcting or retaining; Scorecard summarizes decision quality.

Selecting "switch to session" suspends the TUI, focuses or attaches to the selected session, then restores the terminal when you return. If the session belongs to Agent Deck, Coding Brain can use Agent Deck for the attach. Agent Deck is optional and cancellation returns directly to the Brain TUI.

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
coding-brain init
coding-brain doctor
# Restart Codex and review /hooks.
```

Old global data remains untouched so you can roll back by reinstalling the old build and rerunning its init. Once rollback is no longer needed, `coding-brain init --purge` previews the exact current and legacy global targets and asks for confirmation. Purge is irreversible; it does not remove `.coding-brain.toml` or `.coding-brain/project.toml` from projects.

For a fork that should learn independently, remove its `.coding-brain/project.toml` and rerun `coding-brain init`. Do not edit the stored UUID.
