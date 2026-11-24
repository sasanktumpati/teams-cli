mod common;

use common::*;
use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use teams_cli::query::api;
use teams_cli::storage::{archive, backups, migrations};

const V4_INDEXES: &[&str] = &[
    "messages_by_conv_kind_time",
    "messages_by_mid",
    "messages_by_edited",
    "messages_by_deleted",
    "attachments_by_mid",
];
const V5_INDEXES: &[&str] = &[
    "conversation_members_by_mri",
    "conversation_members_by_oid",
    "messages_by_kind_time",
    "messages_by_sender_kind",
    "record_versions_by_source_id",
];
const V6_INDEXES: &[&str] = &["message_mentions_by_oid"];
const V7_INDEXES: &[&str] = &["messages_by_sender_name"];
const V8_INDEXES: &[&str] = &[
    "messages_by_kind_last_seen",
    "record_versions_by_source_captured",
    "attachments_by_kind",
];

fn all_new_indexes() -> BTreeSet<String> {
    [V4_INDEXES, V5_INDEXES, V6_INDEXES, V7_INDEXES, V8_INDEXES]
        .concat()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn indexes(c: &Connection) -> BTreeSet<String> {
    let mut st = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index'")
        .unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

fn user_version(c: &Connection) -> i64 {
    c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

fn schema_shape(c: &Connection) -> BTreeMap<(String, String), Vec<String>> {
    let mut st = c
        .prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
        .unwrap();
    let objs: Vec<(String, String)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    objs.into_iter()
        .map(|(ty, name)| {
            let cols = if ty == "table" {
                let mut ti = c.prepare(&format!("PRAGMA table_info(\"{name}\")")).unwrap();
                ti.query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect()
            } else {
                Vec::new()
            };
            ((ty, name), cols)
        })
        .collect()
}

fn pre_upgrade_backups(home: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(home.join("backups")) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("archive-before-v") && n.ends_with(".sqlite3.xz")
        })
        .collect();
    v.sort();
    v
}

fn unxz_to(src: &Path, dest: &Path) -> Connection {
    let mut out = Vec::new();
    xz2::read::XzDecoder::new(std::fs::File::open(src).unwrap())
        .read_to_end(&mut out)
        .unwrap();
    std::fs::write(dest, out).unwrap();
    Connection::open(dest).unwrap()
}

fn fresh_home() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir, PathBuf) {
    let g = env_lock();
    let td = tempfile::tempdir().unwrap();
    let home = td.path().to_path_buf();
    std::env::set_var("TEAMS_CLI_HOME", &home);
    (g, td, home)
}

fn old_archive(path: &Path, version: i64) {
    let c = Connection::open(path).unwrap();
    archive::create_schema_at(&c, version).unwrap();
    assert_eq!(user_version(&c), version);
    c.execute(
        "INSERT INTO messages (conversation_id, message_id, kind, text, first_seen_at, last_seen_at) \
         VALUES (?, 'm1', 'message', 'current text', 1, 1)",
        [GROUP],
    )
    .unwrap();
}

#[test]
fn fresh_archive_is_current_wal_private_with_all_indexes_and_ledger() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(user_version(&conn), archive::SCHEMA_VERSION);
    assert_eq!(archive::SCHEMA_VERSION, 9);
    assert!(migrations::pending(&conn).unwrap().is_empty());
    let hist = migrations::history(&conn).unwrap();
    let versions: Vec<i64> = hist.iter().map(|m| m.version).collect();
    assert_eq!(versions, (1..=archive::SCHEMA_VERSION).collect::<Vec<_>>());
    let jm: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
    assert_eq!(jm.to_lowercase(), "wal");
    let idxs = indexes(&conn);
    assert!(all_new_indexes().is_subset(&idxs));
    assert!(!idxs.contains("messages_by_sender"));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let plan: Vec<String> = conn
        .prepare("EXPLAIN QUERY PLAN SELECT * FROM messages WHERE message_id = ?")
        .unwrap()
        .query_map(["x"], |r| r.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(plan.join(" ").contains("messages_by_mid"), "{plan:?}");
    assert!(pre_upgrade_backups(&home).is_empty());
}

#[test]
fn every_old_version_upgrades_to_the_fresh_schema_shape() {
    let fresh_shape = {
        let (_g, _td, home) = fresh_home();
        schema_shape(&archive::connect(Some(&home.join("archive.sqlite3"))).unwrap())
    };
    let expected_prefix = format!("archive-before-v{}-", archive::SCHEMA_VERSION);
    for version in 1..archive::SCHEMA_VERSION {
        let (_g, _td, home) = fresh_home();
        let path = home.join("archive.sqlite3");
        old_archive(&path, version);
        let conn = archive::connect(Some(&path)).unwrap();
        assert_eq!(user_version(&conn), archive::SCHEMA_VERSION, "from v{version}");
        assert_eq!(schema_shape(&conn), fresh_shape, "from v{version}");
        let hist = migrations::history(&conn).unwrap();
        assert_eq!(hist.len(), archive::SCHEMA_VERSION as usize, "ledger from v{version}");
        let text: String = conn
            .query_row("SELECT text FROM messages WHERE message_id = 'm1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(text, "current text", "from v{version}");

        let pre = pre_upgrade_backups(&home);
        assert_eq!(pre.len(), 1, "from v{version}: {pre:?}");
        assert!(pre[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(&expected_prefix));
        assert_eq!(std::fs::metadata(&pre[0]).unwrap().permissions().mode() & 0o777, 0o600);
        let old = unxz_to(&pre[0], &home.join("pre.sqlite3"));
        assert_eq!(user_version(&old), version);
        let n: i64 = old
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let name = pre[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(backups::backups_to_keep(std::slice::from_ref(&name), 0, 0, 0).contains(&name));
        assert_eq!(
            backups::verify_archive(Some(&conn), None, true)["ok"],
            true,
            "from v{version}"
        );
        drop(conn);

        drop(archive::connect(Some(&path)).unwrap());
        assert_eq!(pre_upgrade_backups(&home).len(), 1);
        assert!(home.join("archive.sqlite3.migrate.lock").exists());
    }
}

#[test]
fn v2_to_current_reencodes_record_versions_and_indexes_history() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    {
        let old = Connection::open(&path).unwrap();
        archive::create_schema_at(&old, 2).unwrap();
        old.execute(
            "INSERT INTO messages (conversation_id, message_id, kind, text, first_seen_at, last_seen_at) \
             VALUES (?, 'm1', 'message', 'current text', 1, 1)",
            [GROUP],
        )
        .unwrap();
        old.execute(
            "INSERT INTO message_revisions (conversation_id, message_id, content_hash, content_html, text, \
             captured_at) VALUES (?, 'm1', 'h1', '<p>original zebra</p>', 'original zebra', 1)",
            [GROUP],
        )
        .unwrap();
        let rows: Vec<(i64, &str, String, &str, String, i64)> = vec![
            (
                1,
                "message",
                format!("{GROUP}/m1"),
                "a",
                json!({"id": "m1", "content": "x"}).to_string(),
                10,
            ),
            (
                2,
                MENTIONS,
                r#"["<IdbKey 48:mentions>", "<IdbKey 1795413532243.0>"]"#.into(),
                "b",
                json!({"k": "first"}).to_string(),
                20,
            ),
            (
                3,
                MENTIONS,
                "48:mentions/1795413532243".into(),
                "b",
                json!({"k": "dup"}).to_string(),
                30,
            ),
            (
                4,
                "files-mru-manager/files_mru",
                r#""mru_files""#.into(),
                "c",
                json!({"files": [1, 2]}).to_string(),
                40,
            ),
            (
                5,
                "conversation",
                GROUP.into(),
                "d",
                json!({"id": GROUP}).to_string(),
                50,
            ),
        ];
        for r in rows {
            old.execute(
                "INSERT INTO record_versions VALUES (?, ?, ?, ?, ?, ?)",
                params![r.0, r.1, r.2, r.3, r.4, r.5],
            )
            .unwrap();
        }
    }
    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(user_version(&conn), archive::SCHEMA_VERSION);
    let free: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0)).unwrap();
    assert_eq!(free, 0, "VACUUMed after compressing");

    let pre = pre_upgrade_backups(&home);
    assert_eq!(pre.len(), 1);
    let old = unxz_to(&pre[0], &home.join("pre.sqlite3"));
    assert_eq!(user_version(&old), 2);
    let n: i64 = old
        .query_row("SELECT COUNT(*) FROM record_versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 5);

    let mut st = conn
        .prepare("SELECT id, record_key, data, typeof(data) FROM record_versions ORDER BY id")
        .unwrap();
    let got: Vec<(i64, String, serde_json::Value, String)> = st
        .query_map([], |r| {
            let data = archive::decode_record_ref(r.get_ref(2)?).unwrap();
            Ok((r.get(0)?, r.get(1)?, data, r.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(got.iter().all(|g| g.3 == "blob"), "{got:?}");
    let got: Vec<(i64, String, serde_json::Value)> = got.into_iter().map(|g| (g.0, g.1, g.2)).collect();
    assert_eq!(
        got,
        vec![
            (1, format!("{GROUP}/m1"), json!({"id": "m1", "content": "x"})),
            (2, "48:mentions/1795413532243".into(), json!({"k": "first"})),
            (4, "mru_files".into(), json!({"files": [1, 2]})),
            (5, GROUP.into(), json!({"id": GROUP})),
        ]
    );
    let data_type: String = conn
        .query_row(
            "SELECT type FROM pragma_table_info('record_versions') WHERE name = 'data'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(data_type, "BLOB");
    assert!(indexes(&conn).contains("record_versions_by_key"));

    let latest = api::records(&conn, MENTIONS, None, None, 50, false).unwrap();
    let recs = latest["records"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["key"], "48:mentions/1795413532243");
    assert_eq!(recs[0]["data"], json!({"k": "first"}));

    let hits: Vec<i64> = conn
        .prepare("SELECT rowid FROM message_revisions_fts WHERE message_revisions_fts MATCH 'zebra'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(hits, vec![1]);
    assert_eq!(backups::verify_archive(Some(&conn), None, true)["ok"], true);
}

#[test]
fn v3_upgrade_adds_indexes_and_backup_predates_them() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    {
        let c = archive::connect(Some(&path)).unwrap();
        for name in all_new_indexes() {
            c.execute_batch(&format!("DROP INDEX IF EXISTS {name}")).unwrap();
        }
        c.execute_batch(
            "DROP TABLE IF EXISTS conversation_members; DROP TABLE IF EXISTS message_mentions; \
             DROP TABLE IF EXISTS schema_migrations; PRAGMA user_version = 3;",
        )
        .unwrap();
        let snap = snapshot(
            vec![msg(GROUP, "m1", ADA, "<p>hello</p>", t(1.0), "Ada Lovelace")],
            vec![conv(GROUP, "chat", "Launch crew")],
            vec![],
        );
        archive::apply_snapshot(&c, &snap, None).unwrap();
    }
    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(user_version(&conn), archive::SCHEMA_VERSION);
    assert!(all_new_indexes().is_subset(&indexes(&conn)));
    let pre = pre_upgrade_backups(&home);
    assert_eq!(pre.len(), 1);
    let old = unxz_to(&pre[0], &home.join("pre.sqlite3"));
    assert_eq!(user_version(&old), 3);
    assert!(all_new_indexes().is_disjoint(&indexes(&old)));
    assert!(home.join("archive.sqlite3.migrate.lock").exists());
}

#[test]
fn concurrent_openers_take_one_pre_migration_backup() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    old_archive(&path, 3);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let (b, p) = (barrier.clone(), path.clone());
            std::thread::spawn(move || {
                b.wait();
                archive::connect(Some(&p)).map(drop).map_err(|e| e.to_string())
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap().unwrap();
    }
    assert_eq!(pre_upgrade_backups(&home).len(), 1);
    let stray: Vec<_> = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".partial") || n.starts_with('.'))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}

#[test]
fn refuses_archives_from_a_newer_teams_cli() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    drop(archive::connect(Some(&path)).unwrap());
    let future = archive::SCHEMA_VERSION + 1;
    Connection::open(&path)
        .unwrap()
        .execute_batch(&format!("PRAGMA user_version = {future};"))
        .unwrap();
    let err = archive::connect(Some(&path)).unwrap_err().to_string();
    assert!(err.contains("newer than this teams-cli"), "{err}");
    let err = archive::connect_readonly(Some(&path)).unwrap_err().to_string();
    assert!(err.contains("Upgrade teams-cli"), "{err}");
    assert_eq!(user_version(&Connection::open(&path).unwrap()), future);
}

#[test]
fn upgraded_archive_accepts_new_snapshots() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    old_archive(&path, 1);
    let conn = archive::connect(Some(&path)).unwrap();
    let st = archive::apply_snapshot(
        &conn,
        &snapshot(
            vec![msg(GROUP, "m2", ADA, "<p>after upgrade</p>", t(1.0), "Ada Lovelace")],
            vec![conv(GROUP, "chat", "Launch crew")],
            vec![],
        ),
        None,
    )
    .unwrap();
    assert_eq!(st.get("messages_new").copied(), Some(1));
    let hits = api::search(
        &conn,
        Some("upgrade"),
        None,
        None,
        None,
        None,
        30,
        "recent",
        false,
        false,
        false,
    )
    .unwrap();
    assert_eq!(hits["matched"], 1);
}

#[test]
fn verify_detects_missing_schema_migrations_entries() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(backups::verify_archive(Some(&conn), None, false)["ok"], true);
    conn.execute("DELETE FROM schema_migrations WHERE version = 5", [])
        .unwrap();
    let res = backups::verify_archive(Some(&conn), None, false);
    assert_eq!(res["ok"], false, "{res}");
    assert!(
        res["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap_or("").contains("v5")),
        "{res}"
    );
}

#[test]
fn v8_to_v9_clears_raw_drops_sender_index_and_compacts_ephemeral_records() {
    let (_g, _td, home) = fresh_home();
    let path = home.join("archive.sqlite3");
    {
        let c = Connection::open(&path).unwrap();
        archive::create_schema_at(&c, 8).unwrap();
        assert!(indexes(&c).contains("messages_by_sender"));
        c.execute(
            "INSERT INTO people (mri, name, raw, first_seen_at, last_seen_at) VALUES (?, 'Ada', '{\"mri\":\"ada\"}', 1, 1)",
            [ADA],
        )
        .unwrap();
        c.execute(
            "INSERT INTO conversations (id, kind, name, raw, first_seen_at, last_seen_at) VALUES (?, 'chat', 'Launch', '{\"id\":\"g\"}', 1, 1)",
            [GROUP],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages (conversation_id, message_id, kind, text, raw, first_seen_at, last_seen_at) \
             VALUES (?, 'm1', 'message', 'hello', '{\"id\":\"m1\"}', 1, 1)",
            [GROUP],
        )
        .unwrap();
        let insert_rv = |src: &str, key: &str, hash: &str, val: serde_json::Value, cap: i64| {
            c.execute(
                "INSERT INTO record_versions (source, record_key, hash, data, captured_at) VALUES (?, ?, ?, ?, ?)",
                params![src, key, hash, archive::encode_record(&val), cap],
            )
            .unwrap();
        };
        insert_rv("streams-replychain-manager/drafts", "d1", "h1", json!({"v": 1}), 10);
        insert_rv("streams-replychain-manager/drafts", "d1", "h2", json!({"v": 2}), 20);
        insert_rv(
            "consumption-horizon-manager/consumption-horizon-history",
            GROUP,
            "h1",
            json!({"v": 1}),
            10,
        );
        insert_rv(
            "consumption-horizon-manager/consumption-horizon-history",
            GROUP,
            "h2",
            json!({"v": 2}),
            20,
        );
        insert_rv("message", &format!("{GROUP}/m1"), "h1", json!({"v": 1}), 10);
        insert_rv("message", &format!("{GROUP}/m1"), "h2", json!({"v": 2}), 20);
    }

    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(user_version(&conn), 9);
    assert!(!indexes(&conn).contains("messages_by_sender"));
    for tbl in ["messages", "conversations", "people"] {
        let non_null: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {tbl} WHERE raw IS NOT NULL"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(non_null, 0, "{tbl}.raw must be NULL after v9");
    }
    let count_src = |src: &str| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM record_versions WHERE source = ?", [src], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(count_src("streams-replychain-manager/drafts"), 1);
    assert_eq!(count_src("consumption-horizon-manager/consumption-horizon-history"), 1);
    assert_eq!(count_src("message"), 2);
    let latest_draft: serde_json::Value = conn
        .query_row(
            "SELECT data FROM record_versions WHERE source = 'streams-replychain-manager/drafts'",
            [],
            |r| Ok(archive::decode_record_ref(r.get_ref(0)?).unwrap()),
        )
        .unwrap();
    assert_eq!(latest_draft, json!({"v": 2}));
    assert_eq!(backups::verify_archive(Some(&conn), None, true)["ok"], true);
}
