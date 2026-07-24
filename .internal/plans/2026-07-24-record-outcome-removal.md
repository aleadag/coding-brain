# Record-Outcome Pipeline Removal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:executing-plans to implement this plan task-by-task. Each Task is tracked in Beads; steps use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Remove the obsolete record/reap/report outcome pipeline and dormant heuristic test-failure learning while leaving active lifecycle activity behavior unchanged.

**Architecture:** Perform the tightly coupled code removal as one atomic task: add three surviving-contract regressions, remove configuration and learning semantics, then remove the CLI/reaper/report module and its forced distillation path. A second task synchronizes documentation and runs release-quality verification, including protected-path and metadata-only legacy-state checks.

**Tech Stack:** Rust 2024, Clap 4, Cargo workspace tests, project `direnv`/Nix development environment, Beads.

## Global Constraints

- Fast-forward this worktree to the already-fetched `origin/main` before editing; stop if the update is not a clean fast-forward or newly touches target files in a way that invalidates the plan.
- Do not edit provider adapters, `src/lifecycle_hook.rs`, the activity schema/store, Doctor outcome telemetry, or TUI outcome rendering.
- Do not read legacy file contents or move, rewrite, migrate, archive, or delete legacy state files.
- Do not add a compatibility alias, importer, migration marker, cleanup command, or replacement detailed report.
- Keep `--tool`, `--tool-input`, and `--project`; remove `--top` after confirming the deleted reports remain its only consumers.
- Keep a targeted removed-key diagnostic for `[brain].test_runners`; ordinary loading must not restore the removed behavior.
- Execute inline with `beads-superpowers:executing-plans`; the tasks are sequential and do not authorize subagent delegation.
- Do not commit, push, publish, or sync Beads remotely unless the user explicitly authorizes it. Checkpoint steps record proposed commit messages only.

---

### Task 1: Remove the complete legacy outcome pipeline and heuristic learning

**Files:**
- Modify: `crates/coding-brain-core/src/config.rs`
- Modify: `src/config.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/preferences.rs`
- Modify: `src/brain/metrics.rs`
- Modify: `src/main.rs`
- Modify: `src/commands.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/brain/distill.rs`
- Delete: `src/brain/outcomes.rs`
- Modify: `tests/integration_tests.rs`

**Interfaces:**
- Consumes: Existing `ConfigWarning`, `removed_key_message`, `DecisionOutcome`, `DecisionSummary`, `compute_counterfactuals`, root `Cli`, non-interactive command dispatch, `run_headless`, and `brain::distill::run_once`.
- Produces: `BrainConfig` without `test_runners`; `DecisionOutcome::{Success, Error}` only; a targeted removed-key warning; a CLI with no legacy outcome flags; a normal-only distiller; no `brain::outcomes` module.

**Acceptance Criteria:**
- The worktree is fast-forwarded and active hook/Doctor outcome tests pass before editing.
- `[brain].test_runners` is no longer parsed, merged, templated, or stored; validation emits the targeted removal warning.
- No code reads or writes `test-failures/*.json`; `DecisionOutcome::TestFailed` and all projection, weighting, and counterfactual handling are absent.
- All ten removal-only flags are absent from long help and rejected with representative complete arguments; shared tool/project flags remain.
- Headless execution never scans legacy outcome directories.
- `run_after_outcome_change`, its force branch, legacy reaper tests, and the outcomes module are absent.
- Active lifecycle outcome, Doctor telemetry, activity, provider, and TUI code has no task diff.

- [ ] **Step 1: Fast-forward and revalidate scope**

Run:

```bash
git status --short --branch
git merge --ff-only origin/main
git status --short --branch
rg -n 'record_outcome|reap_outcomes|brain_outcomes|brain_baseline|test_runners|TestFailed|run_after_outcome_change' src crates tests README.md docs CHANGELOG.md
```

Expected: the merge fast-forwards cleanly; the three untracked research/spec/plan files remain; target references still match the plan. If the merge is not a fast-forward or changes a target contract, stop and revise the plan before editing.

- [ ] **Step 2: Record metadata-only state counts and active-path baseline**

Run without opening any state file:

```bash
legacy_state_root="${XDG_STATE_HOME:-$HOME/.local/state}/coding-brain/brain"
for legacy_dir in pending-outcomes outcomes outcomes-orphaned test-failures; do
  if [ -d "$legacy_state_root/$legacy_dir" ]; then
    find "$legacy_state_root/$legacy_dir" -maxdepth 1 -type f | wc -l
  else
    echo 0
  fi
done
direnv exec . cargo test --test hook_activity
direnv exec . cargo test doctor::tests::outcome_telemetry
```

Expected: record the four counts in order in the Task 1 Beads note; both test commands PASS before implementation.

- [ ] **Step 3: Add the failing removed-config regression**

In `src/config.rs`, replace the parsing assertions for `test_runners` with:

```rust
#[test]
fn test_runners_is_explicitly_unsupported() {
    use std::io::Write;

    let mut file = tempfile::NamedTempFile::new().unwrap();
    writeln!(file, "[brain]\ntest_runners = [\"cargo test\"]").unwrap();
    file.flush().unwrap();

    let (warnings, has_errors) = validate_config_file(&file.path().to_path_buf());
    assert!(!has_errors);
    assert_eq!(warnings.len(), 1);
    assert_eq!(
        warnings[0].message,
        "legacy heuristic test-failure attribution was removed; delete this setting"
    );
    assert!(!Config::template_string().contains("test_runners"));
}
```

- [ ] **Step 4: Add the failing stale-counterfactual regression**

In the `src/brain/metrics.rs` test module, add:

```rust
#[test]
fn counterfactuals_ignore_removed_test_failed_kind() {
    let mut disagreement =
        coding_brain_core::runtime::DecisionSummary::from(&make_decision("reject"));
    disagreement.action = "deny".into();
    disagreement.pid = 7;

    let mut stale_marker =
        coding_brain_core::runtime::DecisionSummary::from(&make_decision("accept"));
    stale_marker.pid = 7;
    stale_marker.outcome_kind = Some("test_failed".into());
    stale_marker.outcome_detail = Some("cargo test".into());

    assert!(compute_counterfactuals(&[disagreement, stale_marker]).is_empty());
}
```

- [ ] **Step 5: Add the failing CLI-removal regression**

Move these flags from `RETAINED_ARGS` to `REMOVED_ARGS` in `src/main.rs`:

```rust
"--record-outcome",
"--exit-code",
"--duration-ms",
"--stderr-tail",
"--session-id",
"--tool-use-id",
"--reap-outcomes",
"--brain-outcomes",
"--brain-baseline",
"--top",
```

Add:

```rust
#[test]
fn removed_outcome_pipeline_arguments_are_rejected_with_values() {
    for args in [
        vec!["coding-brain", "--record-outcome"],
        vec!["coding-brain", "--exit-code", "0"],
        vec!["coding-brain", "--duration-ms", "5"],
        vec!["coding-brain", "--stderr-tail", "failure"],
        vec!["coding-brain", "--session-id", "session-1"],
        vec!["coding-brain", "--tool-use-id", "call-1"],
        vec!["coding-brain", "--reap-outcomes"],
        vec!["coding-brain", "--brain-outcomes"],
        vec!["coding-brain", "--brain-baseline"],
        vec!["coding-brain", "--top", "10"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
}
```

- [ ] **Step 6: Run all three regressions and confirm red**

Run:

```bash
direnv exec . cargo test test_runners_is_explicitly_unsupported
direnv exec . cargo test counterfactuals_ignore_removed_test_failed_kind
direnv exec . cargo test removed_outcome_pipeline_arguments_are_rejected_with_values
```

Expected: all three commands FAIL for the asserted old behavior, not for compilation or fixture setup.

- [ ] **Step 7: Remove test-runner configuration**

Change `BrainConfig` to:

```rust
pub struct BrainConfig {
    pub enabled: bool,
    #[doc(hidden)]
    pub legacy_mode_configured: bool,
    pub endpoint: String,
    pub model: String,
    pub auto_mode: bool,
    pub timeout_ms: u64,
    pub max_context_tokens: u32,
    pub few_shot_count: usize,
}
```

Delete `default_test_runners`, `RawBrainConfig::test_runners`, parsing and merge branches, template output, re-exports, and obsolete test fixtures. Remove `"test_runners"` from `known_keys("brain")` and add:

```rust
("brain", "test_runners") => {
    Some("legacy heuristic test-failure attribution was removed; delete this setting")
}
```

to `removed_key_message`.

- [ ] **Step 8: Remove marker and `TestFailed` learning semantics**

Reduce the enum to:

```rust
pub enum DecisionOutcome {
    Success,
    Error(String),
}
```

Reduce preference weighting to:

```rust
let weight = match (&d.outcome, d.is_positive()) {
    (Some(DecisionOutcome::Error(_)), true) => 0.3,
    (Some(DecisionOutcome::Error(_)), false) => 1.5,
    _ => 1.0,
};
```

Delete the marker overlay from `backfill_outcomes`, all `TestFailed` preference tests, `test_failed` projection arms, and `test_failed` counterfactual prose and match branch. Do not map stale markers into `Error`.

- [ ] **Step 9: Remove CLI fields, dispatch, handlers, and headless reaping**

Delete the ten `Cli` fields, four command-dispatch branches, and their help text from `src/main.rs`. Keep `tool`, `tool_input`, and `project`.

Delete `run_record_outcome`, `run_reap_outcomes`, `run_brain_outcomes`, `run_brain_baseline`, and report-only `truncate_col` from `src/commands.rs`.

Delete the `test_runners` local and replace:

```rust
let reaped = crate::brain::outcomes::reap_with_runners(&test_runners);
let distillation = if reaped.attributed > 0 || reaped.test_failures_attributed > 0 {
    crate::brain::distill::run_after_outcome_change(&paths)
} else {
    crate::brain::distill::run_once(&paths)
};
if let Err(error) = distillation {
    eprintln!("Warning: Coding Brain preference catch-up failed: {error}");
}
```

with:

```rust
if let Err(error) = crate::brain::distill::run_once(&paths) {
    eprintln!("Warning: Coding Brain preference catch-up failed: {error}");
}
```

Leave the following activity read, emit, compact, and sleep statements unchanged.

- [ ] **Step 10: Remove forced distillation and the outcomes module**

Delete `run_after_outcome_change`, `run_after_outcomes_with_inputs`, and the three outcome-rebuild tests from `src/brain/distill.rs`. Drop the final `false` argument from remaining `run_locked` calls and delete its `force` parameter.

Inside `run_locked`, delete:

```rust
let rebuild_existing = force && pending < DISTILL_INTERVAL && previous.generation_id.is_some();
```

Change the pending gate to:

```rust
if pending < DISTILL_INTERVAL {
    return Ok(DistillOutcome::NotDue { pending });
}
```

Set:

```rust
let candidates = &cursor_decisions[start..];
let generation_decisions = learning_decisions;
```

Delete the conditional candidate/scoped-learning branches, return `processed: pending`, and change `use std::collections::{HashMap, HashSet};` to `use std::collections::HashMap;`.

Delete `pub mod outcomes;`, delete `src/brain/outcomes.rs`, and remove the complete `#220`/`#238` reaper block plus outcome-only helpers/imports from `tests/integration_tests.rs`.

- [ ] **Step 11: Run focused and structural verification**

Run:

```bash
direnv exec . cargo test test_runners_is_explicitly_unsupported
direnv exec . cargo test counterfactuals_ignore_removed_test_failed_kind
direnv exec . cargo test removed_outcome_pipeline_arguments_are_rejected_with_values
direnv exec . cargo test removed_args_fail_and_retained_args_are_in_long_help
direnv exec . cargo test brain::preferences::tests
direnv exec . cargo test brain::metrics::tests
direnv exec . cargo test --test hook_activity
direnv exec . cargo test doctor::tests::outcome_telemetry
direnv exec . cargo check --workspace --all-targets
```

Expected: all commands PASS. These searches must print nothing:

```bash
rg -n 'record_outcome|reap_outcomes|brain_outcomes|brain_baseline|run_after_outcome_change|brain::outcomes' src crates tests
rg -n 'TestFailed|test_failed|test_failures|default_test_runners' src crates tests
```

- [ ] **Step 12: Verify the task boundary**

Run:

```bash
git diff --check
git diff -- src/lifecycle_hook.rs src/provider_hooks crates/coding-brain-core/src/brain_activity.rs src/doctor.rs crates/coding-brain-tui
git diff --stat
```

Expected: the protected-path diff is empty. Proposed commit description if later authorized: `💥 refactor: remove legacy outcome pipeline (codexctl-vwil)`.

---

### Task 2: Synchronize documentation and run release-quality verification

**Files:**
- Modify: `README.md`
- Modify: `docs/reference.md`
- Modify: `docs/configuration.md`
- Modify: `CHANGELOG.md`
- Verify: `.internal/specs/2026-07-24-record-outcome-removal-design.md`

**Interfaces:**
- Consumes: The removed public CLI and configuration surface from Task 1.
- Produces: Current user documentation and an explicit breaking-change record; no runtime interface.

**Acceptance Criteria:**
- Current README/reference/configuration docs contain no removed command or `test_runners` examples.
- `[Unreleased]` names every removed flag and states that legacy files remain untouched and may contain command/stderr data.
- The changelog points to `coding-brain init --purge` only as an existing broad, explicit deletion mechanism.
- Metadata-only legacy-state counts remain unchanged.
- `cargo fmt --check`, `cargo build`, `cargo test`, and Clippy pass.
- Protected active-path diff remains empty.

- [ ] **Step 1: Confirm stale documentation before editing**

Run:

```bash
rg -n -- '--record-outcome|--reap-outcomes|--brain-outcomes|--brain-baseline|--exit-code|--duration-ms|--stderr-tail|--session-id|--tool-use-id|--top' README.md docs
rg -n 'test_runners' README.md docs
```

Expected: the current baseline/outcomes and `test_runners` examples are found.

- [ ] **Step 2: Remove obsolete current documentation**

Delete the baseline example from `README.md`, the outcome/baseline command lines from `docs/reference.md`, and `test_runners` from the Brain TOML example in `docs/configuration.md`. Do not rewrite unrelated provider or activity documentation.

- [ ] **Step 3: Add the breaking changelog entry**

Under `[Unreleased]` → `Changed`, add:

```markdown
- **Breaking:** removed the unused legacy outcome pipeline and its public
  `--record-outcome`, `--reap-outcomes`, `--brain-outcomes`,
  `--brain-baseline`, `--exit-code`, `--duration-ms`, `--stderr-tail`,
  `--session-id`, `--tool-use-id`, and `--top` flags. Managed provider
  outcomes continue through `--lifecycle-hook` and the activity ledger.
  Existing legacy outcome files are left untouched under the Coding Brain
  state directory and may contain historical command or stderr data.
  `coding-brain init --purge` remains the existing broad, explicit state
  deletion mechanism.
- Removed the dormant `[brain].test_runners` heuristic and
  `DecisionOutcome::TestFailed`; configuration validation now tells users to
  delete that setting.
```

Keep historical released changelog entries unchanged.

- [ ] **Step 4: Verify documentation and removal searches**

Run:

```bash
rg -n -- '--record-outcome|--reap-outcomes|--brain-outcomes|--brain-baseline|--exit-code|--duration-ms|--stderr-tail|--session-id|--tool-use-id|--top' README.md docs src crates tests
rg -n 'test_runners|TestFailed|test_failed|test-failures' README.md docs src crates tests
```

Expected: no output except the intentional targeted `test_runners` removed-key diagnostic and its regression test.

- [ ] **Step 5: Run release-quality gates**

Run:

```bash
direnv exec . cargo fmt
direnv exec . cargo fmt --check
direnv exec . cargo build
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
```

Expected: all commands exit 0. If a known flaky test fails, rerun that exact test and record both outputs; do not weaken or skip the full gate.

- [ ] **Step 6: Repeat metadata-only state and protected-path checks**

Run without opening any state file:

```bash
legacy_state_root="${XDG_STATE_HOME:-$HOME/.local/state}/coding-brain/brain"
for legacy_dir in pending-outcomes outcomes outcomes-orphaned test-failures; do
  if [ -d "$legacy_state_root/$legacy_dir" ]; then
    find "$legacy_state_root/$legacy_dir" -maxdepth 1 -type f | wc -l
  else
    echo 0
  fi
done
git status --short
git diff --check
git diff -- src/lifecycle_hook.rs src/provider_hooks crates/coding-brain-core/src/brain_activity.rs src/doctor.rs crates/coding-brain-tui
```

Expected: the four counts exactly match Task 1 Step 2, no legacy state path appears in status, and the protected-path diff is empty.

- [ ] **Step 7: Review checkpoint**

Run:

```bash
git diff --stat
git status --short --branch
```

Expected: only approved code, test, documentation, research, spec, and plan files are changed. Proposed final commit description if later authorized: `💥 refactor: remove legacy outcome pipeline (codexctl-vwil)`.

## Stress Test Results: implementation plan

### Resolved Decisions

- **Base drift:** Fast-forward to the already-fetched `origin/main` before implementation and revalidate target references.
- **Task boundaries:** Merge the two code tasks so `src/brain/outcomes.rs` is not edited in one review unit and deleted in the next.
- **TDD validity:** Use three surviving-contract red/green tests; verify deleted internals through compilation and absence searches.
- **Distillation simplification:** Remove the forced rebuild path because its sole production caller is the legacy reaper.
- **Verification gates:** Add pre-change active-path baselines and the repository’s explicit `cargo build` gate.
- **Protected paths and data:** Compare protected source paths and metadata-only legacy-state counts before and after; never inspect contents.
- **Execution workflow:** Execute two sequential tasks inline without commits, pushes, publishing, or Beads remote sync.

### Changes Made

- Reduced the implementation graph from three tasks to two.
- Added fast-forward, scope revalidation, pre-change tests, and state-count baselines.
- Added `cargo build` and known-flake handling without weakening the full test gate.
- Made inline execution and consent boundaries explicit.
- Corrected metadata-only legacy-state checks to the actual `coding-brain/brain` directory.

### Deferred / Parking Lot

- Commit, push, publication, and Beads remote synchronization require separate user authorization.
- No legacy importer, report replacement, or cleanup command is planned.

### Confidence Assessment

- Overall: High
- Areas of concern: Unknown external callers will break immediately by design. The fast-forward gate must stop if upstream drift invalidates target assumptions.
