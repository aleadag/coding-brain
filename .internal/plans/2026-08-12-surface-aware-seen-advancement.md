# Surface-Aware Seen Advancement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use beads-superpowers:subagent-driven-development (recommended) or beads-superpowers:executing-plans to implement this plan task-by-task. Each Task becomes a bead (`bd create -t task --parent <epic-id>`). Steps within tasks use checkbox (`- [ ]`) syntax for human readability.

**Goal:** Advance a successful explicit single-item seen action to its pre-mutation successor on every review surface without moving selection on mutation or refresh failure.

**Architecture:** Capture the active surface plus the successor's display identity and index before mutation. Return the existing `ReviewMutationResult` from successful submissions, require its surface to match the captured surface, then apply the existing surface-aware selection-restoration helper only when the refreshed projection has observed that revision and still contains the successor identity. Re-clamp and explicitly reset the active evidence viewport after advancement; repeated review display identities otherwise cannot reveal a row-index change to the viewport identity check.

**Tech Stack:** Rust, Cargo workspace, `coding-brain-tui`, in-module unit tests, Nix development shell

## Global Constraints

- Change production behavior only in `crates/coding-brain-tui/src/brain_app.rs`.
- Do not change runtime or storage contracts, projection construction, rendering, or core mock APIs.
- Do not store pending advancement state across refreshes.
- Preserve bulk mark-seen, archive, undo, correction, navigation, periodic refresh, and Review `s` behavior.
- Keep mutation authority unchanged: the request still uses the visibly selected target's keys and expected surface revision.
- Treat a mismatched mutation-result surface as non-advancing; it must never redirect selection.
- Commit only after explicit user authorization under the repository's conservative workflow.

---

### Task 1: Implement and verify surface-aware post-success advancement

**Files:**

- Modify: `crates/coding-brain-tui/src/brain_app.rs:14-18`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:450-500`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:960-1000`
- Modify: `crates/coding-brain-tui/src/brain_app.rs:1090-1160`
- Test: `crates/coding-brain-tui/src/brain_app.rs:3010-3050`
- Test helpers: `crates/coding-brain-tui/src/brain_app.rs:3270-3370` and `3740-3860`

**Interfaces:**

- Consumes: `ReviewMutationResult { surface, surface_revision, .. }`, `BrainApp::review_projection`, and `BrainApp::restore_surface_selection`.
- Produces: `BrainApp::submit_review_mutation(...) -> Option<ReviewMutationResult>` and surface-aware `BrainApp::review_selected()` behavior.

**Acceptance Criteria:**

- A successful `a` action advances to the item that immediately followed the selected item in the active surface's pre-mutation ordering on Attention, Recent, Review, and Diagnostics.
- Advancement occurs only when the refreshed projection reaches the successful mutation result's surface revision and still contains the successor identity.
- The successor's captured index selects the nearest matching row when display identities repeat.
- Mismatched result surfaces, missing successors, last/one/empty surfaces, mutation failures, post-success refresh failures, and older coherent refreshes preserve normal identity restoration and safe clamping without wrapping.
- Successful advancement resets the active Live or Diagnostics evidence viewport just like keyboard navigation.
- Review `s` and all unrelated mutation, refresh, and navigation behavior remain unchanged.
- Focused TUI tests and workspace format, test, Clippy, and build gates pass.

- [x] **Step 1: Add failing success, boundary, and failure regressions**

Add these tests beside `review_selected_new_keys_is_immediate` and retain the existing `review_skip_persists_then_advances_and_failure_preserves_selection` test:

```rust
#[test]
fn mark_seen_advances_on_every_review_surface() {
    for surface in [
        ReviewSurface::Attention,
        ReviewSurface::Recent,
        ReviewSurface::Review,
        ReviewSurface::Diagnostics,
    ] {
        let (mut app, mock) = review_fixture(surface, 2, 0);

        app.handle_key(key(KeyCode::Char('a')));

        assert_eq!(app.selection(), 1, "{surface:?}");
        assert_eq!(review_mutations(&mock).len(), 1, "{surface:?}");
    }
}

#[test]
fn mark_seen_last_single_and_empty_surfaces_do_not_wrap() {
    for surface in [
        ReviewSurface::Attention,
        ReviewSurface::Recent,
        ReviewSurface::Review,
        ReviewSurface::Diagnostics,
    ] {
        let (mut last, _) = review_fixture(surface, 2, 0);
        last.handle_key(key(KeyCode::Char('j')));
        last.handle_key(key(KeyCode::Char('a')));
        assert_eq!(last.selection(), 1, "last {surface:?}");

        let (mut single, _) = review_fixture(surface, 1, 0);
        single.handle_key(key(KeyCode::Char('a')));
        assert_eq!(single.selection(), 0, "single {surface:?}");

        let (mut empty, _) = review_fixture(surface, 0, 0);
        empty.handle_key(key(KeyCode::Char('a')));
        assert_eq!(empty.selection(), 0, "empty {surface:?}");
    }
}

#[test]
fn mark_seen_uses_successor_index_when_attention_display_ids_repeat() {
    let attention = ["duplicate-1", "duplicate-2"].map(|activity_id| {
        let mut item = activity();
        item.activity_id = activity_id.into();
        item.fingerprint = Some("shared-fingerprint".into());
        AttentionItem {
            activity: item,
            occurrences: 1,
            unresolved_occurrences: 1,
        }
    });
    let mock = Arc::new(aligned_mock(MockBrainRuntime {
        activity_snapshot: ActivitySnapshot {
            attention: attention.into(),
            unresolved_count: 2,
            ..ActivitySnapshot::default()
        },
        ..MockBrainRuntime::default()
    }));
    let runtime = BrainRuntime::new(mock.clone(), mock);
    let mut app = BrainApp::new(runtime, Theme::from_mode(ThemeMode::Dark));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(
        app.selected_attention().unwrap().activity.activity_id,
        "duplicate-2"
    );
}

#[test]
fn mark_seen_failures_preserve_selection() {
    for error in [
        ReviewMutationError::Busy,
        ReviewMutationError::StaleRevision,
        ReviewMutationError::DurabilityUncertain,
        ReviewMutationError::Other("SQLite storage unavailable (io)".into()),
    ] {
        let (mut app, mock) = review_fixture(ReviewSurface::Attention, 2, 0);
        mock.fail_next_review_mutation(error.clone());

        app.handle_key(key(KeyCode::Char('a')));

        assert_eq!(app.selection(), 0, "{error:?}");
    }
}

#[test]
fn mark_seen_advancement_resets_live_evidence_scroll() {
    let (mut app, _) = review_fixture(ReviewSurface::Attention, 2, 0);
    app.update_live_evidence_metrics(5, 12);
    app.handle_key(key(KeyCode::PageDown));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(app.selection(), 1);
    assert_eq!(app.live_evidence_scroll(), 0);
}

#[test]
fn mark_seen_advancement_resets_diagnostics_evidence_scroll() {
    let (mut app, _) = review_fixture(ReviewSurface::Diagnostics, 2, 0);
    app.update_diagnostics_evidence_metrics(5, 12);
    app.handle_key(key(KeyCode::PageDown));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(app.selection(), 1);
    assert_eq!(app.diagnostics_evidence_scroll(), 0);
}
```

Add a local scripted review fixture using the existing `ScriptedBrainSource`; this keeps refresh-boundary behavior in the TUI test module without changing core mocks:

```rust
fn scripted_review_app(
    initial: BrainRefresh,
    after_mutation: Result<BrainRefresh, BrainSourceError>,
) -> BrainApp {
    let actions = Arc::new(MockBrainRuntime {
        activity_snapshot: initial.snapshot.clone(),
        review_queue: initial.review_queue.clone(),
        review_state: initial.review_state.clone(),
        ..MockBrainRuntime::default()
    });
    let source = Arc::new(ScriptedBrainSource {
        refreshes: std::sync::Mutex::new(
            [Ok(initial), after_mutation].into_iter().collect(),
        ),
    });
    BrainApp::new(
        BrainRuntime::new(source, actions),
        Theme::from_mode(ThemeMode::Dark),
    )
}

fn reviewed_attention_refresh(activity_ids: &[&str], revision: u64) -> BrainRefresh {
    let mut refresh = mixed_live_refresh(activity_ids, &[]);
    let reviewed = std::mem::take(
        &mut refresh.review_state.attention.items[0].new_member_keys,
    );
    refresh.review_state.attention.new_count -= reviewed.len();
    refresh.review_state.attention.reviewed_count += reviewed.len();
    refresh.review_state.attention.items[0].reviewed_member_keys = reviewed;
    refresh.review_state.attention.revision = revision;
    refresh
}

#[test]
fn mark_seen_missing_successor_keeps_normal_refreshed_selection() {
    let initial = mixed_live_refresh(
        &["attention-selected", "attention-next", "attention-other"],
        &[],
    );
    let refreshed = reviewed_attention_refresh(
        &["attention-selected", "attention-other"],
        1,
    );
    let mut app = scripted_review_app(initial, Ok(refreshed));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(
        app.selected_attention().unwrap().activity.activity_id,
        "attention-selected"
    );
}

#[test]
fn mark_seen_success_with_failed_refresh_does_not_advance_optimistically() {
    let initial = mixed_live_refresh(
        &["attention-selected", "attention-next"],
        &[],
    );
    let mut app = scripted_review_app(initial, Err(BrainSourceError::Busy));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(app.selection(), 0);
    assert_eq!(
        app.selected_attention().unwrap().activity.activity_id,
        "attention-selected"
    );
}

#[test]
fn mark_seen_success_with_older_refresh_does_not_advance_optimistically() {
    let initial = mixed_live_refresh(
        &["attention-selected", "attention-next"],
        &[],
    );
    let older = mixed_live_refresh(
        &["attention-selected", "attention-next"],
        &[],
    );
    let mut app = scripted_review_app(initial, Ok(older));

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(app.selection(), 0);
}
```

Add a focused local action stub that returns a successful result for a different surface:

```rust
struct MismatchedSurfaceActions;

impl BrainActions for MismatchedSurfaceActions {
    fn mutate_review_state(
        &self,
        _request: ReviewMutationRequest,
    ) -> Result<ReviewMutationResult, ReviewMutationError> {
        Ok(ReviewMutationResult {
            surface: ReviewSurface::Review,
            surface_revision: 1,
            reviewed_count: 1,
            archived_count: 0,
            last_archive_count: 0,
        })
    }

    fn record_correction(&self, _correction: CorrectionInput) -> Result<(), String> {
        unreachable!("review fixture does not record corrections")
    }

    fn mark_canonical(&self, _decision_id: &str, _note: Option<String>) -> Result<(), String> {
        unreachable!("review fixture does not mark canonical decisions")
    }

    fn preflight_session_action(
        &self,
        _request: SessionActionPreflightRequest,
    ) -> Result<SessionActionAvailability, SessionActionFailure> {
        unreachable!("review fixture does not preflight session actions")
    }

    fn send_session_action(
        &self,
        _request: SessionActionRequest,
    ) -> Result<(), SessionActionFailure> {
        unreachable!("review fixture does not send session actions")
    }
}

#[test]
fn mark_seen_result_surface_mismatch_cannot_cross_lookup_by_display_identity() {
    let mut initial = diagnostics_refresh(&["diagnostic-selected", "shared-successor"]);
    let mut first = decision();
    first.id = "review-first".into();
    let mut collision = decision();
    collision.id = "shared-successor".into();
    initial.review_queue = [first, collision]
        .into_iter()
        .map(|decision| ReviewItemSummary {
            decision,
            reason: "fixture".into(),
            score: 1.0,
        })
        .collect();
    let initial = aligned_refresh(initial);
    let mut refreshed = initial.clone();
    let reviewed = std::mem::take(
        &mut refreshed.review_state.diagnostics.items[0].new_member_keys,
    );
    refreshed.review_state.diagnostics.new_count -= reviewed.len();
    refreshed.review_state.diagnostics.reviewed_count += reviewed.len();
    refreshed.review_state.diagnostics.items[0].reviewed_member_keys = reviewed;
    refreshed.review_state.diagnostics.revision = 1;
    refreshed.review_state.review.revision = 1;
    let source = Arc::new(ScriptedBrainSource {
        refreshes: std::sync::Mutex::new(
            [Ok(initial), Ok(refreshed)].into_iter().collect(),
        ),
    });
    let mut app = BrainApp::new(
        BrainRuntime::new(source, Arc::new(MismatchedSurfaceActions)),
        Theme::from_mode(ThemeMode::Dark),
    );
    for _ in 0..3 {
        app.handle_key(key(KeyCode::Tab));
    }

    app.handle_key(key(KeyCode::Char('a')));

    assert_eq!(app.selection(), 0);
    assert_eq!(
        app.selected_diagnostic().unwrap().activity_id,
        "diagnostic-selected"
    );
}
```

The Review projection deliberately contains the Diagnostics successor identity at index 1. Without the captured-surface equality guard, a result-directed lookup would move the shared non-Live selection to the wrong Diagnostics row.

- [x] **Step 2: Run the focused tests and confirm the regression**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui mark_seen -- --nocapture
```

Expected: `mark_seen_advances_on_every_review_surface` fails because `a` still calls `review_selected(false)` and selection remains `0`; the new boundary tests must compile and expose any optimistic, stale-revision, cross-surface, viewport, or fallback advancement once the main action begins advancing.

- [x] **Step 3: Return the mutation result and implement guarded surface-aware advancement**

Add `ReviewMutationResult` to the existing `coding_brain_core::review_state` import:

```rust
use coding_brain_core::review_state::{
    BrainReviewProjection, ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest,
    ReviewMutationResult, ReviewSurface, ReviewTarget, SurfaceReviewProjection,
};
```

Remove the now-unused policy boolean and make both `a` and Review `s` call the single persist-then-advance action:

```rust
KeyCode::Char('a') => {
    self.review_selected();
    None
}

KeyCode::Char('s') if self.tab == BrainTab::Review => {
    self.review_selected();
    None
}
```

Capture the successor identity and its own index, consume the successful mutation result, verify the refreshed revision and identity, and reuse the surface-aware helper:

```rust
fn review_selected(&mut self) {
    if self.lifecycle_action_is_blocked() {
        return;
    }
    let Some((projection, target)) = self.selected_review_target() else {
        return;
    };
    let keys = target
        .new_member_keys
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if keys.is_empty() {
        self.status = Some("No NEW items on this selection".into());
        return;
    }
    let surface = target.surface;
    let next_index = self.selection() + 1;
    let next = projection
        .items
        .get(next_index)
        .map(|target| (target.display_id.clone(), next_index));
    let request = ReviewMutationRequest {
        surface: target.surface,
        expected_surface_revision: projection.revision,
        operation: ReviewMutation::SetDisposition {
            keys,
            disposition: ReviewDisposition::Reviewed,
        },
    };
    if let Some(result) = self.submit_review_mutation(request, None)
        && result.surface == surface
        && let Some((next_display_id, next_index)) = next
        && self.review_projection(surface).revision >= result.surface_revision
        && self
            .review_projection(surface)
            .items
            .iter()
            .any(|target| target.display_id == next_display_id)
    {
        self.restore_surface_selection(surface, Some(&next_display_id), next_index);
        self.clamp_selection();
        match surface {
            ReviewSurface::Attention | ReviewSurface::Recent => {
                self.reset_live_evidence_scroll();
            }
            ReviewSurface::Diagnostics => self.diagnostics_evidence.reset(),
            ReviewSurface::Review => {}
        }
    }
}
```

Change the shared helper to return the runtime's existing result without changing its status or refresh behavior:

```rust
fn submit_review_mutation(
    &mut self,
    request: ReviewMutationRequest,
    retry_prompt: Option<String>,
) -> Option<ReviewMutationResult> {
    let success_status = review_success_status(&request);
    match self.runtime.actions.mutate_review_state(request.clone()) {
        Ok(result) => {
            self.refresh();
            self.status = Some(success_status);
            Some(result)
        }
        Err(ReviewMutationError::Busy) => {
            self.status = Some("Review state is busy; retry when ready".into());
            if let Some(prompt) = retry_prompt {
                self.input = Some(BrainInput::ReviewConfirmation { request, prompt });
            }
            None
        }
        Err(ReviewMutationError::DurabilityUncertain) => {
            self.review_mutations_blocked_until_refresh = true;
            self.status = Some("Review state durability is uncertain; refresh required".into());
            None
        }
        Err(
            ReviewMutationError::StaleRevision
            | ReviewMutationError::TargetNoLongerEligible
            | ReviewMutationError::CountMismatch
            | ReviewMutationError::DispositionConflict,
        ) => {
            self.refresh();
            self.status = Some("Review state changed; refresh and retry".into());
            None
        }
        Err(error) => {
            self.status = Some(format!(
                "Could not update review state: {}",
                bounded_status(&error.to_string())
            ));
            None
        }
    }
}
```

Because `Option` is `#[must_use]`, explicitly discard the result in the confirmation and undo callers:

```rust
let _ = self.submit_review_mutation(request, Some(prompt));
```

```rust
let _ = self.submit_review_mutation(
    ReviewMutationRequest {
        surface,
        expected_surface_revision: projection.revision,
        operation: ReviewMutation::UndoLastArchive {
            expected_count: projection.last_archive_count,
        },
    },
    None,
);
```

- [x] **Step 4: Run the focused TUI regression set**

Run:

```bash
nix develop path:. --command cargo test -p coding-brain-tui mark_seen -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui review_skip_persists_then_advances_and_failure_preserves_selection -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui refresh_preserves -- --nocapture
nix develop path:. --command cargo test -p coding-brain-tui refresh_restores -- --nocapture
```

Expected: all selected tests pass; every surface advances on success, all guarded paths preserve selection, and the pre-existing Review `s` and periodic identity-restoration tests remain green.

- [ ] **Step 5: Run workspace quality gates**

Run:

```bash
nix develop path:. --command cargo fmt
nix develop path:. --command cargo fmt --check
nix develop path:. --command cargo test
nix develop path:. --command cargo clippy -- -D warnings
nix develop path:. --command cargo build
git -c core.whitespace=trailing-space,space-before-tab diff --check
```

Expected: every command exits `0`; no warnings, formatting changes, test failures, build failures, or whitespace errors remain.

- [ ] **Step 6: Audit scope and commit only with explicit authorization**

Run:

```bash
git status --short
git diff -- crates/coding-brain-tui/src/brain_app.rs
git diff -- .internal/specs/2026-08-12-surface-aware-seen-advancement-design.md
git diff -- .internal/plans/2026-08-12-surface-aware-seen-advancement.md
```

Expected: every changed line traces to `codexctl-4hpxz`; no unrelated path is staged or modified. After explicit user authorization, stage only the approved paths and use an emoji conventional commit containing the bead ID, for example:

```bash
git add crates/coding-brain-tui/src/brain_app.rs \
  .internal/specs/2026-08-12-surface-aware-seen-advancement-design.md \
  .internal/plans/2026-08-12-surface-aware-seen-advancement.md
git commit -m "🐛 fix: advance seen selection by surface (codexctl-4hpxz)"
```

## Stress Test Results: Surface-Aware Seen Advancement Plan

### Resolved Decisions

- Task boundary: keep one atomic TDD task because code and regressions exercise one inseparable behavior in one file.
- Runtime trust: capture the request surface, require the mutation result to match it, and never let result data redirect selection.
- Helper boundary: keep the return-type change private to `BrainApp` and audit all three callers.
- Edge cases: add older coherent refresh and cross-surface identity-collision regressions.
- Complexity: retain bounded linear scans and inline action logic without caches or new helper types.
- Viewport invariant: re-clamp and explicitly reset the active viewport after advancement because repeated review display identities can hide a row-index change.
- API shape: remove the dead `advance_after_success` boolean once both `a` and Review `s` always advance.
- Security and authority: preserve selected keys and expected revision; fail closed on result-surface mismatch.
- Verification: extend focused refresh coverage and run formatter mutation before the format check.

### Changes Made

- Added a fail-closed result-surface equality check and captured-surface lookup.
- Added older-revision, cross-surface collision, and evidence-scroll tests.
- Added post-advancement clamping plus an explicit active-viewport reset for repeated display identities.
- Removed the unnecessary advancement policy boolean from the planned interface.

### Deferred / Parking Lot

- None.

### Confidence Assessment

- Overall: High.
- Areas of concern: the cross-surface mismatch fixture needs a small local `BrainActions` stub, but must not change production or core mock APIs.
