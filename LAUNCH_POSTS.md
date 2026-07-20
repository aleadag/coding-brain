# Launch posts

## Positioning

Coding Brain is a local TUI for the judgment and learning loop around Codex. Live shows what needs attention, Review turns corrections into teaching evidence, and Scorecard makes decision quality visible. It can switch to a session through native terminal support or optional Agent Deck navigation.

It does not schedule work or replace a durable tracker. Beads and Agent Deck are optional companions for different jobs.

## GitHub Discussion

Title:

`coding-brain`: local judgment and learning for Codex

Body:

I built Coding Brain because the useful part of supervising Codex is not another session launcher. It is seeing which decision needs attention, correcting it quickly, and retaining that preference for the next session.

The TUI has three views:

- Live for current activity and attention
- Review for denials, corrections, and uncertain decisions
- Scorecard for decision quality over time

It reads local Codex hook and transcript evidence. Switching to a session uses the terminal directly or Agent Deck when that optional integration owns the session.

```bash
cargo install coding-brain
coding-brain init
coding-brain doctor
coding-brain
```

Repo: https://github.com/aleadag/codexctl

## Short post

Coding Brain is a local judgment and learning TUI for Codex: Live shows what needs attention, Review captures corrections, and Scorecard tracks whether decisions improve.

It can switch to sessions through native terminal support or optional Agent Deck, but it deliberately leaves durable task tracking to external tools.

```bash
cargo install coding-brain
coding-brain init
coding-brain doctor
coding-brain
```

https://github.com/aleadag/codexctl

## Show HN

Title:

Show HN: Coding Brain - local judgment and learning for Codex

Body:

Coding Brain is a terminal UI for reviewing the decisions around active Codex sessions. Hook events make activity visible immediately, transcript evidence adds context, and operator corrections become learning evidence.

The primary views are Live, Review, and Scorecard. The product does not include a scheduler, mailbox, or distributed coordinator. Session switching uses supported terminal APIs, with Agent Deck available as an optional attach path.

The default brain endpoint is local, and remote endpoints produce visible privacy advisories.

Repo: https://github.com/aleadag/codexctl

## Assets

- `docs/assets/github-social-preview.png`
- `docs/assets/codexctl-demo-hero.gif`
