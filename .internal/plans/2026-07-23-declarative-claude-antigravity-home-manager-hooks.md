# Declarative Claude and Antigravity Home Manager Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Extend `programs.coding-brain` with tested declarative Claude Code and Antigravity CLI hooks while preserving Codex compatibility and preventing silent Antigravity configuration loss.

**Architecture:** Claude hooks append through `programs.claude-code.settings.hooks`. Antigravity hooks follow a genuine enabled `programs.antigravity-cli`, but Home Manager explicitly owns the complete `~/.gemini/config/hooks.json` because the upstream module has no hooks option; unrelated named definitions come from a typed passthrough and normal file-collision checks remain enabled.

**Tech Stack:** Nix, Home Manager modules, `pkgs.formats.json`, Rust workspace verification, Markdown documentation.

## Global Constraints

- Keep `programs.coding-brain.codexHooks.enable` behavior and its trust notice unchanged.
- Every managed command uses `lib.getExe cfg.package`; no managed hook relies on `PATH`.
- Claude hooks require `programs.claude-code.settings` and an enabled Claude Code module.
- Antigravity hooks require a genuine enabled `programs.antigravity-cli`; legacy Gemini mode and a `gemini-cli` package are rejected.
- Antigravity file ownership never sets `force` and never performs an activation-time mutable JSON merge.
- `antigravityHooks.extraDefinitions` rejects the reserved `coding-brain` key and must warn that Nix-store values cannot contain secrets.
- Do not change imperative `coding-brain init` behavior or add a cross-language hook generator.
- Do not commit, push, or publish implementation changes without explicit user authorization.

---

## File structure

- Modify `nix/home-manager.nix`: provider capability detection, public options, assertions, Claude list contributions, Antigravity generated file, and provider activation notice.
- Modify `nix/tests/home-manager-module.nix`: real pinned-provider fixtures, compatibility fixtures, generated hook assertions, preservation, disable, legacy Gemini, and activation guidance.
- Modify `docs/configuration.md`: declarative provider API, ownership boundary, migration, secrets, package upgrades, and imperative/declarative separation.
- Modify `docs/troubleshooting.md`: provider-aware repair and exact rollback procedures.

### Task 1: Add tested declarative provider hooks

**Files:**

- Modify: `nix/tests/home-manager-module.nix:7`
- Modify: `nix/home-manager.nix:10`

**Interfaces:**

- Consumes: `programs.codex.hooks`, `programs.claude-code.settings`, `programs.claude-code.enable`, `programs.antigravity-cli.enable`, `programs.antigravity-cli.package`, and `programs.antigravity-cli.useLegacyGeminiConfig`.
- Produces: `programs.coding-brain.claudeHooks.enable`, `programs.coding-brain.antigravityHooks.enable`, and `programs.coding-brain.antigravityHooks.extraDefinitions`.
- Produces files/settings: merged Claude `settings.hooks` and `home.file.".gemini/config/hooks.json"`.

**Acceptance Criteria:**

- Claude hooks default on only for a supported enabled Claude module and append all eight exact managed definitions after existing hooks.
- Antigravity hooks default on only for a genuine enabled Antigravity CLI and generate all five exact managed definitions beside preserved unrelated named definitions.
- Unsupported, disabled, reserved-key, and legacy Gemini configurations fail through targeted assertions.
- Explicit provider-hook disablement removes only Coding Brain's declarative contribution.
- Both module aliases remain idempotent and existing Codex tests pass unchanged.

- [ ] **Step 1: Add real-provider test fixtures and failing default assertions**

In `configured`, enable the pinned provider modules without installing their packages, add existing Claude and Antigravity definitions, and pass the Antigravity definition through the proposed public option:

```nix
programs.claude-code = {
  enable = true;
  package = null;
  settings.hooks.Stop = [ existingStop ];
};
programs.antigravity-cli = {
  enable = true;
  package = null;
};
programs.coding-brain = {
  enable = true;
  package = testPackage;
  antigravityHooks.extraDefinitions.external = {
    enabled = false;
  };
};
```

Add these assertions before the current Codex event assertions:

```nix
assert cfg.programs.coding-brain.claudeHooks.enable;
assert cfg.programs.coding-brain.antigravityHooks.enable;
assert packageOnly.config.programs.coding-brain.claudeHooks.enable == false;
assert packageOnly.config.programs.coding-brain.antigravityHooks.enable == false;
assert builtins.length cfg.programs.claude-code.settings.hooks.Stop == 2;
assert builtins.head cfg.programs.claude-code.settings.hooks.Stop == existingStop;
assert cfg.home.file.".gemini/config/hooks.json".force == false;
```

- [ ] **Step 2: Run the Home Manager check and confirm the new public API is absent**

Run:

```bash
nix build path:.#checks.x86_64-linux.home-manager-module --no-link
```

Expected: evaluation fails because `programs.coding-brain.claudeHooks` and `programs.coding-brain.antigravityHooks` do not exist.

- [ ] **Step 3: Add provider detection, typed options, and assertions**

In the top-level `let` of `nix/home-manager.nix`, add the JSON format, capability checks, provider state, compatibility-mode calculation, and small hook constructors:

```nix
jsonFormat = pkgs.formats.json { };
hasClaudeSettings = lib.hasAttrByPath [
  "programs"
  "claude-code"
  "settings"
] options;
claudeEnabled = lib.attrByPath [
  "programs"
  "claude-code"
  "enable"
] false config;
hasAntigravityEnable = lib.hasAttrByPath [
  "programs"
  "antigravity-cli"
  "enable"
] options;
antigravityEnabled = lib.attrByPath [
  "programs"
  "antigravity-cli"
  "enable"
] false config;
antigravityPackage = lib.attrByPath [
  "programs"
  "antigravity-cli"
  "package"
] null config;
antigravityUsesLegacyGemini =
  lib.attrByPath
    [
      "programs"
      "antigravity-cli"
      "useLegacyGeminiConfig"
    ]
    false
    config
  || (antigravityPackage != null && lib.getName antigravityPackage == "gemini-cli");
mkHandler =
  command: timeout:
  {
    type = "command";
    inherit command timeout;
  };
mkNestedHook =
  command: timeout: matcher:
  {
    hooks = [ (mkHandler command timeout) ];
  }
  // lib.optionalAttrs (matcher != null) { inherit matcher; };
```

Add these public options after `codexHooks.enable`:

```nix
claudeHooks.enable = lib.mkOption {
  type = lib.types.bool;
  default = hasClaudeSettings && claudeEnabled;
  defaultText = lib.literalExpression ''
    lib.hasAttrByPath [ "programs" "claude-code" "settings" ] options
    && config.programs.claude-code.enable
  '';
  description = "Whether to merge Coding Brain hooks into programs.claude-code.settings.hooks.";
};

antigravityHooks = {
  enable = lib.mkOption {
    type = lib.types.bool;
    default = hasAntigravityEnable && antigravityEnabled && !antigravityUsesLegacyGemini;
    defaultText = lib.literalExpression ''
      lib.hasAttrByPath [ "programs" "antigravity-cli" "enable" ] options
      && config.programs.antigravity-cli.enable
      && !config.programs.antigravity-cli.useLegacyGeminiConfig
      && (
        config.programs.antigravity-cli.package == null
        || lib.getName config.programs.antigravity-cli.package != "gemini-cli"
      )
    '';
    description = "Whether Home Manager owns the complete ~/.gemini/config/hooks.json with Coding Brain's Antigravity hooks.";
  };

  extraDefinitions = lib.mkOption {
    type = lib.types.attrsOf (lib.types.attrsOf jsonFormat.type);
    default = { };
    description = ''
      Unrelated named Antigravity hook definitions preserved beside Coding Brain.
      The `coding-brain` key is reserved. Values are copied to the world-readable
      Nix store; do not put tokens, credentials, or token-bearing commands here.
    '';
  };
};
```

Extend the existing assertion list with:

```nix
{
  assertion = !cfg.claudeHooks.enable || hasClaudeSettings;
  message = "programs.coding-brain.claudeHooks.enable requires Home Manager programs.claude-code.settings; disable it or upgrade Home Manager";
}
{
  assertion = !cfg.claudeHooks.enable || !hasClaudeSettings || claudeEnabled;
  message = "programs.coding-brain.claudeHooks.enable requires programs.claude-code.enable = true";
}
{
  assertion = !cfg.antigravityHooks.enable || hasAntigravityEnable;
  message = "programs.coding-brain.antigravityHooks.enable requires Home Manager programs.antigravity-cli.enable; disable it or upgrade Home Manager";
}
{
  assertion = !cfg.antigravityHooks.enable || !hasAntigravityEnable || antigravityEnabled;
  message = "programs.coding-brain.antigravityHooks.enable requires programs.antigravity-cli.enable = true";
}
{
  assertion = !cfg.antigravityHooks.enable || !antigravityUsesLegacyGemini;
  message = "programs.coding-brain.antigravityHooks.enable requires genuine Antigravity CLI, not legacy Gemini configuration";
}
{
  assertion = !(cfg.antigravityHooks.extraDefinitions ? coding-brain);
  message = "programs.coding-brain.antigravityHooks.extraDefinitions reserves the coding-brain key";
}
```

- [ ] **Step 4: Add the eight Claude hook contributions**

Add a new `lib.mkIf cfg.claudeHooks.enable` member to the module's `lib.mkMerge`. Guard the provider option with `lib.optionalAttrs hasClaudeSettings`:

```nix
(lib.mkIf cfg.claudeHooks.enable (
  lib.optionalAttrs hasClaudeSettings {
    programs.claude-code.settings.hooks = {
      SessionStart = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 "startup|resume|clear|compact")
      ];
      UserPromptSubmit = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 null)
      ];
      PreToolUse = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 "*")
      ];
      PermissionRequest = lib.mkAfter [
        (mkNestedHook "${executable} --permission-hook --provider claude" 30 "*")
      ];
      PostToolUse = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 "*")
      ];
      SubagentStart = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 "*")
      ];
      SubagentStop = lib.mkAfter [
        (mkNestedHook "${executable} --lifecycle-hook --provider claude" 2 "*")
      ];
      Stop = lib.mkAfter [
        (mkNestedHook "${executable} --recovery-hook --provider claude" 30 null)
      ];
    };
  }
))
```

- [ ] **Step 5: Add the Antigravity file and provider activation notice**

Add the managed definition to the top-level `let`:

```nix
managedAntigravityDefinition = {
  PreToolUse = [
    (mkNestedHook
      "${executable} --permission-hook --provider antigravity --antigravity-hook-event PreToolUse"
      30
      "*")
  ];
  PostToolUse = [
    (mkNestedHook
      "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse"
      2
      "*")
  ];
  PreInvocation = [
    (mkHandler
      "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation"
      2)
  ];
  PostInvocation = [
    (mkHandler
      "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation"
      2)
  ];
  Stop = [
    (mkHandler
      "${executable} --recovery-hook --provider antigravity --antigravity-hook-event Stop"
      30)
  ];
};
```

Add these two `lib.mkMerge` members after the Claude member:

```nix
(lib.mkIf cfg.antigravityHooks.enable {
  home.file.".gemini/config/hooks.json".source = jsonFormat.generate "coding-brain-antigravity-hooks.json" (
    cfg.antigravityHooks.extraDefinitions
    // {
      coding-brain = managedAntigravityDefinition;
    }
  );
})
(lib.mkIf (cfg.claudeHooks.enable || cfg.antigravityHooks.enable) {
  home.activation.codingBrainProviderHookNotice = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
    echo "Coding Brain provider hooks use ${executable}; restart Claude Code or Antigravity CLI and run coding-brain doctor after package changes."
  '';
})
```

- [ ] **Step 6: Complete failure, preservation, rollback, and event-shape tests**

Extend `compatibilityOptions` with `home.file` and `home.activation` option stubs so assertion-only evaluations do not fail on unknown Home Manager core options:

```nix
home.file = lib.mkOption {
  type = lib.types.attrsOf lib.types.unspecified;
  default = { };
};
home.activation = lib.mkOption {
  type = lib.types.attrsOf lib.types.unspecified;
  default = { };
};
```

Remove the existing `home.activation` declaration from `codexOptions`; `compatibilityOptions` now supplies it to every synthetic evaluation.

Add provider option stubs:

```nix
claudeOptions =
  { lib, ... }:
  {
    options.programs.claude-code = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      settings = lib.mkOption {
        type = lib.types.attrsOf lib.types.unspecified;
        default = { };
      };
    };
  };
antigravityOptions =
  { lib, ... }:
  {
    options.programs.antigravity-cli = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      package = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
      };
      useLegacyGeminiConfig = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
    };
  };
```

Add a compatibility constructor and the exact failure fixtures:

```nix
evalCompatibility =
  modules: codingBrain:
  lib.evalModules {
    specialArgs = {
      inherit pkgs;
      lib = lib // home-manager.lib;
    };
    modules = [
      compatibilityOptions
      self.homeManagerModules.default
      {
        programs.coding-brain = {
          enable = true;
          package = testPackage;
        }
        // codingBrain;
      }
    ]
    ++ modules;
  };
unsupportedClaude = evalCompatibility [ ] {
  claudeHooks.enable = true;
};
disabledClaude = evalCompatibility [ claudeOptions ] {
  claudeHooks.enable = true;
};
unsupportedAntigravity = evalCompatibility [ ] {
  antigravityHooks.enable = true;
};
disabledAntigravity = evalCompatibility [ antigravityOptions ] {
  antigravityHooks.enable = true;
};
legacyAntigravity = evalCompatibility [
  antigravityOptions
  {
    programs.antigravity-cli = {
      enable = true;
      useLegacyGeminiConfig = true;
    };
  }
] {
  antigravityHooks.enable = true;
};
reservedAntigravity = evalCompatibility [ ] {
  antigravityHooks.extraDefinitions.coding-brain = { };
};
scalarAntigravity = builtins.tryEval (
  (evalCompatibility [ ] {
    antigravityHooks.extraDefinitions.invalid = "not-an-object";
  }).config.programs.coding-brain.antigravityHooks.extraDefinitions.invalid
);
failedAssertions =
  evaluated: builtins.filter (item: !item.assertion) evaluated.config.assertions;
```

Assert `failedAssertions` returns the targeted message from Step 3 for each fixture. `disabledClaude` and `disabledAntigravity` must mention the corresponding provider `enable = true`; `legacyAntigravity` must mention genuine Antigravity CLI; `reservedAntigravity` must mention the reserved key.
Assert `scalarAntigravity.success == false`, proving each passthrough definition must be a JSON object like the imperative installer requires.

For the real `configured` fixture, extract and assert all Claude events:

```nix
claudeHooks = cfg.programs.claude-code.settings.hooks;
claudeLifecycleEntries = [
  (lib.last claudeHooks.SessionStart)
  (lib.last claudeHooks.UserPromptSubmit)
  (lib.last claudeHooks.PreToolUse)
  (lib.last claudeHooks.PostToolUse)
  (lib.last claudeHooks.SubagentStart)
  (lib.last claudeHooks.SubagentStop)
];
claudePermission = lib.last claudeHooks.PermissionRequest;
claudeStop = lib.last claudeHooks.Stop;
```

Assert lifecycle commands use `--lifecycle-hook --provider claude` with timeout 2, PermissionRequest uses `--permission-hook --provider claude` with timeout 30, Stop uses `--recovery-hook --provider claude` with timeout 30, and matcher presence matches the specification.

Read the generated Antigravity JSON in the `runCommand`:

```bash
jq -e '."external".enabled == false' \
  ${cfg.home.file.".gemini/config/hooks.json".source}
jq -e --arg exe "${expectedExe}" '
  ."coding-brain" == {
    "PreToolUse": [{
      "matcher": "*",
      "hooks": [{
        "type": "command",
        "command": ($exe + " --permission-hook --provider antigravity --antigravity-hook-event PreToolUse"),
        "timeout": 30
      }]
    }],
    "PostToolUse": [{
      "matcher": "*",
      "hooks": [{
        "type": "command",
        "command": ($exe + " --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse"),
        "timeout": 2
      }]
    }],
    "PreInvocation": [{
      "type": "command",
      "command": ($exe + " --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation"),
      "timeout": 2
    }],
    "PostInvocation": [{
      "type": "command",
      "command": ($exe + " --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation"),
      "timeout": 2
    }],
    "Stop": [{
      "type": "command",
      "command": ($exe + " --recovery-hook --provider antigravity --antigravity-hook-event Stop"),
      "timeout": 30
    }]
  }
' \
  ${cfg.home.file.".gemini/config/hooks.json".source}
```

Add rollback fixtures with all three hook options false. Assert the existing Codex and Claude Stop definitions remain unchanged and `.gemini/config/hooks.json` is absent. Extend `dualAliasConfigured` with enabled package-null Claude and Antigravity modules, then assert each managed provider event occurs once.

- [ ] **Step 7: Run focused formatting and Home Manager verification**

Run:

```bash
nix fmt -- --check nix/home-manager.nix nix/tests/home-manager-module.nix
nix build path:.#checks.x86_64-linux.home-manager-module --no-link -L
```

Expected: both commands exit 0; the module check builds without creating a `result` symlink.

- [ ] **Step 8: Prepare the atomic implementation commit**

Inspect:

```bash
git diff --check
git diff -- nix/home-manager.nix nix/tests/home-manager-module.nix
```

Expected: only the provider module and its test change, with no whitespace errors.

With explicit user authorization, commit:

```bash
git add nix/home-manager.nix nix/tests/home-manager-module.nix
git commit -m "✨ feat: add declarative provider hooks (codexctl-2cz)"
```

### Task 2: Document ownership, migration, and rollback

**Files:**

- Modify: `docs/configuration.md:35`
- Modify: `docs/troubleshooting.md:11`

**Interfaces:**

- Consumes: the three provider options and behavior implemented in Task 1.
- Produces: user-facing declarative setup, Antigravity migration, repair, and rollback procedures.

**Acceptance Criteria:**

- Configuration docs distinguish merged Codex and Claude ownership from complete Antigravity file ownership.
- Migration preserves unrelated Antigravity definitions without recommending `force` or all-provider removal.
- Rollback distinguishes declarative disablement from imperative repair.
- Nix-store secret exposure and package-upgrade verification are explicit.
- Full Nix and Rust quality gates pass.

- [ ] **Step 1: Capture the stale guidance before editing**

Run:

```bash
rg -n "does not claim ownership|init claude antigravity|Re-run.*init|module does not own Claude or Antigravity" docs/configuration.md docs/troubleshooting.md
```

Expected: matches at `docs/configuration.md:53-66` and `docs/troubleshooting.md:68`.

- [ ] **Step 2: Replace the Home Manager configuration section**

Keep the existing import and Brain settings example, then add provider configuration:

```nix
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
};
```

Document these exact ownership rules in prose:

```markdown
Codex and Claude hooks merge through their Home Manager provider options, so definitions from other Nix modules remain in the generated provider settings. Antigravity has an upstream enable option but no hooks option. When `antigravityHooks.enable` is true, Home Manager owns the complete `~/.gemini/config/hooks.json`; put every unrelated top-level definition under `antigravityHooks.extraDefinitions`.

`claudeHooks.enable` and `antigravityHooks.enable` follow genuine enabled provider modules by default. Antigravity hooks stay disabled for legacy Gemini configuration. Explicit enablement produces a targeted assertion when a provider is unsupported, disabled, or incompatible.

Nix-generated TOML and JSON pass through the world-readable Nix store. Do not put tokens, credentials, token-bearing URLs, or token-bearing hook commands in `settings` or `antigravityHooks.extraDefinitions`.
```

- [ ] **Step 3: Add the Antigravity migration and package-upgrade procedure**

Add this ordered migration procedure:

```markdown
Before enabling declarative Antigravity hooks:

1. Inspect `~/.gemini/config/hooks.json`.
2. Copy every top-level definition except `coding-brain` into `antigravityHooks.extraDefinitions`.
3. Move the complete file to a timestamped backup.
4. Rebuild Home Manager.
5. Restart Antigravity CLI and run `coding-brain doctor`.

Do not set `force = true`. A Home Manager collision means the mutable file has not been migrated.
```

Replace the package-change instruction with: rebuild Home Manager, restart configured providers, inspect Codex `/hooks`, and run `coding-brain doctor`. Imperative init remains correct only for providers not managed declaratively.

- [ ] **Step 4: Rewrite troubleshooting repair and rollback guidance**

Under “Hooks are missing or stale,” first tell declarative users to rebuild Home Manager and restart the affected provider. Keep the existing imperative commands explicitly for providers not managed declaratively.

Replace the final rollback paragraph with:

```markdown
For declarative Codex or Claude hooks, disable the corresponding `programs.coding-brain` hook option and rebuild; other provider-module hooks remain. For declarative Antigravity hooks, disable `antigravityHooks.enable` and rebuild. To return Antigravity to mutable configuration, restore the migration backup, remove only its top-level `coding-brain` definition, run `coding-brain init antigravity` to install a fresh Coding Brain entry, and verify with `coding-brain doctor`. The targeted removal is required because the installer preserves a modified managed definition instead of overwriting it.

`coding-brain init --remove` is a full uninstall of all exact Coding Brain-managed provider hooks and the onboarding marker. Do not use it as a single-provider migration or rollback command.
```

- [ ] **Step 5: Verify documentation claims and formatting**

Run:

```bash
rg -n "force = true|world-readable|timestamped backup|init --remove|claudeHooks|antigravityHooks" docs/configuration.md docs/troubleshooting.md
rg -n "module does not own Claude or Antigravity|Re-run.*init claude antigravity" docs/configuration.md docs/troubleshooting.md
git diff --check
```

Expected: the first command finds the new ownership and safety guidance; the second command produces no matches; `git diff --check` exits 0.

- [ ] **Step 6: Run full repository verification**

Run:

```bash
nix fmt -- --check
nix build path:.#checks.x86_64-linux.home-manager-module --no-link -L
nix flake check path:. -L
nix develop path:. --command cargo fmt --all --check
nix develop path:. --command cargo test --workspace
nix develop path:. --command cargo clippy --workspace --all-targets -- -D warnings
nix develop path:. --command cargo build --workspace
```

Expected: every command exits 0. The Nix build uses `--no-link`, and `git status --short` shows no `result` symlink.

- [ ] **Step 7: Prepare the documentation commit**

Inspect:

```bash
git diff --check
git diff -- docs/configuration.md docs/troubleshooting.md
git status --short
```

Expected: the documentation matches Task 1 behavior and no unrelated file changed.

With explicit user authorization, commit:

```bash
git add docs/configuration.md docs/troubleshooting.md
git commit -m "📝 docs: explain declarative provider hook ownership (codexctl-2cz)"
```
