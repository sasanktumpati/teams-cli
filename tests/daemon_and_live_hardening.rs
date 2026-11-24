mod common;

use base64::Engine;
use common::*;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use teams_cli::daemon::{self, SyncConfig, SyncRunner};
use teams_cli::launchd;
use teams_cli::live::{auth, fetch, http, send};

fn raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("bad response: {text:?}"));
    (status, body.to_string())
}

fn req_with(addr: SocketAddr, method: &str, target: &str, headers: &[&str]) -> (u16, String) {
    let mut r = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\nContent-Length: 0\r\n");
    if !headers
        .iter()
        .any(|h| h.to_ascii_lowercase().starts_with("authorization:"))
    {
        r.push_str(&format!("Authorization: {}\r\n", bearer()));
    }
    for h in headers {
        r.push_str(h);
        r.push_str("\r\n");
    }
    r.push_str("\r\n");
    raw(addr, &r)
}

struct Daemon {
    handle: Option<daemon::DaemonHandle>,
    _td: tempfile::TempDir,
}

impl Daemon {
    fn start(sync: Option<SyncConfig>, log_to_file: bool) -> Self {
        let (td, _home) = home_with_archive();
        let handle = daemon::start(0, sync, log_to_file).unwrap();
        Daemon {
            handle: Some(handle),
            _td: td,
        }
    }
    fn addr(&self) -> SocketAddr {
        self.handle.as_ref().unwrap().addr()
    }
    fn home(&self) -> &Path {
        self._td.path()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

fn jwt(payload: &Value) -> String {
    let e = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    format!(
        "{}.{}.{}",
        e(br#"{"alg":"none"}"#),
        e(payload.to_string().as_bytes()),
        e(b"sig")
    )
}

fn cookie(host: &str, name: &str, value: &str, partitioned: bool) -> auth::Cookie {
    auth::Cookie {
        host_key: host.to_string(),
        name: name.to_string(),
        value: value.to_string(),
        path: "/".to_string(),
        expires_utc: 0,
        partitioned,
    }
}

const NOW_TS: f64 = 1_700_000_000.0;

fn skype_jwt(rgn: &str, exp: f64, skypeid: &str) -> String {
    jwt(&json!({"rgn": rgn, "exp": exp, "skypeid": skypeid, "tid": "tenant"}))
}

#[test]
fn extract_host_rejects_non_numeric_or_empty_ports() {
    assert_eq!(daemon::extract_host(Some("127.0.0.1:8080")), "127.0.0.1");
    assert_eq!(daemon::extract_host(Some("localhost")), "localhost");
    assert_eq!(daemon::extract_host(Some("LOCALHOST:1")), "localhost");
    assert_eq!(daemon::extract_host(Some("127.0.0.1:notaport")), "");
    assert_eq!(daemon::extract_host(Some("localhost:evil.com")), "");
    assert_eq!(daemon::extract_host(Some("127.0.0.1:")), "");
    assert_eq!(daemon::extract_host(Some("127.0.0.1:80a")), "");
    assert_eq!(daemon::extract_host(Some("127.0.0.1:+80")), "");
    assert_eq!(daemon::extract_host(Some("[::1]:47613")), "::1");
    assert_eq!(daemon::extract_host(Some("[::1]:")), "");
    assert!(!daemon::is_allowed(Some("localhost:evil.com"), None, None));
    assert!(!daemon::is_allowed(Some("127.0.0.1:"), None, None));
}

#[test]
fn is_allowed_headers_inspects_every_occurrence() {
    let ok = daemon::is_allowed_headers;
    assert!(ok(&["127.0.0.1:1"], &[], &[]));
    assert!(ok(&["localhost"], &[], &["none"]));
    assert!(ok(&["[::1]:9"], &[], &["", "NONE"]));
    assert!(!ok(&[], &[], &[]));
    assert!(!ok(&["127.0.0.1", "127.0.0.1"], &[], &[]));
    assert!(!ok(&["127.0.0.1", "evil.com"], &[], &[]));
    assert!(!ok(&["evil.com"], &[], &[]));
    assert!(ok(&["127.0.0.1"], &[""], &[]));
    assert!(!ok(&["127.0.0.1"], &["", ""], &[]));
    assert!(!ok(&["127.0.0.1"], &["", "http://evil.com"], &[]));
    assert!(!ok(&["127.0.0.1"], &["null"], &[]));
    assert!(!ok(&["127.0.0.1"], &["http://127.0.0.1"], &[]));
    assert!(!ok(&["127.0.0.1"], &[], &["none", "cross-site"]));
    assert!(!ok(&["127.0.0.1"], &[], &["same-origin"]));
}

#[test]
fn daemon_rejects_duplicate_and_hostile_headers() {
    let _g = env_lock();
    let d = Daemon::start(None, false);
    let a = d.addr();
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1"]).0, 200);
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1:12345"]).0, 200);
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1:notaport"]).0, 403);
    assert_eq!(req_with(a, "GET", "/health", &["Host: localhost:evil.com"]).0, 403);
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1:"]).0, 403);
    assert_eq!(
        req_with(a, "GET", "/health", &["Host: 127.0.0.1", "Host: evil.com"]).0,
        403
    );
    assert_eq!(
        req_with(a, "GET", "/health", &["Host: evil.com", "Host: 127.0.0.1"]).0,
        403
    );
    assert_eq!(
        req_with(a, "GET", "/health", &["Host: 127.0.0.1", "Host: 127.0.0.1"]).0,
        403
    );
    assert_eq!(
        req_with(a, "GET", "/v1/status", &["Host: 127.0.0.1", "Origin: ", "Origin: "]).0,
        403
    );
    assert_eq!(
        req_with(a, "GET", "/v1/status", &["Host: 127.0.0.1", "Origin: null"]).0,
        403
    );
    assert_eq!(
        req_with(
            a,
            "GET",
            "/v1/status",
            &["Host: 127.0.0.1", "Origin: ", "Origin: http://evil.com"]
        )
        .0,
        403
    );
    assert_eq!(
        req_with(
            a,
            "GET",
            "/v1/status",
            &["Host: 127.0.0.1", "Sec-Fetch-Site: none", "Sec-Fetch-Site: cross-site"]
        )
        .0,
        403
    );
    assert_eq!(
        req_with(a, "GET", "/v1/status", &["Host: 127.0.0.1", "Sec-Fetch-Site: none"]).0,
        200
    );
}

#[test]
fn health_only_answers_get_and_head() {
    let _g = env_lock();
    let d = Daemon::start(None, false);
    let a = d.addr();
    let (st, body) = req_with(a, "GET", "/health", &["Host: 127.0.0.1"]);
    assert_eq!(st, 200);
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["ok"], true);
    assert_eq!(req_with(a, "HEAD", "/health", &["Host: 127.0.0.1"]).0, 200);
    for m in ["POST", "DELETE", "PUT", "PATCH"] {
        let (st, body) = req_with(a, m, "/health", &["Host: 127.0.0.1"]);
        assert_eq!(st, 404, "{m} /health");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], "not_found", "{v}");
    }
}

#[test]
fn worker_pool_serves_many_concurrent_requests() {
    let _g = env_lock();
    let d = Daemon::start(None, false);
    let a = d.addr();
    let clients: Vec<_> = (0..(daemon::HTTP_WORKERS * 4))
        .map(|_| thread::spawn(move || req_with(a, "GET", "/v1/status", &["Host: 127.0.0.1"]).0))
        .collect();
    for c in clients {
        let st = c.join().unwrap();
        assert!(st == 200 || st == 503, "unexpected status {st}");
    }
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1"]).0, 200);
}

#[test]
fn slow_requests_do_not_spawn_unbounded_threads_and_shutdown_drains() {
    let _g = env_lock();
    let runner: SyncRunner = Arc::new(|_| {
        thread::sleep(Duration::from_millis(300));
        Ok(json!({"messages": 1}))
    });
    let d = Daemon::start(Some(SyncConfig { runner, interval: None }), false);
    let a = d.addr();
    let sync = thread::spawn(move || req_with(a, "POST", "/v1/sync", &["Host: 127.0.0.1"]).0);
    thread::sleep(Duration::from_millis(50));
    assert_eq!(req_with(a, "GET", "/health", &["Host: 127.0.0.1"]).0, 200);
    assert_eq!(sync.join().unwrap(), 200);
    let started = Instant::now();
    drop(d);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}",
        started.elapsed()
    );
}

#[test]
fn handler_panics_are_logged_to_daemon_log_before_500() {
    let _g = env_lock();
    let runner: SyncRunner = Arc::new(|_| panic!("boom-from-sync-runner"));
    let d = Daemon::start(Some(SyncConfig { runner, interval: None }), true);
    let (st, body) = req_with(d.addr(), "POST", "/v1/sync", &["Host: 127.0.0.1"]);
    assert_eq!(st, 500, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("boom-from-sync-runner"),
        "{v}"
    );
    let log = fs::read_to_string(d.home().join("logs/daemon.log")).unwrap();
    assert!(log.contains("ERROR"), "{log}");
    assert!(
        log.contains("handler panic on POST /v1/sync: boom-from-sync-runner"),
        "{log}"
    );
    assert_eq!(req_with(d.addr(), "GET", "/health", &["Host: 127.0.0.1"]).0, 200);
    drop(d);

    let runner2: SyncRunner = Arc::new(|_| panic!("second-home-panic"));
    let d2 = Daemon::start(
        Some(SyncConfig {
            runner: runner2,
            interval: None,
        }),
        true,
    );
    let _ = req_with(d2.addr(), "POST", "/v1/sync", &["Host: 127.0.0.1"]);
    let log2 = fs::read_to_string(d2.home().join("logs/daemon.log")).unwrap();
    assert!(log2.contains("second-home-panic"), "{log2}");
    assert!(!log2.contains("boom-from-sync-runner"), "{log2}");
}

fn running_rows(home: &Path) -> Vec<(String, Option<String>, Option<i64>)> {
    let c = Connection::open(home.join("archive.sqlite3")).unwrap();
    let mut st = c
        .prepare("SELECT status, message, finished_at FROM sync_runs WHERE source = 'test-interrupted' ORDER BY id")
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn startup_marks_interrupted_sync_runs_as_error() {
    let _g = env_lock();
    let (td, home) = home_with_archive();
    {
        let c = Connection::open(home.join("archive.sqlite3")).unwrap();
        c.execute(
            "INSERT INTO sync_runs (started_at, status, source) VALUES (1, 'running', 'test-interrupted')",
            [],
        )
        .unwrap();
    }
    {
        let _lock = teams_cli::ingest::sync::acquire_sync_lock(0.0).unwrap();
        assert_eq!(daemon::recover_interrupted_sync_runs(), 0);
        assert_eq!(running_rows(&home)[0].0, "running");
    }
    let runner: SyncRunner = Arc::new(|_| Ok(json!({})));
    let h = daemon::start(0, Some(SyncConfig { runner, interval: None }), false).unwrap();
    let rows = running_rows(&home);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "error");
    assert_eq!(rows[0].1.as_deref(), Some("interrupted by daemon restart"));
    assert!(rows[0].2.is_some());
    h.shutdown();
    assert_eq!(daemon::recover_interrupted_sync_runs(), 0);
    drop(td);
}

#[test]
fn launchd_plist_is_written_atomically_with_0644() {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().join("dev.teams-cli.daemon.plist");
    fs::write(&p, "old").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
    launchd::write_plist_atomic(&p, "<plist>new</plist>").unwrap();
    assert_eq!(fs::read_to_string(&p).unwrap(), "<plist>new</plist>");
    assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
    let leftovers: Vec<_> = fs::read_dir(td.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "dev.teams-cli.daemon.plist")
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    assert!(launchd::write_plist_atomic(&td.path().join("nope/x.plist"), "x").is_err());
}

#[test]
fn skypetoken_falls_back_across_candidates() {
    let expired = skype_jwt("emea", NOW_TS - 10.0, "expired");
    let valid_short = skype_jwt("emea", NOW_TS + 100.0, "short");
    let valid_long = skype_jwt("amer", NOW_TS + 9000.0, "long");
    let untrusted = skype_jwt("apac", NOW_TS + 99_999.0, "evil");

    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &expired, false),
        cookie(".asm.skype.com", "skypetoken_asm", "not-a-jwt", false),
        cookie(".asm.skype.com", "skypetoken_asm", &valid_short, false),
    ];
    let tok = auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap();
    assert_eq!(tok.skype_id, "short");
    assert_eq!(tok.region, "emea");

    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &valid_short, false),
        cookie("teams.microsoft.com", "skypetoken_asm", &valid_long, false),
    ];
    assert_eq!(
        auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap().skype_id,
        "long"
    );

    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &valid_long, true),
        cookie(".asm.skype.com", "skypetoken_asm", &valid_short, false),
    ];
    assert_eq!(
        auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap().skype_id,
        "short"
    );

    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &expired, false),
        cookie(".asm.skype.com", "skypetoken_asm", &valid_long, true),
    ];
    assert_eq!(
        auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap().skype_id,
        "long"
    );

    let cookies = vec![
        cookie(".evil.example", "skypetoken_asm", &untrusted, false),
        cookie("skype.com.evil.example", "skypetoken_asm", &untrusted, false),
        cookie(".asm.skype.com", "skypetoken_asm", &valid_short, true),
    ];
    assert_eq!(
        auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap().skype_id,
        "short"
    );
    let only_untrusted = vec![cookie(".evil.example", "skypetoken_asm", &untrusted, false)];
    let err = auth::load_skypetoken(Some(&only_untrusted), Some(NOW_TS)).unwrap_err();
    assert!(err.0.contains("not found"), "{err}");

    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &expired, false),
        cookie(".asm.skype.com", "skypetoken_asm", "garbage", false),
    ];
    let err = auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap_err();
    assert!(err.0.contains("expired"), "{err}");
}

#[test]
fn display_name_ignores_untrusted_authtoken_hosts() {
    let tok = skype_jwt("emea", NOW_TS + 100.0, "me");
    let evil_auth = jwt(&json!({"name": "Mallory"}));
    let good_auth = jwt(&json!({"name": "Ada Lovelace"}));
    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &tok, false),
        cookie(".evil.example", "authtoken_asm", &evil_auth, false),
        cookie(".asm.skype.com", "authtoken_asm", &good_auth, true),
    ];
    let t = auth::load_skypetoken(Some(&cookies), Some(NOW_TS)).unwrap();
    assert_eq!(t.display_name, "Ada Lovelace");
    let cookies = vec![
        cookie(".asm.skype.com", "skypetoken_asm", &tok, false),
        cookie(".evil.example", "authtoken_asm", &evil_auth, false),
    ];
    assert_eq!(
        auth::load_skypetoken(Some(&cookies), Some(NOW_TS))
            .unwrap()
            .display_name,
        ""
    );
}

#[test]
fn cookie_header_skips_crlf_and_nul_cookies() {
    let cookies = vec![
        cookie(".teams.microsoft.com", "good", "v1", false),
        cookie(".teams.microsoft.com", "inject", "x\r\nX-Evil: 1", false),
        cookie(".teams.microsoft.com", "lf", "a\nb", false),
        cookie(".teams.microsoft.com", "nul", "a\0b", false),
        cookie(".teams.microsoft.com", "bad\rname", "v", false),
        cookie("teams.microsoft.com", "dup", "bad\r\n", false),
        cookie(".teams.microsoft.com", "dup", "clean", false),
    ];
    let hdr = auth::build_cookie_header(&cookies, "https://teams.microsoft.com/api", Some(NOW_TS));
    assert!(
        !hdr.contains('\r') && !hdr.contains('\n') && !hdr.contains('\0'),
        "{hdr:?}"
    );
    assert!(hdr.contains("good=v1"), "{hdr}");
    assert!(hdr.contains("dup=clean"), "{hdr}");
    assert!(
        !hdr.contains("inject") && !hdr.contains("lf=") && !hdr.contains("nul="),
        "{hdr}"
    );
}

fn make_cookie_db(path: &Path, rows: &[(&str, &str, &str)]) {
    let c = Connection::open(path).unwrap();
    c.execute_batch(
        "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
         INSERT INTO meta VALUES ('version', '24');
         CREATE TABLE cookies (
             host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB,
             path TEXT, expires_utc INTEGER, top_frame_site_key TEXT
         );",
    )
    .unwrap();
    for (h, n, v) in rows {
        c.execute(
            "INSERT INTO cookies VALUES (?, ?, ?, X'', '/', 0, '')",
            rusqlite::params![h, n, v],
        )
        .unwrap();
    }
}

fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

#[test]
fn load_cookies_propagates_wal_copy_failures() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("Cookies");
    make_cookie_db(&db, &[(".asm.skype.com", "skypetoken_asm", "plain")]);
    let ok = auth::load_cookies(Some(&db), Some("pw"), Some(NOW_TS)).unwrap();
    assert_eq!(ok.len(), 1);

    if running_as_root() {
        return;
    }
    let wal = td.path().join("Cookies-wal");
    fs::write(&wal, b"").unwrap();
    fs::set_permissions(&wal, fs::Permissions::from_mode(0o000)).unwrap();
    let err = auth::load_cookies(Some(&db), Some("pw"), Some(NOW_TS)).unwrap_err();
    assert!(err.0.contains("Could not read Teams cookie database"), "{err}");
    assert!(err.0.contains("Cookies-wal"), "{err}");
    fs::set_permissions(&wal, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn sanitize_filename_caps_utf8_bytes_on_char_boundaries() {
    let cjk = format!("{}.pdf", "日".repeat(100));
    let s = fetch::sanitize_filename(&cjk);
    assert!(s.len() <= fetch::MAX_FILENAME_BYTES, "{} bytes", s.len());
    assert!(s.ends_with(".pdf"), "{s}");
    assert!(s.trim_end_matches(".pdf").chars().all(|c| c == '日'));
    assert_eq!(s.len(), 196 / 3 * 3 + 4);

    let emoji = "😀".repeat(80);
    let s = fetch::sanitize_filename(&emoji);
    assert!(s.len() <= 200 && s.len() == 200, "{}", s.len());
    assert!(s.chars().all(|c| c == '😀'));

    let mixed = format!("{}é漢😀{}", "a".repeat(197), "z".repeat(50));
    let s = fetch::sanitize_filename(&mixed);
    assert!(s.len() <= 200);
    assert!(s.starts_with(&"a".repeat(197)));

    assert_eq!(fetch::sanitize_filename("報告書.docx"), "報告書.docx");
    assert_eq!(fetch::sanitize_filename(&"b".repeat(300)).len(), 200);
}

#[test]
fn disambiguate_dest_keeps_multibyte_names_within_200_bytes() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let name = fetch::sanitize_filename(&format!("{}.pdf", "資料".repeat(60)));
    assert!(name.len() <= 200);
    let first = td.path().join(&name);
    fs::write(&first, b"one").unwrap();
    let second = fetch::disambiguate_dest(first.clone(), Some("12345"));
    let second_name = second.file_name().unwrap().to_str().unwrap().to_string();
    assert!(second_name.len() <= 200, "{} bytes", second_name.len());
    assert!(second_name.ends_with("-12345.pdf"), "{second_name}");
    fs::write(&second, b"two").unwrap();
    let third = fetch::disambiguate_dest(first.clone(), Some("12345"));
    let third_name = third.file_name().unwrap().to_str().unwrap();
    assert!(third_name.len() <= 200, "{} bytes", third_name.len());
    assert!(third_name.ends_with("-2.pdf"), "{third_name}");
    fs::write(&third, b"three").unwrap();
    assert_ne!(third, second);
}

#[test]
fn ambiguous_fetch_suggests_message_flag() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(
        "CREATE TABLE messages (conversation_id TEXT, message_id TEXT, arrived_at INTEGER);
         CREATE TABLE attachments (id INTEGER PRIMARY KEY, conversation_id TEXT, message_id TEXT,
                                   kind TEXT, name TEXT, url TEXT);
         INSERT INTO messages VALUES ('19:a@thread.v2', '1', 1), ('19:a@thread.v2', '2', 2);
         INSERT INTO attachments VALUES
           (1, '19:a@thread.v2', '1', 'file', 'report-a.pdf', 'https://x.sharepoint.com/a'),
           (2, '19:a@thread.v2', '2', 'file', 'report-b.pdf', 'https://x.sharepoint.com/b');",
    )
    .unwrap();
    let err = fetch::resolve_attachment(&c, Some("report"), None, None).unwrap_err();
    assert!(err.0.contains("--message,"), "{err}");
    assert!(!err.0.contains("--message-id"), "{err}");
}

#[test]
fn http_agent_times_out_a_stalled_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hold = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_secs(3));
        drop(s);
    });
    assert_eq!(http::CONNECT_TIMEOUT, Duration::from_secs(15));
    assert_eq!(http::READ_TIMEOUT, Duration::from_secs(60));
    let agent = http::agent(Duration::from_millis(300));
    let started = Instant::now();
    let res = agent.get(&format!("http://{addr}/")).call();
    assert!(res.is_err());
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    hold.join().unwrap();
}

struct Captured {
    url: String,
    method: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn mock_chatsvc(
    status: u16,
    body: &'static str,
    extra_header: Option<(&'static str, &'static str)>,
) -> (String, Arc<Mutex<Vec<Captured>>>, thread::JoinHandle<()>) {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_c = Arc::clone(&seen);
    let h = thread::spawn(move || {
        if let Ok(Some(mut req)) = server.recv_timeout(Duration::from_secs(10)) {
            let mut b = String::new();
            let _ = req.as_reader().read_to_string(&mut b);
            seen_c.lock().unwrap().push(Captured {
                url: req.url().to_string(),
                method: req.method().as_str().to_string(),
                headers: req
                    .headers()
                    .iter()
                    .map(|h| (h.field.as_str().as_str().to_string(), h.value.as_str().to_string()))
                    .collect(),
                body: b,
            });
            let mut resp = tiny_http::Response::from_string(body).with_status_code(status);
            if let Some((k, v)) = extra_header {
                resp = resp.with_header(tiny_http::Header::from_bytes(k, v).unwrap());
            }
            let _ = req.respond(resp);
        }
    });
    (format!("http://{addr}"), seen, h)
}

struct SendEnv {
    _td: tempfile::TempDir,
    token: String,
}

fn send_env() -> SendEnv {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("Cookies");
    let token = jwt(&json!({"rgn": "emea", "exp": 4_000_000_000u64, "skypeid": "me", "tid": "t"}));
    let auth_tok = jwt(&json!({"name": "Ada Sender"}));
    make_cookie_db(
        &db,
        &[
            (".asm.skype.com", "skypetoken_asm", &token),
            (".asm.skype.com", "authtoken_asm", &auth_tok),
        ],
    );
    std::env::set_var("TEAMS_CLI_COOKIES", &db);
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    std::env::set_var(send::ENABLE_ENV, "1");
    std::env::remove_var(send::ALLOWLIST_ENV);
    SendEnv { _td: td, token }
}

fn clear_send_env() {
    std::env::remove_var("TEAMS_CLI_COOKIES");
    std::env::remove_var(send::ENABLE_ENV);
}

#[test]
fn send_message_posts_to_mock_chatsvc() {
    let _g = env_lock();
    let env = send_env();
    let (base, seen, h) = mock_chatsvc(201, r#"{"OriginalArrivalTime": 1700000000123}"#, None);
    let res = send::send_message_via(&base, None, "48:notes", "hi <there>\n\nbye", None, false, None, false).unwrap();
    h.join().unwrap();
    assert_eq!(res["sent"], true);
    assert_eq!(res["status"], 201);
    assert_eq!(res["messageId"], "1700000000123");
    assert_eq!(res["region"], "emea");
    let cid = res["clientMessageId"].as_str().unwrap();
    assert_eq!(cid.len(), 19, "{cid}");
    assert!(cid.bytes().all(|b| b.is_ascii_digit()));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let c = &seen[0];
    assert_eq!(c.method, "POST");
    assert_eq!(c.url, "/api/chatsvc/emea/v1/users/ME/conversations/48:notes/messages");
    let hv = |n: &str| {
        c.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(hv("Authentication"), Some(format!("skypetoken={}", env.token)));
    assert_eq!(hv("Content-Type").as_deref(), Some("application/json"));
    assert!(hv("Cookie").is_none());
    let body: Value = serde_json::from_str(&c.body).unwrap();
    assert_eq!(body["content"], "<p>hi &lt;there&gt;</p><p>bye</p>");
    assert_eq!(body["messagetype"], "RichText/Html");
    assert_eq!(body["imdisplayname"], "Ada Sender");
    assert_eq!(body["clientmessageid"], cid);
    clear_send_env();
}

#[test]
fn send_message_reply_and_error_statuses() {
    let _g = env_lock();
    let _env = send_env();

    let (base, seen, h) = mock_chatsvc(201, "{}", None);
    let res = send::send_message_via(
        &base,
        None,
        "19:abc@thread.v2",
        "r",
        Some("1795000000001"),
        true,
        Some("42"),
        false,
    )
    .unwrap();
    h.join().unwrap();
    assert_eq!(res["messageId"], Value::Null);
    assert_eq!(res["clientMessageId"], "42");
    assert_eq!(
        seen.lock().unwrap()[0].url,
        "/api/chatsvc/emea/v1/users/ME/conversations/19:abc@thread.v2;messageid=1795000000001/messages"
    );

    let (base, _, h) = mock_chatsvc(401, "{}", None);
    let err = send::send_message_via(&base, None, "48:notes", "x", None, false, None, false).unwrap_err();
    h.join().unwrap();
    assert!(err.0.contains("Authentication failed (HTTP 401)"), "{err}");

    let (base, _, h) = mock_chatsvc(302, "", Some(("Location", "http://127.0.0.1:1/steal")));
    let err = send::send_message_via(&base, None, "48:notes", "x", None, false, None, false).unwrap_err();
    h.join().unwrap();
    assert!(err.0.contains("refusing to forward credentials"), "{err}");

    let (base, _, h) = mock_chatsvc(500, "oops", None);
    let err = send::send_message_via(&base, None, "48:notes", "x", None, false, None, false).unwrap_err();
    h.join().unwrap();
    assert!(err.0.contains("HTTP 500"), "{err}");

    for bad in [
        "http://evil.example",
        "https://evil.example",
        "ftp://127.0.0.1",
        "not a url",
    ] {
        let err = send::send_message_via(bad, None, "48:notes", "x", None, false, None, false).unwrap_err();
        assert!(err.0.contains("untrusted chat service"), "{bad}: {err}");
    }

    std::env::remove_var(send::ENABLE_ENV);
    let err =
        send::send_message_via("http://127.0.0.1:1", None, "48:notes", "x", None, false, None, false).unwrap_err();
    assert!(err.0.contains("restricted"), "{err}");
    let dry = send::send_message(None, "48:notes", "x", None, false, None, true).unwrap();
    assert_eq!(dry["dryRun"], true);
    assert_eq!(dry["sent"], false);
    clear_send_env();
}

#[test]
fn sync_subprocess_is_tracked_and_refused_after_shutdown() {
    let _g = env_lock();
    let runner: SyncRunner = Arc::new(|_| Ok(json!({})));
    let d = Daemon::start(Some(SyncConfig { runner, interval: None }), false);
    let (code, kind, msg) = daemon::run_sync_subprocess(false).unwrap_err();
    assert_eq!((code, kind.as_str()), (500, "internal"));
    assert!(msg.contains("json"), "{msg}");
    drop(d);
    let (_, _, msg) = daemon::run_sync_subprocess(false).unwrap_err();
    assert!(msg.contains("shutting down"), "{msg}");
}

#[test]
fn http_rejects_malformed_conflicting_and_oversized_content_length() {
    let _g = env_lock();
    let d = Daemon::start(None, false);
    let a = d.addr();
    assert_eq!(
        req_with(a, "GET", "/health", &["Host: 127.0.0.1", "Content-Length: nope"]).0,
        400
    );
    assert_eq!(
        req_with(
            a,
            "GET",
            "/health",
            &["Host: 127.0.0.1", "Content-Length: 1", "Content-Length: 2"]
        )
        .0,
        400
    );
    let mut s = std::net::TcpStream::connect(a).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    std::io::Write::write_all(
        &mut s,
        b"POST /v1/sync HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 99999999\r\nConnection: close\r\n\r\n",
    )
    .unwrap();
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut s, &mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 413 "), "{buf:?}");

    let send_raw = |raw: &[u8]| -> String {
        let mut s = std::net::TcpStream::connect(a).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        std::io::Write::write_all(&mut s, raw).unwrap();
        let mut out = String::new();
        let _ = std::io::Read::read_to_string(&mut s, &mut out);
        out
    };
    assert!(
        send_raw(b"GET /health HTTP/1.1\r\nHost : 127.0.0.1\r\n\r\n").starts_with("HTTP/1.1 400 "),
        "space before colon must be 400"
    );
    assert!(
        send_raw(b"GET /health HTTP/1.1\r\n Host: 127.0.0.1\r\n\r\n").starts_with("HTTP/1.1 400 "),
        "obs-fold leading space must be 400"
    );
    assert!(
        send_raw(b"POST /v1/sync HTTP/1.1\r\nHost: 127.0.0.1\r\nTransfer-Encoding: chunked\r\n\r\n")
            .starts_with("HTTP/1.1 501 "),
        "Transfer-Encoding must be 501"
    );
    assert!(
        send_raw(b"GET /health HTTP/1.1\nHost: 127.0.0.1\n\nHost: evil\r\n\r\n").starts_with("HTTP/1.1 200 "),
        "body bytes after \\n\\n must never be parsed as headers"
    );
}

#[test]
fn fetch_enforces_max_bytes_limit() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let t = thread::spawn(move || {
        if let Ok(req) = server.recv() {
            let body = vec![b'x'; 1024];
            let _ = req.respond(tiny_http::Response::from_data(body));
        }
    });
    let url = format!("http://{addr}/big.bin");
    let err =
        teams_cli::live::fetch::fetch_attachment_with_limit(None, Some(&url), None, None, None, true, 128).unwrap_err();
    assert!(
        err.0.contains("exceeds limit") || err.0.contains("exceeded limit"),
        "{err}"
    );
    let _ = t.join();
}
