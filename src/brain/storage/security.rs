use sha2::{Digest, Sha256};
use std::ffi::{CStr, CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt as UnixFileExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use super::super::secure_state::state_root_for_traversal;

#[cfg(test)]
#[path = "security/diagnostics.rs"]
mod diagnostics;

#[cfg(test)]
pub(crate) use diagnostics::SidecarDiagnosticGuard;

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
    state_root_descriptor: File,
    state_root_path: PathBuf,
    descriptor: File,
    path: PathBuf,
}

pub(super) struct PrivateFileSnapshot {
    pub(super) bytes: Vec<u8>,
    pub(super) identity: PrivateFileIdentity,
}

#[derive(Clone, Copy)]
pub(super) struct PrivateFileIdentity(EntryMetadata);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ClosedDatabaseIdentity {
    pub(super) device: u64,
    pub(super) inode: u64,
    pub(super) size: u64,
    pub(super) modified_seconds: i64,
    pub(super) modified_nanoseconds: i64,
    pub(super) digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PublicationPresence {
    Neither,
    Staging,
    Canonical,
    LinkedPair,
}

impl SecureDatabaseDirectory {
    pub(super) fn try_clone(&self) -> Result<Self, SecurityError> {
        Ok(Self {
            state_root_descriptor: self.state_root_descriptor.try_clone()?,
            state_root_path: self.state_root_path.clone(),
            descriptor: self.descriptor.try_clone()?,
            path: self.path.clone(),
        })
    }

    pub(super) fn prepare(state_root: &Path, create: bool) -> Result<Self, SecurityError> {
        validate_or_create_state_root(state_root, create)?;
        let state_root_path = state_root_for_traversal(state_root).into_owned();
        let state_root_descriptor = open_existing_directory(&state_root_path)?;
        if create {
            open_or_create_directory_at(&state_root_descriptor, OsStr::new("db"), true)?;
        }
        let descriptor = open_directory_at(&state_root_descriptor, OsStr::new("db"))?;
        validate_private_directory(&descriptor)?;
        validate_local_filesystem(&descriptor)?;
        let path = state_root_path.join("db");
        Ok(Self {
            state_root_descriptor,
            state_root_path,
            descriptor,
            path,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn state_root_descriptor(&self) -> &File {
        &self.state_root_descriptor
    }

    pub(super) fn open_state_root_lock(&self) -> Result<File, SecurityError> {
        let current = open_existing_directory(&self.state_root_path)?;
        let retained = EntryMetadata::from(&self.state_root_descriptor.metadata()?);
        let reopened = EntryMetadata::from(&current.metadata()?);
        if retained.dev != reopened.dev || retained.ino != reopened.ino {
            return Err(SecurityError::Invalid(
                "database state root changed after anchor acquisition",
            ));
        }
        Ok(current)
    }

    pub(super) fn validate_path_correspondence(&self) -> Result<(), SecurityError> {
        let current_state_root = open_existing_directory(&self.state_root_path)?;
        let retained_state_root = EntryMetadata::from(&self.state_root_descriptor.metadata()?);
        let reopened_state_root = EntryMetadata::from(&current_state_root.metadata()?);
        if retained_state_root.dev != reopened_state_root.dev
            || retained_state_root.ino != reopened_state_root.ino
        {
            return Err(SecurityError::Invalid(
                "database state root changed after anchor acquisition",
            ));
        }
        let current_database_directory =
            open_directory_at(&self.state_root_descriptor, OsStr::new("db"))?;
        let retained_database_directory = EntryMetadata::from(&self.descriptor.metadata()?);
        let reopened_database_directory =
            EntryMetadata::from(&current_database_directory.metadata()?);
        if retained_database_directory.dev != reopened_database_directory.dev
            || retained_database_directory.ino != reopened_database_directory.ino
        {
            return Err(SecurityError::Invalid(
                "database directory changed after anchor acquisition",
            ));
        }
        Ok(())
    }

    pub(super) fn reject_untrusted_entries(
        &self,
        database_name: &CStr,
        database_must_exist: bool,
    ) -> Result<(), SecurityError> {
        if database_must_exist {
            self.validate_existing_sidecars(database_name)?;
        } else {
            self.reject_sidecars(database_name)?;
        }
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

    pub(super) fn open_lock_file(&self, name: &CStr, create: bool) -> Result<File, SecurityError> {
        let (descriptor, created) = if create {
            let descriptor = unsafe {
                libc::openat(
                    self.descriptor.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if descriptor >= 0 {
                (descriptor, true)
            } else {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
                (open_regular_at(&self.descriptor, name)?, false)
            }
        } else {
            (open_regular_at(&self.descriptor, name)?, false)
        };
        let file = unsafe { File::from_raw_fd(descriptor) };
        if created && unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if at_path.dev != opened.dev || at_path.ino != opened.ino {
            return Err(SecurityError::Invalid("database lock changed during open"));
        }
        Ok(file)
    }

    pub(super) fn validate_lock_file(&self, name: &CStr, file: &File) -> Result<(), SecurityError> {
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if at_path.dev != opened.dev || at_path.ino != opened.ino {
            return Err(SecurityError::Invalid(
                "database lock changed after locking",
            ));
        }
        Ok(())
    }

    pub(super) fn sync_lock_file(&self, name: &CStr, file: &File) -> Result<(), SecurityError> {
        self.validate_lock_file(name, file)?;
        self.validate_path_correspondence()?;
        file.sync_all()?;
        self.descriptor.sync_all()?;
        self.validate_lock_file(name, file)?;
        self.validate_path_correspondence()?;
        Ok(())
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

    pub(super) fn private_file_len(&self, name: &CStr) -> Result<Option<u64>, SecurityError> {
        let descriptor = match open_readonly_regular_at(&self.descriptor, name) {
            Ok(descriptor) => descriptor,
            Err(SecurityError::Missing) => return Ok(None),
            Err(error) => return Err(error),
        };
        let file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if opened.dev != at_path.dev || opened.ino != at_path.ino {
            return Err(SecurityError::Invalid(
                "private database file changed during inspection",
            ));
        }
        Ok(Some(opened.size))
    }

    pub(super) fn remove_database(&self, database_name: &CStr) -> Result<(), SecurityError> {
        self.validate_existing_sidecars(database_name)?;
        let database_exists = match metadata_at(&self.descriptor, database_name) {
            Ok(metadata) => {
                validate_private_file(&metadata)?;
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !database_exists {
            let database_name = OsStr::from_bytes(database_name.to_bytes());
            let mut sidecar_prefix = database_name.as_bytes().to_vec();
            sidecar_prefix.push(b'-');
            if fs::read_dir(&self.path)?.any(|entry| {
                entry.is_ok_and(|entry| entry.file_name().as_bytes().starts_with(&sidecar_prefix))
            }) {
                return Err(SecurityError::Invalid(
                    "SQLite sidecar exists without its database",
                ));
            }
            return Ok(());
        }

        for suffix in ["-shm", "-wal", "-journal"] {
            let name = sidecar_name(database_name, suffix)?;
            unlink_if_present(&self.descriptor, &name)?;
        }
        unlink_if_present(&self.descriptor, database_name)?;
        self.descriptor.sync_all()?;
        Ok(())
    }

    pub(super) fn publish_database(
        &self,
        staging_name: &CStr,
        canonical_name: &CStr,
    ) -> Result<(), SecurityError> {
        self.validate_path_correspondence()?;
        self.reject_untrusted_entries(staging_name, true)?;
        self.reject_untrusted_entries(canonical_name, false)?;
        let linked = unsafe {
            libc::linkat(
                self.descriptor.as_raw_fd(),
                staging_name.as_ptr(),
                self.descriptor.as_raw_fd(),
                canonical_name.as_ptr(),
                0,
            )
        };
        if linked != 0 {
            return Err(io::Error::last_os_error().into());
        }
        #[cfg(feature = "fault-injection")]
        if canonical_name == super::BRAIN_DATABASE_NAME {
            super::migration::migration_fault("after-brain-link");
        } else if canonical_name == super::REVIEW_DATABASE_NAME {
            super::migration::migration_fault("after-review-link");
        }
        if unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), staging_name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.descriptor.sync_all()?;
        self.validate_after_open(canonical_name)?;
        self.validate_path_correspondence()?;
        Ok(())
    }

    pub(super) fn publication_presence(
        &self,
        staging_name: &CStr,
        canonical_name: &CStr,
    ) -> Result<PublicationPresence, SecurityError> {
        let staging = optional_metadata_at(&self.descriptor, staging_name)?;
        let canonical = optional_metadata_at(&self.descriptor, canonical_name)?;
        match (staging, canonical) {
            (None, None) => Ok(PublicationPresence::Neither),
            (Some(staging), None) => {
                validate_private_file(&staging)?;
                Ok(PublicationPresence::Staging)
            }
            (None, Some(canonical)) => {
                validate_private_file(&canonical)?;
                Ok(PublicationPresence::Canonical)
            }
            (Some(staging), Some(canonical)) => {
                validate_private_linked_pair(&staging, &canonical)?;
                Ok(PublicationPresence::LinkedPair)
            }
        }
    }

    pub(super) fn private_file_present(&self, name: &CStr) -> Result<bool, SecurityError> {
        let Some(metadata) = optional_metadata_at(&self.descriptor, name)? else {
            return Ok(false);
        };
        validate_private_file(&metadata)?;
        Ok(true)
    }

    pub(super) fn private_file_device_inode(
        &self,
        name: &CStr,
    ) -> Result<(u64, u64), SecurityError> {
        let descriptor = open_readonly_regular_at(&self.descriptor, name)?;
        let file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if opened != at_path {
            return Err(SecurityError::Invalid(
                "private file changed during identity capture",
            ));
        }
        self.validate_path_correspondence()?;
        Ok((opened.dev, opened.ino))
    }

    pub(super) fn finish_linked_publication(
        &self,
        staging_name: &CStr,
        canonical_name: &CStr,
    ) -> Result<(), SecurityError> {
        let staging = metadata_at(&self.descriptor, staging_name)?;
        let canonical = metadata_at(&self.descriptor, canonical_name)?;
        validate_private_linked_pair(&staging, &canonical)?;
        for name in [staging_name, canonical_name] {
            for suffix in ["-wal", "-shm", "-journal"] {
                if optional_metadata_at(&self.descriptor, &sidecar_name(name, suffix)?)?.is_some() {
                    return Err(SecurityError::Invalid(
                        "partial publication has a SQLite sidecar",
                    ));
                }
            }
        }
        if unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), staging_name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.descriptor.sync_all()?;
        self.validate_database_without_sidecars(canonical_name)?;
        self.validate_path_correspondence()?;
        Ok(())
    }

    pub(super) fn validate_database_without_sidecars(
        &self,
        database_name: &CStr,
    ) -> Result<(), SecurityError> {
        self.reject_untrusted_entries(database_name, true)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let name = sidecar_name(database_name, suffix)?;
            match metadata_at(&self.descriptor, &name) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_metadata) => {
                    #[cfg(test)]
                    diagnostics::capture_sidecar_rejection(self, database_name, &name, _metadata);
                    return Err(SecurityError::Invalid("staging SQLite sidecar remains"));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn sync_database(&self, database_name: &CStr) -> Result<(), SecurityError> {
        let descriptor = open_regular_at(&self.descriptor, database_name)?;
        let file = unsafe { File::from_raw_fd(descriptor) };
        file.sync_all()?;
        self.descriptor.sync_all()?;
        Ok(())
    }

    pub(super) fn database_journal_mode(
        &self,
        database_name: &CStr,
    ) -> Result<&'static str, SecurityError> {
        self.validate_database_without_sidecars(database_name)?;
        let descriptor = open_readonly_regular_at(&self.descriptor, database_name)?;
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, database_name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if opened.dev != at_path.dev || opened.ino != at_path.ino {
            return Err(SecurityError::Invalid(
                "database changed during journal-mode inspection",
            ));
        }
        let mut header = [0_u8; 20];
        file.read_exact(&mut header)?;
        let after = EntryMetadata::from(&file.metadata()?);
        if opened.dev != after.dev
            || opened.ino != after.ino
            || opened.size != after.size
            || opened.modified_seconds != after.modified_seconds
            || opened.modified_nanoseconds != after.modified_nanoseconds
            || &header[..16] != b"SQLite format 3\0"
        {
            return Err(SecurityError::Invalid(
                "database changed during journal-mode inspection",
            ));
        }
        self.validate_path_correspondence()?;
        match (header[18], header[19]) {
            (1, 1) => Ok("delete"),
            (2, 2) => Ok("wal"),
            _ => Err(SecurityError::Invalid(
                "database has an unsupported journal mode",
            )),
        }
    }

    pub(super) fn normalize_database_header_to_wal(
        &self,
        database_name: &CStr,
    ) -> Result<(), SecurityError> {
        self.validate_database_without_sidecars(database_name)?;
        let descriptor = open_regular_at(&self.descriptor, database_name)?;
        let file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryMetadata::from(&file.metadata()?);
        self.normalize_database_header_to_wal_file(database_name, &file, opened)
    }

    pub(super) fn copy_database_to_bound_file(
        &self,
        source_name: &CStr,
        temporary_name: &CStr,
        temporary_device: u64,
        temporary_inode: u64,
    ) -> Result<(), SecurityError> {
        self.validate_database_without_sidecars(source_name)?;
        let source_descriptor = open_readonly_regular_at(&self.descriptor, source_name)?;
        let mut source = unsafe { File::from_raw_fd(source_descriptor) };
        let source_before = EntryMetadata::from(&source.metadata()?);
        let source_path = metadata_at(&self.descriptor, source_name)?;
        validate_private_file(&source_before)?;
        if source_before != source_path {
            return Err(SecurityError::Invalid(
                "database changed before normalization copy",
            ));
        }

        let temporary_descriptor = open_regular_at(&self.descriptor, temporary_name)?;
        let mut temporary = unsafe { File::from_raw_fd(temporary_descriptor) };
        let temporary_before = EntryMetadata::from(&temporary.metadata()?);
        let temporary_path = metadata_at(&self.descriptor, temporary_name)?;
        validate_private_file(&temporary_before)?;
        if temporary_before != temporary_path
            || temporary_before.dev != temporary_device
            || temporary_before.ino != temporary_inode
        {
            return Err(SecurityError::Invalid(
                "normalization temporary changed before copy",
            ));
        }

        temporary.set_len(0)?;
        io::copy(&mut source, &mut temporary)?;
        temporary.sync_all()?;
        self.descriptor.sync_all()?;
        let source_after = EntryMetadata::from(&source.metadata()?);
        let source_after_path = metadata_at(&self.descriptor, source_name)?;
        let temporary_after = EntryMetadata::from(&temporary.metadata()?);
        let temporary_after_path = metadata_at(&self.descriptor, temporary_name)?;
        if source_after != source_before
            || source_after_path != source_before
            || temporary_after != temporary_after_path
            || temporary_after.dev != temporary_device
            || temporary_after.ino != temporary_inode
        {
            return Err(SecurityError::Invalid(
                "database changed during normalization copy",
            ));
        }
        self.validate_path_correspondence()?;
        Ok(())
    }

    pub(super) fn publish_database_replacement(
        &self,
        canonical_name: &CStr,
        expected: &ClosedDatabaseIdentity,
        temporary_name: &CStr,
        replacement: &ClosedDatabaseIdentity,
    ) -> Result<(), SecurityError> {
        self.publish_database_replacement_with_hook(
            canonical_name,
            expected,
            temporary_name,
            replacement,
            || {},
        )
    }

    fn publish_database_replacement_with_hook(
        &self,
        canonical_name: &CStr,
        expected: &ClosedDatabaseIdentity,
        temporary_name: &CStr,
        replacement: &ClosedDatabaseIdentity,
        before_exchange: impl FnOnce(),
    ) -> Result<(), SecurityError> {
        if &self.closed_database_identity(canonical_name)? != expected {
            return Err(SecurityError::Invalid(
                "database changed before normalized replacement",
            ));
        }
        if &self.closed_database_identity(temporary_name)? != replacement {
            return Err(SecurityError::Invalid(
                "normalization temporary changed before publish",
            ));
        }
        before_exchange();
        exchange_files_at(&self.descriptor, temporary_name, canonical_name)?;
        self.descriptor.sync_all()?;
        let published = self.closed_database_identity(canonical_name)?;
        let displaced = self.closed_database_identity(temporary_name)?;
        if &published != replacement || &displaced != expected {
            exchange_files_at(&self.descriptor, temporary_name, canonical_name)?;
            self.descriptor.sync_all()?;
            if self.closed_database_identity(canonical_name)? != displaced
                || self.closed_database_identity(temporary_name)? != published
            {
                return Err(SecurityError::Invalid(
                    "normalized database replacement could not be restored",
                ));
            }
            return Err(SecurityError::Invalid(
                "database changed during normalized replacement",
            ));
        }
        self.validate_path_correspondence()?;
        Ok(())
    }

    pub(super) fn remove_closed_database_file(
        &self,
        name: &CStr,
        expected: &ClosedDatabaseIdentity,
    ) -> Result<(), SecurityError> {
        if &self.closed_database_identity(name)? != expected {
            return Err(SecurityError::Invalid(
                "database changed before exact removal",
            ));
        }
        if unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.descriptor.sync_all()?;
        self.validate_path_correspondence()?;
        Ok(())
    }

    fn normalize_database_header_to_wal_file(
        &self,
        database_name: &CStr,
        file: &File,
        opened: EntryMetadata,
    ) -> Result<(), SecurityError> {
        let at_path = metadata_at(&self.descriptor, database_name)?;
        validate_private_file(&opened)?;
        validate_private_file(&at_path)?;
        if opened != at_path {
            return Err(SecurityError::Invalid(
                "database changed before WAL normalization",
            ));
        }
        let mut header = [0_u8; 20];
        UnixFileExt::read_exact_at(file, &mut header, 0)?;
        if &header[..16] != b"SQLite format 3\0" {
            return Err(SecurityError::Invalid(
                "database header is invalid before WAL normalization",
            ));
        }
        match (header[18], header[19]) {
            (1, 1) => UnixFileExt::write_all_at(file, &[2, 2], 18)?,
            (2, 2) => {}
            _ => {
                return Err(SecurityError::Invalid(
                    "database has an unsupported journal mode",
                ));
            }
        }
        file.sync_all()?;
        self.descriptor.sync_all()?;

        let after = EntryMetadata::from(&file.metadata()?);
        let after_path = metadata_at(&self.descriptor, database_name)?;
        let mut normalized = [0_u8; 20];
        UnixFileExt::read_exact_at(file, &mut normalized, 0)?;
        if after.dev != opened.dev
            || after.ino != opened.ino
            || after.size != opened.size
            || after_path != after
            || &normalized[..16] != b"SQLite format 3\0"
            || normalized[18..20] != [2, 2]
        {
            return Err(SecurityError::Invalid(
                "database changed during WAL normalization",
            ));
        }
        self.validate_path_correspondence()?;
        Ok(())
    }

    pub(super) fn closed_database_identity(
        &self,
        database_name: &CStr,
    ) -> Result<ClosedDatabaseIdentity, SecurityError> {
        self.closed_database_identity_with_links(database_name, 1)
    }

    #[allow(clippy::unnecessary_cast)] // libc stat fields vary in width across Unix targets.
    pub(super) fn closed_database_identity_with_links(
        &self,
        database_name: &CStr,
        expected_links: u64,
    ) -> Result<ClosedDatabaseIdentity, SecurityError> {
        if expected_links == 1 {
            self.validate_database_without_sidecars(database_name)?;
        }
        let descriptor = open_readonly_regular_at(&self.descriptor, database_name)?;
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let opened = EntryMetadata::from(&file.metadata()?);
        let at_path = metadata_at(&self.descriptor, database_name)?;
        let valid = |metadata: &EntryMetadata| {
            metadata.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
                && metadata.uid == unsafe { libc::geteuid() }
                && metadata.mode & 0o777 == 0o600
                && metadata.nlink == expected_links
        };
        if !valid(&opened) || !valid(&at_path) {
            return Err(SecurityError::Invalid(
                "closed database link identity is invalid",
            ));
        }
        if opened != at_path {
            return Err(SecurityError::Invalid(
                "closed database changed during identity capture",
            ));
        }
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 32 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        let after = EntryMetadata::from(&file.metadata()?);
        let after_path = metadata_at(&self.descriptor, database_name)?;
        if after != opened || after_path != opened {
            return Err(SecurityError::Invalid(
                "closed database changed during identity capture",
            ));
        }
        Ok(ClosedDatabaseIdentity {
            device: opened.dev,
            inode: opened.ino,
            size: opened.size,
            modified_seconds: opened.modified_seconds,
            modified_nanoseconds: opened.modified_nanoseconds,
            digest: format!("{:x}", digest.finalize()),
        })
    }

    pub(super) fn read_private_file(
        &self,
        name: &CStr,
        maximum: usize,
    ) -> Result<PrivateFileSnapshot, SecurityError> {
        let descriptor = open_readonly_regular_at(&self.descriptor, name)?;
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        let before_opened = EntryMetadata::from(&file.metadata()?);
        let before_path = metadata_at(&self.descriptor, name)?;
        validate_private_file(&before_opened)?;
        validate_private_file(&before_path)?;
        if before_opened != before_path || before_opened.size > maximum as u64 {
            return Err(SecurityError::Invalid(
                "private state file changed during open",
            ));
        }
        let mut bytes = Vec::with_capacity(before_opened.size as usize);
        (&mut file)
            .take((maximum + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(SecurityError::Invalid(
                "private state file exceeds its size limit",
            ));
        }
        let after_opened = EntryMetadata::from(&file.metadata()?);
        let after_path = metadata_at(&self.descriptor, name)?;
        if before_opened != after_opened || before_opened != after_path {
            return Err(SecurityError::Invalid(
                "private state file changed during read",
            ));
        }
        self.validate_path_correspondence()?;
        Ok(PrivateFileSnapshot {
            bytes,
            identity: PrivateFileIdentity(before_opened),
        })
    }

    pub(super) fn write_new_private_file(
        &self,
        name: &CStr,
        bytes: &[u8],
    ) -> Result<PrivateFileIdentity, SecurityError> {
        let mut file = self.create_database_file(name)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.descriptor.sync_all()?;
        self.validate_path_correspondence()?;
        Ok(PrivateFileIdentity(EntryMetadata::from(&file.metadata()?)))
    }

    pub(super) fn publish_private_replacement(
        &self,
        name: &CStr,
        expected: PrivateFileIdentity,
        temporary_name: &CStr,
        replacement: PrivateFileIdentity,
    ) -> Result<PrivateFileIdentity, SecurityError> {
        let at_path = metadata_at(&self.descriptor, name)?;
        if at_path != expected.0 {
            return Err(SecurityError::Invalid(
                "private state file changed before replacement",
            ));
        }
        let temporary = metadata_at(&self.descriptor, temporary_name)?;
        if temporary != replacement.0 {
            return Err(SecurityError::Invalid(
                "private state replacement changed before publish",
            ));
        }
        if unsafe {
            libc::renameat(
                self.descriptor.as_raw_fd(),
                temporary_name.as_ptr(),
                self.descriptor.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        self.descriptor.sync_all()?;
        let published = metadata_at(&self.descriptor, name)?;
        if published != replacement.0 {
            return Err(SecurityError::Invalid(
                "private state replacement changed during publish",
            ));
        }
        self.validate_path_correspondence()?;
        Ok(replacement)
    }

    pub(super) fn remove_private_file(
        &self,
        name: &CStr,
        expected: PrivateFileIdentity,
    ) -> Result<(), SecurityError> {
        let at_path = metadata_at(&self.descriptor, name)?;
        if at_path != expected.0 {
            return Err(SecurityError::Invalid(
                "private state file changed before removal",
            ));
        }
        if unsafe { libc::unlinkat(self.descriptor.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.descriptor.sync_all()?;
        self.validate_path_correspondence()?;
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

    fn validate_existing_sidecars(&self, database_name: &CStr) -> Result<(), SecurityError> {
        let mut sidecar_prefix = database_name.to_bytes().to_vec();
        sidecar_prefix.push(b'-');
        for entry in fs::read_dir(&self.path)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let file_name_bytes = file_name.as_bytes();
            if !file_name_bytes.starts_with(&sidecar_prefix) {
                continue;
            }
            let suffix = &file_name_bytes[sidecar_prefix.len() - 1..];
            if !matches!(suffix, b"-wal" | b"-shm" | b"-journal") {
                return Err(SecurityError::Invalid("unknown SQLite sidecar"));
            }
            let name = CString::new(file_name_bytes)
                .map_err(|_| SecurityError::Invalid("invalid SQLite sidecar name"))?;
            match metadata_at(&self.descriptor, &name) {
                Ok(metadata) => validate_private_file(&metadata)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

fn open_regular_at(directory: &File, name: &CStr) -> Result<libc::c_int, SecurityError> {
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor >= 0 {
        Ok(descriptor)
    } else {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
            Err(SecurityError::Invalid("database lock is a symlink"))
        } else {
            Err(error.into())
        }
    }
}

fn open_readonly_regular_at(directory: &File, name: &CStr) -> Result<libc::c_int, SecurityError> {
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor >= 0 {
        Ok(descriptor)
    } else {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ELOOP)) {
            Err(SecurityError::Invalid("private state file is a symlink"))
        } else {
            Err(error.into())
        }
    }
}

#[cfg(feature = "fault-injection")]
pub(super) fn open_fault_capability(path: &Path) -> Result<File, SecurityError> {
    let parent = path
        .parent()
        .ok_or(SecurityError::Invalid("fault capability has no parent"))?;
    let name = path
        .file_name()
        .ok_or(SecurityError::Invalid("fault capability has no name"))?;
    let traversal = state_root_for_traversal(parent);
    let mut directory = open_directory(if traversal.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    })?;
    for component in normal_components(&traversal)? {
        validate_safe_ancestor_metadata(&EntryMetadata::from(&directory.metadata()?))?;
        directory = open_directory_at(&directory, component)?;
    }
    validate_private_directory(&directory)?;
    let c_name = CString::new(name.as_bytes())
        .map_err(|_| SecurityError::Invalid("fault capability name is invalid"))?;
    let before = metadata_at(&directory, &c_name)?;
    validate_private_file(&before)?;
    let descriptor = open_readonly_regular_at(&directory, &c_name)?;
    let file = unsafe { File::from_raw_fd(descriptor) };
    let opened = EntryMetadata::from(&file.metadata()?);
    validate_private_file(&opened)?;
    if before.dev != opened.dev || before.ino != opened.ino {
        return Err(SecurityError::Invalid(
            "fault capability changed during open",
        ));
    }
    Ok(file)
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
    let trusted_owner = metadata.uid == 0 || metadata.uid == unsafe { libc::geteuid() };
    if !trusted_owner {
        return Err(SecurityError::Invalid(
            "state directory ancestor is foreign-owned",
        ));
    }
    if metadata.mode & 0o022 != 0 && metadata.mode & libc::S_ISVTX as u32 == 0 {
        return Err(SecurityError::Invalid(
            "state directory ancestor is replaceable by another user",
        ));
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

#[allow(clippy::unnecessary_cast)] // libc stat fields vary in width across Unix targets.
fn validate_private_linked_pair(
    left: &EntryMetadata,
    right: &EntryMetadata,
) -> Result<(), SecurityError> {
    let owner = unsafe { libc::geteuid() };
    let private_regular = |metadata: &EntryMetadata| {
        metadata.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
            && metadata.uid == owner
            && metadata.mode & 0o777 == 0o600
            && metadata.nlink == 2
    };
    if !private_regular(left)
        || !private_regular(right)
        || left.dev != right.dev
        || left.ino != right.ino
        || left.size != right.size
        || left.modified_seconds != right.modified_seconds
        || left.modified_nanoseconds != right.modified_nanoseconds
    {
        return Err(SecurityError::Invalid(
            "partial publication is not one exact private inode",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct EntryMetadata {
    mode: u32,
    uid: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl From<&fs::Metadata> for EntryMetadata {
    fn from(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            uid: metadata.uid(),
            nlink: metadata.nlink(),
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn exchange_files_at(directory: &File, left: &CStr, right: &CStr) -> io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            left.as_ptr(),
            directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn exchange_files_at(directory: &File, left: &CStr, right: &CStr) -> io::Result<()> {
    let result = unsafe {
        libc::renameatx_np(
            directory.as_raw_fd(),
            left.as_ptr(),
            directory.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn exchange_files_at(_directory: &File, _left: &CStr, _right: &CStr) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic database exchange is unsupported",
    ))
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
        size: stat.st_size as u64,
        modified_seconds: stat.st_mtime,
        modified_nanoseconds: stat.st_mtime_nsec,
    })
}

fn optional_metadata_at(
    directory: &File,
    name: &CStr,
) -> Result<Option<EntryMetadata>, SecurityError> {
    match metadata_at(directory, name) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn unlink_if_present(directory: &File, name: &CStr) -> Result<(), SecurityError> {
    match metadata_at(directory, name) {
        Ok(metadata) => validate_private_file(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
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
    if unsafe { status.assume_init() }.f_flags & libc::MNT_LOCAL as libc::c_uint != 0 {
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
    fn wal_header_normalization_rejects_path_replacement_without_touching_replacement() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = SecureDatabaseDirectory::prepare(root.path(), true).unwrap();
        let name = c"review.sqlite3";
        let mut original = b"SQLite format 3\0".to_vec();
        original.extend_from_slice(&[4, 0, 1, 1]);
        original.resize(512, 0);
        let mut file = directory.create_database_file(name).unwrap();
        file.write_all(&original).unwrap();
        file.sync_all().unwrap();
        let opened = EntryMetadata::from(&file.metadata().unwrap());

        let path = directory.path().join("review.sqlite3");
        fs::rename(&path, directory.path().join("retained-original")).unwrap();
        let replacement = b"replacement-evidence";
        fs::write(&path, replacement).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(
            directory
                .normalize_database_header_to_wal_file(name, &file, opened)
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), replacement);
    }

    #[test]
    fn normalized_exchange_rejects_substituted_canonical_without_changing_either_file() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = SecureDatabaseDirectory::prepare(root.path(), true).unwrap();
        let canonical = c"review.sqlite3";
        let temporary = c".review.sqlite3.normalization-1.tmp";
        let mut original = directory.create_database_file(canonical).unwrap();
        original.write_all(b"original").unwrap();
        original.sync_all().unwrap();
        let expected = directory.closed_database_identity(canonical).unwrap();
        let mut normalized = directory.create_database_file(temporary).unwrap();
        normalized.write_all(b"normalized").unwrap();
        normalized.sync_all().unwrap();
        let replacement = directory.closed_database_identity(temporary).unwrap();

        let temporary_before = directory.closed_database_identity(temporary).unwrap();
        let substituted_identity = std::cell::RefCell::new(None);

        assert!(
            directory
                .publish_database_replacement_with_hook(
                    canonical,
                    &expected,
                    temporary,
                    &replacement,
                    || {
                        let retained = directory.path().join("retained-original");
                        fs::rename(directory.path().join("review.sqlite3"), retained).unwrap();
                        let mut substituted = directory.create_database_file(canonical).unwrap();
                        substituted.write_all(b"substituted").unwrap();
                        substituted.sync_all().unwrap();
                        *substituted_identity.borrow_mut() =
                            Some(directory.closed_database_identity(canonical).unwrap());
                    }
                )
                .is_err()
        );
        assert_eq!(
            directory.closed_database_identity(canonical).unwrap(),
            substituted_identity.into_inner().unwrap()
        );
        assert_eq!(
            directory.closed_database_identity(temporary).unwrap(),
            temporary_before
        );
        assert_eq!(
            fs::read(directory.path().join("review.sqlite3")).unwrap(),
            b"substituted"
        );
        assert_eq!(
            fs::read(directory.path().join(".review.sqlite3.normalization-1.tmp")).unwrap(),
            b"normalized"
        );
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
    fn private_state_root_metadata_rejects_foreign_owner() {
        let metadata = EntryMetadata {
            mode: libc::S_IFDIR as u32 | 0o700,
            uid: unsafe { libc::geteuid() }.wrapping_add(1),
            nlink: 1,
            dev: 1,
            ino: 1,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
        };

        assert!(validate_private_state_root_metadata(&metadata).is_err());
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
    fn private_file_metadata_rejects_foreign_owner() {
        let metadata = EntryMetadata {
            mode: libc::S_IFREG as u32 | 0o600,
            uid: unsafe { libc::geteuid() }.wrapping_add(1),
            nlink: 1,
            dev: 1,
            ino: 1,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
        };

        assert!(validate_private_file(&metadata).is_err());
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
    fn safe_ancestor_metadata_rejects_foreign_owner_even_without_write_bits() {
        let metadata = EntryMetadata {
            mode: libc::S_IFDIR as u32 | 0o755,
            uid: unsafe { libc::geteuid() }.wrapping_add(1),
            nlink: 1,
            dev: 1,
            ino: 1,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
        };

        assert!(validate_safe_ancestor_metadata(&metadata).is_err());
    }

    #[test]
    #[allow(clippy::unnecessary_cast)] // libc mode constants vary in width across Unix targets.
    fn safe_ancestor_metadata_requires_sticky_for_trusted_writable_owner() {
        let mut metadata = EntryMetadata {
            mode: libc::S_IFDIR as u32 | 0o777,
            uid: unsafe { libc::geteuid() },
            nlink: 1,
            dev: 1,
            ino: 1,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
        };

        assert!(validate_safe_ancestor_metadata(&metadata).is_err());
        metadata.mode |= libc::S_ISVTX as u32;
        assert!(validate_safe_ancestor_metadata(&metadata).is_ok());
        metadata.uid = 0;
        assert!(validate_safe_ancestor_metadata(&metadata).is_ok());
    }
}
