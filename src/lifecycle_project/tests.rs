use super::*;

use std::cell::Cell;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::{ProjectCommandError, ProjectId, ProjectProvenance};
use tempfile::TempDir;

use crate::brain::storage::{CacheDeadline, RuntimeCacheReader, RuntimeCacheWriter, StoragePaths};
use crate::lifecycle_timing::HookBudget;
use crate::provider_hooks::OutputBudget;

const PROJECT_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[derive(Clone)]
struct ControlledClock(Rc<Cell<Instant>>);

impl ControlledClock {
    fn new(now: Instant) -> Self {
        Self(Rc::new(Cell::new(now)))
    }

    fn advance(&self, duration: Duration) {
        self.0.set(self.0.get() + duration);
    }
}

impl MonotonicClock for ControlledClock {
    fn now(&self) -> Instant {
        self.0.get()
    }
}

#[derive(Clone)]
struct Reply {
    args: Vec<&'static str>,
    output: Result<Vec<u8>, ProjectCommandError>,
    mutate_after: Option<(PathBuf, Vec<u8>)>,
}

struct CountingRunner {
    replies: VecDeque<Reply>,
    calls: Vec<(Vec<String>, Instant, usize)>,
}

impl CountingRunner {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: replies.into_iter().collect(),
            calls: Vec::new(),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.len()
    }
}

impl LifecycleProjectRunner for CountingRunner {
    fn output(
        &mut self,
        _executable: &Path,
        _cwd: &Path,
        args: &[&str],
        deadline: Instant,
        output: &mut OutputBudget,
    ) -> Result<Vec<u8>, ProjectCommandError> {
        self.calls.push((
            args.iter().map(|arg| (*arg).to_owned()).collect(),
            deadline,
            output.remaining(),
        ));
        let reply = self.replies.pop_front().expect("unexpected Git call");
        assert_eq!(reply.args, args);
        if let Some((path, bytes)) = reply.mutate_after {
            fs::write(path, bytes).unwrap();
        }
        reply.output
    }
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    cwd: PathBuf,
    paths: CodingBrainPaths,
    storage: StoragePaths,
    environment: DiscoveryEnvironment,
    git_config: PathBuf,
    manifest: PathBuf,
    git_executable: PathBuf,
    system_config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let cwd = root.join("src/nested");
        let home = temp.path().join("home");
        let state = temp.path().join("state");
        let bin = temp.path().join("bin");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let git_config = root.join(".git/config");
        fs::write(
            &git_config,
            b"[remote \"origin\"]\n\turl = https://example.test/acme/repo.git\n",
        )
        .unwrap();
        let git_executable = bin.join("git");
        fs::write(&git_executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&git_executable, fs::Permissions::from_mode(0o755)).unwrap();
        let system_config = temp.path().join("etc-gitconfig");
        let paths =
            CodingBrainPaths::resolve(&PathEnvironment::new(None, Some(state), Some(home.clone())))
                .unwrap();
        let storage = StoragePaths::at(paths.state_root());
        let manifest = root.join(".coding-brain/project.toml");
        let environment = DiscoveryEnvironment::fixture(
            DiscoveryPlatform::Linux,
            home,
            None,
            bin.as_os_str().to_owned(),
            vec![system_config.clone()],
        );
        Self {
            _temp: temp,
            root,
            cwd,
            paths,
            storage,
            environment,
            git_config,
            manifest,
            git_executable,
            system_config,
        }
    }

    fn write_manifest(&self) {
        fs::create_dir_all(self.manifest.parent().unwrap()).unwrap();
        fs::write(
            &self.manifest,
            format!("schema_version = 1\nproject_id = \"{PROJECT_UUID}\"\n"),
        )
        .unwrap();
    }

    fn root_reply(&self) -> Reply {
        Reply {
            args: vec!["rev-parse", "--show-toplevel"],
            output: Ok(format!("{}\n", self.root.display()).into_bytes()),
            mutate_after: None,
        }
    }

    fn root_replies(&self) -> Vec<Reply> {
        vec![self.root_reply(), self.root_reply()]
    }

    fn network_replies(&self) -> Vec<Reply> {
        vec![
            self.root_reply(),
            Reply {
                args: vec!["remote", "get-url", "origin"],
                output: Ok(b"https://example.test/acme/repo.git\n".to_vec()),
                mutate_after: None,
            },
            Reply {
                args: vec!["config", "--list", "--show-origin", "--show-scope", "-z"],
                output: Ok(config_output(&[(
                    "local",
                    "file:.git/config",
                    "remote.origin.url",
                    "https://example.test/acme/repo.git",
                )])),
                mutate_after: None,
            },
            self.root_reply(),
        ]
    }

    fn resolve(
        &self,
        reader: Option<&RuntimeCacheReader>,
        runner: &mut CountingRunner,
    ) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
        self.resolve_at(
            &self.cwd,
            reader,
            &HookBudget::from_start(Instant::now()),
            runner,
        )
    }

    fn resolve_at<C: MonotonicClock>(
        &self,
        cwd: &Path,
        reader: Option<&RuntimeCacheReader>,
        budget: &HookBudget<C>,
        runner: &mut CountingRunner,
    ) -> Result<ResolvedLifecycleProject, LifecycleProjectError> {
        resolve_lifecycle_project_with_environment(
            cwd,
            &self.paths,
            reader,
            budget,
            runner,
            &self.environment,
        )
    }

    fn install(&self, refresh: CacheRefresh) {
        let mut writer = RuntimeCacheWriter::create_or_open_after_activity(
            &self.storage,
            CacheDeadline::after(Duration::from_millis(100)),
        )
        .unwrap();
        refresh_after_activity_success(&mut writer, &refresh, 1).unwrap();
    }

    fn reader(&self) -> RuntimeCacheReader {
        RuntimeCacheReader::open_existing_read_only(
            &self.storage,
            CacheDeadline::after(Duration::from_millis(100)),
        )
        .unwrap()
    }
}

fn config_output(records: &[(&str, &str, &str, &str)]) -> Vec<u8> {
    let mut output = Vec::new();
    for record in records {
        for field in [record.0, record.1] {
            output.extend_from_slice(field.as_bytes());
            output.push(0);
        }
        output.extend_from_slice(record.2.as_bytes());
        output.push(b'\n');
        output.extend_from_slice(record.3.as_bytes());
        output.push(0);
    }
    output
}

#[test]
fn explicit_project_deadline_is_shared_by_every_git_command() {
    let fixture = Fixture::new();
    fixture.write_manifest();
    let mut runner = CountingRunner::new(fixture.root_replies());
    let deadline = Instant::now() + Duration::from_secs(1);

    let resolved = resolve_lifecycle_project_until_with_environment(
        &fixture.cwd,
        &fixture.paths,
        None,
        &mut runner,
        &fixture.environment,
        deadline,
    )
    .unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::Manifest);
    assert!(!runner.calls.is_empty());
    assert!(runner.calls.iter().all(|call| call.1 == deadline));
}

#[test]
fn cold_cache_stage_reports_miss() {
    let fixture = Fixture::new();
    fixture.write_manifest();
    let mut runner = CountingRunner::new(fixture.root_replies());
    let mut cache_outcome = None;

    let resolved = resolve_lifecycle_project_until_with_environment_and_cache_stage(
        &fixture.cwd,
        &fixture.paths,
        None,
        &mut runner,
        &fixture.environment,
        Instant::now() + Duration::from_secs(1),
        |outcome| cache_outcome = Some(outcome),
    )
    .unwrap();

    assert_eq!(cache_outcome, Some(ProjectCacheOutcome::Miss));
    assert_eq!(resolved.provenance, ProjectProvenance::Manifest);
}

fn root_reply(root: &Path) -> Reply {
    Reply {
        args: vec!["rev-parse", "--show-toplevel"],
        output: Ok(format!("{}\n", root.display()).into_bytes()),
        mutate_after: None,
    }
}

fn linked_network_replies(root: &Path, common_config: &Path, worktree_config: &Path) -> Vec<Reply> {
    let common_origin = format!("file:{}", common_config.display());
    let worktree_origin = format!("file:{}", worktree_config.display());
    vec![
        root_reply(root),
        Reply {
            args: vec!["remote", "get-url", "origin"],
            output: Ok(b"https://example.test/acme/repo.git\n".to_vec()),
            mutate_after: None,
        },
        Reply {
            args: vec!["config", "--list", "--show-origin", "--show-scope", "-z"],
            output: Ok(config_output(&[
                (
                    "local",
                    &common_origin,
                    "remote.origin.url",
                    "https://example.test/acme/repo.git",
                ),
                (
                    "worktree",
                    &worktree_origin,
                    "extensions.worktreeconfig",
                    "true",
                ),
            ])),
            mutate_after: None,
        },
        root_reply(root),
    ]
}

fn create_linked_worktree(root: &Path, common: &Path, name: &str) -> PathBuf {
    let git_directory = common.join("worktrees").join(name);
    fs::create_dir_all(&git_directory).unwrap();
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join(".git"),
        format!("gitdir: {}\n", git_directory.display()),
    )
    .unwrap();
    fs::write(git_directory.join("commondir"), b"../..\n").unwrap();
    let worktree_config = git_directory.join("config.worktree");
    fs::write(&worktree_config, b"[extensions]\nworktreeConfig = true\n").unwrap();
    worktree_config
}

#[test]
fn valid_manifest_and_network_rows_hit_without_git_or_writes() {
    for manifest in [true, false] {
        let fixture = Fixture::new();
        if manifest {
            fixture.write_manifest();
        }
        let replies = if manifest {
            fixture.root_replies()
        } else {
            fixture.network_replies()
        };
        let first = fixture
            .resolve(None, &mut CountingRunner::new(replies))
            .unwrap();
        let expected_identity = first.identity.clone();
        fixture.install(first.refresh.expect("cold result must be cacheable"));
        let reader = fixture.reader();
        let key = CacheRootKey::from_canonical_path(&fixture.root).unwrap();
        let refresh_order_before = reader.load_selected_row(&key).unwrap().refresh_order();
        let mut hit_runner = CountingRunner::new([]);

        let hit = fixture.resolve(Some(&reader), &mut hit_runner).unwrap();

        assert_eq!(hit.cache_outcome, ProjectCacheOutcome::Hit);
        assert_eq!(hit_runner.call_count(), 0);
        assert_eq!(
            hit.provenance,
            if manifest {
                ProjectProvenance::Manifest
            } else {
                ProjectProvenance::NetworkRemote
            }
        );
        assert_eq!(hit.identity.id(), expected_identity.id());
        assert!(hit.refresh.is_none());
        assert_eq!(
            reader.load_selected_row(&key).unwrap().refresh_order(),
            refresh_order_before
        );
    }
}

#[derive(Clone, Copy)]
enum AuthorityMutation {
    ReplaceConfig,
    DeleteConfig,
    OversizeConfig,
    PermissionDenied,
    SameSizeRewrite,
    GlobalConfig,
    SystemConfig,
    Environment,
    GitWrapper,
    ConfigDirectory,
    MalformedGitMarker,
    NestedGit,
    NestedManifest,
}

#[test]
fn valid_hit_and_every_authority_change_have_closed_outcomes() {
    let cases = [
        (
            "replaced",
            AuthorityMutation::ReplaceConfig,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "deleted",
            AuthorityMutation::DeleteConfig,
            ProjectCacheOutcome::NonCacheable,
            false,
        ),
        (
            "oversized",
            AuthorityMutation::OversizeConfig,
            ProjectCacheOutcome::NonCacheable,
            false,
        ),
        (
            "permission_denied",
            AuthorityMutation::PermissionDenied,
            ProjectCacheOutcome::NonCacheable,
            false,
        ),
        (
            "same_size_rewrite",
            AuthorityMutation::SameSizeRewrite,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "global_config",
            AuthorityMutation::GlobalConfig,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "system_config",
            AuthorityMutation::SystemConfig,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "environment",
            AuthorityMutation::Environment,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "path_wrapper",
            AuthorityMutation::GitWrapper,
            ProjectCacheOutcome::Invalid,
            true,
        ),
        (
            "config_directory",
            AuthorityMutation::ConfigDirectory,
            ProjectCacheOutcome::NonCacheable,
            false,
        ),
        (
            "malformed_git_marker",
            AuthorityMutation::MalformedGitMarker,
            ProjectCacheOutcome::NonCacheable,
            false,
        ),
        (
            "nested_git",
            AuthorityMutation::NestedGit,
            ProjectCacheOutcome::DiscoveryFailure,
            false,
        ),
        (
            "nested_manifest",
            AuthorityMutation::NestedManifest,
            ProjectCacheOutcome::DiscoveryFailure,
            false,
        ),
    ];
    for (name, mutation, expected_outcome, expected_refresh) in cases {
        let mut fixture = Fixture::new();
        let first = fixture
            .resolve(None, &mut CountingRunner::new(fixture.network_replies()))
            .unwrap();
        fixture.install(first.refresh.unwrap());
        apply_mutation(&mut fixture, mutation);
        let reader = fixture.reader();
        let mut git = CountingRunner::new(fixture.network_replies());

        let outcome = fixture.resolve(Some(&reader), &mut git).unwrap();

        assert_eq!(outcome.cache_outcome, expected_outcome, "{name}");
        assert_eq!(git.call_count(), 4, "{name}");
        assert_eq!(outcome.refresh.is_some(), expected_refresh, "{name}");
        let expected_provenance = if matches!(
            mutation,
            AuthorityMutation::NestedGit | AuthorityMutation::NestedManifest
        ) {
            ProjectProvenance::Temporary
        } else {
            ProjectProvenance::NetworkRemote
        };
        assert_eq!(outcome.provenance, expected_provenance, "{name}");
    }
}

fn apply_mutation(fixture: &mut Fixture, mutation: AuthorityMutation) {
    match mutation {
        AuthorityMutation::ReplaceConfig => {
            fs::remove_file(&fixture.git_config).unwrap();
            fs::write(&fixture.git_config, b"replacement config\n").unwrap();
        }
        AuthorityMutation::DeleteConfig => fs::remove_file(&fixture.git_config).unwrap(),
        AuthorityMutation::OversizeConfig => {
            fs::write(
                &fixture.git_config,
                vec![b'x'; MAX_AUTHORITY_FILE_BYTES + 1],
            )
            .unwrap();
        }
        AuthorityMutation::PermissionDenied => {
            fs::set_permissions(&fixture.git_config, fs::Permissions::from_mode(0o000)).unwrap();
        }
        AuthorityMutation::SameSizeRewrite => {
            let len = fs::metadata(&fixture.git_config).unwrap().len() as usize;
            fs::write(&fixture.git_config, vec![b'z'; len]).unwrap();
        }
        AuthorityMutation::GlobalConfig => {
            fs::write(
                fixture.environment.home.join(".gitconfig"),
                b"[user]\nname=x\n",
            )
            .unwrap();
        }
        AuthorityMutation::SystemConfig => {
            fs::write(&fixture.system_config, b"[user]\nname=x\n").unwrap();
        }
        AuthorityMutation::Environment => {
            fixture.environment.extra.push(("LANG".into(), "C".into()));
        }
        AuthorityMutation::GitWrapper => {
            fs::write(&fixture.git_executable, b"#!/bin/sh\nexit 1\n").unwrap();
        }
        AuthorityMutation::ConfigDirectory => {
            fs::remove_file(&fixture.git_config).unwrap();
            fs::create_dir(&fixture.git_config).unwrap();
        }
        AuthorityMutation::MalformedGitMarker => {
            fs::remove_dir_all(fixture.root.join(".git")).unwrap();
            fs::write(fixture.root.join(".git"), b"not a gitdir pointer\n").unwrap();
        }
        AuthorityMutation::NestedGit => {
            fs::create_dir_all(fixture.cwd.join(".git")).unwrap();
        }
        AuthorityMutation::NestedManifest => {
            let path = fixture.cwd.join(".coding-brain/project.toml");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!("schema_version = 1\nproject_id = \"{PROJECT_UUID}\"\n"),
            )
            .unwrap();
        }
    }
}

#[test]
fn synchronized_pre_post_mutation_uses_identity_once_without_refresh() {
    let fixture = Fixture::new();
    let mut replies = fixture.network_replies();
    replies[2].mutate_after = Some((
        fixture.git_config.clone(),
        b"[remote \"origin\"]\n\turl = https://example.test/changed/repo.git\n".to_vec(),
    ));
    let mut runner = CountingRunner::new(replies);

    let resolved = fixture.resolve(None, &mut runner).unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::NetworkRemote);
    assert!(matches!(resolved.identity.id(), ProjectId::Stable(_)));
    assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::NonCacheable);
    assert!(resolved.refresh.is_none());
    assert_eq!(runner.call_count(), 4);
}

#[test]
fn root_confirmations_enclose_the_pre_post_evidence_window() {
    let fixture = Fixture::new();
    let mut replies = fixture.network_replies();
    replies[2].mutate_after = Some((
        fixture.git_config.clone(),
        b"[remote \"origin\"]\nurl = https://example.test/acme/repo.git\n# changed\n".to_vec(),
    ));
    let mut runner = CountingRunner::new(replies);

    let resolved = fixture.resolve(None, &mut runner).unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::NetworkRemote);
    assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::NonCacheable);
    assert!(resolved.refresh.is_none());
    assert_eq!(runner.call_count(), 4);

    let different = fixture._temp.path().join("different-root");
    fs::create_dir_all(&different).unwrap();
    let mut mismatch_replies = fixture.network_replies();
    mismatch_replies[3] = root_reply(&different);
    let mut mismatch_runner = CountingRunner::new(mismatch_replies);
    let mismatch = fixture.resolve(None, &mut mismatch_runner).unwrap();
    assert_eq!(mismatch.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        mismatch.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(mismatch.refresh.is_none());
    assert_eq!(mismatch_runner.call_count(), 4);
}

#[test]
fn same_size_manifest_authority_rewrite_invalidates_on_the_next_hook() {
    let fixture = Fixture::new();
    fixture.write_manifest();
    let cold = fixture
        .resolve(None, &mut CountingRunner::new(fixture.root_replies()))
        .unwrap();
    fixture.install(cold.refresh.unwrap());
    let replacement_uuid = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    fs::write(
        &fixture.manifest,
        format!("schema_version = 1\nproject_id = \"{replacement_uuid}\"\n"),
    )
    .unwrap();
    let reader = fixture.reader();
    let mut runner = CountingRunner::new(fixture.root_replies());

    let resolved = fixture.resolve(Some(&reader), &mut runner).unwrap();

    assert_ne!(resolved.cache_outcome, ProjectCacheOutcome::Hit);
    assert_eq!(runner.call_count(), 2);
    assert_eq!(
        resolved.identity.id(),
        &ProjectId::Stable(replacement_uuid.into())
    );
    assert!(resolved.refresh.is_some());
}

#[test]
fn dynamic_origins_overrides_bad_path_and_unknown_platform_never_refresh() {
    enum Case {
        Include,
        IncludeIf,
        NonFile,
        MalformedConfigOutput,
        UnknownOrigin,
        MissingOriginRemote,
        AmbiguousOriginRemote,
        MismatchedOriginRemote,
        GitDir,
        GitWorkTree,
        GitCommonDir,
        GitConfigCount,
        RelativePath,
        EmptyPath,
        AmbiguousGit,
        ExcessivePathComponents,
        OversizedEnvironment,
        IncompleteEnvironment,
        UnknownPlatform,
        UnsupportedGitLayout,
    }
    for case in [
        Case::Include,
        Case::IncludeIf,
        Case::NonFile,
        Case::MalformedConfigOutput,
        Case::UnknownOrigin,
        Case::MissingOriginRemote,
        Case::AmbiguousOriginRemote,
        Case::MismatchedOriginRemote,
        Case::GitDir,
        Case::GitWorkTree,
        Case::GitCommonDir,
        Case::GitConfigCount,
        Case::RelativePath,
        Case::EmptyPath,
        Case::AmbiguousGit,
        Case::ExcessivePathComponents,
        Case::OversizedEnvironment,
        Case::IncompleteEnvironment,
        Case::UnknownPlatform,
        Case::UnsupportedGitLayout,
    ] {
        let mut fixture = Fixture::new();
        let mut replies = fixture.network_replies();
        match case {
            Case::Include => {
                replies[2].output = Ok(config_output(&[(
                    "local",
                    "file:.git/config",
                    "include.path",
                    "/tmp/other",
                )]))
            }
            Case::IncludeIf => {
                replies[2].output = Ok(config_output(&[(
                    "local",
                    "file:.git/config",
                    "includeIf.gitdir:foo.path",
                    "/tmp/other",
                )]))
            }
            Case::NonFile => {
                replies[2].output = Ok(config_output(&[(
                    "command",
                    "command line:",
                    "user.name",
                    "x",
                )]))
            }
            Case::MalformedConfigOutput => replies[2].output = Ok(b"malformed".to_vec()),
            Case::UnknownOrigin => {
                replies[2].output = Ok(config_output(&[(
                    "local",
                    "file:/tmp/other",
                    "user.name",
                    "x",
                )]))
            }
            Case::MissingOriginRemote => {
                replies[2].output = Ok(config_output(&[(
                    "local",
                    "file:.git/config",
                    "user.name",
                    "x",
                )]))
            }
            Case::AmbiguousOriginRemote => {
                replies[2].output = Ok(config_output(&[
                    (
                        "local",
                        "file:.git/config",
                        "remote.origin.url",
                        "https://example.test/acme/repo.git",
                    ),
                    (
                        "local",
                        "file:.git/config",
                        "remote.origin.url",
                        "https://example.test/acme/repo.git",
                    ),
                ]))
            }
            Case::MismatchedOriginRemote => {
                replies[2].output = Ok(config_output(&[(
                    "local",
                    "file:.git/config",
                    "remote.origin.url",
                    "https://example.test/other/repo.git",
                )]))
            }
            Case::GitDir => fixture
                .environment
                .extra
                .push(("GIT_DIR".into(), "/tmp/git".into())),
            Case::GitWorkTree => fixture
                .environment
                .extra
                .push(("GIT_WORK_TREE".into(), "/tmp/tree".into())),
            Case::GitCommonDir => fixture
                .environment
                .extra
                .push(("GIT_COMMON_DIR".into(), "/tmp/common".into())),
            Case::GitConfigCount => fixture
                .environment
                .extra
                .push(("GIT_CONFIG_COUNT".into(), "1".into())),
            Case::RelativePath => fixture.environment.path = "relative".into(),
            Case::EmptyPath => {
                fixture.environment.path =
                    format!(":{}", fixture.git_executable.parent().unwrap().display()).into()
            }
            Case::AmbiguousGit => {
                let second = fixture._temp.path().join("bin2");
                fs::create_dir(&second).unwrap();
                let git = second.join("git");
                fs::write(&git, b"#!/bin/sh\nexit 0\n").unwrap();
                fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
                fixture.environment.path = format!(
                    "{}:{}",
                    fixture.git_executable.parent().unwrap().display(),
                    second.display()
                )
                .into();
            }
            Case::ExcessivePathComponents => {
                let mut components = (0..=256)
                    .map(|index| fixture._temp.path().join(format!("missing-{index}")))
                    .collect::<Vec<_>>();
                components.push(fixture.git_executable.parent().unwrap().to_path_buf());
                fixture.environment.path = std::env::join_paths(components).unwrap();
            }
            Case::OversizedEnvironment => {
                fixture
                    .environment
                    .extra
                    .push(("LANG".into(), OsString::from_vec(vec![b'x'; 64 * 1024 + 1])));
            }
            Case::IncompleteEnvironment => fixture.environment.extra_complete = false,
            Case::UnknownPlatform => fixture.environment.platform = DiscoveryPlatform::Unknown,
            Case::UnsupportedGitLayout => fixture.environment.supported_git_executable = None,
        }
        let mut runner = CountingRunner::new(replies);
        let resolved = fixture.resolve(None, &mut runner).unwrap();
        assert_eq!(resolved.provenance, ProjectProvenance::NetworkRemote);
        assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::NonCacheable);
        assert!(resolved.refresh.is_none());
        assert_eq!(runner.call_count(), 4);
    }
}

#[test]
fn temporary_failure_excessive_depth_and_malformed_cache_never_hit_or_refresh() {
    let fixture = Fixture::new();
    let failure = Reply {
        args: vec!["rev-parse", "--show-toplevel"],
        output: Err(ProjectCommandError::Timeout),
        mutate_after: None,
    };
    let temporary = fixture
        .resolve(None, &mut CountingRunner::new([failure]))
        .unwrap();
    assert_eq!(temporary.provenance, ProjectProvenance::Temporary);
    assert!(temporary.refresh.is_none());

    fixture.write_manifest();
    let manifest_failure = Reply {
        args: vec!["rev-parse", "--show-toplevel"],
        output: Err(ProjectCommandError::Timeout),
        mutate_after: None,
    };
    let failed_manifest = fixture
        .resolve(None, &mut CountingRunner::new([manifest_failure]))
        .unwrap();
    assert_eq!(failed_manifest.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        failed_manifest.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(failed_manifest.refresh.is_none());
    fs::remove_file(&fixture.manifest).unwrap();

    let first = fixture
        .resolve(None, &mut CountingRunner::new(fixture.network_replies()))
        .unwrap();
    let mut bad = first.refresh.unwrap();
    bad.evidence = b"not valid evidence".to_vec();
    fixture.install(bad);
    let reader = fixture.reader();
    let mut runner = CountingRunner::new(fixture.network_replies());
    let resolved = fixture.resolve(Some(&reader), &mut runner).unwrap();
    assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::Bypassed);
    assert_eq!(runner.call_count(), 4);
    assert!(resolved.refresh.is_none());

    let mut failed_runner = CountingRunner::new([
        fixture.root_reply(),
        Reply {
            args: vec!["remote", "get-url", "origin"],
            output: Err(ProjectCommandError::Timeout),
            mutate_after: None,
        },
    ]);
    let failed = fixture.resolve(Some(&reader), &mut failed_runner).unwrap();
    assert_eq!(failed.provenance, ProjectProvenance::Temporary);
    assert_eq!(failed.cache_outcome, ProjectCacheOutcome::Bypassed);
    assert!(failed.refresh.is_none());
    assert_eq!(failed_runner.call_count(), 2);
}

#[test]
fn most_specific_row_wins_and_invalid_selected_row_never_falls_outward() {
    let fixture = Fixture::new();
    let outer = fixture
        .resolve(None, &mut CountingRunner::new(fixture.network_replies()))
        .unwrap();
    fixture.install(outer.refresh.unwrap());

    let inner = fixture.root.join("src");
    fs::create_dir_all(inner.join(".git")).unwrap();
    let inner_manifest = inner.join(".coding-brain/project.toml");
    fs::create_dir_all(inner_manifest.parent().unwrap()).unwrap();
    fs::write(
        &inner_manifest,
        format!("schema_version = 1\nproject_id = \"{PROJECT_UUID}\"\n"),
    )
    .unwrap();
    let inner_cold = fixture
        .resolve_at(
            &fixture.cwd,
            None,
            &HookBudget::from_start(Instant::now()),
            &mut CountingRunner::new([root_reply(&inner), root_reply(&inner)]),
        )
        .unwrap();
    assert_eq!(inner_cold.root, inner);
    fixture.install(inner_cold.refresh.unwrap());

    let reader = fixture.reader();
    let mut hit_runner = CountingRunner::new([]);
    let hit = fixture
        .resolve_at(
            &fixture.cwd,
            Some(&reader),
            &HookBudget::from_start(Instant::now()),
            &mut hit_runner,
        )
        .unwrap();
    assert_eq!(hit.cache_outcome, ProjectCacheOutcome::Hit);
    assert_eq!(hit.root, inner);
    assert_eq!(hit_runner.call_count(), 0);
    drop(reader);

    fixture.install(CacheRefresh {
        root: inner,
        project_uuid: PROJECT_UUID.into(),
        provenance: CacheProvenance::Manifest,
        evidence: b"malformed selected evidence".to_vec(),
    });
    let reader = fixture.reader();
    let mut miss_runner = CountingRunner::new(fixture.network_replies());
    let miss = fixture
        .resolve_at(
            &fixture.cwd,
            Some(&reader),
            &HookBudget::from_start(Instant::now()),
            &mut miss_runner,
        )
        .unwrap();
    assert_ne!(miss.cache_outcome, ProjectCacheOutcome::Hit);
    assert_eq!(miss.cache_outcome, ProjectCacheOutcome::Bypassed);
    assert_eq!(miss_runner.call_count(), 4);
    assert!(miss.refresh.is_none());
}

#[test]
fn separate_linked_worktrees_keep_closed_rows_and_worktree_config_evidence() {
    let mut fixture = Fixture::new();
    fs::remove_dir_all(fixture.root.join(".git")).unwrap();
    let common = fixture._temp.path().join("common.git");
    fs::create_dir_all(&common).unwrap();
    let common_config = common.join("config");
    fs::write(
        &common_config,
        b"[remote \"origin\"]\nurl = https://example.test/acme/repo.git\n",
    )
    .unwrap();
    fixture.git_config = common_config.clone();

    let first_config = create_linked_worktree(&fixture.root, &common, "one");
    let second_root = fixture._temp.path().join("repo-two");
    let second_cwd = second_root.join("src");
    fs::create_dir_all(&second_cwd).unwrap();
    let second_config = create_linked_worktree(&second_root, &common, "two");

    let first = fixture
        .resolve_at(
            &fixture.cwd,
            None,
            &HookBudget::from_start(Instant::now()),
            &mut CountingRunner::new(linked_network_replies(
                &fixture.root,
                &common_config,
                &first_config,
            )),
        )
        .unwrap();
    let second = fixture
        .resolve_at(
            &second_cwd,
            None,
            &HookBudget::from_start(Instant::now()),
            &mut CountingRunner::new(linked_network_replies(
                &second_root,
                &common_config,
                &second_config,
            )),
        )
        .unwrap();
    assert_eq!(first.identity, second.identity);
    assert_ne!(first.root, second.root);
    fixture.install(first.refresh.unwrap());
    fixture.install(second.refresh.unwrap());

    let reader = fixture.reader();
    for (cwd, expected_root) in [(&fixture.cwd, &fixture.root), (&second_cwd, &second_root)] {
        let mut runner = CountingRunner::new([]);
        let hit = fixture
            .resolve_at(
                cwd,
                Some(&reader),
                &HookBudget::from_start(Instant::now()),
                &mut runner,
            )
            .unwrap();
        assert_eq!(hit.cache_outcome, ProjectCacheOutcome::Hit);
        assert_eq!(&hit.root, expected_root);
        assert_eq!(runner.call_count(), 0);
    }

    fs::write(&first_config, b"[extensions]\nworktreeConfig = false\n").unwrap();
    let mut invalidated_runner = CountingRunner::new(linked_network_replies(
        &fixture.root,
        &common_config,
        &first_config,
    ));
    let invalidated = fixture
        .resolve_at(
            &fixture.cwd,
            Some(&reader),
            &HookBudget::from_start(Instant::now()),
            &mut invalidated_runner,
        )
        .unwrap();
    assert_ne!(invalidated.cache_outcome, ProjectCacheOutcome::Hit);
    assert_eq!(invalidated_runner.call_count(), 4);
}

#[test]
fn excessive_nested_cwd_depth_bypasses_the_cached_ancestor() {
    let fixture = Fixture::new();
    let cold = fixture
        .resolve(None, &mut CountingRunner::new(fixture.network_replies()))
        .unwrap();
    fixture.install(cold.refresh.unwrap());
    let mut deep_cwd = fixture.root.clone();
    for _ in 0..=MAX_COMPONENT_DEPTH {
        deep_cwd.push("d");
    }
    fs::create_dir_all(&deep_cwd).unwrap();
    let reader = fixture.reader();
    let mut runner = CountingRunner::new(fixture.network_replies());

    let resolved = fixture
        .resolve_at(
            &deep_cwd,
            Some(&reader),
            &HookBudget::from_start(Instant::now()),
            &mut runner,
        )
        .unwrap();

    assert_ne!(resolved.cache_outcome, ProjectCacheOutcome::Hit);
    assert_eq!(runner.call_count(), 4);
}

#[test]
fn linux_macos_worktree_slots_are_closed_and_git_commands_share_one_budget() {
    assert!(is_nix_store_git(Path::new(
        "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-git-2.55.0/bin/git"
    )));
    assert!(!is_nix_store_git(Path::new("/custom/bin/git")));
    assert!(is_cellar_git(
        Path::new("/opt/homebrew/Cellar/git/2.55.0/bin/git"),
        Path::new("/opt/homebrew/Cellar/git")
    ));
    assert!(!is_cellar_git(
        Path::new("/opt/homebrew/bin/git"),
        Path::new("/opt/homebrew/Cellar/git")
    ));
    assert_eq!(
        system_config_candidates(DiscoveryPlatform::Linux),
        [PathBuf::from("/etc/gitconfig")]
    );
    assert_eq!(
        system_config_candidates(DiscoveryPlatform::MacOs),
        [
            PathBuf::from("/etc/gitconfig"),
            PathBuf::from("/usr/local/etc/gitconfig"),
            PathBuf::from("/opt/homebrew/etc/gitconfig"),
        ]
    );
    for platform in [DiscoveryPlatform::Linux, DiscoveryPlatform::MacOs] {
        let mut fixture = Fixture::new();
        fixture.environment.platform = platform;
        fixture.environment.system_config_candidates = match platform {
            DiscoveryPlatform::Linux => vec![fixture.system_config.clone()],
            DiscoveryPlatform::MacOs => vec![
                fixture.system_config.clone(),
                fixture._temp.path().join("usr-local-gitconfig"),
                fixture._temp.path().join("homebrew-gitconfig"),
            ],
            DiscoveryPlatform::Unknown => unreachable!(),
        };
        let mut runner = CountingRunner::new(fixture.network_replies());

        let resolved = fixture.resolve(None, &mut runner).unwrap();

        assert!(resolved.refresh.is_some());
        assert_eq!(runner.call_count(), 4);
        assert!(
            runner
                .calls
                .windows(2)
                .all(|calls| calls[0].1 == calls[1].1)
        );
        assert!(runner.calls.iter().all(|call| call.2 <= GIT_OUTPUT_LIMIT));
    }
}

#[test]
fn git_admission_preserves_the_cleanup_and_storage_tail() {
    let fixture = Fixture::new();
    let started = Instant::now();
    let clock = ControlledClock::new(started);
    let budget = HookBudget::with_clock(clock.clone(), Duration::from_millis(1500));
    let mut admitted_runner = CountingRunner::new(fixture.network_replies());

    let admitted = fixture
        .resolve_at(&fixture.cwd, None, &budget, &mut admitted_runner)
        .unwrap();

    assert_eq!(admitted.provenance, ProjectProvenance::NetworkRemote);
    assert!(
        admitted_runner
            .calls
            .iter()
            .all(|call| call.1 == started + GIT_DISCOVERY_BUDGET)
    );

    clock.advance(Duration::from_millis(750));
    let mut rejected_runner = CountingRunner::new([]);
    let rejected = fixture
        .resolve_at(&fixture.cwd, None, &budget, &mut rejected_runner)
        .unwrap();
    assert_eq!(rejected.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        rejected.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert_eq!(rejected_runner.call_count(), 0);
    assert_eq!(GIT_CLEANUP_RESERVE, Duration::from_millis(250));
    assert_eq!(STORAGE_RESERVE, Duration::from_millis(500));
}

#[test]
fn rejected_git_admission_performs_no_filesystem_fallback() {
    let fixture = Fixture::new();
    let missing_cwd = fixture._temp.path().join("missing-cwd");
    let started = Instant::now();
    let clock = ControlledClock::new(started);
    let budget = HookBudget::with_clock(clock.clone(), Duration::from_millis(1500));
    clock.advance(Duration::from_millis(750));
    let mut runner = CountingRunner::new([]);

    let resolved = resolve_lifecycle_project_with_environment(
        &missing_cwd,
        &fixture.paths,
        None,
        &budget,
        &mut runner,
        &fixture.environment,
    )
    .unwrap();

    assert_eq!(resolved.root, missing_cwd);
    assert_eq!(resolved.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        resolved.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(resolved.refresh.is_none());
    assert_eq!(runner.call_count(), 0);
}

#[test]
fn git_timeout_performs_no_post_deadline_filesystem_fallback() {
    let fixture = Fixture::new();
    let fallback_manifest = fixture.cwd.join(".coding-brain/project.toml");
    fs::create_dir_all(fallback_manifest.parent().unwrap()).unwrap();
    fs::write(
        fallback_manifest,
        format!("schema_version = 1\nproject_id = \"{PROJECT_UUID}\"\n"),
    )
    .unwrap();
    let timeout = Reply {
        args: vec!["rev-parse", "--show-toplevel"],
        output: Err(ProjectCommandError::Timeout),
        mutate_after: None,
    };
    let mut runner = CountingRunner::new([timeout]);

    let resolved = fixture.resolve(None, &mut runner).unwrap();

    assert_eq!(resolved.root, fixture.cwd);
    assert_eq!(resolved.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        resolved.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(resolved.refresh.is_none());
    assert_eq!(runner.call_count(), 1);
}

#[test]
fn second_root_confirmation_follows_authority_and_rechecks_cwd_boundaries() {
    let fixture = Fixture::new();
    let nested_marker = fixture.cwd.join(".git");
    let mut replies = fixture.network_replies();
    replies[2].mutate_after = Some((nested_marker.clone(), Vec::new()));
    let mut runner = CountingRunner::new(replies);

    let resolved = fixture.resolve(None, &mut runner).unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        resolved.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(resolved.refresh.is_none());
    assert_eq!(runner.call_count(), 4);
    assert_eq!(runner.calls[3].0, vec!["rev-parse", "--show-toplevel"]);
    assert!(nested_marker.exists());
}

#[test]
fn git_commands_share_the_aggregate_output_budget() {
    let mut fixture = Fixture::new();
    let marker = fixture._temp.path().join("calls");
    let script = format!(
        "#!/bin/sh\nprintf x >> '{}'\ncase \"$1\" in\n  rev-parse) printf '%s\\n' '{}' ;;\n  remote) printf 'https://example.test/'; head -c 40000 /dev/zero | tr '\\000' a; printf '\\n' ;;\n  config) head -c 30000 /dev/zero | tr '\\000' b ;;\nesac\n",
        marker.display(),
        fixture.root.display(),
    );
    fs::write(&fixture.git_executable, script).unwrap();
    fs::set_permissions(&fixture.git_executable, fs::Permissions::from_mode(0o755)).unwrap();
    fixture.environment.path = fixture
        .git_executable
        .parent()
        .unwrap()
        .as_os_str()
        .to_owned();
    let mut runner = SystemLifecycleProjectRunner;

    let resolved = resolve_lifecycle_project_with_environment(
        &fixture.cwd,
        &fixture.paths,
        None,
        &HookBudget::from_start(Instant::now()),
        &mut runner,
        &fixture.environment,
    )
    .unwrap();

    assert_eq!(fs::read(&marker).unwrap(), b"xxx");
    assert_eq!(resolved.provenance, ProjectProvenance::Temporary);
    assert_eq!(
        resolved.cache_outcome,
        ProjectCacheOutcome::DiscoveryFailure
    );
    assert!(resolved.refresh.is_none());
}

#[test]
fn root_discovery_failure_never_reads_a_manifest_or_refreshes() {
    let mut fixture = Fixture::new();
    fs::remove_dir_all(fixture.root.join(".git")).unwrap();
    fixture.cwd = fixture.root.clone();
    fixture.write_manifest();
    let root_failure = Reply {
        args: vec!["rev-parse", "--show-toplevel"],
        output: Err(ProjectCommandError::ExitStatus),
        mutate_after: None,
    };

    let cold = fixture
        .resolve(None, &mut CountingRunner::new([root_failure]))
        .unwrap();

    assert_eq!(cold.provenance, ProjectProvenance::Temporary);
    assert_eq!(cold.cache_outcome, ProjectCacheOutcome::DiscoveryFailure);
    assert!(cold.refresh.is_none());
}

#[test]
fn non_regular_config_dependencies_are_noncacheable() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.git_config).unwrap();
    fs::create_dir(&fixture.git_config).unwrap();

    let resolved = fixture
        .resolve(None, &mut CountingRunner::new(fixture.network_replies()))
        .unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::NetworkRemote);
    assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::NonCacheable);
    assert!(resolved.refresh.is_none());
}

#[test]
fn excessive_git_topology_depth_is_noncacheable() {
    let fixture = Fixture::new();
    fs::remove_dir_all(fixture.root.join(".git")).unwrap();
    let mut git_directory = fixture._temp.path().join("deep-git");
    for _ in 0..=MAX_COMPONENT_DEPTH {
        git_directory.push("d");
    }
    fs::create_dir_all(&git_directory).unwrap();
    fs::write(
        fixture.root.join(".git"),
        format!("gitdir: {}\n", git_directory.display()),
    )
    .unwrap();
    fixture.write_manifest();

    let resolved = fixture
        .resolve(None, &mut CountingRunner::new(fixture.root_replies()))
        .unwrap();

    assert_eq!(resolved.provenance, ProjectProvenance::Manifest);
    assert_eq!(resolved.cache_outcome, ProjectCacheOutcome::NonCacheable);
    assert!(resolved.refresh.is_none());
}
