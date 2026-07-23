# Coding Brain

Coding Brain supervises Codex, Claude Code, and Antigravity CLI (`agy`) through local judgment and learning. Hook events make new activity visible immediately, provider evidence fills in context, and operator corrections become preference evidence for later decisions.

The TUI is organized around three questions:

- **Live:** Which current Brain activity needs attention?
- **Review:** Which denials, corrections, or uncertain decisions should teach the Brain?
- **Scorecard:** Is decision quality improving?

Coding Brain can switch from selected Live activity to its exact source session. Claude background sessions can use native attach; [Agent Deck](https://github.com/asheshgoplani/agent-deck) and terminal focus are optional fallbacks.

## Start here

The Cargo package and installed command are both named `coding-brain`:

```bash
cargo install coding-brain
coding-brain init codex              # or: claude, antigravity, several names, all
coding-brain doctor
# Restart the configured agents after doctor reports current managed hooks.
coding-brain
```

Bare interactive `coding-brain init` detects installed providers and asks which ones to configure. Explicit selectors install that provider set without the picker. Init installs lifecycle, permission, and recovery hooks and creates `.coding-brain/project.toml`; see the [quick start](quickstart.md) for managed paths and non-interactive setup.

## Local model

Deterministic rules work without a model. For local-model evaluation:

```bash
ollama pull gemma4:e4b
ollama serve
coding-brain config set mode on
coding-brain
```

Mode is global and persistent. New installs start in `off`; choose `on` for advisory model evaluation or `auto` for high-confidence automatic decisions. Deterministic safety checks and lifecycle recording stay active in every mode.

## Boundaries and privacy

Coding Brain records immediate activity, decisions, outcomes, corrections, and learned preferences. It is not a general session dashboard. Usage/cost tracking is outside the supported product surface; this provider feature adds no usage/cost ingestion or dashboard/view. Coding Brain also does not own durable tasks, dependencies, claims, or handoffs; use Beads or another external tracker when work must survive a session. Beads is optional.

State lives under `$XDG_STATE_HOME/coding-brain/`, normally `~/.local/state/coding-brain/`. User config lives at `$XDG_CONFIG_HOME/coding-brain/config.toml`. Remote endpoints produce a privacy advisory, with a stronger warning for plaintext HTTP.

There is no automatic migration from the old executable or paths. See the [quick start](quickstart.md#cutover-from-an-older-build) before removing rollback data.
