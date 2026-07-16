# Declarative Home Manager Module Design

## Goal

Export a reusable Home Manager module from the codexctl flake so Nix users can install codexctl, generate its TOML configuration, and merge codexctl's lifecycle hooks into Home Manager's existing `programs.codex.hooks` configuration. Hook commands must invoke the exact Nix package selected by the module rather than relying on `PATH`.

## Scope

The first module manages three things:

1. the codexctl package in `home.packages`;
2. `~/.config/codexctl/config.toml` from a TOML-typed settings attribute set;
3. the same Codex hooks installed by `codexctl init`, merged through `programs.codex.hooks`.

It does not create a headless user service. Native `PermissionRequest` hooks are invoked directly by Codex and do not require a daemon. Always-on monitoring remains downstream policy and can be designed separately if demand appears.

## Public Flake Output

The flake exports the module at both names for ecosystem compatibility:

```nix
homeManagerModules.default
homeModules.default
```

Both names reference the same module. Per-system package and development-shell outputs remain unchanged.

The flake adds a `home-manager` input that follows the existing `nixpkgs` input. The public module remains consumer-importable, while the pinned input provides the real Home Manager module system used by flake checks.

## Public Module API

The option namespace is `programs.codexctl`:

```nix
programs.codexctl = {
  enable = true;

  settings = {
    brain = {
      enabled = true;
      endpoint = "http://localhost:11434/api/generate";
      model = "gemma4:e4b";
      auto = false;
      timeout_ms = 25000;
      terminal_auto_approve_fallback = false;
    };
  };

  codexHooks.enable = true;
};
```

- `enable` defaults to `false`.
- `package` is a package option whose default is this flake's package for the evaluating host system.
- `settings` uses `pkgs.formats.toml { }` so incompatible Nix values fail during evaluation. An empty attribute set does not create a config file.
- `codexHooks.enable` defaults to the value of `programs.codex.enable` when that option is available, otherwise `false`. Users can disable only the Codex hook contribution while retaining package and TOML management.

The module does not duplicate every codexctl setting as a typed Nix option. The TOML format type provides serialization validation while keeping the module compatible with new codexctl configuration keys.

## Generated Configuration

When enabled, the module adds the selected package to `home.packages`. Non-empty `settings` generate `xdg.configFile."codexctl/config.toml"`.

When `codexHooks.enable` is true, the module appends these definitions to `programs.codex.hooks`:

- `PermissionRequest` with matcher `Bash`, timeout 30 seconds, status message `Brain reviewing permission…`, and command `${lib.getExe cfg.package} --permission-hook`;
- `PostToolUse` with matcher `*` and the existing lightweight JSON refresh command;
- `Stop` with the existing lightweight JSON refresh command.

The module contributes each event list with `lib.mkAfter`. Nix list merging preserves hooks declared elsewhere, including an existing `Stop` hook, and keeps those existing hooks before codexctl's entries. The module never owns or overwrites `~/.codex/hooks.json` directly.

All generated commands use `${lib.getExe cfg.package}`. This guarantees that Codex invokes the same immutable Nix package selected by Home Manager, including when Codex starts outside an interactive shell.

Declarative and imperative installation remain idempotent with each other. The Rust installer recognizes the exact supported absolute command shapes for both the permission handler and lightweight JSON refresh handlers, so running `codexctl init` after Home Manager activation does not append duplicate managed hooks.

## Compatibility And Diagnostics

Hook discovery must recognize an exact command consisting of a bare `codexctl` or an absolute path ending in `/codexctl`, followed only by `--permission-hook`, as both managed and current when the matcher, handler type, timeout, and status message match. Extra arguments make the handler stale. `codexctl doctor` must not label the declarative Nix hook stale merely because its command is absolute.

The module detects `programs.codex.hooks` through its `options` argument. Older Home Manager versions can still use package and TOML management because hook integration defaults off when the Codex hook option is absent. If `codexHooks.enable` is explicitly true and that option is unavailable, evaluation fails with a targeted assertion explaining that hook integration must be disabled or Home Manager upgraded. This is preferable to silently writing a competing hooks file.

The module does not enable `programs.codex` automatically. If hook integration is enabled while `programs.codex.enable` is false, evaluation fails with a targeted assertion because Home Manager would not render the merged hooks.

## Security

- The native permission hook remains the preferred approval boundary.
- `terminal_auto_approve_fallback` remains false unless the user explicitly sets it in `settings`.
- The module does not generate codexctl static approve or deny rules.
- The absolute package path prevents an earlier `PATH` entry from substituting another executable.
- Existing hook trust behavior remains owned by Codex; users must review changed non-managed hooks through `/hooks`.
- Hook commands intentionally embed the immutable Nix-store executable. A codexctl package upgrade therefore changes the hook definition and requires renewed `/hooks` trust. This is preferred over a stable mutable profile symlink whose target could change without a hook-definition review.
- `settings` are rendered into the world-readable Nix store. The option documentation must warn users not to place secrets, including token-bearing webhook URLs, in this attribute set. Runtime secret injection is out of scope until codexctl has a dedicated non-store mechanism.

## Testing

Implementation follows test-first development.

1. Add a flake check that imports the pinned Home Manager input and the exported module, builds a real activation package, and asserts:
   - the selected package is installed;
   - TOML contains the configured Brain values;
   - all codexctl hooks use the absolute package executable;
   - an unrelated existing `Stop` hook remains present and ordered before codexctl's hook;
   - both exported module names reference the same module behavior.
2. Add evaluation coverage for package-only use when `programs.codex.hooks` is unavailable, plus focused assertion coverage for explicitly enabled hooks with unavailable or disabled Codex integration.
3. Add Rust regression coverage proving exact absolute Nix-store permission and JSON-refresh commands are managed/current, extra arguments are stale, and imperative initialization does not duplicate declarative hooks.
4. Run `nix flake check`, `nix fmt -- --check`, Rust focused tests, workspace tests, Clippy with warnings denied, and the workspace build.

## Alternatives Rejected

### Own `~/.codex/hooks.json` directly

This would make the module independent of `programs.codex`, but it would conflict with the user's existing declarative Codex hooks and force codexctl to own the entire file.

### Run `codexctl init` during Home Manager activation

This preserves the Rust installer as the single implementation, but it is imperative, mutates a file behind Home Manager's back, and cannot provide stable evaluation-time composition.

### Add a headless service now

The service is not required for native permission decisions and would introduce restart, logging, environment, and platform policy unrelated to the requested declarative configuration.

## Acceptance Criteria

- Importing either exported module name exposes `programs.codexctl`.
- Enabling the module installs the selected codexctl package.
- Non-empty settings render the expected global TOML; empty settings render no file.
- Codex hook definitions merge without deleting user hooks and invoke the absolute selected binary.
- Declarative absolute-path permission hooks are reported as current by discovery and doctor.
- Hook integration fails clearly when the required `programs.codex` surface is unavailable or disabled.
- No headless service is created.
- Nix and Rust quality gates pass.

Rollback is declarative: disabling `programs.codexctl` or reverting the flake input removes the package, generated config, and contributed hook definitions on the next Home Manager rebuild. No mutable activation migration is involved.

## Stress Test Results: Home Manager Module

### Resolved Decisions

- Add a pinned Home Manager input following `nixpkgs`, and export identical `homeManagerModules.default` and `homeModules.default` entry points.
- Default hook integration from `programs.codex.enable` when available; retain package/config-only compatibility otherwise.
- Mirror all three imperative installer hooks and append them after existing declarative hooks.
- Use absolute package executables for every hook and recognize only exact supported command shapes as current.
- Preserve Nix-store paths in hook definitions so package upgrades intentionally trigger Codex trust review.
- Give Home Manager full ownership of non-empty generated settings without activation-time merging.
- Keep older Home Manager package/config use working and fail clearly only when unsupported hooks are explicitly requested.
- Verify with a real Home Manager activation package, focused Rust regressions, and full Nix/Rust gates.

### Changes Made

- Refined the hook default so codexctl does not force or require Codex for package-only use.
- Added exact absolute-command idempotence requirements for permission and refresh hooks.
- Added a Nix-store secret exposure warning for `settings`.
- Documented renewed hook trust after codexctl package upgrades as an intentional security property.
- Added explicit compatibility behavior and declarative rollback semantics.

### Deferred / Parking Lot

- Optional `codexctl-headless` user service.
- Runtime secret injection for codexctl settings.

### Confidence Assessment

- Overall: High.
- Areas of concern: Home Manager's `programs.codex.hooks` surface is version-dependent, addressed through option detection, pinned evaluation tests, and targeted assertions. Nix upgrades require renewed hook trust by design.
