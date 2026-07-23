use std::time::Duration;

use coding_brain_core::provider::AgentProvider;

// Test session status display and sorting
#[test]
fn test_session_status_sort_order() {
    use coding_brain::session::SessionStatus;
    assert!(SessionStatus::NeedsInput.sort_key() < SessionStatus::Processing.sort_key());
    assert!(SessionStatus::Processing.sort_key() < SessionStatus::WaitingInput.sort_key());
    assert!(SessionStatus::WaitingInput.sort_key() < SessionStatus::Unknown.sort_key());
    assert!(SessionStatus::Unknown.sort_key() < SessionStatus::Idle.sort_key());
    assert!(SessionStatus::Idle.sort_key() < SessionStatus::Finished.sort_key());
}

#[test]
fn test_session_status_display() {
    use coding_brain::session::SessionStatus;
    assert_eq!(SessionStatus::NeedsInput.to_string(), "Needs Input");
    assert_eq!(SessionStatus::Processing.to_string(), "Processing");
    assert_eq!(SessionStatus::WaitingInput.to_string(), "Waiting");
    assert_eq!(SessionStatus::Unknown.to_string(), "Unknown");
    assert_eq!(SessionStatus::Idle.to_string(), "Idle");
    assert_eq!(SessionStatus::Finished.to_string(), "Finished");
}

#[test]
fn test_session_from_raw() {
    use coding_brain::session::{AgentSession, RawAgentSession};
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 12345,
        process_start_identity: None,
        session_id: "abc-123".to_string(),
        cwd: "/Users/test/projects/my-app".to_string(),
        started_at: 0,
    };
    let session = AgentSession::from_raw(raw);
    assert_eq!(session.pid, 12345);
    assert_eq!(session.project_name, "my-app");
    assert_eq!(session.display_name(), "my-app");
}

#[test]
fn test_session_display_name_prefers_session_name() {
    use coding_brain::session::{AgentSession, RawAgentSession};
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "x".to_string(),
        cwd: "/tmp/foo".to_string(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    session.session_name = "my-custom-name".to_string();
    assert_eq!(session.display_name(), "my-custom-name");
}

#[test]
fn test_format_elapsed() {
    use coding_brain::session::{AgentSession, RawAgentSession};
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "x".to_string(),
        cwd: "/tmp".to_string(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);
    session.elapsed = Duration::from_secs(3661);
    assert_eq!(session.format_elapsed(), "01:01:01");

    session.elapsed = Duration::from_secs(125);
    assert_eq!(session.format_elapsed(), "02:05");
}

#[test]
fn test_format_tokens() {
    use coding_brain::session::{AgentSession, RawAgentSession, TelemetryStatus};
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "x".to_string(),
        cwd: "/tmp".to_string(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);

    assert_eq!(session.format_tokens(), "n/a");

    session.telemetry_status = TelemetryStatus::Available;
    session.usage_metrics_available = true;
    session.total_input_tokens = 1_500_000;
    session.total_output_tokens = 42_000;
    assert_eq!(session.format_tokens(), "1.5M/42.0k");
}

#[test]
fn test_format_cost() {
    use coding_brain::session::{AgentSession, RawAgentSession, TelemetryStatus};
    let raw = RawAgentSession {
        provider: AgentProvider::Codex,
        pid: 1,
        process_start_identity: None,
        session_id: "x".to_string(),
        cwd: "/tmp".to_string(),
        started_at: 0,
    };
    let mut session = AgentSession::from_raw(raw);

    assert_eq!(session.format_cost(), "n/a");

    session.telemetry_status = TelemetryStatus::Available;
    session.usage_metrics_available = true;
    session.cost_usd = 0.42;
    assert_eq!(session.format_cost(), "$0.42");

    session.cost_usd = 12.5;
    assert_eq!(session.format_cost(), "$12.5");
}

#[test]
fn test_cwd_to_project_name() {
    use coding_brain::session::{AgentSession, RawAgentSession};
    let cases = vec![
        ("/Users/foo/bar/my-project", "my-project"),
        ("/tmp", "tmp"),
        ("/a/b/c/deeply-nested", "deeply-nested"),
    ];
    for (cwd, expected) in cases {
        let raw = RawAgentSession {
            provider: AgentProvider::Codex,
            pid: 1,
            process_start_identity: None,
            session_id: "x".to_string(),
            cwd: cwd.to_string(),
            started_at: 0,
        };
        let session = AgentSession::from_raw(raw);
        assert_eq!(session.project_name, expected, "cwd={cwd}");
    }
}
