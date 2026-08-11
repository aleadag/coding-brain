# Legacy Writer Guard Descriptor-open Race Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make `LegacyWriterGuard` re-enumerate a journal removed after `openat` but before descriptor validation, without weakening unsafe-storage rejection.

**Architecture:** Move the existing private test callback to the newly observed race boundary immediately after `openat`. Snapshot the opened descriptor, classify and validate the pathname first, and validate the descriptor before returning `Stable`; a disappeared or safely changed pathname discards the unread descriptor as `Changed` and reuses the existing bounded enumeration loop.

**Tech Stack:** Rust 2024, Unix `openat`/`fstatat` through `libc`, `fs2` advisory locks, Cargo tests, Nix development shell, GitHub Actions macOS runners.

## Global Constraints

- Rebase the local worktree onto current `origin/main` before implementation; do not duplicate the already-upstream original zha56 patch.
- Do not change `LEGACY_WRITER_LOCK_ORDER`, `LEGACY_LOCK_RETRY`, `StorageDeadline`, enumeration bounds, the two-second integration deadline, or public APIs.
- Keep `O_NOFOLLOW`, `O_NONBLOCK`, `O_CLOEXEC`, descriptor-relative access, and both successful-lock and contended-lock pathname validation.
- A descriptor discarded after confirmed namespace churn must remain unread and untrusted.
- Unsafe pathname identities, matching-path unsafe descriptors, and unexpected I/O remain fail-closed `StorageError` values.
- Keep the generic secure-open helper and all non-journal legacy-source behavior unchanged.
- Do not commit, push, publish a PR, rerun hosted CI, or close `codexctl-zha56` without the authorization required by repository policy.

## Existing Beads

- Reuse epic `codexctl-g73cj`; do not create a duplicate execution epic.
- Claim local implementation task `codexctl-0t6ma` for Task 1.
- Leave hosted acceptance task `codexctl-ncvn8` blocked by Task 1 until publication is separately authorized.

---

### Task 1: Close the descriptor-open removal interval

**Files:**
- Modify: `src/brain/storage/legacy.rs:1318-1376`
- Exercise existing unit tests: `src/brain/storage/legacy.rs:2107-2259`
- Exercise existing integration test: `tests/storage_migration.rs:4560-4598`
- Reference: `.internal/specs/2026-08-10-legacy-writer-guard-journal-race-design.md`

**Interfaces:**
- Consumes: `open_journal_entry_with<F>(directory: &File, expected: &JournalGuardEntry, after_open: &mut F) -> Result<JournalEntryOpen, StorageError>`, `EntryIdentity`, `metadata_at`, and `validate_journal_entry_identity`.
- Produces: the same private function signature and `JournalEntryOpen::{Stable(File), Changed}` contract; no public interface changes.

**Acceptance Criteria:**
- Removing the journal immediately after `openat` deterministically reproduces the recurrent zero-link `InvalidStorage` result before the production correction.
- The corrected helper returns `Changed` for disappearance and safe identity churn at that boundary.
- The helper returns `Stable` only after pathname and descriptor identities match the enumerated device, inode, type, mode, owner, and link count.
- Symlink, wrong-mode, wrong-owner, extra-link, unsupported-type, and unexpected-I/O observations remain errors.
- Both downstream lock outcomes retain pathname validation, and all existing deadline and lock-order tests pass unchanged.
- Focused tests pass 100 consecutive times; storage migration, the full serialized suite, formatting, clippy, and whitespace checks pass locally.

- [ ] **Step 1: Establish the current-main baseline**

Before changing source, obtain explicit authority for any documentation checkpoint needed to make the worktree clean. Then run:

```bash
git rev-parse HEAD
git fetch origin main
git rebase origin/main
git status --short --branch
git cherry origin/main HEAD
git diff origin/main -- src/brain/storage/legacy.rs tests/storage_migration.rs
```

Record the pre-rebase SHA as the restore point. Expected: Git skips the patch-equivalent original zha56 commit, replays only unique documentation commits, status is clean, and there is no source/test diff from `origin/main`. If rebase reports an unexpected source conflict or does not automatically skip the original patch, run `git rebase --abort`, preserve the output, and stop for review rather than resolving it speculatively.

Run the unchanged focused baseline:

```bash
nix develop path:. --command cargo test journal_open_ --lib -- --nocapture
nix develop path:. --command cargo test --test storage_migration legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal -- --exact --nocapture
```

Expected: all selected tests PASS before moving the callback.

- [ ] **Step 2: Move only the deterministic callback to obtain RED**

In `open_journal_entry_with`, move `after_open()` directly below `File::from_raw_fd` while retaining the current validation order:

```rust
let file = unsafe { File::from_raw_fd(descriptor) };
after_open();
let opened = EntryIdentity::from_metadata(&file.metadata()?);
validate_journal_entry_identity(opened, expected)?;
if opened != expected.identity {
    return Ok(JournalEntryOpen::Changed);
}
```

Do not change production behavior elsewhere; `open_journal_entry` still supplies a no-op callback.

Run:

```bash
nix develop path:. --command cargo test journal_open_classifies_removal_as_changed --lib -- --nocapture
```

Expected: FAIL with `Err(InvalidStorage("legacy guard file is not an owner-only single-link regular file"))`. This is the deterministic RED matching macOS job `93744653412`.

Before continuing, retain evidence that the command exited nonzero, only `journal_open_classifies_removal_as_changed` failed, and its diagnostic exactly matched the recurrent error. A different failure returns the task to root-cause investigation.

- [ ] **Step 3: Classify the pathname before trusting the descriptor**

Replace the post-`openat` portion of `open_journal_entry_with` with:

```rust
let file = unsafe { File::from_raw_fd(descriptor) };
after_open();
let opened = EntryIdentity::from_metadata(&file.metadata()?);
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
validate_journal_entry_identity(opened, expected)?;
if opened != expected.identity {
    return Ok(JournalEntryOpen::Changed);
}
Ok(JournalEntryOpen::Stable(file))
```

This validates any still-named replacement before classifying it, discards an unread descriptor when the name disappeared or safely changed, and validates the descriptor before `Stable`.

- [ ] **Step 4: Verify GREEN and the fail-closed identity matrix**

Run:

```bash
nix develop path:. --command cargo test journal_open_classifies_removal_as_changed --lib -- --nocapture
nix develop path:. --command cargo test journal_open_ --lib -- --nocapture
nix develop path:. --command cargo test journal_path_ --lib -- --nocapture
nix develop path:. --command cargo test journal_identity_ --lib -- --nocapture
nix develop path:. --command cargo test --test storage_migration legacy_writer_guard -- --nocapture
```

Expected: all selected tests PASS. In particular, safe removal/rename/replacement return `Changed`, while unsafe replacement and identity tests still return `InvalidStorage`.

- [ ] **Step 5: Repeat the hosted-failure-shaped integration test**

Run 100 iterations inside one Nix development shell:

```bash
nix develop path:. --command bash -c '
  for zha56_run in {1..100}; do
    cargo test --test storage_migration legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal -- --exact > /tmp/codexctl-zha56-focused.log 2>&1 || {
      printf "failed iteration %s\n" "$zha56_run"
      sed -n "1,240p" /tmp/codexctl-zha56-focused.log
      exit 1
    }
  done
'
```

Expected: 100/100 PASS without changing the two-second deadline.

- [ ] **Step 6: Run local repository gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test --test storage_migration -- --test-threads=1
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
git diff --check
git status --short
```

Expected: formatting, 100% of storage migration tests, the full serialized suite, clippy with denied warnings, and whitespace checks PASS. If an unrelated nondeterministic test fails, capture its exact output and compare a focused rerun/current-main baseline; do not weaken or skip it under zha56.

- [ ] **Step 7: Review scope and record local evidence**

Run:

```bash
git diff -- src/brain/storage/legacy.rs tests/storage_migration.rs
rg -n 'LEGACY_WRITER_LOCK_ORDER|LEGACY_LOCK_RETRY|open_journal_entry_with|validate_journal_entry_path|journal_entry_path_matches' src/brain/storage/legacy.rs
```

Expected: the implementation diff only moves the private callback and reorders journal-specific validation; the integration test, generic secure-open helper, constants, downstream path checks, and public APIs are unchanged.

Confirm both unchanged unexpected-I/O arms still propagate errors directly. Do not introduce an additional callback or abstraction solely to inject an error into these unchanged branches.

Update `codexctl-zha56` notes with the exact RED result, focused counts, and local gate results. Request separate authorization before creating an implementation commit; the proposed atomic message is:

```text
🐛 fix: close zha56 descriptor-open removal race
```

Do not push, publish, trigger hosted CI, or close the root Bead in this task.

---

### Task 2: Obtain hosted macOS acceptance

**Files:**
- No source changes expected.
- Record evidence in: Bead `codexctl-zha56` and the authorized draft PR.

**Interfaces:**
- Consumes: the reviewed Task 1 candidate SHA and its complete local verification evidence.
- Produces: focused and full serialized macOS acceptance evidence tied to an unchanged candidate SHA.

**Acceptance Criteria:**
- Publication, push, PR creation, and CI reruns occur only after explicit authorization.
- The exact journal unit tests, the end-to-end rename/removal regression, and the full serialized macOS job pass on the candidate SHA.
- At least one unchanged macOS repetition passes, preventing a single favorable schedule from being treated as acceptance.
- Any unrelated CI failure is separated by exact test, job, commit, and environment evidence.
- `codexctl-zha56` is closed only after all acceptance criteria are documented.

- [ ] **Step 1: Stop at the external-action gate**

Present the Task 1 diff, local results, proposed commit, branch name `fix/zha56`, and draft-PR scope. Obtain explicit authorization before each consequential action not already covered by the user's instruction.

- [ ] **Step 2: Publish the unchanged candidate when authorized**

After authorization, create the atomic implementation commit if it does not yet exist, push `fix/zha56`, and open or update a draft PR. Record the candidate SHA in both the PR and `codexctl-zha56`. Do not merge.

- [ ] **Step 3: Verify hosted macOS behavior**

Inspect the PR's macOS job and record the run/job IDs, runner image, Rust version, candidate SHA, exact focused regression result, storage migration result, and full serialized result. Rerun the unchanged macOS job once after the first pass.

Expected: both macOS attempts PASS the exact deterministic removal test, the writer-shaped rename/removal integration regression, and the full serialized job on the same SHA.

- [ ] **Step 4: Close only on complete acceptance**

Record both macOS attempts and all local evidence in `codexctl-zha56`. Close it only if every acceptance criterion is satisfied; otherwise leave it open with the exact remaining failure or authorization gate.

## Stress Test Results: Descriptor-open Recurrence Plan

### Resolved Decisions

- Rebase only from a clean tree, record the pre-rebase SHA, and abort on any unexpected source conflict or duplicate original patch.
- Treat the callback move as RED only when the focused command fails solely with the exact recurrent zero-link `InvalidStorage` diagnostic.
- Preserve the exact pathname-first, descriptor-before-`Stable` validation order and forbid broader helper refactoring.
- Cover unsafe identities with the existing executable matrix; preserve unexpected-I/O handling as an unchanged diff-review invariant.
- Retain every local gate and separate unrelated intermittent failures with exact evidence rather than skips or weakened checks.
- Stop after local Task 1 evidence until implementation commit, push, PR, hosted CI, and Bead closure are separately authorized.
- Roll back only the narrow source edit with an explicit patch; abort failed rebases and return failed hypotheses to systematic debugging.
- Reuse existing epic `codexctl-g73cj` and tasks `codexctl-0t6ma` and `codexctl-ncvn8` instead of creating duplicate tracker records.

### Changes Made

- Added explicit rebase abort and restore-point instructions.
- Made the deterministic RED evidence requirements exact.
- Added the unchanged unexpected-I/O review invariant.
- Bound execution to the existing Beads hierarchy.

### Deferred / Parking Lot

- Publication and hosted macOS acceptance remain blocked pending explicit authorization after the local candidate is reviewed.

### Confidence Assessment

- Overall: High
- Remaining concern: the local plan can prove classification and safety, but only repeated hosted macOS jobs can accept the original scheduling-sensitive failure mode.
