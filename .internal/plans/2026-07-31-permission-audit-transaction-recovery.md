# Permission Audit Transaction Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make permission proposal, lifecycle, and terminal-activity persistence recoverable across process interruption while presenting legacy stale evaluations as incomplete rather than interrupted tools.

**Architecture:** Add an immutable, per-request write-ahead journal under the trusted state root. Stable decision and activity identities make destination writes idempotent; recovery validates lifecycle authority before completing an allow and never replays a provider response. Keep the journal engine in a focused binary module, with narrow idempotency and authority-query primitives in the existing stores.

**Tech Stack:** Rust 2024 workspace, serde/serde_json, fs2 file locks, existing `durable_replace`, Cargo test fixtures, ratatui TUI.

## Global Constraints

- Correctness must not depend on the 25-second inference timeout leaving spare time inside the managed 30-second provider deadline.
- The guarantee is recoverable consistency, not atomic visibility across independent files.
- The provider receives no model-derived allow until proposal, exact lifecycle authority, and terminal activity verify durable.
- Recovery never emits or replays a provider response.
- Deterministic, provider-policy, and model denials remain available even when audit persistence fails.
- A proposal or stale evaluation never becomes execution or outcome authority.
- `PostToolUse` correlation continues to require the existing eligible terminal `Allowed` event.
- Journals contain only bounded, redacted destination data and no caller-controlled destination paths.
- The transaction directory is owner-only; invalid or uncertain journals remain for diagnosis unless unlink has already succeeded and only its directory sync is uncertain, which has a dedicated non-pending outcome.
- `Incomplete` is projection-only and must be rejected as an `ActivityEvent` persistence state.
- Existing decision, lifecycle, and activity schemas remain readable without migration.
- Commit steps below require explicit user authorization; without it, stop after the task's verification step and report the verified diff.

## File Structure

- Create `src/brain/permission_transaction.rs`: immutable journal schema, filesystem validation and locking, bounded discovery, idempotent commit/recovery, and recovery reports.
- Modify `src/brain/mod.rs`: register the new focused module.
- Modify `src/brain/decisions.rs`: prepare stable owned hook records and atomically ensure an exact decision record by ID.
- Modify `src/brain/activity.rs`: atomically ensure an exact terminal activity while allowing existing non-terminal rows with the same activity ID; project stale permission evaluations as `Incomplete`.
- Modify `crates/coding-brain-core/src/brain_activity.rs`: add projection state `Incomplete` and reject it from persisted event payloads.
- Modify `crates/coding-brain-core/src/lifecycle/projection.rs`: expose the effective permission disposition for an exact request key.
- Modify `crates/coding-brain-core/src/lifecycle/store.rs`: query exact request-key lifecycle authority through the existing shared lock.
- Modify `src/brain/permission_hook.rs`: use bounded admission recovery and the new transaction commit path while preserving fail-safe denials.
- Modify `src/runtime/brain.rs`: recover pending permission transactions before reading an activity snapshot.
- Modify `src/doctor.rs`: report pending, active, invalid, over-budget, removal-sync-uncertain, and unrecoverable permission transactions.
- Modify `crates/coding-brain-tui/src/ui/brain/live.rs`: render `INCOMPLETE` and `permission evaluation timed out`.
- Modify `tests/hook_activity.rs`: real-binary crash/recovery and multi-process permission regressions.

---

### Task 1: Projection-Only Incomplete Permission State

**Files:**
- Modify: `crates/coding-brain-core/src/brain_activity.rs`
- Modify: `src/brain/activity.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`

**Interfaces:**
- Consumes: Existing `ActivityEvent`, `ActivityItem`, `ActivityState`, `project_activity`, and TUI badge/status functions.
- Produces: `ActivityState::Incomplete`, usable by projected `ActivityItem` values but rejected by `ActivityEvent::has_consistent_payload`.

**Acceptance Criteria:**
- Stale `Observed`/`Evaluating` decision activity projects to `Incomplete`, remains in Needs Attention, and does not rewrite the source log.
- Persisting an `ActivityEvent` with `Incomplete` fails validation.
- Live renders badge `INCOMPLETE` and outcome `permission evaluation timed out`; it contains neither `STOPPED` nor an interrupted tool outcome.
- All exhaustive state matches and existing serialized activity fixtures remain valid.

- [ ] **Step 1: Write failing core and projection tests**

Add tests beside `ActivityEvent::has_consistent_payload` and
`stale_evaluating_projects_as_interrupted_without_rewriting_source`:

```rust
#[test]
fn incomplete_is_projection_only() {
    let mut event = event("cargo test", "reason", "note");
    event.state = ActivityState::Incomplete;
    assert!(!event.has_consistent_payload());
}

#[test]
fn stale_evaluating_projects_as_incomplete_without_rewriting_source() {
    let (root, store) = fixture_store();
    let store = store.with_clock(1_000);
    store.append(event_at("a1", ActivityState::Observed, 100)).unwrap();
    store.append(event_at("a1", ActivityState::Evaluating, 101)).unwrap();

    let snapshot = store
        .snapshot(SnapshotLimits {
            interrupted_after_ms: 100,
            ..SnapshotLimits::default()
        })
        .unwrap();

    assert_eq!(snapshot.attention[0].state, ActivityState::Incomplete);
    assert_eq!(store.read().unwrap().events().len(), 2);
    drop(root);
}
```

- [ ] **Step 2: Write failing TUI rendering tests**

Extend the existing state-label and badge tables:

```rust
let mut item = activity();
item.state = ActivityState::Incomplete;
item.delivery = DeliveryState::NotApplicable;
assert_eq!(activity_badge(&item).label, "INCOMPLETE");
assert_eq!(
    activity_status(&item),
    "permission evaluation timed out"
);
```

Also render the evidence panel and assert it does not contain `STOPPED` or
`interrupted`.

- [ ] **Step 3: Run the focused tests and confirm the missing variant failure**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core incomplete_is_projection_only
nix develop path:. --command cargo test stale_evaluating_projects_as_incomplete_without_rewriting_source
nix develop path:. --command cargo test -p coding-brain-tui incomplete
```

Expected: compilation/test failure because `ActivityState::Incomplete` and its
rendering do not exist.

- [ ] **Step 4: Implement the projection-only state**

Add the enum variant and prohibit its persistence:

```rust
pub enum ActivityState {
    Observed,
    Evaluating,
    Allowed,
    Denied,
    Abstained,
    Error,
    Delivered,
    DeliveryFailed,
    Outcome,
    Correction,
    Interrupted,
    Incomplete,
}

pub fn has_consistent_payload(&self) -> bool {
    if self.state == ActivityState::Incomplete {
        return false;
    }
    // existing validation follows unchanged
}
```

In `project_activity`, replace only the stale non-terminal projection:

```rust
if terminal.is_none()
    && matches!(state, ActivityState::Observed | ActivityState::Evaluating)
    && now_ms.saturating_sub(latest_at) > stale_after_ms
{
    state = ActivityState::Incomplete;
}
```

Include `Incomplete` in Needs Attention and ranking at the former
`Interrupted` rank. Add TUI matches:

```rust
ActivityState::Incomplete => "permission evaluation timed out",
ActivityState::Incomplete => ActivityBadge::new("INCOMPLETE", BadgeTone::Warning),
```

Keep `Interrupted` rendering unchanged for genuine existing interruption
states.

- [ ] **Step 5: Run focused tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core incomplete_is_projection_only
nix develop path:. --command cargo test stale_evaluating_projects_as_incomplete_without_rewriting_source
nix develop path:. --command cargo test -p coding-brain-tui
```

Expected: all pass.

- [ ] **Step 6: Commit after explicit authorization**

```bash
git add crates/coding-brain-core/src/brain_activity.rs src/brain/activity.rs crates/coding-brain-tui/src/ui/brain/live.rs
git commit -m "🐛 fix: distinguish incomplete permission evaluations (codexctl-ug26)"
```

---

### Task 2: Exact Destination Idempotency and Lifecycle Authority

**Files:**
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/activity.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Consumes: `HookDecisionAudit`, decision-store lock, `ActivityStore`, `LifecycleStore`, and request-key permission bits.
- Produces:
  - `HookDecisionRecord::from_audit(audit, decision_id, user_action)`.
  - `ensure_hook_record_at(path, record) -> io::Result<EnsureRecord>`.
  - `ActivityStore::ensure_terminal(event) -> Result<EnsureRecord, ActivityStoreError>`.
  - `LifecycleStore::permission_disposition(identity, request_key) -> Result<Option<PermissionDisposition>, StoreError>`.
  - `EnsureRecord::{Inserted, Present}`; exact-ID conflicts return an error.

**Acceptance Criteria:**
- A stable hook decision record can be prepared without writing.
- Ensuring the same exact proposal or terminal twice produces one row.
- Reusing an ID with different content fails visibly and preserves the original row.
- Existing `Observed`/`Evaluating` rows do not prevent ensuring the exact terminal for their activity ID.
- Lifecycle authority returns `Decided` only for the exact provider-qualified session, turn, and request key; a later `NeedsInput` compensation wins.

- [ ] **Step 1: Write failing decision-store tests**

```rust
#[test]
fn ensure_hook_record_is_idempotent_and_rejects_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("decisions.jsonl");
    let record = hook_record("decision-1", "approve");

    assert_eq!(
        ensure_hook_record_at(&path, &record).unwrap(),
        EnsureRecord::Inserted
    );
    assert_eq!(
        ensure_hook_record_at(&path, &record).unwrap(),
        EnsureRecord::Present
    );

    let conflicting = hook_record("decision-1", "deny");
    assert!(ensure_hook_record_at(&path, &conflicting).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
}
```

- [ ] **Step 2: Write failing activity terminal tests**

```rust
#[test]
fn ensure_terminal_ignores_initial_rows_but_rejects_terminal_conflict() {
    let (_root, store) = fixture_store();
    store.append(event("a1", ActivityState::Observed)).unwrap();
    store.append(event("a1", ActivityState::Evaluating)).unwrap();
    let mut allowed = event("a1", ActivityState::Allowed);
    allowed.decision_id = Some("decision-1".into());

    assert_eq!(store.ensure_terminal(allowed.clone()).unwrap(), EnsureRecord::Inserted);
    assert_eq!(store.ensure_terminal(allowed).unwrap(), EnsureRecord::Present);

    let mut denied = event("a1", ActivityState::Denied);
    denied.decision_id = Some("decision-1".into());
    assert!(store.ensure_terminal(denied).is_err());
}
```

- [ ] **Step 3: Write failing lifecycle authority tests**

```rust
#[test]
fn permission_disposition_is_exact_and_compensation_wins() {
    let store = fixture_lifecycle_store();
    let identity = open_turn(&store, "session-1", "turn-1");
    assert_eq!(
        store.permission_disposition(&identity, &"a".repeat(64)).unwrap(),
        None
    );

    record_permission(&store, identity.clone(), PermissionDisposition::Decided, "a");
    assert_eq!(
        store.permission_disposition(&identity, &"a".repeat(64)).unwrap(),
        Some(PermissionDisposition::Decided)
    );

    record_permission(&store, identity.clone(), PermissionDisposition::NeedsInput, "a");
    assert_eq!(
        store.permission_disposition(&identity, &"a".repeat(64)).unwrap(),
        Some(PermissionDisposition::NeedsInput)
    );
    assert_eq!(
        store.permission_disposition(&identity, &"b".repeat(64)).unwrap(),
        None
    );
}
```

- [ ] **Step 4: Run tests and confirm missing APIs**

Run:

```bash
nix develop path:. --command cargo test ensure_hook_record_is_idempotent_and_rejects_conflict
nix develop path:. --command cargo test ensure_terminal_ignores_initial_rows_but_rejects_terminal_conflict
nix develop path:. --command cargo test -p coding-brain-core permission_disposition_is_exact_and_compensation_wins
```

Expected: compilation failure for the new types and methods.

- [ ] **Step 5: Implement owned records and exact ensure operations**

Define an owned serde record matching the existing JSON shape:

```rust
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct HookDecisionRecord {
    pub provider: AgentProvider,
    pub ts: String,
    pub pid: u32,
    pub project: String,
    pub tool: String,
    pub command: String,
    pub brain_action: String,
    pub brain_confidence: f64,
    pub brain_reasoning: String,
    pub brain_source: String,
    pub brain_threshold: Option<f64>,
    pub user_action: String,
    pub decision_type: String,
    pub suggested_at: u64,
    pub resolved_at: u64,
    pub decision_id: String,
    pub session_id: String,
    pub turn_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnsureRecord {
    Inserted,
    Present,
}
```

Under the existing decision lock, scan bounded JSONL records for the same
`decision_id`; return `Present` only for exact equality, error on conflict, and
otherwise append/flush/sync the prepared record.

Implement `ActivityStore::ensure_terminal` under its exclusive lock. Normalize
and validate the target, require `state.is_terminal()`, ignore non-terminal rows
with the same activity ID, return `Present` for an exact existing terminal, and
return `InvalidEvent` for a different first terminal.

Expose effective request-key permission evidence from
`SessionLifecycleState`:

```rust
pub fn permission_disposition(&self, request_key: &str) -> Option<PermissionDisposition> {
    let bits = self.permission_request_events.get(request_key).copied()?;
    if bits & PERMISSION_NEEDS_INPUT_BIT != 0 {
        Some(PermissionDisposition::NeedsInput)
    } else if bits & PERMISSION_DECIDED_BIT != 0 {
        Some(PermissionDisposition::Decided)
    } else {
        None
    }
}
```

`LifecycleStore::permission_disposition` reads under the shared lock, resolves
the exact native `AgentSessionKey`, verifies provider-session identity, cwd,
and current turn against `LifecycleIdentity`, then delegates to the state
method.

- [ ] **Step 6: Run focused and compatibility tests**

Run:

```bash
nix develop path:. --command cargo test ensure_hook_record_is_idempotent_and_rejects_conflict
nix develop path:. --command cargo test ensure_terminal_ignores_initial_rows_but_rejects_terminal_conflict
nix develop path:. --command cargo test -p coding-brain-core permission_disposition
nix develop path:. --command cargo test activity_store
nix develop path:. --command cargo test decisions
```

Expected: all pass.

- [ ] **Step 7: Commit after explicit authorization**

```bash
git add src/brain/decisions.rs src/brain/activity.rs crates/coding-brain-core/src/lifecycle/projection.rs crates/coding-brain-core/src/lifecycle/store.rs
git commit -m "♻️ refactor: add exact permission audit persistence primitives (codexctl-ug26)"
```

---

### Task 3: Immutable Permission Transaction Journal

**Files:**
- Create: `src/brain/permission_transaction.rs`
- Modify: `src/brain/mod.rs`

**Interfaces:**
- Consumes: `durable_replace`, `HookDecisionRecord`, `ActivityEvent`, `LifecycleIdentity`, `PermissionDisposition`, and fs2 locking.
- Produces:
  - `PermissionTransactionJournal`.
  - `PermissionTransactionStore::at(state_root)`.
  - `prepare(journal) -> Result<PreparedTransaction, TransactionError>`.
  - `discover(limit) -> Result<(Vec<RecoverableTransaction>, RecoveryReport), TransactionError>`.
  - `RecoveryLimits { max_journals: 256, max_total_bytes: 16 * 1024 * 1024 }`.
  - `RecoveryReport { completed, active, invalid, over_budget, pending, removal_sync_uncertain }`.

**Acceptance Criteria:**
- Preparation writes and fsyncs a unique temporary file, locks its inode, atomically renames it to an immutable final journal, and fsyncs the mode-`0700` parent directory.
- The creator holds an exclusive journal lock until `PreparedTransaction` completes or drops.
- Discovery is oldest-first, bounded by 256 files and 16 MiB, and non-blockingly skips active locked journals.
- Symlinks, hard links, non-regular files, wrong ownership where supported, unsupported schemas, oversized payloads, inconsistent IDs, and destination-path fields are rejected and retained.
- An unlocked, validated generated temporary file is removable because no destination writes occur before publication; locked or invalid temporary files remain untouched.
- Journal preparation and discovery never expose raw record contents in errors.

- [ ] **Step 1: Write failing journal preparation and locking tests**

```rust
#[test]
fn prepared_journal_is_private_immutable_and_locked() {
    let temp = tempfile::tempdir().unwrap();
    let store = PermissionTransactionStore::at(temp.path());
    let prepared = store.prepare(journal("tx-1")).unwrap();

    assert_eq!(prepared.journal(), &journal("tx-1"));
    assert_eq!(store.discover(RecoveryLimits::default()).unwrap().1.active, 1);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(prepared.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(prepared);
    assert_eq!(store.discover(RecoveryLimits::default()).unwrap().0.len(), 1);
}
```

- [ ] **Step 2: Write failing bounded and adversarial discovery tests**

Cover an over-count directory, total-byte overflow, symlink, hard link,
directory entry, unsupported schema, mismatched decision IDs, and wrong owner
where the test runner can create one. Each test asserts `invalid` or
`over_budget`, asserts no journal deletion, and asserts no parsed raw command in
the error text.

Add two preparation-interruption cases: an unlocked, valid
`permission-transaction.tmp-*` file is cleaned without destination writes, while
a locked temporary file is counted active and retained.

- [ ] **Step 3: Run tests and confirm the module is missing**

Run:

```bash
nix develop path:. --command cargo test permission_transaction::tests
```

Expected: compilation failure because the module and types do not exist.

- [ ] **Step 4: Implement the bounded immutable journal**

Use an exact schema:

```rust
const JOURNAL_SCHEMA_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PermissionTransactionJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub proposal: HookDecisionRecord,
    pub terminal: ActivityEvent,
    pub lifecycle_identity: LifecycleIdentity,
    pub request_key: String,
    pub disposition: PermissionDisposition,
    pub allow_requires_lifecycle_authority: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecoveryLimits {
    pub max_journals: usize,
    pub max_total_bytes: usize,
}
```

Validate agreement among transaction, decision, activity, provider, session,
turn, tool, project, and decision IDs before writing. Derive
`state_root/brain/permission-transactions` internally; do not deserialize any
path.

Prepare a unique sibling temporary with `create_new`, mode `0600`, and
no-follow semantics on Unix. Serialize/write/sync, validate `fstat`, acquire an
exclusive fs2 lock on that open file, atomically rename the locked inode to its
unique final `.json` name, and sync the parent directory.
`PreparedTransaction` retains that same open locked file and removes/syncs the
journal only through an explicit `complete(self)` method. Do not close and
reopen between rename and commit.

Discovery reads directory metadata without following links, sorts by filename
creation identity, enforces count/byte bounds before parsing, opens and
validates the final file handle, then uses `try_lock_exclusive`. `WouldBlock`
increments `active`; invalid files increment `invalid` and remain untouched.
Generated temporary files use the same metadata and lock validation: a locked
temp is active, an unlocked valid temp is removed and the directory synced, and
an invalid temp remains diagnostic evidence.

- [ ] **Step 5: Run journal tests**

Run:

```bash
nix develop path:. --command cargo test permission_transaction::tests
```

Expected: all journal preparation, lock, validation, and bound tests pass.

- [ ] **Step 6: Commit after explicit authorization**

```bash
git add src/brain/mod.rs src/brain/permission_transaction.rs
git commit -m "✨ feat: add immutable permission transaction journals (codexctl-ug26)"
```

---

### Task 4: Idempotent Commit and Fail-Closed Recovery

**Files:**
- Modify: `src/brain/permission_transaction.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Consumes: Task 2 ensure/query primitives and Task 3 locked journals.
- Produces:
  - `commit(prepared, lifecycle_store, activity_store, decisions_path) -> Result<CommitReport, TransactionError>`.
  - `recover_pending(state_root, limits) -> Result<RecoveryReport, TransactionError>`.
  - `RecoveryDisposition::{Completed, Active, Invalid, Unresolved, RemovalSyncUncertain}`.

**Acceptance Criteria:**
- Every interruption boundary recovers to exactly one proposal, one authoritative terminal event, and the correct effective lifecycle disposition.
- Repeating recovery is a no-op.
- An allow terminal is completed only when the exact request-key lifecycle disposition is `Decided`.
- Missing, ambiguous, unreadable, or compensated allow authority becomes `Error`/`NeedsInput`; recovery never emits a response.
- Deny, abstain, and inference-error terminals roll forward without creating positive authority.
- Journal removal occurs only after exact destination rereads verify completion.
- A directory-sync failure after successful unlink is reported as typed, non-pending `RemovalSyncUncertain`; allow remains suppressed and lifecycle becomes `NeedsInput`.

- [ ] **Step 1: Write a table-driven failing crash matrix**

Add a test-only fault enum:

```rust
#[cfg(test)]
#[derive(Clone, Copy)]
enum CommitFault {
    None,
    AfterPrepare,
    AfterProposal,
    AfterLifecycle,
    AfterTerminal,
    BeforeJournalRemoval,
}
```

For every fault, invoke `commit_with_fault`, drop the locked transaction, call
`recover_pending` twice, and assert one exact proposal, one exact terminal,
effective lifecycle disposition, and zero remaining journals.

- [ ] **Step 2: Write failing allow-authority tests**

```rust
#[test]
fn recovery_never_reconstructs_allow_without_exact_lifecycle_authority() {
    let fixture = transaction_fixture(ActivityState::Allowed);
    fixture.persist_journal_and_proposal();

    let report = fixture.recover().unwrap();

    assert_eq!(report.completed, 1);
    assert_eq!(
        fixture.terminal_states(),
        [ActivityState::Error]
    );
    assert_eq!(
        fixture.permission_disposition(),
        Some(PermissionDisposition::NeedsInput)
    );
}

#[test]
fn recovery_completes_undelivered_allow_with_exact_authority() {
    let fixture = transaction_fixture(ActivityState::Allowed);
    fixture.persist_journal_proposal_and_decided_lifecycle();

    fixture.recover().unwrap();

    assert_eq!(fixture.terminal_states(), [ActivityState::Allowed]);
    assert!(!fixture.has_delivery_event());
}
```

- [ ] **Step 3: Run tests and confirm recovery is unimplemented**

Run:

```bash
nix develop path:. --command cargo test permission_transaction::tests::crash
nix develop path:. --command cargo test permission_transaction::tests::recovery
```

Expected: compilation/test failure for missing commit/recovery behavior.

- [ ] **Step 4: Implement commit and recovery**

Commit while holding the journal lock:

```rust
ensure_hook_record_at(decisions_path, &journal.proposal)?;
ensure_lifecycle(lifecycle_store, journal)?;
ensure_terminal_for_authority(activity_store, lifecycle_store, journal)?;
verify_destinations(decisions_path, lifecycle_store, activity_store, journal)?;
prepared.complete()?;
```

For `Allowed`, `ensure_terminal_for_authority` queries the exact lifecycle
request. `Decided` permits the journal's `Allowed` terminal. Any other result
constructs an `Error` terminal with the same activity and decision IDs,
bounded reasoning `permission transaction recovery lacked executable lifecycle authority`,
records `NeedsInput`, and ensures that error instead.

For deny/abstain/error journals, ensure the intended lifecycle disposition and
terminal without creating positive authority. Treat exact already-present
destinations as success and identity conflicts as unresolved errors that retain
the journal.

Verification rereads all three destinations. Do not rely on an in-memory return
value from a write. Only after exact verification call `PreparedTransaction::complete`.
If unlink succeeds but its following directory sync fails, return the typed
`RemovalSyncUncertain` error. Recovery increments
`RecoveryReport.removal_sync_uncertain`, not `pending`; the hook must suppress
allow and Task 6 must surface the uncertainty as a failing diagnostic. A crash
may make the name reappear, in which case ordinary recovery remains
idempotent and fail closed.

- [ ] **Step 5: Run crash, authority, and idempotency tests**

Run:

```bash
nix develop path:. --command cargo test permission_transaction
nix develop path:. --command cargo test -p coding-brain-core permission_disposition
```

Expected: all pass.

- [ ] **Step 6: Commit after explicit authorization**

```bash
git add src/brain/permission_transaction.rs crates/coding-brain-core/src/lifecycle/store.rs
git commit -m "✨ feat: recover permission audit transactions safely (codexctl-ug26)"
```

---

### Task 5: Permission Hook Transaction Integration

**Files:**
- Modify: `src/brain/permission_hook.rs`
- Modify: `src/brain/decisions.rs`
- Test: `src/brain/permission_hook.rs`

**Interfaces:**
- Consumes: `recover_pending`, `PermissionTransactionStore::prepare`, transaction `commit`, stable owned decision records, and existing provider response adapters.
- Produces: Transaction-backed model evaluation persistence before response delivery; bounded admission failure that preserves native confirmation.

**Acceptance Criteria:**
- The hook performs bounded prior recovery before inference; unresolved prior transactions prevent model-derived allow.
- Every model proposal and terminal event share stable IDs from one immutable journal.
- The hook emits model allow only after the transaction verifies and journal cleanup succeeds durably; `RemovalSyncUncertain` suppresses allow and preserves native confirmation.
- Inference timeout/error is durably represented as terminal `Error`.
- Deterministic, provider-policy, and model deny responses remain available when journal or audit persistence fails.
- Antigravity persistence failures return `ask`; Codex/Claude emit no allow and preserve native confirmation.
- Existing reproof, lifecycle compensation, delivery evidence, and correlation rules remain intact.

- [ ] **Step 1: Replace the proposal-only regression with failing transaction expectations**

Change `model_terminal_failure_abstains_with_proposal_only` into:

```rust
#[test]
fn model_terminal_failure_retains_recoverable_transaction() {
    let fixture = permission_fixture("approve");
    fixture.fail_after_proposal();

    let output = fixture.run();

    assert!(output.stdout.is_empty());
    assert!(output.stderr_text().contains("permission transaction"));
    assert_eq!(fixture.pending_transactions(), 1);
    fixture.recover().unwrap();
    assert_eq!(fixture.proposals(), 1);
    assert_eq!(fixture.terminal_states(), [ActivityState::Error]);
}
```

Add an inference-error case asserting the recovered terminal reasoning contains
the bounded `Brain query failed:` diagnostic.

- [ ] **Step 2: Add failing response-gating and denial-availability tests**

Cover:

- model allow plus journal creation failure: no allow output;
- model allow plus destination conflict: no allow output;
- Antigravity model allow persistence failure: `ask`;
- deterministic safety deny plus journal failure: deny response still emitted;
- provider-policy deny plus journal failure: deny response still emitted; and
- model deny plus journal failure: deny response still emitted with diagnostic.

- [ ] **Step 3: Add failing bounded-admission test**

Seed an invalid or over-budget prior journal. Assert inference is not called,
Codex/Claude output is empty, Antigravity outputs `ask`, and stderr contains a
bounded recovery diagnostic without raw journal content.

- [ ] **Step 4: Run focused tests and confirm the old direct path fails**

Run:

```bash
nix develop path:. --command cargo test brain::permission_hook
```

Expected: new transaction assertions fail against direct proposal/activity
appends.

- [ ] **Step 5: Integrate transaction preparation and commit**

At hook entry, run bounded recovery before safety/model evaluation. Active,
invalid, over-budget, unresolved, or removal-sync-uncertain prior transactions
set a persistence error that prevents model inference and model allow.

After evaluation and serialization:

```rust
let decision_id = decisions::gen_decision_id();
let proposal = HookDecisionRecord::from_audit(
    &audit,
    decision_id.clone(),
    proposal_action,
);
terminal.decision_id = Some(decision_id);
let journal = PermissionTransactionJournal::new(
    proposal,
    terminal,
    request.lifecycle.clone(),
    request.request_key.clone(),
    intended_disposition,
    behavior == Some(PermissionBehavior::Allow),
)?;
let prepared = transaction_store.prepare(journal)?;
commit(prepared, lifecycle_store, activity_store, decisions_path)?;
```

Write a model-derived allow only after commit returns verified success. Append
delivery evidence afterward as today. Preserve Codex reproof checks before
positive lifecycle authority.

Route deterministic/provider-policy/model denial through the transaction when
available, but retain the deny response if preparation or commit fails. Never
reuse that exception for allow.

- [ ] **Step 6: Run permission-hook and correlation regressions**

Run:

```bash
nix develop path:. --command cargo test brain::permission_hook
nix develop path:. --command cargo test --test hook_activity
nix develop path:. --command cargo test --test lifecycle_hook_cli
```

Expected: all pass, including denial availability and no proposal-only terminal
failure expectation.

- [ ] **Step 7: Commit after explicit authorization**

```bash
git add src/brain/permission_hook.rs src/brain/decisions.rs
git commit -m "🐛 fix: commit permission audits through recovery journals (codexctl-ug26)"
```

---

### Task 6: Startup Recovery and Doctor Visibility

**Files:**
- Modify: `src/runtime/brain.rs`
- Modify: `src/doctor.rs`
- Modify: `src/brain/permission_transaction.rs`

**Interfaces:**
- Consumes: `recover_pending` and `RecoveryReport`.
- Produces:
  - Activity refresh that attempts bounded recovery before reading stores.
  - Doctor check `Permission transaction recovery`.

**Acceptance Criteria:**
- Brain refresh attempts recovery before reading decisions/activity and never presents a known recoverable proposal-only state.
- Active journals are reported without being stolen from live hooks.
- Invalid, over-budget, and unrecoverable journals make Doctor fail with a bounded fix hint.
- Removal-sync-uncertain outcomes make Doctor fail without claiming a discoverable pending journal.
- A clean store adds no failure; recovered journals report the completed count without leaking content.
- Rollback readiness is equivalent to zero pending, active, invalid, over-budget, unresolved, or removal-sync-uncertain transactions.

- [ ] **Step 1: Write failing runtime refresh tests**

Seed a journal interrupted after proposal. Call
`LiveBrainSource::refresh_from_store` through a test helper that accepts
`state_root`; assert recovery runs before snapshot projection and the resulting
activity is terminal `Error`, not `Incomplete`.

Seed an actively locked journal and assert refresh does not steal or delete it.

- [ ] **Step 2: Write failing Doctor tests**

```rust
#[test]
fn permission_transaction_recovery_failure_is_failing() {
    let check = permission_transaction_recovery_check(RecoveryReport {
        invalid: 1,
        pending: 1,
        ..RecoveryReport::default()
    })
    .unwrap();
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(!check.message.contains("secret"));
}
```

Add clean, recovered, active, and over-budget report cases.
Add a removal-sync-uncertain case that fails with a bounded message and does not
claim that a pending journal exists.

- [ ] **Step 3: Run tests and confirm startup/Doctor integration is absent**

Run:

```bash
nix develop path:. --command cargo test runtime::brain
nix develop path:. --command cargo test doctor
```

Expected: new tests fail.

- [ ] **Step 4: Integrate bounded recovery**

Change `LiveBrainSource::refresh_from_store` to receive the state root or a
transaction store, call bounded recovery before acquiring the activity-store
read lock, and then read/project the coherent destinations. Never invoke
recovery while an activity or lifecycle store lock is held.

Add `permission_transaction_recovery_check` next to the existing provider-hook
transaction check. It returns:

- no check for a clean store;
- Pass/Advisory with a bounded completed count after successful recovery;
- Advisory for active locked journals; and
- Fail for invalid, over-budget, pending-unresolved, removal-sync-uncertain, or recovery errors.

Run this check early in `run_all_checks`.

- [ ] **Step 5: Run runtime and Doctor tests**

Run:

```bash
nix develop path:. --command cargo test runtime::brain
nix develop path:. --command cargo test doctor
nix develop path:. --command cargo test public_namespace
```

Expected: all pass.

- [ ] **Step 6: Commit after explicit authorization**

```bash
git add src/runtime/brain.rs src/doctor.rs src/brain/permission_transaction.rs
git commit -m "✨ feat: recover and diagnose pending permission audits (codexctl-ug26)"
```

---

### Task 7: Real Process Interruption, Concurrency, and Full Gates

**Files:**
- Modify: `tests/hook_activity.rs`
- Modify if required by test seam only: `src/brain/permission_transaction.rs`
- Modify if required by test seam only: `src/brain/permission_hook.rs`

**Interfaces:**
- Consumes: Real `cbrain --permission-hook`, immutable journal files, recovery entry points, and existing hook fixture helpers.
- Produces: Deterministic subprocess crash and multi-process concurrency regressions covering OS lock release and durable recovery.

**Acceptance Criteria:**
- A real hook process paused after proposal persistence can be killed and recovered without a literal 25-second wait.
- Recovery produces exactly one proposal and one terminal timeout `Error`, then remains idempotent.
- Concurrent permission processes use unique journals and preserve complete independent lifecycles.
- Journal and directory modes are owner-only on Unix.
- Existing native confirmation and PostToolUse fail-closed correlation tests pass.
- Formatting, workspace tests, Clippy with warnings denied, and workspace build pass.

- [ ] **Step 1: Add a test-only subprocess fault seam**

Use a test-only environment variable accepted only by integration-test builds:

```rust
#[cfg(debug_assertions)]
fn pause_at_test_fault(point: &str) {
    if std::env::var("CBRAIN_TEST_PERMISSION_TX_FAULT").as_deref() == Ok(point) {
        write_readiness_marker(point);
        wait_for_test_release_or_process_kill();
    }
}
```

The marker path is another test-only environment value under the fixture temp
directory. Production release behavior must not read or honor these variables.

- [ ] **Step 2: Write the real process-kill regression**

Spawn the real permission hook with an inference fixture that returns the exact
curl timeout diagnostic and pauses after proposal persistence. Wait for the
readiness marker with the existing bounded polling helper, assert the journal
and proposal exist, kill/wait the child, then invoke a recovery entry point.

Assert:

```rust
assert_eq!(proposal_count(home.path()), 1);
assert_eq!(
    permission_states(home.path()),
    [
        ActivityState::Observed,
        ActivityState::Evaluating,
        ActivityState::Error,
    ]
);
assert!(terminal_reason(home.path()).contains("Brain query failed:"));
assert_eq!(pending_transaction_count(home.path()), 0);
```

Run recovery again and repeat the counts.

- [ ] **Step 3: Write the multi-process concurrency regression**

Launch multiple real permission hooks with distinct requests, hold each after
journal preparation, assert distinct journal filenames, release them together,
and wait with a five-second parent-side deadline. Assert one proposal and one
terminal lifecycle per request, no invalid journals, and zero pending journals.

- [ ] **Step 4: Run real-binary regressions repeatedly**

Run:

```bash
nix develop path:. --command cargo test --test hook_activity permission_transaction -- --nocapture
for i in 1 2 3 4 5; do
  nix develop path:. --command cargo test --test hook_activity parallel_permission_transactions
done
```

Expected: every run passes without sleeps, lock timeouts, duplicate records, or
leftover journals.

- [ ] **Step 5: Run repository quality gates serially**

Run:

```bash
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
git status --short
```

Expected: all commands exit 0. Status contains only the approved ug26 plan/spec
and implementation files; no unrelated artifacts are staged.

- [ ] **Step 6: Update Beads and commit after explicit authorization**

Close child tasks only with their focused verification evidence. Close
`codexctl-ug26` only after every acceptance criterion and full gate passes.

If commit authorization is granted:

```bash
git add tests/hook_activity.rs src/brain/permission_transaction.rs src/brain/permission_hook.rs
git commit -m "✅ test: cover permission transaction crash recovery (codexctl-ug26)"
```

Do not push or publish without separate authorization.
