# Configuration

Coding Brain merges configuration in this order, with later values winning:

1. user config at `$XDG_CONFIG_HOME/coding-brain/config.toml`
2. project config at `.coding-brain.toml`
3. CLI flags

On a typical Linux system, the user file is `~/.config/coding-brain/config.toml`. Inspect effective values with `coding-brain config show`, print a template with `coding-brain config template`, and validate known config files with `coding-brain config validate`.

Project config may tune model behavior, but it cannot select `brain.endpoint`. Endpoint choice is restricted to user config or an explicit CLI flag because it determines where transcript context is sent.

## Brain settings

```toml
theme = "dark"

[brain]
endpoint = "http://localhost:11434/api/generate"
model = "gemma4:e4b"
timeout_ms = 5000
max_context_tokens = 4000
few_shot_count = 5
test_runners = ["cargo test", "npm test", "pytest", "go test", "bun test"]
```

Brain mode is separate from TOML configuration. Set it with `coding-brain config set mode off|on|auto` and inspect it with `coding-brain config get mode`. The setting is global, persists under `$XDG_STATE_HOME/coding-brain/`, and takes effect after the settings command exits. New installs default to `off`; `on` enables advisory model evaluation, while `auto` permits high-confidence automatic decisions.

`off` disables model evaluation, not the safety system. Deterministic safety checks and lifecycle recording remain active in all three modes. Existing `brain.enabled` and `brain.auto` values are read only as a compatibility fallback when no explicit mode state exists; new templates and managed configuration do not emit them.

Loopback endpoints keep model requests on the machine. Coding Brain warns when an endpoint is not loopback and gives plaintext remote HTTP a stronger warning. These advisories do not override an endpoint the user selected in CLI or user config.

## Home Manager

Import the module from the `codexctl` flake input, then configure the public `programs.coding-brain` option:

```nix
{
  imports = [ inputs.codexctl.homeManagerModules.default ];

  programs.claude-code = {
    enable = true;
  };
  programs.antigravity-cli = {
    enable = true;
  };
  programs.coding-brain = {
    enable = true;
    claudeHooks.enable = true;
    antigravityHooks = {
      enable = true;
      extraDefinitions.my-linter.enabled = false;
    };
    settings.brain = {
      endpoint = "http://localhost:11434/api/generate";
      model = "gemma4:e4b";
    };
  };
}
```

The module installs its selected package, writes `coding-brain/config.toml`, and configures provider hooks with immutable Nix store executable paths. Codex and Claude hooks merge through their Home Manager provider options, so definitions from other Nix modules remain in the generated provider settings.

Antigravity has an upstream enable option but no hooks option. When `antigravityHooks.enable` is true, Home Manager owns the complete `~/.gemini/config/hooks.json`; put every unrelated top-level definition under `antigravityHooks.extraDefinitions`. Each extra definition must be an object, not a scalar JSON value.

`claudeHooks.enable` and `antigravityHooks.enable` follow genuine enabled provider modules by default. Antigravity hooks stay disabled for legacy Gemini configuration. Explicit enablement produces a targeted assertion when a provider is unsupported, disabled, or incompatible.

Before enabling declarative Antigravity hooks:

1. Inspect `~/.gemini/config/hooks.json`.
2. Copy every top-level definition except `coding-brain` into `antigravityHooks.extraDefinitions`.
3. Move the complete file to a timestamped backup.
4. Rebuild Home Manager.
5. Restart Antigravity CLI and run `coding-brain doctor`.

Do not set `force = true`. A Home Manager collision means the mutable file has not been migrated.

Home Manager owns the read-only TOML settings above. Select the writable global mode separately with `coding-brain config set mode on`; an explicit mode state overrides legacy TOML mode fields without modifying the Home Manager file.

Nix-generated TOML and JSON pass through the world-readable Nix store. Do not put tokens, credentials, token-bearing URLs, or token-bearing hook commands in `settings` or `antigravityHooks.extraDefinitions`.

After changing the package, rebuild Home Manager, restart every configured provider, inspect Codex `/hooks`, and run `coding-brain doctor`. Run imperative init after a package change only for providers that are not managed declaratively.

## Managed hooks

Imperative setup names the providers to configure:

```bash
coding-brain init codex
coding-brain init claude antigravity
coding-brain init all
```

Use these commands only for providers that are not managed declaratively through Home Manager.

`--plugin-only` remains a deprecated Codex-only alias for one compatibility release. Codex and Claude receive lifecycle handlers for `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `SubagentStart`, `SubagentStop`, and `Stop`, plus a `PermissionRequest` handler. Antigravity receives `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, and `Stop` definitions.

Init parses and stages the complete selected-provider change set before atomically replacing each file. It preserves unrelated settings, disabled entries, and modified former managed entries. If replacement is interrupted, evidence-based recovery restores or completes only files whose recorded hashes still match, so concurrent user edits are not overwritten.

Hook activity is bounded status evidence, not authorization by itself. Permission decisions still pass through deterministic rules and, when enabled, the Brain evaluator. Transcript discovery supplies richer evidence when the rollout catches up.

## Project identity

Coding Brain first checks the Git project root for `.coding-brain/project.toml`. Without that explicit override, it derives a stable identity from a canonical network `origin`; if the origin is missing, local, `file:`, or otherwise unusable, it falls back to a path-derived temporary identity.

For a normal Git clone with a usable network origin, `coding-brain init` is optional for identity. Run it when you want an explicit manifest override or need stable identity despite an unusable origin; imperative setup also uses init to install managed hooks.

## Paths

| Data | Path |
| --- | --- |
| User config | `$XDG_CONFIG_HOME/coding-brain/config.toml` |
| User state | `$XDG_STATE_HOME/coding-brain/` |
| Lifecycle snapshot | `$XDG_STATE_HOME/coding-brain/hooks/lifecycle.json` |
| Brain prompts | `$XDG_STATE_HOME/coding-brain/brain/prompts/` |
| Project config | `.coding-brain.toml` |
| Project identity | `.coding-brain/project.toml` |
| Codex managed hooks | project `.codex/hooks.json` or user `~/.codex/hooks.json` |
| Claude managed hooks | `~/.claude/settings.json` |
| Antigravity managed hooks | `~/.gemini/config/hooks.json` |

If `XDG_STATE_HOME` is unset, Coding Brain uses `~/.local/state`. Removing `.coding-brain/project.toml` and rerunning init deliberately creates a new project identity; use that only when a fork should learn independently.
