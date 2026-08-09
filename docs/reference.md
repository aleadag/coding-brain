# Command reference

`cbrain --help` is the canonical option list. This page groups the main workflows.

## TUI and headless runtime

```bash
cbrain
cbrain --theme dark
cbrain --headless
cbrain --headless --json
```

The default command opens the Live, Review, Scorecard, and Diagnostics tabs. `--headless` keeps evaluation and context-rot prevention active without taking over a terminal; activity remains visible to a Brain TUI running elsewhere.

Live rows lead with a compact condition badge and bold project name, followed by the provider, action, and an occurrence count when needed. `j`/`k` and the arrow keys move within the selected Needs Attention or Recent list, while `J`/`K` switches lists and restores each list's last valid selection. Enter switches to the exact source of the selected activity. Coding Brain may use provider-qualified Agent Deck navigation, native `claude attach` for an exact background identity, or terminal focus. It does not expose a session list, terminate sessions, route work, or spawn workers.

At 120 columns and wider, Live keeps the selected activity's Evidence beside the stacked lists. Narrower terminals keep all three panes vertical and bound Evidence to its content, up to 12 rows. Evidence presents status and outcome before action and context. A Recent selection shows its relative age in seconds, minutes, hours, or days, while an actionable selection keeps the `Needs attention` label. Use PageUp and PageDown when `↑ more` or `↓ more` appears in the Evidence title.

`INCOMPLETE` with `permission evaluation timed out` means an observed or evaluating permission attempt became stale without terminal evidence. It remains in Needs Attention, but it is a projection only: Coding Brain does not persist an `Incomplete` activity row, and elapsed time does not prove that the hook died. It is distinct from an interrupted tool and does not prove that a response was delivered or a command executed.

Permission authority, terminal activity, and the matching decision are committed atomically in `brain.sqlite3`. A failed or uncertain commit is never response-eligible, and recovery never replays a provider response. Delivery and later outcome evidence remain separate from commitment.

Press `x` in Live to preflight the selected exact provider session, pane, and current prompt. Coding Brain exposes `a` (allow) and `d` (deny) only for a recognized permission prompt, `c` (continue) only for a recognized recovery prompt, and `t` for bounded hidden manual text after exact target and capture validation. Semantic dispatch independently rediscovers and revalidates the target and prompt, so changed prompt evidence is rejected without fallback input. Manual-text dispatch independently revalidates the exact target, backend, and bounded capture but does not require prompt equality. Escape cancels. Outside action mode, `c` keeps correction behavior and Enter keeps navigation behavior. Review, Scorecard, and Diagnostics do not dispatch session actions.

Outside Live action mode, itemized views share review lifecycle keys. Press `a` to review the selected NEW item (`seen` in Recent), `A` to confirm all visible NEW items, `d` to confirm archiving the selected reviewed item, `D` to confirm archiving every reviewed item in that surface, and `u` to restore the latest archive. Recent supports only `a` and `A`. In Review, `s` reviews the selected item and advances; `m` and `n` keep their canonical-marking behavior. These keys change surface-local visibility only. They do not delete activity or decision evidence, change Scorecard results, or authorize a session action. See [Review lifecycle state](configuration.md#review-lifecycle-state) for persistence, reset, and failure behavior.

Diagnostics is a metadata-only viewer for safe categories covering hook and correlation faults, rejected or uncertain session actions and recovery, and storage integrity. It is not ordinary command output and never stores captured terminal content or manual text. Use `j`/`k` or the arrow keys to select recent diagnostic events. Review and archive keys change only the Diagnostics projection; they do not modify authoritative audit rows in `brain.sqlite3`. Event rows identify the provider, project, and tool, while Evidence shows the selected event's metadata and reason. Visible control characters are escaped before display. At 120 columns and wider, the event list and Evidence appear side by side; in narrower terminals they stack, and PageUp/PageDown scroll Evidence when its `↑ more` or `↓ more` title indicators appear.

## Brain evaluation

```bash
cbrain config get mode
cbrain config set mode off|on|auto
cbrain --url <endpoint> --brain-model <model>
cbrain --brain-query --tool Bash --tool-input "cargo test"
```

The mode is global, persists after the settings command exits, and defaults to `off` on a new install. `off` disables model evaluation, `on` enables advisory evaluation, and `auto` allows high-confidence automatic decisions. Deterministic safety checks and lifecycle recording remain active in every mode. `--brain-query` is the non-interactive permission-hook path and normally receives structured hook input rather than being typed manually.

## Learning and diagnostics

```bash
cbrain --brain-review [list]
cbrain --brain-mark-canonical <decision-id>
cbrain --brain-stats <report>
cbrain --insights [on|off|status]
cbrain --brain-garden [--apply]
cbrain --brain-briefing --project <name>
cbrain --autopsy [--session <id>]
```

The `Review` and `Scorecard` TUI tabs are the primary surfaces. These commands expose the same records for scripts, focused reports, or markdown output.

## Storage operations

```bash
cbrain storage export-audit <directory>
cbrain storage export-legacy <directory>
cbrain storage reset-review-state
```

- `export-audit` writes a bounded, versioned archive whose manifest marks it non-executable. It preserves audit meaning but cannot recreate live permission authority.
- `export-legacy` writes the exact frozen v0.59.1 layout for a deliberate downgrade and verifies it with the frozen legacy readers. It is not a live mirror.
- `reset-review-state` replaces only `db/review.sqlite3`. Stop other Coding Brain processes first; an open review database makes the command return Busy.
- Storage subcommands require current SQLite storage but do not start migration themselves. Run an ordinary non-hook command such as `cbrain doctor` first after upgrading.

## Setup and health

```bash
cbrain init
cbrain init codex
cbrain init claude antigravity
cbrain init all
cbrain init --check
cbrain init --upgrade
cbrain init --remove
cbrain init --purge
cbrain doctor [--json]
cbrain completions <shell>
cbrain man
```

- Bare interactive `init` detects provider executables and asks which providers to configure. Detected providers are selected by default, but any listed provider can be selected for later installation.
- Positional selectors are `codex`, `claude`, `antigravity`, and exclusive shorthand `all`. Multiple provider names are accepted and deduplicated; `all` cannot be combined with another selector.
- Explicit selectors skip the provider picker and run the normal provider-neutral Brain onboarding.
- New non-interactive setup must name a provider, such as `cbrain init claude --non-interactive`. Provider-less `--non-interactive` is a deprecated Codex-only compatibility path.
- `--plugin-only` is a deprecated Codex-only alias for `cbrain init codex`.
- `--check` compares onboarding records with current state.
- `--upgrade` refreshes the installed or drifted providers recorded by prior onboarding and updates the marker version.
- `--remove` removes all exact Coding Brain-managed provider hooks and the onboarding marker but preserves data and unrelated entries.
- `--purge` additionally removes the previewed current and legacy global config/state targets after confirmation. It is irreversible.
- `doctor` checks the executable, hook definitions, affected provider compatibility, trust visibility, project identity, SQLite schema and migration status, WAL size, endpoint privacy, transcript discovery, and terminal integration.
- Routine Doctor storage checks are bounded and do not run `PRAGMA integrity_check`; human and JSON output therefore report integrity as `not_checked`. The deep integrity API is restricted to non-hook callers and is not currently exposed by the CLI.
- `doctor` emits one setup row for Codex, Claude, and Antigravity, plus separate compatibility, Agent Deck navigation, Claude native attach, guarded semantic input, and focus-only fallback rows. For a non-current setup, each resolved candidate appears in inspection order with its path, scope, ownership, state, and a bounded reason when applicable; the list is empty when no candidate path can be resolved, such as when `HOME` is unavailable. `doctor --json` returns the same records under `evidence.provider_files`. With current managed Antigravity hooks, exact `agy` 1.1.5 produces an `Antigravity hook contract` advisory because that version may retain the native prompt after a valid hook decision. Other versions remain unverified. An unselected absent provider is skipped, while a selected provider with a missing executable is advisory. For invalid or stale declaratively managed hooks, rebuild Home Manager and restart the affected provider; for Codex, also inspect `/hooks` before rerunning `cbrain doctor`. For imperatively managed providers, run the `cbrain init <provider>` repair command shown in the provider row.

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
| Context pressure | Bounded percentage from provider capacity or a known-model fallback | Unavailable | Unavailable |

Automatic terminal input revalidates provider process identity, a unique pane, a versioned prompt fingerprint, and pending request evidence immediately before input, then verifies that the prompt cleared or advanced. A mismatch leaves the activity for manual attention. Terminal focus alone never grants input authority.

## Configuration helpers

```bash
cbrain config show
cbrain config get mode
cbrain config set mode on
cbrain config template
cbrain config validate
cbrain config init
cbrain --hooks
```

Current config uses `.coding-brain.toml` and `$XDG_CONFIG_HOME/coding-brain/config.toml`. Old config and state are never read during ordinary operation.

## Product boundary

Coding Brain keeps immediate judgment, learning evidence, review, recovery, and navigation local. It is Brain activity rather than a general session dashboard. Coding Brain does not collect or display token usage or cost. Coding Brain may derive a bounded context-window percentage for context-rot prevention, but it does not retain the provider token counts used to derive it. Only that bounded percentage is retained. The percentage uses provider-supplied context capacity when available and otherwise a known-model fallback; it is not raw usage or cost accounting.

Coding Brain has no durable task queue, dependency executor, distributed peer transport, or embedded project tracker. Beads and Agent Deck are optional companion tools for different jobs.
