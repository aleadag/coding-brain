use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Deserialize;

use crate::codex_transcript::CodexResumeEvidence;
use crate::provider::{AgentProvider, AgentSessionKey};

use super::{
    ANTIGRAVITY_CHILD_BITS, ActiveSubagentState, ApplyOutcome, IgnoreReason,
    LIFECYCLE_SCHEMA_VERSION, LifecycleEvent, LifecycleIdentity, LifecycleSnapshot,
    MAX_ACTIVE_SUBAGENTS, MAX_ANTIGRAVITY_INVOCATION_STEPS, MAX_PERMISSION_REQUESTS_PER_TURN,
    MAX_RECENT_TURNS, PERMISSION_BITS,
};

pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
pub const MAX_SESSIONS: usize = 128;
pub const SESSION_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const LOCK_RETRY: Duration = Duration::from_millis(5);
const MAX_CORRUPT_FILES: usize = 3;

#[derive(Clone, Debug)]
pub struct LifecycleStore {
    root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedLifecycleEvent {
    pub outcome: ApplyOutcome,
    pub sequence: u64,
}

impl LifecycleStore {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn hooks_dir(&self) -> PathBuf {
        self.root.join("hooks")
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.hooks_dir().join("lifecycle.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.hooks_dir().join("lifecycle.lock")
    }

    pub fn read(&self) -> Result<StoreView, StoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, LockKind::Shared)?;
        match self.load()? {
            LoadedSnapshot::Missing => Ok(StoreView {
                snapshot: None,
                condition: StoreCondition::Missing,
            }),
            LoadedSnapshot::Healthy(snapshot) => Ok(StoreView {
                snapshot: Some(snapshot),
                condition: StoreCondition::Healthy,
            }),
            LoadedSnapshot::Corrupt => Ok(StoreView {
                snapshot: None,
                condition: StoreCondition::Corrupt,
            }),
            LoadedSnapshot::NewerSchema(version) => Ok(StoreView {
                snapshot: None,
                condition: StoreCondition::NewerSchema(version),
            }),
        }
    }

    pub fn codex_subagent_is_proven(
        &self,
        identity: &LifecycleIdentity,
    ) -> Result<bool, StoreError> {
        if identity.provider() != AgentProvider::Codex {
            return Ok(false);
        }
        let (Some(parent_id), Some(turn_id)) = (identity.provider_session_id(), identity.turn_id())
        else {
            return Ok(false);
        };
        let view = self.read()?;
        let Some(snapshot) = view.snapshot else {
            return Ok(false);
        };
        let Some(owner_key) = snapshot.active_subagent_owner_key(
            AgentProvider::Codex,
            parent_id,
            identity.session_id(),
        ) else {
            return Ok(false);
        };
        Ok(
            snapshot.sessions[&owner_key].active_subagents[identity.session_id()].turn_id
                == turn_id,
        )
    }

    pub fn reprove_codex_subagent(
        &self,
        identity: &LifecycleIdentity,
        evidence: &CodexResumeEvidence,
    ) -> Result<ApplyOutcome, StoreError> {
        self.reprove_codex_subagent_at(identity, evidence, epoch_ms())
    }

    fn reprove_codex_subagent_at(
        &self,
        identity: &LifecycleIdentity,
        evidence: &CodexResumeEvidence,
        received_at_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, LockKind::Exclusive)?;
        let mut snapshot = self.load_for_locked_update(received_at_ms)?;

        let outcome = (|| {
            if identity.provider() != AgentProvider::Codex {
                return ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent);
            }
            let (Some(parent_id), Some(turn_id), Some(transcript_path)) = (
                identity.provider_session_id(),
                identity.turn_id(),
                identity.transcript_path(),
            ) else {
                return ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent);
            };
            if let Some((owner_key, active)) =
                snapshot.sessions.iter().find_map(|(storage_key, state)| {
                    (AgentSessionKey::from_storage_key(storage_key)
                        .is_some_and(|key| key.provider == AgentProvider::Codex))
                    .then(|| {
                        state
                            .active_subagents
                            .get(identity.session_id())
                            .map(|active| (storage_key, active))
                    })
                    .flatten()
                })
            {
                if !snapshot.topology_contains_session(AgentProvider::Codex, parent_id, owner_key) {
                    return ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch);
                }
                return ApplyOutcome::Ignored(if active.turn_id == turn_id {
                    IgnoreReason::Duplicate
                } else {
                    IgnoreReason::SubagentTurnMismatch
                });
            }
            let Some(parent_key) = snapshot.stopped_subagent_owner_key(
                AgentProvider::Codex,
                parent_id,
                identity.session_id(),
            ) else {
                return ApplyOutcome::Ignored(
                    if snapshot.sessions.iter().any(|(storage_key, state)| {
                        AgentSessionKey::from_storage_key(storage_key)
                            .is_some_and(|key| key.provider == AgentProvider::Codex)
                            && state.stopped_subagents.contains_key(identity.session_id())
                    }) {
                        IgnoreReason::ProviderSessionMismatch
                    } else {
                        IgnoreReason::UnprovenSubagent
                    },
                );
            };
            let parent = &snapshot.sessions[&parent_key];
            let stopped = &parent.stopped_subagents[identity.session_id()];
            let requested_path = lexical_normalize_path(&evidence.requested_transcript_path);
            let canonical_identity_path = fs::canonicalize(transcript_path).ok();
            let canonical_identity_is_file = canonical_identity_path
                .as_deref()
                .and_then(|path| fs::metadata(path).ok())
                .is_some_and(|metadata| metadata.is_file());
            if evidence.child_session_id != identity.session_id()
                || evidence.provider_session_id != parent_id
                || evidence.turn_id != turn_id
                || requested_path.as_deref() != Some(transcript_path)
                || canonical_identity_path.as_deref()
                    != Some(evidence.canonical_transcript_path.as_path())
                || !canonical_identity_is_file
                || evidence.turn_id == stopped.turn_id
                || evidence.started_at_ms <= stopped.received_at_ms
                || evidence.started_at_ms > received_at_ms.saturating_add(5_000)
            {
                return ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent);
            }
            if parent.active_subagents.len() >= MAX_ACTIVE_SUBAGENTS {
                return ApplyOutcome::Ignored(IgnoreReason::ActiveSubagentCapacity);
            }
            if snapshot.next_sequence == 0 || snapshot.next_sequence >= u64::MAX - 1 {
                return ApplyOutcome::Ignored(IgnoreReason::SequenceExhausted);
            }

            let sequence = snapshot.next_sequence;
            snapshot.next_sequence += 1;
            let parent = snapshot
                .sessions
                .get_mut(&parent_key)
                .expect("validated Codex topology owner");
            parent.stopped_subagents.remove(identity.session_id());
            parent.active_subagents.insert(
                identity.session_id().to_owned(),
                ActiveSubagentState {
                    started_sequence: sequence,
                    received_at_ms,
                    turn_id: turn_id.to_owned(),
                },
            );
            parent.latest_sequence = sequence;
            parent.latest_received_at_ms = received_at_ms;
            parent.ignored_reason = None;
            ApplyOutcome::Applied
        })();

        self.persist_locked_snapshot(&snapshot)?;
        Ok(outcome)
    }

    pub fn record(&self, event: LifecycleEvent) -> Result<ApplyOutcome, StoreError> {
        self.record_at(event, epoch_ms())
    }

    pub fn record_with_sequence(
        &self,
        event: LifecycleEvent,
    ) -> Result<RecordedLifecycleEvent, StoreError> {
        self.record_with_sequence_at(event, epoch_ms())
    }

    fn record_at(
        &self,
        event: LifecycleEvent,
        received_at_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.record_with_sequence_at(event, received_at_ms)
            .map(|recorded| recorded.outcome)
    }

    fn record_with_sequence_at(
        &self,
        event: LifecycleEvent,
        received_at_ms: u64,
    ) -> Result<RecordedLifecycleEvent, StoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, LockKind::Exclusive)?;
        let mut snapshot = self.load_for_locked_update(received_at_ms)?;
        let session_key =
            AgentSessionKey::native(event.identity().provider(), event.identity().session_id())
                .storage_key();
        let outcome = snapshot.apply(event, received_at_ms);
        let sequence = match outcome {
            ApplyOutcome::Applied => snapshot
                .sessions
                .get(&session_key)
                .map(|state| state.latest_sequence)
                .ok_or(StoreError::Serialization)?,
            ApplyOutcome::Ignored(_) => 0,
        };
        self.persist_locked_snapshot(&snapshot)?;
        Ok(RecordedLifecycleEvent { outcome, sequence })
    }

    fn load_for_locked_update(&self, received_at_ms: u64) -> Result<LifecycleSnapshot, StoreError> {
        self.cleanup_abandoned_temps()?;
        let mut snapshot = match self.load()? {
            LoadedSnapshot::Missing => LifecycleSnapshot::default(),
            LoadedSnapshot::Healthy(snapshot) => snapshot,
            LoadedSnapshot::Corrupt => {
                self.quarantine_corrupt(received_at_ms)?;
                LifecycleSnapshot::default()
            }
            LoadedSnapshot::NewerSchema(version) => {
                return Err(StoreError::NewerSchema(version));
            }
        };
        retain_sessions(&mut snapshot, received_at_ms);
        Ok(snapshot)
    }

    fn persist_locked_snapshot(&self, snapshot: &LifecycleSnapshot) -> Result<(), StoreError> {
        if snapshot.sessions.len() > MAX_SESSIONS {
            return Err(StoreError::SessionCapacity);
        }
        if !valid_snapshot_shape(snapshot) {
            return Err(StoreError::InvalidSnapshot);
        }
        let bytes = serde_json::to_vec(snapshot).map_err(|_| StoreError::Serialization)?;
        ensure_serialized_size(&bytes)?;
        self.persist(&bytes)
    }

    fn open_lock(&self) -> Result<File, StoreError> {
        fs::create_dir_all(self.hooks_dir()).map_err(|_| StoreError::Io)?;
        set_dir_mode(&self.hooks_dir())?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.lock_path())
            .map_err(|_| StoreError::Io)?;
        set_file_mode(&file)?;
        Ok(file)
    }

    fn load(&self) -> Result<LoadedSnapshot, StoreError> {
        let path = self.snapshot_path();
        if !path.exists() {
            return Ok(LoadedSnapshot::Missing);
        }
        let mut file = File::open(path).map_err(|_| StoreError::Io)?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take((MAX_SNAPSHOT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| StoreError::Io)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Ok(LoadedSnapshot::Corrupt);
        }

        let Ok(header) = serde_json::from_slice::<SchemaHeader>(&bytes) else {
            return Ok(LoadedSnapshot::Corrupt);
        };
        if header.schema_version > LIFECYCLE_SCHEMA_VERSION {
            return Ok(LoadedSnapshot::NewerSchema(header.schema_version));
        }
        if !matches!(header.schema_version, 1 | 2 | LIFECYCLE_SCHEMA_VERSION) {
            return Ok(LoadedSnapshot::Corrupt);
        }
        let Ok(mut snapshot) = serde_json::from_slice::<LifecycleSnapshot>(&bytes) else {
            return Ok(LoadedSnapshot::Corrupt);
        };
        if header.schema_version == 1 {
            snapshot = project_schema_one(snapshot);
        }
        if header.schema_version <= 2 {
            snapshot = project_schema_two(snapshot);
        }
        if !valid_snapshot_shape(&snapshot) {
            return Ok(LoadedSnapshot::Corrupt);
        }
        Ok(LoadedSnapshot::Healthy(snapshot))
    }

    fn persist(&self, bytes: &[u8]) -> Result<(), StoreError> {
        let mut temp = tempfile::Builder::new()
            .prefix("lifecycle.tmp-")
            .tempfile_in(self.hooks_dir())
            .map_err(|_| StoreError::Io)?;
        set_file_mode(temp.as_file())?;
        temp.write_all(bytes).map_err(|_| StoreError::Io)?;
        temp.flush().map_err(|_| StoreError::Io)?;
        temp.persist(self.snapshot_path())
            .map_err(|_| StoreError::Io)?;
        Ok(())
    }

    fn cleanup_abandoned_temps(&self) -> Result<(), StoreError> {
        for entry in fs::read_dir(self.hooks_dir()).map_err(|_| StoreError::Io)? {
            let entry = entry.map_err(|_| StoreError::Io)?;
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("lifecycle.tmp-") {
                fs::remove_file(entry.path()).map_err(|_| StoreError::Io)?;
            }
        }
        Ok(())
    }

    fn quarantine_corrupt(&self, received_at_ms: u64) -> Result<(), StoreError> {
        let mut suffix = received_at_ms;
        let path = loop {
            let candidate = self
                .hooks_dir()
                .join(format!("lifecycle.json.corrupt-{suffix}"));
            if !candidate.exists() {
                break candidate;
            }
            suffix = suffix.saturating_add(1);
        };
        fs::rename(self.snapshot_path(), path).map_err(|_| StoreError::Quarantine)?;
        let mut corrupt = self.corrupt_paths()?;
        while corrupt.len() > MAX_CORRUPT_FILES {
            fs::remove_file(corrupt.remove(0)).map_err(|_| StoreError::Io)?;
        }
        Ok(())
    }

    fn corrupt_paths(&self) -> Result<Vec<PathBuf>, StoreError> {
        let mut paths = fs::read_dir(self.hooks_dir())
            .map_err(|_| StoreError::Io)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("lifecycle.json.corrupt-")
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }
}

pub fn coding_brain_state_root() -> PathBuf {
    crate::paths::CodingBrainPaths::resolve(&crate::paths::PathEnvironment::current())
        .map(|paths| paths.state_root().to_path_buf())
        .unwrap_or_else(|_| std::env::temp_dir().join("coding-brain"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreCondition {
    Healthy,
    Missing,
    Corrupt,
    NewerSchema(u32),
    Unavailable,
}

impl StoreCondition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::NewerSchema(_) => "newer_schema",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreView {
    pub snapshot: Option<LifecycleSnapshot>,
    pub condition: StoreCondition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StoreError {
    InvalidSnapshot,
    Io,
    LockTimeout,
    NewerSchema(u32),
    Quarantine,
    Serialization,
    SnapshotTooLarge,
    SessionCapacity,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshot => f.write_str("lifecycle snapshot invariants are invalid"),
            Self::Io => f.write_str("lifecycle store I/O failed"),
            Self::LockTimeout => f.write_str("lifecycle store lock timed out"),
            Self::NewerSchema(version) => {
                write!(f, "lifecycle schema {version} is newer than supported")
            }
            Self::Quarantine => f.write_str("corrupt lifecycle state could not be quarantined"),
            Self::Serialization => f.write_str("lifecycle state serialization failed"),
            Self::SnapshotTooLarge => f.write_str("lifecycle snapshot exceeds its size limit"),
            Self::SessionCapacity => f.write_str("lifecycle session capacity reached"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Deserialize)]
struct SchemaHeader {
    schema_version: u32,
}

enum LoadedSnapshot {
    Missing,
    Healthy(LifecycleSnapshot),
    Corrupt,
    NewerSchema(u32),
}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

struct LockGuard<'a> {
    file: &'a File,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.file);
    }
}

fn lock_with_timeout(file: &File, kind: LockKind) -> Result<LockGuard<'_>, StoreError> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        let result = match kind {
            LockKind::Shared => FileExt::try_lock_shared(file),
            LockKind::Exclusive => FileExt::try_lock_exclusive(file),
        };
        match result {
            Ok(()) => return Ok(LockGuard { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(StoreError::LockTimeout);
                }
                thread::sleep(LOCK_RETRY);
            }
            Err(_) => return Err(StoreError::Io),
        }
    }
}

fn valid_snapshot_shape(snapshot: &LifecycleSnapshot) -> bool {
    if snapshot.schema_version != LIFECYCLE_SCHEMA_VERSION
        || snapshot.sessions.len() > MAX_SESSIONS
        || snapshot.next_sequence == 0
        || snapshot.next_sequence == u64::MAX
    {
        return false;
    }

    let mut active_children = BTreeSet::new();
    let mut stopped_children = BTreeSet::new();
    for (storage_key, state) in &snapshot.sessions {
        let Some(key) = AgentSessionKey::from_storage_key(storage_key) else {
            return false;
        };
        let antigravity_state_valid = match (
            key.provider,
            state.antigravity_initial_step,
            state.antigravity_child_events.is_empty(),
        ) {
            (_, None, true) => true,
            (AgentProvider::Antigravity, Some(floor), _) => {
                state.turn_open
                    && state
                        .current_turn
                        .as_deref()
                        .and_then(|turn| turn.strip_prefix("invocation-"))
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some()
                    && state.antigravity_child_events.len() <= MAX_ANTIGRAVITY_INVOCATION_STEPS
                    && state.antigravity_child_events.iter().all(|(step, bits)| {
                        *step >= floor && *bits != 0 && *bits & !ANTIGRAVITY_CHILD_BITS == 0
                    })
            }
            _ => false,
        };
        let permission_events_valid = state.permission_request_events.len()
            <= MAX_PERMISSION_REQUESTS_PER_TURN
            && state
                .permission_request_events
                .iter()
                .all(|(request_key, bits)| {
                    request_key.len() == 64
                        && request_key
                            .bytes()
                            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                        && *bits != 0
                        && *bits & !PERMISSION_BITS == 0
                })
            && (state.permission_request_events.is_empty()
                || state.turn_open && state.current_turn.is_some());
        if !valid_id(&key.session_id)
            || !valid_path(&state.cwd)
            || !state.transcript_path.as_deref().is_none_or(valid_path)
            || !state.current_turn.as_deref().is_none_or(valid_id)
            || state.recent_turns.len() > MAX_RECENT_TURNS
            || !state.recent_turns.iter().all(|turn| valid_id(turn))
            || state.latest_sequence == 0
            || state.latest_sequence >= snapshot.next_sequence
            || !state
                .status_sequence
                .is_none_or(|sequence| sequence != 0 && sequence < snapshot.next_sequence)
            || state.active_subagents.len() > MAX_ACTIVE_SUBAGENTS
            || state.stopped_subagents.len() > MAX_ACTIVE_SUBAGENTS
            || (!state.stopped_subagents.is_empty() && key.provider != AgentProvider::Codex)
            || !antigravity_state_valid
            || !permission_events_valid
        {
            return false;
        }

        for (agent_id, subagent) in &state.active_subagents {
            let child_key = AgentSessionKey::native(key.provider, agent_id).storage_key();
            if !valid_id(agent_id)
                || !valid_id(&subagent.turn_id)
                || subagent.started_sequence == 0
                || subagent.started_sequence >= snapshot.next_sequence
                || stopped_children.contains(&child_key)
                || !active_children.insert(child_key)
            {
                return false;
            }
            if key.provider == AgentProvider::Codex
                && let Some(child) = snapshot
                    .sessions
                    .get(&AgentSessionKey::native(key.provider, agent_id).storage_key())
                && !child
                    .provider_session_id
                    .as_deref()
                    .is_some_and(|provider_session_id| {
                        snapshot.topology_contains_session(
                            key.provider,
                            provider_session_id,
                            storage_key,
                        )
                    })
            {
                return false;
            }
        }

        for (agent_id, subagent) in &state.stopped_subagents {
            let child_key = AgentSessionKey::native(key.provider, agent_id).storage_key();
            if !valid_id(agent_id)
                || !valid_id(&subagent.turn_id)
                || subagent.stopped_sequence == 0
                || subagent.stopped_sequence >= snapshot.next_sequence
                || state.active_subagents.contains_key(agent_id)
                || active_children.contains(&child_key)
                || !stopped_children.insert(child_key)
            {
                return false;
            }
        }

        if let Some(provider_session_id) = state.provider_session_id.as_deref() {
            let owner_key = if key.provider == AgentProvider::Codex {
                let Some(owner_key) = snapshot.active_subagent_owner_key(
                    key.provider,
                    provider_session_id,
                    &key.session_id,
                ) else {
                    return false;
                };
                owner_key
            } else {
                let owner_key =
                    AgentSessionKey::native(key.provider, provider_session_id).storage_key();
                if !snapshot
                    .sessions
                    .get(&owner_key)
                    .is_some_and(|owner| owner.active_subagents.contains_key(&key.session_id))
                {
                    return false;
                }
                owner_key
            };
            let active = &snapshot.sessions[&owner_key].active_subagents[&key.session_id];
            if provider_session_id == key.session_id
                || !valid_id(provider_session_id)
                || state
                    .current_turn
                    .as_deref()
                    .is_some_and(|turn| turn != active.turn_id)
            {
                return false;
            }
        }
    }

    linked_topology_is_acyclic(snapshot)
}

fn linked_topology_is_acyclic(snapshot: &LifecycleSnapshot) -> bool {
    for storage_key in snapshot.sessions.keys() {
        let mut visited = BTreeSet::new();
        let mut current_key = storage_key.clone();
        loop {
            if !visited.insert(current_key.clone()) {
                return false;
            }
            let Some(state) = snapshot.sessions.get(&current_key) else {
                return false;
            };
            let Some(provider_session_id) = state.provider_session_id.as_deref() else {
                break;
            };
            let Some(key) = AgentSessionKey::from_storage_key(&current_key) else {
                return false;
            };
            current_key = AgentSessionKey::native(key.provider, provider_session_id).storage_key();
        }
    }

    let active_owners = snapshot
        .sessions
        .iter()
        .filter_map(|(owner_key, state)| {
            (AgentSessionKey::from_storage_key(owner_key)
                .expect("validated session key")
                .provider
                == AgentProvider::Codex)
                .then_some((owner_key, state))
        })
        .flat_map(|(owner_key, state)| {
            state.active_subagents.keys().map(move |agent_id| {
                (
                    AgentSessionKey::native(AgentProvider::Codex, agent_id).storage_key(),
                    owner_key,
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    for storage_key in snapshot.sessions.keys() {
        let mut visited = BTreeSet::new();
        let mut current_key = storage_key;
        while let Some(owner_key) = active_owners.get(current_key) {
            if !visited.insert(current_key) {
                return false;
            }
            current_key = owner_key;
        }
    }
    true
}

fn project_schema_one(mut snapshot: LifecycleSnapshot) -> LifecycleSnapshot {
    snapshot.sessions = snapshot
        .sessions
        .into_iter()
        .map(|(session_id, state)| {
            (
                AgentSessionKey::native(AgentProvider::Codex, session_id).storage_key(),
                state,
            )
        })
        .collect();
    snapshot
}

fn project_schema_two(mut snapshot: LifecycleSnapshot) -> LifecycleSnapshot {
    snapshot.schema_version = LIFECYCLE_SCHEMA_VERSION;
    for state in snapshot.sessions.values_mut() {
        state.provider_session_id = None;
        state.active_subagents.clear();
        state.stopped_subagents.clear();
    }
    snapshot
}

fn retain_sessions(snapshot: &mut LifecycleSnapshot, received_at_ms: u64) {
    for state in snapshot.sessions.values_mut() {
        state.stopped_subagents.retain(|_, stopped| {
            received_at_ms.saturating_sub(stopped.received_at_ms) <= SESSION_RETENTION_MS
        });
    }
    let mut removed_sessions = snapshot
        .sessions
        .iter()
        .filter(|(_, state)| {
            received_at_ms.saturating_sub(state.latest_received_at_ms) > SESSION_RETENTION_MS
        })
        .map(|(storage_key, _)| storage_key.clone())
        .collect::<BTreeSet<_>>();
    let expired_topologies = snapshot
        .sessions
        .iter()
        .filter_map(|(storage_key, state)| {
            let expired = state
                .active_subagents
                .iter()
                .filter(|(_, subagent)| {
                    received_at_ms.saturating_sub(subagent.received_at_ms) > SESSION_RETENTION_MS
                })
                .map(|(agent_id, _)| agent_id.clone())
                .collect::<BTreeSet<_>>();
            (!expired.is_empty()).then(|| (storage_key.clone(), expired))
        })
        .collect::<BTreeMap<_, _>>();
    for (provider_key, expired_agents) in &expired_topologies {
        if let Some(key) = AgentSessionKey::from_storage_key(provider_key)
            && key.provider == AgentProvider::Codex
        {
            removed_sessions.extend(
                expired_agents
                    .iter()
                    .map(|agent_id| AgentSessionKey::native(key.provider, agent_id).storage_key()),
            );
        }
    }
    for (storage_key, state) in &snapshot.sessions {
        let Some(key) = AgentSessionKey::from_storage_key(storage_key) else {
            continue;
        };
        if key.provider != AgentProvider::Codex
            && state
                .provider_session_id
                .as_deref()
                .is_some_and(|provider_session_id| {
                    expired_topologies
                        .get(
                            &AgentSessionKey::native(key.provider, provider_session_id)
                                .storage_key(),
                        )
                        .is_some_and(|agents| agents.contains(&key.session_id))
                })
        {
            removed_sessions.insert(storage_key.clone());
        }
    }
    loop {
        let mut descendants = snapshot
            .sessions
            .iter()
            .filter_map(|(storage_key, state)| {
                let provider_session_id = state.provider_session_id.as_deref()?;
                let key = AgentSessionKey::from_storage_key(storage_key)?;
                removed_sessions
                    .contains(
                        &AgentSessionKey::native(key.provider, provider_session_id).storage_key(),
                    )
                    .then(|| storage_key.clone())
            })
            .collect::<BTreeSet<_>>();
        for storage_key in &removed_sessions {
            let Some(state) = snapshot.sessions.get(storage_key) else {
                continue;
            };
            let Some(key) = AgentSessionKey::from_storage_key(storage_key) else {
                continue;
            };
            if key.provider != AgentProvider::Codex {
                continue;
            }
            descendants.extend(
                state
                    .active_subagents
                    .keys()
                    .map(|agent_id| AgentSessionKey::native(key.provider, agent_id).storage_key()),
            );
        }
        if descendants.is_subset(&removed_sessions) {
            break;
        }
        removed_sessions.extend(descendants);
    }
    snapshot
        .sessions
        .retain(|storage_key, _| !removed_sessions.contains(storage_key));
    for (provider_key, expired_agents) in expired_topologies {
        if let Some(provider) = snapshot.sessions.get_mut(&provider_key) {
            provider
                .active_subagents
                .retain(|agent_id, _| !expired_agents.contains(agent_id));
        }
    }
    for (storage_key, state) in &mut snapshot.sessions {
        let Some(key) = AgentSessionKey::from_storage_key(storage_key) else {
            continue;
        };
        state.active_subagents.retain(|agent_id, _| {
            !removed_sessions
                .contains(&AgentSessionKey::native(key.provider, agent_id).storage_key())
        });
    }
}

fn lexical_normalize_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= super::MAX_ID_BYTES
}

fn valid_path(path: &Path) -> bool {
    path.is_absolute()
        && !path.as_os_str().is_empty()
        && path.to_string_lossy().len() <= super::MAX_PATH_BYTES
}

fn ensure_serialized_size(bytes: &[u8]) -> Result<(), StoreError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        Err(StoreError::SnapshotTooLarge)
    } else {
        Ok(())
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn set_dir_mode(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| StoreError::Io)
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &File) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| StoreError::Io)
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;

    use super::super::IgnoreReason;
    use super::super::ProjectedStatus;
    use super::*;
    use crate::codex_transcript::CodexResumeEvidence;
    use crate::provider::{AgentProvider, AgentSessionKey};

    fn prompt(session: &str, turn: &str) -> LifecycleEvent {
        LifecycleEvent::parse(
            json!({
                "session_id": session,
                "turn_id": turn,
                "cwd": "/work/codexctl",
                "hook_event_name": "UserPromptSubmit"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    fn provider_prompt(provider: AgentProvider, session: &str, turn: &str) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new(
                provider,
                session.into(),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::UserPromptSubmit,
        )
        .unwrap()
    }

    fn provider_subagent_start(
        provider: AgentProvider,
        session: &str,
        child: &str,
        turn: &str,
    ) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new(
                provider,
                session.into(),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::SubagentStart {
                agent_id: child.into(),
            },
        )
        .unwrap()
    }

    fn provider_linked_event(
        provider: AgentProvider,
        session: &str,
        provider_session: &str,
        turn: &str,
        kind: super::super::LifecycleEventKind,
    ) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new_with_provider_session(
                provider,
                session.into(),
                Some(provider_session.into()),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            kind,
        )
        .unwrap()
    }

    fn key(session_id: &str) -> String {
        AgentSessionKey::native(AgentProvider::Codex, session_id).storage_key()
    }

    fn store() -> LifecycleStore {
        LifecycleStore::at(tempfile::tempdir().unwrap().keep())
    }

    fn subagent_start(root: &str, child: &str, turn: &str) -> LifecycleEvent {
        let identity = super::super::LifecycleIdentity::try_new(
            AgentProvider::Codex,
            root.into(),
            Some(turn.into()),
            None,
            "/work/project".into(),
        )
        .unwrap();
        LifecycleEvent::from_parts(
            identity,
            super::super::LifecycleEventKind::SubagentStart {
                agent_id: child.into(),
            },
        )
        .unwrap()
    }

    fn subagent_stop(root: &str, child: &str, turn: &str) -> LifecycleEvent {
        let identity = super::super::LifecycleIdentity::try_new(
            AgentProvider::Codex,
            root.into(),
            Some(turn.into()),
            None,
            "/work/project".into(),
        )
        .unwrap();
        LifecycleEvent::from_parts(
            identity,
            super::super::LifecycleEventKind::SubagentStop {
                agent_id: child.into(),
            },
        )
        .unwrap()
    }

    fn linked_tool(child: &str, provider_session: &str, turn: &str) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new_with_provider_session(
                AgentProvider::Codex,
                child.into(),
                Some(provider_session.into()),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::PreToolUse,
        )
        .unwrap()
    }

    fn linked_identity(
        provider: AgentProvider,
        child: &str,
        provider_session: &str,
        turn: &str,
        transcript_path: &Path,
    ) -> super::super::LifecycleIdentity {
        super::super::LifecycleIdentity::try_new_with_provider_session(
            provider,
            child.into(),
            Some(provider_session.into()),
            Some(turn.into()),
            Some(transcript_path.into()),
            "/work/project".into(),
        )
        .unwrap()
    }

    fn permission(identity: super::super::LifecycleIdentity) -> LifecycleEvent {
        LifecycleEvent::permission(identity, super::super::PermissionDisposition::Decided).unwrap()
    }

    fn resume_evidence(
        child: &str,
        provider_session: &str,
        turn: &str,
        transcript_path: &Path,
        started_at_ms: u64,
    ) -> CodexResumeEvidence {
        CodexResumeEvidence {
            child_session_id: child.into(),
            provider_session_id: provider_session.into(),
            parent_thread_id: None,
            turn_id: turn.into(),
            started_at_ms,
            requested_transcript_path: transcript_path.into(),
            canonical_transcript_path: fs::canonicalize(transcript_path).unwrap(),
        }
    }

    fn transcript_path(name: &str) -> PathBuf {
        let directory = tempfile::tempdir().unwrap().keep();
        let path = directory.join(name);
        fs::write(&path, b"transcript").unwrap();
        path
    }

    fn stopped_store(
        transcript_path: &Path,
    ) -> (
        LifecycleStore,
        super::super::LifecycleIdentity,
        CodexResumeEvidence,
    ) {
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root-1", "child-1", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(subagent_stop("root-1", "child-1", "turn-1"), 2_000),
            Ok(ApplyOutcome::Applied)
        );
        (
            store,
            linked_identity(
                AgentProvider::Codex,
                "child-1",
                "root-1",
                "turn-2",
                transcript_path,
            ),
            resume_evidence("child-1", "root-1", "turn-2", transcript_path, 2_500),
        )
    }

    fn linked_subagent_start(
        child: &str,
        provider_session: &str,
        turn: &str,
        nested: &str,
    ) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new_with_provider_session(
                AgentProvider::Codex,
                child.into(),
                Some(provider_session.into()),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::SubagentStart {
                agent_id: nested.into(),
            },
        )
        .unwrap()
    }

    fn linked_subagent_stop(
        child: &str,
        provider_session: &str,
        turn: &str,
        nested: &str,
    ) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new_with_provider_session(
                AgentProvider::Codex,
                child.into(),
                Some(provider_session.into()),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::SubagentStop {
                agent_id: nested.into(),
            },
        )
        .unwrap()
    }

    fn linked_stop(child: &str, provider_session: &str, turn: &str) -> LifecycleEvent {
        LifecycleEvent::from_parts(
            super::super::LifecycleIdentity::try_new_with_provider_session(
                AgentProvider::Codex,
                child.into(),
                Some(provider_session.into()),
                Some(turn.into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            super::super::LifecycleEventKind::Stop,
        )
        .unwrap()
    }

    fn store_with_schema_three_snapshot(snapshot: LifecycleSnapshot) -> LifecycleStore {
        let store = store();
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(
            store.snapshot_path(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        store
    }

    fn assert_schema_three_corrupt(snapshot: LifecycleSnapshot, label: &str) {
        assert_eq!(
            store_with_schema_three_snapshot(snapshot)
                .read()
                .unwrap()
                .condition,
            StoreCondition::Corrupt,
            "{label}"
        );
    }

    fn assert_schema_three_json_corrupt(value: serde_json::Value, label: &str) {
        let store = store();
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(store.snapshot_path(), serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            store.read().unwrap().condition,
            StoreCondition::Corrupt,
            "{label}"
        );
    }

    fn snapshot_with_permission_events(events: serde_json::Value) -> serde_json::Value {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(prompt("session-1", "turn-1"), 1_000),
            ApplyOutcome::Applied
        );
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["sessions"][key("session-1")]["permission_request_events"] = events;
        value
    }

    fn expired_linked_group(root: &str, child: &str, turn: &str) -> LifecycleSnapshot {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(subagent_start(root, child, turn), 0),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(linked_tool(child, root, turn), 0),
            ApplyOutcome::Applied
        );
        snapshot
    }

    fn antigravity_invocation() -> LifecycleEvent {
        let identity = super::super::LifecycleIdentity::try_new(
            AgentProvider::Antigravity,
            "agy-conversation-1".into(),
            Some("invocation-1".into()),
            None,
            "/work/antigravity".into(),
        )
        .unwrap();
        LifecycleEvent::from_parts_with_turn_initial_step(
            identity,
            super::super::LifecycleEventKind::UserPromptSubmit,
            Some(5),
        )
        .unwrap()
    }

    fn antigravity_key() -> String {
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key()
    }

    #[test]
    fn paths_are_relative_to_the_injected_state_root() {
        let store = LifecycleStore::at("/state/codexctl");
        assert_eq!(
            store.snapshot_path(),
            Path::new("/state/codexctl/hooks/lifecycle.json")
        );
        assert_eq!(
            store.lock_path(),
            Path::new("/state/codexctl/hooks/lifecycle.lock")
        );
    }

    #[test]
    fn missing_then_recorded_snapshot_has_explicit_conditions() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        let missing = store.read().unwrap();
        assert_eq!(missing.condition, StoreCondition::Missing);
        assert!(missing.snapshot.is_none());

        assert_eq!(
            store.record_at(prompt("session-1", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        let healthy = store.read().unwrap();
        assert_eq!(healthy.condition, StoreCondition::Healthy);
        assert_eq!(
            healthy.snapshot.unwrap().sessions
                [&AgentSessionKey::native(AgentProvider::Codex, "session-1").storage_key()]
                .projected_status,
            Some(ProjectedStatus::Processing)
        );
    }

    #[test]
    fn recorded_event_returns_its_exact_sequence_under_the_store_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());

        let first = store
            .record_with_sequence_at(prompt("session-1", "turn-1"), 1_000)
            .unwrap();
        let second = store
            .record_with_sequence_at(prompt("session-2", "turn-1"), 1_001)
            .unwrap();

        assert_eq!(first.outcome, ApplyOutcome::Applied);
        assert_eq!(first.sequence, 1);
        assert_eq!(second.outcome, ApplyOutcome::Applied);
        assert_eq!(second.sequence, 2);
    }

    #[test]
    fn schema_one_snapshot_projects_bare_keys_as_codex_without_rewriting_on_read() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let mut legacy = LifecycleSnapshot {
            schema_version: 1,
            ..LifecycleSnapshot::default()
        };
        legacy.apply(prompt("legacy-session", "turn-1"), 1_000);
        let qualified =
            AgentSessionKey::native(AgentProvider::Codex, "legacy-session").storage_key();
        let state = legacy.sessions.remove(&qualified).unwrap();
        legacy.sessions.insert("legacy-session".into(), state);
        let original = serde_json::to_vec(&legacy).unwrap();
        fs::write(store.snapshot_path(), &original).unwrap();

        let view = store.read().unwrap();

        assert_eq!(view.condition, StoreCondition::Healthy);
        let projected = view.snapshot.unwrap();
        assert_eq!(projected.schema_version, LIFECYCLE_SCHEMA_VERSION);
        assert!(projected.sessions.contains_key(&qualified));
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), original);
    }

    #[test]
    fn newer_schema_is_read_only_and_byte_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let original = br#"{"schema_version":4}"#;
        fs::write(store.snapshot_path(), original).unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::NewerSchema(4));
        assert!(view.snapshot.is_none());
        assert_eq!(
            store.record_at(prompt("session-1", "turn-1"), 1_000),
            Err(StoreError::NewerSchema(4))
        );
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), original);
    }

    #[test]
    fn corrupt_snapshot_is_read_without_mutation_then_quarantined_on_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(store.snapshot_path(), b"not-json").unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::Corrupt);
        assert_eq!(fs::read(store.snapshot_path()).unwrap(), b"not-json");

        store
            .record_at(prompt("session-1", "turn-1"), 1_000)
            .unwrap();
        assert_eq!(store.read().unwrap().condition, StoreCondition::Healthy);
        let quarantines = store.corrupt_paths().unwrap();
        assert_eq!(quarantines.len(), 1);
        assert_eq!(fs::read(&quarantines[0]).unwrap(), b"not-json");
    }

    #[test]
    fn quarantine_retention_and_abandoned_temp_cleanup_are_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(
            store.hooks_dir().join("lifecycle.tmp-abandoned"),
            b"partial",
        )
        .unwrap();

        for index in 0..4 {
            fs::write(store.snapshot_path(), format!("corrupt-{index}")).unwrap();
            store
                .record_at(prompt(&format!("session-{index}"), "turn-1"), 1_000 + index)
                .unwrap();
        }

        assert!(!store.hooks_dir().join("lifecycle.tmp-abandoned").exists());
        assert_eq!(store.corrupt_paths().unwrap().len(), 3);
    }

    #[test]
    fn retention_prunes_old_sessions_and_capacity_rejects_new_active_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        store.record_at(prompt("old", "turn-1"), 1_000).unwrap();
        store
            .record_at(prompt("fresh", "turn-1"), SESSION_RETENTION_MS + 1_001)
            .unwrap();
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(!snapshot.sessions.contains_key(&key("old")));
        assert!(snapshot.sessions.contains_key(&key("fresh")));

        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        for index in 0..MAX_SESSIONS {
            store
                .record_at(prompt(&format!("session-{index}"), "turn-1"), 1_000)
                .unwrap();
        }
        assert_eq!(
            store.record_at(prompt("overflow", "turn-1"), 1_000),
            Err(StoreError::SessionCapacity)
        );
    }

    #[test]
    fn serialized_snapshot_limit_rejects_oversized_output() {
        assert_eq!(
            ensure_serialized_size(&vec![b'x'; MAX_SNAPSHOT_BYTES + 1]),
            Err(StoreError::SnapshotTooLarge)
        );
        assert!(ensure_serialized_size(&vec![b'x'; MAX_SNAPSHOT_BYTES]).is_ok());
    }

    #[test]
    fn loaded_snapshot_rejects_oversized_nested_identity() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("session-1", "turn-1"), 1_000);
        snapshot
            .sessions
            .get_mut(&key("session-1"))
            .unwrap()
            .current_turn = Some("x".repeat(super::super::MAX_ID_BYTES + 1));
        fs::write(
            store.snapshot_path(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        assert_eq!(store.read().unwrap().condition, StoreCondition::Corrupt);
    }

    #[test]
    fn schema_two_snapshot_defaults_absent_antigravity_state() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("session-1", "turn-1"), 1_000);
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let state = &json["sessions"][key("session-1")];
        assert!(state.get("antigravity_initial_step").is_none());
        assert!(state.get("antigravity_child_events").is_none());
        fs::write(store.snapshot_path(), bytes).unwrap();

        let loaded = store.read().unwrap().snapshot.unwrap();
        let state = &loaded.sessions[&key("session-1")];
        assert_eq!(state.antigravity_initial_step, None);
        assert!(state.antigravity_child_events.is_empty());
    }

    #[test]
    fn schema_three_snapshot_defaults_absent_permission_replay_state() {
        let value = snapshot_with_permission_events(json!({}));
        let mut legacy = value;
        legacy["sessions"][key("session-1")]
            .as_object_mut()
            .unwrap()
            .remove("permission_request_events");

        let store = store();
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(store.snapshot_path(), serde_json::to_vec(&legacy).unwrap()).unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::Healthy);
        assert!(
            view.snapshot.unwrap().sessions[&key("session-1")]
                .permission_request_events
                .is_empty()
        );
    }

    #[test]
    fn schema_three_snapshot_defaults_absent_stopped_subagents() {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(prompt("session-1", "turn-1"), 1_000),
            ApplyOutcome::Applied
        );
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["sessions"][key("session-1")]
            .as_object_mut()
            .unwrap()
            .remove("stopped_subagents");

        let store = store();
        fs::create_dir_all(store.hooks_dir()).unwrap();
        fs::write(store.snapshot_path(), serde_json::to_vec(&value).unwrap()).unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::Healthy);
        assert!(
            view.snapshot.unwrap().sessions[&key("session-1")]
                .stopped_subagents
                .is_empty()
        );
    }

    #[test]
    fn exact_newer_codex_resume_evidence_reactivates_the_child() {
        let path = transcript_path("rollout-child-1.jsonl");
        let (store, identity, mut evidence) = stopped_store(&path);
        evidence.requested_transcript_path = path
            .parent()
            .unwrap()
            .join("unused")
            .join("..")
            .join(path.file_name().unwrap());

        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
            Ok(ApplyOutcome::Applied)
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        let parent = &snapshot.sessions[&key("root-1")];
        assert_eq!(parent.active_subagents["child-1"].turn_id, "turn-2");
        assert_eq!(parent.active_subagents["child-1"].started_sequence, 3);
        assert_eq!(parent.latest_sequence, 3);
        assert_eq!(parent.latest_received_at_ms, 3_000);
        assert_eq!(
            parent.latest_event,
            Some(super::super::LifecycleEventName::SubagentStop)
        );
        assert!(!parent.stopped_subagents.contains_key("child-1"));

        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_001),
            Ok(ApplyOutcome::Ignored(IgnoreReason::Duplicate))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            snapshot.next_sequence
        );
        assert_eq!(
            store.record_at(permission(identity.clone()), 3_001),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(subagent_stop("root-1", "child-1", "turn-2"), 3_002),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(permission(identity), 3_003),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
    }

    #[test]
    fn nested_codex_resume_uses_root_shared_provider_topology() {
        let path = transcript_path("rollout-child-1.jsonl");
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root-1", "child-parent", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_start("child-parent", "root-1", "turn-1", "child-1"),
                1_500,
            ),
            Ok(ApplyOutcome::Applied)
        );
        let original_identity =
            linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-1", &path);
        assert_eq!(store.codex_subagent_is_proven(&original_identity), Ok(true));
        let wrong_root_identity =
            linked_identity(AgentProvider::Codex, "child-1", "root-2", "turn-1", &path);
        assert_eq!(
            store.codex_subagent_is_proven(&wrong_root_identity),
            Ok(false)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_stop("child-parent", "root-1", "turn-1", "child-1"),
                2_000,
            ),
            Ok(ApplyOutcome::Applied)
        );

        let resumed_identity =
            linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-2", &path);
        let mut evidence = resume_evidence("child-1", "root-1", "turn-2", &path, 2_500);
        evidence.parent_thread_id = Some("child-parent".into());
        let wrong_root_identity =
            linked_identity(AgentProvider::Codex, "child-1", "root-2", "turn-2", &path);
        let wrong_root_evidence = resume_evidence("child-1", "root-2", "turn-2", &path, 2_500);
        let before = store.read().unwrap().snapshot.unwrap().next_sequence;
        assert_eq!(
            store.reprove_codex_subagent_at(&wrong_root_identity, &wrong_root_evidence, 3_000,),
            Ok(ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before
        );
        assert_eq!(
            store.reprove_codex_subagent_at(&resumed_identity, &evidence, 3_000),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(store.codex_subagent_is_proven(&resumed_identity), Ok(true));
        assert_eq!(
            store.record_at(permission(resumed_identity), 3_001),
            Ok(ApplyOutcome::Applied)
        );
    }

    #[test]
    fn immediate_owner_cannot_reprove_root_shared_stopped_authority() {
        let path = transcript_path("rollout-child-1.jsonl");
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root-1", "child-parent", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_start("child-parent", "root-1", "turn-1", "child-1"),
                1_500,
            ),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_stop("child-parent", "root-1", "turn-1", "child-1"),
                2_000,
            ),
            Ok(ApplyOutcome::Applied)
        );

        let identity = linked_identity(
            AgentProvider::Codex,
            "child-1",
            "child-parent",
            "turn-2",
            &path,
        );
        let mut evidence = resume_evidence("child-1", "child-parent", "turn-2", &path, 2_500);
        evidence.parent_thread_id = Some("child-parent".into());
        let before = store.read().unwrap().snapshot.unwrap().next_sequence;
        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
            Ok(ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before
        );
    }

    #[test]
    fn immediate_owner_cannot_read_or_persist_root_shared_active_authority() {
        let path = transcript_path("rollout-child-1.jsonl");
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root-1", "child-parent", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_start("child-parent", "root-1", "turn-1", "child-1"),
                1_500,
            ),
            Ok(ApplyOutcome::Applied)
        );
        let identity = linked_identity(
            AgentProvider::Codex,
            "child-1",
            "child-parent",
            "turn-1",
            &path,
        );

        assert_eq!(store.codex_subagent_is_proven(&identity), Ok(false));
        let before = store.read().unwrap().snapshot.unwrap().next_sequence;
        assert_eq!(
            store.record_at(permission(identity), 2_000),
            Ok(ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before
        );
    }

    #[test]
    fn rejected_codex_resume_evidence_does_not_consume_a_sequence() {
        let path = transcript_path("rollout-child-1.jsonl");
        let other_path = transcript_path("rollout-other.jsonl");
        let (_, _, base) = stopped_store(&path);
        let mut cases = Vec::new();

        let mut evidence = base.clone();
        evidence.child_session_id = "child-2".into();
        cases.push(("child mismatch", evidence));
        let mut evidence = base.clone();
        evidence.provider_session_id = "root-2".into();
        cases.push(("provider session mismatch", evidence));
        let mut evidence = base.clone();
        evidence.turn_id = "turn-3".into();
        cases.push(("turn mismatch", evidence));
        let mut evidence = base.clone();
        evidence.requested_transcript_path = other_path.clone();
        cases.push(("requested transcript mismatch", evidence));
        let mut evidence = base.clone();
        evidence.canonical_transcript_path = fs::canonicalize(&other_path).unwrap();
        cases.push(("canonical transcript mismatch", evidence));
        let mut evidence = base.clone();
        evidence.started_at_ms = 2_000;
        cases.push(("timestamp equal to stop", evidence));
        let mut evidence = base.clone();
        evidence.started_at_ms = 1_999;
        cases.push(("timestamp before stop", evidence));
        let mut evidence = base.clone();
        evidence.started_at_ms = 8_001;
        cases.push(("timestamp too far in future", evidence));

        for (label, evidence) in cases {
            let (store, identity, _) = stopped_store(&path);
            let before = store.read().unwrap().snapshot.unwrap().next_sequence;
            assert_eq!(
                store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
                Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent)),
                "{label}"
            );
            let after = store.read().unwrap().snapshot.unwrap();
            assert_eq!(after.next_sequence, before, "{label}");
            assert!(
                after.sessions[&key("root-1")]
                    .stopped_subagents
                    .contains_key("child-1"),
                "{label}"
            );
        }
    }

    #[test]
    fn old_turn_never_proven_and_wrong_parent_resume_attempts_fail_closed() {
        let path = transcript_path("rollout-child-1.jsonl");
        let (lifecycle_store, _, _) = stopped_store(&path);
        let old_identity =
            linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-1", &path);
        let old_evidence = resume_evidence("child-1", "root-1", "turn-1", &path, 2_500);
        let before = lifecycle_store
            .read()
            .unwrap()
            .snapshot
            .unwrap()
            .next_sequence;
        assert_eq!(
            lifecycle_store.reprove_codex_subagent_at(&old_identity, &old_evidence, 3_000),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        assert_eq!(
            lifecycle_store
                .read()
                .unwrap()
                .snapshot
                .unwrap()
                .next_sequence,
            before
        );

        let never = store();
        let identity = linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-2", &path);
        let evidence = resume_evidence("child-1", "root-1", "turn-2", &path, 2_500);
        assert_eq!(
            never.reprove_codex_subagent_at(&identity, &evidence, 3_000),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );

        let wrong_parent =
            linked_identity(AgentProvider::Codex, "child-1", "root-2", "turn-2", &path);
        let wrong_parent_evidence = resume_evidence("child-1", "root-2", "turn-2", &path, 2_500);
        assert_eq!(
            lifecycle_store
                .reprove_codex_subagent_at(&wrong_parent, &wrong_parent_evidence, 3_000,),
            Ok(ApplyOutcome::Ignored(IgnoreReason::ProviderSessionMismatch))
        );
    }

    #[test]
    fn delayed_old_stop_after_reproof_cannot_remove_new_turn_authority() {
        let path = transcript_path("rollout-child-1.jsonl");
        let (store, identity, evidence) = stopped_store(&path);
        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
            Ok(ApplyOutcome::Applied)
        );

        assert_eq!(
            store.record_at(subagent_stop("root-1", "child-1", "turn-1"), 3_001),
            Ok(ApplyOutcome::Ignored(IgnoreReason::SubagentTurnMismatch))
        );
        assert_eq!(
            store.record_at(permission(identity), 3_002),
            Ok(ApplyOutcome::Applied)
        );
    }

    #[test]
    fn concurrent_new_turn_stop_wins_over_already_read_resume_evidence() {
        let path = transcript_path("rollout-child-1.jsonl");
        let (store, identity, evidence) = stopped_store(&path);

        assert_eq!(
            store.record_at(subagent_stop("root-1", "child-1", "turn-2"), 3_000),
            Ok(ApplyOutcome::Applied)
        );
        let stopped = store.read().unwrap().snapshot.unwrap();
        assert_eq!(stopped.next_sequence, 4);
        assert_eq!(
            stopped.sessions[&key("root-1")].stopped_subagents["child-1"].turn_id,
            "turn-2"
        );
        assert_eq!(
            stopped.sessions[&key("root-1")].stopped_subagents["child-1"].received_at_ms,
            3_000
        );

        let before_reproof = stopped.next_sequence;
        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_001),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before_reproof
        );

        assert_eq!(
            store.record_at(permission(identity), 3_002),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before_reproof
        );
    }

    #[test]
    fn reproof_rejects_active_capacity_and_expired_tombstone_without_a_sequence() {
        let path = transcript_path("rollout-child-1.jsonl");
        let (store, identity, evidence) = stopped_store(&path);
        for index in 0..MAX_ACTIVE_SUBAGENTS {
            assert_eq!(
                store.record_at(
                    subagent_start("root-1", &format!("other-{index}"), "other-turn"),
                    2_100 + index as u64,
                ),
                Ok(ApplyOutcome::Applied)
            );
        }
        let before = store.read().unwrap().snapshot.unwrap().next_sequence;
        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
            Ok(ApplyOutcome::Ignored(IgnoreReason::ActiveSubagentCapacity))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before
        );

        let (store, identity, evidence) = stopped_store(&path);
        assert_eq!(
            store.record_at(prompt("root-1", "root-turn"), SESSION_RETENTION_MS + 2_000),
            Ok(ApplyOutcome::Applied)
        );
        let before = store.read().unwrap().snapshot.unwrap().next_sequence;
        assert_eq!(
            store.reprove_codex_subagent_at(&identity, &evidence, SESSION_RETENTION_MS + 2_001,),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        assert_eq!(
            store.read().unwrap().snapshot.unwrap().next_sequence,
            before
        );
    }

    #[test]
    fn codex_subagent_proof_read_is_exact_and_fail_closed() {
        let path = transcript_path("rollout-child-1.jsonl");
        let lifecycle_store = store();
        assert_eq!(
            lifecycle_store.record_at(subagent_start("root-1", "child-1", "turn-1"), 1_000),
            Ok(ApplyOutcome::Applied)
        );
        let exact = linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-1", &path);
        assert_eq!(lifecycle_store.codex_subagent_is_proven(&exact), Ok(true));

        for identity in [
            linked_identity(AgentProvider::Codex, "child-2", "root-1", "turn-1", &path),
            linked_identity(AgentProvider::Codex, "child-1", "root-2", "turn-1", &path),
            linked_identity(AgentProvider::Codex, "child-1", "root-1", "turn-2", &path),
            linked_identity(AgentProvider::Claude, "child-1", "root-1", "turn-1", &path),
        ] {
            assert_eq!(
                lifecycle_store.codex_subagent_is_proven(&identity),
                Ok(false)
            );
        }

        assert_eq!(
            LifecycleStore::at(tempfile::tempdir().unwrap().path())
                .codex_subagent_is_proven(&exact),
            Ok(false)
        );

        let corrupt = store();
        fs::create_dir_all(corrupt.hooks_dir()).unwrap();
        fs::write(corrupt.snapshot_path(), b"not-json").unwrap();
        assert_eq!(corrupt.codex_subagent_is_proven(&exact), Ok(false));

        let unavailable_root = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            LifecycleStore::at(unavailable_root.path()).codex_subagent_is_proven(&exact),
            Err(StoreError::Io)
        );
    }

    #[test]
    fn malformed_permission_replay_keys_are_rejected() {
        for (label, request_key) in [
            ("empty key", String::new()),
            ("oversized key", "a".repeat(65)),
            ("non-hex key", "g".repeat(64)),
            ("uppercase key", "A".repeat(64)),
        ] {
            assert_schema_three_json_corrupt(
                snapshot_with_permission_events(json!({ request_key: 1 })),
                label,
            );
        }
    }

    #[test]
    fn oversized_permission_replay_state_is_rejected() {
        let events = (0..65)
            .map(|index| (format!("{index:064x}"), serde_json::Value::from(1)))
            .collect();
        assert_schema_three_json_corrupt(
            snapshot_with_permission_events(serde_json::Value::Object(events)),
            "permission replay capacity",
        );
    }

    #[test]
    fn invalid_permission_replay_bits_are_rejected() {
        let request_key = "a".repeat(64);
        for (label, bits) in [("zero bits", 0), ("unknown bits", 1 << 7)] {
            assert_schema_three_json_corrupt(
                snapshot_with_permission_events(json!({ request_key.clone(): bits })),
                label,
            );
        }
    }

    #[test]
    fn permission_replay_state_requires_an_open_current_turn() {
        let request_key = "a".repeat(64);
        let mut closed = snapshot_with_permission_events(json!({ request_key.clone(): 1 }));
        closed["sessions"][key("session-1")]["turn_open"] = json!(false);
        assert_schema_three_json_corrupt(closed, "closed turn");

        let mut no_current_turn = snapshot_with_permission_events(json!({ request_key: 1 }));
        no_current_turn["sessions"][key("session-1")]["current_turn"] = serde_json::Value::Null;
        assert_schema_three_json_corrupt(no_current_turn, "missing current turn");
    }

    #[test]
    fn schema_two_snapshot_defaults_linked_state() {
        let store = store();
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(prompt("session-1", "turn-1"), 1_000);
        snapshot.schema_version = 2;
        fs::write(
            store.snapshot_path(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();

        let view = store.read().unwrap();
        let snapshot = view.snapshot.unwrap();
        assert_eq!(view.condition, StoreCondition::Healthy);
        assert_eq!(snapshot.schema_version, 3);
        assert!(
            snapshot
                .sessions
                .values()
                .all(|state| state.provider_session_id.is_none())
        );
        assert!(
            snapshot
                .sessions
                .values()
                .all(|state| state.active_subagents.is_empty())
        );
    }

    #[test]
    fn linked_activity_refreshes_provider_topology_retention() {
        let store = store();
        store
            .record_at(subagent_start("root", "child-a", "turn-a"), 1)
            .unwrap();
        store
            .record_at(
                linked_tool("child-a", "root", "turn-a"),
                SESSION_RETENTION_MS,
            )
            .unwrap();

        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(snapshot.sessions.contains_key(&key("root")));
        assert!(snapshot.sessions.contains_key(&key("child-a")));
        assert_eq!(
            snapshot.sessions[&key("root")].latest_received_at_ms,
            SESSION_RETENTION_MS
        );
    }

    #[test]
    fn retention_removes_expired_provider_and_linked_children_atomically() {
        let store =
            store_with_schema_three_snapshot(expired_linked_group("root", "child-a", "turn-a"));
        store
            .record_at(prompt("other-root", "other-turn"), SESSION_RETENTION_MS + 1)
            .unwrap();

        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(!snapshot.sessions.contains_key(&key("root")));
        assert!(!snapshot.sessions.contains_key(&key("child-a")));
        assert!(snapshot.sessions.contains_key(&key("other-root")));
    }

    #[test]
    fn retention_removes_transitive_linked_descendants() {
        let mut snapshot = LifecycleSnapshot::default();
        assert_eq!(
            snapshot.apply(subagent_start("root", "child-a", "turn-a"), 0),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(
                linked_subagent_start("child-a", "root", "turn-a", "child-b"),
                0,
            ),
            ApplyOutcome::Applied
        );
        assert_eq!(
            snapshot.apply(
                linked_tool("child-b", "root", "turn-a"),
                SESSION_RETENTION_MS,
            ),
            ApplyOutcome::Applied
        );
        let store = store_with_schema_three_snapshot(snapshot);

        store
            .record_at(prompt("other-root", "other-turn"), SESSION_RETENTION_MS + 1)
            .unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::Healthy);
        let snapshot = view.snapshot.unwrap();
        assert!(!snapshot.sessions.contains_key(&key("root")));
        assert!(!snapshot.sessions.contains_key(&key("child-a")));
        assert!(!snapshot.sessions.contains_key(&key("child-b")));
        assert!(snapshot.sessions.contains_key(&key("other-root")));
    }

    #[test]
    fn retention_removes_root_shared_nested_descendants_with_expired_outer_edge() {
        let path = transcript_path("rollout-child-1.jsonl");
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root", "child-parent", "turn-1"), 0,),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_start("child-parent", "root", "turn-1", "child-1"),
                0,
            ),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                permission(linked_identity(
                    AgentProvider::Codex,
                    "child-1",
                    "root",
                    "turn-1",
                    &path,
                )),
                SESSION_RETENTION_MS,
            ),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(prompt("root", "root-turn"), SESSION_RETENTION_MS,),
            Ok(ApplyOutcome::Applied)
        );

        assert_eq!(
            store.record_at(prompt("other-root", "other-turn"), SESSION_RETENTION_MS + 1,),
            Ok(ApplyOutcome::Applied)
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(snapshot.sessions.contains_key(&key("root")));
        assert!(!snapshot.sessions.contains_key(&key("child-parent")));
        assert!(!snapshot.sessions.contains_key(&key("child-1")));
        assert!(snapshot.sessions[&key("root")].active_subagents.is_empty());
    }

    #[test]
    fn retention_does_not_treat_non_codex_active_ids_as_owned_sessions() {
        for provider in [AgentProvider::Claude, AgentProvider::Antigravity] {
            let mut snapshot = LifecycleSnapshot::default();
            assert_eq!(
                snapshot.apply(
                    provider_subagent_start(
                        provider,
                        "stale-session",
                        "fresh-session",
                        "stale-turn",
                    ),
                    0,
                ),
                ApplyOutcome::Applied
            );
            assert_eq!(
                snapshot.apply(
                    provider_prompt(provider, "fresh-session", "fresh-turn"),
                    SESSION_RETENTION_MS,
                ),
                ApplyOutcome::Applied
            );
            let store = store_with_schema_three_snapshot(snapshot);

            assert_eq!(
                store.record_at(
                    prompt("codex-other", "other-turn"),
                    SESSION_RETENTION_MS + 1,
                ),
                Ok(ApplyOutcome::Applied)
            );
            let snapshot = store.read().unwrap().snapshot.unwrap();
            let fresh_key = AgentSessionKey::native(provider, "fresh-session").storage_key();
            assert!(
                snapshot.sessions.contains_key(&fresh_key),
                "{provider:?} independent session was removed"
            );
        }
    }

    #[test]
    fn retention_expires_stopped_subagents_without_expiring_fresh_parent() {
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root", "child-a", "turn-a"), 1),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(subagent_stop("root", "child-a", "turn-a"), 2),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(prompt("root", "root-turn"), SESSION_RETENTION_MS + 2),
            Ok(ApplyOutcome::Applied)
        );
        assert!(
            store.read().unwrap().snapshot.unwrap().sessions[&key("root")]
                .stopped_subagents
                .contains_key("child-a")
        );

        assert_eq!(
            store.record_at(prompt("other-root", "other-turn"), SESSION_RETENTION_MS + 3,),
            Ok(ApplyOutcome::Applied)
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(snapshot.sessions.contains_key(&key("root")));
        assert!(snapshot.sessions[&key("root")].stopped_subagents.is_empty());
    }

    #[test]
    fn full_capacity_preserves_structured_unproven_child_ignore() {
        let mut snapshot = LifecycleSnapshot::default();
        for index in 0..MAX_SESSIONS {
            assert_eq!(
                snapshot.apply(prompt(&format!("session-{index}"), "turn-1"), 1),
                ApplyOutcome::Applied
            );
        }
        let store = store_with_schema_three_snapshot(snapshot);

        assert_eq!(
            store.record_with_sequence_at(linked_tool("child-a", "root", "turn-a"), 2),
            Ok(RecordedLifecycleEvent {
                outcome: ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent),
                sequence: 0,
            })
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert_eq!(snapshot.sessions.len(), MAX_SESSIONS);
        assert!(!snapshot.sessions.contains_key(&key("child-a")));

        assert_eq!(
            store.record_at(prompt("overflow", "turn-1"), 2),
            Err(StoreError::SessionCapacity)
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert_eq!(snapshot.sessions.len(), MAX_SESSIONS);
        assert!(!snapshot.sessions.contains_key(&key("overflow")));
    }

    #[test]
    fn expired_topology_rejects_delayed_child_callback_when_root_stays_live() {
        let store = store();
        store
            .record_at(subagent_start("root", "child-a", "turn-a"), 0)
            .unwrap();
        store
            .record_at(prompt("root", "root-turn"), SESSION_RETENTION_MS)
            .unwrap();

        assert_eq!(
            store.record_at(
                linked_tool("child-a", "root", "turn-a"),
                SESSION_RETENTION_MS + 1,
            ),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(snapshot.sessions.contains_key(&key("root")));
        assert!(!snapshot.sessions.contains_key(&key("child-a")));
        assert!(
            !snapshot.sessions[&key("root")]
                .active_subagents
                .contains_key("child-a")
        );
    }

    #[test]
    fn unproven_child_ignore_returns_reserved_sequence() {
        let store = store();

        assert_eq!(
            store.record_with_sequence_at(linked_tool("child-a", "root", "turn-a"), 1),
            Ok(RecordedLifecycleEvent {
                outcome: ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent),
                sequence: 0,
            })
        );
        assert_eq!(store.read().unwrap().condition, StoreCondition::Healthy);
        assert!(store.read().unwrap().snapshot.unwrap().sessions.is_empty());
    }

    #[test]
    fn malformed_antigravity_authority_state_is_rejected() {
        let assert_corrupt = |mutate: &dyn Fn(&mut LifecycleSnapshot), label: &str| {
            let temp = tempfile::tempdir().unwrap();
            let store = LifecycleStore::at(temp.path());
            fs::create_dir_all(store.hooks_dir()).unwrap();
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(antigravity_invocation(), 1_000);
            mutate(&mut snapshot);
            fs::write(
                store.snapshot_path(),
                serde_json::to_vec(&snapshot).unwrap(),
            )
            .unwrap();
            assert_eq!(
                store.read().unwrap().condition,
                StoreCondition::Corrupt,
                "{label}"
            );
        };

        assert_corrupt(
            &|snapshot| {
                let state = snapshot.sessions.get_mut(&antigravity_key()).unwrap();
                state.turn_open = false;
            },
            "closed invocation",
        );
        assert_corrupt(
            &|snapshot| {
                let state = snapshot.sessions.get_mut(&antigravity_key()).unwrap();
                state.current_turn = Some("turn-1".into());
            },
            "non-invocation turn",
        );
        assert_corrupt(
            &|snapshot| {
                snapshot
                    .sessions
                    .get_mut(&antigravity_key())
                    .unwrap()
                    .antigravity_child_events
                    .insert(4, 1);
            },
            "below-floor step",
        );
        assert_corrupt(
            &|snapshot| {
                snapshot
                    .sessions
                    .get_mut(&antigravity_key())
                    .unwrap()
                    .antigravity_child_events
                    .insert(5, 0);
            },
            "zero event bits",
        );
        assert_corrupt(
            &|snapshot| {
                snapshot
                    .sessions
                    .get_mut(&antigravity_key())
                    .unwrap()
                    .antigravity_child_events
                    .insert(5, 1 << 7);
            },
            "unknown event bits",
        );
        assert_corrupt(
            &|snapshot| {
                let state = snapshot.sessions.get_mut(&antigravity_key()).unwrap();
                for step in 5..=5 + super::super::MAX_ANTIGRAVITY_INVOCATION_STEPS as u64 {
                    state.antigravity_child_events.insert(step, 1);
                }
            },
            "child capacity",
        );
        assert_corrupt(
            &|snapshot| {
                let state = snapshot.sessions.get_mut(&antigravity_key()).unwrap();
                state.antigravity_initial_step = None;
                state.antigravity_child_events.insert(5, 1);
            },
            "children without floor",
        );
        assert_corrupt(
            &|snapshot| {
                let state = snapshot.sessions.remove(&antigravity_key()).unwrap();
                snapshot
                    .sessions
                    .insert(key("codex-session-with-ag-state"), state);
            },
            "non-Antigravity provider",
        );
    }

    #[test]
    fn schema_three_rejects_invalid_sequence_authority() {
        let valid = || {
            let mut snapshot = LifecycleSnapshot::default();
            assert_eq!(
                snapshot.apply(prompt("session-1", "turn-1"), 1),
                ApplyOutcome::Applied
            );
            snapshot
        };

        let mut snapshot = valid();
        snapshot.next_sequence = 0;
        assert_schema_three_corrupt(snapshot, "zero next sequence");

        let mut snapshot = valid();
        snapshot.next_sequence = u64::MAX;
        assert_schema_three_corrupt(snapshot, "maximum next sequence");

        let mut snapshot = valid();
        snapshot
            .sessions
            .get_mut(&key("session-1"))
            .unwrap()
            .latest_sequence = 0;
        assert_schema_three_corrupt(snapshot, "zero latest sequence");

        let mut snapshot = valid();
        snapshot
            .sessions
            .get_mut(&key("session-1"))
            .unwrap()
            .latest_sequence = snapshot.next_sequence;
        assert_schema_three_corrupt(snapshot, "future latest sequence");

        let mut snapshot = valid();
        snapshot
            .sessions
            .get_mut(&key("session-1"))
            .unwrap()
            .status_sequence = Some(0);
        assert_schema_three_corrupt(snapshot, "zero status sequence");

        let mut snapshot = valid();
        snapshot
            .sessions
            .get_mut(&key("session-1"))
            .unwrap()
            .status_sequence = Some(snapshot.next_sequence);
        assert_schema_three_corrupt(snapshot, "future status sequence");

        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .get_mut("child-a")
            .unwrap()
            .started_sequence = 0;
        assert_schema_three_corrupt(snapshot, "zero subagent start sequence");

        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        let next_sequence = snapshot.next_sequence;
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .get_mut("child-a")
            .unwrap()
            .started_sequence = next_sequence;
        assert_schema_three_corrupt(snapshot, "future subagent start sequence");

        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot.apply(subagent_stop("root", "child-a", "turn-a"), 2);
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .stopped_subagents
            .get_mut("child-a")
            .unwrap()
            .stopped_sequence = 0;
        assert_schema_three_corrupt(snapshot, "zero subagent stop sequence");

        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot.apply(subagent_stop("root", "child-a", "turn-a"), 2);
        let next_sequence = snapshot.next_sequence;
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .stopped_subagents
            .get_mut("child-a")
            .unwrap()
            .stopped_sequence = next_sequence;
        assert_schema_three_corrupt(snapshot, "future subagent stop sequence");
    }

    #[test]
    fn schema_three_rejects_invalid_stopped_topology() {
        let stopped = || {
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
            snapshot.apply(subagent_stop("root", "child-a", "turn-a"), 2);
            snapshot
        };

        let mut snapshot = stopped();
        let tombstone = snapshot.sessions[&key("root")].stopped_subagents["child-a"].clone();
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .stopped_subagents
            .insert("x".repeat(super::super::MAX_ID_BYTES + 1), tombstone);
        assert_schema_three_corrupt(snapshot, "oversized stopped child id");

        let mut snapshot = stopped();
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .stopped_subagents
            .get_mut("child-a")
            .unwrap()
            .turn_id = "x".repeat(super::super::MAX_ID_BYTES + 1);
        assert_schema_three_corrupt(snapshot, "oversized stopped child turn");

        let mut snapshot = stopped();
        let tombstone = snapshot.sessions[&key("root")].stopped_subagents["child-a"].clone();
        let state = snapshot.sessions.get_mut(&key("root")).unwrap();
        for index in 0..=MAX_ACTIVE_SUBAGENTS {
            state
                .stopped_subagents
                .insert(format!("child-{index}"), tombstone.clone());
        }
        assert_schema_three_corrupt(snapshot, "stopped child capacity");

        let mut snapshot = stopped();
        let tombstone = snapshot.sessions[&key("root")].stopped_subagents["child-a"].clone();
        let state = snapshot.sessions.get_mut(&key("root")).unwrap();
        state.active_subagents.insert(
            "child-a".into(),
            super::super::ActiveSubagentState {
                started_sequence: tombstone.stopped_sequence,
                received_at_ms: tombstone.received_at_ms,
                turn_id: "turn-b".into(),
            },
        );
        assert_schema_three_corrupt(snapshot, "active and stopped child overlap");

        let mut snapshot = stopped();
        let tombstone = snapshot.sessions[&key("root")].stopped_subagents["child-a"].clone();
        snapshot.apply(subagent_start("other-root", "child-a", "turn-b"), 3);
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .stopped_subagents
            .insert("child-a".into(), tombstone);
        assert_schema_three_corrupt(snapshot, "active and stopped child across roots");
    }

    #[test]
    fn schema_three_rejects_duplicate_child_topology_within_provider() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root-a", "child-a", "turn-a"), 1);
        snapshot.apply(prompt("root-b", "root-turn"), 2);
        let active = snapshot.sessions[&key("root-a")].active_subagents["child-a"].clone();
        snapshot
            .sessions
            .get_mut(&key("root-b"))
            .unwrap()
            .active_subagents
            .insert("child-a".into(), active);

        assert_schema_three_corrupt(snapshot, "duplicate child topology");
    }

    #[test]
    fn schema_three_rejects_missing_reverse_topology_and_turn_mismatch() {
        let linked = || {
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
            snapshot.apply(linked_tool("child-a", "root", "turn-a"), 2);
            snapshot
        };

        let mut snapshot = linked();
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .remove("child-a");
        assert_schema_three_corrupt(snapshot, "missing reverse topology");

        let mut snapshot = linked();
        snapshot
            .sessions
            .get_mut(&key("child-a"))
            .unwrap()
            .current_turn = Some("other-turn".into());
        assert_schema_three_corrupt(snapshot, "linked turn mismatch");
    }

    #[test]
    fn schema_three_rejects_linked_cycles() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot.apply(linked_tool("child-a", "root", "turn-a"), 2);
        let root_sequence = snapshot.sessions[&key("root")].latest_sequence;
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .provider_session_id = Some("child-a".into());
        snapshot
            .sessions
            .get_mut(&key("child-a"))
            .unwrap()
            .active_subagents
            .insert(
                "root".into(),
                super::super::ActiveSubagentState {
                    started_sequence: root_sequence,
                    received_at_ms: 2,
                    turn_id: "turn-a".into(),
                },
            );

        assert_schema_three_corrupt(snapshot, "linked cycle");
    }

    #[test]
    fn schema_three_rejects_root_shared_active_ownership_cycles() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot.apply(
            linked_subagent_start("child-a", "root", "turn-a", "child-b"),
            2,
        );
        snapshot.apply(linked_tool("child-b", "root", "turn-a"), 3);
        let child_a = snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .remove("child-a")
            .unwrap();
        snapshot
            .sessions
            .get_mut(&key("child-b"))
            .unwrap()
            .active_subagents
            .insert("child-a".into(), child_a);

        assert_schema_three_corrupt(snapshot, "root-shared active ownership cycle");
    }

    #[test]
    fn schema_three_accepts_non_codex_active_id_cycles_as_independent_state() {
        for provider in [AgentProvider::Claude, AgentProvider::Antigravity] {
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(
                provider_subagent_start(provider, "session-a", "session-b", "turn-a"),
                1,
            );
            snapshot.apply(
                provider_subagent_start(provider, "session-b", "session-a", "turn-b"),
                2,
            );

            assert_eq!(
                store_with_schema_three_snapshot(snapshot)
                    .read()
                    .unwrap()
                    .condition,
                StoreCondition::Healthy,
                "{provider:?} active IDs are not lifecycle ownership edges"
            );
        }
    }

    #[test]
    fn schema_three_keeps_non_codex_reverse_edges_direct() {
        for provider in [AgentProvider::Claude, AgentProvider::Antigravity] {
            let mut snapshot = LifecycleSnapshot::default();
            snapshot.apply(
                provider_subagent_start(provider, "root", "child-parent", "turn-a"),
                1,
            );
            snapshot.apply(
                provider_linked_event(
                    provider,
                    "child-parent",
                    "root",
                    "turn-a",
                    super::super::LifecycleEventKind::SubagentStart {
                        agent_id: "child-a".into(),
                    },
                ),
                2,
            );
            snapshot.apply(
                provider_linked_event(
                    provider,
                    "child-a",
                    "root",
                    "turn-a",
                    super::super::LifecycleEventKind::PreToolUse,
                ),
                3,
            );

            assert_schema_three_corrupt(snapshot, "non-Codex reverse edge is not direct");
        }
    }

    #[test]
    fn schema_three_rejects_oversized_active_topology_identity() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        let active = snapshot.sessions[&key("root")].active_subagents["child-a"].clone();
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .insert("x".repeat(super::super::MAX_ID_BYTES + 1), active);
        assert_schema_three_corrupt(snapshot, "oversized child id");

        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot
            .sessions
            .get_mut(&key("root"))
            .unwrap()
            .active_subagents
            .get_mut("child-a")
            .unwrap()
            .turn_id = "x".repeat(super::super::MAX_ID_BYTES + 1);
        assert_schema_three_corrupt(snapshot, "oversized child turn");
    }

    #[test]
    fn valid_nested_linked_chain_remains_healthy() {
        let mut snapshot = LifecycleSnapshot::default();
        snapshot.apply(subagent_start("root", "child-a", "turn-a"), 1);
        snapshot.apply(
            linked_subagent_start("child-a", "root", "turn-a", "child-b"),
            2,
        );
        assert_eq!(
            snapshot.apply(linked_tool("child-b", "root", "turn-a"), 3),
            ApplyOutcome::Applied
        );

        assert_eq!(
            store_with_schema_three_snapshot(snapshot)
                .read()
                .unwrap()
                .condition,
            StoreCondition::Healthy
        );
    }

    #[test]
    fn linked_child_stop_persists_stopped_parent_and_removes_descendants() {
        let store = store();
        assert_eq!(
            store.record_at(subagent_start("root", "child-a", "turn-a"), 1),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(
                linked_subagent_start("child-a", "root", "turn-a", "child-b"),
                2,
            ),
            Ok(ApplyOutcome::Applied)
        );
        assert_eq!(
            store.record_at(linked_tool("child-b", "root", "turn-a"), 3),
            Ok(ApplyOutcome::Applied)
        );

        assert_eq!(
            store.record_at(linked_stop("child-a", "root", "turn-a"), 4),
            Ok(ApplyOutcome::Applied)
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        let child_a = &snapshot.sessions[&key("child-a")];
        assert!(!child_a.turn_open);
        assert_eq!(child_a.projected_status, Some(ProjectedStatus::Idle));
        assert!(child_a.active_subagents.is_empty());
        assert!(!snapshot.sessions.contains_key(&key("child-b")));

        assert_eq!(
            store.record_at(linked_tool("child-b", "root", "turn-a"), 5),
            Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
        );
        let snapshot = store.read().unwrap().snapshot.unwrap();
        assert!(snapshot.sessions.contains_key(&key("child-a")));
        assert!(!snapshot.sessions.contains_key(&key("child-b")));
    }

    #[test]
    fn atomic_replacement_never_exposes_partial_json() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        store
            .record_at(prompt("session-0", "turn-1"), 1_000)
            .unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let writer_done = Arc::clone(&done);
        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            for index in 1..40 {
                writer_store
                    .record_at(prompt(&format!("session-{index}"), "turn-1"), 1_000 + index)
                    .unwrap();
            }
            writer_done.store(true, Ordering::Release);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Acquire) && Instant::now() < deadline {
            let bytes = fs::read(store.snapshot_path()).unwrap();
            serde_json::from_slice::<LifecycleSnapshot>(&bytes).unwrap();
        }
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_enforces_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        store
            .record_at(prompt("session-1", "turn-1"), 1_000)
            .unwrap();

        assert_eq!(
            fs::metadata(store.hooks_dir())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store.lock_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(store.snapshot_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
