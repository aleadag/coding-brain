# Launch Posts

## Positioning

codexctl is a local-brain companion for people running several Codex sessions.
It keeps session state, health, context pressure, and pending actions visible in
one terminal. Decisions are advisory by default; `--auto-run` is an explicit
opt-in for high-confidence actions.

Durable task tracking and multi-agent coordination are intentionally left to
external tools such as Beads.

## GitHub Discussion

Title:

`codexctl`: a local brain for your Codex sessions

Body:

I kept losing track of which Codex session was blocked, waiting for approval,
or quietly consuming context, so I built `codexctl`.

It gives me one local dashboard to:

- see active sessions and their health
- review pending decisions without tab hunting
- apply deterministic rules before consulting a local LLM
- learn from corrections
- optionally execute high-confidence actions with `--auto-run`

Quick start:

```bash
brew install aleadag/tap/codexctl
codexctl init
codexctl
```

Repo: https://github.com/aleadag/codexctl

## Short Post

I built `codexctl` because supervising several Codex sessions should not
require tab hunting.

It is a local terminal dashboard with a learning brain: it shows session health
and pending actions, stays advisory by default, and can act on high-confidence
decisions when you opt in with `--auto-run`.

```bash
brew install aleadag/tap/codexctl
codexctl init
codexctl
```

https://github.com/aleadag/codexctl

## Show HN

Title:

Show HN: codexctl – a local brain for supervising Codex sessions

Body:

If you run several Codex sessions at once, `codexctl` shows which ones are
blocked, waiting for approval, unhealthy, or approaching context limits.

The brain combines deterministic rules with a local LLM and learns from
operator corrections. It only advises by default; automatic execution requires
`--auto-run`.

The project is local-first and supports macOS and Linux terminals including
Ghostty, tmux, Kitty, Warp, iTerm2, and GNOME Terminal.

Repo: https://github.com/aleadag/codexctl

## Assets

- `docs/assets/github-social-preview.png`
- `docs/assets/codexctl-demo-hero.gif`
