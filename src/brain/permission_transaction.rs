#![allow(dead_code)] // Task 3 APIs are integrated by the following recovery and hook tasks.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::ffi::{CStr, CString};
#[cfg(unix)]
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::path::Component;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityState, MAX_ACTIVITY_EVENT_BYTES,
};
use coding_brain_core::lifecycle::{
    EnsurePermissionDecision, LifecycleIdentity, LifecycleStore, MAX_ID_BYTES, PermissionAction,
    PermissionAuthority, PermissionDecision, PermissionDisposition,
};
use fs2::FileExt;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::activity::{ActivityLog, ActivityStore, ActivityStoreError, LiveEvidenceBudget};
use super::decisions::{
    DecisionStoreError, EnsureRecord, HookDecisionRecord, ensure_hook_record_at,
    ensure_hook_record_at_bounded, validate_hook_decision_record,
};
use super::permission_request_lock::{
    PermissionRequestGuard, PermissionRequestLockStore, state_root_for_traversal,
};

const JOURNAL_SCHEMA_VERSION: u32 = 2;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_JOURNALS: usize = 256;
const DEFAULT_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const LIVE_MAX_DESTINATION_BYTES: usize = 16 * 1024 * 1024;
const FINAL_PREFIX: &str = "permission-transaction-";
const FINAL_SUFFIX: &str = ".json";
const TEMP_PREFIX: &str = "permission-transaction.tmp-";
const MAX_CREATE_ATTEMPTS: usize = 128;
const RECOVERY_AUTHORITY_ERROR_REASON: &str =
    "permission transaction recovery lacked executable lifecycle authority";

static JOURNAL_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PermissionTransactionJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub proposal: HookDecisionRecord,
    pub terminal: ActivityEvent,
    pub lifecycle_identity: LifecycleIdentity,
    pub request_key: String,
    pub disposition: PermissionDisposition,
    pub allow_requires_lifecycle_authority: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryLimits {
    pub max_journals: usize,
    pub max_total_bytes: usize,
    pub max_destination_bytes: usize,
    pub directory_lock: LockAcquisition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LockAcquisition {
    Blocking,
    Nonblocking,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            max_journals: DEFAULT_MAX_JOURNALS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_destination_bytes: usize::MAX,
            directory_lock: LockAcquisition::Nonblocking,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use coding_brain_core::brain_activity::{
        ActivityKind, ProjectEvidence, SessionTarget, SessionTargetProvenance,
    };
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::provider::AgentProvider;

    pub(crate) fn journal(transaction_id: &str) -> PermissionTransactionJournal {
        let cwd = PathBuf::from("/work/project");
        let decision_id = format!("decision-{transaction_id}");
        let project_id = ProjectId::Temporary("project-id".into());
        PermissionTransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            transaction_id: transaction_id.into(),
            proposal: HookDecisionRecord {
                provider: AgentProvider::Codex,
                ts: "2026-07-31T00:00:00Z".into(),
                pid: 0,
                project: "project".into(),
                tool: "Bash".into(),
                command: "printf do-not-leak-this-raw-command".into(),
                brain_action: "approve".into(),
                brain_confidence: 0.9,
                brain_reasoning: "bounded reasoning".into(),
                brain_source: "test".into(),
                brain_threshold: Some(0.8),
                user_action: "hook_proposal".into(),
                decision_type: "session".into(),
                suggested_at: 1,
                resolved_at: 1,
                decision_id: decision_id.clone(),
                session_id: "session-1".into(),
                turn_id: "turn-1".into(),
            },
            terminal: ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: format!("activity-{transaction_id}"),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: cwd.clone(),
                    label: Some("project".into()),
                },
                session: Some(SessionTarget {
                    provider: AgentProvider::Codex,
                    session_id: "session-1".into(),
                    provider_session_id: None,
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("tool-use-1".into()),
                    project_id,
                    cwd: cwd.clone(),
                    provider_hints: Vec::new(),
                    provenance: SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("printf do-not-leak-this-raw-command".into()),
                fingerprint: None,
                rule_id: None,
                confidence: Some(0.9),
                threshold: Some(0.8),
                reasoning: Some("bounded reasoning".into()),
                decision_id: Some(decision_id),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            },
            lifecycle_identity: LifecycleIdentity::try_new(
                AgentProvider::Codex,
                "session-1".into(),
                Some("turn-1".into()),
                None,
                cwd,
            )
            .unwrap(),
            request_key: "a".repeat(64),
            disposition: PermissionDisposition::Decided,
            allow_requires_lifecycle_authority: true,
        }
    }

    pub(crate) fn proposal_only(state_root: &Path, transaction_id: &str) -> String {
        let journal = journal(transaction_id);
        let activity_id = journal.terminal.activity_id.clone();
        let prepared = PermissionTransactionStore::at(state_root)
            .prepare(journal.clone())
            .unwrap();
        ensure_hook_record_at(&state_root.join("brain/decisions.jsonl"), &journal.proposal)
            .unwrap();
        drop(prepared);
        activity_id
    }

    pub(crate) fn active(state_root: &Path, transaction_id: &str) -> PreparedTransaction {
        PermissionTransactionStore::at(state_root)
            .prepare(journal(transaction_id))
            .unwrap()
    }
}

impl RecoveryLimits {
    pub(crate) fn startup() -> Self {
        Self {
            max_journals: DEFAULT_MAX_JOURNALS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_destination_bytes: LIVE_MAX_DESTINATION_BYTES,
            directory_lock: LockAcquisition::Nonblocking,
        }
    }

    pub(crate) fn live() -> Self {
        Self {
            max_journals: 1,
            max_total_bytes: MAX_JOURNAL_BYTES,
            max_destination_bytes: LIVE_MAX_DESTINATION_BYTES,
            directory_lock: LockAcquisition::Nonblocking,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RecoveryReport {
    pub completed: usize,
    pub active: usize,
    pub invalid: usize,
    pub over_budget: usize,
    pub over_budget_detail: Option<OverBudgetDetail>,
    pub removal_sync_uncertain: usize,
    pub pending: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OverBudgetSource {
    JournalCount,
    JournalBytes,
    DecisionEvidence,
    ActivityEvidence,
}

impl OverBudgetSource {
    pub(crate) fn store_label(self) -> &'static str {
        match self {
            Self::JournalCount => "journal_count",
            Self::JournalBytes => "journal_bytes",
            Self::DecisionEvidence => "decisions.jsonl",
            Self::ActivityEvidence => "activity.jsonl",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OverBudgetDetail {
    pub source: OverBudgetSource,
    pub limit: usize,
}

impl RecoveryReport {
    pub(crate) fn rollback_ready(self) -> bool {
        self.active == 0
            && self.invalid == 0
            && self.over_budget == 0
            && self.removal_sync_uncertain == 0
            && self.pending == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommitReport;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryDisposition {
    Completed,
    Active,
    OverBudget,
    Invalid,
    RemovalSyncUncertain,
    Unresolved,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitFault {
    None,
    AfterPrepare,
    AfterProposal,
    AfterLifecycle,
    AfterTerminal,
    BeforeJournalRemoval,
    AfterUnlinkDirectorySyncFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommitPoint {
    AfterPrepare,
    AfterProposal,
    AfterLifecycle,
    AfterTerminal,
    BeforeJournalRemoval,
}

#[cfg(all(unix, debug_assertions))]
fn pause_at_test_fault(point: CommitPoint) {
    let expected = match point {
        CommitPoint::AfterPrepare => "after_prepare",
        CommitPoint::AfterProposal => "after_proposal",
        _ => return,
    };
    if std::env::var("CBRAIN_TEST_PERMISSION_TX_FAULT").as_deref() != Ok(expected) {
        return;
    }
    let (Some(marker), Some(release)) = (
        std::env::var_os("CBRAIN_TEST_PERMISSION_TX_MARKER").map(PathBuf::from),
        std::env::var_os("CBRAIN_TEST_PERMISSION_TX_RELEASE").map(PathBuf::from),
    ) else {
        return;
    };
    if !marker.is_absolute()
        || !release.is_absolute()
        || marker == release
        || marker.parent() != release.parent()
    {
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let Ok(fault_directory) = open_test_fault_directory(&home, &marker, &release) else {
        return;
    };
    let Some(marker_name) = marker.file_name() else {
        return;
    };
    let Some(release_name) = release.file_name() else {
        return;
    };
    if create_test_fault_marker(&fault_directory, marker_name, expected).is_err() {
        return;
    }
    let _ = wait_for_test_fault_release(&fault_directory, release_name);
}

#[cfg(all(unix, debug_assertions))]
fn open_test_fault_directory(home: &Path, marker: &Path, release: &Path) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;

    const FAULT_DIR: &str = ".cbrain-test-permission-tx";

    if !home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test HOME is not absolute",
        ));
    }
    let canonical_home = fs::canonicalize(home)?;
    let canonical_home_metadata = fs::symlink_metadata(&canonical_home)?;
    let home_name = CString::new(canonical_home.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "test HOME contains NUL"))?;
    let home_descriptor = unsafe {
        libc::open(
            home_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if home_descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let home_directory = unsafe { File::from_raw_fd(home_descriptor) };
    let home_metadata = home_directory.metadata()?;
    if !home_metadata.file_type().is_dir()
        || home_metadata.uid() != unsafe { libc::geteuid() }
        || canonical_home_metadata.dev() != home_metadata.dev()
        || canonical_home_metadata.ino() != home_metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test HOME is not owned by the current user",
        ));
    }

    let fault_path = canonical_home.join(FAULT_DIR);
    let marker_parent = marker.parent().map(state_root_for_traversal);
    let release_parent = release.parent().map(state_root_for_traversal);
    if marker_parent.as_deref() != Some(fault_path.as_path())
        || release_parent.as_deref() != Some(fault_path.as_path())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test fault paths are outside the dedicated directory",
        ));
    }
    let fault_name = c".cbrain-test-permission-tx";
    let path_metadata = directory_metadata_at(&home_directory, fault_name)?;
    open_owner_only_directory_at(&home_directory, fault_name, &path_metadata, 0o700)
}

#[cfg(all(unix, debug_assertions))]
fn create_test_fault_marker(
    fault_directory: &File,
    marker_name: &OsStr,
    label: &str,
) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let marker_name = cstring_name(marker_name)?;
    let descriptor = unsafe {
        libc::openat(
            fault_directory.as_raw_fd(),
            marker_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut marker = unsafe { File::from_raw_fd(descriptor) };
    marker.set_permissions(fs::Permissions::from_mode(0o600))?;
    let metadata = marker.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "test fault marker metadata is unsafe",
        ));
    }
    marker.write_all(label.as_bytes())
}

#[cfg(all(unix, debug_assertions))]
fn wait_for_test_fault_release(fault_directory: &File, release_name: &OsStr) -> io::Result<()> {
    let release_name = cstring_name(release_name)?;
    loop {
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                fault_directory.as_raw_fd(),
                release_name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
        std::thread::yield_now();
    }
}

#[cfg(not(all(unix, debug_assertions)))]
fn pause_at_test_fault(_point: CommitPoint) {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalLockOutcome {
    Acquired,
    Contended,
}

#[derive(Debug)]
pub(crate) enum TransactionError {
    InvalidJournal,
    Filesystem(&'static str),
    Destination(&'static str),
    RemovalSyncUncertain(&'static str),
    Interrupted,
    RequestLock,
    Active,
    OverBudget,
}

impl TransactionError {
    fn filesystem(operation: &'static str, _error: io::Error) -> Self {
        Self::Filesystem(operation)
    }
}

impl fmt::Display for TransactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJournal => {
                formatter.write_str("permission transaction journal is invalid")
            }
            Self::Filesystem(operation) => {
                write!(
                    formatter,
                    "permission transaction filesystem operation failed: {operation}"
                )
            }
            Self::Destination(operation) => {
                write!(
                    formatter,
                    "permission transaction destination failed: {operation}"
                )
            }
            Self::RemovalSyncUncertain(operation) => {
                write!(
                    formatter,
                    "permission transaction journal removal sync is uncertain: {operation}"
                )
            }
            Self::Interrupted => formatter.write_str("permission transaction commit interrupted"),
            Self::RequestLock => formatter.write_str("permission request lock is unavailable"),
            Self::Active => formatter.write_str("permission transaction storage is active"),
            Self::OverBudget => {
                formatter.write_str("permission transaction evidence is over budget")
            }
        }
    }
}

impl std::error::Error for TransactionError {}

struct RecoveryEvidence<'a> {
    budget: Option<&'a mut LiveEvidenceBudget>,
    budget_limit: Option<usize>,
    over_budget_detail: Option<OverBudgetDetail>,
}

impl RecoveryEvidence<'_> {
    fn unlimited() -> Self {
        Self {
            budget: None,
            budget_limit: None,
            over_budget_detail: None,
        }
    }

    fn bounded(budget: &mut LiveEvidenceBudget, limit: usize) -> RecoveryEvidence<'_> {
        RecoveryEvidence {
            budget: Some(budget),
            budget_limit: Some(limit),
            over_budget_detail: None,
        }
    }

    fn record_over_budget<T>(
        &mut self,
        result: Result<T, TransactionError>,
        source: OverBudgetSource,
    ) -> Result<T, TransactionError> {
        if matches!(result, Err(TransactionError::OverBudget)) && self.over_budget_detail.is_none()
        {
            self.over_budget_detail = Some(OverBudgetDetail {
                source,
                limit: self
                    .budget_limit
                    .expect("bounded evidence has a configured limit"),
            });
        }
        result
    }

    fn read_activity(&mut self, store: &ActivityStore) -> Result<ActivityLog, TransactionError> {
        let result = match self.budget.as_deref_mut() {
            Some(budget) => store.read_bounded(budget).map_err(|error| match error {
                ActivityStoreError::OverBudget => TransactionError::OverBudget,
                _ => TransactionError::Destination("read activity evidence"),
            }),
            None => store
                .read()
                .map_err(|_| TransactionError::Destination("read activity evidence")),
        };
        self.record_over_budget(result, OverBudgetSource::ActivityEvidence)
    }

    fn read_decisions(&mut self, path: &Path) -> Result<Vec<u8>, TransactionError> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(TransactionError::Destination("read decision evidence")),
        };
        let result = match self.budget.as_deref_mut() {
            Some(budget) => (|| {
                if usize::try_from(
                    file.metadata()
                        .map_err(|_| TransactionError::Destination("read decision evidence"))?
                        .len(),
                )
                .map_or(true, |length| length > budget.remaining())
                {
                    return Err(TransactionError::OverBudget);
                }
                let limit = u64::try_from(budget.remaining()).unwrap_or(u64::MAX);
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take(limit.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|_| TransactionError::Destination("read decision evidence"))?;
                if bytes.len() > budget.remaining() {
                    return Err(TransactionError::OverBudget);
                }
                budget
                    .charge(bytes.len())
                    .map_err(|_| TransactionError::OverBudget)?;
                Ok(bytes)
            })(),
            None => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|_| TransactionError::Destination("read decision evidence"))?;
                Ok(bytes)
            }
        };
        self.record_over_budget(result, OverBudgetSource::DecisionEvidence)
    }

    fn ensure_proposal(
        &mut self,
        path: &Path,
        record: &HookDecisionRecord,
    ) -> Result<EnsureRecord, TransactionError> {
        let result = match self.budget.as_deref_mut() {
            Some(budget) => {
                ensure_hook_record_at_bounded(path, record, budget).map_err(|error| match error {
                    DecisionStoreError::OverBudget => TransactionError::OverBudget,
                    DecisionStoreError::Io(_) => TransactionError::Destination("persist proposal"),
                })
            }
            None => ensure_hook_record_at(path, record)
                .map_err(|_| TransactionError::Destination("persist proposal")),
        };
        self.record_over_budget(result, OverBudgetSource::DecisionEvidence)
    }

    fn ensure_terminal(
        &mut self,
        store: &ActivityStore,
        event: ActivityEvent,
    ) -> Result<EnsureRecord, TransactionError> {
        let result = match self.budget.as_deref_mut() {
            Some(budget) => store.ensure_terminal_bounded(event, budget),
            None => store.ensure_terminal(event),
        };
        let result = result.map_err(|error| match error {
            ActivityStoreError::OverBudget => TransactionError::OverBudget,
            _ => TransactionError::Destination("persist terminal activity"),
        });
        self.record_over_budget(result, OverBudgetSource::ActivityEvidence)
    }
}

struct LockedFile {
    file: File,
}

impl LockedFile {
    fn lock(file: File) -> io::Result<Self> {
        FileExt::lock_exclusive(&file)?;
        Ok(Self { file })
    }

    fn try_lock(file: File) -> io::Result<Option<Self>> {
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Deref for LockedFile {
    type Target = File;

    fn deref(&self) -> &Self::Target {
        &self.file
    }
}

impl DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.file
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

struct DirectoryAdmissionLock<'a> {
    file: &'a File,
}

impl<'a> DirectoryAdmissionLock<'a> {
    fn lock_exclusive(file: &'a File) -> io::Result<Self> {
        FileExt::lock_exclusive(file)?;
        Ok(Self { file })
    }

    fn try_lock_shared(file: &'a File) -> io::Result<Option<Self>> {
        match FileExt::try_lock_shared(file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn try_lock_exclusive(file: &'a File) -> io::Result<Option<Self>> {
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error),
        }
    }
}

impl Drop for DirectoryAdmissionLock<'_> {
    fn drop(&mut self) {
        let _ = FileExt::unlock(self.file);
    }
}

struct TransactionDirectory {
    file: File,
    path: PathBuf,
}

impl TransactionDirectory {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
        })
    }
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    mode: u32,
    uid: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
    len: u64,
}

impl EntryMetadata {
    fn len(self) -> u64 {
        self.len
    }
}

pub(crate) struct PermissionTransactionStore {
    state_root: PathBuf,
    directory: PathBuf,
}

impl PermissionTransactionStore {
    pub(crate) fn at(state_root: &Path) -> Self {
        Self {
            state_root: state_root.to_owned(),
            directory: state_root.join("brain/permission-transactions"),
        }
    }

    pub(crate) fn prepare(
        &self,
        journal: PermissionTransactionJournal,
    ) -> Result<PreparedTransaction, TransactionError> {
        self.prepare_with_lock(journal, LockAcquisition::Blocking)
    }

    pub(crate) fn prepare_live(
        &self,
        journal: PermissionTransactionJournal,
    ) -> Result<PreparedTransaction, TransactionError> {
        self.prepare_with_lock(journal, LockAcquisition::Nonblocking)
    }

    fn prepare_with_lock(
        &self,
        journal: PermissionTransactionJournal,
        lock_acquisition: LockAcquisition,
    ) -> Result<PreparedTransaction, TransactionError> {
        validate_journal(&journal)?;
        let serialized =
            serde_json::to_vec(&journal).map_err(|_| TransactionError::InvalidJournal)?;
        if serialized.is_empty() || serialized.len() > MAX_JOURNAL_BYTES {
            return Err(TransactionError::InvalidJournal);
        }
        if decode_exact_journal(&serialized).as_ref() != Some(&journal) {
            return Err(TransactionError::InvalidJournal);
        }
        let directory = self
            .open_transaction_directory(true)?
            .ok_or(TransactionError::Filesystem("create journal directory"))?;

        let directory_lock = match lock_acquisition {
            LockAcquisition::Blocking => DirectoryAdmissionLock::lock_exclusive(&directory.file)
                .map_err(|error| TransactionError::filesystem("lock journal directory", error))?,
            LockAcquisition::Nonblocking => {
                DirectoryAdmissionLock::try_lock_exclusive(&directory.file)
                    .map_err(|error| TransactionError::filesystem("lock journal directory", error))?
                    .ok_or(TransactionError::Active)?
            }
        };
        let (temporary_name, final_name, file) = create_unique_temporary(&directory)?;
        let final_path = self.directory.join(&final_name);
        let mut file = LockedFile::lock(file)
            .map_err(|error| TransactionError::filesystem("lock journal temporary", error))?;
        set_file_mode(&file)
            .map_err(|error| TransactionError::filesystem("set journal mode", error))?;
        drop(directory_lock);
        write_and_sync_locked_file(&mut file, &serialized)?;

        let path_metadata = entry_metadata_at(&directory, &temporary_name)
            .map_err(|error| TransactionError::filesystem("inspect journal temporary", error))?;
        let opened_metadata = file
            .metadata()
            .map_err(|error| TransactionError::filesystem("inspect open journal", error))?;
        if !valid_opened_journal_file(&path_metadata, &opened_metadata)
            || path_metadata.len() != serialized.len() as u64
            || opened_metadata.len() != serialized.len() as u64
        {
            return Err(TransactionError::InvalidJournal);
        }
        match entry_metadata_at(&directory, &final_name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => return Err(TransactionError::Filesystem("reserve final journal name")),
            Err(error) => {
                return Err(TransactionError::filesystem(
                    "inspect final journal name",
                    error,
                ));
            }
        }

        publish_journal(&directory, &temporary_name, &final_name)
            .map_err(|error| TransactionError::filesystem("publish journal", error))?;
        if !entry_matches_open_file(&directory, &final_name, &file) {
            return Err(TransactionError::InvalidJournal);
        }
        directory
            .file
            .sync_all()
            .map_err(|error| TransactionError::filesystem("sync journal directory", error))?;

        Ok(PreparedTransaction {
            path: final_path,
            name: final_name,
            directory,
            file,
            journal,
        })
    }

    pub(crate) fn discover(
        &self,
        limits: RecoveryLimits,
    ) -> Result<(Vec<RecoverableTransaction>, RecoveryReport), TransactionError> {
        self.discover_with_lock_hook(limits, &mut |_, _| {})
    }

    pub(crate) fn preflight_live(&self) -> Result<RecoveryReport, TransactionError> {
        drop(
            self.open_transaction_directory(true)?
                .ok_or(TransactionError::Filesystem("create journal directory"))?,
        );
        let (transactions, report) = self.discover(RecoveryLimits::live())?;
        drop(transactions);
        Ok(report)
    }

    fn discover_with_lock_hook<F>(
        &self,
        limits: RecoveryLimits,
        lock_hook: &mut F,
    ) -> Result<(Vec<RecoverableTransaction>, RecoveryReport), TransactionError>
    where
        F: FnMut(&OsStr, JournalLockOutcome),
    {
        let Some(directory) = self.open_transaction_directory(false)? else {
            return Ok((Vec::new(), RecoveryReport::default()));
        };
        let directory_lock = match DirectoryAdmissionLock::try_lock_shared(&directory.file)
            .map_err(|error| TransactionError::filesystem("lock journal directory", error))?
        {
            Some(lock) => lock,
            None => {
                return Ok((
                    Vec::new(),
                    RecoveryReport {
                        active: 1,
                        ..RecoveryReport::default()
                    },
                ));
            }
        };
        let entries = self.scan_entries(&directory, limits)?;
        self.discover_entries_with_lock_hook(&directory, entries, limits, directory_lock, lock_hook)
    }

    fn scan_entries(
        &self,
        directory: &TransactionDirectory,
        limits: RecoveryLimits,
    ) -> Result<Vec<DiscoveredEntry>, TransactionError> {
        let max_journals = limits.max_journals.min(DEFAULT_MAX_JOURNALS);
        let mut entries = Vec::new();
        let names = read_directory_names(directory, max_journals.saturating_add(1))
            .map_err(|error| TransactionError::filesystem("scan journal directory", error))?;
        for name in names {
            let metadata = entry_metadata_at(directory, &name)
                .map_err(|error| TransactionError::filesystem("inspect journal entry", error))?;
            entries.push(DiscoveredEntry {
                path: self.directory.join(&name),
                name,
                metadata,
            });
        }
        Ok(entries)
    }

    fn discover_entries(
        &self,
        directory: &TransactionDirectory,
        entries: Vec<DiscoveredEntry>,
        limits: RecoveryLimits,
        directory_lock: DirectoryAdmissionLock<'_>,
    ) -> Result<(Vec<RecoverableTransaction>, RecoveryReport), TransactionError> {
        self.discover_entries_with_lock_hook(
            directory,
            entries,
            limits,
            directory_lock,
            &mut |_, _| {},
        )
    }

    fn discover_entries_with_lock_hook<F>(
        &self,
        directory: &TransactionDirectory,
        mut entries: Vec<DiscoveredEntry>,
        limits: RecoveryLimits,
        directory_lock: DirectoryAdmissionLock<'_>,
        lock_hook: &mut F,
    ) -> Result<(Vec<RecoverableTransaction>, RecoveryReport), TransactionError>
    where
        F: FnMut(&OsStr, JournalLockOutcome),
    {
        let max_journals = limits.max_journals.min(DEFAULT_MAX_JOURNALS);
        if entries.len() > max_journals {
            return Ok(over_budget_discovery(
                OverBudgetSource::JournalCount,
                max_journals,
            ));
        }
        let max_total_bytes = limits.max_total_bytes.min(DEFAULT_MAX_TOTAL_BYTES);
        let mut total_bytes = 0usize;
        for entry in &entries {
            if !add_to_total(&mut total_bytes, entry.metadata.len(), max_total_bytes) {
                return Ok(over_budget_discovery(
                    OverBudgetSource::JournalBytes,
                    max_total_bytes,
                ));
            }
        }

        entries.sort_by(|left, right| left.name.cmp(&right.name));
        let mut report = RecoveryReport::default();
        let mut preflight = Vec::with_capacity(entries.len());
        let mut retained_unlocked = Vec::new();
        let mut current_total = 0usize;
        let mut unstable = false;
        let mut abort_before_decode = false;
        for entry in entries {
            let Some(kind) = generated_file_kind(&entry.name) else {
                report.invalid += 1;
                abort_before_decode = true;
                match open_entry_file(directory, &entry.name) {
                    Ok(file) => {
                        let Ok(metadata) = file.metadata() else {
                            unstable = true;
                            continue;
                        };
                        if !entry_matches_metadata(&entry.metadata, &metadata)
                            || entry.metadata.len() != metadata.len()
                        {
                            unstable = true;
                        }
                        if !add_to_total(&mut current_total, metadata.len(), max_total_bytes) {
                            return Ok(over_budget_discovery(
                                OverBudgetSource::JournalBytes,
                                max_total_bytes,
                            ));
                        }
                        retained_unlocked.push(file);
                    }
                    Err(_) => {
                        let Ok(metadata) = entry_metadata_at(directory, &entry.name) else {
                            unstable = true;
                            continue;
                        };
                        if !same_entry_identity(&entry.metadata, &metadata)
                            || entry.metadata.len() != metadata.len()
                        {
                            unstable = true;
                        }
                        if !add_to_total(&mut current_total, metadata.len(), max_total_bytes) {
                            return Ok(over_budget_discovery(
                                OverBudgetSource::JournalBytes,
                                max_total_bytes,
                            ));
                        }
                    }
                }
                continue;
            };
            if !valid_path_metadata(&entry.metadata) {
                report.invalid += 1;
                abort_before_decode = true;
                let Ok(metadata) = entry_metadata_at(directory, &entry.name) else {
                    unstable = true;
                    continue;
                };
                if !same_entry_identity(&entry.metadata, &metadata)
                    || entry.metadata.len() != metadata.len()
                {
                    unstable = true;
                }
                if !add_to_total(&mut current_total, metadata.len(), max_total_bytes) {
                    return Ok(over_budget_discovery(
                        OverBudgetSource::JournalBytes,
                        max_total_bytes,
                    ));
                }
                continue;
            }

            let file = match open_journal_file(directory, &entry.name) {
                Ok(file) => file,
                Err(_) => {
                    report.invalid += 1;
                    abort_before_decode = true;
                    match entry_metadata_at(directory, &entry.name) {
                        Ok(metadata) => {
                            if !same_entry_identity(&entry.metadata, &metadata)
                                || entry.metadata.len() != metadata.len()
                            {
                                unstable = true;
                            }
                            if !add_to_total(&mut current_total, metadata.len(), max_total_bytes) {
                                return Ok(over_budget_discovery(
                                    OverBudgetSource::JournalBytes,
                                    max_total_bytes,
                                ));
                            }
                        }
                        Err(_) => unstable = true,
                    }
                    continue;
                }
            };
            let opened_metadata = match file.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    report.invalid += 1;
                    unstable = true;
                    continue;
                }
            };
            if !add_to_total(&mut current_total, opened_metadata.len(), max_total_bytes) {
                return Ok(over_budget_discovery(
                    OverBudgetSource::JournalBytes,
                    max_total_bytes,
                ));
            }
            if !valid_opened_journal_file(&entry.metadata, &opened_metadata) {
                unstable = true;
            }
            match LockedFile::try_lock(file)
                .map_err(|error| TransactionError::filesystem("lock journal entry", error))?
            {
                Some(file) => {
                    lock_hook(&entry.name, JournalLockOutcome::Acquired);
                    let current_path_metadata = entry_metadata_at(directory, &entry.name).ok();
                    let current_opened_metadata = file.metadata().ok();
                    if !matches!(
                        (current_path_metadata.as_ref(), current_opened_metadata.as_ref()),
                        (Some(path), Some(opened))
                            if valid_opened_journal_file(path, opened)
                                && same_entry_identity(&entry.metadata, path)
                                && entry.metadata.len() == path.len()
                                && opened_metadata.len() == opened.len()
                    ) {
                        unstable = true;
                    }
                    preflight.push(PreflightEntry { entry, kind, file });
                }
                None => {
                    lock_hook(&entry.name, JournalLockOutcome::Contended);
                    report.active += 1;
                    if unstable {
                        report.invalid = report.invalid.max(1);
                    }
                    return Ok((Vec::new(), report));
                }
            }
        }
        if unstable {
            report.invalid = report.invalid.max(1);
        }
        if unstable || abort_before_decode {
            return Ok((Vec::new(), report));
        }
        drop(directory_lock);
        drop(retained_unlocked);

        let mut recoverable = Vec::new();
        for mut locked in preflight {
            let entry = locked.entry;
            let kind = locked.kind;
            let file = &mut locked.file;
            let Some(journal) = read_and_validate_journal(file) else {
                report.invalid += 1;
                continue;
            };

            match kind {
                GeneratedFileKind::Temporary => {
                    report.pending += 1;
                    recoverable.push(RecoverableTransaction {
                        path: entry.path,
                        name: entry.name,
                        directory: directory.try_clone().map_err(|error| {
                            TransactionError::filesystem("retain journal directory", error)
                        })?,
                        file: locked.file,
                        journal,
                        kind,
                    });
                }
                GeneratedFileKind::Final => {
                    report.pending += 1;
                    recoverable.push(RecoverableTransaction {
                        path: entry.path,
                        name: entry.name,
                        directory: directory.try_clone().map_err(|error| {
                            TransactionError::filesystem("retain journal directory", error)
                        })?,
                        file: locked.file,
                        journal,
                        kind,
                    });
                }
            }
        }

        Ok((recoverable, report))
    }

    fn open_transaction_directory(
        &self,
        create: bool,
    ) -> Result<Option<TransactionDirectory>, TransactionError> {
        open_transaction_directory(&self.state_root, &self.directory, create)
    }
}

fn write_and_sync_locked_file(
    file: &mut LockedFile,
    serialized: &[u8],
) -> Result<(), TransactionError> {
    file.write_all(serialized)
        .map_err(|error| TransactionError::filesystem("write journal temporary", error))?;
    file.flush()
        .map_err(|error| TransactionError::filesystem("flush journal temporary", error))?;
    file.sync_all()
        .map_err(|error| TransactionError::filesystem("sync journal temporary", error))
}

fn over_budget_discovery(
    source: OverBudgetSource,
    limit: usize,
) -> (Vec<RecoverableTransaction>, RecoveryReport) {
    (
        Vec::new(),
        RecoveryReport {
            over_budget: 1,
            over_budget_detail: Some(OverBudgetDetail { source, limit }),
            ..RecoveryReport::default()
        },
    )
}

fn add_to_total(total: &mut usize, length: u64, maximum: usize) -> bool {
    let Ok(length) = usize::try_from(length) else {
        return false;
    };
    let Some(next) = total.checked_add(length) else {
        return false;
    };
    *total = next;
    next <= maximum
}

pub(crate) struct PreparedTransaction {
    path: PathBuf,
    name: OsString,
    directory: TransactionDirectory,
    file: LockedFile,
    journal: PermissionTransactionJournal,
}

impl PreparedTransaction {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn journal(&self) -> &PermissionTransactionJournal {
        &self.journal
    }

    pub(crate) fn complete(self) -> Result<(), TransactionError> {
        remove_locked_file(&self.directory, &self.name, &self.file)
    }

    fn complete_with_directory_sync<S>(self, sync_directory: &mut S) -> Result<(), TransactionError>
    where
        S: FnMut(&File) -> io::Result<()>,
    {
        remove_locked_file_with_directory_sync(
            &self.directory,
            &self.name,
            &self.file,
            sync_directory,
        )
    }
}

impl fmt::Debug for PreparedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedTransaction")
            .finish_non_exhaustive()
    }
}

pub(crate) struct RecoverableTransaction {
    path: PathBuf,
    name: OsString,
    directory: TransactionDirectory,
    file: LockedFile,
    journal: PermissionTransactionJournal,
    kind: GeneratedFileKind,
}

impl RecoverableTransaction {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn journal(&self) -> &PermissionTransactionJournal {
        &self.journal
    }

    fn is_temporary(&self) -> bool {
        matches!(self.kind, GeneratedFileKind::Temporary)
    }

    pub(crate) fn complete(self) -> Result<(), TransactionError> {
        remove_locked_file(&self.directory, &self.name, &self.file)
    }

    fn complete_with_directory_sync<S>(self, sync_directory: &mut S) -> Result<(), TransactionError>
    where
        S: FnMut(&File) -> io::Result<()>,
    {
        remove_locked_file_with_directory_sync(
            &self.directory,
            &self.name,
            &self.file,
            sync_directory,
        )
    }
}

impl fmt::Debug for RecoverableTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoverableTransaction")
            .finish_non_exhaustive()
    }
}

pub(crate) fn commit(
    prepared: PreparedTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
) -> Result<CommitReport, TransactionError> {
    let mut sync_directory = |directory: &File| directory.sync_all();
    let mut evidence = RecoveryEvidence::unlimited();
    commit_impl(
        prepared,
        lifecycle_store,
        activity_store,
        decisions_path,
        &mut evidence,
        &mut |_| Ok(()),
        &mut sync_directory,
    )
}

pub(crate) fn commit_live(
    prepared: PreparedTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
    budget: &mut LiveEvidenceBudget,
) -> Result<CommitReport, TransactionError> {
    let mut sync_directory = |directory: &File| directory.sync_all();
    let mut evidence = RecoveryEvidence::bounded(budget, LIVE_MAX_DESTINATION_BYTES);
    commit_impl(
        prepared,
        lifecycle_store,
        activity_store,
        decisions_path,
        &mut evidence,
        &mut |_| Ok(()),
        &mut sync_directory,
    )
}

#[cfg(test)]
fn commit_with_fault(
    prepared: PreparedTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
    fault: CommitFault,
) -> Result<CommitReport, TransactionError> {
    let mut sync_directory = |directory: &File| {
        if fault == CommitFault::AfterUnlinkDirectorySyncFailure {
            Err(io::Error::other("injected directory sync failure"))
        } else {
            directory.sync_all()
        }
    };
    let mut evidence = RecoveryEvidence::unlimited();
    commit_impl(
        prepared,
        lifecycle_store,
        activity_store,
        decisions_path,
        &mut evidence,
        &mut |point| {
            let injected = matches!(
                (fault, point),
                (CommitFault::AfterPrepare, CommitPoint::AfterPrepare)
                    | (CommitFault::AfterProposal, CommitPoint::AfterProposal)
                    | (CommitFault::AfterLifecycle, CommitPoint::AfterLifecycle)
                    | (CommitFault::AfterTerminal, CommitPoint::AfterTerminal)
                    | (
                        CommitFault::BeforeJournalRemoval,
                        CommitPoint::BeforeJournalRemoval
                    )
            );
            if injected {
                Err(TransactionError::Interrupted)
            } else {
                Ok(())
            }
        },
        &mut sync_directory,
    )
}

fn commit_impl<F, S>(
    prepared: PreparedTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
    evidence: &mut RecoveryEvidence<'_>,
    fault: &mut F,
    sync_directory: &mut S,
) -> Result<CommitReport, TransactionError>
where
    F: FnMut(CommitPoint) -> Result<(), TransactionError>,
    S: FnMut(&File) -> io::Result<()>,
{
    let journal = prepared.journal().clone();
    fault(CommitPoint::AfterPrepare)?;
    pause_at_test_fault(CommitPoint::AfterPrepare);
    if let Err(error) = decision_evidence_is_readable(decisions_path, evidence) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        if journal.allow_requires_lifecycle_authority {
            let _ = lifecycle_store.ensure_permission_decision(
                &journal.lifecycle_identity,
                &journal.request_key,
                PermissionDecision::NeedsInput,
            );
        }
        return Err(TransactionError::Destination(
            "read existing proposal evidence",
        ));
    }
    if let Err(error) = evidence.ensure_proposal(decisions_path, &journal.proposal) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            TransactionError::Destination("persist proposal"),
        ));
    }
    fault(CommitPoint::AfterProposal)?;
    pause_at_test_fault(CommitPoint::AfterProposal);

    if let Err(error) = activity_evidence_is_readable(activity_store, evidence) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        if journal.allow_requires_lifecycle_authority {
            let _ = lifecycle_store.ensure_permission_decision(
                &journal.lifecycle_identity,
                &journal.request_key,
                PermissionDecision::NeedsInput,
            );
        }
        return Err(TransactionError::Destination(
            "read existing terminal evidence",
        ));
    }

    let expected = expected_decision(&journal);
    if lifecycle_store
        .ensure_permission_decision(
            &journal.lifecycle_identity,
            &journal.request_key,
            expected.clone(),
        )
        .is_err()
    {
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            TransactionError::Destination("persist lifecycle disposition"),
        ));
    }
    fault(CommitPoint::AfterLifecycle)?;

    if lifecycle_store
        .permission_decision(&journal.lifecycle_identity, &journal.request_key)
        .ok()
        .flatten()
        != Some(expected.clone())
    {
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            TransactionError::Destination("verify executable lifecycle authority"),
        ));
    }

    if let Err(error) = evidence.ensure_terminal(activity_store, journal.terminal.clone()) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            TransactionError::Destination("persist terminal activity"),
        ));
    }
    fault(CommitPoint::AfterTerminal)?;
    if let Err(error) = verify_destinations(
        decisions_path,
        lifecycle_store,
        activity_store,
        &journal,
        &journal.terminal,
        &expected,
        evidence,
    ) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            error,
        ));
    }
    fault(CommitPoint::BeforeJournalRemoval)?;
    if let Err(error) = prepared.complete_with_directory_sync(sync_directory) {
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            error,
        ));
    }
    Ok(CommitReport)
}

pub(crate) fn recover_pending(
    state_root: &Path,
    limits: RecoveryLimits,
) -> Result<RecoveryReport, TransactionError> {
    recover_pending_impl(state_root, limits, None, &mut |directory: &File| {
        directory.sync_all()
    })
}

pub(crate) fn recover_pending_with_guard(
    state_root: &Path,
    limits: RecoveryLimits,
    guard: &PermissionRequestGuard,
) -> Result<RecoveryReport, TransactionError> {
    recover_pending_impl(state_root, limits, Some(guard), &mut |directory: &File| {
        directory.sync_all()
    })
}

#[cfg(test)]
fn recover_pending_with_directory_sync_failure(
    state_root: &Path,
    limits: RecoveryLimits,
) -> Result<RecoveryReport, TransactionError> {
    recover_pending_impl(state_root, limits, None, &mut |_directory: &File| {
        Err(io::Error::other("injected directory sync failure"))
    })
}

fn recover_pending_impl<S>(
    state_root: &Path,
    limits: RecoveryLimits,
    existing_guard: Option<&PermissionRequestGuard>,
    sync_directory: &mut S,
) -> Result<RecoveryReport, TransactionError>
where
    S: FnMut(&File) -> io::Result<()>,
{
    let request_locks = PermissionRequestLockStore::at(state_root);
    request_locks
        .validate()
        .map_err(|_| TransactionError::RequestLock)?;
    let store = PermissionTransactionStore::at(state_root);
    let (transactions, mut report) = store.discover(limits)?;
    let lifecycle_store = LifecycleStore::at(state_root);
    let activity_store = ActivityStore::at(state_root.join("activity.jsonl"));
    let decisions_path = state_root.join("brain/decisions.jsonl");
    let mut live_budget = (limits.max_destination_bytes != usize::MAX)
        .then(|| LiveEvidenceBudget::new(limits.max_destination_bytes));
    report.pending = 0;

    for transaction in transactions {
        let journal = transaction.journal();
        let acquired;
        let guard = if let Some(guard) = existing_guard {
            if !guard.matches(&journal.lifecycle_identity, &journal.request_key) {
                report.invalid += 1;
                continue;
            }
            guard
        } else {
            acquired = request_locks
                .try_acquire(&journal.lifecycle_identity, &journal.request_key)
                .map_err(|_| TransactionError::RequestLock)?;
            let Some(guard) = acquired.as_ref() else {
                report.active += 1;
                continue;
            };
            guard
        };
        debug_assert!(guard.matches(&journal.lifecycle_identity, &journal.request_key));
        if transaction.is_temporary() {
            match transaction.complete_with_directory_sync(sync_directory) {
                Ok(()) => {}
                Err(TransactionError::RemovalSyncUncertain(_)) => {
                    report.removal_sync_uncertain += 1;
                }
                Err(_) => report.invalid += 1,
            }
            continue;
        }
        let mut evidence = match live_budget.as_mut() {
            Some(budget) => RecoveryEvidence::bounded(budget, limits.max_destination_bytes),
            None => RecoveryEvidence::unlimited(),
        };
        let disposition = recover_transaction(
            transaction,
            &lifecycle_store,
            &activity_store,
            &decisions_path,
            &mut evidence,
            sync_directory,
        );
        if disposition == RecoveryDisposition::OverBudget && report.over_budget_detail.is_none() {
            report.over_budget_detail = evidence.over_budget_detail;
        }
        match disposition {
            RecoveryDisposition::Completed => report.completed += 1,
            RecoveryDisposition::Active => report.active += 1,
            RecoveryDisposition::OverBudget => report.over_budget += 1,
            RecoveryDisposition::Invalid => report.invalid += 1,
            RecoveryDisposition::RemovalSyncUncertain => report.removal_sync_uncertain += 1,
            RecoveryDisposition::Unresolved => report.pending += 1,
        }
    }
    Ok(report)
}

fn recover_transaction(
    transaction: RecoverableTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
    evidence: &mut RecoveryEvidence<'_>,
    sync_directory: &mut impl FnMut(&File) -> io::Result<()>,
) -> RecoveryDisposition {
    match recover_transaction_result(
        transaction,
        lifecycle_store,
        activity_store,
        decisions_path,
        evidence,
        sync_directory,
    ) {
        Ok(()) => RecoveryDisposition::Completed,
        Err(TransactionError::RemovalSyncUncertain(_)) => RecoveryDisposition::RemovalSyncUncertain,
        Err(TransactionError::OverBudget) => RecoveryDisposition::OverBudget,
        Err(_) => RecoveryDisposition::Unresolved,
    }
}

fn recover_transaction_result(
    transaction: RecoverableTransaction,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    decisions_path: &Path,
    evidence: &mut RecoveryEvidence<'_>,
    sync_directory: &mut impl FnMut(&File) -> io::Result<()>,
) -> Result<(), TransactionError> {
    let journal = transaction.journal().clone();
    if let Err(error) = decision_evidence_is_readable(decisions_path, evidence) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        if journal.allow_requires_lifecycle_authority {
            let _ = lifecycle_store.ensure_permission_decision(
                &journal.lifecycle_identity,
                &journal.request_key,
                PermissionDecision::NeedsInput,
            );
        }
        return Err(TransactionError::Destination(
            "read recovery proposal evidence",
        ));
    }
    if let Err(error) = evidence.ensure_proposal(decisions_path, &journal.proposal) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            TransactionError::Destination("recover proposal"),
        ));
    }

    if let Err(error) = activity_evidence_is_readable(activity_store, evidence) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        if journal.allow_requires_lifecycle_authority {
            let _ = lifecycle_store.ensure_permission_decision(
                &journal.lifecycle_identity,
                &journal.request_key,
                PermissionDecision::NeedsInput,
            );
        }
        return Err(TransactionError::Destination(
            "read recovery terminal evidence",
        ));
    }

    let (terminal, decision) = if journal.allow_requires_lifecycle_authority {
        match recover_allow_destinations(lifecycle_store, activity_store, &journal, evidence) {
            Ok(destinations) => destinations,
            Err(error) => {
                if matches!(error, TransactionError::OverBudget) {
                    return Err(error);
                }
                return Err(compensate_allow_failure(
                    lifecycle_store,
                    activity_store,
                    &journal,
                    evidence,
                    error,
                ));
            }
        }
    } else {
        let expected = expected_decision(&journal);
        lifecycle_store
            .ensure_permission_decision(
                &journal.lifecycle_identity,
                &journal.request_key,
                expected.clone(),
            )
            .map_err(|_| TransactionError::Destination("recover lifecycle disposition"))?;
        evidence.ensure_terminal(activity_store, journal.terminal.clone())?;
        (journal.terminal.clone(), expected)
    };

    if let Err(error) = verify_destinations(
        decisions_path,
        lifecycle_store,
        activity_store,
        &journal,
        &terminal,
        &decision,
        evidence,
    ) {
        if matches!(error, TransactionError::OverBudget) {
            return Err(error);
        }
        return Err(compensate_allow_failure(
            lifecycle_store,
            activity_store,
            &journal,
            evidence,
            error,
        ));
    }
    transaction
        .complete_with_directory_sync(sync_directory)
        .map_err(|error| {
            compensate_allow_failure(lifecycle_store, activity_store, &journal, evidence, error)
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AllowTerminalEvidence {
    Absent,
    Allowed,
    Error,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthorityEvidence {
    Absent,
    Decided,
    NeedsInput,
    Unreadable,
}

fn recover_allow_destinations(
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    journal: &PermissionTransactionJournal,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<(ActivityEvent, PermissionDecision), TransactionError> {
    let error = recovery_authority_error(journal);
    match allow_terminal_evidence(activity_store, journal, &error, evidence)? {
        AllowTerminalEvidence::Error => {
            ensure_fail_closed_allow(lifecycle_store, activity_store, journal, evidence)?;
            Ok((error, PermissionDecision::NeedsInput))
        }
        AllowTerminalEvidence::Conflict => Err(TransactionError::Destination(
            "conflicting terminal activity evidence",
        )),
        AllowTerminalEvidence::Allowed => match authority_evidence(lifecycle_store, journal) {
            AuthorityEvidence::Decided => {
                Ok((journal.terminal.clone(), expected_decision(journal)))
            }
            AuthorityEvidence::Absent
            | AuthorityEvidence::NeedsInput
            | AuthorityEvidence::Unreadable => Err(TransactionError::Destination(
                "allowed activity lacks lifecycle authority",
            )),
        },
        AllowTerminalEvidence::Absent => match authority_evidence(lifecycle_store, journal) {
            AuthorityEvidence::Decided => {
                evidence.ensure_terminal(activity_store, journal.terminal.clone())?;
                Ok((journal.terminal.clone(), expected_decision(journal)))
            }
            AuthorityEvidence::Absent | AuthorityEvidence::NeedsInput => {
                ensure_fail_closed_allow(lifecycle_store, activity_store, journal, evidence)?;
                Ok((error, PermissionDecision::NeedsInput))
            }
            AuthorityEvidence::Unreadable => Err(TransactionError::Destination(
                "unreadable lifecycle authority",
            )),
        },
    }
}

fn authority_evidence(
    lifecycle_store: &LifecycleStore,
    journal: &PermissionTransactionJournal,
) -> AuthorityEvidence {
    match lifecycle_store.permission_decision(&journal.lifecycle_identity, &journal.request_key) {
        Ok(Some(decision)) if decision == expected_decision(journal) => AuthorityEvidence::Decided,
        Ok(Some(PermissionDecision::NeedsInput)) => AuthorityEvidence::NeedsInput,
        Ok(Some(PermissionDecision::Decided(_))) => AuthorityEvidence::Unreadable,
        Ok(None) => AuthorityEvidence::Absent,
        Err(_) => AuthorityEvidence::Unreadable,
    }
}

fn ensure_fail_closed_allow(
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    journal: &PermissionTransactionJournal,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<EnsurePermissionDecision, TransactionError> {
    let lifecycle = lifecycle_store.ensure_permission_decision(
        &journal.lifecycle_identity,
        &journal.request_key,
        PermissionDecision::NeedsInput,
    );
    let terminal = activity_evidence_is_readable(activity_store, evidence)
        .and_then(|()| evidence.ensure_terminal(activity_store, recovery_authority_error(journal)));
    let lifecycle =
        lifecycle.map_err(|_| TransactionError::Destination("record lifecycle compensation"))?;
    terminal?;
    Ok(lifecycle)
}

fn compensate_allow_failure(
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    journal: &PermissionTransactionJournal,
    evidence: &mut RecoveryEvidence<'_>,
    error: TransactionError,
) -> TransactionError {
    if journal.allow_requires_lifecycle_authority {
        let _ = ensure_fail_closed_allow(lifecycle_store, activity_store, journal, evidence);
    }
    error
}

fn recovery_authority_error(journal: &PermissionTransactionJournal) -> ActivityEvent {
    let mut error = journal.terminal.clone();
    error.state = ActivityState::Error;
    error.reasoning = Some(RECOVERY_AUTHORITY_ERROR_REASON.into());
    error
}

fn allow_terminal_evidence(
    activity_store: &ActivityStore,
    journal: &PermissionTransactionJournal,
    error: &ActivityEvent,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<AllowTerminalEvidence, TransactionError> {
    let log = evidence.read_activity(activity_store)?;
    let Some(terminal) = log.events().iter().find(|event| {
        event.activity_id == journal.terminal.activity_id && event.state.is_terminal()
    }) else {
        return Ok(AllowTerminalEvidence::Absent);
    };
    let terminal = serde_json::to_value(terminal)
        .map_err(|_| TransactionError::Destination("encode terminal activity"))?;
    let allowed = serde_json::to_value(&journal.terminal)
        .map_err(|_| TransactionError::Destination("encode expected allowed activity"))?;
    let error = serde_json::to_value(error)
        .map_err(|_| TransactionError::Destination("encode expected error activity"))?;
    Ok(if terminal == allowed {
        AllowTerminalEvidence::Allowed
    } else if terminal == error {
        AllowTerminalEvidence::Error
    } else {
        AllowTerminalEvidence::Conflict
    })
}

fn activity_evidence_is_readable(
    activity_store: &ActivityStore,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<(), TransactionError> {
    let log = evidence.read_activity(activity_store)?;
    let diagnostics = log.diagnostics();
    if diagnostics.malformed_rows > 0
        || diagnostics.duplicate_terminal_states > 0
        || diagnostics.truncated_tails > 0
        || diagnostics.discarded_tail_bytes > 0
    {
        return Err(TransactionError::Destination(
            "activity evidence is unreadable",
        ));
    }
    Ok(())
}

fn decision_evidence_is_readable(
    path: &Path,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<(), TransactionError> {
    let bytes = evidence.read_decisions(path)?;
    let mut reader = BufReader::new(bytes.as_slice());
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|_| TransactionError::Destination("read decision evidence"))?;
        if read == 0 {
            return Ok(());
        }
        if line.len() > MAX_JOURNAL_BYTES + 1 || line.pop() != Some(b'\n') || line.is_empty() {
            return Err(TransactionError::Destination(
                "decision evidence is unreadable",
            ));
        }
        let mut deserializer = serde_json::Deserializer::from_slice(&line);
        if UniqueJsonValue::deserialize(&mut deserializer).is_err() || deserializer.end().is_err() {
            return Err(TransactionError::Destination(
                "decision evidence is unreadable",
            ));
        }
    }
}

fn verify_destinations(
    decisions_path: &Path,
    lifecycle_store: &LifecycleStore,
    activity_store: &ActivityStore,
    journal: &PermissionTransactionJournal,
    terminal: &ActivityEvent,
    decision: &PermissionDecision,
    evidence: &mut RecoveryEvidence<'_>,
) -> Result<(), TransactionError> {
    decision_evidence_is_readable(decisions_path, evidence)?;
    if evidence.ensure_proposal(decisions_path, &journal.proposal)? != EnsureRecord::Present {
        return Err(TransactionError::Destination("reread proposal"));
    }
    if lifecycle_store
        .permission_decision(&journal.lifecycle_identity, &journal.request_key)
        .map_err(|_| TransactionError::Destination("verify lifecycle disposition"))?
        != Some(decision.clone())
    {
        return Err(TransactionError::Destination(
            "reread lifecycle disposition",
        ));
    }
    activity_evidence_is_readable(activity_store, evidence)?;
    if evidence.ensure_terminal(activity_store, terminal.clone())? != EnsureRecord::Present {
        return Err(TransactionError::Destination("reread terminal activity"));
    }
    Ok(())
}

fn expected_decision(journal: &PermissionTransactionJournal) -> PermissionDecision {
    match journal.terminal.state {
        ActivityState::Allowed => PermissionDecision::Decided(PermissionAuthority {
            transaction_id: journal.transaction_id.clone(),
            action: PermissionAction::Allow,
        }),
        ActivityState::Denied => PermissionDecision::Decided(PermissionAuthority {
            transaction_id: journal.transaction_id.clone(),
            action: PermissionAction::Deny,
        }),
        ActivityState::Abstained | ActivityState::Error => PermissionDecision::NeedsInput,
        _ => unreachable!("validated journal terminal state"),
    }
}

struct DiscoveredEntry {
    name: OsString,
    path: PathBuf,
    metadata: EntryMetadata,
}

struct PreflightEntry {
    entry: DiscoveredEntry,
    kind: GeneratedFileKind,
    file: LockedFile,
}

#[derive(Clone, Copy)]
enum GeneratedFileKind {
    Final,
    Temporary,
}

fn generated_file_kind(name: &OsStr) -> Option<GeneratedFileKind> {
    let name = name.to_str()?;
    if name
        .strip_prefix(FINAL_PREFIX)
        .and_then(|identity| identity.strip_suffix(FINAL_SUFFIX))
        .is_some_and(valid_creation_identity)
    {
        Some(GeneratedFileKind::Final)
    } else if name
        .strip_prefix(TEMP_PREFIX)
        .is_some_and(valid_creation_identity)
    {
        Some(GeneratedFileKind::Temporary)
    } else {
        None
    }
}

fn valid_creation_identity(identity: &str) -> bool {
    let mut parts = identity.split('-');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(nanos), Some(pid), Some(sequence), None)
            if nanos.len() == 39
                && pid.len() == 10
                && sequence.len() == 20
                && nanos.bytes().all(|byte| byte.is_ascii_digit())
                && pid.bytes().all(|byte| byte.is_ascii_digit())
                && sequence.bytes().all(|byte| byte.is_ascii_digit())
    )
}

pub(crate) fn validate_journal(
    journal: &PermissionTransactionJournal,
) -> Result<(), TransactionError> {
    if !matches!(journal.schema_version, 1 | JOURNAL_SCHEMA_VERSION)
        || !valid_id(&journal.transaction_id)
        || !validate_hook_decision_record(&journal.proposal)
        || !valid_id(&journal.terminal.activity_id)
        || journal.request_key.len() != 64
        || !journal
            .request_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || journal.terminal.schema_version != ACTIVITY_SCHEMA_VERSION
        || !journal.terminal.state.is_terminal()
        || !journal.terminal.has_consistent_payload()
        || journal.terminal.clone().normalized() != journal.terminal
        || serde_json::to_vec(&journal.terminal).map_or(true, |serialized| {
            serialized.len() > MAX_ACTIVITY_EVENT_BYTES
        })
        || serde_json::to_vec(&journal.proposal)
            .map_or(true, |serialized| serialized.len() > MAX_JOURNAL_BYTES)
    {
        return Err(TransactionError::InvalidJournal);
    }

    let Some(session) = journal.terminal.session.as_ref() else {
        return Err(TransactionError::InvalidJournal);
    };
    if journal.proposal.provider != journal.lifecycle_identity.provider()
        || session.provider != journal.lifecycle_identity.provider()
        || journal.proposal.session_id != journal.lifecycle_identity.session_id()
        || session.session_id != journal.lifecycle_identity.session_id()
        || session.provider_session_id.as_deref()
            != journal.lifecycle_identity.provider_session_id()
        || journal.proposal.turn_id != journal.lifecycle_identity.turn_id().unwrap_or_default()
        || session.turn_id.as_deref() != journal.lifecycle_identity.turn_id()
        || session.cwd != journal.lifecycle_identity.cwd()
        || journal.terminal.project.cwd != journal.lifecycle_identity.cwd()
        || session.project_id != journal.terminal.project.project_id
        || journal.terminal.project.label.as_deref() != Some(journal.proposal.project.as_str())
        || journal.terminal.tool.as_deref() != Some(journal.proposal.tool.as_str())
        || journal.terminal.decision_id.as_deref() != Some(journal.proposal.decision_id.as_str())
        || !action_matches_terminal(&journal.proposal.brain_action, journal.terminal.state)
        || journal.allow_requires_lifecycle_authority
            != (journal.terminal.state == ActivityState::Allowed)
        || !matches!(
            (journal.terminal.state, journal.disposition),
            (
                ActivityState::Allowed | ActivityState::Denied,
                PermissionDisposition::Decided
            ) | (
                ActivityState::Abstained | ActivityState::Error,
                PermissionDisposition::NeedsInput
            )
        )
    {
        return Err(TransactionError::InvalidJournal);
    }

    let rebuilt_identity = LifecycleIdentity::try_new_with_provider_session(
        journal.lifecycle_identity.provider(),
        journal.lifecycle_identity.session_id().to_owned(),
        journal
            .lifecycle_identity
            .provider_session_id()
            .map(str::to_owned),
        journal.lifecycle_identity.turn_id().map(str::to_owned),
        journal
            .lifecycle_identity
            .transcript_path()
            .map(Path::to_owned),
        journal.lifecycle_identity.cwd().to_owned(),
    )
    .map_err(|_| TransactionError::InvalidJournal)?;
    if rebuilt_identity != journal.lifecycle_identity {
        return Err(TransactionError::InvalidJournal);
    }

    Ok(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

fn action_matches_terminal(action: &str, state: ActivityState) -> bool {
    matches!(
        (action, state),
        ("approve", ActivityState::Allowed)
            | ("deny", ActivityState::Denied)
            | ("approve" | "deny" | "abstain", ActivityState::Abstained)
            | ("abstain", ActivityState::Error)
    )
}

#[cfg(unix)]
fn create_unique_temporary(
    directory: &TransactionDirectory,
) -> Result<(OsString, OsString, File), TransactionError> {
    for _ in 0..MAX_CREATE_ATTEMPTS {
        let identity = next_creation_identity();
        let temporary_name = OsString::from(format!("{TEMP_PREFIX}{identity}"));
        let final_name = OsString::from(format!("{FINAL_PREFIX}{identity}{FINAL_SUFFIX}"));
        match open_file_at(
            directory,
            &temporary_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        ) {
            Ok(file) => return Ok((temporary_name, final_name, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(TransactionError::filesystem(
                    "create journal temporary",
                    error,
                ));
            }
        }
    }
    Err(TransactionError::Filesystem("allocate unique journal name"))
}

#[cfg(not(unix))]
fn create_unique_temporary(
    _directory: &TransactionDirectory,
) -> Result<(OsString, OsString, File), TransactionError> {
    Err(TransactionError::Filesystem(
        "anchored journal creation unsupported",
    ))
}

fn next_creation_identity() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = JOURNAL_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:039}-{:010}-{sequence:020}", std::process::id())
}

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "android"))]
type RenameAt2 = unsafe extern "C" fn(
    libc::c_int,
    *const libc::c_char,
    libc::c_int,
    *const libc::c_char,
    libc::c_uint,
) -> libc::c_int;

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "android"))]
const _: RenameAt2 = libc::renameat2;

#[cfg(target_os = "linux")]
const RENAME_NOREPLACE_FLAG: libc::c_uint = libc::RENAME_NOREPLACE;

#[cfg(target_os = "android")]
const RENAME_NOREPLACE_FLAG: libc::c_uint = libc::RENAME_NOREPLACE as libc::c_uint;

#[cfg(target_os = "android")]
const _: () = assert!(libc::RENAME_NOREPLACE >= 0);

#[cfg(any(all(target_os = "linux", target_env = "gnu"), target_os = "android"))]
unsafe fn rename_noreplace_at(
    old_directory: libc::c_int,
    old_name: *const libc::c_char,
    new_directory: libc::c_int,
    new_name: *const libc::c_char,
) -> libc::c_int {
    unsafe {
        libc::renameat2(
            old_directory,
            old_name,
            new_directory,
            new_name,
            RENAME_NOREPLACE_FLAG,
        )
    }
}

#[cfg(all(target_os = "linux", target_env = "musl"))]
unsafe fn rename_noreplace_at(
    old_directory: libc::c_int,
    old_name: *const libc::c_char,
    new_directory: libc::c_int,
    new_name: *const libc::c_char,
) -> libc::c_int {
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            old_directory,
            old_name,
            new_directory,
            new_name,
            RENAME_NOREPLACE_FLAG,
        ) as libc::c_int
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish_journal(
    directory: &TransactionDirectory,
    temporary: &OsStr,
    final_name: &OsStr,
) -> io::Result<()> {
    let temporary = cstring_name(temporary)?;
    let final_name = cstring_name(final_name)?;
    let result = unsafe {
        rename_noreplace_at(
            directory.file.as_raw_fd(),
            temporary.as_ptr(),
            directory.file.as_raw_fd(),
            final_name.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn publish_journal(
    directory: &TransactionDirectory,
    temporary: &OsStr,
    final_name: &OsStr,
) -> io::Result<()> {
    let temporary = cstring_name(temporary)?;
    let final_name = cstring_name(final_name)?;
    let result = unsafe {
        libc::renameatx_np(
            directory.file.as_raw_fd(),
            temporary.as_ptr(),
            directory.file.as_raw_fd(),
            final_name.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn publish_journal(
    _directory: &TransactionDirectory,
    _temporary: &OsStr,
    _final_name: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored exclusive journal rename is unsupported",
    ))
}

fn read_and_validate_journal(file: &mut File) -> Option<PermissionTransactionJournal> {
    let metadata = file.metadata().ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_JOURNAL_BYTES as u64 {
        return None;
    }
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_JOURNAL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() > MAX_JOURNAL_BYTES {
        return None;
    }
    let journal = decode_exact_journal(&bytes)?;
    validate_journal(&journal).ok()?;
    Some(journal)
}

pub(crate) fn decode_exact_journal(bytes: &[u8]) -> Option<PermissionTransactionJournal> {
    let raw_numbers = serde_json::from_slice::<RawJournalNumbers<'_>>(bytes).ok()?;
    if !raw_numbers.are_lossless() {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJsonValue(encoded) = UniqueJsonValue::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    let journal: PermissionTransactionJournal = serde_json::from_value(encoded.clone()).ok()?;
    (serde_json::to_value(&journal).ok()? == encoded).then_some(journal)
}

pub(crate) fn decode_exact_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJsonValue(value) = UniqueJsonValue::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(value)
}

#[derive(Deserialize)]
struct RawJournalNumbers<'a> {
    #[serde(borrow)]
    proposal: RawProposalNumbers<'a>,
    #[serde(borrow)]
    terminal: Option<RawTerminalNumbers<'a>>,
}

#[derive(Deserialize)]
struct RawProposalNumbers<'a> {
    #[serde(borrow)]
    brain_confidence: &'a serde_json::value::RawValue,
    #[serde(borrow)]
    brain_threshold: Option<&'a serde_json::value::RawValue>,
}

pub(crate) fn hook_decision_numbers_are_lossless(bytes: &[u8]) -> bool {
    serde_json::from_slice::<RawProposalNumbers<'_>>(bytes)
        .is_ok_and(|numbers| numbers.are_lossless())
}

impl RawProposalNumbers<'_> {
    fn are_lossless(&self) -> bool {
        lossless_f64_token(self.brain_confidence)
            && self.brain_threshold.is_none_or(lossless_f64_token)
    }
}

#[derive(Deserialize)]
struct RawTerminalNumbers<'a> {
    #[serde(borrow)]
    confidence: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow)]
    threshold: Option<&'a serde_json::value::RawValue>,
}

impl RawJournalNumbers<'_> {
    fn are_lossless(&self) -> bool {
        self.proposal.are_lossless()
            && self.terminal.as_ref().is_none_or(|terminal| {
                terminal.confidence.is_none_or(lossless_f64_token)
                    && terminal.threshold.is_none_or(lossless_f64_token)
            })
    }
}

fn lossless_f64_token(token: &serde_json::value::RawValue) -> bool {
    let token = token.get();
    let Ok(value) = token.parse::<f64>() else {
        return false;
    };
    let Some(round_trip) = serde_json::Number::from_f64(value) else {
        return false;
    };
    normalized_decimal(token) == normalized_decimal(&round_trip.to_string())
}

fn normalized_decimal(value: &str) -> Option<(bool, String, i64)> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let exponent_index = unsigned.find(['e', 'E']);
    let (mantissa, explicit_exponent) = match exponent_index {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let fraction_len = i64::try_from(fraction.len()).ok()?;
    let mut digits = String::with_capacity(integer.len().checked_add(fraction.len())?);
    digits.push_str(integer);
    digits.push_str(fraction);
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let Some(first_nonzero) = digits.bytes().position(|byte| byte != b'0') else {
        return Some((false, String::new(), 0));
    };
    let last_nonzero = digits.bytes().rposition(|byte| byte != b'0')?;
    let trailing_zeroes = i64::try_from(digits.len().checked_sub(last_nonzero + 1)?).ok()?;
    let exponent = explicit_exponent
        .checked_sub(fraction_len)?
        .checked_add(trailing_zeroes)?;
    Some((
        negative,
        digits[first_nonzero..=last_nonzero].to_owned(),
        exponent,
    ))
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

#[cfg(unix)]
fn open_transaction_directory(
    state_root: &Path,
    display_path: &Path,
    create: bool,
) -> Result<Option<TransactionDirectory>, TransactionError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let state_root = state_root_for_traversal(state_root);
    let mut directory = open_directory(if state_root.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })
    .map_err(|error| TransactionError::filesystem("open journal hierarchy root", error))?;
    let mut sync_child = |child: &File| child.sync_all();
    let mut sync_parent = |parent: &File| parent.sync_all();
    for (component, private_component) in state_root
        .components()
        .map(|component| (component, false))
        .chain([(Component::Normal(OsStr::new("brain")), true)])
        .chain([(
            Component::Normal(OsStr::new("permission-transactions")),
            true,
        )])
    {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(TransactionError::Filesystem(
                    "validate journal directory path",
                ));
            }
        };
        directory = match open_or_create_directory_at_with_syncs(
            &directory,
            name,
            private_component,
            create,
            &mut sync_child,
            &mut sync_parent,
        ) {
            Ok(directory) => directory,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(TransactionError::filesystem(
                    "open journal directory component",
                    error,
                ));
            }
        };
    }

    let metadata = directory
        .metadata()
        .map_err(|error| TransactionError::filesystem("inspect journal directory", error))?;
    if !metadata.file_type().is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(TransactionError::Filesystem("validate journal directory"));
    }
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| TransactionError::filesystem("set journal directory mode", error))?;
    directory
        .sync_all()
        .map_err(|error| TransactionError::filesystem("sync journal directory", error))?;
    let metadata = directory
        .metadata()
        .map_err(|error| TransactionError::filesystem("reinspect journal directory", error))?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(TransactionError::Filesystem("validate journal directory"));
    }

    Ok(Some(TransactionDirectory {
        file: directory,
        path: display_path.to_owned(),
    }))
}

#[cfg(not(unix))]
fn open_transaction_directory(
    _state_root: &Path,
    _display_path: &Path,
    _create: bool,
) -> Result<Option<TransactionDirectory>, TransactionError> {
    Err(TransactionError::Filesystem(
        "anchored journal directories unsupported",
    ))
}

#[cfg(unix)]
fn open_or_create_directory_at_with_syncs<C, P>(
    parent: &File,
    name: &OsStr,
    private_component: bool,
    create: bool,
    sync_child: &mut C,
    sync_parent: &mut P,
) -> io::Result<File>
where
    C: FnMut(&File) -> io::Result<()>,
    P: FnMut(&File) -> io::Result<()>,
{
    let name = cstring_name(name)?;
    let created = if create {
        let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
        if result == 0 {
            true
        } else {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
            false
        }
    } else {
        false
    };
    let metadata = directory_metadata_at(parent, &name)?;
    let permissions = metadata.mode & 0o777;
    let (child, mode_corrected) = if created || private_component {
        open_private_directory_at(parent, &name, &metadata, created || permissions != 0o700)?
    } else if permissions & 0o077 == 0 {
        (
            open_owner_only_directory_at(parent, &name, &metadata, permissions)?,
            false,
        )
    } else {
        (open_directory_at(parent, &name)?, false)
    };
    if created || mode_corrected {
        sync_child(&child)?;
    }
    if created {
        sync_parent(parent)?;
    }
    Ok(child)
}

#[cfg(unix)]
fn open_private_directory_at(
    parent: &File,
    name: &CStr,
    metadata: &EntryMetadata,
    set_exact_mode: bool,
) -> io::Result<(File, bool)> {
    use std::os::unix::fs::MetadataExt;

    if set_exact_mode {
        let repair = open_directory_for_mode_repair_at(parent, name)?;
        let repair_metadata = repair.metadata()?;
        if !repair_metadata.file_type().is_dir()
            || repair_metadata.uid() != unsafe { libc::geteuid() }
            || !entry_matches_metadata(metadata, &repair_metadata)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "journal directory component changed before mode correction",
            ));
        }
        chmod_mode_repair_directory(&repair)?;
    }

    let corrected_metadata = directory_metadata_at(parent, name)?;
    if !same_entry_identity(metadata, &corrected_metadata)
        || corrected_metadata.uid != unsafe { libc::geteuid() }
        || corrected_metadata.mode & 0o777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "journal directory component changed during mode correction",
        ));
    }

    let child = open_directory_at(parent, name)?;
    let opened_metadata = child.metadata()?;
    if !valid_exact_private_directory_opened_metadata(&opened_metadata)
        || !entry_matches_metadata(&corrected_metadata, &opened_metadata)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "journal directory component changed during open",
        ));
    }
    Ok((child, set_exact_mode))
}

#[cfg(unix)]
fn open_owner_only_directory_at(
    parent: &File,
    name: &CStr,
    metadata: &EntryMetadata,
    expected_mode: u32,
) -> io::Result<File> {
    let child = open_directory_at(parent, name)?;
    let opened_metadata = child.metadata()?;
    if !valid_owner_only_directory_opened_metadata(&opened_metadata, expected_mode)
        || !entry_matches_metadata(metadata, &opened_metadata)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "journal directory component changed during open",
        ));
    }
    Ok(child)
}

#[cfg(unix)]
fn valid_exact_private_directory_opened_metadata(metadata: &Metadata) -> bool {
    valid_owner_only_directory_opened_metadata(metadata, 0o700)
}

#[cfg(unix)]
fn valid_owner_only_directory_opened_metadata(metadata: &Metadata, expected_mode: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.file_type().is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o077 == 0
        && metadata.mode() & 0o777 == expected_mode
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // libc stat field widths vary between Unix targets.
fn directory_metadata_at(parent: &File, name: &CStr) -> io::Result<EntryMetadata> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    Ok(EntryMetadata {
        mode: metadata.st_mode as u32,
        uid: metadata.st_uid,
        nlink: metadata.st_nlink as u64,
        dev: metadata.st_dev as u64,
        ino: metadata.st_ino as u64,
        len: u64::try_from(metadata.st_size).unwrap_or(u64::MAX),
    })
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &CStr) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const MODE_REPAIR_DIRECTORY_OPEN_FLAGS: libc::c_int =
    libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[cfg(target_vendor = "apple")]
const MODE_REPAIR_DIRECTORY_OPEN_FLAGS: libc::c_int =
    libc::O_SEARCH | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
const MODE_REPAIR_DIRECTORY_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

#[cfg(unix)]
fn open_directory_for_mode_repair_at(parent: &File, name: &CStr) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            MODE_REPAIR_DIRECTORY_OPEN_FLAGS,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn chmod_mode_repair_directory(directory: &File) -> io::Result<()> {
    const SYS_FCHMODAT2: libc::c_long = 452;
    let result = unsafe {
        libc::syscall(
            SYS_FCHMODAT2,
            directory.as_raw_fd(),
            c"".as_ptr(),
            0o700 as libc::mode_t,
            libc::AT_EMPTY_PATH,
        )
    };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn chmod_mode_repair_directory(directory: &File) -> io::Result<()> {
    let result = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
    (result == 0)
        .then_some(())
        .ok_or_else(io::Error::last_os_error)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // C variadics promote Darwin's narrower mode_t.
fn open_file_at(
    directory: &TransactionDirectory,
    name: &OsStr,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    let name = cstring_name(name)?;
    let descriptor = unsafe {
        libc::openat(
            directory.file.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(unix)]
fn open_journal_file(directory: &TransactionDirectory, name: &OsStr) -> io::Result<File> {
    open_file_at(directory, name, libc::O_RDWR, 0)
}

#[cfg(not(unix))]
fn open_journal_file(_directory: &TransactionDirectory, _name: &OsStr) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored journal open unsupported",
    ))
}

#[cfg(unix)]
fn open_entry_file(directory: &TransactionDirectory, name: &OsStr) -> io::Result<File> {
    open_file_at(directory, name, libc::O_RDONLY, 0)
}

#[cfg(not(unix))]
fn open_entry_file(_directory: &TransactionDirectory, _name: &OsStr) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored entry open unsupported",
    ))
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // libc stat field widths vary between Unix targets.
fn entry_metadata_at(directory: &TransactionDirectory, name: &OsStr) -> io::Result<EntryMetadata> {
    let name = cstring_name(name)?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.file.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    Ok(EntryMetadata {
        mode: metadata.st_mode as u32,
        uid: metadata.st_uid,
        nlink: metadata.st_nlink as u64,
        dev: metadata.st_dev as u64,
        ino: metadata.st_ino as u64,
        len: u64::try_from(metadata.st_size).unwrap_or(u64::MAX),
    })
}

#[cfg(not(unix))]
fn entry_metadata_at(
    _directory: &TransactionDirectory,
    _name: &OsStr,
) -> io::Result<EntryMetadata> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored journal metadata unsupported",
    ))
}

#[cfg(unix)]
fn read_directory_names(
    directory: &TransactionDirectory,
    maximum: usize,
) -> io::Result<Vec<OsString>> {
    let descriptor = unsafe { libc::dup(directory.file.as_raw_fd()) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(descriptor);
        }
        return Err(error);
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        set_errno(0);
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = current_errno();
            return if error == 0 {
                Ok(names)
            } else {
                Err(io::Error::from_raw_os_error(error))
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
            if names.len() >= maximum {
                return Ok(names);
            }
        }
    }
}

#[cfg(not(unix))]
fn read_directory_names(
    _directory: &TransactionDirectory,
    _maximum: usize,
) -> io::Result<Vec<OsString>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored journal scan unsupported",
    ))
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(target_os = "linux")]
const _: unsafe extern "C" fn() -> *mut libc::c_int = libc::__errno_location;

#[cfg(target_os = "linux")]
fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "android")]
const _: unsafe extern "C" fn() -> *mut libc::c_int = libc::__errno;

#[cfg(target_os = "android")]
fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno() }
}

#[cfg(target_vendor = "apple")]
fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn errno_location() -> *mut libc::c_int {
    std::ptr::null_mut()
}

#[cfg(unix)]
fn set_errno(value: libc::c_int) {
    let location = errno_location();
    if !location.is_null() {
        unsafe {
            *location = value;
        }
    }
}

#[cfg(unix)]
fn current_errno() -> libc::c_int {
    let location = errno_location();
    if location.is_null() {
        libc::ENOTSUP
    } else {
        unsafe { *location }
    }
}

#[cfg(unix)]
fn cstring_name(name: &OsStr) -> io::Result<CString> {
    if name.as_bytes().contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal entry name contains a separator",
        ));
    }
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal entry name contains NUL",
        )
    })
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // libc mode_t widths vary between Unix targets.
fn valid_path_metadata(metadata: &EntryMetadata) -> bool {
    metadata.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
        && metadata.uid == unsafe { libc::geteuid() }
        && metadata.mode & 0o777 == 0o600
        && metadata.nlink == 1
}

#[cfg(not(unix))]
fn valid_path_metadata(_metadata: &EntryMetadata) -> bool {
    false
}

#[cfg(unix)]
fn valid_opened_metadata(metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    metadata.file_type().is_file()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o777 == 0o600
        && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn valid_opened_metadata(_metadata: &Metadata) -> bool {
    false
}

fn valid_opened_journal_file(path_metadata: &EntryMetadata, opened_metadata: &Metadata) -> bool {
    valid_path_metadata(path_metadata)
        && valid_opened_metadata(opened_metadata)
        && entry_matches_metadata(path_metadata, opened_metadata)
}

#[cfg(unix)]
fn entry_matches_metadata(entry: &EntryMetadata, metadata: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    entry.dev == metadata.dev() && entry.ino == metadata.ino()
}

#[cfg(not(unix))]
fn entry_matches_metadata(_entry: &EntryMetadata, _metadata: &Metadata) -> bool {
    false
}

fn same_entry_identity(left: &EntryMetadata, right: &EntryMetadata) -> bool {
    left.dev == right.dev && left.ino == right.ino
}

fn entry_matches_open_file(directory: &TransactionDirectory, name: &OsStr, file: &File) -> bool {
    let Ok(path_metadata) = entry_metadata_at(directory, name) else {
        return false;
    };
    let Ok(opened_metadata) = file.metadata() else {
        return false;
    };
    valid_opened_journal_file(&path_metadata, &opened_metadata)
}

fn set_file_mode(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn remove_locked_file(
    directory: &TransactionDirectory,
    name: &OsStr,
    file: &File,
) -> Result<(), TransactionError> {
    remove_locked_file_with_directory_sync(directory, name, file, &mut |directory: &File| {
        directory.sync_all()
    })
}

fn remove_locked_file_with_directory_sync<S>(
    directory: &TransactionDirectory,
    name: &OsStr,
    file: &File,
    sync_directory: &mut S,
) -> Result<(), TransactionError>
where
    S: FnMut(&File) -> io::Result<()>,
{
    if !entry_matches_open_file(directory, name, file) {
        return Err(TransactionError::InvalidJournal);
    }
    unlink_file_at(directory, name)
        .map_err(|error| TransactionError::filesystem("remove journal", error))?;
    sync_directory(&directory.file)
        .map_err(|_| TransactionError::RemovalSyncUncertain("sync journal directory"))
}

#[cfg(unix)]
fn unlink_file_at(directory: &TransactionDirectory, name: &OsStr) -> io::Result<()> {
    let name = cstring_name(name)?;
    let result = unsafe { libc::unlinkat(directory.file.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn unlink_file_at(_directory: &TransactionDirectory, _name: &OsStr) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "anchored journal removal unsupported",
    ))
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use coding_brain_core::brain_activity::{
        ActivityEvent, ActivityState, MAX_ACTIVITY_FIELD_BYTES,
    };
    use coding_brain_core::lifecycle::{LifecycleSnapshot, LifecycleStore, PermissionDisposition};
    use fs2::FileExt;

    use super::*;
    use crate::brain::activity::ActivityStore;
    use crate::brain::decisions::{HookDecisionRecord, ensure_hook_record_at};

    const RAW_COMMAND: &str = "printf do-not-leak-this-raw-command";

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn test_fault_directory_authority_survives_parent_rename_and_symlink_swap() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let home = tempfile::tempdir().unwrap();
        let fault_path = home.path().join(".cbrain-test-permission-tx");
        fs::create_dir(&fault_path).unwrap();
        fs::set_permissions(&fault_path, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = fault_path.join("marker");
        let release = fault_path.join("release");
        let authority = open_test_fault_directory(home.path(), &marker, &release).unwrap();

        let retained_path = home.path().join("retained-fault-dir");
        fs::rename(&fault_path, &retained_path).unwrap();
        let attacker = home.path().join("attacker");
        fs::create_dir(&attacker).unwrap();
        symlink(&attacker, &fault_path).unwrap();
        fs::write(retained_path.join("release"), b"release").unwrap();

        create_test_fault_marker(&authority, marker.file_name().unwrap(), "after_prepare").unwrap();
        wait_for_test_fault_release(&authority, release.file_name().unwrap()).unwrap();

        assert_eq!(
            fs::read(retained_path.join("marker")).unwrap(),
            b"after_prepare"
        );
        assert!(!attacker.join("marker").exists());
        assert!(!attacker.join("release").exists());
    }

    fn journal(transaction_id: &str) -> PermissionTransactionJournal {
        test_support::journal(transaction_id)
    }

    fn transaction_dir(state_root: &Path) -> PathBuf {
        state_root.join("brain/permission-transactions")
    }

    fn create_journal_parent(path: &Path) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        #[cfg(unix)]
        if parent.file_name() == Some(OsStr::new("permission-transactions"))
            && parent.parent().and_then(Path::file_name) == Some(OsStr::new("brain"))
        {
            use std::os::unix::fs::PermissionsExt;

            for directory in [parent.parent().unwrap(), parent] {
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
            }
        }
    }

    fn write_journal(path: &Path, value: &PermissionTransactionJournal) {
        create_journal_parent(path);
        let bytes = serde_json::to_vec(value).unwrap();
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn creation_identity(order: u64) -> String {
        format!("{order:039}-0000000001-00000000000000000000")
    }

    fn final_path(state_root: &Path, order: u64) -> PathBuf {
        transaction_dir(state_root).join(format!(
            "permission-transaction-{}.json",
            creation_identity(order)
        ))
    }

    fn temp_path(state_root: &Path, order: u64) -> PathBuf {
        transaction_dir(state_root).join(format!(
            "permission-transaction.tmp-{}",
            creation_identity(order)
        ))
    }

    fn decisions_path(state_root: &Path) -> PathBuf {
        state_root.join("brain/decisions.jsonl")
    }

    fn activity_store(state_root: &Path) -> ActivityStore {
        ActivityStore::at(state_root.join("activity.jsonl"))
    }

    fn lifecycle_store(state_root: &Path) -> LifecycleStore {
        LifecycleStore::at(state_root)
    }

    fn record_authority(
        state_root: &Path,
        journal: &PermissionTransactionJournal,
        disposition: PermissionDisposition,
    ) {
        let decision = match disposition {
            PermissionDisposition::Decided => expected_decision(journal),
            PermissionDisposition::NeedsInput => PermissionDecision::NeedsInput,
        };
        lifecycle_store(state_root)
            .ensure_permission_decision(&journal.lifecycle_identity, &journal.request_key, decision)
            .unwrap();
    }

    fn proposal_records(state_root: &Path) -> Vec<HookDecisionRecord> {
        fs::read_to_string(decisions_path(state_root))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn terminal_events(state_root: &Path) -> Vec<ActivityEvent> {
        activity_store(state_root)
            .read()
            .unwrap()
            .events()
            .iter()
            .filter(|event| event.state.is_terminal())
            .cloned()
            .collect()
    }

    fn expected_recovery_error(journal: &PermissionTransactionJournal) -> ActivityEvent {
        let mut event = journal.terminal.clone();
        event.state = ActivityState::Error;
        event.reasoning = Some(RECOVERY_AUTHORITY_ERROR_REASON.into());
        event
    }

    fn assert_destinations(
        state_root: &Path,
        journal: &PermissionTransactionJournal,
        terminal: &ActivityEvent,
        disposition: PermissionDisposition,
    ) {
        assert_eq!(
            proposal_records(state_root).as_slice(),
            std::slice::from_ref(&journal.proposal)
        );
        assert_eq!(
            terminal_events(state_root).as_slice(),
            std::slice::from_ref(terminal)
        );
        assert_eq!(
            lifecycle_store(state_root)
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(disposition)
        );
    }

    fn assert_no_pending_journals(state_root: &Path) {
        let (recoverable, report) = PermissionTransactionStore::at(state_root)
            .discover(RecoveryLimits::default())
            .unwrap();
        assert!(recoverable.is_empty());
        assert_eq!(report, RecoveryReport::default());
    }

    #[test]
    fn rollback_is_ready_only_without_recovery_blockers() {
        assert!(RecoveryReport::default().rollback_ready());
        assert!(
            RecoveryReport {
                completed: 3,
                ..RecoveryReport::default()
            }
            .rollback_ready()
        );
        for report in [
            RecoveryReport {
                active: 1,
                ..RecoveryReport::default()
            },
            RecoveryReport {
                invalid: 1,
                ..RecoveryReport::default()
            },
            RecoveryReport {
                over_budget: 1,
                ..RecoveryReport::default()
            },
            RecoveryReport {
                pending: 1,
                ..RecoveryReport::default()
            },
            RecoveryReport {
                removal_sync_uncertain: 1,
                ..RecoveryReport::default()
            },
        ] {
            assert!(!report.rollback_ready(), "unexpectedly ready: {report:?}");
        }
    }

    #[test]
    fn startup_recovery_limits_all_inputs() {
        let limits = RecoveryLimits::startup();
        assert!(limits.max_journals <= DEFAULT_MAX_JOURNALS);
        assert!(limits.max_total_bytes <= DEFAULT_MAX_TOTAL_BYTES);
        assert!(limits.max_destination_bytes <= LIVE_MAX_DESTINATION_BYTES);
        assert_eq!(limits.directory_lock, LockAcquisition::Nonblocking);
    }

    #[test]
    fn crash_matrix_recovers_exact_destinations_and_is_idempotent() {
        let cases = [
            (CommitFault::None, ActivityState::Allowed),
            (CommitFault::AfterPrepare, ActivityState::Error),
            (CommitFault::AfterProposal, ActivityState::Error),
            (CommitFault::AfterLifecycle, ActivityState::Allowed),
            (CommitFault::AfterTerminal, ActivityState::Allowed),
            (CommitFault::BeforeJournalRemoval, ActivityState::Allowed),
        ];

        for (fault, expected_state) in cases {
            let temp = tempfile::tempdir().unwrap();
            let journal = journal(&format!("crash-{fault:?}"));
            let prepared = PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap();
            let result = commit_with_fault(
                prepared,
                &lifecycle_store(temp.path()),
                &activity_store(temp.path()),
                &decisions_path(temp.path()),
                fault,
            );
            if fault == CommitFault::None {
                result.unwrap();
            } else {
                assert!(result.is_err(), "fault {fault:?} unexpectedly committed");
            }

            let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
            assert_eq!(
                first.completed,
                usize::from(fault != CommitFault::None),
                "unexpected first recovery report for {fault:?}: {first:?}"
            );
            let (terminal, disposition) = if expected_state == ActivityState::Allowed {
                (journal.terminal.clone(), PermissionDisposition::Decided)
            } else {
                (
                    expected_recovery_error(&journal),
                    PermissionDisposition::NeedsInput,
                )
            };
            assert_destinations(temp.path(), &journal, &terminal, disposition);
            assert_no_pending_journals(temp.path());

            let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
            assert_eq!(second, RecoveryReport::default());
            assert_destinations(temp.path(), &journal, &terminal, disposition);
        }
    }

    #[test]
    fn commit_reports_post_unlink_directory_sync_uncertainty() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("commit-removal-sync-uncertain");
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();
        let journal_path = prepared.path().to_path_buf();

        let result = commit_with_fault(
            prepared,
            &lifecycle_store(temp.path()),
            &activity_store(temp.path()),
            &decisions_path(temp.path()),
            CommitFault::AfterUnlinkDirectorySyncFailure,
        );

        assert!(matches!(
            result,
            Err(TransactionError::RemovalSyncUncertain(
                "sync journal directory"
            ))
        ));
        assert!(!journal_path.exists());
        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::NeedsInput,
        );
        assert!(
            activity_store(temp.path())
                .read()
                .unwrap()
                .events()
                .iter()
                .all(|event| !matches!(
                    event.state,
                    ActivityState::Delivered | ActivityState::DeliveryFailed
                ))
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn recovery_reports_post_unlink_sync_uncertainty_and_reappearance_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("recovery-removal-sync-uncertain");
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();
        let journal_path = prepared.path().to_path_buf();
        assert!(
            commit_with_fault(
                prepared,
                &lifecycle_store(temp.path()),
                &activity_store(temp.path()),
                &decisions_path(temp.path()),
                CommitFault::BeforeJournalRemoval,
            )
            .is_err()
        );

        let report =
            recover_pending_with_directory_sync_failure(temp.path(), RecoveryLimits::default())
                .unwrap();

        assert_eq!(report.completed, 0);
        assert_eq!(report.pending, 0);
        assert_eq!(report.removal_sync_uncertain, 1);
        assert!(!journal_path.exists());
        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::NeedsInput,
        );
        assert!(terminal_events(temp.path()).iter().all(|event| !matches!(
            event.state,
            ActivityState::Delivered | ActivityState::DeliveryFailed
        )));
        assert_eq!(
            recover_pending(temp.path(), RecoveryLimits::default()).unwrap(),
            RecoveryReport::default()
        );

        write_journal(&journal_path, &journal);
        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::NeedsInput,
        );
        assert!(journal_path.exists());
    }

    #[test]
    fn recovery_never_reconstructs_allow_without_exact_lifecycle_authority() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("missing-authority");
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(report.completed, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn recovery_completes_undelivered_allow_with_exact_authority() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("exact-authority");
        record_authority(temp.path(), &journal, PermissionDisposition::Decided);
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(report.completed, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::Decided,
        );
        assert!(
            activity_store(temp.path())
                .read()
                .unwrap()
                .events()
                .iter()
                .all(|event| !matches!(
                    event.state,
                    ActivityState::Delivered | ActivityState::DeliveryFailed
                ))
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn recovery_compensates_allow_when_authority_was_revoked() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("compensated-authority");
        record_authority(temp.path(), &journal, PermissionDisposition::Decided);
        record_authority(temp.path(), &journal, PermissionDisposition::NeedsInput);
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(report.completed, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn recovery_rejects_inexact_request_authority() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("wrong-request-authority");
        let mut other = journal.clone();
        other.request_key = "b".repeat(64);
        record_authority(temp.path(), &other, PermissionDisposition::Decided);
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_destinations(
            temp.path(),
            &journal,
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &other.request_key)
                .unwrap(),
            Some(PermissionDisposition::Decided)
        );
    }

    #[test]
    fn exact_authority_rejects_same_request_with_wrong_action_or_transaction() {
        for (name, authority) in [
            (
                "wrong-action",
                PermissionAuthority {
                    transaction_id: "wrong-action".into(),
                    action: PermissionAction::Deny,
                },
            ),
            (
                "wrong-transaction",
                PermissionAuthority {
                    transaction_id: "another-transaction".into(),
                    action: PermissionAction::Allow,
                },
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let journal = journal(name);
            lifecycle_store(temp.path())
                .ensure_permission_decision(
                    &journal.lifecycle_identity,
                    &journal.request_key,
                    PermissionDecision::Decided(authority),
                )
                .unwrap();
            drop(
                PermissionTransactionStore::at(temp.path())
                    .prepare(journal.clone())
                    .unwrap(),
            );

            let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
            let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

            assert_eq!(first.pending, 1);
            assert_eq!(second.completed, 1);
            assert_destinations(
                temp.path(),
                &journal,
                &expected_recovery_error(&journal),
                PermissionDisposition::NeedsInput,
            );
            assert_no_pending_journals(temp.path());
        }
    }

    #[test]
    fn live_recovery_rejects_destination_byte_overage_before_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("live-over-budget");
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );
        create_journal_parent(&decisions_path(temp.path()));
        fs::write(
            decisions_path(temp.path()),
            vec![b' '; LIVE_MAX_DESTINATION_BYTES + 1],
        )
        .unwrap();
        let guard = PermissionRequestLockStore::at(temp.path())
            .try_acquire(&journal.lifecycle_identity, &journal.request_key)
            .unwrap()
            .unwrap();

        let report =
            recover_pending_with_guard(temp.path(), RecoveryLimits::live(), &guard).unwrap();

        assert_eq!(report.over_budget, 1);
        assert_eq!(
            report.over_budget_detail,
            Some(OverBudgetDetail {
                source: OverBudgetSource::DecisionEvidence,
                limit: LIVE_MAX_DESTINATION_BYTES,
            })
        );
        assert!(terminal_events(temp.path()).is_empty());
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_decision(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            None
        );
        let (_, pending) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::live())
            .unwrap();
        assert_eq!(pending.pending, 1);
    }

    #[test]
    fn startup_recovery_reports_active_request_without_destination_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("active-request");
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );
        let guard = PermissionRequestLockStore::at(temp.path())
            .try_acquire(&journal.lifecycle_identity, &journal.request_key)
            .unwrap()
            .unwrap();

        let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(report.active, 1);
        assert!(proposal_records(temp.path()).is_empty());
        assert!(terminal_events(temp.path()).is_empty());
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_decision(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            None
        );
        drop(guard);
        let compensated = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let completed = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        assert_eq!(compensated.completed, 1);
        assert_eq!(completed, RecoveryReport::default());
    }

    #[test]
    fn live_budget_is_combined_across_scans_and_final_verification() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first.jsonl");
        let second = temp.path().join("second.jsonl");
        fs::write(&first, vec![b'x'; 8 * 1024 * 1024]).unwrap();
        fs::write(&second, vec![b'y'; 8 * 1024 * 1024]).unwrap();
        let mut budget = LiveEvidenceBudget::new(LIVE_MAX_DESTINATION_BYTES);
        let mut evidence = RecoveryEvidence::bounded(&mut budget, LIVE_MAX_DESTINATION_BYTES);

        assert_eq!(
            evidence.read_decisions(&first).unwrap().len(),
            8 * 1024 * 1024
        );
        assert_eq!(
            evidence.read_decisions(&second).unwrap().len(),
            8 * 1024 * 1024
        );
        assert!(matches!(
            evidence.read_decisions(&first),
            Err(TransactionError::OverBudget)
        ));
    }

    #[test]
    fn live_activity_destination_overage_reports_fixed_store_and_limit() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("live-activity-over-budget");
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );
        fs::write(
            temp.path().join("activity.jsonl"),
            vec![b' '; LIVE_MAX_DESTINATION_BYTES + 1],
        )
        .unwrap();
        let guard = PermissionRequestLockStore::at(temp.path())
            .try_acquire(&journal.lifecycle_identity, &journal.request_key)
            .unwrap()
            .unwrap();

        let report =
            recover_pending_with_guard(temp.path(), RecoveryLimits::live(), &guard).unwrap();

        assert_eq!(report.over_budget, 1);
        assert_eq!(
            report.over_budget_detail,
            Some(OverBudgetDetail {
                source: OverBudgetSource::ActivityEvidence,
                limit: LIVE_MAX_DESTINATION_BYTES,
            })
        );
    }

    #[test]
    fn live_prepare_uses_one_nonblocking_directory_lock_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let directory = store.open_transaction_directory(true).unwrap().unwrap();
        let held = DirectoryAdmissionLock::lock_exclusive(&directory.file).unwrap();

        assert!(matches!(
            store.prepare_live(journal("live-directory-busy")),
            Err(TransactionError::Active)
        ));
        drop(held);
        assert!(store.prepare_live(journal("live-directory-ready")).is_ok());
    }

    #[test]
    fn live_preflight_initializes_and_releases_transaction_directory() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());

        assert_eq!(store.preflight_live().unwrap(), RecoveryReport::default());
        assert!(transaction_dir(temp.path()).is_dir());
        assert!(store.prepare_live(journal("after-live-preflight")).is_ok());
    }

    #[test]
    fn live_limits_accept_one_journal_and_reject_two_or_one_mib_overage() {
        let temp = tempfile::tempdir().unwrap();
        let first = final_path(temp.path(), 1);
        let second = final_path(temp.path(), 2);
        write_journal(&first, &journal("live-first"));
        let (one, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::live())
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(report.pending, 1);
        drop(one);

        write_journal(&second, &journal("live-second"));
        let (two, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::live())
            .unwrap();
        assert!(two.is_empty());
        assert_eq!(report.over_budget, 1);

        fs::remove_file(&second).unwrap();
        fs::write(&first, vec![b'x'; MAX_JOURNAL_BYTES + 1]).unwrap();
        let (oversized, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::live())
            .unwrap();
        assert!(oversized.is_empty());
        assert_eq!(report.over_budget, 1);
    }

    #[test]
    fn recovery_preserves_corrupt_authority_and_remains_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("corrupt-authority");
        let lifecycle = lifecycle_store(temp.path());
        fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
        fs::write(lifecycle.snapshot_path(), b"{not-json").unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_eq!(fs::read(lifecycle.snapshot_path()).unwrap(), b"{not-json");
        assert_eq!(
            terminal_events(temp.path()),
            [expected_recovery_error(&journal)]
        );
        assert!(final_path(temp.path(), 0).parent().unwrap().exists());
    }

    #[test]
    fn normal_allow_commit_rejects_corrupt_lifecycle_and_recovers_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("commit-corrupt-authority");
        let lifecycle = lifecycle_store(temp.path());
        fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
        fs::write(lifecycle.snapshot_path(), b"{not-json").unwrap();
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();

        let result = commit(
            prepared,
            &lifecycle,
            &activity_store(temp.path()),
            &decisions_path(temp.path()),
        );

        assert!(result.is_err());
        assert_eq!(fs::read(lifecycle.snapshot_path()).unwrap(), b"{not-json");
        assert_eq!(
            terminal_events(temp.path()),
            [expected_recovery_error(&journal)]
        );
        let (_, pending) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();
        assert_eq!(pending.pending, 1);

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_eq!(fs::read(lifecycle.snapshot_path()).unwrap(), b"{not-json");
        let (_, pending) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();
        assert_eq!(pending.pending, 1);
    }

    #[test]
    fn unreadable_decision_evidence_compensates_and_retains_journal() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("unreadable-decision-evidence");
        record_authority(temp.path(), &journal, PermissionDisposition::Decided);
        create_journal_parent(&final_path(temp.path(), 1));
        fs::write(decisions_path(temp.path()), b"{not-json\n").unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert!(terminal_events(temp.path()).is_empty());
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::NeedsInput)
        );
        assert_eq!(
            PermissionTransactionStore::at(temp.path())
                .discover(RecoveryLimits::default())
                .unwrap()
                .1
                .pending,
            1
        );
    }

    #[test]
    fn unreadable_activity_evidence_compensates_and_retains_journal() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("unreadable-activity-evidence");
        record_authority(temp.path(), &journal, PermissionDisposition::Decided);
        fs::write(temp.path().join("activity.jsonl"), b"{not-json\n").unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert!(terminal_events(temp.path()).is_empty());
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::NeedsInput)
        );
        assert_eq!(
            PermissionTransactionStore::at(temp.path())
                .discover(RecoveryLimits::default())
                .unwrap()
                .1
                .pending,
            1
        );
    }

    #[test]
    fn unreadable_activity_preserves_unreadable_authority_and_retains_journal() {
        for (case, authority) in [
            ("corrupt", b"{not-json".to_vec()),
            ("future", {
                let mut snapshot = serde_json::to_value(LifecycleSnapshot::default()).unwrap();
                snapshot["schema_version"] = serde_json::json!(u32::MAX);
                serde_json::to_vec(&snapshot).unwrap()
            }),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let journal = journal(&format!("unreadable-activity-{case}-authority"));
            let lifecycle = lifecycle_store(temp.path());
            fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
            fs::write(lifecycle.snapshot_path(), &authority).unwrap();
            fs::write(temp.path().join("activity.jsonl"), b"{not-json\n").unwrap();
            drop(
                PermissionTransactionStore::at(temp.path())
                    .prepare(journal)
                    .unwrap(),
            );

            let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

            assert_eq!(report.pending, 1, "{case}");
            assert_eq!(
                fs::read(lifecycle.snapshot_path()).unwrap(),
                authority,
                "{case}"
            );
            let hooks = fs::read_dir(lifecycle.hooks_dir())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            assert!(
                hooks
                    .iter()
                    .all(|name| !name.to_string_lossy().contains(".corrupt-")),
                "{case}: {hooks:?}"
            );
            assert_eq!(
                PermissionTransactionStore::at(temp.path())
                    .discover(RecoveryLimits::default())
                    .unwrap()
                    .1
                    .pending,
                1,
                "{case}"
            );
        }
    }

    #[test]
    fn newer_authority_records_error_but_retains_unresolved_journal() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("newer-authority");
        let lifecycle = lifecycle_store(temp.path());
        fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
        let mut snapshot = serde_json::to_value(LifecycleSnapshot::default()).unwrap();
        snapshot["schema_version"] = serde_json::json!(u32::MAX);
        fs::write(
            lifecycle.snapshot_path(),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_eq!(
            proposal_records(temp.path()).as_slice(),
            std::slice::from_ref(&journal.proposal)
        );
        assert_eq!(
            terminal_events(temp.path()),
            [expected_recovery_error(&journal)]
        );
        assert_eq!(
            lifecycle
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            None
        );
        assert!(final_path(temp.path(), 1).parent().unwrap().exists());
        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(report.pending, 1);
    }

    #[test]
    fn busy_authority_records_error_then_completes_after_lock_release() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("busy-authority");
        let lifecycle = lifecycle_store(temp.path());
        fs::create_dir_all(lifecycle.hooks_dir()).unwrap();
        let held = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lifecycle.lock_path())
            .unwrap();
        held.lock_exclusive().unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let blocked = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        assert_eq!(blocked.pending, 1);
        assert_eq!(
            terminal_events(temp.path()),
            [expected_recovery_error(&journal)]
        );

        FileExt::unlock(&held).unwrap();
        let completed = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(completed.completed, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn recovery_rolls_non_allow_terminals_forward_without_allow_authority() {
        let cases = [
            (
                "deny",
                ActivityState::Denied,
                "deny",
                PermissionDisposition::Decided,
            ),
            (
                "abstain",
                ActivityState::Abstained,
                "abstain",
                PermissionDisposition::NeedsInput,
            ),
            (
                "error",
                ActivityState::Error,
                "abstain",
                PermissionDisposition::NeedsInput,
            ),
        ];

        for (transaction_id, state, action, disposition) in cases {
            let temp = tempfile::tempdir().unwrap();
            let mut journal = journal(transaction_id);
            journal.proposal.brain_action = action.into();
            journal.terminal.state = state;
            journal.disposition = disposition;
            journal.allow_requires_lifecycle_authority = false;
            drop(
                PermissionTransactionStore::at(temp.path())
                    .prepare(journal.clone())
                    .unwrap(),
            );

            let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
            let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

            assert_eq!(first.completed, 1);
            assert_eq!(second, RecoveryReport::default());
            assert_destinations(temp.path(), &journal, &journal.terminal, disposition);
            assert!(
                terminal_events(temp.path())
                    .iter()
                    .all(|event| event.state != ActivityState::Allowed)
            );
            assert_no_pending_journals(temp.path());
        }
    }

    #[test]
    fn schema_one_hook_journal_remains_functional_until_hook_cutover() {
        let temp = tempfile::tempdir().unwrap();
        let mut journal = journal("schema-one-hook");
        journal.schema_version = 1;

        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();
        commit(
            prepared,
            &lifecycle_store(temp.path()),
            &activity_store(temp.path()),
            &decisions_path(temp.path()),
        )
        .unwrap();

        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::Decided,
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn validation_rejects_every_terminal_disposition_contradiction() {
        for (name, state, action, disposition) in [
            (
                "allow-needs-input",
                ActivityState::Allowed,
                "approve",
                PermissionDisposition::NeedsInput,
            ),
            (
                "deny-needs-input",
                ActivityState::Denied,
                "deny",
                PermissionDisposition::NeedsInput,
            ),
            (
                "abstain-decided",
                ActivityState::Abstained,
                "abstain",
                PermissionDisposition::Decided,
            ),
            (
                "error-decided",
                ActivityState::Error,
                "abstain",
                PermissionDisposition::Decided,
            ),
        ] {
            for schema_version in [1, JOURNAL_SCHEMA_VERSION] {
                let temp = tempfile::tempdir().unwrap();
                let mut invalid = journal(name);
                invalid.schema_version = schema_version;
                invalid.terminal.state = state;
                invalid.proposal.brain_action = action.into();
                invalid.disposition = disposition;
                invalid.allow_requires_lifecycle_authority = state == ActivityState::Allowed;

                assert!(matches!(
                    PermissionTransactionStore::at(temp.path()).prepare(invalid),
                    Err(TransactionError::InvalidJournal)
                ));

                let recovery = tempfile::tempdir().unwrap();
                let path = final_path(recovery.path(), 1);
                let mut stored = journal(name);
                stored.schema_version = schema_version;
                stored.terminal.state = state;
                stored.proposal.brain_action = action.into();
                stored.disposition = disposition;
                stored.allow_requires_lifecycle_authority = state == ActivityState::Allowed;
                write_journal(&path, &stored);

                let report = recover_pending(recovery.path(), RecoveryLimits::default()).unwrap();
                assert_eq!(report.invalid, 1);
                assert!(path.exists());
                assert!(!decisions_path(recovery.path()).exists());
                assert!(!recovery.path().join("activity.jsonl").exists());
                assert!(!recovery.path().join("brain/lifecycle").exists());
            }
        }
    }

    #[test]
    fn terminal_action_validation_accepts_advisory_actions_and_rejects_error_authority() {
        for (action, state) in [
            ("approve", ActivityState::Allowed),
            ("deny", ActivityState::Denied),
            ("approve", ActivityState::Abstained),
            ("deny", ActivityState::Abstained),
            ("abstain", ActivityState::Abstained),
            ("abstain", ActivityState::Error),
        ] {
            assert!(action_matches_terminal(action, state), "{action}/{state:?}");
        }

        for (action, state) in [
            ("approve", ActivityState::Denied),
            ("deny", ActivityState::Allowed),
            ("abstain", ActivityState::Allowed),
            ("abstain", ActivityState::Denied),
            ("approve", ActivityState::Error),
            ("deny", ActivityState::Error),
        ] {
            assert!(
                !action_matches_terminal(action, state),
                "{action}/{state:?}"
            );
        }
    }

    #[test]
    fn recovery_retains_conflicting_proposal_as_unresolved() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("proposal-conflict");
        let mut conflicting = journal.proposal.clone();
        conflicting.brain_action = "deny".into();
        ensure_hook_record_at(&decisions_path(temp.path()), &conflicting).unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_eq!(proposal_records(temp.path()), [conflicting]);
        assert_eq!(
            terminal_events(temp.path()),
            [expected_recovery_error(&journal)]
        );
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::NeedsInput)
        );
        assert_eq!(
            PermissionTransactionStore::at(temp.path())
                .discover(RecoveryLimits::default())
                .unwrap()
                .1
                .pending,
            1
        );
    }

    #[test]
    fn proposal_conflict_compensation_preserves_unreadable_activity_bytes() {
        let cases: [(&str, &[u8]); 2] = [
            ("malformed", b"{not-json\n"),
            ("truncated", b"{\"schema_version\":3"),
        ];

        for (case, activity_bytes) in cases {
            let temp = tempfile::tempdir().unwrap();
            let journal = journal(&format!("proposal-conflict-{case}"));
            let mut conflicting = journal.proposal.clone();
            conflicting.brain_action = "deny".into();
            ensure_hook_record_at(&decisions_path(temp.path()), &conflicting).unwrap();
            fs::write(temp.path().join("activity.jsonl"), activity_bytes).unwrap();
            let prepared = PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap();

            let result = commit(
                prepared,
                &lifecycle_store(temp.path()),
                &activity_store(temp.path()),
                &decisions_path(temp.path()),
            );

            assert!(matches!(
                result,
                Err(TransactionError::Destination("persist proposal"))
            ));
            assert_eq!(
                fs::read(temp.path().join("activity.jsonl")).unwrap(),
                activity_bytes
            );
            assert_eq!(
                lifecycle_store(temp.path())
                    .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                    .unwrap(),
                Some(PermissionDisposition::NeedsInput)
            );
            let (_, report) = PermissionTransactionStore::at(temp.path())
                .discover(RecoveryLimits::default())
                .unwrap();
            assert_eq!(report.pending, 1);
        }
    }

    #[test]
    fn recovery_compensates_proposal_conflict_after_durable_decided() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("decided-proposal-conflict");
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();
        assert!(
            commit_with_fault(
                prepared,
                &lifecycle_store(temp.path()),
                &activity_store(temp.path()),
                &decisions_path(temp.path()),
                CommitFault::AfterLifecycle,
            )
            .is_err()
        );
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::Decided)
        );
        let mut conflicting = journal.proposal.clone();
        conflicting.brain_action = "deny".into();
        let mut conflict_bytes = serde_json::to_vec(&conflicting).unwrap();
        conflict_bytes.push(b'\n');
        fs::write(decisions_path(temp.path()), conflict_bytes).unwrap();

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_destinations(
            temp.path(),
            &PermissionTransactionJournal {
                proposal: conflicting,
                ..journal.clone()
            },
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );

        let mut exact_bytes = serde_json::to_vec(&journal.proposal).unwrap();
        exact_bytes.push(b'\n');
        fs::write(decisions_path(temp.path()), exact_bytes).unwrap();
        let repaired = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let repeated = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(repaired.completed, 1);
        assert_eq!(repeated, RecoveryReport::default());
        assert_destinations(
            temp.path(),
            &journal,
            &expected_recovery_error(&journal),
            PermissionDisposition::NeedsInput,
        );
        assert_no_pending_journals(temp.path());
    }

    #[test]
    fn final_verification_failure_revokes_decided_across_retries() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("verification-failure");
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal.clone())
            .unwrap();
        let mut conflicting = journal.proposal.clone();
        conflicting.brain_action = "deny".into();
        let decisions_path = decisions_path(temp.path());
        let mut injected = false;
        let mut sync_directory = |directory: &File| directory.sync_all();
        let mut evidence = RecoveryEvidence::unlimited();

        let result = commit_impl(
            prepared,
            &lifecycle_store(temp.path()),
            &activity_store(temp.path()),
            &decisions_path,
            &mut evidence,
            &mut |point| {
                if point == CommitPoint::AfterTerminal && !injected {
                    let mut bytes = serde_json::to_vec(&conflicting).unwrap();
                    bytes.push(b'\n');
                    fs::write(&decisions_path, bytes).unwrap();
                    injected = true;
                }
                Ok(())
            },
            &mut sync_directory,
        );

        assert!(result.is_err());
        assert_eq!(proposal_records(temp.path()), [conflicting]);
        assert_eq!(
            terminal_events(temp.path()).as_slice(),
            std::slice::from_ref(&journal.terminal)
        );
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::NeedsInput)
        );

        let mut exact_bytes = serde_json::to_vec(&journal.proposal).unwrap();
        exact_bytes.push(b'\n');
        fs::write(&decisions_path, exact_bytes).unwrap();
        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_destinations(
            temp.path(),
            &journal,
            &journal.terminal,
            PermissionDisposition::NeedsInput,
        );
        let (_, pending) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();
        assert_eq!(pending.pending, 1);
    }

    #[test]
    fn recovery_compensates_but_retains_conflicting_terminal_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal("terminal-conflict");
        record_authority(temp.path(), &journal, PermissionDisposition::Decided);
        let mut conflicting = journal.terminal.clone();
        conflicting.state = ActivityState::Denied;
        activity_store(temp.path())
            .ensure_terminal(conflicting.clone())
            .unwrap();
        drop(
            PermissionTransactionStore::at(temp.path())
                .prepare(journal.clone())
                .unwrap(),
        );

        let first = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();
        let second = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(first.pending, 1);
        assert_eq!(second.pending, 1);
        assert_eq!(terminal_events(temp.path()), [conflicting]);
        assert_eq!(
            lifecycle_store(temp.path())
                .permission_disposition(&journal.lifecycle_identity, &journal.request_key)
                .unwrap(),
            Some(PermissionDisposition::NeedsInput)
        );
        assert_eq!(
            PermissionTransactionStore::at(temp.path())
                .discover(RecoveryLimits::default())
                .unwrap()
                .1
                .pending,
            1
        );
    }

    #[test]
    fn prepared_journal_is_private_immutable_and_locked() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let expected = journal("tx-1");
        let prepared = store.prepare(expected.clone()).unwrap();
        let published = fs::read(prepared.path()).unwrap();

        assert_eq!(prepared.journal(), &expected);
        let (recoverable, report) = store.discover(RecoveryLimits::default()).unwrap();
        assert!(recoverable.is_empty());
        assert_eq!(report.active, 1);
        assert_eq!(fs::read(prepared.path()).unwrap(), published);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(prepared.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(transaction_dir(temp.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }

        drop(prepared);
        let (recoverable, report) = store.discover(RecoveryLimits::default()).unwrap();
        assert_eq!(report.pending, 1, "unexpected discovery report: {report:?}");
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].journal(), &expected);
        assert_eq!(fs::read(recoverable[0].path()).unwrap(), published);
    }

    #[test]
    fn prepared_journal_is_removed_only_by_explicit_completion() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let prepared = store.prepare(journal("tx-complete")).unwrap();
        let path = prepared.path().to_owned();

        prepared.complete().unwrap();

        assert!(!path.exists());
        assert_eq!(
            store.discover(RecoveryLimits::default()).unwrap().1,
            RecoveryReport::default()
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_intermediate_symlink_ancestry() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("state");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, state_root.join("brain")).unwrap();

        assert!(
            PermissionTransactionStore::at(&state_root)
                .prepare(journal("symlink-ancestry"))
                .is_err()
        );
        assert!(!outside.join("permission-transactions").exists());
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_app_owned_brain_directory_is_narrowed_without_mutating_ancestors() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let ordinary_ancestor = temp.path().join("ordinary");
        let state_root = ordinary_ancestor.join("state");
        let brain = state_root.join("brain");
        fs::create_dir_all(&brain).unwrap();
        fs::set_permissions(&ordinary_ancestor, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&brain, fs::Permissions::from_mode(0o755)).unwrap();

        let prepared = PermissionTransactionStore::at(&state_root)
            .prepare(journal("preexisting-brain"))
            .unwrap();

        assert_eq!(
            fs::metadata(&ordinary_ancestor)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        for path in [&brain, &transaction_dir(&state_root)] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(prepared);
        let (recoverable, report) = PermissionTransactionStore::at(&state_root)
            .discover(RecoveryLimits::default())
            .unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(report.pending, 1);
        assert_eq!(
            fs::metadata(&ordinary_ancestor)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn restrictive_umask_does_not_strand_new_journal_hierarchy() {
        let output = std::process::Command::new(
            std::env::current_exe().expect("test executable must be available"),
        )
        .args([
            "--ignored",
            "--exact",
            "brain::permission_transaction::tests::restrictive_umask_journal_hierarchy_subprocess_helper",
            "--nocapture",
        ])
        .env("CODING_BRAIN_UG26_RESTRICTIVE_UMASK_SUBPROCESS", "1")
        .output()
        .expect("restrictive-umask subprocess must run");

        assert!(
            output.status.success(),
            "restrictive-umask subprocess failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper"]
    fn restrictive_umask_journal_hierarchy_subprocess_helper() {
        use std::os::unix::fs::PermissionsExt;

        if std::env::var_os("CODING_BRAIN_UG26_RESTRICTIVE_UMASK_SUBPROCESS").as_deref()
            != Some(OsStr::new("1"))
        {
            return;
        }

        let temp = tempfile::tempdir().expect("fixture root must be created before umask change");
        let store = PermissionTransactionStore::at(temp.path());
        unsafe {
            libc::umask(0o600);
        }

        drop(
            open_transaction_directory(temp.path(), &store.directory, true)
                .expect("first journal directory open must succeed")
                .expect("journal directory must be created"),
        );
        for path in [temp.path().join("brain"), transaction_dir(temp.path())] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let (recoverable, report) = store
            .discover(RecoveryLimits::default())
            .expect("recovery open must succeed under the same umask");
        assert!(recoverable.is_empty());
        assert_eq!(report, RecoveryReport::default());
    }

    #[cfg(unix)]
    #[test]
    fn preexisting_owner_only_state_ancestor_retains_its_mode() {
        let output = std::process::Command::new(
            std::env::current_exe().expect("test executable must be available"),
        )
        .args([
            "--ignored",
            "--exact",
            "brain::permission_transaction::tests::preexisting_owner_only_state_ancestor_subprocess_helper",
            "--nocapture",
        ])
        .env("CODING_BRAIN_UG26_OWNER_ONLY_ANCESTOR_SUBPROCESS", "1")
        .output()
        .expect("owner-only-ancestor subprocess must run");

        assert!(
            output.status.success(),
            "owner-only-ancestor subprocess failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper"]
    fn preexisting_owner_only_state_ancestor_subprocess_helper() {
        use std::os::unix::fs::PermissionsExt;

        if std::env::var_os("CODING_BRAIN_UG26_OWNER_ONLY_ANCESTOR_SUBPROCESS").as_deref()
            != Some(OsStr::new("1"))
        {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let ordinary_ancestor = temp.path().join("ordinary");
        let state_root = ordinary_ancestor.join("state");
        let brain = state_root.join("brain");
        let transactions = brain.join("permission-transactions");
        fs::create_dir_all(&transactions).unwrap();
        fs::set_permissions(&ordinary_ancestor, fs::Permissions::from_mode(0o500)).unwrap();
        fs::set_permissions(&brain, fs::Permissions::from_mode(0o100)).unwrap();
        fs::set_permissions(&transactions, fs::Permissions::from_mode(0o100)).unwrap();

        let store = PermissionTransactionStore::at(&state_root);
        let prepared = store.prepare(journal("owner-only-ancestor"));
        let after_prepare = [
            fs::metadata(&ordinary_ancestor)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            fs::metadata(&brain).unwrap().permissions().mode() & 0o777,
            fs::metadata(&transactions).unwrap().permissions().mode() & 0o777,
        ];
        let discovery = match prepared {
            Ok(prepared) => {
                drop(prepared);
                store.discover(RecoveryLimits::default())
            }
            Err(error) => Err(error),
        };
        let after_discovery = [
            fs::metadata(&ordinary_ancestor)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            fs::metadata(&brain).unwrap().permissions().mode() & 0o777,
            fs::metadata(&transactions).unwrap().permissions().mode() & 0o777,
        ];
        fs::set_permissions(&ordinary_ancestor, fs::Permissions::from_mode(0o700)).unwrap();

        let (recoverable, report) = discovery.expect("prepared journal must be discoverable");
        assert_eq!(report.pending, 1, "unexpected discovery report: {report:?}");
        assert_eq!(recoverable.len(), 1);
        assert_eq!(after_prepare, [0o500, 0o700, 0o700]);
        assert_eq!(after_discovery, [0o500, 0o700, 0o700]);
    }

    #[cfg(unix)]
    #[test]
    fn completion_uses_retained_directory_authority_after_leaf_substitution() {
        let temp = tempfile::tempdir().unwrap();
        let prepared = PermissionTransactionStore::at(temp.path())
            .prepare(journal("retained-authority"))
            .unwrap();
        let journal_name = prepared.path().file_name().unwrap().to_owned();
        let original_directory = transaction_dir(temp.path());
        let moved_directory = temp.path().join("moved-transactions");
        fs::rename(&original_directory, &moved_directory).unwrap();
        fs::create_dir_all(&original_directory).unwrap();
        let attacker_path = original_directory.join(&journal_name);
        fs::write(&attacker_path, b"attacker").unwrap();

        prepared.complete().unwrap();

        assert_eq!(fs::read(&attacker_path).unwrap(), b"attacker");
        assert!(!moved_directory.join(journal_name).exists());
    }

    #[cfg(unix)]
    #[test]
    fn new_directory_is_not_accepted_when_ordered_parent_sync_fails() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        let order = std::cell::RefCell::new(Vec::new());
        let mut sync_child = |_child: &File| {
            order.borrow_mut().push("child");
            Ok(())
        };
        let mut sync_parent = |_parent: &File| {
            order.borrow_mut().push("parent");
            Err(io::Error::other("injected parent sync failure"))
        };

        let error = open_or_create_directory_at_with_syncs(
            &parent,
            OsStr::new("new-child"),
            false,
            true,
            &mut sync_child,
            &mut sync_parent,
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(*order.borrow(), ["child", "parent"]);
        assert!(temp.path().join("new-child").is_dir());
    }

    #[test]
    fn prepare_rejects_non_finite_values_before_creating_storage() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let mut invalid = journal("tx-nan");
        invalid.proposal.brain_confidence = f64::NAN;

        let error = store.prepare(invalid).unwrap_err();

        assert!(matches!(error, TransactionError::InvalidJournal));
        assert!(!transaction_dir(temp.path()).exists());
    }

    #[test]
    fn prepare_rejects_unredacted_or_unbounded_proposal_text_before_storage() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let mut unredacted = journal("tx-unredacted");
        unredacted.proposal.command = "curl --token super-secret".into();

        assert!(matches!(
            store.prepare(unredacted),
            Err(TransactionError::InvalidJournal)
        ));
        assert!(!transaction_dir(temp.path()).exists());

        let mut unredacted_reasoning = journal("tx-unredacted-reasoning");
        unredacted_reasoning.proposal.brain_reasoning = "AWS_SECRET_ACCESS_KEY=tenth-secret".into();
        assert!(matches!(
            store.prepare(unredacted_reasoning),
            Err(TransactionError::InvalidJournal)
        ));
        assert!(!transaction_dir(temp.path()).exists());

        let mut unbounded = journal("tx-unbounded");
        unbounded.proposal.brain_reasoning = "x".repeat(MAX_ACTIVITY_FIELD_BYTES + 1);
        assert!(matches!(
            store.prepare(unbounded),
            Err(TransactionError::InvalidJournal)
        ));
        assert!(!transaction_dir(temp.path()).exists());
    }

    #[test]
    fn decoder_rejects_duplicate_keys_at_top_level_and_nested_objects() {
        let encoded = serde_json::to_string(&journal("tx-duplicate-key")).unwrap();
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
    fn decoder_rejects_lossy_numbers_in_every_float_field() {
        let encoded = serde_json::to_string(&journal("tx-lossy-number")).unwrap();
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn linux_like_publication_wrapper_has_target_independent_signature() {
        let _: unsafe fn(
            libc::c_int,
            *const libc::c_char,
            libc::c_int,
            *const libc::c_char,
        ) -> libc::c_int = rename_noreplace_at;
    }

    #[cfg(unix)]
    #[test]
    fn dropping_prepared_journal_unlocks_an_inherited_file_description() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let prepared = store.prepare(journal("tx-inherited-lock")).unwrap();
        let inherited = prepared.file.try_clone().unwrap();

        drop(prepared);
        let (recoverable, report) = store.discover(RecoveryLimits::default()).unwrap();

        assert_eq!(report.pending, 1, "unexpected discovery report: {report:?}");
        assert_eq!(recoverable.len(), 1);
        drop(inherited);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_directory_admission_lock_explicitly_unlocks() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let directory = store.open_transaction_directory(true).unwrap().unwrap();
        let contender = store.open_transaction_directory(false).unwrap().unwrap();

        {
            let _held = DirectoryAdmissionLock::lock_exclusive(&directory.file).unwrap();
            assert_eq!(
                FileExt::try_lock_shared(&contender.file)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
        }

        let acquired = DirectoryAdmissionLock::try_lock_shared(&contender.file).unwrap();
        assert!(acquired.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn preparation_error_after_lock_explicitly_unlocks_inherited_description() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("read-only-journal");
        fs::write(&path, b"").unwrap();
        let file = OpenOptions::new().read(true).open(&path).unwrap();
        let inherited = file.try_clone().unwrap();
        let mut locked = LockedFile::lock(file).unwrap();

        assert!(write_and_sync_locked_file(&mut locked, b"journal").is_err());
        drop(locked);

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        contender.try_lock_exclusive().unwrap();
        drop(inherited);
    }

    #[cfg(unix)]
    #[test]
    fn invalid_discovery_after_lock_explicitly_unlocks_inherited_description() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("invalid-journal");
        fs::write(&path, b"not-json").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let inherited = file.try_clone().unwrap();
        let mut locked = LockedFile::try_lock(file).unwrap().unwrap();

        assert!(read_and_validate_journal(&mut locked).is_none());
        drop(locked);

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        contender.try_lock_exclusive().unwrap();
        drop(inherited);
    }

    #[test]
    fn discovery_is_deterministic_oldest_first() {
        let temp = tempfile::tempdir().unwrap();
        let directory = transaction_dir(temp.path());
        let newest = final_path(temp.path(), 3);
        let oldest = final_path(temp.path(), 1);
        let middle = final_path(temp.path(), 2);
        write_journal(&newest, &journal("newest"));
        write_journal(&oldest, &journal("oldest"));
        write_journal(&middle, &journal("middle"));
        assert!(directory.is_dir());

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert_eq!(report.pending, 3);
        assert_eq!(
            recoverable
                .iter()
                .map(|transaction| transaction.journal().transaction_id.as_str())
                .collect::<Vec<_>>(),
            ["oldest", "middle", "newest"]
        );
    }

    #[test]
    fn concurrent_discoveries_do_not_split_journal_locks() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        #[derive(Debug, Eq, PartialEq)]
        enum ScheduleEvent {
            FirstAcquiredOldest,
            SecondContendedOldest,
            SecondAcquiredNewest,
            FirstContendedNewest,
            SecondReturned,
        }

        const WAIT: Duration = Duration::from_secs(5);

        let temp = tempfile::tempdir().unwrap();
        let oldest = final_path(temp.path(), 1);
        let newest = final_path(temp.path(), 2);
        write_journal(&oldest, &journal("oldest"));
        write_journal(&newest, &journal("newest"));
        let oldest_name = oldest.file_name().unwrap().to_owned();
        let newest_name = newest.file_name().unwrap().to_owned();
        let state_root = temp.path().to_owned();

        let (event_tx, event_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let (release_second_tx, release_second_rx) = mpsc::channel();

        let first_events = event_tx.clone();
        let first_oldest = oldest_name.clone();
        let first_newest = newest_name.clone();
        let first_root = state_root.clone();
        let first = thread::spawn(move || {
            let mut lock_hook = |name: &OsStr, outcome| {
                if name == first_oldest && outcome == JournalLockOutcome::Acquired {
                    first_events
                        .send(ScheduleEvent::FirstAcquiredOldest)
                        .unwrap();
                    release_first_rx.recv_timeout(WAIT).unwrap();
                } else if name == first_newest && outcome == JournalLockOutcome::Contended {
                    first_events
                        .send(ScheduleEvent::FirstContendedNewest)
                        .unwrap();
                }
            };
            PermissionTransactionStore::at(&first_root)
                .discover_with_lock_hook(RecoveryLimits::default(), &mut lock_hook)
                .unwrap()
        });

        assert_eq!(
            event_rx.recv_timeout(WAIT).unwrap(),
            ScheduleEvent::FirstAcquiredOldest
        );

        let second_events = event_tx;
        let second_oldest = oldest_name;
        let second_newest = newest_name;
        let second = thread::spawn(move || {
            let mut lock_hook = |name: &OsStr, outcome| {
                if name == second_oldest && outcome == JournalLockOutcome::Contended {
                    second_events
                        .send(ScheduleEvent::SecondContendedOldest)
                        .unwrap();
                } else if name == second_newest && outcome == JournalLockOutcome::Acquired {
                    second_events
                        .send(ScheduleEvent::SecondAcquiredNewest)
                        .unwrap();
                    release_second_rx.recv_timeout(WAIT).unwrap();
                }
            };
            let result = PermissionTransactionStore::at(&state_root)
                .discover_with_lock_hook(RecoveryLimits::default(), &mut lock_hook)
                .unwrap();
            second_events.send(ScheduleEvent::SecondReturned).unwrap();
            result
        });

        assert_eq!(
            event_rx.recv_timeout(WAIT).unwrap(),
            ScheduleEvent::SecondContendedOldest
        );
        match event_rx.recv_timeout(WAIT).unwrap() {
            ScheduleEvent::SecondAcquiredNewest => {
                release_first_tx.send(()).unwrap();
                assert_eq!(
                    event_rx.recv_timeout(WAIT).unwrap(),
                    ScheduleEvent::FirstContendedNewest
                );
                release_second_tx.send(()).unwrap();
                assert_eq!(
                    event_rx.recv_timeout(WAIT).unwrap(),
                    ScheduleEvent::SecondReturned
                );
            }
            ScheduleEvent::SecondReturned => release_first_tx.send(()).unwrap(),
            event => panic!("unexpected lock schedule event: {event:?}"),
        }

        let (first_recoverable, first_report) = first.join().unwrap();
        let (second_recoverable, second_report) = second.join().unwrap();

        assert_eq!(
            first_recoverable
                .iter()
                .map(|transaction| transaction.journal().transaction_id.as_str())
                .collect::<Vec<_>>(),
            ["oldest", "newest"]
        );
        assert_eq!(first_report.pending, 2);
        assert_eq!(first_report.active, 0);
        assert!(second_recoverable.is_empty());
        assert_eq!(second_report.active, 1);
        assert_eq!(second_report.invalid, 0);
        assert!(oldest.exists());
        assert!(newest.exists());
    }

    #[test]
    fn discovery_rejects_over_count_without_parsing_or_deleting() {
        let temp = tempfile::tempdir().unwrap();
        let first = final_path(temp.path(), 1);
        let second = final_path(temp.path(), 2);
        write_journal(&first, &journal("first"));
        write_journal(&second, &journal("second"));

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits {
                max_journals: 1,
                max_total_bytes: usize::MAX,
                ..RecoveryLimits::default()
            })
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.over_budget, 1);
        assert_eq!(
            report.over_budget_detail,
            Some(OverBudgetDetail {
                source: OverBudgetSource::JournalCount,
                limit: 1,
            })
        );
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn discovery_rejects_total_byte_overflow_without_parsing_or_deleting() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        write_journal(&path, &journal("byte-limit"));
        let length = fs::metadata(&path).unwrap().len() as usize;

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits {
                max_journals: RecoveryLimits::default().max_journals,
                max_total_bytes: length - 1,
                ..RecoveryLimits::default()
            })
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.over_budget, 1);
        assert_eq!(
            report.over_budget_detail,
            Some(OverBudgetDetail {
                source: OverBudgetSource::JournalBytes,
                limit: length - 1,
            })
        );
        assert!(path.exists());
    }

    #[test]
    fn post_open_byte_preflight_detects_growth_before_decoding_any_entry() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let generated_temporary = temp_path(temp.path(), 1);
        let non_generated = transaction_dir(temp.path()).join("operator-note");
        write_journal(&generated_temporary, &journal("must-remain-before-decode"));
        fs::write(&non_generated, b"x").unwrap();
        let directory = store.open_transaction_directory(false).unwrap().unwrap();
        let directory_lock = DirectoryAdmissionLock::try_lock_shared(&directory.file)
            .unwrap()
            .unwrap();
        let entries = store
            .scan_entries(&directory, RecoveryLimits::default())
            .unwrap();
        let initial_total = entries
            .iter()
            .map(|entry| entry.metadata.len() as usize)
            .sum();
        fs::write(&non_generated, b"xx").unwrap();

        let (recoverable, report) = store
            .discover_entries(
                &directory,
                entries,
                RecoveryLimits {
                    max_journals: RecoveryLimits::default().max_journals,
                    max_total_bytes: initial_total,
                    ..RecoveryLimits::default()
                },
                directory_lock,
            )
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.over_budget, 1);
        assert!(generated_temporary.exists());
    }

    #[cfg(unix)]
    #[test]
    fn post_fstat_byte_preflight_counts_replaced_unopenable_entries() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let path = temp_path(temp.path(), 1);
        create_journal_parent(&path);
        symlink("x", &path).unwrap();
        let directory = store.open_transaction_directory(false).unwrap().unwrap();
        let directory_lock = DirectoryAdmissionLock::try_lock_shared(&directory.file)
            .unwrap()
            .unwrap();
        let entries = store
            .scan_entries(&directory, RecoveryLimits::default())
            .unwrap();
        let initial_total = entries[0].metadata.len() as usize;
        fs::remove_file(&path).unwrap();
        symlink("x".repeat(initial_total + 1), &path).unwrap();

        let (recoverable, report) = store
            .discover_entries(
                &directory,
                entries,
                RecoveryLimits {
                    max_journals: RecoveryLimits::default().max_journals,
                    max_total_bytes: initial_total,
                    ..RecoveryLimits::default()
                },
                directory_lock,
            )
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.over_budget, 1);
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlink_and_retains_it() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.json");
        write_journal(&outside, &journal("outside"));
        let path = final_path(temp.path(), 1);
        create_journal_parent(&path);
        symlink(&outside, &path).unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_dangling_transaction_directory_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let directory = transaction_dir(temp.path());
        fs::create_dir_all(directory.parent().unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            directory.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        symlink(temp.path().join("missing"), &directory).unwrap();

        let result =
            PermissionTransactionStore::at(temp.path()).discover(RecoveryLimits::default());

        assert!(result.is_err());
        assert!(
            fs::symlink_metadata(directory)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn publication_never_replaces_an_existing_final_name() {
        let temp = tempfile::tempdir().unwrap();
        let temporary = temp.path().join("temporary");
        let final_path = temp.path().join("final");
        fs::write(&temporary, b"new").unwrap();
        fs::write(&final_path, b"existing").unwrap();
        let directory = TransactionDirectory {
            file: open_directory(temp.path()).unwrap(),
            path: temp.path().to_owned(),
        };

        let error = publish_journal(
            &directory,
            temporary.file_name().unwrap(),
            final_path.file_name().unwrap(),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&final_path).unwrap(), b"existing");
        assert_eq!(fs::read(&temporary).unwrap(), b"new");
    }

    #[test]
    fn discovery_rejects_hard_links_and_retains_them() {
        let temp = tempfile::tempdir().unwrap();
        let first = final_path(temp.path(), 1);
        let second = final_path(temp.path(), 2);
        write_journal(&first, &journal("hard-link"));
        fs::hard_link(&first, &second).unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 2);
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    fn discovery_rejects_directory_entry_and_retains_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        create_journal_parent(&path);
        fs::create_dir(&path).unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_wrong_mode_and_retains_it() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        write_journal(&path, &journal("wrong-mode"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_wrong_owner_when_runner_can_create_one() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        write_journal(&path, &journal("wrong-owner"));
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(c_path.as_ptr(), 1, u32::MAX) }, 0);

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn discovery_rejects_unsupported_schema_and_retains_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        let mut invalid = journal("unsupported-schema");
        invalid.schema_version += 1;
        write_journal(&path, &invalid);

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn discovery_rejects_mismatched_decision_ids_without_leaking_content() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        let mut invalid = journal("mismatched");
        invalid.terminal.decision_id = Some("different-decision".into());
        write_journal(&path, &invalid);

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(!format!("{report:?}").contains(RAW_COMMAND));
        assert!(path.exists());

        let error = PermissionTransactionStore::at(temp.path())
            .prepare(invalid)
            .unwrap_err();
        assert!(!error.to_string().contains(RAW_COMMAND));
    }

    #[test]
    fn discovery_rejects_destination_path_fields_and_retains_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        let mut value = serde_json::to_value(journal("destination-path")).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("activity_path".into(), serde_json::json!("/tmp/attacker"));
        create_journal_parent(&path);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        file.sync_all().unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn discovery_rejects_nested_destination_path_fields_and_retains_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        let mut value = serde_json::to_value(journal("nested-destination-path")).unwrap();
        value["proposal"]
            .as_object_mut()
            .unwrap()
            .insert("activity_path".into(), serde_json::json!("/tmp/attacker"));
        create_journal_parent(&path);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        file.sync_all().unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn discovery_rejects_oversized_journal_and_retains_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let path = final_path(temp.path(), 1);
        create_journal_parent(&path);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_JOURNAL_BYTES + 1]).unwrap();
        file.sync_all().unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn live_preflight_is_read_only_for_valid_generated_temporary() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_path(temp.path(), 1);
        write_journal(&path, &journal("interrupted-before-publication"));

        let report = PermissionTransactionStore::at(temp.path())
            .preflight_live()
            .unwrap();

        assert_eq!(report.pending, 1);
        assert!(path.exists());
        assert!(!temp.path().join("brain/decisions.jsonl").exists());
        assert!(!temp.path().join("brain/activity.jsonl").exists());
        assert!(!temp.path().join("brain/lifecycle").exists());
    }

    #[test]
    fn live_guard_for_request_a_does_not_remove_request_b_temporary() {
        let temp = tempfile::tempdir().unwrap();
        let request_a = journal("request-a");
        let mut request_b = journal("request-b");
        request_b.request_key = "b".repeat(64);
        let path = temp_path(temp.path(), 1);
        write_journal(&path, &request_b);
        let guard = PermissionRequestLockStore::at(temp.path())
            .try_acquire(&request_a.lifecycle_identity, &request_a.request_key)
            .unwrap()
            .unwrap();

        let report =
            recover_pending_with_guard(temp.path(), RecoveryLimits::live(), &guard).unwrap();

        assert_eq!(report.invalid, 1);
        assert!(path.exists());
        assert!(!decisions_path(temp.path()).exists());
        assert!(!temp.path().join("activity.jsonl").exists());
        assert!(!temp.path().join("brain/lifecycle").exists());
    }

    #[test]
    fn startup_removes_valid_temporary_only_after_acquiring_its_request_guard() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_path(temp.path(), 1);
        write_journal(&path, &journal("guarded-temporary-cleanup"));

        let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

        assert_eq!(report, RecoveryReport::default());
        assert!(!path.exists());
        assert!(!decisions_path(temp.path()).exists());
        assert!(!temp.path().join("activity.jsonl").exists());
        assert!(!temp.path().join("brain/lifecycle").exists());
    }

    #[test]
    fn startup_busy_final_and_temporary_leave_all_state_untouched() {
        for (name, path) in [
            ("busy-final", final_path as fn(&Path, u64) -> PathBuf),
            ("busy-temporary", temp_path as fn(&Path, u64) -> PathBuf),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let journal = journal(name);
            let journal_path = path(temp.path(), 1);
            write_journal(&journal_path, &journal);
            let guard = PermissionRequestLockStore::at(temp.path())
                .try_acquire(&journal.lifecycle_identity, &journal.request_key)
                .unwrap()
                .unwrap();
            let journal_bytes = fs::read(&journal_path).unwrap();

            let report = recover_pending(temp.path(), RecoveryLimits::default()).unwrap();

            assert_eq!(report.active, 1);
            assert_eq!(fs::read(&journal_path).unwrap(), journal_bytes);
            assert!(!decisions_path(temp.path()).exists());
            assert!(!temp.path().join("activity.jsonl").exists());
            assert!(!temp.path().join("brain/lifecycle").exists());
            drop(guard);
        }
    }

    #[test]
    fn locked_generated_temporary_is_active_and_retained() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_path(temp.path(), 1);
        write_journal(&path, &journal("active-preparer"));
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        held.lock_exclusive().unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.active, 1);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn directory_lock_hides_locked_mode_zero_temporary_from_discovery() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let store = PermissionTransactionStore::at(temp.path());
        let directory = store.open_transaction_directory(true).unwrap().unwrap();
        directory.file.lock_exclusive().unwrap();
        let path = temp_path(temp.path(), 1);
        write_journal(&path, &journal("active-before-mode-correction"));
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        held.lock_exclusive().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let contender = store.open_transaction_directory(false).unwrap().unwrap();
        assert_eq!(
            FileExt::try_lock_shared(&contender.file)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(contender);

        let (recoverable, report) = store.discover(RecoveryLimits::default()).unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.active, 1);
        assert_eq!(report.invalid, 0);
        assert!(path.exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o000
        );
        FileExt::unlock(&directory.file).unwrap();
    }

    #[test]
    fn locked_zero_length_generated_temporary_is_active_and_retained() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_path(temp.path(), 1);
        create_journal_parent(&path);
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let held = options.open(&path).unwrap();
        held.lock_exclusive().unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.active, 1);
        assert_eq!(report.invalid, 0);
        assert!(path.exists());
    }

    #[test]
    fn invalid_generated_temporary_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp_path(temp.path(), 1);
        create_journal_parent(&path);
        File::create(&path).unwrap().write_all(b"not-json").unwrap();

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }

    #[test]
    fn similar_looking_but_non_generated_temporary_is_retained() {
        let temp = tempfile::tempdir().unwrap();
        let path = transaction_dir(temp.path()).join("permission-transaction.tmp-user-note");
        write_journal(&path, &journal("user-note"));

        let (recoverable, report) = PermissionTransactionStore::at(temp.path())
            .discover(RecoveryLimits::default())
            .unwrap();

        assert!(recoverable.is_empty());
        assert_eq!(report.invalid, 1);
        assert!(path.exists());
    }
}
