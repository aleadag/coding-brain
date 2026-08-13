#![allow(dead_code)] // The SQLite foundation stays inactive until the runtime cutover task.

mod activity;
mod decisions;
mod export;
#[cfg(feature = "fault-injection")]
mod fault_injection;
mod legacy;
mod lifecycle;
mod maintenance;
mod migration;
mod permissions;
mod review;
mod runtime_cache;
mod schema;
mod security;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
pub(crate) use maintenance::with_sqlite_fault;

#[cfg(feature = "fault-injection")]
#[allow(unused_imports)]
pub(crate) use fault_injection::{
    Activation as FaultActivation, FaultPoint, FaultPosition, FaultSelection, MigrationFaultStage,
    activate as activate_fault, hit as hit_fault, run_worker as run_fault_worker,
};

use std::cell::Cell;
use std::ffi::{CStr, OsStr};
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use coding_brain_core::review_state::ReviewRequestError;
use fs2::FileExt;
use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags};

use security::{SecureDatabaseDirectory, SecurityError};

#[cfg(test)]
pub(crate) use security::SidecarDiagnosticGuard;

#[allow(unused_imports)]
pub use activity::{ActivityCursor, ActivityPage, ActivityRecord, RecoveryReservationOutcome};
#[allow(unused_imports)]
pub use decisions::{
    DecisionIdentity, DecisionKind, DecisionPayload, ErasureState, LearningDecisionPage,
    LearningErasePaths, LearningReadSession,
};
pub use export::{AuditExporter, LegacyExporter};
#[allow(unused_imports)]
pub use legacy::{
    LEGACY_EXPORT_PROFILE, LegacyFingerprint, LegacyFreezeArtifact, LegacyFreezeIdentity,
    LegacySnapshot, LegacySourceDescriptor, LegacySourceKind, LegacySourceSet, LegacyWriterGuard,
};
#[allow(unused_imports)]
pub use maintenance::{
    IntegrityHealth, MaintenanceOutcome, MigrationHealth, StorageHealth, WAL_AUTOCHECKPOINT_PAGES,
    WAL_HARD_LIMIT_BYTES, WAL_WARNING_BYTES, WalHealth,
};
#[allow(unused_imports)]
pub use migration::{FrozenSourceManifest, MigrationCoordinator, MigrationStatus};
#[allow(unused_imports)]
pub use permissions::{
    AttemptId, CommittedPermission, DeliveryEvidence, HistoricalDeliveryState,
    HistoricalPermissionAuthority, HistoricalPermissionAuthorityPage,
    HistoricalPermissionProvenance, PermissionAdmission, PermissionAttemptGuard,
    PermissionEvidenceKind, PermissionState, PreparedPermissionCommit,
};
#[allow(unused_imports)]
pub use review::{ReviewEligibility, ReviewEligibleOccurrence, ReviewSurfaceState};
#[allow(unused_imports)]
pub use runtime_cache::{
    CacheDeadline, CacheProvenance, CacheRootKey, CacheRow, MAX_RUNTIME_CACHE_ROWS,
    RUNTIME_CACHE_APPLICATION_ID, RUNTIME_CACHE_SCHEMA_VERSION, RuntimeCacheBypass,
    RuntimeCacheReader, RuntimeCacheWriter,
};

pub const BRAIN_APPLICATION_ID: i32 = 0x4342_524e;
pub const BRAIN_SCHEMA_VERSION: i32 = 1;
pub const REVIEW_APPLICATION_ID: i32 = 0x4342_5256;
pub const REVIEW_SCHEMA_VERSION: i32 = 1;

const BRAIN_DATABASE_NAME: &CStr = c"brain.sqlite3";
const REVIEW_DATABASE_NAME: &CStr = c"review.sqlite3";
const RUNTIME_CACHE_DATABASE_NAME: &CStr = c"runtime-cache-v1.sqlite3";
const REVIEW_RESET_GATE_NAME: &CStr = c"review-reset.lock";
const MIGRATION_LOCK_NAME: &CStr = c"migration.lock";

#[derive(Debug, Clone)]
pub struct StoragePaths {
    state_root: PathBuf,
    db_dir: PathBuf,
}

impl StoragePaths {
    pub fn at(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_owned(),
            db_dir: state_root.join("db"),
        }
    }

    pub fn db_dir(&self) -> &Path {
        &self.db_dir
    }

    pub fn brain_db(&self) -> PathBuf {
        self.db_dir
            .join(OsStr::from_bytes(BRAIN_DATABASE_NAME.to_bytes()))
    }

    pub fn review_db(&self) -> PathBuf {
        self.db_dir
            .join(OsStr::from_bytes(REVIEW_DATABASE_NAME.to_bytes()))
    }

    pub fn runtime_cache_v1(&self) -> PathBuf {
        self.db_dir
            .join(OsStr::from_bytes(RUNTIME_CACHE_DATABASE_NAME.to_bytes()))
    }

    fn brain_learning_root(&self) -> PathBuf {
        self.state_root.join("brain")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRole {
    Hook,
    NonHook,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageOperation {
    Open,
    Read,
    Admission,
    Commit,
    Delivery,
    Checkpoint,
    Maintenance,
    Integrity,
    Activity,
    Decision,
    Lifecycle,
    Review,
    Migration,
    Export,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageFaultCategory {
    Busy,
    Full,
    Io,
    Corrupt,
    Other,
}

impl StorageFaultCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Full => "full",
            Self::Io => "io",
            Self::Corrupt => "corrupt",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct StorageDeadline(Instant);

thread_local! {
    // SQLite's timeout budget restarts for each lock event; keep one deadline
    // across every busy callback made by the current synchronous operation.
    static SQLITE_BUSY_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn retry_sqlite_busy_until_deadline(_attempt: i32) -> bool {
    SQLITE_BUSY_DEADLINE.with(|deadline| {
        let Some(deadline) = deadline.get() else {
            return false;
        };
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
        Instant::now() < deadline
    })
}

impl StorageDeadline {
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    pub(crate) fn at(deadline: Instant) -> Self {
        Self(deadline)
    }

    pub fn remaining(self) -> Result<Duration, StorageError> {
        let remaining = self
            .0
            .checked_duration_since(Instant::now())
            .ok_or(StorageError::Busy)?;
        if remaining.is_zero() {
            Err(StorageError::Busy)
        } else {
            Ok(remaining)
        }
    }

    fn ensure_remaining(self) -> Result<(), StorageError> {
        self.remaining().map(|_| ())
    }

    fn apply(self, connection: &Connection) -> Result<(), StorageError> {
        self.ensure_remaining()?;
        SQLITE_BUSY_DEADLINE.with(|deadline| deadline.set(Some(self.0)));
        connection.busy_handler(Some(retry_sqlite_busy_until_deadline))?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum StorageError {
    Busy,
    MaintenanceRequired,
    HookMaintenanceForbidden,
    StorageFault {
        operation: StorageOperation,
        category: StorageFaultCategory,
    },
    CommitUncertain {
        operation: StorageOperation,
        category: StorageFaultCategory,
    },
    MigrationRequired,
    MigrationActive,
    UnsupportedSchema {
        application_id: i32,
        schema_version: i32,
    },
    InvalidStorage(&'static str),
    InvalidReviewRequest(ReviewRequestError),
    StaleReviewRevision,
    ReviewTargetNotEligible,
    ReviewCountMismatch,
    ReviewDispositionConflict,
    ReviewCapacityExceeded,
    ReviewRevisionOverflow,
    PermissionAttemptMismatch,
    PermissionAlreadyCommitted,
    Sqlite(rusqlite::Error),
    Io(io::Error),
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("SQLite storage deadline elapsed or storage is busy"),
            Self::MaintenanceRequired => {
                formatter.write_str("SQLite storage maintenance is required")
            }
            Self::HookMaintenanceForbidden => {
                formatter.write_str("hook processes cannot run SQLite maintenance")
            }
            Self::StorageFault {
                operation,
                category,
            } => write!(
                formatter,
                "SQLite storage {operation:?} failed ({})",
                category.as_str()
            ),
            Self::CommitUncertain {
                operation,
                category,
            } => write!(
                formatter,
                "SQLite storage {operation:?} commit result is uncertain ({})",
                category.as_str()
            ),
            Self::MigrationRequired => formatter.write_str("SQLite storage migration is required"),
            Self::MigrationActive => formatter.write_str("SQLite storage migration is active"),
            Self::UnsupportedSchema {
                application_id,
                schema_version,
            } => write!(
                formatter,
                "unsupported SQLite application/schema {application_id:#x}/{schema_version}"
            ),
            Self::InvalidStorage(reason) => write!(formatter, "invalid SQLite storage: {reason}"),
            Self::InvalidReviewRequest(error) => {
                write!(formatter, "invalid review request: {error:?}")
            }
            Self::StaleReviewRevision => formatter.write_str("review surface revision changed"),
            Self::ReviewTargetNotEligible => {
                formatter.write_str("review target is no longer eligible")
            }
            Self::ReviewCountMismatch => formatter.write_str("review target count changed"),
            Self::ReviewDispositionConflict => {
                formatter.write_str("review target disposition changed")
            }
            Self::ReviewCapacityExceeded => formatter.write_str("review state key limit exceeded"),
            Self::ReviewRevisionOverflow => formatter.write_str("review surface revision overflow"),
            Self::PermissionAttemptMismatch => {
                formatter.write_str("permission attempt identity changed")
            }
            Self::PermissionAlreadyCommitted => {
                formatter.write_str("permission attempt is already committed")
            }
            Self::Sqlite(error) => write!(formatter, "SQLite storage failed: {error}"),
            Self::Io(error) => write!(formatter, "SQLite storage I/O failed: {error}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        maintenance::map_sqlite_error(StorageOperation::Read, false, error)
    }
}

impl StorageError {
    pub fn fault_category(&self) -> StorageFaultCategory {
        match self {
            Self::Busy => StorageFaultCategory::Busy,
            Self::StorageFault { category, .. } | Self::CommitUncertain { category, .. } => {
                *category
            }
            Self::Io(_) => StorageFaultCategory::Io,
            Self::InvalidStorage(_) => StorageFaultCategory::Corrupt,
            Self::Sqlite(error) => {
                maintenance::sqlite_fault_category(error).unwrap_or(StorageFaultCategory::Other)
            }
            _ => StorageFaultCategory::Other,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SecurityError> for StorageError {
    fn from(error: SecurityError) -> Self {
        match error {
            SecurityError::Missing => Self::MigrationRequired,
            SecurityError::Invalid(reason) => Self::InvalidStorage(reason),
            SecurityError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Self::InvalidStorage("database already exists")
            }
            SecurityError::Io(error) => Self::Io(error),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum DatabaseKind {
    Brain,
    Review,
}

impl DatabaseKind {
    fn application_id(self) -> i32 {
        match self {
            Self::Brain => BRAIN_APPLICATION_ID,
            Self::Review => REVIEW_APPLICATION_ID,
        }
    }

    fn schema_version(self) -> i32 {
        match self {
            Self::Brain => BRAIN_SCHEMA_VERSION,
            Self::Review => REVIEW_SCHEMA_VERSION,
        }
    }
}

pub struct BrainDb {
    connection: Connection,
    deadline: Option<StorageDeadline>,
    role: OpenRole,
    database_path: PathBuf,
    learning_root: PathBuf,
}

impl fmt::Debug for BrainDb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrainDb(..)")
    }
}

impl BrainDb {
    pub fn create_current(paths: &StoragePaths) -> Result<Self, StorageError> {
        let connection = create_current(paths, BRAIN_DATABASE_NAME, DatabaseKind::Brain)?;
        Ok(Self {
            connection,
            deadline: None,
            role: OpenRole::NonHook,
            database_path: paths.brain_db(),
            learning_root: paths.brain_learning_root(),
        })
    }

    pub fn open_current(
        paths: &StoragePaths,
        role: OpenRole,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        if role == OpenRole::Hook {
            deadline.ensure_remaining()?;
            migration::hook_preflight(paths)?;
        }
        let connection = open_current(paths, BRAIN_DATABASE_NAME, DatabaseKind::Brain, deadline)?;
        let database = Self {
            connection,
            deadline: Some(deadline),
            role,
            database_path: paths.brain_db(),
            learning_root: paths.brain_learning_root(),
        };
        if role == OpenRole::Hook && !database.erasure_state()?.complete {
            return Err(StorageError::MigrationRequired);
        }
        Ok(database)
    }

    pub fn schema_sql() -> &'static str {
        schema::BRAIN_SCHEMA_SQL
    }

    fn create_staging(
        paths: &StoragePaths,
        database_name: &CStr,
        generation: u64,
    ) -> Result<Self, StorageError> {
        if generation == 0 || generation > i64::MAX as u64 {
            return Err(StorageError::InvalidStorage(
                "migration generation is out of range",
            ));
        }
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, true)?;
        let connection =
            create_current_in_directory(&directory, database_name, DatabaseKind::Brain)?;
        let updated = connection.execute(
            "UPDATE schema_meta
             SET migration_state = 'in_progress', migration_generation = ?1
             WHERE singleton = 1 AND migration_state = 'complete' AND migration_generation = 0",
            [generation as i64],
        )?;
        if updated != 1 {
            return Err(StorageError::InvalidStorage(
                "staging migration metadata is invalid",
            ));
        }
        Ok(Self {
            connection,
            deadline: None,
            role: OpenRole::NonHook,
            database_path: directory
                .path()
                .join(OsStr::from_bytes(database_name.to_bytes())),
            learning_root: paths.brain_learning_root(),
        })
    }

    fn open_published_incomplete(paths: &StoragePaths) -> Result<Self, StorageError> {
        Self::open_incomplete_named(paths, BRAIN_DATABASE_NAME, None, false)
    }

    fn open_published_for_completion(
        paths: &StoragePaths,
        generation: u64,
    ) -> Result<Self, StorageError> {
        Self::open_incomplete_named(paths, BRAIN_DATABASE_NAME, Some(generation), true)
    }

    fn complete_published_migration(
        self,
        paths: &StoragePaths,
        generation: u64,
    ) -> Result<(), StorageError> {
        if generation == 0 || generation > i64::MAX as u64 {
            return Err(StorageError::InvalidStorage(
                "migration generation is out of range",
            ));
        }
        let updated = self.connection.execute(
            "UPDATE schema_meta
             SET migration_state = 'complete'
             WHERE singleton = 1 AND migration_state = 'in_progress'
                   AND migration_generation = ?1",
            [generation as i64],
        )?;
        if updated > 1 {
            return Err(StorageError::InvalidStorage(
                "published migration completion metadata is invalid",
            ));
        }
        migration::migration_fault("after-database-complete");
        self.finish_published(paths)
    }

    fn open_staging_incomplete(
        paths: &StoragePaths,
        database_name: &CStr,
        generation: u64,
    ) -> Result<Self, StorageError> {
        Self::open_incomplete_named(paths, database_name, Some(generation), false)
    }

    fn open_incomplete_named(
        paths: &StoragePaths,
        database_name: &CStr,
        expected_generation: Option<u64>,
        allow_complete: bool,
    ) -> Result<Self, StorageError> {
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)?;
        directory.reject_untrusted_entries(database_name, true)?;
        let database_path = directory
            .path()
            .join(OsStr::from_bytes(database_name.to_bytes()));
        let connection = open_connection(&database_path)?;
        schema::configure_connection(&connection, None)?;
        let metadata = connection.query_row(
            "SELECT application_id, schema_version, schema_generation,
                    migration_state, migration_generation
             FROM schema_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        if metadata.0 != BRAIN_APPLICATION_ID
            || metadata.1 != BRAIN_SCHEMA_VERSION
            || metadata.2 != BRAIN_SCHEMA_VERSION
            || (!allow_complete && metadata.3 != "in_progress")
            || (allow_complete && !matches!(metadata.3.as_str(), "in_progress" | "complete"))
            || metadata.4 <= 0
            || expected_generation.is_some_and(|generation| metadata.4 != generation as i64)
        {
            return Err(StorageError::InvalidStorage(
                "published migration metadata is invalid",
            ));
        }
        directory.validate_after_open(database_name)?;
        directory.validate_path_correspondence()?;
        Ok(Self {
            connection,
            deadline: None,
            role: OpenRole::NonHook,
            database_path,
            learning_root: paths.brain_learning_root(),
        })
    }

    fn discard_staging(
        self,
        paths: &StoragePaths,
        database_name: &CStr,
    ) -> Result<(), StorageError> {
        self.connection
            .close()
            .map_err(|(_, error)| StorageError::Sqlite(error))?;
        SecureDatabaseDirectory::prepare(&paths.state_root, false)?
            .remove_database(database_name)?;
        Ok(())
    }

    fn finish_staging(
        self,
        paths: &StoragePaths,
        database_name: &CStr,
    ) -> Result<(), StorageError> {
        let checkpoint = self
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| {
                maintenance::map_sqlite_error(StorageOperation::Checkpoint, false, error)
            })?;
        if checkpoint != (0, 0, 0) {
            return Err(StorageError::InvalidStorage(
                "staging WAL checkpoint is incomplete",
            ));
        }
        let journal_mode: String =
            self.connection
                .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
        if journal_mode != "delete" {
            return Err(StorageError::InvalidStorage(
                "staging journal mode did not close cleanly",
            ));
        }
        self.connection
            .close()
            .map_err(|(_, error)| StorageError::Sqlite(error))?;
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)?;
        directory.validate_database_without_sidecars(database_name)?;
        directory.sync_database(database_name)?;
        directory.validate_path_correspondence()?;
        Ok(())
    }

    fn finish_published(self, paths: &StoragePaths) -> Result<(), StorageError> {
        let journal_mode: String =
            self.connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if journal_mode != "wal" {
            return Err(StorageError::InvalidStorage(
                "published database did not enter WAL mode",
            ));
        }
        let checkpoint = self
            .connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| {
                maintenance::map_sqlite_error(StorageOperation::Checkpoint, false, error)
            })?;
        if checkpoint != (0, 0, 0) {
            return Err(StorageError::InvalidStorage(
                "published WAL checkpoint is incomplete",
            ));
        }
        self.connection
            .close()
            .map_err(|(_, error)| StorageError::Sqlite(error))?;
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)?;
        directory.validate_database_without_sidecars(BRAIN_DATABASE_NAME)?;
        directory.sync_database(BRAIN_DATABASE_NAME)?;
        directory.validate_path_correspondence()?;
        Ok(())
    }

    pub fn application_id(&self) -> Result<i32, StorageError> {
        self.pragma_i64("application_id").map(|value| value as i32)
    }

    pub fn user_version(&self) -> Result<i32, StorageError> {
        self.pragma_i64("user_version").map(|value| value as i32)
    }

    pub fn pragma_i64(&self, pragma: &str) -> Result<i64, StorageError> {
        let sql = match pragma {
            "application_id" => "PRAGMA application_id",
            "foreign_keys" => "PRAGMA foreign_keys",
            "secure_delete" => "PRAGMA secure_delete",
            "synchronous" => "PRAGMA synchronous",
            "trusted_schema" => "PRAGMA trusted_schema",
            "user_version" => "PRAGMA user_version",
            "wal_autocheckpoint" => "PRAGMA wal_autocheckpoint",
            _ => return Err(StorageError::InvalidStorage("unsupported pragma query")),
        };
        Ok(self.connection.query_row(sql, [], |row| row.get(0))?)
    }

    pub fn pragma_string(&self, pragma: &str) -> Result<String, StorageError> {
        let sql = match pragma {
            "journal_mode" => "PRAGMA journal_mode",
            _ => return Err(StorageError::InvalidStorage("unsupported pragma query")),
        };
        Ok(self.connection.query_row(sql, [], |row| row.get(0))?)
    }

    pub fn defensive_mode(&self) -> Result<bool, StorageError> {
        Ok(self
            .connection
            .db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE)?)
    }

    pub fn limit(&self, limit: Limit) -> Result<i32, StorageError> {
        Ok(self.connection.limit(limit)?)
    }
}

pub struct ReviewDb {
    connection: Connection,
    _reset_guard: ReviewResetGuard,
    deadline: Option<StorageDeadline>,
}

struct ReviewResetGuard {
    _gate: File,
    directory: SecureDatabaseDirectory,
}

struct ReviewMigrationGuard {
    _state_root_lock: File,
    gate: Option<File>,
    directory: SecureDatabaseDirectory,
}

impl ReviewMigrationGuard {
    fn acquire_gate(&mut self) -> Result<(), StorageError> {
        let gate = self
            .directory
            .open_lock_file(REVIEW_RESET_GATE_NAME, true)?;
        lock_review_reset_file(&gate, true)?;
        self.directory
            .sync_lock_file(REVIEW_RESET_GATE_NAME, &gate)?;
        self.gate = Some(gate);
        Ok(())
    }

    fn validate(&self) -> Result<(), StorageError> {
        let gate = self.gate.as_ref().ok_or(StorageError::InvalidStorage(
            "review reset gate is not held",
        ))?;
        self.directory
            .validate_lock_file(REVIEW_RESET_GATE_NAME, gate)?;
        self.directory.validate_path_correspondence()?;
        Ok(())
    }
}

impl fmt::Debug for ReviewDb {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ReviewDb(..)")
    }
}

impl ReviewDb {
    pub fn create_current(paths: &StoragePaths) -> Result<Self, StorageError> {
        let reset_guard = acquire_review_reset_guard(paths, true, false)?;
        Self::create_current_after_guard(reset_guard)
    }

    fn create_current_after_guard(reset_guard: ReviewResetGuard) -> Result<Self, StorageError> {
        let connection = create_current_in_directory(
            &reset_guard.directory,
            REVIEW_DATABASE_NAME,
            DatabaseKind::Review,
        )?;
        reset_guard.directory.validate_path_correspondence()?;
        Ok(Self {
            connection,
            _reset_guard: reset_guard,
            deadline: None,
        })
    }

    pub fn open_current(
        paths: &StoragePaths,
        _role: OpenRole,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        deadline.ensure_remaining()?;
        let reset_guard = acquire_review_reset_guard(paths, false, false)?;
        deadline.ensure_remaining()?;
        Self::open_current_after_guard(reset_guard, deadline)
    }

    fn open_current_after_guard(
        reset_guard: ReviewResetGuard,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        reset_guard.directory.validate_path_correspondence()?;
        let connection = open_current_in_directory(
            &reset_guard.directory,
            REVIEW_DATABASE_NAME,
            DatabaseKind::Review,
            deadline,
        )?;
        reset_guard.directory.validate_path_correspondence()?;
        Ok(Self {
            connection,
            _reset_guard: reset_guard,
            deadline: Some(deadline),
        })
    }

    pub fn reset(paths: &StoragePaths) -> Result<(), StorageError> {
        let reset_guard = acquire_review_reset_guard(paths, true, true)?;
        reset_guard
            .directory
            .remove_database(REVIEW_DATABASE_NAME)?;
        drop(create_current_in_directory(
            &reset_guard.directory,
            REVIEW_DATABASE_NAME,
            DatabaseKind::Review,
        )?);
        reset_guard.directory.validate_path_correspondence()?;
        Ok(())
    }

    pub fn schema_sql() -> &'static str {
        schema::REVIEW_SCHEMA_SQL
    }

    pub fn application_id(&self) -> Result<i32, StorageError> {
        Ok(self
            .connection
            .query_row("PRAGMA application_id", [], |row| row.get(0))?)
    }

    pub fn user_version(&self) -> Result<i32, StorageError> {
        Ok(self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }
}

fn acquire_review_reset_guard(
    paths: &StoragePaths,
    create: bool,
    exclusive: bool,
) -> Result<ReviewResetGuard, StorageError> {
    let directory = SecureDatabaseDirectory::prepare(&paths.state_root, create)?;
    lock_review_reset_file(directory.state_root_descriptor(), exclusive)?;
    directory.validate_path_correspondence()?;
    let gate = directory.open_lock_file(REVIEW_RESET_GATE_NAME, create)?;
    lock_review_reset_file(&gate, exclusive)?;
    directory.validate_lock_file(REVIEW_RESET_GATE_NAME, &gate)?;
    directory.validate_path_correspondence()?;
    Ok(ReviewResetGuard {
        _gate: gate,
        directory,
    })
}

fn acquire_review_reset_guard_for_migration(
    paths: &StoragePaths,
) -> Result<ReviewMigrationGuard, StorageError> {
    let directory = SecureDatabaseDirectory::prepare(&paths.state_root, true)?;
    let mut guard = acquire_review_reset_root_for_migration(&directory)?;
    guard.acquire_gate()?;
    Ok(guard)
}

fn acquire_review_reset_root_for_migration(
    directory: &SecureDatabaseDirectory,
) -> Result<ReviewMigrationGuard, StorageError> {
    let directory = directory.try_clone()?;
    let state_root_lock = directory.open_state_root_lock()?;
    lock_review_reset_file(&state_root_lock, true)?;
    directory.validate_path_correspondence()?;
    Ok(ReviewMigrationGuard {
        _state_root_lock: state_root_lock,
        gate: None,
        directory,
    })
}

fn lock_review_reset_file(file: &File, exclusive: bool) -> Result<(), StorageError> {
    let result = if exclusive {
        file.try_lock_exclusive()
    } else {
        FileExt::try_lock_shared(file)
    };
    result.map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            StorageError::Busy
        } else {
            StorageError::Io(error)
        }
    })
}

fn create_current(
    paths: &StoragePaths,
    database_name: &CStr,
    kind: DatabaseKind,
) -> Result<Connection, StorageError> {
    let directory = SecureDatabaseDirectory::prepare(&paths.state_root, true)?;
    create_current_in_directory(&directory, database_name, kind)
}

fn create_current_in_directory(
    directory: &SecureDatabaseDirectory,
    database_name: &CStr,
    kind: DatabaseKind,
) -> Result<Connection, StorageError> {
    directory.validate_path_correspondence()?;
    directory.reject_untrusted_entries(database_name, false)?;
    let file = directory.create_database_file(database_name)?;
    drop(file);
    let connection = open_connection(
        &directory
            .path()
            .join(OsStr::from_bytes(database_name.to_bytes())),
    )?;
    schema::configure_connection(&connection, None)?;
    schema::initialize_current(&connection, kind)?;
    directory.validate_after_open(database_name)?;
    directory.validate_path_correspondence()?;
    Ok(connection)
}

fn open_current(
    paths: &StoragePaths,
    database_name: &CStr,
    kind: DatabaseKind,
    deadline: StorageDeadline,
) -> Result<Connection, StorageError> {
    (|| {
        deadline.ensure_remaining()?;
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)?;
        open_current_in_directory(&directory, database_name, kind, deadline)
    })()
    .map_err(|error| maintenance::map_storage_error(StorageOperation::Open, false, error))
}

fn open_current_in_directory(
    directory: &SecureDatabaseDirectory,
    database_name: &CStr,
    kind: DatabaseKind,
    deadline: StorageDeadline,
) -> Result<Connection, StorageError> {
    deadline.ensure_remaining()?;
    directory.reject_untrusted_entries(database_name, true)?;
    deadline.ensure_remaining()?;
    let connection = open_connection(
        &directory
            .path()
            .join(OsStr::from_bytes(database_name.to_bytes())),
    )?;
    deadline.apply(&connection)?;
    schema::configure_connection(&connection, Some(deadline))?;
    deadline.ensure_remaining()?;
    schema::verify_current(&connection, kind, deadline)?;
    directory.validate_after_open(database_name)?;
    deadline.ensure_remaining()?;
    Ok(connection)
}

fn open_connection(path: &Path) -> Result<Connection, StorageError> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_EXRESCODE,
    )?)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn replace_database_directory(root: &Path, copy_review_database: bool) {
        let paths = StoragePaths::at(root);
        let retained = root.join("db-retained");
        fs::rename(paths.db_dir(), &retained).unwrap();
        fs::create_dir(paths.db_dir()).unwrap();
        fs::set_permissions(paths.db_dir(), fs::Permissions::from_mode(0o700)).unwrap();
        if copy_review_database {
            fs::copy(retained.join("review.sqlite3"), paths.review_db()).unwrap();
            fs::set_permissions(paths.review_db(), fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn review_create_rejects_database_directory_replacement_after_guard_acquisition() {
        let create_root = tempfile::tempdir().unwrap();
        fs::set_permissions(create_root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let create_paths = StoragePaths::at(create_root.path());
        let create_guard = acquire_review_reset_guard(&create_paths, true, false).unwrap();
        replace_database_directory(create_root.path(), false);

        assert!(matches!(
            ReviewDb::create_current_after_guard(create_guard),
            Err(StorageError::InvalidStorage(_))
        ));
        assert!(!create_paths.review_db().exists());
    }

    #[test]
    fn review_open_rejects_database_directory_replacement_after_guard_acquisition() {
        let open_root = tempfile::tempdir().unwrap();
        fs::set_permissions(open_root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let open_paths = StoragePaths::at(open_root.path());
        drop(ReviewDb::create_current(&open_paths).unwrap());
        let open_guard = acquire_review_reset_guard(&open_paths, false, false).unwrap();
        replace_database_directory(open_root.path(), true);

        assert!(matches!(
            ReviewDb::open_current_after_guard(
                open_guard,
                StorageDeadline::after(Duration::from_millis(250)),
            ),
            Err(StorageError::InvalidStorage(_))
        ));
    }

    #[test]
    fn review_migration_guard_final_validation_rejects_gate_identity_change() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StoragePaths::at(root.path());
        let guard = acquire_review_reset_guard_for_migration(&paths).unwrap();
        let gate = paths.db_dir().join("review-reset.lock");
        fs::remove_file(&gate).unwrap();
        let replacement = File::options()
            .write(true)
            .create_new(true)
            .open(&gate)
            .unwrap();
        replacement
            .set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        replacement.sync_all().unwrap();

        assert!(matches!(
            guard.validate(),
            Err(StorageError::InvalidStorage(_))
        ));
    }
}
