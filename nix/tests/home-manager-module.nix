{
  home-manager,
  pkgs,
  self,
}:

let
  inherit (pkgs) lib;
  testPackage = pkgs.writeShellScriptBin "coding-brain" "exit 0";
  legacyGeminiPackage = pkgs.writeShellScriptBin "gemini-cli" "exit 0";
  expectedExe = lib.getExe testPackage;
  existingStop = {
    hooks = [
      {
        type = "command";
        command = "echo existing-stop";
      }
    ];
  };
  baseHome = {
    home.username = "codexctl-test";
    home.homeDirectory = "/home/codexctl-test";
    home.stateVersion = "25.11";
  };
  configured = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
      baseHome
      {
        programs.codex.enable = true;
        programs.codex.hooks.Stop = [ existingStop ];
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
          settings.theme = "dark";
          settings.brain = {
            endpoint = "http://localhost:11434/api/generate";
            model = "gemma4:e4b";
            timeout_ms = 25000;
          };
        };
      }
    ];
  };
  aliasConfigured = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeModules.default
      baseHome
      {
        programs.coding-brain = {
          enable = true;
          package = testPackage;
          codexHooks.enable = false;
        };
      }
    ];
  };
  rollbackConfigured = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
      baseHome
      {
        programs.codex.enable = true;
        programs.codex.hooks.Stop = [ existingStop ];
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
          codexHooks.enable = false;
          claudeHooks.enable = false;
          antigravityHooks.enable = false;
        };
      }
    ];
  };
  dualAliasConfigured = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
      self.homeModules.default
      baseHome
      {
        programs.codex.enable = true;
        programs.claude-code = {
          enable = true;
          package = null;
        };
        programs.antigravity-cli = {
          enable = true;
          package = null;
        };
        programs.coding-brain = {
          enable = true;
          package = testPackage;
        };
      }
    ];
  };
  compatibilityOptions =
    { lib, ... }:
    {
      options = {
        assertions = lib.mkOption {
          type = lib.types.listOf lib.types.unspecified;
          default = [ ];
        };
        home.packages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
        };
        home.file = lib.mkOption {
          type = lib.types.attrsOf lib.types.unspecified;
          default = { };
        };
        home.activation = lib.mkOption {
          type = lib.types.attrsOf lib.types.unspecified;
          default = { };
        };
        xdg.configFile = lib.mkOption {
          type = lib.types.attrsOf lib.types.unspecified;
          default = { };
        };
      };
    };
  codexOptions =
    { lib, ... }:
    {
      options = {
        programs.codex = {
          enable = lib.mkOption {
            type = lib.types.bool;
            default = false;
          };
          hooks = lib.mkOption {
            type = lib.types.attrsOf lib.types.unspecified;
            default = { };
          };
        };
      };
    };
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
  claudeSettingsOnlyOptions =
    { lib, ... }:
    {
      options.programs.claude-code.settings = lib.mkOption {
        type = lib.types.attrsOf lib.types.unspecified;
        default = { };
      };
    };
  claudeEnableOnlyOptions =
    { lib, ... }:
    {
      options.programs.claude-code.enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
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
  antigravityEnableOnlyOptions =
    { lib, ... }:
    {
      options.programs.antigravity-cli.enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
    };
  antigravityPackageOnlyOptions =
    { lib, ... }:
    {
      options.programs.antigravity-cli.package = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
      };
    };
  codexEnableOnlyOptions =
    { lib, ... }:
    {
      options.programs.codex.enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
    };
  packageOnly = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      compatibilityOptions
      self.homeManagerModules.default
      { programs.coding-brain.enable = true; }
    ];
  };
  unsupportedHooks = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      compatibilityOptions
      self.homeManagerModules.default
      {
        programs.coding-brain = {
          enable = true;
          codexHooks.enable = true;
        };
      }
    ];
  };
  enableOnlyCodex = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      compatibilityOptions
      codexEnableOnlyOptions
      self.homeManagerModules.default
      {
        programs.codex.enable = true;
        programs.coding-brain.enable = true;
      }
    ];
  };
  disabledCodex = lib.evalModules {
    specialArgs = {
      inherit pkgs;
      lib = lib // home-manager.lib;
    };
    modules = [
      compatibilityOptions
      codexOptions
      self.homeManagerModules.default
      {
        programs.codex.enable = false;
        programs.coding-brain = {
          enable = true;
          package = testPackage;
          codexHooks.enable = true;
        };
      }
    ];
  };
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
  settingsOnlyClaudeDefault = evalCompatibility [ claudeSettingsOnlyOptions ] { };
  enableOnlyClaudeDefault = evalCompatibility [
    claudeEnableOnlyOptions
    { programs.claude-code.enable = true; }
  ] { };
  disabledClaudeDefault = evalCompatibility [
    claudeOptions
    { programs.claude-code.enable = false; }
  ] { };
  unsupportedAntigravity = evalCompatibility [ ] {
    antigravityHooks.enable = true;
  };
  disabledAntigravity = evalCompatibility [ antigravityOptions ] {
    antigravityHooks.enable = true;
  };
  enableOnlyAntigravityDefault = evalCompatibility [ antigravityEnableOnlyOptions ] { };
  packageOnlyAntigravityDefault = evalCompatibility [ antigravityPackageOnlyOptions ] { };
  disabledAntigravityDefault = evalCompatibility [
    antigravityOptions
    { programs.antigravity-cli.enable = false; }
  ] { };
  legacyAntigravity =
    evalCompatibility
      [
        antigravityOptions
        {
          programs.antigravity-cli = {
            enable = true;
            useLegacyGeminiConfig = true;
          };
        }
      ]
      {
        antigravityHooks.enable = true;
      };
  legacyPackageAntigravityDefault = evalCompatibility [
    antigravityOptions
    {
      programs.antigravity-cli = {
        enable = true;
        package = legacyGeminiPackage;
      };
    }
  ] { };
  legacyPackageAntigravityForced =
    evalCompatibility
      [
        antigravityOptions
        {
          programs.antigravity-cli = {
            enable = true;
            package = legacyGeminiPackage;
          };
        }
      ]
      {
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
  failedAssertions = evaluated: builtins.filter (item: !item.assertion) evaluated.config.assertions;
  cfg = configured.config;
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
  sessionStart = lib.last cfg.programs.codex.hooks.SessionStart;
  userPromptSubmit = lib.last cfg.programs.codex.hooks.UserPromptSubmit;
  preToolUse = lib.last cfg.programs.codex.hooks.PreToolUse;
  permission = lib.last cfg.programs.codex.hooks.PermissionRequest;
  postToolUse = lib.last cfg.programs.codex.hooks.PostToolUse;
  subagentStart = lib.last cfg.programs.codex.hooks.SubagentStart;
  subagentStop = lib.last cfg.programs.codex.hooks.SubagentStop;
  permissionHandler = builtins.elemAt permission.hooks 0;
  stopHooks = cfg.programs.codex.hooks.Stop;
  managedStop = lib.last stopHooks;
  lifecycleEntries = [
    sessionStart
    userPromptSubmit
    preToolUse
    postToolUse
    subagentStart
    subagentStop
  ];
  trustNotice = cfg.home.activation.codingBrainHookTrustNotice.data;
  unsupportedFailures = builtins.filter (item: !item.assertion) unsupportedHooks.config.assertions;
  enableOnlyFailures = builtins.filter (item: !item.assertion) enableOnlyCodex.config.assertions;
  disabledFailures = builtins.filter (item: !item.assertion) disabledCodex.config.assertions;
in
assert builtins.elem testPackage cfg.home.packages;
assert aliasConfigured.config.programs.coding-brain.enable;
assert cfg.programs.coding-brain.claudeHooks.enable;
assert cfg.programs.coding-brain.antigravityHooks.enable;
assert packageOnly.config.programs.coding-brain.claudeHooks.enable == false;
assert packageOnly.config.programs.coding-brain.antigravityHooks.enable == false;
assert builtins.length cfg.programs.claude-code.settings.hooks.Stop == 2;
assert builtins.head cfg.programs.claude-code.settings.hooks.Stop == existingStop;
assert cfg.home.file.".gemini/config/hooks.json".force == false;
assert builtins.length dualAliasConfigured.config.programs.codex.hooks.PermissionRequest == 1;
assert dualAliasConfigured.config.programs.codex.hooks ? PostToolUse;
assert dualAliasConfigured.config.programs.codex.hooks ? Stop;
assert builtins.all
  (
    event: builtins.length dualAliasConfigured.config.programs.claude-code.settings.hooks.${event} == 1
  )
  [
    "SessionStart"
    "UserPromptSubmit"
    "PreToolUse"
    "PermissionRequest"
    "PostToolUse"
    "SubagentStart"
    "SubagentStop"
    "Stop"
  ];
assert packageOnly.config.programs.coding-brain.codexHooks.enable == false;
assert enableOnlyCodex.config.programs.coding-brain.codexHooks.enable == false;
assert enableOnlyFailures == [ ];
assert builtins.length unsupportedFailures == 1;
assert
  (builtins.head unsupportedFailures).message
  == "programs.coding-brain.codexHooks.enable requires Home Manager programs.codex.hooks; disable it or upgrade Home Manager";
assert builtins.length disabledFailures == 1;
assert lib.hasInfix "programs.codex.enable = true" (builtins.head disabledFailures).message;
assert
  (failedAssertions unsupportedClaude) == [
    {
      assertion = false;
      message = "programs.coding-brain.claudeHooks.enable requires Home Manager programs.claude-code.settings; disable it or upgrade Home Manager";
    }
  ];
assert builtins.length (failedAssertions disabledClaude) == 1;
assert lib.hasInfix "programs.claude-code.enable = true"
  (builtins.head (failedAssertions disabledClaude)).message;
assert settingsOnlyClaudeDefault.config.programs.coding-brain.claudeHooks.enable == false;
assert enableOnlyClaudeDefault.config.programs.coding-brain.claudeHooks.enable == false;
assert disabledClaudeDefault.config.programs.coding-brain.claudeHooks.enable == false;
assert
  (failedAssertions unsupportedAntigravity) == [
    {
      assertion = false;
      message = "programs.coding-brain.antigravityHooks.enable requires Home Manager programs.antigravity-cli.enable; disable it or upgrade Home Manager";
    }
  ];
assert builtins.length (failedAssertions disabledAntigravity) == 1;
assert lib.hasInfix "programs.antigravity-cli.enable = true"
  (builtins.head (failedAssertions disabledAntigravity)).message;
assert enableOnlyAntigravityDefault.config.programs.coding-brain.antigravityHooks.enable == false;
assert packageOnlyAntigravityDefault.config.programs.coding-brain.antigravityHooks.enable == false;
assert disabledAntigravityDefault.config.programs.coding-brain.antigravityHooks.enable == false;
assert builtins.length (failedAssertions legacyAntigravity) == 1;
assert lib.hasInfix "genuine Antigravity CLI"
  (builtins.head (failedAssertions legacyAntigravity)).message;
assert lib.getName legacyGeminiPackage == "gemini-cli";
assert
  legacyPackageAntigravityDefault.config.programs.coding-brain.antigravityHooks.enable == false;
assert builtins.length (failedAssertions legacyPackageAntigravityForced) == 1;
assert lib.hasInfix "genuine Antigravity CLI"
  (builtins.head (failedAssertions legacyPackageAntigravityForced)).message;
assert builtins.length (failedAssertions reservedAntigravity) == 1;
assert lib.hasInfix "reserves the coding-brain key"
  (builtins.head (failedAssertions reservedAntigravity)).message;
assert scalarAntigravity.success == false;
assert (builtins.elemAt claudePermission.hooks 0).type == "command";
assert
  (builtins.elemAt claudePermission.hooks 0).command
  == "${expectedExe} --permission-hook --provider claude";
assert (builtins.elemAt claudePermission.hooks 0).timeout == 30;
assert claudePermission.matcher == "*";
assert
  (builtins.elemAt claudeStop.hooks 0).command == "${expectedExe} --recovery-hook --provider claude";
assert (builtins.elemAt claudeStop.hooks 0).timeout == 30;
assert !(claudeStop ? matcher);
assert (builtins.elemAt (builtins.elemAt claudeLifecycleEntries 0).hooks 0).type == "command";
assert (builtins.elemAt claudeLifecycleEntries 0).matcher == "startup|resume|clear|compact";
assert !(builtins.elemAt claudeLifecycleEntries 1 ? matcher);
assert (builtins.elemAt claudeLifecycleEntries 2).matcher == "*";
assert (builtins.elemAt claudeLifecycleEntries 3).matcher == "*";
assert (builtins.elemAt claudeLifecycleEntries 4).matcher == "*";
assert (builtins.elemAt claudeLifecycleEntries 5).matcher == "*";
assert builtins.all (
  entry:
  let
    handler = builtins.elemAt entry.hooks 0;
  in
  handler.type == "command"
  && handler.command == "${expectedExe} --lifecycle-hook --provider claude"
  && handler.timeout == 2
) claudeLifecycleEntries;
assert permission.matcher == "*";
assert permissionHandler.type == "command";
assert permissionHandler.command == "${expectedExe} --permission-hook --provider codex";
assert permissionHandler.timeout == 30;
assert permissionHandler.statusMessage == "Brain reviewing permission…";
assert sessionStart.matcher == "startup|resume|clear|compact";
assert !(userPromptSubmit ? matcher);
assert preToolUse.matcher == "*";
assert postToolUse.matcher == "*";
assert subagentStart.matcher == "*";
assert subagentStop.matcher == "*";
assert !(managedStop ? matcher);
assert builtins.all (
  entry:
  let
    handler = builtins.elemAt entry.hooks 0;
  in
  handler.type == "command"
  && handler.command == "${expectedExe} --lifecycle-hook --provider codex"
  && handler.timeout == 2
) lifecycleEntries;
assert
  (builtins.elemAt managedStop.hooks 0).command == "${expectedExe} --recovery-hook --provider codex";
assert (builtins.elemAt managedStop.hooks 0).timeout == 30;
assert
  stopHooks == [
    existingStop
    managedStop
  ];
assert rollbackConfigured.config.programs.codex.hooks.Stop == [ existingStop ];
assert !(rollbackConfigured.config.programs.codex.hooks ? SessionStart);
assert !(rollbackConfigured.config.programs.codex.hooks ? UserPromptSubmit);
assert !(rollbackConfigured.config.programs.codex.hooks ? PreToolUse);
assert !(rollbackConfigured.config.programs.codex.hooks ? PermissionRequest);
assert !(rollbackConfigured.config.programs.codex.hooks ? PostToolUse);
assert !(rollbackConfigured.config.programs.codex.hooks ? SubagentStart);
assert !(rollbackConfigured.config.programs.codex.hooks ? SubagentStop);
assert rollbackConfigured.config.programs.claude-code.settings.hooks.Stop == [ existingStop ];
assert !(rollbackConfigured.config.programs.claude-code.settings.hooks ? SessionStart);
assert
  !(lib.hasAttrByPath [
    "home"
    "file"
    ".gemini/config/hooks.json"
  ] rollbackConfigured.config);
assert
  trustNotice == ''
    echo "Coding Brain hooks use ${expectedExe}; restart Codex and review /hooks after package changes."
  '';
assert
  !(lib.hasAttrByPath [
    "xdg"
    "configFile"
    "coding-brain/config.toml"
  ] aliasConfigured.config);
assert
  !(lib.hasAttrByPath [
    "systemd"
    "user"
    "services"
    "coding-brain-headless"
  ] cfg);
pkgs.runCommand "coding-brain-home-manager-module-check" { nativeBuildInputs = [ pkgs.jq ]; } ''
  grep -F 'endpoint = "http://localhost:11434/api/generate"' \
    ${cfg.xdg.configFile."coding-brain/config.toml".source}
  grep -F 'model = "gemma4:e4b"' \
    ${cfg.xdg.configFile."coding-brain/config.toml".source}
  grep -F 'timeout_ms = 25000' \
    ${cfg.xdg.configFile."coding-brain/config.toml".source}
  grep -F 'theme = "dark"' \
    ${cfg.xdg.configFile."coding-brain/config.toml".source}
  ! grep -F 'enabled =' ${cfg.xdg.configFile."coding-brain/config.toml".source}
  ! grep -F 'auto =' ${cfg.xdg.configFile."coding-brain/config.toml".source}
  ! grep -F 'terminal_auto_approve_fallback' ${cfg.xdg.configFile."coding-brain/config.toml".source}
  grep -F 'restart Codex' ${configured.activationPackage}/activate
  grep -F '/hooks' ${configured.activationPackage}/activate
  grep -F 'Coding Brain provider hooks use ${expectedExe}' ${configured.activationPackage}/activate
  grep -F 'restart Claude Code or Antigravity CLI' ${configured.activationPackage}/activate
  grep -F 'coding-brain doctor' ${configured.activationPackage}/activate
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
  jq -e '
    ."coding-brain"
    | all(.[]; length == 1)
  ' \
    ${dualAliasConfigured.config.home.file.".gemini/config/hooks.json".source}
  touch "$out"
''
