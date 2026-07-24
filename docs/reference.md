# Command reference

`coding-brain --help` is the canonical option list. This page groups the main workflows.

## TUI and headless runtime

```bash
coding-brain
coding-brain --theme dark
coding-brain --headless
coding-brain --headless --json
```

The default command opens the Live, Review, Scorecard, and Diagnostics tabs. `--headless` keeps evaluation and context-rot prevention active without taking over a terminal; activity remains visible to a Brain TUI running elsewhere.

Live rows lead with a compact condition badge and bold project name, followed by the provider, action, and an occurrence count when needed. `j`/`k` and the arrow keys move within the selected Needs Attention or Recent list, while `J`/`K` switches lists and restores each list's last valid selection. Enter switches to the exact source of the selected activity. Coding Brain may use provider-qualified Agent Deck navigation, native `claude attach` for an exact background identity, or terminal focus. It does not expose a session list, terminate sessions, route work, or spawn workers.

At 120 columns and wider, Live keeps the selected activity's Evidence beside the stacked lists. Narrower terminals keep all three panes vertical and bound Evidence to its content, up to 12 rows. Evidence presents status and outcome before action and context; use PageUp and PageDown when `↑ more` or `↓ more` appears in its title.

Press `x` in Live to enter one-shot action mode. The next key is `a` (allow), `d` (deny), `c` (continue), or `t` (bounded hidden literal text, sent with Enter). Escape cancels. Semantic actions require recognized prompt evidence and exact current authority; manual text still requires an operator-selected exact live target. Outside action mode, `c` keeps correction behavior and Enter keeps navigation behavior. Review, Scorecard, and Diagnostics do not dispatch session actions.

Diagnostics is a read-only viewer for metadata-only hook and correlation diagnostics, not failed commands. Use `j`/`k` or the arrow keys to select recent diagnostic events. It displays Store integrity counters for malformed rows, duplicate terminal states, truncated tails, and discarded tail bytes, together with neutral `Diagnostic` status and Evidence for the selected event. Visible control characters are escaped before display. At 120 columns and wider, the event list and Evidence appear side by side; in narrower terminals they stack, and PageUp/PageDown scroll Evidence when its `↑ more` or `↓ more` title indicators appear. The raw audit rows remain in `$XDG_STATE_HOME/coding-brain/activity.jsonl`.

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

The `Review` and `Scorecard` TUI tabs are the primary surfaces. These commands expose the same records for scripts, focused reports, or markdown output.

## Setup and health

```bash
coding-brain init
coding-brain init codex
coding-brain init claude antigravity
coding-brain init all
coding-brain init --check
coding-brain init --upgrade
coding-brain init --remove
coding-brain init --purge
coding-brain doctor [--json]
coding-brain completions <shell>
coding-brain man
```

- Bare interactive `init` detects provider executables and asks which providers to configure. Detected providers are selected by default, but any listed provider can be selected for later installation.
- Positional selectors are `codex`, `claude`, `antigravity`, and exclusive shorthand `all`. Multiple provider names are accepted and deduplicated; `all` cannot be combined with another selector.
- Explicit selectors skip the provider picker and run the normal provider-neutral Brain onboarding.
- New non-interactive setup must name a provider, such as `coding-brain init claude --non-interactive`. Provider-less `--non-interactive` is a deprecated Codex-only compatibility path.
- `--plugin-only` is a deprecated Codex-only alias for `coding-brain init codex`.
- `--check` compares onboarding records with current state.
- `--upgrade` refreshes the installed or drifted providers recorded by prior onboarding and updates the marker version.
- `--remove` removes all exact Coding Brain-managed provider hooks and the onboarding marker but preserves data and unrelated entries.
- `--purge` additionally removes the previewed current and legacy global config/state targets after confirmation. It is irreversible.
- `doctor` checks the executable, hook definitions, affected provider compatibility, trust visibility, project identity, lifecycle state, outcome telemetry, endpoint privacy, transcript discovery, and terminal integration.
- `doctor` emits one setup row for Codex, Claude, and Antigravity, plus separate compatibility, Agent Deck navigation, Claude native attach, guarded semantic input, and focus-only fallback rows. With current managed Antigravity hooks, exact `agy` 1.1.5 produces an `Antigravity hook contract` advisory because that version may retain the native prompt after a valid hook decision. Other versions remain unverified. An unselected absent provider is skipped, while a selected provider with a missing executable is advisory. For invalid or stale declaratively managed hooks, rebuild Home Manager and restart the affected provider; for Codex, also inspect `/hooks` before rerunning `coding-brain doctor`. For imperatively managed providers, run the `coding-brain init <provider>` repair command shown in the provider row.

Managed setup paths are:

| Provider | Managed path |
| --- | --- |
| Codex | project `.codex/hooks.json` or user `~/.codex/hooks.json` |
| Claude Code | `~/.claude/settings.json` |
| Antigravity CLI | `~/.gemini/config/hooks.json` |

Multi-provider init parses, validates, and stages the complete selected set before replacement. It preserves unrelated and user-modified former managed entries. Its crash recovery uses recorded file evidence and does not overwrite a file changed concurrently after staging.

## Provider capabilities

| Capability | Codex | Claude Code | Antigravity CLI |
| --- | --- | --- | --- |
| Structured discovery | Rollout JSONL joined to live process evidence | Bounded `claude agents --json`, with process fallback | No external inventory; process discovery plus hook correlation |
| Lifecycle hooks | Session, prompt, tool, subagent, and Stop events | Session, prompt, tool, subagent, and Stop events | Tool, invocation, and Stop events |
| Permission guard | `PermissionRequest` allow/deny response | `PermissionRequest` allow/deny response; provider deny/ask policy remains authoritative | `PreToolUse` returns `allow` or `deny`; abstention and unsafe input return `ask` |
| Stop continuation | Recovery hook can send guarded terminal `continue` in `auto` mode | Recovery hook can send guarded terminal `continue` in `auto` mode | `Stop` returns structured `continue` only after a validated automatic decision |
| Native attach | Unavailable | Exact background identity via `claude attach` | Unavailable |
| Terminal focus | Exact supported terminal target; optional Agent Deck | Exact supported terminal target; optional Agent Deck | Exact supported terminal target; optional Agent Deck |
| Guarded input | Semantic allow/deny/continue and manual literal text through an exact tmux binding | Semantic allow/deny/continue and manual literal text through an exact tmux binding | Structured hooks first; guarded tmux for process-only, manual, or uncovered prompts |
| Transcript context | Codex rollout JSONL | Unavailable: the hook transcript path is retained as lifecycle identity/status evidence, but records are not parsed into `AgentSession` context | Unavailable: the hook transcript path is retained as lifecycle identity/status evidence, but records are not parsed into `AgentSession` context; SQLite is not read |
| Usage/cost surface | Unsupported; this provider feature adds no ingestion or dashboard/view | Unsupported; this provider feature adds no ingestion or dashboard/view | Unsupported; this provider feature adds no ingestion or dashboard/view |

Automatic terminal input revalidates provider process identity, a unique pane, a versioned prompt fingerprint, and pending request evidence immediately before input, then verifies that the prompt cleared or advanced. A mismatch leaves the activity for manual attention. Terminal focus alone never grants input authority.

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

Coding Brain keeps immediate judgment, learning evidence, review, recovery, and navigation local. It is Brain activity rather than a general session dashboard. Usage/cost tracking is outside the supported product surface; this provider feature adds no usage/cost ingestion or dashboard/view. Coding Brain has no durable task queue, dependency executor, distributed peer transport, or embedded project tracker. Beads and Agent Deck are optional companion tools for different jobs.
