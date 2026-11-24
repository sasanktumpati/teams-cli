use crate::paths;
use crate::storage::archive::{
    self, compress_bytes, extract_conv_read_state, extract_mention_oids, index_conversation_members,
    normalize_record_key, ArchiveError,
};
use crate::storage::{backups, db};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Instant;

pub const APP_VERSION: &str = crate::VERSION;

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub description: &'static str,
    pub needs_vacuum_on_upgrade: bool,
    pub apply: fn(&Connection) -> Result<(), ArchiveError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub name: String,
    pub applied_at: i64,
    pub app_version: String,
    pub duration_ms: i64,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        description: "Core people, conversations, messages, revisions, attachments, meta, sync_runs, and messages_fts",
        needs_vacuum_on_upgrade: false,
        apply: up_v1,
    },
    Migration {
        version: 2,
        name: "record_versions",
        description: "Append-only raw Teams record_versions store",
        needs_vacuum_on_upgrade: false,
        apply: up_v2,
    },
    Migration {
        version: 3,
        name: "compressed_records_and_revision_fts",
        description: "Zlib-compressed BLOB payloads in record_versions, normalized keys, and message_revisions_fts",
        needs_vacuum_on_upgrade: true,
        apply: up_v3,
    },
    Migration {
        version: 4,
        name: "query_indexes",
        description: "Indexes for message_id lookups, conversation timelines, edited/deleted views, and attachments",
        needs_vacuum_on_upgrade: false,
        apply: up_v4,
    },
    Migration {
        version: 5,
        name: "conversation_members_and_composite_indexes",
        description: "Normalized conversation_members lookup table and composite message/record indexes",
        needs_vacuum_on_upgrade: false,
        apply: up_v5,
    },
    Migration {
        version: 6,
        name: "message_mentions_and_read_state",
        description:
            "Indexed message_mentions table and persisted conversation read_horizon/is_read/last_from_me columns",
        needs_vacuum_on_upgrade: false,
        apply: up_v6,
    },
    Migration {
        version: 7,
        name: "sender_name_index",
        description: "Partial index on messages (sender_mri, sender_name) for fast sender lookup",
        needs_vacuum_on_upgrade: false,
        apply: up_v7,
    },
    Migration {
        version: 8,
        name: "schema_migrations_and_perf_indexes",
        description: "Persistent schema_migrations ledger and covering indexes for status, records, and attachments",
        needs_vacuum_on_upgrade: false,
        apply: up_v8,
    },
];

pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

pub fn check_schema_version(current: i64) -> Result<(), ArchiveError> {
    if current > SCHEMA_VERSION {
        return Err(ArchiveError(format!(
            "Archive schema v{current} is newer than this teams-cli v{APP_VERSION} (schema v{SCHEMA_VERSION}). Upgrade teams-cli."
        )));
    }
    Ok(())
}

pub fn pending(conn: &Connection) -> Result<Vec<&'static Migration>, ArchiveError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    check_schema_version(current)?;
    Ok(MIGRATIONS.iter().filter(|m| m.version > current).collect())
}

pub fn history(conn: &Connection) -> Result<Vec<AppliedMigration>, ArchiveError> {
    if !db::table_exists(conn, "schema_migrations") {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT version, name, applied_at, app_version, duration_ms FROM schema_migrations ORDER BY version",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(AppliedMigration {
                version: r.get(0)?,
                name: r.get(1)?,
                applied_at: r.get(2)?,
                app_version: r.get(3)?,
                duration_ms: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn migrate(conn: &Connection) -> Result<(), ArchiveError> {
    let mut current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    check_schema_version(current)?;
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    let db_file = db::db_file(conn);
    let _lock_guard = if !db_file.is_empty() {
        let p = Path::new(&db_file);
        let lock_name = format!(
            "{}.migrate.lock",
            p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        );
        let lock_path = p.with_file_name(lock_name);
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| ArchiveError(format!("migrate lock: {e}")))?;
        f.lock()
            .map_err(|e| ArchiveError(format!("migrate lock acquire: {e}")))?;
        current = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        check_schema_version(current)?;
        if current == SCHEMA_VERSION {
            return Ok(());
        }
        Some(f)
    } else {
        None
    };

    let upgrading = current > 0 && current < SCHEMA_VERSION && archive::has_data(conn);
    let err_marker = (!db_file.is_empty()).then(|| Path::new(&db_file).with_file_name(".migrate.error"));
    let record_err = |e: &ArchiveError| {
        if let Some(ref mp) = err_marker {
            let _ = paths::atomic_write_text(mp, &e.0);
        }
    };
    if upgrading {
        let bdir = paths::backups_dir();
        let pages: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))? as u64;
        let psize: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))? as u64;
        let db_size = pages.saturating_mul(psize);
        let free = paths::disk_free_bytes(&bdir);
        if free > 0 && free.saturating_sub(db_size) < backups::MIN_FREE_BYTES {
            let e = ArchiveError(format!(
                "Cannot upgrade archive to v{SCHEMA_VERSION}: only {:.1} GB free on disk for the pre-upgrade safety backup.",
                free as f64 / 1e9
            ));
            record_err(&e);
            return Err(e);
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let dest = bdir.join(format!("archive-before-v{SCHEMA_VERSION}-{stamp}.sqlite3.xz"));
        if !dest.exists() {
            if let Err(e) = backups::backup(conn, &dest) {
                record_err(&e);
                return Err(e);
            }
        }
    }

    let mut need_vacuum = false;
    for m in MIGRATIONS.iter().filter(|m| m.version > current) {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let res = (|| -> Result<(), ArchiveError> {
            let cur_now: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            if cur_now >= m.version {
                return Ok(());
            }
            apply_migration(conn, m)
        })();
        match res {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK;");
                record_err(&e);
                return Err(e);
            }
        }
        if upgrading && m.needs_vacuum_on_upgrade {
            need_vacuum = true;
        }
    }
    if let Some(ref mp) = err_marker {
        let _ = std::fs::remove_file(mp);
    }
    if need_vacuum {
        if let Err(e) = conn.execute_batch("VACUUM;") {
            eprintln!("teams-cli: warning: post-migration VACUUM skipped ({e}); schema is up to date.");
        }
    }
    Ok(())
}

fn apply_migration(conn: &Connection, m: &Migration) -> Result<(), ArchiveError> {
    let t0 = Instant::now();
    (m.apply)(conn)?;
    let elapsed_ms = t0.elapsed().as_millis() as i64;
    conn.execute_batch(&format!("PRAGMA user_version = {};", m.version))?;
    if db::table_exists(conn, "schema_migrations") {
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at, app_version, duration_ms) \
             VALUES (?, ?, ?, ?, ?)",
            params![m.version, m.name, archive::now_ms(), APP_VERSION, elapsed_ms],
        )?;
    }
    Ok(())
}

#[doc(hidden)]
pub fn create_schema_at(conn: &Connection, version: i64) -> Result<(), ArchiveError> {
    if !(1..=SCHEMA_VERSION).contains(&version) {
        return Err(ArchiveError(format!("schema version must be 1..={SCHEMA_VERSION}")));
    }
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for m in MIGRATIONS
        .iter()
        .filter(|m| m.version > current && m.version <= version)
    {
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        if let Err(e) = apply_migration(conn, m) {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
        conn.execute_batch("COMMIT;")?;
    }
    Ok(())
}

const V1_SQL: &str = r#"
    CREATE TABLE people (
        mri            TEXT PRIMARY KEY,
        name           TEXT,
        email          TEXT,
        job_title      TEXT,
        department     TEXT,
        kind           TEXT,
        raw            TEXT,
        first_seen_at  INTEGER NOT NULL,
        last_seen_at   INTEGER NOT NULL
    );

    CREATE TABLE conversations (
        id               TEXT PRIMARY KEY,
        kind             TEXT NOT NULL,
        name             TEXT,
        topic            TEXT,
        team_id          TEXT,
        members          TEXT,
        last_message_at  INTEGER,
        raw              TEXT,
        first_seen_at    INTEGER NOT NULL,
        last_seen_at     INTEGER NOT NULL
    );

    CREATE TABLE messages (
        pk               INTEGER PRIMARY KEY,
        conversation_id  TEXT NOT NULL,
        message_id       TEXT NOT NULL,
        thread_id        TEXT,
        parent_id        TEXT,
        version          INTEGER NOT NULL DEFAULT 0,
        arrived_at       INTEGER,
        edited_at        INTEGER,
        deleted_at       INTEGER,
        sender_mri       TEXT,
        sender_name      TEXT,
        message_type     TEXT,
        kind             TEXT NOT NULL,
        content_html     TEXT,
        text             TEXT,
        subject          TEXT,
        importance       TEXT,
        reactions        TEXT,
        properties       TEXT,
        raw              TEXT,
        content_hash     TEXT,
        first_seen_at    INTEGER NOT NULL,
        last_seen_at     INTEGER NOT NULL,
        UNIQUE (conversation_id, message_id)
    );
    CREATE INDEX messages_by_conversation ON messages (conversation_id, arrived_at);
    CREATE INDEX messages_by_time ON messages (arrived_at);
    CREATE INDEX messages_by_sender ON messages (sender_mri);
    CREATE INDEX messages_by_thread ON messages (conversation_id, thread_id);

    CREATE TABLE message_revisions (
        id               INTEGER PRIMARY KEY,
        conversation_id  TEXT NOT NULL,
        message_id       TEXT NOT NULL,
        version          INTEGER,
        content_hash     TEXT NOT NULL,
        content_html     TEXT,
        text             TEXT,
        subject          TEXT,
        edited_at        INTEGER,
        captured_at      INTEGER NOT NULL,
        UNIQUE (conversation_id, message_id, content_hash)
    );

    CREATE TABLE attachments (
        id               INTEGER PRIMARY KEY,
        conversation_id  TEXT NOT NULL,
        message_id       TEXT NOT NULL,
        kind             TEXT NOT NULL,
        name             TEXT,
        url              TEXT NOT NULL,
        UNIQUE (conversation_id, message_id, kind, url)
    );
    CREATE INDEX attachments_by_message ON attachments (conversation_id, message_id);

    CREATE TABLE meta (
        key    TEXT PRIMARY KEY,
        value  TEXT
    );

    CREATE TABLE sync_runs (
        id           INTEGER PRIMARY KEY,
        started_at   INTEGER NOT NULL,
        finished_at  INTEGER,
        status       TEXT NOT NULL,
        source       TEXT,
        stats        TEXT,
        message      TEXT
    );

    CREATE VIRTUAL TABLE messages_fts USING fts5 (
        text, subject, sender_name,
        content = 'messages', content_rowid = 'pk',
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts (rowid, text, subject, sender_name)
        VALUES (new.pk, new.text, new.subject, new.sender_name);
    END;
    CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts (messages_fts, rowid, text, subject, sender_name)
        VALUES ('delete', old.pk, old.text, old.subject, old.sender_name);
    END;
    CREATE TRIGGER messages_fts_update AFTER UPDATE OF text, subject, sender_name ON messages BEGIN
        INSERT INTO messages_fts (messages_fts, rowid, text, subject, sender_name)
        VALUES ('delete', old.pk, old.text, old.subject, old.sender_name);
        INSERT INTO messages_fts (rowid, text, subject, sender_name)
        VALUES (new.pk, new.text, new.subject, new.sender_name);
    END;
"#;

const V2_SQL: &str = r#"
    CREATE TABLE record_versions (
        id           INTEGER PRIMARY KEY,
        source       TEXT NOT NULL,
        record_key   TEXT NOT NULL,
        hash         TEXT NOT NULL,
        data         TEXT NOT NULL,
        captured_at  INTEGER NOT NULL,
        UNIQUE (source, record_key, hash)
    );
    CREATE INDEX record_versions_by_key ON record_versions (source, record_key, captured_at);
"#;

const MESSAGE_REVISIONS_FTS_SQL: &str = r#"
    CREATE VIRTUAL TABLE message_revisions_fts USING fts5 (
        text, subject,
        content = 'message_revisions', content_rowid = 'id',
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER message_revisions_fts_insert AFTER INSERT ON message_revisions BEGIN
        INSERT INTO message_revisions_fts (rowid, text, subject)
        VALUES (new.id, new.text, new.subject);
    END;
    CREATE TRIGGER message_revisions_fts_delete AFTER DELETE ON message_revisions BEGIN
        INSERT INTO message_revisions_fts (message_revisions_fts, rowid, text, subject)
        VALUES ('delete', old.id, old.text, old.subject);
    END;
    CREATE TRIGGER message_revisions_fts_update AFTER UPDATE OF text, subject ON message_revisions BEGIN
        INSERT INTO message_revisions_fts (message_revisions_fts, rowid, text, subject)
        VALUES ('delete', old.id, old.text, old.subject);
        INSERT INTO message_revisions_fts (rowid, text, subject)
        VALUES (new.id, new.text, new.subject);
    END;
    INSERT INTO message_revisions_fts (message_revisions_fts) VALUES ('rebuild');
"#;

const V4_SQL: &str = r#"
    CREATE INDEX IF NOT EXISTS messages_by_conv_kind_time ON messages (conversation_id, kind, arrived_at);
    CREATE INDEX IF NOT EXISTS messages_by_mid ON messages (message_id);
    CREATE INDEX IF NOT EXISTS messages_by_edited ON messages (edited_at) WHERE edited_at IS NOT NULL;
    CREATE INDEX IF NOT EXISTS messages_by_deleted ON messages (deleted_at) WHERE deleted_at IS NOT NULL;
    CREATE INDEX IF NOT EXISTS attachments_by_mid ON attachments (message_id);
"#;

const V7_SQL: &str = r#"
    CREATE INDEX IF NOT EXISTS messages_by_sender_name
        ON messages (sender_mri, sender_name)
        WHERE sender_mri IS NOT NULL AND sender_mri != '';
"#;

const V8_SQL: &str = r#"
    CREATE TABLE IF NOT EXISTS schema_migrations (
        version      INTEGER PRIMARY KEY,
        name         TEXT NOT NULL,
        applied_at   INTEGER NOT NULL,
        app_version  TEXT NOT NULL,
        duration_ms  INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS messages_by_kind_last_seen
        ON messages (kind, last_seen_at);
    CREATE INDEX IF NOT EXISTS record_versions_by_source_captured
        ON record_versions (source, captured_at DESC, id DESC);
    CREATE INDEX IF NOT EXISTS attachments_by_kind
        ON attachments (kind, conversation_id, message_id);
"#;

const MIGRATE_V3_BATCH: usize = 500;

fn up_v1(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(V1_SQL)?;
    Ok(())
}

fn up_v2(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(V2_SQL)?;
    Ok(())
}

fn up_v3(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(
        "CREATE TABLE record_versions_v3 (
            id           INTEGER PRIMARY KEY,
            source       TEXT NOT NULL,
            record_key   TEXT NOT NULL,
            hash         TEXT NOT NULL,
            data         BLOB NOT NULL,
            captured_at  INTEGER NOT NULL,
            UNIQUE (source, record_key, hash)
        );",
    )?;
    {
        let mut sel = conn.prepare(
            "SELECT id, source, record_key, hash, data, captured_at FROM record_versions \
             WHERE id > ? ORDER BY id LIMIT ?",
        )?;
        let mut ins = conn.prepare(
            "INSERT OR IGNORE INTO record_versions_v3 (id, source, record_key, hash, data, captured_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )?;
        let mut after = i64::MIN;
        loop {
            let batch = sel
                .query_map(params![after, MIGRATE_V3_BATCH as i64], |r| {
                    let id: i64 = r.get(0)?;
                    let source: String = r.get(1)?;
                    let rkey: String = r.get(2)?;
                    let hash: String = r.get(3)?;
                    let blob = match r.get_ref(4)? {
                        rusqlite::types::ValueRef::Text(t) => compress_bytes(t),
                        rusqlite::types::ValueRef::Blob(b) => b.to_vec(),
                        _ => Vec::new(),
                    };
                    let cap: i64 = r.get(5)?;
                    Ok((id, source, normalize_record_key(&rkey), hash, blob, cap))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let Some(last) = batch.last() else {
                break;
            };
            after = last.0;
            for (id, src, rkey, hash, blob, cap) in batch {
                ins.execute(params![id, src, rkey, hash, blob, cap])?;
            }
        }
    }
    conn.execute_batch(
        "DROP TABLE record_versions;
         ALTER TABLE record_versions_v3 RENAME TO record_versions;
         CREATE INDEX record_versions_by_key ON record_versions (source, record_key, captured_at);",
    )?;
    conn.execute_batch(MESSAGE_REVISIONS_FTS_SQL)?;
    Ok(())
}

fn up_v4(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(V4_SQL)?;
    Ok(())
}

fn up_v5(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS conversation_members (
            conversation_id  TEXT NOT NULL,
            mri              TEXT NOT NULL,
            oid              TEXT NOT NULL,
            PRIMARY KEY (conversation_id, mri)
        );
        CREATE INDEX IF NOT EXISTS conversation_members_by_mri ON conversation_members (mri);
        CREATE INDEX IF NOT EXISTS conversation_members_by_oid ON conversation_members (oid);
        CREATE INDEX IF NOT EXISTS messages_by_kind_time ON messages (kind, arrived_at);
        CREATE INDEX IF NOT EXISTS messages_by_sender_kind ON messages (sender_mri, kind, arrived_at);
        CREATE INDEX IF NOT EXISTS record_versions_by_source_id ON record_versions (source, record_key, id);",
    )?;
    let mut convs = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, members FROM conversations")?;
        let iter = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for row in iter {
            convs.push(row?);
        }
    }
    for (cid, members_raw) in convs {
        let members: Vec<String> = members_raw
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        index_conversation_members(conn, &cid, &members)?;
    }
    Ok(())
}

fn up_v6(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_mentions (
            conversation_id  TEXT NOT NULL,
            message_id       TEXT NOT NULL,
            oid              TEXT NOT NULL,
            PRIMARY KEY (conversation_id, message_id, oid)
        );
        CREATE INDEX IF NOT EXISTS message_mentions_by_oid
            ON message_mentions (oid, conversation_id, message_id);",
    )?;
    let mut mention_rows = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT conversation_id, message_id, properties FROM messages \
             WHERE kind = 'message' AND properties LIKE '%mentions%'",
        )?;
        let iter = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in iter {
            let (cid, mid, props_s) = row?;
            if let Some(s) = props_s {
                if let Ok(v) = serde_json::from_str::<Value>(&s) {
                    for o in extract_mention_oids(&v) {
                        mention_rows.push((cid.clone(), mid.clone(), o));
                    }
                }
            }
        }
    }
    {
        let mut ins =
            conn.prepare("INSERT OR IGNORE INTO message_mentions (conversation_id, message_id, oid) VALUES (?, ?, ?)")?;
        for (cid, mid, o) in mention_rows {
            ins.execute(params![cid, mid, o])?;
        }
    }

    let mut cols = HashSet::new();
    {
        let mut stmt = conn.prepare("PRAGMA table_info(conversations)")?;
        let iter = stmt.query_map([], |r| r.get::<_, String>(1))?;
        for c in iter {
            cols.insert(c?);
        }
    }
    for col in ["read_horizon", "is_read", "last_from_me"] {
        if !cols.contains(col) {
            conn.execute_batch(&format!("ALTER TABLE conversations ADD COLUMN {col} INTEGER;"))?;
        }
    }
    let mut updates = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT id, raw FROM conversations")?;
        let iter = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for row in iter {
            let (cid, raw_s) = row?;
            let raw_v = raw_s
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            let (h, ir, lfm) = extract_conv_read_state(&raw_v);
            updates.push((h, ir, lfm, cid));
        }
    }
    {
        let mut upd =
            conn.prepare("UPDATE conversations SET read_horizon = ?, is_read = ?, last_from_me = ? WHERE id = ?")?;
        for (h, ir, lfm, cid) in updates {
            upd.execute(params![h, ir, lfm, cid])?;
        }
    }
    Ok(())
}

fn up_v7(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(V7_SQL)?;
    Ok(())
}

fn up_v8(conn: &Connection) -> Result<(), ArchiveError> {
    conn.execute_batch(V8_SQL)?;
    let mut ins = conn.prepare(
        "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at, app_version, duration_ms) \
         VALUES (?, ?, 0, 'backfill', 0)",
    )?;
    for m in MIGRATIONS.iter().filter(|m| m.version < 8) {
        ins.execute(params![m.version, m.name])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_registry_is_contiguous_and_unique() {
        assert!(!MIGRATIONS.is_empty());
        assert_eq!(SCHEMA_VERSION, MIGRATIONS.len() as i64);
        let mut names = HashSet::new();
        for (idx, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, (idx + 1) as i64, "version gap at index {idx}");
            assert!(!m.name.is_empty());
            assert!(!m.description.is_empty());
            assert!(names.insert(m.name), "duplicate migration name {}", m.name);
        }
    }
}
