use crate::identity;
use crate::ingest::extract::{self, Snapshot};
use crate::ingest::indexeddb::TeamsCache;
use crate::ingest::{SourceError, SyncBusy};
use crate::paths;
use crate::storage::{archive, backups, db};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const NOTIFY_AFTER_HOURS: f64 = 6.0;
pub const STALE_SOURCE_DAYS: i64 = identity::STALE_SOURCE_DAYS;
pub const STALE_NOTIFY_HOURS: f64 = 24.0;

pub struct SyncLockGuard {
    _file: File,
}

pub fn acquire_sync_lock(wait_seconds: f64) -> Result<SyncLockGuard, SyncBusy> {
    let lock_path = paths::lock_path();
    if let Some(parent) = lock_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| SyncBusy(format!("could not open sync lock: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs_f64(wait_seconds.max(0.0));
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(SyncLockGuard { _file: f }),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(SyncBusy("Another sync is still running.".to_string()));
                }
                thread::sleep(Duration::from_millis(500));
            }
            Err(TryLockError::Error(e)) => {
                return Err(SyncBusy(format!("sync lock failed: {e}")));
            }
        }
    }
}

fn walk_sorted(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(dir) {
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                walk_sorted(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}

pub fn source_fingerprint(source: &Path) -> String {
    let mut hasher = Sha256::new();
    for suffix in ["leveldb", "blob"] {
        let d = source.join(format!("{}.{suffix}", paths::ORIGIN));
        if !d.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        walk_sorted(&d, &mut files);
        for p in files {
            if p.file_name().and_then(|n| n.to_str()) == Some("LOCK") {
                continue;
            }
            let rel = p.strip_prefix(source).unwrap_or(&p).to_string_lossy();
            match fs::metadata(&p) {
                Ok(st) => {
                    if !st.is_file() {
                        continue;
                    }
                    let line = format!(
                        "{}|{}|{}\n",
                        rel,
                        st.len(),
                        st.mtime_nsec() as i128 + (st.mtime() as i128) * 1_000_000_000
                    );
                    hasher.update(line.as_bytes());
                }
                Err(_) => {
                    hasher.update(format!("{rel}|missing\n").as_bytes());
                }
            }
        }
    }
    hex::encode(hasher.finalize())
}

fn cache_missing_error(source: &Path) -> Option<SourceError> {
    let ldb = source.join(format!("{}.leveldb", paths::ORIGIN));
    if ldb.is_dir() {
        None
    } else {
        Some(SourceError(format!(
            "Teams cache not found at {}. Is the new Microsoft Teams installed and signed in?",
            ldb.display()
        )))
    }
}

fn take_snapshot(source: &Path, attempts: usize, wait_secs: u64) -> Result<(Snapshot, String), SourceError> {
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        if let Some(e) = cache_missing_error(source) {
            return Err(e);
        }
        let before = source_fingerprint(source);
        let check = || -> Result<(), SourceError> {
            if source_fingerprint(source) != before {
                return Err(SourceError(format!(
                    "Teams wrote to its cache during the copy (attempt {attempt}/{attempts})"
                )));
            }
            Ok(())
        };
        match TeamsCache::open(Some(source), Some(check)) {
            Ok(mut cache) => {
                let snap = extract::extract(&mut cache)?;
                return Ok((snap, before));
            }
            Err(e) => {
                if let Some(missing) = cache_missing_error(source) {
                    return Err(missing);
                }
                if attempt == attempts {
                    return Err(SourceError(format!(
                        "Could not take a consistent snapshot of the Teams cache after {attempts} attempts: {}",
                        e.0
                    )));
                }
                let backoff = wait_secs.saturating_mul(1u64 << (attempt - 1).min(16));
                thread::sleep(Duration::from_secs(backoff));
            }
        }
    }
    Err(SourceError("Failed to snapshot Teams cache".to_string()))
}

pub fn snapshot_source(source: &Path, attempts: usize, wait_secs: u64) -> Result<(Snapshot, String), SourceError> {
    take_snapshot(source, attempts, wait_secs)
}

pub fn cleanup_stale_temp_dirs(tmp_root: &Path, max_age: Duration) -> usize {
    let Ok(rd) = fs::read_dir(tmp_root) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in rd.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("teams-cli-") {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_dir() {
            continue;
        }
        let old = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if old && fs::remove_dir_all(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn drift_warning(prev: Option<&archive::SyncRun>, stats: &Map<String, Value>) -> Option<String> {
    let prev = prev?;
    let prev_stats: Value = serde_json::from_str(&prev.stats).unwrap_or(Value::Null);
    let before = prev_stats.get("cache_messages").and_then(|v| v.as_i64()).unwrap_or(0);
    let now = stats.get("cache_messages").and_then(|v| v.as_i64()).unwrap_or(0);
    if before >= 200 && (now as f64) < (before as f64) * 0.2 {
        return Some(format!(
            "Teams cache returned {now} messages vs {before} last time. Teams may have changed its \
             storage format; the archive is intact, but new messages may be missing."
        ));
    }
    let bad = stats.get("cache_bad_records").and_then(|v| v.as_i64()).unwrap_or(0);
    if bad > 50 {
        return Some(format!("{bad} cache records could not be decoded."));
    }
    None
}

pub fn run_sync(source: Option<&Path>, notify: bool, force: bool) -> Result<Value, String> {
    let src = source.map(|p| p.to_path_buf()).unwrap_or_else(paths::source_dir);
    let _lock = acquire_sync_lock(120.0).map_err(|e| e.to_string())?;
    cleanup_stale_temp_dirs(&std::env::temp_dir(), Duration::from_secs(3600));
    let conn = archive::connect(None).map_err(|e| e.to_string())?;
    let prev = archive::last_run(&conn, true);
    let fp = source_fingerprint(&src);

    if !force && prev.is_some() && archive::get_meta(&conn, "source_fingerprint").as_deref() == Some(fp.as_str()) {
        let mut stats = Map::new();
        stats.insert(
            "skipped".to_string(),
            Value::String("Teams cache unchanged since last sync".to_string()),
        );
        let _ = archive::set_meta(&conn, "last_checked_at", &archive::now_ms().to_string());
        if let Ok(n) = archive::rerender(&conn, false) {
            if n > 0 {
                stats.insert("rerendered".to_string(), json!(n));
            }
        }
        maybe_backup(&conn, &mut stats, notify);
        if notify {
            notify_if_cache_stale(&conn);
        }
        return Ok(Value::Object(stats));
    }

    let run_id = archive::start_run(&conn, &src.to_string_lossy()).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let res = (|| -> Result<Map<String, Value>, String> {
        let (snap, used_fp) = take_snapshot(&src, 4, 3).map_err(|e| e.to_string())?;
        let merged = archive::apply_snapshot(&conn, &snap, None).map_err(|e| e.to_string())?;
        db::optimize(&conn);
        let _ = db::wal_checkpoint(&conn, "TRUNCATE");
        let mut stats = Map::new();
        for (k, v) in merged {
            stats.insert(k, json!(v));
        }
        for (k, v) in snap.stats {
            stats.insert(k, json!(v));
        }
        if let Ok(n) = archive::rerender(&conn, false) {
            if n > 0 {
                stats.insert("rerendered".to_string(), json!(n));
            }
        }
        stats.insert("duration_ms".to_string(), json!(t0.elapsed().as_millis() as i64));
        let warning = drift_warning(prev.as_ref(), &stats);
        let status_str = if warning.is_some() { "warning" } else { "ok" };
        let stats_val = Value::Object(stats.clone());
        archive::finish_run(&conn, run_id, status_str, Some(&stats_val), warning.as_deref())
            .map_err(|e| e.to_string())?;
        archive::set_meta(&conn, "source_fingerprint", &used_fp).map_err(|e| e.to_string())?;
        archive::set_meta(&conn, "last_checked_at", &archive::now_ms().to_string()).map_err(|e| e.to_string())?;
        Ok(stats)
    })();

    match res {
        Ok(mut stats) => {
            maybe_backup(&conn, &mut stats, notify);
            if notify {
                notify_if_cache_stale(&conn);
            }
            Ok(Value::Object(stats))
        }
        Err(e) => {
            let _ = archive::finish_run(&conn, run_id, "error", None, Some(&e));
            if notify {
                notify_if_stale(&conn, &e);
            }
            Err(e)
        }
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

fn maybe_backup(conn: &rusqlite::Connection, stats: &mut Map<String, Value>, notify: bool) {
    match backups::backup_if_due(conn, 24.0) {
        Ok(Some(dest)) => {
            stats.insert("backup".to_string(), Value::String(dest.to_string_lossy().into_owned()));
            let _ = archive::set_meta(conn, "backup_error", "");
        }
        Ok(None) => {
            let _ = archive::set_meta(conn, "backup_error", "");
        }
        Err(e) => {
            let msg = e.to_string();
            stats.insert("backup_error".to_string(), Value::String(msg.clone()));
            let stamp = crate::timeutil::local_now().strftime("%Y-%m-%d %H:%M").to_string();
            let _ = archive::set_meta(conn, "backup_error", &format!("{stamp}: {msg}"));
            if notify {
                send_notification(
                    "backup",
                    &format!("Teams archive backup failed: {}", truncate_chars(&msg, 120)),
                    NOTIFY_AFTER_HOURS,
                );
            }
        }
    }
}

fn notify_if_stale(conn: &rusqlite::Connection, err: &str) {
    if let Some(ok) = archive::last_run(conn, true) {
        if let Some(fin) = ok.finished_at {
            if (archive::now_ms() - fin) < (NOTIFY_AFTER_HOURS * 3_600_000.0) as i64 {
                return;
            }
        }
    }
    send_notification(
        "sync",
        &format!(
            "Teams archive sync is failing: {}. Run: teams status",
            truncate_chars(err, 120)
        ),
        NOTIFY_AFTER_HOURS,
    );
}

fn notify_if_cache_stale(conn: &rusqlite::Connection) {
    if let Some(ok) = archive::last_run(conn, true) {
        if let Some(fin) = ok.finished_at {
            if archive::now_ms() - fin > STALE_SOURCE_DAYS * identity::DAY_MS {
                send_notification(
                    "stale-cache",
                    &format!(
                        "Teams cache hasn't changed in over {STALE_SOURCE_DAYS} days. \
                         Teams may have moved its storage; run: teams status"
                    ),
                    STALE_NOTIFY_HOURS,
                );
            }
        }
    }
}

pub fn notification_args(msg: &str) -> Vec<String> {
    vec![
        "-e".to_string(),
        "on run argv".to_string(),
        "-e".to_string(),
        "display notification (item 1 of argv) with title \"teams-cli\"".to_string(),
        "-e".to_string(),
        "end run".to_string(),
        {
            let m = msg.replace(['\r', '\n'], " ");
            if m.starts_with('-') {
                format!(" {m}")
            } else {
                m
            }
        },
    ]
}

fn send_notification(topic: &str, msg: &str, cooldown_hours: f64) {
    let stamp = paths::home().join(format!("last-{topic}-notification"));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(meta) = fs::metadata(&stamp) {
        if let Ok(mt) = meta
            .modified()
            .and_then(|t| t.duration_since(UNIX_EPOCH).map_err(std::io::Error::other))
        {
            if now - mt.as_secs_f64() < cooldown_hours * 3600.0 {
                return;
            }
        }
    }
    let _ = fs::write(&stamp, b"");
    let mut cmd = Command::new("osascript");
    cmd.args(notification_args(msg));
    let _ = paths::run_command_with_timeout(&mut cmd, Duration::from_secs(10));
}
