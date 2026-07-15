# Nested Kitty Approval Capture

**Date:** 2026-07-15  
**Status:** Approved for review  
**Tracking:** `codexctl-ntv`

## Problem

When Codex runs beneath Neovim in Kitty, the Codex process is a descendant of
the shell that Kitty owns. Kitty's `pid:` matcher only recognizes that direct
window child, so capturing with the Codex PID fails. Codexctl therefore cannot
see the visible shell-permission prompt, leaves the session `Processing`, and
does not give the brain an actionable `NeedsInput` observation.

## Design

Kitty capture will first try the Codex session PID, preserving the direct-launch
fast path. If Kitty reports no matching window, capture will walk a bounded
chain of parent PIDs and try the same exact `pid:<pid>` selector for each
ancestor. It will stop after 16 candidates, at the root process, or on a cycle.
The first successful capture records the actual ancestor target in the existing
approval evidence.

Parent lookup will use `ps -o ppid= -p <pid>`, matching the project's supported
desktop environments without introducing Linux-only `/proc` parsing. Codexctl
will not fall back to cwd, title, or fuzzy window matching because those values
can be shared by multiple sessions.

Direct-call approval matching remains unchanged. A pending shell call becomes
`NeedsInput` only when the captured pane contains the matching Codex approval
prompt and exact displayed command. The brain may then apply existing policy.
Immediately before sending Enter, codexctl must recapture the same backend and
resolved Kitty target and match the same process, transcript, call ID, command,
and prompt fingerprint. Any mismatch or capture failure cancels the action.

### Functions Exec Wrappers

Live verification showed that current Codex records a `functions.exec` request
as one `custom_tool_call` named `exec`. Its transcript input is the JavaScript
wrapper, while the nested `tools.exec_command` call and the command waiting for
permission are not emitted as separate transcript events.

For a pending `exec` wrapper whose input contains a `tools.exec_command(`
invocation, codexctl will treat the last complete visible Codex approval block
as the authoritative nested command. It will scan complete prompt blocks from
the bottom of the captured pane, require the question, ordered choices,
confirmation text, and a displayed `$ ` command from the same block, then bind
that command to the outer call ID, process, transcript, backend, resolved Kitty
target, prompt version, and block fingerprint.

Once confirmed, the session's pending tool identity becomes `exec_command` and
its pending input becomes the displayed command so static rules and the brain
evaluate what the user would approve rather than the JavaScript wrapper. Direct
`exec_command`, `shell`, and `Bash` transcript calls retain exact equality
between transcript input and displayed command. An `exec` wrapper without a
`tools.exec_command(` invocation remains non-actionable.

Immediately before Enter, codexctl recaptures the same pane and requires the
same last complete prompt evidence. Earlier lookalike prompts cannot override a
newer real prompt, and a changed or disappeared final block cancels the action.

## Failure Handling

Failure to find a Kitty window through the bounded ancestor chain remains an
unknown, non-actionable observation. Invalid parent output, a zero parent, a
self-parent, or a cycle stops traversal. A successful capture from an ancestor
does not weaken prompt matching or authorize input by itself.

## Verification

Focused tests will cover:

- direct Codex PID capture succeeds without parent lookup;
- nested capture tries ancestors in order and records the successful target;
- traversal stops at a missing parent, root, cycle, or the candidate bound;
- no matching window remains non-actionable;
- existing stale-prompt revalidation continues to reject changed evidence.
- a current `exec` wrapper with `tools.exec_command(` uses the last complete
  visible prompt command and becomes `NeedsInput`;
- wrapper calls without a nested shell invocation remain non-actionable;
- an earlier lookalike block cannot override the final approval block;
- confirmed wrapper command/tool identity is what rules and the brain receive.

Completion also requires `cargo test`, `cargo fmt --check`,
`cargo clippy -- -D warnings`, and a live nested-Neovim approval check after the
dashboard is restarted with the rebuilt binary.
