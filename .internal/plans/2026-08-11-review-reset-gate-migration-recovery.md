# Review Reset Gate Migration Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make fresh and already-completed SQLite migrations establish the secure Review reset gate, and make runtime health detect Review storage failures before the TUI does.

**Architecture:** The migration coordinator acquires a migration-only exclusive Review guard before publication or completed-state repair, using the same secure descriptor and identity checks as runtime Review access. Recovery is limited to an exact `Complete`/`Published` manifest; validated legacy `DELETE` Review artifacts are normalized once to the runtime `WAL` contract with row preservation and a durable artifact update. Doctor remains read-only while validating both Brain and Review runtime opens.

**Tech Stack:** Rust 2024 workspace, `rusqlite`, `fs2`/`flock`, Unix `openat` security primitives, Cargo integration tests, feature-gated migration fault injection.

## Global Constraints

- Runtime `ReviewDb::open_current` must continue to require an existing gate and must not create one lazily.
- Migration lock order is `migration.lock` exclusive, state-root exclusive, then `review-reset.lock` exclusive; no Review-guarded path may acquire `migration.lock`.
- Only a validated `Complete` manifest with `ReviewMigrationResult::Published` may repair an absent gate.
- Existing unsafe or identity-changing gate entries are rejected unchanged; no unlink, chmod, or replacement fallback is allowed.
- Deliberately degraded Review migration remains unavailable and is not converted into empty Review state.
- Preserve Brain bytes and Review rows across repair. Review bytes may change only for the approved `DELETE`-to-`WAL` normalization, after which the manifest artifact must be updated durably.
- Doctor is read-only, reports the failing Brain or Review stage with redacted paths, and passes only when both runtime opens work.
- Do not change migration or Review schemas, runtime timeouts, dependency versions, or release versioning.
- Do not commit without explicit user authorization. At each conditional commit step, otherwise report the proposed message and continue with an uncommitted changeset.
- Use subagents only if the user selects Subagent-Driven execution. Inline execution must not dispatch subagents.
- The selected execution workflow may create, claim, and close its own epic/task Beads. Keep `codexctl-dzlb9.16.5` in progress until the user explicitly authorizes closure.
- Do not push, open a PR, sync Beads/Dolt, bump a version, publish, or release without separate explicit authority.

---

### Task 1: Establish and repair the migration-owned Review gate

**Files:**
- Modify: `src/brain/storage/security.rs`
- Modify: `src/brain/storage/mod.rs`
- Modify: `src/brain/storage/migration.rs`
- Modify: `src/runtime/brain.rs`
- Test: `tests/storage_migration.rs`
- Test: `src/runtime/brain.rs`

**Interfaces:**
- Consumes: `SecureDatabaseDirectory::open_lock_file`, `validate_lock_file`, `validate_path_correspondence`, `publish_database`, `finish_linked_publication`; existing `ReviewArtifact`, `ReviewMigrationResult`, and `verify_closed_review`.
- Produces: `SecureDatabaseDirectory::sync_lock_file(&self, name: &CStr, file: &File) -> Result<(), SecurityError>`; `acquire_review_reset_guard_for_migration(&StoragePaths) -> Result<ReviewResetGuard, StorageError>`; migration helpers that validate a published Review result and repair only its missing gate.

**Acceptance Criteria:**
- Fresh Review publication creates a durable owner-only `review-reset.lock` before canonical Review publication and returns a Review database that `ReviewDb::open_current` can open.
- A valid completed migration missing only the gate repairs under the migration lock without changing Brain bytes or Review rows; a legacy `DELETE` artifact is normalized to `WAL` and re-recorded.
- The migration guard is exclusive and retained through Review publication plus the `Published` state update.
- A degraded Review result is not repaired into an empty database or gate.
- A production-shaped migrated fixture fails runtime refresh while the gate is absent and succeeds after migration resume; reset uses the recovered gate.
- Resume succeeds while a healthy live Review connection holds the existing gate, but missing-gate repair returns `Busy` while a live connection retains the unlinked old gate inode.

- [ ] **Step 1: Write failing fresh-publication and completed-repair tests**

Add focused tests beside the existing Review migration tests in `tests/storage_migration.rs`:

```rust
#[test]
fn review_publication_creates_the_runtime_reset_gate() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());

    assert_eq!(coordinator.run_non_hook().unwrap(), MigrationStatus::Complete);
    let paths = StoragePaths::at(fixture.state_root());
    let gate = paths.db_dir().join("review-reset.lock");
    let metadata = fs::metadata(&gate).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(metadata.nlink(), 1);
    drop(
        coding_brain::brain::storage::ReviewDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(1)),
        )
        .unwrap(),
    );
}

#[test]
fn complete_published_review_repairs_only_a_missing_gate() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    assert_eq!(coordinator.run_non_hook().unwrap(), MigrationStatus::Complete);
    let paths = StoragePaths::at(fixture.state_root());
    let brain_before = fs::read(paths.brain_db()).unwrap();
    let rows_before = review_rows(&paths.review_db());
    let gate = paths.db_dir().join("review-reset.lock");
    if gate.exists() {
        fs::remove_file(&gate).unwrap();
    }
    assert!(!gate.exists());

    assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
    assert_eq!(fs::read(paths.brain_db()).unwrap(), brain_before);
    assert_eq!(review_rows(&paths.review_db()), rows_before);
    assert!(paths.db_dir().join("review-reset.lock").is_file());
    drop(
        coding_brain::brain::storage::ReviewDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(1)),
        )
        .unwrap(),
    );
}
```

Define the stable logical-row snapshot in `tests/storage_migration.rs`:

```rust
fn review_rows(path: &std::path::Path) -> Vec<(String, String, i64, String, i64)> {
    let connection = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .unwrap();
    connection
        .prepare(
            "SELECT surface, group_id, source_cursor, disposition, revision
             FROM review_marks
             ORDER BY surface, group_id, source_cursor",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn ensure_test_review_gate(paths: &StoragePaths) {
    let gate = paths.db_dir().join("review-reset.lock");
    if !gate.exists() {
        write_private(&gate, b"");
    }
}
```

Add both lock-lifetime regressions before implementation:

```rust
#[test]
fn complete_review_validation_allows_a_healthy_live_holder() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    coordinator.run_non_hook().unwrap();
    let paths = StoragePaths::at(fixture.state_root());
    ensure_test_review_gate(&paths);
    let held = coding_brain::brain::storage::ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();

    assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
    drop(held);
}

#[test]
fn complete_review_gate_repair_waits_for_unlinked_old_inode_holder() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    coordinator.run_non_hook().unwrap();
    let paths = StoragePaths::at(fixture.state_root());
    ensure_test_review_gate(&paths);
    let held = coding_brain::brain::storage::ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    fs::remove_file(paths.db_dir().join("review-reset.lock")).unwrap();

    assert!(matches!(coordinator.resume(), Err(StorageError::Busy)));
    assert!(!paths.db_dir().join("review-reset.lock").exists());
    drop(held);
    assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
}
```

In the `src/runtime/brain.rs` test module, add this complete private fixture
copier and runtime acceptance test:

```rust
fn copy_legacy_storage_fixture(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
        std::fs::create_dir_all(destination).unwrap();
        std::fs::set_permissions(
            destination,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
                std::fs::set_permissions(
                    target,
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
        }
    }

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let state_root = root.path().join("state/coding-brain");
    copy_tree(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/storage")
            .join(name),
        &state_root,
    );
    (root, state_root)
}

#[test]
fn migrated_sqlite_refresh_uses_repaired_review_gate() {
    let (_root, state_root) = copy_legacy_storage_fixture("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(&state_root);
    assert_eq!(coordinator.run_non_hook().unwrap(), MigrationStatus::Complete);
    let paths = StoragePaths::at(&state_root);
    let gate = paths.db_dir().join("review-reset.lock");
    if gate.exists() {
        std::fs::remove_file(&gate).unwrap();
    }
    assert!(!gate.exists());

    assert!(matches!(
        LiveBrainSource::refresh_from_sqlite_store(&state_root, SnapshotLimits::default()),
        Err(BrainSourceError::StorageUnavailable(_)),
    ));
    assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
    LiveBrainSource::refresh_from_sqlite_store(&state_root, SnapshotLimits::default()).unwrap();

    let held = ReviewDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    assert!(matches!(ReviewDb::reset(&paths), Err(StorageError::Busy)));
    drop(held);
    ReviewDb::reset(&paths).unwrap();
}
```

- [ ] **Step 2: Run the tests and confirm the exact regression**

Run:

```bash
nix develop path:. --command cargo test --test storage_migration review_publication_creates_the_runtime_reset_gate -- --exact
nix develop path:. --command cargo test --test storage_migration complete_published_review_repairs_only_a_missing_gate -- --exact
nix develop path:. --command cargo test --test storage_migration complete_review_validation_allows_a_healthy_live_holder -- --exact
nix develop path:. --command cargo test --test storage_migration complete_review_gate_repair_waits_for_unlinked_old_inode_holder -- --exact
nix develop path:. --command cargo test runtime::brain::tests::migrated_sqlite_refresh_uses_repaired_review_gate -- --exact
```

Expected: fresh publication, missing-gate repair, old-inode exclusion, and runtime acceptance fail for the reported defect. The healthy-holder test captures the required non-disruptive behavior and may pass before the fix.

- [ ] **Step 3: Add the secure migration guard and durability operation**

In `src/brain/storage/security.rs`, add the narrow descriptor-anchored sync operation:

```rust
pub(super) fn sync_lock_file(&self, name: &CStr, file: &File) -> Result<(), SecurityError> {
    self.validate_lock_file(name, file)?;
    self.validate_path_correspondence()?;
    file.sync_all()?;
    self.descriptor.sync_all()?;
    self.validate_lock_file(name, file)?;
    self.validate_path_correspondence()?;
    Ok(())
}
```

In `src/brain/storage/mod.rs`, give `ReviewResetGuard` small internal `validate` and `sync` methods, then add:

```rust
pub(super) fn acquire_review_reset_guard_for_migration(
    paths: &StoragePaths,
) -> Result<ReviewResetGuard, StorageError> {
    let guard = acquire_review_reset_guard(paths, true, true)?;
    guard.sync()?;
    Ok(guard)
}
```

`ReviewResetGuard::validate` must call `validate_lock_file` and `validate_path_correspondence`; `sync` must call `sync_lock_file(REVIEW_RESET_GATE_NAME, &self._gate)`. Do not alter `ReviewDb::open_current`, `create_current`, mutation, or reset locking.

- [ ] **Step 4: Make Review publication and completed recovery use the guard**

Change `publish_verified_review` to accept `paths: &StoragePaths` and split it
into two explicit phases. First inspect `PublicationPresence` and verify the
staging, linked, or canonical artifact without mutation. Only after successful
verification acquire/sync the exclusive migration guard. Revalidate the chosen
presence/identity, perform `publish_database` or `finish_linked_publication`,
persist `Published`, and validate the held guard again before returning. Do not
create the gate when artifact verification fails.

Add these migration-local helpers in `src/brain/storage/migration.rs`:

```rust
fn validate_published_review_result(
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError>;

fn repair_complete_review_gate(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError>;
```

`validate_published_review_result` must call `verify_closed_review` with the manifest artifact, row digest, and row count. `repair_complete_review_gate` must return immediately for `Degraded`; for `Published`, validate first, acquire/sync the exclusive migration guard, validate the guard again, release it, and prove `ReviewDb::open_current` works through the ordinary shared path.

Both fresh publication and completed repair must normalize a verified closed
Review artifact from `DELETE` to the runtime `WAL` contract while the exclusive
Review guard is held. Recompute and compare the logical row digest/count before
and after, sync the database, derive the new artifact, and persist that artifact
before releasing the guard. A completed artifact already in `WAL` is validated
without rewriting it. No normalization is allowed for `Degraded`, unvalidated,
or mismatched state.

Before taking the exclusive repair guard, securely inspect the gate through the
descriptor-anchored directory API. If a valid gate exists, skip exclusive
repair and prove the ordinary shared `ReviewDb::open_current` path works; this
must succeed alongside healthy live holders. If the gate is missing, acquire
the exclusive repair guard so an unlinked old-inode holder returns `Busy`. If
inspection finds an unsafe entry or races with a pathname change, return the
typed storage error without creating, removing, or replacing anything.

Call published-Review validation from `validate_state` before its `Complete` early return. In `resume_locked`, call `repair_complete_review_gate` only after `validate_state` and frozen-manifest validation while `migration.lock` remains held. Do not repair from `inspect`, hook preflight, Doctor, or runtime reads.

- [ ] **Step 5: Run focused migration and Review storage tests**

Run:

```bash
nix develop path:. --command cargo test --test storage_migration review_publication_creates_the_runtime_reset_gate -- --exact
nix develop path:. --command cargo test --test storage_migration complete_published_review_repairs_only_a_missing_gate -- --exact
nix develop path:. --command cargo test --test storage_migration review_migration
nix develop path:. --command cargo test --test sqlite_storage review_reset
nix develop path:. --command cargo test runtime::brain::tests::migrated_sqlite_refresh_uses_repaired_review_gate -- --exact
```

Expected: all pass; no test weakens existing reset or storage validation.

- [ ] **Step 6: Review Task 1 diff and conditionally commit**

Verify every changed line traces to gate lifecycle or its tests. If commit authority is granted:

```bash
git add src/brain/storage/security.rs src/brain/storage/mod.rs src/brain/storage/migration.rs src/runtime/brain.rs tests/storage_migration.rs
git commit -m "🐛 fix: recover the migrated Review reset gate"
```

Otherwise leave the task uncommitted and report that proposed message.

---

### Task 2: Prove fail-closed concurrency and restart behavior

**Files:**
- Modify: `src/brain/storage/migration.rs`
- Modify: `src/brain/storage/mod.rs`
- Modify: `src/brain/storage/security.rs`
- Test: `tests/storage_migration.rs`

**Interfaces:**
- Consumes: Task 1's `acquire_review_reset_guard_for_migration`, `validate_published_review_result`, and `repair_complete_review_gate`.
- Produces: fault boundaries `after-review-gate-sync` and `after-complete-review-gate-sync`; regression coverage for unsafe entries, an unlinked live gate holder, and deterministic restart.

**Acceptance Criteria:**
- Crashes after fresh and repair gate sync resume without manual deletion or database reset.
- A live connection holding an unlinked old gate makes repair return `Busy`; repair succeeds after the holder exits.
- Symlinked, hard-linked, wrong-mode, non-regular, foreign-owner metadata, and identity-changing gate cases remain fail-closed and unchanged.

- [ ] **Step 1: Add adversarial, characterization, and fault-injection tests**

Extend `tests/storage_migration.rs` with:

```rust
#[test]
#[cfg(feature = "fault-injection")]
fn review_gate_sync_faults_resume_without_reset() {
    for fault in ["after-review-gate-sync", "after-complete-review-gate-sync"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        if fault == "after-complete-review-gate-sync" {
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap();
            fs::remove_file(fixture.state_root().join("db/review-reset.lock")).unwrap();
        }
        assert!(!migration_child(fixture.state_root(), fault).success(), "{fault}");
        assert_eq!(
            MigrationCoordinator::at(fixture.state_root()).resume().unwrap(),
            MigrationStatus::Complete,
            "{fault}",
        );
        assert!(fixture.state_root().join("db/review-reset.lock").is_file());
    }
}
```

Add a table-driven recovery test for gate symlink, hard link, mode `0o644`, and FIFO/non-regular entry. Snapshot the tree before `resume`; assert `InvalidStorage` or the typed mapped storage fault and exact tree equality afterward. In `src/brain/storage/mod.rs`, add a deterministic unit test that acquires the migration guard, replaces the gate pathname directly, and proves the held guard's final `validate` rejects the identity change. Add a `security.rs` metadata characterization test that feeds `validate_private_file` a foreign UID to retain owner rejection without requiring privileged `chown`.

- [ ] **Step 2: Run the new tests and verify they fail for the intended reasons**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection --test storage_migration review_gate_sync_faults_resume_without_reset -- --exact
nix develop path:. --command cargo test brain::storage::security::tests::private_file_metadata_rejects_foreign_owner -- --exact
```

Expected: before the Task 2 instrumentation, the fault names are not reached. The foreign-owner characterization passes once added, proving the existing validation invariant that recovery must reuse.

- [ ] **Step 3: Add deterministic fault points and final identity checks**

Immediately after migration guard sync in fresh publication, call:

```rust
migration_fault("after-review-gate-sync");
```

Immediately after sync in completed-state repair, call:

```rust
migration_fault("after-complete-review-gate-sync");
```

Retain the exclusive guard through the applicable durable boundary and call its `validate` method afterward. Do not add sleeps, retries, broader file permissions, or replacement cleanup.

- [ ] **Step 4: Run the adversarial matrix and existing security suites**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection --test storage_migration review_gate
nix develop path:. --command cargo test --test storage_migration unsafe_review
nix develop path:. --command cargo test --test sqlite_storage review_reset
nix develop path:. --command cargo test brain::storage::security::tests
```

Expected: all pass, rejected trees remain byte-for-byte unchanged, and no fault requires manual cleanup.

- [ ] **Step 5: Review Task 2 diff and conditionally commit**

If commit authority is granted:

```bash
git add src/brain/storage/migration.rs src/brain/storage/mod.rs src/brain/storage/security.rs tests/storage_migration.rs
git commit -m "🧪 test: prove Review gate recovery boundaries"
```

Otherwise leave the task uncommitted and report that proposed message.

---

### Task 3: Validate the Review contract in Doctor

**Files:**
- Modify: `src/doctor.rs`
- Modify: `CHANGELOG.md`
- Test: `src/doctor.rs`

**Interfaces:**
- Consumes: Task 1's completed-migration repair and unchanged `ReviewDb::open_current` runtime API.
- Produces: stage-aware SQLite Doctor failures for Brain versus Review and an `[Unreleased]` compatibility note.

**Acceptance Criteria:**
- Doctor fails with Review-specific redacted evidence when Brain is healthy but Review cannot open, and passes only when both opens work.
- User-visible behavior is recorded under `[Unreleased]` without a version bump.

- [ ] **Step 1: Write the failing Doctor acceptance test**

In `src/doctor.rs`, update the passing fixture to create both databases, then add:

```rust
#[test]
fn sqlite_storage_doctor_identifies_an_unusable_review_database() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let paths = StoragePaths::at(root.path());
    drop(BrainDb::create_current(&paths).unwrap());
    drop(crate::brain::storage::ReviewDb::create_current(&paths).unwrap());
    std::fs::remove_file(paths.db_dir().join("review-reset.lock")).unwrap();

    let check = sqlite_storage_check_at(root.path());
    let json = serde_json::to_string(&check).unwrap();
    assert_eq!(check.status, CheckStatus::Fail);
    assert!(check.message.contains("Review"));
    assert!(json.contains("$XDG_STATE_HOME/coding-brain/db/review.sqlite3"));
    assert!(!json.contains(&root.path().display().to_string()));
}
```

- [ ] **Step 2: Run the Doctor test and confirm the blind spot**

Run:

```bash
nix develop path:. --command cargo test doctor::tests::sqlite_storage_doctor_identifies_an_unusable_review_database -- --exact
```

Expected: Doctor incorrectly passes Brain-only health even though the Review gate is missing.

- [ ] **Step 3: Make Doctor stage-aware without adding a second check**

Add a private stage and typed failure in `src/doctor.rs`:

```rust
#[derive(Clone, Copy)]
enum SqliteStorageStage {
    Migration,
    Brain,
    Review,
}

impl SqliteStorageStage {
    fn label(self) -> &'static str {
        match self {
            Self::Migration => "migration",
            Self::Brain => "Brain",
            Self::Review => "Review",
        }
    }

    fn redacted_path(self) -> &'static str {
        match self {
            Self::Migration => "$XDG_STATE_HOME/coding-brain/db",
            Self::Brain => "$XDG_STATE_HOME/coding-brain/db/brain.sqlite3",
            Self::Review => "$XDG_STATE_HOME/coding-brain/db/review.sqlite3",
        }
    }
}

struct SqliteStorageCheckFailure {
    stage: SqliteStorageStage,
    error: StorageError,
}
```

Change `sqlite_storage_check_at` to tag coordinator inspection failures as
`Migration`, open/health-check Brain as `Brain`, then open Review as `Review`
with the same bounded deadline. Change `sqlite_storage_check_from_result` to
accept `Result<StorageHealth, SqliteStorageCheckFailure>` and render the stage
plus fixed category with the matching redacted path. Update the existing direct
error-category unit test to construct a typed `Brain` failure, and add a
`Migration` redaction assertion. Keep successful `StorageHealth` evidence
unchanged.

- [ ] **Step 4: Add the changelog entry and run focused consumer tests**

Under `CHANGELOG.md` → `[Unreleased]` → `Fixed`, add:

```markdown
- SQLite migration now creates and safely recovers the Review reset gate before
  reporting healthy Review storage. Doctor validates both Brain and Review, so
  an unusable migrated Review database is reported before the TUI opens it.
```

Run:

```bash
nix develop path:. --command cargo test doctor::tests::sqlite_storage
nix develop path:. --command cargo test runtime::brain::tests::migrated_sqlite_refresh_uses_repaired_review_gate -- --exact
nix develop path:. --command cargo test runtime::brain::tests::live_refresh_reads_sqlite_without_creating_legacy_state -- --exact
```

Expected: all pass; Doctor output remains redacted and runtime creates no legacy state.

- [ ] **Step 5: Run workspace quality gates**

Create one isolated state root while retaining the configured Cargo cache, then
run every required gate against the first attempt:

```bash
review_gate_test_root=$(mktemp -d)
install -d -m 700 "$review_gate_test_root/home" "$review_gate_test_root/config" "$review_gate_test_root/state" "$review_gate_test_root/cache"
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build
env HOME="$review_gate_test_root/home" XDG_CONFIG_HOME="$review_gate_test_root/config" XDG_STATE_HOME="$review_gate_test_root/state" XDG_CACHE_HOME="$review_gate_test_root/cache" CARGO_HOME=/home/alexander/.cargo nix develop path:. --command cargo test
env HOME="$review_gate_test_root/home" XDG_CONFIG_HOME="$review_gate_test_root/config" XDG_STATE_HOME="$review_gate_test_root/state" XDG_CACHE_HOME="$review_gate_test_root/cache" CARGO_HOME=/home/alexander/.cargo nix develop path:. --command cargo test --features fault-injection --test storage_migration
```

Expected: every command exits 0. Do not classify a rerun as a flake; if any first attempt fails, retain and diagnose the exact output before retrying.

- [ ] **Step 6: Inspect the final surgical diff and conditionally commit**

Confirm the diff contains only the approved spec, plan, gate lifecycle, tests, Doctor/runtime validation, and changelog entry. If commit authority is granted:

```bash
git add .internal/specs/2026-08-11-review-reset-gate-migration-recovery-design.md .internal/plans/2026-08-11-review-reset-gate-migration-recovery.md src/brain/storage/security.rs src/brain/storage/mod.rs src/brain/storage/migration.rs src/doctor.rs src/runtime/brain.rs tests/storage_migration.rs CHANGELOG.md
git commit -m "🐛 fix: recover migrated Review storage (codexctl-dzlb9.16.5)"
```

Otherwise leave all work uncommitted, report validation evidence and proposed commit message, and wait for authorization.

## Stress Test Results: Review Reset Gate Implementation Plan

### Resolved Decisions

- The unlinked-old-inode holder regression moves before implementation so the
  exclusive repair lock is proven red-to-green.
- Review publication uses separate verify, exclusive-gate, revalidate,
  publication, and durable-state phases; invalid staging cannot create a gate.
- Healthy existing gates use ordinary shared validation, avoiding `Busy` for a
  live TUI; exclusive locking is reserved for genuinely missing-gate repair.
- Test helpers and runtime acceptance are fully specified, including private
  fixture copying, stable logical rows, and reset-lock behavior.
- Pre-fix fixtures conditionally establish or remove only test-local gates so
  failures reach repair and old-inode behavior instead of panicking in setup.
- Doctor failures carry typed Migration, Brain, or Review provenance and use
  the matching redacted path.
- Workspace gates include the required build and exact isolated HOME/XDG state
  for first-attempt test evidence.
- Subagent, tracker, commit, push, PR, sync, version, and release authority are
  explicit and remain separately gated.
- The red tests exposed that historical completed Review artifacts are closed in
  `DELETE` while runtime requires `WAL`; the user approved a one-time guarded
  normalization with logical-row verification and durable artifact replacement.

### Changes Made

- Reordered TDD coverage and added the healthy-live-holder regression.
- Refined publication and completed-repair control flow to avoid premature gate
  creation and unnecessary exclusive locking.
- Replaced prose-only test instructions with executable Rust snippets.
- Corrected pre-fix fixture setup for the already-missing production gate.
- Added typed Doctor stages and reproducible workspace commands.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: the implementer must preserve the two-phase publication
  identity checks without duplicating or weakening existing security helpers.
