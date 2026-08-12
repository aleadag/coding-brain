# Nix Release-Profile Checks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the installable Nix package and all of its package checks use one Cargo release profile while retaining every existing check and ordinary debug-profile CI coverage.

**Architecture:** Let `rustPlatform.buildRustPackage` inherit its check profile from its release `buildType`, and select release explicitly only in the independent `postCheck` Cargo command. Strengthen the existing source-level workflow contract first, then prove the real derivation with forced before/after builds and keep hosted macOS acceptance distinct from local Linux readiness.

**Tech Stack:** Nix flakes, Nixpkgs `rustPlatform.buildRustPackage`, Cargo/Rust integration tests, GitHub Actions, Beads.

**Tracking:** Reuse epic `codexctl-c0wp5` and its ordered child tasks
`codexctl-j0nlh`, `codexctl-ri4hx`, `codexctl-0nhhh`, `codexctl-8ycvh`, and
hosted-acceptance task `codexctl-c0wp5.1`; do not create
duplicate execution Beads.

## Global Constraints

- Preserve the core and TUI package selection, the two Darwin-only skips, offline target execution, serialized tests, and both `release_workflow` and `release_metadata` integration tests.
- Preserve ordinary Linux and macOS `cargo test --all-targets -- --test-threads=1` debug-profile CI coverage.
- Do not disable checks, add retries or privileges, enable network access, broaden skips, or weaken storage/security assertions.
- Define artifact reuse as one Cargo profile and dependency graph; test harnesses, `cfg(test)` variants, and additional integration binaries may still compile in that release graph.
- Build timing is contextual evidence, never a correctness threshold.
- Keep `codexctl-fwrpm` in progress until a clean hosted macOS Nix package build passes.
- Do not push, publish, open a PR, or close `codexctl-fwrpm` without explicit user authorization.

## File Structure

- Do not modify the committed design or plan during implementation.
- Modify `tests/release_workflow.rs` to enforce profile inheritance and scope `--release` to `postCheck` while retaining existing portability and CI contracts.
- Modify `flake.nix` to remove the debug override and select release for the custom integration-test command.
- Do not modify `.github/workflows/ci.yml`, `nix/home-manager.nix`, Cargo metadata, runtime code, or storage/security tests.

---

### Task 1: Finalize the reviewed design on the current upstream base

**Files:**
- Modify: none

**Interfaces:**
- Consumes: the clean committed design and plan, including both completed stress tests.
- Produces: a clean branch rebased onto current `origin/main`, with the target files rechecked before measurement or code changes.

**Acceptance Criteria:**
- The approved spec, plan, and both stress-test findings are committed before execution.
- The branch is rebased onto current `origin/main` before baseline measurement.
- `flake.nix`, `tests/release_workflow.rs`, `Cargo.toml`, `Cargo.lock`, and `.github/workflows/ci.yml` retain the behavior analyzed by the design, or execution stops for renewed design review.
- No unrelated worktree changes are staged, amended, or rebased away.

- [ ] **Step 1: Audit the clean planning stack**

Run:

```bash
git status --short --branch
git log --oneline --decorate --max-count=4
git diff --name-only origin/main...HEAD
```

Expected: the worktree is clean and the local stack contains only the approved
planning documentation; implementation target files are unchanged.

- [ ] **Step 2: Rebase onto the current upstream base**

Run:

```bash
git rebase origin/main
git log --oneline --left-right HEAD...origin/main
```

Expected: rebase completes without conflict and the left/right log contains only
the local planning commits on the left. If a target file conflicts or upstream
now changes a target file, stop and re-run design analysis instead of resolving
silently.

- [ ] **Step 3: Reconfirm the analyzed boundary after rebase**

Run:

```bash
rg -n "checkType|postCheck|cargoTestFlags|release_workflow|release_metadata" flake.nix tests/release_workflow.rs
rg -n "cargo test --all-targets -- --test-threads=1" .github/workflows/ci.yml
git status --short --branch
```

Expected: `flake.nix` still has `checkType = "debug"` and a profile-less
`postCheck`; the contract still requires debug; CI still contains ordinary
debug `cargo test`; the worktree remains clean.

### Task 2: Capture the forced before-change package baseline

**Files:**
- Modify: none
- Evidence: `/tmp/fwrpm-before.log`, `/tmp/fwrpm-before-path`

**Interfaces:**
- Consumes: clean current-base branch with the old debug package checks.
- Produces: timestamped forced-rebuild log, wall-clock measurement, phase markers, test-selection evidence, and a copied baseline package output.

**Acceptance Criteria:**
- The exact baseline derivation is first realized if absent, then the measured rebuild is forced with `--rebuild`; the realization run is not timing evidence.
- The command exits successfully and records total wall time plus Nix-reported build/check phase durations.
- The log proves the primary package check and custom integration tests use the debug profile before the fix.
- The baseline output is copied outside the Nix store so garbage collection cannot remove the comparison input.

- [ ] **Step 1: Realize the exact baseline derivation**

Run:

```bash
nix build --no-link --print-build-logs .#
```

Expected: exit 0 and the exact current derivation has a valid store output.
This setup run may build or substitute the package and is not used as timing
evidence; it exists because `--rebuild` rejects an unrealized derivation.

- [ ] **Step 2: Force and time the baseline build**

Run in `zsh`:

```bash
set -o pipefail
/usr/bin/time -f 'wall_seconds=%e user_seconds=%U system_seconds=%S' \
  nix build --rebuild --no-link --print-build-logs --print-out-paths .# 2>&1 \
  | awk '{ print systime(), $0; fflush() }' \
  | tee /tmp/fwrpm-before.log
```

Expected: exit 0; the forced top-level package rebuild may reuse warm store
dependencies. The log contains `Executing cargoBuildHook`, `Executing
cargoCheckHook`, `Finished cargoCheckHook`, test results, the output store path,
and the `wall_seconds` line.

- [ ] **Step 3: Record the baseline output and phase evidence**

Run:

```bash
nix path-info .# > /tmp/fwrpm-before-path
baseline_copy=$(mktemp -d /tmp/fwrpm-before-output.XXXXXXXXXX)
cp -a "$(< /tmp/fwrpm-before-path)/." "${baseline_copy}/"
printf '%s\n' "${baseline_copy}" > /tmp/fwrpm-before-copy-path
rg "Executing cargo(Build|Check)Hook|Finished cargo(Build|Check)Hook|cargoCheckHook flags|cargo test|wall_seconds|test result:" /tmp/fwrpm-before.log
```

Expected: the saved path and copied output represent the successfully built
package; primary check flags omit `--profile release`, and the custom `cargo
test` omits `--release`. Use Nix's `buildPhase completed in ...` and `checkPhase
completed in ...` messages for contextual phase durations because Nix may flush
builder output as one batch. If the baseline fails, stop and diagnose it before
implementation.

- [ ] **Step 4: Record baseline evidence before changing code**

Run:

```bash
before_wall=$(rg -o 'wall_seconds=[0-9.]+' /tmp/fwrpm-before.log | tail -1)
before_build=$(sed -n 's/^.*buildPhase completed in //p' /tmp/fwrpm-before.log | tail -1)
before_check=$(sed -n 's/^.*checkPhase completed in //p' /tmp/fwrpm-before.log | tail -1)
baseline_copy=$(< /tmp/fwrpm-before-copy-path)
bd -C /home/alexander/.beads-planning note codexctl-fwrpm "Forced before-change package rebuild passed: ${before_wall}, buildPhase=${before_build}, checkPhase=${before_check}. Primary and custom checks used the debug profile. Baseline installed output copied to ${baseline_copy}."
```

Expected: the Bead contains baseline evidence before any implementation edit.

### Task 3: Enforce and implement one release-profile check graph

**Files:**
- Modify: `tests/release_workflow.rs`
- Modify: `flake.nix`

**Interfaces:**
- Consumes: the existing static release-workflow contracts and Nix package expression.
- Produces: a regression contract that forbids a `checkType` override and requires `--release` within `postCheck`, plus the minimal Nix configuration satisfying it.

**Acceptance Criteria:**
- The regression test fails against the old Nix expression for exactly the profile mismatch.
- `flake.nix` contains no explicit `checkType` override.
- The primary Nix check inherits release from `buildType`; the custom `postCheck` contains `--release`.
- Existing package selection, Darwin skips, offline target, serialization, integration tests, feature-free release boundary, and debug CI contract remain asserted.
- The focused release-workflow test passes after the minimal implementation.

- [ ] **Step 1: Write the structural failing contract**

In `official_release_nix_and_package_paths_remain_feature_free`, remove only the
old `assert_contract(flake, "checkType = \"debug\"");` line. In
`nix_package_check_is_portable_and_bounded`, remove
`"checkType = \"debug\";"` from the `required` array, then add this after that
required-contract loop:

```rust
    let package = flake
        .split_once("package = pkgs.rustPlatform.buildRustPackage {")
        .expect("Nix must retain the installable Rust package")
        .1
        .split_once("\n        };")
        .expect("Nix Rust package must remain a bounded attribute set")
        .0;
    assert!(
        !package.contains("checkType"),
        "Nix package checks must inherit the release build profile"
    );
    let post_check = package
        .split_once("postCheck = ''")
        .expect("Nix package checks must retain the custom integration post-check")
        .1
        .split_once("'';")
        .expect("Nix package post-check must remain a bounded shell block")
        .0;
    assert_contract(post_check, "cargo test");
    assert_contract(post_check, "--release");
```

Do not remove any other required or forbidden assertion.

- [ ] **Step 2: Run the contract and prove it fails before implementation**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow
```

Expected: FAIL in `nix_package_check_is_portable_and_bounded` because the bounded
package block still contains `checkType = "debug"`; this proves the owning test
detects the old configuration.

- [ ] **Step 3: Apply the minimal Nix fix**

In `flake.nix`, delete:

```nix
          checkType = "debug";
```

Change only the custom command prefix to:

```nix
          postCheck = ''
            cargo test \
              --release \
              --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
```

Leave the remaining flags and package expression unchanged.

- [ ] **Step 4: Run the focused contract and prove it passes**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow
```

Expected: all `release_workflow` tests pass.

- [ ] **Step 5: Format and inspect the surgical diff**

Run:

```bash
nix fmt -- flake.nix
nix develop path:. --command cargo fmt --all
git diff --check
git diff -- flake.nix tests/release_workflow.rs
```

Expected: only the approved profile contract and two-line Nix behavior change appear; no adjacent reformatting or workflow changes.

- [ ] **Step 6: Commit the tested implementation**

Run:

```bash
git add flake.nix tests/release_workflow.rs
git diff --cached --check
git diff --cached --name-only
git commit -m "⚡️ perf: reuse release profile in Nix checks (codexctl-fwrpm)"
```

Expected: the commit contains exactly the Nix expression and its contract test.

### Task 4: Verify local Linux readiness

**Files:**
- Modify: none unless a directly related failure requires returning to Task 3.
- Evidence: `/tmp/fwrpm-after.log`, `/tmp/fwrpm-after-path`, and Beads notes.

**Interfaces:**
- Consumes: the committed implementation and `/tmp/fwrpm-before-*` evidence.
- Produces: local Linux correctness/timing comparison and exact package-output comparison.

**Acceptance Criteria:**
- Nix formatting/evaluation, Rust formatting, Clippy, focused contracts, and relevant workspace tests pass.
- A forced Linux package rebuild passes with primary `--profile release`, custom `--release`, retained tests, and no debug-profile command or artifact graph.
- Before/after phase and wall timings are recorded as contextual evidence.
- The before/after installed package outputs are byte-for-byte identical, preserving the optimized binary and package metadata.
- Local evidence is recorded without claiming hosted acceptance or closing `codexctl-fwrpm`.

- [ ] **Step 1: Run static and workspace quality gates**

Run:

```bash
nix fmt -- --check
nix flake check --all-systems --no-build
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo test --test release_workflow
nix develop path:. --command cargo test --all-targets -- --test-threads=1
```

Expected: every command exits 0. The ordinary all-targets command remains debug-profile coverage outside the Nix derivation.

- [ ] **Step 2: Force and time the after-change package build**

Run in `zsh`:

```bash
set -o pipefail
/usr/bin/time -f 'wall_seconds=%e user_seconds=%U system_seconds=%S' \
  nix build --rebuild --no-link --print-build-logs --print-out-paths .# 2>&1 \
  | awk '{ print systime(), $0; fflush() }' \
  | tee /tmp/fwrpm-after.log
nix path-info .# > /tmp/fwrpm-after-path
```

Expected: exit 0 and the successfully built after-change output path is recorded.

- [ ] **Step 3: Prove profile selection, retained tests, and output compatibility**

Run:

```bash
rg "Executing cargo(Build|Check)Hook|Finished cargo(Build|Check)Hook|cargoCheckHook flags|cargo test|wall_seconds|test result:" /tmp/fwrpm-after.log
rg -- "--profile release|--release" /tmp/fwrpm-after.log
if rg -- "--profile debug|target/.*/debug" /tmp/fwrpm-after.log; then exit 1; fi
diff -qr "$(< /tmp/fwrpm-before-copy-path)" "$(< /tmp/fwrpm-after-path)"
```

Expected: primary check flags contain `--profile release`; the custom command
contains `--release`; core, TUI, `release_workflow`, and `release_metadata` tests
execute; no debug command/artifact path appears; installed outputs are
identical. Report Nix phase and wall timings from both logs without imposing a
pass/fail threshold.

- [ ] **Step 4: Record local evidence without closing the Bead**

Run against the canonical planning checkout:

```bash
before_wall=$(rg -o 'wall_seconds=[0-9.]+' /tmp/fwrpm-before.log | tail -1)
after_wall=$(rg -o 'wall_seconds=[0-9.]+' /tmp/fwrpm-after.log | tail -1)
before_build=$(sed -n 's/^.*buildPhase completed in //p' /tmp/fwrpm-before.log | tail -1)
before_check=$(sed -n 's/^.*checkPhase completed in //p' /tmp/fwrpm-before.log | tail -1)
after_build=$(sed -n 's/^.*buildPhase completed in //p' /tmp/fwrpm-after.log | tail -1)
after_check=$(sed -n 's/^.*checkPhase completed in //p' /tmp/fwrpm-after.log | tail -1)
implementation_commit=$(git rev-parse HEAD)
bd -C /home/alexander/.beads-planning note codexctl-fwrpm "Local Linux verification at ${implementation_commit}: forced nix build --rebuild passed before and after; before ${before_wall}, buildPhase=${before_build}, checkPhase=${before_check}; after ${after_wall}, buildPhase=${after_build}, checkPhase=${after_check}. After log shows primary --profile release and postCheck --release, with no debug command/artifact path. Core, TUI, release_workflow, and release_metadata tests ran. nix fmt -- --check, all-system flake evaluation, cargo fmt, clippy -D warnings, focused contracts, and serialized all-target tests passed. diff -qr of the recorded installed outputs was empty. Hosted macOS acceptance remains pending."
git status --short --branch
```

Expected: the note contains concrete evidence rather than a hosted success
claim; the branch is clean and `codexctl-fwrpm` remains in progress. Task 4 can
close while hosted acceptance remains blocked on separate authority.

### Task 5: Publish and obtain hosted acceptance

**Files:**
- Modify: none unless a directly related hosted failure returns execution to Task 3.
- Evidence: draft PR, GitHub Actions job results and logs, and Beads notes.

**Interfaces:**
- Consumes: completed local Linux verification and a clean committed branch.
- Produces: hosted Linux/macOS Nix evidence and a closure recommendation.

**Acceptance Criteria:**
- Publication occurs only after explicit user authorization.
- `Nix (ubuntu-latest)` and `Nix (macos-latest)` pass.
- Each job's `Build Nix package` step log demonstrates release-profile package checks.
- `nix/home-manager.nix` remains unchanged and the local installed-output comparison is byte-identical.
- `codexctl-fwrpm` closes only after explicit user authorization.

- [ ] **Step 1: Obtain explicit publication authorization**

Ask the user whether to push the branch and open a draft PR. Do not infer this authority from approval to execute local implementation.

Expected: execution pauses unless the user explicitly authorizes publication.

- [ ] **Step 2: Publish and verify hosted acceptance only when authorized**

Use the repository's GitHub publication workflow to push the rebased branch and open a draft PR. Then inspect the exact PR checks and logs, requiring at minimum:

```text
Nix (ubuntu-latest) / Build Nix package
Nix (macos-latest) / Build Nix package
```

Expected: both hosted Nix package jobs pass. Inspect each job log rather than
relying only on aggregate status; its `Build Nix package` step shows the primary
release profile and custom `--release`. Record hosted timings as contextual
evidence. If a failure is unrelated, file a separate Bead; do not weaken
`fwrpm` to mask it.

- [ ] **Step 3: Reconfirm Home Manager/package compatibility**

Run:

```bash
git diff --exit-code origin/main...HEAD -- nix/home-manager.nix
diff -qr "$(< /tmp/fwrpm-before-copy-path)" "$(< /tmp/fwrpm-after-path)"
```

Expected: Home Manager wiring is unchanged and the installed package output is
byte-identical.

- [ ] **Step 4: Request closure authorization**

Present the local, hosted, Home Manager/package-output, and timing evidence to the user. Close `codexctl-fwrpm` only after explicit authorization:

```bash
bd -C /home/alexander/.beads-planning close codexctl-fwrpm --reason "Release-profile Nix checks verified on Linux and macOS; checks retained and installed package output unchanged"
```

Expected: the Bead remains open unless every acceptance criterion is evidenced and the user authorizes closure.

## Stress Test Results: Nix Release-Profile Checks Implementation Plan

### Resolved Decisions

- Begin execution from the committed planning stack; remove obsolete amend and
  plan-commit steps before rebasing.
- Describe `--rebuild` evidence as a forced top-level package rebuild with warm
  dependencies possible, not an empty-store clean build.
- Realize the exact package derivation before timing when its output is absent;
  Nix rejects `--rebuild` before hooks otherwise, and the realization run is not
  timing evidence.
- Use Nix's own phase-completion durations and `/usr/bin/time` totals as
  contextual measurements; external timestamps may be batch-flushed.
- Scope `checkType` and `postCheck` assertions to the bounded installable package
  expression, with one owning regression test.
- Split local verification from publication and hosted acceptance.
- Copy the baseline installed output outside the Nix store before implementation
  into a unique `mktemp` directory so garbage collection and stale-path
  collisions cannot invalidate comparison evidence.
- Keep all security, failure, publication, and closure boundaries fail-closed.

### Changes Made

- Rewrote Task 1 for the already-committed restore point.
- Tightened the structural test code and expected red signal.
- Added immediate baseline evidence capture and a durable output copy.
- Made the baseline copy collision-safe for repeated or interrupted executions.
- Added the required untimed realization precondition for the forced baseline.
- Switched phase timing extraction to Nix's emitted phase durations after
  observing batched external timestamps.
- Split the former combined verification task into local Task 4 and hosted Task 5.
- Required per-job hosted log inspection and explicit Home Manager compatibility
  checks.

### Deferred / Parking Lot

- Publication, hosted Linux/macOS acceptance, and Bead closure remain gated on
  explicit user authorization.

### Confidence Assessment

- Overall: High
- Areas of concern: Timing comparisons remain host- and store-state-dependent;
  they are useful context but not correctness gates.
