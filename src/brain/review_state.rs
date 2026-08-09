#![allow(dead_code)] // Review projections and runtime mutations are wired in later tasks.

use std::collections::{BTreeMap, BTreeSet, HashSet};
#[cfg(test)]
use std::ffi::{CStr, CString};
use std::fmt;
#[cfg(test)]
use std::fs::File;
use std::io;
#[cfg(test)]
use std::io::{Read, Write};
#[cfg(test)]
use std::os::unix::fs::MetadataExt;
#[cfg(test)]
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::thread;
#[cfg(test)]
use std::time::{Duration, Instant};

use coding_brain_core::review_state::{
    MAX_REVIEW_KEYS, MAX_REVIEW_STATE_BYTES, ReviewDisposition, ReviewKey, ReviewMutation,
    ReviewMutationRequest, ReviewMutationResult, ReviewRequestError, ReviewSurface,
};
#[cfg(test)]
use fs2::FileExt;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::secure_state::{SecureEntryMetadata, SecureStateDirectory, SecureStateError};

const REVIEW_STATE_SCHEMA_VERSION: u32 = 1;
#[cfg(test)]
const STATE_NAME: &CStr = c"review-state.json";
#[cfg(test)]
const LOCK_NAME: &CStr = c"review-state.lock";
#[cfg(test)]
const LOCK_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(test)]
const LOCK_RETRY: Duration = Duration::from_millis(5);
#[cfg(test)]
const TEMP_ATTEMPTS: usize = 128;
#[cfg(test)]
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) enum ReviewStateError {
    Busy,
    DurabilityUncertain,
    InvalidRequest(ReviewRequestError),
    StaleRevision,
    TargetNotEligible,
    CountMismatch,
    DispositionConflict,
    CapacityExceeded,
    RevisionOverflow,
    StateTooLarge,
    UnsupportedSchema(u32),
    InvalidStorage(&'static str),
    Serialization(serde_json::Error),
    Io(io::Error),
}

impl fmt::Display for ReviewStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("review state is busy"),
            Self::DurabilityUncertain => {
                formatter.write_str("review state replacement durability is uncertain")
            }
            Self::InvalidRequest(error) => write!(formatter, "invalid review request: {error:?}"),
            Self::StaleRevision => formatter.write_str("review surface revision changed"),
            Self::TargetNotEligible => formatter.write_str("review target is no longer eligible"),
            Self::CountMismatch => formatter.write_str("review target count changed"),
            Self::DispositionConflict => formatter.write_str("review target disposition changed"),
            Self::CapacityExceeded => formatter.write_str("review state key limit exceeded"),
            Self::RevisionOverflow => formatter.write_str("review surface revision overflow"),
            Self::StateTooLarge => formatter.write_str("review state exceeds its byte limit"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported review state schema {version}")
            }
            Self::InvalidStorage(reason) => write!(formatter, "invalid review storage: {reason}"),
            Self::Serialization(error) => write!(formatter, "invalid review state JSON: {error}"),
            Self::Io(error) => write!(formatter, "review state I/O failed: {error}"),
        }
    }
}

impl std::error::Error for ReviewStateError {}

impl From<io::Error> for ReviewStateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ReviewStateError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

#[cfg(test)]
impl From<SecureStateError> for ReviewStateError {
    fn from(error: SecureStateError) -> Self {
        match error {
            SecureStateError::Io(error) => Self::Io(error),
            SecureStateError::InvalidStorage(reason) => Self::InvalidStorage(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SurfaceState {
    revision: u64,
    items: BTreeMap<ReviewKey, ReviewDisposition>,
    last_archive: BTreeSet<ReviewKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewStateSnapshot {
    surfaces: BTreeMap<ReviewSurface, SurfaceState>,
}

impl Default for ReviewStateSnapshot {
    fn default() -> Self {
        Self {
            surfaces: all_surfaces()
                .map(|surface| (surface, SurfaceState::default()))
                .collect(),
        }
    }
}

impl ReviewStateSnapshot {
    pub(crate) fn from_sqlite_surfaces(
        surfaces: impl IntoIterator<Item = crate::brain::storage::ReviewSurfaceState>,
    ) -> Result<Self, &'static str> {
        let mut projected = BTreeMap::new();
        for surface in surfaces {
            let key = surface.surface();
            let state = SurfaceState {
                revision: surface.surface_revision(),
                items: surface.dispositions().collect(),
                last_archive: surface.last_archive().collect(),
            };
            if projected.insert(key, state).is_some() {
                return Err("duplicate SQLite review surface");
            }
        }
        if projected.len() != all_surfaces().count()
            || all_surfaces().any(|surface| !projected.contains_key(&surface))
        {
            return Err("incomplete SQLite review surfaces");
        }
        Ok(Self {
            surfaces: projected,
        })
    }

    pub(crate) fn surface_revision(&self, surface: ReviewSurface) -> u64 {
        self.surface(surface).revision
    }

    pub(crate) fn disposition(
        &self,
        surface: ReviewSurface,
        key: &ReviewKey,
    ) -> Option<ReviewDisposition> {
        self.surface(surface).items.get(key).copied()
    }

    pub(crate) fn last_archive(&self, surface: ReviewSurface) -> &BTreeSet<ReviewKey> {
        &self.surface(surface).last_archive
    }

    pub(crate) fn reviewed_count(&self, surface: ReviewSurface) -> usize {
        disposition_count(&self.surface(surface).items, ReviewDisposition::Reviewed)
    }

    pub(crate) fn archived_count(&self, surface: ReviewSurface) -> usize {
        disposition_count(&self.surface(surface).items, ReviewDisposition::Archived)
    }

    pub(crate) fn items(
        &self,
        surface: ReviewSurface,
    ) -> impl Iterator<Item = (&ReviewKey, ReviewDisposition)> {
        self.surface(surface)
            .items
            .iter()
            .map(|(key, disposition)| (key, *disposition))
    }

    fn surface(&self, surface: ReviewSurface) -> &SurfaceState {
        self.surfaces
            .get(&surface)
            .expect("review snapshot contains every closed surface")
    }
}

#[cfg(test)]
pub(crate) struct ReviewStateStore {
    state_root: PathBuf,
}

#[cfg(test)]
impl ReviewStateStore {
    pub(crate) fn at(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_owned(),
        }
    }

    pub(crate) fn read(&self) -> Result<ReviewStateSnapshot, ReviewStateError> {
        let directory = SecureStateDirectory::open_or_create(&self.state_root)?;
        let lock = open_exact_regular(&directory, LOCK_NAME, true)?
            .expect("creating the review lock always returns a file");
        let _guard = lock_exclusive(&directory, &lock)?;
        let snapshot = read_snapshot(&directory)?;
        directory.validate_regular(LOCK_NAME, &lock, 0o600)?;
        Ok(snapshot)
    }

    pub(crate) fn mutate(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
    ) -> Result<ReviewMutationResult, ReviewStateError> {
        self.mutate_with_syncs(
            request,
            eligible,
            File::sync_all,
            SecureStateDirectory::sync,
        )
    }

    fn mutate_with_directory_sync<F>(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
        sync_directory: F,
    ) -> Result<ReviewMutationResult, ReviewStateError>
    where
        F: FnOnce(&SecureStateDirectory) -> io::Result<()>,
    {
        self.mutate_with_syncs(request, eligible, File::sync_all, sync_directory)
    }

    fn mutate_with_syncs<F, D>(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
        sync_file: F,
        sync_directory: D,
    ) -> Result<ReviewMutationResult, ReviewStateError>
    where
        F: FnOnce(&File) -> io::Result<()>,
        D: FnOnce(&SecureStateDirectory) -> io::Result<()>,
    {
        self.mutate_with_storage_hooks(
            request,
            eligible,
            sync_file,
            |_, _| Ok(()),
            |_, _| Ok(()),
            sync_directory,
        )
    }

    #[cfg(test)]
    fn mutate_with_publish_hook<P>(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
        before_publish: P,
    ) -> Result<ReviewMutationResult, ReviewStateError>
    where
        P: FnOnce(&SecureStateDirectory, &CStr) -> io::Result<()>,
    {
        self.mutate_with_storage_hooks(
            request,
            eligible,
            File::sync_all,
            |_, _| Ok(()),
            before_publish,
            SecureStateDirectory::sync,
        )
    }

    #[cfg(test)]
    fn mutate_with_lock_hook<L>(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
        after_state_read: L,
    ) -> Result<ReviewMutationResult, ReviewStateError>
    where
        L: FnOnce(&SecureStateDirectory, &File) -> io::Result<()>,
    {
        self.mutate_with_storage_hooks(
            request,
            eligible,
            File::sync_all,
            after_state_read,
            |_, _| Ok(()),
            SecureStateDirectory::sync,
        )
    }

    fn mutate_with_storage_hooks<F, L, P, D>(
        &self,
        request: &ReviewMutationRequest,
        eligible: &BTreeSet<ReviewKey>,
        sync_file: F,
        after_state_read: L,
        before_publish: P,
        sync_directory: D,
    ) -> Result<ReviewMutationResult, ReviewStateError>
    where
        F: FnOnce(&File) -> io::Result<()>,
        L: FnOnce(&SecureStateDirectory, &File) -> io::Result<()>,
        P: FnOnce(&SecureStateDirectory, &CStr) -> io::Result<()>,
        D: FnOnce(&SecureStateDirectory) -> io::Result<()>,
    {
        request
            .validate()
            .map_err(ReviewStateError::InvalidRequest)?;
        let directory = SecureStateDirectory::open_or_create(&self.state_root)?;
        let lock = open_exact_regular(&directory, LOCK_NAME, true)?
            .expect("creating the review lock always returns a file");
        let _guard = lock_exclusive(&directory, &lock)?;
        let mut state = read_persisted(&directory)?;
        directory.validate_regular(LOCK_NAME, &lock, 0o600)?;
        after_state_read(&directory, &lock)?;

        let (revision, reviewed_count, archived_count, last_archive_count) = {
            let surface = state.surfaces.entry(request.surface).or_default();
            let result = mutate_surface(
                request,
                eligible,
                &mut surface.revision,
                &mut surface.items,
                &mut surface.last_archive,
            )?;
            (
                result.surface_revision,
                result.reviewed_count,
                result.archived_count,
                result.last_archive_count,
            )
        };

        validate_persisted(&state)?;
        let encoded = serde_json::to_vec(&state)?;
        if encoded.len() > MAX_REVIEW_STATE_BYTES {
            return Err(ReviewStateError::StateTooLarge);
        }
        persist(
            &directory,
            &lock,
            &encoded,
            sync_file,
            before_publish,
            sync_directory,
        )?;
        Ok(ReviewMutationResult {
            surface: request.surface,
            surface_revision: revision,
            reviewed_count,
            archived_count,
            last_archive_count,
        })
    }
}

fn all_surfaces() -> impl Iterator<Item = ReviewSurface> {
    [
        ReviewSurface::Attention,
        ReviewSurface::Review,
        ReviewSurface::Diagnostics,
        ReviewSurface::Recent,
    ]
    .into_iter()
}

fn disposition_count(
    items: &BTreeMap<ReviewKey, ReviewDisposition>,
    disposition: ReviewDisposition,
) -> usize {
    items
        .values()
        .filter(|candidate| **candidate == disposition)
        .count()
}

fn prune_surface(
    items: &mut BTreeMap<ReviewKey, ReviewDisposition>,
    last_archive: &mut BTreeSet<ReviewKey>,
    eligible: &BTreeSet<ReviewKey>,
) {
    items.retain(|key, _| eligible.contains(key));
    last_archive.retain(|key| {
        eligible.contains(key) && items.get(key).copied() == Some(ReviewDisposition::Archived)
    });
}

pub(crate) fn mutate_surface(
    request: &ReviewMutationRequest,
    eligible: &BTreeSet<ReviewKey>,
    revision: &mut u64,
    items: &mut BTreeMap<ReviewKey, ReviewDisposition>,
    last_archive: &mut BTreeSet<ReviewKey>,
) -> Result<ReviewMutationResult, ReviewStateError> {
    request
        .validate()
        .map_err(ReviewStateError::InvalidRequest)?;
    prune_surface(items, last_archive, eligible);
    if *revision != request.expected_surface_revision {
        return Err(ReviewStateError::StaleRevision);
    }
    match &request.operation {
        ReviewMutation::SetDisposition { keys, disposition } => {
            if !keys.iter().all(|key| eligible.contains(key)) {
                return Err(ReviewStateError::TargetNotEligible);
            }
            match disposition {
                ReviewDisposition::Reviewed => {
                    if keys.iter().any(|key| items.contains_key(key)) {
                        return Err(ReviewStateError::DispositionConflict);
                    }
                    if items.len().saturating_add(keys.len()) > MAX_REVIEW_KEYS {
                        return Err(ReviewStateError::CapacityExceeded);
                    }
                    items.extend(keys.iter().map(|key| (*key, ReviewDisposition::Reviewed)));
                }
                ReviewDisposition::Archived => {
                    if !request.surface.supports_archive() {
                        return Err(ReviewStateError::InvalidRequest(
                            ReviewRequestError::UnsupportedOperation,
                        ));
                    }
                    if keys
                        .iter()
                        .any(|key| items.get(key).copied() != Some(ReviewDisposition::Reviewed))
                    {
                        return Err(ReviewStateError::DispositionConflict);
                    }
                    for key in keys {
                        items.insert(*key, ReviewDisposition::Archived);
                    }
                    last_archive.clone_from(keys);
                }
            }
        }
        ReviewMutation::ArchiveAllReviewed { expected_count } => {
            let reviewed = items
                .iter()
                .filter_map(|(key, disposition)| {
                    (*disposition == ReviewDisposition::Reviewed).then_some(*key)
                })
                .collect::<BTreeSet<_>>();
            if reviewed.len() != *expected_count {
                return Err(ReviewStateError::CountMismatch);
            }
            for key in &reviewed {
                items.insert(*key, ReviewDisposition::Archived);
            }
            *last_archive = reviewed;
        }
        ReviewMutation::UndoLastArchive { expected_count } => {
            if last_archive.len() != *expected_count {
                return Err(ReviewStateError::CountMismatch);
            }
            if last_archive
                .iter()
                .any(|key| items.get(key).copied() != Some(ReviewDisposition::Archived))
            {
                return Err(ReviewStateError::DispositionConflict);
            }
            for key in last_archive.iter() {
                items.insert(*key, ReviewDisposition::Reviewed);
            }
            last_archive.clear();
        }
    }
    if items.len() > MAX_REVIEW_KEYS {
        return Err(ReviewStateError::CapacityExceeded);
    }
    *revision = revision
        .checked_add(1)
        .ok_or(ReviewStateError::RevisionOverflow)?;
    Ok(ReviewMutationResult {
        surface: request.surface,
        surface_revision: *revision,
        reviewed_count: disposition_count(items, ReviewDisposition::Reviewed),
        archived_count: disposition_count(items, ReviewDisposition::Archived),
        last_archive_count: last_archive.len(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedState {
    schema_version: u32,
    surfaces: BTreeMap<ReviewSurface, PersistedSurface>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema_version: REVIEW_STATE_SCHEMA_VERSION,
            surfaces: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSurface {
    revision: u64,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    last_archive: BTreeSet<ReviewKey>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    items: BTreeMap<ReviewKey, ReviewDisposition>,
}

#[cfg(test)]
fn read_snapshot(
    directory: &SecureStateDirectory,
) -> Result<ReviewStateSnapshot, ReviewStateError> {
    let state = read_persisted(directory)?;
    let mut snapshot = ReviewStateSnapshot::default();
    for (surface, persisted) in state.surfaces {
        snapshot.surfaces.insert(
            surface,
            SurfaceState {
                revision: persisted.revision,
                items: persisted.items,
                last_archive: persisted.last_archive,
            },
        );
    }
    Ok(snapshot)
}

#[cfg(test)]
fn read_persisted(directory: &SecureStateDirectory) -> Result<PersistedState, ReviewStateError> {
    let Some(mut file) = open_exact_regular(directory, STATE_NAME, false)? else {
        return Ok(PersistedState::default());
    };
    let length = file.metadata()?.len();
    if length > MAX_REVIEW_STATE_BYTES as u64 {
        return Err(ReviewStateError::StateTooLarge);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take((MAX_REVIEW_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REVIEW_STATE_BYTES {
        return Err(ReviewStateError::StateTooLarge);
    }
    let UniqueJsonValue(value) = serde_json::from_slice::<UniqueJsonValue>(&bytes)?;
    let state = serde_json::from_value::<PersistedState>(value)?;
    validate_persisted(&state)?;
    Ok(state)
}

pub(crate) fn decode_legacy_snapshot(
    bytes: &[u8],
) -> Result<ReviewStateSnapshot, ReviewStateError> {
    if bytes.len() > MAX_REVIEW_STATE_BYTES {
        return Err(ReviewStateError::StateTooLarge);
    }
    let UniqueJsonValue(value) = serde_json::from_slice::<UniqueJsonValue>(bytes)?;
    let state = serde_json::from_value::<PersistedState>(value)?;
    validate_persisted(&state)?;
    let mut snapshot = ReviewStateSnapshot::default();
    for (surface, persisted) in state.surfaces {
        snapshot.surfaces.insert(
            surface,
            SurfaceState {
                revision: persisted.revision,
                items: persisted.items,
                last_archive: persisted.last_archive,
            },
        );
    }
    Ok(snapshot)
}

fn validate_persisted(state: &PersistedState) -> Result<(), ReviewStateError> {
    if state.schema_version != REVIEW_STATE_SCHEMA_VERSION {
        return Err(ReviewStateError::UnsupportedSchema(state.schema_version));
    }
    for (surface, state) in &state.surfaces {
        if state.items.len() > MAX_REVIEW_KEYS || state.last_archive.len() > MAX_REVIEW_KEYS {
            return Err(ReviewStateError::CapacityExceeded);
        }
        if *surface == ReviewSurface::Recent
            && (!state.last_archive.is_empty()
                || state
                    .items
                    .values()
                    .any(|value| *value == ReviewDisposition::Archived))
        {
            return Err(ReviewStateError::InvalidStorage(
                "Recent contains archive state",
            ));
        }
        if state
            .last_archive
            .iter()
            .any(|key| state.items.get(key).copied() != Some(ReviewDisposition::Archived))
        {
            return Err(ReviewStateError::InvalidStorage(
                "last archive does not name archived items",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn open_exact_regular(
    directory: &SecureStateDirectory,
    name: &CStr,
    create: bool,
) -> Result<Option<File>, ReviewStateError> {
    open_exact_regular_with_missing_hook(directory, name, create, || {})
}

#[cfg(test)]
fn open_exact_regular_with_missing_hook(
    directory: &SecureStateDirectory,
    name: &CStr,
    create: bool,
    after_missing: impl FnOnce(),
) -> Result<Option<File>, ReviewStateError> {
    match directory.metadata(name) {
        Ok(metadata) => {
            validate_exact_file_mode(&metadata)?;
            Ok(Some(directory.open_regular_strict(name, false)?))
        }
        Err(SecureStateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            after_missing();
            if create {
                Ok(Some(directory.open_regular_strict(name, true)?))
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
fn validate_exact_file_mode(metadata: &SecureEntryMetadata) -> Result<(), ReviewStateError> {
    if metadata.mode & 0o777 != 0o600 {
        return Err(ReviewStateError::InvalidStorage(
            "review state file mode is not 0600",
        ));
    }
    Ok(())
}

#[cfg(test)]
struct LockGuard<'a>(&'a File);

#[cfg(test)]
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.0);
    }
}

#[cfg(test)]
fn lock_exclusive<'a>(
    directory: &SecureStateDirectory,
    file: &'a File,
) -> Result<LockGuard<'a>, ReviewStateError> {
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => {
                let path = directory.metadata(LOCK_NAME)?;
                let opened = file.metadata()?;
                if path.dev != opened.dev() || path.ino != opened.ino() {
                    FileExt::unlock(file)?;
                    return Err(ReviewStateError::InvalidStorage(
                        "review lock replaced while acquiring",
                    ));
                }
                return Ok(LockGuard(file));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(ReviewStateError::Busy);
                }
                thread::sleep(LOCK_RETRY.min(deadline.saturating_duration_since(Instant::now())));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
fn create_temporary(directory: &SecureStateDirectory) -> Result<(CString, File), ReviewStateError> {
    let mut randomness = File::open("/dev/urandom")?;
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut random = [0_u8; 16];
        randomness.read_exact(&mut random)?;
        let mut random_hex = String::with_capacity(random.len() * 2);
        for byte in random {
            use std::fmt::Write as _;
            write!(&mut random_hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let name = CString::new(format!(
            ".review-state.tmp-{}-{sequence:020}-{random_hex}",
            std::process::id()
        ))
        .expect("fixed temporary name contains no NUL");
        match directory.create_regular_exclusive(&name) {
            Ok(file) => return Ok((name, file)),
            Err(SecureStateError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(ReviewStateError::InvalidStorage(
        "could not allocate review state temporary",
    ))
}

#[cfg(test)]
fn persist<F, P, D>(
    directory: &SecureStateDirectory,
    lock: &File,
    encoded: &[u8],
    sync_file: F,
    before_publish: P,
    sync_directory: D,
) -> Result<(), ReviewStateError>
where
    F: FnOnce(&File) -> io::Result<()>,
    P: FnOnce(&SecureStateDirectory, &CStr) -> io::Result<()>,
    D: FnOnce(&SecureStateDirectory) -> io::Result<()>,
{
    let (temporary_name, mut temporary) = create_temporary(directory)?;
    let before_rename = (|| -> io::Result<()> {
        temporary.write_all(encoded)?;
        temporary.flush()?;
        sync_file(&temporary)
    })();
    if let Err(error) = before_rename {
        let _ = directory.unlink(&temporary_name);
        return Err(error.into());
    }
    if let Err(error) = before_publish(directory, &temporary_name) {
        let _ = directory.unlink(&temporary_name);
        return Err(error.into());
    }
    if let Err(error) = directory.publish_regular(&temporary_name, &temporary, STATE_NAME, || {
        directory.validate_regular(LOCK_NAME, lock, 0o600)
    }) {
        let _ = directory.unlink(&temporary_name);
        return Err(error.into());
    }
    sync_directory(directory).map_err(|_| ReviewStateError::DurabilityUncertain)
}

struct UniqueJsonValue(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonValueVisitor)
    }
}

struct UniqueJsonValueVisitor;

impl<'de> Visitor<'de> for UniqueJsonValueVisitor {
    type Value = UniqueJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .map(UniqueJsonValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(serde_json::Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueJsonValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJsonValue(serde_json::Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let UniqueJsonValue(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJsonValue(serde_json::Value::Object(values)))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    use coding_brain_core::review_state::{
        MAX_REVIEW_KEYS, ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest,
        ReviewSurface,
    };
    use fs2::FileExt;

    use super::*;

    fn key(surface: ReviewSurface, index: usize) -> ReviewKey {
        ReviewKey::derive(surface, &index.to_be_bytes())
    }

    fn keys(surface: ReviewSurface, count: usize) -> BTreeSet<ReviewKey> {
        (0..count).map(|index| key(surface, index)).collect()
    }

    fn set_request(
        surface: ReviewSurface,
        revision: u64,
        keys: BTreeSet<ReviewKey>,
        disposition: ReviewDisposition,
    ) -> ReviewMutationRequest {
        ReviewMutationRequest {
            surface,
            expected_surface_revision: revision,
            operation: ReviewMutation::SetDisposition { keys, disposition },
        }
    }

    fn write_private(path: &std::path::Path, contents: impl AsRef<[u8]>) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn private_tempdir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    fn assert_temporary_substitution_is_rejected(
        substitute: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let mut eligible = keys(ReviewSurface::Recent, 2);
        let first = key(ReviewSurface::Recent, 0);
        store
            .mutate(
                &set_request(
                    ReviewSurface::Recent,
                    0,
                    [first].into_iter().collect(),
                    ReviewDisposition::Reviewed,
                ),
                &eligible,
            )
            .unwrap();
        let state_path = temp.path().join("review-state.json");
        let previous = fs::read(&state_path).unwrap();
        let attacker = temp.path().join("attacker-selected");
        write_private(&attacker, b"attacker-selected");
        let second = key(ReviewSurface::Recent, 1);
        eligible.insert(second);

        let result = store.mutate_with_publish_hook(
            &set_request(
                ReviewSurface::Recent,
                1,
                [second].into_iter().collect(),
                ReviewDisposition::Reviewed,
            ),
            &eligible,
            |_, name| {
                let temporary = temp.path().join(OsStr::from_bytes(name.to_bytes()));
                substitute(&temporary, &attacker)
            },
        );

        assert!(matches!(result, Err(ReviewStateError::InvalidStorage(_))));
        assert_eq!(fs::read(&state_path).unwrap(), previous);
        assert_eq!(
            store
                .read()
                .unwrap()
                .surface_revision(ReviewSurface::Recent),
            1
        );
    }

    #[test]
    fn missing_state_reads_as_revision_zero_empty_surfaces() {
        let temp = private_tempdir();
        let snapshot = ReviewStateStore::at(temp.path()).read().unwrap();

        for surface in [
            ReviewSurface::Attention,
            ReviewSurface::Review,
            ReviewSurface::Diagnostics,
            ReviewSurface::Recent,
        ] {
            assert_eq!(snapshot.surface_revision(surface), 0);
            assert_eq!(snapshot.reviewed_count(surface), 0);
            assert_eq!(snapshot.archived_count(surface), 0);
            assert!(snapshot.last_archive(surface).is_empty());
        }
    }

    #[test]
    fn mutations_are_revisioned_per_surface_and_reject_stale_writers() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let attention = keys(ReviewSurface::Attention, 2);
        let recent = keys(ReviewSurface::Recent, 1);

        let first = store
            .mutate(
                &set_request(
                    ReviewSurface::Attention,
                    0,
                    attention.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &attention,
            )
            .unwrap();
        assert_eq!(first.surface_revision, 1);
        assert_eq!(first.reviewed_count, 2);

        let other = store
            .mutate(
                &set_request(
                    ReviewSurface::Recent,
                    0,
                    recent.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &recent,
            )
            .unwrap();
        assert_eq!(other.surface_revision, 1);
        assert!(matches!(
            store.mutate(
                &set_request(
                    ReviewSurface::Attention,
                    0,
                    [key(ReviewSurface::Attention, 0)].into_iter().collect(),
                    ReviewDisposition::Reviewed,
                ),
                &attention,
            ),
            Err(ReviewStateError::StaleRevision)
        ));
        let snapshot = store.read().unwrap();
        assert_eq!(snapshot.surface_revision(ReviewSurface::Attention), 1);
        assert_eq!(snapshot.surface_revision(ReviewSurface::Recent), 1);
    }

    #[test]
    fn archive_all_reaches_beyond_displayed_subsets_and_undo_survives_restart() {
        let temp = private_tempdir();
        let eligible = keys(ReviewSurface::Review, 128);
        let store = ReviewStateStore::at(temp.path());
        store
            .mutate(
                &set_request(
                    ReviewSurface::Review,
                    0,
                    eligible.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &eligible,
            )
            .unwrap();
        let archive = ReviewMutationRequest {
            surface: ReviewSurface::Review,
            expected_surface_revision: 1,
            operation: ReviewMutation::ArchiveAllReviewed {
                expected_count: eligible.len(),
            },
        };
        let result = store.mutate(&archive, &eligible).unwrap();
        assert_eq!(result.surface_revision, 2);
        assert_eq!(result.archived_count, eligible.len());
        assert_eq!(result.last_archive_count, eligible.len());

        let reopened = ReviewStateStore::at(temp.path());
        assert_eq!(
            reopened.read().unwrap().last_archive(ReviewSurface::Review),
            &eligible
        );
        let undo = ReviewMutationRequest {
            surface: ReviewSurface::Review,
            expected_surface_revision: 2,
            operation: ReviewMutation::UndoLastArchive {
                expected_count: eligible.len(),
            },
        };
        let result = reopened.mutate(&undo, &eligible).unwrap();
        assert_eq!(result.reviewed_count, eligible.len());
        assert_eq!(result.last_archive_count, 0);
    }

    #[test]
    fn a_later_archive_replaces_the_single_undo_slot() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let eligible = keys(ReviewSurface::Diagnostics, 2);
        store
            .mutate(
                &set_request(
                    ReviewSurface::Diagnostics,
                    0,
                    eligible.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &eligible,
            )
            .unwrap();
        let archived = eligible.iter().copied().collect::<Vec<_>>();
        for (revision, key) in archived.iter().enumerate() {
            store
                .mutate(
                    &set_request(
                        ReviewSurface::Diagnostics,
                        revision as u64 + 1,
                        [*key].into_iter().collect(),
                        ReviewDisposition::Archived,
                    ),
                    &eligible,
                )
                .unwrap();
        }

        let snapshot = store.read().unwrap();
        assert_eq!(
            snapshot.last_archive(ReviewSurface::Diagnostics),
            &[archived[1]].into_iter().collect()
        );
        store
            .mutate(
                &ReviewMutationRequest {
                    surface: ReviewSurface::Diagnostics,
                    expected_surface_revision: 3,
                    operation: ReviewMutation::UndoLastArchive { expected_count: 1 },
                },
                &eligible,
            )
            .unwrap();
        let snapshot = store.read().unwrap();
        assert_eq!(
            snapshot.disposition(ReviewSurface::Diagnostics, &archived[0]),
            Some(ReviewDisposition::Archived)
        );
        assert_eq!(
            snapshot.disposition(ReviewSurface::Diagnostics, &archived[1]),
            Some(ReviewDisposition::Reviewed)
        );
    }

    #[test]
    fn ineligible_targets_and_count_mismatches_leave_state_unchanged() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let eligible = keys(ReviewSurface::Diagnostics, 2);
        let before = store.read().unwrap();
        let request = set_request(
            ReviewSurface::Diagnostics,
            0,
            [key(ReviewSurface::Diagnostics, 9)].into_iter().collect(),
            ReviewDisposition::Reviewed,
        );
        assert!(matches!(
            store.mutate(&request, &eligible),
            Err(ReviewStateError::TargetNotEligible)
        ));
        assert_eq!(store.read().unwrap(), before);
    }

    #[test]
    fn capacity_failure_keeps_the_still_eligible_extra_key_new() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let retained = keys(ReviewSurface::Attention, MAX_REVIEW_KEYS);
        store
            .mutate(
                &set_request(
                    ReviewSurface::Attention,
                    0,
                    retained.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &retained,
            )
            .unwrap();
        let mut eligible = retained.clone();
        let extra = key(ReviewSurface::Attention, MAX_REVIEW_KEYS);
        eligible.insert(extra);
        let request = set_request(
            ReviewSurface::Attention,
            1,
            [extra].into_iter().collect(),
            ReviewDisposition::Reviewed,
        );

        assert!(matches!(
            store.mutate(&request, &eligible),
            Err(ReviewStateError::CapacityExceeded)
        ));
        let snapshot = store.read().unwrap();
        assert_eq!(snapshot.surface_revision(ReviewSurface::Attention), 1);
        assert_eq!(
            snapshot.reviewed_count(ReviewSurface::Attention),
            MAX_REVIEW_KEYS
        );
        assert_eq!(snapshot.disposition(ReviewSurface::Attention, &extra), None);

        let archive = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 1,
            operation: ReviewMutation::ArchiveAllReviewed {
                expected_count: MAX_REVIEW_KEYS,
            },
        };
        let archived = store.mutate(&archive, &eligible).unwrap();
        assert_eq!(archived.archived_count, MAX_REVIEW_KEYS);
        let undo = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 2,
            operation: ReviewMutation::UndoLastArchive {
                expected_count: MAX_REVIEW_KEYS,
            },
        };
        let restored = store.mutate(&undo, &eligible).unwrap();
        assert_eq!(restored.reviewed_count, MAX_REVIEW_KEYS);
    }

    #[test]
    fn malformed_duplicate_oversized_and_unsupported_state_fail_closed() {
        for contents in [
            br#"{"schema_version":1,"schema_version":1,"surfaces":{}}"#.as_slice(),
            br#"{"schema_version":2,"surfaces":{}}"#.as_slice(),
            br#"{"schema_version":1,"surfaces":{"recent":{"revision":1,"items":{"0000000000000000000000000000000000000000000000000000000000000000":"archived"}}}}"#.as_slice(),
        ] {
            let temp = private_tempdir();
            write_private(&temp.path().join("review-state.json"), contents);
            assert!(ReviewStateStore::at(temp.path()).read().is_err());
        }

        let temp = private_tempdir();
        write_private(
            &temp.path().join("review-state.json"),
            vec![b' '; coding_brain_core::review_state::MAX_REVIEW_STATE_BYTES + 1],
        );
        assert!(matches!(
            ReviewStateStore::at(temp.path()).read(),
            Err(ReviewStateError::StateTooLarge)
        ));
    }

    #[test]
    fn state_and_lock_symlinks_and_hard_links_are_rejected() {
        for name in ["review-state.json", "review-state.lock"] {
            let temp = private_tempdir();
            let victim = temp.path().join("victim");
            write_private(&victim, b"victim");
            symlink(&victim, temp.path().join(name)).unwrap();
            assert!(matches!(
                ReviewStateStore::at(temp.path()).read(),
                Err(ReviewStateError::InvalidStorage(_))
            ));
            assert_eq!(fs::read(&victim).unwrap(), b"victim");
        }

        for name in ["review-state.json", "review-state.lock"] {
            let temp = private_tempdir();
            let store = ReviewStateStore::at(temp.path());
            store.read().unwrap();
            if name == "review-state.json" {
                let eligible = keys(ReviewSurface::Recent, 1);
                store
                    .mutate(
                        &set_request(
                            ReviewSurface::Recent,
                            0,
                            eligible.clone(),
                            ReviewDisposition::Reviewed,
                        ),
                        &eligible,
                    )
                    .unwrap();
            }
            fs::hard_link(temp.path().join(name), temp.path().join("alias")).unwrap();
            assert!(matches!(
                store.read(),
                Err(ReviewStateError::InvalidStorage(_))
            ));
        }
    }

    #[test]
    fn strict_review_storage_rejects_existing_file_modes_without_repair() {
        for name in ["review-state.json", "review-state.lock"] {
            let temp = private_tempdir();
            let store = ReviewStateStore::at(temp.path());
            let eligible = keys(ReviewSurface::Recent, 1);
            store
                .mutate(
                    &set_request(
                        ReviewSurface::Recent,
                        0,
                        eligible.clone(),
                        ReviewDisposition::Reviewed,
                    ),
                    &eligible,
                )
                .unwrap();
            let path = temp.path().join(name);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

            assert!(matches!(
                store.read(),
                Err(ReviewStateError::InvalidStorage(_))
            ));
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn strict_review_storage_rejects_existing_final_directory_mode_without_repair() {
        let temp = private_tempdir();
        let state_root = temp.path().join("review-owned");
        fs::create_dir(&state_root).unwrap();
        fs::set_permissions(&state_root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(
            ReviewStateStore::at(&state_root).read(),
            Err(ReviewStateError::InvalidStorage(_))
        ));
        assert_eq!(
            fs::metadata(state_root).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn strict_review_storage_allows_ordinary_ancestor_modes() {
        let temp = private_tempdir();
        let ancestor = temp.path().join("ordinary-ancestor");
        fs::create_dir(&ancestor).unwrap();
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o755)).unwrap();
        let state_root = ancestor.join("review-owned");

        ReviewStateStore::at(&state_root).read().unwrap();

        assert_eq!(
            fs::metadata(ancestor).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(state_root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn strict_review_storage_rejects_missing_to_existing_file_race_without_repair() {
        let temp = private_tempdir();
        let directory = SecureStateDirectory::open_or_create(temp.path()).unwrap();
        let lock_path = temp.path().join("review-state.lock");

        let result = open_exact_regular_with_missing_hook(&directory, LOCK_NAME, true, || {
            fs::write(&lock_path, []).unwrap();
            fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        });

        assert!(matches!(result, Err(ReviewStateError::InvalidStorage(_))));
        assert_eq!(
            fs::metadata(lock_path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn concurrent_same_surface_writers_cannot_both_commit_one_revision() {
        let temp = private_tempdir();
        let store = Arc::new(ReviewStateStore::at(temp.path()));
        store.read().unwrap();
        let eligible = Arc::new(keys(ReviewSurface::Attention, 2));
        let barrier = Arc::new(Barrier::new(3));
        let threads = (0..2)
            .map(|index| {
                let store = Arc::clone(&store);
                let eligible = Arc::clone(&eligible);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let request = set_request(
                        ReviewSurface::Attention,
                        0,
                        [key(ReviewSurface::Attention, index)].into_iter().collect(),
                        ReviewDisposition::Reviewed,
                    );
                    barrier.wait();
                    store.mutate(&request, &eligible)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(ReviewStateError::StaleRevision)))
                .count(),
            1
        );
    }

    #[test]
    fn lock_contention_is_busy_before_mutation() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        store.read().unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path().join("review-state.lock"))
            .unwrap();
        lock.try_lock_exclusive().unwrap();
        let eligible = keys(ReviewSurface::Recent, 1);
        assert!(matches!(
            store.mutate(
                &set_request(
                    ReviewSurface::Recent,
                    0,
                    eligible.clone(),
                    ReviewDisposition::Reviewed,
                ),
                &eligible,
            ),
            Err(ReviewStateError::Busy)
        ));
        FileExt::unlock(&lock).unwrap();
        assert_eq!(
            store
                .read()
                .unwrap()
                .surface_revision(ReviewSurface::Recent),
            0
        );
    }

    #[test]
    fn lock_path_replacement_after_acquisition_is_rejected_before_publication() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let eligible = keys(ReviewSurface::Recent, 2);
        store
            .mutate(
                &set_request(
                    ReviewSurface::Recent,
                    0,
                    [key(ReviewSurface::Recent, 0)].into_iter().collect(),
                    ReviewDisposition::Reviewed,
                ),
                &eligible,
            )
            .unwrap();
        let state_path = temp.path().join("review-state.json");
        let previous = fs::read(&state_path).unwrap();
        let lock_path = temp.path().join("review-state.lock");

        let result = store.mutate_with_lock_hook(
            &set_request(
                ReviewSurface::Recent,
                1,
                [key(ReviewSurface::Recent, 1)].into_iter().collect(),
                ReviewDisposition::Reviewed,
            ),
            &eligible,
            |_, held_lock| {
                fs::remove_file(&lock_path)?;
                fs::write(&lock_path, [])?;
                fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;
                let replacement = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&lock_path)?;
                let held_metadata = held_lock.metadata()?;
                let replacement_metadata = replacement.metadata()?;
                assert_ne!(
                    (held_metadata.dev(), held_metadata.ino()),
                    (replacement_metadata.dev(), replacement_metadata.ino())
                );
                replacement.try_lock_exclusive()?;
                FileExt::unlock(&replacement)
            },
        );

        assert!(matches!(result, Err(ReviewStateError::InvalidStorage(_))));
        assert_eq!(fs::read(&state_path).unwrap(), previous);
        assert_eq!(
            store
                .read()
                .unwrap()
                .surface_revision(ReviewSurface::Recent),
            1
        );
    }

    #[test]
    fn post_rename_sync_failure_is_uncertain_and_requires_fresh_read() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let eligible = keys(ReviewSurface::Recent, 1);
        let request = set_request(
            ReviewSurface::Recent,
            0,
            eligible.clone(),
            ReviewDisposition::Reviewed,
        );
        let error = store
            .mutate_with_directory_sync(&request, &eligible, |_| {
                Err(std::io::Error::other("injected directory sync failure"))
            })
            .unwrap_err();
        assert!(matches!(error, ReviewStateError::DurabilityUncertain));
        assert_eq!(
            store
                .read()
                .unwrap()
                .surface_revision(ReviewSurface::Recent),
            1
        );
        assert!(matches!(
            store.mutate(&request, &eligible),
            Err(ReviewStateError::StaleRevision)
        ));
    }

    #[test]
    fn pre_rename_sync_failure_preserves_the_old_revision() {
        let temp = private_tempdir();
        let store = ReviewStateStore::at(temp.path());
        let eligible = keys(ReviewSurface::Recent, 1);
        let request = set_request(
            ReviewSurface::Recent,
            0,
            eligible.clone(),
            ReviewDisposition::Reviewed,
        );
        let error = store
            .mutate_with_syncs(
                &request,
                &eligible,
                |_| Err(std::io::Error::other("injected file sync failure")),
                SecureStateDirectory::sync,
            )
            .unwrap_err();
        assert!(matches!(error, ReviewStateError::Io(_)));
        assert_eq!(
            store
                .read()
                .unwrap()
                .surface_revision(ReviewSurface::Recent),
            0
        );
        assert!(!temp.path().join("review-state.json").exists());
    }

    #[test]
    fn regular_temporary_substitution_before_publication_is_rejected() {
        assert_temporary_substitution_is_rejected(|temporary, attacker| {
            fs::remove_file(temporary)?;
            fs::copy(attacker, temporary)?;
            fs::set_permissions(temporary, fs::Permissions::from_mode(0o600))
        });
    }

    #[test]
    fn symlink_temporary_substitution_before_publication_is_rejected() {
        assert_temporary_substitution_is_rejected(|temporary, attacker| {
            fs::remove_file(temporary)?;
            symlink(attacker, temporary)
        });
    }

    #[test]
    fn hard_link_temporary_substitution_before_publication_is_rejected() {
        assert_temporary_substitution_is_rejected(|temporary, attacker| {
            fs::remove_file(temporary)?;
            fs::hard_link(attacker, temporary)
        });
    }
}
