# Launch posts

## Positioning

Coding Brain is a local TUI for the judgment and learning loop around Codex, Claude Code, and Antigravity CLI. Live shows what needs attention, Review turns corrections into teaching evidence, Scorecard makes decision quality visible, and Diagnostics exposes metadata-only correlation and activity-store evidence. It can switch to an exact source session through native Claude attach, terminal support, or optional Agent Deck navigation.

It does not schedule work or replace a durable tracker. Beads and Agent Deck are optional companions for different jobs.

## GitHub Discussion

Title:

`coding-brain`: local judgment and learning for coding agents

Body:

I built Coding Brain because the useful part of supervising coding agents is not another session launcher. It is seeing which decision needs attention, correcting it quickly, and retaining that preference for the next session.

The TUI has four views:

- Live for current activity and attention
- Review for denials, corrections, and uncertain decisions
- Scorecard for decision quality over time
- Diagnostics for read-only correlation and activity-store evidence

It reads structured hooks and process evidence from Codex, Claude Code, and Antigravity, plus Codex rollout transcripts. Switching to a source session uses native Claude attach, the terminal directly, or Agent Deck when that optional integration owns the session.

```bash
cargo install coding-brain
coding-brain init all
coding-brain doctor
coding-brain
```

Repo: https://github.com/aleadag/coding-brain

## Short post

Coding Brain is a local judgment and learning TUI for Codex, Claude Code, and Antigravity: Live shows what needs attention, Review captures corrections, Scorecard tracks whether decisions improve, and Diagnostics exposes metadata-only correlation and activity-store evidence.

It can switch to sessions through native terminal support or optional Agent Deck, but it deliberately leaves durable task tracking to external tools.

```bash
cargo install coding-brain
coding-brain init all
coding-brain doctor
coding-brain
```

https://github.com/aleadag/coding-brain

## Show HN

Title:

Show HN: Coding Brain - local judgment and learning for coding agents

Body:

Coding Brain is a terminal UI for reviewing decisions around active Codex, Claude Code, and Antigravity sessions. Hook events make activity visible immediately, provider evidence adds context, and operator corrections become learning evidence.

The primary views are Live, Review, Scorecard, and Diagnostics. The product does not include a scheduler, mailbox, or distributed coordinator. Session switching uses supported terminal APIs, with Agent Deck available as an optional attach path.

The default brain endpoint is local, and remote endpoints produce visible privacy advisories.

Repo: https://github.com/aleadag/coding-brain

## Assets

- `docs/assets/github-social-preview.png`
