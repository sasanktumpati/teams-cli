mod common;

use common::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use teams_cli::daemon::{self, DaemonHandle, SyncConfig, SyncRunner};

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

const NO_AUTH: (&str, &str) = ("X-Test-No-Auth", "");

fn raw_request(addr: SocketAddr, method: &str, target: &str, headers: &[(&str, &str)]) -> Resp {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\nConnection: close\r\nContent-Length: 0\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("Host")) {
        req.push_str("Host: 127.0.0.1\r\n");
    }
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("Authorization") || *k == NO_AUTH.0)
    {
        req.push_str(&format!("Authorization: {}\r\n", bearer()));
    }
    for (k, v) in headers {
        if !k.is_empty() && *k != NO_AUTH.0 {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8(buf).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").expect("http response");
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let body = serde_json::from_str(body).unwrap_or_else(|e| panic!("non-JSON body ({e}): {body:?}"));
    Resp { status, headers, body }
}

fn enc(params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    format!("?{}", ser.finish())
}

struct Fixture {
    _guard: std::sync::MutexGuard<'static, ()>,
    _td: tempfile::TempDir,
    handle: Option<DaemonHandle>,
}

impl Fixture {
    fn new(sync: Option<SyncConfig>) -> Self {
        let guard = env_lock();
        let (td, _home) = home_with_archive();
        let handle = daemon::start(0, sync, false).unwrap();
        Fixture {
            _guard: guard,
            _td: td,
            handle: Some(handle),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.handle.as_ref().unwrap().addr()
    }

    fn req(&self, method: &str, path: &str, params: &[(&str, &str)], headers: &[(&str, &str)]) -> Resp {
        raw_request(self.addr(), method, &format!("{path}{}", enc(params)), headers)
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Value {
        let r = self.req("GET", path, params, &[]);
        assert_eq!(r.status, 200, "{path}: {}", r.body);
        r.body
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

fn ids(v: &Value, key: &str, field: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} missing in {v}"))
        .iter()
        .map(|x| x[field].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn health_status_and_response_headers() {
    let f = Fixture::new(None);
    let r = f.req("GET", "/health", &[], &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, json!({"ok": true, "version": daemon::VERSION}));
    assert_eq!(r.header("Content-Type"), Some("application/json; charset=utf-8"));
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    assert!(r.header("Server").unwrap().starts_with("teams-cli/"));
    let st = f.get("/v1/status", &[]);
    assert_eq!(
        st["archive"]["schemaVersion"],
        teams_cli::storage::archive::SCHEMA_VERSION,
        "{st}"
    );
    assert_eq!(f.get("/health/", &[])["ok"], true);
}

#[test]
fn chats_member() {
    let f = Fixture::new(None);
    let r = f.get("/v1/chats", &[("member", "bob@corp.test")]);
    assert_eq!(ids(&r, "chats", "id"), vec![DM_BOB]);
    assert_eq!(r["memberMatches"][0]["id"], BOB);
}

#[test]
fn messages_around_and_param_errors() {
    let f = Fixture::new(None);
    let r = f.get("/v1/messages", &[("around", MID), ("context", "1")]);
    assert_eq!(r["aroundMessageId"], MID);
    assert_eq!(r["chat"]["id"], GROUP);
    assert_eq!(r["messages"].as_array().unwrap().len(), 3);
    assert_eq!(r["hasBefore"], true);
    assert_eq!(r["hasAfter"], true);

    let r = f.req("GET", "/v1/messages", &[], &[]);
    assert_eq!(r.status, 400);
    assert_eq!(r.body["error"]["code"], "bad_param");
    let r = f.req("GET", "/v1/messages", &[("around", MID), ("context", "x")], &[]);
    assert_eq!(r.status, 400);
    assert!(r.body["error"]["message"].as_str().unwrap().contains("context"));
    let r = f.get("/v1/messages", &[("chat", "Launch"), ("limit", "100"), ("limit", "2")]);
    assert_eq!(r["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn search_history_and_count() {
    let f = Fixture::new(None);
    assert_eq!(f.get("/v1/search", &[("query", "budget")])["matched"], 0);
    let r = f.get(
        "/v1/search",
        &[("query", "budget"), ("includeHistory", "1"), ("countOnly", "1")],
    );
    assert_eq!(r["matched"], 1);
    assert_eq!(r["countOnly"], true);
    assert!(r.get("results").is_none());
    let r = f.get("/v1/search", &[("query", "budget"), ("includeHistory", "true")]);
    assert_eq!(r["results"][0]["id"], EDITED);
    assert_eq!(r["results"][0]["matchedInHistory"], true);
}

#[test]
fn person_route() {
    let f = Fixture::new(None);
    let r = f.get("/v1/person", &[("person", "Ada Lovelace"), ("limit", "1")]);
    assert_eq!(r["person"]["email"], "ada@corp.test");
    assert_eq!(r["sharedChats"].as_array().unwrap().len(), 1);
    let r = f.req("GET", "/v1/person", &[], &[]);
    assert_eq!((r.status, r.body["error"]["code"].as_str()), (400, Some("bad_param")));
    assert_eq!(
        f.req("GET", "/v1/person", &[("person", "nobody-here")], &[]).status,
        404
    );
}

#[test]
fn views_routes() {
    let f = Fixture::new(None);
    let r = f.get("/v1/inbox", &[("since", "30d")]);
    assert_eq!(r["since"], "30d");
    assert_eq!(r["counts"]["mentions"], 1);
    let r = f.get("/v1/mentions", &[("unreadOnly", "1")]);
    assert_eq!(ids(&r, "mentions", "messageId"), vec!["1795000000009"]);
    assert_eq!(f.get("/v1/mentions", &[("chat", "Bob")])["matched"], 0);
    let r = f.get("/v1/calls", &[("missedOnly", "1"), ("person", "bob")]);
    assert_eq!(ids(&r, "calls", "id"), vec!["c2"]);
    assert_eq!(f.get("/v1/calls", &[])["matched"], 2);
    assert_eq!(
        ids(&f.get("/v1/meetings", &[("query", "sprint")]), "meetings", "id"),
        vec!["m1"]
    );
    assert_eq!(f.get("/v1/saved", &[("includeUnsaved", "1")])["matched"], 1);
    let r = f.get("/v1/pinned", &[("chat", "Launch")]);
    assert_eq!(r["chat"]["id"], GROUP);
    assert_eq!(r["pinned"][0]["subject"], "Rota");
}

#[test]
fn records_changes_attachments_people() {
    let f = Fixture::new(None);
    let srcs = f.get("/v1/records", &[]);
    let names: Vec<&str> = srcs["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["source"].as_str())
        .collect();
    assert!(names.contains(&CALLS), "{srcs}");
    let r = f.get("/v1/records", &[("source", CALLS), ("key", "c1")]);
    assert_eq!(r["records"][0]["key"], "c1");
    let r = f.get("/v1/changes", &[("since", "all")]);
    assert!(ids(&r, "changes", "id").contains(&EDITED.to_string()), "{r}");
    assert!(f.get("/v1/attachments", &[]).get("attachments").is_some());
    let r = f.get("/v1/people", &[("query", "ada")]);
    assert_eq!(ids(&r, "people", "id"), vec![ADA]);
}

#[test]
fn since_all_clears_default_window() {
    let f = Fixture::new(None);
    assert_eq!(f.get("/v1/inbox", &[])["since"], "7d");
    assert_eq!(f.get("/v1/inbox", &[("since", "")])["since"], Value::Null);
    assert_eq!(f.get("/v1/inbox", &[("since", "all")])["since"], Value::Null);
    assert_eq!(f.get("/v1/inbox", &[("since", "30d")])["since"], "30d");
}

#[test]
fn unknown_routes_and_methods() {
    let f = Fixture::new(None);
    let r = f.req("GET", "/v1/nope", &[], &[]);
    assert_eq!((r.status, r.body["error"]["code"].as_str()), (404, Some("not_found")));
    assert_eq!(f.req("POST", "/v1/chats", &[], &[]).status, 404);
    assert_eq!(f.req("GET", "/v1/sync", &[], &[]).status, 404);
    let r = f.req("POST", "/v1/sync", &[], &[]);
    assert_eq!((r.status, r.body["error"]["code"].as_str()), (503, Some("no_syncer")));
}

#[test]
fn rejects_non_local_clients() {
    let f = Fixture::new(None);
    let cases: &[&[(&str, &str)]] = &[
        &[("Host", "evil.example:47613")],
        &[("Host", "127.0.0.1"), ("Origin", "https://evil.example")],
        &[("Host", "localhost"), ("Origin", "null")],
        &[("Host", "127.0.0.1.evil.example")],
        &[("Host", "")],
        &[("Host", "evil.example"), ("Host", "127.0.0.1")],
    ];
    for headers in cases {
        for path in ["/v1/chats", "/health"] {
            let r = f.req("GET", path, &[], headers);
            assert_eq!(r.status, 403, "{headers:?} {path}");
            assert_eq!(r.body["error"]["code"], "forbidden");
        }
        assert_eq!(f.req("POST", "/v1/sync", &[], headers).status, 403, "{headers:?}");
    }
}

#[test]
fn allows_loopback_hosts() {
    let f = Fixture::new(None);
    let port_host = format!("localhost:{}", f.addr().port());
    for host in [
        "127.0.0.1",
        port_host.as_str(),
        "[::1]:1",
        "[::1]",
        "LOCALHOST",
        " 127.0.0.1 ",
    ] {
        assert_eq!(f.req("GET", "/health", &[], &[("Host", host)]).status, 200, "{host}");
    }
    assert_eq!(f.req("GET", "/health", &[], &[("Host", "[::1")]).status, 403);
    assert_eq!(
        f.req("GET", "/health", &[], &[("Host", "[::1].evil.com:80")]).status,
        403
    );
    assert_eq!(f.req("GET", "/health", &[], &[("Origin", "")]).status, 200);
}

#[test]
fn sec_fetch_site() {
    let f = Fixture::new(None);
    for (site, status) in [
        ("cross-site", 403),
        ("same-site", 403),
        ("same-origin", 403),
        ("none", 200),
        ("NONE", 200),
        ("", 200),
    ] {
        let r = f.req("GET", "/v1/chats", &[], &[("Sec-Fetch-Site", site)]);
        assert_eq!(r.status, status, "{site:?}");
    }
}

#[test]
fn post_sync_with_syncer_ok_and_busy() {
    let busy = Arc::new(Mutex::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (b, c) = (Arc::clone(&busy), Arc::clone(&calls));
    let runner: SyncRunner = Arc::new(move |_notify| {
        c.fetch_add(1, Ordering::SeqCst);
        if *b.lock().unwrap() {
            Err((409, "sync_busy".into(), "Another sync is still running.".into()))
        } else {
            Ok(json!({"messages_new": 3}))
        }
    });
    let f = Fixture::new(Some(SyncConfig { runner, interval: None }));
    let r = f.req("POST", "/v1/sync", &[], &[]);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, json!({"ok": true, "stats": {"messages_new": 3}}));
    *busy.lock().unwrap() = true;
    let r = f.req("POST", "/v1/sync", &[], &[]);
    assert_eq!((r.status, r.body["error"]["code"].as_str()), (409, Some("sync_busy")));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn periodic_sync_runs_with_notify_and_shutdown_stops_it() {
    let runs = Arc::new(Mutex::new(Vec::new()));
    let r2 = Arc::clone(&runs);
    let runner: SyncRunner = Arc::new(move |notify| {
        r2.lock().unwrap().push(notify);
        Ok(json!({}))
    });
    let mut f = Fixture::new(Some(SyncConfig {
        runner,
        interval: Some(3600),
    }));
    for _ in 0..200 {
        if !runs.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let started = std::time::Instant::now();
    f.handle.take().unwrap().shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown must wake the sync loop"
    );
    assert_eq!(*runs.lock().unwrap(), vec![true]);
}

#[test]
fn missing_archive_is_a_json_error_not_a_crash() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let h = daemon::start(0, None, false).unwrap();
    assert_eq!(raw_request(h.addr(), "GET", "/health", &[]).status, 200);
    let r = raw_request(h.addr(), "GET", "/v1/chats", &[]);
    assert!(r.status >= 400, "{}", r.body);
    assert!(r.body["error"]["code"].is_string());
    h.shutdown();
}

#[test]
fn a_panicking_handler_answers_500_and_the_daemon_keeps_serving() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let runner: SyncRunner = Arc::new(move |_| {
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("boom in sync runner");
        }
        Ok(json!({"messages_new": 0}))
    });
    let f = Fixture::new(Some(SyncConfig { runner, interval: None }));
    let r = f.req("POST", "/v1/sync", &[], &[]);
    assert_eq!(r.status, 500);
    assert_eq!(r.body["error"]["code"], "internal");
    assert!(r.body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("boom in sync runner"));
    assert_eq!(f.get("/health", &[])["ok"], true);
    assert_eq!(f.req("POST", "/v1/sync", &[], &[]).status, 200);
}

#[test]
fn v1_routes_require_a_single_valid_bearer_token() {
    let f = Fixture::new(Some(SyncConfig {
        runner: Arc::new(|_| Ok(json!({}))),
        interval: None,
    }));
    let good = bearer();
    let bad_cases: Vec<Vec<(&str, &str)>> = vec![
        vec![NO_AUTH],
        vec![("Authorization", "")],
        vec![("Authorization", "Bearer")],
        vec![("Authorization", "Bearer ")],
        vec![("Authorization", "Basic dXNlcjpwYXNz")],
        vec![("Authorization", "Bearer not-the-token")],
        vec![("Authorization", &good[..good.len() - 1])],
        vec![("Authorization", good.as_str()), ("Authorization", "Bearer bogus")],
        vec![("Authorization", "Bearer bogus"), ("Authorization", good.as_str())],
        vec![("Authorization", good.as_str()), ("Authorization", good.as_str())],
    ];
    for headers in &bad_cases {
        for (method, path) in [
            ("GET", "/v1/status"),
            ("GET", "/v1/chats"),
            ("POST", "/v1/sync"),
            ("GET", "/v1/nope"),
        ] {
            let r = f.req(method, path, &[], headers);
            assert_eq!(r.status, 401, "{method} {path} {headers:?}: {}", r.body);
            assert_eq!(r.header("WWW-Authenticate"), Some("Bearer"), "{headers:?}");
            assert_eq!(r.body["error"]["code"], "unauthorized");
            assert!(r.body["error"]["message"].as_str().unwrap().contains("teams token"));
        }
    }
    for headers in [vec![NO_AUTH], vec![("Authorization", "Bearer wrong")]] {
        let r = f.req("GET", "/health", &[], &headers);
        assert_eq!(r.status, 200);
        assert_eq!(r.body, json!({"ok": true, "version": daemon::VERSION}));
    }
    let token = api_token();
    for scheme in ["Bearer", "bearer", "BEARER"] {
        let v = format!("{scheme} {token}");
        assert_eq!(
            f.req("GET", "/v1/status", &[], &[("Authorization", &v)]).status,
            200,
            "{scheme}"
        );
    }
    assert_eq!(f.req("POST", "/v1/sync", &[], &[]).status, 200);
    let r = f.req("GET", "/v1/status", &[], &[("Origin", "https://evil.example")]);
    assert_eq!(r.status, 403);
}

fn teams_token(home: &std::path::Path, args: &[&str]) -> (String, String) {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .arg("token")
        .args(args)
        .env("TEAMS_CLI_HOME", home)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    (
        String::from_utf8(o.stdout).unwrap(),
        String::from_utf8(o.stderr).unwrap(),
    )
}

#[test]
fn only_named_tokens_are_accepted() {
    use std::os::unix::fs::PermissionsExt;
    let _g = env_lock();
    let (td, home) = home_with_archive();
    let legacy_path = home.join("api.token");
    std::fs::write(&legacy_path, "legacy-plaintext-secret\n").unwrap();

    let bare = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .arg("token")
        .env("TEAMS_CLI_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(bare.status.code(), Some(2));

    let h = daemon::start(0, None, false).unwrap();
    let call_status = |tok: &str| {
        raw_request(
            h.addr(),
            "GET",
            "/v1/status",
            &[("Authorization", &format!("Bearer {tok}"))],
        )
    };

    assert_eq!(call_status("legacy-plaintext-secret").status, 401);

    std::env::set_var("TEAMS_CLI_API_TOKEN", "env-secret-token");
    let env_res = call_status("env-secret-token");
    std::env::remove_var("TEAMS_CLI_API_TOKEN");
    assert_eq!(env_res.status, 401);

    let (out, _) = teams_token(&home, &["create", "script", "--json"]);
    let secret = serde_json::from_str::<Value>(&out).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(call_status(&secret).status, 200);

    let store_path = home.join("api.tokens.json");
    std::fs::set_permissions(&store_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(call_status(&secret).status, 200);
    assert_eq!(
        std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    std::fs::write(&store_path, "{corrupt json").unwrap();
    assert_eq!(call_status(&secret).status, 500);

    std::fs::remove_file(&store_path).unwrap();
    assert_eq!(call_status(&secret).status, 401);

    h.shutdown();
    drop(td);
}

#[test]
fn missing_archive_answers_409_empty_archive() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let h = daemon::start(0, None, false).unwrap();
    assert!(!td.path().join("api.token").exists());
    for path in ["/v1/status", "/v1/chats"] {
        let r = raw_request(h.addr(), "GET", path, &[]);
        assert_eq!(
            (r.status, r.body["error"]["code"].as_str()),
            (409, Some("empty_archive")),
            "{path}"
        );
    }
    h.shutdown();
    assert!(!td.path().join("archive.sqlite3").exists());
    assert!(!td.path().join("api.token").exists());
}

#[test]
fn bearer_token_parsing() {
    use daemon::bearer_token;
    assert_eq!(bearer_token(&["Bearer abc"]), Some("abc"));
    assert_eq!(bearer_token(&["bearer   abc  "]), Some("abc"));
    assert_eq!(bearer_token(&["BeArEr\tabc"]), Some("abc"));
    for bad in [
        "",
        "Bearer",
        "Bearer ",
        "Bearerabc",
        "Basic abc",
        "Bearer a b",
        "Token abc",
        "Bearé abc",
    ] {
        assert_eq!(bearer_token(&[bad]), None, "{bad:?}");
    }
    assert_eq!(bearer_token(&[]), None);
    assert_eq!(bearer_token(&["Bearer abc", "Bearer abc"]), None);
}

#[test]
fn named_tokens_create_expiry_revoke_and_prune_live() {
    use std::os::unix::fs::PermissionsExt;
    let _g = env_lock();
    let (td, home) = home_with_archive();
    let h = daemon::start(0, None, false).unwrap();
    assert!(!home.join("api.token").exists());

    let call_status = |tok: &str| {
        raw_request(
            h.addr(),
            "GET",
            "/v1/status",
            &[("Authorization", &format!("Bearer {tok}"))],
        )
    };

    let (out, _) = teams_token(&home, &["create", "ci-bot", "--expires", "30d", "--json"]);
    let created: Value = serde_json::from_str(&out).unwrap();
    let secret = created["token"].as_str().unwrap().to_string();
    let id = created["id"].as_str().unwrap().to_string();
    assert!(secret.starts_with("tc_"), "{secret}");
    assert!(created["expiresAt"].is_string());

    let store_path = home.join("api.tokens.json");
    assert_eq!(
        std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let raw_store = std::fs::read_to_string(&store_path).unwrap();
    assert!(!raw_store.contains(&secret), "secret must never be stored in plaintext");

    assert_eq!(call_status(&secret).status, 200);

    let dup = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .args(["token", "create", "ci-bot"])
        .env("TEAMS_CLI_HOME", &home)
        .output()
        .unwrap();
    assert!(!dup.status.success());

    let (list_out, _) = teams_token(&home, &["list", "--json"]);
    assert!(!list_out.contains(&secret));
    assert!(!list_out.contains("sha256"));
    let listed: Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(listed["count"], 1);
    assert_eq!(listed["tokens"][0]["name"], "ci-bot");

    let (rev_out, _) = teams_token(&home, &["revoke", &id, "--json"]);
    let rev: Value = serde_json::from_str(&rev_out).unwrap();
    assert_eq!(rev["revoked"], true);
    assert_eq!(call_status(&secret).status, 401);

    let (exp_out, _) = teams_token(&home, &["create", "short-lived", "--expires", "30m", "--json"]);
    let exp_tok = serde_json::from_str::<Value>(&exp_out).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(call_status(&exp_tok).status, 200);
    let mut store_val: Value = serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    store_val["tokens"][0]["expires_at_ms"] = json!(1_000);
    std::fs::write(&store_path, serde_json::to_string_pretty(&store_val).unwrap()).unwrap();
    assert_eq!(call_status(&exp_tok).status, 401);
    assert_eq!(
        serde_json::from_str::<Value>(&teams_token(&home, &["list", "--json"]).0).unwrap()["count"],
        0
    );
    assert_eq!(
        serde_json::from_str::<Value>(&teams_token(&home, &["list", "--include-expired", "--json"]).0).unwrap()
            ["count"],
        1
    );
    let pruned: Value = serde_json::from_str(&teams_token(&home, &["prune", "--json"]).0).unwrap();
    assert_eq!(pruned["pruned"], 1);
    assert_eq!(pruned["remaining"], 0);

    let hex_name = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .args(["token", "create", "deadbeef"])
        .env("TEAMS_CLI_HOME", &home)
        .output()
        .unwrap();
    assert!(!hex_name.status.success());

    let huge_exp = std::process::Command::new(env!("CARGO_BIN_EXE_teams"))
        .args(["token", "create", "far-future", "--expires", "200y"])
        .env("TEAMS_CLI_HOME", &home)
        .output()
        .unwrap();
    assert!(!huge_exp.status.success());

    let (re1, _) = teams_token(&home, &["create", "reused", "--expires", "30m", "--json"]);
    let _ = re1;
    let mut sv: Value = serde_json::from_str(&std::fs::read_to_string(&store_path).unwrap()).unwrap();
    sv["tokens"][0]["expires_at_ms"] = json!(1_000);
    std::fs::write(&store_path, serde_json::to_string_pretty(&sv).unwrap()).unwrap();
    let (re2, _) = teams_token(&home, &["create", "reused", "--expires", "30m", "--json"]);
    let re2_tok = serde_json::from_str::<Value>(&re2).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(call_status(&re2_tok).status, 200);
    let _ = teams_token(&home, &["revoke", "reused", "--json"]);
    assert_eq!(call_status(&re2_tok).status, 401);

    h.shutdown();
    drop(td);
}

#[test]
fn concurrent_post_sync_returns_409_busy_immediately() {
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let entered_tx = Mutex::new(Some(entered_tx));
    let release_rx = Mutex::new(release_rx);
    let f = Fixture::new(Some(SyncConfig {
        runner: Arc::new(move |_| {
            if let Some(tx) = entered_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let _ = release_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));
            Ok(json!({"messages_new": 1}))
        }),
        interval: None,
    }));
    let addr = f.addr();
    let t1 = std::thread::spawn(move || raw_request(addr, "POST", "/v1/sync", &[]).status);
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first sync did not start in time");
    let t2 = raw_request(addr, "POST", "/v1/sync", &[]);
    assert_eq!((t2.status, t2.body["error"]["code"].as_str()), (409, Some("sync_busy")));
    let _ = release_tx.send(());
    assert_eq!(t1.join().unwrap(), 200);
}

#[test]
fn daemon_returns_503_migrating_instead_of_upgrading_in_worker() {
    let _g = env_lock();
    let td = tempfile::tempdir().unwrap();
    std::env::set_var("TEAMS_CLI_HOME", td.path());
    let db_path = td.path().join("archive.sqlite3");
    {
        let c = rusqlite::Connection::open(&db_path).unwrap();
        teams_cli::storage::archive::create_schema_at(&c, 7).unwrap();
    }
    let h = daemon::start(0, None, false).unwrap();
    let r = raw_request(h.addr(), "GET", "/v1/status", &[]);
    assert_eq!((r.status, r.body["error"]["code"].as_str()), (503, Some("migrating")));
    let ver: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 7);
    h.shutdown();
}
