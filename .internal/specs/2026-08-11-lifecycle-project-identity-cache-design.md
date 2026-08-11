# Lifecycle project-identity cache

- Date: 2026-08-11
- Task: `codexctl-9j39s`
- Research: `codexctl-9j39s.1`
- Brainstorming: `codexctl-9j39s.2`
- Status: Approved and stress-tested

## Summary

Coding Brain will stop running unbounded Git discovery for every applied
lifecycle event. It will persist validated project identities in a bounded
`project_identity_cache` table inside the reconstructible
`runtime-cache-v1.sqlite3` database, reuse them across hook processes, and
invalidate them on the next hook when any authority-relevant dependency
changes.

The cache remains a performance projection, never an identity authority.
`.coding-brain/project.toml` remains the explicit durable authority, a
canonical network remote remains the inferred stable authority, and the
canonical-root temporary identity remains the conservative fallback. Cache
misses use bounded Git subprocesses; cache or discovery failure never promotes
an identity, weakens SQLite durability, changes permission authority, or raises
Codex's two-second lifecycle-hook timeout.

## Problem

Codex installs SessionStart, UserPromptSubmit, PreToolUse, PostToolUse,
SubagentStart, and SubagentStop lifecycle handlers with a two-second timeout.
For every applied event, `observation_event` loads a project identity after the
lifecycle transaction commits and before the activity transaction commits.

`ProjectIdentity::load` currently runs `git rev-parse --show-toplevel` on every
call. When the project-root manifest is absent it also runs
`git remote get-url origin`. Both calls use synchronous `Command::output()`
without an internal deadline or output bound. A stalled child can therefore
outlive Codex's deadline after lifecycle state is already durable but before
the corresponding activity observation exists.

The existing 500 ms `StorageDeadline` is an absolute SQLite budget, not a
whole-process supervisor. It does not bound Git, input, parent discovery, or
an already-entered durable commit. The existing ignored latency smoke reports
only aggregate wall time and cannot attribute an overrun.

## Goals

- Remove Git subprocesses from valid cache-hit lifecycle invocations.
- Invalidate a cached identity on the first hook after an authority-relevant
  dependency changes.
- Bound all Git discovery children under one shared deadline and output budget.
- Distinguish hook stages with opt-in, privacy-safe timing evidence.
- Preserve lifecycle-before-activity ordering and `synchronous=FULL` commits.
- Preserve manifest, network-remote, and temporary identity semantics.
- Preserve hook role restrictions, authoritative storage integrity, and fail-closed
  PermissionRequest authority.
- Prove cache reuse across separate `cbrain` processes and cover all lifecycle
  event classes that share the executable.

## Non-goals

- Automatically creating, modifying, or removing
  `.coding-brain/project.toml`.
- Making cached state an identity authority or using stale identity after
  failed validation.
- Caching temporary identities, failed discovery, or negative repository
  lookups.
- Reimplementing general Git configuration, conditional-include, or URL-rewrite
  semantics in Rust.
- Adding a daemon, increasing provider hook timeouts, weakening fsync, dropping
  activity evidence, or changing permission decisions.
- Guaranteeing preemption of an individual blocking filesystem syscall or an
  SQLite commit that has already begun.

## Identity authority

The authority order remains unchanged:

1. A valid project-root `.coding-brain/project.toml` supplies the explicit
   stable UUID.
2. Otherwise, a credential-stripped canonical network remote supplies the
   deterministic stable UUID.
3. Otherwise, the canonical project root supplies a machine-local temporary
   identity.

The manifest remains optional but is not replaced by the cache. It is the only
explicit identity that naturally survives remote renames, offline/local
repositories, clones, and machines. No cache hit may turn inferred or
temporary evidence into manifest authority.

Every cached row carries a closed provenance value: `manifest` or
`network_remote`. Temporary identities are deliberately non-cacheable so a
directory that becomes a repository cannot remain negatively classified.

## Cache storage

The cache is a dedicated private SQLite database named
`runtime-cache-v1.sqlite3`. Its closed v1 schema initially contains only the
bounded `project_identity_cache` table. The generic filename permits a future
explicit schema decision to add other reconstructible runtime projections; no
other table is part of this task.

Each row is keyed by canonical project root and stores:

- the `ProjectId` kind and value;
- closed provenance;
- versioned, bounded evidence for closed dependency slots;
- a digest of the supported discovery environment;
- refresh order; and
- the row schema version.

The row does not store the raw remote URL, command, hook payload, prompt,
transcript, arbitrary dependency path, or arbitrary diagnostic text. The table
holds at most 256 worktree-root entries. A cold refresh upserts its row and
prunes the oldest refresh-order rows in one cache transaction. Hits do not
write last-use metadata, create a transaction, or add an fsync.

The cache is non-authoritative and independent from `brain.sqlite3` and
`review.sqlite3`. Cache absence, incompatibility, contention, corruption, or
write failure cannot block lifecycle or activity persistence. A cache commit
without a corresponding activity, or an activity commit without a cache
refresh, is harmless because every hit revalidates independent authority
evidence.

## Cache initialization and format rollback

Hooks may lazily create only the exact `runtime-cache-v1.sqlite3` schema when
the cache is absent. Creation uses trusted-path validation, a closed schema,
bounded SQLite locking, and a short cache-specific deadline. Concurrent losers
bypass the cache rather than wait. Hooks never migrate, repair, delete,
quarantine, or replace an existing cache.

An incompatible, unsafe, or corrupt cache produces a bounded bypass and closed
diagnostic. Non-hook startup may quarantine it and recreate the exact current
format. Future incompatible formats use new filenames such as
`runtime-cache-v2.sqlite3`; they do not rewrite v1. Older binaries therefore
ignore newer files and can reuse an untouched v1 cache after rollback. Pruning
obsolete cache versions is a separate non-hook policy and is not added here.

The authoritative Brain schema remains v1. Existing legacy migration, erasure,
review storage, frozen-schema verification, and immutable publication behavior
are unchanged.

## Dependency evidence and immediate invalidation

A cache entry is usable only when every closed dependency slot can be recomputed
from the current canonical cwd/root and supported environment, then re-opened
with matching bounded evidence. Cached rows never supply paths to open.
Evidence uses the existing platform-appropriate file identity conventions and
includes file type, stable identity where available, size, and high-resolution
modification metadata. Small authority files are also read through a bounded
descriptor and content-digested, preventing a same-size rewrite from evading
validation.

Manifest provenance records the root-relative manifest and repository/worktree
topology used to establish its project root. Network-remote provenance uses
only recomputed closed slots:

- repository and worktree topology, including `.git` indirection;
- repository/common and worktree Git configuration;
- standard system and global configuration candidates recomputed from the
  current `HOME` and `XDG_CONFIG_HOME`, recording both presence and absence;
  and
- a digest of `HOME`, `XDG_CONFIG_HOME`, `PATH`, and other non-path-changing
  discovery inputs without storing their raw values;
- the canonical location and bounded file identity/digest of the Git executable
  recomputed from the current `PATH`.

Any `include`, `includeIf`, non-file configuration origin, `GIT_DIR`,
`GIT_WORK_TREE`, `GIT_COMMON_DIR`, redirected system/global configuration,
counted configuration injection, or other path-changing override makes a
Git-derived result non-cacheable. Static URL rewrite configuration is cacheable
only when it comes from the closed configuration slots above.

Git executable selection is also closed. Cache validation resolves `git` from
the current `PATH` without executing or trusting a cache-supplied path, then
compares canonical location and evidence. Relative or empty PATH components,
non-regular executables, unresolvable symlinks, or ambiguous selection make a
network-derived result non-cacheable. Manifest-derived entries do not depend on
Git executable evidence after root/topology validation.

Cold discovery asks Git for origin-aware configuration provenance rather than
inventing conditional-include or `insteadOf` semantics. If Git reports any
dynamic, non-file, redirected, oversized, too-numerous, or otherwise
unrepresentable dependency, the result is valid for the current activity but
is marked non-cacheable. Correctness wins over cache coverage.

Lookup canonicalizes the hook cwd and selects the longest cached root that
contains it by path components, never string prefix. At most 256 candidates are
examined. Separate Git worktrees retain separate rows even when they derive the
same `ProjectId`. An ambiguous, non-canonical, moved, or changed-root match is a
miss.

Before accepting an ancestor hit, validation scans each bounded canonical path
component between root and cwd for a nested `.git` marker or
`.coding-brain/project.toml`. Either boundary rejects the outer hit and triggers
bounded discovery. Excessive component depth bypasses the cache. The scan does
not follow arbitrary symlinks because root and cwd have already been
canonicalized.

Validation is fail-closed for the cache: missing, replaced, changed, malformed,
oversized, unreadable, unrepresentable, or cache-directed evidence produces a
miss. A refresh timeout never permits the stale row to be used.

## Hook data flow

The ordered path is:

1. Capture the hook start instant at the beginning of `main`, before CLI
   parsing, and read/parse the bounded hook input.
2. Discover the live provider parent under the remaining subprocess budget.
3. Open or boundedly bypass the auxiliary cache, canonicalize cwd, and lookup
   the longest validated root.
4. On a miss, run bounded Git discovery and assemble cacheable evidence or a
   conservative temporary identity.
5. Open and verify the Brain database with a fresh existing 500 ms storage
   deadline.
6. Apply and durably commit the lifecycle projection.
7. Build PostToolUse correlation, when applicable.
8. Append activity through the unchanged `synchronous=FULL` activity
   transaction.
9. After authoritative activity success, best-effort upsert/prune a cacheable
   refresh in its independent cache transaction.
10. Emit the final opt-in timing record and return with empty stdout.

Moving project resolution before authoritative persistence prevents cache/Git
latency from consuming the Brain storage deadline or creating a new
lifecycle-only failure window. Lifecycle remains durably ordered before
activity. Cache refresh is deliberately outside both authoritative
transactions.

## Budgets and subprocess supervision

Lifecycle-hook execution establishes an internal 1,500 ms monotonic budget
from the earliest Rust entry point, leaving at least 500 ms of nominal
headroom below Codex's outer deadline. The budget is passed explicitly to
bounded child operations; it does not redefine authoritative SQLite commit
semantics.

The existing SQLite deadline remains one absolute 500 ms deadline covering
open and subsequent operations. Parent discovery receives at most 250 ms of
the remaining hook budget. All Git commands in one cold resolution share at
most 250 ms and a total 64 KiB output budget. No later command receives a fresh
full allowance.

Git uses the established process-group supervision pattern: nonblocking
bounded output, monotonic deadline, descendant termination, bounded cleanup,
and the process-wide reaper. Timeout, output overflow, spawn failure,
non-success status, or malformed output is a closed discovery failure.

The internal budget gates entry to work. It cannot retract a successful
SQLite commit or safely preempt a kernel filesystem call already in progress.
Installed-release measurements, rather than the budget constant alone, must
demonstrate normal headroom.

## Timing diagnostics

An opt-in `CBRAIN_HOOK_TIMING=1` diagnostic mode emits one bounded stderr line
as each closed stage completes:

- `cli_input`;
- `parent_discovery`;
- `project_cache`;
- `project_git`, when executed;
- `sqlite_open`;
- `lifecycle_commit`;
- `posttool_correlation`, when executed;
- `activity_commit`;
- `cache_refresh`, when executed; and
- `total`.

Each record contains only schema version, provider, closed event class, closed
stage/outcome code, and integer elapsed/remaining milliseconds. It never emits
paths, project IDs, remotes, commands, payloads, prompts, environment values,
session identifiers, or free-form errors. The number and encoded size of lines
are fixed and bounded. Per-stage emission preserves the last completed boundary
when an outer test supervisor kills a controlled timeout case.

Default production mode remains quiet except for existing bounded diagnostics.
New production failures use closed codes such as `project_git_timeout`,
`project_git_output_limit`, `project_cache_invalid`, and
`project_cache_write_failed`. Stdout remains empty; stderr emission is not
provider acceptance evidence.

## Failure behavior

- **Valid hit:** use the cached identity and perform no cache write.
- **Invalid hit:** discard it as authority, run one bounded refresh, and replace
  it only with complete representable evidence.
- **Git timeout/failure:** use the existing canonical-root temporary identity
  for this activity, emit a closed diagnostic, and do not cache the result.
- **Uncacheable valid Git result:** use the resolved stable identity for this
  activity, emit no error, and deliberately skip the cache upsert.
- **Cache read corruption/version mismatch:** bypass the cache, report a closed
  diagnostic, and do not repair or trust any partial row from a hook.
- **Cache initialization contention/failure:** the hook bypasses immediately;
  concurrent losers never wait for the winner.
- **Cache write failure/uncertain commit:** authoritative activity remains
  successful; the cache row is ignored until a later read validates it, and a
  miss safely repeats bounded discovery.
- **Uncertain activity commit:** preserve existing uncertain-commit
  classification and recover by authoritative Brain database reread; never
  retry an identity-qualified activity blindly.
- **Storage contention/unavailable/migration active:** respect the existing
  storage deadline and hook-native fallback behavior without schema
  maintenance or partial evidence.
- **Successful commit crossing a deadline:** retain the successful durable
  result as authoritative.

PermissionRequest inference, decisions, response delivery, and fail-closed
authority are out of this data path and remain unchanged.

## Security and privacy

The auxiliary cache database uses the existing trusted-ancestor, owner, mode,
no-follow, closed-schema, and integrity patterns. Serialized evidence is
untrusted on every read. Validation is bounded before allocation or filesystem
traversal, and a row cannot name a dependency path or unbounded number of
bytes. Root-relative and standard configuration locations are recomputed from
validated current inputs.

Remote credentials are stripped by the existing canonicalization before a
stable `ProjectId` is derived. Raw remotes and relevant environment values are
never cached or timed. Diagnostics use closed enums and pass through the
existing bounded redaction layer. Cache failure cannot grant permission,
confirm provider execution, or create delivery evidence.

## Testing

### Deterministic cache tests

- Unchanged manifest and Git-derived entries hit without invoking the resolver.
- Replaced, deleted, malformed, oversized, permission-denied, and content-
  changed dependencies miss on the next lookup.
- Repository/global/system config, worktree topology, and supported environment
  changes invalidate immediately; includes and path-changing overrides are
  non-cacheable.
- Temporary, failed, and unrepresentable results are never cached.
- A first lookup refreshes once; a second lookup through a newly opened cache
  connection invokes the resolver zero times.
- Table pruning retains at most 256 entries and cannot remove the row used by
  the transaction being committed.
- Nested cwd lookups use one root row, longest-component-ancestor matching, and
  keep separate worktree rows; newly created, removed, or replaced nested Git
  and manifest boundaries invalidate outer-root hits.
- Network-derived entries invalidate when PATH selection, a Git wrapper, or the
  resolved executable changes; immutable unchanged Nix-store Git remains a hit.

### Deterministic budget tests

An injected monotonic clock and controlled resolver outcomes advance named
stages without wall sleeps. Tests assert the shared remaining budget, one
deadline across all Git commands, timeout classification, skipped later work,
and fixed timing output. This virtual-budget suite is the CI regression gate
for renewed deadline overruns.

Real process-supervision tests use marker pipes and a child blocked on a
controlled descriptor. They prove timeout and output-limit termination,
descendant cleanup, and reaping without assuming a fixed scheduling delay.

### Process, storage, and lifecycle tests

- Separate hook processes share the auxiliary cache and invoke a counting Git
  wrapper only on the first cacheable resolution.
- Concurrent first-use hooks create one exact cache; losing creators bypass
  without waiting, and crash/corruption never changes Brain evidence.
- Cache contention, unavailable cache, malformed rows, cache commit uncertainty,
  authoritative pre-commit failure, post-commit uncertainty, and reopen
  recovery preserve exact lifecycle and activity evidence.
- Exact lazy v1 creation, incompatible/corrupt bypass, non-hook quarantine, and
  versioned-file rollback have deterministic coverage.
- Existing legacy migration, erasure, integrity, and rollback fixtures remain
  green.
- UserPromptSubmit, PreToolUse, and PostToolUse run through miss, hit,
  invalidation, and timeout cases; PostToolUse retains exact correlation.
- A binary integration test runs separate `cbrain` processes and proves only
  the first valid invocation reaches a counting Git wrapper.
- Stdout is always empty, and timing/diagnostic snapshots reject sensitive or
  free-form fields.

### Installed-release evidence

The release-profile/Nix candidate is exercised from an isolated HOME/XDG tree
with synthetic, non-sensitive payloads against typical and generated
production-sized SQLite state. It never copies live activity contents or
writes the user's live state. The evidence records cold-miss and warm-hit
p50/p95, stage distributions, database size/row counts, invalidation, cache
initialization concurrency, controlled Git timeout, and SQLite contention. It
covers UserPromptSubmit, PreToolUse, and PostToolUse.

Wall-clock measurements are reported evidence, not a fixed CI threshold.
Acceptance requires warm normal p95 below 100 ms and controlled failure return
below 1,500 ms on the reference Linux environment, leaving documented headroom
below Codex's two-second deadline. Hosted macOS receives the same functional
and virtual-budget gates; measured numbers are reported separately rather than
compared across machines.

No benchmark persists raw commands, prompts, hook payloads, secrets, real
session identifiers, raw remotes, or sensitive paths.

## Rollout and rollback

The authoritative Brain schema does not change. Release notes describe the new
reconstructible `runtime-cache-v1.sqlite3` file and its safe bypass behavior.
Candidate verification is separate from versioning, publishing, Home Manager
installation, and live production acceptance.

Rollback uses the previous binary and its exact cache-version filename. Future
cache formats use parallel versioned filenames, so they do not rewrite the v1
cache. Hooks never delete incompatible files. Non-hook quarantine and obsolete-
version cleanup remain explicit, recoverable operations.

## Acceptance criteria

- Valid cache-hit lifecycle hooks launch no Git child across separate
  processes.
- Authority-relevant file, topology, configuration, and environment changes
  invalidate on the next lookup; conditional includes, path-changing overrides,
  and other unrepresentable evidence are not cached.
- Canonical project-root rows serve nested cwd values through bounded
  longest-component-ancestor matching, reject intervening Git/manifest
  boundaries, and remain separate per worktree.
- Network-derived rows validate the currently selected Git executable and PATH
  provenance without trusting a cache-supplied executable path.
- Git discovery has one shared 250 ms deadline, a 64 KiB aggregate output cap,
  descendant cleanup, and closed diagnostics.
- Temporary and failed identities are never cached or promoted.
- `runtime-cache-v1.sqlite3` is non-authoritative, independently versioned, and
  safely bypassed when absent, contended, corrupt, incompatible, or unsafe.
- Hook lazy creation is bounded and exact; hooks never migrate, repair, replace,
  quarantine, or delete an existing cache.
- Project resolution completes before the unchanged lifecycle and activity
  transactions; cache refresh is best-effort only after activity success.
- Brain schema, migration, frozen-schema verification, and hook role
  restrictions are unchanged.
- Opt-in timing distinguishes every specified stage without sensitive fields,
  while default stdout remains empty.
- Virtual-clock tests detect budget regressions without fixed sleeps.
- UserPromptSubmit, PreToolUse, and PostToolUse preserve lifecycle and activity
  coverage, including PostToolUse correlation.
- Cache/Brain contention, storage-unavailable, Git-timeout, malformed-cache,
  and uncertain-commit paths are bounded and produce no false authoritative
  success.
- Installed release-profile evidence shows warm normal p95 below 100 ms and
  controlled failures below 1,500 ms on the reference Linux environment with
  production-sized state.
- Codex's two-second lifecycle timeout, SQLite `synchronous=FULL`, migration and
  integrity checks, provider acceptance semantics, and fail-closed permission
  authority are unchanged.

## Stress Test Results: Lifecycle project-identity cache

### Resolved Decisions

- Isolate the rebuildable cache in `runtime-cache-v1.sqlite3`; do not change the
  authoritative Brain schema or activity transaction.
- Cache only closed, simple Git provenance. Conditional includes, path-changing
  overrides, and unrepresentable sources remain bounded but non-cacheable.
- Resolve project identity before opening the Brain storage deadline and before
  lifecycle/activity persistence.
- Permit bounded exact lazy cache creation by hooks, but reserve migration,
  repair, quarantine, replacement, and deletion for non-hook operation.
- Key rows by canonical worktree root and use bounded longest-component-
  ancestor matching for nested cwd values.
- Recompute dependency locations from closed slots; cache rows never direct
  arbitrary file reads.
- Keep stage timing, bounded Git, and non-Git diagnosis; a fast cache hit alone
  cannot close the timeout bug.
- Use parallel versioned runtime-cache filenames for rollback compatibility.
- Require deterministic unit, real process-integration, and isolated installed-
  candidate evidence.
- Generate production-sized synthetic state; never benchmark by polluting live
  audit state or raw-copying sensitive WAL contents.
- Reject cached ancestor hits across newly introduced nested Git/manifest
  boundaries.
- Invalidate network-derived rows when PATH or the resolved Git executable
  changes.

### Changes Made

- Removed the proposed Brain v2 migration and retained-backup workflow.
- Decoupled cache commits from authoritative lifecycle/activity durability.
- Tightened cache eligibility and dependency-path validation.
- Moved project resolution ahead of authoritative writes and their 500 ms
  storage deadline.
- Added lazy initialization concurrency, nested-root, corrupt-cache, rollback,
  and isolated-candidate coverage.
- Added intermediate repository-boundary scans and Git executable provenance.

### Deferred / Parking Lot

- Future runtime-cache tables, cache format v2, corrupt-file quarantine policy,
  and obsolete-version cleanup require separate explicit designs.

### Confidence Assessment

- Overall: High after one reflexion pass and 12 resolved branches.
- Areas of concern: proving the chosen subprocess budgets on macOS as well as
  Linux; this remains an explicit installed-candidate evidence gate rather than
  an assumed cross-platform constant.
