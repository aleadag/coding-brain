use std::cell::Cell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};

use super::security::{SecureDatabaseDirectory, SecurityError};
use super::{RUNTIME_CACHE_DATABASE_NAME, StoragePaths};

pub const RUNTIME_CACHE_APPLICATION_ID: i32 = 0x4342_5243;
pub const RUNTIME_CACHE_SCHEMA_VERSION: i32 = 1;
pub const MAX_RUNTIME_CACHE_ROWS: usize = 256;

const MAX_EVIDENCE_BYTES: usize = 65_536;
const MAX_ROOT_BYTES: usize = 4_096;
const MAX_TOTAL_ROOT_BYTES: usize = 65_536;
const MAX_ROOT_COMPONENTS: usize = 128;
const MAX_SQL_BYTES: i32 = 16_384;
const MAX_VALUE_BYTES: i32 = 131_072;
const MAX_COLUMNS: i32 = 8;

const RUNTIME_CACHE_SCHEMA_SQL: &str = "CREATE TABLE project_identity_cache (
    canonical_root BLOB PRIMARY KEY NOT NULL,
    project_uuid TEXT NOT NULL CHECK(length(project_uuid) = 36),
    provenance INTEGER NOT NULL CHECK(provenance IN (1, 2)),
    evidence BLOB NOT NULL CHECK(length(evidence) BETWEEN 1 AND 65536),
    refresh_order INTEGER NOT NULL,
    row_version INTEGER NOT NULL CHECK(row_version = 1)
) STRICT";

thread_local! {
    static CACHE_BUSY_DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

fn retry_cache_busy_until_deadline(_attempt: i32) -> bool {
    CACHE_BUSY_DEADLINE.with(|deadline| {
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

#[derive(Clone, Copy, Debug)]
pub struct CacheDeadline(Instant);

impl CacheDeadline {
    pub fn at(deadline: Instant) -> Self {
        Self(deadline)
    }

    pub fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    fn ensure_remaining(self) -> Result<(), RuntimeCacheBypass> {
        if self.0.saturating_duration_since(Instant::now()).is_zero() {
            Err(RuntimeCacheBypass::Deadline)
        } else {
            Ok(())
        }
    }

    fn apply(self, connection: &Connection) -> Result<(), RuntimeCacheBypass> {
        self.apply_with_busy_policy(connection, BusyPolicy::Deadline)
    }

    fn apply_with_busy_policy(
        self,
        connection: &Connection,
        busy_policy: BusyPolicy,
    ) -> Result<(), RuntimeCacheBypass> {
        self.ensure_remaining()?;
        match busy_policy {
            BusyPolicy::Deadline => {
                CACHE_BUSY_DEADLINE.with(|deadline| deadline.set(Some(self.0)));
                connection
                    .busy_handler(Some(retry_cache_busy_until_deadline))
                    .map_err(map_sqlite_error)?;
            }
            BusyPolicy::Immediate => {
                CACHE_BUSY_DEADLINE.with(|deadline| deadline.set(None));
                connection.busy_handler(None).map_err(map_sqlite_error)?;
            }
        }
        let deadline = self.0;
        connection
            .progress_handler(1_000, Some(move || Instant::now() >= deadline))
            .map_err(map_sqlite_error)
    }
}

#[derive(Clone, Copy)]
enum BusyPolicy {
    Deadline,
    Immediate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCacheBypass {
    Missing,
    Miss,
    Unsafe,
    Incompatible,
    Corrupt,
    Contended,
    Deadline,
}

impl fmt::Display for RuntimeCacheBypass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Missing => "runtime cache is absent",
            Self::Miss => "runtime cache row is absent",
            Self::Unsafe => "runtime cache path is unsafe",
            Self::Incompatible => "runtime cache format is incompatible",
            Self::Corrupt => "runtime cache is corrupt",
            Self::Contended => "runtime cache is contended",
            Self::Deadline => "runtime cache deadline elapsed",
        })
    }
}

impl std::error::Error for RuntimeCacheBypass {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheProvenance {
    Manifest,
    NetworkRemote,
}

impl CacheProvenance {
    fn encode(self) -> i64 {
        match self {
            Self::Manifest => 1,
            Self::NetworkRemote => 2,
        }
    }

    fn decode(value: i64) -> Result<Self, RuntimeCacheBypass> {
        match value {
            1 => Ok(Self::Manifest),
            2 => Ok(Self::NetworkRemote),
            _ => Err(RuntimeCacheBypass::Corrupt),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CacheRootKey(Vec<u8>);

impl CacheRootKey {
    pub fn from_canonical_path(path: &Path) -> Result<Self, RuntimeCacheBypass> {
        Self::from_bytes(path.as_os_str().as_bytes().to_vec())
    }

    fn from_bytes(bytes: Vec<u8>) -> Result<Self, RuntimeCacheBypass> {
        if bytes.is_empty() || bytes.len() > MAX_ROOT_BYTES || bytes.contains(&0) {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        let path = PathBuf::from(OsString::from_vec(bytes.clone()));
        let normalized = path.components().collect::<PathBuf>();
        if normalized.as_os_str().as_bytes() != bytes {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        let mut components = path.components();
        if !matches!(components.next(), Some(Component::RootDir)) {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        let mut depth = 0usize;
        for component in components {
            if !matches!(component, Component::Normal(_)) {
                return Err(RuntimeCacheBypass::Corrupt);
            }
            depth = depth.checked_add(1).ok_or(RuntimeCacheBypass::Corrupt)?;
            if depth > MAX_ROOT_COMPONENTS {
                return Err(RuntimeCacheBypass::Corrupt);
            }
        }
        Ok(Self(bytes))
    }

    pub fn as_path(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0.clone()))
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheRow {
    root: CacheRootKey,
    project_uuid: String,
    provenance: CacheProvenance,
    evidence: Vec<u8>,
    refresh_order: i64,
}

impl CacheRow {
    pub fn new(
        root: CacheRootKey,
        project_uuid: &str,
        provenance: CacheProvenance,
        evidence: Vec<u8>,
        refresh_order: i64,
    ) -> Result<Self, RuntimeCacheBypass> {
        if !is_canonical_uuid(project_uuid)
            || evidence.is_empty()
            || evidence.len() > MAX_EVIDENCE_BYTES
        {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        Ok(Self {
            root,
            project_uuid: project_uuid.to_owned(),
            provenance,
            evidence,
            refresh_order,
        })
    }

    pub fn root(&self) -> &CacheRootKey {
        &self.root
    }

    pub fn project_uuid(&self) -> &str {
        &self.project_uuid
    }

    pub fn provenance(&self) -> CacheProvenance {
        self.provenance
    }

    pub fn evidence(&self) -> &[u8] {
        &self.evidence
    }

    pub fn refresh_order(&self) -> i64 {
        self.refresh_order
    }
}

pub struct RuntimeCacheReader {
    connection: Connection,
    deadline: CacheDeadline,
}

impl fmt::Debug for RuntimeCacheReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeCacheReader(..)")
    }
}

impl RuntimeCacheReader {
    pub fn open_existing_read_only(
        paths: &StoragePaths,
        deadline: CacheDeadline,
    ) -> Result<Self, RuntimeCacheBypass> {
        deadline.ensure_remaining()?;
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, false)
            .map_err(map_security_error)?;
        directory
            .reject_untrusted_entries(RUNTIME_CACHE_DATABASE_NAME, false)
            .map_err(map_security_error)?;
        if directory
            .private_file_len(RUNTIME_CACHE_DATABASE_NAME)
            .map_err(map_security_error)?
            .is_none()
        {
            return Err(RuntimeCacheBypass::Missing);
        }
        deadline.ensure_remaining()?;
        let database_path = directory
            .path()
            .join(OsStr::from_bytes(RUNTIME_CACHE_DATABASE_NAME.to_bytes()));
        let connection = Connection::open_with_flags(
            sqlite_read_only_uri(&database_path),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_EXRESCODE,
        )
        .map_err(map_sqlite_error)?;
        configure_reader(&connection, deadline)?;
        verify_cache(&connection, deadline, BusyPolicy::Immediate)?;
        directory
            .validate_after_open(RUNTIME_CACHE_DATABASE_NAME)
            .map_err(map_security_error)?;
        directory
            .validate_path_correspondence()
            .map_err(map_security_error)?;
        deadline.ensure_remaining()?;
        Ok(Self {
            connection,
            deadline,
        })
    }

    pub fn candidate_roots(&self) -> Result<Vec<CacheRootKey>, RuntimeCacheBypass> {
        self.deadline.apply(&self.connection)?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT canonical_root
                 FROM project_identity_cache
                 ORDER BY canonical_root
                 LIMIT 257",
            )
            .map_err(map_sqlite_error)?;
        let mut rows = statement.query([]).map_err(map_sqlite_error)?;
        let mut roots = Vec::new();
        let mut total_bytes = 0usize;
        while let Some(row) = rows.next().map_err(map_sqlite_error)? {
            if roots.len() == MAX_RUNTIME_CACHE_ROWS {
                return Err(RuntimeCacheBypass::Corrupt);
            }
            let bytes = row
                .get::<_, Vec<u8>>(0)
                .map_err(|_| RuntimeCacheBypass::Corrupt)?;
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .ok_or(RuntimeCacheBypass::Corrupt)?;
            if total_bytes > MAX_TOTAL_ROOT_BYTES {
                return Err(RuntimeCacheBypass::Corrupt);
            }
            roots.push(CacheRootKey::from_bytes(bytes)?);
            self.deadline.ensure_remaining()?;
        }
        Ok(roots)
    }

    pub fn load_selected_row(&self, key: &CacheRootKey) -> Result<CacheRow, RuntimeCacheBypass> {
        self.deadline.apply(&self.connection)?;
        let selected = self
            .connection
            .query_row(
                "SELECT project_uuid, provenance,
                        CASE WHEN length(evidence) BETWEEN 1 AND 65536 THEN evidence END,
                        refresh_order, row_version
                 FROM project_identity_cache
                 WHERE canonical_root = ?1",
                [key.as_bytes()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite_error)?;
        self.deadline.ensure_remaining()?;
        let Some((project_uuid, provenance, evidence, refresh_order, row_version)) = selected
        else {
            return Err(RuntimeCacheBypass::Miss);
        };
        if row_version != 1 {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        CacheRow::new(
            key.clone(),
            &project_uuid,
            CacheProvenance::decode(provenance)?,
            evidence.ok_or(RuntimeCacheBypass::Corrupt)?,
            refresh_order,
        )
    }
}

pub struct RuntimeCacheWriter {
    connection: Connection,
    deadline: CacheDeadline,
}

impl fmt::Debug for RuntimeCacheWriter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeCacheWriter(..)")
    }
}

impl RuntimeCacheWriter {
    pub fn create_or_open_after_activity(
        paths: &StoragePaths,
        deadline: CacheDeadline,
    ) -> Result<Self, RuntimeCacheBypass> {
        deadline.ensure_remaining()?;
        let directory = SecureDatabaseDirectory::prepare(&paths.state_root, true)
            .map_err(map_security_error)?;
        directory
            .validate_path_correspondence()
            .map_err(map_security_error)?;
        directory
            .reject_untrusted_entries(RUNTIME_CACHE_DATABASE_NAME, false)
            .map_err(map_security_error)?;
        let exists = directory
            .private_file_len(RUNTIME_CACHE_DATABASE_NAME)
            .map_err(map_security_error)?
            .is_some();
        let created = if exists {
            false
        } else {
            let file = directory
                .create_database_file(RUNTIME_CACHE_DATABASE_NAME)
                .map_err(map_creation_error)?;
            drop(file);
            true
        };
        deadline.ensure_remaining()?;
        let database_path = directory
            .path()
            .join(OsStr::from_bytes(RUNTIME_CACHE_DATABASE_NAME.to_bytes()));
        let connection = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_EXRESCODE,
        )
        .map_err(map_sqlite_error)?;
        configure_writer(&connection, deadline, created)?;
        if created {
            initialize_cache(&connection, deadline)?;
        } else {
            verify_cache(&connection, deadline, BusyPolicy::Immediate)?;
        }
        directory
            .validate_after_open(RUNTIME_CACHE_DATABASE_NAME)
            .map_err(map_security_error)?;
        directory
            .validate_path_correspondence()
            .map_err(map_security_error)?;
        deadline.ensure_remaining()?;
        Ok(Self {
            connection,
            deadline,
        })
    }

    pub fn upsert_and_prune(&mut self, row: &CacheRow) -> Result<(), RuntimeCacheBypass> {
        self.deadline.apply(&self.connection)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite_error)?;
        transaction
            .execute(
                "INSERT INTO project_identity_cache (
                     canonical_root, project_uuid, provenance, evidence, refresh_order, row_version
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 1)
                 ON CONFLICT(canonical_root) DO UPDATE SET
                     project_uuid = excluded.project_uuid,
                     provenance = excluded.provenance,
                     evidence = excluded.evidence,
                     refresh_order = excluded.refresh_order,
                     row_version = excluded.row_version",
                params![
                    row.root.as_bytes(),
                    row.project_uuid,
                    row.provenance.encode(),
                    row.evidence,
                    row.refresh_order,
                ],
            )
            .map_err(map_sqlite_error)?;
        let count = transaction
            .query_row("SELECT count(*) FROM project_identity_cache", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(map_sqlite_error)?;
        if count < 0 || count > MAX_RUNTIME_CACHE_ROWS as i64 + 1 {
            return Err(RuntimeCacheBypass::Corrupt);
        }
        let excess = (count - MAX_RUNTIME_CACHE_ROWS as i64).max(0);
        if excess != 0 {
            transaction
                .execute(
                    "DELETE FROM project_identity_cache
                     WHERE canonical_root IN (
                         SELECT canonical_root
                         FROM project_identity_cache
                         WHERE canonical_root <> ?1
                         ORDER BY refresh_order ASC, canonical_root ASC
                         LIMIT ?2
                     )",
                    params![row.root.as_bytes(), excess],
                )
                .map_err(map_sqlite_error)?;
        }
        self.deadline.ensure_remaining()?;
        #[cfg(feature = "fault-injection")]
        match super::hit_fault(
            super::FaultPoint::CacheCommitBeforeCall,
            super::FaultPosition::Before,
        ) {
            Ok(true) => std::process::abort(),
            Ok(false) => {}
            Err(_) => return Err(RuntimeCacheBypass::Corrupt),
        }
        transaction.commit().map_err(map_sqlite_error)?;
        #[cfg(feature = "fault-injection")]
        match super::hit_fault(
            super::FaultPoint::CacheCommitAfterReturn,
            super::FaultPosition::After,
        ) {
            Ok(true) => std::process::abort(),
            Ok(false) => {}
            Err(_) => return Err(RuntimeCacheBypass::Corrupt),
        }
        self.deadline.ensure_remaining()
    }
}

fn configure_reader(
    connection: &Connection,
    deadline: CacheDeadline,
) -> Result<(), RuntimeCacheBypass> {
    configure_common(connection, deadline, BusyPolicy::Immediate)?;
    connection
        .execute_batch("PRAGMA query_only = ON;")
        .map_err(map_sqlite_error)?;
    deadline.ensure_remaining()
}

fn configure_writer(
    connection: &Connection,
    deadline: CacheDeadline,
    created: bool,
) -> Result<(), RuntimeCacheBypass> {
    let busy_policy = if created {
        BusyPolicy::Deadline
    } else {
        BusyPolicy::Immediate
    };
    configure_common(connection, deadline, busy_policy)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")
        .map_err(map_sqlite_error)?;
    deadline.ensure_remaining()
}

fn configure_common(
    connection: &Connection,
    deadline: CacheDeadline,
    busy_policy: BusyPolicy,
) -> Result<(), RuntimeCacheBypass> {
    deadline.apply_with_busy_policy(connection, busy_policy)?;
    connection
        .load_extension_disable()
        .map_err(map_sqlite_error)?;
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_BYTES),
        (Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, 32),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 4),
        (Limit::SQLITE_LIMIT_VDBE_OP, 10_000),
        (Limit::SQLITE_LIMIT_FUNCTION_ARG, 16),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 256),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 16),
        (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 0),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        connection
            .set_limit(limit, value)
            .map_err(map_sqlite_error)?;
        deadline.ensure_remaining()?;
    }
    connection
        .execute_batch("PRAGMA trusted_schema = OFF;")
        .map_err(map_sqlite_error)?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(map_sqlite_error)?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(map_sqlite_error)?;
    deadline.ensure_remaining()
}

fn initialize_cache(
    connection: &Connection,
    deadline: CacheDeadline,
) -> Result<(), RuntimeCacheBypass> {
    deadline.apply(connection)?;
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false)
        .map_err(map_sqlite_error)?;
    let result = connection
        .execute_batch(&format!(
            "PRAGMA journal_mode = DELETE;
             BEGIN IMMEDIATE;
             {RUNTIME_CACHE_SCHEMA_SQL};
             PRAGMA application_id = {RUNTIME_CACHE_APPLICATION_ID};
             PRAGMA user_version = {RUNTIME_CACHE_SCHEMA_VERSION};
             COMMIT;"
        ))
        .map_err(map_sqlite_error);
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(map_sqlite_error)?;
    result?;
    deadline.ensure_remaining()?;
    verify_cache(connection, deadline, BusyPolicy::Deadline)
}

fn verify_cache(
    connection: &Connection,
    deadline: CacheDeadline,
    busy_policy: BusyPolicy,
) -> Result<(), RuntimeCacheBypass> {
    deadline.apply_with_busy_policy(connection, busy_policy)?;
    let application_id = connection
        .query_row("PRAGMA application_id", [], |row| row.get::<_, i32>(0))
        .map_err(map_sqlite_error)?;
    let user_version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
        .map_err(map_sqlite_error)?;
    let journal_mode = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?;
    if application_id != RUNTIME_CACHE_APPLICATION_ID
        || user_version != RUNTIME_CACHE_SCHEMA_VERSION
        || journal_mode != "delete"
    {
        return Err(RuntimeCacheBypass::Incompatible);
    }
    deadline.ensure_remaining()?;

    let objects = {
        let mut statement = connection
            .prepare(
                "SELECT type, name, tbl_name, sql
                 FROM sqlite_schema
                 WHERE name NOT LIKE 'sqlite_%'
                 ORDER BY type, name
                 LIMIT 2",
            )
            .map_err(map_sqlite_error)?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(map_sqlite_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sqlite_error)?
    };
    if objects
        != [(
            "table".to_owned(),
            "project_identity_cache".to_owned(),
            "project_identity_cache".to_owned(),
            Some(RUNTIME_CACHE_SCHEMA_SQL.to_owned()),
        )]
    {
        return Err(RuntimeCacheBypass::Incompatible);
    }
    let integrity = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get::<_, String>(0))
        .map_err(map_sqlite_error)?;
    if integrity != "ok" {
        return Err(RuntimeCacheBypass::Corrupt);
    }
    let excessive_row = connection
        .query_row(
            "SELECT 1 FROM project_identity_cache LIMIT 1 OFFSET 256",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sqlite_error)?;
    if excessive_row.is_some() {
        return Err(RuntimeCacheBypass::Corrupt);
    }
    deadline.ensure_remaining()
}

fn is_canonical_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(byte),
        })
}

fn sqlite_read_only_uri(path: &Path) -> PathBuf {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut uri = b"file:".to_vec();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(*byte);
        } else {
            uri.extend_from_slice(&[b'%', HEX[(byte >> 4) as usize], HEX[(byte & 0x0f) as usize]]);
        }
    }
    uri.extend_from_slice(b"?mode=ro");
    PathBuf::from(OsString::from_vec(uri))
}

fn map_security_error(error: SecurityError) -> RuntimeCacheBypass {
    match error {
        SecurityError::Missing => RuntimeCacheBypass::Missing,
        SecurityError::Invalid(_) => RuntimeCacheBypass::Unsafe,
        SecurityError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            RuntimeCacheBypass::Contended
        }
        SecurityError::Io(_) => RuntimeCacheBypass::Unsafe,
    }
}

fn map_creation_error(error: SecurityError) -> RuntimeCacheBypass {
    match error {
        SecurityError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            RuntimeCacheBypass::Contended
        }
        error => map_security_error(error),
    }
}

fn map_sqlite_error(error: rusqlite::Error) -> RuntimeCacheBypass {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            RuntimeCacheBypass::Contended
        }
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            RuntimeCacheBypass::Deadline
        }
        _ => RuntimeCacheBypass::Corrupt,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn reader_connection_is_query_only() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StoragePaths::at(root.path());
        drop(
            RuntimeCacheWriter::create_or_open_after_activity(
                &paths,
                CacheDeadline::after(Duration::from_millis(250)),
            )
            .unwrap(),
        );

        let reader = RuntimeCacheReader::open_existing_read_only(
            &paths,
            CacheDeadline::after(Duration::from_millis(250)),
        )
        .unwrap();
        assert_eq!(
            reader
                .connection
                .query_row("PRAGMA query_only", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            1
        );
    }
}
