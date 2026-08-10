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
  providerHomeManagerFiles = pkgs.runCommand "home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    cp ${codexHooksJson} "$out/.codex/hooks.json"
    cp ${claudeSettingsJson} "$out/.claude/settings.json"
    cp ${configured.config.home.file.".gemini/config/hooks.json".source} \
      "$out/.gemini/config/hooks.json"
  '';
  invalidProviderHomeManagerFiles = pkgs.runCommand "invalid-antigravity-home-manager-files" { } ''
    mkdir -p "$out/.codex" "$out/.claude" "$out/.gemini/config"
    cp ${providerHomeManagerFiles}/.codex/hooks.json "$out/.codex/hooks.json"
    cp ${providerHomeManagerFiles}/.claude/settings.json "$out/.claude/settings.json"
    printf '%s\n' '["SECRET_PROVIDER_CONTENT"]' \
      > "$out/.gemini/config/hooks.json"
  '';
  fakeProviders = pkgs.runCommand "coding-brain-fake-providers" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.coreutils}/bin/true "$out/bin/codex"
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
