pub mod auth;
mod http;
pub mod live_routes;
pub mod pool;
pub mod sync_loop;

pub use auth::{
    bearer_token, create_named_token, extract_host, is_allowed, is_allowed_headers, list_named_tokens,
    prune_expired_tokens, revoke_named_token,
};
pub use pool::{FileSig, PoolAcquireError, ReadConnPool, IDLE_CONN_TTL};
pub use sync_loop::{recover_interrupted_sync_runs, run_sync_subprocess, SyncConfig, SyncFailure, SyncRunner};

use crate::json_codec;
use crate::live::error::LiveError;
use crate::live::policy::{Principal, Scope};
use crate::live::service::LiveService;
use crate::live::transport::ServiceEndpoints;
use crate::paths;
use crate::query::api::{self, ApiError};
use crate::query::views;
use crate::timeutil;
use auth::{ApiToken, UNAUTHORIZED_MESSAGE};
use http::{HttpRequest, LoopbackListener};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sync_loop::{ensure_migrated_in_subprocess, terminate_active_sync_child, SyncCoordinator, SYNC_CANCELLED};
use url::Url;

pub const VERSION: &str = crate::VERSION;
pub const HTTP_WORKERS: usize = 4;
pub const HTTP_QUEUE_DEPTH: usize = 64;
pub const MAX_CONCURRENT_LIVE_WRITES: usize = 2;
const WORKER_STACK_SIZE: usize = 512 * 1024;
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(15);

static STOP_SIGNAL: LazyLock<(Mutex<bool>, Condvar)> = LazyLock::new(|| (Mutex::new(false), Condvar::new()));
static SIGNAL_INSTALLED: AtomicBool = AtomicBool::new(false);
static LOG_STATE: Mutex<Option<(Option<OsString>, PathBuf)>> = Mutex::new(None);

struct WriteSemaphore {
    max: usize,
    active: Mutex<usize>,
}

struct WritePermit<'a> {
    sem: &'a WriteSemaphore,
}

impl Drop for WritePermit<'_> {
    fn drop(&mut self) {
        let mut g = self.sem.active.lock().unwrap_or_else(|p| p.into_inner());
        *g = g.saturating_sub(1);
    }
}

impl WriteSemaphore {
    fn new(max: usize) -> Self {
        Self {
            max,
            active: Mutex::new(0),
        }
    }

    fn try_acquire(&self) -> Option<WritePermit<'_>> {
        let mut g = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if *g >= self.max {
            return None;
        }
        *g += 1;
        Some(WritePermit { sem: self })
    }
}

pub(crate) fn log_line(to_file: bool, level: &str, msg: &str) {
    let ts = timeutil::local_now().strftime("%Y-%m-%d %H:%M:%S,%3f");
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

fn send_json_with_headers(req: HttpRequest, status: u16, extra: &[(&str, &str)], body: &Value) {
    let data = json_codec::to_string(body).into_bytes();
    let srv = format!("teams-cli/{VERSION}");
    let mut headers = vec![
        ("Content-Type", "application/json; charset=utf-8"),
        ("Cache-Control", "no-store"),
        ("Server", srv.as_str()),
    ];
    headers.extend_from_slice(extra);
    req.respond(status, &headers, &data);
}

fn send_json(req: HttpRequest, status: u16, body: &Value) {
    send_json_with_headers(req, status, &[], body);
}

fn send_live_error(req: HttpRequest, err: &LiveError) {
    let body = err.to_json();
    if let Some(secs) = err.retry_after_secs {
        let ra = secs.to_string();
        send_json_with_headers(req, err.http_status(), &[("Retry-After", ra.as_str())], &body);
    } else {
        send_json(req, err.http_status(), &body);
    }
}

fn send_unauthorized(req: HttpRequest) {
    let body = json!({
        "error": {"code": "unauthorized", "message": UNAUTHORIZED_MESSAGE}
    });
    send_json_with_headers(req, 401, &[("WWW-Authenticate", "Bearer")], &body);
}

fn check_allowed(req: &HttpRequest) -> bool {
    is_allowed_headers(
        &req.header_values("Host"),
        &req.header_values("Origin"),
        &req.header_values("Sec-Fetch-Site"),
    )
}

fn check_scope(principal: &Principal, required: Scope) -> Result<(), LiveError> {
    if principal.has_scope(required) {
        Ok(())
    } else {
        Err(LiveError::forbidden(
            "scope_required",
            format!(
                "Token '{}' lacks required scope '{}'. Create a token with `--scope {}`.",
                principal.name,
                required.as_str(),
                required.as_str()
            ),
        )
        .with_details(json!({ "requiredScope": required.as_str() })))
    }
}

fn is_json_content_type(ct: Option<&str>) -> bool {
    ct.is_some_and(|v| {
        v.split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("application/json")
    })
}

fn route_archive(
    method: &str,
    path: &str,
    q: &HashMap<String, String>,
    pool: &ReadConnPool,
    syncer: Option<&Arc<SyncCoordinator>>,
    principal: &Principal,
) -> Result<Value, LiveError> {
    if method == "POST" && path == "/v1/sync" {
        check_scope(principal, Scope::Sync)?;
        let Some(s) = syncer else {
            return Err(LiveError::new(
                crate::live::error::LiveErrorKind::Unavailable,
                "no_syncer",
                "This server does not run syncs.",
            ));
        };
        return match s.sync_now(false, true) {
            Ok(stats) => Ok(json!({"ok": true, "stats": stats})),
            Err((code, kind, msg)) => {
                let kind_enum = match code {
                    409 => crate::live::error::LiveErrorKind::Conflict,
                    503 => crate::live::error::LiveErrorKind::Unavailable,
                    _ => crate::live::error::LiveErrorKind::Internal,
                };
                let static_code: &'static str = match kind.as_str() {
                    "sync_busy" => "sync_busy",
                    "no_syncer" => "no_syncer",
                    _ => "sync_error",
                };
                Err(LiveError::new(kind_enum, static_code, msg))
            }
        };
    }
    if method != "GET" {
        return Err(LiveError::not_found(format!("No route {method} {path}")));
    }
    check_scope(principal, Scope::Read)?;
    let (conn, sig) = pool.acquire().map_err(|e| match e {
        PoolAcquireError::MissingArchive(msg) => {
            LiveError::new(crate::live::error::LiveErrorKind::Conflict, "empty_archive", msg)
        }
        PoolAcquireError::Migrating(msg) => {
            LiveError::new(crate::live::error::LiveErrorKind::Unavailable, "migrating", msg)
        }
        PoolAcquireError::Other(msg) => LiveError::internal(msg),
    })?;
    let known_populated = pool.is_known_populated(sig.as_ref());
    let res = dispatch_get(&conn, path, q, known_populated);
    if res.is_ok() && !known_populated && path != "/v1/status" {
        pool.mark_populated(sig.as_ref());
    }
    pool.release(conn, sig);
    res.map_err(|e| {
        let kind = match e.status {
            400 => crate::live::error::LiveErrorKind::BadInput,
            403 => crate::live::error::LiveErrorKind::Forbidden,
            404 => crate::live::error::LiveErrorKind::NotFound,
            409 => crate::live::error::LiveErrorKind::Conflict,
            503 => crate::live::error::LiveErrorKind::Unavailable,
            _ => crate::live::error::LiveErrorKind::Internal,
        };
        let mut le = LiveError::new(kind, e.code, e.message);
        if let Some(d) = e.details {
            le = le.with_details(*d);
        }
        le
    })
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
                api::as_bool(gs("includeHistory")) || api::as_bool(gs("history")),
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
            api::as_bool(gs("includeHistory")) || api::as_bool(gs("history")),
            api::as_bool(gs("countOnly")),
            api::as_bool(gs("hasAttachments")),
        ),
        "/v1/files" | "/v1/attachments" => api::attachments(
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

#[allow(clippy::too_many_arguments)]
fn handle(
    mut req: HttpRequest,
    pool: &ReadConnPool,
    syncer: Option<&Arc<SyncCoordinator>>,
    token: &ApiToken,
    live: &LiveService,
    write_sem: &WriteSemaphore,
    to_file: bool,
) {
    if !check_allowed(&req) {
        let e = ApiError::with_status("forbidden", "Local clients only.", 403);
        send_json(req, 403, &e.to_json());
        return;
    }
    let (path, q) = parse_target(&req.target);
    let method = req.method.clone();

    if path == "/health" && matches!(method.as_str(), "GET" | "HEAD") {
        send_json(req, 200, &json!({"ok": true, "version": VERSION}));
        return;
    }

    let Some(candidate) = bearer_token(&req.header_values("Authorization")) else {
        send_unauthorized(req);
        return;
    };
    let principal = match token.verify(candidate) {
        Ok(Some(p)) => p,
        Ok(None) => {
            send_unauthorized(req);
            return;
        }
        Err(msg) => {
            log_line(to_file, "ERROR", &format!("API token unavailable: {msg}"));
            let e = ApiError::with_status("internal", format!("API token unavailable: {msg}"), 500);
            send_json(req, 500, &e.to_json());
            return;
        }
    };

    if let Some((required_scope, is_write_body, body_cap)) = live_routes::route_spec(&method, &path) {
        if let Err(err) = check_scope(&principal, required_scope) {
            send_live_error(req, &err);
            return;
        }
        if is_write_body && !is_json_content_type(req.first_header("Content-Type")) {
            let err = LiveError::new(
                crate::live::error::LiveErrorKind::BadInput,
                "unsupported_media_type",
                "Content-Type: application/json is required.",
            );
            send_json(req, 415, &err.to_json());
            return;
        }
        let body = if body_cap > 0 {
            match req.read_body(body_cap, BODY_READ_TIMEOUT) {
                Ok(b) => b,
                Err((status, msg)) => {
                    let code = match status {
                        413 => "body_too_large",
                        408 => "request_timeout",
                        _ => "bad_request",
                    };
                    let e = ApiError::with_status(code, msg, status);
                    send_json(req, status, &e.to_json());
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let idempotency_key = req.first_header("Idempotency-Key").map(String::from);
        let permit = if is_write_body {
            match write_sem.try_acquire() {
                Some(p) => Some(p),
                None => {
                    let err = LiveError::new(
                        crate::live::error::LiveErrorKind::Unavailable,
                        "write_busy",
                        "Live write concurrency limit reached; retry shortly.",
                    )
                    .with_retry_after(2);
                    send_live_error(req, &err);
                    return;
                }
            }
        } else {
            None
        };
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            live_routes::dispatch(live, pool, &principal, &method, &path, &q, &body, idempotency_key)
        }));
        drop(permit);
        match outcome {
            Ok(Ok(val)) => send_json(req, 200, &val),
            Ok(Err(err)) => send_live_error(req, &err),
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
        return;
    }

    if req.content_length() > live_routes::SMALL_JSON_BODY_CAP {
        let e = ApiError::with_status("body_too_large", "Request body too large", 413);
        send_json(req, 413, &e.to_json());
        return;
    }

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        route_archive(&method, &path, &q, pool, syncer, &principal)
    }));
    match outcome {
        Ok(Ok(body)) => send_json(req, 200, &body),
        Ok(Err(err)) => send_live_error(req, &err),
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
    start_inner(
        port,
        sync,
        log_to_file,
        false,
        true,
        Arc::new(LiveService::new(ServiceEndpoints::default(), false)),
    )
}

pub fn start_with_live(
    port: u16,
    sync: Option<SyncConfig>,
    log_to_file: bool,
    live: Arc<LiveService>,
) -> Result<DaemonHandle, String> {
    start_inner(port, sync, log_to_file, false, true, live)
}

struct WorkerPoolState {
    queue: VecDeque<TcpStream>,
    active_workers: usize,
    idle_workers: usize,
    next_id: usize,
    shutting_down: bool,
}

#[allow(clippy::too_many_arguments)]
fn spawn_http_worker(
    worker_id: usize,
    shared: Arc<(Mutex<WorkerPoolState>, Condvar)>,
    pool: Arc<ReadConnPool>,
    token: Arc<ApiToken>,
    syncer: Option<Arc<SyncCoordinator>>,
    live: Arc<LiveService>,
    write_sem: Arc<WriteSemaphore>,
    log_to_file: bool,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(format!("teams-http-{worker_id}"))
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || loop {
            let stream = {
                let (m, cv) = &*shared;
                let mut st = m.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if let Some(s) = st.queue.pop_front() {
                        break Some(s);
                    }
                    if st.shutting_down {
                        st.active_workers = st.active_workers.saturating_sub(1);
                        break None;
                    }
                    st.idle_workers += 1;
                    let (guard, timeout) = cv.wait_timeout(st, IDLE_CONN_TTL).unwrap_or_else(|p| p.into_inner());
                    st = guard;
                    st.idle_workers = st.idle_workers.saturating_sub(1);
                    if st.queue.is_empty() && (st.shutting_down || timeout.timed_out()) {
                        st.active_workers = st.active_workers.saturating_sub(1);
                        break None;
                    }
                }
            };
            let Some(stream) = stream else {
                break;
            };
            if let Ok(Some(req)) = http::read_request_head(stream) {
                handle(req, &pool, syncer.as_ref(), &token, &live, &write_sem, log_to_file);
            }
        })
}

fn write_busy_response(mut stream: TcpStream) {
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

fn start_inner(
    port: u16,
    sync: Option<SyncConfig>,
    log_to_file: bool,
    fast_change_check: bool,
    recover_in_process: bool,
    live: Arc<LiveService>,
) -> Result<DaemonHandle, String> {
    let listener = LoopbackListener::bind(port).map_err(|e| format!("failed binding 127.0.0.1:{port}: {e}"))?;
    let bound = listener.addr();
    let token = Arc::new(ApiToken::default());
    token
        .ensure_ready()
        .map_err(|e| format!("failed loading the API token: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let pool = Arc::new(ReadConnPool::new(2));
    let write_sem = Arc::new(WriteSemaphore::new(MAX_CONCURRENT_LIVE_WRITES));

    if sync.is_some() {
        SYNC_CANCELLED.store(false, Ordering::SeqCst);
        if recover_in_process {
            let n = recover_interrupted_sync_runs();
            if n > 0 {
                log_line(
                    log_to_file,
                    "WARNING",
                    &format!("marked {n} interrupted sync run(s) as error"),
                );
            }
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
    let worker_pool = Arc::new((
        Mutex::new(WorkerPoolState {
            queue: VecDeque::new(),
            active_workers: 0,
            idle_workers: 0,
            next_id: 0,
            shutting_down: false,
        }),
        Condvar::new(),
    ));

    let mut threads = Vec::with_capacity(2);
    let accept_thread = thread::Builder::new()
        .name("teams-accept".to_string())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || {
            let mut workers: Vec<thread::JoinHandle<()>> = Vec::new();
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
                workers.retain(|h| !h.is_finished());
                let spawn_id = {
                    let (m, cv) = &*worker_pool;
                    let mut st = m.lock().unwrap_or_else(|p| p.into_inner());
                    if st.queue.len() >= HTTP_QUEUE_DEPTH {
                        drop(st);
                        write_busy_response(stream);
                        continue;
                    }
                    st.queue.push_back(stream);
                    let id = if st.idle_workers == 0 && st.active_workers < HTTP_WORKERS {
                        st.active_workers += 1;
                        let wid = st.next_id;
                        st.next_id = st.next_id.wrapping_add(1);
                        Some(wid)
                    } else {
                        None
                    };
                    cv.notify_one();
                    id
                };
                if let Some(wid) = spawn_id {
                    match spawn_http_worker(
                        wid,
                        Arc::clone(&worker_pool),
                        Arc::clone(&pool_http),
                        Arc::clone(&token),
                        syncer_http.clone(),
                        Arc::clone(&live),
                        Arc::clone(&write_sem),
                        log_to_file,
                    ) {
                        Ok(h) => workers.push(h),
                        Err(e) => {
                            let (m, _) = &*worker_pool;
                            let mut st = m.lock().unwrap_or_else(|p| p.into_inner());
                            st.active_workers = st.active_workers.saturating_sub(1);
                            log_line(log_to_file, "ERROR", &format!("failed spawning HTTP worker: {e}"));
                        }
                    }
                }
            }
            {
                let (m, cv) = &*worker_pool;
                let mut st = m.lock().unwrap_or_else(|p| p.into_inner());
                st.shutting_down = true;
                cv.notify_all();
            }
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

pub fn serve(
    port_opt: Option<u16>,
    interval: u64,
    sync: bool,
    allow_writes: bool,
    log_to_file: bool,
) -> Result<(), String> {
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
    let live = Arc::new(LiveService::new(ServiceEndpoints::default(), allow_writes));
    let handle = start_inner(port, cfg, log_to_file, true, !sync, live)?;
    log_line(
        log_to_file,
        "INFO",
        &format!(
            "teams-cli {VERSION} listening on http://{} (refresh check every {interval}s, writes={allow_writes})",
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
