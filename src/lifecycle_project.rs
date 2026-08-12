#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use coding_brain_core::paths::CodingBrainPaths;
use coding_brain_core::project::{
    ProjectCommandError, ProjectCommandRunner, ProjectError, ProjectId, ProjectIdentity,
    ProjectProvenance,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::brain::storage::{
    CacheProvenance, CacheRootKey, CacheRow, RuntimeCacheBypass, RuntimeCacheReader,
    RuntimeCacheWriter,
};
use crate::lifecycle_timing::{HookBudget, MonotonicClock};
use crate::provider_hooks::{BoundedProcessError, OutputBudget, run_bounded_process_until};

const GIT_DISCOVERY_BUDGET: Duration = Duration::from_millis(250);
const GIT_CLEANUP_RESERVE: Duration = Duration::from_millis(250);
const STORAGE_RESERVE: Duration = Duration::from_millis(500);
const GIT_OUTPUT_LIMIT: usize = 64 * 1024;
const MAX_COMPONENT_DEPTH: usize = 128;
const MAX_CLOSED_PATH_BYTES: usize = 4 * 1024;
const MAX_PATH_COMPONENTS: usize = 256;
const MAX_ENVIRONMENT_BYTES: usize = 64 * 1024;
const MAX_ENVIRONMENT_RECORDS: usize = 64;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_AUTHORITY_FILE_BYTES: usize = 64 * 1024;
const MAX_EXECUTABLE_BYTES: usize = 8 * 1024 * 1024;
const EVIDENCE_READ_CHUNK_BYTES: usize = 8 * 1024;
const MAX_CONFIG_RECORDS: usize = 512;
const EVIDENCE_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DependencySlot {
    RootManifest,
    GitMarker,
    GitDirectory,
    CommonDirectoryMarker,
    CommonDirectory,
    CommonConfig,
    WorktreeConfig,
    SystemConfig,
    SystemConfigAlt,
    SystemConfigHomebrew,
    GlobalConfig,
    XdgGlobalConfig,
    GitExecutable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Presence {
    Present,
    Missing,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClosedFileType {
    Regular,
    Directory,
    Absent,
}

#[derive(Clone, Copy)]
enum FileTypeRequirement {
    OptionalRegular,
    RequiredRegular,
    RequiredDirectory,
    RequiredGitMarker,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StableFileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FileEvidence {
    slot: DependencySlot,
    presence: Presence,
    file_type: ClosedFileType,
    stable_identity: StableFileIdentity,
    len: u64,
    modified_ns: Option<u128>,
    digest: [u8; 32],
    path_digest: [u8; 32],
}

impl FileEvidence {
    fn missing(slot: DependencySlot) -> Self {
        Self {
            slot,
            presence: Presence::Missing,
            file_type: ClosedFileType::Absent,
            stable_identity: StableFileIdentity {
                device: 0,
                inode: 0,
            },
            len: 0,
            modified_ns: None,
            digest: [0; 32],
            path_digest: [0; 32],
        }
    }

    fn not_applicable(slot: DependencySlot) -> Self {
        Self {
            presence: Presence::NotApplicable,
            ..Self::missing(slot)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DependencyEvidenceV1 {
    version: u8,
    provenance: EvidenceProvenance,
    environment_digest: [u8; 32],
    root_manifest: FileEvidence,
    git_marker: FileEvidence,
    git_directory: FileEvidence,
    common_directory_marker: FileEvidence,
    common_directory: FileEvidence,
    common_config: FileEvidence,
    worktree_config: FileEvidence,
    system_configs: [FileEvidence; 3],
    global_config: FileEvidence,
    xdg_global_config: FileEvidence,
    git_executable: FileEvidence,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceProvenance {
    Manifest,
    NetworkRemote,
}

#[derive(Clone, Debug)]
pub(crate) struct ClosedGitSlots {
    git_marker: PathBuf,
    git_directory: PathBuf,
    common_directory_marker: PathBuf,
    common_directory: PathBuf,
    common_config: PathBuf,
    worktree_config: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectCacheOutcome {
    Hit,
    Miss,
    Invalid,
    Bypassed,
    NonCacheable,
    DiscoveryFailure,
}

#[derive(Clone, Debug)]
pub(crate) struct CacheRefresh {
    root: PathBuf,
    project_uuid: String,
    provenance: CacheProvenance,
    evidence: Vec<u8>,
}

impl CacheRefresh {
    fn as_row(&self, refresh_order: i64) -> Result<CacheRow, RuntimeCacheBypass> {
        CacheRow::new(
            CacheRootKey::from_canonical_path(&self.root)?,
            &self.project_uuid,
            self.provenance,
            self.evidence.clone(),
            refresh_order,
        )
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedLifecycleProject {
    pub(crate) identity: ProjectIdentity,
    pub(crate) root: PathBuf,
    pub(crate) provenance: ProjectProvenance,
    pub(crate) cache_outcome: ProjectCacheOutcome,
    pub(crate) refresh: Option<CacheRefresh>,
}

impl ResolvedLifecycleProject {
    pub(crate) fn cache_outcome(&self) -> ProjectCacheOutcome {
        self.cache_outcome
    }
}

#[derive(Debug)]
pub(crate) enum LifecycleProjectError {
    Cwd(io::Error),
    Project(ProjectError),
}

impl fmt::Display for LifecycleProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cwd(error) => write!(formatter, "lifecycle cwd unavailable: {error}"),
            Self::Project(error) => {
                write!(formatter, "lifecycle project resolution failed: {error}")
            }
        }
    }
}

impl std::error::Error for LifecycleProjectError {}

impl From<ProjectError> for LifecycleProjectError {
    fn from(error: ProjectError) -> Self {
        Self::Project(error)
    }
}

pub(crate) trait LifecycleProjectRunner {
    fn output(
        &mut self,
        executable: &Path,
        cwd: &Path,
        args: &[&str],
        deadline: Instant,
        output: &mut OutputBudget,
    ) -> Result<Vec<u8>, ProjectCommandError>;
}

pub(crate) struct SystemLifecycleProjectRunner;

impl LifecycleProjectRunner for SystemLifecycleProjectRunner {
    fn output(
        &mut self,
        executable: &Path,
        cwd: &Path,
        args: &[&str],
        deadline: Instant,
        output: &mut OutputBudget,
    ) -> Result<Vec<u8>, ProjectCommandError> {
        let mut command = std::process::Command::new(executable);
        command.args(args).current_dir(cwd);
        run_bounded_process_until(&mut command, deadline, output).map_err(map_process_error)
    }
}

fn map_process_error(error: BoundedProcessError) -> ProjectCommandError {
    match error {
        BoundedProcessError::Spawn => ProjectCommandError::Spawn,
        BoundedProcessError::Io => ProjectCommandError::Io,
        BoundedProcessError::Timeout => ProjectCommandError::Timeout,
        BoundedProcessError::OutputLimit => ProjectCommandError::OutputLimit,
        BoundedProcessError::ExitStatus => ProjectCommandError::ExitStatus,
        BoundedProcessError::Cleanup => ProjectCommandError::Cleanup,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DiscoveryPlatform {
    Linux,
    MacOs,
    Unknown,
}

#[derive(Clone, Debug)]
struct DiscoveryEnvironment {
    platform: DiscoveryPlatform,
    home: PathBuf,
    xdg_config_home: Option<PathBuf>,
    path: OsString,
    system_config_candidates: Vec<PathBuf>,
    extra: Vec<(OsString, OsString)>,
    extra_complete: bool,
    #[cfg(test)]
    supported_git_executable: Option<PathBuf>,
}

impl DiscoveryEnvironment {
    fn current(deadline: Instant) -> Result<Self, EvidenceError> {
        ensure_before(deadline)?;
        let platform = if cfg!(target_os = "linux") {
            DiscoveryPlatform::Linux
        } else if cfg!(target_os = "macos") {
            DiscoveryPlatform::MacOs
        } else {
            DiscoveryPlatform::Unknown
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut extra = Vec::new();
        let mut extra_bytes = 0usize;
        let mut extra_complete = true;
        for (index, (key, value)) in std::env::vars_os().enumerate() {
            ensure_before(deadline)?;
            if index == MAX_ENVIRONMENT_ENTRIES {
                extra_complete = false;
                break;
            }
            let key_bytes = key.as_bytes();
            if !key_bytes.starts_with(b"GIT_")
                && !matches!(key_bytes, b"LANG" | b"LC_ALL" | b"LC_CTYPE")
            {
                continue;
            }
            let Some(next_bytes) = extra_bytes
                .checked_add(key.as_bytes().len())
                .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
            else {
                extra_complete = false;
                break;
            };
            if extra.len() == MAX_ENVIRONMENT_RECORDS || next_bytes > MAX_ENVIRONMENT_BYTES {
                extra_complete = false;
                break;
            }
            extra_bytes = next_bytes;
            extra.push((key, value));
        }
        ensure_before(deadline)?;
        Ok(Self {
            platform,
            home,
            xdg_config_home,
            path,
            system_config_candidates: system_config_candidates(platform),
            extra,
            extra_complete,
            #[cfg(test)]
            supported_git_executable: None,
        })
    }

    #[cfg(test)]
    fn fixture(
        platform: DiscoveryPlatform,
        home: PathBuf,
        xdg_config_home: Option<PathBuf>,
        path: OsString,
        system_config_candidates: Vec<PathBuf>,
    ) -> Self {
        let supported_git_executable = bin_git_from_path(&path);
        Self {
            platform,
            home,
            xdg_config_home,
            path,
            system_config_candidates,
            extra: Vec::new(),
            extra_complete: true,
            supported_git_executable: Some(supported_git_executable),
        }
    }

    fn has_path_changing_override(&self) -> bool {
        self.extra
            .iter()
            .any(|(key, _)| key.as_bytes().starts_with(b"GIT_"))
    }

    fn cache_inputs_are_bounded(&self, deadline: Instant) -> bool {
        if ensure_before(deadline).is_err() {
            return false;
        }
        if !self.extra_complete
            || self.home.as_os_str().as_bytes().len() > MAX_CLOSED_PATH_BYTES
            || self
                .xdg_config_home
                .as_deref()
                .is_some_and(|path| path.as_os_str().as_bytes().len() > MAX_CLOSED_PATH_BYTES)
            || self.path.as_bytes().len() > MAX_ENVIRONMENT_BYTES
            || self.extra.len() > MAX_ENVIRONMENT_RECORDS
            || self.system_config_candidates.len() > 3
        {
            return false;
        }
        let extra_bytes = self.extra.iter().try_fold(0usize, |total, (key, value)| {
            total
                .checked_add(key.as_bytes().len())?
                .checked_add(value.as_bytes().len())
        });
        if extra_bytes.is_none_or(|bytes| bytes > MAX_ENVIRONMENT_BYTES) {
            return false;
        }
        let mut components = std::env::split_paths(&self.path);
        for _ in 0..MAX_PATH_COMPONENTS {
            if ensure_before(deadline).is_err() {
                return false;
            }
            let Some(component) = components.next() else {
                return true;
            };
            if component.as_os_str().as_bytes().len() > MAX_CLOSED_PATH_BYTES {
                return false;
            }
        }
        components.next().is_none()
    }

    fn global_paths(&self) -> Result<(PathBuf, PathBuf), EvidenceError> {
        if !self.home.is_absolute() || self.home.as_os_str().is_empty() {
            return Err(EvidenceError::Unrepresentable);
        }
        let xdg = match &self.xdg_config_home {
            Some(path) if !path.as_os_str().is_empty() && path.is_absolute() => path.clone(),
            Some(_) => return Err(EvidenceError::Unrepresentable),
            None => self.home.join(".config"),
        };
        Ok((self.home.join(".gitconfig"), xdg.join("git/config")))
    }

    fn digest(&self, deadline: Instant) -> Result<[u8; 32], EvidenceError> {
        ensure_before(deadline)?;
        let mut hasher = Sha256::new();
        digest_field(&mut hasher, b"home", self.home.as_os_str());
        digest_field(
            &mut hasher,
            b"xdg_config_home",
            self.xdg_config_home
                .as_deref()
                .map(Path::as_os_str)
                .unwrap_or_default(),
        );
        digest_field(&mut hasher, b"path", &self.path);
        let mut extra = self.extra.clone();
        extra.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
        for (key, value) in extra {
            ensure_before(deadline)?;
            digest_field(&mut hasher, key.as_bytes(), &value);
        }
        ensure_before(deadline)?;
        Ok(hasher.finalize().into())
    }

    fn closed_system_config_candidates(&self, executable: &Path) -> Option<Vec<PathBuf>> {
        #[cfg(test)]
        if self.supported_git_executable.as_deref() == Some(executable) {
            return Some(self.system_config_candidates.clone());
        }

        let supported = match self.platform {
            DiscoveryPlatform::Linux => {
                executable == Path::new("/usr/bin/git") || is_nix_store_git(executable)
            }
            DiscoveryPlatform::MacOs => {
                executable == Path::new("/usr/bin/git")
                    || is_nix_store_git(executable)
                    || is_cellar_git(executable, Path::new("/usr/local/Cellar/git"))
                    || is_cellar_git(executable, Path::new("/opt/homebrew/Cellar/git"))
            }
            DiscoveryPlatform::Unknown => false,
        };
        supported.then(|| system_config_candidates(self.platform))
    }
}

#[cfg(test)]
fn bin_git_from_path(path: &OsStr) -> PathBuf {
    std::env::split_paths(path)
        .next()
        .unwrap_or_default()
        .join("git")
}

fn is_nix_store_git(path: &Path) -> bool {
    let components = path.components().collect::<Vec<_>>();
    if components.len() != 6
        || !matches!(components[0], Component::RootDir)
        || components[1].as_os_str() != "nix"
        || components[2].as_os_str() != "store"
        || components[4].as_os_str() != "bin"
        || components[5].as_os_str() != "git"
    {
        return false;
    }
    let leaf = components[3].as_os_str().as_bytes();
    leaf.len() > 37
        && leaf[32..].starts_with(b"-git-")
        && leaf[..32]
            .iter()
            .all(|byte| b"0123456789abcdfghijklmnpqrsvwxyz".contains(byte))
}

fn is_cellar_git(path: &Path, prefix: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(prefix) else {
        return false;
    };
    let components = relative.components().collect::<Vec<_>>();
    components.len() == 3
        && matches!(components[0], Component::Normal(_))
        && components[1].as_os_str() == "bin"
        && components[2].as_os_str() == "git"
}

fn digest_field(hasher: &mut Sha256, name: &[u8], value: &OsStr) {
    hasher.update((name.len() as u64).to_le_bytes());
    hasher.update(name);
    hasher.update((value.as_bytes().len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn system_config_candidates(platform: DiscoveryPlatform) -> Vec<PathBuf> {
    match platform {
        DiscoveryPlatform::Linux => vec![PathBuf::from("/etc/gitconfig")],
        DiscoveryPlatform::MacOs => vec![
            PathBuf::from("/etc/gitconfig"),
            PathBuf::from("/usr/local/etc/gitconfig"),
            PathBuf::from("/opt/homebrew/etc/gitconfig"),
        ],
        DiscoveryPlatform::Unknown => Vec::new(),
    }
}

struct SelectedGit {
    executable: PathBuf,
    system_config_candidates: Vec<PathBuf>,
    cacheable: bool,
}

fn select_git(environment: &DiscoveryEnvironment, deadline: Instant) -> SelectedGit {
    if !environment.cache_inputs_are_bounded(deadline) {
        return SelectedGit {
            executable: PathBuf::from("git"),
            system_config_candidates: Vec::new(),
            cacheable: false,
        };
    }
    let mut selected = None;
    let mut distinct = Vec::new();
    let mut cacheable = true;
    for component in std::env::split_paths(&environment.path) {
        if ensure_before(deadline).is_err() {
            cacheable = false;
            break;
        }
        if component.as_os_str().is_empty() || !component.is_absolute() {
            cacheable = false;
            continue;
        }
        let candidate = component.join("git");
        let Ok(metadata_result) = checked_io(deadline, || fs::metadata(&candidate)) else {
            cacheable = false;
            break;
        };
        let Ok(metadata) = metadata_result else {
            continue;
        };
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let Ok(canonical_result) = checked_io(deadline, || fs::canonicalize(candidate)) else {
            cacheable = false;
            break;
        };
        let Ok(canonical) = canonical_result else {
            cacheable = false;
            continue;
        };
        if selected.is_none() {
            selected = Some(canonical.clone());
        }
        if !distinct.contains(&canonical) {
            distinct.push(canonical);
        }
    }
    if distinct.len() != 1 {
        cacheable = false;
    }
    let executable = selected.unwrap_or_else(|| PathBuf::from("git"));
    let system_config_candidates = environment
        .closed_system_config_candidates(&executable)
        .unwrap_or_default();
    if system_config_candidates.is_empty() {
        cacheable = false;
    }
    SelectedGit {
        executable,
        system_config_candidates,
        cacheable,
    }
}

struct GitDiscovery<'a, R> {
    runner: &'a mut R,
    executable: &'a Path,
    deadline: Instant,
    output: OutputBudget,
}

impl<'a, R: LifecycleProjectRunner> GitDiscovery<'a, R> {
    fn run(&mut self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError> {
        if Instant::now() >= self.deadline {
            return Err(ProjectCommandError::Timeout);
        }
        let result =
            self.runner
                .output(self.executable, cwd, args, self.deadline, &mut self.output);
        if Instant::now() >= self.deadline {
            return Err(ProjectCommandError::Timeout);
        }
        result
    }
}

struct KnownRootRunner<'a, 'b, R> {
    root: &'a Path,
    discovery: &'b mut GitDiscovery<'a, R>,
    root_replayed: bool,
    remote_output: Option<Vec<u8>>,
}

impl<R: LifecycleProjectRunner> ProjectCommandRunner for KnownRootRunner<'_, '_, R> {
    fn output(&mut self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>, ProjectCommandError> {
        if !self.root_replayed && args == ["rev-parse", "--show-toplevel"] {
            self.root_replayed = true;
            let mut bytes = self.root.as_os_str().as_bytes().to_vec();
            bytes.push(b'\n');
            return Ok(bytes);
        }
        let output = self.discovery.run(cwd, args)?;
        if args == ["remote", "get-url", "origin"] {
            self.remote_output = Some(output.clone());
        }
        Ok(output)
    }
}

pub(crate) fn resolve_lifecycle_project(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    budget: &HookBudget,
    runner: &mut impl LifecycleProjectRunner,
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    let Some(deadline) =
        budget.optional_child_deadline(GIT_DISCOVERY_BUDGET, GIT_CLEANUP_RESERVE + STORAGE_RESERVE)
    else {
        return Ok(resolve_without_discovery(cwd));
    };
    let environment = match DiscoveryEnvironment::current(deadline) {
        Ok(environment) => environment,
        Err(_) => return Ok(resolve_without_discovery(cwd)),
    };
    resolve_lifecycle_project_until_with_environment_and_cache_stage(
        cwd,
        paths,
        reader,
        runner,
        &environment,
        deadline,
        |_| {},
    )
}

pub(crate) fn resolve_lifecycle_project_until(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    runner: &mut impl LifecycleProjectRunner,
    deadline: Instant,
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    let environment = match DiscoveryEnvironment::current(deadline) {
        Ok(environment) => environment,
        Err(_) => return Ok(resolve_without_discovery(cwd)),
    };
    resolve_lifecycle_project_until_with_environment_and_cache_stage(
        cwd,
        paths,
        reader,
        runner,
        &environment,
        deadline,
        |_| {},
    )
}

pub(crate) fn resolve_lifecycle_project_until_with_cache_stage(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    runner: &mut impl LifecycleProjectRunner,
    deadline: Instant,
    on_cache: impl FnOnce(ProjectCacheOutcome),
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    let environment = match DiscoveryEnvironment::current(deadline) {
        Ok(environment) => environment,
        Err(_) => return Ok(resolve_without_discovery(cwd)),
    };
    resolve_lifecycle_project_until_with_environment_and_cache_stage(
        cwd,
        paths,
        reader,
        runner,
        &environment,
        deadline,
        on_cache,
    )
}

fn resolve_lifecycle_project_with_environment<C: MonotonicClock>(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    budget: &HookBudget<C>,
    runner: &mut impl LifecycleProjectRunner,
    environment: &DiscoveryEnvironment,
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    let Some(deadline) =
        budget.optional_child_deadline(GIT_DISCOVERY_BUDGET, GIT_CLEANUP_RESERVE + STORAGE_RESERVE)
    else {
        return Ok(resolve_without_discovery(cwd));
    };
    resolve_lifecycle_project_until_with_environment_and_cache_stage(
        cwd,
        paths,
        reader,
        runner,
        environment,
        deadline,
        |_| {},
    )
}

fn resolve_lifecycle_project_until_with_environment(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    runner: &mut impl LifecycleProjectRunner,
    environment: &DiscoveryEnvironment,
    deadline: Instant,
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    resolve_lifecycle_project_until_with_environment_and_cache_stage(
        cwd,
        paths,
        reader,
        runner,
        environment,
        deadline,
        |_| {},
    )
}

fn resolve_lifecycle_project_until_with_environment_and_cache_stage(
    cwd: &Path,
    paths: &CodingBrainPaths,
    reader: Option<&RuntimeCacheReader>,
    runner: &mut impl LifecycleProjectRunner,
    environment: &DiscoveryEnvironment,
    deadline: Instant,
    on_cache: impl FnOnce(ProjectCacheOutcome),
) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
    ensure_before(deadline)
        .map_err(|_| LifecycleProjectError::Cwd(io::Error::other("deadline")))?;
    let canonical_cwd = fs::canonicalize(cwd).map_err(LifecycleProjectError::Cwd)?;
    ensure_before(deadline)
        .map_err(|_| LifecycleProjectError::Cwd(io::Error::other("deadline")))?;
    let (hit, initial_outcome) = lookup_cache(&canonical_cwd, reader, environment, deadline);
    on_cache(initial_outcome);
    if let Some(hit) = hit {
        return Ok(hit);
    }

    let selected_git = select_git(environment, deadline);
    let mut discovery = GitDiscovery {
        runner,
        executable: &selected_git.executable,
        deadline,
        output: OutputBudget::new(GIT_OUTPUT_LIMIT),
    };
    let root_output = match discovery.run(&canonical_cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(output) => output,
        Err(_) => {
            return Ok(resolve_without_discovery_with_outcome(
                &canonical_cwd,
                initial_outcome,
            ));
        }
    };
    let Some(root) = parse_root_output(&canonical_cwd, root_output, deadline) else {
        return Ok(resolve_without_discovery_with_outcome(
            &canonical_cwd,
            initial_outcome,
        ));
    };

    let pre_manifest = collect_evidence(
        &root,
        environment,
        EvidenceProvenance::Manifest,
        None,
        deadline,
    );
    let pre_network = collect_evidence(
        &root,
        environment,
        EvidenceProvenance::NetworkRemote,
        Some(&selected_git),
        deadline,
    );
    ensure_before(deadline).map_err(|_| ProjectError::Io(io::Error::other("deadline")))?;
    let (resolution, remote_output) = {
        let mut replay = KnownRootRunner {
            root: &root,
            discovery: &mut discovery,
            root_replayed: false,
            remote_output: None,
        };
        let resolution = ProjectIdentity::resolve_with(&canonical_cwd, paths, &mut replay)?;
        (resolution, replay.remote_output)
    };
    ensure_before(deadline).map_err(|_| ProjectError::Io(io::Error::other("deadline")))?;

    let provenance = resolution.provenance();
    let identity = resolution.identity().clone();
    let resolved_root = resolution.root().to_path_buf();
    if provenance == ProjectProvenance::Temporary {
        return Ok(ResolvedLifecycleProject {
            identity,
            root: resolved_root,
            provenance,
            cache_outcome: if initial_outcome == ProjectCacheOutcome::Bypassed {
                ProjectCacheOutcome::Bypassed
            } else {
                ProjectCacheOutcome::DiscoveryFailure
            },
            refresh: None,
        });
    }

    let origin_cacheable = if provenance == ProjectProvenance::NetworkRemote {
        let config = match discovery.run(
            &resolved_root,
            &["config", "--list", "--show-origin", "--show-scope", "-z"],
        ) {
            Ok(config) => config,
            Err(_) => {
                return Ok(resolve_without_discovery_with_outcome(
                    &resolved_root,
                    initial_outcome,
                ));
            }
        };
        let Some(remote_output) = remote_output.as_deref() else {
            return Ok(resolve_without_discovery_with_outcome(
                &resolved_root,
                initial_outcome,
            ));
        };
        config_origins_are_closed(
            &config,
            remote_output,
            &resolved_root,
            environment,
            &selected_git,
            deadline,
        )
        .unwrap_or(false)
    } else {
        true
    };

    let (pre, post) = match provenance {
        ProjectProvenance::Manifest => (
            pre_manifest,
            collect_evidence(
                &resolved_root,
                environment,
                EvidenceProvenance::Manifest,
                None,
                deadline,
            ),
        ),
        ProjectProvenance::NetworkRemote => (
            pre_network,
            collect_evidence(
                &resolved_root,
                environment,
                EvidenceProvenance::NetworkRemote,
                Some(&selected_git),
                deadline,
            ),
        ),
        ProjectProvenance::Temporary => unreachable!(),
    };
    let confirmed_root_output =
        match discovery.run(&canonical_cwd, &["rev-parse", "--show-toplevel"]) {
            Ok(output) => output,
            Err(_) => {
                return Ok(resolve_without_discovery_with_outcome(
                    &canonical_cwd,
                    initial_outcome,
                ));
            }
        };
    let Some(confirmed_root) = parse_root_output(&canonical_cwd, confirmed_root_output, deadline)
    else {
        return Ok(resolve_without_discovery_with_outcome(
            &canonical_cwd,
            initial_outcome,
        ));
    };
    if confirmed_root != root
        || has_intervening_boundary(&root, &canonical_cwd, deadline).unwrap_or(true)
    {
        return Ok(resolve_without_discovery_with_outcome(
            &canonical_cwd,
            initial_outcome,
        ));
    }
    let refresh = if origin_cacheable && initial_outcome != ProjectCacheOutcome::Bypassed {
        matching_refresh(&resolved_root, &identity, provenance, pre, post)
    } else {
        None
    };
    let cache_outcome = if initial_outcome == ProjectCacheOutcome::Bypassed {
        ProjectCacheOutcome::Bypassed
    } else if refresh.is_some() {
        initial_outcome
    } else {
        ProjectCacheOutcome::NonCacheable
    };
    Ok(ResolvedLifecycleProject {
        identity,
        root: resolved_root,
        provenance,
        cache_outcome,
        refresh,
    })
}

fn resolve_without_discovery(cwd: &Path) -> ResolvedLifecycleProject {
    resolve_without_discovery_with_outcome(cwd, ProjectCacheOutcome::DiscoveryFailure)
}

fn resolve_without_discovery_with_outcome(
    cwd: &Path,
    initial_outcome: ProjectCacheOutcome,
) -> ResolvedLifecycleProject {
    ResolvedLifecycleProject {
        identity: ProjectIdentity::from_temporary_root(cwd),
        root: cwd.to_path_buf(),
        provenance: ProjectProvenance::Temporary,
        cache_outcome: if initial_outcome == ProjectCacheOutcome::Bypassed {
            ProjectCacheOutcome::Bypassed
        } else {
            ProjectCacheOutcome::DiscoveryFailure
        },
        refresh: None,
    }
}

fn parse_root_output(cwd: &Path, mut output: Vec<u8>, deadline: Instant) -> Option<PathBuf> {
    ensure_before(deadline).ok()?;
    while output
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        output.pop();
    }
    if output.is_empty() || output.contains(&0) {
        return None;
    }
    let path = PathBuf::from(OsString::from_vec(output));
    let candidate = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let canonical = checked_io(deadline, || fs::canonicalize(candidate))
        .ok()?
        .ok()?;
    Some(canonical)
}

fn lookup_cache(
    cwd: &Path,
    reader: Option<&RuntimeCacheReader>,
    environment: &DiscoveryEnvironment,
    deadline: Instant,
) -> (Option<ResolvedLifecycleProject>, ProjectCacheOutcome) {
    if ensure_before(deadline).is_err() {
        return (None, ProjectCacheOutcome::Bypassed);
    }
    let Some(reader) = reader else {
        return (None, ProjectCacheOutcome::Miss);
    };
    let roots = match reader.candidate_roots() {
        Ok(roots) => roots,
        Err(_) => return (None, ProjectCacheOutcome::Bypassed),
    };
    if ensure_before(deadline).is_err() {
        return (None, ProjectCacheOutcome::Bypassed);
    }
    let selected = roots
        .into_iter()
        .filter(|root| cwd.starts_with(root.as_path()))
        .max_by_key(|root| root.as_path().components().count());
    let Some(key) = selected else {
        return (None, ProjectCacheOutcome::Miss);
    };
    let root = key.as_path();
    let canonical_matches = checked_io(deadline, || fs::canonicalize(&root))
        .ok()
        .and_then(Result::ok)
        .is_some_and(|canonical| canonical == root);
    if !canonical_matches || has_intervening_boundary(&root, cwd, deadline).unwrap_or(true) {
        return (None, ProjectCacheOutcome::Invalid);
    }
    let row = match reader.load_selected_row(&key) {
        Ok(row) => row,
        Err(_) => return (None, ProjectCacheOutcome::Bypassed),
    };
    if ensure_before(deadline).is_err() {
        return (None, ProjectCacheOutcome::Bypassed);
    }
    let evidence: DependencyEvidenceV1 = match serde_json::from_slice(row.evidence()) {
        Ok(evidence) => evidence,
        Err(_) => return (None, ProjectCacheOutcome::Bypassed),
    };
    let (provenance, expected_provenance) = match row.provenance() {
        CacheProvenance::Manifest => (ProjectProvenance::Manifest, EvidenceProvenance::Manifest),
        CacheProvenance::NetworkRemote => (
            ProjectProvenance::NetworkRemote,
            EvidenceProvenance::NetworkRemote,
        ),
    };
    if evidence.version != EVIDENCE_VERSION || evidence.provenance != expected_provenance {
        return (None, ProjectCacheOutcome::Bypassed);
    }
    let selected_git = (expected_provenance == EvidenceProvenance::NetworkRemote)
        .then(|| select_git(environment, deadline));
    let Ok(current) = collect_evidence(
        &root,
        environment,
        expected_provenance,
        selected_git.as_ref(),
        deadline,
    ) else {
        return (None, ProjectCacheOutcome::Invalid);
    };
    if evidence != current {
        return (None, ProjectCacheOutcome::Invalid);
    }
    let Ok(identity) = ProjectIdentity::from_stable_uuid(row.project_uuid()) else {
        return (None, ProjectCacheOutcome::Bypassed);
    };
    (
        Some(ResolvedLifecycleProject {
            identity,
            root,
            provenance,
            cache_outcome: ProjectCacheOutcome::Hit,
            refresh: None,
        }),
        ProjectCacheOutcome::Hit,
    )
}

fn has_intervening_boundary(root: &Path, cwd: &Path, deadline: Instant) -> io::Result<bool> {
    ensure_before(deadline).map_err(|_| io::Error::other("deadline"))?;
    let relative = cwd
        .strip_prefix(root)
        .map_err(|_| io::Error::other("cwd is outside selected root"))?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() > MAX_COMPONENT_DEPTH {
        return Ok(true);
    }
    let mut current = root.to_path_buf();
    for component in components {
        ensure_before(deadline).map_err(|_| io::Error::other("deadline"))?;
        let Component::Normal(component) = component else {
            return Ok(true);
        };
        current.push(component);
        for boundary in [
            current.join(".git"),
            current.join(".coding-brain/project.toml"),
        ] {
            let metadata = checked_io(deadline, || fs::symlink_metadata(boundary))
                .map_err(|_| io::Error::other("deadline"))?;
            match metadata {
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(false)
}

#[derive(Debug)]
enum EvidenceError {
    Io,
    Deadline,
    MissingTopology,
    Unrepresentable,
    Oversized,
}

fn ensure_before(deadline: Instant) -> Result<(), EvidenceError> {
    if Instant::now() < deadline {
        Ok(())
    } else {
        Err(EvidenceError::Deadline)
    }
}

fn checked_io<T>(
    deadline: Instant,
    operation: impl FnOnce() -> io::Result<T>,
) -> Result<io::Result<T>, EvidenceError> {
    ensure_before(deadline)?;
    let result = operation();
    ensure_before(deadline)?;
    Ok(result)
}

fn collect_evidence(
    root: &Path,
    environment: &DiscoveryEnvironment,
    provenance: EvidenceProvenance,
    selected_git: Option<&SelectedGit>,
    deadline: Instant,
) -> Result<DependencyEvidenceV1, EvidenceError> {
    ensure_before(deadline)?;
    ensure_closed_absolute_path(root)?;
    let manifest = collect_file(
        &root.join(".coding-brain/project.toml"),
        DependencySlot::RootManifest,
        MAX_AUTHORITY_FILE_BYTES,
        false,
        false,
        FileTypeRequirement::OptionalRegular,
        deadline,
    )?;
    let slots = match resolve_git_slots(root, deadline) {
        Ok(slots) => Some(slots),
        Err(EvidenceError::MissingTopology) if provenance == EvidenceProvenance::Manifest => None,
        Err(error) => return Err(error),
    };
    let (git_marker, git_directory, common_directory_marker, common_directory) =
        if let Some(slots) = &slots {
            (
                collect_file(
                    &slots.git_marker,
                    DependencySlot::GitMarker,
                    MAX_AUTHORITY_FILE_BYTES,
                    false,
                    false,
                    FileTypeRequirement::RequiredGitMarker,
                    deadline,
                )?,
                collect_file(
                    &slots.git_directory,
                    DependencySlot::GitDirectory,
                    MAX_AUTHORITY_FILE_BYTES,
                    false,
                    false,
                    FileTypeRequirement::RequiredDirectory,
                    deadline,
                )?,
                collect_file(
                    &slots.common_directory_marker,
                    DependencySlot::CommonDirectoryMarker,
                    MAX_AUTHORITY_FILE_BYTES,
                    false,
                    false,
                    FileTypeRequirement::OptionalRegular,
                    deadline,
                )?,
                collect_file(
                    &slots.common_directory,
                    DependencySlot::CommonDirectory,
                    MAX_AUTHORITY_FILE_BYTES,
                    false,
                    false,
                    FileTypeRequirement::RequiredDirectory,
                    deadline,
                )?,
            )
        } else {
            (
                FileEvidence::missing(DependencySlot::GitMarker),
                FileEvidence::not_applicable(DependencySlot::GitDirectory),
                FileEvidence::not_applicable(DependencySlot::CommonDirectoryMarker),
                FileEvidence::not_applicable(DependencySlot::CommonDirectory),
            )
        };

    let mut evidence = DependencyEvidenceV1 {
        version: EVIDENCE_VERSION,
        provenance,
        environment_digest: [0; 32],
        root_manifest: manifest,
        git_marker,
        git_directory,
        common_directory_marker,
        common_directory,
        common_config: FileEvidence::not_applicable(DependencySlot::CommonConfig),
        worktree_config: FileEvidence::not_applicable(DependencySlot::WorktreeConfig),
        system_configs: [
            FileEvidence::not_applicable(DependencySlot::SystemConfig),
            FileEvidence::not_applicable(DependencySlot::SystemConfigAlt),
            FileEvidence::not_applicable(DependencySlot::SystemConfigHomebrew),
        ],
        global_config: FileEvidence::not_applicable(DependencySlot::GlobalConfig),
        xdg_global_config: FileEvidence::not_applicable(DependencySlot::XdgGlobalConfig),
        git_executable: FileEvidence::not_applicable(DependencySlot::GitExecutable),
    };
    if provenance == EvidenceProvenance::Manifest {
        return Ok(evidence);
    }
    let slots = slots.ok_or(EvidenceError::Unrepresentable)?;
    let selected_git = selected_git.ok_or(EvidenceError::Unrepresentable)?;
    if environment.platform == DiscoveryPlatform::Unknown
        || !environment.cache_inputs_are_bounded(deadline)
        || environment.has_path_changing_override()
        || !selected_git.cacheable
        || selected_git.system_config_candidates.len() > 3
    {
        return Err(EvidenceError::Unrepresentable);
    }
    let (global, xdg_global) = environment.global_paths()?;
    evidence.environment_digest = environment.digest(deadline)?;
    evidence.common_config = collect_file(
        &slots.common_config,
        DependencySlot::CommonConfig,
        MAX_AUTHORITY_FILE_BYTES,
        false,
        false,
        FileTypeRequirement::OptionalRegular,
        deadline,
    )?;
    evidence.worktree_config = collect_file(
        &slots.worktree_config,
        DependencySlot::WorktreeConfig,
        MAX_AUTHORITY_FILE_BYTES,
        false,
        false,
        FileTypeRequirement::OptionalRegular,
        deadline,
    )?;
    for (index, path) in selected_git.system_config_candidates.iter().enumerate() {
        let slot = [
            DependencySlot::SystemConfig,
            DependencySlot::SystemConfigAlt,
            DependencySlot::SystemConfigHomebrew,
        ][index];
        evidence.system_configs[index] = collect_file(
            path,
            slot,
            MAX_AUTHORITY_FILE_BYTES,
            false,
            false,
            FileTypeRequirement::OptionalRegular,
            deadline,
        )?;
    }
    evidence.global_config = collect_file(
        &global,
        DependencySlot::GlobalConfig,
        MAX_AUTHORITY_FILE_BYTES,
        false,
        false,
        FileTypeRequirement::OptionalRegular,
        deadline,
    )?;
    evidence.xdg_global_config = collect_file(
        &xdg_global,
        DependencySlot::XdgGlobalConfig,
        MAX_AUTHORITY_FILE_BYTES,
        false,
        false,
        FileTypeRequirement::OptionalRegular,
        deadline,
    )?;
    evidence.git_executable = collect_file(
        &selected_git.executable,
        DependencySlot::GitExecutable,
        MAX_EXECUTABLE_BYTES,
        true,
        true,
        FileTypeRequirement::RequiredRegular,
        deadline,
    )?;
    ensure_before(deadline)?;
    Ok(evidence)
}

fn resolve_git_slots(root: &Path, deadline: Instant) -> Result<ClosedGitSlots, EvidenceError> {
    ensure_before(deadline)?;
    let git_marker = root.join(".git");
    let marker = match checked_io(deadline, || fs::symlink_metadata(&git_marker))? {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(EvidenceError::MissingTopology);
        }
        Err(_) => return Err(EvidenceError::Io),
    };
    if marker.file_type().is_symlink() {
        return Err(EvidenceError::Unrepresentable);
    }
    let git_directory = if marker.is_dir() {
        git_marker.clone()
    } else if marker.is_file() {
        let contents = read_closed_file(&git_marker, MAX_AUTHORITY_FILE_BYTES, false, deadline)?;
        let path = parse_git_pointer(&contents, b"gitdir: ")?;
        canonicalize_relative(git_marker.parent().unwrap_or(root), path, deadline)?
    } else {
        return Err(EvidenceError::Unrepresentable);
    };
    let git_directory_metadata =
        checked_io(deadline, || fs::metadata(&git_directory))?.map_err(|_| EvidenceError::Io)?;
    if !git_directory_metadata.is_dir() {
        return Err(EvidenceError::Unrepresentable);
    }
    let common_directory_marker = git_directory.join("commondir");
    let common_directory =
        match checked_io(deadline, || fs::symlink_metadata(&common_directory_marker))? {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let contents = read_closed_file(
                    &common_directory_marker,
                    MAX_AUTHORITY_FILE_BYTES,
                    false,
                    deadline,
                )?;
                let path = trim_line(&contents).ok_or(EvidenceError::Unrepresentable)?;
                canonicalize_relative(&git_directory, path, deadline)?
            }
            Ok(_) => return Err(EvidenceError::Unrepresentable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => git_directory.clone(),
            Err(_) => return Err(EvidenceError::Io),
        };
    let common_directory_metadata =
        checked_io(deadline, || fs::metadata(&common_directory))?.map_err(|_| EvidenceError::Io)?;
    if !common_directory_metadata.is_dir() {
        return Err(EvidenceError::Unrepresentable);
    }
    Ok(ClosedGitSlots {
        git_marker,
        git_directory: git_directory.clone(),
        common_directory_marker,
        common_directory: common_directory.clone(),
        common_config: common_directory.join("config"),
        worktree_config: git_directory.join("config.worktree"),
    })
}

fn parse_git_pointer<'a>(contents: &'a [u8], prefix: &[u8]) -> Result<&'a [u8], EvidenceError> {
    let line = trim_line(contents).ok_or(EvidenceError::Unrepresentable)?;
    line.strip_prefix(prefix)
        .ok_or(EvidenceError::Unrepresentable)
}

fn trim_line(contents: &[u8]) -> Option<&[u8]> {
    let contents = contents.strip_suffix(b"\n").unwrap_or(contents);
    let contents = contents.strip_suffix(b"\r").unwrap_or(contents);
    (!contents.is_empty() && !contents.contains(&0) && !contents.contains(&b'\n'))
        .then_some(contents)
}

fn canonicalize_relative(
    base: &Path,
    bytes: &[u8],
    deadline: Instant,
) -> Result<PathBuf, EvidenceError> {
    ensure_before(deadline)?;
    let path = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    let candidate = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    ensure_bounded_candidate_path(&candidate)?;
    let canonical =
        checked_io(deadline, || fs::canonicalize(candidate))?.map_err(|_| EvidenceError::Io)?;
    ensure_closed_absolute_path(&canonical)?;
    Ok(canonical)
}

fn collect_file(
    path: &Path,
    slot: DependencySlot,
    cap: usize,
    require_executable: bool,
    include_path_digest: bool,
    requirement: FileTypeRequirement,
    deadline: Instant,
) -> Result<FileEvidence, EvidenceError> {
    let evidence = open_file_snapshot(
        path,
        slot,
        cap,
        require_executable,
        include_path_digest,
        deadline,
    )?
    .0;
    let accepted = match requirement {
        FileTypeRequirement::OptionalRegular => {
            evidence.presence == Presence::Missing || evidence.file_type == ClosedFileType::Regular
        }
        FileTypeRequirement::RequiredRegular => {
            evidence.presence == Presence::Present && evidence.file_type == ClosedFileType::Regular
        }
        FileTypeRequirement::RequiredDirectory => {
            evidence.presence == Presence::Present
                && evidence.file_type == ClosedFileType::Directory
        }
        FileTypeRequirement::RequiredGitMarker => {
            evidence.presence == Presence::Present
                && matches!(
                    evidence.file_type,
                    ClosedFileType::Regular | ClosedFileType::Directory
                )
        }
    };
    if !accepted {
        return Err(EvidenceError::Unrepresentable);
    }
    Ok(evidence)
}

fn open_file_snapshot(
    path: &Path,
    slot: DependencySlot,
    cap: usize,
    require_executable: bool,
    include_path_digest: bool,
    deadline: Instant,
) -> Result<(FileEvidence, Option<Vec<u8>>), EvidenceError> {
    ensure_closed_absolute_path(path)?;
    let before = match checked_io(deadline, || fs::symlink_metadata(path))? {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((FileEvidence::missing(slot), None));
        }
        Err(_) => return Err(EvidenceError::Io),
    };
    if before.file_type().is_symlink() {
        return Err(EvidenceError::Unrepresentable);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = checked_io(deadline, || options.open(path))?.map_err(|_| EvidenceError::Io)?;
    let metadata = checked_io(deadline, || file.metadata())?.map_err(|_| EvidenceError::Io)?;
    if before.dev() != metadata.dev() || before.ino() != metadata.ino() {
        return Err(EvidenceError::Io);
    }
    if require_executable && metadata.permissions().mode() & 0o111 == 0 {
        return Err(EvidenceError::Unrepresentable);
    }
    let (file_type, digest, contents) = if metadata.is_file() {
        if metadata.len() > cap as u64 {
            return Err(EvidenceError::Oversized);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        let mut chunk = [0_u8; EVIDENCE_READ_CHUNK_BYTES];
        loop {
            ensure_before(deadline)?;
            let remaining = cap.saturating_add(1).saturating_sub(bytes.len());
            if remaining == 0 {
                return Err(EvidenceError::Oversized);
            }
            let chunk_len = remaining.min(chunk.len());
            let read = file.read(&mut chunk[..chunk_len]);
            ensure_before(deadline)?;
            let read = read.map_err(|_| EvidenceError::Io)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
        }
        if bytes.len() > cap || bytes.len() as u64 != metadata.len() {
            return Err(EvidenceError::Oversized);
        }
        let digest = Sha256::digest(&bytes).into();
        (ClosedFileType::Regular, digest, Some(bytes))
    } else if metadata.is_dir() {
        (ClosedFileType::Directory, [0; 32], None)
    } else {
        return Err(EvidenceError::Unrepresentable);
    };
    let after = checked_io(deadline, || file.metadata())?.map_err(|_| EvidenceError::Io)?;
    if metadata.dev() != after.dev()
        || metadata.ino() != after.ino()
        || metadata.len() != after.len()
        || metadata.modified().ok() != after.modified().ok()
    {
        return Err(EvidenceError::Io);
    }
    let path_digest = if include_path_digest {
        Sha256::digest(path.as_os_str().as_bytes()).into()
    } else {
        [0; 32]
    };
    Ok((
        FileEvidence {
            slot,
            presence: Presence::Present,
            file_type,
            stable_identity: StableFileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            len: metadata.len(),
            modified_ns: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
            digest,
            path_digest,
        },
        contents,
    ))
}

fn read_closed_file(
    path: &Path,
    cap: usize,
    require_executable: bool,
    deadline: Instant,
) -> Result<Vec<u8>, EvidenceError> {
    let (evidence, contents) = open_file_snapshot(
        path,
        DependencySlot::GitMarker,
        cap,
        require_executable,
        false,
        deadline,
    )?;
    if evidence.file_type != ClosedFileType::Regular {
        return Err(EvidenceError::Unrepresentable);
    }
    contents.ok_or(EvidenceError::Unrepresentable)
}

fn ensure_closed_absolute_path(path: &Path) -> Result<(), EvidenceError> {
    if !path.is_absolute() || path.as_os_str().as_bytes().len() > MAX_CLOSED_PATH_BYTES {
        return Err(EvidenceError::Unrepresentable);
    }
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(_) => {
                depth += 1;
                if depth > MAX_COMPONENT_DEPTH {
                    return Err(EvidenceError::Unrepresentable);
                }
            }
            _ => return Err(EvidenceError::Unrepresentable),
        }
    }
    Ok(())
}

fn ensure_bounded_candidate_path(path: &Path) -> Result<(), EvidenceError> {
    if !path.is_absolute() || path.as_os_str().as_bytes().len() > MAX_CLOSED_PATH_BYTES {
        return Err(EvidenceError::Unrepresentable);
    }
    let component_count = path
        .components()
        .filter(|component| !matches!(component, Component::RootDir))
        .count();
    if component_count > MAX_COMPONENT_DEPTH {
        return Err(EvidenceError::Unrepresentable);
    }
    Ok(())
}

fn matching_refresh(
    root: &Path,
    identity: &ProjectIdentity,
    provenance: ProjectProvenance,
    pre: Result<DependencyEvidenceV1, EvidenceError>,
    post: Result<DependencyEvidenceV1, EvidenceError>,
) -> Option<CacheRefresh> {
    let (Ok(pre), Ok(post)) = (pre, post) else {
        return None;
    };
    let pre = serde_json::to_vec(&pre).ok()?;
    let post = serde_json::to_vec(&post).ok()?;
    if pre != post {
        return None;
    }
    let ProjectId::Stable(project_uuid) = identity.id() else {
        return None;
    };
    let provenance = match provenance {
        ProjectProvenance::Manifest => CacheProvenance::Manifest,
        ProjectProvenance::NetworkRemote => CacheProvenance::NetworkRemote,
        ProjectProvenance::Temporary => return None,
    };
    Some(CacheRefresh {
        root: root.to_path_buf(),
        project_uuid: project_uuid.clone(),
        provenance,
        evidence: pre,
    })
}

pub(crate) fn refresh_after_activity_success(
    writer: &mut RuntimeCacheWriter,
    refresh: &CacheRefresh,
    refresh_order: i64,
) -> Result<(), RuntimeCacheBypass> {
    writer.upsert_and_prune(&refresh.as_row(refresh_order)?)
}

fn config_origins_are_closed(
    output: &[u8],
    remote_output: &[u8],
    root: &Path,
    environment: &DiscoveryEnvironment,
    selected_git: &SelectedGit,
    deadline: Instant,
) -> Result<bool, EvidenceError> {
    ensure_before(deadline)?;
    if environment.platform == DiscoveryPlatform::Unknown
        || environment.has_path_changing_override()
    {
        return Ok(false);
    }
    let expected_remote = trim_line(remote_output).ok_or(EvidenceError::Unrepresentable)?;
    let slots = resolve_git_slots(root, deadline)?;
    let (global, xdg_global) = environment.global_paths()?;
    let mut candidates = vec![
        (DependencySlot::CommonConfig, slots.common_config, "local"),
        (
            DependencySlot::WorktreeConfig,
            slots.worktree_config,
            "worktree",
        ),
        (DependencySlot::GlobalConfig, global, "global"),
        (DependencySlot::XdgGlobalConfig, xdg_global, "global"),
    ];
    for (index, path) in selected_git.system_config_candidates.iter().enumerate() {
        candidates.push((
            [
                DependencySlot::SystemConfig,
                DependencySlot::SystemConfigAlt,
                DependencySlot::SystemConfigHomebrew,
            ][index],
            path.clone(),
            "system",
        ));
    }
    let fields = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    if fields.last() != Some(&&[][..]) || (fields.len() - 1) % 3 != 0 {
        return Ok(false);
    }
    let records = (fields.len() - 1) / 3;
    if records > MAX_CONFIG_RECORDS {
        return Ok(false);
    }
    let mut matching_remote_records = 0usize;
    for record in fields[..fields.len() - 1].chunks_exact(3) {
        ensure_before(deadline)?;
        let scope = std::str::from_utf8(record[0]).map_err(|_| EvidenceError::Unrepresentable)?;
        let origin = std::str::from_utf8(record[1]).map_err(|_| EvidenceError::Unrepresentable)?;
        let Some(separator) = record[2].iter().position(|byte| *byte == b'\n') else {
            return Ok(false);
        };
        let key = std::str::from_utf8(&record[2][..separator])
            .map_err(|_| EvidenceError::Unrepresentable)?
            .to_ascii_lowercase();
        let value = &record[2][separator + 1..];
        if key == "include.path" || key.starts_with("includeif.") {
            return Ok(false);
        }
        let Some(origin) = origin.strip_prefix("file:") else {
            return Ok(false);
        };
        let origin = resolve_origin_path(origin, root, environment, deadline)?;
        let matched = candidates.iter().any(|(_, candidate, expected_scope)| {
            scope == *expected_scope
                && same_existing_path(&origin, candidate, deadline).unwrap_or(false)
        });
        if !matched {
            return Ok(false);
        }
        if key == "remote.origin.url" {
            if value != expected_remote {
                return Ok(false);
            }
            matching_remote_records += 1;
        }
    }
    ensure_before(deadline)?;
    Ok(matching_remote_records == 1)
}

fn resolve_origin_path(
    value: &str,
    root: &Path,
    environment: &DiscoveryEnvironment,
    deadline: Instant,
) -> Result<PathBuf, EvidenceError> {
    ensure_before(deadline)?;
    let resolved = if let Some(relative) = value.strip_prefix("~/") {
        environment.home.join(relative)
    } else {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    };
    ensure_closed_absolute_path(&resolved)?;
    Ok(resolved)
}

fn same_existing_path(left: &Path, right: &Path, deadline: Instant) -> Result<bool, EvidenceError> {
    let left = match checked_io(deadline, || fs::canonicalize(left))? {
        Ok(left) => left,
        Err(_) => return Ok(false),
    };
    let right = match checked_io(deadline, || fs::canonicalize(right))? {
        Ok(right) => right,
        Err(_) => return Ok(false),
    };
    Ok(left == right)
}

#[cfg(test)]
mod tests;
