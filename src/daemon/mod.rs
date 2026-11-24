pub mod auth;
mod http;
pub mod pool;
pub mod sync_loop;

pub use auth::{
    bearer_token, create_named_token, env_api_token, extract_host, is_allowed, is_allowed_headers, list_named_tokens,
    load_or_create_api_token, peek_api_token, prune_expired_tokens, revoke_named_token, rotate_api_token,
    API_TOKEN_ENV,
};
pub use pool::{FileSig, PoolAcquireError, ReadConnPool, IDLE_CONN_TTL};
pub use sync_loop::{recover_interrupted_sync_runs, run_sync_subprocess, SyncConfig, SyncFailure, SyncRunner};

use crate::json_codec;
use crate::paths;
use crate::query::api::{self, ApiError};
use crate::query::views;
use auth::{ApiToken, UNAUTHORIZED_MESSAGE};
use chrono::Local;
use http::{HttpRequest, LoopbackListener};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TrySendError};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sync_loop::{ensure_migrated_in_subprocess, terminate_active_sync_child, SyncCoordinator, SYNC_CANCELLED};
use url::Url;

pub const VERSION: &str = crate::VERSION;
pub const HTTP_WORKERS: usize = 4;
pub const HTTP_QUEUE_DEPTH: usize = 64;
const WORKER_STACK_SIZE: usize = 512 * 1024;

static STOP_SIGNAL: LazyLock<(Mutex<bool>, Condvar)> = LazyLock::new(|| (Mutex::new(false), Condvar::new()));
static SIGNAL_INSTALLED: AtomicBool = AtomicBool::new(false);
static LOG_STATE: Mutex<Option<(Option<OsString>, PathBuf)>> = Mutex::new(None);

pub(crate) fn log_line(to_file: bool, level: &str, msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
    let line = format!("{ts} {level} teamscli.daemon: {msg}\n");
    if to_file {
        let mut state = LOG_STATE.lock().unwrap_or_else(|p| p.into_inner());
        let key = env::var_os("TEAMS_CLI_HOME").filter(|v| !v.is_empty());
        let dir = match state.as_ref() {
            Some((k, d)) if *k == key => d.clone(),
            _ => {
                let d = paths::log_dir();
                *state = Some((key, d.clone()));
                d
            }
        };
        let log_path = dir.join("daemon.log");
        if log_path.metadata().map(|m| m.len()).unwrap_or(0) > 2_000_000 {
            for i in (1..=3).rev() {
                let src = if i == 1 {
                    log_path.clone()
                } else {
                    dir.join(format!("daemon.log.{}", i - 1))
                };
                let dst = dir.join(format!("daemon.log.{i}"));
                let _ = fs::rename(&src, &dst);
            }
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path) {
            let _ = f.write_all(line.as_bytes());
        }
    } else {
        eprint!("{line}");
    }
}

pub fn since_param<'a>(q: &'a HashMap<String, String>, default: Option<&'a str>) -> Option<&'a str> {
    match q.get("since") {
        None => default,
        Some(v) => {
            let t = v.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("all") {
                None
            } else {
                Some(v.as_str())
            }
        }
    }
}

fn send_json(req: HttpRequest, status: u16, body: &Value) {
    let data = json_codec::to_string(body).into_bytes();
    let srv = format!("teams-cli/{VERSION}");
    let headers = [
        ("Content-Type", "application/json; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Server", srv.as_str()),
    ];
    req.respond(status, &headers, &data);
}

fn send_unauthorized(req: HttpRequest) {
    let body = json_codec::to_string(&json!({
        "error": {"code": "unauthorized", "message": UNAUTHORIZED_MESSAGE}
    }))
    .into_bytes();
    let srv = format!("teams-cli/{VERSION}");
    let headers = [
        ("Content-Type", "application/json; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Server", srv.as_str()),
        ("WWW-Authenticate", "Bearer"),
    ];
    req.respond(401, &headers, &body);
}

fn check_allowed(req: &HttpRequest) -> bool {
    is_allowed_headers(
        &req.header_values("Host"),
        &req.header_values("Origin"),
        &req.header_values("Sec-Fetch-Site"),
    )
}

fn route(
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    pool: &ReadConnPool,
    syncer: Option<&Arc<SyncCoordinator>>,
) -> Result<Value, (u16, Value)> {
    if path == "/health" && matches!(method, "GET" | "HEAD") {
        return Ok(json!({"ok": true, "version": VERSION}));
    }
    if method == "POST" && path == "/v1/sync" {
        let Some(s) = syncer else {
            let e = ApiError::with_status("no_syncer", "This server does not run syncs.", 503);
            return Err((e.status, e.to_json()));
        };
        return match s.sync_now(false, true) {
            Ok(stats) => Ok(json!({"ok": true, "stats": stats})),
            Err((code, kind, msg)) => {
                let e = ApiError::with_status(&kind, msg, code);
                Err((e.status, e.to_json()))
            }
        };
    }
    if method != "GET" {
        let e = ApiError::with_status("not_found", format!("No route {method} {path}"), 404);
        return Err((e.status, e.to_json()));
    }
    let (conn, sig) = pool.acquire().map_err(|e| match e {
        PoolAcquireError::MissingArchive(msg) => {
            let ae = ApiError::with_status("empty_archive", msg, 409);
            (ae.status, ae.to_json())
        }
        PoolAcquireError::Migrating(msg) => {
            let ae = ApiError::with_status("migrating", msg, 503);
            (ae.status, ae.to_json())
        }
        PoolAcquireError::Other(msg) => {
            let ae = ApiError::with_status("internal", msg, 500);
            (ae.status, ae.to_json())
        }
    })?;
    let known_populated = pool.is_known_populated(sig.as_ref());
    let res = dispatch_get(&conn, path, q, known_populated);
    if res.is_ok() && !known_populated && path != "/v1/status" {
        pool.mark_populated(sig.as_ref());
    }
    pool.release(conn, sig);
    res.map_err(|e| (e.status, e.to_json()))
}

fn dispatch_get(
    conn: &Connection,
    path: &str,
    q: &HashMap<String, String>,
    known_populated: bool,
) -> Result<Value, ApiError> {
    let gs = |k: &str| q.get(k).map(String::as_str);
    if path == "/v1/status" {
        return api::status(conn);
    }
    if !known_populated {
        api::require_data(conn)?;
    }
    match path {
        "/v1/chats" => api::list_chats(
            conn,
            gs("query"),
            gs("kind"),
            api::as_int(gs("limit"), 50, "limit")?,
            api::as_bool(gs("includeSystem")),
            gs("member"),
        ),
        "/v1/messages" => {
            if gs("chat").filter(|s| !s.is_empty()).is_none() && gs("around").filter(|s| !s.is_empty()).is_none() {
                return Err(ApiError::bad("bad_param", "chat or around is required"));
            }
            api::read_messages(
                conn,
                gs("chat"),
                since_param(q, None),
                gs("until"),
                gs("sender"),
                api::as_int(gs("limit"), 200, "limit")?,
                api::as_bool(gs("includeEvents")),
                gs("thread"),
                api::as_bool(gs("includeHistory")),
                gs("around"),
                api::as_int(gs("context"), 10, "context")?,
            )
        }
        "/v1/changes" => api::changes(
            conn,
            since_param(q, Some("7d")),
            gs("until"),
            gs("chat"),
            gs("what").filter(|s| !s.is_empty()).unwrap_or("all"),
            api::as_int(gs("limit"), 100, "limit")?,
        ),
        "/v1/records" => match gs("source").filter(|s| !s.is_empty()) {
            None => api::record_sources(conn),
            Some(src) => api::records(
                conn,
                src,
                gs("key"),
                since_param(q, None),
                api::as_int(gs("limit"), 50, "limit")?,
                api::as_bool(gs("allVersions")),
            ),
        },
        "/v1/search" => api::search_with_options(
            conn,
            Some(gs("query").unwrap_or("")),
            gs("chat"),
            gs("sender"),
            since_param(q, None),
            gs("until"),
            api::as_int(gs("limit"), 30, "limit")?,
            gs("sort").filter(|s| !s.is_empty()).unwrap_or("recent"),
            api::as_bool(gs("includeEvents")),
            api::as_bool(gs("includeHistory")),
            api::as_bool(gs("countOnly")),
            api::as_bool(gs("hasAttachments")),
        ),
        "/v1/attachments" => api::attachments(
            conn,
            gs("query"),
            gs("chat"),
            gs("sender"),
            gs("kind"),
            gs("type"),
            since_param(q, None),
            gs("until"),
            api::as_int(gs("limit"), 50, "limit")?,
        ),
        "/v1/people" => api::people(
            conn,
            gs("query"),
            api::as_int(gs("limit"), 50, "limit")?,
            api::as_bool(gs("includeBots")),
        ),
        "/v1/person" => {
            let p = gs("person")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ApiError::bad("bad_param", "person is required"))?;
            api::person(conn, p, api::as_int(gs("limit"), 20, "limit")?)
        }
        "/v1/inbox" => views::inbox(conn, since_param(q, Some("7d")), api::as_int(gs("limit"), 30, "limit")?),
        "/v1/mentions" => views::mentions(
            conn,
            since_param(q, None),
            gs("until"),
            gs("chat"),
            api::as_bool(gs("unreadOnly")),
            api::as_int(gs("limit"), 50, "limit")?,
        ),
        "/v1/calls" => views::calls(
            conn,
            since_param(q, None),
            gs("until"),
            gs("person"),
            api::as_bool(gs("missedOnly")),
            api::as_int(gs("limit"), 50, "limit")?,
        ),
        "/v1/meetings" => views::meetings(
            conn,
            since_param(q, None),
            gs("until"),
            gs("query"),
            api::as_int(gs("limit"), 50, "limit")?,
        ),
        "/v1/saved" => views::saved(
            conn,
            api::as_bool(gs("includeUnsaved")),
            api::as_int(gs("limit"), 50, "limit")?,
        ),
        "/v1/pinned" => views::pinned(conn, gs("chat"), api::as_int(gs("limit"), 50, "limit")?),
        _ => Err(ApiError::with_status("not_found", format!("No route GET {path}"), 404)),
    }
}

fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let parsed = if target.starts_with('/') {
        Url::parse(&format!("http://127.0.0.1{target}"))
    } else {
        Url::parse(target).or_else(|_| Url::parse(&format!("http://127.0.0.1/{target}")))
    };
    match parsed {
        Ok(u) => {
            let p = u.path().trim_end_matches('/');
            let p = if p.is_empty() { "/".to_string() } else { p.to_string() };
            let mut map = HashMap::new();
            for (k, v) in u.query_pairs() {
                map.insert(k.into_owned(), v.into_owned());
            }
            (p, map)
        }
        Err(_) => ("/".to_string(), HashMap::new()),
    }
}

fn handle(
    req: HttpRequest,
    pool: &ReadConnPool,
    syncer: Option<&Arc<SyncCoordinator>>,
    token: &ApiToken,
    to_file: bool,
) {
    if !check_allowed(&req) {
        let e = ApiError::with_status("forbidden", "Local clients only.", 403);
        send_json(req, 403, &e.to_json());
        return;
    }
    let (path, q) = parse_target(&req.target);
    let method = req.method.clone();
    if path != "/health" {
        let Some(candidate) = bearer_token(&req.header_values("Authorization")) else {
            send_unauthorized(req);
            return;
        };
        match token.verify(candidate) {
            Ok(true) => {}
            Ok(false) => {
                send_unauthorized(req);
                return;
            }
            Err(msg) => {
                log_line(to_file, "ERROR", &format!("API token unavailable: {msg}"));
                let e = ApiError::with_status("internal", format!("API token unavailable: {msg}"), 500);
                send_json(req, 500, &e.to_json());
                return;
            }
        }
    }
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| route(&method, &path, &q, pool, syncer)));
    match outcome {
        Ok(Ok(body)) => send_json(req, 200, &body),
        Ok(Err((code, body))) => send_json(req, code, &body),
        Err(payload) => {
            let what = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            log_line(to_file, "ERROR", &format!("handler panic on {method} {path}: {what}"));
            let e = ApiError::with_status("internal", format!("panic: {what}"), 500);
            send_json(req, 500, &e.to_json());
        }
    }
}

pub struct DaemonHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    pool: Arc<ReadConnPool>,
    syncer: Option<Arc<SyncCoordinator>>,
    threads: Vec<thread::JoinHandle<()>>,
    to_file: bool,
}

impl DaemonHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        LoopbackListener::wake(self.addr);
        if let Some(ref s) = self.syncer {
            SYNC_CANCELLED.store(true, Ordering::SeqCst);
            s.wake();
            if terminate_active_sync_child() {
                log_line(self.to_file, "INFO", "terminated running sync worker");
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        for t in self.threads.drain(..) {
            while !t.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(15));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
        self.pool.clear();
        log_line(self.to_file, "INFO", "teams-cli stopped");
    }
}

pub fn start(port: u16, sync: Option<SyncConfig>, log_to_file: bool) -> Result<DaemonHandle, String> {
    start_inner(port, sync, log_to_file, false)
}

fn start_inner(
    port: u16,
    sync: Option<SyncConfig>,
    log_to_file: bool,
    fast_change_check: bool,
) -> Result<DaemonHandle, String> {
    let listener = LoopbackListener::bind(port).map_err(|e| format!("failed binding 127.0.0.1:{port}: {e}"))?;
    let bound = listener.addr();
    let token = Arc::new(ApiToken::default());
    token
        .ensure_ready()
        .map_err(|e| format!("failed loading the API token: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(ReadConnPool::new(2));
    if sync.is_some() {
        SYNC_CANCELLED.store(false, Ordering::SeqCst);
        let n = recover_interrupted_sync_runs();
        if n > 0 {
            log_line(
                log_to_file,
                "WARNING",
                &format!("marked {n} interrupted sync run(s) as error"),
            );
        }
    }

    let syncer = sync.as_ref().map(|cfg| {
        Arc::new(SyncCoordinator::new(
            Arc::clone(&cfg.runner),
            log_to_file,
            fast_change_check,
        ))
    });

    let stop_http = Arc::clone(&stop);
    let syncer_http = syncer.clone();
    let pool_http = Arc::clone(&pool);
    let (tx, rx) = mpsc::sync_channel::<TcpStream>(HTTP_QUEUE_DEPTH);
    let rx = Arc::new(Mutex::new(rx));
    let mut workers = Vec::with_capacity(HTTP_WORKERS);
    for i in 0..HTTP_WORKERS {
        let rx_w = Arc::clone(&rx);
        let pool_w = Arc::clone(&pool_http);
        let token_w = Arc::clone(&token);
        let syncer_w = syncer_http.clone();
        match thread::Builder::new()
            .name(format!("teams-http-{i}"))
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || loop {
                let next = rx_w.lock().unwrap_or_else(|p| p.into_inner()).recv();
                match next {
                    Ok(stream) => {
                        if let Ok(Some(req)) = http::read_request(stream) {
                            handle(req, &pool_w, syncer_w.as_ref(), &token_w, log_to_file);
                        }
                    }
                    Err(_) => break,
                }
            }) {
            Ok(h) => workers.push(h),
            Err(e) => {
                drop(tx);
                for w in workers {
                    let _ = w.join();
                }
                return Err(format!("failed spawning HTTP worker: {e}"));
            }
        }
    }

    let mut threads = Vec::with_capacity(2);
    let accept_thread = thread::Builder::new()
        .name("teams-accept".to_string())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || {
            while !stop_http.load(Ordering::SeqCst) {
                let stream = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => {
                        if stop_http.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };
                if stop_http.load(Ordering::SeqCst) {
                    drop(stream);
                    break;
                }
                match tx.try_send(stream) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut stream)) => {
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                        let e = ApiError::with_status("busy", "Server is busy; retry shortly.", 503);
                        let body = json_codec::to_string(&e.to_json());
                        let resp = format!(
                            "HTTP/1.1 503 Service Unavailable\r\n\
                             Content-Type: application/json; charset=utf-8\r\n\
                             Cache-Control: no-store\r\n\
                             Server: teams-cli/{VERSION}\r\n\
                             Content-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            drop(tx);
            for w in workers {
                let _ = w.join();
            }
        })
        .map_err(|e| format!("failed spawning accept thread: {e}"))?;
    threads.push(accept_thread);

    if let (Some(cfg), Some(s)) = (sync, syncer.as_ref()) {
        if let Some(interval) = cfg.interval {
            let bg = Arc::clone(s);
            let stop_bg = Arc::clone(&stop);
            let pool_bg = Arc::clone(&pool);
            let sync_interval = Duration::from_secs(interval.max(1));
            match thread::Builder::new()
                .name("teams-sync".to_string())
                .stack_size(WORKER_STACK_SIZE)
                .spawn(move || {
                    let mut next_sync = Instant::now();
                    while !stop_bg.load(Ordering::SeqCst) {
                        let now = Instant::now();
                        if now >= next_sync {
                            let delay = bg.periodic_tick(sync_interval);
                            pool_bg.reap_idle();
                            next_sync = Instant::now() + delay;
                        } else {
                            pool_bg.reap_idle();
                        }
                        if stop_bg.load(Ordering::SeqCst) {
                            break;
                        }
                        let until_sync = next_sync.saturating_duration_since(Instant::now());
                        let sleep_for = if pool_bg.has_idle() {
                            until_sync.min(IDLE_CONN_TTL)
                        } else {
                            until_sync
                        };
                        bg.wait_for(sleep_for, &stop_bg);
                    }
                }) {
                Ok(t) => threads.push(t),
                Err(e) => {
                    stop.store(true, Ordering::SeqCst);
                    LoopbackListener::wake(bound);
                    for t in threads {
                        let _ = t.join();
                    }
                    return Err(format!("failed spawning sync loop thread: {e}"));
                }
            }
        }
    }

    Ok(DaemonHandle {
        addr: bound,
        stop,
        pool,
        syncer,
        threads,
        to_file: log_to_file,
    })
}

pub fn serve(port_opt: Option<u16>, interval: u64, sync: bool, log_to_file: bool) -> Result<(), String> {
    {
        let (m, _) = &*STOP_SIGNAL;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }
    if !SIGNAL_INSTALLED.swap(true, Ordering::SeqCst) {
        let _ = ctrlc::set_handler(|| {
            let (m, cv) = &*STOP_SIGNAL;
            *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
            cv.notify_all();
        });
    }
    ensure_migrated_in_subprocess();
    let port = port_opt.unwrap_or_else(paths::port);
    let cfg = sync.then(|| SyncConfig {
        runner: Arc::new(run_sync_subprocess) as SyncRunner,
        interval: Some(interval),
    });
    let handle = start_inner(port, cfg, log_to_file, true)?;
    log_line(
        log_to_file,
        "INFO",
        &format!(
            "teams-cli {VERSION} listening on http://{} (refresh check every {interval}s)",
            handle.addr()
        ),
    );
    let (m, cv) = &*STOP_SIGNAL;
    let mut stopped = m.lock().unwrap_or_else(|p| p.into_inner());
    while !*stopped {
        stopped = cv.wait(stopped).unwrap_or_else(|p| p.into_inner());
    }
    drop(stopped);
    log_line(log_to_file, "INFO", "received signal; shutting down");
    handle.shutdown();
    Ok(())
}
