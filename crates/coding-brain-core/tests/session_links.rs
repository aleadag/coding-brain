use coding_brain_core::provider::{AgentProvider, AgentSessionKey, LiveProcessIdentity};
use coding_brain_core::session_links::{
    SESSION_IDENTITY_LINK_SCHEMA_VERSION, SessionIdentityLink, SessionLinkLimits, SessionLinkStore,
};

fn live(provider: AgentProvider, pid: u32) -> LiveProcessIdentity {
    LiveProcessIdentity::try_new(provider, pid, 9_001, format!("/dev/pts/{pid}")).unwrap()
}

#[test]
fn session_links_rebuild_native_process_aliases_from_append_only_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
    let live_process = live(AgentProvider::Antigravity, 42);
    store
        .append(SessionIdentityLink {
            schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
            recorded_at_ms: 1_000,
            provider: AgentProvider::Antigravity,
            native_session_id: "conversation-7".into(),
            live_process: live_process.clone(),
        })
        .unwrap();

    let projection = store.read_projection().unwrap();

    assert_eq!(projection.native_for(&live_process), Some("conversation-7"));
    assert_eq!(
        projection.live_for(&AgentSessionKey::native(
            AgentProvider::Antigravity,
            "conversation-7"
        )),
        Some(&live_process)
    );
}

#[test]
fn session_links_resolve_conflicting_aliases_to_the_newest_consistent_link() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
    let old_live = live(AgentProvider::Claude, 42);
    let new_live = live(AgentProvider::Claude, 43);
    for (recorded_at_ms, live_process) in [(1_000, old_live.clone()), (2_000, new_live.clone())] {
        store
            .append(SessionIdentityLink {
                schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                recorded_at_ms,
                provider: AgentProvider::Claude,
                native_session_id: "session-7".into(),
                live_process,
            })
            .unwrap();
    }

    let projection = store.read_projection().unwrap();
    let native = AgentSessionKey::native(AgentProvider::Claude, "session-7");
    assert_eq!(projection.live_for(&native), Some(&new_live));
    assert_eq!(projection.native_for(&old_live), None);
    assert_eq!(projection.native_for(&new_live), Some("session-7"));
}

#[test]
fn session_links_reject_mismatched_provider_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionLinkStore::at(temp.path().join("session-links.jsonl"));

    assert!(
        store
            .append(SessionIdentityLink {
                schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
                recorded_at_ms: 1_000,
                provider: AgentProvider::Claude,
                native_session_id: "session-7".into(),
                live_process: live(AgentProvider::Codex, 42),
            })
            .is_err()
    );
}

#[test]
fn session_links_reject_incomplete_or_noncanonical_public_live_identity() {
    let temp = tempfile::tempdir().unwrap();
    let store = SessionLinkStore::at(temp.path().join("session-links.jsonl"));
    let base = SessionIdentityLink {
        schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
        recorded_at_ms: 1_000,
        provider: AgentProvider::Claude,
        native_session_id: "session-7".into(),
        live_process: live(AgentProvider::Claude, 42),
    };
    let mut invalid = Vec::new();
    let mut zero_pid = base.clone();
    zero_pid.live_process.pid = 0;
    invalid.push(zero_pid);
    let mut zero_start = base.clone();
    zero_start.live_process.process_start_identity = 0;
    invalid.push(zero_start);
    let mut unusable_tty = base.clone();
    unusable_tty.live_process.tty = "?".into();
    invalid.push(unusable_tty);
    let mut noncanonical_tty = base;
    noncanonical_tty.live_process.tty = "/dev/pts/42".into();
    invalid.push(noncanonical_tty);

    for link in invalid {
        assert!(store.append(link).is_err());
    }
}

#[test]
fn session_links_abstain_on_complete_corrupt_or_unsupported_rows() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session-links.jsonl");
    let store = SessionLinkStore::at(&path);
    let valid = SessionIdentityLink {
        schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
        recorded_at_ms: 1_000,
        provider: AgentProvider::Claude,
        native_session_id: "session-7".into(),
        live_process: live(AgentProvider::Claude, 42),
    };
    let valid_row = serde_json::to_string(&valid).unwrap();

    for bad_row in [
        "{not-json}".to_string(),
        serde_json::to_string(&SessionIdentityLink {
            schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION + 1,
            ..valid.clone()
        })
        .unwrap(),
        serde_json::to_string(&SessionIdentityLink {
            live_process: LiveProcessIdentity {
                pid: 0,
                ..valid.live_process.clone()
            },
            ..valid.clone()
        })
        .unwrap(),
        format!("{{\"padding\":\"{}\"}}", "x".repeat(65 * 1024)),
    ] {
        std::fs::write(&path, format!("{valid_row}\n{bad_row}\n")).unwrap();
        assert!(store.read_projection().is_err());
    }
}

#[test]
fn session_links_compaction_retains_the_newest_unique_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session-links.jsonl");
    let store = SessionLinkStore::at(&path).with_limits(SessionLinkLimits {
        lock_timeout_ms: 100,
        compact_at_bytes: 1,
        retained_links: 2,
    });
    let first = SessionIdentityLink {
        schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
        recorded_at_ms: 1_000,
        provider: AgentProvider::Claude,
        native_session_id: "session-1".into(),
        live_process: live(AgentProvider::Claude, 41),
    };
    for link in [
        first.clone(),
        first,
        SessionIdentityLink {
            schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
            recorded_at_ms: 2_000,
            provider: AgentProvider::Claude,
            native_session_id: "session-2".into(),
            live_process: live(AgentProvider::Claude, 42),
        },
        SessionIdentityLink {
            schema_version: SESSION_IDENTITY_LINK_SCHEMA_VERSION,
            recorded_at_ms: 3_000,
            provider: AgentProvider::Claude,
            native_session_id: "session-3".into(),
            live_process: live(AgentProvider::Claude, 43),
        },
    ] {
        store.append(link).unwrap();
    }

    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    let projection = store.read_projection().unwrap();
    assert_eq!(
        projection.native_for(&live(AgentProvider::Claude, 41)),
        None
    );
    assert_eq!(
        projection.native_for(&live(AgentProvider::Claude, 42)),
        Some("session-2")
    );
    assert_eq!(
        projection.native_for(&live(AgentProvider::Claude, 43)),
        Some("session-3")
    );
}
