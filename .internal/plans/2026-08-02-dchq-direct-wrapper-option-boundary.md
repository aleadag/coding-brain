# Direct Wrapper Option Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Stop direct external `time` and `env` forms with uncertain command-position semantics from producing unjustified deterministic denies while preserving proven nested-shell denies and provider-wide zero-model handling.

**Architecture:** Tighten the existing iterative wrapper normalization in `unwrap_command_with_context`; do not add another parser or runtime utility detection. `classify_time_option` and `classify_env_option` accept only the approved common grammar, and the `env` scan becomes a one-way options-to-assignments-to-command state transition. Existing `Deny > Indeterminate > NoDeterministicDecision` aggregation and provider hooks remain unchanged.

**Tech Stack:** Rust 2021, Brush shell AST, existing Coding Brain safety evaluator and permission-hook test harness, Cargo through `nix develop path:.`.

## Global Constraints

- Runtime utility identity is unavailable; do not infer GNU or BSD identity from a command spelling, path, or build target.
- Direct external `time` accepts only `-p`, `-a`, and `-o` with separate or attached output paths.
- Direct external `env` accepts only `-`, `-i`, `-u` with separate or attached names, `-v`, and `--` before assignments.
- Long options, abbreviations, implementation-specific options, incompatible options, malformed forms, and uncertain command positions are `Indeterminate` before model inference.
- `env` parsing is a one-way `options -> assignments -> command` transition; option parsing never resumes after the first literal `NAME=VALUE`.
- Every accepted separate option value preserves argv cardinality: `ShellWord` marks any expansion that may produce other than exactly one argv, including quoted multi-word parameter forms. Those values fail closed before the scanner consumes them; proven exact-one quoted scalars remain supported. This applies to direct, BusyBox, and nested builtin dispatch.
- Preserve `Deny > Indeterminate > NoDeterministicDecision`, current parser budgets, rule identifiers, provider payloads, public APIs, and multicall applet grammar.
- Deny and indeterminate paths invoke the Coding Brain model zero times for Codex, Claude Code, and Antigravity.
- Keep changes to `src/brain/safety.rs`, `src/brain/safety/shell.rs`, `src/brain/permission_hook.rs`, `tests/shell_safety_helper_cli.rs`, the user-authorized integration corpus in `tests/hook_activity.rs`, this plan, and the approved spec.
- Do not change versions, commit, push, open a pull request, merge, or publish.

---

### Task 1: Establish the direct-wrapper regression corpus

**Files:**
- Modify: `src/brain/safety.rs` test module near `ambiguous_time_options_are_indeterminate`
- Modify: `src/brain/permission_hook.rs` test `nested_shell_safety_precedes_inference_for_every_provider`
- Modify: `tests/shell_safety_helper_cli.rs` near `shipped_helper_preserves_multicall_terminating_option_uncertainty`

**Interfaces:**
- Consumes: `evaluate_result(&str) -> SafetyEvaluation`, `run_shipped_helper(&str, Option<(&str, &str)>) -> serde_json::Value`, and the existing provider test harness.
- Produces: a failing corpus that distinguishes direct-wrapper indeterminate forms from supported deterministic-deny controls.

**Acceptance Criteria:**
- Direct `/usr/bin/time -h`, `env --help`, `env --version`, `env -0 COMMAND`, abbreviated or implementation-specific options, and option-looking commands after assignments expect `Indeterminate`.
- Exact `time -p` and `env -i` controls continue to expect deterministic root-delete denial.
- Both statement orders preserve deny-over-indeterminate precedence.
- Shipped-helper expectations mirror evaluator expectations.
- Every provider sends uncertain forms to native confirmation with zero model requests.
- The evaluator, helper, and provider RED commands each identify at least one named assertion that fails because current code returns a hard deny.

- [ ] **Step 1: Add failing evaluator tests**

Add these tests beside the existing direct-wrapper option tests in `src/brain/safety.rs`:

```rust
#[test]
fn direct_external_wrapper_nonexecuting_forms_are_indeterminate() {
    for command in [
        "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time --verbose sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -vf FORMAT sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --ver sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -i0 sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -0i sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -a displayed sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --argv0=displayed sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -iC/tmp sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --chdir=/tmp sh -c 'rm --no-preserve-root -rf /'",
    ] {
        assert!(
            matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
            "{command}"
        );
    }
}

#[test]
fn env_assignments_end_option_parsing() {
    for command in [
        "env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar -- sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar BAR=baz -v sh -c 'rm --no-preserve-root -rf /'",
    ] {
        assert!(
            matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
            "{command}"
        );
    }

    for command in [
        "env -i FOO=bar sh -c 'rm --no-preserve-root -rf /'",
        "env -v FOO=bar BAR=baz sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
    }
}

#[test]
fn dynamic_direct_wrapper_transitions_remain_indeterminate() {
    for command in [
        "/usr/bin/time \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
        "env \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar \"$COMMAND\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        assert!(
            matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
            "{command}"
        );
    }
}

#[test]
fn direct_wrapper_common_option_arity_reaches_the_child() {
    for command in [
        "/usr/bin/time -o log sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -olog sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -aolog sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u HOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -uHOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -ivuHOME sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
    }
}

#[test]
fn uncertain_direct_wrapper_keeps_deny_precedence_in_both_orders() {
    for command in [
        "/usr/bin/env --help sh -c 'rm -rf /'; rm --no-preserve-root -rf /",
        "rm --no-preserve-root -rf /; /usr/bin/env --help sh -c 'rm -rf /'",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert_eq!(deny.rule_id, "irreversible-root-delete", "{command}");
    }
}
```

Extend the existing reserved-word control corpus with:

```rust
assert_eq!(
    evaluate_result("time -h sh -c 'rm --no-preserve-root -rf /'"),
    SafetyEvaluation::NoDeterministicDecision
);
```

Update existing expectations that conflict with the approved common grammar:

- in `env_argv0_forms_preserve_child_execution_and_uncertainty`, move the four destructive `env -a`/`--argv0` cases from deterministic deny expectations to `Indeterminate`;
- in `supported_wrappers_preserve_literal_and_dynamic_command_selection`, move `env -iC/tmp`, `env --chdir=/tmp`, and the four `/usr/bin/time` cases using `-v` or `-f` into the new indeterminate corpus;
- in `variable_expanded_command_after_supported_wrappers_denies`, move the three `env -iC` cases into an indeterminate assertion loop because the unsupported wrapper option must stop projection before the dynamic command;
- preserve `env --`, `env -u`, `/usr/bin/time -p`, `/usr/bin/time -a`, and `/usr/bin/time -o` deterministic-deny controls.

- [ ] **Step 2: Run evaluator tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test direct_external_wrapper_nonexecuting_forms_are_indeterminate -- --nocapture
nix develop path:. --command cargo test env_assignments_end_option_parsing -- --nocapture
nix develop path:. --command cargo test dynamic_direct_wrapper_transitions_remain_indeterminate -- --nocapture
nix develop path:. --command cargo test direct_wrapper_common_option_arity_reaches_the_child -- --nocapture
```

Expected: the regression commands fail because current code returns `Deny` for named reported forms. Record the first failing command and assertion from each corpus before production edits. The dynamic, reserved-word, supported-arity, and precedence controls may already pass and do not substitute for RED evidence.

- [ ] **Step 3: Add failing shipped-helper regression**

Add to `tests/shell_safety_helper_cli.rs`:

```rust
#[test]
fn shipped_helper_preserves_direct_wrapper_command_position_uncertainty() {
    for command in [
        "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -vf FORMAT sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --argv0=displayed sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar -- sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}
```

Also extend `shipped_helper_denies_literal_nested_root_deletion` with the two
supported direct controls:

```rust
"/usr/bin/time -p sh -c 'rm --no-preserve-root -rf /'",
"/usr/bin/env -i sh -c 'rm --no-preserve-root -rf /'",
```

- [ ] **Step 4: Run helper test and verify RED**

Run:

```bash
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_preserves_direct_wrapper_command_position_uncertainty -- --exact --nocapture
```

Expected: FAIL because the helper currently returns `deny` for the reported forms.

- [ ] **Step 5: Extend provider-wide zero-inference coverage**

Add these `false` entries to the corpus in `nested_shell_safety_precedes_inference_for_every_provider` in `src/brain/permission_hook.rs`:

```rust
(
    "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'",
    false,
),
(
    "env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'",
    false,
),
```

Change the existing `env --argv0=displayed ...` corpus entry from `true` to
`false`; it is implementation-specific and must now preserve native
confirmation rather than project the nested delete.

Keep these supported controls marked `true`:

```rust
("/usr/bin/time -p sh -c 'rm --no-preserve-root -rf /'", true),
("/usr/bin/env -i sh -c 'rm --no-preserve-root -rf /'", true),
```

- [ ] **Step 6: Run provider test and verify RED**

Run:

```bash
nix develop path:. --command cargo test brain::permission_hook::tests::nested_shell_safety_precedes_inference_for_every_provider -- --exact --nocapture
```

Expected: FAIL on a newly added uncertain form because current classification produces a hard provider denial instead of native confirmation.

- [ ] **Step 7: Review the red-only diff**

Run:

```bash
git diff --check
git diff -- src/brain/safety.rs src/brain/permission_hook.rs tests/shell_safety_helper_cli.rs
```

Expected: only regression tests changed; every new failure corresponds to a current false hard-deny.

---

### Task 2: Implement the closed direct-wrapper grammar

**Files:**
- Modify: `src/brain/safety.rs` functions `unwrap_command_with_context`, `classify_time_option`, and `classify_env_option`
- Modify: `src/brain/safety/shell.rs` `ShellWord` projection metadata
- Test: `src/brain/safety.rs`
- Test: `src/brain/permission_hook.rs`
- Test: `tests/shell_safety_helper_cli.rs`
- Test: `tests/hook_activity.rs`

**Interfaces:**
- Consumes: the Task 1 regression corpus and existing `EnvOption::{Supported, SplitString, Unsupported}` result contract.
- Produces: `classify_time_option(&str) -> Option<bool>` and `classify_env_option(&str) -> EnvOption` restricted to the approved common grammar; `unwrap_command_with_context` marks option-looking post-assignment command positions indeterminate.

**Acceptance Criteria:**
- All Task 1 regressions pass without changing permission-hook production code.
- Supported common forms still reach nested-shell analysis and deny proven root deletion.
- Unsupported or uncertain direct forms stop wrapper projection and become `Indeterminate`.
- Separate `time -o` / `env -u` and BusyBox `time -f` values that may produce zero or multiple argv fail closed through direct, BusyBox, and nested builtin paths, while proven exact-one values remain supported.
- BusyBox/Toybox-specific classifier behavior is unchanged.
- Focused evaluator, helper, provider, and adjacent multicall suites pass before this task closes.

- [ ] **Step 1: Restrict direct external `time` options**

Replace `classify_time_option` in `src/brain/safety.rs` with:

```rust
fn classify_time_option(word: &str) -> Option<bool> {
    if word.starts_with("--") {
        return None;
    }

    let mut options = word.strip_prefix('-')?.chars().peekable();
    options.peek()?;
    while let Some(option) = options.next() {
        match option {
            'o' => return Some(options.peek().is_none()),
            'a' | 'p' => {}
            _ => return None,
        }
    }
    Some(false)
}
```

This keeps attached `-oFILE` and separate `-o FILE` arity exact. It makes `-h`, `-q`, `-v`, `-f`, all long options, and incompatible true clusters indeterminate.

- [ ] **Step 2: Restrict direct external `env` options**

Replace `classify_env_option` with the closed short-option grammar below. Leave `classify_busybox_env_option` unchanged.

```rust
fn classify_env_option(word: &str) -> EnvOption {
    if word.starts_with("--") {
        return EnvOption::Unsupported;
    }

    let mut options = word
        .strip_prefix('-')
        .unwrap_or_default()
        .chars()
        .peekable();
    let mut child_context = false;
    while let Some(option) = options.next() {
        match option {
            'S' => return EnvOption::SplitString,
            'u' => {
                return EnvOption::Supported {
                    takes_separate_value: options.peek().is_none(),
                    child_context: true,
                };
            }
            'i' => child_context = true,
            'v' => {}
            _ => return EnvOption::Unsupported,
        }
    }
    EnvOption::Supported {
        takes_separate_value: false,
        child_context,
    }
}
```

The wrapper loop already consumes exact `--` before calling the classifier and handles a lone `-` separately. All long forms, `-0`, `-a`, `-C`, and unknown clusters now return `Unsupported`, which the existing caller converts to indeterminate.

- [ ] **Step 3: Make the `env` assignment transition one-way**

In the `(_, "env")` branch of `unwrap_command_with_context`, retain the existing `options_ended` variable, set it permanently on the first assignment, and use this branch ordering inside the word loop:

```rust
let mut options_ended = false;
while let Some(word) = words.first() {
    let Some(literal) = word.literal.as_deref() else {
        return if word_can_select_command(word) {
            Err(UnwrapError::DynamicOrUnsupported)
        } else {
            Ok(UnwrappedCommand {
                words,
                indeterminate_child_context,
                indeterminate_after_scan,
                eval_context,
                time_keyword_allowed,
                eval_prefix_assignments_persist,
            })
        };
    };
    if !options_ended && literal == "--" {
        options_ended = true;
        words = &words[1..];
    } else if !options_ended && literal == "-" {
        indeterminate_child_context = true;
        words = &words[1..];
    } else if !options_ended && literal.starts_with('-') && literal != "-" {
        // Keep the existing multicall-aware EnvOption match and value handling.
        // Do not change BusyBox/Toybox classification.
        let option = match multicall_launcher {
            Some("busybox") => classify_busybox_env_option(literal),
            Some("toybox") => EnvOption::Unsupported,
            Some(_) => unreachable!("multicall launcher registry"),
            None => classify_env_option(literal),
        };
        match option {
            EnvOption::Supported {
                takes_separate_value,
                child_context,
            } => {
                indeterminate_child_context |= child_context;
                words = &words[1..];
                if takes_separate_value && words.is_empty() {
                    indeterminate_after_scan = true;
                    return Ok(UnwrappedCommand {
                        words,
                        indeterminate_child_context,
                        indeterminate_after_scan,
                        eval_context,
                        time_keyword_allowed,
                        eval_prefix_assignments_persist,
                    });
                }
                if takes_separate_value {
                    words = &words[1..];
                }
            }
            EnvOption::SplitString => return Err(UnwrapError::UnsafeExpansion),
            EnvOption::Unsupported => {
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
        }
    } else if is_env_environment_argument(literal) {
        options_ended = true;
        indeterminate_child_context = true;
        words = &words[1..];
    } else if options_ended && literal.starts_with('-') {
        indeterminate_after_scan = true;
        return Ok(UnwrappedCommand {
            words: original_words,
            indeterminate_child_context,
            indeterminate_after_scan,
            eval_context,
            time_keyword_allowed,
            eval_prefix_assignments_persist,
        });
    } else {
        break;
    }
}
```

Do not extract a new state-machine abstraction: this is a single local transition in the existing wrapper scanner.

- [ ] **Step 4: Guard separate option-value cardinality and add end-to-end controls**

Extend `ShellWord` with metadata derived from Brush parameter expressions that
marks values which may not expand to exactly one argv. Cover unquoted field
splitting, brace/pathname expansion, quoted non-concatenating positional or
array-all expansion, member/name lists, and separate-word transforms. Preserve
quoted scalar parameters, scalar positional values, `$*`, and concatenating
array forms as exact-one controls.

Before consuming a separate value for any accepted `time` or `env` option,
return a typed unsafe-expansion failure when that metadata is set. Reuse the
check for shared direct and BusyBox scanner paths, including BusyBox `time -f`.
Propagate the typed failure as deterministic denial through nested
`builtin command` and `builtin exec`, while retaining existing indeterminate
handling for genuinely unsupported dispatch.

Add RED/GREEN evaluator, shipped-helper, and all-provider zero-inference
coverage for direct and BusyBox `time -o` / `env -u` parameter, brace, and
pathname expansion. Add one representative literal short `env -S` deny to the
helper and provider corpora; retain long and ANSI-C-obscured indeterminate
controls.

- [ ] **Step 5: Run the focused evaluator corpus**

Run:

```bash
nix develop path:. --command cargo test direct_external_wrapper_nonexecuting_forms_are_indeterminate -- --nocapture
nix develop path:. --command cargo test env_assignments_end_option_parsing -- --nocapture
nix develop path:. --command cargo test uncertain_direct_wrapper_keeps_deny_precedence_in_both_orders -- --nocapture
nix develop path:. --command cargo test ambiguous_time_options_are_indeterminate -- --nocapture
nix develop path:. --command cargo test supported_wrappers_preserve_literal_and_dynamic_command_selection -- --nocapture
```

Expected: PASS. The exact `env -a`/`--argv0`/`-C` and external `time -v`/`-f` expectations listed in Task 1 are now indeterminate; do not restore permissive classification.

- [ ] **Step 6: Run helper and provider regressions**

Run:

```bash
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_preserves_direct_wrapper_command_position_uncertainty -- --exact --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_denies_literal_nested_root_deletion -- --exact --nocapture
nix develop path:. --command cargo test brain::permission_hook::tests::nested_shell_safety_precedes_inference_for_every_provider -- --exact --nocapture
```

Expected: PASS; uncertain and deny paths each make zero model requests for all three providers.

- [ ] **Step 7: Run adjacent multicall regressions**

Run:

```bash
nix develop path:. --command cargo test multicall_terminating_or_unsupported_options_are_indeterminate -- --nocapture
nix develop path:. --command cargo test literal_nested_shell_destruction_denies -- --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_preserves_multicall_terminating_option_uncertainty -- --exact --nocapture
```

Expected: PASS with no BusyBox/Toybox behavior change.

- [ ] **Step 8: Inspect the surgical diff**

Run:

```bash
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
git diff --stat
git diff -- src/brain/safety.rs src/brain/safety/shell.rs src/brain/permission_hook.rs tests/hook_activity.rs tests/shell_safety_helper_cli.rs
sed -n '1,240p' .internal/specs/2026-08-02-dchq-direct-wrapper-option-boundary-design.md
sed -n '1,680p' .internal/plans/2026-08-02-dchq-direct-wrapper-option-boundary.md
```

Expected: the seven-file boundary contains the five tracked production/test files plus the approved specification and plan. Production changes are limited to `src/brain/safety.rs` and `src/brain/safety/shell.rs`, covering the shared direct/BusyBox wrapper scan and projected argv-cardinality metadata; all other tracked changes are regressions.

---

### Task 3: Verify runtime semantics and repository gates

**Files:**
- Verify: `src/brain/safety.rs`
- Verify: `src/brain/safety/shell.rs`
- Verify: `src/brain/permission_hook.rs`
- Verify: `tests/hook_activity.rs`
- Verify: `tests/shell_safety_helper_cli.rs`
- Verify: `.internal/specs/2026-08-02-dchq-direct-wrapper-option-boundary-design.md`
- Verify: `.internal/plans/2026-08-02-dchq-direct-wrapper-option-boundary.md`

**Interfaces:**
- Consumes: the completed Task 2 implementation and regression corpus.
- Produces: fresh runtime differential evidence, complete serial quality-gate evidence, and a clean handoff without publication.

**Acceptance Criteria:**
- Real GNU probes confirm the reported terminating/incompatible forms do not execute harmless child markers.
- The shipped helper returns `indeterminate` for uncertain forms and `deny` for supported destructive controls.
- Full serial tests, Clippy with warnings denied, formatting, and build pass.
- Final status lists only approved files and no commit or publication occurs.

- [ ] **Step 1: Rebuild the shipped helper**

Run:

```bash
nix develop path:. --command cargo build
```

Expected: exit 0.

- [ ] **Step 2: Replay harmless real-utility differentials**

Run each command separately and record its exit status and absence of the marker:

```bash
/usr/bin/time -h sh -c 'printf TIME_CHILD_MUST_NOT_APPEAR'
/usr/bin/env --help sh -c 'printf ENV_HELP_CHILD_MUST_NOT_APPEAR'
/usr/bin/env --version sh -c 'printf ENV_VERSION_CHILD_MUST_NOT_APPEAR'
/usr/bin/env -0 sh -c 'printf ENV_ZERO_CHILD_MUST_NOT_APPEAR'
/usr/bin/env FOO=bar -i sh -c 'printf ENV_ASSIGN_CHILD_MUST_NOT_APPEAR'
```

Expected on the current GNU host: no `*_CHILD_MUST_NOT_APPEAR` marker is printed. These probes are evidence only and are not added as host-dependent automated tests.

- [ ] **Step 3: Replay shipped-helper differentials**

Feed the same destructive corpus to `target/debug/cbrain --shell-safety-helper` one command at a time.

Expected:

```text
/usr/bin/time -h ...       -> {"result":"indeterminate"}
/usr/bin/env --help ...    -> {"result":"indeterminate"}
/usr/bin/env --version ... -> {"result":"indeterminate"}
/usr/bin/env -0 ...        -> {"result":"indeterminate"}
env FOO=bar -i ...         -> {"result":"indeterminate"}
/usr/bin/time -p ...       -> {"result":"deny","rule_id":"irreversible-root-delete"}
/usr/bin/env -i ...        -> {"result":"deny","rule_id":"irreversible-root-delete"}
```

- [ ] **Step 4: Run the full serial test suite**

Run without concurrent Cargo/Nix jobs against this worktree:

```bash
nix develop path:. --command cargo test --all-targets
```

Expected: exit 0. If an unrelated timing test fails, preserve the original output, rerun that exact test once for diagnosis, then rerun the complete serial command. Do not claim the full suite passed unless a fresh complete command exits 0.

- [ ] **Step 5: Run static quality gates serially**

Run:

```bash
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo build
nix build path:. --no-link
```

Expected: each command exits 0. The user accepted the separately tracked
`codexctl-kpco` exception only when an exact package failure is reproduced at
clean base with the same test names and counts; do not generalize that
exception to any dchq regression.

- [ ] **Step 6: Verify final scope and handoff state**

Run:

```bash
git diff --check
git status --short
git diff --stat
git diff -- src/brain/safety.rs src/brain/safety/shell.rs src/brain/permission_hook.rs tests/hook_activity.rs tests/shell_safety_helper_cli.rs
sed -n '1,240p' .internal/specs/2026-08-02-dchq-direct-wrapper-option-boundary-design.md
sed -n '1,680p' .internal/plans/2026-08-02-dchq-direct-wrapper-option-boundary.md
```

Expected: every changed line maps to the approved direct-wrapper fix. Report changed files, exact validation results, Beads status, and the proposed commit message; stop before commit, push, PR creation, merge, version change, or publication.

## Stress Test Results: Direct Wrapper Option Boundary Plan

### Resolved Decisions

- Require named failing assertions from evaluator, helper, and provider RED runs before production edits; already-green controls do not substitute for regression evidence.
- Cover accepted option arity and ordering as explicitly as rejected clusters and implementation-specific forms.
- Retain the existing local `options_ended` state instead of renaming or extracting a new state-machine abstraction.
- Relocate every affected existing GNU-specific regression into an explicit indeterminate corpus rather than deleting compatibility evidence.
- Exercise supported direct controls and uncertain forms across evaluator, shipped-helper, and every provider boundary.
- Close each ordered task only after its own evidence gate; stop for review if implementation exceeds the approved grammar.
- Preserve original failure output and require a fresh complete serial rerun before reporting a recovered full suite.
- Enforce the seven-file scope and make no provider production, runtime-detection, parser-architecture, version, or publication changes.
- Add `nix build path:. --no-link` after Cargo gates; keep crates.io packaging out of scope.

### Changes Made

- Added named RED evidence and task-closure requirements.
- Added accepted attached/separate value and ordering controls.
- Simplified the planned `env` implementation to update `options_ended` in place.
- Added helper deny controls and clarified relocation of existing option-family regressions.
- Strengthened full-suite recovery semantics and added the Nix package gate.

### Deferred / Parking Lot

- Provider-native policy as the primary long-term enforcement boundary remains outside this patch.
- Crates.io packageability is unrelated to the wrapper grammar and remains a release-level concern.

### Confidence Assessment

- Overall: High
- Areas of concern: The intentionally smaller common option grammar will create native-confirmation friction for implementation-specific forms; the explicit migrated corpora must remain visible.
