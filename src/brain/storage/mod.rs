#![allow(dead_code)] // The SQLite foundation stays inactive until the runtime cutover task.

mod activity;
mod decisions;
mod lifecycle;
mod permissions;
mod review;
mod schema;
mod security;

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
use rusqlite::{Connection, ErrorCode, OpenFlags};

use security::{SecureDatabaseDirectory, SecurityError};

#[allow(unused_imports)]
pub use activity::{ActivityCursor, ActivityPage, ActivityRecord};
#[allow(unused_imports)]
pub use decisions::{
    DecisionIdentity, DecisionKind, DecisionPayload, ErasureState, LearningDecisionPage,
    LearningErasePaths, LearningReadSession,
};
#[allow(unused_imports)]
pub use permissions::{
    AttemptId, CommittedPermission, DeliveryEvidence, HistoricalDeliveryState,
    HistoricalPermissionAuthority, HistoricalPermissionAuthorityPage,
    HistoricalPermissionProvenance, PermissionAdmission, PermissionAttemptGuard,
    PermissionEvidenceKind, PermissionState, PreparedPermissionCommit,
};
#[allow(unused_imports)]
pub use review::{ReviewEligibility, ReviewEligibleOccurrence, ReviewSurfaceState};

pub const BRAIN_APPLICATION_ID: i32 = 0x4342_524e;
pub const BRAIN_SCHEMA_VERSION: i32 = 1;
pub const REVIEW_APPLICATION_ID: i32 = 0x4342_5256;
pub const REVIEW_SCHEMA_VERSION: i32 = 1;

const BRAIN_DATABASE_NAME: &CStr = c"brain.sqlite3";
const REVIEW_DATABASE_NAME: &CStr = c"review.sqlite3";
const REVIEW_RESET_GATE_NAME: &CStr = c"review-reset.lock";

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

    fn brain_learning_root(&self) -> PathBuf {
        self.state_root.join("brain")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OpenRole {
    Hook,
    NonHook,
}

#[derive(Clone, Copy, Debug)]
pub struct StorageDeadline(Instant);

impl StorageDeadline {
    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
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
        connection.busy_timeout(self.remaining()?)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum StorageError {
    Busy,
    MigrationRequired,
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
            Self::MigrationRequired => formatter.write_str("SQLite storage migration is required"),
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
        if matches!(
            error.sqlite_error_code(),
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
        ) {
            Self::Busy
        } else {
            Self::Sqlite(error)
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
            database_path: paths.brain_db(),
            learning_root: paths.brain_learning_root(),
        })
    }

    pub fn open_current(
        paths: &StoragePaths,
        role: OpenRole,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        let connection = open_current(paths, BRAIN_DATABASE_NAME, DatabaseKind::Brain, deadline)?;
        let database = Self {
            connection,
            deadline: Some(deadline),
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
    deadline.ensure_remaining()?;
    let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)?;
    open_current_in_directory(&directory, database_name, kind, deadline)
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
}
