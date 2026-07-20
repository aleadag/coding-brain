# Coding Brain Crate Namespace Design

- Status: Accepted
- Date: 2026-07-20
- Bead: `codexctl-5ls`

## Context

Coding Brain now exposes `coding-brain` as its only executable and uses the
`coding-brain` namespace for configuration and state, but its publishable Rust
packages and source directories still use the former `codexctl` name. This
leaves `cargo install codexctl` as an inconsistent public installation path and
makes internal dependency names disagree with the product boundary established
by ADR-0002.

A separate codename would introduce a third vocabulary without identifying a
new architectural boundary. Names such as Dream remain appropriate for future
capabilities with their own responsibilities, not for the existing workspace
layers.

## Decision

Rename the complete Rust workspace namespace to Coding Brain while preserving
the existing three-crate dependency direction and independent publication:

| Role | Cargo package | Rust crate | Directory |
| --- | --- | --- | --- |
| Binary and runtime integration | `coding-brain` | `coding_brain` | repository root |
| Shared evidence and runtime contracts | `coding-brain-core` | `coding_brain_core` | `crates/coding-brain-core` |
| Terminal interface | `coding-brain-tui` | `coding_brain_tui` | `crates/coding-brain-tui` |

The dependency direction remains:

```text
coding-brain -> coding-brain-tui -> coding-brain-core
```

All three packages remain independently publishable on crates.io. Path
dependencies retain explicit versions so crates.io can resolve them after
stripping local paths. The executable remains `coding-brain`; no compatibility
package, executable alias, crate alias, or codename is added.

## Scope

The rename covers workspace members, package and library names, dependency
keys, Rust import paths, crate directories, standalone crate checks, packaging
metadata, installation instructions, architecture documentation, fixtures, and
lockfile package entries.

The GitHub repository name and historical changelog entries remain unchanged.
Historical release notes describe artifacts that existed at the time and are
not rewritten. No persistent data paths or runtime behavior change as part of
this work.

## Verification

The implementation is complete when:

- no active manifest, source, test, workflow, packaging, or current
  documentation reference requires the old Rust package or crate names;
- `cargo install coding-brain` is the documented crates.io installation path;
- each publishable package can be packaged independently;
- workspace formatting, tests, clippy, and build pass; and
- the final executable remains named `coding-brain`.
