# Diagnostics Label Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Remove the redundant per-row `Diagnostic` prefix and Evidence `Status: Diagnostic` field while preserving all useful Diagnostics context and behavior.

**Architecture:** Keep the existing Diagnostics renderer and activity model. Change only the row text assembled by `diagnostic_row`, the constant fields assembled by `evidence_lines`, and the existing TUI rendering assertions.

**Tech Stack:** Rust, Ratatui `TestBackend`, Cargo test

## Global Constraints

- Preserve the `Diagnostics` tab and `Recent Diagnostics` title and count.
- Preserve provider, project, tool, and remaining Evidence fields.
- Preserve store-health rendering, empty-state wording, selection, scrolling, and navigation behavior.
- Do not add renderer abstractions or change the activity data model.
- Do not commit, push, or publish without separate user authorization.

---

### Task 1: Remove redundant Diagnostics labels

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/diagnostics.rs`

**Interfaces:**
- Consumes: existing `diagnostic_row(&ActivityItem, usize, &BrainApp) -> Line<'static>` and `evidence_lines(Option<&ActivityItem>, &Theme) -> Vec<Line<'static>>`
- Produces: unchanged function signatures with revised visible text only

**Acceptance Criteria:**
- Diagnostic event rows render provider, project, and tool without a `Diagnostic` prefix.
- Evidence omits `Status: Diagnostic` while retaining Activity, Recorded, Provider, Session, Project, Tool, and Reason.
- Narrow width `119` and wide width `120` rendering tests cover the revised labels.
- The Diagnostics tab, Recent Diagnostics count/title, empty state, and selection behavior remain unchanged.
- The focused TUI tests and repository quality gates pass.

- [ ] **Step 1: Write failing narrow and wide rendering assertions**

In `crates/coding-brain-tui/src/ui/brain/mod.rs`, update the populated
Diagnostics test and its `119`/`120` width loop to require the useful row text
and reject the two redundant forms:

```rust
for expected in [
    "[ Diagnostics ]",
    "Recent Diagnostics (2)",
    "Codex  project  Bash",
    "Activity: diagnostic-1",
    "Provider: Codex",
    "Project: project",
    "Tool: Bash",
] {
    assert!(text.contains(expected), "missing {expected}:\n{text}");
}
for forbidden in ["Diagnostic  Codex", "Status: Diagnostic"] {
    assert!(!text.contains(forbidden), "found {forbidden}:\n{text}");
}
```

Remove assertions that merely require the broad substring `"Diagnostic"`,
because the preserved tab and panel titles intentionally still contain it.
Keep the existing empty-state, store-health, escaping, scrolling, theme, and
selection tests intact.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui ui::brain::tests::diagnostics_ -- --nocapture
```

Expected: the populated rendering test fails because it still finds
`Diagnostic  Codex` and `Status: Diagnostic`. The empty-state and unrelated
Diagnostics behavior tests continue to pass.

- [ ] **Step 3: Implement the minimal renderer change**

In `crates/coding-brain-tui/src/ui/brain/diagnostics.rs`, assemble the row
without the constant prefix:

```rust
let text = format!(
    "{}  {}  {}",
    live::safe_row_text(provider),
    project,
    live::safe_row_text(tool),
);
```

Remove `("Status", "Diagnostic".into())` from the Evidence field array. Remove
the special first-value color branch that existed only to emphasize that
status, leaving every remaining value in `theme.text_primary`:

```rust
.map(|(label, value)| {
    Line::from(vec![
        Span::styled(format!("{label}: "), label_style),
        Span::styled(value, Style::default().fg(theme.text_primary)),
    ])
})
```

- [ ] **Step 4: Run focused tests and verify GREEN**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui ui::brain::tests::diagnostics_ -- --nocapture
```

Expected: all focused Diagnostics tests pass.

- [ ] **Step 5: Run formatting and repository quality gates**

Run:

```bash
direnv exec . cargo fmt --check
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo build
```

Expected: every command exits successfully with no warnings from Clippy.

- [ ] **Step 6: Review the uncommitted result**

Run:

```bash
git diff --check
git status --short
git diff -- crates/coding-brain-tui/src/ui/brain/mod.rs crates/coding-brain-tui/src/ui/brain/diagnostics.rs
```

Expected: only the approved design/plan documents, targeted renderer, and TUI
test changes are present. Report the diff and verification evidence without
committing or publishing.
