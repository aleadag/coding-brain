use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};

pub const SESSION_IDENTITY_LINK_SCHEMA_VERSION: u32 = 1;
pub const MAX_SESSION_LINK_LOG_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_RETAINED_SESSION_LINKS: usize = 10_000;
const MAX_SESSION_LINK_ROW_BYTES: usize = 64 * 1024;
const MAX_READ_BYTES: u64 = MAX_SESSION_LINK_LOG_BYTES * 2;
const MAX_NATIVE_SESSION_ID_BYTES: usize = 512;
const LOCK_RETRY: Duration = Duration::from_millis(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentityLink {
    pub schema_version: u32,
    pub recorded_at_ms: u64,
    pub provider: AgentProvider,
    pub native_session_id: String,
    pub live_process: LiveProcessIdentity,
}

#[derive(Debug, Clone)]
pub struct SessionLinkLimits {
    pub lock_timeout_ms: u64,
    pub compact_at_bytes: u64,
    pub retained_links: usize,
}

impl Default for SessionLinkLimits {
    fn default() -> Self {
        Self {
            lock_timeout_ms: 100,
            compact_at_bytes: MAX_SESSION_LINK_LOG_BYTES,
            retained_links: MAX_RETAINED_SESSION_LINKS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionLinkStore {
    path: PathBuf,
    lock_path: PathBuf,
    limits: SessionLinkLimits,
}

impl SessionLinkStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            limits: SessionLinkLimits::default(),
        }
    }

    pub fn with_limits(mut self, limits: SessionLinkLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn append(&self, link: SessionIdentityLink) -> Result<(), SessionLinkStoreError> {
        validate_link(&link)?;
        let mut row =
            serde_json::to_vec(&link).map_err(|_| SessionLinkStoreError::Serialization)?;
        if row.len() > MAX_SESSION_LINK_ROW_BYTES {
            return Err(SessionLinkStoreError::RowTooLarge);
        }
        row.push(b'\n');

        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Exclusive)?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|_| SessionLinkStoreError::Io)?;
        set_file_mode(&file)?;
        repair_tail(&mut file)?;
        file.seek(SeekFrom::End(0))
            .map_err(|_| SessionLinkStoreError::Io)?;
        file.write_all(&row)
            .map_err(|_| SessionLinkStoreError::Io)?;
        file.flush().map_err(|_| SessionLinkStoreError::Io)?;
        file.sync_data().map_err(|_| SessionLinkStoreError::Io)?;
        if file
            .metadata()
            .map_err(|_| SessionLinkStoreError::Io)?
            .len()
            >= self.limits.compact_at_bytes
        {
            drop(file);
            self.compact_locked()?;
        }
        Ok(())
    }

    pub fn read_projection(&self) -> Result<SessionIdentityProjection, SessionLinkStoreError> {
        let lock = self.open_lock()?;
        let _guard = lock_with_timeout(&lock, self.limits.lock_timeout_ms, LockKind::Shared)?;
        let mut projection = SessionIdentityProjection::default();
        for link in self.read_links()? {
            projection.apply(link);
        }
        Ok(projection)
    }

    fn compact_locked(&self) -> Result<(), SessionLinkStoreError> {
        let links = self.read_links()?;
        let mut seen = HashSet::new();
        let mut retained = links
            .into_iter()
            .rev()
            .filter(|link| {
                seen.insert((
                    AgentSessionKey::native(link.provider, &link.native_session_id),
                    link.live_process.clone(),
                ))
            })
            .take(self.limits.retained_links)
            .collect::<Vec<_>>();
        retained.reverse();

        let parent = parent_dir(&self.path);
        let mut temp = tempfile::Builder::new()
            .prefix("session-links.tmp-")
            .tempfile_in(parent)
            .map_err(|_| SessionLinkStoreError::Io)?;
        set_file_mode(temp.as_file())?;
        for link in retained {
            serde_json::to_writer(&mut temp, &link)
                .map_err(|_| SessionLinkStoreError::Serialization)?;
            temp.write_all(b"\n")
                .map_err(|_| SessionLinkStoreError::Io)?;
        }
        temp.flush().map_err(|_| SessionLinkStoreError::Io)?;
        temp.as_file()
            .sync_data()
            .map_err(|_| SessionLinkStoreError::Io)?;
        temp.persist(&self.path)
            .map_err(|_| SessionLinkStoreError::Io)?;
        Ok(())
    }

    fn read_links(&self) -> Result<Vec<SessionIdentityLink>, SessionLinkStoreError> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(SessionLinkStoreError::Io),
        };
        if file
            .metadata()
            .map_err(|_| SessionLinkStoreError::Io)?
            .len()
            > MAX_READ_BYTES
        {
            return Err(SessionLinkStoreError::LogTooLarge);
        }
        let mut rows = Vec::new();
        let mut reader = BufReader::new(file);
        let mut row = Vec::new();
        loop {
            row.clear();
            let bytes = reader
                .read_until(b'\n', &mut row)
                .map_err(|_| SessionLinkStoreError::Io)?;
            if bytes == 0 {
                break;
            }
            if row.last() != Some(&b'\n') {
                continue;
            }
            if row.len() > MAX_SESSION_LINK_ROW_BYTES + 1 {
                return Err(SessionLinkStoreError::RowTooLarge);
            }
            row.pop();
            let link = serde_json::from_slice::<SessionIdentityLink>(&row)
                .map_err(|_| SessionLinkStoreError::Serialization)?;
            validate_link(&link)?;
            rows.push(link);
        }
        Ok(rows)
    }

    fn open_lock(&self) -> Result<File, SessionLinkStoreError> {
        let parent = parent_dir(&self.path);
        fs::create_dir_all(parent).map_err(|_| SessionLinkStoreError::Io)?;
        set_dir_mode(parent)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|_| SessionLinkStoreError::Io)?;
        set_file_mode(&lock)?;
        Ok(lock)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionIdentityProjection {
    native_to_live: HashMap<AgentSessionKey, LiveProcessIdentity>,
    live_to_native: HashMap<LiveProcessIdentity, AgentSessionKey>,
}

impl SessionIdentityProjection {
    pub fn native_for(&self, live_process: &LiveProcessIdentity) -> Option<&str> {
        self.live_to_native
            .get(live_process)
            .map(|key| key.session_id.as_str())
    }

    pub fn live_for(&self, native: &AgentSessionKey) -> Option<&LiveProcessIdentity> {
        self.native_to_live.get(native)
    }

    fn apply(&mut self, link: SessionIdentityLink) {
        let native = AgentSessionKey::native(link.provider, link.native_session_id);
        if let Some(previous_live) = self
            .native_to_live
            .insert(native.clone(), link.live_process.clone())
        {
            self.live_to_native.remove(&previous_live);
        }
        if let Some(previous_native) = self
            .live_to_native
            .insert(link.live_process.clone(), native.clone())
            && previous_native != native
        {
            self.native_to_live.remove(&previous_native);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLinkStoreError {
    Io,
    LockTimeout,
    InvalidLink,
    UnsupportedSchema(u32),
    Serialization,
    RowTooLarge,
    LogTooLarge,
}

impl fmt::Display for SessionLinkStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => formatter.write_str("session identity link store I/O failed"),
            Self::LockTimeout => formatter.write_str("session identity link store lock timed out"),
            Self::InvalidLink => {
                formatter.write_str("session identity link is incomplete or inconsistent")
            }
            Self::UnsupportedSchema(version) => {
                write!(
                    formatter,
                    "unsupported session identity link schema {version}"
                )
            }
            Self::Serialization => {
                formatter.write_str("session identity link serialization failed")
            }
            Self::RowTooLarge => {
                formatter.write_str("session identity link exceeds its size limit")
            }
            Self::LogTooLarge => {
                formatter.write_str("session identity link log exceeds its read limit")
            }
        }
    }
}

impl std::error::Error for SessionLinkStoreError {}

fn validate_link(link: &SessionIdentityLink) -> Result<(), SessionLinkStoreError> {
    if link.schema_version != SESSION_IDENTITY_LINK_SCHEMA_VERSION {
        return Err(SessionLinkStoreError::UnsupportedSchema(
            link.schema_version,
        ));
    }
    if link.provider != link.live_process.provider
        || link.native_session_id.is_empty()
        || link.native_session_id.len() > MAX_NATIVE_SESSION_ID_BYTES
        || link.native_session_id.trim() != link.native_session_id
    {
        return Err(SessionLinkStoreError::InvalidLink);
    }
    let canonical_live_process = LiveProcessIdentity::try_new(
        link.live_process.provider,
        link.live_process.pid,
        link.live_process.process_start_identity,
        &link.live_process.tty,
    )
    .ok_or(SessionLinkStoreError::InvalidLink)?;
    if canonical_live_process != link.live_process {
        return Err(SessionLinkStoreError::InvalidLink);
    }
    Ok(())
}

fn repair_tail(file: &mut File) -> Result<(), SessionLinkStoreError> {
    let len = file
        .metadata()
        .map_err(|_| SessionLinkStoreError::Io)?
        .len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| SessionLinkStoreError::Io)?;
    let mut bytes = Vec::new();
    Read::by_ref(file)
        .take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SessionLinkStoreError::Io)?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(SessionLinkStoreError::LogTooLarge);
    }
    if bytes.last() == Some(&b'\n') {
        return Ok(());
    }
    let repaired_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    file.set_len(repaired_len as u64)
        .map_err(|_| SessionLinkStoreError::Io)
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
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

fn lock_with_timeout(
    file: &File,
    timeout_ms: u64,
    kind: LockKind,
) -> Result<LockGuard<'_>, SessionLinkStoreError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let result = match kind {
            LockKind::Shared => FileExt::try_lock_shared(file),
            LockKind::Exclusive => FileExt::try_lock_exclusive(file),
        };
        match result {
            Ok(()) => return Ok(LockGuard { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(SessionLinkStoreError::LockTimeout);
                }
                thread::sleep(LOCK_RETRY);
            }
            Err(_) => return Err(SessionLinkStoreError::Io),
        }
    }
}

#[cfg(unix)]
fn set_dir_mode(path: &Path) -> Result<(), SessionLinkStoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| SessionLinkStoreError::Io)
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path) -> Result<(), SessionLinkStoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &File) -> Result<(), SessionLinkStoreError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| SessionLinkStoreError::Io)
}

#[cfg(not(unix))]
fn set_file_mode(_file: &File) -> Result<(), SessionLinkStoreError> {
    Ok(())
}
