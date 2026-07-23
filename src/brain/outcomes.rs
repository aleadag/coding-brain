#![allow(dead_code)]

//! Outcome capture for brain decisions (#220 baselining v1).
//!
//! A `PostToolUse` hook writes a "pending outcome" file each
//! time a tool finishes. The reaper periodically attributes each pending
//! outcome to the most recent matching decision in `decisions.jsonl` and
//! writes the resolved outcome to `outcomes/<decision_id>.json`. Distillation
//! reads decisions and outcomes together to build per-approach success
//! statistics for local brain baseline reporting.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityOutcome, ActivityState,
    ProjectEvidence,
};
use coding_brain_core::paths::{CodingBrainPaths, PathEnvironment};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;

use super::activity::ActivityStore;
use super::decisions::{DecisionRecord, decisions_dir, read_all_decisions};

// ────────────────────────────────────────────────────────────────────────────
// Constants
// ────────────────────────────────────────────────────────────────────────────

/// How recent a decision must be (seconds) to be a candidate for outcome
/// attribution. Keeps fuzzy matching from binding outcomes to ancient
/// decisions when many sessions reuse the same command.
const ATTRIBUTION_WINDOW_SECS: u64 = 600;

/// How long an unattributed pending outcome lives before being marked orphaned.
const ORPHAN_AFTER_SECS: u64 = 86_400;

/// Cap on stderr_tail bytes stored — protects against runaway log capture.
pub const MAX_STDERR_TAIL_BYTES: usize = 2_048;

/// Lookback window (seconds) for attributing a test-runner failure to recent
/// brain-approved edits (#238). Shorter than `ATTRIBUTION_WINDOW_SECS` because
/// the signal degrades quickly — edits 10 minutes before a failing test run
/// are less likely to be the cause.
const TEST_FAILURE_FANOUT_WINDOW_SECS: u64 = 300;

/// Cap on how many recent edits a single test failure attributes to. Prevents
/// long stretches of refactor edits from all sharing one failure.
const TEST_FAILURE_FANOUT_MAX_EDITS: usize = 5;

/// Edit-like tools whose decisions get tagged when a subsequent test run fails.
const EDIT_LIKE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

// ────────────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────────────

/// What the PostToolUse hook saw, written before any decision attribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingOutcome {
    /// Provider that emitted the hook outcome. Missing legacy values are Codex.
    #[serde(default)]
    pub provider: AgentProvider,
    /// Tool name (e.g., "Bash", "Edit").
    pub tool: String,
    /// Command or input summary captured by the hook.
    #[serde(default)]
    pub command: Option<String>,
    /// Project slug (basename of cwd at hook time).
    pub project: String,
    /// Provider-native session id, if the hook payload carried one.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Provider-native tool-use id, if available — used for stricter joining later.
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// Tool exit code (0 = success). None when the hook can't infer one.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Wall-clock duration of the tool call in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Last MAX_STDERR_TAIL_BYTES of stderr or tool error output.
    #[serde(default)]
    pub stderr_tail: Option<String>,
    /// Epoch seconds when the outcome was captured.
    pub ts: u64,
}

/// Resolved outcome: a `PendingOutcome` attributed to a specific decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedOutcome {
    pub decision_id: String,
    pub tool: String,
    #[serde(default)]
    pub command: Option<String>,
    pub project: String,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub stderr_tail: Option<String>,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApproachBaselineRow {
    pub approach_ref: String,
    pub success_rate: f64,
    pub sample_count: u32,
    pub median_cost_usd: Option<f64>,
    pub median_duration_ms: Option<u64>,
}

#[derive(Default)]
struct OutcomeBucket {
    samples: u32,
    successes: u32,
    costs: Vec<f64>,
    durations_ms: Vec<u64>,
}

fn approach_ref_for(decision: &DecisionRecord) -> Option<String> {
    let tool = decision.tool.as_deref()?;
    let command = decision
        .command
        .as_deref()
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "*".into());
    Some(format!("pattern:{tool}:{command}"))
}

fn median_f64(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn median_u64(mut values: Vec<u64>) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2
    } else {
        values[middle]
    })
}

pub fn rank_approaches(
    decisions: &[DecisionRecord],
    resolved: &std::collections::HashMap<String, ResolvedOutcome>,
    project: Option<&str>,
) -> Vec<ApproachBaselineRow> {
    let mut buckets = std::collections::HashMap::<String, OutcomeBucket>::new();
    for decision in decisions {
        if project.is_some_and(|name| !decision.project.eq_ignore_ascii_case(name)) {
            continue;
        }
        let Some(decision_id) = decision.decision_id.as_deref() else {
            continue;
        };
        let Some(outcome) = resolved.get(decision_id) else {
            continue;
        };
        let Some(approach_ref) = approach_ref_for(decision) else {
            continue;
        };
        let bucket = buckets.entry(approach_ref).or_default();
        bucket.samples += 1;
        if outcome.exit_code == Some(0) {
            bucket.successes += 1;
        }
        if let Some(cost) = decision
            .context
            .as_ref()
            .map(|context| context.cost_usd)
            .filter(|cost| *cost > 0.0)
        {
            bucket.costs.push(cost);
        }
        if let Some(duration_ms) = outcome.duration_ms {
            bucket.durations_ms.push(duration_ms);
        }
    }

    let mut rows = buckets
        .into_iter()
        .map(|(approach_ref, bucket)| ApproachBaselineRow {
            approach_ref,
            success_rate: bucket.successes as f64 / bucket.samples as f64,
            sample_count: bucket.samples,
            median_cost_usd: median_f64(bucket.costs),
            median_duration_ms: median_u64(bucket.durations_ms),
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let a_score = a.success_rate * f64::from(a.sample_count);
        let b_score = b.success_rate * f64::from(b.sample_count);
        b_score
            .partial_cmp(&a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.approach_ref.cmp(&b.approach_ref))
    });
    rows
}

/// Stats returned by `reap()`.
#[derive(Debug, Default, Clone)]
pub struct ReapStats {
    pub scanned: u32,
    pub attributed: u32,
    pub orphaned: u32,
    pub still_pending: u32,
    pub errors: u32,
    /// Edit decisions newly tagged as `TestFailed` from a fan-out attribution.
    pub test_failures_attributed: u32,
}

/// Marker file written into `test-failures/<decision_id>.json` when a
/// test-runner command failed shortly after a brain-approved edit (#238).
/// Backfilled into `DecisionOutcome::TestFailed` during distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestFailureMarker {
    pub decision_id: String,
    /// The exact failing command (e.g. "cargo test", "npm test --watch=false").
    pub failed_test_command: String,
    /// Epoch seconds when the failure was observed.
    pub outcome_ts: u64,
}

// ────────────────────────────────────────────────────────────────────────────
// Path helpers
// ────────────────────────────────────────────────────────────────────────────

/// Directory where pending PostToolUse outcomes accumulate.
pub fn pending_dir() -> PathBuf {
    decisions_dir().join("pending-outcomes")
}

/// Directory where attributed outcomes live, keyed by `<decision_id>.json`.
pub fn outcomes_dir() -> PathBuf {
    decisions_dir().join("outcomes")
}

/// Directory where pending files that failed attribution after `ORPHAN_AFTER_SECS`
/// are quarantined for inspection.
pub fn orphaned_dir() -> PathBuf {
    decisions_dir().join("outcomes-orphaned")
}

/// Directory where test-failure attribution markers live, keyed by `<decision_id>.json`.
pub fn test_failures_dir() -> PathBuf {
    decisions_dir().join("test-failures")
}

fn ensure_dir(path: &PathBuf) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

// ────────────────────────────────────────────────────────────────────────────
// ID generation
// ────────────────────────────────────────────────────────────────────────────

static OUTCOME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Generate a unique pending outcome filename stem (no extension).
fn gen_pending_id() -> String {
    let epoch = epoch_secs();
    let seq = OUTCOME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("po_{epoch}_{pid}_{seq}")
}

// ────────────────────────────────────────────────────────────────────────────
// Write / read
// ────────────────────────────────────────────────────────────────────────────

/// Truncate stderr to MAX_STDERR_TAIL_BYTES from the tail.
pub fn truncate_stderr(s: &str) -> String {
    if s.len() <= MAX_STDERR_TAIL_BYTES {
        return s.to_string();
    }
    // Take the trailing slice on a char boundary.
    let start = s.len() - MAX_STDERR_TAIL_BYTES;
    let safe_start = (start..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    s[safe_start..].to_string()
}

/// Persist a pending outcome to `pending-outcomes/<id>.json`.
pub fn write_pending(out: &PendingOutcome) -> std::io::Result<PathBuf> {
    let dir = pending_dir();
    ensure_dir(&dir)?;
    let path = dir.join(format!("{}.json", gen_pending_id()));
    let json = serde_json::to_string(out).map_err(std::io::Error::other)?;
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)?;
    file.write_all(json.as_bytes())?;
    Ok(path)
}

/// Read all pending outcomes (path + parsed body).
pub fn list_pending() -> Vec<(PathBuf, PendingOutcome)> {
    let dir = pending_dir();
    let mut out = Vec::new();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(p) = serde_json::from_str::<PendingOutcome>(&content) {
            out.push((path, p));
        }
    }
    out
}

/// Load all attributed outcomes keyed by `decision_id`.
pub fn load_resolved_map() -> std::collections::HashMap<String, ResolvedOutcome> {
    let mut map = std::collections::HashMap::new();
    let dir = outcomes_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(r) = serde_json::from_str::<ResolvedOutcome>(&content) {
            map.insert(r.decision_id.clone(), r);
        }
    }
    map
}

// ────────────────────────────────────────────────────────────────────────────
// Test failure attribution (#238)
// ────────────────────────────────────────────────────────────────────────────

/// Check whether `cmd` is invocation of one of the configured test runners.
/// Match is a normalized, case-insensitive prefix comparison on whitespace-
/// collapsed forms — so `"  CARGO   test --release "` matches `"cargo test"`.
pub fn is_test_runner_cmd(cmd: &str, runners: &[String]) -> bool {
    let cmd_norm = normalize_command(cmd).to_lowercase();
    if cmd_norm.is_empty() {
        return false;
    }
    runners.iter().any(|r| {
        let r_norm = normalize_command(r).to_lowercase();
        if r_norm.is_empty() {
            return false;
        }
        // Prefix match on token boundary (either equal or followed by space/end).
        if cmd_norm == r_norm {
            return true;
        }
        if let Some(rest) = cmd_norm.strip_prefix(&r_norm) {
            return rest.starts_with(' ');
        }
        false
    })
}

/// Load existing test-failure markers, keyed by decision_id.
pub fn load_test_failures() -> std::collections::HashMap<String, TestFailureMarker> {
    let mut map = std::collections::HashMap::new();
    let dir = test_failures_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return map,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(m) = serde_json::from_str::<TestFailureMarker>(&content) {
            map.insert(m.decision_id.clone(), m);
        }
    }
    map
}

/// Fan a failed test-runner pending outcome out to the most recent brain-
/// approved edits in the same project, writing one marker per edit decision.
///
/// Returns the number of new markers written. Idempotent: existing markers
/// for a decision_id are never overwritten.
fn fanout_test_failures(
    pending: &[(PathBuf, PendingOutcome)],
    decisions: &[DecisionRecord],
    runners: &[String],
) -> u32 {
    if runners.is_empty() {
        return 0;
    }
    let existing = load_test_failures();
    let dir = test_failures_dir();
    let _ = ensure_dir(&dir);
    let mut written = 0u32;

    for (_, p) in pending {
        if p.exit_code.unwrap_or(0) == 0 {
            continue;
        }
        let Some(cmd) = &p.command else {
            continue;
        };
        if !is_test_runner_cmd(cmd, runners) {
            continue;
        }

        // Collect candidate edit decisions: same project, positive outcome,
        // edit-like tool, timestamp inside the fan-out window before this run.
        let mut candidates: Vec<&DecisionRecord> = decisions
            .iter()
            .filter(|d| {
                if d.provider != p.provider {
                    return false;
                }
                let Some(_did) = d.decision_id.as_deref() else {
                    return false;
                };
                if !d.project.eq_ignore_ascii_case(&p.project) {
                    return false;
                }
                let tool = d.tool.as_deref().unwrap_or("");
                if !EDIT_LIKE_TOOLS.contains(&tool) {
                    return false;
                }
                if !d.is_positive() {
                    return false;
                }
                let Some(d_ts) = parse_ts(&d.timestamp) else {
                    return false;
                };
                d_ts <= p.ts && p.ts.saturating_sub(d_ts) <= TEST_FAILURE_FANOUT_WINDOW_SECS
            })
            .collect();

        // Most recent first so we attribute the last N edits.
        candidates.sort_by(|a, b| {
            parse_ts(&b.timestamp)
                .unwrap_or(0)
                .cmp(&parse_ts(&a.timestamp).unwrap_or(0))
        });

        for d in candidates.iter().take(TEST_FAILURE_FANOUT_MAX_EDITS) {
            let did = match d.decision_id.as_deref() {
                Some(s) => s.to_string(),
                None => continue,
            };
            if existing.contains_key(&did) {
                continue;
            }
            let marker = TestFailureMarker {
                decision_id: did.clone(),
                failed_test_command: cmd.clone(),
                outcome_ts: p.ts,
            };
            let dest = dir.join(format!("{did}.json"));
            let Ok(json) = serde_json::to_string(&marker) else {
                continue;
            };
            // create_new makes this idempotent: if a marker already exists on
            // disk (from a previous reap pass), we skip silently.
            match OpenOptions::new().create_new(true).write(true).open(&dest) {
                Ok(mut file) => {
                    if file.write_all(json.as_bytes()).is_ok() {
                        written += 1;
                    }
                }
                Err(_) => continue,
            }
        }
    }
    written
}

// ────────────────────────────────────────────────────────────────────────────
// Reaper
// ────────────────────────────────────────────────────────────────────────────

/// Normalise a command string for fuzzy matching against decision records.
/// Strips leading/trailing whitespace and collapses internal runs.
fn normalize_command(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a decision timestamp (currently stored as `"<epoch_secs>"`).
fn parse_ts(s: &str) -> Option<u64> {
    s.trim_matches('"').parse::<u64>().ok()
}

/// Walk pending outcomes, attribute each to a matching decision, and write
/// resolved outcomes. Pending files older than `ORPHAN_AFTER_SECS` are moved
/// to `orphaned_dir()` for inspection.
///
/// Attribution rule: the most recent decision in `decisions.jsonl` such that
///   - same tool
///   - normalized command equals the outcome's normalized command (when both present)
///   - same project (case-insensitive)
///   - decision timestamp <= outcome timestamp, within ATTRIBUTION_WINDOW_SECS
///   - decision has a `decision_id`
///   - no resolved outcome exists yet for that `decision_id`
pub fn reap() -> ReapStats {
    let runners = crate::config::Config::load()
        .brain
        .map(|b| b.test_runners)
        .unwrap_or_else(crate::config::default_test_runners);
    reap_with_runners(&runners)
}

/// `reap()` with explicit test-runner patterns. Exposed for tests so they
/// don't depend on a Config layered TOML load.
pub fn reap_with_runners(_test_runners: &[String]) -> ReapStats {
    let mut stats = ReapStats::default();
    let pending = list_pending();
    if pending.is_empty() {
        return stats;
    }

    let _ = ensure_dir(&outcomes_dir());
    let _ = ensure_dir(&orphaned_dir());

    let decisions = read_all_decisions();
    let resolved = load_resolved_map();
    let now = epoch_secs();
    let activity = current_activity_store();

    // Track decisions claimed within this reap pass so a single decision
    // doesn't get attributed to two pending outcomes when we run before
    // the resolved map is reloaded.
    let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, p) in pending {
        stats.scanned += 1;

        let strict_decision_id = match activity.as_ref() {
            Some(activity) => match append_activity_outcome(activity, &p) {
                Ok(Some(decision_id)) => decision_id,
                Ok(None) => {
                    if now.saturating_sub(p.ts) > ORPHAN_AFTER_SECS {
                        let dest = orphaned_dir().join(
                            path.file_name()
                                .map(|name| name.to_owned())
                                .unwrap_or_else(|| std::ffi::OsString::from("orphan.json")),
                        );
                        if fs::rename(&path, &dest).is_ok() {
                            stats.orphaned += 1;
                        } else {
                            stats.errors += 1;
                        }
                    } else {
                        stats.still_pending += 1;
                    }
                    continue;
                }
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            },
            None => {
                stats.errors += 1;
                continue;
            }
        };

        if resolved.contains_key(&strict_decision_id) {
            if fs::remove_file(&path).is_ok() {
                stats.attributed += 1;
            } else {
                stats.errors += 1;
            }
            continue;
        }

        let best = decisions.iter().position(|decision| {
            decision.provider == p.provider
                && decision.decision_id.as_deref() == Some(strict_decision_id.as_str())
                && !claimed.contains(&strict_decision_id)
        });

        if let Some(idx) = best {
            let d = &decisions[idx];
            let decision_id = d.decision_id.clone().unwrap();
            let resolved = ResolvedOutcome {
                decision_id: decision_id.clone(),
                tool: p.tool.clone(),
                command: p.command.clone(),
                project: p.project.clone(),
                exit_code: p.exit_code,
                duration_ms: p.duration_ms,
                stderr_tail: p.stderr_tail.clone(),
                ts: p.ts,
            };
            let dest = outcomes_dir().join(format!("{decision_id}.json"));
            match fs::write(&dest, serde_json::to_string(&resolved).unwrap_or_default()) {
                Ok(_) => {
                    claimed.insert(decision_id.clone());
                    let _ = fs::remove_file(&path);
                    stats.attributed += 1;
                }
                Err(_) => stats.errors += 1,
            }
        } else if now.saturating_sub(p.ts) > ORPHAN_AFTER_SECS {
            // Move to orphaned for inspection.
            let dest = orphaned_dir().join(
                path.file_name()
                    .map(|n| n.to_owned())
                    .unwrap_or_else(|| std::ffi::OsString::from("orphan.json")),
            );
            if fs::rename(&path, &dest).is_ok() {
                stats.orphaned += 1;
            } else {
                stats.errors += 1;
            }
        } else {
            stats.still_pending += 1;
        }
    }

    stats
}

fn current_activity_store() -> Option<ActivityStore> {
    let environment = PathEnvironment::new(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    );
    let paths = CodingBrainPaths::resolve(&environment).ok()?;
    let path = paths.state_root().join("activity.jsonl");
    Some(ActivityStore::at(path))
}

fn append_activity_outcome(
    activity: &ActivityStore,
    pending: &PendingOutcome,
) -> Result<Option<String>, String> {
    let log = activity.read().map_err(|error| error.to_string())?;
    let matched = pending.session_id.as_deref().and_then(|session_id| {
        let tool_use_id = pending.tool_use_id.as_deref()?;
        log.events().iter().rev().find(|event| {
            event.state.is_terminal()
                && event.decision_id.is_some()
                && event.session.as_ref().is_some_and(|session| {
                    session.provider == pending.provider
                        && session.session_id == session_id
                        && session.tool_use_id.as_deref() == Some(tool_use_id)
                })
        })
    });
    let Some(matched) = matched else {
        append_orphan_activity(
            activity,
            pending,
            "orphan outcome: stable hook IDs did not match",
        )?;
        return Ok(None);
    };
    let decision_id = matched
        .decision_id
        .clone()
        .ok_or_else(|| "matched activity has no decision ID".to_string())?;
    if log.events().iter().any(|event| {
        event.activity_id == matched.activity_id
            && event.state == ActivityState::Outcome
            && event.decision_id.as_deref() == Some(&decision_id)
    }) {
        return Ok(Some(decision_id));
    }
    activity
        .append(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: matched.kind,
            activity_id: matched.activity_id.clone(),
            recorded_at_ms: pending.ts.saturating_mul(1_000),
            project: matched.project.clone(),
            session: matched.session.clone(),
            state: ActivityState::Outcome,
            tool: matched.tool.clone(),
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: None,
            decision_id: Some(decision_id.clone()),
            outcome: Some(if pending.exit_code == Some(0) {
                ActivityOutcome::Succeeded
            } else if pending.exit_code.is_some() || pending.stderr_tail.is_some() {
                ActivityOutcome::Failed
            } else {
                ActivityOutcome::Succeeded
            }),
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|error| error.to_string())?;
    Ok(Some(decision_id))
}

fn append_orphan_activity(
    activity: &ActivityStore,
    pending: &PendingOutcome,
    diagnostic: &str,
) -> Result<(), String> {
    let activity_id = format!(
        "orphan_outcome_{}_{}_{}_{}",
        pending.ts,
        pending.provider.as_str(),
        pending.session_id.as_deref().unwrap_or("missing-session"),
        pending.tool_use_id.as_deref().unwrap_or("missing-tool-use")
    );
    if activity
        .read()
        .map_err(|error| error.to_string())?
        .events()
        .iter()
        .any(|event| event.activity_id == activity_id)
    {
        return Ok(());
    }
    activity
        .append(ActivityEvent {
            schema_version: ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Diagnostic,
            activity_id,
            recorded_at_ms: pending.ts.saturating_mul(1_000),
            project: ProjectEvidence {
                project_id: ProjectId::Temporary("orphan-outcome".into()),
                cwd: std::env::current_dir().unwrap_or_else(|_| Path::new("/").to_path_buf()),
                label: None,
            },
            session: None,
            state: ActivityState::Error,
            tool: Some(pending.tool.clone()),
            normalized_command: None,
            fingerprint: None,
            rule_id: None,
            confidence: None,
            threshold: None,
            reasoning: Some(diagnostic.into()),
            decision_id: None,
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        })
        .map_err(|error| error.to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn decision_with_id(id: &str, project: &str, tool: &str, command: &str) -> DecisionRecord {
        DecisionRecord {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            timestamp: "2026-07-14T00:00:00Z".into(),
            pid: 1,
            project: project.into(),
            tool: Some(tool.into()),
            command: Some(command.into()),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "fixture".into(),
            user_action: "auto".into(),
            context: None,
            outcome: None,
            decision_type: crate::brain::decisions::DecisionType::Session,
            suggested_at: None,
            resolved_at: None,
            override_reason: None,
            decision_id: Some(id.into()),
            brain_decision_ms: None,
            cache_hit: None,
            canonical: None,
        }
    }

    fn resolved_outcomes(
        rows: &[(&str, &str, i32, u64)],
    ) -> std::collections::HashMap<String, ResolvedOutcome> {
        rows.iter()
            .map(|(id, project, exit_code, duration_ms)| {
                (
                    (*id).to_string(),
                    ResolvedOutcome {
                        decision_id: (*id).to_string(),
                        tool: "Bash".into(),
                        command: Some("cargo test".into()),
                        project: (*project).to_string(),
                        exit_code: Some(*exit_code),
                        duration_ms: Some(*duration_ms),
                        stderr_tail: None,
                        ts: 1,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn rank_approaches_is_local_and_project_filterable() {
        let decisions = vec![
            decision_with_id("d1", "alpha", "Bash", "cargo test"),
            decision_with_id("d2", "alpha", "Bash", "cargo test"),
            decision_with_id("d3", "beta", "Bash", "cargo test"),
        ];
        let resolved = resolved_outcomes(&[
            ("d1", "alpha", 0, 100),
            ("d2", "alpha", 1, 300),
            ("d3", "beta", 0, 200),
        ]);

        let rows = rank_approaches(&decisions, &resolved, Some("alpha"));

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].approach_ref, "pattern:Bash:cargo test");
        assert_eq!(rows[0].sample_count, 2);
        assert_eq!(rows[0].success_rate, 0.5);
        assert_eq!(rows[0].median_duration_ms, Some(200));
    }

    #[test]
    fn rank_approaches_breaks_score_ties_by_approach_ref() {
        let decisions = vec![
            decision_with_id("d1", "alpha", "Bash", "z command"),
            decision_with_id("d2", "alpha", "Bash", "a command"),
        ];
        let resolved = resolved_outcomes(&[("d1", "alpha", 0, 100), ("d2", "alpha", 0, 100)]);

        let rows = rank_approaches(&decisions, &resolved, Some("alpha"));

        assert_eq!(rows[0].approach_ref, "pattern:Bash:a command");
        assert_eq!(rows[1].approach_ref, "pattern:Bash:z command");
    }

    #[test]
    fn truncate_stderr_short() {
        assert_eq!(truncate_stderr("hello"), "hello");
    }

    #[test]
    fn truncate_stderr_long_keeps_tail() {
        let s = "a".repeat(MAX_STDERR_TAIL_BYTES * 2);
        let t = truncate_stderr(&s);
        assert_eq!(t.len(), MAX_STDERR_TAIL_BYTES);
        assert!(t.chars().all(|c| c == 'a'));
    }

    #[test]
    fn truncate_stderr_respects_char_boundary() {
        // "é" is two bytes in UTF-8. Construct a string whose tail boundary
        // would split a multibyte char if we naively sliced.
        let mut s = String::new();
        for _ in 0..MAX_STDERR_TAIL_BYTES {
            s.push('é');
        }
        let t = truncate_stderr(&s);
        // Must be valid UTF-8 (the assertion is implicit in String — we just
        // verify it didn't panic and produced something <= cap bytes).
        assert!(t.len() <= MAX_STDERR_TAIL_BYTES);
    }

    #[test]
    fn normalize_command_collapses_whitespace() {
        assert_eq!(normalize_command("  cargo   test  "), "cargo test");
        assert_eq!(normalize_command("cargo\ttest"), "cargo test");
    }

    #[test]
    fn parse_ts_handles_quoted_and_plain() {
        assert_eq!(parse_ts("123"), Some(123));
        assert_eq!(parse_ts("\"123\""), Some(123));
        assert_eq!(parse_ts("not a number"), None);
    }

    #[test]
    fn pending_outcome_round_trip_json() {
        let p = PendingOutcome {
            provider: AgentProvider::Codex,
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            project: "codexctl".into(),
            session_id: Some("sess-1".into()),
            tool_use_id: Some("tu-1".into()),
            exit_code: Some(0),
            duration_ms: Some(1234),
            stderr_tail: None,
            ts: 100,
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: PendingOutcome = serde_json::from_str(&s).unwrap();
        assert_eq!(back.tool, "Bash");
        assert_eq!(back.command.as_deref(), Some("cargo test"));
        assert_eq!(back.exit_code, Some(0));
    }

    #[test]
    fn pending_outcome_parses_minimal_json() {
        // Hook scripts may omit optional fields.
        let s = r#"{"tool":"Bash","project":"p","ts":1}"#;
        let p: PendingOutcome = serde_json::from_str(s).unwrap();
        assert_eq!(p.tool, "Bash");
        assert!(p.command.is_none());
        assert!(p.exit_code.is_none());
    }

    #[test]
    fn pending_outcome_defaults_legacy_provider_and_retains_explicit_provider() {
        let legacy: PendingOutcome =
            serde_json::from_str(r#"{"tool":"Bash","project":"p","ts":1}"#).unwrap();
        let claude: PendingOutcome =
            serde_json::from_str(r#"{"provider":"claude","tool":"Bash","project":"p","ts":1}"#)
                .unwrap();

        assert_eq!(serde_json::to_value(legacy).unwrap()["provider"], "codex");
        assert_eq!(serde_json::to_value(claude).unwrap()["provider"], "claude");
    }

    #[test]
    fn stable_hook_ids_join_only_the_matching_provider() {
        let temp = tempfile::tempdir().unwrap();
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project".into());
        for (provider, activity_id, decision_id) in [
            (
                coding_brain_core::provider::AgentProvider::Claude,
                "claude-activity",
                "claude-decision",
            ),
            (
                coding_brain_core::provider::AgentProvider::Codex,
                "codex-activity",
                "codex-decision",
            ),
        ] {
            activity
                .append(ActivityEvent {
                    schema_version: ACTIVITY_SCHEMA_VERSION,
                    kind: ActivityKind::Decision,
                    activity_id: activity_id.into(),
                    recorded_at_ms: 1,
                    project: ProjectEvidence {
                        project_id: project_id.clone(),
                        cwd: temp.path().to_path_buf(),
                        label: None,
                    },
                    session: Some(coding_brain_core::brain_activity::SessionTarget {
                        provider,
                        session_id: "same-session".into(),
                        turn_id: Some("same-turn".into()),
                        tool_use_id: Some("same-tool".into()),
                        project_id: project_id.clone(),
                        cwd: temp.path().to_path_buf(),
                        provider_hints: Vec::new(),
                        provenance:
                            coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                    }),
                    state: ActivityState::Allowed,
                    tool: Some("Bash".into()),
                    normalized_command: None,
                    fingerprint: None,
                    rule_id: None,
                    confidence: None,
                    threshold: None,
                    reasoning: None,
                    decision_id: Some(decision_id.into()),
                    outcome: None,
                    correction: None,
                    note: None,
                    supersedes: None,
                })
                .unwrap();
        }
        let pending: PendingOutcome = serde_json::from_value(serde_json::json!({
            "provider": "claude",
            "tool": "Bash",
            "project": "project",
            "session_id": "same-session",
            "tool_use_id": "same-tool",
            "exit_code": 0,
            "ts": 2
        }))
        .unwrap();

        assert_eq!(
            append_activity_outcome(&activity, &pending).unwrap(),
            Some("claude-decision".into())
        );
    }

    #[test]
    fn stable_hook_ids_append_outcome_without_copying_command() {
        let temp = tempfile::tempdir().unwrap();
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project".into());
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "activity-1".into(),
                recorded_at_ms: 1,
                project: ProjectEvidence {
                    project_id: project_id.clone(),
                    cwd: temp.path().to_path_buf(),
                    label: None,
                },
                session: Some(coding_brain_core::brain_activity::SessionTarget {
                    provider: coding_brain_core::provider::AgentProvider::Codex,
                    session_id: "session-1".into(),
                    turn_id: Some("turn-1".into()),
                    tool_use_id: Some("call-1".into()),
                    project_id,
                    cwd: temp.path().to_path_buf(),
                    provider_hints: Vec::new(),
                    provenance:
                        coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
                }),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let pending = PendingOutcome {
            provider: AgentProvider::Codex,
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            project: "wrong-project-name-must-not-matter".into(),
            session_id: Some("session-1".into()),
            tool_use_id: Some("call-1".into()),
            exit_code: Some(0),
            duration_ms: None,
            stderr_tail: None,
            ts: 2,
        };

        assert_eq!(
            append_activity_outcome(&activity, &pending).unwrap(),
            Some("decision-1".into())
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events[1].state, ActivityState::Outcome);
        assert_eq!(events[1].outcome, Some(ActivityOutcome::Succeeded));
        assert!(events[1].normalized_command.is_none());
    }

    #[test]
    fn stable_hook_ids_skip_newer_lifecycle_observation() {
        let temp = tempfile::tempdir().unwrap();
        let activity = ActivityStore::at(temp.path().join("activity.jsonl"));
        let project_id = ProjectId::Temporary("project".into());
        let project = ProjectEvidence {
            project_id: project_id.clone(),
            cwd: temp.path().to_path_buf(),
            label: None,
        };
        let session = coding_brain_core::brain_activity::SessionTarget {
            provider: coding_brain_core::provider::AgentProvider::Codex,
            session_id: "session-1".into(),
            turn_id: Some("turn-1".into()),
            tool_use_id: Some("call-1".into()),
            project_id,
            cwd: temp.path().to_path_buf(),
            provider_hints: Vec::new(),
            provenance: coding_brain_core::brain_activity::SessionTargetProvenance::Structured,
        };
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Decision,
                activity_id: "decision-activity".into(),
                recorded_at_ms: 1,
                project: project.clone(),
                session: Some(session.clone()),
                state: ActivityState::Allowed,
                tool: Some("Bash".into()),
                normalized_command: Some("cargo test".into()),
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: None,
                decision_id: Some("decision-1".into()),
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        activity
            .append(ActivityEvent {
                schema_version: ACTIVITY_SCHEMA_VERSION,
                kind: ActivityKind::Lifecycle,
                activity_id: "lifecycle-activity".into(),
                recorded_at_ms: 2,
                project,
                session: Some(session),
                state: ActivityState::Abstained,
                tool: Some("Bash".into()),
                normalized_command: None,
                fingerprint: None,
                rule_id: None,
                confidence: None,
                threshold: None,
                reasoning: Some("lifecycle observation".into()),
                decision_id: None,
                outcome: None,
                correction: None,
                note: None,
                supersedes: None,
            })
            .unwrap();
        let pending = PendingOutcome {
            provider: AgentProvider::Codex,
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            project: "codexctl".into(),
            session_id: Some("session-1".into()),
            tool_use_id: Some("call-1".into()),
            exit_code: Some(0),
            duration_ms: None,
            stderr_tail: None,
            ts: 3,
        };

        assert_eq!(
            append_activity_outcome(&activity, &pending).unwrap(),
            Some("decision-1".into())
        );
        let events = activity.read().unwrap().events().to_vec();
        assert_eq!(events.len(), 3);
        assert_eq!(events[1].activity_id, "lifecycle-activity");
        assert_eq!(events[1].kind, ActivityKind::Lifecycle);
        assert_eq!(events[1].state, ActivityState::Abstained);
        assert!(events[1].decision_id.is_none());
        assert_eq!(events[2].activity_id, "decision-activity");
        assert_eq!(events[2].state, ActivityState::Outcome);
        assert_eq!(events[2].decision_id.as_deref(), Some("decision-1"));
        assert!(events.iter().all(|event| {
            event.kind != ActivityKind::Diagnostic && event.state != ActivityState::Error
        }));
    }

    #[test]
    fn gen_pending_id_unique_within_process() {
        let a = gen_pending_id();
        let b = gen_pending_id();
        assert_ne!(a, b);
    }

    // ── Test-runner detection (#238) ──────────────────────────────────

    fn runners() -> Vec<String> {
        ["cargo test", "npm test", "pytest", "go test", "bun test"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn is_test_runner_cmd_matches_exact_prefix() {
        assert!(is_test_runner_cmd("cargo test", &runners()));
        assert!(is_test_runner_cmd("pytest", &runners()));
        assert!(is_test_runner_cmd("go test", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_matches_with_args() {
        assert!(is_test_runner_cmd("cargo test --release", &runners()));
        assert!(is_test_runner_cmd("pytest tests/foo.py", &runners()));
        assert!(is_test_runner_cmd("npm test -- --watch=false", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_case_insensitive_and_whitespace() {
        assert!(is_test_runner_cmd("  CARGO   TEST --release  ", &runners()));
        assert!(is_test_runner_cmd("Cargo\tTest", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_rejects_unrelated() {
        assert!(!is_test_runner_cmd("ls", &runners()));
        assert!(!is_test_runner_cmd("cargo build", &runners()));
        // Substring without token boundary must not match.
        assert!(!is_test_runner_cmd("cargotest", &runners()));
        // Empty command is not a test run.
        assert!(!is_test_runner_cmd("", &runners()));
    }

    #[test]
    fn is_test_runner_cmd_empty_runners_never_matches() {
        assert!(!is_test_runner_cmd("cargo test", &[]));
    }

    #[test]
    fn test_failure_marker_round_trip_json() {
        let m = TestFailureMarker {
            decision_id: "dec_1_2_3".into(),
            failed_test_command: "cargo test".into(),
            outcome_ts: 100,
        };
        let s = serde_json::to_string(&m).unwrap();
        let back: TestFailureMarker = serde_json::from_str(&s).unwrap();
        assert_eq!(back.decision_id, "dec_1_2_3");
        assert_eq!(back.failed_test_command, "cargo test");
        assert_eq!(back.outcome_ts, 100);
    }
}
