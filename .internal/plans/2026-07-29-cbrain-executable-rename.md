# `cbrain` Executable Rename Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. The implementation epic is `codexctl-ic9o`; its four child tasks already exist. Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Ship exactly one executable named `cbrain` while preserving every Coding Brain package, repository, crate, configuration, state, environment-variable, and project namespace.

**Architecture:** A root-crate executable classifier separates the current `cbrain` basename from cleanup-only stale `coding-brain` and `codexctl` basenames. Cargo, Clap, hooks, installers, and packaging use `cbrain`; existing package and archive identities remain `coding-brain`, and tests guard both sides of that boundary.

**Tech Stack:** Rust 2024, Clap, Cargo integration tests, Nix/Home Manager, POSIX shell installers, Homebrew Ruby formulae, Arch PKGBUILD, GitHub Actions.

## Global Constraints

- Install exactly one executable: `cbrain`.
- Do not ship a `coding-brain` wrapper, symlink, alias, or compatibility executable.
- Keep the crates.io package, repository, internal Rust crates, `programs.coding-brain`, `$XDG_CONFIG_HOME/coding-brain`, `$XDG_STATE_HOME/coding-brain`, `.coding-brain*`, and `CODING_BRAIN_*` namespaces unchanged.
- Treat `coding-brain` and `codexctl` as stale managed hook programs only when the complete known hook shape matches; never claim modified or lookalike commands.
- Keep release archive filenames in the form `coding-brain-<version>-<target>.tar.gz`, with `cbrain` as their sole executable member.
- Make `install.sh` fail before writing if `${INSTALL_DIR}/coding-brain` exists; never delete that path automatically.
- Preserve unrelated worktree changes.
- Do not commit, push, publish, or sync Beads without explicit user authorization.

---

## File Structure

- Create `src/executable.rs`: one source of truth for current, stale-managed, and unmanaged executable basenames.
- Modify `src/main.rs`, `src/doctor.rs`, `src/commands.rs`, `src/brain/*.rs`, and `src/init/**`: change command-facing text and hook classification while preserving product/path strings.
- Modify `Cargo.toml` and Rust integration tests: expose and consume only `CARGO_BIN_EXE_cbrain`.
- Modify `flake.nix`, `nix/**`, `install.sh`, `.github/workflows/release.yml`, `scripts/render-*.sh`, and `packaging/**`: install/archive `cbrain` without renaming packages or archives.
- Modify current user documentation and release guidance: show `cbrain` commands while leaving namespace and historical references intact.
- Keep the approved design and ADR as the rationale: `.internal/specs/2026-07-29-cbrain-executable-rename-design.md` and `docs/decisions/ADR-0005-use-cbrain-as-the-sole-executable.md`.

### Task 1: Centralize executable identity

**Files:**

- Create: `src/executable.rs`
- Modify: `src/main.rs`

**Interfaces:**

- Consumes: `std::path::Path`
- Produces:
  - `pub(crate) const CURRENT_PROGRAM: &str`
  - `pub(crate) const STALE_MANAGED_PROGRAMS: &[&str]`
  - `pub(crate) enum ProgramIdentity`
  - `pub(crate) fn classify_program(&str) -> ProgramIdentity`
  - `pub(crate) fn is_current_program(&str) -> bool`
  - `pub(crate) fn is_managed_program(&str) -> bool`

**Acceptance Criteria:**

- `cbrain` and absolute paths whose basename is `cbrain` classify as current.
- `coding-brain`, `codexctl`, and absolute paths with those exact basenames classify as stale-managed.
- Empty, trailing-component, and lookalike names classify as unmanaged.
- This task changes no installed binary or generated hook behavior yet.

- [ ] **Step 1: Add failing classifier tests**

Create `src/executable.rs` with the public shape and tests first:

```rust
use std::path::Path;

pub(crate) const CURRENT_PROGRAM: &str = "cbrain";
pub(crate) const STALE_MANAGED_PROGRAMS: &[&str] = &["coding-brain", "codexctl"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramIdentity {
    Current,
    StaleManaged,
    Unmanaged,
}

pub(crate) fn classify_program(_program: &str) -> ProgramIdentity {
    todo!("implemented after the tests fail")
}

pub(crate) fn is_current_program(program: &str) -> bool {
    classify_program(program) == ProgramIdentity::Current
}

pub(crate) fn is_managed_program(program: &str) -> bool {
    classify_program(program) != ProgramIdentity::Unmanaged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_only_exact_current_and_stale_basenames() {
        for program in ["cbrain", "/nix/store/hash/bin/cbrain"] {
            assert_eq!(classify_program(program), ProgramIdentity::Current);
        }
        for program in [
            "coding-brain",
            "codexctl",
            "/usr/local/bin/coding-brain",
            "/opt/tools/codexctl",
        ] {
            assert_eq!(classify_program(program), ProgramIdentity::StaleManaged);
        }
        for program in [
            "",
            "cbrain-old",
            "coding-brain-wrapper",
            "my-codexctl",
            "/usr/local/bin/",
        ] {
            assert_eq!(classify_program(program), ProgramIdentity::Unmanaged);
        }
    }
}
```

Add `mod executable;` beside the other root modules in `src/main.rs`.

- [ ] **Step 2: Run the classifier test and observe the intended failure**

Run:

```sh
cargo test --bin coding-brain executable::tests::classifies_only_exact_current_and_stale_basenames -- --exact
```

Expected: FAIL because `classify_program` reaches the temporary `todo!`.

- [ ] **Step 3: Implement exact basename classification**

Replace the temporary function with:

```rust
pub(crate) fn classify_program(program: &str) -> ProgramIdentity {
    let Some(name) = Path::new(program).file_name().and_then(|name| name.to_str()) else {
        return ProgramIdentity::Unmanaged;
    };
    if name == CURRENT_PROGRAM {
        ProgramIdentity::Current
    } else if STALE_MANAGED_PROGRAMS.contains(&name) {
        ProgramIdentity::StaleManaged
    } else {
        ProgramIdentity::Unmanaged
    }
}
```

- [ ] **Step 4: Verify the classifier**

Run:

```sh
cargo test --bin coding-brain executable::tests::classifies_only_exact_current_and_stale_basenames -- --exact
```

Expected: PASS.

- [ ] **Step 5: Review checkpoint**

Run `git diff --check` and `git diff -- src/executable.rs src/main.rs`. Confirm this task only introduces the classifier and module declaration. Do not commit without explicit authorization.

### Task 2: Rename the Rust executable and integrate safe stale-hook handling

**Files:**

- Modify: `Cargo.toml`
- Modify: `src/main.rs`
- Modify: `src/commands.rs`
- Modify: `src/doctor.rs`
- Modify: `src/brain/briefing.rs`
- Modify: `src/brain/decisions.rs`
- Modify: `src/brain/insights.rs`
- Modify: `src/brain/metrics.rs`
- Modify: `src/brain/review.rs`
- Modify: `src/init/hooks.rs`
- Modify: `src/init/marker.rs`
- Modify: `src/init/mod.rs`
- Modify: `src/init/phases.rs`
- Modify: `src/init/prompt.rs`
- Modify: `src/init/provider_hooks/antigravity.rs`
- Modify: `src/init/provider_hooks/mod.rs`
- Modify: `tests/config_mode_cli.rs`
- Modify: `tests/distill_process.rs`
- Modify: `tests/headless_activity.rs`
- Modify: `tests/hook_activity.rs`
- Modify: `tests/integration_tests.rs`
- Modify: `tests/lifecycle_hook_cli.rs`
- Modify: `tests/public_namespace.rs`
- Modify: `tests/removed_surfaces.rs`

**Interfaces:**

- Consumes: Task 1's executable classifier.
- Produces: Cargo binary target `cbrain`, Clap command `cbrain`, current hook commands using the running `cbrain`, and cleanup-only stale recognition.

**Acceptance Criteria:**

- Cargo exposes only `CARGO_BIN_EXE_cbrain`; Rust tests compile without `CARGO_BIN_EXE_coding-brain`.
- Help, version, completions, manpage, doctor, onboarding, and command hints use `cbrain`.
- Current generated Codex, Claude, and Antigravity hooks use `cbrain` or the running absolute `.../cbrain`.
- Exact `coding-brain` and `codexctl` managed hooks are stale, replaceable, and removable; modified and lookalike commands remain preserved.
- XDG, project, environment-variable, repository, package, and Rust crate strings remain `coding-brain`.

- [ ] **Step 1: Add failing front-door and stale-hook regressions**

In `tests/public_namespace.rs`, update `isolated_command` to use
`env!("CARGO_BIN_EXE_cbrain")`, then add:

```rust
#[test]
fn cli_is_cbrain_while_public_namespaces_remain_coding_brain() {
    let temp = tempfile::tempdir().unwrap();
    let help = isolated_command(&temp).arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Usage: cbrain"));

    let config = isolated_command(&temp)
        .args(["config", "show"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&config.stdout).contains("coding-brain/config.toml"));
}
```

Extend `stale_hooks_are_diagnostic_until_init` so it runs the same assertion
for an exact `coding-brain --permission-hook` fixture as for `codexctl`, and
asserts the rewritten file contains `env!("CARGO_BIN_EXE_cbrain")`. Add a
lookalike `coding-brain-wrapper --permission-hook` entry and assert it survives
unchanged.

At the existing provider-specific merge/inspection test boundaries in
`src/init/provider_hooks/mod.rs` and
`src/init/provider_hooks/antigravity.rs`, add a table with these command
basenames:

```rust
fn claude_stop(program: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("{program} --recovery-hook --provider claude"),
            "timeout": 30
        }]
    })
}
```

For each of `coding-brain` and `codexctl`, write
`{"hooks":{"Stop":[claude_stop(program)]}}` to the Claude fixture path, call
`stage_provider_hooks_at`, and assert
`replacement_json(&plans, AgentProvider::Claude)["hooks"]["Stop"][0]["hooks"][0]["command"]`
equals `cbrain --recovery-hook --provider claude`. For
`coding-brain-wrapper` and `my-codexctl`, assert the original
`claude_stop(program)` remains present and `preserved_modified_entries`
contains `claude:Stop`.

For Antigravity, refactor `definition()` into:

```rust
fn definition() -> Value {
    definition_for(CURRENT_PROGRAM)
}

fn definition_for(program: &str) -> Value {
    json!({
        "PreToolUse": [{"matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("{program} --permission-hook --provider antigravity --antigravity-hook-event PreToolUse"),
            "timeout": 30
        }]}],
        "PostToolUse": [{"matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse"),
            "timeout": 2
        }]}],
        "PreInvocation": [{
            "type": "command",
            "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation"),
            "timeout": 2
        }],
        "PostInvocation": [{
            "type": "command",
            "command": format!("{program} --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation"),
            "timeout": 2
        }],
        "Stop": [{
            "type": "command",
            "command": format!("{program} --recovery-hook --provider antigravity --antigravity-hook-event Stop"),
            "timeout": 30
        }]
    })
}

fn is_exact_managed_definition(existing: &Value) -> bool {
    existing == &definition()
        || STALE_MANAGED_PROGRAMS
            .iter()
            .any(|program| existing == &definition_for(program))
}
```

Use `is_exact_managed_definition` only at the existing whole-definition
ownership boundary; a single changed argument, timeout, matcher, event, or
field must still preserve the entry as modified. Test `definition_for` with
`cbrain`, both stale names, and both lookalikes through `merge`.

- [ ] **Step 2: Run the front-door test and observe the compile failure**

Run:

```sh
cargo test --test public_namespace cli_is_cbrain_while_public_namespaces_remain_coding_brain -- --exact
```

Expected: compilation fails because Cargo does not yet define
`CARGO_BIN_EXE_cbrain`.

- [ ] **Step 3: Change the Cargo and Clap identities**

Change only the binary target in `Cargo.toml`:

```toml
[[bin]]
name = "cbrain"
path = "src/main.rs"
```

Keep `[package].name = "coding-brain"` and `[lib].name = "coding_brain"`.
Change the Clap declaration in `src/main.rs` to:

```rust
#[derive(Parser)]
#[command(
    name = "cbrain",
    version,
    about = "Supervise coding-agent activity with a local brain that learns from you."
)]
```

Change every integration-test binary macro in the files listed above from
`CARGO_BIN_EXE_coding-brain` to `CARGO_BIN_EXE_cbrain`.

- [ ] **Step 4: Integrate current-versus-stale classification into hook ownership**

In `src/init/hooks.rs`, import Task 1's helpers and replace the local basename
predicates:

```rust
use crate::executable::{is_current_program, is_managed_program, CURRENT_PROGRAM};

#[cfg(test)]
fn managed_executable() -> PathBuf {
    PathBuf::from(CURRENT_PROGRAM)
}
```

Keep `is_exact_current_command` restricted to `is_current_program` and
`is_exact_managed_command` restricted to `is_managed_program`. Do not weaken
matcher, event, argument, timeout, status-message, or provider checks.

In `src/init/provider_hooks/mod.rs`, replace the duplicated basename matcher
with `crate::executable::is_managed_program`, and change both production and
test fallbacks to `CURRENT_PROGRAM`:

```rust
#[cfg(not(test))]
fn managed_executable() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from(CURRENT_PROGRAM))
}

#[cfg(test)]
fn managed_executable() -> PathBuf {
    PathBuf::from(CURRENT_PROGRAM)
}
```

Retain the existing full `handler_is_exact` and `command_targets_provider`
checks before removal. Update `src/init/provider_hooks/antigravity.rs` fixtures
and definitions so current generated commands use `cbrain`.

- [ ] **Step 5: Change command-facing prose without changing namespaces**

In the listed `src/**` files, change only invocations and executable-path hints,
for example:

```rust
println!("cbrain init --check");
println!("Run `cbrain` to open the Brain TUI.");
out.push_str("cbrain doctor\n");
```

Preserve strings such as:

```rust
"coding-brain/config.toml"
".coding-brain/project.toml"
"CODING_BRAIN_SKIP_FIRST_RUN"
"https://github.com/aleadag/coding-brain"
```

Update unit-test expectations in the same files to match the command-facing
changes.

- [ ] **Step 6: Verify Rust behavior**

Run:

```sh
cargo test --test public_namespace
cargo test --test integration_tests
cargo test --test lifecycle_hook_cli
cargo test --test hook_activity
cargo test
```

Expected: all commands PASS; generated current hooks contain `cbrain`, exact
old hooks are stale and replaceable, and namespace tests still find
`coding-brain` paths.

- [ ] **Step 7: Review checkpoint**

Run:

```sh
rg -n 'CARGO_BIN_EXE_coding-brain' src tests
git diff --check
git diff -- Cargo.toml src tests
```

Expected: the first command returns no matches. Review every remaining
`coding-brain` change contextually; do not commit without explicit
authorization.

### Task 3: Rename packaged artifacts and make raw upgrades fail safely

**Files:**

- Modify: `flake.nix`
- Modify: `nix/home-manager.nix`
- Modify: `nix/tests/home-manager-module.nix`
- Modify: `install.sh`
- Modify: `.github/workflows/release.yml`
- Modify: `scripts/render-homebrew-formula.sh`
- Modify: `scripts/render-aur-bin-files.sh`
- Modify: `packaging/homebrew-core/coding-brain.rb`
- Modify: `packaging/aur/coding-brain-bin/PKGBUILD`
- Modify: `packaging/aur/coding-brain-bin/.SRCINFO`
- Modify: `packaging/nixpkgs/README.md`
- Modify: `tests/install_script.rs`
- Modify: `tests/public_namespace.rs`
- Modify: `tests/release_workflow.rs`

**Interfaces:**

- Consumes: Task 2's `cbrain` Cargo artifact and CLI identity.
- Produces: Nix/Home Manager, release, installer, Homebrew, AUR, and nixpkgs definitions that install only `cbrain`.

**Acceptance Criteria:**

- Nix keeps `pname = "coding-brain"` and uses `mainProgram = "cbrain"`.
- Home Manager's immutable hook executable ends in `/bin/cbrain`; the option and generated config paths remain `programs.coding-brain` and `coding-brain/...`.
- Release archives retain `coding-brain-...tar.gz` names and contain `cbrain`.
- `install.sh` exits before download or install when `${INSTALL_DIR}/coding-brain` exists, and otherwise installs `${INSTALL_DIR}/cbrain`.
- Homebrew and AUR retain package identities but install, test, complete, and generate a manpage for `cbrain`.
- Rendered and checked-in package files agree.

- [ ] **Step 1: Add failing installer and package-contract tests**

In `tests/install_script.rs`, add a fixture with an existing old destination:

```rust
#[test]
fn refuses_regular_or_broken_symlink_old_binary_before_download_or_install() {
    for broken_symlink in [false, true] {
        let fixture = Fixture::new();
        fixture.add_verifier("shasum");
        let old = fixture.install_dir.join("coding-brain");
        if broken_symlink {
            symlink(fixture.root.path().join("missing"), &old).unwrap();
        } else {
            fs::write(&old, b"old").unwrap();
        }

        let output = fixture.run();

        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("remove the existing coding-brain executable"));
        assert!(fixture.log().is_empty());
        assert!(fs::symlink_metadata(&old).is_ok());
        assert!(!fixture.install_dir.join("cbrain").exists());
    }
}
```

Extend `tests/public_namespace.rs::front_door_metadata_is_provider_aware` and
`tests/release_workflow.rs` to assert:

```rust
assert!(flake.contains("mainProgram = \"cbrain\""));
assert!(release_workflow.contains("tar czf ../../../coding-brain-${TAG}-${{ matrix.target }}.tar.gz cbrain"));
assert!(!release_workflow.contains(".tar.gz coding-brain\n"));
```

- [ ] **Step 2: Run focused tests and observe failures**

Run:

```sh
cargo test --test install_script refuses_old_binary_before_download_or_install -- --exact
cargo test --test public_namespace front_door_metadata_is_provider_aware -- --exact
cargo test --test release_workflow
```

Expected: FAIL because the installer and packaging still target
`coding-brain`.

- [ ] **Step 3: Update Nix and Home Manager executable outputs**

In `flake.nix`, keep `pname` and change:

```nix
meta.mainProgram = "cbrain";
```

In `nix/tests/home-manager-module.nix`, create the test package with:

```nix
testPackage = pkgs.writeShellScriptBin "cbrain" "exit 0";
```

Keep all `programs.coding-brain`, `coding-brain/config.toml`, Antigravity
definition-key, and derivation-name strings unchanged. Change only activation
instructions that users execute, such as `cbrain doctor`, and assert
`expectedExe` ends in `/bin/cbrain`.

- [ ] **Step 4: Add the raw-installer preflight and new destination**

In `install.sh`, perform this check immediately after resolving `INSTALL_DIR`,
before checksum-tool detection, OS detection, network access, temporary
directory creation, or privilege escalation:

```sh
OLD_DESTINATION="${INSTALL_DIR}/coding-brain"
DESTINATION="${INSTALL_DIR}/cbrain"

if [ -e "${OLD_DESTINATION}" ] || [ -L "${OLD_DESTINATION}" ]; then
    echo "error: remove the existing coding-brain executable at ${OLD_DESTINATION}, then rerun this installer" >&2
    exit 1
fi
```

Install `"${TMP_DIR}/cbrain"` to `"${DESTINATION}"`, print that exact
destination, and finish with `Run 'cbrain init' to get started.` Preserve
`REPO`, archive filename, checksum URL, and temporary-directory safety.

- [ ] **Step 5: Update release and downstream packaging**

In `.github/workflows/release.yml`, archive `cbrain` while retaining the
existing archive filename:

```yaml
tar czf ../../../coding-brain-${TAG}-${{ matrix.target }}.tar.gz cbrain
```

In `scripts/render-homebrew-formula.sh` and the checked-in Homebrew formula,
install `cbrain`, invoke `#{bin}/cbrain`, generate completions from `cbrain`,
and write `man1/"cbrain.1"`.

In `scripts/render-aur-bin-files.sh`, `PKGBUILD`, and `.SRCINFO`, keep
`pkgname`, `pkgbase`, `source`, `provides`, and `conflicts` as package
identities, but install:

```sh
install -Dm755 "${srcdir}/cbrain" "${pkgdir}/usr/bin/cbrain"
```

Update `packaging/nixpkgs/README.md` to use `mainProgram = "cbrain"` and test
`cbrain --help`, while keeping the nixpkgs package path and `pname`.

- [ ] **Step 6: Verify installer, renderers, and Home Manager**

Run:

```sh
cargo test --test install_script
cargo test --test public_namespace
cargo test --test release_workflow
nix build .#checks.x86_64-linux.home-manager-module
```

Expected: PASS. Inspect the Nix result and confirm its selected main program is
`cbrain`; package and config namespaces remain `coding-brain`. The repository
proves current package outputs and manifests, not the internal upgrade engines
of Cargo, Homebrew, AUR, or Nix. Cargo's tracked binary-name-set contract
triggers reinstallation when the set changes; the raw installer needs the
explicit preflight because it has no ownership database.

- [ ] **Step 7: Review checkpoint**

Run:

```sh
git diff --check
git diff -- flake.nix nix install.sh .github/workflows/release.yml scripts packaging tests/install_script.rs tests/release_workflow.rs tests/public_namespace.rs
```

Confirm every producer/consumer pair agrees and no supported output installs
both names. Do not commit without explicit authorization.

### Task 4: Update current documentation and run the full regression gates

**Files:**

- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `.github/RELEASE_TEMPLATE.md`
- Modify: `docs/configuration.md`
- Modify: `docs/index.md`
- Modify: `docs/llms.txt`
- Modify: `docs/quickstart.md`
- Modify: `docs/reference.md`
- Modify: `docs/terminal-support.md`
- Modify: `docs/troubleshooting.md`
- Modify: `packaging/aur/README.md`
- Modify: `packaging/homebrew-core/README.md`
- Verify: `docs/decisions/ADR-0001-lifecycle-hooks-as-status-evidence.md`
- Verify: `docs/decisions/ADR-0002-coding-brain-product-boundary.md`
- Verify: `docs/decisions/ADR-0003-fail-safe-hook-and-learning-persistence.md`
- Verify: `docs/decisions/ADR-0004-provider-aware-guards-and-terminal-actuation.md`
- Verify: `docs/decisions/ADR-0005-use-cbrain-as-the-sole-executable.md`
- Modify: `tests/public_namespace.rs`

**Interfaces:**

- Consumes: Tasks 1–3's final executable and packaging behavior.
- Produces: current documentation that consistently teaches `cbrain`, plus final guards proving preserved namespaces and the single-executable contract.

**Acceptance Criteria:**

- Current installation and usage examples invoke `cbrain`.
- Documentation clearly states that packages remain named `coding-brain`.
- Upgrade guidance tells imperative-hook users to run `cbrain init <provider>` and explains the raw-installer old-path preflight.
- Historical ADR and changelog statements remain unchanged unless they are current instructions or become factually false.
- Rust, formatting, Clippy, Nix, installer, release, packaging, and documentation guards all pass.

- [ ] **Step 1: Add a failing current-documentation regression**

In `tests/public_namespace.rs`, add:

```rust
#[test]
fn current_documentation_uses_cbrain_commands_and_preserves_namespaces() {
    let current_docs = [
        ("README", include_str!("../README.md")),
        ("configuration", include_str!("../docs/configuration.md")),
        ("quickstart", include_str!("../docs/quickstart.md")),
        ("reference", include_str!("../docs/reference.md")),
        ("troubleshooting", include_str!("../docs/troubleshooting.md")),
    ];
    for (name, document) in current_docs {
        assert!(document.contains("cbrain"), "{name}");
        assert!(!document.contains("Run `coding-brain"), "{name}");
        assert!(document.contains("coding-brain"), "{name} must retain package or path context");
    }
}
```

Keep this guard targeted to current instructional phrasing; do not ban all
historical `coding-brain` text.

- [ ] **Step 2: Run the documentation regression and observe failure**

Run:

```sh
cargo test --test public_namespace current_documentation_uses_cbrain_commands_and_preserves_namespaces -- --exact
```

Expected: FAIL because current docs still instruct users to invoke
`coding-brain`.

- [ ] **Step 3: Update current user and release guidance**

Use this installation pattern consistently:

```sh
cargo install coding-brain
cbrain init codex
cbrain doctor
cbrain
```

Explain once in README/quickstart that the package is `coding-brain` and its
sole executable is `cbrain`. Update current command examples, hook repair
instructions, Home Manager post-rebuild commands, completion/man invocations,
and release-template guidance.

Add an `[Unreleased]` changelog entry for the breaking executable rename and
the required imperative-hook repair. Preserve dated historical entries that
describe the prior `coding-brain` executable.

Update packaging READMEs only where they tell maintainers which installed
command to test. Do not rename formula, AUR package, repository, source
archive, or nixpkgs package paths.

- [ ] **Step 4: Run deterministic namespace and diff audits**

Run:

```sh
rg -n 'CARGO_BIN_EXE_coding-brain' src tests
rg -n 'Run `coding-brain|coding-brain (init|doctor|config|completions|man)|coding-brain --' README.md docs .github/RELEASE_TEMPLATE.md packaging
rg -n '\\$XDG_(CONFIG|STATE)_HOME/cbrain|\\.cbrain|programs\\.cbrain|CODING_BRAIN' README.md docs src nix tests
git diff --check
```

Expected:

- no old Cargo binary macro;
- remaining old command hits are historical quotations or explicit stale-hook
  discussion reviewed one by one;
- no `cbrain` namespace replacements;
- `CODING_BRAIN_*` hits remain present and unchanged;
- no whitespace errors.

- [ ] **Step 5: Run the full quality gates serially**

Run:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
nix build path:.#checks.x86_64-linux.home-manager-module
nix build path:.#default
```

Expected: every command exits 0. Run Cargo and Nix gates serially to avoid
shared target/store contention.

- [ ] **Step 6: Inspect the final output and status**

Run:

```sh
test -x result/bin/cbrain
test ! -e result/bin/coding-brain
result/bin/cbrain --version
result/bin/cbrain --help
git status --short
git diff --stat
```

Expected: only `result/bin/cbrain` exists; version/help identify `cbrain`; the
diff contains only files traceable to `codexctl-w4cj`, its design, and ADR.

- [ ] **Step 7: Beads and review checkpoint**

Close each completed implementation task only after its task-specific gates
pass. Run `bd lint` and the final verification workflow before closing
`codexctl-w4cj`. Report the uncommitted diff and await explicit authorization
before any commit, push, PR, or Beads remote sync.

## Stress Test Results: `cbrain` Executable Rename Implementation Plan

### Resolved Decisions

- Keep the four-task sequence because the Cargo target switch and Rust
  integration-test macro migration form one atomic compilation boundary.
- Use the missing `CARGO_BIN_EXE_cbrain` compile error as Task 2's narrow red
  phase, then restore compilation immediately.
- Test stale cleanup independently at the Codex, Claude, and Antigravity
  merge/inspection boundaries.
- Test the raw-installer preflight with both a regular old path and a broken
  symlink before any external action.
- Verify repository-controlled package outputs and manifests without claiming
  to reimplement every external package manager's upgrade engine.
- Separate automated current-guidance guards from a contextual audit of
  historical command text.
- Use explicit workspace/all-target Cargo gates and the flake's real
  `default` Nix package attribute.

### Changes Made

- Added all-provider stale/current/lookalike hook regressions to Task 2.
- Expanded Task 3's old-path preflight test to regular files and broken
  symlinks, and moved the preflight before every external action.
- Narrowed package-upgrade proof to tracked package contracts plus
  repository-controlled outputs and manifests.
- Corrected the final Cargo and Nix commands.

### Deferred / Parking Lot

- Live end-to-end upgrade tests inside Cargo, Homebrew, pacman, and Nix are not
  reproduced in this repository; their ownership databases and generation
  semantics remain external contracts.

### Confidence Assessment

- Overall: High.
- Areas of concern: context-specific command-text edits remain broad, so the
  final namespace and historical-text audit is mandatory even after tests pass.
