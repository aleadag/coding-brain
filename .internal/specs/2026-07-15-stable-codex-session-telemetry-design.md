# Stable Codex Session Telemetry

**Date:** 2026-07-15  
**Status:** Approved for planning  
**Tracking:** `codexctl-0bq`

## Summary

Codexctl will derive activity from Codex transcript lifecycle events instead of
using low parent-process CPU as evidence that a tool needs approval. A pending
shell tool becomes actionable `NeedsInput` only when a readable terminal pane
also shows the matching Codex approval prompt. Each live Codex process will
retain one stable transcript attachment, including when several sessions run
from the same directory.

The dashboard's cost value will be an API-equivalent estimate. It will
accumulate the price of each completed model request from
`last_token_usage`, using the model and pricing rules active for that request.
The estimate will not decrease during a session.

## Observed Failures

The current Codex parser treats a `function_call` as a pending tool. When the
Codex parent process has low CPU, status inference maps that pending tool to
`NeedsInput`, even while an approved child command is running. Current rollout
files also contain `custom_tool_call`, `reasoning`, `task_started`, and
`task_complete` events that the status state machine does not use.

Transcript discovery sorts rollout files by modification time and independently
chooses the newest compatible transcript for every process. Two processes with
the same working directory can therefore attach to the same transcript or
switch attachments as their rollout files alternate writes. The TUI correctly
resets accumulated state when a transcript changes, but a false change makes
cost temporarily fall or disappear.

Cost is currently recomputed from cumulative tokens using the session's latest
model. This cannot price mixed-model sessions or per-request long-context
multipliers correctly. `gpt-5.6-sol` also falls through the unknown-model
profile, which gives it the wrong 258,400-token context limit. OpenAI documents
a 1,050,000-token context window, $5.00 input, $0.50 cached input, and $30.00
output per million tokens. Requests above 272,000 input tokens apply 2x input
and 1.5x output pricing for the full request. Cache writes cost 1.25x uncached
input. See the [GPT-5.6 Sol model page](https://developers.openai.com/api/docs/models/gpt-5.6-sol).

## Goals

- Show `Processing` while Codex is reasoning or executing a tool, regardless
  of low parent-process CPU.
- Show `NeedsInput` for `request_user_input`, a legacy explicit waiting signal,
  or a pending shell tool whose terminal pane visibly confirms the matching
  Codex approval prompt.
- Let the brain evaluate confirmed shell permission prompts, while preventing a
  stale decision from sending input to a command that has resumed.
- Keep one live process attached to one stable rollout transcript.
- Make the API-equivalent cost estimate accurate per request and monotonic for
  a session.
- Support GPT-5.6 Sol, Terra, and Luna model profiles and context limits.

## Non-Goals

- Report the user's actual ChatGPT or Codex subscription charge.
- Infer approval state from CPU thresholds or elapsed time.
- Reparse every historical rollout on every dashboard refresh.
- Change approval policy semantics or Codex sandbox permissions.

## Transcript Assignment

Discovery will assign transcripts across the complete set of live processes,
not process by process. An explicit `codex resume <session-id>` match has first
priority. Other candidate transcripts must match the process working directory
and have a session start time compatible with the process start. Candidates are
ranked by the distance between process start and transcript start, using the
timestamp in `session_meta`; file modification time is only a freshness
constraint and tie-breaker. When an interactive bare `codex` process resumes an
older session without exposing its ID in the command line, one otherwise-unused
older transcript may instead match if it is the only same-directory transcript
with activity after the process launched. Multiple such transcripts remain
unassigned.

A transcript may be assigned to at most one live process in a discovery pass.
The merge layer retains an existing attachment while it remains a valid
candidate. It changes attachment only when Codex exposes a different session
identity, the current transcript disappears, or a new process gains a unique
reliable match. Ambiguous new processes remain visible with pending telemetry
instead of borrowing another process's transcript.

`/clear` remains a real transcript transition. Once the new transcript has a
distinct session identity and matches the same process, the TUI replaces the
old telemetry state while preserving process-level history such as CPU
samples. If multiple newer same-directory transcripts exist, only a unique
most-recently-modified candidate may enter the transition confirmation; a tie
keeps the retained transcript. A fresh dashboard may seed the first transition
observation immediately after establishing its retained assignment, but it may
switch only after the next outer refresh confirms the same candidate using a
new uncached scan.

A bare interactive Codex process may also resume a transcript that started
before its retained transcript. When the retained transcript has stopped
advancing, an older same-directory transcript may enter the same two-scan
confirmation only if its modification time is newer than the retained
transcript's last observed activity and it is the unique most-recently-active
unclaimed candidate. This activity exception permits reattachment to a resumed
session without weakening the uniqueness and confirmation safeguards used for
`/clear`.

## Status State Machine

The Codex parser will expose lifecycle meaning rather than converting response
items directly into legacy `last_type` and `stop_reason` strings:

- `task_started`, a user message, reasoning, an agent message produced during
  the turn, and tool calls or outputs all indicate `Processing`.
- `function_call` and `custom_tool_call` open an in-flight tool call keyed by
  `call_id`; their matching output closes it. Both states remain `Processing`.
- The explicit `request_user_input` tool and legacy `waiting_for_task` signal
  indicate `NeedsInput` until the transcript records the user's response or
  continued work.
- A pending shell tool is an approval candidate, not approval evidence. On
  tmux and Kitty, codexctl captures the session's pane and promotes the
  candidate to `NeedsInput` only when the visible Codex approval UI matches the
  pending call. A running command with no matching prompt remains
  `Processing`.
- `task_complete` indicates `WaitingInput` for a recent live session and `Idle`
  after the existing inactivity window.
- `turn_aborted` ends processing without fabricating an approval request.

High CPU may confirm `Processing`, but low CPU cannot create `NeedsInput`.
Current rollout files do not expose a distinct shell-permission event, so pane
confirmation is required for that transition. Unsupported or unreadable
terminal backends do not enable shell auto-approval. Legacy transcript parsing
remains intact, but legacy pending-tool state is subject to the same terminal
confirmation before it becomes actionable.

Unknown modern events are ignored permissively. Once `task_started` is seen,
the task remains `Processing` until completion or abort unless an explicit
input event or terminal-confirmed approval supersedes it. Function and custom
tool calls and outputs are paired by `call_id`.

## Approval Safety

The monitor owns the authoritative session snapshot used by the brain. An
approval proposal is bound to the process, transcript session, terminal pane,
pending call ID, tool, command, and a fingerprint of the visible prompt. Deny
rules take precedence over brain approval.

Immediately before sending Enter, codexctl recaptures the pane and requires the
same approval prompt and identity. A missing, changed, or unreadable prompt
cancels the action. CPU, elapsed time, and a pending transcript tool can request
another observation, but none can authorize input on their own.

## Cost Ledger

Each parsed `token_count` event contributes at most one request entry. The
entry uses `last_token_usage`, the model from the active `turn_context`, and
that model's pricing profile. A monitor-held transcript offset handles normal
incremental reads, while the cumulative `total_token_usage` value is a second
watermark that prevents duplicate charging after replay or truncation. The
cumulative fields remain the source for token and context displays, not cost
recomputation.

The pricing calculation separates uncached input, cached input, cache writes,
and output. Model profiles may define a long-context threshold and input/output
multipliers. The threshold applies to the input tokens for that request, not
the session's cumulative input. Codexctl prices only categories exposed by the
transcript; if a charge-bearing category such as cache writes is absent, it
does not invent usage and marks the estimate unverified.

The parent request ledger and each subagent ledger accumulate independently,
then the displayed estimate sums them. Changing models affects only later
requests. Unknown models use the configured fallback and retain the existing
`?` marker. The compact table labels the value `Est. $`; the detail panel uses
`Estimated cost`. Machine-readable output retains `cost_usd` and may add
verification or pricing-profile metadata without changing its meaning.

If a transcript is truncated in place, codexctl preserves the existing ledger,
replays without charging snapshots at or below the cumulative watermark, and
continues when a later snapshot advances it. If the cumulative counter itself
resets, codexctl freezes the last estimate and marks it unverified rather than
showing a lower value. A genuine transcript transition such as `/clear` starts
a new session estimate; ordinary refreshes never clamp a lower recomputation
or mask a changed attachment.

## Refresh And Scale

Discovery caches transcript summaries and limits candidates by working
directory and a compatible process-start window. It does not parse full
transcripts while assigning them. The monitor reads only complete JSONL bytes
appended after its last offset and uses the cumulative token watermark to
reject replay.

Terminal capture is limited to sessions with an unresolved approval candidate
and is briefly debounced; codexctl does not scrape every pane on every refresh.
Existing transcript-index caching remains in place.

## Failure Handling

- Ignore an incomplete trailing JSONL line until a later refresh completes it.
- Ignore duplicate request events already covered by the monitor-held offset.
- Preserve the last valid attachment and totals when discovery is temporarily
  ambiguous or a transcript cannot be read.
- If terminal capture fails, times out, or is unsupported, retain a
  non-actionable approval-unknown diagnostic, keep the session working, and
  retry after a bounded delay.
- Cancel a proposed action when its pre-send pane recapture does not match.
- Mark unsupported models as unverified instead of silently presenting their
  fallback price as exact.
- Parsing remains observational; only the existing policy and brain action path
  may approve, deny, or send input after the safety recheck.

## Verification

Regression tests will cover:

- a tool running with low parent CPU remains `Processing`;
- `request_user_input` and legacy waiting signals become `NeedsInput`;
- a pending shell call without a matching visible approval prompt remains
  `Processing`;
- matching tmux and Kitty approval prompts become actionable `NeedsInput`;
- unsupported, failed, and stale pane captures never authorize input;
- a stale brain decision cannot send Enter after the prompt or call changes;
- deny rules override a brain approval;
- reasoning and custom tool events remain `Processing` until task completion;
- matching tool outputs close in-flight calls without ending the turn;
- two same-directory processes receive distinct stable transcripts as their
  files alternate writes;
- an ambiguous new process remains pending rather than borrowing a transcript;
- `/clear` replaces telemetry only after a distinct session is identified;
- a long-running dashboard reattaches when bare Codex resumes an older
  transcript and that file becomes the unique most-recently-active candidate;
- repeated refreshes charge each request once and never lower the estimate;
- mixed-model requests preserve their individual prices;
- cached input, exposed cache writes, and GPT-5.6 long-context multipliers are
  applied to the correct request, while missing charge data is unverified;
- truncated and partially written transcripts recover without double charging;
- legacy lifecycle fixtures retain their behavior, subject to terminal
  confirmation for actionable shell approval;
- unknown modern events do not terminate an active task;
- the table and detail labels are `Est. $` and `Estimated cost`.

Targeted tests should run before the full workspace gates. Completion requires
`cargo test`, `cargo fmt --check`, and `cargo clippy -- -D warnings`.

## Stress Test Results: Stable Codex Session Telemetry

The review resolved eight branches on 2026-07-15: transcript identity, event
semantics, cost accounting, truncation and replay, compatibility, refresh
scale, failure recovery, and security verification. Six recommendations were
accepted unchanged. Two were modified during review:

- Pending shell tools are not always `Processing`; a matching visible Codex
  approval prompt may promote them to `NeedsInput` so the brain can judge the
  permission request.
- The user-facing cost label is shortened to `Est. $` in the table and
  `Estimated cost` in details, while retaining the API-equivalent meaning.

The resulting design preserves legacy parsing and existing policy semantics,
adds only compatible model-profile fields and optional output metadata, and
does not add a persistent-state migration. No reviewed branch remains deferred.
The design is ready for an implementation plan, with the highest-risk tests
covering stale approval actions, same-directory transcript assignment, and
monotonic per-request cost accumulation.
