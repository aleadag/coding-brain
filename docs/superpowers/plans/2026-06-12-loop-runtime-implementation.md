# Loop Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the first working vertical slice of `codexctl loop`: discover loop definition files, validate them, fetch normalized source items, persist deduped loop state, apply deterministic/model decisions inside allowlists, and submit safe actionable items to the existing coord task ledger.

**Architecture:** Add a binary-crate `src/loop/` subsystem above `coord`. The loop runtime owns source polling, dedupe, policy, and item state; `coord` remains the executor. V1 supports project-local TOML loop files, shell and GitHub issue sources, SQLite state, dry-run, report mode, and coord task submission.

**Tech Stack:** Rust 2024, `clap`, `serde_json`, `rusqlite` behind the existing `coord` feature, existing `brain::client` curl-backed LLM helper, existing `coord::tasks`.

---

## File Structure

- Create `src/loop/mod.rs`: module exports and shared result type.
- Create `src/loop/config.rs`: loop file discovery and hand-rolled TOML subset parser.
- Create `src/loop/store.rs`: SQLite loop DB, migrations, run/item/event CRUD.
- Create `src/loop/sources/mod.rs`: `LoopSource`, `SourceItem`, source construction.
- Create `src/loop/sources/shell.rs`: command-backed JSONL source adapter for tests and custom sources.
- Create `src/loop/sources/github_issues.rs`: `gh issue list` source adapter.
- Create `src/loop/policy.rs`: `LoopDecision`, deterministic/model decision parsing and validation.
- Create `src/loop/prompt.rs`: model prompt and coord task prompt rendering.
- Create `src/loop/submit.rs`: convert accepted loop items into `coord::tasks::NewTask`.
- Create `src/loop/cli.rs`: `codexctl loop` commands.
- Modify `src/main.rs`: add `mod r#loop`, `Command::Loop`, and dispatch.
- Modify `src/lib.rs`: export `r#loop` for tests.
- Modify `src/brain/client.rs`: expose an existing plain LLM completion helper for model triage.

## Task 1: Config Discovery And Validation

**Files:**
- Create: `src/loop/mod.rs`
- Create: `src/loop/config.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing config tests**

Add tests in `src/loop/config.rs`:

```rust
#[test]
fn parse_loop_config_accepts_minimal_shell_loop() {
    let body = r#"
name = "daily-email"
enabled = true
mode = "report"
cadence = "1d"

[source]
kind = "shell"
command = "printf '{}\n'"
limit = 3

[triage]
mode = "deterministic"
skill = "loop-triage"

[execution]
cwd = "."
worktree = "none"
session = "headless"

[gates]
max_items_per_run = 2
max_concurrent = 1
"#;

    let cfg = parse_loop_config(body, std::path::PathBuf::from(".codexctl/loops/daily-email.toml")).unwrap();

    assert_eq!(cfg.name, "daily-email");
    assert_eq!(cfg.mode, LoopMode::Report);
    assert_eq!(cfg.source.kind, SourceKind::Shell);
    assert_eq!(cfg.source.command.as_deref(), Some("printf '{}\\n'"));
    assert_eq!(cfg.execution.worktree, WorktreeMode::None);
    assert_eq!(cfg.gates.max_items_per_run, 2);
}

#[test]
fn validate_loop_config_rejects_missing_skill() {
    let mut cfg = LoopConfig::minimal_for_test("issue-triage");
    cfg.triage.skill = Some("missing-skill".into());
    let available = std::collections::HashSet::from(["loop-triage".to_string()]);

    let err = cfg.validate_with_skills(&available).unwrap_err();

    assert!(err.contains("required skill missing-skill not found"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codexctl loop::config::tests --features coord`

Expected: compile failure because `src/loop/config.rs` and related types do not exist.

- [ ] **Step 3: Implement config parser**

Create `src/loop/mod.rs` with:

```rust
pub mod config;

pub type LoopResult<T> = Result<T, String>;
```

Create `src/loop/config.rs` with focused structs:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode { Report, Assisted, Unattended }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind { Shell, GithubIssues }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriageMode { Deterministic, Model }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeMode { None, Existing, Required, Auto }
```

Add `LoopConfig`, nested config structs, a `parse_loop_config(&str, PathBuf) -> LoopResult<LoopConfig>` parser for the plan's TOML subset, `discover_project_loops(root: &Path)`, and `validate_with_skills(&HashSet<String>)`.

Modify `src/lib.rs`:

```rust
#[cfg(feature = "coord")]
pub mod r#loop;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p codexctl loop::config::tests --features coord`

Expected: tests pass.

## Task 2: SQLite Loop Store

**Files:**
- Create: `src/loop/store.rs`
- Modify: `src/loop/mod.rs`

- [ ] **Step 1: Write failing store tests**

Add tests in `src/loop/store.rs`:

```rust
#[test]
fn upsert_item_dedupes_by_loop_source_and_item_id() {
    let conn = open_memory();
    let item = NewLoopItem {
        loop_name: "issue-triage".into(),
        source_kind: "github_issues".into(),
        source_item_id: "github:aleadag/codexctl#123".into(),
        title: "Bug".into(),
        body_summary: "Body".into(),
        url: Some("https://github.com/aleadag/codexctl/issues/123".into()),
        raw_json: serde_json::json!({"number": 123}),
    };

    let first = upsert_item(&conn, &item).unwrap();
    let second = upsert_item(&conn, &item).unwrap();
    let rows = list_items(&conn, Some("issue-triage")).unwrap();

    assert_eq!(first, second);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, LoopItemState::Seen);
}

#[test]
fn mark_submitted_stores_coord_task_id() {
    let conn = open_memory();
    let id = upsert_item(&conn, &NewLoopItem::for_test("loop-a", "source-1")).unwrap();

    mark_submitted(&conn, &id, "task_1", Some("/tmp/worktree")).unwrap();
    let row = get_item(&conn, &id).unwrap().unwrap();

    assert_eq!(row.state, LoopItemState::Submitted);
    assert_eq!(row.coord_task_id.as_deref(), Some("task_1"));
    assert_eq!(row.worktree_path.as_deref(), Some("/tmp/worktree"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codexctl loop::store::tests --features coord`

Expected: compile failure because `store` is not implemented.

- [ ] **Step 3: Implement store**

Add `pub mod store;` in `src/loop/mod.rs`.

Implement:

```rust
pub fn open() -> LoopResult<rusqlite::Connection>;
pub fn open_memory() -> rusqlite::Connection;
pub fn begin_run(conn: &Connection, loop_name: &str, config_path: &Path) -> LoopResult<String>;
pub fn finish_run(conn: &Connection, run_id: &str, status: &str, error: Option<&str>) -> LoopResult<()>;
pub fn upsert_item(conn: &Connection, item: &NewLoopItem) -> LoopResult<String>;
pub fn list_items(conn: &Connection, loop_name: Option<&str>) -> LoopResult<Vec<LoopItemRow>>;
pub fn get_item(conn: &Connection, item_id: &str) -> LoopResult<Option<LoopItemRow>>;
pub fn mark_decision(conn: &Connection, item_id: &str, state: LoopItemState, decision: &serde_json::Value) -> LoopResult<()>;
pub fn mark_submitted(conn: &Connection, item_id: &str, task_id: &str, worktree_path: Option<&str>) -> LoopResult<()>;
pub fn log_event(conn: &Connection, run_id: Option<&str>, item_id: Option<&str>, level: &str, event_type: &str, message: &str, data: serde_json::Value) -> LoopResult<()>;
```

Use a sibling DB at `~/.codexctl/loop/loop.db` and in-memory migrations for tests.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p codexctl loop::store::tests --features coord`

Expected: tests pass.

## Task 3: Sources And Decision Policy

**Files:**
- Create: `src/loop/sources/mod.rs`
- Create: `src/loop/sources/shell.rs`
- Create: `src/loop/sources/github_issues.rs`
- Create: `src/loop/policy.rs`
- Create: `src/loop/prompt.rs`
- Modify: `src/loop/mod.rs`
- Modify: `src/brain/client.rs`

- [ ] **Step 1: Write failing source/policy tests**

Add tests:

```rust
#[test]
fn parse_model_decision_rejects_unallowed_verifier() {
    let cfg = crate::r#loop::config::LoopConfig::minimal_for_test("issue-triage");
    let json = r#"{
      "action": "submit",
      "risk": "low",
      "reason": "clear",
      "task_name": "Fix issue",
      "task_prompt": "Fix it",
      "worktree": "none",
      "verifiers": ["rm -rf /"]
    }"#;

    let err = parse_and_validate_decision(json, &cfg).unwrap_err();

    assert!(err.contains("verifier rm -rf / is not allowed"));
}

#[test]
fn deterministic_report_mode_reports_items() {
    let mut cfg = crate::r#loop::config::LoopConfig::minimal_for_test("daily-email");
    cfg.mode = crate::r#loop::config::LoopMode::Report;
    let item = crate::r#loop::sources::SourceItem::for_test("msg-1");

    let decision = deterministic_decision(&cfg, &item).unwrap();

    assert_eq!(decision.action, LoopAction::Report);
}
```

Add shell source test:

```rust
#[test]
fn shell_source_reads_json_lines() {
    let source = ShellSource::new("test", "printf '{\"id\":\"one\",\"title\":\"One\",\"body\":\"Body\"}\\n'".into(), 10);

    let fetched = source.fetch(None, 10).unwrap();

    assert_eq!(fetched.items.len(), 1);
    assert_eq!(fetched.items[0].source_item_id, "shell:test:one");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p codexctl loop::policy::tests --features coord
cargo test -p codexctl loop::sources::shell::tests --features coord
```

Expected: compile failure because modules are missing.

- [ ] **Step 3: Implement sources and policy**

Add modules in `src/loop/mod.rs`:

```rust
pub mod policy;
pub mod prompt;
pub mod sources;
```

Implement `SourceItem`, `FetchResult`, `LoopSource`, `ShellSource`, and `GithubIssuesSource`.

Expose `pub fn complete(config: &BrainConfig, prompt: &str) -> Result<String, String>` in `src/brain/client.rs` by renaming the private `call_llm` or wrapping it.

Implement `LoopDecision`, `LoopAction`, `parse_and_validate_decision`, `deterministic_decision`, `build_model_triage_prompt`, and `render_task_prompt`.

- [ ] **Step 4: Run tests to verify they pass**

Run:

```bash
cargo test -p codexctl loop::policy::tests --features coord
cargo test -p codexctl loop::sources::shell::tests --features coord
```

Expected: tests pass.

## Task 4: Run Pipeline And Coord Submission

**Files:**
- Create: `src/loop/submit.rs`
- Create: `src/loop/cli.rs`
- Modify: `src/loop/mod.rs`
- Modify: `src/main.rs`

- [ ] **Step 1: Write failing pipeline tests**

Add tests in `src/loop/cli.rs` or `src/loop/submit.rs`:

```rust
#[test]
fn submit_decision_creates_coord_task_and_marks_item_submitted() {
    let loop_conn = crate::r#loop::store::open_memory();
    let coord_conn = crate::coord::store::open_memory();
    let cfg = crate::r#loop::config::LoopConfig::minimal_for_test("issue-triage");
    let source_item = crate::r#loop::sources::SourceItem::for_test("github:repo#1");
    let loop_item_id = crate::r#loop::store::upsert_item(&loop_conn, &crate::r#loop::store::NewLoopItem::from_source("issue-triage", &source_item)).unwrap();
    let decision = crate::r#loop::policy::LoopDecision::submit_for_test("Fix it");

    let task_id = submit_coord_task(&coord_conn, &loop_conn, &cfg, &loop_item_id, &source_item, &decision, None).unwrap();

    let task = crate::coord::tasks::get_task(&coord_conn, &task_id).unwrap().unwrap();
    let item = crate::r#loop::store::get_item(&loop_conn, &loop_item_id).unwrap().unwrap();
    assert_eq!(task.name, "Fix it");
    assert_eq!(item.coord_task_id.as_deref(), Some(task_id.as_str()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p codexctl loop::submit::tests --features coord`

Expected: compile failure because submission is missing.

- [ ] **Step 3: Implement submit and CLI**

Implement:

```rust
pub fn submit_coord_task(
    coord_conn: &rusqlite::Connection,
    loop_conn: &rusqlite::Connection,
    cfg: &LoopConfig,
    loop_item_id: &str,
    source_item: &SourceItem,
    decision: &LoopDecision,
    worktree_path: Option<&str>,
) -> LoopResult<String>;
```

Implement `LoopCommand`:

```rust
pub enum LoopCommand {
    List,
    Validate { name: Option<String> },
    Run { name: String, dry_run: bool, limit: Option<usize> },
    Status { name: Option<String> },
    Logs { name: String, item: Option<String> },
    Pause { name: String },
    Resume { name: String },
    Export { name: String, format: String },
}
```

Wire `src/main.rs`:

```rust
#[cfg(feature = "coord")]
mod r#loop;

#[cfg(feature = "coord")]
Command::Loop { command } => return r#loop::cli::dispatch(command, &cfg),
```

V1 `run` should support dry-run, shell source, GitHub issue source, deterministic decisions, model decisions when `[brain]` is configured, SQLite item insertion, and coord submission unless `--dry-run` or `mode = "report"`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p codexctl loop::submit::tests --features coord`

Expected: tests pass.

## Task 5: End-To-End Verification

**Files:**
- Modify: files touched in Tasks 1-4 only when verification exposes a defect in that task's implementation.

- [ ] **Step 1: Run focused loop tests**

Run: `cargo test -p codexctl loop:: --features coord`

Expected: all loop module tests pass.

- [ ] **Step 2: Run broader compile/test gate**

Run: `cargo test -p codexctl --features coord`

Expected: root package tests pass.

- [ ] **Step 3: Run formatting**

Run: `cargo fmt --check`

Expected: no diff.

- [ ] **Step 4: Run clippy**

Run: `cargo clippy -- -D warnings`

Expected: no warnings. If unrelated pre-existing warnings appear, capture them and do not broaden the implementation.

- [ ] **Step 5: Check jj diff**

Run: `jj --no-pager diff --git`

Expected: changes are limited to the loop runtime, brain helper exposure, main/lib wiring, and docs plan.
