use crate::formatters::Quoted;
use crate::storage::{archive, db};
use regex::Regex;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde_json::Value;
use std::sync::LazyLock;

static DIRECT_CONV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^19:[^/\s]+@[a-zA-Z0-9._-]+$").unwrap());
static DIRECT_MRI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^8:[a-z0-9]+:[^\s]+$").unwrap());
static NUMERIC_ID_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    Loose,
    Strict,
}

#[derive(Debug, Clone)]
pub enum ResolveError {
    BadInput(String),
    NotFound(String),
    Ambiguous(String),
    ScopeRequired(String),
    Db(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::BadInput(m)
            | ResolveError::NotFound(m)
            | ResolveError::Ambiguous(m)
            | ResolveError::ScopeRequired(m)
            | ResolveError::Db(m) => write!(f, "{m}"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResolveScope<'a> {
    pub allow_archive_search: bool,
    pub allow_chats: Option<&'a [String]>,
}

impl ResolveScope<'_> {
    pub fn unrestricted() -> Self {
        Self {
            allow_archive_search: true,
            allow_chats: None,
        }
    }

    pub fn allows_chat(&self, conv_id: &str) -> bool {
        self.allow_chats.is_none_or(|list| list.iter().any(|c| c == conv_id))
    }
}

impl std::error::Error for ResolveError {}

#[derive(Debug, Clone)]
pub struct ResolvedChat {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub team_id: Option<String>,
    pub members: Vec<String>,
    pub sharepoint_folder: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPerson {
    pub mri: String,
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedMessage {
    pub conversation_id: String,
    pub chat_name: String,
    pub chat_kind: String,
    pub message_id: String,
    pub thread_root: Option<String>,
    pub sender_mri: String,
    pub sender_name: String,
    pub arrived_at: Option<i64>,
    pub text: String,
    pub content_html: String,
    pub message_type: String,
    pub archived: bool,
}

fn parse_members_json(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

pub fn channel_sharepoint_folder(conn: &Connection, conv_id: &str) -> Option<String> {
    let val: Value = conn
        .query_row(
            "SELECT data FROM record_versions WHERE source = 'conversation' AND record_key = ? ORDER BY id DESC LIMIT 1",
            [conv_id],
            |r| Ok(archive::decode_record_ref(r.get_ref(0)?).ok()),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()?;
    let tp = val.get("threadProperties")?.as_object()?;
    for key in [
        "channelDocsFolderRelativeUrl",
        "filesRelativePath",
        "sharepointFolderRelativeUrl",
    ] {
        if let Some(s) = tp.get(key).and_then(|v| v.as_str()).map(str::trim) {
            if !s.is_empty() && !s.eq_ignore_ascii_case("none") && s.starts_with('/') {
                return Some(s.to_string());
            }
        }
    }
    if let Some(ff) = tp.get("filesFolder") {
        let parsed = match ff {
            Value::Object(_) => Some(ff.clone()),
            Value::String(s) => serde_json::from_str::<Value>(s).ok(),
            _ => None,
        };
        if let Some(rel) = parsed
            .as_ref()
            .and_then(|v| v.get("relativeUrl").or_else(|| v.get("serverRelativeUrl")))
            .and_then(Value::as_str)
            .map(str::trim)
        {
            if !rel.is_empty() && rel.starts_with('/') {
                return Some(rel.to_string());
            }
        }
    }
    None
}

pub fn resolve_chat(
    conn: Option<&Connection>,
    target: &str,
    strictness: Strictness,
) -> Result<(ResolvedChat, Vec<ResolvedChat>), ResolveError> {
    resolve_chat_scoped(conn, target, strictness, ResolveScope::unrestricted())
}

fn synthetic_chat(id: &str) -> ResolvedChat {
    ResolvedChat {
        id: id.to_string(),
        name: if id == "48:notes" {
            "Self-chat (notes)".to_string()
        } else {
            id.to_string()
        },
        kind: if id.starts_with("48:") { "system" } else { "chat" }.to_string(),
        team_id: None,
        members: Vec::new(),
        sharepoint_folder: None,
    }
}

pub fn resolve_chat_scoped(
    conn: Option<&Connection>,
    target: &str,
    strictness: Strictness,
    scope: ResolveScope<'_>,
) -> Result<(ResolvedChat, Vec<ResolvedChat>), ResolveError> {
    let q = target.trim();
    if q.is_empty() {
        return Err(ResolveError::BadInput(
            "Target chat name or conversation id is required.".to_string(),
        ));
    }
    let is_direct = q == "48:notes" || DIRECT_CONV_RE.is_match(q);
    if is_direct && !scope.allows_chat(q) {
        return Ok((synthetic_chat(q), Vec::new()));
    }

    if let Some(c) = conn {
        if scope.allows_chat(q) {
            if let Ok(Some((id, name, kind, team_id, members_raw))) = c
                .query_row(
                    "SELECT id, name, kind, team_id, members FROM conversations WHERE id = ?",
                    [q],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()
            {
                let sp = if kind == "channel" {
                    channel_sharepoint_folder(c, &id)
                } else {
                    None
                };
                let resolved = if scope.allow_archive_search {
                    ResolvedChat {
                        name: name.filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
                        id,
                        kind,
                        team_id,
                        members: parse_members_json(members_raw),
                        sharepoint_folder: sp,
                    }
                } else {
                    ResolvedChat {
                        name: if id == "48:notes" {
                            "Self-chat (notes)".to_string()
                        } else {
                            id.clone()
                        },
                        id,
                        kind,
                        team_id: None,
                        members: Vec::new(),
                        sharepoint_folder: sp,
                    }
                };
                return Ok((resolved, Vec::new()));
            }
        }
    }

    if is_direct {
        return Ok((synthetic_chat(q), Vec::new()));
    }

    if !scope.allow_archive_search {
        return Err(ResolveError::ScopeRequired(
            "Resolving chats by name requires the 'read' scope. Pass an exact conversation id or add `--scope read`."
                .to_string(),
        ));
    }

    let Some(c) = conn else {
        return Err(ResolveError::BadInput(format!(
            "Cannot resolve chat name {} without an archive connection. Pass an exact conversation id.",
            Quoted(target)
        )));
    };

    let pat = db::like_pattern(q);
    let sql = format!(
        "SELECT c.id, c.name, c.kind, c.team_id, c.members, {} AS message_count, {} AS activity \
         FROM conversations c \
         WHERE (c.kind != 'system' AND c.name LIKE ? ESCAPE '\\') \
            OR (c.id = '48:notes' AND (c.name LIKE ? ESCAPE '\\' OR lower(?) = '48:notes')) \
         ORDER BY activity DESC, message_count DESC, c.id ASC",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    let mut stmt = c.prepare(&sql).map_err(|e| ResolveError::Db(e.to_string()))?;
    type ChatCandidateRow = (String, Option<String>, String, Option<String>, Option<String>, i64, i64);
    let mut rows: Vec<ChatCandidateRow> = stmt
        .query_map(params![pat, pat, q], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .map_err(|e| ResolveError::Db(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ResolveError::Db(e.to_string()))?;

    rows.retain(|r| scope.allows_chat(&r.0));

    if rows.is_empty() {
        return Err(ResolveError::NotFound(format!(
            "No chat matches {}. Run `teams chat list` to find its name or id.",
            Quoted(target)
        )));
    }

    let ql = q.to_ascii_lowercase();
    rows.sort_by(|a, b| {
        let a_exact = a.1.as_deref().unwrap_or("").to_ascii_lowercase() == ql;
        let b_exact = b.1.as_deref().unwrap_or("").to_ascii_lowercase() == ql;
        b_exact
            .cmp(&a_exact)
            .then_with(|| (b.5 > 0).cmp(&(a.5 > 0)))
            .then_with(|| b.6.cmp(&a.6))
    });

    if strictness == Strictness::Strict && rows.len() > 1 {
        let exact_matches: Vec<_> = rows
            .iter()
            .filter(|r| r.1.as_deref().unwrap_or("").trim().eq_ignore_ascii_case(q))
            .collect();
        if exact_matches.len() == 1 {
            let m = exact_matches[0];
            let sp = if m.2 == "channel" {
                channel_sharepoint_folder(c, &m.0)
            } else {
                None
            };
            return Ok((
                ResolvedChat {
                    name: m.1.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| m.0.clone()),
                    id: m.0.clone(),
                    kind: m.2.clone(),
                    team_id: m.3.clone(),
                    members: parse_members_json(m.4.clone()),
                    sharepoint_folder: sp,
                },
                Vec::new(),
            ));
        }
        let mut lines = vec![format!(
            "Ambiguous target {} matches {} chats; refusing to guess. Pass the exact conversation id:",
            Quoted(target),
            rows.len()
        )];
        for (id, name, kind, _, _, mc, _) in rows.iter().take(10) {
            let n = name.as_deref().unwrap_or(id);
            lines.push(format!(
                "  - {id}  (kind={kind}, messageCount={mc}, name={})",
                Quoted(n)
            ));
        }
        return Err(ResolveError::Ambiguous(lines.join("\n")));
    }

    let mut converted: Vec<ResolvedChat> = rows
        .into_iter()
        .take(9)
        .map(|(id, name, kind, team_id, members, _, _)| {
            let sp = if kind == "channel" {
                channel_sharepoint_folder(c, &id)
            } else {
                None
            };
            ResolvedChat {
                name: name.filter(|s| !s.is_empty()).unwrap_or_else(|| id.clone()),
                id,
                kind,
                team_id,
                members: parse_members_json(members),
                sharepoint_folder: sp,
            }
        })
        .collect();
    let best = converted.remove(0);
    Ok((best, converted))
}

pub fn resolve_person(
    conn: Option<&Connection>,
    query: &str,
    strictness: Strictness,
) -> Result<(ResolvedPerson, Vec<ResolvedPerson>), ResolveError> {
    resolve_person_scoped(conn, query, strictness, ResolveScope::unrestricted())
}

pub fn resolve_person_scoped(
    conn: Option<&Connection>,
    query: &str,
    strictness: Strictness,
    scope: ResolveScope<'_>,
) -> Result<(ResolvedPerson, Vec<ResolvedPerson>), ResolveError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(ResolveError::BadInput(
            "Person name, email, or MRI is required.".to_string(),
        ));
    }
    let is_direct_mri = DIRECT_MRI_RE.is_match(q);
    if !scope.allow_archive_search {
        if is_direct_mri {
            return Ok((
                ResolvedPerson {
                    mri: q.to_string(),
                    name: q.to_string(),
                    email: String::new(),
                },
                Vec::new(),
            ));
        }
        return Err(ResolveError::ScopeRequired(
            "Resolving people by name or email requires the 'read' scope. Pass an exact MRI (8:orgid:...) or add `--scope read`."
                .to_string(),
        ));
    }
    if let Some(c) = conn {
        if let Ok(Some((mri, name, email))) = c
            .query_row(
                "SELECT mri, name, email FROM people WHERE mri = ? OR lower(email) = lower(?) LIMIT 1",
                params![q, q],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    ))
                },
            )
            .optional()
        {
            return Ok((
                ResolvedPerson {
                    name: if name.trim().is_empty() {
                        mri.clone()
                    } else {
                        name.trim().to_string()
                    },
                    mri,
                    email: email.trim().to_string(),
                },
                Vec::new(),
            ));
        }
    }

    if is_direct_mri {
        return Ok((
            ResolvedPerson {
                mri: q.to_string(),
                name: q.to_string(),
                email: String::new(),
            },
            Vec::new(),
        ));
    }

    let Some(c) = conn else {
        return Err(ResolveError::BadInput(format!(
            "Cannot resolve person {} without an archive connection. Pass an exact MRI (8:orgid:...).",
            Quoted(query)
        )));
    };

    let pat = db::like_pattern(q);
    let mut stmt = c
        .prepare(
            "SELECT mri, name, email FROM people \
             WHERE name LIKE ? ESCAPE '\\' OR email LIKE ? ESCAPE '\\' \
             ORDER BY (lower(name) = lower(?)) DESC, (email != '') DESC, name ASC",
        )
        .map_err(|e| ResolveError::Db(e.to_string()))?;
    let rows: Vec<ResolvedPerson> = stmt
        .query_map(params![pat, pat, q], |r| {
            let mri: String = r.get(0)?;
            let name: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let email: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
            Ok(ResolvedPerson {
                name: if name.trim().is_empty() {
                    mri.clone()
                } else {
                    name.trim().to_string()
                },
                mri,
                email: email.trim().to_string(),
            })
        })
        .map_err(|e| ResolveError::Db(e.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ResolveError::Db(e.to_string()))?;

    if rows.is_empty() {
        return Err(ResolveError::NotFound(format!(
            "No person matches {}. Run `teams people` to inspect known contacts.",
            Quoted(query)
        )));
    }

    if strictness == Strictness::Strict && rows.len() > 1 {
        let exact: Vec<&ResolvedPerson> = rows
            .iter()
            .filter(|p| p.name.eq_ignore_ascii_case(q) || (!p.email.is_empty() && p.email.eq_ignore_ascii_case(q)))
            .collect();
        if exact.len() == 1 {
            return Ok((exact[0].clone(), Vec::new()));
        }
        let mut lines = vec![format!(
            "Ambiguous person {} matches {} people; pass email or exact MRI:",
            Quoted(query),
            rows.len()
        )];
        for p in rows.iter().take(10) {
            if p.email.is_empty() {
                lines.push(format!("  - {}  ({})", p.mri, Quoted(&p.name)));
            } else {
                lines.push(format!("  - {}  ({}, {})", p.mri, Quoted(&p.name), p.email));
            }
        }
        return Err(ResolveError::Ambiguous(lines.join("\n")));
    }

    let mut list = rows;
    let best = list.remove(0);
    list.truncate(8);
    Ok((best, list))
}

pub fn parse_message_spec(chat_hint: Option<&str>, spec: &str) -> Result<(Option<String>, String), ResolveError> {
    let s = spec.trim();
    if s.is_empty() {
        return Err(ResolveError::BadInput("Message id is required.".to_string()));
    }
    if let Some((cid, mid)) = s.rsplit_once('/') {
        let cid = cid.trim();
        let mid = mid.trim();
        if !cid.is_empty() && NUMERIC_ID_RE.is_match(mid) {
            return Ok((Some(cid.to_string()), mid.to_string()));
        }
    }
    if !NUMERIC_ID_RE.is_match(s) {
        return Err(ResolveError::BadInput(format!(
            "Invalid message id {} (expected numeric id or chatId/msgId).",
            Quoted(spec)
        )));
    }
    Ok((
        chat_hint.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()),
        s.to_string(),
    ))
}

pub fn resolve_message(
    conn: Option<&Connection>,
    chat_hint: Option<&str>,
    msg_spec: &str,
) -> Result<ResolvedMessage, ResolveError> {
    resolve_message_scoped(conn, chat_hint, msg_spec, ResolveScope::unrestricted())
}

pub fn resolve_message_scoped(
    conn: Option<&Connection>,
    chat_hint: Option<&str>,
    msg_spec: &str,
    scope: ResolveScope<'_>,
) -> Result<ResolvedMessage, ResolveError> {
    let (parsed_chat, msg_id) = parse_message_spec(chat_hint, msg_spec)?;

    if parsed_chat.is_none() && !scope.allow_archive_search {
        return Err(ResolveError::ScopeRequired(
            "Looking up a message without a conversation id requires the 'read' scope. Pass chatId/msgId or add `--scope read`."
                .to_string(),
        ));
    }

    let resolved_chat = match parsed_chat {
        Some(ref c) => Some(resolve_chat_scoped(conn, c, Strictness::Strict, scope)?.0),
        None => None,
    };

    if let Some(ref ch) = resolved_chat {
        if !scope.allows_chat(&ch.id) {
            return Ok(ResolvedMessage {
                conversation_id: ch.id.clone(),
                chat_name: ch.name.clone(),
                chat_kind: ch.kind.clone(),
                message_id: msg_id,
                thread_root: None,
                sender_mri: String::new(),
                sender_name: String::new(),
                arrived_at: None,
                text: String::new(),
                content_html: String::new(),
                message_type: "RichText/Html".to_string(),
                archived: false,
            });
        }
    }

    if let Some(c) = conn {
        let mut rows: Vec<ResolvedMessage> = if let Some(ref ch) = resolved_chat {
            let mut stmt = c
                .prepare(
                    "SELECT m.conversation_id, COALESCE(c.name, m.conversation_id), COALESCE(c.kind, 'chat'), \
                            m.message_id, m.thread_id, COALESCE(m.sender_mri, ''), COALESCE(m.sender_name, ''), \
                            m.arrived_at, COALESCE(m.text, ''), COALESCE(m.content_html, ''), COALESCE(m.message_type, 'RichText/Html') \
                     FROM messages m LEFT JOIN conversations c ON c.id = m.conversation_id \
                     WHERE m.conversation_id = ? AND m.message_id = ? LIMIT 1",
                )
                .map_err(|e| ResolveError::Db(e.to_string()))?;
            let found = stmt
                .query_map(params![ch.id, msg_id], row_to_resolved_message)
                .map_err(|e| ResolveError::Db(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ResolveError::Db(e.to_string()))?;
            found
        } else if let Some(allowed) = scope.allow_chats {
            if allowed.is_empty() {
                Vec::new()
            } else {
                let placeholders = vec!["?"; allowed.len()].join(", ");
                let sql = format!(
                    "SELECT m.conversation_id, COALESCE(c.name, m.conversation_id), COALESCE(c.kind, 'chat'), \
                            m.message_id, m.thread_id, COALESCE(m.sender_mri, ''), COALESCE(m.sender_name, ''), \
                            m.arrived_at, COALESCE(m.text, ''), COALESCE(m.content_html, ''), COALESCE(m.message_type, 'RichText/Html') \
                     FROM messages m LEFT JOIN conversations c ON c.id = m.conversation_id \
                     WHERE m.message_id = ? AND m.conversation_id IN ({placeholders}) LIMIT 5"
                );
                let mut bind: Vec<&str> = Vec::with_capacity(1 + allowed.len());
                bind.push(msg_id.as_str());
                for cid in allowed {
                    bind.push(cid.as_str());
                }
                let mut stmt = c.prepare(&sql).map_err(|e| ResolveError::Db(e.to_string()))?;
                let found = stmt
                    .query_map(params_from_iter(bind), row_to_resolved_message)
                    .map_err(|e| ResolveError::Db(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| ResolveError::Db(e.to_string()))?;
                found
            }
        } else {
            let mut stmt = c
                .prepare(
                    "SELECT m.conversation_id, COALESCE(c.name, m.conversation_id), COALESCE(c.kind, 'chat'), \
                            m.message_id, m.thread_id, COALESCE(m.sender_mri, ''), COALESCE(m.sender_name, ''), \
                            m.arrived_at, COALESCE(m.text, ''), COALESCE(m.content_html, ''), COALESCE(m.message_type, 'RichText/Html') \
                     FROM messages m LEFT JOIN conversations c ON c.id = m.conversation_id \
                     WHERE m.message_id = ? LIMIT 5",
                )
                .map_err(|e| ResolveError::Db(e.to_string()))?;
            let found = stmt
                .query_map(params![msg_id], row_to_resolved_message)
                .map_err(|e| ResolveError::Db(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| ResolveError::Db(e.to_string()))?;
            found
        };

        if rows.len() == 1 {
            let mut row = rows.remove(0);
            if !scope.allow_archive_search {
                if let Some(ref ch) = resolved_chat {
                    row.chat_name = ch.name.clone();
                } else {
                    row.chat_name = row.conversation_id.clone();
                }
            }
            return Ok(row);
        }
        if rows.len() > 1 {
            let mut lines = vec![format!(
                "Message id {} exists in {} chats; pass chatId/msgId:",
                Quoted(&msg_id),
                rows.len()
            )];
            for r in &rows {
                lines.push(format!(
                    "  - {}/{}  ({})",
                    r.conversation_id,
                    r.message_id,
                    Quoted(&r.chat_name)
                ));
            }
            return Err(ResolveError::Ambiguous(lines.join("\n")));
        }
    }

    if let Some(ch) = resolved_chat {
        return Ok(ResolvedMessage {
            conversation_id: ch.id,
            chat_name: ch.name,
            chat_kind: ch.kind,
            message_id: msg_id,
            thread_root: None,
            sender_mri: String::new(),
            sender_name: String::new(),
            arrived_at: None,
            text: String::new(),
            content_html: String::new(),
            message_type: "RichText/Html".to_string(),
            archived: false,
        });
    }

    Err(ResolveError::NotFound(format!(
        "Message {} not found in archive; pass chatId/msgId or specify the chat.",
        Quoted(msg_spec)
    )))
}

fn row_to_resolved_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<ResolvedMessage> {
    let conv_id: String = r.get(0)?;
    let chat_name: String = r.get(1)?;
    let chat_kind: String = r.get(2)?;
    let message_id: String = r.get(3)?;
    let thread_id: Option<String> = r.get(4)?;
    let thread_root = thread_id.filter(|t| !t.is_empty() && t != &message_id && chat_kind == "channel");
    Ok(ResolvedMessage {
        conversation_id: conv_id,
        chat_name,
        chat_kind,
        message_id,
        thread_root,
        sender_mri: r.get(5)?,
        sender_name: r.get(6)?,
        arrived_at: r.get(7)?,
        text: r.get(8)?,
        content_html: r.get(9)?,
        message_type: r.get(10)?,
        archived: true,
    })
}

pub fn me_mri(conn: Option<&Connection>) -> Option<String> {
    conn.and_then(archive::me_mri)
}

pub fn me_display_name(conn: Option<&Connection>) -> String {
    let Some(c) = conn else {
        return String::new();
    };
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
