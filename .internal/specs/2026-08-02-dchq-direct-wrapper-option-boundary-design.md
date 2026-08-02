# Direct Wrapper Option Boundary Design

> **Date:** 2026-08-02
> **Issue:** `codexctl-dchq`
> **Design session:** `codexctl-4rgw`
> **Status:** Approved
> **Extends:** `2026-08-02-dchq-multicall-applet-boundary-design.md`

## Context

The nested-shell safety evaluator statically unwraps recognized `time` and
`env` commands so it can evaluate the command they execute. Runtime utility
identity is intentionally unavailable: the same lexical command can resolve to
GNU, BSD, or another implementation. The current direct option classifiers
advance past forms that do not execute the remaining arguments on at least one
supported implementation:

- GNU `time -h` is invalid and exits before its child, while BSD `time -h`
  formats output and executes its child.
- GNU `env --help` and `env --version` terminate before a child.
- GNU `env -0` is incompatible with a command and exits before it.
- After the first `NAME=VALUE` operand, GNU `env` treats later option-looking
  words as the command rather than as options.

The evaluator currently continues through those boundaries and can hard-deny a
destructive shell payload that the real wrapper would not execute. This is not
a fail-open bypass, but it violates the approved boundary: uncertain or
non-executing wrapper forms require native confirmation rather than a
provider-level deterministic denial.

## Decision

Use an exact, closed, cross-platform grammar for direct external `time` and
`env` wrapper normalization. An option may advance the inferred command
position only when its execution semantics agree across the audited supported
utility implementations. A differing, implementation-specific, abbreviated,
or undocumented form is `Indeterminate`.

For direct external `time`:

- Continue to unwrap only the audited common command-carrying forms: `-p`,
  `-a`, and `-o` with a separate or attached output path. No long option is in
  the common set.
- Treat `-h`, implementation-specific options, long-option abbreviations,
  unknown options, and missing option values as `Indeterminate` before model
  inference.
- Classify a short-option token atomically. A terminating or incompatible flag
  anywhere in a true cluster makes the token indeterminate, while characters
  following a value-taking option remain that option's attached value.

For direct external `env`:

- Continue to unwrap only exact command-carrying options whose semantics agree
  across the audited supported implementations: `-`, `-i`, `-u` with a
  separate or attached name, `-v`, and the option terminator `--`. No long
  option is in the common set.
- Treat terminating `--help` and `--version`, and command-incompatible `-0` or
  `--null`, as `Indeterminate` when remaining words could otherwise be
  mistaken for a child command.
- Do not accept unique long-option abbreviations. An exact long option is
  normalized only when it belongs to the audited common grammar.
- End option parsing immediately after consuming the first literal
  `NAME=VALUE` operand. Later assignment operands remain assignments, but
  option parsing never resumes. A following option-looking word, including
  `--`, is the command position rather than another `env` option; the evaluator
  must not project a later nested-shell argument through it. Such an execution
  boundary remains `Indeterminate` and receives native confirmation.

For every accepted option that takes a separate value, preserve runtime argv
cardinality before consuming that value. `ShellWord` records whether expansion
may produce anything other than exactly one argv. This includes unquoted field
splitting, brace and pathname expansion, quoted `$@`, non-concatenating array
all-elements expansion, variable/member name lists, and transformations that
emit separate words. Those forms fail closed under the existing
`unsafe-recursive-delete-expansion` rule for both direct and BusyBox wrapper
paths. Quoted scalar parameters, positional scalars, `$*`, and concatenating
array forms remain one argv and continue to be consumed as the option value.

The wrapper scanner returns a typed unsafe-expansion failure for this boundary.
Nested `builtin command` and `builtin exec` dispatch preserve that deterministic
denial instead of converting it into generic unsupported syntax.

The existing BusyBox/Toybox-specific classifiers remain unchanged. The
existing precedence remains `Deny > Indeterminate > NoDeterministicDecision`.
Therefore a separate proven destructive command in the same shell program
still denies even when one wrapper form is indeterminate.

The portability boundary is grounded in the GNU `env` manual and current BSD
`env`/`time` manuals. In particular, GNU documents that `env` options precede
operands and the first non-assignment operand is the command; current FreeBSD
documents `-0` only in the no-utility synopsis; and BSD `time -h` executes a
child while GNU `time -h` rejects the option. The implementation does not
encode a detected utility version from these references:

- <https://www.gnu.org/s/coreutils/manual/html_node/env-invocation.html>
- <https://man.freebsd.org/cgi/man.cgi?manpath=FreeBSD+15.1-RELEASE&query=env&sektion=1>
- <https://man.freebsd.org/cgi/man.cgi?manpath=FreeBSD+6.2-RELEASE&query=time&sektion=1>

## Data Flow

1. Brush parses the outer shell program into shell words.
2. `unwrap_command_with_context` identifies an exact direct `time` or `env`
   wrapper.
3. The wrapper-specific classifier consumes only options proven to preserve a
   child command position.
4. A terminating, incompatible, ambiguous, or structurally uncertain form
   marks the evaluation indeterminate and stops projection through that
   wrapper.
5. Permission hooks convert `Indeterminate` into native provider confirmation
   without invoking the Coding Brain model.
6. Exact command-carrying forms continue into nested-shell analysis; proven
   destructive payloads retain their existing deterministic deny rule.

## Security Properties

- No terminating or incompatible wrapper form is treated as proof that its
  remaining words execute.
- No runtime path, operating-system target, or command spelling is treated as
  proof of GNU or BSD utility identity.
- No unknown form falls through to model inference.
- No accepted separate option value can expand into additional argv fields and
  inject an unanalyzed child command.
- Exact destructive payloads behind supported command-carrying forms still
  deny for Codex, Claude Code, and Antigravity.
- Indeterminate and deny paths make zero model requests for every provider.
- No runtime binary probing, `PATH` inference, filesystem lookup, or provider-
  specific parsing is added to production evaluation.
- Existing parser budgets and destructive-command rule identifiers are
  unchanged.

## Verification

Tests are written red before production changes and cover:

- evaluator `Indeterminate` results for direct `time -h`, `env --help`,
  `env --version`, `env -0 COMMAND`, and `env FOO=bar -i COMMAND` forms carrying
  a destructive nested shell payload;
- atomic option-token coverage for both orderings of incompatible clusters,
  exact versus abbreviated long options, and attached versus separate values;
- `env` state-transition coverage for multiple assignments, an option-looking
  command, and `--` after the first assignment;
- shipped-helper `indeterminate` results for the same corpus;
- Codex, Claude Code, and Antigravity native-confirmation behavior with zero
  model requests;
- preserved deterministic denial for audited controls such as `time -p` and
  `env -i` carrying the same destructive payload;
- a bare Bash reserved-word `time -h` control that remains non-denied and is
  not reclassified as an external wrapper;
- deny-over-indeterminate precedence in both statement orders when a separate
  proven delete exists;
- dynamic options, dynamic command positions, missing values, malformed
  clusters, and resource-limit paths remaining indeterminate;
- separate `time -o` / `env -u` and BusyBox `time -f` values that may produce
  zero or multiple argv failing closed across direct, BusyBox, and nested
  builtin dispatch, while proven exact-one values remain supported;
- representative literal short `env -S` denial at evaluator, shipped-helper,
  and every provider boundary;
- harmless real-binary differential probes where the installed GNU utilities
  are available, without making the automated suite depend on host binaries.

Focused evaluator, helper, and provider tests run first. Final verification is
serial `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --all --check`, and `cargo build` through `nix develop path:.`.

## Scope

Production changes are limited to wrapper normalization in
`src/brain/safety.rs` and argv-cardinality projection in
`src/brain/safety/shell.rs`. The shared separate-value guard intentionally
applies to direct and BusyBox wrapper paths without changing their classifiers.
Regression coverage may also change
`src/brain/permission_hook.rs`, `tests/shell_safety_helper_cli.rs`, and the
user-authorized integration corpus in `tests/hook_activity.rs`.

This fix does not change provider payloads, public APIs, configuration, model
policy, rule identifiers, release versions, or multicall applet grammar. It
does not authorize a commit, push, pull request, merge, or publication.

## Stress Test Results: Direct Wrapper Option Boundaries

### Resolved Decisions

- Treat wrapper identity as unknown and normalize only a closed semantic
  intersection established for supported GNU and BSD utility families.
- Keep Bash reserved-word `time` behavior outside this direct external-wrapper
  fix; retain a non-denial control for `time -h`.
- Classify clustered, abbreviated, and value-taking options atomically.
- Model `env` parsing as a one-way `options -> assignments -> command` state
  transition; never resume option parsing after the first assignment.
- Preserve order-independent `Deny > Indeterminate > NoDeterministicDecision`
  precedence.
- Reuse existing dynamic-input, malformed-input, and resource-limit handling;
  add no fallback parser or expansion logic.
- Keep production policy lexical and closed; real utility probes are
  verification evidence, not runtime policy inputs.
- Route every uncertain transition to native confirmation with zero Coding
  Brain model requests for Codex, Claude Code, and Antigravity.
- Implement the change within the existing wrapper loop so every iteration
  consumes input or returns and existing parser budgets remain unchanged.

### Changes Made

- Replaced the GNU-identity assumption with an audited cross-platform semantic
  intersection.
- Required exact long-option names and atomic short-option classification.
- Made the `env` assignment-state transition and option-looking command result
  explicit.
- Expanded verification for reserved-word controls, clusters, assignment
  ordering, precedence ordering, dynamic input, and provider-wide inference.

### Deferred / Parking Lot

- Provider-native permission enforcement remains the preferred long-term
  primary boundary, but replacing deterministic defense in depth is outside
  this fix.
- Adding new wrapper options remains an explicit classifier-and-regression
  change rather than automatic version detection.

### Confidence Assessment

- Overall: High
- Areas of concern: The accepted option set must remain a deliberately small
  documented intersection; compatibility friction for implementation-specific
  forms is the intended cost of avoiding unjustified hard denies.
