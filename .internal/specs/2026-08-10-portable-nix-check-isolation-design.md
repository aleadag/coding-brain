# Portable Nix Check Isolation

- Date: 2026-08-10
- Task: `codexctl-dzlb9.14`
- Brainstorming: `codexctl-ne1bf`
- Status: Approved and stress-tested

## Summary

The package derivation will stop wrapping Cargo with Bubblewrap. Its ordinary
`cargoCheckHook` will retain debug-profile, serialized checks that are portable
across supported Nix builders, while the existing required GitHub Actions test
job continues to run the complete unchanged default-feature Rust suite on
Ubuntu and macOS.

A required NixOS VM flake check will exercise the installed default-feature
package as an unprivileged consumer under an explicit filesystem policy. This
keeps production path and ownership validation unchanged and avoids compiling a
test-only path-selection seam into any `cbrain` binary.

## Problem

The current Linux package check replaces `cargo` with a Bubblewrap wrapper that
creates a nested user namespace and a synthetic filesystem root. The wrapper
exists because the Nix sandbox presents `/` as foreign-owned to its build user,
while Coding Brain intentionally rejects absolute storage paths beneath
foreign-owned ancestors.

Nested user namespaces are not guaranteed by the Nix sandbox contract. The
downstream Ubuntu 24.04 multi-user Nix builder therefore fails at Bubblewrap's
UID-map step before Cargo starts. Removing the wrapper without changing the test
boundary is also insufficient: direct storage tests and real `cbrain`
integration tests reject the sandbox root, and the production path resolver
correctly refuses relative XDG bases.

The fix must not weaken ancestor validation, disable checks, add privileges to
the downstream workflow, or make test binaries select storage differently from
installed binaries.

## Test boundary

The complete gate has three required layers:

1. **Portable package check.** `packages.default` keeps `doCheck` enabled and
   uses `cargoCheckHook` in the debug profile with one Rust test thread. It runs
   the complete `coding-brain-core` and `coding-brain-tui` crate suites, plus the
   root package's `release_workflow` and `release_metadata` contract targets.
   This stable responsibility boundary replaces a per-test portability
   allowlist.
2. **Complete Rust suite.** The existing required `Test (ubuntu-latest)` and
   `Test (macos-latest)` jobs continue running
   `cargo test --all-targets -- --test-threads=1` with default features. No test
   is ignored, conditionally bypassed, or rewritten for Nix.
3. **Installed-package VM check.** A Linux-only `runNixOSTest` derivation boots a
   NixOS guest, installs `packages.default`, and runs `cbrain` as a normal
   unprivileged user with private absolute HOME and XDG directories. It proves
   normal startup can create/open current SQLite storage, verifies the resulting
   owner and modes, and runs `cbrain doctor --json` against both current storage
   and the generated Home Manager provider-hook fixtures.

The flake defines one package value and derives the VM test from that package.
It exposes the two values through `packages.default` and
`checks.x86_64-linux.storage-security-vm`, respectively. It does not add a
`passthru.tests` back-reference from the consumed package to its VM consumer;
the explicit flake check is both the public and enforced test interface.

## Package derivation

Remove `nixCheckEntrypoint`, `nixCheckCargo`, the Linux `preCheck`, and all
Bubblewrap references from `flake.nix`. Retain:

- `checkType = "debug"`;
- `dontUseCargoParallelTests = true`;
- `doCheck` through the normal `cargoCheckHook`; and
- default build features for the installed package.

The package check uses ordinary explicit Cargo package selection only.
`cargoCheckHook` runs the two complete lower-layer crate suites by naming
`coding-brain-core` and `coding-brain-tui`; it does not use an open-ended
`--workspace --exclude` selection that would silently absorb future crates. A
small `postCheck` invokes the two named default-feature root contract targets.
The additional invocation must retain the debug profile, offline dependency
boundary, target selection, and serialized test execution of the hook. It must
not replace or suppress `cargoCheckHook`.

The package declares Git and curl as native check inputs. This is not a runtime
code change: retained core tests exercise real Git and webhook subprocess
boundaries, and a pure Nix check environment does not inherit the operator's
curl installation. The curl input prevents the webhook test from timing out
before it can send its loopback request.

The package check must not use a custom runtime feature, hidden CLI argument,
ambient test-root override, owner-check exception, per-test portability
allowlist, or `--unshare-user-try` fallback. Every root-package target remains
covered by the complete Ubuntu/macOS Cargo jobs; installed root-binary behavior
is additionally covered by the VM check.

The same portable package-check selection applies on Linux and Darwin. Only the
`storage-security-vm` check is conditional, and it is exposed on
`x86_64-linux` only. This gives `packages.default` one cross-platform check
contract while the required Ubuntu/macOS Cargo matrix remains the complete
root-package suite on both operating systems.

## VM consumer test

Add a Linux-only NixOS test under `nix/tests/`. Its guest configuration will:

- install the exact flake package under test;
- create an unprivileged `cbrain-test` user with an ordinary private home;
- set absolute `HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` owned by that
  user with mode `0700`;
- set `CODING_BRAIN_SKIP_FIRST_RUN=1` to avoid interactive onboarding; and
- use the guest's default kernel/filesystem namespace rather than Bubblewrap.

The test script exercises five installed-binary scenarios:

1. A private XDG hierarchy owned by `cbrain-test` runs
   `cbrain --distill-once`, `cbrain --brain-review list`, and
   `cbrain doctor --json` successfully. The review command explicitly creates
   the review database before its ownership and mode are asserted.
2. A private XDG state directory beneath a traversable ancestor owned by a
   second unprivileged user fails with
   `state directory ancestor is foreign-owned`.
3. A state directory beneath a trusted-owner ancestor that is writable by other
   users without the sticky bit fails with
   `state directory ancestor is replaceable by another user`.
4. Generated Home Manager provider-hook files pass Doctor, retain the Codex
   trust advisory, and report mixed global/project ownership accurately.
5. Malformed Home Manager Antigravity content fails with the exact structured
   diagnostic without exposing the fixture's secret content.

The two Home Manager scenarios were moved from the plain cross-platform module
derivation after execution proved that a Nix build sandbox's foreign-owned `/`
cannot validly host real absolute-XDG startup under the unchanged production
validator. The plain derivation continues to cover module generation on every
supported system; the VM supplies the runtime filesystem boundary.

The positive case inspects the state tree from the guest to prove:

- the Coding Brain state root and database directory are owned by
  `cbrain-test` and mode `0700`;
- SQLite database files are owned by `cbrain-test` and mode `0600`;
- no legacy JSONL path is created as canonical live storage; and
- the installed binary is the package output supplied to the test.

Each negative case asserts the exact fixed error category and proves that no
SQLite database or partial state publication was created. The VM's multiple
real users and root-controlled fixture setup are the reason for this layer; a
positive startup smoke test alone would not justify a VM.

Before invoking `cbrain` in the foreign-owner case, the test runs as
`cbrain-test` and proves it can traverse the foreign ancestor and write within
the pre-created private state leaf. In the replaceable case, it proves the
ancestor is owned by `cbrain-test`, mode `0777`, lacks the sticky bit, and is
writable by that user through an explicit write probe. These preconditions
ensure the expected failures come from Coding Brain's validator, not an
incidental kernel permission denial.

The VM is a consumer test, not an alternate build. It does not compile source,
enable Cargo features, patch the binary, or grant the test user elevated
privileges.

The test is explicitly runnable without hardware virtualization:

- `requiredFeatures.kvm = false`;
- `qemu.forceAccel = false`, allowing QEMU TCG software emulation;
- the NixOS test has a 15-minute global timeout; and
- the CI job has a 30-minute timeout.

The implementation must verify the VM locally and on the exact GitHub Ubuntu
runner. If TCG is too slow or unreliable, the work stops for a runner-boundary
decision; the check must not become optional, soft-failing, or dependent on
nested user namespaces.

The test script uses named subtests for the positive, foreign-owner, and
replaceable-ancestor scenarios. Each invocation captures exit status plus only
the final 16 KiB of stdout and stderr; negative assertion failures also print a
bounded `namei -l` ownership/mode chain for the relevant hierarchy. There are no
command retries, so infrastructure timeouts, fixture-precondition failures, and
product validation failures remain distinct.

## CI and downstream verification

Add a required 30-minute-per-leg Nix matrix on `ubuntu-latest` and
`macos-latest` using the same installer generation as the downstream workflow.
A named preflight asserts that Nix reports sandboxing enabled. Stable per-OS
matrix names make both legs independently requireable. The Ubuntu leg first
runs `nix flake check --all-systems --no-build`. Both legs then build the
current-system default package with `--print-build-logs`; a separately named
Ubuntu-only step builds:

```text
.#checks.x86_64-linux.storage-security-vm
```

The Ubuntu package build is the regression for a builder where nested user
namespace creation is unavailable: the derivation contains no Bubblewrap
invocation, so Cargo must start and the declared portable checks must complete.
The macOS package leg proves the same Nix package contract builds on Darwin.
`nix flake check --all-systems --no-build` additionally evaluates all four
standard flake systems, but does not create runtime evidence for architectures
without runners. The VM check provides deterministic installed-package
filesystem coverage. Existing Ubuntu and macOS Cargo jobs remain the
authoritative complete Rust-suite gate. Package, full-suite, and VM results must
be reported as separate claims; a green package derivation must never be
described as proof that all root-package tests ran.

After repository CI is green, rerun the linked downstream Ubuntu 24.04 Home
Manager build. It builds only the consumer package path and must not require the
VM check or additional privileges.

## Security properties

Production code is unchanged. In particular:

- absolute XDG bases remain mandatory;
- every storage ancestor continues to require its existing trusted-owner and
  non-replaceable-mode checks;
- final state directories and SQLite files retain their owner-only contracts;
- secure descriptor traversal, no-follow behavior, and inode correspondence
  remain unchanged; and
- default debug and release binaries gain no test controls.

The package check has narrower environment-independent coverage than the full
Cargo job, but no test is removed from the required repository gate. CI must
require both the complete Cargo job and the Nix package/VM job before merge.

## Regression tests

Extend `tests/release_workflow.rs` to assert that:

- `flake.nix` contains no Bubblewrap or nested-user-namespace Cargo wrapper;
- the package retains debug checks and serialized test execution;
- the package check runs both complete lower-layer crate suites and the two
  named root release/package contract targets;
- the required CI test matrix still contains both `ubuntu-latest` and
  `macos-latest` and runs the exact serialized `cargo test --all-targets`
  command;
- the storage-security VM check is exposed under the flake checks; and
- the CI workflow explicitly builds the package and VM check.

The regression is source-level support for the runtime CI evidence. It cannot
substitute for building the package on the exact Ubuntu multi-user Nix runner.
At handoff, inspect whether repository branch protection requires both complete
Cargo legs and both stable Nix matrix checks. A missing requirement is an
explicit merge blocker, and insufficient read permission is an unresolved
external verification blocker. Branch-protection mutation is external
administration and requires separate authorization.

## Acceptance mapping

1. The Ubuntu 24.04 multi-user Nix package build succeeds without nested user
   namespaces.
2. `doCheck` remains enabled with debug, serialized portable checks; the full
   unchanged default-feature Rust suite remains required in the existing
   Ubuntu/macOS Cargo jobs.
3. The VM consumer check runs the installed default-feature binary, and no
   storage security logic changes.
4. The package contains no Bubblewrap or equivalent namespace requirement.
5. Package, Home Manager module, release contracts, local NixOS, Darwin Cargo,
   and required VM checks pass.
6. The linked downstream Home Manager build or an equivalent exact runner
   reproduction is green.

This mapping intentionally revises the original requirement that every Rust
test execute inside the ordinary package derivation. The complete suite remains
mandatory, but environment-dependent coverage runs at the appropriate required
CI/VM boundary.

## Maintenance and rollback

The VM remains bounded to the five approved scenarios. A timeout or flaky VM
is a failing gate; the workflow must not add retries, optional status, or a
namespace-dependent fallback. Source-level contract tests guard both the stable
package target selection and required VM wiring.

The implementation does not update `flake.lock` unless the pinned Nixpkgs is
proven to lack a required test API. If the VM cannot meet the 30-minute GitHub
runner budget, implementation stops before merge for a new runner-boundary
decision. After merge, failures are fixed forward where possible; reverting the
entire change knowingly restores the downstream package-build defect.

Local pre-commit Nix verification uses `path:.` flake references so the newly
created VM module is included without staging it merely for Nix source
discovery. Committed CI continues to use ordinary `.#` references from its clean
checkout.

Because runtime behavior and persistent data are unchanged, no migration or
runtime rollback procedure is required. The packaging and verification-boundary
change is recorded in the changelog, without adding user-facing runtime
configuration documentation.

## Out of scope

- Relaxing or making Linux storage ancestor validation namespace-aware.
- Adding public or hidden storage-root overrides.
- Changing XDG path semantics.
- Granting GitHub Actions extra user-namespace or mount privileges.
- Replacing the existing Ubuntu/macOS full Cargo jobs.
- Committing, pushing, or rerunning downstream workflows without separate
  authorization.

## Stress Test Results: Portable Nix Check Isolation

### Resolved Decisions

- Package checks use a stable responsibility boundary: both complete lower-layer
  crate suites plus the root release/package contract targets.
- The existing Ubuntu/macOS `cargo test --all-targets` matrix remains the
  explicitly separate authoritative complete-suite gate.
- The VM proves one positive and two real multi-user negative filesystem cases;
  it is not merely a startup smoke test.
- The VM permits TCG, does not require KVM, and fails closed within bounded test
  and CI timeouts.
- The flake exposes the VM only through `checks`, avoiding a package/VM
  `passthru.tests` reference knot.
- CI builds the Nix package on `ubuntu-latest` and `macos-latest`, asserts Nix
  sandboxing, and reports the Linux-only VM as a separate logged step.
- VM fixture preconditions prove negative results come from Coding Brain's
  validator rather than incidental Unix permission denial.
- Named subtests and bounded metadata diagnostics distinguish infrastructure,
  fixture, and product failures without retries.
- Linux and Darwin packages use the same portable check contract; only the VM
  check is Linux-specific.
- VM failures are fixed forward and never converted into skips or namespace
  fallbacks; a runner-budget failure reopens the design before merge.

### Changes Made

- Replaced a per-test portability allowlist with a stable crate/contract split.
- Strengthened the VM from a positive consumer smoke test to explicit positive,
  foreign-owner, and replaceable-ancestor cases.
- Removed the optional `passthru.tests` exposure.
- Made software-emulated VM execution and time budgets explicit.
- Unified Linux and Darwin package-check selection.
- Added exact CI authority, diagnostics, maintenance, and rollback requirements.

### Deferred / Parking Lot

- Repository branch-protection requirements are inspected at handoff; missing
  required checks block merge, and changing settings requires separate
  authorization.
- If GitHub-hosted TCG cannot meet the bounded budget, runner selection requires
  a new design decision rather than an automatic fallback.

### Confidence Assessment

- Overall: High
- Areas of concern: GitHub-hosted TCG duration remains an empirical acceptance
  gate; the implementation must also confirm the exact portable Cargo command
  shape supported by the pinned Nixpkgs hook.
