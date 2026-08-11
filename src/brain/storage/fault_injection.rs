use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::{BrainDb, MigrationCoordinator, OpenRole, StorageDeadline, StorageError, StoragePaths};

const CAPABILITY_VERSION: u8 = 1;
const MAX_CAPABILITY_BYTES: u64 = 4 * 1024;
const MARKER_PREFIX: &[u8] = b"CBRAIN-FAULT-V1\0";

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FaultPoint {
    AdmissionWrite,
    InferenceExit,
    CommitBeforeCall,
    CommitAfterReturn,
    StdoutWrite,
    DeliveryWrite,
    Checkpoint,
    MigrationPublish,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FaultPosition {
    Before,
    After,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MigrationFaultStage {
    Building,
    AfterBuildingRebaseStateTempSync,
    Verified,
    AfterVerifiedStateTempSync,
    AfterBrainLink,
    AfterBrainPublication,
    AfterPublishedStateTempSync,
    ReviewBeforeCreate,
    ReviewBuilding,
    ReviewVerified,
    AfterReviewStagingSync,
    AfterReviewGateSync,
    AfterReviewLink,
    AfterReviewPublication,
    AfterCompleteReviewNormalizationReserving,
    AfterCompleteReviewNormalizationPreparing,
    AfterCompleteReviewNormalizationReady,
    AfterCompleteReviewNormalizationPublished,
    AfterCompleteReviewNormalizationCleanup,
    AfterCompleteReviewGateSync,
    AfterReviewResultStateTempSync,
    BeforeFreezeGuard,
    AfterFreezeBuildingStateSync,
    AfterFreezeProgressReadyStateSync,
    FreezePreparingSynced,
    FreezeTempSynced,
    FreezePreparedRecordSynced,
    FreezeEntryPublished,
    FreezeProgressSynced,
    AfterDirectoryFreezingStateSync,
    AfterJournalDirectoryChmod,
    AfterDirectoryFrozenStateSync,
    AfterManifestBuildingStateSync,
    AfterManifestTempSync,
    AfterManifestVerifiedStateSync,
    AfterManifestPublication,
    AfterManifestPublishedStateSync,
    AfterLegacyFrozen,
    AfterLegacyFrozenStateTempSync,
    AfterDatabaseComplete,
    AfterCompleteStateTempSync,
    AfterCompleteState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "selection", rename_all = "kebab-case")]
pub(crate) enum FaultSelection {
    Matrix(FaultPoint),
    MigrationRegression(MigrationFaultStage),
}

impl FaultPoint {
    fn as_str(self) -> &'static str {
        match self {
            Self::AdmissionWrite => "admission-write",
            Self::InferenceExit => "inference-exit",
            Self::CommitBeforeCall => "commit-before-call",
            Self::CommitAfterReturn => "commit-after-return",
            Self::StdoutWrite => "stdout-write",
            Self::DeliveryWrite => "delivery-write",
            Self::Checkpoint => "checkpoint",
            Self::MigrationPublish => "migration-publish",
        }
    }

    pub(crate) fn was_consumed(self) -> bool {
        CONTROLLER
            .get()
            .is_some_and(|controller| controller.was_consumed(self))
    }
}

impl FaultPosition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

impl MigrationFaultStage {
    pub(crate) fn from_label(label: &str) -> Option<Self> {
        Some(match label {
            "building" => Self::Building,
            "after-building-rebase-state-temp-sync" => Self::AfterBuildingRebaseStateTempSync,
            "verified" => Self::Verified,
            "after-verified-state-temp-sync" => Self::AfterVerifiedStateTempSync,
            "after-brain-link" => Self::AfterBrainLink,
            "after-brain-publication" => Self::AfterBrainPublication,
            "after-published-state-temp-sync" => Self::AfterPublishedStateTempSync,
            "review-before-create" => Self::ReviewBeforeCreate,
            "review-building" => Self::ReviewBuilding,
            "review-verified" => Self::ReviewVerified,
            "after-review-staging-sync" => Self::AfterReviewStagingSync,
            "after-review-gate-sync" => Self::AfterReviewGateSync,
            "after-review-link" => Self::AfterReviewLink,
            "after-review-publication" => Self::AfterReviewPublication,
            "after-complete-review-normalization-reserving" => {
                Self::AfterCompleteReviewNormalizationReserving
            }
            "after-complete-review-normalization-preparing" => {
                Self::AfterCompleteReviewNormalizationPreparing
            }
            "after-complete-review-normalization-ready" => {
                Self::AfterCompleteReviewNormalizationReady
            }
            "after-complete-review-normalization-published" => {
                Self::AfterCompleteReviewNormalizationPublished
            }
            "after-complete-review-normalization-cleanup" => {
                Self::AfterCompleteReviewNormalizationCleanup
            }
            "after-complete-review-gate-sync" => Self::AfterCompleteReviewGateSync,
            "after-review-result-state-temp-sync" => Self::AfterReviewResultStateTempSync,
            "before-freeze-guard" => Self::BeforeFreezeGuard,
            "after-freeze-building-state-sync" => Self::AfterFreezeBuildingStateSync,
            "after-freeze-progress-ready-state-sync" => Self::AfterFreezeProgressReadyStateSync,
            "freeze-preparing-synced" => Self::FreezePreparingSynced,
            "freeze-temp-synced" => Self::FreezeTempSynced,
            "freeze-prepared-record-synced" => Self::FreezePreparedRecordSynced,
            "freeze-entry-published" => Self::FreezeEntryPublished,
            "freeze-progress-synced" => Self::FreezeProgressSynced,
            "after-directory-freezing-state-sync" => Self::AfterDirectoryFreezingStateSync,
            "after-journal-directory-chmod" => Self::AfterJournalDirectoryChmod,
            "after-directory-frozen-state-sync" => Self::AfterDirectoryFrozenStateSync,
            "after-manifest-building-state-sync" => Self::AfterManifestBuildingStateSync,
            "after-manifest-temp-sync" => Self::AfterManifestTempSync,
            "after-manifest-verified-state-sync" => Self::AfterManifestVerifiedStateSync,
            "after-manifest-publication" => Self::AfterManifestPublication,
            "after-manifest-published-state-sync" => Self::AfterManifestPublishedStateSync,
            "after-legacy-frozen" => Self::AfterLegacyFrozen,
            "after-legacy-frozen-state-temp-sync" => Self::AfterLegacyFrozenStateTempSync,
            "after-database-complete" => Self::AfterDatabaseComplete,
            "after-complete-state-temp-sync" => Self::AfterCompleteStateTempSync,
            "after-complete-state" => Self::AfterCompleteState,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Building => "building",
            Self::AfterBuildingRebaseStateTempSync => "after-building-rebase-state-temp-sync",
            Self::Verified => "verified",
            Self::AfterVerifiedStateTempSync => "after-verified-state-temp-sync",
            Self::AfterBrainLink => "after-brain-link",
            Self::AfterBrainPublication => "after-brain-publication",
            Self::AfterPublishedStateTempSync => "after-published-state-temp-sync",
            Self::ReviewBeforeCreate => "review-before-create",
            Self::ReviewBuilding => "review-building",
            Self::ReviewVerified => "review-verified",
            Self::AfterReviewStagingSync => "after-review-staging-sync",
            Self::AfterReviewGateSync => "after-review-gate-sync",
            Self::AfterReviewLink => "after-review-link",
            Self::AfterReviewPublication => "after-review-publication",
            Self::AfterCompleteReviewNormalizationReserving => {
                "after-complete-review-normalization-reserving"
            }
            Self::AfterCompleteReviewNormalizationPreparing => {
                "after-complete-review-normalization-preparing"
            }
            Self::AfterCompleteReviewNormalizationReady => {
                "after-complete-review-normalization-ready"
            }
            Self::AfterCompleteReviewNormalizationPublished => {
                "after-complete-review-normalization-published"
            }
            Self::AfterCompleteReviewNormalizationCleanup => {
                "after-complete-review-normalization-cleanup"
            }
            Self::AfterCompleteReviewGateSync => "after-complete-review-gate-sync",
            Self::AfterReviewResultStateTempSync => "after-review-result-state-temp-sync",
            Self::BeforeFreezeGuard => "before-freeze-guard",
            Self::AfterFreezeBuildingStateSync => "after-freeze-building-state-sync",
            Self::AfterFreezeProgressReadyStateSync => "after-freeze-progress-ready-state-sync",
            Self::FreezePreparingSynced => "freeze-preparing-synced",
            Self::FreezeTempSynced => "freeze-temp-synced",
            Self::FreezePreparedRecordSynced => "freeze-prepared-record-synced",
            Self::FreezeEntryPublished => "freeze-entry-published",
            Self::FreezeProgressSynced => "freeze-progress-synced",
            Self::AfterDirectoryFreezingStateSync => "after-directory-freezing-state-sync",
            Self::AfterJournalDirectoryChmod => "after-journal-directory-chmod",
            Self::AfterDirectoryFrozenStateSync => "after-directory-frozen-state-sync",
            Self::AfterManifestBuildingStateSync => "after-manifest-building-state-sync",
            Self::AfterManifestTempSync => "after-manifest-temp-sync",
            Self::AfterManifestVerifiedStateSync => "after-manifest-verified-state-sync",
            Self::AfterManifestPublication => "after-manifest-publication",
            Self::AfterManifestPublishedStateSync => "after-manifest-published-state-sync",
            Self::AfterLegacyFrozen => "after-legacy-frozen",
            Self::AfterLegacyFrozenStateTempSync => "after-legacy-frozen-state-temp-sync",
            Self::AfterDatabaseComplete => "after-database-complete",
            Self::AfterCompleteStateTempSync => "after-complete-state-temp-sync",
            Self::AfterCompleteState => "after-complete-state",
        }
    }
}

impl FaultSelection {
    fn point_label(self) -> &'static str {
        match self {
            Self::Matrix(point) => point.as_str(),
            Self::MigrationRegression(_) => FaultPoint::MigrationPublish.as_str(),
        }
    }

    fn detail_label(self) -> Option<&'static str> {
        match self {
            Self::Matrix(_) => None,
            Self::MigrationRegression(stage) => Some(stage.as_str()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct Activation {
    pub(crate) capability: PathBuf,
    pub(crate) state_root: PathBuf,
    pub(crate) nonce: String,
    pub(crate) selection: FaultSelection,
    pub(crate) control_fd: RawFd,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CapabilityRecord {
    version: u8,
    state_root: PathBuf,
    nonce: String,
    selection: FaultSelection,
    control_device: u64,
    control_inode: u64,
}

struct Controller {
    selection: FaultSelection,
    fired: AtomicBool,
    marker_failed: AtomicBool,
    control: File,
}

impl Controller {
    fn new(activation: Activation) -> Result<Self, StorageError> {
        if activation.control_fd < 3 {
            return Err(StorageError::InvalidStorage(
                "fault control descriptor is reserved",
            ));
        }
        let status_flags = unsafe { libc::fcntl(activation.control_fd, libc::F_GETFL) };
        if status_flags < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let control = unsafe { File::from_raw_fd(activation.control_fd) };
        if status_flags & libc::O_ACCMODE != libc::O_WRONLY {
            return Err(StorageError::InvalidStorage(
                "fault control descriptor is not write-only",
            ));
        }

        let mut capability = super::security::open_fault_capability(&activation.capability)?;
        if capability.metadata()?.len() > MAX_CAPABILITY_BYTES {
            return Err(StorageError::InvalidStorage(
                "fault capability exceeds size limit",
            ));
        }
        let mut bytes = Vec::new();
        capability
            .by_ref()
            .take(MAX_CAPABILITY_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CAPABILITY_BYTES {
            return Err(StorageError::InvalidStorage(
                "fault capability exceeds size limit",
            ));
        }
        let record: CapabilityRecord = serde_json::from_slice(&bytes)
            .map_err(|error| StorageError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
        if record.version != CAPABILITY_VERSION {
            return Err(StorageError::InvalidStorage(
                "fault capability version is unsupported",
            ));
        }
        if record.state_root != activation.state_root {
            return Err(StorageError::InvalidStorage(
                "fault capability state root does not match",
            ));
        }
        if record.nonce != activation.nonce {
            return Err(StorageError::InvalidStorage(
                "fault capability nonce does not match",
            ));
        }
        if record.selection != activation.selection {
            return Err(StorageError::InvalidStorage(
                "fault capability selection does not match",
            ));
        }

        let metadata = control.metadata()?;
        if !metadata.file_type().is_fifo()
            || metadata.dev() != record.control_device
            || metadata.ino() != record.control_inode
        {
            return Err(StorageError::InvalidStorage(
                "fault control descriptor does not match capability FIFO",
            ));
        }
        let descriptor_flags = unsafe { libc::fcntl(control.as_raw_fd(), libc::F_GETFD) };
        if descriptor_flags < 0
            || unsafe {
                libc::fcntl(
                    control.as_raw_fd(),
                    libc::F_SETFD,
                    descriptor_flags | libc::FD_CLOEXEC,
                )
            } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            selection: activation.selection,
            fired: AtomicBool::new(false),
            marker_failed: AtomicBool::new(false),
            control,
        })
    }

    fn hit(&self, point: FaultPoint, position: FaultPosition) -> Result<bool, StorageError> {
        if self.selection != FaultSelection::Matrix(point) {
            return Ok(false);
        }
        self.fire(position)
    }

    fn was_consumed(&self, point: FaultPoint) -> bool {
        self.selection == FaultSelection::Matrix(point) && self.fired.load(Ordering::Acquire)
    }

    fn hit_migration(&self, stage: MigrationFaultStage) -> Result<bool, StorageError> {
        let matches = self.selection == FaultSelection::MigrationRegression(stage)
            || (self.selection == FaultSelection::Matrix(FaultPoint::MigrationPublish)
                && stage == MigrationFaultStage::AfterBrainPublication);
        if !matches {
            return Ok(false);
        }
        self.fire(FaultPosition::After)
    }

    fn fire(&self, position: FaultPosition) -> Result<bool, StorageError> {
        if self
            .fired
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(false);
        }
        let marker = marker(&self.selection, position);
        if marker.len() > 512 {
            self.marker_failed.store(true, Ordering::Release);
            return Err(StorageError::InvalidStorage(
                "fault marker exceeds atomic pipe frame limit",
            ));
        }
        let written = unsafe {
            libc::write(
                self.control.as_raw_fd(),
                marker.as_ptr().cast(),
                marker.len(),
            )
        };
        if written < 0 {
            self.marker_failed.store(true, Ordering::Release);
            return Err(io::Error::last_os_error().into());
        }
        if written as usize != marker.len() {
            self.marker_failed.store(true, Ordering::Release);
            return Err(StorageError::Io(io::Error::new(
                io::ErrorKind::WriteZero,
                "fault marker write was incomplete",
            )));
        }
        Ok(true)
    }
}

fn marker(selection: &FaultSelection, position: FaultPosition) -> Vec<u8> {
    let marker = format!(
        "CBRAIN-FAULT-V1\0{}\0{}\0{}\n",
        selection.point_label(),
        position.as_str(),
        selection.detail_label().unwrap_or("-"),
    )
    .into_bytes();
    debug_assert!(marker.starts_with(MARKER_PREFIX));
    marker
}

static CONTROLLER: OnceLock<Controller> = OnceLock::new();

pub(crate) fn activate(activation: Activation) -> Result<(), StorageError> {
    let controller = Controller::new(activation)?;
    CONTROLLER
        .set(controller)
        .map_err(|_| StorageError::InvalidStorage("fault controller is already initialized"))
}

pub(crate) fn hit(point: FaultPoint, position: FaultPosition) -> Result<bool, StorageError> {
    match CONTROLLER.get() {
        Some(controller) => controller.hit(point, position),
        None => Ok(false),
    }
}

pub(crate) fn hit_migration(stage: MigrationFaultStage) -> Result<bool, StorageError> {
    match CONTROLLER.get() {
        Some(controller) => controller.hit_migration(stage),
        None => Ok(false),
    }
}

pub(crate) fn run_worker(
    selection: &FaultSelection,
    state_root: &Path,
) -> Result<(), StorageError> {
    let paths = StoragePaths::at(state_root);
    let operation = match selection {
        FaultSelection::Matrix(FaultPoint::Checkpoint) => {
            let deadline = StorageDeadline::after(Duration::from_secs(2));
            BrainDb::open_current(&paths, OpenRole::NonHook, deadline).and_then(|mut database| {
                database.maintain_bounded(None, deadline).map(|_outcome| ())
            })
        }
        FaultSelection::Matrix(FaultPoint::MigrationPublish)
        | FaultSelection::MigrationRegression(_) => MigrationCoordinator::at(state_root)
            .run_non_hook()
            .map(|_status| ()),
        _ => {
            return Err(StorageError::InvalidStorage(
                "fault point requires permission-hook role",
            ));
        }
    };
    if CONTROLLER.get().is_some_and(|controller| {
        controller.selection == *selection && controller.marker_failed.load(Ordering::Acquire)
    }) {
        return Err(StorageError::InvalidStorage("fault marker emission failed"));
    }
    operation?;
    if CONTROLLER.get().is_some_and(|controller| {
        controller.selection == *selection && !controller.fired.load(Ordering::Acquire)
    }) {
        return Err(StorageError::InvalidStorage(
            "fault selection was not consumed",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File, OpenOptions};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;

    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CapabilityRecord {
        version: u8,
        state_root: PathBuf,
        nonce: String,
        selection: FaultSelection,
        control_device: u64,
        control_inode: u64,
    }

    const CAPABILITY_VERSION: u8 = 1;
    const MARKER_PREFIX: &[u8] = b"CBRAIN-FAULT-V1\0";

    struct Fixture {
        _temp: TempDir,
        capability: PathBuf,
        state_root: PathBuf,
        nonce: String,
        selection: FaultSelection,
        read: File,
        write: Option<File>,
    }

    impl Fixture {
        fn new(selection: FaultSelection) -> Self {
            let temp = tempfile::tempdir().unwrap();
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let state_root = temp.path().join("state");
            fs::create_dir(&state_root).unwrap();
            fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();
            let capability_dir = temp.path().join("capability");
            fs::create_dir(&capability_dir).unwrap();
            fs::set_permissions(&capability_dir, fs::Permissions::from_mode(0o700)).unwrap();
            let (read, write) = pipe();
            let metadata = write.metadata().unwrap();
            let capability = capability_dir.join("fault-capability.json");
            let nonce = "test-nonce".to_owned();
            write_capability(
                &capability,
                &CapabilityRecord {
                    version: CAPABILITY_VERSION,
                    state_root: state_root.clone(),
                    nonce: nonce.clone(),
                    selection,
                    control_device: metadata.dev(),
                    control_inode: metadata.ino(),
                },
            );
            Self {
                _temp: temp,
                capability,
                state_root,
                nonce,
                selection,
                read,
                write: Some(write),
            }
        }

        fn activation(&mut self) -> Activation {
            Activation {
                capability: self.capability.clone(),
                state_root: self.state_root.clone(),
                nonce: self.nonce.clone(),
                selection: self.selection,
                control_fd: self.write.take().unwrap().into_raw_fd(),
            }
        }

        fn activation_with_control_fd(&mut self, control_fd: i32) -> Activation {
            drop(self.write.take());
            Activation {
                capability: self.capability.clone(),
                state_root: self.state_root.clone(),
                nonce: self.nonce.clone(),
                selection: self.selection,
                control_fd,
            }
        }
    }

    fn pipe() -> (File, File) {
        let mut descriptors = [-1; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        unsafe {
            (
                File::from_raw_fd(descriptors[0]),
                File::from_raw_fd(descriptors[1]),
            )
        }
    }

    fn write_capability(path: &Path, record: &CapabilityRecord) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        serde_json::to_writer(options.open(path).unwrap(), record).unwrap();
    }

    fn replace_capability(fixture: &Fixture, update: impl FnOnce(&mut CapabilityRecord)) {
        let bytes = fs::read(&fixture.capability).unwrap();
        let mut record: CapabilityRecord = serde_json::from_slice(&bytes).unwrap();
        update(&mut record);
        fs::remove_file(&fixture.capability).unwrap();
        write_capability(&fixture.capability, &record);
    }

    #[test]
    fn valid_capability_fires_one_bounded_marker_once() {
        let selection = FaultSelection::Matrix(FaultPoint::CommitBeforeCall);
        let mut fixture = Fixture::new(selection);
        let controller = Controller::new(fixture.activation()).unwrap();
        assert!(
            controller
                .hit(FaultPoint::CommitBeforeCall, FaultPosition::Before)
                .unwrap()
        );
        assert!(
            !controller
                .hit(FaultPoint::CommitBeforeCall, FaultPosition::Before)
                .unwrap()
        );
        drop(controller);
        let mut marker = Vec::new();
        fixture.read.read_to_end(&mut marker).unwrap();
        assert!(marker.starts_with(MARKER_PREFIX));
        assert!(marker.len() <= 512);
        assert_eq!(marker, b"CBRAIN-FAULT-V1\0commit-before-call\0before\0-\n");
    }

    #[test]
    fn wrong_point_does_not_consume_selection() {
        let mut fixture = Fixture::new(FaultSelection::Matrix(FaultPoint::DeliveryWrite));
        let controller = Controller::new(fixture.activation()).unwrap();
        assert!(
            !controller
                .hit(FaultPoint::AdmissionWrite, FaultPosition::Before)
                .unwrap()
        );
        assert!(!controller.was_consumed(FaultPoint::AdmissionWrite));
        assert!(!controller.was_consumed(FaultPoint::DeliveryWrite));
        assert!(
            controller
                .hit(FaultPoint::DeliveryWrite, FaultPosition::After)
                .unwrap()
        );
        assert!(controller.was_consumed(FaultPoint::DeliveryWrite));
        assert!(!controller.was_consumed(FaultPoint::AdmissionWrite));
    }

    #[test]
    fn rejects_symlink_hard_link_and_public_mode_capabilities() {
        let mut symlink = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let real = symlink.capability.with_extension("real");
        fs::rename(&symlink.capability, &real).unwrap();
        std::os::unix::fs::symlink(&real, &symlink.capability).unwrap();
        assert!(Controller::new(symlink.activation()).is_err());

        let mut hard_link = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::hard_link(
            &hard_link.capability,
            hard_link.capability.with_extension("link"),
        )
        .unwrap();
        assert!(Controller::new(hard_link.activation()).is_err());

        let mut public = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::set_permissions(&public.capability, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Controller::new(public.activation()).is_err());
    }

    #[test]
    fn rejects_non_regular_capability_and_unsafe_ancestor() {
        let mut wrong_type = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::remove_file(&wrong_type.capability).unwrap();
        fs::create_dir(&wrong_type.capability).unwrap();
        fs::set_permissions(&wrong_type.capability, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(Controller::new(wrong_type.activation()).is_err());

        let mut unsafe_ancestor = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::set_permissions(
            unsafe_ancestor._temp.path(),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        assert!(Controller::new(unsafe_ancestor.activation()).is_err());
    }

    #[test]
    fn rejects_wrong_owner_where_supported() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let mut fixture = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let name =
            std::ffi::CString::new(fixture.capability.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(name.as_ptr(), 1, u32::MAX) }, 0);
        assert!(Controller::new(fixture.activation()).is_err());
    }

    #[test]
    fn rejects_wrong_nonce_selection_state_root_and_version() {
        for mismatch in 0..4 {
            let mut fixture = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
            match mismatch {
                0 => fixture.nonce = "wrong".to_owned(),
                1 => fixture.selection = FaultSelection::Matrix(FaultPoint::DeliveryWrite),
                2 => fixture.state_root = fixture._temp.path().join("other-state"),
                3 => replace_capability(&fixture, |record| record.version += 1),
                _ => unreachable!(),
            }
            assert!(Controller::new(fixture.activation()).is_err());
        }
    }

    #[test]
    fn rejects_malformed_and_oversized_capabilities() {
        let mut malformed = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::write(&malformed.capability, b"not-json").unwrap();
        assert!(Controller::new(malformed.activation()).is_err());

        let mut oversized = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        fs::write(
            &oversized.capability,
            vec![b'x'; MAX_CAPABILITY_BYTES as usize + 1],
        )
        .unwrap();
        assert!(Controller::new(oversized.activation()).is_err());
    }

    #[test]
    fn rejects_invalid_control_descriptors() {
        let mut low = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let activation = low.activation_with_control_fd(2);
        assert!(Controller::new(activation).is_err());

        let mut readonly = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let (read, _write) = pipe();
        let activation = readonly.activation_with_control_fd(read.into_raw_fd());
        assert!(Controller::new(activation).is_err());

        let mut regular = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let file = OpenOptions::new()
            .write(true)
            .open(&regular.capability)
            .unwrap();
        let activation = regular.activation_with_control_fd(file.into_raw_fd());
        assert!(Controller::new(activation).is_err());

        let mut different = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let (_read, write) = pipe();
        let activation = different.activation_with_control_fd(write.into_raw_fd());
        assert!(Controller::new(activation).is_err());
    }

    #[test]
    fn activation_restores_close_on_exec() {
        let mut fixture = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        let control_fd = fixture.write.as_ref().unwrap().as_raw_fd();
        let _controller = Controller::new(fixture.activation()).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "brain::storage::fault_injection::tests::exec_child_sees_control_fd_closed",
            ])
            .env("CBRAIN_TEST_CONTROL_FD", control_fd.to_string())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(CONTROLLER.get().is_none());
    }

    #[test]
    fn exec_child_sees_control_fd_closed() {
        let Ok(value) = std::env::var("CBRAIN_TEST_CONTROL_FD") else {
            return;
        };
        let descriptor: i32 = value.parse().unwrap();
        assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[test]
    fn global_activation_rejects_duplicate_initialization() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "brain::storage::fault_injection::tests::global_activation_rejects_duplicate_initialization_process_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("CBRAIN_TEST_GLOBAL_ACTIVATION", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    #[ignore]
    fn global_activation_rejects_duplicate_initialization_process_helper() {
        if std::env::var_os("CBRAIN_TEST_GLOBAL_ACTIVATION").is_none() {
            return;
        }
        let mut first = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        activate(first.activation()).unwrap();
        let mut second = Fixture::new(FaultSelection::Matrix(FaultPoint::AdmissionWrite));
        assert!(activate(second.activation()).is_err());
    }

    #[test]
    fn migration_regression_selection_emits_closed_stage_detail_once() {
        let stage = MigrationFaultStage::AfterBrainPublication;
        let mut fixture = Fixture::new(FaultSelection::MigrationRegression(stage));
        let controller = Controller::new(fixture.activation()).unwrap();
        assert!(controller.hit_migration(stage).unwrap());
        assert!(!controller.hit_migration(stage).unwrap());
        drop(controller);
        let mut marker = Vec::new();
        fixture.read.read_to_end(&mut marker).unwrap();
        assert_eq!(
            marker,
            b"CBRAIN-FAULT-V1\0migration-publish\0after\0after-brain-publication\n"
        );
    }

    #[test]
    fn marker_write_failure_is_recorded_without_firing_an_abort() {
        let stage = MigrationFaultStage::AfterBrainPublication;
        let mut fixture = Fixture::new(FaultSelection::MigrationRegression(stage));
        let controller = Controller::new(fixture.activation()).unwrap();
        let descriptor = controller.control.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let bytes = [b'x'; 4096];
        loop {
            let written = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
            if written < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }

        assert!(controller.hit_migration(stage).is_err());
        assert!(controller.marker_failed.load(Ordering::Acquire));
    }

    #[test]
    fn migration_labels_are_closed_and_matrix_matches_only_publication() {
        for stage in [
            MigrationFaultStage::Building,
            MigrationFaultStage::AfterBuildingRebaseStateTempSync,
            MigrationFaultStage::Verified,
            MigrationFaultStage::AfterVerifiedStateTempSync,
            MigrationFaultStage::AfterBrainLink,
            MigrationFaultStage::AfterBrainPublication,
            MigrationFaultStage::AfterPublishedStateTempSync,
            MigrationFaultStage::ReviewBeforeCreate,
            MigrationFaultStage::ReviewBuilding,
            MigrationFaultStage::ReviewVerified,
            MigrationFaultStage::AfterReviewStagingSync,
            MigrationFaultStage::AfterReviewGateSync,
            MigrationFaultStage::AfterReviewLink,
            MigrationFaultStage::AfterReviewPublication,
            MigrationFaultStage::AfterCompleteReviewNormalizationReserving,
            MigrationFaultStage::AfterCompleteReviewNormalizationPreparing,
            MigrationFaultStage::AfterCompleteReviewNormalizationReady,
            MigrationFaultStage::AfterCompleteReviewNormalizationPublished,
            MigrationFaultStage::AfterCompleteReviewNormalizationCleanup,
            MigrationFaultStage::AfterCompleteReviewGateSync,
            MigrationFaultStage::AfterReviewResultStateTempSync,
            MigrationFaultStage::BeforeFreezeGuard,
            MigrationFaultStage::AfterFreezeBuildingStateSync,
            MigrationFaultStage::AfterFreezeProgressReadyStateSync,
            MigrationFaultStage::FreezePreparingSynced,
            MigrationFaultStage::FreezeTempSynced,
            MigrationFaultStage::FreezePreparedRecordSynced,
            MigrationFaultStage::FreezeEntryPublished,
            MigrationFaultStage::FreezeProgressSynced,
            MigrationFaultStage::AfterDirectoryFreezingStateSync,
            MigrationFaultStage::AfterJournalDirectoryChmod,
            MigrationFaultStage::AfterDirectoryFrozenStateSync,
            MigrationFaultStage::AfterManifestBuildingStateSync,
            MigrationFaultStage::AfterManifestTempSync,
            MigrationFaultStage::AfterManifestVerifiedStateSync,
            MigrationFaultStage::AfterManifestPublication,
            MigrationFaultStage::AfterManifestPublishedStateSync,
            MigrationFaultStage::AfterLegacyFrozen,
            MigrationFaultStage::AfterLegacyFrozenStateTempSync,
            MigrationFaultStage::AfterDatabaseComplete,
            MigrationFaultStage::AfterCompleteStateTempSync,
            MigrationFaultStage::AfterCompleteState,
        ] {
            assert_eq!(MigrationFaultStage::from_label(stage.as_str()), Some(stage));
        }
        assert_eq!(MigrationFaultStage::from_label("unknown-stage"), None);

        let mut fixture = Fixture::new(FaultSelection::Matrix(FaultPoint::MigrationPublish));
        let controller = Controller::new(fixture.activation()).unwrap();
        assert!(
            !controller
                .hit_migration(MigrationFaultStage::AfterBrainLink)
                .unwrap()
        );
        assert!(
            controller
                .hit_migration(MigrationFaultStage::AfterBrainPublication)
                .unwrap()
        );
    }

    #[test]
    fn worker_rejects_every_hook_owned_matrix_point() {
        let temp = tempfile::tempdir().unwrap();
        for point in [
            FaultPoint::AdmissionWrite,
            FaultPoint::InferenceExit,
            FaultPoint::CommitBeforeCall,
            FaultPoint::CommitAfterReturn,
            FaultPoint::StdoutWrite,
            FaultPoint::DeliveryWrite,
        ] {
            assert!(run_worker(&FaultSelection::Matrix(point), temp.path()).is_err());
        }
    }

    #[test]
    fn worker_accepts_only_non_hook_storage_selections() {
        let checkpoint = tempfile::tempdir().unwrap();
        fs::set_permissions(checkpoint.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let checkpoint_root = checkpoint.path().join("state");
        fs::create_dir(&checkpoint_root).unwrap();
        fs::set_permissions(&checkpoint_root, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = StoragePaths::at(&checkpoint_root);
        drop(BrainDb::create_current(&paths).unwrap());
        run_worker(
            &FaultSelection::Matrix(FaultPoint::Checkpoint),
            &checkpoint_root,
        )
        .unwrap();

        for selection in [
            FaultSelection::Matrix(FaultPoint::MigrationPublish),
            FaultSelection::MigrationRegression(MigrationFaultStage::Building),
        ] {
            let migration = tempfile::tempdir().unwrap();
            fs::set_permissions(migration.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let state_root = migration.path().join("state");
            run_worker(&selection, &state_root).unwrap();
        }
    }
}
