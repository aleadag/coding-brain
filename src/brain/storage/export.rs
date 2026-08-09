use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use coding_brain_core::brain_activity::MAX_ACTIVITY_EVENT_BYTES;
use coding_brain_core::lifecycle::MAX_SNAPSHOT_BYTES;
use coding_brain_core::review_state::{MAX_REVIEW_KEYS, MAX_REVIEW_STATE_BYTES};
use rusqlite::{Transaction, params};
use serde_json::{Map, Value};

use super::{
    ActivityCursor, BrainDb, LegacySourceSet, OpenRole, ReviewDb, StorageDeadline, StorageError,
    StoragePaths,
};

const AUDIT_MANIFEST: &[u8] = b"{\"format\":\"coding-brain-audit-v1\",\"executable\":false}\n";
const DECISION_LIMIT: usize = crate::brain::decisions::MAX_DECISION_RECORD_BYTES as usize;
const EXPORT_PAGE_ROWS: usize = 256;
const EXPORT_PAGE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct AuditExporter {
    paths: StoragePaths,
}

impl AuditExporter {
    pub fn new(paths: &StoragePaths) -> Self {
        Self {
            paths: paths.clone(),
        }
    }

    pub fn export(&self, output: &Path) -> Result<(), StorageError> {
        self.export_with_seams(output, |_| Ok(()), |_| Ok(()))
            .map_err(|error| {
                super::maintenance::map_storage_error(super::StorageOperation::Export, false, error)
            })
    }

    fn export_with_seams(
        &self,
        output: &Path,
        after_bounds: impl FnOnce(&BrainExportBounds) -> Result<(), StorageError>,
        before_publication: impl FnOnce(&Path) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        with_brain_export_bounds(&self.paths, false, after_bounds, |bounds| {
            let directory = NewExportDirectory::create(output)?;
            let prepared = (|| {
                directory.publish("decisions.jsonl", |file| {
                    stream_audit_decisions(&self.paths, bounds, file)
                })?;
                directory.publish("activity.jsonl", |file| {
                    stream_activity(&self.paths, bounds, file)
                })?;
                before_publication(&directory.root)?;
                directory.validate_path_correspondence()?;
                require_same_erasure_generation(&self.paths, bounds.erasure_generation)?;
                directory.publish("manifest.json", |file| {
                    file.write_all(AUDIT_MANIFEST)?;
                    Ok(())
                })?;
                directory.sync()
            })();
            if let Err(error) = prepared {
                directory.cleanup_owned()?;
                return Err(error);
            }
            Ok(())
        })
    }
}

#[derive(Clone, Debug)]
pub struct LegacyExporter {
    paths: StoragePaths,
}

impl LegacyExporter {
    pub fn new(paths: &StoragePaths) -> Self {
        Self {
            paths: paths.clone(),
        }
    }

    pub fn export(&self, output: &Path) -> Result<(), StorageError> {
        self.export_with_before_validation(output, |_| Ok(()))
            .map_err(|error| {
                super::maintenance::map_storage_error(super::StorageOperation::Export, false, error)
            })
    }

    fn export_with_before_validation(
        &self,
        output: &Path,
        before_validation: impl FnOnce(&Path) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        with_brain_export_bounds(
            &self.paths,
            true,
            |_| Ok(()),
            |bounds| {
                let lifecycle = bounds
                    .lifecycle
                    .as_deref()
                    .ok_or_else(|| invalid("legacy lifecycle snapshot is absent"))?;
                let review = legacy_review_bytes(&self.paths)?;

                let directory = NewExportDirectory::create(output)?;
                let prepared = (|| {
                    let staging = directory.create_owned_subdirectory(".legacy-stage")?;
                    staging.create_owned_subdirectory("brain")?;
                    staging.create_owned_subdirectory("hooks")?;
                    staging.publish("brain/decisions.jsonl", |file| {
                        stream_legacy_decisions(&self.paths, bounds, file)
                    })?;
                    staging.publish("activity.jsonl", |file| {
                        stream_activity(&self.paths, bounds, file)
                    })?;
                    staging.publish("hooks/lifecycle.json", |file| {
                        file.write_all(lifecycle)?;
                        Ok(())
                    })?;
                    if let Some(review) = &review {
                        staging.publish("review-state.json", |file| {
                            file.write_all(review)?;
                            Ok(())
                        })?;
                    }
                    staging.sync()?;
                    before_validation(&staging.root)?;
                    directory.validate_path_correspondence()?;
                    staging.validate_path_correspondence()?;
                    LegacySourceSet::from_descriptor(&staging.descriptor)?.read_all_bounded()?;
                    directory.validate_path_correspondence()?;
                    staging.validate_path_correspondence()?;
                    Ok(staging)
                })();
                let staging = match prepared {
                    Ok(staging) => staging,
                    Err(error) => {
                        directory.cleanup_owned()?;
                        return Err(error);
                    }
                };
                let published = (|| {
                    require_same_erasure_generation(&self.paths, bounds.erasure_generation)?;
                    directory.publish_staged_profile(&staging, review.is_some())?;
                    directory.sync()
                })();
                if let Err(error) = published {
                    directory.cleanup_owned()?;
                    return Err(error);
                }
                Ok(())
            },
        )
    }
}

fn open_brain(paths: &StoragePaths) -> Result<BrainDb, StorageError> {
    BrainDb::open_current(
        paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(5)),
    )
}

#[derive(Debug)]
struct BrainExportBounds {
    erasure_generation: u64,
    decision_rowid: i64,
    decision_count: i64,
    activity_high_water: i64,
    activity_count: i64,
    lifecycle: Option<Vec<u8>>,
}

fn with_brain_export_bounds<T>(
    paths: &StoragePaths,
    include_legacy: bool,
    after_bounds: impl FnOnce(&BrainExportBounds) -> Result<(), StorageError>,
    export: impl FnOnce(&BrainExportBounds) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let brain = open_brain(paths)?;
    let erasure_gate = brain.learning_read_session()?;
    let transaction = brain.connection.unchecked_transaction()?;
    let bounds = capture_export_bounds(&brain, &transaction, include_legacy)?;
    commit_export_snapshot(transaction)?;
    after_bounds(&bounds)?;
    let result = export(&bounds)?;
    drop(erasure_gate);
    Ok(result)
}

fn capture_export_bounds(
    brain: &BrainDb,
    transaction: &Transaction<'_>,
    include_legacy: bool,
) -> Result<BrainExportBounds, StorageError> {
    let erasure = erasure_state_in(transaction)?;
    if !erasure.complete {
        return Err(invalid("privacy erasure is incomplete"));
    }
    if include_legacy {
        reject_lossy_authority(transaction)?;
    }
    let decision_rowid = transaction.query_row(
        "SELECT coalesce(max(rowid), 0) FROM decision_payloads",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let activity_high_water = super::activity::validated_high_water(transaction)?;
    let decision_count = transaction.query_row(
        "SELECT count(*) FROM decision_payloads WHERE rowid <= ?1",
        [decision_rowid],
        |row| row.get::<_, i64>(0),
    )?;
    let activity_count = transaction.query_row(
        "SELECT count(*) FROM activity_events WHERE source_cursor <= ?1",
        [activity_high_water],
        |row| row.get::<_, i64>(0),
    )?;
    let lifecycle = include_legacy
        .then(|| {
            let bytes = serde_json::to_vec(&super::lifecycle::load_lifecycle_snapshot(
                transaction,
                brain.deadline,
            )?)
            .map_err(|_| invalid("legacy lifecycle cannot be serialized"))?;
            if bytes.len() > MAX_SNAPSHOT_BYTES {
                return Err(invalid("legacy lifecycle exceeds its frozen size limit"));
            }
            Ok(bytes)
        })
        .transpose()?;
    Ok(BrainExportBounds {
        erasure_generation: erasure.generation,
        decision_rowid,
        decision_count,
        activity_high_water,
        activity_count,
        lifecycle,
    })
}

fn require_same_erasure_generation(
    paths: &StoragePaths,
    expected_generation: u64,
) -> Result<(), StorageError> {
    let brain = open_brain(paths)?;
    let transaction = brain.connection.unchecked_transaction()?;
    let erasure = erasure_state_in(&transaction)?;
    if !erasure.complete || erasure.generation != expected_generation {
        return Err(invalid("privacy erasure changed during export"));
    }
    commit_export_snapshot(transaction)?;
    Ok(())
}

fn erasure_state_in(transaction: &Transaction<'_>) -> Result<super::ErasureState, StorageError> {
    let (state, generation): (String, i64) = transaction.query_row(
        "SELECT erasure_state, erasure_generation FROM schema_meta WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let generation =
        u64::try_from(generation).map_err(|_| invalid("erasure generation is invalid"))?;
    let complete = match state.as_str() {
        "complete" => true,
        "in_progress" => false,
        _ => return Err(invalid("erasure state is invalid")),
    };
    Ok(super::ErasureState {
        generation,
        complete,
    })
}

fn reject_lossy_authority(transaction: &Transaction<'_>) -> Result<(), StorageError> {
    let live = transaction.query_row("SELECT count(*) FROM permission_commits", [], |row| {
        row.get::<_, i64>(0)
    })?;
    if live != 0 {
        return Err(invalid(
            "live permission authority has no exact frozen representation",
        ));
    }
    let correlated = transaction.query_row(
        "SELECT count(*) FROM historical_permission_authority
         WHERE provenance_kind != 'proposal_terminal'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if correlated != 0 {
        return Err(invalid(
            "correlated historical authority has no exact frozen representation",
        ));
    }
    let unrepresented = transaction.query_row(
        "SELECT count(*) FROM decision_identities AS identity
         WHERE identity.identity_kind = 'permission'
           AND NOT EXISTS (
               SELECT 1 FROM historical_permission_authority AS historical
               WHERE historical.decision_id = identity.decision_id
           )",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if unrepresented != 0 {
        return Err(invalid(
            "permission decision has no exact frozen authority representation",
        ));
    }
    Ok(())
}

struct ExportPage {
    lines: Vec<Vec<u8>>,
    next_cursor: Option<ActivityCursor>,
}

fn stream_audit_decisions(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    file: &mut File,
) -> Result<(), StorageError> {
    stream_decisions(paths, bounds, file, false)
}

fn stream_legacy_decisions(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    file: &mut File,
) -> Result<(), StorageError> {
    stream_decisions(paths, bounds, file, true)
}

fn stream_decisions(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    file: &mut File,
    legacy: bool,
) -> Result<(), StorageError> {
    let mut after = None;
    let mut count = 0_i64;
    loop {
        let page = load_decision_page(paths, bounds, after, legacy)?;
        count = checked_export_count(
            count,
            page.lines.len(),
            "decision export row count is invalid",
        )?;
        write_lines(file, &page.lines)?;
        let Some(next) = page.next_cursor else {
            break;
        };
        after = Some(next);
    }
    if count != bounds.decision_count {
        return Err(invalid("decision export contains invalid typed rows"));
    }
    Ok(())
}

fn load_decision_page(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    after: Option<ActivityCursor>,
    legacy: bool,
) -> Result<ExportPage, StorageError> {
    let brain = open_brain(paths)?;
    let transaction = brain.connection.unchecked_transaction()?;
    let typed = brain.learning_decisions_bounded_after_locked(
        after,
        bounds.decision_rowid,
        EXPORT_PAGE_ROWS,
        EXPORT_PAGE_BYTES,
    )?;
    let Some(last) = typed
        .decisions
        .last()
        .map(|decision| decision.source_cursor)
    else {
        commit_export_snapshot(transaction)?;
        return Ok(ExportPage {
            lines: Vec::new(),
            next_cursor: None,
        });
    };
    let lines = if legacy {
        load_legacy_decision_lines(&transaction, bounds, after, last, &typed.decisions)?
    } else {
        load_audit_decision_lines(&transaction, bounds, after, last, &typed.decisions)?
    };
    if lines.len() != typed.decisions.len() {
        return Err(invalid("decision export contains invalid typed rows"));
    }
    let next_cursor = typed.next_cursor;
    commit_export_snapshot(transaction)?;
    Ok(ExportPage { lines, next_cursor })
}

fn load_audit_decision_lines(
    transaction: &Transaction<'_>,
    bounds: &BrainExportBounds,
    after: Option<ActivityCursor>,
    last: ActivityCursor,
    typed: &[super::DecisionPayload],
) -> Result<Vec<Vec<u8>>, StorageError> {
    let mut statement = transaction.prepare(
        "SELECT decision_record FROM decision_payloads
         WHERE rowid <= ?1 AND source_cursor > ?2 AND source_cursor <= ?3
         ORDER BY source_cursor ASC, decision_id ASC",
    )?;
    let mut rows = statement.query(params![
        bounds.decision_rowid,
        cursor_value(after),
        cursor_value(Some(last)),
    ])?;
    let mut lines = Vec::new();
    while let Some(row) = rows.next()? {
        let bytes = row.get::<_, Vec<u8>>(0)?;
        let decision = typed
            .get(lines.len())
            .ok_or_else(|| invalid("audit decision has no typed row"))?;
        validate_canonical_value(
            &bytes,
            validate_json_row(
                &super::decisions::serialize_record(&decision.record)?,
                DECISION_LIMIT,
                "audit decision is invalid",
            )?,
            DECISION_LIMIT,
            "audit decision is invalid",
        )?;
        lines.push(bytes);
    }
    Ok(lines)
}

fn stream_activity(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    file: &mut File,
) -> Result<(), StorageError> {
    let mut after = None;
    let mut count = 0_i64;
    loop {
        let page = load_activity_page(paths, bounds, after)?;
        count = checked_export_count(
            count,
            page.lines.len(),
            "activity export row count is invalid",
        )?;
        write_lines(file, &page.lines)?;
        let Some(next) = page.next_cursor else {
            break;
        };
        after = Some(next);
    }
    if count != bounds.activity_count {
        return Err(invalid("activity export contains invalid typed rows"));
    }
    Ok(())
}

fn load_activity_page(
    paths: &StoragePaths,
    bounds: &BrainExportBounds,
    after: Option<ActivityCursor>,
) -> Result<ExportPage, StorageError> {
    let brain = open_brain(paths)?;
    let transaction = brain.connection.unchecked_transaction()?;
    let typed = brain.activity_bounded_after_locked(
        after,
        bounds.activity_high_water,
        EXPORT_PAGE_ROWS,
        EXPORT_PAGE_BYTES,
    )?;
    let Some(last) = typed.events.last().map(|event| event.cursor) else {
        commit_export_snapshot(transaction)?;
        return Ok(ExportPage {
            lines: Vec::new(),
            next_cursor: None,
        });
    };
    let lines = {
        let mut statement = transaction.prepare(
            "SELECT event_payload FROM activity_events
             WHERE source_cursor > ?1 AND source_cursor <= ?2 AND source_cursor <= ?3
             ORDER BY source_cursor ASC",
        )?;
        let mut rows = statement.query(params![
            cursor_value(after),
            cursor_value(Some(last)),
            bounds.activity_high_water,
        ])?;
        let mut lines = Vec::new();
        while let Some(row) = rows.next()? {
            let bytes = row.get::<_, Vec<u8>>(0)?;
            let activity = typed
                .events
                .get(lines.len())
                .ok_or_else(|| invalid("activity export has no typed row"))?;
            validate_canonical_value(
                &bytes,
                serde_json::to_value(&activity.event)
                    .map_err(|_| invalid("activity export is invalid"))?,
                MAX_ACTIVITY_EVENT_BYTES,
                "activity export is invalid",
            )?;
            lines.push(bytes);
        }
        lines
    };
    if lines.len() != typed.events.len() {
        return Err(invalid("activity export contains invalid typed rows"));
    }
    let next_cursor = typed.next_cursor;
    commit_export_snapshot(transaction)?;
    Ok(ExportPage { lines, next_cursor })
}

fn checked_export_count(
    current: i64,
    additional: usize,
    reason: &'static str,
) -> Result<i64, StorageError> {
    current
        .checked_add(i64::try_from(additional).map_err(|_| invalid(reason))?)
        .ok_or_else(|| invalid(reason))
}

fn cursor_value(cursor: Option<ActivityCursor>) -> i64 {
    cursor.map_or(0, |cursor| cursor.get() as i64)
}

fn write_lines(file: &mut File, lines: &[Vec<u8>]) -> Result<(), StorageError> {
    for line in lines {
        file.write_all(line)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn validate_json_row(
    bytes: &[u8],
    maximum: usize,
    reason: &'static str,
) -> Result<Value, StorageError> {
    if bytes.is_empty() || bytes.len() > maximum || bytes.contains(&b'\n') || bytes.contains(&b'\r')
    {
        return Err(invalid(reason));
    }
    serde_json::from_slice(bytes).map_err(|_| invalid(reason))
}

fn validate_canonical_value(
    bytes: &[u8],
    canonical: Value,
    maximum: usize,
    reason: &'static str,
) -> Result<Value, StorageError> {
    let raw = validate_json_row(bytes, maximum, reason)?;
    if raw != canonical {
        return Err(invalid(reason));
    }
    Ok(raw)
}

fn load_legacy_decision_lines(
    transaction: &Transaction<'_>,
    bounds: &BrainExportBounds,
    after: Option<ActivityCursor>,
    last: ActivityCursor,
    typed: &[super::DecisionPayload],
) -> Result<Vec<Vec<u8>>, StorageError> {
    let mut statement = transaction.prepare(
        "SELECT payload.decision_record, identity.identity_kind, identity.provider,
                identity.session_id, identity.turn_id, identity.decision_source,
                activity.event_payload
         FROM decision_payloads AS payload
         JOIN decision_identities AS identity USING (decision_id)
         JOIN activity_events AS activity ON activity.source_cursor = payload.source_cursor
         WHERE payload.rowid <= ?1 AND payload.source_cursor > ?2
               AND payload.source_cursor <= ?3
         ORDER BY payload.source_cursor ASC, payload.decision_id ASC",
    )?;
    let mut rows = statement.query(params![
        bounds.decision_rowid,
        cursor_value(after),
        cursor_value(Some(last)),
    ])?;
    let mut lines = Vec::new();
    while let Some(row) = rows.next()? {
        let stored = row.get::<_, Vec<u8>>(0)?;
        let decision = typed
            .get(lines.len())
            .ok_or_else(|| invalid("legacy decision has no typed row"))?;
        validate_canonical_value(
            &stored,
            validate_json_row(
                &super::decisions::serialize_record(&decision.record)?,
                DECISION_LIMIT,
                "legacy decision is invalid",
            )?,
            DECISION_LIMIT,
            "legacy decision is invalid",
        )?;
        let kind = row.get::<_, String>(1)?;
        let line = match kind.as_str() {
            "observation" => {
                validate_json_row(&stored, DECISION_LIMIT, "legacy audit decision is invalid")?;
                stored
            }
            "permission" => reconstruct_hook_decision(
                &stored,
                &row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?.as_deref(),
                row.get::<_, Option<String>>(4)?.as_deref(),
                row.get::<_, Option<String>>(5)?.as_deref(),
                &row.get::<_, Vec<u8>>(6)?,
            )?,
            _ => return Err(invalid("decision identity kind is invalid")),
        };
        if line.len() > DECISION_LIMIT {
            return Err(invalid("legacy decision exceeds its frozen size limit"));
        }
        lines.push(line);
    }
    Ok(lines)
}

fn reconstruct_hook_decision(
    stored: &[u8],
    provider: &str,
    session_id: Option<&str>,
    turn_id: Option<&str>,
    decision_source: Option<&str>,
    activity: &[u8],
) -> Result<Vec<u8>, StorageError> {
    let record = validate_json_row(
        stored,
        DECISION_LIMIT,
        "legacy permission decision is invalid",
    )?;
    let record = record
        .as_object()
        .ok_or_else(|| invalid("legacy permission decision is not an object"))?;
    let activity = validate_json_row(
        activity,
        MAX_ACTIVITY_EVENT_BYTES,
        "legacy permission activity is invalid",
    )?;
    let source = match decision_source {
        Some("model") => "model",
        Some("deterministic_safety") => "deterministic",
        Some("native_provider") => "provider_policy",
        _ => {
            return Err(invalid(
                "permission decision source is not frozen-representable",
            ));
        }
    };
    let mut hook = Map::new();
    hook.insert("provider".into(), Value::String(provider.to_owned()));
    for name in [
        "ts",
        "pid",
        "project",
        "tool",
        "command",
        "brain_action",
        "brain_confidence",
        "brain_reasoning",
        "user_action",
        "decision_type",
        "suggested_at",
        "resolved_at",
        "decision_id",
    ] {
        let value = record
            .get(name)
            .filter(|value| !value.is_null())
            .cloned()
            .ok_or_else(|| invalid("permission decision lost a frozen field"))?;
        hook.insert(name.to_owned(), value);
    }
    hook.insert("brain_source".into(), Value::String(source.to_owned()));
    hook.insert(
        "brain_threshold".into(),
        activity.get("threshold").cloned().unwrap_or(Value::Null),
    );
    hook.insert(
        "session_id".into(),
        Value::String(
            session_id
                .ok_or_else(|| invalid("permission session is missing"))?
                .to_owned(),
        ),
    );
    hook.insert(
        "turn_id".into(),
        Value::String(
            turn_id
                .ok_or_else(|| invalid("permission turn is missing"))?
                .to_owned(),
        ),
    );
    serde_json::to_vec(&hook)
        .map_err(|_| invalid("legacy permission decision cannot be serialized"))
}

fn legacy_review_bytes(paths: &StoragePaths) -> Result<Option<Vec<u8>>, StorageError> {
    if fs::symlink_metadata(paths.review_db())
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
    {
        return Ok(None);
    }
    let review =
        ReviewDb::open_frozen_for_export(paths, StorageDeadline::after(Duration::from_secs(5)))?;
    let transaction = review.connection.unchecked_transaction()?;
    let mut surfaces = Map::new();
    let mut meta = transaction.prepare(
        "SELECT surface, revision, last_archive_revision FROM review_meta ORDER BY surface",
    )?;
    let mut meta_rows = meta.query([])?;
    while let Some(row) = meta_rows.next()? {
        let surface = row.get::<_, String>(0)?;
        let revision = row.get::<_, i64>(1)?;
        let last_archive_revision = row.get::<_, Option<i64>>(2)?;
        if revision < 0 {
            return Err(invalid("review revision is invalid"));
        }
        let mut items = Map::new();
        let mut last_archive = Vec::new();
        let mut marks = transaction.prepare(
            "SELECT group_id, disposition, revision FROM review_marks
             WHERE surface = ?1 ORDER BY group_id, source_cursor",
        )?;
        let mut mark_rows = marks.query([&surface])?;
        let mut count = 0usize;
        while let Some(mark) = mark_rows.next()? {
            count += 1;
            if count > MAX_REVIEW_KEYS {
                return Err(invalid("review export exceeds its frozen key limit"));
            }
            let key = mark.get::<_, String>(0)?;
            validate_review_key(&key)?;
            let disposition = mark.get::<_, String>(1)?;
            if !matches!(disposition.as_str(), "reviewed" | "archived")
                || (surface == "recent" && disposition == "archived")
            {
                return Err(invalid("review disposition is not frozen-representable"));
            }
            let row_revision = mark.get::<_, i64>(2)?;
            if row_revision <= 0 || row_revision > revision {
                return Err(invalid("review mark revision is invalid"));
            }
            if disposition == "archived" && last_archive_revision == Some(row_revision) {
                last_archive.push(Value::String(key.clone()));
            }
            if items.insert(key, Value::String(disposition)).is_some() {
                return Err(invalid("review export contains duplicate keys"));
            }
        }
        if revision == 0 && !items.is_empty() {
            return Err(invalid("review state is not frozen-representable"));
        }
        if revision != 0 || !items.is_empty() {
            let mut value = Map::new();
            value.insert("revision".into(), Value::from(revision));
            if !last_archive.is_empty() {
                value.insert("last_archive".into(), Value::Array(last_archive));
            }
            if !items.is_empty() {
                value.insert("items".into(), Value::Object(items));
            }
            surfaces.insert(surface, Value::Object(value));
        }
    }
    drop(meta_rows);
    drop(meta);
    commit_export_snapshot(transaction)?;
    let mut state = Map::new();
    state.insert("schema_version".into(), Value::from(1));
    state.insert("surfaces".into(), Value::Object(surfaces));
    let bytes =
        serde_json::to_vec(&state).map_err(|_| invalid("review state cannot be serialized"))?;
    if bytes.len() > MAX_REVIEW_STATE_BYTES {
        return Err(invalid("review state exceeds its frozen size limit"));
    }
    Ok(Some(bytes))
}

fn validate_review_key(key: &str) -> Result<(), StorageError> {
    if key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(invalid("review key is invalid"))
    }
}

struct NewExportDirectory {
    root: PathBuf,
    parent: File,
    name: CString,
    descriptor: File,
}

impl NewExportDirectory {
    fn create(root: &Path) -> Result<Self, StorageError> {
        let parent_path = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = cstring_component(
            root.file_name()
                .ok_or_else(|| invalid("export path has no final component"))?,
        )?;
        let parent = open_directory(parent_path)?;
        let temporary_name = random_export_name("create")?;
        let descriptor = create_directory_at(&parent, &temporary_name)?;
        if let Err(error) = rename_noreplace_between(&parent, &temporary_name, &parent, &name) {
            let _ = unlink_at(&parent, &temporary_name, libc::AT_REMOVEDIR);
            return Err(error);
        }
        parent.sync_all()?;
        let directory = Self {
            root: root.to_owned(),
            parent,
            name,
            descriptor,
        };
        directory.validate_path_correspondence()?;
        if !directory_entry_names(&directory.descriptor)?.is_empty() {
            return Err(invalid("new export directory is not empty"));
        }
        Ok(directory)
    }

    fn create_owned_subdirectory(&self, relative: &str) -> Result<Self, StorageError> {
        let name = single_relative_component(relative)?;
        let path = self.root.join(relative);
        let directory = Self {
            root: path,
            parent: self.descriptor.try_clone()?,
            descriptor: create_directory_at(&self.descriptor, &name)?,
            name,
        };
        directory.validate_path_correspondence()?;
        Ok(directory)
    }

    fn publish(
        &self,
        relative: &str,
        write: impl FnOnce(&mut File) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let (parent, file_name) = self.open_relative_parent(relative)?;
        let temporary = CString::new(format!(
            ".{}.tmp",
            file_name
                .to_str()
                .map_err(|_| invalid("export filename is invalid"))?
        ))
        .map_err(|_| invalid("export temporary filename is invalid"))?;
        let mut file = create_regular_at(&parent, &temporary)?;
        if let Err(error) = write(&mut file).and_then(|_| {
            file.flush()?;
            file.sync_all()?;
            Ok(())
        }) {
            let _ = unlink_at(&parent, &temporary, 0);
            return Err(error);
        }
        validate_file_at(&parent, &temporary, &file)?;
        if unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                file_name.as_ptr(),
                0,
            )
        } != 0
        {
            let _ = unlink_at(&parent, &temporary, 0);
            return Err(io::Error::last_os_error().into());
        }
        unlink_at(&parent, &temporary, 0)?;
        validate_file_at(&parent, &file_name, &file)?;
        parent.sync_all()?;
        Ok(())
    }

    fn open_relative_parent(&self, relative: &str) -> Result<(File, CString), StorageError> {
        let components = relative_components(relative)?;
        let (file_name, parents) = components
            .split_last()
            .ok_or_else(|| invalid("export filename is absent"))?;
        let mut directory = self.descriptor.try_clone()?;
        for name in parents {
            directory = open_directory_at(&directory, name)?;
        }
        Ok((directory, file_name.clone()))
    }

    fn validate_path_correspondence(&self) -> Result<(), StorageError> {
        let at_path = metadata_at(&self.parent, &self.name)?;
        let opened = ExportMetadata::from(&self.descriptor.metadata()?);
        validate_private_directory(&at_path)?;
        validate_private_directory(&opened)?;
        if at_path.device != opened.device || at_path.inode != opened.inode {
            return Err(invalid("export directory no longer matches its path"));
        }
        Ok(())
    }

    fn sync(&self) -> Result<(), StorageError> {
        self.validate_path_correspondence()?;
        self.descriptor.sync_all()?;
        Ok(())
    }

    fn publish_staged_profile(
        &self,
        staging: &Self,
        includes_review: bool,
    ) -> Result<(), StorageError> {
        if staging.name.as_bytes() != b".legacy-stage"
            || !same_file(&staging.parent, &self.descriptor)?
        {
            return Err(invalid("legacy staging directory is invalid"));
        }
        let mut entries = vec!["brain", "hooks", "activity.jsonl"];
        if includes_review {
            entries.push("review-state.json");
        }
        for relative in entries {
            let name = single_relative_component(relative)?;
            rename_noreplace_between(&staging.descriptor, &name, &self.descriptor, &name)?;
            staging.descriptor.sync_all()?;
            self.descriptor.sync_all()?;
        }
        unlink_at(&self.descriptor, &staging.name, libc::AT_REMOVEDIR)?;
        self.descriptor.sync_all()?;
        Ok(())
    }

    fn cleanup_owned(&self) -> Result<(), StorageError> {
        let quarantine_name = random_export_name("cleanup")?;
        let quarantine = create_directory_at(&self.parent, &quarantine_name)?;
        let claim = c"root";
        if unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                quarantine.as_raw_fd(),
                claim.as_ptr(),
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            remove_empty_quarantine(&self.parent, &quarantine_name, &quarantine)?;
            return Err(error.into());
        }
        let claimed = metadata_at(&quarantine, claim)?;
        let opened = ExportMetadata::from(&self.descriptor.metadata()?);
        if claimed.device != opened.device || claimed.inode != opened.inode {
            rename_noreplace_between(&quarantine, claim, &self.parent, &self.name)?;
            remove_all_entries(&self.descriptor)?;
            remove_empty_quarantine(&self.parent, &quarantine_name, &quarantine)?;
            return Err(invalid("export directory changed before cleanup"));
        }
        remove_all_entries(&self.descriptor)?;
        unlink_at(&quarantine, claim, libc::AT_REMOVEDIR)?;
        quarantine.sync_all()?;
        remove_empty_quarantine(&self.parent, &quarantine_name, &quarantine)?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct ExportMetadata {
    mode: u32,
    uid: u32,
    links: u64,
    device: u64,
    inode: u64,
}

impl From<&fs::Metadata> for ExportMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            links: metadata.nlink(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn cstring_component(name: &OsStr) -> Result<CString, StorageError> {
    CString::new(name.as_bytes()).map_err(|_| invalid("export path component is invalid"))
}

fn relative_components(relative: &str) -> Result<Vec<CString>, StorageError> {
    Path::new(relative)
        .components()
        .map(|component| match component {
            Component::Normal(name) => cstring_component(name),
            _ => Err(invalid("export relative path is invalid")),
        })
        .collect()
}

fn single_relative_component(relative: &str) -> Result<CString, StorageError> {
    let mut components = relative_components(relative)?;
    if components.len() != 1 {
        return Err(invalid("export entry name is invalid"));
    }
    Ok(components.remove(0))
}

fn open_directory(path: &Path) -> Result<File, StorageError> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?)
}

fn open_directory_at(parent: &File, name: &CStr) -> Result<File, StorageError> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    validate_private_directory(&ExportMetadata::from(&directory.metadata()?))?;
    Ok(directory)
}

fn create_directory_at(parent: &File, name: &CStr) -> Result<File, StorageError> {
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let directory = open_directory_at(parent, name)?;
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let at_path = metadata_at(parent, name)?;
    let opened = ExportMetadata::from(&directory.metadata()?);
    validate_private_directory(&at_path)?;
    validate_private_directory(&opened)?;
    if at_path.device != opened.device || at_path.inode != opened.inode {
        return Err(invalid("created export directory changed during open"));
    }
    Ok(directory)
}

fn create_regular_at(parent: &File, name: &CStr) -> Result<File, StorageError> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    validate_file_at(parent, name, &file)?;
    Ok(file)
}

#[allow(clippy::unnecessary_cast)] // libc metadata types vary across Unix targets.
fn validate_file_at(parent: &File, name: &CStr, file: &File) -> Result<(), StorageError> {
    let at_path = metadata_at(parent, name)?;
    let opened = ExportMetadata::from(&file.metadata()?);
    if at_path.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || opened.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || at_path.uid != unsafe { libc::geteuid() }
        || opened.uid != unsafe { libc::geteuid() }
        || at_path.mode & 0o777 != 0o600
        || opened.mode & 0o777 != 0o600
        || at_path.links != 1
        || opened.links != 1
        || at_path.device != opened.device
        || at_path.inode != opened.inode
    {
        return Err(invalid("export file identity is invalid"));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary across Unix targets.
fn validate_private_directory(metadata: &ExportMetadata) -> Result<(), StorageError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.mode & 0o777 != 0o700
    {
        return Err(invalid("export directory is not owner-only"));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)] // libc stat field types vary across Unix targets.
fn metadata_at(parent: &File, name: &CStr) -> Result<ExportMetadata, StorageError> {
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let metadata = unsafe { metadata.assume_init() };
    Ok(ExportMetadata {
        mode: metadata.st_mode as u32,
        uid: metadata.st_uid,
        links: metadata.st_nlink as u64,
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
    })
}

fn same_file(left: &File, right: &File) -> Result<bool, StorageError> {
    let left = ExportMetadata::from(&left.metadata()?);
    let right = ExportMetadata::from(&right.metadata()?);
    Ok(left.device == right.device && left.inode == right.inode)
}

fn unlink_at(parent: &File, name: &CStr, flags: libc::c_int) -> Result<(), StorageError> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn directory_entry_names(directory: &File) -> Result<Vec<CString>, StorageError> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(error.into());
    }
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    loop {
        errno::set_errno(errno::Errno(0));
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = errno::errno().0;
            unsafe { libc::closedir(stream) };
            return if error == 0 {
                Ok(names)
            } else {
                Err(io::Error::from_raw_os_error(error).into())
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary across Unix targets.
fn remove_all_entries(directory: &File) -> Result<(), StorageError> {
    for name in directory_entry_names(directory)? {
        let metadata = metadata_at(directory, &name)?;
        match metadata.mode & libc::S_IFMT as u32 {
            kind if kind == libc::S_IFREG as u32 => unlink_at(directory, &name, 0)?,
            kind if kind == libc::S_IFDIR as u32 => {
                let child = open_directory_at(directory, &name)?;
                let opened = ExportMetadata::from(&child.metadata()?);
                if metadata.device != opened.device || metadata.inode != opened.inode {
                    return Err(invalid("export cleanup directory changed during open"));
                }
                remove_all_entries(&child)?;
                let after = metadata_at(directory, &name)?;
                if after.device != opened.device || after.inode != opened.inode {
                    return Err(invalid("export cleanup directory changed before removal"));
                }
                unlink_at(directory, &name, libc::AT_REMOVEDIR)?;
            }
            _ => return Err(invalid("export cleanup contains an unsafe entry")),
        }
    }
    directory.sync_all()?;
    Ok(())
}

fn random_export_name(purpose: &str) -> Result<CString, StorageError> {
    let mut randomness = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut randomness)?;
    let suffix = randomness
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    CString::new(format!(".coding-brain-export-{purpose}-{suffix}"))
        .map_err(|_| invalid("random export name is invalid"))
}

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "android"))]
fn rename_noreplace_between(
    source_parent: &File,
    source: &CStr,
    destination_parent: &File,
    destination: &CStr,
) -> Result<(), StorageError> {
    let result = unsafe {
        libc::renameat2(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "musl"))]
fn rename_noreplace_between(
    source_parent: &File,
    source: &CStr,
    destination_parent: &File,
    destination: &CStr,
) -> Result<(), StorageError> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
fn rename_noreplace_between(
    source_parent: &File,
    source: &CStr,
    destination_parent: &File,
    destination: &CStr,
) -> Result<(), StorageError> {
    let result = unsafe {
        libc::renameatx_np(
            source_parent.as_raw_fd(),
            source.as_ptr(),
            destination_parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_noreplace_between(
    _source_parent: &File,
    _source: &CStr,
    _destination_parent: &File,
    _destination: &CStr,
) -> Result<(), StorageError> {
    Err(invalid("exclusive export cleanup rename is unsupported"))
}

fn remove_empty_quarantine(
    parent: &File,
    name: &CStr,
    quarantine: &File,
) -> Result<(), StorageError> {
    let at_path = metadata_at(parent, name)?;
    let opened = ExportMetadata::from(&quarantine.metadata()?);
    if at_path.device != opened.device || at_path.inode != opened.inode {
        return Err(invalid("export cleanup quarantine changed before removal"));
    }
    unlink_at(parent, name, libc::AT_REMOVEDIR)?;
    parent.sync_all()?;
    Ok(())
}

fn invalid(reason: &'static str) -> StorageError {
    StorageError::InvalidStorage(reason)
}

fn commit_export_snapshot(transaction: Transaction<'_>) -> Result<(), StorageError> {
    super::maintenance::sqlite_fault("export-snapshot-commit")
        .and_then(|()| transaction.commit())
        .map_err(|error| {
            super::maintenance::map_sqlite_error(super::StorageOperation::Export, false, error)
        })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use coding_brain_core::brain_activity::{
        ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    };
    use coding_brain_core::project::ProjectId;
    use rusqlite::TransactionBehavior;

    use super::*;
    use crate::brain::decisions::{DecisionContext, DecisionOutcome, DecisionRecord, DecisionType};
    use crate::brain::storage::{
        DecisionIdentity, DecisionKind, DecisionPayload, LearningErasePaths, StoragePaths,
    };
    use coding_brain_core::provider::AgentProvider;

    fn private_tempdir() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    #[test]
    fn export_snapshot_commit_maps_injected_io_to_export() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        let transaction = connection.unchecked_transaction().unwrap();
        let result = crate::brain::storage::maintenance::with_sqlite_fault(
            "export-snapshot-commit",
            rusqlite::ffi::SQLITE_IOERR_FSYNC,
            || commit_export_snapshot(transaction),
        );

        assert!(matches!(
            result,
            Err(StorageError::StorageFault {
                operation: super::super::StorageOperation::Export,
                category: super::super::StorageFaultCategory::Io,
            })
        ));
    }

    fn activity(root: &Path, activity_id: &str, decision_id: Option<&str>) -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: activity_id.into(),
            recorded_at_ms: 1,
            project: ProjectEvidence {
                project_id: ProjectId::Temporary("export-test".into()),
                cwd: root.into(),
                label: None,
            },
            session: None,
            state: ActivityState::Observed,
            tool: None,
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: decision_id.map(str::to_owned),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn decision(decision_id: &str) -> DecisionRecord {
        DecisionRecord {
            provider: AgentProvider::Codex,
            timestamp: "2026-08-06T00:00:00Z".into(),
            pid: 42,
            project: "export-test".into(),
            tool: None,
            command: None,
            brain_action: "observe".into(),
            brain_confidence: 1.0,
            brain_reasoning: "bounded".into(),
            user_action: "observed".into(),
            context: Some(DecisionContext {
                context_pct: None,
                last_tool_error: false,
                error_message: None,
                model: "test".into(),
                elapsed_secs: 0,
                files_modified_count: 0,
                total_tool_calls: 0,
                has_file_conflict: false,
                status: "observed".into(),
                recent_error_count: 0,
                subagent_count: 0,
                hour: None,
            }),
            outcome: Some(DecisionOutcome::Success),
            decision_type: DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some(decision_id.into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    fn inject_unknown_field(bytes: Vec<u8>, field: &str) -> Vec<u8> {
        let mut value = serde_json::from_slice::<Value>(&bytes).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert(field.into(), Value::String("secret".into()));
        serde_json::to_vec(&value).unwrap()
    }

    fn assert_both_exporters_fail_closed(paths: &StoragePaths, root: &Path) {
        let audit = root.join("audit-private-field");
        let audit_result = AuditExporter::new(paths).export(&audit);
        assert!(
            matches!(audit_result, Err(StorageError::InvalidStorage(_))),
            "unexpected audit result: {audit_result:?}"
        );
        assert!(!audit.exists());

        let legacy = root.join("legacy-private-field");
        let legacy_result = LegacyExporter::new(paths).export(&legacy);
        assert!(
            matches!(legacy_result, Err(StorageError::InvalidStorage(_))),
            "unexpected legacy result: {legacy_result:?}"
        );
        assert!(!legacy.exists());
    }

    fn substitute_export_root(output: &Path, displaced: &Path) {
        fs::rename(output, displaced).unwrap();
        fs::create_dir(output).unwrap();
        fs::set_permissions(output, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(output.join("sentinel"), b"replacement").unwrap();
    }

    fn assert_replacement_is_untouched(output: &Path) {
        assert_eq!(fs::read(output.join("sentinel")).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(output).unwrap().count(), 1);
    }

    #[test]
    fn exporters_reject_unknown_private_activity_fields() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut brain = BrainDb::create_current(&paths).unwrap();
        let cursor = brain
            .append_activity(activity(root.path(), "private-activity", None))
            .unwrap();
        let stored = brain
            .connection
            .query_row(
                "SELECT event_payload FROM activity_events WHERE source_cursor = ?1",
                [cursor.get() as i64],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        brain
            .connection
            .execute(
                "UPDATE activity_events SET event_payload = ?1 WHERE source_cursor = ?2",
                params![
                    inject_unknown_field(stored, "private_authority"),
                    cursor.get() as i64
                ],
            )
            .unwrap();
        drop(brain);

        assert_both_exporters_fail_closed(&paths, root.path());
    }

    #[test]
    fn exporters_reject_unknown_private_decision_fields() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        let mut brain = BrainDb::create_current(&paths).unwrap();
        let decision_id = "private-decision";
        let cursor = brain
            .append_activity(activity(
                root.path(),
                "private-decision-activity",
                Some(decision_id),
            ))
            .unwrap();
        brain
            .insert_decision(
                &DecisionIdentity::observation(decision_id, AgentProvider::Codex, 1),
                &DecisionPayload::new(DecisionKind::Observation, cursor, decision(decision_id)),
            )
            .unwrap();
        let stored = brain
            .connection
            .query_row(
                "SELECT decision_record FROM decision_payloads WHERE decision_id = ?1",
                [decision_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        brain
            .connection
            .execute(
                "UPDATE decision_payloads SET decision_record = ?1 WHERE decision_id = ?2",
                params![
                    inject_unknown_field(stored, "private_reasoning"),
                    decision_id
                ],
            )
            .unwrap();
        drop(brain);

        assert_both_exporters_fail_closed(&paths, root.path());
    }

    #[test]
    fn audit_export_rejects_root_substitution_without_touching_replacement() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = root.path().join("audit-export");
        let displaced = root.path().join("displaced-audit-export");

        let result = AuditExporter::new(&paths).export_with_seams(
            &output,
            |_| Ok(()),
            |created| {
                substitute_export_root(created, &displaced);
                Ok(())
            },
        );

        assert!(matches!(result, Err(StorageError::InvalidStorage(_))));
        assert_replacement_is_untouched(&output);
    }

    #[test]
    fn legacy_export_rejects_root_substitution_without_touching_replacement() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = root.path().join("legacy-export");
        let displaced = root.path().join("displaced-legacy-export");

        let result =
            LegacyExporter::new(&paths).export_with_before_validation(&output, |staging| {
                let created = staging
                    .parent()
                    .ok_or_else(|| invalid("legacy staging path has no parent"))?;
                substitute_export_root(created, &displaced);
                Ok(())
            });

        assert!(matches!(result, Err(StorageError::InvalidStorage(_))));
        assert_replacement_is_untouched(&output);
    }

    #[test]
    fn export_uses_short_snapshots_but_holds_the_erasure_gate_until_publication() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = root.path().join("audit-export");

        let result = AuditExporter::new(&paths).export_with_seams(
            &output,
            |_| {
                let mut writer = open_brain(&paths)?;
                let transaction = writer
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                transaction.execute(
                    "UPDATE schema_meta SET activity_high_water = activity_high_water
                     WHERE singleton = 1",
                    [],
                )?;
                transaction.commit()?;
                writer.append_activity(ActivityEvent {
                    schema_version: ACTIVITY_SCHEMA_VERSION,
                    kind: ActivityKind::Diagnostic,
                    activity_id: "after-export-bounds".into(),
                    recorded_at_ms: 1,
                    project: ProjectEvidence {
                        project_id: ProjectId::Temporary("export-test".into()),
                        cwd: root.path().into(),
                        label: None,
                    },
                    session: None,
                    state: ActivityState::Observed,
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
                })?;

                let mut eraser = open_brain(&paths)?;
                assert!(matches!(
                    eraser.forget_learning(&LearningErasePaths::new(
                        root.path().join("brain"),
                        Vec::new(),
                    )),
                    Err(StorageError::Busy)
                ));
                Ok(())
            },
            |staging| {
                assert_eq!(fs::read(staging.join("activity.jsonl"))?, b"");
                let brain = open_brain(&paths)?;
                brain.connection.execute(
                    "UPDATE schema_meta
                     SET erasure_generation = erasure_generation + 1
                     WHERE singleton = 1",
                    [],
                )?;
                Ok(())
            },
        );

        assert!(matches!(result, Err(StorageError::InvalidStorage(_))));
        assert!(!output.exists());
    }

    #[test]
    fn invalid_hidden_legacy_stage_is_removed_before_publication() {
        let root = private_tempdir();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = root.path().join("legacy-export");

        let result =
            LegacyExporter::new(&paths).export_with_before_validation(&output, |staging| {
                fs::write(staging.join("hooks/lifecycle.json"), b"{")?;
                Ok(())
            });

        assert!(result.is_err());
        assert!(!output.exists());
    }
}
