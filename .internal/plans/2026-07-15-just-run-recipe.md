# Just Run Recipe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Provide one general `just run` command and remove the redundant `headless-json` alias.

**Architecture:** Modify only the repository `justfile`. A zero-or-more
variadic parameter forwards CLI arguments after Cargo's `--` separator.

**Tech Stack:** just, Cargo, codexctl CLI

## Global Constraints

- `just run` must work with no arguments.
- All supplied CLI arguments must be forwarded unchanged to codexctl.
- `headless-json` must no longer appear as a recipe.

---

### Task 1: Replace the specialized runner with a passthrough runner

**Files:**
- Modify: `justfile`
- Test: `just --dry-run` command rendering

**Interfaces:**
- Consumes: codexctl's existing Cargo binary target and CLI arguments.
- Produces: `just run *args`.

**Acceptance Criteria:**
- `just --list` shows `run` and omits `headless-json`.
- `just --dry-run run` prints `cargo run --`.
- `just --dry-run run --headless --json` prints
  `cargo run -- --headless --json`.

- [ ] **Step 1: Confirm the new recipe does not exist**

Run:

```bash
just --dry-run run
```

Expected: failure stating that recipe `run` does not exist.

- [ ] **Step 2: Add the minimal recipe and remove the old alias**

Replace the `headless-json` recipe with:

```just
# Run codexctl, forwarding optional CLI arguments.
run *args:
    cargo run -- {{args}}
```

- [ ] **Step 3: Verify argument forwarding and recipe discovery**

Run:

```bash
just --dry-run run
just --dry-run run --headless --json
just --list
```

Expected: the dry runs render `cargo run --` with zero and two forwarded
arguments; the list contains `run` and not `headless-json`.

- [ ] **Step 4: Verify repository formatting**

Run:

```bash
just fmt-check
```

Expected: exit code 0.

- [ ] **Step 5: Inspect the jj change**

Run:

```bash
jj --no-pager diff --git justfile
jj --no-pager st
```

Expected: the implementation changes only the runner recipes in `justfile`;
the design, plan, and Beads tracking artifacts are the only supporting changes.
