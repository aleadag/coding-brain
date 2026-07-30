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
