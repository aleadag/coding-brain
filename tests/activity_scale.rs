use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use codexctl::brain::activity::{ActivityLimits, ActivityStore};
use codexctl_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityState, ProjectEvidence,
};
use codexctl_core::project::ProjectId;

fn event(activity_id: impl Into<String>, recorded_at_ms: u64) -> ActivityEvent {
    let activity_id = activity_id.into();
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        activity_id: activity_id.clone(),
        recorded_at_ms,
        project: ProjectEvidence {
            project_id: ProjectId::Temporary("scale-project".into()),
            cwd: PathBuf::from("/work/scale-project"),
            label: Some("scale-project".into()),
        },
        session: None,
        state: ActivityState::Denied,
        tool: Some("Bash".into()),
        normalized_command: Some(format!("command-{activity_id}")),
        fingerprint: Some(format!("fingerprint-{activity_id}")),
        rule_id: Some("scale".into()),
        confidence: Some(0.9),
        threshold: Some(0.8),
        reasoning: Some("bounded scale fixture".into()),
        decision_id: Some(format!("decision-{activity_id}")),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

fn scale_store(path: &Path, retained_lifecycles: usize) -> ActivityStore {
    ActivityStore::at(path).with_limits(ActivityLimits {
        compact_at_bytes: 1,
        retained_lifecycles,
        ..ActivityLimits::default()
    })
}

#[test]
fn concurrent_append_and_compaction_preserve_successes() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(scale_store(&root.path().join("activity.jsonl"), 1_000));
    let successful = Arc::new(Mutex::new(HashSet::new()));
    let writers_done = Arc::new(AtomicBool::new(false));

    let compactor = {
        let store = Arc::clone(&store);
        let writers_done = Arc::clone(&writers_done);
        thread::spawn(move || {
            while !writers_done.load(Ordering::Acquire) {
                let _ = store.compact_if_needed();
                thread::yield_now();
            }
            store.compact_if_needed().unwrap();
        })
    };

    let writers = (0..4)
        .map(|writer| {
            let store = Arc::clone(&store);
            let successful = Arc::clone(&successful);
            thread::spawn(move || {
                for index in 0..100 {
                    let id = format!("writer-{writer}-{index}");
                    if store
                        .append(event(&id, (writer * 100 + index) as u64))
                        .is_ok()
                    {
                        successful.lock().unwrap().insert(id);
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }
    writers_done.store(true, Ordering::Release);
    compactor.join().unwrap();

    let retained = store
        .read()
        .unwrap()
        .events()
        .iter()
        .map(|event| event.activity_id.clone())
        .collect::<HashSet<_>>();
    let successful = successful.lock().unwrap();
    assert!(!successful.is_empty());
    assert!(successful.iter().all(|id| retained.contains(id)));
}

#[test]
fn hundred_thousand_events_preserve_retention() {
    let root = tempfile::tempdir().unwrap();
    let store = scale_store(&root.path().join("activity.jsonl"), 10_000);
    for index in 0..100_000_u64 {
        store
            .append(event(format!("activity-{index}"), index))
            .unwrap();
    }
    assert!(store.compact_if_needed().unwrap());
    let log = store.read().unwrap();
    assert_eq!(log.complete_lifecycles(), 10_000);
    let ids = log
        .events()
        .iter()
        .map(|event| event.activity_id.as_str())
        .collect::<HashSet<_>>();
    assert!(ids.contains("activity-99999"));
    assert!(!ids.contains("activity-0"));
}

#[test]
fn killed_split_append_is_repaired_by_the_next_writer() {
    const HELPER_ENV: &str = "CODING_BRAIN_SPLIT_APPEND_HELPER";
    if std::env::var_os(HELPER_ENV).is_some() {
        split_append_helper();
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("activity.jsonl");
    let ready = root.path().join("ready");
    let store = ActivityStore::at(&path);
    store.append(event("before-crash", 1)).unwrap();

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "killed_split_append_is_repaired_by_the_next_writer",
        ])
        .env(HELPER_ENV, "1")
        .env("CODING_BRAIN_ACTIVITY_PATH", &path)
        .env("CODING_BRAIN_READY_PATH", &ready)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready.exists(), "split-append helper did not become ready");
    child.kill().unwrap();
    child.wait().unwrap();

    store.append(event("after-crash", 2)).unwrap();
    let log = store.read().unwrap();
    let ids = log
        .events()
        .iter()
        .map(|event| event.activity_id.as_str())
        .collect::<HashSet<_>>();
    assert!(ids.contains("before-crash"));
    assert!(ids.contains("after-crash"));
    assert_eq!(log.diagnostics().truncated_tails, 1);
}

fn split_append_helper() {
    use std::io::Write;

    let path = PathBuf::from(std::env::var_os("CODING_BRAIN_ACTIVITY_PATH").unwrap());
    let ready = PathBuf::from(std::env::var_os("CODING_BRAIN_READY_PATH").unwrap());
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(b"{\"activity_id\":\"partial-secret-value")
        .unwrap();
    file.flush().unwrap();
    fs::write(ready, b"ready").unwrap();
    loop {
        thread::park();
    }
}

#[test]
#[ignore = "release-only wall-clock budget"]
fn release_activity_budgets() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("activity.jsonl");
    let store = scale_store(&path, 10_000);
    let mut append_times = Vec::with_capacity(100_000);
    for index in 0..100_000_u64 {
        let started = Instant::now();
        store
            .append(event(format!("activity-{index}"), index))
            .unwrap();
        append_times.push(started.elapsed());
    }
    append_times.sort_unstable();
    let p95 = append_times[append_times.len() * 95 / 100];
    let compact_started = Instant::now();
    assert!(store.compact_if_needed().unwrap());
    let compact_elapsed = compact_started.elapsed();
    eprintln!(
        "activity fixture={} append_p95={p95:?} compaction={compact_elapsed:?}",
        path.display()
    );
    assert!(p95 < Duration::from_millis(20));
    assert!(compact_elapsed < Duration::from_secs(5));
    assert_eq!(store.read().unwrap().complete_lifecycles(), 10_000);
}
