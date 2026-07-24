# Recent Live Decision Corrections Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Let operators correct the selected Decision in either Live list, clearly highlight the exact target, and prove that Brain-wrong corrections feed the existing Review and scorecard projections.

**Architecture:** Reuse `BrainApp::selected_live_activity()` and the existing `BrainInput::Correction`/`record_correction` path. Keep runtime validation and projection logic unchanged; add only focused application, renderer, and persisted-projection regressions.

**Tech Stack:** Rust 2024, Ratatui 0.29, Crossterm 0.28, Cargo workspace tests

## Global Constraints

- A correction must target only the `activity_id` captured when its prompt opens; submission must never fall back to the current selection.
- Both Live lists accept corrections only for `ActivityKind::Decision`; runtime validation remains authoritative.
- Use the existing `CorrectionInput`, `BrainActions::record_correction`, refresh, Review, and scorecard paths.
- Selected rows use `theme.header` plus `Modifier::BOLD`; semantic foreground colors may flatten, but textual badges must remain readable.
- Keep `> ` and `HighlightSpacing::Always` on both lists so focus changes never shift content.
- Do not change activity storage, compaction, list membership, ordering, projection semantics, navigation, or layout.
- Do not commit, push, publish, or sync without explicit user authorization.

---

### Task 1: Target corrections through the active Live selection

**Files:**
- Modify: `crates/coding-brain-tui/src/brain_app.rs:383-420`
- Test: `crates/coding-brain-tui/src/brain_app.rs:1548-1630`

**Interfaces:**
- Consumes: `BrainApp::selected_live_activity() -> Option<&ActivityItem>`, `BrainInput::Correction`, and `BrainActions::record_correction(CorrectionInput)`.
- Produces: `begin_correction()` for either Live list and fail-closed `choose_correction(...)` requiring an active correction input.

**Acceptance Criteria:**
- Selecting a Decision in Needs Attention or Recent and pressing `c` opens the existing correction prompt.
- Choosing Brain right, Brain wrong, or Exception records the exact Recent `activity_id` through `record_correction`.
- Calling correction submission without an active correction prompt records nothing and reports `No correction in progress`.
- A non-Decision row in either list remains ineligible and reports the existing Decision-only status.
- Existing Needs Attention correction behavior remains unchanged.

- [ ] **Step 1: Add failing Recent-selection and fail-closed tests**

Add these tests beside `correction_records_right_wrong_or_exception`:

```rust
#[test]
fn recent_decision_correction_records_exact_activity_for_every_disposition() {
    for (key_code, disposition) in [
        ('r', CorrectionDisposition::BrainRight),
        ('w', CorrectionDisposition::BrainWrong),
        ('e', CorrectionDisposition::Exception),
    ] {
        let (mut app, mock) = fixture_app(false);
        let mut recent = activity();
        recent.activity_id = "recent-decision".into();
        app.snapshot.recent = vec![recent];
        app.clamp_selection();

        app.handle_key(key(KeyCode::Char('c')));
        assert!(app.input_prompt().unwrap().starts_with("Correction:"));
        app.handle_key(key(KeyCode::Char(key_code)));
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            non_poll_actions(&mock),
            vec![MockBrainAction::RecordCorrection(CorrectionInput {
                activity_id: "recent-decision".into(),
                disposition,
                note: None,
            })]
        );
    }
}

#[test]
fn correction_submission_without_prompt_fails_closed() {
    let (mut app, mock) = fixture_app(true);

    app.choose_correction(CorrectionDisposition::BrainWrong, None);

    assert_eq!(app.status(), Some("No correction in progress"));
    assert!(non_poll_actions(&mock).is_empty());
}

#[test]
fn diagnostic_recent_does_not_open_correction_input() {
    let (mut app, mock) = fixture_app(false);
    app.snapshot.recent = vec![diagnostic_activity("diagnostic-recent", 1)];
    app.clamp_selection();

    app.handle_key(key(KeyCode::Char('c')));

    assert_eq!(app.input_prompt(), None);
    assert_eq!(
        app.status(),
        Some("Corrections are only available for Decision activity")
    );
    assert!(non_poll_actions(&mock).is_empty());
}
```

- [ ] **Step 2: Run the focused tests and confirm the intended failures**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::recent_decision_correction_records_exact_activity_for_every_disposition -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::correction_submission_without_prompt_fails_closed -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::diagnostic_recent_does_not_open_correction_input -- --nocapture
```

Expected: the Recent tests fail because `begin_correction` reads only Needs Attention, and the direct-submission test fails because `choose_correction` still falls back to selection.

- [ ] **Step 3: Generalize prompt startup and require its captured identity**

Replace the two selection-specific blocks with:

```rust
pub fn begin_correction(&mut self) {
    let Some(item) = self.selected_live_activity() else {
        self.status = Some("Select a Live activity first".into());
        return;
    };
    if item.kind != coding_brain_core::brain_activity::ActivityKind::Decision {
        self.status = Some("Corrections are only available for Decision activity".into());
        return;
    }
    self.input = Some(BrainInput::Correction {
        activity_id: item.activity_id.clone(),
        disposition: None,
        note: String::new(),
    });
}

pub fn choose_correction(&mut self, disposition: CorrectionDisposition, note: Option<String>) {
    let Some(BrainInput::Correction { activity_id, .. }) = &self.input else {
        self.status = Some("No correction in progress".into());
        return;
    };
    let correction = CorrectionInput {
        activity_id: activity_id.clone(),
        disposition,
        note: note.and_then(|note| bounded_note(&note)),
    };
    match self.runtime.actions.record_correction(correction) {
        Ok(()) => {
            self.status = Some("Correction recorded".into());
            self.input = None;
            self.refresh();
        }
        Err(error) => self.status = Some(format!("Could not record correction: {error}")),
    }
}
```

- [ ] **Step 4: Run Task 1 regressions**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::correction_ -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::diagnostic_ -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::action_mode_is_live_only_and_correction_key_is_unchanged -- --nocapture
```

Expected: all selected tests pass; recorded actions use the captured Recent ID and existing Needs Attention coverage remains green.

- [ ] **Step 5: Inspect the task diff without committing**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-tui/src/brain_app.rs
```

Expected: only correction selection, fail-closed submission, and focused tests changed. Do not commit without user authorization.

### Task 2: Highlight the selected row in either Live list

**Files:**
- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs:105-150`
- Test: `crates/coding-brain-tui/src/ui/brain/mod.rs:384-418`
- Test helper: `crates/coding-brain-tui/src/ui/brain/mod.rs:962-979`

**Interfaces:**
- Consumes: `BrainApp::theme()`, `selected_attention_index()`, `selected_recent_index()`, and Ratatui `List::highlight_style`.
- Produces: identical active-row styling for Needs Attention and Recent while preserving the stable cursor column.

**Acceptance Criteria:**
- The selected row in the active Live list uses `theme.header` and `Modifier::BOLD`.
- The inactive list has no selected row style or cursor.
- Dark, light, and no-color themes keep the selected row and textual badge readable.
- Moving focus between lists preserves row content columns and exactly one `> ` cursor.

- [ ] **Step 1: Add a reusable rendered-buffer helper**

Import `ratatui::buffer::Buffer`, then refactor the existing text helper:

```rust
fn render_buffer_at(app: &BrainApp, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| render(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn buffer_text(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_text_at(app: &BrainApp, width: u16, height: u16) -> String {
    buffer_text(&render_buffer_at(app, width, height))
}
```

- [ ] **Step 2: Add the failing theme and focus regression**

Add this test beside `live_list_indentation_stays_fixed_when_selection_moves_between_lists`:

```rust
#[test]
fn live_active_row_uses_theme_highlight_without_shifting_content() {
    for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::None] {
        let mut app = populated_live_app_with_theme(mode);
        let theme = *app.theme();

        let attention_buffer = render_buffer_at(&app, 110, 38);
        let attention_text = buffer_text(&attention_buffer);
        let attention_row = attention_text
            .lines()
            .position(|line| line.contains("attention-1"))
            .unwrap();
        let attention_column =
            content_column(&attention_text, "attention-1", "attention-1");
        let recent_row = attention_text
            .lines()
            .position(|line| line.contains("recent-1"))
            .unwrap();
        let recent_column = content_column(&attention_text, "recent-1", "recent-1");

        assert_eq!(
            attention_buffer[(attention_column as u16, attention_row as u16)].fg,
            theme.header
        );
        assert!(
            attention_buffer[(attention_column as u16, attention_row as u16)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !attention_buffer[(recent_column as u16, recent_row as u16)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(attention_text.matches("> ").count(), 1);

        app.handle_key(key(KeyCode::Char('J')));
        let recent_buffer = render_buffer_at(&app, 110, 38);
        let recent_text = buffer_text(&recent_buffer);
        let attention_row_after = recent_text
            .lines()
            .position(|line| line.contains("attention-1"))
            .unwrap();
        let attention_column_after =
            content_column(&recent_text, "attention-1", "attention-1");
        let recent_row_after = recent_text
            .lines()
            .position(|line| line.contains("recent-1"))
            .unwrap();
        let recent_column_after = content_column(&recent_text, "recent-1", "recent-1");

        assert!(
            !recent_buffer[(attention_column_after as u16, attention_row_after as u16)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            recent_buffer[(recent_column_after as u16, recent_row_after as u16)].fg,
            theme.header
        );
        assert!(
            recent_buffer[(recent_column_after as u16, recent_row_after as u16)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(attention_column, attention_column_after);
        assert_eq!(recent_column, recent_column_after);
        assert!(
            recent_text
                .lines()
                .nth(recent_row_after)
                .unwrap()
                .contains("ALLOW")
        );
        assert_eq!(recent_text.matches("> ").count(), 1);
    }
}
```

Add a small themed fixture rather than duplicating snapshot setup:

```rust
fn populated_live_app_with_theme(mode: ThemeMode) -> BrainApp {
    let mut attention = activity("attention-1", DeliveryState::Unknown);
    attention.normalized_command = Some("attention-1".into());
    let mut recent = activity("recent-1", DeliveryState::Delivered);
    recent.state = ActivityState::Allowed;
    fixture_app_with_theme(
        MockBrainRuntime {
            activity_snapshot: ActivitySnapshot {
                attention: vec![AttentionItem {
                    activity: attention,
                    occurrences: 1,
                    unresolved_occurrences: 1,
                }],
                recent: vec![recent],
                unresolved_count: 1,
                ..ActivitySnapshot::default()
            },
            endpoint_health: online(),
            ..MockBrainRuntime::default()
        },
        mode,
    )
}
```

- [ ] **Step 3: Run the renderer regression and confirm it fails**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::live_active_row_uses_theme_highlight_without_shifting_content -- --nocapture
```

Expected: FAIL because both Live lists currently use `Style::default()` as their highlight style.

- [ ] **Step 4: Apply the shared theme-aware style to both lists**

In both `render_attention` and `render_recent`, replace the default highlight with:

```rust
.highlight_style(
    Style::default()
        .fg(app.theme().header)
        .add_modifier(Modifier::BOLD),
)
.highlight_symbol("> ")
.highlight_spacing(HighlightSpacing::Always);
```

Do not change when each `ListState` selects an index.

- [ ] **Step 5: Run Task 2 regressions**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::live_active_row_uses_theme_highlight_without_shifting_content -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::live_list_indentation_stays_fixed_when_selection_moves_between_lists -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::live_list_jumps_keep_highlight_and_evidence_in_sync -- --nocapture
```

Expected: all selected tests pass in all three theme modes with one cursor and stable columns.

- [ ] **Step 6: Inspect the task diff without committing**

Run:

```bash
git diff --check
git diff -- crates/coding-brain-tui/src/ui/brain/live.rs crates/coding-brain-tui/src/ui/brain/mod.rs
```

Expected: identical highlight configuration in the two Live lists plus focused buffer-style coverage. Do not commit without user authorization.

### Task 3: Prove persisted Brain-wrong projection and run release gates

**Files:**
- Test: `src/runtime/brain.rs:600-790`
- Verify: entire Cargo workspace

**Interfaces:**
- Consumes: `record_correction_at_path`, `ActivityStore`, `review_queue_from`, and `scorecard_from`.
- Produces: one persisted-event regression proving that the same correction consumed after refresh updates both existing projections.

**Acceptance Criteria:**
- A Brain-wrong correction persisted for an eligible automatic decision enters the existing Review queue.
- The same persisted correction updates scorecard accuracy through the existing latest-correction overlay.
- No new runtime interface, fake runtime, storage path, or projection path is introduced.
- The persisted projection test is characterization coverage and passes on its first run without production changes.
- TUI tests and all repository quality gates pass.

- [ ] **Step 1: Add the persisted projection regression**

Add this test beside the existing correction and Review tests:

```rust
#[test]
fn persisted_brain_wrong_correction_updates_review_and_scorecard_projections() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("activity.jsonl");
    let store = brain::activity::ActivityStore::at(path.clone());
    let mut source = source_event(ActivityKind::Decision);
    source.decision_id = Some("review".into());
    store.append(source).unwrap();

    record_correction_at_path(
        &path,
        CorrectionInput {
            activity_id: "activity-1".into(),
            disposition: CorrectionDisposition::BrainWrong,
            note: None,
        },
    )
    .unwrap();

    let events = store.read().unwrap();
    let review = review_queue_from(vec![review_record()], events.events());
    let scorecard = scorecard_from(
        &[summary(
            "review",
            "approve",
            Some("hook_proposal"),
            Some("Bash"),
            Some("rm -rf /tmp/build"),
        )],
        events.events(),
    );

    assert_eq!(review.len(), 1);
    assert_eq!(review[0].decision.id, "review");
    assert_eq!(scorecard.total_decisions, 1);
    assert_eq!(scorecard.brain_decisions, 1);
    assert_eq!(scorecard.correct_decisions, 0);
    assert_eq!(scorecard.accuracy_pct, 0.0);
}
```

- [ ] **Step 2: Run the new runtime test**

Run:

```bash
nix develop path:. --command cargo test --bin coding-brain runtime::brain::tests::persisted_brain_wrong_correction_updates_review_and_scorecard_projections -- --nocapture
```

Expected: PASS on the first run without production changes; this is intentional characterization/integration coverage. If it fails, investigate the existing contract before changing production code.

- [ ] **Step 3: Run focused TUI and runtime suites**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui brain_app::tests::correction_ -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui ui::brain::tests::live_ -- --nocapture
nix develop path:. --command cargo test --bin coding-brain runtime::brain::tests -- --nocapture
```

Expected: all selected tests pass.

- [ ] **Step 4: Run repository quality gates**

Run:

```bash
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo build
```

Expected: every command exits successfully with no test failures or warnings.

- [ ] **Step 5: Verify scope and hand off without publishing**

Run:

```bash
git diff --check
git diff --stat
git status --short --branch
```

Expected: changes are limited to the approved spec and plan, `brain_app.rs`, `live.rs`, `ui/brain/mod.rs`, and the focused runtime test in `src/runtime/brain.rs`. Do not commit, push, publish, or sync without explicit user authorization.

## Stress Test Results: Recent Live Decision Corrections Implementation Plan

### Resolved Decisions

- Tasks 1 and 2 remain independent; Task 3 depends on both before full validation.
- Renderer coverage verifies highlight ownership in both directions for every theme.
- The persisted runtime projection test is characterization coverage and must not induce speculative production changes.
- Verification uses the currently working `nix develop path:. --command cargo` environment instead of changing blocked direnv trust.
- Implementation remains local and surgical, with runtime validation authoritative and no commit or publication authority implied.

### Changes Made

- Strengthened the renderer regression to render before and after the list jump.
- Clarified the runtime test's expected first-run pass and investigation boundary.
- Replaced blocked direnv commands with the verified Nix development shell.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High
- Areas of concern: The renderer test intentionally asserts structural cells rather than a full-screen golden and must retain stable fixture command text.
