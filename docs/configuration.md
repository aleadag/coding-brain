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

  programs.coding-brain = {
    enable = true;
    settings.brain = {
      endpoint = "http://localhost:11434/api/generate";
      model = "gemma4:e4b";
    };
  };
}
```

The module installs its selected package, writes `coding-brain/config.toml`, and can merge eight Codex lifecycle, permission, and recovery definitions into `programs.codex.hooks`. Every command uses the package's immutable Nix store executable with explicit `--provider codex`; the Codex `Stop` definition calls `--recovery-hook --provider codex`. Unrelated Codex hooks remain independent.

The module owns Codex hooks declaratively because Home Manager can merge `programs.codex.hooks`. It does not claim ownership of Claude or Antigravity JSON. Configure those providers imperatively after activation:

```bash
coding-brain init claude antigravity
coding-brain doctor
```

This command safely merges `~/.claude/settings.json` and `~/.gemini/config/hooks.json`; no Home Manager option is required for those files.

Home Manager owns the read-only TOML settings above. Select the writable global mode separately with `coding-brain config set mode on`; an explicit mode state overrides legacy TOML mode fields without modifying the Home Manager file.

Settings rendered by Nix are world-readable in the Nix store. Do not put tokens, credentials, or token-bearing URLs in `programs.coding-brain.settings`.

`codexHooks.enable` defaults to true only when Home Manager exposes `programs.codex.hooks` and `programs.codex.enable` is true. After changing the package, rebuild Home Manager, restart Codex, and inspect `/hooks` before trusting the changed executable path. Re-run `coding-brain init claude antigravity` after replacing the package so their imperative commands use the current executable.

## Managed hooks

Imperative setup names the providers to configure:

```bash
coding-brain init codex
coding-brain init claude antigravity
coding-brain init all
```

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
