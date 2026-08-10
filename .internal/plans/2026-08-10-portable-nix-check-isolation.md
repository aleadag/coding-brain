# Portable Nix Check Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make the default Nix package build succeed without nested user namespaces while preserving unchanged production storage validation, complete Ubuntu/macOS Cargo coverage, and an installed-package multi-user filesystem security check.

**Architecture:** The package derivation removes its Bubblewrap Cargo wrapper and runs a stable portable selection: the `coding-brain-core` and `coding-brain-tui` suites through `cargoCheckHook`, followed by the root `release_workflow` and `release_metadata` contract targets. Darwin Nix excludes only two exact tests whose loopback and invalid-byte pathname operations its sandbox rejects; Linux Nix retains them. The existing Ubuntu/macOS Cargo matrix remains the complete source-suite authority, while an `x86_64-linux` NixOS VM check runs the installed default-feature package through one positive and two negative real-filesystem scenarios.

**Tech Stack:** Rust 2024, Cargo workspaces, Nix flakes, `buildRustPackage`, `cargoCheckHook`, `pkgs.testers.runNixOSTest`, NixOS Python test driver, GitHub Actions, Beads.

**Beads:** Original epic `codexctl-5ymc2` and tasks `codexctl-5ymc2.1` through `codexctl-5ymc2.4` cover Tasks 1-4. Execution epic `codexctl-dzlb9.14.2`, discovered from brainstorming session `codexctl-dzlb9.14.1`, covers the Task 5 CI amendment.

## Global Constraints

- Do not change `src/brain/storage/security.rs`, XDG path resolution, storage ownership rules, or any production runtime path.
- Do not add a Cargo feature, hidden CLI argument, ambient test-root override, owner exception, Bubblewrap fallback, nested namespace, or extra workflow privilege.
- Keep `checkType = "debug"`, `dontUseCargoParallelTests = true`, default build features, and normal `cargoCheckHook` execution.
- Use the same package-check responsibility boundary on Linux and Darwin. Append exactly two test filters only when `pkgs.stdenv.isDarwin`; expose the VM only as `checks.x86_64-linux.storage-security-vm`.
- Keep `cargo test --all-targets -- --test-threads=1` unchanged and required on both `ubuntu-latest` and `macos-latest`.
- The VM must use the installed package, absolute XDG paths, real unprivileged users, `requiredFeatures.kvm = false`, `qemu.forceAccel = false`, a 15-minute test timeout, and no retries.
- The GitHub Nix job is a required `ubuntu-latest`/`macos-latest` matrix, asserts `sandbox = true`, builds the current-system default package on both operating systems, runs the VM only on Ubuntu, prints build logs, and has a 30-minute timeout per matrix leg.
- Do not update `flake.lock` unless the pinned Nixpkgs is proven to lack a required API.
- Use `path:.` flake references for local pre-commit verification so newly created untracked Nix files are visible without staging; committed CI uses ordinary `.#` references.
- Do not commit, push, change branch protection, or rerun downstream workflows without separate user authorization.

---

### Task 1: Replace the Bubblewrap package check with a portable Cargo boundary

**Files:**
- Modify: `tests/release_workflow.rs:104-128`
- Modify: `flake.nix:22-82`

**Interfaces:**
- Consumes: Nixpkgs `buildRustPackage`, `cargoTestFlags`, `postCheck`, `pkgs.curl` as a check-only dependency for the real webhook test, and `pkgs.stdenv.hostPlatform.rust.rustcTarget`.
- Produces: a local `package` derivation used by `packages.default` and Task 2's VM; `cargoCheckHook` covers both lower-layer crates, and `postCheck` covers `release_workflow` plus `release_metadata`.

**Acceptance Criteria:**
- `flake.nix` contains no `bubblewrap`, `bwrap`, `--unshare-user`, Cargo replacement wrapper, or UID/GID mapping environment.
- The package retains debug-profile and serialized checks with default features.
- `cargoCheckHook` runs all tests for `coding-brain-core` and `coding-brain-tui`.
- `postCheck` runs the root `release_workflow` and `release_metadata` targets offline, for the host Rust target, with one test thread.
- `nativeCheckInputs` supplies both Git and curl because retained core tests execute real Git and webhook subprocess boundaries.
- `nix build --no-link --print-build-logs path:.#packages.x86_64-linux.default` starts Cargo without Bubblewrap and succeeds.

- [ ] **Step 1: Add the failing package-boundary contract test**

Add this test after `official_release_nix_and_package_paths_remain_feature_free` in `tests/release_workflow.rs`:

```rust
#[test]
fn nix_package_check_is_portable_and_bounded() {
    let flake = include_str!("../flake.nix");

    for forbidden in [
        "bubblewrap",
        "/bin/bwrap",
        "--unshare-user",
        "nixCheckCargo",
        "CBRAIN_NIX_CHECK_UID",
        "CBRAIN_NIX_CHECK_GID",
        "--workspace",
        "--exclude",
    ] {
        assert!(
            !flake.contains(forbidden),
            "Nix package checks must not require {forbidden}"
        );
    }

    for required in [
        "checkType = \"debug\";",
        "dontUseCargoParallelTests = true;",
        "cargoTestFlags = [",
        "\"coding-brain-core\"",
        "\"coding-brain-tui\"",
        "nativeCheckInputs = [ pkgs.git pkgs.curl ];",
        "cargo test",
        "--offline",
        "--test release_workflow",
        "--test release_metadata",
        "--test-threads=1",
    ] {
        assert_contract(flake, required);
    }
}
```

- [ ] **Step 2: Run the contract test and confirm the old wrapper fails it**

Run:

```bash
cargo test --test release_workflow nix_package_check_is_portable_and_bounded -- --exact
```

Expected: FAIL at `Nix package checks must not require bubblewrap` or another forbidden wrapper token.

- [ ] **Step 3: Refactor `flake.nix` to one portable package value**

Delete `nixCheckEntrypoint`, `nixCheckCargo`, and the package's `preCheck`. Replace the inline `packages.default` derivation with this local value and output binding:

```nix
      let
        pkgs = nixpkgs.legacyPackages.${system};
        cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
        package = pkgs.rustPlatform.buildRustPackage {
          pname = "coding-brain";
          version = cargoToml.package.version;
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          checkType = "debug";
          dontUseCargoParallelTests = true;
          cargoTestFlags = [
            "-p"
            "coding-brain-core"
            "-p"
            "coding-brain-tui"
          ];
          postCheck = ''
            cargo test \
              --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
              --offline \
              --test release_workflow \
              --test release_metadata \
              -- \
              --test-threads=1
          '';
          nativeCheckInputs = [ pkgs.git pkgs.curl ];

          meta = with pkgs.lib; {
            description = "Local brain for supervising and learning from coding-agent activity.";
            homepage = "https://github.com/aleadag/coding-brain";
            license = licenses.mit;
            mainProgram = "cbrain";
            platforms = platforms.unix;
          };
        };
      in
      {
        packages.default = package;
```

Keep the existing Home Manager module-generation assertions, formatter, and dev shell below this binding unchanged. The real `cbrain doctor` Home Manager assertions are handled by the approved Task 2 adjustment below.

- [ ] **Step 4: Run the focused contract and release metadata targets**

Run:

```bash
cargo test --test release_workflow nix_package_check_is_portable_and_bounded -- --exact
cargo test --test release_metadata
```

Expected: both commands PASS.

- [ ] **Step 5: Build the unwrapped Linux package**

Run:

```bash
nix build --no-link --print-build-logs path:.#packages.x86_64-linux.default
```

Expected: the pure check environment resolves both Git and curl; `cargoCheckHook` logs both explicit `-p` package selections, all `coding-brain-core` and `coding-brain-tui` tests pass, `postCheck` runs both named root targets, and no `bwrap` process or UID-map error appears.

- [ ] **Step 6: Review the Task 1 diff and stop at the commit authorization gate**

Run:

```bash
git diff --check
git diff -- flake.nix tests/release_workflow.rs
git status --short
```

Expected: only Task 1 files plus previously approved internal research/spec/plan artifacts are changed. Do not stage or commit without explicit user authorization. If authorized later, the proposed commit is:

```text
🐛 fix: make Nix package checks namespace-portable
```

---

### Task 2: Add the installed-package multi-user NixOS VM check

**Files:**
- Create: `nix/tests/storage-security-vm.nix`
- Create: `nix/tests/home-manager-doctor-fixtures.nix`
- Modify: `flake.nix:22-95`
- Modify: `nix/tests/home-manager-module.nix`
- Modify: `tests/release_workflow.rs:104-155`

**Approved implementation adjustment:** Running the existing real `cbrain doctor` assertions in the plain Home Manager derivation proved invalid because the Nix sandbox root is foreign-owned relative to the builder, which production storage validation correctly rejects. With user approval, keep cross-platform Home Manager module generation in the plain derivation and move the unchanged installed-binary Doctor assertions and generated provider fixtures into this VM.

**Interfaces:**
- Consumes: Task 1's `package` derivation and the installed `${package}/bin/cbrain` binary.
- Produces: `storageSecurityVm`, exposed only as `checks.storage-security-vm` on `x86_64-linux`; five named VM subtests covering storage and Home Manager Doctor runtime contracts.

**Acceptance Criteria:**
- The positive VM case initializes current SQLite storage through `cbrain --distill-once` and `cbrain --brain-review list`, parses `cbrain doctor --json`, and proves `0700` directories plus `0600` database files.
- The foreign-owner case proves the test user can write inside its private state leaf, then fails with `state directory ancestor is foreign-owned` and creates no Coding Brain state root.
- The replaceable-ancestor case proves owner/mode `cbrain-test:0777`, then fails with `state directory ancestor is replaceable by another user` and creates no Coding Brain state root.
- The Home Manager Doctor cases preserve the existing valid, mixed-scope, malformed-content, exact-diagnostic, and redaction assertions against the installed package.
- The VM uses the exact installed default-feature package, no runtime patch or feature, no KVM requirement, no forced acceleration, no retries, and a 15-minute timeout.
- Darwin evaluation does not evaluate or expose the VM check.

- [ ] **Step 1: Add the failing VM-wiring contract test**

Add this test to `tests/release_workflow.rs`:

```rust
#[test]
fn nix_exposes_a_bounded_storage_security_vm_check() {
    let flake = include_str!("../flake.nix");
    let vm = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/nix/tests/storage-security-vm.nix"
    ))
    .expect("storage VM test module must exist");

    for required in [
        "storageSecurityVm = pkgs.testers.runNixOSTest",
        "system == \"x86_64-linux\"",
        "storage-security-vm = storageSecurityVm",
        "requiredFeatures.kvm = false",
        "qemu.forceAccel = false",
        "globalTimeout = 15 * 60",
        "--distill-once",
        "--brain-review list",
        "doctor --json",
        "state directory ancestor is foreign-owned",
        "state directory ancestor is replaceable by another user",
    ] {
        assert!(
            flake.contains(required) || vm.contains(required),
            "missing storage VM contract: {required}"
        );
    }
    assert!(!flake.contains("passthru.tests"));
}
```

- [ ] **Step 2: Run the contract and confirm the missing VM file fails compilation**

Run:

```bash
cargo test --test release_workflow nix_exposes_a_bounded_storage_security_vm_check -- --exact
```

Expected: FAIL at `storage VM test module must exist` because `nix/tests/storage-security-vm.nix` does not exist.

- [ ] **Step 3: Create the complete NixOS VM test module**

Create `nix/tests/storage-security-vm.nix` with:

```nix
{ package }:

{
  name = "coding-brain-storage-security";
  globalTimeout = 15 * 60;
  requiredFeatures.kvm = false;
  qemu.forceAccel = false;

  nodes.machine =
    { pkgs, ... }:
    {
      users.groups."cbrain-test" = { };
      users.groups."cbrain-attacker" = { };
      users.users."cbrain-test" = {
        isNormalUser = true;
        group = "cbrain-test";
        home = "/home/cbrain-test";
        createHome = true;
      };
      users.users."cbrain-attacker" = {
        isNormalUser = true;
        group = "cbrain-attacker";
        home = "/home/cbrain-attacker";
        createHome = true;
      };
      environment.systemPackages = [
        package
        pkgs.coreutils
        pkgs.util-linux
      ];
    };

  testScript = ''
    import json

    machine.start()
    machine.wait_for_unit("multi-user.target")

    binary = "${package}/bin/cbrain"
    home = "/home/cbrain-test"
    config = f"{home}/.config"

    def run_cbrain(label, state, arguments):
        stdout = f"/tmp/{label}.stdout"
        stderr = f"/tmp/{label}.stderr"
        command = (
            "runuser -u cbrain-test -- env "
            f"HOME={home} XDG_CONFIG_HOME={config} XDG_STATE_HOME={state} "
            "CODING_BRAIN_SKIP_FIRST_RUN=1 "
            f"{binary} {arguments} >{stdout} 2>{stderr}"
        )
        status, _ = machine.execute(command)
        return (
            status,
            machine.succeed(f"tail -c 16384 {stdout}"),
            machine.succeed(f"tail -c 16384 {stderr}"),
        )

    def metadata(path):
        return machine.succeed(f"stat -c '%U:%G:%a' {path}").strip()

    def hierarchy(path):
        return machine.succeed(f"namei -l {path}")

    with subtest("private absolute XDG storage succeeds"):
        state = f"{home}/.local/state"
        machine.succeed(
            "install -d -o cbrain-test -g cbrain-test -m 0700 "
            f"{home} {config} {state}"
        )
        machine.succeed(
            f"test \"$(readlink -f $(command -v cbrain))\" = {binary}"
        )
        status, stdout, stderr = run_cbrain("positive-init", state, "--distill-once")
        assert status == 0, f"init failed: stdout={stdout!r} stderr={stderr!r}"
        status, stdout, stderr = run_cbrain("positive-review", state, "--brain-review list")
        assert status == 0, f"review init failed: stdout={stdout!r} stderr={stderr!r}"
        status, stdout, stderr = run_cbrain("positive-doctor", state, "doctor --json")
        assert status == 0, f"doctor failed: stdout={stdout!r} stderr={stderr!r}"
        json.loads(stdout)
        root = f"{state}/coding-brain"
        assert metadata(root) == "cbrain-test:cbrain-test:700"
        assert metadata(f"{root}/db") == "cbrain-test:cbrain-test:700"
        assert metadata(f"{root}/db/brain.sqlite3") == "cbrain-test:cbrain-test:600"
        assert metadata(f"{root}/db/review.sqlite3") == "cbrain-test:cbrain-test:600"
        machine.succeed(f"test ! -e {root}/brain/decisions.jsonl")
        machine.succeed(f"test ! -e {root}/activity.jsonl")
        machine.succeed(f"test ! -e {root}/lifecycle.jsonl")

    with subtest("foreign-owned ancestor fails closed"):
        machine.succeed("install -d -o cbrain-attacker -g cbrain-attacker -m 0755 /srv/foreign")
        machine.succeed("install -d -o cbrain-test -g cbrain-test -m 0700 /srv/foreign/state")
        machine.succeed("runuser -u cbrain-test -- touch /srv/foreign/state/write-probe")
        machine.succeed("rm /srv/foreign/state/write-probe")
        status, stdout, stderr = run_cbrain("foreign", "/srv/foreign/state", "--distill-once")
        diagnostic = hierarchy("/srv/foreign/state")
        assert status != 0, f"foreign ancestor unexpectedly succeeded: {stdout!r}\n{diagnostic}"
        assert "state directory ancestor is foreign-owned" in stderr, f"{stderr}\n{diagnostic}"
        machine.succeed("test ! -e /srv/foreign/state/coding-brain")

    with subtest("replaceable ancestor fails closed"):
        machine.succeed("install -d -o cbrain-test -g cbrain-test -m 0777 /srv/replaceable")
        assert metadata("/srv/replaceable") == "cbrain-test:cbrain-test:777"
        machine.succeed("runuser -u cbrain-test -- touch /srv/replaceable/write-probe")
        machine.succeed("rm /srv/replaceable/write-probe")
        status, stdout, stderr = run_cbrain("replaceable", "/srv/replaceable", "--distill-once")
        diagnostic = hierarchy("/srv/replaceable")
        assert status != 0, f"replaceable ancestor unexpectedly succeeded: {stdout!r}\n{diagnostic}"
        assert "state directory ancestor is replaceable by another user" in stderr, f"{stderr}\n{diagnostic}"
        machine.succeed("test ! -e /srv/replaceable/coding-brain")
  '';
}
```

- [ ] **Step 4: Wire the VM derivation into Linux flake checks without a package back-reference**

In `flake.nix`, add this value after Task 1's `package`:

```nix
        storageSecurityVm = pkgs.testers.runNixOSTest (
          import ./nix/tests/storage-security-vm.nix { inherit package; }
        );
```

Replace the singular Home Manager check binding with:

```nix
        checks =
          {
            home-manager-module = import ./nix/tests/home-manager-module.nix {
              inherit home-manager pkgs self;
            };
          }
          // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
            storage-security-vm = storageSecurityVm;
          };
```

Do not add `passthru.tests` to `package`.

- [ ] **Step 5: Run the source contract and evaluation checks**

Run:

```bash
cargo test --test release_workflow nix_exposes_a_bounded_storage_security_vm_check -- --exact
nix eval --raw path:.#checks.x86_64-linux.storage-security-vm.drvPath
nix flake check path:. --all-systems --no-build
```

Expected: the Rust contract passes, the first Nix command prints a derivation path, and all supported-system flake outputs evaluate without Darwin trying to expose the VM.

- [ ] **Step 6: Run the VM locally through TCG**

Run:

```bash
nix build --no-link --print-build-logs path:.#checks.x86_64-linux.storage-security-vm
```

Expected: all five named subtests PASS without a `kvm` system-feature error and complete within 15 minutes. If the guest cannot boot or the test exceeds the limit, stop and reopen the runner-boundary design; do not add retries or make the check optional.

- [ ] **Step 7: Review the Task 2 diff and stop at the commit authorization gate**

Run:

```bash
git diff --check
git diff -- flake.nix nix/tests/storage-security-vm.nix tests/release_workflow.rs
git status --short
```

Expected: only Task 1-2 files plus approved internal artifacts are changed. Do not stage or commit without explicit authorization. If authorized later, the proposed commit is:

```text
🧪 test: verify installed storage security in a NixOS VM
```

---

### Task 3: Enforce cross-platform package portability and Linux VM security in GitHub Actions

**Files:**
- Modify: `.github/workflows/ci.yml:155-175`
- Modify: `tests/release_workflow.rs:35-105`

**Interfaces:**
- Consumes: Task 1's package output and Task 2's `checks.x86_64-linux.storage-security-vm` output.
- Produces: a 30-minute-per-leg `nix` CI matrix with sandbox preflight, current-system package builds on Ubuntu and macOS, and a separately attributable Ubuntu-only VM step; source-level guards for the full Cargo matrix and Nix job.

**Acceptance Criteria:**
- The existing `test` matrix still uses `ubuntu-latest` and `macos-latest` and the exact serialized `cargo test --all-targets` command.
- A new `ubuntu-latest`/`macos-latest` Nix matrix uses stable per-OS check names, `cachix/install-nix-action@v31`, enables and verifies sandboxing, and has `timeout-minutes: 30` per leg.
- The Ubuntu leg evaluates every standard flake system without building before running its package and VM builds.
- The named package step builds the current-system default output on both operating systems; the named VM step uses its exact Linux flake attribute and is guarded by `runner.os == 'Linux'`.
- Neither CI step grants privileges, probes around AppArmor, enables KVM, or soft-fails.

- [ ] **Step 1: Strengthen the existing full-suite authority test and add a failing Nix-job contract**

Extend `ci_bounds_parallel_tests_for_global_process_state`:

```rust
#[test]
fn ci_bounds_parallel_tests_for_global_process_state() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let test = job(workflow, "test", "core-standalone");
    assert_contract(test, "os: [ubuntu-latest, macos-latest]");
    assert_contract(test, "cargo test --all-targets -- --test-threads=1");
}
```

Add:

```rust
#[test]
fn ci_builds_the_sandboxed_nix_package_and_storage_vm() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    let nix = job(workflow, "nix", "core-standalone");

    for required in [
        "os: [ubuntu-latest, macos-latest]",
        "name: Nix (${{ matrix.os }})",
        "runs-on: ${{ matrix.os }}",
        "timeout-minutes: 30",
        "cachix/install-nix-action@v31",
        "sandbox = true",
        "- name: Verify Nix sandbox",
        "nix config show sandbox",
        "- name: Evaluate all flake systems",
        "nix flake check --all-systems --no-build",
        "- name: Build Nix package",
        "nix build --no-link --print-build-logs .#",
        "- name: Run installed storage security VM",
        "if: runner.os == 'Linux'",
        "nix build --no-link --print-build-logs .#checks.x86_64-linux.storage-security-vm",
    ] {
        assert_contract(nix, required);
    }
    assert!(!nix.contains("continue-on-error"));
    assert!(!nix.contains("--unshare-user"));
}
```

- [ ] **Step 2: Run the Nix-job contract and confirm the job is missing**

Run:

```bash
cargo test --test release_workflow ci_builds_the_sandboxed_nix_package_and_storage_vm -- --exact
```

Expected: FAIL with `missing workflow job: nix`.

- [ ] **Step 3: Add the bounded Nix CI job**

Insert this job before `core-standalone` in `.github/workflows/ci.yml`:

```yaml
  nix:
    name: Nix (${{ matrix.os }})
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest]
    runs-on: ${{ matrix.os }}
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v6
      - uses: cachix/install-nix-action@v31
        with:
          extra_nix_config: |
            sandbox = true
      - name: Verify Nix sandbox
        run: test "$(nix config show sandbox)" = "true"
      - name: Evaluate all flake systems
        if: runner.os == 'Linux'
        run: nix flake check --all-systems --no-build
      - name: Build Nix package
        run: nix build --no-link --print-build-logs .#
      - name: Run installed storage security VM
        if: runner.os == 'Linux'
        run: nix build --no-link --print-build-logs .#checks.x86_64-linux.storage-security-vm
```

- [ ] **Step 4: Run all release/workflow contract tests**

Run:

```bash
cargo test --test release_workflow
```

Expected: every workflow contract passes, including the unchanged Ubuntu/macOS complete-suite matrix and new Nix job.

- [ ] **Step 5: Validate workflow and Nix formatting**

Run:

```bash
nix run path:.#formatter.x86_64-linux -- --check flake.nix nix/tests/storage-security-vm.nix
git diff --check
```

Expected: Nix files are formatted and no whitespace errors are reported. If the formatter does not support `--check`, run `nix run path:.#formatter.x86_64-linux -- flake.nix nix/tests/storage-security-vm.nix`, inspect the mechanical diff, then rerun `git diff --check`.

- [ ] **Step 6: Review the Task 3 diff and stop at the commit authorization gate**

Run:

```bash
git diff -- .github/workflows/ci.yml tests/release_workflow.rs
git status --short
```

Expected: the workflow contains no privilege escalation, retry, or soft-failure path. Do not stage or commit without explicit authorization. If authorized later, the proposed commit is:

```text
👷 ci: require portable Nix package and storage VM checks
```

---

### Task 4: Document and verify the complete three-gate contract

**Files:**
- Modify: `CHANGELOG.md:7-22`
- Verify only: `flake.nix`
- Verify only: `nix/tests/home-manager-module.nix`
- Verify only: `nix/tests/home-manager-doctor-fixtures.nix`
- Verify only: `nix/tests/storage-security-vm.nix`
- Verify only: `.github/workflows/ci.yml`
- Verify only: `tests/release_workflow.rs`

**Interfaces:**
- Consumes: Tasks 1-3 as one candidate patch.
- Produces: changelog disclosure, fresh local quality-gate evidence, and a handoff that separates package, complete-suite, VM, Darwin, branch-protection, and downstream status.

**Acceptance Criteria:**
- The changelog explains the portable package boundary and required installed-package VM without claiming a runtime change.
- Formatting, Clippy, full default-feature Cargo tests, release contracts, Linux package build, Home Manager module check, and storage VM check all pass from the final tree.
- No production storage/path file changed and `flake.lock` remains unchanged.
- Darwin package and GitHub-hosted TCG results are reported as CI-dependent until the actual matrix legs complete; unevaluated architectures receive no runtime claim.
- Missing branch-protection requirements for either complete Cargo leg or either Nix matrix leg are an explicit merge blocker, not an optional handoff observation.
- No commit, push, branch-protection mutation, or downstream rerun occurs without authorization.

- [ ] **Step 1: Add the changelog entry**

Add this bullet under `## [Unreleased]` → `### Changed` in `CHANGELOG.md`:

```markdown
- Nix package checks no longer require a nested Bubblewrap user namespace.
  Package derivations run the portable lower-layer and release-contract suites,
  while required Ubuntu/macOS Cargo jobs retain the complete Rust suite and an
  x86_64-linux NixOS VM verifies installed SQLite storage ownership and unsafe
  ancestor rejection with real unprivileged users.
```

- [ ] **Step 2: Run formatting and static analysis**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
nix run path:.#formatter.x86_64-linux -- --check flake.nix nix/tests/storage-security-vm.nix
git diff --check
```

Expected: all commands PASS. If the pinned formatter does not support `--check`, run it through the same `path:.#formatter.x86_64-linux` output without `--check`, inspect the formatting-only diff, and rerun `git diff --check`.

- [ ] **Step 3: Run the complete default-feature Rust suite**

Run:

```bash
cargo test --all-targets -- --test-threads=1
```

Expected: the complete root, core, TUI, and integration suite passes with no newly ignored or skipped security tests.

- [ ] **Step 4: Run focused release and Home Manager contracts**

Run:

```bash
cargo test --test release_workflow
cargo test --test release_metadata
nix build --no-link --print-build-logs path:.#checks.x86_64-linux.home-manager-module
```

Expected: all commands PASS.

- [ ] **Step 5: Rebuild the final package and VM checks**

Run:

```bash
nix build --no-link --print-build-logs path:.#packages.x86_64-linux.default
nix build --no-link --print-build-logs path:.#checks.x86_64-linux.storage-security-vm
```

Expected: both commands PASS; package logs show normal `cargoCheckHook` plus the two contract targets, and VM logs show all five named storage and Home Manager Doctor scenarios.

- [ ] **Step 6: Prove the patch did not change production security or dependency pins**

Run:

```bash
git diff --exit-code -- src/brain/storage/security.rs crates/coding-brain-core/src/paths.rs Cargo.toml Cargo.lock flake.lock
rg -n "bubblewrap|/bin/bwrap|--unshare-user|CBRAIN_NIX_CHECK_(UID|GID)|nixCheckCargo" flake.nix .github/workflows/ci.yml
git status --short
```

Expected: the first command reports no diff, `rg` returns no matches, and status lists only the approved packaging/test/CI/changelog/internal artifacts.

- [ ] **Step 7: Inspect external gate configuration without mutating it**

After a PR exists and network access is authorized, run:

```bash
gh api repos/aleadag/coding-brain/branches/main/protection
```

Expected: report whether both Ubuntu/macOS complete-suite checks and both stable Nix matrix checks are required. A missing required check is a merge blocker; insufficient read permission is an unresolved external verification blocker. Do not change repository settings automatically.

- [ ] **Step 8: Stop at commit, push, CI, and downstream authorization gates**

Present the final diff and local evidence. Do not commit or push without explicit authorization. If authorized, preserve the task-level commit split proposed in Tasks 1-3 and use this final documentation commit:

```text
📝 docs: document portable Nix verification boundary
```

After an authorized push, wait for both Nix matrix legs and the Ubuntu/macOS Cargo jobs. The Ubuntu Nix leg is the equivalent exact runner reproduction required by `codexctl-dzlb9.14`; do not rerun the linked `nix-configs` workflow unless its Coding Brain input has first been updated and the user separately authorizes that external change.

---

### Task 5: Make the package check respect Darwin Nix sandbox capabilities

**Files:**
- Modify: `tests/release_workflow.rs:150-185`
- Modify: `flake.nix:31-40`
- Verify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: the existing explicit `cargoTestFlags` package selection, Nixpkgs `pkgs.lib.optionals`, `pkgs.stdenv.isDarwin`, and the unchanged complete Ubuntu/macOS Cargo matrix.
- Produces: two exact libtest filters appended only for Darwin Nix builds; Linux Nix retains both tests, and ordinary macOS Cargo remains their required runtime gate.

**Acceptance Criteria:**
- Darwin Nix skips only `helpers::tests::status_webhook_keeps_only_retained_session_fields` and `project::tests::git_root_preserves_non_utf8_path_bytes`.
- The exclusions are encoded in `cargoTestFlags` with `pkgs.lib.optionals pkgs.stdenv.isDarwin`; they are not environment-variable self-skips in Rust.
- Linux Nix still runs both named tests and its package build passes.
- The ordinary `Test (macos-latest)` job retains the exact complete serialized Cargo command and passes both named tests.
- The macOS Nix package build passes with sandboxing still asserted true.
- Production Rust code, sandbox settings, dependencies, `flake.lock`, and the `fix/zha56` worktree are unchanged.

- [ ] **Step 1: Add a failing release-contract assertion for the Darwin-only filters**

Extend `nix_package_check_is_portable_and_bounded` in `tests/release_workflow.rs` after its existing required-contract loop:

```rust
    let darwin = flake
        .split_once("++ pkgs.lib.optionals pkgs.stdenv.isDarwin [")
        .expect("Nix package checks must isolate Darwin-only capability filters")
        .1
        .split_once("];")
        .expect("Darwin-only capability filters must be a bounded list")
        .0;
    for required in [
        "\"--\"",
        "helpers::tests::status_webhook_keeps_only_retained_session_fields",
        "project::tests::git_root_preserves_non_utf8_path_bytes",
    ] {
        assert_contract(darwin, required);
    }
    assert_eq!(
        darwin.matches("\"--skip\"").count(),
        2,
        "Darwin Nix must skip only the two approved capability-dependent tests"
    );
```

- [ ] **Step 2: Run the contract test and verify RED**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow nix_package_check_is_portable_and_bounded -- --exact
```

Expected: FAIL with `Nix package checks must isolate Darwin-only capability filters` because `cargoTestFlags` has no Darwin conditional yet.

- [ ] **Step 3: Append the two exact Darwin-only libtest filters**

Change the existing `cargoTestFlags` assignment in `flake.nix` to:

```nix
          cargoTestFlags = [
            "-p"
            "coding-brain-core"
            "-p"
            "coding-brain-tui"
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            "--"
            "--skip"
            "helpers::tests::status_webhook_keeps_only_retained_session_fields"
            "--skip"
            "project::tests::git_root_preserves_non_utf8_path_bytes"
          ];
```

Do not modify either Rust test. The argument separator and filters remain absent from Linux because `lib.optionals false` contributes an empty list.

- [ ] **Step 4: Run focused GREEN verification**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow nix_package_check_is_portable_and_bounded -- --exact
nix flake check path:. --all-systems --no-build
nix run path:.#formatter.x86_64-linux -- --check flake.nix
```

Expected: all three commands pass; all four default systems evaluate, and `flake.nix` is formatted.

- [ ] **Step 5: Prove Linux Nix did not lose either test**

Run:

```bash
nix build --no-link --print-build-logs path:.#packages.x86_64-linux.default
```

Expected: PASS, with package logs containing successful executions of both `status_webhook_keeps_only_retained_session_fields` and `git_root_preserves_non_utf8_path_bytes`. Absence of either name is a failed acceptance check.

- [ ] **Step 6: Run the local regression gates**

Run:

```bash
nix develop path:. --command cargo test --test release_workflow
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo clippy --all-targets -- -D warnings
git diff --exit-code -- crates/coding-brain-core/src/helpers.rs crates/coding-brain-core/src/project.rs src/brain/storage/legacy.rs tests/storage_migration.rs Cargo.toml Cargo.lock flake.lock
```

Expected: all commands pass and the protected production, `zha56`, dependency, and lock files have no diff.

- [ ] **Step 7: Commit, push, and inspect the exact replacement jobs only after authorization**

After an authorized push, inspect PR #87:

```bash
gh pr checks 87
```

Expected: `Nix (macos-latest)`, `Nix (ubuntu-latest)`, and `Test (macos-latest)` pass. The macOS Cargo log must still show both exact tests passing; the macOS Nix log must show exactly those two tests filtered and no sandbox relaxation. Do not rerun or modify the separate `zha56` worktree.

---

## Final Handoff Checklist

- Report changed files and the exact purpose of each.
- Report package-check evidence separately from complete Cargo-suite evidence.
- Report VM positive/foreign-owner/replaceable-ancestor evidence separately.
- Report Darwin as locally unverified unless a real Darwin job completed; distinguish complete Cargo coverage from the two-filter Nix package check.
- Report GitHub-hosted TCG as unverified until the new CI job completes within 30 minutes.
- Report branch-protection state without mutating it.
- Report commit, push, PR, and downstream rerun status explicitly.

## Stress Test Results: Portable Nix Check Isolation Implementation Plan

### Resolved Decisions

- Preserve the standard `cargoCheckHook`; require build-log evidence that both
  explicitly selected lower-layer crates run before the two root contract
  targets in `postCheck`.
- Give both unsafe-ancestor VM fixtures explicit write probes so failures are
  attributable to Coding Brain validation rather than incidental Unix access.
- Keep one Linux/Darwin package definition, expose the NixOS VM only on
  `x86_64-linux`, and evaluate every standard flake system.
- Build the current-system Nix package on both `ubuntu-latest` and
  `macos-latest`; run the installed-package VM only on Ubuntu.
- Give both Nix matrix legs stable check names and treat missing branch
  protection for any complete Cargo or Nix leg as a merge blocker.
- Bound VM command output to 16 KiB and include the relevant `namei -l`
  hierarchy on negative-case assertion failures, without retries.
- Retain the 15-minute VM and 30-minute CI budgets; a TCG budget failure reopens
  runner selection instead of weakening the check.
- Preserve task-level review boundaries and every commit, push, PR,
  branch-protection, and downstream authorization gate.
- Select `coding-brain-core` and `coding-brain-tui` explicitly so future
  workspace crates do not silently enter the portable package boundary.
- Use `path:.` for local pre-commit Nix verification so untracked new Nix files
  are tested without staging them; committed CI uses `.#`.
- Keep both capability-dependent tests in Linux Nix and ordinary macOS Cargo;
  append their exact `--skip` filters only to Darwin Nix `cargoTestFlags`.

### Changes Made

- Added a replaceable-ancestor write probe and explicit review-database
  initialization.
- Expanded the Nix job from Ubuntu-only to an Ubuntu/macOS package matrix with
  an Ubuntu-only all-systems evaluation and VM step.
- Added stable matrix identities and made missing protection an explicit merge
  blocker.
- Strengthened VM diagnostics while bounding output.
- Replaced open-ended workspace selection with two explicit portable crates.
- Corrected local Nix commands to include untracked implementation files.
- Added curl as a declared check-only input after the first unwrapped package
  build proved the retained webhook test otherwise times out before making its
  loopback request.
- Added the PR #87 follow-up that retains both capability tests on Linux and in
  ordinary macOS Cargo while filtering them only from Darwin Nix.

### Deferred / Parking Lot

- Real GitHub-hosted TCG duration remains empirical; failure within the approved
  budget requires a new runner-boundary decision.
- Runtime evidence for architectures without actual runners is not claimed;
  those outputs are evaluation-checked only.
- Branch-protection mutation and downstream workflow reruns require separate
  authorization.

### Confidence Assessment

- Overall: High
- Areas of concern: the replacement real Darwin Nix package build remains an
  execution-time acceptance gate; source evaluation cannot prove Seatbelt
  behavior.
