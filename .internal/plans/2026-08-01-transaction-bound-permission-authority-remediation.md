# Transaction-Bound Permission Authority Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent codexctl-fb9y`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Close all Task 5 review findings by making permission authority transaction/action-exact, serializing response selection without timeouts, bounding live-hook recovery work, and blocking inference on unreadable lifecycle state.

**Architecture:** Tasks 1 and 2 introduce exact authority APIs and transaction support while retaining lifecycle schema 3 and compatibility writers. Task 3 atomically removes execution-capable bare writers, migrates lifecycle state to schema 4, and cuts every hook over to `PermissionDecision::Decided(PermissionAuthority)`. A separate owner-only 256-shard lock store serializes same-shard hooks and recovery with one nonblocking acquisition. Live hooks use a one-journal, 1 MiB journal, 16 MiB combined destination budget enforced by a mutable reader-charged budget; startup recovery retains its larger budget.

**Tech Stack:** Rust 2024 workspace, serde JSON lifecycle snapshots, fs2/libc descriptor-relative filesystem operations, SHA-256 request hashing, Cargo/Nix tests.

## Global Constraints

- No hook writes execution-capable `Decided` authority outside a verified transaction.
- No executable hook response is selected without the request-shard guard.
- Lock acquisition is nonblocking; no timeout margin or retry loop is introduced.
- `NeedsInput` dominates every decided authority and can never be reversed.
- Schema-3 bare decisions migrate as non-executable legacy evidence.
- The schema-4 bump, legacy-writer removal, and hook cutover occur together in Task 3; intermediate Task 1/2 commits must not be published.
- Live recovery handles at most one journal, 1 MiB of journal data, and 16 MiB combined destination evidence.
- Every live destination scan uses a bounded reader; metadata checks alone are insufficient.
- Corrupt/newer lifecycle state is preserved and blocks inference without hook-side quarantine.
- Deterministic/provider/model deny remains available after guard acquisition even if later audit persistence fails.
- Recovery never emits provider responses.
- Changes remain local; do not push, publish, create a PR, or commit `.internal/sdd/`.

## File Structure

- `crates/coding-brain-core/src/lifecycle/input.rs`: permission action, authority, and decision wire/value types.
- `crates/coding-brain-core/src/lifecycle/projection.rs`: authority projection, monotonic state, and Task 3 schema-4 cutover.
- `crates/coding-brain-core/src/lifecycle/store.rs`: exact decision APIs, corrupt/newer query errors, and Task 3 schema-3 migration.
- `src/brain/permission_request_lock.rs`: fixed sharded nonblocking cross-process guard.
- `src/brain/mod.rs`: register the request-lock module.
- `src/brain/permission_transaction.rs`: journal schema, exact authority commit/recovery, bounded live mode, and nonblocking directory preparation.
- `src/brain/decisions.rs`: bounded decision-evidence scans used by live transactions.
- `src/brain/activity.rs`: bounded activity-evidence scans used by live transactions.
- `src/brain/permission_hook.rs`: guard acquisition, admission gating, exact transaction construction, and provider behavior.
- `tests/hook_activity.rs`: provider activity/audit behavior under bounded failure.
- `tests/lifecycle_hook_cli.rs`: corrupt/newer lifecycle and cross-process locking regressions.

---

### Task 1: Add transaction/action-bound lifecycle authority

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/input.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Produces: `PermissionAction::{Allow,Deny}`.
- Produces: `PermissionAuthority { transaction_id, action }`.
- Produces: `PermissionDecision::{NeedsInput,Decided(PermissionAuthority)}`.
- Produces: `LifecycleStore::permission_decision(identity, request_key)` and `ensure_permission_decision(identity, request_key, decision)`.
- Preserves: legacy disposition projection for display/diagnostics, but never as executable transaction authority.

**Acceptance Criteria:**
- Exact transaction/action authority types, projection maps, and store APIs exist without changing the current schema version or removing compatibility writers.
- Bare schema-3 `Decided` remains diagnostics/duplicate evidence only and cannot authorize a transaction through the new APIs.
- Same authority is idempotent; transaction/action mismatch conflicts.
- `NeedsInput` dominates and cannot reverse to `Decided`.
- Antigravity requires exact request key, exact step, exact child bits, and exact authority.
- Corrupt/newer lifecycle query returns a typed error; only missing state returns clean `None`.

- [ ] **Step 1: Write failing authority and migration tests**

Add focused tests in `store.rs` and `projection.rs` using exact helpers:

```rust
#[test]
fn permission_authority_is_transaction_action_exact_and_monotonic() {
    let allow_a = PermissionDecision::Decided(PermissionAuthority {
        transaction_id: "transaction-a".into(),
        action: PermissionAction::Allow,
    });
    let allow_b = PermissionDecision::Decided(PermissionAuthority {
        transaction_id: "transaction-b".into(),
        action: PermissionAction::Allow,
    });
    let deny_a = PermissionDecision::Decided(PermissionAuthority {
        transaction_id: "transaction-a".into(),
        action: PermissionAction::Deny,
    });

    assert_eq!(store.ensure_permission_decision(&identity, &key, allow_a.clone()).unwrap(), EnsurePermissionDecision::Inserted);
    assert_eq!(store.ensure_permission_decision(&identity, &key, allow_a.clone()).unwrap(), EnsurePermissionDecision::Present);
    assert_eq!(store.ensure_permission_decision(&identity, &key, allow_b), Err(StoreError::PermissionConflict));
    assert_eq!(store.ensure_permission_decision(&identity, &key, deny_a), Err(StoreError::PermissionConflict));
    store.ensure_permission_decision(&identity, &key, PermissionDecision::NeedsInput).unwrap();
    assert_eq!(store.permission_decision(&identity, &key).unwrap(), Some(PermissionDecision::NeedsInput));
    assert_eq!(store.ensure_permission_decision(&identity, &key, allow_a), Err(StoreError::PermissionConflict));
}
```

Add schema-3 tests asserting exact authority fields round-trip without changing
the current schema version and legacy bare decisions load with no executable
authority. Add corrupt and future-schema query tests expecting `InvalidSnapshot`
and `NewerSchema` rather than `Ok(None)`. The schema-4 migration tests belong to
Task 3 so the cutover is atomic with removal of legacy writers.

- [ ] **Step 2: Run the red tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core permission_authority
nix develop path:. --command cargo test -p coding-brain-core lifecycle_schema
```

Expected: compile/test failure because the authority types and exact store APIs
do not exist.

- [ ] **Step 3: Implement the exact lifecycle value types**

In `input.rs`, add serde-stable snake-case types and extend keyed permission
events with optional authority:

```rust
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction { Allow, Deny }

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionAuthority {
    pub transaction_id: String,
    pub action: PermissionAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionDecision {
    NeedsInput,
    Decided(PermissionAuthority),
}
```

Validate transaction IDs with the existing bounded ID rules. A serialized
`Decided` permission event must have authority; `NeedsInput` must not.

- [ ] **Step 4: Project exact authority while retaining schema 3**

Keep `LIFECYCLE_SCHEMA_VERSION` at `3`. Add a default, empty
`permission_authorities: BTreeMap<String, PermissionAuthority>` to session
state. Project exact keyed events atomically with permission bits and, for
Antigravity, the existing request-to-step map. Reject partial/mismatched exact
maps in snapshot validation, while treating an absent authority map from an
existing schema-3 snapshot as legacy non-executable evidence.

- [ ] **Step 5: Implement exact store APIs**

Implement:

```rust
pub fn permission_decision(
    &self,
    identity: &LifecycleIdentity,
    request_key: &str,
) -> Result<Option<PermissionDecision>, StoreError>;

pub fn ensure_permission_decision(
    &self,
    identity: &LifecycleIdentity,
    request_key: &str,
    decision: PermissionDecision,
) -> Result<EnsurePermissionDecision, StoreError>;
```

`Missing` returns `Ok(None)`; `Corrupt` returns `InvalidSnapshot`; newer schema
returns `NewerSchema`. Ensure rereads the full exact value after projection
before persistence. Preserve a separate non-executable legacy-disposition query
only where diagnostics/duplicate suppression require it.

Inventory every compatibility writer with `rg` and record the call sites that
Task 3 must remove or restrict. Keep those APIs source-compatible through Tasks
1 and 2, but ensure the new exact APIs never synthesize authority from them.

- [ ] **Step 6: Run focused and core gates**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core permission_authority
nix develop path:. --command cargo test -p coding-brain-core permission_disposition
nix develop path:. --command cargo test -p coding-brain-core antigravity_permission
nix develop path:. --command cargo test -p coding-brain-core lifecycle_schema
nix develop path:. --command cargo test -p coding-brain-core
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy -p coding-brain-core -- -D warnings
```

Expected: all exit 0.

- [ ] **Step 7: Commit, independently review, and remediate Task 1**

Create the local implementation commit:

```bash
git add crates/coding-brain-core/src/lifecycle/input.rs \
  crates/coding-brain-core/src/lifecycle/projection.rs \
  crates/coding-brain-core/src/lifecycle/store.rs
git commit -m "🔒 fix: bind lifecycle authority to permission transactions (codexctl-ug26)"
```

Then obtain a fresh independent review. Put each required fix in a separate
local remediation commit and obtain a fresh re-review before closing the Task 1
Bead. Do not publish any Task 1 commit.

---

### Task 2: Add nonblocking request serialization and bounded transaction mode

**Files:**
- Create: `src/brain/permission_request_lock.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/brain/permission_transaction.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/activity.rs`

**Interfaces:**
- Consumes: Task 1 `PermissionDecision` and `PermissionAuthority`.
- Produces: `PermissionRequestLockStore::at(state_root).try_acquire(identity, request_key) -> Result<Option<PermissionRequestGuard>, RequestLockError>`.
- Produces: `RecoveryLimits::live()` with one journal, 1 MiB journal bytes, and 16 MiB combined destination bytes.
- Produces: `LiveEvidenceBudget`, charged from bytes actually read across every scan and final verification in one recovery phase.
- Produces: live bounded decision/activity evidence readers and nonblocking journal preparation.
- Produces: journal-derived exact `PermissionDecision` for commit/recovery.

**Acceptance Criteria:**
- Same shard has one cross-process winner with one nonblocking attempt; different shards proceed independently.
- Persistent lock files cannot split through unlink, symlink, replacement, hard link, ownership, mode, or content attacks.
- Journal commit/recovery verifies exact transaction/action authority.
- Deny authority cannot complete allow; mismatch compensates allow to `NeedsInput`/`Error`.
- Live mode rejects more than one journal, more than 1 MiB journal data, or more than 16 MiB combined destination evidence.
- Every live evidence scan is reader-bounded against concurrent growth.
- Live directory preflight and preparation are nonblocking; the global directory lock is never held across inference.
- Request-lock storage is initialized and validated before recovery; any anomaly blocks before lifecycle, journal, or activity mutation.

- [ ] **Step 1: Write failing request-lock subprocess tests**

Add a test-only helper entry point that acquires a chosen shard, writes a ready
byte to a pipe/file, and waits on a release byte. Spawn separate test processes
to prove:

```rust
assert!(first_holder_acquires_same_shard());
assert_eq!(second_same_shard_try_acquire(), LockAttempt::Busy);
assert_eq!(different_shard_try_acquire(), LockAttempt::Acquired);
kill(first_holder_pid);
assert_eq!(same_shard_try_acquire_after_exit(), LockAttempt::Acquired);
```

Add anchored filesystem cases for mode narrowing to `0600`, symlink, foreign
owner, nonzero length, multiple links, inode replacement, and no unlink after
guard drop.

- [ ] **Step 2: Write failing authority and live-budget transaction tests**

Extend the crash matrix with exact authority assertions. Add regressions where
a deny token exists for the same request but an allow journal expects a
different action/transaction; recovery must append `Error`, ensure
`NeedsInput`, retain/remove the journal according to verified completion, and
never append `Allowed`.

Add live-mode tests for one versus two journals, a held directory lock, exactly
16 MiB combined evidence, one byte over budget, and a file that grows after
metadata preflight. Exercise one mutable `LiveEvidenceBudget` across every scan
and final verification in the phase; assert the typed `OverBudget` result occurs
before any reader consumes more than the remaining budget.

- [ ] **Step 3: Run the red tests**

Run:

```bash
nix develop path:. --command cargo test brain::permission_request_lock
nix develop path:. --command cargo test brain::permission_transaction::tests::exact_authority
nix develop path:. --command cargo test brain::permission_transaction::tests::live_
```

Expected: compile/test failures against the existing request-scoped and default
recovery implementation.

- [ ] **Step 4: Implement the fixed sharded lock store**

Use 256 names `permission-request-lock-00` through
`permission-request-lock-ff` beneath owner-only
`brain/permission-request-locks`. Hash a domain-separated, length-prefixed
serialization of provider, session, provider session, turn, cwd, and request
key; use the first digest byte.

Open/create descriptor-relative with `O_NOFOLLOW|O_CLOEXEC`, validate current
euid, regular file, length zero, link count one, stable device/inode, and exact
`0600` after permitted mode narrowing. Call `try_lock_exclusive` exactly once.
Keep the open file in an RAII guard and never unlink it.

Initialize and validate the owner-only lock directory before any recovery work.
Recovery requires proof of a matching held guard. Startup recovery derives the
journal's identity/request shard and tries the guard once; `Busy` reports an
active request and leaves the journal, lifecycle, and destinations untouched.
Live-hook recovery passes its already-held matching guard and never reacquires
the same file lock.

- [ ] **Step 5: Bind journals to exact authority**

Advance the unshipped transaction journal schema and derive:

```rust
fn expected_decision(journal: &PermissionTransactionJournal) -> PermissionDecision {
    match journal.terminal.state {
        ActivityState::Allowed => PermissionDecision::Decided(authority(journal, PermissionAction::Allow)),
        ActivityState::Denied => PermissionDecision::Decided(authority(journal, PermissionAction::Deny)),
        ActivityState::Abstained | ActivityState::Error => PermissionDecision::NeedsInput,
        _ => unreachable!(),
    }
}
```

Commit, verify, compensation, and recovery use Task 1 exact APIs. Any authority
mismatch fails closed; no code path treats a bare disposition as sufficient.

- [ ] **Step 6: Implement bounded live evidence and nonblocking directory mode**

Add bounded decision/activity read variants that accept a mutable
`LiveEvidenceBudget`, use `Read::take(remaining + 1)`, and return a typed
over-budget error before parsing bytes beyond the limit. Metadata is an early
check only.

Add `RecoveryLimits::live()`:

```rust
Self {
    max_journals: 1,
    max_total_bytes: 1024 * 1024,
    max_destination_bytes: 16 * 1024 * 1024,
    directory_lock: LockAcquisition::Nonblocking,
}
```

Keep startup defaults unchanged. Add a nonblocking preflight method and a
nonblocking `prepare_live`; neither holds the directory lock across caller
inference. Recheck and reader-enforce the same phase budget during commit and
final verification; starting another scan does not reset it.

- [ ] **Step 7: Run focused transaction and filesystem gates**

Run:

```bash
nix develop path:. --command cargo test brain::permission_request_lock
nix develop path:. --command cargo test brain::permission_transaction::tests
nix develop path:. --command cargo test brain::decisions
nix develop path:. --command cargo test brain::activity
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy -- -D warnings
```

Expected: all exit 0; subprocess tests prove real process locking.

- [ ] **Step 8: Commit, independently review, and remediate Task 2**

Create the local implementation commit:

```bash
git add src/brain/mod.rs src/brain/permission_request_lock.rs \
  src/brain/permission_transaction.rs src/brain/decisions.rs src/brain/activity.rs
git commit -m "🔒 fix: serialize and bound permission transactions (codexctl-ug26)"
```

Then obtain a fresh security-focused review. Put required fixes in separate
local remediation commits and obtain a fresh re-review before closing the Task
2 Bead. Do not publish any Task 2 commit.

---

### Task 3: Integrate exact guarded authority into permission hooks

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`
- Modify: `src/brain/permission_hook.rs`
- Modify: `tests/hook_activity.rs`
- Modify: `tests/lifecycle_hook_cli.rs`

**Interfaces:**
- Consumes: Task 1 exact decisions and Task 2 request guard/live transaction mode.
- Produces: one guarded admission-to-delivery permission flow for Codex, Claude, and Antigravity.

**Acceptance Criteria:**
- The request guard is acquired before recovery/admission and lives through response/delivery evidence.
- Guard failure performs zero request lifecycle/journal/activity mutation and zero inference; Codex/Claude emit no decision and Antigravity returns `ask`.
- Corrupt/newer lifecycle performs zero inference and is not quarantined or rewritten by the hook.
- Same-request conflicting subprocesses produce only the shard winner's response/mutations in either deterministic winner order.
- No deny persistence failure writes `Decided`; after guard acquisition, deny output remains available and fallback is `NeedsInput` only.
- Model allow requires exact allow authority and verified live transaction completion.
- All Task 5, provider, reproof, delivery, and correlation regressions pass.
- Schema 4, schema-3 migration, legacy-writer removal/restriction, and hook cutover land together.

- [ ] **Step 1: Write failing same-request subprocess integrations**

Add a hidden test-only subprocess entry point with deterministic ready/release
pipe or file handshakes around guard acquisition and inference. Run two real
`cbrain` permission-hook subprocesses with the same lifecycle identity and
request key but conflicting provider policy/model outcomes. Assert inference
with an externally observable test-only report/marker, not an in-process
counter that cannot cross the process boundary.

Test both orders:

```rust
// Deny holder wins: allow process preserves native confirmation and never infers.
assert_eq!(deny.stdout_behavior(), Some("deny"));
assert!(allow.stdout_is_empty());
assert!(!allow.inference_marker_exists());

// Allow holder wins: deny process preserves native confirmation and performs no mutation.
assert_eq!(allow.stdout_behavior(), Some("allow"));
assert!(deny.stdout_is_empty());
assert_eq!(loser_observable_delta(), ObservableDelta::None);
```

For both winner orders, assert the loser emits no response and creates no
lifecycle, journal, or `activity.jsonl` delta. Include killed-holder recovery:
after the synchronized holder is killed, a new subprocess acquires the shard.

Add the retained locked allow journal case: a same-key deny cannot bypass the
busy shard, and later recovery without exact allow authority produces
`NeedsInput`/`Error`, never `Allowed`.

- [ ] **Step 2: Write failing admission and provider regressions**

Add corrupt and future-schema lifecycle fixtures whose inference closures panic
if called. Assert no hook-side quarantine files or rewritten snapshot. Add
same-shard collision, invalid lock, two-journal backlog, over-budget/growing
destination, and held directory-lock cases for all providers.

For each blocked case:

```rust
assert_eq!(inference_calls, 0);
assert!(!stdout_contains_allow(&stdout));
assert_eq!(provider == AgentProvider::Antigravity, stdout_is_ask(&stdout));
assert!(diagnostic_is_bounded_and_redacted(&stderr));
```

Add deny-audit failure assertions proving the response remains deny only after
the guard is held, lifecycle is `NeedsInput` or unchanged, and no false
transaction-backed `Denied` terminal is claimed.

- [ ] **Step 3: Run the red hook suites**

Run:

```bash
nix develop path:. --command cargo test brain::permission_hook
nix develop path:. --command cargo test --test hook_activity
nix develop path:. --command cargo test --test lifecycle_hook_cli
```

Expected: new guard, exact-authority, and admission assertions fail against
commit `98b53353` behavior.

- [ ] **Step 4: Perform the atomic schema-4 and legacy-writer cutover**

Set `LIFECYCLE_SCHEMA_VERSION` to `4`, add `project_schema_three` with an empty
authority map, accept versions `1 | 2 | 3 | 4`, and move the newer-schema fixture
to 5. Add tests proving schema-3 bare decisions migrate as diagnostic evidence
only and a loader capped at version 3 rejects schema 4.

Use the Task 1 inventory to remove or restrict every bare `Decided` writer so no
execution path can create authority without a transaction. Verify with a
targeted `rg` audit plus compilation before integrating the hooks.

- [ ] **Step 5: Integrate guarded admission and exact transactions**

After payload parsing, resolve state paths and call `try_acquire`. If no guard
is returned, write only the bounded diagnostic/provider-native confirmation and
return before initial activity or lifecycle mutation.

With the guard alive:

1. initialize/validate and acquire the request shard before recovery;
2. preflight live destination and directory budgets;
3. recover with `RecoveryLimits::live()` by passing the already-held matching
   guard, without reacquiring it;
4. query exact lifecycle decision, preserving corrupt/newer errors;
5. append initial activity;
6. evaluate deterministic/provider policy, then model only if admission clean;
7. build the journal whose transaction ID/action define authority;
8. use nonblocking live prepare/commit;
9. emit the one response selected by the shard winner; and
10. append delivery evidence before dropping the guard.

Remove both out-of-transaction deny `Decided` fallbacks. After guard acquisition
a failed deny transaction may attempt exact `NeedsInput`; against corrupt/newer
state it performs no fallback write.

- [ ] **Step 6: Run all focused integration gates**

Run:

```bash
nix develop path:. --command cargo test brain::permission_hook
nix develop path:. --command cargo test brain::permission_transaction::tests
nix develop path:. --command cargo test -p coding-brain-core permission_authority
nix develop path:. --command cargo test -p coding-brain-core antigravity_permission
nix develop path:. --command cargo test --test hook_activity
nix develop path:. --command cargo test --test lifecycle_hook_cli
```

Expected: all exit 0, including real subprocess concurrency/crash tests.

- [ ] **Step 7: Run full workspace gates**

Run serially:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo build
nix develop path:. --command cargo test
git -c core.whitespace=trailing-space,space-before-tab diff --check 3b6f4874 HEAD
rg -n 'Decided|ensure_permission_disposition|set_permission_disposition' crates src tests
```

Expected: all exit 0. Nix hard-link saturation warnings are reported honestly
but are not failures when commands exit 0.

- [ ] **Step 8: Commit, independently review, and remediate Task 3**

Create the local implementation/cutover commit:

```bash
git add crates/coding-brain-core/src/lifecycle/projection.rs \
  crates/coding-brain-core/src/lifecycle/store.rs src/brain/permission_hook.rs \
  tests/hook_activity.rs tests/lifecycle_hook_cli.rs
git commit -m "🐛 fix: gate permission responses on exact authority (codexctl-ug26)"
```

Then obtain a fresh independent adversarial review covering both original Task
5 and all remediation findings. Put required fixes in separate local
remediation commits and obtain a fresh re-review before closing the Task 3
Bead. Do not publish any intermediate or final commit. Do not close
`codexctl-fb9y` until the post-commit reviewer approves the full range from
`9cff9721` through the final Task 3 remediation commit.

---

## Completion Evidence

Task 5 remediation is complete only when:

- all three remediation tasks have independent approval and local commits;
- full workspace format, clippy, build, and test gates exit 0;
- the original Critical same-request authority race and both Important review
  findings have explicit green regressions;
- the full Task 5 range receives fresh final approval;
- tracked status contains only expected committed work and `.internal/sdd/`
  remains untracked; and
- `codexctl-fb9y` is closed with exact commit and verification evidence before
  Task 6 begins.

---

## Stress Test Results: Transaction-Bound Authority Remediation Plan

The approved adversarial review produced eight binding corrections:

1. Task 1 adds exact authority types, maps, and APIs while retaining schema 3;
   Task 3 performs the schema-4 migration atomically with hook cutover.
2. Compatibility writers remain through Tasks 1 and 2, with an explicit call-site
   inventory; Task 3 removes or restricts them and verifies the result by search
   and compilation.
3. Cross-process tests use deterministic hidden test helpers and ready/release
   handshakes, including killed-holder reacquisition.
4. One mutable `LiveEvidenceBudget` charges actual bytes during every scan and
   final verification, returning a typed over-budget result.
5. Lock storage is validated before recovery, and recovery holds the same
   nonblocking shard before any mutation.
6. Both allow-winner and deny-winner subprocess orderings assert that the loser
   emits no response and causes no lifecycle, journal, or activity mutation.
7. Every task is locally committed before independent review; remediation gets
   separate commits and fresh re-review. No intermediate commit may be published.
8. Recovery requires one matching guard: startup recovery acquires it, while a
   live hook passes its existing guard so the same lock is never reacquired.
