use crate::session::AgentSession;
use crate::terminals::{BoundedOutput, PaneCapture, Terminal, checked_capture, run_bounded};

fn pane_target(session: &AgentSession) -> Result<String, String> {
    let output = run_bounded(std::process::Command::new("tmux").args([
        "list-panes",
        "-a",
        "-F",
        "#{pane_tty}\t#{session_name}:#{window_index}.#{pane_index}",
    ]))?;
    if !output.status.success() {
        return Err("tmux list-panes returned non-zero".into());
    }
    let wanted = session.tty.trim_start_matches("/dev/");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(tty, _)| tty.trim_start_matches("/dev/") == wanted)
        .map(|(_, target)| target.to_string())
        .ok_or_else(|| format!("TTY {} not found in tmux panes", session.tty))
}

pub fn capture(session: &AgentSession) -> Result<PaneCapture, String> {
    let target = pane_target(session)?;
    let output = run_bounded(std::process::Command::new("tmux").args([
        "capture-pane",
        "-p",
        "-S",
        "-80",
        "-t",
        &target,
    ]))?;
    checked_capture(Terminal::Tmux, target, output)
}

pub fn send_enter(target: &str) -> Result<(), String> {
    let BoundedOutput { status, .. } =
        run_bounded(std::process::Command::new("tmux").args(["send-keys", "-t", target, "Enter"]))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "tmux send-keys returned non-zero".into())
}

pub fn launch(cwd: &str, prompt: Option<&str>, resume: Option<&str>) -> Result<String, String> {
    let mut parts = vec!["codex".to_string()];
    parts.extend(
        super::build_codex_args(prompt, resume)
            .into_iter()
            .map(|arg| super::shell_escape(&arg)),
    );
    let command = parts.join(" ");

    let output = std::process::Command::new("tmux")
        .args(["new-window", "-c", cwd, &command])
        .output()
        .map_err(|e| format!("tmux new-window failed: {e}"))?;

    if output.status.success() {
        Ok("tmux window".into())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn switch(session: &AgentSession) -> Result<(), String> {
    // tmux can list panes with their TTY: `tmux list-panes -a -F '#{pane_tty} #{session_name}:#{window_index}.#{pane_index}'`
    let target = pane_target(session)?;
    let _ = std::process::Command::new("tmux")
        .args(["select-window", "-t", &target])
        .output();
    let _ = std::process::Command::new("tmux")
        .args(["select-pane", "-t", &target])
        .output();
    Ok(())
}

pub fn send_input(session: &AgentSession, text: &str) -> Result<(), String> {
    let target = pane_target(session)?;
    let output = std::process::Command::new("tmux")
        .args(["send-keys", "-t", &target, text, ""])
        .output()
        .map_err(|error| format!("tmux send-keys failed: {error}"))?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| "tmux send-keys returned non-zero".into())
}
