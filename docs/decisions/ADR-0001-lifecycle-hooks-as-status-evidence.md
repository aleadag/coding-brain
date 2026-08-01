# ADR-0001: Treat Codex Lifecycle Hooks as Status Evidence

- Status: Accepted
- Date: 2026-07-17
- Bead: `codexctl-rqm`

## Context

Transcript discovery gives codexctl durable telemetry, but transcript writes can
lag the Codex session state that an operator needs to see. Codex lifecycle hooks
provide earlier signals for prompts, tool execution, permission requests,
subagents, and stops. Those payloads can also contain commands, prompts, tool
inputs, and other data that must not become a second authorization or telemetry
channel.

The existing permission hook has a narrower responsibility: the brain may make
confident allow or deny decisions for Bash commands. Expanding lifecycle status
coverage must not silently expand that authority.

## Decision

Codexctl will consume lifecycle hooks as a bounded, status-only overlay:

- A core lifecycle projection stores validated identity, event kind, ordering,
  receipt time, and bounded diagnostic state. It never stores prompt, command,
  tool input, tool output, or raw rejected values.
- Hook state is derivative. Writers use a short advisory lock and atomic file
  replacement; consumers fall back to transcript and process evidence when the
  state is missing, invalid, newer than the supported schema, or expired.
- Process death and explicit approval or `request_user_input` evidence outrank
  hook status. Only strictly newer, non-future transcript timestamps may
  invalidate fresh hook evidence.
- `PermissionRequest` observes every tool for status. Brain inference and
  allow/deny responses remain Bash-only; non-Bash requests record
  `NeedsInput` and emit no decision.
- Lifecycle evidence cannot populate pending-tool identity, approval evidence,
  terminal targets, rule inputs, or brain authorization inputs.
- Managed hook definitions and Codex trust are separate diagnostics. Codexctl
  can verify its JSON definitions, but the operator must review trust in Codex
  with `/hooks`.
- **Superseded by [ADR-0002](ADR-0002-coding-brain-product-boundary.md):**
  the accepted hook-evidence design originally retained the compatibility state
  root under `~/.codexctl`; the Coding Brain cutover now writes lifecycle state
  below `XDG_STATE_HOME/coding-brain` without automatic migration.

### 2026-08-01 clarification: transaction-bound permission authority

General lifecycle events remain the bounded, status-only evidence described
above. Lifecycle schema v4 also contains a separate permission-decision map
whose executable authority is bound to the exact request, transaction ID, and
`Allow` or `Deny` action. A bare legacy `Decided` value from schema v3 remains
useful for diagnostics and duplicate suppression but cannot authorize a
permission response.

This separate authority record does not make arbitrary lifecycle payloads
actionable. Permission hooks still derive policy inputs independently and emit
a model decision only after the exact authority, proposal, and terminal
activity agree. Corrupt or newer lifecycle authority evidence is preserved for
startup and Doctor handling; a permission hook does not quarantine, replace,
initialize, or write fallback state against it.

The detailed event model, leases, storage bounds, rollout behavior, and test
matrix live in the [approved design](https://github.com/aleadag/codexctl/blob/main/.internal/specs/2026-07-17-codex-lifecycle-hook-status-design.md)
and [implementation plan](https://github.com/aleadag/codexctl/blob/main/.internal/plans/2026-07-17-codex-lifecycle-hook-status.md).

## Rationale

Hooks improve status freshness without replacing the transcript, which remains
the durable source for telemetry and semantic correction. Keeping lifecycle
data non-actionable limits the blast radius of malformed, stale, or spoofed
hook input. Preserving the Bash-only authorization boundary also lets status
coverage expand independently from the higher-risk question of which tools the
brain may approve.

A bounded snapshot is simpler than a new daemon or event database. It supports
short-lived hook processes, cross-process updates, and one dashboard read per
refresh while remaining disposable after corruption or data loss.

The disposable-status rationale applies to the general lifecycle overlay. The
schema-v4 permission authority map has a stronger fail-closed persistence
contract because it participates in deciding whether a response may leave the
hook process.

## Consequences

- Session status can update before the corresponding transcript line is
  visible, with provenance exposed in JSON and the TUI.
- The implementation must maintain a validated state machine, cross-process
  locking, leases, transcript reconciliation, and exact hook installation and
  removal tests.
- General lifecycle-hook input or status-persistence failures fail open for
  Codex operation and never create an authorization response. Status may
  temporarily fall back to the existing transcript, CPU, and process
  heuristics. Permission-authority failures instead fail closed as described
  above.
- Lifecycle snapshots use private same-directory atomic replacement with durable
  file and parent-directory sync on Unix. General status remains reconstructable,
  while unreadable permission authority fails closed.
- Broader non-Bash authorization is deferred to `codexctl-85x`. XDG state
  migration is deferred to `codexctl-2yk`.
