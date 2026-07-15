# codexctl

codexctl is a local-brain companion for Codex sessions. It observes active sessions, evaluates pending actions with deterministic rules and a local LLM, learns from operator corrections, and can execute high-confidence decisions when `--auto-run` is enabled.

It reads Codex transcripts from your machine and presents session state, health, cost, and context pressure in one terminal dashboard. Advisory mode is the default: the brain recommends an action and leaves control with you.

## Install

```bash
cargo install codexctl
# or, from this repository
cargo install --path .
```

## Quick start

```bash
codexctl init
codexctl doctor
codexctl
```

To enable the brain with Ollama:

```bash
ollama pull gemma4:e4b
ollama serve
codexctl --brain
```

Use `codexctl --brain --auto-run` only when you want high-confidence suggestions executed automatically. Without `--auto-run`, suggestions remain advisory.

## What the brain can do

The six immediate actions are:

- `approve`: allow a pending tool call.
- `deny`: reject a risky or unwanted tool call.
- `send`: send text to a waiting session.
- `terminate`: stop a session.
- `route`: send summarized context to another live session.
- `spawn`: open a new Codex session for an immediate subtask.

These are live, best-effort actions. codexctl does not own a durable task ledger, dependency queue, worker pool, or distributed coordinator.

## Learning and review

Brain decisions, outcomes, preferences, prompt overrides, review data, and mailbox state remain under `~/.codexctl/brain/`. Useful commands include:

```bash
codexctl --brain-review list
codexctl --brain-review
codexctl --brain-stats scorecard
codexctl --brain-baseline
codexctl --brain-briefing --project my-project
```

Prompt overrides live in `~/.codexctl/brain/prompts/`. The session mailbox is local and delivery is best effort: a message is marked delivered only after terminal input succeeds.

## Privacy

The default brain endpoint is loopback-only. If an enabled brain uses a non-loopback endpoint, codexctl warns that transcript context may leave the machine. Review the endpoint's privacy policy before continuing.

## External coordination with Beads

codexctl deliberately keeps durable project coordination outside the process. If you need tasks, dependencies, claims, blockers, gates, or handoffs across sessions, use [Beads](https://github.com/steveyegge/beads) or another external tracker. Beads is optional and is not a codexctl runtime dependency.

## Compatibility

Configuration stays in `.codexctl.toml` and `~/.config/codexctl/config.toml`; state stays under `~/.codexctl`. Legacy `[relay]`, `[hive]`, `[idle]`, `[agents.*]`, and `lifecycle.retention_days` settings are ignored with warnings.

Normal startup and `codexctl init --upgrade` leave legacy data untouched. `codexctl init --purge` is the explicit destructive cleanup path.

## Architecture

```text
codexctl -> codexctl-tui -> codexctl-core

crates/
├── codexctl-core/    # session types, discovery, monitoring, runtime contracts
└── codexctl-tui/     # terminal UI, recording, demo fixtures
src/                   # binary wiring, local brain, configuration, init
```

Codex integration uses:

- `~/.codex/sessions/**/rollout-*.jsonl` for session discovery.
- `.codex/hooks.json` and `~/.codex/hooks.json` for hook installation.
- `~/.codex/skills`, plugin skills, and project `.codex/skills` for discovery.
- supported terminal backends for input, focus, launch, and termination.

## Build and test

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

See the [documentation](https://aleadag.github.io/codexctl/) for setup, configuration, CLI reference, terminal support, and troubleshooting.
