#[cfg(target_os = "macos")]
mod apple;
#[cfg(target_os = "macos")]
mod ghostty;
mod gnome_terminal;
#[cfg(target_os = "macos")]
mod iterm2;
mod kitty;
mod tmux;
#[cfg(target_os = "macos")]
mod warp;
mod wezterm;
mod windows_terminal;

use crate::provider::AgentProvider;
use crate::session::{AgentSession, ApprovalEvidence, ApprovalObservation};
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
use std::process::{Command, ExitStatus};
#[cfg(any(unix, test))]
use std::time::Duration;

#[cfg(unix)]
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const CAPTURE_LINES: usize = 80;

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCapture {
    pub backend: Terminal,
    pub target: String,
    pub text: String,
}

#[derive(Clone, PartialEq, Eq)]
pub enum TerminalSessionAction {
    Allow,
    Deny,
    Continue,
    Text(String),
}

impl std::fmt::Debug for TerminalSessionAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow => formatter.write_str("Allow"),
            Self::Deny => formatter.write_str("Deny"),
            Self::Continue => formatter.write_str("Continue"),
            Self::Text(text) => formatter
                .debug_tuple("Text")
                .field(&format_args!("<redacted:{} bytes>", text.len()))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEvidence {
    pub provider: AgentProvider,
    pub action: TerminalSessionAction,
    pub backend: Terminal,
    pub target: String,
    pub pattern_version: u16,
    pub fingerprint: u64,
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalActionOutcome {
    pub action: TerminalSessionAction,
    pub backend: Terminal,
    pub target: String,
    pub prompt_cleared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardedActionFailure {
    NotSent(String),
    DeliveryUnknown(String),
}

impl GuardedActionFailure {
    pub fn message(&self) -> &str {
        match self {
            Self::NotSent(message) | Self::DeliveryUnknown(message) => message,
        }
    }
}

trait GuardedTerminalBackend {
    fn resolve_exact_target(&self, session: &AgentSession) -> Result<String, String>;
    fn capture(&self, target: &str) -> Result<PaneCapture, String>;
    fn send_literal(&self, target: &str, text: &str) -> Result<(), String>;
    fn send_keys(&self, target: &str, keys: &[&str]) -> Result<(), String>;
}

struct TmuxGuardedBackend;

impl GuardedTerminalBackend for TmuxGuardedBackend {
    fn resolve_exact_target(&self, session: &AgentSession) -> Result<String, String> {
        tmux::resolve_exact_target(session)
    }

    fn capture(&self, target: &str) -> Result<PaneCapture, String> {
        tmux::capture_target(target)
    }

    fn send_literal(&self, target: &str, text: &str) -> Result<(), String> {
        tmux::send_literal(target, text)
    }

    fn send_keys(&self, target: &str, keys: &[&str]) -> Result<(), String> {
        tmux::send_keys(target, keys)
    }
}

#[cfg(unix)]
fn terminate_process_group(process_group: u32) {
    if let Ok(process_group) = i32::try_from(process_group) {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

#[cfg(unix)]
fn terminate_and_reap(child: &mut std::process::Child) {
    terminate_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn set_nonblocking(stream: &(impl std::os::fd::AsRawFd + ?Sized)) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: `fd` belongs to a live child pipe; `fcntl` only reads and updates
    // that descriptor's file status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is still owned by the live pipe and `flags` came from it.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn read_available(
    stream: &mut impl Read,
    bytes: &mut Vec<u8>,
    label: &str,
) -> Result<bool, String> {
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
                if read > remaining {
                    return Err("terminal capture exceeded 64 KiB".into());
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(format!("terminal capture {label} read failed")),
        }
    }
}

#[cfg(unix)]
pub(crate) fn run_bounded(command: &mut Command) -> Result<BoundedOutput, String> {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("terminal capture command failed: {error}"))?;
    let Some(mut stdout) = child.stdout.take() else {
        terminate_and_reap(&mut child);
        return Err("terminal capture stdout unavailable".into());
    };
    let Some(mut stderr) = child.stderr.take() else {
        terminate_and_reap(&mut child);
        return Err("terminal capture stderr unavailable".into());
    };
    if set_nonblocking(&stdout).is_err() || set_nonblocking(&stderr).is_err() {
        terminate_and_reap(&mut child);
        drop(stdout);
        drop(stderr);
        return Err("terminal capture pipe configuration failed".into());
    }
    let deadline = std::time::Instant::now() + CAPTURE_TIMEOUT;
    let mut status = None;
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();

    loop {
        let read_result = (|| {
            if stdout_open {
                stdout_open = !read_available(&mut stdout, &mut stdout_bytes, "stdout")?;
            }
            if stderr_open {
                stderr_open = !read_available(&mut stderr, &mut stderr_bytes, "stderr")?;
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = read_result {
            terminate_and_reap(&mut child);
            drop(stdout);
            drop(stderr);
            return Err(error);
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    terminate_and_reap(&mut child);
                    drop(stdout);
                    drop(stderr);
                    return Err(format!("terminal capture wait failed: {error}"));
                }
            }
        }
        if let Some(status) = status.filter(|_| !stdout_open && !stderr_open) {
            terminate_process_group(child.id());
            return Ok(BoundedOutput {
                status,
                stdout: stdout_bytes,
            });
        }
        if std::time::Instant::now() >= deadline {
            terminate_and_reap(&mut child);
            drop(stdout);
            drop(stderr);
            return Err("terminal capture timed out".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(not(unix))]
pub(crate) fn run_bounded(_command: &mut Command) -> Result<BoundedOutput, String> {
    Err("guarded terminal capture is unsupported on this platform".into())
}

pub(crate) fn checked_capture(
    backend: Terminal,
    target: String,
    output: BoundedOutput,
) -> Result<PaneCapture, String> {
    if !output.status.success() {
        return Err("terminal capture command returned non-zero".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(CAPTURE_LINES);
    Ok(PaneCapture {
        backend,
        target,
        text: lines[start..].join("\n"),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalAction {
    Launch,
    Switch,
    Input,
    Approve,
}

impl TerminalAction {
    fn label(&self) -> &'static str {
        match self {
            TerminalAction::Launch => "Launch new session",
            TerminalAction::Switch => "Switch to session terminal",
            TerminalAction::Input => "Send input to session",
            TerminalAction::Approve => "Approve prompt",
        }
    }

    fn summary_name(&self) -> &'static str {
        match self {
            TerminalAction::Launch => "launch",
            TerminalAction::Switch => "switch",
            TerminalAction::Input => "input",
            TerminalAction::Approve => "approve",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoctorStatus {
    Ready,
    Blocked,
    Unsupported,
}

impl DoctorStatus {
    fn label(&self) -> &'static str {
        match self {
            DoctorStatus::Ready => "ok",
            DoctorStatus::Blocked => "blocked",
            DoctorStatus::Unsupported => "n/a",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: DoctorStatus,
    pub detail: String,
    pub fix: Option<String>,
}

impl DoctorCheck {
    fn ready(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Ready,
            detail: detail.into(),
            fix: None,
        }
    }

    #[cfg(test)]
    fn blocked(
        name: &'static str,
        detail: impl Into<String>,
        fix: impl Into<Option<String>>,
    ) -> Self {
        Self {
            name,
            status: DoctorStatus::Blocked,
            detail: detail.into(),
            fix: fix.into(),
        }
    }

    fn unsupported(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: DoctorStatus::Unsupported,
            detail: detail.into(),
            fix: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorReport {
    pub terminal: String,
    pub platform: String,
    pub actions: Vec<DoctorCheck>,
    pub prerequisites: Vec<DoctorCheck>,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Terminal {
    Gnome,
    Ghostty,
    Warp,
    ITerm2,
    Kitty,
    WezTerm,
    WindowsTerm,
    Apple,
    Tmux,
    Unknown(String),
}

fn terminal_name(t: &Terminal) -> &str {
    match t {
        Terminal::Gnome => "GNOME Terminal",
        Terminal::Ghostty => "Ghostty",
        Terminal::Warp => "Warp",
        Terminal::ITerm2 => "iTerm2",
        Terminal::Kitty => "Kitty",
        Terminal::WezTerm => "WezTerm",
        Terminal::WindowsTerm => "Windows Terminal",
        Terminal::Apple => "Apple Terminal",
        Terminal::Tmux => "tmux",
        Terminal::Unknown(name) => name,
    }
}

fn platform_label(os: &str, is_wsl: bool) -> String {
    if is_wsl && os == "linux" {
        "linux (WSL)".to_string()
    } else {
        os.to_string()
    }
}

fn platform_name() -> String {
    platform_label(std::env::consts::OS, is_wsl())
}

#[cfg(test)]
fn environment_notes(is_wsl: bool, has_windows_terminal_bridge: bool) -> Vec<String> {
    if !is_wsl {
        return Vec::new();
    }

    let mut notes = vec![
        "WSL detected. Linux session discovery should work normally inside the distro."
            .to_string(),
        "For reliable switch, input, and approval automation in WSL today, prefer tmux or Kitty inside WSL."
            .to_string(),
    ];

    if has_windows_terminal_bridge {
        notes.push(
            "Windows Terminal launch is available from WSL through `cmd.exe /c wt.exe`, but tab switching and input automation still rely on tmux or Kitty."
                .to_string(),
        );
    } else {
        notes.push(
            "Windows Terminal launch is not available from this WSL shell, so Coding Brain currently relies on Linux-native terminals inside WSL."
                .to_string(),
        );
    }

    notes
}

#[cfg(test)]
fn windows_terminal_bridge_ready() -> bool {
    command_ready("cmd.exe") && command_ready("wt.exe")
}

#[cfg(test)]
fn wsl_interop_check(is_wsl: bool) -> Option<DoctorCheck> {
    if !is_wsl {
        return None;
    }

    if windows_terminal_bridge_ready() {
        Some(DoctorCheck::ready(
            "Windows Terminal interop",
            "`cmd.exe /c wt.exe` is reachable from WSL.",
        ))
    } else if !command_ready("cmd.exe") {
        Some(DoctorCheck::blocked(
            "Windows Terminal interop",
            "`cmd.exe` is not on PATH from this WSL environment.",
            Some(
                "Enable WSL Windows interop or reopen this distro from a normal WSL shell."
                    .to_string(),
            ),
        ))
    } else {
        Some(DoctorCheck::blocked(
            "Windows Terminal interop",
            "`wt.exe` is not on PATH from this WSL environment.",
            Some(
                "Install Windows Terminal or enable WSL interop, then reopen the shell."
                    .to_string(),
            ),
        ))
    }
}

fn is_wsl() -> bool {
    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::env::var_os("WSL_INTEROP").is_some()
        {
            return true;
        }

        for path in ["/proc/sys/kernel/osrelease", "/proc/version"] {
            let Ok(contents) = std::fs::read_to_string(path) else {
                continue;
            };

            if contents.to_ascii_lowercase().contains("microsoft") {
                return true;
            }
        }
    }

    false
}

fn supported_actions(terminal: &Terminal) -> Vec<TerminalAction> {
    match terminal {
        Terminal::Gnome | Terminal::WindowsTerm => vec![TerminalAction::Launch],
        Terminal::Kitty | Terminal::Tmux => vec![
            TerminalAction::Launch,
            TerminalAction::Switch,
            TerminalAction::Input,
            TerminalAction::Approve,
        ],
        Terminal::WezTerm => vec![TerminalAction::Launch, TerminalAction::Switch],
        #[cfg(target_os = "macos")]
        Terminal::Ghostty | Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => {
            vec![TerminalAction::Switch, TerminalAction::Input]
        }
        Terminal::Unknown(_) => Vec::new(),
        #[cfg(not(target_os = "macos"))]
        _ => Vec::new(),
    }
}

pub(crate) fn build_codex_args(prompt: Option<&str>, resume: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(resume_id) = resume {
        args.push("resume".to_string());
        args.push(resume_id.to_string());
    }
    if let Some(prompt_text) = prompt {
        args.push(prompt_text.to_string());
    }
    args
}

pub(crate) fn shell_escape(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

pub fn detect_terminal() -> Terminal {
    if std::env::var("TMUX").is_ok() {
        return Terminal::Tmux;
    }

    if std::env::var("GNOME_TERMINAL_SERVICE").is_ok()
        || std::env::var("GNOME_TERMINAL_SCREEN").is_ok()
        || ancestor_process_contains("gnome-terminal")
    {
        return Terminal::Gnome;
    }

    if is_wsl() && std::env::var_os("WT_SESSION").is_some() {
        return Terminal::WindowsTerm;
    }

    // Terminal-specific env vars that don't rely on TERM_PROGRAM.
    // Some terminals (notably kitty on Linux) don't set TERM_PROGRAM at all.
    if let Some(term) = detect_by_native_env() {
        return term;
    }

    match std::env::var("TERM_PROGRAM").as_deref() {
        Ok("ghostty") => Terminal::Ghostty,
        Ok("WarpTerminal") => Terminal::Warp,
        Ok("iTerm.app") => Terminal::ITerm2,
        Ok("kitty") => Terminal::Kitty,
        Ok("WezTerm") => Terminal::WezTerm,
        Ok("Apple_Terminal") => Terminal::Apple,
        Ok(other) => Terminal::Unknown(other.to_string()),
        Err(_) => Terminal::Unknown("unknown".to_string()),
    }
}

/// Detect terminal from native env vars that each terminal sets unconditionally,
/// without relying on TERM_PROGRAM (which some terminals don't set on Linux).
fn detect_by_native_env() -> Option<Terminal> {
    // Kitty: KITTY_WINDOW_ID is set unconditionally per-window.
    // TERM=xterm-kitty is also reliable but can be inherited by child shells.
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return Some(Terminal::Kitty);
    }

    // WezTerm: WEZTERM_EXECUTABLE is set on all platforms.
    if std::env::var_os("WEZTERM_EXECUTABLE").is_some() {
        return Some(Terminal::WezTerm);
    }

    // Ghostty: GHOSTTY_RESOURCES_DIR is set on all platforms.
    if std::env::var_os("GHOSTTY_RESOURCES_DIR").is_some() {
        return Some(Terminal::Ghostty);
    }

    // TERM=xterm-kitty as last resort (weaker signal — can be inherited through ssh/tmux)
    if std::env::var("TERM").as_deref() == Ok("xterm-kitty") {
        return Some(Terminal::Kitty);
    }

    None
}

fn ancestor_process_contains(needle: &str) -> bool {
    let mut pid = unsafe { libc::getppid() } as u32;
    let needle = needle.to_ascii_lowercase();

    for _ in 0..8 {
        if pid == 0 {
            break;
        }

        let output = match std::process::Command::new("ps")
            .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
            .output()
        {
            Ok(output) => output,
            Err(_) => return false,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.trim();
        if line.is_empty() {
            break;
        }

        let mut parts = line.split_whitespace();
        let parent = parts
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let command = parts.collect::<Vec<_>>().join(" ").to_ascii_lowercase();
        if command.contains(&needle) {
            return true;
        }
        pid = parent;
    }

    false
}

#[allow(dead_code)]
pub(crate) fn can_launch_session() -> bool {
    supported_actions(&detect_terminal()).contains(&TerminalAction::Launch)
}

#[allow(dead_code)]
pub(crate) fn help_capability_summary() -> String {
    help_capability_summary_for(&detect_terminal())
}

fn help_capability_summary_for(terminal: &Terminal) -> String {
    let actions = supported_actions(terminal);
    if actions.is_empty() {
        format!(
            "Current terminal: {} (monitor-only)",
            terminal_name(terminal)
        )
    } else {
        let summary = actions
            .iter()
            .map(TerminalAction::summary_name)
            .collect::<Vec<_>>()
            .join(", ");
        format!("Current terminal: {} ({summary})", terminal_name(terminal))
    }
}

#[cfg(test)]
fn find_command_path(name: &str) -> Option<PathBuf> {
    if name.contains(std::path::MAIN_SEPARATOR) {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
fn binary_check(name: &'static str) -> DoctorCheck {
    match find_command_path(name) {
        Some(path) => DoctorCheck::ready(name, format!("Found at {}", path.display())),
        None => DoctorCheck::blocked(
            name,
            format!("`{name}` is not on PATH."),
            Some(format!("Install `{name}` or add it to PATH.")),
        ),
    }
}

#[cfg(test)]
fn command_ready(name: &'static str) -> bool {
    find_command_path(name).is_some()
}

#[cfg(test)]
fn output_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("Command exited with status {}", output.status)
}

#[cfg(test)]
fn probe_kitty_remote_control() -> Result<(), String> {
    let output = std::process::Command::new("kitty")
        .args(["@", "ls"])
        .output()
        .map_err(|e| format!("kitty @ ls failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

#[cfg(test)]
fn probe_tmux_connectivity() -> Result<(), String> {
    let output = std::process::Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_tty}"])
        .output()
        .map_err(|e| format!("tmux list-panes failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

#[cfg(test)]
fn probe_wezterm_cli() -> Result<(), String> {
    let output = std::process::Command::new("wezterm")
        .args(["cli", "list", "--format", "json"])
        .output()
        .map_err(|e| format!("wezterm cli list failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

#[cfg(all(test, target_os = "macos"))]
fn probe_system_events_access() -> Result<(), String> {
    let script = r#"tell application "System Events" to return UI elements enabled"#;
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("osascript probe failed: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(output_message(&output))
    }
}

#[cfg(test)]
fn action_check(
    action: TerminalAction,
    status: DoctorStatus,
    detail: impl Into<String>,
    fix: impl Into<Option<String>>,
) -> DoctorCheck {
    match status {
        DoctorStatus::Ready => DoctorCheck::ready(action.label(), detail),
        DoctorStatus::Blocked => DoctorCheck::blocked(action.label(), detail, fix),
        DoctorStatus::Unsupported => DoctorCheck::unsupported(action.label(), detail.into()),
    }
}

pub fn doctor_report() -> DoctorReport {
    navigation_doctor_report_for(detect_terminal())
}

fn navigation_doctor_report_for(terminal: Terminal) -> DoctorReport {
    let supported = matches!(
        terminal,
        Terminal::Kitty | Terminal::Tmux | Terminal::WezTerm
    ) || cfg!(target_os = "macos")
        && matches!(
            terminal,
            Terminal::Ghostty | Terminal::Warp | Terminal::ITerm2 | Terminal::Apple
        );
    let action = if supported {
        DoctorCheck::ready(
            TerminalAction::Switch.label(),
            "Coding Brain can focus a matching live session in this terminal.",
        )
    } else {
        DoctorCheck::unsupported(
            TerminalAction::Switch.label(),
            "Automatic session focus is unavailable in this terminal; Agent Deck may provide navigation when installed.",
        )
    };
    DoctorReport {
        terminal: terminal_name(&terminal).to_string(),
        platform: platform_name(),
        actions: vec![action],
        prerequisites: Vec::new(),
        notes: vec![
            "Agent Deck integration is optional; Coding Brain falls back to terminal focus."
                .to_string(),
        ],
    }
}

#[cfg(test)]
fn doctor_report_for(terminal: Terminal) -> DoctorReport {
    let terminal_label = terminal_name(&terminal).to_string();
    let is_wsl = is_wsl();
    let mut prerequisites = vec![binary_check("codex")];
    if let Some(wsl_check) = wsl_interop_check(is_wsl) {
        prerequisites.push(wsl_check);
    }
    let mut actions = Vec::new();
    let mut notes = vec![
        "Run `coding-brain doctor` inside the same terminal family that launches Codex."
            .to_string(),
        "`n` and `--new` use the same launch capability shown here.".to_string(),
    ];
    notes.extend(environment_notes(is_wsl, windows_terminal_bridge_ready()));

    match terminal {
        Terminal::Gnome => {
            let gnome_check = binary_check("gnome-terminal");
            let gnome_ready = gnome_check.status == DoctorStatus::Ready;
            prerequisites.push(gnome_check);

            let launch_status = if gnome_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let launch_detail = if gnome_ready {
                "GNOME Terminal can launch visible Codex sessions with `gnome-terminal --window`."
            } else {
                "GNOME Terminal CLI is unavailable, so visible launch cannot run."
            };
            let launch_fix =
                Some("Install GNOME Terminal and ensure `gnome-terminal` is on PATH.".to_string());
            actions.push(action_check(
                TerminalAction::Launch,
                launch_status,
                launch_detail,
                launch_fix.clone(),
            ));

            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "GNOME Terminal launch is supported, but reliable remote focus/input automation is not currently available.",
                    Some(
                        "Use tmux or Kitty when you need remote switching, input, or approval from Coding Brain."
                            .to_string(),
                    ),
                ));
            }

            notes.push(
                "GNOME Terminal launch works on Linux and was smoke-tested under Docker/X11. Remote focus/input automation is intentionally disabled until window targeting is reliable."
                    .to_string(),
            );
        }
        Terminal::WindowsTerm => {
            let cmd_check = binary_check("cmd.exe");
            let cmd_ready = cmd_check.status == DoctorStatus::Ready;
            prerequisites.push(cmd_check);

            let wt_check = binary_check("wt.exe");
            let wt_ready = wt_check.status == DoctorStatus::Ready;
            prerequisites.push(wt_check);

            let launch_status = if cmd_ready && wt_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let launch_detail = if launch_status == DoctorStatus::Ready {
                "Windows Terminal can open a new WSL tab in the current window and run `codex` there."
            } else {
                "Windows Terminal launch needs both `cmd.exe` and `wt.exe` reachable from this WSL shell."
            };
            let launch_fix = Some(
                "Enable WSL Windows interop, ensure Windows Terminal is installed, then rerun `coding-brain doctor`."
                    .to_string(),
            );
            actions.push(action_check(
                TerminalAction::Launch,
                launch_status,
                launch_detail,
                launch_fix.clone(),
            ));

            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "Windows Terminal launch works from WSL, but remote tab switching and input automation are not implemented there yet.",
                    Some(
                        "Use tmux or Kitty inside WSL when you need switch/input/approve automation."
                            .to_string(),
                    ),
                ));
            }

            notes.push(
                "Windows Terminal support is WSL-only and currently covers visible launch into a new tab, not remote control of existing tabs."
                    .to_string(),
            );
        }
        Terminal::Kitty => {
            let kitty_check = binary_check("kitty");
            let kitty_ready = kitty_check.status == DoctorStatus::Ready;
            prerequisites.push(kitty_check);

            let remote_check = if kitty_ready {
                match probe_kitty_remote_control() {
                    Ok(()) => DoctorCheck::ready(
                        "kitty remote control",
                        "`kitty @` is reachable from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "kitty remote control",
                        format!("`kitty @` is unavailable: {err}"),
                        Some(
                            "Set `allow_remote_control yes` or `allow_remote_control socket-only` in kitty.conf, then restart Kitty."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "kitty remote control",
                    "Kitty CLI is unavailable, so `kitty @` cannot be used.",
                    Some("Install Kitty and ensure `kitty` is on PATH.".to_string()),
                )
            };
            let remote_ready = remote_check.status == DoctorStatus::Ready;
            prerequisites.push(remote_check);

            let action_status = if kitty_ready && remote_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "Kitty can focus tabs and send text through `kitty @`."
            } else {
                "Kitty support is configured, but remote control is not currently available."
            };
            let fix = Some(
                "Enable Kitty remote control in kitty.conf and rerun `coding-brain doctor`."
                    .to_string(),
            );

            for action in supported_actions(&Terminal::Kitty) {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
        }
        Terminal::Tmux => {
            let tmux_check = binary_check("tmux");
            let tmux_ready = tmux_check.status == DoctorStatus::Ready;
            prerequisites.push(tmux_check);

            let session_check = if tmux_ready {
                match probe_tmux_connectivity() {
                    Ok(()) => DoctorCheck::ready(
                        "tmux session access",
                        "`tmux list-panes` can see the active server.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "tmux session access",
                        format!("tmux is installed, but pane discovery failed: {err}"),
                        Some(
                            "Run Coding Brain from inside the tmux session that owns the Codex panes."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "tmux session access",
                    "tmux is unavailable, so pane discovery cannot run.",
                    Some("Install tmux and rerun `coding-brain doctor`.".to_string()),
                )
            };
            let session_ready = session_check.status == DoctorStatus::Ready;
            prerequisites.push(session_check);

            let action_status = if tmux_ready && session_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "tmux can open windows, locate panes by TTY, and send keys."
            } else {
                "tmux support needs a reachable tmux server from this shell."
            };
            let fix = Some(
                "Run Coding Brain inside tmux or connect it to the same tmux server.".to_string(),
            );

            for action in supported_actions(&Terminal::Tmux) {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
        }
        Terminal::WezTerm => {
            let wezterm_check = binary_check("wezterm");
            let wezterm_ready = wezterm_check.status == DoctorStatus::Ready;
            prerequisites.push(wezterm_check);

            let cli_check = if wezterm_ready {
                match probe_wezterm_cli() {
                    Ok(()) => DoctorCheck::ready(
                        "wezterm cli",
                        "`wezterm cli` can query panes from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "wezterm cli",
                        format!("WezTerm CLI is installed, but pane discovery failed: {err}"),
                        Some(
                            "Run Coding Brain inside WezTerm with a reachable mux server."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "wezterm cli",
                    "WezTerm CLI is unavailable, so pane discovery cannot run.",
                    Some("Install WezTerm and ensure `wezterm` is on PATH.".to_string()),
                )
            };
            let cli_ready = cli_check.status == DoctorStatus::Ready;
            prerequisites.push(cli_check);

            let action_status = if wezterm_ready && cli_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = if action_status == DoctorStatus::Ready {
                "WezTerm supports visible launch and pane activation through `wezterm cli`."
            } else {
                "WezTerm support needs a reachable mux server from this shell."
            };
            let fix = Some(
                "Start Coding Brain from the same WezTerm environment that owns the Codex panes."
                    .to_string(),
            );

            for action in [TerminalAction::Launch, TerminalAction::Switch] {
                actions.push(action_check(action, action_status, detail, fix.clone()));
            }
            for action in [TerminalAction::Input, TerminalAction::Approve] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    "WezTerm integration currently supports launch and pane focus only.",
                    None::<String>,
                ));
            }
            notes.push("WezTerm input injection is not implemented yet.".to_string());
        }
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => {
            let apple_script_check = binary_check("osascript");
            let apple_script_ready = apple_script_check.status == DoctorStatus::Ready;
            prerequisites.push(apple_script_check);

            let detail = if apple_script_ready {
                "Ghostty exposes switch/input/approve through its AppleScript API."
            } else {
                "Ghostty support requires `osascript`."
            };
            let status = if apple_script_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let fix = Some(
                "Ensure macOS automation tools are available and Ghostty is running normally."
                    .to_string(),
            );

            for action in supported_actions(&Terminal::Ghostty) {
                actions.push(action_check(action, status, detail, fix.clone()));
            }
            actions.push(action_check(
                TerminalAction::Launch,
                DoctorStatus::Unsupported,
                "Visible launch is only implemented for tmux, Kitty, and WezTerm.",
                None::<String>,
            ));
            notes.push("Ghostty does not need Kitty-style remote control setup, but macOS may still prompt for automation access.".to_string());
        }
        #[cfg(target_os = "macos")]
        Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => {
            let apple_script_check = binary_check("osascript");
            let apple_script_ready = apple_script_check.status == DoctorStatus::Ready;
            prerequisites.push(apple_script_check);

            let system_events_check = if apple_script_ready {
                match probe_system_events_access() {
                    Ok(()) => DoctorCheck::ready(
                        "System Events access",
                        "AppleScript can talk to System Events from this shell.",
                    ),
                    Err(err) => DoctorCheck::blocked(
                        "System Events access",
                        format!("macOS UI scripting is not currently available: {err}"),
                        Some(
                            "Grant Automation/Accessibility access in System Settings > Privacy & Security, then rerun `coding-brain doctor`."
                                .to_string(),
                        ),
                    ),
                }
            } else {
                DoctorCheck::blocked(
                    "System Events access",
                    "`osascript` is unavailable, so macOS UI scripting cannot run.",
                    Some(
                        "Ensure `/usr/bin/osascript` is available and rerun the doctor."
                            .to_string(),
                    ),
                )
            };
            let system_events_ready = system_events_check.status == DoctorStatus::Ready;
            prerequisites.push(system_events_check);

            actions.push(action_check(
                TerminalAction::Launch,
                DoctorStatus::Unsupported,
                "Visible launch is only implemented for tmux, Kitty, and WezTerm.",
                None::<String>,
            ));

            let status = if apple_script_ready && system_events_ready {
                DoctorStatus::Ready
            } else {
                DoctorStatus::Blocked
            };
            let detail = format!(
                "{} uses AppleScript and System Events for focus and input control.",
                terminal_name(&terminal)
            );
            let fix = Some(
                "Grant Automation/Accessibility permissions to the terminal and rerun `coding-brain doctor`."
                    .to_string(),
            );
            for action in [
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(action, status, &detail, fix.clone()));
            }
        }
        Terminal::Unknown(name) => {
            for action in [
                TerminalAction::Launch,
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    format!(
                        "No integration is configured for `{name}`. Supported terminals: GNOME Terminal, Windows Terminal on WSL, tmux, Kitty, WezTerm, Ghostty, Warp, iTerm2, Terminal.app."
                    ),
                    None::<String>,
                ));
            }
            notes.push(
                "Monitoring still works in unsupported terminals, but control actions stay manual."
                    .to_string(),
            );
        }
        #[cfg(not(target_os = "macos"))]
        Terminal::Ghostty | Terminal::Warp | Terminal::ITerm2 | Terminal::Apple => {
            for action in [
                TerminalAction::Launch,
                TerminalAction::Switch,
                TerminalAction::Input,
                TerminalAction::Approve,
            ] {
                actions.push(action_check(
                    action,
                    DoctorStatus::Unsupported,
                    format!(
                        "{} control hooks are currently only implemented on macOS.",
                        terminal_name(&terminal)
                    ),
                    None::<String>,
                ));
            }
            notes.push(
                "Monitoring still works in unsupported terminals, but control actions stay manual."
                    .to_string(),
            );
        }
    }

    if !command_ready("codex") {
        notes.push("Launching a new session will fail until `codex` is on PATH.".to_string());
    }

    DoctorReport {
        terminal: terminal_label,
        platform: platform_name(),
        actions,
        prerequisites,
        notes,
    }
}

pub fn format_doctor_report(report: &DoctorReport) -> String {
    let mut lines = vec![
        "coding-brain doctor".to_string(),
        String::new(),
        format!("Platform: {}", report.platform),
        format!("Detected terminal: {}", report.terminal),
        String::new(),
        "Prerequisites".to_string(),
    ];

    for check in &report.prerequisites {
        lines.push(format!(
            "  [{}] {}: {}",
            check.status.label(),
            check.name,
            check.detail
        ));
        if let Some(fix) = &check.fix {
            lines.push(format!("      fix: {fix}"));
        }
    }

    lines.push(String::new());
    lines.push("Capabilities".to_string());
    for action in &report.actions {
        lines.push(format!(
            "  [{}] {}: {}",
            action.status.label(),
            action.name,
            action.detail
        ));
        if let Some(fix) = &action.fix {
            lines.push(format!("      fix: {fix}"));
        }
    }

    if !report.notes.is_empty() {
        lines.push(String::new());
        lines.push("Notes".to_string());
        for note in &report.notes {
            lines.push(format!("  - {note}"));
        }
    }

    lines.join("\n")
}

#[allow(dead_code)]
pub(crate) fn launch_session(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
) -> Result<String, String> {
    let terminal = detect_terminal();
    match terminal {
        Terminal::Gnome => gnome_terminal::launch(cwd, prompt, resume),
        Terminal::Kitty => kitty::launch(cwd, prompt, resume),
        Terminal::Tmux => tmux::launch(cwd, prompt, resume),
        Terminal::WezTerm => wezterm::launch(cwd, prompt, resume),
        Terminal::WindowsTerm => windows_terminal::launch(cwd, prompt, resume),
        other => Err(format!(
            "Visible session launch is not supported in {}. Start `codex` manually, use tmux/Kitty/WezTerm/GNOME Terminal/Windows Terminal on WSL, or run `coding-brain doctor` for setup guidance.",
            terminal_name(&other)
        )),
    }
}

pub fn switch_to_terminal(session: &AgentSession) -> Result<(), String> {
    let terminal = detect_terminal();

    // Only require a TTY for terminals that match sessions by TTY name.
    // Kitty, Ghostty, and Warp use their own IPC (PID/cwd matching) and don't need it.
    let needs_tty = matches!(
        terminal,
        Terminal::Tmux | Terminal::WezTerm | Terminal::Apple | Terminal::ITerm2
    );
    if needs_tty && session.tty.is_empty() {
        return Err("No TTY associated with this session".into());
    }
    crate::logger::log(
        "DEBUG",
        &format!(
            "terminal switch: {} (tty={}) via {:?}",
            session.display_name(),
            session.tty,
            terminal_name(&terminal)
        ),
    );

    match terminal {
        Terminal::Gnome => gnome_terminal::switch(session),
        Terminal::Kitty => kitty::switch(session),
        Terminal::WezTerm => wezterm::switch(session),
        Terminal::Tmux => tmux::switch(session),
        Terminal::WindowsTerm => Err(
            "Windows Terminal currently supports WSL launch only. Use tmux or Kitty inside WSL for session switching."
                .into(),
        ),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::ITerm2 => iterm2::switch(session),
        #[cfg(target_os = "macos")]
        Terminal::Apple => apple::switch(session),
        Terminal::Unknown(name) => Err(format!(
            "Unsupported terminal: {name}. Supported: GNOME Terminal, Windows Terminal on WSL (launch only), Ghostty, Warp, iTerm2, Kitty, WezTerm, Terminal.app, tmux. Run `coding-brain doctor` for details."
        )),
        #[cfg(not(target_os = "macos"))]
        _ => Err("Terminal switching not supported on this platform. Run `coding-brain doctor` for details.".into()),
    }
}

/// Focuses only a terminal pane bound to the session's complete live process identity.
pub fn focus_exact_terminal(session: &AgentSession) -> Result<(), String> {
    let target = TmuxGuardedBackend.resolve_exact_target(session)?;
    tmux::focus_target(&target)
}

#[allow(dead_code)]
pub(crate) fn send_input(session: &AgentSession, text: &str) -> Result<(), String> {
    match detect_terminal() {
        Terminal::Gnome => gnome_terminal::send_input(session, text),
        #[cfg(target_os = "macos")]
        Terminal::Ghostty => ghostty::send_input(session, text),
        Terminal::Kitty => kitty::send_input(session, text),
        Terminal::Tmux => tmux::send_input(session, text),
        Terminal::WindowsTerm => Err(
            "Windows Terminal currently supports WSL launch only. Use tmux or Kitty inside WSL for session input automation."
                .into(),
        ),
        #[cfg(target_os = "macos")]
        Terminal::Warp => warp::send_input(session, text),
        #[cfg(target_os = "macos")]
        _ => {
            // iTerm2, Apple Terminal, etc: switch + System Events keystroke
            switch_to_terminal(session)?;
            std::thread::sleep(std::time::Duration::from_millis(300));
            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
            run_osascript(&format!(
                r#"tell application "System Events" to keystroke "{escaped}""#,
            ))
        }
        #[cfg(not(target_os = "macos"))]
        _ => Err("Input injection not supported for this terminal. Run `coding-brain doctor` for details.".into()),
    }
}

struct ApprovalPromptPattern {
    version: u16,
    question: &'static str,
    choice_anchors: &'static [&'static str],
    confirmation: &'static str,
}

const APPROVAL_PROMPT_PATTERNS: &[ApprovalPromptPattern] = &[
    ApprovalPromptPattern {
        version: 1,
        question: "would you like to run the following command?",
        choice_anchors: &[
            "1. yes, just this once",
            "2. yes, and don't ask again for commands that start with",
            "3. no, and tell codex what to do differently",
        ],
        confirmation: "press enter to confirm",
    },
    ApprovalPromptPattern {
        version: 2,
        question: "would you like to run the following command?",
        choice_anchors: &[
            "1. yes, proceed",
            "2. yes, and don't ask again for commands that start with",
            "3. no, and tell codex what to do differently",
        ],
        confirmation: "press enter to confirm",
    },
    ApprovalPromptPattern {
        version: 3,
        question: "would you like to run the following command?",
        choice_anchors: &[
            "1. yes, just this once",
            "2. yes, and don't ask again for this command in this session",
            "3. no, and tell codex what to do differently",
        ],
        confirmation: "press enter to confirm",
    },
];

trait ApprovalIo {
    fn capture(&self, session: &AgentSession) -> Result<PaneCapture, String>;
    fn send_enter(
        &self,
        session: &AgentSession,
        backend: Terminal,
        target: &str,
    ) -> Result<(), String>;
}

struct RealApprovalIo;

impl ApprovalIo for RealApprovalIo {
    fn capture(&self, session: &AgentSession) -> Result<PaneCapture, String> {
        capture_session(session)
    }

    fn send_enter(
        &self,
        _session: &AgentSession,
        backend: Terminal,
        target: &str,
    ) -> Result<(), String> {
        send_enter_to_target(backend, target)
    }
}

fn capture_session(session: &AgentSession) -> Result<PaneCapture, String> {
    let captures = [tmux::capture(session), kitty::capture(session)]
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    match captures.as_slice() {
        [capture] => Ok(capture.clone()),
        [] => Err("no supported terminal pane matched the session".into()),
        _ => Err("multiple terminal panes matched the session".into()),
    }
}

fn send_enter_to_target(backend: Terminal, target: &str) -> Result<(), String> {
    match backend {
        Terminal::Tmux => tmux::send_enter(target),
        Terminal::Kitty => kitty::send_enter(target),
        _ => Err("approval backend does not support guarded input".into()),
    }
}

fn strip_ansi(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn normalize_whitespace(value: &str) -> String {
    strip_ansi(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn fingerprint(value: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn displayed_command(lines: &[&str], question_index: usize, choice_index: usize) -> Option<String> {
    let command_index = lines[question_index + 1..choice_index]
        .iter()
        .rposition(|line| line.contains("$ "))?
        + question_index
        + 1;
    let command_column = lines[command_index].find("$ ")?;

    let mut command_lines = Vec::new();
    command_lines.push(lines[command_index][command_column + 2..].trim());
    for line in &lines[command_index + 1..choice_index] {
        let continuation = line.get(command_column..).unwrap_or(line).trim();
        if continuation.is_empty() {
            break;
        }
        command_lines.push(continuation);
    }
    Some(normalize_whitespace(&command_lines.join(" ")))
}

fn strip_prompt_gutter(line: &str) -> &str {
    let Some((prefix, content)) = line.split_once('│') else {
        return line;
    };
    let marker = prefix.trim();
    if marker.is_empty()
        || marker.chars().all(|character| {
            character.is_ascii_digit() || character.is_ascii_whitespace() || character == '#'
        })
    {
        let mut content = content.trim_end();
        for _ in 0..2 {
            let Some(stripped) = content.strip_suffix('│') else {
                break;
            };
            content = stripped.trim_end();
        }
        content
    } else {
        line
    }
}

fn last_approval_prompt(text: &str) -> Option<(&'static ApprovalPromptPattern, String, String)> {
    let cleaned = strip_ansi(text);
    let lines = cleaned.lines().map(strip_prompt_gutter).collect::<Vec<_>>();
    let normalized = lines
        .iter()
        .map(|line| normalize_whitespace(line).to_ascii_lowercase())
        .collect::<Vec<_>>();

    let question_index = (0..lines.len()).rev().find(|index| {
        APPROVAL_PROMPT_PATTERNS
            .iter()
            .any(|pattern| normalized[*index].contains(pattern.question))
    })?;
    for pattern in APPROVAL_PROMPT_PATTERNS {
        if !normalized[question_index].contains(pattern.question) {
            continue;
        }

        let mut cursor = question_index + 1;
        let mut first_choice_index = None;
        let mut complete = true;
        for anchor in pattern.choice_anchors {
            let Some(choice_index) = normalized[cursor..]
                .iter()
                .position(|line| line.contains(anchor))
                .map(|relative| cursor + relative)
            else {
                complete = false;
                break;
            };
            first_choice_index.get_or_insert(choice_index);
            cursor = choice_index + 1;
        }
        if !complete {
            continue;
        }

        let Some(confirmation_index) = normalized[cursor..]
            .iter()
            .position(|line| line.contains(pattern.confirmation))
            .map(|relative| cursor + relative)
        else {
            continue;
        };
        let command = displayed_command(&lines, question_index, first_choice_index?)?;
        let block = normalize_whitespace(&lines[question_index..=confirmation_index].join("\n"))
            .to_ascii_lowercase();
        return Some((pattern, command, block));
    }

    None
}

fn match_approval_prompt(
    capture: &PaneCapture,
    session: &AgentSession,
) -> Option<ApprovalEvidence> {
    if !session.is_shell_permission_request() {
        return None;
    }
    let call_id = session.pending_tool_call_id.as_deref()?;
    let tool = session.pending_tool_name.as_deref()?;
    let pending_input = session.pending_tool_input.as_deref()?;
    let (pattern, displayed_command, block) = last_approval_prompt(&capture.text)?;
    let is_wrapper = tool == "exec" && pending_input.contains("tools.exec_command(");
    if !is_wrapper
        && !normalize_whitespace(&displayed_command)
            .eq_ignore_ascii_case(&normalize_whitespace(pending_input))
    {
        return None;
    }
    let evidence_tool = if is_wrapper { "exec_command" } else { tool };

    Some(ApprovalEvidence {
        session_id: session.session_id.clone(),
        tty: session.tty.clone(),
        call_id: call_id.into(),
        tool: evidence_tool.into(),
        command: displayed_command,
        backend: capture.backend.clone(),
        target: capture.target.clone(),
        prompt_pattern_version: pattern.version,
        prompt_fingerprint: fingerprint(&block),
    })
}

const CODEX_RECOVERY_PATTERN_VERSION: u16 = 1;
const CODEX_RECOVERY_PLACEHOLDER: &str = "› Ask Codex to do anything";
const CLAUDE_RECOVERY_PATTERN_VERSION: u16 = 1;
const ANTIGRAVITY_PERMISSION_PATTERN_VERSION: u16 = 1;
const ANTIGRAVITY_RECOVERY_PATTERN_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedProviderPrompt {
    pattern_version: u16,
    block: String,
    tool_use_id: Option<String>,
    has_trailing_text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LatestProviderPrompt {
    Absent,
    Incomplete,
    Complete(ParsedProviderPrompt),
}

#[derive(Debug)]
struct PromptLine {
    exact: String,
    lower: String,
}

fn prompt_lines(text: &str) -> Vec<PromptLine> {
    let cleaned = strip_ansi(text);
    cleaned
        .lines()
        .map(normalize_whitespace)
        .filter(|line| !line.is_empty())
        .map(|exact| PromptLine {
            lower: exact.to_ascii_lowercase(),
            exact,
        })
        .collect()
}

fn latest_question(lines: &[PromptLine], question: &str) -> Option<usize> {
    lines.iter().rposition(|line| line.lower == question)
}

fn has_prompt_remnants(
    lines: &[PromptLine],
    provider: AgentProvider,
    action: &TerminalSessionAction,
) -> bool {
    lines.iter().any(|line| match (provider, action) {
        (AgentProvider::Claude, TerminalSessionAction::Continue) => {
            matches!(
                line.lower.as_str(),
                "1. continue" | "2. stop" | "press enter to confirm"
            ) || (line.lower.contains("claude")
                && (line.lower.contains("stopped") || line.lower.contains("completing the task")))
        }
        (
            AgentProvider::Antigravity,
            TerminalSessionAction::Allow | TerminalSessionAction::Deny,
        ) => {
            line.lower.starts_with("tool use id:")
                || line.lower.starts_with("request id:")
                || matches!(
                    line.lower.as_str(),
                    "1. allow" | "2. deny" | "press enter to confirm"
                )
                || (line.lower.contains("antigravity")
                    && (line.lower.contains("run a tool") || line.lower.contains("permission")))
        }
        (AgentProvider::Antigravity, TerminalSessionAction::Continue) => {
            matches!(
                line.lower.as_str(),
                "1. continue" | "2. stop" | "press enter to confirm"
            ) || (line.lower.contains("antigravity")
                && (line.lower.contains("stopped") || line.lower.contains("completing the task")))
        }
        _ => false,
    })
}

fn exact_fixed_prompt(
    lines: &[PromptLine],
    start: usize,
    pattern_version: u16,
    expected: &[&str],
) -> LatestProviderPrompt {
    let Some(candidate) = lines.get(start..start.saturating_add(expected.len())) else {
        return LatestProviderPrompt::Incomplete;
    };
    if !candidate
        .iter()
        .zip(expected)
        .all(|(line, expected)| line.lower == *expected)
    {
        return LatestProviderPrompt::Incomplete;
    }
    LatestProviderPrompt::Complete(ParsedProviderPrompt {
        pattern_version,
        block: candidate
            .iter()
            .map(|line| line.lower.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        tool_use_id: None,
        has_trailing_text: lines.len() != start + expected.len(),
    })
}

fn latest_claude_prompt(text: &str, action: &TerminalSessionAction) -> LatestProviderPrompt {
    let lines = prompt_lines(text);
    let expected = match action {
        TerminalSessionAction::Continue => [
            "claude stopped before completing the task.",
            "1. continue",
            "2. stop",
            "press enter to confirm",
        ],
        _ => return LatestProviderPrompt::Absent,
    };
    let Some(start) = latest_question(&lines, expected[0]) else {
        return if has_prompt_remnants(&lines, AgentProvider::Claude, action) {
            LatestProviderPrompt::Incomplete
        } else {
            LatestProviderPrompt::Absent
        };
    };
    exact_fixed_prompt(&lines, start, CLAUDE_RECOVERY_PATTERN_VERSION, &expected)
}

fn codex_context_footer(line: &PromptLine) -> bool {
    let tokens = line.lower.split_whitespace().collect::<Vec<_>>();
    let context_percent = tokens
        .get(3)
        .and_then(|token| token.strip_suffix('%'))
        .and_then(|percent| percent.parse::<u8>().ok());
    tokens.get(0..3) == Some(&["?", "for", "shortcuts"])
        && context_percent.is_some_and(|percent| percent <= 100)
        && tokens.get(4..) == Some(&["context", "left"])
}

fn exact_codex_active_recovery_prompt(lines: &[PromptLine]) -> bool {
    let [.., status, composer, footer] = lines else {
        return false;
    };
    status.exact.starts_with("• ")
        && status.exact.contains(" (")
        && status.exact.ends_with(" • esc to interrupt)")
        && composer.exact == CODEX_RECOVERY_PLACEHOLDER
        && codex_context_footer(footer)
}

fn latest_codex_recovery_prompt(text: &str) -> LatestProviderPrompt {
    let lines = prompt_lines(text);
    let has_remnant = lines.iter().any(|line| {
        line.exact == CODEX_RECOVERY_PLACEHOLDER
            || line.lower.starts_with("? for shortcuts")
            || line.lower.contains("context left")
    });
    let [.., composer, footer] = lines.as_slice() else {
        return if has_remnant {
            LatestProviderPrompt::Incomplete
        } else {
            LatestProviderPrompt::Absent
        };
    };
    if exact_codex_active_recovery_prompt(&lines)
        || composer.exact != CODEX_RECOVERY_PLACEHOLDER
        || !codex_context_footer(footer)
    {
        return LatestProviderPrompt::Incomplete;
    }
    LatestProviderPrompt::Complete(ParsedProviderPrompt {
        pattern_version: CODEX_RECOVERY_PATTERN_VERSION,
        block: format!("{} {}", composer.exact, footer.lower),
        tool_use_id: None,
        has_trailing_text: false,
    })
}

fn latest_antigravity_prompt(text: &str, action: &TerminalSessionAction) -> LatestProviderPrompt {
    let lines = prompt_lines(text);
    match action {
        TerminalSessionAction::Allow | TerminalSessionAction::Deny => {
            const QUESTION: &str = "antigravity wants to run a tool.";
            let Some(start) = latest_question(&lines, QUESTION) else {
                return if has_prompt_remnants(&lines, AgentProvider::Antigravity, action) {
                    LatestProviderPrompt::Incomplete
                } else {
                    LatestProviderPrompt::Absent
                };
            };
            let Some(candidate) = lines.get(start..start.saturating_add(5)) else {
                return LatestProviderPrompt::Incomplete;
            };
            let tool_prefix = "tool use id:";
            let tool_use_id = candidate[1]
                .exact
                .get(tool_prefix.len()..)
                .map(str::trim)
                .filter(|id| {
                    candidate[1].lower.starts_with(tool_prefix)
                        && !id.is_empty()
                        && !id.chars().any(char::is_whitespace)
                })
                .map(str::to_owned);
            if candidate[0].lower != QUESTION
                || tool_use_id.is_none()
                || candidate[2].lower != "1. allow"
                || candidate[3].lower != "2. deny"
                || candidate[4].lower != "press enter to confirm"
            {
                return LatestProviderPrompt::Incomplete;
            }
            LatestProviderPrompt::Complete(ParsedProviderPrompt {
                pattern_version: ANTIGRAVITY_PERMISSION_PATTERN_VERSION,
                block: candidate
                    .iter()
                    .map(|line| line.lower.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                tool_use_id,
                has_trailing_text: lines.len() != start + candidate.len(),
            })
        }
        TerminalSessionAction::Continue => {
            const EXPECTED: [&str; 4] = [
                "antigravity stopped before completing the task.",
                "1. continue",
                "2. stop",
                "press enter to confirm",
            ];
            let Some(start) = latest_question(&lines, EXPECTED[0]) else {
                return if has_prompt_remnants(&lines, AgentProvider::Antigravity, action) {
                    LatestProviderPrompt::Incomplete
                } else {
                    LatestProviderPrompt::Absent
                };
            };
            exact_fixed_prompt(
                &lines,
                start,
                ANTIGRAVITY_RECOVERY_PATTERN_VERSION,
                &EXPECTED,
            )
        }
        TerminalSessionAction::Text(_) => LatestProviderPrompt::Absent,
    }
}

fn recognized_provider_prompt(
    candidate: LatestProviderPrompt,
) -> Option<(u16, String, Option<String>)> {
    let LatestProviderPrompt::Complete(prompt) = candidate else {
        return None;
    };
    if prompt.has_trailing_text {
        return None;
    }
    Some((prompt.pattern_version, prompt.block, prompt.tool_use_id))
}

fn match_semantic_prompt(
    capture: &PaneCapture,
    session: &AgentSession,
    action: &TerminalSessionAction,
) -> Option<PromptEvidence> {
    let (pattern_version, block, tool_use_id) = match session.provider {
        AgentProvider::Codex => {
            if matches!(action, TerminalSessionAction::Continue) {
                recognized_provider_prompt(latest_codex_recovery_prompt(&capture.text))?
            } else {
                let evidence = match_approval_prompt(capture, session)?;
                if !matches!(
                    action,
                    TerminalSessionAction::Allow | TerminalSessionAction::Deny
                ) {
                    return None;
                }
                (
                    evidence.prompt_pattern_version,
                    evidence.prompt_fingerprint.to_string(),
                    Some(evidence.call_id),
                )
            }
        }
        AgentProvider::Claude => {
            recognized_provider_prompt(latest_claude_prompt(&capture.text, action))?
        }
        AgentProvider::Antigravity => {
            recognized_provider_prompt(latest_antigravity_prompt(&capture.text, action))?
        }
    };
    if session.provider == AgentProvider::Antigravity
        && matches!(
            action,
            TerminalSessionAction::Allow | TerminalSessionAction::Deny
        )
    {
        let current = tool_use_id.as_deref()?;
        if session
            .pending_tool_call_id
            .as_deref()
            .is_some_and(|pending| current != pending)
        {
            return None;
        }
    }
    Some(PromptEvidence {
        provider: session.provider,
        action: action.clone(),
        backend: capture.backend.clone(),
        target: capture.target.clone(),
        pattern_version,
        fingerprint: if session.provider == AgentProvider::Codex
            && !matches!(action, TerminalSessionAction::Continue)
        {
            block.parse().ok()?
        } else {
            fingerprint(&block)
        },
        tool_use_id,
    })
}

fn post_prompt_advanced(
    capture: &PaneCapture,
    session: &AgentSession,
    action: &TerminalSessionAction,
    expected: &PromptEvidence,
) -> bool {
    let candidate = match session.provider {
        AgentProvider::Codex => {
            if matches!(action, TerminalSessionAction::Continue) {
                return match latest_codex_recovery_prompt(&capture.text) {
                    LatestProviderPrompt::Complete(prompt) => {
                        fingerprint(&prompt.block) != expected.fingerprint
                    }
                    LatestProviderPrompt::Absent => true,
                    LatestProviderPrompt::Incomplete => {
                        exact_codex_active_recovery_prompt(&prompt_lines(&capture.text))
                    }
                };
            } else {
                let question_present = prompt_lines(&capture.text).iter().any(|line| {
                    line.lower
                        .contains("would you like to run the following command?")
                });
                return !question_present;
            }
        }
        AgentProvider::Claude => latest_claude_prompt(&capture.text, action),
        AgentProvider::Antigravity => latest_antigravity_prompt(&capture.text, action),
    };
    match candidate {
        LatestProviderPrompt::Absent => true,
        LatestProviderPrompt::Incomplete => false,
        LatestProviderPrompt::Complete(prompt) if prompt.has_trailing_text => false,
        LatestProviderPrompt::Complete(prompt) => expected
            .tool_use_id
            .as_deref()
            .zip(prompt.tool_use_id.as_deref())
            .is_some_and(|(previous, current)| previous != current),
    }
}

fn semantic_keys(
    provider: AgentProvider,
    action: &TerminalSessionAction,
) -> Option<&'static [&'static str]> {
    match (provider, action) {
        (AgentProvider::Codex, TerminalSessionAction::Allow) => Some(&["Enter"]),
        (AgentProvider::Codex, TerminalSessionAction::Deny) => Some(&["3", "Enter"]),
        (AgentProvider::Claude | AgentProvider::Antigravity, TerminalSessionAction::Allow) => {
            Some(&["1", "Enter"])
        }
        (AgentProvider::Claude | AgentProvider::Antigravity, TerminalSessionAction::Deny) => {
            Some(&["2", "Enter"])
        }
        (AgentProvider::Claude | AgentProvider::Antigravity, TerminalSessionAction::Continue) => {
            Some(&["1", "Enter"])
        }
        _ => None,
    }
}

fn validate_manual_text(text: &str) -> Result<(), String> {
    if text.len() > 4096 {
        return Err("manual text exceeds 4096 bytes".into());
    }
    if text.contains('\n') || text.contains('\r') {
        return Err("manual text must be a single line".into());
    }
    if text.chars().any(|character| character.is_ascii_control()) {
        return Err("manual text contains a control character".into());
    }
    Ok(())
}

fn checked_target_capture(
    backend: &dyn GuardedTerminalBackend,
    target: &str,
) -> Result<PaneCapture, String> {
    let capture = backend
        .capture(target)
        .map_err(|_| "guarded terminal capture failed".to_string())?;
    if capture.target != target {
        return Err("guarded terminal capture target changed".into());
    }
    if capture.text.len() > MAX_CAPTURE_BYTES || capture.text.lines().count() > CAPTURE_LINES {
        return Err("guarded terminal capture exceeded bounds".into());
    }
    Ok(capture)
}

fn execute_guarded_action_classified_with(
    session: &AgentSession,
    action: TerminalSessionAction,
    backend: &dyn GuardedTerminalBackend,
) -> Result<TerminalActionOutcome, GuardedActionFailure> {
    let not_sent = GuardedActionFailure::NotSent;
    let delivery_unknown = GuardedActionFailure::DeliveryUnknown;
    if let TerminalSessionAction::Text(text) = &action {
        validate_manual_text(text).map_err(not_sent)?;
    }
    session.live_process_identity().ok_or_else(|| {
        not_sent("guarded terminal action requires an exact live process identity".to_string())
    })?;

    let target = backend.resolve_exact_target(session).map_err(not_sent)?;
    let initial = checked_target_capture(backend, &target).map_err(not_sent)?;

    if let TerminalSessionAction::Text(text) = &action {
        let baseline_fingerprint = fingerprint(&normalize_whitespace(&initial.text));
        if backend.resolve_exact_target(session).map_err(not_sent)? != target {
            return Err(not_sent(
                "guarded terminal target changed; action cancelled".into(),
            ));
        }
        backend
            .send_literal(&target, text)
            .map_err(|_| not_sent("guarded terminal literal send failed".to_string()))?;
        backend
            .send_keys(&target, &["Enter"])
            .map_err(|_| delivery_unknown("guarded terminal key send failed".to_string()))?;
        let post = checked_target_capture(backend, &target).map_err(delivery_unknown)?;
        if post.backend != initial.backend {
            return Err(delivery_unknown(
                "guarded terminal capture backend changed".into(),
            ));
        }
        if fingerprint(&normalize_whitespace(&post.text)) == baseline_fingerprint {
            return Err(delivery_unknown(
                "terminal prompt did not advance after action".into(),
            ));
        }
        return Ok(TerminalActionOutcome {
            action: TerminalSessionAction::Text(String::new()),
            backend: initial.backend,
            target,
            prompt_cleared: true,
        });
    }

    let expected = match_semantic_prompt(&initial, session, &action)
        .ok_or_else(|| not_sent("no recognized complete provider prompt for action".to_string()))?;
    if backend.resolve_exact_target(session).map_err(not_sent)? != target {
        return Err(not_sent(
            "guarded terminal target changed; action cancelled".into(),
        ));
    }
    let recapture = checked_target_capture(backend, &target).map_err(not_sent)?;
    let current = match_semantic_prompt(&recapture, session, &action)
        .ok_or_else(|| not_sent("provider prompt changed or disappeared".to_string()))?;
    if current != expected {
        return Err(not_sent("provider prompt changed; action cancelled".into()));
    }
    if session.provider == AgentProvider::Codex && matches!(action, TerminalSessionAction::Continue)
    {
        backend
            .send_literal(&target, "continue")
            .map_err(|_| not_sent("guarded terminal literal send failed".to_string()))?;
        backend
            .send_keys(&target, &["Enter"])
            .map_err(|_| not_sent("guarded terminal key send failed".to_string()))?;
    } else {
        let keys = semantic_keys(session.provider, &action).ok_or_else(|| {
            not_sent("provider does not support this terminal action".to_string())
        })?;
        backend
            .send_keys(&target, keys)
            .map_err(|_| not_sent("guarded terminal key send failed".to_string()))?;
    }
    let post = checked_target_capture(backend, &target).map_err(delivery_unknown)?;
    if post.backend != initial.backend {
        return Err(delivery_unknown(
            "guarded terminal capture backend changed".into(),
        ));
    }
    if !post_prompt_advanced(&post, session, &action, &expected) {
        return Err(delivery_unknown(
            "terminal prompt did not advance after action".into(),
        ));
    }

    Ok(TerminalActionOutcome {
        action,
        backend: initial.backend,
        target,
        prompt_cleared: true,
    })
}

fn execute_guarded_action_with(
    session: &AgentSession,
    action: TerminalSessionAction,
    backend: &dyn GuardedTerminalBackend,
) -> Result<TerminalActionOutcome, String> {
    execute_guarded_action_classified_with(session, action, backend)
        .map_err(|error| error.message().to_string())
}

fn probe_recovery_prompt_with(
    session: &AgentSession,
    backend: &dyn GuardedTerminalBackend,
) -> Result<PromptEvidence, String> {
    session.live_process_identity().ok_or_else(|| {
        "recovery prompt probe requires an exact live process identity".to_string()
    })?;
    let target = backend.resolve_exact_target(session)?;
    let capture = checked_target_capture(backend, &target)?;
    match_semantic_prompt(&capture, session, &TerminalSessionAction::Continue)
        .ok_or_else(|| "no recognized complete provider recovery prompt".to_string())
}

pub fn probe_recovery_prompt(session: &AgentSession) -> Result<PromptEvidence, String> {
    probe_recovery_prompt_with(session, &TmuxGuardedBackend)
}

fn probe_actionable_prompt_with(
    session: &AgentSession,
    backend: &dyn GuardedTerminalBackend,
) -> Result<PromptEvidence, String> {
    session.live_process_identity().ok_or_else(|| {
        "actionable prompt probe requires an exact live process identity".to_string()
    })?;
    let target = backend.resolve_exact_target(session)?;
    let capture = checked_target_capture(backend, &target)?;
    [
        TerminalSessionAction::Continue,
        TerminalSessionAction::Allow,
        TerminalSessionAction::Deny,
    ]
    .into_iter()
    .find_map(|action| match_semantic_prompt(&capture, session, &action))
    .ok_or_else(|| "no recognized complete provider actionable prompt".to_string())
}

pub fn probe_actionable_prompt(session: &AgentSession) -> Result<PromptEvidence, String> {
    probe_actionable_prompt_with(session, &TmuxGuardedBackend)
}

pub fn execute_guarded_action(
    session: &AgentSession,
    action: TerminalSessionAction,
) -> Result<TerminalActionOutcome, String> {
    execute_guarded_action_with(session, action, &TmuxGuardedBackend)
}

pub fn execute_guarded_action_classified(
    session: &AgentSession,
    action: TerminalSessionAction,
) -> Result<TerminalActionOutcome, GuardedActionFailure> {
    execute_guarded_action_classified_with(session, action, &TmuxGuardedBackend)
}

fn refresh_approval_observation_with(
    io: &impl ApprovalIo,
    session: &mut AgentSession,
    checked_at_ms: u64,
) {
    session.approval_checked_at_ms = checked_at_ms;
    if !session.is_shell_permission_request() {
        session.approval = ApprovalObservation::NotChecked;
        return;
    }
    let observation = match io.capture(session) {
        Ok(capture) => match match_approval_prompt(&capture, session) {
            Some(evidence) => ApprovalObservation::Confirmed(evidence),
            None => ApprovalObservation::Unknown("no matching Codex approval prompt".into()),
        },
        Err(error) => ApprovalObservation::Unknown(error),
    };
    session.approval = observation;
}

#[allow(dead_code)]
pub(crate) fn refresh_approval_observation(session: &mut AgentSession) {
    let checked_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    refresh_approval_observation_with(&RealApprovalIo, session, checked_at_ms);
}

fn approve_shell_permission_with(
    io: &impl ApprovalIo,
    session: &AgentSession,
) -> Result<(), String> {
    let ApprovalObservation::Confirmed(expected) = &session.approval else {
        return Err("approval is not terminal-confirmed".into());
    };
    if !session.is_shell_permission_request() {
        return Err("shell call is no longer pending".into());
    }
    let capture = io.capture(session)?;
    let current = match_approval_prompt(&capture, session)
        .ok_or_else(|| "approval prompt changed or disappeared".to_string())?;
    if &current != expected {
        return Err("approval identity changed; action cancelled".into());
    }
    io.send_enter(session, expected.backend.clone(), expected.target.as_str())
}

#[allow(dead_code)]
pub(crate) fn approve_shell_permission(session: &AgentSession) -> Result<(), String> {
    approve_shell_permission_with(&RealApprovalIo, session)
}

#[cfg(target_os = "macos")]
pub(crate) fn run_osascript(script: &str) -> Result<(), String> {
    let output = std::process::Command::new("osascript")
        .args(["-e", script])
        .output()
        .map_err(|e| format!("Failed to run osascript: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("AppleScript error: {}", stderr.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::RawAgentSession;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct FakeApprovalIo {
        captures: std::sync::Mutex<VecDeque<Result<PaneCapture, String>>>,
        sends: AtomicUsize,
    }

    impl FakeApprovalIo {
        fn with_captures(captures: impl IntoIterator<Item = Result<PaneCapture, String>>) -> Self {
            Self {
                captures: std::sync::Mutex::new(captures.into_iter().collect()),
                sends: AtomicUsize::new(0),
            }
        }
    }

    fn capture(text: &str) -> PaneCapture {
        PaneCapture {
            backend: Terminal::Tmux,
            target: "test-pane".into(),
            text: text.into(),
        }
    }

    fn pending_shell_session(call_id: &str, command: &str) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider: crate::provider::AgentProvider::Codex,
            pid: 7,
            process_start_identity: None,
            session_id: "session-7".into(),
            cwd: "/repo".into(),
            started_at: 0,
        });
        session.tty = "pts/7".into();
        session.pending_tool_name = Some("exec_command".into());
        session.pending_tool_call_id = Some(call_id.into());
        session.pending_tool_input = Some(command.into());
        session
    }

    fn pending_exec_wrapper_session(call_id: &str, input: &str) -> AgentSession {
        let mut session = pending_shell_session(call_id, input);
        session.pending_tool_name = Some("exec".into());
        session
    }

    impl ApprovalIo for FakeApprovalIo {
        fn capture(&self, _session: &AgentSession) -> Result<PaneCapture, String> {
            self.captures.lock().unwrap().pop_front().unwrap()
        }

        fn send_enter(
            &self,
            _session: &AgentSession,
            _backend: Terminal,
            _target: &str,
        ) -> Result<(), String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeGuardedBackend {
        targets: std::sync::Mutex<VecDeque<Result<String, String>>>,
        captures: std::sync::Mutex<VecDeque<Result<PaneCapture, String>>>,
        literals: std::sync::Mutex<Vec<String>>,
        keys: std::sync::Mutex<Vec<Vec<String>>>,
        literal_error: Option<String>,
        keys_error: Option<String>,
    }

    impl FakeGuardedBackend {
        fn with_captures(captures: impl IntoIterator<Item = Result<PaneCapture, String>>) -> Self {
            Self {
                targets: std::sync::Mutex::new(
                    [Ok("test-pane".into()), Ok("test-pane".into())]
                        .into_iter()
                        .collect(),
                ),
                captures: std::sync::Mutex::new(captures.into_iter().collect()),
                ..Self::default()
            }
        }

        fn with_target_error(error: &str) -> Self {
            Self {
                targets: std::sync::Mutex::new([Err(error.into())].into_iter().collect()),
                ..Self::default()
            }
        }
    }

    impl GuardedTerminalBackend for FakeGuardedBackend {
        fn resolve_exact_target(&self, _session: &AgentSession) -> Result<String, String> {
            self.targets.lock().unwrap().pop_front().unwrap()
        }

        fn capture(&self, _target: &str) -> Result<PaneCapture, String> {
            self.captures.lock().unwrap().pop_front().unwrap()
        }

        fn send_literal(&self, _target: &str, text: &str) -> Result<(), String> {
            self.literals.lock().unwrap().push(text.into());
            self.literal_error.clone().map_or(Ok(()), Err)
        }

        fn send_keys(&self, _target: &str, keys: &[&str]) -> Result<(), String> {
            self.keys
                .lock()
                .unwrap()
                .push(keys.iter().map(|key| (*key).into()).collect());
            self.keys_error.clone().map_or(Ok(()), Err)
        }
    }

    fn guarded_session(provider: crate::provider::AgentProvider) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider,
            pid: 7,
            process_start_identity: Some(99),
            session_id: "session-7".into(),
            cwd: "/repo".into(),
            started_at: 0,
        });
        session.tty = "pts/7".into();
        session.pending_tool_call_id = Some("tool-42".into());
        session
    }

    fn guarded_capture(text: &str) -> PaneCapture {
        PaneCapture {
            backend: Terminal::Tmux,
            target: "test-pane".into(),
            text: text.into(),
        }
    }

    #[test]
    fn guarded_semantic_continue_requires_stable_prompt_and_post_action_advancement() {
        let fixture = include_str!("../../../../tests/fixtures/claude-recovery-pane.txt");
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture("Claude is working")),
        ]);

        let result = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            TerminalSessionAction::Continue,
            &backend,
        )
        .unwrap();

        assert_eq!(result.action, TerminalSessionAction::Continue);
        assert!(result.prompt_cleared);
        assert_eq!(
            backend.keys.lock().unwrap().as_slice(),
            [vec!["1", "Enter"]]
        );
    }

    #[test]
    fn guarded_codex_continue_requires_exact_idle_composer_and_sends_fixed_literal() {
        let fixture = include_str!("../../../../tests/fixtures/codex-recovery-composer-pane.txt");
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(include_str!(
                "../../../../tests/fixtures/codex-active-composer-pane.txt"
            ))),
        ]);

        let outcome = execute_guarded_action_classified_with(
            &guarded_session(crate::provider::AgentProvider::Codex),
            TerminalSessionAction::Continue,
            &backend,
        )
        .unwrap();

        assert_eq!(outcome.action, TerminalSessionAction::Continue);
        assert_eq!(backend.literals.lock().unwrap().as_slice(), ["continue"]);
        assert_eq!(backend.keys.lock().unwrap().as_slice(), [vec!["Enter"]]);
    }

    #[test]
    fn guarded_codex_continue_rejects_active_or_unchanged_composer() {
        let fixture = include_str!("../../../../tests/fixtures/codex-recovery-composer-pane.txt");
        let active = include_str!("../../../../tests/fixtures/codex-active-composer-pane.txt");
        let active_backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(active))]);
        assert!(matches!(
            execute_guarded_action_classified_with(
                &guarded_session(crate::provider::AgentProvider::Codex),
                TerminalSessionAction::Continue,
                &active_backend,
            ),
            Err(GuardedActionFailure::NotSent(_))
        ));
        assert!(active_backend.literals.lock().unwrap().is_empty());

        let unchanged_backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
        ]);
        assert!(matches!(
            execute_guarded_action_classified_with(
                &guarded_session(crate::provider::AgentProvider::Codex),
                TerminalSessionAction::Continue,
                &unchanged_backend,
            ),
            Err(GuardedActionFailure::DeliveryUnknown(_))
        ));
    }

    #[test]
    fn guarded_codex_continue_treats_incomplete_post_capture_as_unknown() {
        let fixture = include_str!("../../../../tests/fixtures/codex-recovery-composer-pane.txt");
        for incomplete in [
            "• Prior response\n› Ask Codex to do anything\n? for shortcuts",
            "• Prior response\n› continue\n? for shortcuts 100% context left",
            "• Prior response\n› Ask Codex to do anything\n? for shortcuts 101% context left",
        ] {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(incomplete)),
            ]);

            assert!(matches!(
                execute_guarded_action_classified_with(
                    &guarded_session(crate::provider::AgentProvider::Codex),
                    TerminalSessionAction::Continue,
                    &backend,
                ),
                Err(GuardedActionFailure::DeliveryUnknown(_))
            ));
            assert_eq!(backend.literals.lock().unwrap().as_slice(), ["continue"]);
            assert_eq!(backend.keys.lock().unwrap().as_slice(), [vec!["Enter"]]);
        }
    }

    #[test]
    fn recovery_probe_returns_only_recognized_provider_fingerprint_without_sending() {
        let fixture = include_str!("../../../../tests/fixtures/claude-recovery-pane.txt");
        let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(fixture))]);

        let evidence = probe_recovery_prompt_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            &backend,
        )
        .unwrap();

        assert_eq!(evidence.provider, crate::provider::AgentProvider::Claude);
        assert_eq!(evidence.action, TerminalSessionAction::Continue);
        assert_ne!(evidence.fingerprint, 0);
        assert!(backend.keys.lock().unwrap().is_empty());
        assert!(backend.literals.lock().unwrap().is_empty());
    }

    #[test]
    fn recovery_probe_rejects_idle_or_unrecognized_terminal_text() {
        let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture("Claude is idle"))]);
        let error = probe_recovery_prompt_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            &backend,
        )
        .unwrap_err();

        assert!(error.contains("recognized"));
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn actionable_probe_recognizes_permission_without_sending() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(fixture))]);

        let evidence = probe_actionable_prompt_with(
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            &backend,
        )
        .unwrap();

        assert_eq!(evidence.action, TerminalSessionAction::Allow);
        assert!(backend.keys.lock().unwrap().is_empty());
        assert!(backend.literals.lock().unwrap().is_empty());
    }

    #[test]
    fn guarded_antigravity_allow_requires_matching_tool_identity() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        let changed = fixture.replace("tool-42", "tool-99");
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(&changed)),
        ]);

        let error = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            TerminalSessionAction::Allow,
            &backend,
        )
        .unwrap_err();

        assert!(error.contains("prompt changed"));
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn guarded_antigravity_permission_actions_send_only_fixed_semantic_keys() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        for (action, expected_keys) in [
            (TerminalSessionAction::Allow, vec!["1", "Enter"]),
            (TerminalSessionAction::Deny, vec!["2", "Enter"]),
        ] {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture("Antigravity is working")),
            ]);

            let outcome = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                action.clone(),
                &backend,
            )
            .unwrap();

            assert_eq!(outcome.action, action);
            assert_eq!(backend.keys.lock().unwrap().as_slice(), [expected_keys]);
            assert!(backend.literals.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_automatic_action_rejects_unknown_or_incomplete_prompt() {
        for text in [
            "unknown terminal contents",
            "Antigravity wants to run a tool.\n  1. Allow\n",
        ] {
            let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(text))]);
            let error = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Deny,
                &backend,
            )
            .unwrap_err();
            assert!(error.contains("recognized"));
            assert!(backend.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_automatic_action_rejects_backend_capture_beyond_bounds() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        for pane in [
            format!("{}\n{fixture}", "x".repeat(MAX_CAPTURE_BYTES)),
            format!("{}\n{fixture}", "log\n".repeat(CAPTURE_LINES)),
        ] {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(&pane)),
                Ok(guarded_capture(&pane)),
                Ok(guarded_capture("Antigravity is working")),
            ]);

            let result = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Allow,
                &backend,
            );

            assert!(result.is_err());
            assert!(backend.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_newer_incomplete_provider_prompt_blocks_older_complete_prompt() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        let pane = format!("{fixture}\n\nAntigravity wants to run a tool.\nTool use ID: tool-99\n");
        let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(&pane))]);

        let error = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            TerminalSessionAction::Allow,
            &backend,
        )
        .unwrap_err();

        assert!(error.contains("recognized"));
        assert!(backend.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn guarded_prompt_grammar_rejects_logged_quoted_out_of_order_and_stale_text() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        let invalid = [
            fixture.replacen(
                "Antigravity wants to run a tool.",
                "log: Antigravity wants to run a tool.",
                1,
            ),
            fixture.replacen(
                "Antigravity wants to run a tool.",
                "\"Antigravity wants to run a tool.\"",
                1,
            ),
            fixture.replace("  1. Allow\n  2. Deny", "  2. Deny\n  1. Allow"),
            format!("{fixture}\nterminal log: prior prompt copied"),
            fixture
                .lines()
                .map(|line| format!("│{line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ];
        for pane in invalid {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(&pane)),
                Ok(guarded_capture(&pane)),
                Ok(guarded_capture("Antigravity is working")),
            ]);

            let result = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Allow,
                &backend,
            );

            assert!(result.is_err(), "invalid prompt authorized: {pane:?}");
            assert!(backend.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_antigravity_permission_requires_nonempty_exact_tool_identity() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt")
            .replace("Tool use ID: tool-42", "Tool use ID:");
        for pending_tool_call_id in [Some("tool-42".to_string()), None] {
            let mut session = guarded_session(crate::provider::AgentProvider::Antigravity);
            session.pending_tool_call_id = pending_tool_call_id;
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(&fixture)),
                Ok(guarded_capture(&fixture)),
                Ok(guarded_capture("Antigravity is working")),
            ]);

            let result =
                execute_guarded_action_with(&session, TerminalSessionAction::Allow, &backend);

            assert!(result.is_err());
            assert!(backend.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_antigravity_recovery_does_not_invent_tool_identity() {
        let capture = guarded_capture(include_str!(
            "../../../../tests/fixtures/antigravity-recovery-pane.txt"
        ));

        let evidence = match_semantic_prompt(
            &capture,
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            &TerminalSessionAction::Continue,
        )
        .unwrap();

        assert_eq!(evidence.tool_use_id, None);
    }

    #[test]
    fn guarded_ambiguous_or_expired_target_never_sends_input() {
        for error in [
            "multiple tmux panes matched the live process",
            "process start identity changed",
        ] {
            let backend = FakeGuardedBackend::with_target_error(error);
            let result = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Continue,
                &backend,
            );
            assert!(result.is_err());
            assert!(backend.keys.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_unchanged_post_action_prompt_is_not_success() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-recovery-pane.txt");
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
        ]);

        let error = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            TerminalSessionAction::Continue,
            &backend,
        )
        .unwrap_err();

        assert!(error.contains("did not advance"));
    }

    #[test]
    fn guarded_delivery_classifies_send_failure_and_post_send_uncertainty() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-recovery-pane.txt");
        let send_failure = FakeGuardedBackend {
            targets: std::sync::Mutex::new(
                [Ok("test-pane".into()), Ok("test-pane".into())]
                    .into_iter()
                    .collect(),
            ),
            captures: std::sync::Mutex::new(
                [Ok(guarded_capture(fixture)), Ok(guarded_capture(fixture))]
                    .into_iter()
                    .collect(),
            ),
            keys_error: Some("send failed".into()),
            ..FakeGuardedBackend::default()
        };
        assert!(matches!(
            execute_guarded_action_classified_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Continue,
                &send_failure,
            ),
            Err(GuardedActionFailure::NotSent(_))
        ));

        let uncertain = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Err("capture failed after send".into()),
        ]);
        assert!(matches!(
            execute_guarded_action_classified_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Continue,
                &uncertain,
            ),
            Err(GuardedActionFailure::DeliveryUnknown(_))
        ));
        assert_eq!(
            uncertain.keys.lock().unwrap().as_slice(),
            [vec!["1", "Enter"]]
        );
    }

    #[test]
    fn guarded_postverify_rejects_original_prompt_with_noise_or_newer_incomplete_candidate() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        for post in [
            format!("{fixture}\nterminal log: incidental output"),
            format!("{fixture}\n\nAntigravity wants to run a tool.\nTool use ID: tool-99\n"),
        ] {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(&post)),
            ]);

            let error = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Allow,
                &backend,
            )
            .unwrap_err();

            assert!(error.contains("did not advance"));
        }
    }

    #[test]
    fn guarded_postverify_rejects_prompt_with_mutated_question_anchor() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        for post in [
            fixture.replace(
                "Antigravity wants to run a tool.",
                "Antigravity wants to run a tool!",
            ),
            fixture.replace(
                "Antigravity wants to run a tool.",
                "Antigravity requests permission to run a tool.",
            ),
        ] {
            let backend = FakeGuardedBackend::with_captures([
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(fixture)),
                Ok(guarded_capture(&post)),
            ]);

            let error = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Antigravity),
                TerminalSessionAction::Allow,
                &backend,
            )
            .unwrap_err();

            assert!(error.contains("did not advance"), "post capture: {post:?}");
        }
    }

    #[test]
    fn guarded_postverify_accepts_only_a_distinct_complete_tool_prompt_identity() {
        let fixture = include_str!("../../../../tests/fixtures/antigravity-permission-pane.txt");
        let next = fixture.replace("tool-42", "tool-99");
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(fixture)),
            Ok(guarded_capture(&next)),
        ]);

        let outcome = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Antigravity),
            TerminalSessionAction::Allow,
            &backend,
        )
        .unwrap();

        assert!(outcome.prompt_cleared);
    }

    #[test]
    fn guarded_manual_text_is_literal_redacted_and_requires_post_advancement() {
        let secret = "C-c secret-token-42";
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture("arbitrary unrecognized prompt")),
            Ok(guarded_capture("agent resumed")),
        ]);

        let outcome = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            TerminalSessionAction::Text(secret.into()),
            &backend,
        )
        .unwrap();

        assert_eq!(backend.literals.lock().unwrap().as_slice(), [secret]);
        assert_eq!(backend.keys.lock().unwrap().as_slice(), [vec!["Enter"]]);
        assert_eq!(outcome.action, TerminalSessionAction::Text(String::new()));
        assert!(!format!("{outcome:?}").contains(secret));
    }

    #[test]
    fn guarded_manual_text_rejects_multiline_control_and_oversized_input() {
        for text in [
            "line one\nline two".to_string(),
            "contains\u{7f}control".to_string(),
            "x".repeat(4097),
        ] {
            let backend = FakeGuardedBackend::default();
            let error = execute_guarded_action_with(
                &guarded_session(crate::provider::AgentProvider::Claude),
                TerminalSessionAction::Text(text),
                &backend,
            )
            .unwrap_err();
            assert!(error.contains("manual text"));
            assert!(backend.literals.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn guarded_manual_text_is_absent_from_send_errors() {
        let secret = "secret-token-42";
        let backend = FakeGuardedBackend {
            targets: std::sync::Mutex::new(
                [Ok("test-pane".into()), Ok("test-pane".into())]
                    .into_iter()
                    .collect(),
            ),
            captures: std::sync::Mutex::new([Ok(guarded_capture("prompt"))].into_iter().collect()),
            literal_error: Some(format!("failed to send {secret}")),
            ..FakeGuardedBackend::default()
        };

        let error = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            TerminalSessionAction::Text(secret.into()),
            &backend,
        )
        .unwrap_err();

        assert!(!error.contains(secret));
    }

    #[test]
    fn guarded_manual_text_rejects_changed_capture_backend() {
        let backend = FakeGuardedBackend::with_captures([
            Ok(guarded_capture("prompt")),
            Ok(PaneCapture {
                backend: Terminal::Kitty,
                target: "test-pane".into(),
                text: "advanced".into(),
            }),
        ]);

        let error = execute_guarded_action_with(
            &guarded_session(crate::provider::AgentProvider::Claude),
            TerminalSessionAction::Text("continue".into()),
            &backend,
        )
        .unwrap_err();

        assert!(error.contains("backend changed"));
    }

    #[test]
    fn exact_shell_approval_is_confirmed() {
        let mut session = pending_shell_session("call-7", "cargo test");
        let io = FakeApprovalIo::with_captures([Ok(capture(include_str!(
            "../../../../tests/fixtures/codex-shell-approval-pane.txt"
        )))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        assert!(matches!(
            session.approval,
            ApprovalObservation::Confirmed(_)
        ));
    }

    #[test]
    fn exec_wrapper_uses_last_complete_visible_prompt() {
        let earlier = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt")
            .replace("$ cargo test", "$ cargo clippy");
        let current = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
        let pane = format!("{earlier}\n\n{current}");
        let wrapper = "const args = next(); await tools.exec_command(args);";
        let mut session = pending_exec_wrapper_session("call-7", wrapper);
        let io = FakeApprovalIo::with_captures([Ok(capture(&pane))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        let ApprovalObservation::Confirmed(evidence) = &session.approval else {
            panic!("wrapper approval was not confirmed");
        };
        assert_eq!(evidence.tool, "exec_command");
        assert_eq!(evidence.command, "cargo test");
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(session.pending_tool_input.as_deref(), Some(wrapper));
    }

    #[test]
    fn exec_wrapper_confirms_sequential_prompts_without_rewriting_transcript_identity() {
        let fixture = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
        let clippy = fixture.replace("$ cargo test", "$ cargo clippy");
        let wrapper = "const args = next(); await tools.exec_command(args);";
        let mut session = pending_exec_wrapper_session("call-7", wrapper);
        let io = FakeApprovalIo::with_captures([
            Ok(capture(fixture)),
            Ok(capture(include_str!(
                "../../../../tests/fixtures/codex-running-shell-pane.txt"
            ))),
            Ok(capture(&clippy)),
            Ok(capture(&clippy)),
        ]);

        refresh_approval_observation_with(&io, &mut session, 10_000);
        let ApprovalObservation::Confirmed(first) = &session.approval else {
            panic!("first wrapper approval was not confirmed");
        };
        assert_eq!(first.command, "cargo test");
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(session.pending_tool_input.as_deref(), Some(wrapper));

        refresh_approval_observation_with(&io, &mut session, 11_000);
        assert!(matches!(session.approval, ApprovalObservation::Unknown(_)));
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(session.pending_tool_input.as_deref(), Some(wrapper));

        refresh_approval_observation_with(&io, &mut session, 12_000);
        let ApprovalObservation::Confirmed(evidence) = &session.approval else {
            panic!("second wrapper approval was not confirmed");
        };
        assert_eq!(evidence.command, "cargo clippy");
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(session.pending_tool_input.as_deref(), Some(wrapper));

        approve_shell_permission_with(&io, &session).unwrap();
        assert_eq!(io.sends.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn exec_wrapper_without_nested_shell_is_not_actionable() {
        let mut session = pending_exec_wrapper_session("call-7", "text(true);");
        let io = FakeApprovalIo::with_captures([Ok(capture(include_str!(
            "../../../../tests/fixtures/codex-shell-approval-pane.txt"
        )))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        assert!(matches!(session.approval, ApprovalObservation::NotChecked));
    }

    #[test]
    fn exec_wrapper_reads_prompt_inside_neovim_gutter() {
        let pane = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt")
            .lines()
            .enumerate()
            .map(|(index, line)| format!("{:>4} # │{line} │ │", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let mut session =
            pending_exec_wrapper_session("call-7", "await tools.exec_command(runtime_args);");
        let io = FakeApprovalIo::with_captures([Ok(capture(&pane))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        let ApprovalObservation::Confirmed(evidence) = &session.approval else {
            panic!("guttered wrapper approval was not confirmed");
        };
        assert_eq!(evidence.command, "cargo test");
    }

    #[test]
    fn newer_incomplete_prompt_blocks_older_complete_prompt() {
        let current = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
        let pane = format!(
            "{current}\n\nWould you like to run the following command?\n\n$ cargo clippy\n"
        );
        let wrapper = "await tools.exec_command(runtime_args);";
        let mut session = pending_exec_wrapper_session("call-7", wrapper);
        let io = FakeApprovalIo::with_captures([Ok(capture(&pane))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        assert!(matches!(session.approval, ApprovalObservation::Unknown(_)));
        assert_eq!(session.pending_tool_name.as_deref(), Some("exec"));
        assert_eq!(session.pending_tool_input.as_deref(), Some(wrapper));
    }

    #[test]
    fn running_and_lookalike_panes_are_not_confirmed() {
        for pane in [
            include_str!("../../../../tests/fixtures/codex-running-shell-pane.txt"),
            include_str!("../../../../tests/fixtures/codex-approval-lookalike-pane.txt"),
        ] {
            let mut session = pending_shell_session("call-7", "cargo test");
            let io = FakeApprovalIo::with_captures([Ok(capture(pane))]);

            refresh_approval_observation_with(&io, &mut session, 10_000);

            assert!(matches!(session.approval, ApprovalObservation::Unknown(_)));
            assert_eq!(io.sends.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn command_mismatch_is_not_confirmed() {
        let mut session = pending_shell_session("call-7", "cargo clippy");
        let io = FakeApprovalIo::with_captures([Ok(capture(include_str!(
            "../../../../tests/fixtures/codex-shell-approval-pane.txt"
        )))]);

        refresh_approval_observation_with(&io, &mut session, 10_000);

        assert!(matches!(session.approval, ApprovalObservation::Unknown(_)));
    }

    #[test]
    fn command_prefix_or_superstring_is_not_confirmed() {
        let fixture = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
        for (pending, displayed) in [
            ("cargo test", "cargo test --all"),
            ("cargo test", "cargo test-danger"),
            ("cargo test --all", "cargo test"),
        ] {
            let pane = fixture.replacen("$ cargo test", &format!("$ {displayed}"), 1);
            let mut session = pending_shell_session("call-7", pending);
            let io = FakeApprovalIo::with_captures([Ok(capture(&pane))]);

            refresh_approval_observation_with(&io, &mut session, 10_000);

            assert!(matches!(session.approval, ApprovalObservation::Unknown(_)));
            assert_eq!(io.sends.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn stale_prompt_never_sends_enter() {
        let mut session = pending_shell_session("call-7", "cargo test");
        let io = FakeApprovalIo::with_captures([
            Ok(capture(include_str!(
                "../../../../tests/fixtures/codex-shell-approval-pane.txt"
            ))),
            Ok(capture(include_str!(
                "../../../../tests/fixtures/codex-running-shell-pane.txt"
            ))),
        ]);
        refresh_approval_observation_with(&io, &mut session, 10_000);
        assert!(matches!(
            session.approval,
            ApprovalObservation::Confirmed(_)
        ));

        let error = approve_shell_permission_with(&io, &session).unwrap_err();

        assert!(error.contains("approval prompt changed"));
        assert_eq!(io.sends.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn changed_backend_or_target_never_sends_enter() {
        let pane = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
        let mut session = pending_shell_session("call-7", "cargo test");
        let io = FakeApprovalIo::with_captures([
            Ok(capture(pane)),
            Ok(PaneCapture {
                backend: Terminal::Kitty,
                target: "pid:99".into(),
                text: pane.into(),
            }),
        ]);
        refresh_approval_observation_with(&io, &mut session, 10_000);

        let error = approve_shell_permission_with(&io, &session).unwrap_err();

        assert!(error.contains("identity changed"));
        assert_eq!(io.sends.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn request_user_input_never_captures_or_sends_enter() {
        let mut session = pending_shell_session("question-1", "Continue?");
        session.pending_tool_name = Some("request_user_input".into());
        session.approval = ApprovalObservation::Confirmed(ApprovalEvidence {
            session_id: session.session_id.clone(),
            tty: session.tty.clone(),
            call_id: "question-1".into(),
            tool: "request_user_input".into(),
            command: "Continue?".into(),
            backend: Terminal::Tmux,
            target: "test-pane".into(),
            prompt_pattern_version: 1,
            prompt_fingerprint: 1,
        });
        let io = FakeApprovalIo::default();

        let error = approve_shell_permission_with(&io, &session).unwrap_err();

        assert!(error.contains("no longer pending"));
        assert_eq!(io.sends.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bounded_command_runner_handles_success_and_non_zero_exit() {
        let success = run_bounded(Command::new("sh").args(["-c", "printf ok"])).unwrap();
        assert!(success.status.success());
        assert_eq!(success.stdout, b"ok");

        let failure = run_bounded(Command::new("sh").args(["-c", "exit 7"])).unwrap();
        assert_eq!(failure.status.code(), Some(7));
    }

    #[test]
    fn bounded_command_runner_times_out() {
        let error = run_bounded(Command::new("sh").args(["-c", "sleep 2"])).unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[test]
    fn bounded_command_runner_does_not_wait_for_inherited_pipe() {
        let started = std::time::Instant::now();
        let error = run_bounded(Command::new("sh").args(["-c", "sleep 2 &"])).unwrap_err();

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_command_runner_rejects_oversized_output() {
        let error = run_bounded(Command::new("sh").args([
            "-c",
            "i=0; while [ $i -lt 70000 ]; do printf x; i=$((i + 1)); done",
        ]))
        .unwrap_err();
        assert!(error.contains("exceeded 64 KiB"));
    }

    #[cfg(unix)]
    fn process_exists(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    fn assert_descendant_cleaned(pid_file: &std::path::Path) {
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let started = std::time::Instant::now();
        while process_exists(pid) && started.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let was_cleaned = !process_exists(pid);
        if !was_cleaned {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            was_cleaned,
            "descendant {pid} survived bounded command cleanup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn guarded_bounded_timeout_cleans_descendant_process_group_before_return() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("timeout-child.pid");
        let error = run_bounded(Command::new("sh").args([
            "-c",
            "sleep 30 & child=$!; printf '%s' \"$child\" > \"$1\"; wait",
            "sh",
            pid_file.to_str().unwrap(),
        ]))
        .unwrap_err();

        assert!(error.contains("timed out"));
        assert_descendant_cleaned(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn guarded_bounded_reader_timeout_cleans_pipe_holding_descendant_before_return() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("reader-child.pid");
        let error = run_bounded(Command::new("sh").args([
            "-c",
            "sleep 30 & child=$!; printf '%s' \"$child\" > \"$1\"; exit 0",
            "sh",
            pid_file.to_str().unwrap(),
        ]))
        .unwrap_err();

        assert!(error.contains("timed out"));
        assert_descendant_cleaned(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn guarded_bounded_oversize_cleans_detached_descendant_before_return() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("oversize-child.pid");
        let error = run_bounded(Command::new("sh").args([
            "-c",
            "sleep 30 >/dev/null 2>&1 & child=$!; printf '%s' \"$child\" > \"$1\"; i=0; while [ $i -lt 70000 ]; do printf x; i=$((i + 1)); done",
            "sh",
            pid_file.to_str().unwrap(),
        ]))
        .unwrap_err();

        assert!(error.contains("exceeded 64 KiB"));
        assert_descendant_cleaned(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn guarded_bounded_sets_id_descendant_cannot_hold_output_readers_open() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("setsid-child.pid");
        let helper = std::env::current_exe().unwrap();
        let helper_test = "terminals::tests::guarded_bounded_sets_id_output_holder_helper";
        let started = std::time::Instant::now();
        let error = run_bounded(
            Command::new("sh")
                .args([
                    "-c",
                    "\"$1\" --ignored --exact \"$2\" --nocapture & wait",
                    "fixture",
                ])
                .arg(helper)
                .arg(helper_test)
                .env("CODING_BRAIN_SETSID_OUTPUT_PID", &pid_file),
        )
        .unwrap_err();

        assert!(error.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_descendant_cleaned(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess helper selected explicitly by setsid regression"]
    fn guarded_bounded_sets_id_output_holder_helper() {
        use std::io::Write;

        let Some(pid_file) = std::env::var_os("CODING_BRAIN_SETSID_OUTPUT_PID") else {
            return;
        };
        assert_ne!(unsafe { libc::setsid() }, -1);
        std::fs::write(pid_file, std::process::id().to_string()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let mut stdout = stdout.lock();
        let mut stderr = stderr.lock();
        while std::time::Instant::now() < deadline {
            if stdout.write_all(b"x").is_err()
                || stdout.flush().is_err()
                || stderr.write_all(b"y").is_err()
                || stderr.flush().is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn help_summary_lists_kitty_actions() {
        let summary = help_capability_summary_for(&Terminal::Kitty);
        assert_eq!(
            summary,
            "Current terminal: Kitty (launch, switch, input, approve)"
        );
    }

    #[test]
    fn help_summary_marks_unknown_terminal_monitor_only() {
        let summary = help_capability_summary_for(&Terminal::Unknown("foot".into()));
        assert_eq!(summary, "Current terminal: foot (monitor-only)");
    }

    #[test]
    fn help_summary_mentions_gnome_terminal() {
        let summary = help_capability_summary_for(&Terminal::Gnome);
        assert!(summary.starts_with("Current terminal: GNOME Terminal"));
    }

    #[test]
    fn help_summary_lists_windows_terminal_launch() {
        let summary = help_capability_summary_for(&Terminal::WindowsTerm);
        assert_eq!(summary, "Current terminal: Windows Terminal (launch)");
    }

    #[test]
    fn doctor_report_for_unknown_terminal_marks_actions_unsupported() {
        let report = doctor_report_for(Terminal::Unknown("foot".into()));
        assert_eq!(report.actions.len(), 4);
        assert!(
            report
                .actions
                .iter()
                .all(|action| action.status == DoctorStatus::Unsupported)
        );
    }

    #[test]
    fn platform_label_marks_wsl_explicitly() {
        assert_eq!(platform_label("linux", true), "linux (WSL)");
        assert_eq!(platform_label("macos", false), "macos");
    }

    #[test]
    fn environment_notes_describe_wsl_interop_state() {
        let notes = environment_notes(true, true);
        assert!(notes.iter().any(|note| note.contains("WSL detected")));
        assert!(notes.iter().any(|note| note.contains("cmd.exe /c wt.exe")));
    }

    #[test]
    fn wsl_interop_check_reports_when_available() {
        let check = wsl_interop_check(true).unwrap();
        assert_eq!(check.name, "Windows Terminal interop");
    }

    // Native env var detection tests.
    // These mutate env vars and must be serialized.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Helper: clear all terminal-related env vars, run f(), then restore.
    fn with_clean_env<F: FnOnce() -> R, R>(f: F) -> R {
        let _guard = ENV_LOCK.lock().unwrap();

        let keys = [
            "KITTY_WINDOW_ID",
            "KITTY_PID",
            "WEZTERM_EXECUTABLE",
            "GHOSTTY_RESOURCES_DIR",
            "TERM",
            "TERM_PROGRAM",
            "TMUX",
            "GNOME_TERMINAL_SERVICE",
            "GNOME_TERMINAL_SCREEN",
            "WT_SESSION",
        ];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        for key in &keys {
            unsafe { std::env::remove_var(key) };
        }

        let result = f();

        for (key, val) in saved {
            match val {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        result
    }

    #[test]
    fn detect_kitty_via_kitty_window_id() {
        with_clean_env(|| {
            unsafe { std::env::set_var("KITTY_WINDOW_ID", "49") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }

    #[test]
    fn detect_kitty_via_term_xterm_kitty() {
        with_clean_env(|| {
            unsafe { std::env::set_var("TERM", "xterm-kitty") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }

    #[test]
    fn detect_wezterm_via_wezterm_executable() {
        with_clean_env(|| {
            unsafe { std::env::set_var("WEZTERM_EXECUTABLE", "/usr/bin/wezterm") };
            assert_eq!(detect_by_native_env(), Some(Terminal::WezTerm));
        });
    }

    #[test]
    fn detect_ghostty_via_ghostty_resources_dir() {
        with_clean_env(|| {
            unsafe { std::env::set_var("GHOSTTY_RESOURCES_DIR", "/usr/share/ghostty") };
            assert_eq!(detect_by_native_env(), Some(Terminal::Ghostty));
        });
    }

    #[test]
    fn detect_native_env_returns_none_when_clean() {
        with_clean_env(|| {
            assert_eq!(detect_by_native_env(), None);
        });
    }

    #[test]
    fn kitty_window_id_takes_priority_over_term_xterm_kitty() {
        // Both set — KITTY_WINDOW_ID should match first (stronger signal)
        with_clean_env(|| {
            unsafe {
                std::env::set_var("KITTY_WINDOW_ID", "1");
                std::env::set_var("TERM", "xterm-kitty");
            }
            assert_eq!(detect_by_native_env(), Some(Terminal::Kitty));
        });
    }
}
