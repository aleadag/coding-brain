# Codex Lifecycle Hook Status Design

**Issue:** `codexctl-rqm`  
**Status:** Stress-tested; ready for implementation planning  
**Date:** 2026-07-17

## Problem

codexctl infers live Codex status from three indirect sources:

- process discovery and liveness;
- incremental transcript parsing; and
- terminal inspection for visible approval prompts.

Those sources remain necessary, but they can lag the actual Codex lifecycle or
be unavailable. Codex now exposes documented command hooks for session, turn,
tool, permission, and subagent events. A hook receives the canonical
`session_id`, optional `turn_id`, `transcript_path`, cwd, event name, and
event-specific fields on standard input.

codexctl previously installed `PostToolUse` and `Stop` handlers that ran
`codexctl --json`. Those handlers neither consumed their input nor persisted an
event, and their session-array output was not a valid Codex hook response. The
`codexctl-9am` cleanup removed them and deliberately retained only the native
`PermissionRequest` brain handler. This design introduces a valid lifecycle
integration rather than restoring the removed snapshot commands.

## Goals

- Reflect lifecycle changes immediately when a configured and trusted hook
  fires.
- Preserve transcripts for token, cost, tool identity, explicit
  `request_user_input`, and reconciliation.
- Preserve process discovery for PID liveness, `Finished`, and sessions without
  transcripts.
- Keep lifecycle hooks optional and fail safely when they are absent, disabled,
  untrusted, malformed, duplicated, delayed, or unable to persist state.
- Isolate sessions by canonical `session_id` and `turn_id`, including multiple
  sessions with the same cwd.
- Keep lifecycle observations outside every approval and authorization path.

## Non-Goals

- Do not replace transcript telemetry or process discovery.
- Do not make app-server events a requirement.
- Do not infer token usage, cost, pending tool identity, or approval evidence
  from lifecycle hook state.
- Do not automatically merge, push, deploy, or perform destructive migration.
- Do not move existing state from `~/.codexctl`; XDG state migration is tracked
  separately by `codexctl-2yk`.
- Do not change cost-estimate semantics; estimate explainability is tracked by
  `codexctl-iyk`.

## Official Hook Contract Used

Every installed command hook receives one JSON object on standard input. This
design uses the documented common fields:

- `session_id`;
- `turn_id` for turn-scoped events;
- `transcript_path` when available;
- `cwd`;
- `hook_event_name`; and
- `permission_mode` where supplied.

The generic lifecycle handler emits no standard output. The existing
`PermissionRequest` handler remains the only codexctl lifecycle command that
may emit a protocol response.

## Architecture

### Core lifecycle module

Add a focused module to `codexctl-core` that owns:

- validated lifecycle input and persisted-state types;
- an environment-only compatibility state-root resolver;
- the bounded state store and locking protocol;
- event-to-status projection;
- session and turn ordering rules; and
- reconciliation against transcript and process evidence.

The module does not depend on binary-only brain, config, initialization, or TUI
code. The binary can write observations through it, and the TUI can read and
apply them through the existing downward dependency flow.

The store accepts an explicit root through `LifecycleStore::at(root)`. A small
core-owned resolver supplies the current compatibility root to production
callers without reading binary configuration. Tests always inject a temporary
root. This keeps writer and reader path selection identical and provides the
single seam that `codexctl-2yk` will later migrate to XDG state paths.

### Generic lifecycle adapter

Add a hidden `codexctl --lifecycle-hook` mode. It:

1. reads at most 64 KiB containing exactly one JSON object from standard input;
2. validates the common and event-specific fields;
3. persists the projected observation;
4. writes diagnostics only to standard error; and
5. exits without standard output.

Invalid input or persistence failure must not block Codex. The adapter exits
successfully after reporting the diagnostic so existing observation paths can
take over.

Unknown JSON fields are accepted for forward compatibility. Validation rejects
unsupported event names and missing or invalid fields required by the selected
event; it does not reject a payload merely because a newer Codex version added
an unrelated field. Session, turn, and agent identifiers are limited to 512
UTF-8 bytes, while cwd and transcript paths are limited to 4 KiB. Oversized
input or fields are rejected before persistence without echoing the raw value.

### Permission adapter integration

Do not install a second `PermissionRequest` handler. Multiple matching command
hooks run concurrently, and a duplicate handler would complicate both trust and
status ordering.

Instead, change the existing `--permission-hook` matcher to `*` and give the
single adapter two deliberately separate responsibilities. It first validates
the common lifecycle identity for every official permission request. Then:

- a Bash request additionally validates `tool_input.command` and follows the
  existing brain decision path;
- a confident native Bash allow or deny records `Processing` because no human
  input is required;
- Bash abstention, disabled brain, inference failure, unsupported action, or a
  below-threshold result records `NeedsInput` immediately before returning no
  decision;
- a non-Bash request records `NeedsInput` and immediately returns no decision,
  without invoking the brain; and
- malformed common identity, or a malformed Bash command, records no lifecycle
  observation.

Lifecycle persistence is a best-effort status side effect. It never creates an
allow or deny and does not weaken the permission handler's existing audit and
response-ordering requirements. Brain authorization remains Bash-only; broader
tool authorization is tracked separately in `codexctl-85x`.

### Hook installation

Imperative initialization and the Home Manager module install these managed
events:

| Event | Handler | Matcher | Timeout |
| --- | --- | --- | --- |
| `SessionStart` | `codexctl --lifecycle-hook` | `startup|resume|clear|compact` | 2 seconds |
| `UserPromptSubmit` | `codexctl --lifecycle-hook` | omitted | 2 seconds |
| `PreToolUse` | `codexctl --lifecycle-hook` | `*` | 2 seconds |
| `PermissionRequest` | `codexctl --permission-hook` | `*` | 30 seconds |
| `PostToolUse` | `codexctl --lifecycle-hook` | `*` | 2 seconds |
| `SubagentStart` | `codexctl --lifecycle-hook` | `*` | 2 seconds |
| `SubagentStop` | `codexctl --lifecycle-hook` | `*` | 2 seconds |
| `Stop` | `codexctl --lifecycle-hook` | omitted | 2 seconds |

Initialization replaces only exact codexctl-managed handlers and preserves
unrelated hooks and matcher groups structurally. It continues to recognize the
removed `--json` lifecycle forms only for exact cleanup.

Installing or upgrading the binary alone never mutates hook definitions.
Lifecycle handlers appear only after explicit `codexctl init`,
`codexctl init --plugin-only`, or a Home Manager rebuild. `codexctl init
--remove` recognizes and removes the exact managed lifecycle and permission
commands without touching unrelated handlers.

`PreCompact` and `PostCompact` are not installed. `SessionStart` with source
`compact` provides the identity boundary this feature needs, while transcript
parsing remains responsible for compaction telemetry.

## Persisted State

### Location and permissions

Use the current compatibility state root:

```text
~/.codexctl/hooks/lifecycle.json
~/.codexctl/hooks/lifecycle.lock
```

The hooks state directory is created with mode `0700` and files with mode
`0600` on Unix. Hook payload values are stored only as JSON data; no payload
value is used to construct a path.

### Snapshot shape

The versioned snapshot contains:

- a schema version;
- the next receive sequence;
- at most 128 session entries;
- the current turn id and whether it is open or closed;
- at most 32 recently superseded or closed turn ids per session;
- the latest event, receive sequence, and receive timestamp;
- cwd and optional transcript identity;
- the projected status;
- at most 64 active subagents keyed by agent id; and
- enough event-specific metadata for diagnostics, but not raw prompts, tool
  inputs, command strings, or tool outputs.

The store intentionally excludes sensitive payload bodies that status
projection does not need.

### Atomic update protocol

Writers try to acquire an exclusive OS advisory lock on the stable lock file
for at most 100 milliseconds, read and validate the snapshot, apply one event,
write all serialized bytes to a sibling temporary file, and atomically rename
it over `lifecycle.json`. The separate lock inode is never replaced, so
concurrent hook processes cannot lose accepted updates during the snapshot
rename.

The hot hook path does not `fsync` the temporary file or parent directory.
Atomic replacement prevents readers from observing a partial snapshot;
durability across power loss is unnecessary because lifecycle state is
derivative and all consumers safely fall back to transcript and process
evidence.

Readers take a shared lock long enough to read and parse one complete snapshot.
They retain their last valid in-memory observation if a concurrent refresh
temporarily fails.

Entries older than 24 hours are pruned while a writer already holds the lock.
If the store still reaches 128 sessions, the writer removes the least recently
updated inactive session before accepting a new session. If every retained
session is active, the new-session event is ignored with a capacity diagnostic
instead of evicting active evidence. The dashboard ignores expired entries
even if no subsequent writer performs pruning.

Serialization is capped at 1 MiB after pruning. An update that would exceed the
cap is rejected without replacing the last valid snapshot.

If the existing snapshot uses a newer schema, codexctl preserves it unchanged,
treats the store as read-only, and ignores lifecycle evidence. Doctor reports
that the running binary is older than the state schema and must not mutate it.

If the existing snapshot claims the current schema but is corrupt, the next
writer quarantines it under the store lock as
`lifecycle.json.corrupt-<timestamp>`, creates a fresh empty snapshot, and
records the recovery for diagnostics. At most three quarantined snapshots are
retained. Abandoned sibling temporary files are removed while a writer already
holds the lock. If quarantine or initialization fails, the hook leaves the
existing file untouched and falls back without blocking Codex.

The lock file itself needs no stale-lock protocol: the operating system
releases an advisory lock when its owning process exits.

## Ordering Rules

Codex hook inputs do not expose a documented causal sequence or event
timestamp. codexctl therefore assigns a receive sequence under the store lock
and records its own wall-clock receive time.

Within one session:

- the first turn-scoped event establishes the current turn;
- a newer event for the current turn updates its projection;
- `Stop` closes only the matching current turn;
- an event for a recently closed or superseded turn is ignored;
- `UserPromptSubmit` for a different, unknown turn supersedes an open turn
  whose `Stop` was missed, and the old turn id becomes superseded;
- any other event for a different open turn is ambiguous, ignored, and recorded
  for diagnostics;
- repeating the same event is idempotent; and
- `SessionStart` updates identity, records its source, and clears current
  transient status without discarding the bounded recent-turn guard.

The bounded recent-turn list retains the 32 most recent closed or
superseded turn ids. This prevents a delayed `Stop` or permission event from an
old turn from replacing the current projection without creating an unbounded
event log.

Receive order cannot distinguish a genuinely delayed distinct event from a
legitimate repeated event with identical documented fields. The design relies
on Codex's synchronous lifecycle progression, warns about duplicate managed
scopes, bounds every observation by freshness, and uses newer transcript
evidence for correction.

The snapshot retains only the resulting bounded state, not every transition.
"Concurrent writer" coverage therefore proves that no accepted update is lost
to a read-modify-write race and that the final state reflects lock acquisition
order; it does not turn the snapshot into an event log.

## Event Projection

| Event | State effect |
| --- | --- |
| `SessionStart` | Update session/transcript identity; no direct status |
| `UserPromptSubmit` | `Processing` |
| `PreToolUse` | `Processing` |
| `PermissionRequest` with native allow/deny | `Processing` |
| `PermissionRequest` with no native decision | `NeedsInput` |
| `PostToolUse` | `Processing` |
| `SubagentStart` | Add agent and project parent as `Processing` |
| `SubagentStop` | Remove matching agent; do not end the parent turn |
| `Stop` | Close matching turn, clear active subagents, and project `Idle` |

`SubagentStart` and `SubagentStop` use the parent `session_id`, their documented
`turn_id`, and `agent_id`. A stop for an unknown agent is idempotent and does
not change the parent status.

## Reconciliation and Precedence

### Session identity hints

`SessionStart` records the canonical session id and transcript path before the
dashboard necessarily observes the corresponding transcript transition. The
discovery layer may use that record as an attachment hint only when all
applicable safeguards hold:

- the observation is no more than 30 seconds old;
- `transcript_path` is non-null and exists;
- the transcript's `session_meta` matches the recorded session id and
  normalized cwd;
- the transcript has post-process-launch activity, preserving safe resume of
  an older transcript; and
- the target is either the retained live process already associated with the
  session or the only unmatched compatible process.

If multiple unmatched processes share the cwd, the hint is ambiguous and is
not assigned. A null transcript path never drives attachment. Normal
transcript discovery must resolve either case. Lifecycle hooks do not inspect
process ancestry or invent a PID that the official payload does not provide.

The dashboard continues to discover and enrich processes, parse new transcript
bytes, inspect terminal approval evidence, and then resolve final status. The
status resolver uses this precedence:

1. a dead process is `Finished`;
2. terminal-confirmed approval or transcript-confirmed
   `request_user_input` is `NeedsInput`;
3. a newer, fresh lifecycle observation for the exact live session applies;
4. newer transcript lifecycle evidence applies;
5. existing CPU and process heuristics apply; and
6. missing or unsupported evidence remains `Unknown` rather than inventing an
   actionable state.

A lifecycle observation is eligible only when:

- its `session_id` exactly matches the discovered live session;
- its normalized cwd matches;
- its transcript path, when both sides have one, matches the attached
  transcript;
- it remains within the state-specific freshness bound.

Transcript serialization and hook invocation have no documented relative
ordering. File mtime alone therefore never decides which source wins. The
transcript parser retains the latest relevant lifecycle class and entry
timestamp, and reconciliation compares lifecycle meaning:

- transcript `task_complete` ends hook-derived `Processing`;
- a transcript user message, task start, tool call, or explicit input request
  that represents new progress overrides an older hook `Stop`;
- transcript `task_complete` does not override the matching hook `Stop`, because
  both represent the same completed turn; and
- explicit input evidence and process death retain their absolute precedence.

Freshness bounds are event-specific:

- `UserPromptSubmit`, automatic permission decisions, and `PostToolUse`: 30
  seconds;
- `PreToolUse`: 10 minutes or until `PostToolUse`, `Stop`, a newer turn, or
  newer semantic transcript evidence;
- `SubagentStart`: 10 minutes or until its matching stop or newer semantic
  evidence;
- `NeedsInput`: 10 minutes, aligned with the existing waiting window and still
  subordinate to explicit terminal/transcript evidence; and
- `Idle` from `Stop`: 10 minutes or until any newer hook or transcript event.

A persisted receive timestamp more than five seconds in the future is unusable
clock-skew evidence and does not contribute to status. A subsequent valid event
may replace it normally by receive sequence.

These bounds make hooks an immediate ordered overlay rather than a permanent
second source of truth. A missing `Stop` cannot leave a session processing
indefinitely, and a disabled hook cannot leave old state authoritative after a
restart.

Lifecycle observations never populate `pending_tool_name`,
`pending_tool_call_id`, `pending_tool_input`, approval evidence, terminal
targets, rule inputs, or brain authorization inputs.

## Failure and Security Behavior

- Generic lifecycle success emits no standard output.
- Input beyond 64 KiB, invalid JSON, missing or oversized identity, wrong event
  names, more than 64 active subagents, lock timeout, and persistence failure
  produce a standard-error diagnostic and no state transition.
- Hook absence, disablement, trust failure, timeout, or stale state falls back
  to existing observation paths.
- A newer-schema snapshot is never mutated by an older binary. A corrupt
  current-schema snapshot is quarantined and rebuilt because it is derivative,
  reconstructible status data rather than a source of truth.
- A lifecycle write failure never produces an approval.
- Permission-hook allow and deny behavior remains governed by its existing
  validation, brain threshold, durable audit, and response serialization.
- State applies only to a matching discovered live process; cwd alone is never
  sufficient.
- State files contain no prompt text, command text, tool input, or tool output.
- Diagnostics never echo raw hook payloads or rejected field values.
- No lifecycle observation causes terminal input, process termination, file
  edits, or external mutations.

The local user remains inside the trust boundary: a user who can modify
`~/.codexctl` can already modify codexctl configuration and state. Private file
permissions prevent accidental cross-user disclosure on a shared machine.
Existing symlink-based compatibility state roots remain supported; this
reconstructible cache does not add a migration or reject an existing root
solely because it resolves through a symlink.

A forged lifecycle payload can at most affect displayed status, and only when
its session evidence matches a discovered live process. It cannot populate
approval evidence, authorize a tool, or trigger any action.

## Diagnostics

Extend hook discovery and doctor to report the lifecycle definition as:

- missing;
- current;
- stale;
- disabled;
- duplicated across applicable scopes.

Codex hook trust is not reliably observable from the hook definition alone.
Trust is therefore a separate `unverified` diagnostic dimension for every
otherwise enabled definition. codexctl directs the user to `/hooks` rather
than claiming a hook is trusted.

JSON/session detail diagnostics expose:

- whether hook evidence is available;
- the last accepted event and its age;
- whether it currently contributes to status;
- the last ignored-event reason; and
- corrupt or newer-schema store state.

Diagnostics expose identities and event names, not sensitive payload bodies.

## Testing

### Parsing and adapters

- Sanitized fixtures cover every installed official event payload without
  retaining prompts, commands, tool inputs, or outputs.
- Valid payloads for every installed lifecycle event.
- Optional documented fields, unknown additional fields, and null transcript
  paths.
- Malformed JSON, wrong event name, empty session/turn ids, and missing
  event-specific identity.
- Input over 64 KiB, identifiers over 512 bytes, and paths over 4 KiB are
  rejected without raw-value diagnostics.
- Generic handler emits no standard output on success or failure.
- Permission allow/deny, abstention, disabled brain, inference failure, and
  below-threshold projections without changing native decision behavior.
- Non-Bash permission requests record `NeedsInput`, do not invoke brain
  inference, and emit no authorization response.
- Subprocess tests invoke the built codexctl binary with temporary state roots
  and assert exit status, standard output, standard error discipline, input
  limits, and the resulting persisted snapshot. Permission subprocess cases
  separately assert the official response JSON when a decision is returned.

### Store and ordering

- Atomic replacement and recovery from an interrupted temporary write.
- Separate hook processes writing concurrently do not lose accepted updates
  and assign unique receive sequences; final state follows lock acquisition
  order. Lock-timeout coverage also uses separate processes rather than
  threads so it exercises operating-system advisory locks.
- Duplicate events are idempotent.
- Old-turn events cannot replace a current turn.
- `UserPromptSubmit` supersedes a turn whose `Stop` was missed; other events for
  a different open turn are ignored as ambiguous.
- Newer-schema snapshots are preserved and treated as read-only.
- Corrupt current-schema snapshots are quarantined and rebuilt, with at most
  three quarantines retained; recovery failure leaves the original untouched.
- Abandoned temporary files are cleaned under the writer lock, while advisory
  locks recover automatically on process exit.
- Lock failure, 24-hour pruning, the 128-session and 1-MiB caps, active-store
  capacity rejection, the 64-subagent cap, private permissions, symlinked
  compatibility roots, and 32-entry recent-turn history.
- Deterministic permutations of short event sequences are checked against a
  small reference projection model, including duplicate, delayed, superseded,
  and ambiguous-turn events.

### Reconciliation

- Hook state appears before matching transcript output.
- Hook-before-transcript and transcript-before-hook serialization produce the
  same semantic status.
- Transcript `task_complete` ends hook-derived processing without erasing a
  matching hook `Stop`.
- New transcript progress overrides an older hook `Stop`.
- Process death always becomes `Finished`.
- Missing `Stop` expires safely.
- Receive timestamps more than five seconds in the future are ignored as clock
  skew and can be replaced by a later valid event.
- Disabled or missing hooks preserve current behavior.
- Same-cwd sessions remain isolated.
- Startup, resume, clear, compact, transcript replacement, stale SessionStart
  records, null transcript paths, and mismatched transcript metadata.
- Permission fallthrough versus automatic allow/deny.
- Overlapping subagent starts/stops and unknown subagent stops.
- Explicit `request_user_input` and terminal-confirmed approval retain
  precedence.
- Monitor integration fixtures combine a hook snapshot, synthetic transcript,
  and injected process discovery to verify the final public session status and
  diagnostic provenance.

### Installation and diagnostics

- Imperative init and uninit are idempotent.
- Global and project imperative hook files round-trip through installation and
  removal in isolated temporary homes.
- Home Manager renders the same managed definitions with the selected absolute
  package executable.
- Unrelated hooks and matcher groups remain unchanged.
- Legacy `--json` lifecycle commands remain cleanup-only.
- Doctor covers missing, stale, disabled, duplicate, corrupt, unavailable or
  mismatched handlers, and trust-unverified states.
- Downgrade fixtures prove the newer remover strips lifecycle definitions while
  preserving unrelated hooks and state.

### Regression and quality gates

Existing token, cost, transcript assignment, status inference, pending-tool
identity, terminal approval, shell-approval safety, brain, and process-liveness
tests remain green.

A non-gating local benchmark exercises warm lifecycle-hook invocations under
nominal load and targets less than 50 milliseconds per invocation. Timing is
reported for engineering visibility rather than enforced as a potentially
flaky CI assertion.

Final gates:

```text
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
nix fmt -- --check
nix flake check
```

Tests use temporary homes and state roots. Automated verification does not
modify the operator's live `~/.codex/hooks.json`, `~/.codexctl`, or Home Manager
generation. A documented smoke checklist likewise uses an isolated temporary
home and verifies generated hook definitions and handler subprocesses without
starting against the operator's live configuration.

## Rollout

Imperative users run `codexctl init` or `codexctl init --plugin-only`, restart
Codex, and review the changed definitions through `/hooks`. Declarative users
rebuild Home Manager, restart Codex, and perform the same trust review after a
package or hook-definition change. Package installation or ordinary execution
does not edit hooks implicitly.

Transparent imperative downgrade is not guaranteed because an older binary
does not recognize the newer `--lifecycle-hook` command or its managed-command
signature. The supported sequence is:

1. run `codexctl init --remove` with the newer binary;
2. downgrade codexctl; and
3. run the older initialization command if its hooks are still wanted.

If the binary was downgraded first, recovery is to remove only exact handlers
whose command is `codexctl --lifecycle-hook` from the applicable Codex hook
files, or temporarily restore the newer binary and run `codexctl init
--remove`. The troubleshooting guide states this limitation. Home Manager
rollback remains generation-safe because it rewrites the absolute package path
and managed hook definitions together.

Existing dashboards and sessions remain functional without the lifecycle
hooks. Uninstall removes only exact codexctl-managed hook definitions. Expired
lifecycle state is ignored and pruned; hard purge continues to remove the
legacy `~/.codexctl` state root under its existing confirmation rules.

## Alternatives Considered

### Advisory-only hooks

Hooks could merely annotate transcript inference. This avoids precedence
rules, but it does not fix the status lag that motivated the feature.

### Hooks as the primary state machine

Making hooks permanently authoritative would simplify immediate projection but
would strand disabled, untrusted, failed, or pre-existing sessions and would
contradict the requirement to retain transcript and process reconciliation.

### Append-only event log

An append-only log preserves more forensic detail, but it requires compaction,
partial-write recovery, and sensitive-payload discipline for data that the
dashboard does not need. A locked bounded snapshot is smaller and sufficient
for current status.

### Per-event immutable files

Immutable event files avoid a shared write lock but create an unbounded spool
and make ordering, pruning, and dashboard scans more complex. The bounded
snapshot better matches the single-machine local status use case.

## Acceptance Criteria

- Installed hooks provide immediate lifecycle status without using
  `codexctl --json` as a handler.
- The dedicated adapter consumes official JSON and persists bounded state
  atomically by session and turn.
- Fresh hook state participates in the approved precedence order and yields to
  newer transcript evidence and process liveness.
- Duplicate, stale, out-of-order, missing-`Stop`, disabled-hook, same-cwd,
  resume, clear, compact, permission, and subagent cases have regression
  coverage.
- Doctor and JSON diagnostics expose hook availability and uncertainty without
  claiming trust that codexctl cannot observe.
- Lifecycle state cannot authorize tools or terminal input.
- Existing telemetry, cost, transcript assignment, approval safety, and
  process-liveness behavior remains green.

## Stress Test Results

The design was challenged branch by branch on 2026-07-17. Every resulting
decision was explicitly approved.

### Resolved Decisions

1. **Ownership boundary:** `codexctl-core` owns an injectable
   `LifecycleStore::at(root)` and one compatibility root resolver, preserving a
   narrow seam for the later XDG migration.
2. **Cross-turn ordering:** only `UserPromptSubmit` may supersede a different
   open turn; other cross-turn events are ambiguous and ignored with a
   diagnostic.
3. **Session identity:** `SessionStart` is a guarded short-lived identity hint,
   never a cwd-only assignment rule.
4. **Transcript ordering:** hook and transcript flush order is undocumented, so
   reconciliation compares lifecycle meaning and applies event-specific
   leases instead of relying on file mtime alone.
5. **Failure recovery:** newer schemas remain untouched and read-only; corrupt
   current-schema derivative state is quarantined, bounded, and rebuilt.
6. **Performance:** the hot path keeps atomic replacement but omits durability
   `fsync`, bounds sessions, turns, and serialized size, and uses a non-gating
   latency benchmark.
7. **Security:** hook input, identities, paths, and active subagents are
   bounded; diagnostics omit raw payloads; clock-skew evidence is ignored; and
   lifecycle state remains display-only.
8. **Rollout:** hook mutation requires explicit initialization or declarative
   rebuild, exact managed removal is idempotent, and imperative downgrade has
   a documented remove-before-downgrade sequence.
9. **Verification:** binary subprocesses, separate-process lock tests,
   reference-model event permutations, monitor integration, and installer
   round trips supplement unit coverage.
10. **Permission coverage:** the single permission matcher observes every tool
    for status, but brain authorization remains Bash-only. Non-Bash requests
    record `NeedsInput` and return no decision. Broader authorization is
    tracked separately by `codexctl-85x`.

### Changes Made

- Replaced underspecified ordering and identity assumptions with guarded,
  testable rules and bounded freshness leases.
- Added explicit corruption recovery, storage and input limits, clock-skew
  handling, rollback instructions, and process-level verification.
- Broadened permission observation to every tool without broadening the
  authorization surface.

### Deferred / Parking Lot

- `codexctl-85x` owns brain authorization for explicitly supported non-Bash
  tools.
- `codexctl-2yk` owns migration of the compatibility state root to XDG state.
- `codexctl-iyk` owns cost-estimate semantics and explainability.

### Confidence Assessment

- **Overall:** High.
- **Accepted concerns:** the Codex trust decision is unobservable; hook versus
  transcript serialization order is undocumented; imperative downgrade is not
  transparent after the newer remover is unavailable; and the local latency
  target is reported rather than enforced in CI. Each has an explicit
  diagnostic, fallback, or recovery path in this design.
