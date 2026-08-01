#![allow(dead_code)] // The hook integration task consumes these Task 2 interfaces.

use std::borrow::Cow;
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use coding_brain_core::lifecycle::LifecycleIdentity;
use fs2::FileExt;
use sha2::{Digest, Sha256};

const LOCK_DIRECTORY: &str = "brain/permission-request-locks";
const SHARD_COUNT: usize = 256;
const HASH_DOMAIN: &[u8] = b"coding-brain.permission-request-lock.v1";

#[derive(Debug)]
pub(crate) enum RequestLockError {
    Io(io::Error),
    InvalidStorage(&'static str),
}

impl fmt::Display for RequestLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "permission request lock I/O failed: {error}"),
            Self::InvalidStorage(reason) => {
                write!(
                    formatter,
                    "permission request lock storage is invalid: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for RequestLockError {}

impl From<io::Error> for RequestLockError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) struct PermissionRequestLockStore {
    directory: PathBuf,
}

impl PermissionRequestLockStore {
    pub(crate) fn at(state_root: &Path) -> Self {
        Self {
            directory: state_root.join(LOCK_DIRECTORY),
        }
    }

    pub(crate) fn try_acquire(
        &self,
        identity: &LifecycleIdentity,
        request_key: &str,
    ) -> Result<Option<PermissionRequestGuard>, RequestLockError> {
        let directory = self.initialize()?;
        let fingerprint = request_fingerprint(identity, request_key);
        let shard = fingerprint[0];
        let name = shard_name(shard);
        let file = open_valid_shard(&directory, &name)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(PermissionRequestGuard {
                file,
                shard,
                fingerprint,
                path: self.directory.join(name),
            })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn shard_for(&self, identity: &LifecycleIdentity, request_key: &str) -> u8 {
        request_fingerprint(identity, request_key)[0]
    }

    pub(crate) fn validate(&self) -> Result<(), RequestLockError> {
        self.initialize().map(drop)
    }

    fn initialize(&self) -> Result<File, RequestLockError> {
        let state_root = self
            .directory
            .parent()
            .and_then(Path::parent)
            .ok_or(RequestLockError::InvalidStorage("missing state root"))?;
        let state_root = state_root_for_traversal(state_root);
        let mut directory = open_directory(if state_root.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
        for (component, private) in state_root
            .components()
            .map(|component| (component, false))
            .chain([(Component::Normal(std::ffi::OsStr::new("brain")), true)])
            .chain([(
                Component::Normal(std::ffi::OsStr::new("permission-request-locks")),
                true,
            )])
        {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(RequestLockError::InvalidStorage(
                        "invalid lock directory path",
                    ));
                }
            };
            directory = open_or_create_directory_at(&directory, name, private)?;
        }
        for shard in 0..SHARD_COUNT {
            open_valid_shard(&directory, &shard_name(shard as u8))?;
        }
        Ok(directory)
    }
}

pub(crate) struct PermissionRequestGuard {
    file: File,
    shard: u8,
    fingerprint: [u8; 32],
    path: PathBuf,
}

impl PermissionRequestGuard {
    pub(crate) fn shard(&self) -> u8 {
        self.shard
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn matches(&self, identity: &LifecycleIdentity, request_key: &str) -> bool {
        self.fingerprint == request_fingerprint(identity, request_key)
    }
}

impl Drop for PermissionRequestGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn request_fingerprint(identity: &LifecycleIdentity, request_key: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(HASH_DOMAIN);
    hash_field(&mut hash, identity.provider().as_str().as_bytes());
    hash_field(&mut hash, identity.session_id().as_bytes());
    hash_optional(&mut hash, identity.provider_session_id().map(str::as_bytes));
    hash_optional(&mut hash, identity.turn_id().map(str::as_bytes));
    hash_field(&mut hash, identity.cwd().as_os_str().as_bytes());
    hash_field(&mut hash, request_key.as_bytes());
    hash.finalize().into()
}

fn hash_optional(hash: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_field(hash, value);
        }
        None => hash.update([0]),
    }
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn shard_name(shard: u8) -> String {
    format!("permission-request-lock-{shard:02x}")
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn open_or_create_directory_at(
    parent: &File,
    name: &std::ffi::OsStr,
    private: bool,
) -> Result<File, RequestLockError> {
    open_or_create_directory_at_with_hook(parent, name, private, || {})
}

fn open_or_create_directory_at_with_hook(
    parent: &File,
    name: &std::ffi::OsStr,
    private: bool,
    after_created_metadata: impl FnOnce(),
) -> Result<File, RequestLockError> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| RequestLockError::InvalidStorage("invalid directory name"))?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let created_metadata = if created {
        let initial = metadata_at(parent, &name)?;
        after_created_metadata();
        repair_created_directory(parent, &name, &initial)?;
        let repaired = metadata_at(parent, &name)?;
        if repaired.mode & 0o777 != 0o700
            || repaired.uid != unsafe { libc::geteuid() }
            || repaired.dev != initial.dev
            || repaired.ino != initial.ino
        {
            return Err(RequestLockError::InvalidStorage(
                "created directory changed during mode repair",
            ));
        }
        Some(repaired)
    } else {
        None
    };
    let before = metadata_at(parent, &name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(RequestLockError::InvalidStorage("open directory component"));
    }
    let child = unsafe { File::from_raw_fd(descriptor) };
    let opened_metadata = child.metadata()?;
    let opened = ShardMetadata::from(&opened_metadata);
    if before.dev != opened.dev
        || before.ino != opened.ino
        || !opened_metadata.file_type().is_dir()
        || (private && opened.uid != unsafe { libc::geteuid() })
    {
        return Err(RequestLockError::InvalidStorage(
            "directory changed during open",
        ));
    }
    if let Some(created) = created_metadata.as_ref() {
        if created.dev != opened.dev || created.ino != opened.ino {
            return Err(RequestLockError::InvalidStorage(
                "created directory changed before final validation",
            ));
        }
    }
    if private && !created && opened.mode & 0o700 != 0o700 {
        return Err(RequestLockError::InvalidStorage(
            "private directory lacks owner permissions",
        ));
    }
    if private && opened.mode & 0o777 != 0o700 {
        child.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let after = ShardMetadata::from(&child.metadata()?);
    if private
        && (after.mode & 0o777 != 0o700
            || after.uid != unsafe { libc::geteuid() }
            || after.dev != opened.dev
            || after.ino != opened.ino)
    {
        return Err(RequestLockError::InvalidStorage(
            "invalid private directory",
        ));
    }
    Ok(child)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod created_directory_platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    pub(super) const OPEN_FLAGS: libc::c_int =
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    pub(super) fn chmod(directory: &File) -> io::Result<()> {
        // Linux assigns 452 to fchmodat2; the locked libc omits the named
        // constant on supported aarch64 and Android targets.
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
}

#[cfg(target_vendor = "apple")]
mod created_directory_platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    pub(super) const OPEN_FLAGS: libc::c_int = libc::O_SEARCH | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    pub(super) fn chmod(directory: &File) -> io::Result<()> {
        let result = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
        (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod created_directory_platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    pub(super) const OPEN_FLAGS: libc::c_int =
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

    pub(super) fn chmod(directory: &File) -> io::Result<()> {
        let result = unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) };
        (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)
    }
}

pub(super) fn state_root_for_traversal(state_root: &Path) -> Cow<'_, Path> {
    #[cfg(target_vendor = "apple")]
    // These are fixed root-owned Darwin compatibility aliases. The remaining
    // caller-selected suffix is still traversed component-by-component without following symlinks.
    for (alias, target) in [
        (Path::new("/var"), Path::new("/private/var")),
        (Path::new("/tmp"), Path::new("/private/tmp")),
    ] {
        if let Ok(suffix) = state_root.strip_prefix(alias) {
            return Cow::Owned(target.join(suffix));
        }
    }

    Cow::Borrowed(state_root)
}

fn repair_created_directory(
    parent: &File,
    name: &CString,
    initial: &ShardMetadata,
) -> Result<(), RequestLockError> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            created_directory_platform::OPEN_FLAGS,
        )
    };
    if descriptor < 0 {
        return Err(RequestLockError::InvalidStorage(
            "open created directory for mode repair",
        ));
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    validate_created_directory_descriptor(&directory, initial)?;
    if initial.mode & 0o777 != 0o700 {
        created_directory_platform::chmod(&directory)?;
    }
    validate_repaired_directory_descriptor(&directory, initial)
}

fn validate_created_directory_descriptor(
    directory: &File,
    initial: &ShardMetadata,
) -> Result<(), RequestLockError> {
    let metadata = directory.metadata()?;
    let opened = ShardMetadata::from(&metadata);
    if !metadata.file_type().is_dir()
        || opened.uid != unsafe { libc::geteuid() }
        || opened.dev != initial.dev
        || opened.ino != initial.ino
    {
        return Err(RequestLockError::InvalidStorage(
            "created directory changed before mode repair",
        ));
    }
    Ok(())
}

fn validate_repaired_directory_descriptor(
    directory: &File,
    initial: &ShardMetadata,
) -> Result<(), RequestLockError> {
    let metadata = directory.metadata()?;
    let repaired = ShardMetadata::from(&metadata);
    if repaired.mode & 0o777 != 0o700
        || !metadata.file_type().is_dir()
        || repaired.uid != unsafe { libc::geteuid() }
        || repaired.dev != initial.dev
        || repaired.ino != initial.ino
    {
        return Err(RequestLockError::InvalidStorage(
            "invalid created directory after mode repair",
        ));
    }
    Ok(())
}

fn open_valid_shard(directory: &File, name: &str) -> Result<File, RequestLockError> {
    open_valid_shard_with_hook(directory, name, || {})
}

fn open_valid_shard_with_hook(
    directory: &File,
    name: &str,
    after_open: impl FnOnce(),
) -> Result<File, RequestLockError> {
    let name = CString::new(name).expect("fixed shard name contains no NUL");
    let mut created = false;
    let mut descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
        descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        created = descriptor >= 0;
        if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
            descriptor = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
        }
    }
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
            Err(RequestLockError::InvalidStorage("shard symlink"))
        } else {
            Err(error.into())
        };
    }
    let file = unsafe { File::from_raw_fd(descriptor) };
    if created {
        let result = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    after_open();
    let before = metadata_at(directory, &name)?;
    validate_shard_metadata(&before)?;
    let opened = ShardMetadata::from(&file.metadata()?);
    validate_shard_metadata(&opened)?;
    if before.dev != opened.dev || before.ino != opened.ino {
        return Err(RequestLockError::InvalidStorage(
            "shard replaced during open",
        ));
    }
    if !created && opened.mode & 0o600 != 0o600 {
        return Err(RequestLockError::InvalidStorage(
            "shard lacks owner permissions",
        ));
    }
    if opened.mode & 0o777 != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    let after = metadata_at(directory, &name)?;
    let opened_after = ShardMetadata::from(&file.metadata()?);
    validate_shard_metadata(&after)?;
    validate_shard_metadata(&opened_after)?;
    if after.mode & 0o777 != 0o600
        || opened_after.mode & 0o777 != 0o600
        || after.dev != opened_after.dev
        || after.ino != opened_after.ino
        || before.dev != after.dev
        || before.ino != after.ino
    {
        return Err(RequestLockError::InvalidStorage("unstable shard inode"));
    }
    Ok(file)
}

#[derive(Clone, Copy)]
struct ShardMetadata {
    mode: u32,
    uid: u32,
    len: u64,
    nlink: u64,
    dev: u64,
    ino: u64,
}

impl From<&fs::Metadata> for ShardMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            len: metadata.len(),
            nlink: metadata.nlink(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[allow(clippy::unnecessary_cast)]
fn metadata_at(directory: &File, name: &CString) -> io::Result<ShardMetadata> {
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
    Ok(ShardMetadata {
        mode: stat.st_mode as u32,
        uid: stat.st_uid as u32,
        len: stat.st_size as u64,
        nlink: stat.st_nlink as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

#[allow(clippy::unnecessary_cast)]
fn validate_shard_metadata(metadata: &ShardMetadata) -> Result<(), RequestLockError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.len != 0
        || metadata.nlink != 1
    {
        return Err(RequestLockError::InvalidStorage(
            "shard owner, type, content, or links",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    #[cfg(target_vendor = "apple")]
    use std::path::Path;

    use coding_brain_core::lifecycle::LifecycleIdentity;
    use coding_brain_core::provider::AgentProvider;

    #[cfg(target_vendor = "apple")]
    use super::state_root_for_traversal;
    use super::{
        PermissionRequestLockStore, RequestLockError, open_directory,
        open_or_create_directory_at_with_hook, open_valid_shard_with_hook, shard_name,
    };

    const HELPER_ENV: &str = "CODING_BRAIN_PERMISSION_LOCK_HELPER";

    fn identity(session: &str) -> LifecycleIdentity {
        LifecycleIdentity::try_new_with_provider_session(
            AgentProvider::Codex,
            session.to_owned(),
            Some(format!("provider-{session}")),
            Some("turn-1".to_owned()),
            None,
            "/workspace".into(),
        )
        .unwrap()
    }

    fn wait_for(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn holder(
        root: &std::path::Path,
        ready: &std::path::Path,
        release: &std::path::Path,
        session: &str,
        request: &str,
        restrictive_umask: bool,
    ) -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("brain::permission_request_lock::tests::subprocess_lock_holder")
            .arg("--nocapture")
            .env(HELPER_ENV, "1")
            .env("CODING_BRAIN_PERMISSION_LOCK_ROOT", root)
            .env("CODING_BRAIN_PERMISSION_LOCK_READY", ready)
            .env("CODING_BRAIN_PERMISSION_LOCK_RELEASE", release)
            .env("CODING_BRAIN_PERMISSION_LOCK_SESSION", session)
            .env("CODING_BRAIN_PERMISSION_LOCK_REQUEST", request)
            .stdout(Stdio::null());
        if restrictive_umask {
            command.env("CODING_BRAIN_PERMISSION_LOCK_UMASK", "0600");
        }
        command.spawn().unwrap()
    }

    #[test]
    fn subprocess_lock_holder() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let root = std::env::var_os("CODING_BRAIN_PERMISSION_LOCK_ROOT").unwrap();
        let ready = std::env::var_os("CODING_BRAIN_PERMISSION_LOCK_READY").unwrap();
        let release = std::env::var_os("CODING_BRAIN_PERMISSION_LOCK_RELEASE").unwrap();
        let session = std::env::var("CODING_BRAIN_PERMISSION_LOCK_SESSION").unwrap();
        let request = std::env::var("CODING_BRAIN_PERMISSION_LOCK_REQUEST").unwrap();
        if std::env::var_os("CODING_BRAIN_PERMISSION_LOCK_UMASK").is_some() {
            unsafe { libc::umask(0o600) };
        }
        let guard = PermissionRequestLockStore::at(std::path::Path::new(&root))
            .try_acquire(&identity(&session), &request)
            .unwrap()
            .expect("helper acquires shard");
        fs::write(&ready, b"ready").unwrap();
        wait_for(std::path::Path::new(&release));
        drop(guard);
    }

    #[test]
    fn same_shard_has_one_cross_process_winner_and_recovers_after_kill() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        let mut child = holder(temp.path(), &ready, &release, "same", "request", false);
        wait_for(&ready);

        let store = PermissionRequestLockStore::at(temp.path());
        assert!(
            store
                .try_acquire(&identity("same"), "request")
                .unwrap()
                .is_none()
        );

        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            store
                .try_acquire(&identity("same"), "request")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn different_shards_proceed_independently() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        let store = PermissionRequestLockStore::at(temp.path());
        let first_shard = store.shard_for(&identity("a"), "one");
        let mut child = holder(temp.path(), &ready, &release, "a", "one", false);
        wait_for(&ready);
        let (other, other_shard) = (0..10_000)
            .find_map(|number| {
                let request = format!("other-{number}");
                let shard = store.shard_for(&identity("b"), &request);
                (shard != first_shard).then_some((request, shard))
            })
            .unwrap();
        let second = store.try_acquire(&identity("b"), &other).unwrap();
        assert!(second.is_some());
        assert_ne!(first_shard, other_shard);
        fs::write(release, b"release").unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn restrictive_umask_creation_repairs_exact_modes_and_remains_acquirable() {
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");
        fs::write(&release, b"release").unwrap();

        let mut child = holder(temp.path(), &ready, &release, "umask", "request", true);
        wait_for(&ready);
        assert!(child.wait().unwrap().success());

        let store = PermissionRequestLockStore::at(temp.path());
        let guard = store
            .try_acquire(&identity("umask"), "request")
            .unwrap()
            .expect("normal acquisition succeeds after restrictive creation");
        assert_eq!(
            fs::metadata(temp.path().join("brain"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(temp.path().join("brain/permission-request-locks"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(guard.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn directory_repair_source_cfg_contains_platform_specific_apis() {
        let source = include_str!("permission_request_lock.rs");
        let linux_start = source
            .find("#[cfg(any(target_os = \"linux\", target_os = \"android\"))]")
            .unwrap();
        let apple_start = source.find("#[cfg(target_vendor = \"apple\")]").unwrap();
        for linux_only in [
            concat!("libc::", "O_PATH"),
            concat!("libc::", "AT_EMPTY_PATH"),
            concat!("libc::", "syscall"),
            concat!("SYS_", "FCHMODAT2"),
        ] {
            let occurrences = source.match_indices(linux_only).collect::<Vec<_>>();
            assert!(
                !occurrences.is_empty(),
                "Linux request-lock repair must reference {linux_only}"
            );
            assert!(
                occurrences
                    .iter()
                    .all(|(index, _)| (linux_start..apple_start).contains(index)),
                "{linux_only} must remain inside the Linux/Android cfg"
            );
        }
        let apple_repair = &source[apple_start..source.find("#[cfg(not(any(").unwrap()];
        assert!(apple_repair.contains(concat!("libc::", "O_SEARCH")));
        assert!(apple_repair.contains(concat!("libc::", "fchmod")));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn traversal_normalizes_only_fixed_darwin_system_aliases() {
        assert_eq!(
            state_root_for_traversal(Path::new("/var/folders/state")),
            Path::new("/private/var/folders/state")
        );
        assert_eq!(
            state_root_for_traversal(Path::new("/tmp/state")),
            Path::new("/private/tmp/state")
        );
        assert_eq!(
            state_root_for_traversal(Path::new("/various/state")),
            Path::new("/various/state")
        );
        assert_eq!(
            state_root_for_traversal(Path::new("/Users/runner/state")),
            Path::new("/Users/runner/state")
        );
    }

    #[test]
    fn rejects_same_owner_created_directory_replacement_during_mode_repair() {
        let temp = tempfile::tempdir().unwrap();
        let parent = open_directory(temp.path()).unwrap();
        let path = temp.path().join("managed");
        let displaced = temp.path().join("displaced");

        let result = open_or_create_directory_at_with_hook(
            &parent,
            std::ffi::OsStr::new("managed"),
            true,
            || {
                fs::rename(&path, &displaced).unwrap();
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
            },
        );

        assert!(matches!(result, Err(RequestLockError::InvalidStorage(_))));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert_ne!(
            fs::metadata(path).unwrap().ino(),
            fs::metadata(displaced).unwrap().ino()
        );
    }

    #[test]
    fn guard_proof_matches_full_request_not_only_shard() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionRequestLockStore::at(temp.path());
        let identity = identity("collision");
        let guard = store.try_acquire(&identity, "original").unwrap().unwrap();
        let collision = (0..100_000)
            .map(|number| format!("collision-{number}"))
            .find(|request| store.shard_for(&identity, request) == guard.shard())
            .unwrap();

        assert!(!guard.matches(&identity, &collision));
    }

    #[test]
    fn initialization_rejects_brain_symlink_without_mutating_its_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let state_root = temp.path().join("state");
        let victim = temp.path().join("victim");
        fs::create_dir_all(&state_root).unwrap();
        fs::create_dir_all(&victim).unwrap();
        symlink(&victim, state_root.join("brain")).unwrap();

        let result = PermissionRequestLockStore::at(&state_root)
            .try_acquire(&identity("symlink"), "request");

        assert!(matches!(result, Err(RequestLockError::InvalidStorage(_))));
        assert!(!victim.join("permission-request-locks").exists());
        assert!(fs::read_dir(&victim).unwrap().next().is_none());
    }

    #[test]
    fn narrows_mode_and_never_unlinks_persistent_shard() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionRequestLockStore::at(temp.path());
        let identity = identity("mode");
        let guard = store.try_acquire(&identity, "request").unwrap().unwrap();
        let path = guard.path().to_owned();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        drop(guard);
        let guard = store.try_acquire(&identity, "request").unwrap().unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(guard);
        assert!(path.exists());
    }

    #[test]
    fn rejects_without_widening_existing_restrictive_managed_modes() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionRequestLockStore::at(temp.path());
        let guard = store
            .try_acquire(&identity("restrictive-existing"), "request")
            .unwrap()
            .unwrap();
        let shard = guard.path().to_owned();
        drop(guard);
        fs::set_permissions(&shard, fs::Permissions::from_mode(0o400)).unwrap();

        assert!(
            store
                .try_acquire(&identity("restrictive-existing"), "request")
                .is_err()
        );
        assert_eq!(
            fs::metadata(&shard).unwrap().permissions().mode() & 0o777,
            0o400
        );

        fs::set_permissions(&shard, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&store.directory, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(
            store
                .try_acquire(&identity("restrictive-existing"), "request")
                .is_err()
        );
        assert_eq!(
            fs::metadata(&store.directory).unwrap().permissions().mode() & 0o777,
            0o500
        );
    }

    #[test]
    fn rejects_symlink_content_and_hard_link_attacks() {
        use std::os::unix::fs::symlink;

        for attack in ["symlink", "content", "hardlink"] {
            let temp = tempfile::tempdir().unwrap();
            let store = PermissionRequestLockStore::at(temp.path());
            let identity = identity(attack);
            let guard = store.try_acquire(&identity, "request").unwrap().unwrap();
            let path = guard.path().to_owned();
            drop(guard);
            match attack {
                "symlink" => {
                    fs::remove_file(&path).unwrap();
                    let target = temp.path().join("target");
                    fs::write(&target, b"").unwrap();
                    symlink(target, &path).unwrap();
                }
                "content" => fs::write(&path, b"x").unwrap(),
                "hardlink" => {
                    fs::hard_link(&path, temp.path().join("alias")).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                store.try_acquire(&identity, "request"),
                Err(RequestLockError::InvalidStorage(_))
            ));
        }
    }

    #[test]
    fn rejects_inode_replacement_between_open_and_validation() {
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionRequestLockStore::at(temp.path());
        let directory = store.initialize().unwrap();
        let name = shard_name(store.shard_for(&identity("replacement"), "request"));
        let path = store.directory.join(&name);

        let result = open_valid_shard_with_hook(&directory, &name, || {
            fs::remove_file(&path).unwrap();
            fs::write(&path, b"").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        });

        assert!(matches!(result, Err(RequestLockError::InvalidStorage(_))));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_foreign_owner_when_runner_can_create_one() {
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let store = PermissionRequestLockStore::at(temp.path());
        let identity = identity("owner");
        let guard = store.try_acquire(&identity, "request").unwrap().unwrap();
        let path = guard.path().to_owned();
        drop(guard);
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 1, u32::MAX) }, 0);

        assert!(matches!(
            store.try_acquire(&identity, "request"),
            Err(RequestLockError::InvalidStorage(_))
        ));
    }
}
