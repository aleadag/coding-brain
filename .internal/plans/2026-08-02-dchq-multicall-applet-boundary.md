# Multicall Applet Safety Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> beads-superpowers:subagent-driven-development (recommended) or
> beads-superpowers:executing-plans to implement this plan task-by-task. Each
> Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within
> tasks use checkbox (`- [ ]`) syntax for human readability.
>
> **Plan Epic:** `codexctl-ydxg`; reuse existing task `codexctl-zv4v` during
> execution rather than creating duplicates.

**Goal:** Prevent destructive or unresolved commands behind BusyBox/Toybox
applet dispatch from reaching model inference.

**Architecture:** Normalize exact `busybox|toybox env|time` composition inside
the existing iterative wrapper classifier, then reuse the existing nested-shell
analysis. Treat unsupported multicall selectors and ambiguous `time` options as
indeterminate, preserving deny-over-indeterminate precedence and provider-wide
zero-model behavior.

**Tech Stack:** Rust 1.88, existing Brush-backed shell evaluator, isolated
`cbrain --shell-safety-helper`, Cargo tests through `nix develop path:.`.

## Global Constraints

- Change only `src/brain/safety.rs`, `src/brain/permission_hook.rs`,
  `tests/hook_activity.rs`, `tests/shell_safety_helper_cli.rs`, `CHANGELOG.md`,
  and the approved spec/plan.
- Add no dependency, configuration field, public API, provider payload, or
  activity-schema change.
- Never execute analyzed shell text or inspect runtime applet availability.
- Keep the execution-bearing applet registry closed to audited `env` and
  `time`; unsupported selectors are `Indeterminate`.
- Preserve existing rule IDs, helper isolation, audit behavior, trusted
  `HOME`, parser budgets, and `Deny > Indeterminate > NoDeterministicDecision`.
- Codex, Claude Code, and Antigravity deny/indeterminate paths make zero model
  requests.
- Do not commit, push, publish, merge, bump versions, or sync Beads/Dolt without
  explicit user authorization.

---

### Task 1: Close multicall applet composition before inference

**Files:**

- Modify: `src/brain/safety.rs:2845-2890,2960-3335,4217-4255,5668-5683,6022-6065`
- Modify: `src/brain/permission_hook.rs:2134-2465`
- Modify: `tests/hook_activity.rs:1751-1860,2439-2495`
- Modify: `tests/shell_safety_helper_cli.rs:48-62`
- Modify: `CHANGELOG.md:55-75`

**Interfaces:**

- Consumes: `unwrap_command_with_context`, `classify_shell_invocation`,
  `SafetyEvaluation`, and the existing permission-hook/helper test harnesses.
- Produces: private `classify_time_option(&str) -> Option<bool>` and composed
  multicall normalization; no public interface changes.

**Acceptance Criteria:**

- BusyBox/Toybox `env` and `time` destructive shell payloads deterministically
  deny when their command position is exact.
- Unsupported, missing, dynamic, or option-like applet selectors and unknown or
  ambiguous `time` options are indeterminate.
- Benign exact `env`/`time` composition and inert quoted text retain normal
  evaluation.
- Codex, Claude Code, and Antigravity deny/indeterminate multicall paths invoke
  the model zero times.
- The exact reopened shipped-helper reproductions no longer return
  `no_deterministic_decision`.
- Existing nested-shell, wrapper, helper, and direct-shell regressions pass.

- [ ] **Step 1: Add failing evaluator regressions**

Extend `literal_nested_shell_destruction_denies` in `src/brain/safety.rs` with:

```rust
"busybox env sh -c 'rm --no-preserve-root -rf /'",
"busybox env -i sh -c 'rm --no-preserve-root -rf /'",
"busybox env -u HOME sh -c 'rm --no-preserve-root -rf /'",
"busybox time sh -c 'rm --no-preserve-root -rf /'",
"busybox time -p sh -c 'rm --no-preserve-root -rf /'",
"busybox time -o log sh -c 'rm --no-preserve-root -rf /'",
"toybox env sh -c 'rm --no-preserve-root -rf /'",
"toybox time sh -c 'rm --no-preserve-root -rf /'",
"busybox env toybox time sh -c 'rm --no-preserve-root -rf /'",
```

Extend `multicall_nested_execution_requires_an_exact_applet_selector` with
unsupported and option-like controls:

```rust
"busybox ls",
"busybox --list",
"toybox printf ok",
```

Add focused option and benign-control tests beside the existing wrapper tests:

```rust
#[test]
fn multicall_command_carrying_applets_preserve_benign_literal_programs() {
    for command in [
        "busybox env sh -c 'printf ok'",
        "busybox time sh -c 'printf ok'",
        "toybox env sh -c 'printf ok'",
        "toybox time sh -c 'printf ok'",
        "busybox env toybox time sh -c 'printf ok'",
        "printf '%s' \"busybox env sh -c 'rm -rf /'\"",
    ] {
        assert_eq!(
            evaluate_result(command),
            SafetyEvaluation::NoDeterministicDecision,
            "{command}"
        );
    }
}

#[test]
fn ambiguous_time_options_are_indeterminate() {
    for command in [
        "/usr/bin/time --unknown sh -c 'printf ok'",
        "/usr/bin/time --out log sh -c 'printf ok'",
        "/usr/bin/time --help sh -c 'printf ok'",
        "/usr/bin/time -o",
        "busybox time --unknown sh -c 'printf ok'",
        "busybox time -f",
        "toybox time -Z sh -c 'printf ok'",
    ] {
        assert!(
            matches!(evaluate_result(command), SafetyEvaluation::Indeterminate(_)),
            "{command}"
        );
    }
}
```

- [ ] **Step 2: Run evaluator regressions and verify RED**

Run:

```bash
nix develop path:. --command cargo test literal_nested_shell_destruction_denies -- --nocapture
nix develop path:. --command cargo test multicall_nested_execution_requires_an_exact_applet_selector -- --nocapture
nix develop path:. --command cargo test multicall_command_carrying_applets_preserve_benign_literal_programs -- --nocapture
nix develop path:. --command cargo test ambiguous_time_options_are_indeterminate -- --nocapture
```

Expected: destructive multicall inputs currently produce
`NoDeterministicDecision`; unsupported selectors and unknown external `time`
options, abbreviated long options, and missing option values do not yet become
`Indeterminate`. The inert quoted-text control passes.

- [ ] **Step 3: Add failing shipped-helper and provider regressions**

Add to `shipped_helper_denies_literal_nested_root_deletion` in
`tests/shell_safety_helper_cli.rs`:

```rust
"busybox env sh -c 'rm --no-preserve-root -rf /'",
"busybox time sh -c 'rm --no-preserve-root -rf /'",
"toybox env sh -c 'rm --no-preserve-root -rf /'",
"toybox time sh -c 'rm --no-preserve-root -rf /'",
```

Add these cases to the `(command, proven_deny)` matrix in
`nested_shell_safety_precedes_inference_for_every_provider`:

```rust
("busybox env sh -c 'rm --no-preserve-root -rf /'", true),
("busybox time sh -c 'rm --no-preserve-root -rf /'", true),
("toybox env sh -c 'rm --no-preserve-root -rf /'", true),
("toybox time sh -c 'rm --no-preserve-root -rf /'", true),
("busybox ls", false),
("busybox time --unknown sh -c 'rm --no-preserve-root -rf /'", false),
```

The existing panic-on-inference closure and provider assertions are retained;
`false` means native confirmation/`ask`, not automatic approval.

- [ ] **Step 4: Run helper/provider regressions and verify RED**

Run:

```bash
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_denies_literal_nested_root_deletion -- --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider -- --nocapture
```

Expected: the helper reports `no_deterministic_decision` for the new destructive
cases, and the provider matrix reaches its inference panic.

- [ ] **Step 5: Compose literal multicall wrappers in the existing loop**

In `unwrap_command_with_context`, before the wrapper `match`, consume only a
literal audited execution-bearing selector:

```rust
let wrapper_name = command_name(wrapper);
if matches!(wrapper_name, "busybox" | "toybox")
    && words
        .get(1)
        .and_then(|selector| selector.literal.as_deref())
        .is_some_and(|selector| matches!(selector, "env" | "time"))
{
    words = &words[1..];
    eval_context = EvalContext::External;
    time_keyword_allowed = false;
    eval_prefix_assignments_persist = false;
    continue;
}
```

This peels only the multicall launcher, leaving `env` or `time` for the existing
wrapper branch. Every successful iteration shortens `words`; it does not parse
again or reset a budget.

In `classify_shell_invocation`, change an unsupported literal BusyBox/Toybox
applet from `None` to the same fail-closed result used by missing, dynamic, or
option-like selectors:

```rust
if !supported {
    return Some(NestedExecution::ChildUnresolved(
        ShellAnalysisError::UnsupportedSyntax,
    ));
}
```

- [ ] **Step 6: Make external `time` option classification exact**

Replace permissive `time_option_takes_separate_value` with:

```rust
fn classify_time_option(word: &str) -> Option<bool> {
    if let Some(long) = word.strip_prefix("--") {
        let (name, attached) = long
            .split_once('=')
            .map_or((long, false), |(name, _)| (name, true));
        return match name {
            "output" | "format" => Some(!attached),
            "append" | "portability" | "quiet" | "verbose" if !attached => Some(false),
            _ => None,
        };
    }

    let mut options = word.strip_prefix('-')?.chars().peekable();
    options.peek()?;
    while let Some(option) = options.next() {
        match option {
            'o' | 'f' => return Some(options.peek().is_none()),
            'a' | 'h' | 'p' | 'q' | 'v' => {}
            _ => return None,
        }
    }
    Some(false)
}
```

Use it in the external `time` branch:

```rust
let Some(takes_value) = classify_time_option(option) else {
    indeterminate_after_scan = true;
    return Ok(UnwrappedCommand {
        words: original_words,
        indeterminate_child_context,
        indeterminate_after_scan,
        eval_context,
        time_keyword_allowed,
        eval_prefix_assignments_persist,
    });
};
words = &words[1..];
if takes_value && words.is_empty() {
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
if takes_value {
    words = &words[1..];
}
```

Keep exact `--` handling and the shell-keyword-only `-p` behavior unchanged.
Rename `abbreviated_gnu_time_value_option_reaches_the_wrapped_delete` to
`abbreviated_gnu_time_value_option_is_indeterminate` and replace its deny
assertion with:

```rust
assert!(matches!(
    evaluate_result("/usr/bin/time --out log rm -rf /"),
    SafetyEvaluation::Indeterminate(_)
));
```

Move the same abbreviated-option case from the provider deny corpus to the
provider indeterminate corpus, and add the four reopened multicall payloads to
the provider deny corpus.

- [ ] **Step 7: Verify focused evaluator, helper, and provider GREEN**

Run serially:

```bash
nix develop path:. --command cargo test literal_nested_shell_destruction_denies -- --nocapture
nix develop path:. --command cargo test multicall_nested_execution_requires_an_exact_applet_selector -- --nocapture
nix develop path:. --command cargo test multicall_command_carrying_applets_preserve_benign_literal_programs -- --nocapture
nix develop path:. --command cargo test ambiguous_time_options_are_indeterminate -- --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider -- --nocapture
nix develop path:. --command cargo test --test shell_safety_helper_cli shipped_helper_denies_literal_nested_root_deletion -- --nocapture
```

Expected: all focused tests pass; destructive paths deny, unresolved paths ask
without inference, and benign/inert controls retain their expected result.

- [ ] **Step 8: Add runtime evidence and documentation**

Build and probe the shipped helper directly with the exact adversarial inputs:

```bash
nix develop path:. --command cargo build --bin cbrain
for command in \
  "busybox env sh -c 'rm --no-preserve-root -rf /'" \
  "busybox time sh -c 'rm --no-preserve-root -rf /'" \
  "toybox env sh -c 'rm --no-preserve-root -rf /'" \
  "toybox time sh -c 'rm --no-preserve-root -rf /'"
do
  printf '%s' "$command" | target/debug/cbrain --shell-safety-helper
done
```

Expected: each helper response is JSON with `"result":"deny"` and the
existing `irreversible-root-delete` rule ID.

Run harmless installed-BusyBox probes:

```bash
busybox env sh -c 'printf busybox-env-ok'
busybox time sh -c 'printf busybox-time-ok' 2>&1
```

Expected: both print their marker, proving these applets execute the following
shell command on the available BusyBox build. If BusyBox is absent, record the
probe as unavailable; automated tests remain self-contained.

Update the current deterministic-safety entry in `CHANGELOG.md` with:

```markdown
BusyBox/Toybox `env` and `time` applet composition now reuses the nested-shell
boundary; unsupported multicall dispatch and ambiguous `time` options preserve
native confirmation without model inference.
```

- [ ] **Step 9: Run formatting and focused regression suites**

Run serially:

```bash
nix develop path:. --command cargo fmt --all
nix develop path:. --command cargo test brain::safety::tests -- --test-threads=1
nix develop path:. --command cargo test brain::permission_hook::tests -- --test-threads=1
nix develop path:. --command cargo test --test shell_safety_helper_cli -- --test-threads=1
nix develop path:. --command cargo fmt --all --check
```

Expected: all focused suites pass and formatting is clean.

- [ ] **Step 10: Run complete repository gates serially**

Run:

```bash
nix develop path:. --command cargo test --all-targets -- --test-threads=1
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build
nix build path:. --no-link
```

Expected: every command exits zero with no failed test or Clippy warning.

- [ ] **Step 11: Review the exact scope and hand off**

Run:

```bash
git diff --check
git diff -- src/brain/safety.rs src/brain/permission_hook.rs tests/hook_activity.rs tests/shell_safety_helper_cli.rs CHANGELOG.md .internal/specs/2026-08-02-dchq-multicall-applet-boundary-design.md .internal/plans/2026-08-02-dchq-multicall-applet-boundary.md
git status --short
```

Expected: every changed line traces to `codexctl-dchq`; no unrelated file,
version bump, commit, push, merge, publication, or tracker sync is present.

## Stress Test Results: Multicall Applet Safety Boundary Plan

### Resolved Decisions

- Keep evaluator, helper, provider, documentation, and gate work in one atomic
  security task rather than creating an unprotected intermediate state.
- Require distinct RED evidence for evaluator results, helper JSON, and model
  inference; benign controls are characterization rather than RED proof.
- Normalize one literal multicall launcher inline, force external execution
  context, and consume input iteratively without a new parser or recursion.
- Accept only exact, non-terminating `time` options with known argument
  consumption. Unknown options, abbreviations, terminating help/version forms,
  and missing values are indeterminate.
- Treat native-confirmation friction for unsupported multicall applets and
  unusual `time` invocations as an intentional compatibility cost.
- Preserve deny for structurally proven destruction and indeterminate for
  unresolved command position; both paths issue zero model requests.
- Require focused tests, direct built-helper probes, harmless real BusyBox
  probes, and complete serial Cargo/Nix gates.
- Roll back only task-owned lines with reviewed inverse patches; never use a
  broad reset or restore, and revisit architecture after three failed fixes.
- Confine production changes to `src/brain/safety.rs`; every other touched file
  is a regression, documentation, or planning artifact.

### Changes Made

- Added direct post-fix `target/debug/cbrain --shell-safety-helper` probes for
  all four audited multicall payloads.

### Deferred / Parking Lot

- Provider-native policy replacing deterministic defense in depth remains out
  of scope until provider parity and drift behavior are independently proven.

### Confidence Assessment

- Overall: High
- Areas of concern: Exact `time` option classification deliberately changes
  prior GNU abbreviation behavior and must remain explicit in tests and
  changelog text.
