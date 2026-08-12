# Lifecycle Project-Identity Cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make every lifecycle hook return with measured headroom below Codex's two-second timeout by eliminating Git work on validated cache hits, bounding cold Git discovery, and preserving authoritative lifecycle/activity durability.

**Architecture:** Resolve project identity before opening `brain.sqlite3`, using an independently versioned and reconstructible `runtime-cache-v1.sqlite3`. Cache rows contain only closed, recomputable dependency evidence; misses use the existing process-group supervisor under one shared deadline, while authoritative lifecycle and activity commits retain their existing order and `synchronous=FULL` behavior. Opt-in closed timing records attribute all stages without exposing paths, identities, payloads, or free-form errors.

**Tech Stack:** Rust 2024 workspace, `rusqlite`, existing secure SQLite directory primitives, monotonic `Instant`, existing Unix process-group supervisor, Cargo/Nix test and release profiles.

## Global Constraints

- Keep `.coding-brain/project.toml` as the optional explicit durable authority; authority order remains manifest, canonical network remote, canonical-root temporary fallback.
- Keep `BRAIN_SCHEMA_VERSION == 1`, Codex's two-second lifecycle timeout, hook role restrictions, migrations, integrity checks, and fail-closed permission authority unchanged.
- Keep lifecycle commit before activity commit and keep both authoritative transactions at `synchronous=FULL`; never drop, weaken, retry blindly, or reinterpret a successful commit.
- Use a 1,500 ms whole-hook monotonic entry budget, at most 250 ms for parent discovery, one shared 250 ms Git budget, and one aggregate 64 KiB Git-output limit.
- Apply the original absolute hook deadline to stdin as well as later stages; a provider that leaves the input pipe open must not consume Codex's outer timeout.
- Keep the existing fresh 500 ms `StorageDeadline` for Brain open and authoritative operations, created only after project resolution.
- Reserve the complete 500 ms authoritative-storage window before optional discovery work; do not begin Brain persistence when that reserve is unavailable.
- Store the rebuildable projection only in `runtime-cache-v1.sqlite3`; hooks may create the exact v1 schema but may not migrate, repair, replace, quarantine, or delete an existing cache.
- Bound cache rows to 256, write only after authoritative activity success, and perform no cache write on a hit.
- Use a read-only pre-persistence cache connection and open/create a separate writer only after activity success; a hit must not create SQLite sidecars.
- Never cache temporary identities, discovery failures, negative lookups, conditional includes, path-changing overrides, non-file origins, or unrepresentable dependency evidence.
- Cache only stable UUID values with `manifest` or `network_remote` provenance; the schema must be unable to represent a temporary identity.
- Cache a cold result only when closed dependency snapshots taken immediately before and after discovery are identical.
- Exclude the runtime cache from authoritative exports, migrations, integrity authority, review reset, and permission evidence; corrupt-cache quarantine and obsolete-version cleanup remain deferred.
- Default stdout remains empty and default stderr behavior remains unchanged; `CBRAIN_HOOK_TIMING=1` emits only closed enums and integer durations.
- All regression tests are deterministic and use injected clocks, controlled descriptors, or explicit synchronization; add no fixed sleeps to new tests.
- Do not commit, push, publish, install, or deploy without explicit user authorization.

---

## File Structure

- Create `src/lifecycle_timing.rs`: monotonic hook budget, closed stage/outcome enums, and privacy-safe timing writer.
- Create `src/lifecycle_project.rs`: lifecycle-only cache lookup, closed dependency collection/validation, bounded Git orchestration, and post-activity refresh preparation.
- Create `src/brain/storage/runtime_cache.rs`: exact v1 auxiliary SQLite schema, secure lazy open/create, bounded lookup/upsert/pruning, and bypass classification.
- Modify `src/brain/storage/mod.rs`: register and narrowly re-export the runtime-cache API without changing Brain or Review schemas.
- Modify `src/provider_hooks/mod.rs`: preserve the existing process-group implementation while returning typed bounded-process failures and accepting a shared absolute deadline.
- Modify `crates/coding-brain-core/src/project.rs`: expose an injected project-command seam and closed resolution provenance while retaining `ProjectIdentity::load` compatibility.
- Modify `src/lifecycle_hook.rs`: reorder production stages, inject the resolved identity into activity construction, preserve transaction ordering, and emit timing boundaries.
- Modify `src/main.rs`: capture the earliest hook `Instant`, register the two focused modules, and pass the start time into lifecycle dispatch.
- Modify `tests/lifecycle_hook_cli.rs`: cross-process cache-hit, invalidation, timeout, timing, concurrency, and event-class integration coverage.
- Modify `tests/sqlite_storage.rs`: secure exact-cache creation, defensive read-only lookup, incompatible/corrupt bypass, contention, pruning, and version coexistence coverage.
- Modify `tests/live_fault_matrix.rs`: cache/Brain commit uncertainty and authoritative-evidence recovery assertions.
- Modify `CHANGELOG.md`: describe the reconstructible cache file and unchanged timeout/durability contract.
- Update `.internal/research/2026-08-11-lifecycle-hook-latency-boundaries.md`: record sanitized release-candidate measurements after implementation.

### Task 1: Whole-Hook Budget, Closed Timing, and Typed Process Outcomes

**Files:**
- Create: `src/lifecycle_timing.rs`
- Modify: `src/main.rs:1-25,379-407,585-594`
- Modify: `src/lifecycle_hook.rs:35-75,956-985`
- Modify: `src/provider_hooks/mod.rs:504-610,870-915`
- Test: `src/lifecycle_timing.rs` unit tests
- Test: `src/lifecycle_hook.rs` input unit tests
- Test: `src/provider_hooks/mod.rs` unit tests

**Interfaces:**
- Consumes: existing `provider_hooks::run_bounded_process`, process-group termination, and reaper.
- Produces: `HookBudget::from_start(Instant)`, `HookBudget::child_deadline(Duration) -> Option<Instant>`, `HookTiming::finish(HookStage, HookOutcome)`, `read_bounded_hook_input_until`, and `run_bounded_process_until(&mut Command, Instant, &mut OutputBudget) -> Result<Vec<u8>, BoundedProcessError>`.

**Acceptance Criteria:**
- The 1,500 ms budget starts before CLI parsing and later stages receive only remaining time.
- Bounded stdin read uses the original absolute deadline; an open writer produces closed `input_timeout` without cache or authoritative writes.
- Parent and Git callers can share absolute deadlines and an aggregate output allowance.
- Process timeout, output overflow, spawn, I/O, exit-status, and cleanup outcomes are closed enums; timeout still kills descendants and reaps the child.
- `CBRAIN_HOOK_TIMING=1` output contains only schema/provider/event/stage/outcome/elapsed/remaining fields with fixed bounds; default mode emits nothing.
- Virtual-clock tests require no wall sleeps, and process tests use a controlled descriptor/marker.

- [ ] **Step 1: Add failing virtual-budget and timing-format tests**

```rust
#[test]
fn stages_share_the_entry_budget_and_timing_is_closed() {
    let clock = FakeClock::at(Instant::now());
    let budget = HookBudget::with_clock(clock.clone(), Duration::from_millis(1500));
    clock.advance(Duration::from_millis(1200));
    assert_eq!(budget.remaining(), Duration::from_millis(300));
    assert_eq!(budget.allowance(Duration::from_millis(500)), Duration::from_millis(300));

    let line = format_timing(TimingRecord::new(
        AgentProvider::Codex,
        HookEventClass::UserPromptSubmit,
        HookStage::ProjectGit,
        HookOutcome::Timeout,
        250,
        1010,
    ));
    assert_eq!(line, "cbrain_hook_timing v=1 provider=codex event=user_prompt_submit stage=project_git outcome=timeout elapsed_ms=250 remaining_ms=1010\n");
    assert!(!line.contains('/'));
}
```

- [ ] **Step 2: Run the focused tests and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain lifecycle_timing -- --nocapture`

Expected: FAIL because `lifecycle_timing` and its closed types do not exist.

- [ ] **Step 3: Implement the budget and timing surface**

```rust
pub(crate) const HOOK_BUDGET: Duration = Duration::from_millis(1500);

pub(crate) struct HookBudget<C = SystemClock> {
    clock: C,
    deadline: Instant,
}

impl HookBudget<SystemClock> {
    pub(crate) fn from_start(started: Instant) -> Self {
        Self { clock: SystemClock, deadline: started + HOOK_BUDGET }
    }
}

impl<C: MonotonicClock> HookBudget<C> {
    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(self.clock.now())
    }

    pub(crate) fn child_deadline(&self, cap: Duration) -> Option<Instant> {
        let remaining = self.remaining();
        (!remaining.is_zero()).then(|| self.clock.now() + remaining.min(cap))
    }

    pub(crate) fn optional_child_deadline(
        &self,
        cap: Duration,
        reserve: Duration,
    ) -> Option<Instant> {
        let optional = self.remaining().saturating_sub(reserve);
        (!optional.is_zero()).then(|| self.clock.now() + optional.min(cap))
    }
}
```

Define `HookStage`, `HookOutcome`, and `HookEventClass` as exhaustive enums with `as_str()` mappings. Make `HookTiming` write only the fixed record above when `CBRAIN_HOOK_TIMING == "1"`; never pass arbitrary text into this writer.

- [ ] **Step 4: Refactor the existing bounded-process helper without changing cleanup semantics**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BoundedProcessError {
    Spawn,
    Io,
    Timeout,
    OutputLimit,
    ExitStatus,
    Cleanup,
}

pub(crate) struct OutputBudget { remaining: usize }

pub(crate) fn run_bounded_process_until(
    command: &mut Command,
    deadline: Instant,
    output: &mut OutputBudget,
) -> Result<Vec<u8>, BoundedProcessError>;
```

Keep `run_bounded_process(command, timeout, limit) -> Option<Vec<u8>>` as a compatibility wrapper for current parent/safety callers. In the Unix implementation, decrement `OutputBudget` as bytes arrive and call the existing `terminate_process_group` on every post-spawn error.

- [ ] **Step 5: Bound stdin against the same absolute deadline**

```rust
pub(crate) fn read_bounded_hook_input_until(
    reader: &mut (impl Read + std::os::fd::AsFd),
    deadline: Instant,
) -> Result<Vec<u8>, HookInputError>;
```

On Unix, set the supplied descriptor nonblocking and use `poll` with the remaining monotonic deadline before each read. Preserve the 64 KiB cap and distinguish `Read`, `TooLarge`, and `Timeout`. Regular files remain immediately readable. The controlled test writes a ready marker, deliberately retains the writer, advances the injected deadline, and asserts no cache/Brain fixture was touched. Parent/Git call sites use `optional_child_deadline(cap, Duration::from_millis(500))` so optional work cannot consume the storage reserve.

- [ ] **Step 6: Prove typed timeout/output cleanup and run formatting checks**

Run: `nix develop path:. --command cargo test -p coding-brain provider_hooks::tests::bounded_process lifecycle_timing -- --nocapture`

Expected: PASS; marker-controlled child and descendant are gone, output overflow is `OutputLimit`, and no new test contains `sleep`.

Run: `nix develop path:. --command cargo fmt --check`

Expected: PASS.

- [ ] **Step 7: Prepare the task changeset only when commit authority is granted**

```bash
git add src/main.rs src/lifecycle_hook.rs src/lifecycle_timing.rs src/provider_hooks/mod.rs
git commit -m "⏱️ feat: bound lifecycle hook stages"
```

### Task 2: Injectable Project Resolution with Closed Provenance

**Files:**
- Modify: `crates/coding-brain-core/src/project.rs:1-170,430-730`
- Test: `crates/coding-brain-core/src/project.rs` unit tests

**Interfaces:**
- Consumes: `ProjectId`, `ProjectIdentity`, manifest parsing, remote canonicalization, and current `ProjectIdentity::load` behavior.
- Produces: `ProjectCommandRunner`, `ProjectCommandError`, `ProjectResolution`, `ProjectProvenance`, and `ProjectIdentity::resolve_with(cwd, paths, runner)`.

**Acceptance Criteria:**
- Existing callers of `ProjectIdentity::load` retain manifest/remote/temporary semantics.
- Hook callers can inject a bounded command runner and receive canonical root plus `Manifest`, `NetworkRemote`, or `Temporary` provenance.
- A command failure never promotes a stable identity; canonical-root temporary fallback remains machine-local.
- Existing remote credential stripping and manifest UUID validation remain unchanged.

- [ ] **Step 1: Add failing injected-runner tests**

```rust
#[test]
fn injected_runner_reports_provenance_and_never_retries_after_failure() {
    let mut runner = FixtureRunner::new([
        (vec!["rev-parse", "--show-toplevel"], Ok(b"/repo\n".to_vec())),
        (vec!["remote", "get-url", "origin"], Err(ProjectCommandError::Timeout)),
    ]);
    let resolved = ProjectIdentity::resolve_with(Path::new("/repo/src"), &paths, &mut runner).unwrap();
    assert_eq!(resolved.provenance(), ProjectProvenance::Temporary);
    assert_eq!(runner.calls(), 2);
}
```

- [ ] **Step 2: Run the core project tests and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain-core project::tests::injected_runner -- --nocapture`

Expected: FAIL because the injected resolution API is absent.

- [ ] **Step 3: Add the minimal closed resolution API**

```rust
pub trait ProjectCommandRunner {
    fn output(&mut self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectProvenance { Manifest, NetworkRemote, Temporary }

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectResolution {
    root: PathBuf,
    identity: ProjectIdentity,
    provenance: ProjectProvenance,
}

impl ProjectIdentity {
    pub fn resolve_with(
        cwd: &Path,
        paths: &CodingBrainPaths,
        runner: &mut impl ProjectCommandRunner,
    ) -> Result<ProjectResolution, ProjectError>;
}
```

Implement `ProjectIdentity::load` by constructing the current system runner and delegating to `resolve_with`. Preserve the exact `canonical_remote`, UUID-v5 namespace, manifest parser, and temporary hash functions.

- [ ] **Step 4: Run all project identity tests**

Run: `nix develop path:. --command cargo test -p coding-brain-core project::tests -- --nocapture`

Expected: PASS, including existing clone-equivalence, manifest override, machine-local remote rejection, and new provenance/failure cases.

- [ ] **Step 5: Prepare the task changeset only when commit authority is granted**

```bash
git add crates/coding-brain-core/src/project.rs
git commit -m "♻️ refactor: expose bounded project resolution"
```

### Task 3: Secure Versioned Runtime Cache Database

**Files:**
- Create: `src/brain/storage/runtime_cache.rs`
- Modify: `src/brain/storage/mod.rs:1-115`
- Test: `tests/sqlite_storage.rs`

**Interfaces:**
- Consumes: `SecureDatabaseDirectory`, `SecurityError`, `StoragePaths`, `rusqlite`, and existing private SQLite configuration patterns.
- Produces: `RuntimeCacheReader::open_existing_read_only(&StoragePaths, CacheDeadline)`, `RuntimeCacheWriter::create_or_open_after_activity(&StoragePaths, CacheDeadline)`, `candidate_roots()`, `load_selected_row()`, `upsert_and_prune(CacheRow)`, `CacheRow`, `CacheProvenance`, and `StoragePaths::runtime_cache_v1()`.

**Acceptance Criteria:**
- Exact post-activity lazy creation produces only `runtime-cache-v1.sqlite3` with application id, user version 1, and one closed table.
- Existing incompatible, corrupt, unsafe, or contended cache files are bypassed without migration, repair, replacement, deletion, or quarantine.
- Pre-persistence lookup opens only an existing read-only/query-only cache and cannot create WAL/SHM files; refresh opens/creates a separate writer after activity success and performs upsert/pruning in one independent transaction capped at 256 rows.
- The schema stores only canonical stable UUID values and closed `manifest`/`network_remote` provenance; no column or enum value can encode `ProjectId::Temporary`.
- `brain.sqlite3`, `review.sqlite3`, their schema constants, migrations, and frozen-schema fixtures are byte-for-byte unaffected by cache initialization.
- Hooks use short cache-local busy/deadline behavior and losing concurrent creators return immediately.

- [ ] **Step 1: Add failing storage-contract tests**

```rust
#[test]
fn hook_creates_exact_runtime_cache_v1_without_changing_brain_schema() {
    let paths = isolated_storage_paths();
    let mut cache = RuntimeCacheWriter::create_or_open_after_activity(
        &paths,
        CacheDeadline::after(Duration::from_millis(25)),
    ).unwrap();
    assert_eq!(query_i64(cache.connection(), "PRAGMA user_version"), 1);
    assert_eq!(table_names(cache.connection()), ["project_identity_cache"]);
    assert_eq!(BRAIN_SCHEMA_VERSION, 1);
    assert!(!paths.review_db().exists());
}
```

Add cases for read-only absence, no sidecars on a hit, a v2 user version, wrong application id, a temporary-identity-shaped row, malformed/oversized evidence, symlink, mode violation, lock contention, two concurrent post-activity creators, 257 inserts, and `runtime-cache-v2.sqlite3` coexistence.

- [ ] **Step 2: Run the focused storage tests and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain --test sqlite_storage runtime_cache -- --nocapture`

Expected: FAIL because `RuntimeCache` and `StoragePaths::runtime_cache_v1` do not exist.

- [ ] **Step 3: Implement the exact v1 schema and secure open path**

```sql
CREATE TABLE project_identity_cache (
    canonical_root BLOB PRIMARY KEY NOT NULL,
    project_uuid TEXT NOT NULL CHECK(length(project_uuid) = 36),
    provenance INTEGER NOT NULL CHECK(provenance IN (1, 2)),
    evidence BLOB NOT NULL CHECK(length(evidence) BETWEEN 1 AND 65536),
    refresh_order INTEGER NOT NULL,
    row_version INTEGER NOT NULL CHECK(row_version = 1)
) STRICT;
```

Use `SecureDatabaseDirectory::prepare`, `reject_untrusted_entries`, `create_database_file`, and the same no-follow descriptor correspondence checks as Brain/Review. The reader uses SQLite read-only URI flags plus query-only, defensive, trusted-schema-off, column-count, SQL-length, and value-length limits before querying. Set a cache-specific application id and `user_version=1`; do not invoke `MigrationCoordinator` or authoritative schema helpers.

- [ ] **Step 4: Implement bounded row access and atomic pruning**

```rust
pub(crate) const MAX_RUNTIME_CACHE_ROWS: usize = 256;

pub(crate) fn candidate_roots(&self) -> Result<Vec<CacheRootKey>, RuntimeCacheBypass>;

pub(crate) fn load_selected_row(
    &self,
    key: &CacheRootKey,
) -> Result<CacheRow, RuntimeCacheBypass>;

pub(crate) fn upsert_and_prune(&mut self, row: &CacheRow) -> Result<(), RuntimeCacheBypass> {
    // BEGIN IMMEDIATE; UPSERT row; delete oldest rows excluding the refreshed root; COMMIT.
}
```

Phase 1 reads only bounded root keys/row identifiers, selects the longest component ancestor, then Phase 2 fetches exactly one evidence blob. Reject more than 256 keys, excessive total root bytes/path depth, oversized serialized evidence, unknown provenance, non-UUID values, invalid roots, and unknown row versions before use. If the selected row is invalid, miss rather than falling back to a less-specific ancestor. A hit path must issue no `INSERT`, `UPDATE`, `DELETE`, schema creation, or sidecar creation.

- [ ] **Step 5: Run cache and authoritative storage suites**

Run: `nix develop path:. --command cargo test -p coding-brain --test sqlite_storage runtime_cache -- --nocapture`

Expected: PASS.

Run: `nix develop path:. --command cargo test -p coding-brain --test sqlite_storage frozen_schema migration integrity -- --nocapture`

Expected: PASS with Brain schema version still 1.

- [ ] **Step 6: Prepare the task changeset only when commit authority is granted**

```bash
git add src/brain/storage/mod.rs src/brain/storage/runtime_cache.rs tests/sqlite_storage.rs
git commit -m "🗃️ feat: add versioned runtime cache"
```

### Task 4: Closed Dependency Evidence and Cache Validation

**Files:**
- Create: `src/lifecycle_project.rs`
- Modify: `src/main.rs:10-20`
- Test: `src/lifecycle_project.rs` unit tests

**Interfaces:**
- Consumes: Task 1 `HookBudget`, `OutputBudget`, and typed process result; Task 2 `ProjectResolution`; Task 3 `RuntimeCache` and `CacheRow`.
- Produces: `resolve_lifecycle_project(cwd, paths, reader, budget, runner) -> ResolvedLifecycleProject`, `CacheRefresh`, `DependencyEvidenceV1`, platform-specific `ClosedGitSlots`, `DependencySlot`, `ProjectCacheOutcome`, and `refresh_after_activity_success`.

**Acceptance Criteria:**
- A valid manifest or network-remote row hits without executing Git and without writing the cache.
- Lookup uses canonical component ancestry, examines at most 256 rows, and rejects intervening nested `.git` or `.coding-brain/project.toml` boundaries.
- Each dependency path is recomputed from closed slots; no cache blob supplies an arbitrary path to open.
- File identity, type, size, high-resolution metadata, and bounded content digest invalidate replacements and same-size rewrites on the next hook.
- Cold manifest/Git results are cacheable only when independently collected pre-discovery and post-discovery evidence snapshots are identical.
- Network rows include recomputed repository/common/worktree config, standard global/system candidates, supported environment digest, and current PATH-selected Git executable evidence.
- Includes, non-file origins, path-changing Git/environment overrides, relative/empty PATH components, ambiguous executables, and unrepresentable evidence make a valid result non-cacheable.
- Linux and macOS use explicit closed config-slot resolvers; every Git-reported file origin must map to a recomputed supported slot, otherwise the current result is non-cacheable.
- Temporary/failure results carry no refresh; refresh is callable only after activity success.

- [ ] **Step 1: Add a table-driven failing validation suite**

```rust
#[test]
fn valid_hit_and_every_authority_change_have_closed_outcomes() {
    for case in validation_cases() {
        let mut git = CountingRunner::new(case.git_outputs);
        let outcome = resolve_fixture(case.fixture, &mut git);
        assert_eq!(outcome.cache_outcome(), case.expected_cache_outcome, "{}", case.name);
        assert_eq!(git.call_count(), case.expected_git_calls, "{}", case.name);
    }
}
```

`validation_cases()` must include unchanged manifest/network hits; replaced/deleted/malformed/oversized/permission-denied dependencies; same-size content rewrite; synchronized mutation between pre/post snapshots; repo/common/worktree/global/system config change; env/PATH/wrapper change; include/includeIf/non-file origin; `GIT_DIR`, `GIT_WORK_TREE`, `GIT_COMMON_DIR`, `GIT_CONFIG*`; nested repo/manifest boundaries; separate worktrees; excessive depth; Linux/macOS supported slots; unknown-platform/origin fallback; and temporary/failure/noncacheable results.

- [ ] **Step 2: Run the focused tests and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain lifecycle_project::tests -- --nocapture`

Expected: FAIL because the lifecycle resolver and evidence model do not exist.

- [ ] **Step 3: Implement closed evidence types and bounded file validation**

```rust
enum DependencySlot {
    RootManifest,
    GitMarker,
    CommonConfig,
    WorktreeConfig,
    SystemConfig,
    GlobalConfig,
    XdgGlobalConfig,
    GitExecutable,
}

struct FileEvidence {
    slot: DependencySlot,
    presence: Presence,
    file_type: ClosedFileType,
    stable_identity: StableFileIdentity,
    len: u64,
    modified_ns: Option<u128>,
    digest: [u8; 32],
}
```

Serialize a versioned fixed-shape `DependencyEvidenceV1`. Recompute paths from canonical root/current environment, open no-follow regular files through bounded descriptors, cap authority/config files and component depth, and digest content read from the validated descriptor.

- [ ] **Step 4: Implement closed Git discovery eligibility**

Run Git through one `GitDiscovery` object holding a single absolute deadline and one 64 KiB `OutputBudget`. Query root, remote, and `git config --show-origin --show-scope --null` provenance needed to represent only the supported closed slots. All non-Linux parent-discovery `ps` calls likewise share one absolute 250 ms parent deadline. Use the stable identity for the current event even when evidence is valid-but-noncacheable; return temporary identity on timeout/failure and a closed diagnostic code.

Collect `DependencyEvidenceV1` immediately before and after manifest/Git resolution. Only construct `CacheRefresh` when the snapshots are byte-for-byte equal. A synchronized test hook mutates one config after Git returns but before the second snapshot; the identity is used once and no row is written.

```rust
pub(crate) struct ResolvedLifecycleProject {
    pub(crate) identity: ProjectIdentity,
    pub(crate) root: PathBuf,
    pub(crate) provenance: ProjectProvenance,
    pub(crate) cache_outcome: ProjectCacheOutcome,
    pub(crate) refresh: Option<CacheRefresh>,
}
```

- [ ] **Step 5: Prove hit, invalidation, and shared-deadline behavior**

Run: `nix develop path:. --command cargo test -p coding-brain lifecycle_project::tests -- --nocapture`

Expected: PASS; second newly opened cache connection makes zero runner calls, all authority changes miss immediately, and command two receives only the first command's remaining deadline/output bytes.

- [ ] **Step 6: Prepare the task changeset only when commit authority is granted**

```bash
git add src/main.rs src/lifecycle_project.rs
git commit -m "🔐 feat: validate cached project authority"
```

### Task 5: Reorder the Lifecycle Persistence Pipeline

**Files:**
- Modify: `src/main.rs:379-407,585-600`
- Modify: `src/lifecycle_hook.rs:1-40,452-490,956-1085`
- Test: `src/lifecycle_hook.rs` unit tests

**Interfaces:**
- Consumes: Task 1 entry `Instant`/timing, Task 4 `ResolvedLifecycleProject` and deferred `CacheRefresh`, existing `BrainDb`, `persist_recovery_event_in_order`, correlation, and activity APIs.
- Produces: `lifecycle_hook::run(provider, event, started)`, `persist_provider_hook_sqlite(..., project: &ProjectIdentity, ...)`, and the approved ordered production data flow.

**Acceptance Criteria:**
- Production order is input, parent, cache/Git project resolution, fresh Brain deadline/open, lifecycle commit, optional correlation, activity commit, then best-effort cache refresh.
- `observation_event` receives the already resolved identity and never launches Git.
- Cache/Git failure occurs before authoritative persistence; cache refresh failure after activity success cannot change authoritative success.
- Brain open begins only while the full 500 ms reserved storage window remains; its deadline is the earlier of the whole-hook deadline and 500 ms after storage open.
- Lifecycle-before-activity order, recovery-link behavior, PostToolUse exact correlation, successful-commit authority, and existing bounded diagnostics remain intact.
- Timing covers `cli_input`, `parent_discovery`, `project_cache`, optional `project_git`, `sqlite_open`, `lifecycle_commit`, optional `posttool_correlation`, `activity_commit`, optional `cache_refresh`, and `total`.

- [ ] **Step 1: Add failing ordered-stage tests with injected seams**

```rust
#[test]
fn project_resolution_precedes_brain_open_and_refresh_follows_activity() {
    let trace = Trace::default();
    run_sqlite_fixture_with_trace(&trace, HookEventClass::UserPromptSubmit).unwrap();
    assert_eq!(trace.events(), [
        "input", "parent", "project", "brain_open", "lifecycle_commit",
        "activity_commit", "cache_refresh",
    ]);
}

#[test]
fn failed_activity_never_refreshes_cache() {
    let result = run_with_fault(Fault::ActivityPreCommit);
    assert!(result.is_err());
    assert_eq!(cache_refresh_count(), 0);
}

#[test]
fn insufficient_storage_reserve_starts_no_authoritative_write() {
    let result = run_with_clock_advanced_before_storage(Duration::from_millis(1001));
    assert_eq!(result, HookOutcome::StorageReserveUnavailable);
    assert_eq!(authoritative_write_count(), 0);
}
```

- [ ] **Step 2: Run lifecycle-hook unit tests and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain lifecycle_hook::tests::project_resolution_precedes -- --nocapture`

Expected: FAIL because current production opens Brain before parent/project resolution and `observation_event` loads identity itself.

- [ ] **Step 3: Pass the earliest start instant into lifecycle dispatch**

```rust
fn main() -> io::Result<()> {
    let started = Instant::now();
    let cli = Cli::parse();
    // existing early dispatch...
    run_main(cli, started)
}

fn run_main(cli: Cli, started: Instant) -> io::Result<()> {
    if cli.lifecycle_hook {
        lifecycle_hook::run(provider, cli.antigravity_hook_event.as_deref(), started);
        return Ok(());
    }
    // existing non-hook behavior unchanged
}
```

Update direct `run_main` tests to pass `Instant::now()`; do not start a new budget after parsing.

- [ ] **Step 4: Rewire the production hook path and observation constructor**

```rust
fn observation_event(
    lifecycle: &LifecycleEvent,
    input: &LifecycleActivityInput,
    project: &ProjectIdentity,
) -> Result<ActivityEvent, String>;
```

Reserve 500 ms while allocating parent/Git allowances, resolve `ResolvedLifecycleProject` before `BrainDb::open_current`, and refuse to enter storage if the reserve is no longer available. Create `StorageDeadline` immediately before Brain open with `min(hook_deadline, storage_started + 500 ms)`, then pass the identity through `persist_provider_hook_sqlite`. Preserve existing successful-commit authority even when an entered commit crosses that deadline. Only after `append_activity_batch` succeeds and hook time remains, open/create `RuntimeCacheWriter` and call `refresh_after_activity_success`; map refresh failures to closed diagnostics without changing the authoritative result.

- [ ] **Step 5: Run lifecycle, activity, and permission regression suites**

Run: `nix develop path:. --command cargo test -p coding-brain lifecycle_hook brain::storage::lifecycle brain::storage::activity brain::permission_hook -- --nocapture`

Expected: PASS; existing lifecycle/activity ordering and permission authority assertions are unchanged.

Run: `nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings`

Expected: PASS.

- [ ] **Step 6: Prepare the task changeset only when commit authority is granted**

```bash
git add src/main.rs src/lifecycle_hook.rs
git commit -m "⚡ fix: move project resolution before lifecycle persistence"
```

### Task 6: Cross-Process, Event-Class, and Fault-Matrix Regression Coverage

**Files:**
- Modify: `tests/lifecycle_hook_cli.rs`
- Modify: `tests/live_fault_matrix.rs`
- Modify: `tests/sqlite_storage.rs`

**Interfaces:**
- Consumes: the complete binary hook pipeline from Tasks 1-5 and existing fault-injection/storage readers.
- Produces: deterministic public regression evidence for separate processes, controlled subprocess failures, cache concurrency, authoritative uncertainty, and all required lifecycle event classes.

**Acceptance Criteria:**
- Separate `cbrain` processes invoke a counting Git wrapper only for the first cacheable resolution; the second validated hit launches no Git child.
- UserPromptSubmit, PreToolUse, and PostToolUse cover miss, hit, invalidation, and Git-timeout paths; PostToolUse preserves exact correlation.
- Controlled descriptor blocking proves return below the internal budget, descendant cleanup, no partial false success, and closed timing/diagnostic output without fixed sleeps.
- Cache creation contention/corruption/unavailability/commit uncertainty never changes Brain evidence; Brain contention/storage unavailable/pre-commit failure/post-commit uncertainty retain existing authoritative classifications.
- Stdout remains empty and timing snapshots reject paths, raw remotes, commands, payloads, environment values, and free-form error strings.
- Malicious cache rows cannot affect permission decisions or authoritative audit/legacy exports; rollback binaries ignore the auxiliary filename and open Brain state normally.

- [ ] **Step 1: Add the failing cross-process counting-wrapper test**

```rust
#[test]
fn separate_hook_processes_share_a_validated_cache() {
    let fixture = GitWrapperFixture::new();
    let first = fixture.run_hook(PROMPT);
    let calls_after_first = fixture.git_invocations();
    let second = fixture.run_hook(PRE_TOOL_USE);
    assert!(first.status.success() && second.status.success());
    assert!(first.stdout.is_empty() && second.stdout.is_empty());
    assert!(calls_after_first > 0);
    assert_eq!(fixture.git_invocations(), calls_after_first);
}
```

The wrapper records only one fixed byte per invocation, never arguments or environment. For timeout tests it first writes a ready byte to a pre-opened marker FD, then blocks reading a release FD. A descendant inherits a separate liveness FD; EOF proves the complete process group exited. The harness waits for readiness before advancing the injected clock or closing the release channel, and uses its outer deadline only as a deadlock safety ceiling.

- [ ] **Step 2: Add event/invalidation/timing matrix cases and verify red**

Run: `nix develop path:. --command cargo test -p coding-brain --test lifecycle_hook_cli project_cache -- --nocapture`

Expected: FAIL before the fixture expectations are satisfied by the full pipeline.

- [ ] **Step 3: Add cache/Brain fault cases**

Extend existing `live_fault_matrix` fixtures with explicit cache pre-commit and post-commit/uncertain outcomes. After every run, reopen `brain.sqlite3` and assert exact lifecycle/activity rows rather than trusting process stderr. Feed a private cache malformed schemas, oversized blobs, invalid UUIDs, and temporary-identity-shaped values; assert permission decisions, provider success, and delivery evidence remain unchanged. Run audit and legacy exports and prove no runtime-cache row/path/evidence is included. Exercise the previous compatible binary/fixture against state containing `runtime-cache-v1.sqlite3` and prove it ignores the auxiliary file while reading authoritative state normally.

- [ ] **Step 4: Run the full focused integration and fault suites**

Run: `nix develop path:. --command cargo test -p coding-brain --test lifecycle_hook_cli --test live_fault_matrix --test sqlite_storage -- --nocapture`

Expected: PASS for all three event classes, concurrent cache creation, controlled Git timeout/output overflow, cache corrupt/incompatible/unsafe bypass, storage contention, and uncertain commits.

- [ ] **Step 5: Run the complete workspace quality gates**

Run: `nix develop path:. --command cargo fmt --check`

Expected: PASS.

Run: `nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings`

Expected: PASS.

Run: `nix develop path:. --command cargo test --workspace`

Expected: PASS with no ignored-test requirement weakened and no provider timeout increased.

- [ ] **Step 6: Prepare the task changeset only when commit authority is granted**

```bash
git add tests/lifecycle_hook_cli.rs tests/live_fault_matrix.rs tests/sqlite_storage.rs
git commit -m "🧪 test: cover lifecycle cache latency boundaries"
```

### Task 7: Installed-Candidate Evidence and Release Documentation

**Files:**
- Modify: `tests/lifecycle_hook_cli.rs` ignored measurement harness near the existing `warm_lifecycle_hook_latency_and_roundtrip`
- Modify: `.internal/research/2026-08-11-lifecycle-hook-latency-boundaries.md`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: completed release candidate, opt-in timing schema, synthetic storage generators, and the Task 6 isolated hook harness.
- Produces: sanitized cold/warm/invalidation/failure p50/p95 evidence and user-facing cache/rollback documentation.

**Acceptance Criteria:**
- An isolated HOME/XDG release-profile/Nix candidate reports cold miss, warm hit, invalidation, concurrent initialization, controlled Git timeout, and SQLite contention for UserPromptSubmit, PreToolUse, and PostToolUse.
- Synthetic production-sized state is generated; no live database/WAL is copied or mutated and no raw command, prompt, payload, remote, identifier, secret, or sensitive path is recorded.
- Reference Linux warm normal p95 is below 100 ms and controlled failures return below 1,500 ms; macOS measurements are reported separately rather than compared.
- Release notes describe `runtime-cache-v1.sqlite3` as reconstructible, explain safe bypass/rollback, and state that timeout, Brain schema, durability, and permission authority are unchanged.
- Release notes disclose that the mode-0600 private cache contains canonical local project-root metadata, while diagnostics and reports contain no paths.
- Candidate verification remains separate from versioning, publishing, installation, deployment, and production acceptance.

- [ ] **Step 1: Replace the stale aggregate smoke with a sanitized stage-aware ignored harness**

```rust
#[derive(Serialize)]
struct SanitizedLatencyEvidence {
    event: HookEventClass,
    scenario: Scenario,
    samples: usize,
    p50_ms: u64,
    p95_ms: u64,
    max_ms: u64,
    stage_ms: BTreeMap<HookStage, Percentiles>,
}
```

Generate synthetic lifecycle/activity rows to the target row count and byte size. Parse only closed timing records; assert the report structure cannot serialize raw hook input, cwd, project ID, remote, or environment. Keep wall-clock thresholds behind the explicit `CBRAIN_LATENCY_EVIDENCE=1` gate so ordinary CI uses virtual-clock correctness rather than host-speed assertions.

- [ ] **Step 2: Run the ignored harness against the release candidate**

Run: `nix develop path:. --command cargo build --release`

Expected: PASS and `target/release/cbrain` exists.

Run: `CBRAIN_LATENCY_EVIDENCE=1 CBRAIN_HOOK_TIMING=1 nix develop path:. --command cargo test --release -p coding-brain --test lifecycle_hook_cli installed_candidate_latency_evidence -- --ignored --nocapture`

Expected: PASS; sanitized JSON reports warm p95 `< 100` ms and controlled failures `< 1500` ms on reference Linux.

- [ ] **Step 3: Record evidence and update release notes**

Append the command, candidate revision, platform, synthetic row/byte counts, scenario sample counts, p50/p95/max, and closed stage distributions to the research artifact. Add a changelog entry that names `runtime-cache-v1.sqlite3`, its private canonical-root metadata, safe bypass/rollback, the explicit deferral of quarantine/obsolete-version cleanup, and unchanged authority/durability contracts; explicitly label publication, installation, and live acceptance as not performed.

- [ ] **Step 4: Run documentation and final repository checks**

Run: `rg -n "runtime-cache-v1|two-second|synchronous=FULL|p95|1500" CHANGELOG.md .internal/research/2026-08-11-lifecycle-hook-latency-boundaries.md`

Expected: matches cover cache reconstruction/rollback, unchanged timeout/durability, and sanitized acceptance measurements.

Run: `git diff --check`

Expected: no whitespace errors.

Run: `nix develop path:. --command cargo test --workspace`

Expected: PASS.

- [ ] **Step 5: Prepare the task changeset only when commit authority is granted**

```bash
git add CHANGELOG.md .internal/research/2026-08-11-lifecycle-hook-latency-boundaries.md tests/lifecycle_hook_cli.rs
git commit -m "📝 docs: record lifecycle cache latency evidence"
```

## Final Verification

- [ ] Run `nix develop path:. --command cargo fmt --check` and require PASS.
- [ ] Run `nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings` and require PASS.
- [ ] Run `nix develop path:. --command cargo test --workspace` and require PASS.
- [ ] Run the release-candidate evidence command from Task 7 and require warm p95 `< 100` ms plus controlled failure `< 1500` ms on reference Linux.
- [ ] Inspect `git diff --stat`, `git diff --check`, and `git status --short`; every changed line must trace to `codexctl-9j39s`.
- [ ] Reopen authoritative Brain state after every fault scenario; do not infer success from hook exit status, stderr, or cache contents.
- [ ] Report candidate verification separately from any unperformed commit, push, publish, install, deploy, rollback, or live production acceptance step.

## Stress Test Results: Lifecycle Project-Identity Cache Implementation Plan

### Resolved Decisions

- Split cache access into an existing-file, read-only pre-persistence reader and a separate post-activity writer/creator so a hit cannot create SQLite sidecars.
- Apply the original whole-hook deadline to bounded stdin reads; an open provider pipe yields a closed input timeout before cache or Brain work.
- Store only stable UUIDs with manifest/network provenance; remove the cache representation for temporary identities.
- Require identical independently collected dependency snapshots immediately before and after cold discovery to prevent caching a mixed TOCTOU result.
- Reserve the complete 500 ms authoritative-storage window before optional work and refuse to begin Brain persistence without it; entered successful commits remain authoritative.
- Read bounded root keys first and deserialize evidence for only the most-specific selected row; an invalid selected row is a miss, not permission to fall back outward.
- Use explicit Linux/macOS closed Git config-slot resolvers and make unknown platforms/origins conservative non-cacheable results; all parent `ps` calls share one deadline.
- Define descriptor/marker/liveness synchronization for process tests and assert zero additional Git calls after the first hook without coupling tests to a cold command count.
- Apply defensive SQLite limits and keep the auxiliary file outside authoritative export, migration, integrity, review-reset, permission, and provider-acceptance paths.

### Changes Made

- Added deadline-aware stdin work and a storage-reserve API to Task 1.
- Replaced the mutable pre-persistence cache handle with read-only reader and post-success writer interfaces in Tasks 3-5.
- Removed `project_kind` from the cache schema and tightened malformed-row tests.
- Added pre/post discovery snapshots, platform-specific origin mapping, and TOCTOU mutation tests to Task 4.
- Added two-phase maximum-size lookup and exact no-sidecar hit assertions to Task 3.
- Added storage-reserve, crossed-commit, exact process-group, malicious-cache, export-exclusion, and rollback coverage to Tasks 5-7.
- Clarified that wall-clock thresholds are gated release evidence, while normal CI uses virtual-clock correctness.
- Corrected the plan's fake-clock type example and made optional child deadlines preserve the authoritative storage reserve.

### Deferred / Parking Lot

- Corrupt-cache quarantine, obsolete cache-version pruning, future runtime-cache tables, and cache format v2 remain separate non-hook designs.
- Publishing, installation, deployment, and live production acceptance remain separate authorized stages after candidate verification.

### Confidence Assessment

- Overall: High after nine resolved branches and one reflexion pass.
- Areas of concern: platform-specific Git origin coverage and release latency distributions still require Linux plus hosted-macOS evidence; the plan deliberately makes unknown cases non-cacheable rather than guessing.
