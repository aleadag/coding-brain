# Close provider-specific destructive-command safety bypasses

> **Date:** 2026-07-28
> **Issue:** codexctl-msos
> **Status:** Approved

## Context

The deterministic safety rail currently evaluates a request only when its raw
tool name is exactly `Bash`. Codex and Claude use that name, but Antigravity
represents shell execution as `run_command`. Antigravity parsing already
extracts the command into `PermissionHookRequest.command`, yet the later safety
check consults the provider-specific tool name and can therefore allow a model
decision to bypass the destructive-command deny.

The shell tokenizer also records `$` as a variable expansion without
distinguishing command substitution. The variable-reference parser cannot
resolve `$(...)` or backtick substitution, so a recursive delete such as
`rm -rf "$(command)"` is not denied even though the target can resolve to the
filesystem root or home directory at execution time.

## Decision

Use `PermissionHookRequest.command: Option<String>` as the existing
provider-neutral shell-command capability. Provider parsers remain responsible
for setting it only for supported command-execution tools:

- Codex `Bash`;
- Claude `Bash`;
- Antigravity `run_command`.

The deterministic safety evaluator will inspect the extracted command whenever
that capability is present. It will no longer decide applicability from the raw
provider tool name. Raw tool names remain unchanged for audit records,
permission identity, model context, and provider responses.

Do not add a new public capability enum or teach the safety module a growing
list of provider aliases. The optional normalized command already expresses the
only capability this safety policy needs. Document beside
`PermissionHookRequest.command` that provider parsers must populate it only for
recognized shell-command tools; unknown tools must leave it unset and therefore
fall back to manual input.

Provider-native permission policy is the preferred long-term enforcement point.
Keep Coding Brain's deterministic deny as defense-in-depth until every supported
provider demonstrably enforces equivalent destructive-command rules.

## Expansion handling

Extend the private shell-word representation to distinguish command
substitution from ordinary parameter expansion. Recognize both shell forms:

- `$(...)`;
- backticks.

For `rm`, any active command substitution in its arguments is unresolved
runtime input and must deterministically deny with the existing
`unsafe-recursive-delete-expansion` rule. Check this before classifying flags or
targets because an unquoted substitution can synthesize both, including `-rf`
and `/`. This applies whether the substitution is a whole argument or is
embedded in a larger shell word.

Do not attempt to execute, evaluate, partially parse, or prove the output of a
command substitution safe. Ordinary parameter expansion keeps its existing
assignment and default-value analysis. Command substitution outside `rm`
arguments remains outside this rule.

## Data flow and failure behavior

1. The provider parser validates the permission payload and extracts a command
   only for its supported shell-execution tool.
2. The permission hook passes that normalized capability to deterministic
   safety before model inference.
3. A matched safety rule persists and returns the existing deterministic deny.
4. A supported command with no matched rule continues to model evaluation.
5. A non-command tool has no command capability and remains unsupported or
   governed by its existing provider policy.

Parsing ambiguity must fail closed only within the established boundary:
complex expansion in a recursive-delete target. This change must not convert
unrelated commands or non-command tools into deterministic denies.

## Regression tests

Add focused tests proving:

1. The same recursive root deletion is denied for Codex `Bash`, Claude `Bash`,
   and Antigravity `run_command` permission payloads.
2. Antigravity cannot reach model inference for a destructive command even
   when the model stub would return allow.
3. Unsupported Antigravity tools do not acquire shell-command capability.
4. `rm` arguments containing active `$(...)` are denied.
5. `rm` arguments containing active backticks are denied.
6. Substitution embedded in an argument is denied.
7. Substitution capable of synthesizing `-rf` and `/` is denied before flag
   classification.
8. Single-quoted and escaped substitution syntax remains inert.
9. Existing literal root, home, unresolved parameter, wrapper-command, and
   ordinary-command cases retain their behavior.

The provider-matrix and command-substitution regressions must fail before
production code changes.

## Scope

Change only the private permission-hook safety boundary, the private shell
tokenizer state needed to recognize command substitution, and focused tests.
No configuration, activity schema, provider response, audit identity, TUI,
documentation, or compatibility-path changes are required.

Do not recursively interpret `sh -c`, `bash -c`, `eval`, or equivalent nested
shell programs in this change. A lightweight recursive parser would be
unsound; provider-native policy or a dedicated shell-policy parser should cover
that limitation separately.

## Verification

Run:

1. focused safety and permission-hook tests;
2. `cargo fmt --check`;
3. `cargo test`;
4. `cargo clippy --all-targets -- -D warnings`;
5. `cargo build`.

## Stress Test Results: Provider-neutral destructive-command safety

### Resolved Decisions

- **Capability boundary:** Treat an extracted permission command as the
  provider-neutral shell capability; keep raw tool names for identity and
  audit only.
- **Substitution syntax:** Recognize `$()` and backticks only when active
  outside single quotes and escapes.
- **Provider evolution:** Unknown provider tools receive no command capability
  and fall back to manual input.
- **Enforcement ownership:** Prefer provider-native permission policy
  long-term, while retaining Coding Brain's deterministic deny as
  defense-in-depth until provider parity is proven.
- **Nested shells:** Do not add unsound recursive parsing of `sh -c`, `eval`,
  or equivalent wrappers to this focused fix.
- **Capability invariant:** Document and test that only recognized
  shell-command tools populate `PermissionHookRequest.command`.
- **Regression boundary:** Cover tokenizer behavior and full permission-hook
  behavior independently across all providers.
- **Dynamic flags:** Deny active command substitution anywhere in `rm`
  arguments before deciding which arguments are flags or targets.

### Changes Made

- Broadened command-substitution denial from recursive-delete targets to all
  dynamic `rm` arguments so substitutions cannot synthesize recursive flags.
- Added the provider-native enforcement direction and the condition for
  retaining centralized defense-in-depth.
- Made the command-capability invariant and inert-syntax regression cases
  explicit.
- Documented nested-shell parsing as a separate limitation rather than
  extending the lightweight tokenizer unsafely.

### Deferred / Parking Lot

- Move primary destructive-command enforcement to provider-native permission
  policy after equivalent behavior is verified for every supported provider.
- Address destructive commands hidden inside nested shell programs through
  provider-native enforcement or a dedicated shell-policy parser.

### Confidence Assessment

- **Overall:** High
- **Areas of concern:** The lightweight tokenizer intentionally does not
  interpret nested shell programs; centralized safety remains defense-in-depth,
  not a complete shell policy engine.
