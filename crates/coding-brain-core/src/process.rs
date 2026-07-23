use std::fs;
use std::path::{Path, PathBuf};

use crate::session::{AgentSession, SessionStatus};

const PROCESS_COLUMNS: &str = "pid=,ppid=,tty=,%cpu=,rss=,etime=,lstart=,comm=,args=";

#[derive(Debug, Clone, Default)]
pub struct ProcessSnapshot {
    pub(crate) entries: Vec<ProcessSnapshotEntry>,
    pub(crate) succeeded: bool,
}

impl ProcessSnapshot {
    fn successful(entries: Vec<ProcessSnapshotEntry>) -> Self {
        Self {
            entries,
            succeeded: true,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<ProcessSnapshotEntry>) -> Self {
        Self::successful(entries)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessSnapshotEntry {
    pub(crate) pid: u32,
    pub(crate) tty: String,
    pub(crate) cpu_percent: f32,
    pub(crate) mem_mb: f64,
    pub(crate) command: String,
    pub(crate) args: String,
    pub(crate) cwd: PathBuf,
    pub(crate) started_at: u64,
    pub(crate) start_identity: u64,
}

impl ProcessSnapshotEntry {
    pub(crate) fn has_executable_basename(&self, expected: &[&str]) -> bool {
        executable_basename(&self.command).is_some_and(|name| expected.contains(&name))
            || self
                .args
                .split_whitespace()
                .next()
                .and_then(executable_basename)
                .is_some_and(|name| expected.contains(&name))
    }

    #[cfg(test)]
    pub(crate) fn fixture(
        pid: u32,
        tty: &str,
        command: &str,
        cwd: &str,
        start_identity: u64,
    ) -> Self {
        Self {
            pid,
            tty: tty.into(),
            cpu_percent: 0.0,
            mem_mb: 32.0,
            command: command.into(),
            args: command.into(),
            cwd: cwd.into(),
            started_at: start_identity,
            start_identity,
        }
    }
}

pub fn capture_process_snapshot() -> ProcessSnapshot {
    if std::env::var_os("CODEXCTL_DISABLE_PROCESS_DISCOVERY").is_some() {
        return ProcessSnapshot::default();
    }

    let mut command = std::process::Command::new("ps");
    command.args(["-eo", process_snapshot_columns()]);
    capture_process_snapshot_with(&mut command)
}

fn capture_process_snapshot_with(command: &mut std::process::Command) -> ProcessSnapshot {
    command.env_clear();
    let Ok(output) = crate::terminals::run_bounded(command) else {
        return ProcessSnapshot::default();
    };
    if !output.status.success() {
        return ProcessSnapshot::default();
    }

    let Ok(stdout) = std::str::from_utf8(&output.stdout) else {
        return ProcessSnapshot::default();
    };
    parse_process_snapshot(stdout, unix_time_ms(), process_cwd)
}

fn process_snapshot_columns() -> &'static str {
    PROCESS_COLUMNS
}

fn parse_process_snapshot<F>(ps_stdout: &str, now_ms: u64, mut cwd_resolver: F) -> ProcessSnapshot
where
    F: FnMut(u32) -> Option<PathBuf>,
{
    let entries = ps_stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| parse_process_snapshot_entry(line, now_ms, &mut cwd_resolver))
        .collect::<Result<Vec<_>, ()>>();
    match entries {
        Ok(entries) => ProcessSnapshot::successful(entries.into_iter().flatten().collect()),
        Err(()) => ProcessSnapshot::default(),
    }
}

fn parse_process_snapshot_entry<F>(
    line: &str,
    now_ms: u64,
    cwd_resolver: &mut F,
) -> Result<Option<ProcessSnapshotEntry>, ()>
where
    F: FnMut(u32) -> Option<PathBuf>,
{
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 12 {
        return Err(());
    }

    let pid = fields[0].parse::<u32>().map_err(|_| ())?;
    let tty = fields[2].to_owned();
    let cpu_percent = fields[3].parse::<f32>().unwrap_or(0.0);
    let rss_kb = fields[4].parse::<f64>().unwrap_or(0.0);
    let elapsed_secs = parse_elapsed_seconds(fields[5]).ok_or(())?;
    let start_description = fields[6..11].join(" ");
    let command = fields[11].to_owned();
    let args = fields.get(12..).unwrap_or_default().join(" ");
    let recognized = ["codex", ".codex-wrapped", "claude", "agy"];
    if !executable_basename(&command).is_some_and(|name| recognized.contains(&name))
        && !args
            .split_whitespace()
            .next()
            .and_then(executable_basename)
            .is_some_and(|name| recognized.contains(&name))
    {
        return Ok(None);
    }
    let cwd = cwd_resolver(pid).unwrap_or_default();
    let started_at = process_started_at_ms(now_ms, elapsed_secs);
    let start_identity =
        process_start_identity(pid).or_else(|| stable_start_identity(&start_description));
    let start_identity = start_identity.ok_or(())?;

    Ok(Some(ProcessSnapshotEntry {
        pid,
        tty,
        cpu_percent,
        mem_mb: rss_kb / 1024.0,
        command,
        args,
        cwd,
        started_at,
        start_identity,
    }))
}

fn executable_basename(command: &str) -> Option<&str> {
    Path::new(command).file_name()?.to_str()
}

fn process_cwd(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn process_started_at_ms(now_ms: u64, elapsed_secs: u64) -> u64 {
    now_ms
        .saturating_div(1000)
        .saturating_sub(elapsed_secs)
        .saturating_mul(1000)
}

fn parse_elapsed_seconds(elapsed: &str) -> Option<u64> {
    let (days, clock) = if let Some((days, clock)) = elapsed.split_once('-') {
        (days.parse::<u64>().ok()?, clock)
    } else {
        (0_u64, elapsed)
    };
    let parts: Vec<_> = clock.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (
            0_u64,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds.parse::<u64>().ok()?,
        ),
        _ => return None,
    };
    Some(
        days.saturating_mul(86_400)
            .saturating_add(hours.saturating_mul(3_600))
            .saturating_add(minutes.saturating_mul(60))
            .saturating_add(seconds),
    )
}

fn normalize_start_description(start_description: &str) -> Option<String> {
    let normalized = start_description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

pub(crate) fn stable_start_identity(start_description: &str) -> Option<u64> {
    let normalized = normalize_start_description(start_description)?;
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Some(hash.max(1))
}

fn process_start_identity(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        parse_proc_start_ticks(&stat)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub(crate) fn parse_proc_start_ticks(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Check which PIDs are alive and fetch TTY, CPU%, MEM, command args — all via `ps`.
/// No sysinfo dependency needed.
pub fn fetch_and_enrich(sessions: &mut [AgentSession]) {
    if sessions.is_empty() || sessions.iter().all(|s| !s.process_backed) {
        return;
    }

    let pids: Vec<String> = sessions
        .iter()
        .filter(|s| s.process_backed)
        .map(|s| s.pid.to_string())
        .collect();
    let pid_arg = pids.join(",");

    let output = std::process::Command::new("ps")
        .args(["-o", "pid=,tty=,%cpu=,rss=,command=", "-p", &pid_arg])
        .env_clear()
        .output();

    let output = match output {
        Ok(o) => o,
        Err(e) => {
            crate::logger::log("ERROR", &format!("ps command failed: {e}"));
            // ps failed — mark all as Finished (will show tombstone for 30s)
            for s in sessions.iter_mut() {
                s.status = SessionStatus::Finished;
                s.cpu_percent = 0.0;
            }
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Collect alive PIDs from ps output
    let mut alive_pids = std::collections::HashSet::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        let Ok(pid) = fields[0].parse::<u32>() else {
            continue;
        };
        let tty = fields[1].to_string();
        let cpu = fields[2].parse::<f32>().unwrap_or(0.0);
        let rss_kb = fields[3].parse::<f64>().unwrap_or(0.0);
        let mem_mb = rss_kb / 1024.0;
        let command = fields[4..].join(" ");

        // Only count this PID as alive if it's actually a Codex process.
        // PIDs get reused on macOS — a dead Codex session's PID may belong
        // to an unrelated process now.
        if !command.contains("codex") {
            continue;
        }

        alive_pids.insert(pid);

        for session in sessions.iter_mut() {
            if session.pid == pid {
                session.tty = tty.clone();
                session.mem_mb = mem_mb;

                // CPU smoothing: track last 3 readings, use average
                session.cpu_history.push(cpu);
                if session.cpu_history.len() > 3 {
                    session.cpu_history.remove(0);
                }
                session.cpu_percent =
                    session.cpu_history.iter().sum::<f32>() / session.cpu_history.len() as f32;

                // Extract args (everything after "codex")
                if let Some(idx) = command.find("codex") {
                    let after_codex = &command[idx + 5..];
                    session.command_args = after_codex.trim().to_string();
                }

                // Extract session name from --name, --resume, or `codex resume`
                let cmd_parts: Vec<&str> = command.split_whitespace().collect();
                extract_session_meta(&cmd_parts, session);

                break;
            }
        }
    }

    // Mark dead PIDs as Finished instead of removing them immediately.
    // They'll be displayed briefly so the user can see what exited.
    for session in sessions.iter_mut() {
        if session.process_backed && !alive_pids.contains(&session.pid) {
            session.status = crate::session::SessionStatus::Finished;
            session.cpu_percent = 0.0;
        }
    }
}

fn extract_session_meta(cmd: &[&str], session: &mut AgentSession) {
    let mut i = 0;
    while i < cmd.len() {
        match cmd[i] {
            "--name" | "-n" if i + 1 < cmd.len() => {
                session.session_name = cmd[i + 1].to_string();
                i += 2;
                continue;
            }
            "--resume" | "-r" | "resume" if i + 1 < cmd.len() => {
                let val = cmd[i + 1];
                if !looks_like_uuid(val) {
                    session.session_name = val.to_string();
                }
                i += 2;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && s.matches('-').count() == 4
}

#[cfg(test)]
mod provider_snapshot_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn process_snapshot_command_uses_portable_elapsed_and_stable_start_columns() {
        assert_eq!(
            process_snapshot_columns(),
            "pid=,ppid=,tty=,%cpu=,rss=,etime=,lstart=,comm=,args="
        );
        assert!(!process_snapshot_columns().contains("etimes="));
    }

    #[test]
    fn process_snapshot_keeps_unknown_cwd_and_stable_start_identity() {
        let first = parse_process_snapshot(
            "42 1 ttys001 1.5 2048 01:02 Wed Jul 22 08:00:00 2026 /usr/local/bin/agy /usr/local/bin/agy",
            1_000_000,
            |_| None,
        );
        let second = parse_process_snapshot(
            "42 1 ttys001 1.5 2048 01:03 Wed Jul 22 08:00:00 2026 /usr/local/bin/agy /usr/local/bin/agy",
            1_001_000,
            |_| None,
        );

        assert_eq!(first.entries.len(), 1);
        assert_eq!(first.entries[0].cwd, PathBuf::new());
        assert_eq!(first.entries[0].started_at, second.entries[0].started_at);
        assert_eq!(
            first.entries[0].start_identity,
            second.entries[0].start_identity
        );
        assert_ne!(first.entries[0].start_identity, 0);
    }

    #[test]
    fn process_snapshot_distinguishes_empty_success_from_parse_failure() {
        let empty = parse_process_snapshot("", 1_000_000, |_| None);
        let malformed = parse_process_snapshot("not a ps row", 1_000_000, |_| None);

        assert!(empty.succeeded);
        assert!(empty.entries.is_empty());
        assert!(!malformed.succeeded);
        assert!(malformed.entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn process_snapshot_command_is_time_bounded() {
        let started = Instant::now();
        let snapshot =
            capture_process_snapshot_with(std::process::Command::new("sh").args(["-c", "sleep 2"]));

        assert!(!snapshot.succeeded);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn process_snapshot_command_is_output_bounded() {
        let snapshot = capture_process_snapshot_with(std::process::Command::new("sh").args([
            "-c",
            "i=0; while [ $i -lt 70000 ]; do printf x; i=$((i + 1)); done",
        ]));

        assert!(!snapshot.succeeded);
    }

    #[test]
    fn start_identity_normalizes_full_lstart_text_and_rejects_empty_input() {
        assert_eq!(
            normalize_start_description("  Wed   Jul 22 08:00:00 2026  ").as_deref(),
            Some("Wed Jul 22 08:00:00 2026")
        );
        assert_eq!(
            stable_start_identity("  Wed   Jul 22 08:00:00 2026  "),
            Some(839_426_995_300_319_526)
        );
        assert_eq!(stable_start_identity(" \t\n "), None);
    }
}
