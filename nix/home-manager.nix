{ self }:
{
  config,
  lib,
  options,
  pkgs,
  ...
}:

let
  cfg = config.programs.coding-brain;
  tomlFormat = pkgs.formats.toml { };
  hasCodexHooks = lib.hasAttrByPath [
    "programs"
    "codex"
    "hooks"
  ] options;
  codexEnabled = lib.attrByPath [
    "programs"
    "codex"
    "enable"
  ] false config;
  executable = lib.getExe cfg.package;
in
{
  _file = ./home-manager.nix;
  key = "coding-brain-home-manager-module";

  options.programs.coding-brain = {
    enable = lib.mkEnableOption "Coding Brain supervision and learning";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.codexctl.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The Coding Brain package used by the CLI and generated hooks.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      description = ''
        Non-secret configuration written to
        {file}`$XDG_CONFIG_HOME/coding-brain/config.toml`.
        Values are copied to the world-readable Nix store; do not put tokens,
        credentials, or token-bearing webhook URLs here.
      '';
    };

    codexHooks.enable = lib.mkOption {
      type = lib.types.bool;
      default = hasCodexHooks && codexEnabled;
      defaultText = lib.literalExpression ''
        lib.hasAttrByPath [ "programs" "codex" "hooks" ] options
        && config.programs.codex.enable
      '';
      description = "Whether to merge Coding Brain's Codex lifecycle, permission, and recovery hooks into programs.codex.hooks.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        home.packages = [ cfg.package ];
        xdg.configFile."coding-brain/config.toml" = lib.mkIf (cfg.settings != { }) {
          source = tomlFormat.generate "coding-brain-config.toml" cfg.settings;
        };
        assertions = [
          {
            assertion = !cfg.codexHooks.enable || hasCodexHooks;
            message = "programs.coding-brain.codexHooks.enable requires Home Manager programs.codex.hooks; disable it or upgrade Home Manager";
          }
          {
            assertion = !cfg.codexHooks.enable || !hasCodexHooks || codexEnabled;
            message = "programs.coding-brain.codexHooks.enable requires programs.codex.enable = true";
          }
        ];
      }
      (lib.mkIf cfg.codexHooks.enable (
        lib.optionalAttrs hasCodexHooks {
          programs.codex.hooks = {
            SessionStart = lib.mkAfter [
              {
                matcher = "startup|resume|clear|compact";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            UserPromptSubmit = lib.mkAfter [
              {
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            PreToolUse = lib.mkAfter [
              {
                matcher = "*";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            PermissionRequest = lib.mkAfter [
              {
                matcher = "*";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --permission-hook --provider codex";
                    timeout = 30;
                    statusMessage = "Brain reviewing permission…";
                  }
                ];
              }
            ];
            PostToolUse = lib.mkAfter [
              {
                matcher = "*";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            SubagentStart = lib.mkAfter [
              {
                matcher = "*";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            SubagentStop = lib.mkAfter [
              {
                matcher = "*";
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --lifecycle-hook --provider codex";
                    timeout = 2;
                  }
                ];
              }
            ];
            Stop = lib.mkAfter [
              {
                hooks = [
                  {
                    type = "command";
                    command = "${executable} --recovery-hook --provider codex";
                    timeout = 30;
                  }
                ];
              }
            ];
          };
          home.activation.codingBrainHookTrustNotice = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
            echo "Coding Brain hooks use ${executable}; restart Codex and review /hooks after package changes."
          '';
        }
      ))
    ]
  );
}
