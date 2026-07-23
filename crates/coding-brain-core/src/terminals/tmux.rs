use crate::session::AgentSession;
use crate::terminals::{BoundedOutput, PaneCapture, Terminal, checked_capture, run_bounded};

const MAX_ANCESTRY_DEPTH: usize = 64;

trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String>;
}

struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        let output = run_bounded(std::process::Command::new(program).args(args))?;
        if !output.status.success() {
            return Err(format!("{program} returned non-zero"));
        }
        String::from_utf8(output.stdout).map_err(|_| format!("{program} returned non-UTF-8 output"))
    }
}

#[derive(Debug)]
struct TmuxPane {
    target: String,
    tty: String,
    pid: u32,
}

fn normalize_tty(tty: &str) -> &str {
    tty.trim().trim_start_matches("/dev/")
}

fn parse_panes(output: &str) -> Result<Vec<TmuxPane>, String> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let target = fields.next().unwrap_or_default().trim();
            let tty = fields.next().unwrap_or_default().trim();
            let pid = fields
                .next()
                .and_then(|value| value.trim().parse::<u32>().ok());
            if target.is_empty() || normalize_tty(tty).is_empty() || fields.next().is_some() {
                return Err("tmux returned malformed pane identity".into());
            }
            Ok(TmuxPane {
                target: target.into(),
                tty: normalize_tty(tty).into(),
                pid: pid
                    .filter(|pid| *pid != 0)
                    .ok_or_else(|| "tmux returned malformed pane process identity".to_string())?,
            })
        })
        .collect()
}

fn ancestry_contains(
    pid: u32,
    ancestor: u32,
    parent_of: &mut impl FnMut(u32) -> Option<u32>,
) -> bool {
    let mut current = pid;
    let mut visited = Vec::with_capacity(MAX_ANCESTRY_DEPTH);
    for _ in 0..MAX_ANCESTRY_DEPTH {
        if current == ancestor {
            return true;
        }
        if current == 0 || visited.contains(&current) {
            return false;
        }
        visited.push(current);
        let Some(parent) = parent_of(current) else {
            return false;
        };
        current = parent;
    }
    false
}

fn select_exact_pane(
    identity: &crate::provider::LiveProcessIdentity,
    panes: &[TmuxPane],
    mut parent_of: impl FnMut(u32) -> Option<u32>,
) -> Result<String, String> {
    let matches = panes
        .iter()
        .filter(|pane| {
            pane.tty == identity.tty && ancestry_contains(identity.pid, pane.pid, &mut parent_of)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [pane] => Ok(pane.target.clone()),
        [] => Err("no tmux pane matched the exact live process identity".into()),
        _ => Err("multiple tmux panes matched the live process ancestry".into()),
    }
}

fn process_row(runner: &dyn CommandRunner, pid: u32) -> Result<(u32, u32, String), String> {
    let output = runner.run(
        "ps",
        &[
            "-o".into(),
            "pid=,ppid=,tty=".into(),
            "-p".into(),
            pid.to_string(),
        ],
    )?;
    let mut fields = output.split_whitespace();
    let observed_pid = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "ps returned malformed process identity".to_string())?;
    let parent_pid = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "ps returned malformed process ancestry".to_string())?;
    let tty = fields
        .next()
        .map(normalize_tty)
        .filter(|tty| !tty.is_empty())
        .ok_or_else(|| "ps returned missing process TTY".to_string())?;
    if fields.next().is_some() {
        return Err("ps returned malformed process row".into());
    }
    Ok((observed_pid, parent_pid, tty.into()))
}

fn parse_proc_start_ticks(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(any(not(target_os = "linux"), test))]
fn stable_start_identity(lstart: &str) -> Option<u64> {
    let normalized = lstart.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut hash = 14_695_981_039_346_656_037_u64;
    for byte in normalized.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    Some(hash.max(1))
}

#[cfg(any(not(target_os = "linux"), test))]
fn process_start_matches_lstart(expected: u64, lstart: &str) -> bool {
    stable_start_identity(lstart) == Some(expected)
}

#[cfg(target_os = "linux")]
fn process_start_matches(
    runner: &dyn CommandRunner,
    identity: &crate::provider::LiveProcessIdentity,
) -> bool {
    runner
        .run("cat", &[format!("/proc/{}/stat", identity.pid)])
        .ok()
        .and_then(|stat| parse_proc_start_ticks(&stat))
        == Some(identity.process_start_identity)
}

#[cfg(not(target_os = "linux"))]
fn process_start_matches(
    runner: &dyn CommandRunner,
    identity: &crate::provider::LiveProcessIdentity,
) -> bool {
    let Ok(output) = runner.run(
        "ps",
        &[
            "-o".into(),
            "lstart=".into(),
            "-p".into(),
            identity.pid.to_string(),
        ],
    ) else {
        return false;
    };
    process_start_matches_lstart(identity.process_start_identity, &output)
}

fn resolve_exact_target_with(
    session: &AgentSession,
    runner: &dyn CommandRunner,
) -> Result<String, String> {
    let identity = session.live_process_identity().ok_or_else(|| {
        "guarded terminal action requires an exact live process identity".to_string()
    })?;
    let (pid, _, tty) = process_row(runner, identity.pid)?;
    if !identity.matches_provider(session.provider, pid, identity.process_start_identity, &tty)
        || !process_start_matches(runner, &identity)
    {
        return Err("live process identity changed".into());
    }
    let panes = parse_panes(&runner.run(
        "tmux",
        &[
            "list-panes".into(),
            "-a".into(),
            "-F".into(),
            "#{pane_id}\t#{pane_tty}\t#{pane_pid}".into(),
        ],
    )?)?;
    select_exact_pane(&identity, &panes, |pid| {
        process_row(runner, pid).ok().map(|(_, parent, _)| parent)
    })
}

pub fn resolve_exact_target(session: &AgentSession) -> Result<String, String> {
    resolve_exact_target_with(session, &RealCommandRunner)
}

fn focus_target_with(target: &str, runner: &dyn CommandRunner) -> Result<(), String> {
    for action in ["select-window", "select-pane"] {
        runner.run("tmux", &[action.into(), "-t".into(), target.into()])?;
    }
    Ok(())
}

pub fn focus_target(target: &str) -> Result<(), String> {
    focus_target_with(target, &RealCommandRunner)
}

pub fn capture_target(target: &str) -> Result<PaneCapture, String> {
    let output = run_bounded(std::process::Command::new("tmux").args([
        "capture-pane",
        "-p",
        "-S",
        "-80",
        "-t",
        target,
    ]))?;
    checked_capture(Terminal::Tmux, target.into(), output)
}

pub fn send_literal(target: &str, text: &str) -> Result<(), String> {
    let BoundedOutput { status, .. } = run_bounded(std::process::Command::new("tmux").args([
        "send-keys",
        "-t",
        target,
        "-l",
        "--",
        text,
    ]))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "tmux literal send returned non-zero".into())
}

pub fn send_keys(target: &str, keys: &[&str]) -> Result<(), String> {
    let mut command = std::process::Command::new("tmux");
    command.args(["send-keys", "-t", target, "--"]);
    command.args(keys);
    let BoundedOutput { status, .. } = run_bounded(&mut command)?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "tmux key send returned non-zero".into())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{AgentProvider, LiveProcessIdentity};
    use crate::session::RawAgentSession;
    use std::collections::VecDeque;

    struct FakeCommandRunner {
        outputs: std::sync::Mutex<VecDeque<(&'static str, Result<String, String>)>>,
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    }

    impl FakeCommandRunner {
        fn new(outputs: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
            Self {
                outputs: std::sync::Mutex::new(
                    outputs
                        .into_iter()
                        .map(|(program, output)| (program, Ok(output.into())))
                        .collect(),
                ),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn with_results(
            outputs: impl IntoIterator<Item = (&'static str, Result<&'static str, &'static str>)>,
        ) -> Self {
            Self {
                outputs: std::sync::Mutex::new(
                    outputs
                        .into_iter()
                        .map(|(program, output)| {
                            (program, output.map(str::to_owned).map_err(str::to_owned))
                        })
                        .collect(),
                ),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeCommandRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec()));
            let (expected_program, output) = self.outputs.lock().unwrap().pop_front().unwrap();
            assert_eq!(program, expected_program);
            output
        }
    }

    fn guarded_session() -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider: AgentProvider::Antigravity,
            pid: 7,
            process_start_identity: Some(99),
            session_id: "agy-7".into(),
            cwd: "/repo".into(),
            started_at: 0,
        });
        session.tty = "pts/7".into();
        session
    }

    #[test]
    fn guarded_exact_pane_requires_matching_tty_and_agent_ancestry() {
        let identity =
            LiveProcessIdentity::try_new(AgentProvider::Antigravity, 7, 99, "pts/7").unwrap();
        let panes = parse_panes("%1\t/dev/pts/7\t100\n%2\t/dev/pts/8\t200\n").unwrap();

        let target = select_exact_pane(&identity, &panes, |pid| match pid {
            7 => Some(50),
            50 => Some(100),
            _ => None,
        })
        .unwrap();

        assert_eq!(target, "%1");
    }

    #[test]
    fn guarded_nested_matching_panes_are_ambiguous() {
        let identity = LiveProcessIdentity::try_new(AgentProvider::Claude, 7, 99, "pts/7").unwrap();
        let panes = parse_panes("%1\t/dev/pts/7\t100\n%2\t/dev/pts/7\t50\n").unwrap();

        let error = select_exact_pane(&identity, &panes, |pid| match pid {
            7 => Some(50),
            50 => Some(100),
            _ => None,
        })
        .unwrap_err();

        assert!(error.contains("multiple"));
    }

    #[test]
    fn proc_start_parser_reads_linux_start_ticks() {
        let stat = "7 (agy worker) S 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 99 22 23";
        assert_eq!(parse_proc_start_ticks(stat), Some(99));
    }

    #[test]
    fn portable_start_identity_matches_task4_lstart_representation_exactly() {
        assert_eq!(
            stable_start_identity("  Wed   Jul 22 08:00:00 2026  "),
            Some(839_426_995_300_319_526)
        );
        assert_eq!(stable_start_identity(" \t\n "), None);
        assert!(process_start_matches_lstart(
            839_426_995_300_319_526,
            "Wed Jul 22 08:00:00 2026"
        ));
        assert!(!process_start_matches_lstart(
            839_426_995_300_319_526,
            "Wed Jul 22 08:00:01 2026"
        ));
    }

    #[test]
    fn guarded_exact_target_rejects_changed_tty_before_listing_panes() {
        let runner = FakeCommandRunner::new([("ps", "7 1 pts/8\n")]);

        let error = resolve_exact_target_with(&guarded_session(), &runner).unwrap_err();

        assert!(error.contains("identity changed"));
        assert!(runner.outputs.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guarded_exact_target_rejects_changed_process_start_before_listing_panes() {
        let stat = "7 (agy worker) S 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 98 22 23";
        let runner = FakeCommandRunner::new([("ps", "7 1 pts/7\n"), ("cat", stat)]);

        let error = resolve_exact_target_with(&guarded_session(), &runner).unwrap_err();

        assert!(error.contains("identity changed"));
        assert!(runner.outputs.lock().unwrap().is_empty());
    }

    #[test]
    fn exact_focus_selects_the_containing_window_before_the_pane() {
        let runner = FakeCommandRunner::new([("tmux", ""), ("tmux", "")]);

        focus_target_with("%7", &runner).unwrap();

        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            [
                (
                    "tmux".into(),
                    vec!["select-window".into(), "-t".into(), "%7".into()]
                ),
                (
                    "tmux".into(),
                    vec!["select-pane".into(), "-t".into(), "%7".into()]
                ),
            ]
        );
    }

    #[test]
    fn exact_focus_stops_after_a_checked_window_failure() {
        let runner = FakeCommandRunner::with_results([("tmux", Err("window failed"))]);

        let error = focus_target_with("%7", &runner).unwrap_err();

        assert!(error.contains("window failed"));
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
        assert!(runner.outputs.lock().unwrap().is_empty());
    }

    #[test]
    fn exact_focus_reports_a_checked_pane_failure() {
        let runner =
            FakeCommandRunner::with_results([("tmux", Ok("")), ("tmux", Err("pane failed"))]);

        let error = focus_target_with("%7", &runner).unwrap_err();

        assert!(error.contains("pane failed"));
        assert_eq!(runner.calls.lock().unwrap().len(), 2);
        assert!(runner.outputs.lock().unwrap().is_empty());
    }
}
