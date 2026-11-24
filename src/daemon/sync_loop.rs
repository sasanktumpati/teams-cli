use crate::daemon::log_line;
use crate::ingest::sync;
use crate::paths;
use crate::storage::{archive, db};
use serde_json::{json, Value};
use std::env;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SYNC_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(600);
const MIGRATE_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(120);
const SYNC_CHILD_TERM_GRACE: Duration = Duration::from_secs(3);
pub(crate) const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(15 * 60);

static ACTIVE_SYNC_CHILD: Mutex<Option<Child>> = Mutex::new(None);
pub(crate) static SYNC_CANCELLED: AtomicBool = AtomicBool::new(false);

pub type SyncFailure = (u16, String, String);

pub type SyncRunner = Arc<dyn Fn(bool) -> Result<Value, SyncFailure> + Send + Sync>;

pub struct SyncConfig {
    pub runner: SyncRunner,
    pub interval: Option<u64>,
}

#[derive(Default)]
struct SyncState {
    last_ok: Option<(String, Instant)>,
    consecutive_errors: u32,
    last_error_msg: Option<String>,
}

pub(crate) struct SyncCoordinator {
    lock: Mutex<()>,
    wake: (Mutex<bool>, Condvar),
    runner: SyncRunner,
    to_file: bool,
    fast_change_check: bool,
    state: Mutex<SyncState>,
}

impl SyncCoordinator {
    pub(crate) fn new(runner: SyncRunner, to_file: bool, fast_change_check: bool) -> Self {
        Self {
            lock: Mutex::new(()),
            wake: (Mutex::new(false), Condvar::new()),
            runner,
            to_file,
            fast_change_check,
            state: Mutex::new(SyncState::default()),
        }
    }

    pub(crate) fn sync_now(&self, notify: bool, non_blocking: bool) -> Result<Value, SyncFailure> {
        let _guard = if non_blocking {
            match self.lock.try_lock() {
                Ok(g) => g,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err((409, "sync_busy".into(), "Another sync is still running.".into()));
                }
            }
        } else {
            self.lock.lock().unwrap_or_else(|p| p.into_inner())
        };

        let pre_fp = self
            .fast_change_check
            .then(|| sync::source_fingerprint(&paths::source_dir()));
        match (self.runner)(notify) {
            Ok(stats) => {
                {
                    let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    if let Some(fp) = pre_fp {
                        st.last_ok = Some((fp, Instant::now()));
                    }
                    st.consecutive_errors = 0;
                    st.last_error_msg = None;
                }
                if let Some(obj) = stats.as_object() {
                    let brief: serde_json::Map<String, Value> = obj
                        .iter()
                        .filter(|(k, _)| !k.starts_with("cache_"))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    log_line(self.to_file, "INFO", &format!("sync ok: {}", Value::Object(brief)));
                }
                Ok(stats)
            }
            Err((code, kind, msg)) => {
                if kind == "sync_busy" {
                    log_line(self.to_file, "INFO", "sync skipped: another sync is running");
                } else {
                    let should_log = {
                        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                        st.consecutive_errors = st.consecutive_errors.saturating_add(1);
                        let changed = st.last_error_msg.as_deref() != Some(msg.as_str());
                        st.last_error_msg = Some(msg.clone());
                        changed || st.consecutive_errors == 1
                    };
                    if should_log {
                        log_line(self.to_file, "ERROR", &format!("sync failed ({kind}): {msg}"));
                    }
                }
                Err((code, kind, msg))
            }
        }
    }

    pub(crate) fn periodic_tick(&self, base_interval: Duration) -> Duration {
        if self.fast_change_check {
            let fp = sync::source_fingerprint(&paths::source_dir());
            let skip = {
                let st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                matches!(
                    st.last_ok.as_ref(),
                    Some((last_fp, last_at)) if *last_fp == fp && last_at.elapsed() < HOUSEKEEPING_INTERVAL
                )
            };
            if skip {
                return base_interval;
            }
        }
        let _ = self.sync_now(true, false);
        let fails = self.state.lock().unwrap_or_else(|p| p.into_inner()).consecutive_errors;
        if fails == 0 {
            base_interval
        } else {
            let mult = 1u32.checked_shl(fails.min(5)).unwrap_or(32);
            base_interval
                .saturating_mul(mult)
                .min(HOUSEKEEPING_INTERVAL.max(base_interval))
        }
    }

    pub(crate) fn wait_for(&self, timeout: Duration, stop: &AtomicBool) {
        let (m, cv) = &self.wake;
        let mut woke = m.lock().unwrap_or_else(|p| p.into_inner());
        if !*woke && !stop.load(Ordering::SeqCst) {
            woke = cv
                .wait_timeout(woke, timeout)
                .map(|(g, _)| g)
                .unwrap_or_else(|p| p.into_inner().0);
        }
        *woke = false;
    }

    pub(crate) fn wake(&self) {
        let (m, cv) = &self.wake;
        *m.lock().unwrap_or_else(|p| p.into_inner()) = true;
        cv.notify_all();
    }
}

pub fn run_sync_subprocess(notify: bool) -> Result<Value, SyncFailure> {
    let exe = env::current_exe().map_err(|e| (500, "internal".into(), e.to_string()))?;
    let mut cmd = Command::new(exe);
    cmd.args(["sync", "--json"]);
    if notify {
        cmd.arg("--notify");
    }
    let out = match run_tracked_child(&mut cmd, SYNC_SUBPROCESS_TIMEOUT) {
        Ok(o) => o,
        Err(paths::CommandTimeoutError::TimedOut) => {
            return Err((
                500,
                "internal".into(),
                format!("sync worker timed out after {}s", SYNC_SUBPROCESS_TIMEOUT.as_secs()),
            ));
        }
        Err(paths::CommandTimeoutError::Io(e)) => {
            return Err((500, "internal".into(), format!("failed spawning sync worker: {e}")));
        }
    };
    if out.status.success() {
        let txt = String::from_utf8_lossy(&out.stdout);
        let parsed = serde_json::from_str(txt.trim()).unwrap_or_else(|_| json!({}));
        return Ok(parsed);
    }
    let raw_err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let err = raw_err.strip_prefix("teams: ").unwrap_or(&raw_err).to_string();
    if err.contains("Another sync is still running") {
        return Err((409, "sync_busy".into(), err));
    }
    Err((
        500,
        "internal".into(),
        if err.is_empty() { "sync failed".into() } else { err },
    ))
}

fn run_tracked_child(cmd: &mut Command, timeout: Duration) -> Result<std::process::Output, paths::CommandTimeoutError> {
    use paths::CommandTimeoutError as E;
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (stdout_pipe, stderr_pipe) = {
        let mut slot = ACTIVE_SYNC_CHILD.lock().unwrap_or_else(|p| p.into_inner());
        if SYNC_CANCELLED.load(Ordering::SeqCst) {
            return Err(E::Io(std::io::Error::other("daemon is shutting down")));
        }
        let mut child = cmd.spawn().map_err(E::Io)?;
        let pipes = (child.stdout.take(), child.stderr.take());
        *slot = Some(child);
        pipes
    };
    let read_all = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut s) = pipe {
                let _ = s.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_reader = read_all(stdout_pipe.map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err_reader = read_all(stderr_pipe.map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = Instant::now() + timeout;
    let status = loop {
        let mut slot = ACTIVE_SYNC_CHILD.lock().unwrap_or_else(|p| p.into_inner());
        let Some(child) = slot.as_mut() else {
            drop(slot);
            let _ = out_reader.join();
            let _ = err_reader.join();
            return Err(E::Io(std::io::Error::other(
                "sync worker terminated by daemon shutdown",
            )));
        };
        match child.try_wait() {
            Ok(Some(st)) => {
                *slot = None;
                break st;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                *slot = None;
                drop(slot);
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(E::TimedOut);
            }
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                *slot = None;
                return Err(E::Io(e));
            }
        }
        drop(slot);
        thread::sleep(Duration::from_millis(50));
    };
    Ok(std::process::Output {
        status,
        stdout: out_reader.join().unwrap_or_default(),
        stderr: err_reader.join().unwrap_or_default(),
    })
}

pub(crate) fn terminate_active_sync_child() -> bool {
    let mut slot = ACTIVE_SYNC_CHILD.lock().unwrap_or_else(|p| p.into_inner());
    let Some(mut child) = slot.take() else {
        return false;
    };
    drop(slot);
    if let Ok(None) = child.try_wait() {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + SYNC_CHILD_TERM_GRACE;
        while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
    }
    let _ = child.wait();
    true
}

pub fn recover_interrupted_sync_runs() -> usize {
    let p = paths::archive_path();
    if !p.is_file() {
        return 0;
    }
    let Ok(_lock) = sync::acquire_sync_lock(0.0) else {
        return 0;
    };
    let Ok(conn) = db::connect_rw(Some(&p)) else {
        return 0;
    };
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    if ver != archive::SCHEMA_VERSION {
        return 0;
    }
    conn.execute(
        "UPDATE sync_runs SET status = 'error', finished_at = COALESCE(finished_at, ?), \
         message = 'interrupted by daemon restart' WHERE status = 'running'",
        [archive::now_ms()],
    )
    .unwrap_or(0)
}

pub(crate) fn ensure_migrated_in_subprocess() {
    let p = paths::archive_path();
    if !p.is_file() {
        return;
    }
    if let Ok(conn) = archive::connect_readonly(Some(&p)) {
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if ver == archive::SCHEMA_VERSION {
            return;
        }
    }
    if let Ok(exe) = env::current_exe() {
        let mut cmd = Command::new(exe);
        cmd.args(["status", "--json"]);
        let _ = paths::run_command_with_timeout(&mut cmd, MIGRATE_SUBPROCESS_TIMEOUT);
    }
}
