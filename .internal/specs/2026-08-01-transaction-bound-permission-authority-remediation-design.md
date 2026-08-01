# Transaction-Bound Permission Authority Remediation

- Date: 2026-08-01
- Bead: `codexctl-ug26`
- Task: `codexctl-fb9y`
- Brainstorming: `codexctl-fb9y.1`
- Status: Approved; adversarial stress-test complete

## Summary

Permission lifecycle authority will identify the exact transaction and action
that created it. A request-scoped `Decided` bit is insufficient because a deny
transaction can otherwise satisfy the authority check of a concurrent or
retained allow transaction for the same provider request.

The remediation introduces transaction/action-bound lifecycle decisions,
nonblocking same-request serialization, a separate live-hook recovery budget,
and fail-closed lifecycle admission. Deny responses remain available when
audit persistence fails, but no failure path writes execution-capable
authority outside a verified transaction.

This specification amends the approved permission audit transaction design. It
does not change provider response formats, replay responses during recovery, or
weaken deterministic safety denials.

## Security Invariants

1. An allow journal is executable only when lifecycle state contains the exact
   `(request identity, request key, transaction ID, Allow)` authority expected
   by that journal.
2. `Deny` authority cannot satisfy an `Allow` journal, even when the request key
   is identical.
3. `NeedsInput` dominates every decided authority and cannot be reversed by a
   later `Decided` event.
4. No hook writes `Decided` outside a successfully verified transaction.
5. At most one hook for a lock shard may pass admission, evaluate, commit, and
   select a response at a time. No hook selects an executable response without
   the shard guard. Lock contention fails closed without waiting for a timeout.
6. Corrupt, unavailable, or newer lifecycle state is never interpreted as a
   clean missing decision.
7. Live-hook recovery and destination inspection have fixed count, byte, and
   lock-acquisition bounds.

## Transaction-Bound Lifecycle Decisions

### Authority model

Core lifecycle state gains these versioned value types:

```rust
pub enum PermissionAction {
    Allow,
    Deny,
}

pub struct PermissionAuthority {
    pub transaction_id: String,
    pub action: PermissionAction,
}

pub enum PermissionDecision {
    NeedsInput,
    Decided(PermissionAuthority),
}
```

Keyed permission events retain the request key and carry an optional authority:
`NeedsInput` has no authority; `Decided` requires one. Unkeyed legacy provider
events remain readable but cannot prove executable transaction authority.

The snapshot retains the existing monotonic permission bits for projection and
adds a request-keyed authority map. Antigravity continues to require the exact
request-key-to-step binding and exact step child bits; transaction authority is
an additional required equality, not a replacement for those checks.

Store operations consume and return `PermissionDecision`, not a bare
disposition. Exact ensure semantics are:

- absent plus `Decided(A)` inserts `A`;
- existing `Decided(A)` plus the same `Decided(A)` is idempotent;
- existing `Decided(A)` plus different transaction or action authority is a
  conflict;
- existing `Decided(A)` plus `NeedsInput` records the dominating fail-closed
  state;
- existing `NeedsInput` plus any `Decided` is a conflict; and
- corrupt/newer state returns a typed error instead of `Ok(None)`.

Each journal derives its expected authority from its immutable transaction ID
and terminal action. Allow commit and recovery compare the full authority.
Deny commit likewise verifies its exact deny authority. Abstain and inference
error journals use `NeedsInput` and carry no decided authority.

### Schema and rollback safety

The lifecycle snapshot schema advances from version 3 to version 4. Version 3
snapshots migrate with an empty authority map. Existing bare `Decided` evidence
remains visible for diagnostics and duplicate suppression, but is not
execution-capable transaction authority.

The version bump is mandatory: an older binary must reject a version 4
snapshot rather than silently discard authority tokens and reinterpret a bare
request-scoped `Decided` bit as executable authority.

The transaction journal schema advances for the new authority contract. The
intermediate journal format exists only on this unshipped branch, so no
production migration from it is required. New journals validate that terminal
state, action, transaction ID, and intended lifecycle decision agree exactly.

## Same-Request Serialization

Authority tokens prevent cross-authorization but do not prevent two concurrent
hooks from emitting contradictory provider responses. Hooks therefore acquire
a persistent sharded lock before recovery and admission.

- Locks live in a separate owner-only managed directory under
  `brain/permission-request-locks`, never in `permission-transactions`.
- There are 256 fixed shard names. The shard is selected by the first byte of a
  domain-separated SHA-256 digest over the provider-qualified lifecycle
  identity and request key.
- Lock files are persistent owner-only regular files with one link. They are
  never unlinked after use, avoiding split-lock races.
- Acquisition uses a single nonblocking exclusive attempt. Busy or invalid
  lock state becomes a bounded admission diagnostic; it never sleeps or waits
  for a timeout.
- The guard is held from before transaction recovery and lifecycle admission
  through inference, transaction commit, provider response write, and delivery
  evidence append.
- Every recovery path acquires the same shard before mutating a journal's
  destinations. Startup recovery handles journals one at a time; a busy shard
  leaves that journal pending instead of racing a live hook.

A shard collision may temporarily suppress inference for an unrelated
request. That is an intentional bounded availability tradeoff: the affected
hook preserves native confirmation. Codex and Claude emit no hook decision;
Antigravity returns `ask`. The fixed shard set avoids unbounded lock-file
growth.

A hook that does not acquire the shard performs no lifecycle, journal, or
activity mutation for that request. The shard winner determines the one
eligible response; a later conflicting request cannot retroactively replace
that response without waiting, which this design intentionally forbids.

Once the shard guard is held, a deny response remains available if later
transaction persistence fails. The hook attempts monotonic `NeedsInput`, never
`Decided`, and does not claim a transaction-backed `Denied` terminal unless
the deny transaction actually verified.

## Bounded Live Recovery

Startup recovery and live-hook recovery use different budgets.

The live hook:

- discovers and recovers at most one journal;
- accepts at most 1 MiB of journal data;
- enforces a 16 MiB combined budget across activity and decision evidence;
- checks destination metadata before inference and enforces the same combined
  budget with bounded readers during every scan and final verification;
- uses nonblocking request-lock and transaction-directory lock acquisition;
- treats additional journals, oversized destinations, contention, invalid
  entries, active journals, unresolved recovery, or removal-sync uncertainty
  as admission failure; and
- emits no model-derived response while admission is blocked.

These are deterministic work bounds, not timeout margins. A blocked live hook
preserves native confirmation. Startup recovery retains the larger backlog
budget because it does not run inside a provider response deadline.

Crossing a destination-size limit deliberately disables model inference until
startup recovery or operator maintenance restores the store below the live
budget. Deterministic and provider-policy denials remain available. Doctor must
report the exact over-budget store and limit; the hook diagnostic remains
bounded and contains no state contents.

Current-transaction preparation first preflights and releases the directory
admission lock before inference, then uses one nonblocking acquisition when the
complete journal is available. The global directory lock is never held across
inference. Destination-size preflight occurs before model evaluation; bounded
readers enforce the budget again after inference so concurrent log growth
cannot bypass it. If contention or growth appears after inference, allow is
suppressed and the transaction is retained or compensated fail-closed.

## Lifecycle Admission

Permission admission distinguishes lifecycle conditions explicitly:

- `Missing`: clean bootstrap with no existing decision;
- `Healthy`: query the exact `PermissionDecision`;
- `Corrupt`, `NewerSchema`, `Unavailable`, lock error, or I/O error: bounded
  admission failure.

On admission failure, Codex and Claude emit no allow; Antigravity returns
`ask`. Inference is not invoked. Diagnostics contain condition labels and
fixed operation names, never raw lifecycle or journal bytes.

Permission hooks do not quarantine, replace, or reinitialize corrupt/newer
lifecycle state and do not attempt fallback lifecycle writes against it. The
evidence remains intact for Doctor or startup handling. `Missing` is the only
clean bootstrap condition.

## Provider and Recovery Behavior

- Model allow: emitted only after exact `Allow` authority, proposal, terminal,
  and journal removal are verified durable.
- Deterministic/provider-policy/model deny: response remains available on audit
  failure after the request guard is acquired. A failed transaction records
  `NeedsInput` where possible and never leaves execution-capable authority.
- Request-lock admission failure: no executable hook response; Codex and Claude
  preserve native confirmation and Antigravity returns `ask`.
- Abstain/inference error: exact `NeedsInput`; no executable response. Inference
  errors retain bounded `Brain query failed:` reasoning in terminal `Error`.
- Recovery: never emits a provider response. Authority mismatch converts an
  allow journal to `Error`/`NeedsInput`; it never borrows authority from another
  transaction.
- Removal-sync uncertainty: suppresses allow and compensates to `NeedsInput` as
  already approved.

## Test Strategy

The remediation is test-driven and must cover:

1. Two same-key hooks with conflicting provider-policy deny and model-allow
   outcomes. Deterministic ordering proves that only the shard winner may
   evaluate, mutate state, or emit a response in either winner order; the
   loser preserves native confirmation.
2. A retained locked allow journal followed by a same-key deterministic deny.
   The deny hook cannot bypass the busy shard, and later recovery without exact
   allow authority records `NeedsInput`/`Error` without appending `Allowed`.
3. Exact authority idempotence and conflicts across transaction IDs and
   `Allow`/`Deny` actions.
4. Version 3 migration produces non-executable legacy decision evidence;
   version 4 is rejected by the old-schema fixture.
5. Same-shard contention returns promptly without inference; different shards
   can proceed independently.
6. Lock files reject symlinks, foreign ownership, wrong modes, multiple links,
   replacement, and malformed names without exposing raw paths.
7. Live recovery accepts one in-budget journal, rejects two journals, rejects
   oversized destinations, and performs no inference when blocked.
8. A held transaction-directory lock returns promptly and preserves native
   confirmation.
9. Corrupt and future-schema lifecycle snapshots invoke zero inference for all
   providers.
10. Destination files that exceed or grow beyond the combined 16 MiB live
    budget stop bounded reads, suppress allow, and retain/compensate recovery.
11. Lock and conflicting-response cases use separate subprocesses with
    deterministic pause points; killing a holder releases the kernel lock and
    never creates a replacement-inode split lock.
12. Existing permission-hook, transaction crash matrix, Antigravity exact-step,
    hook activity, lifecycle CLI, reproof, denial-availability, delivery, and
    full workspace gates remain green.

## Scope

Expected implementation scope is limited to lifecycle input/projection/store,
permission transaction storage/commit/recovery, permission hook integration,
and their focused integration tests. No provider response schema, unrelated
lifecycle state, UI behavior, release version, push, PR, or deployment is in
scope.

## Stress Test Results: Transaction-Bound Permission Authority

### Resolved Decisions

- Schema-3 bare `Decided` evidence is never execution-capable after migration.
- Lifecycle schema 4 intentionally fails closed on binary rollback; no
  automatic downgrade snapshot is created.
- 256 persistent nonblocking shards serialize response selection and recovery
  without timeout waits or unbounded lock-file growth.
- Existing lock shards may only narrow mode when the same current-user-owned,
  zero-length, single-link regular inode remains stable; all other anomalies
  fail closed.
- Live recovery handles one journal and enforces a 16 MiB combined destination
  evidence budget.
- The transaction-directory lock is preflighted and released before inference;
  preparation later acquires it once, nonblocking.
- Permission hooks preserve corrupt/newer lifecycle evidence for Doctor/startup
  and perform no fallback writes against it.
- No deterministic or provider-policy denial bypasses request-lock admission.
- A conflicting same-request loser performs no mutation or response; the
  nonblocking shard winner is authoritative for that invocation.
- Cross-process subprocess tests, including killed holders, are required for
  the locking contract.
- Bounded readers, not metadata alone, enforce destination budgets against
  concurrent growth.

### Changes Made

- Reduced the live destination budget from 32 MiB per log to 16 MiB combined.
- Removed executable denial responses when the request shard is unavailable.
- Added explicit recovery shard acquisition, corrupt-state preservation,
  nonblocking two-stage directory admission, subprocess crash tests, and
  bounded-read TOCTOU protection.

### Deferred / Parking Lot

- None. Every security and provider-deadline finding from Task 5 review is in
  scope for remediation before Task 5 can close.

### Confidence Assessment

- Overall: High
- Areas of concern: implementation must prove actual cross-process locking and
  schema rollback behavior; thread-only tests are insufficient.
