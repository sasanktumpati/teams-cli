use crate::identity;
use crate::ingest::extract::{
    self, render_text, volatile_fields, Conversation, ExtractError, ExtractSeed, ExtractSink, Message, Person,
    RawRecord, Snapshot, RENDER_VERSION,
};
use crate::ingest::indexeddb::TeamsCache;
use crate::ingest::SourceError;
use crate::json_codec;
use crate::storage::{db, migrations};
use crate::timeutil;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use regex::Regex;
use rusqlite::{params, params_from_iter, types::Value as SqlVal, Connection, OptionalExtension};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::LazyLock;

pub use crate::storage::migrations::{check_schema_version, create_schema_at, SCHEMA_VERSION};

static IDBKEY_REPR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^<IdbKey (.*)>$").unwrap());
static INTEGRAL_FLOAT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-?\d+\.0$").unwrap());

#[derive(Debug, Clone)]
pub struct ArchiveError(pub String);

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ArchiveError {}

impl From<rusqlite::Error> for ArchiveError {
    fn from(e: rusqlite::Error) -> Self {
        ArchiveError(e.to_string())
    }
}

pub fn now_ms() -> i64 {
    timeutil::now_ms()
}

pub fn connect(path: Option<&Path>) -> Result<Connection, ArchiveError> {
    let conn = db::connect_rw(path)?;
    migrations::migrate(&conn)?;
    Ok(conn)
}

pub fn connect_readonly(path: Option<&Path>) -> Result<Connection, ArchiveError> {
    let conn = db::connect_ro(path)?;
    let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    check_schema_version(ver)?;
    Ok(conn)
}

pub fn has_data(conn: &Connection) -> bool {
    conn.query_row("SELECT EXISTS (SELECT 1 FROM messages)", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
        == 1
}

pub(crate) fn index_conversation_members(conn: &Connection, cid: &str, members: &[String]) -> Result<(), ArchiveError> {
    let mut seen = HashSet::new();
    let mut stmt =
        conn.prepare_cached("INSERT OR IGNORE INTO conversation_members (conversation_id, mri, oid) VALUES (?, ?, ?)")?;
    for m in members {
        if !m.is_empty() && seen.insert(m.clone()) {
            stmt.execute(params![cid, m, identity::oid(m)])?;
        }
    }
    for part_oid in identity::one_on_one_oids(cid) {
        let mri = format!("8:orgid:{part_oid}");
        if seen.insert(mri.clone()) {
            stmt.execute(params![cid, mri, part_oid])?;
        }
    }
    Ok(())
}

pub fn horizon_first(v: Option<&Value>) -> i64 {
    let Some(v) = v else { return 0 };
    if let Some(m) = identity::ms(v) {
        return m;
    }
    if let Some(s) = v.as_str() {
        let head = s.split(';').next().unwrap_or("").trim();
        if !head.is_empty() {
            return identity::ms_str(head).unwrap_or(0);
        }
    }
    0
}

pub fn extract_conv_read_state(raw: &Value) -> (i64, Option<i64>, Option<i64>) {
    let Some(obj) = raw.as_object() else {
        return (0, None, None);
    };
    let props = obj.get("properties").and_then(|v| v.as_object());
    let tp = obj.get("threadProperties").and_then(|v| v.as_object());
    let horizon = horizon_first(props.and_then(|p| p.get("consumptionhorizon")));
    let is_read = tp
        .and_then(|t| t.get("isRead"))
        .and_then(|v| v.as_bool())
        .map(|b| if b { 1 } else { 0 });
    let last_from_me = tp
        .and_then(|t| t.get("isLastMessageFromMe"))
        .and_then(|v| v.as_bool())
        .map(|b| if b { 1 } else { 0 });
    (horizon, is_read, last_from_me)
}

pub(crate) fn extract_mention_oids(props: &Value) -> Vec<String> {
    let Some(obj) = props.as_object() else {
        return Vec::new();
    };
    let mentions_val = match obj.get("mentions") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let Some(arr) = mentions_val.as_array() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in arr {
        if let Some(mri) = item.get("mri").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            let o = identity::oid(mri);
            if !o.is_empty() && seen.insert(o.clone()) {
                out.push(o);
            }
        }
    }
    out
}

fn index_message_mentions(conn: &Connection, cid: &str, mid: &str, props: &Value) -> Result<(), ArchiveError> {
    let oids = extract_mention_oids(props);
    if oids.is_empty() {
        return Ok(());
    }
    let mut stmt = conn
        .prepare_cached("INSERT OR IGNORE INTO message_mentions (conversation_id, message_id, oid) VALUES (?, ?, ?)")?;
    for o in oids {
        stmt.execute(params![cid, mid, o])?;
    }
    Ok(())
}

struct HashWriter(Sha256);

impl Write for HashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sha256_hex32(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..16])
}

pub fn content_hash(html: &str, subject: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(subject.as_bytes());
    hasher.update(b"\x00");
    hasher.update(html.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..16])
}

pub fn compress_bytes(raw: &[u8]) -> Vec<u8> {
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    let _ = enc.write_all(raw);
    enc.finish().unwrap_or_default()
}

pub fn encode_record(data: &Value) -> Vec<u8> {
    let s = json_codec::to_compact(data);
    compress_bytes(s.as_bytes())
}

pub fn decode_record_bytes(raw: &[u8]) -> Result<Value, String> {
    let mut dec = ZlibDecoder::new(raw);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).map_err(|e| format!("zlib: {e}"))?;
    let s = String::from_utf8(out).map_err(|e| format!("utf8: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("json: {e}"))
}

pub fn decode_record_ref(val: rusqlite::types::ValueRef<'_>) -> Result<Value, String> {
    match val {
        rusqlite::types::ValueRef::Blob(b) => decode_record_bytes(b),
        rusqlite::types::ValueRef::Text(t) => {
            let s = std::str::from_utf8(t).map_err(|e| format!("utf8: {e}"))?;
            serde_json::from_str(s).map_err(|e| format!("json: {e}"))
        }
        other => Err(format!("unexpected record data type {:?}", other.data_type())),
    }
}

fn clean_key_part_val(v: &Value) -> String {
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.is_finite() {
                    (f as i64).to_string()
                } else {
                    f.to_string()
                }
            } else {
                n.to_string()
            }
        }
        Value::String(s) => clean_key_part_str(s),
        other => other.to_string(),
    }
}

fn clean_key_part_str(s: &str) -> String {
    if let Some(cap) = IDBKEY_REPR_RE.captures(s) {
        let inner = &cap[1];
        if INTEGRAL_FLOAT_RE.is_match(inner) {
            return inner[..inner.len() - 2].to_string();
        }
        return inner.to_string();
    }
    s.to_string()
}

pub fn normalize_record_key(raw_key: &str) -> String {
    if raw_key.starts_with('[') || raw_key.starts_with('"') || INTEGRAL_FLOAT_RE.is_match(raw_key) {
        if let Ok(v) = serde_json::from_str::<Value>(raw_key) {
            match v {
                Value::Array(arr) => {
                    return arr.iter().map(clean_key_part_val).collect::<Vec<_>>().join("/");
                }
                Value::String(_) | Value::Number(_) => return clean_key_part_val(&v),
                _ => {}
            }
        }
    }
    clean_key_part_str(raw_key)
}

struct CachedMsg {
    pk: i64,
    version: i64,
    content_hash: Option<String>,
    edited_at: Option<i64>,
    deleted_at: Option<i64>,
    thread_id: Option<String>,
    sender_name: Option<String>,
    importance: Option<String>,
    properties: Option<String>,
    reactions: Option<String>,
}

const CACHED_MSG_COLS: &str = "pk, version, content_hash, edited_at, deleted_at, thread_id, sender_name, \
                               importance, properties, reactions";

impl CachedMsg {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(CachedMsg {
            pk: r.get(0)?,
            version: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            content_hash: r.get(2)?,
            edited_at: r.get(3)?,
            deleted_at: r.get(4)?,
            thread_id: r.get(5)?,
            sender_name: r.get(6)?,
            importance: r.get(7)?,
            properties: r.get(8)?,
            reactions: r.get(9)?,
        })
    }
}

struct PreparedMsg {
    hash: Option<String>,
    props_json: String,
    reactions_json: String,
}

impl PreparedMsg {
    fn of(m: &Message) -> Self {
        PreparedMsg {
            hash: (!m.content_html.trim().is_empty()).then(|| content_hash(&m.content_html, &m.subject)),
            props_json: json_codec::to_compact(&m.properties),
            reactions_json: json_codec::to_compact(&m.reactions),
        }
    }
}

fn message_unchanged(m: &Message, p: &PreparedMsg, c: &CachedMsg) -> bool {
    m.version == c.version
        && p.hash == c.content_hash
        && m.edited_at == c.edited_at
        && m.deleted_at == c.deleted_at
        && (m.thread_id.is_none() || m.thread_id == c.thread_id)
        && (m.sender_name.is_empty() || c.sender_name.as_deref() == Some(m.sender_name.as_str()))
        && (m.importance.is_empty() || c.importance.as_deref() == Some(m.importance.as_str()))
        && c.reactions.as_deref() == Some(p.reactions_json.as_str())
        && c.properties.as_deref() == Some(p.props_json.as_str())
}

fn load_existing_messages(
    conn: &Connection,
    keys: &[(&str, &str)],
) -> Result<HashMap<String, HashMap<String, CachedMsg>>, ArchiveError> {
    let mut out: HashMap<String, HashMap<String, CachedMsg>> = HashMap::new();
    for chunk in keys.chunks(db::LOOKUP_CHUNK) {
        let sql = format!(
            "SELECT {CACHED_MSG_COLS}, conversation_id, message_id FROM messages \
             WHERE (conversation_id, message_id) IN (VALUES {})",
            db::values_placeholders(chunk.len(), 2)
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(chunk.iter().flat_map(|(c, m)| [*c, *m])),
            |r| {
                Ok((
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                    CachedMsg::from_row(r)?,
                ))
            },
        )?;
        for row in rows {
            let (cid, mid, v) = row?;
            out.entry(cid).or_default().insert(mid, v);
        }
    }
    Ok(out)
}

type RecordVersionSet = HashMap<String, HashMap<String, HashSet<String>>>;

fn load_existing_record_versions(
    conn: &Connection,
    keys: &[(&str, &str, &str)],
) -> Result<RecordVersionSet, ArchiveError> {
    let mut out: RecordVersionSet = HashMap::new();
    if keys.is_empty() || !db::table_exists(conn, "record_versions") {
        return Ok(out);
    }
    for chunk in keys.chunks(db::LOOKUP_CHUNK) {
        let sql = format!(
            "SELECT source, record_key, hash FROM record_versions \
             WHERE (source, record_key, hash) IN (VALUES {})",
            db::values_placeholders(chunk.len(), 3)
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(chunk.iter().flat_map(|(s, k, h)| [*s, *k, *h])),
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )?;
        for row in rows {
            let (src, rk, h) = row?;
            out.entry(src).or_default().entry(rk).or_default().insert(h);
        }
    }
    Ok(out)
}

fn batch_touch_messages(conn: &Connection, pks: &[i64], now: i64) -> Result<(), ArchiveError> {
    for chunk in pks.chunks(500) {
        let qs = vec!["?"; chunk.len()].join(",");
        let sql = format!("UPDATE messages SET last_seen_at = ? WHERE pk IN ({qs})");
        let mut args = Vec::with_capacity(1 + chunk.len());
        args.push(SqlVal::Integer(now));
        args.extend(chunk.iter().map(|&pk| SqlVal::Integer(pk)));
        conn.prepare_cached(&sql)?.execute(params_from_iter(args))?;
    }
    Ok(())
}

fn schema_capabilities(conn: &Connection) -> Result<(bool, bool, bool), ArchiveError> {
    let schema_ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
    let has_member_table = schema_ver >= 5 || db::table_exists(conn, "conversation_members");
    let has_mention_table = schema_ver >= 6 || db::table_exists(conn, "message_mentions");
    let has_read_cols = if schema_ver >= 6 {
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
    Ok((has_member_table, has_mention_table, has_read_cols))
}

fn apply_message_slice(
    conn: &Connection,
    messages: &[Message],
    now: i64,
    has_mention_table: bool,
    st: &mut BTreeMap<String, i64>,
    touch_pks: &mut Vec<i64>,
) -> Result<(), ArchiveError> {
    if messages.is_empty() {
        return Ok(());
    }
    let mut by_msg: HashMap<(&str, &str), Vec<&Message>> = HashMap::with_capacity(messages.len());
    for m in messages {
        by_msg
            .entry((m.conversation_id.as_str(), m.message_id.as_str()))
            .or_default()
            .push(m);
    }
    let msg_keys: Vec<(&str, &str)> = by_msg.keys().copied().collect();
    let existing_msgs = load_existing_messages(conn, &msg_keys)?;

    for ((cid, mid), mut group) in by_msg {
        let cached = existing_msgs.get(cid).and_then(|m| m.get(mid));
        if group.len() > 1 {
            group.sort_by(|a, b| {
                a.version
                    .cmp(&b.version)
                    .then_with(|| a.edited_at.unwrap_or(0).cmp(&b.edited_at.unwrap_or(0)))
                    .then_with(|| a.arrived_at.unwrap_or(0).cmp(&b.arrived_at.unwrap_or(0)))
                    .then_with(|| a.content_html.cmp(&b.content_html))
            });
            let newest = *group.last().unwrap();
            let newest_h =
                (!newest.content_html.trim().is_empty()).then(|| content_hash(&newest.content_html, &newest.subject));
            if cached.is_some_and(|c| c.version == newest.version && c.content_hash == newest_h) {
                let mut rev_stmt = conn.prepare_cached(
                    "INSERT OR IGNORE INTO message_revisions \
                     (conversation_id, message_id, version, content_hash, content_html, text, subject, edited_at, captured_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )?;
                for older in &group[..group.len() - 1] {
                    if !older.content_html.trim().is_empty() {
                        let oh = content_hash(&older.content_html, &older.subject);
                        let rc = rev_stmt.execute(params![
                            older.conversation_id,
                            older.message_id,
                            older.version,
                            oh,
                            older.content_html,
                            older.text,
                            older.subject,
                            older.edited_at,
                            now
                        ])?;
                        *st.entry("revisions_new".to_string()).or_insert(0) += rc as i64;
                    }
                }
                group = vec![newest];
            }
        }

        if let [m] = group[..] {
            let prep = PreparedMsg::of(m);
            if let Some(c) = cached {
                if message_unchanged(m, &prep, c) {
                    touch_pks.push(c.pk);
                    continue;
                }
            }
            upsert_message(conn, m, prep, now, st, touch_pks, has_mention_table)?;
        } else {
            for m in group {
                upsert_message(conn, m, PreparedMsg::of(m), now, st, touch_pks, has_mention_table)?;
            }
        }
    }
    Ok(())
}

fn flush_record_versions(
    conn: &Connection,
    records: &[RawRecord],
    hashes: &[String],
    now: i64,
) -> Result<i64, ArchiveError> {
    if records.is_empty() {
        return Ok(0);
    }
    let rv_keys: Vec<(&str, &str, &str)> = records
        .iter()
        .zip(hashes)
        .map(|(r, h)| (r.source.as_str(), r.key.as_str(), h.as_str()))
        .collect();
    let mut existing_rv = load_existing_record_versions(conn, &rv_keys)?;
    let mut rv_new = 0i64;
    for (r, h) in records.iter().zip(hashes) {
        rv_new += keep_version(conn, r, h, now, &mut existing_rv)?;
    }
    Ok(rv_new)
}

pub struct ApplyCacheResult {
    pub merged: BTreeMap<String, i64>,
    pub cache_stats: BTreeMap<String, i64>,
}

pub enum ApplyCacheError {
    Source(SourceError),
    Archive(ArchiveError),
    MissingBlob,
}

struct StreamingApplier<'a> {
    conn: &'a Connection,
    now: i64,
    has_member_table: bool,
    has_mention_table: bool,
    has_read_cols: bool,
    st: BTreeMap<String, i64>,
    conv_new: i64,
    rv_new: i64,
    me_mri: Option<String>,
    touch_pks: Vec<i64>,
    msg_buf: Vec<Message>,
    rv_buf: Vec<RawRecord>,
    rv_hashes: Vec<String>,
}

impl<'a> StreamingApplier<'a> {
    fn flush_messages(&mut self) -> Result<(), ArchiveError> {
        if self.msg_buf.is_empty() {
            return Ok(());
        }
        apply_message_slice(
            self.conn,
            &self.msg_buf,
            self.now,
            self.has_mention_table,
            &mut self.st,
            &mut self.touch_pks,
        )?;
        self.msg_buf.clear();
        if self.touch_pks.len() >= 500 {
            batch_touch_messages(self.conn, &self.touch_pks, self.now)?;
            self.touch_pks.clear();
        }
        Ok(())
    }

    fn flush_records(&mut self) -> Result<(), ArchiveError> {
        if self.rv_buf.is_empty() {
            return Ok(());
        }
        self.rv_new += flush_record_versions(self.conn, &self.rv_buf, &self.rv_hashes, self.now)?;
        self.rv_buf.clear();
        self.rv_hashes.clear();
        Ok(())
    }

    fn finish(mut self) -> Result<BTreeMap<String, i64>, ArchiveError> {
        self.flush_messages()?;
        if !self.touch_pks.is_empty() {
            batch_touch_messages(self.conn, &self.touch_pks, self.now)?;
            self.touch_pks.clear();
        }
        self.flush_records()?;
        self.st.insert("conversations_new".to_string(), self.conv_new);
        self.st.insert("record_versions_new".to_string(), self.rv_new);
        if let Some(ref me) = self.me_mri {
            set_meta(self.conn, "me_mri", me)?;
        }
        Ok(self.st)
    }
}

impl ExtractSink for StreamingApplier<'_> {
    type Error = ArchiveError;

    fn on_me(&mut self, me_mri: &str) -> Result<(), Self::Error> {
        self.me_mri = Some(me_mri.to_string());
        Ok(())
    }

    fn on_person(&mut self, person: Person) -> Result<(), Self::Error> {
        upsert_person(self.conn, &person, self.now)
    }

    fn on_conversation(&mut self, conv: Conversation) -> Result<(), Self::Error> {
        self.conv_new += upsert_conversation(self.conn, &conv, self.now, self.has_member_table, self.has_read_cols)?;
        Ok(())
    }

    fn on_message(&mut self, msg: Message) -> Result<(), Self::Error> {
        self.msg_buf.push(msg);
        if self.msg_buf.len() >= db::LOOKUP_CHUNK {
            self.flush_messages()?;
        }
        Ok(())
    }

    fn on_record(&mut self, rec: RawRecord) -> Result<(), Self::Error> {
        let h = record_hash(&rec.source, &rec.data);
        self.rv_buf.push(rec);
        self.rv_hashes.push(h);
        if self.rv_buf.len() >= db::LOOKUP_CHUNK {
            self.flush_records()?;
        }
        Ok(())
    }
}

pub fn load_extract_seed(conn: &Connection, me_mri: Option<String>) -> Result<ExtractSeed, ArchiveError> {
    let mut by_mri = HashMap::new();
    let mut by_oid = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT mri, name FROM people WHERE name IS NOT NULL AND name != ''")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (mri, name) = row?;
            let o = identity::oid(&mri);
            if !o.is_empty() {
                by_oid.insert(o, name.clone());
            }
            by_mri.insert(mri, name);
        }
    }
    let mut seen_convs = HashSet::new();
    let mut team_names = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, kind, name FROM conversations")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (id, kind, name) = row?;
            if kind == "team" {
                if let Some(n) = name.filter(|s| !s.is_empty()) {
                    team_names.insert(id.clone(), n);
                }
            }
            seen_convs.insert(id);
        }
    }
    Ok(ExtractSeed {
        incremental: true,
        me_mri,
        by_mri,
        by_oid,
        seen_convs,
        team_names,
    })
}

pub fn apply_cache(
    conn: &Connection,
    cache: &mut TeamsCache,
    now_override: Option<i64>,
) -> Result<ApplyCacheResult, ApplyCacheError> {
    apply_cache_with_seed(conn, cache, ExtractSeed::default(), now_override)
}

pub fn apply_cache_incremental(
    conn: &Connection,
    cache: &mut TeamsCache,
    seed: ExtractSeed,
    now_override: Option<i64>,
) -> Result<ApplyCacheResult, ApplyCacheError> {
    apply_cache_with_seed(conn, cache, seed, now_override)
}

fn apply_cache_with_seed(
    conn: &Connection,
    cache: &mut TeamsCache,
    seed: ExtractSeed,
    now_override: Option<i64>,
) -> Result<ApplyCacheResult, ApplyCacheError> {
    let incremental = seed.incremental;
    let now = now_override.unwrap_or_else(now_ms);
    let (has_member_table, has_mention_table, has_read_cols) =
        schema_capabilities(conn).map_err(ApplyCacheError::Archive)?;
    conn.execute_batch("BEGIN IMMEDIATE;")
        .map_err(|e| ApplyCacheError::Archive(ArchiveError::from(e)))?;

    let mut applier = StreamingApplier {
        conn,
        now,
        has_member_table,
        has_mention_table,
        has_read_cols,
        st: BTreeMap::new(),
        conv_new: 0,
        rv_new: 0,
        me_mri: None,
        touch_pks: Vec::new(),
        msg_buf: Vec::with_capacity(db::LOOKUP_CHUNK),
        rv_buf: Vec::with_capacity(db::LOOKUP_CHUNK),
        rv_hashes: Vec::with_capacity(db::LOOKUP_CHUNK),
    };

    let res = match extract::extract_into_with_seed(cache, &mut applier, seed) {
        Ok(cache_stats) => {
            if incremental && (cache.missing_blob || cache.bad_records > 0) {
                Err(ApplyCacheError::MissingBlob)
            } else {
                applier
                    .finish()
                    .map(|merged| ApplyCacheResult { merged, cache_stats })
                    .map_err(ApplyCacheError::Archive)
            }
        }
        Err(ExtractError::Source(e)) => Err(ApplyCacheError::Source(e)),
        Err(ExtractError::Sink(e)) => Err(ApplyCacheError::Archive(e)),
    };

    match res {
        Ok(pair) => {
            conn.execute_batch("COMMIT;")
                .map_err(|e| ApplyCacheError::Archive(ArchiveError::from(e)))?;
            Ok(pair)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

pub fn apply_snapshot(
    conn: &Connection,
    snap: &Snapshot,
    now_override: Option<i64>,
) -> Result<BTreeMap<String, i64>, ArchiveError> {
    let now = now_override.unwrap_or_else(now_ms);
    let mut st: BTreeMap<String, i64> = BTreeMap::new();
    let (has_member_table, has_mention_table, has_read_cols) = schema_capabilities(conn)?;

    let record_hashes: Vec<String> = snap.records.iter().map(|r| record_hash(&r.source, &r.data)).collect();

    let mut touch_pks: Vec<i64> = Vec::new();
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<(), ArchiveError> {
        for p in snap.people.values() {
            upsert_person(conn, p, now)?;
        }
        let mut conv_new = 0i64;
        for c in snap.conversations.values() {
            conv_new += upsert_conversation(conn, c, now, has_member_table, has_read_cols)?;
        }
        st.insert("conversations_new".to_string(), conv_new);

        apply_message_slice(conn, &snap.messages, now, has_mention_table, &mut st, &mut touch_pks)?;

        if !touch_pks.is_empty() {
            batch_touch_messages(conn, &touch_pks, now)?;
        }

        let rv_new = flush_record_versions(conn, &snap.records, &record_hashes, now)?;
        st.insert("record_versions_new".to_string(), rv_new);

        if let Some(ref me) = snap.me_mri {
            set_meta(conn, "me_mri", me)?;
        }
        Ok(())
    })();

    match res {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(st)
}

pub fn record_hash(source: &str, data: &Value) -> String {
    let mut hw = HashWriter(Sha256::new());
    if json_codec::write_sorted_filtered(&mut hw, data, volatile_fields(source)).is_err() {
        return sha256_hex32(b"null");
    }
    let digest = hw.0.finalize();
    hex::encode(&digest[..16])
}

fn keep_version(
    conn: &Connection,
    r: &RawRecord,
    hash: &str,
    now: i64,
    existing_rv: &mut RecordVersionSet,
) -> Result<i64, ArchiveError> {
    if existing_rv
        .get(&r.source)
        .and_then(|m| m.get(&r.key))
        .is_some_and(|s| s.contains(hash))
    {
        return Ok(0);
    }
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO record_versions (source, record_key, hash, data, captured_at) VALUES (?, ?, ?, ?, ?)",
    )?;
    let rc = stmt.execute(params![r.source, r.key, hash, encode_record(&r.data), now])? as i64;
    existing_rv
        .entry(r.source.clone())
        .or_default()
        .entry(r.key.clone())
        .or_default()
        .insert(hash.to_string());
    Ok(rc)
}

fn upsert_person(conn: &Connection, p: &Person, now: i64) -> Result<(), ArchiveError> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO people (mri, name, email, job_title, department, kind, raw, first_seen_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (mri) DO UPDATE SET
             name       = COALESCE(NULLIF(excluded.name, ''), name),
             email      = COALESCE(NULLIF(excluded.email, ''), email),
             job_title  = COALESCE(NULLIF(excluded.job_title, ''), job_title),
             department = COALESCE(NULLIF(excluded.department, ''), department),
             kind       = COALESCE(NULLIF(excluded.kind, ''), kind),
             raw        = excluded.raw,
             last_seen_at = excluded.last_seen_at",
    )?;
    let raw_s = json_codec::to_compact(&p.raw);
    stmt.execute(params![
        p.mri,
        p.name,
        p.email,
        p.job_title,
        p.department,
        p.kind,
        raw_s,
        now,
        now
    ])?;
    Ok(())
}

fn upsert_conversation(
    conn: &Connection,
    c: &Conversation,
    now: i64,
    index_members: bool,
    has_read_cols: bool,
) -> Result<i64, ArchiveError> {
    let existed: bool = conn
        .prepare_cached("SELECT 1 FROM conversations WHERE id = ?")?
        .query_row([&c.id], |_| Ok(()))
        .optional()?
        .is_some();
    let members_s = json_codec::to_compact(&c.members);
    let raw_s = json_codec::to_compact(&c.raw);
    let fb: i64 = if c.name_is_fallback { 1 } else { 0 };
    if has_read_cols {
        let (horizon, is_read, last_from_me) = extract_conv_read_state(&c.raw);
        let mut stmt = conn.prepare_cached(
            "INSERT INTO conversations (
                id, kind, name, topic, team_id, members, last_message_at, raw,
                read_horizon, is_read, last_from_me, first_seen_at, last_seen_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                kind         = CASE WHEN excluded.raw = '{}' THEN kind ELSE excluded.kind END,
                name         = CASE WHEN ? AND name IS NOT NULL THEN name ELSE excluded.name END,
                topic        = COALESCE(NULLIF(excluded.topic, ''), topic),
                team_id      = COALESCE(excluded.team_id, team_id),
                members      = CASE WHEN excluded.members = '[]' THEN members ELSE excluded.members END,
                last_message_at = MAX(COALESCE(excluded.last_message_at, 0), COALESCE(last_message_at, 0)),
                raw          = CASE WHEN excluded.raw = '{}' THEN raw ELSE excluded.raw END,
                read_horizon = CASE WHEN excluded.raw = '{}' THEN read_horizon ELSE excluded.read_horizon END,
                is_read      = CASE WHEN excluded.raw = '{}' THEN is_read ELSE excluded.is_read END,
                last_from_me = CASE WHEN excluded.raw = '{}' THEN last_from_me ELSE excluded.last_from_me END,
                last_seen_at = excluded.last_seen_at",
        )?;
        stmt.execute(params![
            c.id,
            c.kind,
            c.name,
            c.topic,
            c.team_id,
            members_s,
            c.last_message_at,
            raw_s,
            horizon,
            is_read,
            last_from_me,
            now,
            now,
            fb
        ])?;
    } else {
        let mut stmt = conn.prepare_cached(
            "INSERT INTO conversations (id, kind, name, topic, team_id, members, last_message_at, raw, first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (id) DO UPDATE SET
                kind    = CASE WHEN excluded.raw = '{}' THEN kind ELSE excluded.kind END,
                name    = CASE WHEN ? AND name IS NOT NULL THEN name ELSE excluded.name END,
                topic   = COALESCE(NULLIF(excluded.topic, ''), topic),
                team_id = COALESCE(excluded.team_id, team_id),
                members = CASE WHEN excluded.members = '[]' THEN members ELSE excluded.members END,
                last_message_at = MAX(COALESCE(excluded.last_message_at, 0), COALESCE(last_message_at, 0)),
                raw     = CASE WHEN excluded.raw = '{}' THEN raw ELSE excluded.raw END,
                last_seen_at = excluded.last_seen_at",
        )?;
        stmt.execute(params![
            c.id,
            c.kind,
            c.name,
            c.topic,
            c.team_id,
            members_s,
            c.last_message_at,
            raw_s,
            now,
            now,
            fb
        ])?;
    }
    if index_members {
        index_conversation_members(conn, &c.id, &c.members)?;
    }
    Ok(if existed { 0 } else { 1 })
}

fn merge_json_object(old_str: Option<&str>, new_val: &Value, skip_nulls: bool) -> Value {
    let mut base = old_str
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(new_obj) = new_val.as_object() {
        for (k, v) in new_obj {
            if !skip_nulls || !v.is_null() {
                base.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(base)
}

fn upsert_message(
    conn: &Connection,
    m: &Message,
    prep: PreparedMsg,
    now: i64,
    st: &mut BTreeMap<String, i64>,
    touch_pks: &mut Vec<i64>,
    index_mentions: bool,
) -> Result<(), ArchiveError> {
    let h = prep.hash.clone();
    let has_content = h.is_some();

    let existing = conn
        .prepare_cached(&format!(
            "SELECT {CACHED_MSG_COLS}, raw FROM messages WHERE conversation_id = ? AND message_id = ?"
        ))?
        .query_row(params![m.conversation_id, m.message_id], |r| {
            Ok((CachedMsg::from_row(r)?, r.get::<_, Option<String>>(10)?))
        })
        .optional()?;

    let text_opt = if m.text.is_empty() { None } else { Some(m.text.as_str()) };

    match existing {
        None => {
            let html_opt = if has_content {
                Some(m.content_html.as_str())
            } else {
                None
            };
            let raw_s = json_codec::to_compact(&m.raw);
            conn.prepare_cached(
                "INSERT INTO messages (conversation_id, message_id, thread_id, parent_id, version, arrived_at,
                    edited_at, deleted_at, sender_mri, sender_name, message_type, kind, content_html, text,
                    subject, importance, reactions, properties, raw, content_hash, first_seen_at, last_seen_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                m.conversation_id,
                m.message_id,
                m.thread_id,
                m.parent_id,
                m.version,
                m.arrived_at,
                m.edited_at,
                m.deleted_at,
                m.sender_mri,
                m.sender_name,
                m.message_type,
                m.kind,
                html_opt,
                text_opt,
                m.subject,
                m.importance,
                prep.reactions_json,
                prep.props_json,
                raw_s,
                h,
                now,
                now
            ])?;
            *st.entry("messages_new".to_string()).or_insert(0) += 1;
            if index_mentions && m.kind == "message" {
                index_message_mentions(conn, &m.conversation_id, &m.message_id, &m.properties)?;
            }
        }
        Some((cached, old_raw)) => {
            if message_unchanged(m, &prep, &cached) {
                touch_pks.push(cached.pk);
                return Ok(());
            }
            let PreparedMsg {
                mut props_json,
                mut reactions_json,
                ..
            } = prep;
            let CachedMsg {
                pk,
                version: old_ver,
                content_hash: old_hash,
                deleted_at: old_deleted,
                properties: old_props,
                reactions: old_reacts,
                ..
            } = cached;

            let newer = m.version >= old_ver;
            if has_content && h != old_hash && (newer || old_hash.is_none()) {
                conn.prepare_cached(
                    "UPDATE messages SET content_html = ?, text = ?, subject = ?, content_hash = ? WHERE pk = ?",
                )?
                .execute(params![m.content_html, text_opt, m.subject, h, pk])?;
                if old_hash.is_some() {
                    *st.entry("messages_edited".to_string()).or_insert(0) += 1;
                }
            }
            if newer {
                let (merged_props, raw_s) = if !has_content {
                    let mp = merge_json_object(old_props.as_deref(), &m.properties, true);
                    props_json = json_codec::to_compact(&mp);
                    if m.reactions.is_empty() {
                        if let Some(ref r) = old_reacts {
                            reactions_json = r.clone();
                        }
                    }
                    let mr = merge_json_object(old_raw.as_deref(), &m.raw, false);
                    (mp, json_codec::to_compact(&mr))
                } else {
                    (m.properties.clone(), json_codec::to_compact(&m.raw))
                };
                let hc: i64 = if has_content { 1 } else { 0 };
                conn.prepare_cached(
                    "UPDATE messages SET
                        version = ?, thread_id = COALESCE(?, thread_id), parent_id = COALESCE(?, parent_id),
                        arrived_at = COALESCE(arrived_at, ?), edited_at = COALESCE(?, edited_at),
                        deleted_at = COALESCE(deleted_at, ?), sender_mri = COALESCE(NULLIF(?, ''), sender_mri),
                        sender_name = COALESCE(NULLIF(?, ''), sender_name),
                        message_type = CASE WHEN ? OR content_html IS NULL THEN ? ELSE message_type END,
                        kind = CASE WHEN ? OR content_html IS NULL THEN ? ELSE kind END,
                        importance = COALESCE(NULLIF(?, ''), importance), reactions = ?, properties = ?,
                        raw = ?, last_seen_at = ?,
                        text = CASE WHEN content_html IS NULL AND ? != '' THEN ? ELSE text END
                     WHERE pk = ?",
                )?
                .execute(params![
                    m.version,
                    m.thread_id,
                    m.parent_id,
                    m.arrived_at,
                    m.edited_at,
                    m.deleted_at,
                    m.sender_mri,
                    m.sender_name,
                    hc,
                    m.message_type,
                    hc,
                    m.kind,
                    m.importance,
                    reactions_json,
                    props_json,
                    raw_s,
                    now,
                    m.text,
                    m.text,
                    pk
                ])?;
                if m.deleted_at.is_some() && old_deleted.is_none() {
                    *st.entry("messages_deleted".to_string()).or_insert(0) += 1;
                }
                if index_mentions && m.kind == "message" {
                    index_message_mentions(conn, &m.conversation_id, &m.message_id, &merged_props)?;
                }
            } else {
                touch_pks.push(pk);
            }
        }
    }

    if has_content {
        let rc = conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_revisions \
                 (conversation_id, message_id, version, content_hash, content_html, text, subject, edited_at, captured_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )?
            .execute(params![
                m.conversation_id, m.message_id, m.version, h, m.content_html, m.text, m.subject, m.edited_at, now
            ])?;
        *st.entry("revisions_new".to_string()).or_insert(0) += rc as i64;
    }
    if !m.attachments.is_empty() {
        let mut astmt = conn.prepare_cached(
            "INSERT OR IGNORE INTO attachments (conversation_id, message_id, kind, name, url) VALUES (?, ?, ?, ?, ?)",
        )?;
        for a in &m.attachments {
            astmt.execute(params![m.conversation_id, m.message_id, a.kind, a.name, a.url])?;
        }
    }
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), ArchiveError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn me_mri(conn: &Connection) -> Option<String> {
    if let Some(v) = get_meta(conn, "me_mri").filter(|s| !s.is_empty()) {
        return Some(v);
    }
    if db::table_exists(conn, "record_versions") {
        let mut counts: HashMap<String, usize> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT data FROM record_versions WHERE id IN (\
                SELECT MAX(id) FROM record_versions \
                WHERE source = 'streams-replychain-manager/mentions' GROUP BY record_key)",
        ) {
            if let Ok(rows) = stmt.query_map([], |r| Ok(decode_record_ref(r.get_ref(0)?).ok())) {
                for v in rows.flatten().flatten() {
                    if let Some(mm) = v.get("messageMap").and_then(|x| x.as_object()) {
                        for m in mm.values() {
                            if let Some(t) = m
                                .get("properties")
                                .and_then(|p| p.get("activity"))
                                .and_then(|a| a.get("targetUserId"))
                                .and_then(|x| x.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                *counts.entry(t.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
        }
        if let Some((best, _)) = counts.into_iter().max_by_key(|(_, c)| *c) {
            return Some(best);
        }
    }
    let mut ids: HashMap<String, usize> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id FROM conversations WHERE id LIKE '19:%'") {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
            for cid in rows.flatten() {
                if let Some(cap) = identity::ONE_ON_ONE_RE.captures(&cid) {
                    let a = cap[1].to_string();
                    let b = cap[2].to_string();
                    *ids.entry(a.clone()).or_insert(0) += 1;
                    if b != a {
                        *ids.entry(b).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    let mut pairs: Vec<(String, usize)> = ids.into_iter().collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if let Some((top_id, top_n)) = pairs.first() {
        if *top_n >= 2 && (pairs.len() == 1 || *top_n > pairs[1].1) {
            return Some(format!("8:orgid:{top_id}"));
        }
    }
    None
}

const RERENDER_BATCH: i64 = 500;

pub fn rerender(conn: &Connection, force: bool) -> Result<i64, ArchiveError> {
    if !force && get_meta(conn, "render_version").as_deref() == Some(&RENDER_VERSION.to_string()) {
        return Ok(0);
    }
    let mut by_mri = HashMap::new();
    let mut by_oid = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT mri, name FROM people WHERE name IS NOT NULL AND name != ''")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (mri, name) = row?;
            let o = identity::oid(&mri);
            if !o.is_empty() {
                by_oid.insert(o, name.clone());
            }
            by_mri.insert(mri, name);
        }
    }
    let name_of = |mri: &str| -> String {
        by_mri
            .get(mri)
            .or_else(|| by_oid.get(&identity::oid(mri)))
            .cloned()
            .unwrap_or_else(|| mri.to_string())
    };

    let mut changed = 0i64;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let res = (|| -> Result<(), ArchiveError> {
        let mut last = 0i64;
        loop {
            let mut batch = Vec::new();
            {
                let mut stmt = conn.prepare_cached(
                    "SELECT pk, message_type, content_html, properties, kind, text \
                     FROM messages WHERE pk > ? ORDER BY pk LIMIT ?",
                )?;
                for row in stmt.query_map(params![last, RERENDER_BATCH], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(3)?.unwrap_or_else(|| "{}".to_string()),
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                })? {
                    batch.push(row?);
                }
            }
            if batch.is_empty() {
                break;
            }
            last = batch.last().unwrap().0;
            let mut upd = conn.prepare_cached("UPDATE messages SET kind = ?, text = ? WHERE pk = ?")?;
            for (pk, mtype, html, props_s, old_kind, old_text) in batch {
                let props: Value = serde_json::from_str(&props_s).unwrap_or_else(|_| Value::Object(Map::new()));
                let (kind, text_s) = render_text(&mtype, &html, &props, &name_of);
                let text = if text_s.is_empty() { None } else { Some(text_s) };
                if (kind.as_str(), text.as_deref()) != (old_kind.as_str(), old_text.as_deref())
                    && (text.is_some() || old_text.is_none())
                {
                    upd.execute(params![kind, text, pk])?;
                    changed += 1;
                }
            }
        }

        let mut last_rev = 0i64;
        loop {
            let mut batch = Vec::new();
            {
                let mut stmt = conn.prepare_cached(
                    "SELECT r.id, r.content_html, r.text, m.message_type, m.properties \
                     FROM message_revisions r JOIN messages m USING (conversation_id, message_id) \
                     WHERE r.id > ? ORDER BY r.id LIMIT ?",
                )?;
                for row in stmt.query_map(params![last_rev, RERENDER_BATCH], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "{}".to_string()),
                    ))
                })? {
                    batch.push(row?);
                }
            }
            if batch.is_empty() {
                break;
            }
            last_rev = batch.last().unwrap().0;
            let mut upd = conn.prepare_cached("UPDATE message_revisions SET text = ? WHERE id = ?")?;
            for (id, html, old_text, mtype, props_s) in batch {
                let props: Value = serde_json::from_str(&props_s).unwrap_or_else(|_| Value::Object(Map::new()));
                let (_, text) = render_text(&mtype, &html, &props, &name_of);
                if !text.is_empty() && old_text.as_deref() != Some(text.as_str()) {
                    upd.execute(params![text, id])?;
                }
            }
        }

        set_meta(conn, "render_version", &RENDER_VERSION.to_string())?;
        Ok(())
    })();
    match res {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(changed)
}

#[derive(Debug, Clone)]
pub struct SyncRun {
    #[allow(dead_code)]
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub stats: String,
    pub message: Option<String>,
}

pub fn start_run(conn: &Connection, source: &str) -> Result<i64, ArchiveError> {
    conn.execute(
        "INSERT INTO sync_runs (started_at, status, source) VALUES (?, 'running', ?)",
        params![now_ms(), source],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_run(
    conn: &Connection,
    run_id: i64,
    status: &str,
    stats: Option<&Value>,
    message: Option<&str>,
) -> Result<(), ArchiveError> {
    let stats_s = stats.map(json_codec::to_compact).unwrap_or_else(|| "{}".to_string());
    conn.execute(
        "UPDATE sync_runs SET finished_at = ?, status = ?, stats = ?, message = ? WHERE id = ?",
        params![now_ms(), status, stats_s, message, run_id],
    )?;
    Ok(())
}

pub fn last_run(conn: &Connection, ok_only: bool) -> Option<SyncRun> {
    let where_sql = if ok_only {
        "WHERE status IN ('ok', 'warning')"
    } else {
        "WHERE status != 'running'"
    };
    conn.query_row(
        &format!("SELECT id, started_at, finished_at, status, COALESCE(stats, '{{}}'), message FROM sync_runs {where_sql} ORDER BY id DESC LIMIT 1"),
        [],
        |r| {
            Ok(SyncRun {
                id: r.get(0)?,
                started_at: r.get(1)?,
                finished_at: r.get(2)?,
                status: r.get(3)?,
                stats: r.get(4)?,
                message: r.get(5)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}
