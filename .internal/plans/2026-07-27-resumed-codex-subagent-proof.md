# Resumed Codex Subagent Proof Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Let a legitimately resumed Codex child receive a permission decision only after Coding Brain re-establishes exact parent-to-child-and-turn proof from a trusted stop tombstone and bounded child-transcript evidence.

**Architecture:** Extend the existing Codex transcript parser with a bounded fixed-file resume-evidence reader. Extend lifecycle schema-3 state with defaulted, non-authorizing stopped-child tombstones and a lock-serialized re-proof operation. The permission hook attempts re-proof before inference, while final permission persistence remains the authoritative allow gate and catches concurrent stops.

**Tech Stack:** Rust 2024 workspace, Serde/serde_json, `std::fs::File` plus `Read`/`Seek`, existing `LifecycleStore` file locking, Cargo integration tests.

## Global Constraints

- Parent-side `sub_agent_activity(kind = "interacted")` is never authority.
- Transcript evidence refreshes prior trusted topology and can never bootstrap a child that lacks a matching stop tombstone.
- Read at most 1 MiB from the transcript head and 8 MiB from its tail through one regular-file descriptor bounded by its captured initial length.
- Reject task-start evidence at or before the stop timestamp or more than 5,000 ms in the future.
- Keep lifecycle schema version 3; new persisted fields use Serde defaults and older writers may only disable resume automation.
- Keep active and stopped child maps independently capped at `MAX_ACTIVE_SUBAGENTS` (64); evicting the oldest tombstone must fail closed.
- Preserve deterministic denials and the existing proposal → lifecycle → terminal activity → response → delivery ordering.
- Do not infer immediate ancestry from `parent_thread_id`; normal child callbacks expose the root/shared provider session represented by transcript `session_id`.
- Do not add configuration, a watcher, polling, retries, or user-facing documentation.
- Do not commit, push, or publish unless the user separately authorizes it.

## Plan Tracking

- Epic: `codexctl-8198`
- Task 1: `codexctl-don9`
- Task 2: `codexctl-ve5i` (blocked by Task 1)
- Task 3: `codexctl-6u1i` (blocked by Task 2)

The execution workflow must reuse and claim these existing Beads. It must not
create a duplicate epic or duplicate task records.

---

### Task 1: Parse bounded Codex resume evidence

**Files:**
- Modify: `crates/coding-brain-core/src/codex_transcript.rs`
- Modify: `crates/coding-brain-core/src/monitor.rs`

**Interfaces:**
- Consumes: existing `parse_timed_line`, `CodexEvent`, `TimedCodexEvent`, and Codex rollout JSONL schema.
- Produces:
  - `CodexSessionMeta::provider_session_id: Option<String>`
  - `CodexSessionMeta::parent_thread_id: Option<String>`
  - `CodexLifecycleEvent::TaskStarted { turn_id: Option<String> }`
  - `CodexResumeEvidence { child_session_id: String, provider_session_id: String, parent_thread_id: Option<String>, turn_id: String, started_at_ms: u64, requested_transcript_path: PathBuf, canonical_transcript_path: PathBuf }`
  - `read_codex_resume_evidence(path: &Path) -> Result<CodexResumeEvidence, CodexResumeEvidenceError>`

**Acceptance Criteria:**
- A real-schema depth-1 or depth-2 child transcript returns exact child, root/shared provider session, optional immediate parent, newest task-start turn, outer timestamp, and canonical transcript path.
- The reader uses one regular-file descriptor and never reads beyond the captured initial length, 1 MiB head, or 8 MiB tail.
- Partial leading tail rows, malformed JSON, missing metadata, missing IDs/timestamps, non-regular files, and out-of-bound required rows return a bounded reason category without transcript content or raw paths.
- Existing monitor state transitions and transcript parsing tests remain green after `TaskStarted` carries a turn.

- [ ] **Step 1: Add RED parser tests for provider identity and exact task turns**

Add tests beside the existing `codex_transcript.rs` parser tests:

```rust
#[test]
fn parses_child_provider_session_and_immediate_parent_separately() {
    let line = r#"{"timestamp":"2026-07-27T10:23:05.157Z","type":"session_meta","payload":{"id":"child-2","session_id":"root-1","parent_thread_id":"child-1","cwd":"/work/project","model_provider":"openai"}}"#;

    let CodexEvent::SessionMeta(meta) = parse_line(line).unwrap() else {
        panic!("expected session metadata");
    };
    assert_eq!(meta.session_id, "child-2");
    assert_eq!(meta.provider_session_id.as_deref(), Some("root-1"));
    assert_eq!(meta.parent_thread_id.as_deref(), Some("child-1"));
}

#[test]
fn parses_task_started_turn_and_outer_timestamp() {
    let timed = parse_timed_line(
        r#"{"timestamp":"2026-07-27T10:23:05.157Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn-2"}}"#,
    )
    .unwrap();

    assert_eq!(timed.timestamp_ms, Some(1_785_147_785_157));
    assert_eq!(
        timed.event,
        CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted {
            turn_id: Some("turn-2".into()),
        })
    );
}
```

- [ ] **Step 2: Run the parser tests and verify RED**

Run:

```bash
cargo test -p coding-brain-core codex_transcript::tests::parses_child_provider_session_and_immediate_parent_separately
cargo test -p coding-brain-core codex_transcript::tests::parses_task_started_turn_and_outer_timestamp
```

Expected: compilation fails because the metadata fields and structured `TaskStarted` variant do not exist.

- [ ] **Step 3: Extend the shared Codex event model minimally**

Implement the model and parser changes:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexLifecycleEvent {
    TaskStarted { turn_id: Option<String> },
    TaskComplete,
    TurnAborted,
    UserMessage,
    AgentMessage,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSessionMeta {
    pub session_id: String,
    pub provider_session_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub cwd: String,
    pub timestamp: Option<String>,
    pub model_provider: Option<String>,
    pub cli_version: Option<String>,
}
```

Populate `provider_session_id` from `payload.session_id`,
`parent_thread_id` from `payload.parent_thread_id`, and parse task starts as:

```rust
"task_started" => CodexLifecycleEvent::TaskStarted {
    turn_id: payload
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::to_owned),
},
```

Update the four exhaustive `TaskStarted` matches in `monitor.rs` to use
`CodexLifecycleEvent::TaskStarted { .. }`; do not change their behavior.

- [ ] **Step 4: Run focused parser and monitor tests**

Run:

```bash
cargo test -p coding-brain-core codex_transcript
cargo test -p coding-brain-core monitor
```

Expected: all focused tests pass.

- [ ] **Step 5: Add RED bounded resume-evidence tests**

Add these public result types and test the intended interface:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexResumeEvidence {
    pub child_session_id: String,
    pub provider_session_id: String,
    pub parent_thread_id: Option<String>,
    pub turn_id: String,
    pub started_at_ms: u64,
    pub requested_transcript_path: PathBuf,
    pub canonical_transcript_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexResumeEvidenceError {
    NotRegularFile,
    MetadataMissing,
    TaskStartMissing,
    InvalidRecord,
    BoundsExceeded,
}
```

Create temporary JSONL files and assert:

```rust
#[test]
fn reads_newest_bounded_resume_evidence_from_one_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("rollout-child.jsonl");
    fs::write(
        &path,
        concat!(
            "{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"child-2\",\"session_id\":\"root-1\",\"parent_thread_id\":\"child-1\",\"cwd\":\"/work\"}}\n",
            "{\"timestamp\":\"2026-07-27T10:01:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-old\"}}\n",
            "{\"timestamp\":\"2026-07-27T10:02:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-new\"}}\n",
        ),
    )
    .unwrap();

    let evidence = read_codex_resume_evidence(&path).unwrap();
    assert_eq!(evidence.child_session_id, "child-2");
    assert_eq!(evidence.provider_session_id, "root-1");
    assert_eq!(evidence.parent_thread_id.as_deref(), Some("child-1"));
    assert_eq!(evidence.turn_id, "turn-new");
    assert_eq!(evidence.requested_transcript_path, path);
    assert_eq!(
        evidence.canonical_transcript_path,
        std::fs::canonicalize(&evidence.requested_transcript_path).unwrap()
    );
}
```

Add separate tests for a depth-2 metadata row, a directory, a partial first
tail row, invalid required JSON, a metadata row beyond 1 MiB, and a newest task
start beyond the 8 MiB tail.

- [ ] **Step 6: Run the evidence tests and verify RED**

Run:

```bash
cargo test -p coding-brain-core codex_transcript::tests::reads_newest_bounded_resume_evidence_from_one_transcript
```

Expected: compilation fails because `read_codex_resume_evidence` does not exist.

- [ ] **Step 7: Implement the fixed-file bounded reader**

Add exact byte limits and read through one descriptor:

```rust
pub const MAX_CODEX_RESUME_HEAD_BYTES: u64 = 1024 * 1024;
pub const MAX_CODEX_RESUME_TAIL_BYTES: u64 = 8 * 1024 * 1024;

pub fn read_codex_resume_evidence(
    path: &Path,
) -> Result<CodexResumeEvidence, CodexResumeEvidenceError> {
    let requested_transcript_path = path.to_path_buf();
    let canonical_transcript_path =
        std::fs::canonicalize(path).map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let mut file =
        File::open(&canonical_transcript_path)
            .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let metadata = file
        .metadata()
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    if !metadata.is_file() {
        return Err(CodexResumeEvidenceError::NotRegularFile);
    }
    let initial_len = metadata.len();

    let head_len = initial_len.min(MAX_CODEX_RESUME_HEAD_BYTES);
    let mut head = Vec::with_capacity(head_len as usize);
    Read::by_ref(&mut file)
        .take(head_len)
        .read_to_end(&mut head)
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    if !head.contains(&b'\n') {
        return Err(if initial_len > head_len {
            CodexResumeEvidenceError::BoundsExceeded
        } else {
            CodexResumeEvidenceError::MetadataMissing
        });
    }
    let first_line = head
        .split(|byte| *byte == b'\n')
        .next()
        .filter(|line| !line.is_empty())
        .ok_or(CodexResumeEvidenceError::MetadataMissing)?;
    let meta = match parse_line(
        std::str::from_utf8(first_line)
            .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?,
    ) {
        Some(CodexEvent::SessionMeta(meta)) => meta,
        _ => return Err(CodexResumeEvidenceError::MetadataMissing),
    };

    let tail_start = initial_len.saturating_sub(MAX_CODEX_RESUME_TAIL_BYTES);
    file.seek(SeekFrom::Start(tail_start))
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let mut tail = Vec::with_capacity((initial_len - tail_start) as usize);
    Read::by_ref(&mut file)
        .take(initial_len - tail_start)
        .read_to_end(&mut tail)
        .map_err(|_| CodexResumeEvidenceError::InvalidRecord)?;
    let tail = if tail_start == 0 {
        tail.as_slice()
    } else {
        let newline = tail
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or(CodexResumeEvidenceError::BoundsExceeded)?;
        &tail[newline + 1..]
    };
    let mut newest = None;
    for line in tail.split(|byte| *byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let Some(timed) = parse_timed_line(line) else {
            continue;
        };
        if let CodexEvent::Lifecycle(CodexLifecycleEvent::TaskStarted {
            turn_id: Some(turn_id),
        }) = timed.event
            && let Some(started_at_ms) = timed.timestamp_ms
        {
            newest = Some((turn_id, started_at_ms));
        }
    }
    let (turn_id, started_at_ms) =
        newest.ok_or(CodexResumeEvidenceError::TaskStartMissing)?;
    Ok(CodexResumeEvidence {
        child_session_id: meta.session_id,
        provider_session_id: meta
            .provider_session_id
            .ok_or(CodexResumeEvidenceError::MetadataMissing)?,
        parent_thread_id: meta.parent_thread_id,
        turn_id,
        started_at_ms,
        requested_transcript_path,
        canonical_transcript_path,
    })
}
```

Keep error categories bounded and implement `Display` with fixed strings only.
The explicit newline check above ensures a required first row that fills the
1 MiB head returns `BoundsExceeded` rather than parsing a partial row. Tail
iteration is file order: later complete valid `task_started` rows replace
earlier ones even if their embedded timestamps are not monotonic.

- [ ] **Step 8: Run Task 1 tests and format check**

Run:

```bash
cargo test -p coding-brain-core codex_transcript
cargo test -p coding-brain-core monitor
cargo fmt --check
```

Expected: all commands pass.

- [ ] **Step 9: Prepare the Task 1 checkpoint**

Run:

```bash
git diff --check -- crates/coding-brain-core/src/codex_transcript.rs crates/coding-brain-core/src/monitor.rs
git status --short
```

Expected: only the approved spec/plan and Task 1 files are changed. Do not
commit without explicit user authorization.

---

### Task 2: Retain stopped topology and re-prove it atomically

**Files:**
- Modify: `crates/coding-brain-core/src/lifecycle/projection.rs`
- Modify: `crates/coding-brain-core/src/lifecycle/store.rs`

**Interfaces:**
- Consumes: `CodexResumeEvidence` from Task 1 and exact linked `LifecycleIdentity`.
- Produces:
  - `StoppedSubagentState { stopped_sequence: u64, received_at_ms: u64, turn_id: String }`
  - `SessionLifecycleState::stopped_subagents: BTreeMap<String, StoppedSubagentState>`
  - `LifecycleStore::codex_subagent_is_proven(identity: &LifecycleIdentity) -> Result<bool, StoreError>`
  - `LifecycleStore::reprove_codex_subagent(identity: &LifecycleIdentity, evidence: &CodexResumeEvidence) -> Result<ApplyOutcome, StoreError>`

**Acceptance Criteria:**
- Accepted Codex stop removes active authority, deletes transient child state, and retains an exact non-authorizing tombstone.
- Only exact child, root/shared provider session, transcript path, new turn, post-stop timestamp, and non-future evidence can replace a tombstone with an active edge.
- Old-turn, never-proven, mismatched, stale, future, capacity, retention, cleanup, and schema-default cases fail closed.
- Two matching re-proofs converge; re-proof followed by stop makes final permission persistence reject.

- [ ] **Step 1: Add RED projection tests for tombstone lifecycle**

Add helpers and assertions in `projection.rs`:

```rust
#[test]
fn codex_stop_moves_exact_child_from_active_to_stopped() {
    let mut snapshot = LifecycleSnapshot::default();
    assert_eq!(
        snapshot.apply(codex_subagent_start("root-1", "child-1", "turn-1"), 1_000),
        ApplyOutcome::Applied
    );
    assert_eq!(
        snapshot.apply(codex_subagent_stop("root-1", "child-1", "turn-1"), 2_000),
        ApplyOutcome::Applied
    );

    let root = &snapshot.sessions[&key(AgentProvider::Codex, "root-1")];
    assert!(!root.active_subagents.contains_key("child-1"));
    assert_eq!(root.stopped_subagents["child-1"].turn_id, "turn-1");
    assert_eq!(root.stopped_subagents["child-1"].received_at_ms, 2_000);
}

#[test]
fn stopped_codex_child_remains_unproven_for_ordinary_events() {
    let mut snapshot = stopped_codex_child();
    assert_eq!(
        snapshot.apply(linked_tool("child-1", "root-1", "turn-1"), 3_000),
        ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent)
    );
}
```

Also cover real `SubagentStart` clearing the tombstone, non-compact root
restart clearing it, nested subtree removal retaining only the immediate
parent's resumable stopped-child tombstone, and oldest-stop eviction at 64.
Add a dedicated assertion that the `remove_linked_children` call made by the
same accepted `SubagentStop` deletes the child session and descendants without
erasing the new tombstone in the immediate parent's map.

- [ ] **Step 2: Run the projection tests and verify RED**

Run:

```bash
cargo test -p coding-brain-core lifecycle::projection::tests::codex_stop_moves_exact_child_from_active_to_stopped
```

Expected: compilation fails because `stopped_subagents` does not exist.

- [ ] **Step 3: Implement defaulted bounded tombstones**

Add:

```rust
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoppedSubagentState {
    pub stopped_sequence: u64,
    pub received_at_ms: u64,
    pub turn_id: String,
}
```

Add to `SessionLifecycleState`:

```rust
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub stopped_subagents: BTreeMap<String, StoppedSubagentState>,
```

Initialize and clear it with existing transient topology. On accepted Codex
`SubagentStop`, remove the active entry, evict the minimum
`stopped_sequence` when inserting a distinct sixty-fifth child, then insert
the exact tombstone before linked child-session cleanup. On real
`SubagentStart`, remove the same child's tombstone before adding its active
edge.

Extend `valid_snapshot_shape` to require bounded valid IDs/turns, nonzero
sequences below `next_sequence`, and disjoint active/stopped child IDs.
Extend retention to expire tombstones older than `SESSION_RETENTION_MS`.
Tombstones inside a removed descendant disappear with that descendant state;
do not make generic subtree cleanup remove the immediate parent's tombstone for
the stopped subtree root. Root `Stop` and non-compact restart clear the root's
own tombstones through `clear_transient_status`.

- [ ] **Step 4: Run projection and store compatibility tests**

Run:

```bash
cargo test -p coding-brain-core lifecycle::projection
cargo test -p coding-brain-core lifecycle::store
```

Expected: all existing and new tombstone tests pass, including a schema-3 JSON
fixture with the field absent.

- [ ] **Step 5: Add RED store tests for exact resume proof**

In `store.rs`, construct `CodexResumeEvidence` directly and test:

```rust
#[test]
fn exact_newer_codex_resume_evidence_reactivates_the_child() {
    let store = store();
    assert_eq!(
        store.record_at(subagent_start("root-1", "child-1", "turn-1"), 1_000),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(subagent_stop("root-1", "child-1", "turn-1"), 2_000),
        Ok(ApplyOutcome::Applied)
    );
    let identity = linked_identity(
        "child-1",
        "root-1",
        "turn-2",
        "/tmp/rollout-child-1.jsonl",
    );
    let evidence = resume_evidence(
        "child-1",
        "root-1",
        "turn-2",
        "/tmp/rollout-child-1.jsonl",
        2_500,
    );

    assert_eq!(
        store.reprove_codex_subagent_at(&identity, &evidence, 3_000),
        Ok(ApplyOutcome::Applied)
    );
    assert_eq!(
        store.record_at(permission(identity), 3_001),
        Ok(ApplyOutcome::Applied)
    );
}
```

Add table-driven cases for old turn, never-proven child, child mismatch,
provider-session mismatch, turn mismatch, canonical transcript mismatch,
timestamp equal to/before stop, and timestamp over `now + 5_000`. Assert
`UnprovenSubagent`, `ProviderSessionMismatch`, or `SubagentTurnMismatch`
according to the identity dimension, with no sequence consumption.

Add deterministic interleavings:

```rust
// Matching second re-proof observes the already-active exact edge.
assert_eq!(
    store.reprove_codex_subagent_at(&identity, &evidence, 3_001),
    Ok(ApplyOutcome::Ignored(IgnoreReason::Duplicate))
);

// A real stop after re-proof removes authority before final persistence.
assert_eq!(
    store.record_at(subagent_stop("root-1", "child-1", "turn-2"), 3_002),
    Ok(ApplyOutcome::Applied)
);
assert_eq!(
    store.record_at(permission(identity), 3_003),
    Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent))
);
```

Add a third ordering in a fresh store: re-prove `turn-2`, then apply the
delayed parent-scoped `SubagentStop(root-1, child-1, turn-1)`. Assert
`SubagentTurnMismatch`, then assert a `turn-2` permission still applies.

- [ ] **Step 6: Run the resume-proof tests and verify RED**

Run:

```bash
cargo test -p coding-brain-core lifecycle::store::tests::exact_newer_codex_resume_evidence_reactivates_the_child
```

Expected: compilation fails because the re-proof API does not exist.

- [ ] **Step 7: Implement lock-serialized re-proof**

Add a public wrapper and private timestamp-injected method:

```rust
pub fn reprove_codex_subagent(
    &self,
    identity: &LifecycleIdentity,
    evidence: &CodexResumeEvidence,
) -> Result<ApplyOutcome, StoreError> {
    self.reprove_codex_subagent_at(identity, evidence, epoch_ms())
}
```

Inside `reprove_codex_subagent_at`, take the same exclusive lifecycle lock,
load/migrate/retain the snapshot, and then:

```rust
if identity.provider() != AgentProvider::Codex {
    return Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent));
}
let (Some(parent_id), Some(turn_id), Some(transcript_path)) = (
    identity.provider_session_id(),
    identity.turn_id(),
    identity.transcript_path(),
) else {
    return Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent));
};
let parent_key = AgentSessionKey::native(AgentProvider::Codex, parent_id).storage_key();
let Some(parent) = snapshot.sessions.get_mut(&parent_key) else {
    return Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent));
};
if let Some(active) = parent.active_subagents.get(identity.session_id()) {
    return Ok(if active.turn_id == turn_id {
        ApplyOutcome::Ignored(IgnoreReason::Duplicate)
    } else {
        ApplyOutcome::Ignored(IgnoreReason::SubagentTurnMismatch)
    });
}
let Some(stopped) = parent.stopped_subagents.get(identity.session_id()) else {
    return Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent));
};
if evidence.child_session_id != identity.session_id()
    || evidence.provider_session_id != parent_id
    || evidence.turn_id != turn_id
    || evidence.requested_transcript_path != transcript_path
    || evidence.turn_id == stopped.turn_id
    || evidence.started_at_ms <= stopped.received_at_ms
    || evidence.started_at_ms > received_at_ms.saturating_add(5_000)
{
    return Ok(ApplyOutcome::Ignored(IgnoreReason::UnprovenSubagent));
}
```

After validation, consume one sequence, remove the tombstone, insert
`ActiveSubagentState` for the new turn, update the parent's latest receive
time/sequence without fabricating a lifecycle event name, validate the whole
snapshot, and persist it through the existing atomic temp-file path.

Refactor only the existing load/retain/validate/persist boilerplate needed to
avoid duplicating unsafe persistence behavior; do not introduce a general
transaction abstraction.

- [ ] **Step 8: Add the exact active-edge read API**

Add:

```rust
pub fn codex_subagent_is_proven(
    &self,
    identity: &LifecycleIdentity,
) -> Result<bool, StoreError> {
    if identity.provider() != AgentProvider::Codex {
        return Ok(false);
    }
    let (Some(parent_id), Some(turn_id)) =
        (identity.provider_session_id(), identity.turn_id())
    else {
        return Ok(false);
    };
    let view = self.read()?;
    let Some(snapshot) = view.snapshot else {
        return Ok(false);
    };
    let parent_key =
        AgentSessionKey::native(AgentProvider::Codex, parent_id).storage_key();
    Ok(snapshot.sessions.get(&parent_key).is_some_and(|parent| {
        parent
            .active_subagents
            .get(identity.session_id())
            .is_some_and(|active| active.turn_id == turn_id)
    }))
}
```

Test exact active child/parent/turn success and child, parent, turn, provider,
missing, corrupt, and unavailable failure. This is only a read optimization;
`record_permission` remains authoritative if the edge changes or expires after
the read.

- [ ] **Step 9: Run all lifecycle tests**

Run:

```bash
cargo test -p coding-brain-core lifecycle
```

Expected: all lifecycle projection, store, input, and reconcile tests pass.

- [ ] **Step 10: Prepare the Task 2 checkpoint**

Run:

```bash
git diff --check -- crates/coding-brain-core/src/lifecycle/projection.rs crates/coding-brain-core/src/lifecycle/store.rs
git status --short
```

Expected: only the approved documents and Task 1–2 files are changed. Do not
commit without explicit user authorization.

---

### Task 3: Re-prove before Codex permission inference and verify delivery

**Files:**
- Modify: `src/brain/permission_hook.rs`
- Modify: `tests/hook_activity.rs`
- Modify: `tests/lifecycle_hook_cli.rs` only if its existing process-level permission seam is needed for the final delivery assertion.

**Interfaces:**
- Consumes:
  - `read_codex_resume_evidence(path: &Path)`
  - `LifecycleStore::codex_subagent_is_proven(identity)`
  - `LifecycleStore::reprove_codex_subagent(identity, evidence)`
  - existing `PermissionHookRequest.lifecycle`
- Produces:
  - private `try_reprove_codex_subagent(store: &LifecycleStore, identity: &LifecycleIdentity) -> Option<CodexResumeEvidenceError>`
  - unchanged provider permission response schema and activity ordering.

**Acceptance Criteria:**
- Spawn → accepted start → accepted stop → exact resumed transcript → child permission produces one persisted and delivered allow.
- No fresh transcript proof, stopped old turn, and mismatched child/provider/turn/path/stale/future evidence emit no allow and retain `UnprovenSubagent` error evidence.
- Parent `interacted` data is neither read nor accepted.
- Deterministic child denial still emits deny when topology cannot be re-proven.
- Relevant lifecycle, Codex provider-hook, lifecycle-hook, and permission-hook tests pass.

- [ ] **Step 1: Add a RED full-hook resume regression**

Add a helper in `tests/hook_activity.rs` that writes a real child transcript and
updates the permission fixture's `transcript_path`:

```rust
fn child_permission_payload_with_transcript(
    home: &Path,
    agent_id: &str,
    turn_id: &str,
    transcript: &Path,
) -> Vec<u8> {
    let mut value: serde_json::Value =
        serde_json::from_slice(&child_permission_payload(home, agent_id, turn_id)).unwrap();
    value["transcript_path"] = serde_json::json!(transcript);
    serde_json::to_vec(&value).unwrap()
}

fn write_child_resume_transcript(
    path: &Path,
    child_id: &str,
    provider_session_id: &str,
    immediate_parent_id: &str,
    turn_id: &str,
    timestamp: &str,
) {
    fs::write(
        path,
        format!(
            "{{\"timestamp\":\"2026-07-27T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{child_id}\",\"session_id\":\"{provider_session_id}\",\"parent_thread_id\":\"{immediate_parent_id}\",\"cwd\":\"/work\"}}}}\n\
             {{\"timestamp\":\"{timestamp}\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn_id}\"}}}}\n"
        ),
    )
    .unwrap();
}
```

Use a timestamp exactly one second after the test's current time so it is
deterministically after the persisted stop and still within the five-second
future-skew allowance:

```rust
#[test]
fn resumed_codex_child_permission_is_reproved_and_delivered() {
    let home = tempfile::tempdir().unwrap();
    install_model_fixture(home.path(), "approve");
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_start_payload(home.path(), "child-a", "turn-a"),
    );
    run_provider_lifecycle_hook(
        home.path(),
        "codex",
        None,
        &subagent_stop_payload(home.path(), "child-a", "turn-a"),
    );
    let transcript = home.path().join("rollout-child-a.jsonl");
    write_child_resume_transcript(
        &transcript,
        "child-a",
        "root-1",
        "root-1",
        "turn-b",
        &one_second_from_now_rfc3339(),
    );

    let permission = run_provider_permission_hook(
        home.path(),
        "codex",
        None,
        &child_permission_payload_with_transcript(
            home.path(),
            "child-a",
            "turn-b",
            &transcript,
        ),
    );

    assert!(permission.status.success());
    let response: serde_json::Value = serde_json::from_slice(&permission.stdout).unwrap();
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "allow"
    );
    let events = activity(home.path()).read().unwrap().events().to_vec();
    assert!(events.iter().any(|event| {
        event.state == ActivityState::Delivered
            && event.session.as_ref().is_some_and(|session| {
                session.session_id == "child-a"
                    && session.provider_session_id.as_deref() == Some("root-1")
                    && session.turn_id.as_deref() == Some("turn-b")
            })
    }));
}
```

Implement `one_second_from_now_rfc3339()` using
`OffsetDateTime::now_utc() + time::Duration::seconds(1)` and the workspace's
existing `time` crate rather than adding a dependency.

- [ ] **Step 2: Run the full-hook regression and verify RED**

Run:

```bash
cargo test --test hook_activity resumed_codex_child_permission_is_reproved_and_delivered -- --exact
```

Expected: FAIL because permission stdout is empty and stderr contains
`UnprovenSubagent`.

- [ ] **Step 3: Add private re-proof orchestration**

In `permission_hook.rs`, import the Task 1 reader and add:

```rust
fn try_reprove_codex_subagent(
    store: &LifecycleStore,
    identity: &LifecycleIdentity,
) -> Option<CodexResumeEvidenceError> {
    if identity.provider() != AgentProvider::Codex
        || identity.provider_session_id().is_none()
    {
        return None;
    }
    match store.codex_subagent_is_proven(identity) {
        Ok(true) => return None,
        Ok(false) => {}
        Err(_) => return Some(CodexResumeEvidenceError::InvalidRecord),
    }
    let Some(path) = identity.transcript_path() else {
        return Some(CodexResumeEvidenceError::MetadataMissing);
    };
    let evidence = match read_codex_resume_evidence(path) {
        Ok(evidence) => evidence,
        Err(error) => return Some(error),
    };
    match store.reprove_codex_subagent(identity, &evidence) {
        Ok(ApplyOutcome::Applied | ApplyOutcome::Ignored(IgnoreReason::Duplicate)) => None,
        Ok(ApplyOutcome::Ignored(_)) => Some(CodexResumeEvidenceError::InvalidRecord),
        Err(_) => Some(CodexResumeEvidenceError::InvalidRecord),
    }
}
```

Call it once after the permission request and activity context have been parsed
but before inference. Keep its bounded category in local hook context. Do not
return early and do not emit an allow from this function. If final executable
permission persistence fails, append the fixed category to the existing
bounded diagnostic; never include transcript content or its path.

The read-only active-edge check is an optimization, not authority for the
response. If an edge stops or expires after it returns true, final
`record_permission` must still suppress the allow.

- [ ] **Step 4: Run the valid resume regression**

Run:

```bash
cargo test --test hook_activity resumed_codex_child_permission_is_reproved_and_delivered -- --exact
```

Expected: PASS with a Codex `allow` response and a `Delivered` child Decision
activity for `turn-b`.

- [ ] **Step 5: Add fail-closed integration cases**

Add a table-driven integration test that recreates a fresh start/stop per case
and mutates exactly one dimension:

```rust
for case in [
    ResumeCase::NoTaskStart,
    ResumeCase::StoppedTurn,
    ResumeCase::WrongChild,
    ResumeCase::WrongProviderSession,
    ResumeCase::WrongTurn,
    ResumeCase::StaleTimestamp,
    ResumeCase::FutureTimestamp,
] {
    let output = run_resume_case(case);
    assert!(output.stdout.is_empty(), "{case:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("UnprovenSubagent"),
        "{case:?}"
    );
}
```

Retain the existing `deterministic_child_deny_survives_missing_topology` test
unchanged to prove denial availability. Add one depth-2 transcript case where
`provider_session_id = root-1` and `parent_thread_id = child-parent`; it must
deliver based on the root/shared provider identity. Transcript-path mismatch
remains a Task 2 store-boundary test: the production hook always reads evidence
from the callback's own path, so a separate evidence path cannot be injected
through the process interface.

- [ ] **Step 6: Run relevant hook suites**

Run:

```bash
cargo test --test hook_activity codex_child
cargo test --test lifecycle_hook_cli child
cargo test brain::permission_hook
cargo test provider_hooks::codex
```

Expected: all selected tests pass; stopped children without fresh exact proof
still show `UnprovenSubagent`.

- [ ] **Step 7: Run full repository quality gates**

Run:

```bash
cargo fmt
cargo test
cargo clippy -- -D warnings
cargo fmt --check
cargo build
```

Expected: every command exits zero. If the bare shell lacks dependencies, run
the same commands through `direnv exec .`; if `.envrc` is blocked, use
`nix develop path:. --command`.

- [ ] **Step 8: Verify the final diff and Bead acceptance criteria**

Run:

```bash
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
git diff --stat
git status --short
bd -C /home/alexander/.beads-planning show codexctl-4gi0
```

Expected:

- every changed production/test line traces to `codexctl-4gi0`;
- the approved spec and plan are present;
- the valid resume regression passes;
- stale and mismatched cases remain fail-closed;
- no unrelated files are changed.

- [ ] **Step 9: Prepare the implementation handoff**

Update and close child implementation Beads with exact test evidence. Close
`codexctl-4gi0` only after all acceptance criteria and quality gates pass.
Report changed files, verification output, Bead status, the retained
stress-test restore stash, and that commit/push remain awaiting separate user
authorization.

## Stress Test Results: Resumed Codex Subagent Proof Plan

### Resolved Decisions

- Keep three reviewable tasks: transcript evidence, lifecycle topology, and
  permission integration.
- Add a read-only exact active-edge preflight so ordinary active child
  permissions do not depend on transcript I/O; final persistence remains the
  authority.
- Make metadata-newline and file-order tail semantics executable requirements,
  not prose-only edge cases.
- Model concurrency with parent-scoped `SubagentStop`; linked child `Stop`
  cannot stand in for topology removal.
- Preserve the immediate parent's new tombstone while deleting stopped child
  session and descendant state.
- Keep re-proof non-authorizing and preserve all executable decision
  persistence ordering.
- Use injected store timestamps and one post-stop `now + 1 second` process
  fixture; add no sleeps or timing races.
- Reuse the already-created plan epic/tasks and their dependency chain.
- Keep schema-3 rollback fail-safe and retain both recoverable `4gi0` stashes
  through handoff.

### Changes Made

- Added `LifecycleStore::codex_subagent_is_proven` and wired it into the Task 3
  interface.
- Corrected concurrency tests to use parent-scoped stops and cover delayed
  old-turn stops.
- Strengthened bounded reader and tombstone-cleanup steps.
- Separated the normalized callback path used for identity binding from the
  canonical regular file opened by the evidence reader.
- Added exact existing Bead IDs for execution reuse.

### Deferred / Parking Lot

- No transcript index or watcher is introduced for task-start rows outside the
  fixed tail bound.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Implementation must keep the active-edge preflight
  read-only and must not treat it as a substitute for final permission
  persistence.
