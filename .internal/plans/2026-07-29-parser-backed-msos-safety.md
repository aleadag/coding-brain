# Parser-Backed MSOS Safety Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Replace the repeatedly bypassed shell tokenizer with a Brush-backed, provider-dialect-aware, crash-isolated deterministic safety boundary that denies the complete reopened MSOS corpus before inference.

**Architecture:** Provider adapters attach an explicit Bash dialect to recognized command tools. The permission hook sends the bounded command over stdin to an isolated invocation of the current executable; that child parses with `brush-parser` 0.4.0, projects a private project-owned analysis model, and applies the existing destructive-delete policy. Parse, protocol, timeout, dialect, and unsupported-syntax failures return `Indeterminate` to the surviving parent, which preserves provider-native confirmation without model inference.

**Tech Stack:** Rust 1.88, edition 2024, `brush-parser` 0.4.0, Serde JSON, existing bounded Unix process runner, Cargo/Nix, Beads.

## Global Constraints

- Add `brush-parser = "0.4.0"` only to the root package; do not add `brush-core`.
- Keep Brush AST and word types private to `src/brain/safety/shell.rs`.
- Preserve rule IDs `irreversible-root-delete`, `irreversible-home-delete`, and `unsafe-recursive-delete-expansion`.
- Preserve the 64 KiB hook-input limit and cap projected AST/word visits at 131,072.
- Pass commands to the helper over stdin, never argv; cap helper output at 512 bytes and runtime at 2 seconds.
- Only `ShellDialect::Bash` is analyzable; unknown dialects produce `Indeterminate`.
- Do not execute expansions, consult the filesystem/environment, resolve `PATH`, or recursively parse `sh -c`/`eval`.
- Brush's initial-`]` compatibility shim may inspect only unquoted `WordPiece::Text`; it must not tokenize commands or redirects.
- No production legacy evaluator or feature flag remains after migration.
- Do not commit, push, publish, or open a PR without explicit user authorization. Commit steps below are authorization-gated handoff boundaries.

---

## File Structure

- `Cargo.toml`, `Cargo.lock` — root-only Brush dependency.
- `src/provider_hooks/mod.rs` — `ShellDialect`, `ShellCommandInput`, and normalized permission-request contract.
- `src/provider_hooks/{codex,claude,antigravity}.rs` — attach Bash authority only to recognized provider command tools.
- `src/brain/safety.rs` — public safety result, policy rules, helper wire protocol, in-process helper entry, and isolated parent entry.
- `src/brain/safety/shell.rs` — Brush-only parser adapter, project-owned analysis types, AST traversal, word classification, and initial-`]` compatibility shim.
- `src/brain/permission_hook.rs` — precedence and native-confirmation mapping for `Indeterminate`.
- `src/main.rs` — hidden `--shell-safety-helper` dispatch before configuration/model initialization.
- `tests/hook_activity.rs` — real-binary provider matrix and no-inference/failure-path coverage.
- `CHANGELOG.md` — accurate unreleased behavior and explicit non-goals.

### Task 1: Make Dialect and Analysis Outcomes Explicit

**Files:**
- Modify: `src/provider_hooks/mod.rs:35-70`
- Modify: `src/provider_hooks/codex.rs:35-90`
- Modify: `src/provider_hooks/claude.rs:55-110`
- Modify: `src/provider_hooks/antigravity.rs:70-125`
- Modify: `src/brain/safety.rs:1-125`
- Modify: `src/brain/permission_hook.rs:560-750`

**Interfaces:**
- Produces: `ShellDialect`, `ShellCommandInput`, `SafetyEvaluation`, and a permission-hook safety-evaluator seam used by Tasks 2 and 6.
- Consumes: existing `PermissionHookRequest`, `SafetyDeny`, provider-policy precedence, and provider-native abstention paths.

**Acceptance Criteria:**
- Codex/Claude `Bash` and Antigravity `run_command` normalize to `ShellCommandInput { dialect: Bash, source }`.
- Other tools carry no shell command input.
- `SafetyEvaluation::{Deny, NoDeterministicDecision, Indeterminate}` is exhaustive.
- Provider-policy deny outranks `Indeterminate`; `Indeterminate` outranks inference.
- `Indeterminate` invokes no inference and maps to no response for Codex/Claude and `ask` for Antigravity.
- Existing deterministic rule IDs and audit-failure behavior remain unchanged.

- [ ] **Step 1: Add failing provider normalization tests**

In each provider module, replace direct string assertions with:

```rust
assert_eq!(
    parsed.command,
    Some(ShellCommandInput {
        dialect: ShellDialect::Bash,
        source: "cargo test".into(),
    })
);
```

Also retain a non-command-tool case asserting `parsed.command.is_none()`.

- [ ] **Step 2: Run provider tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test provider_hooks --quiet
```

Expected: compilation fails because `ShellDialect` and `ShellCommandInput` do not exist.

- [ ] **Step 3: Introduce normalized shell authority**

Add in `src/provider_hooks/mod.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellDialect {
    Bash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ShellCommandInput {
    pub dialect: ShellDialect,
    pub source: String,
}
```

Change `PermissionHookRequest.command` to `Option<ShellCommandInput>`. In each provider adapter, wrap the already validated command string only for its recognized command tool:

```rust
let command = command.map(|source| ShellCommandInput {
    dialect: ShellDialect::Bash,
    source,
});
```

At existing string-only call sites, borrow the source explicitly:

```rust
let command = request
    .command
    .as_ref()
    .map(|command| command.source.as_str());
```

Use that borrowed string for activity redaction and deterministic safety evaluation. Clone `source` only where constructing the owned `BrainDecisionRequest`.

Update request-key construction to continue hashing the provider's original tool input, not this derived wrapper.

- [ ] **Step 4: Add failing safety-outcome and precedence tests**

Add to `safety.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SafetyEvaluation {
    Deny(SafetyDeny),
    NoDeterministicDecision,
    Indeterminate(ShellAnalysisError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellAnalysisError {
    UnsupportedDialect,
    UnsupportedSyntax,
    ResourceLimit,
    HelperFailure,
}
```

Add permission-hook tests through a new internal evaluator seam:

```rust
|_| SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedSyntax)
```

The fake inference closure must panic if called. Assert Codex/Claude write no decision and Antigravity writes `ask`. Add a provider-policy-deny case with the same indeterminate evaluator and assert the provider deny wins.

- [ ] **Step 5: Run the new tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test permission_hook::tests --quiet
```

Expected: compilation or assertions fail because the hook still treats `None` as the only non-deny safety outcome.

- [ ] **Step 6: Implement the three-way contract without changing the legacy policy**

Rename the current lexer-backed body to `evaluate_in_process`. Map its existing `Option<SafetyDeny>` result:

```rust
match legacy_evaluate(command.map(|input| input.source.as_str())) {
    Some(deny) => SafetyEvaluation::Deny(deny),
    None => SafetyEvaluation::NoDeterministicDecision,
}
```

Return `UnsupportedDialect` before inspecting source when the dialect is not Bash. Extract the hook's decision construction into an internal function accepting:

```rust
impl FnOnce(Option<&ShellCommandInput>) -> SafetyEvaluation
```

The production wrapper initially supplies `safety::evaluate_in_process`; Task 2 replaces that with the isolated entry.

Use this exact precedence:

```rust
match safety_evaluation {
    SafetyEvaluation::Deny(safety) => deterministic_safety_deny(safety),
    _ if request.provider_policy == ProviderPermissionPolicy::Denies => {
        deterministic_provider_deny()
    }
    SafetyEvaluation::Indeterminate(error) => abstain_without_brain(error.reason()),
    SafetyEvaluation::NoDeterministicDecision => evaluate_with_model(),
}
```

Keep reasons bounded constants; do not format the command or third-party errors.

- [ ] **Step 7: Run focused contract tests and verify GREEN**

Run:

```bash
nix develop path:. --command cargo test provider_hooks --quiet
nix develop path:. --command cargo test permission_hook::tests --quiet
```

Expected: all focused tests pass and fake inference is never called for deny or indeterminate outcomes.

- [ ] **Step 8: Authorization-gated commit boundary**

If and only if the user has authorized commits:

```bash
git add src/provider_hooks src/brain/safety.rs src/brain/permission_hook.rs
git commit -m "🛡️ refactor: make shell safety outcomes explicit"
```

Otherwise leave the task changes uncommitted and report the boundary.

### Task 2: Isolate Parser and Policy Evaluation

**Files:**
- Modify: `src/main.rs:220-260,450-510`
- Modify: `src/brain/safety.rs`
- Reuse: `src/provider_hooks/mod.rs:500-615`
- Test: `src/brain/safety.rs`
- Test: `src/main.rs`

**Interfaces:**
- Consumes: `ShellCommandInput`, `SafetyEvaluation`, and `evaluate_in_process` from Task 1.
- Produces: `evaluate_isolated`, hidden helper entry `run_helper`, and bounded `HelperResponse`.

**Acceptance Criteria:**
- Production permission evaluation invokes the current executable with hidden `--shell-safety-helper`.
- Command bytes travel through a temporary stdin file and never argv/environment.
- Helper runtime is at most 2 seconds and stdout at most 512 bytes.
- Timeout, crash/nonzero exit, malformed/oversized output, invalid rule ID, or spawn failure becomes `Indeterminate(HelperFailure)`.
- The parent accepts only the three canonical rule IDs and reconstructs canonical reasons locally.
- The helper dispatches before configuration loading or model initialization.
- Non-Unix builds return `Indeterminate` rather than using unsafe in-process fallback.

- [ ] **Step 1: Add failing wire-protocol tests**

Define the final protocol:

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
enum HelperResponse {
    Deny { rule_id: String },
    NoDeterministicDecision,
    Indeterminate,
}
```

Tests must accept each canonical response and reject:

```rust
r#"{"result":"deny","rule_id":"invented-rule"}"#
r#"{"result":"allow"}"#
b"not-json"
```

Expected rejected result:

```rust
SafetyEvaluation::Indeterminate(ShellAnalysisError::HelperFailure)
```

- [ ] **Step 2: Add deterministic process-failure tests**

Add an internal `evaluate_isolated_with` that accepts a prepared `std::process::Command`. Test:

- a fixture process returning valid JSON;
- `exit 7`;
- output of 513 bytes;
- a sleeping process exceeding a short test timeout;
- valid JSON followed by extra bytes.

Use the existing process-group timeout runner; do not assert a narrow elapsed-time window. Assert only the returned classification and that the child is reaped.

- [ ] **Step 3: Run helper tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test safety::tests::isolated --quiet
```

Expected: compilation fails because the helper protocol and isolated evaluator do not exist.

- [ ] **Step 4: Implement bounded isolated evaluation**

Add constants:

```rust
const HELPER_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_HELPER_OUTPUT_BYTES: usize = 512;
const MAX_SHELL_COMMAND_BYTES: usize = 64 * 1024;
```

Write the already-bounded source to `tempfile::tempfile()`, rewind it, and configure it as child stdin:

```rust
let mut input = tempfile::tempfile().map_err(|_| ShellAnalysisError::HelperFailure)?;
input.write_all(source.as_bytes()).map_err(|_| ShellAnalysisError::HelperFailure)?;
input.rewind().map_err(|_| ShellAnalysisError::HelperFailure)?;

let mut command = Command::new(std::env::current_exe().map_err(|_| ShellAnalysisError::HelperFailure)?);
command
    .arg("--shell-safety-helper")
    .stdin(Stdio::from(input))
    .env_clear();
```

Call the existing `provider_hooks::run_bounded_process`. Decode exactly one JSON object and map only the canonical rule IDs back through:

```rust
fn deny_for_rule_id(rule_id: &str) -> Option<SafetyDeny>
```

On `#[cfg(not(unix))]`, return `HelperFailure` without spawning or evaluating in-process.

- [ ] **Step 5: Implement the hidden helper entry**

Add a hidden Clap boolean:

```rust
#[arg(long, hide = true)]
pub(crate) shell_safety_helper: bool,
```

Dispatch it immediately after CLI parsing and before `Config::load()`:

```rust
if cli.shell_safety_helper {
    brain::safety::run_helper()?;
    return Ok(());
}
```

`run_helper` reads at most `MAX_SHELL_COMMAND_BYTES + 1`, rejects oversize input with a nonzero return, calls `evaluate_in_process` with Bash authority, serializes one `HelperResponse`, and writes only that JSON plus a newline.

- [ ] **Step 6: Switch production permission evaluation**

The normal hook supplies `safety::evaluate_isolated`. Unit tests use the evaluator seam from Task 1; helper unit tests call `evaluate_in_process` directly. Add a CLI test proving `--help` does not expose `--shell-safety-helper`.

- [ ] **Step 7: Run focused helper and hook tests**

Run:

```bash
nix develop path:. --command cargo test safety::tests::isolated --quiet
nix develop path:. --command cargo test permission_hook::tests --quiet
nix develop path:. --command cargo test permission_hook_flag_is_hidden --quiet
```

Expected: all pass; timeout/crash/malformed cases become indeterminate and no inference occurs.

- [ ] **Step 8: Authorization-gated commit boundary**

If authorized:

```bash
git add src/main.rs src/brain/safety.rs src/brain/permission_hook.rs
git commit -m "🛡️ feat: isolate deterministic shell analysis"
```

Otherwise leave changes uncommitted.

### Task 3: Project Brush Syntax into a Private Shell Model

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/brain/safety/shell.rs`
- Modify: `src/brain/safety.rs`
- Test: `src/brain/safety/shell.rs`

**Interfaces:**
- Consumes: `brush_parser::{Parser, ParserOptions, ast, word}` internally only.
- Produces: `shell::analyze(&str) -> Result<ShellProgram, ShellAnalysisError>`.

**Acceptance Criteria:**
- Brush is a root-only normal dependency and `brush-core` is absent from the dependency graph.
- Simple commands preserve assignment, command, argument, and redirect positions.
- Attached and separated redirects produce identical structural command positions.
- Pipelines and supported control-flow bodies expose their contained simple commands.
- Arithmetic commands and extended tests are inert structural contexts.
- Brace groups, subshells, coprocesses, and process substitutions set explicit execution features.
- Unsupported function/case/AST forms produce `UnsupportedSyntax`; they never become literal commands.
- Traversal visits at most 131,072 AST/word nodes.

- [ ] **Step 1: Add failing structural adapter tests**

Add table-driven tests for:

```rust
("rm>/dev/null -rf /", Some("rm"), vec!["-rf", "/"], 1),
("echo ready | rm -rf /", Some("rm"), vec!["-rf", "/"], 0),
("\"2\">/dev/null rm -rf /", Some("2"), vec!["rm", "-rf", "/"], 1),
("((0 || rm -rf / 1))", None, vec![], 0),
("[;]", Some("["), vec![], 0),
```

Also assert process substitution and `{ ...; }` set the corresponding `ExecutionFeatures`.

- [ ] **Step 2: Run the structural tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test safety::shell::tests::structure --quiet
```

Expected: compilation fails because the dependency and adapter do not exist.

- [ ] **Step 3: Add Brush and final project-owned types**

Add:

```toml
brush-parser = "0.4.0"
```

Create private types:

```rust
pub(super) struct ShellProgram {
    pub commands: Vec<ShellCommand>,
    pub features: ExecutionFeatures,
}

#[derive(Default)]
pub(super) struct ExecutionFeatures {
    pub command_substitution: bool,
    pub process_substitution: bool,
    pub executable_group: bool,
}

pub(super) struct ShellCommand {
    pub assignments: Vec<ShellAssignment>,
    pub command: Option<ShellWord>,
    pub arguments: Vec<ShellWord>,
    pub redirects: Vec<ShellRedirect>,
}

pub(super) struct ShellAssignment {
    pub name: String,
    pub value: ShellWord,
}

pub(super) struct ShellRedirect {
    pub target: Option<ShellWord>,
}

pub(super) struct ShellWord {
    pub raw: String,
    pub literal: Option<String>,
    pub parts: Vec<WordPart>,
}

pub(super) enum WordPart {
    Literal(String),
    TildeHome,
    Parameter(ParameterUse),
    Arithmetic,
    CommandSubstitution,
    AnsiCEscape,
    PathnamePattern,
    BraceExpansion,
}

pub(super) enum ParameterUse {
    Named { name: String },
    Fallback { name: String, value: ShellWord },
    Other,
}
```

Map a plain named parameter to `Named`. Map the `-`, `:-`, `=`, `:=`, `+`,
and `:+` default/assignment/alternative forms to `Fallback`, retaining the
parameter name and recursively classified operand word. Map length, substring,
prefix/suffix removal, substitution, transformation, and every other parameter
operation to `Other`. Do not store Brush AST types in these structs.

- [ ] **Step 4: Implement structural traversal**

Parse with:

```rust
let mut parser = Parser::new(Cursor::new(input), &ParserOptions::default());
let program = parser
    .parse_program()
    .map_err(|_| ShellAnalysisError::UnsupportedSyntax)?;
```

Walk `Program.complete_commands → CompoundListItem.0 → AndOrList → Pipeline.seq`. Match every `ast::Command` and `ast::CompoundCommand` variant explicitly:

- `Simple`: split `CommandPrefixOrSuffixItem` into assignments, words, redirects, and process substitution.
- `Compound::Arithmetic` and `ExtendedTest`: inert, while still inspect attached redirects for process substitution.
- `BraceGroup`, `Subshell`, and `Coprocess`: set execution-bearing features.
- `IfClause`, `WhileClause`, `UntilClause`, and `ForClause`: visit their condition/body lists.
- `ArithmeticForClause`: keep arithmetic headers inert and visit its body.
- `Function`, unsupported `CaseClause`, or any variant whose execution semantics are not projected: return `UnsupportedSyntax`.

Count each AST node, word, and word piece through one `AnalysisBudget`; return `ResourceLimit` after 131,072 visits.

- [ ] **Step 5: Keep redirects structural**

Match every `IoRedirect` variant. Store its optional FD, kind, and target separately. Inspect redirect target words for nested execution but never insert them into command arguments. Treat `IoFileRedirectTarget::ProcessSubstitution` and `CommandPrefixOrSuffixItem::ProcessSubstitution` as execution-bearing features.

- [ ] **Step 6: Run structural tests and inspect dependency scope**

Run:

```bash
nix develop path:. --command cargo test safety::shell::tests::structure --quiet
nix develop path:. --command cargo tree -p coding-brain
```

Expected: structural tests pass; `brush-parser` appears under the root package and `brush-core` does not appear.

- [ ] **Step 7: Authorization-gated commit boundary**

If authorized:

```bash
git add Cargo.toml Cargo.lock src/brain/safety.rs src/brain/safety/shell.rs
git commit -m "🧩 feat: project Brush shell syntax into safety IR"
```

Otherwise leave changes uncommitted.

### Task 4: Classify Words, Expansions, and the Initial-`]` Gap

**Files:**
- Modify: `src/brain/safety/shell.rs`
- Test: `src/brain/safety/shell.rs`

**Interfaces:**
- Consumes: Brush `word::parse`, `word::parse_brace_expansions`, `pattern::pattern_has_glob_metacharacters`, and Task 3's final IR.
- Produces: positive-literal `ShellWord` values used by Task 5 policy.

**Acceptance Criteria:**
- Only explicit literal, single-quoted, double-quoted-literal, and escaped-literal pieces contribute to `ShellWord.literal`.
- Parameter, command, arithmetic, tilde, ANSI-C escape, brace, and pathname forms are explicit project-owned parts.
- Arithmetic word content is reparsed with Brush word parsing so nested command substitution is visible.
- Brace expansion is detected only in unquoted text; `foo{bar}` remains literal while `/{,}` is active.
- Brush-recognized globs and the initial-`]` Bash form are active only in unquoted text.
- `/bin/r[]m]` and `/bin/r[\m]` are patterns; `[;]`, quoted brackets, escaped brackets, `foo[]`, and unmatched `foo[bar` are not.
- Unknown or failed word classification returns `UnsupportedSyntax`, never a safe literal.

- [ ] **Step 1: Add failing word-classification tables**

Include:

```rust
("/bin/r[]m]", WordExpectation::PathnamePattern),
("/bin/r[\\m]", WordExpectation::PathnamePattern),
("'[abc]'", WordExpectation::Literal("[abc]")),
("\\[abc\\]", WordExpectation::Literal("[abc]")),
("foo[]", WordExpectation::Literal("foo[]")),
("foo[bar", WordExpectation::Literal("foo[bar")),
("/{,}", WordExpectation::BraceExpansion),
("foo{bar}", WordExpectation::Literal("foo{bar}")),
("$'rm'", WordExpectation::Literal("rm")),
("$'\\x72m'", WordExpectation::AnsiCEscape),
("$((1 + $(resolve-target)))", WordExpectation::CommandSubstitution),
```

- [ ] **Step 2: Run classifier tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test safety::shell::tests::words --quiet
```

Expected: failures show unimplemented expansion classification and Brush's false result for `/bin/r[]m]`.

- [ ] **Step 3: Implement recursive positive-literal classification**

Call `word::parse(&word.value, options)`. Recursively classify:

- `Text`: append only after brace/glob classification;
- `SingleQuotedText`: append literally;
- `DoubleQuotedSequence`/`GettextDoubleQuotedSequence`: recurse with pattern/brace detection disabled;
- `EscapeSequence`: append its escaped payload literally;
- `AnsiCQuotedText`: append only when it contains no backslash; otherwise add `AnsiCEscape`;
- `TildeExpansion`: add `TildeHome` for home forms and an explicit nonliteral part for other forms;
- `ParameterExpansion`: convert to project-owned `ParameterUse`;
- command/backtick substitution: add `CommandSubstitution`;
- arithmetic: add `Arithmetic`, then call `word::parse` on `expr.value` and propagate any nested command substitution.

If any recursive Brush call errors, return `UnsupportedSyntax`.

- [ ] **Step 4: Implement brace and ordinary glob detection**

For each unquoted `Text`, call `word::parse_brace_expansions`; any returned `BraceExpressionOrText::Expr`, `NumberSequence`, or `CharSequence` adds `BraceExpansion`.

Call `pattern_has_glob_metacharacters(text, true)` for ordinary wildcard/bracket/extglob recognition. Never call either helper on quoted or escaped pieces.

- [ ] **Step 5: Add the narrow initial-`]` compatibility shim**

Implement:

```rust
fn has_initial_closing_bracket_pattern(text: &str) -> bool
```

For each unescaped `[`, skip optional `!` or `^`. Accept `]` as the first member only when at least one later member and a later unescaped closing `]` exist. Return false for `[]`, `foo[]`, unmatched brackets, or escaped opening brackets. The caller invokes this only for unquoted `Text` pieces whose Brush glob helper returned false.

Do not recognize `*`, `?`, extglob, separators, redirects, or quotes in this shim.

- [ ] **Step 6: Run classifier and structural tests**

Run:

```bash
nix develop path:. --command cargo test safety::shell::tests --quiet
```

Expected: all word and structural cases pass, including initial-`]` and inert `[;]`.

- [ ] **Step 7: Authorization-gated commit boundary**

If authorized:

```bash
git add src/brain/safety/shell.rs
git commit -m "🛡️ fix: classify Bash expansion provenance"
```

Otherwise leave changes uncommitted.

### Task 5: Migrate the Destructive-Delete Policy and Remove the Lexer

**Files:**
- Modify: `src/brain/safety.rs`
- Modify: `src/brain/safety/shell.rs`
- Test: `src/brain/safety.rs`

**Interfaces:**
- Consumes: `shell::analyze`, `ShellProgram`, `ShellWord`, `ExecutionFeatures`.
- Produces: final parser-backed `evaluate_in_process` and canonical helper responses.

**Acceptance Criteria:**
- Every existing safety behavior retains stable rule IDs except the documented
  Brush 0.4.0 `$`-plus-backslash-newline substitution gap, whose exact forms
  now assert `Indeterminate`.
- `/bin/r[]m]`, attached/quoted redirect cases, process substitution, groups, `/{,}`, variable recursive flags, ANSI-C root targets, and ANSI-C `env -S` options cannot reach inference.
- `"2">/dev/null rm -rf /`, `printf %s foo{bar}`, `((0 || rm -rf / 1))`, `[;]`, and all existing inert quoted/escaped cases return `NoDeterministicDecision`.
- Dynamic command position and execution-bearing substitutions remain broadly fail-closed.
- Arithmetic remains inert unless its structured content contains command substitution.
- The hand tokenizer, expansion scanners, redirection stripper, and obsolete helper tests are deleted.
- Nested `sh -c`/`eval` remains unchanged and tracked by `codexctl-dchq`.
- No pre-parse line-continuation normalizer or retry shim is added; normalized
  command substitutions still deny and normalized arithmetic remains inert.

- [ ] **Step 1: Add the complete reopened RED corpus**

Add table-driven assertions for all spec cases. At minimum:

```rust
let deny = [
    "/bin/r[]m] --no-preserve-root -rf /",
    "/bin/r[\\m] --no-preserve-root -rf /",
    "rm>/dev/null --no-preserve-root -rf /",
    ">'>' rm --no-preserve-root -rf /",
    "rm --no-preserve-root -rf /{,}",
    "FLAGS=-rf; rm $FLAGS /",
    "rm -rf $'\\x2f'",
    "env $'-\\x53' 'rm -rf /'",
];

let undecided = [
    "\"2\">/dev/null rm -rf /",
    "printf %s foo{bar}",
    "((0 || rm -rf / 1))",
    "[;]",
];
```

Assert exact rule IDs for denies and `NoDeterministicDecision` for undecided cases.
For `$\\\n(` and `$\\\n((` continuation forms that Brush 0.4.0 rejects,
assert `Indeterminate(UnsupportedSyntax)` and verify they cannot reach
inference. Keep equivalent normalized forms in their existing deny/undecided
tables.

- [ ] **Step 2: Run safety tests and verify RED**

Run:

```bash
nix develop path:. --command cargo test brain::safety::tests --quiet
```

Expected: the new corpus fails while the legacy evaluator remains active.

- [ ] **Step 3: Apply policy to parsed commands**

Start `evaluate_in_process` with:

```rust
let Some(input) = command else {
    return SafetyEvaluation::NoDeterministicDecision;
};
if input.dialect != ShellDialect::Bash {
    return SafetyEvaluation::Indeterminate(ShellAnalysisError::UnsupportedDialect);
}
let program = match shell::analyze(&input.source) {
    Ok(program) => program,
    Err(error) => return SafetyEvaluation::Indeterminate(error),
};
```

If `ExecutionFeatures` contains command/process substitution or executable grouping, return `unsafe-recursive-delete-expansion`. Otherwise iterate `program.commands`, preserving the existing assignment map, wrapper option handling, command basename matching, root/home lexical normalization, and rule reasons.

Use `ShellWord.literal` only for literal command/flag/target decisions. A dynamic command, recursive flag, or dangerous target returns `unsafe-recursive-delete-expansion`. Use project-owned `ParameterUse` for unset/empty/root default checks.

- [ ] **Step 4: Preserve wrapper semantics**

Port `sudo`, `env`, `exec`, `command`, and `time` option handling to literal words. Any dynamic option that can alter wrapper command selection is fail-closed. Preserve:

- attached/separate value-taking `sudo` and `env` options;
- abbreviated `env --split-string`;
- clustered `env -S` and options;
- leading assignments and `--` terminators.

Do not interpret `sh -c`, `bash -c`, or `eval` payloads.

- [ ] **Step 5: Delete the legacy tokenizer**

Remove `tokenize_commands`, `ShellExpansions`, quote-state scanners, arithmetic/glob/brace lookahead helpers, `without_redirections`, and tests that exercise those helpers directly. Retain only policy helpers that consume project-owned parsed words.

- [ ] **Step 6: Run focused safety suites in both targets**

Run:

```bash
nix develop path:. --command cargo test --lib brain::safety::tests --quiet
nix develop path:. --command cargo test --bin cbrain brain::safety::tests --quiet
```

Expected: all existing and reopened cases pass with no ignored tests.

- [ ] **Step 7: Authorization-gated commit boundary**

If authorized:

```bash
git add src/brain/safety.rs src/brain/safety/shell.rs
git commit -m "🛡️ fix: enforce destructive safety from parsed Bash"
```

Otherwise leave changes uncommitted.

### Task 6: Prove End-to-End Failure Safety and Release Compatibility

**Files:**
- Modify: `tests/hook_activity.rs`
- Modify: `src/brain/safety.rs`
- Modify: `CHANGELOG.md`
- Verify: `.github/workflows/ci.yml`
- Verify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: final provider normalization, isolated helper, parser adapter, and policy.
- Produces: end-to-end evidence, accurate docs, and dependency/size measurements.

**Acceptance Criteria:**
- Real-binary Codex, Claude, and Antigravity safety denies do not contact the fake model.
- Malformed/unsupported syntax, unsupported dialect, and helper failure preserve native confirmation without contacting the fake model.
- Provider-policy deny still wins over parser indeterminate.
- 64 KiB and deeply nested inputs complete safely; helper timeout/output bounds are proven.
- Full Cargo/Nix quality gates pass.
- Root dependency-count and release-binary-size deltas are recorded in the Beads task note.
- All configured Darwin/musl release targets remain required CI gates before publication.
- Changelog describes parser-backed direct-shell enforcement without claiming nested-shell/runtime resolution.

- [ ] **Step 1: Add end-to-end provider regressions**

Extend the existing fake-server hook harness with the reopened commands for each provider payload. Assert:

```rust
assert_eq!(fake_model.request_count(), 0);
assert_eq!(terminal.rule_id.as_deref(), Some(expected_rule_id));
```

For malformed Bash, assert Codex/Claude stdout is empty, Antigravity returns `ask`, activity is `NeedsInput`/abstained, and the fake model count remains zero.

- [ ] **Step 2: Add helper resource regressions**

Cover:

- a 64 KiB sequence of ordinary literal commands;
- deeply nested arithmetic parentheses without shell command splitting;
- deeply nested quoted words;
- helper timeout fixture;
- 513-byte helper output fixture;
- malformed helper JSON.

Do not use narrow wall-clock assertions. Assert classification, bounded output handling, and child reaping.

- [ ] **Step 3: Run focused integration tests**

Run:

```bash
nix develop path:. --command cargo test --test hook_activity safety --quiet
nix develop path:. --command cargo test helper --quiet
```

Expected: all provider and failure-path tests pass with zero fake-model calls.

- [ ] **Step 4: Update the unreleased changelog**

Add one scoped bullet under `[Unreleased]` stating that deterministic direct-shell safety now uses isolated Brush parsing, closes dynamic glob/brace/redirection/substitution variants, and preserves native confirmation for unsupported syntax. Explicitly state that nested `sh -c`/`eval` remains tracked separately.

- [ ] **Step 5: Record dependency and binary deltas**

Use the base revision `fde858ab` as the comparison point. Record commands and byte/count results in the task's Beads note:

```bash
nix develop path:. --command cargo tree -p coding-brain --edges normal
nix develop path:. --command cargo build --release
wc -c target/release/cbrain
```

Compare against an uncontaminated build of `fde858ab` in a temporary worktree; do not reset or overwrite this worktree.

- [ ] **Step 6: Run complete local quality gates serially**

Run:

```bash
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --quiet
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo build
nix build path:.#packages.x86_64-linux.default
git diff --check
```

Expected: every command exits 0. If repository whitespace attributes conflict with rustfmt, run the established normalized diff check and record both results rather than weakening formatting.

- [ ] **Step 7: Confirm publication gates without publishing**

Inspect `.github/workflows/release.yml` and record that x86_64/aarch64 Darwin plus static-musl Linux builds remain configured. Do not push or create a PR. Those CI targets must pass after publication is separately authorized.

- [ ] **Step 8: Authorization-gated commit boundary**

If authorized:

```bash
git add tests/hook_activity.rs CHANGELOG.md
git commit -m "✅ test: prove parser-backed shell safety"
```

Otherwise leave changes uncommitted and report all verification evidence.

---

### Task 7: Close Final Parser-Safety Review Bypasses

**Files:**
- Modify: `src/brain/safety.rs`
- Modify: `src/brain/safety/shell.rs`
- Modify: `src/brain/permission_hook.rs`
- Modify: `tests/hook_activity.rs`
- Modify: `CHANGELOG.md`

**Interfaces:**
- Consumes: the complete Task 1-6 parser-backed safety boundary.
- Produces: a conservative remediation of the final adversarial review without
  adding a lexer fallback, production test hook, or nested-shell inspection.

**Acceptance Criteria:**
- The isolated helper retains trusted literal-home policy context, and
  real-binary home deletion denies before inference.
- Quote-fragmented glob/brace syntax and all valid initial-`]` bracket classes
  cannot hide a destructive command or target.
- Unquoted parameter field splitting, tilde expansion, and stale tracked
  assignments cannot hide recursive flags or destructive targets.
- GNU `rm` recursive long-option abbreviations and supported `exec`/GNU `time`
  option grammars cannot expose a wrapped destructive command as safe.
- Quoted or escaped root globs remain literal and do not trigger a deterministic
  root-delete false positive.
- The exact reproductions pass through Codex, Claude, and Antigravity real
  binaries with zero model calls for deterministic denies.
- Helper timeout/reaping behavior and the changelog match the actual contract.
- Focused adversarial tests, full Cargo/Nix gates, and a fresh whole-change
  adversarial review pass.

- [ ] **Step 1: Add the final-review reproductions**

Add RED tests for:

- isolated `rm -rf "$HOME"`/literal-home deletion;
- `/bin/r["m"]`, `/{'',}`, `[!]]`, and `[^]]` active patterns;
- `X='safe -rf'; rm -f $X /`;
- `ROOT=/tmp; export ROOT=/; rm -rf "$ROOT"` and representative mutating
  builtins;
- `HOME=-rf; rm -f ~ /` and `PWD=-Rf; rm -f ~+ /`;
- `rm --rec --no-preserve-root /`;
- `exec -ca display rm -rf /` and
  `/usr/bin/time --out log rm -rf /`;
- quoted and escaped `/*` literals remaining undecided.

- [ ] **Step 2: Preserve trusted helper home context**

Keep `env_clear()` and restore only the trusted home value needed by policy, or
carry equivalent bounded context through the helper protocol. Do not expose
command text in argv or diagnostics.

- [ ] **Step 3: Make word provenance conservative across pieces**

Project enough whole-word quote/splitting provenance to detect active
glob/brace constructs spanning AST pieces. Complete the narrow initial-`]`
shim for negated singleton classes. Do not reintroduce textual shell lexing.

- [ ] **Step 4: Close state and option-policy gaps**

Treat unquoted parameter expansion and tilde expansion conservatively where
they can supply flags or targets. Invalidate tracked state across commands not
proven unable to mutate it. Implement the required `rm`, `exec`, and GNU `time`
option forms; ambiguous execution-bearing syntax remains indeterminate.

- [ ] **Step 5: Remove the quoted-root false positive**

Do not strip root-glob spellings from positive literals. Active glob expansion
must be represented by nonliteral provenance instead.

- [ ] **Step 6: Extend real-binary provider coverage**

Run every Critical reproduction through Codex, Claude, and Antigravity and
assert the canonical deny plus zero fake-model requests. Preserve native
confirmation for any intentionally indeterminate form.

- [ ] **Step 7: Verify and re-review**

Run focused unit/integration suites, formatting, full tests, Clippy with
warnings denied, Cargo build, the Nix package build, normalized diff hygiene,
and a fresh adversarial whole-change review. Record updated dependency/binary
deltas only if production dependencies or release size changed materially.

---

## Task Dependency Order

```text
Task 1: explicit dialect/outcome contract
  -> Task 2: isolated helper boundary
    -> Task 3: structural Brush adapter
      -> Task 4: word/expansion classification
        -> Task 5: policy migration and lexer removal
          -> Task 6: end-to-end and release verification
            -> Task 7: final adversarial-review remediation
```

Every task blocks the next because each consumes the exact interfaces produced
by its predecessor. Do not execute these tasks in parallel against the shared
files.

## Stress Test Results: Parser-Backed MSOS Implementation Plan

### Resolved Decisions

- Keep the task order so the bounded helper is active before Brush enters the
  production permission-hook path.
- Treat shell dialect as explicit provider-owned authority. Unrecognized tools
  or dialects remain indeterminate and cannot invoke inference.
- Use the Cargo-compatible `brush-parser = "0.4.0"` requirement with a
  committed lockfile, a private adapter, and full gates after lockfile updates.
- Accept only one complete, bounded helper response with canonical rule IDs.
  Every parser, protocol, process, and resource failure becomes indeterminate.
- Keep the 64 KiB input, 2-second runtime, 512-byte output, and 131,072-node
  projection bounds. Parser runtime and peak memory remain measured release
  gates because the node budget applies only after parsing.
- Ship no legacy fallback or runtime feature flag. Roll back the parser
  migration and lockfile together if the release gates fail.
- Require the complete unit, provider, helper, Cargo, Nix, dependency-delta,
  binary-size, Darwin, and static-musl evidence listed in Task 6.
- Preserve the fail-closed boundary: deterministic and provider-policy denies
  retain precedence; uncertain analysis preserves native confirmation without
  inference; command and parser details do not cross diagnostic boundaries.

### Changes Made

- Made string borrowing and ownership explicit at call sites after
  `PermissionHookRequest.command` becomes `ShellCommandInput`.
- Added `ResourceLimit` to the concrete analysis-error contract.
- Defined the project-owned execution-feature, assignment, redirect, and
  parameter-use types instead of leaving their shapes implicit.

### Deferred / Parking Lot

- Recursive enforcement inside `sh -c`, `bash -c`, and `eval` remains owned by
  `codexctl-dchq`.
- No alternate parser is implemented now; the private adapter is the
  replacement seam if Brush later fails a compatibility gate.

### Confidence Assessment

- Overall: High.
- Remaining concern: Brush parser CPU and memory behavior occurs before the
  projection budget. Task 6 treats maximum-size and deeply nested measurements
  as shipment-blocking evidence rather than assuming the helper timeout alone
  is sufficient.
