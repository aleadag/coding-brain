#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use coding_brain::brain::activity::ActivityStore;
use coding_brain::runtime::{LiveBrainActions, LiveBrainSource};
use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SnapshotLimits,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::review_state::{
    ReviewDisposition, ReviewMutation, ReviewMutationRequest, ReviewRequestError, ReviewSurface,
};
use coding_brain_core::runtime::{BrainActions, BrainSource, ReviewMutationError};
use serde_json::{Value, json};

const STEP_ENV: &str = "CODING_BRAIN_REVIEW_TEST_STEP";
const SURFACE_ENV: &str = "CODING_BRAIN_REVIEW_TEST_SURFACE";
const RESULT_ENV: &str = "CODING_BRAIN_REVIEW_TEST_RESULT";

fn state_root(root: &Path) -> PathBuf {
    root.join("state/coding-brain")
}

fn child(root: &Path, step: &str, surface: Option<ReviewSurface>) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "review_state_child", "--ignored", "--nocapture"])
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env(STEP_ENV, step)
        .env(RESULT_ENV, root.join("result.json"));
    if let Some(surface) = surface {
        command.env(SURFACE_ENV, surface.as_str());
    }
    command
}

fn run_child(root: &Path, step: &str, surface: Option<ReviewSurface>) {
    let output = child(root, step, surface).output().unwrap();
    assert!(
        output.status.success(),
        "child step {step} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn observe(root: &Path) -> Value {
    run_child(root, "observe", None);
    serde_json::from_slice(&fs::read(root.join("result.json")).unwrap()).unwrap()
}

fn surface<'a>(observation: &'a Value, name: &str) -> &'a Value {
    &observation["surfaces"][name]
}

fn count(observation: &Value, name: &str, field: &str) -> u64 {
    surface(observation, name)[field].as_u64().unwrap()
}

fn fixture_event(id: &str, kind: ActivityKind, state: ActivityState, at: u64) -> ActivityEvent {
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind,
        activity_id: id.into(),
        recorded_at_ms: at,
        project: ProjectEvidence {
            project_id: ProjectId::Stable("review-matrix".into()),
            cwd: PathBuf::from("/work/review-matrix"),
            label: Some("review-matrix".into()),
        },
        session: None,
        state,
        tool: (kind == ActivityKind::Decision).then(|| "Bash".into()),
        normalized_command: (kind == ActivityKind::Decision)
            .then(|| "rm -rf /tmp/review-matrix".into()),
        fingerprint: (kind == ActivityKind::Decision).then(|| "shared-fingerprint".into()),
        rule_id: None,
        confidence: (kind == ActivityKind::Decision).then_some(0.95),
        threshold: None,
        reasoning: Some("restart matrix fixture".into()),
        decision_id: (kind == ActivityKind::Decision).then(|| "review-decision".into()),
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

fn create_fixture(root: &Path) {
    fs::create_dir_all(root.join("home")).unwrap();
    fs::create_dir_all(root.join("config")).unwrap();
    fs::create_dir_all(state_root(root).join("brain")).unwrap();
    fs::set_permissions(state_root(root), fs::Permissions::from_mode(0o700)).unwrap();

    let activity = ActivityStore::at(state_root(root).join("activity.jsonl"));
    activity
        .append(fixture_event(
            "shared-activity-1",
            ActivityKind::Decision,
            ActivityState::Denied,
            1,
        ))
        .unwrap();
    activity
        .append(fixture_event(
            "diagnostic-1",
            ActivityKind::Diagnostic,
            ActivityState::Error,
            2,
        ))
        .unwrap();
    activity
        .append(fixture_event(
            "recent-1",
            ActivityKind::Decision,
            ActivityState::Allowed,
            3,
        ))
        .unwrap();
    activity
        .append(fixture_event(
            "recent-1",
            ActivityKind::Decision,
            ActivityState::Delivered,
            4,
        ))
        .unwrap();

    let decision = json!({
        "provider": "codex",
        "ts": "1",
        "pid": 1,
        "project": "review-matrix",
        "tool": "Bash",
        "command": "rm -rf /tmp/review-matrix",
        "brain_action": "approve",
        "brain_confidence": 0.95,
        "brain_reasoning": "restart matrix fixture",
        "user_action": "deny_rule_override",
        "decision_type": "session",
        "decision_id": "review-decision"
    });
    fs::write(
        state_root(root).join("brain/decisions.jsonl"),
        format!("{decision}\n"),
    )
    .unwrap();
}

fn assert_surface_mutation_isolated(before: &Value, after: &Value, changed: &str) {
    for name in ["attention", "review", "diagnostics", "recent"] {
        if name == changed {
            assert_eq!(
                count(after, name, "revision"),
                count(before, name, "revision").checked_add(1).unwrap(),
                "revision for mutated {name} surface"
            );
        } else {
            assert_eq!(
                surface(after, name),
                surface(before, name),
                "non-target {name} surface changed"
            );
        }
    }
}

#[test]
fn surface_mutation_isolation_rejects_non_target_count_drift() {
    let summary = || {
        json!({
            "revision": 0,
            "new": 1,
            "reviewed": 0,
            "last_archive": 0,
            "rows": 1,
        })
    };
    let before = json!({
        "surfaces": {
            "attention": summary(),
            "review": summary(),
            "diagnostics": summary(),
            "recent": summary(),
        }
    });
    let mut after = before.clone();
    after["surfaces"]["attention"]["revision"] = 1.into();
    after["surfaces"]["recent"]["new"] = 2.into();

    let result = std::panic::catch_unwind(|| {
        assert_surface_mutation_isolated(&before, &after, "attention");
    });
    assert!(result.is_err(), "non-target count drift was not rejected");
}

#[test]
fn review_lifecycle_process_matrix_spans_all_four_surfaces() {
    let root = tempfile::tempdir().unwrap();
    create_fixture(root.path());

    let first_run = observe(root.path());
    for name in ["attention", "review", "diagnostics", "recent"] {
        assert_eq!(count(&first_run, name, "revision"), 0);
        assert!(count(&first_run, name, "new") > 0, "{name} starts NEW");
        assert_eq!(count(&first_run, name, "reviewed"), 0);
    }
    assert!(first_run["scorecard_total"].as_u64().unwrap() > 0);
    let original_scorecard = first_run["scorecard"].clone();

    let mut previous = first_run;
    for (name, review_surface) in [
        ("attention", ReviewSurface::Attention),
        ("review", ReviewSurface::Review),
        ("diagnostics", ReviewSurface::Diagnostics),
        ("recent", ReviewSurface::Recent),
    ] {
        run_child(root.path(), "review", Some(review_surface));
        let reviewed = observe(root.path());
        assert_surface_mutation_isolated(&previous, &reviewed, name);
        assert!(count(&reviewed, name, "reviewed") > 0);
        assert_eq!(reviewed["scorecard"], original_scorecard);
        previous = reviewed;
    }

    run_child(root.path(), "reject_recent_archive", None);
    let after_rejected_archive = observe(root.path());
    assert_eq!(after_rejected_archive, previous);

    for (name, review_surface) in [
        ("attention", ReviewSurface::Attention),
        ("review", ReviewSurface::Review),
        ("diagnostics", ReviewSurface::Diagnostics),
    ] {
        run_child(root.path(), "archive", Some(review_surface));
        let archived = observe(root.path());
        assert_surface_mutation_isolated(&previous, &archived, name);
        assert_eq!(count(&archived, name, "reviewed"), 0);
        assert!(count(&archived, name, "last_archive") > 0);
        assert_eq!(archived["scorecard"], original_scorecard);

        run_child(root.path(), "undo", Some(review_surface));
        let restored = observe(root.path());
        assert_surface_mutation_isolated(&archived, &restored, name);
        assert!(count(&restored, name, "reviewed") > 0);
        assert_eq!(count(&restored, name, "last_archive"), 0);
        assert_eq!(restored["scorecard"], original_scorecard);
        previous = restored;
    }

    let activity = ActivityStore::at(state_root(root.path()).join("activity.jsonl"));
    activity
        .append(fixture_event(
            "shared-activity-2",
            ActivityKind::Decision,
            ActivityState::Denied,
            5,
        ))
        .unwrap();
    let reopened = observe(root.path());
    assert_eq!(count(&reopened, "attention", "new"), 1);
    assert_eq!(count(&reopened, "attention", "reviewed"), 1);
    assert_eq!(surface(&reopened, "attention")["rows"], 1);
    assert_eq!(reopened["scorecard"], original_scorecard);

    fs::remove_file(state_root(root.path()).join("review-state.json")).unwrap();
    let reset = observe(root.path());
    for name in ["attention", "review", "diagnostics", "recent"] {
        assert_eq!(count(&reset, name, "revision"), 0);
        assert_eq!(count(&reset, name, "reviewed"), 0);
        assert!(count(&reset, name, "new") > 0, "{name} resets to NEW");
    }
    assert_eq!(reset["scorecard"], original_scorecard);

    fs::write(
        state_root(root.path()).join("review-state.json"),
        b"{\"schema_version\":1,\"surfaces\":not-json}",
    )
    .unwrap();
    fs::set_permissions(
        state_root(root.path()).join("review-state.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    run_child(root.path(), "observe_invalid", None);
}

fn parse_surface() -> ReviewSurface {
    match std::env::var(SURFACE_ENV).unwrap().as_str() {
        "attention" => ReviewSurface::Attention,
        "review" => ReviewSurface::Review,
        "diagnostics" => ReviewSurface::Diagnostics,
        "recent" => ReviewSurface::Recent,
        other => panic!("unknown review surface {other}"),
    }
}

fn projection(
    refresh: &coding_brain_core::runtime::BrainRefresh,
    surface: ReviewSurface,
) -> &coding_brain_core::review_state::SurfaceReviewProjection {
    match surface {
        ReviewSurface::Attention => &refresh.review_state.attention,
        ReviewSurface::Review => &refresh.review_state.review,
        ReviewSurface::Diagnostics => &refresh.review_state.diagnostics,
        ReviewSurface::Recent => &refresh.review_state.recent,
    }
}

fn observation(refresh: &coding_brain_core::runtime::BrainRefresh) -> Value {
    fn summary(projection: &coding_brain_core::review_state::SurfaceReviewProjection) -> Value {
        json!({
            "revision": projection.revision,
            "new": projection.new_count,
            "reviewed": projection.reviewed_count,
            "last_archive": projection.last_archive_count,
            "rows": projection.items.len(),
        })
    }

    json!({
        "surfaces": {
            "attention": summary(&refresh.review_state.attention),
            "review": summary(&refresh.review_state.review),
            "diagnostics": summary(&refresh.review_state.diagnostics),
            "recent": summary(&refresh.review_state.recent),
        },
        "scorecard_total": refresh.scorecard.total_decisions,
        "scorecard": format!("{:?}", refresh.scorecard),
    })
}

fn run_review_test_step(step: &str) -> Result<(), String> {
    let source = LiveBrainSource::default();
    if step == "observe_invalid" {
        return match source.refresh(SnapshotLimits::default()) {
            Err(_) => Ok(()),
            Ok(_) => Err("invalid review state unexpectedly refreshed".into()),
        };
    }

    let refresh = source
        .refresh(SnapshotLimits::default())
        .map_err(|error| error.to_string())?;
    if step == "observe" {
        let path = PathBuf::from(std::env::var_os(RESULT_ENV).unwrap());
        fs::write(path, serde_json::to_vec(&observation(&refresh)).unwrap())
            .map_err(|error| error.to_string())?;
        return Ok(());
    }

    let actions = LiveBrainActions::default();
    if step == "reject_recent_archive" {
        let recent = projection(&refresh, ReviewSurface::Recent);
        let result = actions.mutate_review_state(ReviewMutationRequest {
            surface: ReviewSurface::Recent,
            expected_surface_revision: recent.revision,
            operation: ReviewMutation::ArchiveAllReviewed {
                expected_count: recent.reviewed_count,
            },
        });
        return match result {
            Err(ReviewMutationError::InvalidRequest(ReviewRequestError::UnsupportedOperation)) => {
                Ok(())
            }
            other => Err(format!("Recent archive returned {other:?}")),
        };
    }

    let surface = parse_surface();
    let projected = projection(&refresh, surface);
    let operation = match step {
        "review" => {
            let target = projected
                .items
                .iter()
                .find(|target| !target.new_member_keys.is_empty())
                .ok_or_else(|| format!("no NEW {surface:?} target"))?;
            ReviewMutation::SetDisposition {
                keys: target.new_member_keys.iter().copied().collect(),
                disposition: ReviewDisposition::Reviewed,
            }
        }
        "archive" => {
            let target = projected
                .items
                .iter()
                .find(|target| !target.reviewed_member_keys.is_empty())
                .ok_or_else(|| format!("no reviewed {surface:?} target"))?;
            ReviewMutation::SetDisposition {
                keys: target.reviewed_member_keys.iter().copied().collect(),
                disposition: ReviewDisposition::Archived,
            }
        }
        "undo" => ReviewMutation::UndoLastArchive {
            expected_count: projected.last_archive_count,
        },
        other => return Err(format!("unknown step {other}")),
    };
    actions
        .mutate_review_state(ReviewMutationRequest {
            surface,
            expected_surface_revision: projected.revision,
            operation,
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[test]
#[ignore = "subprocess helper"]
fn review_state_child() {
    let step = std::env::var(STEP_ENV).unwrap();
    run_review_test_step(&step).unwrap();
}
