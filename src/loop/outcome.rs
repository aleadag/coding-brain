use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::LoopResult;
use super::store;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OutcomeSummary {
    pub resolved: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptOutcome {
    pub cwd: String,
    pub final_message: String,
}

pub fn reconcile_completed() -> LoopResult<OutcomeSummary> {
    let loop_conn = store::open()?;
    let coord_conn = crate::coord::store::open()?;
    let transcripts = scan_transcript_outcomes();
    reconcile_completed_with_transcripts(&loop_conn, &coord_conn, &transcripts)
}

pub fn reconcile_completed_with_transcripts(
    loop_conn: &Connection,
    coord_conn: &Connection,
    transcripts: &[TranscriptOutcome],
) -> LoopResult<OutcomeSummary> {
    let mut summary = OutcomeSummary::default();

    for item in store::list_submitted_items(loop_conn)? {
        let Some(task_id) = item.coord_task_id.as_deref() else {
            summary.skipped += 1;
            continue;
        };
        let Some(task) = crate::coord::tasks::get_task(coord_conn, task_id)? else {
            summary.skipped += 1;
            continue;
        };
        if task.state != crate::coord::tasks::TaskState::Done {
            summary.skipped += 1;
            continue;
        }

        let Some(transcript) = transcripts
            .iter()
            .find(|transcript| transcript.cwd == task.cwd)
        else {
            let error = "completed task transcript not found";
            mark_outcome_failed(loop_conn, &item.id, error)?;
            summary.failed += 1;
            continue;
        };
        let Some(url) = extract_github_pr_url(&transcript.final_message) else {
            let error = "completed task finished without a reported PR URL";
            mark_outcome_failed(loop_conn, &item.id, error)?;
            summary.failed += 1;
            continue;
        };

        store::mark_done(loop_conn, &item.id, Some(&url))?;
        store::log_event(
            loop_conn,
            None,
            Some(&item.id),
            "info",
            "result_url_recorded",
            "recorded result URL from completed loop task transcript",
            serde_json::json!({ "url": url }),
        )?;
        summary.resolved += 1;
    }

    Ok(summary)
}

fn mark_outcome_failed(loop_conn: &Connection, item_id: &str, error: &str) -> LoopResult<()> {
    store::mark_failed(loop_conn, item_id, error)?;
    store::log_event(
        loop_conn,
        None,
        Some(item_id),
        "error",
        "outcome_missing",
        error,
        serde_json::json!({}),
    )
}

fn scan_transcript_outcomes() -> Vec<TranscriptOutcome> {
    let mut paths = Vec::new();
    collect_rollout_jsonls(&codex_sessions_dir(), &mut paths);
    paths.sort_by_key(|path| std::cmp::Reverse(file_mtime_ms(path).unwrap_or_default()));
    paths
        .into_iter()
        .filter_map(transcript_outcome_from_jsonl)
        .collect()
}

fn codex_sessions_dir() -> PathBuf {
    std::env::var_os("CODEXCTL_CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".codex"))
        .join("sessions")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn collect_rollout_jsonls(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_jsonls(&path, paths);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-"))
        {
            paths.push(path);
        }
    }
}

fn transcript_outcome_from_jsonl(path: PathBuf) -> Option<TranscriptOutcome> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut cwd = None;
    let mut final_message = None;

    use std::io::BufRead;
    for line in reader.lines().map_while(Result::ok) {
        match codexctl_core::codex_transcript::parse_line(line.trim()) {
            Some(codexctl_core::codex_transcript::CodexEvent::SessionMeta(meta)) => {
                cwd = Some(meta.cwd);
            }
            Some(codexctl_core::codex_transcript::CodexEvent::ResponseItem(item))
                if item.kind == codexctl_core::codex_transcript::CodexResponseKind::Message
                    && item.role.as_deref() == Some("assistant") =>
            {
                if let Some(text) = item.text {
                    final_message = Some(text);
                }
            }
            Some(codexctl_core::codex_transcript::CodexEvent::EventMessage(message)) => {
                final_message = Some(message);
            }
            _ => {}
        }
    }

    Some(TranscriptOutcome {
        cwd: cwd?,
        final_message: final_message?,
    })
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    Some(
        std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis() as u64,
    )
}

fn extract_github_pr_url(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(idx) = rest.find("https://github.com/") {
        let candidate = &rest[idx..];
        let end = candidate
            .find(|ch: char| ch.is_whitespace() || matches!(ch, ')' | ']' | '}' | '<' | '>'))
            .unwrap_or(candidate.len());
        let url = candidate[..end].trim_end_matches(['.', ',', ';', ':', '`', '\'', '"']);
        if url.contains("/pull/") {
            return Some(url.to_string());
        }
        rest = &candidate[end..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coord::tasks::{NewTask, TaskState};

    fn done_loop_task(
        loop_conn: &Connection,
        coord_conn: &mut Connection,
        cwd: &str,
    ) -> (String, String) {
        let source_item = crate::r#loop::sources::SourceItem::for_test("github:aleadag/codexctl#1");
        let item_id = store::upsert_item(
            loop_conn,
            &store::NewLoopItem::from_source("issue-triage", &source_item),
        )
        .unwrap();
        let task_id = crate::coord::tasks::insert_task(
            coord_conn,
            &NewTask {
                name: "Fix it".into(),
                role: None,
                cwd: cwd.into(),
                prompt: "Fix it".into(),
                model: None,
                budget_usd: None,
                max_retries: None,
                timeout_min: None,
                depends_on: Vec::new(),
                policy: None,
                verifiers: Vec::new(),
            },
        )
        .unwrap();
        crate::coord::tasks::transition(
            coord_conn,
            &task_id,
            TaskState::Pending,
            TaskState::Done,
            "test",
        )
        .unwrap();
        store::mark_submitted(loop_conn, &item_id, &task_id, None).unwrap();
        (item_id, task_id)
    }

    #[test]
    fn reconciles_done_task_from_reported_pr_url() {
        let loop_conn = store::open_memory();
        let mut coord_conn = crate::coord::store::open_memory();
        let (item_id, _) = done_loop_task(&loop_conn, &mut coord_conn, "/work/task-1");

        let summary = reconcile_completed_with_transcripts(
            &loop_conn,
            &coord_conn,
            &[TranscriptOutcome {
                cwd: "/work/task-1".into(),
                final_message: "Opened PR: https://github.com/aleadag/codexctl/pull/4".into(),
            }],
        )
        .unwrap();
        let row = store::get_item(&loop_conn, &item_id).unwrap().unwrap();

        assert_eq!(summary.resolved, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(row.state, store::LoopItemState::Done);
        assert_eq!(
            row.result_url.as_deref(),
            Some("https://github.com/aleadag/codexctl/pull/4")
        );
    }

    #[test]
    fn fails_done_task_without_reported_pr_url() {
        let loop_conn = store::open_memory();
        let mut coord_conn = crate::coord::store::open_memory();
        let (item_id, _) = done_loop_task(&loop_conn, &mut coord_conn, "/work/task-1");

        let summary = reconcile_completed_with_transcripts(
            &loop_conn,
            &coord_conn,
            &[TranscriptOutcome {
                cwd: "/work/task-1".into(),
                final_message: "Tests passed, but I did not create a PR.".into(),
            }],
        )
        .unwrap();
        let row = store::get_item(&loop_conn, &item_id).unwrap().unwrap();

        assert_eq!(summary.resolved, 0);
        assert_eq!(summary.failed, 1);
        assert_eq!(row.state, store::LoopItemState::Failed);
        assert_eq!(row.result_url, None);
        assert!(
            row.last_error
                .as_deref()
                .unwrap()
                .contains("without a reported PR URL")
        );
    }

    #[test]
    fn extracts_github_pr_url_without_trailing_punctuation() {
        assert_eq!(
            extract_github_pr_url("PR: https://github.com/aleadag/codexctl/pull/4."),
            Some("https://github.com/aleadag/codexctl/pull/4".into())
        );
    }
}
