# Coding Brain

Coding Brain is a local TUI for supervising Codex through judgment and learning. It observes hook and transcript evidence, evaluates permission requests with deterministic rules and an optional local LLM, and learns from operator corrections. It does not schedule work or replace a task tracker.

The default TUI has three views:

- **Live** shows active sessions, current activity, attention state, and the latest Brain decision.
- **Review** collects denials, corrections, and other decisions worth teaching from.
- **Scorecard** tracks decision quality and whether the Brain is improving.

From Live or Review, you can switch to the selected session. If [Agent Deck](https://github.com/asheshgoplani/agent-deck) is installed and the session is managed by it, Coding Brain can attach through Agent Deck; this integration is optional.

## Install and activate

The crates.io package and its executable are both named `coding-brain`:

```bash
cargo install coding-brain
coding-brain init
coding-brain doctor
# Restart Codex after doctor reports the new managed hooks.
coding-brain
```

From this repository, use `cargo install --path .`. Nix users can install the default flake package; Home Manager users can enable `programs.coding-brain`.

`coding-brain init` creates `.coding-brain/project.toml` for stable project identity and installs managed Codex hooks. Review the generated commands with `/hooks` after restarting Codex. Hook events provide the first activity signal; transcript discovery then supplies richer session evidence.

To enable local-model evaluation with Ollama:

```bash
ollama pull gemma4:e4b
ollama serve
coding-brain --brain
```

Suggestions are advisory by default. `--auto-run` opts into high-confidence automatic actions and requires `--brain`.

## State and configuration

Coding Brain uses XDG paths and project-local identity:

- user config: `$XDG_CONFIG_HOME/coding-brain/config.toml`, normally `~/.config/coding-brain/config.toml`
- user state: `$XDG_STATE_HOME/coding-brain/`, normally `~/.local/state/coding-brain/`
- project config: `.coding-brain.toml`
- project identity: `.coding-brain/project.toml`

Project config cannot select `brain.endpoint`; that choice must come from the CLI or user config. A non-loopback endpoint produces a privacy advisory, and remote plaintext HTTP produces a stronger warning because context and credentials may be exposed in transit.

Useful non-TUI commands include:

```bash
coding-brain --brain-review list
coding-brain --brain-stats scorecard
coding-brain --brain-baseline
coding-brain --brain-briefing --project my-project
```

## Product boundary

Coding Brain owns immediate judgment, learning evidence, review, and session navigation. Durable tasks, dependency graphs, claims, blockers, and cross-session handoffs belong in an external tool such as [Beads](https://github.com/steveyegge/beads). Beads and Agent Deck are both optional; neither is a runtime dependency.

## Breaking cutover from codexctl

There is no automatic data migration or compatibility executable. Normal startup and `coding-brain doctor` diagnose exact stale managed hooks but do not modify old hooks or read old config/state. Run `coding-brain init` to replace those managed hook entries atomically, then restart Codex.

Old data remains available for rollback until you purge it. Before purge, rollback means reinstalling the old build and rerunning its init command. When you no longer need that option, `coding-brain init --purge` removes the documented current and legacy global config/state targets after confirmation. Purge is irreversible and does not delete project `.coding-brain.toml` or `.coding-brain/project.toml` files.

To make a fork learn independently, remove `.coding-brain/project.toml` in that fork and rerun `coding-brain init`. Do not edit the UUID by hand.

## Architecture

Coding Brain is a three-crate Rust workspace:

```text
coding-brain -> coding-brain-tui -> coding-brain-core

crates/
├── coding-brain-core/    # session evidence, paths, project identity, runtime contracts
└── coding-brain-tui/     # Live, Review, and Scorecard terminal UI
src/                   # coding-brain binary wiring, local brain, config, init
```

Codex integration reads `~/.codex/sessions/**/rollout-*.jsonl`, installs hooks in `.codex/hooks.json` or `~/.codex/hooks.json`, and uses supported terminal backends for session focus. Agent Deck navigation is used only when explicitly requested from the TUI.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

See the [documentation](https://aleadag.github.io/codexctl/) for configuration, CLI reference, terminal support, and troubleshooting.
