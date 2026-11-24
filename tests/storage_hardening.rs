mod common;

use common::*;
use jiff::civil::{date, DateTime};
use jiff::tz::{Offset, TimeZone};
use jiff::ZonedDateTime;
use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use teams_cli::ingest::extract::{Message, RawRecord};
use teams_cli::paths;
use teams_cli::storage::{archive, backups, db};
use teams_cli::timeutil;

fn stat(st: &BTreeMap<String, i64>, k: &str) -> i64 {
    st.get(k).copied().unwrap_or(0)
}

fn fresh_archive() -> (tempfile::TempDir, Connection) {
    let td = tempfile::tempdir().unwrap();
    let conn = archive::connect(Some(&td.path().join("archive.sqlite3"))).unwrap();
    (td, conn)
}

fn many_msgs(n: usize, offset: usize) -> Vec<Message> {
    (offset..offset + n)
        .map(|i| {
            msg(
                GROUP,
                &format!("{}", 1_800_000_000_000u64 + i as u64),
                ADA,
                &format!("<p>note {i}</p>"),
                1_700_000_000_000 + i as i64,
                "Ada Lovelace",
            )
        })
        .collect()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn incremental_snapshot_across_lookup_chunks_detects_new_edited_and_unchanged() {
    const { assert!(db::LOOKUP_CHUNK <= 300) };
    let (_td, conn) = fresh_archive();
    let first = many_msgs(1000, 0);
    let st = archive::apply_snapshot(&conn, &snapshot(first.clone(), vec![], vec![]), Some(1_000)).unwrap();
    assert_eq!(stat(&st, "messages_new"), 1000);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1000);

    let st = archive::apply_snapshot(&conn, &snapshot(first.clone(), vec![], vec![]), Some(2_000)).unwrap();
    assert_eq!(stat(&st, "messages_new"), 0);
    assert_eq!(stat(&st, "messages_edited"), 0);
    assert_eq!(stat(&st, "revisions_new"), 0);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM messages WHERE last_seen_at = 2000"),
        1000
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM messages WHERE first_seen_at = 1000"),
        1000
    );

    let mut snap_msgs: Vec<Message> = first[..650].to_vec();
    for m in &first[650..700] {
        let mut e = m.clone();
        e.version = 2;
        e.edited_at = Some(5);
        e.content_html = format!("{} (edited)", m.content_html);
        e.text = format!("{} (edited)", m.text);
        snap_msgs.push(e);
    }
    snap_msgs.extend(many_msgs(10, 5000));
    let st = archive::apply_snapshot(&conn, &snapshot(snap_msgs, vec![], vec![]), Some(3_000)).unwrap();
    assert_eq!(stat(&st, "messages_new"), 10);
    assert_eq!(stat(&st, "messages_edited"), 50);
    assert_eq!(stat(&st, "revisions_new"), 60);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages"), 1010);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM messages WHERE last_seen_at = 3000"),
        710
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE version = 2"), 50);
}

#[test]
fn small_snapshot_uses_same_unchanged_shortcut() {
    let (_td, conn) = fresh_archive();
    let mut m = msg(GROUP, "1", ADA, "<p>hi</p>", 10, "Ada Lovelace");
    m.reactions = vec![json!({"key": "like", "users": [{"mri": BOB}]})];
    m.properties = json!({"links": "[]"});
    archive::apply_snapshot(&conn, &snapshot(vec![m.clone()], vec![], vec![]), Some(1)).unwrap();
    let st = archive::apply_snapshot(&conn, &snapshot(vec![m.clone()], vec![], vec![]), Some(2)).unwrap();
    assert_eq!(
        stat(&st, "messages_new") + stat(&st, "messages_edited") + stat(&st, "revisions_new"),
        0
    );
    assert_eq!(count(&conn, "SELECT last_seen_at FROM messages"), 2);

    m.reactions = vec![];
    archive::apply_snapshot(&conn, &snapshot(vec![m], vec![], vec![]), Some(3)).unwrap();
    let r: String = conn
        .query_row("SELECT reactions FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(r, "[]");
}

#[test]
fn duplicate_copies_record_older_revision_when_archive_has_newest() {
    let (_td, conn) = fresh_archive();
    let v1 = msg(GROUP, "7", ADA, "<p>draft</p>", 10, "Ada Lovelace");
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.content_html = "<p>final</p>".into();
    v2.text = "final".into();
    archive::apply_snapshot(&conn, &snapshot(vec![v2.clone()], vec![], vec![]), Some(1)).unwrap();
    let st = archive::apply_snapshot(&conn, &snapshot(vec![v1, v2], vec![], vec![]), Some(2)).unwrap();
    assert_eq!(stat(&st, "revisions_new"), 1, "the older copy becomes a revision");
    assert_eq!(stat(&st, "messages_edited"), 0);
    let text: String = conn.query_row("SELECT text FROM messages", [], |r| r.get(0)).unwrap();
    assert_eq!(text, "final");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM message_revisions"), 2);
}

#[test]
fn content_update_with_empty_text_stores_null_like_insert() {
    let (_td, conn) = fresh_archive();
    let m = msg(GROUP, "9", ADA, "<p>hello</p>", 10, "Ada Lovelace");
    archive::apply_snapshot(&conn, &snapshot(vec![m.clone()], vec![], vec![]), Some(1)).unwrap();
    let mut img = m.clone();
    img.version = 2;
    img.content_html = r#"<img src="x.png">"#.into();
    img.text = String::new();
    let st = archive::apply_snapshot(&conn, &snapshot(vec![img], vec![], vec![]), Some(2)).unwrap();
    assert_eq!(stat(&st, "messages_edited"), 1);
    let ty: String = conn
        .query_row("SELECT typeof(text) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ty, "null");

    let mut fresh = msg(GROUP, "10", ADA, r#"<img src="y.png">"#, 11, "Ada Lovelace");
    fresh.text = String::new();
    archive::apply_snapshot(&conn, &snapshot(vec![fresh], vec![], vec![]), Some(3)).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE text = ''"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM messages WHERE text IS NULL"), 2);
}

fn record(i: usize, payload: &str) -> RawRecord {
    RawRecord {
        source: "test-manager/things".into(),
        key: format!("thing/{i}"),
        data: json!({"id": i, "payload": payload}),
    }
}

#[test]
fn record_versions_dedupe_with_chunked_key_lookups() {
    let (_td, conn) = fresh_archive();
    let recs: Vec<RawRecord> = (0..750).map(|i| record(i, "a")).collect();
    let st = archive::apply_snapshot(&conn, &snapshot(vec![], vec![], recs.clone()), Some(1)).unwrap();
    assert_eq!(stat(&st, "record_versions_new"), 750);

    let st = archive::apply_snapshot(&conn, &snapshot(vec![], vec![], recs.clone()), Some(2)).unwrap();
    assert_eq!(stat(&st, "record_versions_new"), 0);

    let mut next = recs;
    next[701] = record(701, "b");
    next.push(record(701, "b"));
    let st = archive::apply_snapshot(&conn, &snapshot(vec![], vec![], next), Some(3)).unwrap();
    assert_eq!(stat(&st, "record_versions_new"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM record_versions"), 751);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM record_versions WHERE record_key = 'thing/701'"
        ),
        2
    );
}

#[test]
fn keyed_lookups_use_unique_indexes_not_table_scans() {
    let (_td, conn) = fresh_archive();
    for (table, cols, n) in [
        ("messages", "(conversation_id, message_id)", 2),
        ("record_versions", "(source, record_key, hash)", 3),
    ] {
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT * FROM {table} WHERE {cols} IN (VALUES {})",
            db::values_placeholders(3, n)
        );
        let plan: Vec<String> = conn
            .prepare(&sql)
            .unwrap()
            .query_map(rusqlite::params_from_iter(vec!["x"; 3 * n]), |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter()
                .any(|p| p.contains(&format!("SEARCH {table} USING")) && p.contains("INDEX")),
            "{table}: {plan:?}"
        );
        assert!(
            !plan.iter().any(|p| p.starts_with(&format!("SCAN {table}"))),
            "{table}: {plan:?}"
        );
    }
    assert_eq!(db::values_placeholders(2, 3), "(?, ?, ?), (?, ?, ?)");
}

#[test]
fn concurrent_writer_is_visible_to_lookup_under_write_lock() {
    let td = tempfile::tempdir().unwrap();
    let path = td.path().join("archive.sqlite3");
    let a = archive::connect(Some(&path)).unwrap();
    let b = archive::connect(Some(&path)).unwrap();
    let msgs = many_msgs(60, 0);
    archive::apply_snapshot(&b, &snapshot(msgs.clone(), vec![], vec![]), Some(1)).unwrap();
    let st = archive::apply_snapshot(&a, &snapshot(msgs, vec![], vec![]), Some(2)).unwrap();
    assert_eq!(stat(&st, "messages_new"), 0);
}

#[test]
fn v2_to_v3_copies_record_versions_in_batches_without_loss() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let path = td.path().join("archive.sqlite3");
    let n = 1_234i64;
    {
        let old = Connection::open(&path).unwrap();
        archive::create_schema_at(&old, 2).unwrap();
        old.execute_batch("BEGIN;").unwrap();
        for i in 0..n {
            old.execute(
                "INSERT INTO record_versions VALUES (?, 'src', ?, ?, ?, ?)",
                params![
                    i * 3 + 7,
                    format!("k{i}"),
                    format!("h{i}"),
                    json!({"i": i}).to_string(),
                    i
                ],
            )
            .unwrap();
        }
        old.execute_batch("COMMIT;").unwrap();
    }
    let conn = archive::connect(Some(&path)).unwrap();
    assert_eq!(count(&conn, "PRAGMA user_version"), archive::SCHEMA_VERSION);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM record_versions"), n);
    assert_eq!(count(&conn, "SELECT MIN(id) FROM record_versions"), 7);
    assert_eq!(count(&conn, "SELECT MAX(id) FROM record_versions"), (n - 1) * 3 + 7);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM record_versions WHERE typeof(data) != 'blob'"
        ),
        0
    );
    let mut stmt = conn
        .prepare("SELECT id, data FROM record_versions ORDER BY id")
        .unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut i = 0i64;
    while let Some(r) = rows.next().unwrap() {
        assert_eq!(r.get::<_, i64>(0).unwrap(), i * 3 + 7);
        let v = archive::decode_record_ref(r.get_ref(1).unwrap()).unwrap();
        assert_eq!(v, json!({"i": i}));
        i += 1;
    }
    assert_eq!(i, n);
}

fn set_mtime_ago(p: &Path, ago: Duration) {
    let t = SystemTime::now() - ago;
    fs::File::open(p).unwrap().set_modified(t).unwrap();
}

#[test]
fn is_scheduled_backup_classifies_names() {
    assert!(backups::is_scheduled_backup("archive-20260101-000000.sqlite3.xz"));
    assert!(backups::is_scheduled_backup("archive-20260101-000000.sqlite3"));
    assert!(!backups::is_scheduled_backup(
        "archive-before-restore-20260101-000000.sqlite3.xz"
    ));
    assert!(!backups::is_scheduled_backup(
        "archive-20260101-000000.sqlite3.xz.partial"
    ));
    assert!(!backups::is_scheduled_backup("notes.sqlite3"));
    assert!(!backups::is_scheduled_backup(""));
}

#[test]
fn stale_verify_dirs_are_removed_fresh_ones_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let old = tmp.path().join("teams-cli-verify-old");
    let fresh = tmp.path().join("teams-cli-verify-fresh");
    let other = tmp.path().join("unrelated-old");
    for d in [&old, &fresh, &other] {
        fs::create_dir(d).unwrap();
        fs::write(d.join("verify.sqlite3"), b"x").unwrap();
    }
    set_mtime_ago(&old, Duration::from_secs(2 * 3600));
    set_mtime_ago(&other, Duration::from_secs(2 * 3600));
    assert_eq!(backups::remove_stale_verify_dirs(Some(tmp.path())), 1);
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(other.exists());
}

#[test]
fn pathless_destinations_return_errors_instead_of_panicking() {
    let (_td, conn) = fresh_archive();
    archive::apply_snapshot(&conn, &snapshot(many_msgs(2, 0), vec![], vec![]), Some(1)).unwrap();
    for bad in ["/", "..", "/tmp/.."] {
        assert!(backups::backup(&conn, Path::new(bad)).is_err(), "backup({bad})");
        let v = backups::verify_archive(None, Some(Path::new(bad)), false);
        assert_eq!(v["ok"], false, "verify({bad})");
    }
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backups::backup(&conn, &bk_dir.path().join("snap.sqlite3.xz")).unwrap();
    let err = backups::restore_backup(&bk, Some(Path::new("/")), false).unwrap_err();
    assert!(err.to_string().contains("not a file path"), "{err}");
    assert!(backups::restore_backup(Path::new("/"), None, false).is_err());
}

#[test]
fn materialized_restore_candidate_is_never_world_readable() {
    let (_td, conn) = fresh_archive();
    let mut msgs = many_msgs(3000, 0);
    for (i, m) in msgs.iter_mut().enumerate() {
        m.content_html = format!(
            "<p>{}</p>",
            format!("{:x}", (i as u64).wrapping_mul(0x9E3779B97F4A7C15)).repeat(40)
        );
    }
    archive::apply_snapshot(&conn, &snapshot(msgs, vec![], vec![]), Some(1)).unwrap();
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backups::backup(&conn, &bk_dir.path().join("snap.sqlite3.xz")).unwrap();

    let dest_dir = tempfile::tempdir().unwrap();
    let dest = dest_dir.path().join("restored.sqlite3");
    let candidate = dest_dir.path().join("restored.sqlite3.restore-partial");
    let stop = Arc::new(AtomicBool::new(false));
    let bad_mode = Arc::new(AtomicU32::new(0));
    let seen = Arc::new(AtomicU32::new(0));
    let watcher = {
        let (stop, bad_mode, seen, candidate) = (stop.clone(), bad_mode.clone(), seen.clone(), candidate.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if let Ok(md) = fs::metadata(&candidate) {
                    seen.fetch_add(1, Ordering::Relaxed);
                    let mode = md.permissions().mode() & 0o777;
                    if mode != 0o600 {
                        bad_mode.store(mode, Ordering::Relaxed);
                    }
                }
            }
        })
    };
    let r = backups::restore_backup(&bk, Some(&dest), false).unwrap();
    stop.store(true, Ordering::Relaxed);
    watcher.join().unwrap();
    assert_eq!(bad_mode.load(Ordering::Relaxed), 0, "candidate was briefly not 0600");
    assert_eq!(r["messages"], 3000);
    assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!candidate.exists());
    eprintln!("watcher observed the candidate {} times", seen.load(Ordering::Relaxed));

    fs::write(&candidate, b"stale").unwrap();
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644)).unwrap();
    let v = backups::verify_archive(None, Some(&bk), false);
    assert_eq!(v["ok"], true, "{v}");
    backups::restore_backup(&bk, Some(&dest), false).unwrap();
    assert_eq!(fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn corrupt_legacy_backup_is_skipped_and_healthy_ones_compressed() {
    let (_td, conn) = fresh_archive();
    archive::apply_snapshot(&conn, &snapshot(many_msgs(5, 0), vec![], vec![]), Some(1)).unwrap();
    let d = tempfile::tempdir().unwrap();
    let good = d.path().join("archive-20250102-000000.sqlite3");
    backups::backup(&conn, &good).unwrap();
    let corrupt = d.path().join("archive-20250101-000000.sqlite3");
    fs::write(
        &corrupt,
        b"this is not a sqlite database at all, just junk bytes".repeat(100),
    )
    .unwrap();

    let (n, warnings) = backups::compress_old_backups_with_warnings(Some(d.path()));
    assert_eq!(n, 1);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("archive-20250101-000000.sqlite3"), "{warnings:?}");
    assert!(corrupt.exists(), "corrupt legacy file left for inspection");
    assert!(!good.exists());
    assert!(d.path().join("archive-20250102-000000.sqlite3.xz").exists());
    assert_eq!(backups::compress_old_backups(Some(d.path())).unwrap(), 0);
}

#[test]
fn backup_if_due_still_rotates_when_a_legacy_backup_is_corrupt() {
    let _g = env_lock();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", home.path());
    let conn = archive::connect(Some(&home.path().join("archive.sqlite3"))).unwrap();
    archive::apply_snapshot(&conn, &snapshot(many_msgs(5, 0), vec![], vec![]), Some(1)).unwrap();
    let bdir = paths::backups_dir();
    if paths::disk_free_bytes(&bdir) < backups::MIN_FREE_BYTES + (64 << 20) {
        eprintln!("skipping: not enough free disk for backup_if_due");
        return;
    }
    fs::write(bdir.join("archive-20240101-000000.sqlite3"), b"junk".repeat(500)).unwrap();
    let fakes: Vec<PathBuf> = (0..12)
        .map(|i| {
            let p = bdir.join(format!("archive-20250101-0000{i:02}.sqlite3.xz"));
            fs::write(&p, b"fake").unwrap();
            p
        })
        .collect();
    let dest = backups::backup_if_due(&conn, 0.0).unwrap().expect("due");
    assert!(dest.exists());
    for p in &fakes[..6] {
        assert!(!p.exists(), "{} should be rotated away", p.display());
    }
    for p in &fakes[6..] {
        assert!(p.exists(), "{} should be kept", p.display());
    }
    let warn = archive::get_meta(&conn, "backup_warning").unwrap_or_default();
    assert!(warn.contains("archive-20240101-000000.sqlite3"), "{warn}");
    std::env::remove_var("TEAMS_CLI_HOME");
}

static TZ_LOCK: Mutex<()> = Mutex::new(());

fn with_tz<T>(tz: &str, f: impl FnOnce() -> T) -> T {
    let _g = TZ_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var_os("TZ");
    std::env::set_var("TZ", tz);
    let out = f();
    match prev {
        Some(v) => std::env::set_var("TZ", v),
        None => std::env::remove_var("TZ"),
    }
    out
}

fn utc_ms(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> i64 {
    DateTime::new(y, mo, d, h, mi, 0, 0)
        .unwrap()
        .to_zoned_datetime(TimeZone::UTC)
        .unwrap()
        .timestamp()
        .as_millisecond()
}

fn local_zoned(y: i16, mo: i8, d: i8, h: i8, mi: i8) -> ZonedDateTime {
    DateTime::new(y, mo, d, h, mi, 0, 0)
        .unwrap()
        .to_zoned_datetime(timeutil::local_tz())
        .unwrap()
}

fn when(v: &str, end: bool, now: &ZonedDateTime) -> i64 {
    timeutil::parse_when_at(Some(v), end, now).unwrap().unwrap()
}

#[test]
fn today_and_yesterday_follow_calendar_days_across_spring_forward() {
    if !Path::new("/usr/share/zoneinfo/America/New_York").exists() {
        return;
    }
    with_tz("America/New_York", || {
        let now = local_zoned(2026, 3, 9, 12, 0);
        assert_eq!(when("yesterday", false, &now), utc_ms(2026, 3, 8, 5, 0));
        assert_eq!(when("yesterday", true, &now), utc_ms(2026, 3, 9, 4, 0));
        assert_eq!(when("today", false, &now), utc_ms(2026, 3, 9, 4, 0));
        assert_eq!(when("today", true, &now), utc_ms(2026, 3, 10, 4, 0));
        let on_dst_day = local_zoned(2026, 3, 8, 20, 0);
        assert_eq!(
            when("today", true, &on_dst_day) - when("today", false, &on_dst_day),
            23 * 3_600_000
        );
    });
}

#[test]
fn today_and_yesterday_follow_calendar_days_across_fall_back() {
    if !Path::new("/usr/share/zoneinfo/America/New_York").exists() {
        return;
    }
    with_tz("America/New_York", || {
        let now = local_zoned(2026, 11, 2, 9, 30);
        assert_eq!(when("yesterday", false, &now), utc_ms(2026, 11, 1, 4, 0));
        assert_eq!(when("yesterday", true, &now), utc_ms(2026, 11, 2, 5, 0));
        assert_eq!(
            when("yesterday", true, &now) - when("yesterday", false, &now),
            25 * 3_600_000
        );
        assert_eq!(when("2026-11-01", true, &now), when("today", false, &now));
    });
}

#[test]
fn bare_date_in_a_midnight_dst_gap_resolves_to_first_valid_instant() {
    if !Path::new("/usr/share/zoneinfo/America/Santiago").exists() {
        return;
    }
    with_tz("America/Santiago", || {
        let first = utc_ms(2026, 9, 6, 4, 0);
        let now = local_zoned(2026, 9, 6, 10, 0);
        assert_eq!(when("2026-09-06", false, &now), first);
        assert_eq!(when("2026-09-05", true, &now), first);
        assert_eq!(when("today", false, &now), first);
        let next_day = local_zoned(2026, 9, 7, 10, 0);
        assert_eq!(when("yesterday", false, &next_day), first);
        assert_eq!(timeutil::local_midnight_ms(date(2026, 9, 6)), Some(first));
    });
}

#[test]
fn first_valid_local_ms_is_midnight_when_it_exists() {
    let tz = TimeZone::fixed(Offset::from_seconds(5 * 3600 + 1800).unwrap());
    let d = date(2026, 11, 1);
    assert_eq!(
        timeutil::first_valid_local_ms(&tz, d),
        Some(utc_ms(2026, 10, 31, 18, 30))
    );
}

#[test]
fn atomic_writes_leave_no_temp_files_and_apply_mode() {
    let d = tempfile::tempdir().unwrap();
    let a = d.path().join("a.json");
    paths::atomic_write_bytes(&a, b"{}", 0o600).unwrap();
    paths::atomic_write_bytes(&a, b"{\"x\":1}", 0o640).unwrap();
    assert_eq!(fs::read(&a).unwrap(), b"{\"x\":1}");
    assert_eq!(fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o640);
    let b = d.path().join("b.bin");
    assert_eq!(paths::atomic_copy_file(&a, &b, 0o600, ".copy-").unwrap(), 7);
    assert_eq!(fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o600);
    let names: Vec<String> = fs::read_dir(d.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(paths::atomic_copy_file(&d.path().join("missing"), &b, 0o600, ".copy-").is_err());
    assert_eq!(fs::read_dir(d.path()).unwrap().count(), 2);
}
