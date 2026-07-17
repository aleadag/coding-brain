use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::project::ProjectId;

pub const ACTIVITY_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_INTERRUPTED_AFTER_MS: u64 = 30_000;
pub const MAX_ACTIVITY_EVENT_BYTES: usize = 64 * 1024;
pub const MAX_ACTIVITY_FIELD_BYTES: usize = 4_096;
pub const MAX_PROVIDER_HINTS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEvidence {
    pub project_id: ProjectId,
    #[serde(with = "path_serde")]
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTarget {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    pub project_id: ProjectId,
    #[serde(with = "path_serde")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub provider_hints: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Observed,
    Evaluating,
    Allowed,
    Denied,
    Abstained,
    Error,
    Delivered,
    DeliveryFailed,
    Outcome,
    Correction,
    Interrupted,
}

impl ActivityState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Allowed | Self::Denied | Self::Abstained | Self::Error
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Unknown,
    Delivered,
    Failed,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionDisposition {
    BrainRight,
    BrainWrong,
    Exception,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub schema_version: u32,
    pub activity_id: String,
    pub recorded_at_ms: u64,
    pub project: ProjectEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionTarget>,
    pub state: ActivityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normalized_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ActivityOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<CorrectionDisposition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

impl ActivityEvent {
    pub fn normalized(mut self) -> Self {
        self.activity_id = bounded(&self.activity_id, false);
        normalize_project_id(&mut self.project.project_id);
        self.project.label = self.project.label.map(|value| bounded(&value, true));
        if let Some(session) = &mut self.session {
            session.session_id = bounded(&session.session_id, false);
            session.turn_id = session.turn_id.take().map(|value| bounded(&value, false));
            session.tool_use_id = session
                .tool_use_id
                .take()
                .map(|value| bounded(&value, false));
            normalize_project_id(&mut session.project_id);
            session.provider_hints.truncate(MAX_PROVIDER_HINTS);
            session.provider_hints = session
                .provider_hints
                .drain(..)
                .map(|value| bounded(&value, true))
                .collect();
        }
        self.tool = self.tool.map(|value| bounded(&value, false));
        self.normalized_command = self.normalized_command.map(|value| bounded(&value, true));
        self.fingerprint = self.fingerprint.map(|value| bounded(&value, false));
        self.rule_id = self.rule_id.map(|value| bounded(&value, false));
        self.reasoning = self.reasoning.map(|value| bounded(&value, true));
        self.decision_id = self.decision_id.map(|value| bounded(&value, false));
        self.note = self.note.map(|value| bounded(&value, true));
        self.supersedes = self.supersedes.map(|value| bounded(&value, false));
        self
    }

    pub fn has_consistent_payload(&self) -> bool {
        match self.state {
            ActivityState::Outcome => {
                self.outcome.is_some() && self.correction.is_none() && self.note.is_none()
            }
            ActivityState::Correction => self.outcome.is_none() && self.correction.is_some(),
            _ => self.outcome.is_none() && self.correction.is_none() && self.note.is_none(),
        }
    }
}

fn normalize_project_id(project_id: &mut ProjectId) {
    let value = match project_id {
        ProjectId::Stable(value) | ProjectId::Temporary(value) => value,
    };
    *value = bounded(value, false);
}

fn bounded(value: &str, redact: bool) -> String {
    let value = if redact {
        redact_activity_text(value)
    } else {
        value.to_owned()
    };
    if value.len() <= MAX_ACTIVITY_FIELD_BYTES {
        return value;
    }
    let mut end = MAX_ACTIVITY_FIELD_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub fn redact_activity_text(value: &str) -> String {
    let mut redact_next = false;
    value
        .split_whitespace()
        .map(|token| {
            let normalized = token
                .trim_matches(|character: char| {
                    !character.is_ascii_alphanumeric() && character != '_' && character != '-'
                })
                .to_ascii_lowercase();
            if redact_next {
                if matches!(token.trim_matches(['\'', '\"']), "=" | ":") {
                    return token.to_owned();
                }
                redact_next = false;
                return "[REDACTED]".to_owned();
            }
            if secret_assignment(token) || secret_prefix(&normalized) {
                return "[REDACTED]".to_owned();
            }
            if matches!(normalized.as_str(), "bearer" | "basic")
                || normalized.ends_with("authorization:bearer")
                || normalized.ends_with("authorization:basic")
                || is_secret_key(normalized.trim_start_matches('-'))
            {
                redact_next = true;
                return token.to_owned();
            }
            token.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn secret_assignment(token: &str) -> bool {
    let Some((key, _)) = token.split_once('=') else {
        return false;
    };
    let key = key
        .rsplit(|character: char| {
            !character.is_ascii_alphanumeric() && character != '_' && character != '-'
        })
        .next()
        .unwrap_or(key)
        .to_ascii_lowercase();
    is_secret_key(&key)
}

fn is_secret_key(key: &str) -> bool {
    let key = key
        .trim_start_matches('-')
        .replace('-', "_")
        .to_ascii_lowercase();
    ["token", "password", "passwd", "secret", "api_key", "apikey"]
        .iter()
        .any(|candidate| key == *candidate || key.ends_with(&format!("_{candidate}")))
        || key.split('_').any(|segment| segment == "secret")
}

fn secret_prefix(value: &str) -> bool {
    value.starts_with("sk-")
        || value.starts_with("ghp_")
        || value.starts_with("github_pat_")
        || value.starts_with("xoxb-")
        || value.starts_with("xoxa-")
        || value.starts_with("xoxp-")
        || value.starts_with("xoxr-")
        || value.starts_with("xoxs-")
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActivityItem {
    pub activity_id: String,
    pub recorded_at_ms: u64,
    pub project: ProjectEvidence,
    pub session: Option<SessionTarget>,
    pub state: ActivityState,
    pub delivery: DeliveryState,
    pub tool: Option<String>,
    pub normalized_command: Option<String>,
    pub fingerprint: Option<String>,
    pub rule_id: Option<String>,
    pub confidence: Option<f64>,
    pub threshold: Option<f64>,
    pub reasoning: Option<String>,
    pub decision_id: Option<String>,
    pub outcome: Option<ActivityOutcome>,
    pub correction: Option<CorrectionDisposition>,
    pub note: Option<String>,
    pub tool_execution_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttentionItem {
    pub activity: ActivityItem,
    pub occurrences: usize,
    pub unresolved_occurrences: usize,
}

impl std::ops::Deref for AttentionItem {
    type Target = ActivityItem;

    fn deref(&self) -> &Self::Target {
        &self.activity
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityDiagnostics {
    pub malformed_rows: usize,
    pub malformed_offsets: Vec<u64>,
    pub duplicate_terminal_states: usize,
    pub truncated_tails: usize,
    pub discarded_tail_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivitySnapshot {
    pub attention: Vec<AttentionItem>,
    pub recent: Vec<ActivityItem>,
    pub unresolved_count: usize,
    pub diagnostics: ActivityDiagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotLimits {
    pub attention: usize,
    pub recent: usize,
    pub interrupted_after_ms: u64,
}

impl Default for SnapshotLimits {
    fn default() -> Self {
        Self {
            attention: 100,
            recent: 100,
            interrupted_after_ms: DEFAULT_INTERRUPTED_AFTER_MS,
        }
    }
}

mod path_serde {
    use std::fmt;
    use std::path::{Path, PathBuf};

    use serde::de::{SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(path) = path.to_str() {
            return serializer.serialize_str(path);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            serializer.serialize_bytes(path.as_os_str().as_bytes())
        }
        #[cfg(not(unix))]
        {
            Err(serde::ser::Error::custom("path is not valid Unicode"))
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PathVisitor)
    }

    struct PathVisitor;

    impl<'de> Visitor<'de> for PathVisitor {
        type Value = PathBuf;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a UTF-8 path or an array of opaque path bytes")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
            Ok(PathBuf::from(value))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
            Ok(PathBuf::from(value))
        }

        #[cfg(unix)]
        fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            Ok(PathBuf::from(OsString::from_vec(value.to_vec())))
        }

        #[cfg(unix)]
        fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            Ok(PathBuf::from(OsString::from_vec(value)))
        }

        #[cfg(unix)]
        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let mut bytes = Vec::new();
            while let Some(byte) = sequence.next_element::<u8>()? {
                bytes.push(byte);
            }
            Ok(PathBuf::from(OsString::from_vec(bytes)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(command: &str, reasoning: &str, note: &str) -> ActivityEvent {
        ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            activity_id: "activity-1".into(),
            recorded_at_ms: 1,
            project: ProjectEvidence {
                project_id: ProjectId::Temporary("temporary-1".into()),
                cwd: PathBuf::from("/work/project"),
                label: Some("project".into()),
            },
            session: None,
            state: ActivityState::Denied,
            tool: Some("Bash".into()),
            normalized_command: Some(command.into()),
            fingerprint: Some("fingerprint".into()),
            rule_id: Some("destructive".into()),
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some(reasoning.into()),
            decision_id: Some("decision-1".into()),
            outcome: None,
            correction: None,
            note: Some(note.into()),
            supersedes: None,
        }
    }

    #[test]
    fn bounds_and_redacts_persisted_text() {
        let normalized = event(
            "curl -H 'Authorization: Bearer sk-secret-value' https://example.test",
            &"r".repeat(MAX_ACTIVITY_FIELD_BYTES + 100),
            "token=private-value",
        )
        .normalized();
        let serialized = serde_json::to_string(&normalized).unwrap();
        assert!(!serialized.contains("sk-secret-value"));
        assert!(!serialized.contains("private-value"));
        assert_eq!(
            normalized.reasoning.as_ref().unwrap().len(),
            MAX_ACTIVITY_FIELD_BYTES
        );
    }

    #[test]
    fn only_evaluation_decisions_are_terminal() {
        assert!(ActivityState::Allowed.is_terminal());
        assert!(ActivityState::Denied.is_terminal());
        assert!(ActivityState::Abstained.is_terminal());
        assert!(ActivityState::Error.is_terminal());
        assert!(!ActivityState::Delivered.is_terminal());
        assert!(!ActivityState::Outcome.is_terminal());
    }

    #[test]
    fn normalization_bounds_nested_context_and_url_secrets() {
        let mut activity = event(
            "curl https://example.test?token=private-value",
            "reason",
            "note",
        );
        activity.project.project_id = ProjectId::Temporary("p".repeat(5_000));
        activity.project.cwd = PathBuf::from(format!("/{}", "c".repeat(5_000)));
        activity.session = Some(SessionTarget {
            session_id: "s".repeat(5_000),
            turn_id: Some("t".repeat(5_000)),
            tool_use_id: Some("u".repeat(5_000)),
            project_id: ProjectId::Temporary("q".repeat(5_000)),
            cwd: PathBuf::from(format!("/{}", "d".repeat(5_000))),
            provider_hints: (0..100).map(|index| format!("hint-{index}")).collect(),
        });

        let normalized = activity.normalized();
        let serialized = serde_json::to_string(&normalized).unwrap();
        assert!(!serialized.contains("private-value"));
        assert!(matches!(
            normalized.project.project_id,
            ProjectId::Temporary(ref value) if value.len() <= MAX_ACTIVITY_FIELD_BYTES
        ));
        let session = normalized.session.unwrap();
        assert!(session.session_id.len() <= MAX_ACTIVITY_FIELD_BYTES);
        assert_eq!(session.provider_hints.len(), MAX_PROVIDER_HINTS);
    }

    #[test]
    fn redacts_separated_flags_and_compact_authorization_headers() {
        let normalized = event(
            "tool --password hunter2 --token opaque Authorization:Bearer third-secret --api-key sixth-secret Authorization:Basic ninth-secret",
            "password = fourth-secret --api-key=seventh-secret AWS_SECRET_ACCESS_KEY=tenth-secret",
            "--api_key fifth-secret --client-secret eighth-secret",
        )
        .normalized();
        let serialized = serde_json::to_string(&normalized).unwrap();
        for secret in [
            "hunter2",
            "opaque",
            "third-secret",
            "fourth-secret",
            "fifth-secret",
            "sixth-secret",
            "seventh-secret",
            "eighth-secret",
            "ninth-secret",
            "tenth-secret",
        ] {
            assert!(!serialized.contains(secret), "leaked {secret}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn normalization_preserves_opaque_non_utf8_navigation_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let opaque = PathBuf::from(OsString::from_vec(vec![b'/', b'w', 0x80]));
        let mut activity = event("command", "reason", "note");
        activity.project.cwd = opaque.clone();
        activity.session = Some(SessionTarget {
            session_id: "session".into(),
            turn_id: Some("turn".into()),
            tool_use_id: Some("tool-use".into()),
            project_id: ProjectId::Temporary("project".into()),
            cwd: opaque.clone(),
            provider_hints: Vec::new(),
        });
        let normalized = activity.normalized();
        assert_eq!(normalized.project.cwd, opaque);
        assert_eq!(
            normalized.session.as_ref().unwrap().cwd,
            normalized.project.cwd
        );
        let encoded = serde_json::to_vec(&normalized).unwrap();
        let decoded: ActivityEvent = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.project.cwd, normalized.project.cwd);
        assert_eq!(decoded.session.unwrap().cwd, normalized.project.cwd);
    }
}
