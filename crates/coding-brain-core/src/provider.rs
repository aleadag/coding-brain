use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    #[default]
    Codex,
    Claude,
    Antigravity,
}

impl AgentProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Antigravity => "antigravity",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::Antigravity => "Antigravity",
        }
    }

    pub(crate) const fn supports_structured_discovery(self) -> bool {
        matches!(self, Self::Codex | Self::Claude)
    }

    pub(crate) const fn supports_native_attach(self) -> bool {
        matches!(self, Self::Claude)
    }

    fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "antigravity" => Some(Self::Antigravity),
            _ => None,
        }
    }
}

impl fmt::Display for AgentProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentSessionKey {
    #[serde(default)]
    pub provider: AgentProvider,
    pub session_id: String,
}

impl AgentSessionKey {
    pub fn native(provider: AgentProvider, session_id: impl Into<String>) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
        }
    }

    pub fn storage_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.provider.as_str(),
            self.session_id.len(),
            self.session_id
        )
    }

    pub fn from_storage_key(value: &str) -> Option<Self> {
        let (provider, remainder) = value.split_once(':')?;
        let provider = AgentProvider::from_storage_name(provider)?;
        let (length, session_id) = remainder.split_once(':')?;
        let parsed_length = length.parse::<usize>().ok()?;
        if length != parsed_length.to_string() || session_id.len() != parsed_length {
            return None;
        }
        Some(Self::native(provider, session_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LiveProcessIdentity {
    pub provider: AgentProvider,
    pub pid: u32,
    pub process_start_identity: u64,
    pub tty: String,
}

impl LiveProcessIdentity {
    pub fn try_new(
        provider: AgentProvider,
        pid: u32,
        process_start_identity: u64,
        tty: impl AsRef<str>,
    ) -> Option<Self> {
        if pid == 0 || process_start_identity == 0 {
            return None;
        }
        let tty = normalize_tty(tty.as_ref())?;
        Some(Self {
            provider,
            pid,
            process_start_identity,
            tty,
        })
    }

    pub fn matches(&self, pid: u32, process_start_identity: u64, tty: &str) -> bool {
        self.pid == pid
            && self.process_start_identity == process_start_identity
            && normalize_tty(tty).is_some_and(|tty| tty == self.tty)
    }

    pub fn matches_provider(
        &self,
        provider: AgentProvider,
        pid: u32,
        process_start_identity: u64,
        tty: &str,
    ) -> bool {
        self.provider == provider && self.matches(pid, process_start_identity, tty)
    }
}

fn normalize_tty(tty: &str) -> Option<String> {
    let tty = tty.trim();
    let tty = tty.strip_prefix("/dev/").unwrap_or(tty);
    (!tty.is_empty() && !matches!(tty, "?" | "??" | "-" | "none")).then(|| tty.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_keys_do_not_collide() {
        let codex = AgentSessionKey::native(AgentProvider::Codex, "same-id");
        let claude = AgentSessionKey::native(AgentProvider::Claude, "same-id");
        assert_ne!(codex, claude);
    }

    #[test]
    fn live_identity_expires_when_process_evidence_changes() {
        let original =
            LiveProcessIdentity::try_new(AgentProvider::Antigravity, 42, 9001, "/dev/pts/7")
                .unwrap();
        assert!(original.matches(42, 9001, "pts/7"));
        assert!(!original.matches(42, 9002, "pts/7"));
        assert!(!original.matches(42, 9001, "pts/8"));
    }

    #[test]
    fn missing_provider_deserializes_as_codex() {
        let key: AgentSessionKey = serde_json::from_str(r#"{"session_id":"legacy"}"#).unwrap();
        assert_eq!(key.provider, AgentProvider::Codex);
    }

    #[test]
    fn storage_key_roundtrips_ids_containing_colons() {
        let key = AgentSessionKey::native(AgentProvider::Claude, "workspace:agent:42");
        assert_eq!(
            AgentSessionKey::from_storage_key(&key.storage_key()).unwrap(),
            key
        );
    }

    #[test]
    fn provider_serialization_and_labels_are_stable() {
        for (provider, serialized, label) in [
            (AgentProvider::Codex, r#""codex""#, "Codex"),
            (AgentProvider::Claude, r#""claude""#, "Claude"),
            (
                AgentProvider::Antigravity,
                r#""antigravity""#,
                "Antigravity",
            ),
        ] {
            assert_eq!(serde_json::to_string(&provider).unwrap(), serialized);
            assert_eq!(provider.label(), label);
        }
    }

    #[test]
    fn storage_key_uses_utf8_byte_length_and_rejects_malformed_input() {
        let key = AgentSessionKey::native(AgentProvider::Claude, "é:agent");
        assert_eq!(key.storage_key(), "claude:8:é:agent");
        assert_eq!(
            AgentSessionKey::from_storage_key(&key.storage_key()),
            Some(key)
        );

        for malformed in [
            "unknown:2:id",
            "codex:x:id",
            "codex:3:id",
            "codex:2:id:extra",
            "codex:2",
        ] {
            assert_eq!(AgentSessionKey::from_storage_key(malformed), None);
        }
    }

    #[test]
    fn storage_keys_are_injective_for_separator_heavy_ids() {
        let short = AgentSessionKey::native(AgentProvider::Codex, "1:a:b");
        let long = AgentSessionKey::native(AgentProvider::Codex, "a:b");
        assert_ne!(short.storage_key(), long.storage_key());
    }

    #[test]
    fn live_identity_rejects_invalid_evidence_and_provider_mismatch() {
        assert!(LiveProcessIdentity::try_new(AgentProvider::Codex, 0, 1, "pts/1").is_none());
        assert!(LiveProcessIdentity::try_new(AgentProvider::Codex, 1, 0, "pts/1").is_none());
        assert!(LiveProcessIdentity::try_new(AgentProvider::Codex, 1, 1, "/dev/").is_none());
        assert!(LiveProcessIdentity::try_new(AgentProvider::Codex, 1, 1, "?").is_none());

        let identity =
            LiveProcessIdentity::try_new(AgentProvider::Codex, 1, 1, "/dev/pts/1").unwrap();
        assert!(identity.matches_provider(AgentProvider::Codex, 1, 1, "pts/1"));
        assert!(!identity.matches_provider(AgentProvider::Claude, 1, 1, "pts/1"));
    }
}
