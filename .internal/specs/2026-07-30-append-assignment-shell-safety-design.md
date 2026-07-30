# Preserve Append-Assignment Shell Safety

## Context

The parser-backed shell safety adapter currently projects Brush assignments
without retaining `ast::Assignment.append`. The policy therefore treats `X+=rf`
as replacement with `rf`, rather than appending to the current value.

This loses the destructive meaning of commands such as:

```bash
X=-; X+=rf; rm --no-preserve-root -f $X /
X=/home/; X+=alexander; rm -rf "$X"
```

Bash resolves those assignments to `-rf` and `/home/alexander`. The current
projection can instead return no deterministic decision and allow the command
to reach model inference.

## Decision

Preserve the parser's append flag in the internal `ShellAssignment`
projection. When policy applies a top-level assignment:

- a replacement assignment continues to replace the tracked value;
- an append assignment concatenates the previous and appended values only when
  both are known literals;
- an append assignment with an unknown previous or appended value invalidates
  the tracked assignment.

Known literal appends update the tracked `String` in place. This preserves
linear work across repeated appends within the existing input and analysis
resource bounds.

The existing context rules remain unchanged. Assignments in conditional,
loop, pipeline, asynchronous, group, subshell, and process-substitution
contexts keep their current conservative treatment.

## Safety Behavior

Known literal appends reproduce Bash string-assignment semantics closely enough
for deterministic safety analysis. Unknown append inputs remain fail closed:
the analyzer must not invent a value or treat the right-hand side as a
replacement.

After resolving a nonliteral target from tracked assignments, policy compares
the resolved path with the trusted `HOME` before applying generic dynamic-target
handling. A match is denied as irreversible home deletion. This is required
because the existing `$HOME`-specific check does not recognize another
variable, such as `X`, whose resolved value equals `HOME`.

Both reported commands must be denied before model inference for Codex, Claude,
and Antigravity. The flag-construction case should be rejected as unsafe
recursive-delete expansion; the trusted-home construction must be rejected as
irreversible home deletion when the trusted `HOME` is `/home/alexander`.

Array assignments remain unsupported. This change does not broaden supported
shell syntax or alter provider hook response contracts.

## Implementation Scope

Only the parser projection, assignment-state application, and directly related
tests should change:

- retain `append` in `src/brain/safety/shell.rs`;
- apply known-literal concatenation or conservative invalidation in
  `src/brain/safety.rs`;
- classify any resolved nonliteral target equal to trusted `HOME` as
  irreversible home deletion in `src/brain/safety.rs`;
- add structural projection and in-process policy regressions;
- add real-provider boundary regressions in `tests/hook_activity.rs`.

No parser replacement, policy refactor, configuration change, or compatibility
behavior is included.

## Verification

Use test-driven development:

1. Add regressions for both reported commands and observe the current
   no-decision behavior.
2. Add structural coverage proving the parser projection retains `append`.
3. Add policy coverage for repeated and empty literal appends.
4. Implement the minimal append-aware projection and state update.
5. Verify real-provider denials and zero fake-model requests for Codex, Claude,
   and Antigravity.
6. Run the pre-existing parser-backed adversarial corpus.
7. Run formatting, Clippy with warnings denied, and all-target workspace tests.

Success requires deterministic denial before any fake-model request for all
three providers and no regression in the existing shell-safety corpus.

## Stress Test Results: Append-Assignment Shell Safety

### Resolved Decisions

- Parser projection is the sole syntax boundary; policy does not reparse raw
  assignment text.
- Only known prior and appended literals are concatenated. Any unknown operand
  invalidates the tracked value.
- Existing execution-context behavior remains unchanged.
- Repeated and empty literal appends receive explicit policy coverage; array
  assignments remain unsupported.
- Known strings are extended in place to avoid quadratic repeated
  concatenation.
- Structural, in-process, and real-provider tests jointly prove the security
  boundary.
- Resolved nonliteral targets are compared with trusted `HOME`, regardless of
  the parameter name that produced the value.

### Changes Made

- Required in-place concatenation for repeated known-literal appends.
- Expanded verification to cover structural append projection, repeated and
  empty appends, all providers, expected safety rules, and zero model requests.
- Added the resolved-target trusted-home check required for the quoted `X`
  construction.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: none beyond the normal requirement to rerun the existing
  adversarial and all-target suites after implementation.
