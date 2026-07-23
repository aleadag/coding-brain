use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SessionTarget,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;

#[test]
fn headless_emits_activity_without_a_session_roster() {
    let home = tempfile::tempdir().unwrap();
    let state = home.path().join("state");
    let activity_dir = state.join("coding-brain");
    std::fs::create_dir_all(&activity_dir).unwrap();
    let event = ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: "activity-process-fixture".into(),
        recorded_at_ms: 1,
        project: ProjectEvidence {
            project_id: ProjectId::Stable("project-1".into()),
            cwd: "/work/project".into(),
            label: Some("project".into()),
        },
        session: Some(SessionTarget {
            provider: AgentProvider::Antigravity,
            session_id: "conversation-1".into(),
            turn_id: Some("turn-1".into()),
            tool_use_id: None,
            project_id: ProjectId::Stable("project-1".into()),
            cwd: "/work/project".into(),
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        }),
        state: ActivityState::Denied,
        tool: Some("Bash".into()),
        normalized_command: Some("cargo test".into()),
        fingerprint: Some("fixture".into()),
        rule_id: None,
        confidence: Some(0.9),
        threshold: Some(0.8),
        reasoning: Some("fixture".into()),
        decision_id: Some("decision-1".into()),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    };
    std::fs::write(
        activity_dir.join("activity.jsonl"),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_coding-brain"))
        .args(["--headless", "--json"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_STATE_HOME", &state)
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = send.send(result);
    });
    let line = match receive.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result.unwrap(),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("headless produced no activity within five seconds: {error}");
        }
    };
    child.kill().unwrap();
    child.wait().unwrap();

    let output: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(output["type"], "activity");
    assert_eq!(output["activity_id"], "activity-process-fixture");
    assert_eq!(output["state"], "denied");
    assert!(output.get("sessions").is_none());
    assert!(output.get("session").is_none());
    assert!(output.get("terminal").is_none());
    assert!(output.get("normalized_command").is_none());
}
