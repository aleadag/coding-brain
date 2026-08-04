use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use coding_brain_core::lifecycle::{
    ActiveSubagentState, IgnoreReason, LIFECYCLE_SCHEMA_VERSION, LifecycleEvent,
    LifecycleEventKind, LifecycleEventName, LifecycleEventSignature, LifecycleSnapshot,
    MAX_ACTIVE_SUBAGENTS, MAX_ANTIGRAVITY_INVOCATION_STEPS, MAX_RECENT_TURNS, ProjectedStatus,
    RecordedLifecycleEvent, SessionLifecycleState, SessionStartSource, StoppedSubagentState,
};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{BrainDb, StorageDeadline, StorageError};

const MAX_SESSIONS: usize = 128;
const PRE_TOOL_BIT: u8 = 1 << 2;
const POST_TOOL_BIT: u8 = 1 << 3;

#[derive(Debug)]
struct SessionRow {
    provider: String,
    session_id: String,
    cwd: Vec<u8>,
    transcript_path: Option<Vec<u8>>,
    provider_session_id: Option<String>,
    latest_event: String,
    latest_sequence: i64,
    latest_received_at_ms: i64,
    session_start_source: Option<String>,
    ignored_reason: Option<String>,
    signature_event: Option<String>,
    signature_turn_id: Option<String>,
    signature_detail_id: Option<String>,
    signature_session_start_source: Option<String>,
}

impl BrainDb {
    pub fn read_lifecycle(&self) -> Result<LifecycleSnapshot, StorageError> {
        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self.connection.unchecked_transaction()?;
        let snapshot = load_lifecycle_snapshot(&transaction, self.deadline)?;
        transaction.commit()?;
        ensure_deadline(self.deadline)?;
        Ok(snapshot)
    }

    pub fn record_lifecycle(
        &mut self,
        event: LifecycleEvent,
        received_at_ms: u64,
    ) -> Result<RecordedLifecycleEvent, StorageError> {
        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut snapshot = load_lifecycle_snapshot(&transaction, self.deadline)?;
        let recorded = snapshot.record_at(event, received_at_ms);
        snapshot.remove_permission_state();
        validate_snapshot(&snapshot)?;
        persist_lifecycle_snapshot(&transaction, &snapshot, self.deadline)?;
        ensure_deadline(self.deadline)?;
        transaction.commit()?;
        ensure_deadline(self.deadline)?;
        Ok(recorded)
    }
}

fn ensure_deadline(deadline: Option<StorageDeadline>) -> Result<(), StorageError> {
    deadline.map_or(Ok(()), StorageDeadline::ensure_remaining)
}

fn apply_deadline(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    match deadline {
        Some(deadline) => deadline.apply(connection),
        None => Ok(()),
    }
}

fn load_lifecycle_snapshot(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<LifecycleSnapshot, StorageError> {
    apply_deadline(connection, deadline)?;
    let next_sequence = connection
        .query_row(
            "SELECT next_sequence FROM lifecycle_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or(StorageError::InvalidStorage("missing lifecycle metadata"))?;
    let next_sequence = positive_u64(next_sequence, "invalid lifecycle sequence")?;

    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(
        "SELECT provider, session_id, cwd, transcript_path, provider_session_id,
                latest_event, latest_sequence, latest_received_at_ms,
                session_start_source, ignored_reason, signature_event,
                signature_turn_id, signature_detail_id, signature_session_start_source
         FROM lifecycle_sessions
         ORDER BY provider, session_id
         LIMIT 129",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok(SessionRow {
                provider: row.get(0)?,
                session_id: row.get(1)?,
                cwd: row.get(2)?,
                transcript_path: row.get(3)?,
                provider_session_id: row.get(4)?,
                latest_event: row.get(5)?,
                latest_sequence: row.get(6)?,
                latest_received_at_ms: row.get(7)?,
                session_start_source: row.get(8)?,
                ignored_reason: row.get(9)?,
                signature_event: row.get(10)?,
                signature_turn_id: row.get(11)?,
                signature_detail_id: row.get(12)?,
                signature_session_start_source: row.get(13)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > MAX_SESSIONS {
        return Err(StorageError::InvalidStorage("too many lifecycle sessions"));
    }

    let mut sessions = BTreeMap::new();
    for row in rows {
        ensure_deadline(deadline)?;
        let provider = parse_provider(&row.provider)?;
        let latest_event = parse_event_name(&row.latest_event)?;
        let session_start_source = row
            .session_start_source
            .as_deref()
            .map(parse_start_source)
            .transpose()?;
        let last_signature = parse_signature(
            row.signature_event.as_deref(),
            row.signature_turn_id,
            row.signature_detail_id,
            row.signature_session_start_source.as_deref(),
        )?;
        let state = SessionLifecycleState {
            cwd: path_from_bytes(row.cwd)?,
            transcript_path: row.transcript_path.map(path_from_bytes).transpose()?,
            provider_session_id: row.provider_session_id,
            current_turn: None,
            turn_open: false,
            recent_turns: VecDeque::new(),
            latest_event: Some(latest_event),
            latest_sequence: positive_u64(row.latest_sequence, "invalid session sequence")?,
            latest_received_at_ms: nonnegative_u64(
                row.latest_received_at_ms,
                "invalid session timestamp",
            )?,
            status_event: None,
            status_sequence: None,
            status_received_at_ms: None,
            projected_status: None,
            active_subagents: BTreeMap::new(),
            stopped_subagents: BTreeMap::new(),
            session_start_source,
            ignored_reason: row
                .ignored_reason
                .as_deref()
                .map(parse_ignore_reason)
                .transpose()?,
            antigravity_initial_step: None,
            antigravity_child_events: BTreeMap::new(),
            antigravity_permission_requests: BTreeMap::new(),
            permission_request_events: BTreeMap::new(),
            permission_authorities: BTreeMap::new(),
            last_signature,
        };
        let key = AgentSessionKey::native(provider, row.session_id).storage_key();
        if sessions.insert(key, state).is_some() {
            return Err(StorageError::InvalidStorage("duplicate lifecycle session"));
        }
    }
    drop(statement);

    load_leases(connection, &mut sessions, deadline)?;
    load_turns(connection, &mut sessions, deadline)?;
    load_subagents(connection, &mut sessions, deadline)?;
    load_invocations(connection, &mut sessions, next_sequence, deadline)?;

    let snapshot = LifecycleSnapshot {
        schema_version: LIFECYCLE_SCHEMA_VERSION,
        next_sequence,
        sessions,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

fn load_leases(
    connection: &Connection,
    sessions: &mut BTreeMap<String, SessionLifecycleState>,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(
        "SELECT provider, session_id, status_event, status_sequence,
                status_received_at_ms, projected_status
         FROM lifecycle_leases ORDER BY provider, session_id LIMIT 129",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > sessions.len() {
        return Err(StorageError::InvalidStorage("too many lifecycle leases"));
    }
    for (provider, session_id, event, sequence, received_at_ms, status) in rows {
        ensure_deadline(deadline)?;
        let state = session_mut(sessions, &provider, &session_id)?;
        state.status_event = Some(parse_event_name(&event)?);
        state.status_sequence = Some(positive_u64(sequence, "invalid lease sequence")?);
        state.status_received_at_ms =
            Some(nonnegative_u64(received_at_ms, "invalid lease timestamp")?);
        state.projected_status = Some(parse_status(&status)?);
    }
    Ok(())
}

fn load_turns(
    connection: &Connection,
    sessions: &mut BTreeMap<String, SessionLifecycleState>,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    let limit = MAX_SESSIONS * (MAX_RECENT_TURNS + 1) + 1;
    let sql = format!(
        "SELECT provider, session_id, continuity_state, turn_id, turn_open, recent_position
         FROM lifecycle_turns
         ORDER BY provider, session_id, continuity_state, recent_position
         LIMIT {limit}"
    );
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() == limit {
        return Err(StorageError::InvalidStorage("too many lifecycle turns"));
    }
    for (provider, session_id, continuity, turn_id, turn_open, recent_position) in rows {
        ensure_deadline(deadline)?;
        let state = session_mut(sessions, &provider, &session_id)?;
        match continuity.as_str() {
            "current" if recent_position.is_none() && state.current_turn.is_none() => {
                state.current_turn = Some(turn_id);
                state.turn_open = parse_bool(turn_open)?;
            }
            "recent" => {
                let position = recent_position
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or(StorageError::InvalidStorage("invalid recent turn position"))?;
                if position != state.recent_turns.len() || turn_open != 0 {
                    return Err(StorageError::InvalidStorage("invalid recent turn ordering"));
                }
                state.recent_turns.push_back(turn_id);
            }
            _ => return Err(StorageError::InvalidStorage("invalid lifecycle turn")),
        }
    }
    Ok(())
}

fn load_subagents(
    connection: &Connection,
    sessions: &mut BTreeMap<String, SessionLifecycleState>,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    let limit = MAX_SESSIONS * MAX_ACTIVE_SUBAGENTS * 2 + 1;
    let sql = format!(
        "SELECT provider, parent_session_id, agent_id, turn_id, subagent_state,
                topology_slot, state_sequence, received_at_ms
         FROM lifecycle_subagents
         ORDER BY provider, parent_session_id, subagent_state, topology_slot
         LIMIT {limit}"
    );
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() == limit {
        return Err(StorageError::InvalidStorage("too many lifecycle subagents"));
    }
    let mut expected_slots = BTreeMap::<(String, String, String), usize>::new();
    for (provider, parent, agent, turn, state_name, slot, sequence, received_at) in rows {
        ensure_deadline(deadline)?;
        let expected = expected_slots
            .entry((provider.clone(), parent.clone(), state_name.clone()))
            .or_default();
        if usize::try_from(slot).ok() != Some(*expected) {
            return Err(StorageError::InvalidStorage(
                "invalid subagent topology ordering",
            ));
        }
        *expected += 1;
        let state = session_mut(sessions, &provider, &parent)?;
        let sequence = positive_u64(sequence, "invalid subagent sequence")?;
        let received_at_ms = nonnegative_u64(received_at, "invalid subagent timestamp")?;
        match state_name.as_str() {
            "active" => {
                state.active_subagents.insert(
                    agent,
                    ActiveSubagentState {
                        started_sequence: sequence,
                        received_at_ms,
                        turn_id: turn,
                    },
                );
            }
            "stopped" => {
                state.stopped_subagents.insert(
                    agent,
                    StoppedSubagentState {
                        stopped_sequence: sequence,
                        received_at_ms,
                        turn_id: turn,
                    },
                );
            }
            _ => return Err(StorageError::InvalidStorage("invalid subagent state")),
        }
    }
    Ok(())
}

fn load_invocations(
    connection: &Connection,
    sessions: &mut BTreeMap<String, SessionLifecycleState>,
    next_sequence: u64,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(
        "SELECT provider, session_id, invocation_id, invocation_state, initial_step,
                state_sequence, received_at_ms
         FROM lifecycle_invocations ORDER BY provider, session_id LIMIT 129",
    )?;
    let invocations = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if invocations.len() > sessions.len() {
        return Err(StorageError::InvalidStorage(
            "too many lifecycle invocations",
        ));
    }
    let mut known = BTreeSet::new();
    for (provider, session_id, invocation_id, invocation_state, initial, sequence, received) in
        invocations
    {
        ensure_deadline(deadline)?;
        if invocation_state != "active" {
            return Err(StorageError::InvalidStorage(
                "unsupported stopped invocation",
            ));
        }
        let state = session_mut(sessions, &provider, &session_id)?;
        if provider != "antigravity"
            || !state.turn_open
            || state.current_turn.as_deref() != Some(invocation_id.as_str())
            || sequence <= 0
            || sequence as u64 >= next_sequence
            || sequence as u64 > state.latest_sequence
        {
            return Err(StorageError::InvalidStorage(
                "mismatched lifecycle invocation",
            ));
        }
        state.antigravity_initial_step = Some(nonnegative_u64(
            initial.ok_or(StorageError::InvalidStorage(
                "missing invocation initial step",
            ))?,
            "invalid invocation initial step",
        )?);
        positive_u64(sequence, "invalid invocation sequence")?;
        nonnegative_u64(received, "invalid invocation timestamp")?;
        known.insert((provider, session_id, invocation_id));
    }
    drop(statement);

    let limit = MAX_SESSIONS * MAX_ANTIGRAVITY_INVOCATION_STEPS + 1;
    let sql = format!(
        "SELECT provider, session_id, invocation_id, step, step_slot,
                pre_tool_seen, post_tool_seen
         FROM lifecycle_invocation_steps
         ORDER BY provider, session_id, invocation_id, step_slot LIMIT {limit}"
    );
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(&sql)?;
    let steps = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if steps.len() == limit {
        return Err(StorageError::InvalidStorage("too many invocation steps"));
    }
    let mut expected_slots = BTreeMap::<(String, String, String), usize>::new();
    for (provider, session_id, invocation_id, step, slot, pre, post) in steps {
        ensure_deadline(deadline)?;
        let key = (provider.clone(), session_id.clone(), invocation_id.clone());
        if !known.contains(&key) {
            return Err(StorageError::InvalidStorage("orphan invocation step"));
        }
        let expected = expected_slots.entry(key).or_default();
        if usize::try_from(slot).ok() != Some(*expected) {
            return Err(StorageError::InvalidStorage(
                "invalid invocation step ordering",
            ));
        }
        *expected += 1;
        let mut bits = 0;
        if parse_bool(pre)? {
            bits |= PRE_TOOL_BIT;
        }
        if parse_bool(post)? {
            bits |= POST_TOOL_BIT;
        }
        if bits == 0 {
            return Err(StorageError::InvalidStorage("empty invocation step"));
        }
        session_mut(sessions, &provider, &session_id)?
            .antigravity_child_events
            .insert(nonnegative_u64(step, "invalid invocation step")?, bits);
    }
    Ok(())
}

fn persist_lifecycle_snapshot(
    transaction: &Transaction<'_>,
    snapshot: &LifecycleSnapshot,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    apply_deadline(transaction, deadline)?;
    transaction.execute_batch(
        "DELETE FROM lifecycle_invocation_steps;
         DELETE FROM lifecycle_invocations;
         DELETE FROM lifecycle_subagents;
         DELETE FROM lifecycle_turns;
         DELETE FROM lifecycle_leases;
         DELETE FROM lifecycle_sessions;",
    )?;

    for storage_key in ordered_session_keys(snapshot)? {
        apply_deadline(transaction, deadline)?;
        let state = &snapshot.sessions[storage_key];
        let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
            StorageError::InvalidStorage("invalid lifecycle session key"),
        )?;
        let latest_event = state.latest_event.ok_or(StorageError::InvalidStorage(
            "missing latest lifecycle event",
        ))?;
        let (signature_event, signature_turn, signature_detail, signature_source) =
            signature_columns(state.last_signature.as_ref())?;
        transaction.execute(
            "INSERT INTO lifecycle_sessions (
                provider, session_id, cwd, transcript_path, provider_session_id,
                latest_event, latest_sequence, latest_received_at_ms,
                session_start_source, ignored_reason, signature_event,
                signature_turn_id, signature_detail_id, signature_session_start_source
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                key.provider.as_str(),
                key.session_id,
                state.cwd.as_os_str().as_bytes(),
                state
                    .transcript_path
                    .as_deref()
                    .map(|path| path.as_os_str().as_bytes()),
                state.provider_session_id,
                event_name_db(latest_event),
                sqlite_i64(state.latest_sequence, "lifecycle sequence overflow")?,
                sqlite_i64(state.latest_received_at_ms, "lifecycle timestamp overflow")?,
                state.session_start_source.map(start_source_db),
                state.ignored_reason.map(ignore_reason_db),
                signature_event,
                signature_turn,
                signature_detail,
                signature_source,
            ],
        )?;
    }

    for (storage_key, state) in &snapshot.sessions {
        apply_deadline(transaction, deadline)?;
        let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
            StorageError::InvalidStorage("invalid lifecycle session key"),
        )?;
        if let (Some(event), Some(sequence), Some(received_at), Some(status)) = (
            state.status_event,
            state.status_sequence,
            state.status_received_at_ms,
            state.projected_status,
        ) {
            apply_deadline(transaction, deadline)?;
            transaction.execute(
                "INSERT INTO lifecycle_leases (
                    provider, session_id, status_event, status_sequence,
                    status_received_at_ms, projected_status
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    key.provider.as_str(),
                    key.session_id,
                    event_name_db(event),
                    sqlite_i64(sequence, "lease sequence overflow")?,
                    sqlite_i64(received_at, "lease timestamp overflow")?,
                    status_db(status),
                ],
            )?;
        }
        if let Some(turn_id) = state.current_turn.as_deref() {
            apply_deadline(transaction, deadline)?;
            transaction.execute(
                "INSERT INTO lifecycle_turns (
                    provider, session_id, continuity_state, turn_id, turn_open, recent_position
                 ) VALUES (?1, ?2, 'current', ?3, ?4, NULL)",
                params![
                    key.provider.as_str(),
                    key.session_id,
                    turn_id,
                    i64::from(state.turn_open),
                ],
            )?;
        }
        for (position, turn_id) in state.recent_turns.iter().enumerate() {
            apply_deadline(transaction, deadline)?;
            transaction.execute(
                "INSERT INTO lifecycle_turns (
                    provider, session_id, continuity_state, turn_id, turn_open, recent_position
                 ) VALUES (?1, ?2, 'recent', ?3, 0, ?4)",
                params![
                    key.provider.as_str(),
                    key.session_id,
                    turn_id,
                    i64::try_from(position)
                        .map_err(|_| StorageError::InvalidStorage("turn position overflow"))?,
                ],
            )?;
        }
        for (slot, (agent_id, subagent)) in state.active_subagents.iter().enumerate() {
            apply_deadline(transaction, deadline)?;
            insert_subagent(
                transaction,
                key.provider,
                &key.session_id,
                agent_id,
                &subagent.turn_id,
                "active",
                slot,
                subagent.started_sequence,
                subagent.received_at_ms,
            )?;
        }
        for (slot, (agent_id, subagent)) in state.stopped_subagents.iter().enumerate() {
            apply_deadline(transaction, deadline)?;
            insert_subagent(
                transaction,
                key.provider,
                &key.session_id,
                agent_id,
                &subagent.turn_id,
                "stopped",
                slot,
                subagent.stopped_sequence,
                subagent.received_at_ms,
            )?;
        }
        if let Some(initial_step) = state.antigravity_initial_step {
            let invocation_id =
                state
                    .current_turn
                    .as_deref()
                    .ok_or(StorageError::InvalidStorage(
                        "invocation missing current lifecycle turn",
                    ))?;
            apply_deadline(transaction, deadline)?;
            transaction.execute(
                "INSERT INTO lifecycle_invocations (
                    provider, session_id, invocation_id, invocation_state,
                    initial_step, state_sequence, received_at_ms
                 ) VALUES (?1, ?2, ?3, 'active', ?4, ?5, ?6)",
                params![
                    key.provider.as_str(),
                    key.session_id,
                    invocation_id,
                    sqlite_i64(initial_step, "invocation initial step overflow")?,
                    sqlite_i64(state.latest_sequence, "invocation sequence overflow")?,
                    sqlite_i64(state.latest_received_at_ms, "invocation timestamp overflow")?,
                ],
            )?;
            for (slot, (step, bits)) in state.antigravity_child_events.iter().enumerate() {
                apply_deadline(transaction, deadline)?;
                transaction.execute(
                    "INSERT INTO lifecycle_invocation_steps (
                        provider, session_id, invocation_id, step, step_slot,
                        pre_tool_seen, post_tool_seen
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        key.provider.as_str(),
                        key.session_id,
                        invocation_id,
                        sqlite_i64(*step, "invocation step overflow")?,
                        i64::try_from(slot).map_err(|_| {
                            StorageError::InvalidStorage("invocation step slot overflow")
                        })?,
                        i64::from(bits & PRE_TOOL_BIT != 0),
                        i64::from(bits & POST_TOOL_BIT != 0),
                    ],
                )?;
            }
        }
    }

    apply_deadline(transaction, deadline)?;
    transaction.execute(
        "UPDATE lifecycle_meta SET next_sequence = ?1 WHERE singleton = 1",
        [sqlite_i64(
            snapshot.next_sequence,
            "lifecycle sequence overflow",
        )?],
    )?;
    ensure_deadline(deadline)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_subagent(
    transaction: &Transaction<'_>,
    provider: AgentProvider,
    parent_session_id: &str,
    agent_id: &str,
    turn_id: &str,
    state: &str,
    slot: usize,
    sequence: u64,
    received_at_ms: u64,
) -> Result<(), StorageError> {
    transaction.execute(
        "INSERT INTO lifecycle_subagents (
            provider, parent_session_id, agent_id, turn_id, subagent_state,
            topology_slot, state_sequence, received_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            provider.as_str(),
            parent_session_id,
            agent_id,
            turn_id,
            state,
            i64::try_from(slot)
                .map_err(|_| StorageError::InvalidStorage("subagent slot overflow"))?,
            sqlite_i64(sequence, "subagent sequence overflow")?,
            sqlite_i64(received_at_ms, "subagent timestamp overflow")?,
        ],
    )?;
    Ok(())
}

fn ordered_session_keys(snapshot: &LifecycleSnapshot) -> Result<Vec<&str>, StorageError> {
    let mut ordered = Vec::with_capacity(snapshot.sessions.len());
    let mut inserted = BTreeSet::new();
    while ordered.len() < snapshot.sessions.len() {
        let before = ordered.len();
        for (storage_key, state) in &snapshot.sessions {
            if inserted.contains(storage_key) {
                continue;
            }
            let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
                StorageError::InvalidStorage("invalid lifecycle session key"),
            )?;
            let parent_ready = state.provider_session_id.as_deref().is_none_or(|parent| {
                inserted.contains(&AgentSessionKey::native(key.provider, parent).storage_key())
            });
            if parent_ready {
                inserted.insert(storage_key.clone());
                ordered.push(storage_key.as_str());
            }
        }
        if ordered.len() == before {
            return Err(StorageError::InvalidStorage("cyclic lifecycle topology"));
        }
    }
    Ok(ordered)
}

fn validate_snapshot(snapshot: &LifecycleSnapshot) -> Result<(), StorageError> {
    if snapshot.schema_version != LIFECYCLE_SCHEMA_VERSION
        || snapshot.next_sequence == 0
        || snapshot.next_sequence > i64::MAX as u64
        || snapshot.sessions.len() > MAX_SESSIONS
    {
        return Err(StorageError::InvalidStorage(
            "invalid lifecycle snapshot metadata",
        ));
    }
    let mut child_owners = BTreeMap::<(AgentProvider, String), String>::new();
    for (storage_key, state) in &snapshot.sessions {
        let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
            StorageError::InvalidStorage("invalid lifecycle session key"),
        )?;
        validate_id(&key.session_id)?;
        validate_path(&state.cwd)?;
        if let Some(path) = state.transcript_path.as_deref() {
            validate_path(path)?;
        }
        if state.latest_event.is_none()
            || state.latest_sequence == 0
            || state.latest_sequence >= snapshot.next_sequence
            || state.latest_received_at_ms > i64::MAX as u64
            || state.recent_turns.len() > MAX_RECENT_TURNS
            || state.active_subagents.len() > MAX_ACTIVE_SUBAGENTS
            || state.stopped_subagents.len() > MAX_ACTIVE_SUBAGENTS
            || !state.permission_request_events.is_empty()
            || !state.permission_authorities.is_empty()
            || !state.antigravity_permission_requests.is_empty()
        {
            return Err(StorageError::InvalidStorage(
                "invalid lifecycle session state",
            ));
        }
        validate_session_signature(state)?;
        validate_turns(state)?;
        validate_lease(state, snapshot.next_sequence)?;
        if !state.stopped_subagents.is_empty() && key.provider != AgentProvider::Codex {
            return Err(StorageError::InvalidStorage(
                "invalid stopped subagent provider",
            ));
        }
        for (agent_id, sequence, received_at, turn_id) in state
            .active_subagents
            .iter()
            .map(|(id, value)| {
                (
                    id,
                    value.started_sequence,
                    value.received_at_ms,
                    value.turn_id.as_str(),
                )
            })
            .chain(state.stopped_subagents.iter().map(|(id, value)| {
                (
                    id,
                    value.stopped_sequence,
                    value.received_at_ms,
                    value.turn_id.as_str(),
                )
            }))
        {
            validate_id(agent_id)?;
            validate_id(turn_id)?;
            if key.provider == AgentProvider::Codex && agent_id == &key.session_id {
                return Err(StorageError::InvalidStorage(
                    "invalid subagent self ownership",
                ));
            }
            if sequence == 0
                || sequence >= snapshot.next_sequence
                || sequence > state.latest_sequence
                || received_at > i64::MAX as u64
                || child_owners
                    .insert((key.provider, agent_id.clone()), storage_key.clone())
                    .is_some()
            {
                return Err(StorageError::InvalidStorage("invalid lifecycle subagent"));
            }
        }
        validate_invocation(key.provider, state, snapshot.next_sequence)?;
    }
    validate_topology(snapshot, &child_owners)
}

fn validate_session_signature(state: &SessionLifecycleState) -> Result<(), StorageError> {
    let latest = state.latest_event.ok_or(StorageError::InvalidStorage(
        "missing latest lifecycle event",
    ))?;
    match (latest, state.last_signature.as_ref()) {
        (LifecycleEventName::PermissionRequest, None) => {}
        (_, Some(signature)) if signature.kind.name() == latest => {
            signature_columns(Some(signature))?;
        }
        _ => {
            return Err(StorageError::InvalidStorage(
                "invalid lifecycle event signature",
            ));
        }
    }
    match (
        latest,
        state.session_start_source,
        state.last_signature.as_ref(),
    ) {
        (
            LifecycleEventName::SessionStart,
            Some(source),
            Some(LifecycleEventSignature {
                kind: LifecycleEventKind::SessionStart { source: signature },
                ..
            }),
        ) if source == *signature => Ok(()),
        (LifecycleEventName::SessionStart, _, _) => Err(StorageError::InvalidStorage(
            "mismatched session start source",
        )),
        (_, None, _) => Ok(()),
        (_, Some(_), _) => Err(StorageError::InvalidStorage(
            "unexpected session start source",
        )),
    }
}

fn validate_turns(state: &SessionLifecycleState) -> Result<(), StorageError> {
    if state.turn_open && state.current_turn.is_none() {
        return Err(StorageError::InvalidStorage(
            "open lifecycle turn missing identity",
        ));
    }
    if let Some(turn) = state.current_turn.as_deref() {
        validate_id(turn)?;
    }
    let mut turns = BTreeSet::new();
    for turn in &state.recent_turns {
        validate_id(turn)?;
        if !turns.insert(turn) {
            return Err(StorageError::InvalidStorage("duplicate lifecycle turn"));
        }
    }
    Ok(())
}

fn validate_lease(state: &SessionLifecycleState, next_sequence: u64) -> Result<(), StorageError> {
    match (
        state.status_event,
        state.status_sequence,
        state.status_received_at_ms,
        state.projected_status,
    ) {
        (None, None, None, None) => Ok(()),
        (Some(event), Some(sequence), Some(received), Some(status))
            if sequence > 0
                && sequence < next_sequence
                && sequence <= state.latest_sequence
                && received <= i64::MAX as u64
                && valid_lease_projection(event, status) =>
        {
            Ok(())
        }
        _ => Err(StorageError::InvalidStorage("invalid lifecycle lease")),
    }
}

fn validate_invocation(
    provider: AgentProvider,
    state: &SessionLifecycleState,
    _next_sequence: u64,
) -> Result<(), StorageError> {
    match state.antigravity_initial_step {
        None if state.antigravity_child_events.is_empty() => Ok(()),
        Some(floor)
            if provider == AgentProvider::Antigravity
                && state.turn_open
                && state
                    .current_turn
                    .as_deref()
                    .and_then(|turn| turn.strip_prefix("invocation-"))
                    .and_then(|suffix| suffix.parse::<u64>().ok())
                    .is_some()
                && state.antigravity_child_events.len() <= MAX_ANTIGRAVITY_INVOCATION_STEPS
                && state.antigravity_child_events.iter().all(|(step, bits)| {
                    *step >= floor && *bits != 0 && *bits & !(PRE_TOOL_BIT | POST_TOOL_BIT) == 0
                }) =>
        {
            Ok(())
        }
        _ => Err(StorageError::InvalidStorage("invalid lifecycle invocation")),
    }
}

fn validate_topology(
    snapshot: &LifecycleSnapshot,
    owners: &BTreeMap<(AgentProvider, String), String>,
) -> Result<(), StorageError> {
    for (storage_key, state) in &snapshot.sessions {
        let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
            StorageError::InvalidStorage("invalid lifecycle session key"),
        )?;
        if let Some(parent) = state.provider_session_id.as_deref() {
            validate_id(parent)?;
            if parent == key.session_id {
                return Err(StorageError::InvalidStorage("invalid lifecycle parent"));
            }
            let Some(owner_key) = owners.get(&(key.provider, key.session_id.clone())) else {
                return Err(StorageError::InvalidStorage("unowned lifecycle child"));
            };
            let expected_parent = AgentSessionKey::native(key.provider, parent).storage_key();
            if !snapshot.sessions.contains_key(&expected_parent) {
                return Err(StorageError::InvalidStorage("missing lifecycle parent"));
            }
            let owner_state = &snapshot.sessions[owner_key];
            let active = owner_state.active_subagents.get(&key.session_id).ok_or(
                StorageError::InvalidStorage("missing lifecycle ownership edge"),
            )?;
            if state
                .current_turn
                .as_deref()
                .is_some_and(|turn| turn != active.turn_id)
            {
                return Err(StorageError::InvalidStorage("mismatched subagent turn"));
            }
            if key.provider == AgentProvider::Codex {
                if !topology_descends_from(snapshot, owner_key, &expected_parent)? {
                    return Err(StorageError::InvalidStorage("mismatched lifecycle parent"));
                }
            } else if owner_key != &expected_parent {
                return Err(StorageError::InvalidStorage("mismatched lifecycle parent"));
            }
        }

        let mut visited = BTreeSet::new();
        let mut current = storage_key.clone();
        loop {
            if !visited.insert(current.clone()) {
                return Err(StorageError::InvalidStorage("cyclic lifecycle topology"));
            }
            let current_state =
                snapshot
                    .sessions
                    .get(&current)
                    .ok_or(StorageError::InvalidStorage(
                        "missing lifecycle topology node",
                    ))?;
            let Some(parent) = current_state.provider_session_id.as_deref() else {
                break;
            };
            let current_key = AgentSessionKey::from_storage_key(&current).ok_or(
                StorageError::InvalidStorage("invalid lifecycle session key"),
            )?;
            current = AgentSessionKey::native(current_key.provider, parent).storage_key();
        }
    }

    for storage_key in snapshot.sessions.keys() {
        if AgentSessionKey::from_storage_key(storage_key)
            .is_none_or(|key| key.provider != AgentProvider::Codex)
        {
            continue;
        }
        let mut visited = BTreeSet::new();
        let mut current = storage_key.clone();
        loop {
            if !visited.insert(current.clone()) {
                return Err(StorageError::InvalidStorage("cyclic subagent ownership"));
            }
            let key = AgentSessionKey::from_storage_key(&current).ok_or(
                StorageError::InvalidStorage("invalid lifecycle session key"),
            )?;
            let Some(owner) = owners.get(&(key.provider, key.session_id)) else {
                break;
            };
            current = owner.clone();
        }
    }
    Ok(())
}

fn topology_descends_from(
    snapshot: &LifecycleSnapshot,
    node: &str,
    ancestor: &str,
) -> Result<bool, StorageError> {
    let mut current = node.to_owned();
    let mut visited = BTreeSet::new();
    loop {
        if current == ancestor {
            return Ok(true);
        }
        if !visited.insert(current.clone()) {
            return Err(StorageError::InvalidStorage("cyclic lifecycle topology"));
        }
        let state = snapshot
            .sessions
            .get(&current)
            .ok_or(StorageError::InvalidStorage(
                "missing lifecycle topology node",
            ))?;
        let Some(parent) = state.provider_session_id.as_deref() else {
            return Ok(false);
        };
        let key = AgentSessionKey::from_storage_key(&current).ok_or(
            StorageError::InvalidStorage("invalid lifecycle session key"),
        )?;
        current = AgentSessionKey::native(key.provider, parent).storage_key();
    }
}

fn session_mut<'a>(
    sessions: &'a mut BTreeMap<String, SessionLifecycleState>,
    provider: &str,
    session_id: &str,
) -> Result<&'a mut SessionLifecycleState, StorageError> {
    let provider = parse_provider(provider)?;
    sessions
        .get_mut(&AgentSessionKey::native(provider, session_id).storage_key())
        .ok_or(StorageError::InvalidStorage("orphan lifecycle row"))
}

fn parse_provider(value: &str) -> Result<AgentProvider, StorageError> {
    match value {
        "codex" => Ok(AgentProvider::Codex),
        "claude" => Ok(AgentProvider::Claude),
        "antigravity" => Ok(AgentProvider::Antigravity),
        _ => Err(StorageError::InvalidStorage("invalid lifecycle provider")),
    }
}

fn parse_event_name(value: &str) -> Result<LifecycleEventName, StorageError> {
    match value {
        "session_start" => Ok(LifecycleEventName::SessionStart),
        "user_prompt_submit" => Ok(LifecycleEventName::UserPromptSubmit),
        "pre_tool_use" => Ok(LifecycleEventName::PreToolUse),
        "post_tool_use" => Ok(LifecycleEventName::PostToolUse),
        "permission_request" => Ok(LifecycleEventName::PermissionRequest),
        "subagent_start" => Ok(LifecycleEventName::SubagentStart),
        "subagent_stop" => Ok(LifecycleEventName::SubagentStop),
        "stop" => Ok(LifecycleEventName::Stop),
        _ => Err(StorageError::InvalidStorage("invalid lifecycle event")),
    }
}

fn event_name_db(value: LifecycleEventName) -> &'static str {
    match value {
        LifecycleEventName::SessionStart => "session_start",
        LifecycleEventName::UserPromptSubmit => "user_prompt_submit",
        LifecycleEventName::PreToolUse => "pre_tool_use",
        LifecycleEventName::PostToolUse => "post_tool_use",
        LifecycleEventName::PermissionRequest => "permission_request",
        LifecycleEventName::SubagentStart => "subagent_start",
        LifecycleEventName::SubagentStop => "subagent_stop",
        LifecycleEventName::Stop => "stop",
    }
}

fn parse_start_source(value: &str) -> Result<SessionStartSource, StorageError> {
    match value {
        "startup" => Ok(SessionStartSource::Startup),
        "resume" => Ok(SessionStartSource::Resume),
        "clear" => Ok(SessionStartSource::Clear),
        "compact" => Ok(SessionStartSource::Compact),
        _ => Err(StorageError::InvalidStorage("invalid session start source")),
    }
}

fn start_source_db(value: SessionStartSource) -> &'static str {
    match value {
        SessionStartSource::Startup => "startup",
        SessionStartSource::Resume => "resume",
        SessionStartSource::Clear => "clear",
        SessionStartSource::Compact => "compact",
    }
}

fn parse_ignore_reason(value: &str) -> Result<IgnoreReason, StorageError> {
    match value {
        "duplicate" => Ok(IgnoreReason::Duplicate),
        "recent_turn" => Ok(IgnoreReason::RecentTurn),
        "ambiguous_turn" => Ok(IgnoreReason::AmbiguousTurn),
        "active_subagent_capacity" => Ok(IgnoreReason::ActiveSubagentCapacity),
        "sequence_exhausted" => Ok(IgnoreReason::SequenceExhausted),
        "unproven_subagent" => Ok(IgnoreReason::UnprovenSubagent),
        "provider_session_mismatch" => Ok(IgnoreReason::ProviderSessionMismatch),
        "subagent_turn_mismatch" => Ok(IgnoreReason::SubagentTurnMismatch),
        _ => Err(StorageError::InvalidStorage(
            "invalid lifecycle ignore reason",
        )),
    }
}

fn ignore_reason_db(value: IgnoreReason) -> &'static str {
    match value {
        IgnoreReason::Duplicate => "duplicate",
        IgnoreReason::RecentTurn => "recent_turn",
        IgnoreReason::AmbiguousTurn => "ambiguous_turn",
        IgnoreReason::ActiveSubagentCapacity => "active_subagent_capacity",
        IgnoreReason::SequenceExhausted => "sequence_exhausted",
        IgnoreReason::UnprovenSubagent => "unproven_subagent",
        IgnoreReason::ProviderSessionMismatch => "provider_session_mismatch",
        IgnoreReason::SubagentTurnMismatch => "subagent_turn_mismatch",
    }
}

fn parse_status(value: &str) -> Result<ProjectedStatus, StorageError> {
    match value {
        "processing" => Ok(ProjectedStatus::Processing),
        "needs_input" => Ok(ProjectedStatus::NeedsInput),
        "idle" => Ok(ProjectedStatus::Idle),
        _ => Err(StorageError::InvalidStorage(
            "invalid projected lifecycle status",
        )),
    }
}

fn status_db(value: ProjectedStatus) -> &'static str {
    match value {
        ProjectedStatus::Processing => "processing",
        ProjectedStatus::NeedsInput => "needs_input",
        ProjectedStatus::Idle => "idle",
    }
}

fn valid_lease_projection(event: LifecycleEventName, status: ProjectedStatus) -> bool {
    matches!(
        (event, status),
        (LifecycleEventName::Stop, ProjectedStatus::Idle)
            | (
                LifecycleEventName::PermissionRequest,
                ProjectedStatus::Processing | ProjectedStatus::NeedsInput
            )
            | (
                LifecycleEventName::UserPromptSubmit
                    | LifecycleEventName::PreToolUse
                    | LifecycleEventName::PostToolUse
                    | LifecycleEventName::SubagentStart,
                ProjectedStatus::Processing
            )
    )
}

fn parse_signature(
    event: Option<&str>,
    turn_id: Option<String>,
    detail_id: Option<String>,
    source: Option<&str>,
) -> Result<Option<LifecycleEventSignature>, StorageError> {
    let Some(event) = event else {
        if turn_id.is_some() || detail_id.is_some() || source.is_some() {
            return Err(StorageError::InvalidStorage("partial lifecycle signature"));
        }
        return Ok(None);
    };
    let event = parse_event_name(event)?;
    let kind = match event {
        LifecycleEventName::SessionStart
            if turn_id.is_none() && detail_id.is_none() && source.is_some() =>
        {
            LifecycleEventKind::SessionStart {
                source: parse_start_source(source.expect("checked signature source"))?,
            }
        }
        LifecycleEventName::UserPromptSubmit
            if turn_id.is_some() && detail_id.is_none() && source.is_none() =>
        {
            LifecycleEventKind::UserPromptSubmit
        }
        LifecycleEventName::PreToolUse
            if turn_id.is_some() && detail_id.is_none() && source.is_none() =>
        {
            LifecycleEventKind::PreToolUse
        }
        LifecycleEventName::PostToolUse
            if turn_id.is_some() && detail_id.is_none() && source.is_none() =>
        {
            LifecycleEventKind::PostToolUse
        }
        LifecycleEventName::SubagentStart
            if turn_id.is_some() && detail_id.is_some() && source.is_none() =>
        {
            LifecycleEventKind::SubagentStart {
                agent_id: detail_id.expect("checked signature detail"),
            }
        }
        LifecycleEventName::SubagentStop
            if turn_id.is_some() && detail_id.is_some() && source.is_none() =>
        {
            LifecycleEventKind::SubagentStop {
                agent_id: detail_id.expect("checked signature detail"),
            }
        }
        LifecycleEventName::Stop
            if turn_id.is_some() && detail_id.is_none() && source.is_none() =>
        {
            LifecycleEventKind::Stop
        }
        LifecycleEventName::PermissionRequest => {
            return Err(StorageError::InvalidStorage(
                "permission signature persisted",
            ));
        }
        _ => return Err(StorageError::InvalidStorage("invalid lifecycle signature")),
    };
    Ok(Some(LifecycleEventSignature { turn_id, kind }))
}

type SignatureColumns<'a> = (
    Option<&'static str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'static str>,
);

fn signature_columns(
    signature: Option<&LifecycleEventSignature>,
) -> Result<SignatureColumns<'_>, StorageError> {
    let Some(signature) = signature else {
        return Ok((None, None, None, None));
    };
    match &signature.kind {
        LifecycleEventKind::SessionStart { source } if signature.turn_id.is_none() => Ok((
            Some("session_start"),
            None,
            None,
            Some(start_source_db(*source)),
        )),
        LifecycleEventKind::UserPromptSubmit => {
            signature_turn_columns("user_prompt_submit", signature)
        }
        LifecycleEventKind::PreToolUse => signature_turn_columns("pre_tool_use", signature),
        LifecycleEventKind::PostToolUse => signature_turn_columns("post_tool_use", signature),
        LifecycleEventKind::Stop => signature_turn_columns("stop", signature),
        LifecycleEventKind::SubagentStart { agent_id } => {
            signature_detail_columns("subagent_start", signature, agent_id)
        }
        LifecycleEventKind::SubagentStop { agent_id } => {
            signature_detail_columns("subagent_stop", signature, agent_id)
        }
        LifecycleEventKind::PermissionRequest { .. } => Err(StorageError::InvalidStorage(
            "permission signature persisted",
        )),
        LifecycleEventKind::SessionStart { .. } => Err(StorageError::InvalidStorage(
            "invalid session start signature",
        )),
    }
}

fn signature_turn_columns<'a>(
    event: &'static str,
    signature: &'a LifecycleEventSignature,
) -> Result<SignatureColumns<'a>, StorageError> {
    let turn = signature
        .turn_id
        .as_deref()
        .ok_or(StorageError::InvalidStorage("missing signature turn"))?;
    validate_id(turn)?;
    Ok((Some(event), Some(turn), None, None))
}

fn signature_detail_columns<'a>(
    event: &'static str,
    signature: &'a LifecycleEventSignature,
    detail: &'a str,
) -> Result<SignatureColumns<'a>, StorageError> {
    let (_, turn, _, _) = signature_turn_columns(event, signature)?;
    validate_id(detail)?;
    Ok((Some(event), turn, Some(detail), None))
}

fn path_from_bytes(value: Vec<u8>) -> Result<PathBuf, StorageError> {
    if value.is_empty() || value.len() > 4096 || value[0] != b'/' {
        return Err(StorageError::InvalidStorage("invalid lifecycle path"));
    }
    Ok(PathBuf::from(OsString::from_vec(value)))
}

fn validate_path(path: &Path) -> Result<(), StorageError> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > 4096 || !path.is_absolute() {
        Err(StorageError::InvalidStorage("invalid lifecycle path"))
    } else {
        Ok(())
    }
}

fn validate_id(value: &str) -> Result<(), StorageError> {
    if value.is_empty() || value.len() > 512 {
        Err(StorageError::InvalidStorage("invalid lifecycle identifier"))
    } else {
        Ok(())
    }
}

fn parse_bool(value: i64) -> Result<bool, StorageError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(StorageError::InvalidStorage("invalid lifecycle boolean")),
    }
}

fn positive_u64(value: i64, error: &'static str) -> Result<u64, StorageError> {
    if value <= 0 {
        Err(StorageError::InvalidStorage(error))
    } else {
        Ok(value as u64)
    }
}

fn nonnegative_u64(value: i64, error: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::InvalidStorage(error))
}

fn sqlite_i64(value: u64, error: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::InvalidStorage(error))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use coding_brain_core::lifecycle::{LifecycleEvent, LifecycleEventKind, LifecycleIdentity};

    use super::*;

    #[test]
    fn immediate_transaction_error_rolls_back_the_prior_lifecycle_snapshot() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let paths = super::super::StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "session-1".into(),
            Some("turn-1".into()),
            None,
            "/work/project".into(),
        )
        .unwrap();
        db.record_lifecycle(
            LifecycleEvent::from_parts(identity.clone(), LifecycleEventKind::UserPromptSubmit)
                .unwrap(),
            100,
        )
        .unwrap();
        let before = db.read_lifecycle().unwrap();
        db.connection
            .execute_batch(
                "CREATE TEMP TRIGGER abort_lifecycle_session_insert
                 BEFORE INSERT ON lifecycle_sessions
                 BEGIN
                    SELECT RAISE(ABORT, 'injected lifecycle persistence failure');
                 END;",
            )
            .unwrap();

        let error = db
            .record_lifecycle(
                LifecycleEvent::from_parts(identity, LifecycleEventKind::Stop).unwrap(),
                101,
            )
            .unwrap_err();

        assert!(matches!(error, StorageError::Sqlite(_)), "{error:?}");
        db.connection
            .execute_batch("DROP TRIGGER abort_lifecycle_session_insert;")
            .unwrap();
        assert_eq!(db.read_lifecycle().unwrap(), before);
    }
}
