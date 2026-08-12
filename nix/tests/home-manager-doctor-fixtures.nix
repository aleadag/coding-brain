{
  home-manager,
  package,
  pkgs,
  self,
}:

let
  baseHome = {
    home.username = "cbrain-test";
    home.homeDirectory = "/home/cbrain-test";
    home.stateVersion = "25.11";
  };
  configured = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
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
          inherit package;
        };
      }
    ];
  };
  codexHooksJson = pkgs.writeText "codex-hooks.json" (
    builtins.toJSON { hooks = configured.config.programs.codex.hooks; }
  );
  claudeSettingsJson = pkgs.writeText "claude-settings.json" (
    builtins.toJSON configured.config.programs.claude-code.settings
  );
  invalidAntigravityHooksJson = pkgs.writeText "invalid-antigravity-hooks.json" ''
    ["SECRET_PROVIDER_CONTENT"]
  '';
  providerHomeManagerFiles = pkgs.runCommand "home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    ln -s ${codexHooksJson} "$out/.codex/hooks.json"
    ln -s ${claudeSettingsJson} "$out/.claude/settings.json"
    ln -s ${configured.config.home.file.".gemini/config/hooks.json".source} \
      "$out/.gemini/config/hooks.json"
  '';
  invalidProviderHomeManagerFiles = pkgs.runCommand "invalid-antigravity-home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    ln -s ${codexHooksJson} "$out/.codex/hooks.json"
    ln -s ${claudeSettingsJson} "$out/.claude/settings.json"
    ln -s ${invalidAntigravityHooksJson} "$out/.gemini/config/hooks.json"
  '';
  fakeProviders = pkgs.runCommand "coding-brain-fake-providers" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.writeShellScript "fake-codex" ''
      if [ "$1" = "app-server" ]; then
        IFS= read -r _initialize
        printf '%s\n' '{"id":0,"result":{}}'
        IFS= read -r _initialized
        IFS= read -r _hooks_list
        printf '%s\n' "{\"id\":1,\"result\":{\"data\":[{\"cwd\":\"$PWD\",\"hooks\":[{\"eventName\":\"stop\",\"handlerType\":\"command\",\"command\":\"cbrain --recovery-hook\",\"enabled\":true,\"trustStatus\":\"trusted\"}],\"warnings\":[],\"errors\":[]}]}}"
      fi
    ''} "$out/bin/codex"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/claude"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/agy"
  '';
in
{
  inherit
    fakeProviders
    invalidProviderHomeManagerFiles
    providerHomeManagerFiles
    ;
}
