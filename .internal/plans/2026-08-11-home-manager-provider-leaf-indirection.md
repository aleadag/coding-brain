# Home Manager Provider Leaf Indirection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a child Bead of `codexctl-5n458`; do not create a duplicate epic. Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make Doctor accept production-shaped Home Manager provider leaf indirection while preserving fail-closed project diagnostics and strict imperative no-follow behavior.

**Architecture:** The exact outer global link into `*-home-manager-files/<provider suffix>` establishes Home Manager ownership. Read-only inspection may then resolve either the existing regular leaf or exactly one validated absolute link into a normal Nix store object; provider comparison remains unchanged, and mutation paths continue using the strict non-symlink reader.

**Tech Stack:** Rust 1.88, `std::fs`, Cargo unit tests, Nix/Home Manager derivation fixtures, NixOS Python VM tests.

## Global Constraints

- Runtime inspection stays offline: no Nix daemon, profile, Home Manager CLI, xattr, or canonicalization dependency.
- The existing 1 MiB provider-file bound remains exact.
- Project-scope symlinks remain unsupported.
- `read_managed_file`, init, removal, transaction application, rollback, recovery, and journal handling must not become symlink-permissive.
- The accepted inner topology has at most one link and stays under the same Nix-store root beneath a normal top-level store object, excluding dot-prefixed internal namespaces.
- A recognized outer Home Manager link retains Home Manager ownership even when its inner target is unsafe or unreadable.
- Repository verification is candidate evidence; Home Manager activation and installed Doctor verification are a separate authorization gate.
- Do not stage, commit, push, close `codexctl-5n458`, or activate Home Manager without explicit user authorization.
- Use `nix develop path:. --command ...` for Rust gates.

---

### Task 1: Accept One Home Manager Store-Leaf Link

**Files:**
- Modify: `src/init/provider_hooks/mod.rs:416-599`
- Test: `src/init/provider_hooks/mod.rs:2685-3207`

**Interfaces:**
- Consumes: the existing exact outer-link validation in `read_provider_file_for_inspection` and `home_manager_parent_topology_is_supported`.
- Produces: `fn clean_store_relative_target(target: &Path, nix_store_root: &Path) -> Option<PathBuf>`, `fn is_normal_store_object_path(relative: &Path) -> bool`, and `fn home_manager_read_target(target: &Path, nix_store_root: &Path) -> Result<PathBuf, ProviderHookDiagnosticReason>`.
- Preserves: `InspectedProviderFile`, `ProviderHookOwnership`, `ProviderHookDiagnosticReason`, and provider semantic comparison APIs.

**Acceptance Criteria:**
- Direct regular Home Manager leaves remain current for all three providers.
- A production-shaped Home Manager leaf linking once to an absolute regular source under the same test store is current for Codex, Claude, and Antigravity.
- The outer exact generation link remains the ownership authority.
- No mutation reader or transaction code changes.

- [ ] **Step 1: Add a production-shaped test helper**

Add this helper beside `write_home_manager_provider_file`; keep the existing helper unchanged:

```rust
#[cfg(unix)]
fn write_home_manager_provider_store_leaf(
    store: &Path,
    home: &Path,
    project: &Path,
    provider: AgentProvider,
    bytes: &[u8],
) -> PathBuf {
    let generation = store.join("0123456789abcdefghijklmnopqrstuv-home-manager-files");
    let leaf = match provider {
        AgentProvider::Codex => generation.join(".codex/hooks.json"),
        AgentProvider::Claude => generation.join(".claude/settings.json"),
        AgentProvider::Antigravity => generation.join(".gemini/config/hooks.json"),
    };
    let source = match provider {
        AgentProvider::Codex => store.join("11111111111111111111111111111111-codex-hooks"),
        AgentProvider::Claude => store
            .join("22222222222222222222222222222222-claude-settings")
            .join("settings.json"),
        AgentProvider::Antigravity => {
            store.join("33333333333333333333333333333333-antigravity-hooks.json")
        }
    };
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, bytes).unwrap();
    std::fs::create_dir_all(leaf.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&source, &leaf).unwrap();
    let home_link = provider_path(provider, HookScope::Global, home, project);
    std::fs::create_dir_all(home_link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&leaf, &home_link).unwrap();
    source
}
```

- [ ] **Step 2: Add the all-provider red test**

```rust
#[cfg(unix)]
#[test]
fn provider_inspection_accepts_home_manager_store_leaf_links_for_all_providers() {
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
        write_home_manager_provider_store_leaf(
            &store,
            &home,
            &project,
            provider,
            &bytes,
        );

        let inspection =
            inspect_provider_hooks_at_with_store(provider, &home, &project, &store);

        assert_eq!(inspection.state, ProviderHookState::Current, "{provider:?}");
        assert_eq!(
            inspection.ownership,
            ProviderHookOwnership::HomeManager,
            "{provider:?}"
        );
    }
}
```

- [ ] **Step 3: Run the new test and verify RED**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  init::provider_hooks::tests::provider_inspection_accepts_home_manager_store_leaf_links_for_all_providers \
  -- --exact --nocapture
```

Expected: FAIL because each generation leaf is currently classified `Invalid` / `HomeManager` / `UnsupportedTopology`.

- [ ] **Step 4: Add clean store-target parsing**

Add these helpers beside `home_manager_parent_topology_is_supported`:

```rust
fn clean_store_relative_target(target: &Path, nix_store_root: &Path) -> Option<PathBuf> {
    let raw_target = target.to_str()?;
    if !raw_target.starts_with('/')
        || raw_target[1..]
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return None;
    }
    let relative = target.strip_prefix(nix_store_root).ok()?.to_path_buf();
    (!relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))))
    .then_some(relative)
}

fn is_normal_store_object_path(relative: &Path) -> bool {
    relative
        .components()
        .next()
        .and_then(|component| match component {
            std::path::Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .is_some_and(|component| !component.starts_with('.') && component.contains('-'))
}
```

- [ ] **Step 5: Resolve only the supported read target**

Add the read-only resolver:

```rust
fn home_manager_read_target(
    target: &Path,
    nix_store_root: &Path,
) -> Result<PathBuf, ProviderHookDiagnosticReason> {
    let metadata = fs::symlink_metadata(target)
        .map_err(|_| ProviderHookDiagnosticReason::Unreadable)?;
    if metadata.file_type().is_file() {
        return Ok(target.to_path_buf());
    }
    if !metadata.file_type().is_symlink() {
        return Err(ProviderHookDiagnosticReason::UnsupportedTopology);
    }

    let inner_target = fs::read_link(target)
        .map_err(|_| ProviderHookDiagnosticReason::Unreadable)?;
    let relative = clean_store_relative_target(&inner_target, nix_store_root)
        .filter(|relative| is_normal_store_object_path(relative))
        .ok_or(ProviderHookDiagnosticReason::UnsupportedTopology)?;
    let relative_parent = relative
        .parent()
        .ok_or(ProviderHookDiagnosticReason::UnsupportedTopology)?;
    home_manager_parent_topology_is_supported(nix_store_root, relative_parent)?;

    let inner_metadata = fs::symlink_metadata(&inner_target)
        .map_err(|_| ProviderHookDiagnosticReason::Unreadable)?;
    if inner_metadata.file_type().is_symlink() || !inner_metadata.file_type().is_file() {
        return Err(ProviderHookDiagnosticReason::UnsupportedTopology);
    }
    Ok(inner_target)
}
```

In `read_provider_file_for_inspection`, replace the direct target-type rejection with:

```rust
let read_target = match home_manager_read_target(&target, nix_store_root) {
    Ok(target) => target,
    Err(reason) => {
        return invalid_inspected_file(ProviderHookOwnership::HomeManager, reason);
    }
};
```

Then change `File::open(&target)` to `File::open(&read_target)`. Keep the opened-handle regular-file check and 1 MiB read unchanged.

- [ ] **Step 6: Run direct and two-hop tests and verify GREEN**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  init::provider_hooks::tests::provider_inspection_accepts_home_manager_store_leaf_links_for_all_providers \
  -- --exact --nocapture
nix develop path:. --command cargo test --bin cbrain \
  init::provider_hooks::tests::provider_inspection_accepts_home_manager_files_for_all_providers \
  -- --exact --nocapture
```

Expected: both tests PASS.

- [ ] **Step 7: Review the task diff without committing**

Run `git diff --check` and `git diff -- src/init/provider_hooks/mod.rs`. Do not stage or commit without explicit authorization.

---

### Task 2: Preserve Fail-Closed Topology and Project Diagnostics

**Files:**
- Modify: `src/init/provider_hooks/mod.rs:446-599`
- Test: `src/init/provider_hooks/mod.rs:2960-3382`

**Interfaces:**
- Consumes: `clean_store_relative_target`, `is_normal_store_object_path`, and `home_manager_read_target` from Task 1.
- Produces: exhaustive rejection coverage and `NotADirectory` classification as `UnsupportedTopology`.
- Preserves: invalid-before-stale aggregation and unsupported ownership precedence.

**Acceptance Criteria:**
- Relative, non-store, store-internal, non-UTF-8, repeated-separator, `.` or `..`, broken, directory, multi-hop, and symlinked-parent inner targets remain invalid with Home Manager ownership.
- An unsafe project symlink remains unsupported without inspecting its target.
- A project candidate blocked by a regular non-directory ancestor reports `unsupported_topology`, not `unreadable`.
- Mutation staging still rejects both supported Home Manager links and arbitrary symlinks.

- [ ] **Step 1: Add a failing non-directory project test**

```rust
#[test]
fn provider_inspection_reports_non_directory_project_ancestor_as_unsupported_topology() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(project.join(".codex"), b"").unwrap();

    let inspection = inspect_provider_hooks_at(AgentProvider::Codex, &home, &project);
    let project_file = inspection
        .files
        .iter()
        .find(|file| file.scope == HookScope::Project)
        .unwrap();

    assert_eq!(inspection.state, ProviderHookState::Invalid);
    assert_eq!(inspection.ownership, ProviderHookOwnership::Unsupported);
    assert_eq!(
        project_file.reason,
        Some(ProviderHookDiagnosticReason::UnsupportedTopology)
    );
}
```

- [ ] **Step 2: Run it and verify RED**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  init::provider_hooks::tests::provider_inspection_reports_non_directory_project_ancestor_as_unsupported_topology \
  -- --exact --nocapture
```

Expected: FAIL because the current generic metadata-error branch reports `Unreadable`.

- [ ] **Step 3: Classify `NotADirectory` minimally**

Change the initial metadata match in `read_provider_file_for_inspection` to include this arm before the generic error:

```rust
Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
    return invalid_inspected_file(
        ProviderHookOwnership::Unsupported,
        ProviderHookDiagnosticReason::UnsupportedTopology,
    );
}
```

Do not remap permission or other I/O failures.

- [ ] **Step 4: Add inner-target rejection cases**

Add a table-driven `provider_inspection_rejects_unsafe_home_manager_store_leaf_targets` test. Each case must create the valid outer Home Manager link first, then one of these leaves:

```rust
enum StoreLeafFixture {
    Relative,
    NonStore,
    StoreInternal,
    NonUtf8,
    CurrentDirectory,
    ParentDirectory,
    RepeatedSeparator,
    Broken,
    Directory,
    MultiHop,
    SymlinkedParent,
}
```

Assert `ProviderHookState::Invalid` and `ProviderHookOwnership::HomeManager` for every case. Assert `Unreadable` only for `Broken`; assert `UnsupportedTopology` for all other cases. Rework the old `NestedSymlink` case so it tests `MultiHop`, since one normal store leaf link is now supported.

- [ ] **Step 5: Re-run focused security and aggregation tests**

Run:

```bash
nix develop path:. --command cargo test --bin cbrain \
  provider_inspection_rejects_unsafe_home_manager_store_leaf_targets -- --nocapture
nix develop path:. --command cargo test --bin cbrain \
  provider_inspection_rejects_unsupported_symlinks_and_targets -- --nocapture
nix develop path:. --command cargo test --bin cbrain \
  provider_inspection_preserves_failure_precedence_across_sources -- --nocapture
nix develop path:. --command cargo test --bin cbrain \
  mutation_staging -- --nocapture
```

Expected: all selected tests PASS; existing project symlink and mutation rejection expectations remain unchanged.

- [ ] **Step 6: Review the task diff without committing**

Run `git diff --check` and inspect only `src/init/provider_hooks/mod.rs`. Do not stage or commit without explicit authorization.

---

### Task 3: Exercise the Production Topology in the Packaged Nix VM

**Files:**
- Modify: `nix/tests/home-manager-doctor-fixtures.nix:35-55`
- Modify: `nix/tests/storage-security-vm.nix:35-210`

**Interfaces:**
- Consumes: the packaged `cbrain` reader from Tasks 1-2 and existing `configured` Home Manager module evaluation.
- Produces: `providerHomeManagerFiles` and `invalidProviderHomeManagerFiles` with symlink leaves matching a real Home Manager generation.
- Preserves: fake provider executables, isolated XDG paths, storage-security assertions, and Codex trust advisory assertions.

**Acceptance Criteria:**
- Each simulated home provider path traverses exactly two links before reaching a regular Nix-store source.
- Packaged Doctor passes all three valid global provider rows.
- Packaged Doctor reports the zero-byte Codex project ancestor as project/unsupported/invalid/unsupported_topology.
- Packaged Doctor retains Claude's regular project definition as a mixed duplicate.
- Invalid Antigravity content remains fail-closed without exposing its content.

- [ ] **Step 1: Make fixture leaves real store links**

Replace the copy-based fixture with:

```nix
  invalidAntigravityHooksJson = pkgs.writeText "invalid-antigravity-hooks.json" ''
    ["SECRET_PROVIDER_CONTENT"]
  '';
  providerHomeManagerFiles = pkgs.runCommand "home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    ln -s ${codexHooksJson} "$out/.codex/hooks.json"
    ln -s ${claudeSettingsJson} "$out/.claude/settings.json"
    ln -s ${configured.config.home.file.".gemini/config/hooks.json".source} \
      "$out/.gemini/config/hooks.json"
  '';
  invalidProviderHomeManagerFiles = pkgs.runCommand "invalid-antigravity-home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    ln -s ${codexHooksJson} "$out/.codex/hooks.json"
    ln -s ${claudeSettingsJson} "$out/.claude/settings.json"
    ln -s ${invalidAntigravityHooksJson} "$out/.gemini/config/hooks.json"
  '';
```

- [ ] **Step 2: Add a VM topology assertion**

Before the first Home Manager Doctor run, assert every generation leaf is itself a link and its resolved target is regular:

```python
for relative in [
    ".codex/hooks.json",
    ".claude/settings.json",
    ".gemini/config/hooks.json",
]:
    machine.succeed(f"test -L {provider_files}/{relative}")
    machine.succeed(f"test -f $(readlink -f {provider_files}/{relative})")
```

- [ ] **Step 3: Add the non-directory Codex project fixture**

After the mixed-project assertions, create a separate project and run Doctor:

```python
blocked_project = f"{home}/blocked-codex-project"
machine.succeed(
    "install -d -o cbrain-test -g cbrain-test -m 0700 "
    f"{blocked_project} {blocked_project}/.git"
)
machine.succeed(
    f"runuser -u cbrain-test -- touch {blocked_project}/.codex"
)
status, stdout, stderr = run_cbrain_at(
    "doctor-blocked-codex-project",
    blocked_project,
    home,
    config,
    state,
    "doctor --json",
)
assert status == 1, f"doctor status={status}: stdout={stdout!r} stderr={stderr!r}"
checks = json.loads(stdout)
codex = named_check(checks, "Codex setup")
assert codex["status"] == "fail", codex
assert codex["evidence"]["provider_files"][0]["ownership"] == "home_manager", codex
project_file = next(
    item for item in codex["evidence"]["provider_files"]
    if item["scope"] == "project"
)
assert project_file == {
    "path": f"{blocked_project}/.codex/hooks.json",
    "path_lossy": False,
    "scope": "project",
    "ownership": "unsupported",
    "state": "invalid",
    "reason": "unsupported_topology",
}, codex
```

- [ ] **Step 4: Build the lightweight Home Manager check**

Run:

```bash
nix build .#checks.x86_64-linux.home-manager-module --no-link
```

Expected: exit 0.

- [ ] **Step 5: Run the packaged storage-security VM**

Run:

```bash
nix build .#checks.x86_64-linux.storage-security-vm --no-link
```

Expected: exit 0; the VM logs show the Home Manager provider, mixed-project, blocked-Codex-project, invalid-content, and storage-security subtests complete.

- [ ] **Step 6: Review the Nix diff without committing**

Run `nix fmt -- --check .`, `git diff --check`, and inspect only the two Nix test files. Do not stage or commit without explicit authorization.

---

### Task 4: Document and Verify the Repository Candidate

**Files:**
- Modify: `CHANGELOG.md:7-20`
- Verify: all files changed by Tasks 1-3

**Interfaces:**
- Consumes: the completed Rust and Nix behavior from Tasks 1-3.
- Produces: an `[Unreleased]` user-facing fix note and fresh full-gate evidence.
- Does not produce: a Home Manager activation, installed binary, commit, push, or closed main Bead.

**Acceptance Criteria:**
- The changelog describes the observable Doctor correction and retained fail-closed behavior.
- Formatting, full tests, Clippy with denied warnings, build, Home Manager check, and storage-security VM pass from the final tree.
- The final report distinguishes repository candidate verification from live acceptance.

- [ ] **Step 1: Add the changelog entry**

Under `[Unreleased]` → `Fixed`, add:

```markdown
- Doctor now recognizes Home Manager provider files whose generation leaves
  link to immutable Nix-store sources, while unsupported project paths and
  imperative init or removal remain fail-closed.
```

- [ ] **Step 2: Run formatting checks**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix fmt -- --check .
git diff --check
```

Expected: all exit 0 with no formatting diff or whitespace error.

- [ ] **Step 3: Run the full Rust tests serially**

Run:

```bash
nix develop path:. --command cargo test --workspace -- --test-threads=1
```

Expected: exit 0; all unit, integration, TUI, and doc tests pass.

- [ ] **Step 4: Run lint and build gates**

Run:

```bash
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
```

Expected: both exit 0 with no warnings.

- [ ] **Step 5: Re-run both Nix gates from the final tree**

Run:

```bash
nix build .#checks.x86_64-linux.home-manager-module --no-link
nix build .#checks.x86_64-linux.storage-security-vm --no-link
```

Expected: both exit 0.

- [ ] **Step 6: Audit scope and tracker state**

Run:

```bash
git status --short --branch
git diff --stat
git diff -- src/init/provider_hooks/mod.rs \
  nix/tests/home-manager-doctor-fixtures.nix \
  nix/tests/storage-security-vm.nix \
  CHANGELOG.md
bd -C /home/alexander/.beads-planning show codexctl-5n458
```

Expected: only the approved research/spec/plan, provider inspection, Nix fixture/VM, and changelog files are changed; `codexctl-5n458` remains in progress.

- [ ] **Step 7: Stop at the live-acceptance authorization gate**

Report the verified repository candidate and ask whether to rebuild and activate the user's Home Manager generation. Do not run activation, installed Doctor acceptance, commit, push, or main-Bead closure without explicit authorization.

---

## Task Dependencies

```text
Task 1: two-hop reader
  -> Task 2: negative topology and project diagnostics
    -> Task 3: packaged Nix/VM acceptance
      -> Task 4: documentation and final repository gates
```

When execution begins, import one child task Bead per task under `codexctl-5n458`, copy each task's Acceptance Criteria into its Bead, and add the three sequential `blocks` dependencies shown above. Do not create another epic.
