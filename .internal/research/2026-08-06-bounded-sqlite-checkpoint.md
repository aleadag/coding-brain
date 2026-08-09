# Research: Bounded SQLite WAL checkpoint cancellation

> **Date:** 2026-08-06
> **Bead:** codexctl-dzlb9.10
> **Status:** Complete

## Summary

SQLite progress handlers cannot be assumed to run inside `OP_Checkpoint`, and a one-shot interrupt can race with a later PRAGMA/VDBE operation. Use `sqlite3_wal_checkpoint_v2` directly on a dedicated worker-owned connection so an interrupt set after the worker's deadline check but before the checkpoint remains pending for the checkpoint implementation. Keep the absolute-deadline receive so the caller returns on time even if an individual filesystem operation is slow.

## Key Findings

### `sqlite3_interrupt` is the supported cross-thread cancellation primitive

> **Confidence:** high — independently verified against SQLite and rusqlite primary documentation.

SQLite documents that `sqlite3_interrupt` may be called from another thread and causes a pending database operation to abort at its earliest opportunity [S1]. Rusqlite 0.40.1 exposes this as a `Send + Sync` `InterruptHandle`; `interrupt()` targets the operation executing on that connection and produces SQLite's interrupt result [S2].

The connection must remain open until `interrupt()` returns [S1]. Therefore the checkpoint connection must be owned by the worker thread rather than borrowed from `BrainDb`, and the caller must retain only the interrupt handle and result receiver.

SQLite also documents that an interrupt issued when no operation is running does not affect operations started afterward [S1]. The bundled direct `sqlite3_wal_checkpoint_v2` implementation is important here: it enters the checkpoint without first clearing the connection interrupt flag, checks that flag in WAL frame copying, and clears it only as the API call exits. A test seam after the worker deadline recheck but before the direct API proves that a caller timeout and interrupt cannot be lost at that boundary.

### WAL checkpoint frame copying observes the interrupt flag

> **Confidence:** high — confirmed in both the bundled amalgamation and SQLite's published source evidence.

The bundled `libsqlite3-sys` 0.38.1 amalgamation passes the database handle into `walCheckpoint` and checks `db->u1.isInterrupted` inside the frame-copy loop before each frame read/write. SQLite's published WAL source evidence labels the database argument as the handle used to check interrupts [S3]. This makes `InterruptHandle` materially stronger than a VDBE progress handler for checkpoint internals.

An interrupt is cooperative: SQLite says an operation near completion may still finish [S1], and a filesystem sync already executing in the kernel is not a SQLite loop. The caller therefore also needs a deadline-bounded channel wait and must not join the worker on timeout. The worker keeps exclusive ownership and safely drops the connection whenever SQLite or the filesystem returns.

Rusqlite 0.40.1 exposes both the bundled FFI entry point and `Connection::handle()`. Raw-handle use is safe only under a narrow ownership invariant: the worker exclusively owns the `Connection`, the handle is used synchronously while that connection remains alive, no rusqlite operation runs concurrently on it, and the only cross-thread access is the documented `InterruptHandle`. `SQLITE_OK` maps to a zero busy flag, `SQLITE_BUSY` maps to the PRAGMA-compatible busy flag plus log/checkpoint output tuple, and other return codes use SQLite's extended error code before the existing checkpoint-operation mapping.

### Checkpoint lock waits need the same absolute deadline

> **Confidence:** high — independently verified against SQLite checkpoint documentation.

TRUNCATE inherits RESTART behavior, including busy-handler waits for readers, and also obtains the writer lock. PASSIVE does not invoke the busy handler but may leave the checkpoint incomplete [S4]. A dedicated TRUNCATE connection should receive a busy timeout derived from the same absolute deadline; timeout handling still calls `interrupt()` and returns `Busy`.

## Comparisons

| Mechanism | Covers checkpoint frame loop | Bounds lock wait | Bounds caller return |
|-----------|------------------------------|------------------|----------------------|
| VDBE progress handler | No reliable guarantee | No | No |
| Busy timeout only | No | Yes | No, not a slow frame copy/sync |
| Interrupt handle only | Cooperative frame-loop cancellation | Not by itself | No hard caller bound |
| Dedicated worker + direct checkpoint API + busy timeout + interrupt + timed receive | Yes, including a pre-API pending interrupt | Yes | Yes |

## Codebase Context

`src/brain/storage/maintenance.rs` uses the direct TRUNCATE checkpoint API on a dedicated worker-owned connection. The generic progress-handler deadline wrapper remains appropriate for retention, incremental vacuum, and integrity SQL, but is not used for checkpoint internals.

`src/brain/storage/mod.rs` already has the secure current-database opening and frozen-schema verification path needed to construct a separate checkpoint connection. `Cargo.toml` uses rusqlite 0.40.1 with the `hooks` feature, which exposes `Connection::get_interrupt_handle()`.

## Recommendations

1. Open a security-validated dedicated checkpoint connection before spawning the worker.
2. Move the connection into the worker and obtain its `InterruptHandle` first.
3. Set the connection busy timeout from the absolute remaining deadline.
4. Recheck the absolute deadline inside the worker, then call `sqlite3_wal_checkpoint_v2` directly so an interrupt after that check remains pending for the checkpoint.
5. Wait on a result channel only until that same deadline. On timeout, interrupt and return `Busy` without joining.
6. Prove the boundary with a deterministic seam after the deadline check but before the direct API: timeout and interrupt, release the seam, prove the worker exits, then prove the primary connection and committed WAL data remain valid.

## Open Questions

- No userspace SQLite mechanism can force a blocked kernel filesystem operation to finish. The worker-ownership design bounds the caller while allowing the isolated connection to finish or fail safely in the background.

## Refuted / Discarded Claims

- Discarded: a SQLite progress handler alone bounds `PRAGMA wal_checkpoint`. The checkpoint opcode enters SQLite's checkpoint implementation rather than periodically executing VDBE opcodes.
- Discarded: a one-shot interrupt before a later PRAGMA is sufficient. SQLite does not apply an idle interrupt to later operations, and the intervening VDBE boundary creates a cancellation race.
- Discarded: `sqlite3_interrupt` alone is a hard wall-clock guarantee. It is cooperative and SQLite explicitly allows nearly completed operations to finish.

## Sources

- [SQLite: Interrupt A Long-Running Query](https://sqlite.org/c3ref/interrupt.html) — Primary/Official — accessed 2026-08-06 — cross-thread safety, lifetime, and cooperative cancellation semantics [S1].
- [rusqlite 0.40.1 `InterruptHandle`](https://docs.rs/rusqlite/0.40.1/rusqlite/struct.InterruptHandle.html) — Primary/Official crate documentation — accessed 2026-08-06 — `Send`, `Sync`, and `SQLITE_INTERRUPT` behavior [S2].
- [SQLite WAL source evidence](https://www3.sqlite.org/matrix/ev/src/wal.html) — Primary/Official source evidence — accessed 2026-08-06 — checkpoint database handle is used for interrupt checks [S3].
- [SQLite: Checkpoint a database](https://sqlite.org/c3ref/wal_checkpoint_v2.html) — Primary/Official — accessed 2026-08-06 — checkpoint modes, busy-handler waits, and completion semantics [S4].
