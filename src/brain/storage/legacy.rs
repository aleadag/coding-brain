use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, MAX_ACTIVITY_EVENT_BYTES, MIN_ACTIVITY_SCHEMA_VERSION,
};
use coding_brain_core::lifecycle::LifecycleSnapshot;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::brain::decisions::{
    DecisionRecord, HookDecisionRecord, MAX_DECISION_RECORD_BYTES, parse_decision_value,
    validate_hook_decision_record,
};
use crate::brain::review_state::{ReviewStateSnapshot, decode_legacy_snapshot as decode_review};
use crate::brain::secure_state::{SecureStateDirectory, SecureStateError};

use super::{StorageDeadline, StorageError};

mod permission_journal;

pub(super) use permission_journal::{
    PermissionTransactionJournal, decode_exact_journal, decode_exact_json,
    hook_decision_numbers_are_lossless, validate_journal,
};

pub const LEGACY_EXPORT_PROFILE: &str = "legacy-v0.59.1";
pub(super) const LEGACY_LEARNING_FILES: [&CStr; 3] =
    [c"decisions.jsonl", c"canonical.jsonl", c"preferences.json"];

const LEGACY_SOURCES: [LegacySourceDescriptor; 5] = [
    LegacySourceDescriptor::new(LegacySourceKind::Decisions, "brain/decisions.jsonl"),
    LegacySourceDescriptor::new(LegacySourceKind::Activity, "activity.jsonl"),
    LegacySourceDescriptor::new(LegacySourceKind::Lifecycle, "hooks/lifecycle.json"),
    LegacySourceDescriptor::new(
        LegacySourceKind::PermissionTransactions,
        "brain/permission-transactions",
    ),
    LegacySourceDescriptor::new(LegacySourceKind::ReviewState, "review-state.json"),
];

const LEGACY_WRITER_LOCK_ORDER: [&str; 5] = [
    "brain/permission-transactions/",
    "brain/decisions.lock",
    "activity.lock",
    "hooks/lifecycle.lock",
    "review-state.lock",
];
const LEGACY_LOCK_RETRY: Duration = Duration::from_millis(5);
const MAX_JOURNAL_GUARD_ENTRIES: usize = 4_096;
const MAX_JOURNAL_GUARD_NAME_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacySourceKind {
    Decisions,
    Activity,
    Lifecycle,
    PermissionTransactions,
    ReviewState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacySourceDescriptor {
    kind: LegacySourceKind,
    relative_path: &'static str,
}

impl LegacySourceDescriptor {
    const fn new(kind: LegacySourceKind, relative_path: &'static str) -> Self {
        Self {
            kind,
            relative_path,
        }
    }

    pub fn kind(self) -> LegacySourceKind {
        self.kind
    }

    pub fn relative_path(self) -> &'static str {
        self.relative_path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyFingerprint {
    pub kind: LegacySourceKind,
    relative_path: PathBuf,
    pub present: bool,
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified_seconds: i64,
    pub modified_nanoseconds: i64,
}

impl LegacyFingerprint {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_persisted_parts(
        kind: LegacySourceKind,
        relative_path: PathBuf,
        present: bool,
        device: u64,
        inode: u64,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
    ) -> Self {
        Self {
            kind,
            relative_path,
            present,
            device,
            inode,
            size,
            modified_seconds,
            modified_nanoseconds,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyFreezeIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
    sha256: [u8; 32],
}

impl LegacyFreezeIdentity {
    pub fn device(&self) -> u64 {
        self.device
    }

    pub fn inode(&self) -> u64 {
        self.inode
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified_seconds(&self) -> i64 {
        self.modified_seconds
    }

    pub fn modified_nanoseconds(&self) -> i64 {
        self.modified_nanoseconds
    }

    pub fn mode(&self) -> u32 {
        self.mode & 0o777
    }

    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyFreezeArtifact {
    relative_path: PathBuf,
    temporary_name: String,
    source: LegacyFreezeIdentity,
    target: LegacyFreezeIdentity,
}

impl LegacyFreezeArtifact {
    pub fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub fn temporary_name(&self) -> &str {
        &self.temporary_name
    }

    pub fn source(&self) -> &LegacyFreezeIdentity {
        &self.source
    }

    pub fn target(&self) -> &LegacyFreezeIdentity {
        &self.target
    }
}

#[derive(Debug)]
pub struct LegacySnapshot {
    source_count: usize,
    decision_count: u64,
    activity_count: u64,
    lifecycle_count: u64,
    journal_count: u64,
    review_state_count: u64,
    max_record_buffer_bytes: usize,
}

impl LegacySnapshot {
    pub fn profile(&self) -> &'static str {
        LEGACY_EXPORT_PROFILE
    }

    pub fn source_count(&self) -> usize {
        self.source_count
    }

    pub fn decision_count(&self) -> u64 {
        self.decision_count
    }

    pub fn activity_count(&self) -> u64 {
        self.activity_count
    }

    pub fn lifecycle_count(&self) -> u64 {
        self.lifecycle_count
    }

    pub fn journal_count(&self) -> u64 {
        self.journal_count
    }

    pub fn review_state_count(&self) -> u64 {
        self.review_state_count
    }

    pub(crate) fn max_record_buffer_bytes(&self) -> usize {
        self.max_record_buffer_bytes
    }
}

pub(crate) enum LegacyDecision {
    Hook(HookDecisionRecord),
    Audit(DecisionRecord),
}

pub(crate) trait LegacyImportSink {
    fn decision(&mut self, decision: LegacyDecision) -> Result<(), StorageError>;
    fn activity(&mut self, activity: ActivityEvent) -> Result<(), StorageError>;
    fn lifecycle(&mut self, lifecycle: LifecycleSnapshot) -> Result<(), StorageError>;
    fn journal(&mut self, journal: PermissionTransactionJournal) -> Result<(), StorageError>;
    fn review(&mut self, review: ReviewStateSnapshot) -> Result<(), StorageError>;
}

pub struct LegacySourceSet {
    state_root: LegacySourceRoot,
}

enum LegacySourceRoot {
    Path(SecureStateDirectory),
    Anchored(File),
}

impl LegacySourceRoot {
    fn descriptor_clone(&self) -> Result<File, StorageError> {
        match self {
            Self::Path(directory) => Ok(directory.descriptor_clone()?),
            Self::Anchored(directory) => Ok(directory.try_clone()?),
        }
    }

    fn validate(&self) -> Result<(), StorageError> {
        match self {
            Self::Path(directory) => directory
                .validate_path_correspondence()
                .map_err(map_secure_state_error),
            Self::Anchored(directory) => {
                let metadata = directory.metadata()?;
                if !metadata.file_type().is_dir()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.permissions().mode() & 0o777 != 0o700
                {
                    return Err(invalid("anchored legacy source root is invalid"));
                }
                Ok(())
            }
        }
    }
}

impl std::fmt::Debug for LegacySourceSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LegacySourceSet(..)")
    }
}

impl LegacySourceSet {
    pub fn at(state_root: &Path) -> Result<Self, StorageError> {
        let state_root = SecureStateDirectory::open_existing_strict(state_root)
            .map_err(map_secure_state_error)?;
        Ok(Self {
            state_root: LegacySourceRoot::Path(state_root),
        })
    }

    pub(super) fn from_descriptor(state_root: &File) -> Result<Self, StorageError> {
        let state_root = LegacySourceRoot::Anchored(state_root.try_clone()?);
        state_root.validate()?;
        Ok(Self { state_root })
    }

    pub fn descriptors(&self) -> &'static [LegacySourceDescriptor] {
        &LEGACY_SOURCES
    }

    pub fn fingerprints(&self) -> Result<Vec<LegacyFingerprint>, StorageError> {
        let fingerprints = self
            .descriptors()
            .iter()
            .copied()
            .map(|descriptor| self.fingerprint(descriptor))
            .collect::<Result<Vec<_>, _>>()?;
        self.state_root.validate()?;
        Ok(fingerprints)
    }

    /// Streams static-source fingerprints followed by arbitrary journal-entry
    /// fingerprints. Consumers must compare/store entries by relative path;
    /// descriptor-relative directory enumeration has no stable order.
    pub(crate) fn stream_fingerprints(
        &self,
        sink: &mut impl FnMut(LegacyFingerprint) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        for descriptor in self.descriptors().iter().copied() {
            let fingerprint = self.fingerprint(descriptor)?;
            let present = fingerprint.present;
            sink(fingerprint)?;
            if present && descriptor.kind == LegacySourceKind::PermissionTransactions {
                let root = self.state_root.descriptor_clone()?;
                let source = open_relative(&root, descriptor)?
                    .ok_or_else(|| invalid("legacy journal directory disappeared"))?;
                let mut entries = DirectoryEntries::open(&source.file)?;
                while let Some(name) = entries.next_name()? {
                    validate_journal_name(&name)?;
                    let file = openat(&source.file, &name, libc::O_RDONLY)?
                        .ok_or_else(|| invalid("legacy journal disappeared"))?;
                    let metadata = file.metadata()?;
                    validate_regular(&metadata)?;
                    sink(fingerprint_from_metadata(
                        descriptor.kind,
                        PathBuf::from(descriptor.relative_path).join(name),
                        &metadata,
                    ))?;
                }
            }
        }
        self.state_root.validate()?;
        Ok(())
    }

    pub fn read_all_bounded(&self) -> Result<LegacySnapshot, StorageError> {
        let mut source_count = 0;
        for descriptor in self.descriptors().iter().copied() {
            if self.fingerprint(descriptor)?.present {
                source_count += 1;
            }
        }
        let mut counts = CountingSink::default();
        let max_record_buffer_bytes = self.stream_into_with_metrics(&mut counts)?;
        Ok(LegacySnapshot {
            source_count,
            decision_count: counts.decisions,
            activity_count: counts.activities,
            lifecycle_count: counts.lifecycle,
            journal_count: counts.journals,
            review_state_count: counts.review,
            max_record_buffer_bytes,
        })
    }

    pub(crate) fn stream_into(&self, sink: &mut impl LegacyImportSink) -> Result<(), StorageError> {
        self.stream_into_with_metrics(sink).map(|_| ())
    }

    pub(crate) fn stream_kind_into(
        &self,
        kind: LegacySourceKind,
        sink: &mut impl LegacyImportSink,
    ) -> Result<usize, StorageError> {
        let descriptor = self
            .descriptors()
            .iter()
            .copied()
            .find(|descriptor| descriptor.kind == kind)
            .ok_or_else(|| invalid("unknown legacy source kind"))?;
        let root = self.state_root.descriptor_clone()?;
        let Some(mut source) = open_relative(&root, descriptor)? else {
            return Ok(0);
        };
        let maximum = match descriptor.kind {
            LegacySourceKind::Decisions => stream_decisions(&mut source.file, sink)?,
            LegacySourceKind::Activity => stream_activity(&mut source.file, sink)?,
            LegacySourceKind::Lifecycle => {
                let bytes = read_bounded_file(
                    &mut source.file,
                    coding_brain_core::lifecycle::MAX_SNAPSHOT_BYTES,
                )?;
                let snapshot = coding_brain_core::lifecycle::decode_legacy_snapshot(&bytes)
                    .map_err(|_| invalid("invalid legacy lifecycle snapshot"))?;
                sink.lifecycle(snapshot)?;
                bytes.len()
            }
            LegacySourceKind::PermissionTransactions => stream_journals(&source.file, sink)?,
            LegacySourceKind::ReviewState => {
                let bytes = read_bounded_file(
                    &mut source.file,
                    coding_brain_core::review_state::MAX_REVIEW_STATE_BYTES,
                )?;
                let snapshot =
                    decode_review(&bytes).map_err(|_| invalid("invalid legacy review state"))?;
                sink.review(snapshot)?;
                bytes.len()
            }
        };
        self.state_root.validate()?;
        Ok(maximum)
    }

    fn stream_into_with_metrics(
        &self,
        sink: &mut impl LegacyImportSink,
    ) -> Result<usize, StorageError> {
        let mut max_record_buffer_bytes = 0;
        for descriptor in self.descriptors().iter().copied() {
            max_record_buffer_bytes =
                max_record_buffer_bytes.max(self.stream_kind_into(descriptor.kind, sink)?);
        }
        self.state_root.validate()?;
        Ok(max_record_buffer_bytes)
    }

    fn fingerprint(
        &self,
        descriptor: LegacySourceDescriptor,
    ) -> Result<LegacyFingerprint, StorageError> {
        let root = self.state_root.descriptor_clone()?;
        match open_relative(&root, descriptor)? {
            Some(source) => Ok(LegacyFingerprint {
                kind: descriptor.kind,
                relative_path: PathBuf::from(descriptor.relative_path),
                present: true,
                device: source.metadata.dev(),
                inode: source.metadata.ino(),
                size: source.metadata.len(),
                modified_seconds: source.metadata.mtime(),
                modified_nanoseconds: source.metadata.mtime_nsec(),
            }),
            None => Ok(LegacyFingerprint {
                kind: descriptor.kind,
                relative_path: PathBuf::from(descriptor.relative_path),
                present: false,
                device: 0,
                inode: 0,
                size: 0,
                modified_seconds: 0,
                modified_nanoseconds: 0,
            }),
        }
    }
}

/// Holds every mutable legacy writer gate while a non-hook migration step
/// validates a stable source view. The coordinator does not acquire this guard
/// until the final cutover slice is implemented.
pub struct LegacyWriterGuard {
    state_root_path: PathBuf,
    state_root: SecureStateDirectory,
    journal_directory: File,
    allow_frozen_journal: bool,
    writer_locks: Vec<HeldLegacyLock>,
}

impl std::fmt::Debug for LegacyWriterGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LegacyWriterGuard(..)")
    }
}

impl LegacyWriterGuard {
    pub fn acquisition_order() -> &'static [&'static str] {
        &LEGACY_WRITER_LOCK_ORDER
    }

    pub fn acquire(
        state_root_path: &Path,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        Self::acquire_internal(state_root_path, deadline, false)
    }

    pub fn acquire_freeze_resume(
        state_root_path: &Path,
        deadline: StorageDeadline,
    ) -> Result<Self, StorageError> {
        Self::acquire_internal(state_root_path, deadline, true)
    }

    fn acquire_internal(
        state_root_path: &Path,
        deadline: StorageDeadline,
        allow_frozen_journal: bool,
    ) -> Result<Self, StorageError> {
        // This may create absent writer guards, including the journal directory.
        // Coordinator integration must therefore acquire the guard before its
        // initial legacy fingerprint capture, not only at final cutover.
        deadline.ensure_remaining()?;
        let state_root = SecureStateDirectory::open_existing_strict(state_root_path)
            .map_err(map_secure_state_error)?;
        let root = state_root.descriptor_clone()?;
        let brain = open_or_create_guard_directory(&root, "brain")?;
        let journal_directory = open_or_create_journal_guard_directory(
            &brain,
            "permission-transactions",
            allow_frozen_journal,
        )?;
        lock_until_deadline(&journal_directory, deadline)?;

        let mut guard = Self {
            state_root_path: state_root_path.to_owned(),
            state_root,
            journal_directory,
            allow_frozen_journal,
            writer_locks: Vec::with_capacity(4),
        };
        drain_journal_writers(&guard.journal_directory, deadline, allow_frozen_journal)?;
        for relative_path in &LEGACY_WRITER_LOCK_ORDER[1..] {
            let file = open_or_create_guard_lock(&root, relative_path)?;
            lock_until_deadline(&file, deadline)?;
            guard.writer_locks.push(HeldLegacyLock {
                relative_path,
                file,
            });
        }
        guard.validate()?;
        Ok(guard)
    }

    pub fn validate(&self) -> Result<(), StorageError> {
        self.state_root
            .validate_path_correspondence()
            .map_err(map_secure_state_error)?;
        let root = self.state_root.descriptor_clone()?;
        validate_held_directory(
            &root,
            "brain/permission-transactions",
            &self.journal_directory,
            self.allow_frozen_journal,
        )?;
        for held in &self.writer_locks {
            validate_held_lock(&root, held.relative_path, &held.file)?;
        }
        self.state_root
            .validate_path_correspondence()
            .map_err(map_secure_state_error)
    }

    /// Fingerprints every source and journal entry while all writer gates are retained.
    pub fn fingerprints(&self) -> Result<Vec<LegacyFingerprint>, StorageError> {
        self.validate()?;
        let sources = LegacySourceSet::at(&self.state_root_path)?;
        let mut fingerprints = Vec::new();
        let mut journal_entries = 0usize;
        let mut journal_name_bytes = 0usize;
        sources.stream_fingerprints(&mut |fingerprint| {
            if fingerprint.kind == LegacySourceKind::PermissionTransactions
                && fingerprint.relative_path() != Path::new("brain/permission-transactions")
            {
                journal_entries = journal_entries
                    .checked_add(1)
                    .ok_or_else(|| invalid("legacy journal guard entry count overflow"))?;
                let name_bytes = fingerprint
                    .relative_path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| invalid("legacy journal guard name is not UTF-8"))?
                    .len();
                journal_name_bytes = journal_name_bytes
                    .checked_add(name_bytes)
                    .ok_or_else(|| invalid("legacy journal guard name budget overflow"))?;
                if journal_entries > MAX_JOURNAL_GUARD_ENTRIES
                    || journal_name_bytes > MAX_JOURNAL_GUARD_NAME_BYTES
                {
                    return Err(invalid(
                        "legacy journal guard enumeration exceeds its bound",
                    ));
                }
            }
            fingerprints.push(fingerprint);
            Ok(())
        })?;
        self.validate()?;
        Ok(fingerprints)
    }

    pub(super) fn final_journal_paths(
        &self,
        deadline: StorageDeadline,
    ) -> Result<Vec<PathBuf>, StorageError> {
        self.validate()?;
        let entries = enumerate_journal_entries(
            &self.journal_directory,
            deadline,
            self.allow_frozen_journal,
        )?;
        let mut paths = Vec::with_capacity(entries.len());
        for entry in entries {
            validate_journal_name(&entry.name)?;
            paths.push(Path::new("brain/permission-transactions").join(entry.name));
        }
        self.validate()?;
        Ok(paths)
    }

    pub fn freeze_journal_directory(&mut self) -> Result<(), StorageError> {
        self.validate()?;
        if unsafe { libc::fchmod(self.journal_directory.as_raw_fd(), 0o500) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.journal_directory.sync_all()?;
        let root = self.state_root.descriptor_clone()?;
        let brain = open_existing_guard_path(&root, "brain")?;
        brain.sync_all()?;
        self.allow_frozen_journal = true;
        validate_held_directory(
            &root,
            "brain/permission-transactions",
            &self.journal_directory,
            true,
        )
    }

    pub fn prepare_freeze(
        &self,
        relative_path: &Path,
        temporary_name: &str,
        expected: &LegacyFingerprint,
    ) -> Result<LegacyFreezeArtifact, StorageError> {
        self.validate()?;
        validate_freeze_request(relative_path, temporary_name, expected)?;
        let root = self.state_root.descriptor_clone()?;
        let (parent_path, source_name) = split_relative_path(relative_path)?;
        let parent = open_existing_guard_path(&root, parent_path)?;
        let source = open_required_at(&parent, source_name, libc::O_RDONLY)?;
        let source_metadata = source.metadata()?;
        validate_regular(&source_metadata)?;
        validate_expected_fingerprint(expected, &source_metadata)?;

        let temporary_c = CString::new(temporary_name)
            .map_err(|_| invalid("legacy freeze temporary name contains NUL"))?;
        let temporary = match metadata_at(&parent, &temporary_c) {
            Ok(identity) => {
                validate_freeze_temp_identity(identity)?;
                match identity.mode & 0o777 {
                    0o600 => {
                        let file = open_required_at(&parent, temporary_name, libc::O_RDWR)?;
                        validate_entry_correspondence(&parent, &temporary_c, &file)?;
                        file.set_len(0)?;
                        file
                    }
                    0o400 => {
                        let file = open_required_at(&parent, temporary_name, libc::O_RDONLY)?;
                        validate_entry_correspondence(&parent, &temporary_c, &file)?;
                        let (source_hash, source_size) = hash_file(source.try_clone()?)?;
                        revalidate_source(&parent, source_name, &source, expected)?;
                        let (target_hash, target_size) = hash_file(file.try_clone()?)?;
                        if source_hash != target_hash || source_size != target_size {
                            return Err(invalid(
                                "completed legacy freeze temporary does not match its source",
                            ));
                        }
                        let source_identity = freeze_identity(&source.metadata()?, source_hash);
                        let target_identity = freeze_identity(&file.metadata()?, target_hash);
                        self.validate()?;
                        return Ok(LegacyFreezeArtifact {
                            relative_path: relative_path.to_owned(),
                            temporary_name: temporary_name.to_owned(),
                            source: source_identity,
                            target: target_identity,
                        });
                    }
                    _ => return Err(invalid("legacy freeze temporary has an unsafe mode")),
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_freeze_temp(&parent, &temporary_c)?
            }
            Err(error) => return Err(error.into()),
        };

        let mut source_reader = source.try_clone()?;
        let mut temporary_writer = temporary;
        source_reader.seek(SeekFrom::Start(0))?;
        temporary_writer.seek(SeekFrom::Start(0))?;
        let mut source_hasher = Sha256::new();
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        let mut first = true;
        loop {
            let read = source_reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            temporary_writer.write_all(&buffer[..read])?;
            source_hasher.update(&buffer[..read]);
            size = size
                .checked_add(read as u64)
                .ok_or_else(|| invalid("legacy freeze source size overflow"))?;
            if first {
                first = false;
                legacy_freeze_fault("after-first-copy-chunk");
            }
        }
        revalidate_source(&parent, source_name, &source, expected)?;
        temporary_writer.sync_all()?;
        let source_hash: [u8; 32] = source_hasher.finalize().into();
        let (target_hash, target_size) = hash_file(temporary_writer.try_clone()?)?;
        if source_hash != target_hash || size != target_size {
            return Err(invalid("legacy freeze copy verification failed"));
        }
        if unsafe { libc::fchmod(temporary_writer.as_raw_fd(), 0o400) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        temporary_writer.sync_all()?;
        parent.sync_all()?;
        legacy_freeze_fault("after-prepared-sync");
        validate_entry_correspondence(&parent, &temporary_c, &temporary_writer)?;
        let source_identity = freeze_identity(&source.metadata()?, source_hash);
        let target_identity = freeze_identity(&temporary_writer.metadata()?, target_hash);
        self.validate()?;
        Ok(LegacyFreezeArtifact {
            relative_path: relative_path.to_owned(),
            temporary_name: temporary_name.to_owned(),
            source: source_identity,
            target: target_identity,
        })
    }

    pub fn publish_freeze(&self, artifact: &LegacyFreezeArtifact) -> Result<(), StorageError> {
        self.validate()?;
        validate_artifact_request(artifact)?;
        let root = self.state_root.descriptor_clone()?;
        let (parent_path, source_name) = split_relative_path(&artifact.relative_path)?;
        let parent = open_existing_guard_path(&root, parent_path)?;
        let temporary_c = CString::new(artifact.temporary_name.as_str())
            .map_err(|_| invalid("legacy freeze temporary name contains NUL"))?;

        let canonical = openat(&parent, source_name, libc::O_RDONLY)?;
        let temporary = openat(&parent, &artifact.temporary_name, libc::O_RDONLY)?;
        match (canonical, temporary) {
            (Some(canonical), Some(temporary)) => {
                require_exact_freeze_file(&canonical, &artifact.source, 0o600)?;
                require_exact_freeze_file(&temporary, &artifact.target, 0o400)?;
                validate_entry_correspondence(&parent, &temporary_c, &temporary)?;
                let source_c = CString::new(source_name)
                    .map_err(|_| invalid("legacy freeze source name contains NUL"))?;
                if unsafe {
                    libc::renameat(
                        parent.as_raw_fd(),
                        temporary_c.as_ptr(),
                        parent.as_raw_fd(),
                        source_c.as_ptr(),
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error().into());
                }
                legacy_freeze_fault("after-rename");
                parent.sync_all()?;
            }
            (Some(canonical), None) => {
                require_exact_freeze_file(&canonical, &artifact.target, 0o400)?;
                parent.sync_all()?;
            }
            _ => {
                return Err(invalid(
                    "legacy freeze publication entries are inconsistent",
                ));
            }
        }
        let canonical = open_required_at(&parent, source_name, libc::O_RDONLY)?;
        require_exact_freeze_file(&canonical, &artifact.target, 0o400)?;
        match metadata_at(&parent, &temporary_c) {
            Ok(_) => {
                return Err(invalid(
                    "legacy freeze temporary remained after publication",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.validate()
    }
}

impl Drop for LegacyWriterGuard {
    fn drop(&mut self) {
        for held in self.writer_locks.iter().rev() {
            let _ = FileExt::unlock(&held.file);
        }
        let _ = FileExt::unlock(&self.journal_directory);
    }
}

fn validate_freeze_request(
    relative_path: &Path,
    temporary_name: &str,
    expected: &LegacyFingerprint,
) -> Result<(), StorageError> {
    let regular_source = LEGACY_SOURCES.iter().any(|descriptor| {
        descriptor.kind != LegacySourceKind::PermissionTransactions
            && relative_path == Path::new(descriptor.relative_path)
            && expected.kind == descriptor.kind
    });
    let journal = expected.kind == LegacySourceKind::PermissionTransactions
        && validate_freeze_journal_path(relative_path).is_ok();
    if (!regular_source && !journal)
        || expected.relative_path() != relative_path
        || !expected.present
    {
        return Err(invalid("legacy freeze source is absent or ineligible"));
    }
    validate_temporary_name(relative_path, temporary_name)
}

fn validate_artifact_request(artifact: &LegacyFreezeArtifact) -> Result<(), StorageError> {
    let regular_source = LEGACY_SOURCES.iter().any(|descriptor| {
        descriptor.kind != LegacySourceKind::PermissionTransactions
            && artifact.relative_path == Path::new(descriptor.relative_path)
    });
    if !regular_source && validate_freeze_journal_path(&artifact.relative_path).is_err() {
        return Err(invalid("legacy freeze artifact source is ineligible"));
    }
    validate_temporary_name(&artifact.relative_path, &artifact.temporary_name)
}

fn validate_freeze_journal_path(relative_path: &Path) -> Result<(), StorageError> {
    let parent = relative_path.parent();
    let name = relative_path.file_name().and_then(OsStr::to_str);
    if parent != Some(Path::new("brain/permission-transactions")) {
        return Err(invalid("legacy freeze journal parent is invalid"));
    }
    validate_guard_journal_name(
        name.ok_or_else(|| invalid("legacy freeze journal name is not UTF-8"))?,
    )
}

fn validate_temporary_name(relative_path: &Path, temporary_name: &str) -> Result<(), StorageError> {
    let path = Path::new(temporary_name);
    if temporary_name.is_empty()
        || path.file_name() != Some(OsStr::new(temporary_name))
        || temporary_name == "."
        || temporary_name == ".."
        || relative_path.file_name() == Some(OsStr::new(temporary_name))
    {
        return Err(invalid("legacy freeze temporary name is not one component"));
    }
    let sibling = relative_path.with_file_name(temporary_name);
    let reserved = LEGACY_SOURCES
        .iter()
        .any(|descriptor| sibling == Path::new(descriptor.relative_path))
        || LEGACY_WRITER_LOCK_ORDER
            .iter()
            .any(|path| sibling == Path::new(path.trim_end_matches('/')))
        || sibling == Path::new("session-links.jsonl")
        || sibling == Path::new("session-links.lock")
        || (relative_path.parent() == Some(Path::new("brain/permission-transactions"))
            && validate_guard_journal_name(temporary_name).is_ok());
    if reserved {
        return Err(invalid("legacy freeze temporary name is reserved"));
    }
    Ok(())
}

fn split_relative_path(relative_path: &Path) -> Result<(&str, &str), StorageError> {
    let relative = relative_path
        .to_str()
        .ok_or_else(|| invalid("legacy freeze source path is not UTF-8"))?;
    Ok(relative
        .rsplit_once('/')
        .map_or(("", relative), |(parent, name)| (parent, name)))
}

fn validate_expected_fingerprint(
    expected: &LegacyFingerprint,
    metadata: &fs::Metadata,
) -> Result<(), StorageError> {
    if expected.device != metadata.dev()
        || expected.inode != metadata.ino()
        || expected.size != metadata.size()
        || expected.modified_seconds != metadata.mtime()
        || expected.modified_nanoseconds != metadata.mtime_nsec()
    {
        return Err(invalid(
            "legacy freeze source does not match expected fingerprint",
        ));
    }
    Ok(())
}

fn revalidate_source(
    parent: &File,
    source_name: &str,
    source: &File,
    expected: &LegacyFingerprint,
) -> Result<(), StorageError> {
    let metadata = source.metadata()?;
    validate_regular(&metadata)?;
    validate_expected_fingerprint(expected, &metadata)?;
    let name =
        CString::new(source_name).map_err(|_| invalid("legacy freeze source name contains NUL"))?;
    validate_entry_correspondence(parent, &name, source)
}

fn validate_freeze_temp_identity(identity: EntryIdentity) -> Result<(), StorageError> {
    #[allow(clippy::unnecessary_cast)]
    if identity.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || identity.owner != unsafe { libc::geteuid() }
        || identity.links != 1
    {
        return Err(invalid(
            "legacy freeze temporary is not an owner single-link regular file",
        ));
    }
    Ok(())
}

fn create_freeze_temp(parent: &File, name: &CStr) -> Result<File, StorageError> {
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
    validate_entry_correspondence(parent, name, &file)?;
    validate_freeze_temp_identity(EntryIdentity::from_metadata(&file.metadata()?))?;
    Ok(file)
}

fn validate_entry_correspondence(
    parent: &File,
    name: &CStr,
    file: &File,
) -> Result<(), StorageError> {
    let opened = EntryIdentity::from_metadata(&file.metadata()?);
    let at_path = metadata_at(parent, name)?;
    if opened != at_path {
        return Err(invalid("legacy freeze entry changed after descriptor open"));
    }
    Ok(())
}

fn hash_file(mut file: File) -> Result<([u8; 32], u64), StorageError> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| invalid("legacy freeze file size overflow"))?;
    }
    Ok((hasher.finalize().into(), size))
}

fn freeze_identity(metadata: &fs::Metadata, sha256: [u8; 32]) -> LegacyFreezeIdentity {
    LegacyFreezeIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        mode: metadata.mode(),
        sha256,
    }
}

fn require_exact_freeze_file(
    file: &File,
    expected: &LegacyFreezeIdentity,
    mode: u32,
) -> Result<(), StorageError> {
    let metadata = file.metadata()?;
    let identity = EntryIdentity::from_metadata(&metadata);
    validate_freeze_temp_identity(identity)?;
    if identity.mode & 0o777 != mode {
        return Err(invalid("legacy freeze file mode is not exact"));
    }
    let (hash, size) = hash_file(file.try_clone()?)?;
    if freeze_identity(&metadata, hash) != *expected || size != expected.size {
        return Err(invalid("legacy freeze file does not match its artifact"));
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn legacy_freeze_fault(stage: &str) {
    if std::env::var_os("CODING_BRAIN_SQLITE_LEGACY_FREEZE_FAULT").as_deref()
        == Some(OsStr::new(stage))
    {
        std::process::abort();
    }
}

#[cfg(not(debug_assertions))]
fn legacy_freeze_fault(_stage: &str) {}

struct HeldLegacyLock {
    relative_path: &'static str,
    file: File,
}

fn lock_until_deadline(file: &File, deadline: StorageDeadline) -> Result<(), StorageError> {
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline.remaining()?;
                thread::sleep(LEGACY_LOCK_RETRY.min(remaining));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn open_or_create_guard_lock(root: &File, relative_path: &str) -> Result<File, StorageError> {
    let (parent_path, name) = relative_path
        .rsplit_once('/')
        .map_or(("", relative_path), |(parent, name)| (parent, name));
    let parent = open_or_create_guard_path(root, parent_path)?;
    let name = CString::new(name).map_err(|_| invalid("legacy lock name contains NUL"))?;
    let mut created = false;
    let mut descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
        descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        created = descriptor >= 0;
        if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
            descriptor = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
    }
    if descriptor < 0 {
        return Err(invalid(
            "legacy lock is not a safe descriptor-anchored entry",
        ));
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if created {
        if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        file.sync_all()?;
        parent.sync_all()?;
    }
    validate_held_entry(&parent, &name, &file, false)?;
    Ok(file)
}

fn open_or_create_guard_path(root: &File, relative_path: &str) -> Result<File, StorageError> {
    let mut directory = root.try_clone()?;
    if relative_path.is_empty() {
        return Ok(directory);
    }
    for component in relative_path.split('/') {
        directory = open_or_create_guard_directory(&directory, component)?;
    }
    Ok(directory)
}

fn open_or_create_guard_directory(parent: &File, name: &str) -> Result<File, StorageError> {
    let name =
        CString::new(name).map_err(|_| invalid("legacy guard directory name contains NUL"))?;
    let mut created = false;
    let mut descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700 as libc::mode_t) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        } else {
            created = true;
        }
        descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
    }
    if descriptor < 0 {
        return Err(invalid(
            "legacy guard directory is not a safe descriptor-anchored entry",
        ));
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    if created {
        if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        directory.sync_all()?;
        parent.sync_all()?;
    }
    validate_held_entry(parent, &name, &directory, true)?;
    Ok(directory)
}

fn open_or_create_journal_guard_directory(
    parent: &File,
    name: &str,
    allow_frozen: bool,
) -> Result<File, StorageError> {
    if !allow_frozen {
        return open_or_create_guard_directory(parent, name);
    }
    if let Some(directory) = openat(parent, name, libc::O_RDONLY | libc::O_DIRECTORY)? {
        let identity = EntryIdentity::from_metadata(&directory.metadata()?);
        #[allow(clippy::unnecessary_cast)]
        if identity.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
            || identity.owner != unsafe { libc::geteuid() }
            || !matches!(identity.mode & 0o777, 0o700 | 0o500)
        {
            return Err(invalid("legacy guard directory has an unsafe frozen mode"));
        }
        return Ok(directory);
    }
    open_or_create_guard_directory(parent, name)
}

fn validate_held_directory(
    root: &File,
    relative_path: &str,
    held: &File,
    allow_frozen: bool,
) -> Result<(), StorageError> {
    let (parent_path, name) = relative_path
        .rsplit_once('/')
        .ok_or_else(|| invalid("legacy guard directory path is invalid"))?;
    let parent = open_existing_guard_path(root, parent_path)?;
    let name =
        CString::new(name).map_err(|_| invalid("legacy guard directory name contains NUL"))?;
    if !allow_frozen {
        return validate_held_entry(&parent, &name, held, true);
    }
    let opened = EntryIdentity::from_metadata(&held.metadata()?);
    let at_path = metadata_at(&parent, &name)?;
    for identity in [opened, at_path] {
        #[allow(clippy::unnecessary_cast)]
        if identity.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
            || identity.owner != unsafe { libc::geteuid() }
            || !matches!(identity.mode & 0o777, 0o700 | 0o500)
        {
            return Err(invalid("legacy guard directory has an unsafe frozen mode"));
        }
    }
    if opened != at_path {
        return Err(invalid("legacy guard entry changed after descriptor open"));
    }
    Ok(())
}

fn validate_held_lock(root: &File, relative_path: &str, held: &File) -> Result<(), StorageError> {
    let (parent_path, name) = relative_path
        .rsplit_once('/')
        .map_or(("", relative_path), |(parent, name)| (parent, name));
    let parent = open_existing_guard_path(root, parent_path)?;
    let name = CString::new(name).map_err(|_| invalid("legacy lock name contains NUL"))?;
    validate_held_entry(&parent, &name, held, false)
}

fn open_existing_guard_path(root: &File, relative_path: &str) -> Result<File, StorageError> {
    let mut directory = root.try_clone()?;
    if relative_path.is_empty() {
        return Ok(directory);
    }
    for component in relative_path.split('/') {
        directory = open_required_at(&directory, component, libc::O_RDONLY | libc::O_DIRECTORY)?;
        validate_directory(&directory.metadata()?)?;
    }
    Ok(directory)
}

fn open_required_at(
    directory: &File,
    name: &str,
    flags: libc::c_int,
) -> Result<File, StorageError> {
    openat(directory, name, flags)?.ok_or_else(|| invalid("legacy guard path disappeared"))
}

fn validate_held_entry(
    parent: &File,
    name: &CString,
    held: &File,
    directory: bool,
) -> Result<(), StorageError> {
    let opened = EntryIdentity::from_metadata(&held.metadata()?);
    let at_path = metadata_at(parent, name)?;
    if directory {
        validate_directory_identity(opened)?;
        validate_directory_identity(at_path)?;
    } else {
        validate_regular_identity(opened)?;
        validate_regular_identity(at_path)?;
    }
    if opened != at_path {
        return Err(invalid("legacy guard entry changed after descriptor open"));
    }
    Ok(())
}

fn drain_journal_writers(
    directory: &File,
    deadline: StorageDeadline,
    allow_frozen: bool,
) -> Result<(), StorageError> {
    loop {
        deadline.ensure_remaining()?;
        let before = enumerate_journal_entries(directory, deadline, allow_frozen)?;
        let mut changed = false;
        for entry in &before {
            if !drain_journal_entry(directory, entry, deadline)? {
                changed = true;
                break;
            }
        }
        if !changed && before == enumerate_journal_entries(directory, deadline, allow_frozen)? {
            return Ok(());
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JournalGuardEntry {
    name: String,
    identity: EntryIdentity,
}

#[derive(Debug)]
enum JournalEntryOpen {
    Stable(File),
    Changed,
}

fn validate_journal_entry_identity(
    identity: EntryIdentity,
    expected: &JournalGuardEntry,
) -> Result<(), StorageError> {
    if expected.identity.mode & 0o777 == 0o400 {
        validate_freeze_temp_identity(identity)?;
        if identity.mode & 0o777 != 0o400 {
            return Err(invalid("legacy freeze temporary has an unsafe mode"));
        }
        Ok(())
    } else {
        validate_regular_identity(identity)
    }
}

fn open_journal_entry(
    directory: &File,
    expected: &JournalGuardEntry,
) -> Result<JournalEntryOpen, StorageError> {
    open_journal_entry_with(directory, expected, &mut || {})
}

fn open_journal_entry_with<F>(
    directory: &File,
    expected: &JournalGuardEntry,
    after_open: &mut F,
) -> Result<JournalEntryOpen, StorageError>
where
    F: FnMut(),
{
    let name = CString::new(expected.name.as_str())
        .map_err(|_| invalid("legacy journal guard name contains NUL"))?;
    let before = match metadata_at(directory, &name) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JournalEntryOpen::Changed);
        }
        Err(error) => return Err(error.into()),
    };
    validate_journal_entry_identity(before, expected)?;
    if before != expected.identity {
        return Ok(JournalEntryOpen::Changed);
    }

    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(JournalEntryOpen::Changed)
        } else {
            Err(invalid(
                "legacy source is not a safe descriptor-anchored entry",
            ))
        };
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    let opened = EntryIdentity::from_metadata(&file.metadata()?);
    validate_journal_entry_identity(opened, expected)?;
    if opened != expected.identity {
        return Ok(JournalEntryOpen::Changed);
    }
    after_open();
    let after = match metadata_at(directory, &name) {
        Ok(identity) => identity,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(JournalEntryOpen::Changed);
        }
        Err(error) => return Err(error.into()),
    };
    validate_journal_entry_identity(after, expected)?;
    if after != expected.identity {
        return Ok(JournalEntryOpen::Changed);
    }
    Ok(JournalEntryOpen::Stable(file))
}

fn enumerate_journal_entries(
    directory: &File,
    deadline: StorageDeadline,
    allow_frozen: bool,
) -> Result<Vec<JournalGuardEntry>, StorageError> {
    loop {
        deadline.ensure_remaining()?;
        let mut entries = Vec::new();
        let mut name_bytes = 0usize;
        let mut changed = false;
        let mut stream = DirectoryEntries::open(directory)?;
        while let Some(name) = stream.next_name()? {
            deadline.ensure_remaining()?;
            validate_guard_journal_name(&name)?;
            name_bytes = name_bytes
                .checked_add(name.len())
                .ok_or_else(|| invalid("legacy journal guard name budget overflow"))?;
            if entries.len() >= MAX_JOURNAL_GUARD_ENTRIES
                || name_bytes > MAX_JOURNAL_GUARD_NAME_BYTES
            {
                return Err(invalid(
                    "legacy journal guard enumeration exceeds its bound",
                ));
            }
            let name_c = CString::new(name.as_str())
                .map_err(|_| invalid("legacy journal guard name contains NUL"))?;
            let identity = match metadata_at(directory, &name_c) {
                Ok(identity) => identity,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    changed = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            if allow_frozen && identity.mode & 0o777 == 0o400 {
                validate_freeze_temp_identity(identity)?;
            } else {
                validate_regular_identity(identity)?;
            }
            entries.push(JournalGuardEntry { name, identity });
        }
        if changed {
            continue;
        }
        entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));
        return Ok(entries);
    }
}

fn drain_journal_entry(
    directory: &File,
    expected: &JournalGuardEntry,
    deadline: StorageDeadline,
) -> Result<bool, StorageError> {
    loop {
        deadline.ensure_remaining()?;
        let file = match open_journal_entry(directory, expected)? {
            JournalEntryOpen::Stable(file) => file,
            JournalEntryOpen::Changed => return Ok(false),
        };
        match file.try_lock_exclusive() {
            Ok(()) => {
                let stable = validate_journal_entry_path(directory, expected, &file)?;
                FileExt::unlock(&file)?;
                return Ok(stable);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !journal_entry_path_matches(directory, expected)? {
                    return Ok(false);
                }
                let remaining = deadline.remaining()?;
                thread::sleep(LEGACY_LOCK_RETRY.min(remaining));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn validate_journal_entry_path(
    directory: &File,
    expected: &JournalGuardEntry,
    file: &File,
) -> Result<bool, StorageError> {
    let opened = EntryIdentity::from_metadata(&file.metadata()?);
    if !journal_entry_path_matches(directory, expected)? {
        return Ok(false);
    }
    validate_journal_entry_identity(opened, expected)?;
    Ok(opened == expected.identity)
}

fn journal_entry_path_matches(
    directory: &File,
    expected: &JournalGuardEntry,
) -> Result<bool, StorageError> {
    let name = CString::new(expected.name.as_str())
        .map_err(|_| invalid("legacy journal guard name contains NUL"))?;
    match metadata_at(directory, &name) {
        Ok(identity) => {
            validate_journal_entry_identity(identity, expected)?;
            Ok(identity == expected.identity)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn validate_guard_journal_name(name: &str) -> Result<(), StorageError> {
    let identity = if let Some(identity) = name
        .strip_prefix("permission-transaction-")
        .and_then(|value| value.strip_suffix(".json"))
    {
        identity
    } else if let Some(identity) = name.strip_prefix("permission-transaction.tmp-") {
        identity
    } else {
        return Err(invalid("unexpected legacy permission transaction entry"));
    };
    validate_journal_identity(identity)
}

fn validate_journal_identity(identity: &str) -> Result<(), StorageError> {
    let mut parts = identity.split('-');
    let exact = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(nanos), Some(pid), Some(sequence), None)
            if nanos.len() == 39
                && pid.len() == 10
                && sequence.len() == 20
                && nanos.bytes().all(|byte| byte.is_ascii_digit())
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    );
    if exact {
        Ok(())
    } else {
        Err(invalid("unexpected legacy permission transaction entry"))
    }
}

#[allow(clippy::unnecessary_cast)]
fn validate_directory_identity(identity: EntryIdentity) -> Result<(), StorageError> {
    if identity.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || identity.owner != unsafe { libc::geteuid() }
        || identity.mode & 0o777 != 0o700
    {
        return Err(invalid("legacy guard directory is not owner-only"));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)]
fn validate_regular_identity(identity: EntryIdentity) -> Result<(), StorageError> {
    if identity.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || identity.owner != unsafe { libc::geteuid() }
        || identity.mode & 0o777 != 0o600
        || identity.links != 1
    {
        return Err(invalid(
            "legacy guard file is not an owner-only single-link regular file",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct CountingSink {
    decisions: u64,
    activities: u64,
    lifecycle: u64,
    journals: u64,
    review: u64,
}

impl LegacyImportSink for CountingSink {
    fn decision(&mut self, decision: LegacyDecision) -> Result<(), StorageError> {
        match decision {
            LegacyDecision::Hook(record) => drop(record),
            LegacyDecision::Audit(record) => drop(record),
        }
        increment(&mut self.decisions)
    }

    fn activity(&mut self, activity: ActivityEvent) -> Result<(), StorageError> {
        drop(activity);
        increment(&mut self.activities)
    }

    fn lifecycle(&mut self, lifecycle: LifecycleSnapshot) -> Result<(), StorageError> {
        drop(lifecycle);
        increment(&mut self.lifecycle)
    }

    fn journal(&mut self, journal: PermissionTransactionJournal) -> Result<(), StorageError> {
        drop(journal);
        increment(&mut self.journals)
    }

    fn review(&mut self, review: ReviewStateSnapshot) -> Result<(), StorageError> {
        drop(review);
        increment(&mut self.review)
    }
}

fn increment(value: &mut u64) -> Result<(), StorageError> {
    *value = value
        .checked_add(1)
        .ok_or_else(|| invalid("legacy record count overflow"))?;
    if *value > i64::MAX as u64 {
        return Err(invalid("legacy record count exceeds SQLite integer range"));
    }
    Ok(())
}

fn stream_decisions(
    file: &mut File,
    sink: &mut impl LegacyImportSink,
) -> Result<usize, StorageError> {
    stream_lines(file, MAX_DECISION_RECORD_BYTES as usize, |line| {
        let value = decode_exact_json(line).ok_or_else(|| invalid("invalid legacy decision"))?;
        let user_action = value.get("user_action").and_then(serde_json::Value::as_str);
        let decision = if matches!(user_action, Some("hook_proposal" | "deterministic_deny")) {
            if !hook_decision_numbers_are_lossless(line) {
                return Err(invalid("legacy hook decision contains a lossy number"));
            }
            let record: HookDecisionRecord = serde_json::from_value(value.clone())
                .map_err(|_| invalid("invalid legacy hook decision"))?;
            if serde_json::to_value(&record).ok().as_ref() != Some(&value) {
                return Err(invalid("legacy hook decision is not exact"));
            }
            if !validate_hook_decision_record(&record) {
                return Err(invalid("legacy hook decision is semantically invalid"));
            }
            LegacyDecision::Hook(record)
        } else {
            validate_decision_numbers(&value)?;
            LegacyDecision::Audit(
                parse_decision_value(&value, &Default::default())
                    .ok_or_else(|| invalid("invalid legacy audit decision"))?,
            )
        };
        sink.decision(decision)
    })
}

fn validate_decision_numbers(value: &serde_json::Value) -> Result<(), StorageError> {
    if value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|pid| u32::try_from(pid).is_err())
    {
        return Err(invalid("legacy decision pid is outside u32"));
    }
    for field in ["suggested_at", "resolved_at", "brain_decision_ms"] {
        if value
            .get(field)
            .is_some_and(|number| number.as_u64().is_none())
        {
            return Err(invalid(
                "legacy decision contains a signed numeric overflow",
            ));
        }
    }
    Ok(())
}

fn stream_activity(
    file: &mut File,
    sink: &mut impl LegacyImportSink,
) -> Result<usize, StorageError> {
    stream_lines(file, MAX_ACTIVITY_EVENT_BYTES, |line| {
        let value = decode_exact_json(line).ok_or_else(|| invalid("invalid legacy activity"))?;
        if let Ok(mut event) = serde_json::from_value::<ActivityEvent>(value.clone()) {
            validate_activity_keys(&value)?;
            if !(MIN_ACTIVITY_SCHEMA_VERSION..=ACTIVITY_SCHEMA_VERSION)
                .contains(&event.schema_version)
                || !event.has_consistent_payload()
            {
                return Err(invalid("unsupported or inconsistent legacy activity"));
            }
            if value.get("kind").is_none() && event.activity_id.starts_with("lifecycle_") {
                event.kind = coding_brain_core::brain_activity::ActivityKind::Lifecycle;
            }
            return sink.activity(event);
        }
        let diagnostic: LegacyDiagnosticRow =
            serde_json::from_value(value).map_err(|_| invalid("invalid legacy activity event"))?;
        if !(MIN_ACTIVITY_SCHEMA_VERSION..=ACTIVITY_SCHEMA_VERSION)
            .contains(&diagnostic.schema_version)
        {
            return Err(invalid("unsupported legacy activity diagnostic"));
        }
        match diagnostic.diagnostic {
            LegacyDiagnostic::TruncatedTail { discarded_bytes } => {
                let _ = discarded_bytes;
            }
            LegacyDiagnostic::MalformedRows { count } => {
                let _ = count;
            }
        }
        Ok(())
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDiagnosticRow {
    schema_version: u32,
    diagnostic: LegacyDiagnostic,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyDiagnostic {
    TruncatedTail { discarded_bytes: u64 },
    MalformedRows { count: usize },
}

fn validate_activity_keys(value: &serde_json::Value) -> Result<(), StorageError> {
    const KEYS: &[&str] = &[
        "schema_version",
        "kind",
        "activity_id",
        "recorded_at_ms",
        "project",
        "session",
        "state",
        "tool",
        "normalized_command",
        "fingerprint",
        "rule_id",
        "confidence",
        "threshold",
        "reasoning",
        "decision_id",
        "outcome",
        "correction",
        "note",
        "supersedes",
    ];
    let object = value
        .as_object()
        .ok_or_else(|| invalid("legacy activity is not an object"))?;
    if object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(invalid("legacy activity contains an unknown field"));
    }
    Ok(())
}

fn stream_lines(
    file: &mut File,
    max_record_bytes: usize,
    mut consume: impl FnMut(&[u8]) -> Result<(), StorageError>,
) -> Result<usize, StorageError> {
    let mut reader = BufReader::new(file);
    let mut line = Vec::with_capacity(8 * 1024);
    let mut maximum_seen = 0;
    loop {
        let buffered = reader.fill_buf()?;
        if buffered.is_empty() {
            if !line.is_empty() {
                consume(&line)?;
            }
            return Ok(maximum_seen);
        }
        let newline = buffered.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffered.len(), |index| index + 1);
        let payload = if newline.is_some() {
            &buffered[..consumed - 1]
        } else {
            &buffered[..consumed]
        };
        if line.len().saturating_add(payload.len()) > max_record_bytes {
            return Err(invalid("legacy record exceeds its frozen size limit"));
        }
        line.extend_from_slice(payload);
        maximum_seen = maximum_seen.max(line.len());
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                consume(&line)?;
            }
            line.clear();
        }
    }
}

fn read_bounded_file(file: &mut File, maximum: usize) -> Result<Vec<u8>, StorageError> {
    if file.metadata()?.len() > maximum as u64 {
        return Err(invalid("legacy source exceeds its frozen size limit"));
    }
    let mut bytes = Vec::new();
    file.take((maximum + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return Err(invalid("legacy source exceeds its frozen size limit"));
    }
    Ok(bytes)
}

fn stream_journals(
    directory: &File,
    sink: &mut impl LegacyImportSink,
) -> Result<usize, StorageError> {
    let mut maximum_seen = 0;
    let mut entries = DirectoryEntries::open(directory)?;
    while let Some(name) = entries.next_name()? {
        validate_journal_name(&name)?;
        let Some(mut file) = openat(directory, &name, libc::O_RDONLY)? else {
            return Err(invalid("legacy permission transaction disappeared"));
        };
        validate_regular(&file.metadata()?)?;
        let bytes = read_bounded_file(&mut file, 1024 * 1024)?;
        maximum_seen = maximum_seen.max(bytes.len());
        let journal = decode_exact_journal(&bytes)
            .ok_or_else(|| invalid("invalid legacy permission transaction"))?;
        validate_journal(&journal)
            .map_err(|_| invalid("invalid legacy permission transaction authority"))?;
        sink.journal(journal)?;
    }
    Ok(maximum_seen)
}

struct DirectoryEntries(*mut libc::DIR);

impl DirectoryEntries {
    fn open(directory: &File) -> Result<Self, StorageError> {
        let duplicate = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if duplicate < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            unsafe { libc::close(duplicate) };
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(stream))
    }

    fn next_name(&mut self) -> Result<Option<String>, StorageError> {
        loop {
            errno::set_errno(errno::Errno(0));
            let entry = unsafe { libc::readdir(self.0) };
            if entry.is_null() {
                let error = errno::errno().0;
                return if error == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::from_raw_os_error(error).into())
                };
            }
            let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            if bytes.len() > 255 || !bytes.is_ascii() {
                return Err(invalid("legacy journal name is unsafe"));
            }
            return String::from_utf8(bytes.to_vec())
                .map(Some)
                .map_err(|_| invalid("legacy journal name is not UTF-8"));
        }
    }
}

impl Drop for DirectoryEntries {
    fn drop(&mut self) {
        unsafe { libc::closedir(self.0) };
    }
}

pub(super) fn validate_journal_name(name: &str) -> Result<(), StorageError> {
    let identity = name
        .strip_prefix("permission-transaction-")
        .and_then(|value| value.strip_suffix(".json"))
        .ok_or_else(|| invalid("unexpected legacy permission transaction entry"))?;
    let mut parts = identity.split('-');
    let exact = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(nanos), Some(pid), Some(sequence), None)
            if nanos.len() == 39
                && pid.len() == 10
                && sequence.len() == 20
                && nanos.bytes().all(|byte| byte.is_ascii_digit())
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    );
    if !exact {
        return Err(invalid("unexpected legacy permission transaction entry"));
    }
    Ok(())
}

fn fingerprint_from_metadata(
    kind: LegacySourceKind,
    relative_path: PathBuf,
    metadata: &fs::Metadata,
) -> LegacyFingerprint {
    LegacyFingerprint {
        kind,
        relative_path,
        present: true,
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

fn invalid(reason: &'static str) -> StorageError {
    StorageError::InvalidStorage(reason)
}

struct OpenedSource {
    #[allow(dead_code)]
    file: File,
    metadata: fs::Metadata,
}

fn open_relative(
    root: &File,
    descriptor: LegacySourceDescriptor,
) -> Result<Option<OpenedSource>, StorageError> {
    let mut directory = root.try_clone()?;
    let components = descriptor.relative_path.split('/').collect::<Vec<_>>();
    for component in &components[..components.len() - 1] {
        let Some(child) = openat(&directory, component, libc::O_RDONLY | libc::O_DIRECTORY)? else {
            return Ok(None);
        };
        validate_directory(&child.metadata()?)?;
        directory = child;
    }
    let final_name = components[components.len() - 1];
    let required_type = if descriptor.kind == LegacySourceKind::PermissionTransactions {
        libc::O_DIRECTORY
    } else {
        0
    };
    let Some(file) = openat(&directory, final_name, libc::O_RDONLY | required_type)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    if descriptor.kind == LegacySourceKind::PermissionTransactions {
        validate_directory(&metadata)?;
    } else {
        validate_regular(&metadata)?;
    }
    Ok(Some(OpenedSource { file, metadata }))
}

fn map_secure_state_error(error: SecureStateError) -> StorageError {
    match error {
        SecureStateError::Io(error) => StorageError::Io(error),
        SecureStateError::InvalidStorage(reason) => StorageError::InvalidStorage(reason),
    }
}

fn openat(directory: &File, name: &str, flags: libc::c_int) -> Result<Option<File>, StorageError> {
    let name = CString::new(name)
        .map_err(|_| StorageError::InvalidStorage("legacy source name contains NUL"))?;
    let before = match metadata_at(directory, &name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor >= 0 {
        let file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryIdentity::from_metadata(&file.metadata()?);
        let after = metadata_at(directory, &name)?;
        if before != opened || opened != after {
            return Err(invalid("legacy source changed during descriptor open"));
        }
        return Ok(Some(file));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound {
        Ok(None)
    } else {
        Err(StorageError::InvalidStorage(
            "legacy source is not a safe descriptor-anchored entry",
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EntryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
}

impl EntryIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            owner: metadata.uid(),
            links: metadata.nlink(),
        }
    }
}

#[allow(clippy::unnecessary_cast)]
fn metadata_at(directory: &File, name: &std::ffi::CStr) -> io::Result<EntryIdentity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok(EntryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        owner: stat.st_uid as u32,
        links: stat.st_nlink as u64,
    })
}

fn validate_regular(metadata: &fs::Metadata) -> Result<(), StorageError> {
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(StorageError::InvalidStorage(
            "legacy source is not an owner-only single-link regular file",
        ));
    }
    Ok(())
}

fn validate_directory(metadata: &fs::Metadata) -> Result<(), StorageError> {
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(StorageError::InvalidStorage(
            "legacy source is not an owner-only directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    use super::*;
    use crate::brain::permission_transaction::test_support;

    fn private_directory(path: &Path) {
        fs::create_dir_all(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn guard_journal_name(index: u64) -> String {
        format!(
            "permission-transaction-{:039}-{:010}-{:020}.json",
            index + 1,
            1,
            index + 1
        )
    }

    fn guarded_journal_fixture() -> (tempfile::TempDir, File, JournalGuardEntry, PathBuf) {
        guarded_journal_fixture_with(0o600, false)
    }

    fn guarded_frozen_journal_fixture() -> (tempfile::TempDir, File, JournalGuardEntry, PathBuf) {
        guarded_journal_fixture_with(0o400, true)
    }

    fn guarded_journal_fixture_with(
        mode: u32,
        allow_frozen: bool,
    ) -> (tempfile::TempDir, File, JournalGuardEntry, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory_path = root.path().join("brain/permission-transactions");
        private_directory(directory_path.parent().unwrap());
        private_directory(&directory_path);
        let path = directory_path.join(guard_journal_name(0));
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(mode)
            .open(&path)
            .unwrap();
        let directory = File::open(&directory_path).unwrap();
        let expected = enumerate_journal_entries(
            &directory,
            StorageDeadline::after(Duration::from_secs(1)),
            allow_frozen,
        )
        .unwrap()
        .pop()
        .unwrap();
        (root, directory, expected, path)
    }

    #[test]
    fn journal_open_classifies_safe_rename_as_changed() {
        let (_root, directory, expected, path) = guarded_journal_fixture();
        let renamed = path.parent().unwrap().join(guard_journal_name(1));
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::rename(&path, &renamed).unwrap();
        });
        assert!(
            matches!(&result, Ok(JournalEntryOpen::Changed)),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_classifies_removal_as_changed() {
        let (_root, directory, expected, path) = guarded_journal_fixture();
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::remove_file(&path).unwrap();
        });
        assert!(
            matches!(&result, Ok(JournalEntryOpen::Changed)),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_rejects_symlink_replacement() {
        let (root, directory, expected, path) = guarded_journal_fixture();
        let outside = root.path().join("outside");
        fs::write(&outside, b"").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(&outside, &path).unwrap();
        });
        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_classifies_safe_same_name_replacement_as_changed() {
        let (_root, directory, expected, path) = guarded_journal_fixture();
        let displaced = path.with_extension("displaced");
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::rename(&path, &displaced).unwrap();
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
        });
        assert!(
            matches!(&result, Ok(JournalEntryOpen::Changed)),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_rejects_wrong_mode_after_open() {
        let (_root, directory, expected, path) = guarded_journal_fixture();
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        });
        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_rejects_wrong_mode_after_open_for_frozen_journal() {
        let (_root, directory, expected, path) = guarded_frozen_journal_fixture();
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        });
        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn journal_open_classifies_safe_same_name_frozen_replacement_as_changed() {
        let (_root, directory, expected, path) = guarded_frozen_journal_fixture();
        let displaced = path.with_extension("displaced");
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::rename(&path, &displaced).unwrap();
            fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o400)
                .open(&path)
                .unwrap();
        });
        assert!(
            matches!(&result, Ok(JournalEntryOpen::Changed)),
            "{result:?}"
        );
    }

    #[test]
    fn journal_path_validation_rejects_wrong_mode_for_frozen_journal() {
        let (_root, directory, expected, path) = guarded_frozen_journal_fixture();
        let file = File::open(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let result = validate_journal_entry_path(&directory, &expected, &file);

        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn journal_path_validation_classifies_removal_as_changed() {
        let (_root, directory, expected, path) = guarded_journal_fixture();
        let file = File::open(&path).unwrap();
        fs::remove_file(&path).unwrap();

        let result = validate_journal_entry_path(&directory, &expected, &file);

        assert!(matches!(result, Ok(false)), "{result:?}");
    }

    #[test]
    fn journal_path_matcher_rejects_wrong_mode_for_frozen_journal() {
        let (_root, directory, expected, path) = guarded_frozen_journal_fixture();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let result = journal_entry_path_matches(&directory, &expected);

        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn journal_identity_rejects_wrong_owner() {
        let (_root, _directory, expected, _path) = guarded_journal_fixture();
        let identity = EntryIdentity {
            owner: expected.identity.owner ^ 1,
            ..expected.identity
        };

        assert!(matches!(
            validate_journal_entry_identity(identity, &expected),
            Err(StorageError::InvalidStorage(_))
        ));
    }

    #[test]
    #[allow(clippy::unnecessary_cast)]
    fn journal_identity_rejects_unsupported_type() {
        let (_root, _directory, expected, _path) = guarded_journal_fixture();
        let identity = EntryIdentity {
            mode: (expected.identity.mode & !(libc::S_IFMT as u32)) | libc::S_IFIFO as u32,
            ..expected.identity
        };

        assert!(matches!(
            validate_journal_entry_identity(identity, &expected),
            Err(StorageError::InvalidStorage(_))
        ));
    }

    #[test]
    fn journal_open_rejects_extra_link_after_open() {
        let (root, directory, expected, path) = guarded_journal_fixture();
        let result = open_journal_entry_with(&directory, &expected, &mut || {
            fs::hard_link(&path, root.path().join("alias")).unwrap();
        });
        assert!(
            matches!(&result, Err(StorageError::InvalidStorage(_))),
            "{result:?}"
        );
    }

    #[test]
    fn arbitrary_journal_count_streams_and_fingerprints_by_exact_path() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("brain/permission-transactions");
        private_directory(directory.parent().unwrap());
        private_directory(&directory);
        for index in 0..300_u64 {
            let identity = format!("{:039}-{:010}-{:020}", index + 1, 1, index + 1);
            let journal = test_support::journal(&format!("journal-{index}"));
            let path = directory.join(format!("permission-transaction-{identity}.json"));
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            serde_json::to_writer(&mut file, &journal).unwrap();
            file.flush().unwrap();
        }

        let sources = LegacySourceSet::at(root.path()).unwrap();
        assert_eq!(sources.read_all_bounded().unwrap().journal_count(), 300);

        let mut first = BTreeMap::new();
        sources
            .stream_fingerprints(&mut |fingerprint| {
                first.insert(fingerprint.relative_path().to_owned(), fingerprint);
                Ok(())
            })
            .unwrap();
        assert_eq!(first.len(), 305);
        assert_eq!(
            first
                .keys()
                .filter(|path| path.components().count() == 3)
                .count(),
            300
        );

        let changed = directory.join(format!(
            "permission-transaction-{:039}-{:010}-{:020}.json",
            1, 1, 1
        ));
        let mut file = fs::OpenOptions::new().append(true).open(&changed).unwrap();
        file.write_all(b" ").unwrap();
        drop(file);
        let mut second = BTreeMap::new();
        sources
            .stream_fingerprints(&mut |fingerprint| {
                second.insert(fingerprint.relative_path().to_owned(), fingerprint);
                Ok(())
            })
            .unwrap();
        assert_ne!(
            first[&changed.strip_prefix(root.path()).unwrap().to_owned()],
            second[&changed.strip_prefix(root.path()).unwrap().to_owned()]
        );
    }

    #[test]
    fn permission_journal_decoder_rejects_duplicate_keys() {
        let encoded = serde_json::to_string(&test_support::journal("tx-duplicate-key")).unwrap();
        let duplicate_top_level = encoded.replacen('{', "{\"schema_version\":1,", 1);
        let duplicate_nested = encoded.replacen(
            "\"proposal\":{",
            "\"proposal\":{\"decision_type\":\"session\",",
            1,
        );

        assert!(decode_exact_journal(duplicate_top_level.as_bytes()).is_none());
        assert!(decode_exact_journal(duplicate_nested.as_bytes()).is_none());
    }

    #[test]
    fn permission_journal_decoder_rejects_lossy_numbers_in_every_float_field() {
        let encoded = serde_json::to_string(&test_support::journal("tx-lossy-number")).unwrap();
        let fields = [
            ("proposal.brain_confidence", "brain_confidence", "0.9"),
            ("proposal.brain_threshold", "brain_threshold", "0.8"),
            ("terminal.confidence", "confidence", "0.9"),
            ("terminal.threshold", "threshold", "0.8"),
        ];
        let lossy_tokens = ["1e-9999999999", "18446744073709551616"];
        let mut accepted = Vec::new();

        for (path, key, original) in fields {
            let needle = format!("\"{key}\":{original}");
            for token in lossy_tokens {
                let replacement = format!("\"{key}\":{token}");
                let altered = encoded.replacen(&needle, &replacement, 1);
                assert_ne!(altered, encoded, "missing fixture field {path}");
                if decode_exact_journal(altered.as_bytes()).is_some() {
                    accepted.push(format!("{path}={token}"));
                }
            }
        }

        assert!(accepted.is_empty(), "accepted lossy numbers: {accepted:?}");
    }
}
