use crate::paths;
use crate::query::api;
use crate::storage::{archive, db};
use rusqlite::Connection;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const IDLE_CONN_TTL: Duration = Duration::from_secs(30);

pub type FileSig = (String, u64, u64);

#[derive(Debug)]
pub enum PoolAcquireError {
    MissingArchive(String),
    Migrating(String),
    Other(String),
}

struct PooledConn {
    sig: FileSig,
    conn: Connection,
    released_at: Instant,
}

pub struct ReadConnPool {
    max_size: usize,
    idle: Mutex<Vec<PooledConn>>,
    has_data_cache: Mutex<Option<FileSig>>,
}

impl ReadConnPool {
    pub fn new(max_size: usize) -> Self {
        Self {
            max_size,
            idle: Mutex::new(Vec::new()),
            has_data_cache: Mutex::new(None),
        }
    }

    fn file_sig() -> Option<FileSig> {
        let p = paths::archive_path();
        let st = fs::metadata(&p).ok()?;
        Some((p.to_string_lossy().into_owned(), st.dev(), st.ino()))
    }

    pub fn is_known_populated(&self, sig: Option<&FileSig>) -> bool {
        let Some(s) = sig else { return false };
        self.has_data_cache.lock().unwrap_or_else(|p| p.into_inner()).as_ref() == Some(s)
    }

    pub fn mark_populated(&self, sig: Option<&FileSig>) {
        if let Some(s) = sig {
            *self.has_data_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(s.clone());
        }
    }

    pub fn acquire(&self) -> Result<(Connection, Option<FileSig>), PoolAcquireError> {
        let p = paths::archive_path();
        let Some(sig) = Self::file_sig() else {
            return Err(PoolAcquireError::MissingArchive(api::no_archive_message(&p)));
        };
        let now = Instant::now();
        {
            let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
            while let Some(entry) = guard.pop() {
                if entry.sig == sig && now.duration_since(entry.released_at) < IDLE_CONN_TTL {
                    return Ok((entry.conn, Some(sig)));
                }
                db::shrink_memory(&entry.conn);
                drop(entry.conn);
            }
        }
        let conn = archive::connect_readonly(Some(&p)).map_err(|e| PoolAcquireError::Other(e.to_string()))?;
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| PoolAcquireError::Other(e.to_string()))?;
        if ver < archive::SCHEMA_VERSION {
            let err_path = p.with_file_name(".migrate.error");
            if let Ok(reason) = fs::read_to_string(&err_path) {
                let msg = reason.trim();
                if !msg.is_empty() {
                    return Err(PoolAcquireError::Other(format!("Archive schema upgrade failed: {msg}")));
                }
            }
            return Err(PoolAcquireError::Migrating(format!(
                "Archive schema v{ver} is upgrading to v{}; retry shortly.",
                archive::SCHEMA_VERSION
            )));
        }
        Ok((conn, Some(sig)))
    }

    pub fn release(&self, conn: Connection, sig: Option<FileSig>) {
        let Some(s) = sig else { return };
        if Self::file_sig().as_ref() != Some(&s) {
            db::shrink_memory(&conn);
            return;
        }
        db::shrink_memory(&conn);
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() < self.max_size {
            guard.push(PooledConn {
                sig: s,
                conn,
                released_at: Instant::now(),
            });
        }
    }

    pub fn reap_idle(&self) -> usize {
        let now = Instant::now();
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|entry| {
            let keep = now.duration_since(entry.released_at) < IDLE_CONN_TTL;
            if !keep {
                db::shrink_memory(&entry.conn);
            }
            keep
        });
        guard.len()
    }

    pub fn has_idle(&self) -> bool {
        !self.idle.lock().unwrap_or_else(|p| p.into_inner()).is_empty()
    }

    pub fn clear(&self) {
        let mut guard = self.idle.lock().unwrap_or_else(|p| p.into_inner());
        for entry in guard.drain(..) {
            db::shrink_memory(&entry.conn);
        }
        *self.has_data_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}
