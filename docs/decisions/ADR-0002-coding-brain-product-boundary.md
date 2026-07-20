# ADR-0002: Make Brain the Coding Brain Product Boundary

- Status: Accepted
- Date: 2026-07-17
- Bead: `codexctl-0cy`

## Context

Codexctl combines two product directions. Its dashboard discovers and manages
terminal sessions, while Brain evaluates actions, records outcomes and
corrections, and learns operator preferences. Agent Deck and other terminal
tools already cover session management, so keeping both directions would make
codexctl compete with a dedicated session manager while obscuring its more
distinctive judgment and learning work.

The existing Brain screen is an overlay inside the dashboard, and the public
CLI still exposes session listing, spawning, routing, messaging, and
termination. State and configuration also use the `codexctl` name even though
the intended product is Coding Brain. This is a pre-release project with one
tester, so compatibility machinery would cost more than a manual reset.

The approved design and its twelve-branch stress test are recorded in the
[Coding Brain design](https://github.com/aleadag/codexctl/blob/main/.internal/specs/2026-07-17-brain-primary-tui-design.md).

## Decision

The product is renamed **Coding Brain**, with `coding-brain` as its only
executable. It does not install `cb` or retain a `codexctl` compatibility
executable. The later Rust package, crate, and source-directory rename is now
adopted under the accepted
[Coding Brain crate namespace design](https://github.com/aleadag/codexctl/blob/main/.internal/specs/2026-07-20-coding-brain-crate-namespace-design.md);
the GitHub repository remains named `codexctl`.

Brain becomes the default and only interactive TUI in `coding-brain-tui`. It has
three tabs: Live, Review, and Scorecard. Live is an attention-first view over
persisted activity rather than a session list. Review remains the teaching
queue, and Scorecard remains the quality and safety view.

Coding Brain owns judgment and learning, not session management:

- Its executable decisions are allow and deny. It may abstain so Codex shows
  its native prompt.
- The dashboard, mailbox, general session views, and send, terminate, route,
  spawn, resume, and similar management commands are removed.
- Codex session discovery remains internal evidence. Public TUI and runtime
  contracts receive Brain projections and an opaque navigation target, not a
  general session collection.
- Switching to the source session is explicit operator navigation. Agent Deck
  is an optional adapter invoked through its public CLI only after the operator
  presses `Enter`; a terminal-focus backend remains the fallback.

The runtime is hook-first. Independent permission and lifecycle hook processes
evaluate requests and append activity whether or not the TUI is open. The TUI
is a cockpit over persisted state, and `--headless` is the only continuous
evaluator. Coding Brain adds no daemon or background service.

Activity uses an append-only lifecycle in `activity.jsonl`, while
`decisions.jsonl` retains model proposals and learning evidence. Cross-process
append and compaction share one lock. Deterministic safety denies run before
inference and still deny if audit persistence fails; a model-derived decision
abstains unless its proposal and committed activity were persisted. The exact
proposal, commitment, delivery, execution, corruption-recovery, and preference
publication semantics are defined in
[ADR-0003](ADR-0003-fail-safe-hook-and-learning-persistence.md).

Model endpoints may be local or remote, but only a CLI flag or user-level
configuration can select one. Project configuration cannot redirect model
traffic, and non-loopback endpoints remain visibly identified.

All public persistent paths use the Coding Brain namespace:

- `$XDG_CONFIG_HOME/coding-brain/config.toml`, falling back to
  `~/.config/coding-brain/config.toml`;
- `$XDG_STATE_HOME/coding-brain/`, falling back to
  `~/.local/state/coding-brain/`;
- `.coding-brain.toml` for project configuration;
- `.coding-brain/` for project identity and generated project memory.

Coding Brain does not read, write, or migrate `.codexctl.toml`,
`~/.config/codexctl`, or `~/.codexctl`. It leaves old data untouched unless the
operator explicitly requests the documented `coding-brain init --purge`
cleanup. This supersedes only the persistent-path compatibility decision in
[ADR-0001](ADR-0001-lifecycle-hooks-as-status-evidence.md); ADR-0001's bounded,
status-only lifecycle evidence and authorization boundaries remain accepted.

Dream is reserved as a future local-model reflection capability. Its canonical
memory will be a Brain-owned typed ledger keyed by the stable project UUID in
`.coding-brain/project.toml`; `.coding-brain/MEMORY.md` and optional Beads
memories are projections. Repository, transcript, tool, fetched, and model text
cannot activate durable memory without trusted corroboration.

## Rationale

The narrower boundary gives Coding Brain one job that remains useful across
terminal managers: judge agent actions, expose uncertain or incorrect
decisions, and learn from outcomes and corrections. Removing session-management
surfaces avoids duplicating Agent Deck while retaining the one cross-product
interaction the operator needs, namely jumping from a denied or disputed
decision to its source session.

Hook-first persistence keeps evaluation independent of TUI uptime without
introducing a daemon. Separating the complete activity log from the resolved
learning set preserves auditability without turning every failed or malformed
attempt into training data. Stable project identity and typed Dream records
leave room for durable memory without making Beads mandatory or allowing
untrusted project content to become instructions.

A clean namespace break is simpler and more honest than aliases, compatibility
reads, or a migration command for a single pre-release tester. XDG paths also
separate configuration from mutable state before those locations become a
public compatibility promise.

## Consequences

- Existing dashboard and session-management users must use Agent Deck or
  another terminal manager. Agent Deck remains optional for Coding Brain.
- Existing `.codexctl` configuration and state are not loaded automatically.
  Wanted data must be copied manually or regenerated; old files otherwise stay
  untouched.
- The implementation must replace the root TUI, remove obsolete CLI and runtime
  surfaces, install `coding-brain` hook commands, and rename persistent paths.
  The executable rename happens last in an unreleased stack of compiling jj
  changesets.
- Live needs bounded attention projection, explicit resolution, duplicate
  collapse, overflow handling, and terminal-safe external navigation.
- Hook and persistence tests must cover multi-process writes, compaction,
  deterministic-deny failure behavior, distillation recovery, redaction, and
  project identity across clones and worktrees.
- Dream commands, reflection prompts, retrieval, and Beads publication remain
  outside the current implementation.
