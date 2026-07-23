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
  jsonFormat = pkgs.formats.json { };
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
    lib.attrByPath [
      "programs"
      "antigravity-cli"
      "useLegacyGeminiConfig"
    ] false config
    || (antigravityPackage != null && lib.getName antigravityPackage == "gemini-cli");
  executable = lib.getExe cfg.package;
  mkHandler = command: timeout: {
    type = "command";
    inherit command timeout;
  };
  mkNestedHook =
    command: timeout: matcher:
    {
      hooks = [ (mkHandler command timeout) ];
    }
    // lib.optionalAttrs (matcher != null) { inherit matcher; };
  managedAntigravityDefinition = {
    PreToolUse = [
      (mkNestedHook
        "${executable} --permission-hook --provider antigravity --antigravity-hook-event PreToolUse"
        30
        "*"
      )
    ];
    PostToolUse = [
      (mkNestedHook
        "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PostToolUse"
        2
        "*"
      )
    ];
    PreInvocation = [
      (mkHandler "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PreInvocation" 2)
    ];
    PostInvocation = [
      (mkHandler "${executable} --lifecycle-hook --provider antigravity --antigravity-hook-event PostInvocation" 2)
    ];
    Stop = [
      (mkHandler "${executable} --recovery-hook --provider antigravity --antigravity-hook-event Stop" 30)
    ];
  };
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
      (lib.mkIf cfg.antigravityHooks.enable {
        home.file.".gemini/config/hooks.json".source =
          jsonFormat.generate "coding-brain-antigravity-hooks.json"
            (
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
    ]
  );
}
