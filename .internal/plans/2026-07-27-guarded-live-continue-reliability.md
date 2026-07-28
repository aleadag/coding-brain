# Guarded Live Continue Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make Live expose only currently recognized semantic session actions, preserve authoritative guarded dispatch, and retain safe manual and automatic failure evidence in Diagnostics.

**Architecture:** Core terminal code supplies typed target, prompt, send, and delivery-certainty results. The runtime independently resolves exact sessions for preflight and dispatch, persists safe diagnostics, and exposes typed capabilities to an asynchronous TUI worker; automatic recovery returns typed outcomes and deduplicates durable diagnostics without changing reservations or polling.

**Tech Stack:** Rust 2024 workspace, Ratatui 0.29, bounded tmux terminal integration, append-only JSONL activity storage, Cargo tests through the Nix flake.

## Global Constraints

- Preflight is advisory only and must not return or retain authority-bearing target handles, prompt fingerprints, or pane content.
- Dispatch must independently repeat provider-qualified session discovery, exact process validation, unique-pane resolution, bounded capture, prompt matching, recapture equality, fixed semantic input, and post-send advancement.
- `NotSent` applies only before any backend send call; after a send call is attempted, failure certainty is `DeliveryUnknown`.
- Never persist terminal captures, prompt fragments, raw manual text, or secret-bearing backend detail.
- Manual text remains hidden, single-line, control-character-free, and bounded to 4096 bytes.
- Preserve automatic recovery mode, adaptive threshold, stable-evidence checks, ten-second reservation/cooldown, cross-process deduplication, postflight checks, two-worker limit, 64-item queue, and polling cadence.
- Preserve the activity store's 100 ms production lock bound and 32 MiB compaction threshold.
- Diagnostic activity stays excluded from Live attention, Review, Scorecard, and learning.
- Do not add a live-tmux requirement to CI; use fixture-backed core tests and injected runtime/coordinator seams.
- Do not commit, push, or publish under the conservative repository profile without explicit user authorization.

## Beads Tracking

Reuse the existing parent and child tasks; do not create duplicate execution
beads:

- Parent: `codexctl-vlwz`
- Task 1: `codexctl-c1ll`
- Task 2: `codexctl-cw2k` (blocked by Task 1)
- Task 3: `codexctl-spib` (blocked by Task 2)
- Task 4: `codexctl-p9n2` (blocked by Task 1)
- Task 5: `codexctl-dclz` (blocked by Tasks 3 and 4)

---

### Task 1: Type guarded prompt preflight and delivery failures

**Files:**
- Modify: `crates/coding-brain-core/src/terminals/mod.rs:45-115`
- Modify: `crates/coding-brain-core/src/terminals/mod.rs:1981-2292`
- Modify: `crates/coding-brain-core/src/terminals/mod.rs:2470-3078`
- Modify: `crates/coding-brain-core/src/terminals/tmux.rs:1-217`
- Modify: `src/brain/recovery.rs:1065-1070` (direct certainty consumer only)
- Test: `crates/coding-brain-core/src/terminals/mod.rs`
- Test: `crates/coding-brain-core/src/terminals/tmux.rs`

**Interfaces:**
- Consumes: `AgentSession`, `TerminalSessionAction`, `PromptEvidence`, `PaneCapture`, existing provider prompt parsers, and `TmuxGuardedBackend`.
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExactTargetFailureKind {
    Unavailable,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactTargetFailure {
    pub kind: ExactTargetFailureKind,
    detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedActionFailureCategory {
    LiveProcessUnavailable,
    ExactTargetUnavailable,
    ExactTargetAmbiguous,
    CaptureUnavailable,
    PromptUnrecognized,
    PromptChanged,
    SendFailed,
    PostflightUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionablePrompt {
    Allow,
    Deny,
    Continue,
}

impl GuardedActionFailureCategory {
    pub fn rule_suffix(self) -> &'static str;
    pub fn safe_message(self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryCertainty {
    NotSent,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedActionFailure {
    pub category: GuardedActionFailureCategory,
    pub certainty: DeliveryCertainty,
    detail: String,
}

pub fn probe_actionable_prompt_classified(
    session: &AgentSession,
) -> Result<Option<ActionablePrompt>, GuardedActionFailure>;
```

**Acceptance Criteria:**
- Exact tmux target absence and ambiguity are typed at `select_exact_pane`, not inferred from strings.
- A successful actionable probe proves exact live identity, unique target, and bounded capture; no semantic prompt returns `Ok(None)`.
- Classified preflight exposes only `ActionablePrompt`; authority-bearing `PromptEvidence` remains private to the legacy compatibility wrapper.
- Initial target/capture/prompt failures are `NotSent`.
- Any `send_literal` or `send_keys` error is `DeliveryUnknown`, including the first Codex literal send attempt.
- Post-send capture/backend/prompt-advancement failures are `DeliveryUnknown`.
- Existing successful Codex, Claude, and Antigravity semantic actions remain byte-for-byte unchanged.

- [ ] **Step 1: Write failing typed-target and delivery-certainty tests**

Add tests that assert the source-level classifications:

```rust
#[test]
fn exact_tmux_target_distinguishes_absent_and_ambiguous() {
    let identity =
        LiveProcessIdentity::try_new(AgentProvider::Codex, 42, 99, "pts/1").unwrap();
    assert_eq!(
        select_exact_pane(&identity, &[], |_| None).unwrap_err().kind,
        ExactTargetFailureKind::Unavailable
    );
    let panes = parse_panes("%1\t/dev/pts/1\t10\n%2\t/dev/pts/1\t11\n").unwrap();
    assert_eq!(
        select_exact_pane(&identity, &panes, |pid| match pid {
            42 => Some(11),
            11 => Some(10),
            _ => None,
        })
            .unwrap_err()
            .kind,
        ExactTargetFailureKind::Ambiguous
    );
}

#[test]
fn actionable_probe_allows_manual_only_when_no_semantic_prompt_is_present() {
    let backend = FakeGuardedBackend::with_captures([Ok(guarded_capture(
        "ordinary terminal output",
    ))]);
    let result = probe_actionable_prompt_classified_with(
        &guarded_session(AgentProvider::Codex),
        &backend,
    )
    .unwrap();
    assert_eq!(result, None);
}

#[test]
fn every_semantic_and_manual_send_call_failure_is_delivery_unknown_and_redacted() {
    for fixture in send_failure_fixtures() {
        let failure = execute_guarded_action_classified_with(
            &fixture.session,
            fixture.action,
            &fixture.backend,
        )
        .unwrap_err();
        assert_eq!(failure.category, GuardedActionFailureCategory::SendFailed);
        assert_eq!(failure.certainty, DeliveryCertainty::Unknown);
        assert!(!format!("{failure:?}").contains("top-secret-literal"));
    }
}
```

`send_failure_fixtures` covers semantic and manual `send_literal` and
`send_keys` failures. It supplies enough target/capture responses to reach each
send call and uses `top-secret-literal` for manual cases.

- [ ] **Step 2: Run the new tests and confirm the red state**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core exact_tmux_target_distinguishes_absent_and_ambiguous -- --nocapture
nix develop path:. --command cargo test -p coding-brain-core actionable_probe_allows_manual_only_when_no_semantic_prompt_is_present -- --nocapture
nix develop path:. --command cargo test -p coding-brain-core every_semantic_and_manual_send_call_failure_is_delivery_unknown_and_redacted -- --nocapture
```

Expected: compilation fails because the typed failure APIs do not exist.

- [ ] **Step 3: Implement typed target and action failures**

Replace `GuardedActionFailure`'s string variants with the produced types above. Add constructors that preserve bounded internal detail while fixing certainty at each call site:

```rust
impl GuardedActionFailure {
    fn not_sent(category: GuardedActionFailureCategory, detail: impl Into<String>) -> Self {
        Self {
            category,
            certainty: DeliveryCertainty::NotSent,
            detail: detail.into(),
        }
    }

    fn unknown(category: GuardedActionFailureCategory, detail: impl Into<String>) -> Self {
        Self {
            category,
            certainty: DeliveryCertainty::Unknown,
            detail: detail.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.detail
    }

    fn from_exact_target(error: ExactTargetFailure) -> Self {
        let category = match error.kind {
            ExactTargetFailureKind::Unavailable => {
                GuardedActionFailureCategory::ExactTargetUnavailable
            }
            ExactTargetFailureKind::Ambiguous => {
                GuardedActionFailureCategory::ExactTargetAmbiguous
            }
        };
        Self::not_sent(category, error.detail)
    }
}
```

Change `GuardedTerminalBackend::resolve_exact_target` to return
`ExactTargetFailure`. Add `tmux::resolve_exact_target_classified` with that
typed result, while preserving the existing string-returning
`tmux::resolve_exact_target` as a compatibility wrapper for navigation, focus,
and other non-guarded callers. Make `focus_exact_terminal` use the compatibility
wrapper rather than the guarded backend trait. Map process, command, parse, and
missing-pane failures to `Unavailable`; map multiple exact panes to
`Ambiguous`. Add `ExactTargetFailure::unavailable(detail)` and
`ExactTargetFailure::ambiguous(detail)` constructors beside the type so tmux
never constructs or classifies failures by parsing display text.

Add `probe_actionable_prompt_classified_with`. It resolves and captures once,
then tries Continue, Allow, and Deny. A successful capture with no recognized
semantic prompt returns `Ok(None)`:

```rust
fn probe_actionable_prompt_classified_with(
    session: &AgentSession,
    backend: &dyn GuardedTerminalBackend,
) -> Result<Option<ActionablePrompt>, GuardedActionFailure> {
    session.live_process_identity().ok_or_else(|| {
        GuardedActionFailure::not_sent(
            GuardedActionFailureCategory::LiveProcessUnavailable,
            "actionable prompt probe requires an exact live process identity",
        )
    })?;
    let target = backend
        .resolve_exact_target(session)
        .map_err(GuardedActionFailure::from_exact_target)?;
    let capture = checked_target_capture(backend, &target).map_err(|detail| {
        GuardedActionFailure::not_sent(
            GuardedActionFailureCategory::CaptureUnavailable,
            detail,
        )
    })?;
    Ok([
        TerminalSessionAction::Continue,
        TerminalSessionAction::Allow,
        TerminalSessionAction::Deny,
    ]
    .into_iter()
    .find_map(|action| match_semantic_prompt(&capture, session, &action))
    .map(|evidence| match evidence.action {
        TerminalSessionAction::Allow => ActionablePrompt::Allow,
        TerminalSessionAction::Deny => ActionablePrompt::Deny,
        TerminalSessionAction::Continue => ActionablePrompt::Continue,
        TerminalSessionAction::Text(_) => unreachable!(),
    }))
}
```

Keep the existing string-returning `execute_guarded_action`,
`probe_recovery_prompt`, `probe_actionable_prompt`, and exact-target APIs as
compatibility wrappers. Update classified execution so every backend send-call
error uses `GuardedActionFailure::unknown` with `SendFailed`, including manual
text's first literal call and every semantic or manual key call.

Before manual text sends, repeat bounded target capture and require the same
target/backend. Compare post-send advancement against this fresh pre-send
capture, not the earlier initial capture. Update the direct recovery failure
adapter to match `DeliveryCertainty`; leave Task 4's broader recovery taxonomy
for Task 4.

- [ ] **Step 4: Run core terminal and tmux tests**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core terminals::tmux::tests -- --nocapture
nix develop path:. --command cargo test -p coding-brain-core terminals::tests -- --nocapture
```

Expected: all terminal tests pass, including unchanged exact-prompt and zero-send race regressions.

- [ ] **Step 5: Review the task diff**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git diff -- crates/coding-brain-core/src/terminals/mod.rs crates/coding-brain-core/src/terminals/tmux.rs
```

Expected: only typed terminal failures, classified preflight, and directly related tests changed. Do not commit without explicit authorization.

---

### Task 2: Add runtime preflight, capabilities, and manual diagnostics

**Files:**
- Modify: `crates/coding-brain-core/src/runtime.rs:215-263`
- Modify: `crates/coding-brain-core/src/runtime.rs:383-546`
- Modify: `src/runtime/brain.rs:375-478`
- Modify: `src/runtime/brain.rs:651-699`
- Modify: `crates/coding-brain-tui/src/brain_app.rs` (direct compile-only consumer)
- Test: `crates/coding-brain-core/src/runtime.rs`
- Test: `src/runtime/brain.rs`

**Interfaces:**
- Consumes: Task 1's `probe_actionable_prompt_classified`,
  `ActionablePrompt`,
  `GuardedActionFailureCategory`, `DeliveryCertainty`, existing
  `action_target_matches`, session-link projection, and `ActivityStore`.
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionCapability {
    Allow,
    Deny,
    Continue,
    ManualText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActionFailureCategory {
    AuthorityUnavailable,
    ExactSessionUnavailable,
    ExactSessionAmbiguous,
    Guarded(GuardedActionFailureCategory),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionFailure {
    pub category: SessionActionFailureCategory,
    pub diagnostic_persisted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionTarget {
    pub provider: AgentProvider,
    pub session_id: String,
    pub project_id: ProjectId,
    pub cwd: PathBuf,
    pub provenance: SessionTargetProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionAttempt {
    pub attempt_id: String,
    pub target: SessionActionTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionPreflightRequest {
    pub attempt: SessionActionAttempt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionAvailability {
    pub attempt: SessionActionAttempt,
    pub capabilities: Vec<SessionActionCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionRequest {
    pub attempt: SessionActionAttempt,
    pub action: TerminalSessionAction,
}
```

`BrainActions` gains:

```rust
fn preflight_session_action(
    &self,
    request: SessionActionPreflightRequest,
) -> Result<SessionActionAvailability, SessionActionFailure>;

fn send_session_action(
    &self,
    request: SessionActionRequest,
) -> Result<(), SessionActionFailure>;
```

`SessionActionFailure` provides only safe operator/persistence text. Task 1's
core category owns the guarded rule suffix and safe message; runtime adds only
the session-action namespace:

```rust
impl SessionActionFailureCategory {
    pub fn rule_id(self) -> String {
        match self {
            Self::AuthorityUnavailable => "session_action_authority_unavailable".into(),
            Self::ExactSessionUnavailable => "session_action_session_unavailable".into(),
            Self::ExactSessionAmbiguous => "session_action_session_ambiguous".into(),
            Self::Guarded(category) => {
                format!("session_action_{}", category.rule_suffix())
            }
        }
    }

    pub fn safe_message(self) -> &'static str {
        match self {
            Self::AuthorityUnavailable => "Session action authority is unavailable",
            Self::ExactSessionUnavailable => "No exact live provider session for action",
            Self::ExactSessionAmbiguous => "Exact live provider session is ambiguous",
            Self::Guarded(category) => category.safe_message(),
        }
    }
}

impl SessionActionFailure {
    pub fn safe_message(&self) -> &'static str {
        self.category.safe_message()
    }
}

fn record_session_action_failure(
    path: impl Into<PathBuf>,
    attempt: &SessionActionAttempt,
    action: Option<&TerminalSessionAction>,
    category: SessionActionFailureCategory,
) -> SessionActionFailure;
```

**Acceptance Criteria:**
- `SessionActionAttempt::new(target)` creates an opaque UUIDv4 attempt ID in core, without adding a TUI dependency.
- Attempt construction projects only provider, session ID, project ID, cwd, and provenance; provider-session, turn, tool-use, and provider-hint fields never cross the preflight or diagnostic boundary.
- Preflight and dispatch independently rediscover the exact live provider session.
- A permission prompt yields Allow, Deny, and ManualText; a recovery prompt yields Continue and ManualText; no semantic prompt yields ManualText only.
- Unknown authority, missing session, ambiguous session, target/capture failure, prompt change, send failure, and delivery uncertainty map to fixed categories without string parsing or duplicating core guarded categories.
- Each failed manual attempt appends one safe `ActivityKind::Diagnostic` event keyed by its attempt ID.
- Preflight diagnostics do not fabricate an action; dispatch diagnostics never persist manual text.
- Diagnostic append failure preserves the originating category, sets `diagnostic_persisted = false`, and never retries terminal input.

- [ ] **Step 1: Write failing runtime contract and diagnostic tests**

Add core runtime tests for opaque identity and mock action recording:

```rust
fn session_target() -> SessionTarget {
    SessionTarget {
        provider: AgentProvider::Claude,
        session_id: "session-42".into(),
        provider_session_id: None,
        turn_id: Some("turn-7".into()),
        tool_use_id: None,
        project_id: ProjectId::Stable("project-1".into()),
        cwd: "/work/project".into(),
        provider_hints: Vec::new(),
        provenance: SessionTargetProvenance::Structured,
    }
}

#[test]
fn session_action_preflight_creates_opaque_attempt_identity() {
    let request = SessionActionPreflightRequest::new(session_target());
    assert!(uuid::Uuid::parse_str(&request.attempt.attempt_id).is_ok());
    assert_eq!(
        request.attempt.target,
        SessionActionTarget::from(session_target()),
    );
}

#[test]
fn mock_runtime_records_preflight_and_dispatch_separately() {
    let mock = Arc::new(MockBrainRuntime::default());
    let runtime = BrainRuntime::new(mock.clone(), mock.clone());
    let preflight = SessionActionPreflightRequest::new(session_target());
    let availability = runtime
        .actions
        .preflight_session_action(preflight.clone())
        .unwrap();
    runtime
        .actions
        .send_session_action(SessionActionRequest {
            attempt: availability.attempt,
            action: TerminalSessionAction::Continue,
        })
        .unwrap();
    assert!(matches!(mock.actions()[0], MockBrainAction::SessionActionPreflight(_)));
    assert!(matches!(mock.actions()[1], MockBrainAction::SessionAction(_)));
}
```

Add binary runtime tests around injected session/probe helpers:

```rust
fn structured_target() -> SessionTarget {
    SessionTarget {
        provider: AgentProvider::Codex,
        session_id: "native-1".into(),
        provider_session_id: None,
        turn_id: Some("turn-1".into()),
        tool_use_id: None,
        project_id: ProjectId::Temporary("project".into()),
        cwd: "/work/project".into(),
        provider_hints: Vec::new(),
        provenance: SessionTargetProvenance::Structured,
    }
}

fn manual_request(text: &str) -> SessionActionRequest {
    let preflight = SessionActionPreflightRequest::new(structured_target());
    SessionActionRequest {
        attempt: preflight.attempt,
        action: TerminalSessionAction::Text(text.into()),
    }
}

#[test]
fn preflight_maps_recovery_prompt_to_continue_and_manual_text() {
    let request = SessionActionPreflightRequest::new(structured_target());
    let result = preflight_session_action_from(
        request,
        vec![discovered_session(AgentProvider::Codex, "native-1")],
        SessionIdentityProjection::default(),
        |_| Ok(Some(ActionablePrompt::Continue)),
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(
        result.capabilities,
        vec![
            SessionActionCapability::Continue,
            SessionActionCapability::ManualText,
        ]
    );
}

#[test]
fn manual_failure_diagnostic_contains_category_but_not_terminal_or_text() {
    let temp = tempfile::tempdir().unwrap();
    let request = manual_request("top-secret-literal");
    let failure = record_session_action_failure(
        temp.path().join("activity.jsonl"),
        &request.attempt,
        Some(&request.action),
        SessionActionFailureCategory::Guarded(
            GuardedActionFailureCategory::PostflightUnconfirmed,
        ),
    );
    assert!(failure.diagnostic_persisted);
    let stored = std::fs::read_to_string(temp.path().join("activity.jsonl")).unwrap();
    assert!(stored.contains("session_action_postflight_unconfirmed"));
    assert!(!stored.contains("top-secret-literal"));
}

#[test]
fn diagnostic_failure_preserves_category_and_never_retries_delivery() {
    let sends = Cell::new(0);
    let request = manual_request("top-secret-literal");
    let failure = dispatch_session_action_from(
        request,
        |_| {
            sends.set(sends.get() + 1);
            Err(GuardedActionFailure::unknown(
                GuardedActionFailureCategory::SendFailed,
                "send failed",
            ))
        },
        |_, _, _| Err("activity store busy".into()),
    )
    .unwrap_err();
    assert_eq!(
        failure.category,
        SessionActionFailureCategory::Guarded(
            GuardedActionFailureCategory::SendFailed,
        )
    );
    assert!(!failure.diagnostic_persisted);
    assert_eq!(sends.get(), 1);
}
```

- [ ] **Step 2: Run the runtime tests and confirm the red state**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core session_action_preflight -- --nocapture
nix develop path:. --command cargo test preflight_maps_recovery_prompt_to_continue_and_manual_text -- --nocapture
nix develop path:. --command cargo test manual_failure_diagnostic_contains_category_but_not_terminal_or_text -- --nocapture
nix develop path:. --command cargo test diagnostic_failure_preserves_category_and_never_retries_delivery -- --nocapture
```

Expected: compilation fails because the preflight contract and diagnostic helper are absent.

- [ ] **Step 3: Implement the runtime types and mock behavior**

Add the produced types to `runtime.rs`. Implement:

```rust
impl SessionActionAttempt {
    pub fn new(target: SessionTarget) -> Self {
        Self {
            attempt_id: uuid::Uuid::new_v4().to_string(),
            target,
        }
    }
}

impl SessionActionPreflightRequest {
    pub fn new(target: SessionTarget) -> Self {
        Self {
            attempt: SessionActionAttempt::new(target),
        }
    }
}

impl SessionActionCapability {
    pub fn permits(self, action: &TerminalSessionAction) -> bool {
        matches!(
            (self, action),
            (Self::Allow, TerminalSessionAction::Allow)
                | (Self::Deny, TerminalSessionAction::Deny)
                | (Self::Continue, TerminalSessionAction::Continue)
                | (Self::ManualText, TerminalSessionAction::Text(_))
        )
    }
}
```

Extend `MockBrainRuntime` with configurable preflight capabilities and typed
errors. Record `MockBrainAction::SessionActionPreflight` separately from
dispatch.

- [ ] **Step 4: Implement exact-session preflight and classified dispatch**

Extract one private exact-session resolver shared as code, not evidence:

```rust
fn resolve_action_session(
    target: &SessionTarget,
    sessions: Vec<AgentSession>,
    projection: &SessionIdentityProjection,
) -> Result<AgentSession, SessionActionFailureCategory>
```

Add an injected helper used by production and tests:

```rust
fn preflight_session_action_from(
    request: SessionActionPreflightRequest,
    sessions: Vec<AgentSession>,
    projection: SessionIdentityProjection,
    probe: impl FnOnce(&AgentSession) -> Result<Option<ActionablePrompt>, GuardedActionFailure>,
    persist_failure: impl FnOnce(
        &SessionActionPreflightRequest,
        SessionActionFailureCategory,
    ) -> Result<(), String>,
) -> Result<SessionActionAvailability, SessionActionFailure>
```

Call it independently from `preflight_session_action` and
`send_session_action`. Map prompt evidence to capabilities:

```rust
fn capabilities_for_prompt(prompt: Option<ActionablePrompt>) -> Vec<SessionActionCapability> {
    match prompt {
        Some(ActionablePrompt::Allow | ActionablePrompt::Deny) => vec![
            SessionActionCapability::Allow,
            SessionActionCapability::Deny,
            SessionActionCapability::ManualText,
        ],
        Some(ActionablePrompt::Continue) => vec![
            SessionActionCapability::Continue,
            SessionActionCapability::ManualText,
        ],
        _ => vec![SessionActionCapability::ManualText],
    }
}
```

Before dispatch, verify that the requested action is compatible with a fresh
prompt by relying on Task 1's guarded executor; never reuse preflight evidence.
Map core failures exhaustively to
`SessionActionFailureCategory::Guarded(error.category)` so runtime taxonomy
cannot drift from the source classification.

Use `record_session_action_failure` for every failed preflight or dispatch. It
accepts the shared attempt and an optional action, performs exactly one
best-effort append, and returns the original category with
`diagnostic_persisted` reflecting only that append. It must not retry discovery,
capture, or delivery. Build the event as:

```rust
ActivityEvent {
    schema_version: ACTIVITY_SCHEMA_VERSION,
    kind: ActivityKind::Diagnostic,
    activity_id: format!("session_action:{}", attempt.attempt_id),
    recorded_at_ms: epoch_ms(),
    project: ProjectEvidence {
        project_id: attempt.target.project_id.clone(),
        cwd: attempt.target.cwd.clone(),
        label: None,
    },
    session: Some(SessionTarget {
        provider: attempt.target.provider,
        session_id: attempt.target.session_id.clone(),
        provider_session_id: None,
        turn_id: None,
        tool_use_id: None,
        project_id: attempt.target.project_id.clone(),
        cwd: attempt.target.cwd.clone(),
        provider_hints: Vec::new(),
        provenance: attempt.target.provenance,
    }),
    state: ActivityState::Error,
    tool: Some("session_action".into()),
    rule_id: Some(failure.category.rule_id().into()),
    reasoning: Some(failure.category.safe_message().into()),
    normalized_command: None,
    fingerprint: None,
    confidence: None,
    threshold: None,
    decision_id: None,
    outcome: None,
    correction: None,
    note: None,
    supersedes: None,
}
```

Never copy `TerminalSessionAction::Text` into the diagnostic. Preflight passes
`None`; dispatch may pass the action only so the helper can distinguish
semantic metadata without serializing or formatting manual content.

- [ ] **Step 5: Run runtime and activity regressions**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-core runtime::tests -- --nocapture
nix develop path:. --command cargo test runtime::brain::tests -- --nocapture
nix develop path:. --command cargo test brain::activity::tests -- --nocapture
```

Expected: all tests pass; diagnostic projection remains metadata-only and excluded from Live.

- [ ] **Step 6: Review the task diff**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git diff -- crates/coding-brain-core/src/runtime.rs src/runtime/brain.rs
```

Expected: only the typed runtime contract, exact-session helper, safe diagnostic append, mocks, and tests changed. Do not commit without explicit authorization.

---

### Task 3: Make Live preflight asynchronous and capability-gated

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs:85-175`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:285-330`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:458-640`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:875-894`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:1185-1435`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs:100-122`
- Test: `crates/coding-brain-tui/src/brain_app.rs`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs`

**Interfaces:**
- Consumes: Task 2's `SessionActionPreflightRequest`,
  `SessionActionAvailability`, `SessionActionCapability`,
  `SessionActionFailure`, and typed `SessionActionRequest`.
- Produces an asynchronous worker result:

```rust
enum SessionActionWorkerResult {
    Preflight(Result<SessionActionAvailability, SessionActionFailure>),
    Delivery {
        kind: SessionActionKind,
        result: Result<(), SessionActionFailure>,
    },
}
```

`BrainInput::SessionAction` retains the shared `SessionActionAttempt`,
`capabilities`, and optional hidden text.

**Acceptance Criteria:**
- Pressing `x` captures the selected target, starts one worker, and immediately shows `Checking available actions…`.
- The render thread performs no discovery, tmux capture, activity I/O, or join of an in-flight worker; completed/disconnected handles are joined during cleanup.
- Permission, recovery, and manual-only menus expose exactly their returned capabilities.
- Key handling and rendering consult the same capability set; an unavailable `c`, `a`, or `d` never dispatches.
- Dispatch uses the preflight attempt ID but no preflight evidence.
- A preflight result whose captured target no longer matches the selected target is discarded without opening a menu.
- Preflight and delivery failures show safe fixed categories and indicate when diagnostic persistence failed.
- Existing manual-text redaction/bounds, Escape, single-flight, exit blocking, refresh precedence, and `x action` footer remain covered.

- [ ] **Step 1: Write failing asynchronous preflight and capability tests**

Add tests:

```rust
fn fixture_app_with_capabilities(
    capabilities: Vec<SessionActionCapability>,
) -> (BrainApp, Arc<MockBrainRuntime>) {
    let mock = Arc::new(MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            attention: vec![AttentionItem {
                activity: activity(),
                occurrences: 1,
                unresolved_occurrences: 1,
            }],
            unresolved_count: 1,
            ..ActivitySnapshot::default()
        },
        session_action_capabilities: std::sync::Mutex::new(capabilities),
        ..MockBrainRuntime::default()
    });
    let runtime = BrainRuntime::new(mock.clone(), mock.clone());
    (BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark)), mock)
}

fn open_action_menu(app: &mut BrainApp) {
    app.handle_key(key(KeyCode::Char('x')));
    let deadline = Instant::now() + Duration::from_secs(1);
    while app.input_prompt().is_none() && Instant::now() < deadline {
        app.refresh();
        std::thread::yield_now();
    }
    assert!(app.input_prompt().is_some());
}

#[test]
fn x_starts_nonblocking_preflight_before_opening_action_menu() {
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut app, actions) =
        slow_preflight_fixture(Duration::from_millis(250), completed);
    let started = Instant::now();
    app.handle_key(key(KeyCode::Char('x')));
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(app.input_prompt(), None);
    assert_eq!(app.status(), Some("Checking available actions…"));
    wait_for_preflight(&mut app, &actions);
    assert_eq!(
        app.input_prompt(),
        Some("Action: [c] continue  [t] manual text".into())
    );
}

#[test]
fn unavailable_semantic_key_cannot_dispatch_hidden_action() {
    let (mut app, mock) = fixture_app_with_capabilities(vec![
        SessionActionCapability::ManualText,
    ]);
    open_action_menu(&mut app);
    app.handle_key(key(KeyCode::Char('c')));
    assert!(non_poll_actions(&mock)
        .iter()
        .all(|action| !matches!(action, MockBrainAction::SessionAction(_))));
    assert!(app.input_prompt().unwrap().contains("[t] manual text"));
    assert!(app.input_prompt().unwrap().contains("recognized recovery prompt"));
}

#[test]
fn dispatch_reuses_attempt_identity_but_not_preflight_evidence() {
    let (mut app, mock) = fixture_app_with_capabilities(vec![
        SessionActionCapability::Continue,
        SessionActionCapability::ManualText,
    ]);
    open_action_menu(&mut app);
    let preflight_id = mock
        .actions()
        .into_iter()
        .find_map(|action| match action {
            MockBrainAction::SessionActionPreflight(request) => {
                Some(request.attempt.attempt_id)
            }
            _ => None,
        })
        .unwrap();
    app.handle_key(key(KeyCode::Char('c')));
    wait_for_actions(&mut app, &mock, 2);
    let request = mock
        .actions()
        .into_iter()
        .find_map(|action| match action {
            MockBrainAction::SessionAction(request) => Some(request),
            _ => None,
        })
        .unwrap();
    assert_eq!(request.attempt.attempt_id, preflight_id);
}

#[test]
fn preflight_result_is_discarded_when_selection_changes() {
    let (mut app, actions) = slow_preflight_fixture(
        Duration::from_millis(100),
        Arc::new(AtomicBool::new(false)),
    );
    app.handle_key(key(KeyCode::Char('x')));
    app.handle_key(key(KeyCode::Down));
    wait_for_preflight(&mut app, &actions);
    assert_eq!(app.input_prompt(), None);
    assert_eq!(app.status(), Some("Selection changed; action cancelled"));
}
```

- [ ] **Step 2: Run the TUI tests and confirm the red state**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui x_starts_nonblocking_preflight_before_opening_action_menu -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui unavailable_semantic_key_cannot_dispatch_hidden_action -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui dispatch_reuses_attempt_identity_but_not_preflight_evidence -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui preflight_result_is_discarded_when_selection_changes -- --nocapture
```

Expected: compilation fails because the worker supports delivery only and the input has no capability set.

- [ ] **Step 3: Generalize the session-action worker**

Replace the delivery-only receiver with
`Receiver<SessionActionWorkerResult>`. `begin_session_action` keeps the existing
selection/provenance gates, builds `SessionActionPreflightRequest::new(target)`,
sets `Checking available actions…`, and spawns the preflight call.

Extend the existing `SlowBrainActions` test double with:

```rust
preflight_delay: Duration,
preflight_capabilities: Vec<SessionActionCapability>,
```

and implement `preflight_session_action` by incrementing its call counter,
sleeping for `preflight_delay`, setting the existing completion flag, and
returning the request identity with `preflight_capabilities`. Add
`slow_preflight_fixture(delay, completed)` beside `slow_fixture_app`; it uses
the same attention snapshot and supplies Continue plus ManualText.

Poll results before source refresh as today. On preflight success, create:

```rust
BrainInput::SessionAction {
    attempt: availability.attempt,
    capabilities: availability.capabilities,
    text: None,
}
```

Before creating the input, compare `availability.attempt.target` with
`SessionActionTarget::from(current_selected_target)`. If they differ—or the
selection no longer has a target—discard the result, leave `input` empty, and show
`Selection changed; action cancelled`. Target equality is stable authority;
do not compare list indices because refresh can reorder rows.

On failure, leave `input` empty and format only
`failure.category.safe_message()` plus `"; diagnostic unavailable"` when
`diagnostic_persisted` is false.

- [ ] **Step 4: Gate menu copy and key handling from capabilities**

Add one helper:

```rust
fn capability_prompt(capabilities: &[SessionActionCapability]) -> String {
    let mut actions = Vec::new();
    if capabilities.contains(&SessionActionCapability::Allow) {
        actions.push("[a] allow");
    }
    if capabilities.contains(&SessionActionCapability::Deny) {
        actions.push("[d] deny");
    }
    if capabilities.contains(&SessionActionCapability::Continue) {
        actions.push("[c] continue");
    }
    if capabilities.contains(&SessionActionCapability::ManualText) {
        actions.push("[t] manual text");
    }
    let mut prompt = format!("Action: {}", actions.join("  "));
    if capabilities == [SessionActionCapability::ManualText] {
        prompt.push_str(" · Continue requires a recognized recovery prompt");
    }
    prompt
}
```

Before dispatch or entering manual text, require the matching capability's
`permits` result. Unknown keys leave the menu open.

Keep `SessionActionWorker::finish` joining the handle only after the receiver
returns `Ready` or `Disconnected`; this cannot wait for an in-flight action.
Preserve the existing `q`/Enter guard while a worker is in flight and the Drop
cleanup behavior. The slow-preflight test remains the proof that refresh and
render polling do not block on the worker.

- [ ] **Step 5: Preserve refresh and layout behavior**

Update existing worker tests for the extra preflight action. Add render
assertions at 119 and 120 columns proving `x action` remains visible and that
Diagnostics remains reachable after a failure refresh. Do not change footer
navigation keys or the Live list/evidence layout.

- [ ] **Step 6: Run TUI regressions**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests -- --nocapture
nix develop path:. --command cargo test --test brain_tui_smoke -- --nocapture
```

Expected: all TUI tests pass, including slow-worker, hidden text, refresh priority, and 119/120-column rendering.

- [ ] **Step 7: Review the task diff**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git diff -- crates/coding-brain-tui/src/brain_app.rs crates/coding-brain-tui/src/ui/brain/mod.rs
```

Expected: only asynchronous preflight, capability-driven input/menu behavior, status formatting, and related tests changed. Do not commit without explicit authorization.

---

### Task 4: Classify and deduplicate automatic recovery outcomes

**Files:**
- Modify: `src/brain/recovery.rs:156-307`
- Modify: `src/brain/recovery.rs:603-659`
- Modify: `src/brain/recovery.rs:678-822`
- Modify: `src/brain/recovery.rs:1010-1070`
- Modify: `src/brain/recovery.rs:1310-1380`
- Modify: `src/brain/recovery.rs:1685-1963`
- Test: `src/brain/recovery.rs`

**Interfaces:**
- Consumes: Task 1's typed `GuardedActionFailure`,
  `GuardedActionFailureCategory`, and `DeliveryCertainty`; existing
  `RecoveryAttemptKey`, `RecoveryReservationStore`, `append_recovery_audit`,
  `ActivityStore::append_if_absent`, and RecoveryCoordinator queue.
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryExecution {
    Continued,
    InactiveMode,
    InvalidEvidence,
    InferenceFailed,
    ModelAbstained,
    BelowThreshold,
    EvidenceUnavailable,
    EvidenceChanged,
    ReservationDuplicate,
    ReservationCooldown,
    ReservationFailed,
    PreSendAuditFailed,
    DeliveryNotSent(GuardedActionFailureCategory),
    DeliveryUnknown(GuardedActionFailureCategory),
    PostflightUncertain,
    PostSendAuditUncertain,
}

impl RecoveryExecution {
    fn diagnostic_category(self) -> Option<&'static str>;
    fn safe_message(self) -> &'static str;
}
```

**Acceptance Criteria:**
- `execute_recovery_with` and Antigravity structured recovery return the exact bounded exit category.
- Low confidence and model LeaveAlone are distinct; raw model errors are never persisted.
- Snapshot/read failure is `EvidenceUnavailable`; a successful non-matching snapshot is `EvidenceChanged`.
- Duplicate and cooldown reservations are distinct and do not change reservation semantics.
- Not-sent guarded failure, delivery unknown, postflight uncertainty, and post-send audit uncertainty remain distinct.
- Each diagnostic uses a stable provider/session/epoch/category ID and `ActivityKind::Diagnostic`.
- `ActivityStore::append_if_absent` is the cross-process dedupe guard.
- RecoveryCoordinator uses a bounded 64-entry in-memory attempt/category cache to avoid repeated disk scans.
- Automatic `Evaluating` persists before send; successful delivery still appends `Delivered`.

- [ ] **Step 1: Write failing typed outcome tests**

Replace undifferentiated abstention assertions with a table:

```rust
#[test]
fn recovery_returns_exact_abstention_category_without_sending() {
    for (suggestion, expected) in [
        (Ok(RecoverySuggestion {
            decision: RecoveryDecision::LeaveAlone,
            reasoning: "leave alone".into(),
            confidence: 0.91,
            suggested_at: 1_000,
        }), RecoveryExecution::ModelAbstained),
        (Ok(suggestion(0.59)), RecoveryExecution::BelowThreshold),
        (Err("model raw secret".into()), RecoveryExecution::InferenceFailed),
    ] {
        let sends = Cell::new(0);
        let original = target(AgentProvider::Codex);
        let outcome = execute_recovery_with(
            BrainGateMode::Auto,
            original.clone(),
            0.60,
            || suggestion.clone(),
            || Ok(original.clone()),
            |_| Ok(ReservationOutcome::Reserved),
            |_| Ok(()),
            |_| {
            sends.set(sends.get() + 1);
            Ok(())
            },
            |_| Ok(()),
        );
        assert_eq!(outcome, expected);
        assert_eq!(sends.get(), 0);
    }
}

#[test]
fn recovery_diagnostic_is_atomic_and_contains_no_model_or_prompt_text() {
    let temp = tempfile::tempdir().unwrap();
    let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
    let target = target(AgentProvider::Codex);
    assert!(append_recovery_diagnostic(
        &activity,
        &process_session(AgentProvider::Codex),
        &target,
        RecoveryExecution::BelowThreshold,
    )
    .unwrap());
    assert!(!append_recovery_diagnostic(
        &activity,
        &process_session(AgentProvider::Codex),
        &target,
        RecoveryExecution::BelowThreshold,
    )
    .unwrap());
    let stored = std::fs::read_to_string(temp.path().join("activity.jsonl")).unwrap();
    assert!(!stored.contains("model raw secret"));
    assert!(!stored.contains("terminal pane"));
}

#[test]
fn bounded_reported_diagnostics_cache_suppresses_repeats() {
    let mut cache = ReportedRecoveryDiagnostics::default();
    assert!(cache.reserve_if_new("attempt-1:reservation_duplicate".into()));
    assert!(!cache.reserve_if_new("attempt-1:reservation_duplicate".into()));
    for index in 0..MAX_RECOVERY_QUEUE {
        cache.reserve_if_new(format!("attempt-{index}:below_threshold"));
    }
    assert!(cache.len() <= MAX_RECOVERY_QUEUE);
}

#[test]
fn concurrent_diagnostic_reservation_writes_once_and_retries_after_failure() {
    let cache = Arc::new(Mutex::new(ReportedRecoveryDiagnostics::default()));
    let id = "attempt-1:below_threshold".to_string();
    let reserved = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let worker = {
        let cache = Arc::clone(&cache);
        let id = id.clone();
        let reserved = Arc::clone(&reserved);
        let release = Arc::clone(&release);
        std::thread::spawn(move || {
            assert!(cache.lock().unwrap().reserve_if_new(id.clone()));
            reserved.wait();
            release.wait();
            cache.lock().unwrap().remove(&id);
        })
    };
    reserved.wait();
    assert!(!cache.lock().unwrap().reserve_if_new(id.clone()));
    release.wait();
    worker.join().unwrap();
    assert!(cache.lock().unwrap().reserve_if_new(id));
}

#[test]
fn recovery_diagnostic_identity_changes_with_attempt_or_category() {
    let base = recovery_diagnostic_id(&attempt("session-1", 7), "below_threshold");
    assert_ne!(
        base,
        recovery_diagnostic_id(&attempt("session-1", 8), "below_threshold")
    );
    assert_ne!(
        base,
        recovery_diagnostic_id(&attempt("session-1", 7), "model_abstained")
    );
}
```

- [ ] **Step 2: Run the recovery tests and confirm the red state**

Run:

```bash
nix develop path:. --command cargo test recovery_returns_exact_abstention_category_without_sending -- --nocapture
nix develop path:. --command cargo test recovery_diagnostic_is_atomic_and_contains_no_model_or_prompt_text -- --nocapture
nix develop path:. --command cargo test bounded_reported_diagnostics_cache_suppresses_repeats -- --nocapture
nix develop path:. --command cargo test concurrent_diagnostic_reservation_writes_once_and_retries_after_failure -- --nocapture
nix develop path:. --command cargo test recovery_diagnostic_identity_changes_with_attempt_or_category -- --nocapture
```

Expected: compilation fails because typed recovery outcomes and the reporter do not exist.

- [ ] **Step 3: Return precise recovery outcomes**

Replace each `RecoveryExecution::Abstained` return with the exact produced
variant. Preserve operation order:

```rust
let suggestion = match infer() {
    Ok(suggestion) => suggestion,
    Err(_) => return RecoveryExecution::InferenceFailed,
};
if !suggestion.confidence.is_finite() || suggestion.confidence < threshold {
    return RecoveryExecution::BelowThreshold;
}
if !matches!(suggestion.decision, RecoveryDecision::Continue(_)) {
    return RecoveryExecution::ModelAbstained;
}
```

For both snapshots, match the result instead of using `is_ok_and`: return
`EvidenceUnavailable` on `Err(_)`, and `EvidenceChanged` only when an `Ok`
snapshot does not match the pending attempt. Keep `InactiveMode` and initially
`InvalidEvidence` non-diagnostic because no automatic recovery attempt began.

Map reservation outcomes directly. Keep `audit(Evaluating)` before the final
snapshot and send. Map Task 1 failures by certainty:

```rust
match deliver(&target) {
    Ok(()) => {}
    Err(RecoveryDeliveryFailure::NotSent(category)) => {
        let _ = audit(ActivityState::DeliveryFailed);
        return RecoveryExecution::DeliveryNotSent(category);
    }
    Err(RecoveryDeliveryFailure::Unknown(category)) => {
        return RecoveryExecution::DeliveryUnknown(category);
    }
}
```

Postflight failure becomes `PostflightUncertain`; failed delivered-audit after
send becomes `PostSendAuditUncertain`.

Define:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDeliveryFailure {
    NotSent(GuardedActionFailureCategory),
    Unknown(GuardedActionFailureCategory),
}

fn recovery_delivery_failure(error: GuardedActionFailure) -> RecoveryDeliveryFailure {
    match error.certainty {
        DeliveryCertainty::NotSent => {
            RecoveryDeliveryFailure::NotSent(error.category)
        }
        DeliveryCertainty::Unknown => {
            RecoveryDeliveryFailure::Unknown(error.category)
        }
    }
}
```

`RecoveryExecution::diagnostic_category` returns no category for
`Continued`, `InactiveMode`, or `InvalidEvidence`. Fixed category IDs are
`inference_failed`, `model_abstained`, `below_threshold`,
`evidence_unavailable`, `evidence_changed`, `reservation_duplicate`, `reservation_cooldown`,
`reservation_failed`, `pre_send_audit_failed`, `postflight_uncertain`, and
`post_send_audit_uncertain`. Delivery variants combine `delivery_not_sent` or
`delivery_unknown` with the fixed `GuardedActionFailureCategory` name.
`safe_message` uses matching fixed prose and never includes model reasoning,
prompt text, or backend detail.

- [ ] **Step 4: Persist safe automatic diagnostics**

Build stable IDs from serialized `RecoveryAttemptKey` plus the fixed category:

```rust
fn append_recovery_diagnostic(
    activity: &ActivityStore,
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
    outcome: RecoveryExecution,
) -> Result<bool, String> {
    let category = outcome
        .diagnostic_category()
        .ok_or_else(|| "recovery outcome is not diagnostic".to_string())?;
    let activity_id = recovery_diagnostic_id(&target.attempt, category)?;
    activity
        .append_if_absent(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Diagnostic,
            activity_id,
            recorded_at_ms: epoch_ms(),
            project: recovery_project(session, target),
            session: Some(recovery_session_target(session, target)),
            state: ActivityState::Error,
            tool: Some("recovery".into()),
            rule_id: Some(format!("recovery_{category}")),
            reasoning: Some(outcome.safe_message().into()),
            normalized_command: None,
            fingerprint: None,
            confidence: None,
            threshold: None,
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|_| "recovery diagnostic persistence failed".to_string())
}
```

`recovery_diagnostic_id` serializes the canonical `(RecoveryAttemptKey,
category)` tuple, hashes it with the workspace's existing `sha2::Sha256`
dependency, and returns `recovery_diagnostic:<64 lowercase hex digits>`. Use
the full digest rather than `compact_fingerprint`; the category participates
in the digest. Test deterministic equality plus distinct attempt/category
inputs.

Factor the repeated safe project/session construction already present in
`append_recovery_audit` into:

```rust
fn recovery_project(
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
) -> ProjectEvidence {
    ProjectEvidence {
        project_id: ProjectId::Temporary(format!(
            "recovery:{}",
            target.attempt.session.storage_key()
        )),
        cwd: PathBuf::from(&session.cwd),
        label: None,
    }
}

fn recovery_session_target(
    session: &AgentSession,
    target: &RecoveryTargetSnapshot,
) -> SessionTarget {
    let project = recovery_project(session, target);
    SessionTarget {
        provider: session.provider,
        session_id: session.session_id.clone(),
        provider_session_id: None,
        turn_id: target.turn_id.clone(),
        tool_use_id: target.pending_tool_use_id.clone(),
        project_id: project.project_id,
        cwd: project.cwd,
        provider_hints: Vec::new(),
        provenance: SessionTargetProvenance::Unknown,
    }
}
```

Use the same helper from Stop-hook recovery and TUI polling. Do not record
inactive `On`/`Off` mode as a diagnostic.

- [ ] **Step 5: Add bounded coordinator-local deduplication**

Introduce a 64-entry FIFO/set cache owned by `RecoveryCoordinator`. Reserving
an unseen ID inserts it before calling `append_if_absent`; that entry represents
an in-flight diagnostic and prevents concurrent local workers from starting the
same write. Retain it after a successful append or confirmed existing row.
Remove it after persistence failure so a later poll can retry. Evict the oldest
retained ID at 64. `append_if_absent` remains the authoritative cross-process
dedupe guard. The cache does not touch `RecoveryReservationStore`, queue
membership, `inflight`, scan cadence, or worker counts.

Implement the cache as:

```rust
#[derive(Default)]
struct ReportedRecoveryDiagnostics {
    order: VecDeque<String>,
    ids: HashSet<String>,
}

impl ReportedRecoveryDiagnostics {
    fn reserve_if_new(&mut self, id: String) -> bool {
        if self.ids.contains(&id) {
            return false;
        }
        if self.order.len() >= MAX_RECOVERY_QUEUE
            && let Some(evicted) = self.order.pop_front()
        {
            self.ids.remove(&evicted);
        }
        self.ids.insert(id.clone());
        self.order.push_back(id);
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len()
    }

    fn remove(&mut self, id: &str) {
        if self.ids.remove(id) {
            self.order.retain(|candidate| candidate != id);
        }
    }
}
```

Store it behind an `Arc<Mutex<ReportedRecoveryDiagnostics>>` owned by
`RecoveryCoordinator` and captured by its worker evaluator. If persistence
fails, remove the just-inserted ID so a later poll may retry the diagnostic
write without retrying terminal input. Hold the cache mutex only for
reserve/retain/remove transitions, never across activity-store I/O. The
barrier-backed concurrency test must prove one local write while reserved and
successful retry after a failed reservation is released.

- [ ] **Step 6: Run recovery and activity-store regressions**

Run:

```bash
nix develop path:. --command cargo test brain::recovery::tests -- --nocapture
nix develop path:. --command cargo test brain::activity::tests -- --nocapture
nix develop path:. --command cargo test brain::activity::tests::lock_wait_is_bounded_and_busy_compaction_skips -- --nocapture
```

Expected: all recovery outcome, reservation, audit-order, deduplication, and 100 ms lock-bound tests pass.

- [ ] **Step 7: Review the task diff**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git diff -- src/brain/recovery.rs
```

Expected: only typed recovery exits, safe diagnostic persistence, bounded coordinator dedupe, and directly related tests changed. Do not commit without explicit authorization.

---

### Task 5: Update operator documentation and verify the complete safety path

**Files:**
- Modify: `README.md:3-12`
- Modify: `docs/reference.md:14-24`
- Modify: `docs/terminal-support.md:20-35`
- Modify: `docs/quickstart.md:41-52`
- Modify: `docs/llms.txt:1-15`
- Test: `crates/coding-brain-core/src/terminals/mod.rs`
- Test: `src/runtime/brain.rs`
- Test: `crates/coding-brain-tui/src/brain_app.rs`
- Test: `src/brain/recovery.rs`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs`

**Interfaces:**
- Consumes: Tasks 1-4 complete typed preflight, dispatch, diagnostics, recovery outcomes, and TUI behavior.
- Produces: accurate operator documentation and final verification evidence for `codexctl-vlwz`.

**Acceptance Criteria:**
- Documentation says `x` preflights current exact-session/pane/prompt state and exposes only recognized semantic actions.
- Documentation says dispatch revalidates independently and can still reject a changed prompt.
- Diagnostics is described as metadata-only safe categories for hooks, correlation, session actions, recovery, and store integrity—not captured content and not ordinary command output.
- Terminal support retains manual text as explicit operator input and never implies unknown prompts can trigger semantic input.
- Focused successful manual and automatic Codex Continue tests pass.
- All workspace quality gates pass from the Nix development shell.

- [ ] **Step 1: Add or confirm cross-boundary regression coverage**

Ensure the final test suite includes these exact named scenarios:

```text
guarded_codex_continue_requires_exact_idle_composer_and_sends_fixed_literal
preflight_maps_recovery_prompt_to_continue_and_manual_text
x_starts_nonblocking_preflight_before_opening_action_menu
unavailable_semantic_key_cannot_dispatch_hidden_action
preflight_continue_then_changed_prompt_is_rejected_and_diagnosable
auto_recovery_sends_only_after_stable_evidence_reservation_and_audits
recovery_diagnostic_is_atomic_and_contains_no_model_or_prompt_text
wide_and_narrow_live_keep_action_entry_point_discoverable
```

If the last render test does not exist, add it to
`crates/coding-brain-tui/src/ui/brain/mod.rs` using `TestBackend` at widths 119
and 120 and assert the rendered footer contains `x action`.

Implement
`preflight_continue_then_changed_prompt_is_rejected_and_diagnosable` as a
cross-boundary fixture: preflight returns Continue, the dispatch fixture then
reports `PromptChanged`, pressing `c` sends no terminal input, the footer shows
the fixed safe category, exactly one metadata-only diagnostic row exists for
the shared attempt ID, and a normal refresh leaves that row reachable through
Diagnostics.

- [ ] **Step 2: Run focused safety-path tests**

Run:

```bash
nix develop path:. --command cargo test guarded_codex_continue_requires_exact_idle_composer_and_sends_fixed_literal -- --nocapture
nix develop path:. --command cargo test preflight_maps_recovery_prompt_to_continue_and_manual_text -- --nocapture
nix develop path:. --command cargo test x_starts_nonblocking_preflight_before_opening_action_menu -- --nocapture
nix develop path:. --command cargo test unavailable_semantic_key_cannot_dispatch_hidden_action -- --nocapture
nix develop path:. --command cargo test preflight_continue_then_changed_prompt_is_rejected_and_diagnosable -- --nocapture
nix develop path:. --command cargo test auto_recovery_sends_only_after_stable_evidence_reservation_and_audits -- --nocapture
nix develop path:. --command cargo test recovery_diagnostic_is_atomic_and_contains_no_model_or_prompt_text -- --nocapture
nix develop path:. --command cargo test wide_and_narrow_live_keep_action_entry_point_discoverable -- --nocapture
```

Expected: every named test passes; no test requires a live tmux server.

- [ ] **Step 3: Update user-facing documentation**

Replace unconditional menu wording with:

```markdown
Press `x` in Live to preflight the selected exact provider session. Coding
Brain exposes Allow/Deny only for a recognized permission prompt, Continue only
for a recognized recovery prompt, and bounded hidden manual text after exact
session, pane, and capture validation. Dispatch independently revalidates the
target and prompt, so a changed prompt is rejected without fallback input.
```

Describe Diagnostics with:

```markdown
Diagnostics is a read-only viewer for metadata-only safe categories covering
hook/correlation faults, rejected or uncertain session actions and recovery,
and activity-store integrity. It never stores captured terminal content or
manual text.
```

Apply the concise equivalent to `README.md`, `docs/quickstart.md`,
`docs/terminal-support.md`, and `docs/llms.txt` without duplicating the full
reference text.

- [ ] **Step 4: Run documentation checks**

Run:

```bash
rg -n "next key is|x.*,.*a.*d.*c.*t|not failed commands|unknown prompt" README.md docs
docs_output=$(mktemp -d)
nix shell nixpkgs#python3Packages.mkdocs-material --command mkdocs build --strict --site-dir "$docs_output"
```

Expected: no stale unconditional-action or “Diagnostics excludes failed actions” claims remain; strict MkDocs succeeds.

- [ ] **Step 5: Run full workspace quality gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test --workspace --all-targets
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
```

Expected: all commands exit 0 with no warnings.

- [ ] **Step 6: Inspect final scope and Beads status**

Run:

```bash
git -c core.whitespace=trailing-space,space-before-tab diff --check
git status --short
git diff --stat
bd -C /home/alexander/.beads-planning show codexctl-vlwz
```

Expected: only the approved spec, plan, terminal/runtime/TUI/recovery code and tests, and required documentation are changed. `codexctl-vlwz` remains in progress until implementation verification is complete. Do not commit, push, or close the issue without the applicable execution and completion gates.

## Stress Test Results

**Mode:** Interactive branch interrogation

**Decisions incorporated:**

1. Share one `SessionActionAttempt` across preflight, availability, dispatch,
   and diagnostics; preflight failures do not fabricate an action.
2. Add a classified exact-target API while preserving string-returning terminal
   compatibility wrappers for unrelated focus/navigation paths.
3. Discard asynchronous preflight results when the selected stable target has
   changed.
4. Join only completed/disconnected worker handles; never wait on an in-flight
   worker from render or refresh.
5. Treat the bounded recovery diagnostic cache entry as an in-flight local
   reservation; durable `append_if_absent` remains cross-process authority.
6. Derive automatic diagnostic IDs with full SHA-256 over canonical attempt and
   category input.
7. Separate unavailable recovery evidence from successfully read but changed
   evidence.
8. Centralize best-effort manual diagnostic persistence without changing the
   originating category or retrying delivery.
9. Add a cross-boundary preflight-success/prompt-change regression proving no
   send and durable Diagnostics visibility.
10. Test semantic and manual literal/key send failures as delivery-unknown,
    including manual-text redaction.

**Plan changes:** Tasks 1-5 now specify the shared attempt contract, scoped
classified wrappers, stale-result cancellation, nonblocking cleanup semantics,
typed evidence outcomes, collision-resistant identities, concurrent
deduplication behavior, centralized persistence failure handling, and the
expanded safety-path verification matrix.

**Reflexion pass:** The revised interfaces have one owner for guarded failure
taxonomy, preflight and dispatch share identity but not authority-bearing
evidence, every send-call failure has unknown delivery certainty, local cache
state cannot authorize input, and final verification covers the primary
time-of-check/time-of-use race. Compatibility wrappers keep unrelated
navigation paths outside the change. No new terminal fallback, polling change,
lock-bound change, commit, or publish authority was introduced.

**Confidence:** High. The remaining uncertainty is implementation-level and is
addressed by the task-local red tests plus full workspace verification.
