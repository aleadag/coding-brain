# Multicall Applet Safety Boundary Design

> **Date:** 2026-08-02
> **Brainstorm:** `codexctl-p30h`
> **Issue:** `codexctl-dchq`
> **Status:** Approved and stress-tested
> **Extends:** `2026-07-31-dchq-nested-shell-safety-design.md`

## Context

The nested-shell safety evaluator recognizes direct `env` and `time` wrappers
and direct `busybox sh|ash` and `toybox sh` carriers. These registries do not
compose: `busybox env sh -c ...`, `busybox time sh -c ...`, and
`toybox env sh -c ...` currently return `NoDeterministicDecision`, even though
the multicall applet executes the nested command. This permits model inference
for a destructive command hidden behind two individually recognized execution
boundaries.

## Decision

Treat a literal `busybox` or `toybox` selector as a fail-closed multicall
dispatch boundary:

- `sh` and the already supported platform-specific shell applets retain their
  existing nested-shell analysis.
- Known command-carrying `env` and `time` applets feed their remaining words
  through the existing wrapper normalization and nested-execution analysis.
- A missing, dynamic, option-like, or otherwise unsupported applet selector is
  `Indeterminate`, not `NoDeterministicDecision`.

The execution-bearing applet registry is closed: only audited `env` and `time`
selectors are normalized. Another command-carrying applet requires an explicit
code and regression-test update. For `time`, only exact known options may
advance classification to the nested command; unknown or ambiguous options
are `Indeterminate` rather than assumed to take no value.

The evaluator remains lexical and provider-neutral. It does not inspect the
installed BusyBox/Toybox applet set, execute a probe, resolve `PATH`, or infer
runtime availability. Existing aggregate parser budgets and
`Deny > Indeterminate > NoDeterministicDecision` precedence remain unchanged.

## Consequences

Proven destructive payloads behind multicall `env` or `time` are denied with
the existing destructive-command rule. Ambiguous or unsupported multicall
dispatch invokes the model zero times and preserves native provider
confirmation. Consequently, benign unsupported forms such as a literal
`busybox ls` are conservative `Indeterminate` results rather than candidates
for automatic approval.

No provider payloads, configuration, activity schemas, or public APIs change.
The implementation should reuse the existing wrapper classifier rather than
create a second `env` or `time` option parser. Tightening unknown or ambiguous
direct `time` options to `Indeterminate` is an intentional fail-closed behavior
change needed to keep command-position classification sound.

## Verification

Tests are added before production changes and cover:

- evaluator denial for BusyBox/Toybox `env` and `time` applets carrying a
  destructive literal shell program;
- shipped-helper denial for the reopened `busybox env`, `busybox time`, and
  `toybox env` reproductions;
- Codex, Claude Code, and Antigravity denial or indeterminate handling with
  zero model requests;
- missing, dynamic, option-like, and unsupported applet selectors returning
  `Indeterminate`;
- benign recognized `env`/`time` payloads and inert quoted text as controls;
- repeated recognized wrapper composition without resetting parse budgets;
- the existing nested-shell, wrapper, and direct destructive-command suites.

Focused checks run first, followed by the repository-required format, Clippy,
and full serial test gates. Real installed BusyBox `env` and `time` probes are
used as runtime evidence when available, but tests do not depend on those
external binaries.

## Stress Test Results: Multicall Applet Safety Boundary

### Resolved Decisions

- Compose multicall dispatch inside the existing iterative wrapper
  normalization; do not add another recursive evaluation path.
- Keep a closed `env`/`time` applet registry and fail closed for registry drift.
- Recognize only an exact literal selector in argument position one; do not
  guess through launcher options, aliases, abbreviations, or runtime identity.
- Tighten unknown and ambiguous `time` options to `Indeterminate` so an option
  cannot silently shift the inferred command position.
- Preserve the existing aggregate source/node limits and require every
  normalization step to consume input.
- Accept native-confirmation friction for benign unsupported multicall applets
  as the deliberate cost of a general fail-closed dispatch boundary.
- Retain deterministic analysis instead of relying solely on provider-native
  policy or reducing every recognized multicall payload to uncertainty.
- Preserve `Deny > Indeterminate > NoDeterministicDecision`, existing rule
  identifiers, fail-closed audit behavior, and zero model requests for deny and
  indeterminate results across all three providers.
- Verify the evaluator, shipped helper, real BusyBox semantics, and every
  provider hook before running the complete serial quality gates.

### Changes Made

- Made the closed applet registry and selector-position contract explicit.
- Required fail-closed handling for unknown or ambiguous `time` options.
- Added repeated-wrapper budget coverage and the complete provider/helper test
  boundaries.

### Deferred / Parking Lot

- Provider-native permission enforcement remains the preferred long-term
  primary boundary, but replacing deterministic defense in depth is deferred
  until parity and provider-drift behavior are independently verified.

### Confidence Assessment

- Overall: High
- Areas of concern: Compatibility friction for benign unsupported multicall
  applets is intentional and must remain visible in regression tests.
