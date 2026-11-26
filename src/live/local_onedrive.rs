use crate::paths;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const CACHE_TTL: Duration = Duration::from_secs(5);

struct CacheEntry {
    at: Instant,
    sig: Vec<(PathBuf, i64)>,
    files: HashMap<String, PathBuf>,
}

static CACHE: Mutex<Option<CacheEntry>> = Mutex::new(None);

fn onedrive_dirs() -> Vec<PathBuf> {
    if let Some(override_v) = env::var_os("TEAMS_CLI_ONEDRIVE_DIR") {
        let s = override_v.to_string_lossy();
        if !s.is_empty() {
            return s.split(':').filter(|p| !p.is_empty()).map(PathBuf::from).collect();
        }
    }
    let cloud = paths::home_dir().join("Library/CloudStorage");
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(&cloud) {
        let mut names: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("OneDrive"))
            .map(|e| e.path().join("Microsoft Teams Chat Files"))
            .collect();
        names.sort();
        out = names;
    }
    out
}

fn is_safe_filename(filename: &str) -> bool {
    if filename.is_empty() || filename.contains('\0') || filename.contains('/') || filename.contains('\\') {
        return false;
    }
    if filename == "." || filename == ".." {
        return false;
    }
    Path::new(filename).file_name().and_then(|s| s.to_str()) == Some(filename)
}

fn dir_signature(dirs: &[PathBuf]) -> Vec<(PathBuf, i64)> {
    dirs.iter()
        .map(|d| {
            let mt = fs::metadata(d)
                .map(|m| m.mtime() * 1_000_000_000 + m.mtime_nsec())
                .unwrap_or(-1);
            (d.clone(), mt)
        })
        .collect()
}

pub fn list_onedrive_files() -> HashMap<String, PathBuf> {
    let dirs = onedrive_dirs();
    let sig = dir_signature(&dirs);
    if let Ok(guard) = CACHE.lock() {
        if let Some(ref entry) = *guard {
            if entry.at.elapsed() < CACHE_TTL && entry.sig == sig {
                return entry.files.clone();
            }
        }
    }
    let mut found = HashMap::new();
    for d in &dirs {
        if !d.is_dir() {
            continue;
        }
        let Ok(d_resolved) = fs::canonicalize(d) else {
            continue;
        };
        let Ok(rd) = fs::read_dir(d) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if found.contains_key(&name) {
                continue;
            }
            let p = entry.path();
            let Ok(resolved) = fs::canonicalize(&p) else {
                continue;
            };
            if !resolved.starts_with(&d_resolved) || !resolved.is_file() {
                continue;
            }
            found.insert(name, resolved);
        }
    }
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some(CacheEntry {
            at: Instant::now(),
            sig,
            files: found.clone(),
        });
    }
    found
}

pub fn find_onedrive_file(filename: &str, index: Option<&HashMap<String, PathBuf>>) -> Option<PathBuf> {
    if !is_safe_filename(filename) {
        return None;
    }
    if let Some(idx) = index {
        return idx.get(filename).cloned();
    }
    for d in onedrive_dirs() {
        if !d.is_dir() {
            continue;
        }
        let Ok(d_resolved) = fs::canonicalize(&d) else {
            continue;
        };
        let cand = d.join(filename);
        let Ok(resolved) = fs::canonicalize(&cand) else {
            continue;
        };
        if resolved.starts_with(&d_resolved) && resolved.is_file() {
            return Some(cand);
        }
    }
    None
}
