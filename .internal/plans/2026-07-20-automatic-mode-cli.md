# Automatic Mode CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Replace overlapping Brain and configuration flags with one global `off|on|auto` mode and one `coding-brain config ...` command namespace.

**Architecture:** Resolve mode centrally from the existing writable gate-mode state, with legacy TOML fields used only when explicit state is absent. Route config commands, permission hooks, and the read-only TUI header through that resolver; remove process-local Brain flags, the TUI mode mutation, and the old top-level config flags.

**Tech Stack:** Rust 2024, Clap derive, `tempfile` atomic persistence, Ratatui, Cargo integration tests.

## Global Constraints

- `coding-brain` without a subcommand continues to launch the Brain TUI.
- The only public model control is `coding-brain config get|set mode`.
- Existing config display, template, validation, and initialization operations move under `coding-brain config`.
- The TUI header reports mode but offers no keybinding that changes it.
- Mode is global and persists in writable XDG state, not in Home Manager-managed TOML.
- `off` disables model evaluation, but never bypasses deterministic safety checks or lifecycle recording.
- Explicit state values are exactly `off`, `on`, or `auto`; invalid or unreadable state fails closed to `off`.
- Legacy `[brain].enabled` and `[brain].auto` are fallback inputs only when explicit state is absent.
- All mode writes are atomic and use last-writer-wins semantics.
- Add no new dependency; `tempfile` is already present.

---

### Task 1: Centralize mode resolution and atomic persistence

**Files:**
- Modify: `crates/coding-brain-core/src/runtime.rs`
- Modify: `crates/coding-brain-tui/src/brain_app.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs`
- Modify: `src/brain/mod.rs`
- Modify: `src/runtime/brain.rs`
- Modify: `src/brain/permission_hook.rs`

**Interfaces:**
- Produces: `brain::GateModeResolution { mode: BrainGateMode, warning: Option<String> }`
- Produces: `brain::resolve_gate_mode(config: Option<&BrainConfig>) -> GateModeResolution`
- Produces: `brain::write_gate_mode(mode: BrainGateMode) -> io::Result<()>`
- Produces: crate-visible path-injected resolver and writer helpers for tests
- Consumes: `coding_brain_core::runtime::BrainGateMode`

**Acceptance Criteria:**
- Valid explicit state wins over all legacy values.
- Without explicit state, legacy `enabled` and `auto_mode` map to `off`, `on`, or `auto`; no Brain config maps to `off`.
- Invalid or unreadable explicit state resolves to `off` with a warning and is not overwritten.
- Every writer stores an explicit complete value atomically, including `on`.
- Permission hooks and the TUI source use the same resolver.
- The TUI no longer exposes the `g` mode-changing shortcut or `BrainActions::set_gate_mode`.
- `off` leaves deterministic safety checks and lifecycle recording unchanged.

- [ ] **Step 1: Write failing resolver tests in `src/brain/mod.rs`**

Add deterministic path-injected tests:

```rust
#[test]
fn explicit_mode_wins_over_legacy_config() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gate-mode");
    std::fs::write(&path, "auto").unwrap();
    let legacy = BrainConfig {
        enabled: false,
        auto_mode: false,
        ..BrainConfig::default()
    };

    let resolved = resolve_gate_mode_at(&path, Some(&legacy));

    assert_eq!(resolved.mode, BrainGateMode::Auto);
    assert!(resolved.warning.is_none());
}

#[test]
fn missing_state_uses_legacy_config_then_defaults_off() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gate-mode");
    let advisory = BrainConfig {
        enabled: true,
        auto_mode: false,
        ..BrainConfig::default()
    };
    let automatic = BrainConfig {
        enabled: true,
        auto_mode: true,
        ..BrainConfig::default()
    };

    assert_eq!(resolve_gate_mode_at(&path, None).mode, BrainGateMode::Off);
    assert_eq!(
        resolve_gate_mode_at(&path, Some(&advisory)).mode,
        BrainGateMode::On
    );
    assert_eq!(
        resolve_gate_mode_at(&path, Some(&automatic)).mode,
        BrainGateMode::Auto
    );
}

#[test]
fn invalid_explicit_state_fails_closed_without_rewriting() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gate-mode");
    std::fs::write(&path, "automatic").unwrap();

    let resolved = resolve_gate_mode_at(&path, Some(&BrainConfig::default()));

    assert_eq!(resolved.mode, BrainGateMode::Off);
    assert!(resolved.warning.as_deref().unwrap().contains("automatic"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "automatic");
}
```

- [ ] **Step 2: Run resolver tests and verify RED**

Run:

```bash
cargo test --bin coding-brain brain::tests::explicit_mode_wins -- --nocapture
cargo test --bin coding-brain brain::tests::missing_state_uses -- --nocapture
cargo test --bin coding-brain brain::tests::invalid_explicit_state -- --nocapture
```

Expected: compilation fails because `GateModeResolution` and
`resolve_gate_mode_at` do not exist.

- [ ] **Step 3: Implement typed resolution**

Use the shared runtime enum and a result that preserves diagnostic context:

```rust
use coding_brain_core::runtime::BrainGateMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateModeResolution {
    pub mode: BrainGateMode,
    pub warning: Option<String>,
}

fn legacy_gate_mode(config: Option<&BrainConfig>) -> BrainGateMode {
    match config {
        Some(config) if !config.enabled => BrainGateMode::Off,
        Some(config) if config.auto_mode => BrainGateMode::Auto,
        Some(_) => BrainGateMode::On,
        None => BrainGateMode::Off,
    }
}
```

`resolve_gate_mode_at` distinguishes `NotFound` from other read failures. It
accepts only trimmed `off`, `on`, and `auto`; invalid UTF-8, permissions, empty
content, and unknown values return `Off` with a warning.

- [ ] **Step 4: Run resolver tests and verify GREEN**

Run:

```bash
cargo test --bin coding-brain brain::tests -- --nocapture
```

Expected: all Brain mode resolver tests pass.

- [ ] **Step 5: Write a failing atomic-writer test**

Prove `on` is explicit and replacing a value leaves only a complete canonical
value:

```rust
#[test]
fn writer_persists_every_mode_explicitly() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("brain/gate-mode");

    write_gate_mode_at(&path, BrainGateMode::On).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "on\n");

    write_gate_mode_at(&path, BrainGateMode::Auto).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "auto\n");
}
```

- [ ] **Step 6: Run the writer test and verify RED**

Run:

```bash
cargo test --bin coding-brain brain::tests::writer_persists -- --nocapture
```

Expected: compilation fails because `write_gate_mode_at` does not exist.

- [ ] **Step 7: Implement atomic mode replacement**

Create the parent directory, write `mode.as_str()` plus a newline to a sibling
`tempfile::NamedTempFile`, flush it, and call `persist(path)`. Do not delete the
file for `on`; absence is reserved for legacy/default resolution.

- [ ] **Step 8: Run the writer tests and verify GREEN**

Run:

```bash
cargo test --bin coding-brain brain::tests -- --nocapture
```

Expected: all resolver and writer tests pass.

- [ ] **Step 9: Write failing runtime and permission-hook integration tests**

In `src/runtime/brain.rs`, replace unknown-to-on parsing coverage with a test
that injects a resolved mode. In `src/brain/permission_hook.rs`, add a case with
`mode = off` and an inference closure that panics if called, while the existing
deterministic destructive-command case still denies before the mode gate. Add
a TUI key test proving `g` has no mode-changing effect and remove it from the
rendered help.

- [ ] **Step 10: Run focused integration tests and verify RED**

Run:

```bash
cargo test --bin coding-brain runtime::brain::tests::gate_mode -- --nocapture
cargo test --bin coding-brain permission_hook::tests::mode_off -- --nocapture
cargo test -p coding-brain-tui brain_app -- --nocapture
```

Expected: at least one assertion fails because runtime parsing defaults invalid
state to `on` and the hook still uses independent string/config gates.

- [ ] **Step 11: Route runtime and hook code through the resolver**

Load the same resolved configuration in `LiveBrainSource::gate_mode` and the
permission-hook entry point. Remove duplicated mode parsing and direct writes.
Delete `BrainActions::set_gate_mode`, its live and mock implementations, and
the `g` key branch from `BrainApp`.

Keep deterministic safety evaluation before the `Off` model gate. When model
mode is `Off`, abstain to Codex's normal permission flow after safety checks.

- [ ] **Step 12: Run focused integration tests and verify GREEN**

Run:

```bash
cargo test --bin coding-brain runtime::brain::tests -- --nocapture
cargo test --bin coding-brain permission_hook -- --nocapture
cargo test -p coding-brain-tui -- --nocapture
```

Expected: all runtime and permission-hook tests pass.

### Task 2: Consolidate configuration under one subcommand

**Files:**
- Modify: `src/main.rs`
- Modify: `src/commands.rs`

**Interfaces:**
- Produces: Clap `Command::Config { action: ConfigAction }`
- Produces: Clap `ConfigAction::{Show, Get, Set, Template, Validate, Init}`
- Produces: `commands::run_config_get(cfg: &Config, key: &str) -> io::Result<()>`
- Produces: `commands::run_config_set(key: &str, value: &str) -> io::Result<()>`
- Consumes: Task 1 resolver and atomic writer

**Acceptance Criteria:**
- `coding-brain config set mode off|on|auto` writes explicit state and exits.
- `coding-brain config get mode` reports the effective mode and any fail-closed diagnostic.
- Unsupported config keys and mode values return errors without writing.
- `--brain`, `--auto-run`, and `--mode` fail Clap parsing and disappear from help.
- `--config`, `--config-template`, `--config-validate`, and `--config-init` are replaced by config subcommands.
- Plain `coding-brain` remains the only interactive launch path.

- [ ] **Step 1: Write failing Clap contract tests in `src/main.rs`**

```rust
#[test]
fn persistent_mode_uses_config_subcommand() {
    let cli = Cli::try_parse_from(["coding-brain", "config", "set", "mode", "auto"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Config {
            action: ConfigAction::Set { ref key, ref value }
        }) if key == "mode" && value == "auto"
    ));
}

#[test]
fn overlapping_brain_flags_are_removed() {
    for flag in [
        "--brain",
        "--auto-run",
        "--mode",
        "--config",
        "--config-template",
        "--config-validate",
        "--config-init",
    ] {
        assert!(Cli::try_parse_from(["coding-brain", flag]).is_err(), "{flag}");
    }
}
```

Move all three flags from `RETAINED_ARGS` to `REMOVED_ARGS`.

- [ ] **Step 2: Run CLI parser tests and verify RED**

Run:

```bash
cargo test --bin coding-brain brain_only_cli_tests -- --nocapture
```

Expected: the config subcommand types do not exist and the old flags still parse.

- [ ] **Step 3: Add nested config commands and remove old fields**

```rust
#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    Show,
    Get { key: String },
    Set { key: String, value: String },
    Template,
    Validate,
    Init,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    // Preserve the existing Init, Completions, Man, and Doctor variants.
}
```

Delete `Cli::brain`, `Cli::auto_run`, `Cli::mode`, `Cli::config`,
`Cli::config_template`, `Cli::config_validate`, and `Cli::config_init`. Remove
the Brain flag branches from `apply_brain_cli_overrides`, `run_brain_query`,
insights validation, and the old `run_brain_mode` dispatcher. Update endpoint
and model help so it no longer claims `--brain` is required; explicit endpoint
and model overrides do not control gate mode.

Dispatch `Show`, `Template`, `Validate`, and `Init` through the existing
`Config::print_resolved`, `Config::print_template`, validation, and
`write_config_init` paths without changing their underlying behavior.

- [ ] **Step 4: Write failing command-handler tests in `src/commands.rs`**

Use path-injected helpers to avoid real XDG state:

```rust
#[test]
fn config_set_mode_rejects_unknown_value_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gate-mode");

    let error = set_mode_at(&path, "automatic").unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(!path.exists());
}

#[test]
fn config_get_mode_reports_fail_closed_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("gate-mode");
    std::fs::write(&path, "broken").unwrap();

    let report = mode_report_at(&path, Some(&BrainConfig::default()));

    assert!(report.contains("mode: off"));
    assert!(report.contains("config set mode <off|on|auto>"));
}
```

- [ ] **Step 5: Run command-handler tests and verify RED**

Run:

```bash
cargo test --bin coding-brain commands::tests::config_ -- --nocapture
```

Expected: compilation fails because the config command helpers do not exist.

- [ ] **Step 6: Implement strict get and set handlers**

Support exactly the key `mode`. Parse values into `BrainGateMode`; reject any
other key or value with `io::ErrorKind::InvalidInput`. `get` uses the resolver
and includes its warning plus the corrective command when present. Dispatch
the subcommand after loading config and before all interactive or continuous
modes.

- [ ] **Step 7: Run focused CLI and command tests and verify GREEN**

Run:

```bash
cargo test --bin coding-brain brain_only_cli_tests -- --nocapture
cargo test --bin coding-brain commands::tests::config_ -- --nocapture
```

Expected: all focused tests pass.

### Task 3: Verify the process contract and migrate public documentation

**Files:**
- Create: `tests/config_mode_cli.rs`
- Modify: `src/config.rs`
- Modify: `nix/tests/home-manager-module.nix`
- Modify: `README.md`
- Modify: `docs/configuration.md`
- Modify: `docs/index.md`
- Modify: `docs/quickstart.md`
- Modify: `docs/reference.md`

**Interfaces:**
- Consumes: Task 1 mode resolver and compatibility mapping
- Consumes: Task 2 `config get|set mode` process interface
- Produces: binary-level regressions using `CARGO_BIN_EXE_coding-brain`

**Acceptance Criteria:**
- Binary tests prove every config subcommand exits without entering the TUI and shares isolated XDG state where applicable.
- Binary tests prove all seven replaced top-level flags fail parsing.
- New templates and docs omit `[brain].enabled` and `[brain].auto`.
- Legacy fields remain parse-only fallback inputs when explicit state is absent.
- Documentation explains global scope, persistent mode, default-off behavior, and always-on deterministic safety.
- Full repository quality gates pass.

- [ ] **Step 1: Write failing binary-level CLI tests**

Create `tests/config_mode_cli.rs` with an isolated process helper:

```rust
use std::process::Command;

fn command(temp: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_coding-brain"));
    command
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("config"))
        .env("XDG_STATE_HOME", temp.path().join("state"))
        .env("CODING_BRAIN_SKIP_FIRST_RUN", "1");
    command
}

#[test]
fn config_set_then_get_mode_exits_and_round_trips() {
    let temp = tempfile::tempdir().unwrap();
    let set = command(&temp)
        .args(["config", "set", "mode", "auto"])
        .output()
        .unwrap();
    assert!(set.status.success(), "{}", String::from_utf8_lossy(&set.stderr));

    let get = command(&temp)
        .args(["config", "get", "mode"])
        .output()
        .unwrap();
    assert!(get.status.success(), "{}", String::from_utf8_lossy(&get.stderr));
    assert_eq!(String::from_utf8_lossy(&get.stdout), "mode: auto\n");
}

#[test]
fn removed_mode_flags_fail_at_process_boundary() {
    let temp = tempfile::tempdir().unwrap();
    for flag in [
        "--brain",
        "--auto-run",
        "--mode",
        "--config",
        "--config-template",
        "--config-validate",
        "--config-init",
    ] {
        assert!(!command(&temp).arg(flag).output().unwrap().status.success());
    }
}
```

- [ ] **Step 2: Run binary tests and verify RED**

Run:

```bash
cargo test --test config_mode_cli -- --nocapture
```

Expected: the set/get test fails because `config` is not a recognized subcommand
and the old flags still parse.

- [ ] **Step 3: Complete legacy parsing and template migration**

Retain raw parsing of `enabled` and `auto` so Task 1 can resolve old installs.
Remove both keys from `Config::template_string`, resolved-config output, and
new setup output. Do not emit a warning on every launch for these compatibility
fields; an explicit mode state supersedes them without modifying read-only
Home Manager files.

Add config tests for all three legacy mappings and for explicit-state
precedence. Keep project/user layering tests focused on endpoint, model,
timeouts, and test runners.

Update `nix/tests/home-manager-module.nix` so its representative managed config
no longer emits `enabled`, `auto`, or `terminal_auto_approve_fallback`; retain
the endpoint, model, timeout, and unrelated-setting preservation assertions.

- [ ] **Step 4: Run config and binary tests and verify GREEN**

Run:

```bash
cargo test --bin coding-brain config::tests -- --nocapture
cargo test --test config_mode_cli -- --nocapture
```

Expected: both test commands pass.

- [ ] **Step 5: Update public documentation**

Use this activation flow consistently:

```text
coding-brain config set mode on
coding-brain
```

Document `off`, `on`, and `auto`; state that mode is global and persists after
the settings command exits. Explain that `off` disables model evaluation while
deterministic safety and lifecycle recording remain active. Remove examples of
`--brain`, `--auto-run`, `--mode`, `enabled = ...`, and `auto = ...` from active
configuration and Home Manager documentation. Replace the four old config
flags with `config show`, `config template`, `config validate`, and
`config init`. Remove the TUI `g` key from help and usage documentation.

- [ ] **Step 6: Run documentation consistency searches**

Run:

```bash
rg -n -- '--brain|--auto-run|--mode|--config-template|--config-validate|--config-init|enabled = |auto = ' README.md docs src crates tests nix
rg -n 'config (show|template|validate|init)|config (get|set) mode|mode (off|on|auto)' README.md docs src tests
```

Expected: the first command finds only deliberate removed-argument tests,
legacy parser tests, internal hook flag names such as `--brain-query`, or
historical internal documents. The second finds the new help, tests, and docs.

- [ ] **Step 7: Run full repository quality gates**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
nix build .#checks.x86_64-linux.home-manager-module
```

Expected: every command exits 0 with no failed tests or Clippy warnings.

- [ ] **Step 8: Review the final jj diff and status**

Run:

```bash
jj --no-pager diff --git
jj --no-pager st
jj --no-pager log -r '@|@-' --no-graph
```

Expected: the implementation changeset contains only the single-mode CLI,
tests, migration documentation, and related plan/spec updates, with an emoji
conventional description naming `codexctl-twn`.

## Stress Test Results: Automatic Mode CLI

### Resolved Decisions

- Mode remains global because the existing TUI and permission gate supervise all local sessions.
- `--auto-run` is removed instead of gaining temporary lease semantics.
- `--brain`, `[brain].enabled`, and `[brain].auto` leave the active interface; one `off|on|auto` mode replaces them.
- The agreed command remains `coding-brain config get|set mode`; its storage is writable XDG state so it works with read-only Home Manager configuration.
- Legacy enablement fields map to the new mode only when explicit state is absent; a fresh install defaults to `off`.
- Corrupt or unreadable explicit state fails closed to `off` and is never overwritten automatically.
- `off` does not bypass deterministic safety or lifecycle recording.
- All writers use atomic explicit values with last-writer-wins concurrency.
- Binary-level tests cover command dispatch and exit behavior in addition to parser and unit tests.
- The TUI header is read-only; the `g` mode-changing shortcut is removed.
- Existing config flags move into `config show|template|validate|init`, giving configuration one command namespace.

### Changes Made

- Removed the file-lock lease design and the separate `brain.enabled` config writer.
- Replaced three implementation tasks with central resolution, CLI replacement, and process/documentation verification tasks.
- Added legacy fallback, default-off, fail-closed corruption, atomic write, global-scope, and deterministic-safety requirements.
- Added TUI mode-mutation removal, config namespace consolidation, and Home Manager fixture coverage.

### Deferred / Parking Lot

- Project- or session-scoped automatic mode requires a future hook identity protocol.
- Removal of legacy TOML parsing can happen after the compatibility window.
- Hook management may move from top-level and hidden flags into a future `coding-brain hook ...` subcommand.

### Confidence Assessment

- Overall: High
- Areas of concern: legacy project-level enablement can differ until an explicit global mode is written; documentation must make the global migration path clear.
