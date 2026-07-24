# Live Activity Scanability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make Live rows condition-first and make all selected activity evidence structured, safe, and scrollable without changing lifecycle inference or list behavior.

**Architecture:** Keep all presentation derivation in pure helpers in the TUI Live renderer. Add only the minimum UI state needed for Evidence scrolling to `BrainApp`; lifecycle models, list membership, ordering, and responsive layout remain unchanged.

**Tech Stack:** Rust 2024, Ratatui 0.29, Crossterm 0.28, `unicode-width` 0.2

## Global Constraints

- Do not change activity inference, lifecycle semantics, Needs Attention/Recent membership, or ordering.
- Preserve the 120-column breakpoint, 67%/33% wide split, bounded 12-row narrow Evidence pane, and existing `j`/`k` and `J`/`K` behavior.
- Use the existing semantic theme palette and retain meaningful monochrome output.
- Never emit raw control characters; do not add persistence, logging, clipboard, export, or TUI-specific secret redaction.
- Keep rendering linear and stateless apart from bounded Evidence viewport metrics and scroll position; do not add parsing, regexes, or caches.

---

### Task 1: Condition-first Live rows

**Files:**
- Modify: `crates/coding-brain-tui/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`

**Interfaces:**
- Consumes: `ActivityItem`, `ActivityOutcome`, `ActivityState`, `DeliveryState`, and the existing `Theme`.
- Produces: `activity_badge(&ActivityItem) -> ActivityBadge`, `safe_row_text(&str) -> String`, `truncate_display(&str, usize) -> String`, and `activity_row(&ActivityItem, Option<usize>, usize, &Theme) -> Line<'static>`.

**Acceptance Criteria:**
- Status and project remain visible before provider and action at every usable width.
- Status badges exhaustively preserve existing outcome/correction/delivery/state precedence.
- Provider is omitted before project is capped and action is omitted.
- Unicode truncation uses terminal display columns and includes a visible ellipsis without splitting a character.
- Whitespace is collapsed in rows and non-whitespace controls render as safe visible escapes.
- Selection does not overwrite badge, project, or provider styles; monochrome remains understandable.

- [ ] **Step 1: Add failing pure-helper tests**

Add `#[cfg(test)]` tests beside the helpers in `live.rs` covering every state and outcome badge, delivery precedence, Unicode truncation (`"界面"`), control input (`"\u{1b}[31m"`), whitespace collapse, occurrence alignment, and provider omission at constrained widths.

- [ ] **Step 2: Verify the focused tests fail**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui ui::brain::live::tests -- --nocapture
```

Expected: compilation failures for the missing row helpers.

- [ ] **Step 3: Add the display-width dependency and minimal helpers**

Add `unicode-width = "0.2"` to the TUI crate. Implement a compact badge value containing label and semantic class; safe row/evidence text normalization; display-width truncation; and deterministic row field allocation. Render list items as styled `Line` values in badge, project, provider, action, count order.

- [ ] **Step 4: Use structured rows in both Live lists**

Replace formatted attention/recent strings with `activity_row`. Pass the list content width after borders and the persistent two-column selection marker. Remove the foreground-changing highlight style while retaining `> ` and `HighlightSpacing::Always`.

- [ ] **Step 5: Verify Task 1**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui ui::brain::live::tests -- --nocapture
direnv exec . cargo test -p coding-brain-tui ui::brain::tests::live_list_indentation_stays_fixed_when_selection_moves_between_lists -- --nocapture
```

Expected: all selected tests pass.

### Task 2: Urgency-first, scrollable Evidence

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs`

**Interfaces:**
- Consumes: the selected `ActivityItem`, compact badge helper, current Live list selection, Evidence render area, and PageUp/PageDown key events.
- Produces: `EvidenceDensity::{Wide, Compact}`, `evidence_lines(&ActivityItem, EvidenceDensity, &Theme)`, and `BrainApp` methods for current scroll, viewport metrics, page up/down, and selection-reset behavior.

**Acceptance Criteria:**
- Evidence presents Status, Outcome, Action, and Context in that urgency order.
- Wide mode uses section headings and restrained spacing; narrow mode keeps Status and Outcome visible with compact inline labels.
- Full command, activity ID, provider, project, and present optional evidence remain reachable.
- PageUp/PageDown move one visible page, clamp at both ends, reset on selection change, and expose `↑ more`/`↓ more` title indicators.
- Existing selection, list jumps, navigation, correction, and action targeting remain unchanged.

- [ ] **Step 1: Add failing application-state tests**

In `brain_app.rs`, add tests that PageDown/PageUp use recorded viewport metrics, clamp at the maximum, and reset scroll after `j`, `k`, `J`, `K`, refresh-driven selection change, and tab changes.

- [ ] **Step 2: Verify application-state tests fail**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui brain_app::tests::live_evidence -- --nocapture
```

Expected: compilation failures for missing Evidence scroll state.

- [ ] **Step 3: Implement bounded Evidence scrolling state**

Add scroll offset and last rendered page/max-scroll metrics to `BrainApp`. Handle `PageUp` and `PageDown` only on Live. Reset the offset whenever the selected activity identity changes or the user changes Live selection/list/tab. Expose read-only accessors plus a render-metrics update method; metrics are viewport bookkeeping, not cached content.

- [ ] **Step 4: Add failing Evidence rendering tests**

In `ui/brain/mod.rs`, add TestBackend coverage for all optional fields, no optional fields, safe control rendering, compact narrow order, 119/120 breakpoints, theme modes, overflow indicators, PageUp/PageDown movement, and selection reset.

- [ ] **Step 5: Implement the shared semantic Evidence builder**

Replace flat key/value lines with one density-aware builder. Use a styled badge header, Outcome before Action and Context, muted labels, bold project, wrapped safe values, and no absent optional rows. Compute wrapped content height from these lines, apply `Paragraph::scroll`, update bounded viewport metrics, and decorate the Evidence title with overflow indicators.

- [ ] **Step 6: Document scrolling in the footer**

Add `PgUp/PgDn evidence` to the Live footer without changing other shortcuts.

- [ ] **Step 7: Verify Task 2**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui brain_app::tests::live_evidence -- --nocapture
direnv exec . cargo test -p coding-brain-tui ui::brain::tests::live_ -- --nocapture
```

Expected: all selected tests pass.

### Task 3: Regression coverage and release-quality validation

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs`
- Modify only if required by test findings: `crates/coding-brain-tui/src/brain_app.rs`

**Interfaces:**
- Consumes: Tasks 1 and 2 helpers and state.
- Produces: focused structural regressions for row hierarchy, widths, themes, Evidence reachability, and preserved navigation.

**Acceptance Criteria:**
- TestBackend coverage includes extreme narrow, typical narrow, 119, 120, and representative wide widths.
- Attention/recent row order and styles, occurrence counts, Unicode/control safety, optional Evidence, themes, bounded narrow height, scrolling, and overflow are covered.
- Existing Live and full workspace tests pass with formatting and warnings denied.

- [ ] **Step 1: Fill remaining coverage gaps with focused tests**

Add structural assertions rather than full-screen goldens for content column order, right-aligned counts, project/action survival, ThemeMode Dark/Light/None, and Outcome visibility inside the narrow cap.

- [ ] **Step 2: Run the full TUI test suite**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui
```

Expected: all TUI tests pass.

- [ ] **Step 3: Run workspace quality gates**

Run:

```bash
direnv exec . cargo fmt --check
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
```

Expected: each command exits successfully with no warnings or failures.

- [ ] **Step 4: Inspect the final diff and status**

Run:

```bash
git diff --check
git diff --stat
git status --short --branch
```

Expected: only the cpt5 plan, TUI dependency/lockfile, Live renderer, app scroll state, and focused tests are changed.
