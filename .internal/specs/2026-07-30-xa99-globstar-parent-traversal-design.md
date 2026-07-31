# XA99 Globstar Parent Traversal Design Amendment

## Context

The existing xa99 executed-field reachability fix conservatively treats an
active `**` pathname component as matching zero or more path components.
However, `lexical_pattern_parts` currently applies lexical parent traversal
before pathname-pattern matching. For:

```bash
shopt -s globstar
IFS=:
X='/home/alexander/**/../alexander'
rm -rf $X
```

it pops `**` when it sees `..`. Bash instead expands the pathname pattern first,
producing `/home/alexander/../alexander`, which lexically resolves to trusted
HOME. Removing `**` before matching therefore hides a reachable destructive
target and allows the command to reach model inference.

This amendment remains inside the approved xa99 safety boundary. The evaluator
still has no trusted filesystem snapshot or shell-option state, so active
pathname patterns must be analyzed conservatively without filesystem access.

## Decision

Preserve parent components in pathname-pattern parts until reachability
matching. Use a bounded, memoized state matcher so it evaluates `..` after
accounting for the components that an earlier `**` may consume. For `**/..`,
the matcher must cover both semantic classes: zero consumed components makes
`..` remove the preceding fixed component, while one or more consumed
components makes `..` remove one consumed component and leaves `**` able to
consume more. A match exists when at least one permitted expansion followed by
lexical parent traversal can produce the absolute root, the candidate HOME
path, or one of its non-root ancestors. Query the empty absolute candidate
before the HOME candidates so root reachability cannot fall through to lexical
normalization of the unresolved pattern.

Keep ordinary lexical path normalization unchanged for non-pattern paths.
Keep ordinary component patterns bounded to their corresponding path component;
only an exact active `**` component may consume zero or more components.
Continue considering case-folded component compatibility because trusted
`nocaseglob` state is unavailable.

Represent reachability as `Reachable`, `Unreachable`, or `Unknown`.
`ExpansionThenTraversal` reachability produces the root- or HOME-deletion
denial for the candidate being tested. `DirectExpansion` remains conservative:
for the empty root candidate it fails closed through the recursive-expansion
denial because the direct multiple-globstar matcher does not prove mandatory
middle components absent. Malformed patterns, non-UTF-8 components, traversal
above root, or resource exhaustion also fail closed through that rule. Only
proven `Unreachable` patterns may continue to model inference.

Use an explicit worklist rather than recursion proportional to input length.
Share a limit of 16,384 unique matcher states across the entire safety
evaluation, including every recursive-delete target and resolved field.
Exhausting that aggregate budget produces `Unknown`.

## Scope

Modify only the xa99 pattern reachability helper in `src/brain/safety.rs` and
its existing unit and provider regression matrices. Do not change shell
parsing, assignment resolution, provider payloads, helper protocols,
configuration, public documentation, or versions.

No new abstraction is required outside the existing component matcher.

## Testing

Add unit regressions for active absolute globstars followed by parent traversal
that can normalize to exact HOME and to `/`. Add the same unsafe commands to
the Codex, Claude, and Antigravity provider matrix and assert deterministic
denial with zero model requests.

Retain and explicitly exercise:

- zero-consumption and one-or-more-consumption `**/..` paths;
- chained parent traversal, multiple globstars, and an ordinary wildcard
  component before parent traversal;
- consecutive helper calls sharing one deliberately small budget;
- production aggregate matcher-budget exhaustion through individually
  unrelated fields, which fails closed without recursion or model inference;
- a globstar-parent-traversal value used through a quoted `"$X"` target
  expansion, which does not undergo pathname expansion and remains
  model-backed;
- an unrelated active globstar-parent-traversal pattern that cannot reach HOME
  or its ancestors under either consumption class, which remains model-backed;
- the existing globstar, normalized-pattern, descendant, ancestor, shared
  prefix, and parent-traversal safety corpus.

Run the focused unit and provider tests first, then the prior shell-safety
corpus, full serial tests, formatting, Clippy with warnings denied, build, and
normalized diff checks.

## Security Properties

- Recursive deletion that may expand and normalize to `/`, HOME, or a non-root
  HOME ancestor is denied before model inference.
- Pattern reachability is conservative when shell-option or filesystem state is
  unavailable.
- Components between multiple globstars retain the existing conservative
  treatment; the matcher rewrite must not make that case permissive.
- Quoting remains authoritative: inactive pathname syntax is not treated as an
  active glob.
- Ordinary unrelated patterns are not denied merely because they contain
  `**` and `..`.

## Consequences

The matcher becomes slightly more complex because parent traversal must be
evaluated in the same state space as globstar consumption. The change avoids a
broader rule that would deny every unquoted pathname pattern containing `..`,
preserving the existing unrelated controls while closing the confirmed bypass.

## Stress Test Results: Globstar Parent Traversal

### Resolved Decisions

- Use a bounded, memoized state matcher that models both zero and nonzero
  globstar consumption before parent traversal.
- Propagate tri-state reachability so malformed or unbounded analysis fails
  closed rather than being mistaken for a safe mismatch.
- Normalize dot and repeated separators while preserving every parent component
  for state matching; ordinary patterns never cross component boundaries.
- Quote the destructive target expansion, not merely the assignment, in the
  inactive-pattern control.
- Share a 16,384-state work budget across the complete safety evaluation and
  use an explicit worklist to avoid stack exhaustion.
- Keep the implementation provider-neutral, lexical, and independent of cwd,
  filesystem state, and model output.

### Changes Made

- Added tri-state failure semantics and an aggregate matcher work budget.
- Expanded traversal, exhaustion, provider, and false-denial regression
  requirements.
- Corrected the quoted control and required preservation of existing
  multi-globstar conservatism.

### Deferred / Parking Lot

- None. Filesystem-backed glob resolution and a blanket parent-pattern denial
  were rejected as respectively race-prone and unnecessarily restrictive.

### Confidence Assessment

- Overall: High
- Areas of concern: implementation must keep the state representation bounded
  and must propagate `Unknown` through every field and target without converting
  it to `Unreachable`.
