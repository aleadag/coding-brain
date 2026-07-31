# Nested-Shell Destructive-Command Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> beads-superpowers:subagent-driven-development (recommended) or
> beads-superpowers:executing-plans to implement this plan task-by-task. Each
> Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within
> tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Prevent automatic approval of destructive or unresolved commands
executed through supported nested-shell boundaries while preserving normal
evaluation for fully analyzed benign literal `-c` and `eval` programs.

**Architecture:** Reuse the private Brush adapter recursively. Share one
bounded parser-work budget across the outer and nested programs, evaluate
literal `eval` against caller state, isolate child-shell state, and combine
results with deny-over-indeterminate precedence. Recognized shell execution
whose program or dialect cannot be inspected remains indeterminate before
model inference.

**Tech Stack:** Rust 1.88, `brush-parser` 0.4.x selected by `Cargo.lock`,
existing isolated shell-safety helper, Cargo tests through `nix develop`.

## Global Constraints

- Change only `src/brain/safety.rs`, `src/brain/safety/shell.rs`,
  `src/brain/permission_hook.rs`, `tests/shell_safety_helper_cli.rs`, and
  `CHANGELOG.md`.
- Keep `brush-parser` private to the root package; add no dependency or feature
  flag.
- Never execute shell code, expansions, script files, or standard-input
  programs during analysis.
- Preserve existing rule IDs, provider response contracts, audit ordering,
  helper isolation, trusted `HOME`, and all direct-shell regressions.
- Proven denial dominates uncertainty; unresolved nested execution never
  reaches model inference.
- Use a cumulative 64-KiB parse-byte budget, 131,072 projected-node budget,
  and eight nested string-evaluation boundaries.
- Preserve pathname-pattern exhaustion as its existing fail-closed expansion
  denial; byte, node, and nested-depth exhaustion are indeterminate.
- Do not commit, push, publish, or sync. End each task with a diff/status
  checkpoint.

## Execution Tracking Setup

Before Task 1, atomically import three child tasks under `codexctl-dchq`,
matching Tasks 1–3, and declare Task 1 → Task 2 → Task 3 ordering. Do not
create a duplicate epic. Copy each task's Acceptance Criteria into its child
Bead and close each child only after its task-local green checkpoint.

---

### Task 1: Share the Brush projection budget across recursive parses

**Files:**

- Modify: `src/brain/safety/shell.rs`
- Test: `src/brain/safety/shell.rs`

**Interfaces:**

- Consumes: existing `shell::analyze(&str) -> Result<ShellProgram,
  ShellAnalysisError>`.
- Produces:
  `shell::AnalysisBudget::default()` and
  `shell::analyze_with_budget(&str, &mut AnalysisBudget)`.
- Preserves: the existing `shell::analyze` convenience entry point for adapter
  tests that need a fresh budget.

**Acceptance Criteria:**

- Separate parses can consume one shared 131,072-node budget.
- A fresh `analyze` call retains current behavior.
- Budget exhaustion returns `ShellAnalysisError::ResourceLimit`.
- Existing adapter tests remain green.

- [ ] **Step 1: Add a failing shared-budget regression**

Add this test beside the existing adapter resource-limit tests in
`src/brain/safety/shell.rs`:

```rust
#[test]
fn shared_analysis_budget_accumulates_across_programs() {
    let mut probe = AnalysisBudget::with_limit(usize::MAX);
    analyze_with_budget(":", &mut probe).unwrap();
    let one_program = probe.visited();

    let mut budget = AnalysisBudget::with_limit(one_program * 2 - 1);
    assert!(analyze_with_budget(":", &mut budget).is_ok());
    assert_eq!(
        analyze_with_budget(":", &mut budget),
        Err(ShellAnalysisError::ResourceLimit)
    );
}

#[test]
fn fresh_analysis_keeps_an_independent_budget() {
    assert!(analyze(":").is_ok());
    assert!(analyze(":").is_ok());
}
```

- [ ] **Step 2: Run the new test and verify RED**

Run:

```bash
nix develop path:. --command cargo test shared_analysis_budget_accumulates_across_programs
```

Expected: compilation fails because `AnalysisBudget::with_limit`,
`AnalysisBudget::visited`, and `analyze_with_budget` are not exposed yet.

- [ ] **Step 3: Borrow a caller-owned budget in the adapter**

Refactor the existing budget without changing AST projection:

```rust
#[derive(Debug)]
pub(super) struct AnalysisBudget {
    visited: usize,
    limit: usize,
}

impl Default for AnalysisBudget {
    fn default() -> Self {
        Self::with_limit(MAX_ANALYSIS_NODES)
    }
}

impl AnalysisBudget {
    pub(super) fn with_limit(limit: usize) -> Self {
        Self { visited: 0, limit }
    }

    #[cfg(test)]
    fn visited(&self) -> usize {
        self.visited
    }

    fn visit(&mut self) -> Result<(), ShellAnalysisError> {
        self.visited = self
            .visited
            .checked_add(1)
            .ok_or(ShellAnalysisError::ResourceLimit)?;
        if self.visited > self.limit {
            return Err(ShellAnalysisError::ResourceLimit);
        }
        Ok(())
    }
}

struct Analyzer<'a> {
    options: ParserOptions,
    budget: &'a mut AnalysisBudget,
    nesting: usize,
    result: ShellProgram,
}

pub(super) fn analyze(input: &str) -> Result<ShellProgram, ShellAnalysisError> {
    analyze_with_budget(input, &mut AnalysisBudget::default())
}

pub(super) fn analyze_with_budget(
    input: &str,
    budget: &mut AnalysisBudget,
) -> Result<ShellProgram, ShellAnalysisError> {
    let options = ParserOptions::default();
    let mut parser = Parser::new(Cursor::new(input), &options);
    let program = parser
        .parse_program()
        .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?;
    let mut analyzer = Analyzer {
        options,
        budget,
        nesting: 0,
        result: ShellProgram {
            commands: Vec::new(),
            compound_redirects: Vec::new(),
            features: ExecutionFeatures::default(),
        },
    };
    analyzer.visit_program(&program)?;
    Ok(analyzer.result)
}
```

Keep every existing `self.budget.visit()` call unchanged. Adjust only lifetime
annotations required by the borrowed field.

- [ ] **Step 4: Verify GREEN and adapter compatibility**

Run:

```bash
nix develop path:. --command cargo test brain::safety::shell::tests
```

Expected: the new shared-budget tests and all existing shell-adapter tests
pass.

- [ ] **Step 5: Review the task diff**

Run:

```bash
git diff --check -- src/brain/safety/shell.rs
git diff -- src/brain/safety/shell.rs
git status --short
```

Expected: only the adapter budget ownership and its tests changed.

---

### Task 2: Recursively evaluate supported nested-shell boundaries

**Files:**

- Modify: `src/brain/safety.rs`
- Modify: `src/brain/permission_hook.rs`
- Create: `tests/shell_safety_helper_cli.rs`
- Test: `src/brain/safety.rs`
- Test: `src/brain/permission_hook.rs`
- Test: `tests/shell_safety_helper_cli.rs`

**Interfaces:**

- Consumes:
  `shell::analyze_with_budget`,
  `ShellProgram`,
  `ShellWord.literal`, existing wrapper normalization, and existing
  `SafetyEvaluation`.
- Produces private `EvaluationState`, `EvaluationBudget`,
  `EvaluationSummary`, and `NestedExecution` helpers.
- Produces recursive
  `evaluate_program(&str, &mut EvaluationState, &mut EvaluationBudget)`.
- Preserves public-crate entry points `evaluate_in_process` and
  `evaluate_isolated`.
- Consumes real normalized Codex, Claude, and Antigravity payloads through the
  normal provider test path.

**Acceptance Criteria:**

- Literal destructive `bash|sh|dash|ash -c`, multicall shell, and `eval`
  programs deny with existing rule IDs.
- Plain benign literal `-c` and `eval` programs return no deterministic
  decision.
- Recognized file/stdin shells, unsupported interpreters, dynamic payloads,
  ambiguous options, malformed nested input, and exhausted budgets are
  indeterminate.
- `eval` reads and mutates caller abstract state; child-shell state is
  isolated.
- Deny dominates indeterminate evidence across siblings.
- Inert quoted data remains non-executable.
- Provider deny and indeterminate cases invoke the model zero times with exact
  provider response envelopes.
- The shipped `cbrain --shell-safety-helper` dispatch emits the expected nested
  deny envelope.

- [ ] **Step 1: Add RED regressions for literal nested execution**

Add the destructive RED table beside `reopened_parser_backed_policy_corpus`:

```rust
#[test]
fn literal_nested_shell_destruction_denies() {
    for command in [
        "sh -c 'rm --no-preserve-root -rf /'",
        "/bin/bash -c 'rm --no-preserve-root -rf /'",
        "dash -c 'rm --no-preserve-root -rf /'",
        "ash -c 'rm --no-preserve-root -rf /'",
        "busybox sh -c 'rm --no-preserve-root -rf /'",
        "busybox ash -c 'rm --no-preserve-root -rf /'",
        "toybox sh -c 'rm --no-preserve-root -rf /'",
        "env bash -c 'rm --no-preserve-root -rf /'",
        "sudo sh -c 'rm --no-preserve-root -rf /'",
        "command bash -c 'rm --no-preserve-root -rf /'",
        "exec bash -c 'rm --no-preserve-root -rf /'",
        "time bash -c 'rm --no-preserve-root -rf /'",
        "eval 'rm --no-preserve-root -rf /'",
        "eval rm --no-preserve-root -rf /",
        "builtin eval 'rm --no-preserve-root -rf /'",
        "sh -c \"eval 'rm --no-preserve-root -rf /'\"",
    ] {
        let deny = evaluate_command(command).unwrap_or_else(|| panic!("{command}"));
        assert!(
            matches!(
                deny.rule_id,
                "irreversible-root-delete" | "unsafe-recursive-delete-expansion"
            ),
            "{command}: {}",
            deny.rule_id
        );
    }
}

```

Add existing-behavior characterization separately. Run this before production
editing and require it to remain green; do not count it as RED evidence:

```rust
#[test]
fn inert_nested_shell_text_stays_non_executable() {
    for command in [
        "printf '%s' \"sh -c 'rm -rf /'\"",
        "printf '%s' \"eval 'rm -rf /'\"",
    ] {
        assert_eq!(
            evaluate_result(command),
            SafetyEvaluation::NoDeterministicDecision,
            "{command}"
        );
    }
}
```

- [ ] **Step 2: Add RED regressions for uncertainty, state, and precedence**

Add explicit uncertainty RED tests:

```rust
#[test]
fn unresolved_nested_execution_is_indeterminate() {
    for command in [
        "sh -c \"$PROGRAM\"",
        "bash script.sh",
        "printf program | bash",
        "bash <<< 'printf ok'",
        "zsh -c 'printf ok'",
        "bash -lc 'printf ok'",
        "bash --rcfile profile -c 'printf ok'",
        "busybox --help sh -c 'printf ok'",
        "bash -c",
    ] {
        assert!(matches!(
            evaluate_result(command),
            SafetyEvaluation::Indeterminate(_)
        ), "{command}");
    }
}

#[test]
fn eval_state_flows_back_to_outer_commands() {
    assert_eq!(
        evaluate_result("eval 'TARGET=/tmp/safe'; rm -rf \"$TARGET\""),
        SafetyEvaluation::NoDeterministicDecision
    );
}
```

Add these characterization/security regressions separately; they may already
pass before implementation and are not RED evidence:

```rust
#[test]
fn child_shell_state_does_not_escape() {
    assert_eq!(
        evaluate_result("sh -c 'TARGET=/'; printf '%s' \"$TARGET\""),
        SafetyEvaluation::NoDeterministicDecision
    );
}

#[test]
fn proven_sibling_deny_dominates_nested_uncertainty() {
    assert!(matches!(
        evaluate_result("eval \"$UNKNOWN\"; rm --no-preserve-root -rf /"),
        SafetyEvaluation::Deny(_)
    ));
}
```

Also add named single-behavior tests for:

- `'sh' -'c' 'printf ok'` as exact quote-fragmented literal execution;
- `sh $'-\\x63' 'printf ok'` as dynamic/unsupported option input;
- `sh -c '$0 --no-preserve-root -rf /' rm` as unresolved positional command
  selection;
- `sh -- -c 'rm --no-preserve-root -rf /'` as no active `-c`;
- `bash -lc 'rm --no-preserve-root -rf /'` as deny despite option uncertainty;
- `bash -lc 'printf ok'` as indeterminate after a benign payload scan;
- dynamic/missing BusyBox applet selection as indeterminate;
- indeterminate→deny, deny→indeterminate, indeterminate→benign, and
  parent-indeterminate→benign nesting order;
- top-level `eval` propagation and pipeline/subshell non-propagation.

- [ ] **Step 3: Add the provider RED matrix before production changes**

Extend the existing provider payload helper with a command parameter:

```rust
fn permission_payload_for_provider_command(
    provider: AgentProvider,
    command: &str,
) -> Vec<u8> {
    let mut payload: serde_json::Value =
        serde_json::from_slice(&permission_payload_for_provider(provider, false))
            .unwrap();
    match provider {
        AgentProvider::Codex | AgentProvider::Claude => {
            payload["tool_input"]["command"] = serde_json::json!(command);
        }
        AgentProvider::Antigravity => {
            payload["toolCall"]["args"]["CommandLine"] = serde_json::json!(command);
        }
    }
    serde_json::to_vec(&payload).unwrap()
}
```

Add `nested_shell_safety_precedes_inference_for_every_provider` beside the
existing indeterminate provider matrix. Reuse its lifecycle/activity setup and
normal provider transaction, but do not inject a synthetic safety result.
Use an inference closure that panics for:

```rust
[
    "sh -c 'rm --no-preserve-root -rf /'",
    "sh -c \"$PROGRAM\"",
]
```

Assert exact envelopes:

- Codex/Claude deny:
  `hookSpecificOutput.decision.behavior == "deny"`;
- Antigravity deny: `decision == "deny"`;
- Codex/Claude indeterminate: empty stdout;
- Antigravity indeterminate: `decision == "ask"`;
- inference call count: zero.

Add a separate benign literal `sh -c 'printf %s ok'` matrix whose inference
closure returns a known suggestion and is called exactly once per provider.

- [ ] **Step 4: Run characterization and verify every RED failure**

Run:

```bash
nix develop path:. --command cargo test inert_nested_shell_text_stays_non_executable
nix develop path:. --command cargo test nested_shell -- --nocapture
nix develop path:. --command cargo test nested_execution -- --nocapture
nix develop path:. --command cargo test eval_state -- --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider
```

Expected:

- inert characterization passes;
- literal nested destruction reports no deterministic decision;
- unresolved shell execution reports no deterministic decision;
- safe `eval` state precision receives the current broad expansion denial;
- provider destructive input reaches the inference panic;
- no production source has changed yet.

- [ ] **Step 5: Introduce shared evaluation state and budget**

Move the current local assignment, IFS, and pattern budgets into private
state:

```rust
const MAX_RECURSIVE_PARSE_BYTES: usize = MAX_SHELL_COMMAND_BYTES;
const MAX_NESTED_EXECUTION_DEPTH: usize = 8;

struct EvaluationState {
    trusted_home: Option<String>,
    assignments: HashMap<String, String>,
    ifs_unknown: bool,
}

impl EvaluationState {
    fn trusted() -> Self {
        let mut assignments = HashMap::new();
        let trusted_home = std::env::var("HOME").ok();
        if let Some(home) = &trusted_home {
            assignments.insert("HOME".into(), home.clone());
        }
        Self {
            trusted_home,
            assignments,
            ifs_unknown: false,
        }
    }

    fn invalidate_mutable(&mut self) {
        self.assignments.clear();
        if let Some(home) = &self.trusted_home {
            self.assignments.insert("HOME".into(), home.clone());
        }
        self.ifs_unknown = true;
    }
}

struct EvaluationBudget {
    remaining_bytes: usize,
    remaining_nested: usize,
    shell: shell::AnalysisBudget,
    patterns: PatternMatchBudget,
}

impl EvaluationBudget {
    fn new() -> Self {
        Self::with_limits(
            MAX_RECURSIVE_PARSE_BYTES,
            MAX_NESTED_EXECUTION_DEPTH,
            shell::AnalysisBudget::default(),
        )
    }

    fn with_limits(
        remaining_bytes: usize,
        remaining_nested: usize,
        shell: shell::AnalysisBudget,
    ) -> Self {
        Self {
            remaining_bytes,
            remaining_nested,
            shell,
            patterns: PatternMatchBudget {
                remaining_states: MAX_PATTERN_MATCH_STATES,
                remaining_components: MAX_PATTERN_MATCH_COMPONENTS,
            },
        }
    }

    fn charge_parse(&mut self, source: &str) -> Result<(), ShellAnalysisError> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(source.len())
            .ok_or(ShellAnalysisError::ResourceLimit)?;
        Ok(())
    }

    fn evaluate_nested(
        &mut self,
        source: &str,
        state: &mut EvaluationState,
    ) -> SafetyEvaluation {
        let Some(remaining) = self.remaining_nested.checked_sub(1) else {
            return SafetyEvaluation::Indeterminate(ShellAnalysisError::ResourceLimit);
        };
        self.remaining_nested = remaining;
        let result = evaluate_program(source, state, self);
        self.remaining_nested += 1;
        result
    }
}

#[derive(Default)]
struct EvaluationSummary {
    indeterminate: Option<ShellAnalysisError>,
}

impl EvaluationSummary {
    fn observe(&mut self, result: SafetyEvaluation) -> Option<SafetyEvaluation> {
        match result {
            SafetyEvaluation::Deny(deny) => Some(SafetyEvaluation::Deny(deny)),
            SafetyEvaluation::Indeterminate(error) => {
                self.indeterminate.get_or_insert(error);
                None
            }
            SafetyEvaluation::NoDeterministicDecision => None,
        }
    }

    fn finish(self) -> SafetyEvaluation {
        self.indeterminate.map_or(
            SafetyEvaluation::NoDeterministicDecision,
            SafetyEvaluation::Indeterminate,
        )
    }
}
```

Keep trusted `HOME` immutable when invalidating other caller state.
Use `with_limits` in deterministic tests for exact/plus-one byte and depth
boundaries, many small sibling payloads, and depth restoration. Use Task 1's
probe-derived node limit for aggregate node exhaustion. Pathname-pattern
budget exhaustion retains its existing unsafe-expansion denial.

- [ ] **Step 6: Classify exact nested execution boundaries**

Add a private classifier after existing wrapper normalization:

```rust
enum NestedExecution {
    Eval(String),
    EvalUnresolved(ShellAnalysisError),
    ChildLiteral {
        program: String,
        indeterminate_after_scan: bool,
    },
    ChildUnresolved(ShellAnalysisError),
}

fn classify_nested_execution(
    words: &[&shell::ShellWord],
) -> Option<NestedExecution> {
    let command = command_name(words.first()?.literal.as_deref()?);

    if command == "builtin"
        && words.get(1).and_then(|word| word.literal.as_deref()) == Some("eval")
    {
        let arguments = words[2..]
            .iter()
            .map(|word| word.literal.as_deref())
            .collect::<Option<Vec<_>>>();
        return Some(arguments.map_or(
            NestedExecution::EvalUnresolved(ShellAnalysisError::UnsupportedSyntax),
            |arguments| NestedExecution::Eval(arguments.join(" ")),
        ));
    }
    if command == "eval" {
        let arguments = words[1..]
            .iter()
            .map(|word| word.literal.as_deref())
            .collect::<Option<Vec<_>>>();
        return Some(arguments.map_or(
            NestedExecution::EvalUnresolved(ShellAnalysisError::UnsupportedSyntax),
            |arguments| NestedExecution::Eval(arguments.join(" ")),
        ));
    }

    classify_shell_invocation(words)
}
```

Implement `classify_shell_invocation` with these exact transitions:

1. Direct literal basenames `bash|sh|dash|ash` enter interpreter parsing.
2. `busybox` requires a literal first applet `sh|ash`; `toybox` requires
   literal `sh`. Missing or dynamic selectors are `ChildUnresolved`; other
   literal applets return `None`.
3. Literal `zsh|ksh|mksh|fish` return `ChildUnresolved`.
4. Plain exact `-c` plus a literal following payload yields
   `ChildLiteral { indeterminate_after_scan: false }`.
5. A literal short cluster containing `c` plus any other letter can locate the
   following literal payload but yields
   `ChildLiteral { indeterminate_after_scan: true }`.
6. `--` before `-c`, long options, value-taking options, a non-option before
   `-c`, missing payload, or dynamic option/payload return `ChildUnresolved`
   without guessing.
7. `builtin eval` is recognized; a missing or dynamic builtin selector returns
   `EvalUnresolved` because it may select `eval`.

Do not scan arbitrary argument strings or add generic launcher recognition.

- [ ] **Step 7: Extract and recurse through `evaluate_program`**

Make `evaluate_in_process` initialize state and budget, then delegate:

```rust
pub(crate) fn evaluate_in_process(
    command: Option<&ShellCommandInput>,
) -> SafetyEvaluation {
    let Some(input) = command else {
        return SafetyEvaluation::NoDeterministicDecision;
    };
    if input.dialect != ShellDialect::Bash {
        return SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedDialect);
    }

    evaluate_program(
        &input.source,
        &mut EvaluationState::trusted(),
        &mut EvaluationBudget::new(),
    )
}
```

`evaluate_program` must:

1. charge source bytes;
2. call `shell::analyze_with_budget`;
3. preserve existing command-substitution/process-substitution/group denial;
4. run the current command loop against `EvaluationState`;
5. classify `eval` before generic executable-command state invalidation and
   before the existing `rm` branch;
6. recursively evaluate top-level `Eval` against the same state; apply current
   conservative conditional/loop invalidation; evaluate pipeline,
   asynchronous, subshell, and currently isolated contexts against a cloned
   state and discard effects;
7. recursively evaluate `ChildLiteral` through `evaluate_nested` against fresh
   trusted child state and
   remember indeterminate when `indeterminate_after_scan` is true;
8. route every nested result through `EvaluationSummary::observe`;
9. remember indeterminate results and continue safe sibling scanning; only a
   deny may return early inside a parsed program;
10. invalidate caller state after `EvalUnresolved` or indeterminate recursive
    `Eval`;
11. return a proven deny immediately;
12. return `EvaluationSummary::finish()` after the loop.

Pass `budget.patterns` to the existing target-risk functions so recursive
programs also share pathname-analysis work. Preserve immediate existing
dynamic-execution denials and outer parse-failure indeterminate returns.

- [ ] **Step 8: Exercise the helper protocol**

Add protocol-level tests using `run_helper_with` rather than injecting a safety
result:

```rust
#[test]
fn helper_protocol_projects_nested_deny() {
    let mut output = Vec::new();
    run_helper_with(
        Cursor::new("sh -c 'rm --no-preserve-root -rf /'"),
        &mut output,
    )
    .unwrap();

    assert!(matches!(
        decode_helper_response(&output),
        SafetyEvaluation::Deny(_)
    ));
}

#[test]
fn helper_protocol_projects_unresolved_nested_execution_as_indeterminate() {
    let mut output = Vec::new();
    run_helper_with(Cursor::new("sh -c \"$PROGRAM\""), &mut output).unwrap();

    assert!(matches!(
        decode_helper_response(&output),
        SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
    ));
}
```

The second assertion intentionally expects `HelperFailure`: the helper wire
format redacts internal indeterminate categories, and decoding preserves the
existing generic fail-safe result.

- [ ] **Step 9: Add the shipped-binary helper integration**

Create `tests/shell_safety_helper_cli.rs`:

```rust
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn shipped_helper_denies_literal_nested_root_deletion() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cbrain"))
        .arg("--shell-safety-helper")
        .env_clear()
        .env("HOME", "/tmp/cbrain-shell-safety-home")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"sh -c 'rm --no-preserve-root -rf /'")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "irreversible-root-delete");
}
```

The helper receives only inert source bytes; the test never invokes a shell.

- [ ] **Step 10: Verify GREEN across evaluator, provider, and helper**

Run:

```bash
nix develop path:. --command cargo test nested_shell -- --nocapture
nix develop path:. --command cargo test nested_execution -- --nocapture
nix develop path:. --command cargo test eval_state -- --nocapture
nix develop path:. --command cargo test nested_shell_safety_precedes_inference_for_every_provider
nix develop path:. --command cargo test --test shell_safety_helper_cli
nix develop path:. --command cargo test brain::safety::tests
```

Expected: nested and provider tests pass, every existing safety regression
remains green, and no test emits warnings or unexpected stderr.

- [ ] **Step 11: Review the task diff**

Run:

```bash
git diff --check -- src/brain/safety.rs src/brain/safety/shell.rs src/brain/permission_hook.rs tests/shell_safety_helper_cli.rs
git diff --stat
git status --short
```

Expected: only the approved private safety boundary and tests changed.

---

### Task 3: Document, review, and verify the completed boundary

**Files:**

- Modify: `CHANGELOG.md`
- Verify: `src/brain/safety.rs`
- Verify: `src/brain/safety/shell.rs`
- Verify: `src/brain/permission_hook.rs`
- Verify: `tests/shell_safety_helper_cli.rs`

**Interfaces:**

- Consumes the fully green Task 2 implementation and regressions.
- Produces accurate changelog text and final review/verification evidence.
- Introduces no new behavior.

**Acceptance Criteria:**

- The changelog accurately describes supported boundaries and limitations.
- Focused evaluator, helper, provider, and binary tests pass.
- `cargo test --all-targets`, Clippy, formatting, build, and Nix build pass
  serially.
- Requesting-code-review and verification-before-completion gates find no
  unresolved Critical or Important issue.
- Final diff contains only approved files.

- [ ] **Step 1: Update the changelog**

Replace the final nested-shell limitation sentence in the current direct-shell
safety entry with:

```markdown
Literal programs passed through supported Bash/POSIX `-c` and `eval`
boundaries are recursively analyzed with shared resource limits; proven
destructive commands deny before inference. Dynamic or malformed payloads,
unsupported shell dialects or options, and recognized shell execution from
files or standard input preserve provider-native confirmation without model
inference. Coverage is intentionally limited to the documented interpreter,
multicall applet, and wrapper registry.
```

- [ ] **Step 2: Run focused verification**

Run serially:

```bash
nix develop path:. --command cargo test brain::safety::tests
nix develop path:. --command cargo test brain::safety::shell::tests
nix develop path:. --command cargo test brain::permission_hook::tests
nix develop path:. --command cargo test --test shell_safety_helper_cli
nix develop path:. --command cargo fmt --all --check
```

Expected: all focused tests and formatting pass.

- [ ] **Step 3: Run full repository gates**

Run serially:

```bash
nix develop path:. --command cargo test --all-targets
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build
nix build path:. --no-link
```

Expected: every command exits zero with no warnings or failures.

- [ ] **Step 4: Run code-review and completion-verification gates**

Invoke `beads-superpowers:requesting-code-review` against the complete
post-fix diff. Resolve any Critical or Important finding through a new RED→GREEN
cycle, then rerun affected focused and full gates.

Invoke `beads-superpowers:verification-before-completion` only after review is
clean. Use fresh command output; never rely on results from before the final
edit.

- [ ] **Step 5: Run final diff and status checks**

Run:

```bash
git diff --check
git diff --stat
git diff -- src/brain/safety.rs src/brain/safety/shell.rs src/brain/permission_hook.rs tests/shell_safety_helper_cli.rs CHANGELOG.md
git status --short
```

If ordinary Rust or Markdown indentation alone triggers
`indent-with-non-tab`, normalize that diagnostic and rerun the equivalent
whitespace check. Expected final scope:

- `.internal/specs/2026-07-31-dchq-nested-shell-safety-design.md`
- `.internal/plans/2026-07-31-dchq-nested-shell-safety.md`
- `src/brain/safety.rs`
- `src/brain/safety/shell.rs`
- `src/brain/permission_hook.rs`
- `tests/shell_safety_helper_cli.rs`
- `CHANGELOG.md`

- [ ] **Step 6: Close the durable task hierarchy**

Close `codexctl-dchq` only after Task 3's review and fresh full verification pass.
Do not commit, push, publish, or Dolt-sync without explicit authorization.

## Stress Test Results: Nested-shell implementation plan

### Resolved Decisions

- Move provider RED tests before recursive production changes so the complete
  authorization boundary proves RED→GREEN.
- Separate characterization tests from genuine RED evidence and record the
  expected failure reason for each RED test.
- Add deterministic test limits and a single depth-restoring nested-evaluation
  helper; preserve existing pathname-budget denial behavior.
- Process `eval` before generic state invalidation and propagate effects only
  in contexts where the current evaluator tracks caller state.
- Specify every interpreter, multicall, option, and builtin classifier
  transition explicitly.
- Permit early return only for proven denial inside a parsed program; aggregate
  uncertainty across all safely inspectable siblings.
- Assert provider-specific response envelopes, combine real normalized
  provider unit tests with helper protocol tests, and add one shipped-binary
  helper integration.
- Run all-target Cargo gates and `nix build path:. --no-link`, followed by
  code-review and completion-verification skills.
- Use `codexctl-dchq` as the durable parent and preserve the conservative
  no-commit/no-push boundary.

### Changes Made

- Moved provider behavior tests from post-implementation Task 3 into Task 2's
  RED phase.
- Replaced masked state-propagation assertions with a safe-target precision
  regression that fails against current broad denial.
- Added exact budget-test seams, state-context rules, classifier transitions,
  response envelopes, binary-helper coverage, and serial full gates.
- Reduced Task 3 to documentation, review, verification, and tracker closure.

### Deferred / Parking Lot

- Arbitrary third-party launcher adapters remain outside the explicit registry.
- Commit, push, PR creation, and Dolt sync await separate user authorization.

### Confidence Assessment

- **Overall:** High.
- **Areas of concern:** The implementation must keep classifier option grammar
  narrow and avoid borrow-driven shortcuts that reset shared state or budgets.
