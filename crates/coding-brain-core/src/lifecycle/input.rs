use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provider::AgentProvider;

pub const MAX_ID_BYTES: usize = 512;
pub const MAX_PATH_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectedStatus {
    Processing,
    NeedsInput,
    Idle,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDisposition {
    Decided,
    NeedsInput,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

impl SessionStartSource {
    fn parse(value: &str) -> Result<Self, LifecycleInputError> {
        match value {
            "startup" => Ok(Self::Startup),
            "resume" => Ok(Self::Resume),
            "clear" => Ok(Self::Clear),
            "compact" => Ok(Self::Compact),
            _ => Err(LifecycleInputError::Invalid("source")),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEventName {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    SubagentStart,
    SubagentStop,
    Stop,
}

impl LifecycleEventName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::SubagentStart => "SubagentStart",
            Self::SubagentStop => "SubagentStop",
            Self::Stop => "Stop",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LifecycleEventKind {
    SessionStart { source: SessionStartSource },
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest { disposition: PermissionDisposition },
    SubagentStart { agent_id: String },
    SubagentStop { agent_id: String },
    Stop,
}

impl LifecycleEventKind {
    pub fn name(&self) -> LifecycleEventName {
        match self {
            Self::SessionStart { .. } => LifecycleEventName::SessionStart,
            Self::UserPromptSubmit => LifecycleEventName::UserPromptSubmit,
            Self::PreToolUse => LifecycleEventName::PreToolUse,
            Self::PostToolUse => LifecycleEventName::PostToolUse,
            Self::PermissionRequest { .. } => LifecycleEventName::PermissionRequest,
            Self::SubagentStart { .. } => LifecycleEventName::SubagentStart,
            Self::SubagentStop { .. } => LifecycleEventName::SubagentStop,
            Self::Stop => LifecycleEventName::Stop,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleIdentity {
    provider: AgentProvider,
    session_id: String,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
}

impl LifecycleIdentity {
    pub fn try_new(
        provider: AgentProvider,
        session_id: String,
        turn_id: Option<String>,
        transcript_path: Option<PathBuf>,
        cwd: PathBuf,
    ) -> Result<Self, LifecycleInputError> {
        validate_id("session_id", &session_id)?;
        if let Some(turn_id) = turn_id.as_deref() {
            validate_id("turn_id", turn_id)?;
        }
        let cwd = validate_path("cwd", cwd)?;
        let transcript_path = transcript_path
            .map(|path| validate_path("transcript_path", path))
            .transpose()?;
        Ok(Self {
            provider,
            session_id,
            turn_id,
            transcript_path,
            cwd,
        })
    }

    pub fn provider(&self) -> AgentProvider {
        self.provider
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn transcript_path(&self) -> Option<&Path> {
        self.transcript_path.as_deref()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleEvent {
    identity: LifecycleIdentity,
    kind: LifecycleEventKind,
}

impl LifecycleEvent {
    pub fn parse(raw: &[u8]) -> Result<Self, LifecycleInputError> {
        let raw: RawLifecycleEvent =
            serde_json::from_slice(raw).map_err(|_| LifecycleInputError::InvalidJson)?;
        let session_id = required(raw.session_id, "session_id")?;
        let cwd = required(raw.cwd, "cwd")?;
        let event_name = required(raw.hook_event_name, "hook_event_name")?;
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            session_id,
            raw.turn_id,
            raw.transcript_path,
            PathBuf::from(cwd),
        )?;

        let kind = match event_name.as_str() {
            "SessionStart" => LifecycleEventKind::SessionStart {
                source: SessionStartSource::parse(&required(raw.source, "source")?)?,
            },
            "UserPromptSubmit" => {
                require_turn(&identity)?;
                LifecycleEventKind::UserPromptSubmit
            }
            "PreToolUse" => {
                require_turn(&identity)?;
                LifecycleEventKind::PreToolUse
            }
            "PostToolUse" => {
                require_turn(&identity)?;
                LifecycleEventKind::PostToolUse
            }
            "SubagentStart" => {
                require_turn(&identity)?;
                LifecycleEventKind::SubagentStart {
                    agent_id: validated_agent(raw.agent_id)?,
                }
            }
            "SubagentStop" => {
                require_turn(&identity)?;
                LifecycleEventKind::SubagentStop {
                    agent_id: validated_agent(raw.agent_id)?,
                }
            }
            "Stop" => {
                require_turn(&identity)?;
                LifecycleEventKind::Stop
            }
            _ => return Err(LifecycleInputError::UnsupportedEvent),
        };
        Ok(Self { identity, kind })
    }

    pub fn permission(
        identity: LifecycleIdentity,
        disposition: PermissionDisposition,
    ) -> Result<Self, LifecycleInputError> {
        require_turn(&identity)?;
        Ok(Self {
            identity,
            kind: LifecycleEventKind::PermissionRequest { disposition },
        })
    }

    pub fn identity(&self) -> &LifecycleIdentity {
        &self.identity
    }

    pub fn kind(&self) -> &LifecycleEventKind {
        &self.kind
    }

    pub fn name(&self) -> LifecycleEventName {
        self.kind.name()
    }
}

#[derive(Debug, Deserialize)]
struct RawLifecycleEvent {
    session_id: Option<String>,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: Option<String>,
    hook_event_name: Option<String>,
    source: Option<String>,
    agent_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LifecycleInputError {
    InvalidJson,
    UnsupportedEvent,
    Missing(&'static str),
    Empty(&'static str),
    TooLong(&'static str),
    Invalid(&'static str),
}

impl fmt::Display for LifecycleInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => f.write_str("invalid JSON"),
            Self::UnsupportedEvent => f.write_str("unsupported lifecycle event"),
            Self::Missing(field) => write!(f, "missing {field}"),
            Self::Empty(field) => write!(f, "empty {field}"),
            Self::TooLong(field) => write!(f, "{field} exceeds its size limit"),
            Self::Invalid(field) => write!(f, "invalid {field}"),
        }
    }
}

impl std::error::Error for LifecycleInputError {}

fn required(value: Option<String>, field: &'static str) -> Result<String, LifecycleInputError> {
    let value = value.ok_or(LifecycleInputError::Missing(field))?;
    if value.is_empty() {
        return Err(LifecycleInputError::Empty(field));
    }
    Ok(value)
}

fn require_turn(identity: &LifecycleIdentity) -> Result<(), LifecycleInputError> {
    match identity.turn_id() {
        Some(_) => Ok(()),
        None => Err(LifecycleInputError::Missing("turn_id")),
    }
}

fn validated_agent(agent_id: Option<String>) -> Result<String, LifecycleInputError> {
    let agent_id = required(agent_id, "agent_id")?;
    validate_id("agent_id", &agent_id)?;
    Ok(agent_id)
}

fn validate_id(field: &'static str, value: &str) -> Result<(), LifecycleInputError> {
    if value.is_empty() {
        return Err(LifecycleInputError::Empty(field));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(LifecycleInputError::TooLong(field));
    }
    Ok(())
}

fn validate_path(field: &'static str, path: PathBuf) -> Result<PathBuf, LifecycleInputError> {
    if path.as_os_str().is_empty() {
        return Err(LifecycleInputError::Empty(field));
    }
    if path.to_string_lossy().len() > MAX_PATH_BYTES {
        return Err(LifecycleInputError::TooLong(field));
    }
    if !path.is_absolute() {
        return Err(LifecycleInputError::Invalid(field));
    }
    lexical_normalize(&path).ok_or(LifecycleInputError::Invalid(field))
}

fn lexical_normalize(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;

    const GENERIC_FIXTURES: &[&[u8]] = &[
        include_bytes!("../../../../tests/fixtures/hooks/session-start.json"),
        include_bytes!("../../../../tests/fixtures/hooks/user-prompt-submit.json"),
        include_bytes!("../../../../tests/fixtures/hooks/pre-tool-use.json"),
        include_bytes!("../../../../tests/fixtures/hooks/post-tool-use.json"),
        include_bytes!("../../../../tests/fixtures/hooks/subagent-start.json"),
        include_bytes!("../../../../tests/fixtures/hooks/subagent-stop.json"),
        include_bytes!("../../../../tests/fixtures/hooks/stop.json"),
    ];

    fn event_json(event: &str, session_id: &str, turn_id: &str, cwd: &str) -> String {
        json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "transcript_path": "/tmp/rollout.jsonl",
            "cwd": cwd,
            "hook_event_name": event,
        })
        .to_string()
    }

    #[test]
    fn parses_installed_generic_events_without_sensitive_bodies() {
        for raw in GENERIC_FIXTURES {
            let event = LifecycleEvent::parse(raw).unwrap();
            let persisted = serde_json::to_string(&event).unwrap();
            assert!(!persisted.contains("do not persist me"));
            assert!(!persisted.contains("gpt-test"));
        }
    }

    #[test]
    fn generic_fixture_names_are_stable() {
        let names = GENERIC_FIXTURES
            .iter()
            .map(|raw| LifecycleEvent::parse(raw).unwrap().name())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                LifecycleEventName::SessionStart,
                LifecycleEventName::UserPromptSubmit,
                LifecycleEventName::PreToolUse,
                LifecycleEventName::PostToolUse,
                LifecycleEventName::SubagentStart,
                LifecycleEventName::SubagentStop,
                LifecycleEventName::Stop,
            ]
        );
    }

    #[test]
    fn rejects_oversized_identity_and_path() {
        let raw = event_json("UserPromptSubmit", &"x".repeat(513), "turn-1", "/work");
        assert_eq!(
            LifecycleEvent::parse(raw.as_bytes()).unwrap_err(),
            LifecycleInputError::TooLong("session_id")
        );

        let raw = event_json(
            "UserPromptSubmit",
            "session-1",
            "turn-1",
            &format!("/{}", "x".repeat(4096)),
        );
        assert_eq!(
            LifecycleEvent::parse(raw.as_bytes()).unwrap_err(),
            LifecycleInputError::TooLong("cwd")
        );
    }

    #[test]
    fn rejects_wrong_or_adapter_owned_events() {
        for event in ["PermissionRequest", "PreCompact", "PostCompact", "Unknown"] {
            let raw = event_json(event, "session-1", "turn-1", "/work");
            assert_eq!(
                LifecycleEvent::parse(raw.as_bytes()).unwrap_err(),
                LifecycleInputError::UnsupportedEvent
            );
        }
    }

    #[test]
    fn rejects_missing_or_empty_required_fields() {
        let empty_turn = event_json("UserPromptSubmit", "session-1", "", "/work");
        assert_eq!(
            LifecycleEvent::parse(empty_turn.as_bytes()).unwrap_err(),
            LifecycleInputError::Empty("turn_id")
        );

        let missing_source = json!({
            "session_id": "session-1",
            "cwd": "/work",
            "hook_event_name": "SessionStart"
        });
        assert_eq!(
            LifecycleEvent::parse(missing_source.to_string().as_bytes()).unwrap_err(),
            LifecycleInputError::Missing("source")
        );

        for event in ["SubagentStart", "SubagentStop"] {
            let missing_agent = event_json(event, "session-1", "turn-1", "/work");
            assert_eq!(
                LifecycleEvent::parse(missing_agent.as_bytes()).unwrap_err(),
                LifecycleInputError::Missing("agent_id")
            );
        }
    }

    #[test]
    fn identity_constructor_is_bounded_and_normalizes_paths() {
        assert_eq!(
            LifecycleIdentity::try_new(
                AgentProvider::Codex,
                "".into(),
                Some("turn-1".into()),
                None,
                PathBuf::from("/work")
            )
            .unwrap_err(),
            LifecycleInputError::Empty("session_id")
        );
        assert_eq!(
            LifecycleIdentity::try_new(
                AgentProvider::Codex,
                "x".repeat(513),
                Some("turn-1".into()),
                None,
                PathBuf::from("/work")
            )
            .unwrap_err(),
            LifecycleInputError::TooLong("session_id")
        );
        assert_eq!(
            LifecycleIdentity::try_new(
                AgentProvider::Codex,
                "session-1".into(),
                Some("turn-1".into()),
                None,
                PathBuf::from(format!("/{}", "x".repeat(4096)))
            )
            .unwrap_err(),
            LifecycleInputError::TooLong("cwd")
        );

        let identity = LifecycleIdentity::try_new(
            AgentProvider::Claude,
            "session-1".into(),
            Some("turn-1".into()),
            Some(PathBuf::from("/tmp/./nested/../rollout.jsonl")),
            PathBuf::from("/work/./repo/../codexctl"),
        )
        .unwrap();
        assert_eq!(identity.provider(), AgentProvider::Claude);
        assert_eq!(identity.cwd(), Path::new("/work/codexctl"));
        assert_eq!(
            identity.transcript_path(),
            Some(Path::new("/tmp/rollout.jsonl"))
        );
    }

    #[test]
    fn permission_constructor_requires_a_turn_bearing_validated_identity() {
        let identity = LifecycleIdentity::try_new(
            AgentProvider::Codex,
            "session-1".into(),
            None,
            None,
            PathBuf::from("/work"),
        )
        .unwrap();
        assert_eq!(
            LifecycleEvent::permission(identity, PermissionDisposition::Decided).unwrap_err(),
            LifecycleInputError::Missing("turn_id")
        );
    }
}
