use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{SessionTarget, redact_activity_text};
use coding_brain_core::runtime::{
    ExternalCommand, NavigationError, NavigationPlan, SessionNavigation,
};
use coding_brain_core::{discovery, terminals};
use serde::Deserialize;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LIST_OUTPUT: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_OUTPUT: usize = 4 * 1024;
const MAX_ERROR_CHARS: usize = 512;

pub struct LiveSessionNavigation {
    agent_deck: PathBuf,
}

impl Default for LiveSessionNavigation {
    fn default() -> Self {
        Self::new("agent-deck")
    }
}

impl LiveSessionNavigation {
    pub fn new(agent_deck: impl Into<PathBuf>) -> Self {
        Self {
            agent_deck: agent_deck.into(),
        }
    }

    fn query(&self) -> Result<Vec<DeckSession>, NavigationError> {
        let bytes = run_bounded_query(&self.agent_deck)?;
        let trimmed = bytes.as_slice();
        let trimmed = trimmed.strip_suffix(b"\n").unwrap_or(trimmed);
        let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
        if trimmed.is_empty() || trimmed == b"No sessions found" {
            return Ok(Vec::new());
        }
        let rows = serde_json::from_slice::<Vec<DeckSessionRow>>(&bytes)
            .map_err(|error| NavigationError::Malformed(bounded_error(&error.to_string())))?;
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| row.validate(index))
            .collect()
    }
}

impl SessionNavigation for LiveSessionNavigation {
    fn resolve(&self, target: &SessionTarget) -> Result<NavigationPlan, NavigationError> {
        let sessions = self.query()?;
        let exact_id = sessions
            .iter()
            .filter(|session| session.id == target.session_id)
            .collect::<Vec<_>>();
        let matched = match exact_id.as_slice() {
            [session] => *session,
            [] => {
                let cwd = normalize_path(&target.cwd);
                let path_matches = sessions
                    .iter()
                    .filter(|session| normalize_path(&session.path) == cwd)
                    .collect::<Vec<_>>();
                match path_matches.as_slice() {
                    [session] => *session,
                    [] => return Err(NavigationError::NoMatch),
                    many => {
                        let hinted = many
                            .iter()
                            .copied()
                            .filter(|session| {
                                matches_provider_hint(session, &target.provider_hints)
                            })
                            .collect::<Vec<_>>();
                        match hinted.as_slice() {
                            [session] => *session,
                            _ => {
                                return Err(NavigationError::Ambiguous {
                                    matches: many.len(),
                                });
                            }
                        }
                    }
                }
            }
            many => {
                return Err(NavigationError::Ambiguous {
                    matches: many.len(),
                });
            }
        };

        Ok(NavigationPlan::External(ExternalCommand::new(
            self.agent_deck.clone(),
            [
                OsString::from("session"),
                OsString::from("attach"),
                OsString::from(&matched.id),
            ],
        )))
    }

    fn focus_fallback(&self, target: &SessionTarget) -> Result<(), String> {
        let mut sessions = discovery::scan_sessions();
        discovery::resolve_jsonl_paths(&mut sessions);
        let exact = sessions
            .iter()
            .filter(|session| session.session_id == target.session_id)
            .collect::<Vec<_>>();
        let matched = match exact.as_slice() {
            [session] => *session,
            [] => {
                let cwd = normalize_path(&target.cwd);
                let by_cwd = sessions
                    .iter()
                    .filter(|session| normalize_path(Path::new(&session.cwd)) == cwd)
                    .collect::<Vec<_>>();
                match by_cwd.as_slice() {
                    [session] => *session,
                    [] => return Err("no matching live Codex session".into()),
                    _ => return Err("live Codex session match is ambiguous".into()),
                }
            }
            _ => return Err("live Codex session ID is ambiguous".into()),
        };
        terminals::switch_to_terminal(matched)
    }
}

#[derive(Deserialize)]
struct DeckSessionRow {
    id: Option<String>,
    title: Option<String>,
    path: Option<PathBuf>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

struct DeckSession {
    id: String,
    title: String,
    path: PathBuf,
    tool: Option<String>,
    profile: Option<String>,
}

impl DeckSessionRow {
    fn validate(self, index: usize) -> Result<DeckSession, NavigationError> {
        let id = required_text(self.id, index, "id")?;
        let title = required_text(self.title, index, "title")?;
        let path = self
            .path
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or(NavigationError::MissingIdentity {
                index,
                field: "path",
            })?;
        Ok(DeckSession {
            id,
            title,
            path,
            tool: self.tool.filter(|value| !value.trim().is_empty()),
            profile: self.profile.filter(|value| !value.trim().is_empty()),
        })
    }
}

fn required_text(
    value: Option<String>,
    index: usize,
    field: &'static str,
) -> Result<String, NavigationError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or(NavigationError::MissingIdentity { index, field })
}

fn matches_provider_hint(session: &DeckSession, hints: &[String]) -> bool {
    hints.iter().any(|hint| {
        let hint = hint.trim();
        hint == session.title
            || hint == session.id
            || hint.strip_prefix("title:") == Some(session.title.as_str())
            || hint.strip_prefix("agent-deck:") == Some(session.id.as_str())
            || session
                .tool
                .as_deref()
                .is_some_and(|tool| hint.strip_prefix("tool:") == Some(tool))
            || session
                .profile
                .as_deref()
                .is_some_and(|profile| hint.strip_prefix("profile:") == Some(profile))
    })
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn run_bounded_query(agent_deck: &Path) -> Result<Vec<u8>, NavigationError> {
    let mut command = Command::new(agent_deck);
    command
        .args(["list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_query_process(&mut command);
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            NavigationError::Unavailable(error.to_string())
        } else {
            NavigationError::QueryFailed(bounded_error(&error.to_string()))
        }
    })?;
    let stdout = child.stdout.take().expect("piped stdout missing");
    let stderr = child.stderr.take().expect("piped stderr missing");
    let stdout_exceeded = Arc::new(AtomicBool::new(false));
    let stderr_exceeded = Arc::new(AtomicBool::new(false));
    let stdout_reader = {
        let exceeded = stdout_exceeded.clone();
        thread::spawn(move || read_bounded(stdout, MAX_LIST_OUTPUT, exceeded))
    };
    let stderr_reader = {
        let exceeded = stderr_exceeded.clone();
        thread::spawn(move || read_bounded(stderr, MAX_DIAGNOSTIC_OUTPUT, exceeded))
    };

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let status = loop {
        if stdout_exceeded.load(Ordering::SeqCst) {
            terminate_query_process(&mut child);
            return Err(NavigationError::OutputTooLarge {
                limit: MAX_LIST_OUTPUT,
            });
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_query_process(&mut child);
                return Err(NavigationError::TimedOut);
            }
            Err(error) => {
                terminate_query_process(&mut child);
                return Err(NavigationError::QueryFailed(bounded_error(
                    &error.to_string(),
                )));
            }
        }
    };
    while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
        if stdout_exceeded.load(Ordering::SeqCst) {
            terminate_query_process(&mut child);
            return Err(NavigationError::OutputTooLarge {
                limit: MAX_LIST_OUTPUT,
            });
        }
        if Instant::now() >= deadline {
            terminate_query_process(&mut child);
            return Err(NavigationError::TimedOut);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (stdout, stdout_exceeded) = join_reader(stdout_reader)?;
    let (stderr, _) = join_reader(stderr_reader)?;
    if stdout_exceeded {
        return Err(NavigationError::OutputTooLarge {
            limit: MAX_LIST_OUTPUT,
        });
    }
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(NavigationError::QueryFailed(if detail.trim().is_empty() {
            format!("exit status {status}")
        } else {
            bounded_error(&detail)
        }));
    }
    Ok(stdout)
}

fn read_bounded(
    mut reader: impl Read,
    limit: usize,
    exceeded_signal: Arc<AtomicBool>,
) -> io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
        exceeded |= read > remaining;
        if exceeded {
            exceeded_signal.store(true, Ordering::SeqCst);
        }
    }
    Ok((output, exceeded))
}

#[cfg(unix)]
fn isolate_query_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: `setpgid` is async-signal-safe and creates a dedicated process
    // group so the bounded query can terminate descendants that inherit its
    // output pipes.
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
fn isolate_query_process(_command: &mut Command) {}

fn terminate_query_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let process_group = -(child.id() as i32);
        // SAFETY: the child was placed in a dedicated process group before
        // exec; SIGKILL ensures inherited output pipes are closed promptly.
        unsafe {
            libc::kill(process_group, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn join_reader(
    reader: thread::JoinHandle<io::Result<(Vec<u8>, bool)>>,
) -> Result<(Vec<u8>, bool), NavigationError> {
    reader
        .join()
        .map_err(|_| NavigationError::QueryFailed("output reader panicked".into()))?
        .map_err(|error| NavigationError::QueryFailed(bounded_error(&error.to_string())))
}

fn bounded_error(value: &str) -> String {
    redact_activity_text(value.trim())
        .chars()
        .take(MAX_ERROR_CHARS)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use coding_brain_core::brain_activity::SessionTarget;
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::runtime::{
        BrainRuntime, ExternalCommand, MockBrainRuntime, NavigationError, NavigationPlan,
        SessionNavigation,
    };

    use super::LiveSessionNavigation;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn exact_session_id_builds_attach_argv() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[{"id":"deck-1","title":"project-a","path":"/work/project-a","future":true}]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        let plan = navigator
            .resolve(&target("deck-1", "/work/project-a"))
            .unwrap();

        assert_eq!(
            plan,
            NavigationPlan::External(ExternalCommand::new(
                fixture.path(),
                ["session", "attach", "deck-1"],
            ))
        );
        assert_eq!(fixture.invocation_args(), vec!["list --json"]);
    }

    #[test]
    fn ambiguous_path_is_an_error_not_a_guess() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[
                {"id":"deck-1","title":"one","path":"/work/project-a"},
                {"id":"deck-2","title":"two","path":"/work/project-a"}
            ]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        assert!(matches!(
            navigator.resolve(&target("unknown", "/work/project-a")),
            Err(NavigationError::Ambiguous { .. })
        ));
    }

    #[test]
    fn normalized_cwd_resolves_a_single_match() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[{"id":"deck-1","title":"project-a","path":"/work/project-a"}]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        let plan = navigator
            .resolve(&target("unknown", "/work/other/../project-a"))
            .unwrap();

        assert_eq!(
            plan,
            NavigationPlan::External(ExternalCommand::new(
                fixture.path(),
                ["session", "attach", "deck-1"],
            ))
        );
    }

    #[test]
    fn exact_provider_hint_disambiguates_same_cwd() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[
                {"id":"deck-1","title":"one","path":"/work/project-a","tool":"codex"},
                {"id":"deck-2","title":"two","path":"/work/project-a","tool":"claude"}
            ]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());
        let mut target = target("unknown", "/work/project-a");
        target.provider_hints.push("title:two".into());

        let plan = navigator.resolve(&target).unwrap();

        assert_eq!(
            plan,
            NavigationPlan::External(ExternalCommand::new(
                fixture.path(),
                ["session", "attach", "deck-2"],
            ))
        );
    }

    #[test]
    fn missing_required_identity_fields_are_typed_errors() {
        let _lock = test_lock();
        for (json, field) in [
            (r#"[{"title":"one","path":"/work/project"}]"#, "id"),
            (r#"[{"id":"deck-1","path":"/work/project"}]"#, "title"),
            (r#"[{"id":"deck-1","title":"one"}]"#, "path"),
        ] {
            let fixture = FixtureAgentDeck::new(json, 0);
            let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

            assert_eq!(
                navigator.resolve(&target("deck-1", "/work/project")),
                Err(NavigationError::MissingIdentity { index: 0, field })
            );
        }
    }

    #[test]
    fn missing_malformed_nonzero_and_no_match_are_nonfatal_errors() {
        let _lock = test_lock();
        let missing = LiveSessionNavigation::new("/definitely/missing/agent-deck");
        assert!(matches!(
            missing.resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::Unavailable(_))
        ));

        let malformed = FixtureAgentDeck::new("not-json", 0);
        assert!(matches!(
            LiveSessionNavigation::new(malformed.path().to_path_buf())
                .resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::Malformed(_))
        ));

        let nonzero = FixtureAgentDeck::script("echo fixture-error >&2\nexit 3\n");
        assert!(matches!(
            LiveSessionNavigation::new(nonzero.path().to_path_buf())
                .resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::QueryFailed(_))
        ));

        let empty = FixtureAgentDeck::new("No sessions found\n", 0);
        assert_eq!(
            LiveSessionNavigation::new(empty.path().to_path_buf())
                .resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::NoMatch)
        );
    }

    #[test]
    fn oversized_output_is_rejected() {
        let _lock = test_lock();
        let fixture =
            FixtureAgentDeck::script("head -c 1048577 /dev/zero | tr '\\000' x\nexit 0\n");
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        assert_eq!(
            navigator.resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::OutputTooLarge { limit: 1024 * 1024 })
        );
    }

    #[test]
    fn slow_query_times_out() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::script("exec sleep 3\n");
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        assert_eq!(
            navigator.resolve(&target("deck-1", "/work/project")),
            Err(NavigationError::TimedOut)
        );
    }

    #[test]
    fn descendant_held_pipes_cannot_extend_query_deadline() {
        let _lock = test_lock();
        for body in ["(sleep 10) &\nexit 0\n", "sleep 10\n"] {
            let fixture = FixtureAgentDeck::script(body);
            let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());
            let started = Instant::now();

            assert_eq!(
                navigator.resolve(&target("deck-1", "/work/project")),
                Err(NavigationError::TimedOut)
            );
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }

    #[test]
    fn agent_deck_is_not_invoked_when_brain_runtime_is_built() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::script("exit 99\n");
        let navigator = Arc::new(LiveSessionNavigation::new(fixture.path().to_path_buf()));
        let mock = Arc::new(MockBrainRuntime::default());

        let _runtime = BrainRuntime::new(mock.clone(), mock).with_navigation(navigator);

        assert_eq!(fixture.invocations(), 0);
    }

    fn target(id: &str, cwd: &str) -> SessionTarget {
        SessionTarget {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            session_id: id.into(),
            turn_id: None,
            tool_use_id: None,
            project_id: ProjectId::Stable("project".into()),
            cwd: cwd.into(),
            provider_hints: Vec::new(),
        }
    }

    fn test_lock() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    struct FixtureAgentDeck {
        _directory: tempfile::TempDir,
        path: PathBuf,
        invocations: PathBuf,
    }

    impl FixtureAgentDeck {
        fn new(json: &str, exit_code: i32) -> Self {
            Self::script(&format!("printf '%s' '{json}'\nexit {exit_code}\n"))
        }

        fn script(body: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("agent-deck");
            let invocations = directory.path().join("invocations");
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}",
                    invocations.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                _directory: directory,
                path,
                invocations,
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn invocations(&self) -> usize {
            self.invocation_args().len()
        }

        fn invocation_args(&self) -> Vec<String> {
            fs::read_to_string(&self.invocations)
                .map(|value| value.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }
    }
}
