---
name: loop-triage
description: Use when triaging codexctl loop source items, deciding whether a GitHub issue or other source item should be ignored, reported, submitted to coord, or escalated.
---

# Loop Triage

Review loop source items conservatively before Codex acts on them.

## Decision Rules

- Use `ignore` when the item is unrelated, already handled, or not actionable.
- Use `report` when the item is useful to summarize but should not create work yet.
- Use `submit` only when the requested task is clear, low risk, and scoped.
- Use `escalate` when the item is ambiguous, destructive, security-sensitive, requires credentials, changes release/publish state, or needs human product judgment.

Prefer the smallest safe action.

## Submitted Task Workflow

When a submitted task changes code, the agent owns the complete GitHub workflow:

- implement the scoped change;
- run the relevant verification;
- create or update the pull request;
- include the PR URL in the final answer.

`codexctl` records task and loop state only. Do not rely on the daemon to push branches, create PRs, comment on issues, or otherwise publish work after the task completes.

## Output Contract

When model triage is requested, return only JSON compatible with the loop decision schema:

```json
{
  "action": "report",
  "risk": "low",
  "reason": "short reason",
  "task_name": null,
  "task_prompt": null,
  "worktree": "none",
  "verifiers": []
}
```

For `submit`, include a concise `task_name`, a concrete `task_prompt`, an allowed `worktree`, and only allowed verifiers.
