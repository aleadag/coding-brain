use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use coding_brain_core::lifecycle::{ApplyOutcome, LifecycleEvent, LifecycleStore, StoreError};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use fs2::FileExt;
use serde_json::json;

fn prompt_for(index: usize) -> LifecycleEvent {
    LifecycleEvent::parse(
        json!({
            "session_id": format!("session-{index}"),
            "turn_id": "turn-1",
            "cwd": "/work/codexctl",
            "hook_event_name": "UserPromptSubmit"
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

#[test]
fn concurrent_processes_preserve_all_accepted_updates() {
    let temp = tempfile::tempdir().unwrap();
    let children = (0..4)
        .map(|index| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "child_records_one_event",
                    "--nocapture",
                ])
                .env("CODEXCTL_LIFECYCLE_CHILD_ROOT", temp.path())
                .env("CODEXCTL_LIFECYCLE_CHILD_INDEX", index.to_string())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect::<Vec<_>>();

    let mut accepted = Vec::new();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report = String::from_utf8(output.stdout).unwrap();
        let result = report
            .lines()
            .find(|line| line.starts_with("accepted:") || line.starts_with("rejected:"))
            .expect("child result line");
        if let Some(index) = result.strip_prefix("accepted:") {
            accepted.push(index.parse::<usize>().unwrap());
        } else {
            assert!(result.starts_with("rejected:lock-timeout:"), "{result}");
        }
    }

    assert!(!accepted.is_empty());
    let snapshot = LifecycleStore::at(temp.path())
        .read()
        .unwrap()
        .snapshot
        .unwrap();
    for index in &accepted {
        assert!(
            snapshot.sessions.contains_key(
                &AgentSessionKey::native(AgentProvider::Codex, format!("session-{index}"))
                    .storage_key()
            )
        );
    }
    assert_eq!(snapshot.sessions.len(), accepted.len());
    let mut sequences = snapshot
        .sessions
        .values()
        .map(|state| state.latest_sequence)
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    sequences.dedup();
    assert_eq!(sequences.len(), accepted.len());
}

#[test]
#[ignore]
fn child_records_one_event() {
    let Some(root) = std::env::var_os("CODEXCTL_LIFECYCLE_CHILD_ROOT") else {
        return;
    };
    let index = std::env::var("CODEXCTL_LIFECYCLE_CHILD_INDEX")
        .unwrap()
        .parse::<usize>()
        .unwrap();
    match LifecycleStore::at(root).record(prompt_for(index)) {
        Ok(ApplyOutcome::Applied) => println!("accepted:{index}"),
        Err(StoreError::LockTimeout) => println!("rejected:lock-timeout:{index}"),
        result => panic!("unexpected child result: {result:?}"),
    }
}

#[test]
fn separate_process_lock_timeout_preserves_the_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let store = LifecycleStore::at(temp.path());
    store.record(prompt_for(0)).unwrap();
    let before = fs::read(store.snapshot_path()).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "child_holds_lifecycle_lock",
            "--nocapture",
        ])
        .env("CODEXCTL_LIFECYCLE_CHILD_ROOT", temp.path())
        .spawn()
        .unwrap();
    let ready = temp.path().join("lock-ready");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready.exists(), "lock holder did not signal readiness");

    assert_eq!(store.record(prompt_for(1)), Err(StoreError::LockTimeout));
    assert!(child.wait().unwrap().success());
    assert_eq!(fs::read(store.snapshot_path()).unwrap(), before);
}

#[test]
#[ignore]
fn child_holds_lifecycle_lock() {
    let Some(root) = std::env::var_os("CODEXCTL_LIFECYCLE_CHILD_ROOT") else {
        return;
    };
    let store = LifecycleStore::at(&root);
    fs::create_dir_all(store.hooks_dir()).unwrap();
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(store.lock_path())
        .unwrap();
    lock.lock_exclusive().unwrap();
    let mut ready = fs::File::create(std::path::PathBuf::from(root).join("lock-ready")).unwrap();
    ready.write_all(b"ready").unwrap();
    ready.flush().unwrap();
    thread::sleep(Duration::from_millis(250));
    FileExt::unlock(&lock).unwrap();
}
