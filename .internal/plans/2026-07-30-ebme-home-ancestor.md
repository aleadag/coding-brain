# Trusted HOME Ancestor Denial Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> beads-superpowers:executing-plans to implement this plan task-by-task. Each
> Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within
> tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Deny recursive deletion of any non-root lexical ancestor of trusted
`HOME` before model inference for every supported permission provider.

**Architecture:** Add a small pure lexical component-prefix classifier and an
environment-backed wrapper used by literal and resolved target checks. Preserve
the existing root rule and the existing split/dynamic target classifiers.

**Tech Stack:** Rust, Cargo unit tests, Rust integration tests.

## Global Constraints

- Keep `/` classified by `irreversible-root-delete`.
- Classify exact trusted `HOME` and its non-root ancestors by
  `irreversible-home-delete`.
- Do not broaden relative-path, working-directory, glob, field-splitting, or
  dynamic-target behavior.
- Do not commit, push, publish, or sync without separate user authorization.

---

### Task 1: Classify and deny trusted HOME ancestors

**Files:**

- Modify: `src/brain/safety.rs:420`
- Modify: `src/brain/safety.rs:483`
- Test: `src/brain/safety.rs`
- Test: `tests/hook_activity.rs:1192`

**Interfaces:**

- Consumes: `lexical_absolute_parts(&Path) -> Option<Vec<OsString>>` and trusted
  `HOME` from `std::env::var_os`.
- Produces:
  `path_is_home_or_ancestor(target: &Path, home: &Path) -> bool` and
  `literal_home_or_ancestor_target(target: &str) -> bool`.

**Acceptance Criteria:**

- Literal and resolved non-root trusted-HOME ancestors are denied as
  `irreversible-home-delete`.
- Codex, Claude, and Antigravity deny these cases before model inference with
  zero model requests.
- Exact trusted `HOME` remains denied.
- Quoted descendants and unrelated absolute paths remain outside this rule.
- Targeted tests, full tests, formatting, and Clippy pass.

- [ ] **Step 1: Add the failing lexical-classifier unit regression**

Add a unit test beside the existing safety evaluator tests:

```rust
#[test]
fn lexical_home_or_ancestor_classification_excludes_root_and_descendants() {
    let home = Path::new("/home/alexander");

    for target in ["/home", "/home/./alexander", "/home/alexander"] {
        assert!(
            path_is_home_or_ancestor(Path::new(target), home),
            "{target}"
        );
    }
    for target in [
        "/",
        "/hom",
        "/home/alex",
        "/home/alexander/safe",
        "/srv",
        "home/alexander",
    ] {
        assert!(
            !path_is_home_or_ancestor(Path::new(target), home),
            "{target}"
        );
    }
}
```

- [ ] **Step 2: Add failing provider-boundary regressions**

Extend
`literal_home_delete_denies_before_model_inference_for_every_provider` so each
provider receives a fresh temporary HOME and model fixture for these commands:

```rust
let home_parent = home
    .path()
    .parent()
    .expect("temporary HOME must have a parent");
assert_ne!(home_parent, Path::new("/"));
let commands = [
    format!("rm -rf \"{}\"", home.path().display()),
    format!("rm -rf \"{}\"", home_parent.display()),
    format!("X='{}'; rm -rf \"$X\"", home_parent.display()),
];
```

For every command, retain the existing assertions:

```rust
assert!(output.status.success(), "{provider:?}: {command}");
let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
if provider == AgentProvider::Antigravity {
    assert_eq!(response["decision"], "deny", "{provider:?}: {command}");
} else {
    assert_eq!(
        response["hookSpecificOutput"]["decision"]["behavior"],
        "deny",
        "{provider:?}: {command}"
    );
}
assert_eq!(fake_model.request_count(), 0, "{provider:?}: {command}");
assert_safety_deny(home.path(), "irreversible-home-delete");
```

Keep the existing all-provider quoted-descendant and unrelated-path controls in
`home_alias_field_splitting_is_denied_before_model_inference_for_every_provider`
unchanged.

- [ ] **Step 3: Run the new tests and verify the ancestor cases fail**

Run:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::lexical_home_or_ancestor_classification_excludes_root_and_descendants
nix develop path:. --command cargo test \
  --test hook_activity \
  literal_home_delete_denies_before_model_inference_for_every_provider
```

Expected: the unit test fails to compile because the classifier does not exist;
after adding only its signature if needed to expose the behavioral failure, the
`/home` assertion fails. The provider test fails because literal and resolved
HOME-parent targets reach model inference instead of receiving a safety deny.

- [ ] **Step 4: Implement the pure classifier and environment wrapper**

Add these helpers beside `literal_home_target`:

```rust
fn path_is_home_or_ancestor(target: &Path, home: &Path) -> bool {
    let Some(target) = lexical_absolute_parts(target) else {
        return false;
    };
    if target.is_empty() {
        return false;
    }
    lexical_absolute_parts(home)
        .is_some_and(|home| target.len() <= home.len() && home.starts_with(&target))
}

fn literal_home_or_ancestor_target(target: &str) -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        path_is_home_or_ancestor(Path::new(target), Path::new(&home))
    })
}
```

Use `literal_home_or_ancestor_target` for the literal target check and the
`resolve_word` result only. Keep `literal_home_target` and its use inside
`dynamic_target_is_dangerous` unchanged so dynamic and split-target behavior is
not broadened by this fix.

- [ ] **Step 5: Run targeted tests and preserve controls**

Run:

```bash
nix develop path:. --command cargo test \
  brain::safety::tests::lexical_home_or_ancestor_classification_excludes_root_and_descendants
nix develop path:. --command cargo test \
  --test hook_activity \
  literal_home_delete_denies_before_model_inference_for_every_provider
nix develop path:. --command cargo test \
  --test hook_activity \
  home_alias_field_splitting_is_denied_before_model_inference_for_every_provider
```

Expected: all targeted tests pass. The control test continues to show one model
request and an allow response for quoted descendants and unrelated paths.

- [ ] **Step 6: Run repository quality gates**

Run serially:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo test --workspace
```

Expected: every command exits successfully with no formatting diff, Clippy
warning, or test failure.

- [ ] **Step 7: Review the final surgical diff**

Run:

```bash
git diff --check
git diff -- src/brain/safety.rs tests/hook_activity.rs
git status --short
```

Expected: every changed production line implements HOME-ancestor
classification; tests cover the requested cases; no unrelated files are
modified beyond this approved spec and plan.
