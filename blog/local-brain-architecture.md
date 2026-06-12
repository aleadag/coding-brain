# The Local Brain: Why Your Agent Orchestrator Should Think On-Device

*How codexctl uses a local LLM to supervise, teach, and coordinate AI coding agents — without your reasoning traces ever leaving your machine.*

---

## The Problem With Babysitting Agents

If you run Codex on a non-trivial codebase, you know the rhythm: approve this Bash command, deny that file write, approve this read, wait — why is it running `rm -rf`? You're babysitting. And if you're running three sessions in parallel across a monorepo, you're babysitting three times over, context-switching between terminal tabs, trying to remember which session is doing what.

The obvious fix is automation: write rules that auto-approve safe operations and deny dangerous ones. We built that — codexctl has a rule engine that matches on tool name, command substring, project, cost threshold, and more. Deny rules always override approvals. It works.

But rules are static. They can't reason about *why* a session is calling `git push --force` — is it rebasing a feature branch (probably fine) or force-pushing to main (stop everything)? They can't notice that two sessions are editing the same file and decide which one should yield. They can't look at a session that's spent $8 with no file edits and decide whether it's legitimately researching or stuck in a loop.

You need something that can reason. The question is: where does that reasoning happen?

## Why Not Just Use the Cloud?

The naive approach: send session context to Codex or GPT-4 and ask it what to do. This works technically — a frontier model is excellent at evaluating tool calls. But it creates three problems that compound badly at scale:

**1. Your reasoning traces are your workflow DNA.**

Every Codex session produces a JSONL transcript: what files were read, what edits were made, what commands were run, what the model reasoned about, what you approved and denied. Over weeks, these traces encode something extraordinarily valuable — your engineering judgment. Your code review standards. Your security practices. The architectural patterns you reach for. The commands you consider dangerous versus routine.

Sending these traces to a cloud API means your workflow DNA becomes training data for someone else's model. Even if the provider promises not to train on API data, the traces still transit their infrastructure, land in their logs, and are subject to their data retention policies.

**2. Latency kills the supervision loop.**

Agent supervision is a tight loop: session produces tool call, supervisor evaluates, supervisor responds. A cloud round-trip adds 500ms-2s per decision. When you have 5 sessions each making 10 tool calls per minute, that's 50 cloud API calls per minute just for supervision. The latency alone makes real-time orchestration impractical. The cost makes it absurd — you'd spend more on supervision than on the agents themselves.

**3. The supervisor becomes a single point of failure.**

If your cloud supervisor goes down, rate-limits, or times out, all your sessions block. Your local development environment is now dependent on an external service for basic approve/deny decisions. This is the wrong dependency direction.

## The Local Brain Architecture

codexctl's brain is a local LLM (Gemma 4 4B, Llama 3, or any model you run locally) that observes your sessions and makes decisions. Here's what makes it work:

### What the brain sees

Every 2 seconds, the brain builds a context snapshot for each session that needs a decision:

```
Session: acme-api | Processing | gpt-5.5 | Cost: $3.20 | Context: 45%
Pending tool: Bash | Command: cargo test --release
Last tool ERRORED: test compilation failed (src/auth.rs:47)

Recent transcript (last 8 messages):
  [assistant] called Edit (src/auth.rs)
  [tool_result] ok
  [assistant] called Bash (cargo test)
  [tool_result] ERROR: compilation failed...
  [assistant] "I see the error. Let me fix the import..."
  [assistant] called Edit (src/auth.rs)
  [tool_result] ok
  [assistant] called Bash (cargo test --release)

Global session map:
  PID 1234 acme-api     Processing  $3.20  45% ctx  pending: Bash(cargo test --release)
  PID 5678 web-frontend Waiting     $1.80  30% ctx
  PID 9012 ml-pipeline  NeedsInput  $5.10  72% ctx  pending: Write(src/model.py)
```

This is enough context for a small model to make a binary decision. It doesn't need to understand the code — it needs to understand the *pattern*: is this a safe operation? Is this session making progress or looping? Should this tool call be approved given what other sessions are doing?

### The decision space is small

This is the key insight that makes local supervision viable. A 4B parameter model can't write production code. But it doesn't need to. The brain's decision space is:

| Decision | When |
|----------|------|
| **Approve** | Tool call looks safe based on tool name, command, and context |
| **Deny** | Tool call is dangerous, redundant, or conflicts with another session |
| **Send** | Session needs a nudge (e.g., "the API tests are in tests/api/") |
| **Terminate** | Session is looping, stalled, or over budget |
| **Route** | Session A produced output that session B needs |
| **Spawn** | Work can be parallelized by starting a new session |

Six actions. Each with a confidence score. This is a classification task with structured input, not open-ended generation. A quantized 4B model running on your laptop's GPU handles this in 200-500ms — faster than a cloud round-trip.

### Few-shot learning from your corrections

Every time the brain suggests an action and you press `b` (accept) or `B` (reject), that decision is logged:

```json
{
  "timestamp": "2026-04-15T14:30:00Z",
  "pid": 1234,
  "project": "acme-api",
  "tool": "Bash",
  "command": "cargo test --release",
  "brain_action": "approve",
  "confidence": 0.85,
  "reasoning": "Test command in a project that has been actively editing test files",
  "accepted": true
}
```

On the next inference, the brain retrieves the 5 most similar past decisions (scored by tool name and project match) and includes them as few-shot examples in the prompt:

```
Previous decisions for similar situations:
1. Bash(cargo test) in acme-api → approved (accepted by user)
2. Bash(cargo build --release) in acme-api → approved (accepted by user)
3. Bash(rm -rf target/) in acme-api → denied (accepted by user)
4. Bash(git push --force) in acme-api → denied (accepted by user)
5. Write(src/main.rs) in acme-api → approved (rejected by user — user wanted to review first)
```

The brain adapts to your preferences without fine-tuning. Decision #5 is particularly powerful — the user rejected the brain's approval, teaching it that file writes in this project need manual review. Next time, the brain will suggest review instead of auto-approve.

This learning loop stays entirely on your machine. The decision log is a JSONL file in `~/.codexctl/brain/decisions.jsonl`. You can inspect it, edit it, delete it. It's yours.

### Cross-session orchestration

The brain's most powerful capability is seeing all sessions simultaneously. A human can only focus on one terminal at a time. The brain sees them all, every 2 seconds.

This enables decisions that no single-session supervisor could make:

**Conflict detection**: Session A is editing `src/auth.rs`. Session B is about to write to `src/auth.rs`. The brain denies B's write and sends it a message: "File src/auth.rs is being edited by session acme-api (PID 1234). Wait for it to finish." This happens automatically, before the conflict creates a merge problem.

**Context routing**: Session A finishes implementing JWT auth middleware. Session B is writing API tests but doesn't know the auth middleware API changed. The brain summarizes A's changes (via the local LLM) and routes the summary to B: "[From acme-api] Added JWT auth middleware with Bearer token validation. New middleware function: `validate_jwt(req)` in src/auth.rs. Tests should use `Authorization: Bearer <token>` header."

**Redundancy detection**: Sessions A and B are both reading the same set of files to understand the codebase. The brain notices the overlap and terminates the redundant session, saving tokens and cost.

**Spawn decisions**: A session is working on a backend API change. The brain recognizes that frontend client code will need updating and spawns a new session in the frontend directory with a prompt derived from the backend changes.

All of this orchestration happens locally. The reasoning traces — which sessions are doing what, which files they're touching, what decisions were made — never leave your machine.

## The Model Doesn't Need to Be Smart. It Needs to Be Agent-Aware.

A common objection: "A 4B model can't reason well enough to supervise a frontier model." This misunderstands the task. The brain isn't competing with Codex on reasoning — it's making structured decisions based on observable signals.

Consider what the brain actually evaluates:

1. **Tool name** — Is this a read-only operation (Read, Grep, Glob) or a mutating one (Bash, Write, Edit)?
2. **Command content** — Does the Bash command contain `rm`, `git push --force`, `DROP TABLE`, or other dangerous patterns?
3. **Session state** — Is this session making progress (new files appearing, costs reasonable) or stuck (high cost, no output, same tool called 10+ times)?
4. **Cross-session state** — Is any other session touching the same files?
5. **Historical precedent** — What did the user decide last time this tool was called in this project?

None of these require deep reasoning. They require pattern matching on structured data — exactly what small models excel at. The few-shot examples do the heavy lifting: they encode your judgment into a format the model can mimic.

What the model *does* need is:

- **Enough context window** to see the full session snapshot (4K-8K tokens is sufficient)
- **Instruction-following** to return structured JSON (action, confidence, reasoning)
- **Fast inference** to keep the supervision loop under 500ms
- **Agent-awareness** — understanding that it's evaluating tool calls from another AI, not generating responses itself

The last point is subtle. The brain's system prompt explicitly frames it as a supervisor:

> You are a session supervisor for Codex agents. You observe what each session is doing and decide what to approve, deny, or coordinate. You do not write code. You evaluate tool calls.

This framing matters. Without it, small models sometimes try to "help" by suggesting code changes instead of making approve/deny decisions. Agent-aware prompting keeps the model in its lane.

## What Stays On-Device

Let's be explicit about the data sovereignty model:

| Data | Where it lives | Who can access it |
|------|---------------|-------------------|
| Session JSONL transcripts | `~/.codex/sessions/` | You |
| Brain decision log | `~/.codexctl/brain/decisions.jsonl` | You |
| Few-shot examples | Derived from decision log at inference time | You |
| Brain prompt templates | `~/.codexctl/brain/prompts/` (customizable) | You |
| Orchestration context | In-memory during TUI session | Never persisted |
| LLM inference | localhost (ollama/llama.cpp/vLLM) | You |

Nothing transits a network. Nothing is logged to a cloud service. Nothing is used for training. The entire supervision and orchestration loop runs on your hardware, using your models, learning from your decisions.

## The Compound Effect

The real value isn't any single decision. It's the compound effect over weeks.

In week 1, you're reviewing most of the brain's suggestions. It gets your basic preferences wrong — maybe it approves file writes you want to review, or denies test commands you consider safe.

By week 4, it has 500+ logged decisions. The few-shot retrieval finds highly relevant examples for most tool calls. You're only reviewing novel situations — new tools, new projects, unusual commands.

By week 8, the brain is effectively a codification of your engineering judgment for routine operations. It knows that `cargo test` is always safe in your Rust projects but `npm run deploy` needs review. It knows that Read operations are fine but Write operations in `src/config/` need manual approval. It knows that sessions over $10 with no file edits should be flagged.

This is your workflow, encoded as data, running on your hardware, improving with every correction. No one else can access it. No one else benefits from it. It's your competitive advantage as an engineer who runs AI agents at scale.

## Getting Started

```bash
# Install codexctl
brew install aleadag/tap/codexctl

# Install and start ollama with a suitable model
ollama pull gemma4:e4b
ollama serve

# Run with brain in advisory mode
codexctl --brain

# Once you trust it, enable auto-execution
codexctl --brain --auto-run
```

The brain starts with zero history and defaults to conservative suggestions. As you accept and reject its proposals, it learns. There's no setup, no training pipeline, no data export. Just use it.

---

*codexctl is open-source and MIT-licensed. The brain subsystem ships in the same binary — no separate service, no account, no API key. Your machine, your models, your decisions.*
