use std::fs::{self, OpenOptions};
#[cfg(feature = "fault-injection")]
use std::io::Read;
use std::io::Write;
#[cfg(feature = "fault-injection")]
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(feature = "fault-injection")]
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::time::Duration;

use coding_brain::brain::storage::{
    BrainDb, DecisionIdentity, FrozenSourceManifest, HistoricalDeliveryState,
    LEGACY_EXPORT_PROFILE, LegacyFreezeArtifact, LegacySourceKind, LegacySourceSet,
    LegacyWriterGuard, MigrationCoordinator, MigrationStatus, OpenRole, PermissionAdmission,
    PermissionState, StorageDeadline, StorageError, StoragePaths,
};
use coding_brain_core::lifecycle::{
    LifecycleEvent, LifecycleEventKind, LifecycleIdentity, LifecycleSnapshot, PermissionAction,
    PermissionAuthority,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;
use coding_brain_core::review_state::{ReviewKey, ReviewSurface};
use fs2::FileExt;
#[cfg(feature = "fault-injection")]
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

struct LegacyFixture {
    _root: tempfile::TempDir,
    state_root: std::path::PathBuf,
}

impl LegacyFixture {
    fn copy(name: &str) -> Self {
        fn copy_tree(source: &std::path::Path, destination: &std::path::Path) {
            fs::create_dir_all(destination).unwrap();
            fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
            for entry in fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                let target = destination.join(entry.file_name());
                if entry.file_type().unwrap().is_dir() {
                    copy_tree(&entry.path(), &target);
                } else {
                    fs::copy(entry.path(), &target).unwrap();
                    fs::set_permissions(target, fs::Permissions::from_mode(0o600)).unwrap();
                }
            }
        }

        let root = private_tempdir();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let state_root = state.join("coding-brain");
        copy_tree(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/storage")
                .join(name),
            &state_root,
        );
        Self {
            _root: root,
            state_root,
        }
    }

    fn state_root(&self) -> &std::path::Path {
        &self.state_root
    }
}

fn private_tempdir() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    root
}

fn write_private(path: &std::path::Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn remove_fixture_journal(state_root: &std::path::Path) {
    fs::remove_file(state_root.join(
        "brain/permission-transactions/permission-transaction-000000000000000000000000000000000000001-0000000001-00000000000000000001.json",
    ))
    .unwrap();
}

fn write_lifecycle_authorities(
    state_root: &std::path::Path,
    provider: AgentProvider,
    session_id: &str,
    turn_id: &str,
    cwd: &str,
    authorities: &[(&str, &str, PermissionAction)],
) {
    let identity = LifecycleIdentity::try_new(
        provider,
        session_id.to_owned(),
        Some(turn_id.to_owned()),
        None,
        cwd.into(),
    )
    .unwrap();
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.record_at(
        LifecycleEvent::from_parts(identity.clone(), LifecycleEventKind::UserPromptSubmit).unwrap(),
        1,
    );
    for (index, (request_key, transaction_id, action)) in authorities.iter().enumerate() {
        snapshot.record_at(
            LifecycleEvent::permission_with_authority(
                identity.clone(),
                (*request_key).to_owned(),
                PermissionAuthority {
                    transaction_id: (*transaction_id).to_owned(),
                    action: *action,
                },
            )
            .unwrap(),
            u64::try_from(index).unwrap() + 2,
        );
    }
    write_private(
        &state_root.join("hooks/lifecycle.json"),
        &serde_json::to_vec(&snapshot).unwrap(),
    );
}

fn write_antigravity_lifecycle_authority(
    state_root: &std::path::Path,
    permission_turn: &str,
    request_key: &str,
    transaction_id: &str,
) {
    let prompt_identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "session-4vh58".to_owned(),
        Some("invocation-3".to_owned()),
        None,
        "/fixture".into(),
    )
    .unwrap();
    let permission_identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "session-4vh58".to_owned(),
        Some(permission_turn.to_owned()),
        None,
        "/fixture".into(),
    )
    .unwrap();
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.record_at(
        LifecycleEvent::from_parts_with_turn_initial_step(
            prompt_identity,
            LifecycleEventKind::UserPromptSubmit,
            Some(5),
        )
        .unwrap(),
        1,
    );
    snapshot.record_at(
        LifecycleEvent::permission_with_authority(
            permission_identity,
            request_key.to_owned(),
            PermissionAuthority {
                transaction_id: transaction_id.to_owned(),
                action: PermissionAction::Allow,
            },
        )
        .unwrap(),
        2,
    );
    write_private(
        &state_root.join("hooks/lifecycle.json"),
        &serde_json::to_vec(&snapshot).unwrap(),
    );
}

fn rewrite_fixture_as_antigravity(state_root: &std::path::Path) {
    rewrite_json_lines(&state_root.join("brain/decisions.jsonl"), |_, value| {
        value["provider"] = serde_json::Value::from("antigravity");
        value["turn_id"] = serde_json::Value::from("step-5");
    });
    rewrite_json_lines(&state_root.join("activity.jsonl"), |_, value| {
        value["session"]["provider"] = serde_json::Value::from("antigravity");
        value["session"]["turn_id"] = serde_json::Value::from("step-5");
        value["session"]["tool_use_id"] = serde_json::Value::from("step-5");
    });
}

#[derive(Debug, Eq, PartialEq)]
#[cfg(feature = "fault-injection")]
struct TreeEntry {
    path: std::path::PathBuf,
    mode: u32,
    links: u64,
    len: u64,
    modified_ns: i128,
    bytes: Vec<u8>,
}

#[cfg(feature = "fault-injection")]
fn tree_snapshot(root: &std::path::Path) -> Vec<TreeEntry> {
    fn visit(root: &std::path::Path, path: &std::path::Path, entries: &mut Vec<TreeEntry>) {
        let mut children = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let metadata = fs::symlink_metadata(&child).unwrap();
            let bytes = if metadata.file_type().is_file() {
                fs::read(&child).unwrap()
            } else {
                Vec::new()
            };
            entries.push(TreeEntry {
                path: child.strip_prefix(root).unwrap().to_owned(),
                mode: metadata.mode(),
                links: metadata.nlink(),
                len: metadata.len(),
                modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
                bytes,
            });
            if metadata.file_type().is_dir() {
                visit(root, &child, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

#[cfg(feature = "fault-injection")]
fn assert_no_sqlite_sidecars(root: &std::path::Path) {
    assert!(tree_snapshot(root).iter().all(|entry| {
        let name = entry.path.file_name().unwrap().as_encoded_bytes();
        !name.ends_with(b"-wal") && !name.ends_with(b"-shm") && !name.ends_with(b"-journal")
    }));
}

#[cfg(feature = "fault-injection")]
fn migration_child(root: &std::path::Path, fault: &str) -> std::process::ExitStatus {
    let capability_root = private_tempdir();
    let mut descriptors = [-1; 2];
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    let mut control_reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
    let control_writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
    let metadata = control_writer.metadata().unwrap();
    let state_root = fs::canonicalize(root).unwrap();
    let nonce = "migration-regression-2o9fo";
    let capability = capability_root.path().join("capability.json");
    let mut capability_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&capability)
        .unwrap();
    serde_json::to_writer(
        &mut capability_file,
        &serde_json::json!({
            "version": 1,
            "state_root": state_root,
            "nonce": nonce,
            "selection": {
                "kind": "migration-regression",
                "selection": fault,
            },
            "control_device": metadata.dev(),
            "control_inode": metadata.ino(),
        }),
    )
    .unwrap();
    capability_file.flush().unwrap();

    let control_fd = control_writer.as_raw_fd().to_string();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .args([
            "--fault-worker",
            "--migration-fault-stage",
            fault,
            "--fault-nonce",
            nonce,
            "--fault-control-fd",
            &control_fd,
        ])
        .arg("--fault-capability")
        .arg(&capability)
        .env("HOME", capability_root.path())
        .env("XDG_STATE_HOME", root.parent().unwrap())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(control_writer);
    let status = child.wait().unwrap();
    let mut marker = Vec::new();
    control_reader.read_to_end(&mut marker).unwrap();
    assert_eq!(
        marker,
        format!("CBRAIN-FAULT-V1\0migration-publish\0after\0{fault}\n").as_bytes(),
        "{fault}"
    );
    assert_eq!(status.signal(), Some(libc::SIGABRT), "{fault}");
    status
}

fn legacy_freeze_child(
    root: &std::path::Path,
    fault: &str,
    publish: bool,
) -> std::process::ExitStatus {
    Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("legacy_freeze_fault_process_helper")
        .arg("--nocapture")
        .env("CODING_BRAIN_LEGACY_FREEZE_CHILD_ROOT", root)
        .env("CODING_BRAIN_SQLITE_LEGACY_FREEZE_FAULT", fault)
        .env(
            "CODING_BRAIN_LEGACY_FREEZE_CHILD_PUBLISH",
            if publish { "1" } else { "0" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
}

#[cfg(feature = "fault-injection")]
fn staging_path(root: &std::path::Path) -> std::path::PathBuf {
    let mut staging = fs::read_dir(root.join("db"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let name = path.file_name().unwrap().as_encoded_bytes();
            name.starts_with(b".brain.sqlite3.migrate-")
                && !name.ends_with(b"-wal")
                && !name.ends_with(b"-shm")
                && !name.ends_with(b"-journal")
        })
        .collect::<Vec<_>>();
    staging.sort();
    assert_eq!(staging.len(), 1, "expected one exact staging database");
    staging.pop().unwrap()
}

#[cfg(feature = "fault-injection")]
fn review_staging_path(root: &std::path::Path) -> std::path::PathBuf {
    fs::read_dir(root.join("db"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .as_encoded_bytes()
                .starts_with(b".review.sqlite3.migration-")
        })
        .expect("expected Review staging database")
}

#[cfg(feature = "fault-injection")]
fn freeze_progress_path(root: &std::path::Path) -> std::path::PathBuf {
    fs::read_dir(root.join("db"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .as_encoded_bytes()
                .starts_with(b".brain.sqlite3.freeze-progress-")
        })
        .expect("expected freeze progress directory")
}

#[test]
fn legacy_freeze_fault_process_helper() {
    let Some(root) = std::env::var_os("CODING_BRAIN_LEGACY_FREEZE_CHILD_ROOT") else {
        return;
    };
    let guard = LegacyWriterGuard::acquire(
        std::path::Path::new(&root),
        StorageDeadline::after(Duration::from_secs(5)),
    )
    .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    let artifact = guard
        .prepare_freeze(
            std::path::Path::new("activity.jsonl"),
            ".activity.jsonl.freeze-fault.tmp",
            &expected,
        )
        .unwrap();
    if std::env::var_os("CODING_BRAIN_LEGACY_FREEZE_CHILD_PUBLISH").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        guard.publish_freeze(&artifact).unwrap();
    }
}

#[test]
fn migration_reconciles_4vh58_without_response_authority() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT response_eligible, delivery_state
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap(),
        (0, "unknown".to_owned())
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM activity_events
             WHERE activity_id = 'activity-4vh58' AND outcome IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
}

#[test]
fn legacy_brain_proposals_import_as_model_historical_non_authority() {
    let fixture = LegacyFixture::copy("legacy-brain-proposals");
    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let connection =
        rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT (SELECT count(*) FROM permission_attempts),
                        (SELECT count(*) FROM permission_commits)",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap(),
        (0, 0)
    );
    drop(connection);

    let paths = StoragePaths::at(fixture.state_root());
    let mut db = BrainDb::open_current(
        &paths,
        OpenRole::NonHook,
        StorageDeadline::after(Duration::from_secs(2)),
    )
    .unwrap();
    for (decision_id, action) in [
        ("legacy-brain-allow", PermissionAction::Allow),
        ("legacy-brain-deny", PermissionAction::Deny),
    ] {
        let identity = db.decision_identity(decision_id).unwrap().unwrap();
        assert!(matches!(
            identity,
            DecisionIdentity::Permission {
                authority_action,
                ref decision_source,
                ..
            } if authority_action == action && decision_source == "model"
        ));
        assert_eq!(
            db.decision_payload(decision_id)
                .unwrap()
                .unwrap()
                .record
                .brain_action,
            match action {
                PermissionAction::Allow => "approve",
                PermissionAction::Deny => "deny",
            }
        );
    }
    let source_records =
        fs::read_to_string(fixture.state_root().join("brain/decisions.jsonl")).unwrap();
    assert!(source_records.lines().all(|line| {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["brain_source"] == "brain"
    }));

    let historical = db
        .historical_permission_authority_after(None, 10, 1024 * 1024)
        .unwrap();
    assert_eq!(historical.authorities.len(), 2);
    assert!(historical.authorities.iter().all(|row| {
        !row.response_eligible && row.delivery_state == HistoricalDeliveryState::Unknown
    }));

    let live_identity = LifecycleIdentity::try_new(
        AgentProvider::Antigravity,
        "legacy-brain-session".into(),
        Some("step-5".into()),
        None,
        "/fixture".into(),
    )
    .unwrap();
    let admission = PermissionAdmission::new(
        live_identity,
        "a".repeat(64),
        ProjectId::Temporary("fixture".into()),
        "Bash",
        Some("live-tool-use".into()),
        "live-activity",
        3000,
        3001,
    );
    let guard = db.admit_permission(admission).unwrap().unwrap();
    assert_eq!(
        db.permission_state(guard.attempt_id()).unwrap(),
        PermissionState::Absent
    );
    assert_eq!(db.permission_decision(guard.attempt_id()).unwrap(), None);
}

#[test]
#[cfg(feature = "fault-injection")]
fn legacy_brain_proposal_building_and_verified_restarts_complete_safely() {
    for fault in ["building", "verified"] {
        let fixture = LegacyFixture::copy("legacy-brain-proposals");
        let decisions_before =
            fs::read(fixture.state_root().join("brain/decisions.jsonl")).unwrap();
        let activity_before = fs::read(fixture.state_root().join("activity.jsonl")).unwrap();

        assert!(!migration_child(fixture.state_root(), fault).success());
        let state_before = migration_state(fixture.state_root());
        let generation = state_before["generation"].as_u64().unwrap();
        assert_eq!(state_before["status"], fault);
        assert!(!fixture.state_root().join("db/brain.sqlite3").exists());
        if fault == "building" {
            assert!(staging_path(fixture.state_root()).exists());
        }

        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
        assert_eq!(
            migration_state(fixture.state_root())["generation"],
            generation
        );
        assert_eq!(
            fs::read(fixture.state_root().join("brain/decisions.jsonl")).unwrap(),
            decisions_before
        );
        assert_eq!(
            fs::read(fixture.state_root().join("activity.jsonl")).unwrap(),
            activity_before
        );
        assert!(fixture.state_root().join("db/brain.sqlite3").exists());
        assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
    }
}

#[test]
fn migration_freezes_manifest_and_completes_normal_run() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
    assert_eq!(
        fs::metadata(fixture.state_root().join("brain/permission-transactions"),)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
}

#[test]
fn coordinator_rejects_self_consistent_manifest_substitution_bound_to_other_state() {
    #[derive(serde::Deserialize, serde::Serialize)]
    struct ManifestRow {
        relative_path: Vec<u8>,
        present: bool,
        device: u64,
        inode: u64,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        mode: u32,
    }

    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    assert_eq!(
        coordinator.run_non_hook().unwrap(),
        MigrationStatus::Complete
    );
    let path = fixture
        .state_root()
        .join("db/.brain.sqlite3.frozen-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["review_result"] = serde_json::Value::from("degraded");
    manifest["rows"].as_array_mut().unwrap().pop().unwrap();
    manifest["count"] = serde_json::Value::from(manifest["rows"].as_array().unwrap().len() as u64);
    let mut digest = Sha256::new();
    digest.update(b"coding-brain-frozen-source-manifest-v1");
    digest.update(manifest["generation"].as_u64().unwrap().to_be_bytes());
    digest.update((LEGACY_EXPORT_PROFILE.len() as u64).to_be_bytes());
    digest.update(LEGACY_EXPORT_PROFILE.as_bytes());
    digest.update(("degraded".len() as u64).to_be_bytes());
    digest.update(b"degraded");
    digest.update(manifest["count"].as_u64().unwrap().to_be_bytes());
    for row in manifest["rows"].as_array().unwrap() {
        let row: ManifestRow = serde_json::from_value(row.clone()).unwrap();
        let bytes = serde_json::to_vec(&row).unwrap();
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    manifest["digest"] = serde_json::Value::from(format!("{:x}", digest.finalize()));
    write_private(&path, &serde_json::to_vec(&manifest).unwrap());

    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
    assert!(matches!(
        coordinator.inspect(),
        Err(StorageError::InvalidStorage(
            "frozen manifest does not match migration state"
        ))
    ));
    assert!(coordinator.resume().is_err());
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_crashes_resume_to_one_complete_generation() {
    for fault in [
        "after-freeze-building-state-sync",
        "after-freeze-progress-ready-state-sync",
        "freeze-preparing-synced",
        "freeze-temp-synced",
        "freeze-prepared-record-synced",
        "freeze-entry-published",
        "freeze-progress-synced",
        "after-directory-freezing-state-sync",
        "after-journal-directory-chmod",
        "after-directory-frozen-state-sync",
        "after-manifest-building-state-sync",
        "after-manifest-temp-sync",
        "after-manifest-verified-state-sync",
        "after-manifest-publication",
        "after-manifest-published-state-sync",
        "after-legacy-frozen",
        "after-database-complete",
        "after-complete-state",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(
            !migration_child(fixture.state_root(), fault).success(),
            "fault seam did not terminate the child: {fault}"
        );
        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert_eq!(
            coordinator
                .resume()
                .unwrap_or_else(|error| panic!("resume failed at {fault}: {error:?}")),
            MigrationStatus::Complete,
            "{fault}"
        );
        assert_eq!(
            coordinator
                .resume()
                .unwrap_or_else(|error| panic!("second resume failed at {fault}: {error:?}")),
            MigrationStatus::Complete,
            "{fault}"
        );
        FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_resume_rejects_unaccounted_published_brain_payload_corruption() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "freeze-prepared-record-synced").success());
    let database = fixture.state_root().join("db/brain.sqlite3");
    let connection = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE decision_payloads SET note = 'corrupt-unaccounted-payload'",
                [],
            )
            .unwrap(),
        1
    );
    connection.close().unwrap();
    assert_no_sqlite_sidecars(fixture.state_root());

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_resume_rejects_same_bytes_published_brain_inode_substitution() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "freeze-prepared-record-synced").success());
    let database = fixture.state_root().join("db/brain.sqlite3");
    let original = fixture.state_root().join("brain.sqlite3.original");
    let original_inode = fs::metadata(&database).unwrap().ino();
    fs::rename(&database, &original).unwrap();
    fs::copy(&original, &database).unwrap();
    fs::set_permissions(&database, fs::Permissions::from_mode(0o600)).unwrap();
    assert_ne!(fs::metadata(&database).unwrap().ino(), original_inode);

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_rejects_progress_directory_and_record_substitution() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(
        !migration_child(
            fixture.state_root(),
            "after-freeze-progress-ready-state-sync",
        )
        .success()
    );
    let progress = freeze_progress_path(fixture.state_root());
    fs::rename(&progress, progress.with_extension("saved")).unwrap();
    fs::create_dir(&progress).unwrap();
    fs::set_permissions(&progress, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(
            "freeze progress directory identity changed"
        ))
    ));

    for mutation in ["symlink", "hardlink", "contents"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(!migration_child(fixture.state_root(), "freeze-preparing-synced").success());
        let record = freeze_progress_path(fixture.state_root()).join("0000.json");
        let saved = record.with_extension("saved");
        match mutation {
            "symlink" => {
                fs::rename(&record, &saved).unwrap();
                symlink(&saved, &record).unwrap();
            }
            "hardlink" => {
                fs::hard_link(&record, &saved).unwrap();
            }
            "contents" => write_private(&record, b"{}"),
            _ => unreachable!(),
        }
        assert!(
            MigrationCoordinator::at(fixture.state_root())
                .resume()
                .is_err(),
            "{mutation}"
        );
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_rejects_source_race_after_building_state_is_durable() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "after-freeze-building-state-sync").success());
    OpenOptions::new()
        .append(true)
        .open(fixture.state_root().join("activity.jsonl"))
        .unwrap()
        .write_all(b"racing-writer\n")
        .unwrap();

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(
            "legacy sources changed after Brain publication"
        ))
    ));
    assert!(
        !fixture
            .state_root()
            .join("db/.brain.sqlite3.frozen-manifest.json")
            .exists()
    );
}

#[test]
#[cfg(feature = "fault-injection")]
fn freeze_rejects_review_race_after_review_publication() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    assert!(!migration_child(fixture.state_root(), "before-freeze-guard").success());
    OpenOptions::new()
        .append(true)
        .open(fixture.state_root().join("review-state.json"))
        .unwrap()
        .write_all(b" \n")
        .unwrap();

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(
            "published Review source changed before freeze"
        ))
    ));
}

#[test]
fn frozen_manifest_rejects_inode_substitution_and_absent_source_recreation() {
    for relative in [
        "activity.jsonl",
        "brain/decisions.jsonl",
        "brain/permission-transactions/permission-transaction-000000000000000000000000000000000000001-0000000001-00000000000000000001.json",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete
        );
        let path = fixture.state_root().join(relative);
        let original = fs::read(&path).unwrap();
        let frozen_parent = relative.starts_with("brain/permission-transactions/");
        if frozen_parent {
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::rename(&path, path.with_extension("saved")).unwrap();
        write_private(&path, &original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        if frozen_parent {
            fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o500)).unwrap();
        }

        assert!(FrozenSourceManifest::load_and_validate(fixture.state_root()).is_err());
    }

    for relative in ["hooks/lifecycle.json", "review-state.json"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete
        );
        write_private(&fixture.state_root().join(relative), b"recreated\n");

        assert!(FrozenSourceManifest::load_and_validate(fixture.state_root()).is_err());
    }
}

#[test]
fn preopened_legacy_writer_cannot_diverge_from_frozen_manifest() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let path = fixture.state_root().join("activity.jsonl");
    let mut old_writer = OpenOptions::new().write(true).open(&path).unwrap();
    let old_inode = old_writer.metadata().unwrap().ino();

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    assert_ne!(fs::metadata(&path).unwrap().ino(), old_inode);
    old_writer.write_all(b"detached-old-inode").unwrap();
    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
}

#[test]
fn degraded_review_is_preserved_mutable_and_excluded_from_manifest() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let review = fixture.state_root().join("review-state.json");
    write_private(&review, b"{malformed-review");
    let before = fs::read(&review).unwrap();
    let inode = fs::metadata(&review).unwrap().ino();

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    assert_eq!(fs::read(&review).unwrap(), before);
    assert_eq!(fs::metadata(&review).unwrap().ino(), inode);
    assert_eq!(
        fs::metadata(&review).unwrap().permissions().mode() & 0o777,
        0o600
    );
    write_private(&review, b"still-owned-by-legacy-review\n");
    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
}

#[test]
fn session_links_remain_byte_identical_writable_and_outside_manifest() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let links = fixture.state_root().join("session-links.jsonl");
    write_private(&links, b"opaque-session-link\n");
    let before = fs::read(&links).unwrap();
    let inode = fs::metadata(&links).unwrap().ino();

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    assert_eq!(fs::read(&links).unwrap(), before);
    assert_eq!(fs::metadata(&links).unwrap().ino(), inode);
    assert_eq!(
        fs::metadata(&links).unwrap().permissions().mode() & 0o777,
        0o600
    );
    OpenOptions::new()
        .append(true)
        .open(&links)
        .unwrap()
        .write_all(b"new-link\n")
        .unwrap();
    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
}

#[test]
fn malformed_review_degrades_without_invalidating_exact_brain_publication() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let review_path = fixture.state_root().join("review-state.json");
    write_private(&review_path, b"{malformed-review");
    let source_before = fs::metadata(&review_path).unwrap();
    let bytes_before = fs::read(&review_path).unwrap();

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let brain = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        brain
            .query_row("SELECT count(*) FROM activity_events", [], |row| row
                .get::<_, i64>(0),)
            .unwrap(),
        2
    );
    assert_eq!(
        brain
            .query_row(
                "SELECT response_eligible, delivery_state
                 FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap(),
        (0, "unknown".to_owned())
    );
    let state = migration_state(fixture.state_root());
    assert_eq!(state["review_result"]["status"], "degraded");
    assert_eq!(state["review_result"]["reason"], "malformed");
    assert!(!fixture.state_root().join("db/review.sqlite3").exists());

    let source_after = fs::metadata(&review_path).unwrap();
    assert_eq!(source_after.dev(), source_before.dev());
    assert_eq!(source_after.ino(), source_before.ino());
    assert_eq!(fs::read(&review_path).unwrap(), bytes_before);
}

#[test]
fn missing_review_source_publishes_an_empty_current_review_database() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let review =
        rusqlite::Connection::open(fixture.state_root().join("db/review.sqlite3")).unwrap();
    assert_eq!(
        review
            .query_row("SELECT count(*) FROM review_marks", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        migration_state(fixture.state_root())["review_result"]["status"],
        "published"
    );
}

#[test]
fn review_migration_preserves_all_surface_cursors_revisions_and_latest_archive() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let activity_path = fixture.state_root().join("activity.jsonl");
    let mut activity = fs::read_to_string(&activity_path).unwrap();
    activity.push_str(
        "{\"schema_version\":3,\"kind\":\"diagnostic\",\"activity_id\":\"diagnostic-review\",\"recorded_at_ms\":4,\"project\":{\"project_id\":{\"kind\":\"temporary\",\"value\":\"fixture\"},\"cwd\":\"/fixture\"},\"state\":\"error\"}\n",
    );
    write_private(&activity_path, activity.as_bytes());
    let attention = ReviewKey::derive(ReviewSurface::Attention, b"activity-4vh58");
    let recent = ReviewKey::derive(ReviewSurface::Recent, b"activity-4vh58");
    let diagnostics = ReviewKey::derive(ReviewSurface::Diagnostics, b"diagnostic-review");
    let review = ReviewKey::derive(ReviewSurface::Review, b"decision-4vh58");
    write_private(
        &fixture.state_root().join("review-state.json"),
        serde_json::to_string(&serde_json::json!({
            "schema_version": 1,
            "surfaces": {
                "attention": {"revision": 2, "items": {attention.to_string(): "reviewed"}},
                "review": {"revision": 2, "items": {review.to_string(): "archived"},
                           "last_archive": [review.to_string()]},
                "diagnostics": {"revision": 2, "items": {diagnostics.to_string(): "archived"}},
                "recent": {"revision": 1, "items": {recent.to_string(): "reviewed"}}
            }
        }))
        .unwrap()
        .as_bytes(),
    );

    MigrationCoordinator::at(fixture.state_root())
        .run_non_hook()
        .unwrap();
    let db = rusqlite::Connection::open(fixture.state_root().join("db/review.sqlite3")).unwrap();
    let rows = db
        .prepare(
            "SELECT surface, group_id, source_cursor, disposition, revision
             FROM review_marks ORDER BY surface",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "attention".into(),
                attention.to_string(),
                2,
                "reviewed".into(),
                1
            ),
            (
                "diagnostics".into(),
                diagnostics.to_string(),
                3,
                "archived".into(),
                1
            ),
            ("recent".into(), recent.to_string(), 2, "reviewed".into(), 1),
            ("review".into(), review.to_string(), 1, "archived".into(), 2),
        ]
    );
    assert_eq!(
        db.query_row(
            "SELECT revision, source_high_water, last_archive_revision
             FROM review_meta WHERE surface = 'review'",
            [],
            |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?
            )),
        )
        .unwrap(),
        (2, 3, 2)
    );
}

#[test]
fn unmapped_review_key_degrades_without_partial_review_publication() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let review_path = fixture.state_root().join("review-state.json");
    let unmapped = ReviewKey::derive(ReviewSurface::Review, b"unanchored-decision");
    write_private(
        &review_path,
        serde_json::to_string(&serde_json::json!({
            "schema_version": 1,
            "surfaces": {"review": {"revision": 1, "items": {
                unmapped.to_string(): "reviewed"
            }}}
        }))
        .unwrap()
        .as_bytes(),
    );
    let before = fs::read(&review_path).unwrap();

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    assert_eq!(
        migration_state(fixture.state_root())["review_result"]["reason"],
        "unmapped"
    );
    assert!(!fixture.state_root().join("db/review.sqlite3").exists());
    assert_eq!(fs::read(review_path).unwrap(), before);
}

#[test]
#[cfg(feature = "fault-injection")]
fn review_source_mutation_after_brain_publication_does_not_invalidate_brain() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
    write_private(
        &fixture.state_root().join("review-state.json"),
        b"{malformed-after-brain",
    );
    let coordinator = MigrationCoordinator::at(fixture.state_root());

    assert_eq!(
        coordinator.inspect().unwrap(),
        MigrationStatus::BrainPublishedIncomplete
    );
    assert_eq!(coordinator.resume().unwrap(), MigrationStatus::Complete);
    assert_eq!(
        migration_state(fixture.state_root())["review_result"]["reason"],
        "malformed"
    );
    let brain = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        brain
            .query_row("SELECT count(*) FROM activity_events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn published_review_source_change_is_split_brain_after_completion() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    coordinator.run_non_hook().unwrap();
    assert_eq!(
        migration_state(fixture.state_root())["review_result"]["status"],
        "published"
    );
    write_private(
        &fixture.state_root().join("review-state.json"),
        b"{changed-after-review-publication",
    );

    assert!(coordinator.resume().is_err());
    assert!(fixture.state_root().join("db/brain.sqlite3").exists());
}

#[test]
#[cfg(feature = "fault-injection")]
fn review_publication_resumes_verified_link_and_result_boundaries() {
    for fault in [
        "review-verified",
        "after-review-link",
        "after-review-publication",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(
            !migration_child(fixture.state_root(), fault).success(),
            "{fault}"
        );
        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert_eq!(
            coordinator.inspect().unwrap(),
            MigrationStatus::BrainPublishedIncomplete,
            "{fault}"
        );
        assert_eq!(
            coordinator.resume().unwrap(),
            MigrationStatus::Complete,
            "{fault}"
        );
        assert_eq!(
            migration_state(fixture.state_root())["review_result"]["status"],
            "published"
        );
        assert!(fixture.state_root().join("db/review.sqlite3").exists());
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn unowned_same_name_review_staging_is_preserved_and_rejected() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
    let generation = migration_state(fixture.state_root())["generation"]
        .as_u64()
        .unwrap();
    let staging = fixture
        .state_root()
        .join(format!("db/.review.sqlite3.migration-{generation}"));
    write_private(&staging, b"unowned-review-staging-evidence");
    let before = tree_snapshot(fixture.state_root());

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).resume(),
        Err(StorageError::InvalidStorage(_))
    ));
    assert_eq!(tree_snapshot(fixture.state_root()), before);
}

#[test]
#[cfg(feature = "fault-injection")]
fn malformed_extra_and_unsafe_review_temps_are_preserved_and_rejected() {
    for kind in [
        "malformed-result",
        "extra-result",
        "unsafe-result",
        "extra-staging",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
        let generation = migration_state(fixture.state_root())["generation"]
            .as_u64()
            .unwrap();
        let database_dir = fixture.state_root().join("db");
        let exact_result =
            database_dir.join(format!(".brain.sqlite3.review-result-{generation}.tmp"));
        match kind {
            "malformed-result" => write_private(&exact_result, b"{malformed"),
            "extra-result" => write_private(
                &database_dir.join(".brain.sqlite3.review-result-extra.tmp"),
                b"extra",
            ),
            "unsafe-result" => {
                write_private(&exact_result, b"unsafe");
                fs::hard_link(&exact_result, database_dir.join("review-result-evidence")).unwrap();
            }
            "extra-staging" => write_private(
                &database_dir.join(".review.sqlite3.migration-extra"),
                b"extra",
            ),
            _ => unreachable!(),
        }
        let before = tree_snapshot(fixture.state_root());
        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert!(coordinator.inspect().is_err(), "{kind}");
        assert!(coordinator.resume().is_err(), "{kind}");
        assert_eq!(tree_snapshot(fixture.state_root()), before, "{kind}");
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn review_build_and_result_temp_crashes_resume_exactly() {
    for fault in [
        "after-review-result-state-temp-sync",
        "review-before-create",
        "review-building",
        "after-review-staging-sync",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        let review_source = fixture.state_root().join("review-state.json");
        let source_before = review_source.exists().then(|| {
            (
                fs::metadata(&review_source).unwrap(),
                fs::read(&review_source).unwrap(),
            )
        });
        assert!(
            !migration_child(fixture.state_root(), fault).success(),
            "{fault}"
        );
        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert_eq!(
            coordinator.inspect().unwrap(),
            MigrationStatus::BrainPublishedIncomplete,
            "{fault}"
        );
        assert_eq!(
            coordinator.resume().unwrap(),
            MigrationStatus::Complete,
            "{fault}"
        );
        assert_eq!(
            migration_state(fixture.state_root())["review_result"]["status"],
            "published",
            "{fault}"
        );
        if let Some((source_before, source_bytes)) = source_before {
            let source_after = fs::metadata(&review_source).unwrap();
            assert_eq!(fs::read(&review_source).unwrap(), source_bytes, "{fault}");
            assert_eq!(source_after.dev(), source_before.dev(), "{fault}");
            assert_eq!(source_after.ino(), source_before.ino(), "{fault}");
            assert_eq!(source_after.mtime(), source_before.mtime(), "{fault}");
            assert_eq!(
                source_after.mtime_nsec(),
                source_before.mtime_nsec(),
                "{fault}"
            );
        } else {
            assert!(!review_source.exists(), "{fault}");
        }
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn review_source_race_result_temp_recovers_with_changed_fingerprint() {
    let boundary = "building";
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    let coordinator = MigrationCoordinator::at(fixture.state_root());
    assert!(!migration_child(fixture.state_root(), "review-before-create").success());
    write_private(
        &fixture.state_root().join("review-state.json"),
        b"{changed-for-source-race",
    );
    let changed_source = fixture.state_root().join("review-state.json");
    let source_before = fs::metadata(&changed_source).unwrap();
    let source_bytes = fs::read(&changed_source).unwrap();
    assert!(
        !migration_child(fixture.state_root(), "after-review-result-state-temp-sync").success(),
        "{boundary}"
    );
    assert_eq!(
        coordinator.inspect().unwrap(),
        MigrationStatus::BrainPublishedIncomplete,
        "{boundary}"
    );
    assert_eq!(
        coordinator.resume().unwrap(),
        MigrationStatus::Complete,
        "{boundary}"
    );
    assert_eq!(
        migration_state(fixture.state_root())["review_result"]["reason"],
        "source_race",
        "{boundary}"
    );
    let source_after = fs::metadata(&changed_source).unwrap();
    assert_eq!(
        fs::read(changed_source).unwrap(),
        source_bytes,
        "{boundary}"
    );
    assert_eq!(source_after.dev(), source_before.dev(), "{boundary}");
    assert_eq!(source_after.ino(), source_before.ino(), "{boundary}");
    assert_eq!(source_after.mtime(), source_before.mtime(), "{boundary}");
    assert_eq!(
        source_after.mtime_nsec(),
        source_before.mtime_nsec(),
        "{boundary}"
    );
}

#[test]
#[cfg(feature = "fault-injection")]
fn verified_review_same_size_corruption_is_rejected_without_deletion() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "review-verified").success());
    let staging = review_staging_path(fixture.state_root());
    let mut bytes = fs::read(&staging).unwrap();
    let index = bytes.len() - 1;
    bytes[index] ^= 1;
    fs::write(&staging, &bytes).unwrap();

    assert!(
        MigrationCoordinator::at(fixture.state_root())
            .resume()
            .is_err()
    );
    assert_eq!(fs::read(&staging).unwrap(), bytes);
    assert!(!fixture.state_root().join("db/review.sqlite3").exists());
}

fn migration_state(root: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(root.join("db/.brain.sqlite3.migration-state.json")).unwrap())
        .unwrap()
}

fn rewrite_json_lines(
    path: &std::path::Path,
    mut rewrite: impl FnMut(usize, &mut serde_json::Value),
) {
    let input = fs::read_to_string(path).unwrap();
    let mut output = String::new();
    for (index, line) in input.lines().enumerate() {
        let mut value = serde_json::from_str(line).unwrap();
        rewrite(index, &mut value);
        output.push_str(&serde_json::to_string(&value).unwrap());
        output.push('\n');
    }
    write_private(path, output.as_bytes());
}

fn remove_json_lines(path: &std::path::Path, mut remove: impl FnMut(&serde_json::Value) -> bool) {
    let input = fs::read_to_string(path).unwrap();
    let mut output = String::new();
    for line in input.lines() {
        let value = serde_json::from_str(line).unwrap();
        if !remove(&value) {
            output.push_str(line);
            output.push('\n');
        }
    }
    write_private(path, output.as_bytes());
}

#[test]
fn legacy_brain_source_alias_does_not_promote_incomplete_proposals() {
    for case in ["missing-terminal", "session-mismatch", "abstain"] {
        let fixture = LegacyFixture::copy("legacy-brain-proposals");
        match case {
            "missing-terminal" => {
                remove_json_lines(&fixture.state_root().join("activity.jsonl"), |value| {
                    value["decision_id"] == "legacy-brain-allow"
                })
            }
            "session-mismatch" => rewrite_json_lines(
                &fixture.state_root().join("brain/decisions.jsonl"),
                |index, value| {
                    if index == 0 {
                        value["session_id"] = serde_json::Value::from("other-session");
                    }
                },
            ),
            "abstain" => rewrite_json_lines(
                &fixture.state_root().join("brain/decisions.jsonl"),
                |index, value| {
                    if index == 0 {
                        value["brain_action"] = serde_json::Value::from("abstain");
                    }
                },
            ),
            _ => unreachable!(),
        }

        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete,
            "{case}"
        );
        let db = BrainDb::open_current(
            &StoragePaths::at(fixture.state_root()),
            OpenRole::NonHook,
            StorageDeadline::after(Duration::from_secs(2)),
        )
        .unwrap();
        assert_eq!(db.decision_identity("legacy-brain-allow").unwrap(), None);
        assert!(
            db.historical_permission_authority_after(None, 10, 1024 * 1024)
                .unwrap()
                .authorities
                .iter()
                .all(|row| row.decision_id != "legacy-brain-allow"),
            "{case}"
        );
        assert_eq!(
            migration_state(fixture.state_root())["accounting"]["skips"]["incomplete_proposals"],
            1,
            "{case}"
        );
    }
}

#[test]
fn legacy_migration_rejects_unknown_exact_proposal_source() {
    let fixture = LegacyFixture::copy("legacy-brain-proposals");
    rewrite_json_lines(
        &fixture.state_root().join("brain/decisions.jsonl"),
        |index, value| {
            if index == 0 {
                value["brain_source"] = serde_json::Value::from("future_source");
            }
        },
    );
    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
        Err(StorageError::InvalidStorage(
            "legacy proposal decision source is unsupported"
        ))
    ));
}

fn duplicate_permission_evidence(state_root: &std::path::Path) {
    let decisions = state_root.join("brain/decisions.jsonl");
    let mut proposal: serde_json::Value = serde_json::from_str(
        fs::read_to_string(&decisions)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    proposal["decision_id"] = serde_json::Value::from("decision-4vh58-second");
    let mut decision_bytes = fs::read(&decisions).unwrap();
    decision_bytes.extend_from_slice(&serde_json::to_vec(&proposal).unwrap());
    decision_bytes.push(b'\n');
    write_private(&decisions, &decision_bytes);

    let activities = state_root.join("activity.jsonl");
    let mut terminal: serde_json::Value = serde_json::from_str(
        fs::read_to_string(&activities)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    terminal["activity_id"] = serde_json::Value::from("activity-4vh58-second");
    terminal["decision_id"] = serde_json::Value::from("decision-4vh58-second");
    terminal["recorded_at_ms"] = serde_json::Value::from(4);
    let mut activity_bytes = fs::read(&activities).unwrap();
    activity_bytes.extend_from_slice(&serde_json::to_vec(&terminal).unwrap());
    activity_bytes.push(b'\n');
    write_private(&activities, &activity_bytes);
}

#[cfg(feature = "fault-injection")]
fn assert_sqlite_valid(database: &rusqlite::Connection) {
    assert_eq!(
        database
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert!(
        database
            .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
            .optional()
            .unwrap()
            .is_none()
    );
}

#[cfg(feature = "fault-injection")]
fn assert_restart_rejects_corruption(state_root: &std::path::Path) {
    assert!(matches!(
        MigrationCoordinator::at(state_root).resume(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
#[cfg(feature = "fault-injection")]
fn restart_revalidation_rejects_exact_activity_payload_session_and_tool_corruption() {
    for corruption in ["payload", "session", "tool"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
        let database =
            rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        let payload = database
            .query_row(
                "SELECT event_payload FROM activity_events WHERE source_cursor = 1",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap();
        let mut event: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        match corruption {
            "payload" => event["reasoning"] = serde_json::Value::from("corrupt-reasoning"),
            "session" => event["session"]["cwd"] = serde_json::Value::from("/other"),
            "tool" => event["tool"] = serde_json::Value::from("OtherTool"),
            _ => unreachable!(),
        }
        database
            .execute(
                "UPDATE activity_events SET event_payload = ?1 WHERE source_cursor = 1",
                [serde_json::to_vec(&event).unwrap()],
            )
            .unwrap();
        assert_sqlite_valid(&database);
        drop(database);

        assert_restart_rejects_corruption(fixture.state_root());
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn restart_revalidation_rejects_exact_corruption_at_verified_staging_boundary() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "verified").success());
    let database = rusqlite::Connection::open(staging_path(fixture.state_root())).unwrap();
    let payload = database
        .query_row(
            "SELECT event_payload FROM activity_events WHERE source_cursor = 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .unwrap();
    let mut event: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    event["reasoning"] = serde_json::Value::from("corrupt-staging-reasoning");
    database
        .execute(
            "UPDATE activity_events SET event_payload = ?1 WHERE source_cursor = 1",
            [serde_json::to_vec(&event).unwrap()],
        )
        .unwrap();
    assert_sqlite_valid(&database);
    drop(database);

    assert_restart_rejects_corruption(fixture.state_root());
}

#[test]
#[cfg(feature = "fault-injection")]
fn restart_revalidation_rejects_exact_decision_and_historical_tuple_corruption() {
    for corruption in ["identity", "payload", "journal-request-key"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
        let database =
            rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        match corruption {
            "identity" => {
                database
                    .execute(
                        "UPDATE decision_identities SET provider = 'claude'
                         WHERE decision_id = 'decision-4vh58'",
                        [],
                    )
                    .unwrap();
            }
            "payload" => {
                let payload = database
                    .query_row(
                        "SELECT decision_record FROM decision_payloads
                         WHERE decision_id = 'decision-4vh58'",
                        [],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .unwrap();
                let mut record: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                record["brain_reasoning"] = serde_json::Value::from("corrupt-reasoning");
                database
                    .execute(
                        "UPDATE decision_payloads SET decision_record = ?1
                         WHERE decision_id = 'decision-4vh58'",
                        [serde_json::to_vec(&record).unwrap()],
                    )
                    .unwrap();
            }
            "journal-request-key" => {
                database
                    .execute(
                        "UPDATE historical_permission_authority SET request_key = ?1
                         WHERE decision_id = 'decision-4vh58'",
                        ["b".repeat(64)],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_sqlite_valid(&database);
        drop(database);

        assert_restart_rejects_corruption(fixture.state_root());
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn restart_revalidation_rejects_exact_imported_lifecycle_corruption() {
    for corruption in ["snapshot", "correlation-tuple"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        remove_fixture_journal(fixture.state_root());
        write_lifecycle_authorities(
            fixture.state_root(),
            AgentProvider::Codex,
            "session-4vh58",
            "turn-4vh58",
            "/fixture",
            &[(
                &"a".repeat(64),
                "transaction-lifecycle",
                PermissionAction::Allow,
            )],
        );
        assert!(!migration_child(fixture.state_root(), "after-brain-publication").success());
        let database =
            rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        match corruption {
            "snapshot" => {
                database
                    .execute(
                        "UPDATE lifecycle_sessions SET cwd = ?1
                         WHERE provider = 'codex' AND session_id = 'session-4vh58'",
                        [b"/other".as_slice()],
                    )
                    .unwrap();
            }
            "correlation-tuple" => {
                database
                    .execute(
                        "UPDATE historical_permission_authority
                         SET request_key = ?1, transaction_id = 'transaction-corrupt'
                         WHERE decision_id = 'decision-4vh58'",
                        ["b".repeat(64)],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_sqlite_valid(&database);
        drop(database);

        assert_restart_rejects_corruption(fixture.state_root());
    }
}

#[test]
fn migration_correlates_one_exact_retained_lifecycle_authority_without_live_capability() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    remove_fixture_journal(fixture.state_root());
    write_lifecycle_authorities(
        fixture.state_root(),
        AgentProvider::Codex,
        "session-4vh58",
        "turn-4vh58",
        "/fixture",
        &[(
            &"a".repeat(64),
            "transaction-lifecycle",
            PermissionAction::Allow,
        )],
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT provenance_kind, transaction_id, request_key,
                    response_eligible, delivery_state
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
            )),
        )
        .unwrap(),
        (
            "lifecycle_correlated".to_owned(),
            "transaction-lifecycle".to_owned(),
            "a".repeat(64),
            0,
            "unknown".to_owned(),
        )
    );
    assert_eq!(
        db.query_row(
            "SELECT (SELECT count(*) FROM permission_attempts),
                    (SELECT count(*) FROM permission_commits)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        (0, 0)
    );
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["historical"]["lifecycle_correlated"],
        1
    );
}

#[test]
fn migration_lifecycle_correlates_exact_antigravity_request_step_authority() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    remove_fixture_journal(fixture.state_root());
    rewrite_fixture_as_antigravity(fixture.state_root());
    let request_key = "1".repeat(64);
    write_antigravity_lifecycle_authority(
        fixture.state_root(),
        "step-5",
        &request_key,
        "transaction-antigravity",
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT provenance_kind, transaction_id, request_key, response_eligible
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            )),
        )
        .unwrap(),
        (
            "lifecycle_correlated".to_owned(),
            "transaction-antigravity".to_owned(),
            request_key,
            0,
        )
    );
}

#[test]
fn migration_lifecycle_does_not_correlate_antigravity_authority_from_wrong_step() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    remove_fixture_journal(fixture.state_root());
    rewrite_fixture_as_antigravity(fixture.state_root());
    write_antigravity_lifecycle_authority(
        fixture.state_root(),
        "step-6",
        &"2".repeat(64),
        "transaction-wrong-step",
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT provenance_kind, transaction_id, request_key
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            )),
        )
        .unwrap(),
        ("proposal_terminal".to_owned(), None, None)
    );
}

#[test]
fn migration_does_not_attach_one_lifecycle_authority_to_ambiguous_historical_rows() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    remove_fixture_journal(fixture.state_root());
    duplicate_permission_evidence(fixture.state_root());
    write_lifecycle_authorities(
        fixture.state_root(),
        AgentProvider::Codex,
        "session-4vh58",
        "turn-4vh58",
        "/fixture",
        &[(
            &"b".repeat(64),
            "transaction-ambiguous-row",
            PermissionAction::Allow,
        )],
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM historical_permission_authority
             WHERE provenance_kind = 'proposal_terminal'
               AND transaction_id IS NULL AND request_key IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        2
    );
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["historical"]["lifecycle_correlated"],
        0
    );
}

#[test]
fn migration_does_not_choose_between_ambiguous_retained_lifecycle_authorities() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    remove_fixture_journal(fixture.state_root());
    let first_key = "c".repeat(64);
    let second_key = "d".repeat(64);
    write_lifecycle_authorities(
        fixture.state_root(),
        AgentProvider::Codex,
        "session-4vh58",
        "turn-4vh58",
        "/fixture",
        &[
            (&first_key, "transaction-first", PermissionAction::Allow),
            (&second_key, "transaction-second", PermissionAction::Allow),
        ],
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT provenance_kind, transaction_id, request_key
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            )),
        )
        .unwrap(),
        ("proposal_terminal".to_owned(), None, None)
    );
}

#[test]
fn migration_does_not_correlate_mismatched_lifecycle_identity_facts() {
    for mismatch in [
        "provider",
        "session",
        "turn",
        "action",
        "provider-session",
        "cwd",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        remove_fixture_journal(fixture.state_root());
        let (provider, session, turn, action, cwd) = match mismatch {
            "provider" => (
                AgentProvider::Claude,
                "session-4vh58",
                "turn-4vh58",
                PermissionAction::Allow,
                "/fixture",
            ),
            "session" => (
                AgentProvider::Codex,
                "other-session",
                "turn-4vh58",
                PermissionAction::Allow,
                "/fixture",
            ),
            "turn" => (
                AgentProvider::Codex,
                "session-4vh58",
                "other-turn",
                PermissionAction::Allow,
                "/fixture",
            ),
            "action" => (
                AgentProvider::Codex,
                "session-4vh58",
                "turn-4vh58",
                PermissionAction::Deny,
                "/fixture",
            ),
            "provider-session" => {
                rewrite_json_lines(
                    &fixture.state_root().join("activity.jsonl"),
                    |index, value| {
                        if index == 0 {
                            value["session"]["provider_session_id"] =
                                serde_json::Value::from("parent-session");
                        }
                    },
                );
                (
                    AgentProvider::Codex,
                    "session-4vh58",
                    "turn-4vh58",
                    PermissionAction::Allow,
                    "/fixture",
                )
            }
            "cwd" => (
                AgentProvider::Codex,
                "session-4vh58",
                "turn-4vh58",
                PermissionAction::Allow,
                "/other",
            ),
            _ => unreachable!(),
        };
        let request_key = "e".repeat(64);
        write_lifecycle_authorities(
            fixture.state_root(),
            provider,
            session,
            turn,
            cwd,
            &[(request_key.as_str(), "transaction-mismatch", action)],
        );

        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete,
            "{mismatch}"
        );
        let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT provenance_kind, transaction_id, request_key
                 FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
                [],
                |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                )),
            )
            .unwrap(),
            ("proposal_terminal".to_owned(), None, None),
            "{mismatch}"
        );
    }
}

#[test]
fn migration_gives_exact_journal_correlation_precedence_over_lifecycle() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    write_lifecycle_authorities(
        fixture.state_root(),
        AgentProvider::Codex,
        "session-4vh58",
        "turn-4vh58",
        "/fixture",
        &[(
            &"f".repeat(64),
            "transaction-lifecycle-loses",
            PermissionAction::Allow,
        )],
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT provenance_kind, transaction_id, request_key
             FROM historical_permission_authority WHERE decision_id = 'decision-4vh58'",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            )),
        )
        .unwrap(),
        (
            "journal_correlated".to_owned(),
            "transaction-4vh58".to_owned(),
            "a".repeat(64),
        )
    );
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["historical"],
        serde_json::json!({
            "proposal_terminal": 0, "journal_correlated": 1, "lifecycle_correlated": 0
        })
    );
}

#[test]
fn migration_does_not_create_historical_or_live_authority_from_lifecycle_alone() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    write_lifecycle_authorities(
        fixture.state_root(),
        AgentProvider::Codex,
        "session-only",
        "turn-only",
        "/fixture",
        &[(&"0".repeat(64), "transaction-only", PermissionAction::Allow)],
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT (SELECT count(*) FROM historical_permission_authority),
                    (SELECT count(*) FROM permission_attempts),
                    (SELECT count(*) FROM permission_commits)",
            [],
            |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            )),
        )
        .unwrap(),
        (0, 0, 0)
    );
}

#[test]
fn migration_imports_legacy_v0591_as_incomplete_non_authority() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );

    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*), min(source_cursor), max(source_cursor) FROM activity_events",
            [],
            |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?
            )),
        )
        .unwrap(),
        (1, 1, 1)
    );
    assert_eq!(
        db.query_row(
            "SELECT activity_high_water FROM schema_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT next_sequence FROM lifecycle_meta WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    for table in [
        "historical_permission_authority",
        "permission_attempts",
        "permission_commits",
    ] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            0,
            "{table}"
        );
    }
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["skips"]["incomplete_proposals"],
        1
    );
}

#[test]
fn migration_keeps_mismatched_proposals_incomplete_but_rejects_critical_duplicates() {
    for mismatch in ["provider", "session", "action", "missing-session"] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        if mismatch == "provider" {
            rewrite_json_lines(
                &fixture.state_root().join("brain/decisions.jsonl"),
                |_, value| value["provider"] = serde_json::Value::from("claude"),
            );
        } else if mismatch == "session" {
            rewrite_json_lines(
                &fixture.state_root().join("brain/decisions.jsonl"),
                |_, value| value["session_id"] = serde_json::Value::from("other-session"),
            );
        } else if mismatch == "action" {
            rewrite_json_lines(
                &fixture.state_root().join("activity.jsonl"),
                |index, value| {
                    if index == 0 {
                        value["state"] = serde_json::Value::from("denied");
                    }
                },
            );
        } else {
            rewrite_json_lines(
                &fixture.state_root().join("activity.jsonl"),
                |index, value| {
                    if index == 0 {
                        value.as_object_mut().unwrap().remove("session");
                    }
                },
            );
        }
        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete,
            "{mismatch}"
        );
        let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        for table in [
            "historical_permission_authority",
            "decision_identities",
            "permission_attempts",
            "permission_commits",
        ] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
                0,
                "{mismatch}: {table}"
            );
        }
        assert_eq!(
            migration_state(fixture.state_root())["accounting"]["skips"]["incomplete_proposals"],
            1,
            "{mismatch}"
        );
    }

    for duplicate in ["proposal", "proposal-only", "terminal"] {
        let fixture = LegacyFixture::copy(if duplicate == "proposal-only" {
            "legacy-v0.59.1"
        } else {
            "permission-journal-4vh58"
        });
        let path = if matches!(duplicate, "proposal" | "proposal-only") {
            fixture.state_root().join("brain/decisions.jsonl")
        } else {
            fixture.state_root().join("activity.jsonl")
        };
        let mut bytes = fs::read(&path).unwrap();
        let first = bytes.split(|byte| *byte == b'\n').next().unwrap().to_vec();
        bytes.extend_from_slice(&first);
        bytes.push(b'\n');
        write_private(&path, &bytes);
        assert!(matches!(
            MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
            Err(StorageError::Sqlite(_)) | Err(StorageError::InvalidStorage(_))
        ));
    }
}

#[test]
fn migration_imports_audit_decisions_only_with_exact_activity_anchors() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    rewrite_json_lines(&fixture.state_root().join("activity.jsonl"), |_, value| {
        value["decision_id"] = serde_json::Value::from("fixture-decision");
    });
    let activity_path = fixture.state_root().join("activity.jsonl");
    let mut outcome: serde_json::Value =
        serde_json::from_slice(&fs::read(&activity_path).unwrap()).unwrap();
    outcome["recorded_at_ms"] = serde_json::Value::from(2);
    outcome["state"] = serde_json::Value::from("outcome");
    outcome["outcome"] = serde_json::Value::from("succeeded");
    let mut activity = fs::read_to_string(&activity_path).unwrap();
    activity.push_str(&serde_json::to_string(&outcome).unwrap());
    activity.push('\n');
    write_private(&activity_path, activity.as_bytes());
    let decisions = fixture.state_root().join("brain/decisions.jsonl");
    let mut anchored: serde_json::Value =
        serde_json::from_slice(&fs::read(&decisions).unwrap()).unwrap();
    anchored["user_action"] = serde_json::Value::from("accept");
    let mut unanchored = anchored.clone();
    unanchored["decision_id"] = serde_json::Value::from("unanchored-decision");
    write_private(
        &decisions,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&anchored).unwrap(),
            serde_json::to_string(&unanchored).unwrap()
        )
        .as_bytes(),
    );

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT i.identity_kind, p.source_cursor
             FROM decision_identities i JOIN decision_payloads p USING (decision_id)
             WHERE i.decision_id = 'fixture-decision'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap(),
        ("observation".to_owned(), 1)
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM decision_identities WHERE decision_id = 'unanchored-decision'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    assert_eq!(
        migration_state(fixture.state_root())["accounting"]["skips"]["unanchored_audits"],
        1
    );
}

#[test]
fn migration_rejects_duplicate_unmatched_journal_transaction_ids() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    let journal = include_bytes!(
        "fixtures/storage/permission-journal-4vh58/brain/permission-transactions/permission-transaction-000000000000000000000000000000000000001-0000000001-00000000000000000001.json"
    );
    let directory = fixture.state_root().join("brain/permission-transactions");
    for index in 1..=2 {
        write_private(
            &directory.join(format!(
                "permission-transaction-{index:039}-0000000001-{index:020}.json"
            )),
            journal,
        );
    }
    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
        Err(StorageError::Sqlite(_))
    ));
}

#[test]
fn migration_rejects_ambiguous_audit_activity_anchors() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    let activity_path = fixture.state_root().join("activity.jsonl");
    let mut first: serde_json::Value =
        serde_json::from_slice(&fs::read(&activity_path).unwrap()).unwrap();
    first["decision_id"] = serde_json::Value::from("fixture-decision");
    let mut second = first.clone();
    second["activity_id"] = serde_json::Value::from("fixture-activity-second");
    write_private(
        &activity_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        )
        .as_bytes(),
    );
    rewrite_json_lines(
        &fixture.state_root().join("brain/decisions.jsonl"),
        |_, value| value["user_action"] = serde_json::Value::from("accept"),
    );

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
        Err(StorageError::InvalidStorage(
            "legacy audit decision has ambiguous activity anchors"
        ))
    ));
}

#[test]
fn migration_rejects_audit_anchor_with_conflicting_provider_facts() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    let terminal: serde_json::Value = serde_json::from_str(
        include_str!("fixtures/storage/permission-journal-4vh58/activity.jsonl")
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    rewrite_json_lines(&fixture.state_root().join("activity.jsonl"), |_, value| {
        value["decision_id"] = serde_json::Value::from("fixture-decision");
        value["session"] = terminal["session"].clone();
        value["session"]["provider"] = serde_json::Value::from("claude");
    });
    rewrite_json_lines(
        &fixture.state_root().join("brain/decisions.jsonl"),
        |_, value| value["user_action"] = serde_json::Value::from("accept"),
    );

    assert!(matches!(
        MigrationCoordinator::at(fixture.state_root()).run_non_hook(),
        Err(StorageError::InvalidStorage(
            "legacy audit decision and activity provider disagree"
        ))
    ));
}

#[test]
#[cfg(feature = "fault-injection")]
fn migration_persists_exact_fixed_accounting_before_verified() {
    let fixture = LegacyFixture::copy("permission-journal-4vh58");
    assert!(!migration_child(fixture.state_root(), "verified").success());
    let state = migration_state(fixture.state_root());
    let accounting = state["accounting"].as_object().unwrap();
    assert_eq!(accounting.len(), 7);
    assert_eq!(
        accounting["sources"],
        serde_json::json!({
            "decisions": 1, "activities": 2, "lifecycle_snapshots": 0,
            "journals": 1
        })
    );
    assert_eq!(
        accounting["imports"],
        serde_json::json!({"decisions": 1, "activities": 2, "lifecycle_snapshots": 0})
    );
    assert_eq!(
        accounting["skips"],
        serde_json::json!({
            "incomplete_proposals": 0, "unanchored_audits": 0, "unmatched_journals": 0
        })
    );
    assert_eq!(accounting["activity"]["count"], 2);
    assert_eq!(accounting["activity"]["high_water"], 2);
    assert_eq!(accounting["activity"]["first_cursor"], 1);
    assert_eq!(accounting["activity"]["last_cursor"], 2);
    assert_eq!(
        accounting["activity"]["order_digest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert_eq!(accounting["activity"].as_object().unwrap().len(), 5);
    assert_eq!(
        accounting["lifecycle"],
        serde_json::json!({
            "next_sequence": 1, "sessions": 0, "leases": 0, "turns": 0,
            "subagents": 0, "invocations": 0, "invocation_steps": 0
        })
    );
    assert_eq!(
        accounting["historical"],
        serde_json::json!({
            "proposal_terminal": 0, "journal_correlated": 1, "lifecycle_correlated": 0
        })
    );
    assert_eq!(accounting["table_counts"].as_object().unwrap().len(), 12);
}

#[test]
fn migration_streams_more_than_300_unmatched_journals_with_bounded_state() {
    let fixture = LegacyFixture::copy("legacy-v0.59.1");
    let journal_template: serde_json::Value = serde_json::from_slice(include_bytes!(
        "fixtures/storage/permission-journal-4vh58/brain/permission-transactions/permission-transaction-000000000000000000000000000000000000001-0000000001-00000000000000000001.json"
    ))
    .unwrap();
    let directory = fixture.state_root().join("brain/permission-transactions");
    for index in 0..320 {
        let mut journal = journal_template.clone();
        let suffix = format!("unmatched-{index:03}");
        journal["transaction_id"] = serde_json::Value::from(format!("transaction-{suffix}"));
        journal["proposal"]["decision_id"] = serde_json::Value::from(format!("decision-{suffix}"));
        journal["terminal"]["decision_id"] = serde_json::Value::from(format!("decision-{suffix}"));
        journal["terminal"]["activity_id"] = serde_json::Value::from(format!("activity-{suffix}"));
        write_private(
            &directory.join(format!(
                "permission-transaction-{index:039}-0000000001-{index:020}.json"
            )),
            &serde_json::to_vec(&journal).unwrap(),
        );
    }

    assert_eq!(
        MigrationCoordinator::at(fixture.state_root())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let state_bytes = fs::read(
        fixture
            .state_root()
            .join("db/.brain.sqlite3.migration-state.json"),
    )
    .unwrap();
    assert!(state_bytes.len() < 64 * 1024, "{}", state_bytes.len());
    let state: serde_json::Value = serde_json::from_slice(&state_bytes).unwrap();
    assert_eq!(state["accounting"]["sources"]["journals"], 320);
    assert_eq!(state["accounting"]["skips"]["unmatched_journals"], 320);
    FrozenSourceManifest::load_and_validate(fixture.state_root()).unwrap();
    assert_eq!(
        fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o500
    );
    assert!(
        fs::read_dir(&directory).unwrap().all(|entry| {
            entry.unwrap().metadata().unwrap().permissions().mode() & 0o777 == 0o400
        })
    );
    let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT (SELECT count(*) FROM permission_attempts),
                    (SELECT count(*) FROM permission_commits),
                    (SELECT count(*) FROM historical_permission_authority)",
            [],
            |row| Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?
            )),
        )
        .unwrap(),
        (0, 0, 0)
    );
}

#[test]
#[cfg(feature = "fault-injection")]
fn hook_open_is_typed_prompt_and_immutable_across_migration_states() {
    let missing = private_tempdir();
    let missing_before = tree_snapshot(missing.path());
    assert!(matches!(
        BrainDb::open_current(
            &StoragePaths::at(missing.path()),
            OpenRole::Hook,
            StorageDeadline::after(Duration::from_millis(100)),
        ),
        Err(StorageError::MigrationRequired)
    ));
    assert_eq!(tree_snapshot(missing.path()), missing_before);

    for (fault, expected) in [
        ("building", MigrationStatus::Building),
        (
            "after-brain-publication",
            MigrationStatus::BrainPublishedIncomplete,
        ),
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(!migration_child(fixture.state_root(), fault).success());
        let before = tree_snapshot(fixture.state_root());
        assert_eq!(
            MigrationCoordinator::at(fixture.state_root())
                .inspect()
                .unwrap(),
            expected
        );
        assert!(matches!(
            BrainDb::open_current(
                &StoragePaths::at(fixture.state_root()),
                OpenRole::Hook,
                StorageDeadline::after(Duration::from_millis(100)),
            ),
            Err(StorageError::MigrationActive)
        ));
        assert_eq!(tree_snapshot(fixture.state_root()), before, "{fault}");
        if expected == MigrationStatus::BrainPublishedIncomplete {
            assert_no_sqlite_sidecars(fixture.state_root());
        }
    }
}

#[test]
fn hook_open_accepts_completed_migration_without_mutating_coordinator() {
    let root = private_tempdir();
    assert_eq!(
        MigrationCoordinator::at(root.path())
            .run_non_hook()
            .unwrap(),
        MigrationStatus::Complete
    );
    let state_path = root.path().join("db/.brain.sqlite3.migration-state.json");
    let manifest_path = root.path().join("db/.brain.sqlite3.frozen-manifest.json");
    let state_before = fs::read(&state_path).unwrap();
    let manifest_before = fs::read(&manifest_path).unwrap();

    drop(
        BrainDb::open_current(
            &StoragePaths::at(root.path()),
            OpenRole::Hook,
            StorageDeadline::after(Duration::from_millis(100)),
        )
        .unwrap(),
    );

    assert_eq!(fs::read(state_path).unwrap(), state_before);
    assert_eq!(fs::read(manifest_path).unwrap(), manifest_before);
}

#[test]
fn completed_migration_accepts_restart_after_live_database_write() {
    let root = private_tempdir();
    let paths = StoragePaths::at(root.path());
    let coordinator = MigrationCoordinator::at(root.path());
    assert_eq!(
        coordinator.run_non_hook().unwrap(),
        MigrationStatus::Complete
    );
    let mut database = BrainDb::open_current(
        &paths,
        OpenRole::Hook,
        StorageDeadline::after(Duration::from_millis(100)),
    )
    .unwrap();
    let identity = LifecycleIdentity::try_new(
        AgentProvider::Codex,
        "live-session".into(),
        Some("live-turn".into()),
        None,
        root.path().to_path_buf(),
    )
    .unwrap();
    database
        .record_lifecycle(
            LifecycleEvent::from_parts(identity, LifecycleEventKind::UserPromptSubmit).unwrap(),
            1,
        )
        .unwrap();
    drop(database);

    assert_eq!(
        coordinator.run_non_hook().unwrap(),
        MigrationStatus::Complete
    );
}

#[test]
#[cfg(feature = "fault-injection")]
fn interrupted_migration_inspects_and_resumes_each_published_boundary() {
    for (fault, interrupted) in [
        ("building", MigrationStatus::Building),
        ("verified", MigrationStatus::Verified),
        ("after-verified-state-temp-sync", MigrationStatus::Building),
        (
            "after-brain-link",
            MigrationStatus::BrainPublishedIncomplete,
        ),
        (
            "after-brain-publication",
            MigrationStatus::BrainPublishedIncomplete,
        ),
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        assert!(
            !migration_child(fixture.state_root(), fault).success(),
            "{fault}"
        );
        let coordinator = MigrationCoordinator::at(fixture.state_root());
        assert_eq!(coordinator.inspect().unwrap(), interrupted, "{fault}");
        assert_eq!(
            coordinator.resume().unwrap(),
            MigrationStatus::Complete,
            "{fault}"
        );
        assert_eq!(
            coordinator.inspect().unwrap(),
            MigrationStatus::Complete,
            "{fault}"
        );
        let db = rusqlite::Connection::open(fixture.state_root().join("db/brain.sqlite3")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT response_eligible FROM historical_permission_authority
                 WHERE decision_id = 'decision-4vh58'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "{fault}"
        );
    }
}

#[test]
#[cfg(feature = "fault-injection")]
fn migration_rejects_ambiguous_or_unsafe_staging_without_deleting_it() {
    for unsafe_kind in [
        "ambiguous",
        "hardlink",
        "symlink",
        "mode",
        "generation",
        "sidecar",
        "state-temp-malformed",
        "extra-state-temp",
        "state-temp-mode",
        "state-temp-hardlink",
        "state-temp-accounting-missing",
    ] {
        let fixture = LegacyFixture::copy("permission-journal-4vh58");
        let fault = if unsafe_kind == "sidecar" {
            "verified"
        } else {
            "building"
        };
        assert!(!migration_child(fixture.state_root(), fault).success());
        let staging = staging_path(fixture.state_root());
        match unsafe_kind {
            "ambiguous" => write_private(
                &fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migrate-ambiguous"),
                b"ambiguous",
            ),
            "hardlink" => fs::hard_link(
                &staging,
                fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migrate-hardlink"),
            )
            .unwrap(),
            "symlink" => symlink(
                &staging,
                fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migrate-symlink"),
            )
            .unwrap(),
            "mode" => fs::set_permissions(&staging, fs::Permissions::from_mode(0o640)).unwrap(),
            "generation" => {
                let state = fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migration-state.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
                value["generation"] = serde_json::Value::from(
                    value["generation"]
                        .as_u64()
                        .unwrap()
                        .checked_add(1)
                        .unwrap(),
                );
                write_private(&state, &serde_json::to_vec(&value).unwrap());
            }
            "sidecar" => write_private(
                &std::path::PathBuf::from(format!("{}-wal", staging.display())),
                b"sidecar",
            ),
            "state-temp-malformed" => {
                let state = fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migration-state.json");
                let value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
                let generation = value["generation"].as_u64().unwrap();
                write_private(
                    &fixture.state_root().join(format!(
                        "db/.brain.sqlite3.migration-state-{generation}-building-to-verified.tmp"
                    )),
                    b"malformed",
                );
            }
            "extra-state-temp" => write_private(
                &fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migration-state-extra.tmp"),
                b"extra",
            ),
            "state-temp-mode" | "state-temp-hardlink" => {
                let state = fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migration-state.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
                let generation = value["generation"].as_u64().unwrap();
                value["status"] = serde_json::Value::from("verified");
                let temporary = fixture.state_root().join(format!(
                    "db/.brain.sqlite3.migration-state-{generation}-building-to-verified.tmp"
                ));
                write_private(&temporary, &serde_json::to_vec(&value).unwrap());
                if unsafe_kind == "state-temp-mode" {
                    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o640)).unwrap();
                } else {
                    fs::hard_link(
                        &temporary,
                        fixture.state_root().join("db/state-temp-hardlink-evidence"),
                    )
                    .unwrap();
                }
            }
            "state-temp-accounting-missing" => {
                let state = fixture
                    .state_root()
                    .join("db/.brain.sqlite3.migration-state.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
                let generation = value["generation"].as_u64().unwrap();
                value["status"] = serde_json::Value::from("verified");
                write_private(
                    &fixture.state_root().join(format!(
                        "db/.brain.sqlite3.migration-state-{generation}-building-to-verified.tmp"
                    )),
                    &serde_json::to_vec(&value).unwrap(),
                );
            }
            _ => unreachable!(),
        }
        let before = tree_snapshot(fixture.state_root());
        assert!(matches!(
            MigrationCoordinator::at(fixture.state_root()).resume(),
            Err(StorageError::InvalidStorage(_))
        ));
        assert_eq!(tree_snapshot(fixture.state_root()), before, "{unsafe_kind}");
    }
}

#[test]
fn legacy_sources_reject_symlinked_components_without_following_them() {
    let root = private_tempdir();
    let outside = private_tempdir();
    fs::write(outside.path().join("decisions.jsonl"), b"{}\n").unwrap();
    fs::set_permissions(
        outside.path().join("decisions.jsonl"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    symlink(outside.path(), root.path().join("brain")).unwrap();

    let sources = LegacySourceSet::at(root.path()).unwrap();
    let error = sources.fingerprints().unwrap_err();

    assert!(matches!(error, StorageError::InvalidStorage(_)));
}

#[test]
fn legacy_sources_reject_symlinked_state_root_ancestor() {
    let parent = private_tempdir();
    let outside = private_tempdir();
    let state_root = outside.path().join("state");
    fs::create_dir(&state_root).unwrap();
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(outside.path(), parent.path().join("alias")).unwrap();

    assert!(matches!(
        LegacySourceSet::at(&parent.path().join("alias/state")),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn legacy_v0591_fixture_streams_typed_sources_in_stable_order() {
    let root = private_tempdir();
    write_private(
        &root.path().join("brain/decisions.jsonl"),
        include_bytes!("fixtures/storage/legacy-v0.59.1/brain/decisions.jsonl"),
    );
    write_private(
        &root.path().join("activity.jsonl"),
        include_bytes!("fixtures/storage/legacy-v0.59.1/activity.jsonl"),
    );
    write_private(
        &root.path().join("hooks/lifecycle.json"),
        include_bytes!("fixtures/storage/legacy-v0.59.1/hooks/lifecycle.json"),
    );
    write_private(
        &root.path().join("review-state.json"),
        include_bytes!("fixtures/storage/legacy-v0.59.1/review-state.json"),
    );
    fs::create_dir_all(root.path().join("brain/permission-transactions")).unwrap();
    fs::set_permissions(
        root.path().join("brain/permission-transactions"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write_private(&root.path().join("session-links.jsonl"), b"not-json\n");
    let session_links_before = fs::read(root.path().join("session-links.jsonl")).unwrap();

    let snapshot = LegacySourceSet::at(root.path())
        .unwrap()
        .read_all_bounded()
        .unwrap();

    assert_eq!(snapshot.source_count(), 5);
    assert_eq!(snapshot.decision_count(), 1);
    assert_eq!(snapshot.activity_count(), 1);
    assert_eq!(snapshot.lifecycle_count(), 1);
    assert_eq!(snapshot.journal_count(), 0);
    assert_eq!(snapshot.review_state_count(), 1);
    assert_eq!(
        fs::read(root.path().join("session-links.jsonl")).unwrap(),
        session_links_before
    );
}

#[test]
fn legacy_lifecycle_projects_every_schema_supported_by_v0591() {
    for schema_version in 1..=4 {
        let root = private_tempdir();
        write_private(
            &root.path().join("hooks/lifecycle.json"),
            format!(
                "{{\"schema_version\":{schema_version},\"next_sequence\":1,\"sessions\":{{}}}}"
            )
            .as_bytes(),
        );

        let snapshot = LegacySourceSet::at(root.path())
            .unwrap()
            .read_all_bounded()
            .unwrap();

        assert_eq!(snapshot.lifecycle_count(), 1, "schema {schema_version}");
    }
}

#[test]
fn legacy_jsonl_larger_than_sixteen_mib_streams_with_one_record_buffer() {
    let root = private_tempdir();
    let path = root.path().join("brain/decisions.jsonl");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let row = b"{\"provider\":\"codex\",\"ts\":\"2026-08-01T00:00:00Z\",\"pid\":1,\"project\":\"fixture\",\"user_action\":\"accept\"}\n";
    let count = (17 * 1024 * 1024 / row.len()) + 1;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    for _ in 0..count {
        file.write_all(row).unwrap();
    }
    drop(file);

    let snapshot = LegacySourceSet::at(root.path())
        .unwrap()
        .read_all_bounded()
        .unwrap();

    assert_eq!(snapshot.decision_count(), count as u64);
}

#[test]
fn legacy_reader_rejects_unsafe_final_entries() {
    for unsafe_kind in ["mode", "hardlink", "symlink", "fifo"] {
        let root = private_tempdir();
        let path = root.path().join("activity.jsonl");
        match unsafe_kind {
            "mode" => {
                write_private(&path, b"");
                fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            }
            "hardlink" => {
                write_private(&path, b"");
                fs::hard_link(&path, root.path().join("activity-copy.jsonl")).unwrap();
            }
            "symlink" => {
                let outside = private_tempdir();
                write_private(&outside.path().join("activity.jsonl"), b"");
                symlink(outside.path().join("activity.jsonl"), &path).unwrap();
            }
            "fifo" => {
                let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }

        let sources = LegacySourceSet::at(root.path()).unwrap();
        assert!(
            matches!(
                sources.read_all_bounded(),
                Err(StorageError::InvalidStorage(_))
            ),
            "{unsafe_kind}"
        );
    }
}

#[test]
fn legacy_reader_accepts_valid_unterminated_tail_and_rejects_oversized_one() {
    let root = private_tempdir();
    let row = include_bytes!("fixtures/storage/legacy-v0.59.1/brain/decisions.jsonl");
    write_private(
        &root.path().join("brain/decisions.jsonl"),
        row.strip_suffix(b"\n").unwrap(),
    );
    assert_eq!(
        LegacySourceSet::at(root.path())
            .unwrap()
            .read_all_bounded()
            .unwrap()
            .decision_count(),
        1
    );

    write_private(
        &root.path().join("brain/decisions.jsonl"),
        &vec![b'x'; 1024 * 1024 + 1],
    );
    assert!(matches!(
        LegacySourceSet::at(root.path()).unwrap().read_all_bounded(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn legacy_reader_rejects_newer_schemas_and_critical_numeric_corruption() {
    for (relative, bytes) in [
        (
            "hooks/lifecycle.json",
            br#"{"schema_version":5,"next_sequence":1,"sessions":{}}"#.as_slice(),
        ),
        (
            "review-state.json",
            br#"{"schema_version":2,"surfaces":{}}"#.as_slice(),
        ),
        (
            "activity.jsonl",
            br#"{"schema_version":4,"kind":"decision","activity_id":"a","recorded_at_ms":1,"project":{"project_id":{"kind":"temporary","value":"p"},"cwd":"/p"},"state":"observed"}"#.as_slice(),
        ),
        (
            "brain/decisions.jsonl",
            br#"{"provider":"codex","ts":"x","pid":4294967296,"project":"p","user_action":"accept"}"#.as_slice(),
        ),
        (
            "brain/decisions.jsonl",
            br#"{"provider":"codex","provider":"claude","ts":"x","pid":1,"project":"p","user_action":"accept"}"#.as_slice(),
        ),
    ] {
        let root = private_tempdir();
        write_private(&root.path().join(relative), bytes);
        assert!(matches!(
            LegacySourceSet::at(root.path())
                .unwrap()
                .read_all_bounded(),
            Err(StorageError::InvalidStorage(_))
        ));
    }
}

#[test]
fn legacy_reader_rejects_semantically_invalid_hook_proposals() {
    let exact = include_str!("fixtures/storage/legacy-v0.59.1/brain/decisions.jsonl");
    let cases = [
        exact.replace(
            "\"decision_id\":\"fixture-decision\"",
            "\"decision_id\":\"\"",
        ),
        exact.replace("\"session_id\":\"fixture-session\"", "\"session_id\":\"\""),
        exact.replace("\"turn_id\":\"fixture-turn\"", "\"turn_id\":\"\""),
        exact.replace("\"tool\":\"Bash\"", "\"tool\":\"\""),
        exact.replace(
            "\"decision_type\":\"session\"",
            "\"decision_type\":\"project\"",
        ),
        exact.replace(
            "\"brain_action\":\"approve\"",
            "\"brain_action\":\"execute\"",
        ),
        exact.replace("\"brain_source\":\"model\"", "\"brain_source\":\"\""),
        exact.replace(
            "\"command\":\"cargo test\"",
            "\"command\":\"curl --token super-secret\"",
        ),
        exact.replace(
            "\"brain_reasoning\":\"fixture\"",
            "\"brain_reasoning\":\"AWS_SECRET_ACCESS_KEY=secret-value\"",
        ),
        exact.replace(
            "\"brain_confidence\":0.9",
            "\"brain_confidence\":1e-9999999999",
        ),
        exact.replace("\"brain_confidence\":0.9", "\"brain_confidence\":1.1"),
        exact.replace(
            "\"brain_threshold\":0.8",
            "\"brain_threshold\":1e-9999999999",
        ),
        exact.replace("\"brain_threshold\":0.8", "\"brain_threshold\":1.1"),
        exact.replace("\"resolved_at\":1", "\"resolved_at\":0"),
        exact.replace(
            "\"user_action\":\"hook_proposal\"",
            "\"user_action\":\"deterministic_deny\"",
        ),
        exact
            .replace(
                "\"user_action\":\"hook_proposal\"",
                "\"user_action\":\"deterministic_deny\"",
            )
            .replace("\"brain_action\":\"approve\"", "\"brain_action\":\"deny\""),
        exact.replace(
            "\"decision_id\":\"fixture-decision\"",
            &format!("\"decision_id\":\"{}\"", "x".repeat(513)),
        ),
    ];

    for (index, encoded) in cases.into_iter().enumerate() {
        let root = private_tempdir();
        write_private(
            &root.path().join("brain/decisions.jsonl"),
            encoded.as_bytes(),
        );
        assert!(
            matches!(
                LegacySourceSet::at(root.path()).unwrap().read_all_bounded(),
                Err(StorageError::InvalidStorage(_))
            ),
            "case {index}"
        );
    }
}

#[test]
fn legacy_hook_timestamps_must_fit_sqlite_signed_integers() {
    let exact = include_str!("fixtures/storage/legacy-v0.59.1/brain/decisions.jsonl");
    let signed_max = i64::MAX.to_string();
    let signed_overflow = (i64::MAX as u64 + 1).to_string();
    let at_signed_max = exact
        .replace(
            "\"suggested_at\":1",
            &format!("\"suggested_at\":{signed_max}"),
        )
        .replace(
            "\"resolved_at\":1",
            &format!("\"resolved_at\":{signed_max}"),
        );
    let read = |encoded: &str| {
        let root = private_tempdir();
        write_private(
            &root.path().join("brain/decisions.jsonl"),
            encoded.as_bytes(),
        );
        LegacySourceSet::at(root.path()).unwrap().read_all_bounded()
    };

    assert!(read(&at_signed_max).is_ok());

    for encoded in [
        at_signed_max.replace(
            &format!("\"suggested_at\":{signed_max}"),
            &format!("\"suggested_at\":{signed_overflow}"),
        ),
        at_signed_max
            .replace(
                &format!("\"suggested_at\":{signed_max}"),
                &format!("\"suggested_at\":{signed_overflow}"),
            )
            .replace(
                &format!("\"resolved_at\":{signed_max}"),
                &format!("\"resolved_at\":{signed_overflow}"),
            ),
        at_signed_max.replace(
            &format!("\"resolved_at\":{signed_max}"),
            &format!("\"resolved_at\":{signed_overflow}"),
        ),
    ] {
        assert!(matches!(
            read(&encoded),
            Err(StorageError::InvalidStorage(_))
        ));
    }
}

#[test]
fn authority_shaped_activity_with_diagnostic_field_is_not_skipped() {
    let root = private_tempdir();
    write_private(
        &root.path().join("activity.jsonl"),
        br#"{"schema_version":3,"kind":"decision","activity_id":"a","recorded_at_ms":1,"project":{"project_id":{"kind":"temporary","value":"p"},"cwd":"/p"},"state":"allowed","decision_id":"d","diagnostic":{"kind":"malformed_rows","count":1}}"#,
    );
    assert!(matches!(
        LegacySourceSet::at(root.path()).unwrap().read_all_bounded(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn legacy_v0591_sources_are_read_without_mutation() {
    let root = private_tempdir();
    let sources = LegacySourceSet::at(root.path()).unwrap();
    let descriptors = sources.descriptors();
    let before = sources.fingerprints().unwrap();

    let snapshot = sources.read_all_bounded().unwrap();

    assert_eq!(LEGACY_EXPORT_PROFILE, "legacy-v0.59.1");
    assert_eq!(
        descriptors
            .iter()
            .map(|descriptor| (descriptor.kind(), descriptor.relative_path()))
            .collect::<Vec<_>>(),
        [
            (LegacySourceKind::Decisions, "brain/decisions.jsonl"),
            (LegacySourceKind::Activity, "activity.jsonl"),
            (LegacySourceKind::Lifecycle, "hooks/lifecycle.json"),
            (
                LegacySourceKind::PermissionTransactions,
                "brain/permission-transactions",
            ),
            (LegacySourceKind::ReviewState, "review-state.json"),
        ]
    );
    assert_eq!(snapshot.profile(), LEGACY_EXPORT_PROFILE);
    assert_eq!(snapshot.source_count(), 0);
    assert_eq!(sources.fingerprints().unwrap(), before);
    assert!(root.path().read_dir().unwrap().next().is_none());
}

fn legacy_guard_lock(root: &std::path::Path, relative: &str) -> std::fs::File {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    fs::set_permissions(root.join(relative), fs::Permissions::from_mode(0o600)).unwrap();
    file
}

fn legacy_journal_name(index: usize) -> String {
    format!(
        "permission-transaction-{:039}-{:010}-{:020}.json",
        index + 1,
        1,
        index + 1
    )
}

fn create_legacy_journal(root: &std::path::Path, index: usize) -> std::path::PathBuf {
    let directory = root.join("brain/permission-transactions");
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(root.join("brain"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.join(legacy_journal_name(index));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    drop(file);
    path
}

#[test]
fn legacy_writer_guard_exposes_writer_compatible_order() {
    assert_eq!(
        LegacyWriterGuard::acquisition_order(),
        [
            "brain/permission-transactions/",
            "brain/decisions.lock",
            "activity.lock",
            "hooks/lifecycle.lock",
            "review-state.lock",
        ]
    );
}

fn guarded_fingerprint(
    guard: &LegacyWriterGuard,
    kind: LegacySourceKind,
) -> coding_brain::brain::storage::LegacyFingerprint {
    guard
        .fingerprints()
        .unwrap()
        .into_iter()
        .find(|fingerprint| fingerprint.kind == kind)
        .unwrap()
}

#[test]
fn legacy_freeze_streams_large_source_into_separate_read_only_inode() {
    let root = private_tempdir();
    let source = root.path().join("activity.jsonl");
    let bytes = (0..3 * 1024 * 1024 + 137)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    write_private(&source, &bytes);
    let original = fs::metadata(&source).unwrap();
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);

    let prepared: LegacyFreezeArtifact = guard
        .prepare_freeze(
            std::path::Path::new("activity.jsonl"),
            ".activity.jsonl.freeze-1.tmp",
            &expected,
        )
        .unwrap();

    let temporary = root.path().join(".activity.jsonl.freeze-1.tmp");
    let temporary_metadata = fs::metadata(&temporary).unwrap();
    assert_eq!(fs::read(&temporary).unwrap(), bytes);
    assert_eq!(temporary_metadata.permissions().mode() & 0o777, 0o400);
    assert_ne!(temporary_metadata.ino(), original.ino());
    assert_eq!(prepared.source().inode(), original.ino());
    assert_eq!(prepared.target().inode(), temporary_metadata.ino());
}

#[test]
fn legacy_freeze_prepare_resumes_owned_partial_and_complete_temps() {
    for fault in ["after-first-copy-chunk", "after-prepared-sync"] {
        let root = private_tempdir();
        let bytes = vec![b'x'; 3 * 1024 * 1024];
        write_private(&root.path().join("activity.jsonl"), &bytes);
        let status = legacy_freeze_child(root.path(), fault, false);
        assert!(!status.success());

        let temporary = root.path().join(".activity.jsonl.freeze-fault.tmp");
        assert!(temporary.exists());
        let guard =
            LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(5)))
                .unwrap();
        let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
        let artifact = guard
            .prepare_freeze(
                std::path::Path::new("activity.jsonl"),
                ".activity.jsonl.freeze-fault.tmp",
                &expected,
            )
            .unwrap();
        assert_eq!(fs::read(&temporary).unwrap(), bytes);
        assert_eq!(artifact.target().mode(), 0o400);
    }
}

#[test]
fn legacy_freeze_publish_is_idempotent_after_rename_crash() {
    let root = private_tempdir();
    write_private(&root.path().join("activity.jsonl"), b"frozen\n");
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    let artifact = guard
        .prepare_freeze(
            std::path::Path::new("activity.jsonl"),
            ".activity.jsonl.freeze-fault.tmp",
            &expected,
        )
        .unwrap();
    drop(guard);

    let status = legacy_freeze_child(root.path(), "after-rename", true);
    assert!(!status.success());
    assert!(
        !root
            .path()
            .join(".activity.jsonl.freeze-fault.tmp")
            .exists()
    );

    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    guard.publish_freeze(&artifact).unwrap();
    guard.publish_freeze(&artifact).unwrap();
    assert_eq!(
        fs::read(root.path().join("activity.jsonl")).unwrap(),
        b"frozen\n"
    );
}

#[test]
fn legacy_freeze_publish_displaces_preopened_writer_inode() {
    let root = private_tempdir();
    let source = root.path().join("activity.jsonl");
    write_private(&source, b"before\n");
    let mut old_writer = OpenOptions::new().write(true).open(&source).unwrap();
    let old_inode = old_writer.metadata().unwrap().ino();
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    let artifact = guard
        .prepare_freeze(
            std::path::Path::new("activity.jsonl"),
            ".activity.jsonl.freeze-writer.tmp",
            &expected,
        )
        .unwrap();
    guard.publish_freeze(&artifact).unwrap();
    let frozen = fs::metadata(&source).unwrap();
    assert_ne!(frozen.ino(), old_inode);
    assert_eq!(frozen.permissions().mode() & 0o777, 0o400);
    old_writer.write_all(b"old inode only").unwrap();
    assert_eq!(fs::read(&source).unwrap(), b"before\n");
    assert!(OpenOptions::new().write(true).open(&source).is_err());
}

#[test]
fn legacy_freeze_rejects_ineligible_or_absent_sources() {
    let root = private_tempdir();
    write_private(&root.path().join("session-links.jsonl"), b"link\n");
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let absent = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    let journals = guarded_fingerprint(&guard, LegacySourceKind::PermissionTransactions);
    for (path, expected) in [
        ("activity.jsonl", &absent),
        ("session-links.jsonl", &absent),
        ("activity.lock", &absent),
        ("brain/permission-transactions", &journals),
    ] {
        assert!(
            guard
                .prepare_freeze(
                    std::path::Path::new(path),
                    ".ineligible.freeze.tmp",
                    expected,
                )
                .is_err()
        );
    }
    assert_eq!(
        fs::read(root.path().join("session-links.jsonl")).unwrap(),
        b"link\n"
    );
    assert!(!root.path().join(".ineligible.freeze.tmp").exists());
}

#[test]
fn legacy_freeze_rejects_unsafe_source_and_preserves_entries() {
    enum Mutation {
        Symlink,
        Hardlink,
        Mode,
        Replace,
    }
    for mutation in [
        Mutation::Symlink,
        Mutation::Hardlink,
        Mutation::Mode,
        Mutation::Replace,
    ] {
        let root = private_tempdir();
        let source = root.path().join("activity.jsonl");
        write_private(&source, b"original\n");
        let guard =
            LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
                .unwrap();
        let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
        match mutation {
            Mutation::Symlink => {
                fs::rename(&source, root.path().join("saved")).unwrap();
                symlink(root.path().join("saved"), &source).unwrap();
            }
            Mutation::Hardlink => fs::hard_link(&source, root.path().join("alias")).unwrap(),
            Mutation::Mode => {
                fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap()
            }
            Mutation::Replace => {
                fs::rename(&source, root.path().join("saved")).unwrap();
                write_private(&source, b"replacement\n");
            }
        }
        assert!(
            guard
                .prepare_freeze(
                    std::path::Path::new("activity.jsonl"),
                    ".activity.jsonl.freeze-unsafe.tmp",
                    &expected,
                )
                .is_err()
        );
        assert!(
            !root
                .path()
                .join(".activity.jsonl.freeze-unsafe.tmp")
                .exists()
        );
    }
}

#[test]
fn legacy_freeze_preserves_canonical_and_substituted_temp_on_publish_failure() {
    let root = private_tempdir();
    let source = root.path().join("activity.jsonl");
    let temporary = root.path().join(".activity.jsonl.freeze-substitute.tmp");
    write_private(&source, b"source\n");
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    let artifact = guard
        .prepare_freeze(
            std::path::Path::new("activity.jsonl"),
            ".activity.jsonl.freeze-substitute.tmp",
            &expected,
        )
        .unwrap();
    fs::rename(&temporary, root.path().join("prepared.saved")).unwrap();
    write_private(&temporary, b"substitute\n");

    assert!(guard.publish_freeze(&artifact).is_err());
    assert_eq!(fs::read(&source).unwrap(), b"source\n");
    assert_eq!(fs::read(&temporary).unwrap(), b"substitute\n");
    assert_eq!(
        fs::read(root.path().join("prepared.saved")).unwrap(),
        b"source\n"
    );
}

#[test]
fn legacy_freeze_supports_exact_final_journal_and_displaces_preopened_writer() {
    let root = private_tempdir();
    let journal = create_legacy_journal(root.path(), 0);
    OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .write_all(b"journal\n")
        .unwrap();
    let mut old_writer = OpenOptions::new().write(true).open(&journal).unwrap();
    let old_inode = old_writer.metadata().unwrap().ino();
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let relative =
        std::path::PathBuf::from("brain/permission-transactions").join(legacy_journal_name(0));
    let expected = guard
        .fingerprints()
        .unwrap()
        .into_iter()
        .find(|fingerprint| fingerprint.relative_path() == relative)
        .unwrap();
    assert!(
        guard
            .prepare_freeze(&relative, &legacy_journal_name(1), &expected)
            .is_err()
    );
    let artifact = guard
        .prepare_freeze(&relative, ".journal.freeze.tmp", &expected)
        .unwrap();
    guard.publish_freeze(&artifact).unwrap();
    assert_ne!(fs::metadata(&journal).unwrap().ino(), old_inode);
    assert_eq!(
        fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
        0o400
    );
    old_writer.write_all(b"old").unwrap();
    assert_eq!(fs::read(&journal).unwrap(), b"journal\n");
}

#[test]
fn legacy_freeze_rejects_path_traversal_and_non_component_temp_names() {
    let root = private_tempdir();
    write_private(&root.path().join("activity.jsonl"), b"source\n");
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(2)))
            .unwrap();
    let expected = guarded_fingerprint(&guard, LegacySourceKind::Activity);
    for source in [
        "../activity.jsonl",
        "brain/../activity.jsonl",
        "/activity.jsonl",
    ] {
        assert!(
            guard
                .prepare_freeze(std::path::Path::new(source), ".safe.tmp", &expected)
                .is_err()
        );
    }
    for temporary in [
        "../escape",
        "nested/temp",
        ".",
        "..",
        "activity.jsonl",
        "activity.lock",
        "session-links.jsonl",
        "session-links.lock",
    ] {
        assert!(
            guard
                .prepare_freeze(std::path::Path::new("activity.jsonl"), temporary, &expected,)
                .is_err()
        );
    }
    assert_eq!(
        fs::read(root.path().join("activity.jsonl")).unwrap(),
        b"source\n"
    );
}

#[test]
fn legacy_writer_guard_has_one_deadline_and_releases_prefix_on_each_contention() {
    for &contended in LegacyWriterGuard::acquisition_order() {
        let root = private_tempdir();
        drop(
            LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
                .unwrap(),
        );
        let held = if contended.ends_with('/') {
            std::fs::File::open(root.path().join(contended)).unwrap()
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.path().join(contended))
                .unwrap()
        };
        held.try_lock_exclusive().unwrap();

        assert!(matches!(
            LegacyWriterGuard::acquire(
                root.path(),
                StorageDeadline::after(Duration::from_millis(40))
            ),
            Err(StorageError::Busy)
        ));

        for prefix in LegacyWriterGuard::acquisition_order()
            .iter()
            .take_while(|path| **path != contended)
        {
            let candidate = if prefix.ends_with('/') {
                std::fs::File::open(root.path().join(prefix)).unwrap()
            } else {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(root.path().join(prefix))
                    .unwrap()
            };
            candidate.try_lock_exclusive().unwrap();
            FileExt::unlock(&candidate).unwrap();
        }
        FileExt::unlock(&held).unwrap();
    }
}

#[test]
fn legacy_writer_guard_releases_all_earlier_locks_when_review_is_busy() {
    let root = private_tempdir();
    drop(
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
            .unwrap(),
    );
    let review = legacy_guard_lock(root.path(), "review-state.lock");
    review.try_lock_exclusive().unwrap();

    assert!(matches!(
        LegacyWriterGuard::acquire(
            root.path(),
            StorageDeadline::after(Duration::from_millis(40))
        ),
        Err(StorageError::Busy)
    ));
    for relative in &LegacyWriterGuard::acquisition_order()[..4] {
        let file = if relative.ends_with('/') {
            std::fs::File::open(root.path().join(relative)).unwrap()
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.path().join(relative))
                .unwrap()
        };
        file.try_lock_exclusive().unwrap();
        FileExt::unlock(&file).unwrap();
    }
    FileExt::unlock(&review).unwrap();
}

#[test]
fn legacy_writer_guard_drains_a_contended_journal_entry() {
    let root = private_tempdir();
    let journal = create_legacy_journal(root.path(), 0);
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .open(journal)
        .unwrap();
    held.try_lock_exclusive().unwrap();

    assert!(matches!(
        LegacyWriterGuard::acquire(
            root.path(),
            StorageDeadline::after(Duration::from_millis(40))
        ),
        Err(StorageError::Busy)
    ));
    FileExt::unlock(&held).unwrap();
    assert!(
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
            .is_ok()
    );
}

#[test]
fn legacy_writer_guard_reenumerates_after_locked_journal_rename_or_removal() {
    for (case, remove) in [("rename", false), ("removal", true)] {
        let root = private_tempdir();
        let journal = create_legacy_journal(root.path(), 0);
        let held = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&journal)
            .unwrap();
        held.try_lock_exclusive().unwrap();
        let state_root = root.path().to_owned();
        let acquire = std::thread::spawn(move || {
            LegacyWriterGuard::acquire(&state_root, StorageDeadline::after(Duration::from_secs(2)))
        });
        let directory =
            std::fs::File::open(root.path().join("brain/permission-transactions")).unwrap();
        for _ in 0..200 {
            if directory.try_lock_exclusive().is_err() {
                break;
            }
            FileExt::unlock(&directory).unwrap();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(directory.try_lock_exclusive().is_err(), "{case}");
        if remove {
            fs::remove_file(&journal).unwrap();
        } else {
            fs::rename(
                &journal,
                journal.parent().unwrap().join(legacy_journal_name(1)),
            )
            .unwrap();
        }
        FileExt::unlock(&held).unwrap();
        let result = acquire.join().unwrap();
        assert!(result.is_ok(), "{case}: {result:?}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_writer_guard_drains_more_than_300_journals_with_bounded_file_descriptors() {
    let root = private_tempdir();
    for index in 0..350 {
        create_legacy_journal(root.path(), index);
    }
    let descriptors_before = fs::read_dir("/proc/self/fd").unwrap().count();
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(3)))
            .unwrap();
    let descriptors_held = fs::read_dir("/proc/self/fd").unwrap().count();
    assert!(descriptors_held <= descriptors_before + 12);
    drop(guard);
}

#[test]
fn legacy_writer_guard_rejects_unsafe_or_replaced_lock_files() {
    let outside = private_tempdir();
    write_private(&outside.path().join("lock"), b"");

    let symlink_root = private_tempdir();
    fs::create_dir_all(symlink_root.path().join("brain")).unwrap();
    fs::set_permissions(
        symlink_root.path().join("brain"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    symlink(
        outside.path().join("lock"),
        symlink_root.path().join("brain/decisions.lock"),
    )
    .unwrap();
    assert!(matches!(
        LegacyWriterGuard::acquire(
            symlink_root.path(),
            StorageDeadline::after(Duration::from_secs(1))
        ),
        Err(StorageError::InvalidStorage(_))
    ));

    let hardlink_root = private_tempdir();
    let lock = legacy_guard_lock(hardlink_root.path(), "activity.lock");
    drop(lock);
    fs::hard_link(
        hardlink_root.path().join("activity.lock"),
        hardlink_root.path().join("activity-alias.lock"),
    )
    .unwrap();
    assert!(matches!(
        LegacyWriterGuard::acquire(
            hardlink_root.path(),
            StorageDeadline::after(Duration::from_secs(1))
        ),
        Err(StorageError::InvalidStorage(_))
    ));

    let mode_root = private_tempdir();
    drop(legacy_guard_lock(mode_root.path(), "hooks/lifecycle.lock"));
    fs::set_permissions(
        mode_root.path().join("hooks/lifecycle.lock"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    assert!(matches!(
        LegacyWriterGuard::acquire(
            mode_root.path(),
            StorageDeadline::after(Duration::from_secs(1))
        ),
        Err(StorageError::InvalidStorage(_))
    ));

    let replacement_root = private_tempdir();
    let guard = LegacyWriterGuard::acquire(
        replacement_root.path(),
        StorageDeadline::after(Duration::from_secs(1)),
    )
    .unwrap();
    fs::rename(
        replacement_root.path().join("review-state.lock"),
        replacement_root.path().join("review-state.lock.old"),
    )
    .unwrap();
    drop(legacy_guard_lock(
        replacement_root.path(),
        "review-state.lock",
    ));
    assert!(matches!(
        guard.validate(),
        Err(StorageError::InvalidStorage(_))
    ));
}

#[test]
fn legacy_writer_guard_keeps_fingerprints_stable_until_release() {
    let root = private_tempdir();
    write_private(&root.path().join("activity.jsonl"), b"before\n");
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
            .unwrap();
    let before = guard.fingerprints().unwrap();
    let activity_lock = root.path().join("activity.lock");
    let activity = root.path().join("activity.jsonl");
    let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(activity_lock)
            .unwrap();
        attempted_tx.send(()).unwrap();
        lock.lock_exclusive().unwrap();
        OpenOptions::new()
            .append(true)
            .open(activity)
            .unwrap()
            .write_all(b"after\n")
            .unwrap();
        FileExt::unlock(&lock).unwrap();
    });
    attempted_rx.recv().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(guard.fingerprints().unwrap(), before);
    drop(guard);
    writer.join().unwrap();
    assert_ne!(
        LegacySourceSet::at(root.path())
            .unwrap()
            .fingerprints()
            .unwrap(),
        before
    );
}

#[test]
fn legacy_writer_guard_fingerprints_include_journal_names_and_content_metadata() {
    let root = private_tempdir();
    let journal = create_legacy_journal(root.path(), 0);
    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
            .unwrap();

    let before = guard.fingerprints().unwrap();
    let before_entry = before
        .iter()
        .find(|fingerprint| {
            fingerprint
                .relative_path()
                .ends_with(legacy_journal_name(0))
        })
        .unwrap();
    assert_eq!(before_entry.size, 0);

    OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let after_content = guard.fingerprints().unwrap();
    assert_eq!(
        after_content
            .iter()
            .find(|fingerprint| fingerprint.relative_path() == before_entry.relative_path())
            .unwrap()
            .size,
        7
    );

    let renamed = journal.parent().unwrap().join(legacy_journal_name(1));
    fs::rename(&journal, &renamed).unwrap();
    let after_rename = guard.fingerprints().unwrap();
    assert!(after_rename.iter().any(|fingerprint| {
        fingerprint
            .relative_path()
            .ends_with(legacy_journal_name(1))
    }));
    assert!(after_rename.iter().all(|fingerprint| {
        !fingerprint
            .relative_path()
            .ends_with(legacy_journal_name(0))
    }));
}

#[test]
fn legacy_writer_guard_creates_only_guards_and_never_touches_session_links() {
    let root = private_tempdir();
    write_private(&root.path().join("session-links.jsonl"), b"session\n");
    let session_lock = legacy_guard_lock(root.path(), "session-links.lock");
    session_lock.try_lock_exclusive().unwrap();
    let session_before = fs::metadata(root.path().join("session-links.jsonl")).unwrap();
    let lock_before = fs::metadata(root.path().join("session-links.lock")).unwrap();

    let guard =
        LegacyWriterGuard::acquire(root.path(), StorageDeadline::after(Duration::from_secs(1)))
            .unwrap();
    let fingerprints = guard.fingerprints().unwrap();
    assert_eq!(fingerprints.len(), 5);
    // Guard creation makes the empty journal-directory fingerprint present, so
    // coordinator integration must acquire it before initial fingerprint capture.
    assert!(
        fingerprints
            .iter()
            .find(|fingerprint| fingerprint.kind == LegacySourceKind::PermissionTransactions)
            .unwrap()
            .present
    );
    drop(guard);

    for data in [
        "activity.jsonl",
        "brain/decisions.jsonl",
        "hooks/lifecycle.json",
        "review-state.json",
    ] {
        assert!(!root.path().join(data).exists());
    }
    assert!(
        fs::read_dir(root.path().join("brain/permission-transactions"))
            .unwrap()
            .next()
            .is_none()
    );
    let session_after = fs::metadata(root.path().join("session-links.jsonl")).unwrap();
    let lock_after = fs::metadata(root.path().join("session-links.lock")).unwrap();
    assert_eq!(
        fs::read(root.path().join("session-links.jsonl")).unwrap(),
        b"session\n"
    );
    assert_eq!(
        (
            session_after.dev(),
            session_after.ino(),
            session_after.mode()
        ),
        (
            session_before.dev(),
            session_before.ino(),
            session_before.mode()
        )
    );
    assert_eq!(
        (lock_after.dev(), lock_after.ino(), lock_after.mode()),
        (lock_before.dev(), lock_before.ino(), lock_before.mode())
    );
    FileExt::unlock(&session_lock).unwrap();
}
