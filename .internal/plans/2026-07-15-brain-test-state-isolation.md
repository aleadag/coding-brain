# Brain Test State Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Prevent unit tests from reading or writing the user's live brain data while preserving the production `~/.codexctl/brain` path.

**Architecture:** Keep `decisions_dir()` as the single path boundary used by brain persistence. Under `cfg(test)`, return a temp-rooted directory namespaced by process ID and test-thread name; production builds retain the existing `HOME/.codexctl/brain` result.

**Tech Stack:** Rust, Cargo unit tests, `tempfile`-independent standard-library paths, jq for live-store verification

## Global Constraints

- Production brain persistence remains under `~/.codexctl/brain`.
- Unit-test paths must not depend on process-wide `HOME`.
- Parallel test threads and separate Cargo test binaries must not share brain state.
- Cleanup removes only records whose exact `project` field equals `"test"`; the full original log remains backed up.

---

### Task 1: Isolate brain persistence in unit-test builds

**Files:**
- Modify: `src/brain/decisions.rs:255-262`
- Test: `src/brain/decisions.rs`

**Interfaces:**
- Consumes: `std::env::temp_dir`, `std::process::id`, `std::thread::current`, and existing `project_slug(&str) -> String`.
- Produces: unchanged `pub(super) fn decisions_dir() -> PathBuf`, with a test-only temp namespace and the existing production path.

**Acceptance Criteria:**
- Two named test threads resolve distinct brain directories.
- Both unit-test directories are below `<temp>/codexctl-tests/<process-id>/` and end in `brain`.
- Production code retains `HOME/.codexctl/brain` unchanged.
- The full Rust quality gates pass.
- A full `cargo test` run adds zero records to the live decision log, and the live review queue remains empty.

- [ ] **Step 1: Write the failing test**

Add this unit test inside `src/brain/decisions.rs`:

```rust
#[test]
fn unit_test_decision_paths_are_thread_scoped() {
    let path_for = |name: &str| {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(decisions_dir)
            .unwrap()
            .join()
            .unwrap()
    };

    let first = path_for("brain-test-first");
    let second = path_for("brain-test-second");
    let root = std::env::temp_dir()
        .join("codexctl-tests")
        .join(std::process::id().to_string());

    assert_ne!(first, second);
    assert!(first.starts_with(&root));
    assert!(second.starts_with(&root));
    assert!(first.ends_with("brain"));
    assert!(second.ends_with("brain"));
}
```

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test brain::decisions::tests::unit_test_decision_paths_are_thread_scoped -- --exact --nocapture
```

Expected: FAIL because both threads currently resolve the same `HOME/.codexctl/brain` path.

- [ ] **Step 3: Implement the test-only path boundary**

Change `decisions_dir()` to:

```rust
pub(super) fn decisions_dir() -> PathBuf {
    #[cfg(test)]
    {
        let thread = std::thread::current();
        let scope = project_slug(thread.name().unwrap_or("unnamed-test"));
        return std::env::temp_dir()
            .join("codexctl-tests")
            .join(std::process::id().to_string())
            .join(scope)
            .join("brain");
    }

    #[cfg(not(test))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home).join(".codexctl").join("brain")
    }
}
```

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```bash
cargo test brain::decisions::tests::unit_test_decision_paths_are_thread_scoped -- --exact --nocapture
```

Expected: PASS with one test and zero failures.

- [ ] **Step 5: Verify the live store is unchanged by the full suite**

Before and after `cargo test`, run:

```bash
jq -s '{total:length,test:map(select(.project == "test"))|length}' /home/alexander/.codexctl/brain/decisions.jsonl
```

Expected: identical total counts and `test: 0` before and after.

- [ ] **Step 6: Run all repository quality gates**

Run:

```bash
cargo fmt --check
cargo test
cargo clippy -- -D warnings
cargo build
```

Expected: every command exits 0 with no warnings or test failures.

- [ ] **Step 7: Verify the user-visible queue and jj diff**

Run:

```bash
target/debug/codexctl --brain-review list
jj --no-pager diff --git
jj --no-pager st
```

Expected: Review Queue reports `0 item(s)`; the diff contains only the approved design, plan, regression test, and path isolation implementation; the current jj description remains `🐛 fix: isolate brain tests from user state`.
