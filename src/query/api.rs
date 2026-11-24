use crate::formatters::Quoted;
use crate::identity;
use crate::live::onedrive;
use crate::paths;
use crate::storage::{archive, backups, db};
use crate::timeutil::{iso, parse_when};
use regex::Regex;
use rusqlite::{params_from_iter, types::Value as SqlVal, Connection, OptionalExtension, Row};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::UNIX_EPOCH;

pub const ATTACHMENT_KINDS: &[&str] = &["file", "image", "link"];
pub const CHAT_KINDS: &[&str] = &["chat", "channel", "meeting", "team", "community", "system"];
pub const SEARCH_SORTS: &[&str] = &["recent", "relevance"];

static FTS_TOK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"-?"[^"]*"|\S+"#).unwrap());
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\w").unwrap());

#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub status: u16,
    pub details: Option<Box<Value>>,
}

impl ApiError {
    pub fn bad(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            status: 400,
            details: None,
        }
    }

    pub fn with_status(code: &str, message: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            status,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }

    pub fn to_json(&self) -> Value {
        let mut inner = Map::new();
        inner.insert("code".to_string(), Value::String(self.code.clone()));
        inner.insert("message".to_string(), Value::String(self.message.clone()));
        if let Some(ref d) = self.details {
            inner.insert("details".to_string(), (**d).clone());
        }
        json!({"error": inner})
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::with_status("internal", e.to_string(), 500)
    }
}

pub fn is_fts_syntax_error(e: &rusqlite::Error) -> bool {
    let msg = e.to_string().to_ascii_lowercase();
    msg.contains("fts5:")
        || msg.contains("no such column")
        || msg.contains("malformed match")
        || msg.contains("unterminated string")
}

fn search_err(e: rusqlite::Error) -> ApiError {
    if is_fts_syntax_error(&e) {
        ApiError::bad("bad_query", format!("Search query not understood: {e}"))
    } else {
        ApiError::from(e)
    }
}

pub fn parse_when_or_error(value: Option<&str>, end: bool) -> Result<Option<i64>, ApiError> {
    if let Some(v) = value {
        if v.trim().eq_ignore_ascii_case("all") {
            return Ok(None);
        }
    }
    parse_when(value, end).map_err(|e| ApiError::bad("bad_time", e))
}

#[derive(Debug, Clone)]
pub struct ChatRow {
    pub id: String,
    pub name: Option<String>,
    pub kind: String,
    pub topic: Option<String>,
    pub team_id: Option<String>,
    pub message_count: i64,
    pub activity: i64,
}

impl ChatRow {
    pub fn from_row(r: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get("id")?,
            name: r.get("name")?,
            kind: r.get("kind")?,
            topic: r.get("topic")?,
            team_id: r.get("team_id")?,
            message_count: r.get("message_count")?,
            activity: r.get::<_, Option<i64>>("activity")?.unwrap_or(0),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "topic": self.topic.as_deref().filter(|s| !s.is_empty()),
            "teamId": self.team_id,
            "messageCount": self.message_count,
            "lastActivity": iso(if self.activity != 0 { Some(self.activity) } else { None }),
            "lastActivityMs": if self.activity != 0 { Some(self.activity) } else { None },
        })
    }
}

#[derive(Debug, Clone)]
struct MsgRow {
    pk: i64,
    conversation_id: String,
    message_id: String,
    thread_id: Option<String>,
    arrived_at: Option<i64>,
    edited_at: Option<i64>,
    deleted_at: Option<i64>,
    sender_mri: Option<String>,
    sender: Option<String>,
    kind: String,
    text: Option<String>,
    subject: Option<String>,
    importance: Option<String>,
    reactions: Option<String>,
    content_hash: Option<String>,
    deleted_by: Option<String>,
    chat_name: Option<String>,
    chat_kind: Option<String>,
}

impl MsgRow {
    fn from_row(r: &Row<'_>, with_chat: bool) -> rusqlite::Result<Self> {
        Ok(Self {
            pk: r.get("pk")?,
            conversation_id: r.get("conversation_id")?,
            message_id: r.get("message_id")?,
            thread_id: r.get("thread_id")?,
            arrived_at: r.get("arrived_at")?,
            edited_at: r.get("edited_at")?,
            deleted_at: r.get("deleted_at")?,
            sender_mri: r.get("sender_mri")?,
            sender: r.get("sender")?,
            kind: r.get("kind")?,
            text: r.get("text")?,
            subject: r.get("subject")?,
            importance: r.get("importance")?,
            reactions: r.get("reactions")?,
            content_hash: r.get("content_hash")?,
            deleted_by: r.get("deleted_by")?,
            chat_name: if with_chat { r.get("chat_name")? } else { None },
            chat_kind: if with_chat { r.get("chat_kind")? } else { None },
        })
    }
}

fn fetch_attachments_by_msg(
    conn: &Connection,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<Value>>, ApiError> {
    let mut out: HashMap<(String, String), Vec<Value>> = HashMap::new();
    for chunk in keys.chunks(300) {
        let vals = vec!["(?, ?)"; chunk.len()].join(",");
        let sql = format!(
            "SELECT conversation_id, message_id, kind, name, url FROM attachments \
             WHERE (conversation_id, message_id) IN (VALUES {vals}) ORDER BY id"
        );
        let mut params: Vec<SqlVal> = Vec::with_capacity(chunk.len() * 2);
        for (c, m) in chunk {
            params.push(SqlVal::Text(c.clone()));
            params.push(SqlVal::Text(m.clone()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                json!({
                    "kind": r.get::<_, String>(2)?,
                    "name": r.get::<_, Option<String>>(3)?,
                    "url": r.get::<_, String>(4)?,
                }),
            ))
        })?;
        for row in rows {
            let (c, m, v) = row?;
            out.entry((c, m)).or_default().push(v);
        }
    }
    Ok(out)
}

type RevisionRow = (String, String, Option<i64>, i64);

fn fetch_revisions_by_msg(
    conn: &Connection,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), Vec<RevisionRow>>, ApiError> {
    let mut out: HashMap<(String, String), Vec<RevisionRow>> = HashMap::new();
    for chunk in keys.chunks(300) {
        let vals = vec!["(?, ?)"; chunk.len()].join(",");
        let sql = format!(
            "SELECT conversation_id, message_id, content_hash, COALESCE(text, ''), edited_at, captured_at \
             FROM message_revisions WHERE (conversation_id, message_id) IN (VALUES {vals}) ORDER BY id"
        );
        let mut params: Vec<SqlVal> = Vec::with_capacity(chunk.len() * 2);
        for (c, m) in chunk {
            params.push(SqlVal::Text(c.clone()));
            params.push(SqlVal::Text(m.clone()));
        }
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        for row in rows {
            let (c, m, h, t, ed, cap) = row?;
            out.entry((c, m)).or_default().push((h, t, ed, cap));
        }
    }
    Ok(out)
}

fn history_json(r: &MsgRow, revs: &HashMap<(String, String), Vec<RevisionRow>>) -> Value {
    let key = (r.conversation_id.clone(), r.message_id.clone());
    let hist: Vec<Value> = revs
        .get(&key)
        .map(|v| {
            v.iter()
                .filter(|(h, _, _, _)| r.content_hash.as_deref() != Some(h.as_str()))
                .map(|(_, t, ed, cap)| {
                    json!({
                        "text": t,
                        "editedAt": iso(*ed),
                        "capturedAt": iso(Some(*cap)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Value::Array(hist)
}

fn messages_json(
    conn: &Connection,
    rows: &[MsgRow],
    extra: Option<&HashMap<i64, Map<String, Value>>>,
    history: bool,
) -> Result<Vec<Value>, ApiError> {
    let mut seen = HashSet::new();
    let mut keys = Vec::new();
    for r in rows {
        let k = (r.conversation_id.clone(), r.message_id.clone());
        if seen.insert(k.clone()) {
            keys.push(k);
        }
    }
    let atts = fetch_attachments_by_msg(conn, &keys)?;
    let revs = if history {
        fetch_revisions_by_msg(conn, &keys)?
    } else {
        HashMap::new()
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let is_reply = r
            .thread_id
            .as_deref()
            .map(|t| !t.is_empty() && t != r.message_id)
            .unwrap_or(false);
        let reactions: Vec<Value> = r
            .reactions
            .as_deref()
            .and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|x| {
                json!({
                    "emoji": x.get("emoji"),
                    "count": x.get("count"),
                    "users": x.get("users").cloned().unwrap_or_else(|| json!([])),
                })
            })
            .collect();
        let key = (r.conversation_id.clone(), r.message_id.clone());
        let mut obj = Map::new();
        obj.insert("id".into(), json!(r.message_id));
        obj.insert("chatId".into(), json!(r.conversation_id));
        obj.insert("threadId".into(), json!(r.thread_id));
        obj.insert("isReply".into(), json!(is_reply));
        obj.insert("time".into(), json!(iso(r.arrived_at)));
        obj.insert("timeMs".into(), json!(r.arrived_at));
        obj.insert("sender".into(), json!(r.sender));
        obj.insert("senderId".into(), json!(r.sender_mri));
        obj.insert("kind".into(), json!(r.kind));
        obj.insert("text".into(), json!(r.text.as_deref().unwrap_or("")));
        obj.insert("subject".into(), json!(r.subject.as_deref().filter(|s| !s.is_empty())));
        obj.insert(
            "importance".into(),
            json!(r.importance.as_deref().filter(|s| !s.is_empty())),
        );
        obj.insert("edited".into(), json!(r.edited_at.map(|v| v != 0).unwrap_or(false)));
        obj.insert("deleted".into(), json!(r.deleted_at.map(|v| v != 0).unwrap_or(false)));
        obj.insert("reactions".into(), Value::Array(reactions));
        obj.insert(
            "attachments".into(),
            Value::Array(atts.get(&key).cloned().unwrap_or_default()),
        );
        obj.insert("editedAt".into(), json!(iso(r.edited_at)));
        obj.insert("deletedAt".into(), json!(iso(r.deleted_at)));
        obj.insert("deletedBy".into(), json!(r.deleted_by));
        if history {
            obj.insert("history".into(), history_json(r, &revs));
        }
        if let Some(ext_map) = extra.and_then(|e| e.get(&r.pk)) {
            for (k, v) in ext_map {
                obj.insert(k.clone(), v.clone());
            }
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

pub fn list_chats(
    conn: &Connection,
    query: Option<&str>,
    kind: Option<&str>,
    limit: usize,
    include_system: bool,
    member: Option<&str>,
) -> Result<Value, ApiError> {
    if let Some(k) = kind.filter(|s| !s.is_empty()) {
        if !CHAT_KINDS.contains(&k) {
            return Err(ApiError::bad(
                "bad_param",
                format!("kind must be one of {}", CHAT_KINDS.join(", ")),
            ));
        }
    }
    let mut where_parts: Vec<String> = Vec::new();
    let mut args: Vec<SqlVal> = Vec::new();
    if let Some(q) = query.filter(|s| !s.is_empty()) {
        where_parts.push("(c.name LIKE ? ESCAPE '\\' OR c.id = ?)".to_string());
        args.push(SqlVal::Text(db::like_pattern(q)));
        args.push(SqlVal::Text(q.to_string()));
    }
    if let Some(k) = kind.filter(|s| !s.is_empty()) {
        where_parts.push("c.kind = ?".to_string());
        args.push(SqlVal::Text(k.to_string()));
    } else if !include_system {
        where_parts.push("c.kind != 'system'".to_string());
    }
    let mut member_matches_out: Option<Vec<Value>> = None;
    if let Some(m) = member.filter(|s| !s.is_empty()) {
        let matches = member_matches(conn, m)?;
        let mris: HashSet<String> = matches.iter().map(|x| x.0.clone()).collect();
        let mm_json: Vec<Value> = matches
            .iter()
            .map(|(id, name, email)| json!({"id": id, "name": name, "email": email}))
            .collect();
        let ids = conversations_with(conn, &mris)?;
        if ids.is_empty() {
            return Ok(json!({"chats": [], "total": 0, "memberMatches": mm_json}));
        }
        where_parts.push("c.id IN (SELECT value FROM json_each(?))".to_string());
        args.push(SqlVal::Text(db::id_list(&ids)));
        member_matches_out = Some(mm_json);
    }
    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };
    let sql = format!(
        "SELECT c.*, {} AS message_count, {} AS activity FROM conversations c {where_sql} ORDER BY activity DESC",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<ChatRow> = stmt
        .query_map(params_from_iter(args), ChatRow::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let total = rows.len();
    let shown: Vec<Value> = if limit > 0 {
        rows.iter().take(limit).map(ChatRow::to_json).collect()
    } else {
        rows.iter().map(ChatRow::to_json).collect()
    };
    let mut out = Map::new();
    out.insert("chats".into(), Value::Array(shown));
    out.insert("total".into(), json!(total));
    if let Some(mm) = member_matches_out {
        out.insert("memberMatches".into(), Value::Array(mm));
    }
    Ok(Value::Object(out))
}

type MemberMatch = (String, Option<String>, Option<String>);

fn member_matches(conn: &Connection, member: &str) -> Result<Vec<MemberMatch>, ApiError> {
    let pat = db::like_pattern(member);
    let mut found: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT mri, name, email FROM people WHERE name LIKE ? ESCAPE '\\' OR email LIKE ? ESCAPE '\\' OR mri = ?",
        )?;
        for row in stmt.query_map([&pat, &pat, member], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
            ))
        })? {
            let (mri, name, email) = row?;
            if !found.contains_key(&mri) {
                order.push(mri.clone());
            }
            found.insert(mri, (name, email));
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT m.sender_mri, MAX(NULLIF(m.sender_name, '')) FROM messages m \
             WHERE (m.sender_name LIKE ? ESCAPE '\\' OR m.sender_mri = ?) \
               AND m.sender_mri IS NOT NULL AND m.sender_mri != '' GROUP BY m.sender_mri",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([&pat, member], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (mri, sname) in rows {
            if let std::collections::hash_map::Entry::Vacant(slot) = found.entry(mri.clone()) {
                let p: Option<(Option<String>, Option<String>)> = conn
                    .query_row("SELECT name, email FROM people WHERE mri = ?", [&mri], |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?.filter(|s| !s.is_empty()),
                            r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                        ))
                    })
                    .optional()?;
                let (pname, pemail) = p.unwrap_or((None, None));
                order.push(mri);
                slot.insert((pname.or(sname), pemail));
            }
        }
    }
    Ok(order
        .into_iter()
        .map(|id| {
            let (n, e) = found.remove(&id).unwrap_or((None, None));
            (id, n, e)
        })
        .collect())
}

fn conversations_with(conn: &Connection, mris: &HashSet<String>) -> Result<HashSet<String>, ApiError> {
    if mris.is_empty() {
        return Ok(HashSet::new());
    }
    let oids: HashSet<String> = mris.iter().filter_map(|m| identity::valid_oid(m)).collect();
    let mut out = HashSet::new();
    let mri_vec: Vec<String> = mris.iter().cloned().collect();

    if db::table_exists(conn, "conversation_members") {
        for chunk in mri_vec.chunks(400) {
            let qs = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT conversation_id FROM conversation_members WHERE mri IN ({qs})"
            ))?;
            for r in stmt.query_map(params_from_iter(chunk.iter().cloned()), |r| r.get::<_, String>(0))? {
                out.insert(r?);
            }
        }
        let oid_vec: Vec<String> = oids.iter().cloned().collect();
        for chunk in oid_vec.chunks(400) {
            let qs = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT conversation_id FROM conversation_members WHERE oid IN ({qs})"
            ))?;
            for r in stmt.query_map(params_from_iter(chunk.iter().cloned()), |r| r.get::<_, String>(0))? {
                out.insert(r?);
            }
        }
    } else {
        let mut stmt = conn.prepare("SELECT id, members FROM conversations")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (cid, mem_s) = row?;
            let mems: Vec<String> = mem_s
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();
            if mems.iter().any(|m| mris.contains(m)) || identity::one_on_one_oids(&cid).iter().any(|o| oids.contains(o))
            {
                out.insert(cid);
            }
        }
    }
    for chunk in mri_vec.chunks(500) {
        let qs = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT conversation_id FROM messages WHERE sender_mri IN ({qs})"
        ))?;
        for r in stmt.query_map(params_from_iter(chunk.iter().cloned()), |r| r.get::<_, String>(0))? {
            out.insert(r?);
        }
    }
    Ok(out)
}

pub fn resolve_chat(conn: &Connection, chat: &str) -> Result<(ChatRow, Vec<ChatRow>), ApiError> {
    let chat = chat.trim();
    if chat.is_empty() {
        return Err(ApiError::bad("bad_param", "chat is required"));
    }
    let base = format!(
        "SELECT c.*, {} AS message_count, {} AS activity FROM conversations c",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    if let Some(row) = conn
        .query_row(&format!("{base} WHERE c.id = ?"), [chat], ChatRow::from_row)
        .optional()?
    {
        return Ok((row, Vec::new()));
    }
    let pat = db::like_pattern(chat);
    let mut stmt = conn.prepare(&format!(
        "{base} WHERE c.name LIKE ? ESCAPE '\\' AND c.kind != 'system'"
    ))?;
    let mut rows: Vec<ChatRow> = stmt
        .query_map([pat], ChatRow::from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.is_empty() {
        return Err(ApiError::with_status(
            "chat_not_found",
            format!("No chat matches {}. List chats to find its name or id.", Quoted(chat)),
            404,
        ));
    }
    let q = chat.trim().to_ascii_lowercase();
    rows.sort_by(|a, b| {
        let a_exact = a.name.as_deref().unwrap_or("").to_ascii_lowercase() == q;
        let b_exact = b.name.as_deref().unwrap_or("").to_ascii_lowercase() == q;
        b_exact
            .cmp(&a_exact)
            .then_with(|| (b.message_count > 0).cmp(&(a.message_count > 0)))
            .then_with(|| b.activity.cmp(&a.activity))
    });
    let best = rows.remove(0);
    rows.truncate(8);
    Ok((best, rows))
}

const SENDER_COND: &str = "(m.sender_name LIKE ? ESCAPE '\\' OR p.name LIKE ? ESCAPE '\\' \
                           OR p.email LIKE ? ESCAPE '\\' OR m.sender_mri = ?)";

fn apply_filters(
    where_parts: &mut Vec<String>,
    args: &mut Vec<SqlVal>,
    since: Option<&str>,
    until: Option<&str>,
    sender: Option<&str>,
    include_events: bool,
) -> Result<(), ApiError> {
    if let Some(t) = parse_when_or_error(since, false)? {
        where_parts.push("m.arrived_at >= ?".to_string());
        args.push(SqlVal::Integer(t));
    }
    if let Some(t) = parse_when_or_error(until, true)? {
        where_parts.push("m.arrived_at < ?".to_string());
        args.push(SqlVal::Integer(t));
    }
    if let Some(s) = sender.filter(|x| !x.is_empty()) {
        where_parts.push(SENDER_COND.to_string());
        let pat = db::like_pattern(s);
        args.push(SqlVal::Text(pat.clone()));
        args.push(SqlVal::Text(pat.clone()));
        args.push(SqlVal::Text(pat));
        args.push(SqlVal::Text(s.to_string()));
    }
    if !include_events {
        where_parts.push("m.kind = 'message'".to_string());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn read_messages(
    conn: &Connection,
    chat: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    sender: Option<&str>,
    limit: usize,
    include_events: bool,
    thread: Option<&str>,
    history: bool,
    around: Option<&str>,
    context: usize,
) -> Result<Value, ApiError> {
    if let Some(ar) = around.filter(|s| !s.is_empty()) {
        return read_around(conn, chat, ar, context, include_events, thread, history);
    }
    let Some(chat_q) = chat.filter(|s| !s.is_empty()) else {
        return Err(ApiError::bad("bad_param", "chat is required"));
    };
    let (row, others) = resolve_chat(conn, chat_q)?;
    let mut where_parts = vec!["m.conversation_id = ?".to_string()];
    let mut args = vec![SqlVal::Text(row.id.clone())];
    apply_filters(&mut where_parts, &mut args, since, until, sender, include_events)?;
    if let Some(t) = thread.filter(|s| !s.is_empty()) {
        where_parts.push("m.thread_id = ?".to_string());
        args.push(SqlVal::Text(t.to_string()));
    }
    let cond = where_parts.join(" AND ");
    let people_join = if sender.filter(|s| !s.is_empty()).is_some() {
        "LEFT JOIN people p ON p.mri = m.sender_mri"
    } else {
        ""
    };
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m {people_join} WHERE {cond}"),
        params_from_iter(args.clone()),
        |r| r.get(0),
    )?;
    let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
    if limit > 0 {
        args.push(SqlVal::Integer(limit as i64));
    }
    let sql = format!(
        "SELECT {} FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri \
         WHERE {cond} ORDER BY m.arrived_at DESC, m.pk DESC {limit_sql}",
        db::MESSAGE_COLS_SQL
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<MsgRow> = stmt
        .query_map(params_from_iter(args), |r| MsgRow::from_row(r, false))?
        .collect::<Result<Vec<_>, _>>()?;
    rows.reverse();
    let shown_len = rows.len() as i64;
    Ok(json!({
        "chat": row.to_json(),
        "messages": messages_json(conn, &rows, None, history)?,
        "matched": total,
        "truncated": total > shown_len,
        "otherMatches": others.iter().map(|o| json!({"id": o.id, "name": o.name, "kind": o.kind})).collect::<Vec<_>>(),
    }))
}

fn conversation_row_fallback(conn: &Connection, cid: &str) -> Result<ChatRow, ApiError> {
    let base = format!(
        "SELECT c.*, {} AS message_count, {} AS activity FROM conversations c WHERE c.id = ?",
        db::MESSAGE_COUNT_SQL,
        db::ACTIVITY_SQL
    );
    if let Some(r) = conn.query_row(&base, [cid], ChatRow::from_row).optional()? {
        return Ok(r);
    }
    let (count, act): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT SUM(kind = 'message'), MAX(arrived_at) FROM messages WHERE conversation_id = ?",
        [cid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(ChatRow {
        id: cid.to_string(),
        name: Some(cid.to_string()),
        kind: if cid.starts_with("48:") { "system" } else { "chat" }.to_string(),
        topic: None,
        team_id: None,
        message_count: count.unwrap_or(0),
        activity: act.unwrap_or(0),
    })
}

fn read_around(
    conn: &Connection,
    chat: Option<&str>,
    around: &str,
    context: usize,
    include_events: bool,
    thread: Option<&str>,
    history: bool,
) -> Result<Value, ApiError> {
    let mut mid = around.trim().to_string();
    let mut chat_owned = chat.map(String::from);
    if (mid.starts_with("19:") || mid.starts_with("48:")) && mid.contains('/') {
        if let Some((conv, m)) = mid.rsplit_once('/') {
            if chat_owned.as_deref().unwrap_or("").is_empty() {
                chat_owned = Some(conv.to_string());
            }
            mid = m.to_string();
        }
    }
    if mid.is_empty() {
        return Err(ApiError::bad("bad_param", "around needs a message id"));
    }
    let base = format!(
        "SELECT {} FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri",
        db::MESSAGE_COLS_SQL
    );
    let (row, others, anchor) = if let Some(ref cq) = chat_owned.filter(|s| !s.is_empty()) {
        let (r, o) = resolve_chat(conn, cq)?;
        let anc = conn
            .query_row(
                &format!("{base} WHERE m.conversation_id = ? AND m.message_id = ?"),
                [&r.id, &mid],
                |row| MsgRow::from_row(row, false),
            )
            .optional()?
            .ok_or_else(|| {
                ApiError::with_status(
                    "message_not_found",
                    format!(
                        "No message with id {} in {}.",
                        Quoted(&mid),
                        Quoted(r.name.as_deref().unwrap_or(&r.id))
                    ),
                    404,
                )
            })?;
        (r, o, anc)
    } else {
        let mut stmt = conn.prepare(&format!("{base} WHERE m.message_id = ?"))?;
        let found: Vec<MsgRow> = stmt
            .query_map([&mid], |r| MsgRow::from_row(r, false))?
            .collect::<Result<Vec<_>, _>>()?;
        if found.is_empty() {
            return Err(ApiError::with_status(
                "message_not_found",
                format!("No message with id {}.", Quoted(&mid)),
                404,
            ));
        }
        let mut chats: Vec<String> = found
            .iter()
            .map(|f| f.conversation_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        chats.sort();
        if chats.len() > 1 {
            return Err(ApiError::bad(
                "ambiguous_message",
                format!(
                    "Message {} appears in multiple chats; pass chat to disambiguate.",
                    Quoted(&mid)
                ),
            )
            .with_details(json!({"chats": chats})));
        }
        let anc = found.into_iter().next().unwrap();
        let r = conversation_row_fallback(conn, &anc.conversation_id)?;
        (r, Vec::new(), anc)
    };

    let mut where_parts = vec!["m.conversation_id = ?".to_string()];
    let mut args = vec![SqlVal::Text(row.id.clone())];
    if !include_events {
        where_parts.push("m.kind = 'message'".to_string());
    }
    if let Some(t) = thread.filter(|s| !s.is_empty()) {
        where_parts.push("m.thread_id = ?".to_string());
        args.push(SqlVal::Text(t.to_string()));
    }
    let cond = where_parts.join(" AND ");
    let key_t = anchor.arrived_at.unwrap_or(0);
    let key_pk = anchor.pk;

    let mut b_args = args.clone();
    b_args.push(SqlVal::Integer(key_t));
    b_args.push(SqlVal::Integer(key_pk));
    b_args.push(SqlVal::Integer((context + 1) as i64));
    let mut stmt_b = conn.prepare(&format!(
        "{base} WHERE {cond} AND (COALESCE(m.arrived_at, 0), m.pk) < (?, ?) \
         ORDER BY COALESCE(m.arrived_at, 0) DESC, m.pk DESC LIMIT ?"
    ))?;
    let before: Vec<MsgRow> = stmt_b
        .query_map(params_from_iter(b_args), |r| MsgRow::from_row(r, false))?
        .collect::<Result<_, _>>()?;

    let mut a_args = args.clone();
    a_args.push(SqlVal::Integer(key_t));
    a_args.push(SqlVal::Integer(key_pk));
    a_args.push(SqlVal::Integer((context + 1) as i64));
    let mut stmt_a = conn.prepare(&format!(
        "{base} WHERE {cond} AND (COALESCE(m.arrived_at, 0), m.pk) > (?, ?) \
         ORDER BY COALESCE(m.arrived_at, 0), m.pk LIMIT ?"
    ))?;
    let after: Vec<MsgRow> = stmt_a
        .query_map(params_from_iter(a_args), |r| MsgRow::from_row(r, false))?
        .collect::<Result<_, _>>()?;

    let has_before = before.len() > context;
    let has_after = after.len() > context;
    let mut rows: Vec<MsgRow> = before.into_iter().take(context).collect();
    rows.reverse();
    let around_id = anchor.message_id.clone();
    rows.push(anchor);
    rows.extend(after.into_iter().take(context));

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m WHERE {cond}"),
        params_from_iter(args),
        |r| r.get(0),
    )?;
    Ok(json!({
        "chat": row.to_json(),
        "messages": messages_json(conn, &rows, None, history)?,
        "matched": total,
        "truncated": has_before || has_after,
        "otherMatches": others.iter().map(|o| json!({"id": o.id, "name": o.name, "kind": o.kind})).collect::<Vec<_>>(),
        "aroundMessageId": around_id,
        "hasBefore": has_before,
        "hasAfter": has_after,
    }))
}

pub fn fts_query(text: &str, prefix: bool) -> Result<String, ApiError> {
    let mut parts: Vec<String> = Vec::new();
    let mut exclusions: Vec<String> = Vec::new();
    for m in FTS_TOK_RE.find_iter(text) {
        let tok = m.as_str();
        if matches!(tok, "OR" | "NOT" | "AND") {
            if let Some(last) = parts.last_mut() {
                if matches!(last.as_str(), "OR" | "NOT" | "AND") {
                    *last = tok.to_string();
                    continue;
                }
            }
            parts.push(tok.to_string());
            continue;
        }
        let (negated, body) = match tok.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest),
            _ => (false, tok),
        };
        let quoted = body.starts_with('"') && body.ends_with('"') && body.len() > 1;
        let phrase = if quoted { &body[1..body.len() - 1] } else { body };
        if !WORD_RE.is_match(phrase) {
            continue;
        }
        let escaped = phrase.replace('"', "\"\"");
        let suffix = if quoted || !prefix { "" } else { "*" };
        let term = format!("\"{escaped}\"{suffix}");
        if negated {
            exclusions.push(term);
        } else {
            parts.push(term);
        }
    }
    while parts
        .first()
        .map(|s| matches!(s.as_str(), "OR" | "NOT" | "AND"))
        .unwrap_or(false)
    {
        parts.remove(0);
    }
    while parts
        .last()
        .map(|s| matches!(s.as_str(), "OR" | "NOT" | "AND"))
        .unwrap_or(false)
    {
        parts.pop();
    }
    if parts.is_empty() {
        if !exclusions.is_empty() {
            return Err(ApiError::bad(
                "bad_query",
                "Exclusion terms (-word / NOT) must be combined with at least one positive search term.",
            ));
        }
        return Err(ApiError::bad("bad_query", "Search needs at least one word."));
    }
    let positive = parts.join(" ");
    let expr = if exclusions.is_empty() {
        positive
    } else {
        let nots = exclusions
            .into_iter()
            .map(|e| format!("NOT {e}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!("({positive}) {nots}")
    };
    Ok(format!("{{text subject}} : ({expr})"))
}

#[allow(clippy::too_many_arguments)]
pub fn search(
    conn: &Connection,
    query: Option<&str>,
    chat: Option<&str>,
    sender: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
    sort: &str,
    include_events: bool,
    include_history: bool,
    count_only: bool,
) -> Result<Value, ApiError> {
    search_with_options(
        conn,
        query,
        chat,
        sender,
        since,
        until,
        limit,
        sort,
        include_events,
        include_history,
        count_only,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn search_with_options(
    conn: &Connection,
    query: Option<&str>,
    chat: Option<&str>,
    sender: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
    sort: &str,
    include_events: bool,
    include_history: bool,
    count_only: bool,
    has_attachments: bool,
) -> Result<Value, ApiError> {
    if !SEARCH_SORTS.contains(&sort) {
        return Err(ApiError::bad(
            "bad_param",
            format!("sort must be one of {}", SEARCH_SORTS.join(", ")),
        ));
    }
    let words = query.unwrap_or("").trim();
    let since_ms = parse_when_or_error(since, false)?;
    let until_ms = parse_when_or_error(until, true)?;
    let has_sender = sender.filter(|s| !s.is_empty()).is_some();
    let has_chat = chat.filter(|s| !s.is_empty()).is_some();
    if words.is_empty() && !(has_sender || has_chat || since_ms.is_some() || until_ms.is_some() || has_attachments) {
        return Err(ApiError::bad(
            "bad_query",
            "Search needs at least one word, or a filter such as a sender, chat, time, or --has-attachments.",
        ));
    }
    let fq = if !words.is_empty() {
        Some(fts_query(words, true)?)
    } else {
        None
    };
    let mut where_parts: Vec<String> = Vec::new();
    let mut args: Vec<SqlVal> = Vec::new();
    let mut chat_obj = Value::Null;
    if let Some(c) = chat.filter(|s| !s.is_empty()) {
        let (row, _) = resolve_chat(conn, c)?;
        chat_obj = row.to_json();
        where_parts.push("m.conversation_id = ?".to_string());
        args.push(SqlVal::Text(row.id));
    }
    apply_filters(&mut where_parts, &mut args, since, until, sender, include_events)?;
    if has_attachments {
        where_parts.push(
            "EXISTS (SELECT 1 FROM attachments a WHERE a.conversation_id = m.conversation_id AND a.message_id = m.message_id)"
                .to_string(),
        );
    }
    let senders_val = if let Some(s) = sender.filter(|x| !x.is_empty()) {
        Value::Array(matching_senders(conn, s)?)
    } else {
        Value::Null
    };
    let mut head = Map::new();
    head.insert(
        "query".into(),
        if words.is_empty() {
            Value::Null
        } else {
            Value::String(words.to_string())
        },
    );
    head.insert("chat".into(), chat_obj);
    head.insert("senders".into(), senders_val);

    if let Some(ref fq_s) = fq {
        if include_history {
            return search_with_history(conn, words, fq_s, &where_parts, &args, limit, sort, count_only, head);
        }
    }

    if let Some(ref fq_s) = fq {
        where_parts.insert(0, "messages_fts MATCH ?".to_string());
        args.insert(0, SqlVal::Text(fq_s.clone()));
    }
    let cond = where_parts.join(" AND ");
    let people_join = if has_sender {
        "LEFT JOIN people p ON p.mri = m.sender_mri"
    } else {
        ""
    };

    let (total, pks): (i64, Vec<i64>) = if fq.is_some() {
        let source = "messages_fts CROSS JOIN messages m ON m.pk = messages_fts.rowid";
        if count_only {
            let total: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {source} {people_join} WHERE {cond}"),
                    params_from_iter(args),
                    |r| r.get(0),
                )
                .map_err(search_err)?;
            (total, Vec::new())
        } else if sort == "relevance" {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT m.pk, COALESCE(m.arrived_at, 0), bm25(messages_fts, 1.0, 2.5, 0.5) \
                     FROM {source} {people_join} WHERE {cond}"
                ))
                .map_err(search_err)?;
            let mut hits: Vec<(i64, i64, f64)> = stmt
                .query_map(params_from_iter(args), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))
                })
                .map_err(search_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_err)?;
            let total = hits.len() as i64;
            let cmp = |a: &(i64, i64, f64), b: &(i64, i64, f64)| {
                a.2.total_cmp(&b.2)
                    .then_with(|| b.1.cmp(&a.1))
                    .then_with(|| b.0.cmp(&a.0))
            };
            if limit > 0 && hits.len() > limit {
                hits.select_nth_unstable_by(limit, cmp);
                hits.truncate(limit);
            }
            hits.sort_unstable_by(cmp);
            (total, hits.into_iter().map(|(pk, _, _)| pk).collect())
        } else {
            use std::cmp::Reverse;
            use std::collections::BinaryHeap;
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT m.pk, COALESCE(m.arrived_at, 0) FROM {source} {people_join} WHERE {cond}"
                ))
                .map_err(search_err)?;
            let mut total: i64 = 0;
            let mut rows = stmt.query(params_from_iter(args)).map_err(search_err)?;
            let winners: Vec<i64> = if limit > 0 {
                let mut heap: BinaryHeap<Reverse<(i64, i64)>> = BinaryHeap::with_capacity(limit + 1);
                while let Some(r) = rows.next().map_err(search_err)? {
                    total += 1;
                    let pk: i64 = r.get(0).map_err(search_err)?;
                    let at: i64 = r.get(1).map_err(search_err)?;
                    if heap.len() < limit {
                        heap.push(Reverse((at, pk)));
                    } else if let Some(&Reverse(min_item)) = heap.peek() {
                        if (at, pk) > min_item {
                            heap.pop();
                            heap.push(Reverse((at, pk)));
                        }
                    }
                }
                let mut items: Vec<(i64, i64)> = heap.into_iter().map(|Reverse(x)| x).collect();
                items.sort_unstable_by(|a, b| b.cmp(a));
                items.into_iter().map(|(_, pk)| pk).collect()
            } else {
                let mut items: Vec<(i64, i64)> = Vec::new();
                while let Some(r) = rows.next().map_err(search_err)? {
                    total += 1;
                    let pk: i64 = r.get(0).map_err(search_err)?;
                    let at: i64 = r.get(1).map_err(search_err)?;
                    items.push((at, pk));
                }
                items.sort_unstable_by(|a, b| b.cmp(a));
                items.into_iter().map(|(_, pk)| pk).collect()
            };
            (total, winners)
        }
    } else {
        let total: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM messages m {people_join} WHERE {cond}"),
                params_from_iter(args.clone()),
                |r| r.get(0),
            )
            .map_err(search_err)?;
        if count_only || total == 0 {
            (total, Vec::new())
        } else {
            let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
            if limit > 0 {
                args.push(SqlVal::Integer(limit as i64));
            }
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT m.pk FROM messages m {people_join} WHERE {cond} ORDER BY m.arrived_at DESC, m.pk DESC {limit_sql}"
                ))
                .map_err(search_err)?;
            let pks = stmt
                .query_map(params_from_iter(args), |r| r.get::<_, i64>(0))
                .map_err(search_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(search_err)?;
            (total, pks)
        }
    };

    if count_only {
        head.insert("matched".into(), json!(total));
        head.insert("countOnly".into(), json!(true));
        return Ok(Value::Object(head));
    }
    if total == 0 {
        head.insert("results".into(), json!([]));
        head.insert("matched".into(), json!(0));
        head.insert("truncated".into(), json!(false));
        return Ok(Value::Object(head));
    }

    let rows = fetch_enriched_messages(conn, &pks)?;
    let terms = fq.as_deref().map(|_| extract_highlight_terms(words));
    let mut extra: HashMap<i64, Map<String, Value>> = HashMap::new();
    for r in &rows {
        let snippet = terms
            .as_deref()
            .and_then(|t| build_snippet(t, r.text.as_deref(), r.subject.as_deref()));
        let mut m = Map::new();
        m.insert("chatName".into(), json!(r.chat_name));
        m.insert("chatKind".into(), json!(r.chat_kind));
        m.insert("snippet".into(), json!(snippet));
        extra.insert(r.pk, m);
    }
    let results = messages_json(conn, &rows, Some(&extra), false)?;
    let truncated = total > results.len() as i64;
    head.insert("results".into(), Value::Array(results));
    head.insert("matched".into(), json!(total));
    head.insert("truncated".into(), json!(truncated));
    Ok(Value::Object(head))
}

fn fetch_enriched_messages(conn: &Connection, pks: &[i64]) -> Result<Vec<MsgRow>, ApiError> {
    let mut by_pk: HashMap<i64, MsgRow> = HashMap::with_capacity(pks.len());
    for chunk in pks.chunks(500) {
        let qs = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT {}, c.name AS chat_name, c.kind AS chat_kind \
             FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri \
             LEFT JOIN conversations c ON c.id = m.conversation_id \
             WHERE m.pk IN ({qs})",
            db::MESSAGE_COLS_SQL
        );
        let mut stmt = conn.prepare(&sql)?;
        for r in stmt.query_map(params_from_iter(chunk.iter().copied()), |r| MsgRow::from_row(r, true))? {
            let row = r?;
            by_pk.insert(row.pk, row);
        }
    }
    Ok(pks.iter().filter_map(|pk| by_pk.remove(pk)).collect())
}

enum HighlightTerm {
    Phrase(Vec<String>),
    Prefix(Vec<String>),
}

fn tokenize_words(s: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, ch) in s.char_indices() {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(st) = start.take() {
            out.push((st, idx, s[st..idx].to_lowercase()));
        }
    }
    if let Some(st) = start {
        out.push((st, s.len(), s[st..].to_lowercase()));
    }
    out
}

fn extract_highlight_terms(raw_query: &str) -> Vec<HighlightTerm> {
    let mut terms = Vec::new();
    let mut skip_next = false;
    for m in FTS_TOK_RE.find_iter(raw_query) {
        let tok = m.as_str();
        if tok == "NOT" {
            skip_next = true;
            continue;
        }
        if matches!(tok, "OR" | "AND") {
            skip_next = false;
            continue;
        }
        if skip_next {
            skip_next = false;
            continue;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            continue;
        }
        let quoted = tok.starts_with('"') && tok.ends_with('"') && tok.len() > 1;
        let body = if quoted { &tok[1..tok.len() - 1] } else { tok };
        let words: Vec<String> = tokenize_words(body).into_iter().map(|(_, _, w)| w).collect();
        if words.is_empty() {
            continue;
        }
        if quoted {
            terms.push(HighlightTerm::Phrase(words));
        } else {
            terms.push(HighlightTerm::Prefix(words));
        }
    }
    terms
}

fn render_snippet_field(terms: &[HighlightTerm], field: &str) -> Option<(usize, String)> {
    const WINDOW_WORDS: usize = 16;
    if field.trim().is_empty() {
        return None;
    }
    let words = tokenize_words(field);
    if words.is_empty() {
        return None;
    }
    let mut matched = vec![false; words.len()];
    let mut hit_count = 0usize;
    for i in 0..words.len() {
        for term in terms {
            match term {
                HighlightTerm::Phrase(parts) => {
                    if i + parts.len() <= words.len() && parts.iter().enumerate().all(|(j, p)| words[i + j].2 == *p) {
                        for m in &mut matched[i..i + parts.len()] {
                            if !*m {
                                *m = true;
                                hit_count += 1;
                            }
                        }
                    }
                }
                HighlightTerm::Prefix(parts) => {
                    if i + parts.len() <= words.len() {
                        let last = parts.len() - 1;
                        let ok = parts[..last].iter().enumerate().all(|(j, p)| words[i + j].2 == *p)
                            && words[i + last].2.starts_with(&parts[last]);
                        if ok {
                            for m in &mut matched[i..i + parts.len()] {
                                if !*m {
                                    *m = true;
                                    hit_count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if hit_count == 0 {
        return None;
    }
    let first_hit = matched.iter().position(|&m| m).unwrap_or(0);
    let win_start = if words.len() <= WINDOW_WORDS {
        0
    } else {
        first_hit
            .saturating_sub(WINDOW_WORDS / 3)
            .min(words.len() - WINDOW_WORDS)
    };
    let win_end = (win_start + WINDOW_WORDS).min(words.len());

    let byte_start = words[win_start].0;
    let byte_end = words[win_end - 1].1;
    let mut out = String::with_capacity((byte_end - byte_start) + 32);
    if win_start > 0 {
        out.push('…');
    }
    let mut cursor = byte_start;
    for idx in win_start..win_end {
        let (ws, we, _) = words[idx];
        if ws > cursor {
            out.push_str(&field[cursor..ws]);
        }
        if matched[idx] {
            out.push('«');
            out.push_str(&field[ws..we]);
            out.push('»');
        } else {
            out.push_str(&field[ws..we]);
        }
        cursor = we;
    }
    if win_end < words.len() {
        out.push('…');
    }
    Some((hit_count, out))
}

fn build_snippet(terms: &[HighlightTerm], text: Option<&str>, subject: Option<&str>) -> Option<String> {
    if !terms.is_empty() {
        if let Some(t) = text {
            if let Some((_, sn)) = render_snippet_field(terms, t) {
                return Some(sn);
            }
        }
        if let Some(s) = subject {
            if let Some((_, sn)) = render_snippet_field(terms, s) {
                return Some(sn);
            }
        }
    }
    let src = text
        .filter(|s| !s.trim().is_empty())
        .or_else(|| subject.filter(|s| !s.trim().is_empty()))?;
    let words = tokenize_words(src);
    if words.len() <= 16 {
        return Some(src.to_string());
    }
    let end = words[15].1;
    Some(format!("{}…", &src[..end]))
}

struct Hit {
    at: i64,
    score: f64,
    revision: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
fn search_with_history(
    conn: &Connection,
    raw_query: &str,
    fq: &str,
    where_parts: &[String],
    args: &[SqlVal],
    limit: usize,
    sort: &str,
    count_only: bool,
    mut head: Map<String, Value>,
) -> Result<Value, ApiError> {
    let filt: String = where_parts.iter().map(|w| format!(" AND {w}")).collect();
    let cur_sql = format!(
        "SELECT m.pk, COALESCE(m.arrived_at, 0), bm25(messages_fts, 1.0, 2.5, 0.5) \
         FROM messages_fts CROSS JOIN messages m ON m.pk = messages_fts.rowid \
         LEFT JOIN people p ON p.mri = m.sender_mri \
         WHERE messages_fts MATCH ?{filt}"
    );
    let hist_sql = format!(
        "SELECT m.pk, COALESCE(m.arrived_at, 0), bm25(message_revisions_fts, 1.0, 2.5), r.id \
         FROM message_revisions_fts CROSS JOIN message_revisions r ON r.id = message_revisions_fts.rowid \
         JOIN messages m ON m.conversation_id = r.conversation_id AND m.message_id = r.message_id \
         LEFT JOIN people p ON p.mri = m.sender_mri \
         WHERE message_revisions_fts MATCH ?{filt}"
    );
    let mut full_args = Vec::with_capacity(1 + args.len());
    full_args.push(SqlVal::Text(fq.to_string()));
    full_args.extend(args.iter().cloned());

    let mut hits: HashMap<i64, Hit> = HashMap::new();
    {
        let mut stmt = conn.prepare(&cur_sql).map_err(search_err)?;
        let rows = stmt
            .query_map(params_from_iter(full_args.clone()), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, f64>(2)?))
            })
            .map_err(search_err)?;
        for r in rows {
            let (pk, at, score) = r.map_err(search_err)?;
            hits.insert(
                pk,
                Hit {
                    at,
                    score,
                    revision: None,
                },
            );
        }
    }
    {
        let mut stmt = conn.prepare(&hist_sql).map_err(search_err)?;
        let rows = stmt
            .query_map(params_from_iter(full_args), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .map_err(search_err)?;
        for r in rows {
            let (pk, at, score, rev_id) = r.map_err(search_err)?;
            let replace = match hits.get(&pk) {
                None => true,
                Some(cur) => cur.revision.is_some() && score < cur.score,
            };
            if replace {
                hits.insert(
                    pk,
                    Hit {
                        at,
                        score,
                        revision: Some(rev_id),
                    },
                );
            }
        }
    }
    let total = hits.len() as i64;
    if count_only {
        head.insert("matched".into(), json!(total));
        head.insert("countOnly".into(), json!(true));
        return Ok(Value::Object(head));
    }
    let mut order: Vec<i64> = hits.keys().copied().collect();
    if sort == "relevance" {
        order.sort_by(|a, b| {
            let ha = &hits[a];
            let hb = &hits[b];
            ha.score
                .total_cmp(&hb.score)
                .then_with(|| hb.at.cmp(&ha.at))
                .then_with(|| b.cmp(a))
        });
    } else {
        order.sort_by(|a, b| {
            let ha = &hits[a];
            let hb = &hits[b];
            hb.at.cmp(&ha.at).then_with(|| b.cmp(a))
        });
    }
    if limit > 0 {
        order.truncate(limit);
    }

    let terms = extract_highlight_terms(raw_query);
    let rev_ids: Vec<i64> = order.iter().filter_map(|pk| hits[pk].revision).collect();
    let mut rev_texts: HashMap<i64, (Option<String>, Option<String>)> = HashMap::new();
    for chunk in rev_ids.chunks(500) {
        let qs = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, text, subject FROM message_revisions WHERE id IN ({qs})"
        ))?;
        for r in stmt.query_map(params_from_iter(chunk.iter().copied()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (id, text, subj) = r?;
            rev_texts.insert(id, (text, subj));
        }
    }

    let rows = fetch_enriched_messages(conn, &order)?;
    let mut extra: HashMap<i64, Map<String, Value>> = HashMap::new();
    for r in &rows {
        let h = &hits[&r.pk];
        let snippet = match h.revision {
            Some(rid) => rev_texts
                .get(&rid)
                .and_then(|(t, s)| build_snippet(&terms, t.as_deref(), s.as_deref())),
            None => build_snippet(&terms, r.text.as_deref(), r.subject.as_deref()),
        };
        let mut m = Map::new();
        m.insert("chatName".into(), json!(r.chat_name));
        m.insert("chatKind".into(), json!(r.chat_kind));
        m.insert("snippet".into(), json!(snippet));
        m.insert("matchedInHistory".into(), json!(h.revision.is_some()));
        extra.insert(r.pk, m);
    }
    let mut out = messages_json(conn, &rows, Some(&extra), false)?;
    let hist_keys: Vec<(String, String)> = rows
        .iter()
        .filter(|r| hits[&r.pk].revision.is_some())
        .map(|r| (r.conversation_id.clone(), r.message_id.clone()))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    if !hist_keys.is_empty() {
        let revs = fetch_revisions_by_msg(conn, &hist_keys)?;
        for (r, d) in rows.iter().zip(out.iter_mut()) {
            if hits[&r.pk].revision.is_some() {
                if let Some(obj) = d.as_object_mut() {
                    obj.insert("history".into(), history_json(r, &revs));
                }
            }
        }
    }
    let truncated = total > out.len() as i64;
    head.insert("results".into(), Value::Array(out));
    head.insert("matched".into(), json!(total));
    head.insert("truncated".into(), json!(truncated));
    Ok(Value::Object(head))
}

pub fn matching_senders(conn: &Connection, sender: &str) -> Result<Vec<Value>, ApiError> {
    let pat = db::like_pattern(sender);
    let mut stmt = conn.prepare(&format!(
        "SELECT m.sender_mri, COALESCE(NULLIF(p.name, ''), NULLIF(m.sender_name, ''), m.sender_mri), \
                p.email, COUNT(*) AS sent \
         FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri \
         WHERE {SENDER_COND} AND m.kind = 'message' \
         GROUP BY m.sender_mri ORDER BY sent DESC LIMIT 20"
    ))?;
    let rows = stmt
        .query_map([&pat, &pat, &pat, sender], |r| {
            Ok(json!({
                "id": r.get::<_, Option<String>>(0)?,
                "name": r.get::<_, Option<String>>(1)?,
                "email": r.get::<_, Option<String>>(2)?,
                "messages": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn file_type_ext(name: Option<&str>) -> Option<String> {
    let n = name?.trim_matches(|c| c == '.' || c == ' ');
    let (_, ext) = n.rsplit_once('.')?;
    let ext = ext.trim().to_ascii_lowercase();
    if !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        Some(ext)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub fn attachments(
    conn: &Connection,
    query: Option<&str>,
    chat: Option<&str>,
    sender: Option<&str>,
    kind: Option<&str>,
    file_type: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Value, ApiError> {
    if let Some(k) = kind.filter(|s| !s.is_empty()) {
        if !ATTACHMENT_KINDS.contains(&k) {
            return Err(ApiError::bad(
                "bad_param",
                format!("kind must be one of {}", ATTACHMENT_KINDS.join(", ")),
            ));
        }
    }
    let mut where_parts: Vec<String> = Vec::new();
    let mut args: Vec<SqlVal> = Vec::new();
    if let Some(q) = query.filter(|s| !s.is_empty()) {
        where_parts.push("(a.name LIKE ? ESCAPE '\\' OR a.url LIKE ? ESCAPE '\\')".to_string());
        let p = db::like_pattern(q);
        args.push(SqlVal::Text(p.clone()));
        args.push(SqlVal::Text(p));
    }
    if let Some(k) = kind.filter(|s| !s.is_empty()) {
        where_parts.push("a.kind = ?".to_string());
        args.push(SqlVal::Text(k.to_string()));
    }
    if let Some(ft) = file_type.filter(|s| !s.is_empty()) {
        where_parts.push("a.kind = 'file' AND lower(a.name) LIKE ? ESCAPE '\\'".to_string());
        let cleaned = ft.to_ascii_lowercase();
        let ext = cleaned.trim_start_matches('.');
        args.push(SqlVal::Text(format!("%.{}", db::like_escape(ext))));
    }
    let mut chat_obj = Value::Null;
    if let Some(c) = chat.filter(|s| !s.is_empty()) {
        let (row, _) = resolve_chat(conn, c)?;
        chat_obj = row.to_json();
        where_parts.push("m.conversation_id = ?".to_string());
        args.push(SqlVal::Text(row.id));
    }
    apply_filters(&mut where_parts, &mut args, since, until, sender, true)?;
    let cond = if where_parts.is_empty() {
        "1".to_string()
    } else {
        where_parts.join(" AND ")
    };
    let count_base = if sender.filter(|s| !s.is_empty()).is_some() {
        "FROM attachments a JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id LEFT JOIN people p ON p.mri = m.sender_mri"
    } else {
        "FROM attachments a JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id"
    };
    let base = "FROM attachments a \
                JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id \
                LEFT JOIN people p ON p.mri = m.sender_mri \
                LEFT JOIN conversations c ON c.id = m.conversation_id";
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) {count_base} WHERE {cond}"),
        params_from_iter(args.clone()),
        |r| r.get(0),
    )?;
    let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
    if limit > 0 {
        args.push(SqlVal::Integer(limit as i64));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT a.kind, a.name, a.url, m.message_id, m.conversation_id, m.thread_id, m.arrived_at, m.deleted_at, \
                m.sender_mri, COALESCE(NULLIF(m.sender_name, ''), p.name, m.sender_mri) AS sender, \
                c.name AS chat_name, c.kind AS chat_kind, substr(m.text, 1, 200) AS text \
         {base} WHERE {cond} ORDER BY m.arrived_at DESC, a.id DESC {limit_sql}"
    ))?;
    struct AttRow {
        kind: String,
        name: Option<String>,
        url: String,
        message_id: String,
        conversation_id: String,
        thread_id: Option<String>,
        arrived_at: Option<i64>,
        deleted_at: Option<i64>,
        sender_mri: Option<String>,
        sender: Option<String>,
        chat_name: Option<String>,
        chat_kind: Option<String>,
        text: Option<String>,
    }
    let rows: Vec<AttRow> = stmt
        .query_map(params_from_iter(args), |r| {
            Ok(AttRow {
                kind: r.get(0)?,
                name: r.get(1)?,
                url: r.get(2)?,
                message_id: r.get(3)?,
                conversation_id: r.get(4)?,
                thread_id: r.get(5)?,
                arrived_at: r.get(6)?,
                deleted_at: r.get(7)?,
                sender_mri: r.get(8)?,
                sender: r.get(9)?,
                chat_name: r.get(10)?,
                chat_kind: r.get(11)?,
                text: r.get(12)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let need_od = rows
        .iter()
        .any(|r| r.kind == "file" && r.name.as_deref().map(|s| !s.is_empty()).unwrap_or(false));
    let od_idx = if need_od {
        Some(onedrive::list_onedrive_files())
    } else {
        None
    };
    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            let local_path = if r.kind == "file" {
                r.name
                    .as_deref()
                    .and_then(|n| onedrive::find_onedrive_file(n, od_idx.as_ref()))
                    .map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            };
            json!({
                "kind": r.kind,
                "name": r.name,
                "type": if r.kind == "file" { file_type_ext(r.name.as_deref()) } else { None },
                "url": r.url,
                "time": iso(r.arrived_at),
                "timeMs": r.arrived_at,
                "sender": r.sender,
                "senderId": r.sender_mri,
                "chatId": r.conversation_id,
                "chatName": r.chat_name,
                "chatKind": r.chat_kind,
                "messageId": r.message_id,
                "threadId": r.thread_id,
                "messageDeleted": r.deleted_at.map(|v| v != 0).unwrap_or(false),
                "messageText": r.text.as_deref().unwrap_or(""),
                "localPath": local_path,
            })
        })
        .collect();
    let senders_val = if let Some(s) = sender.filter(|x| !x.is_empty()) {
        Value::Array(matching_senders(conn, s)?)
    } else {
        Value::Null
    };
    Ok(json!({
        "chat": chat_obj,
        "senders": senders_val,
        "attachments": out,
        "matched": total,
        "truncated": total > rows.len() as i64,
    }))
}

pub fn changes(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    chat: Option<&str>,
    what: &str,
    limit: usize,
) -> Result<Value, ApiError> {
    if !matches!(what, "all" | "edited" | "deleted") {
        return Err(ApiError::bad("bad_param", "what must be all, edited or deleted"));
    }
    let lo = parse_when_or_error(since, false)?.unwrap_or(1).max(1);
    let hi = parse_when_or_error(until, true)?.unwrap_or(i64::MAX / 2);
    let mut conds = Vec::new();
    let mut args: Vec<SqlVal> = Vec::new();
    if matches!(what, "all" | "edited") {
        conds.push("(m.edited_at >= ? AND m.edited_at < ?)");
        args.push(SqlVal::Integer(lo));
        args.push(SqlVal::Integer(hi));
    }
    if matches!(what, "all" | "deleted") {
        conds.push("(m.deleted_at >= ? AND m.deleted_at < ?)");
        args.push(SqlVal::Integer(lo));
        args.push(SqlVal::Integer(hi));
    }
    let mut where_parts = vec![format!("({})", conds.join(" OR ")), "m.kind = 'message'".to_string()];
    let mut chat_obj = Value::Null;
    if let Some(c) = chat.filter(|s| !s.is_empty()) {
        let (row, _) = resolve_chat(conn, c)?;
        chat_obj = row.to_json();
        where_parts.push("m.conversation_id = ?".to_string());
        args.push(SqlVal::Text(row.id));
    }
    let cond = where_parts.join(" AND ");
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM messages m WHERE {cond}"),
        params_from_iter(args.clone()),
        |r| r.get(0),
    )?;
    let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
    if limit > 0 {
        args.push(SqlVal::Integer(limit as i64));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {}, c.name AS chat_name, c.kind AS chat_kind, \
                MAX(COALESCE(m.edited_at, 0), COALESCE(m.deleted_at, 0)) AS changed_at \
         FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri \
         LEFT JOIN conversations c ON c.id = m.conversation_id \
         WHERE {cond} ORDER BY changed_at DESC {limit_sql}",
        db::MESSAGE_COLS_SQL
    ))?;
    let rows: Vec<MsgRow> = stmt
        .query_map(params_from_iter(args), |r| MsgRow::from_row(r, true))?
        .collect::<Result<_, _>>()?;
    let mut extra = HashMap::new();
    for r in &rows {
        let mut m = Map::new();
        m.insert("chatName".into(), json!(r.chat_name));
        m.insert("chatKind".into(), json!(r.chat_kind));
        extra.insert(r.pk, m);
    }
    Ok(json!({
        "chat": chat_obj,
        "changes": messages_json(conn, &rows, Some(&extra), true)?,
        "matched": total,
        "truncated": total > rows.len() as i64,
    }))
}

pub fn record_sources(conn: &Connection) -> Result<Value, ApiError> {
    let mut stmt = conn.prepare(
        "SELECT source, COUNT(DISTINCT record_key) AS records, COUNT(*) AS versions, MAX(captured_at) AS last \
         FROM record_versions GROUP BY source ORDER BY source",
    )?;
    let sources: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "records": r.get::<_, i64>(1)?,
                "versions": r.get::<_, i64>(2)?,
                "lastCaptured": iso(r.get::<_, Option<i64>>(3)?),
            }))
        })?
        .collect::<Result<_, _>>()?;
    Ok(json!({"sources": sources}))
}

pub fn records(
    conn: &Connection,
    source: &str,
    key: Option<&str>,
    since: Option<&str>,
    limit: usize,
    all_versions: bool,
) -> Result<Value, ApiError> {
    let mut where_parts = vec!["source = ?".to_string()];
    let mut args = vec![SqlVal::Text(source.to_string())];
    if let Some(k) = key.filter(|s| !s.is_empty()) {
        where_parts.push("record_key = ?".to_string());
        args.push(SqlVal::Text(k.to_string()));
    }
    if let Some(t) = parse_when_or_error(since, false)? {
        where_parts.push("captured_at >= ?".to_string());
        args.push(SqlVal::Integer(t));
    }
    if !all_versions {
        let sub_cond = where_parts.join(" AND ");
        where_parts.push(format!(
            "id IN (SELECT MAX(id) FROM record_versions WHERE {sub_cond} GROUP BY record_key)"
        ));
        let dup = args.clone();
        args.extend(dup);
    }
    let cond = where_parts.join(" AND ");
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM record_versions WHERE {cond}"),
        params_from_iter(args.clone()),
        |r| r.get(0),
    )?;
    if total == 0 {
        let exists = conn
            .query_row(
                "SELECT 1 FROM record_versions WHERE source = ? LIMIT 1",
                [source],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            let known: Vec<String> = record_sources(conn)?
                .get("sources")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.get("source").and_then(|x| x.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            return Err(ApiError::with_status(
                "unknown_source",
                format!(
                    "No records from {}. Known sources: {}",
                    Quoted(source),
                    known.join(", ")
                ),
                404,
            ));
        }
    }
    let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
    if limit > 0 {
        args.push(SqlVal::Integer(limit as i64));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT record_key, captured_at, data FROM record_versions WHERE {cond} \
         ORDER BY captured_at DESC, id DESC {limit_sql}"
    ))?;
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(args))?;
    while let Some(r) = rows.next()? {
        let rk: String = r.get(0)?;
        let cap: i64 = r.get(1)?;
        let data = archive::decode_record_ref(r.get_ref(2)?).map_err(|e| ApiError::with_status("internal", e, 500))?;
        out.push(json!({
            "key": rk,
            "capturedAt": iso(Some(cap)),
            "data": data,
        }));
    }
    Ok(json!({
        "source": source,
        "records": out,
        "matched": total,
        "truncated": total > out.len() as i64,
    }))
}

pub fn people(conn: &Connection, query: Option<&str>, limit: usize, include_bots: bool) -> Result<Value, ApiError> {
    let mut where_parts: Vec<String> = Vec::new();
    let mut args: Vec<SqlVal> = Vec::new();
    if let Some(q) = query.filter(|s| !s.is_empty()) {
        where_parts
            .push("(name LIKE ? ESCAPE '\\' OR email LIKE ? ESCAPE '\\' OR job_title LIKE ? ESCAPE '\\')".to_string());
        let p = db::like_pattern(q);
        args.push(SqlVal::Text(p.clone()));
        args.push(SqlVal::Text(p.clone()));
        args.push(SqlVal::Text(p));
    }
    if !include_bots {
        where_parts.push("COALESCE(p.kind, '') != 'bot'".to_string());
    }
    let where_sql = if where_parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_parts.join(" AND "))
    };
    let limit_sql = if limit > 0 { "LIMIT ?" } else { "" };
    if limit > 0 {
        args.push(SqlVal::Integer(limit as i64));
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT p.mri, p.name, p.email, p.job_title, p.department, p.kind, \
                (SELECT COUNT(*) FROM messages m WHERE m.sender_mri = p.mri) AS sent \
         FROM people p {where_sql} ORDER BY sent DESC, name COLLATE NOCASE {limit_sql}"
    ))?;
    let out: Vec<Value> = stmt
        .query_map(params_from_iter(args), |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, Option<String>>(1)?,
                "email": r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                "jobTitle": r.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
                "department": r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
                "kind": r.get::<_, Option<String>>(5)?,
                "messagesInArchive": r.get::<_, i64>(6)?,
            }))
        })?
        .collect::<Result<_, _>>()?;
    Ok(json!({"people": out}))
}

pub fn person(conn: &Connection, person_q: &str, limit: usize) -> Result<Value, ApiError> {
    let q = person_q.trim();
    if q.is_empty() {
        return Err(ApiError::bad("bad_param", "person is required"));
    }
    let pat = db::like_pattern(q);
    #[derive(Clone)]
    struct Cand {
        id: String,
        name: Option<String>,
        email: Option<String>,
        job_title: Option<String>,
        department: Option<String>,
        kind: Option<String>,
        sent: i64,
    }
    let mut cands: HashMap<String, Cand> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT p.mri, p.name, p.email, p.job_title, p.department, p.kind, \
                    (SELECT COUNT(*) FROM messages m WHERE m.sender_mri = p.mri AND m.kind = 'message') AS sent \
             FROM people p WHERE p.mri = ? OR lower(p.email) = lower(?) OR p.name LIKE ? ESCAPE '\\' \
                OR p.email LIKE ? ESCAPE '\\' OR p.job_title LIKE ? ESCAPE '\\'",
        )?;
        for r in stmt.query_map([q, q, &pat, &pat, &pat], |r| {
            Ok(Cand {
                id: r.get(0)?,
                name: r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                email: r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                job_title: r.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
                department: r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
                kind: r.get::<_, Option<String>>(5)?.filter(|s| !s.is_empty()),
                sent: r.get(6)?,
            })
        })? {
            let c = r?;
            cands.insert(c.id.clone(), c);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT m.sender_mri, MAX(NULLIF(m.sender_name, '')), SUM(m.kind = 'message') \
             FROM messages m WHERE (m.sender_mri = ? OR m.sender_name LIKE ? ESCAPE '\\') \
               AND m.sender_mri IS NOT NULL AND m.sender_mri != '' GROUP BY m.sender_mri",
        )?;
        let rows: Vec<(String, Option<String>, Option<i64>)> = stmt
            .query_map([q, &pat], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        for (mri, sname, sent) in rows {
            if !cands.contains_key(&mri) {
                let p = conn
                    .query_row(
                        "SELECT name, email, job_title, department, kind FROM people WHERE mri = ?",
                        [&mri],
                        |r| {
                            Ok((
                                r.get::<_, Option<String>>(0)?.filter(|s| !s.is_empty()),
                                r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                                r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                                r.get::<_, Option<String>>(3)?.filter(|s| !s.is_empty()),
                                r.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
                            ))
                        },
                    )
                    .optional()?;
                let (pn, pe, pj, pd, pk) = p.unwrap_or((None, None, None, None, None));
                cands.insert(
                    mri.clone(),
                    Cand {
                        id: mri,
                        name: pn.or(sname),
                        email: pe,
                        job_title: pj,
                        department: pd,
                        kind: pk,
                        sent: sent.unwrap_or(0),
                    },
                );
            }
        }
    }
    if cands.is_empty() {
        return Err(ApiError::with_status(
            "person_not_found",
            format!("No person matches {}.", Quoted(person_q)),
            404,
        ));
    }
    let ql = q.to_ascii_lowercase();
    let mut ranked: Vec<Cand> = cands.into_values().collect();
    ranked.sort_by(|a, b| {
        (b.id == q)
            .cmp(&(a.id == q))
            .then_with(|| {
                (b.email.as_deref().unwrap_or("").to_ascii_lowercase() == ql)
                    .cmp(&(a.email.as_deref().unwrap_or("").to_ascii_lowercase() == ql))
            })
            .then_with(|| {
                (b.name.as_deref().unwrap_or("").to_ascii_lowercase() == ql)
                    .cmp(&(a.name.as_deref().unwrap_or("").to_ascii_lowercase() == ql))
            })
            .then_with(|| b.sent.cmp(&a.sent))
    });
    let best = ranked.remove(0);
    ranked.truncate(8);

    let mut mris = HashSet::new();
    mris.insert(best.id.clone());
    if let Some(poid) = identity::valid_oid(&best.id) {
        let mut stmt = conn.prepare("SELECT DISTINCT sender_mri FROM messages WHERE sender_mri LIKE ? ESCAPE '\\'")?;
        for r in stmt.query_map([format!("%:{}", db::like_escape(&poid))], |r| r.get::<_, String>(0))? {
            mris.insert(r?);
        }
    }
    let mri_list: Vec<String> = mris.iter().cloned().collect();
    let ph = vec!["?"; mri_list.len()].join(",");
    let (sent, first, last): (i64, Option<i64>, Option<i64>) = conn.query_row(
        &format!(
            "SELECT COUNT(*), MIN(arrived_at), MAX(arrived_at) FROM messages WHERE sender_mri IN ({ph}) AND kind = 'message'"
        ),
        params_from_iter(mri_list.iter().cloned()),
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let ids = conversations_with(conn, &mris)?;
    let chats: Vec<Value> = if ids.is_empty() {
        Vec::new()
    } else {
        let mut cargs: Vec<SqlVal> = mri_list.iter().cloned().map(SqlVal::Text).collect();
        cargs.push(SqlVal::Text(db::id_list(&ids)));
        let mut stmt = conn.prepare(&format!(
            "SELECT c.id, c.name, c.kind, {} AS message_count, {} AS activity, \
                    (SELECT COUNT(*) FROM messages x WHERE x.conversation_id = c.id AND x.kind = 'message' \
                     AND +x.sender_mri IN ({ph})) AS from_person \
             FROM conversations c WHERE c.kind != 'system' AND c.id IN (SELECT value FROM json_each(?)) \
             ORDER BY activity DESC",
            db::MESSAGE_COUNT_SQL,
            db::ACTIVITY_SQL
        ))?;
        let res: Vec<Value> = stmt
            .query_map(params_from_iter(cargs), |r| {
                let act = r.get::<_, Option<i64>>(4)?.unwrap_or(0);
                Ok(json!({
                    "id": r.get::<_, String>(0)?,
                    "name": r.get::<_, Option<String>>(1)?,
                    "kind": r.get::<_, String>(2)?,
                    "messagesFromPerson": r.get::<_, i64>(5)?,
                    "messageCount": r.get::<_, i64>(3)?,
                    "lastActivity": iso(if act != 0 { Some(act) } else { None }),
                    "lastActivityMs": if act != 0 { Some(act) } else { None },
                }))
            })?
            .collect::<Result<_, _>>()?;
        res
    };
    let total_shared = chats.len();
    let shown_chats: Vec<Value> = if limit > 0 {
        chats.into_iter().take(limit).collect()
    } else {
        chats
    };

    let att_base = format!(
        "FROM attachments a JOIN messages m ON m.conversation_id = a.conversation_id AND m.message_id = a.message_id \
         LEFT JOIN conversations c ON c.id = m.conversation_id WHERE m.sender_mri IN ({ph})"
    );
    let total_atts: i64 = conn.query_row(
        &format!("SELECT COUNT(*) {att_base}"),
        params_from_iter(mri_list.iter().cloned()),
        |r| r.get(0),
    )?;
    let mut aargs: Vec<SqlVal> = mri_list.iter().cloned().map(SqlVal::Text).collect();
    let lim_sql = if limit > 0 {
        aargs.push(SqlVal::Integer(limit as i64));
        "LIMIT ?"
    } else {
        ""
    };
    let mut astmt = conn.prepare(&format!(
        "SELECT a.kind, a.name, a.url, m.arrived_at, m.conversation_id, m.message_id, c.name \
         {att_base} ORDER BY m.arrived_at DESC, a.id DESC {lim_sql}"
    ))?;
    let recent_files: Vec<Value> = astmt
        .query_map(params_from_iter(aargs), |r| {
            let kind: String = r.get(0)?;
            let name: Option<String> = r.get(1)?;
            let arrived: Option<i64> = r.get(3)?;
            Ok(json!({
                "kind": kind,
                "name": name,
                "type": if kind == "file" { file_type_ext(name.as_deref()) } else { None },
                "url": r.get::<_, String>(2)?,
                "time": iso(arrived),
                "timeMs": arrived,
                "chatId": r.get::<_, String>(4)?,
                "chatName": r.get::<_, Option<String>>(6)?,
                "messageId": r.get::<_, String>(5)?,
            }))
        })?
        .collect::<Result<_, _>>()?;

    Ok(json!({
        "person": {
            "id": best.id,
            "name": best.name,
            "email": best.email,
            "jobTitle": best.job_title,
            "department": best.department,
            "kind": best.kind,
            "messagesInArchive": sent,
            "firstContact": iso(first),
            "lastContact": iso(last),
        },
        "sharedChats": shown_chats,
        "totalSharedChats": total_shared,
        "recentFiles": recent_files,
        "totalAttachments": total_atts,
        "otherMatches": ranked.iter().map(|c| json!({"id": c.id, "name": c.name, "email": c.email, "messagesInArchive": c.sent})).collect::<Vec<_>>(),
    }))
}

fn run_json(r: Option<&archive::SyncRun>) -> Value {
    let Some(r) = r else { return Value::Null };
    let stats: Value = serde_json::from_str(&r.stats).unwrap_or_else(|_| json!({}));
    json!({
        "status": r.status,
        "startedAt": iso(Some(r.started_at)),
        "finishedAt": iso(r.finished_at),
        "message": r.message,
        "stats": stats,
    })
}

pub fn status(conn: &Connection) -> Result<Value, ApiError> {
    let one = |sql: &str| -> i64 {
        conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
            .ok()
            .flatten()
            .unwrap_or(0)
    };
    let last = archive::last_run(conn, false);
    let ok = archive::last_run(conn, true);
    let preserved = if let Some(ref ok_r) = ok {
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE kind = 'message' AND last_seen_at < ?",
            [ok_r.started_at],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
    } else {
        0
    };
    let db_path = paths::archive_path();
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let size: u64 = [&db_path, &wal_path]
        .iter()
        .filter_map(|p| p.metadata().ok().map(|m| m.len()))
        .sum();
    let bks = backups::list_backups(None);
    let scheduled_bks: Vec<&PathBuf> = bks
        .iter()
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("archive-before-restore-"))
                .unwrap_or(false)
        })
        .collect();
    let free = paths::disk_free_bytes(&db_path);
    let now = archive::now_ms();
    let mut problems: Vec<String> = Vec::new();
    if ok.is_none() {
        problems.push("No successful sync yet. Run `teams sync`.".to_string());
    }
    let ok_fin = ok.as_ref().and_then(|r| r.finished_at).unwrap_or(0);
    let last_checked_meta = archive::get_meta(conn, "last_checked_at")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let checked = ok_fin.max(last_checked_meta);
    if ok.is_some() && now - checked > 2 * 3_600_000 {
        problems.push(format!(
            "Teams cache last checked {}; the background sync may not be running.",
            iso(Some(checked)).unwrap_or_default()
        ));
    }
    if let Some(ref l) = last {
        if l.status == "error" {
            problems.push(format!("Last sync failed: {}", l.message.as_deref().unwrap_or("")));
        } else if l.status == "warning" {
            problems.push(format!("Last sync warning: {}", l.message.as_deref().unwrap_or("")));
        }
    }
    if ok.is_some() && ok_fin > 0 && now - ok_fin > identity::STALE_SOURCE_DAYS * identity::DAY_MS {
        problems.push(format!(
            "The Teams cache hasn't changed since {}. If you've used Teams since then, \
             it may be storing data somewhere new; check `teams raw stores`.",
            iso(Some(ok_fin)).unwrap_or_default()
        ));
    }
    if let Some(warn) = archive::get_meta(conn, "backup_warning").filter(|s| !s.trim().is_empty()) {
        problems.push(format!("Last backup warning: {warn}"));
    }
    if let Some(err) = archive::get_meta(conn, "backup_error").filter(|s| !s.is_empty()) {
        problems.push(format!("Last backup failed: {err}"));
    } else if ok.is_some() {
        let latest_bk_ms = scheduled_bks
            .last()
            .and_then(|p| p.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if scheduled_bks.is_empty() || ok_fin - latest_bk_ms > 2 * identity::DAY_MS {
            problems.push("No recent backup. Run `teams backup`.".to_string());
        }
    }
    if free < backups::MIN_FREE_BYTES {
        problems.push(format!(
            "Disk nearly full ({:.1} GB free): backups are paused and syncs may fail.",
            free as f64 / 1e9
        ));
    }

    let (oldest, newest): (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT MIN(arrived_at), MAX(arrived_at) FROM messages WHERE kind = 'message'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    Ok(json!({
        "healthy": problems.is_empty(),
        "problems": problems,
        "archive": {
            "path": db_path.to_string_lossy(),
            "sizeBytes": size,
            "schemaVersion": one("PRAGMA user_version"),
            "diskFreeBytes": free,
        },
        "counts": {
            "messages": one("SELECT COUNT(*) FROM messages WHERE kind = 'message'"),
            "events": one("SELECT COUNT(*) FROM messages WHERE kind = 'event'"),
            "chats": one("SELECT COUNT(*) FROM conversations WHERE kind != 'system'"),
            "people": one("SELECT COUNT(*) FROM people"),
            "revisions": one("SELECT COUNT(*) FROM message_revisions"),
            "attachments": one("SELECT COUNT(*) FROM attachments"),
            "recordVersions": one("SELECT COUNT(*) FROM record_versions"),
            "preservedAfterTeamsDropped": preserved,
        },
        "range": {
            "oldest": iso(oldest),
            "newest": iso(newest),
        },
        "lastChecked": if checked > 0 { iso(Some(checked)) } else { None },
        "lastSync": run_json(last.as_ref()),
        "lastSuccessfulSync": run_json(ok.as_ref()),
        "backups": {
            "count": bks.len(),
            "latest": bks.last().map(|p| p.to_string_lossy().into_owned()),
        },
    }))
}

pub fn no_archive_message(p: &std::path::Path) -> String {
    format!("No archive yet at {}. Run `teams sync` first.", p.display())
}

pub fn open_archive_readonly() -> Result<Connection, archive::ArchiveError> {
    let p = paths::archive_path();
    if !p.is_file() {
        return Err(archive::ArchiveError(no_archive_message(&p)));
    }
    let conn = archive::connect_readonly(Some(&p))?;
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if ver == archive::SCHEMA_VERSION {
        return Ok(conn);
    }
    drop(conn);
    archive::connect(Some(&p))
}

pub fn require_data(conn: &Connection) -> Result<(), ApiError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sync_runs WHERE status IN ('ok', 'warning')",
        [],
        |r| r.get(0),
    )?;
    if n == 0 {
        return Err(ApiError::with_status(
            "empty_archive",
            "The archive is empty. Run `teams sync` first.",
            409,
        ));
    }
    Ok(())
}

pub fn as_int(v: Option<&str>, default: usize, name: &str) -> Result<usize, ApiError> {
    let Some(s) = v.filter(|x| !x.is_empty()) else {
        return Ok(default);
    };
    let n: i64 = s
        .parse()
        .map_err(|_| ApiError::bad("bad_param", format!("{name} must be a whole number")))?;
    if n < 0 {
        return Err(ApiError::bad("bad_param", format!("{name} must be 0 or more, not {n}")));
    }
    usize::try_from(n).map_err(|_| ApiError::bad("bad_param", format!("{name} is too large")))
}

pub fn as_bool(v: Option<&str>) -> bool {
    matches!(
        v.unwrap_or("").trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
