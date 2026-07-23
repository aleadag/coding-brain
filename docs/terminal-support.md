# Terminal support

Run `coding-brain doctor` inside the same terminal environment that launches Codex, Claude Code, or Antigravity CLI. The report separates Agent Deck navigation, Claude native attach, guarded semantic input, and focus-only fallback.

Terminal focus and terminal input are different capabilities. Focus requires an exact source target. Input additionally requires a current provider process identity and unique pane; automatic semantic input also requires a complete versioned prompt, immediate recapture, and post-action verification.

## Navigation matrix

| Terminal | Switch to session | Setup |
| --- | --- | --- |
| Ghostty | Yes | macOS Automation/Accessibility permission |
| Kitty | Yes | `allow_remote_control yes` in `kitty.conf` |
| tmux | Yes | Coding Brain must reach the same tmux server |
| WezTerm | Yes | Reachable `wezterm cli` mux server |
| Warp | Yes | macOS Automation/Accessibility permission |
| iTerm2 | Yes | macOS Automation/Accessibility permission |
| Terminal.app | Yes | macOS Automation/Accessibility permission |
| GNOME Terminal | No | Launch-only backend; switch is not exposed |
| Windows Terminal from WSL | No | Launch-only bridge; remote tab control is not exposed |

Coding Brain restores the terminal before it hands control to an external attach command and re-enters the TUI after that command returns. These terminal backends provide focus; guarded input specifically uses a reachable tmux pane with exact process ancestry.

## Provider action paths

| Provider | Preferred structured action | Native attach | Guarded fallback |
| --- | --- | --- | --- |
| Codex | `PermissionRequest` allow/deny | None | tmux allow, deny, continue, or explicit literal text |
| Claude Code | `PermissionRequest` allow/deny | `claude attach <id>` for exact background identities | tmux allow, deny, continue, or explicit literal text |
| Antigravity CLI | `PreToolUse` allow/deny/ask and `Stop` continue | None | tmux for process-only, manual, or uncovered prompts |

Automatic recovery runs only in Brain `auto` mode and only after current Stop and prompt evidence is reserved and revalidated. Antigravity can receive `continue` through its structured Stop response. Codex and Claude use guarded terminal `continue`. Manual Live actions use `x`, then `a`, `d`, `c`, or `t`; an unknown prompt can be focused or answered with explicit manual text, but it never triggers an automatic semantic action.

## Optional Agent Deck

When selected Live activity has an exact Agent Deck target, the TUI can attach through Agent Deck's tmux workflow. Matching is provider-qualified; a same-named session from another provider is not a fallback. Agent Deck is optional, and a missing installation or cancelled attach leaves the Brain TUI usable.

Use `coding-brain doctor` for concrete setup advice when a supported terminal cannot be reached.
