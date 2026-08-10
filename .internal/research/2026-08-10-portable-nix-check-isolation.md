# Research: Portable Nix Check Isolation

> **Date:** 2026-08-10
> **Bead:** codexctl-mpbq8
> **Status:** Complete

## Summary

The Linux package check fails before Cargo when its Bubblewrap wrapper cannot create a nested user namespace, but deleting the wrapper exposes a real path-boundary mismatch: unit and CLI integration tests reject the Nix sandbox's foreign-owned absolute root ancestor. Relative fixtures can cover direct storage APIs, but they cannot by themselves preserve real `cbrain` subprocess coverage because the production path resolver correctly requires absolute XDG bases. A check-only runtime feature would make the tested binary differ at the path-selection boundary and is not the preferred design. The clean boundary is to retain portable checks in `cargoCheckHook` and run the unchanged filesystem/CLI security suite as an explicitly required VM-backed flake/package test.

## Key Findings

### The wrapper adds a host capability before the normal check phase

> **Confidence:** high — repository evidence and the Nixpkgs hook source agree; the external citation was independently verified.

`flake.nix` replaces `cargo` during Linux checks with a Bubblewrap invocation containing `--unshare-user`, creates a tmpfs `/`, normalizes `/` and `/tmp`, then invokes the original Cargo. Nixpkgs' hook still owns the intended test command: it runs `cargo test`, and only assigns `cargoCheckHook` when `checkPhase` is otherwise unset. [S1]

Bubblewrap 0.11.2 defines `--unshare-user` as strict creation of a new user namespace and separately defines `--unshare-user-try` as the option that skips creation when unavailable. [S2] Therefore the current wrapper has no portable fallback, and using the `-try` form would silently change the filesystem boundary rather than preserve the test environment.

### The wrapper compensates for a real fixture-ancestry mismatch

> **Confidence:** high — reproduced twice with complete Nix derivations and a stable error category.

An override that removed `preCheck` reached `cargoCheckHook`, ran 1,128 unit tests, and failed 53 storage-related tests with `InvalidStorage("state directory ancestor is foreign-owned")`; 1,063 passed and 12 were ignored. The failures originate in `validate_or_create_state_root`, which starts absolute paths at `/`, and `validate_safe_ancestor_metadata`, which accepts only UID 0 or the effective UID and rejects unsafe writable modes.

A second derivation created an owner-only relative `.nix-test-tmp` and exported it through `TMPDIR`. It still failed the same storage tests because Rust temporary paths are resolved as absolute paths before storage validation; it also exposed five unrelated Home Manager inspection failures caused by changing the test environment. Environment-only `TMPDIR` rebasing is therefore insufficient.

### Existing regressions do not exercise denied nested namespaces

> **Confidence:** high — direct codebase inspection.

`tests/release_workflow.rs` checks that the flake remains feature-free and uses debug checks, but it does not execute the package check. The Home Manager module test overrides the real package with `doCheck = false`, so it validates wiring rather than the default package's check phase. The downstream Ubuntu 24.04 job is the exact runner evidence: the release build completed, then Bubblewrap failed while setting the UID map before Rust tests began. [S3]

`tempfile` 3.27.0 deliberately converts even `tempdir_in(".")` to an absolute path so cleanup remains valid after a working-directory change. Therefore changing constructors alone cannot establish the relative traversal boundary: test support must retain the absolute `TempDir` for cleanup while separately exposing a relative path beneath the current directory to storage APIs.

### Relative fixtures alone cannot preserve CLI integration coverage

> **Confidence:** high — reproduced with a focused unwrapped Nix derivation and confirmed against the path resolver.

A focused unwrapped derivation ran `cargo test --offline --test storage_export -- --nocapture`. All six tests failed at `tests/storage_export.rs:54` with `InvalidStorage("state directory ancestor is foreign-owned")`. Rebasing the parent-side fixture would only move the failure: the spawned `cbrain` process resolves storage through `CodingBrainPaths`, and `crates/coding-brain-core/src/paths.rs` rejects relative `HOME`, `XDG_CONFIG_HOME`, and `XDG_STATE_HOME` bases. The full intended CLI suite therefore cannot consume the relative storage path without a narrowly gated test seam, while changing the production XDG contract would expand scope and weaken the boundary.

### Local success and GitHub failure differ at the host user-namespace policy

> **Confidence:** high for the boundary; medium for the GitHub policy mechanism because the failed job did not record its user-namespace sysctls or AppArmor decision.

The current host runs Linux 6.11 and Nix 2.34.6 with `sandbox = true`. It reports `user.max_user_namespaces = 2147483647` and `kernel.unprivileged_userns_clone = 1`; `/usr/bin/unshare --user --map-root-user /usr/bin/true` succeeds. A fresh default `nix build` also crossed `cargoCheckHook` and the Bubblewrap UID-map boundary and began running the 1,128-test unit suite, including the storage tests that failed without the wrapper.

The GitHub runner used Ubuntu 24.04.4 and Nix 2.35.1 with sandboxing enabled. Its release build completed, but the first wrapped Cargo invocation stopped at `bwrap: setting up uid map: Permission denied`. This proves that the derivation inputs and outer Nix sandbox are not the differentiator: the nested child namespace mapping is permitted on the current host and denied by the GitHub host's kernel/LSM/namespace policy. Ubuntu documents that AppArmor can deny unprivileged user namespaces [S5], but the job lacks the sysctl and audit evidence needed to attribute this particular `EPERM` exclusively to AppArmor rather than an enclosing namespace mapping constraint.

### Best-practice boundary: package checks plus a required VM test

> **Confidence:** high — the Nixpkgs and NixOS manuals explicitly define these test roles.

Nixpkgs documents `passthru.tests` for tests that should access a package as consumers do without changing every package build, and documents NixOS tests as VM-backed derivations. [S6] The NixOS manual provides `pkgs.testers.runNixOSTest` for projects outside Nixpkgs. [S7] This matches the actual boundary here: ordinary Rust checks that do not depend on filesystem-root ownership remain in `cargoCheckHook`, while the unchanged storage/CLI suite runs in a guest whose user, filesystem ancestry, and security policy are explicit.

A non-default check-only feature is technically possible because Nixpkgs separates `cargoCheckFeatures` from `cargoBuildFeatures`, but it is rejected as the preferred design: it changes runtime path selection in the test binary, so the suite would no longer exercise the same path-resolution boundary as the installed binary. A dedicated VM test changes only the execution environment.

## Comparisons

| Approach | Portability | Security fidelity | Scope | Verdict |
|---|---|---|---|---|
| Delete the wrapper | Portable | Production checks remain strict, but fixtures fail | Small | Rejected by reproduction |
| `--unshare-user-try` or skip failures | Host-dependent | Silently removes the intended isolated root | Small | Rejected |
| Relax foreign-owner validation in production | Portable | Weakens the documented ancestor contract | Small | Rejected by acceptance criteria |
| Rebase direct storage fixtures only | Portable | Keeps production validation, but does not cover real CLI subprocesses | Moderate test-only change | Insufficient alone |
| Add a non-default, check-only relative state-root seam plus relative fixtures | Portable | Test binary differs from the shipped binary at path selection | Moderate-to-large test boundary | Rejected as non-idiomatic |
| Move the exact storage/CLI suite to a required NixOS VM check | Deterministic if VM is runnable | Same binary and unchanged security logic | Larger test derivation | Recommended |

## Codebase Context

- `flake.nix:28-75` defines and installs the Linux-only Cargo/Bubblewrap wrapper.
- `src/brain/storage/security.rs:685-708` chooses `/` for absolute state paths and `.` for relative paths.
- `src/brain/storage/security.rs:840-857` enforces trusted ownership and non-replaceable ancestor modes.
- `tests/release_workflow.rs:104-123` is the existing source-level Nix/release contract seam.
- Storage tests use `tempfile::tempdir()` across unit and integration suites; a shared test-support path constructor is preferable to production environment exceptions.

## Recommendations

1. Revise the task's verification wording so the ordinary package derivation keeps `doCheck = true` and runs the portable debug/serialized checks, while the complete unchanged storage/CLI suite is mandatory in a VM-backed flake check rather than inside every package build.
2. Remove the Bubblewrap Cargo wrapper and add a `pkgs.testers.runNixOSTest` derivation exposed under `checks.<system>` and, where useful, `passthru.tests`. Configure the guest's unprivileged user and filesystem policy explicitly and run the same default-feature binary/tests without a path-selection seam.
3. Make CI explicitly build both the package and VM check; `passthru.tests` alone is insufficient because Nixpkgs notes that CI systems do not universally build them by default. [S6]
4. Verify the package's portable checks, the full VM suite, Home Manager module check, release workflow contracts, formatting, and clippy; then rerun the linked downstream Ubuntu 24.04 Home Manager build.

## Open Questions

- Whether the exact GitHub-hosted runner can execute a NixOS VM test with acceptable reliability remains unproven.
- The current acceptance wording requires the full suite inside the ordinary package check; adopting the recommended boundary requires explicit approval to treat the required package check plus required VM check as the complete gate.
- The current environment cannot by itself prove the GitHub-hosted runner result; the exact downstream rerun remains the final acceptance gate.

## Refuted / Discarded Claims

- "The wrapper can simply be removed because ordinary Nix sandbox metadata is already accepted." Refuted by 53 deterministic `foreign-owned` failures.
- "A relative `TMPDIR` is sufficient." Refuted by the second full derivation.
- "Relative test fixtures alone preserve the full suite." Refuted by the focused `storage_export` derivation and the absolute-XDG resolver contract.
- "A check-only runtime feature is the best practice." Rejected: although Nixpkgs supports separate check features, changing path selection makes the test binary differ at the boundary under test.

## Sources

- [Nixpkgs cargo-check-hook.sh](https://github.com/NixOS/nixpkgs/blob/73f45ba0f4283ef24474c0d3fafd46ea6fd83d06/pkgs/build-support/rust/hooks/cargo-check-hook.sh#L2-L50) — Primary/Official — 2026-08-10 snapshot — cargo test and checkPhase assignment contract. [S1]
- [Bubblewrap 0.11.2 manual](https://github.com/containers/bubblewrap/blob/v0.11.2/bwrap.xml#L423-L431) — Primary/Official — 2026-04-23 release — strict and best-effort user namespace options. [S2]
- [Downstream Home Manager failure](https://github.com/aleadag/nix-configs/actions/runs/31335895845/job/93301552864) — Primary/Runtime evidence — 2026-08-10 — Ubuntu 24.04 nested UID-map denial. [S3]
- [Nix sandbox configuration reference](https://nix.dev/manual/nix/stable/command-ref/conf-file.html#conf-sandbox) — Primary/Official — Nix 2.28 stable documentation — sandbox namespace contract.
- [Ubuntu AppArmor unprivileged user namespace restrictions](https://documentation.ubuntu.com/security/security-features/privilege-restriction/apparmor/#apparmor-unprivileged-user-namespace-restrictions) — Primary/Official — Ubuntu security documentation — host LSM mediation of user namespace creation. [S5]
- [Nixpkgs package and `passthru.tests` manual](https://nixos.org/manual/nixpkgs/stable/#sec-package-tests) — Primary/Official — Nixpkgs 26.05 manual — consumer tests and VM-backed NixOS tests. [S6]
- [NixOS test manual](https://nixos.org/manual/nixos/unstable/#sec-nixos-tests) — Primary/Official — current unstable manual — `runNixOSTest` returns a test derivation for projects outside Nixpkgs. [S7]
