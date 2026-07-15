use crate::session::CodexSession;
use crate::terminals::{PaneCapture, Terminal, checked_capture, run_bounded};

pub fn capture(session: &CodexSession) -> Result<PaneCapture, String> {
    let target = format!("pid:{}", session.pid);
    let output = run_bounded(
        std::process::Command::new("kitty")
            .args(["@", "get-text", "--match", &target, "--extent", "screen"]),
    )?;
    checked_capture(Terminal::Kitty, target, output)
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

pub fn switch(session: &CodexSession) -> Result<(), String> {
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

pub fn send_input(session: &CodexSession, text: &str) -> Result<(), String> {
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
