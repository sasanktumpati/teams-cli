use crate::formatters::Quoted;
use crate::live::cookies;
use crate::live::error::LiveError;
use serde_json::Value;
use std::io::Read;
use std::time::Duration;
use url::Url;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_TIMEOUT: Duration = Duration::from_secs(60);
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

pub const DEFAULT_CHATSVC_BASE: &str = "https://teams.microsoft.com";
pub const DEFAULT_AMS_BASE: &str = "https://us-api.asm.skype.com";
pub const DEFAULT_PRESENCE_BASE: &str = "https://presence.teams.microsoft.com";

#[derive(Debug, Clone)]
pub struct ServiceEndpoints {
    pub chatsvc_base: String,
    pub ams_base: String,
    pub presence_base: String,
    pub sharepoint_base_override: Option<String>,
}

impl Default for ServiceEndpoints {
    fn default() -> Self {
        Self {
            chatsvc_base: DEFAULT_CHATSVC_BASE.to_string(),
            ams_base: DEFAULT_AMS_BASE.to_string(),
            presence_base: DEFAULT_PRESENCE_BASE.to_string(),
            sharepoint_base_override: None,
        }
    }
}

pub fn agent(read_timeout: Duration) -> ureq::Agent {
    agent_with_timeouts(CONNECT_TIMEOUT, read_timeout)
}

pub fn agent_with_timeouts(connect_timeout: Duration, read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(connect_timeout)
        .timeout_read(read_timeout)
        .timeout_write(read_timeout)
        .build()
}

pub fn is_allowed_endpoint_url(url_str: &str) -> bool {
    Url::parse(url_str).is_ok_and(|u| match u.scheme() {
        "https" => cookies::is_trusted_host(u.host_str()),
        "http" => matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    })
}

pub fn is_safe_regional_redirect(orig_url: &str, location: &str) -> Option<String> {
    let orig = Url::parse(orig_url).ok()?;
    let next = orig.join(location).ok()?;
    let next_str = next.as_str();
    if !orig.scheme().eq_ignore_ascii_case("https")
        || !next.scheme().eq_ignore_ascii_case("https")
        || next.port_or_known_default() != Some(443)
        || !is_allowed_endpoint_url(next_str)
    {
        return None;
    }
    let orig_host = orig.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    let next_host = next.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if cookies::is_login_host(Some(&next_host)) {
        return None;
    }
    if !next_host.ends_with(&format!(".{orig_host}")) {
        return None;
    }
    Some(next_str.to_string())
}

const MAX_REGIONAL_REDIRECTS: usize = 2;

pub fn validate_endpoint_url(url_str: &str) -> Result<(), LiveError> {
    if !is_allowed_endpoint_url(url_str) {
        return Err(LiveError::bad(format!(
            "Refusing to send credentials to untrusted endpoint {}.",
            Quoted(url_str)
        )));
    }
    Ok(())
}

pub fn redact_secrets(text: &str, secrets: &[&str]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        if !s.is_empty() {
            out = out.replace(s, "[redacted]");
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub location: Option<String>,
}

impl HttpResponse {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

#[derive(Clone, Default)]
pub struct Transport {
    pub endpoints: ServiceEndpoints,
    shared_agent: std::sync::Arc<std::sync::OnceLock<ureq::Agent>>,
}

impl Transport {
    pub fn new(endpoints: ServiceEndpoints) -> Self {
        Self {
            endpoints,
            shared_agent: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    fn get_agent(&self, timeout: Duration) -> std::borrow::Cow<'_, ureq::Agent> {
        if timeout == WRITE_TIMEOUT {
            std::borrow::Cow::Borrowed(self.shared_agent.get_or_init(|| agent(WRITE_TIMEOUT)))
        } else {
            std::borrow::Cow::Owned(agent(timeout))
        }
    }

    pub fn request_bytes(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        secrets: &[&str],
        timeout: Duration,
    ) -> Result<HttpResponse, LiveError> {
        validate_endpoint_url(url)?;
        let ag = self.get_agent(timeout);
        let mut current_url = url.to_string();
        for hop in 0..=MAX_REGIONAL_REDIRECTS {
            let mut req = ag.request(method, &current_url);
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("User-Agent")) {
                req = req.set("User-Agent", cookies::USER_AGENT);
            }
            for (k, v) in headers {
                req = req.set(k, v);
            }
            let resp = match body {
                Some(bytes) => {
                    req = req.set("Content-Length", &bytes.len().to_string());
                    req.send_bytes(bytes)
                }
                None => req.call(),
            };
            let (status, location, raw_body) = match resp {
                Ok(r) => {
                    let st = r.status();
                    let loc = r.header("Location").map(String::from);
                    let mut buf = String::new();
                    let _ = r.into_reader().take(2 * 1024 * 1024).read_to_string(&mut buf);
                    (st, loc, buf)
                }
                Err(ureq::Error::Status(code, r)) => {
                    if matches!(code, 301 | 302 | 303 | 307 | 308) {
                        let loc = r.header("Location").map(String::from);
                        (code, loc, String::new())
                    } else if matches!(code, 401 | 403) {
                        return Err(LiveError::auth(format!(
                            "Authentication failed (HTTP {code}). Open Microsoft Teams to refresh your session, then try again."
                        )));
                    } else {
                        let mut err_body = String::new();
                        let _ = r.into_reader().take(512).read_to_string(&mut err_body);
                        let safe_body = redact_secrets(err_body.trim(), secrets);
                        let detail = if safe_body.is_empty() {
                            format!("Microsoft service request failed (HTTP {code}).")
                        } else {
                            format!("Microsoft service request failed (HTTP {code}): {safe_body}")
                        };
                        return Err(LiveError::upstream(detail));
                    }
                }
                Err(ureq::Error::Transport(t)) => {
                    let reason = redact_secrets(&t.to_string(), secrets);
                    return Err(LiveError::upstream(format!("Request failed: {reason}")));
                }
            };

            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop < MAX_REGIONAL_REDIRECTS && matches!(status, 301 | 302 | 307 | 308) {
                    if let Some(ref loc) = location {
                        if let Some(next_url) = is_safe_regional_redirect(url, loc) {
                            current_url = next_url;
                            continue;
                        }
                    }
                }
                let target_hint = location
                    .as_deref()
                    .and_then(|l| Url::parse(l).ok())
                    .and_then(|u| u.host_str().map(|h| format!(" ({status} -> {h}{})", u.path())))
                    .unwrap_or_else(|| format!(" (HTTP {status})"));
                return Err(LiveError::upstream(format!(
                    "Unexpected redirect{target_hint} from Microsoft service; refusing to forward credentials."
                )));
            }

            return Ok(HttpResponse {
                status,
                body: raw_body,
                location,
            });
        }

        Err(LiveError::upstream(
            "Too many redirects from Microsoft service; refusing to forward credentials.",
        ))
    }

    pub fn request_json(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        payload: Option<&Value>,
        secrets: &[&str],
    ) -> Result<HttpResponse, LiveError> {
        let mut hdrs: Vec<(&str, &str)> = headers.to_vec();
        hdrs.push(("Accept", "application/json"));
        let serialized = payload.map(crate::json_codec::to_string);
        if serialized.is_some() {
            hdrs.push(("Content-Type", "application/json"));
        }
        self.request_bytes(
            method,
            url,
            &hdrs,
            serialized.as_deref().map(str::as_bytes),
            secrets,
            WRITE_TIMEOUT,
        )
    }
}
