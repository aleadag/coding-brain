# XA99 Split HOME Alias Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Deny recursively deleted, field-splittable aliases that resolve to trusted HOME when cwd safety cannot be proven.

**Architecture:** Keep the provider and isolated-helper boundaries unchanged. Exercise the real provider hooks with both provider-reported and subprocess cwd set to `/`, add a shell-internal `cd /` control, then restore the evaluator's resolved-HOME denial regardless of field splitting.

**Tech Stack:** Rust, Cargo tests, Nix development shell, Beads.

## Global Constraints

- Fail closed when trusted cwd and shell directory changes are not modeled.
- Do not change provider schemas, `ShellCommandInput`, or the helper protocol.
- Preserve existing root, unresolved-expansion, quoted HOME, and exact HOME behavior.
- Keep changes surgical to `src/brain/safety.rs`, `tests/hook_activity.rs`, and approved internal artifacts.
- Commit, push `fix/xa99`, and open a draft PR after verification.
- Do not merge or change versions.

---

### Task 1: Restore fail-closed resolved-HOME classification

**Files:**
- Modify: `tests/hook_activity.rs:221`
- Modify: `tests/hook_activity.rs:947`
- Modify: `src/brain/safety.rs:427`
- Modify: `src/brain/safety.rs:1728`

**Interfaces:**
- Consumes: existing `shell_permission_payload`, `run_provider_permission_hook`, `evaluate_command`, and provider response formats.
- Produces: test-only `run_provider_permission_hook_from(home: &Path, cwd: &Path, provider: &str, antigravity_event: Option<&str>, payload: &[u8]) -> Output`; restored `irreversible-home-delete` classification.

**Acceptance Criteria:**
- A field-splittable alias resolving exactly to trusted HOME denies with `irreversible-home-delete`.
- Codex, Claude, and Antigravity deny before model inference when provider-reported cwd and hook subprocess cwd are `/`.
- The same providers deny before model inference when the payload starts in temporary HOME but the shell source executes `cd /` before deletion.
- Existing quoted aliases and direct `$HOME` deletion remain denied.
- Provider schemas and the isolated-helper protocol are unchanged.

- [ ] **Step 1: Add a test helper with explicit subprocess cwd**

In `tests/hook_activity.rs`, keep existing callers unchanged by making the current helper delegate:

```rust
fn run_provider_permission_hook(
    home: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    run_provider_permission_hook_from(home, home, provider, antigravity_event, payload)
}

fn run_provider_permission_hook_from(
    home: &Path,
    cwd: &Path,
    provider: &str,
    antigravity_event: Option<&str>,
    payload: &[u8],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    command.args(["--permission-hook", "--provider", provider]);
    if let Some(event) = antigravity_event {
        command.args(["--antigravity-hook-event", event]);
    }
    let mut child = command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("PATH", isolated_path(home))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(payload).unwrap();
    child.wait_with_output().unwrap()
}
```

- [ ] **Step 2: Write the failing unit and real-provider regressions**

In `src/brain/safety.rs`, change the split-alias unit assertion so it requires the canonical denial:

```rust
let split_alias = format!("IFS=/; {assignment}; rm -rf $X");
let deny = evaluate_command(&split_alias).unwrap_or_else(|| panic!("{split_alias}"));
assert_eq!(
    deny.rule_id,
    "irreversible-home-delete",
    "{split_alias}"
);
```

In `tests/hook_activity.rs`, rename the provider test to
`home_alias_field_splitting_is_denied_before_model_inference_for_every_provider`.
Replace its body with the complete two-case regression:

```rust
for provider in [
    AgentProvider::Codex,
    AgentProvider::Claude,
    AgentProvider::Antigravity,
] {
    for case in ["reported root cwd", "shell changes cwd"] {
        let home = tempfile::tempdir().unwrap();
        let fake_model = install_model_fixture(home.path(), "approve");
        if provider == AgentProvider::Antigravity {
            seed_antigravity_invocation(home.path(), 5);
        }
        let home_text = home.path().to_str().unwrap();
        let split = home_text
            .char_indices()
            .next_back()
            .expect("temporary HOME must not be empty")
            .0;
        let assignment = format!(
            "X='{}'; X+='{}'",
            &home_text[..split],
            &home_text[split..]
        );
        let (command, command_cwd, hook_cwd) = match case {
            "reported root cwd" => (
                format!("IFS=/; {assignment}; rm -rf $X"),
                Path::new("/"),
                Path::new("/"),
            ),
            "shell changes cwd" => (
                format!("cd /; IFS=/; {assignment}; rm -rf $X"),
                home.path(),
                home.path(),
            ),
            _ => unreachable!(),
        };
        let (provider_name, event, payload) =
            shell_permission_payload(command_cwd, provider, &command, None);
        let output = run_provider_permission_hook_from(
            home.path(),
            hook_cwd,
            provider_name,
            event,
            &payload,
        );

        assert!(output.status.success(), "{provider:?}: {case}: {command}");
        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap();
        let decision = if provider == AgentProvider::Antigravity {
            response["decision"].as_str()
        } else {
            response["hookSpecificOutput"]["decision"]["behavior"].as_str()
        };
        assert_eq!(
            (decision, fake_model.request_count()),
            (Some("deny"), 0),
            "{provider:?}: {case}: {command}"
        );
        assert_safety_deny(home.path(), "irreversible-home-delete");
    }
}
```

Leave the existing quoted append-alias regression and
`literal_home_delete_denies_before_model_inference_for_every_provider` intact.

- [ ] **Step 3: Run focused tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::home_alias_classification_respects_field_splitting \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: both regressions fail because the split alias reaches
`NoDeterministicDecision`; the provider assertion reports both the model-backed
`allow` decision and request count `1`, rather than deterministic denial and
zero requests. If RED instead fails during setup, parsing, or process launch,
stop and debug the test before changing production code.

- [ ] **Step 4: Restore the minimal evaluator guard**

In `src/brain/safety.rs`, remove only the `!target.can_split_fields` condition:

```rust
if resolve_word(target, &command_assignments)
    .is_some_and(|resolved| literal_home_target(&resolved))
{
    return canonical_deny("irreversible-home-delete");
}
```

- [ ] **Step 5: Run focused tests and verify GREEN**

Repeat the two commands from Step 3.

Expected: both exit 0; all providers deny both unsafe cwd cases without model
inference. If the minimal guard removal does not make them pass, return to
root-cause analysis rather than layering on another production change.

- [ ] **Step 6: Run the prior safety corpus**

Run serially:

```bash
nix develop path:. --command cargo test safety -- --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  reopened_shell_safety_corpus_denies_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  append_assignment_bypasses_are_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  literal_home_delete_denies_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: every command exits 0.

- [ ] **Step 7: Run full verification**

Run the shared Cargo/Nix gates serially:

```bash
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --quiet -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build
git diff --check
git status --short
```

Expected: formatting, tests, Clippy, build, and diff checks exit 0. Status shows
only the two source/test files and approved internal design/plan artifacts.

- [ ] **Step 8: Audit documentation and publish a draft PR**

After Step 7 succeeds:

1. Use `beads-superpowers:document-release` to confirm no public documentation
   requires an xa99 update.
2. Close the implementation task and `codexctl-xa99` with exact verification
   evidence.
3. Inspect status and the complete diff, stage only the xa99 source, tests, and
   approved internal artifacts, then use the `commit-message` skill to create
   one atomic emoji conventional commit containing `codexctl-xa99`.
4. Push `fix/xa99` to `origin`.
5. Use `github:yeet` to open a draft PR against the remote default branch with
   root cause, behavior change, user impact, and exact validation commands.
6. Report the branch, commit, draft PR URL, validation, and tracker state.

Do not merge or change versions.

## Stress Test Results: XA99 Implementation Plan

### Resolved Decisions

- Keep the helper/test and evaluator changes in one RED-GREEN task.
- Require RED evidence to show model-backed `allow` and request count `1`, not
  merely an assertion failure.
- Set both provider-reported cwd and hook subprocess cwd to `/`; separately
  cover shell-internal `cd /`.
- Keep all config, state, and model writes in temporary HOME and never execute
  the destructive command.
- Limit production code to removing `!target.can_split_fields`.
- Reuse the existing IFS, flag, root, quoted HOME, and direct HOME corpus rather
  than duplicating permutations.
- Run focused RED/GREEN evidence and all broader gates serially in the Nix
  development shell.
- Stop and re-debug if RED fails for setup reasons or GREEN needs more than the
  single hypothesized production change.
- Keep the approving fake model to prove deterministic interception before
  inference.
- Give every provider/cwd case a fresh HOME, model fixture, invocation seed, and
  activity store so rule-ID evidence cannot leak between cases.
- Close xa99 after verification, then commit, push, and open a draft PR as
  explicitly authorized.

### Changes Made

- Combined provider decision and model request count in one assertion so RED
  output proves the intended bypass.
- Added explicit stop conditions for invalid RED and unsuccessful minimal GREEN.
- Isolated every provider/cwd case in its own temporary state.
- Replaced the uncommitted handoff with authorized commit, push, and draft PR
  publication steps.

### Deferred / Parking Lot

- Merge and version changes remain outside this plan.
- Complete cwd-aware shell-state modeling remains separate future work.

### Confidence Assessment

- Overall: High
- Areas of concern: conservative false denial is intentional until cwd and
  directory changes can be modeled soundly.
