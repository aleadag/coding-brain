# Nested Kitty Approval Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Detect a visible Codex shell-permission prompt when Codex runs beneath Neovim in Kitty, so the session becomes actionable `NeedsInput` without weakening guarded input.

**Architecture:** Extend Kitty capture by walking at most 16 exact parent PID targets. For current Codex `functions.exec` wrappers, recognize a nested `tools.exec_command(` invocation and bind the last complete visible approval block to the outer call ID; promote its displayed command to the existing pending shell identity so rules, the brain, and stale-prompt revalidation use the same evidence.

**Tech Stack:** Rust, Cargo unit tests, Kitty remote control CLI, POSIX `ps`

## Global Constraints

- Never match a Kitty pane by cwd, title, or another fuzzy selector for approval capture.
- Finding a pane does not authorize Enter; existing call ID, command, prompt fingerprint, backend, target, process, and transcript checks remain mandatory.
- Stop traversal after 16 candidates, at PID zero, on a missing parent, or on a repeated PID.
- Direct shell calls must still match their transcript command exactly.
- Wrapper calls without `tools.exec_command(` remain non-actionable.
- Earlier lookalike prompt blocks must never override a later complete prompt.
- Keep changes limited to terminal approval capture/matching, their unit tests, and the approved design/plan documents.

---

### Task 1: Detect nested Kitty wrapper approvals

**Tracking:** `codexctl-1bq.1`

**Files:**
- Modify: `crates/codexctl-core/src/terminals/kitty.rs`
- Test: `crates/codexctl-core/src/terminals/kitty.rs`
- Modify: `crates/codexctl-core/src/terminals/mod.rs`
- Test: `crates/codexctl-core/src/terminals/mod.rs`

**Interfaces:**
- Consumes: `CodexSession::pid`, pending call identity, raw custom-tool input, terminal pane text, `run_bounded`, `checked_capture`, and Kitty's exact `pid:<pid>` selector.
- Produces: ancestor-aware `capture`; wrapper-aware `ApprovalEvidence` whose tool is `exec_command` and command is the last complete visible prompt command; promoted pending shell identity for downstream rules and brain evaluation.

**Acceptance Criteria:**
- Direct PID capture performs no parent lookup.
- Nested capture tries exact parent PID targets in order and records the successful target.
- Traversal stops safely on root, cycle, missing parent, or 16 candidates.
- An unmatched capture stays non-actionable through the existing approval state machine.
- Current `exec` wrappers containing `tools.exec_command(` become actionable only from the last complete prompt block.
- Wrappers without a nested shell invocation remain non-actionable.
- Direct shell commands retain exact transcript/display equality.
- Existing stale-prompt identity comparison rejects changed wrapper evidence.
- Focused and full Rust quality gates pass.

- [ ] **Step 1: Add failing direct and nested capture tests**

Add a `#[cfg(test)]` module to `kitty.rs`. Exercise a private injected helper so tests do not invoke real `ps` or Kitty:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn pane(target: &str) -> PaneCapture {
        PaneCapture {
            backend: Terminal::Kitty,
            target: target.into(),
            text: "approval prompt".into(),
        }
    }

    #[test]
    fn direct_pid_capture_does_not_lookup_parent() {
        let mut parent_lookups = 0;
        let mut targets = Vec::new();
        let capture = capture_with(
            30,
            |_| {
                parent_lookups += 1;
                None
            },
            |target| {
                targets.push(target.to_string());
                Ok(pane(target))
            },
        )
        .unwrap();

        assert_eq!(targets, ["pid:30"]);
        assert_eq!(parent_lookups, 0);
        assert_eq!(capture.target, "pid:30");
    }

    #[test]
    fn nested_pid_capture_uses_first_matching_ancestor() {
        let mut targets = Vec::new();
        let capture = capture_with(
            30,
            |pid| match pid {
                30 => Some(20),
                20 => Some(10),
                _ => None,
            },
            |target| {
                targets.push(target.to_string());
                match target {
                    "pid:10" => Ok(pane(target)),
                    _ => Err("no matching Kitty window".into()),
                }
            },
        )
        .unwrap();

        assert_eq!(targets, ["pid:30", "pid:20", "pid:10"]);
        assert_eq!(capture.target, "pid:10");
    }
}
```

- [ ] **Step 2: Run focused tests and confirm the missing helper fails compilation**

Run:

```bash
cargo test -p codexctl-core terminals::kitty::tests -- --nocapture
```

Expected: compilation fails because `capture_with` is not yet defined.

- [ ] **Step 3: Implement the minimal injected traversal and real process helpers**

Replace the direct-only body of `capture` and add these private helpers above it:

```rust
const MAX_PID_TARGETS: usize = 16;

fn parent_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn capture_target(target: &str) -> Result<PaneCapture, String> {
    let output = run_bounded(
        std::process::Command::new("kitty")
            .args(["@", "get-text", "--match", target, "--extent", "screen"]),
    )?;
    checked_capture(Terminal::Kitty, target.into(), output)
}

fn capture_with(
    pid: u32,
    mut parent_of: impl FnMut(u32) -> Option<u32>,
    mut capture: impl FnMut(&str) -> Result<PaneCapture, String>,
) -> Result<PaneCapture, String> {
    let mut current = pid;
    let mut visited = Vec::with_capacity(MAX_PID_TARGETS);
    let mut last_error = None;

    for _ in 0..MAX_PID_TARGETS {
        if current == 0 || visited.contains(&current) {
            break;
        }
        visited.push(current);

        let target = format!("pid:{current}");
        match capture(&target) {
            Ok(pane) => return Ok(pane),
            Err(error) => last_error = Some(error),
        }

        let Some(parent) = parent_of(current) else {
            break;
        };
        current = parent;
    }

    Err(last_error.unwrap_or_else(|| "no Kitty PID target matched the session".into()))
}

pub fn capture(session: &CodexSession) -> Result<PaneCapture, String> {
    capture_with(session.pid, parent_pid, capture_target)
}
```

- [ ] **Step 4: Run focused tests and confirm direct/nested cases pass**

Run:

```bash
cargo test -p codexctl-core terminals::kitty::tests -- --nocapture
```

Expected: both tests pass.

- [ ] **Step 5: Add boundary tests for missing parent, root, cycle, and candidate limit**

Add tests that always return capture errors and assert the exact attempted targets:

```rust
#[test]
fn traversal_stops_when_parent_is_missing() {
    let mut targets = Vec::new();
    let error = capture_with(
        30,
        |_| None,
        |target| {
            targets.push(target.to_string());
            Err("no match".into())
        },
    )
    .unwrap_err();
    assert_eq!(targets, ["pid:30"]);
    assert_eq!(error, "no match");
}

#[test]
fn traversal_stops_before_root_or_cycle() {
    for parent in [0, 30] {
        let mut targets = Vec::new();
        capture_with(
            30,
            |_| Some(parent),
            |target| {
                targets.push(target.to_string());
                Err("no match".into())
            },
        )
        .unwrap_err();
        assert_eq!(targets, ["pid:30"]);
    }
}

#[test]
fn traversal_is_bounded() {
    let mut targets = Vec::new();
    capture_with(
        100,
        |pid| Some(pid - 1),
        |target| {
            targets.push(target.to_string());
            Err("no match".into())
        },
    )
    .unwrap_err();
    assert_eq!(targets.len(), MAX_PID_TARGETS);
    assert_eq!(targets.first().map(String::as_str), Some("pid:100"));
    assert_eq!(targets.last().map(String::as_str), Some("pid:85"));
}
```

- [ ] **Step 6: Run focused tests and format the implementation**

Run:

```bash
cargo test -p codexctl-core terminals::kitty::tests -- --nocapture
cargo fmt
cargo fmt --check
```

Expected: all Kitty tests pass and formatting produces no remaining diff.

- [ ] **Step 7: Run full workspace verification**

Run:

```bash
cargo test
cargo clippy -- -D warnings
cargo build
```

Expected: every command exits successfully with no test failures or Clippy warnings.

- [ ] **Step 8: Add failing wrapper and last-prompt tests**

In the existing approval test module in `terminals/mod.rs`, construct a pending
custom `exec` session and verify that the confirmed evidence is promoted to the
nested shell identity:

```rust
fn pending_exec_wrapper_session(call_id: &str, input: &str) -> CodexSession {
    let mut session = pending_shell_session(call_id, input);
    session.pending_tool_name = Some("exec".into());
    session
}

#[test]
fn exec_wrapper_uses_last_complete_visible_prompt() {
    let earlier = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt")
        .replace("$ cargo test", "$ cargo clippy");
    let current = include_str!("../../../../tests/fixtures/codex-shell-approval-pane.txt");
    let pane = format!("{earlier}\n\n{current}");
    let mut session = pending_exec_wrapper_session(
        "call-7",
        "const args = next(); await tools.exec_command(args);",
    );
    let io = FakeApprovalIo::with_captures([Ok(capture(&pane))]);

    refresh_approval_observation_with(&io, &mut session, 10_000);

    let ApprovalObservation::Confirmed(evidence) = &session.approval else {
        panic!("wrapper approval was not confirmed");
    };
    assert_eq!(evidence.tool, "exec_command");
    assert_eq!(evidence.command, "cargo test");
    assert_eq!(session.pending_tool_name.as_deref(), Some("exec_command"));
    assert_eq!(session.pending_tool_input.as_deref(), Some("cargo test"));
}

#[test]
fn exec_wrapper_without_nested_shell_is_not_actionable() {
    let mut session = pending_exec_wrapper_session("call-7", "text(true);");
    let io = FakeApprovalIo::with_captures([Ok(capture(include_str!(
        "../../../../tests/fixtures/codex-shell-approval-pane.txt"
    )))]);

    refresh_approval_observation_with(&io, &mut session, 10_000);

    assert!(matches!(session.approval, ApprovalObservation::NotChecked));
}
```

- [ ] **Step 9: Run approval tests and confirm wrapper detection is RED**

Run:

```bash
cargo test -p codexctl-core terminals::tests::exec_wrapper -- --nocapture
```

Expected: the wrapper confirmation test fails because `exec` is not yet a
pending shell call.

- [ ] **Step 10: Implement last-block wrapper matching and identity promotion**

Refactor prompt matching into a line-based helper that scans question lines
from bottom to top. For each candidate, require one pattern's ordered choice
anchors and confirmation line after the question, and extract the last `$ `
command before the first choice. Direct shell calls require exact normalized
equality with transcript input. An `exec` wrapper is eligible only when its raw
input contains `tools.exec_command(` and uses the displayed command:

```rust
fn is_exec_wrapper(tool: &str, input: &str) -> bool {
    tool == "exec" && input.contains("tools.exec_command(")
}

// After constructing a confirmed observation:
if let ApprovalObservation::Confirmed(evidence) = &observation {
    session.pending_tool_name = Some(evidence.tool.clone());
    session.pending_tool_input = Some(evidence.command.clone());
}
session.approval = observation;
```

Keep `ApprovalEvidence` equality and `approve_shell_permission_with`
unchanged so the second capture must reproduce the same final prompt block,
backend, resolved target, call ID, tool, and command.

- [ ] **Step 11: Run focused and full verification**

Run:

```bash
cargo test -p codexctl-core terminals::tests -- --nocapture
cargo test -p codexctl-core terminals::kitty::tests -- --nocapture
cargo fmt
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
```

Expected: all focused tests and workspace gates pass.

- [ ] **Step 12: Verify the live nested approval flow**

Restart the dashboard with its existing arguments, poll PID `730687` through a
harmless shell approval, and confirm:

```text
Dashboard transition: Processing -> Needs Input
Approval evidence backend: Kitty
Approval evidence target: pid:<owning Kitty window child>
Brain: receives the displayed shell command, not the JavaScript wrapper
```

Confirm that dismissing or changing the final prompt before the action causes
revalidation to cancel Enter.

- [ ] **Step 13: Review the final jj change**

Run:

```bash
jj --no-pager st
jj --no-pager diff --git
```

Expected: only the approved spec, plan, and Kitty backend/test changes appear under `🐛 fix: capture nested Kitty approval prompts (codexctl-ntv)`.

- [ ] **Step 14: Close Beads implementation records**

After all verification succeeds, close `codexctl-1bq.1`, then `codexctl-1bq`, with reasons that include the focused tests, full workspace gates, and live nested approval result. Do not push or sync without explicit user authorization.
