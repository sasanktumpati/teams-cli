use crate::query::api::ApiError;
use crate::query::resolve::ResolveError;
use serde_json::{json, Value};
use std::borrow::Cow;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveErrorKind {
    BadInput,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    RateLimited,
    Unavailable,
    Upstream,
    Internal,
}

#[derive(Debug, Clone)]
pub struct LiveError {
    pub kind: LiveErrorKind,
    pub code: Cow<'static, str>,
    pub message: String,
    pub details: Option<Box<Value>>,
    pub retry_after_secs: Option<u64>,
}

impl LiveError {
    pub fn new(kind: LiveErrorKind, code: impl Into<Cow<'static, str>>, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: code.into(),
            message: message.into(),
            details: None,
            retry_after_secs: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }

    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::BadInput, "bad_param", message)
    }

    pub fn ambiguous(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Conflict, "ambiguous_target", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::NotFound, "not_found", message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Unauthorized, "auth_error", message)
    }

    pub fn session_expired(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Unavailable, "teams_session_expired", message)
    }

    pub fn auth_unavailable(message: impl Into<String>, retry_after_secs: u64) -> Self {
        Self::new(LiveErrorKind::Unavailable, "live_auth_unavailable", message).with_retry_after(retry_after_secs)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Forbidden, code, message)
    }

    pub fn rate_limited(retry_after_secs: u64) -> Self {
        Self::new(
            LiveErrorKind::RateLimited,
            "rate_limited",
            format!("Write rate limit exceeded. Retry in {retry_after_secs}s."),
        )
        .with_retry_after(retry_after_secs)
    }

    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Upstream, "upstream_error", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(LiveErrorKind::Internal, "internal", message)
    }

    pub fn http_status(&self) -> u16 {
        match self.kind {
            LiveErrorKind::BadInput => 400,
            LiveErrorKind::Unauthorized => 401,
            LiveErrorKind::Forbidden => 403,
            LiveErrorKind::NotFound => 404,
            LiveErrorKind::Conflict => 409,
            LiveErrorKind::RateLimited => 429,
            LiveErrorKind::Unavailable => 503,
            LiveErrorKind::Upstream => 502,
            LiveErrorKind::Internal => 500,
        }
    }

    pub fn cli_exit_code(&self) -> i32 {
        match self.kind {
            LiveErrorKind::BadInput => 2,
            _ => 1,
        }
    }

    pub fn to_json(&self) -> Value {
        let mut err = json!({
            "code": self.code,
            "message": self.message,
        });
        if let Some(ref d) = self.details {
            err["details"] = (**d).clone();
        }
        json!({ "error": err })
    }

    pub fn to_api_error(&self) -> ApiError {
        ApiError::with_status(self.code.as_ref(), self.message.clone(), self.http_status())
    }
}

impl fmt::Display for LiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for LiveError {}

impl From<ResolveError> for LiveError {
    fn from(err: ResolveError) -> Self {
        match err {
            ResolveError::BadInput(m) => LiveError::bad(m),
            ResolveError::NotFound(m) => LiveError::not_found(m),
            ResolveError::Ambiguous(m) => LiveError::ambiguous(m),
            ResolveError::ScopeRequired(m) => {
                LiveError::forbidden("scope_required", m).with_details(json!({ "requiredScope": "read" }))
            }
            ResolveError::Db(m) => LiveError::internal(m),
        }
    }
}
