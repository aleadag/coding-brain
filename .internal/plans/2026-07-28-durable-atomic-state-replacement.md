# Durable Atomic State Replacement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Add one durable streaming replacement primitive in `coding-brain-core` and migrate lifecycle snapshots plus activity compaction without changing their locking, retention, or error behavior.

**Architecture:** `coding-brain-core::durable_file::durable_replace` owns same-directory temporary-file creation, private file modes, file syncing, atomic persistence, and Unix parent-directory syncing. Lifecycle supplies already-serialized bytes; activity supplies a streaming serializer closure. A private two-sync seam makes pre-rename and post-rename failure semantics deterministic in unit tests without widening the public API.

**Tech Stack:** Rust 2024, `std::fs`, `std::io`, `tempfile`, Cargo tests.

## Global Constraints

- Migrate only lifecycle snapshot replacement and activity-log compaction.
- The destination parent hierarchy already exists and is durably established; directory creation stays caller-owned.
- Preserve lifecycle/activity locks, the production 100 ms activity timeout, validation, schemas, retention, ordering, diagnostics, and public error types.
- Enforce replacement-file mode `0600` on Unix before and after the writer callback; existing caller directories remain `0700`.
- On Unix, sync the file and destination directory entry; on non-Unix, sync file contents and retain best-effort directory-entry crash durability.
- Add no dependencies, transaction abstraction, public fault-injection API, retries, background syncing, or configurable durability levels.
- Tests use channels or barriers and generous test-only hang deadlines, never tight elapsed-time assertions.
- Do not migrate decisions, session links, project identity, distillation, append paths, quarantine renames, or temp cleanup.
- Do not commit, push, or publish without explicit user authorization; leave verified changes in the working tree and report the recommended commit separately.

---

### Task 1: Add the core durable replacement primitive

**Files:**
- Create: `crates/coding-brain-core/src/durable_file.rs`
- Modify: `crates/coding-brain-core/src/lib.rs`

**Interfaces:**
- Consumes: Existing `tempfile = "3"` dependency and standard `File`, `Path`, `Write`, and `io::Error` types.
- Produces:

```rust
pub fn durable_replace<E, F>(
    path: &Path,
    temp_prefix: &str,
    write: F,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>;
```

**Acceptance Criteria:**
- Successful replacement exposes only complete old or new contents and leaves no matching temp file.
- Writer or file-sync failure before rename preserves the old destination.
- Directory-sync failure after rename returns an error while the complete new destination remains visible.
- Unix replacement mode is `0600` even if the writer callback changes it.
- Non-Unix directory syncing remains an explicit no-op.
- Empty, absolute, or multi-component temp prefixes return `InvalidInput` before creating a file.

- [ ] **Step 1: Export the new module and add red-first tests**

Add this export in alphabetical position in `crates/coding-brain-core/src/lib.rs`:

```rust
pub mod durable_file;
```

Create `crates/coding-brain-core/src/durable_file.rs` with imports, the private seam signature, platform helpers, and the following tests. Leave the public and private function bodies as `unimplemented!()` for this red step:

```rust
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path};

/// Atomically replaces `path` after durably syncing a same-directory temp file.
///
/// `path` must have an already-established parent hierarchy, and `temp_prefix`
/// must be one non-empty normal path component. The trusted writer callback
/// receives a private temp file; Unix mode `0600` is enforced before and after
/// the callback.
///
/// On Unix, success means both file contents and the destination-directory
/// entry completed their durability barriers. On non-Unix platforms, file
/// contents are synced and replacement is atomic, while directory-entry crash
/// durability is best-effort.
///
/// # Errors
///
/// Before replacement, an error preserves the old destination. A
/// directory-sync error occurs after replacement, so the complete new file may
/// already be visible even though crash durability is uncertain. Filesystem
/// errors are converted through `E: From<io::Error>`; callback errors retain
/// their caller-defined type.
pub fn durable_replace<E, F>(
    path: &Path,
    temp_prefix: &str,
    write: F,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
{
    let _ = (path, temp_prefix, write);
    unimplemented!()
}

fn durable_replace_with<E, F, S, D>(
    path: &Path,
    temp_prefix: &str,
    write: F,
    sync_file: S,
    sync_directory: D,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
    S: FnOnce(&File) -> io::Result<()>,
    D: FnOnce(&Path) -> io::Result<()>,
{
    let _ = (path, temp_prefix, write, sync_file, sync_directory);
    unimplemented!()
}

#[cfg(unix)]
fn set_file_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;

    fn matching_temps(parent: &Path, prefix: &str) -> Vec<std::path::PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
            .map(|entry| entry.path())
            .collect()
    }

    #[test]
    fn successful_replacement_writes_complete_contents_without_temp_leak() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        durable_replace::<io::Error, _>(&path, "state.tmp-", |file| {
            file.write_all(b"new")
        })
        .unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn writer_failure_preserves_old_contents_without_temp_leak() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let error = durable_replace::<io::Error, _>(&path, "state.tmp-", |file| {
            file.write_all(b"partial")?;
            Err(io::Error::other("writer failed"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn invalid_temp_prefixes_are_rejected_before_file_creation() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        fs::create_dir(&private).unwrap();
        let path = private.join("state.json");
        fs::write(&path, b"old").unwrap();

        for prefix in ["", "/absolute-", "../escaped-"] {
            let error = durable_replace::<io::Error, _>(&path, prefix, |file| {
                file.write_all(b"new")
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert_eq!(fs::read(&path).unwrap(), b"old");
        let entries = fs::read_dir(root.path())
            .unwrap()
            .map(Result::unwrap)
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [std::ffi::OsString::from("private")]);
    }

    #[test]
    fn file_sync_failure_preserves_old_contents() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let result = durable_replace_with::<io::Error, _, _, _>(
            &path,
            "state.tmp-",
            |file| file.write_all(b"new"),
            |_| Err(io::Error::other("file sync failed")),
            |_| Ok(()),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn directory_sync_failure_reports_error_after_complete_replacement() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        let result = durable_replace_with::<io::Error, _, _, _>(
            &path,
            "state.tmp-",
            |file| file.write_all(b"new"),
            |_| Ok(()),
            |_| Err(io::Error::other("directory sync failed")),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(matching_temps(root.path(), "state.tmp-").is_empty());
    }

    #[test]
    fn raw_readers_observe_only_complete_replacements() {
        let root = tempfile::tempdir().unwrap();
        let path = Arc::new(root.path().join("state.json"));
        let first = vec![b'a'; 64 * 1024];
        let second = vec![b'b'; 64 * 1024];
        fs::write(path.as_ref(), &first).unwrap();

        let reader_path = Arc::clone(&path);
        let reader_first = first.clone();
        let reader_second = second.clone();
        let (reader_ready_tx, reader_ready_rx) = mpsc::channel();
        let (reader_stop_tx, reader_stop_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut first_read = true;
            loop {
                let observed = fs::read(reader_path.as_ref()).unwrap();
                assert!(observed == reader_first || observed == reader_second);
                if first_read {
                    reader_ready_tx.send(()).unwrap();
                    first_read = false;
                }
                match reader_stop_rx.try_recv() {
                    Ok(()) => break,
                    Err(mpsc::TryRecvError::Empty) => std::thread::yield_now(),
                    Err(mpsc::TryRecvError::Disconnected) => {
                        panic!("reader stop channel disconnected")
                    }
                }
            }
        });

        reader_ready_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("reader exceeded test-only startup deadline");
        let writer_path = Arc::clone(&path);
        let writer_first = first.clone();
        let writer_second = second.clone();
        let (writer_done_tx, writer_done_rx) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            for index in 0..8 {
                let payload = if index % 2 == 0 {
                    &writer_second
                } else {
                    &writer_first
                };
                durable_replace::<io::Error, _>(
                    writer_path.as_ref(),
                    "state.tmp-",
                    |file| file.write_all(payload),
                )
                .unwrap();
            }
            writer_done_tx.send(()).unwrap();
        });

        writer_done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("writer exceeded test-only completion deadline");
        reader_stop_tx.send(()).unwrap();
        writer.join().unwrap();
        reader.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_reasserts_private_permissions_after_writer() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"old").unwrap();

        durable_replace::<io::Error, _>(&path, "state.tmp-", |file| {
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
            file.write_all(b"new")
        })
        .unwrap();

        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
```

- [ ] **Step 2: Run the helper tests and confirm the red state**

Run:

```bash
cargo test -p coding-brain-core durable_file::tests
```

Expected: tests fail at `unimplemented!()`; no production helper exists yet.

- [ ] **Step 3: Implement the minimal durable sequence**

Replace both `unimplemented!()` bodies with the following implementation,
retaining the public Rustdoc added in Step 1:

```rust
/// Atomically replaces `path` after durably syncing a same-directory temp file.
///
/// `path` must have an already-established parent hierarchy, and `temp_prefix`
/// must be one non-empty normal path component. The trusted writer callback
/// receives a private temp file; Unix mode `0600` is enforced before and after
/// the callback.
///
/// On Unix, success means both file contents and the destination-directory
/// entry completed their durability barriers. On non-Unix platforms, file
/// contents are synced and replacement is atomic, while directory-entry crash
/// durability is best-effort.
///
/// # Errors
///
/// Before replacement, an error preserves the old destination. A
/// directory-sync error occurs after replacement, so the complete new file may
/// already be visible even though crash durability is uncertain. Filesystem
/// errors are converted through `E: From<io::Error>`; callback errors retain
/// their caller-defined type.
pub fn durable_replace<E, F>(
    path: &Path,
    temp_prefix: &str,
    write: F,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
{
    durable_replace_with(
        path,
        temp_prefix,
        write,
        File::sync_all,
        sync_parent,
    )
}

fn durable_replace_with<E, F, S, D>(
    path: &Path,
    temp_prefix: &str,
    write: F,
    sync_file: S,
    sync_directory: D,
) -> Result<(), E>
where
    E: From<io::Error>,
    F: FnOnce(&mut File) -> Result<(), E>,
    S: FnOnce(&File) -> io::Result<()>,
    D: FnOnce(&Path) -> io::Result<()>,
{
    let mut prefix_components = Path::new(temp_prefix).components();
    if !matches!(
        (prefix_components.next(), prefix_components.next()),
        (Some(Component::Normal(_)), None)
    ) {
        return Err(E::from(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable replacement temp prefix must be one normal path component",
        )));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| E::from(io::Error::new(
            io::ErrorKind::InvalidInput,
            "durable replacement path has no parent",
        )))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(temp_prefix)
        .tempfile_in(parent)
        .map_err(E::from)?;
    set_file_mode(temporary.as_file()).map_err(E::from)?;
    write(temporary.as_file_mut())?;
    set_file_mode(temporary.as_file()).map_err(E::from)?;
    temporary.flush().map_err(E::from)?;
    sync_file(temporary.as_file()).map_err(E::from)?;
    temporary
        .persist(path)
        .map_err(|error| E::from(error.error))?;
    sync_directory(parent).map_err(E::from)
}
```

- [ ] **Step 4: Run focused core tests**

Run:

```bash
cargo test -p coding-brain-core durable_file::tests
```

Expected: all durable-file tests pass.

- [ ] **Step 5: Format and inspect the Task 1 diff**

Run:

```bash
cargo fmt
git diff --check
sed -n '1,360p' crates/coding-brain-core/src/durable_file.rs
git diff -- crates/coding-brain-core/src/lib.rs
```

Expected: formatting succeeds, `git diff --check` prints nothing, and every changed line belongs to the helper or its export.

### Task 2: Migrate lifecycle snapshot persistence

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs:1-12`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs:383-394`
- Test: `crates/coding-brain-core/src/lifecycle/store.rs:3081-3135`

**Interfaces:**
- Consumes: `coding_brain_core::durable_file::durable_replace` from Task 1 with `E = io::Error`.
- Produces: Lifecycle snapshots using the same `lifecycle.tmp-` prefix and `StoreError::Io` mapping, now with file and Unix directory durability barriers.

**Acceptance Criteria:**
- `LifecycleStore::persist` delegates replacement to the core helper.
- Existing lifecycle validation, cleanup, locking, errors, atomic visibility, and private permissions remain unchanged.
- No lifecycle timing assertion or lock timeout changes are introduced.

- [ ] **Step 1: Run existing lifecycle regressions before editing**

Run:

```bash
cargo test -p coding-brain-core lifecycle::store::tests::atomic_replacement_never_exposes_partial_json
cargo test -p coding-brain-core lifecycle::store::tests::quarantine_retention_and_abandoned_temp_cleanup_are_bounded
cargo test -p coding-brain-core lifecycle::store::tests::store_enforces_private_unix_permissions
```

Expected: all existing regressions pass, establishing the behavior to preserve.

- [ ] **Step 2: Replace the local lifecycle temp-file sequence**

Change the imports to include `io` and the helper. Keep `Write` because the
closure calls `write_all`:

```rust
use std::io::{self, Read, Write};

use crate::codex_transcript::CodexResumeEvidence;
use crate::durable_file::durable_replace;
use crate::provider::{AgentProvider, AgentSessionKey};
```

Replace `LifecycleStore::persist` with:

```rust
fn persist(&self, bytes: &[u8]) -> Result<(), StoreError> {
    durable_replace::<io::Error, _>(
        &self.snapshot_path(),
        "lifecycle.tmp-",
        |file| file.write_all(bytes),
    )
    .map_err(|_| StoreError::Io)
}
```

- [ ] **Step 3: Run focused lifecycle and helper coverage**

Run:

```bash
cargo test -p coding-brain-core durable_file::tests
cargo test -p coding-brain-core lifecycle::store::tests::atomic_replacement_never_exposes_partial_json
cargo test -p coding-brain-core lifecycle::store::tests::quarantine_retention_and_abandoned_temp_cleanup_are_bounded
cargo test -p coding-brain-core lifecycle::store::tests::store_enforces_private_unix_permissions
```

Expected: all tests pass; the existing atomic-reader test uses the core helper through `LifecycleStore`.

- [ ] **Step 4: Run the complete core crate tests**

Run:

```bash
cargo test -p coding-brain-core
```

Expected: all `coding-brain-core` tests pass.

- [ ] **Step 5: Format and inspect the Task 2 diff**

Run:

```bash
cargo fmt
git diff --check
git diff -- crates/coding-brain-core/src/lifecycle/store.rs
```

Expected: only imports and `LifecycleStore::persist` change; lock, cleanup, quarantine, and retention code remain untouched.

### Task 3: Migrate activity compaction

**Files:**
- Modify: `src/brain/activity.rs:1-25`
- Modify: `src/brain/activity.rs:523-556`
- Test: `src/brain/activity.rs:1030-2080`

**Interfaces:**
- Consumes: `coding_brain_core::durable_file::durable_replace` from Task 1 with `E = ActivityStoreError`.
- Produces: Streaming activity compaction with unchanged rows, ordering, diagnostics, retention, lock behavior, and error categories.

**Acceptance Criteria:**
- Activity compaction streams its existing diagnostics and retained events through the core helper.
- `serde_json::Error` still becomes `ActivityStoreError::Serialization`; helper filesystem errors become `ActivityStoreError::Io`.
- Compacted output is complete parseable JSONL, has `0600` mode on Unix, and leaves no `activity.tmp-*` file.
- Existing activity retention, mixed-schema, busy-lock, and private-permission tests pass.
- Focused activity formatting and regression tests pass.

- [ ] **Step 1: Add a deterministic compaction integration test**

Add `durable_compaction_writes_complete_private_jsonl_without_temp_leak` near
the existing compaction tests:

```rust
#[test]
fn durable_compaction_writes_complete_private_jsonl_without_temp_leak() {
    let (root, store) = fixture_store();
    let store = store.with_limits(ActivityLimits {
        compact_at_bytes: 1,
        retained_lifecycles: 10,
        ..ActivityLimits::default()
    });
    store
        .append(event_at("first", ActivityState::Allowed, 1))
        .unwrap();
    store
        .append(event_at("second", ActivityState::Denied, 2))
        .unwrap();

    assert!(store.compact_if_needed().unwrap());

    let path = root.path().join("activity.jsonl");
    let bytes = fs::read(&path).unwrap();
    let rows = bytes
        .split(|byte| *byte == b'\n')
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    for row in rows {
        serde_json::from_slice::<serde_json::Value>(row).unwrap();
    }
    let temps = fs::read_dir(root.path())
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("activity.tmp-")
        })
        .collect::<Vec<_>>();
    assert!(temps.is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
```

- [ ] **Step 2: Run the new integration test before migration**

Run:

```bash
cargo test durable_compaction_writes_complete_private_jsonl_without_temp_leak
```

Expected: the test may already pass because it validates preserved caller
behavior, not unobservable sync calls. Record the result; do not weaken or
replace the red-first helper tests from Task 1.

- [ ] **Step 3: Stream compaction through the core helper**

Add the helper import:

```rust
use coding_brain_core::durable_file::durable_replace;
```

Replace the local temporary-file block in `compact_if_needed` with:

```rust
durable_replace(&self.path, "activity.tmp-", |temporary| {
    if log.diagnostics.truncated_tails > 0 {
        write_diagnostic(
            temporary,
            StoreDiagnostic::TruncatedTail {
                discarded_bytes: log.diagnostics.discarded_tail_bytes,
            },
        )?;
    }
    if log.diagnostics.malformed_rows > 0 {
        write_diagnostic(
            temporary,
            StoreDiagnostic::MalformedRows {
                count: log.diagnostics.malformed_rows,
            },
        )?;
    }
    for event in &log.events {
        let activity_id = event.activity_id.as_str();
        if retained.contains(activity_id) || retained_incomplete.contains(activity_id) {
            serde_json::to_writer(&mut *temporary, event)?;
            temporary.write_all(b"\n")?;
        }
    }
    Ok::<(), ActivityStoreError>(())
})?;
Ok(true)
```

Do not change `set_file_mode`: append files and lock files still use it.

- [ ] **Step 4: Run focused activity regressions**

Run:

```bash
cargo test durable_compaction_writes_complete_private_jsonl_without_temp_leak
cargo test mixed_v1_v2_rows_read_and_compact_without_version_rewrite
cargo test v1_decision_and_v2_outcome_project_and_compact_together
cargo test compaction_preserves_all_fresh_incomplete_lifecycles
cargo test lock_wait_is_bounded_and_busy_compaction_skips
cargo test activity_storage_uses_private_permissions
```

Expected: all focused tests pass. The busy-lock test uses its existing channel
and generous test-only deadline; the production 100 ms timeout is unchanged.

- [ ] **Step 5: Run focused formatting and inspect the activity diff**

Run:

```bash
cargo fmt
cargo fmt --check
git diff --check
git diff -- src/brain/activity.rs
```

Expected: formatting succeeds, `git diff --check` prints nothing, and only the
helper import, compaction replacement block, and deterministic integration test
change.

### Task 4: Run final workspace verification and scope audit

**Files:**
- Verify: `crates/coding-brain-core/src/durable_file.rs`
- Verify: `crates/coding-brain-core/src/lib.rs`
- Verify: `crates/coding-brain-core/src/lifecycle/store.rs`
- Verify: `src/brain/activity.rs`

**Interfaces:**
- Consumes: Completed Task 2 lifecycle migration and completed Task 3 activity migration.
- Produces: Fresh full-workspace validation evidence and the conservative-profile handoff for `codexctl-va45`.

**Acceptance Criteria:**
- `cargo fmt --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `cargo build` all pass.
- The final diff contains only the core helper/export, lifecycle migration, activity migration/test, design, and plan.
- No timeout, schema, retention, configuration, or unrelated persistence path changes are present.
- The implementation task hierarchy, epic, and `codexctl-va45` close only after all gates pass.
- Working-tree status and the recommended commit are reported without committing or pushing.

- [ ] **Step 1: Run the full quality gates**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
cargo build
```

Expected: every command exits successfully with no warnings promoted by
Clippy.

- [ ] **Step 2: Audit final scope and status**

Run:

```bash
git diff --check
git status --short
git diff --stat
sed -n '1,360p' crates/coding-brain-core/src/durable_file.rs
git diff -- crates/coding-brain-core/src/lib.rs crates/coding-brain-core/src/lifecycle/store.rs src/brain/activity.rs
```

Expected: production changes are limited to the new core helper/export and the
two approved migrations; the design and plan documents are the only additional
files. No decisions, session links, project identity, distillation, timeout, or
configuration code changes.

- [ ] **Step 3: Prepare the conservative-profile handoff**

After the execution workflow closes Task 4, close the implementation epic and
originating issue only if Tasks 1-4 are closed and every Step 1 gate passed:

```bash
bd -C /home/alexander/.beads-planning close codexctl-zqmg --reason="Implemented durable atomic replacement; all child tasks and full quality gates passed."
bd -C /home/alexander/.beads-planning close codexctl-va45 --reason="Core helper plus lifecycle/activity migrations completed and verified."
```

Report:

```text
Changed files: durable helper/export, lifecycle migration, activity migration/test, design, plan
Validation: exact focused and full commands with pass/fail results
Beads: implementation tasks and codexctl-va45 status
Awaiting authorization: commit/push only if the user explicitly requests them
```

Expected: the task hierarchy and `codexctl-va45` are closed only after fresh
full-gate evidence; the handoff does not claim a commit, push, or publication
that did not occur.

## Stress Test Results: Durable replacement implementation plan

### Resolved Decisions

- **Task graph:** Lifecycle and activity migrations both depend directly on the
  core helper; neither migration falsely blocks the other.
- **Prefix safety:** Validate the public temp prefix as one non-empty normal
  path component before filesystem mutation.
- **Atomic-reader test:** Coordinate reader readiness, writer completion, and
  reader shutdown through channels with generous test-only deadlines.
- **Lifecycle scope:** Keep the lifecycle migration limited to the helper call
  and existing `StoreError::Io` mapping.
- **Final verification:** Run full workspace gates in a fourth task blocked by
  both migrations.
- **Public API:** Add Rustdoc describing preconditions, failure phases,
  portability, error conversion, and trusted-callback permissions.
- **Completion:** Link the implementation epic to `codexctl-va45`; close the
  task hierarchy only after full gates, without commit or push.

### Changes Made

- Corrected the Beads dependency graph to fan out both migrations from the core
  helper.
- Added temp-prefix validation and regressions for empty, absolute, and
  traversal-like prefixes.
- Replaced the scheduler-luck barrier test with explicit channel coordination.
- Split final workspace verification from activity migration.
- Added public durability-contract documentation.
- Added explicit post-verification Beads closure and conservative handoff.

### Deferred / Parking Lot

- Commit, push, and publication remain separately authorized actions.
- Lifecycle durability latency remains tracked by `codexctl-6wbh`.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Filesystem barrier latency is environment-dependent;
  full validation proves correctness, while `codexctl-6wbh` owns measurement.
