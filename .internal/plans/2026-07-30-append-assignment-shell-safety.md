# Append-Assignment Shell Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make parser-backed shell safety deterministically deny destructive flag and trusted-home paths assembled with scalar `+=` before any provider reaches model inference.

**Architecture:** Preserve Brush's `Assignment.append` bit in the shell projection, then apply known scalar appends to the existing tracked string and invalidate unknown appends. Before generic dynamic-target handling, compare every resolved nonliteral target with trusted `HOME` so an alias such as `X` receives the canonical home-delete denial.

**Tech Stack:** Rust 2024, `brush-parser` 0.4, Cargo workspace tests, provider-hook integration fixtures.

## Global Constraints

- Keep the change limited to `src/brain/safety/shell.rs`, `src/brain/safety.rs`, `tests/hook_activity.rs`, and these approved design/plan artifacts.
- Concatenate only when both the prior tracked value and appended value are known literals; otherwise invalidate the assignment.
- Do not broaden supported shell syntax or array-assignment support.
- Preserve all existing execution-context behavior.
- Codex, Claude, and Antigravity must deny both exploit classes before model inference.
- Do not commit, push, publish, or sync without explicit user authorization.

---

### Task 1: Close the scalar append-assignment safety gap

**Files:**
- Modify: `src/brain/safety/shell.rs`
- Modify: `src/brain/safety.rs`
- Modify: `tests/hook_activity.rs`
- Test: unit tests embedded in `src/brain/safety/shell.rs`
- Test: unit tests embedded in `src/brain/safety.rs`
- Test: `tests/hook_activity.rs`

**Interfaces:**
- Consumes: `brush_parser::ast::Assignment { name, value, append, .. }`; existing `HashMap<String, String>` assignment state; `resolve_word`, `literal_home_target`, and provider-hook test helpers.
- Produces: `ShellAssignment { name: String, value: ShellWord, append: bool }`; append-aware tracked state; canonical `irreversible-home-delete` classification for resolved nonliteral trusted-home targets.

**Acceptance Criteria:**
- `X=-; X+=rf; rm --no-preserve-root -f $X /` deterministically denies as `unsafe-recursive-delete-expansion`.
- A scalar assembled with `+=` to equal trusted `HOME` deterministically denies as `irreversible-home-delete`.
- Unknown append operands invalidate tracked state instead of being treated as replacement.
- Repeated and empty known-literal appends preserve Bash scalar concatenation behavior.
- Codex, Claude, and Antigravity return deny and make zero fake-model requests for both exploit classes.
- Existing parser-backed adversarial tests, all workspace tests, formatting, Clippy with warnings denied, and the build pass.

- [ ] **Step 1: Add failing in-process policy regressions**

Add this test beside the assignment-state tests in `src/brain/safety.rs`:

```rust
#[test]
fn append_assignments_preserve_destructive_values() {
    for command in [
        "X=-; X+=rf; rm --no-preserve-root -f $X /",
        "X=-; X+=; X+=r; X+=f; rm --no-preserve-root -f $X /",
        "X+=-rf; rm --no-preserve-root -f $X /",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(
            deny.rule_id, "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }

    let home = std::env::var("HOME").expect("test requires UTF-8 HOME");
    let split = home
        .char_indices()
        .next_back()
        .expect("HOME must not be empty")
        .0;
    let command = format!(
        "X='{}'; X+='{}'; rm -rf \"$X\"",
        &home[..split],
        &home[split..]
    );
    let deny = evaluate_command(&command).unwrap_or_else(|| panic!("{command}"));
    assert_eq!(deny.rule_id, "irreversible-home-delete", "{command}");
}
```

- [ ] **Step 2: Add failing real-provider regressions**

Add this test beside `reopened_shell_safety_corpus_denies_before_model_inference_for_every_provider` in `tests/hook_activity.rs`:

```rust
#[test]
fn append_assignment_bypasses_are_denied_before_model_inference_for_every_provider() {
    for provider in [
        AgentProvider::Codex,
        AgentProvider::Claude,
        AgentProvider::Antigravity,
    ] {
        for case in ["recursive flag", "trusted home"] {
            let home = tempfile::tempdir().unwrap();
            let fake_model = install_model_fixture(home.path(), "approve");
            let command = match case {
                "recursive flag" => {
                    "X=-; X+=rf; rm --no-preserve-root -f $X /".to_string()
                }
                "trusted home" => {
                    let home_text = home.path().to_str().unwrap();
                    let split = home_text
                        .char_indices()
                        .next_back()
                        .expect("temporary HOME must not be empty")
                        .0;
                    format!(
                        "X='{}'; X+='{}'; rm -rf \"$X\"",
                        &home_text[..split],
                        &home_text[split..]
                    )
                }
                _ => unreachable!(),
            };
            let expected_rule_id = match case {
                "recursive flag" => "unsafe-recursive-delete-expansion",
                "trusted home" => "irreversible-home-delete",
                _ => unreachable!(),
            };
            let (provider_name, event, payload) =
                shell_permission_payload(home.path(), provider, &command, None);

            let output =
                run_provider_permission_hook(home.path(), provider_name, event, &payload);

            assert!(output.status.success(), "{provider:?}: {case}: {command}");
            let response: serde_json::Value =
                serde_json::from_slice(&output.stdout).unwrap();
            if provider == AgentProvider::Antigravity {
                assert_eq!(
                    response["decision"], "deny",
                    "{provider:?}: {case}: {command}"
                );
            } else {
                assert_eq!(
                    response["hookSpecificOutput"]["decision"]["behavior"],
                    "deny",
                    "{provider:?}: {case}: {command}"
                );
            }
            assert_eq!(
                fake_model.request_count(),
                0,
                "{provider:?}: {case}: {command}"
            );
            assert_safety_deny(home.path(), expected_rule_id);
        }
    }
}
```

- [ ] **Step 3: Run the focused tests and verify RED**

Run serially:

```bash
nix develop path:. --command cargo test --lib append_assignments_preserve_destructive_values -- --nocapture
nix develop path:. --command cargo test --test hook_activity append_assignment_bypasses_are_denied_before_model_inference_for_every_provider -- --nocapture
```

Expected: the unit test fails because the reported commands return no deterministic decision; the provider test fails because at least one command reaches the approving fake model instead of receiving a deterministic deny.

- [ ] **Step 4: Add the structural projection regression and verify its RED state**

Add this test beside `structure_preserves_assignments_before_the_command` in `src/brain/safety/shell.rs`:

```rust
#[test]
fn structure_preserves_scalar_append_assignment_semantics() {
    let program = analyze("X=-; X+=rf").expect("scalar assignments");

    assert!(!program.commands[0].assignments[0].append);
    assert!(program.commands[1].assignments[0].append);
    assert_eq!(
        program.commands[1].assignments[0].value.literal.as_deref(),
        Some("rf")
    );
}
```

Run:

```bash
nix develop path:. --command cargo test --lib structure_preserves_scalar_append_assignment_semantics -- --nocapture
```

Expected: compilation fails with no `append` field on `ShellAssignment`, proving the projection contract is absent.

- [ ] **Step 5: Preserve Brush's append bit**

Change `ShellAssignment` in `src/brain/safety/shell.rs` to:

```rust
#[derive(Debug)]
pub(super) struct ShellAssignment {
    pub name: String,
    pub value: ShellWord,
    pub append: bool,
}
```

Change the end of `project_assignment` to:

```rust
Ok(ShellAssignment {
    name,
    value,
    append: assignment.append,
})
```

- [ ] **Step 6: Apply append semantics to tracked top-level assignments**

Replace the top-level literal assignment update inside `evaluate_in_process` in `src/brain/safety.rs` with:

```rust
let value_known = match assignment.value.literal.as_deref() {
    Some(value) if assignment.append => {
        if let Some(current) = assignments.get_mut(&assignment.name) {
            current.push_str(value);
            true
        } else {
            false
        }
    }
    Some(value) => {
        assignments.insert(assignment.name.clone(), value.to_string());
        true
    }
    None => false,
};
if value_known {
    if assignment.name == "IFS" {
        ifs_unknown = false;
    }
} else {
    assignments.remove(&assignment.name);
    if assignment.name == "IFS" {
        ifs_unknown = true;
    }
}
```

Do not change the conditional, loop, pipeline, asynchronous, group, subshell, or process-substitution branches.

- [ ] **Step 7: Classify resolved aliases of trusted HOME**

In the recursive-delete target loop in `src/brain/safety.rs`, after `word_is_home_target` and before `dynamic_target_is_dangerous`, add:

```rust
if resolve_word(target, &command_assignments)
    .is_some_and(|resolved| literal_home_target(&resolved))
{
    return canonical_deny("irreversible-home-delete");
}
```

This keeps literal targets and direct `$HOME` handling unchanged while covering variables such as `X` whose resolved value equals trusted `HOME`.

- [ ] **Step 8: Run focused tests and verify GREEN**

Run serially:

```bash
nix develop path:. --command cargo test --lib structure_preserves_scalar_append_assignment_semantics -- --nocapture
nix develop path:. --command cargo test --lib append_assignments_preserve_destructive_values -- --nocapture
nix develop path:. --command cargo test --test hook_activity append_assignment_bypasses_are_denied_before_model_inference_for_every_provider -- --nocapture
```

Expected: all three commands exit successfully; the provider test proves deny responses and zero model requests for all six provider/case combinations.

- [ ] **Step 9: Run the existing adversarial shell-safety corpus**

Run serially:

```bash
nix develop path:. --command cargo test --lib reopened_parser_backed_policy_corpus -- --nocapture
nix develop path:. --command cargo test --test hook_activity reopened_shell_safety_corpus_denies_before_model_inference_for_every_provider -- --nocapture
nix develop path:. --command cargo test --lib brain::safety::tests --quiet
nix develop path:. --command cargo test --lib brain::safety::shell::tests --quiet
```

Expected: every command exits successfully with no failed tests.

- [ ] **Step 10: Run repository quality gates**

Run serially:

```bash
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --workspace --all-targets
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
git diff --check
git status --short
```

Expected: formatting, all-target tests, Clippy, build, and diff checks succeed. Status contains only the approved spec, plan, and the three implementation/test files. Do not commit or push without separate user authorization.
