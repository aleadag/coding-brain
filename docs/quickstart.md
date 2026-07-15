# Quick Start

## Install and onboard

```bash
cargo install codexctl
codexctl init
codexctl doctor
```

The four onboarding phases cover a weekly budget, local brain detection, Codex hooks, and skill discovery. For automation:

```bash
codexctl init --non-interactive --budget 25 --skip-brain --skip-skills
```

After upgrading, `codexctl init --upgrade` refreshes hook entries and the onboarding marker. It does not open, migrate, or delete legacy state.

## Open the dashboard

Start one or more Codex sessions, then run:

```bash
codexctl
```

Use `codexctl --demo` if you want to explore the dashboard without live sessions.

## Enable the local brain

With Ollama:

```bash
ollama pull gemma4:e4b
ollama serve
codexctl --brain
```

The default mode is advisory. codexctl evaluates pending actions, records the suggestion, and waits for operator control. To execute high-confidence suggestions automatically:

```bash
codexctl --brain --auto-run
```

`--auto-run` can approve, deny, send input, terminate, route summarized context, or spawn a live session. Start in advisory mode and review `codexctl --brain-review list` before enabling it.

## Review what the brain learned

```bash
codexctl --brain-review list
codexctl --brain-review
codexctl --brain-stats scorecard
codexctl --brain-outcomes
codexctl --brain-baseline
```

Brain state is stored under `~/.codexctl/brain/`. A custom prompt can replace a built-in template at `~/.codexctl/brain/prompts/<name>.md`.

## Use a remote endpoint carefully

Set an OpenAI-compatible endpoint with:

```bash
codexctl --brain --url https://brain.example.com/v1/chat/completions
```

A non-loopback endpoint produces a privacy warning because transcript context may leave the machine.

## Coordinate durable work externally

codexctl's route, spawn, and mailbox actions are live-session helpers. For work that must survive restarts or carry dependencies and handoffs, track it in Beads or another external system, then let each Codex session claim and update that external work.

## Remove codexctl

`codexctl init --remove` removes managed hooks and the onboarding marker while preserving state. `codexctl init --purge` additionally deletes `~/.codexctl` and the global config after confirmation.
