use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use coding_brain::brain::storage::{
    LegacySourceSet, MigrationCoordinator, MigrationStatus, StoragePaths,
};

struct SqliteFixture {
    temp: tempfile::TempDir,
    state_root: PathBuf,
}

impl SqliteFixture {
    fn proposal_terminal() -> Self {
        let fixture = Self::copy("permission-journal-4vh58");
        fs::remove_file(
            fixture.state_root.join(
                "brain/permission-transactions/permission-transaction-000000000000000000000000000000000000001-0000000001-00000000000000000001.json",
            ),
        )
        .unwrap();
        fixture.migrate();
        fixture
    }

    fn journal_correlated() -> Self {
        let fixture = Self::copy("permission-journal-4vh58");
        fixture.migrate();
        fixture
    }

    fn copy(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let state_root = state.join("coding-brain");
        copy_tree(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/storage")
                .join(name),
            &state_root,
        );
        Self { temp, state_root }
    }

    fn migrate(&self) {
        assert_eq!(
            MigrationCoordinator::at(&self.state_root)
                .run_non_hook()
                .unwrap(),
            MigrationStatus::Complete
        );
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        Command::new(env!("CARGO_BIN_EXE_cbrain"))
            .args(args)
            .current_dir(self.temp.path())
            .env("HOME", self.temp.path().join("home"))
            .env("XDG_CONFIG_HOME", self.temp.path().join("config"))
            .env("XDG_STATE_HOME", self.temp.path().join("state"))
            .env("CODING_BRAIN_SKIP_FIRST_RUN", "1")
            .output()
            .unwrap()
    }
}

fn copy_tree(source: &Path, destination: &Path) {
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn mode(path: &Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn storage_legacy_export_round_trips_delivery_unknown() {
    let fixture = SqliteFixture::proposal_terminal();
    let output = fixture.temp.path().join("legacy-export");
    let result = fixture.run(["storage", "export-legacy", output.to_str().unwrap()]);
    assert_success(&result);
    LegacySourceSet::at(&output)
        .unwrap()
        .read_all_bounded()
        .unwrap();
    assert_eq!(
        MigrationCoordinator::at(&output).run_non_hook().unwrap(),
        MigrationStatus::Complete
    );
    let database = rusqlite::Connection::open(output.join("db/brain.sqlite3")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT delivery_state FROM historical_permission_authority
                 WHERE decision_id = 'decision-4vh58'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "unknown"
    );
}

#[test]
fn storage_audit_export_is_flat_stable_bounded_and_non_executable() {
    let fixture = SqliteFixture::proposal_terminal();
    let first = fixture.temp.path().join("audit-first");
    let second = fixture.temp.path().join("audit-second");

    assert_success(&fixture.run(["storage", "export-audit", first.to_str().unwrap()]));
    assert_success(&fixture.run(["storage", "export-audit", second.to_str().unwrap()]));

    assert_eq!(mode(&first), 0o700);
    let names = fs::read_dir(&first)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names,
        ["activity.jsonl", "decisions.jsonl", "manifest.json"]
            .into_iter()
            .map(Into::into)
            .collect()
    );
    assert_eq!(
        fs::read_to_string(first.join("manifest.json")).unwrap(),
        "{\"format\":\"coding-brain-audit-v1\",\"executable\":false}\n"
    );
    for name in ["activity.jsonl", "decisions.jsonl", "manifest.json"] {
        assert_eq!(mode(&first.join(name)), 0o600);
        assert_eq!(
            fs::read(first.join(name)).unwrap(),
            fs::read(second.join(name)).unwrap()
        );
    }
    for line in fs::read_to_string(first.join("activity.jsonl"))
        .unwrap()
        .lines()
    {
        assert!(line.len() <= 64 * 1024);
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    for line in fs::read_to_string(first.join("decisions.jsonl"))
        .unwrap()
        .lines()
    {
        assert!(line.len() <= 1024 * 1024);
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    assert!(!first.join("hooks").exists());
    assert!(!first.join("review-state.json").exists());
    assert!(!first.join("brain").exists());
}

#[test]
fn storage_exporters_refuse_preexisting_targets_without_touching_entries() {
    let fixture = SqliteFixture::proposal_terminal();
    for action in ["export-audit", "export-legacy"] {
        let output = fixture.temp.path().join(action);
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep"), b"untouched").unwrap();

        assert_failure(&fixture.run(["storage", action, output.to_str().unwrap()]));
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&output).unwrap().count(), 1);
    }
}

#[test]
fn storage_exporters_fail_closed_while_privacy_erasure_is_incomplete() {
    let fixture = SqliteFixture::proposal_terminal();
    let database = rusqlite::Connection::open(fixture.state_root.join("db/brain.sqlite3")).unwrap();
    database
        .execute(
            "UPDATE schema_meta
             SET erasure_state = 'in_progress', erasure_generation = erasure_generation + 1",
            [],
        )
        .unwrap();
    drop(database);

    for action in ["export-audit", "export-legacy"] {
        let output = fixture.temp.path().join(format!("incomplete-{action}"));
        assert_failure(&fixture.run(["storage", action, output.to_str().unwrap()]));
        assert!(!output.exists());
    }
}

#[test]
fn storage_legacy_export_rejects_lossy_correlated_and_live_unknown_authority() {
    let correlated = SqliteFixture::journal_correlated();
    let correlated_output = correlated.temp.path().join("correlated-export");
    assert_failure(&correlated.run([
        "storage",
        "export-legacy",
        correlated_output.to_str().unwrap(),
    ]));
    assert!(!correlated_output.exists());

    let live = SqliteFixture::proposal_terminal();
    inject_live_unknown_commit(&live.state_root);
    let live_output = live.temp.path().join("live-export");
    assert_failure(&live.run(["storage", "export-legacy", live_output.to_str().unwrap()]));
    assert!(!live_output.exists());
}

fn inject_live_unknown_commit(state_root: &Path) {
    let database = rusqlite::Connection::open(state_root.join("db/brain.sqlite3")).unwrap();
    database
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    database
        .execute(
            "INSERT INTO permission_attempts (
                attempt_id, request_identity_key, provider, session_id, turn_id, tool_use_id,
                request_key, cwd, project_id, tool_name, activity_id, authority_action,
                attempt_state, created_at_ms, updated_at_ms
             ) VALUES (
                'attempt-live', ?1, 'codex', 'session-live', 'turn-live', NULL,
                ?1, x'2f', x'7b7d', 'Bash', 'activity-live', 'allow', 'decided', 1, 1
             )",
            ["a".repeat(64)],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO permission_commits (
                attempt_id, transaction_id, decision_id, terminal_activity_id,
                authority_action, evidence_kind, delivery_state, response_eligible, committed_at_ms
             ) VALUES (
                'attempt-live', 'transaction-live', 'decision-live', 'activity-live',
                'allow', 'provider_authority', 'unknown', 1, 1
             )",
            [],
        )
        .unwrap();
}

#[test]
fn storage_reset_review_state_replaces_only_review_sqlite() {
    let fixture = SqliteFixture::proposal_terminal();
    let paths = StoragePaths::at(&fixture.state_root);
    let review = rusqlite::Connection::open(paths.review_db()).unwrap();
    review
        .execute(
            "UPDATE review_meta SET revision = 7, source_high_water = 7
             WHERE surface = 'attention'",
            [],
        )
        .unwrap();
    drop(review);
    let brain_before = fs::read(paths.brain_db()).unwrap();
    let legacy_before = fs::read(fixture.state_root.join("activity.jsonl")).unwrap();

    assert_success(&fixture.run(["storage", "reset-review-state"]));

    assert_eq!(fs::read(paths.brain_db()).unwrap(), brain_before);
    assert_eq!(
        fs::read(fixture.state_root.join("activity.jsonl")).unwrap(),
        legacy_before
    );
    let review = rusqlite::Connection::open(paths.review_db()).unwrap();
    assert_eq!(
        review
            .query_row(
                "SELECT revision FROM review_meta WHERE surface = 'attention'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    assert!(!fixture.state_root.join(".star-prompted").exists());
}
