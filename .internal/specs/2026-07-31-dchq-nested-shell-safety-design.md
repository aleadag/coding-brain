# Nested-Shell Destructive-Command Safety Design

> **Date:** 2026-07-31
> **Brainstorm:** `codexctl-0ipv`
> **Issue:** `codexctl-dchq`
> **Status:** Approved and stress-tested
> **Research:** `.internal/research/2026-07-29-rust-shell-parser-safety-boundary.md`

## Context

The parser-backed deterministic safety evaluator recognizes destructive
commands in the provider's outer Bash program, including compound commands,
process and command substitutions, wrapper commands, and dynamic command or
argument forms. It does not interpret programs passed as strings through
`bash -c`, `sh -c`, or Bash's `eval` builtin. Brush parses those forms as an
outer command with ordinary arguments, so a destructive nested program can
currently proceed to model inference.

The earlier hand-written tokenizer deliberately did not recurse into these
strings because a lightweight second parser would have been unsound. The
current implementation has replaced that tokenizer with `brush-parser` 0.4.0,
has an explicit indeterminate result that bypasses model inference, and runs
analysis in a bounded isolated helper. This design reuses that maintained
parser and the existing destructive-command policy recursively instead of
adding another shell-language implementation.

## Goals

- Deterministically deny destructive commands proven inside literal
  `bash`/`sh -c` or `eval` programs.
- Allow fully analyzed benign literal nested programs to continue through the
  normal model-evaluation path.
- Preserve native provider confirmation, with zero model inference, whenever
  a nested program or its execution semantics cannot be analyzed safely.
- Treat every recognized nested shell invocation as an execution boundary,
  including script-file and standard-input forms that cannot be inspected
  without external state.
- Apply the same behavior to Codex, Claude Code, and Antigravity shell-command
  capabilities.
- Preserve all existing direct-shell safety behavior, rule identifiers,
  provider responses, audit ordering, and trusted-`HOME` handling.

## Non-Goals

- Executing commands or shell expansions during analysis.
- Resolving executables through `PATH`, consulting the filesystem, loading
  script files, resolving aliases or functions, or reading the live shell
  environment.
- Proving the safety of program strings for shell dialects not covered by the
  Bash/POSIX analysis contract.
- General shell allowlisting or changing when benign commands receive model
  approval.
- Changing configuration, activity schemas, provider hook payloads, TUI
  behavior, or public APIs.

## Enforcement Boundary

After the existing `time`, `exec`, `sudo`, `command`, and `env` wrapper
normalization, the evaluator inspects the actual command word and arguments.
Exact `builtin eval` is also recognized. Wrapper and interpreter recognition
uses a small explicit registry; ordinary arguments are never scanned
generically for strings that resemble shell commands.

### Recognized shell interpreters

The Bash/POSIX-compatible registry contains literal command basenames `bash`,
`sh`, `dash`, and `ash`, including absolute paths. Multicall forms
`busybox sh`, `busybox ash`, and `toybox sh` enter the same boundary after
their literal applet selector.

For these interpreters, an option sequence containing an active `-c` identifies
the word immediately after that option as the nested program. Combined short
options such as `-lc` contain an active `-c`. Exact per-interpreter option
parsing is required to locate the payload; a literal `--` ends shell option
processing, so a later `-c` is not a command-string option. Unknown,
abbreviated, value-taking, dynamic, or otherwise unsupported option forms are
indeterminate rather than guessed.

The payload is eligible for recursive analysis only when Brush projects it as
a completely literal word. Remaining arguments are positional parameters and
are not concatenated into the program or seeded into recursive analysis.

Only the plain, exact `-c` execution form is eligible to return
`NoDeterministicDecision` after a benign payload. Additional options may load
startup files, import external state, or alter language behavior. When an exact
supported grammar can still locate the payload, such as literal `-lc`, the
payload is scanned so proven destruction still denies, but an otherwise benign
result remains indeterminate. Multicall shells likewise require an exact
literal applet selector with no ambiguous launcher options.

Missing payloads, dynamic option words, dynamic payloads, ambiguous option
layouts, or unsupported option forms produce an indeterminate safety result.
They never reach model inference.

Every other invocation of a recognized shell interpreter is also a nested
execution boundary. Script-file, standard-input, here-string, here-document,
and pipeline-fed programs depend on input that this evaluator does not load or
execute, so those invocations are indeterminate. This closes equivalents such
as `bash script.sh`, `printf program | bash`, and `bash <<< program` without
claiming the external program is malicious.

Recognizable execution through other interpreters, including `zsh`, `ksh`,
`mksh`, and `fish`, is an unsupported dialect and therefore indeterminate.
Those inputs are not parsed as Bash and cannot receive automatic approval.
Unknown launchers remain outside this explicit registry; documentation must
refer to supported execution boundaries rather than imply universal
nested-process inspection.

### Bash `eval`

The literal command basename `eval` identifies Bash's builtin string-evaluation
boundary after wrapper normalization. When every argument is statically
literal, the evaluator joins the argument values with one ASCII space, matching
the builtin's command-string construction, and recursively analyzes the
result.

`eval` with no arguments is inert and produces no deterministic decision.
Empty literal arguments retain their position in the joined program. Any
dynamic argument makes the nested program indeterminate. The evaluator does
not invent option handling for `eval`, or attempt to predict parameter
expansion, field splitting, command substitution, or quote removal outside the
syntax information already projected by Brush.

## Recursive Evaluation

The implementation separates program evaluation from top-level helper setup:

```rust
fn evaluate_program(
    source: &str,
    state: &mut EvaluationState,
    budget: &mut ShellAnalysisBudget,
) -> SafetyEvaluation;
```

The names are private design contracts and may be adjusted to match the
existing module style. `EvaluationState` carries the conservative assignment
and invalidation state needed by the current policy. `ShellAnalysisBudget` is
shared by the outer analysis and every recursive call.

Nested programs use the same Brush adapter and the same destructive-command
policy as the outer program. No third-party AST types escape
`src/brain/safety/shell.rs`.

`eval` runs in the current shell, so it receives the caller's mutable abstract
state and propagates assignments, invalidations, and conservative unknown-state
changes back to subsequent outer commands. This makes a literal `eval` program
behave as though its parsed command stream occurred at that point instead of
creating a second semantics engine.

A new Bash/POSIX `-c` process receives a separate conservative state containing
only trusted context that the evaluator can establish without assuming export
state. Its state changes are discarded when the child evaluation returns.
Unknown inherited variables and positional parameters stay unresolved; when
they can select execution, flags, or destructive targets, the existing policy
fails closed.

## Result Precedence

Evaluation combines outer and nested results using this precedence:

1. `Deny` for any proven destructive command.
2. `Indeterminate` when no deny was proven but at least one relevant boundary
   could not be analyzed safely.
3. `NoDeterministicDecision` only when the complete supported input was
   analyzed without a matching safety rule.

The evaluator must continue examining safely available sibling commands after
a nested indeterminate result so that a later proven destructive command still
wins with a deterministic deny. Indeterminate `eval` invalidates all mutable
caller-shell state before sibling scanning because its effects occur in the
current shell. Indeterminate child-shell execution does not mutate outer
abstract state, but still makes the overall result indeterminate. A parse
failure that prevents structural inspection of the enclosing program remains
indeterminate.

All statically represented conditional branches are scanned conservatively.
Danger in any feasible branch denies. `NoDeterministicDecision` is returned
only when no deny and no indeterminate evidence occurred.

The permission-hook boundary is unchanged:

- `Deny` persists and emits the existing provider-specific deny response
  without model inference.
- `Indeterminate` preserves native provider confirmation without model
  inference.
- `NoDeterministicDecision` may proceed to provider policy and model
  evaluation.

## Resource Limits

One aggregate budget covers the outer source and all recursively analyzed
program strings:

- at most 64 KiB of cumulative source bytes submitted to parses, including the
  outer source;
- at most 131,072 projected AST nodes shared across every adapter traversal;
- at most eight nested string-evaluation boundaries.

Nested text is deliberately counted again because every recursive parse
consumes work. Recursive parsing may not reset a budget in a way that
multiplies attacker-controlled work at each level. The isolated helper's
existing wall-clock timeout and crash/failure handling remain the final
containment layer. Any byte, node, depth, helper-timeout, or parser limit
produces `ShellAnalysisError::ResourceLimit` or the existing helper failure
result, and therefore skips model inference.

## Security Properties

- Analysis never executes a nested program or expansion.
- Literal extraction remains quote-aware and comes only from the private Brush
  adapter.
- Only `ShellWord.literal` composed from exact literal and quote-preserving
  pieces may become a nested program. Parameters, command/arithmetic/process
  substitutions, tilde, pathname/brace expansion, locale translation, and
  undecoded ANSI-C escapes remain dynamic.
- Text that merely contains `sh -c`, `eval`, or a destructive command is inert
  unless it occupies an actual parsed execution boundary.
- Wrapper normalization happens before nested-boundary recognition, so forms
  such as `env bash -c`, `sudo sh -c`, `command bash -c`, and
  `exec bash -c` cannot bypass the check.
- Dynamic wrapper, interpreter, option, or payload words retain the existing
  fail-closed behavior.
- Recognized shell script-file and standard-input execution remains
  indeterminate because analysis does not read external program sources.
- Known denial takes precedence over uncertainty; uncertainty never becomes
  permission to infer.
- Shell dialect drift is fail-safe. Only Bash/POSIX-compatible literal program
  strings use recursive Brush analysis.
- Interpreter options that load startup files or alter language behavior can
  never turn a benign payload scan into automatic approval.

## Testing

Tests are written before production changes and must first demonstrate the
current bypass.

### Safety evaluator regressions

- Deny literal destructive programs through `sh -c`, `/bin/sh -c`,
  `bash -c`, `/bin/bash -lc`, `dash -c`, and `ash -c`.
- Deny the equivalent literal `busybox sh|ash` and `toybox sh` forms.
- Deny multiple nested shell levels and nested literal `eval`.
- Deny literal destructive `eval` programs built from one or multiple
  arguments.
- Deny the same forms behind `env`, `sudo`, `command`, `exec`, and `time`.
- Deny destructive literal `builtin eval`.
- Preserve normal evaluation for benign literal `sh -c`, `bash -c`, and
  `eval` programs.
- Preserve normal evaluation for quoted or escaped data that only mentions
  nested-shell syntax.
- Return indeterminate for dynamic options, dynamic program strings,
  unsupported interpreters, recognized shell script/stdin forms, malformed
  nested programs, missing `-c` payloads, and each aggregate resource limit.
- Verify `--` ends shell option processing and that positional parameters are
  not appended to the `-c` program.
- Verify plain `-c` can return no deterministic decision for a benign literal,
  while benign login/interactive/rcfile/language-option forms are
  indeterminate and destructive payloads in exactly parsed forms still deny.
- Verify unknown, value-taking, abbreviated, and dynamic interpreter options
  are indeterminate, and ambiguous multicall options are never guessed.
- Verify assignments and invalidations flow both into and out of literal
  `eval`, while child-shell state remains isolated.
- Verify a proven destructive sibling command wins over a nested indeterminate
  result.
- Verify indeterminate `eval` invalidates caller state before sibling analysis.
- Cover quote fragmentation, escaped metacharacters, undecoded ANSI-C
  spellings, and empty `eval` arguments.
- Cover the exact byte, node, and recursion limits plus limit-plus-one and
  aggregate sibling exhaustion cases.
- Preserve the complete existing direct-shell regression corpus.

### Provider-boundary regressions

For Codex `Bash`, Claude `Bash`, and Antigravity `run_command`:

- proven nested destruction deterministically denies and invokes the model zero
  times;
- dynamic or unsupported nested execution preserves native confirmation and
  invokes the model zero times;
- benign fully analyzed literal nested programs retain the existing inference
  behavior.

Exercise both the in-process evaluator and isolated helper protocol. Provider
tests use real normalized payloads in automatic mode and an inference closure
that panics if called for deny or indeterminate cases. Assert the existing
provider response exactly: deny for proven destruction, no decision response
for Codex/Claude indeterminate input, and `ask` for Antigravity indeterminate
input. A benign literal nested program calls inference exactly once.

### Verification gates

Run the focused safety and permission-hook tests first, then:

```bash
nix develop path:. --command cargo test
nix develop path:. --command cargo clippy --all-targets -- -D warnings
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo build
```

Run normalized `git diff --check` if the environment reports ordinary Rust or
Markdown indentation as `indent-with-non-tab`.

## Documentation

Update `CHANGELOG.md` to replace the tracked nested-shell limitation with the
implemented boundary:

- literal Bash/POSIX nested program strings are recursively analyzed;
- proven destructive nested commands are denied;
- dynamic, unsupported, malformed, or over-budget nested execution preserves
  native confirmation without model inference;
- recognized shell invocations whose program comes from a file or standard
  input also preserve native confirmation without model inference;
- coverage is limited to the explicit interpreter and wrapper registry.

No user configuration or migration documentation changes are required.

## Safe Rollback

The implementation adds no dependency or runtime feature flag. Recursive
projection stays behind the existing project-owned adapter, and the committed
lockfile continues to pin the selected `brush-parser` release. Any parser
lockfile change must replay the nested adversarial corpus.

New or unknown AST variants remain indeterminate. If selective recursive
analysis proves unreliable, the safe rollback is to make every recognized
nested execution boundary indeterminate. Rollback must never restore model
inference for an unresolved nested program.

## Stress Test Results: Nested-shell safety

### Resolved Decisions

- Treat every recognized shell invocation as an execution boundary; recursively
  analyze only exact literal Bash/POSIX `-c` programs and abstain on file/stdin
  or unsupported-dialect execution.
- Share mutable abstract state with literal `eval`, including effects flowing
  back to caller commands; isolate child-shell state.
- Use an explicit interpreter, multicall applet, and wrapper registry instead
  of scanning arbitrary arguments.
- Preserve deny-over-indeterminate precedence while invalidating caller state
  after indeterminate `eval`.
- Share exact cumulative byte, node, and nesting budgets across recursion.
- Accept nested payloads only from exact quote-aware literal projection.
- Prove behavior through the isolated helper and all three provider boundaries,
  including zero-inference assertions.
- Keep Brush private and define fail-safe abstention as the only acceptable
  rollback.
- Parse interpreter options per exact supported grammar; scan destructive
  payloads when their position is known, but abstain for benign forms whose
  options can load external state or change language semantics.

### Changes Made

- Expanded the boundary beyond `-c` to recognized shell script-file and
  standard-input execution.
- Added `ash`, multicall shell applets, unsupported-shell handling, and
  `builtin eval`.
- Made `eval` state propagation, indeterminate-state invalidation, result
  aggregation, exact resource limits, provider assertions, and rollback
  behavior explicit.
- Restricted normal evaluation to plain exact `-c`; login, interactive,
  rcfile, language-option, unknown, and ambiguous multicall forms remain
  indeterminate unless their exactly located payload proves a denial.

### Deferred / Parking Lot

- Arbitrary third-party launcher programs are not inferred from argument text;
  new launchers require an explicit, reviewed adapter.
- Provider-native policy may become primary only after equivalent behavior is
  independently proven for every supported provider.

### Confidence Assessment

- **Overall:** High.
- **Areas of concern:** Exact shell option and multicall-app argument grammars
  are easy to drift; parser or interpreter-support updates require the full
  adversarial corpus.
