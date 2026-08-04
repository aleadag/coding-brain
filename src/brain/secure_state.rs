use std::borrow::Cow;
use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

// C variadics promote Darwin's narrower mode_t to c_uint.
const PRIVATE_CREATE_MODE: libc::c_uint = 0o600;

#[derive(Debug)]
pub(crate) enum SecureStateError {
    Io(io::Error),
    InvalidStorage(&'static str),
}

impl fmt::Display for SecureStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "secure state I/O failed: {error}"),
            Self::InvalidStorage(reason) => write!(formatter, "secure state is invalid: {reason}"),
        }
    }
}

impl std::error::Error for SecureStateError {}

impl From<io::Error> for SecureStateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) struct SecureStateDirectory {
    descriptor: File,
    display_path: PathBuf,
}

impl SecureStateDirectory {
    pub(crate) fn open_or_create(state_root: &Path) -> Result<Self, SecureStateError> {
        let state_root = state_root_for_traversal(state_root);
        let components = normal_components(&state_root)?;
        let mut directory = open_directory(if state_root.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
        for (index, name) in components.iter().enumerate() {
            let final_component = index + 1 == components.len();
            directory = if final_component {
                open_or_create_directory_at_strict(&directory, name)?
            } else {
                open_or_create_directory_at(&directory, name, false)?
            };
        }
        Ok(Self {
            descriptor: directory,
            display_path: state_root.into_owned(),
        })
    }

    pub(crate) fn open_existing_strict(state_root: &Path) -> Result<Self, SecureStateError> {
        let state_root = state_root_for_traversal(state_root);
        let components = normal_components(&state_root)?;
        let mut directory = open_directory(if state_root.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        })?;
        for (index, name) in components.iter().enumerate() {
            directory =
                open_existing_directory_at(&directory, name, index + 1 == components.len())?;
        }
        Ok(Self {
            descriptor: directory,
            display_path: state_root.into_owned(),
        })
    }

    pub(crate) fn open_regular_strict(
        &self,
        name: &CStr,
        create: bool,
    ) -> Result<File, SecureStateError> {
        self.open_regular_with_policy(name, create, false, || {})
    }

    pub(crate) fn metadata(&self, name: &CStr) -> Result<SecureEntryMetadata, SecureStateError> {
        metadata_at(&self.descriptor, name).map_err(Into::into)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.descriptor.sync_all()
    }

    pub(crate) fn validate_path_correspondence(&self) -> Result<(), SecureStateError> {
        let current = Self::open_existing_strict(&self.display_path)?;
        let retained = SecureEntryMetadata::from(&self.descriptor.metadata()?);
        let reopened = SecureEntryMetadata::from(&current.descriptor.metadata()?);
        if retained.dev != reopened.dev || retained.ino != reopened.ino {
            return Err(SecureStateError::InvalidStorage(
                "directory descriptor no longer matches its path",
            ));
        }
        Ok(())
    }

    pub(crate) fn remove_regular_if_present(&self, name: &CStr) -> Result<(), SecureStateError> {
        let metadata = match metadata_at(&self.descriptor, name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        validate_regular_metadata(&metadata)?;
        if metadata.mode & 0o777 != 0o600 {
            return Err(SecureStateError::InvalidStorage(
                "regular entry mode is not 0600",
            ));
        }
        let file = self.open_regular_strict(name, false)?;
        self.validate_regular(name, &file, 0o600)?;
        self.unlink(name)?;
        self.sync()?;
        Ok(())
    }

    pub(crate) fn remove_tree_if_present(&self, name: &CStr) -> Result<(), SecureStateError> {
        let metadata = match metadata_at(&self.descriptor, name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        validate_private_directory_metadata(&metadata)?;
        let child = self.open_child_strict(name)?;
        child.remove_all_entries()?;
        let after = metadata_at(&self.descriptor, name)?;
        let opened = SecureEntryMetadata::from(&child.descriptor.metadata()?);
        validate_private_directory_metadata(&after)?;
        validate_private_directory_metadata(&opened)?;
        if after.dev != opened.dev || after.ino != opened.ino {
            return Err(SecureStateError::InvalidStorage(
                "directory entry descriptor no longer matches its path",
            ));
        }
        let result = unsafe {
            libc::unlinkat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.sync()?;
        Ok(())
    }

    fn open_child_strict(&self, name: &CStr) -> Result<Self, SecureStateError> {
        let before = metadata_at(&self.descriptor, name)?;
        validate_private_directory_metadata(&before)?;
        let descriptor = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(SecureStateError::InvalidStorage(
                "open private directory entry",
            ));
        }
        let descriptor = unsafe { File::from_raw_fd(descriptor) };
        let opened = SecureEntryMetadata::from(&descriptor.metadata()?);
        validate_private_directory_metadata(&opened)?;
        if before.dev != opened.dev || before.ino != opened.ino {
            return Err(SecureStateError::InvalidStorage(
                "private directory changed during open",
            ));
        }
        Ok(Self {
            descriptor,
            display_path: self.display_path.join(OsStr::from_bytes(name.to_bytes())),
        })
    }

    #[allow(clippy::unnecessary_cast)] // libc mode constants vary across Unix targets.
    fn remove_all_entries(&self) -> Result<(), SecureStateError> {
        for name in directory_entry_names(&self.descriptor)? {
            let metadata = metadata_at(&self.descriptor, &name)?;
            let kind = metadata.mode & libc::S_IFMT as u32;
            if kind == libc::S_IFREG as u32 {
                self.remove_regular_if_present(&name)?;
            } else if kind == libc::S_IFDIR as u32 {
                self.remove_tree_if_present(&name)?;
            } else {
                return Err(SecureStateError::InvalidStorage(
                    "managed directory contains an unsafe entry",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn open_regular_with_hook(
        &self,
        name: &CStr,
        create: bool,
        after_open: impl FnOnce(),
    ) -> Result<File, SecureStateError> {
        self.open_regular_with_policy(name, create, true, after_open)
    }

    fn open_regular_with_policy(
        &self,
        name: &CStr,
        create: bool,
        repair_existing: bool,
        after_open: impl FnOnce(),
    ) -> Result<File, SecureStateError> {
        let mut created = false;
        let mut descriptor = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 && create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound
        {
            descriptor = unsafe {
                libc::openat(
                    self.descriptor.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    PRIVATE_CREATE_MODE,
                )
            };
            created = descriptor >= 0;
            if descriptor < 0 && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
                descriptor = unsafe {
                    libc::openat(
                        self.descriptor.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
            }
        }
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
                Err(SecureStateError::InvalidStorage("regular entry symlink"))
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
        let before = metadata_at(&self.descriptor, name)?;
        validate_regular_metadata(&before)?;
        let opened = SecureEntryMetadata::from(&file.metadata()?);
        validate_regular_metadata(&opened)?;
        if before.dev != opened.dev || before.ino != opened.ino {
            return Err(SecureStateError::InvalidStorage(
                "regular entry replaced during open",
            ));
        }
        if !created && !repair_existing && opened.mode & 0o777 != 0o600 {
            return Err(SecureStateError::InvalidStorage(
                "regular entry mode is not 0600",
            ));
        }
        if !created && opened.mode & 0o600 != 0o600 {
            return Err(SecureStateError::InvalidStorage(
                "regular entry lacks owner permissions",
            ));
        }
        if opened.mode & 0o777 != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let after = metadata_at(&self.descriptor, name)?;
        let opened_after = SecureEntryMetadata::from(&file.metadata()?);
        validate_regular_metadata(&after)?;
        validate_regular_metadata(&opened_after)?;
        if after.mode & 0o777 != 0o600
            || opened_after.mode & 0o777 != 0o600
            || after.dev != opened_after.dev
            || after.ino != opened_after.ino
            || before.dev != after.dev
            || before.ino != after.ino
        {
            return Err(SecureStateError::InvalidStorage(
                "unstable regular entry inode",
            ));
        }
        Ok(file)
    }

    pub(super) fn create_regular_exclusive(&self, name: &CStr) -> Result<File, SecureStateError> {
        let descriptor = unsafe {
            libc::openat(
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                PRIVATE_CREATE_MODE,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        let result = unsafe { libc::fchmod(file.as_raw_fd(), 0o600) };
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let path_metadata = metadata_at(&self.descriptor, name)?;
        let opened_metadata = SecureEntryMetadata::from(&file.metadata()?);
        validate_regular_metadata(&path_metadata)?;
        validate_regular_metadata(&opened_metadata)?;
        if path_metadata.mode & 0o777 != 0o600
            || opened_metadata.mode & 0o777 != 0o600
            || path_metadata.dev != opened_metadata.dev
            || path_metadata.ino != opened_metadata.ino
        {
            return Err(SecureStateError::InvalidStorage(
                "invalid exclusive regular entry",
            ));
        }
        Ok(file)
    }

    pub(super) fn validate_regular(
        &self,
        name: &CStr,
        file: &File,
        expected_mode: u32,
    ) -> Result<(), SecureStateError> {
        self.regular_correspondence(name, file, expected_mode)?;
        Ok(())
    }

    pub(super) fn publish_regular<F>(
        &self,
        source_name: &CStr,
        source: &File,
        destination_name: &CStr,
        before_rename: F,
    ) -> Result<(), SecureStateError>
    where
        F: FnOnce() -> Result<(), SecureStateError>,
    {
        let (path_metadata, opened_metadata) =
            self.regular_correspondence(source_name, source, 0o600)?;
        if path_metadata.len != opened_metadata.len {
            return Err(SecureStateError::InvalidStorage(
                "regular publication source changed",
            ));
        }
        before_rename()?;
        let result = unsafe {
            libc::renameat(
                self.descriptor.as_raw_fd(),
                source_name.as_ptr(),
                self.descriptor.as_raw_fd(),
                destination_name.as_ptr(),
            )
        };
        (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)?;
        Ok(())
    }

    fn regular_correspondence(
        &self,
        name: &CStr,
        file: &File,
        expected_mode: u32,
    ) -> Result<(SecureEntryMetadata, SecureEntryMetadata), SecureStateError> {
        let path_metadata = metadata_at(&self.descriptor, name)?;
        let opened_metadata = SecureEntryMetadata::from(&file.metadata()?);
        validate_regular_metadata(&path_metadata)?;
        validate_regular_metadata(&opened_metadata)?;
        if path_metadata.mode & 0o777 != expected_mode
            || opened_metadata.mode & 0o777 != expected_mode
            || path_metadata.dev != opened_metadata.dev
            || path_metadata.ino != opened_metadata.ino
        {
            return Err(SecureStateError::InvalidStorage(
                "regular entry descriptor no longer matches its path",
            ));
        }
        Ok((path_metadata, opened_metadata))
    }

    pub(super) fn unlink(&self, name: &CStr) -> io::Result<()> {
        let result = unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), name.as_ptr(), 0) };
        (result == 0)
            .then_some(())
            .ok_or_else(io::Error::last_os_error)
    }
}

fn open_existing_directory_at(
    parent: &File,
    name: &OsStr,
    private: bool,
) -> Result<File, SecureStateError> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| SecureStateError::InvalidStorage("invalid directory name"))?;
    let before = metadata_at(parent, &name)?;
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(SecureStateError::InvalidStorage("open directory component"));
    }
    let directory = unsafe { File::from_raw_fd(descriptor) };
    let opened = SecureEntryMetadata::from(&directory.metadata()?);
    if before.dev != opened.dev || before.ino != opened.ino {
        return Err(SecureStateError::InvalidStorage(
            "directory changed during open",
        ));
    }
    if private {
        validate_private_directory_metadata(&before)?;
        validate_private_directory_metadata(&opened)?;
    }
    Ok(directory)
}

fn directory_entry_names(directory: &File) -> Result<Vec<CString>, SecureStateError> {
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(duplicate) };
        return Err(error.into());
    }
    let mut names = Vec::new();
    loop {
        errno::set_errno(errno::Errno(0));
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = errno::errno().0;
            unsafe { libc::closedir(stream) };
            return if error == 0 {
                Ok(names)
            } else {
                Err(io::Error::from_raw_os_error(error).into())
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
}

pub(super) fn open_or_create_nested(
    state_root: &Path,
    private_suffix: &[&OsStr],
) -> Result<SecureStateDirectory, SecureStateError> {
    let state_root = state_root_for_traversal(state_root);
    let mut directory = open_directory(if state_root.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    for name in normal_components(&state_root)? {
        directory = open_or_create_directory_at(&directory, name, false)?;
    }
    for name in private_suffix {
        directory = open_or_create_directory_at(&directory, name, true)?;
    }
    Ok(SecureStateDirectory {
        descriptor: directory,
        display_path: private_suffix
            .iter()
            .fold(state_root.into_owned(), |path, name| path.join(name)),
    })
}

fn normal_components(path: &Path) -> Result<Vec<&OsStr>, SecureStateError> {
    path.components()
        .filter_map(|component| match component {
            Component::RootDir | Component::CurDir => None,
            Component::Normal(name) => Some(Ok(name)),
            Component::ParentDir | Component::Prefix(_) => Some(Err(
                SecureStateError::InvalidStorage("invalid state directory path"),
            )),
        })
        .collect()
}

pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

fn open_or_create_directory_at(
    parent: &File,
    name: &OsStr,
    private: bool,
) -> Result<File, SecureStateError> {
    open_or_create_directory_at_with_policy(parent, name, private, true, || {})
}

fn open_or_create_directory_at_strict(
    parent: &File,
    name: &OsStr,
) -> Result<File, SecureStateError> {
    open_or_create_directory_at_with_policy(parent, name, true, false, || {})
}

#[cfg(test)]
pub(super) fn open_or_create_directory_at_with_hook(
    parent: &File,
    name: &OsStr,
    private: bool,
    after_created_metadata: impl FnOnce(),
) -> Result<File, SecureStateError> {
    open_or_create_directory_at_with_policy(parent, name, private, true, after_created_metadata)
}

fn open_or_create_directory_at_with_policy(
    parent: &File,
    name: &OsStr,
    private: bool,
    repair_existing: bool,
    after_created_metadata: impl FnOnce(),
) -> Result<File, SecureStateError> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| SecureStateError::InvalidStorage("invalid directory name"))?;
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
            return Err(SecureStateError::InvalidStorage(
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
        return Err(SecureStateError::InvalidStorage("open directory component"));
    }
    let child = unsafe { File::from_raw_fd(descriptor) };
    let opened_metadata = child.metadata()?;
    let opened = SecureEntryMetadata::from(&opened_metadata);
    if before.dev != opened.dev
        || before.ino != opened.ino
        || !opened_metadata.file_type().is_dir()
        || (private && opened.uid != unsafe { libc::geteuid() })
    {
        return Err(SecureStateError::InvalidStorage(
            "directory changed during open",
        ));
    }
    if let Some(created) = created_metadata.as_ref() {
        if created.dev != opened.dev || created.ino != opened.ino {
            return Err(SecureStateError::InvalidStorage(
                "created directory changed before final validation",
            ));
        }
    }
    if private && !created && !repair_existing && opened.mode & 0o777 != 0o700 {
        return Err(SecureStateError::InvalidStorage(
            "private directory mode is not 0700",
        ));
    }
    if private && !created && opened.mode & 0o700 != 0o700 {
        return Err(SecureStateError::InvalidStorage(
            "private directory lacks owner permissions",
        ));
    }
    if private && opened.mode & 0o777 != 0o700 {
        child.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    let after = SecureEntryMetadata::from(&child.metadata()?);
    if private
        && (after.mode & 0o777 != 0o700
            || after.uid != unsafe { libc::geteuid() }
            || after.dev != opened.dev
            || after.ino != opened.ino)
    {
        return Err(SecureStateError::InvalidStorage(
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
    initial: &SecureEntryMetadata,
) -> Result<(), SecureStateError> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            created_directory_platform::OPEN_FLAGS,
        )
    };
    if descriptor < 0 {
        return Err(SecureStateError::InvalidStorage(
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
    initial: &SecureEntryMetadata,
) -> Result<(), SecureStateError> {
    let metadata = directory.metadata()?;
    let opened = SecureEntryMetadata::from(&metadata);
    if !metadata.file_type().is_dir()
        || opened.uid != unsafe { libc::geteuid() }
        || opened.dev != initial.dev
        || opened.ino != initial.ino
    {
        return Err(SecureStateError::InvalidStorage(
            "created directory changed before mode repair",
        ));
    }
    Ok(())
}

fn validate_repaired_directory_descriptor(
    directory: &File,
    initial: &SecureEntryMetadata,
) -> Result<(), SecureStateError> {
    let metadata = directory.metadata()?;
    let repaired = SecureEntryMetadata::from(&metadata);
    if repaired.mode & 0o777 != 0o700
        || !metadata.file_type().is_dir()
        || repaired.uid != unsafe { libc::geteuid() }
        || repaired.dev != initial.dev
        || repaired.ino != initial.ino
    {
        return Err(SecureStateError::InvalidStorage(
            "invalid created directory after mode repair",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SecureEntryMetadata {
    pub(crate) mode: u32,
    pub(crate) uid: u32,
    pub(crate) len: u64,
    pub(crate) nlink: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

impl From<&fs::Metadata> for SecureEntryMetadata {
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
fn metadata_at(directory: &File, name: &CStr) -> io::Result<SecureEntryMetadata> {
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
    Ok(SecureEntryMetadata {
        mode: stat.st_mode as u32,
        uid: stat.st_uid as u32,
        len: stat.st_size as u64,
        nlink: stat.st_nlink as u64,
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    })
}

#[allow(clippy::unnecessary_cast)]
fn validate_regular_metadata(metadata: &SecureEntryMetadata) -> Result<(), SecureStateError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.nlink != 1
    {
        return Err(SecureStateError::InvalidStorage(
            "regular entry owner, type, or links",
        ));
    }
    Ok(())
}

#[allow(clippy::unnecessary_cast)]
fn validate_private_directory_metadata(
    metadata: &SecureEntryMetadata,
) -> Result<(), SecureStateError> {
    if metadata.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || metadata.uid != unsafe { libc::geteuid() }
        || metadata.mode & 0o777 != 0o700
    {
        return Err(SecureStateError::InvalidStorage(
            "private directory owner, type, or mode",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io;

    use super::{PRIVATE_CREATE_MODE, created_directory_platform};

    #[test]
    fn private_create_mode_has_the_c_variadic_promoted_type() {
        let _: libc::c_uint = PRIVATE_CREATE_MODE;
        assert_eq!(PRIVATE_CREATE_MODE, 0o600);
    }

    #[test]
    fn platform_helper_signatures_remain_stable() {
        let _: libc::c_int = created_directory_platform::OPEN_FLAGS;
        let _: fn(&File) -> io::Result<()> = created_directory_platform::chmod;
    }

    #[test]
    fn directory_enumeration_uses_portable_error_reporting() {
        let source = include_str!("secure_state.rs");
        let start = source.find("fn directory_entry_names").unwrap();
        let end = source[start..]
            .find("pub(super) fn open_or_create_nested")
            .map(|offset| start + offset)
            .unwrap();
        let enumeration = &source[start..end];
        assert!(enumeration.contains(concat!("libc::", "readdir")));
        assert!(enumeration.contains(concat!("errno::", "set_errno")));
        assert!(enumeration.contains(concat!("errno::", "errno")));
        assert!(!enumeration.contains(concat!("libc::", "readdir_r")));
        for target_specific_errno in [
            concat!("__errno", "_location"),
            concat!("libc::", "__errno"),
            concat!("libc::", "___errno"),
            concat!("libc::", "__error"),
        ] {
            assert!(!enumeration.contains(target_specific_errno));
        }
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
            assert!(!occurrences.is_empty());
            assert!(
                occurrences
                    .iter()
                    .all(|(index, _)| (linux_start..apple_start).contains(index))
            );
        }
        let apple_repair = &source[apple_start..source.find("#[cfg(not(any(").unwrap()];
        assert!(apple_repair.contains(concat!("libc::", "O_SEARCH")));
        assert!(apple_repair.contains(concat!("libc::", "fchmod")));
    }
}
