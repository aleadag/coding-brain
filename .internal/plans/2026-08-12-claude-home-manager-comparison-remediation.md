# Claude Home Manager Comparison and Remediation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make Doctor accept an exact Home Manager Claude definition with unrelated empty hook configuration, route failures according to the owner of actual cbrain definitions, retain actionable per-file evidence, and report authoritative Codex hook trust.

**Architecture:** Correct each reduction at its source. The shared nested-hook merge preserves matchers that were empty before cbrain processed them; provider inspection excludes only definition-free imperative records from aggregate ownership; Doctor exposes non-current file evidence on otherwise current rows; and Codex trust comes from a bounded `hooks/list` app-server query rather than a copied hash implementation.

**Tech Stack:** Rust, Serde JSON, Cargo workspace tests, Nix/Home Manager checks.

## Global Constraints

- Preserve exact executable, provider, flag, matcher, timeout, and handler-shape validation.
- Preserve state precedence: `Invalid`, then `Stale`, then the count of `Current` definitions.
- Keep each missing file's path, scope, filesystem ownership, state, and optional reason in diagnostic evidence.
- Do not change Home Manager topology recognition or its bounded store-leaf reader.
- Cover the installed `lib.mkAfter` ordering in which unrelated matchers precede the cbrain matcher; do not add generalized order-insensitive matcher normalization.
- Do not change imperative staging, removal, compare-and-swap, rollback, recovery, or journal handling.
- Do not repair the separate `nix-configs/.codex` non-directory ancestor.
- Do not commit, push, publish, activate Home Manager, or mutate installed configuration without explicit user authorization.

---

## File Map

- Modify `src/init/provider_hooks/mod.rs`: preserve unrelated empty nested-hook matchers, aggregate ownership from definition-bearing or failing records, and add provider inspection regressions.
- Modify `src/init/hooks.rs`: expose command classification needed to select cbrain hooks from authoritative Codex results and replace the unconditional trust flag with explicit trust evidence.
- Modify `src/doctor.rs`: retain non-current file evidence on current provider rows, add remediation coverage, and implement bounded Codex `hooks/list` trust probing.
- No production file, public type, CLI, Nix module, or configuration schema is added.

### Task 1: Preserve Pre-existing Empty Hook Matchers

**Files:**
- Modify: `src/init/provider_hooks/mod.rs` in `merge_nested_hooks`
- Test: `src/init/provider_hooks/mod.rs` provider inspection tests

**Interfaces:**
- Consumes: existing `merge_nested_hooks(root, provider, definitions, remove, accept_legacy, preserved)` and `home_manager_schema_variant_bytes` test helper.
- Produces: unchanged function signatures; a merge result that removes matchers emptied by removal of exact cbrain handlers but preserves matchers that were already empty.
- Execution dependency: none; this task runs first.

**Acceptance Criteria:**
- An exact Home Manager Claude definition with the installed empty `Stop` matcher is `Current / HomeManager`.
- A pre-existing empty unrelated matcher survives provider comparison and merge.
- Missing, modified, extra, or malformed managed definitions remain non-current.

- [ ] **Step 1: Add the failing installed-shape regression**

Add a Unix test beside `provider_inspection_accepts_home_manager_schema_variants`:

```rust
#[cfg(unix)]
#[test]
fn provider_inspection_accepts_home_manager_claude_with_unrelated_empty_matcher() {
    let executable = Path::new("/nix/store/current-coding-brain/bin/cbrain");
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let store = temp.path().join("nix/store");
    std::fs::create_dir_all(&project).unwrap();
    let mut root = serde_json::from_slice::<serde_json::Value>(
        &home_manager_schema_variant_bytes(
            &home,
            &project,
            AgentProvider::Claude,
            executable,
        ),
    )
    .unwrap();
    let stop = root["hooks"]["Stop"].as_array_mut().unwrap();
    stop.insert(0, serde_json::json!({ "matcher": "", "hooks": [] }));
    assert!(stop[0]["hooks"].as_array().unwrap().is_empty());
    assert!(
        stop[1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with("--recovery-hook --provider claude")
    );
    let mut bytes = serde_json::to_vec_pretty(&root).unwrap();
    bytes.push(b'\n');
    let comparison = compare_provider_hook(
        AgentProvider::Claude,
        Some(&bytes),
        false,
        ProviderHookComparisonContract::HomeManager { executable },
    )
    .unwrap();
    assert!(comparison.replacement.is_none());
    assert!(comparison.preserved_modified_entries.is_empty());
    write_home_manager_provider_store_leaf(
        &store,
        &home,
        &project,
        AgentProvider::Claude,
        &bytes,
    );

    let inspection = inspect_provider_hooks_at_with_store_and_executable(
        AgentProvider::Claude,
        &home,
        &project,
        &store,
        executable,
    );

    assert_eq!(inspection.state, ProviderHookState::Current);
    assert_eq!(inspection.ownership, ProviderHookOwnership::HomeManager);
}
```

- [ ] **Step 2: Run the regression and verify the current behavior fails**

Run:

```bash
nix develop path:. --command cargo test provider_inspection_accepts_home_manager_claude_with_unrelated_empty_matcher -- --nocapture
```

Expected: FAIL because the inspection state is `Stale`, proving the pre-existing empty matcher changes the comparison.

- [ ] **Step 3: Preserve only matchers that were already empty**

Replace the mutable matcher loop and unconditional empty-matcher cleanup in `merge_nested_hooks` with `retain_mut`. Capture whether each handler list was non-empty before exact managed handlers are removed:

```rust
matchers.retain_mut(|matcher| {
    let matcher_object = matcher.as_object_mut().expect("shape validated");
    let matcher_is_exact = matcher_object
        .keys()
        .all(|key| key == "matcher" || key == "hooks")
        && matcher_object
            .get("matcher")
            .and_then(serde_json::Value::as_str)
            == definition.matcher
        && (definition.matcher.is_some() || !matcher_object.contains_key("matcher"));
    let handlers = matcher_object
        .get_mut("hooks")
        .and_then(serde_json::Value::as_array_mut)
        .expect("shape validated");
    let had_handlers = !handlers.is_empty();
    handlers.retain(|handler| {
        let Some(command) = handler.get("command").and_then(serde_json::Value::as_str)
        else {
            return true;
        };
        if !provider_hook_candidate(command, provider, accept_legacy) {
            return true;
        }
        let managed = command_targets_provider(command, provider, accept_legacy);
        let exact = matcher_is_exact
            && managed
            && handler_is_exact(handler, provider, definition, accept_legacy);
        if !exact {
            preserved.push(format!("{provider}:{}", definition.event));
            collision |= managed;
        } else {
            removed_exact = true;
        }
        !exact
    });
    !had_handlers || !handlers.is_empty()
});
```

Remove the following unconditional cleanup because `retain_mut` now removes only entries emptied by this merge:

```rust
matchers.retain(|matcher| {
    matcher
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|handlers| !handlers.is_empty())
});
```

- [ ] **Step 4: Run focused merge and inspection tests**

Run:

```bash
nix develop path:. --command cargo test provider_inspection_accepts_home_manager_claude_with_unrelated_empty_matcher -- --nocapture
nix develop path:. --command cargo test provider_merges_preserve_unrelated_config_and_never_add_statusline -- --nocapture
nix develop path:. --command cargo test provider_inspection -- --test-threads=1
```

Expected: all commands exit 0; the matching regression passes in every applicable Cargo harness, and existing stale/malformed/unsafe inspection cases remain green.

- [ ] **Step 5: Review the task diff and respect the commit boundary**

Run:

```bash
git diff -- src/init/provider_hooks/mod.rs
git -c core.whitespace=trailing-space,space-before-tab diff --check -- src/init/provider_hooks/mod.rs
```

Expected: only the merge behavior and its regression changed; the whitespace check exits 0. If and only if the user has explicitly authorized commits, stage this file and use an emoji conventional test/fix commit containing `codexctl-5n458`; otherwise leave it uncommitted and report the verified task boundary.

### Task 2: Exclude Definition-free Imperative Files from Aggregate Ownership and Retain Evidence

**Files:**
- Modify: `src/init/provider_hooks/mod.rs` in `inspect_provider_hooks_at_with_store_and_executable`
- Test: `src/init/provider_hooks/mod.rs` provider inspection tests
- Test: `src/doctor.rs` provider setup tests

**Interfaces:**
- Consumes: `ProviderHookFileInspection { state, ownership, .. }`, `combine_ownership`, and `check_provider_setup`.
- Produces: unchanged `ProviderHookInspection`; aggregate ownership ignores only imperative records with `ProviderHookFileState::Missing`, while the records themselves remain unchanged. Doctor includes evidence whenever any file record is non-current.
- Execution dependency: Task 1 must be complete before this task starts because both tasks modify `src/init/provider_hooks/mod.rs`.

**Acceptance Criteria:**
- Current Home Manager global plus an unrelated regular project file is `Current / HomeManager`.
- Stale Home Manager global plus that project file is `Stale / HomeManager` and receives declarative remediation.
- The unrelated project record remains `Imperative / Missing` evidence.
- That project record remains visible when the aggregate provider row is current.
- A Home Manager `Missing` record retains Home Manager ownership and declarative remediation.
- A genuine current global/project duplicate remains `Duplicate / Mixed`.
- Invalid and unsupported project candidates remain ownership-bearing failures.

- [ ] **Step 1: Add failing ownership aggregation regressions**

Add a test beside `provider_inspection_preserves_failure_precedence_across_sources`. Use the existing Home Manager fixture helpers and write a project file containing only an unrelated hook:

```rust
#[cfg(unix)]
#[test]
fn provider_inspection_ignores_definition_free_project_file_for_ownership() {
    for stale in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let store = temp.path().join("nix/store");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let mut bytes = home_manager_schema_variant_bytes(
            &home,
            &project,
            AgentProvider::Claude,
            Path::new("cbrain"),
        );
        if stale {
            bytes = String::from_utf8(bytes)
                .unwrap()
                .replacen("--lifecycle-hook", "--lifecycle-hook --changed", 1)
                .into_bytes();
        }
        write_home_manager_provider_file(
            &store,
            &home,
            &project,
            AgentProvider::Claude,
            &bytes,
        );
        let project_path = provider_path(
            AgentProvider::Claude,
            HookScope::Project,
            &home,
            &project,
        );
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(
            &project_path,
            br#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"bd prime --hook-json"}]}]}}"#,
        )
        .unwrap();

        let inspection = inspect_provider_hooks_at_with_store(
            AgentProvider::Claude,
            &home,
            &project,
            &store,
        );

        assert_eq!(
            inspection.state,
            if stale {
                ProviderHookState::Stale
            } else {
                ProviderHookState::Current
            }
        );
        assert_eq!(inspection.ownership, ProviderHookOwnership::HomeManager);
        let project_file = inspection
            .files
            .iter()
            .find(|file| file.path == project_path)
            .unwrap();
        assert_eq!(project_file.state, ProviderHookFileState::Missing);
        assert_eq!(project_file.ownership, ProviderHookOwnership::Imperative);
    }
}
```

- [ ] **Step 2: Run the ownership regression and verify it fails**

Run:

```bash
nix develop path:. --command cargo test provider_inspection_ignores_definition_free_project_file_for_ownership -- --nocapture
```

Expected: FAIL because aggregate ownership is `Mixed` before the correction.

- [ ] **Step 3: Ignore only imperative missing records during aggregate ownership reduction**

Change the ownership fold in `inspect_provider_hooks_at_with_store_and_executable`:

```rust
let ownership = files.iter().fold(
    ProviderHookOwnership::Absent,
    |ownership, file| {
        if file.state == ProviderHookFileState::Missing
            && file.ownership == ProviderHookOwnership::Imperative
        {
            ownership
        } else {
            combine_ownership(ownership, file.ownership)
        }
    },
);
```

Do not change the construction of `ProviderHookFileInspection`, `combine_ownership`, or state aggregation. Add a regression proving a Home Manager symlink with no cbrain definition remains aggregate `HomeManager / Missing`.

- [ ] **Step 4: Add Doctor remediation evidence coverage**

Add a `src/doctor.rs` test near `non_current_provider_rows_render_stable_file_evidence`:

```rust
#[test]
fn provider_setup_stale_home_manager_definition_with_unrelated_project_file_uses_declarative_repair() {
    let check = check_provider_setup(
        AgentProvider::Claude,
        ProviderSetupEvidence {
            recorded: true,
            executable_available: true,
            hooks: ProviderHookInspection {
                state: ProviderHookState::Stale,
                ownership: ProviderHookOwnership::HomeManager,
                files: vec![
                    provider_file(
                        "/home/example/.claude/settings.json",
                        HookScope::Global,
                        ProviderHookFileState::Stale,
                        ProviderHookOwnership::HomeManager,
                        Some(ProviderHookDiagnosticReason::ContractMismatch),
                    ),
                    provider_file(
                        "/work/project/.claude/settings.json",
                        HookScope::Project,
                        ProviderHookFileState::Missing,
                        ProviderHookOwnership::Imperative,
                        None,
                    ),
                ],
            },
        },
    );

    assert_eq!(check.status, CheckStatus::Fail);
    let hint = check.fix_hint.unwrap();
    assert!(hint.contains("Home Manager"));
    assert!(!hint.contains("duplicate scope"));
    assert_eq!(
        check.evidence.unwrap().provider_files[1].state,
        ProviderHookFileState::Missing
    );
}
```

- [ ] **Step 5: Run focused aggregation, remediation, and safety tests**

Run:

```bash
nix develop path:. --command cargo test provider_inspection_ignores_definition_free_project_file_for_ownership -- --nocapture
nix develop path:. --command cargo test provider_setup_stale_home_manager_definition_with_unrelated_project_file_uses_declarative_repair -- --nocapture
nix develop path:. --command cargo test provider_inspection_preserves_failure_precedence_across_sources -- --nocapture
nix develop path:. --command cargo test provider_inspection_reports_mixed_codex_and_claude_sources -- --nocapture
nix develop path:. --command cargo test provider_setup -- --test-threads=1
```

Expected: all commands exit 0. The existing true duplicate stays `Duplicate / Mixed`, and unsafe/invalid precedence tests remain unchanged.

- [ ] **Step 6: Review the task diff and respect the commit boundary**

Run:

```bash
git diff -- src/init/provider_hooks/mod.rs src/doctor.rs
git -c core.whitespace=trailing-space,space-before-tab diff --check -- src/init/provider_hooks/mod.rs src/doctor.rs
```

Expected: the diff contains only ownership reduction and the provider-inspection and Doctor regressions shown above. If and only if the user has explicitly authorized commits, stage the two files and use an emoji conventional fix commit containing `codexctl-5n458`; otherwise leave them uncommitted and report the verified task boundary.

### Task 3: Report Authoritative Codex Hook Trust

**Files:**
- Modify: `src/init/hooks.rs`
- Modify: `src/doctor.rs`
- Test: `src/init/hooks.rs`
- Test: `src/doctor.rs`

**Interfaces:**
- Consumes: Codex app-server `hooks/list` as the trust authority.
- Produces: a Pass, Advisory, or Skipped `Codex hook trust` check without reading or writing `trusted_hash` directly.
- Execution dependency: Task 2 must be complete so the final Doctor behavior is tested as one candidate.

**Acceptance Criteria:**
- Eight enabled cbrain hooks reported as `trusted` or `managed` produce Pass.
- Any `untrusted` or `modified` enabled cbrain hook produces Advisory and names the affected event.
- Missing Codex, timeout, malformed JSON-RPC, protocol errors, and unknown trust states produce a bounded trust-unavailable Advisory.
- A successful authoritative response with no enabled cbrain definition produces Skipped.
- The probe never writes config, approves hooks, or derives trust hashes.

- [ ] **Step 1: Add failing trust-result and provider-evidence tests**

Replace `current_definitions_pass_while_trust_remains_advisory` with table-driven assertions over injected authoritative hook-list results. Cover `trusted`, `managed`, `untrusted`, `modified`, empty, malformed, and unavailable results. Add a provider setup assertion that a current aggregate row still includes an `Imperative / Missing` file record.

- [ ] **Step 2: Run the focused tests red**

Run:

```bash
nix develop path:. --command cargo test codex_hook_trust -- --test-threads=1
nix develop path:. --command cargo test current_provider_setup_retains_non_current_file_evidence -- --nocapture
```

Expected: the old unconditional advisory and current-row evidence suppression fail the new assertions.

- [ ] **Step 3: Add the bounded authoritative probe**

In `src/init/hooks.rs`, expose only the existing event-aware cbrain-command classification needed to filter app-server hook rows and remove the misleading `trust_unverified` field.

In `src/doctor.rs`:

- spawn `codex app-server --stdio` with piped stdin/stdout and discarded stderr;
- complete `initialize`, wait for its response, send `initialized`, then request `hooks/list` for the active cwd;
- read size-capped JSON lines through a bounded channel with one shared five-second deadline, ignoring unrelated notifications;
- terminate and reap the owned child on success, error, or timeout;
- parse only enabled command rows that target cbrain lifecycle, permission, or recovery flags;
- map `trusted` and `managed` to Pass, `untrusted` and `modified` to Advisory, and every unavailable/unknown response to fail-closed Advisory.

Keep process execution behind an injected program/probe boundary so result mapping and timeout cleanup are deterministic in tests.

- [ ] **Step 4: Run focused trust and provider tests green**

Run:

```bash
nix develop path:. --command cargo test codex_hook_trust -- --test-threads=1
nix develop path:. --command cargo test current_provider_setup_retains_non_current_file_evidence -- --nocapture
nix develop path:. --command cargo test provider_setup -- --test-threads=1
```

Expected: all commands exit 0, with no unconditional `/hooks` advisory for trusted definitions.

- [ ] **Step 5: Run one live read-only acceptance probe**

Run the built candidate's Doctor against the installed Codex configuration without changing trust state. Confirm `Codex hook trust` is Pass and the provider evidence remains stable. This is local runtime evidence only; installed Home Manager activation remains separately authorized.

- [ ] **Step 6: Review scope and whitespace**

Run:

```bash
git diff -- src/init/hooks.rs src/init/provider_hooks/mod.rs src/doctor.rs
git -c core.whitespace=trailing-space,space-before-tab diff --check -- src/init/hooks.rs src/init/provider_hooks/mod.rs src/doctor.rs
```

Expected: only the approved comparison, ownership, evidence, and trust behavior changed.

### Task 4: Verify the Repository Candidate

**Files:**
- Verify: `src/init/provider_hooks/mod.rs`
- Verify: `src/doctor.rs`
- Verify: `.internal/specs/2026-08-12-claude-home-manager-comparison-remediation-design.md`
- Verify: `.internal/plans/2026-08-12-claude-home-manager-comparison-remediation.md`

**Interfaces:**
- Consumes: the completed Task 1, Task 2, and Task 3 behavior.
- Produces: fresh focused, workspace, formatting, lint, build, Home Manager, and packaged VM evidence for the repository candidate.
- Execution dependency: Tasks 1 through 3 must all be complete.

**Acceptance Criteria:**
- Focused provider and Doctor regressions pass.
- Serial workspace tests, formatting, Clippy with denied warnings, and workspace build pass.
- Home Manager module and storage-security VM checks pass.
- The final diff is limited to the approved design and contains no whitespace errors.
- Installed activation remains unperformed until separately authorized.

- [ ] **Step 1: Run the focused regression set serially**

Run:

```bash
nix develop path:. --command cargo test provider_inspection -- --test-threads=1
nix develop path:. --command cargo test provider_setup -- --test-threads=1
nix develop path:. --command cargo test codex_hook_trust -- --test-threads=1
```

Expected: both commands exit 0 with no failed tests.

- [ ] **Step 2: Run the full serial Rust suite**

Run:

```bash
nix develop path:. --command cargo test --workspace --all-targets -- --test-threads=1
```

Expected: exit 0. If an unchanged recurrent failure appears, isolate it with an exact focused rerun and report it separately; do not weaken or skip the serial gate.

- [ ] **Step 3: Run formatting, Clippy, and build gates**

Run:

```bash
nix develop path:. --command cargo fmt --all
nix develop path:. --command cargo fmt --all -- --check
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
nix fmt -- --check .
```

Expected: every command exits 0 with no formatting diff or Clippy warning.

- [ ] **Step 4: Run packaged Home Manager checks**

Resolve the current system once and use the resulting literal attribute in both commands:

```bash
nix eval --raw --impure --expr builtins.currentSystem
nix build .#checks.x86_64-linux.home-manager-module --no-link
nix build .#checks.x86_64-linux.storage-security-vm --no-link
```

Expected on the current `x86_64-linux` host: all commands exit 0. If the evaluated system differs, replace only `x86_64-linux` with that exact output.

- [ ] **Step 5: Audit the final scope**

Run:

```bash
git status --short
git diff --stat
git diff -- src/init/provider_hooks/mod.rs src/doctor.rs
git -c core.whitespace=trailing-space,space-before-tab diff --check
```

Expected: only the approved source/tests and the uncommitted spec/plan are present; no unrelated file is staged or modified, and the whitespace check exits 0.

- [ ] **Step 6: Stop at the installed-acceptance boundary**

Report the repository evidence and request explicit authorization before changing `nix-configs`, rebuilding or activating Home Manager, or rerunning installed acceptance against a new generation. Do not close `codexctl-5n458` from repository-only evidence.

## Stress Test Results: Claude Home Manager Comparison and Remediation

### Resolved Decisions

- Fix the shared merge behavior rather than adding a Claude-only normalization path: preserve matchers that were empty before cbrain processed them, while still removing entries made empty by exact managed-handler removal.
- Exclude only imperative `Missing` records from aggregate ownership. A Home Manager `Missing` record remains declarative, while every per-file record stays in evidence.
- Attach provider file evidence whenever any file record is non-current, even when the aggregate provider row passes as current.
- Use Codex's bounded `hooks/list` app-server response as the trust authority; do not copy the normalized hash or linked-worktree rules.
- Fail closed to a trust-unavailable Advisory on spawn, protocol, parse, unknown-state, or timeout failures, and always terminate the owned child.
- Preserve existing fail-closed state precedence and defer broader mixed `Stale + Current` remediation because it is not part of the installed regression.
- Test both the direct semantic comparison and the downstream provider inspection against the installed Home Manager Claude shape.
- Execute Tasks 1 through 4 sequentially because the implementation tasks overlap Doctor/provider discovery and the final task consumes all results.
- Keep Home Manager topology recognition and imperative mutation paths unchanged.
- Use explicit workspace-wide Rust gates and retain the separate installed-acceptance authorization boundary.
- Retain the scoped pre-stress-test Git stash until implementation finishes or the user explicitly authorizes its removal.
- Constrain acceptance to the production `lib.mkAfter` ordering; generalized order-insensitive comparison would require a separate design that preserves duplicate exact-handler detection.

### Changes Made

- Added direct `compare_provider_hook` assertions to the installed-shape regression and made focused pass expectations Cargo-harness neutral.
- Added explicit Task 1 to Task 2 to Task 3 execution dependencies.
- Renamed the Doctor regression for inclusion in the `provider_setup` filter and strengthened final commands to workspace-wide tests and Clippy, with formatting applied before the check.
- Made the installed matcher ordering explicit in the spec and plan, and added fixture assertions for the empty matcher followed by the cbrain recovery matcher.

### Deferred / Parking Lot

- Mixed `Stale + Current` remediation remains unchanged; this plan fixes the observed definition-free project scope without redesigning every mixed-state hint.
- Arbitrary hand-reordering of exact and unrelated matchers remains outside this production-shaped fix.
- Writing or repairing Codex trust state remains outside scope; Doctor only reports Codex's authoritative status.
- Installed Home Manager activation, `nix-configs` changes, and Bead closure remain separately authorized actions after repository and packaged verification.
- `stash@{0}` remains available as the pre-stress-test restore point.

### Confidence Assessment

- Overall: High.
- Areas of concern: the final installed result still depends on a separately authorized Home Manager rebuild and must be checked from `nix-configs`; repository and VM success alone do not prove production acceptance.
