mod common;

use common::env_lock;
use rusqlite::Connection;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::time::Duration;
use teams_cli::ingest::{extract, indexeddb::TeamsCache, sync, v8};
use teams_cli::live::{cookies as auth, fetch};
use teams_cli::paths;
use teams_cli::query::{api, views};
use teams_cli::storage::{archive, backups};
use teams_cli::timeutil;

fn push_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn push_utf16be(out: &mut Vec<u8>, s: &str) {
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_be_bytes());
    }
}

fn idb_key_str(s: &str) -> Vec<u8> {
    let mut out = vec![1u8];
    push_varint(&mut out, s.encode_utf16().count() as u64);
    push_utf16be(&mut out, s);
    out
}

fn idb_prefix(db_id: u64, store_id: u64, index_id: u64) -> Vec<u8> {
    vec![0x00, db_id as u8, store_id as u8, index_id as u8]
}

fn v8_str(out: &mut Vec<u8>, s: &str) {
    if s.is_ascii() {
        out.push(b'"');
        push_varint(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    } else {
        out.push(b'S');
        push_varint(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
}

fn v8_val(out: &mut Vec<u8>, v: &serde_json::Value) {
    match v {
        serde_json::Value::Null => out.push(b'0'),
        serde_json::Value::Bool(true) => out.push(b'T'),
        serde_json::Value::Bool(false) => out.push(b'F'),
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                out.push(b'N');
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        serde_json::Value::String(s) => v8_str(out, s),
        serde_json::Value::Array(arr) => {
            out.push(b'A');
            push_varint(out, arr.len() as u64);
            for item in arr {
                v8_val(out, item);
            }
            out.push(b'$');
            push_varint(out, 0);
            push_varint(out, arr.len() as u64);
        }
        serde_json::Value::Object(map) => {
            out.push(b'o');
            for (k, val) in map {
                v8_str(out, k);
                v8_val(out, val);
            }
            out.push(b'{');
            push_varint(out, map.len() as u64);
        }
    }
}

fn encode_idb_value(v: &serde_json::Value, snappy: bool) -> Vec<u8> {
    let mut inner = vec![0xFF, 20];
    inner.extend_from_slice(&[0xFF, 15]);
    v8_val(&mut inner, v);

    let payload = if snappy {
        let compressed = snap::raw::Encoder::new().compress_vec(&inner).unwrap();
        let mut wrapped = vec![0xFF, 0x11, 0x02];
        wrapped.extend_from_slice(&compressed);
        wrapped
    } else {
        inner
    };

    let mut out = Vec::new();
    push_varint(&mut out, 1);
    out.extend_from_slice(&payload);
    out
}

fn write_leveldb_log(path: &Path, entries: &[(Vec<u8>, Vec<u8>)]) {
    let mut batch = Vec::new();
    batch.extend_from_slice(&1u64.to_le_bytes());
    batch.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (k, v) in entries {
        batch.push(1u8);
        push_varint(&mut batch, k.len() as u64);
        batch.extend_from_slice(k);
        push_varint(&mut batch, v.len() as u64);
        batch.extend_from_slice(v);
    }
    let mut log = Vec::new();
    assert!(batch.len() + 7 <= 32_768);
    log.extend_from_slice(&0u32.to_le_bytes());
    log.extend_from_slice(&(batch.len() as u16).to_le_bytes());
    log.push(1u8);
    log.extend_from_slice(&batch);
    fs::write(path, log).unwrap();
}

fn db_meta_entry(origin: &str, db_name: &str, db_id: u64) -> (Vec<u8>, Vec<u8>) {
    let mut k = vec![0, 0, 0, 0, 0xc9];
    push_varint(&mut k, origin.encode_utf16().count() as u64);
    push_utf16be(&mut k, origin);
    push_varint(&mut k, db_name.encode_utf16().count() as u64);
    push_utf16be(&mut k, db_name);
    (k, vec![db_id as u8])
}

fn store_name_entry(db_id: u64, store_id: u64, name: &str) -> (Vec<u8>, Vec<u8>) {
    let mut k = idb_prefix(db_id, 0, 0);
    k.push(50);
    push_varint(&mut k, store_id);
    k.push(0);
    let mut v = Vec::new();
    push_utf16be(&mut v, name);
    (k, v)
}

#[test]
fn synthetic_leveldb_indexeddb_snappy_and_blob_end_to_end_sync() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    let home = td.path().join("home");
    let src = td.path().join("source");
    let ldb = src.join(format!("{}.leveldb", paths::ORIGIN));
    let blob_dir = src.join(format!("{}.blob", paths::ORIGIN));
    fs::create_dir_all(&ldb).unwrap();
    fs::create_dir_all(blob_dir.join("2/00")).unwrap();
    std::env::set_var("TEAMS_CLI_HOME", &home);

    let me_oid = "aaaaaaaa-0000-0000-0000-000000000001";
    let ada_oid = "bbbbbbbb-0000-0000-0000-000000000002";
    let me = format!("8:orgid:{me_oid}");
    let ada = format!("8:orgid:{ada_oid}");
    let conv_id = "19:synth_chat@thread.v2";

    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    entries.push(db_meta_entry(
        "https_teams.microsoft.com_0",
        &format!("Teams:profiles:react-web-client:{me}:en-us"),
        1,
    ));
    entries.push(db_meta_entry(
        "https_teams.microsoft.com_0",
        &format!("Teams:replychain-manager:react-web-client:{me}:en-us"),
        2,
    ));
    entries.push(db_meta_entry(
        "https_teams.microsoft.com_0",
        &format!("Teams:conversation-manager:react-web-client:{me}:en-us"),
        3,
    ));
    entries.push(store_name_entry(1, 1, "profiles"));
    entries.push(store_name_entry(2, 1, "replychains-2"));
    entries.push(store_name_entry(3, 1, "conversations"));

    let mut pk = idb_prefix(1, 1, 1);
    pk.extend_from_slice(&idb_key_str(&ada));
    entries.push((
        pk,
        encode_idb_value(
            &json!({
                "mri": ada,
                "displayName": "Ada Lovelace",
                "email": "ada@synth.test",
                "jobTitle": "Architect",
                "type": "person"
            }),
            false,
        ),
    ));

    let now = archive::now_ms();
    let mut rk1 = idb_prefix(2, 1, 1);
    rk1.extend_from_slice(&idb_key_str("rc1"));
    entries.push((
        rk1,
        encode_idb_value(
            &json!({
                "conversationId": conv_id,
                "replyChainId": "1795000000001",
                "messageMap": {
                    "1795000000001": {
                        "id": "1795000000001",
                        "conversationId": conv_id,
                        "version": 1.0,
                        "originalArrivalTime": (now - 10_000) as f64,
                        "creator": ada,
                        "imDisplayName": "Ada Lovelace",
                        "messageType": "RichText/Html",
                        "content": "<p>Snappy hello café 🚀</p>",
                        "properties": {}
                    }
                }
            }),
            true,
        ),
    ));

    let raw_key2 = idb_key_str("rc2");
    let mut blob_meta_key = idb_prefix(2, 1, 3);
    blob_meta_key.extend_from_slice(&raw_key2);
    let mut blob_meta_val = Vec::new();
    blob_meta_val.push(0u8);
    push_varint(&mut blob_meta_val, 7);
    push_varint(&mut blob_meta_val, 0);
    push_varint(&mut blob_meta_val, 100);
    entries.push((blob_meta_key, blob_meta_val));

    let mut blob_inner = vec![0xFF, 20, 0xFF, 15];
    v8_val(
        &mut blob_inner,
        &json!({
            "conversationId": conv_id,
            "replyChainId": "1795000000002",
            "messageMap": {
                "1795000000002": {
                    "id": "1795000000002",
                    "conversationId": conv_id,
                    "version": 1.0,
                    "originalArrivalTime": (now - 5_000) as f64,
                    "creator": ada,
                    "imDisplayName": "Ada Lovelace",
                    "messageType": "RichText/Html",
                    "content": "<p>External blob message</p>",
                    "properties": {}
                }
            }
        }),
    );
    fs::write(blob_dir.join("2/00/7"), &blob_inner).unwrap();

    let mut rk2 = idb_prefix(2, 1, 1);
    rk2.extend_from_slice(&raw_key2);
    let mut rk2_val = Vec::new();
    push_varint(&mut rk2_val, 1);
    rk2_val.extend_from_slice(&[0xFF, 0x11, 0x01]);
    push_varint(&mut rk2_val, blob_inner.len() as u64);
    push_varint(&mut rk2_val, 0);
    entries.push((rk2, rk2_val));

    let mut ck = idb_prefix(3, 1, 1);
    ck.extend_from_slice(&idb_key_str(conv_id));
    entries.push((
        ck,
        encode_idb_value(
            &json!({
                "id": conv_id,
                "type": "Chat",
                "version": 1.0,
                "threadProperties": {"topic": "Synthetic Sync Crew", "isRead": false, "isLastMessageFromMe": false},
                "members": [{"id": me}, {"id": ada}],
                "lastMessageTimeUtc": (now - 5_000) as f64
            }),
            false,
        ),
    ));

    write_leveldb_log(&ldb.join("000001.log"), &entries);

    let mut cache = TeamsCache::open::<fn() -> Result<(), teams_cli::ingest::SourceError>>(Some(&src), None).unwrap();
    assert_eq!(cache.user_object_id().as_deref(), Some(me_oid));
    let snap = extract::extract(&mut cache).unwrap();
    assert_eq!(cache.bad_records, 0);
    assert_eq!(snap.messages.len(), 2);
    assert_eq!(snap.conversations[conv_id].name, "Synthetic Sync Crew");

    let st1 = sync::run_sync(Some(&src), false, false).unwrap();
    assert_eq!(st1["messages_new"], 2);
    assert_eq!(st1["cache_bad_records"], 0);

    let st2 = sync::run_sync(Some(&src), false, false).unwrap();
    assert!(st2.get("skipped").is_some(), "unchanged cache should skip: {st2}");

    let conn = archive::connect_readonly(None).unwrap();
    let found = api::search(
        &conn,
        Some("blob"),
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
    .unwrap();
    assert_eq!(found["matched"], 1);
    assert_eq!(found["results"][0]["text"], "External blob message");

    let inb = views::inbox(&conn, Some("7d"), 10).unwrap();
    assert_eq!(inb["counts"]["unreadChats"], 1);
}

#[test]
fn v8_deserializer_guards_depth_and_corrupt_arrays() {
    let mut deep = vec![0xFF, 15];
    for _ in 0..200 {
        deep.push(b'o');
        deep.extend_from_slice(&[b'"', 1, b'a']);
    }
    deep.push(b'0');
    let err = v8::Deserializer::deserialize(&deep).unwrap_err();
    assert!(err.contains("depth"), "{err}");

    let mut huge_arr = vec![0xFF, 15, b'A'];
    push_varint(&mut huge_arr, 1_000_000);
    let err = v8::Deserializer::deserialize(&huge_arr).unwrap_err();
    assert!(err.contains("truncated"), "{err}");
}

#[test]
fn load_cookies_reads_uncheckpointed_wal_database() {
    let td = tempfile::tempdir().unwrap();
    let db_path = td.path().join("Cookies");
    let pw = "safe-storage-pw";
    let enc = auth::encrypt_cookie_value("tok-from-wal", ".asm.skype.com", pw, 24);

    let writer = Connection::open(&db_path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO meta VALUES ('version', '24');
             CREATE TABLE cookies (
                 host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB,
                 path TEXT, expires_utc INTEGER, top_frame_site_key TEXT
             );",
        )
        .unwrap();
    writer
        .execute(
            "INSERT INTO cookies VALUES ('.asm.skype.com', 'skypetoken_asm', '', ?, '/', 0, '')",
            [&enc],
        )
        .unwrap();

    assert!(Path::new(&format!("{}-wal", db_path.display())).is_file());

    let loaded = auth::load_cookies(Some(&db_path), Some(pw), Some(1_700_000_000.0)).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].name, "skypetoken_asm");
    assert_eq!(loaded[0].value, "tok-from-wal");
    drop(writer);
}

#[test]
fn urlencoding_decode_handles_multibyte_utf8_after_percent() {
    assert_eq!(auth::urlencoding_decode("hello%20world"), "hello world");
    assert_eq!(auth::urlencoding_decode("%é"), "%é");
    assert_eq!(auth::urlencoding_decode("%aé"), "%aé");
    assert_eq!(auth::urlencoding_decode("100%"), "100%");
    assert_eq!(auth::urlencoding_decode("caf%C3%A9"), "café");
}

#[test]
fn fetch_disambiguates_200_char_filenames_and_downloads_over_http() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());

    let long_stem = "a".repeat(196);
    let first = td.path().join(format!("{long_stem}.pdf"));
    fs::write(&first, b"one").unwrap();
    let second = fetch::disambiguate_dest(first.clone(), Some("999"));
    assert_ne!(second, first);
    assert!(second.file_name().unwrap().to_string_lossy().ends_with("-999.pdf"));
    fs::write(&second, b"two").unwrap();
    let third = fetch::disambiguate_dest(first.clone(), Some("999"));
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert!(third.file_name().unwrap().to_string_lossy().ends_with("-2.pdf"));

    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let handle = std::thread::spawn(move || {
        for _ in 0..3 {
            let Ok(req) = server.recv() else { break };
            match req.url() {
                "/redir" => {
                    let loc = tiny_http::Header::from_bytes("Location", "/file").unwrap();
                    let _ = req.respond(tiny_http::Response::empty(302).with_header(loc));
                }
                "/file" => {
                    let ct = tiny_http::Header::from_bytes("Content-Type", "application/pdf").unwrap();
                    let cd = tiny_http::Header::from_bytes("Content-Disposition", r#"attachment; filename="Spec.pdf""#)
                        .unwrap();
                    let _ = req.respond(
                        tiny_http::Response::from_data(b"%PDF-1.7 mock".to_vec())
                            .with_header(ct)
                            .with_header(cd),
                    );
                }
                "/login" => {
                    let ct = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
                    let _ = req.respond(
                        tiny_http::Response::from_data(b"<html>Sign in to your Microsoft account</html>".to_vec())
                            .with_header(ct),
                    );
                }
                _ => {
                    let _ = req.respond(tiny_http::Response::empty(404));
                }
            }
        }
    });

    let out_file = td.path().join("downloaded.pdf");
    let res = fetch::fetch_attachment(
        None,
        Some(&format!("http://{addr}/redir")),
        None,
        None,
        Some(out_file.to_str().unwrap()),
        true,
    )
    .unwrap();
    assert_eq!(res["source"], "download");
    assert_eq!(res["contentType"], "application/pdf");
    assert_eq!(fs::read(&out_file).unwrap(), b"%PDF-1.7 mock");

    let err = fetch::fetch_attachment(None, Some(&format!("http://{addr}/login")), None, None, None, true).unwrap_err();
    assert!(err.0.contains("Microsoft login page"), "{err}");

    handle.join().unwrap();
}

#[test]
fn df_pk_parser_and_command_timeout() {
    assert_eq!(
        paths::parse_df_pk_available_bytes("/dev/nvme0n1p6 847106984 331751896 472250732 42% /home"),
        472_250_732 * 1024
    );
    assert_eq!(
        paths::parse_df_pk_available_bytes("//server/My Share 1000 400 600 40% /Volumes/My Share"),
        600 * 1024
    );
    assert!(paths::disk_free_bytes(Path::new("/nonexistent/deep/path/archive.sqlite3")) > 0);

    let mut fast = std::process::Command::new("sh");
    fast.args(["-c", "echo ok"]);
    let out = paths::run_command_with_timeout(&mut fast, Duration::from_secs(5)).unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");

    let mut slow = std::process::Command::new("sleep");
    slow.arg("10");
    let t0 = std::time::Instant::now();
    assert!(matches!(
        paths::run_command_with_timeout(&mut slow, Duration::from_millis(100)),
        Err(paths::CommandTimeoutError::TimedOut)
    ));
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn timeutil_iso_offsets_and_overflow() {
    assert!(timeutil::parse_when(Some("2026-11-01T14:00Z"), false)
        .unwrap()
        .is_some());
    assert!(timeutil::parse_when(Some("2026-11-01T14:00+05:30"), false)
        .unwrap()
        .is_some());
    assert!(timeutil::parse_when(Some("2026-11-01 14:00+05:30"), false)
        .unwrap()
        .is_some());
    assert!(timeutil::parse_when(Some("9999999999999999w"), false).is_err());
    assert_eq!(
        timeutil::parse_iso_ms("2026-11-01T14:00Z"),
        timeutil::parse_iso_ms("2026-11-01T14:00:00+00:00")
    );
}

#[test]
fn backups_detect_corrupt_record_versions_and_bad_restore_file() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());

    let db_path = td.path().join("archive.sqlite3");
    let conn = archive::connect(Some(&db_path)).unwrap();
    conn.execute(
        "INSERT INTO record_versions (source, record_key, hash, data, captured_at) VALUES ('message', 'k1', 'h1', ?, 1)",
        [b"not-valid-zlib".as_slice()],
    )
    .unwrap();
    let res = backups::verify_archive(Some(&conn), None, false);
    assert_eq!(res["ok"], false);
    assert_eq!(res["checks"]["recordVersions"]["bad"], 1);
    drop(conn);

    let garbage = td.path().join("garbage.sqlite3");
    fs::write(&garbage, b"not sqlite at all").unwrap();
    let err = backups::restore_backup(&garbage, Some(&db_path), false)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("failed its integrity check") && err.contains("left untouched"),
        "{err}"
    );
}
