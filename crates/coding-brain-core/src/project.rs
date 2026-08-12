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

pub trait ProjectCommandRunner {
    fn output(&mut self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectCommandError {
    Spawn,
    Io,
    Timeout,
    OutputLimit,
    ExitStatus,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectProvenance {
    Manifest,
    NetworkRemote,
    Temporary,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ProjectResolution {
    root: PathBuf,
    identity: ProjectIdentity,
    provenance: ProjectProvenance,
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
    pub fn from_stable_uuid(value: &str) -> Result<Self, uuid::Error> {
        Ok(Self {
            id: ProjectId::Stable(uuid::Uuid::parse_str(value)?.to_string()),
        })
    }

    pub fn from_temporary_root(root: &Path) -> Self {
        Self {
            id: ProjectId::Temporary(temporary_id(root)),
        }
    }

    pub fn load(cwd: &Path, paths: &CodingBrainPaths) -> Result<Self, ProjectError> {
        let mut runner = SystemProjectCommandRunner;
        Self::resolve_with(cwd, paths, &mut runner).map(|resolution| resolution.identity)
    }

    pub fn resolve_with(
        cwd: &Path,
        paths: &CodingBrainPaths,
        runner: &mut impl ProjectCommandRunner,
    ) -> Result<ProjectResolution, ProjectError> {
        let (project_root, root_discovery_failed) = project_root_with(cwd, runner);
        let root = fs::canonicalize(project_root)?;
        let manifest_path = manifest_path(&root, paths);
        match fs::read_to_string(&manifest_path) {
            Ok(contents) => Ok(ProjectResolution {
                root,
                identity: ProjectManifest::parse(&contents)?,
                provenance: ProjectProvenance::Manifest,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if !root_discovery_failed {
                    if let Some(id) = git_remote_project_id_with(&root, runner) {
                        return Ok(ProjectResolution {
                            root,
                            identity: Self { id },
                            provenance: ProjectProvenance::NetworkRemote,
                        });
                    }
                }
                Ok(ProjectResolution {
                    identity: Self {
                        id: ProjectId::Temporary(temporary_id(&root)),
                    },
                    root,
                    provenance: ProjectProvenance::Temporary,
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

impl ProjectResolution {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn identity(&self) -> &ProjectIdentity {
        &self.identity
    }

    pub fn provenance(&self) -> ProjectProvenance {
        self.provenance
    }
}

impl ProjectManifest {
    pub fn create(cwd: &Path, paths: &CodingBrainPaths) -> Result<ProjectIdentity, ProjectError> {
        let root = project_root(cwd);
        let project_dir = paths.project_dir(&root);
        fs::create_dir_all(&project_dir)?;
        set_directory_mode(&project_dir)?;
        let destination = manifest_path(&root, paths);
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
                return ProjectIdentity::load(&root, paths);
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

fn git_root(cwd: &Path) -> Option<PathBuf> {
    let root = git_path_output(cwd, &["rev-parse", "--show-toplevel"])?;
    if root.is_absolute() {
        Some(root)
    } else {
        fs::canonicalize(cwd.join(root)).ok()
    }
}

fn git_root_with(cwd: &Path, runner: &mut impl ProjectCommandRunner) -> (PathBuf, bool) {
    let value = match runner.output(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(value) => value,
        Err(_) => return (cwd.to_path_buf(), true),
    };
    let root = git_path_from_output(value).and_then(|root| {
        if root.is_absolute() {
            Some(root)
        } else {
            fs::canonicalize(cwd.join(root)).ok()
        }
    });
    (root.unwrap_or_else(|| cwd.to_path_buf()), false)
}

fn project_root(cwd: &Path) -> PathBuf {
    git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn project_root_with(cwd: &Path, runner: &mut impl ProjectCommandRunner) -> (PathBuf, bool) {
    git_root_with(cwd, runner)
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

const GIT_REMOTE_NAMESPACE: uuid::Uuid =
    uuid::Uuid::from_u128(0x2c54e35b_775d_4bc5_83df_40d4d2fde58e);

struct SystemProjectCommandRunner;

impl ProjectCommandRunner for SystemProjectCommandRunner {
    fn output(&mut self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|_| ProjectCommandError::Spawn)?;
        if !output.status.success() {
            return Err(ProjectCommandError::ExitStatus);
        }
        Ok(output.stdout)
    }
}

fn git_path_output(cwd: &Path, args: &[&str]) -> Option<PathBuf> {
    let mut runner = SystemProjectCommandRunner;
    git_path_output_with(cwd, args, &mut runner)
}

fn git_path_output_with(
    cwd: &Path,
    args: &[&str],
    runner: &mut impl ProjectCommandRunner,
) -> Option<PathBuf> {
    git_path_from_output(runner.output(cwd, args).ok()?)
}

fn git_path_from_output(mut value: Vec<u8>) -> Option<PathBuf> {
    if value.last() == Some(&b'\n') {
        value.pop();
        #[cfg(windows)]
        if value.last() == Some(&b'\r') {
            value.pop();
        }
    }
    if value.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        Some(PathBuf::from(std::ffi::OsString::from_vec(value)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(value).ok().map(PathBuf::from)
    }
}

fn git_text_output_with(
    cwd: &Path,
    args: &[&str],
    runner: &mut impl ProjectCommandRunner,
) -> Option<String> {
    let value = String::from_utf8(runner.output(cwd, args).ok()?).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn normalize_repo_path(path: &str) -> Option<&str> {
    let path = path.split(['?', '#']).next()?;
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    (!path.is_empty()).then_some(path)
}

fn valid_remote_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

fn valid_remote_authority(host_port: &str) -> bool {
    if host_port.is_empty()
        || host_port
            .chars()
            .any(|character| character.is_whitespace() || matches!(character, '/' | '\\'))
    {
        return false;
    }

    if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        return suffix.is_empty() || suffix.strip_prefix(':').is_some_and(valid_remote_port);
    }

    if host_port.contains('[') || host_port.contains(']') {
        return false;
    }
    let mut parts = host_port.split(':');
    let host = parts.next().unwrap_or_default();
    if host.is_empty() {
        return false;
    }
    match parts.next() {
        None => true,
        Some(port) => parts.next().is_none() && valid_remote_port(port),
    }
}

fn canonical_network_remote(authority: &str, path: &str) -> Option<String> {
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, value)| value);
    if !valid_remote_authority(host_port) {
        return None;
    }
    let path = normalize_repo_path(path)?;
    Some(format!("{}/{path}", host_port.to_ascii_lowercase()))
}

fn canonical_remote(remote: &str) -> Option<String> {
    let remote = remote.trim();
    if remote.is_empty() {
        return None;
    }
    if remote
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"))
    {
        return None;
    }
    if remote
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphabetic)
        && remote.as_bytes().get(1) == Some(&b':')
    {
        return None;
    }
    if let Some((scheme, rest)) = remote.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if !matches!(
            scheme.as_str(),
            "https" | "http" | "ssh" | "git" | "ftp" | "ftps"
        ) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        if authority.contains('?') || authority.contains('#') {
            return None;
        }
        return canonical_network_remote(authority, path);
    }
    if remote.starts_with('[') || remote.contains("@[") {
        return None;
    }
    if let Some((authority, path)) = remote.split_once(':') {
        if !authority.contains('/') {
            return canonical_network_remote(authority, path);
        }
    }
    None
}

fn git_remote_project_id_with(
    git_root: &Path,
    runner: &mut impl ProjectCommandRunner,
) -> Option<ProjectId> {
    let remote = git_text_output_with(git_root, &["remote", "get-url", "origin"], runner)?;
    git_remote_project_id_from_remote(&remote)
}

fn git_remote_project_id_from_remote(remote: &str) -> Option<ProjectId> {
    let canonical = canonical_remote(remote)?;
    let fingerprint = format!("git-remote:v1:{canonical}");
    Some(ProjectId::Stable(
        uuid::Uuid::new_v5(&GIT_REMOTE_NAMESPACE, fingerprint.as_bytes()).to_string(),
    ))
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
    #[cfg(unix)]
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;

    fn fixture_paths(home: &Path) -> CodingBrainPaths {
        CodingBrainPaths::resolve(&PathEnvironment::new(None, None, Some(home.to_path_buf())))
            .unwrap()
    }

    type FixtureResponse = (Vec<String>, Result<Vec<u8>, ProjectCommandError>);

    struct FixtureRunner {
        responses: Vec<FixtureResponse>,
        calls: usize,
    }

    impl FixtureRunner {
        fn new(
            responses: impl IntoIterator<
                Item = (Vec<&'static str>, Result<Vec<u8>, ProjectCommandError>),
            >,
        ) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(args, result)| (args.into_iter().map(str::to_owned).collect(), result))
                    .collect(),
                calls: 0,
            }
        }

        fn calls(&self) -> usize {
            self.calls
        }
    }

    impl ProjectCommandRunner for FixtureRunner {
        fn output(&mut self, _cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError> {
            self.calls += 1;
            let (expected, result) = self
                .responses
                .get_mut(self.calls - 1)
                .unwrap_or_else(|| panic!("unexpected command {args:?}"));
            assert_eq!(expected, args);
            result.clone()
        }
    }

    #[test]
    fn injected_runner_reports_provenance_and_never_retries_after_failure() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().to_path_buf();
        let root_output = format!("{}\n", root.display());
        let paths = fixture_paths(fixture.path());
        let mut runner = FixtureRunner::new([
            (
                vec!["rev-parse", "--show-toplevel"],
                Ok(root_output.into_bytes()),
            ),
            (
                vec!["remote", "get-url", "origin"],
                Err(ProjectCommandError::Timeout),
            ),
        ]);

        let resolved = ProjectIdentity::resolve_with(fixture.path(), &paths, &mut runner).unwrap();

        assert_eq!(resolved.root(), root);
        assert_eq!(resolved.provenance(), ProjectProvenance::Temporary);
        assert_eq!(runner.calls(), 2);
    }

    #[test]
    fn stable_uuid_constructor_canonicalizes_and_never_builds_temporary_identity() {
        let identity =
            ProjectIdentity::from_stable_uuid("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA").unwrap();

        assert_eq!(
            identity.id(),
            &ProjectId::Stable("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into())
        );
        assert!(ProjectIdentity::from_stable_uuid("temporary-project").is_err());
    }

    #[test]
    fn temporary_root_constructor_requires_no_filesystem_discovery() {
        let root = Path::new("/definitely/missing/project-root");

        let identity = ProjectIdentity::from_temporary_root(root);

        assert!(matches!(identity.id(), ProjectId::Temporary(_)));
        assert_eq!(identity, ProjectIdentity::from_temporary_root(root));
    }

    #[test]
    fn root_discovery_failure_never_attempts_remote_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let paths = fixture_paths(fixture.path());
        let mut runner = FixtureRunner::new([(
            vec!["rev-parse", "--show-toplevel"],
            Err(ProjectCommandError::Timeout),
        )]);

        let resolved = ProjectIdentity::resolve_with(fixture.path(), &paths, &mut runner).unwrap();

        assert_eq!(resolved.provenance(), ProjectProvenance::Temporary);
        assert!(!resolved.identity().is_durable());
        assert_eq!(runner.calls(), 1);
    }

    #[test]
    fn injected_runner_reports_manifest_provenance_without_remote_lookup() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().to_path_buf();
        let paths = fixture_paths(fixture.path());
        fs::create_dir_all(root.join(".coding-brain")).unwrap();
        fs::write(
            root.join(".coding-brain/project.toml"),
            format!(
                "schema_version = {PROJECT_SCHEMA_VERSION}\nproject_id = \"{}\"\n",
                uuid::Uuid::new_v4()
            ),
        )
        .unwrap();
        let mut runner = FixtureRunner::new([(
            vec!["rev-parse", "--show-toplevel"],
            Ok(format!("{}\n", root.display()).into_bytes()),
        )]);

        let resolved = ProjectIdentity::resolve_with(&root, &paths, &mut runner).unwrap();

        assert_eq!(resolved.root(), root);
        assert_eq!(resolved.provenance(), ProjectProvenance::Manifest);
        assert!(resolved.identity().is_durable());
        assert_eq!(runner.calls(), 1);
    }

    #[test]
    fn injected_runner_reports_network_remote_provenance() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().to_path_buf();
        let paths = fixture_paths(fixture.path());
        let mut runner = FixtureRunner::new([
            (
                vec!["rev-parse", "--show-toplevel"],
                Ok(format!("{}\n", root.display()).into_bytes()),
            ),
            (
                vec!["remote", "get-url", "origin"],
                Ok(b"https://user:secret@example.com/Owner/Repo.git\n".to_vec()),
            ),
        ]);

        let resolved = ProjectIdentity::resolve_with(&root, &paths, &mut runner).unwrap();

        assert_eq!(resolved.root(), root);
        assert_eq!(resolved.provenance(), ProjectProvenance::NetworkRemote);
        assert!(resolved.identity().is_durable());
        assert_eq!(runner.calls(), 2);
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_git_repository(root: &Path, remote: Option<&str>) {
        fs::create_dir_all(root).unwrap();
        run_git(root, &["init", "--quiet"]);
        if let Some(remote) = remote {
            run_git(root, &["remote", "add", "origin", remote]);
        }
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
    fn canonicalizes_equivalent_network_remotes() {
        for remote in [
            "https://github.com/Owner/Repo.git",
            "https://user:secret@github.com/Owner/Repo.git",
            "https://github.com/Owner/Repo.git?token=secret#clone",
            "ssh://git@github.com/Owner/Repo.git",
            "git@GITHUB.COM:Owner/Repo.git",
        ] {
            assert_eq!(
                canonical_remote(remote).as_deref(),
                Some("github.com/Owner/Repo")
            );
        }
    }

    #[test]
    fn normalization_preserves_ports_and_path_case() {
        assert_eq!(
            canonical_remote("ssh://git@example.com:2222/Owner/Repo.git").as_deref(),
            Some("example.com:2222/Owner/Repo")
        );
        assert_ne!(
            canonical_remote("https://example.com/Owner/Repo"),
            canonical_remote("https://example.com/owner/repo")
        );
    }

    #[test]
    fn rejects_machine_local_remotes() {
        assert_eq!(canonical_remote("upstream.git"), None);
        assert_eq!(canonical_remote("file:///srv/upstream.git"), None);
    }

    #[test]
    fn rejects_file_scheme_in_every_form() {
        for remote in [
            "file:///srv/upstream.git",
            "file:/srv/upstream.git",
            "file:relative.git",
            "FILE:///srv/upstream.git",
            "FiLe:/srv/upstream.git",
            "fIlE:relative.git",
        ] {
            assert_eq!(canonical_remote(remote), None, "accepted {remote:?}");
        }
    }

    #[test]
    fn rejects_windows_drive_paths() {
        for remote in ["C:/work/repo.git", r"C:\work\repo.git", "C:repo.git"] {
            assert_eq!(canonical_remote(remote), None);
        }
    }

    #[test]
    fn rejects_empty_or_incomplete_network_remotes() {
        for remote in [
            "",
            "https://",
            "https://github.com",
            "git@github.com:",
            "custom://github.com/Owner/Repo.git",
            "git@[2001:db8::1]:Owner/Repo.git",
        ] {
            assert_eq!(canonical_remote(remote), None);
        }
    }

    #[test]
    fn rejects_url_query_or_fragment_before_path() {
        for remote in [
            "https://github.com?token=secret/Owner/Repo.git",
            "https://github.com#fragment/Owner/Repo.git",
        ] {
            assert_eq!(canonical_remote(remote), None, "accepted {remote:?}");
        }
    }

    #[test]
    fn rejects_malformed_network_authorities() {
        for remote in [
            "ssh://:2222/Owner/Repo.git",
            "ssh://example.com:ssh/Owner/Repo.git",
            "ssh://example.com:+22/Owner/Repo.git",
            "ssh://example.com:65536/Owner/Repo.git",
            "ssh://2001:db8::1/Owner/Repo.git",
            "ssh://[]/Owner/Repo.git",
            "ssh://[not-ipv6]/Owner/Repo.git",
            "ssh://[2001:db8::1/Owner/Repo.git",
            "ssh://[2001:db8::1]extra/Owner/Repo.git",
            "ssh://[2001:db8::1]:/Owner/Repo.git",
            "ssh://[2001:db8::1]:ssh/Owner/Repo.git",
            "ssh://[2001:db8::1]:+22/Owner/Repo.git",
            "ssh://[2001:db8::1]:65536/Owner/Repo.git",
            "https://example .com/Owner/Repo.git",
            r"git@example\com:Owner/Repo.git",
        ] {
            assert_eq!(canonical_remote(remote), None, "accepted {remote:?}");
        }
    }

    #[test]
    fn accepts_bracketed_ipv6_in_url_form() {
        assert_eq!(
            canonical_remote("ssh://git@[2001:DB8::1]:2222/Owner/Repo.git").as_deref(),
            Some("[2001:db8::1]:2222/Owner/Repo")
        );
    }

    #[test]
    fn git_root_and_subdirectory_share_remote_identity() {
        let root = tempfile::tempdir().unwrap();
        init_git_repository(root.path(), Some("git@github.com:owner/repo.git"));
        let nested = root.path().join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(root.path());
        let root_id = ProjectIdentity::load(root.path(), &paths).unwrap();
        let nested_id = ProjectIdentity::load(&nested, &paths).unwrap();
        assert!(root_id.is_durable());
        assert_eq!(root_id, nested_id);
    }

    #[test]
    fn equivalent_clone_remotes_share_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("first");
        let second = fixture.path().join("second");
        init_git_repository(
            &first,
            Some("https://user:secret@github.com/Owner/Repo.git?token=hidden"),
        );
        init_git_repository(&second, Some("git@github.com:Owner/Repo.git"));
        let paths = fixture_paths(fixture.path());
        let first_id = ProjectIdentity::load(&first, &paths).unwrap();
        let second_id = ProjectIdentity::load(&second, &paths).unwrap();
        assert_eq!(first_id, second_id);
        let ProjectId::Stable(value) = first_id.id() else {
            panic!("remote identity must be stable");
        };
        assert!(uuid::Uuid::parse_str(value).is_ok());
        assert!(!value.contains("secret"));
        assert!(!value.contains("hidden"));
        assert!(!value.contains("Owner"));
    }

    #[test]
    fn root_manifest_overrides_remote_from_nested_directory() {
        let root = tempfile::tempdir().unwrap();
        init_git_repository(root.path(), Some("https://example.com/owner/repo.git"));
        let paths = fixture_paths(root.path());
        let explicit = ProjectManifest::create(root.path(), &paths).unwrap();
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(ProjectIdentity::load(&nested, &paths).unwrap(), explicit);
    }

    #[test]
    fn manifest_creation_from_nested_directory_writes_at_git_root() {
        let root = tempfile::tempdir().unwrap();
        init_git_repository(root.path(), Some("https://example.com/owner/repo.git"));
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(root.path());
        ProjectManifest::create(&nested, &paths).unwrap();
        assert!(root.path().join(".coding-brain/project.toml").is_file());
        assert!(!nested.join(".coding-brain/project.toml").exists());
    }

    #[test]
    fn repository_without_origin_uses_one_temporary_root_identity() {
        let root = tempfile::tempdir().unwrap();
        init_git_repository(root.path(), None);
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(root.path());
        let root_id = ProjectIdentity::load(root.path(), &paths).unwrap();
        let nested_id = ProjectIdentity::load(&nested, &paths).unwrap();
        assert!(!root_id.is_durable());
        assert_eq!(root_id, nested_id);
    }

    #[test]
    fn git_root_preserves_trailing_whitespace() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("repo ");
        init_git_repository(&root, None);
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(fixture.path());

        let root_id = ProjectIdentity::load(&root, &paths).unwrap();
        let nested_id = ProjectIdentity::load(&nested, &paths).unwrap();

        assert!(matches!(root_id.id(), ProjectId::Temporary(_)));
        assert_eq!(root_id, nested_id);
    }

    #[cfg(unix)]
    #[test]
    fn git_root_preserves_non_utf8_path_bytes() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture
            .path()
            .join(OsString::from_vec(b"repo-\xff".to_vec()));
        init_git_repository(&root, None);
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        let paths = fixture_paths(fixture.path());

        let root_id = ProjectIdentity::load(&root, &paths).unwrap();
        let nested_id = ProjectIdentity::load(&nested, &paths).unwrap();

        assert!(matches!(root_id.id(), ProjectId::Temporary(_)));
        assert_eq!(root_id, nested_id);
    }

    #[test]
    fn local_and_file_origins_use_one_temporary_root_identity() {
        let fixture = tempfile::tempdir().unwrap();
        for (name, remote) in [
            ("local", "../upstream.git"),
            ("file", "file:///srv/upstream.git"),
        ] {
            let root = fixture.path().join(name);
            init_git_repository(&root, Some(remote));
            let nested = root.join("nested");
            fs::create_dir_all(&nested).unwrap();
            let paths = fixture_paths(fixture.path());

            let root_id = ProjectIdentity::load(&root, &paths).unwrap();
            let nested_id = ProjectIdentity::load(&nested, &paths).unwrap();

            assert!(matches!(root_id.id(), ProjectId::Temporary(_)));
            assert_eq!(root_id, nested_id, "origin {remote:?}");
        }
    }

    #[test]
    fn malformed_root_manifest_does_not_fall_back_to_origin() {
        let root = tempfile::tempdir().unwrap();
        init_git_repository(root.path(), Some("https://example.com/owner/repo.git"));
        fs::create_dir_all(root.path().join(".coding-brain")).unwrap();
        fs::write(
            root.path().join(".coding-brain/project.toml"),
            "schema_version = 2\nproject_id = \"not-a-uuid\"\n",
        )
        .unwrap();
        let nested = root.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        assert!(ProjectIdentity::load(&nested, &fixture_paths(root.path())).is_err());
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
