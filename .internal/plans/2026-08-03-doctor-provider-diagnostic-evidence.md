# Doctor Provider Diagnostic Evidence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make every non-current `cbrain doctor` provider setup row identify the relevant provider paths, scopes, ownership, states, and bounded failure reasons in concise human output and stable structured JSON.

**Architecture:** Preserve one typed evidence record per deduplicated provider candidate inside `ProviderHookInspection`, then derive the existing aggregate state and ownership exclusively from those records. Doctor maps those internal records into optional serializable `Check` evidence only when the final provider setup state is not current; the filesystem classifier remains the only source of truth and mutation paths remain unchanged.

**Tech Stack:** Rust 2024, Serde/serde_json, Ratatui-independent CLI rendering, Cargo workspace tests, Nix/Home Manager integration checks.

## Global Constraints

- Do not change provider hook schemas, accepted provider definitions, setup severity, failure precedence, or remediation ownership.
- Do not accept any new symlink or Home Manager topology.
- Do not expose raw I/O errors, file contents, parsed JSON fragments, commands, symlink targets, or raw path bytes.
- Preserve every deduplicated inspection candidate in deterministic order: global first, then project directories from detected root to current directory.
- Use full lossy UTF-8 paths with an explicit `path_lossy` marker; escape terminal control and bidirectional-format characters in human output.
- Populate evidence whenever final `ProviderSetupState` is not `Current`; omit it from final-current/pass rows and unrelated checks.
- Keep imperative install/remove, transaction, rollback, recovery, and `read_managed_file` behavior unchanged.
- Do not commit, push, publish, or merge without explicit authorization.

---

### Task 1: Preserve typed per-file classifier evidence

**Files:**
- Modify: `src/init/provider_hooks/mod.rs:42-294`
- Test: `src/init/provider_hooks/mod.rs:2528-3340`

**Interfaces:**
- Consumes: existing `HookScope`, `ProviderHookState`, `ProviderHookOwnership`, `read_provider_file_for_inspection`, `compare_provider_hook`, and `state_from_comparison`.
- Produces: `ProviderHookFileState`, `ProviderHookDiagnosticReason`, `ProviderHookFileInspection`, and `ProviderHookInspection.files: Vec<ProviderHookFileInspection>` for Doctor.

**Acceptance Criteria:**
- Every deduplicated global/project candidate produces one record containing its exact `PathBuf`, scope, ownership, state, and optional closed reason.
- Aggregate state and ownership are derived exclusively from the records with the existing precedence.
- Mixed Home Manager global plus regular project definitions for Codex and Claude expose both current records in global-to-project order.
- Invalid Home Manager Antigravity content retains Home Manager ownership and reports `MalformedContent`.
- Unsupported topology, unreadable input, malformed content, and contract mismatch are distinct.
- Existing fail-closed inspection and imperative mutation tests remain unchanged in meaning and pass.

- [ ] **Step 0: Record the focused pre-change baseline**

Run before editing production code:

```bash
git status --short
nix develop path:. --command cargo test init::provider_hooks::tests::provider_inspection_distinguishes_missing_current_stale_and_invalid -- --exact --nocapture
nix develop path:. --command cargo test init::provider_hooks::tests::mutation_staging_still_rejects_home_manager_symlinks -- --exact --nocapture
nix develop path:. --command cargo test doctor::tests::provider_setup_matrix_maps_internal_states_to_existing_severity -- --exact --nocapture
nix eval --raw --impure --expr builtins.currentSystem
nix build .#checks.<system>.home-manager-module --no-link
```

Expected: only the approved spec and plan are untracked; all focused tests execute one test and pass; the existing packaged Home Manager check passes. Record any pre-existing failure before changing code.

- [ ] **Step 1: Add failing evidence tests for mixed scopes and invalid Antigravity content**

Add Unix tests beside the existing Home Manager inspection tests. Use the existing `write_home_manager_provider_file`, `exact_provider_bytes`, `home_manager_schema_variant_bytes`, and `provider_path` helpers. Assert full records rather than only aggregate fields:

```rust
#[cfg(unix)]
#[test]
fn provider_inspection_reports_mixed_codex_and_claude_sources() {
    for provider in [AgentProvider::Codex, AgentProvider::Claude] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let store = temp.path().join("nix/store");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        let bytes = exact_provider_bytes(&home, &project, provider);
        write_home_manager_provider_file(&store, &home, &project, provider, &bytes);
        let project_path = provider_path(provider, HookScope::Project, &home, &project);
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(&project_path, &bytes).unwrap();

        let inspection =
            inspect_provider_hooks_at_with_store(provider, &home, &project, &store);

        assert_eq!(inspection.state, ProviderHookState::Duplicate);
        assert_eq!(inspection.ownership, ProviderHookOwnership::Mixed);
        assert_eq!(
            inspection.files,
            vec![
                ProviderHookFileInspection {
                    path: provider_path(provider, HookScope::Global, &home, &project),
                    scope: HookScope::Global,
                    state: ProviderHookFileState::Current,
                    ownership: ProviderHookOwnership::HomeManager,
                    reason: None,
                },
                ProviderHookFileInspection {
                    path: project_path,
                    scope: HookScope::Project,
                    state: ProviderHookFileState::Current,
                    ownership: ProviderHookOwnership::Imperative,
                    reason: None,
                },
            ]
        );
    }
}

#[cfg(unix)]
#[test]
fn provider_inspection_explains_invalid_home_manager_antigravity_content() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let store = temp.path().join("nix/store");
    std::fs::create_dir_all(&project).unwrap();
    write_home_manager_provider_file(
        &store,
        &home,
        &project,
        AgentProvider::Antigravity,
        b"[",
    );

    let inspection = inspect_provider_hooks_at_with_store(
        AgentProvider::Antigravity,
        &home,
        &project,
        &store,
    );

    assert_eq!(inspection.state, ProviderHookState::Invalid);
    assert_eq!(inspection.ownership, ProviderHookOwnership::HomeManager);
    assert_eq!(inspection.files.len(), 1);
    assert_eq!(
        inspection.files[0].reason,
        Some(ProviderHookDiagnosticReason::MalformedContent)
    );
}
```

- [ ] **Step 2: Run the focused tests and verify they fail for missing types/fields**

Run:

```bash
nix develop path:. --command cargo test init::provider_hooks::tests::provider_inspection_reports_mixed_codex_and_claude_sources -- --exact --nocapture
nix develop path:. --command cargo test init::provider_hooks::tests::provider_inspection_explains_invalid_home_manager_antigravity_content -- --exact --nocapture
```

Expected: compilation fails because `ProviderHookFileInspection`, `ProviderHookDiagnosticReason`, and `ProviderHookInspection.files` do not exist.

- [ ] **Step 3: Add typed file evidence and derive aggregates from it**

Add these internal types near the existing state and ownership enums:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderHookFileState {
    Missing,
    Current,
    Stale,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderHookDiagnosticReason {
    UnsupportedTopology,
    Unreadable,
    MalformedContent,
    ContractMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderHookFileInspection {
    pub(crate) path: PathBuf,
    pub(crate) scope: HookScope,
    pub(crate) state: ProviderHookFileState,
    pub(crate) ownership: ProviderHookOwnership,
    pub(crate) reason: Option<ProviderHookDiagnosticReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderHookInspection {
    pub(crate) state: ProviderHookState,
    pub(crate) ownership: ProviderHookOwnership,
    pub(crate) files: Vec<ProviderHookFileInspection>,
}
```

Add the same `Serialize`, `Deserialize`, and snake-case derives to `ProviderHookOwnership` and `HookScope`. Give `ProviderHookFileState`, `ProviderHookOwnership`, `HookScope`, and `ProviderHookDiagnosticReason` exhaustive `as_str()` methods whose values exactly match their Serde names; Doctor will reuse these per-file types and labels instead of maintaining parallel enums. Keep aggregate `ProviderHookState` internal and non-serialized because its `Duplicate` variant is invalid for an individual file.

Change `InspectedProviderFile::Invalid` to carry `reason`. Replace `invalid_inspected_file` with:

```rust
fn invalid_inspected_file(
    ownership: ProviderHookOwnership,
    reason: ProviderHookDiagnosticReason,
) -> InspectedProviderFile {
    InspectedProviderFile::Invalid { ownership, reason }
}
```

Preserve the current `read_provider_file_for_inspection` branch conditions and order. Map existing branches only:

```rust
// symlink_metadata/read_link/open/metadata/read/size failures
ProviderHookDiagnosticReason::Unreadable

// unsupported file type, project symlink, lexical target shape, store/suffix,
// symlinked ancestor, nested target symlink, or non-file target
ProviderHookDiagnosticReason::UnsupportedTopology
```

Change the single-file classifier to return a `ProviderHookFileInspection` and map semantic results exactly:

```rust
fn inspect_provider_hook_at(
    provider: AgentProvider,
    scope: HookScope,
    path: &Path,
    nix_store_root: &Path,
    executable: &Path,
) -> ProviderHookFileInspection {
    let (state, ownership, reason) = match read_provider_file_for_inspection(
        path,
        provider,
        scope,
        nix_store_root,
    ) {
        InspectedProviderFile::Missing => (
            ProviderHookFileState::Missing,
            ProviderHookOwnership::Absent,
            None,
        ),
        InspectedProviderFile::Readable { bytes, ownership } => {
            let contract = match ownership {
                ProviderHookOwnership::HomeManager => {
                    ProviderHookComparisonContract::HomeManager { executable }
                }
                _ => ProviderHookComparisonContract::Imperative,
            };
            match compare_provider_hook(provider, Some(&bytes), false, contract) {
                Ok(comparison) => {
                    let state = state_from_comparison(&comparison, Some(&bytes));
                    let reason = (state == ProviderHookFileState::Stale)
                        .then_some(ProviderHookDiagnosticReason::ContractMismatch);
                    (state, ownership, reason)
                }
                Err(_) => (
                    ProviderHookFileState::Invalid,
                    ownership,
                    Some(ProviderHookDiagnosticReason::MalformedContent),
                ),
            }
        }
        InspectedProviderFile::Invalid { ownership, reason } => {
            (ProviderHookFileState::Invalid, ownership, Some(reason))
        }
    };
    ProviderHookFileInspection {
        path: path.to_path_buf(),
        scope,
        state,
        ownership,
        reason,
    }
}
```

Change `state_from_comparison` to return `ProviderHookFileState`. In `inspect_provider_hooks_at_with_store_and_executable`, collect `files`, fold ownership from `file.ownership`, and calculate aggregate `ProviderHookState` from `file.state` using the existing invalid/stale/current-count precedence. Return `{ state, ownership, files }`. Do not create a second classification path.

- [ ] **Step 4: Update existing inspection literals and add reason-matrix regressions**

Update existing `ProviderHookInspection` expectations in `src/init/provider_hooks/mod.rs` with exact `files` values for filesystem fixtures. In the `doctor.rs` test module, add `synthetic_inspection(state, ownership)` returning an empty file vector and use it only for synthetic state-matrix inputs; production inspections must never discard real records.

Extend the existing unsafe-symlink fixture table to assert:

```rust
let expected_reason = match fixture {
    Fixture::Broken => ProviderHookDiagnosticReason::Unreadable,
    Fixture::NestedSymlink
    | Fixture::Directory
    | Fixture::ProjectLocal
    | Fixture::Relative
    | Fixture::Parent
    | Fixture::CurrentDirectory
    | Fixture::NonUtf8
    | Fixture::NonStore
    | Fixture::WrongSuffix
    | Fixture::RepeatedSeparator => ProviderHookDiagnosticReason::UnsupportedTopology,
};
assert_eq!(inspection.files[0].reason, Some(expected_reason));
```

Add a readable stale definition assertion for `ContractMismatch`, invalid JSON and non-object assertions for `MalformedContent`, and an oversized recognized Home Manager target assertion for `Unreadable`. Keep existing `mutation_staging_still_rejects_home_manager_symlinks` unchanged.

- [ ] **Step 5: Run classifier and mutation-safety tests**

Run:

```bash
nix develop path:. --command cargo test provider_inspection -- --nocapture
nix develop path:. --command cargo test init::provider_hooks::tests::mutation_staging_still_rejects_home_manager_symlinks -- --exact --nocapture
```

Expected: all matching tests pass; the mutation test continues to reject recognized Home Manager and arbitrary symlinks for both setup and removal.

- [ ] **Step 6: Review the Task 1 diff without committing**

Run:

```bash
git diff --check
git diff -- src/init/provider_hooks/mod.rs src/doctor.rs
```

Expected: no whitespace errors; all changed classifier branches trace to typed evidence or required literal updates. Do not commit without explicit authorization.

---

### Task 2: Expose stable evidence in Doctor JSON and human output

**Files:**
- Modify: `src/doctor.rs:27-211`
- Modify: `src/doctor.rs:430-518`
- Test: `src/doctor.rs:1800-2605`
- Modify/Test: `nix/tests/home-manager-module.nix:446-760`

**Interfaces:**
- Consumes: Task 1 `ProviderHookInspection.files`, `ProviderHookFileInspection`, `ProviderHookDiagnosticReason`, `ProviderHookState`, `ProviderHookOwnership`, and `HookScope`.
- Produces: optional `Check.evidence`, stable `evidence.provider_files` JSON, and escaped human evidence lines.

**Acceptance Criteria:**
- Evidence is present for every final provider setup state except `Current`, including current definitions with a missing executable.
- Evidence is omitted from final-current/pass provider rows and unrelated checks.
- JSON fields are stable snake-case values and legacy checks without `evidence` deserialize.
- Paths expose full lossy UTF-8 plus `path_lossy`; human output escapes control and bidirectional-format characters.
- Duplicate rows name both conflicting definitions and scopes in deterministic order.
- Raw errors, contents, commands, targets, and raw bytes never appear.
- Packaged CLI fixtures cover exact mixed Codex/Claude paths and non-disclosing malformed Home Manager Antigravity content in the same red-green cycle.

- [ ] **Step 1: Write failing JSON, filtering, and human rendering tests**

Replace the old assertion that provider JSON contains no paths with exact structured assertions. Add a helper that creates synthetic file records, then cover duplicate and unavailable states:

```rust
fn provider_file(
    path: &str,
    scope: HookScope,
    state: ProviderHookFileState,
    ownership: ProviderHookOwnership,
    reason: Option<ProviderHookDiagnosticReason>,
) -> ProviderHookFileInspection {
    ProviderHookFileInspection {
        path: PathBuf::from(path),
        scope,
        state,
        ownership,
        reason,
    }
}

#[test]
fn non_current_provider_rows_render_stable_file_evidence() {
    let check = check_provider_setup(
        AgentProvider::Claude,
        ProviderSetupEvidence {
            recorded: true,
            executable_available: true,
            hooks: ProviderHookInspection {
                state: ProviderHookState::Duplicate,
                ownership: ProviderHookOwnership::Mixed,
                files: vec![
                    provider_file(
                        "/home/example/.claude/settings.json",
                        HookScope::Global,
                        ProviderHookFileState::Current,
                        ProviderHookOwnership::HomeManager,
                        None,
                    ),
                    provider_file(
                        "/work/project/.claude/settings.json",
                        HookScope::Project,
                        ProviderHookFileState::Current,
                        ProviderHookOwnership::Imperative,
                        None,
                    ),
                ],
            },
        },
    );

    let value = serde_json::to_value(&check).unwrap();
    assert_eq!(value["evidence"]["provider_files"][0]["scope"], "global");
    assert_eq!(
        value["evidence"]["provider_files"][0]["ownership"],
        "home_manager"
    );
    assert_eq!(value["evidence"]["provider_files"][1]["scope"], "project");
    assert_eq!(value["evidence"]["provider_files"][1]["state"], "current");
    assert_eq!(value["evidence"]["provider_files"][1]["path_lossy"], false);
    let human = render_checks(&[check]);
    assert!(human.contains("global"));
    assert!(human.contains("/home/example/.claude/settings.json"));
    assert!(human.contains("project"));
    assert!(human.contains("/work/project/.claude/settings.json"));
}
```

Also add:

```rust
#[test]
fn current_setup_omits_evidence_but_unavailable_current_hooks_include_it() {
    for (executable_available, expects_evidence) in [(true, false), (false, true)] {
        let check = check_provider_setup(
            AgentProvider::Codex,
            ProviderSetupEvidence {
                recorded: true,
                executable_available,
                hooks: ProviderHookInspection {
                    state: ProviderHookState::Current,
                    ownership: ProviderHookOwnership::Imperative,
                    files: vec![provider_file(
                        "/home/example/.codex/hooks.json",
                        HookScope::Global,
                        ProviderHookFileState::Current,
                        ProviderHookOwnership::Imperative,
                        None,
                    )],
                },
            },
        );
        assert_eq!(check.evidence.is_some(), expects_evidence);
        assert_eq!(
            serde_json::to_value(check).unwrap().get("evidence").is_some(),
            expects_evidence
        );
    }
}

#[test]
fn legacy_check_json_without_evidence_deserializes() {
    let check: Check = serde_json::from_str(
        r#"{"name":"Codex setup","status":"pass","message":"current","fix_hint":null}"#,
    )
    .unwrap();
    assert!(check.evidence.is_none());
}
```

For terminal safety, build a path containing `\n`, `\u{001b}`, `\u{202e}`, and ordinary `界`; assert the first three are escaped and `界` remains readable. On Unix, construct a non-UTF-8 path with `OsStringExt::from_vec`, assert `path_lossy == true`, and assert JSON serialization succeeds.

```rust
#[test]
fn human_provider_paths_escape_terminal_controls_and_bidi() {
    let escaped = escape_provider_path("/work/line\n\u{001b}\u{202e}界.json");
    assert!(escaped.contains("\\u{a}"));
    assert!(escaped.contains("\\u{1b}"));
    assert!(escaped.contains("\\u{202e}"));
    assert!(escaped.contains('界'));
    assert!(!escaped.contains('\n'));
    assert!(!escaped.contains('\u{001b}'));
    assert!(!escaped.contains('\u{202e}'));
}

#[cfg(unix)]
#[test]
fn non_utf8_provider_path_serializes_with_lossy_marker() {
    use std::os::unix::ffi::OsStringExt;
    let file = ProviderHookFileInspection {
        path: PathBuf::from(std::ffi::OsString::from_vec(
            b"/work/invalid-\xff.json".to_vec(),
        )),
        scope: HookScope::Project,
        state: ProviderHookFileState::Invalid,
        ownership: ProviderHookOwnership::Unsupported,
        reason: Some(ProviderHookDiagnosticReason::UnsupportedTopology),
    };
    let evidence = ProviderFileEvidence::from(&file);
    assert!(evidence.path_lossy);
    assert!(serde_json::to_string(&evidence).is_ok());
}
```

- [ ] **Step 2: Add failing packaged CLI fixtures before Doctor implementation**

In `nix/tests/home-manager-module.nix`, add a derivation whose recognized Home Manager path contains non-object Antigravity JSON with a non-disclosure sentinel:

```nix
invalidAntigravityHomeManagerFiles =
  pkgs.runCommand "invalid-antigravity-home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    cp ${providerHomeManagerFiles}/.codex/hooks.json "$out/.codex/hooks.json"
    cp ${providerHomeManagerFiles}/.claude/settings.json "$out/.claude/settings.json"
    printf '%s\n' '["SECRET_PROVIDER_CONTENT"]' \
      > "$out/.gemini/config/hooks.json"
  '';
```

Extend the runCommand fixture after the existing all-current assertions. First assert passing setup rows omit evidence. Then generate exact imperative project files by temporarily using the project as `HOME`, restore the declarative home, and assert exact mixed paths:

```sh
jq -e '
  [.[] | select(.name | endswith(" setup"))]
  | all(.[]; has("evidence") | not)
' "$TMPDIR/doctor.json"

mixed_project="$TMPDIR/mixed-project"
mkdir -p "$mixed_project/.git"
HOME="$mixed_project" \
XDG_CONFIG_HOME="$TMPDIR/mixed-init-config" \
XDG_STATE_HOME="$TMPDIR/mixed-init-state" \
cbrain init codex claude \
  --non-interactive \
  --skip-brain \
  --skip-skills
export HOME="$fixture_home"
export XDG_CONFIG_HOME="$TMPDIR/config"
export XDG_STATE_HOME="$TMPDIR/state"
cd "$mixed_project"
mixed_status=0
cbrain doctor --json > "$TMPDIR/doctor-mixed.json" || mixed_status="$?"
test "$mixed_status" -eq 0 -o "$mixed_status" -eq 1

check_mixed_provider() {
  provider="$1"
  global_path="$2"
  project_path="$3"
  jq -e \
    --arg name "$provider setup" \
    --arg global "$global_path" \
    --arg project "$project_path" '
      .[]
      | select(.name == $name)
      | .status == "advisory"
        and (.evidence.provider_files | length == 2)
        and .evidence.provider_files[0] == {
          path: $global,
          path_lossy: false,
          scope: "global",
          ownership: "home_manager",
          state: "current"
        }
        and .evidence.provider_files[1] == {
          path: $project,
          path_lossy: false,
          scope: "project",
          ownership: "imperative",
          state: "current"
        }
    ' "$TMPDIR/doctor-mixed.json"
}
check_mixed_provider \
  Codex \
  "$fixture_home/.codex/hooks.json" \
  "$mixed_project/.codex/hooks.json"
check_mixed_provider \
  Claude \
  "$fixture_home/.claude/settings.json" \
  "$mixed_project/.claude/settings.json"
```

Create a second home pointed at `invalidAntigravityHomeManagerFiles`, reset XDG roots, run Doctor, and assert exact structured evidence plus non-disclosure:

```sh
invalid_home="$TMPDIR/invalid-home"
mkdir -p \
  "$invalid_home/.codex" \
  "$invalid_home/.claude" \
  "$invalid_home/.gemini/config" \
  "$TMPDIR/invalid-config" \
  "$TMPDIR/invalid-state"
ln -s ${invalidAntigravityHomeManagerFiles}/.codex/hooks.json \
  "$invalid_home/.codex/hooks.json"
ln -s ${invalidAntigravityHomeManagerFiles}/.claude/settings.json \
  "$invalid_home/.claude/settings.json"
ln -s ${invalidAntigravityHomeManagerFiles}/.gemini/config/hooks.json \
  "$invalid_home/.gemini/config/hooks.json"
export HOME="$invalid_home"
export XDG_CONFIG_HOME="$TMPDIR/invalid-config"
export XDG_STATE_HOME="$TMPDIR/invalid-state"
cd "$TMPDIR"
invalid_status=0
cbrain doctor --json > "$TMPDIR/doctor-invalid-antigravity.json" \
  || invalid_status="$?"
test "$invalid_status" -eq 1
jq -e --arg path "$invalid_home/.gemini/config/hooks.json" '
  .[]
  | select(.name == "Antigravity setup")
  | .status == "fail"
    and .evidence.provider_files == [{
      path: $path,
      path_lossy: false,
      scope: "global",
      ownership: "home_manager",
      state: "invalid",
      reason: "malformed_content"
    }]
' "$TMPDIR/doctor-invalid-antigravity.json"
! grep -F 'SECRET_PROVIDER_CONTENT' "$TMPDIR/doctor-invalid-antigravity.json"
```

- [ ] **Step 3: Run unit and packaged tests and verify the red state**

Run:

```bash
nix develop path:. --command cargo test doctor::tests::non_current_provider_rows_render_stable_file_evidence -- --exact --nocapture
nix develop path:. --command cargo test doctor::tests::legacy_check_json_without_evidence_deserializes -- --exact --nocapture
nix eval --raw --impure --expr builtins.currentSystem
nix build .#checks.<system>.home-manager-module --no-link
```

Expected: Rust test compilation fails because `Check.evidence` and its serialized types do not exist. The Nix fixture uses `doctorPackage` with `doCheck = false`, so it still builds the pre-change binary and then fails at the new `evidence` assertions. Both red tests remain in place for implementation.

- [ ] **Step 4: Add private serializable Doctor evidence types**

Add private types next to `Check` so the public row fields remain unchanged:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckEvidence {
    provider_files: Vec<ProviderFileEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProviderFileEvidence {
    path: String,
    path_lossy: bool,
    scope: HookScope,
    ownership: ProviderHookOwnership,
    state: ProviderHookFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<ProviderHookDiagnosticReason>,
}
```

Add the optional field to `Check`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
evidence: Option<CheckEvidence>,
```

Update every existing `Check` literal with `evidence: None`. Keep the field private; only rendering and tests inside `doctor.rs` need direct access.

- [ ] **Step 5: Convert internal evidence without re-inspection**

Reuse Task 1 enums directly; only convert the path without raw-byte exposure:

```rust
impl From<&ProviderHookFileInspection> for ProviderFileEvidence {
    fn from(file: &ProviderHookFileInspection) -> Self {
        let path = file.path.to_string_lossy();
        let path_lossy = matches!(path, std::borrow::Cow::Owned(_));
        Self {
            path: path.into_owned(),
            path_lossy,
            scope: file.scope,
            ownership: file.ownership,
            state: file.state,
            reason: file.reason,
        }
    }
}
```

In `check_provider_setup`, compute `ProviderSetupState` exactly as today, then set:

```rust
let evidence = (state != ProviderSetupState::Current).then(|| CheckEvidence {
    provider_files: evidence.hooks.files.iter().map(Into::into).collect(),
});
```

Do not gate on `ProviderHookState::Current`; current hooks plus a missing executable must still expose their inspected path.

- [ ] **Step 6: Render safe concise evidence lines**

Add a small helper that escapes only terminal-dangerous characters:

```rust
fn escape_provider_path(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for character in path.chars() {
        let dangerous_format = matches!(
            character,
            '\u{061c}' | '\u{200e}' | '\u{200f}'
                | '\u{200b}'..='\u{200d}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'
                | '\u{2066}'..='\u{2069}'
                | '\u{feff}'
        );
        if character.is_control() || dangerous_format {
            escaped.extend(character.escape_unicode());
        } else {
            escaped.push(character);
        }
    }
    escaped
}
```

Use the Task 1 enums' `as_str()` methods in `render_checks`. After the summary row and before `fix_hint`, render records without debug representations:

```rust
if let Some(evidence) = &c.evidence {
    for file in &evidence.provider_files {
        let mut classification = format!(
            "{}/{}/{}",
            file.scope.as_str(),
            file.ownership.as_str(),
            file.state.as_str(),
        );
        if let Some(reason) = file.reason {
            classification.push_str(", ");
            classification.push_str(reason.as_str());
        }
        if file.path_lossy {
            classification.push_str(", lossy path");
        }
        out.push_str(&format!(
            "      {} — {}\n",
            escape_provider_path(&file.path),
            classification,
        ));
    }
}
```

Name the local boolean `dangerous_format` rather than `bidi`, because it covers bidi and zero-width format characters. Extend `human_provider_paths_escape_terminal_controls_and_bidi` with carriage return, tab, zero-width joiner, word joiner, and BOM assertions while preserving ordinary CJK text.

For example, `ProviderHookDiagnosticReason::as_str` is exhaustive and does not serialize attacker-controlled text:

```rust
impl ProviderHookDiagnosticReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedTopology => "unsupported_topology",
            Self::Unreadable => "unreadable",
            Self::MalformedContent => "malformed_content",
            Self::ContractMismatch => "contract_mismatch",
        }
    }
}
```

Implement the corresponding exhaustive `as_str` matches for scope, ownership, and state in Task 1 using their declared snake-case variant names.

- [ ] **Step 7: Run Doctor tests and the broader provider setup matrix**

Run:

```bash
nix develop path:. --command cargo test doctor::tests::non_current_provider_rows_render_stable_file_evidence -- --exact --nocapture
nix develop path:. --command cargo test doctor::tests::legacy_check_json_without_evidence_deserializes -- --exact --nocapture
nix develop path:. --command cargo test provider_setup -- --nocapture
nix develop path:. --command cargo test doctor::tests::human_and_json_provider_evidence_is_deterministic_and_safe -- --exact --nocapture
nix build .#checks.<system>.home-manager-module --no-link
```

Expected: all Rust and packaged CLI assertions pass. Rename the legacy test to `human_and_json_provider_evidence_is_deterministic_and_safe` because the old name and assertions intentionally prohibited the paths now required by the contract.

- [ ] **Step 8: Review the Task 2 diff without committing**

Run:

```bash
git diff --check
git diff -- src/doctor.rs nix/tests/home-manager-module.nix
```

Expected: all 69 `Check` constructors explicitly omit or set evidence; only provider setup checks populate it. Do not commit without explicit authorization.

---

### Task 3: Run full verification and audit the completed change

**Files:**
- Verify: `src/init/provider_hooks/mod.rs`
- Verify: `src/doctor.rs`
- Verify: `nix/tests/home-manager-module.nix`

**Interfaces:**
- Consumes: Task 1 classifier evidence and Task 2 Doctor/unit/packaged CLI behavior.
- Produces: fresh full-gate evidence and a surgical final diff ready for review.

**Acceptance Criteria:**
- Focused classifier and Doctor tests execute the intended nonzero test counts and pass.
- Rust formatting, serial workspace tests, Clippy with warnings denied, and workspace build pass.
- Nix formatting and the packaged Home Manager module check pass.
- The final diff contains only approved files and no whitespace errors.
- No commit, push, sync, publish, or merge occurs without explicit authorization.

- [ ] **Step 1: Audit the packaged mixed-scope fixture from Task 2**

Confirm the Task 2 fixture uses the packaged imperative installer with the project directory temporarily acting as `HOME`, restores the declarative Home Manager `HOME`, and compares exact global/project paths. Do not replace it with copied Home Manager Claude JSON because the declarative contract intentionally differs from the imperative contract.

```sh
mixed_project="$TMPDIR/mixed-project"
mkdir -p "$mixed_project/.git"
HOME="$mixed_project" \
XDG_CONFIG_HOME="$TMPDIR/mixed-init-config" \
XDG_STATE_HOME="$TMPDIR/mixed-init-state" \
cbrain init codex claude \
  --non-interactive \
  --skip-brain \
  --skip-skills
export HOME="$fixture_home"
export XDG_CONFIG_HOME="$TMPDIR/config"
export XDG_STATE_HOME="$TMPDIR/state"
cd "$mixed_project"
mixed_status=0
cbrain doctor --json > "$TMPDIR/doctor-mixed.json" || mixed_status="$?"
test "$mixed_status" -eq 0 -o "$mixed_status" -eq 1

check_mixed_provider() {
  provider="$1"
  global_path="$2"
  project_path="$3"
  jq -e \
    --arg name "$provider setup" \
    --arg global "$global_path" \
    --arg project "$project_path" '
      .[]
      | select(.name == $name)
      | .status == "advisory"
        and (.evidence.provider_files | length == 2)
        and .evidence.provider_files[0].path == $global
        and .evidence.provider_files[0].path_lossy == false
        and .evidence.provider_files[0].scope == "global"
        and .evidence.provider_files[0].ownership == "home_manager"
        and .evidence.provider_files[0].state == "current"
        and .evidence.provider_files[1].path == $project
        and .evidence.provider_files[1].path_lossy == false
        and .evidence.provider_files[1].scope == "project"
        and .evidence.provider_files[1].ownership == "imperative"
        and .evidence.provider_files[1].state == "current"
    ' "$TMPDIR/doctor-mixed.json"
}
check_mixed_provider \
  Codex \
  "$fixture_home/.codex/hooks.json" \
  "$mixed_project/.codex/hooks.json"
check_mixed_provider \
  Claude \
  "$fixture_home/.claude/settings.json" \
  "$mixed_project/.claude/settings.json"
```

Also assert the original all-current JSON rows do not have `evidence`.

- [ ] **Step 2: Audit the packaged malformed-content non-disclosure fixture**

Confirm the Task 2 derivation name preserves the recognized Home Manager suffix and contains the non-object sentinel JSON:

```nix
invalidAntigravityHomeManagerFiles =
  pkgs.runCommand "invalid-antigravity-home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    cp ${providerHomeManagerFiles}/.codex/hooks.json "$out/.codex/hooks.json"
    cp ${providerHomeManagerFiles}/.claude/settings.json "$out/.claude/settings.json"
    printf '%s\n' '["SECRET_PROVIDER_CONTENT"]' \
      > "$out/.gemini/config/hooks.json"
  '';
```

Point a second temporary home at those exact files, reset the XDG directories, run packaged `cbrain doctor --json`, and assert:

```sh
invalid_home="$TMPDIR/invalid-home"
mkdir -p \
  "$invalid_home/.codex" \
  "$invalid_home/.claude" \
  "$invalid_home/.gemini/config" \
  "$TMPDIR/invalid-config" \
  "$TMPDIR/invalid-state"
ln -s ${invalidAntigravityHomeManagerFiles}/.codex/hooks.json \
  "$invalid_home/.codex/hooks.json"
ln -s ${invalidAntigravityHomeManagerFiles}/.claude/settings.json \
  "$invalid_home/.claude/settings.json"
ln -s ${invalidAntigravityHomeManagerFiles}/.gemini/config/hooks.json \
  "$invalid_home/.gemini/config/hooks.json"
export HOME="$invalid_home"
export XDG_CONFIG_HOME="$TMPDIR/invalid-config"
export XDG_STATE_HOME="$TMPDIR/invalid-state"
cd "$TMPDIR"
invalid_status=0
cbrain doctor --json > "$TMPDIR/doctor-invalid-antigravity.json" \
  || invalid_status="$?"
test "$invalid_status" -eq 1
```

Confirm the assertions compare the exact invalid path, require `path_lossy: false`, and reject sentinel disclosure:

```sh
jq -e --arg path "$invalid_home/.gemini/config/hooks.json" '
  .[]
  | select(.name == "Antigravity setup")
  | .status == "fail"
    and .evidence.provider_files == [{
      path: $path,
      path_lossy: false,
      scope: "global",
      ownership: "home_manager",
      state: "invalid",
      reason: "malformed_content"
    }]
' "$TMPDIR/doctor-invalid-antigravity.json"
! grep -F 'SECRET_PROVIDER_CONTENT' "$TMPDIR/doctor-invalid-antigravity.json"
```

Do not change the generated module definitions themselves.

- [ ] **Step 3: Run the packaged Home Manager check after Tasks 1-2**

Determine the current system without shell substitution in the test command:

```bash
nix eval --raw --impure --expr builtins.currentSystem
```

Then run, substituting the printed system literally:

```bash
nix build .#checks.<system>.home-manager-module --no-link
```

Expected: the all-current, exact mixed-path, malformed-content, and non-disclosure assertions all pass.

- [ ] **Step 4: Run focused and full Rust verification after Tasks 1-2**

Run serially to avoid target-directory contention:

```bash
nix develop path:. --command cargo fmt
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test --workspace -- --test-threads=1
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
```

Expected: every command exits 0 with no warnings promoted by Clippy.

- [ ] **Step 5: Run Nix formatting and packaged Home Manager verification**

Run:

```bash
nix fmt nix/tests/home-manager-module.nix
nix fmt -- --check nix/tests/home-manager-module.nix
nix build .#checks.<system>.home-manager-module --no-link
```

Expected: both exit 0. The packaged check proves current rows omit evidence, mixed Codex/Claude rows identify both scopes, and malformed Home Manager Antigravity identifies its path and reason.

- [ ] **Step 6: Inspect the final surgical diff and tracker state**

Run:

```bash
git diff --check
git status --short
git diff --stat
bd -C /home/alexander/.beads-planning show codexctl-32hkq
```

Expected: changes are limited to the approved spec/plan, `src/init/provider_hooks/mod.rs`, `src/doctor.rs`, and `nix/tests/home-manager-module.nix`; `codexctl-32hkq` remains in progress until all checks pass. Do not commit, push, or sync without explicit authorization.

## Stress Test Results: Doctor provider diagnostic evidence implementation plan

### Resolved Decisions

- Keep three reviewable tasks: classifier evidence, Doctor plus packaged red-green behavior, and verification-only full gates.
- Packaged CLI assertions are added before Doctor implementation so unit and Nix acceptance tests both demonstrate a genuine red state.
- Reuse classifier scope, ownership, reason, and dedicated per-file state types in Doctor instead of maintaining parallel enum mappings.
- Individual file state excludes the aggregate-only `duplicate` value by construction.
- Human paths escape controls, bidi formatting, zero-width formatting, word joiner, and BOM while preserving ordinary Unicode; JSON discloses lossy conversion explicitly.
- Packaged fixtures generate real imperative project definitions, compare exact paths, and prove malformed Home Manager content is not disclosed.
- Verification records a focused baseline and runs formatting, serial workspace tests, strict Clippy, build, Nix formatting, packaged checks, and final diff audit.
- Repository authority remains conservative: no commit, push, sync, publish, merge, broad reset, or broad checkout without explicit authorization.

### Changes Made

- Moved packaged fixture work from the final verification task into Doctor's TDD cycle.
- Removed four duplicate Doctor enums and added a dedicated per-file state without `Duplicate`.
- Strengthened terminal-format escaping, exact packaged path assertions, and sentinel non-disclosure coverage.
- Added focused pre-change baseline commands and made the final task verification-only.

### Deferred / Parking Lot

- No new provider topology, schema, dependency, raw path encoding, or unrelated Doctor warning change is included.

### Confidence Assessment

- Overall: High.
- Areas of concern: the packaged mixed-scope fixture intentionally uses temporary `HOME` generation because the public provider init flow is global-only; the exact-path assertions protect that setup from silently testing the wrong ownership contract.
