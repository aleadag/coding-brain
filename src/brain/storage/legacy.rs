use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, MAX_ACTIVITY_EVENT_BYTES, MIN_ACTIVITY_SCHEMA_VERSION,
};
use coding_brain_core::lifecycle::LifecycleSnapshot;
use serde::Deserialize;

use crate::brain::decisions::{
    DecisionRecord, HookDecisionRecord, MAX_DECISION_RECORD_BYTES, parse_decision_value,
};
use crate::brain::permission_transaction::{
    PermissionTransactionJournal, decode_exact_journal, decode_exact_json, validate_journal,
};
use crate::brain::review_state::{ReviewStateSnapshot, decode_legacy_snapshot as decode_review};
use crate::brain::secure_state::{SecureStateDirectory, SecureStateError};

use super::StorageError;

pub const LEGACY_EXPORT_PROFILE: &str = "legacy-v0.59.1";

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
    state_root: SecureStateDirectory,
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
        self.state_root
            .validate_path_correspondence()
            .map_err(map_secure_state_error)?;
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
        self.state_root
            .validate_path_correspondence()
            .map_err(map_secure_state_error)?;
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

    fn stream_into_with_metrics(
        &self,
        sink: &mut impl LegacyImportSink,
    ) -> Result<usize, StorageError> {
        let mut max_record_buffer_bytes = 0;
        for descriptor in self.descriptors().iter().copied() {
            let root = self.state_root.descriptor_clone()?;
            let Some(mut source) = open_relative(&root, descriptor)? else {
                continue;
            };
            match descriptor.kind {
                LegacySourceKind::Decisions => {
                    max_record_buffer_bytes =
                        max_record_buffer_bytes.max(stream_decisions(&mut source.file, sink)?);
                }
                LegacySourceKind::Activity => {
                    max_record_buffer_bytes =
                        max_record_buffer_bytes.max(stream_activity(&mut source.file, sink)?);
                }
                LegacySourceKind::Lifecycle => {
                    let bytes = read_bounded_file(
                        &mut source.file,
                        coding_brain_core::lifecycle::MAX_SNAPSHOT_BYTES,
                    )?;
                    let snapshot = coding_brain_core::lifecycle::decode_legacy_snapshot(&bytes)
                        .map_err(|_| invalid("invalid legacy lifecycle snapshot"))?;
                    sink.lifecycle(snapshot)?;
                    max_record_buffer_bytes = max_record_buffer_bytes.max(bytes.len());
                }
                LegacySourceKind::PermissionTransactions => {
                    max_record_buffer_bytes =
                        max_record_buffer_bytes.max(stream_journals(&source.file, sink)?);
                }
                LegacySourceKind::ReviewState => {
                    let bytes = read_bounded_file(
                        &mut source.file,
                        coding_brain_core::review_state::MAX_REVIEW_STATE_BYTES,
                    )?;
                    let snapshot = decode_review(&bytes)
                        .map_err(|_| invalid("invalid legacy review state"))?;
                    sink.review(snapshot)?;
                    max_record_buffer_bytes = max_record_buffer_bytes.max(bytes.len());
                }
            }
        }
        self.state_root
            .validate_path_correspondence()
            .map_err(map_secure_state_error)?;
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
            let record: HookDecisionRecord = serde_json::from_value(value.clone())
                .map_err(|_| invalid("invalid legacy hook decision"))?;
            if serde_json::to_value(&record).ok().as_ref() != Some(&value) {
                return Err(invalid("legacy hook decision is not exact"));
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
        let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
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

fn validate_journal_name(name: &str) -> Result<(), StorageError> {
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
}
