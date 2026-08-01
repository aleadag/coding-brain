# Permission Audit Transaction Recovery

- Date: 2026-07-31
- Bead: `codexctl-ug26`
- Brainstorming: `codexctl-lekf`
- Status: Approved

## Summary

Coding Brain will use a recoverable write-ahead transaction for the decision
proposal, executable lifecycle disposition, and terminal activity produced by a
permission hook. The transaction journal, rather than a timeout margin, makes a
partially committed permission evaluation detectable and recoverable.

This is recoverable consistency, not atomic visibility across independent
files. Destination stores may briefly disagree after a process interruption,
but the durable journal makes that state explicit and sufficient for
idempotent, fail-closed recovery. Coding Brain recovers pending transactions
before presenting activity and emits no provider response until the current
transaction verifies complete.

The hook will not emit an executable provider response until the required
lifecycle and audit records are durably present. Recovery may complete a
transaction only when its recorded authority is sufficient. An executable
allow whose lifecycle authority cannot be proven is converted to
`Error`/`NeedsInput`; recovery never reconstructs an allow from a proposal
alone.

Legacy `Observed`/`Evaluating` permission activities that become stale without a
recoverable transaction will project as
`INCOMPLETE — permission evaluation timed out`, not as a stopped tool or an
observed tool interruption.

## Problem

The managed permission hook has a 30-second provider deadline, while model
inference may consume 25 seconds. After inference, the current hook appends a
proposal to `decisions.jsonl` before it appends the corresponding terminal
event to `activity.jsonl`. A provider deadline, process interruption, or
terminal-store failure between those writes can therefore leave:

- a durable proposal;
- only `Observed` and `Evaluating` activity rows;
- no authoritative terminal activity; and
- no durable record explaining why the commit stopped.

After the stale-activity threshold, projection converts those non-terminal rows
to `Interrupted`. Live renders that as `STOPPED` with an interrupted outcome,
even though Coding Brain observed neither a stopped tool nor a tool outcome.
A later native `PostToolUse` cannot correlate to an eligible terminal allow and
may produce a separate orphan diagnostic.

Increasing the inference reserve narrows the failure window but cannot make
independent durable stores atomic. Correctness must not depend on how much time
remains after inference.

## Transaction Model

### Journal location and identity

Permission transactions live in an owner-only directory beneath the trusted
Coding Brain state root. Each transaction uses a unique regular journal file;
parallel permission hooks never share or replace one journal.

The journal has a versioned, bounded schema containing:

- a unique transaction ID;
- the stable activity and decision IDs;
- the bounded, redacted decision proposal;
- the bounded, normalized terminal activity;
- the provider-qualified lifecycle identity and request key;
- the intended lifecycle disposition;
- whether an executable allow requires proven lifecycle authority.

Destination paths are not stored in the journal. Recovery derives every path
from the trusted state root.

The journal is immutable after preparation. Progress is determined by rereading
the destination stores, not by updating a phase marker that could itself
disagree with a completed destination write.

### Durable preparation

After evaluation and response serialization, the hook generates stable IDs and
durably prepares the complete journal using owner-only permissions, atomic
replacement, file synchronization, and parent-directory synchronization.
Journal preparation is the transaction commit boundary.

The creating process writes and syncs a unique sibling temporary file, locks
that open inode, atomically renames it to the final journal name, and syncs the
parent directory before committing destinations. It holds the same lock through
verification and journal removal, so recovery can never observe a published
but not-yet-locked journal. Process death releases the lock and makes the final
journal eligible for recovery.

An interruption before the rename can leave only a temporary file and cannot
have written any destination. Recovery may remove an unlocked temporary file
only after validating its owner, mode, type, link count, and generated filename.
A locked temporary belongs to an active preparer and is left untouched.

If preparation fails, the hook emits no executable allow. It records a terminal
error and `NeedsInput` where those stores remain available, reports a bounded
diagnostic, and preserves the provider's native confirmation path.

### Idempotent commit

Proposal, lifecycle, and terminal writes use the stable journal identity and
are idempotent. Recovery can reread each destination and distinguish an absent
record from the exact record already committed. A conflicting record with the
same identity is an error, never a duplicate success.

The normal commit sequence is:

1. durably prepare the journal;
2. append the proposal if absent;
3. persist the intended lifecycle disposition;
4. append the terminal activity if absent;
5. reread and verify all required durable records;
6. remove the journal and sync its parent directory; and
7. only then emit an executable provider response.

If removal succeeds but the following directory sync fails, the final journal
name is already absent from the current namespace. This is reported as
`RemovalSyncUncertain`, not as a pending journal. A model-derived allow remains
suppressed and its lifecycle authority is compensated to `NeedsInput`. A crash
may make the journal name reappear; recovery then handles the same immutable
journal idempotently and never restores positive authority from it.

Store locks are acquired sequentially and never nested. Existing bounded store
locking remains authoritative. Destination lookup and append-if-absent happen
under the destination's exclusive lock, so two recovery processes cannot append
the same stable identity.

### Executable allow recovery

An `Allowed` terminal event is valid only when the exact lifecycle request has
a durable executable `Decided` disposition. Recovery therefore inspects the
recorded lifecycle identity and request key before completing an interrupted
allow.

- If exact lifecycle authority is present, recovery may idempotently append the
  missing `Allowed` terminal event. The recovered allow remains
  delivery-unknown until separate provider evidence arrives.
- If authority is absent, rejected, ambiguous, or unreadable, recovery records
  `NeedsInput` and an `Error` terminal event. It does not append `Allowed` and
  does not emit or reconstruct an allow response.
- If `Allowed` is already durable, recovery verifies the lifecycle authority
  before treating the transaction as complete. A contradiction remains a
  visible recovery failure.

This preserves the current invariant that a model proposal is not executable
authority and that terminal audit state is durable before provider delivery.

### Deny and abstain recovery

Model denies, provider-policy denies, abstentions, and inference errors carry no
positive execution authority. Recovery may roll their terminal records forward
from the validated journal, then persist their exact lifecycle disposition.
It never turns one of these states into an allow.

Transaction persistence must not weaken a denial. Deterministic safety denies
and provider-policy denies retain their current fail-safe response path: they
attempt durable transaction and audit persistence, but a persistence failure
does not suppress the deny response. Model-derived denies likewise prefer the
deny response on audit failure because denial cannot grant capability, while
reporting the bounded persistence diagnostic.

Model-derived allows remain gated on complete durable transaction state.
Abstentions and inference errors emit no executable response and preserve native
confirmation.

Inference failures retain the bounded `Brain query failed: ...` reasoning in
the terminal `Error` event, making the original timeout visible rather than
replacing it with a projected interruption.

### Journal completion and failure

A journal is removed only after all required destinations have been reread and
verified durable. Failures before a successful unlink retain the journal and
emit a bounded recovery diagnostic. The sole exception is a directory-sync
failure after unlink has already succeeded: the system reports the dedicated
non-pending `RemovalSyncUncertain` outcome, suppresses executable allow, and
compensates lifecycle authority to `NeedsInput` rather than claiming that a
discoverable journal remains.

Recovery runs:

- before accepting a new permission-hook transaction; and
- during normal Coding Brain startup before activity snapshots are presented.

Recovery acquires each journal with a bounded, non-blocking exclusive lock. A
locked journal belongs to an active hook and is skipped rather than treated as
failed. Recovery processes may work on different journals concurrently while
the destination-store locks serialize shared append-only files.

Recovery is safe to repeat after interruption at any phase. It does not delete
invalid or uncertain journals.

## Projection and UI

`Observed` or `Evaluating` permission activity without a terminal event remains
non-terminal source evidence. Once stale, projection uses an `Incomplete`
presentation state rather than `Interrupted`.

`Incomplete` is projection-only and is never serialized to `activity.jsonl`.
Elapsed time cannot prove that the hook died, and persisting a synthetic first
terminal event could block a later observed `Allowed`, `Denied`, or `Error`
event.

Live renders:

- badge: `INCOMPLETE`;
- outcome: `permission evaluation timed out`; and
- no claim that the tool stopped, failed, or completed.

Projection does not rewrite `activity.jsonl`. Existing terminal and outcome
precedence remains unchanged. `PostToolUse` correlation continues to require an
eligible terminal `Allowed` event with a decision ID; a proposal or incomplete
evaluation never becomes outcome authority.

## Validation and Security

Recovery accepts a journal only when:

- its schema version is supported;
- its size and every variable field are within existing persistence bounds;
- its file is owned by the current user, has one link, and is a regular,
  non-symlink file in the trusted transaction directory;
- IDs and lifecycle identity are internally consistent;
- the activity payload passes existing consistency checks; and
- the proposal and terminal records agree on provider, project, session, turn,
  tool, decision ID, and action.

The transaction directory uses mode `0700`; journals use mode `0600`. Recovery
uses no-follow open semantics where available and validates metadata from the
opened file rather than trusting a path-only precheck. Journals contain only
the same bounded, redacted data intended for the destination stores, never raw
tool input or model responses. Error messages do not echo raw journal contents,
commands, or sensitive model output.

Malformed, oversized, unsupported-version, symlinked, conflicting, or otherwise
unverifiable journals remain available for diagnosis and produce a visible
recovery failure. Recovery never guesses, silently deletes them, or weakens
native confirmation.

Journal discovery has explicit file-count and total-byte bounds. Exceeding
either bound is a visible recovery failure and blocks new executable allows
until the pending state is inspected.

## Recovery Scale and Admission

Recovery scans journals in deterministic oldest-first order. Normal startup may
drain recoverable journals in bounded batches. Permission-hook entry performs
only a bounded recovery pass so an old backlog cannot consume the current
provider deadline.

If any prior transaction remains locked, invalid, over-budget, unresolved, or
has a removal-sync-uncertain outcome after the bounded pass, the current request
does not begin model inference and cannot produce an executable allow. It falls
through to native confirmation with a bounded diagnostic. Unrelated permission
requests are never grouped into one journal or one transaction.

## Tests

### Transaction fault matrix

Fault-injection coverage interrupts a transaction:

- after journal preparation;
- after proposal append;
- after lifecycle persistence;
- after terminal append; and
- after verification but before journal removal.

Each case restarts recovery and asserts:

- exactly one proposal;
- exactly one authoritative terminal activity;
- the correct lifecycle disposition;
- no duplicate rows after repeated recovery; and
- journal removal only after verified completion.

A separate fault injects a directory-sync failure after a successful unlink.
It asserts `RemovalSyncUncertain`, no false pending count, suppressed provider
allow, `NeedsInput` compensation, and idempotent fail-closed handling if a
crash makes the journal name reappear.

Every recovered case runs recovery a second time to prove idempotency.

### Fail-closed allow coverage

Tests verify that:

- an interrupted allow with exact durable lifecycle authority recovers its
  terminal `Allowed` event;
- an interrupted allow without that authority becomes `Error`/`NeedsInput`;
- recovery never emits a provider response;
- the live hook emits allow only after transaction completion; and
- outcome correlation still requires the recovered eligible terminal allow.

### Deadline and legacy coverage

The original inference-timeout path is reproduced with an interruption after
proposal persistence. The journal must recover the terminal `Error` with the
original bounded timeout diagnostic.

A real subprocess regression pauses after proposal persistence using an
explicit readiness marker, confirms the journal and proposal, kills the hook,
then runs recovery. This verifies OS-lock release and cross-process recovery
without a wall-clock sleep or a literal 25-second timeout.

Separate projection and TUI tests use legacy stale `Observed`/`Evaluating`
events without a journal and assert `INCOMPLETE — permission evaluation timed
out`, no `STOPPED` badge, no interrupted tool outcome, and no source-log rewrite.

### Concurrency and adversarial journals

Concurrent permission-hook tests assert unique journals, complete independent
transactions, stable lock behavior, and no duplicate proposals or activities.
At least one regression uses multiple processes rather than only threads.

Adversarial tests cover malformed JSON, oversized records, unsupported schema
versions, symlinks, hard links, wrong ownership where supported, non-regular
files, scan-count and total-byte limits, identity disagreement, destination
conflicts, and interruption during recovery. Every case must fail closed and
retain uncertain evidence.

### Regression gates

Run existing provider-response, lifecycle-correlation, permission-burst,
terminal-persistence, and `PostToolUse` outcome regressions before the complete
workspace formatting, test, Clippy, and build gates.

## Compatibility and Rollback

The transaction journal is additive state. Existing decision, activity, and
lifecycle record schemas remain readable. New projection behavior affects only
stale non-terminal permission evaluations and is not serialized.

Journals have an independent schema version. An unsupported version remains
untouched, produces a Doctor failure, and blocks executable allows rather than
being guessed or migrated implicitly. Legacy proposal-only evaluations do not
receive manufactured journals or terminal events.

An older binary ignores journal files but continues to treat proposals as
non-executable and requires existing lifecycle/activity authority. Rolling back
therefore degrades to the former fail-closed behavior, although pending
transactions will not recover until a supporting binary runs.

Doctor reports pending, locked, invalid, and unrecoverable permission
transactions. Before downgrade, the supporting binary must run recovery and
verify that no pending journals remain. Rollback never auto-deletes unresolved
journals; if downgrade proceeds anyway, they remain untouched for a later
supporting binary.

## Non-goals

- Treating a proposal as a committed decision.
- Inferring tool execution or outcome from missing events.
- Emitting a delayed provider response during recovery.
- Raising the provider hook deadline or relying on a larger inference reserve
  for correctness.
- Changing native provider confirmation behavior.
- Relaxing `PostToolUse` correlation eligibility.

## Stress Test Results: Permission Audit Transaction Recovery

### Resolved Decisions

- Consistency guarantee: the journal provides recoverable consistency, not
  atomic visibility across independent files.
- Allow authority: recovery may append `Allowed` only with exact durable
  lifecycle authority, never replays a provider response, and otherwise
  records `Error`/`NeedsInput`.
- Concurrency: the live hook holds an exclusive journal lock; recovery skips
  active journals and destination idempotency is enforced under existing store
  locks.
- Journal security: owner, mode, file type, link count, no-follow, size, scan,
  identity, and redaction checks fail closed.
- Crash recovery: journals are immutable; destination inspection, not a mutable
  phase marker, determines progress.
- Compatibility: `Incomplete` remains projection-only and journal versions are
  independent of existing persisted schemas.
- Scale: recovery is deterministic and bounded; unresolved prior state blocks
  new executable allows without consuming the full provider deadline.
- Rollback: Doctor exposes pending transactions, and downgrade requires a
  verified empty journal set.
- Testing: deterministic fault injection is complemented by real
  multi-process kill/recovery and concurrency regressions.
- Deny availability: transaction persistence cannot suppress deterministic,
  provider-policy, or model denial responses; audit failure never broadens
  capability.

### Changes Made

- Removed mutable transaction phases from the journal design.
- Clarified recoverable-consistency semantics and delivery-unknown recovered
  allows.
- Added per-journal process locking and atomic destination idempotency.
- Expanded filesystem validation, recovery admission bounds, Doctor coverage,
  rollback prerequisites, and subprocess verification.
- Made the projection-only nature of `Incomplete` explicit.
- Preserved fail-safe denial responses independently from audit persistence.

### Deferred / Parking Lot

- Exact journal count and byte limits will be selected in the implementation
  plan from existing persistence bounds.
- A dedicated recovery command remains unnecessary unless implementation shows
  that startup and Doctor cannot safely provide the required recovery path.

### Confidence Assessment

- Overall: High
- Areas of concern: filesystem ownership and no-follow APIs require
  platform-specific implementation and tests; the implementation plan must
  preserve conservative behavior on platforms without identical metadata
  support.
