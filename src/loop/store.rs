#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::LoopResult;
use super::sources::SourceItem;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".codexctl")
        .join("loop")
        .join("loop.db")
}

fn now_iso() -> String {
    crate::logger::timestamp_now()
}

fn gen_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos}_{seq}")
}

pub fn open() -> LoopResult<Connection> {
    open_at(&db_path())
}

pub fn open_at(path: &Path) -> LoopResult<Connection> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("open loop db: {e}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("WAL mode: {e}"))?;
    migrate(&conn).map_err(|e| format!("migrate: {e}"))?;
    Ok(conn)
}

pub fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory loop db");
    migrate(&conn).expect("migrate in-memory loop db");
    conn
}

fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS loop_runs (
            id              TEXT PRIMARY KEY,
            loop_name       TEXT NOT NULL,
            config_path     TEXT NOT NULL,
            started_at      TEXT NOT NULL,
            finished_at     TEXT,
            status          TEXT NOT NULL,
            items_seen      INTEGER NOT NULL DEFAULT 0,
            items_submitted INTEGER NOT NULL DEFAULT 0,
            items_ignored   INTEGER NOT NULL DEFAULT 0,
            error           TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_loop_runs_loop ON loop_runs(loop_name, started_at);

        CREATE TABLE IF NOT EXISTS loop_sources (
            loop_name   TEXT NOT NULL,
            source_key  TEXT NOT NULL,
            cursor_json TEXT,
            updated_at  TEXT NOT NULL,
            PRIMARY KEY(loop_name, source_key)
        );

        CREATE TABLE IF NOT EXISTS loop_items (
            id              TEXT PRIMARY KEY,
            loop_name       TEXT NOT NULL,
            source_kind     TEXT NOT NULL,
            source_item_id  TEXT NOT NULL,
            dedupe_key      TEXT NOT NULL UNIQUE,
            title           TEXT NOT NULL,
            body_summary    TEXT NOT NULL,
            url             TEXT,
            raw_json        TEXT NOT NULL,
            state           TEXT NOT NULL,
            decision_json   TEXT,
            coord_task_id   TEXT,
            worktree_path   TEXT,
            first_seen_at   TEXT NOT NULL,
            last_seen_at    TEXT NOT NULL,
            updated_at      TEXT NOT NULL,
            last_error      TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_loop_items_loop ON loop_items(loop_name, state, updated_at);
        CREATE INDEX IF NOT EXISTS idx_loop_items_coord ON loop_items(coord_task_id);

        CREATE TABLE IF NOT EXISTS loop_events (
            id          TEXT PRIMARY KEY,
            loop_name   TEXT,
            run_id      TEXT,
            item_id     TEXT,
            level       TEXT NOT NULL,
            event_type  TEXT NOT NULL,
            message     TEXT NOT NULL,
            data_json   TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_loop_events_loop ON loop_events(loop_name, created_at);
        CREATE INDEX IF NOT EXISTS idx_loop_events_item ON loop_events(item_id, created_at);
        ",
    )
}

#[derive(Debug, Clone)]
pub struct NewLoopItem {
    pub loop_name: String,
    pub source_kind: String,
    pub source_item_id: String,
    pub title: String,
    pub body_summary: String,
    pub url: Option<String>,
    pub raw_json: serde_json::Value,
}

impl NewLoopItem {
    pub fn from_source(loop_name: &str, item: &SourceItem) -> Self {
        Self {
            loop_name: loop_name.into(),
            source_kind: item.source_kind.clone(),
            source_item_id: item.source_item_id.clone(),
            title: item.title.clone(),
            body_summary: item.summary(),
            url: item.url.clone(),
            raw_json: item.raw_json.clone(),
        }
    }

    #[cfg(test)]
    pub fn for_test(loop_name: &str, source_item_id: &str) -> Self {
        Self {
            loop_name: loop_name.into(),
            source_kind: "test".into(),
            source_item_id: source_item_id.into(),
            title: "Test item".into(),
            body_summary: "Test body".into(),
            url: None,
            raw_json: serde_json::json!({"id": source_item_id}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopItemState {
    Seen,
    Ignored,
    Reported,
    Submitted,
    Escalated,
    Done,
    Failed,
}

impl LoopItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seen => "seen",
            Self::Ignored => "ignored",
            Self::Reported => "reported",
            Self::Submitted => "submitted",
            Self::Escalated => "escalated",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "ignored" => Self::Ignored,
            "reported" => Self::Reported,
            "submitted" => Self::Submitted,
            "escalated" => Self::Escalated,
            "done" => Self::Done,
            "failed" => Self::Failed,
            _ => Self::Seen,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopItemRow {
    pub id: String,
    pub loop_name: String,
    pub source_kind: String,
    pub source_item_id: String,
    pub title: String,
    pub body_summary: String,
    pub url: Option<String>,
    pub raw_json: serde_json::Value,
    pub state: LoopItemState,
    pub decision_json: Option<serde_json::Value>,
    pub coord_task_id: Option<String>,
    pub worktree_path: Option<String>,
    pub last_error: Option<String>,
}

pub fn begin_run(conn: &Connection, loop_name: &str, config_path: &Path) -> LoopResult<String> {
    let id = gen_id("loop_run");
    let now = now_iso();
    conn.execute(
        "INSERT INTO loop_runs(id, loop_name, config_path, started_at, status)
         VALUES (?1, ?2, ?3, ?4, 'running')",
        params![id, loop_name, config_path.to_string_lossy(), now],
    )
    .map_err(|e| format!("begin loop run: {e}"))?;
    Ok(id)
}

pub fn finish_run(
    conn: &Connection,
    run_id: &str,
    status: &str,
    error: Option<&str>,
) -> LoopResult<()> {
    let now = now_iso();
    conn.execute(
        "UPDATE loop_runs SET status = ?1, finished_at = ?2, error = ?3 WHERE id = ?4",
        params![status, now, error, run_id],
    )
    .map_err(|e| format!("finish loop run: {e}"))?;
    Ok(())
}

pub fn upsert_item(conn: &Connection, item: &NewLoopItem) -> LoopResult<String> {
    let id = gen_id("loop_item");
    let now = now_iso();
    let dedupe_key = dedupe_key(&item.loop_name, &item.source_kind, &item.source_item_id);
    let raw_json =
        serde_json::to_string(&item.raw_json).map_err(|e| format!("raw item json: {e}"))?;
    conn.execute(
        "INSERT OR IGNORE INTO loop_items
         (id, loop_name, source_kind, source_item_id, dedupe_key, title, body_summary, url,
          raw_json, state, first_seen_at, last_seen_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'seen', ?10, ?10, ?10)",
        params![
            id,
            item.loop_name,
            item.source_kind,
            item.source_item_id,
            dedupe_key,
            item.title,
            item.body_summary,
            item.url,
            raw_json,
            now,
        ],
    )
    .map_err(|e| format!("insert loop item: {e}"))?;
    conn.execute(
        "UPDATE loop_items
         SET title = ?1, body_summary = ?2, url = ?3, raw_json = ?4,
             last_seen_at = ?5, updated_at = ?5
         WHERE dedupe_key = ?6",
        params![
            item.title,
            item.body_summary,
            item.url,
            raw_json,
            now,
            dedupe_key,
        ],
    )
    .map_err(|e| format!("refresh loop item: {e}"))?;

    conn.query_row(
        "SELECT id FROM loop_items WHERE dedupe_key = ?1",
        params![dedupe_key],
        |row| row.get::<_, String>(0),
    )
    .map_err(|e| format!("lookup loop item: {e}"))
}

pub fn get_item(conn: &Connection, item_id: &str) -> LoopResult<Option<LoopItemRow>> {
    conn.query_row(
        item_select_sql("WHERE id = ?1").as_str(),
        params![item_id],
        row_to_item,
    )
    .optional()
    .map_err(|e| format!("get loop item: {e}"))
}

pub fn list_items(conn: &Connection, loop_name: Option<&str>) -> LoopResult<Vec<LoopItemRow>> {
    let (sql, param) = match loop_name {
        Some(name) => (
            item_select_sql("WHERE loop_name = ?1 ORDER BY updated_at DESC"),
            Some(name.to_string()),
        ),
        None => (item_select_sql("ORDER BY updated_at DESC"), None),
    };
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("prepare: {e}"))?;
    let rows = if let Some(name) = param {
        stmt.query_map(params![name], row_to_item)
            .map_err(|e| format!("query loop items: {e}"))?
            .collect::<Result<Vec<_>, _>>()
    } else {
        stmt.query_map([], row_to_item)
            .map_err(|e| format!("query loop items: {e}"))?
            .collect::<Result<Vec<_>, _>>()
    }
    .map_err(|e| format!("row loop item: {e}"))?;
    Ok(rows)
}

pub fn mark_decision(
    conn: &Connection,
    item_id: &str,
    state: LoopItemState,
    decision: &serde_json::Value,
) -> LoopResult<()> {
    let decision_json =
        serde_json::to_string(decision).map_err(|e| format!("decision json: {e}"))?;
    let now = now_iso();
    conn.execute(
        "UPDATE loop_items SET state = ?1, decision_json = ?2, updated_at = ?3 WHERE id = ?4",
        params![state.as_str(), decision_json, now, item_id],
    )
    .map_err(|e| format!("mark loop decision: {e}"))?;
    Ok(())
}

pub fn mark_submitted(
    conn: &Connection,
    item_id: &str,
    task_id: &str,
    worktree_path: Option<&str>,
) -> LoopResult<()> {
    let now = now_iso();
    conn.execute(
        "UPDATE loop_items
         SET state = 'submitted', coord_task_id = ?1, worktree_path = ?2, updated_at = ?3
         WHERE id = ?4",
        params![task_id, worktree_path, now, item_id],
    )
    .map_err(|e| format!("mark submitted: {e}"))?;
    Ok(())
}

pub fn log_event(
    conn: &Connection,
    run_id: Option<&str>,
    item_id: Option<&str>,
    level: &str,
    event_type: &str,
    message: &str,
    data: serde_json::Value,
) -> LoopResult<()> {
    let id = gen_id("loop_event");
    let now = now_iso();
    let data_json = serde_json::to_string(&data).map_err(|e| format!("event json: {e}"))?;
    let loop_name =
        item_id.and_then(|id| get_item(conn, id).ok().flatten().map(|item| item.loop_name));
    conn.execute(
        "INSERT INTO loop_events(id, loop_name, run_id, item_id, level, event_type, message,
                                data_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            id, loop_name, run_id, item_id, level, event_type, message, data_json, now,
        ],
    )
    .map_err(|e| format!("log loop event: {e}"))?;
    Ok(())
}

fn dedupe_key(loop_name: &str, source_kind: &str, source_item_id: &str) -> String {
    format!("{loop_name}:{source_kind}:{source_item_id}")
}

fn item_select_sql(suffix: &str) -> String {
    format!(
        "SELECT id, loop_name, source_kind, source_item_id, title, body_summary, url,
                raw_json, state, decision_json, coord_task_id, worktree_path, last_error
         FROM loop_items {suffix}"
    )
}

fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoopItemRow> {
    let raw_json: String = row.get(7)?;
    let decision_json: Option<String> = row.get(9)?;
    let state: String = row.get(8)?;
    Ok(LoopItemRow {
        id: row.get(0)?,
        loop_name: row.get(1)?,
        source_kind: row.get(2)?,
        source_item_id: row.get(3)?,
        title: row.get(4)?,
        body_summary: row.get(5)?,
        url: row.get(6)?,
        raw_json: serde_json::from_str(&raw_json).unwrap_or(serde_json::Value::Null),
        state: LoopItemState::parse(&state),
        decision_json: decision_json.and_then(|json| serde_json::from_str(&json).ok()),
        coord_task_id: row.get(10)?,
        worktree_path: row.get(11)?,
        last_error: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_item_dedupes_by_loop_source_and_item_id() {
        let conn = open_memory();
        let item = NewLoopItem {
            loop_name: "issue-triage".into(),
            source_kind: "github_issues".into(),
            source_item_id: "github:aleadag/codexctl#123".into(),
            title: "Bug".into(),
            body_summary: "Body".into(),
            url: Some("https://github.com/aleadag/codexctl/issues/123".into()),
            raw_json: serde_json::json!({"number": 123}),
        };

        let first = upsert_item(&conn, &item).unwrap();
        let second = upsert_item(&conn, &item).unwrap();
        let rows = list_items(&conn, Some("issue-triage")).unwrap();

        assert_eq!(first, second);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, LoopItemState::Seen);
    }

    #[test]
    fn mark_submitted_stores_coord_task_id() {
        let conn = open_memory();
        let id = upsert_item(&conn, &NewLoopItem::for_test("loop-a", "source-1")).unwrap();

        mark_submitted(&conn, &id, "task_1", Some("/tmp/worktree")).unwrap();
        let row = get_item(&conn, &id).unwrap().unwrap();

        assert_eq!(row.state, LoopItemState::Submitted);
        assert_eq!(row.coord_task_id.as_deref(), Some("task_1"));
        assert_eq!(row.worktree_path.as_deref(), Some("/tmp/worktree"));
    }
}
