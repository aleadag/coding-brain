use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::Deserialize;

use super::{
    ApplyOutcome, LIFECYCLE_SCHEMA_VERSION, LifecycleEvent, LifecycleSnapshot,
    MAX_ACTIVE_SUBAGENTS, MAX_RECENT_TURNS,
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

    pub fn record(&self, event: LifecycleEvent) -> Result<ApplyOutcome, StoreError> {
        self.record_at(event, epoch_ms())
    }

    fn record_at(
        &self,
        event: LifecycleEvent,
        received_at_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, LockKind::Exclusive)?;
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

        snapshot.sessions.retain(|_, state| {
            received_at_ms.saturating_sub(state.latest_received_at_ms) <= SESSION_RETENTION_MS
        });
        if !snapshot
            .sessions
            .contains_key(event.identity().session_id())
            && snapshot.sessions.len() >= MAX_SESSIONS
        {
            return Err(StoreError::SessionCapacity);
        }

        let outcome = snapshot.apply(event, received_at_ms);
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| StoreError::Serialization)?;
        ensure_serialized_size(&bytes)?;
        self.persist(&bytes)?;
        Ok(outcome)
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
        if header.schema_version != LIFECYCLE_SCHEMA_VERSION {
            return Ok(LoadedSnapshot::Corrupt);
        }
        let Ok(snapshot) = serde_json::from_slice::<LifecycleSnapshot>(&bytes) else {
            return Ok(LoadedSnapshot::Corrupt);
        };
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
    snapshot.schema_version == LIFECYCLE_SCHEMA_VERSION
        && snapshot.sessions.len() <= MAX_SESSIONS
        && snapshot.next_sequence
            > snapshot
                .sessions
                .values()
                .map(|state| state.latest_sequence)
                .max()
                .unwrap_or(0)
        && snapshot.sessions.iter().all(|(session_id, state)| {
            valid_id(session_id)
                && valid_path(&state.cwd)
                && state.transcript_path.as_deref().is_none_or(valid_path)
                && state.current_turn.as_deref().is_none_or(valid_id)
                && state.recent_turns.len() <= MAX_RECENT_TURNS
                && state.recent_turns.iter().all(|turn| valid_id(turn))
                && state.active_subagents.len() <= MAX_ACTIVE_SUBAGENTS
                && state
                    .active_subagents
                    .keys()
                    .all(|agent_id| valid_id(agent_id))
        })
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

    use super::super::ProjectedStatus;
    use super::*;

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
            healthy.snapshot.unwrap().sessions["session-1"].projected_status,
            Some(ProjectedStatus::Processing)
        );
    }

    #[test]
    fn newer_schema_is_read_only_and_byte_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let store = LifecycleStore::at(temp.path());
        fs::create_dir_all(store.hooks_dir()).unwrap();
        let original = br#"{"schema_version":2}"#;
        fs::write(store.snapshot_path(), original).unwrap();

        let view = store.read().unwrap();
        assert_eq!(view.condition, StoreCondition::NewerSchema(2));
        assert!(view.snapshot.is_none());
        assert_eq!(
            store.record_at(prompt("session-1", "turn-1"), 1_000),
            Err(StoreError::NewerSchema(2))
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
        assert!(!snapshot.sessions.contains_key("old"));
        assert!(snapshot.sessions.contains_key("fresh"));

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
        snapshot.sessions.get_mut("session-1").unwrap().current_turn =
            Some("x".repeat(super::super::MAX_ID_BYTES + 1));
        fs::write(
            store.snapshot_path(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        assert_eq!(store.read().unwrap().condition, StoreCondition::Corrupt);
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
