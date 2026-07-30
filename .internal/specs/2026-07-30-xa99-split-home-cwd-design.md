# XA99 Split HOME Alias Safety Design

## Context

The merged `codexctl-xa99` change lets an unquoted, field-splittable variable
that resolves to trusted `HOME` continue to field classification. That is safe
only when every resulting relative target is known to resolve outside trusted
`HOME` and its parents.

`ShellCommandInput` carries the shell dialect and source, but not a trusted
working directory. Provider permission-hook tests currently launch from the
temporary `HOME`, which hides the unsafe case: from `/`, splitting
`/home/alexander` on `/` produces the relative target `home`, which resolves to
the trusted HOME parent `/home`.

## Decision

Restore fail-closed behavior when a recursively deleted dynamic word resolves
exactly to trusted `HOME`, even if that word can field-split. Without trusted
cwd, the evaluator must not infer that the resulting relative fields are safe.

Do not extend provider payloads, `ShellCommandInput`, or the isolated-helper
protocol with cwd in this fix. That would be a wider trust-boundary change and
is unnecessary to close the confirmed bypass.

## Code Changes

- In `src/brain/safety.rs`, remove the field-splitting exception from the
  resolved-HOME denial.
- Keep the existing exact `$HOME`, quoted alias, recursive-flag, root-target,
  and unresolved-expansion behavior unchanged.
- In `tests/hook_activity.rs`, allow the provider-hook test helper to run from
  an explicit cwd and construct permission payloads with an explicit command
  cwd. Replace the unsafe allow/model-inference expectation with an all-provider
  regression whose payload cwd and hook subprocess cwd are both `/`; keep
  temporary `HOME` only for isolated config, state, and model fixtures.
- Add a shell-internal `cd /` regression executed by the hook from temporary
  `HOME`, proving that initial hook cwd alone cannot make split targets safe.
- Retain quoted/exact HOME controls and assert that every unsafe case denies
  before model inference.

## Security and Error Handling

The evaluator remains fail closed at the missing-context boundary. A benign
command whose split fields happen to be harmless from its actual cwd can be
denied, but no command is allowed based on cwd the evaluator did not receive
and validate.

## Verification

Use RED-GREEN TDD:

1. Add the all-provider regression with both provider-reported command cwd and
   hook subprocess cwd set to `/`, then confirm it fails because the current
   implementation permits model inference.
2. Restore the resolved-HOME denial and confirm the regression passes.
3. Run focused safety unit and provider tests.
4. Run the full test suite, formatting, Clippy with warnings denied, build, and
   normalized diff checks.

## Scope

Only `src/brain/safety.rs`, `tests/hook_activity.rs`, and the required internal
design/plan artifacts are in scope. No config, provider schema, helper protocol,
documentation, version, commit, or publication change is included.

## Stress Test Results: XA99 Split HOME Alias Safety

### Resolved Decisions

- Keep the fix local to the evaluator; initial cwd is insufficient because the
  shell source can change directories before deletion.
- Remove the benign-cwd allow expectation because it relies on context the
  evaluator neither receives nor models.
- Preserve provider parsers, `ShellCommandInput`, and the isolated-helper
  protocol.
- Test the reopened cwd=`/` case for all providers and add a shell-internal
  `cd /` control. Set both provider-reported command cwd and hook subprocess cwd
  to `/` so the test represents the execution context rather than only helper
  process state.
- Preserve bounded in-memory evaluation and existing indeterminate fail-safe
  behavior.
- Reject unconditional denial of every splittable target as unnecessarily
  broad; restore only the resolved-HOME guard.
- Keep `irreversible-home-delete` as the canonical rule for this protected-value
  violation.
- Require real permission-hook binary coverage with zero model requests.
- Do not add an automatic runtime fallback if conservative denial causes false
  positives.

### Changes Made

- Added the shell-internal `cd /` regression requirement.
- Required the cwd=`/` provider regression to set both payload cwd and hook
  subprocess cwd, while retaining temporary HOME for isolated fixtures.

### Deferred / Parking Lot

- Complete cwd-aware shell-state modeling, including directory-changing
  commands and compound execution contexts, is separate future work.

### Confidence Assessment

- Overall: High
- Areas of concern: conservative false denial remains intentional until cwd and
  directory changes can be modeled soundly.

## Reopened Descendant-Path Amendment

### Context

The exact-HOME guard merged in `d9775909` still allows a recursively deleted,
field-splittable alias that resolves below trusted `HOME`. For example, with
`HOME=/home/alexander`, `IFS=/; X=/home/alexander/safe; rm -rf $X` produces
relative fields. From `/`, `home` resolves to the trusted HOME parent `/home`;
from `/home`, `alexander` resolves to trusted HOME itself. The evaluator has no
trusted cwd or shell directory-state model, so it cannot prove either execution
safe.

### Decision

Keep exact-HOME denial unchanged. Additionally, when a dynamic target can split
fields, classify the executed fields against trusted `HOME` and its lexical
ancestors before model inference.

An absolute field is dangerous when it is trusted `HOME` or a lexical ancestor
of trusted `HOME`. A relative field is dangerous when its normalized component
sequence can complete a lexical prefix of trusted `HOME` to trusted `HOME` or
one of its ancestors. For `HOME=/home/alexander`, this includes `home`,
`alexander`, and `home/alexander`. Cwd-dependent parent traversal that cannot be
proven safe retains the existing unresolved-expansion denial.

Do not deny a quoted alias merely because it resolves to a HOME descendant:
without field splitting, recursively deleting that descendant does not imply
deleting HOME or its parent. Preserve unrelated splittable-path controls such
as `/tmp/safe`.

Do not add cwd to provider schemas, `ShellCommandInput`, or the isolated-helper
protocol. Full cwd and shell-state modeling remains outside this fix.

### Code and Tests

- Add one bounded lexical helper or equivalent comparison that classifies
  already-resolved fields without filesystem access or hypothetical cwd
  enumeration.
- Apply the executed-field reachability check only to field-splittable targets;
  retain the existing exact-HOME guard for all resolved targets and the
  existing fail-closed result for unresolved fields.
- Extend the safety unit regression with assignment and append-assignment HOME
  descendants, a non-descendant value whose split field can target a HOME
  ancestor, plus quoted-descendant and non-HOME controls.
- Extend the real Codex, Claude, and Antigravity permission-hook regression with
  exact-HOME, descendant, append-built descendant, and non-descendant
  ancestor-reaching aliases executed from `/` and `/home`.
- Assert every unsafe case denies before model inference and records
  `irreversible-home-delete`.

### Verification

Use RED-GREEN TDD. First add the executed-field unit and provider regressions
and confirm the current implementation reaches no deterministic decision and
the approving model. Then add only the bounded lexical field classification,
rerun the focused tests, the prior shell-safety corpus, and the full serial
formatting, test, Clippy-with-warnings-denied, and build gates.

### Scope

Production and regression changes remain limited to `src/brain/safety.rs` and
`tests/hook_activity.rs`. This amendment does not change configuration,
provider payloads, helper protocols, public documentation, versions, commits,
or publication.

## Stress Test Results: XA99 Executed Split-Field Reachability

### Resolved Decisions

- Use lexical normalization without filesystem canonicalization or symlink
  resolution.
- Classify executed split fields rather than only the unsplit resolved word;
  this covers HOME descendants and non-descendant values that yield a HOME
  ancestor field.
- Gate the new reachability analysis on field splitting. Preserve quoted alias
  behavior and every existing non-splitting classification.
- Reuse the existing assignment map and field resolver for direct and append
  assignments.
- Preserve fail-closed unresolved-field behavior and do not fall back to model
  inference.
- Keep cwd and provider/helper protocols unchanged; initial cwd alone cannot
  safely model shell directory changes.
- Require exact, descendant, append-built, ancestor-reaching, quoted, and safe
  non-HOME cases at the unit and all-provider boundaries.
- Treat absolute HOME/ancestor fields, relative HOME-prefix completions, and
  unprovable parent traversal as unsafe.
- Keep analysis bounded to existing fields and lexical HOME components.

### Changes Made

- Replaced the descendant-only full-word guard with an executed-field
  reachability invariant.
- Added the non-descendant shared-ancestor bypass to the required regression
  matrix.
- Made the resource-bound and unresolved-field behavior explicit.

### Deferred / Parking Lot

- Complete cwd-aware shell-state modeling remains separate work.
- Filesystem-dependent path and symlink interpretation remains outside the
  isolated lexical evaluator.

### Confidence Assessment

- Overall: High
- Areas of concern: conservative false denial remains intentional for relative
  parent traversal because the evaluator has no trusted cwd.

## Final-Review Pathname Normalization Amendment

### Context

The first executed-field implementation normalized candidate path components
but compared active pathname patterns using their original spelling. From
trusted HOME's parent, `./alex*` can expand to `./alexander`, yet comparing the
former with normalized candidate `alexander` misses the HOME target. Repeated
separators have the same mismatch. The whole-path conservative envelope also
treated a suffix beginning with `/` as automatically compatible, falsely
denying a multi-component control such as `home*/safe`.

The isolated boundary also accepted empty or relative UTF-8 HOME values even
though every HOME ancestry predicate requires an absolute path.

### Decision

- Normalize pathname patterns and candidate paths into lexical components
  before comparison. Compare ordinary pattern components only with their
  corresponding candidate components.
- Because trusted shell-option state is unavailable, conservatively account for
  `globstar` by allowing an active `**` component to consume zero or more
  candidate components, and account for `nocaseglob` by accepting either
  case-sensitive or case-folded component compatibility.
- Test absolute patterns against every non-root HOME prefix and relative
  patterns against every nonempty contiguous suffix of those prefixes. Do not
  use the textual pattern component count to exclude a candidate before
  `globstar` matching.
- Preserve fail-closed parent-traversal handling and avoid filesystem access,
  canonicalization, or cwd inference.
- Require the trusted HOME context to be nonempty, absolute, bounded, and UTF-8
  before constructing the isolated helper.
- Add RED-GREEN unit coverage for `./` and repeated-separator patterns, relative
  HOME validation, and the shared-prefix/suffix-mismatch control. Exercise the
  normalized relative pattern and safe suffix control through Codex, Claude,
  and Antigravity with model-request assertions. Cover option-modified
  `globstar` and `nocaseglob` HOME patterns, including a `globstar` match of
  HOME's ancestor, at the same boundary.

No provider payload, helper protocol, configuration, public documentation, or
version change is included.
