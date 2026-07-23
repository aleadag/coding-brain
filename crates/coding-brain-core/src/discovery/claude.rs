use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
#[cfg(not(unix))]
use std::sync::Arc;
#[cfg(not(unix))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(unix))]
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::process::ProcessSnapshotEntry;
use crate::provider::AgentProvider;
use crate::session::{AgentSession, RawAgentSession, SessionStatus};

pub const MAX_INVENTORY_BYTES: usize = 1024 * 1024;
const INVENTORY_TIMEOUT: Duration = Duration::from_secs(2);
const INVENTORY_TTL: Duration = Duration::from_secs(5);
const PROCESS_START_TOLERANCE_MS: u64 = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeInventoryEntry {
    pub provider: AgentProvider,
    pub session_id: Option<String>,
    pub attach_id: Option<String>,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub started_at: Option<u64>,
    pub status: Option<String>,
}

#[derive(Debug, Default)]
pub struct ClaudeInventoryCache {
    pub refreshed_at: Option<Instant>,
    pub last_good: Vec<ClaudeInventoryEntry>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryError {
    Unavailable(String),
    Failed,
    Timeout,
    Oversized,
    Malformed(String),
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => write!(f, "claude inventory unavailable: {error}"),
            Self::Failed => f.write_str("claude inventory command failed"),
            Self::Timeout => f.write_str("claude inventory timed out after two seconds"),
            Self::Oversized => f.write_str("claude inventory exceeded one MiB"),
            Self::Malformed(error) => write!(f, "malformed claude inventory: {error}"),
        }
    }
}

pub fn parse_inventory(bytes: &[u8]) -> Result<Vec<ClaudeInventoryEntry>, InventoryError> {
    if bytes.len() > MAX_INVENTORY_BYTES {
        return Err(InventoryError::Oversized);
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| InventoryError::Malformed(error.to_string()))?;
    let values = match &value {
        Value::Array(values) => values,
        Value::Object(object) => object
            .get("agents")
            .and_then(Value::as_array)
            .ok_or_else(|| InventoryError::Malformed("missing agents array".into()))?,
        _ => {
            return Err(InventoryError::Malformed(
                "expected an array or object".into(),
            ));
        }
    };
    let entries: Vec<_> = values.iter().filter_map(parse_inventory_entry).collect();
    if !values.is_empty() && entries.is_empty() {
        return Err(InventoryError::Malformed(
            "no supported inventory entries".into(),
        ));
    }
    Ok(entries)
}

pub fn parse_inventory_entry(value: &Value) -> Option<ClaudeInventoryEntry> {
    let object = value.as_object()?;
    let cwd = nonempty_string(object.get("cwd")?)?;
    let kind = object.get("kind").and_then(Value::as_str);
    let attach_id = matches!(kind, Some("background"))
        .then(|| object.get("id").and_then(nonempty_string))
        .flatten();

    Some(ClaudeInventoryEntry {
        provider: AgentProvider::Claude,
        session_id: object.get("sessionId").and_then(nonempty_string),
        attach_id,
        cwd: PathBuf::from(cwd),
        pid: object.get("pid").and_then(parse_pid),
        started_at: object.get("startedAt").and_then(parse_started_at),
        status: object.get("status").and_then(nonempty_string),
    })
}

pub fn inventory_with_runner<F>(
    cache: &mut ClaudeInventoryCache,
    now: Instant,
    runner: F,
) -> Vec<ClaudeInventoryEntry>
where
    F: FnOnce(Duration, usize) -> Result<Vec<u8>, InventoryError>,
{
    if cache.refreshed_at.is_some_and(|refreshed_at| {
        now.checked_duration_since(refreshed_at)
            .is_some_and(|age| age < INVENTORY_TTL)
    }) {
        return cache.last_good.clone();
    }

    let refresh = runner(INVENTORY_TIMEOUT, MAX_INVENTORY_BYTES).and_then(|output| {
        if output.len() > MAX_INVENTORY_BYTES {
            return Err(InventoryError::Oversized);
        }
        parse_inventory(&output)
    });
    cache.refreshed_at = Some(now);
    match refresh {
        Ok(entries) => {
            cache.last_good = entries;
            cache.last_error = None;
        }
        Err(error) => cache.last_error = Some(error.to_string()),
    }
    cache.last_good.clone()
}

pub(crate) fn run_inventory_command(
    timeout: Duration,
    output_cap: usize,
) -> Result<Vec<u8>, InventoryError> {
    let mut command = Command::new("claude");
    command.args(["agents", "--json"]);
    run_bounded_inventory_command(&mut command, timeout, output_cap)
}

fn run_bounded_inventory_command(
    command: &mut Command,
    timeout: Duration,
    output_cap: usize,
) -> Result<Vec<u8>, InventoryError> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    isolate_inventory_process(command);
    let mut child = command
        .spawn()
        .map_err(|error| InventoryError::Unavailable(error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| InventoryError::Unavailable("claude stdout was not captured".into()))?;
    supervise_inventory_process(&mut child, stdout, timeout, output_cap)
}

#[cfg(unix)]
fn supervise_inventory_process(
    child: &mut Child,
    mut stdout: ChildStdout,
    timeout: Duration,
    output_cap: usize,
) -> Result<Vec<u8>, InventoryError> {
    if let Err(error) = set_nonblocking(&stdout) {
        terminate_inventory_process(child);
        return Err(InventoryError::Unavailable(error.to_string()));
    }
    let deadline = Instant::now() + timeout;
    let mut output = Vec::with_capacity(output_cap.min(8 * 1024));
    let mut status = None;
    let mut stdout_open = true;

    loop {
        if stdout_open {
            match read_available_output(&mut stdout, &mut output, output_cap) {
                Ok(eof) => stdout_open = !eof,
                Err(error) => {
                    terminate_inventory_process(child);
                    return Err(error);
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(child_status) => status = child_status,
                Err(error) => {
                    terminate_inventory_process(child);
                    return Err(InventoryError::Unavailable(error.to_string()));
                }
            }
        }
        if status.is_some() && !stdout_open {
            break;
        }
        if Instant::now() >= deadline {
            terminate_inventory_process(child);
            return Err(InventoryError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    if !status.expect("status checked above").success() {
        return Err(InventoryError::Failed);
    }
    Ok(output)
}

#[cfg(unix)]
fn set_nonblocking(stdout: &ChildStdout) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let fd = stdout.as_raw_fd();
    // SAFETY: `fd` is the live stdout pipe owned by `stdout`; `fcntl` only
    // reads and updates its file status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn read_available_output(
    stdout: &mut ChildStdout,
    output: &mut Vec<u8>,
    output_cap: usize,
) -> Result<bool, InventoryError> {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match stdout.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                let remaining = output_cap.saturating_sub(output.len());
                if read > remaining {
                    return Err(InventoryError::Oversized);
                }
                output.extend_from_slice(&buffer[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(InventoryError::Unavailable(error.to_string())),
        }
    }
}

#[cfg(not(unix))]
fn supervise_inventory_process(
    child: &mut Child,
    stdout: ChildStdout,
    timeout: Duration,
    output_cap: usize,
) -> Result<Vec<u8>, InventoryError> {
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let output_reader = {
        let exceeded = output_exceeded.clone();
        thread::spawn(move || read_bounded_output(stdout, output_cap, exceeded))
    };
    let deadline = Instant::now() + timeout;

    let status = loop {
        if output_exceeded.load(Ordering::SeqCst) {
            terminate_inventory_process(&mut child);
            finish_output_reader(output_reader, deadline);
            return Err(InventoryError::Oversized);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_inventory_process(&mut child);
                finish_output_reader(output_reader, deadline);
                return Err(InventoryError::Timeout);
            }
            Err(error) => {
                terminate_inventory_process(&mut child);
                finish_output_reader(output_reader, deadline);
                return Err(InventoryError::Unavailable(error.to_string()));
            }
        }
    };

    while !output_reader.is_finished() {
        if output_exceeded.load(Ordering::SeqCst) {
            terminate_inventory_process(&mut child);
            finish_output_reader(output_reader, deadline);
            return Err(InventoryError::Oversized);
        }
        if Instant::now() >= deadline {
            terminate_inventory_process(&mut child);
            finish_output_reader(output_reader, deadline);
            return Err(InventoryError::Timeout);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = output_reader
        .join()
        .map_err(|_| InventoryError::Unavailable("claude output reader panicked".into()))?
        .map_err(|error| InventoryError::Unavailable(error.to_string()))?;
    if output.oversized {
        return Err(InventoryError::Oversized);
    }
    if !status.success() {
        return Err(InventoryError::Failed);
    }
    Ok(output.bytes)
}

#[cfg(not(unix))]
struct BoundedOutput {
    bytes: Vec<u8>,
    oversized: bool,
}

#[cfg(not(unix))]
fn read_bounded_output(
    mut reader: impl Read,
    output_cap: usize,
    exceeded_signal: Arc<AtomicBool>,
) -> io::Result<BoundedOutput> {
    let mut bytes = Vec::with_capacity(output_cap.min(8 * 1024));
    let mut oversized = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = output_cap.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(remaining)]);
        oversized |= read > remaining;
        if oversized {
            exceeded_signal.store(true, Ordering::SeqCst);
        }
    }
    Ok(BoundedOutput { bytes, oversized })
}

#[cfg(unix)]
fn isolate_inventory_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpgid` is async-signal-safe and creates a dedicated process
    // group so timeout and output-cap cleanup also terminates descendants.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn isolate_inventory_process(_command: &mut Command) {}

fn terminate_inventory_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        // SAFETY: the child is placed in a dedicated process group before
        // exec, so this cannot signal the Coding Brain process group.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn finish_output_reader(
    reader: thread::JoinHandle<io::Result<BoundedOutput>>,
    original_deadline: Instant,
) {
    let cleanup_deadline = original_deadline + Duration::from_millis(250);
    while !reader.is_finished() && Instant::now() < cleanup_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if reader.is_finished() {
        let _ = reader.join();
    }
}

pub(crate) fn sessions_from_inventory(
    inventory: &[ClaudeInventoryEntry],
    stale: bool,
    process_snapshot_succeeded: bool,
    processes: &[ProcessSnapshotEntry],
) -> Vec<AgentSession> {
    let claude_processes: Vec<_> = processes
        .iter()
        .filter(|process| process.has_executable_basename(&["claude"]))
        .collect();
    let mut used_pids = std::collections::HashSet::new();
    let mut sessions = Vec::new();

    for entry in inventory {
        let pid_process = entry
            .pid
            .and_then(|pid| claude_processes.iter().find(|process| process.pid == pid))
            .copied();
        let process = pid_process.filter(|process| {
            entry.started_at.is_some_and(|started_at| {
                started_at.abs_diff(process.started_at) <= PROCESS_START_TOLERANCE_MS
            })
        });
        if stale && process_snapshot_succeeded && entry.pid.is_some() && pid_process.is_none() {
            continue;
        }
        let session_id = entry
            .session_id
            .clone()
            .or_else(|| entry.attach_id.as_ref().map(|id| format!("attach:{id}")))
            .or_else(|| process.map(super::process_session_id));
        let Some(session_id) = session_id else {
            continue;
        };
        if let Some(process) = process {
            used_pids.insert(process.pid);
        }

        let mut session = AgentSession::from_raw(RawAgentSession {
            provider: AgentProvider::Claude,
            pid: process.map_or_else(|| entry.pid.unwrap_or_default(), |process| process.pid),
            process_start_identity: process.map(|process| process.start_identity),
            session_id,
            cwd: entry.cwd.to_string_lossy().into_owned(),
            started_at: entry
                .started_at
                .or_else(|| process.map(|process| process.started_at))
                .unwrap_or_default(),
        });
        session.process_backed = process.is_some();
        session.native_attach_id = entry.attach_id.clone();
        session.status = entry
            .status
            .as_deref()
            .map_or(SessionStatus::Unknown, map_status);
        if let Some(process) = process {
            super::apply_process_evidence(&mut session, process);
        }
        sessions.push(session);
    }

    sessions.extend(
        claude_processes
            .into_iter()
            .filter(|process| !used_pids.contains(&process.pid))
            .map(|process| super::session_from_provider_process(AgentProvider::Claude, process)),
    );
    sessions
}

fn nonempty_string(value: &Value) -> Option<String> {
    let value = value.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_pid(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|pid| *pid != 0)
}

fn parse_started_at(value: &Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(if value < 10_000_000_000 {
            value.saturating_mul(1000)
        } else {
            value
        });
    }
    let parsed = OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}

fn map_status(status: &str) -> SessionStatus {
    match status.to_ascii_lowercase().as_str() {
        "working" | "running" | "processing" => SessionStatus::Processing,
        "waiting" | "idle" => SessionStatus::WaitingInput,
        _ => SessionStatus::Unknown,
    }
}

#[cfg(all(test, unix))]
mod bounded_command_tests {
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn timeout_and_oversize_terminate_descendants_holding_stdout() {
        for (name, body, expected) in [
            (
                "timeout",
                "sleep 30 & child=$!; printf '%s' \"$child\" > \"$1\"; wait",
                InventoryError::Timeout,
            ),
            (
                "oversize",
                "sleep 30 & child=$!; printf '%s' \"$child\" > \"$1\"; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
                InventoryError::Oversized,
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let pid_path = directory.path().join(format!("{name}.pid"));
            let mut command = Command::new("sh");
            command.args(["-c", body, "fixture"]).arg(&pid_path);
            let started = Instant::now();

            let error =
                run_bounded_inventory_command(&mut command, Duration::from_millis(300), 1024)
                    .unwrap_err();

            assert_eq!(error, expected);
            assert!(started.elapsed() < Duration::from_secs(2));
            let pid = fs::read_to_string(&pid_path)
                .unwrap()
                .parse::<i32>()
                .unwrap();
            let cleanup_deadline = Instant::now() + Duration::from_secs(1);
            while process_exists(pid) && Instant::now() < cleanup_deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!process_exists(pid), "{name} descendant {pid} leaked");
        }
    }

    #[test]
    fn escaped_process_group_cannot_hold_inventory_stdout_open() {
        let directory = tempfile::tempdir().unwrap();
        let pid_path = directory.path().join("escaped.pid");
        let helper = std::env::current_exe().unwrap();
        let helper_test = "discovery::claude::bounded_command_tests::escaped_stdout_helper";
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "\"$1\" --ignored --exact \"$2\" --nocapture & wait",
                "fixture",
            ])
            .arg(helper)
            .arg(helper_test)
            .env("CODING_BRAIN_ESCAPED_STDOUT_PID", &pid_path);
        let started = Instant::now();

        let error = run_bounded_inventory_command(
            &mut command,
            Duration::from_millis(300),
            MAX_INVENTORY_BYTES,
        )
        .unwrap_err();

        assert_eq!(error, InventoryError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = fs::read_to_string(&pid_path)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let cleanup_deadline = Instant::now() + Duration::from_secs(1);
        while process_exists(pid) && Instant::now() < cleanup_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_exists(pid), "escaped stdout helper {pid} leaked");
    }

    #[test]
    #[ignore = "subprocess helper selected explicitly by escaped-process regression"]
    fn escaped_stdout_helper() {
        let Some(pid_path) = std::env::var_os("CODING_BRAIN_ESCAPED_STDOUT_PID") else {
            return;
        };
        // SAFETY: this subprocess deliberately escapes the inventory command's
        // process group to reproduce an inherited-stdout cleanup boundary.
        assert_ne!(unsafe { libc::setsid() }, -1);
        fs::write(pid_path, std::process::id().to_string()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        while Instant::now() < deadline {
            if stdout.write_all(b"x").is_err() || stdout.flush().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 only probes whether the child PID still exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }
}
