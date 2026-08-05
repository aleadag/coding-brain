use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;

use coding_brain::brain::storage::{
    LEGACY_EXPORT_PROFILE, LegacySourceKind, LegacySourceSet, StorageError,
};

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
