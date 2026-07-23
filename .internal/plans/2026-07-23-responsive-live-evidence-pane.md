# Responsive Live Evidence Pane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Make Live use a two-column layout at 120 columns and wider, keep narrow evidence content-bounded, and let operators jump directly between Needs Attention and Recent without losing either list's selection.

**Architecture:** Replace Live's combined index with an explicit active-list boundary and one remembered row index per list while retaining the existing generic index for Review. Keep rendering in `ui/brain/live.rs`: wide terminals split the body into stacked lists on the left and persistent Evidence on the right; narrow terminals retain stacked panes and compute Evidence height from the wrapped paragraph, capped at 12 rows.

**Tech Stack:** Rust, Ratatui 0.29, Crossterm, Cargo workspace tests

## Global Constraints

- `codexctl-q6o` is the approved specification and source of acceptance criteria.
- Width `>= 120` uses the wide layout; width `< 120` uses the narrow layout.
- Wide layout gives roughly two-thirds of the body to stacked Needs Attention and Recent lists and one-third to persistent Evidence.
- Narrow Evidence height follows wrapped content, includes its border, never exceeds 12 rows, and leaves room for both lists.
- Lowercase `j`/`k` and arrow keys move only within the active Live list.
- Uppercase `J` jumps to Recent; uppercase `K` jumps to Needs Attention.
- A list jump restores that list's last valid row, clamps a removed row to the new last row, and leaves the current selection unchanged when the target list is empty.
- If refresh empties the active list, focus moves to the other non-empty list; if both lists are empty, focus deterministically returns to Needs Attention row zero.
- Existing Live/Review/Scorecard tabs, attention-first ordering, activity projection, evidence fields, navigation, corrections, and action-safety checks remain unchanged.
- False lifecycle rows tracked by `codexctl-5ah` and `codexctl-81g` are out of scope.
- Do not add dependencies, commit, push, or sync without separate user authorization.

---

### Task 1: Give Live independent list focus and remembered row selections

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs:20-275`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:546-631`
- Test: `crates/coding-brain-tui/src/brain_app.rs:660-1380`

**Interfaces:**
- Consumes: existing `ActivitySnapshot.attention`, `ActivitySnapshot.recent`, `BrainTab`, and `KeyCode`
- Produces: `BrainApp::selected_attention_index() -> Option<usize>`, `BrainApp::selected_recent_index() -> Option<usize>`, and list-aware `selected_live_activity()` / `selected_attention()`

**Acceptance Criteria:**
- `j`/`k` and arrow keys move one row inside the active Live list and never cross the Attention/Recent boundary.
- `J` and `K` switch lists, restoring each list's remembered valid row.
- Refresh/removal clamps remembered rows and safely falls back when the active list becomes empty.
- Empty jump targets do not discard the visible selection.
- Navigation, corrections, and actions always use the visibly selected activity.
- Review navigation and tab cycling retain their existing behavior.

- [ ] **Step 1: Write failing list-jump and per-list movement tests**

Add focused tests beside the existing `BrainApp` key-handling tests. Build a Live snapshot with two Attention and two Recent rows, then prove lowercase movement is bounded and uppercase jumps restore each list:

```rust
#[test]
fn live_moves_within_lists_and_restores_each_list_selection() {
    let (mut app, _) = fixture_app(true);
    let mut second_attention = activity();
    second_attention.activity_id = "attention-2".into();
    app.snapshot.attention.push(AttentionItem {
        activity: second_attention,
        occurrences: 1,
        unresolved_occurrences: 1,
    });
    let mut recent_1 = activity();
    recent_1.activity_id = "recent-1".into();
    let mut recent_2 = activity();
    recent_2.activity_id = "recent-2".into();
    app.snapshot.recent = vec![recent_1, recent_2];
    app.clamp_selection();

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "attention-2");
    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "attention-2");

    app.handle_key(key(KeyCode::Char('J')));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "recent-1");
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "recent-2");

    app.handle_key(key(KeyCode::Char('K')));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "attention-2");
    app.handle_key(key(KeyCode::Char('J')));
    assert_eq!(app.selected_live_activity().unwrap().activity_id, "recent-2");
}
```

Add separate tests for an empty jump target and refresh-style clamping:

```rust
#[test]
fn live_empty_jump_target_keeps_the_visible_selection() {
    let (mut app, _) = fixture_app(true);
    let selected = app.selected_live_activity().unwrap().activity_id.clone();

    app.handle_key(key(KeyCode::Char('J')));

    assert_eq!(app.selected_live_activity().unwrap().activity_id, selected);
    assert_eq!(app.selected_attention_index(), Some(0));
    assert_eq!(app.selected_recent_index(), None);
}

#[test]
fn live_clamps_remembered_rows_and_falls_back_from_an_empty_active_list() {
    let (mut app, _) = fixture_app(true);
    let mut recent = activity();
    recent.activity_id = "recent-1".into();
    app.snapshot.recent = vec![recent];
    app.handle_key(key(KeyCode::Char('J')));
    app.snapshot.recent.clear();

    app.clamp_selection();

    assert_eq!(app.selected_attention_index(), Some(0));
    assert_eq!(app.selected_recent_index(), None);
    assert_eq!(
        app.selected_live_activity().unwrap().activity_id,
        app.snapshot.attention[0].activity_id
    );
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p coding-brain-tui live_moves_within_lists_and_restores_each_list_selection
cargo test -p coding-brain-tui live_empty_jump_target_keeps_the_visible_selection
cargo test -p coding-brain-tui live_clamps_remembered_rows_and_falls_back_from_an_empty_active_list
```

Expected: compilation fails because the independent selection accessors do not exist, proving the tests exercise the missing selection model.

- [ ] **Step 3: Add the minimal Live selection model**

Add a private focus enum and remembered indices next to `BrainTab`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveList {
    Attention,
    Recent,
}
```

Extend `BrainApp` with:

```rust
live_list: LiveList,
live_attention_selection: usize,
live_recent_selection: usize,
```

Initialize all three fields to Attention row zero. Keep `selection` for Review. Add these helpers:

```rust
fn live_len(&self, list: LiveList) -> usize {
    match list {
        LiveList::Attention => self.snapshot.attention.len(),
        LiveList::Recent => self.snapshot.recent.len(),
    }
}

fn live_selection(&self, list: LiveList) -> usize {
    match list {
        LiveList::Attention => self.live_attention_selection,
        LiveList::Recent => self.live_recent_selection,
    }
}

fn live_selection_mut(&mut self, list: LiveList) -> &mut usize {
    match list {
        LiveList::Attention => &mut self.live_attention_selection,
        LiveList::Recent => &mut self.live_recent_selection,
    }
}

fn move_live_selection_down(&mut self) {
    let len = self.live_len(self.live_list);
    if len > 0 {
        let next = (self.live_selection(self.live_list) + 1).min(len - 1);
        *self.live_selection_mut(self.live_list) = next;
    }
}

fn move_live_selection_up(&mut self) {
    let current = self.live_selection(self.live_list);
    *self.live_selection_mut(self.live_list) = current.saturating_sub(1);
}

fn jump_live_list(&mut self, target: LiveList) {
    if self.live_len(target) > 0 {
        self.live_list = target;
        let len = self.live_len(target);
        let clamped = self.live_selection(target).min(len - 1);
        *self.live_selection_mut(target) = clamped;
    }
}

fn clamp_live_selection(&mut self) {
    let attention_len = self.live_len(LiveList::Attention);
    let recent_len = self.live_len(LiveList::Recent);
    self.live_attention_selection = self
        .live_attention_selection
        .min(attention_len.saturating_sub(1));
    self.live_recent_selection = self
        .live_recent_selection
        .min(recent_len.saturating_sub(1));

    self.live_list = match (self.live_list, attention_len, recent_len) {
        (_, 0, 0) => LiveList::Attention,
        (LiveList::Attention, 0, _) => LiveList::Recent,
        (LiveList::Recent, _, 0) => LiveList::Attention,
        (list, _, _) => list,
    };
}
```

Update key handling exactly at the existing movement match arms:

```rust
KeyCode::Char('J') if self.tab == BrainTab::Live => {
    self.jump_live_list(LiveList::Recent);
    None
}
KeyCode::Char('K') if self.tab == BrainTab::Live => {
    self.jump_live_list(LiveList::Attention);
    None
}
KeyCode::Char('j') | KeyCode::Down => {
    if self.tab == BrainTab::Live {
        self.move_live_selection_down();
    } else {
        let len = self.current_len();
        if len > 0 {
            self.selection = (self.selection + 1).min(len - 1);
        }
    }
    None
}
KeyCode::Char('k') | KeyCode::Up => {
    if self.tab == BrainTab::Live {
        self.move_live_selection_up();
    } else {
        self.selection = self.selection.saturating_sub(1);
    }
    None
}
```

Make `clamp_selection()` clamp the Live memories plus the non-Live `selection`. Derive all Live targeting from the active list rather than from Attention length arithmetic:

```rust
fn clamp_selection(&mut self) {
    self.clamp_live_selection();
    self.selection = self.selection.min(self.current_len().saturating_sub(1));
}

pub fn selected_attention_index(&self) -> Option<usize> {
    (self.live_list == LiveList::Attention)
        .then_some(self.live_attention_selection)
        .filter(|index| *index < self.snapshot.attention.len())
}

pub fn selected_recent_index(&self) -> Option<usize> {
    (self.live_list == LiveList::Recent)
        .then_some(self.live_recent_selection)
        .filter(|index| *index < self.snapshot.recent.len())
}

pub fn selected_live_activity(&self) -> Option<&ActivityItem> {
    match self.live_list {
        LiveList::Attention => self
            .snapshot
            .attention
            .get(self.live_attention_selection)
            .map(|item| &item.activity),
        LiveList::Recent => self.snapshot.recent.get(self.live_recent_selection),
    }
}

pub fn selected_attention(&self) -> Option<&AttentionItem> {
    self.selected_attention_index()
        .and_then(|index| self.snapshot.attention.get(index))
}

pub fn selection(&self) -> usize {
    if self.tab == BrainTab::Live {
        self.live_selection(self.live_list)
    } else {
        self.selection
    }
}
```

Change `begin_correction()` and its fallback in `choose_correction()` to call `selected_attention()` so a Recent row can never be corrected as though it were Attention. Leave `navigation_effect()` and session actions routed through `selected_live_activity()`.

- [ ] **Step 4: Run focused and crate tests and verify GREEN**

Run:

```bash
cargo test -p coding-brain-tui live_
cargo test -p coding-brain-tui
```

Expected: all new selection tests pass, existing navigation/correction/action tests pass, and the crate reports zero failures.

---

### Task 2: Render responsive Live geometry and document list jumps

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs:1-157`
- Modify: `crates/coding-brain-tui/src/ui/brain/mod.rs:99-121`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs:123-610`

**Interfaces:**
- Consumes: Task 1's `selected_attention_index()` and `selected_recent_index()`
- Produces: wide/narrow Live geometry, content-bounded Evidence, and footer copy containing `J/K lists`

**Acceptance Criteria:**
- At 120 columns and wider, Needs Attention and Recent are stacked on the left beside a persistent Evidence pane.
- Below 120 columns, panes remain vertical and Evidence consumes its wrapped content height, capped at 12 rows.
- Both layouts render distinct, non-overlapping borders and complete `Needs Attention`, `Recent`, and `Evidence` labels.
- Highlight state remains on exactly one visible row and evidence matches that row after list jumps.
- The Live footer advertises `J/K lists`.
- Existing empty, offline, overflow, status-copy, Review, and Scorecard rendering remains intact.

- [ ] **Step 1: Add failing TestBackend regressions for the breakpoint and selection stability**

Generalize the existing helper without changing its 110x38 default:

```rust
fn render_text_at(app: &BrainApp, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_text(app: &BrainApp) -> String {
    render_text_at(app, 110, 38)
}

fn title_position(text: &str, title: &str) -> (usize, usize) {
    text.lines()
        .enumerate()
        .find_map(|(row, line)| line.find(title).map(|column| (row, column)))
        .unwrap_or_else(|| panic!("missing title {title}:\n{text}"))
}
```

Add a breakpoint test using the same populated Live fixture at widths 119 and 120:

```rust
fn populated_live_app_with_note(note: Option<String>) -> BrainApp {
    let mut attention = activity("attention-1", DeliveryState::Unknown);
    attention.note = note;
    let mut recent = activity("recent-1", DeliveryState::Delivered);
    recent.state = ActivityState::Allowed;
    fixture_app(MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            attention: vec![AttentionItem {
                activity: attention,
                occurrences: 1,
                unresolved_occurrences: 1,
            }],
            recent: vec![recent],
            unresolved_count: 1,
            diagnostics: Default::default(),
        },
        endpoint_health: online(),
        ..MockBrainRuntime::default()
    })
}

fn populated_live_app() -> BrainApp {
    populated_live_app_with_note(None)
}

#[test]
fn live_switches_to_side_by_side_evidence_at_120_columns() {
    let app = populated_live_app();
    let narrow = render_text_at(&app, 119, 38);
    let wide = render_text_at(&app, 120, 38);

    let (narrow_attention_row, _) = title_position(&narrow, "Needs Attention");
    let (narrow_recent_row, _) = title_position(&narrow, "Recent");
    let (narrow_evidence_row, _) = title_position(&narrow, "Evidence");
    assert!(narrow_attention_row < narrow_recent_row);
    assert!(narrow_recent_row < narrow_evidence_row);

    let (wide_attention_row, _) = title_position(&wide, "Needs Attention");
    let (wide_recent_row, _) = title_position(&wide, "Recent");
    let (wide_evidence_row, wide_evidence_column) = title_position(&wide, "Evidence");
    assert_eq!(wide_attention_row, wide_evidence_row);
    assert!(wide_recent_row > wide_attention_row);
    assert!(wide_evidence_column >= 75);
}
```

Add the bounded-height and list-jump tests:

```rust
#[test]
fn live_narrow_evidence_height_is_content_bounded() {
    let app = populated_live_app_with_note(Some("wrapped evidence ".repeat(200)));

    let text = render_text_at(&app, 119, 73);
    let (evidence_top, _) = title_position(&text, "Evidence");
    let footer_text = text
        .lines()
        .position(|line| line.contains("j/k select"))
        .expect("Live footer");

    assert!(footer_text - evidence_top - 1 <= 12, "{text}");
    assert!(title_position(&text, "Recent").0 < evidence_top);
}

#[test]
fn live_list_jumps_keep_highlight_and_evidence_in_sync() {
    let mut app = populated_live_app();

    app.handle_key(key(KeyCode::Char('J')));
    let recent = render_text_at(&app, 120, 38);
    assert_eq!(recent.matches("> ").count(), 1);
    assert!(
        recent
            .lines()
            .any(|line| line.contains("> ") && line.contains("recent-1"))
    );
    assert!(recent.contains("Activity: recent-1"));

    app.handle_key(key(KeyCode::Char('K')));
    let attention = render_text_at(&app, 120, 38);
    assert_eq!(attention.matches("> ").count(), 1);
    assert!(
        attention
            .lines()
            .any(|line| line.contains("> ") && line.contains("attention-1"))
    );
    assert!(attention.contains("Activity: attention-1"));
}
```

- [ ] **Step 2: Run the rendering regressions and verify RED**

Run:

```bash
cargo test -p coding-brain-tui live_switches_to_side_by_side_evidence_at_120_columns
cargo test -p coding-brain-tui live_narrow_evidence_height_is_content_bounded
cargo test -p coding-brain-tui live_list_jumps_keep_highlight_and_evidence_in_sync
```

Expected: the breakpoint test fails because both widths still use the fixed vertical layout; the bounded-height and jump tests fail because Evidence is unbounded and rendering still uses the combined index.

- [ ] **Step 3: Extract one Evidence paragraph builder**

Replace the duplicated selected/empty rendering branches with:

```rust
fn evidence_lines(item: &ActivityItem) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(activity_status(item)),
        ]),
        Line::raw(format!("Activity: {}", item.activity_id)),
        Line::raw(format!("Provider: {}", provider_label(item))),
        Line::raw(format!("Project: {}", project_label(item))),
        Line::raw(format!("Command: {}", command_label(item))),
    ];
    if let Some(confidence) = item.confidence {
        lines.push(Line::raw(format!("Confidence: {:.0}%", confidence * 100.0)));
    }
    if let Some(reasoning) = &item.reasoning {
        lines.push(Line::raw(format!("Reason: {reasoning}")));
    }
    if let Some(correction) = item.correction {
        lines.push(Line::raw(format!("Resolved: {correction:?}")));
    }
    if let Some(note) = &item.note {
        lines.push(Line::raw(format!("Note: {note}")));
    }
    lines
}

fn evidence_paragraph(app: &BrainApp) -> Paragraph<'static> {
    let lines = match app.selected_live_activity() {
        Some(item) => evidence_lines(item),
        None => vec![Line::raw("Select an activity to inspect its evidence")],
    };
    Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().title(" Evidence ").borders(Borders::ALL))
}

fn render_evidence(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    frame.render_widget(evidence_paragraph(app), area);
}
```

Move the current status, activity, provider, project, command, confidence, reason, correction, and note lines into `evidence_lines()`. Do not remove or shorten any field.

- [ ] **Step 4: Implement wide and narrow geometry**

Add:

```rust
const WIDE_BREAKPOINT: u16 = 120;
const MAX_NARROW_EVIDENCE_HEIGHT: u16 = 12;
const MIN_LIST_HEIGHT: u16 = 3;
```

For `area.width >= WIDE_BREAKPOINT`, split horizontally with `Constraint::Percentage(67)` and `Constraint::Percentage(33)`, split the left side vertically at approximately 60/40, render the two lists on the left, and render Evidence in the complete right rectangle.

```rust
fn render_wide(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(67), Constraint::Percentage(33)])
        .split(area);
    let lists = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(columns[0]);
    render_attention(frame, lists[0], app);
    render_recent(frame, lists[1], app);
    render_evidence(frame, columns[1], app);
}
```

For narrower terminals, calculate:

```rust
let desired_evidence_height =
    evidence_paragraph(app).line_count(area.width).min(u16::MAX as usize) as u16;
let evidence_height = desired_evidence_height
    .min(MAX_NARROW_EVIDENCE_HEIGHT)
    .min(area.height.saturating_sub(MIN_LIST_HEIGHT * 2));
```

Allocate `Constraint::Min(MIN_LIST_HEIGHT * 2)` to a combined list region and `Constraint::Length(evidence_height)` to Evidence, then split the list region at approximately 60/40. Ratatui's `Paragraph::line_count` already accounts for wrapping and block borders in version 0.29.

```rust
fn render_narrow(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let desired_evidence_height =
        evidence_paragraph(app).line_count(area.width).min(u16::MAX as usize) as u16;
    let evidence_height = desired_evidence_height
        .min(MAX_NARROW_EVIDENCE_HEIGHT)
        .min(area.height.saturating_sub(MIN_LIST_HEIGHT * 2));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(MIN_LIST_HEIGHT * 2),
            Constraint::Length(evidence_height),
        ])
        .split(area);
    let lists = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(rows[0]);
    render_attention(frame, lists[0], app);
    render_recent(frame, lists[1], app);
    render_evidence(frame, rows[1], app);
}

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    if area.width >= WIDE_BREAKPOINT {
        render_wide(frame, area, app);
    } else {
        render_narrow(frame, area, app);
    }
}
```

Update list state to use Task 1's explicit accessors:

```rust
if let Some(index) = app.selected_attention_index() {
    state.select(Some(index));
}
```

and:

```rust
if let Some(index) = app.selected_recent_index() {
    state.select(Some(index));
}
```

- [ ] **Step 5: Make the jump keys discoverable**

Change only the Live footer's default text:

```rust
"j/k select  J/K lists  Enter switch  x action  c correct  Tab tabs  r refresh  q quit"
```

Do not change input prompts or the Review and Scorecard footers.

- [ ] **Step 6: Run focused and full verification**

Run:

```bash
cargo fmt --check
cargo test -p coding-brain-tui
cargo test
cargo clippy -- -D warnings
cargo build
```

Expected: formatting is clean; all TestBackend, TUI, and workspace tests pass; Clippy emits no warnings; the workspace builds successfully.

- [ ] **Step 7: Inspect the final scope without committing**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-tui/src/brain_app.rs crates/coding-brain-tui/src/ui/brain/live.rs crates/coding-brain-tui/src/ui/brain/mod.rs
git status --short
```

Expected: only the three planned Rust files and this plan are changed; every Rust line maps to `codexctl-q6o`; no commit, push, or tracker sync has occurred.
