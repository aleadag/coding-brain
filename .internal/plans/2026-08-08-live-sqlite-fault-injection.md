# Live SQLite Fault Injection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent codexctl-dzlb9.11`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Complete Task 11 with an exact 24-cell Codex, Claude, and Antigravity live-process fault matrix whose controls are absent from ordinary binaries.

**Architecture:** A non-default root-package feature compiles a one-shot fault controller into the real `cbrain` binary. The controller validates an isolated-state capability and inherited marker descriptor before storage opens; hook-owned faults execute in the permission process, while checkpoint and migration execute in a feature-gated non-hook worker followed by provider-specific restart probes.

**Tech Stack:** Rust 2024, Clap, libc descriptor APIs, Serde JSON, rusqlite with bundled SQLite, Cargo integration tests, GitHub Actions, Nix.

## Global Constraints

- The feature name is exactly `fault-injection`; `default = []` remains explicit.
- Fault activation is independent of `debug_assertions` and cannot be selected by environment variables or hook JSON.
- The stable points are `AdmissionWrite`, `InferenceExit`, `CommitBeforeCall`, `CommitAfterReturn`, `StdoutWrite`, `DeliveryWrite`, `Checkpoint`, and `MigrationPublish`.
- `CommitBeforeCall` runs immediately before `transaction.commit()`; `CommitAfterReturn` runs only after that call returns under `PRAGMA synchronous = FULL`.
- `OpenRole::Hook` must continue rejecting migration and maintenance.
- The capability is an accident barrier for isolated state, not authorization against a deliberate same-user source build.
- Marker frames fit within the POSIX minimum `PIPE_BUF` of 512 bytes, and `cbrain` restores `FD_CLOEXEC` before spawning inference.
- Official release, Nix, musl, packaging, linkage-inspection, and upload commands never enable `fault-injection`.
- `flake.nix` retains `checkType = "debug"` and `dontUseCargoParallelTests = true` unchanged.
- Do not add a SQLite VFS, arbitrary fault script, schema change, deadline increase, retry increase, or provider-protocol change.
- Preserve all unrelated working-tree changes. Do not stage, commit, push, or close `codexctl-dzlb9.11` without separate authorization and final verification.

## File Structure

- Create `src/brain/storage/fault_injection.rs`: feature-only point enum, capability reader, one-shot controller, marker framing, and worker dispatch.
- Modify `src/brain/storage/security.rs`: expose one feature-gated, descriptor-based capability opener that reuses the current safe-ancestor rules.
- Modify `src/brain/storage/mod.rs`: compile and re-export the feature-only controller surface.
- Modify `Cargo.toml`: declare the empty non-default feature.
- Modify `src/main.rs`: add feature-only hidden arguments, validate the all-or-none activation tuple before storage, and dispatch the non-hook worker.
- Create `tests/fault_injection_cli.rs`: prove default-feature absence and feature-enabled activation rejection/acceptance.
- Modify `src/brain/storage/permissions.rs`: add admission, before-call, after-return, and delivery fault points without changing unit-only permission injection.
- Modify `src/brain/permission_hook.rs`: mark inference-exit and stdout-write boundaries in the real hook path.
- Modify `src/brain/storage/maintenance.rs`: inject only the named live checkpoint fault while preserving the existing unit seam.
- Modify `src/brain/storage/migration.rs` and `src/brain/storage/security.rs`: replace debug-environment aborts with the feature controller.
- Modify `tests/storage_migration.rs`: move process-crash migration cases from the debug environment to the feature controller.
- Create `tests/live_fault_matrix.rs`: own the isolated capability/control-pipe harness and all 24 exact provider/fault cases.
- Modify `tests/release_workflow.rs` and `.github/workflows/ci.yml`: enforce separate target directories, default-feature releases, feature-enabled profile checks, and musl compilation.
- Modify `.internal/sdd/task-11-report.md`: replace the former specification blocker with final implementation evidence.

## Execution Order

Create the six implementation Beads atomically when execution begins and add a
strict blocking chain: Task 1 blocks Task 2, Task 2 blocks Task 3, Task 3 blocks
Task 4, Task 4 blocks Task 5, and Task 5 blocks Task 6. Reviews and read-only
inspection may overlap, but implementation tasks do not run concurrently
because they share the controller and matrix contracts.

## Rollback Contract

Official artifacts are feature-free, so operational rollback requires no data
or configuration action. Development rollback is atomic: revert the controller,
CLI, converted tests, CI commands, and the removal of the old debug-only
migration seam together. Never delete the feature while leaving migrated crash
tests disabled. No database conversion is needed because the implementation
does not change schemas or stored records.

---

### Task 1: Build the feature-gated controller and secure capability reader

**Files:**
- Modify: `Cargo.toml`
- Create: `src/brain/storage/fault_injection.rs`
- Modify: `src/brain/storage/security.rs`
- Modify: `src/brain/storage/mod.rs`

**Interfaces:**
- Consumes: `security::validate_safe_ancestor_metadata`, `security::validate_private_file`, resolved `CodingBrainPaths::state_root()`, and Unix `File` descriptors.
- Produces: crate-private `FaultPoint`, `FaultPosition`, closed `MigrationFaultStage`, tagged `FaultSelection`, `Activation`, `activate(Activation) -> Result<(), StorageError>`, `hit(FaultPoint, FaultPosition) -> Result<bool, StorageError>`, `hit_migration(MigrationFaultStage) -> Result<bool, StorageError>`, and `run_worker(&FaultSelection, &Path) -> Result<(), StorageError>` behind `cfg(feature = "fault-injection")`.

**Acceptance Criteria:**
- Default builds do not compile the controller module.
- Capability validation reads from one no-follow descriptor and rejects unsafe owner, mode, type, link count, ancestor, nonce, point, and state-root cases.
- The controller accepts one activation, fires the configured point once, emits one bounded marker, and rejects an invalid or inherited-leaking descriptor.
- Worker dispatch accepts only `Checkpoint`, `MigrationPublish`, and closed
  migration-regression selections and always uses non-hook storage APIs.
- Controller tests pass in debug and release profiles with the feature enabled.

- [ ] **Step 1: Add only the feature, module boundary, and closed enums**

Add to `Cargo.toml`:

```toml
[features]
default = []
fault-injection = []
```

Define crate-private `FaultPoint`, `FaultPosition`, `MigrationFaultStage`, and
`FaultSelection` enums. `FaultPoint` derives `Clone`, `Copy`, `Debug`, `Eq`,
`Ord`, `PartialEq`, `PartialOrd`, Serde, and Clap `ValueEnum`. Do not add the
controller or capability implementation in this step.

Add to `src/brain/storage/mod.rs`:

```rust
#[cfg(feature = "fault-injection")]
mod fault_injection;

#[cfg(feature = "fault-injection")]
pub(crate) use fault_injection::{
    Activation as FaultActivation, FaultPoint, FaultPosition, FaultSelection,
    MigrationFaultStage, activate as activate_fault, hit as hit_fault,
    run_worker as run_fault_worker,
};
```

- [ ] **Step 2: Add failing capability and controller unit tests**

Add tests in `src/brain/storage/fault_injection.rs` for a valid capability plus symlink, hard-link, mode `0644`, wrong owner where supported, wrong nonce, wrong point, wrong state root, duplicate initialization, duplicate hit, and wrong-point hit. Use this exact versioned payload:

```rust
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilityRecord {
    version: u8,
    state_root: PathBuf,
    nonce: String,
    selection: FaultSelection,
    control_device: u64,
    control_inode: u64,
}

const CAPABILITY_VERSION: u8 = 1;
const MAX_CAPABILITY_BYTES: u64 = 4 * 1024;
const MARKER_PREFIX: &[u8] = b"CBRAIN-FAULT-V1\0";
```

`FaultSelection` is a Serde-tagged closed enum with `Matrix(FaultPoint)` and
`MigrationRegression(MigrationFaultStage)` variants. `MigrationFaultStage` is
not an arbitrary string. It must cover the existing migration crash seams
exactly: `Building`, `Verified`, `AfterVerifiedStateTempSync`, `AfterBrainLink`,
`AfterBrainPublication`, `AfterPublishedStateTempSync`, `ReviewBeforeCreate`,
`ReviewBuilding`, `ReviewVerified`, `AfterReviewStagingSync`, `AfterReviewLink`,
`AfterReviewPublication`, `AfterReviewResultStateTempSync`, `BeforeFreezeGuard`,
`AfterFreezeBuildingStateSync`, `AfterFreezeProgressReadyStateSync`,
`FreezePreparingSynced`, `FreezeTempSynced`, `FreezePreparedRecordSynced`,
`FreezeEntryPublished`, `FreezeProgressSynced`,
`AfterDirectoryFreezingStateSync`, `AfterJournalDirectoryChmod`,
`AfterDirectoryFrozenStateSync`, `AfterManifestBuildingStateSync`,
`AfterManifestTempSync`, `AfterManifestVerifiedStateSync`,
`AfterManifestPublication`, `AfterManifestPublishedStateSync`,
`AfterLegacyFrozen`, `AfterLegacyFrozenStateTempSync`, `AfterDatabaseComplete`,
`AfterCompleteStateTempSync`, and `AfterCompleteState`. The 24-cell matrix uses
only `Matrix(FaultPoint::MigrationPublish)` at `AfterBrainPublication`; existing
migration tests use `MigrationRegression(stage)`.

Capability and hit tests construct private `Controller` instances so parallel unit tests do not share the process-global `OnceLock`. One serial global-install test covers duplicate `activate`; every integration case receives a fresh `cbrain` process. The successful instance test must read one marker and assert the second hit returns `false`. The descriptor test must spawn an exec child and prove the child sees `EBADF` for the control descriptor after activation restores `FD_CLOEXEC`.
Add descriptor rejection cases for file descriptors below 3, a read-only pipe
end, a regular-file descriptor, and a different FIFO whose `(device, inode)`
does not match the capability record.

- [ ] **Step 3: Run the new test target and capture RED**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection brain::storage::fault_injection::tests -- --test-threads=1
```

Expected: compilation fails on the missing controller and secure capability
functions, not on an unknown Cargo feature.

- [ ] **Step 4: Implement descriptor-safe capability opening**

In `src/brain/storage/security.rs`, add a feature-gated helper that traverses the capability parent with the existing `open_directory_at` and `validate_safe_ancestor_metadata` functions, opens the final component with `O_RDONLY | O_NOFOLLOW | O_CLOEXEC`, compares pre-open and opened `(dev, ino)`, and calls `validate_private_file` on descriptor metadata:

```rust
#[cfg(feature = "fault-injection")]
pub(super) fn open_fault_capability(path: &Path) -> Result<File, SecurityError> {
    let parent = path.parent().ok_or(SecurityError::Invalid("fault capability has no parent"))?;
    let name = path.file_name().ok_or(SecurityError::Invalid("fault capability has no name"))?;
    let traversal = state_root_for_traversal(parent);
    let mut directory = open_directory(if traversal.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    for component in normal_components(&traversal)? {
        validate_safe_ancestor_metadata(&EntryMetadata::from(&directory.metadata()?))?;
        directory = open_directory_at(&directory, component)?;
    }
    validate_private_directory(&directory)?;
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| SecurityError::Invalid("fault capability name is invalid"))?;
    let before = metadata_at(&directory, &c_name)?;
    validate_private_file(&before)?;
    let descriptor = open_readonly_regular_at(&directory, &c_name)?;
    let file = unsafe { File::from_raw_fd(descriptor) };
    let opened = EntryMetadata::from(&file.metadata()?);
    validate_private_file(&opened)?;
    if before.dev != opened.dev || before.ino != opened.ino {
        return Err(SecurityError::Invalid("fault capability changed during open"));
    }
    Ok(file)
}
```

- [ ] **Step 5: Implement the one-shot controller and marker framing**

Use a testable `Controller::new(Activation)` plus a production
`OnceLock<Controller>` and per-controller `AtomicBool`. `activate` installs
exactly one validated controller before any database open. It rejects
descriptors below 3, requires `fcntl(F_GETFL) & O_ACCMODE == O_WRONLY`, verifies
`fstat` reports a FIFO whose device and inode match the capability, then uses
`fcntl(F_GETFD/F_SETFD)` to restore `FD_CLOEXEC` and retains ownership of the
write descriptor. `hit` must no-op for another selection, atomically consume
the configured selection, and write exactly this frame with one `libc::write`
call:

```rust
fn marker(selection: &FaultSelection, position: FaultPosition) -> Vec<u8> {
    format!(
        "CBRAIN-FAULT-V1\0{}\0{}\0{}\n",
        selection.point_label(),
        position.as_str(),
        selection.detail_label().unwrap_or("-"),
    ).into_bytes()
}
```

Reject any frame longer than 512 bytes. Propagate marker-write failure instead of executing an unobservable fault.

Add the exact worker dispatcher as part of the controller surface so Task 2 can
compile and validate the worker CLI without depending on later fault wiring:

```rust
pub(crate) fn run_worker(
    selection: &FaultSelection,
    state_root: &Path,
) -> Result<(), StorageError> {
    let paths = StoragePaths::at(state_root);
    match selection {
        FaultSelection::Matrix(FaultPoint::Checkpoint) => {
            let deadline = StorageDeadline::after(Duration::from_secs(2));
            BrainDb::open_current(&paths, OpenRole::NonHook, deadline)?
                .maintain_bounded(None, deadline)?;
            Ok(())
        }
        FaultSelection::Matrix(FaultPoint::MigrationPublish)
        | FaultSelection::MigrationRegression(_) => {
            MigrationCoordinator::at(state_root).run_non_hook()?;
            Ok(())
        }
        _ => Err(StorageError::InvalidStorage(
            "fault point requires permission-hook role",
        )),
    }
}
```

Add unit coverage for the exhaustive role split. The owning CLI treats a
normal worker return with an unconsumed selection as a failed test invocation.

- [ ] **Step 6: Run focused GREEN and release-profile controller tests**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection brain::storage::fault_injection::tests -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-release nix develop path:. --command cargo test --release --features fault-injection brain::storage::fault_injection::tests -- --test-threads=1
```

Expected: all controller/security tests pass in both profiles.

- [ ] **Step 7: Review checkpoint**

Inspect `jj diff --stat` and `jj diff -- Cargo.toml src/brain/storage/fault_injection.rs src/brain/storage/security.rs src/brain/storage/mod.rs`. Confirm every non-test branch is behind `cfg(feature = "fault-injection")`; do not commit without explicit authorization.

---

### Task 2: Add feature-only CLI activation and prove default absence

**Files:**
- Modify: `src/main.rs`
- Create: `tests/fault_injection_cli.rs`

**Interfaces:**
- Consumes: Task 1 `FaultActivation`, `FaultPoint`, `MigrationFaultStage`, `FaultSelection`, and `activate_fault`.
- Produces: feature-only hidden arguments `--fault-point`, `--migration-fault-stage`, `--fault-capability`, `--fault-nonce`, `--fault-control-fd`, and `--fault-worker`; `fault_activation(&Cli, &Path) -> io::Result<Option<FaultActivation>>`.

**Acceptance Criteria:**
- A default debug or release binary rejects every fault argument as unknown.
- A feature binary rejects partial activation, invalid descriptors, invalid capabilities, and incompatible hook/worker combinations before opening SQLite.
- Exactly one of `--fault-point` and `--migration-fault-stage` is required for
  activation. A valid activation can accompany only `--permission-hook` or
  `--fault-worker`.
- Fault arguments remain hidden from `--help` even in a feature build.

- [ ] **Step 1: Write failing CLI integration tests**

Create `tests/fault_injection_cli.rs` with mutually exclusive profile sections:

```rust
#[cfg(not(feature = "fault-injection"))]
#[test]
fn default_binary_rejects_fault_arguments() {
    let output = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--fault-point", "admission-write"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
}

#[cfg(feature = "fault-injection")]
#[test]
fn feature_binary_hides_and_validates_fault_arguments() {
    let help = Command::new(env!("CARGO_BIN_EXE_cbrain")).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("fault-point"));
    let invalid = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args(["--permission-hook", "--fault-point", "admission-write"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("complete fault activation is required"));
}
```

Add cases proving a worker rejects hook-only points and a hook rejects `Checkpoint` and `MigrationPublish`.
Add cases proving both selectors together are rejected and the CLI selector
must exactly match the tagged selection stored in the capability.

- [ ] **Step 2: Run default and feature tests to capture RED**

Run:

```bash
nix develop path:. --command cargo test --test fault_injection_cli -- --test-threads=1
nix develop path:. --command cargo test --features fault-injection --test fault_injection_cli -- --test-threads=1
```

Expected: the default absence test passes; the feature build fails because the arguments are not defined.

- [ ] **Step 3: Add the hidden all-or-none CLI tuple**

Under `cfg(feature = "fault-injection")`, add these `Cli` fields:

```rust
#[arg(long, hide = true, value_enum)]
fault_point: Option<brain::storage::FaultPoint>,
#[arg(long, hide = true, value_enum)]
migration_fault_stage: Option<brain::storage::MigrationFaultStage>,
#[arg(long, hide = true)]
fault_capability: Option<PathBuf>,
#[arg(long, hide = true)]
fault_nonce: Option<String>,
#[arg(long, hide = true)]
fault_control_fd: Option<i32>,
#[arg(long, hide = true)]
fault_worker: bool,
```

Validate that one selector plus capability, nonce, and descriptor appear
together, resolve `CodingBrainPaths`, activate before `run_main`, and dispatch
`run_fault_worker` only when `fault_worker` is true. Reject incompatible
selections without entering the hook or worker.

- [ ] **Step 4: Run CLI GREEN in default, debug-feature, and release-feature builds**

Run:

```bash
nix develop path:. --command cargo test --test fault_injection_cli -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --features fault-injection --test fault_injection_cli -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-release nix develop path:. --command cargo test --release --features fault-injection --test fault_injection_cli -- --test-threads=1
```

Expected: all three invocations pass; the release-feature result proves no `debug_assertions` dependency.

- [ ] **Step 5: Review checkpoint**

Inspect `jj diff -- src/main.rs tests/fault_injection_cli.rs`. Confirm default `Cli` has no fault fields after cfg expansion and normal command precedence is unchanged; do not commit.

---

### Task 3: Wire hook-owned fault points at exact authority and delivery boundaries

**Files:**
- Modify: `src/brain/storage/permissions.rs`
- Modify: `src/brain/permission_hook.rs`
- Modify: `src/brain/storage/maintenance.rs`
- Test: existing unit modules in those files

**Interfaces:**
- Consumes: Task 1 `hit_fault(FaultPoint, FaultPosition) -> Result<bool, StorageError>`.
- Produces: live seams for `AdmissionWrite`, `InferenceExit`, `CommitBeforeCall`, `CommitAfterReturn`, `StdoutWrite`, `DeliveryWrite`, and the low-level `Checkpoint` error point.

**Acceptance Criteria:**
- Each selected point emits one marker at the named boundary and no marker at unrelated boundaries.
- Before-call termination leaves no permission commit; after-return termination leaves one commit with pending delivery.
- Admission and delivery failures use existing `StorageOperation` mappings.
- Graceful admission/inference failures emit Antigravity's exact native `ask` while Codex and Claude remain empty; abrupt commit crashes emit no bytes; all paths preserve zero replay.
- Existing `cfg(test)` permission and SQLite fault tests remain intact.

- [ ] **Step 1: Add failing boundary-order unit tests**

Add feature-gated tests that arm one point at a time around the existing permission methods. For commit ordering, use subprocess helpers and assert:

```rust
assert_eq!(scalar(&connection, "SELECT count(*) FROM permission_attempts WHERE attempt_state = 'evaluating'"), 1);
assert_eq!(scalar(&connection, "SELECT count(*) FROM permission_commits"), 0);

assert!(matches!(
    database.permission_state(&attempt_id).unwrap(),
    PermissionState::CommittedDeliveryUnknown(authority) if authority.action == PermissionAction::Allow
));
assert_eq!(scalar(&connection, "SELECT count(*) FROM permission_commits"), 1);
```

Add hook tests where the inference fixture exits nonzero and where `write_response` returns `BrokenPipe`; require the matching controller marker and existing exact activity states.

- [ ] **Step 2: Run focused tests to capture RED**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection brain::storage::permissions::tests -- --test-threads=1
nix develop path:. --command cargo test --features fault-injection brain::permission_hook::tests -- --test-threads=1
```

Expected: new tests fail because no live calls reach the controller.

- [ ] **Step 3: Insert the four storage-owned hook seams**

At the start of the admission transaction, return a mapped `SQLITE_FULL` when `AdmissionWrite/Before` fires. Immediately before `transaction.commit()`:

```rust
#[cfg(feature = "fault-injection")]
if super::hit_fault(FaultPoint::CommitBeforeCall, FaultPosition::Before)? {
    std::process::abort();
}
```

Immediately after a successful return:

```rust
#[cfg(feature = "fault-injection")]
if super::hit_fault(FaultPoint::CommitAfterReturn, FaultPosition::After)? {
    std::process::abort();
}
```

Before delivery commit, map a fired `DeliveryWrite/Before` to `SQLITE_IOERR_FSYNC` through the existing `StorageOperation::Delivery` path. Do not alter the existing thread-local unit seam.

If the new admission RED confirms the current Antigravity error branch returns without its native response, add the existing `write_failsafe_ask(&mut stdout, &mut stderr)` call to that graceful error branch. Do not emit it from either abrupt commit point.

- [ ] **Step 4: Mark inference-exit and stdout-write in the real hook path**

In the existing inference-error branch, call `hit_fault(InferenceExit, After)` before recording the bounded error. In the response-write error branch, call `hit_fault(StdoutWrite, After)` before recording `DeliveryEvidence::Failed`. A marker failure is a bounded hook diagnostic and cannot become an allow.

- [ ] **Step 5: Add the checkpoint low-level seam without enabling hook maintenance**

In `maintenance::checkpoint_with_seams`, when `Checkpoint/Before` fires, return a mapped `rusqlite::ffi::SQLITE_IOERR_FSYNC` before `truncate_checkpoint`. Leave `require_non_hook()` unchanged and add a regression that `OpenRole::Hook.maintain_bounded(...)` still returns `HookMaintenanceForbidden` with the feature enabled.

- [ ] **Step 6: Run focused GREEN and all existing permission tests**

Run:

```bash
nix develop path:. --command cargo test --features fault-injection brain::storage::permissions::tests -- --test-threads=1
nix develop path:. --command cargo test --features fault-injection brain::permission_hook::tests -- --test-threads=1
nix develop path:. --command cargo test -p coding-brain brain::storage::permissions::tests -- --test-threads=1
```

Expected: feature tests pass and the default unit suite retains its previous counts and ignored helpers.

- [ ] **Step 7: Review checkpoint**

Inspect the four production files. Confirm every new branch is feature-gated, exact ordering is visible beside the existing transaction calls, and no timeout, retry, role, or provider response changed; do not commit.

---

### Task 4: Replace debug migration activation and wire worker fault boundaries

**Files:**
- Modify: `src/brain/storage/fault_injection.rs`
- Modify: `src/brain/storage/migration.rs`
- Modify: `src/brain/storage/security.rs`
- Modify: `tests/storage_migration.rs`
- Modify: `tests/fault_injection_cli.rs`

**Interfaces:**
- Consumes: Task 1 `run_fault_worker(&FaultSelection, state_root)` dispatcher
  and Task 2 worker CLI.
- Produces: feature-driven migration aborts, verified checkpoint/migration
  worker behavior, and no debug environment activation.

**Acceptance Criteria:**
- `CODING_BRAIN_SQLITE_MIGRATION_FAULT` has no effect in default debug or release binaries.
- Existing migration publication crash cases run through the feature controller.
- Checkpoint worker invokes `BrainDb::maintain_bounded` as `OpenRole::NonHook`.
- Migration worker invokes `MigrationCoordinator::run_non_hook` and aborts only after the configured publication marker.
- Restart reaches one canonical migration generation and preserves exact rows.

- [ ] **Step 1: Add a failing legacy-environment regression**

In `tests/fault_injection_cli.rs`, create isolated legacy state, set `CODING_BRAIN_SQLITE_MIGRATION_FAULT=after-brain-publication`, run the default binary with `--distill-once` and no fault arguments, and assert success plus `MigrationStatus::Complete`. This test must run under `cfg(not(feature = "fault-injection"))`.

- [ ] **Step 2: Convert one migration crash case to the feature controller and capture RED**

Replace `migration_child(root, "after-brain-publication")` with a helper that
executes `CARGO_BIN_EXE_cbrain --fault-worker --migration-fault-stage
after-brain-publication` using a capability containing
`MigrationRegression(AfterBrainPublication)`. Require the exact tagged marker,
abnormal exit, then `MigrationCoordinator::resume()` returning `Complete` with
one generation. The Task 11 matrix separately invokes `--fault-point
migration-publish`, which maps to the same owning publication boundary without
using the regression selector.

Run:

```bash
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --features fault-injection --test storage_migration interrupted_migration_inspects_and_resumes_each_published_boundary -- --exact --test-threads=1
```

Expected: FAIL because worker dispatch and migration marking are not implemented.

- [ ] **Step 3: Verify exact non-hook worker behavior**

Add integration cases proving the Task 1 dispatcher invokes checkpoint through
`BrainDb` with `OpenRole::NonHook`, invokes migration through
`MigrationCoordinator::run_non_hook`, rejects hook-owned selections, and fails
when an expected fault remains unconsumed. Do not add a second dispatcher or a
hook-role bypass.

- [ ] **Step 4: Replace both debug migration environment reads**

Delete the `cfg(debug_assertions)` environment checks in
`migration::migration_fault` and `security::publish_database`. Convert each
existing internal stage label through an exhaustive
`MigrationFaultStage::from_label` match; unknown labels return `None` and never
arm. `Matrix(MigrationPublish)` matches only `AfterBrainPublication`, while
`MigrationRegression(stage)` matches its exact closed stage. Emit the tagged
marker and call `process::abort()` only after that exact match; do not expose
arbitrary stage strings.

- [ ] **Step 5: Convert the remaining integration crash helpers**

Make every `tests/storage_migration.rs` case that used `CODING_BRAIN_SQLITE_MIGRATION_FAULT` compile under `cfg(feature = "fault-injection")` and invoke the capability/pipe helper with its corresponding closed `MigrationFaultStage`. Preserve the existing expected tree snapshots, generation counts, source-race assertions, and idempotent second resume. Do not alter the separate `CODING_BRAIN_SQLITE_LEGACY_FREEZE_FAULT` unit-process seam because it remains confined to the integration-test executable rather than `CARGO_BIN_EXE_cbrain`.

- [ ] **Step 6: Run migration and worker GREEN**

Run:

```bash
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --features fault-injection --test storage_migration -- --test-threads=1
nix develop path:. --command cargo test --test fault_injection_cli default_binary_ignores_legacy_migration_fault_environment -- --exact
nix develop path:. --command cargo test --test storage_migration -- --test-threads=1
```

Expected: feature crash cases pass, the default regression passes, and the feature-free migration suite passes all remaining cases.

- [ ] **Step 7: Review checkpoint**

Run `rg -n "CODING_BRAIN_SQLITE_MIGRATION_FAULT|debug_assertions" src/brain/storage tests/storage_migration.rs`. Expected: no production migration environment read and no profile-dependent live seam; do not commit.

---

### Task 5: Add the exact 24-cell live provider matrix

**Files:**
- Create: `tests/live_fault_matrix.rs`
- Modify: `tests/hook_activity.rs` only to reuse or move existing provider fixture helpers when duplication would otherwise diverge

**Interfaces:**
- Consumes: feature-only CLI from Task 2, hook seams from Task 3, Task 1 worker
  dispatch with Task 4 fault wiring, existing provider payload fixtures, and
  public SQLite storage readers.
- Produces: test-local `ProviderCase`, `MatrixFault`, `ExpectedCell`, `PersistedSnapshot`, a `live_fault_case!` macro, and 24 named test functions; no public library fault API.

**Acceptance Criteria:**
- Exactly three providers cross exactly eight points, with no skipped pair.
- Every cell has a fresh isolated state/config root, capability, marker pipe, provider/session/turn/tool identity, and restart process.
- Assertions use identity-qualified ordered rows and exact counts, not provider aggregates.
- Every armed point produces exactly one correct marker and expected process outcome.
- Stdout bytes, native fallback, delivery state, migration generation, checkpoint preservation, and zero replay match the approved design.

- [ ] **Step 1: Write the failing matrix shape test**

Define exhaustive constants:

```rust
const PROVIDERS: [AgentProvider; 3] = [
    AgentProvider::Codex,
    AgentProvider::Claude,
    AgentProvider::Antigravity,
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MatrixFault {
    AdmissionWrite,
    InferenceExit,
    CommitBeforeCall,
    CommitAfterReturn,
    StdoutWrite,
    DeliveryWrite,
    Checkpoint,
    MigrationPublish,
}

const FAULTS: [MatrixFault; 8] = [
    MatrixFault::AdmissionWrite,
    MatrixFault::InferenceExit,
    MatrixFault::CommitBeforeCall,
    MatrixFault::CommitAfterReturn,
    MatrixFault::StdoutWrite,
    MatrixFault::DeliveryWrite,
    MatrixFault::Checkpoint,
    MatrixFault::MigrationPublish,
];

fn expected_cells() -> BTreeSet<(AgentProvider, MatrixFault)> {
    PROVIDERS.into_iter().flat_map(|provider| {
        FAULTS.into_iter().map(move |fault| (provider, fault))
    }).collect()
}
```

- [ ] **Step 2: Implement the isolated capability and pipe harness**

Create a `FaultHarness` that owns `TempDir`, capability `NamedTempFile`, pipe read descriptor, nonce, and child. Its `Drop` kills and waits for a live child. `spawn_hook` clears `FD_CLOEXEC` only on the write end immediately before `Command::spawn`, then closes the parent's write descriptor immediately after a successful spawn; the child restores close-on-exec on its copy. `read_marker` uses `poll` with a two-second deadline, reads to EOF, and compares one exact frame including any migration-stage detail. Generate the nonce from the random `NamedTempFile` filename and store the canonical state root in the JSON capability.

- [ ] **Step 3: Encode exact provider output fixtures**

Use one exhaustive function rather than parsing the binary's own output as the expectation:

```rust
fn native_fallback(provider: AgentProvider) -> &'static [u8] {
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => b"",
        AgentProvider::Antigravity => {
            br#"{"decision":"ask","reason":"Coding Brain abstained"}"#
        }
    }
}
```

For response-before-fault cells, store the current exact provider response fixture bytes alongside the provider payload fixture. Do not normalize JSON before comparison.

- [ ] **Step 4: Encode the exact durable-state table**

Define test-local `ExpectedDelivery`, `ExpectedStdout`, `ExpectedOutcome`, and `ExpectedCell` types with ordered activity states, decision count, permission-commit count, raw delivery state, expected stdout mode, expected first-process outcome, and recovery requirement. The exhaustive point match is:

```rust
match fault {
    AdmissionWrite => expected(&[], 0, 0, None, NativeFallback, Success),
    InferenceExit => expected(&[Observed, Evaluating, Error], 0, 0, None, NativeFallback, Success),
    CommitBeforeCall => expected(&[Observed, Evaluating], 0, 0, None, NoOutput, Abnormal),
    CommitAfterReturn => expected(&[Observed, Evaluating, Allowed], 1, 1, Some(Pending), NoOutput, Abnormal),
    StdoutWrite => expected(&[Observed, Evaluating, Allowed, DeliveryFailed], 1, 1, Some(Failed), ClosedPipe, Success),
    DeliveryWrite => expected(&[Observed, Evaluating, Allowed], 1, 1, Some(Pending), ExactResponse, Success),
    Checkpoint => ExpectedCell::checkpoint_composite(),
    MigrationPublish => ExpectedCell::migration_composite(),
}
```

If the owning API's current exact state differs during RED, verify it against the approved storage contract before changing this fixture; do not derive it dynamically or weaken it to containment.

Expand `live_fault_case!` exactly 24 times with names such as `codex_admission_write`, `claude_admission_write`, and `antigravity_admission_write`. Add a shape test that collects the macro's declared `(provider, point)` constants and compares them with `expected_cells()`, so a duplicate name or omitted pair fails independently of the cases themselves.

Define `MatrixFault::as_cli_value()` as an exhaustive test-local mapping to the
eight kebab-case CLI values. Do not import or expose the crate-private
production enum.

Build `PersistedSnapshot` from ordered raw SQL queries. Keep provider, session,
turn, tool-use, request key, authority action, attempt state, delivery state,
and every row count literal. Capture generated attempt, decision, and activity
IDs once; assert they are non-empty and unique, then verify every foreign key
references those exact captured IDs and that they remain unchanged after the
restart probe. Normalize timestamps only after asserting monotonic ordering and
the fixture's start/end bounds.

- [ ] **Step 5: Run the matrix to capture RED**

Run:

```bash
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --features fault-injection --test live_fault_matrix -- --test-threads=1 --nocapture
```

Expected: failures identify any seam ordering or exact-state mismatch by provider and point.

- [ ] **Step 6: Complete composite checkpoint and migration cells**

For `Checkpoint`, seed one committed sentinel WAL row, run the fault worker, assert the marker and mapped checkpoint category, then run an unarmed provider hook and prove the sentinel plus provider-qualified rows remain exact. For `MigrationPublish`, copy the existing legacy fixture, run the fault worker to abnormal exit, assert hook-native fallback while migration is incomplete, resume with an unarmed non-hook process, assert one complete generation, then run the provider hook and compare exact rows.

- [ ] **Step 7: Add the second-invocation zero-replay proof**

After every first-state assertion, run an unarmed hook with the same request identity and apply the point-specific restart contract:

- `AdmissionWrite`: the absent first attempt permits one fresh successful attempt, yielding exactly four new activity rows, one decision, one commit, and one provider response.
- `CommitBeforeCall`: admission abandons the old evaluating attempt and permits one fresh successful attempt; the final store has two attempts, the original two non-terminal rows, four new successful rows, one decision, and one commit.
- `InferenceExit`, `CommitAfterReturn`, `StdoutWrite`, and `DeliveryWrite`: the durable `needs_input` or `decided` attempt suppresses re-evaluation, emits native fallback, and does not add a decision, commit, or replayed response.
- `Checkpoint` and `MigrationPublish`: use their exact composite recovery contract and assert the sentinel or canonical generation remains unchanged on a second probe.

Every restart asserts that no second marker exists. A permitted fresh evaluation is distinguished by a new attempt ID and is not counted as replay of the faulted attempt.

- [ ] **Step 8: Run matrix GREEN in debug and release profiles**

Run:

```bash
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --features fault-injection --test live_fault_matrix -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-release nix develop path:. --command cargo test --release --features fault-injection --test live_fault_matrix -- --test-threads=1
```

Expected: 24/24 named cells pass in both profiles with no ignored cases.

- [ ] **Step 9: Review checkpoint**

Inspect failures and the final matrix table. Confirm no expectation uses `>=`, provider-wide counts, unordered containment, sleeps as readiness, or inferred marker consumption; do not commit.

---

### Task 6: Add CI isolation, package gates, and final Task 11 evidence

**Files:**
- Modify: `tests/release_workflow.rs`
- Modify: `.github/workflows/ci.yml`
- Modify: `.internal/sdd/task-11-report.md`
- Verify unchanged: `flake.nix`

**Interfaces:**
- Consumes: Task 5 `live_fault_matrix` target and the existing bundled-SQLite linkage step.
- Produces: fail-closed CI contracts for default/feature artifact separation, Linux/macOS release-profile matrix execution, musl feature compilation, and final Task 11 evidence.

**Acceptance Criteria:**
- Linux and macOS run the feature-enabled release-profile matrix from `target/fault-injection-release`.
- The OS matrix uses `strategy.fail-fast: false`, so both platform results are
  reported.
- Official release/linkage builds use `target/release-default` without the feature.
- Musl checks compile both existing targets with `fault-injection` while retaining current runtime coverage.
- Nix keeps `checkType = "debug"`, its Bubblewrap shim, and serialized tests unchanged.
- Default packageability and bundled-SQLite linkage still pass.
- Task 11 report contains fresh exact commands and results and no longer claims a specification blocker.

- [ ] **Step 1: Add failing workflow contract tests**

Extend `tests/release_workflow.rs` to require these distinct commands and reject feature use in the official release step:

```rust
assert_contract(test_job, "CARGO_TARGET_DIR=target/fault-injection-release");
assert_contract(test_job, "cargo test --locked --release --features fault-injection --test live_fault_matrix -- --test-threads=1");
assert_contract(test_job, "fail-fast: false");
assert_contract(linkage_step, "CARGO_TARGET_DIR=target/release-default");
assert!(!linkage_step.contains("fault-injection"));
assert_contract(musl_job, "cargo check --locked --release --features fault-injection --target ${{ matrix.target }}");
```

- [ ] **Step 2: Run contract RED**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow -- --test-threads=1
```

Expected: failure reports the missing separate target directories and feature matrix commands.

- [ ] **Step 3: Update CI with isolated artifacts**

Set the OS test matrix to `fail-fast: false`. In both Linux and macOS legs, add
one serial release-profile 24-cell invocation using the cached
`target/fault-injection-release`. Change the bundled-SQLite step to export
`CARGO_TARGET_DIR=target/release-default`, then run `cargo build --locked
--release` and inspect `$CARGO_TARGET_DIR/release/cbrain`. Preserve standalone
`ldd`/`otool` captures under `set -e`; never pipe the inspector directly into
`grep`, and never upload the feature-enabled directory.

In the musl job, add a feature-enabled `cargo check` for both matrix targets. Do not add the feature to existing artifact, package, or Nix commands.

- [ ] **Step 4: Run workflow GREEN and focused quality gates**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow -- --test-threads=1
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo clippy --all-targets --features fault-injection -- -D warnings
```

Expected: all workflow tests pass, formatting is clean, and Clippy reports no warnings.

- [ ] **Step 5: Run default and feature regression suites separately**

Run:

```bash
nix develop path:. --command cargo test --all-targets -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-debug nix develop path:. --command cargo test --all-targets --features fault-injection -- --test-threads=1
CARGO_TARGET_DIR=target/fault-injection-release nix develop path:. --command cargo test --release --features fault-injection --test live_fault_matrix -- --test-threads=1
```

Expected: all default tests pass; all feature tests pass; the release matrix reports all 24 cells and no ignored cells.

- [ ] **Step 6: Re-run package, linkage, and Nix gates**

Run:

```bash
CARGO_TARGET_DIR=target/release-default nix develop path:. --command cargo build --locked --release
ldd target/release-default/release/cbrain
nix develop path:. --command cargo package --workspace --allow-dirty
nix build path:. --print-build-logs
```

On macOS CI, use `otool -L target/release-default/release/cbrain`. Expected: no dynamic `libsqlite3`; every publishable crate packages; the Nix build completes with `checkType = "debug"` and feature-free checks.

- [ ] **Step 7: Update the implementation report with fresh evidence**

Replace the blocker section in `.internal/sdd/task-11-report.md` with the final 24-cell counts, controller/CLI/security tests, Linux/macOS CI contracts, musl feature checks, default and feature suite counts, package results, linkage output, and Nix store result. Distinguish commands run locally from CI-only platform coverage.

- [ ] **Step 8: Final review and consent checkpoint**

Invoke `beads-superpowers:requesting-code-review` and then `beads-superpowers:verification-before-completion`. Run `jj st`, `jj diff --stat`, and the scoped diff. If every acceptance criterion has fresh evidence, update the Task 11 Bead note. Do not close the task, stage, describe a revision, commit, push, or open a PR until the user explicitly authorizes that action.

---

## Requirement Traceability

| Approved requirement | Plan task |
| --- | --- |
| Non-default, profile-independent feature and default absence | Tasks 1, 2, 6 |
| Descriptor-safe isolated capability | Task 1 |
| One-shot marker and `FD_CLOEXEC` inheritance protection | Tasks 1, 5 |
| Six real provider-hook fault boundaries | Tasks 3, 5 |
| Honest commit-call names and `FULL` durability bracket | Tasks 3, 5 |
| Non-hook checkpoint/migration ownership | Tasks 3, 4, 5 |
| Remove debug migration environment activation | Task 4 |
| Preserve existing migration crash stages without free-form activation | Tasks 1, 4 |
| Exact 3-by-8 identity-qualified matrix and zero replay | Task 5 |
| Separate feature and official artifacts | Tasks 2, 6 |
| Linux/macOS runtime and musl compile coverage | Tasks 1, 5, 6 |
| Preserve Nix debug checks and bundled SQLite | Task 6 |
| No schema change and reversible removal | Tasks 1-6 |
| Strict serial implementation dependency chain | Tasks 1-6 |
| Tagged closed migration-regression selection | Tasks 1, 2, 4 |
| Control FIFO identity bound into the capability | Tasks 1, 5 |
| Relationally exact snapshots with bounded timestamps | Task 5 |
| Atomic development rollback preserving crash coverage | Tasks 1-6 |

## Stress Test Results: Live SQLite fault injection implementation plan

### Resolved Decisions

- Implementation is a strict Task 1 through Task 6 blocking chain. Only
  reviews and read-only inspection may overlap.
- Production fault enums remain crate-private. The integration matrix owns a
  local exhaustive `MatrixFault` and maps it to exact CLI strings.
- Task 1 first adds only the feature boundary and closed enums, then writes
  controller/security tests whose RED is caused by missing behavior rather
  than by an unknown Cargo feature.
- Activation uses a tagged closed `FaultSelection`: the 24-cell matrix selects
  `Matrix(FaultPoint)`, while existing migration crash tests select
  `MigrationRegression(MigrationFaultStage)`. No free-form stage string is
  accepted.
- `PersistedSnapshot` records literal identity-qualified rows and verifies
  foreign-key relationships, uniqueness, monotonic bounded timestamps, and
  stable generated IDs across restart. Only nondeterministic ID and timestamp
  values are normalized for comparison.
- Linux and macOS each run one serial, release-profile 24-cell matrix with
  `fail-fast: false`, isolated cached feature output, and a clean official
  target. Feature artifacts are never uploaded; musl coverage is compile-only.
- The inherited control descriptor must be at least 3, write-only, a FIFO, and
  match capability-bound `(device, inode)` before `FD_CLOEXEC` is restored.
  The nonce remains an accident barrier rather than same-user authorization.
- Operational rollback is a no-op because official artifacts omit the feature.
  Development rollback is atomic and restores the old debug migration seam
  and its tests if the replacement feature is removed; crash coverage may not
  be silently lost.

### Changes Made

- Added the strict execution chain and atomic rollback contract.
- Kept the production API crate-private and made the matrix enum test-local.
- Reordered Task 1 so its first failing test represents missing behavior.
- Added the tagged migration-regression selector, control-pipe identity checks,
  relational snapshot requirements, two-platform CI isolation, and explicit
  descriptor-substitution tests.
- Moved the exact non-hook worker dispatcher into Task 1 so Task 2's CLI can
  compile against its dependency; Task 4 now wires and verifies the owning
  migration boundaries instead of introducing the dispatcher late.

### Deferred / Parking Lot

- Musl live execution remains deferred until executable runners are available;
  both current musl targets must compile the feature.
- A deliberate same-user custom build can expose the internal CLI surface.
  This is an accepted property of a Cargo feature, not an authorization claim.

### Confidence Assessment

- Overall: High.
- Remaining implementation risks are platform-specific descriptor behavior and
  faithfully converting the existing migration crash-stage suite. The plan
  addresses both with Linux/macOS process tests, a closed stage enum, and an
  atomic rollback that preserves the old tests until their replacements pass.
