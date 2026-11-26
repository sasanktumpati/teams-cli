use crate::live::cookies::{self, AuthError, Cookie};
use crate::live::error::LiveError;
use crate::timeutil;
use regex::Regex;
use serde_json::{json, Value};
use std::fmt;
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

static REGION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9-]+$").unwrap());

const TOKEN_REFRESH_LEAD_SECS: f64 = 120.0;
const COOKIE_CACHE_TTL_SECS: u64 = 300;
const NEGATIVE_CACHE_SECS: u64 = 60;
const MISSING_CREDENTIAL_RETRY_SECS: u64 = 5;

type CookieLoader = Arc<dyn Fn(f64) -> Result<Vec<Cookie>, AuthError> + Send + Sync>;

#[derive(Clone)]
pub struct SkypeToken {
    pub token: String,
    pub region: String,
    pub expires_at: f64,
    pub skype_id: String,
    pub tenant_id: String,
    pub display_name: String,
}

impl fmt::Debug for SkypeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkypeToken")
            .field("region", &self.region)
            .field("expires_at", &self.expires_at)
            .field("skype_id", &self.skype_id)
            .field("tenant_id", &self.tenant_id)
            .field("display_name", &self.display_name)
            .finish()
    }
}

#[derive(Clone)]
pub struct TeamsBearerToken {
    pub token: String,
    pub audience: String,
    pub expires_at: f64,
    pub tenant_id: String,
}

impl fmt::Debug for TeamsBearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TeamsBearerToken")
            .field("audience", &self.audience)
            .field("expires_at", &self.expires_at)
            .field("tenant_id", &self.tenant_id)
            .finish()
    }
}

#[derive(Clone)]
pub struct SharePointSession {
    pub personal_host: String,
    pub team_host: String,
    pub personal_cookie_header: Option<String>,
    pub team_cookie_header: Option<String>,
}

impl fmt::Debug for SharePointSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharePointSession")
            .field("personal_host", &self.personal_host)
            .field("team_host", &self.team_host)
            .field("has_personal_cookie", &self.personal_cookie_header.is_some())
            .field("has_team_cookie", &self.team_cookie_header.is_some())
            .finish()
    }
}

#[derive(Clone)]
pub struct TeamsSession {
    pub skypetoken: Option<SkypeToken>,
    pub bearer: Option<TeamsBearerToken>,
    pub sharepoint: Option<SharePointSession>,
    pub loaded_at: Instant,
    pub expired_skypetoken_seen: bool,
}

fn from_trusted_host(c: &Cookie) -> bool {
    cookies::is_trusted_host(Some(c.host_key.trim_start_matches('.')))
}

fn extract_bearer_from_cookie_value(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let mut cands = Vec::new();
    let unq = cookies::urlencoding_decode(trimmed);
    if unq.contains("Bearer=") {
        for part in unq.split('&') {
            if let Some(v) = part.strip_prefix("Bearer=") {
                if !v.trim().is_empty() {
                    cands.push(v.trim().to_string());
                }
            }
        }
    }
    if cands.is_empty() && !trimmed.is_empty() {
        cands.push(trimmed.to_string());
    }
    cands
}

fn extract_display_name(loaded: &[Cookie]) -> String {
    let mut cands: Vec<&Cookie> = loaded
        .iter()
        .filter(|c| c.name == "authtoken_asm" && !c.value.trim().is_empty() && from_trusted_host(c))
        .collect();
    cands.sort_by_key(|c| c.partitioned);
    for c in cands {
        for cand in extract_bearer_from_cookie_value(&c.value) {
            if let Ok(payload) = cookies::decode_jwt_payload(&cand) {
                if let Some(name) = payload.get("name").and_then(|v| v.as_str()).map(str::trim) {
                    if !name.is_empty() {
                        return name.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

fn parse_skypetoken(raw_token: &str, now_ts: f64) -> Result<SkypeToken, (bool, AuthError)> {
    let payload = cookies::decode_jwt_payload(raw_token).map_err(|_| {
        (
            false,
            AuthError(
                "Teams session token (skypetoken_asm) is malformed. \
                 Open Microsoft Teams to refresh your session, then try again."
                    .to_string(),
            ),
        )
    })?;
    let region = payload
        .get("rgn")
        .and_then(|v| v.as_str())
        .filter(|r| REGION_RE.is_match(r))
        .ok_or_else(|| {
            (
                false,
                AuthError("Teams session token has an invalid or missing region claim.".to_string()),
            )
        })?
        .to_string();
    let exp = payload
        .get("exp")
        .and_then(|v| if v.is_boolean() { None } else { v.as_f64() })
        .ok_or_else(|| {
            (
                false,
                AuthError(
                    "Teams session token has an invalid or missing expiration claim. \
                     Open Microsoft Teams to refresh your session, then try again."
                        .to_string(),
                ),
            )
        })?;
    if exp <= now_ts {
        return Err((
            true,
            AuthError(
                "Teams session token has expired. \
                 Open Microsoft Teams to refresh your session, then try again."
                    .to_string(),
            ),
        ));
    }
    Ok(SkypeToken {
        token: raw_token.to_string(),
        region,
        expires_at: exp,
        skype_id: payload
            .get("skypeid")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        tenant_id: payload.get("tid").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        display_name: String::new(),
    })
}

pub fn load_skypetoken(cookies_opt: Option<&[Cookie]>, now_opt: Option<f64>) -> Result<SkypeToken, AuthError> {
    let now_ts = now_opt.unwrap_or_else(cookies::now_unix_f64);
    let owned: Vec<Cookie>;
    let loaded: &[Cookie] = match cookies_opt {
        Some(c) => c,
        None => {
            owned = cookies::load_cookies(None, None, Some(now_ts))?;
            &owned
        }
    };
    extract_skypetoken(loaded, now_ts).map_err(|(_, e)| e)
}

fn extract_skypetoken(loaded: &[Cookie], now_ts: f64) -> Result<SkypeToken, (bool, AuthError)> {
    let mut cands: Vec<&Cookie> = loaded
        .iter()
        .filter(|c| c.name == "skypetoken_asm" && !c.value.trim().is_empty() && from_trusted_host(c))
        .collect();
    if cands.is_empty() {
        return Err((
            false,
            AuthError(
                "Teams session token (skypetoken_asm) not found. \
                 Open Microsoft Teams and sign in, then try again."
                    .to_string(),
            ),
        ));
    }
    cands.sort_by_key(|c| c.partitioned);
    let mut best: Option<(bool, SkypeToken)> = None;
    let mut first_err: Option<(bool, AuthError)> = None;
    for c in cands {
        match parse_skypetoken(c.value.trim(), now_ts) {
            Ok(tok) => {
                let better = match &best {
                    None => true,
                    Some((bp, bt)) => (c.partitioned, -tok.expires_at) < (*bp, -bt.expires_at),
                };
                if better {
                    best = Some((c.partitioned, tok));
                }
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    match best {
        Some((_, mut tok)) => {
            tok.display_name = extract_display_name(loaded);
            Ok(tok)
        }
        None => Err(first_err.unwrap_or_else(|| {
            (
                false,
                AuthError("Teams session token (skypetoken_asm) not found.".to_string()),
            )
        })),
    }
}

fn extract_teams_bearer(loaded: &[Cookie], now_ts: f64) -> Option<TeamsBearerToken> {
    let mut cands: Vec<&Cookie> = loaded
        .iter()
        .filter(|c| c.name == "authtoken" && !c.value.trim().is_empty() && from_trusted_host(c))
        .collect();
    cands.sort_by_key(|c| c.partitioned);
    let mut best: Option<(bool, bool, f64, TeamsBearerToken)> = None;
    for c in cands {
        for cand in extract_bearer_from_cookie_value(&c.value) {
            let Ok(payload) = cookies::decode_jwt_payload(&cand) else {
                continue;
            };
            let exp = payload
                .get("exp")
                .and_then(|v| if v.is_boolean() { None } else { v.as_f64() })
                .unwrap_or(0.0);
            if exp > 0.0 && exp <= now_ts {
                continue;
            }
            let aud = payload.get("aud").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let tid = payload.get("tid").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let aud_lc = aud.to_ascii_lowercase();
            let non_teams_aud = !(aud_lc.contains("spaces.skype.com")
                || aud_lc.contains("presence.teams.microsoft.com")
                || aud_lc.contains("teams.microsoft.com"));
            let rank = (c.partitioned, non_teams_aud, -exp);
            let better = match &best {
                None => true,
                Some((bp, ba, bneg_exp, _)) => rank < (*bp, *ba, *bneg_exp),
            };
            if better {
                best = Some((
                    c.partitioned,
                    non_teams_aud,
                    -exp,
                    TeamsBearerToken {
                        token: cand,
                        audience: aud,
                        expires_at: exp,
                        tenant_id: tid,
                    },
                ));
            }
        }
    }
    best.map(|(_, _, _, tok)| tok)
}

fn extract_sharepoint_session(loaded: &[Cookie], now_ts: f64) -> Option<SharePointSession> {
    let mut personal_host: Option<String> = None;
    let mut team_host: Option<String> = None;
    for c in loaded {
        if !matches!(c.name.as_str(), "SPOIDCRL" | "FedAuth")
            || c.value.trim().is_empty()
            || cookies::is_expired(c.expires_utc, now_ts)
        {
            continue;
        }
        let host = c.host_key.trim_start_matches('.').to_ascii_lowercase();
        if let Some(prefix) = host.strip_suffix("-my.sharepoint.com") {
            if !prefix.is_empty() && !prefix.contains('.') {
                personal_host.get_or_insert_with(|| host.clone());
                team_host.get_or_insert_with(|| format!("{prefix}.sharepoint.com"));
            }
        } else if let Some(prefix) = host.strip_suffix(".sharepoint.com") {
            if !prefix.is_empty() && !prefix.contains('.') && !prefix.ends_with("-admin") {
                team_host.get_or_insert_with(|| host.clone());
                personal_host.get_or_insert_with(|| format!("{prefix}-my.sharepoint.com"));
            }
        }
    }
    let personal_host = personal_host?;
    let team_host = team_host?;
    let has_auth_for = |target_host: &str| {
        loaded.iter().any(|c| {
            matches!(c.name.as_str(), "SPOIDCRL" | "FedAuth")
                && !c.value.trim().is_empty()
                && !cookies::is_expired(c.expires_utc, now_ts)
                && cookies::cookie_matches_host(&c.host_key, target_host)
        })
    };
    let personal_cookie_header = if has_auth_for(&personal_host) {
        let hdr = cookies::build_cookie_header(loaded, &format!("https://{personal_host}/"), Some(now_ts));
        (!hdr.is_empty()).then_some(hdr)
    } else {
        None
    };
    let team_cookie_header = if has_auth_for(&team_host) {
        let hdr = cookies::build_cookie_header(loaded, &format!("https://{team_host}/"), Some(now_ts));
        (!hdr.is_empty()).then_some(hdr)
    } else {
        None
    };
    if personal_cookie_header.is_none() && team_cookie_header.is_none() {
        return None;
    }
    Some(SharePointSession {
        personal_host,
        team_host,
        personal_cookie_header,
        team_cookie_header,
    })
}

pub fn extract_session_from_cookies(loaded: &[Cookie], now_ts: f64) -> TeamsSession {
    let (skypetoken, expired_skypetoken_seen) = match extract_skypetoken(loaded, now_ts) {
        Ok(tok) => (Some(tok), false),
        Err((expired, _)) => (None, expired),
    };
    let bearer = extract_teams_bearer(loaded, now_ts);
    let sharepoint = extract_sharepoint_session(loaded, now_ts);
    TeamsSession {
        skypetoken,
        bearer,
        sharepoint,
        loaded_at: Instant::now(),
        expired_skypetoken_seen,
    }
}

struct CacheInner {
    session: Option<TeamsSession>,
    last_error: Option<(Instant, String)>,
}

pub struct SessionCache {
    inner: Mutex<CacheInner>,
    override_session: Option<TeamsSession>,
    loader: Option<CookieLoader>,
    missing_credential_retry: Duration,
}

impl Default for SessionCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                session: None,
                last_error: None,
            }),
            override_session: None,
            loader: None,
            missing_credential_retry: Duration::from_secs(MISSING_CREDENTIAL_RETRY_SECS),
        }
    }

    pub fn with_static_session(session: TeamsSession) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                session: None,
                last_error: None,
            }),
            override_session: Some(session),
            loader: None,
            missing_credential_retry: Duration::from_secs(MISSING_CREDENTIAL_RETRY_SECS),
        }
    }

    pub fn with_loader_and_retry<F>(loader: F, missing_credential_retry: Duration) -> Self
    where
        F: Fn(f64) -> Result<Vec<Cookie>, AuthError> + Send + Sync + 'static,
    {
        Self {
            inner: Mutex::new(CacheInner {
                session: None,
                last_error: None,
            }),
            override_session: None,
            loader: Some(Arc::new(loader)),
            missing_credential_retry,
        }
    }

    pub fn invalidate(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.session = None;
        g.last_error = None;
    }

    pub fn get_session(&self) -> Result<TeamsSession, LiveError> {
        self.get_session_with_max_age(Duration::from_secs(COOKIE_CACHE_TTL_SECS))
    }

    fn get_session_with_max_age(&self, max_age: Duration) -> Result<TeamsSession, LiveError> {
        if let Some(ref s) = self.override_session {
            return Ok(s.clone());
        }
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now_ts = cookies::now_unix_f64();

        if let Some(ref cached) = g.session {
            let age_ok = cached.loaded_at.elapsed() < max_age;
            let skype_ok = cached
                .skypetoken
                .as_ref()
                .is_some_and(|t| t.expires_at > now_ts + TOKEN_REFRESH_LEAD_SECS);
            let bearer_ok = cached
                .bearer
                .as_ref()
                .is_none_or(|b| b.expires_at <= 0.0 || b.expires_at > now_ts + TOKEN_REFRESH_LEAD_SECS);
            if age_ok && skype_ok && bearer_ok {
                return Ok(cached.clone());
            }
        }

        if let Some((at, ref msg)) = g.last_error {
            let elapsed = at.elapsed();
            if elapsed < Duration::from_secs(NEGATIVE_CACHE_SECS) {
                let wait = NEGATIVE_CACHE_SECS.saturating_sub(elapsed.as_secs()).max(1);
                return Err(LiveError::auth_unavailable(msg.clone(), wait));
            }
        }

        let loaded_res = match &self.loader {
            Some(f) => f(now_ts),
            None => cookies::load_cookies(None, None, Some(now_ts)),
        };

        match loaded_res {
            Ok(loaded) => {
                let session = extract_session_from_cookies(&loaded, now_ts);
                g.session = Some(session.clone());
                g.last_error = None;
                Ok(session)
            }
            Err(e) => {
                g.last_error = Some((Instant::now(), e.0.clone()));
                Err(LiveError::auth_unavailable(e.0, NEGATIVE_CACHE_SECS))
            }
        }
    }

    pub fn skypetoken(&self) -> Result<SkypeToken, LiveError> {
        let s = self.get_session()?;
        if let Some(tok) = s.skypetoken {
            return Ok(tok);
        }
        if s.expired_skypetoken_seen {
            return Err(LiveError::session_expired(
                "Teams session token has expired. Open Microsoft Teams to refresh your session, then try again.",
            ));
        }
        Err(LiveError::auth(
            "Teams session token (skypetoken_asm) not found. Open Microsoft Teams and sign in, then try again.",
        ))
    }

    pub fn bearer_token(&self) -> Result<TeamsBearerToken, LiveError> {
        let mut s = self.get_session()?;
        if s.bearer.is_none() && self.override_session.is_none() {
            s = self.get_session_with_max_age(self.missing_credential_retry)?;
        }
        s.bearer.ok_or_else(|| {
            LiveError::auth(
                "Teams bearer token (authtoken) not found or expired. Open Microsoft Teams to refresh your session.",
            )
        })
    }

    pub fn sharepoint_session(&self) -> Result<SharePointSession, LiveError> {
        self.sharepoint_session_for(false)
    }

    pub fn sharepoint_session_for(&self, is_channel: bool) -> Result<SharePointSession, LiveError> {
        let mut s = self.get_session()?;
        let missing_needed = s.sharepoint.as_ref().is_none_or(|sp| {
            if is_channel {
                sp.team_cookie_header.is_none()
            } else {
                sp.personal_cookie_header.is_none()
            }
        });
        if missing_needed && self.override_session.is_none() {
            s = self.get_session_with_max_age(self.missing_credential_retry)?;
        }
        s.sharepoint.ok_or_else(|| {
            LiveError::auth(
                "SharePoint/OneDrive session cookie (SPOIDCRL) not found. Open Microsoft Teams and view a file or chat, then try again.",
            )
        })
    }

    pub fn status(&self) -> Value {
        let teams_running = is_teams_process_running();
        match self.get_session() {
            Ok(s) => {
                let skype_exp = s
                    .skypetoken
                    .as_ref()
                    .and_then(|t| timeutil::iso(Some((t.expires_at * 1000.0) as i64)));
                let bearer_exp = s
                    .bearer
                    .as_ref()
                    .and_then(|t| timeutil::iso(Some((t.expires_at * 1000.0) as i64)));
                json!({
                    "ok": s.skypetoken.is_some(),
                    "keychainOk": true,
                    "teamsRunning": teams_running,
                    "skypetokenReady": s.skypetoken.is_some(),
                    "skypetokenExpired": s.expired_skypetoken_seen,
                    "skypetokenExpiresAt": skype_exp,
                    "region": s.skypetoken.as_ref().map(|t| t.region.as_str()),
                    "presenceReady": s.bearer.is_some(),
                    "presenceExpiresAt": bearer_exp,
                    "sharepointReady": s.sharepoint.is_some(),
                    "sharepointPersonalReady": s.sharepoint.as_ref().is_some_and(|sp| sp.personal_cookie_header.is_some()),
                    "sharepointTeamReady": s.sharepoint.as_ref().is_some_and(|sp| sp.team_cookie_header.is_some()),
                    "sharepointHost": s.sharepoint.as_ref().map(|sp| sp.personal_host.as_str()),
                })
            }
            Err(e) => json!({
                "ok": false,
                "keychainOk": false,
                "teamsRunning": teams_running,
                "error": e.message,
                "code": e.code,
            }),
        }
    }
}

pub fn preflight_keychain() -> Result<(), String> {
    cookies::get_keychain_aes_key().map(|_| ()).map_err(|e| e.0)
}

pub fn is_teams_process_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "MSTeams"])
        .output()
        .is_ok_and(|o| o.status.success())
}
