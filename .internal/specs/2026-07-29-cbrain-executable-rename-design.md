# `cbrain` Executable Rename Design

## Summary

Rename the sole installed executable from `coding-brain` to `cbrain`. Keep the
Coding Brain product, package, repository, Rust crate, Home Manager option,
configuration, state, and project namespaces unchanged.

The rename is intentionally breaking: no `coding-brain` wrapper, symlink,
alias, or compatibility executable will be shipped. Exact old managed hook
commands remain recognizable only as stale entries so `cbrain init` and
`cbrain doctor` can replace or remove them safely.

## Context

`coding-brain` is descriptive but cumbersome for frequent interactive use.
The chosen replacement is `cbrain`. An existing, apparently dormant PyPI
project has used the same command name, but this known collision is accepted in
favor of the shorter and more natural command.

This change is narrower than a product or namespace rename. Existing data and
configuration paths remain valid and must not be migrated.

## Decision

The root Cargo package remains named `coding-brain`, while its only binary
target becomes `cbrain`:

```text
cargo install coding-brain
             │
             └── installs bin/cbrain
```

All executable-facing surfaces use `cbrain`, including:

- the Cargo binary target and test binary environment variable;
- Clap help, version output, completions, and manpage output;
- onboarding, doctor, diagnostics, and command examples;
- generated Codex, Claude, and Antigravity hook commands;
- the install script and Nix, Homebrew, AUR, and nixpkgs package outputs;
- release archive contents.

Package and archive identities remain unchanged. For example, release archives
remain named `coding-brain-<version>-<target>.tar.gz`, but contain only the
`cbrain` executable.

## Preserved Namespaces

The following names remain `coding-brain`:

- the crates.io package and internal Rust crates;
- the repository and package/formula identities;
- the Home Manager option `programs.coding-brain`;
- `$XDG_CONFIG_HOME/coding-brain`;
- `$XDG_STATE_HOME/coding-brain`;
- `.coding-brain.toml` and `.coding-brain/project.toml`;
- environment variables beginning with `CODING_BRAIN_`;
- product prose using “Coding Brain.”

No config, state, project metadata, or data migration is part of this change.

## Managed Hook Classification

Executable recognition uses three explicit classes:

```rust
const CURRENT_PROGRAM: &str = "cbrain";
const STALE_MANAGED_PROGRAMS: &[&str] = &["coding-brain", "codexctl"];
```

A shared root-crate classifier identifies an exact executable basename as
current, stale-managed, or unmanaged. Absolute paths are classified by their
final path component.

- Hook generation and current-definition validation accept only `cbrain`.
- Diagnosis, replacement, and removal may recognize exact
  `coding-brain` and `codexctl` managed commands as stale.
- Existing exact argument, provider, timeout, matcher, and ownership checks
  remain in force.
- Similar or arbitrary command names remain unmanaged.

This is cleanup compatibility, not command compatibility: invoking
`coding-brain` after the upgrade fails unless some unrelated installation
provides it.

## Packaging

Every supported installation route installs exactly one executable:
`cbrain`.

- Nix keeps `pname = "coding-brain"` and changes `meta.mainProgram` to
  `cbrain`.
- Home Manager keeps `programs.coding-brain` and derives immutable hook
  commands from the selected package's `cbrain` main program.
- Homebrew and AUR keep their package/formula names but install and test
  `cbrain`.
- Before writing anything, the shell installer refuses to proceed when the old
  `${INSTALL_DIR}/coding-brain` path exists. It tells the user to verify and
  remove that file explicitly, then rerun the installer; it does not guess
  ownership or delete the file silently.
- After that preflight, the shell installer extracts and installs `cbrain`.
- Release workflows archive the `cbrain` build artifact under the existing
  `coding-brain-...tar.gz` archive name.

No installation route includes both executable names.

## Documentation

User-facing commands change to `cbrain`. Documentation explicitly distinguishes
the `coding-brain` package name from the `cbrain` command where installation is
shown:

```sh
cargo install coding-brain
cbrain init codex
cbrain doctor
```

Historical changelog text remains historical unless current guidance or a
current compatibility statement would otherwise become false. Paths and
namespace examples continue to use `coding-brain`.

## Verification

Focused tests must prove:

- the Cargo package exposes `CARGO_BIN_EXE_cbrain`;
- Clap help, completions, and manpage output identify `cbrain`;
- generated hooks use `cbrain`;
- exact `coding-brain` and `codexctl` hook commands are stale-managed;
- lookalike executable names remain unmanaged;
- old exact managed hooks can be diagnosed and replaced without duplication;
- installers and package definitions install only `cbrain`;
- the shell installer fails before writing when the old executable path exists;
- current package outputs and manifests contain only `cbrain`, while the
  package managers' tracked ownership/generation contracts handle replacement;
- release archives contain `cbrain` while retaining existing archive names;
- Home Manager generates immutable `cbrain` hook paths;
- XDG, project, package, repository, and Rust crate namespaces remain
  `coding-brain`.

Final validation includes:

```sh
cargo fmt --check
cargo test
cargo clippy -- -D warnings
nix build .#checks.x86_64-linux.home-manager-module
```

Relevant packaging and release guard tests must also pass.

## Non-Goals

- Renaming the product, repository, package, Rust crates, Home Manager option,
  XDG paths, project files, or environment variables.
- Shipping an alias, wrapper, symlink, or second executable.
- Preserving invocation compatibility with `coding-brain`.
- Modifying unrelated historical records solely to replace old command text.

## Stress Test Results: `cbrain` Executable Rename

### Resolved Decisions

- Keep package and archive identities as `coding-brain`, but state and test
  clearly that users run `cbrain`.
- Recognize old executable basenames only inside full, exact managed-hook
  ownership checks.
- Update every release producer and installer consumer atomically.
- Accept a documented interruption for imperative hooks until users run
  `cbrain init <provider>`.
- Use the running absolute executable for imperative hooks, `lib.getExe` for
  Home Manager hooks, and `cbrain` for fallbacks and PATH diagnostics.
- Accept the known external `cbrain` collision without executing unknown
  binaries or adding unreliable ownership heuristics.
- Use context-specific edits and paired executable-versus-namespace regression
  guards rather than a global text replacement.
- Refuse raw-installer upgrades while the old executable path exists instead of
  leaving two commands or deleting a file with uncertain ownership.

### Changes Made

- Added a fail-before-write shell-installer preflight for an existing
  `${INSTALL_DIR}/coding-brain`.
- Added verification that raw and package-manager upgrade paths do not retain
  the old executable.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: rendered outputs and manifests can prove only the package
  boundary controlled by this repository; external package-manager upgrade
  engines remain upstream contracts.
