# Legacy Writer Guard Journal Race Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make `LegacyWriterGuard` deterministically re-enumerate a safely renamed, removed, or replaced journal during secure open without weakening legacy storage validation.

**Architecture:** Add one private journal-specific secure-open result, `Stable(File)` or `Changed`, while leaving the generic fail-closed `openat` helper untouched. Extract the journal open sequence behind a private post-open test callback, prove the current rename/removal classification fails, then change only post-open disappearance and safe identity mismatches to `Changed`; unsafe identities and unexpected I/O remain errors.

**Tech Stack:** Rust 2024, `libc` descriptor-relative filesystem calls, `fs2` advisory file locks, Cargo integration and unit tests, Nix development shell.

## Global Constraints

- Do not change `LEGACY_WRITER_LOCK_ORDER`, `LEGACY_LOCK_RETRY`, `StorageDeadline`, the two-second integration-test deadline, or public APIs.
- Keep `O_NOFOLLOW`, `O_NONBLOCK`, `O_CLOEXEC`, descriptor-relative access, and post-lock path validation.
- Validate every observed identity before classifying an identity mismatch as `Changed`.
- Only `NotFound` and mismatches among safe journal identities may re-enumerate; unsafe metadata and unexpected I/O remain `StorageError` values.
- Keep the generic `openat` helper and all non-journal legacy-source behavior unchanged.
- Do not commit, push, publish, or trigger external CI without explicit user authorization.

---

### Task 1: Classify journal namespace changes without weakening secure open

**Files:**
- Modify: `src/brain/storage/legacy.rs:1284-1402`
- Modify: `src/brain/storage/legacy.rs:1967-2115`
- Modify: `tests/storage_migration.rs:3751-3788`
- Reference: `.internal/specs/2026-08-10-legacy-writer-guard-journal-race-design.md`

**Interfaces:**
- Consumes: `JournalGuardEntry { name, identity }`, `EntryIdentity`, `validate_regular_identity`, `validate_freeze_temp_identity`, `metadata_at`, and the existing `StorageDeadline`-bounded re-enumeration loop.
- Produces: private `JournalEntryOpen::{Stable(File), Changed}`, `open_journal_entry`, `open_journal_entry_with`, and `validate_journal_entry_identity`; no public interface changes.

**Acceptance Criteria:**
- The integration regression reports `rename` or `removal` and the exact returned `StorageError` on failure.
- Deterministic tests mutate the enumerated journal after descriptor open and before post-open path validation, reproducing the current rename/removal failure before the fix.
- Safe rename, removal, and same-mode regular-file replacement return `Changed` and use the existing bounded re-enumeration path.
- Symlink, wrong-mode, wrong-owner, extra-link, unsupported-type, and unexpected-I/O observations remain errors.
- The generic `openat`, lock order, retry interval, deadline, and public API remain unchanged.
- Focused repeated tests, the storage migration target, the full serialized suite, formatting, and clippy pass locally; macOS GitHub Actions remains a separately authorized final gate.

- [ ] **Step 1: Make the end-to-end regression diagnostic without changing behavior**

Replace the boolean-only loop and assertion in `tests/storage_migration.rs` with named cases and a retained result:

```rust
#[test]
fn legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal() {
    for (case, remove) in [("rename", false), ("removal", true)] {
        let root = private_tempdir();
        let journal = create_legacy_journal(root.path(), 0);
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal)
            .unwrap();
        held.try_lock_exclusive().unwrap();
        let state_root = root.path().to_owned();
        let acquire = std::thread::spawn(move || {
            LegacyWriterGuard::acquire(&state_root, StorageDeadline::after(Duration::from_secs(2)))
        });
        let directory =
            std::fs::File::open(root.path().join("brain/permission-transactions")).unwrap();
        for _ in 0..200 {
            if directory.try_lock_exclusive().is_err() {
                break;
            }
            FileExt::unlock(&directory).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(directory.try_lock_exclusive().is_err(), "{case}");
        if remove {
            fs::remove_file(&journal).unwrap();
        } else {
            fs::rename(
                &journal,
                journal.parent().unwrap().join(legacy_journal_name(1)),
            )
            .unwrap();
        }
        FileExt::unlock(&held).unwrap();
        let result = acquire.join().unwrap();
        assert!(result.is_ok(), "{case}: {result:?}");
    }
}
```

- [ ] **Step 2: Run the unchanged-behavior regression**

Run:

```bash
nix develop path:. --command cargo test --test storage_migration legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal -- --exact --nocapture
```

Expected locally: PASS. Historical RED evidence is macOS run `31359558474`, job `93365556326`; a future failure now prints its case and `StorageError`.

- [ ] **Step 3: Extract the journal-specific open sequence with stable-path behavior preserved**

Add immediately after `JournalGuardEntry` in `src/brain/storage/legacy.rs`:

```rust
#[derive(Debug)]
enum JournalEntryOpen {
    Stable(File),
    Changed,
}

fn validate_journal_entry_identity(
    identity: EntryIdentity,
    expected: &JournalGuardEntry,
) -> Result<(), StorageError> {
    if expected.identity.mode & 0o777 == 0o400 {
        validate_freeze_temp_identity(identity)
    } else {
        validate_regular_identity(identity)
    }
}

fn open_journal_entry(
    directory: &File,
    expected: &JournalGuardEntry,
) -> Result<JournalEntryOpen, StorageError> {
    open_journal_entry_with(directory, expected, &mut || {})
}

fn open_journal_entry_with<F>(
    directory: &File,
    expected: &JournalGuardEntry,
    after_open: &mut F,
) -> Result<JournalEntryOpen, StorageError>
where
    F: FnMut(),
{
    let name = CString::new(expected.name.as_str())
        .map_err(|_| invalid("legacy journal guard name contains NUL"))?;
    let before = match metadata_at(directory, &name) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JournalEntryOpen::Changed);
        }
        Err(error) => return Err(error.into()),
    };
    validate_journal_entry_identity(before, expected)?;
    if before != expected.identity {
        return Ok(JournalEntryOpen::Changed);
    }

    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(JournalEntryOpen::Changed)
        } else {
            Err(invalid(
                "legacy source is not a safe descriptor-anchored entry",
            ))
        };
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let opened = EntryIdentity::from_metadata(&file.metadata()?);
    validate_journal_entry_identity(opened, expected)?;
    after_open();
    let after = match metadata_at(directory, &name) {
        Ok(identity) => identity,
        Err(error) => return Err(error.into()),
    };
    validate_journal_entry_identity(after, expected)?;
    if before != opened || opened != after {
        return Err(invalid("legacy source changed during descriptor open"));
    }
    Ok(JournalEntryOpen::Stable(file))
}
```

Change the start of `drain_journal_entry` from the generic `openat` call to:

```rust
let file = match open_journal_entry(directory, expected)? {
    JournalEntryOpen::Stable(file) => file,
    JournalEntryOpen::Changed => return Ok(false),
};
```

Keep the existing expected-identity check, lock attempt, deadline sleep, and
`validate_journal_entry_path` logic below it. This extraction preserves stable
entry behavior and tightens transient unsafe-identity handling by validating
each observation immediately; the failed macOS regression is the prior RED
evidence authorizing this fix path.

- [ ] **Step 4: Verify the extraction remains green**

Run:

```bash
nix develop path:. --command cargo test --test storage_migration legacy_writer_guard -- --nocapture
nix develop path:. --command cargo test brain::storage::legacy::tests --lib -- --nocapture
```

Expected: all selected tests PASS. If the extraction changes an existing result, fix the extraction before adding new expectations.

- [ ] **Step 5: Add deterministic red tests at the exact open interval**

In `src/brain/storage/legacy.rs` test module, add helpers that create an exact valid journal entry and enumerate its expected identity:

```rust
fn guard_journal_name(index: u64) -> String {
    format!(
        "permission-transaction-{:039}-{:010}-{:020}.json",
        index + 1,
        1,
        index + 1
    )
}

fn guarded_journal_fixture() -> (tempfile::TempDir, File, JournalGuardEntry, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let directory_path = root.path().join("brain/permission-transactions");
    private_directory(directory_path.parent().unwrap());
    private_directory(&directory_path);
    let path = directory_path.join(guard_journal_name(0));
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    let directory = File::open(&directory_path).unwrap();
    let expected = enumerate_journal_entries(
        &directory,
        StorageDeadline::after(Duration::from_secs(1)),
        false,
    )
    .unwrap()
    .pop()
    .unwrap();
    (root, directory, expected, path)
}
```

Add the rename test first. The callback runs after the descriptor identifies
the original file and before the helper looks up the path again:

```rust
#[test]
fn journal_open_classifies_safe_rename_as_changed() {
    let (_root, directory, expected, path) = guarded_journal_fixture();
    let renamed = path.parent().unwrap().join(guard_journal_name(1));
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::rename(&path, &renamed).unwrap();
    });
    assert!(matches!(&result, Ok(JournalEntryOpen::Changed)), "{result:?}");
}
```

Run:

```bash
nix develop path:. --command cargo test journal_open_classifies_safe_rename_as_changed --lib -- --nocapture
```

Expected RED: FAIL with the exact current post-open error, normally an I/O `NotFound`. Do not hard-code that diagnostic as the new behavior.

- [ ] **Step 6: Add removal and unsafe-replacement coverage before changing classification**

Add:

```rust
#[test]
fn journal_open_classifies_removal_as_changed() {
    let (_root, directory, expected, path) = guarded_journal_fixture();
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::remove_file(&path).unwrap();
    });
    assert!(matches!(&result, Ok(JournalEntryOpen::Changed)), "{result:?}");
}

#[test]
fn journal_open_rejects_symlink_replacement() {
    let (root, directory, expected, path) = guarded_journal_fixture();
    let outside = root.path().join("outside");
    fs::write(&outside, b"").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
    });
    assert!(matches!(&result, Err(StorageError::InvalidStorage(_))), "{result:?}");
}

#[test]
fn journal_open_classifies_safe_same_name_replacement_as_changed() {
    let (_root, directory, expected, path) = guarded_journal_fixture();
    let displaced = path.with_extension("displaced");
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::rename(&path, &displaced).unwrap();
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
    });
    assert!(matches!(&result, Ok(JournalEntryOpen::Changed)), "{result:?}");
}

#[test]
fn journal_open_rejects_wrong_mode_after_open() {
    let (_root, directory, expected, path) = guarded_journal_fixture();
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    });
    assert!(matches!(&result, Err(StorageError::InvalidStorage(_))), "{result:?}");
}

#[test]
fn journal_open_rejects_extra_link_after_open() {
    let (root, directory, expected, path) = guarded_journal_fixture();
    let result = open_journal_entry_with(&directory, &expected, &mut || {
        fs::hard_link(&path, root.path().join("alias")).unwrap();
    });
    assert!(matches!(&result, Err(StorageError::InvalidStorage(_))), "{result:?}");
}
```

Run the full helper-test group:

```bash
nix develop path:. --command cargo test journal_open_ --lib -- --nocapture
```

Expected: rename, removal, and safe same-name replacement FAIL with their exact current post-open errors; symlink, wrong-mode, and extra-link cases PASS. This isolates post-open namespace change as the missing classification while proving unsafe replacement remains fail closed.

- [ ] **Step 7: Make the minimal safe-mismatch classification change**

In `open_journal_entry_with`, preserve validation order. After validating the opened descriptor, classify a pre-open/open identity mismatch before invoking the callback:

```rust
validate_journal_entry_identity(opened, expected)?;
if opened != expected.identity {
    return Ok(JournalEntryOpen::Changed);
}
after_open();
```

Classify post-open disappearance and safe mismatch:

```rust
let after = match metadata_at(directory, &name) {
    Ok(identity) => identity,
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return Ok(JournalEntryOpen::Changed);
    }
    Err(error) => return Err(error.into()),
};
validate_journal_entry_identity(after, expected)?;
if after != expected.identity {
    return Ok(JournalEntryOpen::Changed);
}
Ok(JournalEntryOpen::Stable(file))
```

Remove the now-redundant expected-identity check immediately after `open_journal_entry` in `drain_journal_entry`; the helper has already validated and compared it. Do not change the lock loop or post-lock path validation.

- [ ] **Step 8: Verify deterministic green behavior and diagnostic integration coverage**

Run:

```bash
nix develop path:. --command cargo test journal_open_ --lib -- --nocapture
nix develop path:. --command cargo test --test storage_migration legacy_writer_guard -- --nocapture
```

Expected: all deterministic helper tests and all `legacy_writer_guard` integration tests PASS. The symlink test must still return `InvalidStorage`.

- [ ] **Step 9: Repeat the formerly flaky integration case**

Run the focused test 100 times without changing its deadline:

```bash
nix develop path:. --command bash -c '
  for run in {1..100}; do
    cargo test --test storage_migration legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal -- --exact > /tmp/codexctl-zha56-focused.log 2>&1 || {
      printf "failed iteration %s\n" "$run"
      sed -n "1,240p" /tmp/codexctl-zha56-focused.log
      exit 1
    }
  done
'
```

Expected: 100/100 PASS. The temporary log contains no credentials and may be overwritten on each iteration.

- [ ] **Step 10: Run repository quality gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test --test storage_migration -- --test-threads=1
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
git diff --check
git status --short
```

Expected: formatting, storage migration tests, the full serialized workspace suite, clippy, and diff checks PASS. Status lists only the approved zha56 implementation and its design/plan documents.

- [ ] **Step 11: Record local evidence and stop at the external-action gate**

Update `codexctl-zha56` notes with the deterministic red result, focused repeat count, and local quality-gate results. Do not close the issue yet: the acceptance criteria require a full serialized macOS GitHub Actions run. Report the exact remaining command or PR action and wait for explicit authorization to commit, push, or publish a PR.

## Stress Test Results: Implementation Plan

### Resolved Decisions

- Use the existing failed macOS regression as the RED evidence for extracting
  the journal-specific open path; do not claim transient error timing is
  perfectly unchanged.
- Keep the test callback private, generic, and invoked exactly once after a
  safe descriptor matches the enumerated identity.
- Validate every observed identity before returning `Changed`; only `NotFound`
  and safe identity mismatch are recoverable namespace churn.
- Use isolated unit fixtures for the open interval and retain the locked
  integration test as the end-to-end writer-order proof.
- Keep one implementation task because the extraction has no independent value
  without the classification fix.
- Stop after local verification and Beads notes until commit, push, PR, and
  macOS CI actions receive explicit authorization.

### Changes Made

- Borrowed `result` in every `matches!` assertion so diagnostics compile.
- Clarified the extraction's stable-path and fail-closed behavior.
- Moved the 100 focused runs inside one Nix development shell.

### Deferred / Parking Lot

- Commit, push, PR publication, and the serialized macOS GitHub Actions run are
  external gates requiring separate authorization.

### Confidence Assessment

- Overall: High
- Areas of concern: Linux can verify deterministic classification and safety,
  but only macOS CI can accept the original hosted-runner failure mode.

## Approved Hosted-macOS Amendment

The unchanged second macOS run reproduced removal after lock acquisition as
`InvalidStorage("legacy guard file is not an owner-only single-link regular file")`.
Add a deterministic unit test that opens, removes, and post-lock-validates one
journal. Then reorder only `validate_journal_entry_path`: snapshot the held
descriptor, run the existing pathname matcher, return `Changed` for a missing
or safe replacement, and validate the held descriptor when the path matches.
Repeat the focused integration test 100 times and rerun all local and hosted
macOS gates.
