# Research: Declarative Claude and Antigravity Home Manager hooks

> **Date:** 2026-07-23
> **Bead:** codexctl-hl8
> **Status:** Complete

## Summary

The pinned Home Manager revision exposes Claude Code's complete `settings.json` as the mergeable `programs.claude-code.settings` option, so Coding Brain can append provider hooks without owning Claude's file directly. It also exposes `programs.antigravity-cli.enable`, but no Antigravity hooks option: the global `hooks.json` is a top-level map of named definitions, so safe declarative support must either own that whole file explicitly or remain imperative. The recommended design merges Claude hooks through the upstream option and conditionally enables collision-safe Antigravity whole-file ownership for genuine Antigravity CLI configurations.

## Key Findings

### Claude hooks have an upstream composition surface

> **Confidence:** high — verified against the exact Home Manager revision pinned by `flake.lock`, its tests, and Claude's official hook reference.

Home Manager revision `165228b0efefc3e635e5174020c40ea64271dc25` defines `programs.claude-code.settings` with the JSON format type and demonstrates `settings.hooks` in the option example. Its implementation generates Claude's `settings.json` from the merged value. Coding Brain can therefore contribute each event list with `lib.mkAfter`, preserving hook definitions from other Nix modules rather than generating a competing file. [S1][S2]

Claude's official hook reference confirms the required events and matcher semantics, including `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `SubagentStart`, `SubagentStop`, and `Stop`. [S3]

### Antigravity's global file is a named-definition map

> **Confidence:** high — the official provider schema matches the current Rust installer and its tests.

Antigravity configures hooks in a global or workspace `hooks.json`. The document maps arbitrary names to event definitions; each definition may contain `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, and `Stop`. [S4]

The current installer already uses the top-level name `coding-brain` and preserves every other top-level entry. A declarative file must retain that namespace boundary but cannot merge with an existing mutable JSON document at Home Manager evaluation time. [S5]

### Home Manager collision checks provide a safe migration boundary

> **Confidence:** high — verified in Home Manager's official collision guidance and current generated-file behavior.

Home Manager normally refuses to replace a colliding unmanaged file. Its documentation tells users to inspect the existing path, move desired settings into Home Manager configuration, then move or remove the unmanaged file; `force = true` bypasses that protection and can silently delete local changes. [S6]

Therefore Antigravity support should not set `force`. It should require explicit enablement, expose an attrset for unrelated named definitions, reject a user-supplied `coding-brain` key, and generate the complete file as `extraDefinitions // { coding-brain = managedDefinition; }`. Existing mutable files then block activation until the user deliberately migrates them, and disabling the option removes only Home Manager's symlink on the next activation.

## Comparisons

| Criterion | Upstream provider option | Explicit whole-file ownership | Activation-time mutable merge |
|-----------|--------------------------|-------------------------------|-------------------------------|
| Claude | Best fit; upstream owns `settings.json` | Competes with upstream module | Mutates behind Home Manager |
| Antigravity | No available global option | Viable with opt-in and collision checks | Preserves runtime entries but creates mixed ownership |
| Unrelated hooks | Nix list merge | Preserved when declared as passthrough settings | Preserved by runtime merge |
| Rollback | Remove module contribution | Disable option; generated symlink disappears | Requires another mutation |
| Recommendation | Use for Claude | Use for Antigravity | Reject |

## Codebase Context

- `nix/home-manager.nix` currently detects and merges only `programs.codex.hooks`.
- `src/init/provider_hooks/claude.rs` defines the eight Claude event groups and commands.
- `src/init/provider_hooks/antigravity.rs` defines the named `coding-brain` Antigravity object and preserves unrelated top-level definitions.
- `nix/tests/home-manager-module.nix` already covers defaults, unavailable provider options, ordering, absolute executable paths, and rollback for Codex.
- `docs/configuration.md` and `docs/troubleshooting.md` currently direct Claude and Antigravity users to imperative initialization and must be revised.

## Recommendations

1. Add `claudeHooks.enable`, defaulting to true only when `programs.claude-code.settings` exists and `programs.claude-code.enable` is true. Append the same eight event groups used by the imperative installer with the selected package's immutable executable.
2. Add `antigravityHooks.enable`, defaulting to true only when `programs.antigravity-cli.enable` exists, is true, and the module is not using its legacy Gemini compatibility mode.
3. Add `antigravityHooks.extraDefinitions`, typed as an attribute set of JSON objects and defaulting to `{ }`. Generate `home.file.".gemini/config/hooks.json"` without `force`, merge the managed `coding-brain` definition after the passthrough definitions, and assert that passthrough cannot define `coding-brain`.
4. Document a one-time migration: copy unrelated top-level entries into `extraDefinitions`, move the mutable file to a timestamped backup, rebuild, and verify with `coding-brain doctor`.
5. Extend the existing Home Manager check rather than adding a second test harness.

## Open Questions

- Public option naming (`extraDefinitions` versus `settings`) is a design choice; `extraDefinitions` states that the values are top-level Antigravity hook definitions rather than arbitrary provider settings.

## Refuted / Discarded Claims

- Discarded: Home Manager can merge declarative content with the existing mutable Antigravity file during evaluation. Nix evaluation cannot safely depend on mutable home-directory JSON, and an activation merge would create competing ownership.
- Discarded: `force = true` is a convenient migration mechanism. It bypasses the collision protection that prevents silent loss of unrelated hooks.

## Sources

- [Pinned Home Manager Claude Code options](https://github.com/nix-community/home-manager/blob/165228b0efefc3e635e5174020c40ea64271dc25/modules/programs/claude-code/options.nix) — Primary/Official — 2026-07-23 — `settings` JSON type and hook example. [S1]
- [Pinned Home Manager Claude Code implementation](https://github.com/nix-community/home-manager/blob/165228b0efefc3e635e5174020c40ea64271dc25/modules/programs/claude-code/default.nix) — Primary/Official — 2026-07-23 — generation of `settings.json` from merged settings. [S2]
- [Claude Code hooks reference](https://code.claude.com/docs/en/hooks) — Primary/Official — 2026-07-23 — event, matcher, input, and decision contracts. [S3]
- [Antigravity hooks](https://www.antigravity.google/docs/hooks) — Primary/Official — 2026-07-23 — named definition map, event list, and handler schema. [S4]
- `src/init/provider_hooks/antigravity.rs` — Primary/Codebase — 2026-07-23 — current managed name and preservation behavior. [S5]
- [Home Manager file-collision guidance](https://github.com/nix-community/home-manager/blob/master/docs/manual/usage/dotfiles.md) — Primary/Official — 2026-07-23 — collision migration and risks of forced replacement. [S6]
