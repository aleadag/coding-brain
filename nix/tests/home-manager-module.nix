{
  home-manager,
  pkgs,
  self,
}:

let
  inherit (pkgs) lib;
  testPackage = pkgs.writeShellScriptBin "codexctl" "exit 0";
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
        programs.codexctl = {
          enable = true;
          package = testPackage;
          settings.brain = {
            enabled = true;
            endpoint = "http://localhost:11434/api/generate";
            model = "gemma4:e4b";
            auto = false;
            timeout_ms = 25000;
            terminal_auto_approve_fallback = false;
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
        programs.codexctl = {
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
        programs.codexctl = {
          enable = true;
          package = testPackage;
          codexHooks.enable = false;
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
        programs.codexctl = {
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
        home.activation = lib.mkOption {
          type = lib.types.attrsOf lib.types.unspecified;
          default = { };
        };
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
      { programs.codexctl.enable = true; }
    ];
  };
  unsupportedHooks = lib.evalModules {
    specialArgs = { inherit pkgs; };
    modules = [
      compatibilityOptions
      self.homeManagerModules.default
      {
        programs.codexctl = {
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
        programs.codexctl.enable = true;
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
        programs.codexctl = {
          enable = true;
          package = testPackage;
          codexHooks.enable = true;
        };
      }
    ];
  };
  cfg = configured.config;
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
    managedStop
  ];
  trustNotice = cfg.home.activation.codexctlHookTrustNotice.data;
  unsupportedFailures = builtins.filter (item: !item.assertion) unsupportedHooks.config.assertions;
  enableOnlyFailures = builtins.filter (item: !item.assertion) enableOnlyCodex.config.assertions;
  disabledFailures = builtins.filter (item: !item.assertion) disabledCodex.config.assertions;
in
assert builtins.elem testPackage cfg.home.packages;
assert aliasConfigured.config.programs.codexctl.enable;
assert builtins.length dualAliasConfigured.config.programs.codex.hooks.PermissionRequest == 1;
assert dualAliasConfigured.config.programs.codex.hooks ? PostToolUse;
assert dualAliasConfigured.config.programs.codex.hooks ? Stop;
assert packageOnly.config.programs.codexctl.codexHooks.enable == false;
assert enableOnlyCodex.config.programs.codexctl.codexHooks.enable == false;
assert enableOnlyFailures == [ ];
assert builtins.length unsupportedFailures == 1;
assert
  (builtins.head unsupportedFailures).message
  == "programs.codexctl.codexHooks.enable requires Home Manager programs.codex.hooks; disable it or upgrade Home Manager";
assert builtins.length disabledFailures == 1;
assert lib.hasInfix "programs.codex.enable = true" (builtins.head disabledFailures).message;
assert permission.matcher == "*";
assert permissionHandler.type == "command";
assert permissionHandler.command == "${expectedExe} --permission-hook";
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
  && handler.command == "${expectedExe} --lifecycle-hook"
  && handler.timeout == 2
) lifecycleEntries;
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
assert
  trustNotice == ''
    echo "codexctl hooks use ${expectedExe}; restart Codex and review /hooks after package changes."
  '';
assert
  !(lib.hasAttrByPath [
    "xdg"
    "configFile"
    "codexctl/config.toml"
  ] aliasConfigured.config);
assert
  !(lib.hasAttrByPath [
    "systemd"
    "user"
    "services"
    "codexctl-headless"
  ] cfg);
pkgs.runCommand "codexctl-home-manager-module-check" { } ''
  grep -F 'enabled = true' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'endpoint = "http://localhost:11434/api/generate"' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'model = "gemma4:e4b"' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'auto = false' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'timeout_ms = 25000' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'terminal_auto_approve_fallback = false' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F 'restart Codex' ${configured.activationPackage}/activate
  grep -F '/hooks' ${configured.activationPackage}/activate
  touch "$out"
''
