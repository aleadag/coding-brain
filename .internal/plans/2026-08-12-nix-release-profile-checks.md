# Nix Release-Profile Checks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the installable Nix package and all of its package checks use one Cargo release profile while retaining every existing check and ordinary debug-profile CI coverage.

**Architecture:** Let `rustPlatform.buildRustPackage` inherit its check profile from its release `buildType`, and select release explicitly only in the independent `postCheck` Cargo command. Strengthen the existing source-level workflow contract first, then prove the real derivation with forced before/after builds and keep hosted macOS acceptance distinct from local Linux readiness.

**Tech Stack:** Nix flakes, Nixpkgs `rustPlatform.buildRustPackage`, Cargo/Rust integration tests, GitHub Actions, Beads.

**Tracking:** Reuse epic `codexctl-c0wp5` and its ordered child tasks `codexctl-j0nlh`, `codexctl-ri4hx`, `codexctl-0nhhh`, and `codexctl-8ycvh`; do not create duplicate execution Beads.

## Global Constraints

- Preserve the core and TUI package selection, the two Darwin-only skips, offline target execution, serialized tests, and both `release_workflow` and `release_metadata` integration tests.
- Preserve ordinary Linux and macOS `cargo test --all-targets -- --test-threads=1` debug-profile CI coverage.
- Do not disable checks, add retries or privileges, enable network access, broaden skips, or weaken storage/security assertions.
- Define artifact reuse as one Cargo profile and dependency graph; test harnesses, `cfg(test)` variants, and additional integration binaries may still compile in that release graph.
- Build timing is contextual evidence, never a correctness threshold.
- Keep `codexctl-fwrpm` in progress until a clean hosted macOS Nix package build passes.
- Do not push, publish, open a PR, or close `codexctl-fwrpm` without explicit user authorization.

## File Structure

- Modify `.internal/specs/2026-08-12-nix-release-profile-checks-design.md` only to retain the already-approved inline stress-test findings in its existing documentation commit.
- Create `.internal/plans/2026-08-12-nix-release-profile-checks.md` as this execution contract.
- Modify `tests/release_workflow.rs` to enforce profile inheritance and scope `--release` to `postCheck` while retaining existing portability and CI contracts.
- Modify `flake.nix` to remove the debug override and select release for the custom integration-test command.
- Do not modify `.github/workflows/ci.yml`, `nix/home-manager.nix`, Cargo metadata, runtime code, or storage/security tests.

---

### Task 1: Finalize the reviewed design on the current upstream base

**Files:**
- Modify: `.internal/specs/2026-08-12-nix-release-profile-checks-design.md`
- Create: `.internal/plans/2026-08-12-nix-release-profile-checks.md`

**Interfaces:**
- Consumes: approved design commit `b376927d` and the resolved 9/9 stress-test findings.
- Produces: a clean branch rebased onto current `origin/main`, with the target files rechecked before measurement or code changes.

**Acceptance Criteria:**
- The approved inline stress-test findings are included in the design commit.
- The branch is rebased onto current `origin/main` before baseline measurement.
- `flake.nix`, `tests/release_workflow.rs`, `Cargo.toml`, `Cargo.lock`, and `.github/workflows/ci.yml` retain the behavior analyzed by the design, or execution stops for renewed design review.
- No unrelated worktree changes are staged, amended, or rebased away.

- [ ] **Step 1: Audit the exact documentation diff**

Run:

```bash
git status --short --branch
git diff -- .internal/specs/2026-08-12-nix-release-profile-checks-design.md
git diff --check
```

Expected: only the approved stress-test additions to the design and this new plan are uncommitted; whitespace checks pass.

- [ ] **Step 2: Amend only the approved design findings into the restore-point commit**

Run:

```bash
git add .internal/specs/2026-08-12-nix-release-profile-checks-design.md
git diff --cached --name-only
git commit --amend --no-edit
```

Expected: the cached file list contains only the design spec; the amended commit retains `📝 docs: design release-profile Nix checks`.

- [ ] **Step 3: Rebase onto the current upstream base**

Run:

```bash
git rebase origin/main
git log --oneline --left-right HEAD...origin/main
```

Expected: rebase completes without conflict and the left/right log contains only the local design commit on the left. If a target file conflicts or upstream now changes a target file, stop and re-run design analysis instead of resolving silently.

- [ ] **Step 4: Reconfirm the analyzed boundary after rebase**

Run:

```bash
rg -n "checkType|postCheck|cargoTestFlags|release_workflow|release_metadata" flake.nix tests/release_workflow.rs
rg -n "cargo test --all-targets -- --test-threads=1" .github/workflows/ci.yml
git status --short --branch
```

Expected: `flake.nix` still has `checkType = "debug"` and a profile-less `postCheck`; the contract still requires debug; CI still contains ordinary debug `cargo test`; only the plan file remains uncommitted.

- [ ] **Step 5: Commit the implementation plan after user approval**

Run:

```bash
git add .internal/plans/2026-08-12-nix-release-profile-checks.md
git diff --cached --check
git diff --cached --name-only
git commit -m "📝 docs: plan release-profile Nix checks"
```

Expected: the commit contains only the approved plan.

### Task 2: Capture the forced before-change package baseline

**Files:**
- Modify: none
- Evidence: `/tmp/fwrpm-before.log`, `/tmp/fwrpm-before-path`

**Interfaces:**
- Consumes: clean current-base branch with the old debug package checks.
- Produces: timestamped clean-build log, wall-clock measurement, phase markers, test-selection evidence, and the baseline package output path.

**Acceptance Criteria:**
- The baseline rebuild is forced with `--rebuild`; an existing store result or substitute is not accepted as the measurement.
- The command exits successfully and records total wall time plus build/check phase markers.
- The log proves the primary package check and custom integration tests use the debug profile before the fix.
- The baseline output path is retained for after-change output comparison.

- [ ] **Step 1: Force and time the baseline build**

Run in `zsh`:

```bash
set -o pipefail
/usr/bin/time -f 'wall_seconds=%e user_seconds=%U system_seconds=%S' \
  nix build --rebuild --no-link --print-build-logs --print-out-paths .# 2>&1 \
  | awk '{ print systime(), $0; fflush() }' \
  | tee /tmp/fwrpm-before.log
```

Expected: exit 0; the log contains `Executing cargoBuildHook`, `Executing cargoCheckHook`, `Finished cargoCheckHook`, test results, the output store path, and the `wall_seconds` line.

- [ ] **Step 2: Record the baseline output and phase evidence**

Run:

```bash
nix path-info .# > /tmp/fwrpm-before-path
rg "Executing cargo(Build|Check)Hook|Finished cargo(Build|Check)Hook|cargoCheckHook flags|cargo test|wall_seconds|test result:" /tmp/fwrpm-before.log
```

Expected: the saved path is the successfully built package; primary check flags omit `--profile release`, and the custom `cargo test` omits `--release`. Use the leading epoch seconds around hook start/finish lines to report build and check phase durations.

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

In `tests/release_workflow.rs`, replace the debug assertion in `official_release_nix_and_package_paths_remain_feature_free` with:

```rust
    assert!(
        !flake.contains("checkType"),
        "Nix package checks must inherit the release build profile"
    );
```

Remove `"checkType = \"debug\";"` from the `required` array in `nix_package_check_is_portable_and_bounded`, then add this after that required-contract loop:

```rust
    assert!(
        !flake.contains("checkType"),
        "Nix package checks must inherit the release build profile"
    );
    let post_check = flake
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

Expected: FAIL in both profile assertions because `flake.nix` still contains `checkType = "debug"`; this proves the test detects the old configuration.

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

### Task 4: Verify local readiness and obtain hosted acceptance

**Files:**
- Modify: none unless a directly related failure requires returning to Task 3.
- Evidence: `/tmp/fwrpm-after.log`, `/tmp/fwrpm-after-path`, Beads notes, and later GitHub Actions check results.

**Interfaces:**
- Consumes: the committed implementation and `/tmp/fwrpm-before-*` evidence.
- Produces: local Linux correctness/timing comparison, exact package-output comparison, and—only after separate publication authorization—hosted Linux/macOS acceptance.

**Acceptance Criteria:**
- Nix formatting/evaluation, Rust formatting, Clippy, focused contracts, and relevant workspace tests pass.
- A forced Linux package rebuild passes with primary `--profile release`, custom `--release`, retained tests, and no debug-profile command or artifact graph.
- Before/after phase and wall timings are recorded as contextual evidence.
- The before/after installed package outputs are byte-for-byte identical, preserving the optimized binary and package metadata.
- Hosted Linux and macOS Nix package jobs pass before `codexctl-fwrpm` is closed.
- No push, PR, or Bead closure occurs without explicit user authorization.

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
diff -qr "$(< /tmp/fwrpm-before-path)" "$(< /tmp/fwrpm-after-path)"
```

Expected: primary check flags contain `--profile release`; the custom command contains `--release`; core, TUI, `release_workflow`, and `release_metadata` tests execute; no debug command/artifact path appears; installed outputs are identical. Report hook and wall timings from both timestamped logs without imposing a pass/fail threshold.

- [ ] **Step 4: Record local evidence without closing the Bead**

Run against the canonical planning checkout:

```bash
before_wall=$(rg -o 'wall_seconds=[0-9.]+' /tmp/fwrpm-before.log | tail -1)
after_wall=$(rg -o 'wall_seconds=[0-9.]+' /tmp/fwrpm-after.log | tail -1)
before_build=$(awk '/Executing cargoBuildHook/{start=$1} /Finished cargoBuildHook/{print $1-start; exit}' /tmp/fwrpm-before.log)
before_check=$(awk '/Executing cargoCheckHook/{start=$1} /Finished cargoCheckHook/{print $1-start; exit}' /tmp/fwrpm-before.log)
after_build=$(awk '/Executing cargoBuildHook/{start=$1} /Finished cargoBuildHook/{print $1-start; exit}' /tmp/fwrpm-after.log)
after_check=$(awk '/Executing cargoCheckHook/{start=$1} /Finished cargoCheckHook/{print $1-start; exit}' /tmp/fwrpm-after.log)
implementation_commit=$(git rev-parse HEAD)
bd -C /home/alexander/.beads-planning note codexctl-fwrpm "Local Linux verification at ${implementation_commit}: forced nix build --rebuild passed before and after; before ${before_wall}, cargoBuild=${before_build}s, cargoCheck=${before_check}s; after ${after_wall}, cargoBuild=${after_build}s, cargoCheck=${after_check}s. After log shows primary --profile release and postCheck --release, with no debug command/artifact path. Core, TUI, release_workflow, and release_metadata tests ran. nix fmt -- --check, all-system flake evaluation, cargo fmt, clippy -D warnings, focused contracts, and serialized all-target tests passed. diff -qr of the recorded installed outputs was empty. Hosted macOS acceptance remains pending."
git status --short --branch
```

Expected: the note contains concrete evidence rather than a success claim; the branch is clean and `codexctl-fwrpm` remains in progress.

- [ ] **Step 5: Obtain explicit publication authorization**

Ask the user whether to push the branch and open a draft PR. Do not infer this authority from approval to execute local implementation.

Expected: execution pauses unless the user explicitly authorizes publication.

- [ ] **Step 6: Publish and verify hosted acceptance only when authorized**

Use the repository's GitHub publication workflow to push the rebased branch and open a draft PR. Then inspect the exact PR checks and logs, requiring at minimum:

```text
Nix (ubuntu-latest) / Build Nix package
Nix (macos-latest) / Build Nix package
```

Expected: both hosted Nix package jobs pass and their logs show release-profile package checks. Record hosted timings as contextual evidence. If a failure is unrelated, file a separate Bead; do not weaken `fwrpm` to mask it.

- [ ] **Step 7: Request closure authorization**

Present the local, hosted, Home Manager/package-output, and timing evidence to the user. Close `codexctl-fwrpm` only after explicit authorization:

```bash
bd -C /home/alexander/.beads-planning close codexctl-fwrpm --reason "Release-profile Nix checks verified on Linux and macOS; checks retained and installed package output unchanged"
```

Expected: the Bead remains open unless every acceptance criterion is evidenced and the user authorizes closure.
