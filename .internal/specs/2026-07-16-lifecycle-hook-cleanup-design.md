# Lifecycle Hook Cleanup Design

**Issue:** `codexctl-9am`  
**Status:** Approved  
**Date:** 2026-07-16

## Problem

codexctl installs `PostToolUse` and `Stop` handlers that run:

```text
codexctl --json 2>/dev/null || true
```

`codexctl --json` writes a session snapshot array to standard output. Codex treats exit status zero with empty output as hook success, but parses non-empty output as an event-specific hook response object. The snapshot array therefore causes both handlers to fail with invalid JSON output. Redirecting standard error and forcing a successful exit do not suppress standard output.

The command also has no durable lifecycle effect: it performs a one-shot session scan, prints it, and exits. It neither updates a running codexctl process nor persists the hook event.

## Decision

Remove codexctl's ineffective `PostToolUse` and `Stop` snapshot handlers. Keep the `PermissionRequest` handler unchanged. The broader `codexctl-rqm` work may reintroduce lifecycle hooks only through a dedicated handler that consumes the documented hook input and persists useful lifecycle state.

This fix does not add a replacement CLI mode, event store, or background communication path.

## Ownership Boundary

codexctl owns a handler only when its executable is exactly bare `codexctl` or an absolute path ending in `/codexctl` and its arguments match a recognized codexctl hook form.

Event membership does not imply ownership. In particular, `codex-jj-stop-hook` is independently configured by Home Manager and has no relationship to codexctl. It and all other unrelated handlers must remain structurally unchanged.

## Imperative Migration

`src/init/hooks.rs` will:

- stop adding codexctl `PostToolUse` and `Stop` handlers;
- keep recognizing the existing bare and absolute `--json` command forms, with or without the exact redirection suffix, as legacy managed handlers;
- remove only those legacy handlers during `init` and `uninit`;
- retain non-codexctl handlers in the same matcher group;
- remove an empty matcher group or event only when cleanup leaves it empty; and
- update initialization output so it advertises only the installed `PermissionRequest` hook.

Running `codexctl init` upgrades an existing imperative installation by removing the two invalid legacy handlers and retaining every unrelated hook.

## Declarative Migration

`nix/home-manager.nix` will stop contributing `PostToolUse` and `Stop` entries to `programs.codex.hooks`. The existing `programs.codexctl.codexHooks.enable` option remains compatible and controls the remaining `PermissionRequest` integration.

On the next Home Manager rebuild, the module-generated lifecycle handlers disappear. Hooks supplied by other modules, including `codex-jj-stop-hook`, remain owned and rendered by those modules.

The option description, module assertions, tests, and user documentation will describe the narrower hook set.

## Failure and Security Behavior

- Removing these no-op handlers eliminates their invalid output and per-tool process overhead.
- Permission evaluation and native allow or deny responses remain unchanged.
- Terminal fallback remains blocked whenever a managed permission hook is configured, including stale or disabled forms.
- Cleanup uses exact executable and argument matching; similarly named programs, relative paths, extra arguments, and unrelated hooks are preserved.
- No automatic merge, push, deployment, or external state mutation is introduced.

## Verification

Focused Rust tests will prove that:

- fresh init installs `PermissionRequest` without codexctl `PostToolUse` or `Stop` handlers;
- init removes all supported legacy snapshot command forms;
- cleanup preserves unrelated handlers in mixed matcher groups;
- cleanup preserves an independently configured Stop hook;
- uninit remains idempotent; and
- PermissionRequest discovery and migration behavior are unchanged.

The Home Manager evaluation will prove that:

- enabling codexctl hooks contributes only the absolute-path `PermissionRequest` handler;
- an independently configured Stop hook remains present and unchanged;
- no codexctl `PostToolUse` or `Stop` entry is generated; and
- compatibility assertions and package selection still behave as before.

Final validation includes focused hook and doctor tests, the full Rust test suite, formatting, Clippy with warnings denied, the workspace build, Nix formatting, and `nix flake check`.

## Rollout

- Imperative users run `codexctl init`, then review the changed hook configuration through `/hooks` if Codex requests renewed trust.
- Declarative users rebuild their Home Manager configuration.
- No data migration or rollback step is required. Reverting the package restores the previous generated definitions, although those definitions will again produce invalid hook output.

