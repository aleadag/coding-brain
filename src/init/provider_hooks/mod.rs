mod antigravity;
mod claude;
mod codex;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::provider::AgentProvider;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TRANSACTION_BYTES: usize = 3 * MAX_FILE_BYTES;
const JOURNAL_SCHEMA_VERSION: u32 = 2;
static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedFileEdit {
    pub path: PathBuf,
    pub original: Option<Vec<u8>>,
    pub original_mode: Option<u32>,
    pub original_hash: Option<String>,
    pub replacement: Vec<u8>,
    pub replacement_hash: String,
}

#[derive(Debug, Clone)]
pub struct ProviderHookPlan {
    pub provider: AgentProvider,
    pub edits: Vec<ManagedFileEdit>,
    pub preserved_modified_entries: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderHookInspection {
    Missing,
    Current,
    Duplicate,
    Stale,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookTransactionJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub edits: Vec<ManagedFileEdit>,
    pub replaced_paths: Vec<PathBuf>,
    pub in_flight: Option<InFlightEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightEdit {
    pub target_path: PathBuf,
    pub temporary_path: PathBuf,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub restored_paths: Vec<PathBuf>,
    pub concurrent_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum ApplyFault {
    None,
    BeforeReplace(usize),
    CrashAfterReplace(usize),
    ChangeAfterPrepare(usize),
    ChangeAfterPrepareToReplacement(usize),
    CrashAfterIntentBeforeRename(usize),
    CrashAfterRenameBeforeJournal(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryFault {
    None,
    ChangeAfterPrepare,
    FailCompletedRollbackAfterTempValidation,
    FailJournalRemoval,
}

enum CommitPreparedError {
    PreconditionFailed {
        error: io::Error,
        prepared: PreparedWrite,
    },
    CommitUncertain {
        error: io::Error,
        prepared: PreparedWrite,
    },
}

impl CommitPreparedError {
    fn into_io(self) -> io::Error {
        match self {
            Self::PreconditionFailed { error, .. } | Self::CommitUncertain { error, .. } => error,
        }
    }
}

pub fn stage_provider_hooks(
    providers: &[AgentProvider],
    scope: HookScope,
) -> io::Result<Vec<ProviderHookPlan>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is required for provider hook setup"))?;
    stage_provider_hooks_at(providers, scope, &home, &std::env::current_dir()?)
}

pub(crate) fn stage_provider_hooks_at(
    providers: &[AgentProvider],
    scope: HookScope,
    home: &Path,
    cwd: &Path,
) -> io::Result<Vec<ProviderHookPlan>> {
    stage_provider_hooks_with(providers, scope, home, cwd, false)
}

pub(crate) fn inspect_provider_hooks_at(
    provider: AgentProvider,
    home: &Path,
    cwd: &Path,
) -> ProviderHookInspection {
    let mut candidates = vec![(HookScope::Global, cwd.to_path_buf())];
    if provider != AgentProvider::Antigravity {
        candidates.extend(
            super::hooks::applicable_project_dirs(Some(home), cwd)
                .into_iter()
                .map(|directory| (HookScope::Project, directory)),
        );
    }
    let mut seen_paths = BTreeSet::new();
    let inspections = candidates
        .into_iter()
        .filter_map(|(scope, directory)| {
            let path = provider_path(provider, scope, home, &directory);
            seen_paths
                .insert(path)
                .then(|| inspect_provider_hook_at(provider, scope, home, &directory))
        })
        .collect::<Vec<_>>();
    if inspections.contains(&ProviderHookInspection::Invalid) {
        return ProviderHookInspection::Invalid;
    }
    if inspections.contains(&ProviderHookInspection::Stale) {
        return ProviderHookInspection::Stale;
    }
    match inspections
        .iter()
        .filter(|state| **state == ProviderHookInspection::Current)
        .count()
    {
        0 => ProviderHookInspection::Missing,
        1 => ProviderHookInspection::Current,
        _ => ProviderHookInspection::Duplicate,
    }
}

fn inspect_provider_hook_at(
    provider: AgentProvider,
    scope: HookScope,
    home: &Path,
    cwd: &Path,
) -> ProviderHookInspection {
    let Ok(mut plans) = stage_provider_hooks_at(&[provider], scope, home, cwd) else {
        return ProviderHookInspection::Invalid;
    };
    let Some(plan) = plans.pop() else {
        return ProviderHookInspection::Invalid;
    };
    if !plan.preserved_modified_entries.is_empty() {
        return ProviderHookInspection::Stale;
    }
    let Some(edit) = plan.edits.first() else {
        return ProviderHookInspection::Current;
    };
    let Some(original) = edit.original.as_deref() else {
        return ProviderHookInspection::Missing;
    };
    let Ok(root) = serde_json::from_slice::<serde_json::Value>(original) else {
        return ProviderHookInspection::Invalid;
    };
    if contains_managed_command(&root) {
        ProviderHookInspection::Stale
    } else {
        ProviderHookInspection::Missing
    }
}

fn contains_managed_command(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(object) => {
            object
                .get("command")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|command| {
                    let mut words = command.split_whitespace();
                    words.next().is_some_and(is_managed_program) && words.any(is_managed_hook_flag)
                })
                || object.values().any(contains_managed_command)
        }
        serde_json::Value::Array(values) => values.iter().any(contains_managed_command),
        _ => false,
    }
}

pub(crate) fn stage_provider_hook_removal(
    providers: &[AgentProvider],
    scope: HookScope,
) -> io::Result<Vec<ProviderHookPlan>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("HOME is required for provider hook setup"))?;
    stage_provider_hooks_with(providers, scope, &home, &std::env::current_dir()?, true)
}

fn stage_provider_hooks_with(
    providers: &[AgentProvider],
    scope: HookScope,
    home: &Path,
    cwd: &Path,
    remove: bool,
) -> io::Result<Vec<ProviderHookPlan>> {
    let mut total = 0usize;
    let mut plans = Vec::new();
    let mut selected = BTreeSet::new();
    for provider in providers
        .iter()
        .copied()
        .filter(|provider| selected.insert(*provider))
    {
        let path = provider_path(provider, scope, home, cwd);
        if !is_safe_absolute(&path) {
            return Err(io::Error::other(format!(
                "provider configuration target must be absolute: {}",
                path.display()
            )));
        }
        let (original, original_mode) = read_managed_file(&path)?;
        if remove && original.is_none() {
            plans.push(ProviderHookPlan {
                provider,
                edits: Vec::new(),
                preserved_modified_entries: Vec::new(),
            });
            continue;
        }
        total = total
            .checked_add(original.as_ref().map_or(0, Vec::len))
            .ok_or_else(|| io::Error::other("selected provider configuration is too large"))?;
        if total > MAX_TRANSACTION_BYTES {
            return Err(io::Error::other(
                "selected provider configuration exceeds the transaction size limit",
            ));
        }
        let mut root = match original.as_deref() {
            Some(bytes) => serde_json::from_slice::<serde_json::Value>(bytes)
                .map_err(|_| invalid_config(&path, "contains invalid JSON"))?,
            None => serde_json::json!({}),
        };
        if !root.is_object() {
            return Err(invalid_config(&path, "must contain a JSON object"));
        }
        let original_root = root.clone();
        let mut preserved = Vec::new();
        match provider {
            AgentProvider::Codex => codex::merge(&mut root, remove, &mut preserved)?,
            AgentProvider::Claude => claude::merge(&mut root, remove, &mut preserved)?,
            AgentProvider::Antigravity => antigravity::merge(&mut root, remove, &mut preserved)?,
        }
        let edits = if root == original_root {
            Vec::new()
        } else {
            let mut replacement = serde_json::to_vec_pretty(&root).map_err(io::Error::other)?;
            replacement.push(b'\n');
            vec![ManagedFileEdit {
                path,
                original_hash: original.as_deref().map(hash_bytes),
                original,
                original_mode,
                replacement_hash: hash_bytes(&replacement),
                replacement,
            }]
        };
        plans.push(ProviderHookPlan {
            provider,
            edits,
            preserved_modified_entries: preserved,
        });
    }
    Ok(plans)
}

pub fn apply_hook_transaction(plans: &[ProviderHookPlan]) -> io::Result<()> {
    let paths = CodingBrainPaths::resolve(&PathEnvironment::current())
        .map_err(|error| io::Error::other(format!("state path resolution failed: {error:?}")))?;
    apply_hook_transaction_at(plans, paths.state_root(), ApplyFault::None)
}

fn apply_hook_transaction_at(
    plans: &[ProviderHookPlan],
    state_root: &Path,
    fault: ApplyFault,
) -> io::Result<()> {
    let pending = recover_hook_transaction_at(state_root)?;
    if !pending.concurrent_paths.is_empty() {
        return Err(io::Error::other(
            "a previous provider-hook transaction encountered concurrent user changes",
        ));
    }
    let edits = plans
        .iter()
        .flat_map(|plan| plan.edits.iter().cloned())
        .collect::<Vec<_>>();
    if edits.is_empty() {
        return Ok(());
    }
    reject_duplicate_paths(&edits)?;
    let mut journal = HookTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        transaction_id: transaction_id(),
        edits,
        replaced_paths: Vec::new(),
        in_flight: None,
    };
    persist_journal(state_root, &journal)?;
    for index in 0..journal.edits.len() {
        if fault == ApplyFault::BeforeReplace(index) {
            return rollback_after_failure(
                state_root,
                io::Error::other("provider hook transaction failed before replacement"),
            );
        }
        let edit = journal.edits[index].clone();
        let prepared = match prepare_atomic_write(&edit.path, &edit.replacement, edit.original_mode)
        {
            Ok(prepared) => prepared,
            Err(error) => return rollback_after_failure(state_root, error),
        };
        journal.in_flight = Some(InFlightEdit {
            target_path: edit.path.clone(),
            temporary_path: prepared.temporary.clone(),
        });
        if let Err(error) = persist_journal(state_root, &journal) {
            return rollback_after_failure(state_root, error);
        }
        if fault == ApplyFault::CrashAfterIntentBeforeRename(index) {
            prepared.retain_for_recovery();
            return Err(io::Error::other("provider hook transaction interrupted"));
        }
        if fault == ApplyFault::ChangeAfterPrepare(index) {
            if let Some(parent) = edit.path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&edit.path, b"{\"concurrent\":true}\n")?;
        }
        if matches!(
            fault,
            ApplyFault::ChangeAfterPrepareToReplacement(failed_index)
                if failed_index == index
        ) {
            fs::write(&edit.path, &edit.replacement)?;
        }
        match commit_prepared_write(prepared, edit.original_hash.as_deref()) {
            Ok(()) => {}
            Err(CommitPreparedError::PreconditionFailed { error, prepared }) => {
                let result = rollback_after_failure(state_root, error);
                drop(prepared);
                return result;
            }
            Err(CommitPreparedError::CommitUncertain { error, prepared }) => {
                let result = rollback_after_failure(state_root, error);
                drop(prepared);
                return result;
            }
        }
        if fault == ApplyFault::CrashAfterRenameBeforeJournal(index) {
            return Err(io::Error::other("provider hook transaction interrupted"));
        }
        journal.replaced_paths.push(edit.path.clone());
        journal.in_flight = None;
        if let Err(error) = persist_journal(state_root, &journal) {
            return rollback_after_failure(state_root, error);
        }
        if fault == ApplyFault::CrashAfterReplace(index) {
            return Err(io::Error::other("provider hook transaction interrupted"));
        }
    }
    match remove_journal(state_root) {
        Ok(()) => Ok(()),
        Err(error) => rollback_after_failure(state_root, error),
    }
}

fn rollback_after_failure(state_root: &Path, cause: io::Error) -> io::Result<()> {
    match recover_hook_transaction_at(state_root) {
        Ok(report) if report.concurrent_paths.is_empty() => Err(cause),
        Ok(_) => Err(io::Error::other(
            "provider hook transaction failed; concurrent user changes were preserved",
        )),
        Err(_) => Err(io::Error::other(
            "provider hook transaction failed and safe recovery is still pending",
        )),
    }
}

pub fn recover_hook_transaction() -> io::Result<RecoveryReport> {
    let paths = CodingBrainPaths::resolve(&PathEnvironment::current())
        .map_err(|error| io::Error::other(format!("state path resolution failed: {error:?}")))?;
    recover_hook_transaction_at(paths.state_root())
}

fn recover_hook_transaction_at(state_root: &Path) -> io::Result<RecoveryReport> {
    recover_hook_transaction_at_with_fault(state_root, RecoveryFault::None)
}

fn recover_hook_transaction_at_with_fault(
    state_root: &Path,
    fault: RecoveryFault,
) -> io::Result<RecoveryReport> {
    let path = journal_path(state_root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RecoveryReport::default());
        }
        Err(error) => return Err(redacted_io("cannot inspect provider hook journal", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(io::Error::other(
            "provider hook journal is not a regular file",
        ));
    }
    let bytes =
        fs::read(&path).map_err(|error| redacted_io("cannot read provider hook journal", error))?;
    let journal: HookTransactionJournal = serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::other("provider hook journal is invalid"))?;
    validate_journal(&journal)?;
    let mut report = RecoveryReport::default();
    let mut prepared_cleanup = None;
    if let Some(in_flight) = &journal.in_flight {
        let edit = journal
            .edits
            .iter()
            .find(|edit| edit.path == in_flight.target_path)
            .expect("journal membership validated");
        match prepared_temporary_state(&in_flight.temporary_path, edit)? {
            PreparedTemporaryState::Present => {
                prepared_cleanup = Some(in_flight.temporary_path.clone());
            }
            PreparedTemporaryState::Missing => recover_replaced_edit(edit, fault, &mut report)?,
        }
    }
    if prepared_cleanup.is_some()
        && fault == RecoveryFault::FailCompletedRollbackAfterTempValidation
    {
        return Err(io::Error::other(
            "injected completed provider rollback failure",
        ));
    }
    for path in journal.replaced_paths.iter().rev() {
        let edit = journal
            .edits
            .iter()
            .find(|edit| &edit.path == path)
            .expect("journal membership validated");
        recover_replaced_edit(edit, fault, &mut report)?;
    }
    if fault == RecoveryFault::FailJournalRemoval {
        return Err(io::Error::other(
            "injected provider hook journal removal failure",
        ));
    }
    remove_journal(state_root)?;
    if let Some(temporary) = prepared_cleanup {
        match fs::remove_file(&temporary) {
            Ok(()) => sync_parent(&temporary)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(redacted_io(
                    "provider hook transaction recovered but temporary cleanup failed",
                    error,
                ));
            }
        }
    }
    Ok(report)
}

fn recover_replaced_edit(
    edit: &ManagedFileEdit,
    fault: RecoveryFault,
    report: &mut RecoveryReport,
) -> io::Result<()> {
    let current = current_hash(&edit.path)?;
    if current.as_deref() == Some(edit.replacement_hash.as_str()) {
        restore_original(edit, fault)?;
        report.restored_paths.push(edit.path.clone());
    } else if current != edit.original_hash {
        report.concurrent_paths.push(edit.path.clone());
    }
    Ok(())
}

fn validate_journal(journal: &HookTransactionJournal) -> io::Result<()> {
    if journal.schema_version != JOURNAL_SCHEMA_VERSION {
        return Err(io::Error::other(
            "provider hook journal schema is unsupported",
        ));
    }
    reject_duplicate_paths(&journal.edits)?;
    let edit_paths = journal
        .edits
        .iter()
        .map(|edit| edit.path.clone())
        .collect::<BTreeSet<_>>();
    if journal
        .edits
        .iter()
        .any(|edit| !is_safe_absolute(&edit.path))
        || journal
            .replaced_paths
            .iter()
            .any(|path| !is_safe_absolute(path))
        || journal.in_flight.as_ref().is_some_and(|in_flight| {
            !is_safe_absolute(&in_flight.target_path)
                || !is_safe_absolute(&in_flight.temporary_path)
        })
    {
        return Err(io::Error::other(
            "provider hook journal contains a relative path",
        ));
    }
    let replaced = journal
        .replaced_paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if replaced.len() != journal.replaced_paths.len()
        || !replaced.is_subset(&edit_paths)
        || journal.in_flight.as_ref().is_some_and(|in_flight| {
            !edit_paths.contains(&in_flight.target_path)
                || replaced.contains(&in_flight.target_path)
                || !is_expected_temporary_sibling(&in_flight.target_path, &in_flight.temporary_path)
        })
    {
        return Err(io::Error::other(
            "provider hook journal state is inconsistent",
        ));
    }
    for edit in &journal.edits {
        if edit.original.as_deref().map(hash_bytes) != edit.original_hash
            || hash_bytes(&edit.replacement) != edit.replacement_hash
        {
            return Err(io::Error::other(
                "provider hook journal integrity check failed",
            ));
        }
    }
    Ok(())
}

fn provider_path(provider: AgentProvider, scope: HookScope, home: &Path, cwd: &Path) -> PathBuf {
    match (provider, scope) {
        (AgentProvider::Codex, HookScope::Global) => home.join(".codex/hooks.json"),
        (AgentProvider::Codex, HookScope::Project) => cwd.join(".codex/hooks.json"),
        (AgentProvider::Claude, HookScope::Global) => home.join(".claude/settings.json"),
        (AgentProvider::Claude, HookScope::Project) => cwd.join(".claude/settings.json"),
        (AgentProvider::Antigravity, _) => home.join(".gemini/config/hooks.json"),
    }
}

fn read_managed_file(path: &Path) -> io::Result<(Option<Vec<u8>>, Option<u32>)> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => return Err(redacted_io("cannot inspect provider configuration", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(invalid_config(path, "must be a regular non-symlink file"));
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(invalid_config(path, "exceeds the 1 MiB input limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(invalid_config(path, "exceeds the 1 MiB input limit"));
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = None;
    Ok((Some(bytes), mode))
}

fn current_hash(path: &Path) -> io::Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            fs::read(path).map(|bytes| Some(hash_bytes(&bytes)))
        }
        Ok(_) => Err(invalid_config(
            path,
            "is no longer a regular non-symlink file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(redacted_io("cannot inspect provider configuration", error)),
    }
}

enum PreparedTemporaryState {
    Present,
    Missing,
}

fn prepared_temporary_state(
    path: &Path,
    edit: &ManagedFileEdit,
) -> io::Result<PreparedTemporaryState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PreparedTemporaryState::Missing);
        }
        Err(error) => return Err(redacted_io("cannot inspect prepared provider file", error)),
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(io::Error::other(
            "prepared provider configuration is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o600 {
            return Err(io::Error::other(
                "prepared provider configuration is not owner-only",
            ));
        }
    }
    if metadata.len() != edit.replacement.len() as u64
        || current_hash(path)?.as_deref() != Some(edit.replacement_hash.as_str())
    {
        return Err(io::Error::other(
            "prepared provider configuration failed integrity validation",
        ));
    }
    Ok(PreparedTemporaryState::Present)
}

fn restore_original(edit: &ManagedFileEdit, fault: RecoveryFault) -> io::Result<()> {
    if let Some(original) = &edit.original {
        let prepared = prepare_atomic_write(&edit.path, original, edit.original_mode)?;
        if fault == RecoveryFault::ChangeAfterPrepare {
            fs::write(&edit.path, b"{\"concurrent-recovery\":true}\n")?;
        }
        commit_prepared_write(prepared, Some(&edit.replacement_hash))
            .map_err(CommitPreparedError::into_io)
    } else {
        if current_hash(&edit.path)?.as_deref() != Some(edit.replacement_hash.as_str()) {
            return Err(io::Error::other(format!(
                "provider configuration changed during recovery: {}",
                edit.path.display()
            )));
        }
        match fs::remove_file(&edit.path) {
            Ok(()) => sync_parent(&edit.path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(redacted_io("cannot restore provider configuration", error)),
        }
    }
}

struct PreparedWrite {
    target: PathBuf,
    temporary: PathBuf,
    final_mode: Option<u32>,
    cleanup: bool,
}

impl Drop for PreparedWrite {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

impl PreparedWrite {
    fn retain_for_recovery(mut self) {
        self.cleanup = false;
    }
}

fn prepare_atomic_write(path: &Path, bytes: &[u8], mode: Option<u32>) -> io::Result<PreparedWrite> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("provider path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = unique_sibling(path, "tmp");
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(PreparedWrite {
            target: path.to_path_buf(),
            temporary: temporary.clone(),
            final_mode: mode,
            cleanup: true,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| redacted_io("cannot prepare provider configuration", error))
}

fn commit_prepared_write(
    mut prepared: PreparedWrite,
    expected_hash: Option<&str>,
) -> Result<(), CommitPreparedError> {
    let current = match current_hash(&prepared.target) {
        Ok(current) => current,
        Err(error) => {
            return Err(CommitPreparedError::PreconditionFailed { error, prepared });
        }
    };
    if current.as_deref() != expected_hash {
        return Err(CommitPreparedError::PreconditionFailed {
            error: io::Error::other(format!(
                "provider configuration changed before replacement: {}",
                prepared.target.display()
            )),
            prepared,
        });
    }
    let commit_result = (|| {
        fs::rename(&prepared.temporary, &prepared.target)?;
        #[cfg(unix)]
        if let Some(mode) = prepared.final_mode {
            use std::os::unix::fs::PermissionsExt;
            let target_file = File::open(&prepared.target)?;
            fs::set_permissions(&prepared.target, fs::Permissions::from_mode(mode))?;
            target_file.sync_all()?;
        }
        sync_parent(&prepared.target)
    })();
    if let Err(error) = commit_result {
        return Err(CommitPreparedError::CommitUncertain {
            error: redacted_io("cannot replace provider configuration", error),
            prepared,
        });
    }
    prepared.cleanup = false;
    Ok(())
}

fn persist_journal(state_root: &Path, journal: &HookTransactionJournal) -> io::Result<()> {
    let path = journal_path(state_root);
    fs::create_dir_all(path.parent().expect("journal has a parent"))?;
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| io::Error::other("cannot encode provider hook journal"))?;
    let temporary = unique_sibling(&path, "journal.tmp");
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        sync_parent(&path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| redacted_io("cannot persist provider hook journal", error))
}

fn remove_journal(state_root: &Path) -> io::Result<()> {
    let path = journal_path(state_root);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent(&path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(redacted_io("cannot remove provider hook journal", error)),
    }
}

fn journal_path(state_root: &Path) -> PathBuf {
    state_root.join("brain/hook-install-transaction.json")
}
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("path has no parent"))?,
    )?
    .sync_all()
}
fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
    path.with_extension(format!(
        "{suffix}.{}.{}",
        std::process::id(),
        TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}
fn is_expected_temporary_sibling(target: &Path, temporary: &Path) -> bool {
    if target.parent() != temporary.parent() || target == temporary {
        return false;
    }
    let Some(prefix) = target
        .with_extension("tmp")
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}."))
    else {
        return false;
    };
    let Some(suffix) = temporary
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(&prefix))
    else {
        return false;
    };
    let mut parts = suffix.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(pid), Some(counter), None)
            if pid.parse::<u32>().is_ok() && counter.parse::<u64>().is_ok()
    )
}
fn is_safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}
fn transaction_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}
fn reject_duplicate_paths(edits: &[ManagedFileEdit]) -> io::Result<()> {
    let mut paths = BTreeSet::new();
    if edits.iter().all(|edit| paths.insert(edit.path.clone())) {
        Ok(())
    } else {
        Err(io::Error::other(
            "provider hook transaction contains duplicate paths",
        ))
    }
}
fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn invalid_config(path: &Path, reason: &str) -> io::Error {
    io::Error::other(format!(
        "provider configuration {} {reason}",
        path.display()
    ))
}
fn redacted_io(context: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), context.to_owned())
}

#[derive(Clone, Copy)]
struct HookDefinition {
    event: &'static str,
    matcher: Option<&'static str>,
    flag: &'static str,
    timeout: u64,
    status_message: Option<&'static str>,
}
impl HookDefinition {
    const fn nested(
        event: &'static str,
        matcher: Option<&'static str>,
        flag: &'static str,
        timeout: u64,
    ) -> Self {
        Self {
            event,
            matcher,
            flag,
            timeout,
            status_message: None,
        }
    }
    const fn permission(event: &'static str, flag: &'static str) -> Self {
        Self {
            event,
            matcher: Some("*"),
            flag,
            timeout: 30,
            status_message: Some("Brain reviewing permission…"),
        }
    }
}

fn merge_nested_hooks(
    root: &mut serde_json::Value,
    provider: &'static str,
    definitions: &[HookDefinition],
    remove: bool,
    accept_legacy: bool,
    preserved: &mut Vec<String>,
) -> io::Result<()> {
    let object = root.as_object_mut().expect("root validated as object");
    if let Some(hooks) = object.get("hooks") {
        validate_nested_hooks(hooks)?;
    }
    if remove && !object.contains_key("hooks") {
        return Ok(());
    }
    let mut removed_exact = false;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .expect("hooks validated");
    for definition in definitions {
        let mut collision = false;
        if let Some(matchers) = hooks
            .get_mut(definition.event)
            .and_then(serde_json::Value::as_array_mut)
        {
            for matcher in matchers.iter_mut() {
                let matcher_object = matcher.as_object_mut().expect("shape validated");
                let matcher_is_exact = matcher_object
                    .keys()
                    .all(|key| key == "matcher" || key == "hooks")
                    && matcher_object
                        .get("matcher")
                        .and_then(serde_json::Value::as_str)
                        == definition.matcher
                    && (definition.matcher.is_some() || !matcher_object.contains_key("matcher"));
                let handlers = matcher_object
                    .get_mut("hooks")
                    .and_then(serde_json::Value::as_array_mut)
                    .expect("shape validated");
                handlers.retain(|handler| {
                    let managed = handler
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|command| {
                            command_targets_provider(command, provider, accept_legacy)
                                && command.split_whitespace().any(is_managed_hook_flag)
                        });
                    if !managed {
                        return true;
                    }
                    let exact = matcher_is_exact
                        && handler_is_exact(handler, provider, definition, accept_legacy);
                    if !exact {
                        collision = true;
                        preserved.push(format!("{provider}:{}", definition.event));
                    } else {
                        removed_exact = true;
                    }
                    !exact
                });
            }
            matchers.retain(|matcher| {
                matcher
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .is_none_or(|handlers| !handlers.is_empty())
            });
        }
        if !remove && !collision {
            hooks
                .entry(definition.event)
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .expect("shape validated")
                .push(nested_matcher(provider, definition));
        }
    }
    hooks.retain(|_, value| value.as_array().is_none_or(|entries| !entries.is_empty()));
    if hooks.is_empty() && removed_exact {
        object.remove("hooks");
    }
    preserved.sort();
    preserved.dedup();
    Ok(())
}

fn is_managed_hook_flag(word: &str) -> bool {
    matches!(
        word,
        "--lifecycle-hook" | "--permission-hook" | "--recovery-hook"
    )
}

fn validate_nested_hooks(hooks: &serde_json::Value) -> io::Result<()> {
    let object = hooks
        .as_object()
        .ok_or_else(|| io::Error::other("provider hooks must be a JSON object"))?;
    for matchers in object.values() {
        for matcher in matchers
            .as_array()
            .ok_or_else(|| io::Error::other("provider hook events must be arrays"))?
        {
            let matcher = matcher
                .as_object()
                .ok_or_else(|| io::Error::other("provider hook matcher must be an object"))?;
            let handlers = matcher
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| io::Error::other("provider hook handlers must be arrays"))?;
            if handlers.iter().any(|handler| !handler.is_object()) {
                return Err(io::Error::other("provider hook handler must be an object"));
            }
        }
    }
    Ok(())
}

fn nested_matcher(provider: &str, definition: &HookDefinition) -> serde_json::Value {
    let mut value = serde_json::json!({ "hooks": [nested_handler(provider, definition)] });
    if let Some(matcher) = definition.matcher {
        value["matcher"] = serde_json::Value::String(matcher.to_owned());
    }
    value
}
fn nested_handler(provider: &str, definition: &HookDefinition) -> serde_json::Value {
    let mut value = serde_json::json!({
        "type": "command", "command": format!("{} {} --provider {provider}", managed_executable().display(), definition.flag), "timeout": definition.timeout
    });
    if let Some(message) = definition.status_message {
        value["statusMessage"] = serde_json::Value::String(message.to_owned());
    }
    value
}
fn handler_is_exact(
    handler: &serde_json::Value,
    provider: &str,
    definition: &HookDefinition,
    accept_legacy: bool,
) -> bool {
    let Some(object) = handler.as_object() else {
        return false;
    };
    let expected_fields = if definition.status_message.is_some() {
        4
    } else {
        3
    };
    if object.len() != expected_fields
        || object.get("type").and_then(serde_json::Value::as_str) != Some("command")
        || object.get("timeout").and_then(serde_json::Value::as_u64) != Some(definition.timeout)
        || object
            .get("statusMessage")
            .and_then(serde_json::Value::as_str)
            != definition.status_message
    {
        return false;
    }
    let Some(command) = object.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    exact_command(command, &[definition.flag, "--provider", provider])
        || (accept_legacy && exact_command(command, &[definition.flag]))
}
#[cfg(not(test))]
fn managed_executable() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("coding-brain"))
}
#[cfg(test)]
fn managed_executable() -> PathBuf {
    PathBuf::from("coding-brain")
}
fn command_targets_provider(command: &str, provider: &str, accept_legacy: bool) -> bool {
    let words = command.split_whitespace().collect::<Vec<_>>();
    if !words
        .first()
        .is_some_and(|program| is_managed_program(program))
    {
        return false;
    }
    match words.windows(2).find(|pair| pair[0] == "--provider") {
        Some(pair) => pair[1] == provider,
        None => accept_legacy,
    }
}
fn exact_command(command: &str, arguments: &[&str]) -> bool {
    let mut words = command.split_whitespace();
    words.next().is_some_and(is_managed_program) && words.eq(arguments.iter().copied())
}
fn is_managed_program(program: &str) -> bool {
    matches!(program, "coding-brain" | "codexctl")
        || Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "coding-brain" | "codexctl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_brain_core::provider::AgentProvider;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    #[test]
    fn stages_all_provider_files_before_applying() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(home.join(".codex/hooks.json"), b"{\"keep\":\"codex\"}\n").unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            b"not json SECRET-CLAUDE",
        )
        .unwrap();

        let error = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap_err();

        assert!(!error.to_string().contains("SECRET-CLAUDE"));
        assert_eq!(
            std::fs::read(home.join(".codex/hooks.json")).unwrap(),
            b"{\"keep\":\"codex\"}\n"
        );
    }

    #[test]
    fn provider_merges_preserve_unrelated_config_and_never_add_statusline() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
        std::fs::write(
            home.join(".codex/hooks.json"),
            br#"{"keep":1,"hooks":{"Stop":[{"disabled":true,"hooks":[{"command":"external"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            br#"{"statusLine":{"command":"mine"},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"external"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".gemini/config/hooks.json"),
            br#"{"title":{"command":"mine"},"external":{"enabled":false,"Stop":[{"command":"external"}]}}"#,
        )
        .unwrap();

        let plans = stage_provider_hooks_at(
            &[
                AgentProvider::Codex,
                AgentProvider::Claude,
                AgentProvider::Antigravity,
            ],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();

        assert_eq!(plans.len(), 3);
        let codex = replacement_json(&plans, AgentProvider::Codex);
        assert_eq!(codex["keep"], 1);
        assert_eq!(codex["hooks"]["Stop"][0]["disabled"], true);
        assert_eq!(codex["hooks"]["Stop"][0]["hooks"][0]["command"], "external");
        let claude = replacement_json(&plans, AgentProvider::Claude);
        assert_eq!(claude["statusLine"]["command"], "mine");
        assert_eq!(claude["hooks"]["PreToolUse"][0]["matcher"], "Bash");
        assert!(!claude.to_string().contains("--status"));
        let antigravity = replacement_json(&plans, AgentProvider::Antigravity);
        assert_eq!(antigravity["title"]["command"], "mine");
        assert_eq!(antigravity["external"]["enabled"], false);
        assert!(antigravity.get("coding-brain").is_some());
        assert!(!antigravity.to_string().contains("statusLine"));
    }

    #[test]
    fn exact_entries_are_idempotent_and_modified_entries_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Claude], HookScope::Global, &home, &project)
                .unwrap();
        apply_hook_transaction_at(&plans, &temp.path().join("state"), ApplyFault::None).unwrap();
        let first = std::fs::read(home.join(".claude/settings.json")).unwrap();
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Claude], HookScope::Global, &home, &project)
                .unwrap();
        assert!(plans[0].edits.is_empty());

        let mut value: serde_json::Value = serde_json::from_slice(&first).unwrap();
        value["hooks"]["Stop"][0]["hooks"][0]["command"] =
            serde_json::json!("coding-brain --recovery-hook --provider claude --user-option");
        std::fs::write(
            home.join(".claude/settings.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Claude], HookScope::Global, &home, &project)
                .unwrap();
        assert!(plans[0].edits.is_empty());
        assert_eq!(
            std::fs::read(home.join(".claude/settings.json")).unwrap(),
            serde_json::to_vec_pretty(&value).unwrap()
        );
        assert!(!plans[0].preserved_modified_entries.is_empty());
    }

    #[test]
    fn modified_nested_definition_blocks_an_active_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let path = home.join(".claude/settings.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let value = serde_json::json!({
            "hooks": {"Stop": [{"disabled": true, "hooks": [{
                "type": "command",
                "command": "coding-brain --recovery-hook --provider claude --user-option",
                "timeout": 30
            }]}]}
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let plans =
            stage_provider_hooks_at(&[AgentProvider::Claude], HookScope::Global, &home, &project)
                .unwrap();
        let replacement = replacement_json(&plans, AgentProvider::Claude);
        let stop = replacement["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["disabled"], true);
        assert!(
            plans[0]
                .preserved_modified_entries
                .contains(&"claude:Stop".to_owned())
        );
    }

    #[test]
    fn each_nested_collision_shape_suppresses_the_active_definition() {
        let cases = [
            serde_json::json!({"hooks": [{
                "type": "command",
                "command": "coding-brain --recovery-hook --provider claude --extra",
                "timeout": 30
            }]}),
            serde_json::json!({"matcher": "narrowed", "hooks": [{
                "type": "command",
                "command": "coding-brain --recovery-hook --provider claude",
                "timeout": 30
            }]}),
            serde_json::json!({"hooks": [{
                "type": "command",
                "command": "coding-brain --recovery-hook --provider claude",
                "timeout": 30,
                "disabled": true
            }]}),
        ];
        for collision in cases {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let path = home.join(".claude/settings.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                serde_json::to_vec(&serde_json::json!({"hooks": {"Stop": [collision]}})).unwrap(),
            )
            .unwrap();

            let plans = stage_provider_hooks_at(
                &[AgentProvider::Claude],
                HookScope::Global,
                &home,
                &temp.path().join("project"),
            )
            .unwrap();
            let replacement = replacement_json(&plans, AgentProvider::Claude);
            let stop = replacement["hooks"]["Stop"].as_array().unwrap();
            assert_eq!(stop.len(), 1);
            assert!(
                plans[0]
                    .preserved_modified_entries
                    .contains(&"claude:Stop".to_owned())
            );
        }
    }

    #[test]
    fn claude_session_start_includes_compact() {
        let temp = tempfile::tempdir().unwrap();
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Claude],
            HookScope::Global,
            &temp.path().join("home"),
            &temp.path().join("project"),
        )
        .unwrap();
        assert_eq!(
            replacement_json(&plans, AgentProvider::Claude)["hooks"]["SessionStart"][0]["matcher"],
            "startup|resume|clear|compact"
        );
    }

    #[test]
    fn staging_rejects_relative_targets() {
        assert!(
            stage_provider_hooks_at(
                &[AgentProvider::Claude],
                HookScope::Global,
                Path::new("relative-home"),
                Path::new("relative-project"),
            )
            .is_err()
        );
    }

    #[test]
    fn antigravity_modified_named_entry_is_preserved_and_exact_entry_is_removable() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let path = home.join(".gemini/config/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let modified = serde_json::json!({
            "coding-brain": {
                "Stop": [{"type":"command","command":"coding-brain --recovery-hook --provider antigravity --user-option","timeout":30}]
            },
            "external": {"enabled": false}
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&modified).unwrap()).unwrap();

        let plans = stage_provider_hooks_at(
            &[AgentProvider::Antigravity],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        assert!(plans[0].edits.is_empty());
        assert_eq!(
            plans[0].preserved_modified_entries,
            vec!["antigravity:coding-brain"]
        );

        std::fs::remove_file(&path).unwrap();
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Antigravity],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        apply_hook_transaction_at(&plans, &temp.path().join("state"), ApplyFault::None).unwrap();
        let removal = stage_provider_hooks_with(
            &[AgentProvider::Antigravity],
            HookScope::Global,
            &home,
            &project,
            true,
        )
        .unwrap();
        let replacement = replacement_json(&removal, AgentProvider::Antigravity);
        assert!(replacement.get("coding-brain").is_none());
    }

    #[test]
    fn removing_from_a_missing_provider_file_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans = stage_provider_hooks_with(
            &[AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
            true,
        )
        .unwrap();

        apply_hook_transaction_at(&plans, &temp.path().join("state"), ApplyFault::None).unwrap();

        assert!(!home.join(".claude/settings.json").exists());
    }

    #[test]
    fn removing_absent_managed_entries_preserves_user_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let path = home.join(".claude/settings.json");
        let original = b"{\"user\": true, \"hooks\": {}}\n";
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, original).unwrap();

        let plans = stage_provider_hooks_with(
            &[AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
            true,
        )
        .unwrap();
        assert!(plans[0].edits.is_empty());
        apply_hook_transaction_at(&plans, &temp.path().join("state"), ApplyFault::None).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn rejects_wrong_shapes_oversize_symlinks_and_non_regular_files() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/hooks.json"), b"[]").unwrap();
        assert!(
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .is_err()
        );

        std::fs::write(
            home.join(".codex/hooks.json"),
            vec![b' '; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .is_err()
        );

        std::fs::remove_file(home.join(".codex/hooks.json")).unwrap();
        std::fs::create_dir(home.join(".codex/hooks.json")).unwrap();
        assert!(
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .is_err()
        );

        #[cfg(unix)]
        {
            std::fs::remove_dir(home.join(".codex/hooks.json")).unwrap();
            std::os::unix::fs::symlink(home.join("target"), home.join(".codex/hooks.json"))
                .unwrap();
            assert!(
                stage_provider_hooks_at(
                    &[AgentProvider::Codex],
                    HookScope::Global,
                    &home,
                    &project
                )
                .is_err()
            );
        }

        std::fs::create_dir_all(home.join(".gemini/config")).unwrap();
        std::fs::write(home.join(".gemini/config/hooks.json"), b"{\"broken\":[]}").unwrap();
        assert!(
            stage_provider_hooks_at(
                &[AgentProvider::Antigravity],
                HookScope::Global,
                &home,
                &project,
            )
            .is_err()
        );
    }

    #[test]
    fn failure_rolls_back_replacements_byte_for_byte_and_restores_mode() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let codex_path = home.join(".codex/hooks.json");
        let original = b"{\"secret\":\"ROLLBACK-SECRET\"}\n";
        std::fs::write(&codex_path, original).unwrap();
        std::fs::write(home.join(".claude/settings.json"), b"{}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&codex_path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let state = temp.path().join("state");
        let error =
            apply_hook_transaction_at(&plans, &state, ApplyFault::BeforeReplace(1)).unwrap_err();
        assert!(!error.to_string().contains("ROLLBACK-SECRET"));
        assert_eq!(std::fs::read(&codex_path).unwrap(), original);
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&codex_path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!journal_path(&state).exists());
    }

    #[test]
    fn crash_journal_is_private_and_next_entry_recovers() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let path = home.join(".codex/hooks.json");
        std::fs::write(&path, b"{\"secret\":\"CRASH-SECRET\"}\n").unwrap();
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .unwrap();
        let state = temp.path().join("state");
        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterReplace(0)).is_err()
        );
        assert!(journal_path(&state).exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(journal_path(&state))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let report = recover_hook_transaction_at(&state).unwrap();
        assert!(report.concurrent_paths.is_empty());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"{\"secret\":\"CRASH-SECRET\"}\n"
        );
        assert!(!journal_path(&state).exists());
    }

    #[test]
    fn recovery_preserves_a_concurrent_user_edit() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .unwrap();
        let state = temp.path().join("state");
        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterReplace(0)).is_err()
        );
        let path = home.join(".codex/hooks.json");
        std::fs::write(&path, b"{\"concurrent\":true}\n").unwrap();
        let report = recover_hook_transaction_at(&state).unwrap();
        assert_eq!(report.concurrent_paths, vec![path.clone()]);
        assert_eq!(std::fs::read(path).unwrap(), b"{\"concurrent\":true}\n");
        assert!(!journal_path(&state).exists());
    }

    #[test]
    fn recovery_preserves_a_change_made_during_restore_preparation() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let path = home.join(".codex/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"original\":true}\n").unwrap();
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .unwrap();
        let state = temp.path().join("state");
        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterReplace(0),).is_err()
        );

        assert!(
            recover_hook_transaction_at_with_fault(&state, RecoveryFault::ChangeAfterPrepare,)
                .is_err()
        );
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"{\"concurrent-recovery\":true}\n"
        );
        assert!(journal_path(&state).exists());
    }

    #[test]
    fn recovery_rejects_relative_journal_paths() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let replacement = b"replacement".to_vec();
        let edit = ManagedFileEdit {
            path: PathBuf::from("relative.json"),
            original: None,
            original_mode: None,
            original_hash: None,
            replacement_hash: hash_bytes(&replacement),
            replacement,
        };
        persist_journal(
            &state,
            &HookTransactionJournal {
                schema_version: JOURNAL_SCHEMA_VERSION,
                transaction_id: "test".to_owned(),
                edits: vec![edit],
                replaced_paths: vec![PathBuf::from("relative.json")],
                in_flight: None,
            },
        )
        .unwrap();

        assert!(recover_hook_transaction_at(&state).is_err());
        assert!(journal_path(&state).exists());
    }

    #[test]
    fn recovery_rejects_crafted_in_flight_temporary_paths() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let target = temp.path().join("home/.codex/hooks.json");
        let replacement = b"replacement".to_vec();
        let edit = ManagedFileEdit {
            path: target.clone(),
            original: None,
            original_mode: None,
            original_hash: None,
            replacement_hash: hash_bytes(&replacement),
            replacement,
        };
        persist_journal(
            &state,
            &HookTransactionJournal {
                schema_version: JOURNAL_SCHEMA_VERSION,
                transaction_id: "test".to_owned(),
                edits: vec![edit],
                replaced_paths: Vec::new(),
                in_flight: Some(InFlightEdit {
                    target_path: target,
                    temporary_path: temp.path().join("outside/not-a-generated-sibling"),
                }),
            },
        )
        .unwrap();

        assert!(recover_hook_transaction_at(&state).is_err());
        assert!(journal_path(&state).exists());
    }

    #[test]
    fn recovery_rejects_relative_in_flight_temporary_paths() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let target = temp.path().join("home/.codex/hooks.json");
        let replacement = b"replacement".to_vec();
        let edit = ManagedFileEdit {
            path: target.clone(),
            original: None,
            original_mode: None,
            original_hash: None,
            replacement_hash: hash_bytes(&replacement),
            replacement,
        };
        persist_journal(
            &state,
            &HookTransactionJournal {
                schema_version: JOURNAL_SCHEMA_VERSION,
                transaction_id: "test".to_owned(),
                edits: vec![edit],
                replaced_paths: Vec::new(),
                in_flight: Some(InFlightEdit {
                    target_path: target,
                    temporary_path: PathBuf::from("relative.tmp.1.1"),
                }),
            },
        )
        .unwrap();

        assert!(recover_hook_transaction_at(&state).is_err());
        assert!(journal_path(&state).exists());
    }

    #[test]
    fn apply_precondition_preserves_a_change_made_after_staging() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .unwrap();
        let path = home.join(".codex/hooks.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{\"concurrent\":true}\n").unwrap();
        let state = temp.path().join("state");

        assert!(apply_hook_transaction_at(&plans, &state, ApplyFault::None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"concurrent\":true}\n");
        assert!(!journal_path(&state).exists());
    }

    #[test]
    fn apply_preserves_a_change_made_during_temporary_file_preparation() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans =
            stage_provider_hooks_at(&[AgentProvider::Codex], HookScope::Global, &home, &project)
                .unwrap();
        let path = home.join(".codex/hooks.json");
        let state = temp.path().join("state");
        let error = apply_hook_transaction_at(&plans, &state, ApplyFault::ChangeAfterPrepare(0))
            .unwrap_err();
        assert!(error.to_string().contains("concurrent") || error.to_string().contains("changed"));
        assert_eq!(std::fs::read(path).unwrap(), b"{\"concurrent\":true}\n");
    }

    #[test]
    fn exact_replacement_written_concurrently_is_not_rolled_back() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let codex_path = home.join(".codex/hooks.json");
        let claude_path = home.join(".claude/settings.json");
        std::fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
        std::fs::write(&codex_path, b"{\"codex-original\":true}\n").unwrap();
        std::fs::write(&claude_path, b"{\"claude-original\":true}\n").unwrap();
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let concurrent = plans[1].edits[0].replacement.clone();
        let state = temp.path().join("state");

        assert!(
            apply_hook_transaction_at(
                &plans,
                &state,
                ApplyFault::ChangeAfterPrepareToReplacement(1),
            )
            .is_err()
        );

        assert_eq!(
            std::fs::read(codex_path).unwrap(),
            b"{\"codex-original\":true}\n"
        );
        assert_eq!(std::fs::read(claude_path).unwrap(), concurrent);
        assert!(!journal_path(&state).exists());
    }

    #[test]
    fn recovery_failure_after_temp_validation_retains_proof_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let second_replacement = plans[1].edits[0].replacement.clone();
        let first_path = plans[0].edits[0].path.clone();
        let second_path = plans[1].edits[0].path.clone();
        let state = temp.path().join("state");

        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterIntentBeforeRename(1),)
                .is_err()
        );
        std::fs::write(&second_path, &second_replacement).unwrap();

        assert!(
            recover_hook_transaction_at_with_fault(
                &state,
                RecoveryFault::FailCompletedRollbackAfterTempValidation,
            )
            .is_err()
        );
        let journal: HookTransactionJournal =
            serde_json::from_slice(&std::fs::read(journal_path(&state)).unwrap()).unwrap();
        let proof = journal.in_flight.unwrap().temporary_path;
        assert!(proof.exists());
        assert!(first_path.exists(), "completed rollback has not run yet");
        assert_eq!(std::fs::read(&second_path).unwrap(), second_replacement);

        recover_hook_transaction_at(&state).unwrap();

        assert!(!first_path.exists());
        assert_eq!(std::fs::read(second_path).unwrap(), second_replacement);
        assert!(!journal_path(&state).exists());
        assert!(!proof.exists());
    }

    #[test]
    fn journal_removal_failure_retains_temp_proof_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let first_path = plans[0].edits[0].path.clone();
        let second_path = plans[1].edits[0].path.clone();
        let second_replacement = plans[1].edits[0].replacement.clone();
        let state = temp.path().join("state");
        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterIntentBeforeRename(1),)
                .is_err()
        );
        std::fs::write(&second_path, &second_replacement).unwrap();

        assert!(
            recover_hook_transaction_at_with_fault(&state, RecoveryFault::FailJournalRemoval,)
                .is_err()
        );
        let journal: HookTransactionJournal =
            serde_json::from_slice(&std::fs::read(journal_path(&state)).unwrap()).unwrap();
        let proof = journal.in_flight.unwrap().temporary_path;
        assert!(proof.exists());
        assert!(
            !first_path.exists(),
            "completed rollback happened before removal failed"
        );
        assert_eq!(std::fs::read(&second_path).unwrap(), second_replacement);

        recover_hook_transaction_at(&state).unwrap();

        assert_eq!(std::fs::read(second_path).unwrap(), second_replacement);
        assert!(!journal_path(&state).exists());
        assert!(!proof.exists());
    }

    #[test]
    fn crash_before_rename_with_prepared_temp_never_restores_target() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let first_original = plans[0].edits[0].original.clone();
        let first_path = plans[0].edits[0].path.clone();
        let second_path = plans[1].edits[0].path.clone();
        let state = temp.path().join("state");
        assert!(
            apply_hook_transaction_at(&plans, &state, ApplyFault::CrashAfterIntentBeforeRename(1),)
                .is_err()
        );
        let journal: HookTransactionJournal =
            serde_json::from_slice(&std::fs::read(journal_path(&state)).unwrap()).unwrap();
        let prepared = journal.in_flight.unwrap().temporary_path;
        assert!(prepared.exists());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&prepared).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::write(&second_path, b"{\"concurrent-after-crash\":true}\n").unwrap();

        recover_hook_transaction_at(&state).unwrap();

        assert_eq!(std::fs::read(first_path).ok(), first_original);
        assert_eq!(
            std::fs::read(second_path).unwrap(),
            b"{\"concurrent-after-crash\":true}\n"
        );
    }

    #[test]
    fn recovery_only_considers_completed_and_in_flight_edits() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let plans = stage_provider_hooks_at(
            &[AgentProvider::Codex, AgentProvider::Claude],
            HookScope::Global,
            &home,
            &project,
        )
        .unwrap();
        let later = plans[1].edits[0].clone();
        std::fs::create_dir_all(later.path.parent().unwrap()).unwrap();
        std::fs::write(&later.path, &later.replacement).unwrap();
        let state = temp.path().join("state");
        assert!(apply_hook_transaction_at(
            &plans, &state, ApplyFault::CrashAfterRenameBeforeJournal(0),
        ).is_err());
        let journal: HookTransactionJournal =
            serde_json::from_slice(&std::fs::read(journal_path(&state)).unwrap()).unwrap();
        assert!(
            !journal.in_flight.unwrap().temporary_path.exists(),
            "a successful rename must consume the prepared temporary file"
        );

        recover_hook_transaction_at(&state).unwrap();
        assert_eq!(std::fs::read(&later.path).unwrap(), later.replacement);
    }

    fn replacement_json(plans: &[ProviderHookPlan], provider: AgentProvider) -> serde_json::Value {
        let bytes = &plans
            .iter()
            .find(|plan| plan.provider == provider)
            .unwrap()
            .edits[0]
            .replacement;
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn public_contract_types_are_usable() {
        let _: Option<ManagedFileEdit> = None;
        let _: Option<ProviderHookPlan> = None;
        let _: Option<HookTransactionJournal> = None;
        let _: Option<InFlightEdit> = None;
        assert_eq!(HookScope::Project, HookScope::Project);
        assert_eq!(Path::new("x"), Path::new("x"));
    }

    #[test]
    fn provider_inspection_distinguishes_missing_current_stale_and_invalid() {
        for provider in [
            AgentProvider::Codex,
            AgentProvider::Claude,
            AgentProvider::Antigravity,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let project = temp.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            assert_eq!(
                inspect_provider_hooks_at(provider, &home, &project),
                ProviderHookInspection::Missing
            );

            let plans =
                stage_provider_hooks_at(&[provider], HookScope::Global, &home, &project).unwrap();
            let edit = &plans[0].edits[0];
            std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
            std::fs::write(&edit.path, &edit.replacement).unwrap();
            assert_eq!(
                inspect_provider_hooks_at(provider, &home, &project),
                ProviderHookInspection::Current
            );

            let stale = String::from_utf8(edit.replacement.clone())
                .unwrap()
                .replacen("--lifecycle-hook", "--lifecycle-hook --changed", 1);
            std::fs::write(&edit.path, stale).unwrap();
            assert_eq!(
                inspect_provider_hooks_at(provider, &home, &project),
                ProviderHookInspection::Stale
            );

            if provider != AgentProvider::Codex {
                let providerless = String::from_utf8(edit.replacement.clone())
                    .unwrap()
                    .replace(&format!(" --provider {}", provider.as_str()), "");
                std::fs::write(&edit.path, providerless).unwrap();
                assert_eq!(
                    inspect_provider_hooks_at(provider, &home, &project),
                    ProviderHookInspection::Stale
                );
            }

            std::fs::write(&edit.path, b"[]").unwrap();
            assert_eq!(
                inspect_provider_hooks_at(provider, &home, &project),
                ProviderHookInspection::Invalid
            );
        }
    }

    #[test]
    fn provider_inspection_reports_duplicate_scopes_without_exposing_contents() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        for scope in [HookScope::Global, HookScope::Project] {
            let plans =
                stage_provider_hooks_at(&[AgentProvider::Claude], scope, &home, &project).unwrap();
            let edit = &plans[0].edits[0];
            std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
            std::fs::write(&edit.path, &edit.replacement).unwrap();
        }

        assert_eq!(
            inspect_provider_hooks_at(AgentProvider::Claude, &home, &project),
            ProviderHookInspection::Duplicate
        );
    }

    #[test]
    fn provider_inspection_finds_ancestor_project_setup_with_strict_root_boundary() {
        for provider in [AgentProvider::Codex, AgentProvider::Claude] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("project");
            let cwd = root.join("nested/work");
            std::fs::create_dir_all(root.join(".git")).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();

            let plans =
                stage_provider_hooks_at(&[provider], HookScope::Project, &home, &root).unwrap();
            let edit = &plans[0].edits[0];
            std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
            std::fs::write(&edit.path, &edit.replacement).unwrap();

            let outside = provider_path(provider, HookScope::Project, &home, temp.path());
            let secret = b"not-json SECRET_OUTSIDE_PROJECT";
            std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
            std::fs::write(&outside, secret).unwrap();

            let inspection = inspect_provider_hooks_at(provider, &home, &cwd);

            assert_eq!(inspection, ProviderHookInspection::Current);
            assert_eq!(std::fs::read(outside).unwrap(), secret);
            assert!(!format!("{inspection:?}").contains("SECRET_OUTSIDE_PROJECT"));
        }
    }

    #[test]
    fn provider_inspection_deduplicates_home_project_aliases() {
        for provider in [AgentProvider::Codex, AgentProvider::Claude] {
            for nested in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let home = temp.path().join("home");
                let cwd = if nested {
                    home.join("nested/work")
                } else {
                    home.clone()
                };
                std::fs::create_dir_all(home.join(".git")).unwrap();
                std::fs::create_dir_all(&cwd).unwrap();

                let plans =
                    stage_provider_hooks_at(&[provider], HookScope::Global, &home, &cwd).unwrap();
                let edit = &plans[0].edits[0];
                std::fs::create_dir_all(edit.path.parent().unwrap()).unwrap();
                std::fs::write(&edit.path, &edit.replacement).unwrap();

                let outside = provider_path(provider, HookScope::Project, &home, temp.path());
                let secret = b"not-json SECRET_OUTSIDE_HOME_PROJECT";
                std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
                std::fs::write(&outside, secret).unwrap();

                let inspection = inspect_provider_hooks_at(provider, &home, &cwd);

                assert_eq!(
                    inspection,
                    ProviderHookInspection::Current,
                    "{provider} nested={nested}"
                );
                assert_eq!(std::fs::read(outside).unwrap(), secret);
                assert!(!format!("{inspection:?}").contains("SECRET_OUTSIDE_HOME_PROJECT"));
            }
        }
    }
}
