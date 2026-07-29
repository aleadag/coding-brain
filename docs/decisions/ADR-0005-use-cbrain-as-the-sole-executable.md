# ADR-0005: Use `cbrain` as the Sole Executable

- Status: Accepted
- Date: 2026-07-29
- Bead: `codexctl-w4cj`

## Context

The `coding-brain` command is descriptive but cumbersome for frequent
interactive use. The product is still named Coding Brain, and its established
package, repository, Rust crate, Home Manager option, XDG, environment-variable,
and project namespaces are not being reconsidered.

Several shorter commands were evaluated. `cbrain` is the most natural
contraction, although an apparently dormant PyPI project has used the same CLI
name. The collision is accepted as a naming trade-off. Shipping both names
would avoid an immediate command break, but it would create a lasting
compatibility surface and violate the requirement for one executable.

The approved design and its eight-branch stress test are recorded in the
[`cbrain` executable rename design](../../.internal/specs/2026-07-29-cbrain-executable-rename-design.md).

## Decision

The sole installed executable is renamed from `coding-brain` to `cbrain`.
There is no wrapper, symlink, alias, or compatibility executable.

The crates.io package and package-manager identities remain `coding-brain`.
Release archives retain their existing `coding-brain-...tar.gz` names but
contain only `cbrain`. User-facing installation documentation must make the
package/command split explicit.

All executable-facing surfaces use `cbrain`: the Cargo binary target, Clap
identity, help, completions, manpage, diagnostics, onboarding, generated hooks,
installers, package main-program metadata, and current command examples.

Executable recognition distinguishes:

```rust
const CURRENT_PROGRAM: &str = "cbrain";
const STALE_MANAGED_PROGRAMS: &[&str] = &["coding-brain", "codexctl"];
```

Only `cbrain` is current. Exact old `coding-brain` and `codexctl` hook commands
may be recognized as stale managed entries for diagnosis, replacement, and
removal, but only when the complete known hook shape matches. This cleanup
recognition does not provide invocation compatibility and does not claim
modified or lookalike commands.

The raw installer must fail before writing when the old
`${INSTALL_DIR}/coding-brain` path exists. It tells the user to verify and
remove that file explicitly rather than leaving both commands or deleting a
file whose ownership cannot be proven.

## Rationale

`cbrain` is short, pronounceable, and closely tied to the product name.
Accepting its known external collision is preferable to choosing a more opaque
contraction. Keeping all non-executable namespaces unchanged confines the
breaking change to the requested surface and avoids an unrelated data or
configuration migration.

Cleanup-only recognition of old managed hooks prevents duplicate provider
definitions without restoring the old command. Exact ownership checks and the
installer preflight preserve fail-safe behavior where the rename crosses
mutable user configuration or filesystem state.

## Consequences

- Existing invocations of `coding-brain` stop working after upgrade.
- Imperatively managed hooks stop delivering activity until the user runs
  `cbrain init <provider>`; `cbrain doctor` identifies exact old definitions as
  stale.
- Home Manager rebuilds generate immutable hook commands pointing to
  `cbrain`, while the option remains `programs.coding-brain`.
- Package names and command names differ, so installation documentation and
  package tests must state and verify the resulting command.
- Current package outputs and manifests must contain only `cbrain`; tracked
  package-manager ownership and generation contracts handle replacement, while
  the raw installer uses the explicit old-path preflight.
- Users who already have the unrelated `cbrain` CLI must use separate prefixes
  or choose one installation.
- Context-specific edits and regression guards are required to keep
  `coding-brain` storage, configuration, package, repository, and crate
  namespaces unchanged.
