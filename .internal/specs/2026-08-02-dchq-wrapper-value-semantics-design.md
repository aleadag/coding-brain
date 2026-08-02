# Wrapper Option Value Semantics Design

> **Date:** 2026-08-02
> **Issue:** `codexctl-dchq`
> **Design session:** `codexctl-sung`
> **Status:** Approved
> **Extends:** `2026-08-02-dchq-direct-wrapper-option-boundary-design.md`

## Context

The direct-wrapper safety scanner records whether an option takes a separate
value, but it does not retain what that value means. Once a word is known to
expand to exactly one argv, the scanner consumes it and projects the remaining
words as the wrapper's child command.

That is insufficient for values which are structurally one argv but
semantically invalid. At current `origin/main`, the shipped evaluator hard-
denies the trailing destructive payload in these forms:

- direct `time -o '' sh -c 'rm --no-preserve-root -rf /'`;
- direct `env -u '' sh -c 'rm --no-preserve-root -rf /'`;
- direct `env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'`.

The installed GNU utilities exit before executing harmless probe children for
all three forms. Attached `env` spellings such as `-u=HOME` and `-uA=B` have
the same behavior. This is not GNU-only: FreeBSD documents `unsetenv()` as
rejecting an empty name or one containing `=`, and its external `time` likewise
takes `-o file`, which cannot name an empty pathname. BusyBox differs: its
`env -u` applet executes the child for empty and `=`-containing names, while
its `time -o ''` applet exits before the child. A single wrapper-independent
validity rule would therefore be incorrect.

This is a false hard-deny rather than a fail-open bypass. It still violates the
approved enforcement boundary: when a child command is not proven to execute,
Coding Brain must preserve native confirmation instead of issuing a provider-
level deterministic denial.

## Decision

Replace the classifiers' boolean value-arity result with a small typed option-
value classification. The type represents:

- whether the option takes no value or requires a value;
- whether a required value is attached to the option token or supplied by the
  next shell word;
- the semantic validity rule for that value.

The initial closed set of validity rules is:

- `Any`: any content is semantically valid once a required value argument is
  proven present;
- `NonEmpty`: an empty literal value is invalid;
- `EnvUnsetName`: an empty literal or one containing `=` is invalid.

The names used in production may follow local style, but the representation
must keep arity, attached-value provenance, and semantic validation together.
Adding another wrapper option with distinct value semantics should require a
new classifier rule, not another ad hoc check in the scanner loop.

Wrapper grammars select the rule explicitly:

- audited direct external `time -o`: `NonEmpty`;
- BusyBox `time -o` and `time -f`: `NonEmpty`;
- audited direct external `env -u`: `EnvUnsetName`;
- BusyBox `env -u`: `Any`.

Other supported options retain their current behavior. Toybox option-bearing
`env` and `time` forms remain unsupported and indeterminate. No runtime utility
identity detection or host probing is added to production evaluation.

The portability boundary is grounded in the existing direct-wrapper design,
the FreeBSD `time(1)` and `unsetenv(3)` contracts, and harmless GNU/BusyBox
differential probes. Production code encodes the audited grammar; it does not
infer the utility implementation from a path:

- <https://man.freebsd.org/cgi/man.cgi?query=time&sektion=1>
- <https://man.freebsd.org/cgi/man.cgi?query=getenv&sektion=3>

## Data Flow

1. The wrapper-specific classifier parses a literal option token and returns a
   typed option classification. For attached values, it retains the attached
   substring rather than discarding it.
2. A shared value consumer handles both direct and multicall wrappers.
3. Before semantic validation, the consumer checks whether dynamic content may
   expand to zero or multiple argv. That remains a deterministic unsafe-
   expansion denial even when the token also contains literal bytes.
4. A separate value is present when the next shell word exists and expands to
   exactly one argv. For an attached dynamic value, the option prefix alone
   does not prove that bytes remain after the value-taking option: in
   `busybox env -u"$NAME"`, an empty expansion can leave bare `-u` and shift the
   next argv into the value position. Literal bytes either before or after the
   dynamic part, as in `-uX"$NAME"` or `-u"$NAME"X`, prove attachment presence.
5. When a literal value is available, the configured validity rule is applied.
   `Any` accepts all content only after presence is proven. `NonEmpty` and
   `EnvUnsetName` require a literal value proven valid; a non-literal value is
   semantically unknown and makes the wrapper boundary indeterminate. The
   scanner does not evaluate variables to recover a more deterministic result.
6. Proven-present, semantically valid values are consumed and the remaining
   words continue through nested-
   shell and destructive-command analysis.

The indeterminate return retains the original wrapper words so the enclosing
program is still scanned. Existing `Deny > Indeterminate >
NoDeterministicDecision` precedence therefore continues to deny a separate
proven destructive command in the same shell program.

## Error and Security Boundaries

- Provably invalid wrapper values return `Indeterminate`, leading to native
  provider confirmation with zero Coding Brain model requests.
- Missing separate values remain indeterminate.
- Exact-one dynamic values governed by `NonEmpty` or `EnvUnsetName` remain
  indeterminate because their semantic validity is not proven.
- Bare exact-one attached dynamics governed by `Any`, including BusyBox
  `env -u"$NAME"` and `env -iu"$NAME"`, remain indeterminate because `Any` does
  not prove that attached bytes exist. A non-empty literal prefix or suffix
  proves presence, after which `Any` consumes the value.
- Values that may expand to zero or multiple argv retain the existing
  deterministic unsafe-expansion denial before attachment-presence or semantic
  checks.
- Valid GNU and BusyBox wrapper forms carrying a proven destructive child
  retain deterministic denial.
- BusyBox `env -u` empty and `=`-containing names remain command-carrying and
  must not be reclassified using GNU semantics.
- Attached and separate spellings use the same semantic rule.
- No public API, provider payload, parser budget, destructive rule identifier,
  or model policy changes.

## Verification

Follow red-green TDD. The initial evaluator regression must fail because the
current implementation returns a deterministic root-delete denial for GNU
`time -o ''`. Extend the RED corpus before production changes to cover:

- direct `time -o ''`;
- direct `env -u ''` and `env -u '=HOME'`;
- attached direct `env -u=HOME` and `env -uA=B` equivalents;
- exact-one dynamic direct `time -o` and `env -u` values as indeterminate;
- BusyBox `time -o ''` as indeterminate;
- BusyBox `env -u ''`, `env -u '=HOME'`, `env -u=HOME`, and `env -uA=B` as
  command-carrying contrast controls;
- bare exact-one attached BusyBox `env -u"$NAME"` and `env -iu"$NAME"` as
  indeterminate, with `env -uX"$NAME"` and `env -u"$NAME"X` as presence-proven
  command-carrying controls;
- multi-argv `env -uX"$@"`, `env -u"$@"X`, and array equivalents retaining
  deterministic unsafe-expansion denial before `Any` consumption;
- valid direct and BusyBox `time` output values and `env` unset names retaining
  deterministic denial when their child is destructive;
- deny-over-indeterminate precedence in both statement orders.

Mirror the invalid and unsafe-expansion corpus in the shipped-helper integration
test and the Codex, Claude Code, and Antigravity permission-hook matrix. Every
indeterminate or deterministic unsafe-expansion provider case must make zero
model requests; indeterminate cases preserve native confirmation.

Run focused evaluator, helper, and provider tests first. Final gates are serial
`cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`,
`cargo fmt --all --check`, and `cargo build` through `nix develop path:.`, plus
harmless differential probes against installed GNU and BusyBox utilities.

## Scope

Production changes should remain in `src/brain/safety.rs` unless the RED test
demonstrates that the shell-word projection lacks required literal provenance.
Expected regression files are `src/brain/safety.rs`,
`src/brain/permission_hook.rs`, and `tests/shell_safety_helper_cli.rs`.

The change does not refactor unrelated shell analysis, modify documentation or
release versions, or authorize commit, push, pull request, merge, or
publication.

## Stress Test Results: Wrapper Option Value Semantics

### Resolved Decisions

- Keep the reusable abstraction private to `safety.rs` and limited to typed
  option-value consumption; wrapper classifiers continue to own grammar.
- Treat characters after the first value-taking short option as its attached
  value rather than resuming flag parsing.
- Encode only audited direct and launcher-specific semantics; never infer a
  utility implementation from a path or command spelling.
- Treat `Any` as semantic acceptance, not proof that an attached value exists.
  A separate exact-one dynamic value is present by construction; an attached
  dynamic requires non-empty literal bytes before or after its dynamic part.
  Bare exact-one attachments remain `Indeterminate`. Semantic rules requiring
  content validation also remain `Indeterminate` unless a literal is proven
  valid.
- Distinguish consumed, indeterminate, and unsafe-expansion outcomes so native
  confirmation and deterministic denial cannot collapse into one another.
- Use closed enums and exhaustive matches rather than traits, callbacks, or a
  runtime grammar registry.
- Verify behavior at evaluator, shipped-helper, and all-provider boundaries,
  with explicit real-binary differential probes.
- Preserve zero model inference, deny-over-indeterminate precedence, and
  launcher identity on every security-sensitive path.
- Rely on existing `ShellWord::literal` and `ShellWord::parts` provenance:
  empty quoted words are represented as `Some("")`, while the ordered parts
  distinguish literal bytes before or after dynamic content; no shell
  projection change is required.
- Keep evaluation bounded to the existing option-token scan plus one literal
  validity check. The change has no persistent state or migration, so rollback
  is an ordinary source revert.

### Changes Made

- Tightened dynamic-value handling: non-literal values governed by `NonEmpty`
  or `EnvUnsetName` now preserve native confirmation instead of being consumed
  as though semantically valid.
- Made attached short-option value parsing and the three private consumer
  outcomes explicit.
- Separated `Any` value semantics from proof that an attached argument exists,
  while preserving unsafe-expansion precedence over both checks.

### Deferred / Parking Lot

- Do not unify complete `time`, `env`, `exec`, and `sudo` classifier outputs
  until a concrete third consumer needs the same typed value semantics.
- Do not add runtime utility-version detection or variable evaluation.

### Confidence Assessment

- Overall: High.
- Areas of concern: exact preservation of current unsafe-expansion precedence
  and launcher identity must be demonstrated by RED-to-GREEN provider-matrix
  tests before the implementation is accepted.
