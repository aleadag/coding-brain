# Resumed Transcript Reattachment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Keep a long-running dashboard attached to the rollout that a bare interactive Codex process is currently updating, even when Codex resumes a rollout that started before the retained completed rollout.

**Architecture:** Transcript transitions remain unique and confirmed by two uncached scans. The existing later-session start ordering remains available for an attachment that has not yet been superseded; an older resumed transcript instead requires activity newer than the retained transcript, and once that transition is confirmed the superseded flag prevents the stale later-started rollout from winning back the attachment.

**Tech Stack:** Rust, Cargo unit tests, Jujutsu workspace.

## Global Constraints

- Preserve one-to-one process/transcript assignment.
- Preserve two-uncached-scan confirmation and ambiguity rejection.
- Do not infer status from CPU or modify terminal approval behavior.
- Keep the change within `crates/codexctl-core/src/discovery.rs` plus the approved design and plan documents.
- Do not commit or push without explicit user authorization.

---

### Task 1: Reattach to an older resumed rollout by activity

**Files:**
- Modify: `crates/codexctl-core/src/discovery.rs`
- Test: `crates/codexctl-core/src/discovery.rs`

**Interfaces:**
- Consumes: `compatible_transition(process: &LiveCodexProcess, previous: &RetainedTranscript, candidate: &CodexTranscriptSummary) -> bool` and `assign_transcripts_with_state(...)`.
- Produces: activity-ordered transition compatibility that permits older resumed rollouts without changing transition confirmation or assignment ownership.

**Acceptance Criteria:**
- A retained completed rollout does not switch on the first observation of an older resumed rollout.
- The same unique older rollout is selected on the second uncached observation when its modification time is newer than the retained rollout's last observed activity.
- After reattachment, the stale later-started rollout cannot trigger a transition back.
- Existing `/clear`, ambiguity, startup-transition, and one-to-one assignment tests continue to pass.

- [ ] **Step 1: Write the failing resumed-rollout regression test**

Add beside the existing clear-transition tests:

```rust
#[test]
fn retained_process_transitions_to_older_resumed_transcript_by_activity() {
    let processes = vec![process(11, "/repo", 100_000, "")];
    let mut resumed = transcript("resumed", "/repo", 150_000, "/resumed.jsonl");
    resumed.mtime_ms = 400_000;
    let mut completed = transcript("completed", "/repo", 300_000, "/completed.jsonl");
    completed.mtime_ms = 350_000;
    let transcripts = vec![resumed, completed];
    let mut retained_map = retained(11, 100_000, "completed", "/completed.jsonl", 300_000);
    retained_map.get_mut(&11).unwrap().transcript_mtime_ms = 350_000;
    let mut state = TranscriptAssignmentState {
        retained: retained_map,
        transitions: HashMap::new(),
        unmatched_index_generations: HashMap::new(),
    };

    let first = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
    assert_eq!(first[&11].session_id, "completed");
    assert_eq!(state.transitions[&11].session_id, "resumed");

    let second = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
    assert_eq!(second[&11].session_id, "resumed");

    let third = assign_transcripts_with_state(&processes, &transcripts, &mut state, true);
    assert_eq!(third[&11].session_id, "resumed");
    assert!(state.transitions.is_empty());
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test -p codexctl-core discovery::tests::retained_process_transitions_to_older_resumed_transcript_by_activity -- --exact
```

Expected: FAIL because `compatible_transition` rejects a candidate whose `started_at_ms` is earlier than the retained transcript.

- [ ] **Step 3: Replace transcript-start ordering with activity ordering**

Change only `compatible_transition`:

```rust
fn compatible_transition(
    process: &LiveCodexProcess,
    previous: &RetainedTranscript,
    candidate: &CodexTranscriptSummary,
) -> bool {
    candidate.cwd == process.cwd
        && candidate.session_id != previous.session_id
        && (candidate.mtime_ms > previous.transcript_mtime_ms
            || (!previous.resume_superseded
                && candidate.started_at_ms > previous.transcript_started_at_ms))
}
```

The existing caller still requires the retained transcript to stop advancing, selects a unique greatest candidate modification time, rejects cross-process claims, and requires two uncached observations. The `resume_superseded` guard preserves fresh `/clear` seeding when files have equal mtimes while preventing a completed later-started rollout from reclaiming a process after the older resumed rollout wins.

- [ ] **Step 4: Run the focused and discovery test suites and verify GREEN**

Run:

```bash
cargo test -p codexctl-core discovery::tests::retained_process_transitions_to_older_resumed_transcript_by_activity -- --exact
cargo test -p codexctl-core discovery::tests
```

Expected: both commands PASS.

- [ ] **Step 5: Run workspace quality gates**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
```

Expected: all commands exit successfully with no formatting or Clippy warnings.

- [ ] **Step 6: Verify live behavior without restarting the user's existing dashboard**

Build the updated binary, then run a short host-side headless monitor while Codex is processing:

```bash
timeout 6s target/debug/codexctl --headless --json --interval 500
```

Expected: PID `730687` reports `Processing` from the resumed rollout. The already-running TUI continues using its in-memory old code until the user restarts it; do not kill or restart that process automatically.

- [ ] **Step 7: Review the final Jujutsu diff without committing**

Run:

```bash
jj --no-pager diff --git
jj --no-pager st
```

Expected: every changed line is limited to the approved transcript reattachment behavior, its test, and the approved spec/plan documentation. Leave the changes in the current changeset until the user authorizes commit or description changes.
