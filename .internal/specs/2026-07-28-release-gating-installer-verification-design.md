# Release Gating and Installer Verification Design

## Context

The tag-triggered release workflow currently checks only that the root Cargo
version matches the tag before building and publishing. A tag can therefore
publish crates and GitHub release assets without rerunning the release-critical
quality suite.

The binary installer still targets the former `aleadag/codexctl` repository. It
also treats a missing checksum asset or checksum utility as success, and it
applies executable permissions after a potentially privileged move.

## Design

### Release gate

Extend the existing `verify` job in `.github/workflows/release.yml`. After the
tag/version check, install the Rust formatting and Clippy components and run:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`

The existing `build` dependency on `verify` remains the single gate into the
build, crate-publishing, GitHub Release, and package-update chain. A failed
verification step must prevent every downstream release job.

### Installer verification

Change the installer repository and usage URL to the canonical
`aleadag/coding-brain` repository.

Before downloading release assets, require either `shasum` or `sha256sum`.
Download both the archive and its checksum asset with failure-enabled `curl`.
Run the selected verifier from the temporary directory before extracting the
archive. A missing checksum, unavailable verifier, or checksum mismatch must
terminate the installer without changing the destination.

### Installation

After successful verification and extraction, install the binary with
`install -m 0755`. Invoke `install` directly when the destination directory is
writable; otherwise invoke the same operation through `sudo`. This makes the
destination write and final executable mode one privileged operation.

## Error Handling and Security

All verification failures are fail-closed under the script's existing `set -e`
behavior. Extraction and installation occur only after checksum validation.
The temporary directory remains covered by the existing exit trap.

No fallback permits an unverified archive, and no post-install unprivileged
mode change is required.

## Verification

Add a hermetic Rust integration test at `tests/install_script.rs`. The test runs
`install.sh` with temporary stub commands so `cargo test --all-targets`
enforces the installer contract in both CI and the release gate. Cover:

- canonical release URLs;
- refusal when neither checksum utility exists;
- refusal when the checksum asset cannot be downloaded;
- refusal when checksum validation fails;
- successful verification through both the `shasum` and `sha256sum` paths;
- installation through `install -m 0755` after successful validation.

Run:

- `cargo test --all-targets`
- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`

## Scope

Do not refactor the general CI workflow into reusable workflows, change the
release artifact format, or alter unrelated packaging flows.

## Stress Test Results: Release Gating and Installer Verification

### Resolved Decisions

- Enforce installer regressions through a Rust integration test discovered by
  `cargo test --all-targets`, rather than an unhooked standalone shell test.
- Define the tag gate as the release-critical quality suite, not an exact
  duplicate of every normal-CI job. The downstream release build matrix still
  compiles all supported Linux and macOS targets.
- Keep every publishing job transitively dependent on the existing `verify`
  gate.
- Fail closed for missing, malformed, mismatched, or unverifiable checksums
  before extraction.
- Use one direct or privileged `install -m 0755` operation with no
  `mv`/`chmod` fallback.
- Exercise both supported checksum utilities in hermetic tests.

### Changes Made

- Replaced the proposed POSIX shell test harness with a Rust integration test so
  the approved Cargo test gate enforces installer behavior.
- Narrowed the release-gate wording to match the commands it actually runs.

### Deferred / Parking Lot

- Refactoring normal CI into a reusable workflow is intentionally out of scope.
- Existing workflow-wide token permissions are unchanged.

### Confidence Assessment

- Overall: High
- Areas of concern: GitHub Actions execution remains externally verified only
  when the changed workflow runs on a tag.
