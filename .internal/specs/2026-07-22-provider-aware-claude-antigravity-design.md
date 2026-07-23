# Provider-aware Claude Code and Antigravity support

> Date: 2026-07-22
> Feature: `codexctl-jye`
> Research: [Claude Code and Antigravity session capabilities](../research/2026-07-22-claude-code-antigravity-session-capabilities.md)

## Purpose

Coding Brain must evaluate and learn from Codex, Claude Code, and Antigravity CLI activity without becoming a session manager. Provider integration supplies the Brain with trustworthy identity, lifecycle, permission, outcome, and navigation evidence. Live remains an attention-first view over persisted Brain activity; it does not become a session dashboard.

This feature establishes a provider-neutral internal session model, installs provider-specific hooks, exposes provider-specific setup health, and adds guarded terminal actuation while preserving existing Codex behavior.

## Product boundary

The accepted Brain product boundary remains authoritative:

- Live, Review, and Scorecard show Brain activity, teaching evidence, and decision quality.
- Provider discovery is internal evidence. There is no public session list, session-management API, or session usage view.
- The operator can navigate from a Brain activity to its source agent session.
- Usage, token accounting, quota, and cost tracking are not collected or displayed. The provider capability matrix documents that exclusion rather than implementing it.
- Coding Brain does not add a daemon. Hook processes persist activity whether or not the TUI is open.

The original feature wording that sessions "appear in Live" means that activities from each provider appear in Live with explicit provider identity and an opaque navigation target. It does not authorize a general session pane.

## Goals

1. Represent Codex, Claude Code, and Antigravity explicitly in internal session and activity identity.
2. Rename the generic internal session record from `CodexSession` to `AgentSession`.
3. Preserve current Codex discovery, hook, status-inference, permission, and navigation behavior.
4. Prefer each provider's structured hooks, inventory, and transcript paths; degrade to bounded process or terminal evidence when structured data is unavailable.
5. Install managed provider hooks without overwriting unrelated user configuration.
6. Let users choose providers explicitly during `coding-brain init`.
7. Report provider-specific setup and degraded states through `coding-brain doctor`.
8. Support automatic and manual permission and recovery actions through the strongest safe delivery mechanism each provider exposes.

## Non-goals

- A session dashboard, process manager, launcher, messenger, router, or terminator.
- Usage, context-window, quota, pricing, burn-rate, or cost UI.
- Parsing Antigravity's undocumented SQLite conversation schema.
- Treating arbitrary terminal text, cwd, or a display name as sufficient authority for automatic input.
- General terminal-screen understanding. Automatic terminal actions recognize only versioned provider-specific prompts and exact live process/pane bindings.
- A provider trait framework. Three plain provider modules are sufficient.
- Renaming persistent Coding Brain paths or rewriting raw history logs in place.

## User-facing initialization

The normal init command accepts one or more positional provider selectors:

```text
coding-brain init codex
coding-brain init claude
coding-brain init antigravity
coding-brain init codex claude
coding-brain init all
```

An explicit selector skips provider selection and then runs the normal provider-neutral Brain onboarding. Bare `coding-brain init` detects provider executables and interactively asks which providers to configure; detected providers are selected by default, but the user may select an installed-later provider too.

At least one provider must be selected. `all` is shorthand for all three providers and cannot be combined with another selector.

New automation using `coding-brain init --non-interactive` must provide at least one explicit provider or `all`. For compatibility, the existing provider-less form remains Codex-only for one release and prints a deprecation warning with `coding-brain init codex --non-interactive` as its replacement. `--plugin-only` likewise remains as a deprecated Codex-hook-only alias for one release. Existing lifecycle tests continue to cover both compatibility paths.

The administrative modes keep their current broad semantics:

- `init --check` compares every provider recorded in the onboarding marker with current setup.
- `init --upgrade` reapplies the providers recorded by the previous successful init.
- `init --remove` removes every Coding Brain-managed provider entry and leaves unrelated entries intact.
- `init --reset` and `init --purge` remain provider-independent.

Provider selectors conflict with those administrative modes. Targeted removal can be designed separately if needed.

## Core model

### Provider identity

Add a stable enum in `coding-brain-core`:

```rust
pub enum AgentProvider {
    Codex,
    Claude,
    Antigravity,
}
```

It has stable lowercase serialization and a short display label. Provider identity comes from the adapter selected by Coding Brain, never from an untrusted hook payload.

Rename `CodexSession` to `AgentSession` throughout the workspace. This is a mechanical public Rust rename in the same feature; it must not alter behavior by itself. `RawSession` becomes `RawAgentSession` and carries `provider` explicitly.

Every internal join uses a provider-qualified key:

```rust
pub struct AgentSessionKey {
    pub provider: AgentProvider,
    pub session_id: String,
}
```

Native session IDs remain opaque. When a provider ID is unavailable, the synthetic live identity encodes provider, PID, process start time, and TTY. PID alone is not stable enough because it can be reused. A later hook-provided native ID is linked to that live identity without rewriting earlier raw history. Provider qualification prevents two vendors' UUIDs or process identities from colliding in lifecycle, activity, review, or navigation state.

An automatic terminal action may use either a native provider identity or this live process identity. The latter expires as soon as the PID, process start time, or TTY no longer matches. Cwd and display name are never identity evidence.

### Capabilities

`AgentSession` carries explicit provider and evidence availability. Capability checks are narrow methods derived from the provider and current evidence, not a configurable plugin system. The relevant capabilities are:

- structured discovery;
- lifecycle evidence;
- transcript context;
- permission observation;
- executable permission response;
- outcome evidence;
- native attach;
- terminal focus fallback;
- guarded terminal input;
- guarded recovery response.

Usage and cost are intentionally absent from this capability type. Existing legacy fields may be removed in a separate cleanup, but this feature neither expands nor surfaces them.

### Activity and navigation projection

Persisted Brain activity records the provider-qualified session key. `SessionTarget` replaces provider hints with explicit `provider`, opaque native ID, cwd, and only the supplemental navigation evidence required by the runtime.

Live and Review show a concise provider label on provider-backed activity. They do not receive or render a collection of `AgentSession` values. Pressing Enter resolves the selected activity's opaque target through Agent Deck, provider-native attach when supported, or terminal focus fallback.

## Provider adapters

Provider-specific behavior lives in plain modules under `coding-brain-core`; the binary owns hook installation and hook-response policy.

### Codex

Codex remains the regression baseline:

- process discovery recognizes `codex` and `.codex-wrapped`;
- rollout discovery reads `~/.codex/sessions/**/rollout-*.jsonl`;
- current lifecycle and PermissionRequest hooks keep their behavior;
- transcript/status inference keeps its existing transition and retention rules;
- the PermissionRequest hook remains the structured permission guard;
- guarded terminal approval keeps exact backend, target, fingerprint, tool, and command revalidation for explicit fallback;
- provider-specific recovery prompts use a structured response where available, otherwise guarded terminal input;
- Agent Deck and terminal focus navigation remain available.

The mechanical type rename must pass existing Codex tests before another provider behavior is added.

### Claude Code

Claude discovery first runs `claude agents --json` with a two-second timeout and a one-megabyte output bound. Supported entries identify live interactive and background sessions, cwd, start time, optional PID/status, optional full session UUID, and a background-only attach ID. Command absence, unsupported flags, timeout, malformed JSON, or oversized output triggers process fallback rather than a hard failure.

Process fallback recognizes the `claude` executable, records PID/cwd/TTY/start time, assigns a process identity when no structured UUID exists, and exposes status as unknown. It does not infer attention, permission, or cost from CPU or arbitrary terminal contents.

Managed Claude hooks supply `session_id`, `transcript_path`, `cwd`, permission mode, lifecycle events, tool identity, outcomes, and PermissionRequest input. The adapter accepts only documented, bounded fields. It may read the existing generic JSONL subset for recent Brain context, but unknown transcript records are ignored and never promoted to permission evidence.

Claude PermissionRequest responses use Claude's documented synchronous response schema. Allow and deny require an exact provider-qualified identity and current tool request. An adapter error or Brain abstention returns no decision so Claude shows its native prompt. A Coding Brain allow cannot override Claude deny or ask policy.

Claude recovery prompts use guarded terminal input because the PermissionRequest response schema does not answer non-permission continuation prompts. Automatic recovery requires a recognized Claude prompt plus an exact live process/pane binding; manual input requires an operator-selected activity and the same exact binding.

Background sessions with an attach ID use `claude attach <id>` as a provider-native navigation plan. Foreground sessions and older versions use terminal focus fallback.

### Antigravity CLI

Process fallback recognizes the documented `agy` executable and records PID/cwd/TTY/start time with unknown status. Antigravity has no documented external live-session inventory equivalent to Claude's JSON command. A process-only Antigravity session remains actionable through its expiring live process identity.

Managed hooks read the camelCase Antigravity contract: `conversationId`, workspace paths, `transcriptPath`, artifact directory, tool/invocation fields, and stop state. The provider-qualified conversation ID is authoritative for activity correlation. The adapter captures a bounded local parent-process chain to link that identity to PID, process start time, and TTY.

Coding Brain records Antigravity `PreToolUse`, `PostToolUse`, invocation, and stop events as lifecycle evidence. `PreToolUse` is also the primary synchronous permission guard: Brain returns `allow` or `deny` when executable, and abstention returns `ask` so Antigravity retains the native prompt. `force_ask` and permission overrides are parsed but never emitted unless a later design assigns them explicit Brain semantics. `Stop` returns `continue` when Brain has a validated recovery decision; otherwise it allows the stop.

Guarded tmux remains available for process-only sessions, explicit manual text, and recognizable provider prompts with no structured hook response. Automatic fallback input requires all of the following:

1. A provider-native or live process identity whose PID, start time, and TTY still match.
2. Exactly one tmux pane whose process ancestry contains that `agy` process.
3. A complete, versioned Antigravity prompt pattern matching the semantic action.
4. An immediate second capture with the same backend, pane, prompt fingerprint, and pending tool identity before injection.
5. A bounded post-action capture showing that the prompt cleared or advanced.

Any mismatch cancels the action and leaves an attention item. Manual free-form input does not require a recognized prompt, but it does require an explicit operator action and the same exact process/pane binding. It never becomes an automatic policy action. Structured hook delivery is always preferred over terminal injection when both are available.

The hook-provided JSONL path may supply bounded recent context if its record shape matches the supported generic parser. Coding Brain does not scan or open Antigravity SQLite conversation databases. Navigation uses Agent Deck when an exact provider match exists, then terminal focus fallback; `agy --conversation` resumes conversation state and is not treated as a way to focus the original terminal.

### Discovery cadence

Hook events update Brain state immediately. Lightweight process discovery retains the monitor's existing cadence. Claude CLI inventory is cached for five seconds, and transcript indexing retains its existing ten-second cache. Provider commands run with their documented timeout and output bounds outside the TUI render path. A slow or failed refresh keeps timestamped stale data until fallback evidence disproves it; it does not block the TUI or clear a live session.

## Managed hook setup

Provider configuration paths are separate and merged by provider-specific installers:

| Provider | Managed path | Setup |
| --- | --- | --- |
| Codex | `~/.codex/hooks.json` or project `.codex/hooks.json` | Existing managed lifecycle and PermissionRequest definitions |
| Claude | `~/.claude/settings.json` or project `.claude/settings.json` | Managed lifecycle and PermissionRequest definitions in Claude's `hooks` object |
| Antigravity | `~/.gemini/config/hooks.json` | Managed tool, invocation, and stop definitions |

Before replacing any provider file, init parses, validates, and stages the complete selected-provider change set. It writes an owner-only recovery journal containing the original file evidence and replacement hashes, then atomically replaces each target. If a later replacement fails or a subsequent init or doctor run finds an interrupted transaction, recovery restores a replaced file only while its current hash still matches the recorded replacement. Concurrent user edits are preserved. The journal is removed after the transaction succeeds or safe recovery finishes, so successful init leaves no backup-file clutter.

Each installer:

1. Parses the existing file and refuses to overwrite invalid or wrong-shaped data.
2. Replaces only exact Coding Brain-owned commands for that provider.
3. Preserves unrelated events, matchers, handlers, disabled state, and surrounding settings.
4. Writes a complete sibling file, flushes it, and atomically replaces the target.
5. Is idempotent: a second init produces no content change.
6. Removes only exact managed entries during uninstall.
7. Preserves a formerly managed entry that the user modified and reports it instead of deleting or overwriting it silently.

Init does not replace or wrap Claude or Antigravity status-line/title commands.

Hook commands select their adapter explicitly, for example with an internal `--provider codex|claude|antigravity` argument. Payload fields cannot switch providers. Every adapter caps stdin at 64 KiB and rejects malformed, newer-unsupported, or identity-less actionable input without emitting an allow.

The onboarding marker records stable provider phase keys (`hooks.codex`, `hooks.claude`, `hooks.antigravity`). A legacy installed `plugin` phase is interpreted as recorded Codex setup when reading an older marker; new writes use the provider keys.

## Stored-state compatibility

Provider identity is additive in persisted activity, decision, and navigation records. Deserialization defaults a missing provider to Codex because all pre-feature records are Codex-only. Derived projection schema versions are bumped and rebuilt from the raw append-only logs. Raw history is never rewritten in place, and existing storage paths remain unchanged.

## Doctor behavior

`coding-brain doctor` renders one setup row per provider:

- **current**: executable and managed definitions are present and current;
- **degraded**: live process fallback works, but structured hooks or supported inventory are unavailable;
- **stale**: Coding Brain-owned definitions exist but differ from the running binary's expected commands;
- **unavailable**: a selected/recorded provider executable or managed command cannot run;
- **skipped**: the provider was never selected and its executable is absent.

Provider absence is not a global failure when the provider was never selected. A recorded provider whose executable disappeared is an advisory; broken or unsafe managed definitions are failures. Doctor messages name the exact provider and replacement init command.

Session discovery diagnostics report counts by provider without exposing a public session-list command. Terminal diagnostics distinguish Agent Deck, provider-native Claude attach, guarded input, and focus-only fallback.

## Failure and security behavior

- Structured provider data is preferred over transcript data; transcript data is preferred over process-only evidence.
- Missing structured evidence produces explicit unknown or degraded state. Zero is never substituted for unknown telemetry.
- Hook input, transcript text, tool input, cwd, provider output, and terminal text are untrusted data.
- No provider adapter shells through an interpolated command string. External commands use argument arrays, cleared or bounded environments where appropriate, timeouts, and output caps.
- Hook failures abstain. They do not turn into implicit approval.
- Codex, Claude, and Antigravity use their structured permission hooks as the primary permission guards. Antigravity also uses its structured `Stop` continuation response.
- Automatic terminal input requires an exact provider-native or expiring process identity, a unique pane, a recognized semantic prompt, immediate recapture, and post-action verification. Unknown prompts may be focused or answered manually but never acted on automatically.
- Recovery responses use structured hooks where supported and the guarded terminal protocol otherwise.
- Lifecycle and outcome joins require exact provider-qualified session and tool identity. Cwd, display name, and command text are never authorization fallbacks; process start time is used only as part of the complete live process identity.
- Navigation errors are bounded and redacted before reaching the TUI.

## Testing strategy

Implementation follows red-green-refactor in this order:

1. Mechanical `AgentSession` rename with the full existing suite unchanged.
2. Provider identity, process-identity expiry/linking, legacy-Codex defaulting, and collision tests across lifecycle, activity, review, and navigation keys.
3. Claude JSON inventory fixtures for interactive/background sessions, malformed output, timeout, unsupported version, and process fallback.
4. Antigravity process fixtures and camelCase hook fixtures.
5. Provider hook-adapter tests for input bounds, malformed input, abstention, Codex and Claude permission guards, Claude allow/deny schema, Antigravity `allow`/`deny`/`ask`, and Antigravity Stop `continue`.
6. Guarded terminal tests for `Allow`, `Deny`, `Continue`, and manual text; native and process-only identity; changed PID/start time/TTY; ambiguous panes; incomplete, changed, or unknown prompts; injection failure; and post-action verification.
7. Installer transaction tests for empty files, unrelated entries, duplicate scopes, stale or user-modified managed entries, invalid JSON refusal, idempotence, partial-write rollback, and exact removal.
8. Init CLI tests for one provider, several providers, `all`, bare interactive selection, legacy Codex-only non-interactive behavior and warning, administrative conflicts, and deprecated `--plugin-only` behavior.
9. Discovery-cache tests for immediate hook updates, five-second CLI refresh, ten-second transcript refresh, timeout, stale retention, and non-blocking TUI behavior.
10. Doctor tests for current, degraded, stale, unavailable, and skipped states per provider.
11. TUI tests that provider labels appear on activities without adding a session/usage/cost pane.
12. Navigation tests for exact provider matching, Claude background attach, foreground terminal fallback, Antigravity focus/input fallback, ambiguity, and missing tools.
13. Existing Codex discovery, lifecycle, permission, status-inference, transcript, and guarded-terminal regressions.

The final quality gates are:

```text
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
```

## Documentation

README, quickstart, configuration, reference, troubleshooting, terminal support, and Home Manager documentation must describe:

- supported providers and the provider selectors accepted by `init`;
- the Brain-only product boundary;
- managed configuration paths and preservation behavior;
- the provider capability/degradation matrix;
- Claude background attach versus terminal focus fallback;
- structured permission guards for all providers, including Antigravity `PreToolUse` and `Stop` responses;
- automatic versus manual recovery input and the exact-target safety boundary;
- the explicit exclusion of usage and cost tracking.

## Acceptance criteria

The feature is complete when:

- `AgentProvider` and provider-qualified identity are used everywhere session identity crosses a persistence or navigation boundary;
- the workspace uses `AgentSession` instead of `CodexSession`;
- persisted Codex, Claude Code, and Antigravity activity retains explicit provider identity when projected into Live, Review, and Scorecard;
- activities can navigate to their source through exact Agent Deck matching, supported provider attach, or bounded terminal focus fallback;
- all three providers use structured permission guards where available; Antigravity uses `PreToolUse` allow/deny and `Stop` continue; and guarded terminal fallback covers process-only, manual, and unsupported-prompt actions;
- native and process-only live identities can target exactly one verified terminal pane without relying on cwd;
- `coding-brain init` supports explicit provider selectors and interactive selection, while legacy provider-less non-interactive init remains Codex-only with a one-release deprecation warning;
- managed hook setup is transactional, atomic per file, idempotent, preserves unrelated provider configuration, and rolls back partial multi-provider writes;
- `doctor` reports current and degraded setup per provider;
- unknown capabilities remain unknown, and automatic terminal action requires a recognized prompt plus exact process and pane revalidation;
- old persisted records default to Codex, derived projections rebuild, and raw history remains untouched;
- no session dashboard or usage/cost surface is introduced;
- existing Codex behavior remains regression-covered; and
- all workspace quality gates pass.

## Stress test results

The design was stress-tested with the user on 2026-07-22. All nine branches were resolved:

| Branch | Decision | Design consequence |
| --- | --- | --- |
| Scope contract | Brain activity, not sessions, appears in Live; usage/cost is out | Acceptance criteria now match ADR-0002 |
| Type migration | Isolate the `AgentSession` rename before behavior changes | Existing tests must pass at the rename checkpoint |
| Permission and recovery authority | Hooks guard all provider permissions; Antigravity also exposes Stop continue; guarded terminal input covers process-only, manual, and unsupported prompts | Structured delivery is primary and terminal input remains a bounded fallback |
| Init compatibility | Preserve provider-less non-interactive init as deprecated Codex-only behavior for one release | Existing automation does not break immediately |
| Config rollback | Stage all selected providers and roll back partial replacement | Multi-provider init is transactional |
| Provider schema drift | Tolerant bounded parsing with process/hook/transcript fallback | Schema changes degrade capabilities without hiding sessions |
| Exact action targeting | Native identity or expiring provider/PID/start-time/TTY identity may authorize a unique pane | Process-only sessions remain actionable without cwd heuristics |
| Polling scale | Immediate hooks, five-second CLI cache, ten-second transcript cache, stale retention | Provider discovery cannot block or flicker the TUI |
| Existing history | Missing provider means Codex; rebuild projections; never rewrite raw logs | Existing Brain evidence remains readable and auditable |

Post-test reflection: the highest-risk fallback is race-free terminal actuation, but current Antigravity documentation removes it from the normal permission and continuation path. Implementation must prefer structured `PreToolUse` and `Stop` responses, while terminal fallback keeps semantic actions provider-specific, refuses ambiguity, revalidates immediately before input, and proves failure behavior with fixtures. The process-only identity is deliberately live and expiring; it must not become a substitute for durable provider identity in historical learning joins.
