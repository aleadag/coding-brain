# Deny Recursive Deletion of Trusted HOME Ancestors

## Context

The shell safety evaluator currently denies recursive deletion of `/`, exact
trusted `HOME`, and split expansion targets that may reach trusted `HOME` or one
of its ancestors. Literal and resolved non-splitting targets only receive the
root and exact-HOME checks.

With trusted `HOME=/home/alexander`, both `rm -rf /home` and
`X=/home; rm -rf "$X"` therefore return `no_deterministic_decision`, even
though either command recursively deletes trusted `HOME`.

## Decision

Add one lexical classifier for absolute targets that identifies exact trusted
`HOME` or any non-root ancestor of trusted `HOME`. Use it for:

- literal recursive-delete targets;
- resolved non-splitting recursive-delete targets.

Keep `/` under the existing `irreversible-root-delete` rule. Classify exact
trusted `HOME` and its non-root ancestors under
`irreversible-home-delete`.

Do not broaden this change to relative-path, working-directory, glob, or field
splitting behavior. Existing logic continues to own those cases.

## Data Flow

For each target of a recursive `rm` command:

1. Preserve option parsing and the existing root check.
2. Lexically normalize an absolute target and trusted `HOME`.
3. Deny when the normalized target is non-empty and is a component-prefix of
   normalized trusted `HOME`.
4. Continue through existing split and dynamic-expansion checks otherwise.

This component-prefix comparison prevents string-prefix mistakes such as
treating `/hom` as an ancestor of `/home/alexander`.

## Security and Error Handling

The isolated evaluator already validates trusted `HOME` as non-empty, absolute,
bounded, and UTF-8 before invoking the helper. The new classifier retains the
current fail-closed helper behavior for invalid trusted context.

Lexical normalization preserves the evaluator's existing treatment of `.`,
`..`, and repeated separators. No filesystem lookup or symlink resolution is
introduced.

## Verification

Unit regressions must prove:

- a literal non-root trusted-HOME ancestor is denied;
- a quoted alias resolving to that ancestor is denied;
- exact trusted `HOME` remains denied;
- a quoted descendant remains outside this rule;
- an unrelated absolute path remains outside this rule.

Provider-boundary regressions must exercise Codex, Claude, and Antigravity and
prove ancestor cases are denied before model inference, with zero model
requests. Existing descendant and unrelated-path controls must remain allowed
to reach model inference.

Run targeted unit and integration tests, then the repository formatting,
Clippy, and full test gates.
