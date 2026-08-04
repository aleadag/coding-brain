use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use coding_brain_core::brain_activity::{ActivityEvent, ActivityState};
use coding_brain_core::lifecycle::PermissionAction;
use coding_brain_core::provider::AgentProvider;
use fs2::FileExt;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::brain::decisions::{DecisionContext, DecisionOutcome, DecisionRecord, DecisionType};
use crate::brain::secure_state::{SecureStateDirectory, SecureStateError};

use super::{ActivityCursor, BrainDb, StorageError};

pub const MAX_DECISION_RECORD_BYTES: usize = 1024 * 1024;
const MAX_PROJECTION_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecisionKind {
    Permission,
    Observation,
}

impl DecisionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Observation => "observation",
        }
    }

    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "permission" => Ok(Self::Permission),
            "observation" => Ok(Self::Observation),
            _ => Err(StorageError::InvalidStorage("decision kind is invalid")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionIdentity {
    Permission {
        decision_id: String,
        provider: AgentProvider,
        session_id: String,
        turn_id: String,
        tool_use_id: String,
        authority_action: PermissionAction,
        decision_source: String,
        decided_at_ms: u64,
    },
    Observation {
        decision_id: String,
        provider: AgentProvider,
        decided_at_ms: u64,
    },
}

impl DecisionIdentity {
    #[allow(clippy::too_many_arguments)] // Permission identity must carry the complete authority tuple.
    pub fn permission(
        decision_id: impl Into<String>,
        provider: AgentProvider,
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_use_id: impl Into<String>,
        authority_action: PermissionAction,
        decision_source: impl Into<String>,
        decided_at_ms: u64,
    ) -> Self {
        Self::Permission {
            decision_id: decision_id.into(),
            provider,
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            tool_use_id: tool_use_id.into(),
            authority_action,
            decision_source: decision_source.into(),
            decided_at_ms,
        }
    }

    pub fn observation(
        decision_id: impl Into<String>,
        provider: AgentProvider,
        decided_at_ms: u64,
    ) -> Self {
        Self::Observation {
            decision_id: decision_id.into(),
            provider,
            decided_at_ms,
        }
    }

    pub fn decision_id(&self) -> &str {
        match self {
            Self::Permission { decision_id, .. } | Self::Observation { decision_id, .. } => {
                decision_id
            }
        }
    }

    pub fn kind(&self) -> DecisionKind {
        match self {
            Self::Permission { .. } => DecisionKind::Permission,
            Self::Observation { .. } => DecisionKind::Observation,
        }
    }

    fn provider(&self) -> AgentProvider {
        match self {
            Self::Permission { provider, .. } | Self::Observation { provider, .. } => *provider,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionPayload {
    pub kind: DecisionKind,
    pub source_cursor: ActivityCursor,
    pub record: DecisionRecord,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LearningDecisionPage {
    pub decisions: Vec<DecisionPayload>,
    pub next_cursor: Option<ActivityCursor>,
    pub serialized_bytes: usize,
}

/// Holds a stable erasure boundary across a complete learning-data consumption.
///
/// The caller must retain this session through every page and downstream
/// publication. It intentionally does not provide a frozen SQLite snapshot:
/// concurrently committed decisions can appear in later cursor pages.
pub struct LearningReadSession<'database> {
    database: &'database BrainDb,
    _erasure_gate: File,
}

impl LearningReadSession<'_> {
    pub fn page_after(
        &self,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<LearningDecisionPage, StorageError> {
        self.database
            .learning_decisions_after_locked(after, max_rows, max_bytes)
    }
}

impl LearningDecisionPage {
    pub fn into_records(self) -> Vec<DecisionRecord> {
        self.decisions
            .into_iter()
            .map(|payload| payload.record)
            .collect()
    }
}

impl DecisionPayload {
    pub fn new(kind: DecisionKind, source_cursor: ActivityCursor, record: DecisionRecord) -> Self {
        Self {
            kind,
            source_cursor,
            record,
        }
    }

    pub fn serialized_len(&self) -> Result<usize, StorageError> {
        Ok(serialize_record(&self.record)?.len())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErasureState {
    pub generation: u64,
    pub complete: bool,
}

#[derive(Clone, Debug)]
pub struct LearningErasePaths {
    brain_root: PathBuf,
    legacy_sources: Vec<PathBuf>,
}

impl LearningErasePaths {
    pub fn new(brain_root: PathBuf, mut legacy_sources: Vec<PathBuf>) -> Self {
        legacy_sources.sort();
        legacy_sources.dedup();
        Self {
            brain_root,
            legacy_sources,
        }
    }
}

impl BrainDb {
    pub fn insert_decision(
        &mut self,
        identity: &DecisionIdentity,
        payload: &DecisionPayload,
    ) -> Result<(), StorageError> {
        let _erasure_gate = acquire_shared_erasure_gate(&self.learning_root)?;
        self.ensure_learning_available()?;
        validate_identity_payload(identity, payload)?;
        validate_source_activity(&self.connection, identity, payload)?;
        let serialized = serialize_record(&payload.record)?;
        let command = bounded_projection(payload.record.command.as_deref());
        let reasoning = bounded_projection(Some(&payload.record.brain_reasoning));
        let note = bounded_projection(payload.record.override_reason.as_deref());
        let decided_at = identity_decided_at(identity)?;
        let cursor = i64::try_from(payload.source_cursor.get())
            .map_err(|_| StorageError::InvalidStorage("decision cursor is out of range"))?;

        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        match identity {
            DecisionIdentity::Permission {
                decision_id,
                provider,
                session_id,
                turn_id,
                tool_use_id,
                authority_action,
                decision_source,
                ..
            } => {
                transaction.execute(
                    "INSERT INTO decision_identities (
                        decision_id, identity_kind, provider, session_id, turn_id, tool_use_id,
                        authority_action, decision_source, decided_at_ms
                     ) VALUES (?1, 'permission', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        decision_id,
                        provider.as_str(),
                        session_id,
                        turn_id,
                        tool_use_id,
                        action_label(*authority_action),
                        decision_source,
                        decided_at,
                    ],
                )?;
            }
            DecisionIdentity::Observation {
                decision_id,
                provider,
                ..
            } => {
                transaction.execute(
                    "INSERT INTO decision_identities (
                        decision_id, identity_kind, provider, decided_at_ms
                     ) VALUES (?1, 'observation', ?2, ?3)",
                    params![decision_id, provider.as_str(), decided_at],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO decision_payloads (
                decision_id, payload_kind, source_cursor, normalized_command,
                reasoning, note, decision_record
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                identity.decision_id(),
                payload.kind.as_str(),
                cursor,
                command,
                reasoning,
                note,
                serialized,
            ],
        )?;
        decision_write_stage("before-commit")?;
        super::activity::commit_before_deadline(self.deadline, || transaction.commit())
    }

    pub fn decision_identity(
        &self,
        decision_id: &str,
    ) -> Result<Option<DecisionIdentity>, StorageError> {
        validate_id(decision_id)?;
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let row = self
            .connection
            .query_row(
                "SELECT identity_kind, provider, session_id, turn_id, tool_use_id,
                        authority_action, decision_source, decided_at_ms
                 FROM decision_identities WHERE decision_id = ?1",
                [decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        row.map(|row| materialize_identity(decision_id, row))
            .transpose()
    }

    pub fn decision_payload(
        &self,
        decision_id: &str,
    ) -> Result<Option<DecisionPayload>, StorageError> {
        let _erasure_gate = acquire_shared_erasure_gate(&self.learning_root)?;
        self.ensure_learning_available()?;
        validate_id(decision_id)?;
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let row = self
            .connection
            .query_row(
                "SELECT payload_kind, source_cursor, normalized_command, reasoning, note,
                        decision_record
                 FROM decision_payloads WHERE decision_id = ?1",
                [decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()?;
        let payload = row
            .map(|row| materialize_payload(decision_id, row))
            .transpose()?;
        if let Some(payload) = &payload {
            let identity =
                self.decision_identity(decision_id)?
                    .ok_or(StorageError::InvalidStorage(
                        "decision payload identity is absent",
                    ))?;
            if identity.kind() != payload.kind || identity.provider() != payload.record.provider {
                return Err(StorageError::InvalidStorage(
                    "decision payload disagrees with its identity",
                ));
            }
            validate_source_activity(&self.connection, &identity, payload)?;
        }
        Ok(payload)
    }

    pub fn learning_decisions(
        &self,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<Vec<DecisionPayload>, StorageError> {
        Ok(self
            .learning_read_session()?
            .page_after(None, max_rows, max_bytes)?
            .decisions)
    }

    pub fn learning_read_session(&self) -> Result<LearningReadSession<'_>, StorageError> {
        let gate = acquire_shared_erasure_gate(&self.learning_root)?;
        self.ensure_learning_available()?;
        Ok(LearningReadSession {
            database: self,
            _erasure_gate: gate,
        })
    }

    pub fn learning_decisions_after(
        &self,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<LearningDecisionPage, StorageError> {
        self.learning_read_session()?
            .page_after(after, max_rows, max_bytes)
    }

    fn learning_decisions_after_locked(
        &self,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<LearningDecisionPage, StorageError> {
        self.ensure_learning_available()?;
        if max_rows == 0 || max_bytes == 0 || max_rows > i64::MAX as usize {
            return Err(StorageError::InvalidStorage(
                "decision read bounds are invalid",
            ));
        }
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let row_limit = i64::try_from(max_rows)
            .ok()
            .and_then(|limit| limit.checked_add(1))
            .unwrap_or(i64::MAX);
        let mut statement = self.connection.prepare(
            "SELECT p.decision_id, i.identity_kind, i.provider, i.session_id, i.turn_id,
                    i.tool_use_id, i.authority_action, i.decision_source, i.decided_at_ms,
                    p.payload_kind, p.source_cursor, p.normalized_command, p.reasoning, p.note,
                    p.decision_record
             FROM decision_payloads AS p
             JOIN decision_identities AS i ON i.decision_id = p.decision_id
                                           AND i.identity_kind = p.payload_kind
             JOIN activity_events AS a ON a.source_cursor = p.source_cursor
             LEFT JOIN permission_commits AS c ON c.decision_id = i.decision_id
             WHERE (i.identity_kind = 'observation'
                OR (i.identity_kind = 'permission'
                    AND c.terminal_activity_id = a.activity_id
                    AND c.provider = i.provider
                    AND c.session_id = i.session_id
                    AND c.turn_id = i.turn_id
                    AND c.tool_use_id = i.tool_use_id
                    AND c.authority_action = i.authority_action))
               AND p.source_cursor > ?1
             ORDER BY p.source_cursor ASC, p.decision_id ASC LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            after.map_or(0, |cursor| cursor.get() as i64),
            row_limit
        ])?;
        let mut result = Vec::new();
        let mut total = 0usize;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if result.len() == max_rows {
                has_more = true;
                break;
            }
            let decision_id: String = row.get(0)?;
            let identity = materialize_identity(
                &decision_id,
                (
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ),
            )?;
            let raw = (
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
                row.get(13)?,
                row.get::<_, Vec<u8>>(14)?,
            );
            let next_total = total
                .checked_add(raw.5.len())
                .ok_or(StorageError::InvalidStorage("decision byte limit exceeded"))?;
            if next_total > max_bytes {
                if result.is_empty() {
                    return Err(StorageError::InvalidStorage(
                        "decision byte limit cannot hold the next record",
                    ));
                }
                has_more = true;
                break;
            }
            total = next_total;
            let payload = materialize_payload(&decision_id, raw)?;
            if payload.kind != identity.kind() || payload.record.provider != identity.provider() {
                return Err(StorageError::InvalidStorage(
                    "decision payload disagrees with identity",
                ));
            }
            let activity =
                super::activity::validated_activity_at(&self.connection, payload.source_cursor)?;
            validate_source_activity_event(&identity, &activity.event)?;
            result.push(payload);
            super::activity::ensure_deadline(self.deadline)?;
        }
        let next_cursor = has_more
            .then(|| result.last().map(|payload| payload.source_cursor))
            .flatten();
        Ok(LearningDecisionPage {
            decisions: result,
            next_cursor,
            serialized_bytes: total,
        })
    }

    pub fn erasure_state(&self) -> Result<ErasureState, StorageError> {
        let (state, generation): (String, i64) = self.connection.query_row(
            "SELECT erasure_state, erasure_generation FROM schema_meta WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(ErasureState {
            generation: u64::try_from(generation)
                .map_err(|_| StorageError::InvalidStorage("erasure generation is invalid"))?,
            complete: match state.as_str() {
                "complete" => true,
                "in_progress" => false,
                _ => return Err(StorageError::InvalidStorage("erasure state is invalid")),
            },
        })
    }

    pub fn forget_learning(&mut self, paths: &LearningErasePaths) -> Result<u64, StorageError> {
        self.forget_or_resume(paths, false)
    }

    pub fn resume_forget_learning(
        &mut self,
        paths: &LearningErasePaths,
    ) -> Result<u64, StorageError> {
        self.forget_or_resume(paths, true)
    }

    fn forget_or_resume(
        &mut self,
        paths: &LearningErasePaths,
        resume: bool,
    ) -> Result<u64, StorageError> {
        if paths.brain_root != self.learning_root {
            return Err(StorageError::InvalidStorage(
                "erasure brain root does not match storage state",
            ));
        }
        let locks = ErasureLocks::acquire(paths, &self.learning_root)?;
        let current = self.erasure_state()?;
        if resume && current.complete {
            return Ok(current.generation);
        }
        let generation = if !current.complete {
            current.generation
        } else {
            current
                .generation
                .checked_add(1)
                .ok_or(StorageError::InvalidStorage(
                    "erasure generation is exhausted",
                ))?
        };
        if current.complete {
            self.connection.execute(
                "UPDATE schema_meta SET erasure_state = 'in_progress', erasure_generation = ?1
                 WHERE singleton = 1",
                [i64::try_from(generation).map_err(|_| {
                    StorageError::InvalidStorage("erasure generation is exhausted")
                })?],
            )?;
        }
        erasure_stage(&paths.brain_root, "after-in-progress")?;

        self.connection
            .execute("DELETE FROM decision_payloads", [])?;
        erasure_stage(&paths.brain_root, "after-database-delete")?;
        locks.erase_legacy_sources()?;
        erasure_stage(&paths.brain_root, "after-external-delete")?;
        locks.erase_published_preferences()?;
        erasure_stage(&paths.brain_root, "after-generation-delete")?;
        erasure_stage(&paths.brain_root, "before-wal-truncate")?;
        checkpoint_truncate(&self.connection)?;
        erasure_stage(&paths.brain_root, "after-wal-truncate")?;
        SecureStateDirectory::open_existing_strict(self.database_directory()?)
            .map_err(secure_storage_error)?
            .sync()?;
        locks.brain.sync()?;
        erasure_stage(&paths.brain_root, "before-complete")?;
        locks.validate_path_correspondence()?;
        let updated = self.connection.execute(
            "UPDATE schema_meta SET erasure_state = 'complete'
             WHERE singleton = 1 AND erasure_generation = ?1 AND erasure_state = 'in_progress'",
            [i64::try_from(generation)
                .map_err(|_| StorageError::InvalidStorage("erasure generation is invalid"))?],
        )?;
        if updated != 1 {
            return Err(StorageError::InvalidStorage(
                "erasure completion marker did not update exactly one row",
            ));
        }
        erasure_stage(&paths.brain_root, "after-complete")?;
        Ok(generation)
    }

    fn ensure_learning_available(&self) -> Result<(), StorageError> {
        if self.erasure_state()?.complete {
            Ok(())
        } else {
            Err(StorageError::MigrationRequired)
        }
    }

    fn database_directory(&self) -> Result<&Path, StorageError> {
        self.database_path
            .parent()
            .ok_or(StorageError::InvalidStorage(
                "database has no parent directory",
            ))
    }
}

type IdentityRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
);

type PayloadRow = (
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<u8>,
);

fn materialize_identity(
    decision_id: &str,
    row: IdentityRow,
) -> Result<DecisionIdentity, StorageError> {
    let provider = parse_provider(&row.1)?;
    let decided_at = u64::try_from(row.7)
        .map_err(|_| StorageError::InvalidStorage("decision timestamp is invalid"))?;
    match DecisionKind::parse(&row.0)? {
        DecisionKind::Observation => {
            if row.2.is_some()
                || row.3.is_some()
                || row.4.is_some()
                || row.5.is_some()
                || row.6.is_some()
            {
                return Err(StorageError::InvalidStorage(
                    "observation contains authority",
                ));
            }
            Ok(DecisionIdentity::observation(
                decision_id,
                provider,
                decided_at,
            ))
        }
        DecisionKind::Permission => Ok(DecisionIdentity::permission(
            decision_id,
            provider,
            row.2.ok_or(StorageError::InvalidStorage(
                "permission identity is incomplete",
            ))?,
            row.3.ok_or(StorageError::InvalidStorage(
                "permission identity is incomplete",
            ))?,
            row.4.ok_or(StorageError::InvalidStorage(
                "permission identity is incomplete",
            ))?,
            parse_action(row.5.as_deref())?,
            row.6.ok_or(StorageError::InvalidStorage(
                "permission identity is incomplete",
            ))?,
            decided_at,
        )),
    }
}

fn materialize_payload(
    decision_id: &str,
    row: PayloadRow,
) -> Result<DecisionPayload, StorageError> {
    if row.5.len() > MAX_DECISION_RECORD_BYTES {
        return Err(StorageError::InvalidStorage(
            "decision record exceeds its size limit",
        ));
    }
    let kind = DecisionKind::parse(&row.0)?;
    let record = deserialize_record(&row.5)?;
    if record.decision_id.as_deref() != Some(decision_id) {
        return Err(StorageError::InvalidStorage(
            "decision payload ID disagrees with identity",
        ));
    }
    if bounded_projection(record.command.as_deref()) != row.2
        || bounded_projection(Some(&record.brain_reasoning)) != row.3
        || bounded_projection(record.override_reason.as_deref()) != row.4
    {
        return Err(StorageError::InvalidStorage(
            "decision typed projection disagrees with payload",
        ));
    }
    Ok(DecisionPayload {
        kind,
        source_cursor: ActivityCursor::try_from(row.1)?,
        record,
    })
}

fn validate_identity_payload(
    identity: &DecisionIdentity,
    payload: &DecisionPayload,
) -> Result<(), StorageError> {
    validate_id(identity.decision_id())?;
    if identity.kind() != payload.kind
        || payload.record.decision_id.as_deref() != Some(identity.decision_id())
        || payload.record.provider != identity.provider()
    {
        return Err(StorageError::InvalidStorage(
            "decision identity and payload disagree",
        ));
    }
    Ok(())
}

fn validate_source_activity(
    connection: &rusqlite::Connection,
    identity: &DecisionIdentity,
    payload: &DecisionPayload,
) -> Result<(), StorageError> {
    let activity = super::activity::validated_activity_at(connection, payload.source_cursor)?;
    validate_source_activity_event(identity, &activity.event)
}

fn validate_source_activity_event(
    identity: &DecisionIdentity,
    event: &ActivityEvent,
) -> Result<(), StorageError> {
    if event.kind != coding_brain_core::brain_activity::ActivityKind::Decision
        || event.decision_id.as_deref() != Some(identity.decision_id())
    {
        return Err(StorageError::InvalidStorage(
            "decision source activity has a different kind or decision ID",
        ));
    }
    if let DecisionIdentity::Permission {
        provider,
        session_id,
        turn_id,
        tool_use_id,
        authority_action,
        ..
    } = identity
    {
        let session = event.session.as_ref().ok_or(StorageError::InvalidStorage(
            "permission source activity has no session",
        ))?;
        let expected_state = match authority_action {
            PermissionAction::Allow => ActivityState::Allowed,
            PermissionAction::Deny => ActivityState::Denied,
        };
        if event.state != expected_state
            || session.provider != *provider
            || session.session_id != *session_id
            || session.turn_id.as_deref() != Some(turn_id)
            || session.tool_use_id.as_deref() != Some(tool_use_id)
        {
            return Err(StorageError::InvalidStorage(
                "permission source activity disagrees with authority identity",
            ));
        }
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 512 {
        Err(StorageError::InvalidStorage("decision ID is out of range"))
    } else {
        Ok(())
    }
}

fn identity_decided_at(identity: &DecisionIdentity) -> Result<i64, StorageError> {
    let value = match identity {
        DecisionIdentity::Permission { decided_at_ms, .. }
        | DecisionIdentity::Observation { decided_at_ms, .. } => *decided_at_ms,
    };
    i64::try_from(value).map_err(|_| StorageError::InvalidStorage("decision timestamp is invalid"))
}

fn action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Allow => "allow",
        PermissionAction::Deny => "deny",
    }
}

fn parse_action(value: Option<&str>) -> Result<PermissionAction, StorageError> {
    match value {
        Some("allow") => Ok(PermissionAction::Allow),
        Some("deny") => Ok(PermissionAction::Deny),
        _ => Err(StorageError::InvalidStorage("permission action is invalid")),
    }
}

fn parse_provider(value: &str) -> Result<AgentProvider, StorageError> {
    match value {
        "codex" => Ok(AgentProvider::Codex),
        "claude" => Ok(AgentProvider::Claude),
        "antigravity" => Ok(AgentProvider::Antigravity),
        _ => Err(StorageError::InvalidStorage("decision provider is invalid")),
    }
}

fn bounded_projection(value: Option<&str>) -> Option<String> {
    value.map(|value| {
        if value.len() <= MAX_PROJECTION_BYTES {
            value.to_owned()
        } else {
            let mut end = MAX_PROJECTION_BYTES;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value[..end].to_owned()
        }
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDecisionRecord {
    provider: AgentProvider,
    #[serde(rename = "ts")]
    timestamp: String,
    pid: u32,
    project: String,
    tool: Option<String>,
    command: Option<String>,
    brain_action: String,
    brain_confidence: f64,
    brain_reasoning: String,
    user_action: String,
    context: Option<StoredDecisionContext>,
    outcome: Option<StoredDecisionOutcome>,
    decision_type: String,
    suggested_at: Option<u64>,
    resolved_at: Option<u64>,
    override_reason: Option<String>,
    decision_id: Option<String>,
    brain_decision_ms: Option<u64>,
    cache_hit: Option<bool>,
    canonical: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredDecisionContext {
    context_pct: Option<u8>,
    last_tool_error: bool,
    error_message: Option<String>,
    model: String,
    elapsed_secs: u64,
    files_modified_count: u32,
    total_tool_calls: u32,
    has_file_conflict: bool,
    status: String,
    recent_error_count: u8,
    subagent_count: u8,
    hour: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
enum StoredDecisionOutcome {
    Success,
    Error(String),
}

impl From<&DecisionRecord> for StoredDecisionRecord {
    fn from(record: &DecisionRecord) -> Self {
        Self {
            provider: record.provider,
            timestamp: record.timestamp.clone(),
            pid: record.pid,
            project: record.project.clone(),
            tool: record.tool.clone(),
            command: record.command.clone(),
            brain_action: record.brain_action.clone(),
            brain_confidence: record.brain_confidence,
            brain_reasoning: record.brain_reasoning.clone(),
            user_action: record.user_action.clone(),
            context: record
                .context
                .as_ref()
                .map(|context| StoredDecisionContext {
                    context_pct: context.context_pct,
                    last_tool_error: context.last_tool_error,
                    error_message: context.error_message.clone(),
                    model: context.model.clone(),
                    elapsed_secs: context.elapsed_secs,
                    files_modified_count: context.files_modified_count,
                    total_tool_calls: context.total_tool_calls,
                    has_file_conflict: context.has_file_conflict,
                    status: context.status.clone(),
                    recent_error_count: context.recent_error_count,
                    subagent_count: context.subagent_count,
                    hour: context.hour,
                }),
            outcome: record.outcome.as_ref().map(|outcome| match outcome {
                DecisionOutcome::Success => StoredDecisionOutcome::Success,
                DecisionOutcome::Error(detail) => StoredDecisionOutcome::Error(detail.clone()),
            }),
            decision_type: record.decision_type.label().to_owned(),
            suggested_at: record.suggested_at,
            resolved_at: record.resolved_at,
            override_reason: record.override_reason.clone(),
            decision_id: record.decision_id.clone(),
            brain_decision_ms: record.brain_decision_ms,
            cache_hit: record.cache_hit,
            canonical: record.canonical,
        }
    }
}

impl TryFrom<StoredDecisionRecord> for DecisionRecord {
    type Error = StorageError;

    fn try_from(record: StoredDecisionRecord) -> Result<Self, Self::Error> {
        if record.decision_type != "session" || !record.brain_confidence.is_finite() {
            return Err(StorageError::InvalidStorage("decision record is invalid"));
        }
        Ok(Self {
            provider: record.provider,
            timestamp: record.timestamp,
            pid: record.pid,
            project: record.project,
            tool: record.tool,
            command: record.command,
            brain_action: record.brain_action,
            brain_confidence: record.brain_confidence,
            brain_reasoning: record.brain_reasoning,
            user_action: record.user_action,
            context: record.context.map(|context| DecisionContext {
                context_pct: context.context_pct,
                last_tool_error: context.last_tool_error,
                error_message: context.error_message,
                model: context.model,
                elapsed_secs: context.elapsed_secs,
                files_modified_count: context.files_modified_count,
                total_tool_calls: context.total_tool_calls,
                has_file_conflict: context.has_file_conflict,
                status: context.status,
                recent_error_count: context.recent_error_count,
                subagent_count: context.subagent_count,
                hour: context.hour,
            }),
            outcome: record.outcome.map(|outcome| match outcome {
                StoredDecisionOutcome::Success => DecisionOutcome::Success,
                StoredDecisionOutcome::Error(detail) => DecisionOutcome::Error(detail),
            }),
            decision_type: DecisionType::Session,
            suggested_at: record.suggested_at,
            resolved_at: record.resolved_at,
            override_reason: record.override_reason,
            decision_id: record.decision_id,
            brain_decision_ms: record.brain_decision_ms,
            cache_hit: record.cache_hit,
            canonical: record.canonical,
        })
    }
}

fn serialize_record(record: &DecisionRecord) -> Result<Vec<u8>, StorageError> {
    let bytes = serde_json::to_vec(&StoredDecisionRecord::from(record))
        .map_err(|_| StorageError::InvalidStorage("decision record is not serializable"))?;
    if bytes.len() > MAX_DECISION_RECORD_BYTES {
        return Err(StorageError::InvalidStorage(
            "decision record exceeds its size limit",
        ));
    }
    Ok(bytes)
}

fn deserialize_record(bytes: &[u8]) -> Result<DecisionRecord, StorageError> {
    let record: StoredDecisionRecord = serde_json::from_slice(bytes)
        .map_err(|_| StorageError::InvalidStorage("decision record JSON is invalid"))?;
    record.try_into()
}

struct ErasureLocks {
    legacy_sources: Vec<LegacyEraseRoot>,
    brain: SecureStateDirectory,
    files: Vec<File>,
}

enum LegacyEraseRoot {
    Present(SecureStateDirectory),
    Missing(PathBuf),
}

impl ErasureLocks {
    fn acquire(paths: &LearningErasePaths, brain_root: &Path) -> Result<Self, StorageError> {
        let mut legacy_sources = Vec::new();
        let mut files = Vec::new();
        for path in &paths.legacy_sources {
            let directory = match SecureStateDirectory::open_existing_strict(path) {
                Ok(directory) => directory,
                Err(SecureStateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                    legacy_sources.push(LegacyEraseRoot::Missing(path.clone()));
                    continue;
                }
                Err(error) => return Err(secure_storage_error(error)),
            };
            files.push(lock_exclusive(
                directory
                    .open_regular_strict(c"decisions.lock", true)
                    .map_err(secure_storage_error)?,
            )?);
            legacy_sources.push(LegacyEraseRoot::Present(directory));
        }
        let brain =
            SecureStateDirectory::open_or_create(brain_root).map_err(secure_storage_error)?;
        files.push(lock_exclusive(
            brain
                .open_regular_strict(c"erasure.lock", true)
                .map_err(secure_storage_error)?,
        )?);
        files.push(lock_exclusive(
            brain
                .open_regular_strict(c"distill.lock", true)
                .map_err(secure_storage_error)?,
        )?);
        Ok(Self {
            legacy_sources,
            brain,
            files,
        })
    }

    fn erase_legacy_sources(&self) -> Result<(), StorageError> {
        for root in &self.legacy_sources {
            let directory = match root {
                LegacyEraseRoot::Present(directory) => directory,
                LegacyEraseRoot::Missing(path) => {
                    ensure_legacy_root_still_missing(path)?;
                    continue;
                }
            };
            for name in [c"decisions.jsonl", c"canonical.jsonl", c"preferences.json"] {
                directory
                    .remove_regular_if_present(name)
                    .map_err(secure_storage_error)?;
            }
            directory
                .remove_tree_if_present(c"preferences")
                .map_err(secure_storage_error)?;
            directory.sync()?;
        }
        Ok(())
    }

    fn erase_published_preferences(&self) -> Result<(), StorageError> {
        self.brain
            .remove_regular_if_present(c"distill-watermark.json")
            .map_err(secure_storage_error)?;
        self.brain
            .remove_regular_if_present(c"distill-trigger")
            .map_err(secure_storage_error)?;
        self.brain
            .remove_tree_if_present(c"preferences-generations")
            .map_err(secure_storage_error)?;
        self.brain.sync()?;
        Ok(())
    }

    fn validate_path_correspondence(&self) -> Result<(), StorageError> {
        for root in &self.legacy_sources {
            match root {
                LegacyEraseRoot::Present(directory) => directory
                    .validate_path_correspondence()
                    .map_err(secure_storage_error)?,
                LegacyEraseRoot::Missing(path) => ensure_legacy_root_still_missing(path)?,
            }
        }
        self.brain
            .validate_path_correspondence()
            .map_err(secure_storage_error)
    }
}

fn ensure_legacy_root_still_missing(path: &Path) -> Result<(), StorageError> {
    match SecureStateDirectory::open_existing_strict(path) {
        Err(SecureStateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(StorageError::InvalidStorage(
            "legacy state root appeared after erasure lock acquisition",
        )),
        Err(error) => Err(secure_storage_error(error)),
    }
}

fn acquire_shared_erasure_gate(brain_root: &Path) -> Result<File, StorageError> {
    let directory =
        SecureStateDirectory::open_or_create(brain_root).map_err(secure_storage_error)?;
    let file = directory
        .open_regular_strict(c"erasure.lock", true)
        .map_err(secure_storage_error)?;
    FileExt::try_lock_shared(&file).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            StorageError::Busy
        } else {
            StorageError::Io(error)
        }
    })?;
    Ok(file)
}

fn lock_exclusive(file: File) -> Result<File, StorageError> {
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            StorageError::Busy
        } else {
            StorageError::Io(error)
        }
    })?;
    Ok(file)
}

fn secure_storage_error(error: SecureStateError) -> StorageError {
    match error {
        SecureStateError::Io(error) => StorageError::Io(error),
        SecureStateError::InvalidStorage(reason) => StorageError::InvalidStorage(reason),
    }
}

impl Drop for ErasureLocks {
    fn drop(&mut self) {
        for file in self.files.iter().rev() {
            let _ = FileExt::unlock(file);
        }
    }
}

fn checkpoint_truncate(connection: &rusqlite::Connection) -> Result<(), StorageError> {
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy != 0 || log_frames != 0 || checkpointed_frames != 0 {
        return Err(StorageError::Busy);
    }
    Ok(())
}

fn erasure_stage(brain_root: &Path, stage: &'static str) -> Result<(), StorageError> {
    #[cfg(debug_assertions)]
    {
        if std::env::var("CODING_BRAIN_ERASURE_TEST_FAIL").as_deref() == Ok(stage) {
            return Err(StorageError::InvalidStorage("injected erasure failure"));
        }
        if std::env::var("CODING_BRAIN_ERASURE_TEST_PAUSE").as_deref() == Ok(stage) {
            let marker = std::env::var_os("CODING_BRAIN_ERASURE_TEST_MARKER")
                .map(PathBuf::from)
                .ok_or(StorageError::InvalidStorage(
                    "erasure test marker is absent",
                ))?;
            fs::write(marker, stage)?;
            if let Some(release) = std::env::var_os("CODING_BRAIN_ERASURE_TEST_RELEASE") {
                let release = PathBuf::from(release);
                while !release.exists() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            } else {
                loop {
                    std::thread::park();
                }
            }
        }
    }
    let _ = brain_root;
    Ok(())
}

fn decision_write_stage(stage: &'static str) -> Result<(), StorageError> {
    #[cfg(debug_assertions)]
    if std::env::var("CODING_BRAIN_DECISION_WRITE_PAUSE").as_deref() == Ok(stage) {
        let marker = std::env::var_os("CODING_BRAIN_DECISION_WRITE_MARKER")
            .map(PathBuf::from)
            .ok_or(StorageError::InvalidStorage(
                "decision write test marker is absent",
            ))?;
        let release = std::env::var_os("CODING_BRAIN_DECISION_WRITE_RELEASE")
            .map(PathBuf::from)
            .ok_or(StorageError::InvalidStorage(
                "decision write test release is absent",
            ))?;
        fs::write(marker, stage)?;
        while !release.exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    Ok(())
}
