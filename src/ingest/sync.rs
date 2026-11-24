use crate::identity;
use crate::ingest::extract::{self, Snapshot};
use crate::ingest::indexeddb::{IncrementalOpen, TeamsCache};
use crate::ingest::leveldb;
use crate::ingest::{SourceError, SyncBusy};
use crate::paths;
use crate::storage::{archive, backups, db};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const NOTIFY_AFTER_HOURS: f64 = 6.0;
pub const STALE_SOURCE_DAYS: i64 = identity::STALE_SOURCE_DAYS;
pub const STALE_NOTIFY_HOURS: f64 = 24.0;
pub const FULL_SYNC_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
pub const FULL_SYNC_ENV: &str = "TEAMS_CLI_FULL_SYNC";
const INCREMENTAL_STATE_META_KEY: &str = "incremental_state";
const FULL_SYNC_STARTED_META_KEY: &str = "last_full_sync_started_at";

fn full_sync_forced_by_env() -> bool {
    let Ok(v) = std::env::var(FULL_SYNC_ENV) else {
        return false;
    };
    let t = v.trim();
    !t.is_empty() && !matches!(t.to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncrementalState {
    schema_version: i64,
    render_version: i64,
    source: String,
    full_sync_at_ms: i64,
    last_seq: u64,
    me_mri: Option<String>,
    db_ids: BTreeMap<String, u64>,
    store_ids: BTreeMap<u64, BTreeMap<String, u64>>,
    files: BTreeMap<String, (u64, i128)>,
    cache_messages: i64,
}

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
            if matches!(p.file_name().and_then(|n| n.to_str()), Some("LOCK" | "LOG" | "LOG.old")) {
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

fn file_len_and_mtime(path: &Path) -> Option<(u64, i128)> {
    let st = fs::metadata(path).ok()?;
    if !st.is_file() {
        return None;
    }
    let mtime_ns = st.mtime_nsec() as i128 + (st.mtime() as i128) * 1_000_000_000;
    Some((st.len(), mtime_ns))
}

type FileMetaMap = BTreeMap<String, (u64, i128)>;
type DataFileList = Vec<(u64, bool, PathBuf)>;

fn leveldb_data_files_meta(source: &Path) -> Result<(FileMetaMap, DataFileList), String> {
    let ldb = source.join(format!("{}.leveldb", paths::ORIGIN));
    let listed = leveldb::list_data_files(&ldb)?;
    let mut meta = BTreeMap::new();
    for (_, _, p) in &listed {
        if let (Some(name), Some(sig)) = (p.file_name().and_then(|n| n.to_str()), file_len_and_mtime(p)) {
            meta.insert(name.to_string(), sig);
        }
    }
    Ok((meta, listed))
}

fn load_incremental_state(conn: &rusqlite::Connection) -> Option<IncrementalState> {
    let raw = archive::get_meta(conn, INCREMENTAL_STATE_META_KEY)?;
    serde_json::from_str(&raw).ok()
}

fn save_incremental_state(conn: &rusqlite::Connection, state: &IncrementalState) -> Result<(), String> {
    let text = serde_json::to_string(state).map_err(|e| e.to_string())?;
    archive::set_meta(conn, INCREMENTAL_STATE_META_KEY, &text).map_err(|e| e.to_string())
}

struct FullSyncOutput {
    applied: archive::ApplyCacheResult,
    used_fp: String,
    inc_state: IncrementalState,
}

fn sync_snapshot_into_archive(
    conn: &rusqlite::Connection,
    source: &Path,
    full_sync_at_ms: i64,
    attempts: usize,
    wait_secs: u64,
) -> Result<FullSyncOutput, String> {
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        if let Some(e) = cache_missing_error(source) {
            return Err(e.to_string());
        }
        let before = source_fingerprint(source);
        let files_meta = leveldb_data_files_meta(source).map(|(m, _)| m).unwrap_or_default();
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
                let max_seq = cache.max_seq();
                let me_mri = cache.user_object_id().map(|o| format!("8:orgid:{o}"));
                let (db_ids, store_ids) = cache.watched_metadata_snapshot(extract::CORE_STORES, extract::EXTRA_STORES);
                match archive::apply_cache(conn, &mut cache, None) {
                    Ok(applied) => {
                        let cache_messages = applied.cache_stats.get("cache_messages").copied().unwrap_or(0);
                        let inc_state = IncrementalState {
                            schema_version: archive::SCHEMA_VERSION,
                            render_version: extract::RENDER_VERSION,
                            source: source.to_string_lossy().into_owned(),
                            full_sync_at_ms,
                            last_seq: max_seq,
                            me_mri,
                            db_ids,
                            store_ids,
                            files: files_meta,
                            cache_messages,
                        };
                        return Ok(FullSyncOutput {
                            applied,
                            used_fp: before,
                            inc_state,
                        });
                    }
                    Err(archive::ApplyCacheError::Archive(e)) => return Err(e.to_string()),
                    Err(archive::ApplyCacheError::MissingBlob) => {
                        return Err("Unexpected missing blob during full sync".to_string());
                    }
                    Err(archive::ApplyCacheError::Source(e)) => {
                        if let Some(missing) = cache_missing_error(source) {
                            return Err(missing.to_string());
                        }
                        if attempt == attempts {
                            return Err(format!(
                                "Could not take a consistent snapshot of the Teams cache after {attempts} attempts: {}",
                                e.0
                            ));
                        }
                        let backoff = wait_secs.saturating_mul(1u64 << (attempt - 1).min(16));
                        thread::sleep(Duration::from_secs(backoff));
                    }
                }
            }
            Err(e) => {
                if let Some(missing) = cache_missing_error(source) {
                    return Err(missing.to_string());
                }
                if attempt == attempts {
                    return Err(format!(
                        "Could not take a consistent snapshot of the Teams cache after {attempts} attempts: {}",
                        e.0
                    ));
                }
                let backoff = wait_secs.saturating_mul(1u64 << (attempt - 1).min(16));
                thread::sleep(Duration::from_secs(backoff));
            }
        }
    }
    Err("Failed to snapshot Teams cache".to_string())
}

enum IncrementalOutcome {
    NoChanges {
        used_fp: String,
        next_state: IncrementalState,
    },
    Applied {
        applied: archive::ApplyCacheResult,
        used_fp: String,
        next_state: IncrementalState,
    },
    FallbackToFull,
}

fn try_incremental_sync(
    conn: &rusqlite::Connection,
    source: &Path,
    state: &IncrementalState,
) -> Result<IncrementalOutcome, String> {
    if state.last_seq == 0 {
        return Ok(IncrementalOutcome::FallbackToFull);
    }
    for attempt in 1..=2usize {
        if cache_missing_error(source).is_some() {
            return Ok(IncrementalOutcome::FallbackToFull);
        }
        let before = source_fingerprint(source);
        let Ok((cur_files, listed)) = leveldb_data_files_meta(source) else {
            return Ok(IncrementalOutcome::FallbackToFull);
        };
        let changed: Vec<(u64, bool, PathBuf)> = listed
            .into_iter()
            .filter(|(_, _, p)| {
                let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                    return false;
                };
                match (cur_files.get(name), state.files.get(name)) {
                    (Some(cur), Some(prev)) => cur != prev,
                    (Some(_), None) => true,
                    _ => false,
                }
            })
            .collect();

        if changed.is_empty() {
            if source_fingerprint(source) != before {
                if attempt == 2 {
                    return Ok(IncrementalOutcome::FallbackToFull);
                }
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            let mut next_state = state.clone();
            next_state.files = cur_files;
            return Ok(IncrementalOutcome::NoChanges {
                used_fp: before,
                next_state,
            });
        }

        let check = || -> Result<(), SourceError> {
            if source_fingerprint(source) != before {
                return Err(SourceError("Teams wrote to its cache during delta copy".to_string()));
            }
            Ok(())
        };

        let opened = TeamsCache::open_incremental(
            source,
            &changed,
            &state.db_ids,
            &state.store_ids,
            state.last_seq,
            extract::CORE_STORES,
            extract::EXTRA_STORES,
            check,
        );

        let mut cache = match opened {
            Ok(IncrementalOpen::Ready(c)) => *c,
            Ok(IncrementalOpen::NeedsFullSync(_)) => return Ok(IncrementalOutcome::FallbackToFull),
            Err(_) => {
                if attempt == 2 {
                    return Ok(IncrementalOutcome::FallbackToFull);
                }
                thread::sleep(Duration::from_millis(500));
                continue;
            }
        };

        let mut next_state = state.clone();
        next_state.files = cur_files;
        next_state.last_seq = cache.max_seq().max(state.last_seq);

        if !cache.has_records() {
            return Ok(IncrementalOutcome::NoChanges {
                used_fp: before,
                next_state,
            });
        }

        let seed = archive::load_extract_seed(conn, state.me_mri.clone()).map_err(|e| e.to_string())?;
        match archive::apply_cache_incremental(conn, &mut cache, seed, None) {
            Ok(applied) => {
                return Ok(IncrementalOutcome::Applied {
                    applied,
                    used_fp: before,
                    next_state,
                });
            }
            Err(archive::ApplyCacheError::MissingBlob) => {
                return Ok(IncrementalOutcome::FallbackToFull);
            }
            Err(archive::ApplyCacheError::Archive(e)) => return Err(e.to_string()),
            Err(archive::ApplyCacheError::Source(_)) => return Ok(IncrementalOutcome::FallbackToFull),
        }
    }
    Ok(IncrementalOutcome::FallbackToFull)
}

fn has_meaningful_delta(merged: &BTreeMap<String, i64>) -> bool {
    merged.values().any(|&v| v > 0)
}

pub fn run_sync(source: Option<&Path>, notify: bool, force: bool) -> Result<Value, String> {
    let src = source.map(|p| p.to_path_buf()).unwrap_or_else(paths::source_dir);
    let _lock = acquire_sync_lock(120.0).map_err(|e| e.to_string())?;
    cleanup_stale_temp_dirs(&std::env::temp_dir(), Duration::from_secs(3600));
    let conn = archive::connect(None).map_err(|e| e.to_string())?;
    let _ = crate::daemon::sync_loop::recover_interrupted_sync_runs_on_conn(&conn);
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

    let now_before = archive::now_ms();
    let src_str = src.to_string_lossy().into_owned();
    let inc_state = if !force && !full_sync_forced_by_env() && prev.is_some() {
        load_incremental_state(&conn).filter(|st| {
            st.schema_version == archive::SCHEMA_VERSION
                && st.render_version == extract::RENDER_VERSION
                && st.source == src_str
                && now_before.saturating_sub(st.full_sync_at_ms) < FULL_SYNC_INTERVAL_MS
        })
    } else {
        None
    };

    if let Some(inc_state) = inc_state {
        let t0 = Instant::now();
        match try_incremental_sync(&conn, &src, &inc_state) {
            Ok(IncrementalOutcome::NoChanges { used_fp, next_state }) => {
                save_incremental_state(&conn, &next_state)?;
                archive::set_meta(&conn, "source_fingerprint", &used_fp).map_err(|e| e.to_string())?;
                archive::set_meta(&conn, "last_checked_at", &archive::now_ms().to_string())
                    .map_err(|e| e.to_string())?;
                let _ = db::wal_checkpoint(&conn, "TRUNCATE");
                let mut stats = Map::new();
                stats.insert(
                    "skipped".to_string(),
                    Value::String("No new Teams records in cache delta".to_string()),
                );
                stats.insert("incremental".to_string(), json!(true));
                stats.insert("duration_ms".to_string(), json!(t0.elapsed().as_millis() as i64));
                maybe_backup(&conn, &mut stats, notify);
                if notify {
                    notify_if_cache_stale(&conn);
                }
                return Ok(Value::Object(stats));
            }
            Ok(IncrementalOutcome::Applied {
                applied,
                used_fp,
                next_state,
            }) => {
                save_incremental_state(&conn, &next_state)?;
                archive::set_meta(&conn, "source_fingerprint", &used_fp).map_err(|e| e.to_string())?;
                archive::set_meta(&conn, "last_checked_at", &archive::now_ms().to_string())
                    .map_err(|e| e.to_string())?;
                let mut stats = Map::new();
                if has_meaningful_delta(&applied.merged) {
                    let run_id = archive::start_run(&conn, &src_str).map_err(|e| e.to_string())?;
                    for (k, v) in applied.merged {
                        stats.insert(k, json!(v));
                    }
                    stats.insert("cache_messages".to_string(), json!(next_state.cache_messages));
                    stats.insert("incremental".to_string(), json!(true));
                    stats.insert("duration_ms".to_string(), json!(t0.elapsed().as_millis() as i64));
                    let stats_val = Value::Object(stats.clone());
                    archive::finish_run(&conn, run_id, "ok", Some(&stats_val), None).map_err(|e| e.to_string())?;
                } else {
                    stats.insert(
                        "skipped".to_string(),
                        Value::String("No new Teams records in cache delta".to_string()),
                    );
                    stats.insert("incremental".to_string(), json!(true));
                    stats.insert("duration_ms".to_string(), json!(t0.elapsed().as_millis() as i64));
                }
                let _ = db::wal_checkpoint(&conn, "TRUNCATE");
                maybe_backup(&conn, &mut stats, notify);
                if notify {
                    notify_if_cache_stale(&conn);
                }
                return Ok(Value::Object(stats));
            }
            Ok(IncrementalOutcome::FallbackToFull) | Err(_) => {}
        }
    }

    let full_started_at = archive::now_ms();
    let run_id = archive::start_run(&conn, &src_str).map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    let res = (|| -> Result<Map<String, Value>, String> {
        let out = sync_snapshot_into_archive(&conn, &src, full_started_at, 4, 3)?;
        db::optimize(&conn);
        let mut stats = Map::new();
        for (k, v) in out.applied.merged {
            stats.insert(k, json!(v));
        }
        for (k, v) in out.applied.cache_stats {
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
        save_incremental_state(&conn, &out.inc_state)?;
        archive::set_meta(&conn, FULL_SYNC_STARTED_META_KEY, &full_started_at.to_string())
            .map_err(|e| e.to_string())?;
        archive::set_meta(&conn, "source_fingerprint", &out.used_fp).map_err(|e| e.to_string())?;
        archive::set_meta(&conn, "last_checked_at", &archive::now_ms().to_string()).map_err(|e| e.to_string())?;
        let _ = archive::prune_old_sync_runs(&conn, 90, 100);
        let _ = db::wal_checkpoint(&conn, "TRUNCATE");
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
