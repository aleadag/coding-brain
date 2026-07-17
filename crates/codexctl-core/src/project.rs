use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::CodingBrainPaths;

pub const PROJECT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ProjectId {
    Stable(String),
    Temporary(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub project_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    id: ProjectId,
}

#[derive(Debug)]
pub enum ProjectError {
    Io(io::Error),
    InvalidManifest(toml::de::Error),
    UnsupportedSchema(u32),
    InvalidProjectId(uuid::Error),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "project identity I/O failed: {error}"),
            Self::InvalidManifest(error) => write!(formatter, "invalid project manifest: {error}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported project manifest schema {version}")
            }
            Self::InvalidProjectId(error) => write!(formatter, "invalid project UUID: {error}"),
        }
    }
}

impl std::error::Error for ProjectError {}

impl From<io::Error> for ProjectError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl ProjectIdentity {
    pub fn load(cwd: &Path, paths: &CodingBrainPaths) -> Result<Self, ProjectError> {
        let manifest_path = manifest_path(cwd, paths);
        match fs::read_to_string(&manifest_path) {
            Ok(contents) => ProjectManifest::parse(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let canonical = fs::canonicalize(cwd)?;
                Ok(Self {
                    id: ProjectId::Temporary(temporary_id(&canonical)),
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn id(&self) -> &ProjectId {
        &self.id
    }

    pub fn is_durable(&self) -> bool {
        matches!(self.id, ProjectId::Stable(_))
    }
}

impl ProjectManifest {
    pub fn create(cwd: &Path, paths: &CodingBrainPaths) -> Result<ProjectIdentity, ProjectError> {
        let project_dir = paths.project_dir(cwd);
        fs::create_dir_all(&project_dir)?;
        set_directory_mode(&project_dir)?;
        let destination = manifest_path(cwd, paths);
        match fs::read_to_string(&destination) {
            Ok(contents) => return Self::parse(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let project_id = uuid::Uuid::new_v4().to_string();
        let manifest = Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            project_id: project_id.clone(),
        };
        let contents = toml::to_string(&manifest).map_err(|error| {
            ProjectError::Io(io::Error::other(format!(
                "failed to serialize project manifest: {error}"
            )))
        })?;

        let mut temporary = tempfile::NamedTempFile::new_in(&project_dir)?;
        set_file_mode(temporary.as_file())?;
        temporary.write_all(contents.as_bytes())?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => sync_directory(&project_dir)?,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                return ProjectIdentity::load(cwd, paths);
            }
            Err(error) => return Err(ProjectError::Io(error.error)),
        }

        Ok(ProjectIdentity {
            id: ProjectId::Stable(project_id),
        })
    }

    fn parse(contents: &str) -> Result<ProjectIdentity, ProjectError> {
        let manifest: Self = toml::from_str(contents).map_err(ProjectError::InvalidManifest)?;
        if manifest.schema_version != PROJECT_SCHEMA_VERSION {
            return Err(ProjectError::UnsupportedSchema(manifest.schema_version));
        }
        let project_id = uuid::Uuid::parse_str(&manifest.project_id)
            .map_err(ProjectError::InvalidProjectId)?
            .to_string();
        Ok(ProjectIdentity {
            id: ProjectId::Stable(project_id),
        })
    }
}

fn manifest_path(cwd: &Path, paths: &CodingBrainPaths) -> PathBuf {
    paths.project_dir(cwd).join("project.toml")
}

fn temporary_id(path: &Path) -> String {
    // A compact stable hash is sufficient here: temporary IDs are explicitly
    // machine-local and are never promoted to durable project identity.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("temporary-{hash:016x}")
}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_file_mode(file: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_file_mode(_file: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::PathEnvironment;
    use std::fs;

    fn fixture_paths(home: &Path) -> CodingBrainPaths {
        CodingBrainPaths::resolve(&PathEnvironment::new(None, None, Some(home.to_path_buf())))
            .unwrap()
    }

    fn copy_manifest(from: &Path, to: &Path) {
        fs::create_dir_all(to.join(".coding-brain")).unwrap();
        fs::copy(
            from.join(".coding-brain/project.toml"),
            to.join(".coding-brain/project.toml"),
        )
        .unwrap();
    }

    #[test]
    fn missing_manifest_is_temporary_and_cannot_enable_durable_memory() {
        let dir = tempfile::tempdir().unwrap();
        let identity = ProjectIdentity::load(dir.path(), &fixture_paths(dir.path())).unwrap();
        assert!(matches!(identity.id(), ProjectId::Temporary(_)));
        assert!(!identity.is_durable());
    }

    #[test]
    fn tracked_manifest_keeps_identity_across_checkout_paths() {
        let first = tempfile::tempdir().unwrap();
        let created = ProjectManifest::create(first.path(), &fixture_paths(first.path())).unwrap();
        let second = tempfile::tempdir().unwrap();
        copy_manifest(first.path(), second.path());
        let loaded = ProjectIdentity::load(second.path(), &fixture_paths(second.path())).unwrap();
        assert_eq!(created.id(), loaded.id());
    }

    #[test]
    fn copied_manifest_is_authoritative_until_user_resets_it() {
        let original = tempfile::tempdir().unwrap();
        let original_identity =
            ProjectManifest::create(original.path(), &fixture_paths(original.path())).unwrap();
        let fork = tempfile::tempdir().unwrap();
        copy_manifest(original.path(), fork.path());
        assert_eq!(
            original_identity.id(),
            ProjectIdentity::load(fork.path(), &fixture_paths(fork.path()))
                .unwrap()
                .id()
        );

        fs::remove_file(fork.path().join(".coding-brain/project.toml")).unwrap();
        assert!(matches!(
            ProjectIdentity::load(fork.path(), &fixture_paths(fork.path()))
                .unwrap()
                .id(),
            ProjectId::Temporary(_)
        ));
    }

    #[test]
    fn same_named_repositories_without_manifests_have_different_temporary_ids() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first/repo");
        let second = root.path().join("second/repo");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let paths = fixture_paths(root.path());
        let first_id = ProjectIdentity::load(&first, &paths).unwrap();
        let second_id = ProjectIdentity::load(&second, &paths).unwrap();
        assert_ne!(first_id.id(), second_id.id());
    }

    #[test]
    fn rejects_unsupported_schema_and_malformed_uuid() {
        let root = tempfile::tempdir().unwrap();
        let project_dir = root.path().join(".coding-brain");
        fs::create_dir_all(&project_dir).unwrap();
        let manifest = project_dir.join("project.toml");
        fs::write(
            &manifest,
            "schema_version = 2\nproject_id = \"not-a-uuid\"\n",
        )
        .unwrap();
        assert!(ProjectIdentity::load(root.path(), &fixture_paths(root.path())).is_err());
    }

    #[test]
    fn manifest_creation_writes_schema_and_uuid() {
        let root = tempfile::tempdir().unwrap();
        let identity = ProjectManifest::create(root.path(), &fixture_paths(root.path())).unwrap();
        let text = fs::read_to_string(root.path().join(".coding-brain/project.toml")).unwrap();
        let manifest: ProjectManifest = toml::from_str(&text).unwrap();
        assert_eq!(manifest.schema_version, PROJECT_SCHEMA_VERSION);
        assert!(uuid::Uuid::parse_str(&manifest.project_id).is_ok());
        assert_eq!(identity.id(), &ProjectId::Stable(manifest.project_id));
    }

    #[test]
    fn repeated_creation_preserves_an_existing_valid_identity() {
        let root = tempfile::tempdir().unwrap();
        let paths = fixture_paths(root.path());
        let first = ProjectManifest::create(root.path(), &paths).unwrap();
        let second = ProjectManifest::create(root.path(), &paths).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn concurrent_creation_returns_the_single_persisted_identity() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_path_buf();
        let paths = fixture_paths(root.path());
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let cwd = cwd.clone();
                let paths = paths.clone();
                thread::spawn(move || {
                    barrier.wait();
                    ProjectManifest::create(&cwd, &paths).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let identities = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|identity| identity == &identities[0]));
        assert_eq!(ProjectIdentity::load(&cwd, &paths).unwrap(), identities[0]);
    }
}
