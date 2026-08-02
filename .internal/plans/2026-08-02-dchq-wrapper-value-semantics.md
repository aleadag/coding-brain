# Wrapper Option Value Semantics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Prevent invalid or semantically unknown `time`/`env` option values from projecting an unexecuted destructive child while preserving valid direct and BusyBox wrapper denials and zero-model native confirmation.

**Architecture:** Replace lossy boolean option arity with private typed value semantics in `src/brain/safety.rs`. Wrapper classifiers retain separate, literal-attached, or dynamic-attached provenance and select a closed `Any`, `NonEmpty`, or `EnvUnsetName` rule. One shared consumer checks expansion cardinality, proves that a required argument exists, and then applies its semantic rule, producing consumed, indeterminate, or unsafe-expansion outcomes without changing provider hooks or shell-word projection.

**Tech Stack:** Rust 2021, Brush shell AST projection, existing Coding Brain safety evaluator and permission-hook harness, Cargo through `nix develop path:.`.

## Global Constraints

- Keep the reusable abstraction private to `src/brain/safety.rs`; do not add traits, callbacks, a runtime registry, or public APIs.
- Existing wrapper classifiers continue to own grammar and launcher identity.
- Characters after the first value-taking short option are its attached value, not more flags.
- Audited direct `time -o`, BusyBox `time -o/-f`, and direct `env -u` require semantic validation; BusyBox `env -u` uses `Any`, which accepts all content only after a required value argument is proven present.
- A separate exact-one dynamic word proves that its argv exists. Bare attached dynamics such as `-u"$NAME"` do not: an empty expansion may leave no attached bytes and shift the next argv into the value position. Non-empty literal bytes before or after the dynamic part prove attachment presence, as in `-uX"$NAME"` and `-u"$NAME"X`.
- `NonEmpty` and `EnvUnsetName` accept only literals proven valid; non-literals are `Indeterminate`. Bare exact-one attached dynamics governed by `Any` are also `Indeterminate` because semantic acceptance does not prove attachment presence.
- Values that may expand to zero or multiple argv retain deterministic `unsafe-recursive-delete-expansion` denial before presence or semantic checks, including `-uX"$@"`, `-u"$@"X`, and array equivalents.
- Preserve `Deny > Indeterminate > NoDeterministicDecision`, launcher identity, parser budgets, rule identifiers, provider payloads, and zero model requests on deny/indeterminate paths.
- Do not infer utility identity from paths, `PATH`, the filesystem, build targets, providers, or model output.
- Do not modify `src/brain/safety/shell.rs`; current `ShellWord::literal` distinguishes empty literals from unknown content, and `ShellWord::parts` preserves literal bytes around dynamics.
- Scope source changes to `src/brain/safety.rs`, `src/brain/permission_hook.rs`, and `tests/shell_safety_helper_cli.rs`, plus this approved spec and plan.
- Do not change user-facing documentation or release versions.
- Local task commits are authorized for exact review packages. Do not push,
  open a pull request, merge, change versions, publish, or release.

---

### Task 1: Implement typed wrapper option value semantics

**Files:**
- Modify: `src/brain/safety.rs:3045-3110` and `src/brain/safety.rs:3327-3357`
- Modify: `src/brain/safety.rs:3416-3525`
- Test: `src/brain/safety.rs` test module near `direct_wrapper_common_option_arity_reaches_the_child`
- Test: `src/brain/permission_hook.rs` test `nested_shell_safety_precedes_inference_for_every_provider`
- Test: `tests/shell_safety_helper_cli.rs` near `shipped_helper_preserves_direct_wrapper_command_position_uncertainty`

**Interfaces:**
- Consumes: `shell::ShellWord::literal`, ordered `shell::ShellWord::parts`, `shell::ShellWord::may_not_expand_to_exactly_one_argv`, `evaluate_result`, `run_shipped_helper`, and the existing all-provider permission-hook corpus.
- Produces: private `WrapperOptionValue`, `WrapperOptionAttachment`, `WrapperValueRule`, and `WrapperValueConsumption<'a>` types; wrapper classifiers return typed value semantics and attachment evidence; the scanner consumes values through one shared function.

**Acceptance Criteria:**
- Empty direct/BusyBox `time` output values are `Indeterminate`.
- Empty or `=`-containing direct `env -u` names are `Indeterminate` in separate and attached forms.
- Exact-one dynamic direct `time -o` and `env -u` values are `Indeterminate`.
- BusyBox `env -u` empty and `=`-containing names still expose and deny a destructive child.
- Separate exact-one dynamic BusyBox `env -u` values expose the child. Bare exact-one attached dynamics such as `-u"$NAME"` and `-iu"$NAME"` are `Indeterminate`; a non-empty literal prefix or suffix proves presence and exposes the child.
- Valid direct and BusyBox clustered/standalone options still expose and deny a destructive child.
- Multi-argv option-value expansion retains deterministic unsafe-expansion denial before `Any` consumption, including literal-prefix, literal-suffix, and array forms.
- Invalid-value indeterminacy cannot suppress a separate proven delete in either statement order.
- Shipped-helper JSON and all-provider behavior match evaluator results; every indeterminate provider case makes zero model requests.

- [x] **Step 1: Add the evaluator RED corpus**

Add beside the existing direct-wrapper tests in `src/brain/safety.rs`:

```rust
#[test]
fn invalid_or_unknown_wrapper_option_values_are_indeterminate() {
    for command in [
        "/usr/bin/time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -uA=B sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
        "builtin command /usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "builtin exec /usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        assert!(
            matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
            "{command}: {:?}",
            evaluate_result(command)
        );
    }
}

#[test]
fn valid_wrapper_option_values_reach_destructive_children() {
    for command in [
        "/usr/bin/time -o log sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -aolog sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -olog sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -vfFORMAT sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u HOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -ivuHOME sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u '' sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -uA=B sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!(
            "{command}: {:?}",
            evaluate_result(command)
        ));
        assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
    }
}

#[test]
fn invalid_wrapper_value_keeps_deny_precedence_in_both_orders() {
    for command in [
        "/usr/bin/time -o '' sh -c 'rm -rf /'; rm --no-preserve-root -rf /",
        "rm --no-preserve-root -rf /; /usr/bin/env -u '=HOME' sh -c 'rm -rf /'",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
    }
}
```

Keep the existing `expanding_separate_wrapper_option_values_fail_closed` and
`quoted_multi_argv_separate_wrapper_values_fail_closed` tests unchanged; they
are the controls proving unsafe expansion still outranks semantic uncertainty.

- [x] **Step 2: Add shipped-helper and provider RED coverage**

Add to `tests/shell_safety_helper_cli.rs`:

```rust
#[test]
fn shipped_helper_preserves_wrapper_value_uncertainty() {
    for command in [
        "/usr/bin/time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -uA=B sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}
```

Extend `nested_shell_safety_precedes_inference_for_every_provider` in
`src/brain/permission_hook.rs` with these `(command, proven_deny)` entries:

```rust
(
    "/usr/bin/time -o '' sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "busybox time -o '' sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -u '' sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -uA=B sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "busybox env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
    true,
),
(
    "busybox env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    true,
),
```

The existing provider harness already asserts that `false` cases abstain for
native confirmation and make zero model requests; do not change that harness.

- [x] **Step 3: Run the focused corpus and verify RED**

Run serially:

```bash
nix develop path:. --command cargo test invalid_or_unknown_wrapper_option_values_are_indeterminate -- --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_preserves_wrapper_value_uncertainty -- --exact --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider -- --nocapture
```

Expected: each command fails on a new invalid/unknown value because current
code consumes it and returns a deterministic denial. Record the first failing
command and actual result before production edits. Fix test spelling or command
selection if a command errors or passes without exercising the regression.
Each output must name the requested test and report a nonzero executed-test
count; a zero-test success fails this step.

- [x] **Step 4: Add the typed value model and shared consumer**

Add near the current `classify_time_option` and `EnvOption` definitions in
`src/brain/safety.rs`. The validated model separates attachment evidence from
semantic validity:

```rust
enum WrapperOptionValue {
    None,
    Required {
        attached: WrapperOptionAttachment,
        rule: WrapperValueRule,
    },
}

enum WrapperOptionAttachment {
    Separate,
    Literal(String),
    Dynamic {
        literal_prefix: String,
        literal_after_dynamic_proves_nonempty: bool,
        may_not_expand_to_exactly_one_argv: bool,
    },
}

enum WrapperValueRule {
    Any,
    NonEmpty,
    EnvUnsetName,
}

enum WrapperValueConsumption<'a> {
    Consumed(&'a [&'a shell::ShellWord]),
    Indeterminate,
    UnsafeExpansion,
}
```

The consumer applies this order:

1. Any dynamic attachment that may expand to zero or multiple argv returns
   `UnsafeExpansion`, even if a literal prefix or suffix is present.
2. A separate value consumes one existing exact-one word. A dynamic attachment
   is present only when it has non-empty literal bytes before or after its
   dynamic part; the option prefix itself is not value evidence.
3. `Any` accepts any content after presence is proven. `NonEmpty` and
   `EnvUnsetName` still require a literal that passes their semantic rule.

Keep these types private. Do not add a blanket implementation, trait, callback,
or default-accept branch. In particular, do not let `Any` stand in for proof
that an attached argument exists.

- [x] **Step 5: Return typed values from direct and BusyBox classifiers**

Replace the boolean `classify_time_option` and
`classify_busybox_time_option` results with owned `WrapperOptionValue` values.
Literal and dynamic entry points share prefix classifiers. A string cursor
stops at the first value-taking `o`, `f`, or `u`; everything after that option
is value content, not another flag:

```rust
fn classify_time_option(word: &str) -> Option<WrapperOptionValue> {
    classify_time_option_prefix(word, false, false)
}

fn classify_dynamic_time_option(
    word: &shell::ShellWord,
) -> Option<WrapperOptionValue> {
    let option = classify_time_option_prefix(
        &leading_literal_option_prefix(word)?,
        true,
        word_may_not_expand_to_exactly_one_argv(word),
    );
    matches!(&option, Some(WrapperOptionValue::Required { .. }))
        .then_some(option)?
}

fn wrapper_option_attachment(
    attached: &str,
    dynamic_suffix: bool,
    may_not_expand_to_exactly_one_argv: bool,
) -> WrapperOptionAttachment {
    if dynamic_suffix {
        WrapperOptionAttachment::Dynamic {
            literal_prefix: attached.into(),
            literal_after_dynamic_proves_nonempty: false,
            may_not_expand_to_exactly_one_argv,
        }
    } else if attached.is_empty() {
        WrapperOptionAttachment::Separate
    } else {
        WrapperOptionAttachment::Literal(attached.into())
    }
}
```

`EnvOption` is likewise owned and non-generic:

```rust
enum EnvOption {
    Supported {
        value: WrapperOptionValue,
        child_context: bool,
    },
    SplitString,
    Unsupported,
}
```

The dynamic BusyBox `env` entry point first records cardinality from the whole
`ShellWord`, then classifies only its leading literal option prefix. Once a
dynamic `-u` is structurally recognized, it scans the ordered parts for a
non-empty literal after dynamic content and stores that proof in
`literal_after_dynamic_proves_nonempty`. The consumer then preserves the Step
4 order: unsafe cardinality first, attachment presence second, and the `Any`
semantic rule last. Direct `env` and all `time` dynamics remain semantically
indeterminate even when attachment presence is proven.

```rust
if may_not_expand_to_exactly_one_argv {
    return WrapperValueConsumption::UnsafeExpansion;
}
match rule {
    WrapperValueRule::Any
        if !literal_prefix.is_empty()
            || literal_after_dynamic_proves_nonempty =>
    {
        WrapperValueConsumption::Consumed(words)
    }
    _ => WrapperValueConsumption::Indeterminate,
}
```

- [x] **Step 6: Route both scanner loops through the shared consumer**

In the external `time` loop, replace the `takes_value` missing/cardinality
block after consuming the option token with:

```rust
match consume_wrapper_option_value(words, option) {
    WrapperValueConsumption::Consumed(remaining) => words = remaining,
    WrapperValueConsumption::Indeterminate => {
        indeterminate_after_scan = true;
        return Ok(UnwrappedCommand {
            words: original_words,
            indeterminate_child_context,
            indeterminate_after_scan,
            eval_context,
            time_keyword_allowed,
            eval_prefix_assignments_persist,
        });
    }
    WrapperValueConsumption::UnsafeExpansion => {
        return Err(UnwrapError::UnsafeExpansion);
    }
}
```

In the `env` `Supported` arm, preserve `child_context`, consume the option
token, then use:

```rust
EnvOption::Supported {
    value,
    child_context,
} => {
    indeterminate_child_context |= child_context;
    words = &words[1..];
    match consume_wrapper_option_value(words, value) {
        WrapperValueConsumption::Consumed(remaining) => words = remaining,
        WrapperValueConsumption::Indeterminate => {
            indeterminate_after_scan = true;
            return Ok(UnwrappedCommand {
                words: original_words,
                indeterminate_child_context,
                indeterminate_after_scan,
                eval_context,
                time_keyword_allowed,
                eval_prefix_assignments_persist,
            });
        }
        WrapperValueConsumption::UnsafeExpansion => {
            return Err(UnwrapError::UnsafeExpansion);
        }
    }
}
```

Leave `SplitString` and `Unsupported` behavior unchanged.

- [x] **Step 7: Run focused tests and verify GREEN**

Run serially:

```bash
nix develop path:. --command cargo test invalid_or_unknown_wrapper_option_values_are_indeterminate -- --nocapture
nix develop path:. --command cargo test valid_wrapper_option_values_reach_destructive_children -- --nocapture
nix develop path:. --command cargo test invalid_wrapper_value_keeps_deny_precedence_in_both_orders -- --nocapture
nix develop path:. --command cargo test expanding_separate_wrapper_option_values_fail_closed -- --nocapture
nix develop path:. --command cargo test quoted_multi_argv_separate_wrapper_values_fail_closed -- --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_preserves_wrapper_value_uncertainty -- --exact --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider -- --nocapture
```

Expected: all commands exit 0. Confirm the provider test covers Codex, Claude,
and Antigravity and records zero model requests for every new `false` case.
Each output must name the requested test and report a nonzero executed-test
count; a zero-test success fails this step.

- [x] **Step 8: Review Task 1 scope and create the authorized local commit**

Completed with the repository's normalized whitespace check. Its local
`core.whitespace=indent-with-non-tab` configuration reports normal rustfmt
space indentation as errors, so plain `git diff --check` is not a valid gate
for this worktree.

Run:

```bash
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check 3132dd3d..HEAD
git diff --stat
git status --short
```

Expected: only the approved spec/plan and the three implementation/regression
files are changed; the normalized diff check exits 0. Stage only those five
files and create one local emoji-conventional commit that references
`codexctl-dchq`. Do not push.

---

### Task 2: Verify the complete dchq safety boundary

**Files:**
- Verify only: `src/brain/safety.rs`
- Verify only: `src/brain/permission_hook.rs`
- Verify only: `tests/shell_safety_helper_cli.rs`
- Verify only: `.internal/specs/2026-08-02-dchq-wrapper-value-semantics-design.md`
- Verify only: `.internal/plans/2026-08-02-dchq-wrapper-value-semantics.md`

**Interfaces:**
- Consumes: Task 1's typed classifier/consumer implementation and regression corpus.
- Produces: fresh differential, focused, full-suite, lint, formatting, build, and diff evidence suitable for independent review.

**Acceptance Criteria:**
- Harmless installed GNU/BusyBox probes confirm which children execute for invalid and contrast values.
- Focused evaluator, helper, provider, and unsafe-expansion tests pass.
- Serial `cargo test --all-targets`, Clippy with warnings denied, rustfmt check, and Cargo build pass through `nix develop path:.`.
- The normalized diff contains only approved files and no whitespace errors.
- No additional commit, push, PR, merge, version change, or publication occurs
  during final verification.

- [x] **Step 1: Replay harmless real-binary semantics**

Run each command separately and record exit status plus whether the marker was
printed:

```bash
/usr/bin/time -o '' sh -c 'printf TIME_CHILD'
/usr/bin/env -u '' sh -c 'printf ENV_EMPTY_CHILD'
/usr/bin/env -u '=HOME' sh -c 'printf ENV_EQUALS_CHILD'
/usr/bin/env -u=HOME sh -c 'printf ENV_ATTACHED_CHILD'
busybox time -o '' sh -c 'printf BUSYBOX_TIME_CHILD'
busybox env -u '' sh -c 'printf BUSYBOX_EMPTY_CHILD'
busybox env -u '=HOME' sh -c 'printf BUSYBOX_EQUALS_CHILD'
busybox env -u=HOME sh -c 'printf BUSYBOX_ATTACHED_CHILD'
```

Expected: both `time` empty-path probes and all direct `env` invalid-name probes
exit nonzero without printing their marker. BusyBox `env` probes exit 0 and
print their marker. These probes are evidence only; do not make tests depend on
installed host binaries.

Completed: harmless GNU and BusyBox probes recorded exit status and child-
marker presence for invalid, valid, bare-dynamic, prefix-proven, suffix-proven,
clustered, and `--` forms. The observed outcomes matched the audited grammar.

- [x] **Step 2: Run the focused safety corpus serially**

Run:

```bash
nix develop path:. --command cargo test attached_dynamic_wrapper_values_preserve_value_semantics -- --nocapture
nix develop path:. --command cargo test invalid_or_unknown_wrapper_option_values_are_indeterminate -- --nocapture
nix develop path:. --command cargo test valid_wrapper_option_values_reach_destructive_children -- --nocapture
nix develop path:. --command cargo test invalid_wrapper_value_keeps_deny_precedence_in_both_orders -- --nocapture
nix develop path:. --command cargo test expanding_separate_wrapper_option_values_fail_closed -- --nocapture
nix develop path:. --command cargo test quoted_multi_argv_separate_wrapper_values_fail_closed -- --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli -- --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider -- --nocapture
```

Expected: every command exits 0 with no warnings or unexpected stderr.
Every focused output must include its requested test or integration target and
a nonzero executed-test count.

Completed: focused evaluator regressions, the complete 22-test shipped-helper
target, and the all-provider matrix passed. The provider harness retained zero
model requests for Codex, Claude Code, and Antigravity deny/indeterminate
cases.

- [x] **Step 3: Run full repository quality gates serially**

Run one command at a time to avoid Nix/target-directory contention:

```bash
nix develop path:. --command cargo test --all-targets
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo build
```

Expected: all four commands exit 0. If an unrelated timing test fails, rerun
that exact test and compare against clean `origin/main`; do not alter unrelated
code or claim the full suite passed.

Completed serially: `cargo test --all-targets`, Clippy with warnings denied,
rustfmt check, and Cargo build each exited zero. The successful serial run is
the completion evidence; an exact parallel replay was environment-flaky and is
not represented as a passing gate.

- [x] **Step 4: Audit the final diff and stop at the authorization boundary**

Run:

```bash
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check 3132dd3d..HEAD
git diff --stat origin/main...HEAD
git status --short
git diff origin/main...HEAD -- src/brain/safety.rs src/brain/permission_hook.rs tests/shell_safety_helper_cli.rs
```

Expected: only approved files changed, every changed production line traces to
typed wrapper-value semantics, and no unrelated formatting or refactoring is
present. Report evidence and await explicit commit/PR authorization.

Completed: the normalized whitespace check passed, the whole range contained
exactly the approved five tracked files, and manual review found no unrelated
production or provider-policy change. Authorized local follow-up commits were
created; no push, pull request, merge, version, publication, or release action
occurred.

## Stress Test Results: Wrapper Option Value Implementation Plan

### Resolved Decisions

- Preserve strict RED ordering: evaluator, helper, and provider expectations
  are written and observed failing before production edits.
- Treat final-review tests for `-uX"$@"`, `-u"$@"X`, and their array equivalents
  as characterization when they pass immediately against the completed
  implementation. Record the coverage rationale and do not fabricate RED.
- Keep the owned `String` evidence local to private wrapper classifications.
  The small allocation preserves literal prefix/suffix proof from structured
  `ShellWord.parts`; it does not escape the scanner or create a public API.
- Stop flag parsing at the first value-taking short option; all remaining
  characters are its attached value.
- Require every focused Cargo command to name the requested test and execute a
  nonzero count; zero-test success is failure.
- Keep implementation and final verification as separate ordered tasks, and
  stop for a revised spec if `shell.rs` or another source file becomes needed.
- Return to root-cause analysis instead of stacking grammar exceptions; use
  surgical patch rollback and report unrelated full-suite failures honestly.
- Accept only exhaustive, zero-inference, launcher-preserving behavior that
  retains unsafe-expansion and deny-over-indeterminate precedence.
- Keep runtime cost bounded to the existing option-token scan plus one linear
  literal validation; no new search state, recursion, or unbounded allocation
  is introduced.
- Prefer the explicit three-outcome consumer enum over `Result`: invalid or
  unknown semantics and unsafe argv expansion have different policy outcomes
  and must not share one error path.

### Changes Made

- Added explicit nonzero-test and named-test requirements to RED, GREEN, and
  final focused verification steps.
- Clarified that scope expansion beyond the three approved source/test files
  requires a revised spec and plan rather than an implementation-time choice.
- Clarified that `Any` governs value content only after attachment presence is
  proven, and added literal-prefix/literal-suffix multi-argv characterization
  at evaluator, shipped-helper, and all-provider boundaries.

### Deferred / Parking Lot

- Do not generalize complete wrapper grammars. The approved private owned
  strings are limited to structured attachment evidence; broader ownership or
  public option-value APIs still require a separate design.
- Push, pull-request, merge, version, and release work remain outside this
  plan's authorization; one local Task 1 commit is authorized for review.

### Confidence Assessment

- Overall: High.
- Remaining concern: preservation of provider zero-inference behavior must
  continue to be demonstrated by the all-provider matrix rather than inferred
  from code inspection alone.
