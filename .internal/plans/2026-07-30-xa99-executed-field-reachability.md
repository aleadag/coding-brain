# XA99 Executed Split-Field Reachability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Deny recursively deleted split fields that can lexically reach trusted HOME or its ancestors before model inference.

**Architecture:** Keep provider payloads and the isolated-helper boundary unchanged. Reuse the existing assignment and field resolver, add a bounded lexical predicate over executed fields and trusted HOME components, and preserve the existing indeterminate denial for unresolved or cwd-dependent targets.

**Tech Stack:** Rust, Cargo tests, Nix development shell, Beads.

## Global Constraints

- Keep exact-HOME and every existing non-splitting classification unchanged.
- Apply the new HOME/ancestor reachability check only to field-splittable targets.
- Use lexical components only; do not access the filesystem, canonicalize symlinks, enumerate hypothetical cwd values, or change provider/helper protocols.
- Preserve `irreversible-home-delete` for known HOME/ancestor reachability and `unsafe-recursive-delete-expansion` for unresolved fields or relative parent traversal.
- Preserve model inference for quoted descendant aliases and unrelated controls.
- Keep production and regression changes limited to `src/brain/safety.rs` and `tests/hook_activity.rs`.
- Do not change configuration, public documentation, versions, commits, branches, or remotes.

---

### Task 1: Implement executed split-field HOME reachability

**Files:**
- Modify: `src/brain/safety.rs:427-434`
- Modify: `src/brain/safety.rs:472-525`
- Modify: `src/brain/safety.rs:1728-1760`
- Modify: `tests/hook_activity.rs:957-1016`

**Interfaces:**
- Consumes: `resolve_word_fields(&shell::ShellWord, &HashMap<String, String>, bool) -> Option<Vec<ResolvedField>>`, `lexical_absolute_parts(&Path) -> Option<Vec<OsString>>`, trusted `HOME`, and the existing provider-hook fixture helpers.
- Produces: private `split_target_may_reach_home_or_ancestor(&shell::ShellWord, &HashMap<String, String>, bool) -> bool` and `split_field_may_reach_home_or_ancestor(&str) -> bool` predicates; no public API or protocol changes.

**Acceptance Criteria:**
- Exact HOME, HOME descendants, append-built descendants, and non-descendant values whose executed split fields can reach HOME or an ancestor deny deterministically as `irreversible-home-delete`.
- Relative parent traversal that cannot be proven safe remains denied as `unsafe-recursive-delete-expansion`.
- Quoted descendant aliases and unrelated split-path controls preserve existing inference behavior.
- Codex, Claude, and Antigravity unsafe cases deny before model inference in root and trusted-HOME-parent execution contexts.
- Provider schemas, `ShellCommandInput`, helper protocol, configuration, versions, and public documentation remain unchanged.
- Focused tests, the prior safety corpus, full serial tests, formatting, Clippy with warnings denied, build, and normalized diff checks pass.

- [ ] **Step 1: Extend the unit regression and verify RED**

In `home_alias_classification_respects_field_splitting`, retain the exact-HOME
case and add direct descendant, append-built descendant, shared-parent sibling,
relative-parent, quoted-descendant, and unrelated controls:

```rust
let home_path = Path::new(&home);
let home_parent = home_path.parent().expect("HOME must have a parent");
let descendant = home_path.join("safe");
let sibling = home_parent.join("xa99-sibling");

for command in [
    format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
    format!("IFS=/; X='{home}'; X+=/safe; rm -rf $X"),
    format!("IFS=/; X='{}'; rm -rf $X", sibling.display()),
] {
    let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
    assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
}

let parent_traversal = "IFS=,; X='../safe'; rm -rf $X";
let deny =
    evaluate_command(parent_traversal).unwrap_or_else(|| panic!("{parent_traversal}"));
assert_eq!(
    deny.rule_id,
    "unsafe-recursive-delete-expansion",
    "{parent_traversal}"
);

for command in [
    format!("X='{}'; rm -rf \"$X\"", descendant.display()),
    "IFS=/; X=/xa99-safe-control/target; rm -rf $X".to_string(),
] {
    assert!(evaluate_command(&command).is_none(), "{command}");
}
```

Run:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::home_alias_classification_respects_field_splitting \
  -- --exact --nocapture --test-threads=1
```

Expected: FAIL on the first new descendant or shared-parent assertion because
the evaluator returns no deterministic decision. If setup, parsing, the quoted
control, or the unrelated control fails instead, stop and correct the test
before production changes.

- [ ] **Step 2: Extend the real-provider regression and verify RED**

Expand
`home_alias_field_splitting_is_denied_before_model_inference_for_every_provider`
so every provider receives a fresh temporary HOME and approving model for each
case. Replace the existing case loop with:

```rust
for provider in [
    AgentProvider::Codex,
    AgentProvider::Claude,
    AgentProvider::Antigravity,
] {
    for case in [
        "root cwd exact HOME",
        "root cwd descendant",
        "root cwd append descendant",
        "root cwd shared-parent sibling",
        "HOME-parent cwd descendant",
        "shell changes to HOME parent",
    ] {
        let home = tempfile::tempdir().unwrap();
        let fake_model = install_model_fixture(home.path(), "approve");
        if provider == AgentProvider::Antigravity {
            seed_antigravity_invocation(home.path(), 5);
        }
        let home_text = home.path().to_str().unwrap();
        let home_parent = home
            .path()
            .parent()
            .expect("temporary HOME must have a parent");
        let descendant = home.path().join("safe");
        let sibling = home_parent.join("xa99-sibling");
        let (command, command_cwd, hook_cwd) = match case {
            "root cwd exact HOME" => (
                format!("IFS=/; X='{home_text}'; rm -rf $X"),
                Path::new("/"),
                Path::new("/"),
            ),
            "root cwd descendant" => (
                format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
                Path::new("/"),
                Path::new("/"),
            ),
            "root cwd append descendant" => (
                format!("IFS=/; X='{home_text}'; X+=/safe; rm -rf $X"),
                Path::new("/"),
                Path::new("/"),
            ),
            "root cwd shared-parent sibling" => (
                format!("IFS=/; X='{}'; rm -rf $X", sibling.display()),
                Path::new("/"),
                Path::new("/"),
            ),
            "HOME-parent cwd descendant" => (
                format!("IFS=/; X='{}'; rm -rf $X", descendant.display()),
                home_parent,
                home_parent,
            ),
            "shell changes to HOME parent" => (
                format!(
                    "cd '{}'; IFS=/; X='{}'; rm -rf $X",
                    home_parent.display(),
                    descendant.display()
                ),
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
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
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

    for case in ["quoted descendant", "unrelated split path"] {
        let home = tempfile::tempdir().unwrap();
        let fake_model = install_model_fixture(home.path(), "approve");
        if provider == AgentProvider::Antigravity {
            seed_antigravity_invocation(home.path(), 5);
        }
        let command = match case {
            "quoted descendant" => {
                format!("X='{}/safe'; rm -rf \"$X\"", home.path().display())
            }
            "unrelated split path" => {
                "IFS=/; X=/xa99-safe-control/target; rm -rf $X".to_string()
            }
            _ => unreachable!(),
        };
        let (provider_name, event, payload) =
            shell_permission_payload(home.path(), provider, &command, None);
        let output =
            run_provider_permission_hook(home.path(), provider_name, event, &payload);

        assert!(output.status.success(), "{provider:?}: {case}: {command}");
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let decision = if provider == AgentProvider::Antigravity {
            response["decision"].as_str()
        } else {
            response["hookSpecificOutput"]["decision"]["behavior"].as_str()
        };
        assert_eq!(
            (decision, fake_model.request_count()),
            (Some("allow"), 1),
            "{provider:?}: {case}: {command}"
        );
    }
}
```

Run:

```bash
nix develop path:. --command cargo test --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: FAIL with an unsafe descendant or shared-parent case returning
model-backed `allow` and request count `1`. The quoted and unrelated controls
must already pass.

- [ ] **Step 3: Add the minimal bounded lexical predicates**

In `src/brain/safety.rs`, add the HOME-specific executed-field check before
`dynamic_target_is_dangerous`:

```rust
if split_target_may_reach_home_or_ancestor(
    target,
    &command_assignments,
    command_ifs_unknown,
) {
    return canonical_deny("irreversible-home-delete");
}
```

Add the private predicates beside `literal_home_target` and
`dynamic_target_is_dangerous`:

```rust
fn split_target_may_reach_home_or_ancestor(
    target: &shell::ShellWord,
    assignments: &HashMap<String, String>,
    ifs_unknown: bool,
) -> bool {
    if !target.can_split_fields {
        return false;
    }
    resolve_word_fields(target, assignments, ifs_unknown).is_some_and(|fields| {
        fields
            .iter()
            .any(|field| split_field_may_reach_home_or_ancestor(&field.value))
    })
}

fn split_field_may_reach_home_or_ancestor(value: &str) -> bool {
    let Some(home) = std::env::var_os("HOME")
        .and_then(|home| lexical_absolute_parts(Path::new(&home)))
    else {
        return false;
    };
    let path = Path::new(value);
    if let Some(field) = lexical_absolute_parts(path) {
        return !field.is_empty() && field.len() <= home.len() && home.starts_with(&field);
    }

    let mut field = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => field.push(part.to_os_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    !field.is_empty()
        && (1..=home.len()).any(|ancestor_len| {
            let ancestor = &home[..ancestor_len];
            field.len() <= ancestor.len() && ancestor.ends_with(&field)
        })
}
```

Extend `dynamic_target_is_dangerous` so unresolved relative parent traversal
keeps the generic fail-closed rule:

```rust
|| (!Path::new(&field.value).is_absolute()
    && Path::new(&field.value)
        .components()
        .any(|component| component == Component::ParentDir))
```

Do not change `resolve_word_fields`, assignment parsing, provider payloads, or
the helper protocol.

#### Final-review amendment to Step 3

The literal-only sketch above was superseded after review demonstrated active
pathname-pattern bypasses. The implemented predicate consumes each
`ResolvedField`, partitions full-HOME patterns from ancestor-only patterns, and
compares active patterns component by component after lexical normalization.
This handles `./` and repeated separators without allowing ordinary wildcard
envelopes to cross component boundaries. Since shell-option state is not
trusted, exact `**` components conservatively consume zero or more candidate
components and component matching also considers case-folded compatibility for
`nocaseglob`. The isolated trusted-HOME validator requires a nonempty absolute
path before helper construction. Absolute patterns are checked against every
HOME prefix, while relative patterns are checked against every nonempty
contiguous suffix of those prefixes; textual pattern length never short-circuits
`globstar` reachability.

The final regression matrix additionally covers:

- normalized relative patterns from HOME's parent;
- repeated-separator absolute HOME patterns;
- single- and multi-field ancestor patterns;
- a shared-prefix pattern whose final component cannot match HOME;
- `globstar` HOME and HOME-ancestor patterns plus a `nocaseglob` HOME pattern;
- empty and relative trusted-HOME rejection.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run serially:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::home_alias_classification_respects_field_splitting \
  -- --exact --nocapture --test-threads=1
nix develop path:. --command cargo test --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider \
  -- --exact --nocapture --test-threads=1
```

Expected: both exit `0`. Every unsafe provider case denies with zero model
requests; quoted and unrelated controls remain model-backed allows.

- [ ] **Step 5: Run the prior shell-safety corpus**

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

Expected: every command exits `0`.

- [ ] **Step 6: Run full verification and audit scope**

Run the shared Cargo/Nix gates serially:

```bash
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --quiet -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build
git diff --check
git status --short
```

Expected: formatting, tests, Clippy, build, and normalized diff checks exit `0`.
Status shows only the approved spec/plan plus `src/brain/safety.rs` and
`tests/hook_activity.rs`. Use `beads-superpowers:document-release` to confirm
that no public documentation or version change is required. Do not commit,
push, close `codexctl-xa99`, or publish without separate authorization.
