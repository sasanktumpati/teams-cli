pub mod chats;
pub mod messages;
pub mod presence;

use crate::daemon::pool::{PoolAcquireError, ReadConnPool};
use crate::live::error::LiveError;
use crate::live::policy::{Action, Principal, Scope, WriteRateLimiter};
use crate::live::session::SessionCache;
use crate::live::transport::{ServiceEndpoints, Transport};
use crate::query::resolve::ResolveScope;
use rusqlite::Connection;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn resolve_scope(principal: &Principal) -> ResolveScope<'_> {
    if principal.is_cli {
        ResolveScope::unrestricted()
    } else {
        ResolveScope {
            allow_archive_search: principal.has_scope(Scope::Read),
            allow_chats: principal.allow_chats.as_deref(),
        }
    }
}

pub enum ArchiveSource<'a> {
    None,
    Conn(&'a Connection),
    Pool(&'a ReadConnPool),
}

impl ArchiveSource<'_> {
    pub fn with_conn<T>(&self, f: impl FnOnce(Option<&Connection>) -> Result<T, LiveError>) -> Result<T, LiveError> {
        match self {
            ArchiveSource::None => f(None),
            ArchiveSource::Conn(c) => f(Some(c)),
            ArchiveSource::Pool(pool) => match pool.acquire() {
                Ok((conn, sig)) => {
                    let res = f(Some(&conn));
                    pool.release(conn, sig);
                    res
                }
                Err(PoolAcquireError::MissingArchive(_)) => f(None),
                Err(PoolAcquireError::Migrating(msg)) => Err(LiveError::new(
                    crate::live::error::LiveErrorKind::Unavailable,
                    "migrating",
                    msg,
                )),
                Err(PoolAcquireError::Other(msg)) => Err(LiveError::internal(msg)),
            },
        }
    }
}

#[derive(Clone)]
pub struct LiveService {
    pub session: Arc<SessionCache>,
    pub transport: Transport,
    pub rate_limiter: Arc<WriteRateLimiter>,
    pub daemon_writes_enabled: bool,
}

impl Default for LiveService {
    fn default() -> Self {
        Self::new(ServiceEndpoints::default(), false)
    }
}

impl LiveService {
    pub fn new(endpoints: ServiceEndpoints, daemon_writes_enabled: bool) -> Self {
        Self {
            session: Arc::new(SessionCache::new()),
            transport: Transport::new(endpoints),
            rate_limiter: Arc::new(WriteRateLimiter::default()),
            daemon_writes_enabled,
        }
    }

    pub fn with_session(session: Arc<SessionCache>, endpoints: ServiceEndpoints, daemon_writes_enabled: bool) -> Self {
        Self {
            session,
            transport: Transport::new(endpoints),
            rate_limiter: Arc::new(WriteRateLimiter::default()),
            daemon_writes_enabled,
        }
    }

    pub fn status(&self) -> Value {
        let mut s = self.session.status();
        if let Some(obj) = s.as_object_mut() {
            obj.insert("writesEnabled".to_string(), Value::Bool(self.daemon_writes_enabled));
        }
        s
    }

    pub(crate) fn check_write_rate_limit(&self, principal: &Principal, action: Action) -> Result<(), LiveError> {
        if !principal.is_cli && action.is_write() {
            self.rate_limiter.check_and_record(&principal.id)?;
        }
        Ok(())
    }

    pub(crate) fn track_upstream<T>(&self, res: Result<T, LiveError>) -> Result<T, LiveError> {
        if let Err(ref e) = res {
            if matches!(e.kind, crate::live::error::LiveErrorKind::Unauthorized) {
                self.session.invalidate();
            }
        }
        res
    }
}
