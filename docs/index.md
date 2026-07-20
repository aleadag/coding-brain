# Coding Brain

Coding Brain supervises Codex through local judgment and learning. Hook events make new activity visible immediately, transcript evidence fills in session context, and operator corrections become preference evidence for later decisions.

The TUI is organized around three questions:

- **Live:** What are the active sessions doing, and which one needs attention?
- **Review:** Which denials, corrections, or uncertain decisions should teach the Brain?
- **Scorecard:** Is decision quality improving?

Coding Brain can switch to a selected terminal session. [Agent Deck](https://github.com/asheshgoplani/agent-deck) navigation is available when Agent Deck is installed and owns that session; it is never required.

## Start here

The Cargo package and installed command are both named `coding-brain`:

```bash
cargo install coding-brain
coding-brain init
coding-brain doctor
# Restart Codex after doctor reports the new managed hooks.
coding-brain
```

`coding-brain init` installs lifecycle and permission hooks and creates `.coding-brain/project.toml`. Review the commands with `/hooks` after restarting Codex.

## Local model

Deterministic rules work without a model. For local-model evaluation:

```bash
ollama pull gemma4:e4b
ollama serve
coding-brain --brain
```

Advisory mode is the default. `--auto-run` explicitly enables high-confidence automatic actions.

## Boundaries and privacy

Coding Brain records immediate activity, decisions, outcomes, corrections, and learned preferences. It does not own durable tasks, dependencies, claims, or handoffs; use Beads or another external tracker when work must survive a session. Beads is optional.

State lives under `$XDG_STATE_HOME/coding-brain/`, normally `~/.local/state/coding-brain/`. User config lives at `$XDG_CONFIG_HOME/coding-brain/config.toml`. Remote endpoints produce a privacy advisory, with a stronger warning for plaintext HTTP.

There is no automatic migration from the old executable or paths. See the [quick start](quickstart.md#cutover-from-an-older-build) before removing rollback data.
