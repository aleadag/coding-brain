# ADR-0004: Use Provider-Aware Guards and Terminal Actuation

- Status: Accepted
- Date: 2026-07-22
- Bead: `codexctl-jye`

## Context

Coding Brain must evaluate Codex, Claude Code, and Antigravity CLI activity.
All three providers expose structured permission hooks, but their response
schemas differ. Antigravity `PreToolUse` accepts `allow`, `deny`, `ask`, or
`force_ask`, and its `Stop` hook can return `continue`. A provider may still run
without managed hooks or stop at a prompt outside those response contracts; in
tmux, Coding Brain can identify, capture, and control that pane.

Focusing that pane is not enough. A local supervisor must be able to deliver an
allow or deny decision, and agents sometimes stop at non-permission recovery
prompts such as `continue`. Raw terminal injection is dangerous, however: the
process may have exited, a PID may have been reused, or the pane may have moved
to a shell or editor between observation and input.

Provider integrations also need stable correlation. Native session IDs are not
always available during process discovery, and IDs from different providers
can collide. Existing persisted records have no provider because every old
record came from Codex.

The approved provider design and its nine-branch stress test are recorded in
the [provider-aware design](https://github.com/aleadag/coding-brain/blob/main/.internal/specs/2026-07-22-provider-aware-claude-antigravity-design.md).

## Decision

### Qualify every session identity by provider

The shared session record becomes `AgentSession` and carries
`AgentProvider::{Codex, Claude, Antigravity}`. Identity crossing a lifecycle,
persistence, activity, or navigation boundary includes the provider.

A native provider session ID is preferred. Until one exists, a live session
uses an expiring identity made from provider, PID, process start time, and TTY.
The identity stops being valid when any process component changes. A later
hook-provided native ID links to it without rewriting earlier raw history. Cwd
and display name are never sufficient identity evidence.

Persisted records with no provider deserialize as Codex. Derived projection
indexes receive a schema bump and rebuild from the append-only logs; raw
decision and activity history is not rewritten in place.

### Use the strongest permission guard each provider exposes

Codex, Claude, and Antigravity use their structured permission hooks to guard
allow and deny responses. Antigravity abstention returns `ask`, and its `Stop`
hook returns `continue` only for a validated Brain recovery decision. A
malformed request, identity mismatch, or unsupported request leaves the
provider's native behavior in control. General lifecycle hooks remain status evidence under
[ADR-0001](ADR-0001-lifecycle-hooks-as-status-evidence.md); this decision does
not turn arbitrary lifecycle payloads into authorization.

Antigravity `force_ask` and permission overrides are accepted as input but are
not emitted until Coding Brain assigns them explicit policy semantics.
Structured responses are preferred whenever a managed hook can deliver the
action.

Every provider may fall back to guarded terminal input for a process-only
session or a prompt outside its structured hook contract. Explicit manual text
input is also supported when the operator selects an activity with an exact
live target.

### Guard every automatic terminal action against races

An automatic terminal action runs only when Coding Brain can:

1. revalidate a native or live process identity;
2. map its TTY to exactly one pane whose process ancestry contains the provider
   process;
3. match a complete, versioned provider-specific prompt for the semantic
   action;
4. capture the pane again immediately before input and reproduce the backend,
   target, prompt fingerprint, and pending request identity; and
5. capture afterward and verify that the prompt cleared or advanced.

Any mismatch cancels the action and leaves an attention item. An unknown prompt
may be focused or answered through explicit manual input, but it never triggers
an automatic action. Manual text does not require prompt recognition, although
it still requires the same live process and unique-pane binding.

### Keep setup and discovery bounded

`coding-brain init` accepts explicit provider selectors and asks interactively
when none are supplied. Existing provider-less `init --non-interactive` remains
Codex-only for one release and prints a deprecation warning. Provider config
changes are validated and staged together, preserve unrelated entries, roll
back partial replacement, and leave user-modified managed entries untouched
with a warning.

Hooks update Brain state immediately. Process discovery follows the existing
monitor cadence, Claude CLI inventory is cached for five seconds, and transcript
indexing retains its ten-second cache. Failed provider refreshes preserve
timestamped stale data and degrade to bounded process, hook, or transcript
evidence instead of clearing a session or blocking the TUI.

Provider discovery remains internal. Live, Review, and Scorecard show
provider-tagged Brain activity with source-session navigation; they do not add
a session dashboard. Usage, token, quota, and cost tracking remain outside the
product.

## Rationale

Structured permission and continuation hooks avoid terminal races on the
normal path. Terminal input remains necessary for process-only sessions,
manual text, and prompts with no response API. Binding a semantic action to the
provider process, one pane, and one recognized prompt keeps that fallback
narrow enough to fail safely when the terminal changes.

The process identity makes a discovered session actionable before its first
hook without pretending that PID alone is durable. Defaulting old records to
Codex preserves existing history because no other provider wrote those records,
while rebuilding derived indexes avoids a risky raw-log migration.

## Consequences

- The mechanical `CodexSession` to `AgentSession` rename must pass the existing
  suite before provider behavior is added.
- Antigravity automatic permission and Stop continuation use structured hooks.
  Its terminal fallback requires a reachable tmux pane; unsupported or
  ambiguous terminals degrade to attention plus focus.
- Provider prompt patterns are versioned compatibility code. Tests must cover
  incomplete, changed, unknown, and raced prompts before automatic input is
  enabled.
- Process-only identities expire on process change and cannot become durable
  historical learning keys without a provider-ID link.
- Doctor must distinguish structured permission support, guarded input,
  focus-only fallback, stale managed hooks, and unavailable provider tools.
- Init and uninstall must preserve unrelated and user-modified provider config,
  including rollback after a partial multi-provider write.
- No session-management or usage/cost surface is introduced.
