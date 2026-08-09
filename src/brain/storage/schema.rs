use rusqlite::config::DbConfig;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OptionalExtension};

use super::maintenance::WAL_AUTOCHECKPOINT_PAGES;
use super::{
    BRAIN_APPLICATION_ID, BRAIN_SCHEMA_VERSION, DatabaseKind, REVIEW_APPLICATION_ID,
    REVIEW_SCHEMA_VERSION, StorageDeadline, StorageError,
};

pub(super) const BRAIN_SCHEMA_SQL: &str = include_str!("schema-v1/brain.sql");
pub(super) const REVIEW_SCHEMA_SQL: &str = include_str!("schema-v1/review.sql");

// A maximum supported decision blob shares a row with bounded typed projections.
const MAX_VALUE_BYTES: i32 = 1024 * 1024 + 64 * 1024;
const MAX_SQL_BYTES: i32 = 1024 * 1024;
const MAX_COLUMNS: i32 = 128;
const MAX_EXPRESSION_DEPTH: i32 = 100;
const MAX_COMPOUND_TERMS: i32 = 16;
const MAX_VDBE_OPS: i32 = 100_000;
const MAX_FUNCTION_ARGS: i32 = 32;
const MAX_LIKE_PATTERN_BYTES: i32 = 4_096;
const MAX_VARIABLES: i32 = 1_024;
const MAX_TRIGGER_DEPTH: i32 = 16;
// Schema v1 contains 70 objects including SQLite-generated unique indexes.
const MAX_FROZEN_SCHEMA_OBJECTS: usize = 72;

#[derive(Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SchemaObject {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

pub(super) fn configure_connection(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    before_call(connection, deadline)?;
    connection.load_extension_disable()?;
    after_call(deadline)?;
    for pragma in [
        "PRAGMA foreign_keys = ON;",
        "PRAGMA trusted_schema = OFF;",
        "PRAGMA secure_delete = ON;",
        "PRAGMA synchronous = FULL;",
    ] {
        before_call(connection, deadline)?;
        connection.execute_batch(pragma)?;
        after_call(deadline)?;
    }
    before_call(connection, deadline)?;
    connection.execute_batch(&format!(
        "PRAGMA wal_autocheckpoint = {WAL_AUTOCHECKPOINT_PAGES};"
    ))?;
    after_call(deadline)?;
    before_call(connection, deadline)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    after_call(deadline)?;
    before_call(connection, deadline)?;
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    after_call(deadline)?;
    set_limits(connection, deadline)?;
    Ok(())
}

pub(super) fn initialize_current(
    connection: &Connection,
    kind: DatabaseKind,
) -> Result<(), StorageError> {
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, false)?;
    let result = (|| {
        connection.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA journal_mode = WAL;")?;
        let (application_id, schema_version, schema_sql) = match kind {
            DatabaseKind::Brain => (BRAIN_APPLICATION_ID, BRAIN_SCHEMA_VERSION, BRAIN_SCHEMA_SQL),
            DatabaseKind::Review => (
                REVIEW_APPLICATION_ID,
                REVIEW_SCHEMA_VERSION,
                REVIEW_SCHEMA_SQL,
            ),
        };
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             {schema_sql}
             PRAGMA application_id = {application_id};
             PRAGMA user_version = {schema_version};
             COMMIT;"
        ))?;
        Ok(())
    })();
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    result
}

pub(super) fn verify_current(
    connection: &Connection,
    kind: DatabaseKind,
    deadline: StorageDeadline,
) -> Result<(), StorageError> {
    let application_id = deadline_query_i32(connection, deadline, "PRAGMA application_id")?;
    let user_version = deadline_query_i32(connection, deadline, "PRAGMA user_version")?;
    let journal_mode = deadline_query_string(connection, deadline, "PRAGMA journal_mode")?;
    let expected_application_id = kind.application_id();
    let expected_schema_version = kind.schema_version();
    if application_id != expected_application_id {
        return Err(StorageError::UnsupportedSchema {
            application_id,
            schema_version: user_version,
        });
    }
    if user_version < expected_schema_version {
        return Err(StorageError::MigrationRequired);
    }
    if user_version > expected_schema_version || journal_mode != "wal" {
        return Err(StorageError::UnsupportedSchema {
            application_id,
            schema_version: user_version,
        });
    }
    verify_frozen_schema(connection, kind, deadline)?;
    match kind {
        DatabaseKind::Brain => verify_brain_meta(connection, deadline),
        DatabaseKind::Review => verify_review_meta(connection, deadline),
    }
}

pub(super) fn verify_frozen_schema(
    connection: &Connection,
    kind: DatabaseKind,
    deadline: StorageDeadline,
) -> Result<(), StorageError> {
    deadline.ensure_remaining()?;
    let reference = Connection::open_in_memory()?;
    deadline.ensure_remaining()?;
    deadline.apply(&reference)?;
    reference.execute_batch(match kind {
        DatabaseKind::Brain => BRAIN_SCHEMA_SQL,
        DatabaseKind::Review => REVIEW_SCHEMA_SQL,
    })?;
    deadline.ensure_remaining()?;

    let expected = schema_objects(&reference, MAX_FROZEN_SCHEMA_OBJECTS + 1, deadline)?;
    if expected.len() > MAX_FROZEN_SCHEMA_OBJECTS {
        return Err(StorageError::InvalidStorage(
            "frozen schema exceeds its object bound",
        ));
    }
    let actual = schema_objects(connection, expected.len() + 1, deadline)?;
    if actual != expected {
        return Err(StorageError::InvalidStorage(
            "database schema does not match the frozen version",
        ));
    }
    Ok(())
}

fn schema_objects(
    connection: &Connection,
    limit: usize,
    deadline: StorageDeadline,
) -> Result<Vec<SchemaObject>, StorageError> {
    deadline.apply(connection)?;
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql
         FROM main.sqlite_schema
         LIMIT ?1",
    )?;
    let mut rows = statement.query([limit as i64])?;
    let mut objects = Vec::with_capacity(limit);
    while let Some(row) = rows.next()? {
        objects.push(SchemaObject {
            object_type: row.get(0)?,
            name: row.get(1)?,
            table_name: row.get(2)?,
            sql: row.get(3)?,
        });
        deadline.ensure_remaining()?;
    }
    deadline.ensure_remaining()?;
    objects.sort_unstable();
    Ok(objects)
}

fn verify_brain_meta(
    connection: &Connection,
    deadline: StorageDeadline,
) -> Result<(), StorageError> {
    deadline.apply(connection)?;
    let meta = connection
        .query_row(
            "SELECT application_id, schema_version, schema_generation,
                    migration_state, erasure_state
             FROM schema_meta WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i32>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    deadline.ensure_remaining()?;
    let Some((application_id, version, generation, migration, erasure)) = meta else {
        return Err(StorageError::MigrationRequired);
    };
    if application_id != BRAIN_APPLICATION_ID
        || version != BRAIN_SCHEMA_VERSION
        || generation != BRAIN_SCHEMA_VERSION
    {
        return Err(StorageError::UnsupportedSchema {
            application_id,
            schema_version: version,
        });
    }
    if migration != "complete" || !matches!(erasure.as_str(), "complete" | "in_progress") {
        return Err(StorageError::MigrationRequired);
    }
    deadline.apply(connection)?;
    let rows = connection.query_row("SELECT count(*) FROM schema_meta", [], |row| {
        row.get::<_, i64>(0)
    })?;
    deadline.ensure_remaining()?;
    if rows != 1 {
        return Err(StorageError::InvalidStorage(
            "Brain schema metadata is not a singleton",
        ));
    }
    Ok(())
}

fn verify_review_meta(
    connection: &Connection,
    deadline: StorageDeadline,
) -> Result<(), StorageError> {
    deadline.apply(connection)?;
    let valid_rows = connection.query_row(
        "SELECT count(*) FROM review_meta
         WHERE surface IN ('attention', 'review', 'diagnostics', 'recent')",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    deadline.ensure_remaining()?;
    if valid_rows != 4 {
        return Err(StorageError::MigrationRequired);
    }
    Ok(())
}

fn deadline_query_i32(
    connection: &Connection,
    deadline: StorageDeadline,
    sql: &str,
) -> Result<i32, StorageError> {
    deadline.apply(connection)?;
    let value = connection.query_row(sql, [], |row| row.get(0))?;
    deadline.ensure_remaining()?;
    Ok(value)
}

fn deadline_query_string(
    connection: &Connection,
    deadline: StorageDeadline,
    sql: &str,
) -> Result<String, StorageError> {
    deadline.apply(connection)?;
    let value = connection.query_row(sql, [], |row| row.get(0))?;
    deadline.ensure_remaining()?;
    Ok(value)
}

fn set_limits(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, MAX_VALUE_BYTES),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, MAX_SQL_BYTES),
        (Limit::SQLITE_LIMIT_COLUMN, MAX_COLUMNS),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, MAX_EXPRESSION_DEPTH),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, MAX_COMPOUND_TERMS),
        (Limit::SQLITE_LIMIT_VDBE_OP, MAX_VDBE_OPS),
        (Limit::SQLITE_LIMIT_FUNCTION_ARG, MAX_FUNCTION_ARGS),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (
            Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH,
            MAX_LIKE_PATTERN_BYTES,
        ),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, MAX_VARIABLES),
        (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, MAX_TRIGGER_DEPTH),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        before_call(connection, deadline)?;
        connection.set_limit(limit, value)?;
        after_call(deadline)?;
    }
    Ok(())
}

fn before_call(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    if let Some(deadline) = deadline {
        deadline.apply(connection)?;
    }
    Ok(())
}

fn after_call(deadline: Option<StorageDeadline>) -> Result<(), StorageError> {
    if let Some(deadline) = deadline {
        deadline.ensure_remaining()?;
    }
    Ok(())
}
