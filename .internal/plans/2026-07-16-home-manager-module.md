# Home Manager Module Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Export a declarative Home Manager module that installs the selected codexctl package, renders non-secret TOML settings, and merges absolute-path codexctl hooks into `programs.codex.hooks`.

**Architecture:** Extend Rust hook recognition so immutable Nix-store commands remain managed, current, and idempotent with `codexctl init`. Export one Home Manager module under two conventional flake aliases, validate it with a real pinned Home Manager activation package plus compatibility evaluations, and document the immutable-hook trust workflow.

**Tech Stack:** Nix flakes, Home Manager modules, `pkgs.formats.toml`, Rust 2024, Serde JSON fixtures, Jujutsu, Beads.

## Global Constraints

- The module manages package installation, TOML settings, and Codex hooks only; it must not create a headless service.
- Hook definitions merge through `programs.codex.hooks`; the module must not own `~/.codex/hooks.json` directly.
- Every hook command uses `${lib.getExe cfg.package}` so Codex executes the selected immutable Nix package.
- `codexHooks.enable` follows `programs.codex.enable` when that option exists and otherwise defaults false.
- Explicit hook enablement fails clearly when `programs.codex.hooks` is unavailable or `programs.codex.enable` is false.
- `settings` are Nix-store-backed and documented as unsuitable for secrets.
- Package upgrades intentionally change the hook definition and require renewed `/hooks` trust.
- Existing user hooks remain before codexctl hooks; declarative and imperative installation must not duplicate managed handlers.
- Use test-first development: observe each focused test fail for the missing behavior before implementing it.
- Preserve the automatically split jj planning stack; describe each planning change accurately with the commit-message workflow, give every implementation task its own described jj change, and do not squash or push.
- At each task boundary, inspect `jj st`: describe the current `@` when it is empty, or run `jj new -m` when the preceding task still occupies `@`. Never let two workers edit the same jj change.

---

## Pre-execution: Normalize the Automatically Split jj Stack

The configured Codex Stop hook ran `jj new` after planning turns. Before source edits, use the `commit-message` skill to inspect and describe `uk`, `pwq`, `pz`, and `nmp` with accurate emoji-conventional documentation subjects. Keep the changes separate and verify each exact revset with `jj --no-pager show --git <revset>` before describing it.

Then describe the current empty working copy for Task 1:

```bash
jj desc -r @ -m "🐛 fix: recognize Nix-store hook commands"
jj --no-pager st
jj --no-pager log -r 'uk|pwq|pz|nmp|@' --no-graph
```

Expected: every planning change and the Task 1 change has an emoji-conventional description; no content is squashed, abandoned, or pushed.

---

### Task 1: Recognize Exact Absolute codexctl Hook Commands

**Files:**
- Modify: `src/init/hooks.rs`

**Interfaces:**
- Consumes: current managed-hook parsing in `is_current_permission_command`, `is_legacy_snapshot_command`, `is_managed_permission_command`, `inspect_permission_handlers`, and `merge_hooks`.
- Produces: exact current-command classification for bare or absolute codexctl executables; conservative stale classification for modified permission commands; idempotent replacement of absolute refresh handlers.

**Acceptance Criteria:**
- A command whose executable is `codexctl` or ends in `/codexctl` and whose only argument is `--permission-hook` is current when the handler metadata matches.
- Extra permission-hook arguments remain managed/configured but are stale, so they continue blocking terminal fallback.
- Bare and absolute `--json` refresh commands, with or without the exact `2>/dev/null || true` suffix, are treated as managed by init/uninit.
- `merge_hooks` replaces declarative absolute-path managed handlers with one current imperative definition and never duplicates events.
- Unrelated commands whose executable merely contains `codexctl` remain untouched.

- [ ] **Step 1: Add the two behaviorally red tests**

Add these focused cases beside the existing hook discovery and merge tests in `src/init/hooks.rs`:

```rust
#[test]
fn discovery_treats_absolute_permission_hook_as_current() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    write_hooks(
        &home.join(".codex/hooks.json"),
        serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook",
                    "timeout": 30,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }] }
        }),
    );

    let discovery = discover_permission_hooks_at(Some(&home), &cwd);

    assert!(discovery.global.configured);
    assert!(discovery.global.current);
    assert!(!discovery.global.stale);
}

#[test]
fn merge_replaces_absolute_managed_hooks_without_duplicates() {
    let mut settings = serde_json::json!({
        "hooks": {
            "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook",
                    "timeout": 30,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }],
            "PostToolUse": [{
                "matcher": "*",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --json 2>/dev/null || true",
                    "timeout": 5
                }]
            }],
            "Stop": [{
                "matcher": "",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --json 2>/dev/null || true",
                    "timeout": 5
                }]
            }]
        }
    });

    merge_hooks(&mut settings);
    let once = settings.clone();
    merge_hooks(&mut settings);

    assert_eq!(settings, once);
    for event in ["PermissionRequest", "PostToolUse", "Stop"] {
        assert_eq!(settings["hooks"][event].as_array().unwrap().len(), 1);
    }
}
```

- [ ] **Step 2: Run the focused tests and confirm the expected failures**

Run:

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::hooks::tests
```

Expected: the absolute permission handler is configured but reported stale, and absolute refresh entries are retained alongside newly generated handlers.

- [ ] **Step 3: Add green characterization tests for existing safety behavior**

Before changing production code, add these cases:

```rust
#[test]
fn permission_hook_with_extra_arguments_is_managed_but_stale() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("project");
    std::fs::create_dir_all(cwd.join(".git")).unwrap();
    write_hooks(
        &home.join(".codex/hooks.json"),
        serde_json::json!({
            "hooks": { "PermissionRequest": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": "/nix/store/test-codexctl/bin/codexctl --permission-hook --unexpected",
                    "timeout": 30,
                    "statusMessage": "Brain reviewing permission…"
                }]
            }] }
        }),
    );

    let discovery = discover_permission_hooks_at(Some(&home), &cwd);

    assert!(discovery.global.configured);
    assert!(!discovery.global.current);
    assert!(discovery.global.stale);
    assert!(discovery.blocks_terminal_fallback());
}

#[test]
fn merge_preserves_lookalike_permission_executable() {
    let lookalike = serde_json::json!({
        "type": "command",
        "command": "notify-codexctl --permission-hook",
        "timeout": 30
    });
    let mut settings = serde_json::json!({
        "hooks": { "PermissionRequest": [{
            "matcher": "Bash",
            "hooks": [lookalike.clone()]
        }] }
    });

    merge_hooks(&mut settings);

    let permission = settings["hooks"]["PermissionRequest"].as_array().unwrap();
    assert_eq!(permission[0]["hooks"], serde_json::json!([lookalike]));
}
```

Run the same focused command from Step 2.

Expected: both characterization tests pass before implementation, proving extra arguments remain stale/configured and lookalike executables remain user-owned.

- [ ] **Step 4: Implement exact command-shape helpers**

Replace permissive freshness comparison with helpers shaped like:

```rust
fn is_codexctl_program(program: &str) -> bool {
    program == "codexctl" || program.ends_with("/codexctl")
}

fn is_exact_codexctl_command(command: &str, expected_args: &[&str]) -> bool {
    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return false;
    };
    is_codexctl_program(program) && words.eq(expected_args.iter().copied())
}

fn is_current_permission_command(command: &str) -> bool {
    is_exact_codexctl_command(command, &["--permission-hook"])
}

fn contains_managed_permission_flag(command: &str) -> bool {
    let mut words = command.split_whitespace();
    words.next().is_some_and(is_codexctl_program)
        && words.any(|argument| argument == "--permission-hook")
}

fn is_managed_snapshot_command(command: &str) -> bool {
    is_exact_codexctl_command(command, &["--json"])
        || is_exact_codexctl_command(
            command,
            &["--json", "2>/dev/null", "||", "true"],
        )
}
```

Use the exact permission helper in the `current` metadata check. Use the broader flag-presence helper only to retain conservative configured/stale ownership. Replace `is_legacy_snapshot_command` call sites with `is_managed_snapshot_command` so init and uninit recognize absolute refresh handlers.

- [ ] **Step 5: Run hook tests green and check formatting**

Run:

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::hooks::tests
cargo fmt --all --check
```

Expected: every hook test passes and formatting reports no diff.

- [ ] **Step 6: Review the Task 1 diff without changing jj history**

Run:

```bash
jj --no-pager diff --git src/init/hooks.rs
jj --no-pager st
```

Expected: only command classification and its regression tests changed; the task change is described `🐛 fix: recognize Nix-store hook commands`.

---

### Task 2: Export and Verify the Home Manager Module

**Files:**
- Create: `nix/home-manager.nix`
- Create: `nix/tests/home-manager-module.nix`
- Modify: `flake.nix`
- Modify: `flake.lock`

**Interfaces:**
- Consumes: `self.packages.${system}.default`, Home Manager's `programs.codex.hooks`, `home.packages`, and `xdg.configFile`.
- Produces: `homeManagerModules.default`, `homeModules.default`, `programs.codexctl.{enable,package,settings,codexHooks.enable}`, and `checks.${system}.home-manager-module`.

**Acceptance Criteria:**
- Both flake aliases expose the same module behavior.
- Enabling `programs.codexctl` installs the selected package and renders non-empty TOML settings at `xdg.configFile."codexctl/config.toml"`.
- Empty settings do not create the config-file entry.
- Hook integration defaults from `programs.codex.enable`, appends three absolute-path hooks after existing entries, and prints a non-mutating `/hooks` trust reminder during activation.
- Package/config-only evaluation works without `programs.codex.hooks`; explicit unsupported hook enablement records a focused failed assertion.
- Hook enablement with `programs.codex.enable = false` records a focused failed assertion.
- No `systemd.user.services.codexctl-headless` option or service is created.

- [ ] **Step 0: Enter a dedicated Task 2 jj change**

Run `jj --no-pager st`. If `@` is empty because the Stop hook already advanced, run:

```bash
jj desc -r @ -m "✨ feat: add Home Manager module"
```

If `@` still contains Task 1, run:

```bash
jj new -m "✨ feat: add Home Manager module"
```

Verify with `jj --no-pager st` before editing.

- [ ] **Step 1: Add the red Home Manager evaluation fixture**

Create `nix/tests/home-manager-module.nix` with a real Home Manager configuration, a cheap test package, an existing Stop hook, an alias evaluation, and a minimal option-stub evaluation for older Home Manager compatibility:

```nix
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
  disabledCodex = home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      self.homeManagerModules.default
      baseHome
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
  compatibilityOptions = { lib, ... }: {
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
  cfg = configured.config;
  permission = lib.last cfg.programs.codex.hooks.PermissionRequest;
  postToolUse = lib.last cfg.programs.codex.hooks.PostToolUse;
  stopHooks = cfg.programs.codex.hooks.Stop;
  generatedStop = lib.last stopHooks;
  unsupportedFailures = builtins.filter (item: !item.assertion) unsupportedHooks.config.assertions;
  disabledFailures = builtins.filter (item: !item.assertion) disabledCodex.config.assertions;
in
assert builtins.elem testPackage cfg.home.packages;
assert aliasConfigured.config.programs.codexctl.enable;
assert builtins.length dualAliasConfigured.config.programs.codex.hooks.PermissionRequest == 1;
assert builtins.length dualAliasConfigured.config.programs.codex.hooks.PostToolUse == 1;
assert builtins.length dualAliasConfigured.config.programs.codex.hooks.Stop == 1;
assert packageOnly.config.programs.codexctl.codexHooks.enable == false;
assert builtins.length unsupportedFailures == 1;
assert builtins.length disabledFailures == 1;
assert lib.hasInfix "programs.codex.enable = true" (builtins.head disabledFailures).message;
assert (builtins.elemAt permission.hooks 0).command == "${expectedExe} --permission-hook";
assert (builtins.elemAt postToolUse.hooks 0).command == "${expectedExe} --json 2>/dev/null || true";
assert (builtins.head stopHooks) == existingStop;
assert (builtins.elemAt generatedStop.hooks 0).command == "${expectedExe} --json 2>/dev/null || true";
assert !(lib.hasAttrByPath [ "xdg" "configFile" "codexctl/config.toml" ] aliasConfigured.config);
assert !(lib.hasAttrByPath [ "systemd" "user" "services" "codexctl-headless" ] cfg);
pkgs.runCommand "codexctl-home-manager-module-check" { } ''
  grep -F 'endpoint = "http://localhost:11434/api/generate"' \
    ${cfg.xdg.configFile."codexctl/config.toml".source}
  grep -F '/hooks' ${configured.activationPackage}/activate
  touch "$out"
''
```

- [ ] **Step 2: Add an empty module scaffold, wire the input/check/export, and observe the behavioral red failure**

Create `nix/home-manager.nix` as an intentionally empty module scaffold:

```nix
{ self }:
{ ... }:
{
  _file = ./home-manager.nix;
}
```

Add the input and check references in `flake.nix`:

```nix
home-manager = {
  url = "github:nix-community/home-manager";
  inputs.nixpkgs.follows = "nixpkgs";
};
```

Restructure outputs as a merge of system-independent module exports and the complete existing per-system outputs:

```nix
outputs =
  {
    self,
    nixpkgs,
    flake-utils,
    home-manager,
  }:
  let
    homeManagerModule = import ./nix/home-manager.nix { inherit self; };
  in
  flake-utils.lib.eachDefaultSystem (
    system:
    let
      pkgs = nixpkgs.legacyPackages.${system};
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages.default = pkgs.rustPlatform.buildRustPackage {
        pname = "codexctl";
        version = cargoToml.package.version;
        src = ./.;
        cargoLock.lockFile = ./Cargo.lock;
        nativeCheckInputs = [ pkgs.git ];

        meta = with pkgs.lib; {
          description = "Orchestrate Codex sessions with a local-LLM brain that learns from you.";
          homepage = "https://github.com/aleadag/codexctl";
          license = licenses.mit;
          mainProgram = "codexctl";
          platforms = platforms.unix;
        };
      };

      checks.home-manager-module = import ./nix/tests/home-manager-module.nix {
        inherit home-manager pkgs self;
      };

      formatter = pkgs.nixfmt-rfc-style;

      devShells.default = pkgs.mkShell {
        buildInputs = with pkgs; [
          rustc
          cargo
          clippy
          rustfmt
          sqlite
        ];
        env.GH_REPO = "aleadag/codexctl";
      };
    }
  )
  // {
    homeManagerModules.default = homeManagerModule;
    homeModules.default = homeManagerModule;
  };
```

Run:

```bash
nix flake lock
nix flake check --no-build
```

Expected: evaluation reaches the fixture and fails because the empty scaffold does not define `programs.codexctl`. Do not weaken the fixture to make red pass.

- [ ] **Step 3: Implement the minimal Home Manager module**

Create `nix/home-manager.nix` with this structure:

```nix
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
  hasCodexHooks = lib.hasAttrByPath [ "programs" "codex" "hooks" ] options;
  codexEnabled = lib.attrByPath [ "programs" "codex" "enable" ] false config;
  executable = lib.getExe cfg.package;
  refreshCommand = "${executable} --json 2>/dev/null || true";
in
{
  _file = ./home-manager.nix;

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
      default = codexEnabled;
      defaultText = lib.literalExpression "config.programs.codex.enable";
      description = "Whether to merge codexctl lifecycle hooks into programs.codex.hooks.";
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
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
          assertion = !cfg.codexHooks.enable || codexEnabled;
          message = "programs.codexctl.codexHooks.enable requires programs.codex.enable = true";
        }
      ];
    }
    (lib.mkIf (cfg.codexHooks.enable && hasCodexHooks) {
      programs.codex.hooks = {
        PermissionRequest = lib.mkAfter [
          {
            matcher = "Bash";
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
                command = refreshCommand;
                timeout = 5;
              }
            ];
          }
        ];
        Stop = lib.mkAfter [
          {
            matcher = "";
            hooks = [
              {
                type = "command";
                command = refreshCommand;
                timeout = 5;
              }
            ];
          }
        ];
      };
      home.activation.codexctlHookTrustNotice = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        echo "codexctl hooks use ${executable}; restart Codex and review /hooks after package changes."
      '';
    })
  ]);
}
```

Keep the activation entry message-only. Do not run `codexctl init`, mutate Codex state, or bypass hook trust.

- [ ] **Step 4: Run the complete evaluation fixture green**

Keep the exact assertions from Step 1: all three commands use `expectedExe`, the existing Stop hook remains first, empty alias settings omit `xdg.configFile."codexctl/config.toml"`, unsupported/disabled integrations expose their focused failed assertions, and no successful configuration defines `systemd.user.services.codexctl-headless`.

Run the targeted check first:

```bash
nix build .#checks.x86_64-linux.home-manager-module -L
```

Expected: the real activation package and `codexctl-home-manager-module-check` build successfully; compatibility assertions pass during evaluation.

- [ ] **Step 5: Format Nix and re-run the module check**

Run:

```bash
nix fmt
nix fmt -- --check
nix flake check -L
```

Expected: formatter check has no diff and the flake check remains green.

- [ ] **Step 6: Review the Task 2 diff without changing jj history**

Run:

```bash
jj --no-pager diff --git flake.nix flake.lock nix/home-manager.nix nix/tests/home-manager-module.nix
jj --no-pager st
```

Expected: package/dev-shell behavior is preserved, the Home Manager input follows `nixpkgs`, and no service is introduced.

---

### Task 3: Document, Verify, and Hand Off the Declarative Setup

**Files:**
- Modify: `README.md`
- Modify: `docs/configuration.md`
- Verify: `.internal/specs/2026-07-16-home-manager-module-design.md`
- Verify: `.internal/plans/2026-07-16-home-manager-module.md`

**Interfaces:**
- Consumes: the exported module API and immutable hook behavior from Tasks 1-2.
- Produces: copy-paste Home Manager usage, trust/security guidance, and complete Nix/Rust verification evidence.

**Acceptance Criteria:**
- Documentation shows importing `inputs.codexctl.homeManagerModules.default`, enabling `programs.codex`, configuring `programs.codexctl.settings.brain`, and rebuilding Home Manager.
- Documentation states that settings are Nix-store-visible, terminal fallback remains false, and `/hooks` trust must be reviewed after codexctl package changes.
- Documentation does not advertise or create a headless service.
- Nix formatting/checks, focused Rust tests, all workspace tests, Clippy with warnings denied, and workspace build pass under isolated test homes where persistence is involved.
- The live Brain Review `project == "test"` count is unchanged by the isolated verification run.
- A final independent review finds no unresolved correctness or security issue in option compatibility, hook composition, immutable commands, trust behavior, or service absence.
- Final jj inspection shows only the approved module, tests, docs, spec, plan, and lockfile changes; nothing is pushed.

- [ ] **Step 0: Enter a dedicated Task 3 jj change**

Run `jj --no-pager st`. If `@` is empty because the Stop hook already advanced, run:

```bash
jj desc -r @ -m "📝 docs: document Home Manager setup"
```

If `@` still contains Task 2, run:

```bash
jj new -m "📝 docs: document Home Manager setup"
```

Verify with `jj --no-pager st` before editing.

- [ ] **Step 1: Add the public Home Manager usage example**

Add a concise `## Home Manager` section to `docs/configuration.md`:

```nix
{
  imports = [ inputs.codexctl.homeManagerModules.default ];

  programs.codex.enable = true;
  programs.codexctl = {
    enable = true;
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
```

Explain that Home Manager merges the three codexctl handlers into `programs.codex.hooks`, uses the selected package's immutable store path, and prints a reminder to restart Codex and review `/hooks` after upgrades. Warn that `settings` must not contain secrets because generated TOML is stored in the Nix store.

- [ ] **Step 2: Link the module from the README**

Add a short Nix/Home Manager bullet near setup/configuration in `README.md` linking to `docs/configuration.md#home-manager`. State that the module exports both `homeManagerModules.default` and `homeModules.default` without repeating the full example.

- [ ] **Step 3: Validate documentation terminology**

Run:

```bash
rg -n "homeManagerModules.default|programs.codexctl|terminal_auto_approve_fallback|/hooks|Nix store" README.md docs/configuration.md
rg -n "codexctl-headless|systemd.user.services" README.md docs/configuration.md nix flake.nix
```

Expected: the first command finds every required concept; the second command produces no new service documentation or module definition.

- [ ] **Step 4: Capture the live review baseline, then run focused regressions with isolated HOME**

Record the live count without modifying the store:

```bash
jq -s '[.[] | select(.project == "test")] | length' \
  /home/alexander/.codexctl/brain/decisions.jsonl
```

If the file is absent, record the baseline as zero.

Run:

```bash
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib init::hooks::tests
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --lib doctor::tests
nix fmt -- --check
nix flake check -L
```

Expected: all selected Rust tests and every Nix check pass.

- [ ] **Step 5: Run full workspace gates**

Run:

```bash
cargo fmt --all --check
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo test --workspace
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo clippy --workspace --all-targets --all-features -- -D warnings
HOME="$(mktemp -d)" CARGO_HOME=/home/alexander/.cargo cargo build --workspace
```

Expected: formatting is clean; all tests pass; Clippy emits no warnings; every workspace crate builds.

- [ ] **Step 6: Recheck live review state and obtain a final independent review**

Repeat the exact `jq` count from Step 4 and assert it matches the baseline. Ask a fresh reviewer to inspect the final diff against the approved spec and plan, focusing on:

- missing or recursive Home Manager options;
- duplicate or reordered hooks;
- permissive command classification;
- mutable/PATH-based hook execution;
- secret-bearing examples;
- accidental headless-service creation.

Expected: the live count is unchanged and the reviewer returns PASS with no unresolved Critical, Important, or Minor findings.

- [ ] **Step 7: Inspect flake outputs and the final jj scope**

Run:

```bash
nix flake show
jj --no-pager diff --stat
jj --no-pager diff --git
jj --no-pager st
jj --no-pager log -r 'ancestors(@, 8)' --no-graph
```

Expected: flake output includes both Home Manager module aliases, the formatter, and the module check; the jj working copy remains `✨ feat: add Home Manager module`; no unrelated files changed.

- [ ] **Step 8: Close completed Beads work and hand off**

Close the implementation tasks and epic only after their acceptance criteria and all gates pass. Close brainstorming task `codexctl-ryf` after the implementation epic references it through a `discovered-from` dependency. Report changed files, exact validation, the immutable-hook trust behavior, the task-level jj changes created by the Stop-hook-aware workflow, and that nothing was squashed or pushed.

## Stress Test Results: Home Manager Implementation Plan

### Resolved Decisions

- Preserve the Stop-hook-created planning stack, describe each planning change accurately, and use the current empty change for implementation rather than squashing history.
- Count only the missing absolute-current and refresh-idempotence Rust tests as red; record stale/lookalike behavior as already-green characterization coverage.
- Make the Nix red test reach an empty module scaffold and fail on the missing `programs.codexctl` behavior rather than a missing file.
- Give the module a stable `_file` identity and prove importing both aliases does not duplicate hooks.
- Use the targeted x86_64-linux module check for the fast loop and retain flake-wide checks as gates.
- Prove isolated tests do not alter live Brain Review state and require a fresh final security/correctness review.
- Re-check jj state at every task boundary, preserve Stop-hook-created changes, and give each task an accurate description before editing.

### Changes Made

- Added pre-execution jj description normalization through the commit-message workflow.
- Corrected the Rust and Nix red/green sequences.
- Added dual-alias import and exact hook-count assertions.
- Added targeted Nix build, live review baseline, and independent final-review steps.
- Replaced the invalid single-change assumption with an explicit task-per-change jj workflow.

### Deferred / Parking Lot

- Downstream `nix-configs` adoption remains a separate explicitly authorized change.
- Optional headless service and runtime secret injection remain outside this implementation.

### Confidence Assessment

- Overall: High.
- Areas of concern: Home Manager hook normalization and assertion timing are covered by the real activation-package check; any version-specific mismatch must be fixed in the fixture/module without weakening its behavioral assertions.
