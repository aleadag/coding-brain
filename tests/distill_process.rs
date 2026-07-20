#![cfg(unix)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(debug_assertions)]
use std::time::Duration;
use std::time::Instant;

fn write_decisions(home: &Path, count: usize) {
    let root = home.join("state/coding-brain/brain");
    fs::create_dir_all(&root).unwrap();
    let mut file = fs::File::create(root.join("decisions.jsonl")).unwrap();
    for index in 1..=count {
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "ts": index.to_string(),
                "pid": 1,
                "project": if index % 2 == 0 { "alpha" } else { "beta" },
                "tool": "Bash",
                "command": "cargo test",
                "brain_action": "approve",
                "brain_confidence": 0.9,
                "brain_reasoning": "fixture",
                "user_action": "accept",
                "decision_type": "session",
                "decision_id": format!("dec_{index}"),
            })
        )
        .unwrap();
    }
    file.sync_all().unwrap();
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command
        .arg("--distill-once")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn watermark(home: &Path) -> serde_json::Value {
    serde_json::from_slice(
        &fs::read(home.join("state/coding-brain/brain/distill-watermark.json")).unwrap(),
    )
    .unwrap()
}

fn assert_complete_generation(home: &Path, watermark: &serde_json::Value) {
    let generation = watermark["generation_id"].as_str().unwrap();
    let root = home
        .join("state/coding-brain/brain/preferences-generations")
        .join(generation);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("generation.json")).unwrap()).unwrap();
    assert_eq!(manifest["generation_id"], generation);
    let global: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("global.json")).unwrap()).unwrap();
    assert!(global["total_decisions"].as_u64().unwrap() >= 10);
    for project in manifest["projects"].as_array().unwrap() {
        let project = project.as_str().unwrap();
        serde_json::from_slice::<serde_json::Value>(
            &fs::read(root.join("projects").join(format!("{project}.json"))).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn concurrent_workers_publish_one_complete_generation() {
    let home = tempfile::tempdir().unwrap();
    write_decisions(home.path(), 25);

    let mut first = command(home.path()).spawn().unwrap();
    let mut second = command(home.path()).spawn().unwrap();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());

    let watermark = watermark(home.path());
    assert_eq!(watermark["through_decision_id"], "dec_25");
    assert_complete_generation(home.path(), &watermark);
    let generations = fs::read_dir(
        home.path()
            .join("state/coding-brain/brain/preferences-generations"),
    )
    .unwrap()
    .count();
    assert_eq!(generations, 1);
}

#[test]
fn maintenance_retains_only_current_and_previous_generations() {
    let home = tempfile::tempdir().unwrap();
    for count in [10, 20, 30] {
        write_decisions(home.path(), count);
        let output = command(home.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let watermark = watermark(home.path());
    assert_eq!(watermark["through_decision_id"], "dec_30");
    assert_complete_generation(home.path(), &watermark);
    let generations = fs::read_dir(
        home.path()
            .join("state/coding-brain/brain/preferences-generations"),
    )
    .unwrap()
    .count();
    assert_eq!(generations, 2);
}

#[cfg(debug_assertions)]
#[test]
fn killed_worker_keeps_old_generation_and_later_process_retries() {
    for stage in ["file-1", "file-2", "file-3", "file-4", "before-pointer"] {
        let home = tempfile::tempdir().unwrap();
        write_decisions(home.path(), 10);
        assert!(command(home.path()).status().unwrap().success());
        let previous = watermark(home.path());
        write_decisions(home.path(), 20);

        let marker = home.path().join("distill-paused");
        let mut worker = command(home.path())
            .env("CODING_BRAIN_DISTILL_TEST_PAUSE", stage)
            .env("CODING_BRAIN_DISTILL_TEST_MARKER", &marker)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                worker.try_wait().unwrap().is_none(),
                "worker exited before {stage}"
            );
            assert!(Instant::now() < deadline, "worker did not reach {stage}");
            std::thread::yield_now();
        }
        worker.kill().unwrap();
        assert!(!worker.wait().unwrap().success());

        assert_eq!(watermark(home.path()), previous, "{stage}");
        assert_complete_generation(home.path(), &previous);

        let output = command(home.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let current = watermark(home.path());
        assert_eq!(current["through_decision_id"], "dec_20", "{stage}");
        assert_complete_generation(home.path(), &current);
        assert_eq!(
            fs::read_dir(
                home.path()
                    .join("state/coding-brain/brain/preferences-generations")
            )
            .unwrap()
            .count(),
            2,
            "{stage}"
        );
    }
}

#[test]
fn hundred_thousand_decisions_publish_one_generation() {
    let home = tempfile::tempdir().unwrap();
    write_decisions(home.path(), 100_000);

    let output = command(home.path()).output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let watermark = watermark(home.path());
    assert_eq!(watermark["through_decision_id"], "dec_100000");
    assert_complete_generation(home.path(), &watermark);
}

#[test]
#[ignore = "release-only wall-clock budget"]
fn release_distill_budget() {
    if cfg!(debug_assertions) {
        panic!("run this budget with --release");
    }
    let home = tempfile::tempdir().unwrap();
    write_decisions(home.path(), 100_000);
    let started = Instant::now();

    let output = command(home.path()).output().unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed().as_secs_f64() < 5.0,
        "{:?}",
        started.elapsed()
    );
}
