# Live Evidence Relative Age Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> beads-superpowers:subagent-driven-development (recommended) or
> beads-superpowers:executing-plans to implement this plan task-by-task. Each
> Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within
> tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Show a deterministic relative age for selected Recent Live activity
without changing actionable Evidence wording or activity data.

**Architecture:** Keep the change inside the Live TUI renderer. Capture the
current epoch time once at the render boundary, thread it through the existing
shared Evidence builder, and format age with a pure helper so both densities and
clock edge cases are deterministic in tests.

**Tech Stack:** Rust 2024, Ratatui 0.29, Cargo tests.

## Global Constraints

- Recent age units are floored whole seconds, minutes, hours, or days.
- Future or clock-skewed timestamps render as `0s ago`.
- Attention Evidence continues to render `Needs attention`.
- Do not add persistence fields, runtime state, dependencies, or schema changes.
- Preserve badges, Outcome, Action, Context, selection, scrolling, and wrapping.
- Do not commit, push, or publish without explicit authorization.

---

### Task 1: Render deterministic relative age in Live Evidence

**Files:**

- Modify: `crates/coding-brain-tui/src/ui/brain/live.rs`
- Test: `crates/coding-brain-tui/src/ui/brain/live.rs`

**Interfaces:**

- Consumes: `ActivityItem.recorded_at_ms: u64` and one render-time epoch value.
- Produces: `relative_age(recorded_at_ms: u64, now_ms: u64) -> String`; updated
  internal Live render helpers that receive the same `now_ms`.

**Acceptance Criteria:**

- Selected Recent activity renders `Ns ago`, `Nm ago`, `Nh ago`, or `Nd ago`
  in both wide and compact Evidence.
- Unit boundaries use floored values and future timestamps render `0s ago`.
- Selected actionable activity still renders `Needs attention`.
- The current time is sampled once per Live render and no application state is
  added.
- Existing Live renderer tests and all repository quality gates pass.

- [ ] **Step 1: Add failing pure formatting tests**

Add a table-driven test beside the existing `live.rs` unit tests:

```rust
#[test]
fn relative_age_uses_floored_units_and_clamps_future_timestamps() {
    const NOW_MS: u64 = 1_000_000_000;
    for (elapsed_ms, expected) in [
        (0, "0s ago"),
        (12_000, "12s ago"),
        (59_999, "59s ago"),
        (60_000, "1m ago"),
        (3_599_999, "59m ago"),
        (3_600_000, "1h ago"),
        (86_399_999, "23h ago"),
        (86_400_000, "1d ago"),
        (172_800_000, "2d ago"),
    ] {
        assert_eq!(relative_age(NOW_MS - elapsed_ms, NOW_MS), expected);
    }
    assert_eq!(relative_age(NOW_MS + 1, NOW_MS), "0s ago");
}
```

- [ ] **Step 2: Run the formatting test and verify it fails**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui relative_age_uses_floored_units_and_clamps_future_timestamps
```

Expected: compilation fails because `relative_age` does not exist.

- [ ] **Step 3: Implement the minimal pure formatter**

Add constants and a helper near `EvidenceDensity`:

```rust
const SECOND_MS: u64 = 1_000;
const MINUTE_MS: u64 = 60 * SECOND_MS;
const HOUR_MS: u64 = 60 * MINUTE_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

fn relative_age(recorded_at_ms: u64, now_ms: u64) -> String {
    let elapsed_ms = now_ms.saturating_sub(recorded_at_ms);
    if elapsed_ms < MINUTE_MS {
        format!("{}s ago", elapsed_ms / SECOND_MS)
    } else if elapsed_ms < HOUR_MS {
        format!("{}m ago", elapsed_ms / MINUTE_MS)
    } else if elapsed_ms < DAY_MS {
        format!("{}h ago", elapsed_ms / HOUR_MS)
    } else {
        format!("{}d ago", elapsed_ms / DAY_MS)
    }
}
```

- [ ] **Step 4: Run the formatting test and verify it passes**

Run the Step 2 command again.

Expected: one test passes.

- [ ] **Step 5: Add failing Evidence-density tests with controlled time**

Add tests that call the semantic Evidence builder directly:

```rust
#[test]
fn recent_evidence_uses_relative_age_in_both_densities() {
    const NOW_MS: u64 = 1_000_000_000;
    let mut item = activity();
    item.recorded_at_ms = NOW_MS - 4 * MINUTE_MS;
    let theme = Theme::from_mode(ThemeMode::Dark);

    for density in [EvidenceDensity::Wide, EvidenceDensity::Compact] {
        let text = evidence_lines(&item, density, &theme, false, NOW_MS)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("4m ago"), "{text}");
        assert!(!text.contains("Recent"), "{text}");
    }
}

#[test]
fn attention_evidence_keeps_actionable_label_in_both_densities() {
    const NOW_MS: u64 = 1_000_000_000;
    let mut item = activity();
    item.recorded_at_ms = NOW_MS - 4 * MINUTE_MS;
    let theme = Theme::from_mode(ThemeMode::Dark);

    for density in [EvidenceDensity::Wide, EvidenceDensity::Compact] {
        let text = evidence_lines(&item, density, &theme, true, NOW_MS)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Needs attention"), "{text}");
        assert!(!text.contains("4m ago"), "{text}");
    }
}
```

- [ ] **Step 6: Run the Evidence tests and verify they fail**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui evidence_ -- --nocapture
```

Expected: compilation fails because `evidence_lines` does not yet accept
`now_ms`.

- [ ] **Step 7: Thread one clock sample through the renderer**

Import `SystemTime` and `UNIX_EPOCH`, add a bounded epoch conversion, and pass
the same value through `render_wide`, `render_narrow`, `evidence_height`,
`render_evidence`, and `evidence_lines`:

```rust
use std::time::{SystemTime, UNIX_EPOCH};

pub fn render(frame: &mut Frame<'_>, area: Rect, app: &BrainApp) {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    if area.width >= WIDE_BREAKPOINT {
        render_wide(frame, area, app, now_ms);
    } else {
        render_narrow(frame, area, app, now_ms);
    }
}
```

In `evidence_lines`, replace the fixed Recent string with a derived owned value:

```rust
let context_label = needs_attention
    .then(|| "Needs attention".to_owned())
    .unwrap_or_else(|| relative_age(item.recorded_at_ms, now_ms));
```

Use `context_label` for the existing wide header and compact Status spans.
Change no other Evidence content or layout behavior.

- [ ] **Step 8: Run focused TUI tests**

Run:

```bash
direnv exec . cargo test -p coding-brain-tui
```

Expected: all `coding-brain-tui` tests pass.

- [ ] **Step 9: Run repository verification**

Run:

```bash
direnv exec . cargo fmt --check
direnv exec . cargo test
direnv exec . cargo clippy -- -D warnings
direnv exec . cargo build
git -c core.whitespace=blank-at-eol,blank-at-eof,space-before-tab diff --check
git status --short
```

Expected: formatting, tests, Clippy, build, and whitespace checks pass; status
contains only the approved spec, plan, and `live.rs` changes.

- [ ] **Step 10: Record verified completion without publishing**

Close the implementation task and `codexctl-zsyk` with the exact verification
evidence. Report the changed files and leave commit, push, and publication for
explicit user authorization.
