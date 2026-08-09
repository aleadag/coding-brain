use rusqlite::params;

use std::collections::{BTreeMap, BTreeSet};

use super::{
    ActivityCursor, BRAIN_DATABASE_NAME, BRAIN_SCHEMA_VERSION, BrainDb, OpenRole, StorageDeadline,
    StorageError, StorageFaultCategory, StorageOperation, security::SecureDatabaseDirectory,
};

pub const WAL_AUTOCHECKPOINT_PAGES: i64 = 1_000;
pub const WAL_WARNING_BYTES: u64 = 16 * 1024 * 1024;
pub const WAL_HARD_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const RETENTION_BATCH_ROWS: i64 = 128;
const RETENTION_SCAN_ROWS: i64 = 512;
const INCREMENTAL_VACUUM_PAGES: i64 = 128;
const PROGRESS_HANDLER_OPS: i32 = 100;

#[derive(Debug)]
struct RetentionRow {
    cursor: i64,
    activity_id: String,
    event_state: String,
    recorded_at_ms: i64,
    has_permission: bool,
    has_correction: bool,
    has_outcome: bool,
}

#[derive(Debug)]
struct RetentionGroup {
    activity_id: String,
    cursors: Vec<i64>,
    first_cursor: i64,
    last_cursor: i64,
    group_last_cursor: i64,
    last_recorded_at_ms: i64,
    has_permission: bool,
    has_correction: bool,
    has_outcome: bool,
    has_interrupted: bool,
    has_terminal: bool,
    outside_older: bool,
    outside_newer: bool,
    has_decision_payload: bool,
    has_historical_authority: bool,
}

pub(super) fn sqlite_fault_category(error: &rusqlite::Error) -> Option<StorageFaultCategory> {
    match error.sqlite_error_code() {
        Some(
            rusqlite::ErrorCode::DatabaseBusy
            | rusqlite::ErrorCode::DatabaseLocked
            | rusqlite::ErrorCode::OperationInterrupted,
        ) => Some(StorageFaultCategory::Busy),
        Some(rusqlite::ErrorCode::DiskFull) => Some(StorageFaultCategory::Full),
        Some(rusqlite::ErrorCode::SystemIoFailure | rusqlite::ErrorCode::CannotOpen) => {
            Some(StorageFaultCategory::Io)
        }
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            Some(StorageFaultCategory::Corrupt)
        }
        _ => None,
    }
}

struct ProgressHandlerReset<'connection> {
    connection: &'connection rusqlite::Connection,
    active: bool,
}

impl ProgressHandlerReset<'_> {
    fn clear(&mut self) -> rusqlite::Result<()> {
        let result = self.connection.progress_handler(0, None::<fn() -> bool>);
        if result.is_ok() {
            self.active = false;
        }
        result
    }
}

impl Drop for ProgressHandlerReset<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
        }
    }
}

fn run_sql_with_progress<T>(
    connection: &rusqlite::Connection,
    operation: StorageOperation,
    interrupt: impl FnMut() -> bool + Send + 'static,
    sql: impl FnOnce(&rusqlite::Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    connection
        .progress_handler(PROGRESS_HANDLER_OPS, Some(interrupt))
        .map_err(|error| map_sqlite_error(operation, false, error))?;
    let mut reset = ProgressHandlerReset {
        connection,
        active: true,
    };
    let result = sql(connection).map_err(|error| map_storage_error(operation, false, error));
    let cleanup = reset
        .clear()
        .map_err(|error| map_sqlite_error(operation, false, error));
    match cleanup {
        Ok(()) => result,
        Err(error) => Err(error),
    }
}

fn run_sql_with_deadline<T>(
    connection: &rusqlite::Connection,
    deadline: StorageDeadline,
    operation: StorageOperation,
    sql: impl FnOnce(&rusqlite::Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    deadline
        .apply(connection)
        .map_err(|error| map_storage_error(operation, false, error))?;
    let result = run_sql_with_progress(
        connection,
        operation,
        move || deadline.0 <= std::time::Instant::now(),
        sql,
    )?;
    deadline.ensure_remaining()?;
    Ok(result)
}

#[cfg(test)]
thread_local! {
    static SQLITE_FAULT: std::cell::RefCell<Option<(&'static str, i32)>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg(test)]
pub(super) fn with_sqlite_fault<T>(
    point: &'static str,
    extended_code: i32,
    operation: impl FnOnce() -> T,
) -> T {
    struct Reset(Option<(&'static str, i32)>);
    impl Drop for Reset {
        fn drop(&mut self) {
            SQLITE_FAULT.with(|fault| *fault.borrow_mut() = self.0.take());
        }
    }

    let previous = SQLITE_FAULT.with(|fault| fault.borrow_mut().replace((point, extended_code)));
    let _reset = Reset(previous);
    operation()
}

#[cfg(test)]
pub(super) fn sqlite_fault(point: &str) -> rusqlite::Result<()> {
    SQLITE_FAULT.with(|fault| {
        if let Some((injected_point, extended_code)) = *fault.borrow()
            && injected_point == point
        {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(extended_code),
                None,
            ));
        }
        Ok(())
    })
}

#[cfg(test)]
fn captured_sqlite_fault(point: &str) -> Option<i32> {
    SQLITE_FAULT.with(|fault| {
        fault
            .borrow()
            .filter(|(injected_point, _)| *injected_point == point)
            .map(|(_, extended_code)| extended_code)
    })
}

#[cfg(not(test))]
pub(super) fn sqlite_fault(_point: &str) -> rusqlite::Result<()> {
    Ok(())
}

#[cfg(not(test))]
fn captured_sqlite_fault(_point: &str) -> Option<i32> {
    None
}

#[cfg(test)]
fn run_sql_with_progress_for_test<T>(
    connection: &rusqlite::Connection,
    operation: StorageOperation,
    interrupt: impl FnMut() -> bool + Send + 'static,
    sql: impl FnOnce(&rusqlite::Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    run_sql_with_progress(connection, operation, interrupt, sql)
}

pub(super) fn map_sqlite_error(
    operation: StorageOperation,
    commit_uncertain: bool,
    error: rusqlite::Error,
) -> StorageError {
    match sqlite_fault_category(&error) {
        Some(StorageFaultCategory::Busy) => StorageError::Busy,
        Some(category) if commit_uncertain => StorageError::CommitUncertain {
            operation,
            category,
        },
        Some(category) => StorageError::StorageFault {
            operation,
            category,
        },
        None => StorageError::Sqlite(error),
    }
}

pub(super) fn map_storage_error(
    operation: StorageOperation,
    commit_uncertain: bool,
    error: StorageError,
) -> StorageError {
    match error {
        StorageError::StorageFault { category, .. } if commit_uncertain => {
            StorageError::CommitUncertain {
                operation,
                category,
            }
        }
        StorageError::StorageFault { category, .. } => StorageError::StorageFault {
            operation,
            category,
        },
        StorageError::Sqlite(error) => map_sqlite_error(operation, commit_uncertain, error),
        other => other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalHealth {
    Normal,
    Warning,
    HardLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntegrityHealth {
    NotChecked,
    Ok,
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationHealth {
    Complete,
    InProgress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageHealth {
    pub database_path: &'static str,
    pub schema_version: i32,
    pub sqlite_version: &'static str,
    pub migration: MigrationHealth,
    pub wal_bytes: u64,
    pub wal: WalHealth,
    pub integrity: IntegrityHealth,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceOutcome {
    pub wal_bytes_before: u64,
    pub wal_bytes_after: u64,
    pub checkpointed_frames: u64,
    pub deleted_activity_rows: usize,
}

fn retain_activity_window(
    transaction: &rusqlite::Transaction<'_>,
    boundary: i64,
    stale_cutoff: i64,
    deadline: StorageDeadline,
) -> Result<usize, StorageError> {
    let (stored_boundary, stored_scan_before, stored_recent_remaining, stored_overlap, stored_keep): (
        Option<i64>,
        Option<i64>,
        i64,
        Option<String>,
        i64,
    ) = transaction.query_row(
        "SELECT maintenance_retention_boundary, maintenance_scan_before,
                maintenance_recent_remaining, maintenance_overlap_activity_id,
                maintenance_overlap_keep
         FROM schema_meta WHERE singleton = 1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    let continuing = stored_boundary == Some(boundary) && stored_scan_before.is_some();
    let scan_before = if continuing {
        stored_scan_before.unwrap_or(boundary)
    } else {
        boundary
    };
    let recent_remaining = if continuing {
        stored_recent_remaining
    } else {
        32
    };
    let overlap = continuing.then_some(stored_overlap).flatten();
    let overlap_keep = continuing && stored_keep != 0;

    let rows = {
        let mut statement = transaction.prepare(
            "SELECT source_cursor, activity_id, event_state, recorded_at_ms,
                    permission_attempt_id IS NOT NULL, correction IS NOT NULL,
                    outcome IS NOT NULL
             FROM activity_events INDEXED BY activity_events_cursor
             WHERE source_cursor < ?1
             ORDER BY source_cursor DESC
             LIMIT ?2",
        )?;
        let mapped = statement.query_map(params![scan_before, RETENTION_SCAN_ROWS], |row| {
            Ok(RetentionRow {
                cursor: row.get(0)?,
                activity_id: row.get(1)?,
                event_state: row.get(2)?,
                recorded_at_ms: row.get(3)?,
                has_permission: row.get(4)?,
                has_correction: row.get(5)?,
                has_outcome: row.get(6)?,
            })
        })?;
        mapped.collect::<Result<Vec<_>, _>>()?
    };

    if rows.is_empty() {
        transaction.execute(
            "UPDATE schema_meta
             SET maintenance_retention_boundary = ?1,
                 maintenance_scan_before = ?1,
                 maintenance_recent_remaining = 32,
                 maintenance_overlap_activity_id = NULL,
                 maintenance_overlap_keep = 0
             WHERE singleton = 1",
            [boundary],
        )?;
        return Ok(0);
    }

    let candidate_min = rows
        .last()
        .map(|row| row.cursor)
        .ok_or(StorageError::InvalidStorage(
            "maintenance candidate window is empty",
        ))?;
    let mut groups = BTreeMap::<String, RetentionGroup>::new();
    for row in rows {
        let is_interrupted = row.event_state == "interrupted";
        let is_terminal = !matches!(
            row.event_state.as_str(),
            "observed" | "evaluating" | "incomplete"
        );
        let group = groups
            .entry(row.activity_id.clone())
            .or_insert_with(|| RetentionGroup {
                activity_id: row.activity_id,
                cursors: Vec::new(),
                first_cursor: row.cursor,
                last_cursor: row.cursor,
                group_last_cursor: row.cursor,
                last_recorded_at_ms: row.recorded_at_ms,
                has_permission: false,
                has_correction: false,
                has_outcome: false,
                has_interrupted: false,
                has_terminal: false,
                outside_older: false,
                outside_newer: false,
                has_decision_payload: false,
                has_historical_authority: false,
            });
        group.cursors.push(row.cursor);
        group.first_cursor = group.first_cursor.min(row.cursor);
        group.last_cursor = group.last_cursor.max(row.cursor);
        group.group_last_cursor = group.group_last_cursor.max(row.cursor);
        group.last_recorded_at_ms = group.last_recorded_at_ms.max(row.recorded_at_ms);
        group.has_permission |= row.has_permission;
        group.has_correction |= row.has_correction;
        group.has_outcome |= row.has_outcome;
        group.has_interrupted |= is_interrupted;
        group.has_terminal |= is_terminal;
    }

    for group in groups.values_mut() {
        deadline.ensure_remaining()?;
        let (outside_older, outside_newer, has_decision_payload, has_historical_authority): (
            bool,
            bool,
            bool,
            bool,
        ) = transaction.query_row(
            "SELECT
                EXISTS (
                    SELECT 1 FROM activity_events INDEXED BY activity_events_activity_id
                    WHERE activity_id = ?1 AND source_cursor < ?2 LIMIT 1
                ),
                EXISTS (
                    SELECT 1 FROM activity_events INDEXED BY activity_events_activity_id
                    WHERE activity_id = ?1 AND source_cursor >= ?3 LIMIT 1
                ),
                EXISTS (
                    SELECT 1 FROM decision_payloads AS payload
                    JOIN activity_events AS source
                      ON source.source_cursor = payload.source_cursor
                    WHERE source.activity_id = ?1 LIMIT 1
                ),
                EXISTS (
                    SELECT 1 FROM historical_permission_authority AS authority
                    JOIN activity_events AS source
                      ON source.source_cursor = authority.terminal_source_cursor
                    WHERE source.activity_id = ?1 LIMIT 1
                )",
            params![group.activity_id, group.first_cursor, scan_before],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        group.outside_older = outside_older;
        group.outside_newer = outside_newer;
        group.has_decision_payload = has_decision_payload;
        group.has_historical_authority = has_historical_authority;
        if outside_older || outside_newer {
            let (last_cursor, last_recorded_at_ms, has_interrupted, has_terminal): (
                i64,
                i64,
                bool,
                bool,
            ) = transaction.query_row(
                "SELECT MAX(source_cursor), MAX(recorded_at_ms),
                        MAX(event_state = 'interrupted'),
                        MAX(event_state NOT IN ('observed', 'evaluating', 'incomplete'))
                 FROM activity_events INDEXED BY activity_events_activity_id
                 WHERE activity_id = ?1",
                [&group.activity_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            group.group_last_cursor = last_cursor;
            group.last_recorded_at_ms = last_recorded_at_ms;
            group.has_interrupted = has_interrupted;
            group.has_terminal = has_terminal;
        }
    }

    let mut new_history = groups
        .values()
        .filter(|group| {
            (group.has_interrupted
                || (!group.has_terminal && group.last_recorded_at_ms <= stale_cutoff))
                && !group.outside_newer
                && overlap.as_deref() != Some(group.activity_id.as_str())
        })
        .collect::<Vec<_>>();
    new_history.sort_by(|left, right| {
        right
            .group_last_cursor
            .cmp(&left.group_last_cursor)
            .then_with(|| left.activity_id.cmp(&right.activity_id))
    });
    let protected_count = usize::try_from(recent_remaining)
        .unwrap_or(0)
        .min(new_history.len());
    let mut protected_history = new_history
        .iter()
        .take(protected_count)
        .map(|group| group.activity_id.as_str())
        .collect::<BTreeSet<_>>();
    if overlap_keep && let Some(overlap) = overlap.as_deref() {
        protected_history.insert(overlap);
    }
    let next_recent_remaining = (recent_remaining
        - i64::try_from(new_history.len())
            .map_err(|_| StorageError::InvalidStorage("maintenance history is too large"))?)
    .max(0);

    let mut eligible = groups
        .values()
        .filter(|group| {
            !group.outside_older
                && !group.outside_newer
                && !group.has_permission
                && !group.has_correction
                && !group.has_outcome
                && !group.has_decision_payload
                && !group.has_historical_authority
                && !protected_history.contains(group.activity_id.as_str())
                && (group.has_terminal || group.last_recorded_at_ms <= stale_cutoff)
        })
        .collect::<Vec<_>>();
    eligible.sort_by(|left, right| {
        left.first_cursor
            .cmp(&right.first_cursor)
            .then_with(|| left.activity_id.cmp(&right.activity_id))
    });
    let mut selected_cursors = Vec::new();
    for group in eligible {
        if selected_cursors.len() + group.cursors.len() > RETENTION_BATCH_ROWS as usize {
            continue;
        }
        selected_cursors.extend(group.cursors.iter().copied());
    }
    for cursor in &selected_cursors {
        deadline.ensure_remaining()?;
        let deleted = transaction.execute(
            "DELETE FROM activity_events WHERE source_cursor = ?1",
            [cursor],
        )?;
        if deleted != 1 {
            return Err(StorageError::InvalidStorage(
                "maintenance candidate changed during deletion",
            ));
        }
    }

    let bottom = groups
        .values()
        .find(|group| group.cursors.contains(&candidate_min))
        .ok_or(StorageError::InvalidStorage(
            "maintenance boundary group is missing",
        ))?;
    let (next_scan_before, next_overlap, next_overlap_keep) = if bottom.outside_older {
        if overlap.as_deref() == Some(bottom.activity_id.as_str()) {
            (candidate_min, None, false)
        } else {
            let before = bottom
                .last_cursor
                .checked_add(1)
                .ok_or(StorageError::InvalidStorage(
                    "maintenance cursor space is exhausted",
                ))?;
            (
                before.min(boundary),
                Some(bottom.activity_id.as_str()),
                protected_history.contains(bottom.activity_id.as_str()),
            )
        }
    } else {
        (candidate_min, None, false)
    };
    transaction.execute(
        "UPDATE schema_meta
         SET maintenance_retention_boundary = ?1,
             maintenance_scan_before = ?2,
             maintenance_recent_remaining = ?3,
             maintenance_overlap_activity_id = ?4,
             maintenance_overlap_keep = ?5
         WHERE singleton = 1",
        params![
            boundary,
            next_scan_before,
            next_recent_remaining,
            next_overlap,
            i64::from(next_overlap_keep)
        ],
    )?;
    Ok(selected_cursors.len())
}

fn truncate_checkpoint(connection: &rusqlite::Connection) -> Result<(i64, i64, i64), StorageError> {
    let mut log_frames = -1;
    let mut checkpointed_frames = -1;
    // SAFETY: the checkpoint worker exclusively owns `connection` for the
    // duration of this call. The raw handle remains valid, no rusqlite API is
    // used concurrently, and the only cross-thread access is SQLite's
    // documented interrupt operation through `InterruptHandle`.
    let handle = unsafe { connection.handle() };
    let code = unsafe {
        rusqlite::ffi::sqlite3_wal_checkpoint_v2(
            handle,
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_CHECKPOINT_TRUNCATE,
            &mut log_frames,
            &mut checkpointed_frames,
        )
    };
    match code {
        rusqlite::ffi::SQLITE_OK => Ok((0, i64::from(log_frames), i64::from(checkpointed_frames))),
        rusqlite::ffi::SQLITE_BUSY => {
            Ok((1, i64::from(log_frames), i64::from(checkpointed_frames)))
        }
        _ => {
            let extended_code = unsafe { rusqlite::ffi::sqlite3_extended_errcode(handle) };
            let extended_code = if extended_code == rusqlite::ffi::SQLITE_OK {
                code
            } else {
                extended_code
            };
            Err(StorageError::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(extended_code),
                None,
            )))
        }
    }
}

impl BrainDb {
    pub fn admit_model_attempt(&self) -> Result<(), StorageError> {
        super::activity::ensure_deadline(self.deadline)?;
        let wal = self.wal_health()?.1;
        super::activity::ensure_deadline(self.deadline)?;
        if wal == WalHealth::HardLimit {
            Err(StorageError::MaintenanceRequired)
        } else {
            Ok(())
        }
    }

    pub fn admit_deterministic_safety_deny(
        &self,
    ) -> Result<coding_brain_core::lifecycle::PermissionAction, StorageError> {
        Ok(coding_brain_core::lifecycle::PermissionAction::Deny)
    }

    pub fn health(&self) -> Result<StorageHealth, StorageError> {
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let (schema_version, migration_state): (i32, String) = self.connection.query_row(
            "SELECT schema_version, migration_state FROM schema_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let migration = match migration_state.as_str() {
            "complete" => MigrationHealth::Complete,
            "in_progress" => MigrationHealth::InProgress,
            _ => {
                return Err(StorageError::InvalidStorage(
                    "storage migration state is invalid",
                ));
            }
        };
        if schema_version != BRAIN_SCHEMA_VERSION {
            return Err(StorageError::InvalidStorage(
                "storage health schema version is invalid",
            ));
        }
        let (wal_bytes, wal) = self.wal_health()?;
        Ok(StorageHealth {
            database_path: "$XDG_STATE_HOME/coding-brain/db/brain.sqlite3",
            schema_version,
            sqlite_version: rusqlite::version(),
            migration,
            wal_bytes,
            wal,
            integrity: IntegrityHealth::NotChecked,
        })
    }

    pub fn maintain_bounded(
        &mut self,
        retention_cursor: Option<ActivityCursor>,
        deadline: StorageDeadline,
    ) -> Result<MaintenanceOutcome, StorageError> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| StorageError::InvalidStorage("system time is before the Unix epoch"))?
            .as_millis()
            .try_into()
            .map_err(|_| StorageError::InvalidStorage("system time is out of range"))?;
        self.maintain_bounded_at(retention_cursor, deadline, now_ms)
    }

    fn maintain_bounded_at(
        &mut self,
        retention_cursor: Option<ActivityCursor>,
        deadline: StorageDeadline,
        now_ms: u64,
    ) -> Result<MaintenanceOutcome, StorageError> {
        self.require_non_hook()?;
        deadline.apply(&self.connection)?;
        let wal_bytes_before = self.wal_health()?.0;
        let (busy, log_frames, checkpointed_frames) = self.checkpoint(deadline)?;
        if busy != 0 {
            return Err(StorageError::Busy);
        }
        if log_frames < 0 || checkpointed_frames < 0 || checkpointed_frames > log_frames {
            return Err(StorageError::InvalidStorage(
                "SQLite checkpoint result is invalid",
            ));
        }

        let deleted_activity_rows = if let Some(cursor) = retention_cursor {
            let stale_cutoff = now_ms
                .saturating_sub(coding_brain_core::brain_activity::DEFAULT_INTERRUPTED_AFTER_MS);
            let stale_cutoff = i64::try_from(stale_cutoff)
                .map_err(|_| StorageError::InvalidStorage("maintenance time is out of range"))?;
            deadline.ensure_remaining()?;
            let transaction = self
                .connection
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|error| map_sqlite_error(StorageOperation::Maintenance, false, error))?;
            let deleted = run_sql_with_deadline(
                &transaction,
                deadline,
                StorageOperation::Maintenance,
                |_| {
                    retain_activity_window(
                        &transaction,
                        cursor.get() as i64,
                        stale_cutoff,
                        deadline,
                    )
                },
            )?;
            deadline.ensure_remaining()?;
            transaction
                .commit()
                .map_err(|error| map_sqlite_error(StorageOperation::Maintenance, true, error))?;
            deleted
        } else {
            0
        };

        run_sql_with_deadline(
            &self.connection,
            deadline,
            StorageOperation::Maintenance,
            |connection| {
                connection
                    .execute_batch(&format!(
                        "PRAGMA incremental_vacuum({INCREMENTAL_VACUUM_PAGES})"
                    ))
                    .map_err(StorageError::from)
            },
        )?;
        let (busy, _, _) = self.checkpoint(deadline)?;
        if busy != 0 {
            return Err(StorageError::Busy);
        }
        let wal_bytes_after = self.wal_health()?.0;
        Ok(MaintenanceOutcome {
            wal_bytes_before,
            wal_bytes_after,
            checkpointed_frames: checkpointed_frames as u64,
            deleted_activity_rows,
        })
    }

    pub fn deep_integrity_check(
        &self,
        deadline: StorageDeadline,
    ) -> Result<IntegrityHealth, StorageError> {
        self.require_non_hook()?;
        let result = run_sql_with_deadline(
            &self.connection,
            deadline,
            StorageOperation::Integrity,
            |connection| {
                connection
                    .query_row("PRAGMA integrity_check(1)", [], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(StorageError::from)
            },
        )?;
        Ok(if result == "ok" {
            IntegrityHealth::Ok
        } else {
            IntegrityHealth::Corrupt
        })
    }

    fn require_non_hook(&self) -> Result<(), StorageError> {
        if self.role == OpenRole::Hook {
            Err(StorageError::HookMaintenanceForbidden)
        } else {
            Ok(())
        }
    }

    fn checkpoint(&self, deadline: StorageDeadline) -> Result<(i64, i64, i64), StorageError> {
        self.checkpoint_with_seams(deadline, || {}, || {})
    }

    fn checkpoint_with_seams(
        &self,
        deadline: StorageDeadline,
        before_checkpoint_api: impl FnOnce() + Send + 'static,
        worker_finished: impl FnOnce() + Send + 'static,
    ) -> Result<(i64, i64, i64), StorageError> {
        deadline.ensure_remaining()?;
        let state_root = self
            .database_path
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or(StorageError::InvalidStorage("database has no state root"))?;
        let directory = SecureDatabaseDirectory::prepare(state_root, false)?;
        let connection = super::open_current_in_directory(
            &directory,
            BRAIN_DATABASE_NAME,
            super::DatabaseKind::Brain,
            deadline,
        )
        .map_err(|error| map_storage_error(StorageOperation::Checkpoint, false, error))?;
        deadline
            .apply(&connection)
            .map_err(|error| map_storage_error(StorageOperation::Checkpoint, false, error))?;
        let interrupt = connection.get_interrupt_handle();
        let injected_fault = captured_sqlite_fault("maintenance-checkpoint");
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("cbrain-sqlite-checkpoint".into())
            .spawn(move || {
                let result = (|| {
                    deadline.ensure_remaining()?;
                    before_checkpoint_api();
                    if let Some(extended_code) = injected_fault {
                        return Err(StorageError::from(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(extended_code),
                            None,
                        )));
                    }
                    #[cfg(feature = "fault-injection")]
                    if super::hit_fault(
                        super::FaultPoint::Checkpoint,
                        super::FaultPosition::Before,
                    )? {
                        return Err(StorageError::from(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_FSYNC),
                            None,
                        )));
                    }
                    truncate_checkpoint(&connection)
                })()
                .map_err(|error| map_storage_error(StorageOperation::Checkpoint, false, error));
                let _ = result_tx.send(result);
                worker_finished();
            })
            .map_err(StorageError::Io)?;

        let remaining = match deadline.remaining() {
            Ok(remaining) => remaining,
            Err(error) => {
                interrupt.interrupt();
                return Err(error);
            }
        };
        match result_rx.recv_timeout(remaining) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                interrupt.interrupt();
                Err(StorageError::Busy)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(
                StorageError::InvalidStorage("SQLite checkpoint worker terminated"),
            ),
        }
    }

    #[cfg(test)]
    fn checkpoint_with_seams_for_test(
        &self,
        deadline: StorageDeadline,
        before_checkpoint_api: impl FnOnce() + Send + 'static,
        worker_finished: impl FnOnce() + Send + 'static,
    ) -> Result<(i64, i64, i64), StorageError> {
        self.checkpoint_with_seams(deadline, before_checkpoint_api, worker_finished)
    }

    fn wal_health(&self) -> Result<(u64, WalHealth), StorageError> {
        let state_root = self
            .database_path
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or(StorageError::InvalidStorage("database has no state root"))?;
        let directory = SecureDatabaseDirectory::prepare(state_root, false)?;
        directory.validate_path_correspondence()?;
        directory.validate_after_open(BRAIN_DATABASE_NAME)?;
        let bytes = directory
            .private_file_len(c"brain.sqlite3-wal")?
            .unwrap_or(0);
        directory.validate_path_correspondence()?;
        let health = if bytes >= WAL_HARD_LIMIT_BYTES {
            WalHealth::HardLimit
        } else if bytes >= WAL_WARNING_BYTES {
            WalHealth::Warning
        } else {
            WalHealth::Normal
        };
        Ok((bytes, health))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(feature = "fault-injection")]
    use std::fs::File;
    #[cfg(feature = "fault-injection")]
    use std::io::Read;
    #[cfg(feature = "fault-injection")]
    use std::os::fd::{AsRawFd, FromRawFd};
    #[cfg(feature = "fault-injection")]
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use rusqlite::ffi;

    use super::*;

    #[cfg(feature = "fault-injection")]
    #[test]
    #[ignore]
    fn live_checkpoint_fault_process_helper() {
        let Some(root) = std::env::var_os("CODING_BRAIN_CHECKPOINT_LIVE_FAULT_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        crate::brain::storage::activate_fault(crate::brain::storage::FaultActivation {
            capability: std::env::var_os("CODING_BRAIN_CHECKPOINT_LIVE_FAULT_CAPABILITY")
                .unwrap()
                .into(),
            state_root: root.clone(),
            nonce: "checkpoint-live-fault".into(),
            selection: crate::brain::storage::FaultSelection::Matrix(
                crate::brain::storage::FaultPoint::Checkpoint,
            ),
            control_fd: std::env::var("CODING_BRAIN_CHECKPOINT_LIVE_FAULT_FD")
                .unwrap()
                .parse()
                .unwrap(),
        })
        .unwrap();
        let paths = crate::brain::storage::StoragePaths::at(&root);
        let db = BrainDb::open_current(
            &paths,
            OpenRole::NonHook,
            StorageDeadline::after(std::time::Duration::from_secs(2)),
        )
        .unwrap();
        assert!(matches!(
            db.checkpoint(StorageDeadline::after(std::time::Duration::from_secs(2))),
            Err(StorageError::StorageFault {
                operation: StorageOperation::Checkpoint,
                category: StorageFaultCategory::Io,
            })
        ));
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn live_checkpoint_fault_fires_before_truncate_api() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let capability_dir = root.path().join("fault-capability");
        fs::create_dir(&capability_dir).unwrap();
        fs::set_permissions(&capability_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let mut descriptors = [0; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        let mut read = unsafe { File::from_raw_fd(descriptors[0]) };
        let write = unsafe { File::from_raw_fd(descriptors[1]) };
        let metadata = write.metadata().unwrap();
        let capability = capability_dir.join("fault.json");
        fs::write(
            &capability,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "state_root": root.path(),
                "nonce": "checkpoint-live-fault",
                "selection": { "kind": "matrix", "selection": "checkpoint" },
                "control_device": metadata.dev(),
                "control_inode": metadata.ino(),
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&capability, fs::Permissions::from_mode(0o600)).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "brain::storage::maintenance::tests::live_checkpoint_fault_process_helper",
                "--nocapture",
            ])
            .env("CODING_BRAIN_CHECKPOINT_LIVE_FAULT_ROOT", root.path())
            .env("CODING_BRAIN_CHECKPOINT_LIVE_FAULT_CAPABILITY", &capability)
            .env(
                "CODING_BRAIN_CHECKPOINT_LIVE_FAULT_FD",
                write.as_raw_fd().to_string(),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        drop(write);
        assert!(status.success(), "{status:?}");
        let mut marker = Vec::new();
        read.read_to_end(&mut marker).unwrap();
        assert_eq!(marker, b"CBRAIN-FAULT-V1\0checkpoint\0before\0-\n");
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn hook_role_still_cannot_run_maintenance_with_feature_enabled() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut db = BrainDb::open_current(
            &paths,
            OpenRole::Hook,
            StorageDeadline::after(std::time::Duration::from_secs(1)),
        )
        .unwrap();
        assert!(matches!(
            db.maintain_bounded(
                None,
                StorageDeadline::after(std::time::Duration::from_secs(1))
            ),
            Err(StorageError::HookMaintenanceForbidden)
        ));
    }
    use crate::brain::storage::{StorageFaultCategory, StorageOperation};

    fn sqlite_error(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(ffi::Error::new(code), None)
    }

    #[test]
    fn extended_sqlite_faults_keep_operation_and_commit_uncertainty() {
        assert!(matches!(
            super::map_sqlite_error(
                StorageOperation::Admission,
                false,
                sqlite_error(ffi::SQLITE_FULL),
            ),
            StorageError::StorageFault {
                operation: StorageOperation::Admission,
                category: StorageFaultCategory::Full,
            }
        ));
        assert!(matches!(
            super::map_sqlite_error(
                StorageOperation::Commit,
                true,
                sqlite_error(ffi::SQLITE_IOERR_FSYNC),
            ),
            StorageError::CommitUncertain {
                operation: StorageOperation::Commit,
                category: StorageFaultCategory::Io,
            }
        ));
        assert!(matches!(
            super::map_sqlite_error(
                StorageOperation::Integrity,
                false,
                sqlite_error(ffi::SQLITE_CORRUPT),
            ),
            StorageError::StorageFault {
                operation: StorageOperation::Integrity,
                category: StorageFaultCategory::Corrupt,
            }
        ));
        assert!(matches!(
            super::map_sqlite_error(
                StorageOperation::Checkpoint,
                false,
                sqlite_error(ffi::SQLITE_BUSY),
            ),
            StorageError::Busy
        ));
        assert!(matches!(
            super::map_sqlite_error(
                StorageOperation::Checkpoint,
                false,
                sqlite_error(ffi::SQLITE_IOERR_FSYNC),
            ),
            StorageError::StorageFault {
                operation: StorageOperation::Checkpoint,
                category: StorageFaultCategory::Io,
            }
        ));
    }

    #[test]
    fn deadline_progress_interrupts_sql_and_cleans_up_handler() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let callbacks = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&callbacks);

        let result = super::run_sql_with_progress_for_test(
            &connection,
            StorageOperation::Integrity,
            move || {
                observed.fetch_add(1, Ordering::SeqCst);
                true
            },
            |connection| {
                connection
                    .query_row(
                        "WITH RECURSIVE counter(value) AS (
                             VALUES(0) UNION ALL SELECT value + 1 FROM counter WHERE value < 100000
                         ) SELECT sum(value) FROM counter",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(StorageError::from)
            },
        );

        assert!(matches!(result, Err(StorageError::Busy)), "{result:?}");
        assert!(callbacks.load(Ordering::SeqCst) > 0);
        assert_eq!(
            connection
                .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
            "the scoped progress handler must be removed after interruption"
        );
    }

    #[test]
    fn deadline_progress_handler_is_removed_during_unwind() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = super::run_sql_with_progress_for_test(
                &connection,
                StorageOperation::Integrity,
                || true,
                |_| -> Result<(), StorageError> { panic!("injected SQL wrapper panic") },
            );
        }));
        assert!(unwind.is_err());

        let sum = connection.query_row(
            "WITH RECURSIVE counter(value) AS (
                 VALUES(0) UNION ALL SELECT value + 1 FROM counter WHERE value < 1000
             ) SELECT sum(value) FROM counter",
            [],
            |row| row.get::<_, i64>(0),
        );
        assert_eq!(sum.unwrap(), 500_500);
    }

    #[test]
    fn checkpoint_fault_is_mapped_at_the_actual_boundary() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        let db = BrainDb::create_current(&paths).unwrap();

        let result =
            super::with_sqlite_fault("maintenance-checkpoint", ffi::SQLITE_IOERR_FSYNC, || {
                db.checkpoint(StorageDeadline::after(std::time::Duration::from_secs(1)))
            });

        assert!(matches!(
            result,
            Err(StorageError::StorageFault {
                operation: StorageOperation::Checkpoint,
                category: StorageFaultCategory::Io,
            })
        ));
    }

    #[test]
    fn checkpoint_deadline_interrupts_a_worker_before_the_direct_api() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let mut event = coding_brain_core::brain_activity::ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: coding_brain_core::brain_activity::ActivityKind::Diagnostic,
            activity_id: "checkpoint-deadline".into(),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Temporary(
                    "checkpoint-project".into(),
                ),
                cwd: "/work/checkpoint".into(),
                label: None,
            },
            session: None,
            state: coding_brain_core::brain_activity::ActivityState::Error,
            tool: None,
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        };
        db.append_activity(event.clone()).unwrap();

        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let started = std::time::Instant::now();
        let result = db.checkpoint_with_seams_for_test(
            StorageDeadline::after(std::time::Duration::from_millis(30)),
            move || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
            move || finished_tx.send(()).unwrap(),
        );
        let elapsed = started.elapsed();

        assert!(matches!(result, Err(StorageError::Busy)), "{result:?}");
        assert!(
            entered_rx.try_recv().is_ok(),
            "checkpoint worker did not start"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(250),
            "checkpoint exceeded caller deadline allowance: {elapsed:?}"
        );
        release_tx.send(()).unwrap();
        finished_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .expect("interrupted checkpoint worker must exit");

        assert_eq!(
            db.activity_by_id("checkpoint-deadline", None, 10, 64 * 1024)
                .unwrap()
                .events
                .len(),
            1
        );
        event.activity_id = "checkpoint-after-timeout".into();
        db.append_activity(event).unwrap();
        assert_eq!(
            db.activity_by_id("checkpoint-after-timeout", None, 10, 64 * 1024)
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[test]
    fn activity_commit_fault_is_uncertain_at_the_public_boundary() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let event = coding_brain_core::brain_activity::ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: coding_brain_core::brain_activity::ActivityKind::Decision,
            activity_id: "fault-activity".into(),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Temporary(
                    "fault-project".into(),
                ),
                cwd: "/work/fault".into(),
                label: None,
            },
            session: None,
            state: coding_brain_core::brain_activity::ActivityState::Error,
            tool: Some("Bash".into()),
            normalized_command: Some("printf safe".into()),
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: Some("fault-decision".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        };

        let result = super::with_sqlite_fault("activity-commit", ffi::SQLITE_IOERR_FSYNC, || {
            db.append_activity(event)
        });

        assert!(
            matches!(
                &result,
                Err(StorageError::CommitUncertain {
                    operation: StorageOperation::Activity,
                    category: StorageFaultCategory::Io,
                })
            ),
            "{result:?}"
        );
    }

    #[test]
    fn fixed_retention_time_keeps_fresh_incomplete_and_ages_stale_partial_groups() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let now_ms = coding_brain_core::brain_activity::DEFAULT_INTERRUPTED_AFTER_MS + 10_000;
        let make_event = |activity_id: &str, state, recorded_at_ms| {
            coding_brain_core::brain_activity::ActivityEvent {
                schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
                kind: coding_brain_core::brain_activity::ActivityKind::Decision,
                activity_id: activity_id.into(),
                recorded_at_ms,
                project: coding_brain_core::brain_activity::ProjectEvidence {
                    project_id: coding_brain_core::project::ProjectId::Temporary(
                        "retention-project".into(),
                    ),
                    cwd: "/work/retention".into(),
                    label: None,
                },
                session: None,
                state,
                tool: Some("Bash".into()),
                normalized_command: Some("printf safe".into()),
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: Some(format!("decision-{activity_id}")),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            }
        };
        let fresh_at = now_ms - coding_brain_core::brain_activity::DEFAULT_INTERRUPTED_AFTER_MS + 1;
        let mut events = vec![
            make_event(
                "fresh-partial",
                coding_brain_core::brain_activity::ActivityState::Observed,
                fresh_at,
            ),
            make_event(
                "fresh-partial",
                coding_brain_core::brain_activity::ActivityState::Evaluating,
                fresh_at,
            ),
        ];
        for index in 0..40 {
            let activity_id = format!("stale-partial-{index}");
            events.push(make_event(
                &activity_id,
                coding_brain_core::brain_activity::ActivityState::Observed,
                index + 1,
            ));
            events.push(make_event(
                &activity_id,
                coding_brain_core::brain_activity::ActivityState::Evaluating,
                index + 1,
            ));
        }
        db.append_activity_batch(&events).unwrap();
        let retention_cursor = db
            .append_activity(make_event(
                "retention-bound",
                coding_brain_core::brain_activity::ActivityState::Error,
                now_ms,
            ))
            .unwrap();

        let outcome = db
            .maintain_bounded_at(
                Some(retention_cursor),
                StorageDeadline::after(std::time::Duration::from_secs(1)),
                now_ms,
            )
            .unwrap();

        assert_eq!(outcome.deleted_activity_rows, 16);
        assert_eq!(
            db.activity_by_id("fresh-partial", None, 10, 64 * 1024)
                .unwrap()
                .events
                .len(),
            2
        );
        for index in 0..40 {
            let count = db
                .activity_by_id(&format!("stale-partial-{index}"), None, 10, 64 * 1024)
                .unwrap()
                .events
                .len();
            assert_eq!(
                count,
                if index < 8 { 0 } else { 2 },
                "stale-partial-{index}"
            );
        }
    }

    #[test]
    fn decision_lifecycle_and_review_commits_keep_public_operation_context() {
        use coding_brain_core::provider::AgentProvider;
        use coding_brain_core::review_state::{
            ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest, ReviewSurface,
        };

        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = crate::brain::storage::StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let source = coding_brain_core::brain_activity::ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: coding_brain_core::brain_activity::ActivityKind::Decision,
            activity_id: "decision-source".into(),
            recorded_at_ms: 1,
            project: coding_brain_core::brain_activity::ProjectEvidence {
                project_id: coding_brain_core::project::ProjectId::Temporary(
                    "fault-project".into(),
                ),
                cwd: "/work/fault".into(),
                label: None,
            },
            session: None,
            state: coding_brain_core::brain_activity::ActivityState::Error,
            tool: Some("Bash".into()),
            normalized_command: Some("printf safe".into()),
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: Some("decision-write".into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        };
        let cursor = db.append_activity(source).unwrap();
        let record = crate::brain::decisions::DecisionRecord {
            provider: AgentProvider::Codex,
            timestamp: "2026-08-06T00:00:00Z".into(),
            pid: 1,
            project: "fault-project".into(),
            tool: Some("Bash".into()),
            command: Some("printf safe".into()),
            brain_action: "abstain".into(),
            brain_confidence: 0.5,
            brain_reasoning: "bounded".into(),
            user_action: "observed".into(),
            context: None,
            outcome: None,
            decision_type: crate::brain::decisions::DecisionType::Session,
            suggested_at: Some(1),
            resolved_at: Some(1),
            override_reason: None,
            decision_id: Some("decision-write".into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        };
        let decision_result =
            super::with_sqlite_fault("decision-commit", ffi::SQLITE_IOERR_FSYNC, || {
                db.insert_decision(
                    &crate::brain::storage::DecisionIdentity::observation(
                        "decision-write",
                        AgentProvider::Codex,
                        1,
                    ),
                    &crate::brain::storage::DecisionPayload::new(
                        crate::brain::storage::DecisionKind::Observation,
                        cursor,
                        record,
                    ),
                )
            });
        assert!(matches!(
            decision_result,
            Err(StorageError::CommitUncertain {
                operation: StorageOperation::Decision,
                category: StorageFaultCategory::Io,
            })
        ));

        let lifecycle = coding_brain_core::lifecycle::LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "fault-session".into(),
            None,
            None,
            "/work/fault".into(),
        )
        .unwrap();
        let lifecycle_event = coding_brain_core::lifecycle::LifecycleEvent::from_parts(
            lifecycle,
            coding_brain_core::lifecycle::LifecycleEventKind::SessionStart {
                source: coding_brain_core::lifecycle::SessionStartSource::Startup,
            },
        )
        .unwrap();
        let lifecycle_result =
            super::with_sqlite_fault("lifecycle-commit", ffi::SQLITE_IOERR_FSYNC, || {
                db.record_lifecycle(lifecycle_event, 1)
            });
        assert!(matches!(
            lifecycle_result,
            Err(StorageError::CommitUncertain {
                operation: StorageOperation::Lifecycle,
                category: StorageFaultCategory::Io,
            })
        ));

        let mut review = crate::brain::storage::ReviewDb::create_current(&paths).unwrap();
        let review_cursor = ActivityCursor::try_from(1_u64).unwrap();
        let review_key = ReviewKey::derive(ReviewSurface::Attention, b"fault-review");
        let evidence = crate::brain::storage::ReviewEligibility::try_new(
            ReviewSurface::Attention,
            Some(review_cursor),
            vec![crate::brain::storage::ReviewEligibleOccurrence::new(
                ReviewSurface::Attention,
                review_key,
                review_cursor,
            )],
        )
        .unwrap();
        let request = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [review_key].into_iter().collect(),
                disposition: ReviewDisposition::Reviewed,
            },
        };
        let review_result =
            super::with_sqlite_fault("review-commit", ffi::SQLITE_IOERR_FSYNC, || {
                review.mutate(&request, &evidence)
            });
        assert!(matches!(
            review_result,
            Err(StorageError::CommitUncertain {
                operation: StorageOperation::Review,
                category: StorageFaultCategory::Io,
            })
        ));
    }
}
