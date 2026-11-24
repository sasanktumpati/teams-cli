use crate::paths;
use rusqlite::{Connection, OpenFlags, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub const RW_CACHE_KIB: i64 = -16_000;
pub const RO_CACHE_KIB: i64 = -512;
pub const BUSY_TIMEOUT_MS: i64 = 30_000;
pub const WAL_AUTOCHECKPOINT_PAGES: i64 = 1000;

pub const ACTIVITY_SQL: &str = "MAX(COALESCE(c.last_message_at, 0), COALESCE((SELECT MAX(x.arrived_at) FROM messages x WHERE x.conversation_id = c.id), 0))";
pub const MESSAGE_COUNT_SQL: &str =
    "(SELECT COUNT(*) FROM messages x WHERE x.conversation_id = c.id AND x.kind = 'message')";
pub const MESSAGE_COLS_SQL: &str = "
    m.pk, m.conversation_id, m.message_id, m.thread_id, m.parent_id, m.arrived_at, m.edited_at,
    m.deleted_at, m.sender_mri, COALESCE(NULLIF(m.sender_name, ''), p.name, m.sender_mri) AS sender,
    m.message_type, m.kind, m.text, m.subject, m.importance, m.reactions, m.content_hash,
    (SELECT d.name FROM people d WHERE d.mri = json_extract(m.properties, '$.deletedBy')) AS deleted_by
";

pub fn connect_rw(path: Option<&Path>) -> Result<Connection> {
    let p = path.map(|x| x.to_path_buf()).unwrap_or_else(paths::archive_path);
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let is_new = !p.exists();
    let conn = Connection::open_with_flags(
        &p,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    if is_new {
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o600));
    }
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = {RW_CACHE_KIB};
         PRAGMA wal_autocheckpoint = {WAL_AUTOCHECKPOINT_PAGES};"
    ))?;
    Ok(conn)
}

pub fn connect_ro(path: Option<&Path>) -> Result<Connection> {
    let p = path.map(|x| x.to_path_buf()).unwrap_or_else(paths::archive_path);
    let conn = Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.execute_batch(&format!(
        "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};
         PRAGMA query_only = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = {RO_CACHE_KIB};"
    ))?;
    Ok(conn)
}

pub fn db_file(conn: &Connection) -> String {
    let mut stmt = match conn.prepare("PRAGMA database_list") {
        Ok(s) => s,
        Err(_) => return String::new(),
    };
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .ok();
    if let Some(rows) = rows {
        for r in rows.flatten() {
            if r.0 == "main" {
                return r.1;
            }
        }
    }
    String::new()
}

pub fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE name = ?", [name], |_| Ok(()))
        .is_ok()
}

pub fn optimize(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA optimize;");
}

pub fn shrink_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

pub fn wal_checkpoint(conn: &Connection, mode: &str) -> Result<(i64, i64, i64)> {
    if !matches!(mode, "PASSIVE" | "FULL" | "RESTART" | "TRUNCATE") {
        return Err(rusqlite::Error::InvalidQuery);
    }
    conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
}

pub fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

pub fn like_pattern(s: &str) -> String {
    format!("%{}%", like_escape(s))
}

pub fn id_list<I, S>(ids: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut v: Vec<String> = ids.into_iter().map(|s| s.as_ref().to_string()).collect();
    v.sort();
    v.dedup();
    crate::json_codec::to_string(&v)
}

pub const LOOKUP_CHUNK: usize = 300;

pub fn values_placeholders(rows: usize, cols: usize) -> String {
    let one = format!("({})", vec!["?"; cols].join(", "));
    vec![one; rows].join(", ")
}
