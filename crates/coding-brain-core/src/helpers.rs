use crate::session::AgentSession;

/// Fire a webhook POST with session status change payload.
/// Runs in a background thread to avoid blocking the TUI loop.
pub fn fire_webhook(url: &str, session: &AgentSession, old_status: String) {
    let payload = serde_json::json!({
        "event": "status_change",
        "session": {
            "pid": session.pid,
            "project": session.display_name(),
            "old_status": old_status,
            "new_status": session.status.to_string(),
            "context_pct": session.context_pressure,
            "elapsed_secs": session.elapsed.as_secs(),
        },
        "timestamp": chrono_now_iso(),
    });

    let body = serde_json::to_string(&payload).unwrap_or_default();
    let url = url.to_string();

    // Non-blocking: spawn a thread to POST
    std::thread::spawn(move || {
        let _ = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body,
                "--max-time",
                "5",
                &url,
            ])
            .output();
    });
}

/// Simple ISO-8601 timestamp without pulling in the chrono crate.
pub fn chrono_now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO-8601 without pulling in chrono crate
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Approximate date calculation (doesn't handle leap years perfectly but good enough for timestamps)
    let mut y = 1970;
    let mut remaining_days = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        m += 1;
    }
    let d = remaining_days + 1;
    m += 1;

    format!("{y:04}-{m:02}-{d:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Fire a desktop notification (macOS via osascript, Linux via notify-send).
pub fn fire_notification(project: &str) {
    let safe = project.replace('"', "'").replace('\\', "");
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("osascript")
        .args([
            "-e",
            &format!("display notification \"{safe} needs input\" with title \"codexctl\""),
        ])
        .spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("notify-send")
        .args(["codexctl", &format!("{safe} needs input")])
        .spawn();
}

/// Resolve the user's home directory, falling back to /tmp.
pub fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
}

/// Kill a process by PID. Tries SIGTERM first, then SIGKILL on failure.
pub fn kill_process(pid: u32) -> Result<(), String> {
    let output = std::process::Command::new("kill")
        .arg(pid.to_string())
        .output()
        .map_err(|e| format!("Failed to run kill: {e}"))?;

    if output.status.success() {
        return Ok(());
    }

    let output = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map_err(|e| format!("Failed to run kill -9: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::session::{RawAgentSession, SessionStatus};

    const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(1);

    fn make_session() -> AgentSession {
        let raw = RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 12345,
            process_start_identity: None,
            session_id: "abc-def-123".into(),
            cwd: "/Users/test/projects/my-app".into(),
            started_at: 0,
        };
        AgentSession::from_raw(raw)
    }

    fn accept_webhook(listener: &TcpListener) -> std::net::TcpStream {
        let deadline = Instant::now() + WEBHOOK_TIMEOUT;

        loop {
            match listener.accept() {
                Ok((stream, _)) => return stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for webhook request"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("failed to accept webhook request: {error}"),
            }
        }
    }

    fn read_content_length<R: BufRead>(reader: &mut R) -> usize {
        let mut content_length = 0;

        loop {
            let mut line = String::new();
            let bytes_read = reader.read_line(&mut line).unwrap();
            assert_ne!(
                bytes_read, 0,
                "webhook request ended before headers completed"
            );
            if line == "\r\n" {
                return content_length;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                content_length = value.trim().parse().unwrap();
            }
        }
    }

    #[test]
    #[should_panic(expected = "webhook request ended before headers completed")]
    fn webhook_headers_reject_premature_eof() {
        let mut request = BufReader::new(&b"POST / HTTP/1.1\r\nContent-Length: 1\r\n"[..]);
        read_content_length(&mut request);
    }

    #[test]
    fn status_webhook_keeps_only_retained_session_fields() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut stream = accept_webhook(&listener);
            stream.set_read_timeout(Some(WEBHOOK_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(WEBHOOK_TIMEOUT)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let content_length = read_content_length(&mut reader);

            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            String::from_utf8(body).unwrap()
        });

        let mut session = make_session();
        session.context_pressure = Some(42);
        session.status = SessionStatus::Processing;
        session.elapsed = Duration::from_secs(125);
        fire_webhook(&format!("http://{address}"), &session, "Waiting".into());

        let payload: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        let forbidden: Vec<String> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/legacy-forbidden-output-keys.json"
        ))
        .unwrap();
        let session = payload["session"].as_object().unwrap();
        let mut keys: Vec<&str> = session.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "context_pct",
                "elapsed_secs",
                "new_status",
                "old_status",
                "pid",
                "project",
            ]
        );
        assert_eq!(session["pid"], 12345);
        assert_eq!(session["project"], "my-app");
        assert_eq!(session["old_status"], "Waiting");
        assert_eq!(session["new_status"], "Processing");
        assert_eq!(session["context_pct"], 42);
        assert_eq!(session["elapsed_secs"], 125);

        for key in forbidden {
            assert!(
                !session.contains_key(&key),
                "webhook session contains legacy key {key}"
            );
        }
    }
}
