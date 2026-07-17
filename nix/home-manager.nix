{ self }:
{
  config,
  lib,
  options,
  pkgs,
  ...
}:

let
  cfg = config.programs.codexctl;
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
  key = "codexctl-home-manager-module";

  options.programs.codexctl = {
    enable = lib.mkEnableOption "codexctl session supervision";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "inputs.codexctl.packages.\${pkgs.stdenv.hostPlatform.system}.default";
      description = "The codexctl package used by the CLI and generated hooks.";
    };

    settings = lib.mkOption {
      inherit (tomlFormat) type;
      default = { };
      description = ''
        Non-secret configuration written to
        {file}`$XDG_CONFIG_HOME/codexctl/config.toml`.
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
      description = "Whether to merge the codexctl lifecycle and permission hooks into programs.codex.hooks.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        home.packages = [ cfg.package ];
        xdg.configFile."codexctl/config.toml" = lib.mkIf (cfg.settings != { }) {
          source = tomlFormat.generate "codexctl-config.toml" cfg.settings;
        };
        assertions = [
          {
            assertion = !cfg.codexHooks.enable || hasCodexHooks;
            message = "programs.codexctl.codexHooks.enable requires Home Manager programs.codex.hooks; disable it or upgrade Home Manager";
          }
          {
            assertion = !cfg.codexHooks.enable || !hasCodexHooks || codexEnabled;
            message = "programs.codexctl.codexHooks.enable requires programs.codex.enable = true";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --permission-hook";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --lifecycle-hook";
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
                    command = "${executable} --lifecycle-hook";
                    timeout = 2;
                  }
                ];
              }
            ];
          };
          home.activation.codexctlHookTrustNotice = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
            echo "codexctl hooks use ${executable}; restart Codex and review /hooks after package changes."
          '';
        }
      ))
    ]
  );
}
