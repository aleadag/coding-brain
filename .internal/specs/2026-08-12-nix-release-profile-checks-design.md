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
expression. The contract must reject an explicit debug check type and require
the custom integration-test command to select the release profile. Existing
assertions continue to guard package selection, portability constraints,
Darwin skips, offline execution, and serialization.

No new runtime error path is introduced. A release-only test failure remains a
normal Nix check-phase failure and must fail the derivation; no retries, skips,
or weakened checks are added.

## Verification

1. Run the focused release-workflow contract before the implementation and
   confirm it fails for the old profile configuration.
2. Apply the two-line behavioral change and confirm the focused contract passes.
3. Run Nix formatting and evaluation, Rust formatting, Clippy, and the relevant
   workspace tests.
4. Run and time a clean Linux `nix build --no-link --print-build-logs .#`; inspect
   the log for release-profile check commands and absence of a debug dependency
   build graph.
5. Treat a clean macOS Nix build and timing as hosted-CI acceptance that cannot
   be claimed from the current Linux environment.

Build timing is contextual evidence, not a correctness threshold. Success is
the shared release profile, retained checks, and passing package build.

## Security and Compatibility

This changes build-time Cargo profile selection only. It does not alter storage
validation, sandboxing, test coverage boundaries, package metadata, runtime
configuration, or Home Manager wiring. Checks remain fail-closed and enabled.
