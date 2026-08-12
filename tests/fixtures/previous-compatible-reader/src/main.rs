use std::path::PathBuf;

use rusqlite::{Connection, OpenFlags};

fn main() {
    let state_root = PathBuf::from(std::env::args_os().nth(1).expect("state root argument"));
    let database = state_root.join("db/brain.sqlite3");
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .expect("open pre-cache Brain database");
    let application_id: i32 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .expect("read Brain application id");
    let schema_version: i32 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read Brain schema version");
    assert_eq!(application_id, 0x4342_524e);
    assert_eq!(schema_version, 1);

    let mut statement = connection
        .prepare("SELECT event_payload FROM activity_events ORDER BY source_cursor ASC")
        .expect("prepare pre-cache activity query");
    let payloads = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .expect("query pre-cache activities");
    for payload in payloads {
        let payload = payload.expect("read pre-cache activity");
        std::io::Write::write_all(&mut std::io::stdout(), &payload)
            .expect("write pre-cache activity");
        println!();
    }
}
