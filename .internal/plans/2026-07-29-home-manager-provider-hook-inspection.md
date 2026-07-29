# Home Manager Provider Hook Inspection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make `coding-brain doctor` validate matching Home Manager-owned Codex, Claude, and Antigravity hook files while every imperative mutation path continues to reject symlinks.

**Architecture:** Separate strict mutation reads from bounded read-only inspection. Both paths reuse a pure provider-definition comparison helper, while inspection returns state plus ownership so Doctor can route remediation to Home Manager, imperative setup, mixed scopes, or unsupported file replacement.

**Tech Stack:** Rust 1.88, `std::fs`, Serde JSON, Cargo unit tests, Nix/Home Manager evaluation, jq.

## Global Constraints

- Accept declarative links only for global provider paths whose normalized absolute target matches `/nix/store/*-home-manager-files/<exact-provider-relative-path>`.
- Reject `.`, `..`, relative targets, nested symlinks, wrong provider suffixes, project-local symlinks, non-store targets, directories, broken targets, and files larger than 1 MiB.
- Open the captured store target, validate metadata on the opened file handle, and never reopen the home-directory link after ownership validation.
- Keep `read_managed_file`, install, removal, transaction preparation, rollback, and recovery strict regular-file-only paths.
- Do not add dependencies, invoke Nix from Rust, expose inspected paths or contents, or change provider hook schemas or Home Manager options.
- Preserve state precedence: `Invalid`, then `Stale`, then current-definition count producing `Missing`, `Current`, or `Duplicate`.
- Do not commit, push, or publish unless the user separately authorizes it.

---

### Task 1: Ownership-aware read-only provider inspection

**Files:**

- Modify: `src/init/provider_hooks/mod.rs`
- Test: `src/init/provider_hooks/mod.rs`
- Modify mechanically for type compatibility: `src/doctor.rs`
- Test mechanically for type compatibility: `src/doctor.rs`

**Interfaces:**

- Consumes: existing `provider_path`, provider-specific `merge` functions, `MAX_FILE_BYTES`, `contains_managed_command`, and strict `read_managed_file`.
- Produces:
  - `ProviderHookState::{Missing, Current, Duplicate, Stale, Invalid}`
  - `ProviderHookOwnership::{Absent, Imperative, HomeManager, Mixed, Unsupported}`
  - `ProviderHookInspection { state, ownership }`
  - `inspect_provider_hooks_at` using `/nix/store`
  - private `inspect_provider_hooks_at_with_store(..., nix_store_root: &Path)` for deterministic tests.

**Acceptance Criteria:**

- Matching global Home Manager links for all three providers classify as `Current` with `HomeManager` ownership.
- Missing definitions, stale/provider-mismatched definitions, malformed or oversized targets, duplicate mixed scopes, and unsupported symlinks retain the specified state and ownership.
- Inspection rejects non-normal, relative, nested, wrong-suffix, project-local, broken, directory, and non-store targets.
- Regular-file inspection retains existing missing/current/stale/invalid/duplicate behavior.
- Installation and removal staging still reject recognized Home Manager and arbitrary symlinks.
- Doctor compiles against the new state and ownership types without changing status, message, or remediation behavior; ownership-specific Doctor behavior remains Task 2.

- [ ] **Step 1: Introduce failing ownership and Home Manager fixture tests**

Add Unix-only test helpers beside the existing provider inspection tests:

```rust
#[cfg(unix)]
fn write_home_manager_provider_file(
    store: &Path,
    home: &Path,
    project: &Path,
    provider: AgentProvider,
    bytes: &[u8],
) -> PathBuf {
    let generation = store.join("0123456789abcdefghijklmnopqrstuv-home-manager-files");
    let target = match provider {
        AgentProvider::Codex => generation.join(".codex/hooks.json"),
        AgentProvider::Claude => generation.join(".claude/settings.json"),
        AgentProvider::Antigravity => generation.join(".gemini/config/hooks.json"),
    };
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, bytes).unwrap();
    let link = provider_path(provider, HookScope::Global, home, project);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    target
}

#[cfg(unix)]
fn exact_provider_bytes(
    home: &Path,
    project: &Path,
    provider: AgentProvider,
) -> Vec<u8> {
    let mut plans =
        stage_provider_hooks_at(&[provider], HookScope::Global, home, project).unwrap();
    let mut plan = plans.remove(0);
    plan.edits.remove(0).replacement
}
```

Before creating the symlink, call `exact_provider_bytes`. Add:

```rust
#[cfg(unix)]
#[test]
fn provider_inspection_accepts_home_manager_files_for_all_providers() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let store = temp.path().join("nix/store");
        std::fs::create_dir_all(&project).unwrap();
        let bytes = exact_provider_bytes(&home, &project, provider);
        write_home_manager_provider_file(&store, &home, &project, provider, &bytes);

        assert_eq!(
            inspect_provider_hooks_at_with_store(provider, &home, &project, &store),
            ProviderHookInspection {
                state: ProviderHookState::Current,
                ownership: ProviderHookOwnership::HomeManager,
            }
        );
    }
}
```

Add table-driven tests named:

- `provider_inspection_classifies_home_manager_definition_failures`
- `provider_inspection_rejects_unsupported_symlinks_and_targets`
- `provider_inspection_preserves_failure_precedence_across_sources`
- `mutation_staging_still_rejects_home_manager_symlinks`

The definition-failure table must cover malformed JSON → `Invalid`, valid JSON without managed commands → `Missing`, wrong provider or changed command → `Stale`, and oversized bytes → `Invalid`, all with `HomeManager` ownership. The unsupported-target table must cover a relative target, a target containing `..`, a non-store path, the wrong provider suffix, a nested symlink, a directory, a broken target, and a project-local symlink. Directly call both `stage_provider_hooks_at` and `stage_provider_hooks_with(..., true)` in the mutation test and assert both return errors.

- [ ] **Step 2: Run the new tests and verify the red state**

Run:

```bash
cargo test provider_inspection_accepts_home_manager_files_for_all_providers -- --nocapture
cargo test provider_inspection_classifies_home_manager_definition_failures -- --nocapture
cargo test mutation_staging_still_rejects_home_manager_symlinks -- --nocapture
```

Expected: compilation fails because `ProviderHookState`, `ProviderHookOwnership`, and `inspect_provider_hooks_at_with_store` do not exist.

- [ ] **Step 3: Split inspection state from ownership**

Replace the current inspection enum with:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderHookState {
    Missing,
    Current,
    Duplicate,
    Stale,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderHookOwnership {
    Absent,
    Imperative,
    HomeManager,
    Mixed,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProviderHookInspection {
    pub(crate) state: ProviderHookState,
    pub(crate) ownership: ProviderHookOwnership,
}

#[derive(Debug)]
enum InspectedProviderFile {
    Missing,
    Readable {
        bytes: Vec<u8>,
        ownership: ProviderHookOwnership,
    },
    Invalid {
        ownership: ProviderHookOwnership,
    },
}
```

Add a small ownership fold. `Unsupported` wins, Home Manager plus imperative becomes `Mixed`, repeated identical owners stay identical, and `Absent` is neutral:

```rust
fn combine_ownership(
    left: ProviderHookOwnership,
    right: ProviderHookOwnership,
) -> ProviderHookOwnership {
    use ProviderHookOwnership::*;
    match (left, right) {
        (Unsupported, _) | (_, Unsupported) => Unsupported,
        (Absent, owner) | (owner, Absent) => owner,
        (owner, other) if owner == other => owner,
        _ => Mixed,
    }
}
```

- [ ] **Step 4: Implement the inspection-only reader**

Add a private production constant and two helpers:

```rust
const NIX_STORE_ROOT: &str = "/nix/store";

fn expected_home_manager_suffix(provider: AgentProvider) -> &'static Path {
    match provider {
        AgentProvider::Codex => Path::new(".codex/hooks.json"),
        AgentProvider::Claude => Path::new(".claude/settings.json"),
        AgentProvider::Antigravity => Path::new(".gemini/config/hooks.json"),
    }
}
```

`read_provider_file_for_inspection(path, provider, scope, nix_store_root)` must:

1. Return `Missing` on `NotFound`.
2. Read regular files with the existing bounded-read logic and `Imperative` ownership.
3. Return `Invalid { Unsupported }` for non-symlink special files, project-scope symlinks, relative targets, or non-UTF-8 targets.
4. Validate the raw UTF-8 target before `Path` normalization: it must start with `/`, contain no empty interior segment, `.`, `..`, or repeated separator. Only after that check, require `target.strip_prefix(nix_store_root)` to produce exactly a generation component ending in `-home-manager-files` followed by `expected_home_manager_suffix(provider)`.
5. Before assigning Home Manager ownership, require `nix_store_root` and every path component from the generation directory through the target parent to be non-symlink directories. Ancestor symlinks are unsupported topology.
6. Reject `symlink_metadata(target).file_type().is_symlink()`.
7. Open the captured target, validate `file.metadata()?.file_type().is_file()` and its length, then read through `take(MAX_FILE_BYTES + 1)`.
8. Convert failures after exact topology recognition to `Invalid { HomeManager }` without exposing the path or error text through inspection.

Keep `/nix/store` only in the production wrapper:

```rust
pub(crate) fn inspect_provider_hooks_at(
    provider: AgentProvider,
    home: &Path,
    cwd: &Path,
) -> ProviderHookInspection {
    inspect_provider_hooks_at_with_store(provider, home, cwd, Path::new(NIX_STORE_ROOT))
}
```

- [ ] **Step 5: Extract pure provider-definition comparison**

Move the JSON parse, object validation, provider-specific `merge`, and replacement serialization from `stage_provider_hooks_with` into:

```rust
struct ProviderHookComparison {
    replacement: Option<Vec<u8>>,
    preserved_modified_entries: Vec<String>,
}

fn compare_provider_hook(
    provider: AgentProvider,
    original: Option<&[u8]>,
    remove: bool,
    contract: ProviderHookContract,
) -> io::Result<ProviderHookComparison>
```

`ProviderHookContract` distinguishes the existing imperative schema from the exact shipped Home Manager schema. Mutation staging and regular-file inspection always select `Imperative`. Only inspection of a symlink already validated as `HomeManager` selects `HomeManager`; that contract accepts Claude's declarative permission handler without the optional status message and Antigravity's immutable current-package executable. It must not normalize arbitrary executables or make declarative forms valid for regular files.

Mutation staging must continue to obtain owned `(original, original_mode)` only from `read_managed_file`, enforce `MAX_TRANSACTION_BYTES`, call this helper with `original.as_deref()` and the imperative contract, and alone combine a changed replacement with the path, owned original bytes, mode, and hashes to create `ManagedFileEdit`. Inspection calls the same helper with bytes from `InspectedProviderFile::Readable`, selects the contract from the established ownership, and derives state directly from its semantic result. Inspection never constructs, returns, or applies `ManagedFileEdit`.

For each candidate, derive state using the existing rules:

```rust
fn state_from_comparison(
    comparison: &ProviderHookComparison,
    original: Option<&[u8]>,
) -> ProviderHookState {
    if !comparison.preserved_modified_entries.is_empty() {
        return ProviderHookState::Stale;
    }
    if comparison.replacement.is_none() {
        return ProviderHookState::Current;
    }
    let Some(original) = original else {
        return ProviderHookState::Missing;
    };
    match serde_json::from_slice::<serde_json::Value>(original) {
        Ok(root) if contains_managed_command(&root) => ProviderHookState::Stale,
        Ok(_) => ProviderHookState::Missing,
        Err(_) => ProviderHookState::Invalid,
    }
}
```

Aggregate candidate state with unchanged precedence and aggregate ownership independently. An invalid recognized Home Manager target must keep `HomeManager`; an arbitrary invalid symlink must keep `Unsupported`.

- [ ] **Step 6: Adapt Doctor mechanically to the new type**

Import `ProviderHookState` and `ProviderHookOwnership` beside `ProviderHookInspection`. Change state-only comparisons and matches to use `evidence.hooks.state`. Replace existing test and missing-HOME constructors with explicit `ProviderHookInspection { state, ownership }`, using `Imperative` for the existing regular-file matrix, `Absent` for missing-file fixtures, and `Unsupported` when inspection itself is unavailable.

Do not change any Doctor status, message, or fix-hint branch in this task. Ownership-specific remediation and its new tests belong exclusively to Task 2.

- [ ] **Step 7: Run focused and existing provider-hook tests**

Run:

```bash
cargo test provider_inspection_ -- --nocapture
cargo test rejects_wrong_shapes_oversize_symlinks_and_non_regular_files -- --nocapture
cargo test provider_hooks -- --nocapture
cargo test doctor::tests -- --nocapture
```

Expected: all selected tests pass, including the existing strict mutation test.

- [ ] **Step 8: Review checkpoint**

Run:

```bash
git diff --check
git diff -- src/init/provider_hooks/mod.rs src/doctor.rs
```

Verify provider-hook changes belong to inspection ownership, semantic-helper extraction, or regressions, and Doctor changes are mechanical type adaptation only. Do not commit without explicit user authorization.

---

### Task 2: Ownership-aware Doctor status and remediation

**Files:**

- Modify: `src/doctor.rs`
- Test: `src/doctor.rs`

**Interfaces:**

- Consumes: `ProviderHookInspection`, `ProviderHookState`, and `ProviderHookOwnership` plus Task 1's state-only Doctor type adaptation.
- Produces: ownership-specific setup messages and repair hints while retaining current status severity and the separate Codex trust advisory.

**Acceptance Criteria:**

- Current Home Manager definitions pass and have no repair hint.
- Non-current Home Manager definitions never recommend `coding-brain init <provider>`.
- Mixed supported ownership recommends removing a duplicate scope.
- Unsupported ownership recommends replacing the unsafe file or link before setup.
- Imperative regular-file states retain existing status, message, and init remediation.
- Codex hook trust remains a separate advisory mentioning `/hooks`.

- [ ] **Step 1: Write failing Doctor matrix tests**

Update the existing exhaustive setup matrix to construct:

```rust
ProviderHookInspection {
    state: ProviderHookState::Current,
    ownership: ProviderHookOwnership::Imperative,
}
```

Then add a coherent ownership/state table:

```rust
#[test]
fn provider_setup_routes_remediation_by_ownership() {
    for (state, ownership, expected_status, expected_hint) in [
        (
            ProviderHookState::Current,
            ProviderHookOwnership::HomeManager,
            CheckStatus::Pass,
            None,
        ),
        (
            ProviderHookState::Missing,
            ProviderHookOwnership::HomeManager,
            CheckStatus::Advisory,
            Some("Home Manager"),
        ),
        (
            ProviderHookState::Stale,
            ProviderHookOwnership::HomeManager,
            CheckStatus::Fail,
            Some("Home Manager"),
        ),
        (
            ProviderHookState::Invalid,
            ProviderHookOwnership::HomeManager,
            CheckStatus::Fail,
            Some("Home Manager"),
        ),
        (
            ProviderHookState::Duplicate,
            ProviderHookOwnership::Mixed,
            CheckStatus::Advisory,
            Some("duplicate scope"),
        ),
        (
            ProviderHookState::Invalid,
            ProviderHookOwnership::Unsupported,
            CheckStatus::Fail,
            Some("unsafe"),
        ),
    ] {
        let check = check_provider_setup(
            AgentProvider::Claude,
            ProviderSetupEvidence {
                recorded: true,
                executable_available: true,
                hooks: ProviderHookInspection {
                    state,
                    ownership,
                },
            },
        );
        assert_eq!(check.status, expected_status);
        match (check.fix_hint.as_deref(), expected_hint) {
            (None, None) => {}
            (Some(hint), Some(phrase)) => {
                assert!(hint.contains(phrase));
                assert!(!hint.contains("coding-brain init"));
            }
            pair => panic!("unexpected hint pair: {pair:?}"),
        }
    }
}
```

Add or extend a Codex test so a current Home Manager setup row is `Pass`, the trust row remains `Advisory`, and only the trust row contains `/hooks`.

- [ ] **Step 2: Run Doctor tests and verify the red state**

Run:

```bash
cargo test provider_setup_routes_remediation_by_ownership -- --nocapture
```

Expected: tests fail because `check_provider_setup` still emits one imperative repair hint for every non-current state.

- [ ] **Step 3: Match setup state separately from ownership**

Change all state matches to use `evidence.hooks.state`, including `check_antigravity_hook_contract_with`. Build the repair hint after status selection:

```rust
let fix_hint = match (state, evidence.hooks.ownership) {
    (ProviderSetupState::Current | ProviderSetupState::Skipped, _) => None,
    (_, ProviderHookOwnership::HomeManager) => Some(format!(
        "Repair the Home Manager-owned {} definitions in your Nix configuration, rebuild Home Manager, then rerun `coding-brain doctor`.",
        provider.label()
    )),
    (_, ProviderHookOwnership::Mixed) => Some(format!(
        "Remove the duplicate {} scope from either Home Manager or the regular provider configuration, then rerun `coding-brain doctor`.",
        provider.label()
    )),
    (_, ProviderHookOwnership::Unsupported) => Some(format!(
        "Replace the unsafe {} provider file or link before rerunning setup.",
        provider.label()
    )),
    (_, ProviderHookOwnership::Imperative | ProviderHookOwnership::Absent) => Some(format!(
        "Repair {} setup with `coding-brain init {}`.",
        provider.label(),
        provider.as_str()
    )),
};
```

Keep message text unchanged for imperative states. Declarative current rows may use the same current message; ownership belongs in remediation, not noisy success output.

- [ ] **Step 4: Run focused and complete Doctor tests**

Run:

```bash
cargo test doctor::tests -- --nocapture
```

Expected: all Doctor tests pass; regular-file matrix assertions still find `coding-brain init`, while declarative/mixed/unsupported cases do not.

- [ ] **Step 5: Review checkpoint**

Run:

```bash
git diff --check
git diff -- src/doctor.rs
```

Verify status severity did not change, no inspected path or content enters output, and the Codex trust row remains independent. Do not commit without explicit user authorization.

---

### Task 3: Real Home Manager store fixture and full verification

**Files:**

- Modify: `nix/tests/home-manager-module.nix`
- Modify: `src/init/provider_hooks/mod.rs`
- Verify: `src/doctor.rs`

**Interfaces:**

- Consumes: Task 1 production `/nix/store` recognition, Task 2 Doctor JSON rows, existing evaluated `cfg.programs.codex.hooks`, `cfg.programs.claude-code.settings`, and Antigravity generated source.
- Produces: a Nix integration assertion proving the packaged Doctor accepts all three real `home-manager-files` symlinks.

**Acceptance Criteria:**

- The Home Manager check creates one real Nix-store generation fixture containing all three provider files at exact home-relative paths.
- Focused Rust tests prove the shipped Claude and Antigravity Home Manager schemas are current only under recognized declarative ownership; regular files and mutation retain the imperative contract.
- Packaged `coding-brain doctor --json` reports Codex, Claude, and Antigravity setup rows as `pass`.
- Those rows have no imperative repair hints, while Codex hook trust remains a distinct advisory.
- The integration assertion tolerates unrelated Doctor failures only by capturing and inspecting JSON; it does not weaken provider-row assertions.
- Formatting, focused tests, full tests, Clippy with warnings denied, build, and the Home Manager Nix check pass.

- [ ] **Step 1: Add a failing Nix Doctor fixture assertion**

In the test's `let` block, add:

```nix
  codexHooksJson = pkgs.writeText "codex-hooks.json" (
    builtins.toJSON { hooks = cfg.programs.codex.hooks; }
  );
  claudeSettingsJson = pkgs.writeText "claude-settings.json" (
    builtins.toJSON cfg.programs.claude-code.settings
  );
  providerHomeManagerFiles = pkgs.runCommand "home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    cp ${codexHooksJson} "$out/.codex/hooks.json"
    cp ${claudeSettingsJson} "$out/.claude/settings.json"
    cp ${cfg.home.file.".gemini/config/hooks.json".source} \
      "$out/.gemini/config/hooks.json"
  '';
  fakeProviders = pkgs.runCommand "coding-brain-fake-providers" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/codex"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/claude"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/agy"
  '';
```

Extend `nativeBuildInputs` with `self.packages.${pkgs.stdenv.hostPlatform.system}.default`. In the builder script:

```bash
  fixture_home="$TMPDIR/home"
  mkdir -p \
    "$fixture_home/.codex" \
    "$fixture_home/.claude" \
    "$fixture_home/.gemini/config" \
    "$TMPDIR/config" \
    "$TMPDIR/state"
  ln -s ${providerHomeManagerFiles}/.codex/hooks.json \
    "$fixture_home/.codex/hooks.json"
  ln -s ${providerHomeManagerFiles}/.claude/settings.json \
    "$fixture_home/.claude/settings.json"
  ln -s ${providerHomeManagerFiles}/.gemini/config/hooks.json \
    "$fixture_home/.gemini/config/hooks.json"

  export HOME="$fixture_home"
  export XDG_CONFIG_HOME="$TMPDIR/config"
  export XDG_STATE_HOME="$TMPDIR/state"
  export PATH="${fakeProviders}/bin:$PATH"

  cd "$TMPDIR"
  doctor_status=0
  coding-brain doctor --json > "$TMPDIR/doctor.json" \
    || doctor_status="$?"
  test "$doctor_status" -eq 0 -o "$doctor_status" -eq 1

  for provider in Codex Claude Antigravity; do
    jq -e --arg name "$provider setup" '
      any(.[]; .name == $name and .status == "pass" and .fix_hint == null)
    ' "$TMPDIR/doctor.json"
  done
  jq -e '
    any(.[];
      .name == "Codex hook trust"
      and .status == "advisory"
      and (.message | contains("trust unverified"))
      and (.fix_hint | contains("/hooks"))
    )
  ' "$TMPDIR/doctor.json"
  jq -e '
    all(.[];
      if (.name | endswith(" setup"))
      then ((.fix_hint // "") | contains("coding-brain init") | not)
      else true
      end
    )
  ' "$TMPDIR/doctor.json"
```

- [ ] **Step 2: Run the Nix acceptance fixture**

Run:

```bash
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).home-manager-module --no-link
```

Expected: the check passes on the first run when Tasks 1–2 are correct. This task adds end-to-end acceptance coverage; it does not introduce new production behavior that needs a synthetic red phase.

- [ ] **Step 3: Diagnose any evidence-backed fixture or implementation failure**

If the check fails, first determine whether the packaged Doctor rejected a correctly shaped fixture or the fixture serialized the wrong top-level provider JSON. If evaluation shows the Codex or Claude root shape differs from the generated provider file, adjust only `codexHooksJson` or `claudeSettingsJson` to match the provider's actual contract. Otherwise return to the failing Task 1 or Task 2 behavior. Do not change hook definitions, force file ownership, bypass symlink recognition, or weaken jq assertions.

- [ ] **Step 4: Run the Nix check to green**

Run:

```bash
nix build .#checks.$(nix eval --raw --impure --expr builtins.currentSystem).home-manager-module --no-link
```

Expected: build succeeds and every jq assertion passes.

- [ ] **Step 5: Run formatting and the focused regression suite**

Run:

```bash
cargo fmt --check
nix fmt -- --check
cargo test provider_inspection_ -- --nocapture
cargo test doctor::tests -- --nocapture
```

Expected: all commands exit zero.

- [ ] **Step 6: Run the complete Rust quality gates serially**

Run:

```bash
cargo test
cargo clippy -- -D warnings
cargo build
```

If the bare toolchain cannot use the locked environment, rerun each command serially through:

```bash
nix develop path:. --command <command>
```

Expected: tests pass, Clippy emits no warnings, and the workspace builds.

- [ ] **Step 7: Final scope and status review**

Run:

```bash
git diff --check
git status --short
git diff --stat
```

Confirm only these files changed:

- `src/init/provider_hooks/mod.rs`
- `src/doctor.rs`
- `nix/tests/home-manager-module.nix`
- `.internal/specs/2026-07-29-home-manager-provider-hook-inspection-design.md`
- `.internal/plans/2026-07-29-home-manager-provider-hook-inspection.md`

Close `codexctl-z6l0` only after every acceptance criterion and quality gate passes. Report the uncommitted changes and wait for explicit commit/push authorization.

## Stress Test Results: Home Manager provider hook implementation plan

### Resolved Decisions

- Keep three sequential tasks: core inspection, Doctor remediation, then packaged Home Manager verification.
- Treat Task 3 as acceptance verification whose first correct run is green, not as a fabricated red/green implementation cycle.
- Share a pure `ProviderHookComparison`; only mutation staging may construct `ManagedFileEdit`.
- Validate raw UTF-8 target text before `Path` normalization so `.`, `..`, and repeated separators fail closed.
- Test coherent state/ownership pairs while preserving the exhaustive imperative state matrix.
- Run Doctor from an isolated temporary cwd and accept only documented exit statuses 0 or 1 before parsing JSON.
- Add `nix fmt -- --check` and close all task/design issues only after every serial quality gate succeeds.

### Changes Made

- Replaced the edit-producing shared helper with a semantic comparison interface.
- Corrected the Task 3 verification model.
- Hardened raw target validation and the Nix fixture process contract.
- Replaced impossible Doctor matrix combinations with coherent cases.
- Added Nix formatting and explicit issue-closure ordering.

### Deferred / Parking Lot

- No parallel task execution: each task consumes the prior task's concrete interface.
- No commit or publication step without separate authorization.

### Confidence Assessment

- Overall: High.
- Areas of concern: the Nix fixture may expose provider-module serialization details, but the plan requires evidence before changing either fixture or production behavior.
