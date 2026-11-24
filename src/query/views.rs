use crate::identity::{self, clean_id, ms, same_person};
use crate::ingest::extract::render_text;
use crate::query::api::{self, ApiError};
use crate::storage::{archive, db};
use crate::timeutil::{iso, to_ms};
use rusqlite::{params_from_iter, types::Value as SqlVal, Connection};
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

pub const MENTIONS: &str = "streams-replychain-manager/mentions";
pub const FEED: &str = "activity-manager/feed-items";
pub const CALLS: &str = "call-history-manager/call-history";
pub const CALENDAR: &str = "calendar/calendar";
pub const SAVED: &str = "streams-replychain-manager/saved";
pub const PINNED: &[&str] = &[
    "channel-info-pane-manager/pinnedchannelmessages",
    "chat-info-pane-manager/pinned-messages-store",
];
pub const HORIZON_HISTORY: &str = "consumption-horizon-manager/consumption-horizon-history";
const PREVIEW_CHARS: usize = 300;

type LatestRecord = (String, String, Value);

fn latest(conn: &Connection, sources: &[&str]) -> Result<Vec<LatestRecord>, ApiError> {
    let mut out = Vec::new();
    if sources.is_empty() {
        return Ok(out);
    }
    let marks = vec!["?"; sources.len()].join(",");
    let sql = format!(
        "SELECT source, record_key, data FROM record_versions WHERE id IN (\
             SELECT MAX(id) FROM record_versions WHERE source IN ({marks}) GROUP BY source, record_key\
         ) ORDER BY id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params_from_iter(sources.iter().copied()))?;
    while let Some(r) = rows.next()? {
        let src: String = r.get(0)?;
        let rk: String = r.get(1)?;
        if let Ok(val) = archive::decode_record_ref(r.get_ref(2)?) {
            out.push((src, rk, val));
        }
    }
    Ok(out)
}

fn as_obj<'a>(v: Option<&'a Value>) -> Option<Cow<'a, Map<String, Value>>> {
    match v? {
        Value::Object(m) => Some(Cow::Borrowed(m)),
        Value::String(s) if s.starts_with('{') => match serde_json::from_str::<Value>(s).ok()? {
            Value::Object(m) => Some(Cow::Owned(m)),
            _ => None,
        },
        _ => None,
    }
}

fn as_arr<'a>(v: Option<&'a Value>) -> Cow<'a, [Value]> {
    match v {
        Some(Value::Array(a)) => Cow::Borrowed(a.as_slice()),
        Some(Value::String(s)) if s.starts_with('[') => match serde_json::from_str::<Value>(s).ok() {
            Some(Value::Array(a)) => Cow::Owned(a),
            _ => Cow::Borrowed(&[]),
        },
        _ => Cow::Borrowed(&[]),
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(String::from)
}

fn in_window(t: Option<i64>, lo: Option<i64>, hi: Option<i64>) -> bool {
    if lo.is_none() && hi.is_none() {
        return true;
    }
    let Some(t) = t else { return false };
    lo.map(|l| t >= l).unwrap_or(true) && hi.map(|h| t < h).unwrap_or(true)
}

fn preview(text: Option<&str>) -> String {
    let collapsed = text.unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= PREVIEW_CHARS {
        collapsed
    } else {
        let mut s: String = collapsed.chars().take(PREVIEW_CHARS - 1).collect();
        s.push('…');
        s
    }
}

struct Lookup {
    people: HashMap<String, (Option<String>, Option<String>)>,
    by_oid: HashMap<String, (Option<String>, Option<String>)>,
    chats: HashMap<String, (Option<String>, String)>,
}

impl Lookup {
    fn new(conn: &Connection) -> Result<Self, ApiError> {
        let mut people = HashMap::new();
        let mut by_oid = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT mri, name, email FROM people")?;
            for r in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.filter(|s| !s.is_empty()),
                    r.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty()),
                ))
            })? {
                let (mri, name, email) = r?;
                let o = identity::oid(&mri);
                if !o.is_empty() {
                    by_oid.insert(o, (name.clone(), email.clone()));
                }
                people.insert(mri, (name, email));
            }
        }
        let mut chats = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT id, name, kind FROM conversations")?;
            for r in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (id, name, kind) = r?;
                chats.insert(id, (name, kind));
            }
        }
        Ok(Self { people, by_oid, chats })
    }

    fn person(&self, mri: Option<&str>) -> Option<&(Option<String>, Option<String>)> {
        let m = mri?;
        self.people.get(m).or_else(|| self.by_oid.get(&identity::oid(m)))
    }

    fn name(&self, mri: Option<&str>, fallback: Option<&str>) -> Option<String> {
        if let Some(fb) = fallback.filter(|s| !s.is_empty()) {
            return Some(fb.to_string());
        }
        if let Some((Some(n), _)) = self.person(mri) {
            return Some(n.clone());
        }
        mri.map(String::from)
    }

    fn email(&self, mri: Option<&str>) -> Option<&str> {
        self.person(mri).and_then(|(_, e)| e.as_deref())
    }

    fn chat_name(&self, cid: Option<&str>) -> Option<String> {
        self.chats.get(cid?).and_then(|(n, _)| n.clone())
    }

    fn chat_kind(&self, cid: Option<&str>) -> Option<String> {
        self.chats.get(cid?).map(|(_, k)| k.clone())
    }

    fn name_of(&self, mri: &str) -> String {
        self.name(Some(mri), None).unwrap_or_else(|| mri.to_string())
    }
}

struct ArchivedMsg {
    thread_id: Option<String>,
    arrived_at: Option<i64>,
    deleted_at: Option<i64>,
    sender_mri: Option<String>,
    sender: Option<String>,
    text: Option<String>,
    subject: Option<String>,
}

fn fetch_archived_msgs(
    conn: &Connection,
    keys: &[(String, String)],
) -> Result<HashMap<(String, String), ArchivedMsg>, ApiError> {
    let mut out = HashMap::new();
    for chunk in keys.chunks(300) {
        let vals = vec!["(?, ?)"; chunk.len()].join(",");
        let sql = format!(
            "SELECT m.conversation_id, m.message_id, m.thread_id, m.arrived_at, m.deleted_at, m.sender_mri, \
                    COALESCE(NULLIF(m.sender_name, ''), p.name, m.sender_mri), m.text, m.subject \
             FROM messages m LEFT JOIN people p ON p.mri = m.sender_mri \
             WHERE (m.conversation_id, m.message_id) IN (VALUES {vals})"
        );
        let mut params = Vec::with_capacity(chunk.len() * 2);
        for (c, m) in chunk {
            params.push(SqlVal::Text(c.clone()));
            params.push(SqlVal::Text(m.clone()));
        }
        let mut stmt = conn.prepare(&sql)?;
        for r in stmt.query_map(params_from_iter(params), |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                ArchivedMsg {
                    thread_id: r.get(2)?,
                    arrived_at: r.get(3)?,
                    deleted_at: r.get(4)?,
                    sender_mri: r.get(5)?,
                    sender: r.get(6)?,
                    text: r.get(7)?,
                    subject: r.get(8)?,
                },
            ))
        })? {
            let (k, v) = r?;
            out.insert(k, v);
        }
    }
    Ok(out)
}

fn render_raw_msg(m: &Map<String, Value>, look: &Lookup) -> String {
    let mtype = non_empty_str(m.get("messageType")).unwrap_or_else(|| "RichText/Html".to_string());
    let content = non_empty_str(m.get("content")).unwrap_or_default();
    let owned_props;
    let props_ref: &Value = match m.get("properties") {
        Some(v @ Value::Object(_)) => v,
        other => {
            owned_props = as_obj(other)
                .map(|c| Value::Object(c.into_owned()))
                .unwrap_or_else(|| json!({}));
            &owned_props
        }
    };
    render_text(&mtype, &content, props_ref, &|mri| look.name_of(mri)).1
}

fn page_items(mut items: Vec<Value>, key: &str, limit: usize) -> (Vec<Value>, usize) {
    items.sort_by(|a, b| {
        let ta = a.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get(key).and_then(|v| v.as_i64()).unwrap_or(0);
        tb.cmp(&ta)
    });
    let total = items.len();
    if limit > 0 {
        items.truncate(limit);
    }
    (items, total)
}

#[derive(Default)]
struct MentionDraft {
    chat_id: String,
    message_id: String,
    thread_id: Option<String>,
    time_ms: Option<i64>,
    sender_id: Option<String>,
    sender: Option<String>,
    text: Option<String>,
    is_read: Option<bool>,
}

#[allow(clippy::too_many_arguments)]
fn mention_items(
    conn: &Connection,
    me: Option<&str>,
    look: &Lookup,
    feed: &[LatestRecord],
    lo: Option<i64>,
    hi: Option<i64>,
    chat_id: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let mut items: HashMap<(String, String), MentionDraft> = HashMap::new();

    let mut add = |cid: Option<String>,
                   mid: Option<String>,
                   thread_id: Option<String>,
                   time_ms: Option<i64>,
                   sender_id: Option<String>,
                   sender: Option<String>,
                   text: Option<String>,
                   is_read: Option<bool>| {
        let (Some(c), Some(m)) = (cid, mid) else { return };
        if c.is_empty() || m.is_empty() {
            return;
        }
        let e = items.entry((c.clone(), m.clone())).or_insert_with(|| MentionDraft {
            chat_id: c,
            message_id: m,
            ..Default::default()
        });
        if e.thread_id.is_none() {
            e.thread_id = thread_id;
        }
        if e.time_ms.is_none() {
            e.time_ms = time_ms;
        }
        if e.sender_id.is_none() {
            e.sender_id = sender_id;
        }
        if e.sender.is_none() {
            e.sender = sender;
        }
        if e.text.is_none() {
            e.text = text;
        }
        if e.is_read.is_none() {
            e.is_read = is_read;
        }
    };

    for (_, _, data) in latest(conn, &[MENTIONS])? {
        let Some(chain) = as_obj(Some(&data)) else { continue };
        let Some(mm) = chain.get("messageMap").and_then(|v| v.as_object()) else {
            continue;
        };
        for mv in mm.values() {
            let Some(m) = as_obj(Some(mv)) else { continue };
            let props = as_obj(m.get("properties")).unwrap_or_default();
            let a = as_obj(props.get("activity")).unwrap_or_default();
            if let (Some(me_s), Some(target)) = (me, non_empty_str(a.get("targetUserId"))) {
                if !same_person(Some(&target), Some(me_s)) {
                    continue;
                }
            }
            let is_read = match props.get("isread") {
                Some(Value::Bool(b)) => Some(*b),
                Some(Value::String(s)) if s == "true" => Some(true),
                Some(Value::String(s)) if s == "false" => Some(false),
                _ => None,
            };
            let t = a
                .get("activityTimestamp")
                .and_then(to_ms)
                .or_else(|| m.get("originalArrivalTime").and_then(ms))
                .or_else(|| chain.get("originalArrivalTime").and_then(ms));
            let rendered = render_raw_msg(&m, look);
            let txt = if !rendered.is_empty() {
                Some(rendered)
            } else {
                non_empty_str(a.get("messagePreview"))
            };
            add(
                non_empty_str(a.get("sourceThreadId")),
                a.get("sourceMessageId").and_then(clean_id),
                a.get("sourceReplyChainId").and_then(clean_id),
                t,
                non_empty_str(a.get("sourceUserId")),
                non_empty_str(a.get("sourceUserImDisplayName")),
                txt,
                is_read,
            );
        }
    }

    let mut read_map: HashMap<(String, String), bool> = HashMap::new();
    for (_, _, data) in feed {
        let Some(f) = as_obj(Some(data)) else { continue };
        if f.get("activityType").and_then(|v| v.as_str()) != Some("mention") {
            continue;
        }
        let cid = non_empty_str(f.get("sourceThreadId"));
        let mid = f
            .get("sourceMessageId")
            .and_then(clean_id)
            .or_else(|| f.get("messageId").and_then(clean_id));
        if let (Some(b), Some(ref c), Some(ref m)) = (f.get("isRead").and_then(|v| v.as_bool()), &cid, &mid) {
            read_map.insert((c.clone(), m.clone()), b);
        }
        add(
            cid,
            mid,
            f.get("sourceReplyChainId").and_then(clean_id),
            f.get("timestamp").and_then(to_ms),
            non_empty_str(f.get("sourceUserId")),
            non_empty_str(f.get("sourceUserImDisplayName")),
            non_empty_str(f.get("messagePreview")),
            None,
        );
    }

    if let Some(me_s) = me {
        let me_oid = identity::oid(me_s);
        if db::table_exists(conn, "message_mentions") && !me_oid.is_empty() {
            let mut sql = String::from(
                "SELECT mm.conversation_id, mm.message_id, m.sender_mri \
                 FROM message_mentions mm \
                 JOIN messages m ON m.conversation_id = mm.conversation_id AND m.message_id = mm.message_id \
                 WHERE mm.oid = ? AND m.kind = 'message'",
            );
            let mut args = vec![SqlVal::Text(me_oid)];
            if let Some(l) = lo {
                sql.push_str(" AND m.arrived_at >= ?");
                args.push(SqlVal::Integer(l));
            }
            if let Some(h) = hi {
                sql.push_str(" AND m.arrived_at < ?");
                args.push(SqlVal::Integer(h));
            }
            if let Some(c) = chat_id {
                sql.push_str(" AND mm.conversation_id = ?");
                args.push(SqlVal::Text(c.to_string()));
            }
            let mut stmt = conn.prepare(&sql)?;
            for r in stmt.query_map(params_from_iter(args), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })? {
                let (c, m, sm) = r?;
                if same_person(sm.as_deref(), Some(me_s)) {
                    continue;
                }
                add(Some(c), Some(m), None, None, None, None, None, None);
            }
        }
    }

    let keys: Vec<(String, String)> = items.keys().cloned().collect();
    let archived = fetch_archived_msgs(conn, &keys)?;
    let mut out = Vec::with_capacity(items.len());
    for (key, mut it) in items {
        let row = archived.get(&key);
        if let Some(&b) = read_map.get(&key) {
            it.is_read = Some(b);
        }
        let sender_id = row.and_then(|r| r.sender_mri.clone()).or(it.sender_id);
        let t = row.and_then(|r| r.arrived_at).or(it.time_ms);
        let sender = row
            .and_then(|r| r.sender.clone())
            .or_else(|| look.name(sender_id.as_deref(), it.sender.as_deref()))
            .unwrap_or_default();
        let text = row
            .and_then(|r| r.text.as_deref().filter(|s| !s.is_empty()).map(String::from))
            .or(it.text)
            .unwrap_or_default();
        out.push(json!({
            "messageId": it.message_id,
            "chatId": it.chat_id,
            "chatName": look.chat_name(Some(&it.chat_id)),
            "chatKind": look.chat_kind(Some(&it.chat_id)),
            "threadId": row.and_then(|r| r.thread_id.clone()).or(it.thread_id),
            "time": iso(t),
            "timeMs": t,
            "sender": sender,
            "senderId": sender_id,
            "text": text,
            "isRead": it.is_read,
        }));
    }
    Ok(out)
}

pub fn mentions(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    chat: Option<&str>,
    unread_only: bool,
    limit: usize,
) -> Result<Value, ApiError> {
    let lo = api::parse_when_or_error(since, false)?;
    let hi = api::parse_when_or_error(until, true)?;
    let (chat_json, chat_id) = if let Some(c) = chat.filter(|s| !s.is_empty()) {
        let (r, _) = api::resolve_chat(conn, c)?;
        (r.to_json(), Some(r.id))
    } else {
        (Value::Null, None)
    };
    let me = archive::me_mri(conn);
    let look = Lookup::new(conn)?;
    let feed = latest(conn, &[FEED])?;
    let items: Vec<Value> = mention_items(conn, me.as_deref(), &look, &feed, lo, hi, chat_id.as_deref())?
        .into_iter()
        .filter(|m| {
            let t = m.get("timeMs").and_then(|v| v.as_i64());
            in_window(t, lo, hi)
                && chat_id
                    .as_deref()
                    .map(|cid| m.get("chatId").and_then(|v| v.as_str()) == Some(cid))
                    .unwrap_or(true)
                && (!unread_only || m.get("isRead") == Some(&Value::Bool(false)))
        })
        .collect();
    let (shown, total) = page_items(items, "timeMs", limit);
    Ok(json!({
        "me": me,
        "chat": chat_json,
        "mentions": shown,
        "matched": total,
        "truncated": total > shown.len(),
    }))
}

#[derive(Clone, Default)]
pub struct ConvReadState {
    pub horizon: i64,
    pub is_read: Option<bool>,
    pub last_from_me: Option<bool>,
}

pub fn read_state(conn: &Connection) -> Result<HashMap<String, ConvReadState>, ApiError> {
    let mut out = HashMap::new();
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    let has_read_cols = if ver >= 6 {
        true
    } else {
        let mut found = false;
        let mut stmt = conn.prepare("PRAGMA table_info(conversations)")?;
        for c in stmt.query_map([], |r| r.get::<_, String>(1))? {
            if c? == "read_horizon" {
                found = true;
            }
        }
        found
    };
    if has_read_cols {
        let mut stmt =
            conn.prepare("SELECT id, read_horizon, is_read, last_from_me FROM conversations WHERE kind != 'system'")?;
        for r in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.map(|v| v != 0),
                r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
            ))
        })? {
            let (id, h, ir, lfm) = r?;
            out.insert(
                id,
                ConvReadState {
                    horizon: h,
                    is_read: ir,
                    last_from_me: lfm,
                },
            );
        }
    } else {
        let mut stmt = conn.prepare("SELECT id, raw FROM conversations WHERE kind != 'system'")?;
        for r in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))? {
            let (id, raw_s) = r?;
            let raw_v = raw_s
                .as_deref()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            let (h, ir, lfm) = archive::extract_conv_read_state(&raw_v);
            out.insert(
                id,
                ConvReadState {
                    horizon: h,
                    is_read: ir.map(|v| v != 0),
                    last_from_me: lfm.map(|v| v != 0),
                },
            );
        }
    }

    let mut best: HashMap<String, (i64, Option<bool>)> = HashMap::new();
    for (_, key, data) in latest(conn, &[HORIZON_HISTORY])? {
        let Some(d) = as_obj(Some(&data)) else { continue };
        let cid = non_empty_str(d.get("conversationId")).unwrap_or(key);
        for h in as_arr(d.get("historyRecords")).iter() {
            let after = as_obj(h.get("afterUpdate")).unwrap_or_default();
            let t = archive::horizon_first(after.get("consumptionHorizon"));
            let cur_t = best.get(&cid).map(|x| x.0).unwrap_or(0);
            if t > 0 && t >= cur_t {
                best.insert(cid.clone(), (t, after.get("isRead").and_then(|v| v.as_bool())));
            }
        }
    }
    for (cid, (t, is_read)) in best {
        if let Some(st) = out.get_mut(&cid) {
            if t > st.horizon {
                st.horizon = t;
                if is_read.is_some() {
                    st.is_read = is_read;
                }
            } else if st.is_read.is_none() {
                st.is_read = is_read;
            }
        }
    }
    Ok(out)
}

fn me_variants(conn: &Connection, me: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(me_s) = me.filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out = vec![me_s.to_string()];
    if let Some(oid) = identity::valid_oid(me_s) {
        let mut stmt = conn.prepare("SELECT DISTINCT sender_mri FROM messages WHERE sender_mri LIKE ? ESCAPE '\\'")?;
        for r in stmt.query_map([format!("%:{}", db::like_escape(&oid))], |r| r.get::<_, String>(0))? {
            let m = r?;
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

struct LastMsg {
    message_id: String,
    arrived_at: i64,
    sender_mri: Option<String>,
    sender: Option<String>,
    text: Option<String>,
}

type InboxSummary = (HashMap<String, LastMsg>, HashMap<String, i64>, HashMap<String, i64>);

fn inbox_message_summary(
    conn: &Connection,
    me_mris: &[String],
    look: &Lookup,
    cids: Option<&[String]>,
) -> Result<InboxSummary, ApiError> {
    let mut max_any_pairs: Vec<(String, i64)> = Vec::new();
    let mut mine_at: HashMap<String, i64> = HashMap::new();
    let mut other_at: HashMap<String, i64> = HashMap::new();

    let mut scan = |scope_sql: &str, scope_args: &[String]| -> Result<(), ApiError> {
        let mut params: Vec<SqlVal> = Vec::new();
        let sql = if !me_mris.is_empty() {
            let qs = vec!["?"; me_mris.len()].join(",");
            for _ in 0..2 {
                params.extend(me_mris.iter().cloned().map(SqlVal::Text));
            }
            for a in scope_args {
                params.push(SqlVal::Text(a.clone()));
            }
            format!(
                "SELECT conversation_id, MAX(arrived_at), \
                        MAX(CASE WHEN sender_mri IN ({qs}) THEN arrived_at END), \
                        MAX(CASE WHEN sender_mri != '' AND sender_mri IS NOT NULL AND sender_mri NOT IN ({qs}) THEN arrived_at END) \
                 FROM messages WHERE kind = 'message' {scope_sql} GROUP BY conversation_id"
            )
        } else {
            for a in scope_args {
                params.push(SqlVal::Text(a.clone()));
            }
            format!(
                "SELECT conversation_id, MAX(arrived_at), NULL, \
                        MAX(CASE WHEN sender_mri != '' AND sender_mri IS NOT NULL THEN arrived_at END) \
                 FROM messages WHERE kind = 'message' {scope_sql} GROUP BY conversation_id"
            )
        };
        let mut stmt = conn.prepare(&sql)?;
        for r in stmt.query_map(params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })? {
            let (cid, t_any, t_mine, t_other) = r?;
            if let Some(t) = t_any {
                max_any_pairs.push((cid.clone(), t));
            }
            if let Some(t) = t_mine {
                mine_at.insert(cid.clone(), t);
            }
            if let Some(t) = t_other {
                other_at.insert(cid, t);
            }
        }
        Ok(())
    };

    match cids {
        None => scan("", &[])?,
        Some(ids) => {
            for chunk in ids.chunks(400) {
                let qs = vec!["?"; chunk.len()].join(",");
                scan(&format!("AND conversation_id IN ({qs})"), chunk)?;
            }
        }
    }

    let mut last = HashMap::new();
    for chunk in max_any_pairs.chunks(300) {
        let vals = vec!["(?, ?)"; chunk.len()].join(",");
        let mut params = Vec::with_capacity(chunk.len() * 2);
        for (c, t) in chunk {
            params.push(SqlVal::Text(c.clone()));
            params.push(SqlVal::Integer(*t));
        }
        let mut stmt = conn.prepare(&format!(
            "WITH targets(cid, t) AS (VALUES {vals}) \
             SELECT m.pk, m.conversation_id, m.message_id, m.arrived_at, m.sender_mri, m.sender_name, m.text \
             FROM targets tgt CROSS JOIN messages m ON m.conversation_id = tgt.cid AND m.kind = 'message' AND m.arrived_at = tgt.t \
             ORDER BY m.pk"
        ))?;
        for r in stmt.query_map(params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })? {
            let (cid, mid, arr, smri, sname, text) = r?;
            let sender = sname
                .filter(|s| !s.is_empty())
                .or_else(|| look.name(smri.as_deref(), None))
                .or_else(|| smri.clone());
            last.insert(
                cid,
                LastMsg {
                    message_id: mid,
                    arrived_at: arr,
                    sender_mri: smri,
                    sender,
                    text,
                },
            );
        }
    }
    Ok((last, mine_at, other_at))
}

fn unread_counts_batch(
    conn: &Connection,
    cutoffs: &[(String, i64)],
    me_mris: &[String],
) -> Result<HashMap<String, i64>, ApiError> {
    let mut counts = HashMap::new();
    let excl: Vec<String> = if me_mris.is_empty() {
        vec![String::new()]
    } else {
        me_mris.to_vec()
    };
    let excl_qs = vec!["?"; excl.len()].join(",");
    for chunk in cutoffs.chunks(300) {
        let vals = vec!["(?, ?)"; chunk.len()].join(",");
        let mut params = Vec::with_capacity(chunk.len() * 2 + excl.len());
        for (c, u) in chunk {
            params.push(SqlVal::Text(c.clone()));
            params.push(SqlVal::Integer(*u));
        }
        params.extend(excl.iter().cloned().map(SqlVal::Text));
        let mut stmt = conn.prepare(&format!(
            "WITH cutoffs(cid, upto) AS (VALUES {vals}) \
             SELECT m.conversation_id, COUNT(*) \
             FROM cutoffs c CROSS JOIN messages m ON m.conversation_id = c.cid \
             WHERE m.kind = 'message' AND m.arrived_at > c.upto \
               AND m.sender_mri != '' AND m.sender_mri IS NOT NULL AND m.sender_mri NOT IN ({excl_qs}) \
             GROUP BY m.conversation_id"
        ))?;
        for r in stmt.query_map(params_from_iter(params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (cid, n) = r?;
            counts.insert(cid, n);
        }
    }
    Ok(counts)
}

pub fn inbox(conn: &Connection, since: Option<&str>, limit: usize) -> Result<Value, ApiError> {
    let lo = api::parse_when_or_error(since, false)?;
    let me = archive::me_mri(conn);
    let look = Lookup::new(conn)?;
    let states = read_state(conn)?;

    let mut chats: HashMap<String, (Option<String>, String, i64)> = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, name, kind, COALESCE(last_message_at, 0) FROM conversations WHERE kind != 'system'")?;
        for r in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })? {
            let (id, name, kind, lma) = r?;
            chats.insert(id, (name, kind, lma));
        }
    }

    let active_cids: Option<Vec<String>> = if let Some(l) = lo {
        let mut stmt = conn.prepare(
            "SELECT id FROM conversations WHERE kind != 'system' AND (last_message_at >= ? OR id IN (\
                 SELECT DISTINCT conversation_id FROM messages WHERE kind = 'message' AND arrived_at >= ?))",
        )?;
        let v: Vec<String> = stmt.query_map([l, l], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Some(v)
    } else {
        None
    };

    let me_mris = me_variants(conn, me.as_deref())?;
    let (last, mine_at, other_at) = inbox_message_summary(conn, &me_mris, &look, active_cids.as_deref())?;

    let mut unread_cands: Vec<(String, i64, i64)> = Vec::new();
    for (cid, (_, _, lma)) in &chats {
        let st = states.get(cid).cloned().unwrap_or_default();
        let lm = last.get(cid);
        let activity = (*lma).max(lm.map(|x| x.arrived_at).unwrap_or(0));
        if let Some(l) = lo {
            if activity < l {
                continue;
            }
        }
        let m_at = mine_at.get(cid).copied().unwrap_or(0);
        let upto = st.horizon.max(m_at);
        let last_from_me = st.last_from_me.unwrap_or_else(|| {
            lm.map(|x| same_person(x.sender_mri.as_deref(), me.as_deref()))
                .unwrap_or(false)
        });
        let o_at = other_at.get(cid).copied().unwrap_or(0);
        let by_flag = st.is_read == Some(false) && !last_from_me;
        let by_horizon = st.horizon > 0 && o_at > upto;
        if by_flag || by_horizon {
            unread_cands.push((cid.clone(), activity, upto));
        }
    }

    let cutoffs: Vec<(String, i64)> = unread_cands.iter().map(|(c, _, u)| (c.clone(), *u)).collect();
    let ucounts = unread_counts_batch(conn, &cutoffs, &me_mris)?;
    let mut unread_chats: Vec<Value> = unread_cands
        .into_iter()
        .map(|(cid, activity, _)| {
            let (cname, ckind, _) = &chats[&cid];
            let st = states.get(&cid).cloned().unwrap_or_default();
            let lm = last.get(&cid);
            let cnt = ucounts.get(&cid).copied().unwrap_or(0).max(1);
            json!({
                "chatId": cid,
                "chatName": cname,
                "kind": ckind,
                "unreadCount": cnt,
                "lastMessageId": lm.map(|x| &x.message_id),
                "lastMessageAt": iso(if activity != 0 { Some(activity) } else { None }),
                "lastMessageMs": if activity != 0 { Some(activity) } else { None },
                "lastSender": lm.and_then(|x| x.sender.as_deref()),
                "lastText": lm.map(|x| preview(x.text.as_deref())).unwrap_or_default(),
                "readUpTo": iso(if st.horizon > 0 { Some(st.horizon) } else { None }),
            })
        })
        .collect();
    unread_chats.sort_by(|a, b| {
        b.get("lastMessageMs")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .cmp(&a.get("lastMessageMs").and_then(|v| v.as_i64()).unwrap_or(0))
    });
    let unread_ids: HashSet<String> = unread_chats
        .iter()
        .filter_map(|u| u.get("chatId").and_then(|v| v.as_str()).map(String::from))
        .collect();

    let mut awaiting: Vec<Value> = Vec::new();
    if let Some(ref me_s) = me {
        for (cid, lm) in &last {
            let Some((cname, ckind, _)) = chats.get(cid) else {
                continue;
            };
            if let Some(l) = lo {
                if lm.arrived_at < l {
                    continue;
                }
            }
            if lm.sender_mri.as_deref().unwrap_or("").is_empty() || same_person(lm.sender_mri.as_deref(), Some(me_s)) {
                continue;
            }
            awaiting.push(json!({
                "chatId": cid,
                "chatName": cname,
                "kind": ckind,
                "lastMessageId": lm.message_id,
                "lastMessageAt": iso(Some(lm.arrived_at)),
                "lastMessageMs": lm.arrived_at,
                "lastSender": lm.sender,
                "lastSenderId": lm.sender_mri,
                "lastText": preview(lm.text.as_deref()),
                "unread": unread_ids.contains(cid),
            }));
        }
        awaiting.sort_by(|a, b| {
            b.get("lastMessageMs")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .cmp(&a.get("lastMessageMs").and_then(|v| v.as_i64()).unwrap_or(0))
        });
    }

    let feed = latest(conn, &[FEED])?;
    let mut all_mentions: Vec<Value> = mention_items(conn, me.as_deref(), &look, &feed, lo, None, None)?
        .into_iter()
        .filter(|m| in_window(m.get("timeMs").and_then(|v| v.as_i64()), lo, None))
        .collect();
    all_mentions.sort_by(|a, b| {
        let ua = a.get("isRead") == Some(&Value::Bool(false));
        let ub = b.get("isRead") == Some(&Value::Bool(false));
        let ta = a.get("timeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let tb = b.get("timeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        ub.cmp(&ua).then_with(|| tb.cmp(&ta))
    });
    let total_mentions = all_mentions.len();

    let mut unread_acts: Vec<Value> = Vec::new();
    for (_, key, data) in &feed {
        let Some(f) = as_obj(Some(data)) else { continue };
        let t = f.get("timestamp").and_then(to_ms);
        if f.get("isRead") != Some(&Value::Bool(false)) || (lo.is_some() && t.unwrap_or(0) < lo.unwrap()) {
            continue;
        }
        let cid = non_empty_str(f.get("sourceThreadId"));
        let pv = non_empty_str(f.get("messagePreview")).map(|s| preview(Some(&s)));
        unread_acts.push(json!({
            "id": non_empty_str(f.get("activityId")).unwrap_or_else(|| key.clone()),
            "activityType": non_empty_str(f.get("activityType")),
            "activitySubtype": non_empty_str(f.get("activitySubtype")),
            "chatId": cid,
            "chatName": look.chat_name(cid.as_deref()),
            "messageId": f.get("sourceMessageId").and_then(clean_id).or_else(|| f.get("messageId").and_then(clean_id)),
            "sender": look.name(non_empty_str(f.get("sourceUserId")).as_deref(), non_empty_str(f.get("sourceUserImDisplayName")).as_deref()),
            "preview": pv.filter(|s| !s.is_empty()),
            "time": iso(t),
            "timeMs": t,
        }));
    }
    unread_acts.sort_by(|a, b| {
        b.get("timeMs")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .cmp(&a.get("timeMs").and_then(|v| v.as_i64()).unwrap_or(0))
    });

    let c_unread = unread_chats.len();
    let c_await = awaiting.len();
    let c_acts = unread_acts.len();
    if limit > 0 {
        unread_chats.truncate(limit);
        awaiting.truncate(limit);
        all_mentions.truncate(limit);
        unread_acts.truncate(limit);
    }
    let me_json = me
        .as_ref()
        .map(|m| json!({"id": m, "name": look.person(Some(m)).and_then(|(n, _)| n.clone())}));
    Ok(json!({
        "me": me_json,
        "since": since,
        "unreadChats": unread_chats,
        "awaitingReply": awaiting,
        "mentions": all_mentions,
        "unreadActivities": unread_acts,
        "counts": {
            "unreadChats": c_unread,
            "awaitingReply": c_await,
            "mentions": total_mentions,
            "unreadActivities": c_acts,
        },
    }))
}

fn participant_json(v: Option<&Value>, look: &Lookup) -> Option<(Option<String>, Option<String>)> {
    let p = as_obj(v)?;
    let mri = non_empty_str(p.get("id")).or_else(|| non_empty_str(p.get("mri")));
    let dn = non_empty_str(p.get("displayName"));
    if mri.is_none() && dn.is_none() {
        return None;
    }
    let name = look.name(mri.as_deref(), dn.as_deref());
    Some((mri, name))
}

pub fn calls(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    person: Option<&str>,
    missed_only: bool,
    limit: usize,
) -> Result<Value, ApiError> {
    let lo = api::parse_when_or_error(since, false)?;
    let hi = api::parse_when_or_error(until, true)?;
    let look = Lookup::new(conn)?;
    let needle = person.unwrap_or("").trim().to_ascii_lowercase();
    let mut out = Vec::new();
    for (_, key, data) in latest(conn, &[CALLS])? {
        let Some(d) = as_obj(Some(&data)) else { continue };
        let start = d
            .get("startTime")
            .and_then(to_ms)
            .or_else(|| d.get("originalArrivalTime").and_then(to_ms));
        if !in_window(start, lo, hi) {
            continue;
        }
        let state = non_empty_str(d.get("callState"));
        if missed_only && !state.as_deref().unwrap_or("").eq_ignore_ascii_case("missed") {
            continue;
        }
        let orig = participant_json(d.get("originatorParticipant"), &look);
        let targ = participant_json(d.get("targetParticipant"), &look);
        let mut participants = Vec::new();
        let mut seen = HashSet::new();
        let raw_parts = as_arr(d.get("participantList"));
        for rp in raw_parts
            .iter()
            .chain(d.get("originatorParticipant"))
            .chain(d.get("targetParticipant"))
        {
            if let Some((pid, pname)) = participant_json(Some(rp), &look) {
                let k = pid.clone().or_else(|| pname.clone()).unwrap_or_default();
                if seen.insert(k) {
                    participants.push((pid, pname));
                }
            }
        }
        if !needle.is_empty() {
            let matches = participants.iter().any(|(pid, pname)| {
                pid.as_deref().unwrap_or("").to_ascii_lowercase().contains(&needle)
                    || pname.as_deref().unwrap_or("").to_ascii_lowercase().contains(&needle)
                    || look
                        .email(pid.as_deref())
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .contains(&needle)
            });
            if !matches {
                continue;
            }
        }
        let end = d.get("endTime").and_then(to_ms);
        let mut dur_ms = d
            .get("durationInMs")
            .and_then(|v| {
                if v.is_boolean() {
                    None
                } else {
                    v.as_f64().map(|f| f as i64)
                }
            })
            .map(|m| m.max(0));
        if dur_ms.is_none() {
            if let (Some(e), Some(c)) = (end, d.get("connectTime").and_then(to_ms)) {
                dur_ms = Some((e - c).max(0));
            }
        }
        let chat_id = non_empty_str(d.get("groupChatThreadId"));
        out.push(json!({
            "id": non_empty_str(d.get("callId")).or_else(|| non_empty_str(d.get("id"))).unwrap_or(key),
            "time": iso(start),
            "timeMs": start,
            "endTime": iso(end),
            "durationSeconds": dur_ms.map(|m| ((m as f64) / 1000.0).round() as i64),
            "durationMs": dur_ms,
            "direction": non_empty_str(d.get("callDirection")),
            "callType": non_empty_str(d.get("callType")),
            "state": state,
            "originator": orig.map(|(i, n)| json!({"id": i, "name": n})),
            "target": targ.map(|(i, n)| json!({"id": i, "name": n})),
            "participants": participants.into_iter().map(|(i, n)| json!({"id": i, "name": n})).collect::<Vec<_>>(),
            "chatId": chat_id,
            "chatName": look.chat_name(chat_id.as_deref()),
        }));
    }
    let (shown, total) = page_items(out, "timeMs", limit);
    Ok(json!({"calls": shown, "matched": total, "truncated": total > shown.len()}))
}

pub fn meetings(
    conn: &Connection,
    since: Option<&str>,
    until: Option<&str>,
    query: Option<&str>,
    limit: usize,
) -> Result<Value, ApiError> {
    let lo = api::parse_when_or_error(since, false)?;
    let hi = api::parse_when_or_error(until, true)?;
    let needle = query.unwrap_or("").trim().to_ascii_lowercase();
    let mut out = Vec::new();
    for (_, key, data) in latest(conn, &[CALENDAR])? {
        let Some(d) = as_obj(Some(&data)) else { continue };
        let start = d.get("startTime").and_then(to_ms);
        let end = d.get("endTime").and_then(to_ms);
        if let Some(l) = lo {
            if end.or(start).unwrap_or(0) < l {
                continue;
            }
        }
        if let Some(h) = hi {
            if start.map(|s| s >= h).unwrap_or(true) {
                continue;
            }
        }
        let mut attendees = Vec::new();
        for av in as_arr(d.get("attendees")).iter() {
            let Some(a) = as_obj(Some(av)) else { continue };
            let resp = as_obj(a.get("status"))
                .and_then(|s| non_empty_str(s.get("response")))
                .or_else(|| non_empty_str(a.get("status")));
            attendees.push(json!({
                "name": non_empty_str(a.get("name")),
                "email": non_empty_str(a.get("address")),
                "response": resp,
                "type": non_empty_str(a.get("type")),
            }));
        }
        let subj = non_empty_str(d.get("subject"));
        let org = non_empty_str(d.get("organizerName"));
        let org_email = non_empty_str(d.get("organizerAddress"));
        let loc = non_empty_str(d.get("location"));
        if !needle.is_empty() {
            let mut hay: Vec<&str> = Vec::new();
            for s in [&subj, &org, &org_email, &loc].into_iter().flatten() {
                hay.push(s);
            }
            for a in &attendees {
                if let Some(s) = a.get("name").and_then(|v| v.as_str()) {
                    hay.push(s);
                }
                if let Some(s) = a.get("email").and_then(|v| v.as_str()) {
                    hay.push(s);
                }
            }
            if !hay.iter().any(|h| h.to_ascii_lowercase().contains(&needle)) {
                continue;
            }
        }
        let chat_id = as_obj(d.get("skypeTeamsDataObject"))
            .or_else(|| as_obj(d.get("skypeTeamsData")))
            .and_then(|o| non_empty_str(o.get("cid")));
        let pv = non_empty_str(d.get("bodyPreview")).map(|s| preview(Some(&s)));
        out.push(json!({
            "id": non_empty_str(d.get("objectId")).unwrap_or(key),
            "subject": subj,
            "start": iso(start),
            "startMs": start,
            "end": iso(end),
            "endMs": end,
            "organizer": org.or_else(|| org_email.clone()),
            "organizerEmail": org_email,
            "location": loc,
            "isOnlineMeeting": d.get("isOnlineMeeting").and_then(|v| v.as_bool()).unwrap_or(false),
            "joinUrl": non_empty_str(d.get("shortOnlineMeetingJoinUrl")),
            "myResponse": non_empty_str(d.get("myResponseType")),
            "showAs": non_empty_str(d.get("showAs")),
            "chatId": chat_id,
            "attendees": attendees,
            "preview": pv.filter(|s| !s.is_empty()),
        }));
    }
    let (shown, total) = page_items(out, "startMs", limit);
    Ok(json!({"meetings": shown, "matched": total, "truncated": total > shown.len()}))
}

pub fn saved(conn: &Connection, include_unsaved: bool, limit: usize) -> Result<Value, ApiError> {
    let look = Lookup::new(conn)?;
    struct SavedDraft {
        at: Option<i64>,
        thread: Option<String>,
        unsaved: bool,
        sender: Option<String>,
        sender_id: Option<String>,
        text: Option<String>,
    }
    let mut found: HashMap<(String, String), SavedDraft> = HashMap::new();
    for (_, _, data) in latest(conn, &[SAVED])? {
        let Some(chain) = as_obj(Some(&data)) else { continue };
        let Some(mm) = chain.get("messageMap").and_then(|v| v.as_object()) else {
            continue;
        };
        for mv in mm.values() {
            let Some(m) = as_obj(Some(mv)) else { continue };
            let props = as_obj(m.get("properties")).unwrap_or_default();
            let a = as_obj(props.get("activity")).unwrap_or_default();
            let (Some(cid), Some(mid)) = (
                non_empty_str(a.get("sourceThreadId")),
                a.get("sourceMessageId").and_then(clean_id),
            ) else {
                continue;
            };
            let at = m
                .get("originalArrivalTime")
                .and_then(ms)
                .or_else(|| a.get("activityTimestamp").and_then(to_ms))
                .or_else(|| chain.get("originalArrivalTime").and_then(ms));
            if let Some(cur) = found.get(&(cid.clone(), mid.clone())) {
                if cur.at.unwrap_or(0) > at.unwrap_or(0) {
                    continue;
                }
            }
            let rendered = render_raw_msg(&m, &look);
            let txt = if !rendered.is_empty() {
                Some(rendered)
            } else {
                non_empty_str(a.get("messagePreview"))
            };
            found.insert(
                (cid, mid),
                SavedDraft {
                    at,
                    thread: a.get("sourceReplyChainId").and_then(clean_id),
                    unsaved: a.get("activitySubtype").and_then(|v| v.as_str()) == Some("unSaved")
                        || a.get("activityReason").and_then(|v| v.as_str()) == Some("deleted"),
                    sender: non_empty_str(a.get("sourceUserImDisplayName")),
                    sender_id: non_empty_str(a.get("sourceUserId")),
                    text: txt,
                },
            );
        }
    }
    let keys: Vec<(String, String)> = found.keys().cloned().collect();
    let archived = fetch_archived_msgs(conn, &keys)?;
    let mut out = Vec::new();
    for ((cid, mid), f) in found {
        if f.unsaved && !include_unsaved {
            continue;
        }
        let row = archived.get(&(cid.clone(), mid.clone()));
        let sender_id = row.and_then(|r| r.sender_mri.clone()).or(f.sender_id);
        let t = row.and_then(|r| r.arrived_at);
        out.push(json!({
            "messageId": mid,
            "chatId": cid,
            "chatName": look.chat_name(Some(&cid)),
            "chatKind": look.chat_kind(Some(&cid)),
            "threadId": row.and_then(|r| r.thread_id.clone()).or(f.thread),
            "savedAt": iso(f.at),
            "savedAtMs": f.at,
            "time": iso(t),
            "timeMs": t,
            "sender": row.and_then(|r| r.sender.clone()).or_else(|| look.name(sender_id.as_deref(), f.sender.as_deref())),
            "senderId": sender_id,
            "text": row.and_then(|r| r.text.as_deref().filter(|s| !s.is_empty()).map(String::from)).or(f.text).unwrap_or_default(),
            "deleted": row.and_then(|r| r.deleted_at).map(|v| v != 0).unwrap_or(false),
            "unsaved": f.unsaved,
        }));
    }
    let (shown, total) = page_items(out, "savedAtMs", limit);
    Ok(json!({"saved": shown, "matched": total, "truncated": total > shown.len()}))
}

pub fn pinned(conn: &Connection, chat: Option<&str>, limit: usize) -> Result<Value, ApiError> {
    let (chat_json, chat_id) = if let Some(c) = chat.filter(|s| !s.is_empty()) {
        let (r, _) = api::resolve_chat(conn, c)?;
        (r.to_json(), Some(r.id))
    } else {
        (Value::Null, None)
    };
    let look = Lookup::new(conn)?;
    let mut found: HashMap<(String, String), (String, Map<String, Value>)> = HashMap::new();
    for (src, _, data) in latest(conn, PINNED)? {
        let Some(d) = as_obj(Some(&data)) else { continue };
        let mut records = Vec::new();
        if non_empty_str(d.get("conversationId")).is_some() && d.get("id").map(|v| !v.is_null()).unwrap_or(false) {
            records.push(d.into_owned());
        } else {
            for v in d.values() {
                for item in as_arr(Some(v)).iter() {
                    if let Some(x) = as_obj(Some(item)) {
                        if non_empty_str(x.get("conversationId")).is_some()
                            && x.get("id").map(|v| !v.is_null()).unwrap_or(false)
                        {
                            records.push(x.into_owned());
                        }
                    }
                }
            }
        }
        for m in records {
            if let (Some(cid), Some(mid)) = (non_empty_str(m.get("conversationId")), m.get("id").and_then(clean_id)) {
                if chat_id.as_deref().map(|c| c == cid).unwrap_or(true) {
                    found.insert((cid, mid), (src.clone(), m));
                }
            }
        }
    }
    let keys: Vec<(String, String)> = found.keys().cloned().collect();
    let archived = fetch_archived_msgs(conn, &keys)?;
    let mut out = Vec::new();
    for ((cid, mid), (src, m)) in found {
        let row = archived.get(&(cid.clone(), mid.clone()));
        let sender_id = row
            .and_then(|r| r.sender_mri.clone())
            .or_else(|| non_empty_str(m.get("creator")));
        let t = row
            .and_then(|r| r.arrived_at)
            .or_else(|| m.get("originalArrivalTime").and_then(ms));
        let props = as_obj(m.get("properties")).unwrap_or_default();
        out.push(json!({
            "messageId": mid,
            "chatId": cid,
            "chatName": look.chat_name(Some(&cid)),
            "chatKind": look.chat_kind(Some(&cid)),
            "threadId": row.and_then(|r| r.thread_id.clone()).or_else(|| m.get("parentMessageId").and_then(clean_id)),
            "time": iso(t),
            "timeMs": t,
            "sender": row.and_then(|r| r.sender.clone()).or_else(|| look.name(sender_id.as_deref(), non_empty_str(m.get("imDisplayName")).as_deref())),
            "senderId": sender_id,
            "subject": row.and_then(|r| r.subject.as_deref().filter(|s| !s.is_empty()).map(String::from)).or_else(|| non_empty_str(props.get("subject"))),
            "text": row.and_then(|r| r.text.as_deref().filter(|s| !s.is_empty()).map(String::from)).unwrap_or_else(|| render_raw_msg(&m, &look)),
            "source": src,
        }));
    }
    let (shown, total) = page_items(out, "timeMs", limit);
    Ok(json!({"chat": chat_json, "pinned": shown, "matched": total, "truncated": total > shown.len()}))
}
