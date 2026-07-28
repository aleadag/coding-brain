# Centralize durable atomic state replacement

> **Date:** 2026-07-28
> **Issue:** codexctl-va45
> **Status:** Approved and stress-tested

## Context

Lifecycle snapshots and activity-log compaction already replace state through a
temporary file and an atomic rename. Their durability guarantees are incomplete:
lifecycle snapshots flush but do not sync the temporary file, and neither path
syncs the parent directory after replacement. Activity compaction syncs only file
data before replacement.

Other stores repeat related locking, permission, tail-repair, and replacement
mechanics, but migrating every store together would broaden this change beyond
the two confirmed security-sensitive paths. The shared replacement primitive
must therefore be reusable without forcing unrelated callers into this
migration.

## Decision

Add a public `durable_file` module to `coding-brain-core` with one streaming
replacement helper:

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

The destination parent hierarchy must already exist and be durably established.
Directory creation, store locking, serialization policy, retention policy, and
recovery of unrelated abandoned files remain caller responsibilities.

The helper performs this sequence:

1. validate that the destination has a parent and that the supplied temp prefix
   is exactly one non-empty normal path component;
2. create a named temporary file in that parent using the validated prefix;
3. set private `0600` permissions on Unix;
4. invoke the caller's writer;
5. reassert private `0600` permissions on Unix;
6. flush and `sync_all` the temporary file;
7. atomically persist the temporary file over the destination;
8. sync the parent directory on platforms where directory syncing is
   supported.

The closure API lets activity compaction stream retained rows directly to disk
instead of constructing a complete replacement in memory. The generic callback
error preserves caller-specific failures such as
`ActivityStoreError::Serialization`; filesystem failures introduced by the
helper are converted through `From<io::Error>`.

## Failure behavior

If validation, temporary-file creation, permission enforcement, writing,
flushing, or file syncing fails before replacement, the previous destination
remains in place. Dropping the named temporary file removes it.

If atomic persistence fails, the destination remains subject to the underlying
filesystem's rename semantics and the helper returns the persistence error.

If parent-directory syncing fails after the rename, the helper returns an error
even though the new destination may already be visible. This reports the real
state: replacement occurred, but its survival across a crash is not proven.
Callers must not attempt to roll back or silently report success. A later
operation reloads the current store state under its existing lock before
attempting another replacement.

On non-Unix platforms, private-mode operations remain no-ops, matching current
repository behavior. Parent-directory syncing follows the existing portable
repository pattern: sync on Unix and use a no-op fallback where opening a
directory as a file is unsupported. The non-Unix guarantee is therefore
atomic replacement with synced file contents and best-effort directory-entry
crash durability; this task does not add platform-specific directory handles.

## Lifecycle migration

Replace `LifecycleStore::persist`'s local temporary-file sequence with
`durable_replace`. The writer closure writes the already bounded and serialized
snapshot bytes.

Keep unchanged:

- the lifecycle lock and timeout;
- snapshot validation and size limits;
- `lifecycle.tmp-` naming;
- hooks-directory creation and `0700` enforcement;
- abandoned lifecycle-temp cleanup;
- corrupt-snapshot quarantine and retention;
- lifecycle error variants and their fail-closed behavior.

## Activity migration

Replace only the temporary-file replacement inside activity compaction with
`durable_replace`. The writer closure streams the existing diagnostic rows and
retained activity events in their current order.

Keep unchanged:

- activity locking and the production lock timeout;
- append, reservation, and tail-repair paths;
- compaction thresholds and retention calculations;
- row schemas, ordering, diagnostic reconstruction, and size limits;
- `activity.tmp-` naming;
- activity error variants.

Activity does not add abandoned-temp directory scanning in this task. Generic
prefix cleanup would be unsafe when independent stores share a directory and
prefix; lifecycle retains its existing cleanup under its own store lock.

## Security properties

- Replacement files are private (`0600`) on Unix before any state is written
  and the mode is reasserted after the trusted writer callback.
- Caller-owned state directories remain private (`0700`) under their existing
  setup paths.
- Readers can observe either the previous complete file or the replacement
  complete file, never a partially written destination.
- On Unix, given an already-established parent hierarchy, a successful return
  means the replacement file and its destination-directory entry have both
  completed the available durability barriers.
- On non-Unix platforms, a successful return guarantees synced file contents
  and atomic replacement, while directory-entry crash durability remains
  best-effort.
- No lock, validation, retention, or fail-closed behavior is weakened.
- A public caller cannot use the temp prefix to escape the destination parent.

## Regression tests

Add focused `coding-brain-core` helper tests proving:

1. a successful replacement returns the new complete contents;
2. a writer failure preserves the old destination and leaves no matching
   temporary file;
3. an injected file-sync failure before rename preserves the old destination;
4. an injected directory-sync failure after rename returns an error while the
   complete new destination remains visible;
5. concurrent raw readers observe only complete old or new payloads across
   repeated replacements;
6. replacement files retain `0600` permissions on Unix.
7. empty, absolute, or multi-component temp prefixes fail with `InvalidInput`
   before any file is created.

Use a private helper seam for the two sync operations so these phase failures
are deterministic. Production callers still use only `durable_replace`, which
supplies the real file and directory sync operations; do not expose a
filesystem transaction or fault-injection API.

Keep the existing lifecycle regression that repeatedly reads and parses the
snapshot during replacement. Test activity compaction deterministically by
checking retained rows and diagnostics, complete parseable JSONL, private
permissions, and absence of temporary-file leaks after success. The helper's
focused writer-failure test owns failure cleanup coverage; do not add production
injection solely for the activity test. Do not add a raw-reader activity race
that can observe an ordinary in-place append rather than compaction.

The helper failure-preservation and injected sync-failure tests must fail before
production helper code is added. Caller migration tests validate integration
and preserved store behavior; they are not required to infer unobservable
`fsync` calls or fail against the previous local replacement sequences.

Avoid timing-only assertions. In particular, do not repeat the macOS ARM64
lock-test flake caused by asserting a tight wall-clock margin after a worker
could be descheduled. Coordinate writer start and reader lifetime explicitly
with barriers or channels, assert the received result, and use only a generous
test-local deadline as a hang guard. Retain the production activity lock
timeout unchanged.

## Scope

This change adds the reusable core helper and migrates lifecycle snapshot
replacement plus activity-log compaction.

Do not migrate decisions, session links, project identity, distillation, append
paths, corrupt-file quarantine, or other rename operations in this task. Those
callers may adopt the public helper in later independently verified changes.
Do not introduce a filesystem transaction abstraction, configurable durability
levels, background syncing, retries, or new dependencies.
Crash-durable creation of new parent-directory hierarchies is a separate
concern and is not added to this replacement primitive.

Measure the lifecycle durability barriers' latency and lock-contention impact
separately in `codexctl-6wbh`; do not weaken this design without observed
evidence and a new approved design.

## Verification

Run:

1. focused `coding-brain-core` durable-file and lifecycle tests;
2. focused activity-store tests;
3. `cargo fmt --check`;
4. `cargo test`;
5. `cargo clippy --all-targets -- -D warnings`;
6. `cargo build`.

## Stress Test Results: Durable atomic state replacement

### Resolved Decisions

- **Error taxonomy:** Use a generic callback error implementing
  `From<io::Error>` so activity serialization errors remain distinct from
  filesystem failures.
- **Durability boundary:** Require an already-established parent hierarchy;
  recursively durable directory creation is a separate concern.
- **Post-rename failure:** Return parent-sync failures without rollback or a
  new public commit-state type. Later operations reload current state under the
  existing lock.
- **Portability:** Guarantee file and directory-entry durability on Unix.
  Non-Unix replacement remains atomic with synced file contents and
  best-effort directory-entry crash durability.
- **Abandoned temporary files:** Keep cleanup store-owned. Lifecycle retains
  its locked cleanup; the generic helper and activity migration do not delete
  by shared prefix.
- **Latency:** Accept synchronous durability barriers under the existing lock
  and preserve the production 100 ms timeout. Measure operational impact in
  `codexctl-6wbh`.
- **Testing:** Keep atomic-reader coverage in the helper and lifecycle paths;
  test activity compaction deterministically without racing ordinary appends
  or using tight wall-clock assertions.
- **Permissions:** Enforce `0600` before and after the trusted writer callback.
- **Public boundary:** Publish one narrow core helper so both core and binary
  callers can use it; add no transaction abstraction, traits, or configuration.
- **Crash-phase observability:** Use a private two-sync test seam to prove
  pre-rename and post-rename failure semantics without expanding the public API.

### Changes Made

- Generalized the callback error type to preserve caller error categories.
- Narrowed durability claims to established parent hierarchies and documented
  the non-Unix limitation.
- Clarified post-rename retry behavior and store-owned temp cleanup.
- Replaced the ambiguous activity raw-reader race with deterministic
  compaction integration coverage.
- Added the prior macOS ARM64 deflake constraint: channels or barriers plus a
  generous test-only hang deadline, never a tight elapsed-time assertion.
- Reasserted private file permissions after the writer callback.
- Added deterministic injected file-sync and directory-sync failure coverage
  through a private helper seam.
- Filed `codexctl-6wbh` for latency and contention measurement.

### Deferred / Parking Lot

- Crash-durable recursive parent-directory creation.
- Platform-specific non-Unix directory-entry syncing.
- Activity abandoned-temp cleanup if accumulation is observed.
- Migration of decisions, session links, project identity, and distillation.
- Durability-preserving latency optimization, pending `codexctl-6wbh`
  measurements.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Synchronous filesystem barriers can increase lifecycle
  latency on slow filesystems; the follow-up measurement must quantify this
  before any timeout or durability policy changes.
