# codexctl

codexctl is a local-brain companion for Codex sessions. It observes active sessions, evaluates pending actions with deterministic rules and a local LLM, learns from operator corrections, and can execute high-confidence decisions when `--auto-run` is enabled.

The dashboard reads local Codex transcripts and shows which sessions are processing, waiting, blocked, finished, unhealthy, or approaching budget and context limits.

## Start here

```bash
cargo install codexctl
codexctl init
codexctl doctor
codexctl
```

Enable a local brain with:

```bash
ollama pull gemma4:e4b
ollama serve
codexctl --brain
```

Advisory mode is the default. Add `--auto-run` only when codexctl should execute high-confidence actions automatically.

## Immediate actions

The brain can approve, deny, send input, terminate a session, route summarized context to another live session, or spawn a new Codex session. These actions operate on live sessions; codexctl does not maintain a durable task queue.

## Local learning

Decisions, outcomes, preferences, canonical review examples, prompt overrides, and mailbox state are stored below `~/.codexctl/brain/`. Review the learning loop with `codexctl --brain-review` and inspect metrics with `codexctl --brain-stats scorecard`.

## Privacy

Local loopback endpoints keep transcript context on the machine. codexctl emits an advisory before using a non-loopback brain endpoint because transcript context may leave the machine.

## Durable work

For durable tasks, dependencies, claims, blockers, gates, and handoffs, use [Beads](https://github.com/steveyegge/beads) or another external tracker. Beads is optional; codexctl does not embed or require it.

## Documentation

- [Quick Start](quickstart.md)
- [Configuration](configuration.md)
- [CLI Reference](reference.md)
- [Terminal Support](terminal-support.md)
- [Troubleshooting](troubleshooting.md)
- [Contributing](contributing.md)
