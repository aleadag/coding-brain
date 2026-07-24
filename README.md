# Coding Brain

Coding Brain is a local TUI for supervising Codex, Claude Code, and Antigravity CLI (`agy`) through judgment and learning. It observes hook, process, and transcript evidence, evaluates permission requests with deterministic rules and an optional local LLM, and learns from operator corrections. It does not schedule work or replace a task tracker.

The default TUI has four views:

- **Live** shows current Brain activity, attention state, and provider-tagged decisions.
- **Review** collects denials, corrections, and other decisions worth teaching from.
- **Scorecard** tracks decision quality and whether the Brain is improving.
- **Diagnostics** shows metadata-only hook/correlation diagnostics and activity-store integrity without treating them as failed commands.

From Live, you can switch to the source session for the selected activity. Exact Claude background identities can use native attach; [Agent Deck](https://github.com/asheshgoplani/agent-deck) and terminal focus are optional fallbacks.

## Install and activate

The crates.io package and its executable are both named `coding-brain`:

```bash
cargo install coding-brain
coding-brain init codex              # or: claude, antigravity, several names, all
coding-brain doctor
# Restart the configured agents after doctor reports current managed hooks.
coding-brain
```

From this repository, use `cargo install --path .`. Nix users can install the default flake package; Home Manager users can enable `programs.coding-brain`.

Project identity resolves in this order: a project-root `.coding-brain/project.toml` override, the canonical network `origin`, then a path-derived temporary identity. A normal Git clone with a usable network origin does not need `coding-brain init` for identity.

Bare interactive `coding-brain init` detects installed provider executables and asks what to configure, with detected providers selected by default. Explicit selectors skip that picker: `coding-brain init codex claude` configures both providers, while `all` is exclusive shorthand for all three. New automation should always provide a selector, for example `coding-brain init claude --non-interactive`.

Init stages and validates the complete selected set before replacing provider files. It preserves unrelated entries and user-modified former managed entries, and its recovery journal uses file evidence to avoid overwriting concurrent edits. Managed paths are `.codex/hooks.json` or `~/.codex/hooks.json` for Codex, `~/.claude/settings.json` for Claude, and `~/.gemini/config/hooks.json` for Antigravity. Init also creates an explicit project identity override when needed.

To enable local-model evaluation with Ollama:

```bash
ollama pull gemma4:e4b
ollama serve
coding-brain config set mode on
coding-brain
```

Mode is global and persists after `config set` exits. New installs default to `off`; use `on` for advisory model evaluation or `auto` for high-confidence automatic decisions. Deterministic safety checks and lifecycle recording remain active in every mode, including `off`.

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

Coding Brain owns immediate judgment, learning evidence, review, recovery, and source-session navigation. It is Brain activity, not a general session dashboard. Usage/cost tracking is outside the supported product surface; this provider feature adds no usage/cost ingestion or dashboard/view. Durable tasks, dependency graphs, claims, blockers, and cross-session handoffs belong in an external tool such as [Beads](https://github.com/steveyegge/beads). Beads and Agent Deck are both optional; neither is a runtime dependency.

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
└── coding-brain-tui/     # Live, Review, Scorecard, and Diagnostics terminal UI
src/                   # coding-brain binary wiring, local brain, config, init
```

Provider integration prefers structured evidence: Codex rollouts and hooks, bounded `claude agents --json` inventory and Claude hooks, and Antigravity tool/invocation/Stop hooks. Process discovery and exact-target terminal handling provide bounded fallback evidence. See the [capability matrix](docs/reference.md#provider-capabilities) for the limits of each path.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

See the [documentation](https://aleadag.github.io/coding-brain/) for configuration, CLI reference, terminal support, and troubleshooting.
