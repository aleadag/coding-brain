use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coding_brain_core::brain_activity::{ActivityEvent, ActivityState};
use coding_brain_core::lifecycle::{LifecycleSnapshot, PermissionAction, PermissionDisposition};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use coding_brain_core::review_state::{ReviewDisposition, ReviewKey, ReviewSurface};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::legacy::PermissionTransactionJournal;
use crate::brain::decisions::{DecisionRecord, DecisionType, HookDecisionRecord};

use super::decisions::{DecisionIdentity, DecisionKind, DecisionPayload};
use super::legacy::{
    LegacyDecision, LegacyFingerprint, LegacyFreezeArtifact, LegacyImportSink, LegacySourceKind,
    LegacySourceSet, LegacyWriterGuard,
};
use super::security::{
    ClosedDatabaseIdentity, PrivateFileIdentity, PublicationPresence, SecureDatabaseDirectory,
    SecurityError,
};
use super::{
    BRAIN_DATABASE_NAME, BrainDb, MIGRATION_LOCK_NAME, OpenRole, REVIEW_DATABASE_NAME,
    StorageDeadline, StorageError, StoragePaths,
};

static MIGRATION_GENERATION: AtomicU64 = AtomicU64::new(1);
const MIGRATION_STATE_NAME: &CStr = c".brain.sqlite3.migration-state.json";
const MAX_MIGRATION_STATE_BYTES: usize = 64 * 1024;
const FROZEN_MANIFEST_NAME: &CStr = c".brain.sqlite3.frozen-manifest.json";
const MAX_FREEZE_RECORD_BYTES: usize = 16 * 1024;
const MAX_FROZEN_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_FREEZE_FILES: usize = 4_100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationStatus {
    Building,
    Verified,
    BrainPublishedIncomplete,
    LegacyFrozen,
    Complete,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenSourceManifest {
    pub generation: u64,
    pub profile: String,
    pub review_result: String,
    pub count: u64,
    pub digest: String,
    rows: Vec<FrozenManifestRow>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FrozenManifestRow {
    relative_path: Vec<u8>,
    present: bool,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    mode: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum FreezeProgressRecord {
    Preparing {
        index: u64,
        expected: PersistedFingerprint,
        temporary_name: String,
    },
    Prepared {
        index: u64,
        expected: PersistedFingerprint,
        temporary_name: String,
        artifact: Box<LegacyFreezeArtifact>,
    },
}

#[derive(Clone, Debug)]
pub struct MigrationCoordinator {
    paths: StoragePaths,
}

impl MigrationCoordinator {
    pub fn at(state_root: &Path) -> Self {
        Self {
            paths: StoragePaths::at(state_root),
        }
    }

    pub fn inspect(&self) -> Result<MigrationStatus, StorageError> {
        self.inspect_inner().map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Migration, false, error)
        })
    }

    fn inspect_inner(&self) -> Result<MigrationStatus, StorageError> {
        let directory = match SecureDatabaseDirectory::prepare(&self.paths.state_root, false) {
            Ok(directory) => directory,
            Err(SecurityError::Missing) => return Ok(MigrationStatus::Building),
            Err(error) => return Err(error.into()),
        };
        inspect_in_directory(&self.paths, &directory)
    }

    pub fn run_non_hook(&self) -> Result<MigrationStatus, StorageError> {
        self.run_non_hook_inner().map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Migration, false, error)
        })
    }

    fn run_non_hook_inner(&self) -> Result<MigrationStatus, StorageError> {
        let directory = SecureDatabaseDirectory::prepare(&self.paths.state_root, true)?;
        let lock = directory.open_lock_file(MIGRATION_LOCK_NAME, true)?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                StorageError::Busy
            } else {
                StorageError::Io(error)
            }
        })?;
        directory.validate_lock_file(MIGRATION_LOCK_NAME, &lock)?;
        self.resume_locked(&directory)
    }

    pub fn resume(&self) -> Result<MigrationStatus, StorageError> {
        self.run_non_hook()
    }

    fn resume_locked(
        &self,
        directory: &SecureDatabaseDirectory,
    ) -> Result<MigrationStatus, StorageError> {
        let mut state = match load_state(directory)? {
            Some(state) => state,
            None => {
                reject_unmanaged_migration_entries(directory)?;
                if directory.private_file_present(BRAIN_DATABASE_NAME)? {
                    let deadline = StorageDeadline::after(Duration::from_secs(1));
                    drop(BrainDb::open_current(
                        &self.paths,
                        OpenRole::NonHook,
                        deadline,
                    )?);
                    return Ok(MigrationStatus::Complete);
                }
                let _guard = LegacyWriterGuard::acquire(
                    &self.paths.state_root,
                    StorageDeadline::after(Duration::from_secs(5)),
                )?;
                create_initial_state(&self.paths, directory)?
            }
        };
        state = recover_pending_state_transition(&self.paths, directory, state, true)?;
        state = recover_pending_review_result(&self.paths, directory, state, true)?;
        state = recover_pending_freeze_state(directory, state, true)?;
        state = recover_pending_building_rebase(&self.paths, directory, state, true)?;
        if state.manifest.status == MigrationStatus::Building {
            validate_coordinator_metadata(&self.paths, directory, &state.manifest)?;
            state = rebase_building_sources(&self.paths, directory, state)?;
        }
        validate_state(&self.paths, directory, &state.manifest)?;

        if state.manifest.status == MigrationStatus::Complete {
            validate_frozen_manifest_for_state(&self.paths, &state.manifest)?;
            return Ok(MigrationStatus::Complete);
        }
        if state.manifest.status == MigrationStatus::LegacyFrozen {
            validate_frozen_manifest_for_state(&self.paths, &state.manifest)?;
            BrainDb::open_published_for_completion(&self.paths, state.manifest.generation)?
                .complete_published_migration(&self.paths, state.manifest.generation)?;
            migration_fault("after-database-complete");
            state = transition_state(directory, state, MigrationStatus::Complete)?;
            migration_fault("after-complete-state");
            return Ok(state.manifest.status);
        }
        if state.manifest.status == MigrationStatus::BrainPublishedIncomplete {
            if !matches!(
                state.manifest.review_result,
                ReviewMigrationResult::Published { .. } | ReviewMigrationResult::Degraded { .. }
            ) {
                state = migrate_review(&self.paths, directory, state)?;
            }
            return resume_freeze(&self.paths, directory, state);
        }
        let staging_name = state.manifest.staging_cstring()?;
        if state.manifest.status == MigrationStatus::Verified {
            match publication_presence(directory, &state.manifest)? {
                PublicationPresence::Staging => {
                    directory.publish_database(&staging_name, BRAIN_DATABASE_NAME)?;
                }
                PublicationPresence::LinkedPair => {
                    directory.finish_linked_publication(&staging_name, BRAIN_DATABASE_NAME)?;
                }
                PublicationPresence::Canonical => {}
                PublicationPresence::Neither => {
                    return Err(StorageError::InvalidStorage(
                        "verified migration database disappeared",
                    ));
                }
            }
            validate_published(&self.paths, state.manifest.generation)?;
            let published = BrainDb::open_published_incomplete(&self.paths)?;
            validate_brain_artifact(
                &published,
                state
                    .manifest
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )?;
            validate_database_accounting(
                &self.paths,
                &published,
                state
                    .manifest
                    .accounting
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "verified migration has no accounting",
                    ))?,
            )?;
            state = transition_state(directory, state, MigrationStatus::BrainPublishedIncomplete)?;
            migration_fault("after-brain-publication");
            state = migrate_review(&self.paths, directory, state)?;
            return resume_freeze(&self.paths, directory, state);
        }

        if publication_presence(directory, &state.manifest)? == PublicationPresence::Staging {
            let stale = BrainDb::open_staging_incomplete(
                &self.paths,
                &staging_name,
                state.manifest.generation,
            )?;
            stale.discard_staging(&self.paths, &staging_name)?;
        }
        let mut staging =
            BrainDb::create_staging(&self.paths, &staging_name, state.manifest.generation)?;
        migration_fault("building");
        prepare_import_tables(&staging)?;

        let sources = LegacySourceSet::at(&self.paths.state_root)?;
        let before_digest = state.manifest.fingerprint_digest.clone();
        let before_count = state.manifest.fingerprint_count;
        let before_descriptors = state.manifest.descriptors.clone();

        let mut accounting = MigrationAccounting::default();
        {
            let mut sink =
                MigrationImport::new(&mut staging, ImportPhase::Activity, &mut accounting);
            sources.stream_kind_into(LegacySourceKind::Activity, &mut sink)?;
        }
        {
            let mut sink =
                MigrationImport::new(&mut staging, ImportPhase::Decisions, &mut accounting);
            sources.stream_kind_into(LegacySourceKind::Decisions, &mut sink)?;
        }
        {
            let mut sink =
                MigrationImport::new(&mut staging, ImportPhase::Journals, &mut accounting);
            sources.stream_kind_into(LegacySourceKind::PermissionTransactions, &mut sink)?;
        }
        {
            let mut sink =
                MigrationImport::new(&mut staging, ImportPhase::Lifecycle, &mut accounting);
            sources.stream_kind_into(LegacySourceKind::Lifecycle, &mut sink)?;
        }
        let after = brain_source_fingerprint_state(&sources)?;
        if before_digest != after.digest
            || before_count != after.count
            || before_descriptors != after.descriptors
        {
            return Err(StorageError::InvalidStorage(
                "legacy sources changed during migration",
            ));
        }
        verify_staging(&staging)?;
        populate_database_accounting(&staging, &mut accounting)?;
        validate_accounting(&accounting)?;
        let content_digest = brain_logical_digest(&staging.connection)?;
        state.manifest.accounting = Some(accounting);
        staging.finish_staging(&self.paths, &staging_name)?;
        let metadata = fs::symlink_metadata(
            self.paths
                .db_dir
                .join(OsStr::from_bytes(staging_name.to_bytes())),
        )?;
        state.manifest.brain_artifact = Some(BrainArtifact {
            device: metadata.dev(),
            inode: metadata.ino(),
            content_digest,
        });
        state = transition_state(directory, state, MigrationStatus::Verified)?;
        migration_fault("verified");
        directory.publish_database(&staging_name, BRAIN_DATABASE_NAME)?;
        let published = BrainDb::open_published_incomplete(&self.paths)?;
        validate_brain_artifact(
            &published,
            state
                .manifest
                .brain_artifact
                .as_ref()
                .ok_or(StorageError::InvalidStorage(
                    "migration Brain artifact is missing",
                ))?,
        )?;
        state = transition_state(directory, state, MigrationStatus::BrainPublishedIncomplete)?;
        migration_fault("after-brain-publication");
        state = migrate_review(&self.paths, directory, state)?;
        resume_freeze(&self.paths, directory, state)
    }
}

fn migrate_review(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
) -> Result<LoadedState, StorageError> {
    let sources = LegacySourceSet::at(&paths.state_root)?;
    if let ReviewMigrationResult::Published { fingerprint, .. } = &state.manifest.review_result {
        if &review_source_fingerprint(&sources)? != fingerprint {
            let fingerprint = fingerprint.clone();
            state.manifest.review_result = ReviewMigrationResult::Degraded {
                fingerprint,
                reason: "source_race".to_owned(),
            };
            return replace_review_result(directory, state);
        }
        return Ok(state);
    }
    if matches!(
        state.manifest.review_result,
        ReviewMigrationResult::Verified { .. }
    ) {
        return publish_verified_review(directory, state);
    }
    if let ReviewMigrationResult::Building {
        fingerprint,
        staging_name,
    } = state.manifest.review_result.clone()
    {
        return resume_review_build(paths, directory, state, &sources, fingerprint, staging_name);
    }
    let staging = review_staging_name(state.manifest.generation)?;
    if directory.publication_presence(&staging, REVIEW_DATABASE_NAME)?
        != PublicationPresence::Neither
    {
        return Err(StorageError::InvalidStorage(
            "unowned review migration artifact exists",
        ));
    }
    let before = review_source_fingerprint(&sources)?;
    let mut capture = ReviewCapture::default();
    match sources.stream_kind_into(LegacySourceKind::ReviewState, &mut capture) {
        Ok(_) => {
            let after = review_source_fingerprint(&sources)?;
            if after != before {
                state.manifest.review_result = ReviewMigrationResult::Degraded {
                    fingerprint: before,
                    reason: "source_race".to_owned(),
                };
                return replace_review_result(directory, state);
            }
            state.manifest.review_result = ReviewMigrationResult::Building {
                fingerprint: before,
                staging_name: staging.to_string_lossy().into_owned(),
            };
            state = replace_review_result(directory, state)?;
            migration_fault("review-before-create");
            return migrate_review(paths, directory, state);
        }
        Err(StorageError::InvalidStorage("invalid legacy review state")) => {
            state.manifest.review_result = ReviewMigrationResult::Degraded {
                fingerprint: before,
                reason: "malformed".to_owned(),
            };
        }
        Err(error) => return Err(error),
    }
    let state = replace_review_result(directory, state)?;
    if matches!(
        state.manifest.review_result,
        ReviewMigrationResult::Verified { .. }
    ) {
        migration_fault("review-verified");
        publish_verified_review(directory, state)
    } else {
        Ok(state)
    }
}

fn resume_freeze(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
) -> Result<MigrationStatus, StorageError> {
    migration_fault("before-freeze-guard");
    let allow_frozen = !matches!(state.manifest.freeze_state, FreezeState::Pending);
    let mut guard = if allow_frozen {
        LegacyWriterGuard::acquire_freeze_resume(
            &paths.state_root,
            StorageDeadline::after(Duration::from_secs(10)),
        )?
    } else {
        LegacyWriterGuard::acquire(
            &paths.state_root,
            StorageDeadline::after(Duration::from_secs(10)),
        )?
    };

    if matches!(state.manifest.freeze_state, FreezeState::Pending) {
        validate_source_fingerprints(paths, &state.manifest)?;
        let review_included = matches!(
            state.manifest.review_result,
            ReviewMigrationResult::Published { .. }
        );
        let candidates = ordered_freeze_candidates(&guard, review_included)?;
        if let ReviewMigrationResult::Published { fingerprint, .. } = &state.manifest.review_result
        {
            let current_review = candidates
                .iter()
                .find(|candidate| candidate.kind == "review_state")
                .ok_or(StorageError::InvalidStorage(
                    "published Review source is missing from freeze",
                ))?;
            if current_review != fingerprint {
                return Err(StorageError::InvalidStorage(
                    "published Review source changed before freeze",
                ));
            }
        }
        let (source_digest, source_count) =
            freeze_content_digest(paths, &candidates, review_included)?;
        reject_reserved_freeze_entries(paths, state.manifest.generation)?;
        let progress_name = freeze_progress_name(state.manifest.generation);
        state = replace_freeze_state(
            directory,
            state,
            FreezeState::Building {
                progress_name,
                source_digest,
                source_count,
                review_included,
            },
            "after-freeze-building-state-sync",
        )?;
    }

    let (progress_name, source_digest, source_count, review_included) =
        freeze_common(&state.manifest.freeze_state)?;
    let initial_common = (
        progress_name.to_owned(),
        source_digest.to_owned(),
        *source_count,
        *review_included,
    );
    let progress = ensure_progress_directory(paths, &initial_common.0)?;
    if let Some((expected_device, expected_inode)) =
        freeze_progress_identity(&state.manifest.freeze_state)
    {
        progress.validate_expected_identity(expected_device, expected_inode)?;
    } else if matches!(state.manifest.freeze_state, FreezeState::Building { .. }) {
        state = replace_freeze_state(
            directory,
            state,
            FreezeState::ProgressReady {
                progress_name: initial_common.0,
                source_digest: initial_common.1,
                source_count: initial_common.2,
                review_included: initial_common.3,
                progress_device: progress.device,
                progress_inode: progress.inode,
            },
            "after-freeze-progress-ready-state-sync",
        )?;
    }
    let (_, source_digest, source_count, review_included) =
        freeze_common(&state.manifest.freeze_state)?;
    let mut records = load_or_initialize_progress(
        &progress,
        &guard,
        state.manifest.generation,
        *source_count,
        *review_included,
    )?;
    let current_candidates = records
        .iter()
        .map(|record| record_expected(record).clone())
        .collect::<Vec<_>>();
    validate_current_journal_source_set(&guard, &current_candidates)?;
    let (actual_digest, actual_count) =
        freeze_content_digest_persisted(paths, &current_candidates, *review_included)?;
    if actual_digest != source_digest || actual_count != *source_count {
        return Err(StorageError::InvalidStorage(
            "legacy sources changed after Brain publication",
        ));
    }

    if matches!(
        state.manifest.freeze_state,
        FreezeState::ProgressReady { .. }
    ) {
        for (index, record) in records.iter_mut().enumerate() {
            let FreezeProgressRecord::Preparing {
                expected,
                temporary_name,
                ..
            } = record
            else {
                if let FreezeProgressRecord::Prepared { artifact, .. } = record {
                    guard.publish_freeze(artifact)?;
                }
                continue;
            };
            if !expected.present {
                continue;
            }
            migration_fault("freeze-preparing-synced");
            let fingerprint = legacy_fingerprint(expected)?;
            let artifact = guard.prepare_freeze(
                Path::new(&expected.relative_path),
                temporary_name,
                &fingerprint,
            )?;
            migration_fault("freeze-temp-synced");
            let prepared = FreezeProgressRecord::Prepared {
                index: index as u64,
                expected: expected.clone(),
                temporary_name: temporary_name.clone(),
                artifact: Box::new(artifact),
            };
            replace_progress_record(&progress, index, &prepared)?;
            migration_fault("freeze-prepared-record-synced");
            *record = prepared;
            if let FreezeProgressRecord::Prepared { artifact, .. } = record {
                guard.publish_freeze(artifact)?;
            }
            migration_fault("freeze-entry-published");
            progress.sync_all()?;
            migration_fault("freeze-progress-synced");
        }
        let next =
            copy_freeze_common(&state.manifest.freeze_state, FreezePhase::DirectoryFreezing)?;
        state = replace_freeze_state(
            directory,
            state,
            next,
            "after-directory-freezing-state-sync",
        )?;
    }

    if matches!(
        state.manifest.freeze_state,
        FreezeState::DirectoryFreezing { .. }
    ) {
        guard.freeze_journal_directory()?;
        migration_fault("after-journal-directory-chmod");
        let next = copy_freeze_common(&state.manifest.freeze_state, FreezePhase::DirectoryFrozen)?;
        state = replace_freeze_state(directory, state, next, "after-directory-frozen-state-sync")?;
    }

    if matches!(
        state.manifest.freeze_state,
        FreezeState::DirectoryFrozen { .. }
    ) {
        let manifest = build_frozen_manifest(paths, &state.manifest, &records)?;
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|_| StorageError::InvalidStorage("frozen manifest serialization failed"))?;
        if bytes.len() > MAX_FROZEN_MANIFEST_BYTES {
            return Err(StorageError::InvalidStorage(
                "frozen manifest exceeds its bound",
            ));
        }
        let temporary_name = manifest_temp_name(state.manifest.generation);
        let (progress_name, source_digest, source_count, review_included) =
            freeze_common(&state.manifest.freeze_state)?;
        let common = (
            progress_name.to_owned(),
            source_digest.to_owned(),
            *source_count,
            *review_included,
        );
        let (progress_device, progress_inode) =
            freeze_progress_identity(&state.manifest.freeze_state).ok_or(
                StorageError::InvalidStorage("freeze progress identity is missing"),
            )?;
        state = replace_freeze_state(
            directory,
            state,
            FreezeState::ManifestBuilding {
                progress_name: common.0,
                source_digest: common.1,
                source_count: common.2,
                review_included: common.3,
                manifest_digest: manifest.digest,
                manifest_count: manifest.count,
                manifest_temporary: temporary_name,
                progress_device,
                progress_inode,
            },
            "after-manifest-building-state-sync",
        )?;
    }

    if matches!(
        state.manifest.freeze_state,
        FreezeState::ManifestBuilding { .. }
    ) {
        let (expected_digest, expected_count, manifest_temporary) =
            match &state.manifest.freeze_state {
                FreezeState::ManifestBuilding {
                    manifest_digest,
                    manifest_count,
                    manifest_temporary,
                    ..
                } => (
                    manifest_digest.clone(),
                    *manifest_count,
                    manifest_temporary.clone(),
                ),
                _ => unreachable!(),
            };
        let manifest = build_frozen_manifest(paths, &state.manifest, &records)?;
        let bytes = serde_json::to_vec(&manifest)
            .map_err(|_| StorageError::InvalidStorage("frozen manifest serialization failed"))?;
        if bytes.len() > MAX_FROZEN_MANIFEST_BYTES
            || manifest.digest != expected_digest
            || manifest.count != expected_count
        {
            return Err(StorageError::InvalidStorage(
                "owned frozen manifest evidence changed",
            ));
        }
        let temporary = CString::new(manifest_temporary.as_bytes())
            .map_err(|_| StorageError::InvalidStorage("manifest temporary name is invalid"))?;
        if directory.private_file_present(&temporary)? {
            if directory
                .read_private_file(&temporary, MAX_FROZEN_MANIFEST_BYTES)?
                .bytes
                != bytes
            {
                return Err(StorageError::InvalidStorage(
                    "owned frozen manifest temporary changed",
                ));
            }
        } else {
            directory.write_new_private_file(&temporary, &bytes)?;
        }
        migration_fault("after-manifest-temp-sync");
        let (progress_name, source_digest, source_count, review_included) =
            freeze_common(&state.manifest.freeze_state)?;
        let common = (
            progress_name.to_owned(),
            source_digest.to_owned(),
            *source_count,
            *review_included,
        );
        let (progress_device, progress_inode) =
            freeze_progress_identity(&state.manifest.freeze_state).ok_or(
                StorageError::InvalidStorage("freeze progress identity is missing"),
            )?;
        state = replace_freeze_state(
            directory,
            state,
            FreezeState::ManifestVerified {
                progress_name: common.0,
                source_digest: common.1,
                source_count: common.2,
                review_included: common.3,
                manifest_digest: manifest.digest,
                manifest_count: manifest.count,
                manifest_temporary,
                progress_device,
                progress_inode,
            },
            "after-manifest-verified-state-sync",
        )?;
    }

    if let FreezeState::ManifestVerified {
        manifest_temporary, ..
    } = &state.manifest.freeze_state
    {
        let temporary = CString::new(manifest_temporary.as_bytes())
            .map_err(|_| StorageError::InvalidStorage("manifest temporary name is invalid"))?;
        match directory.publication_presence(&temporary, FROZEN_MANIFEST_NAME)? {
            PublicationPresence::Staging => {
                directory.publish_database(&temporary, FROZEN_MANIFEST_NAME)?;
            }
            PublicationPresence::LinkedPair => {
                directory.finish_linked_publication(&temporary, FROZEN_MANIFEST_NAME)?;
            }
            PublicationPresence::Canonical => {}
            PublicationPresence::Neither => {
                return Err(StorageError::InvalidStorage(
                    "verified frozen manifest disappeared",
                ));
            }
        }
        migration_fault("after-manifest-publication");
        validate_frozen_manifest_for_state(paths, &state.manifest)?;
        let (progress_name, source_digest, source_count, review_included) =
            freeze_common(&state.manifest.freeze_state)?;
        let common = (
            progress_name.to_owned(),
            source_digest.to_owned(),
            *source_count,
            *review_included,
        );
        let (manifest_digest, manifest_count) = match &state.manifest.freeze_state {
            FreezeState::ManifestVerified {
                manifest_digest,
                manifest_count,
                ..
            } => (manifest_digest.clone(), *manifest_count),
            _ => unreachable!(),
        };
        let (progress_device, progress_inode) =
            freeze_progress_identity(&state.manifest.freeze_state).ok_or(
                StorageError::InvalidStorage("freeze progress identity is missing"),
            )?;
        state = replace_freeze_state(
            directory,
            state,
            FreezeState::ManifestPublished {
                progress_name: common.0,
                source_digest: common.1,
                source_count: common.2,
                review_included: common.3,
                manifest_digest,
                manifest_count,
                progress_device,
                progress_inode,
            },
            "after-manifest-published-state-sync",
        )?;
    }

    if matches!(
        state.manifest.freeze_state,
        FreezeState::ManifestPublished { .. }
    ) {
        state = transition_state(directory, state, MigrationStatus::LegacyFrozen)?;
        migration_fault("after-legacy-frozen");
        validate_frozen_manifest_for_state(paths, &state.manifest)?;
        BrainDb::open_published_for_completion(paths, state.manifest.generation)?
            .complete_published_migration(paths, state.manifest.generation)?;
        migration_fault("after-database-complete");
        state = transition_state(directory, state, MigrationStatus::Complete)?;
        migration_fault("after-complete-state");
    }
    Ok(state.manifest.status)
}

fn validate_current_journal_source_set(
    guard: &LegacyWriterGuard,
    expected: &[PersistedFingerprint],
) -> Result<(), StorageError> {
    let actual = guard
        .final_journal_paths(StorageDeadline::after(Duration::from_secs(2)))?
        .into_iter()
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .collect::<std::collections::BTreeSet<_>>();
    let expected = expected
        .iter()
        .filter(|fingerprint| {
            fingerprint.kind == "permission_transactions"
                && fingerprint.relative_path != "brain/permission-transactions"
        })
        .map(|fingerprint| fingerprint.relative_path.as_bytes().to_vec())
        .collect::<std::collections::BTreeSet<_>>();
    if actual != expected {
        return Err(StorageError::InvalidStorage(
            "legacy journal source set changed after Brain publication",
        ));
    }
    Ok(())
}

enum FreezePhase {
    DirectoryFreezing,
    DirectoryFrozen,
}

fn ordered_freeze_candidates(
    guard: &LegacyWriterGuard,
    review_included: bool,
) -> Result<Vec<PersistedFingerprint>, StorageError> {
    let fingerprints = guard.fingerprints()?;
    let mut ordered = Vec::new();
    for kind in [
        LegacySourceKind::Decisions,
        LegacySourceKind::Activity,
        LegacySourceKind::Lifecycle,
    ] {
        ordered.push(persisted_fingerprint(
            fingerprints
                .iter()
                .find(|fingerprint| fingerprint.kind == kind)
                .cloned()
                .ok_or(StorageError::InvalidStorage(
                    "freeze source fingerprint is missing",
                ))?,
        ));
    }
    let mut journals = fingerprints
        .iter()
        .filter(|fingerprint| {
            fingerprint.kind == LegacySourceKind::PermissionTransactions
                && fingerprint.relative_path() != Path::new("brain/permission-transactions")
        })
        .cloned()
        .collect::<Vec<_>>();
    journals.sort_by(|left, right| {
        left.relative_path()
            .as_os_str()
            .as_bytes()
            .cmp(right.relative_path().as_os_str().as_bytes())
    });
    ordered.extend(journals.into_iter().map(persisted_fingerprint));
    if review_included {
        ordered.push(persisted_fingerprint(
            fingerprints
                .iter()
                .find(|fingerprint| fingerprint.kind == LegacySourceKind::ReviewState)
                .cloned()
                .ok_or(StorageError::InvalidStorage(
                    "review freeze fingerprint is missing",
                ))?,
        ));
    }
    if ordered.len() > MAX_FREEZE_FILES {
        return Err(StorageError::InvalidStorage(
            "freeze source count exceeds its bound",
        ));
    }
    Ok(ordered)
}

fn freeze_content_digest(
    paths: &StoragePaths,
    candidates: &[PersistedFingerprint],
    review_included: bool,
) -> Result<(String, u64), StorageError> {
    freeze_content_digest_persisted(paths, candidates, review_included)
}

fn freeze_content_digest_persisted(
    paths: &StoragePaths,
    candidates: &[PersistedFingerprint],
    review_included: bool,
) -> Result<(String, u64), StorageError> {
    if candidates.len() > MAX_FREEZE_FILES {
        return Err(StorageError::InvalidStorage(
            "freeze source count exceeds its bound",
        ));
    }
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-legacy-freeze-source-set-v1");
    digest.update((super::LEGACY_EXPORT_PROFILE.len() as u64).to_be_bytes());
    digest.update(super::LEGACY_EXPORT_PROFILE.as_bytes());
    digest.update((candidates.len() as u64).to_be_bytes());
    digest.update([1]);
    digest.update([u8::from(review_included)]);
    for candidate in candidates {
        let kind = candidate.kind.as_bytes();
        let path = candidate.relative_path.as_bytes();
        digest.update((kind.len() as u64).to_be_bytes());
        digest.update(kind);
        digest.update((path.len() as u64).to_be_bytes());
        digest.update(path);
        digest.update([u8::from(candidate.present)]);
        if !candidate.present {
            if fs::symlink_metadata(paths.state_root.join(&candidate.relative_path)).is_ok() {
                return Err(StorageError::InvalidStorage(
                    "absent legacy freeze source was recreated",
                ));
            }
            digest.update(0_u64.to_be_bytes());
            continue;
        }
        let source_path = paths.state_root.join(&candidate.relative_path);
        let metadata = fs::symlink_metadata(&source_path)?;
        validate_freeze_path_metadata(&metadata, false)?;
        digest.update(metadata.len().to_be_bytes());
        let mut file = File::open(&source_path)?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut read_total = 0_u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            read_total = read_total
                .checked_add(read as u64)
                .ok_or(StorageError::InvalidStorage("freeze content size overflow"))?;
        }
        let after = file.metadata()?;
        if read_total != metadata.len()
            || after.dev() != metadata.dev()
            || after.ino() != metadata.ino()
            || after.len() != metadata.len()
            || after.mtime() != metadata.mtime()
            || after.mtime_nsec() != metadata.mtime_nsec()
        {
            return Err(StorageError::InvalidStorage(
                "legacy source changed while computing freeze digest",
            ));
        }
    }
    Ok((format!("{:x}", digest.finalize()), candidates.len() as u64))
}

fn validate_freeze_path_metadata(
    metadata: &fs::Metadata,
    directory: bool,
) -> Result<(), StorageError> {
    let expected_mode = if directory {
        0o500
    } else {
        metadata.mode() & 0o777
    };
    let valid_mode = if directory {
        expected_mode == 0o500
    } else {
        matches!(expected_mode, 0o600 | 0o400)
    };
    if (directory && !metadata.file_type().is_dir())
        || (!directory && !metadata.file_type().is_file())
        || metadata.uid() != unsafe { libc::geteuid() }
        || !valid_mode
        || (!directory && metadata.nlink() != 1)
    {
        return Err(StorageError::InvalidStorage(
            "legacy freeze path metadata is unsafe",
        ));
    }
    Ok(())
}

fn reject_reserved_freeze_entries(
    paths: &StoragePaths,
    generation: u64,
) -> Result<(), StorageError> {
    let progress = paths.db_dir.join(freeze_progress_name(generation));
    let manifest_temp = paths.db_dir.join(manifest_temp_name(generation));
    let freeze_state_temp = paths.db_dir.join(
        freeze_state_temp_name(generation)?
            .to_string_lossy()
            .as_ref(),
    );
    for path in [progress, manifest_temp, freeze_state_temp] {
        if fs::symlink_metadata(path).is_ok() {
            return Err(StorageError::InvalidStorage(
                "reserved freeze namespace is occupied",
            ));
        }
    }
    if fs::symlink_metadata(
        paths
            .db_dir
            .join(FROZEN_MANIFEST_NAME.to_string_lossy().as_ref()),
    )
    .is_ok()
    {
        return Err(StorageError::InvalidStorage(
            "frozen manifest already exists",
        ));
    }
    Ok(())
}

struct FreezeProgressDirectory {
    descriptor: File,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl FreezeProgressDirectory {
    fn validate_expected_identity(&self, device: u64, inode: u64) -> Result<(), StorageError> {
        self.validate_binding()?;
        if self.device != device || self.inode != inode {
            return Err(StorageError::InvalidStorage(
                "freeze progress directory identity changed",
            ));
        }
        Ok(())
    }

    fn validate_binding(&self) -> Result<(), StorageError> {
        let descriptor = self.descriptor.metadata()?;
        let path = fs::symlink_metadata(&self.path)?;
        for metadata in [&descriptor, &path] {
            if !metadata.file_type().is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o700
                || metadata.dev() != self.device
                || metadata.ino() != self.inode
            {
                return Err(StorageError::InvalidStorage(
                    "freeze progress directory binding changed",
                ));
            }
        }
        Ok(())
    }

    fn sync_all(&self) -> Result<(), StorageError> {
        self.validate_binding()?;
        self.descriptor.sync_all()?;
        self.validate_binding()
    }
}

fn ensure_progress_directory(
    paths: &StoragePaths,
    name: &str,
) -> Result<FreezeProgressDirectory, StorageError> {
    let path = paths.db_dir.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_dir()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o700
            {
                return Err(StorageError::InvalidStorage(
                    "freeze progress directory is unsafe",
                ));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            File::open(&paths.db_dir)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    let descriptor = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    let metadata = descriptor.metadata()?;
    let progress = FreezeProgressDirectory {
        descriptor,
        path,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    progress.validate_binding()?;
    Ok(progress)
}

fn progress_record_name(index: usize) -> String {
    format!("{index:04}.json")
}

fn freeze_temporary_name(generation: u64, index: usize) -> String {
    format!(".coding-brain-freeze-{generation}-{index:04}.tmp")
}

fn progress_name(name: &str) -> Result<CString, StorageError> {
    if name.is_empty() || name.as_bytes().contains(&b'/') {
        return Err(StorageError::InvalidStorage(
            "freeze progress name is invalid",
        ));
    }
    CString::new(name).map_err(|_| StorageError::InvalidStorage("freeze progress name is invalid"))
}

fn open_progress_entry(
    progress: &FreezeProgressDirectory,
    name: &str,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> Result<File, StorageError> {
    progress.validate_binding()?;
    let name = progress_name(name)?;
    let descriptor = unsafe {
        libc::openat(
            progress.descriptor.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    validate_progress_entry_correspondence(progress, name.as_c_str(), &file)?;
    progress.validate_binding()?;
    Ok(file)
}

#[allow(clippy::unnecessary_cast)] // libc stat fields vary in width across Unix targets.
fn validate_progress_entry_correspondence(
    progress: &FreezeProgressDirectory,
    name: &CStr,
    file: &File,
) -> Result<(), StorageError> {
    let mut path_stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        libc::fstatat(
            progress.descriptor.as_raw_fd(),
            name.as_ptr(),
            path_stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let path_stat = unsafe { path_stat.assume_init() };
    let metadata = file.metadata()?;
    if path_stat.st_dev as u64 != metadata.dev()
        || path_stat.st_ino as u64 != metadata.ino()
        || path_stat.st_uid != unsafe { libc::geteuid() }
        || path_stat.st_nlink != 1
        || path_stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || path_stat.st_mode & 0o777 != 0o600
    {
        return Err(StorageError::InvalidStorage(
            "freeze progress entry binding changed",
        ));
    }
    Ok(())
}

fn write_new_progress_record(
    progress: &FreezeProgressDirectory,
    index: usize,
    record: &FreezeProgressRecord,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| StorageError::InvalidStorage("freeze progress serialization failed"))?;
    if bytes.len() > MAX_FREEZE_RECORD_BYTES {
        return Err(StorageError::InvalidStorage(
            "freeze progress record exceeds its bound",
        ));
    }
    let mut file = open_progress_entry(
        progress,
        &progress_record_name(index),
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    validate_progress_entry_correspondence(
        progress,
        progress_name(&progress_record_name(index))?.as_c_str(),
        &file,
    )?;
    progress.sync_all()?;
    Ok(())
}

fn read_progress_record(
    progress: &FreezeProgressDirectory,
    index: usize,
) -> Result<FreezeProgressRecord, StorageError> {
    read_progress_record_named(progress, &progress_record_name(index))
}

fn read_progress_record_named(
    progress: &FreezeProgressDirectory,
    name: &str,
) -> Result<FreezeProgressRecord, StorageError> {
    let mut file = open_progress_entry(progress, name, libc::O_RDONLY, 0)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
        || metadata.len() > MAX_FREEZE_RECORD_BYTES as u64
    {
        return Err(StorageError::InvalidStorage(
            "freeze progress record is unsafe",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    validate_progress_entry_correspondence(progress, progress_name(name)?.as_c_str(), &file)?;
    let value = super::legacy::decode_exact_json(&bytes).ok_or(StorageError::InvalidStorage(
        "freeze progress record JSON is invalid",
    ))?;
    let record: FreezeProgressRecord = serde_json::from_value(value.clone())
        .map_err(|_| StorageError::InvalidStorage("freeze progress record is invalid"))?;
    if serde_json::to_value(&record).ok().as_ref() != Some(&value) {
        return Err(StorageError::InvalidStorage(
            "freeze progress record is not canonical",
        ));
    }
    Ok(record)
}

fn progress_entry_names(
    progress: &FreezeProgressDirectory,
) -> Result<Vec<std::ffi::OsString>, StorageError> {
    progress.validate_binding()?;
    let duplicate = unsafe {
        libc::openat(
            progress.descriptor.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error().into());
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Stream(stream);
    let mut names = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if matches!(name, b"." | b"..") {
            continue;
        }
        names.push(std::ffi::OsString::from_vec(name.to_vec()));
        if names.len() > MAX_FREEZE_FILES * 2 + 1 {
            return Err(StorageError::InvalidStorage(
                "freeze progress artifact count exceeds its bound",
            ));
        }
    }
    progress.validate_binding()?;
    Ok(names)
}

fn load_or_initialize_progress(
    progress: &FreezeProgressDirectory,
    guard: &LegacyWriterGuard,
    generation: u64,
    expected_count: u64,
    review_included: bool,
) -> Result<Vec<FreezeProgressRecord>, StorageError> {
    let expected_count = usize::try_from(expected_count)
        .map_err(|_| StorageError::InvalidStorage("freeze progress count is invalid"))?;
    let mut names = progress_entry_names(progress)?;
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in &names {
        let bytes = name.as_bytes();
        let record = bytes.len() == 9
            && bytes[..4].iter().all(u8::is_ascii_digit)
            && &bytes[4..] == b".json";
        let replacement = bytes.len() == 22
            && bytes[..4].iter().all(u8::is_ascii_digit)
            && &bytes[4..] == b".json.prepared.tmp";
        if !record && !replacement {
            return Err(StorageError::InvalidStorage(
                "unexpected freeze progress artifact",
            ));
        }
    }
    let present_records = names
        .iter()
        .filter(|name| name.as_bytes().ends_with(b".json"))
        .count();
    if present_records > expected_count
        || (0..present_records).any(|index| {
            !names
                .iter()
                .any(|name| name.as_bytes() == progress_record_name(index).as_bytes())
        })
    {
        return Err(StorageError::InvalidStorage(
            "freeze progress records are not an exact prefix",
        ));
    }
    if present_records < expected_count {
        let candidates = ordered_freeze_candidates(guard, review_included)?;
        if candidates.len() != expected_count {
            return Err(StorageError::InvalidStorage(
                "freeze source set changed before records",
            ));
        }
        for (index, expected) in candidates.into_iter().enumerate().skip(present_records) {
            write_new_progress_record(
                progress,
                index,
                &FreezeProgressRecord::Preparing {
                    index: index as u64,
                    expected,
                    temporary_name: freeze_temporary_name(generation, index),
                },
            )?;
        }
    }
    let mut records = Vec::with_capacity(expected_count);
    for index in 0..expected_count {
        recover_prepared_progress_replacement(progress, index)?;
        let record = read_progress_record(progress, index)?;
        if record_index(&record) != index as u64 {
            return Err(StorageError::InvalidStorage(
                "freeze progress index is invalid",
            ));
        }
        records.push(record);
    }
    if progress_entry_names(progress)?.len() != expected_count {
        return Err(StorageError::InvalidStorage(
            "freeze progress artifact count is invalid",
        ));
    }
    Ok(records)
}

fn record_index(record: &FreezeProgressRecord) -> u64 {
    match record {
        FreezeProgressRecord::Preparing { index, .. }
        | FreezeProgressRecord::Prepared { index, .. } => *index,
    }
}

fn record_expected(record: &FreezeProgressRecord) -> &PersistedFingerprint {
    match record {
        FreezeProgressRecord::Preparing { expected, .. }
        | FreezeProgressRecord::Prepared { expected, .. } => expected,
    }
}

fn replace_progress_record(
    progress: &FreezeProgressDirectory,
    index: usize,
    record: &FreezeProgressRecord,
) -> Result<(), StorageError> {
    let bytes = serde_json::to_vec(record)
        .map_err(|_| StorageError::InvalidStorage("freeze progress serialization failed"))?;
    if bytes.len() > MAX_FREEZE_RECORD_BYTES {
        return Err(StorageError::InvalidStorage(
            "freeze progress record exceeds its bound",
        ));
    }
    let temporary_name = format!("{}.prepared.tmp", progress_record_name(index));
    let mut replacement = open_progress_entry(
        progress,
        &temporary_name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    )?;
    replacement.write_all(&bytes)?;
    replacement.sync_all()?;
    validate_progress_entry_correspondence(
        progress,
        progress_name(&temporary_name)?.as_c_str(),
        &replacement,
    )?;
    progress.sync_all()?;
    rename_progress_entry(progress, &temporary_name, &progress_record_name(index))?;
    if read_progress_record(progress, index)? != *record {
        return Err(StorageError::InvalidStorage(
            "published freeze progress record changed",
        ));
    }
    progress.sync_all()?;
    Ok(())
}

fn recover_prepared_progress_replacement(
    progress: &FreezeProgressDirectory,
    index: usize,
) -> Result<(), StorageError> {
    let temporary_name = format!("{}.prepared.tmp", progress_record_name(index));
    let replacement = match read_progress_record_named(progress, &temporary_name) {
        Ok(record) => record,
        Err(StorageError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !matches!(replacement, FreezeProgressRecord::Prepared { .. })
        || record_index(&replacement) != index as u64
    {
        return Err(StorageError::InvalidStorage(
            "prepared progress replacement is not exact",
        ));
    }
    let current = read_progress_record(progress, index)?;
    if record_expected(&current) != record_expected(&replacement) {
        return Err(StorageError::InvalidStorage(
            "prepared progress replacement changed source",
        ));
    }
    rename_progress_entry(progress, &temporary_name, &progress_record_name(index))?;
    if read_progress_record(progress, index)? != replacement {
        return Err(StorageError::InvalidStorage(
            "recovered freeze progress record changed",
        ));
    }
    progress.sync_all()?;
    Ok(())
}

fn rename_progress_entry(
    progress: &FreezeProgressDirectory,
    source: &str,
    target: &str,
) -> Result<(), StorageError> {
    progress.validate_binding()?;
    let source = progress_name(source)?;
    let target = progress_name(target)?;
    let source_file = open_progress_entry(
        progress,
        source
            .to_str()
            .map_err(|_| StorageError::InvalidStorage("freeze progress name is invalid"))?,
        libc::O_RDONLY,
        0,
    )?;
    let result = unsafe {
        libc::renameat(
            progress.descriptor.as_raw_fd(),
            source.as_ptr(),
            progress.descriptor.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    validate_progress_entry_correspondence(progress, target.as_c_str(), &source_file)?;
    progress.validate_binding()?;
    Ok(())
}

fn legacy_fingerprint(expected: &PersistedFingerprint) -> Result<LegacyFingerprint, StorageError> {
    let kind = match expected.kind.as_str() {
        "decisions" => LegacySourceKind::Decisions,
        "activity" => LegacySourceKind::Activity,
        "lifecycle" => LegacySourceKind::Lifecycle,
        "permission_transactions" => LegacySourceKind::PermissionTransactions,
        "review_state" => LegacySourceKind::ReviewState,
        _ => {
            return Err(StorageError::InvalidStorage(
                "freeze progress source kind is invalid",
            ));
        }
    };
    Ok(LegacyFingerprint::from_persisted_parts(
        kind,
        Path::new(&expected.relative_path).to_owned(),
        expected.present,
        expected.device,
        expected.inode,
        expected.size,
        expected.modified_seconds,
        expected.modified_nanoseconds,
    ))
}

fn build_frozen_manifest(
    paths: &StoragePaths,
    state: &ValidatedState,
    records: &[FreezeProgressRecord],
) -> Result<FrozenSourceManifest, StorageError> {
    let mut rows = Vec::with_capacity(records.len() + 1);
    let mut directory_inserted = false;
    for record in records {
        let expected = record_expected(record);
        if expected.kind == "review_state" && !directory_inserted {
            rows.push(frozen_journal_directory_row(paths)?);
            directory_inserted = true;
        }
        let row = if expected.present {
            let FreezeProgressRecord::Prepared { artifact, .. } = record else {
                return Err(StorageError::InvalidStorage(
                    "present freeze progress record is not prepared",
                ));
            };
            if artifact.relative_path() != Path::new(&expected.relative_path) {
                return Err(StorageError::InvalidStorage(
                    "prepared freeze artifact path changed",
                ));
            }
            let target = artifact.target();
            FrozenManifestRow {
                relative_path: expected.relative_path.as_bytes().to_vec(),
                present: true,
                device: target.device(),
                inode: target.inode(),
                size: target.size(),
                modified_seconds: target.modified_seconds(),
                modified_nanoseconds: target.modified_nanoseconds(),
                mode: target.mode(),
            }
        } else {
            FrozenManifestRow {
                relative_path: expected.relative_path.as_bytes().to_vec(),
                present: false,
                device: 0,
                inode: 0,
                size: 0,
                modified_seconds: 0,
                modified_nanoseconds: 0,
                mode: 0,
            }
        };
        rows.push(row);
    }
    if !directory_inserted {
        rows.push(frozen_journal_directory_row(paths)?);
    }
    let review_result = match state.review_result {
        ReviewMigrationResult::Published { .. } => "published",
        ReviewMigrationResult::Degraded { .. } => "degraded",
        _ => {
            return Err(StorageError::InvalidStorage(
                "review result is not final before manifest",
            ));
        }
    }
    .to_owned();
    let count = rows.len() as u64;
    let digest = manifest_digest(state.generation, &review_result, &rows)?;
    Ok(FrozenSourceManifest {
        generation: state.generation,
        profile: super::LEGACY_EXPORT_PROFILE.to_owned(),
        review_result,
        count,
        digest,
        rows,
    })
}

fn frozen_journal_directory_row(paths: &StoragePaths) -> Result<FrozenManifestRow, StorageError> {
    let relative = b"brain/permission-transactions".to_vec();
    let metadata = fs::symlink_metadata(paths.state_root.join("brain/permission-transactions"))?;
    validate_freeze_path_metadata(&metadata, true)?;
    Ok(FrozenManifestRow {
        relative_path: relative,
        present: true,
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        mode: metadata.mode() & 0o777,
    })
}

fn manifest_digest(
    generation: u64,
    review_result: &str,
    rows: &[FrozenManifestRow],
) -> Result<String, StorageError> {
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-frozen-source-manifest-v1");
    digest.update(generation.to_be_bytes());
    digest.update((super::LEGACY_EXPORT_PROFILE.len() as u64).to_be_bytes());
    digest.update(super::LEGACY_EXPORT_PROFILE.as_bytes());
    digest.update((review_result.len() as u64).to_be_bytes());
    digest.update(review_result.as_bytes());
    digest.update((rows.len() as u64).to_be_bytes());
    for row in rows {
        let encoded = serde_json::to_vec(row)
            .map_err(|_| StorageError::InvalidStorage("manifest row serialization failed"))?;
        digest.update((encoded.len() as u64).to_be_bytes());
        digest.update(encoded);
    }
    Ok(format!("{:x}", digest.finalize()))
}

impl FrozenSourceManifest {
    pub fn load_and_validate(state_root: &Path) -> Result<Self, StorageError> {
        load_frozen_manifest_named(state_root, FROZEN_MANIFEST_NAME)
    }
}

fn load_frozen_manifest_named(
    state_root: &Path,
    name: &CStr,
) -> Result<FrozenSourceManifest, StorageError> {
    let paths = StoragePaths::at(state_root);
    let directory = SecureDatabaseDirectory::prepare(state_root, false)?;
    let snapshot = directory.read_private_file(name, MAX_FROZEN_MANIFEST_BYTES)?;
    let value = super::legacy::decode_exact_json(&snapshot.bytes).ok_or(
        StorageError::InvalidStorage("frozen manifest JSON is invalid"),
    )?;
    let manifest: FrozenSourceManifest = serde_json::from_value(value.clone())
        .map_err(|_| StorageError::InvalidStorage("frozen manifest fields are invalid"))?;
    if serde_json::to_value(&manifest).ok().as_ref() != Some(&value)
        || manifest.profile != super::LEGACY_EXPORT_PROFILE
        || manifest.count != manifest.rows.len() as u64
        || manifest.rows.len() > MAX_FREEZE_FILES + 1
        || !matches!(manifest.review_result.as_str(), "published" | "degraded")
        || manifest.digest
            != manifest_digest(manifest.generation, &manifest.review_result, &manifest.rows)?
    {
        return Err(StorageError::InvalidStorage(
            "frozen manifest is not canonical",
        ));
    }
    for row in &manifest.rows {
        if row.relative_path.is_empty()
            || row.relative_path[0] == b'/'
            || row
                .relative_path
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b"..")
        {
            return Err(StorageError::InvalidStorage(
                "frozen manifest path is invalid",
            ));
        }
        validate_manifest_row(&paths, row)?;
    }
    validate_manifest_row_order(&manifest)?;
    validate_manifest_journal_set(&paths, &manifest.rows)?;
    Ok(manifest)
}

fn validate_manifest_row_order(manifest: &FrozenSourceManifest) -> Result<(), StorageError> {
    let mut rows = manifest.rows.iter();
    for expected in [
        b"brain/decisions.jsonl".as_slice(),
        b"activity.jsonl".as_slice(),
        b"hooks/lifecycle.json".as_slice(),
    ] {
        if rows.next().map(|row| row.relative_path.as_slice()) != Some(expected) {
            return Err(StorageError::InvalidStorage(
                "frozen manifest order is invalid",
            ));
        }
    }

    let journal_prefix = b"brain/permission-transactions/";
    let mut previous_journal: Option<&[u8]> = None;
    let mut directory_seen = false;
    for row in rows.by_ref() {
        if row.relative_path == b"brain/permission-transactions" {
            if !row.present {
                return Err(StorageError::InvalidStorage(
                    "frozen journal directory must be present",
                ));
            }
            directory_seen = true;
            break;
        }
        let Some(name) = row.relative_path.strip_prefix(journal_prefix) else {
            return Err(StorageError::InvalidStorage(
                "frozen manifest order is invalid",
            ));
        };
        let name = std::str::from_utf8(name)
            .map_err(|_| StorageError::InvalidStorage("frozen journal path is invalid"))?;
        super::legacy::validate_journal_name(name)?;
        if !row.present
            || previous_journal.is_some_and(|previous| previous >= row.relative_path.as_slice())
        {
            return Err(StorageError::InvalidStorage(
                "frozen journal order is invalid",
            ));
        }
        previous_journal = Some(&row.relative_path);
    }
    if !directory_seen {
        return Err(StorageError::InvalidStorage(
            "frozen journal directory row is missing",
        ));
    }

    match manifest.review_result.as_str() {
        "published"
            if rows.next().map(|row| row.relative_path.as_slice())
                == Some(b"review-state.json") => {}
        "degraded" if rows.next().is_none() => return Ok(()),
        _ => return Err(StorageError::InvalidStorage("frozen review row is invalid")),
    }
    if rows.next().is_some() {
        return Err(StorageError::InvalidStorage(
            "frozen manifest has extra rows",
        ));
    }
    Ok(())
}

fn validate_frozen_manifest_for_state(
    paths: &StoragePaths,
    state: &ValidatedState,
) -> Result<FrozenSourceManifest, StorageError> {
    let manifest = FrozenSourceManifest::load_and_validate(&paths.state_root)?;
    validate_frozen_manifest_value_for_state(manifest, state)
}

fn validate_frozen_manifest_value_for_state(
    manifest: FrozenSourceManifest,
    state: &ValidatedState,
) -> Result<FrozenSourceManifest, StorageError> {
    let (manifest_digest, manifest_count) = match &state.freeze_state {
        FreezeState::ManifestBuilding {
            manifest_digest,
            manifest_count,
            ..
        }
        | FreezeState::ManifestVerified {
            manifest_digest,
            manifest_count,
            ..
        }
        | FreezeState::ManifestPublished {
            manifest_digest,
            manifest_count,
            ..
        } => (manifest_digest, *manifest_count),
        _ => {
            return Err(StorageError::InvalidStorage(
                "migration state has no published frozen manifest",
            ));
        }
    };
    let review_result = match state.review_result {
        ReviewMigrationResult::Published { .. } => "published",
        ReviewMigrationResult::Degraded { .. } => "degraded",
        _ => {
            return Err(StorageError::InvalidStorage(
                "migration review result is not final",
            ));
        }
    };
    if manifest.generation != state.generation
        || &manifest.digest != manifest_digest
        || manifest.count != manifest_count
        || manifest.review_result != review_result
    {
        return Err(StorageError::InvalidStorage(
            "frozen manifest does not match migration state",
        ));
    }
    Ok(manifest)
}

fn validate_manifest_row(
    paths: &StoragePaths,
    row: &FrozenManifestRow,
) -> Result<(), StorageError> {
    let directory = row.relative_path == b"brain/permission-transactions";
    if (!row.present
        && (row.device != 0
            || row.inode != 0
            || row.size != 0
            || row.modified_seconds != 0
            || row.modified_nanoseconds != 0
            || row.mode != 0))
        || (row.present && row.mode != if directory { 0o500 } else { 0o400 })
    {
        return Err(StorageError::InvalidStorage(
            "frozen manifest row metadata is invalid",
        ));
    }
    let relative = std::ffi::OsString::from_vec(row.relative_path.clone());
    let path = paths.state_root.join(relative);
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !row.present => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(StorageError::InvalidStorage(
                "frozen legacy path disappeared",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    if !row.present {
        return Err(StorageError::InvalidStorage(
            "absent frozen legacy path was recreated",
        ));
    }
    validate_freeze_path_metadata(&metadata, directory)?;
    if metadata.dev() != row.device
        || metadata.ino() != row.inode
        || metadata.len() != row.size
        || metadata.mtime() != row.modified_seconds
        || metadata.mtime_nsec() != row.modified_nanoseconds
        || metadata.mode() & 0o777 != row.mode
    {
        return Err(StorageError::InvalidStorage("frozen legacy path changed"));
    }
    Ok(())
}

fn validate_manifest_journal_set(
    paths: &StoragePaths,
    rows: &[FrozenManifestRow],
) -> Result<(), StorageError> {
    let expected = rows
        .iter()
        .filter(|row| {
            row.relative_path
                .starts_with(b"brain/permission-transactions/")
                && row.present
        })
        .map(|row| row.relative_path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let actual = fs::read_dir(paths.state_root.join("brain/permission-transactions"))?
        .map(|entry| {
            let name = entry?.file_name();
            let mut path = b"brain/permission-transactions/".to_vec();
            path.extend_from_slice(name.as_bytes());
            Ok(path)
        })
        .collect::<Result<std::collections::BTreeSet<_>, std::io::Error>>()?;
    if actual != expected {
        return Err(StorageError::InvalidStorage(
            "frozen journal source set changed",
        ));
    }
    Ok(())
}

fn freeze_common(state: &FreezeState) -> Result<(&str, &str, &u64, &bool), StorageError> {
    match state {
        FreezeState::Building {
            progress_name,
            source_digest,
            source_count,
            review_included,
        }
        | FreezeState::ProgressReady {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::DirectoryFreezing {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::DirectoryFrozen {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::ManifestBuilding {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::ManifestVerified {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::ManifestPublished {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        } => Ok((progress_name, source_digest, source_count, review_included)),
        FreezeState::Pending => Err(StorageError::InvalidStorage(
            "freeze state is still pending",
        )),
    }
}

fn freeze_progress_identity(state: &FreezeState) -> Option<(u64, u64)> {
    match state {
        FreezeState::ProgressReady {
            progress_device,
            progress_inode,
            ..
        }
        | FreezeState::DirectoryFreezing {
            progress_device,
            progress_inode,
            ..
        }
        | FreezeState::DirectoryFrozen {
            progress_device,
            progress_inode,
            ..
        }
        | FreezeState::ManifestBuilding {
            progress_device,
            progress_inode,
            ..
        }
        | FreezeState::ManifestVerified {
            progress_device,
            progress_inode,
            ..
        }
        | FreezeState::ManifestPublished {
            progress_device,
            progress_inode,
            ..
        } => Some((*progress_device, *progress_inode)),
        FreezeState::Pending | FreezeState::Building { .. } => None,
    }
}

fn copy_freeze_common(
    state: &FreezeState,
    phase: FreezePhase,
) -> Result<FreezeState, StorageError> {
    let (progress_name, source_digest, source_count, review_included) = freeze_common(state)?;
    let fields = (
        progress_name.to_owned(),
        source_digest.to_owned(),
        *source_count,
        *review_included,
    );
    let (progress_device, progress_inode) = freeze_progress_identity(state).ok_or(
        StorageError::InvalidStorage("freeze progress identity is missing"),
    )?;
    Ok(match phase {
        FreezePhase::DirectoryFreezing => FreezeState::DirectoryFreezing {
            progress_name: fields.0,
            source_digest: fields.1,
            source_count: fields.2,
            review_included: fields.3,
            progress_device,
            progress_inode,
        },
        FreezePhase::DirectoryFrozen => FreezeState::DirectoryFrozen {
            progress_name: fields.0,
            source_digest: fields.1,
            source_count: fields.2,
            review_included: fields.3,
            progress_device,
            progress_inode,
        },
    })
}

fn resume_review_build(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    sources: &LegacySourceSet,
    fingerprint: PersistedFingerprint,
    staging_name: String,
) -> Result<LoadedState, StorageError> {
    if review_source_fingerprint(sources)? != fingerprint {
        state.manifest.review_result = ReviewMigrationResult::Degraded {
            fingerprint,
            reason: "source_race".to_owned(),
        };
        return replace_review_result(directory, state);
    }
    let mut capture = ReviewCapture::default();
    match sources.stream_kind_into(LegacySourceKind::ReviewState, &mut capture) {
        Ok(_) => {}
        Err(StorageError::InvalidStorage("invalid legacy review state")) => {
            state.manifest.review_result = ReviewMigrationResult::Degraded {
                fingerprint,
                reason: "malformed".to_owned(),
            };
            return replace_review_result(directory, state);
        }
        Err(error) => return Err(error),
    }
    if review_source_fingerprint(sources)? != fingerprint {
        state.manifest.review_result = ReviewMigrationResult::Degraded {
            fingerprint,
            reason: "source_race".to_owned(),
        };
        return replace_review_result(directory, state);
    }
    let snapshot = capture.snapshot.unwrap_or_default();
    match build_review_staging(paths, directory, state.manifest.generation, &snapshot) {
        Ok((artifact, row_digest, row_count)) => {
            state.manifest.review_result = ReviewMigrationResult::Verified {
                fingerprint,
                staging_name,
                artifact: artifact.into(),
                row_digest,
                row_count,
            };
        }
        Err(StorageError::InvalidStorage("legacy review state cannot be mapped exactly")) => {
            state.manifest.review_result = ReviewMigrationResult::Degraded {
                fingerprint,
                reason: "unmapped".to_owned(),
            };
        }
        Err(error) => return Err(error),
    }
    let state = replace_review_result(directory, state)?;
    if matches!(
        state.manifest.review_result,
        ReviewMigrationResult::Verified { .. }
    ) {
        migration_fault("review-verified");
        publish_verified_review(directory, state)
    } else {
        Ok(state)
    }
}

fn review_source_fingerprint(
    sources: &LegacySourceSet,
) -> Result<PersistedFingerprint, StorageError> {
    sources
        .fingerprints()?
        .into_iter()
        .find(|fingerprint| fingerprint.kind == LegacySourceKind::ReviewState)
        .map(persisted_fingerprint)
        .ok_or(StorageError::InvalidStorage(
            "legacy review fingerprint is missing",
        ))
}

fn review_staging_name(generation: u64) -> Result<CString, StorageError> {
    CString::new(format!(".review.sqlite3.migration-{generation}"))
        .map_err(|_| StorageError::InvalidStorage("review staging name is invalid"))
}

fn freeze_progress_name(generation: u64) -> String {
    format!(".brain.sqlite3.freeze-progress-{generation}")
}

fn manifest_temp_name(generation: u64) -> String {
    format!(".brain.sqlite3.frozen-manifest-{generation}.tmp")
}

fn build_review_staging(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    generation: u64,
    snapshot: &crate::brain::review_state::ReviewStateSnapshot,
) -> Result<(ClosedDatabaseIdentity, String, u64), StorageError> {
    let staging = review_staging_name(generation)?;
    let expected = Connection::open_in_memory()?;
    super::schema::configure_connection(&expected, None)?;
    super::schema::initialize_current(&expected, super::DatabaseKind::Review)?;
    import_review_snapshot(paths, &expected, snapshot)?;
    let (expected_digest, expected_count) = review_row_digest(&expected)?;
    let empty = Connection::open_in_memory()?;
    super::schema::configure_connection(&empty, None)?;
    super::schema::initialize_current(&empty, super::DatabaseKind::Review)?;
    let (empty_digest, empty_count) = review_row_digest(&empty)?;
    let connection = match directory.publication_presence(&staging, REVIEW_DATABASE_NAME)? {
        PublicationPresence::Canonical | PublicationPresence::LinkedPair => {
            return Err(StorageError::InvalidStorage(
                "unverified review canonical database exists",
            ));
        }
        PublicationPresence::Staging => {
            let (artifact, digest, count) = inspect_closed_review(directory, &staging, 1)?;
            if digest == expected_digest && count == expected_count {
                return Ok((artifact, digest, count));
            }
            if digest != empty_digest || count != empty_count {
                return Err(StorageError::InvalidStorage(
                    "building review staging does not match an exact restart boundary",
                ));
            }
            open_owned_review_staging(directory, &staging)?
        }
        PublicationPresence::Neither => {
            let connection = super::create_current_in_directory(
                directory,
                &staging,
                super::DatabaseKind::Review,
            )?;
            close_review_staging(connection, directory, &staging)?;
            migration_fault("review-building");
            open_owned_review_staging(directory, &staging)?
        }
    };
    import_review_snapshot(paths, &connection, snapshot)?;
    let (row_digest, row_count) = review_row_digest(&connection)?;
    close_review_staging(connection, directory, &staging)?;
    migration_fault("after-review-staging-sync");
    let staged_identity = directory.closed_database_identity(&staging)?;
    Ok((staged_identity, row_digest, row_count))
}

fn open_owned_review_staging(
    directory: &SecureDatabaseDirectory,
    staging: &CStr,
) -> Result<Connection, StorageError> {
    let path = directory.path().join(OsStr::from_bytes(staging.to_bytes()));
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_EXRESCODE,
    )?;
    super::schema::configure_connection(&connection, None)?;
    connection.execute_batch("PRAGMA journal_mode = WAL;")?;
    super::schema::verify_current(
        &connection,
        super::DatabaseKind::Review,
        StorageDeadline::after(Duration::from_secs(1)),
    )?;
    directory.validate_after_open(staging)?;
    Ok(connection)
}

fn publish_verified_review(
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
) -> Result<LoadedState, StorageError> {
    let ReviewMigrationResult::Verified {
        fingerprint,
        staging_name,
        artifact,
        row_digest,
        row_count,
    } = state.manifest.review_result.clone()
    else {
        return Ok(state);
    };
    let staging = CString::new(staging_name.as_bytes())
        .map_err(|_| StorageError::InvalidStorage("review staging name is invalid"))?;
    let actual = match directory.publication_presence(&staging, REVIEW_DATABASE_NAME)? {
        PublicationPresence::Staging => {
            let actual = verify_closed_review(directory, &staging, 1, &row_digest, row_count)?;
            if actual != artifact {
                return Err(StorageError::InvalidStorage(
                    "verified review staging identity changed",
                ));
            }
            directory.publish_database(&staging, REVIEW_DATABASE_NAME)?;
            directory.closed_database_identity(REVIEW_DATABASE_NAME)?
        }
        PublicationPresence::LinkedPair => {
            let actual = verify_closed_review(directory, &staging, 2, &row_digest, row_count)?;
            if actual != artifact {
                return Err(StorageError::InvalidStorage(
                    "verified linked review identity changed",
                ));
            }
            directory.finish_linked_publication(&staging, REVIEW_DATABASE_NAME)?;
            directory.closed_database_identity(REVIEW_DATABASE_NAME)?
        }
        PublicationPresence::Canonical => {
            let actual =
                verify_closed_review(directory, REVIEW_DATABASE_NAME, 1, &row_digest, row_count)?;
            ClosedDatabaseIdentity {
                device: actual.device,
                inode: actual.inode,
                size: actual.size,
                modified_seconds: actual.modified_seconds,
                modified_nanoseconds: actual.modified_nanoseconds,
                digest: actual.digest,
            }
        }
        PublicationPresence::Neither => {
            return Err(StorageError::InvalidStorage(
                "verified review database disappeared",
            ));
        }
    };
    if ReviewArtifact::from(actual) != artifact {
        return Err(StorageError::InvalidStorage(
            "published review database identity changed",
        ));
    }
    state.manifest.review_result = ReviewMigrationResult::Published {
        fingerprint,
        staging_name,
        artifact,
        row_digest,
        row_count,
    };
    migration_fault("after-review-publication");
    replace_review_result(directory, state)
}

fn review_row_digest(connection: &Connection) -> Result<(String, u64), StorageError> {
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-review-migration-rows-v1");
    let mut count = 0_u64;
    let mut statement = connection.prepare(
        "SELECT surface, revision, source_high_water, coalesce(last_archive_revision, 0)
         FROM review_meta ORDER BY surface",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        for value in [
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?.to_string(),
            row.get::<_, i64>(2)?.to_string(),
            row.get::<_, i64>(3)?.to_string(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    drop(rows);
    drop(statement);
    let mut statement = connection.prepare(
        "SELECT surface, group_id, source_cursor, disposition, revision
         FROM review_marks ORDER BY surface, group_id, source_cursor",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        for value in [
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?.to_string(),
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?.to_string(),
        ] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        count = count.checked_add(1).ok_or(StorageError::InvalidStorage(
            "review migration row count overflow",
        ))?;
    }
    Ok((format!("{:x}", digest.finalize()), count))
}

fn verify_closed_review(
    directory: &SecureDatabaseDirectory,
    name: &CStr,
    links: u64,
    expected_row_digest: &str,
    expected_row_count: u64,
) -> Result<ReviewArtifact, StorageError> {
    let (identity, row_digest, row_count) = inspect_closed_review(directory, name, links)?;
    if row_digest != expected_row_digest || row_count != expected_row_count {
        return Err(StorageError::InvalidStorage(
            "verified review database content changed",
        ));
    }
    Ok(identity.into())
}

fn inspect_closed_review(
    directory: &SecureDatabaseDirectory,
    name: &CStr,
    links: u64,
) -> Result<(ClosedDatabaseIdentity, String, u64), StorageError> {
    let before = directory.closed_database_identity_with_links(name, links)?;
    let path = directory.path().join(OsStr::from_bytes(name.to_bytes()));
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_EXRESCODE,
    )?;
    let application_id: i32 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    let user_version: i32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let integrity: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    let foreign_key_error = connection
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
        .optional()?;
    super::schema::verify_frozen_schema(
        &connection,
        super::DatabaseKind::Review,
        StorageDeadline::after(Duration::from_secs(1)),
    )?;
    let (row_digest, row_count) = review_row_digest(&connection)?;
    if application_id != super::REVIEW_APPLICATION_ID
        || user_version != super::REVIEW_SCHEMA_VERSION
        || integrity != "ok"
        || foreign_key_error.is_some()
    {
        return Err(StorageError::InvalidStorage(
            "verified review database content changed",
        ));
    }
    drop(connection);
    let after = directory.closed_database_identity_with_links(name, links)?;
    if before != after {
        return Err(StorageError::InvalidStorage(
            "verified review database changed during validation",
        ));
    }
    Ok((after, row_digest, row_count))
}

fn import_review_snapshot(
    paths: &StoragePaths,
    connection: &Connection,
    snapshot: &crate::brain::review_state::ReviewStateSnapshot,
) -> Result<(), StorageError> {
    import_review_snapshot_inner(paths, connection, snapshot).map_err(|error| {
        super::maintenance::map_storage_error(super::StorageOperation::Migration, false, error)
    })
}

fn import_review_snapshot_inner(
    paths: &StoragePaths,
    connection: &Connection,
    snapshot: &crate::brain::review_state::ReviewStateSnapshot,
) -> Result<(), StorageError> {
    let brain = BrainDb::open_published_incomplete(paths)?;
    let high_water = brain.connection.query_row(
        "SELECT activity_high_water FROM schema_meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let mut occurrence = std::collections::BTreeMap::<(ReviewSurface, ReviewKey), i64>::new();
    let mut statement = brain.connection.prepare(
        "SELECT activity_id, event_kind, max(source_cursor)
         FROM activity_events GROUP BY activity_id, event_kind ORDER BY activity_id, event_kind",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let activity_id = row.get::<_, String>(0)?;
        let kind = row.get::<_, String>(1)?;
        let cursor = row.get::<_, i64>(2)?;
        let surfaces: &[ReviewSurface] = match kind.as_str() {
            "decision" => &[ReviewSurface::Attention, ReviewSurface::Recent],
            "diagnostic" => &[ReviewSurface::Diagnostics],
            "lifecycle" => &[],
            _ => {
                return Err(StorageError::InvalidStorage(
                    "staged activity kind is invalid",
                ));
            }
        };
        for surface in surfaces {
            let key = ReviewKey::derive(*surface, activity_id.as_bytes());
            if occurrence.insert((*surface, key), cursor).is_some() {
                return Err(StorageError::InvalidStorage(
                    "legacy review state cannot be mapped exactly",
                ));
            }
        }
    }
    drop(rows);
    drop(statement);
    let mut statement = brain.connection.prepare(
        "SELECT decision_id, source_cursor FROM decision_payloads ORDER BY source_cursor",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let decision_id = row.get::<_, String>(0)?;
        let cursor = row.get::<_, i64>(1)?;
        let payload =
            brain
                .migration_decision_payload(&decision_id)?
                .ok_or(StorageError::InvalidStorage(
                    "staged decision payload is missing",
                ))?;
        let identity = crate::brain::review::review_source_identity(&payload.record);
        let key = ReviewKey::derive(ReviewSurface::Review, &identity);
        if occurrence
            .insert((ReviewSurface::Review, key), cursor)
            .is_some()
        {
            return Err(StorageError::InvalidStorage(
                "legacy review state cannot be mapped exactly",
            ));
        }
    }

    let transaction = connection.unchecked_transaction()?;
    super::maintenance::sqlite_fault("migration-review-body")?;
    for surface in [
        ReviewSurface::Attention,
        ReviewSurface::Review,
        ReviewSurface::Diagnostics,
        ReviewSurface::Recent,
    ] {
        let revision = snapshot.surface_revision(surface);
        if revision > i64::MAX as u64 {
            return Err(StorageError::InvalidStorage(
                "legacy review state cannot be mapped exactly",
            ));
        }
        let items = snapshot.items(surface).collect::<Vec<_>>();
        if revision == 0 && !items.is_empty() {
            return Err(StorageError::InvalidStorage(
                "legacy review state cannot be mapped exactly",
            ));
        }
        let last_archive = snapshot.last_archive(surface);
        if revision == 1
            && !last_archive.is_empty()
            && items.iter().any(|(key, disposition)| {
                *disposition == ReviewDisposition::Archived && !last_archive.contains(key)
            })
        {
            return Err(StorageError::InvalidStorage(
                "legacy review state cannot be mapped exactly",
            ));
        }
        transaction.execute(
            "UPDATE review_meta SET revision = ?1, source_high_water = ?2,
                    last_archive_revision = ?3 WHERE surface = ?4",
            params![
                revision as i64,
                high_water,
                (!last_archive.is_empty()).then_some(revision as i64),
                surface.as_str(),
            ],
        )?;
        for (key, disposition) in items {
            let cursor =
                occurrence
                    .get(&(surface, *key))
                    .copied()
                    .ok_or(StorageError::InvalidStorage(
                        "legacy review state cannot be mapped exactly",
                    ))?;
            let row_revision = if last_archive.contains(key) {
                revision
            } else {
                1
            };
            transaction.execute(
                "INSERT INTO review_marks
                 (surface, group_id, source_cursor, disposition, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    surface.as_str(),
                    key.to_string(),
                    cursor,
                    match disposition {
                        ReviewDisposition::Reviewed => "reviewed",
                        ReviewDisposition::Archived => "archived",
                    },
                    row_revision as i64,
                ],
            )?;
        }
    }
    super::maintenance::sqlite_fault("migration-review-commit")
        .and_then(|()| transaction.commit())
        .map_err(|error| {
            super::maintenance::map_sqlite_error(super::StorageOperation::Migration, true, error)
        })?;
    Ok(())
}

fn close_review_staging(
    connection: Connection,
    directory: &SecureDatabaseDirectory,
    name: &CStr,
) -> Result<(), StorageError> {
    let checkpoint = connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| {
            super::maintenance::map_sqlite_error(super::StorageOperation::Checkpoint, false, error)
        })?;
    if checkpoint != (0, 0, 0) {
        return Err(StorageError::InvalidStorage(
            "review staging WAL checkpoint is incomplete",
        ));
    }
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))?;
    if journal_mode != "delete" {
        return Err(StorageError::InvalidStorage(
            "review staging journal mode did not close cleanly",
        ));
    }
    connection
        .close()
        .map_err(|(_, error)| StorageError::Sqlite(error))?;
    directory.validate_database_without_sidecars(name)?;
    directory.sync_database(name)?;
    Ok(())
}

#[derive(Default)]
struct ReviewCapture {
    snapshot: Option<crate::brain::review_state::ReviewStateSnapshot>,
}

impl LegacyImportSink for ReviewCapture {
    fn decision(&mut self, _decision: LegacyDecision) -> Result<(), StorageError> {
        Ok(())
    }

    fn activity(&mut self, _activity: ActivityEvent) -> Result<(), StorageError> {
        Ok(())
    }

    fn lifecycle(&mut self, _lifecycle: LifecycleSnapshot) -> Result<(), StorageError> {
        Ok(())
    }

    fn journal(&mut self, _journal: PermissionTransactionJournal) -> Result<(), StorageError> {
        Ok(())
    }

    fn review(
        &mut self,
        review: crate::brain::review_state::ReviewStateSnapshot,
    ) -> Result<(), StorageError> {
        self.snapshot = Some(review);
        Ok(())
    }
}

fn replace_review_result(
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
) -> Result<LoadedState, StorageError> {
    let temporary = review_result_temp_name(state.manifest.generation)?;
    let replacement = directory.write_new_private_file(&temporary, &state.manifest.encoded()?)?;
    migration_fault("after-review-result-state-temp-sync");
    state.identity = directory.publish_private_replacement(
        MIGRATION_STATE_NAME,
        state.identity,
        &temporary,
        replacement,
    )?;
    state.manifest.allowed_temporary = None;
    Ok(state)
}

fn freeze_state_temp_name(generation: u64) -> Result<CString, StorageError> {
    CString::new(format!(".brain.sqlite3.freeze-state-{generation}.tmp"))
        .map_err(|_| StorageError::InvalidStorage("freeze state temporary name is invalid"))
}

fn replace_freeze_state(
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    next: FreezeState,
    fault: &str,
) -> Result<LoadedState, StorageError> {
    if !valid_freeze_transition(&state.manifest.freeze_state, &next) {
        return Err(StorageError::InvalidStorage(
            "migration freeze state transition is invalid",
        ));
    }
    state.manifest.freeze_state = next;
    let temporary = freeze_state_temp_name(state.manifest.generation)?;
    let replacement = directory.write_new_private_file(&temporary, &state.manifest.encoded()?)?;
    migration_fault(fault);
    state.identity = directory.publish_private_replacement(
        MIGRATION_STATE_NAME,
        state.identity,
        &temporary,
        replacement,
    )?;
    Ok(state)
}

fn recover_pending_freeze_state(
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    publish: bool,
) -> Result<LoadedState, StorageError> {
    let temporary = freeze_state_temp_name(state.manifest.generation)?;
    if !directory.private_file_present(&temporary)? {
        return Ok(state);
    }
    let snapshot = directory.read_private_file(&temporary, MAX_MIGRATION_STATE_BYTES)?;
    let recovered = decode_state(&snapshot.bytes)?;
    let mut same = recovered.clone();
    same.freeze_state = state.manifest.freeze_state.clone();
    same.allowed_temporary = None;
    let mut current = state.manifest.clone();
    current.allowed_temporary = None;
    if same != current
        || !valid_freeze_transition(&state.manifest.freeze_state, &recovered.freeze_state)
    {
        return Err(StorageError::InvalidStorage(
            "pending freeze state transition is not canonical",
        ));
    }
    if publish {
        state.identity = directory.publish_private_replacement(
            MIGRATION_STATE_NAME,
            state.identity,
            &temporary,
            snapshot.identity,
        )?;
        state.manifest = recovered;
    } else {
        state.manifest = recovered;
        state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
    }
    Ok(state)
}

fn valid_freeze_transition(current: &FreezeState, next: &FreezeState) -> bool {
    matches!(
        (current, next),
        (FreezeState::Pending, FreezeState::Building { .. })
            | (
                FreezeState::Building { .. },
                FreezeState::ProgressReady { .. }
            )
            | (
                FreezeState::ProgressReady { .. },
                FreezeState::DirectoryFreezing { .. }
            )
            | (
                FreezeState::DirectoryFreezing { .. },
                FreezeState::DirectoryFrozen { .. }
            )
            | (
                FreezeState::DirectoryFrozen { .. },
                FreezeState::ManifestBuilding { .. }
            )
            | (
                FreezeState::ManifestBuilding { .. },
                FreezeState::ManifestVerified { .. }
            )
            | (
                FreezeState::ManifestVerified { .. },
                FreezeState::ManifestPublished { .. }
            )
    )
}

fn recover_pending_review_result(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    publish: bool,
) -> Result<LoadedState, StorageError> {
    let temporary = review_result_temp_name(state.manifest.generation)?;
    if !directory.private_file_present(&temporary)? {
        return Ok(state);
    }
    let snapshot = directory.read_private_file(&temporary, MAX_MIGRATION_STATE_BYTES)?;
    let recovered = decode_state(&snapshot.bytes)?;
    let mut same_fields = recovered.clone();
    same_fields.review_result = state.manifest.review_result.clone();
    same_fields.allowed_temporary = None;
    let mut current = state.manifest.clone();
    current.allowed_temporary = None;
    if same_fields != current
        || !valid_review_result_transition(&state.manifest.review_result, &recovered.review_result)
    {
        return Err(StorageError::InvalidStorage(
            "pending review result transition is not canonical",
        ));
    }
    validate_review_result(&recovered.review_result, recovered.generation)?;
    validate_review_transition_evidence(
        paths,
        directory,
        &state.manifest.review_result,
        &recovered.review_result,
        recovered.generation,
    )?;
    if publish {
        state.identity = directory.publish_private_replacement(
            MIGRATION_STATE_NAME,
            state.identity,
            &temporary,
            snapshot.identity,
        )?;
        state.manifest = recovered;
    } else {
        state.manifest = recovered;
        state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
    }
    Ok(state)
}

fn valid_review_result_transition(
    from: &ReviewMigrationResult,
    to: &ReviewMigrationResult,
) -> bool {
    match (from, to) {
        (
            ReviewMigrationResult::Pending,
            ReviewMigrationResult::Building { .. } | ReviewMigrationResult::Degraded { .. },
        )
        | (
            ReviewMigrationResult::Building { .. },
            ReviewMigrationResult::Verified { .. } | ReviewMigrationResult::Degraded { .. },
        )
        | (ReviewMigrationResult::Verified { .. }, ReviewMigrationResult::Published { .. }) => true,
        (
            ReviewMigrationResult::Published { .. },
            ReviewMigrationResult::Degraded { reason, .. },
        ) => reason == "source_race",
        _ => false,
    }
}

fn validate_review_transition_evidence(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    from: &ReviewMigrationResult,
    to: &ReviewMigrationResult,
    generation: u64,
) -> Result<(), StorageError> {
    let sources = LegacySourceSet::at(&paths.state_root)?;
    let current_fingerprint = review_source_fingerprint(&sources)?;
    let staging = review_staging_name(generation)?;
    let presence = directory.publication_presence(&staging, REVIEW_DATABASE_NAME)?;
    match (from, to) {
        (ReviewMigrationResult::Pending, ReviewMigrationResult::Building { fingerprint, .. }) => {
            if presence != PublicationPresence::Neither || &current_fingerprint != fingerprint {
                return Err(StorageError::InvalidStorage(
                    "pending review build transition evidence changed",
                ));
            }
        }
        (
            ReviewMigrationResult::Pending,
            ReviewMigrationResult::Degraded {
                fingerprint,
                reason,
            },
        ) => {
            let source_matches = &current_fingerprint == fingerprint;
            if presence != PublicationPresence::Neither
                || (reason == "source_race") == source_matches
            {
                return Err(StorageError::InvalidStorage(
                    "pending review degradation evidence changed",
                ));
            }
        }
        (
            ReviewMigrationResult::Building { fingerprint, .. },
            ReviewMigrationResult::Verified {
                fingerprint: recovered_fingerprint,
                artifact,
                row_digest,
                row_count,
                ..
            },
        ) => {
            if fingerprint != recovered_fingerprint
                || &current_fingerprint != fingerprint
                || presence != PublicationPresence::Staging
            {
                return Err(StorageError::InvalidStorage(
                    "verified review transition evidence changed",
                ));
            }
            let actual = verify_closed_review(directory, &staging, 1, row_digest, *row_count)?;
            if actual != *artifact {
                return Err(StorageError::InvalidStorage(
                    "verified review transition identity changed",
                ));
            }
        }
        (
            ReviewMigrationResult::Building { fingerprint, .. },
            ReviewMigrationResult::Degraded {
                fingerprint: recovered_fingerprint,
                reason,
            },
        ) => {
            let source_matches = &current_fingerprint == fingerprint;
            if fingerprint != recovered_fingerprint
                || (reason == "source_race") == source_matches
                || !matches!(
                    presence,
                    PublicationPresence::Neither | PublicationPresence::Staging
                )
            {
                return Err(StorageError::InvalidStorage(
                    "building review degradation evidence changed",
                ));
            }
        }
        (
            ReviewMigrationResult::Verified {
                fingerprint,
                artifact,
                row_digest,
                row_count,
                ..
            },
            ReviewMigrationResult::Published {
                fingerprint: recovered_fingerprint,
                artifact: recovered_artifact,
                row_digest: recovered_digest,
                row_count: recovered_count,
                ..
            },
        ) => {
            if fingerprint != recovered_fingerprint
                || artifact != recovered_artifact
                || row_digest != recovered_digest
                || row_count != recovered_count
                || &current_fingerprint != fingerprint
                || presence != PublicationPresence::Canonical
            {
                return Err(StorageError::InvalidStorage(
                    "published review transition evidence changed",
                ));
            }
            let actual =
                verify_closed_review(directory, REVIEW_DATABASE_NAME, 1, row_digest, *row_count)?;
            if actual != *artifact {
                return Err(StorageError::InvalidStorage(
                    "published review transition identity changed",
                ));
            }
        }
        (
            ReviewMigrationResult::Published {
                fingerprint,
                artifact,
                row_digest,
                row_count,
                ..
            },
            ReviewMigrationResult::Degraded {
                fingerprint: recovered_fingerprint,
                reason,
            },
        ) => {
            if reason != "source_race"
                || fingerprint != recovered_fingerprint
                || &current_fingerprint == fingerprint
                || presence != PublicationPresence::Canonical
            {
                return Err(StorageError::InvalidStorage(
                    "published review degradation evidence changed",
                ));
            }
            let actual =
                verify_closed_review(directory, REVIEW_DATABASE_NAME, 1, row_digest, *row_count)?;
            if actual != *artifact {
                return Err(StorageError::InvalidStorage(
                    "published degraded review identity changed",
                ));
            }
        }
        _ => {
            return Err(StorageError::InvalidStorage(
                "pending review result transition is invalid",
            ));
        }
    }
    Ok(())
}

fn review_result_temp_name(generation: u64) -> Result<CString, StorageError> {
    CString::new(format!(".brain.sqlite3.review-result-{generation}.tmp"))
        .map_err(|_| StorageError::InvalidStorage("review result temporary name is invalid"))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedFingerprint {
    kind: String,
    relative_path: String,
    present: bool,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BrainArtifact {
    device: u64,
    inode: u64,
    content_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct MigrationState {
    schema_version: u32,
    generation: u64,
    creator_pid: u32,
    status: String,
    staging_name: String,
    fingerprint_digest: String,
    fingerprint_count: u64,
    descriptors: Vec<PersistedFingerprint>,
    accounting: Option<MigrationAccounting>,
    brain_artifact: Option<BrainArtifact>,
    review_result: ReviewMigrationResult,
    freeze_state: FreezeState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum FreezeState {
    Pending,
    Building {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
    },
    ProgressReady {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        progress_device: u64,
        progress_inode: u64,
    },
    DirectoryFreezing {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        progress_device: u64,
        progress_inode: u64,
    },
    DirectoryFrozen {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        progress_device: u64,
        progress_inode: u64,
    },
    ManifestBuilding {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        manifest_digest: String,
        manifest_count: u64,
        manifest_temporary: String,
        progress_device: u64,
        progress_inode: u64,
    },
    ManifestVerified {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        manifest_digest: String,
        manifest_count: u64,
        manifest_temporary: String,
        progress_device: u64,
        progress_inode: u64,
    },
    ManifestPublished {
        progress_name: String,
        source_digest: String,
        source_count: u64,
        review_included: bool,
        manifest_digest: String,
        manifest_count: u64,
        progress_device: u64,
        progress_inode: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ReviewMigrationResult {
    Pending,
    Building {
        fingerprint: PersistedFingerprint,
        staging_name: String,
    },
    Verified {
        fingerprint: PersistedFingerprint,
        staging_name: String,
        artifact: ReviewArtifact,
        row_digest: String,
        row_count: u64,
    },
    Published {
        fingerprint: PersistedFingerprint,
        staging_name: String,
        artifact: ReviewArtifact,
        row_digest: String,
        row_count: u64,
    },
    Degraded {
        fingerprint: PersistedFingerprint,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReviewArtifact {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    digest: String,
}

impl From<ClosedDatabaseIdentity> for ReviewArtifact {
    fn from(identity: ClosedDatabaseIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
            size: identity.size,
            modified_seconds: identity.modified_seconds,
            modified_nanoseconds: identity.modified_nanoseconds,
            digest: identity.digest,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct MigrationAccounting {
    sources: SourceCounts,
    imports: ImportCounts,
    skips: SkipCounts,
    activity: ActivityAccounting,
    lifecycle: LifecycleAccounting,
    historical: HistoricalCounts,
    table_counts: TableCounts,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceCounts {
    decisions: u64,
    activities: u64,
    lifecycle_snapshots: u64,
    journals: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportCounts {
    decisions: u64,
    activities: u64,
    lifecycle_snapshots: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SkipCounts {
    incomplete_proposals: u64,
    unanchored_audits: u64,
    unmatched_journals: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActivityAccounting {
    count: u64,
    high_water: u64,
    first_cursor: u64,
    last_cursor: u64,
    order_digest: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct LifecycleAccounting {
    next_sequence: u64,
    sessions: u64,
    leases: u64,
    turns: u64,
    subagents: u64,
    invocations: u64,
    invocation_steps: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoricalCounts {
    proposal_terminal: u64,
    journal_correlated: u64,
    lifecycle_correlated: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TableCounts {
    permission_attempts: u64,
    decision_identities: u64,
    decision_payloads: u64,
    activity_events: u64,
    permission_commits: u64,
    historical_permission_authority: u64,
    lifecycle_sessions: u64,
    lifecycle_leases: u64,
    lifecycle_turns: u64,
    lifecycle_subagents: u64,
    lifecycle_invocations: u64,
    lifecycle_invocation_steps: u64,
}

struct LoadedState {
    manifest: ValidatedState,
    identity: PrivateFileIdentity,
}

#[derive(Clone, Eq, PartialEq)]
struct ValidatedState {
    generation: u64,
    creator_pid: u32,
    status: MigrationStatus,
    staging_name: String,
    fingerprint_digest: String,
    fingerprint_count: u64,
    descriptors: Vec<PersistedFingerprint>,
    accounting: Option<MigrationAccounting>,
    brain_artifact: Option<BrainArtifact>,
    review_result: ReviewMigrationResult,
    freeze_state: FreezeState,
    allowed_temporary: Option<String>,
}

impl ValidatedState {
    fn staging_cstring(&self) -> Result<CString, StorageError> {
        CString::new(self.staging_name.as_bytes())
            .map_err(|_| StorageError::InvalidStorage("migration staging name is invalid"))
    }

    fn encoded(&self) -> Result<Vec<u8>, StorageError> {
        serde_json::to_vec(&MigrationState {
            schema_version: 1,
            generation: self.generation,
            creator_pid: self.creator_pid,
            status: status_name(self.status).to_owned(),
            staging_name: self.staging_name.clone(),
            fingerprint_digest: self.fingerprint_digest.clone(),
            fingerprint_count: self.fingerprint_count,
            descriptors: self.descriptors.clone(),
            accounting: self.accounting.clone(),
            brain_artifact: self.brain_artifact.clone(),
            review_result: self.review_result.clone(),
            freeze_state: self.freeze_state.clone(),
        })
        .map_err(|_| StorageError::InvalidStorage("migration state serialization failed"))
    }
}

pub(super) fn hook_preflight(paths: &StoragePaths) -> Result<(), StorageError> {
    let directory = match SecureDatabaseDirectory::prepare(&paths.state_root, false) {
        Ok(directory) => directory,
        Err(SecurityError::Missing) => return Err(StorageError::MigrationRequired),
        Err(error) => return Err(error.into()),
    };
    match load_state(&directory)? {
        Some(state) => {
            validate_hook_state(&directory, &state.manifest)?;
            if state.manifest.status == MigrationStatus::Complete {
                Ok(())
            } else {
                Err(StorageError::MigrationActive)
            }
        }
        None => {
            if directory.private_file_present(BRAIN_DATABASE_NAME)? {
                Ok(())
            } else {
                Err(StorageError::MigrationRequired)
            }
        }
    }
}

fn validate_hook_state(
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    let presence = publication_presence(directory, state)?;
    let valid = match state.status {
        MigrationStatus::Building => matches!(
            presence,
            PublicationPresence::Neither | PublicationPresence::Staging
        ),
        MigrationStatus::Verified => presence != PublicationPresence::Neither,
        MigrationStatus::BrainPublishedIncomplete => presence == PublicationPresence::Canonical,
        MigrationStatus::LegacyFrozen => false,
        MigrationStatus::Complete => presence == PublicationPresence::Canonical,
    };
    if valid {
        Ok(())
    } else {
        Err(StorageError::InvalidStorage(
            "migration coordinator database set is inconsistent",
        ))
    }
}

fn inspect_in_directory(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
) -> Result<MigrationStatus, StorageError> {
    let Some(state) = load_state(directory)? else {
        reject_unmanaged_migration_entries(directory)?;
        if !directory.private_file_present(BRAIN_DATABASE_NAME)? {
            return Ok(MigrationStatus::Building);
        }
        let deadline = StorageDeadline::after(Duration::from_secs(1));
        drop(BrainDb::open_current(paths, OpenRole::NonHook, deadline)?);
        return Ok(MigrationStatus::Complete);
    };
    let state = recover_pending_state_transition(paths, directory, state, false)?;
    let state = recover_pending_review_result(paths, directory, state, false)?;
    let state = recover_pending_freeze_state(directory, state, false)?;
    let state = recover_pending_building_rebase(paths, directory, state, false)?;
    validate_state(paths, directory, &state.manifest)?;
    if matches!(
        state.manifest.freeze_state,
        FreezeState::ManifestPublished { .. }
    ) {
        validate_frozen_manifest_for_state(paths, &state.manifest)?;
    }
    if state.manifest.status == MigrationStatus::Verified {
        match publication_presence(directory, &state.manifest)? {
            PublicationPresence::Canonical => {
                validate_published(paths, state.manifest.generation)?;
                return Ok(MigrationStatus::BrainPublishedIncomplete);
            }
            PublicationPresence::LinkedPair => {
                return Ok(MigrationStatus::BrainPublishedIncomplete);
            }
            PublicationPresence::Neither | PublicationPresence::Staging => {}
        }
    }
    Ok(state.manifest.status)
}

fn create_initial_state(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
) -> Result<LoadedState, StorageError> {
    let sources = LegacySourceSet::at(&paths.state_root)?;
    let generation = next_generation()?;
    let creator_pid = std::process::id();
    let staging_name = staging_name(creator_pid, generation)?
        .into_string()
        .map_err(|_| StorageError::InvalidStorage("migration staging name is invalid"))?;
    let fingerprints = brain_source_fingerprint_state(&sources)?;
    let manifest = ValidatedState {
        generation,
        creator_pid,
        status: MigrationStatus::Building,
        staging_name,
        fingerprint_digest: fingerprints.digest,
        fingerprint_count: fingerprints.count,
        descriptors: fingerprints.descriptors,
        accounting: None,
        brain_artifact: None,
        review_result: ReviewMigrationResult::Pending,
        freeze_state: FreezeState::Pending,
        allowed_temporary: None,
    };
    let identity = directory.write_new_private_file(MIGRATION_STATE_NAME, &manifest.encoded()?)?;
    Ok(LoadedState { manifest, identity })
}

fn building_rebase_temp_name(generation: u64) -> Result<CString, StorageError> {
    CString::new(format!(
        ".brain.sqlite3.migration-state-{generation}-building-rebase.tmp"
    ))
    .map_err(|_| StorageError::InvalidStorage("migration state temporary name is invalid"))
}

fn recover_pending_building_rebase(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    publish: bool,
) -> Result<LoadedState, StorageError> {
    let temporary = building_rebase_temp_name(state.manifest.generation)?;
    if !directory.private_file_present(&temporary)? {
        return Ok(state);
    }
    if state.manifest.status != MigrationStatus::Building {
        return Err(StorageError::InvalidStorage(
            "building rebase temporary exists outside Building state",
        ));
    }
    let snapshot = directory.read_private_file(&temporary, MAX_MIGRATION_STATE_BYTES)?;
    let recovered = decode_state(&snapshot.bytes)?;
    let mut expected = state.manifest.clone();
    expected.fingerprint_digest = recovered.fingerprint_digest.clone();
    expected.fingerprint_count = recovered.fingerprint_count;
    expected.descriptors = recovered.descriptors.clone();
    if recovered != expected {
        return Err(StorageError::InvalidStorage(
            "pending Building source rebase is not canonical",
        ));
    }
    validate_building_source_advance(
        &state.manifest,
        &SourceFingerprintState {
            digest: recovered.fingerprint_digest.clone(),
            count: recovered.fingerprint_count,
            descriptors: recovered.descriptors.clone(),
        },
    )?;
    state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
    validate_coordinator_metadata(paths, directory, &state.manifest)?;
    if publish {
        state.identity = directory.publish_private_replacement(
            MIGRATION_STATE_NAME,
            state.identity,
            &temporary,
            snapshot.identity,
        )?;
    }
    state.manifest = recovered;
    if !publish {
        state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
    }
    Ok(state)
}

fn rebase_building_sources(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
) -> Result<LoadedState, StorageError> {
    let _guard = LegacyWriterGuard::acquire(
        &paths.state_root,
        StorageDeadline::after(Duration::from_secs(5)),
    )?;
    let sources = LegacySourceSet::at(&paths.state_root)?;
    let fingerprints = brain_source_fingerprint_state(&sources)?;
    if state.manifest.fingerprint_digest == fingerprints.digest
        && state.manifest.fingerprint_count == fingerprints.count
        && state.manifest.descriptors == fingerprints.descriptors
    {
        return Ok(state);
    }
    validate_building_source_advance(&state.manifest, &fingerprints)?;
    state.manifest.fingerprint_digest = fingerprints.digest;
    state.manifest.fingerprint_count = fingerprints.count;
    state.manifest.descriptors = fingerprints.descriptors;
    let temporary = building_rebase_temp_name(state.manifest.generation)?;
    let replacement = directory.write_new_private_file(&temporary, &state.manifest.encoded()?)?;
    migration_fault("after-building-rebase-state-temp-sync");
    state.identity = directory.publish_private_replacement(
        MIGRATION_STATE_NAME,
        state.identity,
        &temporary,
        replacement,
    )?;
    Ok(state)
}

fn transition_state(
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    status: MigrationStatus,
) -> Result<LoadedState, StorageError> {
    let previous = state.manifest.status;
    state.manifest.status = status;
    state.manifest.allowed_temporary = None;
    let temporary = state_temp_name(state.manifest.generation, previous, status)?;
    let replacement = directory.write_new_private_file(&temporary, &state.manifest.encoded()?)?;
    migration_fault(match status {
        MigrationStatus::Verified => "after-verified-state-temp-sync",
        MigrationStatus::BrainPublishedIncomplete => "after-published-state-temp-sync",
        MigrationStatus::LegacyFrozen => "after-legacy-frozen-state-temp-sync",
        MigrationStatus::Complete => "after-complete-state-temp-sync",
        MigrationStatus::Building => "unreachable-state-transition",
    });
    state.identity = directory.publish_private_replacement(
        MIGRATION_STATE_NAME,
        state.identity,
        &temporary,
        replacement,
    )?;
    Ok(state)
}

fn recover_pending_state_transition(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    mut state: LoadedState,
    publish: bool,
) -> Result<LoadedState, StorageError> {
    let Some(next_status) = next_status(state.manifest.status) else {
        return Ok(state);
    };
    let temporary = state_temp_name(
        state.manifest.generation,
        state.manifest.status,
        next_status,
    )?;
    if !directory.private_file_present(&temporary)? {
        return Ok(state);
    }
    let snapshot = directory.read_private_file(&temporary, MAX_MIGRATION_STATE_BYTES)?;
    let mut expected = state.manifest.clone();
    expected.status = next_status;
    expected.allowed_temporary = None;
    let recovered = decode_state(&snapshot.bytes)?;
    if next_status == MigrationStatus::Verified {
        expected.accounting = recovered.accounting.clone();
        expected.brain_artifact = recovered.brain_artifact.clone();
    }
    if recovered != expected {
        return Err(StorageError::InvalidStorage(
            "pending migration state transition is not canonical",
        ));
    }
    if state.manifest.status == MigrationStatus::Building {
        if !publish {
            state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
            return Ok(state);
        }
        validate_source_fingerprints(paths, &state.manifest)?;
        if publication_presence(directory, &state.manifest)? != PublicationPresence::Staging {
            return Err(StorageError::InvalidStorage(
                "pending verified transition has no exclusive staging database",
            ));
        }
        let staging_name = state.manifest.staging_cstring()?;
        directory.validate_database_without_sidecars(&staging_name)?;
        let staging =
            BrainDb::open_staging_incomplete(paths, &staging_name, state.manifest.generation)?;
        verify_staging(&staging)?;
        directory.remove_private_file(&temporary, snapshot.identity)?;
        staging.discard_staging(paths, &staging_name)?;
        return Ok(state);
    }
    validate_transition_target(paths, directory, &recovered)?;
    if publish {
        state.identity = directory.publish_private_replacement(
            MIGRATION_STATE_NAME,
            state.identity,
            &temporary,
            snapshot.identity,
        )?;
        state.manifest = recovered;
    } else {
        state.manifest = recovered;
        state.manifest.allowed_temporary = Some(temporary.to_string_lossy().into_owned());
    }
    Ok(state)
}

fn load_state(directory: &SecureDatabaseDirectory) -> Result<Option<LoadedState>, StorageError> {
    if !directory.private_file_present(MIGRATION_STATE_NAME)? {
        return Ok(None);
    }
    let snapshot = directory.read_private_file(MIGRATION_STATE_NAME, MAX_MIGRATION_STATE_BYTES)?;
    let manifest = decode_state(&snapshot.bytes)?;
    Ok(Some(LoadedState {
        manifest,
        identity: snapshot.identity,
    }))
}

fn decode_state(bytes: &[u8]) -> Result<ValidatedState, StorageError> {
    let value = super::legacy::decode_exact_json(bytes).ok_or(StorageError::InvalidStorage(
        "migration state JSON is invalid",
    ))?;
    let encoded: MigrationState = serde_json::from_value(value.clone())
        .map_err(|_| StorageError::InvalidStorage("migration state fields are invalid"))?;
    if serde_json::to_value(&encoded).ok().as_ref() != Some(&value)
        || encoded.schema_version != 1
        || encoded.generation == 0
        || encoded.generation > i64::MAX as u64
        || encoded.creator_pid == 0
        || encoded.staging_name
            != staging_name(encoded.creator_pid, encoded.generation)?.to_string_lossy()
        || encoded.fingerprint_digest.len() != 64
        || !encoded
            .fingerprint_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || encoded.fingerprint_count < 4
        || encoded.descriptors.len() != 4
        || encoded
            .descriptors
            .iter()
            .map(|fingerprint| {
                (
                    fingerprint.kind.as_str(),
                    fingerprint.relative_path.as_str(),
                )
            })
            .ne([
                ("decisions", "brain/decisions.jsonl"),
                ("activity", "activity.jsonl"),
                ("lifecycle", "hooks/lifecycle.json"),
                ("permission_transactions", "brain/permission-transactions"),
            ])
    {
        return Err(StorageError::InvalidStorage(
            "migration state is not canonical",
        ));
    }
    let status = parse_status(&encoded.status)?;
    if match status {
        MigrationStatus::Building => encoded.accounting.is_some(),
        MigrationStatus::Verified
        | MigrationStatus::BrainPublishedIncomplete
        | MigrationStatus::LegacyFrozen
        | MigrationStatus::Complete => encoded.accounting.is_none(),
    } {
        return Err(StorageError::InvalidStorage(
            "migration accounting is invalid",
        ));
    }
    if let Some(accounting) = encoded.accounting.as_ref() {
        validate_accounting(accounting)?;
    }
    let artifact_required = status != MigrationStatus::Building;
    if artifact_required != encoded.brain_artifact.is_some()
        || encoded.brain_artifact.as_ref().is_some_and(|artifact| {
            artifact.device == 0
                || artifact.inode == 0
                || artifact.content_digest.len() != 64
                || !artifact
                    .content_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        return Err(StorageError::InvalidStorage(
            "migration Brain artifact is invalid",
        ));
    }
    validate_review_result(&encoded.review_result, encoded.generation)?;
    validate_freeze_state(
        &encoded.freeze_state,
        &encoded.review_result,
        encoded.generation,
        status,
    )?;
    Ok(ValidatedState {
        generation: encoded.generation,
        creator_pid: encoded.creator_pid,
        status,
        staging_name: encoded.staging_name,
        fingerprint_digest: encoded.fingerprint_digest,
        fingerprint_count: encoded.fingerprint_count,
        descriptors: encoded.descriptors,
        accounting: encoded.accounting,
        brain_artifact: encoded.brain_artifact,
        review_result: encoded.review_result,
        freeze_state: encoded.freeze_state,
        allowed_temporary: None,
    })
}

fn validate_freeze_state(
    freeze: &FreezeState,
    review_result: &ReviewMigrationResult,
    generation: u64,
    top: MigrationStatus,
) -> Result<(), StorageError> {
    let expected_progress = freeze_progress_name(generation);
    let valid_digest = |digest: &str| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    let review_is_published = matches!(review_result, ReviewMigrationResult::Published { .. });
    let common = |progress: &str, digest: &str, count: u64, review_included: bool| {
        progress == expected_progress
            && valid_digest(digest)
            && count <= MAX_FREEZE_FILES as u64
            && review_included == review_is_published
    };
    let valid = match freeze {
        FreezeState::Pending => matches!(
            top,
            MigrationStatus::Building
                | MigrationStatus::Verified
                | MigrationStatus::BrainPublishedIncomplete
        ),
        FreezeState::Building {
            progress_name,
            source_digest,
            source_count,
            review_included,
        }
        | FreezeState::ProgressReady {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::DirectoryFreezing {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        }
        | FreezeState::DirectoryFrozen {
            progress_name,
            source_digest,
            source_count,
            review_included,
            ..
        } => {
            top == MigrationStatus::BrainPublishedIncomplete
                && common(
                    progress_name,
                    source_digest,
                    *source_count,
                    *review_included,
                )
        }
        FreezeState::ManifestBuilding {
            progress_name,
            source_digest,
            source_count,
            review_included,
            manifest_digest,
            manifest_count,
            manifest_temporary,
            ..
        }
        | FreezeState::ManifestVerified {
            progress_name,
            source_digest,
            source_count,
            review_included,
            manifest_digest,
            manifest_count,
            manifest_temporary,
            ..
        } => {
            top == MigrationStatus::BrainPublishedIncomplete
                && common(
                    progress_name,
                    source_digest,
                    *source_count,
                    *review_included,
                )
                && valid_digest(manifest_digest)
                && source_count.checked_add(1) == Some(*manifest_count)
                && manifest_temporary == &manifest_temp_name(generation)
        }
        FreezeState::ManifestPublished {
            progress_name,
            source_digest,
            source_count,
            review_included,
            manifest_digest,
            manifest_count,
            ..
        } => {
            matches!(
                top,
                MigrationStatus::BrainPublishedIncomplete
                    | MigrationStatus::LegacyFrozen
                    | MigrationStatus::Complete
            ) && common(
                progress_name,
                source_digest,
                *source_count,
                *review_included,
            ) && valid_digest(manifest_digest)
                && source_count.checked_add(1) == Some(*manifest_count)
        }
    };
    let identity_valid = freeze_progress_identity(freeze)
        .map(|(device, inode)| device != 0 && inode != 0)
        .unwrap_or(matches!(
            freeze,
            FreezeState::Pending | FreezeState::Building { .. }
        ));
    (valid && identity_valid)
        .then_some(())
        .ok_or(StorageError::InvalidStorage(
            "migration freeze state is invalid",
        ))
}

fn validate_review_result(
    result: &ReviewMigrationResult,
    generation: u64,
) -> Result<(), StorageError> {
    let validate_fingerprint = |fingerprint: &PersistedFingerprint| {
        (fingerprint.kind == "review_state" && fingerprint.relative_path == "review-state.json")
            .then_some(())
            .ok_or(StorageError::InvalidStorage(
                "review migration fingerprint is invalid",
            ))
    };
    match result {
        ReviewMigrationResult::Pending => Ok(()),
        ReviewMigrationResult::Building {
            fingerprint,
            staging_name,
        } => {
            validate_fingerprint(fingerprint)?;
            if staging_name != &review_staging_name(generation)?.to_string_lossy() {
                return Err(StorageError::InvalidStorage(
                    "building review staging name is invalid",
                ));
            }
            Ok(())
        }
        ReviewMigrationResult::Degraded {
            fingerprint,
            reason,
        } => {
            validate_fingerprint(fingerprint)?;
            if !matches!(reason.as_str(), "malformed" | "unmapped" | "source_race") {
                return Err(StorageError::InvalidStorage(
                    "review migration degradation is invalid",
                ));
            }
            Ok(())
        }
        ReviewMigrationResult::Verified {
            fingerprint,
            staging_name,
            artifact,
            row_digest,
            ..
        }
        | ReviewMigrationResult::Published {
            fingerprint,
            staging_name,
            artifact,
            row_digest,
            ..
        } => {
            validate_fingerprint(fingerprint)?;
            if staging_name != &review_staging_name(generation)?.to_string_lossy()
                || artifact.digest.len() != 64
                || row_digest.len() != 64
                || !artifact
                    .digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                || !row_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(StorageError::InvalidStorage(
                    "published review migration identity is invalid",
                ));
            }
            Ok(())
        }
    }
}

fn validate_state(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    validate_coordinator_metadata(paths, directory, state)?;
    if state.status == MigrationStatus::Complete {
        drop(BrainDb::open_current(
            paths,
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(2)),
        )?);
        return Ok(());
    }
    if matches!(state.freeze_state, FreezeState::Pending) {
        validate_source_fingerprints(paths, state)?;
    }
    if let ReviewMigrationResult::Published { artifact, .. } = &state.review_result {
        let actual =
            ReviewArtifact::from(directory.closed_database_identity(REVIEW_DATABASE_NAME)?);
        if &actual != artifact {
            return Err(StorageError::InvalidStorage(
                "published review database identity changed",
            ));
        }
    }
    let Some(accounting) = state.accounting.as_ref() else {
        return Ok(());
    };
    match publication_presence(directory, state)? {
        PublicationPresence::Staging => {
            let name = state.staging_cstring()?;
            let database = BrainDb::open_staging_incomplete(paths, &name, state.generation)?;
            if let Some(artifact) = &state.brain_artifact {
                validate_brain_artifact(&database, artifact)?;
            }
            validate_database_accounting(paths, &database, accounting)
        }
        PublicationPresence::LinkedPair => {
            let name = state.staging_cstring()?;
            validate_linked_brain_artifact(
                directory,
                &name,
                state
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )
        }
        PublicationPresence::Canonical => {
            let database = match state.status {
                MigrationStatus::Complete => BrainDb::open_current(
                    paths,
                    OpenRole::NonHook,
                    StorageDeadline::after(Duration::from_secs(2)),
                )?,
                MigrationStatus::LegacyFrozen => {
                    BrainDb::open_published_for_completion(paths, state.generation)?
                }
                MigrationStatus::Building
                | MigrationStatus::Verified
                | MigrationStatus::BrainPublishedIncomplete => {
                    validate_published(paths, state.generation)?;
                    BrainDb::open_published_incomplete(paths)?
                }
            };
            validate_brain_artifact(
                &database,
                state
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )?;
            if matches!(state.freeze_state, FreezeState::Pending) {
                validate_database_accounting(paths, &database, accounting)
            } else {
                validate_database_accounting_without_replay(&database, accounting)
            }
        }
        PublicationPresence::Neither => Err(StorageError::InvalidStorage(
            "migration accounting has no database",
        )),
    }
}

fn validate_source_fingerprints(
    paths: &StoragePaths,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    let sources = LegacySourceSet::at(&paths.state_root)?;
    let current = brain_source_fingerprint_state(&sources)?;
    if state.fingerprint_digest != current.digest
        || state.fingerprint_count != current.count
        || state.descriptors != current.descriptors
    {
        return Err(StorageError::InvalidStorage(
            "legacy sources changed after migration began",
        ));
    }
    Ok(())
}

fn validate_coordinator_metadata(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    validate_managed_entries(directory, state)?;
    validate_owned_manifest_artifacts(paths, directory, state)
}

fn validate_owned_manifest_artifacts(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    let temporary = CString::new(manifest_temp_name(state.generation))
        .map_err(|_| StorageError::InvalidStorage("manifest temporary name is invalid"))?;
    let presence = directory.publication_presence(&temporary, FROZEN_MANIFEST_NAME)?;
    if matches!(
        presence,
        PublicationPresence::Staging | PublicationPresence::LinkedPair
    ) {
        let manifest = load_frozen_manifest_named(&paths.state_root, &temporary)?;
        validate_frozen_manifest_value_for_state(manifest, state)?;
    }
    if matches!(
        presence,
        PublicationPresence::LinkedPair | PublicationPresence::Canonical
    ) {
        validate_frozen_manifest_for_state(paths, state)?;
    }
    Ok(())
}

fn validate_managed_entries(
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<(), StorageError> {
    let presence = publication_presence(directory, state)?;
    match state.status {
        MigrationStatus::Building
            if !matches!(
                presence,
                PublicationPresence::Neither | PublicationPresence::Staging
            ) =>
        {
            return Err(StorageError::InvalidStorage(
                "building migration has a canonical database",
            ));
        }
        MigrationStatus::Verified if presence == PublicationPresence::Neither => {
            return Err(StorageError::InvalidStorage(
                "verified migration has no database",
            ));
        }
        MigrationStatus::BrainPublishedIncomplete if presence != PublicationPresence::Canonical => {
            return Err(StorageError::InvalidStorage(
                "published migration database set is inconsistent",
            ));
        }
        MigrationStatus::LegacyFrozen | MigrationStatus::Complete
            if presence != PublicationPresence::Canonical =>
        {
            return Err(StorageError::InvalidStorage(
                "completed migration database set is inconsistent",
            ));
        }
        _ => {}
    }
    for entry in fs::read_dir(directory.path())? {
        let name = entry?.file_name();
        let bytes = name.as_bytes();
        let brain_namespace = bytes.starts_with(b".brain.sqlite3.migrate-")
            || bytes.starts_with(b".brain.sqlite3.migration-");
        let review_result_namespace = bytes.starts_with(b".brain.sqlite3.review-result-");
        let review_staging_namespace = bytes.starts_with(b".review.sqlite3.migration-");
        let freeze_namespace = bytes.starts_with(b".brain.sqlite3.freeze-")
            || bytes.starts_with(b".brain.sqlite3.frozen-manifest");
        let managed = brain_namespace
            || review_result_namespace
            || review_staging_namespace
            || freeze_namespace;
        if !managed || bytes == MIGRATION_STATE_NAME.to_bytes() {
            continue;
        }
        let staging_bytes = state.staging_name.as_bytes();
        let allowed_staging = bytes == staging_bytes;
        let allowed_building_sidecar = state.status == MigrationStatus::Building
            && [
                b"-wal".as_slice(),
                b"-shm".as_slice(),
                b"-journal".as_slice(),
            ]
            .iter()
            .any(|suffix| bytes == [staging_bytes, suffix].concat());
        let allowed_state_temporary = state
            .allowed_temporary
            .as_deref()
            .is_some_and(|temporary| bytes == temporary.as_bytes());
        let review_staging = review_staging_name(state.generation)?;
        let review_staging_bytes = review_staging.to_bytes();
        let review_owned = !matches!(&state.review_result, ReviewMigrationResult::Pending);
        let allowed_review_staging = review_owned && bytes == review_staging_bytes;
        let allowed_review_building_sidecar =
            matches!(&state.review_result, ReviewMigrationResult::Building { .. })
                && [
                    b"-wal".as_slice(),
                    b"-shm".as_slice(),
                    b"-journal".as_slice(),
                ]
                .iter()
                .any(|suffix| bytes == [review_staging_bytes, suffix].concat());
        let progress_name = freeze_progress_name(state.generation);
        let allowed_progress = !matches!(state.freeze_state, FreezeState::Pending)
            && bytes == progress_name.as_bytes();
        let allowed_manifest = matches!(
            state.freeze_state,
            FreezeState::ManifestVerified { .. } | FreezeState::ManifestPublished { .. }
        ) && bytes == FROZEN_MANIFEST_NAME.to_bytes();
        let allowed_manifest_temporary = matches!(
            state.freeze_state,
            FreezeState::ManifestBuilding { .. } | FreezeState::ManifestVerified { .. }
        ) && bytes
            == manifest_temp_name(state.generation).as_bytes();
        if !allowed_staging
            && !allowed_building_sidecar
            && !allowed_state_temporary
            && !allowed_review_staging
            && !allowed_review_building_sidecar
            && !allowed_progress
            && !allowed_manifest
            && !allowed_manifest_temporary
        {
            return Err(StorageError::InvalidStorage(
                "ambiguous migration staging entry",
            ));
        }
    }
    let manifest_temporary = CString::new(manifest_temp_name(state.generation))
        .map_err(|_| StorageError::InvalidStorage("manifest temporary name is invalid"))?;
    let manifest_presence =
        directory.publication_presence(&manifest_temporary, FROZEN_MANIFEST_NAME)?;
    let valid_manifest_presence = match state.freeze_state {
        FreezeState::ManifestBuilding { .. } => matches!(
            manifest_presence,
            PublicationPresence::Neither | PublicationPresence::Staging
        ),
        FreezeState::ManifestVerified { .. } => {
            !matches!(manifest_presence, PublicationPresence::Neither)
        }
        FreezeState::ManifestPublished { .. } => {
            manifest_presence == PublicationPresence::Canonical
        }
        _ => manifest_presence == PublicationPresence::Neither,
    };
    if !valid_manifest_presence {
        return Err(StorageError::InvalidStorage(
            "frozen manifest publication set is inconsistent",
        ));
    }
    if state.status == MigrationStatus::Verified && presence == PublicationPresence::Staging {
        directory.validate_database_without_sidecars(&state.staging_cstring()?)?;
    }
    if matches!(
        state.status,
        MigrationStatus::BrainPublishedIncomplete
            | MigrationStatus::LegacyFrozen
            | MigrationStatus::Complete
    ) || (state.status == MigrationStatus::Verified
        && presence == PublicationPresence::Canonical)
    {
        directory.validate_database_without_sidecars(BRAIN_DATABASE_NAME)?;
    }
    Ok(())
}

fn reject_unmanaged_migration_entries(
    directory: &SecureDatabaseDirectory,
) -> Result<(), StorageError> {
    if fs::read_dir(directory.path())?.any(|entry| {
        entry.is_ok_and(|entry| {
            let name = entry.file_name();
            name.as_bytes().starts_with(b".brain.sqlite3.migrate-")
                || name.as_bytes().starts_with(b".brain.sqlite3.migration-")
                || name
                    .as_bytes()
                    .starts_with(b".brain.sqlite3.review-result-")
                || name.as_bytes().starts_with(b".review.sqlite3.migration-")
                || name.as_bytes().starts_with(b".brain.sqlite3.freeze-")
                || name
                    .as_bytes()
                    .starts_with(b".brain.sqlite3.frozen-manifest")
        })
    }) {
        return Err(StorageError::InvalidStorage(
            "unowned migration staging entry",
        ));
    }
    Ok(())
}

struct SourceFingerprintState {
    digest: String,
    count: u64,
    descriptors: Vec<PersistedFingerprint>,
}

fn validate_building_source_advance(
    previous: &ValidatedState,
    current: &SourceFingerprintState,
) -> Result<(), StorageError> {
    let append_only = current.count >= previous.fingerprint_count
        && previous
            .descriptors
            .iter()
            .zip(&current.descriptors)
            .all(|(before, after)| {
                if !before.present {
                    return true;
                }
                if !after.present || before.device != after.device {
                    return false;
                }
                match before.kind.as_str() {
                    "decisions" | "activity" => {
                        before.inode == after.inode && before.size <= after.size
                    }
                    "lifecycle" => before.size <= after.size,
                    "permission_transactions" => before.inode == after.inode,
                    _ => false,
                }
            });
    if !append_only {
        return Err(StorageError::InvalidStorage(
            "legacy source change is not an append-only Building advance",
        ));
    }
    Ok(())
}

fn brain_source_fingerprint_state(
    sources: &LegacySourceSet,
) -> Result<SourceFingerprintState, StorageError> {
    let descriptors = sources
        .fingerprints()?
        .into_iter()
        .filter(|fingerprint| fingerprint.kind != LegacySourceKind::ReviewState)
        .map(persisted_fingerprint)
        .collect::<Vec<_>>();
    let mut unordered = [0u8; 32];
    let mut count = 0u64;
    sources.stream_fingerprints(&mut |fingerprint| {
        if fingerprint.kind == LegacySourceKind::ReviewState {
            return Ok(());
        }
        let encoded = encode_fingerprint(&fingerprint);
        let digest = Sha256::digest(&encoded);
        for (slot, byte) in unordered.iter_mut().zip(digest) {
            *slot ^= byte;
        }
        count = count.checked_add(1).ok_or(StorageError::InvalidStorage(
            "legacy fingerprint count overflow",
        ))?;
        Ok(())
    })?;
    let mut canonical = Sha256::new();
    canonical.update(b"coding-brain-migration-fingerprints-v1");
    canonical.update(count.to_be_bytes());
    canonical.update(unordered);
    for descriptor in &descriptors {
        canonical.update(
            serde_json::to_vec(descriptor).map_err(|_| {
                StorageError::InvalidStorage("legacy descriptor serialization failed")
            })?,
        );
    }
    Ok(SourceFingerprintState {
        digest: format!("{:x}", canonical.finalize()),
        count,
        descriptors,
    })
}

fn persisted_fingerprint(fingerprint: LegacyFingerprint) -> PersistedFingerprint {
    PersistedFingerprint {
        kind: source_kind_name(fingerprint.kind).to_owned(),
        relative_path: fingerprint.relative_path().to_string_lossy().into_owned(),
        present: fingerprint.present,
        device: fingerprint.device,
        inode: fingerprint.inode,
        size: fingerprint.size,
        modified_seconds: fingerprint.modified_seconds,
        modified_nanoseconds: fingerprint.modified_nanoseconds,
    }
}

fn encode_fingerprint(fingerprint: &LegacyFingerprint) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(128);
    for field in [
        source_kind_name(fingerprint.kind).as_bytes(),
        fingerprint.relative_path().as_os_str().as_bytes(),
        &[u8::from(fingerprint.present)],
        &fingerprint.device.to_be_bytes(),
        &fingerprint.inode.to_be_bytes(),
        &fingerprint.size.to_be_bytes(),
        &fingerprint.modified_seconds.to_be_bytes(),
        &fingerprint.modified_nanoseconds.to_be_bytes(),
    ] {
        encoded.extend_from_slice(&(field.len() as u64).to_be_bytes());
        encoded.extend_from_slice(field);
    }
    encoded
}

fn validate_published(paths: &StoragePaths, generation: u64) -> Result<(), StorageError> {
    let database = BrainDb::open_published_incomplete(paths)?;
    let actual = database.connection.query_row(
        "SELECT migration_generation FROM schema_meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if actual != generation as i64 {
        return Err(StorageError::InvalidStorage(
            "published migration generation changed",
        ));
    }
    drop(database);
    Ok(())
}

fn populate_database_accounting(
    database: &BrainDb,
    accounting: &mut MigrationAccounting,
) -> Result<(), StorageError> {
    accounting.table_counts = table_counts(database)?;
    accounting.activity = activity_accounting(database)?;
    accounting.lifecycle = lifecycle_accounting(database)?;
    accounting.historical = historical_counts(database)?;
    Ok(())
}

fn table_count(database: &BrainDb, table: &str) -> Result<u64, StorageError> {
    let count =
        database
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })?;
    u64::try_from(count)
        .map_err(|_| StorageError::InvalidStorage("migration table count is invalid"))
}

fn table_counts(database: &BrainDb) -> Result<TableCounts, StorageError> {
    Ok(TableCounts {
        permission_attempts: table_count(database, "permission_attempts")?,
        decision_identities: table_count(database, "decision_identities")?,
        decision_payloads: table_count(database, "decision_payloads")?,
        activity_events: table_count(database, "activity_events")?,
        permission_commits: table_count(database, "permission_commits")?,
        historical_permission_authority: table_count(database, "historical_permission_authority")?,
        lifecycle_sessions: table_count(database, "lifecycle_sessions")?,
        lifecycle_leases: table_count(database, "lifecycle_leases")?,
        lifecycle_turns: table_count(database, "lifecycle_turns")?,
        lifecycle_subagents: table_count(database, "lifecycle_subagents")?,
        lifecycle_invocations: table_count(database, "lifecycle_invocations")?,
        lifecycle_invocation_steps: table_count(database, "lifecycle_invocation_steps")?,
    })
}

fn activity_accounting(database: &BrainDb) -> Result<ActivityAccounting, StorageError> {
    let (count, first, last, high_water) = database.connection.query_row(
        "SELECT count(*), coalesce(min(source_cursor), 0), coalesce(max(source_cursor), 0),
                (SELECT activity_high_water FROM schema_meta WHERE singleton = 1)
         FROM activity_events",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        },
    )?;
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-migration-activity-order-v1");
    let mut statement = database.connection.prepare(
        "SELECT source_cursor, activity_id, event_kind, event_state FROM activity_events
         ORDER BY source_cursor ASC",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let cursor = row.get::<_, i64>(0)?;
        let activity_id = row.get::<_, String>(1)?;
        let kind = row.get::<_, String>(2)?;
        let state = row.get::<_, String>(3)?;
        for bytes in [
            cursor.to_be_bytes().as_slice(),
            activity_id.as_bytes(),
            kind.as_bytes(),
            state.as_bytes(),
        ] {
            digest.update((bytes.len() as u64).to_be_bytes());
            digest.update(bytes);
        }
    }
    Ok(ActivityAccounting {
        count: migration_u64(count)?,
        high_water: migration_u64(high_water)?,
        first_cursor: migration_u64(first)?,
        last_cursor: migration_u64(last)?,
        order_digest: format!("{:x}", digest.finalize()),
    })
}

fn lifecycle_accounting(database: &BrainDb) -> Result<LifecycleAccounting, StorageError> {
    let next_sequence = database.connection.query_row(
        "SELECT next_sequence FROM lifecycle_meta WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(LifecycleAccounting {
        next_sequence: migration_u64(next_sequence)?,
        sessions: table_count(database, "lifecycle_sessions")?,
        leases: table_count(database, "lifecycle_leases")?,
        turns: table_count(database, "lifecycle_turns")?,
        subagents: table_count(database, "lifecycle_subagents")?,
        invocations: table_count(database, "lifecycle_invocations")?,
        invocation_steps: table_count(database, "lifecycle_invocation_steps")?,
    })
}

fn historical_counts(database: &BrainDb) -> Result<HistoricalCounts, StorageError> {
    let count = |provenance: &str| -> Result<u64, StorageError> {
        let value = database.connection.query_row(
            "SELECT count(*) FROM historical_permission_authority WHERE provenance_kind = ?1",
            [provenance],
            |row| row.get::<_, i64>(0),
        )?;
        migration_u64(value)
    };
    Ok(HistoricalCounts {
        proposal_terminal: count("proposal_terminal")?,
        journal_correlated: count("journal_correlated")?,
        lifecycle_correlated: count("lifecycle_correlated")?,
    })
}

fn migration_u64(value: i64) -> Result<u64, StorageError> {
    u64::try_from(value)
        .map_err(|_| StorageError::InvalidStorage("migration accounting count is invalid"))
}

fn validate_accounting(accounting: &MigrationAccounting) -> Result<(), StorageError> {
    let expected_decisions = accounting
        .imports
        .decisions
        .checked_add(accounting.skips.incomplete_proposals)
        .and_then(|count| count.checked_add(accounting.skips.unanchored_audits))
        .ok_or(StorageError::InvalidStorage("migration count overflow"))?;
    let reconciled = accounting
        .historical
        .proposal_terminal
        .checked_add(accounting.historical.journal_correlated)
        .and_then(|count| count.checked_add(accounting.historical.lifecycle_correlated))
        .ok_or(StorageError::InvalidStorage("migration count overflow"))?;
    if accounting.sources.decisions != expected_decisions
        || accounting.sources.activities != accounting.imports.activities
        || accounting.sources.lifecycle_snapshots != accounting.imports.lifecycle_snapshots
        || accounting.sources.journals
            != accounting
                .historical
                .journal_correlated
                .checked_add(accounting.skips.unmatched_journals)
                .ok_or(StorageError::InvalidStorage("migration count overflow"))?
        || accounting.imports.decisions != accounting.table_counts.decision_identities
        || accounting.imports.decisions != accounting.table_counts.decision_payloads
        || accounting.imports.activities != accounting.table_counts.activity_events
        || accounting.activity.count != accounting.table_counts.activity_events
        || accounting.activity.high_water != accounting.activity.last_cursor
        || accounting.activity.first_cursor != u64::from(accounting.activity.count != 0)
        || accounting.table_counts.permission_attempts != 0
        || accounting.table_counts.permission_commits != 0
        || accounting.table_counts.historical_permission_authority != reconciled
        || accounting.lifecycle.sessions != accounting.table_counts.lifecycle_sessions
        || accounting.lifecycle.leases != accounting.table_counts.lifecycle_leases
        || accounting.lifecycle.turns != accounting.table_counts.lifecycle_turns
        || accounting.lifecycle.subagents != accounting.table_counts.lifecycle_subagents
        || accounting.lifecycle.invocations != accounting.table_counts.lifecycle_invocations
        || accounting.lifecycle.invocation_steps
            != accounting.table_counts.lifecycle_invocation_steps
        || accounting.activity.order_digest.len() != 64
    {
        return Err(StorageError::InvalidStorage(
            "migration accounting is inconsistent",
        ));
    }
    Ok(())
}

fn validate_database_accounting(
    paths: &StoragePaths,
    database: &BrainDb,
    expected: &MigrationAccounting,
) -> Result<(), StorageError> {
    validate_database_accounting_without_replay(database, expected)?;
    validate_replayed_accounting(paths, database, expected)?;
    Ok(())
}

fn validate_database_accounting_without_replay(
    database: &BrainDb,
    expected: &MigrationAccounting,
) -> Result<(), StorageError> {
    let mut actual = expected.clone();
    populate_database_accounting(database, &mut actual)?;
    validate_accounting(&actual)?;
    if &actual != expected {
        return Err(StorageError::InvalidStorage(
            "migration database accounting changed",
        ));
    }
    Ok(())
}

fn brain_logical_digest(connection: &Connection) -> Result<String, StorageError> {
    const TABLES: &[&str] = &[
        "permission_attempts",
        "decision_identities",
        "decision_payloads",
        "activity_events",
        "permission_commits",
        "historical_permission_authority",
        "lifecycle_meta",
        "lifecycle_sessions",
        "lifecycle_leases",
        "lifecycle_turns",
        "lifecycle_subagents",
        "lifecycle_invocations",
        "lifecycle_invocation_steps",
    ];
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-migration-brain-logical-v1");
    digest_query_rows(
        connection,
        "schema_meta",
        "SELECT singleton, application_id, schema_version, schema_generation,
                migration_generation, erasure_state, erasure_generation, activity_high_water
         FROM schema_meta ORDER BY singleton",
        &mut digest,
    )?;
    for table in TABLES {
        let column_count = connection
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))?
            .column_count();
        let order = (1..=column_count)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(",");
        digest_query_rows(
            connection,
            table,
            &format!("SELECT * FROM {table} ORDER BY {order}"),
            &mut digest,
        )?;
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn digest_query_rows(
    connection: &Connection,
    table: &str,
    sql: &str,
    digest: &mut Sha256,
) -> Result<(), StorageError> {
    digest.update((table.len() as u64).to_be_bytes());
    digest.update(table.as_bytes());
    let mut statement = connection.prepare(sql)?;
    let column_count = statement.column_count();
    digest.update((column_count as u64).to_be_bytes());
    let mut rows = statement.query([])?;
    let mut row_count = 0_u64;
    while let Some(row) = rows.next()? {
        row_count = row_count
            .checked_add(1)
            .ok_or(StorageError::InvalidStorage(
                "Brain artifact row count overflow",
            ))?;
        digest.update(b"row");
        for index in 0..column_count {
            use rusqlite::types::ValueRef;
            match row.get_ref(index)? {
                ValueRef::Null => digest.update([0]),
                ValueRef::Integer(value) => {
                    digest.update([1]);
                    digest.update(value.to_be_bytes());
                }
                ValueRef::Real(value) => {
                    digest.update([2]);
                    digest.update(value.to_bits().to_be_bytes());
                }
                ValueRef::Text(value) => {
                    digest.update([3]);
                    digest.update((value.len() as u64).to_be_bytes());
                    digest.update(value);
                }
                ValueRef::Blob(value) => {
                    digest.update([4]);
                    digest.update((value.len() as u64).to_be_bytes());
                    digest.update(value);
                }
            }
        }
    }
    digest.update(row_count.to_be_bytes());
    Ok(())
}

fn validate_brain_artifact(
    database: &BrainDb,
    expected: &BrainArtifact,
) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(&database.database_path)?;
    if metadata.dev() != expected.device
        || metadata.ino() != expected.inode
        || brain_logical_digest(&database.connection)? != expected.content_digest
    {
        return Err(StorageError::InvalidStorage(
            "published Brain artifact changed",
        ));
    }
    Ok(())
}

fn validate_linked_brain_artifact(
    directory: &SecureDatabaseDirectory,
    name: &CStr,
    expected: &BrainArtifact,
) -> Result<(), StorageError> {
    let before = directory.closed_database_identity_with_links(name, 2)?;
    if before.device != expected.device || before.inode != expected.inode {
        return Err(StorageError::InvalidStorage(
            "published Brain artifact changed",
        ));
    }
    let path = directory.path().join(OsStr::from_bytes(name.to_bytes()));
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW
            | rusqlite::OpenFlags::SQLITE_OPEN_EXRESCODE,
    )?;
    if brain_logical_digest(&connection)? != expected.content_digest {
        return Err(StorageError::InvalidStorage(
            "published Brain artifact changed",
        ));
    }
    drop(connection);
    let after = directory.closed_database_identity_with_links(name, 2)?;
    if before != after {
        return Err(StorageError::InvalidStorage(
            "published Brain artifact changed during validation",
        ));
    }
    Ok(())
}

fn validate_replayed_accounting(
    paths: &StoragePaths,
    database: &BrainDb,
    expected: &MigrationAccounting,
) -> Result<(), StorageError> {
    let sources = LegacySourceSet::at(&paths.state_root)?;
    let mut replay = ReplayAccounting::new(database);
    for kind in [
        LegacySourceKind::Activity,
        LegacySourceKind::Decisions,
        LegacySourceKind::PermissionTransactions,
        LegacySourceKind::Lifecycle,
    ] {
        sources.stream_kind_into(kind, &mut replay)?;
    }
    replay.finish()?;
    if replay.sources != expected.sources
        || replay.imports != expected.imports
        || replay.skips != expected.skips
    {
        return Err(StorageError::InvalidStorage(
            "migration source accounting changed",
        ));
    }
    Ok(())
}

struct ReplayAccounting<'database> {
    database: &'database BrainDb,
    sources: SourceCounts,
    imports: ImportCounts,
    skips: SkipCounts,
    next_activity_cursor: u64,
    matched_journals: u64,
}

impl<'database> ReplayAccounting<'database> {
    fn new(database: &'database BrainDb) -> Self {
        Self {
            database,
            sources: SourceCounts::default(),
            imports: ImportCounts::default(),
            skips: SkipCounts::default(),
            next_activity_cursor: 1,
            matched_journals: 0,
        }
    }

    fn row_present(&self, sql: &str, value: &str) -> Result<bool, StorageError> {
        Ok(self
            .database
            .connection
            .query_row(sql, [value], |_| Ok(()))
            .optional()?
            .is_some())
    }

    fn finish(&self) -> Result<(), StorageError> {
        let journal_rows = self.database.connection.query_row(
            "SELECT count(*) FROM historical_permission_authority
             WHERE provenance_kind = 'journal_correlated'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if journal_rows != i64::try_from(self.matched_journals).unwrap_or(-1) {
            return Err(source_accounting_changed());
        }
        Ok(())
    }

    fn exact_hook_decision(&self, record: &HookDecisionRecord) -> Result<bool, StorageError> {
        let historical = self
            .database
            .connection
            .query_row(
                "SELECT terminal_source_cursor, decision_kind, authority_action,
                        terminal_event_kind, terminal_event_state, terminal_action,
                        response_eligible, delivery_state
                 FROM historical_permission_authority WHERE decision_id = ?1",
                [&record.decision_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some(historical) = historical else {
            return Ok(false);
        };
        let cursor = super::ActivityCursor::try_from(historical.0)?;
        let terminal = super::activity::validated_activity_at(&self.database.connection, cursor)?;
        let Some(session) = terminal.event.session.as_ref() else {
            return Err(source_accounting_changed());
        };
        let action = match terminal.event.state {
            ActivityState::Allowed if record.brain_action == "approve" => PermissionAction::Allow,
            ActivityState::Denied if record.brain_action == "deny" => PermissionAction::Deny,
            _ => return Err(source_accounting_changed()),
        };
        if terminal.event.decision_id.as_deref() != Some(record.decision_id.as_str())
            || session.provider != record.provider
            || session.session_id != record.session_id
            || session.turn_id.as_deref() != Some(record.turn_id.as_str())
        {
            return Err(source_accounting_changed());
        }
        let source = match record.brain_source.as_str() {
            "model" | "brain" => "model",
            "deterministic" => "deterministic_safety",
            "provider_policy" => "native_provider",
            _ => {
                return Err(StorageError::InvalidStorage(
                    "legacy proposal decision source is unsupported",
                ));
            }
        };
        let decided_at_ms =
            record
                .resolved_at
                .checked_mul(1000)
                .ok_or(StorageError::InvalidStorage(
                    "legacy proposal timestamp is out of range",
                ))?;
        let identity = DecisionIdentity::permission(
            record.decision_id.clone(),
            record.provider,
            record.session_id.clone(),
            record.turn_id.clone(),
            session.tool_use_id.clone(),
            action,
            source,
            decided_at_ms,
        );
        let payload = DecisionPayload::new(
            DecisionKind::Permission,
            cursor,
            hook_record_as_decision(record),
        );
        if self
            .database
            .decision_identity(&record.decision_id)?
            .as_ref()
            != Some(&identity)
            || self
                .database
                .migration_decision_payload(&record.decision_id)?
                .as_ref()
                != Some(&payload)
        {
            return Err(source_accounting_changed());
        }
        let action = match action {
            PermissionAction::Allow => "allow",
            PermissionAction::Deny => "deny",
        };
        let state = match terminal.event.state {
            ActivityState::Allowed => "allowed",
            ActivityState::Denied => "denied",
            _ => unreachable!(),
        };
        if historical.1 != "permission"
            || historical.2 != action
            || historical.3 != "decision"
            || historical.4 != state
            || historical.5 != action
            || historical.6 != 0
            || historical.7 != "unknown"
        {
            return Err(source_accounting_changed());
        }
        Ok(true)
    }

    fn exact_audit_decision(&self, record: &DecisionRecord) -> Result<bool, StorageError> {
        let Some(decision_id) = record.decision_id.as_deref() else {
            return Ok(false);
        };
        let mut statement = self.database.connection.prepare(
            "SELECT source_cursor, event_payload FROM activity_events
             WHERE event_kind = 'decision' AND event_state NOT IN ('outcome', 'correction')
             ORDER BY source_cursor ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut anchor = None;
        while let Some(row) = rows.next()? {
            let payload = row.get::<_, Vec<u8>>(1)?;
            let activity: ActivityEvent =
                serde_json::from_slice(&payload).map_err(|_| source_accounting_changed())?;
            if activity.decision_id.as_deref() == Some(decision_id) {
                if anchor.is_some() {
                    return Err(StorageError::InvalidStorage(
                        "legacy audit decision has ambiguous activity anchors",
                    ));
                }
                anchor = Some(super::ActivityCursor::try_from(row.get::<_, i64>(0)?)?);
            }
        }
        let Some(cursor) = anchor else {
            return Ok(false);
        };
        let decided_at_ms = record
            .resolved_at
            .or(record.suggested_at)
            .unwrap_or_default()
            .checked_mul(1000)
            .ok_or(StorageError::InvalidStorage(
                "legacy audit timestamp is out of range",
            ))?;
        let identity = DecisionIdentity::observation(decision_id, record.provider, decided_at_ms);
        let payload = DecisionPayload::new(DecisionKind::Observation, cursor, record.clone());
        if self.database.decision_identity(decision_id)?.as_ref() != Some(&identity)
            || self
                .database
                .migration_decision_payload(decision_id)?
                .as_ref()
                != Some(&payload)
        {
            return Err(source_accounting_changed());
        }
        Ok(true)
    }

    fn exact_lifecycle(&self, lifecycle: &LifecycleSnapshot) -> Result<(), StorageError> {
        let mut imported = lifecycle.clone();
        imported.remove_permission_state();
        if self.database.read_lifecycle()? != imported {
            return Err(source_accounting_changed());
        }

        let mut evidence = Vec::new();
        for (storage_key, state) in &lifecycle.sessions {
            let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
                StorageError::InvalidStorage("invalid legacy lifecycle session key"),
            )?;
            if !state.turn_open {
                continue;
            }
            for (request_key, authority) in &state.permission_authorities {
                let turn_id = match key.provider {
                    AgentProvider::Codex | AgentProvider::Claude => {
                        if state.permission_disposition(request_key)
                            != Some(PermissionDisposition::Decided)
                        {
                            continue;
                        }
                        let Some(turn_id) = state.current_turn.as_deref() else {
                            continue;
                        };
                        turn_id.to_owned()
                    }
                    AgentProvider::Antigravity => {
                        let Some(step) = state
                            .antigravity_permission_requests
                            .get(request_key)
                            .copied()
                        else {
                            continue;
                        };
                        if state.antigravity_permission_disposition(request_key, step)
                            != Some(PermissionDisposition::Decided)
                        {
                            continue;
                        }
                        format!("step-{step}")
                    }
                };
                evidence.push(LifecycleReplayEvidence {
                    provider: key.provider,
                    session_id: key.session_id.to_owned(),
                    turn_id,
                    action: authority.action,
                    provider_session_id: state.provider_session_id.clone(),
                    cwd: state.cwd.clone(),
                    transaction_id: authority.transaction_id.clone(),
                    request_key: request_key.clone(),
                });
            }
        }

        let mut expected = 0_i64;
        for authority in &evidence {
            let action = permission_action_name(authority.action);
            let mut statement = self.database.connection.prepare(
                "SELECT h.decision_id, h.terminal_source_cursor
                 FROM historical_permission_authority h
                 JOIN decision_identities i USING (decision_id)
                 WHERE h.provenance_kind IN ('proposal_terminal', 'lifecycle_correlated')
                   AND i.provider = ?1 AND i.session_id = ?2
                   AND i.turn_id = ?3 AND i.authority_action = ?4
                 ORDER BY h.decision_id",
            )?;
            let mut rows = statement.query(params![
                authority.provider.as_str(),
                authority.session_id,
                authority.turn_id,
                action
            ])?;
            let mut sole_candidate = None;
            while let Some(row) = rows.next()? {
                let decision_id = row.get::<_, String>(0)?;
                let cursor = super::ActivityCursor::try_from(row.get::<_, i64>(1)?)?;
                if !self.lifecycle_terminal_matches(authority, cursor)? {
                    continue;
                }
                if sole_candidate.is_some() {
                    sole_candidate = None;
                    break;
                }
                sole_candidate = Some(decision_id);
            }
            drop(rows);
            drop(statement);
            let Some(decision_id) = sole_candidate else {
                continue;
            };
            let mut matching_evidence = 0_u8;
            for sibling in &evidence {
                if self.lifecycle_evidence_matches_decision(sibling, &decision_id)? {
                    matching_evidence = matching_evidence.saturating_add(1);
                    if matching_evidence > 1 {
                        break;
                    }
                }
            }
            if matching_evidence != 1 {
                continue;
            }
            let exact = self.database.connection.query_row(
                "SELECT provenance_kind = 'lifecycle_correlated'
                        AND transaction_id = ?2 AND request_key = ?3
                 FROM historical_permission_authority WHERE decision_id = ?1",
                params![decision_id, authority.transaction_id, authority.request_key],
                |row| row.get::<_, bool>(0),
            )?;
            if !exact {
                return Err(source_accounting_changed());
            }
            expected = expected.checked_add(1).ok_or(StorageError::InvalidStorage(
                "legacy lifecycle evidence count overflow",
            ))?;
        }
        let actual = self.database.connection.query_row(
            "SELECT count(*) FROM historical_permission_authority
             WHERE provenance_kind = 'lifecycle_correlated'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if expected != actual {
            return Err(source_accounting_changed());
        }
        Ok(())
    }

    fn lifecycle_terminal_matches(
        &self,
        authority: &LifecycleReplayEvidence,
        cursor: super::ActivityCursor,
    ) -> Result<bool, StorageError> {
        let terminal = super::activity::validated_activity_at(&self.database.connection, cursor)?;
        let Some(session) = terminal.event.session.as_ref() else {
            return Ok(false);
        };
        Ok(session.provider == authority.provider
            && session.session_id == authority.session_id
            && session.turn_id.as_deref() == Some(authority.turn_id.as_str())
            && session.provider_session_id == authority.provider_session_id
            && session.cwd == authority.cwd
            && terminal.event.project.cwd == authority.cwd)
    }

    fn lifecycle_evidence_matches_decision(
        &self,
        authority: &LifecycleReplayEvidence,
        decision_id: &str,
    ) -> Result<bool, StorageError> {
        let cursor = self
            .database
            .connection
            .query_row(
                "SELECT h.terminal_source_cursor
                 FROM historical_permission_authority h
                 JOIN decision_identities i USING (decision_id)
                 WHERE h.decision_id = ?1
                   AND h.provenance_kind IN ('proposal_terminal', 'lifecycle_correlated')
                   AND i.provider = ?2 AND i.session_id = ?3
                   AND i.turn_id = ?4 AND i.authority_action = ?5",
                params![
                    decision_id,
                    authority.provider.as_str(),
                    authority.session_id,
                    authority.turn_id,
                    permission_action_name(authority.action)
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        match cursor {
            Some(cursor) => {
                self.lifecycle_terminal_matches(authority, super::ActivityCursor::try_from(cursor)?)
            }
            None => Ok(false),
        }
    }
}

struct LifecycleReplayEvidence {
    provider: AgentProvider,
    session_id: String,
    turn_id: String,
    action: PermissionAction,
    provider_session_id: Option<String>,
    cwd: std::path::PathBuf,
    transaction_id: String,
    request_key: String,
}

fn permission_action_name(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Allow => "allow",
        PermissionAction::Deny => "deny",
    }
}

fn source_accounting_changed() -> StorageError {
    StorageError::InvalidStorage("migration source accounting changed")
}

impl LegacyImportSink for ReplayAccounting<'_> {
    fn decision(&mut self, decision: LegacyDecision) -> Result<(), StorageError> {
        checked_increment(&mut self.sources.decisions)?;
        let imported = match &decision {
            LegacyDecision::Hook(record) => self.exact_hook_decision(record)?,
            LegacyDecision::Audit(record) => self.exact_audit_decision(record)?,
        };
        if imported {
            checked_increment(&mut self.imports.decisions)
        } else {
            match &decision {
                LegacyDecision::Hook(_) => checked_increment(&mut self.skips.incomplete_proposals),
                LegacyDecision::Audit(_) => checked_increment(&mut self.skips.unanchored_audits),
            }
        }
    }

    fn activity(&mut self, activity: ActivityEvent) -> Result<(), StorageError> {
        checked_increment(&mut self.sources.activities)?;
        let cursor = super::ActivityCursor::try_from(self.next_activity_cursor)?;
        let stored = super::activity::validated_activity_at(&self.database.connection, cursor)?;
        if stored.cursor != cursor || stored.event != activity {
            return Err(source_accounting_changed());
        }
        self.next_activity_cursor =
            self.next_activity_cursor
                .checked_add(1)
                .ok_or(StorageError::InvalidStorage(
                    "migration activity cursor overflow",
                ))?;
        checked_increment(&mut self.imports.activities)
    }

    fn lifecycle(&mut self, lifecycle: LifecycleSnapshot) -> Result<(), StorageError> {
        checked_increment(&mut self.sources.lifecycle_snapshots)?;
        self.exact_lifecycle(&lifecycle)?;
        checked_increment(&mut self.imports.lifecycle_snapshots)
    }

    fn journal(&mut self, journal: PermissionTransactionJournal) -> Result<(), StorageError> {
        checked_increment(&mut self.sources.journals)?;
        let historical = self
            .database
            .connection
            .query_row(
                "SELECT terminal_source_cursor, provenance_kind, transaction_id, request_key
             FROM historical_permission_authority WHERE decision_id = ?1",
                [&journal.proposal.decision_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((cursor, provenance, transaction_id, request_key)) = historical else {
            checked_increment(&mut self.skips.unmatched_journals)?;
            return Ok(());
        };
        if self
            .database
            .migration_decision_payload(&journal.proposal.decision_id)?
            .is_none()
        {
            checked_increment(&mut self.skips.unmatched_journals)?;
            return Ok(());
        }
        let terminal = super::activity::validated_activity_at(
            &self.database.connection,
            super::ActivityCursor::try_from(cursor)?,
        )?;
        let expected_record = hook_record_as_decision(&journal.proposal);
        let payload = self
            .database
            .migration_decision_payload(&journal.proposal.decision_id)?
            .ok_or_else(source_accounting_changed)?;
        if payload.record != expected_record
            || terminal.event != journal.terminal
            || provenance != "journal_correlated"
            || transaction_id.as_deref() != Some(journal.transaction_id.as_str())
            || request_key.as_deref() != Some(journal.request_key.as_str())
        {
            return Err(source_accounting_changed());
        }
        checked_increment(&mut self.matched_journals)
    }

    fn review(
        &mut self,
        _review: crate::brain::review_state::ReviewStateSnapshot,
    ) -> Result<(), StorageError> {
        Ok(())
    }
}

fn validate_transition_target(
    paths: &StoragePaths,
    directory: &SecureDatabaseDirectory,
    candidate: &ValidatedState,
) -> Result<(), StorageError> {
    let accounting = candidate
        .accounting
        .as_ref()
        .ok_or(StorageError::InvalidStorage(
            "pending transition has no migration accounting",
        ))?;
    match candidate.status {
        MigrationStatus::Verified => {
            if publication_presence(directory, candidate)? != PublicationPresence::Staging {
                return Err(StorageError::InvalidStorage(
                    "pending verified transition has no exclusive staging database",
                ));
            }
            let name = candidate.staging_cstring()?;
            directory.validate_database_without_sidecars(&name)?;
            let staging = BrainDb::open_staging_incomplete(paths, &name, candidate.generation)?;
            verify_staging(&staging)?;
            validate_brain_artifact(
                &staging,
                candidate
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )?;
            validate_database_accounting(paths, &staging, accounting)?;
        }
        MigrationStatus::BrainPublishedIncomplete => {
            match publication_presence(directory, candidate)? {
                PublicationPresence::Canonical => {
                    validate_published(paths, candidate.generation)?;
                    let published = BrainDb::open_published_incomplete(paths)?;
                    validate_brain_artifact(
                        &published,
                        candidate
                            .brain_artifact
                            .as_ref()
                            .ok_or(StorageError::InvalidStorage(
                                "migration Brain artifact is missing",
                            ))?,
                    )?;
                    validate_database_accounting(paths, &published, accounting)?;
                }
                PublicationPresence::Neither
                | PublicationPresence::Staging
                | PublicationPresence::LinkedPair => {
                    return Err(StorageError::InvalidStorage(
                        "pending published transition has no canonical database",
                    ));
                }
            }
        }
        MigrationStatus::LegacyFrozen => {
            validate_frozen_manifest_for_state(paths, candidate)?;
            let database = BrainDb::open_published_for_completion(paths, candidate.generation)?;
            validate_brain_artifact(
                &database,
                candidate
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )?;
        }
        MigrationStatus::Complete => {
            validate_frozen_manifest_for_state(paths, candidate)?;
            let database = BrainDb::open_current(
                paths,
                OpenRole::NonHook,
                StorageDeadline::after(Duration::from_secs(2)),
            )?;
            validate_brain_artifact(
                &database,
                candidate
                    .brain_artifact
                    .as_ref()
                    .ok_or(StorageError::InvalidStorage(
                        "migration Brain artifact is missing",
                    ))?,
            )?;
        }
        MigrationStatus::Building => {
            return Err(StorageError::InvalidStorage(
                "pending migration transition target is invalid",
            ));
        }
    }
    Ok(())
}

fn publication_presence(
    directory: &SecureDatabaseDirectory,
    state: &ValidatedState,
) -> Result<PublicationPresence, StorageError> {
    Ok(directory.publication_presence(&state.staging_cstring()?, BRAIN_DATABASE_NAME)?)
}

fn source_kind_name(kind: LegacySourceKind) -> &'static str {
    match kind {
        LegacySourceKind::Decisions => "decisions",
        LegacySourceKind::Activity => "activity",
        LegacySourceKind::Lifecycle => "lifecycle",
        LegacySourceKind::PermissionTransactions => "permission_transactions",
        LegacySourceKind::ReviewState => "review_state",
    }
}

fn status_name(status: MigrationStatus) -> &'static str {
    match status {
        MigrationStatus::Building => "building",
        MigrationStatus::Verified => "verified",
        MigrationStatus::BrainPublishedIncomplete => "brain_published_incomplete",
        MigrationStatus::LegacyFrozen => "legacy_frozen",
        MigrationStatus::Complete => "complete",
    }
}

fn parse_status(status: &str) -> Result<MigrationStatus, StorageError> {
    match status {
        "building" => Ok(MigrationStatus::Building),
        "verified" => Ok(MigrationStatus::Verified),
        "brain_published_incomplete" => Ok(MigrationStatus::BrainPublishedIncomplete),
        "legacy_frozen" => Ok(MigrationStatus::LegacyFrozen),
        "complete" => Ok(MigrationStatus::Complete),
        _ => Err(StorageError::InvalidStorage(
            "migration state status is invalid",
        )),
    }
}

fn state_temp_name(
    generation: u64,
    from: MigrationStatus,
    to: MigrationStatus,
) -> Result<CString, StorageError> {
    let transition = match (from, to) {
        (MigrationStatus::Building, MigrationStatus::Verified) => "building-to-verified",
        (MigrationStatus::Verified, MigrationStatus::BrainPublishedIncomplete) => {
            "verified-to-published"
        }
        (MigrationStatus::BrainPublishedIncomplete, MigrationStatus::LegacyFrozen) => {
            "published-to-frozen"
        }
        (MigrationStatus::LegacyFrozen, MigrationStatus::Complete) => "frozen-to-complete",
        _ => {
            return Err(StorageError::InvalidStorage(
                "migration state transition is invalid",
            ));
        }
    };
    CString::new(format!(
        ".brain.sqlite3.migration-state-{generation}-{transition}.tmp"
    ))
    .map_err(|_| StorageError::InvalidStorage("migration state temporary name is invalid"))
}

fn next_status(status: MigrationStatus) -> Option<MigrationStatus> {
    match status {
        MigrationStatus::Building => Some(MigrationStatus::Verified),
        MigrationStatus::Verified => Some(MigrationStatus::BrainPublishedIncomplete),
        MigrationStatus::BrainPublishedIncomplete => Some(MigrationStatus::LegacyFrozen),
        MigrationStatus::LegacyFrozen => Some(MigrationStatus::Complete),
        MigrationStatus::Complete => None,
    }
}

#[cfg(feature = "fault-injection")]
pub(super) fn migration_fault(stage: &str) {
    let Some(stage) = super::MigrationFaultStage::from_label(stage) else {
        return;
    };
    if let Ok(true) = super::fault_injection::hit_migration(stage) {
        std::process::abort();
    }
}

#[cfg(not(feature = "fault-injection"))]
pub(super) fn migration_fault(_stage: &str) {}

fn next_generation() -> Result<u64, StorageError> {
    let counter = MIGRATION_GENERATION.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let folded = (nanos ^ u128::from(counter)) & i64::MAX as u128;
    u64::try_from(folded.max(1))
        .map_err(|_| StorageError::InvalidStorage("migration generation is out of range"))
}

fn staging_name(creator_pid: u32, generation: u64) -> Result<CString, StorageError> {
    CString::new(format!(".brain.sqlite3.migrate-{creator_pid}-{generation}"))
        .map_err(|_| StorageError::InvalidStorage("migration staging name is invalid"))
}

fn prepare_import_tables(database: &BrainDb) -> Result<(), StorageError> {
    database.connection.execute_batch(
        "CREATE TEMP TABLE legacy_permission_terminals (
             decision_id TEXT PRIMARY KEY,
             terminal_cursor INTEGER NOT NULL
         ) STRICT;
         CREATE TEMP TABLE legacy_decision_ids (
             decision_id TEXT PRIMARY KEY
         ) STRICT;
         CREATE TEMP TABLE legacy_journal_ids (
             transaction_id TEXT PRIMARY KEY
         ) STRICT;
         CREATE TEMP TABLE legacy_permission_proposals (
             decision_id TEXT PRIMARY KEY,
             proposal BLOB NOT NULL
         ) STRICT;
         CREATE TEMP TABLE legacy_lifecycle_candidates (
             evidence_id INTEGER NOT NULL,
             decision_id TEXT NOT NULL,
             transaction_id TEXT NOT NULL,
             request_key TEXT NOT NULL,
             PRIMARY KEY (evidence_id, decision_id)
         ) STRICT;",
    )?;
    Ok(())
}

fn verify_staging(database: &BrainDb) -> Result<(), StorageError> {
    let integrity: String = database
        .connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(StorageError::InvalidStorage(
            "staging database integrity check failed",
        ));
    }
    let foreign_key_error = database
        .connection
        .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
        .optional()?;
    if foreign_key_error.is_some() {
        return Err(StorageError::InvalidStorage(
            "staging database foreign key check failed",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum ImportPhase {
    Activity,
    Decisions,
    Journals,
    Lifecycle,
}

struct MigrationImport<'database, 'accounting> {
    database: &'database mut BrainDb,
    phase: ImportPhase,
    accounting: &'accounting mut MigrationAccounting,
}

impl<'database, 'accounting> MigrationImport<'database, 'accounting> {
    fn new(
        database: &'database mut BrainDb,
        phase: ImportPhase,
        accounting: &'accounting mut MigrationAccounting,
    ) -> Self {
        Self {
            database,
            phase,
            accounting,
        }
    }

    fn import_hook_decision(&mut self, record: HookDecisionRecord) -> Result<(), StorageError> {
        let cursor = self
            .database
            .connection
            .query_row(
                "SELECT terminal_cursor FROM legacy_permission_terminals WHERE decision_id = ?1",
                [&record.decision_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let Some(cursor) = cursor else {
            checked_increment(&mut self.accounting.skips.incomplete_proposals)?;
            return Ok(());
        };
        let cursor = super::ActivityCursor::try_from(cursor)?;
        let terminal = super::activity::validated_activity_at(&self.database.connection, cursor)?;
        let Some(session) = terminal.event.session.as_ref() else {
            checked_increment(&mut self.accounting.skips.incomplete_proposals)?;
            return Ok(());
        };
        if terminal.event.decision_id.as_deref() != Some(record.decision_id.as_str())
            || session.provider != record.provider
            || session.session_id != record.session_id
            || session.turn_id.as_deref() != Some(record.turn_id.as_str())
        {
            checked_increment(&mut self.accounting.skips.incomplete_proposals)?;
            return Ok(());
        }
        let action = match terminal.event.state {
            ActivityState::Allowed if record.brain_action == "approve" => PermissionAction::Allow,
            ActivityState::Denied if record.brain_action == "deny" => PermissionAction::Deny,
            _ => {
                checked_increment(&mut self.accounting.skips.incomplete_proposals)?;
                return Ok(());
            }
        };
        let source = match record.brain_source.as_str() {
            "model" | "brain" => "model",
            "deterministic" => "deterministic_safety",
            "provider_policy" => "native_provider",
            _ => {
                return Err(StorageError::InvalidStorage(
                    "legacy proposal decision source is unsupported",
                ));
            }
        };
        let decided_at_ms =
            record
                .resolved_at
                .checked_mul(1000)
                .ok_or(StorageError::InvalidStorage(
                    "legacy proposal timestamp is out of range",
                ))?;
        let identity = DecisionIdentity::permission(
            record.decision_id.clone(),
            record.provider,
            record.session_id.clone(),
            record.turn_id.clone(),
            session.tool_use_id.clone(),
            action,
            source,
            decided_at_ms,
        );
        let payload = DecisionPayload::new(
            DecisionKind::Permission,
            cursor,
            hook_record_as_decision(&record),
        );
        self.database.import_decision(&identity, &payload)?;
        let action = match action {
            PermissionAction::Allow => "allow",
            PermissionAction::Deny => "deny",
        };
        let state = match terminal.event.state {
            ActivityState::Allowed => "allowed",
            ActivityState::Denied => "denied",
            _ => unreachable!(),
        };
        self.database.connection.execute(
            "INSERT INTO historical_permission_authority (
                 decision_id, terminal_source_cursor, decision_kind, authority_action,
                 terminal_event_kind, terminal_event_state, terminal_action,
                 provenance_kind, transaction_id, request_key,
                 response_eligible, delivery_state
             ) VALUES (?1, ?2, 'permission', ?3, 'decision', ?4, ?3,
                       'proposal_terminal', NULL, NULL, 0, 'unknown')",
            params![record.decision_id, cursor.get() as i64, action, state],
        )?;
        let encoded = serde_json::to_vec(&record)
            .map_err(|_| StorageError::InvalidStorage("legacy proposal serialization failed"))?;
        self.database.connection.execute(
            "INSERT INTO legacy_permission_proposals (decision_id, proposal) VALUES (?1, ?2)",
            params![record.decision_id, encoded],
        )?;
        checked_increment(&mut self.accounting.imports.decisions)?;
        Ok(())
    }

    fn correlate_lifecycle_authority(
        &mut self,
        lifecycle: &LifecycleSnapshot,
    ) -> Result<(), StorageError> {
        self.database
            .connection
            .execute("DELETE FROM legacy_lifecycle_candidates", [])?;
        let mut evidence_id = 0_i64;
        for (storage_key, state) in &lifecycle.sessions {
            let key = AgentSessionKey::from_storage_key(storage_key).ok_or(
                StorageError::InvalidStorage("invalid legacy lifecycle session key"),
            )?;
            if !state.turn_open {
                continue;
            }
            for (request_key, authority) in &state.permission_authorities {
                let turn_id = match key.provider {
                    AgentProvider::Codex | AgentProvider::Claude => {
                        if state.permission_disposition(request_key)
                            != Some(PermissionDisposition::Decided)
                        {
                            continue;
                        }
                        let Some(turn_id) = state.current_turn.as_deref() else {
                            continue;
                        };
                        turn_id.to_owned()
                    }
                    AgentProvider::Antigravity => {
                        let Some(step) = state
                            .antigravity_permission_requests
                            .get(request_key)
                            .copied()
                        else {
                            continue;
                        };
                        if state.antigravity_permission_disposition(request_key, step)
                            != Some(PermissionDisposition::Decided)
                        {
                            continue;
                        }
                        format!("step-{step}")
                    }
                };
                evidence_id = evidence_id
                    .checked_add(1)
                    .ok_or(StorageError::InvalidStorage(
                        "legacy lifecycle evidence count overflow",
                    ))?;
                let action = match authority.action {
                    PermissionAction::Allow => "allow",
                    PermissionAction::Deny => "deny",
                };
                let mut statement = self.database.connection.prepare(
                    "SELECT h.decision_id, h.terminal_source_cursor
                     FROM historical_permission_authority h
                     JOIN decision_identities i USING (decision_id)
                     WHERE h.provenance_kind = 'proposal_terminal'
                       AND i.provider = ?1 AND i.session_id = ?2
                       AND i.turn_id = ?3 AND i.authority_action = ?4
                     ORDER BY h.decision_id",
                )?;
                let mut rows = statement.query(params![
                    key.provider.as_str(),
                    key.session_id,
                    turn_id,
                    action
                ])?;
                while let Some(row) = rows.next()? {
                    let decision_id = row.get::<_, String>(0)?;
                    let cursor = super::ActivityCursor::try_from(row.get::<_, i64>(1)?)?;
                    let terminal =
                        super::activity::validated_activity_at(&self.database.connection, cursor)?;
                    let Some(session) = terminal.event.session.as_ref() else {
                        continue;
                    };
                    if session.provider != key.provider
                        || session.session_id != key.session_id
                        || session.turn_id.as_deref() != Some(turn_id.as_str())
                        || session.provider_session_id != state.provider_session_id
                        || session.cwd != state.cwd
                        || terminal.event.project.cwd != state.cwd
                    {
                        continue;
                    }
                    self.database.connection.execute(
                        "INSERT INTO legacy_lifecycle_candidates (
                             evidence_id, decision_id, transaction_id, request_key
                         ) VALUES (?1, ?2, ?3, ?4)",
                        params![
                            evidence_id,
                            decision_id,
                            authority.transaction_id,
                            request_key
                        ],
                    )?;
                }
            }
        }
        self.database.connection.execute(
            "UPDATE historical_permission_authority AS historical
             SET provenance_kind = 'lifecycle_correlated',
                 transaction_id = (
                     SELECT candidate.transaction_id
                     FROM legacy_lifecycle_candidates candidate
                     WHERE candidate.decision_id = historical.decision_id
                 ),
                 request_key = (
                     SELECT candidate.request_key
                     FROM legacy_lifecycle_candidates candidate
                     WHERE candidate.decision_id = historical.decision_id
                 )
             WHERE historical.provenance_kind = 'proposal_terminal'
               AND (SELECT count(*) FROM legacy_lifecycle_candidates candidate
                    WHERE candidate.decision_id = historical.decision_id) = 1
               AND (SELECT count(*) FROM legacy_lifecycle_candidates sibling
                    WHERE sibling.evidence_id = (
                        SELECT candidate.evidence_id
                        FROM legacy_lifecycle_candidates candidate
                        WHERE candidate.decision_id = historical.decision_id
                    )) = 1",
            [],
        )?;
        Ok(())
    }

    fn import_audit_decision(&mut self, record: DecisionRecord) -> Result<(), StorageError> {
        let Some(decision_id) = record.decision_id.as_deref() else {
            checked_increment(&mut self.accounting.skips.unanchored_audits)?;
            return Ok(());
        };
        let mut statement = self.database.connection.prepare(
            "SELECT source_cursor, event_payload FROM activity_events
             WHERE event_kind = 'decision' AND event_state NOT IN ('outcome', 'correction')
             ORDER BY source_cursor ASC",
        )?;
        let mut rows = statement.query([])?;
        let mut anchor = None;
        while let Some(row) = rows.next()? {
            let cursor = row.get::<_, i64>(0)?;
            let payload = row.get::<_, Vec<u8>>(1)?;
            let activity: ActivityEvent = serde_json::from_slice(&payload)
                .map_err(|_| StorageError::InvalidStorage("staged legacy activity is invalid"))?;
            if activity.decision_id.as_deref() != Some(decision_id) {
                continue;
            }
            if anchor.is_some() {
                return Err(StorageError::InvalidStorage(
                    "legacy audit decision has ambiguous activity anchors",
                ));
            }
            anchor = Some((cursor, activity));
        }
        drop(rows);
        drop(statement);
        let Some((cursor, activity)) = anchor else {
            checked_increment(&mut self.accounting.skips.unanchored_audits)?;
            return Ok(());
        };
        if activity
            .session
            .as_ref()
            .is_some_and(|session| session.provider != record.provider)
        {
            return Err(StorageError::InvalidStorage(
                "legacy audit decision and activity provider disagree",
            ));
        }
        let decided_at_ms = record
            .resolved_at
            .or(record.suggested_at)
            .unwrap_or_default()
            .checked_mul(1000)
            .ok_or(StorageError::InvalidStorage(
                "legacy audit timestamp is out of range",
            ))?;
        let identity = DecisionIdentity::observation(decision_id, record.provider, decided_at_ms);
        let payload = DecisionPayload::new(
            DecisionKind::Observation,
            super::ActivityCursor::try_from(cursor)?,
            record,
        );
        self.database.import_decision(&identity, &payload)?;
        checked_increment(&mut self.accounting.imports.decisions)
    }
}

impl LegacyImportSink for MigrationImport<'_, '_> {
    fn decision(&mut self, decision: LegacyDecision) -> Result<(), StorageError> {
        if !matches!(self.phase, ImportPhase::Decisions) {
            return Ok(());
        }
        checked_increment(&mut self.accounting.sources.decisions)?;
        if let Some(decision_id) = match &decision {
            LegacyDecision::Hook(record) => Some(record.decision_id.as_str()),
            LegacyDecision::Audit(record) => record.decision_id.as_deref(),
        } {
            self.database.connection.execute(
                "INSERT INTO legacy_decision_ids (decision_id) VALUES (?1)",
                [decision_id],
            )?;
        }
        match decision {
            LegacyDecision::Hook(record) => self.import_hook_decision(record),
            LegacyDecision::Audit(record) => self.import_audit_decision(record),
        }
    }

    fn activity(&mut self, activity: ActivityEvent) -> Result<(), StorageError> {
        if !matches!(self.phase, ImportPhase::Activity) {
            return Ok(());
        }
        checked_increment(&mut self.accounting.sources.activities)?;
        let decision_id = activity.decision_id.clone();
        let is_terminal = matches!(
            activity.state,
            ActivityState::Allowed | ActivityState::Denied
        );
        let cursor = self.database.append_activity(activity)?;
        checked_increment(&mut self.accounting.imports.activities)?;
        if is_terminal && let Some(decision_id) = decision_id {
            self.database.connection.execute(
                "INSERT INTO legacy_permission_terminals (decision_id, terminal_cursor)
                 VALUES (?1, ?2)",
                params![decision_id, cursor.get() as i64],
            )?;
        }
        Ok(())
    }

    fn lifecycle(&mut self, lifecycle: LifecycleSnapshot) -> Result<(), StorageError> {
        if matches!(self.phase, ImportPhase::Lifecycle) {
            checked_increment(&mut self.accounting.sources.lifecycle_snapshots)?;
            self.correlate_lifecycle_authority(&lifecycle)?;
            self.database.import_lifecycle_snapshot(lifecycle)?;
            checked_increment(&mut self.accounting.imports.lifecycle_snapshots)?;
        }
        Ok(())
    }

    fn journal(&mut self, journal: PermissionTransactionJournal) -> Result<(), StorageError> {
        if !matches!(self.phase, ImportPhase::Journals) {
            return Ok(());
        }
        checked_increment(&mut self.accounting.sources.journals)?;
        self.database.connection.execute(
            "INSERT INTO legacy_journal_ids (transaction_id) VALUES (?1)",
            [&journal.transaction_id],
        )?;
        let proposal = self
            .database
            .connection
            .query_row(
                "SELECT proposal FROM legacy_permission_proposals WHERE decision_id = ?1",
                [&journal.proposal.decision_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        let Some(proposal) = proposal else {
            checked_increment(&mut self.accounting.skips.unmatched_journals)?;
            return Ok(());
        };
        let proposal: HookDecisionRecord = serde_json::from_slice(&proposal)
            .map_err(|_| StorageError::InvalidStorage("staged legacy proposal is invalid"))?;
        let cursor = self.database.connection.query_row(
            "SELECT terminal_source_cursor FROM historical_permission_authority
             WHERE decision_id = ?1",
            [&journal.proposal.decision_id],
            |row| row.get::<_, i64>(0),
        )?;
        let terminal = super::activity::validated_activity_at(
            &self.database.connection,
            super::ActivityCursor::try_from(cursor)?,
        )?;
        if proposal != journal.proposal || terminal.event != journal.terminal {
            return Err(StorageError::InvalidStorage(
                "legacy journal disagrees with proposal or terminal evidence",
            ));
        }
        let updated = self.database.connection.execute(
            "UPDATE historical_permission_authority
             SET provenance_kind = 'journal_correlated', transaction_id = ?2, request_key = ?3
             WHERE decision_id = ?1 AND provenance_kind = 'proposal_terminal'",
            params![
                journal.proposal.decision_id,
                journal.transaction_id,
                journal.request_key
            ],
        )?;
        if updated != 1 {
            return Err(StorageError::InvalidStorage(
                "legacy journal historical authority is not unique",
            ));
        }
        Ok(())
    }

    fn review(
        &mut self,
        _review: crate::brain::review_state::ReviewStateSnapshot,
    ) -> Result<(), StorageError> {
        Ok(())
    }
}

fn checked_increment(value: &mut u64) -> Result<(), StorageError> {
    *value = value
        .checked_add(1)
        .ok_or(StorageError::InvalidStorage("migration count overflow"))?;
    if *value > i64::MAX as u64 {
        return Err(StorageError::InvalidStorage(
            "migration count exceeds SQLite integer range",
        ));
    }
    Ok(())
}

fn hook_record_as_decision(record: &HookDecisionRecord) -> DecisionRecord {
    DecisionRecord {
        provider: record.provider,
        timestamp: record.ts.clone(),
        pid: record.pid,
        project: record.project.clone(),
        tool: Some(record.tool.clone()),
        command: Some(record.command.clone()),
        brain_action: record.brain_action.clone(),
        brain_confidence: record.brain_confidence,
        brain_reasoning: record.brain_reasoning.clone(),
        user_action: record.user_action.clone(),
        context: None,
        outcome: None,
        decision_type: DecisionType::Session,
        suggested_at: Some(record.suggested_at),
        resolved_at: Some(record.resolved_at),
        override_reason: None,
        decision_id: Some(record.decision_id.clone()),
        brain_decision_ms: None,
        cache_hit: None,
        canonical: None,
    }
}
