# ADR-0003: Make Hook and Learning Persistence Fail-Safe

- Status: Accepted
- Date: 2026-07-17
- Bead: `codexctl-0cy.1.1`

## Context

Coding Brain evaluates permission requests in short-lived hook processes. The
TUI may be closed, several hooks may append concurrently, and a process may die
at any write boundary. The implementation also has two records with different
purposes: `decisions.jsonl` retains model proposals and learning evidence, while
`activity.jsonl` drives Live, Review, Scorecard, and lifecycle audit.

Those writes cannot form one filesystem transaction with the hook response
pipe. For example, Coding Brain can persist an allow decision and then fail to
write stdout, or it can write stdout and die before recording that delivery.
Calling either case “executed” would overstate the evidence and could train the
Brain on an action Codex never ran.

The same partial-publication problem exists in learning maintenance. A
distillation run produces global and per-project preference files. Replacing
them in place can expose a mixed generation after a crash. An interrupted JSONL
append can also leave a partial tail that corrupts the next otherwise valid
event.

The implementation plan and its eleven-branch stress test are recorded in the
[Coding Brain implementation plan](https://github.com/aleadag/codexctl/blob/main/.internal/plans/2026-07-17-coding-brain-product-boundary.md).

## Decision

### Separate proposal, commitment, delivery, and execution

`decisions.jsonl` stores model proposals and learning evidence.
`activity.jsonl` is the authoritative decision-commit and lifecycle audit. Both
records use the same stable `decision_id` and `activity_id` correlation.

Permission requests first take a nonblocking lock for the exact provider
identity and request key. One same-request invocation becomes the winner;
contenders perform no state mutation, model inference, or response. Requests
with different keys use independent lock shards and may proceed concurrently.

Before a model-derived allow or deny can leave the hook process, Coding Brain
publishes an immutable transaction journal containing the bounded proposal,
terminal activity, lifecycle identity, request key, and intended disposition.
It then must:

1. persist the decision proposal;
2. persist lifecycle-schema-v4 authority bound to the exact transaction ID and
   `Allow` or `Deny` action;
3. persist the matching terminal `Allowed` or `Denied` activity;
4. verify all three destinations against the journal;
5. remove the completed journal and durably sync that removal;
6. write the serialized response to stdout; and
7. append `Delivered` or `DeliveryFailed` best-effort.

Failure of a required destination or final verification before stdout prevents
the model decision from being emitted. Recovery without matching executable
authority records `NeedsInput` and terminal `Error`; it never reconstructs an
`Allowed` event from a proposal or a bare legacy `Decided` value. Deterministic
code-owned denies still run before inference and fail closed when audit stores
are unavailable, after the same request has won guarded admission.

`Allowed` and `Denied` mean that Coding Brain committed the hook decision. They
do not prove Codex received it or ran the tool. A committed decision without a
delivery event projects as `DeliveryUnknown`; a failed stdout write projects as
`DeliveryFailed`. Only later lifecycle or outcome evidence may claim that the
tool executed.

### Recover interrupted permission transactions without replaying responses

Permission transaction journals live under
`$XDG_STATE_HOME/coding-brain/brain/permission-transactions/`. On Unix, the
directory is mode `0700`, journal files are mode `0600`, and ownership is bound
to the current effective user. Journal discovery is bounded, oldest-first, and
retains active, malformed, newer-schema, oversized, or otherwise uncertain
evidence rather than deleting it.

Brain refresh and `cbrain doctor` attempt bounded recovery before projecting
activity. Recovery may complete durable proposal, authority, and terminal
evidence or compensate to `NeedsInput` and `Error`; it never writes the original
provider response. Doctor reports active work separately and fails for invalid,
over-budget, unresolved, removal-sync-uncertain, or recovery-error conditions.
For an over-budget result it exposes only a fixed source label and numeric
limit, never stored content or a caller-controlled path.

Lifecycle schema v4 adds the exact transaction/action authority map. Schema-v3
bare decisions migrate as diagnostic evidence only and remain non-executable.
A permission hook preserves corrupt or newer lifecycle evidence for recovery
and diagnosis instead of replacing it or attempting fallback writes.

Stale `Observed` or `Evaluating` permission activity projects in Live as
`Incomplete` with `permission evaluation timed out`. `Incomplete` is a
projection-only presentation state: it is never serialized to `activity.jsonl`
and elapsed time alone does not prove that the hook died. A later durable
terminal event can still resolve the activity.

### Repair append-only state and publish learning atomically

Activity append and compaction use the same exclusive lock. Before accepting a
new append, the writer inspects bytes after the final newline. It completes a
valid unterminated JSON value, or truncates an invalid fragment to the last
complete newline and records only the discarded byte count. It never copies the
raw fragment into a diagnostic. Readers continue past malformed complete lines
and report bounded offsets.

Distillation writes a complete immutable preference generation under a new
generation ID. It flushes every global and project file before atomically
replacing the watermark/current-generation pointer. Readers use only the named
generation and never write preferences on demand. A crash before the pointer
swap leaves the previous generation active; later maintenance removes abandoned
generations while retaining the current and previous published generations.

A valid tracked UUID in `.coding-brain/project.toml` is authoritative across
clones, worktrees, and forks. Coding Brain does not infer identity from paths,
names, or Git remotes. A user who wants independent learning removes the
manifest and reruns `coding-brain init`.

### Keep external I/O explicit and bounded

The user may select any model endpoint through CLI or user configuration.
Project configuration cannot redirect it. Coding Brain redacts and bounds
model-bound context, sends curl request bodies over stdin rather than argv,
disables redirects, caps response bytes, and shows stronger warnings for
plaintext non-loopback HTTP. Those warnings do not override the user's endpoint
choice.

Confirmed purge accepts only absolute non-root bases and fixed child targets.
It previews and revalidates each file type immediately before deletion, rejects
changed targets, and unlinks symlinks without following them. Project config and
identity files are never purge targets.

## Rationale

The ordering makes safety depend on evidence Coding Brain can actually
guarantee. A model action cannot leave the process until its immutable journal,
proposal, exact lifecycle authority, and terminal activity agree durably, while
deterministic denies remain effective during storage failure. The journal makes
partial destination publication recoverable without pretending that the
filesystem and response pipe share one transaction. Separate delivery and
outcome states continue to distinguish response emission from execution.

Immutable preference generations apply the same principle to learning: publish
one complete snapshot with one atomic pointer instead of coordinating many
in-place renames. Tail repair keeps the JSONL format simple while ensuring that
one killed writer does not consume the next valid event.

Stable UUID authority is intentionally explicit. Automatically comparing paths
or remotes would split worktrees and clones unpredictably, while a manual
manifest reset makes the user's intent reviewable in the repository.

## Consequences

- Activity projections and operator copy must distinguish committed,
  delivered, delivery-failed, delivery-unknown, and outcome-confirmed states.
- Permission transaction recovery must never replay a provider response or
  derive executable authority from legacy, mismatched, corrupt, or newer state.
- Same-request admission is nonblocking and single-winner; independent request
  shards retain concurrency.
- Preference distillation must join proposal records with authoritative
  activity and exclude unpaired proposals from learning.
- Hook tests must inject failures at journal and destination boundaries,
  stdout, delivery append, JSONL tail repair, and each preference-generation
  publication boundary.
- Distillation keeps at least two published generations, using more state in
  exchange for safe rollback after a failed publication.
- Remote endpoints remain usable by explicit choice, but prompts and responses
  are bounded and visible transport warnings remain part of the product UI.
- Purge and project-identity reset remain explicit operator actions; normal
  startup does not migrate, rewrite, or delete legacy data.
