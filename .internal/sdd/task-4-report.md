# Task 4 Report: Decision Payloads and Privacy Erasure

## Result

Task 4 adds inactive SQLite storage for complete decision records and a resumable privacy-erasure path. Production still reads learning data from JSONL; Task 8 remains the only runtime activation point.

The v1 schema now distinguishes permission identities from observations. Permission rows require the complete authority tuple used by `permission_commits`; observation rows cannot carry session, turn, tool-use, action, or source authority. A payload has the same closed kind as its identity, references exactly one immutable activity cursor, and preserves the complete supported `DecisionRecord` in a bounded blob.

## RED

The first focused test was:

```text
nix develop path:. --command cargo test --test sqlite_storage decision_kinds_reject_incomplete_or_fabricated_authority -- --exact
```

It failed to compile because `DecisionKind`, `DecisionIdentity`, `DecisionPayload`, `LearningErasePaths`, and the Task 4 `BrainDb` APIs did not exist. This established the missing schema and adapter contract before implementation.

## Implementation evidence

- Permission and observation records round-trip with all current `DecisionRecord` fields, including nullable metadata, context, outcome, canonical state, timings, and opaque supported strings.
- The exact 1,048,576-byte serialized record boundary round-trips. A 1,048,577-byte record is rejected, while typed projections remain capped at 4,096 bytes.
- Learning pages are ordered and continued by immutable activity cursor. The unique payload-to-cursor index prevents a cursor-only continuation from skipping another row.
- Observation payloads become eligible after their atomic identity/payload insert. Permission payloads require an exact authoritative `permission_commits` join; an uncommitted proposal is excluded.
- Exact and paged reads route their source cursor through the complete Task 3 activity decoder. They reject unsupported schemas, non-normalized or inconsistent payloads, every typed-column disagreement, non-decision activity, decision-ID mismatch, and permission authority mismatch.
- Consumer tests feed SQLite page records through the pure baseline, briefing, retrieval, metrics, insights, and distillation seams without switching production storage selection.
- Erasure deletes payloads, legacy decision/canonical/preference files, every published preference generation, the watermark, and the trigger while preserving activity, decision identity, and permission commit audit rows.
- Payload reads and writes hold a shared gate derived from the database state root through materialization or commit. `LearningReadSession` retains it across pages and publication; erasure/resume uses the same gate exclusively, so it cannot begin while a supported reader or writer is in flight. This is erasure-boundary stability, not a frozen SQLite snapshot: concurrently committed decisions may appear in later cursor pages.
- Erasure acquires sorted legacy decision locks before the derived erasure and distill locks. It retains each validated legacy and brain directory descriptor for the complete generation and deletes only through those descriptors. A renamed or replaced root fails the final device/inode correspondence check without touching its replacement; a supplied legacy root that was initially absent must remain absent. Either race leaves the durable generation incomplete.
- Descriptor-relative directory enumeration uses the portable `errno` crate around `readdir`, avoiding target-specific errno symbols and deprecated fixed-buffer enumeration APIs.
- Each fresh erasure increments the durable generation. An interrupted generation remains unavailable to learning reads, resumes with the same generation, and becomes complete only after verified WAL truncation and directory sync.
- A raw SQLite reader that bypasses the supported shared gate can pin the WAL and make truncation return busy. The generation remains in progress and learning stays unavailable until that reader releases its snapshot; reopening and resuming completes the same generation.
- Process tests kill erasure after the in-progress marker, database deletion, external deletion, generation deletion, before and after WAL truncation, and before and after final completion.
- Crash reopen accepts only validated SQLite `-wal`, `-shm`, and `-journal` sidecars. Creation still rejects all pre-existing sidecars, and reopen rejects unknown, linked, wrongly owned, or broadly accessible files.

## Verification

All commands ran from the Task 4 worktree.

```text
nix develop path:. --command cargo test --test sqlite_storage -- --test-threads=1
76 passed; 0 failed

nix develop path:. --command cargo test --test distill_process -- --test-threads=1
4 passed; 0 failed; 1 release-only test ignored

nix develop path:. --command cargo test brain::tests::sqlite_learning_page_records_drive_every_pure_consumer_seam -- --exact
1 passed; 0 failed

nix develop path:. --command cargo test --workspace --all-targets -- --test-threads=1
exit 0; every workspace target passed

nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
exit 0

nix develop path:. --command cargo fmt --all --check
exit 0

nix develop path:. --command cargo tree -p coding-brain-core --edges normal
exit 0; the coding-brain-core normal dependency tree contains no rusqlite package
```

Nix printed repeated `/nix/store/.links/... has maximum number of links` warnings during several commands. The commands above still returned the stated exit statuses.

## Scope boundary

This task does not migrate JSONL, activate SQLite readers or writers, export downgrade data, change permission transaction ownership, or implement the later review/view-state cutovers. It does not claim erasure of external backups, filesystem snapshots, physical media, or learning data written by an external process after erasure successfully completes.
