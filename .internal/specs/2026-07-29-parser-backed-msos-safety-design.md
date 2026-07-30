# Parser-Backed MSOS Safety Design

> **Date:** 2026-07-29
> **Brainstorm:** `codexctl-cnij`
> **Issue:** `codexctl-msos`
> **Status:** Approved and stress-tested
> **Research:** `.internal/research/2026-07-29-rust-shell-parser-safety-boundary.md`

## Context

The deterministic destructive-command evaluator has been reopened repeatedly
after nearby valid Bash forms bypassed the hand-written tokenizer or produced
false positives. The remaining failures include bracket-pattern command names,
attached and quoted redirects, brace expansion versus literal braces, and
arithmetic syntax containing shell metacharacters. Each repair adds another
partial shell-grammar rule while losing provenance needed by a security
boundary.

This design replaces that tokenizer with `brush-parser` 0.4.0. The user chose a
direct migration rather than a separate acceptance-spike decision. Parser
compatibility, adversarial coverage, resource behavior, and release-target
builds remain completion gates for the migration.

This design supersedes
`.internal/specs/2026-07-28-reopened-msos-shell-provenance-design.md` and its
implementation plan. The incomplete lexer changes currently in
`src/brain/safety.rs` are migration input, not an implementation to extend.

## Goals

- Parse supported Bash-family input with a maintained parser rather than a
  project-specific shell tokenizer.
- Preserve the existing deterministic root/home recursive-deletion policy and
  rule IDs.
- Deny the reopened direct-shell bypass corpus without reintroducing the known
  quoted, brace, redirect, bracket, or arithmetic false positives.
- Make parser failure or unsupported syntax an explicit result that cannot
  reach automatic model inference.
- Keep the parser dependency private to the root package and keep policy code
  independent of third-party AST types.
- Remove the hand-written production tokenizer after parser-backed parity.

## Non-Goals

- Executing shell expansions or commands.
- Resolving globs against the filesystem, consulting the environment, looking
  up `PATH`, or resolving aliases and functions.
- Recursively enforcing programs passed through `sh -c`, `bash -c`, `eval`, or
  equivalent string-evaluation boundaries. `codexctl-dchq` owns that work.
- Turning the deterministic evaluator into a general shell allowlist.
- Adding `brush-core` or any Brush execution/runtime component.
- Changing provider-native prompt behavior, provider policy, or safety rule
  identifiers.

## Dependency Boundary

Add `brush-parser = "0.4.0"` only to the root package dependencies.
This is a Cargo-compatible version requirement rather than an exact
`=0.4.0` pin. The committed `Cargo.lock` selects the reproducible version;
reviewed lockfile updates must rerun the complete safety and release gates.
`coding-brain-core` and `coding-brain-tui` must not acquire the dependency.

All Brush types remain inside a private adapter under `src/brain/safety/`.
Policy evaluation consumes only project-owned analysis types. This isolates AST
API changes, keeps dependency direction unchanged, and lets parser-specific
tests remain separate from destructive-delete policy tests.

The adapter uses the parser crate only. It must not construct a Brush shell,
invoke expansion helpers that depend on live shell state, or link `brush-core`.

## Project-Owned Analysis Model

The adapter parses the complete command and projects only policy-relevant
structure:

```rust
struct ShellProgram {
    commands: Vec<ShellCommand>,
    execution_features: ExecutionFeatures,
}

struct ShellCommand {
    context: ExecutionContext,
    assignments: Vec<ShellAssignment>,
    command: Option<ShellWord>,
    arguments: Vec<ShellWord>,
    redirects: Vec<ShellRedirect>,
}

struct ShellWord {
    source_span: Option<Range<usize>>,
    literal_text: Option<String>,
    parts: Vec<ShellWordPart>,
}

enum ShellWordPart {
    Literal { text: String, quoted: bool },
    TildeExpansion,
    ParameterExpansion,
    ArithmeticExpansion,
    CommandSubstitution,
    ProcessSubstitution,
    AnsiCQuoted { decoded: Option<String>, has_escape: bool },
    PathnamePattern,
    BraceExpansion,
}
```

The names above are design-level contracts, not mandatory public names.
Implementation may combine fields when that makes the private representation
smaller, but it must preserve the same information.

`ExecutionContext` distinguishes top-level commands, pipelines, groups,
subshells, command substitutions, and process substitutions. Redirects are
structural and never occupy command or argument positions. Their targets remain
quote-aware `ShellWord` values, and any nested execution-bearing word parts are
still visible.

`literal_text` is present only when the word can be determined from syntax
without runtime expansion. ANSI-C text without escapes may produce literal
text; ANSI-C escape decoding must either be exact and tested or remain
non-literal. Source spans are optional and diagnostic-only. A missing span does
not make otherwise fully classifiable syntax indeterminate. If Brush does not
expose a required word classification, the adapter returns `Indeterminate`
rather than reconstructing it with the legacy lexer.

Third-party non-exhaustive or newly added AST variants map to unsupported input.
They never silently fall through as ordinary literal words.

Word projection uses a positive literal model. Only explicitly recognized
literal and inert quoted parts may contribute to `literal_text`. Every
expansion-bearing part must be classified explicitly; an unknown, newly added,
or partially decoded part produces `Indeterminate`.

Brush 0.4.0's public glob classifier does not recognize Bash's valid
initial-`]` bracket-member form: it returns false for `/bin/r[]m]`, although
Bash expands that word to `/bin/rm`. The adapter includes one narrow
compatibility classifier for this form. It operates only on unquoted
`WordPiece::Text` after Brush has established word boundaries and quote/escape
provenance. It recognizes an optional `!`/`^`, an initial `]` member, at least
one subsequent member, and the later closing `]`. It does not split commands,
parse redirects, or classify general shell syntax. Brush remains authoritative
for all other glob constructs.

## Shell-Dialect Authority

Provider normalization must retain the expected shell dialect alongside the
command capability. The implementation must verify the execution contract for
Codex `Bash`, Claude `Bash`, and Antigravity `run_command` on supported
platforms.

Brush analysis is authoritative only for provider/tool contracts established
as Bash-compatible. An unknown, unsupported, or drifting dialect produces
`Indeterminate`. A successful Bash parse must not authorize inference for a
command that another shell may interpret differently.

## Evaluation Contract

Replace the ambiguous `Option<SafetyDeny>` return value with an explicit result:

```rust
enum SafetyEvaluation {
    Deny(SafetyDeny),
    NoDeterministicDecision,
    Indeterminate(ShellAnalysisError),
}
```

- `Deny` means a known deterministic safety rule matched.
- `NoDeterministicDecision` means parsing and analysis succeeded but no
  deterministic rule matched.
- `Indeterminate` means syntax, parser output, resource limits, or an
  unsupported construct prevented safe analysis.
- A missing command capability remains `NoDeterministicDecision`; it is not a
  parser failure.

The permission-hook precedence is:

1. `SafetyEvaluation::Deny`;
2. provider-policy deny;
3. `SafetyEvaluation::Indeterminate`;
4. existing model evaluation for `NoDeterministicDecision`.

This ordering preserves provider-specific deterministic denies even when shell
analysis is indeterminate. `Indeterminate` must become the existing abstention
path before the inference callback is invoked:

- Codex and Claude receive no hook decision, leaving native confirmation in
  control.
- Antigravity receives `ask`.

The stored reason is bounded and generic, such as
`unsupported-shell-syntax`; it must not include the submitted command, parser
debug output, or source fragments.

## Policy Semantics

The parser migration preserves the established deterministic policy:

- Literal `rm` invocations with recursive flags and root targets use
  `irreversible-root-delete`.
- Literal `rm` invocations with recursive flags and home targets use
  `irreversible-home-delete`.
- Unresolved, empty, root-valued, or execution-bearing expansion shapes use
  `unsafe-recursive-delete-expansion`.
- Supported wrappers retain their current option handling: `sudo`, `env`,
  `exec`, `command`, `time`, and the existing control constructs.
- Active GNU `env -S`/`--split-string` command payloads remain dynamic
  execution and use `unsafe-recursive-delete-expansion`.

The following syntax remains fail-closed with
`unsafe-recursive-delete-expansion`:

- command or process substitution in any execution-relevant location;
- executable brace groups and subshell groups;
- variables, ANSI-C escapes, pathname patterns, or brace expansion that can
  select the command;
- dynamic material that can supply a recursive flag or dangerous deletion
  target.

The parser must distinguish those forms from inert text:

- quoted or escaped substitution, process-substitution, redirect, glob, and
  grouping lookalikes;
- literal unmatched or non-expanding braces such as `foo{bar}`;
- arithmetic commands and arithmetic expansion without nested command
  substitution;
- bracket commands and literal bracket text such as `[;]`;
- quoted I/O-number and redirect lookalikes.

Arithmetic syntax remains `NoDeterministicDecision` unless its structured word
parts contain command substitution. The policy does not scan arithmetic
operators as shell list separators.

Assignments are tracked across the parsed program only to preserve the
existing exact root/empty expansion checks. The evaluator does not attempt
general variable execution. Dynamic command position or recursive flags remain
fail-closed even when a preceding literal assignment suggests a value.

## Error Handling and Resource Behavior

These conditions produce `Indeterminate`:

- parser syntax errors or incomplete input;
- word-parser or brace/pattern-parser errors;
- unsupported AST or word-part variants;
- analysis node-budget exhaustion.

Brush 0.4.0 does not join Bash backslash-newline continuations between `$` and
the opening `(` or `((` of command/arithmetic substitution. The adapter does
not pre-normalize or retry those forms: doing so without full quote and heredoc
provenance would recreate the tokenizer boundary this migration removes.
Exact continued forms therefore produce `Indeterminate`; equivalent normalized
command substitutions still deny, and normalized arithmetic remains inert.

The adapter traverses its projected tree iteratively where practical and uses a
fixed analysis budget of at most 131,072 visited AST/word-part nodes for the
existing 64 KiB hook input limit. Exceeding the budget returns
`Indeterminate`. This is an adapter traversal/allocation bound applied after
Brush parsing; it does not claim to bound parser CPU time or parser recursion.

The adapter does not implement a fallback tokenizer. It does not catch a parser
panic and continue with inference. Claude's documented hook contract treats
ordinary nonzero hook exits as non-blocking, so an in-process parser abort is
not a safe boundary. Parsing and deterministic safety evaluation therefore run
in an isolated invocation of the current executable for every provider. A
crash, timeout, nonzero helper exit, oversized output, or malformed helper
response becomes `Indeterminate` in the surviving permission-hook process. The
helper receives the command over stdin, never argv, and has bounded runtime and
output.

Maximum-size and deeply nested regression tests must record parser runtime and
memory behavior before release. The post-parse node cap is not a substitute for
those measurements.

Audit-store failures continue to preserve deterministic safety decisions.
Parser diagnostics remain bounded and command-free in terminal and persistent
activity records.

## Migration

1. Add all outstanding reopened cases to the existing regression corpus before
   changing production behavior.
2. Add adapter contract tests for parsing, structure, provenance, unsupported
   syntax, spans, and resource limits.
3. Verify each provider's Bash dialect contract and add it to normalized
   permission input.
4. Add the root-only `brush-parser` dependency, private adapter, and narrow
   initial-`]` glob compatibility classifier.
5. Add the isolated current-executable helper protocol with bounded stdin,
   stdout, and runtime.
6. Introduce `SafetyEvaluation` and provider-level indeterminate/no-inference
   tests.
7. Move the existing policy helpers and rule IDs onto project-owned parsed
   input.
8. Delete the hand-written tokenizer, expansion scanners, redirection stripping,
   and their obsolete helper tests after the complete corpus passes.
9. Update the changelog to describe a parser-backed deterministic boundary
   without claiming nested-shell or runtime-expansion enforcement.

There is no production feature flag or legacy fallback. The migration lands
only when parser-backed behavior passes the complete gate.

## Required Regression Corpus

Preserve every existing `safety.rs` and provider-level safety test, then add at
least:

### Must deny

- `/bin/r[]m] --no-preserve-root -rf /`
- `/bin/r[\m] --no-preserve-root -rf /`
- `rm>/dev/null -rf /`
- `>'>' rm --no-preserve-root -rf /`
- `/bin/r[m] --no-preserve-root -rf /`
- `rm --no-preserve-root -rf /{,}`
- variable-expanded recursive flags
- ANSI-C root targets and GNU `env -S` options
- process substitutions and executable groups containing destructive deletion

### Must remain without a deterministic decision

- `"2">/dev/null rm -rf /`
- `printf %s foo{bar}`
- `((0 || rm -rf / 1))`
- `[;]`
- existing quoted and escaped execution-syntax lookalikes
- arithmetic expansion without command substitution
- ordinary commands and non-dangerous recursive deletion targets

### Must preserve provider-native confirmation without inference

- malformed Bash input;
- incomplete quotes, substitutions, redirects, and groups;
- command or arithmetic substitutions whose `$` and opening delimiter are
  joined across a backslash-newline, which Brush 0.4.0 cannot parse safely;
- unsupported Brush AST/word forms exercised through a test adapter seam;
- unknown or unsupported provider shell dialect;
- analysis-budget exhaustion.

## Verification Gates

Before completion:

- focused adapter and deterministic safety tests pass in root library and binary
  targets;
- provider-matrix and audit-failure integration tests pass;
- the fake inference callback is not called for deterministic deny or
  `Indeterminate`;
- `cargo test` passes for the workspace;
- `cargo clippy --all-targets -- -D warnings` passes;
- `cargo fmt --all --check` passes;
- `cargo build` passes;
- normalized `git diff --check` passes;
- 64 KiB and deeply nested adversarial cases complete without panic or
  disproportionate runtime;
- locally available Nix release targets pass;
- every configured Darwin and static-musl release target passes in CI before
  publication;
- direct and transitive dependency-count delta is recorded;
- release-binary-size delta is recorded.

If Brush cannot meet any security, MSRV, or release-target gate, the migration
does not ship. Failure returns the work to design; it does not authorize a
partial hand-written parser.

## Documentation

Update `CHANGELOG.md` under `[Unreleased]` with the parser-backed deterministic
safety boundary and the remaining native-confirmation behavior for unsupported
syntax. Do not claim protection for nested shell strings, live filesystem
expansion, or arbitrary runtime command construction.

No public configuration or CLI documentation changes are required.

## Stress Test Results: Parser-Backed MSOS Safety

### Resolved Decisions

- Use the Cargo-compatible `brush-parser = "0.4.0"` requirement and committed
  lockfile rather than an exact dependency pin; lockfile upgrades require the
  complete safety and release gates.
- Treat source spans as optional diagnostics. Typed syntax and word
  classification, not span availability, determine whether analysis succeeds.
- Keep broad fail-closed treatment for active nested execution and dynamic
  command selection; do not narrow it to visibly destructive outer commands.
- Do not assume a parser crash preserves provider confirmation. Verify each
  provider's hook-failure contract and isolate parsing when fail-safe behavior
  cannot be established.
- Treat the 131,072-node cap as a post-parse adapter bound and separately
  measure maximum-size and deep-nesting parser behavior.
- Keep production free of a legacy evaluator or safety-disabling feature flag.
  A temporary test-only differential evaluator must be removed before
  completion.
- Retain Brush as the selected parser while isolating it behind project-owned
  types so a future parser switch remains local.
- Use a positive literal model: unknown word syntax is `Indeterminate`, never a
  safe literal.
- Require exact no-inference assertions for deterministic denies and
  indeterminate outcomes; require all configured release targets in CI before
  publication.
- Carry provider shell-dialect authority through normalization and refuse to
  treat Bash parsing as authoritative for an unknown dialect.
- Preserve deterministic handling of Bash's initial-`]` bracket pattern with a
  narrow, word-level compatibility classifier because Brush 0.4.0's public
  glob helper returns false for that valid form.

### Changes Made

- Clarified dependency version and lockfile policy.
- Made source spans optional and strengthened unknown-word handling.
- Added shell-dialect authority requirements.
- Replaced the unsupported parser-crash fallback assertion with a verified
  helper-isolation requirement after the Claude hook contract established that
  ordinary nonzero exits are non-blocking.
- Clarified resource-limit scope and release-target gates.
- Added the approved initial-`]` bracket-pattern compatibility shim.

### Deferred / Parking Lot

- Nested `sh -c`, `bash -c`, and `eval` enforcement remains in
  `codexctl-dchq`.
- A future parser replacement is allowed through the private adapter but is not
  part of this migration.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** Brush word-classification fidelity, provider
  shell-dialect contracts, the bounded helper protocol, and worst-case parser
  resource behavior must be resolved with implementation evidence.

## References Added During Planning

- [Claude Code hooks](https://code.claude.com/docs/en/hooks) — ordinary nonzero
  hook exits are non-blocking; exit 2 is the blocking error path.
- [Antigravity hooks](https://www.antigravity.google/docs/hooks) —
  `run_command` is documented as proposing a Bash command.
