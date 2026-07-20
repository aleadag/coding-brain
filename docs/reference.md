# Command reference

`coding-brain --help` is the canonical option list. This page groups the main workflows.

## TUI and headless runtime

```bash
coding-brain
coding-brain --theme dark
coding-brain --headless
coding-brain --headless --json
```

The default command opens the Live, Review, and Scorecard TUI. `--headless` keeps evaluation and context-rot prevention active without taking over a terminal; activity remains visible to a Brain TUI running elsewhere.

Session navigation is intentionally narrow: Coding Brain can switch to the selected live session. It may use terminal-native focus or optional Agent Deck attach, but it does not send arbitrary messages, terminate sessions, route work, or spawn workers.

## Brain evaluation

```bash
coding-brain config get mode
coding-brain config set mode off|on|auto
coding-brain --url <endpoint> --brain-model <model>
coding-brain --brain-query --tool Bash --tool-input "cargo test"
```

The mode is global, persists after the settings command exits, and defaults to `off` on a new install. `off` disables model evaluation, `on` enables advisory evaluation, and `auto` allows high-confidence automatic decisions. Deterministic safety checks and lifecycle recording remain active in every mode. `--brain-query` is the non-interactive permission-hook path and normally receives structured hook input rather than being typed manually.

## Learning and diagnostics

```bash
coding-brain --brain-review [list]
coding-brain --brain-mark-canonical <decision-id>
coding-brain --brain-stats <report>
coding-brain --brain-outcomes
coding-brain --brain-baseline [--top N]
coding-brain --insights [on|off|status]
coding-brain --brain-garden [--apply]
coding-brain --brain-briefing --project <name>
coding-brain --autopsy [--session <id>]
```

Review and Scorecard in the TUI are the primary surfaces. These commands expose the same records for scripts, focused reports, or markdown output.

## Setup and health

```bash
coding-brain init
coding-brain init --plugin-only
coding-brain init --check
coding-brain init --upgrade
coding-brain init --remove
coding-brain init --purge
coding-brain doctor [--json]
coding-brain completions <shell>
coding-brain man
```

- `init` runs onboarding and creates stable project identity.
- `--plugin-only` atomically refreshes exact managed Codex hooks.
- `--check` compares onboarding records with current state.
- `--upgrade` refreshes managed hooks and the marker version after reinstalling.
- `--remove` removes managed hooks and the onboarding marker but preserves data.
- `--purge` additionally removes the previewed current and legacy global config/state targets after confirmation. It is irreversible.
- `doctor` checks the executable, hook definitions, trust visibility, project identity, lifecycle state, endpoint privacy, transcript discovery, and terminal integration.

## Configuration helpers

```bash
coding-brain config show
coding-brain config get mode
coding-brain config set mode on
coding-brain config template
coding-brain config validate
coding-brain config init
coding-brain --hooks
```

Current config uses `.coding-brain.toml` and `$XDG_CONFIG_HOME/coding-brain/config.toml`. Old config and state are never read during ordinary operation.

## Product boundary

Coding Brain keeps immediate judgment, learning evidence, review, and navigation local. It has no durable task queue, dependency executor, distributed peer transport, or embedded project tracker. Beads and Agent Deck are optional companion tools for different jobs.
