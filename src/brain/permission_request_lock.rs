#![allow(dead_code)] // The hook integration task consumes these Task 2 interfaces.

use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use coding_brain_core::lifecycle::LifecycleIdentity;
use fs2::FileExt;
use sha2::{Digest, Sha256};

pub(super) use super::secure_state::state_root_for_traversal;
use super::secure_state::{SecureStateDirectory, SecureStateError, open_or_create_nested};

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

impl From<SecureStateError> for RequestLockError {
    fn from(error: SecureStateError) -> Self {
        match error {
            SecureStateError::Io(error) => Self::Io(error),
            SecureStateError::InvalidStorage(reason) => Self::InvalidStorage(reason),
        }
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

    fn initialize(&self) -> Result<SecureStateDirectory, RequestLockError> {
        let state_root = self
            .directory
            .parent()
            .and_then(Path::parent)
            .ok_or(RequestLockError::InvalidStorage("missing state root"))?;
        let directory = open_or_create_nested(
            state_root,
            &[
                std::ffi::OsStr::new("brain"),
                std::ffi::OsStr::new("permission-request-locks"),
            ],
        )?;
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

fn open_valid_shard(
    directory: &SecureStateDirectory,
    name: &str,
) -> Result<File, RequestLockError> {
    open_valid_shard_with_hook(directory, name, || {})
}

fn open_valid_shard_with_hook(
    directory: &SecureStateDirectory,
    name: &str,
    after_open: impl FnOnce(),
) -> Result<File, RequestLockError> {
    let name = CString::new(name).expect("fixed shard name contains no NUL");
    match directory.metadata(&name) {
        Ok(metadata) if metadata.len != 0 => {
            return Err(RequestLockError::InvalidStorage("shard content"));
        }
        Ok(_) => {}
        Err(SecureStateError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let file = directory.open_regular_with_hook(&name, true, after_open)?;
    if file.metadata()?.len() != 0 {
        return Err(RequestLockError::InvalidStorage("shard content"));
    }
    Ok(file)
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
    use super::super::secure_state::state_root_for_traversal;
    use super::super::secure_state::{
        SecureStateError, open_directory, open_or_create_directory_at_with_hook,
    };
    use super::{
        PermissionRequestLockStore, RequestLockError, open_valid_shard_with_hook, shard_name,
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
        let source = include_str!("secure_state.rs");
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

        assert!(matches!(result, Err(SecureStateError::InvalidStorage(_))));
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
