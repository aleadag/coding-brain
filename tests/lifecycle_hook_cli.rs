use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use coding_brain::brain::storage::{
    BrainDb, MigrationCoordinator, OpenRole, StorageDeadline, StoragePaths,
};
use coding_brain_core::lifecycle::test_support::LifecycleStore;
use coding_brain_core::lifecycle::{
    ApplyOutcome, LifecycleEvent, LifecycleEventKind, LifecycleEventName, LifecycleIdentity,
    MAX_SESSIONS, ProjectedStatus,
};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use sha2::{Digest, Sha256};

const PROMPT: &[u8] = include_bytes!("fixtures/hooks/user-prompt-submit.json");
const PRE_TOOL_USE: &[u8] = include_bytes!("fixtures/hooks/pre-tool-use.json");
const POST_TOOL_USE: &[u8] = include_bytes!("fixtures/hooks/post-tool-use.json");
const CLAUDE_STOP: &[u8] = include_bytes!("fixtures/hooks/claude-stop.json");
const ANTIGRAVITY_STOP: &[u8] = include_bytes!("fixtures/hooks/antigravity-stop.json");
const ANTIGRAVITY_PRE_TOOL_USE: &[u8] =
    include_bytes!("fixtures/hooks/antigravity-pre-tool-use.json");
const ANTIGRAVITY_POST_TOOL_USE: &[u8] =
    include_bytes!("fixtures/hooks/antigravity-post-tool-use.json");

fn secure_home(home: &std::path::Path) {
    #[cfg(unix)]
    for path in [
        home.to_path_buf(),
        home.join(".local"),
        home.join(".local/state"),
        home.join(".local/state/coding-brain"),
        home.join(".local/state/coding-brain/brain"),
        home.join(".local/state/coding-brain/hooks"),
    ] {
        if path.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let state_root = home.join(".local/state/coding-brain");
    let legacy_guard = state_root.join("brain/permission-transactions");
    if !state_root.join("db/brain.sqlite3").exists() && legacy_guard.is_dir() {
        fs::set_permissions(legacy_guard, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn command_for_home(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .current_dir(home);
    command
}

fn prepare_current_storage(home: &std::path::Path) {
    secure_home(home);
    MigrationCoordinator::at(&home.join(".local/state/coding-brain"))
        .run_non_hook()
        .unwrap();
}

fn open_brain(home: &std::path::Path) -> BrainDb {
    BrainDb::open_current(
        &StoragePaths::at(&home.join(".local/state/coding-brain")),
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap()
}

fn lifecycle_snapshot(home: &std::path::Path) -> coding_brain_core::lifecycle::LifecycleSnapshot {
    open_brain(home).read_lifecycle().unwrap()
}

fn activity_events(
    home: &std::path::Path,
) -> Vec<coding_brain_core::brain_activity::ActivityEvent> {
    let mut records = open_brain(home)
        .read_activity_page(None, 4096, 16 * 1024 * 1024)
        .unwrap()
        .events;
    records.sort_by_key(|record| record.cursor);
    records.into_iter().map(|record| record.event).collect()
}

fn normalized_activity_authority(
    home: &std::path::Path,
) -> Vec<coding_brain_core::brain_activity::ActivityEvent> {
    let mut activity_ids = BTreeMap::<String, String>::new();
    let mut decision_ids = BTreeMap::<String, String>::new();
    activity_events(home)
        .into_iter()
        .map(|mut event| {
            event.recorded_at_ms = 0;
            let next_activity = format!("activity-{}", activity_ids.len());
            event.activity_id = activity_ids
                .entry(event.activity_id)
                .or_insert(next_activity)
                .clone();
            if let Some(decision_id) = event.decision_id.take() {
                let next_decision = format!("decision-{}", decision_ids.len());
                event.decision_id = Some(
                    decision_ids
                        .entry(decision_id)
                        .or_insert(next_decision)
                        .clone(),
                );
            }
            event.project.cwd.clear();
            if let Some(session) = &mut event.session {
                session.cwd.clear();
            }
            event
        })
        .collect()
}

fn run_hook(home: &std::path::Path, input: &[u8]) -> Output {
    run_provider_hook(home, None, input)
}

fn run_provider_hook(home: &std::path::Path, provider: Option<&str>, input: &[u8]) -> Output {
    run_provider_hook_with_event(home, provider, None, input)
}

fn run_provider_hook_with_event(
    home: &std::path::Path,
    provider: Option<&str>,
    antigravity_event: Option<&str>,
    input: &[u8],
) -> Output {
    prepare_current_storage(home);
    let normalized_input = serde_json::from_slice::<serde_json::Value>(input)
        .map(|mut value| {
            value["cwd"] = serde_json::json!(home);
            if value.get("workspacePaths").is_some() {
                value["workspacePaths"] = serde_json::json!([home]);
            }
            serde_json::to_vec(&value).unwrap()
        })
        .unwrap_or_else(|_| input.to_vec());
    let mut command = command_for_home(home);
    command.arg("--lifecycle-hook");
    if let Some(provider) = provider {
        command.args(["--provider", provider]);
    }
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&normalized_input)
        .unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
struct GitWrapperFixture {
    _root: tempfile::TempDir,
    home: std::path::PathBuf,
    wrapper_dir: std::path::PathBuf,
    real_git: std::path::PathBuf,
    invocations: std::path::PathBuf,
}

#[cfg(unix)]
impl GitWrapperFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let wrapper_dir = root.path().join("bin");
        let invocations = root.path().join("git-invocations");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&wrapper_dir).unwrap();
        let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("git"))
            .find(|candidate| candidate.is_file())
            .and_then(|candidate| fs::canonicalize(candidate).ok())
            .expect("git executable on PATH");
        let initialized = Command::new(&real_git)
            .args(["init", "--quiet"])
            .current_dir(&home)
            .status()
            .unwrap();
        assert!(initialized.success());
        let project_dir = home.join(".coding-brain");
        fs::create_dir_all(&project_dir).unwrap();
        fs::write(
            project_dir.join("project.toml"),
            "schema_version = 1\nproject_id = \"123e4567-e89b-12d3-a456-426614174000\"\n",
        )
        .unwrap();
        let wrapper = wrapper_dir.join("git");
        fs::write(
            &wrapper,
            "#!/bin/sh\nprintf x >> \"$CBRAIN_TEST_GIT_INVOCATIONS\"\nexec \"$CBRAIN_TEST_REAL_GIT\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _root: root,
            home,
            wrapper_dir,
            real_git,
            invocations,
        }
    }

    fn run_hook(&self, input: &[u8], timing: bool) -> Output {
        prepare_current_storage(&self.home);
        let mut payload: serde_json::Value = serde_json::from_slice(input).unwrap();
        payload["cwd"] = serde_json::json!(&self.home);
        let payload = serde_json::to_vec(&payload).unwrap();
        let mut command = command_for_home(&self.home);
        command
            .arg("--lifecycle-hook")
            .env("PATH", &self.wrapper_dir)
            .env("CBRAIN_TEST_REAL_GIT", &self.real_git)
            .env("CBRAIN_TEST_GIT_INVOCATIONS", &self.invocations);
        if timing {
            command.env("CBRAIN_HOOK_TIMING", "1");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&payload).unwrap();
        child.wait_with_output().unwrap()
    }

    fn git_invocations(&self) -> usize {
        fs::read(&self.invocations).map_or(0, |calls| calls.len())
    }

    fn invalidate_manifest_evidence(&self) {
        let manifest = self.home.join(".coding-brain/project.toml");
        let mut contents = fs::read(&manifest).unwrap();
        contents.push(b'\n');
        fs::write(manifest, contents).unwrap();
    }

    fn runtime_cache(&self) -> std::path::PathBuf {
        self.home
            .join(".local/state/coding-brain/db/runtime-cache-v1.sqlite3")
    }
}

#[test]
#[cfg(unix)]
fn separate_hook_processes_share_a_validated_project_cache() {
    let fixture = GitWrapperFixture::new();

    let first = fixture.run_hook(PROMPT, false);
    let calls_after_first = fixture.git_invocations();
    let second = fixture.run_hook(PRE_TOOL_USE, false);

    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(first.stdout.is_empty() && second.stdout.is_empty());
    assert!(first.stderr.is_empty() && second.stderr.is_empty());
    assert!(calls_after_first > 0);
    assert_eq!(fixture.git_invocations(), calls_after_first);
}

#[test]
#[cfg(unix)]
fn project_cache_event_matrix_reports_closed_miss_hit_and_invalidation() {
    for (input, event, tool) in [
        (PROMPT, "user_prompt_submit", Some("UserPromptSubmit")),
        (PRE_TOOL_USE, "pre_tool_use", Some("PreToolUse")),
        (POST_TOOL_USE, "post_tool_use", Some("PostToolUse")),
    ] {
        let fixture = GitWrapperFixture::new();
        let miss = fixture.run_hook(input, true);
        let calls_after_miss = fixture.git_invocations();
        let hit = fixture.run_hook(input, true);
        assert_eq!(fixture.git_invocations(), calls_after_miss, "{event}");
        fixture.invalidate_manifest_evidence();
        let invalid = fixture.run_hook(input, true);
        assert!(fixture.git_invocations() > calls_after_miss, "{event}");

        for (output, outcome) in [
            (&miss, "cache_miss"),
            (&hit, "cache_hit"),
            (&invalid, "cache_invalid"),
        ] {
            assert!(output.status.success(), "{event}: {:?}", output.stderr);
            assert!(output.stdout.is_empty());
            let timing = String::from_utf8_lossy(&output.stderr);
            assert!(
                timing.contains(&format!(
                    "event={event} stage=project_cache outcome={outcome}"
                )),
                "{timing:?}"
            );
            for line in timing.lines() {
                assert!(line.starts_with("cbrain_hook_timing v=1 provider=codex event="));
                assert!(!line.contains(fixture.home.to_string_lossy().as_ref()));
                assert!(!line.contains(fixture.real_git.to_string_lossy().as_ref()));
                assert!(!line.contains("do not persist me"));
                assert_eq!(line.split_whitespace().count(), 8);
            }
        }
        let matching = activity_events(&fixture.home)
            .into_iter()
            .filter(|activity| activity.tool.as_deref() == tool)
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "{event}");
        if event == "post_tool_use" {
            assert!(matching.iter().all(|activity| {
                activity.session.as_ref().unwrap().tool_use_id.as_deref() == Some("call-1")
            }));
        }
    }
}

#[test]
#[cfg(unix)]
fn private_cache_bypass_is_immutable_and_preserves_exact_brain_evidence() {
    let healthy = GitWrapperFixture::new();
    assert!(healthy.run_hook(PROMPT, false).status.success());
    assert!(healthy.run_hook(PRE_TOOL_USE, false).status.success());
    let healthy_authority = normalized_activity_authority(&healthy.home);

    for mutation in ["incompatible", "corrupt_uuid", "unsafe_mode"] {
        let fixture = GitWrapperFixture::new();
        assert!(fixture.run_hook(PROMPT, false).status.success());
        let cache = fixture.runtime_cache();
        match mutation {
            "incompatible" => rusqlite::Connection::open(&cache)
                .unwrap()
                .execute_batch("PRAGMA user_version = 2;")
                .unwrap(),
            "corrupt_uuid" => rusqlite::Connection::open(&cache)
                .unwrap()
                .execute_batch(
                    "PRAGMA ignore_check_constraints = ON;
                     UPDATE project_identity_cache SET project_uuid = 'invalid';",
                )
                .unwrap(),
            "unsafe_mode" => {
                fs::set_permissions(&cache, fs::Permissions::from_mode(0o644)).unwrap()
            }
            _ => unreachable!(),
        }
        let bytes_before = fs::read(&cache).unwrap();
        let inode_before = fs::metadata(&cache).unwrap().ino();

        let output = fixture.run_hook(PRE_TOOL_USE, true);

        assert!(output.status.success(), "{mutation}: {:?}", output.stderr);
        assert!(output.stdout.is_empty(), "{mutation}");
        let timing = String::from_utf8(output.stderr).unwrap();
        assert!(
            timing.contains("event=pre_tool_use stage=project_cache outcome=cache_bypassed"),
            "{mutation}: {timing:?}"
        );
        assert_eq!(fs::read(&cache).unwrap(), bytes_before, "{mutation}");
        assert_eq!(
            fs::metadata(&cache).unwrap().ino(),
            inode_before,
            "{mutation}"
        );
        assert_eq!(
            normalized_activity_authority(&fixture.home),
            healthy_authority,
            "{mutation}"
        );
    }
}

#[test]
#[cfg(unix)]
fn contended_private_cache_refresh_preserves_exact_brain_evidence() {
    let healthy = GitWrapperFixture::new();
    assert!(healthy.run_hook(PROMPT, false).status.success());
    assert!(healthy.run_hook(PRE_TOOL_USE, false).status.success());
    let healthy_authority = normalized_activity_authority(&healthy.home);

    let fixture = GitWrapperFixture::new();
    assert!(fixture.run_hook(PROMPT, false).status.success());
    let cache = fixture.runtime_cache();
    let connection = rusqlite::Connection::open(&cache).unwrap();
    let row_before = connection
        .query_row(
            "SELECT project_uuid, provenance, length(evidence), refresh_order, row_version FROM project_identity_cache",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
        )
        .unwrap();
    connection.execute_batch("BEGIN IMMEDIATE;").unwrap();
    fixture.invalidate_manifest_evidence();

    let output = fixture.run_hook(PRE_TOOL_USE, true);

    connection.execute_batch("ROLLBACK;").unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    let timing = String::from_utf8(output.stderr).unwrap();
    assert!(
        timing.contains("event=pre_tool_use stage=project_cache outcome=cache_invalid"),
        "{timing:?}"
    );
    let row_after = connection
        .query_row(
            "SELECT project_uuid, provenance, length(evidence), refresh_order, row_version FROM project_identity_cache",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
        )
        .unwrap();
    assert_eq!(row_after, row_before);
    assert_eq!(
        normalized_activity_authority(&fixture.home),
        healthy_authority
    );
}

#[test]
#[cfg(unix)]
fn malicious_private_cache_rows_cannot_change_permission_or_delivery_authority() {
    let healthy = GitWrapperFixture::new();
    assert!(healthy.run_hook(PROMPT, false).status.success());
    write_brain_config(&healthy.home);
    let permission_request = |home: &std::path::Path| {
        serde_json::to_vec(&serde_json::json!({
            "session_id": "permission-cache-isolation",
            "turn_id": "permission-cache-turn",
            "transcript_path": "/tmp/permission-cache.jsonl",
            "cwd": home,
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"},
        }))
        .unwrap()
    };
    let healthy_output = run_permission_hook(&healthy.home, &permission_request(&healthy.home));
    assert!(healthy_output.status.success());
    let healthy_authority = normalized_activity_authority(&healthy.home);
    for mutation in [
        "invalid_uuid",
        "temporary_identity",
        "oversized",
        "incompatible",
    ] {
        let fixture = GitWrapperFixture::new();
        assert!(fixture.run_hook(PROMPT, false).status.success());
        let cache = fixture.runtime_cache();
        let connection = rusqlite::Connection::open(&cache).unwrap();
        match mutation {
            "incompatible" => connection
                .execute_batch("PRAGMA user_version = 2;")
                .unwrap(),
            kind => {
                connection
                    .execute_batch(
                        "PRAGMA ignore_check_constraints = ON;
                         DELETE FROM project_identity_cache;",
                    )
                    .unwrap();
                let uuid = if kind == "temporary_identity" {
                    "temporary-identity-000000000000000"
                } else {
                    "not-a-canonical-project-uuid-value"
                };
                if kind == "oversized" {
                    connection
                        .execute(
                            "INSERT INTO project_identity_cache VALUES (?1, ?2, 1, zeroblob(65537), 1, 1)",
                            rusqlite::params![fixture.home.as_os_str().as_encoded_bytes(), "123e4567-e89b-12d3-a456-426614174000"],
                        )
                        .unwrap();
                } else {
                    connection
                        .execute(
                            "INSERT INTO project_identity_cache VALUES (?1, ?2, 1, x'01', 1, 1)",
                            rusqlite::params![fixture.home.as_os_str().as_encoded_bytes(), uuid],
                        )
                        .unwrap();
                }
            }
        }
        drop(connection);
        let cache_before = fs::read(&cache).unwrap();
        let inode_before = fs::metadata(&cache).unwrap().ino();
        write_brain_config(&fixture.home);
        let output = run_permission_hook(&fixture.home, &permission_request(&fixture.home));

        assert!(output.status.success(), "{mutation}: {:?}", output.stderr);
        assert_eq!(output.stdout, healthy_output.stdout, "{mutation}");
        assert_eq!(
            normalized_activity_authority(&fixture.home),
            healthy_authority,
            "{mutation}"
        );
        assert_eq!(fs::read(&cache).unwrap(), cache_before, "{mutation}");
        assert_eq!(
            fs::metadata(&cache).unwrap().ino(),
            inode_before,
            "{mutation}"
        );
    }
}

#[test]
#[cfg(unix)]
fn blocked_git_and_descendant_are_bounded_and_reaped() {
    for (input, event, tool_use_id, activity_count) in [
        (PROMPT, "user_prompt_submit", None, 1),
        (PRE_TOOL_USE, "pre_tool_use", Some("call-1"), 1),
        (POST_TOOL_USE, "post_tool_use", Some("call-1"), 2),
    ] {
        assert_blocked_git_is_bounded_and_reaped(input, event, tool_use_id, activity_count);
    }
}

#[cfg(unix)]
fn assert_blocked_git_is_bounded_and_reaped(
    input: &[u8],
    event: &str,
    tool_use_id: Option<&str>,
    activity_count: usize,
) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let ready_fifo = root.path().join("ready");
    let release_fifo = root.path().join("release");
    let liveness_fifo = root.path().join("liveness");
    for fifo in [&ready_fifo, &release_fifo, &liveness_fifo] {
        let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    }
    let mut ready_reader = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&ready_fifo)
        .unwrap();
    let release_keepalive = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release_fifo)
        .unwrap();
    let liveness_keepalive = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&liveness_fifo)
        .unwrap();
    let mut liveness_reader = fs::OpenOptions::new()
        .read(true)
        .open(&liveness_fifo)
        .unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\nexec 9>\"$CBRAIN_TEST_LIVENESS_FIFO\"\n(IFS= read -r _ < \"$CBRAIN_TEST_RELEASE_FIFO\") &\nprintf r > \"$CBRAIN_TEST_READY_FIFO\"\nIFS= read -r _ < \"$CBRAIN_TEST_RELEASE_FIFO\"\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    prepare_current_storage(&home);
    let mut payload: serde_json::Value = serde_json::from_slice(input).unwrap();
    payload["cwd"] = serde_json::json!(&home);
    let payload = serde_json::to_vec(&payload).unwrap();
    let mut command = command_for_home(&home);
    command
        .arg("--lifecycle-hook")
        .env("PATH", &bin)
        .env("CBRAIN_HOOK_TIMING", "1")
        .env("CBRAIN_TEST_READY_FIFO", &ready_fifo)
        .env("CBRAIN_TEST_RELEASE_FIFO", &release_fifo)
        .env("CBRAIN_TEST_LIVENESS_FIFO", &liveness_fifo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(&payload).unwrap();
    let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut ready = [0_u8; 1];
        ready_sender
            .send(ready_reader.read_exact(&mut ready).map(|()| ready))
            .unwrap();
    });
    let ready = ready_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("Git wrapper readiness timeout")
        .unwrap();
    assert_eq!(ready, *b"r");
    drop(liveness_keepalive);

    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || output_sender.send(child.wait_with_output()).unwrap());
    let (liveness_sender, liveness_receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut liveness = [0_u8; 1];
        liveness_sender
            .send(liveness_reader.read(&mut liveness))
            .unwrap();
    });
    let output = output_receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_else(|error| panic!("{event}: outer deadlock safety ceiling: {error}"))
        .unwrap();
    let elapsed = started.elapsed();
    drop(release_keepalive);

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(elapsed < Duration::from_millis(1500), "elapsed {elapsed:?}");
    let timing = String::from_utf8(output.stderr).unwrap();
    assert!(
        timing.contains(&format!("event={event} stage=project_git outcome=error")),
        "{timing:?}"
    );
    assert!(timing.contains("stage=total outcome=success"), "{timing:?}");
    for fifo in [&ready_fifo, &release_fifo, &liveness_fifo] {
        assert!(!timing.contains(&fifo.display().to_string()), "{timing:?}");
    }
    assert_eq!(
        liveness_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Git descendant retained its liveness FIFO")
            .unwrap(),
        0
    );
    assert!(!fixture_cache_path(&home).exists());
    let activities = activity_events(&home);
    assert_eq!(activities.len(), activity_count, "{event}");
    assert!(
        activities.iter().all(|activity| {
            activity
                .session
                .as_ref()
                .and_then(|session| session.tool_use_id.as_deref())
                == tool_use_id
        }),
        "{event}"
    );
}

#[test]
#[cfg(unix)]
fn oversized_git_output_is_bounded_and_timing_is_closed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let bin = root.path().join("bin");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin).unwrap();
    let wrapper = bin.join("git");
    fs::write(
        &wrapper,
        "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 5000 ]; do printf RAW_REMOTE_SECRET; i=$((i + 1)); done\nprintf FREE_FORM_ERROR_SECRET >&2\n",
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    prepare_current_storage(&home);
    let payload = serde_json::to_vec(&serde_json::json!({
        "session_id": "overflow-session",
        "turn_id": "overflow-turn",
        "transcript_path": "/tmp/overflow.jsonl",
        "cwd": home,
        "hook_event_name": "UserPromptSubmit",
        "prompt": "PAYLOAD_SECRET",
    }))
    .unwrap();
    let started = Instant::now();
    let mut child = command_for_home(&home)
        .arg("--lifecycle-hook")
        .env("PATH", &bin)
        .env("GIT_CONFIG_VALUE_0", "ENVIRONMENT_SECRET")
        .env("CBRAIN_HOOK_TIMING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&payload).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(started.elapsed() < Duration::from_millis(1500));
    let timing = String::from_utf8(output.stderr).unwrap();
    assert!(
        timing.contains("stage=project_git outcome=error"),
        "{timing:?}"
    );
    assert!(timing.contains("stage=total outcome=success"), "{timing:?}");
    for secret in [
        "RAW_REMOTE_SECRET",
        "FREE_FORM_ERROR_SECRET",
        "ENVIRONMENT_SECRET",
        "PAYLOAD_SECRET",
        &wrapper.display().to_string(),
    ] {
        assert!(
            !timing.contains(secret),
            "timing leaked {secret:?}: {timing:?}"
        );
    }
    assert!(!fixture_cache_path(&home).exists());
}

#[cfg(unix)]
fn fixture_cache_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local/state/coding-brain/db/runtime-cache-v1.sqlite3")
}

fn assert_antigravity_rejected(event: Option<&str>, payload: &serde_json::Value, label: &str) {
    let home = tempfile::tempdir().unwrap();
    let output = run_provider_hook_with_event(
        home.path(),
        Some("antigravity"),
        event,
        &serde_json::to_vec(payload).unwrap(),
    );
    assert!(output.status.success(), "{label}");
    assert!(output.stdout.is_empty(), "{label}");
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostic.starts_with("cbrain lifecycle hook:"),
        "{label}: {diagnostic:?}"
    );
    assert!(diagnostic.len() < 256, "{label}");
    for path in [
        "hooks/lifecycle.json",
        "activity.jsonl",
        "session-links.jsonl",
    ] {
        assert!(
            !home
                .path()
                .join(".local/state/coding-brain")
                .join(path)
                .exists(),
            "{label}: unexpectedly persisted {path}"
        );
    }
}

fn seed_antigravity_invocation(home: &std::path::Path, initial_step: u64) {
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "agy-conversation-1".into(),
        Some("invocation-1".into()),
        Some("/tmp/agy-conversation-1/transcript.jsonl".into()),
        home.to_path_buf(),
    )
    .unwrap();
    let event = LifecycleEvent::from_parts_with_turn_initial_step(
        identity,
        LifecycleEventKind::UserPromptSubmit,
        Some(initial_step),
    )
    .unwrap();
    assert_eq!(
        LifecycleStore::at(home.join(".local/state/coding-brain"))
            .record(event)
            .unwrap(),
        ApplyOutcome::Applied
    );
}

#[test]
fn claude_lifecycle_hook_records_provider_qualified_stop() {
    let home = tempfile::tempdir().unwrap();
    let output = run_provider_hook(home.path(), Some("claude"), CLAUDE_STOP);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("exact recovery identity link unavailable")
    );

    let snapshot = lifecycle_snapshot(home.path());
    let key = AgentSessionKey::native(AgentProvider::Claude, "claude-session-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::Stop)
    );

    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/session-links.jsonl")
            .exists(),
        "a non-provider test parent must not become live identity evidence"
    );
}

#[test]
fn antigravity_trusted_cli_events_record_provider_qualified_lifecycle() {
    let post_home = tempfile::tempdir().unwrap();
    seed_antigravity_invocation(post_home.path(), 5);
    let post = run_provider_hook_with_event(
        post_home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        ANTIGRAVITY_POST_TOOL_USE,
    );
    assert!(post.status.success());
    assert!(post.stdout.is_empty());
    assert!(
        post.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&post.stderr)
    );
    let snapshot = lifecycle_snapshot(post_home.path());
    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );
    let activity = activity_events(post_home.path());
    let row = activity.last().unwrap();
    assert_eq!(
        row.kind,
        coding_brain_core::brain_activity::ActivityKind::Lifecycle
    );
    assert_eq!(row.tool.as_deref(), Some("PostToolUse"));
    assert_eq!(
        row.session.as_ref().unwrap().provider,
        AgentProvider::Antigravity
    );

    let adversarial_home = tempfile::tempdir().unwrap();
    seed_antigravity_invocation(adversarial_home.path(), 5);
    let mut adversarial: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    adversarial["hookEventName"] = serde_json::json!("Stop");
    adversarial["toolUseId"] = serde_json::json!("payload-controlled-id");
    adversarial["toolName"] = serde_json::json!("payload-controlled-tool");
    adversarial["executionNum"] = serde_json::json!(99);
    adversarial["terminationReason"] = serde_json::json!("payload-stop");
    adversarial["fullyIdle"] = serde_json::json!(true);
    let adversarial = run_provider_hook_with_event(
        adversarial_home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        &serde_json::to_vec(&adversarial).unwrap(),
    );
    assert!(adversarial.status.success());
    assert!(adversarial.stderr.is_empty());
    let snapshot = lifecycle_snapshot(adversarial_home.path());
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );
    assert_eq!(
        snapshot.sessions[&key].current_turn.as_deref(),
        Some("invocation-1")
    );

    let pre_home = tempfile::tempdir().unwrap();
    seed_antigravity_invocation(pre_home.path(), 5);
    let mut pre_payload: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_PRE_TOOL_USE).unwrap();
    pre_payload["hookEventName"] = serde_json::json!("Stop");
    pre_payload["toolUseId"] = serde_json::json!("payload-controlled-id");
    pre_payload["toolName"] = serde_json::json!("payload-controlled-tool");
    let pre = run_provider_hook_with_event(
        pre_home.path(),
        Some("antigravity"),
        Some("PreToolUse"),
        &serde_json::to_vec(&pre_payload).unwrap(),
    );
    assert!(pre.status.success());
    assert!(pre.stdout.is_empty());
    assert!(pre.stderr.is_empty());
    let snapshot = lifecycle_snapshot(pre_home.path());
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PreToolUse)
    );
    assert_eq!(
        snapshot.sessions[&key].current_turn.as_deref(),
        Some("invocation-1")
    );
    let activity = activity_events(pre_home.path());
    assert_eq!(
        activity
            .last()
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .tool_use_id
            .as_deref(),
        Some("step-5")
    );

    let stop_home = tempfile::tempdir().unwrap();
    let mut stop_payload: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_STOP).unwrap();
    stop_payload.as_object_mut().unwrap().remove("error");
    let stop = run_provider_hook_with_event(
        stop_home.path(),
        Some("antigravity"),
        Some("Stop"),
        &serde_json::to_vec(&stop_payload).unwrap(),
    );
    assert!(stop.status.success());
    assert!(stop.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("exact recovery identity link unavailable")
    );
    let snapshot = lifecycle_snapshot(stop_home.path());
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::Stop)
    );
    let invocation_home = tempfile::tempdir().unwrap();
    let invocation = serde_json::json!({
        "invocationNum": 3,
        "initialNumSteps": 10,
        "conversationId": "agy-conversation-1",
        "workspacePaths": [invocation_home.path()],
        "transcriptPath": "/tmp/agy-conversation-1/transcript.jsonl",
        "artifactDirectoryPath": "/tmp/agy-conversation-1/artifacts"
    });
    let pre_invocation = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("PreInvocation"),
        &serde_json::to_vec(&invocation).unwrap(),
    );
    assert!(pre_invocation.status.success());
    assert!(pre_invocation.stderr.is_empty());
    let state_root = invocation_home.path().join(".local/state/coding-brain");
    let before_post = lifecycle_snapshot(invocation_home.path());
    let activity_before_post = activity_events(invocation_home.path());
    let links_path = state_root.join("session-links.jsonl");
    let links_before_post = fs::read(&links_path).ok();

    let post_invocation = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("PostInvocation"),
        &serde_json::to_vec(&invocation).unwrap(),
    );
    assert!(post_invocation.status.success());
    assert!(
        post_invocation.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&post_invocation.stderr)
    );
    assert_eq!(
        lifecycle_snapshot(invocation_home.path()),
        before_post,
        "PostInvocation changed lifecycle state"
    );
    assert_eq!(
        activity_events(invocation_home.path()),
        activity_before_post,
        "PostInvocation appended activity"
    );
    assert_eq!(
        fs::read(&links_path).ok(),
        links_before_post,
        "PostInvocation appended a session link"
    );

    let mut continued_payload: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_PRE_TOOL_USE).unwrap();
    continued_payload["stepIdx"] = serde_json::json!(10);
    let continued_output = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("PreToolUse"),
        &serde_json::to_vec(&continued_payload).unwrap(),
    );
    assert!(continued_output.status.success());
    assert!(continued_output.stderr.is_empty());

    let stop = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("Stop"),
        ANTIGRAVITY_STOP,
    );
    assert!(stop.status.success());
    assert!(
        String::from_utf8_lossy(&stop.stderr).contains("exact recovery identity link unavailable")
    );
    let snapshot = lifecycle_snapshot(invocation_home.path());
    let state = &snapshot.sessions[&key];
    assert_eq!(state.latest_event, Some(LifecycleEventName::Stop));
    assert_eq!(state.current_turn.as_deref(), Some("invocation-3"));
    assert!(!state.turn_open);

    continued_payload["stepIdx"] = serde_json::json!(11);
    let after_stop = run_provider_hook_with_event(
        invocation_home.path(),
        Some("antigravity"),
        Some("PreToolUse"),
        &serde_json::to_vec(&continued_payload).unwrap(),
    );
    assert!(after_stop.status.success());
    assert!(after_stop.stderr.is_empty());
    assert_eq!(
        lifecycle_snapshot(invocation_home.path()).sessions[&key].latest_event,
        Some(LifecycleEventName::Stop)
    );
}

#[test]
fn antigravity_optional_error_is_typed_and_false_idle_is_rejected() {
    let mut post_without_error: serde_json::Value =
        serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    post_without_error.as_object_mut().unwrap().remove("error");
    let home = tempfile::tempdir().unwrap();
    seed_antigravity_invocation(home.path(), 5);
    let output = run_provider_hook_with_event(
        home.path(),
        Some("antigravity"),
        Some("PostToolUse"),
        &serde_json::to_vec(&post_without_error).unwrap(),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    let snapshot = lifecycle_snapshot(home.path());
    let key =
        AgentSessionKey::native(AgentProvider::Antigravity, "agy-conversation-1").storage_key();
    assert_eq!(
        snapshot.sessions[&key].latest_event,
        Some(LifecycleEventName::PostToolUse)
    );

    let mut false_idle: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_STOP).unwrap();
    false_idle["fullyIdle"] = serde_json::json!(false);
    assert_antigravity_rejected(Some("Stop"), &false_idle, "Stop with fullyIdle=false");

    for event in ["Stop", "PostToolUse"] {
        let fixture = if event == "Stop" {
            ANTIGRAVITY_STOP
        } else {
            ANTIGRAVITY_POST_TOOL_USE
        };
        for invalid_error in [
            serde_json::Value::Null,
            serde_json::json!({"message": "boom"}),
        ] {
            let mut payload: serde_json::Value = serde_json::from_slice(fixture).unwrap();
            payload["error"] = invalid_error;
            assert_antigravity_rejected(
                Some(event),
                &payload,
                &format!("{event} with non-string error"),
            );
        }
    }
}

#[test]
fn antigravity_missing_or_unknown_trusted_event_fails_open() {
    let payload: serde_json::Value = serde_json::from_slice(ANTIGRAVITY_POST_TOOL_USE).unwrap();
    assert_antigravity_rejected(None, &payload, "missing trusted event");
    assert_antigravity_rejected(Some("FutureEvent"), &payload, "unknown trusted event");
}

#[test]
fn antigravity_rejects_each_missing_required_event_field() {
    let shapes = [
        (
            "stop",
            "Stop",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_STOP).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "executionNum",
                "terminationReason",
                "fullyIdle",
            ][..],
        ),
        (
            "pre-tool-use",
            "PreToolUse",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_PRE_TOOL_USE).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "stepIdx",
                "toolCall",
                "toolCall.name",
                "toolCall.args",
            ][..],
        ),
        (
            "post-tool-use",
            "PostToolUse",
            serde_json::from_slice::<serde_json::Value>(ANTIGRAVITY_POST_TOOL_USE).unwrap(),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "stepIdx",
            ][..],
        ),
        (
            "invocation",
            "PostInvocation",
            serde_json::json!({
                "invocationNum": 3,
                "initialNumSteps": 10,
                "conversationId": "agy-conversation-1",
                "workspacePaths": ["/tmp"],
                "transcriptPath": "/tmp/transcript.jsonl",
                "artifactDirectoryPath": "/tmp/artifacts"
            }),
            &[
                "conversationId",
                "workspacePaths",
                "transcriptPath",
                "artifactDirectoryPath",
                "invocationNum",
                "initialNumSteps",
            ][..],
        ),
    ];

    for (shape, event, payload, fields) in shapes {
        for field in fields {
            let mut payload = payload.clone();
            if let Some((parent, child)) = field.split_once('.') {
                payload[parent].as_object_mut().unwrap().remove(child);
            } else {
                payload.as_object_mut().unwrap().remove(*field);
            }
            assert_antigravity_rejected(Some(event), &payload, &format!("{shape} without {field}"));
        }
    }
}

#[test]
fn lifecycle_provider_comes_only_from_cli_dispatch() {
    let home = tempfile::tempdir().unwrap();
    let mut payload: serde_json::Value = serde_json::from_slice(CLAUDE_STOP).unwrap();
    payload["provider"] = serde_json::json!("codex");
    let output = run_provider_hook(
        home.path(),
        Some("claude"),
        &serde_json::to_vec(&payload).unwrap(),
    );
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("exact recovery identity link unavailable")
    );
    let snapshot = lifecycle_snapshot(home.path());
    assert!(snapshot.sessions.contains_key(
        &AgentSessionKey::native(AgentProvider::Claude, "claude-session-1").storage_key()
    ));
    assert!(!snapshot.sessions.contains_key(
        &AgentSessionKey::native(AgentProvider::Codex, "claude-session-1").storage_key()
    ));
}

#[test]
fn provider_hook_rejects_oversized_missing_and_unknown_input_without_activity() {
    for payload in [
        vec![b'x'; 65_537],
        br#"{"hook_event_name":"Stop","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"","cwd":"/tmp","hook_event_name":"Stop","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"session","turn_id":"turn","cwd":"/tmp","hook_event_name":"PostToolUse","tool_use_id":"","secret":"do not echo"}"#.to_vec(),
        br#"{"session_id":"session","cwd":"/tmp","hook_event_name":"FutureEvent","secret":"do not echo"}"#.to_vec(),
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = run_provider_hook(home.path(), Some("claude"), &payload);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let diagnostic = String::from_utf8(output.stderr).unwrap();
        assert!(diagnostic.starts_with("cbrain lifecycle hook:"));
        assert!(diagnostic.len() < 256);
        assert!(!diagnostic.contains("secret"));
        assert!(!home.path().join(".local/state/coding-brain/activity.jsonl").exists());
        assert!(!home.path().join(".local/state/coding-brain/hooks/lifecycle.json").exists());
    }
}

fn run_cli(home: &std::path::Path, args: &[&str]) -> Output {
    secure_home(home);
    command_for_home(home).args(args).output().unwrap()
}

fn run_init_check(home: &std::path::Path) -> Output {
    secure_home(home);
    command_for_home(home)
        .args(["init", "--check"])
        .env("PATH", "")
        .output()
        .unwrap()
}

#[test]
fn init_noninteractive_selectors_write_stable_provider_marker_keys() {
    let home = tempfile::tempdir().unwrap();
    let output = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(
            home.path()
                .join(".local/state/coding-brain/onboarding.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["phases"]["hooks.claude"]["status"], "installed");
    assert_eq!(marker["phases"]["hooks.antigravity"]["status"], "installed");
    assert!(marker["phases"].get("plugin").is_none());
    assert!(marker["phases"].get("hooks.codex").is_none());
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(home.path().join(".gemini/config/hooks.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());
}

#[test]
fn subsequent_init_preserves_previous_provider_records_for_check_and_remove() {
    let home = tempfile::tempdir().unwrap();
    for provider in ["claude", "codex"] {
        let output = run_cli(
            home.path(),
            &[
                "init",
                provider,
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        );
        assert!(output.status.success());
    }

    let marker: serde_json::Value = serde_json::from_slice(
        &fs::read(
            home.path()
                .join(".local/state/coding-brain/onboarding.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(marker["phases"]["hooks.codex"]["status"], "installed");
    assert_eq!(marker["phases"]["hooks.claude"]["status"], "installed");
    assert!(run_init_check(home.path()).status.success());

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(remove.status.success());
    for path in [".codex/hooks.json", ".claude/settings.json"] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({}));
    }
}

#[test]
fn init_legacy_noninteractive_and_plugin_only_print_exact_replacements() {
    let home = tempfile::tempdir().unwrap();
    let noninteractive = run_cli(
        home.path(),
        &["init", "--non-interactive", "--skip-brain", "--skip-skills"],
    );
    assert!(noninteractive.status.success());
    assert_eq!(
        String::from_utf8(noninteractive.stderr)
            .unwrap()
            .lines()
            .next(),
        Some(
            "warning: provider-less --non-interactive is deprecated; use `cbrain init codex --non-interactive` instead"
        )
    );

    let plugin_only = run_cli(home.path(), &["init", "--plugin-only"]);
    assert!(plugin_only.status.success());
    assert_eq!(
        String::from_utf8(plugin_only.stderr)
            .unwrap()
            .lines()
            .next(),
        Some("warning: --plugin-only is deprecated; use `cbrain init codex` instead")
    );
}

#[test]
fn init_all_cannot_be_combined_with_another_selector() {
    let home = tempfile::tempdir().unwrap();
    let output = run_cli(home.path(), &["init", "all", "codex", "--non-interactive"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("`all` cannot be combined with another provider selector")
    );
}

#[test]
fn init_check_upgrade_and_remove_use_recorded_providers() {
    let home = tempfile::tempdir().unwrap();
    let init = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(init.status.success());
    assert!(run_init_check(home.path()).status.success());

    fs::remove_file(home.path().join(".claude/settings.json")).unwrap();
    assert!(!run_init_check(home.path()).status.success());
    assert!(
        run_cli(home.path(), &["init", "--upgrade"])
            .status
            .success()
    );
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());

    let claude_path = home.path().join(".claude/settings.json");
    let mut claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    claude["keep"] = serde_json::json!("claude-user-setting");
    fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();
    let antigravity_path = home.path().join(".gemini/config/hooks.json");
    let mut antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    antigravity["keep"] = serde_json::json!({"command": "antigravity-user-setting"});
    fs::write(
        &antigravity_path,
        serde_json::to_vec_pretty(&antigravity).unwrap(),
    )
    .unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(
        remove.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&remove.stdout),
        String::from_utf8_lossy(&remove.stderr)
    );
    let claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    let antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    assert_eq!(claude, serde_json::json!({"keep": "claude-user-setting"}));
    assert_eq!(
        antigravity,
        serde_json::json!({"keep": {"command": "antigravity-user-setting"}})
    );
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/onboarding.json")
            .exists()
    );
}

#[test]
fn init_upgrade_retries_drift_and_leaves_skipped_providers_untouched() {
    let home = tempfile::tempdir().unwrap();
    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(
        &marker,
        br#"{"version":"0.0.1","completed_at":"now","phases":{"hooks.claude":{"status":"drift"},"hooks.antigravity":{"status":"skipped"}}}"#,
    )
    .unwrap();

    let upgrade = run_cli(home.path(), &["init", "--upgrade"]);

    assert!(
        upgrade.status.success(),
        "{}",
        String::from_utf8_lossy(&upgrade.stderr)
    );
    assert!(home.path().join(".claude/settings.json").exists());
    assert!(!home.path().join(".gemini/config/hooks.json").exists());
    assert!(!home.path().join(".codex/hooks.json").exists());
}

#[test]
fn init_remove_keeps_marker_when_multi_provider_staging_fails() {
    let home = tempfile::tempdir().unwrap();
    let init = run_cli(
        home.path(),
        &[
            "init",
            "claude",
            "antigravity",
            "--non-interactive",
            "--skip-brain",
            "--skip-skills",
        ],
    );
    assert!(init.status.success());
    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    let marker_before = fs::read(&marker).unwrap();
    let claude_path = home.path().join(".claude/settings.json");
    let claude_before = fs::read(&claude_path).unwrap();
    let antigravity = home.path().join(".gemini/config/hooks.json");
    fs::remove_file(&antigravity).unwrap();
    fs::create_dir(&antigravity).unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);

    assert!(!remove.status.success());
    assert_eq!(fs::read(&marker).unwrap(), marker_before);
    assert_eq!(fs::read(&claude_path).unwrap(), claude_before);
}

#[test]
fn init_remove_cleans_all_exact_provider_hooks_without_marker_authority() {
    for marker_state in ["missing", "corrupt", "subset"] {
        let home = tempfile::tempdir().unwrap();
        let init = run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        );
        assert!(init.status.success());
        let marker = home
            .path()
            .join(".local/state/coding-brain/onboarding.json");
        match marker_state {
            "missing" => fs::remove_file(&marker).unwrap(),
            "corrupt" => fs::write(&marker, b"{broken").unwrap(),
            "subset" => fs::write(
                &marker,
                br#"{"version":"0.58.0","completed_at":"now","phases":{"hooks.codex":{"status":"installed"}}}"#,
            )
            .unwrap(),
            _ => unreachable!(),
        }

        let remove = run_cli(home.path(), &["init", "--remove"]);
        assert!(
            remove.status.success(),
            "{marker_state}: {}",
            String::from_utf8_lossy(&remove.stderr)
        );
        for path in [
            ".codex/hooks.json",
            ".claude/settings.json",
            ".gemini/config/hooks.json",
        ] {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
            assert_eq!(value, serde_json::json!({}), "{marker_state}: {path}");
        }
    }
}

#[test]
fn init_remove_preserves_unrelated_and_modified_entries_without_marker_authority() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );
    let codex_path = home.path().join(".codex/hooks.json");
    let mut codex: serde_json::Value =
        serde_json::from_slice(&fs::read(&codex_path).unwrap()).unwrap();
    codex["keep"] = serde_json::json!("codex-user-setting");
    fs::write(&codex_path, serde_json::to_vec_pretty(&codex).unwrap()).unwrap();

    let claude_path = home.path().join(".claude/settings.json");
    let mut claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    claude["keep"] = serde_json::json!("claude-user-setting");
    let command = claude["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    claude["hooks"]["Stop"][0]["hooks"][0]["command"] =
        serde_json::json!(format!("{command} --user-option"));
    fs::write(&claude_path, serde_json::to_vec_pretty(&claude).unwrap()).unwrap();

    let antigravity_path = home.path().join(".gemini/config/hooks.json");
    let mut antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    antigravity["keep"] = serde_json::json!({"command": "user-setting"});
    let command = antigravity["coding-brain"]["Stop"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    antigravity["coding-brain"]["Stop"][0]["command"] =
        serde_json::json!(format!("{command} --user-option"));
    fs::write(
        &antigravity_path,
        serde_json::to_vec_pretty(&antigravity).unwrap(),
    )
    .unwrap();
    fs::write(
        home.path()
            .join(".local/state/coding-brain/onboarding.json"),
        b"{broken",
    )
    .unwrap();

    let remove = run_cli(home.path(), &["init", "--remove"]);

    assert!(remove.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&codex_path).unwrap()).unwrap(),
        serde_json::json!({"keep": "codex-user-setting"})
    );
    let claude: serde_json::Value =
        serde_json::from_slice(&fs::read(&claude_path).unwrap()).unwrap();
    assert_eq!(claude["keep"], "claude-user-setting");
    assert!(
        claude["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("--user-option")
    );
    let antigravity: serde_json::Value =
        serde_json::from_slice(&fs::read(&antigravity_path).unwrap()).unwrap();
    assert_eq!(antigravity["keep"]["command"], "user-setting");
    assert!(
        antigravity["coding-brain"]["Stop"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("--user-option")
    );
}

#[test]
fn init_purge_removes_all_exact_provider_hooks_without_a_marker() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );
    fs::remove_file(
        home.path()
            .join(".local/state/coding-brain/onboarding.json"),
    )
    .unwrap();

    let purge = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(purge.status.success());
    for path in [
        ".codex/hooks.json",
        ".claude/settings.json",
        ".gemini/config/hooks.json",
    ] {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(home.path().join(path)).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({}), "{path}");
    }
}

#[cfg(unix)]
#[test]
fn init_purge_stops_before_deleting_targets_when_provider_staging_fails() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(
            home.path(),
            &[
                "init",
                "all",
                "--non-interactive",
                "--skip-brain",
                "--skip-skills",
            ],
        )
        .status
        .success()
    );

    let marker = home
        .path()
        .join(".local/state/coding-brain/onboarding.json");
    let state_sentinel = home.path().join(".local/state/coding-brain/keep");
    let config = home.path().join(".config/coding-brain/config.toml");
    let legacy_state = home.path().join(".codexctl/keep");
    let legacy_config = home.path().join(".config/codexctl/config.toml");
    for (path, contents) in [
        (&state_sentinel, b"state".as_slice()),
        (&config, b"config".as_slice()),
        (&legacy_state, b"legacy-state".as_slice()),
        (&legacy_config, b"legacy-config".as_slice()),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    let codex = home.path().join(".codex/hooks.json");
    let claude = home.path().join(".claude/settings.json");
    let antigravity = home.path().join(".gemini/config/hooks.json");
    let antigravity_target = home.path().join("antigravity-hooks-target.json");
    let antigravity_before = fs::read(&antigravity).unwrap();
    fs::remove_file(&antigravity).unwrap();
    fs::write(&antigravity_target, &antigravity_before).unwrap();
    symlink(&antigravity_target, &antigravity).unwrap();

    let preserved_files = [
        (&marker, fs::read(&marker).unwrap()),
        (&state_sentinel, fs::read(&state_sentinel).unwrap()),
        (&config, fs::read(&config).unwrap()),
        (&legacy_state, fs::read(&legacy_state).unwrap()),
        (&legacy_config, fs::read(&legacy_config).unwrap()),
        (&codex, fs::read(&codex).unwrap()),
        (&claude, fs::read(&claude).unwrap()),
    ];

    let purge = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(!purge.status.success());
    for (path, contents) in preserved_files {
        assert_eq!(
            fs::read(path).unwrap(),
            contents,
            "{} changed",
            path.display()
        );
    }
    assert!(
        antigravity
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&antigravity_target).unwrap(), antigravity_before);
}

fn install_crash_journal(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let target = home.join(name);
    let original = b"{\"original\":true}\n".to_vec();
    let replacement = b"{\"replacement\":true}\n".to_vec();
    fs::write(&target, &replacement).unwrap();
    let hash = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
    let journal = serde_json::json!({
        "schema_version": 2,
        "transaction_id": "integration-crash",
        "edits": [{
            "path": target,
            "original": original,
            "original_mode": null,
            "original_hash": hash(&original),
            "replacement": replacement,
            "replacement_hash": hash(&replacement)
        }],
        "replaced_paths": [target],
        "in_flight": null
    });
    let path = home.join(".local/state/coding-brain/brain/hook-install-transaction.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&journal).unwrap()).unwrap();
    target
}

#[test]
fn init_plugin_only_recovers_before_the_current_hooks_early_return() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(home.path(), &["init", "--plugin-only"])
            .status
            .success()
    );
    let target = install_crash_journal(home.path(), "plugin-recovery.json");

    let output = run_cli(home.path(), &["init", "--plugin-only"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn init_plugin_only_reports_preserved_collision_without_calling_it_current() {
    let home = tempfile::tempdir().unwrap();
    assert!(
        run_cli(home.path(), &["init", "--plugin-only"])
            .status
            .success()
    );
    let hooks_path = home.path().join(".codex/hooks.json");
    let mut hooks: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    hooks["hooks"]["Stop"][0]["hooks"][0]["command"] = serde_json::json!(format!(
        "{} --user-option",
        hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
    ));
    let original = serde_json::to_vec_pretty(&hooks).unwrap();
    fs::write(&hooks_path, &original).unwrap();

    let output = run_cli(home.path(), &["init", "--plugin-only"]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Preserved user-modified"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("hooks are current"));
    assert!(String::from_utf8_lossy(&output.stdout).contains("No managed hook changes applied"));
    assert_eq!(fs::read(hooks_path).unwrap(), original);
}

#[test]
fn init_reset_recovers_a_pending_hook_transaction_first() {
    let home = tempfile::tempdir().unwrap();
    let target = install_crash_journal(home.path(), "reset-recovery.json");

    let output = run_cli(home.path(), &["init", "--reset"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn init_purge_recovers_a_pending_hook_transaction_first() {
    let home = tempfile::tempdir().unwrap();
    let target = install_crash_journal(home.path(), "purge-recovery.json");

    let output = run_cli(home.path(), &["init", "--purge", "--yes"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(target).unwrap(), b"{\"original\":true}\n");
}

#[test]
fn doctor_fails_when_pending_hook_recovery_is_invalid() {
    let home = tempfile::tempdir().unwrap();
    let journal = home
        .path()
        .join(".local/state/coding-brain/brain/hook-install-transaction.json");
    fs::create_dir_all(journal.parent().unwrap()).unwrap();
    fs::write(journal, b"not json").unwrap();

    let output = run_cli(home.path(), &["doctor"]);

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Provider hook recovery"));
}

fn prompt_payload(index: usize) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(PROMPT).unwrap();
    payload["turn_id"] = serde_json::json!(format!("turn-{index}"));
    serde_json::to_vec(&payload).unwrap()
}

#[cfg(unix)]
fn codex_child_permission_payload(
    cwd: &std::path::Path,
    session_id: &str,
    agent_id: &str,
    turn_id: &str,
    command: &str,
) -> Vec<u8> {
    let mut payload: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/hooks/codex-child-permission-request.json"
    ))
    .unwrap();
    payload["cwd"] = serde_json::json!(cwd);
    payload["session_id"] = serde_json::json!(session_id);
    payload["agent_id"] = serde_json::json!(agent_id);
    payload["turn_id"] = serde_json::json!(turn_id);
    payload["tool_name"] = serde_json::json!("Bash");
    payload["tool_input"] = serde_json::json!({"command": command});
    assert!(payload.get("tool_use_id").is_none());
    serde_json::to_vec(&payload).unwrap()
}

#[cfg(unix)]
fn run_permission_hook(home: &std::path::Path, input: &[u8]) -> Output {
    prepare_current_storage(home);
    run_permission_hook_without_prepare(home, input)
}

#[cfg(unix)]
fn run_permission_hook_without_prepare(home: &std::path::Path, input: &[u8]) -> Output {
    secure_home(home);
    let mut paths = vec![home.join("bin")];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let path = std::env::join_paths(paths).unwrap();
    let mut child = command_for_home(home)
        .arg("--permission-hook")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn write_brain_config(home: &std::path::Path) {
    let config_dir = home.join(".config/coding-brain");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(
        config_dir.join("config.toml"),
        "[brain]\nenabled = true\nendpoint = \"http://localhost/api/generate\"\n",
    )
    .unwrap();
    let gate_mode = home.join(".local/state/coding-brain/brain/gate-mode");
    fs::create_dir_all(gate_mode.parent().unwrap()).unwrap();
    fs::write(gate_mode, "auto\n").unwrap();
    let suggestion = serde_json::json!({
        "action": "approve",
        "message": "reviewed by brain",
        "reasoning": "test reasoning",
        "confidence": 0.9
    })
    .to_string();
    let body = serde_json::json!({ "response": suggestion }).to_string();
    let bin_dir = home.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let curl = bin_dir.join("curl");
    let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("sh"))
        .find(|path| path.is_file())
        .expect("sh is available on the test PATH");
    fs::write(
        &curl,
        format!(
            "#!{}\ncat >/dev/null\nprintf '%s' '{body}'\n",
            shell.display()
        ),
    )
    .unwrap();
    fs::set_permissions(curl, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn lifecycle_hook_binary_is_silent_and_records_under_temporary_home() {
    let home = tempfile::tempdir().unwrap();
    let output = run_hook(home.path(), PROMPT);
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );

    let snapshot = lifecycle_snapshot(home.path());
    assert_eq!(
        snapshot.sessions[&coding_brain_core::provider::AgentSessionKey::native(
            coding_brain_core::provider::AgentProvider::Codex,
            "session-1",
        )
        .storage_key()]
            .projected_status,
        Some(ProjectedStatus::Processing)
    );
}

#[test]
fn lifecycle_hook_binary_fails_open_with_empty_stdout() {
    let home = tempfile::tempdir().unwrap();
    let output = run_hook(home.path(), b"malformed secret");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostic.starts_with("cbrain lifecycle hook:"));
    assert!(!diagnostic.contains("secret"));
    assert!(
        !home
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
}

#[test]
fn parsed_session_start_keeps_its_timing_event_when_storage_is_unavailable() {
    let home = tempfile::tempdir().unwrap();
    prepare_current_storage(home.path());
    let paths = StoragePaths::at(&home.path().join(".local/state/coding-brain"));
    fs::remove_file(paths.brain_db()).unwrap();

    let mut input: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/hooks/session-start.json")).unwrap();
    input["cwd"] = serde_json::json!(home.path());
    let input = serde_json::to_vec(&input).unwrap();

    let mut child = command_for_home(home.path())
        .arg("--lifecycle-hook")
        .env("CBRAIN_HOOK_TIMING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("event=session_start stage=parse outcome=success"),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("event=session_start stage=sqlite_open outcome=storage_unavailable"),
        "{stderr:?}"
    );
    assert!(
        !stderr.contains("event=other stage=sqlite_open"),
        "{stderr:?}"
    );
}

#[test]
#[cfg(unix)]
fn permission_allow_is_suppressed_when_sqlite_storage_is_unavailable() {
    let request = |cwd: &std::path::Path| {
        serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "transcript_path": "/tmp/rollout-1.jsonl",
            "cwd": cwd,
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" }
        })
        .to_string()
    };
    let healthy = tempfile::tempdir().unwrap();
    write_brain_config(healthy.path());
    let healthy_request = request(healthy.path());
    let healthy_output = run_permission_hook(healthy.path(), healthy_request.as_bytes());

    let blocked = tempfile::tempdir().unwrap();
    write_brain_config(blocked.path());
    let blocked_request = request(blocked.path());
    let blocked_output =
        run_permission_hook_without_prepare(blocked.path(), blocked_request.as_bytes());

    assert!(healthy_output.status.success());
    assert!(blocked_output.status.success());
    assert!(blocked_output.stdout.is_empty());
    assert!(
        !healthy_output.stdout.is_empty(),
        "healthy permission hook wrote no response; stderr: {}",
        String::from_utf8_lossy(&healthy_output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&healthy_output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    assert!(healthy_output.stderr.is_empty());
    assert!(
        String::from_utf8(blocked_output.stderr)
            .unwrap()
            .contains("SQLite storage unavailable")
    );
    assert!(
        !healthy
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
    assert!(
        !blocked
            .path()
            .join(".local/state/coding-brain/.star-prompted")
            .exists()
    );
}

#[test]
#[cfg(unix)]
fn corrupt_and_future_lifecycle_block_permission_inference_without_rewrite() {
    for original in [
        b"not-json".as_slice(),
        br#"{"schema_version":5}"#.as_slice(),
    ] {
        let home = tempfile::tempdir().unwrap();
        write_brain_config(home.path());
        let inference_marker = home.path().join("inference-called");
        let curl = home.path().join("bin/curl");
        fs::write(
            &curl,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 99\n",
                inference_marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
        let lifecycle = home
            .path()
            .join(".local/state/coding-brain/hooks/lifecycle.json");
        fs::create_dir_all(lifecycle.parent().unwrap()).unwrap();
        fs::write(&lifecycle, original).unwrap();
        let request = serde_json::to_vec(&serde_json::json!({
            "session_id": "session-1",
            "turn_id": "turn-1",
            "cwd": home.path(),
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo test"}
        }))
        .unwrap();

        let output = run_permission_hook_without_prepare(home.path(), &request);

        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("SQLite storage unavailable"));
        assert!(!inference_marker.exists());
        assert_eq!(fs::read(&lifecycle).unwrap(), original);
        assert!(
            !home
                .path()
                .join(".local/state/coding-brain/activity.jsonl")
                .exists()
        );
        assert!(
            fs::read_dir(lifecycle.parent().unwrap())
                .unwrap()
                .all(|entry| {
                    !entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .contains("corrupt-")
                })
        );
    }
}

#[test]
#[cfg(unix)]
fn child_permission_without_topology_suppresses_model_allow() {
    let home = tempfile::tempdir().unwrap();
    write_brain_config(home.path());

    let output = run_permission_hook(
        home.path(),
        &codex_child_permission_payload(home.path(), "root-1", "child-a", "turn-a", "printf child"),
    );

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UnprovenSubagent"));
    let activity = activity_events(home.path());
    assert!(!activity.iter().any(|event| {
        event.state == coding_brain_core::brain_activity::ActivityState::Allowed
            && event
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == "child-a")
    }));
}

#[test]
#[cfg(unix)]
fn deterministic_child_deny_survives_missing_topology() {
    let home = tempfile::tempdir().unwrap();
    write_brain_config(home.path());

    let output = run_permission_hook(
        home.path(),
        &codex_child_permission_payload(home.path(), "root-1", "child-a", "turn-a", "rm -rf /"),
    );

    assert!(output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("allow"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("UnprovenSubagent"));
    let activity = activity_events(home.path());
    assert!(activity.iter().any(|event| {
        event.state == coding_brain_core::brain_activity::ActivityState::Delivered
    }));
    assert!(
        activity.iter().any(|event| {
            event.state == coding_brain_core::brain_activity::ActivityState::Denied
        })
    );
    let snapshot = lifecycle_snapshot(home.path());
    assert!(
        !snapshot
            .sessions
            .contains_key(&AgentSessionKey::native(AgentProvider::Codex, "child-a").storage_key())
    );
}

#[test]
#[cfg(unix)]
fn child_permission_at_global_capacity_does_not_evict_or_authorize() {
    let home = tempfile::tempdir().unwrap();
    write_brain_config(home.path());
    let lifecycle_store = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let root = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "root-1".into(),
        Some("turn-a".into()),
        None,
        home.path().to_path_buf(),
    )
    .unwrap();
    assert_eq!(
        lifecycle_store
            .record(
                LifecycleEvent::from_parts(root.clone(), LifecycleEventKind::UserPromptSubmit)
                    .unwrap()
            )
            .unwrap(),
        ApplyOutcome::Applied
    );
    for index in 0..MAX_SESSIONS - 1 {
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            format!("other-{index}"),
            Some("turn-1".into()),
            None,
            home.path().to_path_buf(),
        )
        .unwrap();
        assert_eq!(
            lifecycle_store
                .record(
                    LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit)
                        .unwrap(),
                )
                .unwrap(),
            ApplyOutcome::Applied
        );
    }
    assert_eq!(
        lifecycle_store
            .record(
                LifecycleEvent::from_parts(
                    root,
                    LifecycleEventKind::SubagentStart {
                        agent_id: "child-a".into(),
                    },
                )
                .unwrap(),
            )
            .unwrap(),
        ApplyOutcome::Applied
    );
    let before = lifecycle_store.read().unwrap().snapshot.unwrap();
    assert_eq!(before.sessions.len(), MAX_SESSIONS);

    let permission = run_permission_hook(
        home.path(),
        &codex_child_permission_payload(home.path(), "root-1", "child-a", "turn-a", "printf child"),
    );

    assert!(permission.status.success());
    assert!(permission.stdout.is_empty());
    assert!(String::from_utf8_lossy(&permission.stderr).contains("ActiveSubagentCapacity"));
    let after = lifecycle_store.read().unwrap().snapshot.unwrap();
    assert_eq!(after.sessions.len(), MAX_SESSIONS);
    assert_eq!(
        after.sessions.keys().collect::<BTreeSet<_>>(),
        before.sessions.keys().collect::<BTreeSet<_>>()
    );
    assert!(
        !after
            .sessions
            .contains_key(&AgentSessionKey::native(AgentProvider::Codex, "child-a").storage_key())
    );
    let activity = activity_events(home.path());
    assert!(!activity.iter().any(|event| {
        event.kind == coding_brain_core::brain_activity::ActivityKind::Decision
            && event.state == coding_brain_core::brain_activity::ActivityState::Delivered
            && event
                .session
                .as_ref()
                .is_some_and(|session| session.session_id == "child-a")
    }));
}

#[test]
#[ignore = "local warm hook latency smoke; not a CI timing gate"]
#[cfg(unix)]
fn warm_lifecycle_hook_latency_and_roundtrip() {
    let home = tempfile::tempdir().unwrap();
    let hooks_path = home.path().join(".codex/hooks.json");
    fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let unrelated = serde_json::json!({
        "allowedTools": ["Read"],
        "hooks": {
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": "echo keep-me",
                    "timeout": 9
                }]
            }]
        }
    });
    fs::write(
        &hooks_path,
        format!("{}\n", serde_json::to_string_pretty(&unrelated).unwrap()),
    )
    .unwrap();

    let init = run_cli(home.path(), &["init", "--plugin-only"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let installed: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    let expected = [
        (
            "SessionStart",
            Some("startup|resume|clear|compact"),
            "--lifecycle-hook",
            2,
        ),
        ("UserPromptSubmit", None, "--lifecycle-hook", 2),
        ("PreToolUse", Some("*"), "--lifecycle-hook", 2),
        ("PermissionRequest", Some("*"), "--permission-hook", 30),
        ("PostToolUse", Some("*"), "--lifecycle-hook", 2),
        ("SubagentStart", Some("*"), "--lifecycle-hook", 2),
        ("SubagentStop", Some("*"), "--lifecycle-hook", 2),
        ("Stop", None, "--lifecycle-hook", 2),
    ];
    for (event, matcher, argument, timeout) in expected {
        let expected_command = format!("codexctl {argument}");
        let groups = installed["hooks"][event].as_array().unwrap();
        let (group, handler) = groups
            .iter()
            .flat_map(|group| {
                group["hooks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |handler| (group, handler))
            })
            .find(|(_, handler)| handler["command"].as_str() == Some(expected_command.as_str()))
            .unwrap_or_else(|| panic!("missing managed {event} handler"));
        assert_eq!(
            group.get("matcher").and_then(|value| value.as_str()),
            matcher
        );
        assert_eq!(handler["timeout"], timeout);
    }

    let mut samples = Vec::new();
    for index in 0..101 {
        let started = Instant::now();
        let output = run_hook(home.path(), &prompt_payload(index));
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        if index > 0 {
            samples.push(started.elapsed());
        }
    }
    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[samples.len() * 95 / 100];
    eprintln!("warm lifecycle hook latency: p50={p50:?} p95={p95:?}; target <50ms");

    write_brain_config(home.path());
    let permission = serde_json::json!({
        "session_id": "session-1",
        "turn_id": "turn-100",
        "transcript_path": "/tmp/rollout-1.jsonl",
        "cwd": "/work/codexctl",
        "hook_event_name": "PermissionRequest",
        "tool_name": "Bash",
        "tool_input": { "command": "cargo test" }
    });
    let permission_output = run_permission_hook(
        home.path(),
        serde_json::to_string(&permission).unwrap().as_bytes(),
    );
    assert!(permission_output.status.success());
    let response: serde_json::Value = serde_json::from_slice(&permission_output.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    let store = LifecycleStore::at(home.path().join(".local/state/coding-brain"));
    let view = store.read().unwrap();
    assert_eq!(
        view.condition,
        coding_brain_core::lifecycle::StoreCondition::Healthy
    );
    let key = coding_brain_core::provider::AgentSessionKey::native(
        coding_brain_core::provider::AgentProvider::Codex,
        "session-1",
    )
    .storage_key();
    let state = &view.snapshot.unwrap().sessions[&key];
    assert_eq!(
        state.latest_event,
        Some(coding_brain_core::lifecycle::LifecycleEventName::PermissionRequest)
    );
    assert_eq!(state.projected_status, Some(ProjectedStatus::Processing));

    let remove = run_cli(home.path(), &["init", "--remove"]);
    assert!(
        remove.status.success(),
        "{}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let removed: serde_json::Value =
        serde_json::from_slice(&fs::read(&hooks_path).unwrap()).unwrap();
    assert_eq!(removed, unrelated);
    assert!(store.snapshot_path().exists());
}
