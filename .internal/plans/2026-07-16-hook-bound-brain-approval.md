# Hook-Bound Brain Approval Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each task must be tracked in Beads before code is written.

**Goal:** Make confident brain allow/deny decisions at Codex's native `PermissionRequest` lifecycle boundary, retain a double-opt-in Kitty/tmux approval fallback only when that hook is absent, and keep completed brain decisions readable in the TUI for 10 seconds.

**Architecture:** Extract the existing standalone brain query into a policy-free decision evaluator, wrap it with a strict synchronous permission-hook adapter, and persist a prepared-decision record before emitting Codex's response. Keep native Codex exec-policy rules as the only static shell policy. The asynchronous dashboard brain may inject Enter only through the existing terminal recapture guard when both fallback flags are enabled and no managed permission hook is configured. The TUI observes new decision IDs from the durable review log and renders a separate timed notice.

**Tech Stack:** Rust 2024 workspace, Clap, Serde/JSON, Cargo unit tests, Codex hooks JSON, ratatui, Beads, Jujutsu (`jj`).

**Tracking:** Beads epic `codexctl-jq1`; hook task `codexctl-jq1.1`; sequential-prompt TUI checkpoint `codexctl-5y2`; bug `codexctl-fjx`. Create any additional child tasks under `codexctl-jq1` before implementation rather than using markdown as the live tracker.

**Design:** `.internal/specs/2026-07-16-hook-bound-brain-approval-design.md`

## Global Constraints

- Preserve the current actionable-identity and guarded terminal-recapture changes in this workspace.
- Standard output from `--permission-hook` is either one valid hook envelope or empty; all diagnostics use standard error.
- A malformed payload, disabled brain, inference failure, timeout, abstention, low confidence, persistence failure, or serialization failure must leave Codex's normal prompt intact.
- Codex native exec-policy rules are the sole static shell authorization policy. Neither the permission hook nor terminal fallback evaluates codexctl `AutoRule` approve/deny rules.
- Identical sequential permission events are independent. Do not cache or deduplicate them.
- A configured managed hook disables terminal fallback even if it is disabled, stale, untrusted, timed out, or abstained.
- Terminal fallback requires both `[brain] auto = true` and `terminal_auto_approve_fallback = true`, supports Kitty/tmux approval only, and never injects a denial.
- Persist hook decisions as `hook_allow` / `hook_deny` before writing the response; the record means prepared, not execution-confirmed.
- Keep ordinary TUI status messages one-tick; only completed brain decisions receive the separate 10-second notice.
- Tests that touch brain persistence run with a temporary `HOME`; inference tests inject deterministic closures and never contact a model endpoint.
- Use jj only. Do not commit, push, or sync without explicit user authority.

## File Structure

- Add `src/brain/query.rs`: policy-free request evaluation shared by `--brain-query` and the permission hook.
- Add `src/brain/permission_hook.rs`: official payload parser, response envelope, fail-open runner, and hook-specific tests.
- Modify `src/brain/mod.rs`: expose the two focused brain modules.
- Modify `src/commands.rs`: make `run_brain_query` a thin JSON adapter over the shared evaluator.
- Modify `src/main.rs`: add hidden `--permission-hook`, fallback CLI override, early hook dispatch, and live hook-state wiring.
- Modify `src/brain/decisions.rs`: add a fallible single-append primitive and hook-decision record writer.
- Modify `src/init/hooks.rs`: install/upgrade the native permission handler and expose managed-hook discovery.
- Modify `src/init/state.rs`: keep onboarding detection aligned with upgraded managed hooks.
- Modify `src/doctor.rs`: diagnose missing/stale/duplicate global and project permission handlers.
- Modify `crates/codexctl-core/src/config.rs`: add the fallback setting with a safe default.
- Modify `src/config.rs`: parse, validate, template, and print the fallback setting.
- Modify `src/brain/engine.rs`: gate shell auto-execution on hook absence plus double opt-in and keep terminal denials advisory.
- Modify `crates/codexctl-tui/src/app.rs`: hold hook state, poll new decision IDs, and manage notice lifetime.
- Modify `crates/codexctl-tui/src/ui/status_bar.rs`: render transient status before the timed brain notice and label unsafe fallback mode.

---

### Task 1: Extract a Policy-Free Brain Decision Evaluator

**Files:**
- Add: `src/brain/query.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/commands.rs:1397`

**Acceptance Criteria:**
- `--brain-query` preserves its current JSON result shape for brain, gate-off, abstain, and error cases.
- The shared evaluator receives explicit project/tool/input and returns action, message, reasoning, confidence, source, threshold, and below-threshold state.
- Configured codexctl approve/deny rules cannot alter the result.
- Tests inject inference and never contact the configured endpoint.

- [ ] **Step 1: Add failing evaluator tests**

Add unit tests in `src/brain/query.rs` for confident approve, confident deny, low confidence, inference failure, gate off, and a config containing matching codexctl approve/deny rules. The last case must prove that only the injected brain result determines authorization.

- [ ] **Step 2: Run the focused red test**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::query::tests
```

Expected: FAIL because the shared evaluator does not exist.

- [ ] **Step 3: Implement the minimal evaluator**

Define a small `BrainDecisionRequest` and `BrainDecision` in `src/brain/query.rs`. Implement `evaluate_with(request, brain_config, gate_mode, infer)` using the existing prompt, preferences, few-shot retrieval, diff digest, and adaptive threshold logic. Do not construct a synthetic session or call `rules::evaluate`.

Keep production `evaluate` as a thin call to `brain::client::infer`; expose the closure-based form only within the crate for tests and the permission-hook adapter.

- [ ] **Step 4: Convert `run_brain_query` to an adapter**

Retain CLI argument/default handling and JSON printing in `src/commands.rs`, but delegate all decision logic to the new evaluator. Delete the static codexctl deny/approve checks from this path.

- [ ] **Step 5: Run focused tests green**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::query::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --bin codexctl commands::digest_parser_tests
```

Expected: all selected tests pass.

---

### Task 2: Implement the Native Permission Hook and Prepared-Decision Audit

**Files:**
- Add: `src/brain/permission_hook.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/brain/decisions.rs:359`
- Modify: `src/main.rs:295,829`

**Acceptance Criteria:**
- The parser requires `PermissionRequest`, non-empty session/turn/cwd/tool, `Bash`, and a string `tool_input.command`.
- Confident approve/deny maps to Codex `allow`/`deny`; every other result writes no stdout decision.
- Brain timeout is capped at 25 seconds for this path.
- Response serialization occurs before audit append; audit append succeeds before stdout write.
- Hook records contain exact project/tool/command, brain fields, `hook_allow` or `hook_deny`, and raw session/turn metadata.
- Concurrent append tests leave every JSONL line parseable.

- [ ] **Step 1: Add parser and envelope red tests**

Cover a valid Bash request plus wrong event, empty identity fields, wrong tool, missing/non-string command, approve mapping, deny mapping, low-confidence fallthrough, abstention, and injected inference failure.

- [ ] **Step 2: Run the focused red test**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::permission_hook::tests
```

Expected: FAIL because the module and hidden mode do not exist.

- [ ] **Step 3: Implement strict payload and output types**

Use Serde structs for only the official fields codexctl consumes. Have the pure handler return `Result<Option<HookDecision>, HookDiagnostic>`; malformed input and inference failures become diagnostics plus `None`, never approval.

- [ ] **Step 4: Make decision append fallible and single-buffered**

In `src/brain/decisions.rs`, extract a helper that serializes a complete JSON value plus newline, opens `decisions.jsonl` with append semantics, and performs one `write_all`. Existing best-effort logging may ignore its result; the permission-hook writer must propagate it.

Add a hook-specific writer that includes `session_id` and `turn_id` in raw JSON without expanding unrelated runtime DTOs. Serialize the hook response first, append the prepared record second, then write the already serialized response to stdout.

- [ ] **Step 5: Add hidden CLI dispatch**

Add `#[arg(long, hide = true)] permission_hook: bool` and dispatch it after configuration merge but before any ordinary output mode. Read stdin to EOF, reserve stdout for the optional envelope, and route all errors/warnings to stderr. Do not use `process::exit` for normal fallthrough.

- [ ] **Step 6: Verify hook behavior and persistence**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::permission_hook::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::decisions::tests
```

Expected: all selected tests pass; tests assert empty stdout on every fallthrough and exactly one JSON envelope after successful persistence.

---

### Task 3: Upgrade Hook Installation, Discovery, Trust Copy, and Doctor

**Files:**
- Modify: `src/init/hooks.rs`
- Modify: `src/init/state.rs`
- Modify: `src/doctor.rs`

**Acceptance Criteria:**
- The installed Bash handler is `codexctl --permission-hook`, timeout 30, status message `Brain reviewing permission…`.
- Re-running init upgrades known managed entries in place and is idempotent.
- Non-codexctl hooks and unrelated JSON keys remain structurally unchanged.
- Presence detection recognizes current, legacy, disabled, and stale managed PermissionRequest handlers in global and applicable project scope.
- Init tells users to restart Codex and verify the command through `/hooks`.
- Doctor warns when both scopes install the managed permission handler.

- [ ] **Step 1: Add installer/discovery red tests**

Extend `src/init/hooks.rs` tests with legacy upgrade, current idempotency, unrelated-handler preservation, disabled/stale presence, project-only presence, and global+project duplicate scope fixtures.

- [ ] **Step 2: Run the focused red tests**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::hooks::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib doctor::tests
```

- [ ] **Step 3: Replace only managed handlers during merge**

Identify codexctl-owned commands narrowly enough to upgrade the legacy `codexctl --json ...` permission entry and the new `--permission-hook` entry without deleting unrelated user commands. Filter managed handlers, retain non-empty matcher groups, then append the current specs once.

- [ ] **Step 4: Expose permission-hook presence by scope**

Return a small discovery result for global/project presence and `configured = global || project`. Treat handler flags and command staleness as diagnostics, not permission to enable fallback. Reuse this parser from onboarding state, main runtime wiring, and doctor.

- [ ] **Step 5: Update success and doctor copy**

Describe the PermissionRequest hook as brain allow/deny integration, print restart plus `/hooks` trust guidance after changes, and add an advisory doctor check for duplicate scope installs.

- [ ] **Step 6: Run installer/doctor tests green**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::hooks::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::state::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib doctor::tests
```

---

### Task 4: Add the Double-Opt-In Terminal Fallback

**Files:**
- Modify: `crates/codexctl-core/src/config.rs:11`
- Modify: `src/config.rs:550,757,873`
- Modify: `src/main.rs:270,594,1015`
- Modify: `src/commands.rs:908`
- Modify: `src/brain/engine.rs:119`
- Modify: `crates/codexctl-core/src/session.rs:327`
- Modify: `crates/codexctl-tui/src/app.rs:313`
- Modify: `crates/codexctl-tui/src/ui/status_bar.rs:113`

**Acceptance Criteria:**
- `terminal_auto_approve_fallback` defaults false and is available through TOML plus `--terminal-auto-approve-fallback`.
- Shell Enter injection requires auto mode, fallback true, managed hook absent, terminal-confirmed evidence, and final exact recapture.
- Hook absence is resolved freshly from each supervised session's cwd; dashboard cwd and cached startup state cannot authorize another project.
- A configured hook disables injection regardless of trust/health/abstention.
- Terminal brain deny remains pending/advisory and never selects a denial option.
- Unsupported terminal, failed/unknown capture, or recapture failure leaves the shell suggestion pending and reports the reason.
- Shell fallback bypasses codexctl static approve/deny rules; non-shell dashboard/orchestration rule behavior remains unchanged.
- The same explicit fallback behavior is wired in TUI and headless modes.
- Global and project `[brain]` tables merge field-by-field so the two opt-ins can be supplied in different layers.

- [ ] **Step 1: Add config red tests**

Test the default, TOML parse, CLI override merge, resolved config/template key, and the requirement that the fallback flag does not imply brain auto mode.

- [ ] **Step 2: Add an engine decision matrix**

Using existing fake sessions and inference injection, cover:

| Auto | Fallback | Hook configured | Brain action | Expected |
|---|---|---|---|---|
| off | on | no | approve | advisory |
| on | off | no | approve | advisory |
| on | on | yes | approve | advisory |
| on | on | no | approve | guarded terminal attempt |
| on | on | no | deny | advisory, no input |

Also add matching codexctl approve/deny rules to the shell fixtures and assert they do not alter the matrix.

Add App-level regressions proving static shell approve/deny rules are not executed before BrainEngine, and Unknown terminal capture still classifies as a shell permission request. Add a cross-project regression where only one supervised session cwd has a managed hook, plus a layered-config regression for global `auto = true` and project `terminal_auto_approve_fallback = true`.

- [ ] **Step 3: Run red tests**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-core config::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::engine::tests
```

- [ ] **Step 4: Implement configuration across all layers**

Add the core field/default, TOML parser and known-key list, generated template/comment, CLI flag, and merge logic. Do not enable brain or auto mode implicitly.

- [ ] **Step 5: Gate only shell permission auto-execution**

Pass the managed-hook discovery result into `BrainEngine`. For a terminal-confirmed shell request, skip codexctl deny-rule override checks. In auto mode, demote to a bound pending suggestion unless the double opt-in is satisfied and the hook is absent. Execute only `Approve` through the existing `rules::execute`/`approve_shell_permission` guard; retain the pending suggestion if the terminal backend or final recapture fails. Keep `Deny` advisory.

Do not change route/spawn/orchestration behavior outside the shell-permission branch.

- [ ] **Step 6: Label unsafe mode in the TUI**

Store the hook-configured state on `App`. When fallback is armed, render compact warning copy such as `Brain: auto/fallback ⚠`; when the hook is configured, do not claim fallback is active.

- [ ] **Step 7: Run focused tests green**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-core config::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-core terminals::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::engine::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib config::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui app::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui status_bar
```

---

### Task 5: Keep Completed Brain Decisions Visible for 10 Seconds

**Files:**
- Modify: `crates/codexctl-tui/src/app.rs:235,1178,1477`
- Modify: `crates/codexctl-tui/src/ui/status_bar.rs:58`
- Modify: `src/main.rs:1015`

**Acceptance Criteria:**
- Startup primes the latest decision ID without replaying old history.
- A new `auto`, `hook_allow`, or `hook_deny` record creates compact notice text and a 10-second deadline.
- Polling the same ID does not restart the deadline.
- A newer decision replaces the notice.
- Generic status text renders first; after it clears, the unexpired brain notice reappears.
- Expiry hides only the notice, not the Brain Review record.

- [ ] **Step 1: Add timed-notice red tests**

Use `MockRuntime` decision fixtures and a helper accepting an explicit `Instant` to test initial cursor priming, allow/deny copy, same-ID polling, replacement, generic status precedence, 9.9-second visibility, and 10-second expiry.

- [ ] **Step 2: Run the focused red test**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui brain_notice
```

- [ ] **Step 3: Implement dedicated notice state**

Add a private notice containing decision ID, compact message, and expiry. Prime the cursor immediately after the live runtime is installed. After each `run_auto_actions`, ask `runtime.brain.recent_decisions(1)` for a new relevant ID and update the notice once. Keep `status_msg.clear()` unchanged.

Limit displayed reasoning to a compact single line; the full reasoning remains in Brain Review.

- [ ] **Step 4: Render with the approved precedence**

In the status bar, preserve search/launch/input and generic `status_msg` precedence, then render an unexpired brain notice before filters, recording, pending suggestions, and gate mode.

- [ ] **Step 5: Run focused tests green**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui brain_notice
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui status_bar
```

---

### Task 6: Audit Sequential Safety and Run Full Gates

**Files:**
- Verify current changes in `crates/codexctl-core/src/session.rs`, `crates/codexctl-core/src/terminals/mod.rs`, `crates/codexctl-core/src/rules.rs`, `src/brain/context.rs`, `src/brain/engine.rs`, and `crates/codexctl-tui/src/app.rs`.
- Modify only files required by a failing acceptance criterion; record any scope change in Beads first.

**Acceptance Criteria:**
- Two byte-identical hook payloads invoke inference and logging twice with distinct decision IDs.
- Existing sequential wrapper tests retain raw transcript identity and guarded second capture.
- Hook-configured mode never calls terminal Enter automatically.
- Hook-absent fallback cannot run without both opt-ins.
- Live Brain Review `project == "test"` count is unchanged by the test run.
- Formatting, all tests, clippy with warnings denied, and build pass.

- [ ] **Step 1: Capture live review baseline without mutation**

Use the existing review reader or JSONL inspection to record the count of live records whose project is `test`. Do not clear or rewrite the store.

- [ ] **Step 2: Run focused sequential and hook regressions**

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-core terminals::tests::exec_wrapper
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::permission_hook::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib brain::engine::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test -p codexctl-tui brain_notice
```

- [ ] **Step 3: Run full quality gates in isolated HOME**

```bash
cargo fmt --check
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --workspace
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo clippy --workspace --all-targets -- -D warnings
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo build --workspace
```

- [ ] **Step 4: Recheck live review count and inspect the jj diff**

```bash
jj --no-pager diff --stat
jj --no-pager st
jj --no-pager log -r '@|@-' --no-graph
```

Expected: live `project == "test"` count is unchanged; the working copy remains in `🐛 fix: preserve sequential approval detection`; no unrelated files changed.

- [ ] **Step 5: Close completed Beads items and hand off**

Close only tasks whose acceptance criteria and gates passed. Report changed files, exact verification output, remaining open issues, and a proposed emoji conventional description. Do not commit, push, or sync without explicit user authority.
