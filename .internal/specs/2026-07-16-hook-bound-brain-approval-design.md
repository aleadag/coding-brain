# Hook-Bound Brain Approval Design

## Problem

Terminal capture can identify the command currently displayed by a Codex permission prompt, but it cannot prove prompt-instance identity when one prompt is replaced by a byte-identical prompt between polling ticks. Consequently, an asynchronous brain suggestion can be stale even when final terminal recapture produces equal semantic evidence.

Codex exposes a synchronous `PermissionRequest` hook for every permission request. The hook receives the canonical tool name and input and can return a native `allow` or `deny` decision before Codex displays the permission prompt. This lifecycle boundary avoids terminal input injection for the normal automatic path.

## Goals

- Automatically approve or deny confident shell permission decisions through the Codex `PermissionRequest` hook on every terminal.
- Preserve the normal Codex permission prompt when the brain is disabled, unavailable, uncertain, or abstains.
- Keep terminal detection for dashboard status, actionable command display, and explicit manual approval.
- Offer an opt-in compatibility fallback that can automatically approve through guarded Enter when codexctl's permission hook is not configured and the session is attached to Kitty or tmux.
- Preserve Brain Review records for hook decisions.
- Upgrade codexctl-managed hook entries without overwriting unrelated user hooks.

## Non-Goals

- Do not auto-reject through terminal key injection. Terminal fallback supports approval only.
- Do not claim prompt-instance safety for terminal fallback. Its limitation must be explicit in configuration and TUI copy.
- Do not replace transcript discovery, process liveness, token/cost accounting, or terminal-backed status detection.
- Do not implement the broader hook lifecycle status store tracked by `codexctl-rqm`.

## Architecture

### Native permission hook

Add a hidden `--permission-hook` command. The installed `PermissionRequest` hook invokes it directly and sends the official hook JSON on standard input. The command validates:

- `hook_event_name == "PermissionRequest"`;
- non-empty `session_id`, `turn_id`, `cwd`, and `tool_name`;
- a JSON `tool_input` value;
- a shell command from `tool_input.command` for the `Bash` hook contract.

The command must reserve standard output exclusively for a valid Codex hook response. Diagnostics go to standard error.

The hook installer uses matcher `Bash`, command `codexctl --permission-hook`, and a 30-second handler timeout. Codex matcher aliases make this apply to shell/exec permission requests independently of the user's terminal emulator.

Brain inference is clamped to 25 seconds so codexctl retains time to serialize the response before Codex's 30-second handler deadline. While the hook is running, its status message is `Brain reviewing permission…`.

### Decision pipeline

Refactor the existing standalone brain-query path so both `--brain-query` and `--permission-hook` use one brain decision function. The input contains project, tool, command, and optional hook identity. The output contains action, message, reasoning, confidence, source, threshold, and whether the result is below threshold.

Codex native exec-policy rules remain the sole static shell policy. Codex evaluates those rules before requesting permission; the hook reviews only requests Codex has chosen to prompt. The permission hook and terminal fallback must not evaluate codexctl `AutoRule` approve/deny entries as a second shell policy. Existing codexctl rules remain available for dashboard and orchestration automation outside this hook path.

The decision function queries the configured brain and applies its adaptive threshold.

The permission hook emits a native decision only when:

- the action is `approve` or `deny`; and
- brain confidence meets the adaptive threshold.

The exact output envelope is:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PermissionRequest",
    "decision": {
      "behavior": "allow",
      "message": "reason"
    }
  }
}
```

`behavior` is `allow` for approve and `deny` for deny.

Malformed input, disabled brain, inference failure, abstention, unsupported actions, and below-threshold results exit successfully without a hook decision. Codex then presents its normal permission prompt. Hook failures must never silently become approval.

### Brain Review logging

Every emitted hook allow/deny is logged once with:

- project derived from `cwd`;
- exact tool and command;
- brain action, confidence, reasoning, and source;
- `user_action = "hook_allow"` for allow and `user_action = "hook_deny"` for deny;
- hook `session_id` and `turn_id` as optional record metadata.

The complete record is persisted before the hook response is written to standard output. If persistence or response serialization fails, the hook emits no decision and leaves the normal prompt to Codex. The record means that a decision was prepared, not that tool execution was later confirmed.

No Brain Review record is written for malformed payloads or results that fall through to the user prompt. Hook records are serialized completely before a single append operation so concurrent sessions do not interleave partial JSONL records. Tests must use a temporary `HOME` so they cannot contaminate the live review store.

### Hook installation and detection

Initialization replaces only codexctl-managed hook handlers, then adds the current definitions. Non-codexctl handlers and unrelated settings remain structurally unchanged. Re-running initialization is idempotent and upgrades the old `codexctl --json` permission handler to `codexctl --permission-hook`.

The runtime detects whether the current codexctl `PermissionRequest` handler is configured in the supported global or project `hooks.json` locations. Configured but disabled, stale, or untrusted hooks are treated as configured: the safe behavior is to leave the normal prompt visible instead of guessing that fallback is permitted. Initialization tells the user to restart Codex and inspect `/hooks` when a newly installed or changed command requires trust.

No semantic deduplication is performed. The official event has no unique permission-request identifier, so byte-identical sequential payloads are independent requests and must each be evaluated. Initialization is idempotent within each `hooks.json`; `doctor` warns when the codexctl handler is installed in both global and applicable project scope, where Codex could invoke both copies.

### Optional terminal fallback

Add this configuration with a CLI override and normal TOML merge support:

```toml
[brain]
terminal_auto_approve_fallback = false
```

The default is `false`.

When `true`, asynchronous brain auto mode may approve a visible, terminal-confirmed shell prompt only if `[brain] auto = true` is also enabled and the current codexctl `PermissionRequest` hook is not configured. Existing guarded recapture and exact semantic evidence comparison remain mandatory. The fallback supports only Kitty and tmux, matching the existing guarded terminal backends.

The fallback never converts a brain deny into terminal input. It also never runs when a codexctl permission hook is configured, including when that hook abstained, timed out, is disabled, or has not been trusted.

The TUI labels this as an unsafe compatibility fallback because identical repainted prompt instances cannot be distinguished. Explicit manual TUI approval remains available through the existing guarded path.

### TUI decision notice

Completed brain decisions use a dedicated notice instead of the generic one-tick status message. The notice uses compact copy such as `Brain allowed Bash — safe read-only command` or `Brain denied Bash — destructive command` and remains readable for 10 seconds.

A newer brain decision replaces the current notice. Immediate keyboard or action feedback may temporarily take precedence; after that transient status clears, the brain notice reappears for the remainder of its original lifetime. Expiry does not remove the durable Brain Review record.

The TUI observes both in-process asynchronous decisions and new `hook_allow`/`hook_deny` review records produced by the separate permission-hook process. It tracks the newest displayed decision identity so refreshes do not repeatedly restart the timer or replay old history.

## Data Flow

### Hook configured

1. Codex raises one `PermissionRequest` event.
2. Codex runs `codexctl --permission-hook` with the event JSON.
3. codexctl evaluates the request with the brain; Codex native rules have already determined that review is required.
4. A confident allow/deny is returned directly to Codex and logged.
5. Otherwise no decision is returned and Codex displays its normal prompt.
6. The asynchronous dashboard brain may advise on the visible prompt, but it cannot automatically send Enter while the hook is configured.

### Hook not configured

1. Codex displays its normal permission prompt.
2. Terminal detection projects the displayed command into status, rules, brain context, and TUI observations.
3. With fallback disabled, approval requires an explicit user action.
4. With fallback and brain auto mode enabled, a confident asynchronous brain approve may use guarded Enter on Kitty/tmux. A deny remains advisory/manual.

## Error Handling and Safety

- Invalid hook input: diagnostic on standard error, successful no-decision exit.
- Brain endpoint failure or timeout: successful no-decision exit.
- Hook handler timeout or trust failure: Codex owns the failure and normal prompt; terminal fallback stays disabled because the hook remains configured.
- Hook review persistence or response serialization failure: emit no decision and preserve the normal prompt.
- Unsupported terminal in fallback mode: keep the prompt visible and report that guarded input is unavailable.
- Changed or disappeared prompt during guarded recapture: cancel input.
- Codex native exec-policy rules remain authoritative; codexctl does not add a duplicate static shell policy inside the hook or fallback.
- No automatic path sends arbitrary text or selects a terminal denial option.

## Testing

- Parser tests for valid Bash payloads and malformed/missing fields.
- Hook output tests for confident brain allow/deny mapping, below-threshold fallthrough, abstain, disabled brain, inference failure, and proof that codexctl `AutoRule` entries do not participate in hook authorization.
- Two identical sequential hook payloads are evaluated independently and never share asynchronous prompt state.
- Decision logging tests verify hook metadata, prepared-decision semantics, and parseable concurrent appends, and use temporary `HOME`.
- Hook installer tests verify idempotent upgrade and preservation of unrelated hooks.
- Hook discovery and doctor tests cover global/project scope, configured-but-disabled entries, stale commands, and duplicate-scope warnings.
- Runtime tests verify fallback defaults off, requires hook absence, supports only Kitty/tmux, preserves exact recapture, and never auto-rejects.
- Runtime tests verify fallback also requires brain auto mode and cannot reuse a prior hook or terminal decision.
- TUI tests use a controllable clock to verify the 10-second brain notice, replacement by a newer decision, temporary status precedence, expiry, and no timer restart from polling the same review record.
- Brain hook tests inject inference results rather than contacting a real model endpoint.
- Existing actionable-identity and sequential-wrapper regressions remain green.
- Full gates: `cargo fmt --check`, `cargo test`, `cargo clippy -- -D warnings`, and `cargo build`, all tests under a temporary `HOME`; live Brain Review `project == "test"` count remains unchanged.

## Acceptance Criteria

- A configured and trusted codexctl permission hook can automatically allow or deny shell permission requests regardless of terminal emulator.
- A configured hook's abstention, uncertainty, failure, or disabled brain leaves the normal Codex prompt visible.
- An unconfigured hook never causes automatic terminal input unless both `[brain] auto = true` and `terminal_auto_approve_fallback = true`.
- Terminal fallback is limited to Kitty/tmux guarded approval, never auto-rejects, and is visibly labeled as an unsafe compatibility mode.
- Automatic dashboard brain approval cannot send Enter while the codexctl permission hook is configured.
- Hook decisions appear once in Brain Review with exact tool/command and hook identity metadata.
- Completed brain decisions remain visible in the TUI for 10 seconds and remain durably available in Brain Review afterward.
- Existing user hooks are preserved during installation upgrade.
- All focused regressions and full quality gates pass without adding live test review records.

## Stress Test Results

The adversarial review resolved these design branches:

1. **Configured-hook detection:** fallback is disabled by the presence of the managed handler, including disabled, stale, or untrusted entries.
2. **Output and audit ordering:** persist the prepared decision before emitting the response; any failure falls through safely.
3. **Static-policy ownership:** Codex native exec-policy rules are the only static shell authorization policy; codexctl rules do not participate in hook or fallback authorization.
4. **Upgrade and trust lifecycle:** initialization upgrades only managed handlers and instructs users to restart Codex and verify trust through `/hooks`.
5. **Duplicate and sequential delivery:** do not deduplicate identical events; make installation idempotent per file and warn about global/project duplicates.
6. **Deadline and record truthfulness:** use a 30-second handler deadline, a 25-second inference ceiling, and `hook_allow`/`hook_deny` records that mean prepared rather than execution-confirmed.
7. **Fallback consent:** require both brain auto mode and the explicit unsafe fallback option.
8. **Concurrency and test isolation:** do not cache decisions across requests or sessions; inject inference in tests and isolate persistent state under a temporary `HOME`.
9. **Readable TUI feedback:** retain completed brain decisions in a dedicated 10-second notice without changing the lifecycle of unrelated status messages.

Confidence after the stress test is high. The remaining terminal fallback risk is inherent and explicitly opt-in: a captured terminal cannot distinguish byte-identical sequential prompt instances. The native hook path removes that risk for configured installations.
