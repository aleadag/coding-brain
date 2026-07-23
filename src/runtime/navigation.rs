use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use coding_brain_core::brain_activity::{SessionTarget, redact_activity_text};
use coding_brain_core::provider::{AgentProvider, AgentSessionKey};
use coding_brain_core::runtime::{
    ExternalCommand, NavigationError, NavigationPlan, SessionNavigation,
};
use coding_brain_core::session::AgentSession;
use coding_brain_core::session_links::{SessionIdentityProjection, SessionLinkStore};
use coding_brain_core::{discovery, terminals};
use serde::Deserialize;

const QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LIST_OUTPUT: usize = 1024 * 1024;
const MAX_DIAGNOSTIC_OUTPUT: usize = 4 * 1024;
const MAX_ERROR_CHARS: usize = 512;
const MAX_NATIVE_ATTACH_ID_BYTES: usize = 512;

pub struct LiveSessionNavigation {
    agent_deck: PathBuf,
    deck_query: Arc<DeckQueryRunner>,
    session_resolver: Arc<SessionResolver>,
    identity_projection: Arc<IdentityProjectionResolver>,
    terminal_focus: Arc<TerminalFocusRunner>,
}

type DeckQueryRunner = dyn Fn(&Path) -> Result<Vec<u8>, NavigationError> + Send + Sync;
type SessionResolver = dyn Fn() -> Result<Vec<AgentSession>, String> + Send + Sync;
type IdentityProjectionResolver =
    dyn Fn() -> Result<SessionIdentityProjection, String> + Send + Sync;
type TerminalFocusRunner = dyn Fn(&AgentSession) -> Result<(), String> + Send + Sync;

impl Default for LiveSessionNavigation {
    fn default() -> Self {
        Self::new("agent-deck")
    }
}

impl LiveSessionNavigation {
    pub fn new(agent_deck: impl Into<PathBuf>) -> Self {
        let discovery_state = Arc::new(Mutex::new(discovery::ProviderDiscoveryState::default()));
        let link_path =
            coding_brain_core::lifecycle::coding_brain_state_root().join("session-links.jsonl");
        Self {
            agent_deck: agent_deck.into(),
            deck_query: Arc::new(run_bounded_query),
            session_resolver: Arc::new(move || {
                let mut state = discovery_state
                    .lock()
                    .map_err(|_| "provider discovery state is unavailable".to_string())?;
                Ok(discovery::scan_agent_sessions_with_state(&mut state))
            }),
            identity_projection: Arc::new(move || match link_path.try_exists() {
                Ok(false) => Ok(SessionIdentityProjection::default()),
                Ok(true) => SessionLinkStore::at(&link_path)
                    .read_projection()
                    .map_err(|error| error.to_string()),
                Err(_) => Err("session identity link path is unavailable".into()),
            }),
            terminal_focus: Arc::new(terminals::focus_exact_terminal),
        }
    }

    #[cfg(test)]
    fn with_runners<Q, R, F>(
        agent_deck: impl Into<PathBuf>,
        deck_query: Q,
        session_resolver: R,
        terminal_focus: F,
    ) -> Self
    where
        Q: Fn(&Path) -> Result<Vec<u8>, NavigationError> + Send + Sync + 'static,
        R: Fn() -> Result<Vec<AgentSession>, String> + Send + Sync + 'static,
        F: Fn(&AgentSession) -> Result<(), String> + Send + Sync + 'static,
    {
        Self {
            agent_deck: agent_deck.into(),
            deck_query: Arc::new(deck_query),
            session_resolver: Arc::new(session_resolver),
            identity_projection: Arc::new(|| Ok(SessionIdentityProjection::default())),
            terminal_focus: Arc::new(terminal_focus),
        }
    }

    #[cfg(test)]
    fn with_identity_runners<Q, R, I, F>(
        agent_deck: impl Into<PathBuf>,
        deck_query: Q,
        session_resolver: R,
        identity_projection: I,
        terminal_focus: F,
    ) -> Self
    where
        Q: Fn(&Path) -> Result<Vec<u8>, NavigationError> + Send + Sync + 'static,
        R: Fn() -> Result<Vec<AgentSession>, String> + Send + Sync + 'static,
        I: Fn() -> Result<SessionIdentityProjection, String> + Send + Sync + 'static,
        F: Fn(&AgentSession) -> Result<(), String> + Send + Sync + 'static,
    {
        Self {
            agent_deck: agent_deck.into(),
            deck_query: Arc::new(deck_query),
            session_resolver: Arc::new(session_resolver),
            identity_projection: Arc::new(identity_projection),
            terminal_focus: Arc::new(terminal_focus),
        }
    }

    fn query(&self) -> Result<Vec<DeckSession>, NavigationError> {
        let bytes = (self.deck_query)(&self.agent_deck)?;
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
        let deck_error = match self.query() {
            Ok(sessions) => {
                let exact = sessions
                    .iter()
                    .filter(|session| matches_target(session, target))
                    .collect::<Vec<_>>();
                match exact.as_slice() {
                    [session] => {
                        return Ok(NavigationPlan::External(ExternalCommand::new(
                            self.agent_deck.clone(),
                            [
                                OsString::from("session"),
                                OsString::from("attach"),
                                OsString::from(&session.id),
                            ],
                        )));
                    }
                    [] => None,
                    many => {
                        return Err(NavigationError::Ambiguous {
                            matches: many.len(),
                        });
                    }
                }
            }
            Err(error) => Some(error),
        };

        let sessions = (self.session_resolver)()
            .map_err(|error| NavigationError::DiscoveryFailed(bounded_error(&error)))?;
        if target.provider == AgentProvider::Claude {
            let exact_attach = exact_native_attach_ids(&sessions, target);
            match exact_attach.as_slice() {
                [attach_id] => {
                    return Ok(NavigationPlan::External(ExternalCommand::new(
                        "claude",
                        ["attach", *attach_id],
                    )));
                }
                [] => {}
                many => {
                    return Err(NavigationError::Ambiguous {
                        matches: many.len(),
                    });
                }
            }
        }
        let projection = (self.identity_projection)()
            .map_err(|error| NavigationError::IdentityProjectionFailed(bounded_error(&error)))?;
        let exact = exact_discovered_sessions(&sessions, target, &projection);
        match exact.as_slice() {
            [] | [_] => Err(deck_error.unwrap_or(NavigationError::NoMatch)),
            many => Err(NavigationError::Ambiguous {
                matches: many.len(),
            }),
        }
    }

    fn focus_fallback(&self, target: &SessionTarget) -> Result<(), String> {
        let projection = (self.identity_projection)().map_err(|error| bounded_error(&error))?;
        let sessions = (self.session_resolver)().map_err(|error| bounded_error(&error))?;
        let exact = exact_discovered_sessions(&sessions, target, &projection);
        let matched = match exact.as_slice() {
            [session] => *session,
            [] => return Err("no matching live provider session".into()),
            many => {
                return Err(format!(
                    "exact live session match is ambiguous ({} matches)",
                    many.len()
                ));
            }
        };
        matched
            .live_process_identity()
            .ok_or_else(|| "matching session has no exact live process identity".to_string())?;
        (self.terminal_focus)(matched).map_err(|error| bounded_error(&error))
    }
}

fn exact_discovered_sessions<'a>(
    sessions: &'a [AgentSession],
    target: &SessionTarget,
    projection: &SessionIdentityProjection,
) -> Vec<&'a AgentSession> {
    let native = AgentSessionKey::native(target.provider, &target.session_id);
    let Some(projected_live) = projection.live_for(&native) else {
        return Vec::new();
    };
    sessions
        .iter()
        .filter(|session| {
            session.provider == target.provider
                && session.live_process_identity().as_ref() == Some(projected_live)
        })
        .collect()
}

fn exact_native_attach_ids<'a>(
    sessions: &'a [AgentSession],
    target: &SessionTarget,
) -> Vec<&'a str> {
    sessions
        .iter()
        .filter_map(|session| {
            let attach_id = session
                .native_attach_id
                .as_deref()
                .filter(|attach_id| valid_native_attach_id(attach_id))?;
            (session.provider == AgentProvider::Claude && session.session_id == target.session_id)
                .then_some(attach_id)
        })
        .collect()
}

#[derive(Deserialize)]
struct DeckSessionRow {
    id: Option<String>,
    #[serde(default)]
    tool: Option<String>,
}

struct DeckSession {
    id: String,
    provider: AgentProvider,
}

impl DeckSessionRow {
    fn validate(self, index: usize) -> Result<DeckSession, NavigationError> {
        let id = required_text(self.id, index, "id")?;
        let provider = self
            .tool
            .as_deref()
            .and_then(provider_from_deck_field)
            .ok_or(NavigationError::MissingIdentity {
                index,
                field: "provider",
            })?;
        Ok(DeckSession { id, provider })
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

fn matches_target(session: &DeckSession, target: &SessionTarget) -> bool {
    session.provider == target.provider && session.id == target.session_id
}

fn valid_native_attach_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_NATIVE_ATTACH_ID_BYTES
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn provider_from_deck_field(value: &str) -> Option<AgentProvider> {
    match value.trim().to_ascii_lowercase().as_str() {
        "codex" => Some(AgentProvider::Codex),
        "claude" | "claude-code" => Some(AgentProvider::Claude),
        "agy" | "antigravity" | "antigravity-cli" => Some(AgentProvider::Antigravity),
        _ => None,
    }
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
            NavigationError::Unavailable(bounded_error(&error.to_string()))
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
    use coding_brain_core::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};
    use coding_brain_core::runtime::{
        BrainRuntime, ExternalCommand, MockBrainRuntime, NavigationError, NavigationPlan,
        SessionNavigation,
    };
    use coding_brain_core::session::{AgentSession, RawAgentSession};
    use coding_brain_core::session_links::{
        SESSION_IDENTITY_LINK_SCHEMA_VERSION, SessionIdentityLink, SessionIdentityProjection,
        SessionLinkStore,
    };

    use super::LiveSessionNavigation;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn exact_session_id_builds_attach_argv() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[{"id":"deck-1","title":"project-a","path":"/work/project-a","tool":"codex","future":true}]"#,
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
    fn provider_qualifies_an_opaque_session_id() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[
                {"id":"same","title":"codex","path":"/work/shared","tool":"codex"},
                {"id":"same","title":"claude","path":"/work/shared","tool":"claude"}
            ]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        let plan = navigator.resolve(&target("same", "/work/shared")).unwrap();

        assert_eq!(
            plan,
            NavigationPlan::External(ExternalCommand::new(
                fixture.path(),
                ["session", "attach", "same"],
            ))
        );
    }

    #[test]
    fn cwd_is_never_navigation_authority() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[{"id":"deck-1","title":"project-a","path":"/work/project-a","tool":"codex"}]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());
        let mut target = target("unknown", "/work/project-a");
        target.provider_hints = vec![
            "agent-deck:deck-1".into(),
            "title:project-a".into(),
            "tool:codex".into(),
        ];

        assert_eq!(navigator.resolve(&target), Err(NavigationError::NoMatch));
    }

    #[test]
    fn duplicate_provider_and_session_id_is_ambiguous() {
        let _lock = test_lock();
        let fixture = FixtureAgentDeck::new(
            r#"[
                {"id":"same","tool":"codex"},
                {"id":"same","tool":"codex"}
            ]"#,
            0,
        );
        let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

        assert_eq!(
            navigator.resolve(&target("same", "/irrelevant")),
            Err(NavigationError::Ambiguous { matches: 2 })
        );
    }

    #[test]
    fn agent_deck_tool_is_authoritative_over_arbitrary_or_conflicting_profile() {
        let _lock = test_lock();
        for profile in ["workspace-profile", "claude"] {
            let fixture = FixtureAgentDeck::new(
                &format!(r#"[{{"id":"same","tool":"codex","profile":"{profile}"}}]"#),
                0,
            );
            let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

            assert_eq!(
                navigator.resolve(&target("same", "/irrelevant")),
                Ok(NavigationPlan::External(ExternalCommand::new(
                    fixture.path(),
                    ["session", "attach", "same"],
                )))
            );
        }
    }

    #[test]
    fn agent_deck_profile_alone_never_identifies_a_provider() {
        let _lock = test_lock();
        for profile in ["codex", "claude", "custom", "shell"] {
            let fixture =
                FixtureAgentDeck::new(&format!(r#"[{{"id":"same","profile":"{profile}"}}]"#), 0);
            let navigator = LiveSessionNavigation::new(fixture.path().to_path_buf());

            assert_eq!(
                navigator.resolve(&target("same", "/irrelevant")),
                Err(NavigationError::MissingIdentity {
                    index: 0,
                    field: "provider",
                })
            );
        }
    }

    #[test]
    fn claude_background_attach_uses_exact_argument_array() {
        let mut session = discovered_session(AgentProvider::Claude, "session-uuid");
        session.native_attach_id = Some("agent-42".into());
        let navigator = LiveSessionNavigation::with_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![session.clone()]),
            |_| panic!("native attach must not focus a terminal"),
        );

        let plan = navigator
            .resolve(&target_for(
                AgentProvider::Claude,
                "session-uuid",
                "/wrong/cwd",
            ))
            .unwrap();

        assert_eq!(
            plan,
            NavigationPlan::External(ExternalCommand::new("claude", ["attach", "agent-42"],))
        );
    }

    #[test]
    fn claude_attach_id_is_bounded_and_canonical_before_argv_construction() {
        let valid_limit = "x".repeat(512);
        let mut valid_session = discovered_session(AgentProvider::Claude, "valid-session");
        valid_session.native_attach_id = Some(valid_limit.clone());
        let valid_navigator = LiveSessionNavigation::with_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![valid_session.clone()]),
            |_| panic!("native attach must not focus a terminal"),
        );

        assert_eq!(
            valid_navigator.resolve(&target_for(
                AgentProvider::Claude,
                "valid-session",
                "/irrelevant",
            )),
            Ok(NavigationPlan::External(ExternalCommand::new(
                "claude",
                ["attach", valid_limit.as_str()],
            )))
        );

        for invalid_attach_id in ["x".repeat(513), " agent-42".into(), "agent-\n42".into()] {
            let mut session = discovered_session(AgentProvider::Claude, "invalid-session");
            session.native_attach_id = Some(invalid_attach_id);
            let navigator = LiveSessionNavigation::with_identity_runners(
                "agent-deck",
                |_| Ok(b"[]".to_vec()),
                move || Ok(vec![session.clone()]),
                || Ok(SessionIdentityProjection::default()),
                |_| panic!("invalid native attach must not focus during resolution"),
            );

            assert_eq!(
                navigator.resolve(&target_for(
                    AgentProvider::Claude,
                    "invalid-session",
                    "/irrelevant",
                )),
                Err(NavigationError::NoMatch)
            );
        }
    }

    #[test]
    fn focus_fallback_requires_exact_provider_id_and_live_identity() {
        let focused = Arc::new(Mutex::new(Vec::new()));
        let recorded = focused.clone();
        let exact = discovered_session(AgentProvider::Antigravity, "conversation-7");
        let projection = identity_projection(
            AgentProvider::Antigravity,
            "conversation-7",
            exact.live_process_identity().unwrap(),
        );
        let wrong_provider = discovered_session(AgentProvider::Codex, "conversation-7");
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![wrong_provider.clone(), exact.clone()]),
            move || Ok(projection.clone()),
            move |session| {
                recorded
                    .lock()
                    .unwrap()
                    .push((session.provider, session.session_id.clone()));
                Ok(())
            },
        );

        navigator
            .focus_fallback(&target_for(
                AgentProvider::Antigravity,
                "conversation-7",
                "/unrelated",
            ))
            .unwrap();

        assert_eq!(
            focused.lock().unwrap().as_slice(),
            [(AgentProvider::Antigravity, "conversation-7".into())]
        );
    }

    #[test]
    fn focus_fallback_rejects_a_session_without_live_process_identity() {
        let mut session = discovered_session(AgentProvider::Claude, "foreground");
        let projection = identity_projection(
            AgentProvider::Claude,
            "foreground",
            session.live_process_identity().unwrap(),
        );
        session.process_start_identity = None;
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![session.clone()]),
            move || Ok(projection.clone()),
            |_| panic!("unidentified session must not be focused"),
        );

        let error = navigator
            .focus_fallback(&target_for(
                AgentProvider::Claude,
                "foreground",
                "/same/cwd",
            ))
            .unwrap_err();

        assert!(error.contains("no matching live provider session"));
    }

    #[test]
    fn focus_fallback_rejects_multiple_exact_discovery_matches() {
        let exact = discovered_session(AgentProvider::Codex, "same");
        let duplicate = exact.clone();
        let projection = identity_projection(
            AgentProvider::Codex,
            "same",
            exact.live_process_identity().unwrap(),
        );
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![exact.clone(), duplicate.clone()]),
            move || Ok(projection.clone()),
            |_| panic!("ambiguous sessions must not be focused"),
        );

        let error = navigator
            .focus_fallback(&target_for(AgentProvider::Codex, "same", "/same/cwd"))
            .unwrap_err();

        assert!(error.contains("ambiguous (2 matches)"));
    }

    #[test]
    fn native_id_focuses_the_projected_exact_process_only_session() {
        let focused = Arc::new(Mutex::new(Vec::new()));
        let recorded = focused.clone();
        let mut session = discovered_session(AgentProvider::Claude, "process:synthetic");
        let live_process = session.live_process_identity().unwrap();
        let projection = identity_projection(AgentProvider::Claude, "native-session", live_process);
        session.native_attach_id = None;
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![session.clone()]),
            move || Ok(projection.clone()),
            move |session| {
                recorded
                    .lock()
                    .unwrap()
                    .push((session.provider, session.session_id.clone()));
                Ok(())
            },
        );

        navigator
            .focus_fallback(&target_for(
                AgentProvider::Claude,
                "native-session",
                "/irrelevant",
            ))
            .unwrap();

        assert_eq!(
            focused.lock().unwrap().as_slice(),
            [(AgentProvider::Claude, "process:synthetic".into())]
        );
    }

    #[test]
    fn projected_focus_rejects_provider_and_live_identity_mismatches() {
        let claude_session = discovered_session(AgentProvider::Claude, "process:claude");
        let antigravity_live =
            LiveProcessIdentity::try_new(AgentProvider::Antigravity, 42, 9001, "pts/7").unwrap();
        let provider_mismatch = identity_projection(
            AgentProvider::Antigravity,
            "native-session",
            antigravity_live,
        );
        let stale_live =
            LiveProcessIdentity::try_new(AgentProvider::Claude, 43, 9002, "pts/8").unwrap();
        let stale_mismatch =
            identity_projection(AgentProvider::Claude, "native-session", stale_live);

        for projection in [provider_mismatch, stale_mismatch] {
            let session = claude_session.clone();
            let navigator = LiveSessionNavigation::with_identity_runners(
                "agent-deck",
                |_| Ok(b"[]".to_vec()),
                move || Ok(vec![session.clone()]),
                move || Ok(projection.clone()),
                |_| panic!("mismatched identity must not be focused"),
            );

            assert!(
                navigator
                    .focus_fallback(&target_for(
                        AgentProvider::Claude,
                        "native-session",
                        "/same/cwd",
                    ))
                    .is_err()
            );
        }
    }

    #[test]
    fn projected_focus_fails_closed_when_the_link_is_missing_or_corrupt() {
        for projection in [
            Ok(SessionIdentityProjection::default()),
            Err("corrupt session identity projection".to_string()),
        ] {
            let session = discovered_session(AgentProvider::Claude, "process:claude");
            let navigator = LiveSessionNavigation::with_identity_runners(
                "agent-deck",
                |_| Ok(b"[]".to_vec()),
                move || Ok(vec![session.clone()]),
                move || projection.clone(),
                |_| panic!("missing or corrupt links must not focus"),
            );

            assert!(
                navigator
                    .focus_fallback(&target_for(
                        AgentProvider::Claude,
                        "native-session",
                        "/same/cwd",
                    ))
                    .is_err()
            );
        }
    }

    #[test]
    fn same_native_id_without_a_projection_never_authorizes_focus() {
        let session = discovered_session(AgentProvider::Claude, "native-session");
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![session.clone()]),
            || Ok(SessionIdentityProjection::default()),
            |_| panic!("an unprojected native ID must not authorize focus"),
        );

        assert!(
            navigator
                .focus_fallback(&target_for(
                    AgentProvider::Claude,
                    "native-session",
                    "/irrelevant",
                ))
                .is_err()
        );
    }

    #[test]
    fn projected_live_identity_wins_over_a_raw_session_id_collision() {
        let focused = Arc::new(Mutex::new(Vec::new()));
        let recorded = focused.clone();
        let projected = discovered_session(AgentProvider::Claude, "process:projected");
        let projection = identity_projection(
            AgentProvider::Claude,
            "native-session",
            projected.live_process_identity().unwrap(),
        );
        let mut collision = discovered_session(AgentProvider::Claude, "native-session");
        collision.pid = 43;
        collision.process_start_identity = Some(9002);
        collision.tty = "pts/8".into();
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![projected.clone(), collision.clone()]),
            move || Ok(projection.clone()),
            move |session| {
                recorded.lock().unwrap().push(session.session_id.clone());
                Ok(())
            },
        );

        navigator
            .focus_fallback(&target_for(
                AgentProvider::Claude,
                "native-session",
                "/irrelevant",
            ))
            .unwrap();

        assert_eq!(focused.lock().unwrap().as_slice(), ["process:projected"]);
    }

    #[test]
    fn projected_focus_rejects_ambiguous_live_identity_matches() {
        let first = discovered_session(AgentProvider::Claude, "process:first");
        let mut second = first.clone();
        second.session_id = "process:second".into();
        let projection = identity_projection(
            AgentProvider::Claude,
            "native-session",
            first.live_process_identity().unwrap(),
        );
        let navigator = LiveSessionNavigation::with_identity_runners(
            "agent-deck",
            |_| Ok(b"[]".to_vec()),
            move || Ok(vec![first.clone(), second.clone()]),
            move || Ok(projection.clone()),
            |_| panic!("ambiguous projected identity must not focus"),
        );

        let error = navigator
            .focus_fallback(&target_for(
                AgentProvider::Claude,
                "native-session",
                "/irrelevant",
            ))
            .unwrap_err();

        assert!(error.contains("ambiguous (2 matches)"));
    }

    #[test]
    fn missing_required_identity_fields_are_typed_errors() {
        let _lock = test_lock();
        for (json, field) in [
            (r#"[{"tool":"codex"}]"#, "id"),
            (
                r#"[{"id":"deck-1","title":"codex","path":"/work/project"}]"#,
                "provider",
            ),
            (r#"[{"id":"deck-1","tool":"unknown"}]"#, "provider"),
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
        target_for(AgentProvider::Codex, id, cwd)
    }

    fn target_for(provider: AgentProvider, id: &str, cwd: &str) -> SessionTarget {
        SessionTarget {
            provider,
            session_id: id.into(),
            turn_id: None,
            tool_use_id: None,
            project_id: ProjectId::Stable("project".into()),
            cwd: cwd.into(),
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        }
    }

    fn discovered_session(provider: AgentProvider, id: &str) -> AgentSession {
        let mut session = AgentSession::from_raw(RawAgentSession {
            provider,
            pid: 42,
            process_start_identity: Some(9001),
            session_id: id.into(),
            cwd: "/work/provider".into(),
            started_at: 1,
        });
        session.tty = "pts/7".into();
        session
    }

    fn identity_projection(
        provider: AgentProvider,
        native_id: &str,
        live_process: LiveProcessIdentity,
    ) -> SessionIdentityProjection {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionLinkStore::at(directory.path().join("session-links.jsonl"));
        store
            .append(SessionIdentityLink {
                schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                recorded_at_ms: 1,
                provider,
                native_session_id: native_id.into(),
                live_process,
            })
            .unwrap();
        let projection = store.read_projection().unwrap();
        assert!(
            projection
                .live_for(&AgentSessionKey::native(provider, native_id))
                .is_some()
        );
        projection
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
