use std::path::Path;
use std::time::Duration;

use rusqlite::OptionalExtension;

use super::LoopResult;
use super::cli;
use super::config::LoopConfig;
use super::store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopDaemonStatus {
    Due,
    NotDue,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopDaemonDecision {
    pub(crate) loop_name: String,
    pub(crate) status: LoopDaemonStatus,
    pub(crate) reason: Option<String>,
    pub(crate) next_due_epoch: Option<u64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaemonSummary {
    pub(crate) ran: usize,
    pub(crate) skipped: usize,
    pub(crate) not_due: usize,
    pub(crate) failed: usize,
    pub(crate) reconciled: usize,
}

pub(crate) fn run_daemon(
    root: &Path,
    name: Option<&str>,
    once: bool,
    json: bool,
    app_cfg: &crate::config::Config,
) -> LoopResult<()> {
    loop {
        run_daemon_once(root, name, json, app_cfg)?;
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

pub(crate) fn run_daemon_once(
    root: &Path,
    name: Option<&str>,
    json: bool,
    app_cfg: &crate::config::Config,
) -> LoopResult<DaemonSummary> {
    let loops = cli::select_loops(root, name)?;
    let loop_conn = store::open()?;
    let coord_conn = crate::coord::store::open()?;
    let now = now_epoch();
    let decisions = select_due_loop_configs(
        &loops,
        |loop_name| {
            last_finished_run_epoch(&loop_conn, loop_name)
                .ok()
                .flatten()
        },
        cli::is_paused,
        now,
    )?;
    let reconciled = match super::outcome::reconcile_completed() {
        Ok(summary) => summary.resolved,
        Err(err) => {
            emit_event(json, "error", None, "reconcile_failed", Some(err.as_str()));
            0
        }
    };
    let mut summary = DaemonSummary {
        reconciled,
        ..DaemonSummary::default()
    };

    for decision in decisions {
        match decision.status {
            LoopDaemonStatus::Due => {
                let Some(cfg) = loops.iter().find(|cfg| cfg.name == decision.loop_name) else {
                    summary.failed += 1;
                    continue;
                };
                match cli::run_loop_config(root, cfg, &loop_conn, &coord_conn, false, None, app_cfg)
                {
                    Ok(run) => {
                        summary.ran += 1;
                        emit_run(json, cfg, &run);
                    }
                    Err(err) => {
                        summary.failed += 1;
                        emit_event(json, "error", Some(&cfg.name), "run_failed", Some(&err));
                    }
                }
            }
            LoopDaemonStatus::Skipped => {
                summary.skipped += 1;
                emit_event(
                    json,
                    "info",
                    Some(&decision.loop_name),
                    "skipped",
                    decision.reason.as_deref(),
                );
            }
            LoopDaemonStatus::NotDue => {
                summary.not_due += 1;
                emit_event(json, "debug", Some(&decision.loop_name), "not_due", None);
            }
        }
    }

    emit_summary(json, &summary);
    Ok(summary)
}

pub(crate) fn select_due_loop_configs<F, G>(
    configs: &[LoopConfig],
    mut last_finished_epoch: F,
    mut is_paused: G,
    now_epoch: u64,
) -> LoopResult<Vec<LoopDaemonDecision>>
where
    F: FnMut(&str) -> Option<u64>,
    G: FnMut(&str) -> bool,
{
    let mut decisions = Vec::new();
    for cfg in configs {
        if !cfg.enabled {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::Skipped,
                reason: Some("disabled".into()),
                next_due_epoch: None,
            });
            continue;
        }
        if is_paused(&cfg.name) {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::Skipped,
                reason: Some("paused".into()),
                next_due_epoch: None,
            });
            continue;
        }

        let cadence = match cfg.cadence.as_deref().map(parse_cadence).transpose() {
            Ok(cadence) => cadence,
            Err(err) => {
                decisions.push(LoopDaemonDecision {
                    loop_name: cfg.name.clone(),
                    status: LoopDaemonStatus::Skipped,
                    reason: Some(err),
                    next_due_epoch: None,
                });
                continue;
            }
        };
        let Some(cadence) = cadence else {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::Due,
                reason: None,
                next_due_epoch: None,
            });
            continue;
        };
        let Some(last_finished) = last_finished_epoch(&cfg.name) else {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::Due,
                reason: None,
                next_due_epoch: None,
            });
            continue;
        };
        let next_due = last_finished.saturating_add(cadence.as_secs());
        if now_epoch >= next_due {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::Due,
                reason: None,
                next_due_epoch: Some(next_due),
            });
        } else {
            decisions.push(LoopDaemonDecision {
                loop_name: cfg.name.clone(),
                status: LoopDaemonStatus::NotDue,
                reason: Some("cadence".into()),
                next_due_epoch: Some(next_due),
            });
        }
    }
    Ok(decisions)
}

pub(crate) fn parse_cadence(value: &str) -> LoopResult<Duration> {
    let trimmed = value.trim();
    let Some((number, unit)) = trimmed.split_at_checked(trimmed.len().saturating_sub(1)) else {
        return Err("cadence is required".into());
    };
    let amount: u64 = number
        .parse()
        .map_err(|_| format!("invalid cadence {value}"))?;
    if amount == 0 {
        return Err(format!("invalid cadence {value}"));
    }
    let seconds = match unit {
        "m" => amount.saturating_mul(60),
        "h" => amount.saturating_mul(60 * 60),
        "d" => amount.saturating_mul(24 * 60 * 60),
        _ => return Err(format!("unsupported cadence {value}")),
    };
    Ok(Duration::from_secs(seconds))
}

fn last_finished_run_epoch(
    conn: &rusqlite::Connection,
    loop_name: &str,
) -> LoopResult<Option<u64>> {
    let timestamp: Option<String> = conn
        .query_row(
            "SELECT COALESCE(finished_at, started_at)
             FROM loop_runs
             WHERE loop_name = ?1
             ORDER BY started_at DESC
             LIMIT 1",
            rusqlite::params![loop_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("last loop run: {e}"))?;
    Ok(timestamp.and_then(|value| parse_iso_to_epoch(&value)))
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_iso_to_epoch(s: &str) -> Option<u64> {
    if s.len() < 20 || !s.ends_with('Z') {
        return None;
    }
    let year: i64 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u32;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u32 + 2) / 5 + (day - 1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_from_epoch = era * 146097 + doe as i64 - 719468;
    let total = days_from_epoch * 86400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64;
    if total < 0 { None } else { Some(total as u64) }
}

fn emit_run(json: bool, cfg: &LoopConfig, run: &cli::RunSummary) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "event": "loop_run",
                "loop": cfg.name,
                "seen": run.seen,
                "submitted": run.submitted,
                "ignored": run.ignored,
            })
        );
    } else {
        println!(
            "{}: seen={} submitted={} ignored={}",
            cfg.name, run.seen, run.submitted, run.ignored
        );
    }
}

fn emit_event(
    json: bool,
    level: &str,
    loop_name: Option<&str>,
    event: &str,
    message: Option<&str>,
) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "event": event,
                "level": level,
                "loop": loop_name,
                "message": message,
            })
        );
    } else if let Some(loop_name) = loop_name {
        println!("{loop_name}: {event}{}", format_message(message));
    } else {
        println!("{event}{}", format_message(message));
    }
}

fn emit_summary(json: bool, summary: &DaemonSummary) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "event": "daemon_tick",
                "ran": summary.ran,
                "skipped": summary.skipped,
                "not_due": summary.not_due,
                "failed": summary.failed,
                "reconciled": summary.reconciled,
            })
        );
    } else {
        println!(
            "daemon: ran={} skipped={} not_due={} failed={} reconciled={}",
            summary.ran, summary.skipped, summary.not_due, summary.failed, summary.reconciled
        );
    }
}

fn format_message(message: Option<&str>) -> String {
    message
        .map(|message| format!(" ({message})"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::r#loop::config::LoopConfig;
    use std::sync::Mutex;
    use std::time::Duration;

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn parse_cadence_accepts_minutes_hours_and_days() {
        assert_eq!(parse_cadence("15m").unwrap(), Duration::from_secs(15 * 60));
        assert_eq!(
            parse_cadence("2h").unwrap(),
            Duration::from_secs(2 * 60 * 60)
        );
        assert_eq!(
            parse_cadence("1d").unwrap(),
            Duration::from_secs(24 * 60 * 60)
        );
    }

    #[test]
    fn parse_cadence_rejects_unknown_units() {
        let err = parse_cadence("30s").unwrap_err();

        assert!(err.contains("unsupported cadence"));
    }

    #[test]
    fn select_due_loops_marks_new_enabled_loops_due() {
        let cfg = LoopConfig::minimal_for_test("issue-triage");
        let decisions = select_due_loop_configs(&[cfg], |_| None, |_| false, 100).unwrap();

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].loop_name, "issue-triage");
        assert_eq!(decisions[0].status, LoopDaemonStatus::Due);
    }

    #[test]
    fn select_due_loops_skips_recent_runs_until_cadence_elapses() {
        let mut cfg = LoopConfig::minimal_for_test("issue-triage");
        cfg.cadence = Some("2h".into());

        let decisions =
            select_due_loop_configs(&[cfg], |_| Some(1000), |_| false, 1000 + 60 * 60).unwrap();

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].status, LoopDaemonStatus::NotDue);
    }

    #[test]
    fn select_due_loops_reports_paused_loops_as_skipped() {
        let cfg = LoopConfig::minimal_for_test("issue-triage");

        let decisions =
            select_due_loop_configs(&[cfg], |_| None, |name| name == "issue-triage", 100).unwrap();

        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].status, LoopDaemonStatus::Skipped);
        assert_eq!(decisions[0].reason.as_deref(), Some("paused"));
    }

    #[test]
    fn run_daemon_once_executes_due_loop_and_submits_coord_task() {
        let _home_lock = HOME_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_shell_loop(project.path(), Some("15m"));
        let _guard = EnvGuard::set_home(home.path());

        let summary = run_daemon_once(
            project.path(),
            None,
            false,
            &crate::config::Config::default(),
        )
        .unwrap();

        assert_eq!(summary.ran, 1);
        assert_eq!(summary.skipped, 0);

        let loop_conn = crate::r#loop::store::open().unwrap();
        let rows = crate::r#loop::store::list_items(&loop_conn, Some("issue-triage")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].state,
            crate::r#loop::store::LoopItemState::Submitted
        );
        assert!(rows[0].coord_task_id.is_some());

        let coord_conn = crate::coord::store::open().unwrap();
        let task =
            crate::coord::tasks::get_task(&coord_conn, rows[0].coord_task_id.as_deref().unwrap())
                .unwrap()
                .unwrap();
        assert_eq!(task.state, crate::coord::tasks::TaskState::Pending);
    }

    #[test]
    fn run_daemon_once_does_not_duplicate_coord_tasks_for_stable_source_id() {
        let _home_lock = HOME_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_shell_loop(project.path(), None);
        let _guard = EnvGuard::set_home(home.path());

        run_daemon_once(
            project.path(),
            None,
            false,
            &crate::config::Config::default(),
        )
        .unwrap();
        run_daemon_once(
            project.path(),
            None,
            false,
            &crate::config::Config::default(),
        )
        .unwrap();

        let coord_conn = crate::coord::store::open().unwrap();
        let task_count: i64 = coord_conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .unwrap();

        assert_eq!(task_count, 1);
    }

    fn write_shell_loop(root: &std::path::Path, cadence: Option<&str>) {
        let loop_dir = root.join(".codexctl/loops");
        std::fs::create_dir_all(&loop_dir).unwrap();
        let cadence_line = cadence
            .map(|value| format!("cadence = \"{value}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            loop_dir.join("issue-triage.toml"),
            format!(
                r#"
name = "issue-triage"
enabled = true
mode = "assisted"
{cadence_line}

[source]
kind = "shell"
command = "printf '{{"id":"one","title":"One","body":"Body"}}\n'"
limit = 1

[triage]
mode = "deterministic"

[execution]
cwd = "."
worktree = "none"
session = "headless"

[[verify]]
kind = "run"
command = "cargo test"

[gates]
max_items_per_run = 1
"#
            ),
        )
        .unwrap();
    }

    struct EnvGuard {
        original_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_home(path: &std::path::Path) -> Self {
            let guard = Self {
                original_home: std::env::var_os("HOME"),
            };
            unsafe {
                std::env::set_var("HOME", path);
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(home) = &self.original_home {
                    std::env::set_var("HOME", home);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }
}
