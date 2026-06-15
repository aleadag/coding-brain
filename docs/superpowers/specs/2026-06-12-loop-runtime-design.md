# Loop Runtime Design

Date: 2026-06-12

## Goal

Add a first-class loop runtime to `codexctl`.

A loop repeatedly watches one or more sources, remembers what it has seen,
uses policy or a model-guided triage skill to decide what should happen, and
submits safe actionable work to the existing coordination executor.

The initial design should support the shape described by the loop-engineering
reference project: report-only loops, assisted loops, optional worktrees,
skills, verifier gates, durable state, budget limits, observability, and human
handoff.

## Current State

The codebase already has useful execution pieces:

- `coord` has a task ledger with states, attempts, verifier definitions,
  retries, resume, cancellation, and human escalation.
- `codexctl supervisor submit/run/status/logs/cancel` can write and inspect
  coord tasks.
- `codexctl --headless` ticks the supervisor and emits structured events.
- `bus` has a SQLite mailbox for delivering work to already-running sessions.
- Skill discovery scans Codex user, plugin, and project skill locations.
- Terminal launch support can start visible Codex sessions.

The missing pieces are:

- A loop definition format.
- Source polling and cursor management.
- Durable loop item state and dedupe.
- Model-guided item triage within a constrained policy envelope.
- Optional worktree preparation before coord task submission.
- A concrete headless spawn path for coord tasks.

## Chosen Approach

Add a `loop` subsystem above `coord`.

The loop runtime owns source discovery, dedupe, triage, policy, budgets, and
item state. When it decides that an item should be acted on, it submits a
`coord::tasks::NewTask`. The coord supervisor remains responsible for session
launch, attempts, verifiers, retries, resume, and terminal/human state.

This keeps the boundary simple:

- `loop` answers: what should we look at, what does it mean, and is action
  allowed?
- `coord` answers: how do we safely run an agent task to completion?

Do not build a second executor inside `loop`.

## Architecture

Add binary-crate modules:

```text
src/loop/
  mod.rs
  cli.rs
  config.rs
  store.rs
  sources/
    mod.rs
    github_issues.rs
    shell.rs
  policy.rs
  prompt.rs
  submit.rs
  worktree.rs
```

The first implementation should keep provider-specific source adapters in the
binary crate. No `codexctl-core` changes are required unless the TUI needs a
shared type later.

Runtime flow:

```text
loop definition files
  -> loop runtime
      -> source adapters fetch normalized items
      -> SQLite loop state dedupes and records items
      -> deterministic and model-guided policy decide item action
      -> actionable items become coord tasks
  -> coord supervisor
      -> optional worktree cwd
      -> Codex session spawn
      -> verifier gates
      -> retry, resume, escalation, cancellation
```

## Loop Definition Files

Use one file per loop.

Project-local loops:

```text
.codexctl/loops/issue-triage.toml
```

Global loops may be added later:

```text
~/.config/codexctl/loops/email-triage.toml
```

V1 should support project-local TOML files. The design should leave room for
global loop files, but implementation can defer global discovery unless it is
cheap to include.

Example:

```toml
name = "issue-triage"
enabled = true
mode = "assisted" # report | assisted | unattended
cadence = "2h"

[source]
kind = "github_issues"
repo = "aleadag/codexctl"
query = "is:open label:codexctl-loop"
limit = 5

[triage]
mode = "model" # deterministic | model
skill = "loop-triage"
instructions = """
Review each source item and decide whether Codex should act.
Prefer report-only when requirements are unclear.
Require a worktree for repo-changing tasks.
Escalate security-sensitive or destructive work.
Return only structured JSON matching the allowed decision schema.
"""

[triage.allowed]
actions = ["ignore", "report", "submit", "escalate"]
worktree = ["none", "existing", "required"]
verifiers = ["cargo test", "cargo clippy -- -D warnings"]

[execution]
cwd = "."
worktree = "auto" # none | existing | required | auto
worktree_root = "../codexctl-worktrees"
branch_template = "loop/{loop_name}/{source_id_slug}"
session = "headless" # headless | terminal
model = "default"
budget_usd = 3.00
max_retries = 2
timeout_min = 90

[[verify]]
kind = "run"
command = "cargo test"

[[verify]]
kind = "run"
command = "cargo clippy -- -D warnings"

[gates]
max_items_per_run = 2
max_concurrent = 1
require_human_for = ["destructive", "ambiguous", "security-sensitive"]
```

### Mode Semantics

- `report`: fetch and record items, emit a summary, never create coord tasks.
- `assisted`: submit only items that policy marks low-risk and allowed.
- `unattended`: submit automatically unless blocked by gates.

### Worktree Semantics

- `none`: no repo isolation; useful for email, reports, or pure connector work.
- `existing`: use configured `cwd` directly.
- `required`: create or reuse one isolated worktree per source item.
- `auto`: model-guided triage may choose among allowed worktree modes.

### Markdown Loop Files

V1 should parse TOML only. A future `.md` format can wrap the same config model
with frontmatter and a fenced TOML block. Markdown should remain a source format
for humans, not a separate runtime state system.

## SQLite State

Use SQLite as the runtime source of truth. Prefer a sibling loop database under
`~/.codexctl/loop/loop.db` so the schema can evolve independently from
`coord.db` and `bus.db`.

Core tables:

```text
loop_runs
  id
  loop_name
  config_path
  started_at
  finished_at
  status              # running | success | partial | failed
  items_seen
  items_submitted
  items_ignored
  error

loop_sources
  loop_name
  source_key
  cursor_json
  updated_at

loop_items
  id
  loop_name
  source_kind
  source_item_id
  dedupe_key
  title
  body_summary
  url
  raw_json
  state               # seen | ignored | reported | submitted | escalated | done | failed
  decision_json
  coord_task_id
  worktree_path
  first_seen_at
  last_seen_at
  updated_at
  last_error

loop_events
  id
  loop_name
  run_id
  item_id
  level               # info | warn | error
  event_type
  message
  data_json
  created_at
```

`loop_items` needs a unique key on `(loop_name, source_kind, source_item_id)`.
This is the dedupe guarantee that prevents scheduled runs from creating the
same task repeatedly.

Stable source item ids:

- GitHub issue: `github:aleadag/codexctl#123`
- Email: provider message or thread id
- Bus inbox: bus message id

State flow:

```text
source item fetched
  -> seen or deduped
  -> triage policy runs
      -> ignored
      -> reported
      -> escalated
      -> submitted
  -> coord task runs separately
  -> loop item mirrors terminal coord status as done or failed
```

`coord_task_id` is the bridge to the execution ledger.

## Source Adapters

Source adapters fetch provider-specific data and update cursors. They should
not decide whether work is safe or executable.

Minimal contract:

```rust
trait LoopSource {
    fn source_key(&self) -> String;
    fn fetch(&self, cursor: Option<Value>, limit: usize) -> Result<FetchResult, String>;
}

struct FetchResult {
    items: Vec<SourceItem>,
    next_cursor: Option<Value>,
}

struct SourceItem {
    source_kind: String,
    source_item_id: String,
    title: String,
    body: String,
    url: Option<String>,
    raw_json: Value,
}
```

Initial sources:

- `github_issues`: fetch issues with `gh issue list --repo <repo> --search
  <query> --json number,title,body,url,labels,updatedAt`.
- `shell`: run a configured command that emits normalized JSON lines. This is
  the escape hatch for email, custom trackers, or private sources before native
  adapters exist.

Later sources:

- `email`: provider-specific or command-backed adapter.
- `bus_inbox`: read existing bus messages from the bus DB or JSON CLI.

## Model-Guided Triage

Use model capability at the policy and triage layer, not as the raw fetcher or
runtime.

Deterministic code owns:

- Fetching from configured sources.
- Credential and command boundaries.
- Cursors and rate-limit state.
- Dedupe keys.
- SQLite state transitions.
- Budget and concurrency limits.
- Worktree creation.
- Coord task submission.

The model may decide, within an enforced envelope:

- `ignore`
- `report`
- `submit`
- `escalate`
- task name and prompt
- risk label and reason
- worktree mode, when `execution.worktree = "auto"`
- verifier commands selected from an allowlist

Decision shape:

```json
{
  "action": "submit",
  "risk": "low",
  "reason": "Clear repo-scoped bug with testable acceptance criteria.",
  "task_name": "Fix issue #123: preserve context window",
  "task_prompt": "Use skill loop-triage. Inspect issue #123...",
  "worktree": "required",
  "verifiers": ["cargo test", "cargo clippy -- -D warnings"]
}
```

Validation rules:

- Unknown action: escalate.
- Unknown verifier: escalate or drop the verifier, depending config.
- Worktree mode outside the allowlist: escalate.
- Missing task prompt for `submit`: fail item.
- `mode = "report"`: never submit, even if the model asks.
- Required skill missing: fail loop validation before processing.

This gives the loop a `/loop`-style natural-language policy surface while
keeping safety and repeatability in code.

## Prompt Generation

Generated task prompts should include:

- Loop name.
- Source kind and stable source id.
- Source title, body summary, URL, and relevant raw metadata.
- Required skill name.
- Model triage reason.
- Success criteria.
- Worktree policy.
- Verifier commands.
- Explicit connector-writeback constraints.

The prompt should tell Codex to use the named skill, but should not inline skill
bodies into loop state or source item rows.

## Worktree Execution

Loop runtime prepares execution context before submitting a coord task.

For `worktree = "required"`:

```text
loop item source id
  -> stable slug
  -> worktree path
  -> coord task cwd = worktree path
```

Worktree names should be deterministic so reruns resume the same item instead
of creating duplicate directories.

VCS rules:

- Detect jj repos via `.jj/`.
- For jj repos, use jj-safe commands and avoid raw Git mutation that can corrupt
  jj state.
- For plain Git repos, use `git worktree`.
- If a worktree already exists, verify it points at the expected repo/item
  before reuse.
- If the worktree is dirty or unexpected, escalate unless config explicitly
  allows reuse.

Task submission:

```text
loop decision says submit
  -> resolve execution mode
  -> prepare cwd or worktree
  -> create coord::tasks::NewTask
  -> set loop_items.coord_task_id
  -> state = submitted
```

`coord` must implement the currently stubbed headless spawn path:

- `session = "headless"` should spawn `codex exec` in the task cwd.
- `session = "terminal"` may use existing terminal launch support.
- `coord` records `task_attempts.session_id` and owns retries/resume.

## CLI

Add a `codexctl loop` command group:

```bash
codexctl loop list
codexctl loop validate [<name>]
codexctl loop run <name> [--dry-run] [--limit N]
codexctl loop tick [--name <loop>] [--json]
codexctl loop status [<name>]
codexctl loop logs <name> [--item <id>]
codexctl loop pause <name>
codexctl loop resume <name>
codexctl loop export <name> --format md
```

`run` is one-shot. It fetches sources, triages items, submits allowed tasks,
prints a summary, and exits.

`export --format md` can produce `STATE.md`-style reports for human review, but
Markdown exports are not the runtime database.

## Scheduling

V1 should rely on external schedulers:

```text
systemd timer or cron
  -> codexctl loop tick --json

codexctl --headless --json
  -> coord supervisor executes submitted tasks
```

`tick` discovers project-local loop configs, skips disabled, paused, and
not-due loops, submits accepted work, reconciles completed loop-submitted coord
tasks, and exits. `run <name>` remains the manual single-loop command.

Loop config may include `cadence`. Later commands can generate timers from it:

```bash
codexctl loop install-timer <name>
```

Do not require a custom sleep loop for the normal production path. A foreground
`codexctl loop daemon` can remain a secondary option if users need one process
that interprets cadence internally.

## Safety Gates

Code must enforce:

- Disabled loops cannot run unless explicitly forced.
- Missing required skill fails validation.
- Model decisions outside configured allowlists escalate.
- Report mode never creates coord tasks.
- Already submitted or done items do not duplicate work.
- Budget and concurrency limits defer items.
- Dirty or unexpected worktrees escalate.
- Source updates, comments, PR creation, or merge actions are disabled unless
  explicitly enabled.

Initial connector writeback should be read-only. Automatic GitHub comments,
labels, PR creation, or email sends should be future work behind explicit
config.

Human handoff:

- Escalated items remain in `loop_items`.
- `loop status` and `loop logs` show why.
- A future `codexctl loop approve <item-id>` can submit a previously escalated
  item.

## Out of Scope

The first implementation will not:

- Parse Markdown loop files.
- Implement native email adapters.
- Auto-comment on GitHub issues.
- Auto-create or auto-merge PRs.
- Interpret schedules inside a long-lived loop daemon.
- Let the model fetch arbitrary remote data directly.
- Replace coord with a second loop-specific executor.
- Treat markdown state files as the source of truth.

## Testing

Add focused tests for:

- Loop TOML parsing and validation.
- Missing skill validation.
- SQLite migrations and state transitions.
- Source item dedupe.
- Fake source adapter fetches.
- Model decision validation and allowlist enforcement.
- Report mode blocking task submission.
- Worktree mode resolution.
- Coord task creation from a submitted loop item.

Use fake source and fake model-policy implementations for integration tests so
tests do not require GitHub, email, or live model access.

## Verification

Success criteria for v1:

- `codexctl loop list` finds `.codexctl/loops/*.toml`.
- `codexctl loop validate <name>` catches missing skills and invalid allowlists.
- `codexctl loop run <name> --dry-run` shows fetched/deduped/triaged items
  without writing coord tasks.
- A fake source run creates exactly one `loop_items` row for a stable source id
  across repeated runs.
- A submit decision creates one coord task and stores its id on the loop item.
- Report mode never creates coord tasks.
- `cargo fmt` passes.
- `cargo test` passes.
- `cargo clippy -- -D warnings` passes, or any remaining failures are
  documented as pre-existing or explicitly out of scope.

## References

- Loop engineering reference:
  https://github.com/cobusgreyling/loop-engineering
- Loop engineering `LOOP.md`:
  https://github.com/cobusgreyling/loop-engineering/blob/main/LOOP.md
- Loop design checklist:
  https://github.com/cobusgreyling/loop-engineering/blob/main/docs/loop-design-checklist.md
