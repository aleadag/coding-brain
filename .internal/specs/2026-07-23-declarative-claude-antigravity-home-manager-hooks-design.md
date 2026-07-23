# Declarative Claude and Antigravity Home Manager hooks

> Research: [Declarative Claude and Antigravity Home Manager hooks](../research/2026-07-23-declarative-claude-antigravity-home-manager-hooks.md)

## Goal

Extend the exported Home Manager module so Claude Code and Antigravity CLI users can install Coding Brain hooks declaratively. Claude hooks must compose through Home Manager's provider module. Antigravity support must make ownership of `~/.gemini/config/hooks.json` explicit, preserve unrelated definitions supplied in Nix, and refuse silent replacement of an existing mutable file.

Existing `programs.coding-brain.codexHooks.enable` behavior remains unchanged.

## Public module API

The module adds these options:

```nix
programs.coding-brain = {
  enable = true;

  claudeHooks.enable = true;

  antigravityHooks = {
    enable = true;
    extraDefinitions.my-linter = {
      enabled = false;
    };
  };
};
```

`claudeHooks.enable` is a boolean. It defaults to true only when Home Manager exposes `programs.claude-code.settings` and `programs.claude-code.enable` is true. Package-only configurations and Home Manager revisions without that option keep it disabled.

`antigravityHooks.enable` is a boolean. It defaults to true only when Home Manager exposes `programs.antigravity-cli.enable`, that option is true, and the selected provider is genuine Antigravity CLI rather than the module's legacy Gemini compatibility mode. Enabling this option gives Home Manager ownership of the complete global hook file.

`antigravityHooks.extraDefinitions` is an attribute set of JSON objects and defaults to `{ }`. Its keys are unrelated top-level Antigravity hook names that should remain beside Coding Brain's definition. Non-object definitions fail option validation, matching Antigravity's hook-file contract. The key `coding-brain` is reserved; evaluation fails if `extraDefinitions` contains it. Values are rendered through the world-readable Nix store and must not contain credentials, tokens, or token-bearing commands.

## Claude composition

When `claudeHooks.enable` is true, the module contributes eight event groups under `programs.claude-code.settings.hooks`:

| Event | Matcher | Command | Timeout |
| --- | --- | --- | --- |
| `SessionStart` | `startup\|resume\|clear\|compact` | `--lifecycle-hook --provider claude` | 2 |
| `UserPromptSubmit` | none | `--lifecycle-hook --provider claude` | 2 |
| `PreToolUse` | `*` | `--lifecycle-hook --provider claude` | 2 |
| `PermissionRequest` | `*` | `--permission-hook --provider claude` | 30 |
| `PostToolUse` | `*` | `--lifecycle-hook --provider claude` | 2 |
| `SubagentStart` | `*` | `--lifecycle-hook --provider claude` | 2 |
| `SubagentStop` | `*` | `--lifecycle-hook --provider claude` | 2 |
| `Stop` | none | `--recovery-hook --provider claude` | 30 |

Each event list is contributed with `lib.mkAfter`, so definitions from the user's Claude Code configuration remain before Coding Brain's entry. Every command begins with `lib.getExe cfg.package`; it never relies on `PATH`.

Enabling Claude hooks without the upstream settings option produces a targeted assertion that instructs the user to disable the option or upgrade Home Manager. Enabling them while `programs.claude-code.enable` is false produces a separate assertion.

Disabling `claudeHooks.enable` removes only these list contributions. Home Manager continues to own Claude's `settings.json` through its provider module.

## Antigravity ownership

When `antigravityHooks.enable` is true, the module generates `home.file.".gemini/config/hooks.json"` from:

```nix
cfg.antigravityHooks.extraDefinitions
// {
  coding-brain = managedAntigravityDefinition;
}
```

The managed definition contains:

| Event | Matcher | Command | Timeout |
| --- | --- | --- | --- |
| `PreToolUse` | `*` | `--permission-hook --provider antigravity --antigravity-hook-event PreToolUse` | 30 |
| `PostToolUse` | `*` | `--lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse` | 2 |
| `PreInvocation` | none | `--lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation` | 2 |
| `PostInvocation` | none | `--lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation` | 2 |
| `Stop` | none | `--recovery-hook --provider antigravity --antigravity-hook-event Stop` | 30 |

Tool events use Antigravity's nested matcher and `hooks` array. Invocation and Stop events contain handler arrays directly. Commands use the selected package's immutable executable.

The generated file does not set Home Manager's `force` option. If `~/.gemini/config/hooks.json` already exists outside the current Home Manager generation, activation stops on the normal file collision instead of replacing it. Users must move all unrelated definitions they want to retain into `extraDefinitions` before Home Manager takes ownership.

Disabling `antigravityHooks.enable` removes the generated symlink on the next activation. It does not reconstruct the pre-migration mutable file. The user's Nix configuration remains the rollback source for unrelated definitions.

Enabling Antigravity hooks without `programs.antigravity-cli.enable` produces a targeted assertion that instructs the user to disable the option or upgrade Home Manager. Enabling them while Antigravity CLI is disabled, `useLegacyGeminiConfig` is true, or the selected package is `gemini-cli` produces a separate assertion. The legacy Gemini path does not prove support for Antigravity's hook contract.

## Imperative migration

Users migrating from `coding-brain init antigravity` follow an explicit one-time sequence:

1. Inspect `~/.gemini/config/hooks.json`.
2. Copy every top-level definition except `coding-brain` into `antigravityHooks.extraDefinitions`.
3. Move the complete mutable file to a timestamped backup. The old Coding Brain entry remains inert in that backup.
4. Enable the declarative provider options and rebuild Home Manager.
5. Run `coding-brain doctor`.

Documentation must not recommend `force = true`. A collision means migration is incomplete, not that Home Manager should discard the file.

Rollback is also explicit: disable `antigravityHooks.enable` and rebuild, restore the backup if returning to mutable configuration, remove only its top-level `coding-brain` definition, run `coding-brain init antigravity` to install a fresh imperative managed entry, and verify with `coding-brain doctor`. Removing that one entry is necessary because the installer intentionally preserves a modified managed definition instead of overwriting it. Home Manager does not restore backups automatically. `coding-brain init --remove` remains an optional full uninstall, not an Antigravity migration step, because it removes all managed provider hooks and the onboarding marker.

Claude users do not need a whole-file migration when `programs.claude-code` already manages `settings.json`; enabling `claudeHooks` adds list contributions through the same module. Users with an unmanaged `~/.claude/settings.json` must follow Home Manager's normal file-collision migration before enabling the upstream Claude Code module.

## Activation guidance

The current Codex trust notice remains. When Claude or Antigravity hooks are enabled, activation also prints a concise reminder to restart the affected provider and run `coding-brain doctor` after package changes. The notice does not mutate provider files or claim that hook delivery is healthy.

## Security and failure behavior

- All hook commands use the immutable executable selected by `programs.coding-brain.package`.
- The module never runs `coding-brain init` during activation.
- Antigravity ownership follows a genuine enabled Antigravity CLI by default, but remains disabled for absent, disabled, or legacy Gemini provider configurations.
- `extraDefinitions.coding-brain` is rejected so a user definition cannot be silently replaced by the managed definition.
- `extraDefinitions` must not contain secrets because its generated JSON passes through the Nix store.
- Existing provider permission semantics remain unchanged: Coding Brain's hook response is bounded by its current mode and deterministic safety checks.
- Non-secret settings remain the only values suitable for Nix-generated files because store paths are world-readable.

## Tests

Extend `nix/tests/home-manager-module.nix` and the existing flake check.

Claude coverage:

- default enabled when `programs.claude-code.settings` exists and Claude is enabled;
- default disabled for package-only, enable-only, and unsupported provider surfaces;
- explicit enable fails when the settings option is unavailable or Claude is disabled;
- all eight events use the expected matcher, timeout, provider argument, and immutable executable;
- an existing hook remains before Coding Brain's entry;
- explicit disable leaves the existing hook unchanged.

Antigravity coverage:

- default enabled for genuine enabled `programs.antigravity-cli`;
- default disabled for absent, disabled, and legacy Gemini provider configurations;
- explicit enable fails for unsupported, disabled, or legacy Gemini configurations;
- explicit enable generates the complete named definition;
- `extraDefinitions` remain semantically equal after JSON generation;
- non-object `extraDefinitions` fail option validation;
- `extraDefinitions.coding-brain` fails evaluation;
- the generated file does not use `force`;
- explicit disable generates no Antigravity hook file;
- commands, direct-versus-nested event shapes, and timeouts match the imperative installer.

Compatibility coverage:

- existing Codex defaults, assertions, ordering, commands, and rollback tests continue to pass;
- importing both module aliases does not duplicate any provider hook;
- the real activation package contains the provider restart and doctor guidance.
- a pinned evaluation proves `lib.mkAfter` keeps an existing Claude hook before Coding Brain's hook.

Run `nix fmt -- --check`, the Home Manager module check with `--no-link`, `nix flake check`, and the repository's Rust formatting, test, Clippy, and build gates. The Nix-only change should not require Rust behavior changes unless verification exposes a mismatch between declarative definitions and current hook recognition.

## Documentation

Update `docs/configuration.md` with the three provider options, ownership table, Antigravity migration sequence, and package-upgrade behavior. Update `docs/troubleshooting.md` so rollback instructions distinguish merged Claude definitions from the Home Manager-owned Antigravity file. Remove guidance that declarative users must rerun imperative Claude or Antigravity setup after every package change.

## Non-goals

- Add an Antigravity Home Manager provider module.
- Speculate about a future `programs.antigravity-cli.hooks` schema. If Home Manager adds one, it should replace direct file ownership rather than coexist with it.
- Merge mutable JSON during activation.
- Import an existing home-directory file during Nix evaluation.
- Manage project-local Claude or Antigravity hooks.
- Change imperative `coding-brain init` behavior.
- Change Codex hook defaults or trust behavior.

## Acceptance criteria

- Claude hooks merge through `programs.claude-code.settings.hooks` without deleting existing definitions.
- Antigravity ownership follows genuine provider enablement, preserves explicitly declared unrelated definitions, reserves the managed key, and refuses silent collision replacement.
- Every provider command uses the selected immutable executable and the current provider-specific event schema.
- Disabling provider hooks cleanly removes only their declarative contribution.
- Existing Codex behavior remains compatible.
- Tests cover defaults, unsupported or disabled providers, event shapes, paths, preservation, and rollback.
- Configuration and troubleshooting documentation explain ownership, migration, verification, and rollback.

## Stress Test Results: declarative Claude and Antigravity Home Manager hooks

### Resolved Decisions

- Antigravity retains explicit whole-file ownership with normal Home Manager collision refusal; activation-time merging and `force` remain rejected.
- `extraDefinitions` is an attribute set of JSON objects, rejects scalar definitions, reserves `coding-brain`, and carries an explicit Nix-store secret warning.
- Claude event lists use `lib.mkAfter`; evaluation against the pinned Home Manager revision confirmed existing definitions remain before the managed definition.
- Both provider defaults follow available upstream enable options. Antigravity additionally rejects legacy Gemini compatibility mode because it does not prove the Antigravity hook contract.
- Migration moves the Antigravity file to a backup instead of invoking the all-provider `init --remove`.
- Rollback is user-directed: disable, rebuild, restore the backup if needed, remove only its stale `coding-brain` definition, refresh imperative setup, and run Doctor.
- Future upstream Antigravity hooks support should replace direct file ownership rather than coexist with it.
- Tests compare the complete managed Antigravity definition semantically and cover every Claude event shape and command plus defaults, compatibility failures, preservation, rollback, and dual-alias idempotence.

### Changes Made

- Corrected the original assumption that Home Manager lacks an Antigravity enable option.
- Added the legacy Gemini compatibility guard.
- Added secret-exposure requirements for declarative passthrough definitions.
- Narrowed migration so it does not remove unrelated provider setup.
- Made rollback and pinned list-merge verification explicit.
- Matched passthrough typing to the installer's object-only Antigravity contract and required a scalar-rejection test.
- Forced the scalar child in `builtins.tryEval` so Nix laziness cannot turn the rejection test into a false positive.
- Replaced partial Antigravity field checks with complete semantic object comparison.
- Routed Rust verification through the locked Nix development shell.
- Hardened rollback for modified managed definitions that the installer intentionally preserves.

### Deferred / Parking Lot

- Adopting a future upstream `programs.antigravity-cli.hooks` option after its schema and merge behavior exist.

### Confidence Assessment

- Overall: High.
- Areas of concern: Antigravity still requires whole-file ownership until Home Manager exposes a hook composition surface; collision refusal and explicit migration contain that risk.
