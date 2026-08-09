use std::collections::HashSet;
use std::num::NonZeroU64;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivitySnapshot,
    ActivityState, CorrectionDisposition, MAX_ACTIVITY_EVENT_BYTES, SnapshotLimits,
};
use coding_brain_core::provider::AgentProvider;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};

use super::{BrainDb, StorageDeadline, StorageError};

const MAX_ACTIVITY_ID_BYTES: usize = 512;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ActivityCursor(NonZeroU64);

impl ActivityCursor {
    pub fn get(self) -> u64 {
        self.0.get()
    }
}

impl TryFrom<u64> for ActivityCursor {
    type Error = StorageError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > i64::MAX as u64 {
            return Err(StorageError::InvalidStorage(
                "activity cursor is out of range",
            ));
        }
        NonZeroU64::new(value)
            .map(Self)
            .ok_or(StorageError::InvalidStorage(
                "activity cursor is out of range",
            ))
    }
}

impl TryFrom<i64> for ActivityCursor {
    type Error = StorageError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u64::try_from(value)
            .map_err(|_| StorageError::InvalidStorage("activity cursor is out of range"))?
            .try_into()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityRecord {
    pub cursor: ActivityCursor,
    pub event: ActivityEvent,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActivityPage {
    pub events: Vec<ActivityRecord>,
    /// Exclusive boundary for the next page, or `None` when this query reached EOF.
    pub next_cursor: Option<ActivityCursor>,
    pub serialized_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryReservationOutcome {
    Reserved,
    Duplicate,
    Cooldown,
}

impl ActivityPage {
    /// Projects a caller-assembled window whose logical activity sequences are complete.
    ///
    /// A single bounded page can end between events for one logical activity. Callers
    /// must join the needed pages (including all needed `activity_by_id` pages)
    /// before using this seam.
    pub fn project_complete_window(
        mut self,
        limits: SnapshotLimits,
        now_ms: u64,
    ) -> ActivitySnapshot {
        self.events.sort_by_key(|record| record.cursor);
        crate::brain::activity::project_activity_events(
            self.events.into_iter().map(|record| record.event).collect(),
            limits,
            now_ms,
        )
    }
}

#[derive(Debug)]
pub(super) struct PreparedActivity {
    event: ActivityEvent,
    payload: Vec<u8>,
    event_kind: &'static str,
    event_state: &'static str,
    recorded_at_ms: i64,
    terminal: Option<TerminalIdentity>,
    outcome: Option<&'static str>,
    correction: Option<&'static str>,
}

#[derive(Debug)]
struct TerminalIdentity {
    provider: &'static str,
    session_id: String,
    turn_id: String,
    tool_use_id: Option<String>,
    action: &'static str,
}

impl BrainDb {
    pub fn append_recovery_activity_if_absent(
        &mut self,
        event: ActivityEvent,
    ) -> Result<bool, StorageError> {
        let diagnostic =
            event.kind == ActivityKind::Diagnostic && event.state == ActivityState::Error;
        let attention = event.kind == ActivityKind::Decision
            && event.state == ActivityState::Abstained
            && event.rule_id.as_deref() == Some("actionable_prompt_attention");
        if !diagnostic && !attention {
            return Err(StorageError::InvalidStorage("recovery activity is invalid"));
        }
        let activity_id = event.activity_id.clone();
        let prepared = prepare_activity(event)?;
        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if transaction
            .query_row(
                "SELECT 1 FROM recovery_once WHERE activity_id = ?1",
                [&activity_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Ok(false);
        }
        let current = validated_high_water(&transaction)?;
        let cursor = current.checked_add(1).ok_or(StorageError::InvalidStorage(
            "activity cursor space is exhausted",
        ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [cursor],
        )?;
        insert_activity(&transaction, cursor, &prepared, None)?;
        transaction.execute(
            "INSERT INTO recovery_once (activity_id, source_cursor) VALUES (?1, ?2)",
            params![activity_id, cursor],
        )?;
        commit_before_deadline(self.deadline, super::StorageOperation::Activity, || {
            transaction.commit()
        })?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_recovery(
        &mut self,
        attempt_key: &str,
        session_key: &str,
        provider: AgentProvider,
        session_id: &str,
        ephemeral: bool,
        epoch_order: u64,
        reserved_at_ms: u64,
        cooldown_ms: u64,
    ) -> Result<RecoveryReservationOutcome, StorageError> {
        if attempt_key.is_empty()
            || attempt_key.len() > 4_096
            || session_key.is_empty()
            || session_key.len() > 4_096
            || session_id.is_empty()
            || session_id.len() > 512
            || epoch_order == 0
            || epoch_order > i64::MAX as u64
            || reserved_at_ms > i64::MAX as u64
            || cooldown_ms > i64::MAX as u64
        {
            return Err(StorageError::InvalidStorage(
                "recovery reservation is out of range",
            ));
        }
        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "DELETE FROM recovery_reservations AS r
             WHERE r.ephemeral = 0 AND NOT EXISTS (
                 SELECT 1 FROM lifecycle_sessions AS l
                 WHERE l.provider = r.provider AND l.session_id = r.session_id
             )",
            [],
        )?;
        let ephemeral_cutoff = reserved_at_ms.saturating_sub(24 * 60 * 60 * 1_000) as i64;
        transaction.execute(
            "DELETE FROM recovery_reservations
             WHERE ephemeral = 1 AND reserved_at_ms < ?1",
            [ephemeral_cutoff],
        )?;
        transaction.execute(
            "DELETE FROM recovery_reservations
             WHERE ephemeral = 1 AND session_key NOT IN (
                 SELECT session_key FROM recovery_reservations
                 WHERE ephemeral = 1 ORDER BY reserved_at_ms DESC LIMIT 255
             )",
            [],
        )?;
        if !ephemeral
            && transaction
                .query_row(
                    "SELECT 1 FROM lifecycle_sessions WHERE provider = ?1 AND session_id = ?2",
                    params![provider.as_str(), session_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_none()
        {
            return Err(StorageError::InvalidStorage(
                "stable recovery session is absent",
            ));
        }
        let existing = transaction
            .query_row(
                "SELECT attempt_key, epoch_order, reserved_at_ms
                 FROM recovery_reservations WHERE session_key = ?1",
                [session_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((existing_attempt, existing_epoch, existing_at)) = existing {
            if existing_attempt == attempt_key || existing_epoch >= epoch_order as i64 {
                return Ok(RecoveryReservationOutcome::Duplicate);
            }
            let cutoff = reserved_at_ms.saturating_sub(cooldown_ms) as i64;
            if existing_at > cutoff {
                return Ok(RecoveryReservationOutcome::Cooldown);
            }
            transaction.execute(
                "UPDATE recovery_reservations
                 SET attempt_key = ?1, provider = ?2, session_id = ?3, ephemeral = ?4,
                     epoch_order = ?5, reserved_at_ms = ?6
                 WHERE session_key = ?7",
                params![
                    attempt_key,
                    provider.as_str(),
                    session_id,
                    i64::from(ephemeral),
                    epoch_order as i64,
                    reserved_at_ms as i64,
                    session_key,
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO recovery_reservations (
                    session_key, attempt_key, provider, session_id, ephemeral,
                    epoch_order, reserved_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_key,
                    attempt_key,
                    provider.as_str(),
                    session_id,
                    i64::from(ephemeral),
                    epoch_order as i64,
                    reserved_at_ms as i64,
                ],
            )?;
        }
        commit_before_deadline(self.deadline, super::StorageOperation::Activity, || {
            transaction.commit()
        })?;
        Ok(RecoveryReservationOutcome::Reserved)
    }

    pub fn append_activity(
        &mut self,
        event: ActivityEvent,
    ) -> Result<ActivityCursor, StorageError> {
        self.append_activity_batch(&[event])?
            .into_iter()
            .next()
            .ok_or(StorageError::InvalidStorage(
                "activity append returned no cursor",
            ))
    }

    pub fn append_activity_batch(
        &mut self,
        events: &[ActivityEvent],
    ) -> Result<Vec<ActivityCursor>, StorageError> {
        self.append_activity_batch_inner(events).map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Activity, false, error)
        })
    }

    fn append_activity_batch_inner(
        &mut self,
        events: &[ActivityEvent],
    ) -> Result<Vec<ActivityCursor>, StorageError> {
        let prepared = events
            .iter()
            .cloned()
            .map(prepare_activity)
            .collect::<Result<Vec<_>, _>>()?;
        if prepared.is_empty() {
            return Ok(Vec::new());
        }

        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("activity-body")?;
        let current = validated_high_water(&transaction)?;
        let count = i64::try_from(prepared.len())
            .map_err(|_| StorageError::InvalidStorage("activity batch is too large"))?;
        let high_water = current
            .checked_add(count)
            .ok_or(StorageError::InvalidStorage(
                "activity cursor space is exhausted",
            ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [high_water],
        )?;
        let mut cursors = Vec::with_capacity(prepared.len());
        for (offset, activity) in prepared.iter().enumerate() {
            ensure_deadline(self.deadline)?;
            let offset = i64::try_from(offset + 1)
                .map_err(|_| StorageError::InvalidStorage("activity batch is too large"))?;
            let cursor = current
                .checked_add(offset)
                .ok_or(StorageError::InvalidStorage(
                    "activity cursor space is exhausted",
                ))?;
            insert_activity(&transaction, cursor, activity, None)?;
            cursors.push(ActivityCursor::try_from(cursor)?);
        }
        commit_before_deadline(self.deadline, super::StorageOperation::Activity, || {
            super::maintenance::sqlite_fault("activity-commit")?;
            transaction.commit()
        })?;
        Ok(cursors)
    }

    pub fn read_activity_page(
        &self,
        before: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        validate_page_limits(max_rows, max_bytes)?;
        apply_deadline(&self.connection, self.deadline)?;
        let row_limit = sql_query_limit(max_rows)?;
        let mut statement = match before {
            Some(_) => self.connection.prepare(
                "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                        terminal_provider, terminal_session_id, terminal_turn_id,
                        terminal_tool_use_id, terminal_action, outcome, correction, event_payload
                 FROM activity_events INDEXED BY activity_events_cursor
                 WHERE source_cursor < ?1
                 ORDER BY source_cursor DESC LIMIT ?2",
            )?,
            None => self.connection.prepare(
                "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                        terminal_provider, terminal_session_id, terminal_turn_id,
                        terminal_tool_use_id, terminal_action, outcome, correction, event_payload
                 FROM activity_events INDEXED BY activity_events_cursor
                 ORDER BY source_cursor DESC LIMIT ?1",
            )?,
        };
        let mut rows = match before {
            Some(cursor) => statement.query(params![cursor_i64(cursor), row_limit])?,
            None => statement.query([row_limit])?,
        };
        materialize_page(
            &self.connection,
            &mut rows,
            max_rows,
            max_bytes,
            self.deadline,
        )
    }

    pub fn activity_by_id(
        &self,
        activity_id: &str,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        validate_page_limits(max_rows, max_bytes)?;
        if activity_id.is_empty() || activity_id.len() > MAX_ACTIVITY_ID_BYTES {
            return Err(StorageError::InvalidStorage("activity ID is out of range"));
        }
        apply_deadline(&self.connection, self.deadline)?;
        let mut statement = self.connection.prepare(
            "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                    terminal_provider, terminal_session_id, terminal_turn_id,
                    terminal_tool_use_id, terminal_action, outcome, correction, event_payload
             FROM activity_events INDEXED BY activity_events_activity_id
             WHERE activity_id = ?1 AND source_cursor > ?2
             ORDER BY source_cursor ASC LIMIT ?3",
        )?;
        let mut rows = statement.query(params![
            activity_id,
            after.map_or(0, cursor_i64),
            sql_query_limit(max_rows)?
        ])?;
        materialize_page(
            &self.connection,
            &mut rows,
            max_rows,
            max_bytes,
            self.deadline,
        )
    }

    pub fn activity_for_permission_identity(
        &self,
        provider: AgentProvider,
        session_id: &str,
        turn_id: &str,
        tool_use_id: Option<&str>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        validate_page_limits(max_rows, max_bytes)?;
        apply_deadline(&self.connection, self.deadline)?;
        let mut statement = self.connection.prepare(
            "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                    terminal_provider, terminal_session_id, terminal_turn_id,
                    terminal_tool_use_id, terminal_action, outcome, correction, event_payload
             FROM activity_events INDEXED BY activity_events_permission_identity
             WHERE terminal_provider = ?1 AND terminal_session_id = ?2
               AND terminal_turn_id = ?3 AND terminal_tool_use_id IS ?4
             ORDER BY source_cursor DESC LIMIT ?5",
        )?;
        let mut rows = statement.query(params![
            provider.as_str(),
            session_id,
            turn_id,
            tool_use_id,
            sql_query_limit(max_rows)?
        ])?;
        materialize_page(
            &self.connection,
            &mut rows,
            max_rows,
            max_bytes,
            self.deadline,
        )
    }

    pub fn activity_for_permission_outcome(
        &self,
        provider: AgentProvider,
        session_id: &str,
        turn_id: &str,
        tool_use_id: &str,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        validate_page_limits(max_rows, max_bytes)?;
        apply_deadline(&self.connection, self.deadline)?;
        let mut statement = self.connection.prepare(
            "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                    terminal_provider, terminal_session_id, terminal_turn_id,
                    terminal_tool_use_id, terminal_action, outcome, correction, event_payload
             FROM activity_events INDEXED BY activity_events_permission_identity
             WHERE terminal_provider = ?1 AND terminal_session_id = ?2
               AND terminal_turn_id = ?3
               AND (terminal_tool_use_id = ?4 OR terminal_tool_use_id IS NULL)
             ORDER BY source_cursor DESC LIMIT ?5",
        )?;
        let mut rows = statement.query(params![
            provider.as_str(),
            session_id,
            turn_id,
            tool_use_id,
            sql_query_limit(max_rows)?
        ])?;
        materialize_page(
            &self.connection,
            &mut rows,
            max_rows,
            max_bytes,
            self.deadline,
        )
    }

    pub fn activity_after_cursor(
        &self,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        self.activity_bounded_after_locked(after, i64::MAX, max_rows, max_bytes)
    }

    pub(super) fn activity_bounded_after_locked(
        &self,
        after: Option<ActivityCursor>,
        through: i64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ActivityPage, StorageError> {
        validate_page_limits(max_rows, max_bytes)?;
        if through < 0 {
            return Err(StorageError::InvalidStorage(
                "activity page bounds are invalid",
            ));
        }
        apply_deadline(&self.connection, self.deadline)?;
        let after = after.map_or(0, cursor_i64);
        let mut statement = self.connection.prepare(
            "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                    terminal_provider, terminal_session_id, terminal_turn_id,
                    terminal_tool_use_id, terminal_action, outcome, correction, event_payload
             FROM activity_events INDEXED BY activity_events_cursor
             WHERE source_cursor > ?1 AND source_cursor <= ?2
             ORDER BY source_cursor ASC LIMIT ?3",
        )?;
        let mut rows = statement.query(params![after, through, sql_query_limit(max_rows)?])?;
        materialize_page(
            &self.connection,
            &mut rows,
            max_rows,
            max_bytes,
            self.deadline,
        )
    }

    pub fn activity_high_water(&self) -> Result<Option<ActivityCursor>, StorageError> {
        apply_deadline(&self.connection, self.deadline)?;
        let value = validated_high_water(&self.connection)?;
        if value == 0 {
            Ok(None)
        } else {
            ActivityCursor::try_from(value).map(Some)
        }
    }

    pub fn delete_activity_before(
        &mut self,
        cursor: ActivityCursor,
    ) -> Result<usize, StorageError> {
        self.delete_activity_before_inner(cursor).map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Activity, false, error)
        })
    }

    fn delete_activity_before_inner(
        &mut self,
        cursor: ActivityCursor,
    ) -> Result<usize, StorageError> {
        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("activity-delete-body")?;
        validated_high_water(&transaction)?;
        let deleted = transaction.execute(
            "DELETE FROM activity_events WHERE source_cursor < ?1",
            [cursor_i64(cursor)],
        )?;
        commit_before_deadline(self.deadline, super::StorageOperation::Activity, || {
            transaction.commit()
        })?;
        Ok(deleted)
    }

    pub fn explain_recent_activity(&self) -> Result<String, StorageError> {
        explain_query(
            &self.connection,
            "EXPLAIN QUERY PLAN
             SELECT source_cursor FROM activity_events INDEXED BY activity_events_cursor
             ORDER BY source_cursor DESC LIMIT 100",
            self.deadline,
        )
    }

    pub fn explain_activity_by_id(&self) -> Result<String, StorageError> {
        explain_query(
            &self.connection,
            "EXPLAIN QUERY PLAN
             SELECT source_cursor FROM activity_events INDEXED BY activity_events_activity_id
             WHERE activity_id = 'query-plan-probe' ORDER BY source_cursor ASC LIMIT 100",
            self.deadline,
        )
    }

    pub fn explain_activity_after_cursor(&self) -> Result<String, StorageError> {
        explain_query(
            &self.connection,
            "EXPLAIN QUERY PLAN
             SELECT source_cursor FROM activity_events INDEXED BY activity_events_cursor
             WHERE source_cursor > 0 ORDER BY source_cursor ASC LIMIT 100",
            self.deadline,
        )
    }
}

pub(super) fn prepare_activity(event: ActivityEvent) -> Result<PreparedActivity, StorageError> {
    if event.schema_version != ACTIVITY_SCHEMA_VERSION {
        return Err(StorageError::InvalidStorage(
            "unsupported activity payload schema",
        ));
    }
    let event = event.normalized();
    if !event.has_consistent_payload() {
        return Err(StorageError::InvalidStorage(
            "inconsistent activity payload",
        ));
    }
    if event.activity_id.is_empty() || event.activity_id.len() > MAX_ACTIVITY_ID_BYTES {
        return Err(StorageError::InvalidStorage("activity ID is out of range"));
    }
    let recorded_at_ms = i64::try_from(event.recorded_at_ms)
        .map_err(|_| StorageError::InvalidStorage("activity timestamp is out of range"))?;
    let payload = serde_json::to_vec(&event)
        .map_err(|_| StorageError::InvalidStorage("activity payload cannot be serialized"))?;
    if payload.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(StorageError::InvalidStorage(
            "activity payload is too large",
        ));
    }
    let terminal = terminal_identity(&event);
    Ok(PreparedActivity {
        event_kind: activity_kind(event.kind),
        event_state: activity_state(event.state),
        outcome: event.outcome.map(activity_outcome),
        correction: event.correction.map(correction_disposition),
        event,
        payload,
        recorded_at_ms,
        terminal,
    })
}

pub(super) fn validated_high_water(connection: &Connection) -> Result<i64, StorageError> {
    let (high_water, retained_max) = connection.query_row(
        "SELECT activity_high_water, (SELECT max(source_cursor) FROM activity_events)
         FROM schema_meta WHERE singleton = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
    )?;
    if high_water < 0 || retained_max.is_some_and(|cursor| cursor > high_water) {
        return Err(StorageError::InvalidStorage(
            "activity high-water is below retained activity",
        ));
    }
    Ok(high_water)
}

fn terminal_identity(event: &ActivityEvent) -> Option<TerminalIdentity> {
    let action = match event.state {
        ActivityState::Allowed => "allow",
        ActivityState::Denied => "deny",
        _ => return None,
    };
    let session = event.session.as_ref()?;
    Some(TerminalIdentity {
        provider: session.provider.as_str(),
        session_id: session.session_id.clone(),
        turn_id: session.turn_id.clone()?,
        tool_use_id: session.tool_use_id.clone(),
        action,
    })
}

pub(super) fn insert_activity(
    transaction: &Transaction<'_>,
    cursor: i64,
    activity: &PreparedActivity,
    permission_attempt_id: Option<&str>,
) -> Result<(), StorageError> {
    let terminal = activity.terminal.as_ref();
    let inserted = transaction.execute(
        "INSERT INTO activity_events (
            source_cursor, activity_id, event_kind, event_state, recorded_at_ms, permission_attempt_id,
            terminal_provider, terminal_session_id, terminal_turn_id,
            terminal_tool_use_id, terminal_action, outcome, correction, event_payload
         )
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         WHERE NOT EXISTS (
             SELECT 1 FROM activity_events
             WHERE activity_id = ?2 AND event_kind <> ?3
         )",
        params![
            cursor,
            activity.event.activity_id,
            activity.event_kind,
            activity.event_state,
            activity.recorded_at_ms,
            permission_attempt_id,
            terminal.map(|value| value.provider),
            terminal.map(|value| value.session_id.as_str()),
            terminal.map(|value| value.turn_id.as_str()),
            terminal.and_then(|value| value.tool_use_id.as_deref()),
            terminal.map(|value| value.action),
            activity.outcome,
            activity.correction,
            activity.payload,
        ],
    )?;
    if inserted != 1 {
        return Err(StorageError::InvalidStorage(
            "one logical activity cannot mix event kinds",
        ));
    }
    Ok(())
}

fn materialize_page(
    connection: &Connection,
    rows: &mut rusqlite::Rows<'_>,
    max_rows: usize,
    max_bytes: usize,
    deadline: Option<StorageDeadline>,
) -> Result<ActivityPage, StorageError> {
    let mut page = ActivityPage::default();
    let mut validated_activity_ids = HashSet::new();
    while let Some(row) = rows.next()? {
        ensure_deadline(deadline)?;
        if page.events.len() == max_rows {
            page.next_cursor = page.events.last().map(|record| record.cursor);
            break;
        }
        let payload = row.get::<_, Vec<u8>>(12)?;
        let next_bytes = page.serialized_bytes.checked_add(payload.len()).ok_or(
            StorageError::InvalidStorage("activity page byte count overflowed"),
        )?;
        if next_bytes > max_bytes {
            if page.events.is_empty() {
                return Err(StorageError::InvalidStorage(
                    "activity byte limit cannot hold the next event",
                ));
            }
            page.next_cursor = page.events.last().map(|record| record.cursor);
            break;
        }
        let record = decode_activity_row(row, &payload)?;
        if validated_activity_ids.insert(record.event.activity_id.clone()) {
            ensure_single_activity_kind(connection, &record.event, deadline)?;
        }
        page.events.push(record);
        page.serialized_bytes = next_bytes;
    }
    ensure_deadline(deadline)?;
    Ok(page)
}

fn decode_activity_row(row: &Row<'_>, payload: &[u8]) -> Result<ActivityRecord, StorageError> {
    if payload.len() > MAX_ACTIVITY_EVENT_BYTES {
        return Err(StorageError::InvalidStorage(
            "activity payload is too large",
        ));
    }
    let event: ActivityEvent = serde_json::from_slice(payload)
        .map_err(|_| StorageError::InvalidStorage("activity payload is malformed"))?;
    if event.schema_version != ACTIVITY_SCHEMA_VERSION
        || !event.has_consistent_payload()
        || event.clone().normalized() != event
    {
        return Err(StorageError::InvalidStorage(
            "activity payload is unsupported or invalid",
        ));
    }
    let prepared = prepare_activity(event.clone())?;
    let cursor = ActivityCursor::try_from(row.get::<_, i64>(0)?)?;
    let typed = (
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, i64>(4)?,
        row.get::<_, Option<String>>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, Option<String>>(7)?,
        row.get::<_, Option<String>>(8)?,
        row.get::<_, Option<String>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, Option<String>>(11)?,
    );
    let terminal = prepared.terminal.as_ref();
    let expected = (
        prepared.event.activity_id.clone(),
        prepared.event_kind.to_owned(),
        prepared.event_state.to_owned(),
        prepared.recorded_at_ms,
        terminal.map(|value| value.provider.to_owned()),
        terminal.map(|value| value.session_id.clone()),
        terminal.map(|value| value.turn_id.clone()),
        terminal.and_then(|value| value.tool_use_id.clone()),
        terminal.map(|value| value.action.to_owned()),
        prepared.outcome.map(str::to_owned),
        prepared.correction.map(str::to_owned),
    );
    if typed != expected {
        return Err(StorageError::InvalidStorage(
            "activity typed columns disagree with payload",
        ));
    }
    Ok(ActivityRecord { cursor, event })
}

pub(super) fn validated_activity_at(
    connection: &Connection,
    cursor: ActivityCursor,
) -> Result<ActivityRecord, StorageError> {
    let mut statement = connection.prepare(
        "SELECT source_cursor, activity_id, event_kind, event_state, recorded_at_ms,
                    terminal_provider, terminal_session_id, terminal_turn_id,
                    terminal_tool_use_id, terminal_action, outcome, correction,
                    CASE WHEN length(event_payload) <= ?2 THEN event_payload END
             FROM activity_events WHERE source_cursor = ?1",
    )?;
    let mut rows = statement.query(params![cursor_i64(cursor), MAX_ACTIVITY_EVENT_BYTES as i64])?;
    let row = rows.next()?.ok_or(StorageError::InvalidStorage(
        "decision source activity is absent",
    ))?;
    let payload = row
        .get::<_, Option<Vec<u8>>>(12)?
        .ok_or(StorageError::InvalidStorage(
            "activity payload is too large",
        ))?;
    decode_activity_row(row, &payload)
}

fn validate_page_limits(max_rows: usize, max_bytes: usize) -> Result<(), StorageError> {
    if max_rows == 0 || max_bytes == 0 {
        Err(StorageError::InvalidStorage(
            "activity page limits must be positive",
        ))
    } else {
        Ok(())
    }
}

fn sql_query_limit(max_rows: usize) -> Result<i64, StorageError> {
    i64::try_from(max_rows.checked_add(1).ok_or(StorageError::InvalidStorage(
        "activity row limit is out of range",
    ))?)
    .map_err(|_| StorageError::InvalidStorage("activity row limit is out of range"))
}

fn cursor_i64(cursor: ActivityCursor) -> i64 {
    i64::try_from(cursor.get()).expect("ActivityCursor is restricted to the SQLite integer range")
}

pub(super) fn ensure_deadline(deadline: Option<StorageDeadline>) -> Result<(), StorageError> {
    deadline.map_or(Ok(()), StorageDeadline::ensure_remaining)
}

pub(super) fn commit_before_deadline<T>(
    deadline: Option<StorageDeadline>,
    operation: super::StorageOperation,
    commit: impl FnOnce() -> rusqlite::Result<T>,
) -> Result<T, StorageError> {
    ensure_deadline(deadline)?;
    // The deadline gates entry to commit. Once SQLite reports commit success, that
    // durable result is authoritative even if the wall-clock deadline has crossed.
    commit().map_err(|error| super::maintenance::map_sqlite_error(operation, true, error))
}

fn ensure_single_activity_kind(
    connection: &Connection,
    event: &ActivityEvent,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    ensure_deadline(deadline)?;
    let conflict = connection
        .query_row(
            "SELECT 1 FROM activity_events INDEXED BY activity_events_activity_id
             WHERE activity_id = ?1 AND event_kind <> ?2 LIMIT 1",
            params![event.activity_id, activity_kind(event.kind)],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    ensure_deadline(deadline)?;
    if conflict.is_some() {
        Err(StorageError::InvalidStorage(
            "one logical activity contains mixed event kinds",
        ))
    } else {
        Ok(())
    }
}

pub(super) fn apply_deadline(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    match deadline {
        Some(deadline) => deadline.apply(connection),
        None => Ok(()),
    }
}

pub(super) fn explain_query(
    connection: &Connection,
    sql: &str,
    deadline: Option<StorageDeadline>,
) -> Result<String, StorageError> {
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(sql)?;
    let plans = statement
        .query_map([], |row| row.get::<_, String>(3))?
        .collect::<Result<Vec<_>, _>>()?;
    ensure_deadline(deadline)?;
    Ok(plans.join("\n"))
}

fn activity_kind(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::Decision => "decision",
        ActivityKind::Lifecycle => "lifecycle",
        ActivityKind::Diagnostic => "diagnostic",
    }
}

fn activity_state(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Observed => "observed",
        ActivityState::Evaluating => "evaluating",
        ActivityState::Allowed => "allowed",
        ActivityState::Denied => "denied",
        ActivityState::Abstained => "abstained",
        ActivityState::Error => "error",
        ActivityState::Delivered => "delivered",
        ActivityState::DeliveryFailed => "delivery_failed",
        ActivityState::Outcome => "outcome",
        ActivityState::Correction => "correction",
        ActivityState::Interrupted => "interrupted",
        ActivityState::Incomplete => "incomplete",
    }
}

fn activity_outcome(outcome: ActivityOutcome) -> &'static str {
    match outcome {
        ActivityOutcome::Succeeded => "succeeded",
        ActivityOutcome::Failed => "failed",
        ActivityOutcome::Cancelled => "cancelled",
        ActivityOutcome::Completed => "completed",
    }
}

fn correction_disposition(correction: CorrectionDisposition) -> &'static str {
    match correction {
        CorrectionDisposition::BrainRight => "brain_right",
        CorrectionDisposition::BrainWrong => "brain_wrong",
        CorrectionDisposition::Exception => "exception",
    }
}
