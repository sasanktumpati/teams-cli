use crate::paths;
use crate::storage::archive::{self, ArchiveError, SCHEMA_VERSION};
use crate::storage::db;
use chrono::{Datelike, Local, NaiveDateTime};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use xz2::read::XzDecoder;
use xz2::write::XzEncoder;

pub const BACKUP_DAILY: usize = 7;
pub const BACKUP_WEEKLY: usize = 4;
pub const BACKUP_MONTHLY: usize = 6;
pub const MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const TMP_PREFIX: &str = ".teams-backup-";
const VERIFY_TMP_PREFIX: &str = "teams-cli-verify-";
const BEFORE_RESTORE_PREFIX: &str = "archive-before-restore-";
const STALE_TMP_SECONDS: u64 = 3600;
const MAX_REPORTED_BAD_RECORDS: usize = 20;
const CORE_TABLES: &[&str] = &["messages", "conversations", "people", "meta", "sync_runs"];

fn is_backup_name(name: &str) -> bool {
    name.ends_with(".sqlite3") || name.ends_with(".sqlite3.xz")
}

pub fn is_scheduled_backup(name: &str) -> bool {
    name.starts_with("archive-") && is_backup_name(name) && !name.starts_with(BEFORE_RESTORE_PREFIX)
}

fn io_err(e: std::io::Error) -> ArchiveError {
    ArchiveError(e.to_string())
}

fn file_name_of(p: &Path) -> Result<String, ArchiveError> {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| ArchiveError(format!("{} is not a file path", p.display())))
}

fn create_private(dest: &Path) -> Result<File, ArchiveError> {
    match fs::symlink_metadata(dest) {
        Ok(md) if md.is_dir() => {
            return Err(ArchiveError(format!("{} is a directory", dest.display())));
        }
        Ok(_) => fs::remove_file(dest).map_err(io_err)?,
        Err(_) => {}
    }
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)
        .map_err(io_err)
}

pub fn backup(conn: &Connection, dest: &Path) -> Result<PathBuf, ArchiveError> {
    let dest_name = file_name_of(dest)?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| ArchiveError(e.to_string()))?;
    }
    if dest.exists() {
        return Err(ArchiveError(format!("{} already exists", dest.display())));
    }
    let work = paths::private_tmpdir(dest, TMP_PREFIX).map_err(|e| ArchiveError(e.to_string()))?;
    let plain = work.path().join("backup.sqlite3");
    conn.execute("VACUUM INTO ?", [plain.to_string_lossy().as_ref()])?;
    let _ = fs::set_permissions(&plain, fs::Permissions::from_mode(0o600));
    verify_sqlite(&plain)?;
    if dest_name.ends_with(".xz") {
        compress_xz(&plain, dest)?;
    } else {
        paths::fsync_file(&plain).map_err(|e| ArchiveError(e.to_string()))?;
        fs::rename(&plain, dest).map_err(|e| ArchiveError(e.to_string()))?;
        if let Some(parent) = dest.parent() {
            paths::fsync_dir(parent);
        }
    }
    Ok(dest.to_path_buf())
}

fn verify_sqlite(path: &Path) -> Result<(), ArchiveError> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = DELETE;")?;
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let results: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if results != vec!["ok".to_string()] {
        let joined = results.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
        return Err(ArchiveError(format!(
            "Backup failed its integrity check ({joined}); the archive may be damaged."
        )));
    }
    Ok(())
}

fn compress_xz(src: &Path, dest: &Path) -> Result<(), ArchiveError> {
    let name = file_name_of(dest)?;
    let work = paths::private_tmpdir(dest, TMP_PREFIX).map_err(|e| ArchiveError(e.to_string()))?;
    let tmp = work.path().join(format!("{name}.partial"));
    let mut want = Sha256::new();
    {
        let mut fin = File::open(src).map_err(|e| ArchiveError(e.to_string()))?;
        let raw = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| ArchiveError(e.to_string()))?;
        let mut enc = XzEncoder::new(raw, 6);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = fin.read(&mut buf).map_err(|e| ArchiveError(e.to_string()))?;
            if n == 0 {
                break;
            }
            want.update(&buf[..n]);
            enc.write_all(&buf[..n]).map_err(|e| ArchiveError(e.to_string()))?;
        }
        let raw_out = enc.finish().map_err(|e| ArchiveError(e.to_string()))?;
        raw_out.sync_all().map_err(|e| ArchiveError(e.to_string()))?;
    }
    let mut got = Sha256::new();
    {
        let fin = File::open(&tmp).map_err(|e| ArchiveError(e.to_string()))?;
        let mut dec = XzDecoder::new(fin);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = dec.read(&mut buf).map_err(|e| ArchiveError(e.to_string()))?;
            if n == 0 {
                break;
            }
            got.update(&buf[..n]);
        }
    }
    if got.finalize() != want.finalize() {
        return Err(ArchiveError(format!(
            "Compressed backup {name} did not read back identically."
        )));
    }
    fs::rename(&tmp, dest).map_err(|e| ArchiveError(e.to_string()))?;
    if let Some(parent) = dest.parent() {
        paths::fsync_dir(parent);
    }
    Ok(())
}

fn mtime_secs(p: &Path) -> f64 {
    p.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn list_backups(dir: Option<&Path>) -> Vec<PathBuf> {
    let d = dir.map(|p| p.to_path_buf()).unwrap_or_else(paths::backups_dir);
    let mut found = Vec::new();
    if let Ok(rd) = fs::read_dir(&d) {
        for entry in rd.flatten() {
            let p = entry.path();
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("archive-") && is_backup_name(name) {
                    found.push(p);
                }
            }
        }
    }
    found.sort_by(|a, b| mtime_secs(a).total_cmp(&mtime_secs(b)));
    found
}

pub fn compress_old_backups(dir: Option<&Path>) -> Result<usize, ArchiveError> {
    let (count, warnings) = compress_old_backups_with_warnings(dir);
    for w in &warnings {
        eprintln!("teams-cli: {w}");
    }
    Ok(count)
}

pub fn compress_old_backups_with_warnings(dir: Option<&Path>) -> (usize, Vec<String>) {
    let mut count = 0;
    let mut warnings = Vec::new();
    for p in list_backups(dir) {
        let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        if !name.ends_with(".sqlite3") || name.starts_with(BEFORE_RESTORE_PREFIX) {
            continue;
        }
        let dest = p.with_file_name(format!("{name}.xz"));
        let mtime = p.metadata().ok().and_then(|m| m.modified().ok());
        if let Err(e) = verify_sqlite(&p).and_then(|_| compress_xz(&p, &dest)) {
            warnings.push(format!("skipped compressing legacy backup {name}: {e}"));
            continue;
        }
        if let Some(mt) = mtime {
            let _ = File::options().write(true).open(&dest).and_then(|f| f.set_modified(mt));
        }
        let _ = fs::remove_file(&p);
        count += 1;
    }
    (count, warnings)
}

pub fn backups_to_keep(names: &[String], daily: usize, weekly: usize, monthly: usize) -> HashSet<String> {
    let mut dated: Vec<(NaiveDateTime, String)> = Vec::new();
    let mut keep = HashSet::new();
    for n in names {
        let norm = format!("{}.xz", n.strip_suffix(".xz").unwrap_or(n));
        if let Ok(t) = NaiveDateTime::parse_from_str(&norm, "archive-%Y%m%d-%H%M%S.sqlite3.xz") {
            dated.push((t, n.clone()));
        } else {
            keep.insert(n.clone());
        }
    }
    dated.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, n) in dated.iter().take(daily) {
        keep.insert(n.clone());
    }
    let mut seen_weeks = HashSet::new();
    for (t, n) in &dated {
        let iso_w = t.iso_week();
        let bucket = (iso_w.year(), iso_w.week());
        if !seen_weeks.contains(&bucket) && seen_weeks.len() < weekly {
            seen_weeks.insert(bucket);
            keep.insert(n.clone());
        }
    }
    let mut seen_months = HashSet::new();
    for (t, n) in &dated {
        let bucket = (t.year(), t.month());
        if !seen_months.contains(&bucket) && seen_months.len() < monthly {
            seen_months.insert(bucket);
            keep.insert(n.clone());
        }
    }
    keep
}

fn remove_stale_entries(d: &Path, matches: impl Fn(&str) -> bool) -> usize {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|x| x.as_secs_f64())
        .unwrap_or(0.0);
    let cutoff = now - STALE_TMP_SECONDS as f64;
    let mut removed = 0;
    if let Ok(rd) = fs::read_dir(d) {
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !matches(&name) {
                continue;
            }
            let Ok(md) = fs::symlink_metadata(&p) else {
                continue;
            };
            let mt = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|x| x.as_secs_f64())
                .unwrap_or(now);
            if mt >= cutoff {
                continue;
            }
            let ok = if md.is_dir() {
                fs::remove_dir_all(&p).is_ok()
            } else {
                fs::remove_file(&p).is_ok()
            };
            removed += usize::from(ok);
        }
    }
    removed
}

fn remove_stale_partials(d: &Path) -> usize {
    remove_stale_entries(d, |name| {
        name.starts_with(TMP_PREFIX) || (name.starts_with("archive-") && name.contains(".partial"))
    })
}

pub fn remove_stale_verify_dirs(tmp: Option<&Path>) -> usize {
    let d = tmp.map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
    remove_stale_entries(&d, |name| name.starts_with(VERIFY_TMP_PREFIX))
}

pub fn backup_if_due(conn: &Connection, every_hours: f64) -> Result<Option<PathBuf>, ArchiveError> {
    let d = paths::backups_dir();
    let scheduled: Vec<PathBuf> = list_backups(Some(&d))
        .into_iter()
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(is_scheduled_backup))
        .collect();
    let now_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|x| x.as_secs_f64())
        .unwrap_or(0.0);
    if let Some(last) = scheduled.last() {
        if every_hours > 0.0 && now_s - mtime_secs(last) < every_hours * 3600.0 {
            return Ok(None);
        }
    }
    remove_stale_partials(&d);
    remove_stale_verify_dirs(None);
    let pages: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))? as u64;
    let psize: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get::<_, i64>(0))? as u64;
    let size = pages.saturating_mul(psize);
    let free = paths::disk_free_bytes(&d);
    if free.saturating_sub(size) < MIN_FREE_BYTES {
        return Err(ArchiveError(format!(
            "Skipped the backup: only {:.1} GB free on disk.",
            free as f64 / 1e9
        )));
    }
    let stamp = Local::now().format("archive-%Y%m%d-%H%M%S.sqlite3.xz").to_string();
    let dest = backup(conn, &d.join(stamp))?;
    archive::set_meta(conn, "backup_error", "")?;
    let (_, warnings) = compress_old_backups_with_warnings(Some(&d));
    for w in &warnings {
        eprintln!("teams-cli: {w}");
    }
    if !warnings.is_empty() || archive::get_meta(conn, "backup_warning").is_some_and(|w| !w.is_empty()) {
        archive::set_meta(conn, "backup_warning", &warnings.join("; "))?;
    }
    let all_after = list_backups(Some(&d));
    let names: Vec<String> = all_after
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
        .collect();
    let keep = backups_to_keep(&names, BACKUP_DAILY, BACKUP_WEEKLY, BACKUP_MONTHLY);
    for old in all_after {
        if let Some(n) = old.file_name().and_then(|x| x.to_str()) {
            if !keep.contains(n) {
                let _ = fs::remove_file(&old);
            }
        }
    }
    Ok(Some(dest))
}

fn check_schema(conn: &Connection) -> Vec<String> {
    let mut errs = Vec::new();
    let missing: Vec<&str> = CORE_TABLES
        .iter()
        .copied()
        .filter(|t| !db::table_exists(conn, t))
        .collect();
    if !missing.is_empty() {
        errs.push(format!(
            "not a teams-cli archive (missing tables: {})",
            missing.join(", ")
        ));
    }
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    if ver <= 0 {
        errs.push("archive schema version is 0 (uninitialized)".to_string());
    } else if ver > SCHEMA_VERSION {
        errs.push(format!(
            "archive schema v{ver} is newer than this teams-cli (v{SCHEMA_VERSION})"
        ));
    } else if ver >= 8 && db::table_exists(conn, "schema_migrations") {
        match crate::storage::migrations::history(conn) {
            Ok(hist) => {
                let recorded: HashSet<i64> = hist.iter().map(|m| m.version).collect();
                let missing_steps: Vec<String> = (1..=ver)
                    .filter(|v| !recorded.contains(v))
                    .map(|v| format!("v{v}"))
                    .collect();
                if !missing_steps.is_empty() {
                    errs.push(format!(
                        "schema_migrations is missing applied version(s): {}",
                        missing_steps.join(", ")
                    ));
                }
            }
            Err(e) => errs.push(format!("schema_migrations unreadable: {e}")),
        }
    }
    errs
}

fn check_database(conn: &Connection) -> (Map<String, Value>, Vec<String>) {
    let mut checks = Map::new();
    let mut errors = Vec::new();

    let integrity_res: Vec<String> = conn
        .prepare("PRAGMA integrity_check")
        .and_then(|mut s| s.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>())
        .unwrap_or_else(|e| vec![e.to_string()]);
    let int_ok = integrity_res == vec!["ok".to_string()];
    checks.insert("integrity".to_string(), Value::Bool(int_ok));
    if !int_ok {
        let joined = integrity_res.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
        errors.push(format!("integrity_check: {joined}"));
        return (checks, errors);
    }

    let schema_errs = check_schema(conn);
    checks.insert("schema".to_string(), Value::Bool(schema_errs.is_empty()));
    if !schema_errs.is_empty() {
        errors.extend(schema_errs);
        return (checks, errors);
    }

    for fts in ["messages_fts", "message_revisions_fts"] {
        if !db::table_exists(conn, fts) {
            continue;
        }
        match conn.execute(&format!("INSERT INTO {fts} ({fts}) VALUES ('integrity-check')"), []) {
            Ok(_) => {
                checks.insert(fts.to_string(), Value::Bool(true));
            }
            Err(e) => {
                checks.insert(fts.to_string(), Value::Bool(false));
                errors.push(format!("{fts}: {e}"));
            }
        }
    }

    if db::table_exists(conn, "record_versions") {
        let mut total = 0usize;
        let mut bad = 0usize;
        match conn.prepare("SELECT id, data FROM record_versions") {
            Ok(mut stmt) => match stmt.query([]) {
                Ok(mut rows) => loop {
                    match rows.next() {
                        Ok(Some(r)) => {
                            total += 1;
                            let id: i64 = r.get(0).unwrap_or(0);
                            let decoded = r
                                .get_ref(1)
                                .map_err(|e| e.to_string())
                                .and_then(archive::decode_record_ref);
                            if let Err(e) = decoded {
                                bad += 1;
                                if bad <= MAX_REPORTED_BAD_RECORDS {
                                    errors.push(format!("record_versions id {id} does not decode: {e}"));
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            errors.push(format!("record_versions scan error: {e}"));
                            break;
                        }
                    }
                },
                Err(e) => errors.push(format!("record_versions query error: {e}")),
            },
            Err(e) => errors.push(format!("record_versions prepare error: {e}")),
        }
        if bad > MAX_REPORTED_BAD_RECORDS {
            errors.push(format!(
                "... and {} more record_versions rows that do not decode",
                bad - MAX_REPORTED_BAD_RECORDS
            ));
        }
        checks.insert("recordVersions".to_string(), json!({"rows": total, "bad": bad}));
    }
    (checks, errors)
}

fn materialize(src: &Path, dest: &Path) -> Result<(), ArchiveError> {
    let is_xz = file_name_of(src)?.ends_with(".xz");
    let fin = File::open(src).map_err(io_err)?;
    let mut fout = create_private(dest)?;
    let res = if is_xz {
        std::io::copy(&mut XzDecoder::new(fin), &mut fout)
    } else {
        let mut fin = fin;
        std::io::copy(&mut fin, &mut fout)
    };
    if let Err(e) = res.and_then(|_| fout.sync_all()) {
        drop(fout);
        let _ = fs::remove_file(dest);
        return Err(io_err(e));
    }
    Ok(())
}

fn quick_check_backup(p: &Path) -> Value {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let size = p.metadata().map(|m| m.len()).unwrap_or(0);
    let res = (|| -> Result<(), ArchiveError> {
        if name.ends_with(".xz") {
            let td = tempfile::Builder::new()
                .prefix(VERIFY_TMP_PREFIX)
                .tempdir()
                .map_err(|e| ArchiveError(e.to_string()))?;
            let plain = td.path().join("backup.sqlite3");
            materialize(p, &plain)?;
            verify_sqlite(&plain)?;
        } else {
            let ro = db::connect_ro(Some(p))?;
            let mut stmt = ro.prepare("PRAGMA quick_check")?;
            let out: Vec<String> = stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            if out != vec!["ok".to_string()] {
                return Err(ArchiveError(format!("quick_check: {}", out.join("; "))));
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => json!({"name": name, "ok": true, "sizeBytes": size}),
        Err(e) => json!({"name": name, "ok": false, "sizeBytes": size, "error": e.to_string()}),
    }
}

pub fn verify_archive(conn: Option<&Connection>, target: Option<&Path>, check_backups: bool) -> Value {
    if let Some(t) = target {
        let mut errors = Vec::new();
        let mut checks = Map::new();
        let name = t.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !t.is_file() {
            errors.push(format!("{} does not exist", t.display()));
        } else if !is_backup_name(name) {
            errors.push(format!("{name} is not a .sqlite3 or .sqlite3.xz file"));
        } else {
            match tempfile::Builder::new().prefix(VERIFY_TMP_PREFIX).tempdir() {
                Ok(td) => {
                    let plain = td.path().join("verify.sqlite3");
                    match materialize(t, &plain).and_then(|_| Connection::open(&plain).map_err(ArchiveError::from)) {
                        Ok(c) => {
                            let (ch, er) = check_database(&c);
                            checks = ch;
                            errors = er;
                        }
                        Err(e) => errors.push(e.to_string()),
                    }
                }
                Err(e) => errors.push(format!("could not create verify tempdir: {e}")),
            }
        }
        return json!({
            "ok": errors.is_empty(),
            "target": t.to_string_lossy(),
            "checks": checks,
            "errors": errors,
        });
    }

    let mut checks = Map::new();
    let mut errors = Vec::new();
    let archive_file: String;
    if let Some(c) = conn {
        archive_file = db::db_file(c);
        let (ch, er) = check_database(c);
        checks = ch;
        errors = er;
    } else {
        let ap = paths::archive_path();
        archive_file = ap.to_string_lossy().into_owned();
        if !ap.is_file() {
            errors.push(format!("Archive not found at {}", ap.display()));
        } else {
            match Connection::open(&ap) {
                Ok(c) => {
                    let _ = c.execute_batch("PRAGMA busy_timeout = 30000;");
                    let (ch, er) = check_database(&c);
                    checks = ch;
                    errors = er;
                }
                Err(e) => {
                    checks.insert("integrity".to_string(), Value::Bool(false));
                    errors.push(e.to_string());
                }
            }
        }
    }

    let mut backups_out = Vec::new();
    if check_backups {
        for p in list_backups(None) {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.starts_with(BEFORE_RESTORE_PREFIX) && name.ends_with(".sqlite3") {
                continue;
            }
            let info = quick_check_backup(&p);
            if info.get("ok") != Some(&Value::Bool(true)) {
                errors.push(format!(
                    "backup {name}: {}",
                    info.get("error").and_then(|v| v.as_str()).unwrap_or("failed")
                ));
            }
            backups_out.push(info);
        }
    }
    json!({
        "ok": errors.is_empty(),
        "archive": archive_file,
        "checks": checks,
        "backups": backups_out,
        "errors": errors,
    })
}

pub fn restore_backup(backup_path: &Path, dest: Option<&Path>, safety_backup: bool) -> Result<Value, ArchiveError> {
    let dest = dest.map(|p| p.to_path_buf()).unwrap_or_else(paths::archive_path);
    let bname = backup_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if !backup_path.is_file() {
        return Err(ArchiveError(format!("{} does not exist", backup_path.display())));
    }
    if !is_backup_name(bname) {
        return Err(ArchiveError(format!("{bname} is not a .sqlite3 or .sqlite3.xz backup")));
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| ArchiveError(e.to_string()))?;
    }
    let cand_name = format!("{}.restore-partial", file_name_of(&dest)?);
    let candidate = dest.with_file_name(cand_name);
    let _ = fs::remove_file(&candidate);

    let swap_res = (|| -> Result<Option<PathBuf>, ArchiveError> {
        materialize(backup_path, &candidate).map_err(|e| ArchiveError(format!("Could not read {bname}: {e}")))?;
        let (int_res, schema_errs) = match (|| -> Result<(Vec<String>, Vec<String>), rusqlite::Error> {
            let c = Connection::open(&candidate)?;
            c.execute_batch("PRAGMA journal_mode = DELETE;")?;
            let mut stmt = c.prepare("PRAGMA integrity_check")?;
            let r: Vec<String> = stmt.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
            let se = if r == vec!["ok".to_string()] {
                check_schema(&c)
            } else {
                Vec::new()
            };
            Ok((r, se))
        })() {
            Ok(pair) => pair,
            Err(e) => (vec![e.to_string()], Vec::new()),
        };
        if int_res != vec!["ok".to_string()] {
            return Err(ArchiveError(format!(
                "{bname} failed its integrity check ({}); the current archive was left untouched.",
                int_res.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
            )));
        }
        if !schema_errs.is_empty() {
            return Err(ArchiveError(format!(
                "{bname} is not a compatible archive ({}); the current archive was left untouched.",
                schema_errs.join("; ")
            )));
        }
        let safety = if safety_backup && dest.exists() {
            safety_copy(&dest)?
        } else {
            None
        };
        let wal = PathBuf::from(format!("{}-wal", dest.display()));
        let shm = PathBuf::from(format!("{}-shm", dest.display()));
        let _ = fs::remove_file(&wal);
        let _ = fs::remove_file(&shm);
        let _ = fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600));
        paths::fsync_file(&candidate).map_err(|e| ArchiveError(e.to_string()))?;
        fs::rename(&candidate, &dest).map_err(|e| ArchiveError(e.to_string()))?;
        if let Some(parent) = dest.parent() {
            paths::fsync_dir(parent);
        }
        Ok(safety)
    })();

    let _ = fs::remove_file(&candidate);
    let safety = swap_res?;

    let c = db::connect_ro(Some(&dest))?;
    let messages: i64 = c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let chats: i64 = c.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?;
    Ok(json!({
        "restoredFrom": backup_path.to_string_lossy(),
        "archive": dest.to_string_lossy(),
        "safetyBackup": safety.map(|p| p.to_string_lossy().into_owned()),
        "messages": messages,
        "chats": chats,
    }))
}

fn safety_copy(dest: &Path) -> Result<Option<PathBuf>, ArchiveError> {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let target = paths::backups_dir().join(format!("archive-before-restore-{stamp}.sqlite3.xz"));
    let try_clean = (|| -> Result<Option<PathBuf>, ArchiveError> {
        let c = Connection::open(dest)?;
        let _ = c.execute_batch("PRAGMA busy_timeout = 30000;");
        if !(db::table_exists(&c, "messages") && archive::has_data(&c)) {
            return Ok(None);
        }
        Ok(Some(backup(&c, &target)?))
    })();
    match try_clean {
        Ok(res) => Ok(res),
        Err(_) => {
            let raw = paths::backups_dir().join(format!("corrupt-before-restore-{stamp}.sqlite3"));
            if raw.exists() {
                return Err(ArchiveError(format!("{} already exists", raw.display())));
            }
            let mut fin = File::open(dest).map_err(io_err)?;
            let mut fout = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&raw)
                .map_err(io_err)?;
            std::io::copy(&mut fin, &mut fout).map_err(io_err)?;
            fout.sync_all().map_err(io_err)?;
            let wal = PathBuf::from(format!("{}-wal", dest.display()));
            if wal.exists() {
                let raw_wal = PathBuf::from(format!("{}-wal", raw.display()));
                let _ = fs::copy(&wal, &raw_wal);
            }
            Ok(Some(raw))
        }
    }
}
