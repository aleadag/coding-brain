# Shared Brain Review Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Give Attention, Review, Diagnostics, and Recent one durable, surface-local review lifecycle with safe bulk cleanup and undo while preserving all authoritative evidence and Scorecard semantics.

**Architecture:** Shared serializable contracts live in `coding-brain-core`; the binary owns secure review-state persistence, eligibility, and projections; the TUI consumes typed projections and sends revision-bound mutations through `BrainActions`. Review state contains only fixed domain-separated keys, is applied before surface grouping/counting/limits, and is never read by permission, learning, audit, or Scorecard code.

**Tech Stack:** Rust 2024, serde/serde_json, sha2, fs2, libc descriptor-relative filesystem APIs, Ratatui 0.29, Cargo workspace, Nix development shell, Beads.

## Global Constraints

- State path: `$XDG_STATE_HOME/coding-brain/review-state.json`, falling back through `CodingBrainPaths`; do not read or migrate legacy codexctl state.
- Surfaces: `attention`, `review`, `diagnostics`, and `recent`; Scorecard has no review namespace.
- Attention, Review, and Diagnostics use NEW -> reviewed -> archived; Recent uses unseen -> seen and rejects archive/undo.
- Review state is surface-local and must not affect another surface, permission hooks, deterministic rules, activity/decision evidence, corrections, canonical marks, learning, or Scorecard.
- Persist only `SHA256("review-item:v1" || surface || length(source_identity) || source_identity)` keys and dispositions; reject duplicate freshly derived keys.
- Limit each namespace and mutation to 10,000 keys and the encoded file to 8 MiB before parsing or replacement.
- The 10,000-key namespace cap is overload protection, not an eviction policy. A mutation that would exceed it returns bounded `CapacityExceeded`; projection remains visible/NEW and no reviewed or archived key is silently discarded.
- Use per-surface revisions, exact target keys, same-surface eligibility revalidation, revision-bound archive-all, and one durable `last_archive` undo slot.
- Read and release authoritative-store locks before acquiring the review-state lock; never retry stale bulk actions automatically.
- Missing state means all retained eligible items are NEW; invalid or unsafe state fails visibly and never hides a fresh projection.
- State-root ancestry, state file, and lock file must reject symlinks, non-regular files, multiple links, wrong ownership, and unsafe modes without following or mutating targets.
- Production keeps current severity-first Attention order; tests must exercise review partitioning under severity-first and recency-first policies without shipping `codexctl-s611` configuration.
- Tasks 1-6 are local implementation checkpoints only. Do not create a feature PR, bump a version, or make user-facing shipped-behavior claims until Task 7 proves all four adapters, controls, rendering, restart persistence, and Scorecard invariance together.
- Do not commit, push, publish, or sync Beads unless the user explicitly authorizes it. Proposed checkpoint descriptions use the required emoji conventional format and include `codexctl-k9h8x`.

---

### Task 1: Define review contracts and secure durable storage

**Files:**
- Create: `crates/coding-brain-core/src/review_state.rs`
- Modify: `crates/coding-brain-core/src/lib.rs`
- Create: `src/brain/secure_state.rs`
- Create: `src/brain/review_state.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/brain/permission_request_lock.rs`

**Interfaces:**
- Produces: `ReviewSurface`, `ReviewDisposition`, `ReviewKey`, `ReviewTarget`, `SurfaceReviewProjection`, `BrainReviewProjection`, `ReviewMutationRequest`, `ReviewMutation`, and `ReviewMutationResult` in `coding_brain_core::review_state`.
- Produces: `ReviewStateStore::at(&Path)`, `read()`, and `mutate(&ReviewMutationRequest, &BTreeSet<ReviewKey>)` in `brain::review_state`.
- Produces: descriptor-relative `SecureStateDirectory` helpers reused by permission-request locks and review-state storage.
- Consumes: `coding_brain_core::durable_file` durability semantics, `sha2`, `fs2`, and existing permission-request-lock directory validation behavior.

**Acceptance Criteria:**
- Fixed review keys are deterministic, domain-separated by surface, serialized as exactly 64 lowercase hex characters, and duplicate fresh keys fail closed.
- Missing state reads as revision-zero empty surfaces; valid mutations increment only the targeted surface revision.
- Review/archive/archive-all/undo transitions enforce surface policy, expected revision/count, eligible-key membership, 10,000-key limits, and one `last_archive` slot.
- A 10,001st still-eligible key remains visible and NEW; mutations that would cross capacity fail without changing state, while archive/undo operations that do not increase stored membership remain available.
- The store rejects malformed/duplicate JSON fields, unsupported schema, files over 8 MiB, symlinks, hard links, wrong ownership/modes, and unsafe ancestry.
- Concurrent same-surface writers cannot lose updates; different surfaces do not invalidate each other.
- Errors distinguish pre-mutation `Busy` from post-replacement `DurabilityUncertain`; the latter cannot be replayed against the captured revision and requires a fresh read.
- Existing permission-request-lock tests remain green after the secure-directory extraction.
- The extraction preserves permission-lock predicates, bounded timeout, error classification, mode/ownership checks, and inode-replacement defenses in effect; it is verified before review-state storage calls the shared primitive.
- Linux/Android and Apple directory-open branches retain their existing descriptor-relative flags (`O_PATH` versus `O_SEARCH`) and tests; no generic path-based fallback replaces them.

- [ ] **Step 1: Write failing core key and contract tests**

Add tests in `crates/coding-brain-core/src/review_state.rs` that compile against these exact public shapes:

```rust
#[test]
fn review_keys_are_surface_separated_and_fixed_width() {
    let attention = ReviewKey::derive(ReviewSurface::Attention, b"activity-1");
    let recent = ReviewKey::derive(ReviewSurface::Recent, b"activity-1");
    assert_ne!(attention, recent);
    assert_eq!(attention.to_string().len(), 64);
    assert_eq!(attention, ReviewKey::derive(ReviewSurface::Attention, b"activity-1"));
}

#[test]
fn recent_rejects_archive_operations() {
    let request = ReviewMutationRequest {
        surface: ReviewSurface::Recent,
        expected_surface_revision: 0,
        operation: ReviewMutation::SetDisposition {
            keys: [ReviewKey::derive(ReviewSurface::Recent, b"recent-1")]
                .into_iter()
                .collect(),
            disposition: ReviewDisposition::Archived,
        },
    };
    assert_eq!(request.validate(), Err(ReviewRequestError::UnsupportedOperation));
}
```

- [ ] **Step 2: Run the core test to verify it fails**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core review_state -- --nocapture
```

Expected: compilation fails because `review_state` and its types do not exist.

- [ ] **Step 3: Implement the minimal core contracts**

Create the module with these exact signatures and serde representations:

```rust
pub const MAX_REVIEW_KEYS: usize = 10_000;
pub const MAX_REVIEW_STATE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSurface { Attention, Review, Diagnostics, Recent }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition { Reviewed, Archived }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReviewKey([u8; 32]);

impl ReviewKey {
    pub fn derive(surface: ReviewSurface, source_identity: &[u8]) -> Self;
    pub fn as_bytes(&self) -> &[u8; 32];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub surface: ReviewSurface,
    pub display_id: String,
    pub new_member_keys: Vec<ReviewKey>,
    pub reviewed_member_keys: Vec<ReviewKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SurfaceReviewProjection {
    pub revision: u64,
    pub items: Vec<ReviewTarget>,
    pub new_count: usize,
    pub reviewed_count: usize,
    pub last_archive_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrainReviewProjection {
    pub attention: SurfaceReviewProjection,
    pub review: SurfaceReviewProjection,
    pub diagnostics: SurfaceReviewProjection,
    pub recent: SurfaceReviewProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMutation {
    SetDisposition {
        keys: BTreeSet<ReviewKey>,
        disposition: ReviewDisposition,
    },
    ArchiveAllReviewed { expected_count: usize },
    UndoLastArchive { expected_count: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewMutationRequest {
    pub surface: ReviewSurface,
    pub expected_surface_revision: u64,
    pub operation: ReviewMutation,
}
```

Use a custom serde visitor for `ReviewKey` so non-lowercase, non-hex, or non-64-byte strings are rejected instead of normalized.

- [ ] **Step 4: Extract and regression-test the secure state-directory primitive**

Move the descriptor-relative traversal/open logic from `permission_request_lock.rs` into `secure_state.rs` without changing its validation predicates, timeout behavior, error mapping, or inode checks. This is a mechanical checkpoint: run the complete permission-request-lock suite immediately after the extraction and before adding any `ReviewStateStore` call site. Expose only:

```rust
pub(crate) struct SecureStateDirectory { /* open directory descriptor + display path */ }

impl SecureStateDirectory {
    pub(crate) fn open_or_create(state_root: &Path) -> Result<Self, SecureStateError>;
    pub(crate) fn open_regular(&self, name: &CStr, create: bool) -> Result<File, SecureStateError>;
    pub(crate) fn metadata(&self, name: &CStr) -> Result<SecureEntryMetadata, SecureStateError>;
    pub(crate) fn sync(&self) -> io::Result<()>;
}
```

Keep permission-request lock behavior covered by its existing symlink, inode-replacement, ownership, and mode tests. Preserve the existing Linux/Android, Apple, and fallback `cfg` branches during extraction, including Apple `O_SEARCH` handling; add compile-time signature checks for platform helpers and keep Apple-specific tests under their current target guards. Add review-specific state/lock symlink and hard-link fixtures before implementing storage.

- [ ] **Step 5: Run secure-state tests to verify the new fixtures fail**

Run:

```bash
nix develop path:. --command cargo test brain::permission_request_lock brain::review_state -- --nocapture
```

Expected: existing permission-request-lock tests pass; new review-state tests fail because `ReviewStateStore` is not implemented.

- [ ] **Step 6: Implement `ReviewStateStore` and exact transitions**

Use strict structs with `#[serde(deny_unknown_fields)]`, a custom top-level deserializer that rejects duplicate fields, descriptor-relative opens, a bounded exclusive `fs2` lock, a same-directory private temporary, file sync, `renameat`, and directory sync. Implement:

```rust
impl ReviewStateStore {
    pub(crate) fn at(state_root: &Path) -> Self;
    pub(crate) fn read(&self) -> Result<ReviewStateSnapshot, ReviewStateError>;
    pub(crate) fn mutate(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
    ) -> Result<ReviewMutationResult, ReviewStateError>;
}
```

Inside `mutate`, prune `items` and `last_archive` to `eligible`, verify the expected surface revision, apply exactly one transition, increment with `checked_add`, encode to a buffer, reject buffers over 8 MiB, durably publish, and return the new revision/counts.

- [ ] **Step 7: Run focused store and regression tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core review_state -- --nocapture
nix develop path:. --command cargo test brain::review_state -- --nocapture
nix develop path:. --command cargo test brain::permission_request_lock -- --nocapture
```

Expected: all focused tests pass, including concurrent writers, stale revisions, archive-all beyond display limits, a 10,001-key fail-visible capacity fixture, durable undo, injected pre/post-rename failures with distinct `Busy`/`DurabilityUncertain` results, and filesystem attack fixtures.

- [ ] **Step 8: Review checkpoint**

Inspect only Task 1 files with `git diff --check` and `git diff -- <paths>`. Proposed description if commit authorization is later granted: `🧠 feat: add secure review state storage (codexctl-k9h8x)`.

### Task 2: Project Attention, Recent, and Diagnostics through review state

**Files:**
- Modify: `src/brain/activity.rs`
- Modify: `crates/coding-brain-core/src/brain_activity.rs`
- Use: `crates/coding-brain-core/src/review_state.rs`

**Interfaces:**
- Consumes: `ReviewStateSnapshot`, `ReviewKey`, `ReviewTarget`, and `SurfaceReviewProjection` from Task 1.
- Produces: `AttentionOrder::{SeverityFirst, RecencyFirst}` and `ActivityReviewProjection { snapshot, attention, recent, diagnostics, eligible }`.
- Produces: same-surface eligible key sets before review filtering from the same pure adapter pass that derives visible rows and review targets; runtime mutation validation reuses these adapters and must not reimplement eligibility.

**Acceptance Criteria:**
- Archived occurrences are filtered before Attention grouping, active/new/unresolved counts, representative choice, overflow, ordering, and truncation.
- Mixed groups expose separate NEW and reviewed member keys; a later matching occurrence reopens the same group as NEW.
- NEW groups precede reviewed groups under both ordering policies; production retains severity-first.
- Recent remains chronologically bounded and exposes unseen/seen counts; Diagnostics exposes NEW/reviewed/archive behavior.
- Resolved failed outcomes can be reviewed and archived without changing their evidence or unresolved count.

- [ ] **Step 1: Write failing mixed-group and ordering tests**

Add a review-state fixture and tests in `src/brain/activity.rs`:

```rust
#[test]
fn reviewed_group_reopens_when_a_new_occurrence_arrives() {
    let log = log_with_same_fingerprint([("old", 10), ("new", 20)]);
    let reviewed = review_snapshot(ReviewSurface::Attention, ["old"], []);
    let projected = project_snapshot_with_review(
        &log,
        SnapshotLimits::default(),
        30,
        &reviewed,
        AttentionOrder::SeverityFirst,
    );
    assert_eq!(projected.snapshot.attention.len(), 1);
    assert_eq!(projected.snapshot.attention[0].occurrences, 2);
    assert_eq!(projected.attention.items[0].new_member_keys.len(), 1);
    assert_eq!(projected.attention.items[0].reviewed_member_keys.len(), 1);
}

#[test]
fn archived_occurrences_do_not_inflate_counts_or_overflow() {
    let log = attention_log(105);
    let state = review_snapshot(ReviewSurface::Attention, [], 0..10);
    let projected = project_snapshot_with_review(
        &log,
        SnapshotLimits::default(),
        200,
        &state,
        AttentionOrder::SeverityFirst,
    );
    assert_eq!(projected.snapshot.unresolved_count, 95);
    assert_eq!(projected.snapshot.attention.len(), 95);
}
```

- [ ] **Step 2: Run the projection tests to verify they fail**

Run:

```bash
nix develop path:. --command cargo test reviewed_group_reopens
nix develop path:. --command cargo test archived_occurrences_do_not_inflate
```

Expected: compilation fails because the review-aware projection and ordering policy do not exist.

- [ ] **Step 3: Implement one classification pass before grouping**

Introduce private `OccurrenceReviewState::{New, Reviewed, Archived}` and compute a `ReviewKey` for every surface-eligible lifecycle. Each surface adapter returns its pre-review eligible set, visible rows, and targets from one pure pass. Build active Attention groups only from New/Reviewed occurrences; accumulate `ReviewTarget` member lists in the same `HashMap` entry as `AttentionItem`. Derive the opaque group display ID with domain `attention-group:v1` and length-delimited existing group-key fields.

Return `ActivityReviewProjection` with parallel `SurfaceReviewProjection.items` vectors in exactly the same order as their visible surface vectors. Assert matching lengths in tests and document the invariant on the struct.

- [ ] **Step 4: Add Recent, Diagnostics, failed-outcome, and comparator tests**

Cover:

```rust
assert_eq!(projected.recent.new_count, 1);
assert_eq!(projected.recent.reviewed_count, 1);
assert!(projected.snapshot.diagnostic_events.iter().all(|item| item.activity_id != "archived"));
assert_eq!(severity.attention.items[0].display_id, recency.attention.items[1].display_id);
assert_eq!(scorecard_input_events(&log), log.events());
```

The final assertion is a test helper proving projection filtering does not mutate or replace the source log.

- [ ] **Step 5: Implement surface projections and ordering policy**

Sort Attention by `(is_new_partition, chosen_order, deterministic_tie_break)` where NEW is always first. Keep current production call sites on `AttentionOrder::SeverityFirst`; add no CLI or config field. Recent stays chronological. Diagnostics stays chronological after archive filtering.

- [ ] **Step 6: Run focused and existing activity tests**

Run:

```bash
nix develop path:. --command cargo test brain::activity -- --nocapture
```

Expected: all existing status/grouping/overflow/compaction tests and new review projection tests pass.

- [ ] **Step 7: Review checkpoint**

Run `git diff --check`. Proposed authorized description: `🧠 feat: project live activity through review state (codexctl-k9h8x)`.

### Task 3: Project Review metadata while keeping Scorecard invariant

**Files:**
- Modify: `src/brain/review.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: fixtures in `tests/brain_tui_smoke.rs`

**Interfaces:**
- Consumes: Task 1 review contracts and Task 2 `ActivityReviewProjection`.
- Produces: `BrainRefresh.review_state: BrainReviewProjection` aligned with Attention, Review, Diagnostics, and Recent vectors.
- Produces: checked `SurfaceReviewProjection::from_items(...)` and `BrainRefresh::validate_review_alignment()` boundaries; production refreshes and mocks cannot publish independently reordered or length-mismatched metadata.
- Produces: `review_queue_from(..., &ReviewStateSnapshot) -> (Vec<ReviewItemSummary>, SurfaceReviewProjection, BTreeSet<ReviewKey>)`.
- Preserves: `scorecard_from` input and output independent from review state.

**Acceptance Criteria:**
- Review candidates are NEW/reviewed/archived by decision identity; canonical candidates remain excluded by existing rules.
- Legacy candidates without decision IDs receive stable `legacy-review:v1` identities, can be reviewed/archived, and cannot be marked canonical.
- Scorecard output is byte-for-byte/equality identical across empty, reviewed, and archived review snapshots.
- Every `BrainRefresh` and mock fixture has aligned review metadata and per-surface revisions/counts.
- A refresh with any row/projection length mismatch is rejected before it reaches the controller; negative tests cover all four itemized surfaces.

- [ ] **Step 1: Write failing Review and Scorecard invariance tests**

```rust
#[test]
fn archived_review_candidate_leaves_queue_but_not_scorecard() {
    let records = vec![review_record()];
    let events = activity_for_records(&records);
    let baseline = scorecard_from(&summaries(&records), &events);
    let state = review_snapshot(ReviewSurface::Review, [], [review_key(&records[0])]);
    let (queue, projection, _) = review_queue_from(records, &events, &state).unwrap();
    assert!(queue.is_empty());
    assert!(projection.items.is_empty());
    assert_eq!(scorecard_from(&summaries_from_events(&events), &events), baseline);
}
```

Add a legacy record test asserting a non-empty deterministic display ID and `canonical_available == false` in its projection metadata.

- [ ] **Step 2: Run the runtime tests to verify they fail**

Run:

```bash
nix develop path:. --command cargo test archived_review_candidate
nix develop path:. --command cargo test legacy_review_candidate
```

Expected: compilation fails because `review_queue_from` has no review-state parameter or projection result.

- [ ] **Step 3: Implement Review identity and projection**

Add a pure helper:

```rust
fn review_source_identity(record: &DecisionRecord) -> Vec<u8> {
    match record.decision_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => id.as_bytes().to_vec(),
        None => legacy_review_identity_v1(record).to_vec(),
    }
}
```

The legacy encoding must be length-delimited and include immutable persisted identity fields only: provider, timestamp, pid, project, tool, command, brain action, and confidence bits. Do not include mutable correction/canonical fields. Mark canonical availability separately from operational review identity.

- [ ] **Step 4: Add `review_state` to `BrainRefresh` and update fixtures mechanically**

Add:

```rust
#[derive(Debug, Clone, Default)]
pub struct BrainRefresh {
    pub snapshot: ActivitySnapshot,
    pub review_queue: Vec<ReviewItemSummary>,
    pub scorecard: ScorecardSummary,
    pub review_state: BrainReviewProjection,
}
```

Update every `BrainRefresh` literal with `review_state: BrainReviewProjection::default()` unless the test asserts review behavior. Do not add review metadata to `ActivityItem`; keep the concern in the refresh envelope. Construct non-empty surface projections through `SurfaceReviewProjection::from_items` and call `BrainRefresh::validate_review_alignment()` in production and mock `BrainSource::refresh` implementations before returning. Add one negative test per surface proving a mismatched row count is rejected rather than indexed positionally.

- [ ] **Step 5: Wire `LiveBrainSource::refresh_from_store`**

Read `ReviewStateStore` after releasing the ActivityStore read lock, project Task 2 activity surfaces, project Review, and assemble one aligned `BrainReviewProjection`. On review-state `Busy`, return `BrainSourceError::Busy`; on invalid storage, return bounded `Other` without replacing the TUI’s last good snapshot.

- [ ] **Step 6: Run runtime, core, and smoke tests**

```bash
nix develop path:. --command cargo test runtime::brain -- --nocapture
nix develop path:. --command cargo test -p coding-brain-core runtime -- --nocapture
nix develop path:. --command cargo test --test brain_tui_smoke -- --nocapture
```

Expected: all pass, including Scorecard invariance and metadata/vector alignment assertions.

- [ ] **Step 7: Review checkpoint**

Run `git diff --check`. Proposed authorized description: `🧠 feat: expose cross-view review projections (codexctl-k9h8x)`.

### Task 4: Add revision-bound runtime mutations and eligibility validation

**Files:**
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `src/brain/review_state.rs`

**Interfaces:**
- Consumes: `ReviewMutationRequest`, `ReviewMutation`, Task 2 activity eligibility, and Task 3 Review eligibility.
- Produces: `BrainActions::mutate_review_state(&self, request) -> Result<ReviewMutationResult, ReviewMutationError>`.
- Produces: `fresh_eligible_review_keys(state_root, surface) -> Result<BTreeSet<ReviewKey>, ReviewMutationError>` with no review-state lock held.

**Acceptance Criteria:**
- Every mutation validates current same-surface eligibility, expected revision, disposition transition, and target count.
- An Attention item that resolves into Recent while a prompt is open is rejected as stale Attention work and remains unseen in Recent.
- Archive-all covers eligible reviewed keys beyond display limits without accepting same-surface revision changes.
- Undo restores only `last_archive ∩ eligible ∩ archived`, survives restart, and cannot restore a newer archive.
- Mutations never hold authoritative-store locks while acquiring the review-state lock.
- Lock release is structural: a scoped helper returns owned authoritative data and eligibility before `ReviewStateStore::mutate` can be called; contention tests prove there is no cross-lock retry or wait cycle.

- [ ] **Step 1: Write failing stale-surface and concurrency tests**

```rust
#[test]
fn attention_action_is_rejected_after_item_resolves_into_recent() {
    let fixture = review_runtime_fixture();
    let refresh = fixture.refresh();
    let request = review_selected(&refresh.review_state.attention, 0);
    fixture.append_success_outcome("activity-1");
    let error = fixture.actions.mutate_review_state(request).unwrap_err();
    assert_eq!(error, ReviewMutationError::TargetNoLongerEligible);
    assert_eq!(fixture.refresh().review_state.recent.new_count, 1);
}

#[test]
fn archive_all_rejects_same_surface_revision_change() {
    let fixture = reviewed_attention_fixture(101);
    let stale = archive_all(&fixture.refresh().review_state.attention);
    fixture.review_another_attention_item();
    assert_eq!(fixture.actions.mutate_review_state(stale), Err(ReviewMutationError::StaleRevision));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
nix develop path:. --command cargo test attention_action_is_rejected
nix develop path:. --command cargo test archive_all_rejects
```

Expected: compilation fails because `BrainActions` has no review mutation method.

- [ ] **Step 3: Extend `BrainActions` and its mock**

Add the exact trait method and `MockBrainAction::ReviewMutation(ReviewMutationRequest)`. The mock validates through an in-memory `ReviewStateSnapshot` so controller tests observe revision changes rather than merely logging calls.

- [ ] **Step 4: Implement fresh eligibility and mutation orchestration**

For each request, use a scoped `fresh_surface_evidence(...) -> OwnedSurfaceEvidence` helper whose return type contains no file, lock, guard, or borrowed store state:

1. resolve `CodingBrainPaths`;
2. read/recover authoritative sources and release their locks;
3. call the same pure surface adapter used by refresh and take its pre-review eligible key set; do not maintain a second eligibility implementation;
4. call `ReviewStateStore::mutate` with no authoritative lock held;
5. map lock timeout to `Busy` and validation/storage failures to bounded typed errors.

Do not automatically retry `StaleRevision`, `TargetNoLongerEligible`, count mismatch, or disposition conflict. Add two barrier-based contention tests: one holds the authoritative lock while another mutation starts, and one holds the review lock while authoritative evidence changes. Assert both complete within the injected fixture bound, do not overlap locks, and do not broaden/replay stale requests.

- [ ] **Step 5: Add archive, archive-all, undo, and namespace tests**

Cover individual archive replacing `last_archive`, later archive replacing the undo slot, undo after restart, ineligible-key pruning, Recent archive/undo rejection, and a mutation in Recent not invalidating a pending Attention request.

- [ ] **Step 6: Run focused runtime and storage tests**

```bash
nix develop path:. --command cargo test runtime::brain -- --nocapture
nix develop path:. --command cargo test brain::review_state -- --nocapture
nix develop path:. --command cargo test -p coding-brain-core runtime -- --nocapture
```

Expected: all pass with zero permission-hook or correction changes.

- [ ] **Step 7: Review checkpoint**

Run `git diff --check`. Proposed authorized description: `🧠 feat: add exact review lifecycle mutations (codexctl-k9h8x)`.

### Task 5: Implement shared TUI review controls and stable selection

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs`
- Modify: `crates/coding-brain-core/src/runtime.rs` mock support as required by controller tests

**Interfaces:**
- Consumes: `BrainRefresh.review_state`, `ReviewTarget`, and `BrainActions::mutate_review_state`.
- Produces: `BrainInput::ReviewConfirmation`, current-surface selection helpers, revision-bound requests, and display-ID selection restoration.
- Preserves: existing correction, canonical, navigation, session-action, Busy, and evidence-scroll behavior.

**Acceptance Criteria:**
- `a` reviews selected NEW keys immediately; `A`, `d`, and `D` confirm exact surface/action/count; `u` restores the latest eligible archive without confirmation.
- Review `s` persists review and moves to the next effective candidate only after persistence succeeds; failure leaves the current item selected.
- Recent exposes only `a`/`A`; Scorecard exposes no review actions.
- New evidence or a surface transition while confirmation is open cannot be swallowed.
- Selection is restored by review `display_id`, with nearest-row fallback only after removal.
- Existing input mode always owns `a`/`d` before lifecycle dispatch, confirmations are consumed before submission to prevent double-send, and the bounded local-store mutation completes before the immediate refresh.
- `Busy` before mutation may preserve a retryable prompt; `DurabilityUncertain` consumes it, blocks replay, and forces a fresh successful read before another lifecycle action.

- [ ] **Step 1: Write failing controller tests for every action**

```rust
#[test]
fn bulk_confirmation_keeps_captured_surface_revision_and_count() {
    let (mut app, mock) = review_fixture(ReviewSurface::Attention, 3, 2);
    app.handle_key(key(KeyCode::Char('D')));
    assert_eq!(app.input_prompt(), Some("Archive 2 reviewed Attention items? y/Esc".into()));
    mock.publish_new_attention_item();
    app.handle_key(key(KeyCode::Char('y')));
    assert!(app.status().unwrap().contains("changed; refresh and retry"));
}

#[test]
fn recent_rejects_archive_keys_but_supports_mark_all_seen() {
    let (mut app, mock) = review_fixture(ReviewSurface::Recent, 2, 0);
    app.handle_key(key(KeyCode::Char('d')));
    assert!(mock.review_mutations().is_empty());
    app.handle_key(key(KeyCode::Char('A')));
    assert!(app.input_prompt().unwrap().contains("Mark 2 Recent items seen"));
}
```

Also cover `a`, `s`, `u`, Esc, any non-`y` cancel, zero-target status, and actions while session delivery is in flight.

- [ ] **Step 2: Run controller tests to verify they fail**

```bash
nix develop path:. --command cargo test -p coding-brain-tui bulk_confirmation
nix develop path:. --command cargo test -p coding-brain-tui recent_rejects_archive
```

Expected: assertions fail because the keys are unhandled.

- [ ] **Step 3: Add current-surface and captured-input helpers**

Implement:

```rust
fn selected_review_target(&self) -> Option<(&SurfaceReviewProjection, &ReviewTarget)>;
fn visible_new_keys(&self) -> BTreeSet<ReviewKey>;
fn begin_review_confirmation(&mut self, operation: ReviewMutation);
fn submit_review_mutation(&mut self, request: ReviewMutationRequest);
fn restore_surface_selection(&mut self, previous_display_id: Option<&str>);
```

`BrainInput::ReviewConfirmation` stores the complete request and prompt metadata. It never stores a positional index as authority.

- [ ] **Step 4: Implement action handling and status semantics**

Use `a/A/d/D/u` only when the active surface supports them. Keep `a` and `d` inside the existing SessionAction input menu unchanged because `handle_input` owns that mode before top-level key dispatch. Take and clear a confirmation before calling the bounded synchronous local-store mutation so repeated `y` cannot resubmit it. Map typed mutation failures to bounded statuses: stale/ineligible failures cancel input and refresh; pre-mutation Busy may reconstruct the same captured prompt; `DurabilityUncertain` never reconstructs or replays it and disables lifecycle input until a fresh read succeeds. Review `s` refreshes and advances by effective display identity only after success; on error it preserves the current item and evidence scroll.

- [ ] **Step 5: Preserve selection and evidence scroll by display identity**

Capture each surface’s selected `display_id` before refresh. Restore by searching the new parallel review projection. If absent, clamp to the nearest row. Reset evidence scroll only when the effective display ID changes. Add regressions for Attention representative changes within the same group and Review `s` after refresh reorder.

- [ ] **Step 6: Run all TUI controller tests**

```bash
nix develop path:. --command cargo test -p coding-brain-tui brain_app -- --nocapture
```

Expected: new and existing navigation, correction, session-action, Busy, selection, and evidence-scroll tests pass.

- [ ] **Step 7: Review checkpoint**

Run `git diff --check`. Proposed authorized description: `✨ feat: add shared Brain review controls (codexctl-k9h8x)`.

### Task 6: Render lifecycle state consistently across all views

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/review.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/diagnostics.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs`

**Interfaces:**
- Consumes: Task 3 parallel `SurfaceReviewProjection` vectors and Task 5 current-surface helpers.
- Produces: NEW/unseen/reviewed row styling, Attention total/new counts, per-surface title counts, confirmation copy, and surface-aware footer help.

**Acceptance Criteria:**
- Attention renders NEW distinctly, reviewed rows de-emphasized, and mixed rows as total plus new count without shifting list columns.
- Review and Diagnostics use the same NEW/reviewed visual language; Recent reports unseen count without archive affordances.
- Footer help advertises only valid actions for the active surface and exposes undo only when available.
- Narrow and wide TestBackend snapshots preserve content columns, selection, overflow, and evidence layout.

- [ ] **Step 1: Write failing TestBackend rendering tests**

```rust
#[test]
fn all_itemized_surfaces_share_new_and_reviewed_language() {
    let mut app = review_render_fixture();
    let live = render_text(&app);
    assert!(live.contains("NEW"));
    assert!(live.contains("x5 · 2 new"));
    app.handle_key(key(KeyCode::Tab));
    assert!(render_text(&app).contains("Review Queue (1 new, 1 reviewed)"));
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Tab));
    assert!(render_text(&app).contains("Diagnostics (1 new, 1 reviewed)"));
}
```

Add a Recent fixture asserting `Recent (2 unseen)` and absence of archive/undo footer keys.

- [ ] **Step 2: Run rendering tests to verify they fail**

```bash
nix develop path:. --command cargo test -p coding-brain-tui all_itemized_surfaces
nix develop path:. --command cargo test -p coding-brain-tui recent_has_no_archive
```

Expected: assertions fail because review state is not rendered.

- [ ] **Step 3: Implement shared row/status helpers**

Create private helpers in `ui/brain/mod.rs`:

```rust
fn review_prefix(target: &ReviewTarget, unseen_label: &str) -> &'static str;
fn review_title(label: &str, projection: &SurfaceReviewProjection) -> String;
fn review_style(target: &ReviewTarget, theme: &Theme) -> Style;
```

Keep surface-specific content in its existing renderer. Use fixed-width prefixes and `HighlightSpacing::Always` so focus changes do not shift content columns.

- [ ] **Step 4: Render Attention counts and other surface titles**

Attention count and overflow use active occurrences after archive filtering. Render mixed occurrence suffix only when `new_member_keys.len() < occurrences`; otherwise retain the existing compact repeated-count form. Review/Diagnostics titles show new and reviewed counts. Recent shows unseen count and remains chronological.

- [ ] **Step 5: Render surface-aware footer and confirmation prompts**

Live Attention: `a review  A review all  d archive  D archive reviewed  u undo` plus existing actions. Live Recent: `a seen  A seen all` plus navigation/correction where valid. Review and Diagnostics use their supported subsets. Scorecard remains unchanged.

- [ ] **Step 6: Run full TUI tests**

```bash
nix develop path:. --command cargo test -p coding-brain-tui -- --nocapture
```

Expected: all rendering and controller tests pass on narrow/wide fixtures with no content-column regressions.

- [ ] **Step 7: Review checkpoint**

Run `git diff --check`. Proposed authorized description: `✨ feat: render shared Brain review state (codexctl-k9h8x)`.

### Task 7: Add restart integration, documentation, and full verification

**Files:**
- Create: `tests/brain_review_state.rs`
- Modify: `docs/configuration.md`
- Modify: any existing TUI key reference found by `rg -n "Needs Attention|Review Queue|Diagnostics|Recent|c correct" README.md docs crates/coding-brain-tui`
- Verify: all files changed by Tasks 1-6

**Interfaces:**
- Consumes: complete shared store, projections, runtime actions, and TUI behavior.
- Produces: process-level restart/race evidence and user-facing state/key/recovery documentation.

**Acceptance Criteria:**
- Isolated process tests run every transition in a fresh child and prove review/restart for all four surfaces, archive/undo for Attention/Review/Diagnostics, Recent seen persistence, Attention reopen-on-new-occurrence, and cross-surface independence.
- First run, explicit reset-to-NEW, invalid-state failure, cross-surface independence, and Scorecard invariance are covered end to end.
- Documentation distinguishes operational archive from evidence purge and describes state path, first-run bulk cleanup, undo, reset, and failure behavior.
- Formatting, Clippy, all-target tests, build, and Nix checks pass with fresh output.
- The final integration gate covers Attention, Review, Diagnostics, and Recent in one run; passing a subset cannot authorize release or a shipped-behavior claim.
- A source-boundary audit proves review-state imports remain confined to lifecycle contracts, projection, runtime mutation, and TUI modules; any dependency from safety, permission authority, execution, learning, correction, canonical, or Scorecard code fails the release gate.

- [ ] **Step 1: Write failing process integration tests**

Use an integration-test subprocess protocol so restart means a fresh OS process without adding a product CLI. The parent writes isolated ActivityStore fixtures, then invokes the integration-test executable itself with one exact ignored child test and `CODING_BRAIN_REVIEW_TEST_STEP=review|archive|undo|refresh`. Every child reconstructs `LiveBrainSource` and `LiveBrainActions` from the environment, performs one typed public runtime operation, writes only the normal review-state file, and exits. Set all three roots explicitly:

```rust
fn child(root: &TempDir, step: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "review_state_child", "--ignored", "--nocapture"])
        .env("HOME", root.path().join("home"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("CODING_BRAIN_REVIEW_TEST_STEP", step);
    command
}

#[test]
#[ignore = "subprocess helper"]
fn review_state_child() {
    let step = std::env::var("CODING_BRAIN_REVIEW_TEST_STEP").unwrap();
    run_review_test_step(&step).unwrap();
}

#[test]
fn review_archive_new_occurrence_and_undo_survive_restart() {
    let root = tempfile::tempdir().unwrap();
    append_group_occurrence(root.path(), "first");
    assert!(child(&root, "review").status().unwrap().success());
    assert!(child(&root, "archive").status().unwrap().success());
    append_group_occurrence(root.path(), "second");
    assert_eq!(read_attention_counts_in_child(&root), (1, 0));
    assert!(child(&root, "undo").status().unwrap().success());
    assert_eq!(read_attention_counts_in_child(&root), (1, 1));
}
```

`run_review_test_step` obtains the current projection through `LiveBrainSource`, constructs its request from the returned `ReviewTarget` and surface revision, and calls `LiveBrainActions`; it must not read or synthesize review keys from JSON. `read_attention_counts_in_child` uses the same child protocol and a result file inside the temporary root. Do not mutate the real user state or expose a test-only product command.

Expand the fixture into a table-driven process matrix. Each review, archive, undo, seen, and assertion step starts a new child. Cover Attention, Review, Diagnostics, and Recent review/restart; archive/undo on the three archivable surfaces; Recent seen persistence and archive rejection; Attention reopen after a matching occurrence; and an action in one surface leaving the other three revisions/counts unchanged. The parent may read the child's typed count result but never edits or constructs `review-state.json`.

- [ ] **Step 2: Run integration tests to verify they fail**

```bash
nix develop path:. --command cargo test --test brain_review_state -- --nocapture
```

Expected: new process tests fail until the public runtime action and projection wiring from Tasks 3-4 is complete.

- [ ] **Step 3: Complete only missing integration wiring**

Add no new product behavior and no public test harness. The integration test uses the already-public `coding_brain::runtime::{LiveBrainSource, LiveBrainActions}` types and core runtime traits. Keep subprocess-only helpers inside `tests/brain_review_state.rs`; preserve production limits and path resolution.

- [ ] **Step 4: Update human-facing documentation**

Document:

- `review-state.json` and `review-state.lock` under the Coding Brain state root;
- surface-local semantics and Scorecard/evidence independence;
- `a/A/d/D/u` keys and Review `s` alias;
- first-run all-NEW behavior and bulk cleanup;
- durable latest-archive undo;
- explicit reset by removing only `review-state.json` while Coding Brain is stopped;
- invalid/unsafe state failure and no automatic migration/purge.
- the 10,000-key overload cap and fail-visible `CapacityExceeded` behavior.

- [ ] **Step 5: Run focused integration and documentation checks**

```bash
nix develop path:. --command cargo test --test brain_review_state -- --nocapture
git diff --check
rg -n "review-state.json|archive|undo|Scorecard" docs README.md
```

Expected: integration tests pass, no whitespace errors, and documentation contains all required concepts.

- [ ] **Step 6: Run full Rust quality gates serially**

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo build
```

Expected: every command exits 0. Run serially to avoid Nix/target-directory contention.

- [ ] **Step 7: Run Nix checks**

```bash
nix flake check -L
```

Expected: every flake check exits 0; production lock timing and release packaging behavior remain unchanged.

Before release, require the GitHub Actions `Test (macos-latest)` job at the exact reviewed head in addition to local Linux/Nix evidence. Its `cargo test --all-targets` result is the portability gate for the extracted Apple `O_SEARCH` branch; a Linux-only pass is insufficient.

- [ ] **Step 8: Audit final scope and task evidence**

Run:

```bash
git diff --check
git status --short
bd -C /home/alexander/.beads-planning show codexctl-k9h8x
```

Run a source-boundary search over `src/brain/{safety,permission_hook,permission_transaction,pref_store,preferences,decisions,review}.rs`, Scorecard construction, and execution/runtime authority modules. Review-state references are allowed only in the operational Review projection portion of `review.rs`; any reference in safety, permission, execution, learning, correction, canonical, or Scorecard paths is a release failure. Confirm with focused permission regressions and the Scorecard invariance tests, not search alone.

Confirm every changed line traces to the spec, all seven implementation task beads are closed only after their acceptance evidence is recorded, and no commit/push/sync occurred without authorization. Proposed authorized final description: `✨ feat: add shared Brain review lifecycle (codexctl-k9h8x)`.

## Stress Test Results: shared Brain review lifecycle implementation plan

### Resolved Decisions

- Parallel surface review metadata remains separate from evidence DTOs, but checked constructors and `BrainRefresh` alignment validation reject mismatched row vectors before publication.
- Secure filesystem helper extraction is a mechanical behavioral-equivalence checkpoint before review-state storage uses it.
- Tasks 1-6 are local checkpoints; only the complete Task 7 integration gate can authorize a feature PR, version bump, or shipped-behavior claim.
- Projection and mutation revalidation share the same pure surface adapters and pre-review eligibility derivation.
- Authoritative lock release is structural: mutation receives owned evidence and eligibility before acquiring the review-state lock.
- The 10,000-key cap fails toward visibility with `CapacityExceeded`; it never silently evicts review state.
- Input mode owns conflicting keys, confirmations cannot double-submit, and Review `s` advances only after persistence succeeds.
- Fresh-process coverage spans all four surfaces, all supported archive/undo paths, Recent seen state, Attention reopening, and namespace independence.
- Post-replacement `DurabilityUncertain` is not retryable; lifecycle actions remain disabled until a fresh read succeeds.
- A final source-boundary audit prevents review state from entering permission, execution, learning, correction, canonical, or Scorecard authority.
- Linux/Android and Apple descriptor-open branches remain distinct, with macOS CI required at the exact reviewed head.

### Changes Made

- Added checked projection construction and negative alignment tests.
- Strengthened the permission-lock extraction checkpoint and platform parity requirements.
- Added an explicit complete-feature release barrier.
- Removed duplicate eligibility logic from the permitted implementation shape.
- Made cross-store lock ordering structural and contention-tested.
- Defined fail-visible capacity overflow behavior.
- Tightened key dispatch, double-submit, selection, and persistence-failure semantics.
- Expanded restart tests into an all-surface subprocess matrix.
- Split pre-mutation Busy from uncertain post-replacement durability.
- Added a final review-state authority-boundary audit.

### Deferred / Parking Lot

- Cross-machine review-state synchronization, full archive history, and user-facing Attention ordering remain outside `codexctl-k9h8x`.
- No generic non-Unix secure-state fallback is introduced; the supported Linux and Apple paths retain descriptor-relative validation.

### Confidence Assessment

- Overall: High.
- Remaining concern: implementation breadth is still material, so the sequential Beads dependency chain and complete Task 7 release barrier must remain intact.
