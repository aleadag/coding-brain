use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use super::super::secure_state::state_root_for_traversal;

#[derive(Debug)]
pub(super) enum SecurityError {
    Missing,
    Invalid(&'static str),
    Io(io::Error),
}

impl From<io::Error> for SecurityError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::NotFound {
            Self::Missing
        } else {
            Self::Io(error)
        }
    }
}

pub(super) struct SecureDatabaseDirectory {
    descriptor: File,
    path: PathBuf,
}

impl SecureDatabaseDirectory {
    pub(super) fn prepare(state_root: &Path, create: bool) -> Result<Self, SecurityError> {
        validate_or_create_state_root(state_root, create)?;
        if create {
            let state_root = open_existing_directory(state_root)?;
            open_or_create_directory_at(&state_root, OsStr::new("db"), true)?;
        }
        let path = state_root.join("db");
        let descriptor = open_existing_directory(&path)?;
        validate_private_directory(&descriptor)?;
        validate_local_filesystem(&descriptor)?;
        Ok(Self { descriptor, path })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn reject_untrusted_entries(
        &self,
        database_name: &CStr,
        database_must_exist: bool,
    ) -> Result<(), SecurityError> {
        self.reject_sidecars(database_name)?;
        match metadata_at(&self.descriptor, database_name) {
            Ok(metadata) => validate_private_file(&metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound && !database_must_exist => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn create_database_file(&self, name: &CStr) -> Result<File, SecurityError> {
        let descriptor = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
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
        validate_private_file(&EntryMetadata::from(&file.metadata()?))?;
        let at_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&at_path)?;
        let opened = EntryMetadata::from(&file.metadata()?);
        if at_path.dev != opened.dev || at_path.ino != opened.ino {
            return Err(SecurityError::Invalid(
                "database file changed during creation",
            ));
        }
        Ok(file)
    }

    pub(super) fn validate_after_open(&self, database_name: &CStr) -> Result<(), SecurityError> {
        let metadata = metadata_at(&self.descriptor, database_name)?;
        validate_private_file(&metadata)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let name = sidecar_name(database_name, suffix)?;
            match metadata_at(&self.descriptor, &name) {
                Ok(metadata) => validate_private_file(&metadata)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn reject_sidecars(&self, database_name: &CStr) -> Result<(), SecurityError> {
        let database_name = OsStr::from_bytes(database_name.to_bytes());
        let mut sidecar_prefix = database_name.as_bytes().to_vec();
        sidecar_prefix.push(b'-');
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            if entry.file_name().as_bytes().starts_with(&sidecar_prefix) {
                return Err(SecurityError::Invalid("pre-existing SQLite sidecar"));
            }
        }
        Ok(())
    }
}

fn validate_or_create_state_root(state_root: &Path, create: bool) -> Result<(), SecurityError> {
    let state_root = state_root_for_traversal(state_root);
    let mut directory = open_directory(if state_root.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    let components = normal_components(&state_root)?;
    if components.is_empty() {
        return validate_private_state_root_metadata(&EntryMetadata::from(&directory.metadata()?));
    }

    for (index, name) in components.iter().enumerate() {
        validate_safe_ancestor_metadata(&EntryMetadata::from(&directory.metadata()?))?;
        directory = if create {
            open_or_create_directory_at(&directory, name, index + 1 == components.len())?
        } else {
            open_directory_at(&directory, name)?
        };
        if index + 1 == components.len() {
            validate_private_state_root_metadata(&EntryMetadata::from(&directory.metadata()?))?;
        }
    }
    Ok(())
}

fn normal_components(path: &Path) -> Result<Vec<&OsStr>, SecurityError> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir | Component::CurDir => None,
            Component::Normal(name) => Some(Ok(name)),
            Component::ParentDir | Component::Prefix(_) => {
                Some(Err(SecurityError::Invalid("invalid state directory path")))
            }
        })
        .collect()
}

fn open_or_create_directory_at(
    parent: &File,
    name: &OsStr,
    private: bool,
) -> Result<File, SecurityError> {
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| SecurityError::Invalid("invalid database directory name"))?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), c_name.as_ptr(), 0o700) } == 0;
    if !created {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }

    let directory = open_directory_at(parent, name)?;
    if created && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let path_metadata = metadata_at(parent, &c_name)?;
    let opened_metadata = EntryMetadata::from(&directory.metadata()?);
    if created || private {
        validate_private_state_root_metadata(&path_metadata)?;
        validate_private_state_root_metadata(&opened_metadata)?;
    }
    if path_metadata.dev != opened_metadata.dev || path_metadata.ino != opened_metadata.ino {
        return Err(SecurityError::Invalid(
            "created directory changed during validation",
        ));
    }
    Ok(directory)
}

fn open_existing_directory(path: &Path) -> Result<File, SecurityError> {
    let path = state_root_for_traversal(path);
    let mut directory = open_directory(if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(SecurityError::Invalid("invalid database directory path"));
            }
        };
        directory = open_directory_at(&directory, name)?;
    }
    Ok(directory)
}

fn open_directory(path: &Path) -> io::Result<File> {
    let name = CString::new(path.as_os_str().as_bytes())?;
    let descriptor = unsafe {
        libc::open(
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

#[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
fn open_directory_at(parent: &File, name: &OsStr) -> Result<File, SecurityError> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| SecurityError::Invalid("invalid database directory name"))?;
    let before = metadata_at(parent, &name)?;
    if before.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
        return Err(SecurityError::Invalid(
            "database directory component is not a directory",
        ));
    }
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
            Err(SecurityError::Invalid("database directory symlink"))
        } else {
            Err(error.into())
        };
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    let opened = directory.metadata()?;
    if !opened.file_type().is_dir() || before.dev != opened.dev() || before.ino != opened.ino() {
        return Err(SecurityError::Invalid(
            "database directory changed during open",
        ));
    }
    Ok(directory)
}

fn validate_private_directory(directory: &File) -> Result<(), SecurityError> {
    let metadata = directory.metadata()?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(SecurityError::Invalid(
            "database directory is not owner-only",
        ));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
fn validate_safe_ancestor_metadata(metadata: &EntryMetadata) -> Result<(), SecurityError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32 {
        return Err(SecurityError::Invalid(
            "state directory ancestor is not a directory",
        ));
    }
    if metadata.mode & 0o022 != 0 {
        let sticky = metadata.mode & libc::S_ISVTX as u32 != 0;
        let trusted_owner = metadata.uid == 0 || metadata.uid == unsafe { libc::geteuid() };
        if !sticky || !trusted_owner {
            return Err(SecurityError::Invalid(
                "state directory ancestor is replaceable by another user",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
fn validate_private_state_root_metadata(metadata: &EntryMetadata) -> Result<(), SecurityError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.mode & 0o777 != 0o700
    {
        return Err(SecurityError::Invalid("state directory is not owner-only"));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
fn validate_private_file(metadata: &EntryMetadata) -> Result<(), SecurityError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.mode & 0o777 != 0o600
        || metadata.nlink != 1
    {
        return Err(SecurityError::Invalid(
            "database entry owner, type, mode, or links",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct EntryMetadata {
    mode: u32,
    uid: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
}

impl From<&fs::Metadata> for EntryMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            nlink: metadata.nlink(),
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }
}

#[allow(clippy::unnecessary_cast)]
fn metadata_at(directory: &File, name: &CStr) -> io::Result<EntryMetadata> {
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
    Ok(EntryMetadata {
        mode: stat.st_mode as u32,
        uid: stat.st_uid as u32,
        nlink: stat.st_nlink as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

fn sidecar_name(database_name: &CStr, suffix: &str) -> Result<CString, SecurityError> {
    let mut bytes = database_name.to_bytes().to_vec();
    bytes.extend_from_slice(suffix.as_bytes());
    CString::new(bytes).map_err(|_| SecurityError::Invalid("invalid SQLite sidecar name"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn validate_local_filesystem(directory: &File) -> Result<(), SecurityError> {
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let filesystem = unsafe { status.assume_init() }.f_type as u64;
    const LOCAL_FILESYSTEMS: &[u64] = &[
        0x0000_4d44, // MSDOS
        0x0102_1994, // tmpfs
        0x2fc1_2fc1, // ZFS
        0x3153_464a, // JFS
        0x5265_4973, // ReiserFS
        0x5846_5342, // XFS
        0x794c_7630, // overlayfs
        0x8584_58f6, // ramfs
        0x9123_683e, // Btrfs
        0xef53,      // ext2/3/4
        0xf2f5_2010, // F2FS
    ];
    if LOCAL_FILESYSTEMS.contains(&filesystem) {
        Ok(())
    } else {
        Err(SecurityError::Invalid(
            "database directory is not on a local filesystem",
        ))
    }
}

#[cfg(target_vendor = "apple")]
fn validate_local_filesystem(directory: &File) -> Result<(), SecurityError> {
    let mut status = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if unsafe { status.assume_init() }.f_flags & libc::MNT_LOCAL != 0 {
        Ok(())
    } else {
        Err(SecurityError::Invalid(
            "database directory is not on a local filesystem",
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn validate_local_filesystem(_directory: &File) -> Result<(), SecurityError> {
    Err(SecurityError::Invalid(
        "local filesystem validation is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
    fn private_state_root_metadata_rejects_foreign_owner() {
        let metadata = EntryMetadata {
            mode: libc::S_IFDIR as u32 | 0o700,
            uid: unsafe { libc::geteuid() }.wrapping_add(1),
            nlink: 1,
            dev: 1,
            ino: 1,
        };

        assert!(validate_private_state_root_metadata(&metadata).is_err());
    }
}
