use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use coding_brain_core::brain_activity::ActivityOutcome;
use coding_brain_core::lifecycle::{
    LifecycleEventKind, LifecycleIdentity, LifecycleInputError, MAX_ID_BYTES, SessionStartSource,
};
use coding_brain_core::provider::{AgentProvider, LiveProcessIdentity};
use serde_json::Value;

pub(crate) mod antigravity;
pub(crate) mod claude;
pub(crate) mod codex;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum HookProvider {
    Codex,
    Claude,
    Antigravity,
}

impl From<HookProvider> for AgentProvider {
    fn from(provider: HookProvider) -> Self {
        match provider {
            HookProvider::Codex => Self::Codex,
            HookProvider::Claude => Self::Claude,
            HookProvider::Antigravity => Self::Antigravity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedLifecycleHook {
    pub identity: LifecycleIdentity,
    pub event: LifecycleEventKind,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<ActivityOutcome>,
    pub live_process: Option<LiveProcessIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderPermissionPolicy {
    PermitsBrainDecision,
    RequiresAsk,
    Denies,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionHookRequest {
    pub provider: AgentProvider,
    pub lifecycle: LifecycleIdentity,
    pub project: String,
    pub tool_name: String,
    pub command: Option<String>,
    pub tool_use_id: Option<String>,
    pub provider_policy: ProviderPermissionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookInputError {
    InvalidJson,
    UnsupportedEvent,
    Missing(&'static str),
    Empty(&'static str),
    TooLong(&'static str),
    Invalid(&'static str),
}

impl fmt::Display for HookInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => formatter.write_str("invalid JSON"),
            Self::UnsupportedEvent => formatter.write_str("unsupported lifecycle event"),
            Self::Missing(field) => write!(formatter, "missing {field}"),
            Self::Empty(field) => write!(formatter, "empty {field}"),
            Self::TooLong(field) => write!(formatter, "{field} exceeds its size limit"),
            Self::Invalid(field) => write!(formatter, "invalid {field}"),
        }
    }
}

impl From<LifecycleInputError> for HookInputError {
    fn from(error: LifecycleInputError) -> Self {
        match error {
            LifecycleInputError::InvalidJson => Self::InvalidJson,
            LifecycleInputError::UnsupportedEvent => Self::UnsupportedEvent,
            LifecycleInputError::Missing(field) => Self::Missing(field),
            LifecycleInputError::Empty(field) => Self::Empty(field),
            LifecycleInputError::TooLong(field) => Self::TooLong(field),
            LifecycleInputError::Invalid(field) => Self::Invalid(field),
        }
    }
}

pub(crate) fn parse_lifecycle(
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    raw: &[u8],
) -> Result<ParsedLifecycleHook, HookInputError> {
    match (provider, antigravity_event) {
        (AgentProvider::Codex, None) => codex::parse_lifecycle(raw),
        (AgentProvider::Claude, None) => claude::parse_lifecycle(raw),
        (AgentProvider::Antigravity, event) => antigravity::parse_lifecycle(event, raw),
        (_, Some(_)) => Err(HookInputError::Invalid("antigravity hook event")),
    }
}

pub(crate) fn parse_permission(
    provider: AgentProvider,
    antigravity_event: Option<&str>,
    raw: &[u8],
) -> Result<PermissionHookRequest, HookInputError> {
    match (provider, antigravity_event) {
        (AgentProvider::Codex, None) => codex::parse_permission(raw),
        (AgentProvider::Claude, None) => claude::parse_permission(raw),
        (AgentProvider::Antigravity, event) => antigravity::parse_permission(event, raw),
        (_, Some(_)) => Err(HookInputError::Invalid("antigravity hook event")),
    }
}

pub(super) fn permission_request(
    lifecycle: LifecycleIdentity,
    tool_name: String,
    command: Option<String>,
    tool_use_id: Option<String>,
    provider_policy: ProviderPermissionPolicy,
) -> PermissionHookRequest {
    let project = lifecycle
        .cwd()
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| lifecycle.cwd().to_string_lossy().into_owned());
    PermissionHookRequest {
        provider: lifecycle.provider(),
        lifecycle,
        project,
        tool_name,
        command,
        tool_use_id,
        provider_policy,
    }
}

pub(super) fn optional_command(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, HookInputError> {
    value
        .map(|value| {
            if value.trim().is_empty() {
                Err(HookInputError::Empty(field))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

pub(super) fn required(
    value: Option<String>,
    field: &'static str,
) -> Result<String, HookInputError> {
    let value = value.ok_or(HookInputError::Missing(field))?;
    if value.is_empty() {
        return Err(HookInputError::Empty(field));
    }
    Ok(value)
}

pub(super) fn identity(
    provider: AgentProvider,
    session_id: String,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
) -> Result<LifecycleIdentity, HookInputError> {
    if session_id.trim().is_empty() {
        return Err(HookInputError::Empty("session_id"));
    }
    if turn_id
        .as_deref()
        .is_some_and(|turn_id| turn_id.trim().is_empty())
    {
        return Err(HookInputError::Empty("turn_id"));
    }
    LifecycleIdentity::try_new(provider, session_id, turn_id, transcript_path, cwd)
        .map_err(Into::into)
}

pub(super) fn optional_id(
    value: Option<String>,
    field: &'static str,
) -> Result<Option<String>, HookInputError> {
    value
        .map(|value| {
            if value.trim().is_empty() {
                Err(HookInputError::Empty(field))
            } else if value.len() > MAX_ID_BYTES {
                Err(HookInputError::TooLong(field))
            } else {
                Ok(value)
            }
        })
        .transpose()
}

pub(super) fn require_tool_use_id(
    event: &LifecycleEventKind,
    tool_use_id: Option<String>,
    field: &'static str,
) -> Result<Option<String>, HookInputError> {
    let tool_use_id = optional_id(tool_use_id, field)?;
    if matches!(
        event,
        LifecycleEventKind::PreToolUse | LifecycleEventKind::PostToolUse
    ) && tool_use_id.is_none()
    {
        return Err(HookInputError::Missing(field));
    }
    Ok(tool_use_id)
}

pub(super) fn event_kind(
    event: &str,
    source: Option<&str>,
    agent_id: Option<String>,
) -> Result<LifecycleEventKind, HookInputError> {
    match event {
        "SessionStart" => Ok(LifecycleEventKind::SessionStart {
            source: match source.ok_or(HookInputError::Missing("source"))? {
                "startup" => SessionStartSource::Startup,
                "resume" => SessionStartSource::Resume,
                "clear" => SessionStartSource::Clear,
                "compact" => SessionStartSource::Compact,
                _ => return Err(HookInputError::Invalid("source")),
            },
        }),
        "UserPromptSubmit" => Ok(LifecycleEventKind::UserPromptSubmit),
        "PreToolUse" => Ok(LifecycleEventKind::PreToolUse),
        "PostToolUse" => Ok(LifecycleEventKind::PostToolUse),
        "SubagentStart" => Ok(LifecycleEventKind::SubagentStart {
            agent_id: required(agent_id, "agent_id")?,
        }),
        "SubagentStop" => Ok(LifecycleEventKind::SubagentStop {
            agent_id: required(agent_id, "agent_id")?,
        }),
        "Stop" => Ok(LifecycleEventKind::Stop),
        _ => Err(HookInputError::UnsupportedEvent),
    }
}

pub(super) fn normalized_outcome(response: Option<&Value>) -> ActivityOutcome {
    let Some(Value::Object(response)) = response else {
        return ActivityOutcome::Completed;
    };
    let status = response.get("status").and_then(Value::as_str);
    if response.get("cancelled").and_then(Value::as_bool) == Some(true)
        || matches!(status, Some("cancelled" | "canceled"))
    {
        ActivityOutcome::Cancelled
    } else if response.get("is_error").and_then(Value::as_bool) == Some(true)
        || response
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|code| code != 0)
        || response.get("success").and_then(Value::as_bool) == Some(false)
        || matches!(status, Some("failed" | "error"))
    {
        ActivityOutcome::Failed
    } else if response.get("exit_code").and_then(Value::as_i64) == Some(0)
        || response.get("success").and_then(Value::as_bool) == Some(true)
        || response.get("is_error").and_then(Value::as_bool) == Some(false)
        || matches!(status, Some("succeeded" | "success"))
    {
        ActivityOutcome::Succeeded
    } else {
        ActivityOutcome::Completed
    }
}

const MAX_PARENT_DEPTH: usize = 16;
const MAX_PARENT_RECORD_BYTES: usize = 4 * 1024;
#[cfg(any(all(unix, not(target_os = "linux")), all(test, unix)))]
const PARENT_PROCESS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Clone, Debug)]
struct ParentProcessEvidence {
    pid: u32,
    parent_pid: u32,
    process_start_identity: u64,
    tty: Option<String>,
    executable: String,
}

pub(crate) fn live_parent_process(provider: AgentProvider) -> Option<LiveProcessIdentity> {
    let mut pid = parent_pid()?;
    for _ in 0..MAX_PARENT_DEPTH {
        if pid == 0 {
            break;
        }
        let parent = read_parent_process(pid)?;
        if let Some(live_process) = select_live_process(provider, std::slice::from_ref(&parent)) {
            return Some(live_process);
        }
        pid = parent.parent_pid;
    }
    None
}

pub(crate) fn revalidate_live_process(live_process: &LiveProcessIdentity) -> bool {
    revalidate_live_process_with(live_process, read_parent_process)
}

fn revalidate_live_process_with(
    live_process: &LiveProcessIdentity,
    read_process: impl FnOnce(u32) -> Option<ParentProcessEvidence>,
) -> bool {
    read_process(live_process.pid)
        .and_then(|evidence| {
            select_live_process(live_process.provider, std::slice::from_ref(&evidence))
        })
        .as_ref()
        == Some(live_process)
}

fn select_live_process(
    provider: AgentProvider,
    evidence: &[ParentProcessEvidence],
) -> Option<LiveProcessIdentity> {
    let expected: &[&str] = match provider {
        AgentProvider::Codex => &["codex", ".codex-wrapped"],
        AgentProvider::Claude => &["claude"],
        AgentProvider::Antigravity => &["agy"],
    };
    evidence.iter().find_map(|parent| {
        let executable = Path::new(&parent.executable).file_name()?.to_str()?;
        if !expected.contains(&executable) {
            return None;
        }
        let tty = provider_tty(parent.tty.as_deref()?)?;
        LiveProcessIdentity::try_new(provider, parent.pid, parent.process_start_identity, tty)
    })
}

fn provider_tty(tty: &str) -> Option<&str> {
    let tty = tty.trim().strip_prefix("/dev/").unwrap_or(tty.trim());
    let valid = tty
        .strip_prefix("pts/")
        .is_some_and(|suffix| !suffix.is_empty())
        || tty
            .strip_prefix("tty")
            .is_some_and(|suffix| !suffix.is_empty());
    valid.then_some(tty)
}

#[cfg(unix)]
fn parent_pid() -> Option<u32> {
    let pid = unsafe { libc::getppid() };
    u32::try_from(pid).ok().filter(|pid| *pid != 0)
}

#[cfg(not(unix))]
fn parent_pid() -> Option<u32> {
    None
}

#[cfg(target_os = "linux")]
fn read_parent_process(pid: u32) -> Option<ParentProcessEvidence> {
    read_linux_parent_process(
        pid,
        |path| read_bounded_file(path, MAX_PARENT_RECORD_BYTES),
        |path| std::fs::read_link(path).ok(),
    )
}

#[cfg(target_os = "linux")]
fn read_linux_parent_process(
    pid: u32,
    read_stat: impl FnOnce(&Path) -> Option<Vec<u8>>,
    read_link: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<ParentProcessEvidence> {
    let proc_dir = PathBuf::from(format!("/proc/{pid}"));
    let stat = read_stat(&proc_dir.join("stat"))?;
    let stat = std::str::from_utf8(&stat).ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let parent_pid = fields.get(1)?.parse().ok()?;
    let process_start_identity = fields.get(19)?.parse().ok()?;
    let executable = proc_executable_name(read_link(&proc_dir.join("exe")))?;
    let tty = [0, 1, 2].into_iter().find_map(|fd| {
        let path = read_link(&proc_dir.join(format!("fd/{fd}")))?;
        let value = path.to_string_lossy();
        (value.starts_with("/dev/") && value.len() <= MAX_PARENT_RECORD_BYTES)
            .then(|| value.into_owned())
    });
    Some(ParentProcessEvidence {
        pid,
        parent_pid,
        process_start_identity,
        tty,
        executable,
    })
}

#[cfg(target_os = "linux")]
fn proc_executable_name(path: Option<PathBuf>) -> Option<String> {
    path?
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn read_parent_process(pid: u32) -> Option<ParentProcessEvidence> {
    use std::process::{Command, Stdio};

    let mut command = Command::new("/bin/ps");
    command
        .args(["-o", "ppid=,tty=,lstart=,comm=", "-p", &pid.to_string()])
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = run_bounded_process(
        &mut command,
        PARENT_PROCESS_TIMEOUT,
        MAX_PARENT_RECORD_BYTES,
    )?;
    let fields = std::str::from_utf8(&output)
        .ok()?
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.len() < 8 {
        return None;
    }
    let parent_pid = fields[0].parse().ok()?;
    let tty = fields[1].to_string();
    let start = fields[2..7].join(" ");
    let executable = fields[7..].join(" ");
    Some(ParentProcessEvidence {
        pid,
        parent_pid,
        process_start_identity: stable_start_identity(&start)?,
        tty: Some(tty),
        executable,
    })
}

#[cfg(any(all(unix, not(target_os = "linux")), all(test, unix)))]
fn run_bounded_process(
    command: &mut std::process::Command,
    timeout: std::time::Duration,
    output_limit: usize,
) -> Option<Vec<u8>> {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Instant;

    command.stdout(Stdio::piped()).stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().ok()?;
    let mut stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_process_group(&mut child);
            return None;
        }
    };
    let descriptor = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        terminate_process_group(&mut child);
        return None;
    }

    let started = Instant::now();
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        let mut chunk = [0_u8; 1024];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(read) => {
                    output.extend_from_slice(&chunk[..read]);
                    if output.len() > output_limit {
                        terminate_process_group(&mut child);
                        return None;
                    }
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    terminate_process_group(&mut child);
                    return None;
                }
            }
        }

        match child.try_wait() {
            Ok(Some(status)) if eof => return status.success().then_some(output),
            Ok(_) => {}
            Err(_) => {
                terminate_process_group(&mut child);
                return None;
            }
        }
        if started.elapsed() >= timeout {
            terminate_process_group(&mut child);
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(any(all(unix, not(target_os = "linux")), all(test, unix)))]
fn terminate_process_group(child: &mut std::process::Child) {
    if let Ok(process_group) = i32::try_from(child.id()) {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let started = std::time::Instant::now();
    while started.elapsed() < PARENT_PROCESS_TIMEOUT {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(not(unix))]
fn read_parent_process(_pid: u32) -> Option<ParentProcessEvidence> {
    None
}

fn read_bounded_file(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= limit).then_some(bytes)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn stable_start_identity(value: &str) -> Option<u64> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return None;
    }
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_process_requires_complete_exact_provider_parent_evidence() {
        let complete = ParentProcessEvidence {
            pid: 42,
            parent_pid: 1,
            process_start_identity: 9001,
            tty: Some("/dev/pts/7".into()),
            executable: "claude".into(),
        };
        assert_eq!(
            select_live_process(AgentProvider::Claude, std::slice::from_ref(&complete)),
            LiveProcessIdentity::try_new(AgentProvider::Claude, 42, 9001, "pts/7")
        );
        assert_eq!(
            select_live_process(AgentProvider::Codex, std::slice::from_ref(&complete)),
            None
        );
        assert_eq!(
            select_live_process(
                AgentProvider::Claude,
                &[ParentProcessEvidence {
                    tty: None,
                    ..complete.clone()
                }]
            ),
            None
        );
        assert_eq!(
            select_live_process(
                AgentProvider::Claude,
                &[ParentProcessEvidence {
                    tty: Some("/dev/null".into()),
                    ..complete.clone()
                }]
            ),
            None
        );
        assert_eq!(
            select_live_process(
                AgentProvider::Claude,
                &[ParentProcessEvidence {
                    pid: 0,
                    ..complete.clone()
                }]
            ),
            None
        );
        assert_eq!(
            select_live_process(
                AgentProvider::Claude,
                &[ParentProcessEvidence {
                    process_start_identity: 0,
                    ..complete.clone()
                }]
            ),
            None
        );

        let original =
            select_live_process(AgentProvider::Claude, std::slice::from_ref(&complete)).unwrap();
        let changed = select_live_process(
            AgentProvider::Claude,
            &[ParentProcessEvidence {
                process_start_identity: complete.process_start_identity + 1,
                ..complete
            }],
        )
        .unwrap();
        assert_ne!(original, changed);
        assert!(!original.matches(changed.pid, changed.process_start_identity, &changed.tty));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_proc_exe_never_uses_mutable_comm_as_executable_evidence() {
        let stat = b"42 (claude) S 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 9001";
        let with_exe = read_linux_parent_process(
            42,
            |_| Some(stat.to_vec()),
            |path| {
                path.ends_with("exe")
                    .then(|| PathBuf::from("/nix/store/hash/bin/claude"))
                    .or_else(|| path.ends_with("fd/0").then(|| PathBuf::from("/dev/pts/7")))
            },
        );
        assert!(with_exe.is_some(), "control evidence must parse");

        let without_exe = read_linux_parent_process(42, |_| Some(stat.to_vec()), |_| None);
        assert!(without_exe.is_none());
    }

    #[test]
    fn live_process_revalidation_rejects_exit_and_identity_changes() {
        let live = LiveProcessIdentity::try_new(AgentProvider::Claude, 42, 9001, "pts/7").unwrap();
        let exact = ParentProcessEvidence {
            pid: 42,
            parent_pid: 1,
            process_start_identity: 9001,
            tty: Some("/dev/pts/7".into()),
            executable: "claude".into(),
        };
        assert!(revalidate_live_process_with(&live, |_| Some(exact.clone())));
        assert!(!revalidate_live_process_with(&live, |_| None));
        assert!(!revalidate_live_process_with(&live, |_| {
            Some(ParentProcessEvidence {
                process_start_identity: 9002,
                ..exact.clone()
            })
        }));
        assert!(!revalidate_live_process_with(&live, |_| {
            Some(ParentProcessEvidence {
                tty: Some("/dev/pts/8".into()),
                ..exact.clone()
            })
        }));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_group_collection_times_out_and_cleans_up() {
        use std::process::Command;
        use std::time::{Duration, Instant};

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 10 & wait"]);
        let started = Instant::now();
        assert_eq!(
            run_bounded_process(&mut command, Duration::from_millis(25), 1024),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
