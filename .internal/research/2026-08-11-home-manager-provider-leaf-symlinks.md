# Research: Home Manager provider leaf symlinks

> **Date:** 2026-08-11
> **Bead:** codexctl-5n458
> **Status:** Complete

## Summary

The installed provider files use a valid two-hop Home Manager topology: each global file points into one `*-home-manager-files` generation, whose expected leaf is itself an absolute symlink to another immutable Nix-store object. Current Doctor inspection deliberately rejects that leaf symlink before reading JSON, while the packaged regression fixture copies regular leaves and therefore does not reproduce production. The narrow repair is to accept one validated store-to-store leaf indirection during read-only Home Manager inspection, retain all existing ancestor and project-scope rejection rules, and make the Nix fixture match the real topology.

## Key Findings

### Current inspection rejects the installed leaf topology

> **Confidence:** high — source, live metadata, and an independent fresh-context verification agree.

`read_provider_file_for_inspection` accepts only a global link with the exact provider suffix under `/nix/store/*-home-manager-files`, then checks the expected leaf with `symlink_metadata`. A symlink leaf returns `home_manager` / `invalid` / `unsupported_topology` before JSON comparison (`src/init/provider_hooks/mod.rs:446-552`). [S1]

All three live global paths have that rejected shape:

- `~/.codex/hooks.json` -> `...-home-manager-files/.codex/hooks.json` -> `/nix/store/...-codex-hooks`
- `~/.claude/settings.json` -> `...-home-manager-files/.claude/settings.json` -> `/nix/store/...-claude-code-settings-directory/settings.json`
- `~/.gemini/config/hooks.json` -> `...-home-manager-files/.gemini/config/hooks.json` -> `/nix/store/...-coding-brain-antigravity-hooks.json`

Each second target is a regular file after following the leaf link. The existing `NestedSymlink` fixture encodes the current rejection (`src/init/provider_hooks/mod.rs:3055-3207`). [S2]

### The failure is not a package-generation mismatch

> **Confidence:** high — the installed executable and every managed provider command resolve to the same immutable package path.

`cbrain --version` reports `0.59.1`; `command -v cbrain` resolves to `/nix/store/1d78r3nc507nrd5glyjp2a1pzlw328jp-coding-brain-0.59.1/bin/cbrain`. Managed Codex, Claude, and Antigravity command strings all use that exact executable. The Home Manager module renders commands from `lib.getExe cfg.package` (`nix/home-manager.nix:56`). [S3]

### Existing Nix coverage is not production-shaped

> **Confidence:** high — fixture construction is explicit.

`providerHomeManagerFiles` copies provider JSON into regular generation leaves (`nix/tests/home-manager-doctor-fixtures.nix:42-48`). The VM then links the simulated home paths to those regular leaves (`nix/tests/storage-security-vm.nix:49-83,119-182`). This proves direct leaves but cannot catch Home Manager's current store-to-store leaf indirection. [S4]

### Project conflicts remain separate and fail closed

> **Confidence:** high — current filesystem types and aggregation behavior are directly observable in source.

Codex and Claude inspect global plus applicable project scopes; Antigravity is global-only (`src/init/provider_hooks/mod.rs:288-312`). Any invalid file wins aggregate state and `Unsupported` wins aggregate ownership (`src/init/provider_hooks/mod.rs:313-338,394-404`). Project-scope symlinks remain unsupported by design, while a regular project definition beside a valid Home Manager global definition becomes `Duplicate` / `Mixed` and receives duplicate-scope remediation.

The reported `nix-configs` project currently has a zero-byte regular `.codex` path, so `.codex/hooks.json` fails metadata lookup through a non-directory ancestor. The generic lookup-error branch reports `unsupported` / `unreadable` (`src/init/provider_hooks/mod.rs:452-462`); classifying `NotADirectory` as `unsupported_topology` would describe the actual defect without weakening inspection. Its `.claude/settings.json` is a regular imperative JSON file, so after the global leaf regression is repaired it should remain an explicit mixed duplicate rather than being silently accepted or removed.

## Comparisons

| Approach | Production match | Security boundary | Scope |
|---|---|---|---|
| Validate one absolute store-to-store leaf link | Exact | Preserves ancestor, suffix, project, and regular-file checks | Narrow |
| Canonicalize arbitrary chains | Broad | Hides intermediate topology and weakens fail-closed evidence | Too permissive |
| Force copied regular leaves in Nix rendering | Does not repair existing valid generations | Avoids reader change but depends on renderer internals | Wrong layer |

## Codebase Context

The predecessor `codexctl-z6l0` intentionally separated read-only Home Manager inspection from imperative mutation. Mutation still uses `read_managed_file`, which rejects every symlink (`src/init/provider_hooks/mod.rs:1172-1194`), and must remain unchanged. Its design also intentionally rejects project-scope symlinks and lets invalid project evidence dominate a valid global definition.

## Recommendations

1. In read-only inspection only, permit exactly one additional leaf symlink when its raw target is an absolute, lexically clean path under the same Nix store; validate every target parent with `symlink_metadata`, require a bounded regular final file, and retain the opened-file metadata check.
2. Keep generation and suffix directories non-symlink, reject relative/non-store/malformed/broken/multi-hop targets, and keep all project-scope symlinks unsupported.
3. Classify a project path blocked by a non-directory ancestor as `unsupported_topology`, retaining the exact path/scope evidence and existing unsupported-path remediation.
4. Change the packaged Home Manager fixture to create real store symlink leaves for all three providers; cover malformed and unsafe inner targets plus the real Codex non-directory project conflict and Claude regular duplicate.
5. Do not modify imperative init, removal, transaction, rollback, or recovery link handling.

## Open Questions

None material. A direct live `cbrain doctor --json` run in this sandbox is blocked earlier by unrelated foreign-owned SQLite ancestor validation; source, focused tests, live `lstat`/`readlink`, and independent verification still establish the provider root cause.

## Sources

- [S1] `src/init/provider_hooks/mod.rs:446-599` — current read-only topology validator and bounded reader.
- [S2] `src/init/provider_hooks/mod.rs:3055-3207` — current nested-leaf rejection regression; focused test passed on 2026-08-11.
- [S3] installed `cbrain --version`, `readlink -f`, and managed JSON command-path inspection on 2026-08-11; `nix/home-manager.nix:56`.
- [S4] `nix/tests/home-manager-doctor-fixtures.nix:35-55` and `nix/tests/storage-security-vm.nix:49-182` — packaged fixture construction and Doctor assertions.
- Beads `codexctl-5n458` and `codexctl-z6l0` — production evidence and prior security contract.
