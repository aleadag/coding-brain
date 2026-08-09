# Architecture

Coding Brain is a three-crate Rust workspace. Dependencies flow downward:

```text
coding-brain -> coding-brain-tui -> coding-brain-core
```

The root binary owns provider integration, model evaluation, and persistence. `coding-brain-tui` renders Live, Review, Scorecard, and Diagnostics through runtime traits. `coding-brain-core` contains provider-neutral types, lifecycle projections, transcript discovery, and terminal contracts; it does not open Coding Brain databases.

## Storage authority

`$XDG_STATE_HOME/coding-brain/db/brain.sqlite3` is the sole live authority for decisions, lifecycle state, permission attempts and commits, activity, delivery and outcome evidence, corrections, and stable learning cursors. A model-derived response becomes eligible only after one short SQLite transaction commits the matching proposal, terminal activity, and exact allow or deny authority. Delivery is recorded separately, so a committed decision is not proof that the provider received it or ran the tool.

Operational review state lives in `$XDG_STATE_HOME/coding-brain/db/review.sqlite3`. The databases are never attached and have no cross-database transaction. Review failure can disable review refreshes and mutations while coherent permission and audit authority remains unchanged; reset replaces only the review database.

`$XDG_STATE_HOME/coding-brain/session-links.jsonl` remains a separate bounded native-session to live-process identity log. It supports navigation and correlation, carries no permission authority, and does not need atomicity with a Brain commit.

## Startup and migration

Ordinary non-hook `cbrain` startup initializes or migrates storage automatically before running the requested workflow. Migration validates and streams the complete supported legacy set into same-directory staging databases, revalidates source fingerprints under fixed-order legacy locks, atomically publishes the databases, and freezes migrated legacy sources read-only. A later mutation of a frozen source is a split-brain error and is never auto-merged. Review migration failure is isolated from publication of the Brain database.

Lifecycle, permission, and recovery hooks return before this migration path. When a hook sees legacy-only, migrating, busy, unsupported, corrupt, or unsafe authority storage, it performs no model inference and promptly leaves the request to the provider's native handling. Hooks never initialize, migrate, vacuum, perform retention, or run integrity checks.

The explicit `cbrain storage` commands do not initiate migration. Run an ordinary non-hook command such as `cbrain doctor` first when migration is required.

## Durability and operating limits

Both databases use bundled SQLite in an owner-only directory on a supported local filesystem. Coding Brain rejects network and unrecognized filesystem types. The Brain database uses WAL mode, `synchronous=FULL`, foreign keys, defensive restrictions, secure deletion, bounded queries, and a shared monotonic hook deadline.

Automatic checkpointing is configured at 1,000 pages. Doctor reports a WAL advisory at 16 MiB and a failure at 64 MiB; the hard limit suspends model inference but does not weaken deterministic safety denies. Routine Doctor checks validate schema, migration state, and WAL health. They deliberately report integrity as `not_checked`: the bounded deep integrity API is non-hook-only and is not currently exposed as a CLI command.

## Export, rollback, and erasure

`cbrain storage export-audit <directory>` creates a versioned, bounded, explicitly non-executable audit archive. It is for inspection and archival; importing it cannot recreate live permission authority.

`cbrain storage export-legacy <directory>` creates the exact frozen v0.59.1 compatibility layout and verifies it with frozen legacy readers. This is the supported downgrade input. Coding Brain never dual-writes legacy files and never swaps an export into live state.

Learning erasure deletes erasable SQLite payloads, published preference generations, WAL/free-page residue, and preserved legacy learning snapshots while retaining the immutable relationships needed to interpret permission audit evidence. This erasure operation is not currently exposed by the `storage` CLI; `cbrain init --purge` is a different, irreversible removal of the previewed global state and configuration targets.
