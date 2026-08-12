# Nix Release-Profile Checks Design

## Context

The installable Nix package uses Cargo's release profile for its build, but
`flake.nix` overrides `buildRustPackage` checks to use the debug profile. Its
custom `postCheck` also runs two integration tests without selecting a profile.
That creates a separate debug dependency and artifact graph inside a package
build whose runtime artifact is release-mode. Ordinary GitHub Actions jobs
already run the workspace tests in debug mode on Linux and macOS.

The pinned Nixpkgs implementation defines `checkType ? buildType`; its default
`buildType` is `"release"`. The Cargo check hook adds `--profile release` when
the check type is not debug.

## Decision

Remove the explicit `checkType = "debug"` override from the package. This lets
the standard Nixpkgs check phase inherit the release build type. Add
`--release` to the custom `postCheck` Cargo invocation so its
`release_workflow` and `release_metadata` integration tests use the same Cargo
profile.

Do not restructure the check phase or change its test selection. Preserve:

- the core and TUI package checks;
- the two Darwin-only test skips;
- offline, target-specific execution;
- serialized test execution;
- both custom integration tests;
- the ordinary debug-profile GitHub Actions test matrix; and
- the package consumed by the Home Manager module.

## Contract Changes

Update the existing release-workflow contract tests before changing the Nix
expression. The contract must reject any explicit `checkType` override and
require `--release` specifically inside the custom `postCheck` block, rather
than accepting the token elsewhere in the flake. Existing assertions continue
to guard package selection, portability constraints, Darwin skips, offline
execution, serialization, and ordinary debug-profile CI coverage.

No new runtime error path is introduced. A release-only test failure remains a
normal Nix check-phase failure and must fail the derivation; no retries, skips,
or weakened checks are added.

## Verification

1. Rebase onto current `origin/main` and confirm the target Nix, Cargo, contract,
   and CI files retain the analyzed behavior.
2. Before changing code, force and time a clean local package rebuild to capture
   the debug-profile baseline without accepting an existing store result or
   binary substitute as a build measurement.
3. Update and run the focused release-workflow contract before the implementation
   and confirm it fails for the old profile configuration.
4. Apply the two-line behavioral change and confirm the focused contract passes.
5. Run Nix formatting and evaluation, Rust formatting, Clippy, and the relevant
   workspace tests.
6. Force and time the same clean Linux package rebuild after the change. Inspect
   its log for a release-profile primary check and `--release` post-check, absence
   of a debug-profile command or artifact graph, and execution of all expected
   tests. Test harnesses, `cfg(test)` variants, and the two additional integration
   binaries may still require compilation within the shared release profile.
7. Treat a clean macOS Nix build and timing as hosted-CI acceptance that cannot
   be claimed from the current Linux environment. Keep `fwrpm` in progress until
   that evidence exists.

Build timing is contextual evidence, not a correctness threshold. Success is
the shared release profile, retained checks, and passing package build.

## Security and Compatibility

This changes build-time Cargo profile selection only. It does not alter storage
validation, sandboxing, test coverage boundaries, package metadata, runtime
configuration, or Home Manager wiring. Checks remain fail-closed and enabled.

## Stress Test Results: Nix Release-Profile Checks

### Resolved Decisions

- Inherit `checkType` from `buildType` so build and check profiles remain
  structurally aligned; do not hard-code a second profile setting.
- Define reuse as one Cargo profile and dependency graph, not a claim that every
  crate or test harness compiles exactly once.
- Rebase before measuring or implementing so verification covers the current
  upstream base.
- Treat release-only test failures as defects to diagnose; do not fall back to
  debug checks or skip tests.
- Keep the separate bounded `postCheck`; merging it into the package-selected
  primary check would widen or complicate existing test boundaries.
- Keep local Linux readiness distinct from hosted macOS acceptance and Bead
  closure.
- Preserve the existing fail-closed security, sandbox, and test boundaries.

### Changes Made

- Tightened the regression contract to forbid a `checkType` override and scope
  the `--release` assertion to `postCheck`.
- Added current-base rebasing, forced before/after clean-build timing, explicit
  log evidence, and hosted macOS completion boundaries to verification.

### Deferred / Parking Lot

- Hosted macOS clean-build evidence and timing require a later authorized push
  or PR; they cannot be produced in the current Linux environment.

### Confidence Assessment

- Overall: High
- Areas of concern: Build timing remains contextual because host load and warm
  Nix-store dependencies vary; correctness does not depend on a timing threshold.
