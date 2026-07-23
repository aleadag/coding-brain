use crate::session::AgentSession;
use crate::terminals::{PaneCapture, Terminal, checked_capture, run_bounded};

const MAX_PID_TARGETS: usize = 16;

fn parent_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn capture_target(target: &str) -> Result<PaneCapture, String> {
    let output = run_bounded(
        std::process::Command::new("kitty")
            .args(["@", "get-text", "--match", target, "--extent", "screen"]),
    )?;
    checked_capture(Terminal::Kitty, target.into(), output)
}

fn capture_with(
    pid: u32,
    mut parent_of: impl FnMut(u32) -> Option<u32>,
    mut capture: impl FnMut(&str) -> Result<PaneCapture, String>,
) -> Result<PaneCapture, String> {
    let mut current = pid;
    let mut visited = Vec::with_capacity(MAX_PID_TARGETS);
    let mut last_error = None;

    for _ in 0..MAX_PID_TARGETS {
        if current == 0 || visited.contains(&current) {
            break;
        }
        visited.push(current);

        let target = format!("pid:{current}");
        match capture(&target) {
            Ok(pane) => return Ok(pane),
            Err(error) => last_error = Some(error),
        }

        let Some(parent) = parent_of(current) else {
            break;
        };
        current = parent;
    }

    Err(last_error.unwrap_or_else(|| "no Kitty PID target matched the session".into()))
}

pub fn capture(session: &AgentSession) -> Result<PaneCapture, String> {
    capture_with(session.pid, parent_pid, capture_target)
}

pub fn send_enter(target: &str) -> Result<(), String> {
    let output = run_bounded(std::process::Command::new("kitty").args([
        "@",
        "send-text",
        "--match",
        target,
        "\r",
    ]))?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| "kitty send-text returned non-zero".into())
}

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("kitty");
    cmd.args(["@", "launch", "--type=tab", "--cwd", cwd, "codex"]);
    for arg in super::build_codex_args(prompt, resume) {
        cmd.arg(arg);
    }

    let output = cmd
        .output()
        .map_err(|e| format!("kitty launch failed: {e}. Is allow_remote_control enabled?"))?;

    if output.status.success() {
        Ok("kitty tab".into())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn switch(session: &AgentSession) -> Result<(), String> {
    // Kitty has a powerful remote control protocol via `kitty @ focus-window`.
    // Requires `allow_remote_control yes` or `allow_remote_control socket-only` in kitty.conf.
    // Match by the PID of the foreground process in the window.
    let pid = session.pid.to_string();

    // First try matching by the foreground process PID
    let output = std::process::Command::new("kitty")
        .args(["@", "focus-window", "--match", &format!("pid:{pid}")])
        .output();

    match output {
        Ok(o) if o.status.success() => return Ok(()),
        _ => {}
    }

    // Fallback: match by cwd
    let output = std::process::Command::new("kitty")
        .args([
            "@",
            "focus-window",
            "--match",
            &format!("cwd:{}", session.cwd),
        ])
        .output()
        .map_err(|e| format!("kitty @ failed: {e}. Is allow_remote_control enabled?"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("Kitty: {}", stderr.trim()))
    }
}

pub fn send_input(session: &AgentSession, text: &str) -> Result<(), String> {
    let output = std::process::Command::new("kitty")
        .args([
            "@",
            "send-text",
            "--match",
            &format!("pid:{}", session.pid),
            text,
        ])
        .output()
        .map_err(|e| format!("kitty send-text failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(target: &str) -> PaneCapture {
        PaneCapture {
            backend: Terminal::Kitty,
            target: target.into(),
            text: "approval prompt".into(),
        }
    }

    #[test]
    fn direct_pid_capture_does_not_lookup_parent() {
        let mut parent_lookups = 0;
        let mut targets = Vec::new();
        let capture = capture_with(
            30,
            |_| {
                parent_lookups += 1;
                None
            },
            |target| {
                targets.push(target.to_string());
                Ok(pane(target))
            },
        )
        .unwrap();

        assert_eq!(targets, ["pid:30"]);
        assert_eq!(parent_lookups, 0);
        assert_eq!(capture.target, "pid:30");
    }

    #[test]
    fn nested_pid_capture_uses_first_matching_ancestor() {
        let mut targets = Vec::new();
        let capture = capture_with(
            30,
            |pid| match pid {
                30 => Some(20),
                20 => Some(10),
                _ => None,
            },
            |target| {
                targets.push(target.to_string());
                match target {
                    "pid:10" => Ok(pane(target)),
                    _ => Err("no matching Kitty window".into()),
                }
            },
        )
        .unwrap();

        assert_eq!(targets, ["pid:30", "pid:20", "pid:10"]);
        assert_eq!(capture.target, "pid:10");
    }

    #[test]
    fn traversal_stops_when_parent_is_missing() {
        let mut targets = Vec::new();
        let error = capture_with(
            30,
            |_| None,
            |target| {
                targets.push(target.to_string());
                Err("no match".into())
            },
        )
        .unwrap_err();

        assert_eq!(targets, ["pid:30"]);
        assert_eq!(error, "no match");
    }

    #[test]
    fn traversal_stops_before_root_or_cycle() {
        for parent in [0, 30] {
            let mut targets = Vec::new();
            capture_with(
                30,
                |_| Some(parent),
                |target| {
                    targets.push(target.to_string());
                    Err("no match".into())
                },
            )
            .unwrap_err();

            assert_eq!(targets, ["pid:30"]);
        }
    }

    #[test]
    fn traversal_is_bounded() {
        let mut targets = Vec::new();
        capture_with(
            100,
            |pid| Some(pid - 1),
            |target| {
                targets.push(target.to_string());
                Err("no match".into())
            },
        )
        .unwrap_err();

        assert_eq!(targets.len(), 16);
        assert_eq!(targets.first().map(String::as_str), Some("pid:100"));
        assert_eq!(targets.last().map(String::as_str), Some("pid:85"));
    }
}
