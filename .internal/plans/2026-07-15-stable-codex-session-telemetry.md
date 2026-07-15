# Stable Codex Session Telemetry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Keep each live Codex process attached to the correct transcript, report only terminal-confirmed shell approvals as actionable `NeedsInput`, and maintain a monotonic per-request estimated cost.

**Architecture:** Discovery performs a global one-to-one process/transcript assignment and lets the TUI retain valid PID attachments. The monitor parses modern Codex lifecycle and request-usage events incrementally; terminal capture supplies separate approval evidence, and the shared approval boundary revalidates that evidence immediately before sending Enter. Cost is accumulated per request with the model profile active at that request instead of recomputed from session totals.

**Tech Stack:** Rust 2024, serde/serde_json JSONL parsing, ratatui, tmux and Kitty command backends, Jujutsu, Beads.

## Global Constraints

- Preserve `~/.codexctl`, `.codexctl.toml`, and `~/.config/codexctl/config.toml`; no persistent-state migration.
- Low CPU, elapsed time, and a pending tool may never authorize input.
- A shell approval action must match process, transcript session, terminal target, call ID, tool, command, and a freshly captured prompt fingerprint.
- Deny rules always override brain approval.
- Unsupported or unreadable terminal capture fails closed: no automatic input.
- Terminal capture is resolved from the target Codex process, never from the dashboard's terminal environment.
- Captured terminal text stays in memory, is bounded to 64 KiB, and is never logged or persisted.
- Unknown modern transcript events are permissive and do not terminate an active task.
- Legacy parsing remains compatible, but legacy shell approval also requires terminal confirmation.
- Cost is an API-equivalent estimate derived only from transcript-exposed usage; missing charge data marks it unverified.
- Table label: `Est. $`. Detail label: `Estimated cost`. Keep machine field `cost_usd`.
- Run targeted tests before `cargo test`, `cargo fmt --check`, and `cargo clippy -- -D warnings`.
- Do not push or sync; the repository uses a conservative Beads/Jujutsu profile.
- Run Beads mutations from `~/.beads-planning` because contributor routing exposes that store read-only from this checkout.
- Start every implementation task in a fresh, correctly described Jujutsu change; do not overwrite one working-copy description across tasks.

## File Map

- `crates/codexctl-core/Cargo.toml`: add RFC 3339 timestamp parsing and bounded child-process waiting.
- `Cargo.lock`: lock the timestamp parser and timeout helper selected by Cargo.
- `crates/codexctl-core/src/discovery.rs`: cache transcript summaries and assign them globally and stably.
- `crates/codexctl-core/src/codex_transcript.rs`: parse modern lifecycle, custom tool, and request-usage events.
- `crates/codexctl-core/src/session.rs`: hold lifecycle, pending-call, approval-evidence, and request-ledger state.
- `crates/codexctl-core/src/monitor.rs`: incremental complete-line parsing, lifecycle status inference, and per-request cost accumulation.
- `crates/codexctl-core/src/models.rs`: GPT-5.6 profiles and optional long-context pricing fields.
- `crates/codexctl-core/src/terminals/mod.rs`: prompt matching, capture abstraction, fail-closed observation, and guarded approval.
- `crates/codexctl-core/src/terminals/tmux.rs`: exact pane lookup, screen capture, and target-bound Enter.
- `crates/codexctl-core/src/terminals/kitty.rs`: PID-targeted screen capture and target-bound Enter.
- `crates/codexctl-core/src/runtime.rs`: pass authoritative `CodexSession` values through the brain driver.
- `crates/codexctl-core/src/rules.rs`: keep rule execution on the guarded approval boundary.
- `crates/codexctl-tui/src/app.rs`: retain transcript attachments, refresh approval observations, and pass monitored sessions to the brain.
- `crates/codexctl-tui/src/ui/table.rs`: render `Est. $` without changing the compatible `Cost` sort key.
- `crates/codexctl-tui/src/ui/detail.rs`: render `Estimated cost`.
- `src/runtime/brain_driver.rs`: stop rediscovering sessions during inference and acceptance.
- `src/config.rs`: parse and document optional long-context override fields.
- `tests/integration_tests.rs`: end-to-end lifecycle, replay, cost, and compatibility regressions.
- `tests/fixtures/`: modern lifecycle and terminal-pane fixtures.

## Execution Tracking

Before Task 1, run Beads commands with `/home/alexander/.beads-planning` as the actual working directory. Create five child tasks under `codexctl-0bq`, store their returned IDs in the execution notes, and add finish-to-start dependencies matching the task order below. Claim and close each child from that same working directory. Do not mutate the routed read-only view from the codexctl checkout, reinitialize either Dolt store, overwrite `.beads/issues.jsonl`, or push/sync Beads state.

---

### Task 1: Stable One-to-One Transcript Assignment

**Files:**

- Modify: `crates/codexctl-core/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/codexctl-core/src/discovery.rs:44-309`
- Modify: `crates/codexctl-tui/src/app.rs:449-620`

**Interfaces:**

- Produces: `pub fn scan_sessions_with_state(state: &mut TranscriptAssignmentState) -> Vec<CodexSession>`.
- Produces: `RetainedTranscript` keyed by PID but bound to process start time, transcript session ID/path/start time, and last observed mtime.
- Produces: `PendingTranscriptTransition` requiring the same newer candidate on two consecutive uncached scans.
- Produces: deterministic constraint-propagation assignment; no process-order greedy fallback.
- Preserves: `pub fn scan_sessions() -> Vec<CodexSession>` as a compatibility wrapper.
- Consumes later: stable `CodexSession.session_id` and `jsonl_path` used by Tasks 2-5.

**Acceptance Criteria:**

- Two new same-directory processes get distinct transcripts by compatible session start time.
- An explicit `resume <session-id>` match wins.
- A retained valid PID/path mapping does not switch when another file becomes newer.
- A reused PID whose process start time changed cannot inherit an old transcript.
- An ambiguous new process remains without a transcript.
- A transcript is assigned to at most one process per scan.
- `/clear` can attach a distinct newer transcript only after the old transcript stops advancing and the same sole candidate survives two uncached scans.
- An unmatched process triggers one uncached summary refresh before it is declared ambiguous.

- [ ] **Step 0: Start a dedicated Jujutsu change**

Run: `jj new -m "🐛 fix: stabilize Codex transcript assignment (codexctl-0bq)"`

Verify: `jj --no-pager log -r '@|@-' --no-graph` shows the new Task 1 description and the reviewed plan as its parent.

- [ ] **Step 1: Add failing pure assignment tests**

Add private test builders and these tests in `discovery.rs`:

```rust
fn transcript(id: &str, cwd: &str, start: u64, path: &str) -> CodexTranscriptSummary {
    CodexTranscriptSummary {
        session_id: id.into(),
        cwd: cwd.into(),
        path: PathBuf::from(path),
        started_at_ms: start,
        mtime_ms: start,
    }
}

fn process(pid: u32, cwd: &str, start: u64, args: &str) -> LiveCodexProcess {
    LiveCodexProcess {
        pid,
        cwd: cwd.into(),
        started_at: start,
        tty: format!("pts/{pid}"),
        cpu_percent: 0.0,
        mem_mb: 32.0,
        command_args: args.into(),
    }
}

#[test]
fn assigns_same_directory_processes_one_to_one_by_start_time() {
    let processes = vec![process(11, "/repo", 100_000, ""), process(12, "/repo", 200_000, "")];
    let transcripts = vec![
        transcript("first", "/repo", 101_000, "/rollout-first.jsonl"),
        transcript("second", "/repo", 201_000, "/rollout-second.jsonl"),
    ];

    let assigned = assign_transcripts(&processes, &transcripts, &HashMap::new());

    assert_eq!(assigned[&11].session_id, "first");
    assert_eq!(assigned[&12].session_id, "second");
    assert_ne!(assigned[&11].path, assigned[&12].path);
}

#[test]
fn retains_valid_attachment_when_mtime_order_changes() {
    let processes = vec![process(11, "/repo", 100_000, "")];
    let transcripts = vec![
        transcript("kept", "/repo", 101_000, "/rollout-kept.jsonl"),
        transcript("newer", "/repo", 102_000, "/rollout-newer.jsonl"),
    ];
    let retained = retained_state(11, 100_000, "kept", "/rollout-kept.jsonl", 101_000);

    let assigned = assign_transcripts(&processes, &transcripts, &retained.retained);

    assert_eq!(assigned[&11].session_id, "kept");
}

#[test]
fn reused_pid_does_not_inherit_retained_transcript() {
    let processes = vec![process(11, "/repo", 300_000, "")];
    let transcripts = vec![transcript("old", "/repo", 101_000, "/old.jsonl")];
    let retained = retained_state(11, 100_000, "old", "/old.jsonl", 101_000);

    assert!(assign_transcripts(&processes, &transcripts, &retained.retained).is_empty());
}

#[test]
fn leaves_equally_close_new_process_unassigned() {
    let processes = vec![process(11, "/repo", 100_000, "")];
    let transcripts = vec![
        transcript("left", "/repo", 99_000, "/left.jsonl"),
        transcript("right", "/repo", 101_000, "/right.jsonl"),
    ];

    assert!(assign_transcripts(&processes, &transcripts, &HashMap::new()).is_empty());
}

#[test]
fn clear_transition_requires_same_unique_candidate_twice() {
    let process = process(11, "/repo", 100_000, "");
    let old = transcript("old", "/repo", 101_000, "/old.jsonl");
    let new = transcript("new", "/repo", 250_000, "/new.jsonl");
    let mut state = retained_state(11, 100_000, "old", "/old.jsonl", 101_000);

    observe_assignment_scan(&[process.clone()], &[old.clone(), new.clone()], &mut state);
    assert_eq!(state.retained[&11].session_id, "old");

    observe_assignment_scan(&[process], &[old, new], &mut state);
    assert_eq!(state.retained[&11].session_id, "new");
}
```

- [ ] **Step 2: Run the discovery tests and confirm failure**

Run: `cargo test -p codexctl-core discovery::tests -- --nocapture`

Expected: compilation fails because the timestamp, retained identity, transition state, and deterministic assignment helpers do not exist.

- [ ] **Step 3: Parse transcript start timestamps**

Add `time = { version = "0.3", features = ["parsing"] }` to `codexctl-core` and add the field/helper:

```rust
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone)]
struct CodexTranscriptSummary {
    session_id: String,
    cwd: String,
    path: PathBuf,
    started_at_ms: u64,
    mtime_ms: u64,
}

fn transcript_started_at_ms(timestamp: Option<&str>) -> Option<u64> {
    let parsed = OffsetDateTime::parse(timestamp?, &Rfc3339).ok()?;
    u64::try_from(parsed.unix_timestamp_nanos() / 1_000_000).ok()
}
```

Populate `started_at_ms` from `session_meta.payload.timestamp`; reject summaries without a valid session identity or start timestamp instead of substituting mtime as identity evidence.

- [ ] **Step 4: Implement identity-bound retention and deterministic assignment**

Add state that distinguishes a continuing process from PID reuse and records a candidate `/clear` transition without immediately switching:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedTranscript {
    pub process_started_at_ms: u64,
    pub session_id: String,
    pub path: PathBuf,
    pub transcript_started_at_ms: u64,
    pub transcript_mtime_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTranscriptTransition {
    pub session_id: String,
    pub path: PathBuf,
    pub consecutive_uncached_scans: u8,
}

#[derive(Debug, Default)]
pub struct TranscriptAssignmentState {
    pub retained: HashMap<u32, RetainedTranscript>,
    pub transitions: HashMap<u32, PendingTranscriptTransition>,
}
```

Use one `used_paths` set for the batch and this fixed order:

1. Reject retained entries when the live process start time differs; this is PID reuse.
2. Assign exact explicit-resume session IDs.
3. Keep valid retained assignments unless a `/clear` transition has matured.
4. Repeatedly assign a sole compatible candidate or a mutual unique-best process/transcript edge, removing each assigned process/path before the next pass.
5. Do not use iteration order to break ties; leave remaining processes unassigned.

Before returning an unmatched process, invalidate the ten-second transcript-summary cache once and rerun the same deterministic pass. Ordinary scans may use the cache; transition confirmation must use uncached scans.

For a retained process, record a possible `/clear` transition only when the retained transcript mtime did not advance, exactly one newer unclaimed transcript has the same cwd and a later `session_meta` start, and no other process has an equally reliable claim. Keep the retained transcript on the first observation. Promote the new session only when the same candidate satisfies those conditions on the next uncached scan; otherwise clear the transition candidate.

- [ ] **Step 5: Wire retained mappings through the TUI and remove independent latest-file fallback**

Keep `TranscriptAssignmentState` in `App` and seed retained entries only from sessions whose PID, process start time, transcript session ID, and path are all present:

```rust
let discovered = discovery::scan_sessions_with_state(&mut self.transcript_assignments);
```

Merge the returned assignments back into the state after each refresh and drop entries for dead PIDs. Keep `scan_sessions()` as a wrapper using default state. In `resolve_jsonl_paths`, retain exact session-ID and explicit-resume resolution, but remove `find_latest_jsonl` and per-session `best_transcript_for_process` fallback for process-backed sessions so an ambiguous session cannot undo the global assignment.

- [ ] **Step 6: Run targeted tests**

Run: `cargo test -p codexctl-core discovery::tests -- --nocapture`

Expected: all discovery tests pass, including cache reuse and distinct same-directory assignment.

Run: `cargo test -p codexctl-tui merge_discovered_session -- --nocapture`

Expected: the existing `/clear` merge test passes.

- [ ] **Step 7: Review the task checkpoint**

Verify: `jj --no-pager st` lists only Task 1 files and `jj --no-pager log -r @ --no-graph` retains the Task 1 description. Do not push; Task 2 creates its own change only after this review passes.

---

### Task 2: Modern Lifecycle and Pending-Call State

**Files:**

- Modify: `crates/codexctl-core/src/codex_transcript.rs:1-265`
- Modify: `crates/codexctl-core/src/session.rs:9-178,286-370`
- Modify: `crates/codexctl-core/src/monitor.rs:279-598`
- Create: `tests/fixtures/codex-modern-lifecycle.jsonl`
- Modify: `tests/integration_tests.rs:1-216,430-555`

**Interfaces:**

- Produces: `CodexLifecycleEvent::{TaskStarted, TaskComplete, TurnAborted, UserMessage, AgentMessage, Other}`.
- Produces: `CodexTaskState::{Unknown, Processing, WaitingInput, Aborted}` in `CodexSession.task_state`.
- Produces: `CodexSession.pending_tool_call_id: Option<String>` and `explicit_input_required: bool`.
- Produces: `pub fn refresh_status(session: &mut CodexSession)` for Task 3 after terminal observation.

**Acceptance Criteria:**

- `task_started`, user messages, reasoning, agent messages, tool calls, and tool outputs keep an active task `Processing`.
- `task_complete` becomes recent `WaitingInput`; `turn_aborted` ends processing without creating `NeedsInput`.
- `function_call`/output and `custom_tool_call`/output close only when `call_id` matches.
- `request_user_input` becomes explicit `NeedsInput` until its matching output or continued task activity.
- A pending shell call with low CPU remains `Processing` until Task 3 supplies approval evidence.
- Unknown events do not terminate an active task.

- [ ] **Step 0: Start a dedicated Jujutsu change**

Run: `jj new -m "🐛 fix: track Codex task lifecycle explicitly (codexctl-0bq)"`

Verify: Task 1 is reviewed and `jj --no-pager log -r '@|@-' --no-graph` shows a new Task 2 change.

- [ ] **Step 1: Add failing parser tests for lifecycle and custom calls**

Add tests in `codex_transcript.rs` using exact JSONL shapes:

```rust
#[test]
fn parses_task_lifecycle_events() {
    let started = r#"{"type":"event_msg","payload":{"type":"task_started"}}"#;
    let complete = r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#;
    assert_eq!(parse_line(started), Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted)));
    assert_eq!(parse_line(complete), Some(CodexEvent::Lifecycle(CodexLifecycleEvent::TaskComplete)));
}

#[test]
fn parses_custom_tool_call_and_output() {
    let call = r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"shell","input":"cargo test","call_id":"call-7"}}"#;
    let output = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call-7","output":"ok"}}"#;
    let Some(CodexEvent::ResponseItem(call)) = parse_line(call) else { panic!("custom call") };
    let Some(CodexEvent::ResponseItem(output)) = parse_line(output) else { panic!("custom output") };
    assert_eq!(call.kind, CodexResponseKind::CustomToolCall);
    assert_eq!(output.kind, CodexResponseKind::CustomToolCallOutput);
}
```

- [ ] **Step 2: Add failing monitor regressions**

Replace tests that expect low-CPU `tool_use` to imply `NeedsInput` and add:

```rust
#[test]
fn pending_shell_with_low_cpu_is_processing_without_approval_evidence() {
    let mut session = make_session(0.1, 30);
    session.task_state = CodexTaskState::Processing;
    session.pending_tool_name = Some("exec_command".into());
    session.pending_tool_call_id = Some("call-7".into());
    monitor::refresh_status(&mut session);
    assert_eq!(session.status, SessionStatus::Processing);
}

#[test]
fn mismatched_tool_output_does_not_close_pending_call() {
    let (mut session, _file) = make_session_with_jsonl(include_str!("fixtures/codex-modern-lifecycle.jsonl"));
    monitor::update_tokens(&mut session);
    assert_eq!(session.pending_tool_call_id.as_deref(), Some("call-live"));
    assert_eq!(session.status, SessionStatus::Processing);
}
```

- [ ] **Step 3: Run the parser and status tests and confirm failure**

Run: `cargo test -p codexctl-core codex_transcript::tests -- --nocapture`

Run: `cargo test --test integration_tests pending_shell_with_low_cpu_is_processing_without_approval_evidence -- --nocapture`

Run: `cargo test --test integration_tests mismatched_tool_output_does_not_close_pending_call -- --nocapture`

Expected: compilation fails on the new lifecycle and session types.

- [ ] **Step 4: Parse structured lifecycle events and custom tool variants**

Replace free-form `EventMessage(String)` with structured lifecycle data:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLifecycleEvent {
    TaskStarted,
    TaskComplete,
    TurnAborted,
    UserMessage,
    AgentMessage,
    Other(String),
}

pub enum CodexEvent {
    SessionMeta(CodexSessionMeta),
    TurnContext(CodexTurnContext),
    TokenCount(CodexTokenCount),
    Lifecycle(CodexLifecycleEvent),
    ResponseItem(CodexResponseItem),
}
```

In `parse_event_msg`, match `payload.type` directly; keep `token_count` special and map every unknown type to `Other(type_name.to_string())`. Extend `CodexResponseKind` with `CustomToolCall` and `CustomToolCallOutput`, reading custom input from `payload.input` into `arguments`.

- [ ] **Step 5: Add explicit lifecycle state to `CodexSession`**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexTaskState {
    #[default]
    Unknown,
    Processing,
    WaitingInput,
    Aborted,
}
```

Add and initialize:

```rust
pub task_state: CodexTaskState,
pub explicit_input_required: bool,
pub pending_tool_call_id: Option<String>,
```

Keep `last_msg_type`, `last_stop_reason`, and `is_waiting_for_task` for legacy compatibility.

- [ ] **Step 6: Apply lifecycle events and pair calls by ID**

In the modern monitor branch:

```rust
fn apply_lifecycle(event: CodexLifecycleEvent, session: &mut CodexSession) {
    match event {
        CodexLifecycleEvent::TaskStarted | CodexLifecycleEvent::UserMessage => {
            session.task_state = CodexTaskState::Processing;
            session.explicit_input_required = false;
        }
        CodexLifecycleEvent::TaskComplete => {
            session.task_state = CodexTaskState::WaitingInput;
            session.explicit_input_required = false;
        }
        CodexLifecycleEvent::TurnAborted => {
            session.task_state = CodexTaskState::Aborted;
            session.explicit_input_required = false;
        }
        CodexLifecycleEvent::AgentMessage | CodexLifecycleEvent::Other(_) => {}
    }
}
```

For function/custom calls, store `call_id`, tool, and command. Set `explicit_input_required` when the tool name is `request_user_input`. For outputs, clear the pending fields only when `item.call_id == session.pending_tool_call_id`; do not let an unrelated output close the active call.

- [ ] **Step 7: Make status lifecycle-first and CPU authorization-free**

Add `refresh_status` and order the status checks as follows:

```rust
pub fn refresh_status(session: &mut CodexSession) {
    let last_type = session.last_msg_type.clone();
    let stop_reason = session.last_stop_reason.clone();
    infer_status(session, &last_type, &stop_reason, session.is_waiting_for_task);
}

// At the start of infer_status, before CPU checks:
if session.explicit_input_required {
    session.status = SessionStatus::NeedsInput;
    return;
}
match session.task_state {
    CodexTaskState::Processing => {
        session.status = SessionStatus::Processing;
        return;
    }
    CodexTaskState::WaitingInput | CodexTaskState::Aborted => {
        session.status = recent_waiting_or_idle(session.last_message_ts);
        return;
    }
    CodexTaskState::Unknown => {}
}
```

Retain high CPU as evidence for `Processing` only after explicit lifecycle state. Delete the legacy branch that maps low CPU plus pending tool or age to `NeedsInput`; pending tools fall through to `Processing`.

- [ ] **Step 8: Run targeted lifecycle tests**

Run: `cargo test -p codexctl-core codex_transcript::tests -- --nocapture`

Run: `cargo test --test integration_tests status_ -- --nocapture`

Run: `cargo test --test integration_tests process_backed_codex_monitor -- --nocapture`

Expected: modern lifecycle tests pass; updated legacy status tests pass without CPU-based shell approval.

- [ ] **Step 9: Review the task checkpoint**

Verify: `jj --no-pager diff --git` contains only Task 2 behavior and tests and `jj --no-pager log -r @ --no-graph` retains the Task 2 description. Do not push; Task 3 creates its own change only after this review passes.

---

### Task 3: Terminal-Confirmed and Revalidated Approval

**Files:**

- Modify: `crates/codexctl-core/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/codexctl-core/src/session.rs:90-178,286-370`
- Modify: `crates/codexctl-core/src/terminals/mod.rs:1-26,239-260,994-1093,1109-1261`
- Modify: `crates/codexctl-core/src/terminals/tmux.rs:24-81`
- Modify: `crates/codexctl-core/src/terminals/kitty.rs:21-91`
- Modify: `crates/codexctl-core/src/monitor.rs:463-598`
- Modify: `crates/codexctl-core/src/runtime.rs:35-64,380-430`
- Modify: `crates/codexctl-tui/src/app.rs:418-473,573-640,1311-1475,2240-2320`
- Modify: `src/runtime/brain_driver.rs:1-110`
- Create: `tests/fixtures/codex-shell-approval-pane.txt`
- Create: `tests/fixtures/codex-running-shell-pane.txt`
- Create: `tests/fixtures/codex-approval-lookalike-pane.txt`

**Interfaces:**

- Produces: `ApprovalEvidence`, `ApprovalObservation`, and backend-bound `PaneCapture` in `terminals`/`session`.
- Produces: `pub fn refresh_approval_observation(session: &mut CodexSession)`.
- Strengthens: `pub fn approve_shell_permission(session: &CodexSession) -> Result<(), String>` as the only recapture-and-compare path to Enter.
- Changes: `BrainDriver::tick` and `cleanup` consume `&[CodexSession]`; `accept` consumes `&CodexSession`.

**Acceptance Criteria:**

- A matching tmux or Kitty Codex approval pane promotes a pending shell call to `NeedsInput`.
- A running tool, unsupported terminal, capture failure, broad “permission” text, or command mismatch does not become actionable.
- Approval evidence binds session, TTY, target backend/identity, call ID, tool, command, prompt pattern version, and fingerprint.
- Every manual, legacy-auto, rule, and brain approval recaptures before sending Enter.
- A changed/disappeared prompt or call cancels the action and sends zero bytes.
- Brain inference and acceptance use the authoritative monitored session object, not rediscovery.
- Deny rules take precedence over every allow source.
- `request_user_input` never enters the shell-approval path and never receives a blind Enter.
- Capture commands finish within 500 ms and return at most 64 KiB from the last 80 terminal lines.
- Captured pane text is never logged, serialized, or persisted.

- [ ] **Step 0: Start a dedicated Jujutsu change**

Run: `jj new -m "🔒 fix: revalidate Codex approvals before input (codexctl-0bq)"`

Verify: Task 2 is reviewed and `jj --no-pager log -r '@|@-' --no-graph` shows a new Task 3 change.

- [ ] **Step 1: Add failing pure prompt-matcher and stale-action tests**

Add a private `FakeApprovalIo` test implementation that returns queued captures and counts sends:

```rust
#[derive(Default)]
struct FakeApprovalIo {
    captures: std::sync::Mutex<VecDeque<Result<PaneCapture, String>>>,
    sends: AtomicUsize,
}

impl FakeApprovalIo {
    fn with_captures(captures: impl IntoIterator<Item = Result<PaneCapture, String>>) -> Self {
        Self {
            captures: std::sync::Mutex::new(captures.into_iter().collect()),
            sends: AtomicUsize::new(0),
        }
    }
}

fn capture(text: &str) -> PaneCapture {
    PaneCapture {
        backend: Terminal::Tmux,
        target: "test-pane".into(),
        text: text.into(),
    }
}

fn pending_shell_session(call_id: &str, command: &str) -> CodexSession {
    let mut session = CodexSession::from_raw(RawSession {
        pid: 7,
        session_id: "session-7".into(),
        cwd: "/repo".into(),
        started_at: 0,
    });
    session.tty = "pts/7".into();
    session.pending_tool_name = Some("exec_command".into());
    session.pending_tool_call_id = Some(call_id.into());
    session.pending_tool_input = Some(command.into());
    session
}

impl ApprovalIo for FakeApprovalIo {
    fn capture(&self, _session: &CodexSession) -> Result<PaneCapture, String> {
        self.captures.lock().unwrap().pop_front().unwrap()
    }

    fn send_enter(
        &self,
        _session: &CodexSession,
        _backend: Terminal,
        _target: &str,
    ) -> Result<(), String> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
```

Test the approval and running fixtures, command mismatch, and stale recapture:

```rust
#[test]
fn stale_prompt_never_sends_enter() {
    let mut session = pending_shell_session("call-7", "cargo test");
    let io = FakeApprovalIo::with_captures([
        Ok(capture(include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt"))),
        Ok(capture(include_str!("../../../../tests/fixtures/codex-running-shell-pane.txt"))),
    ]);
    refresh_approval_observation_with(&io, &mut session, 10_000);
    assert!(matches!(session.approval, ApprovalObservation::Confirmed(_)));

    let error = approve_shell_permission_with(&io, &session).unwrap_err();

    assert!(error.contains("approval prompt changed"));
    assert_eq!(io.sends.load(Ordering::SeqCst), 0);
}
```

- [ ] **Step 2: Run the terminal tests and confirm failure**

Run: `cargo test -p codexctl-core terminals::tests -- --nocapture`

Expected: compilation fails because the approval types and IO boundary do not exist.

- [ ] **Step 3: Add identity-bound approval state**

Add these types and session fields:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCapture {
    pub backend: Terminal,
    pub target: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalEvidence {
    pub session_id: String,
    pub tty: String,
    pub call_id: String,
    pub tool: String,
    pub command: String,
    pub backend: Terminal,
    pub target: String,
    pub prompt_pattern_version: u16,
    pub prompt_fingerprint: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ApprovalObservation {
    #[default]
    NotChecked,
    Confirmed(ApprovalEvidence),
    Unknown(String),
}
```

Add `approval: ApprovalObservation` and `approval_checked_at_ms: u64` to `CodexSession`. Clear both whenever the pending call ID changes or matching output closes it.

- [ ] **Step 4: Implement bounded, target-derived tmux and Kitty capture**

Add `wait-timeout = "0.2"` to `codexctl-core`. Implement one private command runner that spawns with piped output, waits at most 500 ms, kills and reaps on timeout, rejects output beyond 64 KiB, and never includes captured stdout in an error or log. Unit-test success, timeout, non-zero exit, and oversized output with fake commands.

Do not use `detect_terminal()`, `TERM`, `KITTY_WINDOW_ID`, or the dashboard process environment to choose the session backend. Resolve the target from the Codex process itself: exact TTY membership for tmux and exact PID matching for Kitty. Zero or multiple matching backends is unsupported and fails closed.

Refactor tmux pane lookup into one helper used by capture and send:

```rust
fn pane_target(session: &CodexSession) -> Result<String, String> {
    let output = Command::new("tmux")
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_tty}\t#{session_name}:#{window_index}.#{pane_index}",
        ])
        .output()
        .map_err(|error| format!("tmux list-panes failed: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let wanted = session.tty.trim_start_matches("/dev/");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .find(|(tty, _)| tty.trim_start_matches("/dev/") == wanted)
        .map(|(_, target)| target.to_string())
        .ok_or_else(|| format!("TTY {} not found in tmux panes", session.tty))
}

fn checked_capture(
    backend: Terminal,
    target: String,
    output: BoundedOutput,
) -> Result<PaneCapture, String> {
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(PaneCapture {
        backend,
        target,
        text: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

pub fn capture(session: &CodexSession) -> Result<PaneCapture, String> {
    let target = pane_target(session)?;
    let output = run_bounded(
        Command::new("tmux")
            .args(["capture-pane", "-p", "-S", "-80", "-t", &target]),
    )?;
    checked_capture(Terminal::Tmux, target, output)
}
```

For Kitty, use the installed CLI-supported command:

```rust
pub fn capture(session: &CodexSession) -> Result<PaneCapture, String> {
    let target = format!("pid:{}", session.pid);
    let output = run_bounded(
        Command::new("kitty")
            .args(["@", "get-text", "--match", &target, "--extent", "screen"]),
    )?;
    checked_capture(Terminal::Kitty, target, output)
}
```

Both send paths must reuse the evidence backend and target, not perform environment detection or a cwd fallback. Cap Kitty's returned screen to the final 80 logical lines after the byte limit.

- [ ] **Step 5: Implement a versioned, fail-closed Codex approval matcher**

Sanitize real panes from the installed supported Codex versions into the positive fixture. Define a small table of exact structural patterns, each with a stable version number, approval question, and numbered choice anchors. Add the lookalike fixture for generic permission prose, partial commands, reordered/missing choices, and ordinary running output. A future wording change adds a fixture and pattern version; it never broadens matching to generic terms.

Normalize whitespace, require one known complete pattern and the complete normalized pending command:

```rust
fn match_approval_prompt(capture: &PaneCapture, session: &CodexSession) -> Option<ApprovalEvidence> {
    let call_id = session.pending_tool_call_id.as_deref()?;
    let tool = session.pending_tool_name.as_deref()?;
    let command = session.pending_tool_input.as_deref()?;
    let pane = normalize_whitespace(&capture.text).to_ascii_lowercase();
    let command_normalized = normalize_whitespace(command).to_ascii_lowercase();
    let pattern = APPROVAL_PROMPT_PATTERNS.iter().find(|pattern| {
        pane.contains(pattern.question)
            && pattern.choice_anchors.iter().all(|anchor| pane.contains(anchor))
    })?;
    if !pane.contains(&command_normalized) { return None; }

    Some(ApprovalEvidence {
        session_id: session.session_id.clone(),
        tty: session.tty.clone(),
        call_id: call_id.into(),
        tool: tool.into(),
        command: command.into(),
        backend: capture.backend.clone(),
        target: capture.target.clone(),
        prompt_pattern_version: pattern.version,
        prompt_fingerprint: fingerprint(&pane),
    })
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fingerprint(value: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
```

The fingerprint covers only the normalized approval block, not unrelated scrollback. Tests must prove the lookalike fixture returns `None` and never sends input.

- [ ] **Step 6: Observe only unresolved candidates with bounded retry**

Implement private `ApprovalIo` plus real/fake implementations:

```rust
trait ApprovalIo {
    fn capture(&self, session: &CodexSession) -> Result<PaneCapture, String>;
    fn send_enter(
        &self,
        session: &CodexSession,
        backend: Terminal,
        target: &str,
    ) -> Result<(), String>;
}

struct RealApprovalIo;

impl ApprovalIo for RealApprovalIo {
    fn capture(&self, session: &CodexSession) -> Result<PaneCapture, String> {
        capture_session(session)
    }

    fn send_enter(
        &self,
        session: &CodexSession,
        backend: Terminal,
        target: &str,
    ) -> Result<(), String> {
        send_enter_to_target(session, backend, target)
    }
}
```

`refresh_approval_observation_with` must:

- clear observation when there is no pending shell call;
- run at most once per pending candidate during each ordinary application refresh;
- set `Confirmed` only on exact match;
- replace any prior confirmation with `Unknown(reason)` on unsupported/ambiguous backend, timeout/command failure, oversized capture, or non-matching pane;
- never send input.

Call it in `App::refresh` after `monitor::update_tokens`, then call `monitor::refresh_status`. Update inference so `Confirmed` precedes active-task `Processing`; `Unknown` does not.

- [ ] **Step 7: Guard the single shell-approval boundary**

Implement approval as observe-compare-send:

```rust
fn approve_shell_permission_with(
    io: &impl ApprovalIo,
    session: &CodexSession,
) -> Result<(), String> {
    let ApprovalObservation::Confirmed(expected) = &session.approval else {
        return Err("approval is not terminal-confirmed".into());
    };
    let capture = io.capture(session)?;
    let current = match_approval_prompt(&capture, session)
        .ok_or_else(|| "approval prompt changed or disappeared".to_string())?;
    if &current != expected {
        return Err("approval identity changed; action cancelled".into());
    }
    io.send_enter(session, expected.backend.clone(), &expected.target)
}
```

Keep `rules::execute`, manual shell approval, per-PID auto-approve, and brain auto-mode calling `terminals::approve_shell_permission`; remove or privatize any alternate direct Enter path. Require a still-pending shell call, fresh matching evidence, and an allow decision at this boundary. Evaluate deny rules last and make any matching deny veto every allow source. Route `request_user_input` through its explicit text-response path only; it must never call this function.

- [ ] **Step 8: Pass authoritative sessions through `BrainDriver`**

Change the trait surface:

```rust
fn tick(&mut self, sessions: &[CodexSession], deny_rules: &[AutoRule]) -> Vec<(u32, String)>;
fn cleanup(&mut self, sessions: &[CodexSession]);
fn accept(&mut self, session: &CodexSession) -> Option<String>;
```

Delete `LiveBrainDriver::resolve_live` and all discovery calls from `brain_driver.rs`. In `App`, pass `&self.sessions` to tick/cleanup and the currently cloned monitored session to accept. Retain `SessionSnapshot` only for mailbox/session-source DTOs.

- [ ] **Step 9: Verify rules and stale decisions**

Run: `cargo test -p codexctl-core terminals::tests -- --nocapture`

Run: `cargo test -p codexctl-core rules::tests -- --nocapture`

Run: `cargo test -p codexctl-tui app::tests -- --nocapture`

Run: `cargo test runtime::brain_driver::tests -- --nocapture`

Expected: positive and lookalike matcher tests pass; wrong backend/target, stale evidence, timeout, and changed prompt send zero input; deny precedence passes; `request_user_input` never calls Enter; and the brain driver performs no discovery.

- [ ] **Step 10: Review the task checkpoint**

Verify: `jj --no-pager diff --git` contains the shared guarded boundary and no bypass, and `jj --no-pager log -r @ --no-graph` retains the Task 3 description. Do not push; Task 4 creates its own change only after this review passes.

---

### Task 4: Monotonic Per-Request Cost Ledger and GPT-5.6 Profiles

**Files:**

- Modify: `crates/codexctl-core/src/models.rs:1-216`
- Modify: `crates/codexctl-core/src/session.rs:90-223,286-370`
- Modify: `crates/codexctl-core/src/monitor.rs:1-31,279-510,660-775`
- Modify: `src/config.rs:372-402,480-515,681-705,1077-1102,1160-1200`
- Modify: `tests/integration_tests.rs:217-330,430-555,640-810`

**Interfaces:**

- Extends: `ModelProfile` with optional long-context threshold and multipliers.
- Produces: `CodexSession.own_cost_usd`, `priced_total_tokens`, and `cost_ledger_frozen`.
- Produces: `fn price_request(model: &str, usage: &CodexTokenUsage, cache_write_tokens: Option<u64>) -> (f64, bool)`.
- Preserves: `cost_usd`, `usage_metrics_available`, `cost_estimate_unverified`, and model overrides.

**Acceptance Criteria:**

- Each advancing `token_count.total_token_usage.total_tokens` prices `last_token_usage` once.
- Repeated refresh, replay, and in-place truncation do not double-charge or lower cost.
- A cumulative counter reset freezes the last cost and marks it unverified until a genuine transcript transition.
- Mixed-model turns retain each request's original price.
- Parent and subagent ledgers sum without recomputing historical cost at the latest model.
- GPT-5.6 Sol/Terra/Luna use documented context, token prices, cache-write rate, and >272K multipliers.
- Missing cache-write usage is not invented and marks modern estimates unverified.
- Unknown models use the existing fallback profile, preserve monotonicity, and mark the estimate unverified.
- Existing overrides without long-context fields keep multiplier 1.0 and no threshold.
- An incomplete trailing JSONL line is retried after its newline arrives.
- The >272K multiplier is evaluated from each request's input, never from cumulative session tokens.

- [ ] **Step 0: Start a dedicated Jujutsu change**

Run: `jj new -m "🐛 fix: accumulate Codex request cost monotonically (codexctl-0bq)"`

Verify: Task 3 is reviewed and `jj --no-pager log -r '@|@-' --no-graph` shows a new Task 4 change.

- [ ] **Step 1: Add failing model-profile tests**

Add exact assertions in `models.rs`:

```rust
#[test]
fn resolves_gpt_56_family_profiles() {
    let sol = resolve_with_overrides("gpt-5.6", &HashMap::new()).profile;
    assert_eq!(sol.input_per_m, 5.0);
    assert_eq!(sol.cache_read_per_m, 0.5);
    assert_eq!(sol.output_per_m, 30.0);
    assert_eq!(sol.cache_write_per_m, 6.25);
    assert_eq!(sol.context_max, 1_050_000);
    assert_eq!(sol.long_context_threshold, Some(272_000));

    let terra = resolve_with_overrides("gpt-5.6-terra", &HashMap::new()).profile;
    assert_eq!((terra.input_per_m, terra.cache_read_per_m, terra.output_per_m), (2.5, 0.25, 15.0));

    let luna = resolve_with_overrides("gpt-5.6-luna", &HashMap::new()).profile;
    assert_eq!((luna.input_per_m, luna.cache_read_per_m, luna.output_per_m), (1.0, 0.1, 6.0));
}
```

Pricing sources: [GPT-5.6 Sol](https://developers.openai.com/api/docs/models/gpt-5.6-sol), [GPT-5.6 Terra](https://developers.openai.com/api/docs/models/gpt-5.6-terra), and [GPT-5.6 Luna](https://developers.openai.com/api/docs/models/gpt-5.6-luna).

- [ ] **Step 2: Add failing request-ledger regressions**

Add integration tests that append JSONL in stages:

```rust
#[test]
fn codex_cost_prices_each_request_once_and_never_decreases() {
    let (mut session, mut file) = codex_session_file("gpt-5.6-sol");
    append_token_count(&mut file, 100_000, 80_000, 20_000, 1_000, 100_000);
    monitor::update_tokens(&mut session);
    let first = session.cost_usd;

    monitor::update_tokens(&mut session);
    assert_eq!(session.cost_usd, first);

    append_token_count(&mut file, 250_000, 120_000, 40_000, 2_000, 150_000);
    monitor::update_tokens(&mut session);
    assert!(session.cost_usd > first);
}

#[test]
fn counter_reset_freezes_cost_as_unverified() {
    let (mut session, mut file) = codex_session_file("gpt-5.6-sol");
    append_token_count(&mut file, 250_000, 120_000, 40_000, 2_000, 250_000);
    monitor::update_tokens(&mut session);
    let before = session.cost_usd;
    truncate_and_write_token_count(&mut file, 10_000, 8_000, 2_000, 100, 10_000);
    monitor::update_tokens(&mut session);
    assert_eq!(session.cost_usd, before);
    assert!(session.cost_estimate_unverified);
}
```

Add separate tests for mixed turn-context models, long-context multiplier, duplicate replay, and a partial line completed on the next refresh.
Assert after every staged refresh that `cost_usd >= previous_cost_usd`; replaying the same cumulative watermark must leave it exactly equal.

- [ ] **Step 3: Run model and ledger tests and confirm failure**

Run: `cargo test -p codexctl-core models::tests -- --nocapture`

Run: `cargo test --test integration_tests codex_cost_ -- --nocapture`

Run: `cargo test --test integration_tests counter_reset_ -- --nocapture`

Run: `cargo test --test integration_tests mixed_model_ -- --nocapture`

Run: `cargo test --test integration_tests partial_jsonl_ -- --nocapture`

Expected: new profile fields and ledger state are missing; existing cumulative recomputation fails monotonic/mixed-model assertions.

- [ ] **Step 4: Extend model profiles compatibly**

Add fields:

```rust
pub struct ModelProfile {
    pub input_per_m: f64,
    pub output_per_m: f64,
    pub cache_read_per_m: f64,
    pub cache_write_per_m: f64,
    pub context_max: u64,
    pub long_context_threshold: Option<u64>,
    pub long_context_input_multiplier: f64,
    pub long_context_output_multiplier: f64,
}
```

Add `gpt-5.6`/`gpt-5.6-sol`, Terra, and Luna profiles. Use cache-write price `input_per_m * 1.25`, threshold `272_000`, input multiplier `2.0`, and output multiplier `1.5`. Existing profiles and fallback use `None`, `1.0`, `1.0` so behavior does not change.

Update `shorten_model` so the specific Terra/Luna/Sol names are checked before the `gpt-5.6` alias.

- [ ] **Step 5: Add monitor-held ledger state**

Add and initialize:

```rust
pub own_cost_usd: f64,
pub priced_total_tokens: u64,
pub cost_ledger_frozen: bool,
```

For subagent rollups, add `cost_ledger_frozen: bool`; if their file truncates, preserve totals/cost, mark unverified, and stop pricing that path instead of resetting the rollup.

- [ ] **Step 6: Price modern requests on watermark advance**

Apply each token event before overwriting display totals. The model must already reflect the active `turn_context` for that request; record or price the delta immediately so a later model change cannot reprice it:

```rust
fn apply_token_count(count: CodexTokenCount, session: &mut CodexSession) {
    let watermark = count.total.total_tokens;
    if watermark < session.priced_total_tokens {
        session.cost_ledger_frozen = true;
        session.cost_estimate_unverified = true;
    } else if watermark > session.priced_total_tokens && !session.cost_ledger_frozen {
        let (cost, unverified) = price_request(&session.model, &count.last, None);
        session.own_cost_usd += cost;
        session.cost_estimate_unverified |= unverified;
        session.priced_total_tokens = watermark;
    }

    if watermark >= session.priced_total_tokens {
        session.own_input_tokens = count.total.input_tokens;
        session.own_output_tokens = count.total.output_tokens;
        session.own_cache_read_tokens = count.total.cached_input_tokens;
        session.context_tokens = count.last.input_tokens;
    }
}
```

`price_request` subtracts cached/cache-write tokens from uncached input, applies the input multiplier to all input categories and the output multiplier only when this request's input exceeds the configured threshold, and returns `unverified = true` for fallback profiles or missing cache-write data. Equal watermarks are replay and add nothing; lower watermarks freeze the ledger until Task 1 confirms a genuine transcript transition.

Implement it directly:

```rust
fn price_request(
    model: &str,
    usage: &CodexTokenUsage,
    cache_write_tokens: Option<u64>,
) -> (f64, bool) {
    let resolved = models::resolve(model);
    let cache_write = cache_write_tokens.unwrap_or(0);
    let plain_input = usage
        .input_tokens
        .saturating_sub(usage.cached_input_tokens)
        .saturating_sub(cache_write);
    let long = resolved
        .profile
        .long_context_threshold
        .is_some_and(|threshold| usage.input_tokens > threshold);
    let input_multiplier = if long {
        resolved.profile.long_context_input_multiplier
    } else {
        1.0
    };
    let output_multiplier = if long {
        resolved.profile.long_context_output_multiplier
    } else {
        1.0
    };
    let cost = input_multiplier
        * ((plain_input as f64 / 1_000_000.0) * resolved.profile.input_per_m
            + (usage.cached_input_tokens as f64 / 1_000_000.0)
                * resolved.profile.cache_read_per_m
            + (cache_write as f64 / 1_000_000.0) * resolved.profile.cache_write_per_m)
        + output_multiplier
            * (usage.output_tokens as f64 / 1_000_000.0)
            * resolved.profile.output_per_m;
    let unverified = resolved.source == ModelProfileSource::Fallback
        || cache_write_tokens.is_none();
    (cost, unverified)
}
```

In `finalize_usage`, set `session.cost_usd = session.own_cost_usd + subagent_rollup.cost_usd`; never recalculate the parent's historical cost from cumulative token totals.

- [ ] **Step 7: Price legacy messages at their active model**

Move legacy `message.model` assignment before usage pricing. Add each complete message's request cost immediately to `own_cost_usd` using `Some(cache_creation_input_tokens)`. Preserve legacy parser behavior otherwise. On legacy in-place counterless truncation, freeze cost/totals as unverified rather than resetting to zero.

- [ ] **Step 8: Advance offsets only through complete lines**

Add a shared reader helper that returns complete lines and the offset immediately after the final newline:

```rust
fn read_complete_lines(file: &mut File, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let Some(last_newline) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok((Vec::new(), offset));
    };
    let complete = &bytes[..=last_newline];
    let lines = String::from_utf8_lossy(complete).lines().map(str::to_owned).collect();
    Ok((lines, offset + complete.len() as u64))
}
```

Use it in modern parent, legacy parent, and subagent reads. Do not set offsets to `file_len` while an incomplete tail remains.

- [ ] **Step 9: Parse optional override fields**

Extend the dynamic `[models."..."]` parser and template with:

```toml
# long_context_threshold = 272000
# long_context_input_multiplier = 2.0
# long_context_output_multiplier = 1.5
```

Initialize omitted overrides to `None`, `1.0`, `1.0`; add config parsing assertions for both omitted and explicit cases. No CLI flag is added.

- [ ] **Step 10: Run targeted cost/config tests**

Run: `cargo test -p codexctl-core models::tests -- --nocapture`

Run: `cargo test --test integration_tests cost_ -- --nocapture`

Run: `cargo test --test integration_tests codex_ -- --nocapture`

Run: `cargo test config::tests -- --nocapture`

Expected: all request-ledger, truncation, mixed-model, GPT-5.6, and override tests pass; every monotonic assertion holds.

- [ ] **Step 11: Review the task checkpoint**

Verify: `jj --no-pager diff --git` contains Task 4 files and no unrelated model changes, and `jj --no-pager log -r @ --no-graph` retains the Task 4 description. Do not push; Task 5 creates its own change only after this review passes.

---

### Task 5: Compact Labels, Compatibility, and Workspace Verification

**Files:**

- Modify: `crates/codexctl-tui/src/ui/table.rs:135-175`
- Modify: `crates/codexctl-tui/src/ui/detail.rs:70-100`
- Modify: `crates/codexctl-core/src/session.rs:690-780,887-1075`
- Modify: `tests/integration_tests.rs`
- Modify: `crates/codexctl-tui/src/app.rs:2543-2763`

**Interfaces:**

- Preserves: `SORT_COLUMNS` entry `Cost` and JSON `cost_usd`.
- Produces UI copy only: table `Est. $`; detail `Estimated cost`.
- Verifies all interfaces from Tasks 1-4 together.

**Acceptance Criteria:**

- The compact table says `Est. $`; the detail panel says `Estimated cost`.
- Existing `Cost` sort/config behavior and `cost_usd` JSON output remain compatible.
- JSON exposes existing `estimate.verified` and `estimate.profile_source` metadata.
- Same-directory session refreshes never lower cost because of attachment switching.
- A stale brain approval cannot inject Enter.
- All targeted and workspace quality gates pass.

- [ ] **Step 0: Start a dedicated Jujutsu change**

Run: `jj new -m "🐛 fix: stabilize Codex status and cost telemetry (codexctl-0bq)"`

Verify: Task 4 is reviewed and `jj --no-pager log -r '@|@-' --no-graph` shows a new Task 5 change.

- [ ] **Step 1: Add failing UI-copy and JSON compatibility tests**

Promote table headings and the detail cost title to private constants used by render code and tests:

```rust
#[test]
fn cost_labels_are_compact_and_explicit() {
    assert_eq!(TABLE_HEADERS[4], "Est. $");
    assert_eq!(DETAIL_COST_TITLE, " Estimated cost");
    assert_eq!(crate::app::SORT_COLUMNS[2], "Cost");
}

#[test]
fn session_json_keeps_cost_field_and_estimate_metadata() {
    let value = session_with_cost(1.25).to_json_value();
    assert_eq!(value["cost_usd"], 1.25);
    assert!(value["estimate"]["verified"].is_boolean());
    assert!(value["estimate"]["profile_source"].is_string());
}
```

- [ ] **Step 2: Run the UI/session tests and confirm failure**

Run: `cargo test -p codexctl-tui cost_labels_are_compact_and_explicit -- --nocapture`

Run: `cargo test -p codexctl-core session_json_keeps_cost_field_and_estimate_metadata -- --nocapture`

Expected: UI constants/labels are not yet present; JSON compatibility test should pass once its fixture sets usage availability.

- [ ] **Step 3: Change only the rendered labels**

Use:

```rust
const TABLE_HEADERS: [&str; 11] = [
    "PID", "Project", "Status", "Context", "Est. $", "$/hr", "Elapsed", "CPU%", "MEM", "In/Out", "Activity",
];
```

Do not alter adjacent copy. In detail rendering, replace the `" Cost"` section title with `" Estimated cost"`. Do not rename `SORT_COLUMNS`, format placeholders, config values, or `cost_usd`.

- [ ] **Step 4: Add the cross-feature regressions**

Add one integration fixture/test for each high-risk sequence:

- two same-directory sessions alternate writes for three refreshes and retain distinct paths/costs;
- a PID is reused and cannot inherit the previous process's transcript;
- `/clear` exposes a new transcript but attachment changes only after the same unique candidate is seen on two uncached scans;
- a task starts, opens a shell call, runs with low CPU, then completes without ever showing `NeedsInput` when capture is non-matching;
- a matching approval becomes `NeedsInput`, brain decision is delayed, pane changes, and guarded approval records zero sends;
- a prompt comes from the wrong backend/target, resembles approval prose, times out, or exceeds the capture limit and records zero sends;
- allow and deny both match, deny wins, and `request_user_input` never reaches the Enter sender;
- a mixed Sol/Terra transcript is replayed after truncation and cost remains exactly the prior ledger plus only genuinely new requests.

Use fake process/transcript/capture inputs; tests must never address a real terminal.

Add invariant assertions across these fixtures:

- no Enter occurs without freshly recaptured evidence matching session, call, command, backend, target, pattern version, and fingerprint;
- one transcript path is never attached to two live processes in one scan;
- `cost_usd` never decreases within one confirmed transcript session.

Inspect logs and serialization snapshots to prove `PaneCapture.text` is absent; only bounded status/error categories may escape the in-memory matcher.

- [ ] **Step 5: Run all targeted suites**

Run: `cargo test -p codexctl-core discovery::tests -- --nocapture`

Run: `cargo test -p codexctl-core codex_transcript::tests -- --nocapture`

Run: `cargo test -p codexctl-core terminals::tests -- --nocapture`

Run: `cargo test --test integration_tests -- --nocapture`

Run: `cargo test -p codexctl-tui -- --nocapture`

Expected: all pass; no real terminal input occurs.

- [ ] **Step 6: Run workspace quality gates**

Run: `cargo fmt --check`

Expected: exit 0. If it fails, run `cargo fmt`, inspect the formatting-only diff, then rerun.

Run: `cargo test`

Expected: exit 0 with all workspace tests passing.

Run: `cargo clippy -- -D warnings`

Expected: exit 0 with no warnings.

Run: `cargo build`

Expected: exit 0.

- [ ] **Step 7: Inspect the final scoped diff and tracker state**

Run: `jj --no-pager st`

Run: `jj --no-pager diff --git`

Run from `/home/alexander/.beads-planning`: `bd list --status=in_progress`

Expected: only planned files changed; task beads reflect actual completion. If Beads fails from its owning working directory, report the exact command/error and do not reinitialize or overwrite `.beads/issues.jsonl`.

- [ ] **Step 8: Review the final implementation checkpoint**

Verify: `jj --no-pager log -r @ --no-graph` retains the Task 5 description and `jj --no-pager diff --git` is scoped to integration, compatibility, and UI-label work. Do not push, sync, squash, or close the parent bead without explicit user authorization and successful Beads resolution.

## Task Dependency Order

1. Task 1 has no implementation dependency.
2. Task 2 consumes Task 1's stable session identity.
3. Task 3 consumes Task 2's lifecycle and pending call ID.
4. Task 4 consumes stable transcript identity but is sequenced after Task 3 to avoid overlapping edits in `session.rs` and `monitor.rs`.
5. Task 5 consumes Tasks 1-4 and owns the full integration/quality gate.

The tasks are intentionally sequential because Tasks 2-4 share `CodexSession` and monitor state; parallel edits would create avoidable conflicts and weaken review boundaries.

## Stress-Test Results

Reviewed interactively on 2026-07-15 in Beads issue `codexctl-tmk`; all eight recommendations were accepted.

1. Session identity: bind retention to process start/session identity, reject PID reuse, and require two uncached observations for `/clear` transitions.
2. Assignment: replace process-order greedy selection with explicit/retained locks followed by deterministic unique and mutual-best constraint propagation; ambiguity stays pending.
3. Terminal identity: derive backend and exact target from the Codex process and bind versioned prompt evidence to that identity.
4. Capture bounds: capture only pending candidates, enforce 500 ms/80-line/64-KiB limits, clear failed evidence, and always recapture before action.
5. Authorization: use one guarded shell-approval boundary, make deny rules authoritative, and exclude `request_user_input` from blind Enter.
6. Cost ledger: price positive request deltas at the active model, deduplicate by watermark, freeze on reset, and apply long-context rates per request.
7. Execution hygiene: create task beads in their owning planning repository and use one fresh Jujutsu change per task.
8. Compatibility/security: preserve legacy display/JSON surfaces, never persist pane text, and enforce the three safety/monotonicity invariants with negative fixtures and full gates.

## Final Reflection

The revised plan now makes the three dangerous operations explicit and independently testable: transcript attachment, terminal input authorization, and request-cost accumulation. The final adversarial pass found no remaining path where CPU/age/status alone sends input, no order-dependent transcript fallback, and no supported counter replay/reset that lowers displayed session cost. The main compatibility tradeoff is intentional fail-closed behavior: an unfamiliar Codex prompt or unsupported terminal may temporarily remain `Processing`/unconfirmed until a narrow fixture-backed matcher is added, but it cannot receive accidental Enter. Confidence is high enough to proceed task-by-task, with each focused gate required before the next Jujutsu change begins.
