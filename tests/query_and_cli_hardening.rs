mod common;

use common::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use teams_cli::formatters;
use teams_cli::ingest::extract::Attachment;
use teams_cli::query::{api, export, views};
use teams_cli::storage::archive;

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn teams_cmd(home: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_teams"));
    cmd.args(args)
        .env("TEAMS_CLI_HOME", home)
        .env_remove("TEAMS_CLI_ENABLE_SEND")
        .env_remove("TEAMS_CLI_SEND_ALLOWLIST")
        .env("TEAMS_CLI_SOURCE", home.join("no-teams-cache"))
        .env("HOME", home)
        .env("TZ", "UTC");
    cmd
}

fn teams(home: &Path, args: &[&str]) -> Out {
    let o = teams_cmd(home, args).output().unwrap();
    Out {
        code: o.status.code().unwrap_or(-1),
        out: String::from_utf8(o.stdout).unwrap(),
        err: String::from_utf8(o.stderr).unwrap(),
    }
}

fn http_get(addr: SocketAddr, target: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        bearer()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("http response");
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn fixture() -> (std::sync::MutexGuard<'static, ()>, tempfile::TempDir, Connection) {
    let g = env_lock();
    let td = tempfile::tempdir().unwrap();
    let conn = build_archive(td.path());
    (g, td, conn)
}

fn ids(v: &Value, key: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().or(m["chatId"].as_str()).unwrap_or("").to_string())
        .collect()
}

#[test]
fn open_archive_readonly_does_not_create_a_missing_archive() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let err = api::open_archive_readonly().expect_err("must fail without an archive");
    assert!(err.to_string().contains("No archive yet"), "{err}");
    assert!(err.to_string().contains("teams sync"), "{err}");
    assert!(!td.path().join("archive.sqlite3").exists());
}

#[test]
fn open_archive_readonly_opens_an_existing_archive() {
    let (_g, td, _conn) = fixture();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let c = api::open_archive_readonly().unwrap();
    let n: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0)).unwrap();
    assert!(n > 0);
}

#[test]
fn cli_read_commands_without_an_archive_error_cleanly_and_create_nothing() {
    let td = tempfile::tempdir().unwrap();
    for argv in [
        vec!["status"],
        vec!["status", "--json"],
        vec!["chats"],
        vec!["search", "hello"],
        vec!["inbox"],
    ] {
        let o = teams(td.path(), &argv);
        assert_ne!(o.code, 0, "{argv:?}");
        assert!(o.err.starts_with("teams: "), "{argv:?}: {}", o.err);
        assert!(o.err.contains("No archive yet"), "{argv:?}: {}", o.err);
        assert!(!o.err.contains("panicked"), "{argv:?}: {}", o.err);
        assert!(
            !td.path().join("archive.sqlite3").exists(),
            "{argv:?} created an empty archive"
        );
    }
}

#[test]
fn daemon_without_an_archive_answers_json_errors_and_creates_nothing() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let h = teams_cli::daemon::start(0, None, false).unwrap();
    for path in ["/v1/status", "/v1/chats", "/v1/inbox"] {
        let (status, body) = http_get(h.addr(), path);
        assert!(status >= 400, "{path}: {status} {body}");
        assert!(body["error"]["code"].is_string(), "{path}: {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("No archive yet"),
            "{path}: {body}"
        );
    }
    h.shutdown();
    assert!(!td.path().join("archive.sqlite3").exists());
}

#[test]
fn as_int_rejects_negative_numbers() {
    let e = api::as_int(Some("-1"), 50, "limit").unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("bad_param", 400));
    assert!(e.message.contains("limit"), "{}", e.message);
    assert!(api::as_int(Some("-100"), 50, "context").is_err());
    assert_eq!(api::as_int(Some("0"), 50, "limit").unwrap(), 0);
    assert_eq!(api::as_int(Some("7"), 50, "limit").unwrap(), 7);
    assert_eq!(api::as_int(None, 50, "limit").unwrap(), 50);
    assert_eq!(api::as_int(Some(""), 50, "limit").unwrap(), 50);
    assert_eq!(api::as_int(Some("x"), 50, "limit").unwrap_err().code, "bad_param");
}

#[test]
fn list_chats_validates_kind() {
    let (_g, _td, conn) = fixture();
    let e = api::list_chats(&conn, None, Some("bogus"), 0, false, None).unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("bad_param", 400));
    for k in api::CHAT_KINDS {
        api::list_chats(&conn, None, Some(k), 0, false, None).unwrap_or_else(|e| panic!("{k}: {e}"));
    }
    let r = api::list_chats(&conn, None, Some("chat"), 0, false, None).unwrap();
    assert!(r["total"].as_u64().unwrap() >= 2);
    api::list_chats(&conn, None, Some(""), 0, false, None).unwrap();
}

#[test]
fn search_validates_sort() {
    let (_g, _td, conn) = fixture();
    let e = api::search(
        &conn,
        Some("group"),
        None,
        None,
        None,
        None,
        10,
        "bogus",
        false,
        false,
        false,
    )
    .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("bad_param", 400));
    for sort in ["recent", "relevance"] {
        let r = api::search(
            &conn,
            Some("group"),
            None,
            None,
            None,
            None,
            10,
            sort,
            false,
            false,
            false,
        )
        .unwrap();
        assert!(r["matched"].as_i64().unwrap() > 0, "{sort}");
    }
}

#[test]
fn http_contract_rejects_bad_kind_sort_and_negative_limits() {
    let _g = env_lock();
    let (_td, _home) = home_with_archive();
    let h = teams_cli::daemon::start(0, None, false).unwrap();
    let cases = [
        "/v1/chats?kind=bogus",
        "/v1/search?query=group&sort=bogus",
        "/v1/chats?limit=-1",
        "/v1/search?query=group&limit=-1",
        "/v1/messages?chat=Launch&limit=-5",
    ];
    for path in cases {
        let (status, body) = http_get(h.addr(), path);
        assert_eq!(status, 400, "{path}: {body}");
        assert_eq!(body["error"]["code"], "bad_param", "{path}: {body}");
    }
    for path in [
        "/v1/chats?kind=system",
        "/v1/chats?kind=chat",
        "/v1/search?query=group&sort=relevance",
        "/v1/chats?limit=0",
    ] {
        let (status, body) = http_get(h.addr(), path);
        assert_eq!(status, 200, "{path}: {body}");
    }
    h.shutdown();
}

#[test]
fn fts_syntax_errors_are_recognised_and_others_are_not() {
    let (_g, _td, conn) = fixture();
    let e = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'AND'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_err();
    assert!(api::is_fts_syntax_error(&e), "{e}");
    let e = conn
        .query_row(
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'nosuchcol : x'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_err();
    assert!(api::is_fts_syntax_error(&e), "{e}");
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("database is locked".into()),
    );
    assert!(!api::is_fts_syntax_error(&busy));
    let corrupt = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        Some("database disk image is malformed".into()),
    );
    assert!(!api::is_fts_syntax_error(&corrupt));
}

#[test]
fn search_surfaces_real_sqlite_errors_as_500() {
    let (_g, _td, conn) = fixture();
    conn.execute_batch("DROP TABLE messages_fts").unwrap();
    let e = api::search(
        &conn,
        Some("group"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        false,
        false,
    )
    .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("internal", 500), "{}", e.message);
}

#[test]
fn search_with_history_surfaces_real_sqlite_errors_as_500() {
    let (_g, _td, conn) = fixture();
    conn.execute_batch("DROP TABLE message_revisions_fts").unwrap();
    let e = api::search(
        &conn,
        Some("budget"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        true,
        false,
    )
    .unwrap_err();
    assert_eq!((e.code.as_str(), e.status), ("internal", 500), "{}", e.message);
}

#[test]
fn search_with_history_snippets_and_history_for_winners() {
    let (_g, _td, conn) = fixture();
    let r = api::search(
        &conn,
        Some("budget"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        true,
        false,
    )
    .unwrap();
    assert_eq!(r["matched"], 1, "{r}");
    let hit = &r["results"][0];
    assert_eq!(hit["id"], EDITED);
    assert_eq!(hit["matchedInHistory"], true);
    assert!(hit["snippet"].as_str().unwrap().contains("«budget»"), "{hit}");
    let hist = hit["history"].as_array().unwrap();
    assert!(
        hist.iter()
            .any(|h| h["text"].as_str().unwrap().contains("budget draft v1")),
        "{hit}"
    );
    assert!(hit["attachments"].is_array());

    let r = api::search(
        &conn,
        Some("numbers"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        true,
        false,
    )
    .unwrap();
    let hit = &r["results"][0];
    assert_eq!(hit["id"], EDITED);
    assert_eq!(hit["matchedInHistory"], false);
    assert!(hit["snippet"].as_str().unwrap().contains("«numbers»"), "{hit}");
    assert!(hit.get("history").is_none(), "{hit}");

    let r = api::search(
        &conn,
        Some("group"),
        None,
        None,
        None,
        None,
        2,
        "relevance",
        false,
        true,
        false,
    )
    .unwrap();
    let plain = api::search(
        &conn,
        Some("group"),
        None,
        None,
        None,
        None,
        2,
        "relevance",
        false,
        false,
        false,
    )
    .unwrap();
    assert_eq!(r["results"].as_array().unwrap().len(), 2);
    assert_eq!(r["matched"], plain["matched"]);
    assert_eq!(r["truncated"], true);
    for h in r["results"].as_array().unwrap() {
        assert!(h["snippet"].as_str().unwrap().contains("«group»"), "{h}");
    }
    let r = api::search(
        &conn,
        Some("group"),
        None,
        None,
        None,
        None,
        2,
        "recent",
        false,
        true,
        true,
    )
    .unwrap();
    assert_eq!(r["countOnly"], true);
    assert_eq!(r["matched"], plain["matched"]);
}

#[test]
fn views_propagate_record_versions_errors() {
    let (_g, _td, conn) = fixture();
    conn.execute_batch("DROP TABLE record_versions").unwrap();
    let results: Vec<(&str, Result<Value, api::ApiError>)> = vec![
        ("calls", views::calls(&conn, None, None, None, false, 0)),
        ("meetings", views::meetings(&conn, None, None, None, 0)),
        ("saved", views::saved(&conn, true, 0)),
        ("pinned", views::pinned(&conn, None, 0)),
        ("mentions", views::mentions(&conn, None, None, None, false, 0)),
        ("inbox", views::inbox(&conn, None, 0)),
    ];
    for (name, r) in results {
        let e = r.err().unwrap_or_else(|| panic!("{name} swallowed a SQLite error"));
        assert_eq!((e.code.as_str(), e.status), ("internal", 500), "{name}: {}", e.message);
    }
}

#[test]
fn views_still_skip_undecodable_payloads() {
    let (_g, _td, conn) = fixture();
    conn.execute(
        "INSERT INTO record_versions (source, record_key, hash, data, captured_at) VALUES (?, 'bad', 'x', X'00FF10', 1)",
        [CALLS],
    )
    .unwrap();
    let r = views::calls(&conn, None, None, None, false, 0).unwrap();
    assert_eq!(r["matched"], 2, "{r}");
}

#[test]
fn people_includes_null_kind_and_excludes_bots() {
    let (_g, _td, conn) = fixture();
    conn.execute_batch(
        "INSERT INTO people (mri, name, kind, first_seen_at, last_seen_at) \
            VALUES ('8:orgid:dddddddd-0000-0000-0000-000000000004', 'Nully Null', NULL, 1, 1); \
         INSERT INTO people (mri, name, kind, first_seen_at, last_seen_at) \
            VALUES ('28:bot-1', 'Helpful Bot', 'bot', 1, 1);",
    )
    .unwrap();
    let r = api::people(&conn, None, 0, false).unwrap();
    let names: Vec<&str> = r["people"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(names.contains(&"Nully Null"), "{names:?}");
    assert!(names.contains(&"Ada Lovelace"), "{names:?}");
    assert!(!names.contains(&"Helpful Bot"), "{names:?}");
    let r = api::people(&conn, None, 0, true).unwrap();
    let names: Vec<&str> = r["people"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    assert!(
        names.contains(&"Helpful Bot") && names.contains(&"Nully Null"),
        "{names:?}"
    );
}

#[test]
fn changes_all_time_ignores_zero_sentinels() {
    let (_g, _td, conn) = fixture();
    conn.execute(
        "UPDATE messages SET edited_at = 0, deleted_at = 0 WHERE message_id = '1795000000001'",
        [],
    )
    .unwrap();
    for what in ["all", "edited", "deleted"] {
        let r = api::changes(&conn, Some("all"), None, None, what, 0).unwrap();
        let got = ids(&r, "changes");
        assert!(!got.contains(&"1795000000001".to_string()), "{what}: {got:?}");
        if what != "deleted" {
            assert!(got.contains(&EDITED.to_string()), "{what}: {got:?}");
        }
    }
    let r = api::changes(&conn, None, None, None, "all", 0).unwrap();
    assert!(!ids(&r, "changes").contains(&"1795000000001".to_string()));
}

#[test]
fn resolve_chat_trims_before_exact_id_lookup() {
    let (_g, _td, conn) = fixture();
    let (row, others) = api::resolve_chat(&conn, &format!("  {GROUP}\t")).unwrap();
    assert_eq!(row.id, GROUP);
    assert!(others.is_empty());
    let (row, _) = api::resolve_chat(&conn, "  Launch crew  ").unwrap();
    assert_eq!(row.id, GROUP);
    assert_eq!(api::resolve_chat(&conn, "   ").unwrap_err().code, "bad_param");
}

const ME_VARIANT: &str = "8:aaaaaaaa-0000-0000-0000-000000000001";
const VARIANT_CHAT: &str = "19:variant@thread.v2";

#[test]
fn inbox_counts_every_mri_variant_of_me_as_me() {
    let (_g, _td, conn) = fixture();
    archive::apply_snapshot(
        &conn,
        &snapshot(
            vec![
                msg(
                    VARIANT_CHAT,
                    "1795000000500",
                    ADA,
                    "<p>question for you</p>",
                    t(3.0),
                    "Ada Lovelace",
                ),
                msg(
                    VARIANT_CHAT,
                    "1795000000501",
                    ME_VARIANT,
                    "<p>answered from my other id</p>",
                    t(2.0),
                    "Me Myself",
                ),
            ],
            vec![conv(VARIANT_CHAT, "chat", "Variant chat")],
            vec![],
        ),
        None,
    )
    .unwrap();
    conn.execute(
        "UPDATE conversations SET read_horizon = ?, is_read = NULL, last_from_me = NULL WHERE id = ?",
        rusqlite::params![t(4.0), VARIANT_CHAT],
    )
    .unwrap();
    let r = views::inbox(&conn, Some("all"), 0).unwrap();
    let unread: Vec<String> = r["unreadChats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["chatId"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !unread.contains(&VARIANT_CHAT.to_string()),
        "my reply (other MRI) reads the chat: {unread:?}"
    );
    let awaiting: Vec<String> = r["awaitingReply"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["chatId"].as_str().unwrap().to_string())
        .collect();
    assert!(!awaiting.contains(&VARIANT_CHAT.to_string()), "{awaiting:?}");
    assert!(r["counts"]["mentions"].as_u64().unwrap() >= 1, "{}", r["counts"]);
    assert!(
        r["counts"]["unreadActivities"].as_u64().unwrap() >= 1,
        "{}",
        r["counts"]
    );
    assert_eq!(r["mentions"][0]["isRead"], false);
    assert_eq!(r["unreadActivities"][0]["id"], "a1");
}

#[test]
fn calls_clamp_negative_durations_to_zero() {
    let (_g, _td, conn) = fixture();
    archive::apply_snapshot(
        &conn,
        &snapshot(
            vec![],
            vec![],
            vec![call_record(
                "c-neg",
                t(4.0),
                Call {
                    dur: Some(-5000),
                    ..Default::default()
                },
            )],
        ),
        None,
    )
    .unwrap();
    let r = views::calls(&conn, None, None, None, false, 0).unwrap();
    let c = r["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "c-neg")
        .cloned()
        .expect("c-neg present");
    assert_eq!(c["durationMs"], 0, "{c}");
    assert_eq!(c["durationSeconds"], 0, "{c}");
}

const CHANNEL: &str = "19:chan@thread.tacv2";

fn channel_archive(conn: &Connection) {
    let mut root = msg(
        CHANNEL,
        "1000",
        ADA,
        "<p>root with empty thread id</p>",
        t(5.0),
        "Ada Lovelace",
    );
    root.attachments = vec![Attachment {
        kind: "file".into(),
        name: "a b (1).pdf".into(),
        url: "https://files.test/a b (1).pdf".into(),
    }];
    let mut reply = msg(CHANNEL, "1001", BOB, "<p>reply to the root</p>", t(4.0), "Bob Builder");
    reply.thread_id = Some("1000".into());
    reply.parent_id = Some("1000".into());
    let tie = t(3.0);
    let late_id = msg(CHANNEL, "2000", ADA, "<p>tie two thousand</p>", tie, "Ada Lovelace");
    let early_id = msg(
        CHANNEL,
        "1999",
        ADA,
        "<p>tie nineteen ninety-nine</p>",
        tie,
        "Ada Lovelace",
    );
    archive::apply_snapshot(
        conn,
        &snapshot(
            vec![root, reply, late_id, early_id],
            vec![conv(CHANNEL, "channel", "General")],
            vec![],
        ),
        None,
    )
    .unwrap();
    conn.execute("UPDATE messages SET thread_id = '' WHERE message_id = '1000'", [])
        .unwrap();
}

#[test]
fn export_handles_empty_thread_ids_ties_and_unsafe_urls() {
    let (_g, _td, conn) = fixture();
    channel_archive(&conn);
    let md = export::export_markdown(&conn, CHANNEL, Some("all"), None, None, false, false, None).unwrap();
    assert!(md.contains("reply to the root"), "reply lost:\n{md}");
    assert!(!md.contains("Replies to an earlier post"), "{md}");
    let root_at = md.find("root with empty thread id").unwrap();
    let reply_at = md.find("reply to the root").unwrap();
    assert!(root_at < reply_at, "{md}");
    assert!(md.contains("- **"), "reply should be nested as an item:\n{md}");
    assert!(md.contains("- [a b (1).pdf](<https://files.test/a b (1).pdf>)"), "{md}");
    let a = md.find("tie nineteen ninety-nine").unwrap();
    let b = md.find("tie two thousand").unwrap();
    assert!(a < b, "equal timeMs must break ties by id:\n{md}");
    let again = export::export_markdown(&conn, CHANNEL, Some("all"), None, None, false, false, None).unwrap();
    assert_eq!(md, again);
}

#[test]
fn md_link_dest_wraps_or_encodes() {
    assert_eq!(export::md_link_dest("https://x.test/a"), "https://x.test/a");
    assert_eq!(export::md_link_dest("https://x.test/a b"), "<https://x.test/a b>");
    assert_eq!(export::md_link_dest("https://x.test/a)b"), "<https://x.test/a)b>");
    assert_eq!(export::md_link_dest("https://x.test/a(b)"), "<https://x.test/a(b)>");
    assert_eq!(
        export::md_link_dest("https://x.test/<a> b)"),
        "https://x.test/%3Ca%3E%20b%29"
    );
}

#[test]
fn fmt_message_edited_at_is_char_safe() {
    for edited_at in [
        "２０２６-11-24T10:00:00",
        "2026-11-24T10:0é",
        "é",
        "2026-11-24T10:00:00+00:00",
    ] {
        let m = json!({"kind": "message", "timeMs": 1, "text": "hi", "sender": "Ada",
                       "edited": true, "editedAt": edited_at});
        let s = formatters::fmt_message(&m, "", None);
        assert!(s.contains("edited"), "{edited_at}: {s}");
    }
    let m = json!({"kind": "message", "timeMs": 1, "text": "hi", "sender": "Ada",
                   "edited": true, "editedAt": "2026-11-24T10:05:00+00:00"});
    assert!(formatters::fmt_message(&m, "", None).contains("edited 11-24 10:05"));
}

#[test]
fn format_threaded_breaks_equal_time_ties_by_id() {
    let mk = |id: &str, text: &str| {
        json!({"id": id, "threadId": id, "isReply": false, "kind": "message",
                                           "timeMs": 1000, "text": text, "sender": "Ada"})
    };
    let a = mk("100", "first by id");
    let b = mk("200", "second by id");
    let one = formatters::format_threaded(&[b.clone(), a.clone()]);
    let two = formatters::format_threaded(&[a, b]);
    assert_eq!(one, two);
    assert!(
        one.find("first by id").unwrap() < one.find("second by id").unwrap(),
        "{one}"
    );
}

#[test]
fn reindex_waits_for_the_sync_lock() {
    let _g = env_lock();
    let (_td, h) = home_with_archive();
    std::env::set_var("TEAMS_CLI_HOME", &h);
    let lock = teams_cli::ingest::sync::acquire_sync_lock(1.0).unwrap();
    let mut child = teams_cmd(&h, &["reindex"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        child.try_wait().unwrap().is_none(),
        "reindex ran while a sync held the lock"
    );
    drop(lock);
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("Re-rendered"));
}

#[test]
fn restore_yes_restores_an_xz_backup_and_rejects_bad_ones() {
    let (td, h) = {
        let _g = env_lock();
        home_with_archive()
    };
    let dest = td.path().join("snap.sqlite3.xz");
    let o = teams(&h, &["backup", "--out", dest.to_str().unwrap()]);
    assert_eq!(o.code, 0, "{}", o.err);
    let o = teams(&h, &["restore", dest.to_str().unwrap(), "--yes", "--no-safety-backup"]);
    assert_eq!(o.code, 0, "{} / {}", o.out, o.err);
    assert!(o.out.contains("Restored"), "{}", o.out);
    let o = teams(&h, &["chats"]);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.contains("Launch crew"));

    let missing = td.path().join("nope.sqlite3.xz");
    let o = teams(&h, &["restore", missing.to_str().unwrap(), "--yes"]);
    assert_eq!(o.code, 1);
    assert!(o.err.contains("failed verification"), "{}", o.err);

    let wrong = td.path().join("notes.txt");
    std::fs::write(&wrong, b"hello").unwrap();
    let o = teams(&h, &["restore", wrong.to_str().unwrap(), "--yes"]);
    assert_eq!(o.code, 1);
    assert!(o.err.contains("failed verification"), "{}", o.err);

    let bad = td.path().join("bad.sqlite3.xz");
    std::fs::write(&bad, b"garbage".repeat(50)).unwrap();
    let o = teams(&h, &["restore", bad.to_str().unwrap(), "--yes"]);
    assert_eq!(o.code, 1);
    assert!(o.err.contains("failed verification"), "{}", o.err);
    assert_eq!(teams(&h, &["chats"]).code, 0);
}

#[test]
fn agent_interval_label_handles_sub_minute_intervals() {
    assert_eq!(teams_cli::cli::interval_label(30), "30s");
    assert_eq!(teams_cli::cli::interval_label(0), "0s");
    assert_eq!(teams_cli::cli::interval_label(60), "1 min");
    assert_eq!(teams_cli::cli::interval_label(900), "15 min");
    assert_eq!(teams_cli::cli::interval_label(90), "90s");
}

#[test]
fn executor_app_contract() {
    let src = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("executor-app/index.ts")).unwrap();
    assert!(src.contains("TEAMS_CLI_PORT"), "BASE port must be configurable");
    assert!(src.contains("47613"), "default port kept");
    assert!(src.contains(r#"k !== "since""#), "empty since must be forwarded");
    assert!(src.contains("if (v === undefined) continue;"));
    let chats_tool = src.find("chats: query(").unwrap();
    let kind_line = &src[chats_tool..chats_tool + 600];
    assert!(
        kind_line.contains("chat | channel | meeting | team | community | system"),
        "{kind_line}"
    );
}

#[test]
fn status_reports_backup_warnings_as_problems() {
    let (_g, _td, conn) = fixture();
    let before = teams_cli::query::api::status(&conn).unwrap();
    assert!(!before["problems"].to_string().contains("backup warning"), "{before}");
    teams_cli::storage::archive::set_meta(&conn, "backup_warning", "skipped compressing legacy backup x: boom")
        .unwrap();
    let after = teams_cli::query::api::status(&conn).unwrap();
    let problems: Vec<&str> = after["problems"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert!(
        problems.contains(&"Last backup warning: skipped compressing legacy backup x: boom"),
        "{problems:?}"
    );
    assert_eq!(after["healthy"], false);
    teams_cli::storage::archive::set_meta(&conn, "backup_warning", "").unwrap();
    let cleared = teams_cli::query::api::status(&conn).unwrap();
    assert!(!cleared["problems"].to_string().contains("backup warning"));
}

#[test]
fn search_exclusion_subject_weighting_snippets_and_has_attachments() {
    let (_g, _td, conn) = fixture();
    let mut m_subj = msg(GROUP, "9001", ADA, "<p>ordinary text here</p>", t(0.5), "Ada Lovelace");
    m_subj.subject = "Quarterly Orion Plan".to_string();
    let mut m_body = msg(
        GROUP,
        "9002",
        ADA,
        "<p>mentioning orion in body</p>",
        t(0.4),
        "Ada Lovelace",
    );
    m_body.attachments.push(teams_cli::ingest::extract::Attachment {
        kind: "file".into(),
        name: "orion.pdf".into(),
        url: "https://example.sharepoint.com/orion.pdf".into(),
    });
    teams_cli::storage::archive::apply_snapshot(&conn, &snapshot(vec![m_subj, m_body], vec![], vec![]), None).unwrap();

    let rel = teams_cli::query::api::search_with_options(
        &conn,
        Some("orion"),
        None,
        None,
        None,
        None,
        10,
        "relevance",
        false,
        false,
        false,
        false,
    )
    .unwrap();
    let results = rel["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["id"], "9001");
    assert!(
        results[0]["snippet"].as_str().unwrap_or("").contains("«Orion»"),
        "{results:?}"
    );

    let m_or = msg(GROUP, "9003", ADA, "<p>alpha cherry note</p>", t(0.3), "Ada Lovelace");
    let m_or2 = msg(GROUP, "9004", ADA, "<p>beta clean note</p>", t(0.2), "Ada Lovelace");
    teams_cli::storage::archive::apply_snapshot(&conn, &snapshot(vec![m_or, m_or2], vec![], vec![]), None).unwrap();
    let or_excl = teams_cli::query::api::search_with_options(
        &conn,
        Some("alpha OR beta -cherry"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        false,
        false,
        false,
    )
    .unwrap();
    assert_eq!(or_excl["matched"], 1);
    assert_eq!(or_excl["results"][0]["id"], "9004");

    let excl = teams_cli::query::api::search_with_options(
        &conn,
        Some("orion -body"),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        false,
        false,
        false,
    )
    .unwrap();
    assert_eq!(excl["matched"], 1);
    assert_eq!(excl["results"][0]["id"], "9001");
    let bad = teams_cli::query::api::fts_query("-onlynegated", true).unwrap_err();
    assert_eq!(bad.code, "bad_query");

    let att_only = teams_cli::query::api::search_with_options(
        &conn,
        Some(""),
        None,
        None,
        None,
        None,
        10,
        "recent",
        false,
        false,
        false,
        true,
    )
    .unwrap();
    assert_eq!(att_only["matched"], 1);
    assert_eq!(att_only["results"][0]["id"], "9002");
}
