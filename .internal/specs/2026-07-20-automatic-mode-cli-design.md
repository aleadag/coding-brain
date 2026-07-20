# Automatic Mode CLI Design

## Problem

Coding Brain currently exposes several overlapping controls:

- `--brain` enables model evaluation only in the current process, although
  permission hooks run as independent processes;
- `--auto-run` changes only in-memory configuration, while hooks and the TUI
  header read persisted gate state;
- `--mode` changes persistent state and exits even though it looks like a
  launch option;
- `[brain].enabled` and `[brain].auto` duplicate the gate-mode state.

This is why `coding-brain --auto-run` can still display `advisory` and behave
advisorially.

## Public Contract

Coding Brain has one global model mode and one settings interface:

```text
coding-brain
coding-brain config show
coding-brain config set mode off
coding-brain config set mode on
coding-brain config set mode auto
coding-brain config get mode
coding-brain config template
coding-brain config validate
coding-brain config init
```

Running `coding-brain` always opens the TUI. `config set` changes the mode and
exits; every `config` action is non-interactive. `show`, `template`, `validate`,
and `init` replace the old `--config`, `--config-template`,
`--config-validate`, and `--config-init` flags. The top-level `--brain`,
`--auto-run`, and `--mode` options are also removed. New configuration no
longer exposes `[brain].enabled` or `[brain].auto`.

The `config` command names the product's effective configuration rather than a
specific TOML file. Mode remains in the existing writable XDG state file, so
the command works when Home Manager provides `config.toml` through a read-only
Nix store symlink.

## Mode Semantics

- `off` disables local-model evaluation and automatic approval.
- `on` enables advisory model evaluation. Deterministic safety denials remain
  executable.
- `auto` allows high-confidence model approvals as well as denials.

Deterministic safety checks and lifecycle recording remain active in every
mode. User-facing text describes `off` as `model off`; it does not claim that
all Brain safeguards are disabled.

The mode is global because the TUI and existing gate state supervise all local
Coding Brain hook activity. The header is read-only: the TUI's `g` shortcut is
removed, so entering `auto` requires the explicit settings command.
Project- or session-scoped automatic behavior would require a separate hook
identity protocol and is outside this change.

## Persistence and Resolution

`config set mode` uses one atomic replacement function. The file always
contains an explicit `off`, `on`, or `auto`; `on` is no longer represented by
deleting the file. Concurrent commands are last-writer-wins, while readers see
a complete old or new value.

Resolution uses this order:

1. a valid explicit gate-mode state value;
2. legacy `[brain].enabled` and `[brain].auto` values when explicit state is
   absent;
3. `off` when neither source exists.

The legacy mapping is:

| Legacy values | Mode |
| --- | --- |
| `enabled = false` | `off` |
| `enabled = true`, `auto = false` | `on` |
| `enabled = true`, `auto = true` | `auto` |

Legacy fields remain parse-only during the compatibility window. They are not
shown in new templates or documentation. An explicit `config set mode ...`
value always wins and does not modify declaratively managed TOML.

An invalid or unreadable explicit state file fails closed to `off`.
`config get mode` reports the problem and prints the corrective
`coding-brain config set mode <off|on|auto>` command. Coding Brain does not
overwrite damaged state automatically.

## Verification

Unit tests cover explicit resolution, legacy fallback, the default-off case,
invalid and unreadable state, and atomic persistence. Runtime and
permission-hook tests prove the TUI header and hooks use the same resolver.
Binary-level tests with isolated XDG directories prove that every `config`
action exits without entering the TUI and that the replaced top-level flags are
rejected. TUI tests prove the mode header is read-only. Public documentation
uses only the consolidated config interface.
