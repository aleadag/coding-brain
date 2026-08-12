# Research: Lifecycle-hook latency boundaries

> **Date:** 2026-08-11
> **Bead:** codexctl-9j39s.1
> **Status:** Complete

## Summary

Codex gives the lifecycle executable two seconds, but the executable has no whole-hook budget or stage timing. The clearest confirmed defect is project identity: every applied event resolves the Git root, and repositories without a project manifest run a second synchronous Git command; both children are uncapped. The next step is privacy-safe stage instrumentation followed by installed-release measurement, then a bounded fix for the measured dominant stage rather than a timeout increase.

## Key Findings

### The 500 ms storage deadline is not the hook deadline

> **Confidence:** high — independently verified from the hook definition, call path, and storage deadline implementation.

Codex registers SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, SubagentStart, and SubagentStop with a two-second timeout [S1]. After input parsing, `run_provider_with_sqlite` creates one absolute 500 ms `StorageDeadline`, opens SQLite, then reuses that database for lifecycle and activity persistence [S2]. The deadline governs storage operations and SQLite busy callbacks; it does not cancel the process, input read, parent discovery, project identity, or an already-entered commit [S3].

The production call order is:

1. process startup and CLI dispatch;
2. bounded-size input read and lifecycle parse;
3. SQLite migration/security preflight and open;
4. live-parent discovery;
5. lifecycle projection and durable commit;
6. project identity discovery;
7. PostToolUse lookup/correlation when applicable;
8. activity durable commit.

### Project identity is an unbounded post-lifecycle stage

> **Confidence:** high — independently verified from the applied-event path and both Git helpers.

An applied lifecycle event builds an activity observation, which calls `ProjectIdentity::load` [S4]. `ProjectIdentity::load` always resolves the project root with `git rev-parse --show-toplevel`; when `.coding-brain/project.toml` is missing it also resolves `git remote get-url origin` [S5]. Both use synchronous `Command::output()` without a wall-clock timeout or output cap [S5]. A normal repository without a manifest can therefore execute two uncapped Git children after the lifecycle transaction has already committed. PostToolUse diagnostic construction can repeat project identity work.

This is a confirmed boundedness defect, but the current timeout incidents cannot yet be attributed to it as the dominant stage because release 0.59.1 emits no stage timing and Codex kills the process at two seconds. The design must preserve that distinction.

### Durability constrains the safe optimization

> **Confidence:** high — independently verified from lifecycle, activity, schema, and commit-helper code.

Lifecycle and activity are separate SQLite transactions using `synchronous=FULL` [S6]. Each checks the stored deadline before entering commit, while `commit_before_deadline` deliberately treats a reported successful commit as authoritative even when the wall clock crosses the deadline [S7]. A fix must not preempt fsync, reinterpret a successful commit as failure, drop the activity write, or report a partial lifecycle/activity pair as fully successful.

### Existing measurements do not satisfy the regression requirement

> **Confidence:** high — verified from the ignored integration smoke and a local installed-release probe.

The existing ignored `warm_lifecycle_hook_latency_and_roundtrip` test reports only end-to-end p50/p95, includes storage preparation in its timed region, uses a non-Git cwd, and contains a stale Stop-hook expectation [S8]. It cannot attribute a stage or deterministically exercise a deadline overrun.

The installed executable was confirmed as `cbrain 0.59.1`. The current live SQLite file is approximately 19 MB. A private copied-state probe showed warm admitted synthetic events completing below GNU `time`'s 10 ms display resolution, while duplicate events took the ignored fast path. The copy also exposed a sub-second cold/storage-deadline diagnostic, but copying a live SQLite state is not a controlled timeout-case fixture, so that observation is not used to identify the dominant stage. Raw syscall tracing was denied by the execution security policy. No raw hook payload, command, secret, real session identifier, or sensitive path was added to the research artifact.

## Comparisons

| Approach | Attribution | Hard bound | Durability risk | Verdict |
|---|---:|---:|---:|---|
| Raise Codex timeout | none | no | masks overruns | reject |
| End-to-end timing only | low | no | low | insufficient |
| Privacy-safe stage timings, then bound measured stage | high | yes | controllable | recommended |
| Skip activity or weaken SQLite durability | misleading | apparent only | unacceptable | reject |

## Codebase Context

- Input is capped at 64 KiB, but read-to-EOF has no internal wall-clock deadline.
- Linux parent discovery is depth-bounded; other Unix platforms may invoke bounded `ps` helpers per ancestor.
- SQLite busy waiting uses one absolute deadline, but individual filesystem syscalls and entered commits are not preempted.
- `provider_hooks::run_bounded_process` already supplies an in-tree process-group, timeout, output-cap, cleanup, and reaper pattern suitable for Git child supervision.
- PostToolUse database reads are row- and byte-bounded before in-memory correlation.

## Recommendations

1. Add an opt-in, privacy-safe lifecycle timing sink whose closed stage names cover startup/input, parent discovery, SQLite open/preflight, lifecycle write/commit, project identity, PostToolUse correlation, and activity write/commit. Record only provider, event class, outcome class, elapsed duration, and closed diagnostic codes.
2. Make the timer injectable so deterministic tests advance a fake clock and controlled stage seams without fixed sleeps.
3. Use the timing build against healthy and controlled timeout fixtures on the installed release path and a production-sized database before selecting the dominant-stage fix.
4. If project identity is dominant, reuse the existing bounded-process supervision pattern with a small shared hook budget and an output cap; preserve the current temporary-identity fallback only for a clean timeout/failure, with an explicit diagnostic.
5. Keep lifecycle and activity transactions ordered and durable. Add contention, missing storage, subprocess timeout, and uncertain-commit tests before changing the production path.

## Recommended Beads

No additional bead is needed yet; design, implementation, and verification belong to parent `codexctl-9j39s`.

## Open Questions

- Which stage dominates real two-second failures after privacy-safe timing is installed?
- What internal whole-hook budget leaves adequate process startup and diagnostic headroom below Codex's two-second kill boundary?
- Should stage timing be emitted only on threshold crossings, or sampled for healthy baselines as well?

## Refuted / Discarded Claims

- “SQLite's 500 ms deadline bounds the whole hook” — false; it is an absolute storage deadline stored on the database and does not supervise non-storage stages.
- “The uncapped Git child is already proven to dominate current incidents” — not yet supported; it is a confirmed defect and leading hypothesis, but stage evidence is absent.
- “Increasing the provider timeout fixes the bug” — false; it leaves the synchronous path unbounded and violates the bead's acceptance criteria.

## Sources

- [S1: Codex hook definitions](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/init/provider_hooks/codex.rs:5) — Primary source — 2026-08-11 — lifecycle and recovery timeouts.
- [S2: lifecycle SQLite call path](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/lifecycle_hook.rs:962) — Primary source — 2026-08-11 — deadline creation and ordered persistence stages.
- [S3: storage deadline](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/brain/storage/mod.rs:162) — Primary source — 2026-08-11 — absolute deadline and busy-handler behavior.
- [S4: activity observation project load](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/lifecycle_hook.rs:452) — Primary source — 2026-08-11 — applied-event project identity call.
- [S5: project identity Git helpers](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/crates/coding-brain-core/src/project.rs:59) — Primary source — 2026-08-11 — root and remote discovery through synchronous children.
- [S6: lifecycle transaction](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/brain/storage/lifecycle.rs:75) — Primary source — 2026-08-11 — lifecycle persistence order.
- [S7: authoritative commit helper](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/src/brain/storage/activity.rs:869) — Primary source — 2026-08-11 — pre-commit deadline and post-commit authority.
- [S8: ignored latency smoke](/home/alexander/hacking/aleadag/coding-brain/.worktrees/perf-9j39s/tests/lifecycle_hook_cli.rs:1644) — Primary source — 2026-08-11 — current end-to-end measurement limitations.
