use crate::formatters::Quoted;
use crate::json_codec;
use crate::live::auth;
use crate::storage::{archive, db};
use jiff::Timestamp;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

pub const ENABLE_ENV: &str = "TEAMS_CLI_ENABLE_SEND";
pub const ALLOWLIST_ENV: &str = "TEAMS_CLI_SEND_ALLOWLIST";

static DIRECT_CONV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^19:[^/\s]+@[a-zA-Z0-9._-]+$").unwrap());
static CLIENT_MSG_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{1,32}$").unwrap());
static REPLY_TO_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").unwrap());

#[derive(Debug, Clone)]
pub struct SendError(pub String);

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SendError {}

pub fn is_send_enabled() -> bool {
    let val = env::var(ENABLE_ENV).unwrap_or_default();
    matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

pub fn send_allowlist() -> Option<HashSet<String>> {
    let raw = env::var(ALLOWLIST_ENV).ok()?;
    let items: HashSet<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

pub fn is_conversation_allowed(conv_id: &str) -> bool {
    match send_allowlist() {
        None => true,
        Some(set) => set.contains(conv_id),
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub id: String,
    pub name: String,
    pub kind: String,
}

pub fn resolve_target(conn: Option<&Connection>, target: &str) -> Result<ResolvedTarget, SendError> {
    let q = target.trim();
    if q.is_empty() {
        return Err(SendError(
            "Target chat name or conversation id is required.".to_string(),
        ));
    }
    if let Some(c) = conn {
        if let Ok(Some(row)) = c
            .query_row("SELECT id, name, kind FROM conversations WHERE id = ?", [q], |r| {
                let id: String = r.get(0)?;
                let name: Option<String> = r.get(1)?;
                let kind: String = r.get(2)?;
                Ok(ResolvedTarget {
                    name: name.filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
                    id,
                    kind,
                })
            })
            .optional()
        {
            return Ok(row);
        }
    }
    if q == "48:notes" || DIRECT_CONV_RE.is_match(q) {
        return Ok(ResolvedTarget {
            id: q.to_string(),
            name: if q == "48:notes" {
                "Self-chat (notes)".to_string()
            } else {
                q.to_string()
            },
            kind: if q.starts_with("48:") { "system" } else { "chat" }.to_string(),
        });
    }
    let Some(c) = conn else {
        return Err(SendError(format!(
            "Cannot resolve chat name {} without an archive connection. Pass an exact conversation id.",
            Quoted(target)
        )));
    };
    let pat = db::like_pattern(q);
    let sql = format!(
        "SELECT c.id, c.name, c.kind, {} AS message_count, {} AS activity \
         FROM conversations c \
         WHERE (c.kind != 'system' AND c.name LIKE ? ESCAPE '\\') \
            OR (c.id = '48:notes' AND (c.name LIKE ? ESCAPE '\\' OR lower(?) = '48:notes')) \
         ORDER BY activity DESC, message_count DESC, c.id ASC",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    let mut stmt = c.prepare(&sql).map_err(|e| SendError(e.to_string()))?;
    let rows: Vec<(String, Option<String>, String, i64)> = stmt
        .query_map(params![pat, pat, q], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(|e| SendError(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| SendError(e.to_string()))?;
    if rows.is_empty() {
        return Err(SendError(format!(
            "No chat matches {}. Pass a conversation id from `teams chats`.",
            Quoted(target)
        )));
    }
    if rows.len() > 1 {
        let mut lines = vec![format!(
            "Ambiguous target {} matches {} chats; refusing to guess. Pass the exact conversation id:",
            Quoted(target),
            rows.len()
        )];
        for (id, name, kind, mc) in rows.iter().take(10) {
            let n = name.as_deref().unwrap_or(id);
            lines.push(format!(
                "  - {id}  (kind={kind}, messageCount={mc}, name={})",
                Quoted(n)
            ));
        }
        return Err(SendError(lines.join("\n")));
    }
    let (id, name, kind, _) = rows.into_iter().next().unwrap();
    Ok(ResolvedTarget {
        name: name.filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
        id,
        kind,
    })
}

fn html_escape_plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn format_plain_html(text: &str) -> String {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let escaped = html_escape_plain(&norm);
    escaped
        .split("\n\n")
        .map(|p| format!("<p>{}</p>", p.replace('\n', "<br>")))
        .collect::<Vec<_>>()
        .join("")
}

fn random_client_message_id() -> Result<String, SendError> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(|e| SendError(format!("Could not generate a client message id: {e}")))?;
    let r = u64::from_le_bytes(bytes) % 9_000_000_000_000_000_000u64;
    Ok((1_000_000_000_000_000_000u64 + r).to_string())
}

pub fn build_payload(
    text: &str,
    display_name: &str,
    html_mode: bool,
    client_message_id: Option<&str>,
) -> Result<Value, SendError> {
    if text.trim().is_empty() {
        return Err(SendError("Message text cannot be empty.".to_string()));
    }
    let cid = match client_message_id {
        Some(c) => {
            if !CLIENT_MSG_ID_RE.is_match(c) {
                return Err(SendError("client_message_id must be 1 to 32 ASCII digits.".to_string()));
            }
            c.to_string()
        }
        None => random_client_message_id()?,
    };
    let content = if html_mode {
        text.to_string()
    } else {
        format_plain_html(text)
    };
    let composetime = Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    Ok(json!({
        "content": content,
        "messagetype": "RichText/Html",
        "contenttype": "text",
        "clientmessageid": cid,
        "imdisplayname": display_name,
        "composetime": composetime,
        "properties": {"formatVariant": "TEAMS"},
    }))
}

fn me_display_name(conn: Option<&Connection>) -> String {
    let Some(c) = conn else { return String::new() };
    let Some(me) = archive::me_mri(c) else {
        return String::new();
    };
    c.query_row("SELECT name FROM people WHERE mri = ?", [me], |r| {
        r.get::<_, Option<String>>(0)
    })
    .optional()
    .ok()
    .flatten()
    .flatten()
    .map(|s| s.trim().to_string())
    .unwrap_or_default()
}

fn percent_encode_conv(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || matches!(b, b':' | b'@' | b'_' | b'-' | b'.' | b'~');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub const CHATSVC_BASE: &str = "https://teams.microsoft.com";

pub fn send_message(
    conn: Option<&Connection>,
    target: &str,
    text: &str,
    reply_to: Option<&str>,
    html_mode: bool,
    client_message_id: Option<&str>,
    dry_run: bool,
) -> Result<Value, SendError> {
    send_message_via(
        CHATSVC_BASE,
        conn,
        target,
        text,
        reply_to,
        html_mode,
        client_message_id,
        dry_run,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn send_message_via(
    chatsvc_base: &str,
    conn: Option<&Connection>,
    target: &str,
    text: &str,
    reply_to: Option<&str>,
    html_mode: bool,
    client_message_id: Option<&str>,
    dry_run: bool,
) -> Result<Value, SendError> {
    let base = chatsvc_base.trim_end_matches('/');
    let base_ok = url::Url::parse(base).is_ok_and(|u| match u.scheme() {
        "https" => auth::is_trusted_host(u.host_str()),
        "http" => matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    });
    if !base_ok {
        return Err(SendError(format!(
            "Refusing to send to untrusted chat service {}.",
            Quoted(base)
        )));
    }
    let resolved = resolve_target(conn, target)?;
    if let Some(rt) = reply_to {
        if !REPLY_TO_RE.is_match(rt) {
            return Err(SendError("reply_to must be a numeric message id.".to_string()));
        }
    }
    let display_name = me_display_name(conn);
    let mut payload = build_payload(text, &display_name, html_mode, client_message_id)?;
    let enabled = is_send_enabled();
    let allowed = is_conversation_allowed(&resolved.id);

    if dry_run {
        return Ok(json!({
            "dryRun": true,
            "sent": false,
            "restricted": !enabled,
            "allowed": allowed,
            "conversationId": resolved.id,
            "chatName": resolved.name,
            "chatKind": resolved.kind,
            "replyTo": reply_to,
            "text": text,
            "content": payload["content"],
            "clientMessageId": payload["clientmessageid"],
        }));
    }

    if !enabled {
        return Err(SendError(format!(
            "Sending messages is currently restricted. Set {ENABLE_ENV}=1 to enable sending (or use --dry-run to preview)."
        )));
    }
    if !allowed {
        return Err(SendError(format!(
            "Conversation {} is not in {ALLOWLIST_ENV}.",
            Quoted(&resolved.id)
        )));
    }

    let tok = auth::load_skypetoken(None, None).map_err(|e| SendError(e.to_string()))?;
    if payload["imdisplayname"].as_str().unwrap_or("").is_empty() && !tok.display_name.is_empty() {
        payload["imdisplayname"] = Value::String(tok.display_name.clone());
    }

    let mut conv_seg = percent_encode_conv(&resolved.id);
    if let Some(rt) = reply_to {
        conv_seg.push_str(&format!(";messageid={rt}"));
    }
    let url = format!(
        "{base}/api/chatsvc/{}/v1/users/ME/conversations/{conv_seg}/messages",
        tok.region
    );

    let agent = crate::live::http::agent(Duration::from_secs(30));
    let body_str = json_codec::to_string(&payload);
    let resp = agent
        .post(&url)
        .set("Authentication", &format!("skypetoken={}", tok.token))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .set("User-Agent", auth::USER_AGENT)
        .send_string(&body_str);

    let (status, raw_body) = match resp {
        Ok(r) => {
            let st = r.status();
            let s = r.into_string().unwrap_or_default();
            (st, s)
        }
        Err(ureq::Error::Status(code, _)) => {
            if matches!(code, 301 | 302 | 303 | 307 | 308) {
                return Err(SendError(
                    "Unexpected redirect from Teams chat service; refusing to forward credentials.".to_string(),
                ));
            }
            if matches!(code, 401 | 403) {
                return Err(SendError(format!(
                    "Authentication failed (HTTP {code}). Open Microsoft Teams to refresh your session, then try again."
                )));
            }
            return Err(SendError(format!("Failed to send message (HTTP {code}).")));
        }
        Err(ureq::Error::Transport(t)) => {
            let reason = t.to_string().replace(&tok.token, "[redacted]");
            return Err(SendError(format!("Failed to send message: {reason}")));
        }
    };

    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(SendError(
            "Unexpected redirect from Teams chat service; refusing to forward credentials.".to_string(),
        ));
    }
    let msg_id: Option<String> = serde_json::from_str::<Value>(&raw_body).ok().and_then(|v| {
        let oat = v.get("OriginalArrivalTime").or_else(|| v.get("originalArrivalTime"))?;
        if oat.is_boolean() || oat.is_null() {
            None
        } else if let Some(s) = oat.as_str() {
            Some(s.to_string())
        } else {
            Some(oat.to_string())
        }
    });

    Ok(json!({
        "sent": true,
        "status": status,
        "messageId": msg_id,
        "conversationId": resolved.id,
        "chatName": resolved.name,
        "chatKind": resolved.kind,
        "replyTo": reply_to,
        "clientMessageId": payload["clientmessageid"],
        "text": text,
        "region": tok.region,
    }))
}
