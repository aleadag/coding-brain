# Provider-aware Claude Code and Antigravity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent codexctl-jye`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.
>
> **Status:** Complete — all 14 implementation beads under `codexctl-jye` are closed. PR [#21](https://github.com/aleadag/coding-brain/pull/21) contains the reviewed implementation, and [CI run 29979505727](https://github.com/aleadag/coding-brain/actions/runs/29979505727) passed on Ubuntu and macOS on 2026-07-23.

**Goal:** Extend Coding Brain's local supervision from Codex to Claude Code and Antigravity CLI with provider-qualified activity, structured permission and recovery hooks, and exact-target terminal fallback, without adding a session dashboard or usage/cost tracking.

**Architecture:** Keep the current three-crate dependency direction and add a small provider model plus plain Claude and Antigravity modules in `coding-brain-core`. The binary dispatches bounded provider hook payloads, installs provider-owned configuration transactionally, and owns Brain policy; the TUI continues to consume activity projections and gains only provider labels and explicit semantic actions. Restore the former waiting-session recovery path as a narrow `RecoveryCoordinator`, retaining adaptive confidence, cooldown/deduplication, and target revalidation while excluding the removed dashboard/orchestration engine.

**Tech Stack:** Rust 2024 workspace, clap, serde/serde_json, ratatui/crossterm, existing filesystem JSONL stores, bounded `std::process::Command`, tmux/Agent Deck integrations, Cargo tests.

## Global Constraints

- Preserve dependency direction: `coding-brain -> coding-brain-tui -> coding-brain-core`.
- Do not add a daemon, public session list, session-management API, launcher, router, terminator, usage view, quota view, token accounting, or cost tracking.
- Keep the existing token/cost fields behavior-neutral; removing them is a separate cleanup.
- Provider identity is selected by the installed command's `--provider` argument and is never accepted from an untrusted payload.
- Missing provider on legacy persisted records means `AgentProvider::Codex`; rebuild derived projections and never rewrite raw JSONL history.
- Automatic terminal input requires provider-qualified native or expiring PID/start-time/TTY identity, one exact pane, a versioned recognized prompt, immediate recapture, and bounded post-action verification.
- Manual free-form text requires an explicit operator action and exact live target, but not a recognized prompt.
- Raw manual text is transient: never persist or log it; record only semantic action type, byte length, target fingerprint, and delivery outcome.
- Hook-driven automatic recovery works without the TUI; process-only automatic recovery is polled only while the TUI is running because the product does not add a daemon. Process discovery creates a Live attention activity only for recognized actionable prompt evidence, never one row per idle process.
- External commands use argument arrays, two-second or existing tighter timeouts, bounded output, and no interpolated shell command.
- Follow red-green-refactor for every behavior task; existing Codex tests are the regression baseline.
- Do not commit or push without explicit user authorization; task verification results are the review checkpoints.

---

### Task 1: Behavior-neutral session type rename

**Files:**
- Modify: `crates/coding-brain-core/src/session.rs`
- Modify: every current `CodexSession` and `RawSession` consumer found by `rg -l '\b(CodexSession|RawSession)\b' crates src tests`

**Interfaces:**
- Consumes: current `CodexSession` and `RawSession` definitions and constructors.
- Produces: the same fields and methods under `AgentSession` and `RawAgentSession`; no provider field or behavior change in this task.

**Acceptance Criteria:**
- The workspace contains no Rust identifier named `CodexSession` or `RawSession`.
- All pre-existing tests pass without changed assertions or fixtures.
- The diff contains only the mechanical rename and required imports.

- [x] **Step 1: Capture the rename baseline**

Run:

```bash
rg -n '\b(CodexSession|RawSession)\b' crates src tests
cargo test --workspace
```

Expected: the search lists the existing identifiers and the test suite passes before the rename.

- [x] **Step 2: Rename the definitions and all imports/usages**

Keep the definitions structurally identical:

```diff
-pub struct RawSession {
+pub struct RawAgentSession {

-pub struct CodexSession {
+pub struct AgentSession {
```

Rename existing `impl CodexSession`, constructor calls, test helpers, and terminal function parameters to `AgentSession`; do not alter bodies.

- [x] **Step 3: Prove the rename is behavior-neutral**

Run:

```bash
rg -n '\b(CodexSession|RawSession)\b' crates src tests
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: the search returns no matches and both Cargo commands pass.

### Task 2: Provider-qualified session and process identity

**Files:**
- Create: `crates/coding-brain-core/src/provider.rs`
- Modify: `crates/coding-brain-core/src/lib.rs`
- Modify: `crates/coding-brain-core/src/session.rs`
- Test: `crates/coding-brain-core/src/provider.rs`
- Test: `crates/coding-brain-core/src/session.rs`

**Interfaces:**
- Consumes: `AgentSession` and `RawAgentSession` from Task 1.
- Produces: `AgentProvider`, `AgentSessionKey`, `LiveProcessIdentity`, `AgentSession::key()`, and provider capability methods.

**Acceptance Criteria:**
- Provider serialization is exactly `codex`, `claude`, or `antigravity` and missing persisted provider defaults to Codex.
- Session equality/join keys distinguish identical native IDs from different providers.
- `AgentSessionKey::storage_key()` is injective for IDs containing separators and round-trips provider plus opaque ID.
- Synthetic live identity includes provider, PID, process start identity, and normalized TTY; changing any component invalidates it.
- Capability methods do not contain usage or cost capabilities.

- [x] **Step 1: Write failing provider and collision tests**

Add tests covering these exact assertions:

```rust
#[test]
fn provider_keys_do_not_collide() {
    let codex = AgentSessionKey::native(AgentProvider::Codex, "same-id");
    let claude = AgentSessionKey::native(AgentProvider::Claude, "same-id");
    assert_ne!(codex, claude);
}

#[test]
fn live_identity_expires_when_process_evidence_changes() {
    let original = LiveProcessIdentity::try_new(AgentProvider::Antigravity, 42, 9001, "/dev/pts/7").unwrap();
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
    assert_eq!(AgentSessionKey::from_storage_key(&key.storage_key()).unwrap(), key);
}
```

- [x] **Step 2: Run the focused tests and confirm failure**

Run:

```bash
cargo test -p coding-brain-core provider -- --nocapture
```

Expected: compilation fails because the provider types do not exist.

- [x] **Step 3: Add the minimal provider model**

Use these public shapes:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProvider {
    #[default]
    Codex,
    Claude,
    Antigravity,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentSessionKey {
    #[serde(default)]
    pub provider: AgentProvider,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LiveProcessIdentity {
    pub provider: AgentProvider,
    pub pid: u32,
    pub process_start_identity: u64,
    pub tty: String,
}
```

Add `provider: AgentProvider` and `process_start_identity: Option<u64>` to `RawAgentSession` and `AgentSession`. Implement `AgentSessionKey::native`, `LiveProcessIdentity::try_new`, normalized TTY comparison, `AgentSession::key`, and narrow evidence-derived capability methods. Encode projection keys as `<provider>:<session-id-byte-length>:<opaque-session-id>` and reject malformed length/provider input; do not split opaque IDs on their contents.

- [x] **Step 4: Verify provider identity and the full core suite**

Run:

```bash
cargo test -p coding-brain-core provider
cargo test -p coding-brain-core session
cargo test --workspace
```

Expected: both focused suites and the full workspace suite pass before Tasks 3, 4, and 8 branch from the provider model.

### Task 3: Provider-aware activity and lifecycle persistence

**Files:**
- Modify: `crates/coding-brain-core/src/brain_activity.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/input.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/reconcile.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`
- Create: `crates/coding-brain-core/src/session_links.rs`
- Modify: `crates/coding-brain-core/src/lib.rs`
- Modify: `src/brain/decisions.rs`
- Modify: all `SessionTarget` and `LifecycleIdentity::try_new` call sites.
- Test: `tests/hook_activity.rs`
- Test: `tests/lifecycle_hook_cli.rs`

**Interfaces:**
- Consumes: `AgentProvider` and `AgentSessionKey` from Task 2.
- Produces: explicit `SessionTarget.provider`, provider-bearing `LifecycleIdentity`, and lifecycle schema version 2 keyed by `AgentSessionKey`.

**Acceptance Criteria:**
- New activity, lifecycle, decision audit, review, and navigation records retain explicit provider.
- Legacy records with no provider load as Codex.
- Two providers with the same session ID project to separate lifecycle states.
- Lifecycle schema version 1 is rebuilt/defaulted without rewriting the raw event log; newer schema remains read-only/abstaining as today.
- A hook observation that carries native and live-process identity appends a `SessionIdentityLink`; its projection resolves either identity without rewriting earlier activity.

- [x] **Step 1: Add failing legacy and collision fixtures**

Add tests equivalent to:

```rust
#[test]
fn legacy_activity_target_defaults_to_codex() {
    let target: SessionTarget = serde_json::from_value(json!({
        "session_id": "legacy",
        "project_id": {"stable": "project"},
        "cwd": "/tmp/project",
        "provider_hints": []
    })).unwrap();
    assert_eq!(target.provider, AgentProvider::Codex);
}

#[test]
fn lifecycle_projection_is_provider_qualified() {
    let mut snapshot = LifecycleSnapshot::default();
    snapshot.apply(event(AgentProvider::Codex, "same"), 1);
    snapshot.apply(event(AgentProvider::Claude, "same"), 2);
    assert_eq!(snapshot.sessions.len(), 2);
}

#[test]
fn native_process_link_rebuilds_from_append_only_evidence() {
    let store = fixture_session_link_store();
    store.append(link(AgentProvider::Antigravity, "conversation-7", live_agy())).unwrap();
    let projection = store.read_projection().unwrap();
    assert_eq!(projection.native_for(&live_agy()), Some("conversation-7"));
}
```

- [x] **Step 2: Run focused tests and confirm the collision**

Run:

```bash
cargo test -p coding-brain-core lifecycle
cargo test --test hook_activity
```

Expected: new tests fail because lifecycle is keyed by bare `String` and `SessionTarget` has no provider.

- [x] **Step 3: Qualify the persistence boundary**

Change the core shapes to:

```rust
pub const LIFECYCLE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTarget {
    #[serde(default)]
    pub provider: AgentProvider,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub project_id: ProjectId,
    #[serde(with = "path_serde")]
    pub cwd: PathBuf,
}

pub struct LifecycleIdentity {
    provider: AgentProvider,
    session_id: String,
    turn_id: Option<String>,
    transcript_path: Option<PathBuf>,
    cwd: PathBuf,
}

pub struct LifecycleSnapshot {
    pub schema_version: u32,
    pub next_sequence: u64,
    pub sessions: BTreeMap<String, SessionLifecycleState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentityLink {
    pub schema_version: u32,
    pub recorded_at_ms: u64,
    pub provider: AgentProvider,
    pub native_session_id: String,
    pub live_process: LiveProcessIdentity,
}
```

Use `AgentSessionKey::storage_key()` for schema-2 lifecycle map keys. When reading schema 1, treat each bare map key as a Codex session ID and rebuild the schema-2 projection; do not serialize a struct directly as a JSON object key.

Retain a deserialization-only legacy `provider_hints` field or migration helper so schema-1 activity input remains readable, but do not emit it in new records. Update decision audit writes with `provider` defaulting to Codex on read. Store bounded `SessionIdentityLink` rows append-only under the existing Coding Brain state root, lock appends like other stores, and rebuild a bidirectional native/live alias projection. Reuse the activity-store compaction thresholds: compact at 32 MiB and retain the newest 10,000 unique links. Hook adapters append a link only when the selected provider, non-empty native ID, PID, process-start identity, and TTY are all present and consistent.

- [x] **Step 4: Verify persistence compatibility**

Run:

```bash
cargo test -p coding-brain-core lifecycle
cargo test -p coding-brain-core session_links
cargo test --test hook_activity
cargo test --test lifecycle_hook_cli
```

Expected: all commands pass, including legacy Codex fixtures.

### Task 4: Provider discovery and bounded Claude inventory

**Files:**
- Create: `crates/coding-brain-core/src/discovery/claude.rs`
- Create: `crates/coding-brain-core/src/discovery/antigravity.rs`
- Modify: `crates/coding-brain-core/src/discovery.rs`
- Modify: `crates/coding-brain-core/src/process.rs`
- Modify: `crates/coding-brain-core/src/lib.rs`
- Create: `tests/fixtures/claude-agents-interactive.json`
- Create: `tests/fixtures/claude-agents-background.json`
- Test: provider modules and current discovery tests.

**Interfaces:**
- Consumes: `AgentSession`, `AgentProvider`, and `LiveProcessIdentity`.
- Produces: `scan_agent_sessions_with_state`, `ClaudeInventoryEntry`, one shared process snapshot, five-second Claude inventory cache, and process fallback for `claude` and `agy`.

**Acceptance Criteria:**
- Codex discovery results remain unchanged except for explicit provider identity.
- `claude agents --json` has a two-second timeout and one-MiB output cap and parses interactive/background entries tolerantly.
- Missing command, unsupported flag, malformed/oversized output, or timeout retains timestamped stale inventory and uses process fallback.
- `agy` process discovery yields provider/PID/start-time/TTY identity with unknown status.
- Process discovery runs once per scan and never treats cwd, CPU, or terminal text as authorization evidence.

- [x] **Step 1: Write provider inventory and fallback tests**

Cover interactive UUIDs, background attach IDs, unknown JSON fields, malformed JSON, timeout, oversized output, stale retention, and process-only identities. The parser test must assert:

```rust
let entry = parse_inventory_entry(&fixture).unwrap();
assert_eq!(entry.provider, AgentProvider::Claude);
assert_eq!(entry.session_id.as_deref(), Some("session-uuid"));
assert_eq!(entry.attach_id.as_deref(), Some("agent-id"));
```

- [x] **Step 2: Run discovery tests and confirm failure**

Run:

```bash
cargo test -p coding-brain-core discovery -- --nocapture
```

Expected: new provider module imports or assertions fail.

- [x] **Step 3: Add plain provider discovery modules**

Use these bounded interfaces without introducing a provider trait:

```rust
pub struct ClaudeInventoryEntry {
    pub session_id: Option<String>,
    pub attach_id: Option<String>,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub started_at: Option<u64>,
    pub status: Option<String>,
}

pub struct ProviderDiscoveryState {
    pub transcript_assignments: TranscriptAssignmentState,
    pub claude_inventory: ClaudeInventoryCache,
}

pub struct ClaudeInventoryCache {
    pub refreshed_at: Option<Instant>,
    pub last_good: Vec<ClaudeInventoryEntry>,
    pub last_error: Option<String>,
}

pub fn scan_agent_sessions_with_state(state: &mut ProviderDiscoveryState) -> Vec<AgentSession>;
```

Scan `ps -eo pid=,ppid=,tty=,%cpu=,rss=,etimes=,comm=,args=` once, recognize exact executable basenames, merge stronger structured evidence over process evidence, and preserve existing ten-second Codex transcript caching.

- [x] **Step 4: Verify discovery and Codex regression**

Run:

```bash
cargo test -p coding-brain-core discovery
cargo test --test integration_tests discovery
```

Expected: provider fixtures and existing Codex discovery tests pass.

### Task 5: Bounded provider lifecycle hook adapters

**Files:**
- Create: `src/provider_hooks/mod.rs`
- Create: `src/provider_hooks/codex.rs`
- Create: `src/provider_hooks/claude.rs`
- Create: `src/provider_hooks/antigravity.rs`
- Modify: `src/lib.rs`
- Modify: `src/lifecycle_hook.rs`
- Modify: `src/main.rs`
- Create: `tests/fixtures/hooks/claude-stop.json`
- Create: `tests/fixtures/hooks/antigravity-stop.json`
- Create: `tests/fixtures/hooks/antigravity-post-tool-use.json`
- Test: `tests/lifecycle_hook_cli.rs`

**Interfaces:**
- Consumes: provider-qualified lifecycle persistence from Task 3.
- Produces: hidden `--provider codex|claude|antigravity`, provider-selected parsing, and provider-qualified lifecycle/activity events.

**Acceptance Criteria:**
- Provider is taken only from CLI dispatch and cannot be changed by payload fields.
- Omitted `--provider` defaults to Codex only so already-installed legacy Codex hook commands keep working until init refreshes them.
- Each adapter caps stdin at 64 KiB, rejects missing/empty actionable identity, ignores unknown transcript records, and redacts diagnostics.
- Claude snake_case fields and Antigravity camelCase fields map to the same lifecycle model.
- Provider hook parent-process evidence is bounded and recorded only as supplemental live identity; it can link a native hook session to the exact Codex, Claude, or Antigravity process for guarded fallback.
- Existing Codex hook fixtures produce byte-compatible decisions and equivalent activity.

- [x] **Step 1: Write failing adapter CLI tests**

Exercise the real binary with:

```bash
cargo run --quiet -- --lifecycle-hook --provider claude
cargo run --quiet -- --lifecycle-hook --provider antigravity
```

Pipe the fixture payloads from the test harness and assert provider-qualified JSONL output; assert an oversized payload writes no actionable activity.

- [x] **Step 2: Confirm provider dispatch is absent**

Run:

```bash
cargo test --test lifecycle_hook_cli
```

Expected: clap rejects `--provider` or the provider fixtures fail to parse.

- [x] **Step 3: Add provider-selected bounded parsers**

Use a shared parsed form:

```rust
pub struct ParsedLifecycleHook {
    pub identity: LifecycleIdentity,
    pub event: LifecycleEventKind,
    pub tool_use_id: Option<String>,
    pub tool_name: Option<String>,
    pub outcome: Option<ActivityOutcome>,
    pub live_process: Option<LiveProcessIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookInputError {
    InvalidJson,
    UnsupportedEvent,
    Missing(&'static str),
    Empty(&'static str),
    TooLarge,
}

pub fn parse_lifecycle(
    provider: AgentProvider,
    raw: &[u8],
) -> Result<ParsedLifecycleHook, HookInputError> {
    match provider {
        AgentProvider::Codex => codex::parse_lifecycle(raw),
        AgentProvider::Claude => claude::parse_lifecycle(raw),
        AgentProvider::Antigravity => antigravity::parse_lifecycle(raw),
    }
}
```

Keep `read_bounded_hook_input` as the single stdin bound and add clap `ValueEnum` conversion that maps only the three literal provider names.

- [x] **Step 4: Verify all hook fixtures**

Run:

```bash
cargo test --test lifecycle_hook_cli
cargo test --test hook_activity
```

Expected: Codex, Claude, and Antigravity fixture tests pass.

### Task 6: Structured permission guards for all providers

**Files:**
- Modify: `src/brain/permission_hook.rs`
- Modify: `src/provider_hooks/codex.rs`
- Modify: `src/provider_hooks/claude.rs`
- Modify: `src/provider_hooks/antigravity.rs`
- Modify: `src/main.rs`
- Create: `tests/fixtures/hooks/claude-permission-request.json`
- Create: `tests/fixtures/hooks/antigravity-pre-tool-use.json`
- Test: `src/brain/permission_hook.rs`
- Test: `tests/hook_activity.rs`

**Interfaces:**
- Consumes: provider parsers from Task 5 and existing `evaluate_request`/persistence ordering.
- Produces: shared `PermissionHookRequest`, provider-specific response encoders, and abstaining fail-safe behavior.

**Acceptance Criteria:**
- Codex and Claude emit documented `hookSpecificOutput.decision.behavior` allow/deny responses.
- Antigravity emits exactly `decision: allow|deny|ask` with optional bounded reason; Coding Brain never emits `force_ask` or permission overrides.
- Brain abstention, unsupported tool, malformed input, persistence failure, and inference failure never become allow.
- Brain allow cannot override provider deny/ask policy included in the hook evidence.
- Antigravity `force_ask` and permission overrides are parsed as provider policy evidence but never emitted by Coding Brain.
- Decision/activity persistence occurs before executable output and remains provider-qualified.

- [x] **Step 1: Write failing response-schema tests**

Use exact JSON assertions:

```rust
assert_eq!(claude_allow, json!({
    "hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": {"behavior": "allow"}
    }
}));

assert_eq!(antigravity_abstain, json!({
    "decision": "ask",
    "reason": "Coding Brain abstained"
}));
```

Also assert that the output stream is empty or native-prompt-preserving on malformed Codex/Claude input according to each documented contract.

- [x] **Step 2: Run permission tests and confirm failure**

Run:

```bash
cargo test permission_hook -- --nocapture
cargo test --test hook_activity permission
```

Expected: Claude/Antigravity response assertions fail.

- [x] **Step 3: Separate policy evaluation from wire encoding**

Introduce:

```rust
pub struct PermissionHookRequest {
    pub provider: AgentProvider,
    pub lifecycle: LifecycleIdentity,
    pub project: String,
    pub tool_name: String,
    pub command: Option<String>,
    pub tool_use_id: Option<String>,
    pub provider_policy: ProviderPermissionPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPermissionPolicy {
    PermitsBrainDecision,
    RequiresAsk,
    Denies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AntigravityDecision {
    Allow,
    Deny,
    Ask,
}

pub enum ProviderPermissionResponse {
    CodexOrClaude(HookEvaluation),
    Antigravity { decision: AntigravityDecision, reason: Option<String> },
}
```

Reuse the current safety-first `evaluate_request`, adaptive threshold, and persistence sequence. Encode with a provider `match`; do not add a provider trait.

- [x] **Step 4: Verify structured guards and Codex stability**

Run:

```bash
cargo test permission_hook
cargo test --test hook_activity
```

Expected: all fail-safe and schema tests pass.

### Task 7: Restore narrow recovery evaluation and structured Stop continuation

**Files:**
- Create: `src/brain/recovery.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/brain/activity.rs`
- Modify: `src/brain/evals.rs`
- Modify: `src/brain/client.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/provider_hooks/antigravity.rs`
- Modify: `src/main.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Test: `src/brain/recovery.rs`
- Test: `tests/brain_tui_smoke.rs`

**Interfaces:**
- Consumes: discovered sessions from Task 4, provider Stop payloads from Task 5, current local Brain client, and semantic terminal delivery from Task 8.
- Produces: `RecoveryCoordinator`, `RecoveryTargetSnapshot`, `RecoveryDecision`, hidden `--recovery-hook`, and `BrainActions::poll_recovery`.

**Acceptance Criteria:**
- Waiting-session recovery again supports a default `continue` prompt, matching the pre-`b973d753` behavior without restoring dashboard orchestration.
- Decisions bind to provider/session/turn/tool/live-process/prompt evidence and expire if any actionable evidence changes.
- Automatic execution uses the existing adaptive confidence threshold and a ten-second cooldown keyed by a canonical provider-qualified recovery attempt; an exclusive durable reservation prevents separate Stop-hook and TUI processes from sending twice.
- Antigravity Stop returns structured `decision: continue` with a bounded reason only after a validated auto-mode decision.
- `--recovery-hook` first persists the Stop lifecycle event, then evaluates recovery; it replaces the ordinary lifecycle command for managed Stop entries so the event is not duplicated.
- Codex/Claude hook-triggered recovery and process-only TUI polling use guarded terminal delivery; off/on modes abstain or remain advisory as today.
- Process-only polling does not run when the TUI is absent and no background daemon is introduced.
- A hookless process receives a provider-qualified attention activity only after a supported prompt matcher finds actionable evidence; idle processes remain absent from Live and no generic process picker is added.
- TUI polling never runs process discovery, terminal capture, or local-model inference on the render thread; the queue holds at most 64 candidates, permits two concurrent evaluations, and permits one inflight job per recovery attempt.

- [x] **Step 1: Write recovery regression tests from the removed engine contract**

Test these exact behaviors:

```rust
#[test]
fn accepted_waiting_recovery_defaults_to_continue() {
    let decision = evaluate_recovery(fixture_target(), recovery_suggestion(None, 0.91), 0.60);
    assert_eq!(decision, RecoveryDecision::Continue("continue".into()));
}

#[test]
fn changed_target_expires_suggestion() {
    let pending = PendingRecovery::bound(fixture_target());
    let mut changed = fixture_target();
    changed.turn_id = Some("new-turn".into());
    assert!(!pending.matches(&changed));
}

#[test]
fn cooldown_is_provider_qualified_and_deduplicates() {
    let mut coordinator = RecoveryCoordinator::new(Duration::from_secs(10));
    assert_eq!(coordinator.reserve(codex_attempt_key(), 1_000), ReservationOutcome::Reserved);
    assert_eq!(coordinator.reserve(codex_attempt_key(), 1_001), ReservationOutcome::Duplicate);
    assert_eq!(coordinator.reserve(claude_attempt_key(), 1_001), ReservationOutcome::Reserved);
}

#[test]
fn stop_hook_and_tui_poll_share_one_durable_reservation() {
    let store = fixture_recovery_store();
    assert_eq!(store.reserve(native_stop_attempt(), 1_000).unwrap(), ReservationOutcome::Reserved);
    assert_eq!(store.reserve(tui_view_of_same_stop(), 1_001).unwrap(), ReservationOutcome::Duplicate);
}
```

Add hook tests that auto mode returns Antigravity `{"decision":"continue","reason":"..."}` and on/off/error paths return no continuation.

- [x] **Step 2: Confirm recovery tests fail**

Run:

```bash
cargo test recovery -- --nocapture
```

Expected: recovery types and `send` parsing are absent.

- [x] **Step 3: Add a recovery-only decision path**

Keep permission `RuleAction` unchanged and introduce separate types:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    Continue(String),
    LeaveAlone,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecoverySuggestion {
    pub decision: RecoveryDecision,
    pub reasoning: String,
    pub confidence: f64,
    pub suggested_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RecoveryEpoch {
    LifecycleSequence(u64),
    ProcessPrompt {
        last_message_ts: u64,
        prompt_fingerprint: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecoveryAttemptKey {
    pub session: AgentSessionKey,
    pub epoch: RecoveryEpoch,
}

pub struct RecoveryTargetSnapshot {
    pub attempt: RecoveryAttemptKey,
    pub turn_id: Option<String>,
    pub live_process: Option<LiveProcessIdentity>,
    pub status: SessionStatus,
    pub last_message_ts: u64,
    pub pending_tool_use_id: Option<String>,
    pub prompt_fingerprint: Option<u64>,
}

pub struct PendingRecovery {
    pub suggestion: RecoverySuggestion,
    pub target: RecoveryTargetSnapshot,
}

pub struct RecoveryCoordinator {
    inflight: HashSet<RecoveryAttemptKey>,
    pending: HashMap<RecoveryAttemptKey, PendingRecovery>,
    reservations: RecoveryReservationStore,
    cooldown_duration: Duration,
    work_queue: VecDeque<RecoveryWork>,
    result_tx: SyncSender<RecoveryResult>,
    result_rx: Receiver<RecoveryResult>,
    max_inflight: usize,
}

pub struct RecoveryWork {
    pub target: RecoveryTargetSnapshot,
}

pub struct RecoveryResult {
    pub target: RecoveryTargetSnapshot,
    pub suggestion: Result<RecoverySuggestion, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationOutcome {
    Reserved,
    Duplicate,
    Cooldown,
}

pub struct RecoveryReservationStore {
    activity: ActivityStore,
    cooldown_duration: Duration,
}
```

`RecoveryCoordinator` owns in-process inflight keys and pending suggestions, but authority comes from `RecoveryReservationStore`, which uses the existing activity-file exclusive lock to perform read-check-append atomically. Hook-backed attempts use the lifecycle sequence written before evaluation; TUI polling reads that same sequence. Process-only attempts use last-message timestamp plus recognized prompt fingerprint. Under the lock, reject an existing attempt key and enforce the ten-second same-session cooldown before appending the deterministic Observed activity. A lock timeout or persistence error abstains.

Use `adaptive_threshold(Some("recovery"))`, bind each result to `RecoveryTargetSnapshot`, and persist evaluating/delivered or delivery-failed activity after the reservation. Keep a `VecDeque<RecoveryWork>` capped at 64 and a bounded result channel. `BrainActions::poll_recovery(&self) -> Vec<String>` only drains completed results, queues eligible keys, and starts at most two bounded worker threads; `ps`, provider inventory, pane capture, and local-model inference stay on those workers. A full queue or two active evaluations causes abstention plus one deduplicated diagnostic rather than blocking or creating more work. Invoke polling from the existing TUI tick/refresh cadence without passing sessions into the view.

When polling finds a process-only supported permission or recovery prompt, append one deduplicated attention activity whose `SessionTarget` uses the synthetic live identity. That activity is the only process-only manual-action anchor. Do not emit activity merely because an idle provider process exists, and do not add a process/session selection API.

- [x] **Step 4: Verify restored recovery semantics**

Run:

```bash
cargo test recovery
cargo test --test brain_tui_smoke
cargo test evals
```

Expected: target expiry, adaptive threshold, cooldown/deduplication, structured Stop, bounded-queue saturation, render-thread non-blocking, and process-only polling tests pass.

### Task 8: Guarded semantic terminal actuation

**Files:**
- Modify: `crates/coding-brain-core/src/terminals/mod.rs`
- Modify: `crates/coding-brain-core/src/terminals/tmux.rs`
- Modify: `crates/coding-brain-core/src/terminals/kitty.rs`
- Modify: other backend signatures required by `AgentSession`.
- Create: `tests/fixtures/claude-recovery-pane.txt`
- Create: `tests/fixtures/antigravity-permission-pane.txt`
- Create: `tests/fixtures/antigravity-recovery-pane.txt`
- Test: terminal module unit tests.

**Interfaces:**
- Consumes: `AgentSession`, `LiveProcessIdentity`, and provider identity from Task 2.
- Produces: `TerminalSessionAction`, `PromptEvidence`, `TerminalActionOutcome`, exact tmux process-ancestry binding, and `execute_guarded_action`.

**Acceptance Criteria:**
- Semantic Allow, Deny, Continue, and explicit Text actions are supported.
- Automatic actions require an exact unexpired process identity, exactly one pane whose ancestry contains the agent PID, recognized complete provider prompt, same recapture fingerprint/tool identity, and post-action prompt advancement.
- Manual Text skips prompt recognition only; it still requires explicit caller intent and exact live process/pane binding, is single-line UTF-8 bounded to 4 KiB, rejects NUL/control characters, and is delivered literally.
- Ambiguous panes, changed PID/start time/TTY, unknown/incomplete/changed prompt, capture limit/timeout, send failure, and failed post-verification produce no success result.
- Existing Codex Kitty/tmux guarded approval tests remain green.
- Terminal diagnostics and action outcomes never contain raw manual text.

- [x] **Step 1: Write failing semantic and race tests**

Use these public action shapes in tests:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalSessionAction {
    Allow,
    Deny,
    Continue,
    Text(String),
}

let result = execute_guarded_action_with(
    &session,
    TerminalSessionAction::Continue,
    &fake_backend,
);
assert_eq!(result.unwrap().action, TerminalSessionAction::Continue);
```

Fixtures must cover changed recapture, two matching panes, wrong process start identity, a post-capture where the prompt remains unchanged, oversized/manual multiline input, control characters, text that resembles tmux key names such as `C-c`, and a secret-bearing manual string that never appears in persisted activity or an error.

- [x] **Step 2: Run terminal tests and confirm failure**

Run:

```bash
cargo test -p coding-brain-core terminals -- --nocapture
```

Expected: semantic action and ancestry APIs are absent.

- [x] **Step 3: Generalize the existing bounded capture protocol**

Add:

```rust
pub struct PromptEvidence {
    pub provider: AgentProvider,
    pub action: TerminalSessionAction,
    pub backend: Terminal,
    pub target: String,
    pub pattern_version: u16,
    pub fingerprint: u64,
    pub tool_use_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalActionOutcome {
    pub action: TerminalSessionAction,
    pub backend: Terminal,
    pub target: String,
    pub prompt_cleared: bool,
}

trait GuardedTerminalBackend {
    fn resolve_exact_target(&self, session: &AgentSession) -> Result<String, String>;
    fn capture(&self, target: &str) -> Result<PaneCapture, String>;
    fn send_literal(&self, target: &str, text: &str) -> Result<(), String>;
    fn send_keys(&self, target: &str, keys: &[&str]) -> Result<(), String>;
}

pub fn execute_guarded_action(
    session: &AgentSession,
    action: TerminalSessionAction,
) -> Result<TerminalActionOutcome, String>;

fn execute_guarded_action_with(
    session: &AgentSession,
    action: TerminalSessionAction,
    backend: &dyn GuardedTerminalBackend,
) -> Result<TerminalActionOutcome, String>;
```

For tmux, list pane ID, pane TTY, and pane PID in one bounded command, reject non-unique TTY/ancestry matches, verify `/proc` or bounded `ps` ancestry contains the exact agent PID/start identity, capture at most 80 lines/64 KiB, and recapture immediately. Fixed semantic actions use provider-specific key arrays. Manual text must be one UTF-8 line of at most 4,096 bytes with no NUL or other ASCII control characters; deliver it with `tmux send-keys -l -- <text>` and send Enter in a separate command. Then perform one bounded post-capture. Keep provider prompt matchers as plain functions with explicit pattern version constants.

- [x] **Step 4: Verify terminal safety regressions**

Run:

```bash
cargo test -p coding-brain-core terminals
cargo test -p coding-brain-core guarded
```

Expected: all semantic, ambiguity, race, and existing Codex approval tests pass.

### Task 9: Exact provider-aware navigation

**Files:**
- Modify: `src/runtime/navigation.rs`
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: `tests/agent_deck_navigation.rs`
- Test: `src/runtime/navigation.rs`

**Interfaces:**
- Consumes: explicit `SessionTarget.provider`, provider discovery, and exact terminal focus from Tasks 3, 4, and 8.
- Produces: provider-aware Agent Deck matching, Claude background attach, and focus fallback without cwd authorization.

**Acceptance Criteria:**
- Agent Deck matching requires exact provider plus opaque session ID; cwd and display-name matches are never authoritative.
- Claude background sessions with attach ID produce `claude attach <id>` using argument arrays.
- Claude foreground, Codex, and Antigravity fall back to exact terminal focus when native attach is unavailable.
- No match and multiple matches remain bounded errors and never focus or inject by cwd.

- [x] **Step 1: Write failing cross-provider navigation tests**

Add cases where Codex and Claude share `session_id = "same"` and assert only the requested provider matches. Assert exact Claude command construction:

```rust
assert_eq!(
    plan,
    NavigationPlan::External(ExternalCommand::new("claude", ["attach", "agent-42"]))
);
```

- [x] **Step 2: Confirm current provider-hint matching fails**

Run:

```bash
cargo test --test agent_deck_navigation
cargo test navigation -- --nocapture
```

Expected: cross-provider and Claude attach cases fail.

- [x] **Step 3: Replace provider hints with exact provider matching**

Use:

```rust
fn matches_target(session: &DeckSession, target: &SessionTarget) -> bool {
    session.provider == target.provider && session.id == target.session_id
}
```

Add `provider: AgentProvider` to validated `DeckSession`, derived from the bounded Agent Deck tool/profile fields, and reject rows whose provider cannot be identified. Resolve in order: exact Agent Deck entry, Claude native attach ID from current discovery, exact terminal focus via live process identity. Remove cwd fallback from action/navigation authority; cwd may remain display context only.

- [x] **Step 4: Verify navigation behavior**

Run:

```bash
cargo test --test agent_deck_navigation
cargo test navigation
```

Expected: provider collision, attach, fallback, ambiguity, and missing-tool tests pass.

### Task 10: Transactional provider hook installers

**Files:**
- Create: `src/init/provider_hooks/mod.rs`
- Create: `src/init/provider_hooks/codex.rs`
- Create: `src/init/provider_hooks/claude.rs`
- Create: `src/init/provider_hooks/antigravity.rs`
- Modify: `src/init/hooks.rs`
- Modify: `src/init/mod.rs`
- Modify: `src/init/phases.rs`
- Test: installer modules.

**Interfaces:**
- Consumes: provider-selected internal hook CLI from Tasks 5–7.
- Produces: `ProviderHookPlan`, `ManagedFileEdit`, `stage_provider_hooks`, `apply_hook_transaction`, provider-specific discovery/removal.

**Acceptance Criteria:**
- Codex targets existing global/project `hooks.json`, Claude targets `~/.claude/settings.json`, and Antigravity targets `~/.gemini/config/hooks.json`.
- Install parses and stages every selected file before replacement; each replacement is atomic, a durable short-lived journal enables crash recovery, and rollback never overwrites a concurrent user edit.
- Unrelated settings, events, matchers, handlers, and disabled state are preserved.
- Exact unmodified Coding Brain entries are idempotently replaced/removed; user-modified formerly managed entries are preserved and reported.
- Invalid/wrong-shaped JSON refuses the transaction without changing any selected file.
- No status-line/title configuration is installed or modified.
- Managed config input is capped at 1 MiB per file and 3 MiB total; targets must be regular non-symlink files.
- The crash journal is owner-only, preserves original modes for restoration, and its contents never enter stdout, stderr, diagnostics, or activity logs.

- [x] **Step 1: Write failing merge, rollback, and crash-recovery tests**

Create temp-directory tests for empty/missing files, unrelated entries, duplicate Codex scopes, invalid JSON, wrong root shape, idempotence, exact removal, and user-modified managed commands. Add a synthetic failure on the second file that restores the first byte-for-byte, a simulated process exit after the first replacement that the next init recovers from the journal, and a concurrent edit whose hash mismatch prevents both replacement and rollback overwrite. Assert rejection of symlink/non-regular targets, files over 1 MiB, and aggregate input over 3 MiB; on Unix assert journal mode `0600` and restored original file mode. Include a secret-bearing original and assert it never appears in rendered errors or diagnostics.

- [x] **Step 2: Run init hook tests and confirm failure**

Run:

```bash
cargo test init::provider_hooks -- --nocapture
cargo test init::hooks
```

Expected: provider modules and transactional rollback are absent.

- [x] **Step 3: Add plain provider staging and one coordinator**

Use:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedFileEdit {
    pub path: PathBuf,
    pub original: Option<Vec<u8>>,
    pub original_mode: Option<u32>,
    pub original_hash: Option<String>,
    pub replacement: Vec<u8>,
    pub replacement_hash: String,
}

pub struct ProviderHookPlan {
    pub provider: AgentProvider,
    pub edits: Vec<ManagedFileEdit>,
    pub preserved_modified_entries: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookTransactionJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub edits: Vec<ManagedFileEdit>,
    pub replaced_paths: Vec<PathBuf>,
}

pub fn stage_provider_hooks(
    providers: &[AgentProvider],
    scope: HookScope,
) -> io::Result<Vec<ProviderHookPlan>>;

pub fn apply_hook_transaction(plans: &[ProviderHookPlan]) -> io::Result<()>;
```

Reuse current Codex ownership markers and atomic sibling-file replacement. Read at most 1 MiB from each provider file and refuse a selected change set above 3 MiB. Use `symlink_metadata` and accept only missing or regular non-symlink targets. Before replacement, write and fsync `brain/hook-install-transaction.json` under the Coding Brain state root with original bytes/absence, original Unix mode where applicable, and original/replacement hashes. Create the journal through an atomic sibling temp file with owner-only permissions (`0600` through `OpenOptionsExt` on Unix; current-user ACL inheritance on Windows). Recheck the current file against `original_hash` immediately before each atomic replacement and fsync the containing directory after rename. On failure or the next init/doctor run, restore a replaced file and its original mode only when its current hash still equals `replacement_hash`; preserve and report any concurrent edit. Never include original/replacement bytes in messages. Remove and directory-fsync the journal only after every replacement succeeds or safe recovery finishes.

Encode documented Claude hooks under `hooks`; encode Antigravity top-level named Coding Brain hook entries for `PreToolUse`, `PostToolUse`, invocation events, and `Stop` with explicit `--provider` commands. Managed Stop entries invoke `--recovery-hook --provider ...`; other lifecycle entries invoke `--lifecycle-hook --provider ...`, and permission entries invoke `--permission-hook --provider ...`.

- [x] **Step 4: Verify installer preservation and rollback**

Run:

```bash
cargo test init::provider_hooks
cargo test init::hooks
```

Expected: all merge, exact-ownership, idempotence, size/type/mode/confidentiality, hash-precondition, rollback, and crash-recovery tests pass.

### Task 11: Provider-selecting init and marker compatibility

**Files:**
- Modify: `src/main.rs`
- Modify: `src/init/mod.rs`
- Modify: `src/init/phases.rs`
- Modify: `src/init/prompt.rs`
- Modify: `src/init/state.rs`
- Modify: `src/init/marker.rs`
- Modify: `tests/integration_tests.rs`
- Modify: `nix/tests/home-manager-module.nix`

**Interfaces:**
- Consumes: transactional provider installers from Task 10.
- Produces: positional provider selectors, interactive provider selection, stable marker phases, legacy aliases/warnings, and provider-aware check/upgrade/remove.

**Acceptance Criteria:**
- `init codex|claude|antigravity`, combinations, and `all` work; `all` mixed with another selector is rejected.
- Bare interactive init detects executables, selects detected providers by default, and still permits an installed-later provider.
- New noninteractive usage requires a provider; provider-less noninteractive remains Codex-only for one release and prints the exact replacement warning.
- `--plugin-only` remains a one-release deprecated Codex-hook-only alias.
- Provider selectors conflict with `--check`, `--upgrade`, `--remove`, `--reset`, and `--purge`.
- Marker writes `hooks.codex`, `hooks.claude`, and `hooks.antigravity`; legacy `plugin` reads as Codex.
- Check/upgrade use recorded providers; remove removes all exact managed provider entries.

- [x] **Step 1: Write failing clap and marker tests**

Assert parsing and compatibility including:

```rust
assert_eq!(parse_init(["init", "codex", "claude"]).providers,
           vec![AgentProvider::Codex, AgentProvider::Claude]);
assert!(parse_init(["init", "all", "codex"]).is_err());
assert_eq!(legacy_marker.selected_providers(), vec![AgentProvider::Codex]);
```

Capture stderr for provider-less `--non-interactive` and `--plugin-only` and compare the documented deprecation replacement.

- [x] **Step 2: Confirm selector tests fail**

Run:

```bash
cargo test init -- --nocapture
cargo test --test integration_tests init
```

Expected: positional provider selectors are rejected.

- [x] **Step 3: Add provider selection without changing Brain onboarding**

Add a clap value enum and normalize once:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum InitProvider {
    Codex,
    Claude,
    Antigravity,
    All,
}

fn normalize_init_providers(values: &[InitProvider]) -> Result<Vec<AgentProvider>, String>;
```

Pass the normalized provider list only to provider hook phases; Brain and skills phases remain provider-neutral. Preserve administrative semantics and store stable phase keys in the marker.

- [x] **Step 4: Verify init and Home Manager compatibility**

Run:

```bash
cargo test init
cargo test --test integration_tests init
nix build path:.#checks.x86_64-linux.home-manager-module
```

Expected: selector, compatibility warning, marker migration, administrative mode, and module tests pass.

### Task 12: Provider-specific doctor health

**Files:**
- Modify: `src/doctor.rs`
- Modify: `crates/coding-brain-core/src/health.rs`
- Modify: `crates/coding-brain-core/src/terminals/mod.rs`
- Test: doctor and health unit tests.

**Interfaces:**
- Consumes: provider setup discovery, selected marker providers, provider discovery, navigation, and terminal capabilities.
- Produces: one health row per provider and separate navigation/input capability diagnostics.

**Acceptance Criteria:**
- Each provider reports current, degraded, stale, unavailable, or skipped using existing Pass/Advisory/Fail/Skipped severity.
- An unselected absent provider is skipped; a recorded provider with a missing executable is advisory; unsafe/broken managed definitions are failures.
- Messages name the provider and exact `coding-brain init <provider>` repair command.
- Discovery diagnostics report counts by provider without exposing a session list.
- Terminal diagnostics distinguish Agent Deck, Claude attach, guarded input, and focus-only fallback.

- [x] **Step 1: Write failing status-matrix tests**

Table-drive every provider/status combination and assert name, severity, message, and fix. Include selected/unselected executable absence and stale managed command cases.

- [x] **Step 2: Run doctor tests and confirm failure**

Run:

```bash
cargo test doctor -- --nocapture
cargo test -p coding-brain-core health
```

Expected: only Codex-specific rows exist.

- [x] **Step 3: Add provider health classification**

Use an internal classification:

```rust
enum ProviderSetupState {
    Current,
    Degraded(String),
    Stale(String),
    Unavailable(String),
    Skipped,
}

fn check_provider(provider: AgentProvider, recorded: bool) -> Check;
```

Render one row per provider and retain existing global binary, endpoint, lifecycle store, project identity, and terminal checks.

- [x] **Step 4: Verify doctor text and JSON**

Run:

```bash
cargo test doctor
cargo test -p coding-brain-core health
```

Expected: status matrix and existing doctor tests pass.

### Task 13: Provider labels and explicit manual session actions in Live

**Files:**
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: `crates/coding-brain-tui/src/brain_app.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/review.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/scorecard.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `tests/brain_tui_smoke.rs`
- Modify: `tests/headless_activity.rs`

**Interfaces:**
- Consumes: provider-bearing activity, guarded terminal actions, recovery polling, and exact navigation.
- Produces: concise provider labels and explicit Live action input using `SessionActionRequest`.

**Acceptance Criteria:**
- Provider-backed Live, Review, and Scorecard rows show Codex, Claude, or Antigravity labels.
- No provider session collection, token, quota, cost, burn-rate, or usage UI is added.
- Enter retains source navigation.
- In Live, `x` opens semantic action mode; `a`, `d`, `c`, and `t` select allow, deny, continue, or bounded manual text; Escape cancels.
- Manual action requires a selected activity with an exact session target and surfaces delivery failure as bounded status text.
- TUI refresh polls the narrow recovery coordinator without exposing sessions to the view.
- A process-only manual action is available only from the deduplicated attention activity created for recognized actionable evidence; idle processes do not appear as selectable rows.
- Review and Scorecard remain read-only; Live has no persistent composer, sent-message history, or session picker.
- Raw manual text is dropped after delivery and never enters activity, decision, correction, diagnostic, or status-message persistence.

- [x] **Step 1: Write failing rendering and input tests**

Add snapshots/assertions that provider labels render and forbidden usage/cost headings do not. Exercise:

```rust
app.handle_key(key('x'));
app.handle_key(key('c'));
assert_eq!(mock.session_actions()[0].action, TerminalSessionAction::Continue);
```

Also test no selected target, text bounds, Escape, delivery failure, Review/Scorecard read-only behavior, secret-bearing text absence from runtime records/errors, and that existing `c` correction behavior outside action mode remains unchanged.

- [x] **Step 2: Run TUI tests and confirm failure**

Run:

```bash
cargo test -p coding-brain-tui brain_app -- --nocapture
cargo test --test brain_tui_smoke
cargo test --test headless_activity
```

Expected: provider label and action-mode assertions fail.

- [x] **Step 3: Add the smallest runtime action contract**

Extend the runtime with:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionActionRequest {
    pub target: SessionTarget,
    pub action: TerminalSessionAction,
}

pub trait BrainActions: Send + Sync {
    fn record_correction(&self, correction: CorrectionInput) -> Result<(), String>;
    fn mark_canonical(&self, decision_id: &str, note: Option<String>) -> Result<(), String>;
    fn send_session_action(&self, request: SessionActionRequest) -> Result<(), String>;
    fn poll_recovery(&self) -> Vec<String>;
}
```

Add one transient `BrainInput::SessionAction` state and provider label formatting from `SessionTarget.provider`; do not pass `AgentSession` lists into `BrainApp` or view functions. After action dispatch, zero/drop the input buffer as soon as practical and persist only action kind, byte length, target fingerprint, and delivery state.

- [x] **Step 4: Verify UI boundary and manual actions**

Run:

```bash
cargo test -p coding-brain-tui
cargo test --test brain_tui_smoke
cargo test --test headless_activity
cargo test --test removed_surfaces
```

Expected: provider/action tests pass and removed dashboard/usage surfaces remain absent.

### Task 14: Documentation, examples, and final regression gates

**Files:**
- Modify: `README.md`
- Modify: `docs/index.md`
- Modify: `docs/quickstart.md`
- Modify: `docs/configuration.md`
- Modify: `docs/reference.md`
- Modify: `docs/troubleshooting.md`
- Modify: `docs/terminal-support.md`
- Modify: `docs/llms.txt`
- Modify: `nix/home-manager.nix`
- Modify: `nix/tests/home-manager-module.nix`
- Review: `.github/workflows/ci.yml`
- Review: `docs/decisions/ADR-0004-provider-aware-guards-and-terminal-actuation.md`
- Review: `.internal/specs/2026-07-22-provider-aware-claude-antigravity-design.md`

**Interfaces:**
- Consumes: completed provider behavior from Tasks 1–13.
- Produces: user-facing provider/init/hook/capability/recovery documentation and final verification evidence.

**Acceptance Criteria:**
- Documentation lists supported providers, exact init selectors, managed paths, preservation/rollback behavior, and provider capability/degradation matrix.
- Documentation explains structured permission guards for all providers, Antigravity PreToolUse and Stop responses, Claude background attach, automatic/manual recovery, and exact-target terminal safety.
- Documentation states plainly that Coding Brain is Brain activity rather than a session dashboard and does not collect or display usage/cost.
- Home Manager examples expose provider selection without changing compatibility storage paths.
- All workspace format, test, clippy, and build gates pass and `git diff --check` is clean.
- Provider command and terminal runners are injectable so deterministic safety tests do not require installed provider CLIs or tmux.
- Existing Ubuntu and macOS CI test jobs pass before the branch is considered release-ready; absent live provider tools produce diagnostic skips, not false safety coverage.

- [x] **Step 1: Add executable documentation assertions where available**

Extend CLI/help and Home Manager tests to assert `codex`, `claude`, `antigravity`, `all`, the three managed paths, and the provider-less noninteractive deprecation text. Ensure provider inventory and guarded terminal tests inject command/backend runners and execute their timeout, output-cap, ambiguity, recapture, and post-verification paths without requiring real provider binaries or tmux.

- [x] **Step 2: Update human-facing documentation from shipped behavior**

Use a compact capability table with rows for structured discovery, lifecycle hooks, permission guard, Stop continuation, native attach, terminal focus, guarded input, and transcript context. Mark unsupported capabilities as unavailable/degraded, and include one explicit row stating `Usage/cost: intentionally not collected`.

- [x] **Step 3: Run documentation and module checks**

Run:

```bash
cargo test --test integration_tests init
nix build path:.#checks.x86_64-linux.home-manager-module
rg -n 'usage|cost|session dashboard|antigravity|claude' README.md docs nix/home-manager.nix
git diff --check
```

Expected: tests/build pass; search results show the explicit exclusion and provider documentation; diff check is silent.

- [x] **Step 4: Run the complete workspace quality gates**

Run:

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
git status --short
```

Expected: all four Cargo commands exit zero; status shows only files belonging to `codexctl-jye` and the accepted research/spec/ADR/plan artifacts.

- [x] **Step 5: Confirm cross-platform CI before release readiness**

After the branch is published by an authorized user, inspect the existing `.github/workflows/ci.yml` Ubuntu and macOS test jobs.

Expected: both platform jobs pass `cargo test --all-targets`. If publishing is outside the active authorization, report CI verification as pending rather than claiming release readiness. Live provider/tmux checks may report unavailable tools, but deterministic fixture tests must still run and pass.

## Dependency Order

```text
Task 1 -> Task 2 -> Task 3
                  -> Task 4
                  -> Task 8
Task 3 -> Task 5 -> Task 6
Task 4 + Task 5 + Task 6 + Task 8 -> Task 7
Task 3 + Task 4 + Task 8 -> Task 9
Task 2 + Task 5 + Task 6 + Task 7 -> Task 10 -> Task 11 -> Task 12
Task 3 + Task 7 + Task 8 + Task 9 -> Task 13
Tasks 1-13 -> Task 14
```

## Spec Coverage

- Core provider/type identity: Tasks 1–3.
- Claude and Antigravity discovery/caching/degradation: Task 4.
- Provider lifecycle, permission, and Antigravity Stop hooks: Tasks 5–7.
- Restored `continue` recovery with prior safety semantics: Task 7.
- Exact-target tmux/terminal fallback and manual text: Task 8.
- Agent Deck, Claude attach, and terminal focus navigation: Task 9.
- Transactional managed configuration: Task 10.
- Init selection and compatibility marker: Task 11.
- Provider doctor matrix: Task 12.
- Brain-only provider labels and manual semantic actions: Task 13.
- Product boundary, capability matrix, no usage/cost, Home Manager, and full gates: Task 14.

## Stress Test Results: Provider-aware Claude Code and Antigravity plan

### Resolved Decisions

- Foundation sequencing: Tasks 1 and 2 remain serial, and Task 2 now requires the full workspace suite before parallel provider work.
- Recovery authority: Stop hooks and TUI polling share a durable canonical recovery-attempt reservation rather than deduplicating on incompatible prompt evidence.
- Terminal security: manual text is single-line, bounded, control-free, and delivered with tmux literal mode; semantic actions use fixed key sequences.
- Process-only boundary: only recognized actionable evidence creates a selectable attention activity; idle processes never become a Live session list.
- Configuration safety: multi-file provider installation uses a crash-recoverable, hash-guarded journal and preserves concurrent user edits.
- Persistence migration: projection maps use injective string storage keys, and native/process aliases come from append-only identity-link evidence.
- Product/privacy boundary: manual text is a transient one-shot Live action and its raw contents are never persisted or logged.
- Verification portability: deterministic fixtures use injected runners, while existing Ubuntu/macOS CI remains the release-readiness platform gate.
- Scale and responsiveness: recovery work is bounded and asynchronous, and append-only identity links compact at the existing activity-store thresholds.
- Journal confidentiality: config transactions reject unsafe/oversized targets and keep crash-recovery contents owner-only and absent from diagnostics.

### Changes Made

- Strengthened the provider-model checkpoint, recovery reservation protocol, terminal literal-input rules, process-only visibility contract, installer transaction protocol, persistence key/link design, manual-input privacy rules, bounded asynchronous recovery, journal confidentiality, and cross-platform verification gate.

### Deferred / Parking Lot

- Removal of legacy token/cost fields remains a separate cleanup.
- A generic process picker, session dashboard, daemon, and arbitrary messaging surface remain intentionally excluded.
- Live provider smoke checks remain diagnostics because provider executables and tmux are optional host dependencies.

### Confidence Assessment

- Overall: High.
- Areas of concern: provider hook schema drift and platform-specific terminal behavior remain controlled by bounded parsers, deterministic fixtures, exact-target abstention, bounded background work, and Ubuntu/macOS CI.
