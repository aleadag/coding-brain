# BusyBox `time -f` Value Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make BusyBox `time -f` values that occupy exactly one separate argv, including empty and dynamic values, reach deterministic destructive-child analysis without weakening `time -o` or attachment-presence safeguards.

**Architecture:** Keep the existing launcher-aware wrapper scanner and typed `WrapperValueRule` path. Split BusyBox `time` option classification so `-f` uses `Any` while `-o` retains `NonEmpty`; the existing consumption logic will accept a separate exact-one dynamic format but continue rejecting zero/multi-argv expansion and an attached dynamic value whose attachment may disappear.

**Tech Stack:** Rust, Brush shell AST projection, Cargo workspace tests, Nix development shell.

## Global Constraints

- Preserve `Deny > Indeterminate > NoDeterministicDecision` precedence.
- Deterministic deny and indeterminate paths must issue zero model requests for Codex, Claude, and Antigravity.
- Unknown, terminating, abbreviated, missing-value, and unsupported multicall options remain `Indeterminate`.
- BusyBox `time -o` retains `WrapperValueRule::NonEmpty`.
- Dynamic option values that may expand to zero or multiple argv remain fail-closed.
- Do not refactor adjacent wrapper parsing or change Toybox behavior.
- Do not commit, push, publish, merge, or change versions without separate authorization.

---

### Task 1: Implement BusyBox format-value semantics

**Files:**
- Modify: `src/brain/safety.rs:3612-3684`
- Modify: `src/brain/safety.rs:6200-6490`
- Modify: `src/brain/permission_hook.rs:2129-2310`
- Modify: `tests/shell_safety_helper_cli.rs:70-190`

**Interfaces:**
- Consumes: `WrapperOptionValue::Required`, `WrapperOptionAttachment`, `WrapperValueRule`, and `consume_wrapper_option_value` in `src/brain/safety.rs`.
- Produces: launcher-aware classification where BusyBox `time -f` returns `WrapperValueRule::Any` and BusyBox `time -o` returns `WrapperValueRule::NonEmpty`; no public API changes.

**Acceptance Criteria:**
- Literal empty and nonempty BusyBox `time -f` values reach nested destructive-child classification.
- Dynamic BusyBox `time -f` values proven to produce exactly one separate argv reach nested destructive-child classification.
- Zero-or-multiple-argv expansions and attached values without attachment-presence proof remain fail-closed.
- BusyBox `time -o` retains `NonEmpty` semantics.
- Shipped-helper and Codex, Claude, and Antigravity tests prove deterministic denial with zero model inference.
- Focused tests, full serial all-target tests, Clippy with warnings denied, rustfmt check, and Cargo build pass.

- [ ] **Step 1: Add RED evaluator regressions**

In `src/brain/safety.rs`, add these executing forms to `valid_wrapper_option_values_reach_destructive_children`:

```rust
"busybox time -f '' sh -c 'rm --no-preserve-root -rf /'",
"busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
```

Keep `busybox time -f"$FORMAT" ...` in `attached_dynamic_wrapper_values_preserve_value_semantics`: if the expansion is empty, the `-f` argv has no attached value and BusyBox consumes the following argv as the format.

- [ ] **Step 2: Run the evaluator regression and confirm RED**

Run:

```bash
nix develop path:. --command cargo test --lib brain::safety::tests::valid_wrapper_option_values_reach_destructive_children -- --exact
```

Expected before the production change: FAIL because `busybox time -f '' ...` and `busybox time -f "$FORMAT" ...` evaluate as `Indeterminate` instead of returning `irreversible-root-delete`.

- [ ] **Step 3: Add RED shipped-helper and provider regressions**

In `tests/shell_safety_helper_cli.rs`, add the two commands below to `shipped_helper_denies_literal_nested_root_deletion` and assert the existing `deny` / `irreversible-root-delete` response:

```rust
"busybox time -f '' sh -c 'rm --no-preserve-root -rf /'",
"busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
```

In `src/brain/permission_hook.rs`, add the same commands with `proven_deny = true` to `nested_shell_safety_precedes_inference_for_every_provider`:

```rust
("busybox time -f '' sh -c 'rm --no-preserve-root -rf /'", true),
(
    "busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
    true,
),
```

The existing provider-test closure must remain the zero-inference assertion; do not replace it with a permissive mock.

- [ ] **Step 4: Implement the minimal classifier split**

In `classify_busybox_time_option_prefix`, replace the shared match arm with distinct `-o` and `-f` value rules:

```rust
match option {
    'o' => {
        return Some(WrapperOptionValue::Required {
            attached: wrapper_option_attachment(
                options,
                dynamic_suffix,
                may_not_expand_to_exactly_one_argv,
            ),
            rule: WrapperValueRule::NonEmpty,
        });
    }
    'f' => {
        return Some(WrapperOptionValue::Required {
            attached: wrapper_option_attachment(
                options,
                dynamic_suffix,
                may_not_expand_to_exactly_one_argv,
            ),
            rule: WrapperValueRule::Any,
        });
    }
    'a' | 'p' | 'v' => {}
    _ => return None,
}
```

Do not change `consume_wrapper_option_value`: its existing attachment-presence and exact-one-argv checks are required.

- [ ] **Step 5: Run focused GREEN tests**

Run:

```bash
nix develop path:. --command cargo test --lib brain::safety::tests::valid_wrapper_option_values_reach_destructive_children -- --exact
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_denies_literal_nested_root_deletion -- --exact
nix develop path:. --command cargo test --lib brain::permission_hook::tests::nested_shell_safety_precedes_inference_for_every_provider -- --exact
nix develop path:. --command cargo test --lib brain::safety::tests::attached_dynamic_wrapper_values_preserve_value_semantics -- --exact
nix develop path:. --command cargo test --lib brain::safety::tests::quoted_multi_argv_separate_wrapper_values_fail_closed -- --exact
```

Expected: all commands exit 0; destructive exact-one `-f` forms deny, attached ambiguity remains indeterminate, and multi-argv expansion remains `unsafe-recursive-delete-expansion`.

- [ ] **Step 6: Replay harmless real-BusyBox semantics**

Run:

```bash
busybox time -f '' sh -c 'printf CHILD'
FORMAT=; busybox time -f "$FORMAT" sh -c 'printf CHILD'
busybox time -o '' sh -c 'printf CHILD'
```

Expected: the first two commands print `CHILD`; the third reports that the empty output path cannot be opened and does not print `CHILD`.

- [ ] **Step 7: Run full repository verification**

Run serially:

```bash
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo build
git diff --check
git status --short
```

Expected: every gate exits 0; the status contains only the approved spec, plan, and three implementation/test files. If an existing timing test fails, reproduce it on clean `origin/main` before classifying it as unrelated.

- [ ] **Step 8: Review the exact diff and hand off without publishing**

Run:

```bash
git diff -- src/brain/safety.rs src/brain/permission_hook.rs tests/shell_safety_helper_cli.rs .internal/specs/2026-08-02-busybox-time-format-value-semantics-design.md .internal/plans/2026-08-02-busybox-time-format-value-semantics.md
```

Expected: every changed line maps to the approved `-f` semantics, regressions, or required design/plan records. Report verification and Beads status; await explicit authority before any commit or push.
